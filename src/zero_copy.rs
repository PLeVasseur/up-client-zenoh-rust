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

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{trace, warn};
use up_rust::{
    transport::verify_filter_criteria,
    validate_frame_metadata_for_payload,
    zero_copy::UZeroCopyListener,
    zero_copy::{UTxBuffer, UZeroCopyRxFrame, UZeroCopyTransport},
    UCode, UFrameMetadata, UStatus, UUri,
};
use zenoh::{
    bytes::{ZBytes, ZBytesReader, ZBytesSliceIterator},
    sample::Sample,
};
use zenoh::{
    shm::{AllocAlignment, GarbageCollect, MemoryLayout, OwnedShmBuf, ZShmMut},
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
/// [`UZeroCopyTransport::reserve`](up_rust::zero_copy::UZeroCopyTransport::reserve)
/// for [`UPTransportZenoh`]. The payload storage is backed by Zenoh SHM when the
/// frame has a payload. Frames without payload use an empty buffer and no SHM
/// allocation.
///
/// Callers normally do not construct this type directly. Use
/// [`UZeroCopyTransportExt::send_serialized_zero_copy`](up_rust::zero_copy::UZeroCopyTransportExt::send_serialized_zero_copy)
/// to reserve, serialize into, and send a loan in one step.
pub struct ZenohTxBuffer {
    metadata: UFrameMetadata,
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

    fn metadata_mut(&mut self) -> &mut UFrameMetadata {
        &mut self.metadata
    }

    fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    fn payload_mut(&mut self) -> &mut [u8] {
        self.payload.as_mut_slice()
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

#[async_trait]
impl UZeroCopyTransport for UPTransportZenoh {
    type Tx = ZenohTxBuffer;
    type Rx = ZenohRxFrame;

    async fn reserve(
        &self,
        metadata: UFrameMetadata,
        payload_len: usize,
        alignment: usize,
    ) -> Result<Self::Tx, UStatus> {
        validate_alignment(alignment)?;
        if metadata.encoding().is_none() && payload_len != 0 {
            return Err(UStatus::fail_with_code(
                UCode::INVALID_ARGUMENT,
                "message payload is present but payload encoding is absent",
            ));
        }
        validate_frame_metadata_for_payload(&metadata, metadata.encoding().is_some())?;
        let payload = reserve_payload(self, payload_len, alignment)?;
        Ok(ZenohTxBuffer { metadata, payload })
    }

    async fn send_zero_copy(&self, buffer: Self::Tx) -> Result<(), UStatus> {
        validate_frame_metadata_for_payload(
            &buffer.metadata,
            buffer.metadata.encoding().is_some(),
        )?;
        let zenoh_key = to_zenoh_key_string(
            buffer.metadata.attributes().source(),
            buffer.metadata.attributes().sink(),
            self.local_authority.as_str(),
        );
        let attachment = frame_to_attachment(&buffer.metadata).map_err(|e| {
            UStatus::fail_with_code(
                UCode::INVALID_ARGUMENT,
                format!("Unable to transform UFrameMetadata to attachment: {e}"),
            )
        })?;
        let priority = map_zenoh_priority(buffer.metadata.attributes().priority());
        let payload = if buffer.metadata.encoding().is_some() {
            buffer.payload.into_zbytes()
        } else {
            ZBytes::new()
        };

        self.session
            .put(&zenoh_key, payload)
            .priority(priority)
            .attachment(attachment)
            .await
            .inspect(|()| trace!("putting zero-copy frame with key: {zenoh_key}"))
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

fn reserve_payload(
    transport: &UPTransportZenoh,
    payload_len: usize,
    alignment: usize,
) -> Result<ZenohTxPayload, UStatus> {
    if payload_len == 0 {
        return Ok(ZenohTxPayload::Empty);
    }

    let provider = transport.shm_provider()?;
    let alloc_alignment = allocation_alignment(alignment)?;
    let aligned_len = align_len(payload_len, alignment)?;
    let layout = MemoryLayout::new(aligned_len, alloc_alignment).map_err(|err| {
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

    Ok(ZenohTxPayload::Shm(payload))
}

fn allocation_alignment(alignment: usize) -> Result<AllocAlignment, UStatus> {
    let pow = u8::try_from(alignment.trailing_zeros()).map_err(|_| {
        UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "payload alignment is too large")
    })?;
    AllocAlignment::new(pow).map_err(|err| {
        UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            format!("invalid Zenoh SHM payload alignment: {err}"),
        )
    })
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
