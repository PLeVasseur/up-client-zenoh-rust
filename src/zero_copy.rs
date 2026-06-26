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

use std::{
    hash::{Hash, Hasher},
    mem::MaybeUninit,
    num::NonZeroUsize,
    ops::Deref,
    sync::Arc,
};

use async_trait::async_trait;
use tracing::{trace, warn};
use up_rust::{
    LoanedPayload, NativePrefixProtobufMetadataCodec, PayloadAlignment, PayloadLoanProvenance,
    PreparedTxLoanSpec, UCode, UEncodedLoanedRxFrame, UEncodedRxFrame, UEncodedZeroCopyListener,
    UFrameMetadata, UStatus, UTxBuffer, UUninitTxBuffer, UUri, UWire, UWireError, UWireTransport,
    UZeroCopyTransportCore, UZeroCopyUninitTransportCore,
};
use zenoh::{
    bytes::{ZBytes, ZBytesReader, ZBytesSliceIterator},
    sample::Sample,
    shm::{GarbageCollect, OwnedShmBuf, ZShmMut},
    Wait,
};

use crate::mechanics::ZenohWireMechanics;

type ZeroCopySubscriberMap = Arc<
    tokio::sync::Mutex<
        std::collections::HashMap<
            (String, ComparableZeroCopyListener),
            zenoh::pubsub::Subscriber<()>,
        >,
    >,
>;

/// Real Zenoh SHM zero-copy selected-wire transport core.
///
/// `UPTransportZenoh` remains the ordinary product [`up_rust::UTransport`]
/// implementation. This core owns a separate Zenoh transport instance for the
/// generic selected-wire zero-copy adapter, carrying already encoded selected
/// wire metadata as Zenoh attachment bytes and admitting received payload bytes
/// only when Zenoh reports SHM backing.
pub struct ZenohZeroCopyCore {
    mechanics: ZenohWireMechanics,
    subscriber_map: ZeroCopySubscriberMap,
}

/// Builder for [`ZenohZeroCopyCore`].
///
/// The builder keeps zero-copy SHM configuration on the selected-wire core
/// construction path instead of making the legacy `UPTransportZenoh` API the
/// primary zero-copy surface.
pub struct ZenohZeroCopyCoreBuilder {
    config: crate::zenoh_config::Config,
    uri: String,
    shm_segment_size: Option<usize>,
}

impl ZenohZeroCopyCore {
    /// Creates a zero-copy core with its own Zenoh transport internals.
    ///
    /// This is a convenience wrapper around [`Self::builder`]. Use the builder
    /// when tests or deployments need to tune the SHM segment size.
    ///
    /// # Errors
    ///
    /// Returns a status when the Zenoh session, URI validation, or SHM
    /// configuration fails.
    pub async fn new(
        config: crate::zenoh_config::Config,
        uri: impl Into<String>,
    ) -> Result<Self, UStatus> {
        Self::builder(uri).with_config(config).build().await
    }

    /// Starts a builder for an independently constructed zero-copy core.
    #[must_use]
    pub fn builder(uri: impl Into<String>) -> ZenohZeroCopyCoreBuilder {
        ZenohZeroCopyCoreBuilder {
            config: crate::zenoh_config::Config::default(),
            uri: uri.into(),
            shm_segment_size: None,
        }
    }

    /// Wraps this core in the generic selected-wire adapter.
    #[must_use]
    pub fn with_selected_wire<W>(
        self,
        wire: W,
    ) -> UWireTransport<Self, W, NativePrefixProtobufMetadataCodec>
    where
        W: UWire,
    {
        UWireTransport::new(self, wire, NativePrefixProtobufMetadataCodec)
    }
}

impl ZenohZeroCopyCoreBuilder {
    /// Overrides the Zenoh configuration used to open the core's session.
    #[must_use]
    pub fn with_config(mut self, config: crate::zenoh_config::Config) -> Self {
        self.config = config;
        self
    }

    /// Overrides the SHM provider segment size used by transmit loans.
    ///
    /// # Errors
    ///
    /// Returns [`UCode::InvalidArgument`] when `shm_segment_size` is zero.
    pub fn with_shm_segment_size(mut self, shm_segment_size: usize) -> Result<Self, UStatus> {
        if shm_segment_size == 0 {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Zenoh SHM segment size must be non-zero",
            ));
        }
        self.shm_segment_size = Some(shm_segment_size);
        Ok(self)
    }

    /// Builds the selected-wire zero-copy core.
    ///
    /// # Errors
    ///
    /// Returns a status when the Zenoh session, URI validation, or SHM
    /// configuration fails.
    pub async fn build(self) -> Result<ZenohZeroCopyCore, UStatus> {
        let mut mechanics = ZenohWireMechanics::new(self.config, self.uri).await?;
        if let Some(shm_segment_size) = self.shm_segment_size {
            mechanics.set_shm_segment_size(shm_segment_size)?;
        }
        Ok(ZenohZeroCopyCore {
            mechanics,
            subscriber_map: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }
}

pub struct ZenohTxBuffer {
    metadata: UFrameMetadata,
    zenoh_key: String,
    attachment: ZBytes,
    priority: zenoh::qos::Priority,
    payload: ZenohTxPayload,
}

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
                // SAFETY: MaybeUninit<u8> has byte layout and the slice is exclusively borrowed.
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

impl ZenohTxBuffer {
    /// Returns the selected-wire metadata bytes carried as the Zenoh attachment.
    #[must_use]
    pub fn attachment_bytes(&self) -> Vec<u8> {
        self.attachment.to_bytes().to_vec()
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
}

impl UUninitTxBuffer for ZenohUninitTxBuffer {
    type Initialized = ZenohTxBuffer;

    fn metadata(&self) -> &UFrameMetadata {
        &self.metadata
    }

    fn payload_len(&self) -> usize {
        self.payload.as_slice().len()
    }

    fn payload_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.payload.as_uninit_mut_slice()
    }

    unsafe fn assume_payload_init(self) -> Self::Initialized {
        ZenohTxBuffer {
            metadata: self.metadata,
            zenoh_key: self.zenoh_key,
            attachment: self.attachment,
            priority: self.priority,
            payload: self.payload,
        }
    }
}

pub struct ZenohRxFrame {
    encoded_metadata: Vec<u8>,
    sample: Sample,
}

impl ZenohRxFrame {
    fn new(encoded_metadata: Vec<u8>, sample: Sample) -> Self {
        Self {
            encoded_metadata,
            sample,
        }
    }

    #[must_use]
    pub fn sample(&self) -> &Sample {
        &self.sample
    }
}

impl UEncodedRxFrame for ZenohRxFrame {
    type PayloadReader<'a>
        = ZBytesReader<'a>
    where
        Self: 'a;
    type PayloadSlices<'a>
        = ZBytesSliceIterator<'a>
    where
        Self: 'a;

    fn encoded_metadata(&self) -> &[u8] {
        &self.encoded_metadata
    }

    fn payload_len(&self) -> usize {
        self.sample.payload().len()
    }

    fn payload_reader(&self) -> Self::PayloadReader<'_> {
        self.sample.payload().reader()
    }

    fn payload_slices(&self) -> Self::PayloadSlices<'_> {
        self.sample.payload().slices()
    }

    fn try_contiguous_payload(&self) -> Option<&[u8]> {
        let mut slices = self.sample.payload().slices();
        let first = slices.next().unwrap_or_default();
        if slices.next().is_none() {
            Some(first)
        } else {
            None
        }
    }
}

impl UEncodedLoanedRxFrame for ZenohRxFrame {
    fn loaned_contiguous_payload(&self) -> Result<LoanedPayload<'_>, UWireError> {
        let shm = self.sample.payload().as_shm().ok_or_else(|| {
            UWireError::invalid_payload("zero-copy Zenoh receive payload is not SHM-backed")
        })?;
        // SAFETY: as_shm() proves Zenoh retained a shared-memory lease for this sample.
        Ok(unsafe {
            LoanedPayload::new_unchecked(shm.as_ref(), PayloadLoanProvenance::OpaqueTransportLoan)
        })
    }
}

#[async_trait]
impl UZeroCopyTransportCore for ZenohZeroCopyCore {
    type Tx = ZenohTxBuffer;
    type Rx = ZenohRxFrame;

    async fn loan_prepared_tx(&self, spec: PreparedTxLoanSpec) -> Result<Self::Tx, UStatus> {
        let metadata = spec.metadata().clone();
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(&self.mechanics, &spec)?;
        Ok(ZenohTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }

    async fn send_prepared_zero_copy(&self, buffer: Self::Tx) -> Result<(), UStatus> {
        let payload = buffer.payload.into_zbytes();
        self.mechanics
            .session()
            .put(&buffer.zenoh_key, payload)
            .priority(buffer.priority)
            .attachment(buffer.attachment)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to put Zenoh frame: {err}"),
                )
            })?;
        trace!("putting zero-copy frame with key: {}", buffer.zenoh_key);
        Ok(())
    }

    async fn receive_encoded_zero_copy(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
    ) -> Result<Self::Rx, UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let subscriber = self
            .mechanics
            .session()
            .declare_subscriber(&zenoh_key)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to declare Zenoh subscriber: {err}"),
                )
            })?;

        loop {
            let sample = subscriber.recv_async().await.map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to receive Zenoh sample: {err}"),
                )
            })?;
            let Some(attachment) = sample.attachment() else {
                warn!(
                    "Ignoring Zenoh sample without zero-copy attachment [key expr: {}]",
                    sample.key_expr()
                );
                continue;
            };
            ensure_strict_shm_payload(&sample)?;
            return Ok(ZenohRxFrame::new(attachment.to_bytes().to_vec(), sample));
        }
    }

    async fn register_encoded_zero_copy_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedZeroCopyListener<Self::Rx>>,
    ) -> Result<(), UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let comparable_listener = ComparableZeroCopyListener::new(listener);
        let mut listeners = self.subscriber_map.lock().await;
        if listeners.contains_key(&(zenoh_key.clone(), comparable_listener.clone())) {
            return Ok(());
        }

        let callback_listener = comparable_listener.clone();
        let subscriber = self
            .mechanics
            .session()
            .declare_subscriber(&zenoh_key)
            .callback_mut(move |sample: Sample| {
                let Some(attachment) = sample.attachment() else {
                    warn!(
                        "Ignoring Zenoh sample without zero-copy attachment [key expr: {}]",
                        sample.key_expr()
                    );
                    return;
                };
                if let Err(err) = ensure_strict_shm_payload(&sample) {
                    warn!("Dropping non-SHM Zenoh zero-copy sample: {err:?}");
                    return;
                }
                let frame = ZenohRxFrame::new(attachment.to_bytes().to_vec(), sample);
                let listener = callback_listener.clone();
                tokio::spawn(async move {
                    listener.on_receive_encoded_zero_copy(frame).await;
                });
            })
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to register zero-copy listener: {err}"),
                )
            })?;
        listeners.insert((zenoh_key, comparable_listener), subscriber);
        Ok(())
    }

    async fn unregister_encoded_zero_copy_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedZeroCopyListener<Self::Rx>>,
    ) -> Result<(), UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let subscriber = self
            .subscriber_map
            .lock()
            .await
            .remove(&(zenoh_key, ComparableZeroCopyListener::new(listener)))
            .ok_or_else(|| {
                UStatus::fail_with_code(UCode::NotFound, "zero-copy listener not registered")
            })?;
        subscriber.undeclare().await.map_err(|err| {
            UStatus::fail_with_code(
                UCode::Internal,
                format!("failed to undeclare zero-copy listener: {err}"),
            )
        })?;
        Ok(())
    }
}

#[async_trait]
impl UZeroCopyUninitTransportCore for ZenohZeroCopyCore {
    type UninitTx = ZenohUninitTxBuffer;

    async fn loan_prepared_uninit_tx(
        &self,
        spec: PreparedTxLoanSpec,
    ) -> Result<Self::UninitTx, UStatus> {
        let metadata = spec.metadata().clone();
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(&self.mechanics, &spec)?;
        Ok(ZenohUninitTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }
}

fn reserve_tx_parts(
    transport: &ZenohWireMechanics,
    spec: &PreparedTxLoanSpec,
) -> Result<(String, ZBytes, zenoh::qos::Priority, ZenohTxPayload), UStatus> {
    let payload_alignment = spec.payload_alignment_proof();
    if !spec.has_payload() && spec.payload_len() != 0 {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "message payload is present but payload encoding is absent",
        ));
    }
    let metadata = spec.metadata();
    let zenoh_key =
        transport.to_zenoh_key_string(metadata.attributes().source(), metadata.attributes().sink());
    let attachment = ZBytes::from(spec.encoded_metadata().to_vec());
    let priority = crate::mechanics::map_zenoh_priority(
        metadata
            .attributes()
            .priority()
            .unwrap_or(up_rust::UPriority::CS1),
    );
    let payload = reserve_payload(transport, spec.payload_len(), payload_alignment)?;
    Ok((zenoh_key, attachment, priority, payload))
}

fn reserve_payload(
    transport: &ZenohWireMechanics,
    payload_len: usize,
    alignment: PayloadAlignment,
) -> Result<ZenohTxPayload, UStatus> {
    let alignment = alignment.as_usize();
    if payload_len == 0 {
        return Ok(ZenohTxPayload::Empty);
    }
    let provider = transport.shm_provider()?;
    let aligned_len = align_len(payload_len, alignment)?;
    let layout = provider.alloc_layout(aligned_len).map_err(|err| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!("invalid Zenoh SHM layout: {err}"),
        )
    })?;
    let mut payload = layout
        .alloc()
        .with_policy::<GarbageCollect>()
        .wait()
        .map_err(|err| {
            UStatus::fail_with_code(
                UCode::ResourceExhausted,
                format!("failed to allocate Zenoh SHM payload: {err}"),
            )
        })?;
    if aligned_len != payload_len {
        let payload_len = NonZeroUsize::new(payload_len).expect("non-zero payload length");
        payload.try_resize(payload_len).ok_or_else(|| {
            UStatus::fail_with_code(UCode::Internal, "failed to resize Zenoh SHM payload buffer")
        })?;
    }
    let address = payload.as_ref().as_ptr() as usize;
    if address % alignment != 0 {
        return Err(UStatus::fail_with_code(
            UCode::Internal,
            format!("Zenoh SHM payload address 0x{address:x} does not satisfy requested alignment {alignment}"),
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
        .ok_or_else(|| UStatus::fail_with_code(UCode::InvalidArgument, "payload length overflow"))
}

fn ensure_strict_shm_payload(sample: &Sample) -> Result<(), UStatus> {
    if sample.payload().is_empty() || sample.payload().as_shm().is_some() {
        return Ok(());
    }
    Err(UStatus::fail_with_code(
        UCode::FailedPrecondition,
        "zero-copy Zenoh receive requires SHM-backed payload bytes",
    ))
}

#[derive(Clone)]
pub(crate) struct ComparableZeroCopyListener {
    listener: Arc<dyn UEncodedZeroCopyListener<ZenohRxFrame>>,
}

impl ComparableZeroCopyListener {
    fn new(listener: Arc<dyn UEncodedZeroCopyListener<ZenohRxFrame>>) -> Self {
        Self { listener }
    }

    fn pointer_address(&self) -> usize {
        Arc::as_ptr(&self.listener).cast::<()>() as usize
    }
}

impl Deref for ComparableZeroCopyListener {
    type Target = dyn UEncodedZeroCopyListener<ZenohRxFrame>;

    fn deref(&self) -> &Self::Target {
        &*self.listener
    }
}

impl Hash for ComparableZeroCopyListener {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pointer_address().hash(state);
    }
}

impl PartialEq for ComparableZeroCopyListener {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.listener, &other.listener)
    }
}

impl Eq for ComparableZeroCopyListener {}
