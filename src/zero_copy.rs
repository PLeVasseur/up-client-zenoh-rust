/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
 *
 * See the NOTICE file(s) distributed with this work for additional
 * information regarding copyright ownership.
 *
 * This program and the accompanying materials are made available under the
 * terms of the Apache License Version 2.0 which is available at
 * https://www.apache.org/licenses/LICENSE-2.0
 *
 * SPDX-License-Identifier: Apache-2.0
 ********************************************************************************/

use std::sync::Arc;
use std::{mem::MaybeUninit, num::NonZeroUsize};

use async_trait::async_trait;
use tracing::{trace, warn};
use up_rust::{
    payload::UWireError,
    transport::verify_filter_criteria,
    validate_frame_metadata_for_payload,
    zero_copy::UZeroCopyListener,
    zero_copy::{
        LoanedPayload, LoanedPayloadUninitMut, PayloadLoanProvenance,
        ULoanedContiguousZeroCopyRxFrame, UTxBuffer, UUninitTxBuffer, UZeroCopyRxFrame,
        UZeroCopyTransport,
    },
    UCode, UFrameMetadata, UStatus, UTxLoanSpec, UUri, UZeroCopyUninitTransport,
};
use zenoh::{
    bytes::{ZBytes, ZBytesReader, ZBytesSliceIterator},
    sample::Sample,
};
use zenoh::{
    shm::{GarbageCollect, MemoryLayout, OwnedShmBuf, ZShmMut},
    Wait as _,
};

use crate::{
    listener_registry::ComparableZeroCopyListener,
    utransport::{
        attachment_to_frame_metadata, frame_to_attachment, map_zenoh_priority, to_zenoh_key_string,
    },
    UPTransportZenoh,
};

/// Zenoh shared-memory transmit loan used by the `zero-copy` feature.
///
/// Values of this type are returned from
/// [`UZeroCopyTransport::loan_tx`](up_rust::zero_copy::UZeroCopyTransport::loan_tx)
/// for [`UPTransportZenoh`]. The payload storage is backed by Zenoh SHM when the
/// frame has a payload. Frames without payload use an empty buffer and no SHM
/// allocation.
///
/// Callers normally do not construct this type directly. Use
/// [`UZeroCopyTransportExt::send_serialized_zero_copy`](up_rust::zero_copy::UZeroCopyTransportExt::send_serialized_zero_copy)
/// to reserve, serialize into, and send a loan in one step.
pub struct ZenohTxBuffer {
    metadata: UFrameMetadata,
    zenoh_key: String,
    attachment: ZBytes,
    priority: zenoh::qos::Priority,
    payload: ZenohTxPayload,
}

/// Zenoh shared-memory transmit loan with uninitialized application payload bytes.
pub struct ZenohUninitTxBuffer {
    metadata: UFrameMetadata,
    zenoh_key: String,
    attachment: ZBytes,
    priority: zenoh::qos::Priority,
    payload: ZenohTxPayload,
}

enum ZenohTxPayload {
    Empty,
    Shm(ZShmMut),
}

impl ZenohTxPayload {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Shm(payload) => payload.as_ref(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Empty => &mut [],
            Self::Shm(payload) => payload.as_mut(),
        }
    }

    fn as_uninit_mut_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        match self {
            Self::Empty => &mut [],
            Self::Shm(payload) => {
                let payload = payload.as_mut();
                // SAFETY:
                // - `payload` is one mutable Zenoh SHM payload slice and is
                //   exclusively borrowed through `&mut self`.
                // - Per https://doc.rust-lang.org/stable/std/mem/union.MaybeUninit.html#layout-1:
                //
                //   "`MaybeUninit<T>` is guaranteed to have the same size,
                //   alignment, and ABI as `T`."
                //
                // - Per https://doc.rust-lang.org/stable/std/slice/fn.from_raw_parts_mut.html#safety,
                //   `data` must be "valid for both reads and writes" and "must
                //   not be accessed through any other pointer" for the returned
                //   lifetime; those properties come from the original `&mut [u8]`.
                unsafe {
                    std::slice::from_raw_parts_mut(
                        payload.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                        payload.len(),
                    )
                }
            }
        }
    }

    fn into_zbytes(self) -> ZBytes {
        match self {
            Self::Empty => ZBytes::new(),
            Self::Shm(payload) => ZBytes::from(payload),
        }
    }
}

impl UTxBuffer for ZenohTxBuffer {
    fn metadata(&self) -> &UFrameMetadata {
        &self.metadata
    }

    fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    fn payload_mut(&mut self) -> &mut [u8] {
        self.payload.as_mut_slice()
    }

    fn payload_loan_provenance(&self) -> PayloadLoanProvenance {
        match self.payload {
            ZenohTxPayload::Empty => PayloadLoanProvenance::OpaqueTransportLoan,
            ZenohTxPayload::Shm(_) => PayloadLoanProvenance::SharedMemory,
        }
    }
}

impl UUninitTxBuffer for ZenohUninitTxBuffer {
    type Initialized = ZenohTxBuffer;

    fn metadata(&self) -> &UFrameMetadata {
        &self.metadata
    }

    fn payload_len(&self) -> usize {
        self.payload.as_slice().len()
    }

    fn payload_loan_provenance(&self) -> PayloadLoanProvenance {
        match self.payload {
            ZenohTxPayload::Empty => PayloadLoanProvenance::OpaqueTransportLoan,
            ZenohTxPayload::Shm(_) => PayloadLoanProvenance::SharedMemory,
        }
    }

    fn payload_uninit_mut(&mut self) -> LoanedPayloadUninitMut<'_> {
        let provenance = self.payload_loan_provenance();
        // SAFETY:
        // - `as_uninit_mut_slice` returns the exact visible payload range for
        //   this Zenoh transmit loan.
        // - `provenance` is derived from the same payload storage, preserving
        //   whether the range is ordinary transport storage or SHM-backed.
        // - `&mut self` provides exclusive access for the returned loan view.
        // - The external Zenoh contract supplies the SHM/runtime provenance;
        //   Rust only sees the exact borrowed slice and lifetime here.
        unsafe {
            LoanedPayloadUninitMut::new_unchecked(self.payload.as_uninit_mut_slice(), provenance)
        }
    }

    unsafe fn assume_payload_init(self) -> Self::Initialized {
        // SAFETY CONTRACT:
        // - The caller of `UUninitTxBuffer::assume_payload_init` guarantees the
        //   visible application payload range returned by `payload_uninit_mut`
        //   was fully initialized before conversion.
        // - This conversion does not reinterpret pointers or allocate; it only
        //   moves the same Zenoh payload storage into the initialized type-state.
        // - External contract: if the storage is SHM-backed, Zenoh continues to
        //   own a valid payload allocation for the resulting transmit buffer.
        ZenohTxBuffer {
            metadata: self.metadata,
            zenoh_key: self.zenoh_key,
            attachment: self.attachment,
            priority: self.priority,
            payload: self.payload,
        }
    }
}

/// Zenoh zero-copy receive lease used by the `zero-copy` feature.
///
/// The payload is exposed from Zenoh [`ZBytes`] through
/// [`UZeroCopyRxFrame::payload_reader`] and
/// [`UZeroCopyRxFrame::payload_slices`]. The lease may be segmented, so generic
/// callers should use reader-based deserialization instead of assuming a
/// contiguous borrowed slice. [`UZeroCopyRxFrame::try_contiguous_payload`] returns
/// `Some` only when the underlying `ZBytes` payload is already one slice.
///
/// [`ZBytes`]: zenoh::bytes::ZBytes
/// [`UZeroCopyRxFrame::payload_reader`]: up_rust::zero_copy::UZeroCopyRxFrame::payload_reader
/// [`UZeroCopyRxFrame::payload_slices`]: up_rust::zero_copy::UZeroCopyRxFrame::payload_slices
/// [`UZeroCopyRxFrame::try_contiguous_payload`]: up_rust::zero_copy::UZeroCopyRxFrame::try_contiguous_payload
pub struct ZenohRxFrame {
    metadata: UFrameMetadata,
    sample: Sample,
}

impl ZenohRxFrame {
    pub(crate) fn new(metadata: UFrameMetadata, sample: Sample) -> Self {
        Self { metadata, sample }
    }

    /// Returns the underlying Zenoh sample.
    ///
    /// Most uProtocol code should use the [`UZeroCopyRxFrame`] methods instead.
    /// This accessor is provided for Zenoh-specific diagnostics or advanced
    /// integrations that need to inspect sample metadata outside the uProtocol
    /// frame model.
    #[must_use]
    pub fn sample(&self) -> &Sample {
        &self.sample
    }
}

impl UZeroCopyRxFrame for ZenohRxFrame {
    type PayloadReader<'a>
        = ZBytesReader<'a>
    where
        Self: 'a;
    type PayloadSlices<'a>
        = ZBytesSliceIterator<'a>
    where
        Self: 'a;

    fn metadata(&self) -> &UFrameMetadata {
        &self.metadata
    }

    fn payload_len(&self) -> usize {
        if self.metadata.encoding().is_some() {
            self.sample.payload().len()
        } else {
            0
        }
    }

    fn payload_reader(&self) -> Self::PayloadReader<'_> {
        self.sample.payload().reader()
    }

    fn payload_slices(&self) -> Self::PayloadSlices<'_> {
        self.sample.payload().slices()
    }

    fn try_contiguous_payload(&self) -> Option<&[u8]> {
        if self.metadata.encoding().is_none() {
            return Some(&[]);
        }
        let mut slices = self.sample.payload().slices();
        let first = slices.next().unwrap_or_default();
        if slices.next().is_none() {
            Some(first)
        } else {
            None
        }
    }
}

impl ULoanedContiguousZeroCopyRxFrame for ZenohRxFrame {
    fn loaned_contiguous_payload(&self) -> Result<LoanedPayload<'_>, UWireError> {
        let shm = self
            .sample
            .payload()
            .as_shm()
            .ok_or(UWireError::NotLoanBacked)?;
        // SAFETY:
        // - `as_shm()` succeeded, so Zenoh reports that the payload bytes are
        //   backed by shared-memory storage rather than a coalesced owned copy.
        // - The returned slice is borrowed from `self.sample` and cannot outlive
        //   the receive lease.
        // - Per https://doc.rust-lang.org/stable/std/slice/fn.from_raw_parts.html#safety,
        //   a borrowed slice must be valid for reads and contained within one
        //   allocation; Zenoh's `ZShm` lease supplies that external provenance.
        Ok(unsafe {
            LoanedPayload::new_unchecked(shm.as_ref(), PayloadLoanProvenance::SharedMemory)
        })
    }
}

#[async_trait]
impl UZeroCopyTransport for UPTransportZenoh {
    type Tx = ZenohTxBuffer;
    type Rx = ZenohRxFrame;

    async fn loan_tx(&self, spec: UTxLoanSpec) -> Result<Self::Tx, UStatus> {
        let metadata = spec.metadata().clone();
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(
            self,
            &metadata,
            spec.payload_len(),
            spec.payload_alignment(),
        )?;
        Ok(ZenohTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }

    async fn send_zero_copy(&self, buffer: Self::Tx) -> Result<(), UStatus> {
        validate_frame_metadata_for_payload(
            &buffer.metadata,
            buffer.metadata.encoding().is_some(),
        )?;
        let payload = if buffer.metadata.encoding().is_some() {
            buffer.payload.into_zbytes()
        } else {
            ZBytes::new()
        };

        self.session
            .put(&buffer.zenoh_key, payload)
            .priority(buffer.priority)
            .attachment(buffer.attachment)
            .await
            .inspect(|()| trace!("putting zero-copy frame with key: {}", buffer.zenoh_key))
            .map_err(|e| {
                UStatus::fail_with_code(UCode::INTERNAL, format!("failed to put Zenoh frame: {e}"))
            })?;
        Ok(())
    }

    async fn receive_zero_copy(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
    ) -> Result<Self::Rx, UStatus> {
        verify_filter_criteria(source_filter, sink_filter)?;
        let zenoh_key =
            to_zenoh_key_string(source_filter, sink_filter, self.local_authority.as_str());
        let subscriber = self
            .session
            .declare_subscriber(&zenoh_key)
            .await
            .map_err(|e| {
                UStatus::fail_with_code(
                    UCode::INTERNAL,
                    format!("failed to declare Zenoh subscriber: {e}"),
                )
            })?;
        loop {
            let sample = subscriber.recv_async().await.map_err(|e| {
                UStatus::fail_with_code(
                    UCode::INTERNAL,
                    format!("failed to receive Zenoh sample: {e}"),
                )
            })?;
            let Some(attachment) = sample.attachment() else {
                warn!(
                    "Ignoring Zenoh Sample without attachment [key expr: {}]",
                    sample.key_expr()
                );
                continue;
            };
            let metadata = attachment_to_frame_metadata(attachment).map_err(|e| {
                UStatus::fail_with_code(
                    UCode::INVALID_ARGUMENT,
                    format!("Unable to transform attachment to valid UFrameMetadata: {e}"),
                )
            })?;
            if metadata.encoding().is_none() && !sample.payload().is_empty() {
                return Err(UStatus::fail_with_code(
                    UCode::INVALID_ARGUMENT,
                    "Zenoh sample has payload bytes but no payload encoding",
                ));
            }
            validate_frame_metadata_for_payload(&metadata, metadata.encoding().is_some())?;
            if !sink_matches(metadata.attributes().sink(), sink_filter) {
                continue;
            }
            ensure_strict_shm_payload(&metadata, &sample)?;
            return Ok(ZenohRxFrame::new(metadata, sample));
        }
    }

    async fn register_zero_copy_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UZeroCopyListener<Self::Rx>>,
    ) -> Result<(), UStatus> {
        verify_filter_criteria(source_filter, sink_filter)?;
        let zenoh_key =
            to_zenoh_key_string(source_filter, sink_filter, self.local_authority.as_str());
        self.subscribers
            .register_zero_copy_subscriber(zenoh_key, listener)
            .await
    }

    async fn unregister_zero_copy_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UZeroCopyListener<Self::Rx>>,
    ) -> Result<(), UStatus> {
        verify_filter_criteria(source_filter, sink_filter)?;
        let zenoh_key =
            to_zenoh_key_string(source_filter, sink_filter, self.local_authority.as_str());
        self.subscribers
            .unregister_zero_copy(
                zenoh_key.as_str(),
                ComparableZeroCopyListener::new(listener),
            )
            .await
    }
}

#[async_trait]
impl UZeroCopyUninitTransport for UPTransportZenoh {
    type UninitTx = ZenohUninitTxBuffer;

    async fn loan_uninit_tx(&self, spec: UTxLoanSpec) -> Result<Self::UninitTx, UStatus> {
        let metadata = spec.metadata().clone();
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(
            self,
            &metadata,
            spec.payload_len(),
            spec.payload_alignment(),
        )?;
        Ok(ZenohUninitTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }
}

fn validate_alignment(alignment: usize) -> Result<(), UStatus> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            "payload alignment must be a non-zero power of two",
        ));
    }
    Ok(())
}

fn sink_matches(actual: Option<&UUri>, filter: Option<&UUri>) -> bool {
    filter.is_none_or(|filter| actual.is_some_and(|actual| filter.matches(actual)))
}

pub(crate) fn is_strict_shm_payload(metadata: &UFrameMetadata, sample: &Sample) -> bool {
    metadata.encoding().is_none()
        || sample.payload().is_empty()
        || sample.payload().as_shm().is_some()
}

fn ensure_strict_shm_payload(metadata: &UFrameMetadata, sample: &Sample) -> Result<(), UStatus> {
    if is_strict_shm_payload(metadata, sample) {
        return Ok(());
    }
    Err(UStatus::fail_with_code(
        UCode::FAILED_PRECONDITION,
        "zero-copy Zenoh receive requires SHM-backed payload bytes",
    ))
}

fn reserve_tx_parts(
    transport: &UPTransportZenoh,
    metadata: &UFrameMetadata,
    payload_len: usize,
    alignment: usize,
) -> Result<(String, ZBytes, zenoh::qos::Priority, ZenohTxPayload), UStatus> {
    validate_alignment(alignment)?;
    if metadata.encoding().is_none() && payload_len != 0 {
        return Err(UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            "message payload is present but payload encoding is absent",
        ));
    }
    validate_frame_metadata_for_payload(metadata, metadata.encoding().is_some())?;
    let zenoh_key = to_zenoh_key_string(
        metadata.attributes().source(),
        metadata.attributes().sink(),
        transport.local_authority.as_str(),
    );
    let attachment = frame_to_attachment(metadata).map_err(|e| {
        UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            format!("Unable to transform UFrameMetadata to attachment: {e}"),
        )
    })?;
    let priority = map_zenoh_priority(metadata.attributes().priority());
    let payload = reserve_payload(transport, payload_len, alignment)?;
    Ok((zenoh_key, attachment, priority, payload))
}

fn reserve_payload(
    transport: &UPTransportZenoh,
    payload_len: usize,
    alignment: usize,
) -> Result<ZenohTxPayload, UStatus> {
    if payload_len == 0 {
        return Ok(ZenohTxPayload::Empty);
    }

    let provider = transport.shm_provider()?;
    let aligned_len = align_len(payload_len, alignment)?;
    let layout = MemoryLayout::try_from(aligned_len).map_err(|err| {
        UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            format!("invalid Zenoh SHM allocation layout: {err}"),
        )
    })?;

    let mut payload = provider
        .alloc(layout)
        .with_policy::<GarbageCollect>()
        .wait()
        .map_err(|err| {
            UStatus::fail_with_code(
                UCode::RESOURCE_EXHAUSTED,
                format!("failed to allocate Zenoh SHM payload buffer: {err}"),
            )
        })?;

    if aligned_len != payload_len {
        let Some(payload_len) = NonZeroUsize::new(payload_len) else {
            unreachable!("zero-length payloads are handled before SHM allocation")
        };
        payload.try_resize(payload_len).ok_or_else(|| {
            UStatus::fail_with_code(
                UCode::INTERNAL,
                "failed to resize Zenoh SHM payload buffer after aligned allocation",
            )
        })?;
    }

    let address = payload.as_ref().as_ptr() as usize;
    if !address.is_multiple_of(alignment) {
        return Err(UStatus::fail_with_code(
            UCode::INTERNAL,
            format!(
                "Zenoh SHM payload address 0x{address:x} does not satisfy requested alignment {alignment}"
            ),
        ));
    }

    Ok(ZenohTxPayload::Shm(payload))
}

fn align_len(payload_len: usize, alignment: usize) -> Result<usize, UStatus> {
    let remainder = payload_len % alignment;
    if remainder == 0 {
        return Ok(payload_len);
    }
    payload_len
        .checked_add(alignment - remainder)
        .ok_or_else(|| UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "payload length overflow"))
}
