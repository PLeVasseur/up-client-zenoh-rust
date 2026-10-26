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
    borrow::Cow,
    hash::{Hash, Hasher},
    mem::MaybeUninit,
    num::NonZeroUsize,
    ops::Deref,
    sync::{Arc, Mutex, OnceLock},
};

use tracing::{trace, warn};
use up_rust::{
    FrameSink, LoanedPayload, PayloadLoanProvenance, UCode, UEncodedLoanedRxFrame, UEncodedRxFrame,
    UFrameMetadata, UStatus, UTxBuffer, UUninitTxBuffer, UUri, UWireError,
};
#[cfg(feature = "benchmark-owned")]
use up_rust::{UNativePrefixWireTransport, UWire, UWithNativePrefixWire};
use zenoh::{
    bytes::{ZBytes, ZBytesReader, ZBytesSliceIterator},
    sample::Sample,
    shm::{GarbageCollect, OwnedShmBuf, ZShmMut},
    Wait,
};

use crate::mechanics::{ZenohWireMechanics, DEFAULT_SHM_SEGMENT_SIZE};
use up_rust::ListenerAdmission;

type ZeroCopySubscriberMap = Mutex<
    std::collections::HashMap<(String, ComparableZeroCopyListener), Arc<DynamicRegistration>>,
>;

struct DynamicRegistration {
    listener: ComparableZeroCopyListener,
    activity: Arc<ListenerAdmission>,
    subscriber: Mutex<Option<zenoh::pubsub::Subscriber<()>>>,
    outcome: Mutex<Option<Result<(), UStatus>>>,
    ready: tokio::sync::Notify,
}

impl DynamicRegistration {
    async fn wait_ready(&self) -> Result<(), UStatus> {
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if let Some(outcome) = self.outcome.lock().unwrap().clone() {
                return outcome;
            }
            ready.await;
        }
    }

    fn close(&self, status: UStatus, close_sink: bool) -> Option<zenoh::pubsub::Subscriber<()>> {
        self.activity.cancel();
        if close_sink {
            self.listener.close();
        }
        let mut outcome = self.outcome.lock().unwrap();
        *outcome = Some(Err(status));
        let subscriber = self.subscriber.lock().unwrap().take();
        drop(outcome);
        self.ready.notify_waiters();
        subscriber
    }
}

struct CreatingDynamic<'a> {
    entries: &'a ZeroCopySubscriberMap,
    key: (String, ComparableZeroCopyListener),
    entry: Arc<DynamicRegistration>,
    armed: bool,
}

impl CreatingDynamic<'_> {
    fn fail(&mut self, status: UStatus) {
        let mut entries = self.entries.lock().unwrap();
        if entries
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            entries.remove(&self.key);
        }
        drop(entries);
        // Failed setup owns native admission, not the caller's reusable sink.
        drop(self.entry.close(status, false));
        self.armed = false;
    }
}

impl Drop for CreatingDynamic<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.fail(UStatus::fail_with_code(
                UCode::Cancelled,
                "dynamic listener creation cancelled",
            ));
        }
    }
}

/// Dynamic allocating Zenoh SHM control with selected-wire attachment carriage.
///
/// `UPTransportZenoh` remains the ordinary product [`up_rust::UTransport`]
/// implementation. This core owns a separate Zenoh transport instance for the
/// dynamic control, carrying already encoded selected
/// wire metadata as Zenoh attachment bytes and admitting received payload bytes
/// only when Zenoh reports SHM backing.
/// Native unregister closes admission and waits up to the configured quiescence
/// interval, except when re-entrant. Timeout retains live callback/sample owners;
/// it never invalidates a retained SHM loan. Classic `UPTransportZenoh` lifecycle
/// behavior is separate. See `docs/listener-lifecycle.md`.
pub struct ZenohZeroCopyCore {
    mechanics: ZenohWireMechanics,
    subscriber_map: ZeroCopySubscriberMap,
    listener_quiesce_timeout: std::time::Duration,
    publisher_readiness_timeout: Option<std::time::Duration>,
    prewarmed_shm_segment_size: Option<usize>,
}

impl Drop for ZenohZeroCopyCore {
    fn drop(&mut self) {
        // No registry lock: exclusive destruction closes all sinks before any
        // native handle/session is released, including blocked fan-out delivery.
        for entry in self.subscriber_map.get_mut().unwrap().values() {
            entry.activity.cancel();
            entry.listener.close();
        }
    }
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
    shm_prewarm: bool,
    listener_quiesce_timeout: std::time::Duration,
    publisher_readiness_timeout: Option<std::time::Duration>,
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
            shm_prewarm: false,
            listener_quiesce_timeout: std::time::Duration::from_secs(5),
            publisher_readiness_timeout: None,
        }
    }

    /// Returns the configured size of the eagerly initialized SHM provider.
    ///
    /// `None` means prewarming was disabled. This is a setup receipt, not an
    /// operating-system residency measurement or a count of bytes written here.
    #[must_use]
    pub fn prewarmed_shm_segment_size(&self) -> Option<usize> {
        self.prewarmed_shm_segment_size
    }

    /// Wraps this dynamic control's session in the owned selected-wire adapter.
    ///
    /// This allocating benchmark surface is separate from `ZenohPreparedCore`,
    /// which carries metadata in SHM and uses synchronous prepared routes.
    #[cfg(feature = "benchmark-owned")]
    #[must_use]
    pub fn with_selected_wire<W>(
        self,
        wire: W,
    ) -> UNativePrefixWireTransport<crate::ZenohOwnedCore, W>
    where
        W: UWire,
    {
        crate::ZenohOwnedCore::from_mechanics(self.mechanics.clone())
            .into_native_prefix_wire_transport(wire)
    }

    /// Returns the configured dynamic listener quiescence interval.
    #[must_use]
    pub fn listener_quiesce_timeout(&self) -> std::time::Duration {
        self.listener_quiesce_timeout
    }
}

impl ZenohZeroCopyCoreBuilder {
    /// Requires an actual matching Zenoh subscriber before publishing a native frame.
    ///
    /// Disabled by default. The deadline covers publisher declaration and matching;
    /// expiry returns `DeadlineExceeded` without publishing. Dropping a pending send
    /// cancels its wait and releases its owned buffer/publisher. This checks presence,
    /// not a subscriber count, remote application readiness or future delivery.
    /// It does not retransmit, change SHM allocation, or extend listener quiescence.
    ///
    /// # Errors
    /// Returns `InvalidArgument` for a zero or unrepresentable interval.
    pub fn with_publisher_readiness(
        mut self,
        timeout: std::time::Duration,
    ) -> Result<Self, UStatus> {
        if timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none() {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "publisher readiness timeout must be positive and representable",
            ));
        }
        self.publisher_readiness_timeout = Some(timeout);
        Ok(self)
    }

    /// Sets the native unregister deadline interval (five seconds by default).
    ///
    /// The interval begins when admission closes. Timeout reports
    /// `DeadlineExceeded`/`QuiesceTimedOut`, preserving entered sample ownership.
    ///
    /// # Errors
    /// Returns `InvalidArgument` for a zero or unrepresentable interval.
    pub fn with_listener_quiesce_timeout(
        mut self,
        timeout: std::time::Duration,
    ) -> Result<Self, UStatus> {
        if timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none() {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "listener quiescence timeout must be positive and representable",
            ));
        }
        self.listener_quiesce_timeout = timeout;
        Ok(self)
    }

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

    /// Materializes the configured SHM data region before the core becomes usable.
    ///
    /// Disabled by default, preserving lazy provider initialization. When enabled,
    /// construction initializes the actual provider through Zenoh's safe API before
    /// returning. The pinned POSIX implementation maps and locks the complete segment;
    /// setup failure fails construction. No temporary payload loan or redundant writes
    /// are required. Other platform residency is not established by this option.
    /// It does not change pool sizing, publication or retained sample lifetimes.
    #[must_use]
    pub fn with_shm_prewarm(mut self, enabled: bool) -> Self {
        self.shm_prewarm = enabled;
        self
    }

    /// Builds the selected-wire zero-copy core.
    ///
    /// # Errors
    ///
    /// Returns a status when the Zenoh session, URI validation, or SHM
    /// configuration or requested provider prewarming fails.
    pub async fn build(self) -> Result<ZenohZeroCopyCore, UStatus> {
        if self.shm_prewarm {
            let size = self.shm_segment_size.unwrap_or(DEFAULT_SHM_SEGMENT_SIZE);
            std::alloc::Layout::from_size_align(size, 1).map_err(|err| {
                UStatus::fail_with_code(
                    UCode::InvalidArgument,
                    format!("invalid Zenoh SHM prewarming size: {err}"),
                )
            })?;
        }
        let mut mechanics = ZenohWireMechanics::new(self.config, self.uri).await?;
        if let Some(shm_segment_size) = self.shm_segment_size {
            mechanics.set_shm_segment_size(shm_segment_size)?;
        }
        let prewarmed_shm_segment_size = self
            .shm_prewarm
            .then(|| mechanics.prewarm_shm())
            .transpose()?;
        Ok(ZenohZeroCopyCore {
            mechanics,
            subscriber_map: Mutex::new(std::collections::HashMap::new()),
            listener_quiesce_timeout: self.listener_quiesce_timeout,
            publisher_readiness_timeout: self.publisher_readiness_timeout,
            prewarmed_shm_segment_size,
        })
    }
}

/// Initialized send buffer retaining its payload allocation and outgoing metadata attachment.
pub struct ZenohTxBuffer {
    metadata: UFrameMetadata,
    zenoh_key: String,
    attachment: ZBytes,
    priority: zenoh::qos::Priority,
    payload: ZenohTxPayload,
}

/// Send buffer exposing payload storage for typed initialization before publication.
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

    fn payload_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.payload.as_uninit_mut_slice()
    }

    unsafe fn assume_payload_initialized(self) -> Self::Initialized {
        ZenohTxBuffer {
            metadata: self.metadata,
            zenoh_key: self.zenoh_key,
            attachment: self.attachment,
            priority: self.priority,
            payload: self.payload,
        }
    }
}

/// Received frame retaining the Zenoh sample and its opaque encoded metadata.
pub struct ZenohRxFrame {
    encoded_metadata: ZBytes,
    encoded_metadata_fallback: OnceLock<Vec<u8>>,
    sample: Sample,
}

impl ZenohRxFrame {
    fn new(encoded_metadata: ZBytes, sample: Sample) -> Self {
        Self {
            encoded_metadata,
            encoded_metadata_fallback: OnceLock::new(),
            sample,
        }
    }

    /// Borrows the retained native sample for no longer than this frame's lifetime.
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
        match self.encoded_metadata.to_bytes() {
            Cow::Borrowed(bytes) => bytes,
            Cow::Owned(bytes) => self
                .encoded_metadata_fallback
                .get_or_init(|| bytes)
                .as_slice(),
        }
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
        // as_shm() proves Zenoh retained a shared-memory lease for this sample.
        Ok(LoanedPayload::new(
            shm.as_ref(),
            PayloadLoanProvenance::OpaqueTransportLoan,
        ))
    }
}

impl up_rust::Frame for ZenohRxFrame {
    fn encoded_metadata(&self) -> &[u8] {
        UEncodedRxFrame::encoded_metadata(self)
    }

    fn payload(&self) -> Option<LoanedPayload<'_>> {
        let metadata = UEncodedRxFrame::encoded_metadata(self);
        crate::prepared::header_view(metadata)?.payload_encoding?;
        let payload = self.sample.payload();
        let bytes = if payload.is_empty() {
            &[]
        } else {
            payload.as_shm()?.as_ref()
        };
        Some(LoanedPayload::new(
            bytes,
            PayloadLoanProvenance::OpaqueTransportLoan,
        ))
    }
}

impl ZenohZeroCopyCore {
    /// Creates an allocating-control SHM loan with selected-wire attachment bytes.
    ///
    /// # Errors
    /// Rejects invalid payload presence/alignment or unavailable native SHM storage.
    pub fn loan_encoded(
        &self,
        metadata: UFrameMetadata,
        encoded_metadata: &[u8],
        payload_len: usize,
        alignment: usize,
    ) -> Result<ZenohTxBuffer, UStatus> {
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(
            &self.mechanics,
            &metadata,
            encoded_metadata,
            payload_len,
            alignment,
        )?;
        Ok(ZenohTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }

    /// Publishes an allocating control loan, optionally waiting for native matching.
    /// Refuses publication back into an entered local native sink on this
    /// synchronous callback stack before invoking the middleware.
    ///
    /// # Errors
    /// Reports native publisher/matching/publication failure or the readiness deadline.
    pub async fn publish_encoded(&self, buffer: ZenohTxBuffer) -> Result<(), UStatus> {
        let key = zenoh::key_expr::keyexpr::new(&buffer.zenoh_key)
            .map_err(|error| UStatus::fail_with_code(UCode::InvalidArgument, error.to_string()))?;
        if crate::native_callback::would_reenter(self.mechanics.session(), key) {
            return Err(UStatus::fail_with_code(
                UCode::FailedPrecondition,
                "publication would reenter an active native sink",
            ));
        }
        if let Some(timeout) = self.publisher_readiness_timeout {
            let deadline = tokio::time::Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| {
                    UStatus::fail_with_code(
                        UCode::InvalidArgument,
                        "publisher readiness deadline overflow",
                    )
                })?;
            let publisher = tokio::time::timeout_at(deadline, async {
                let publisher = self
                    .mechanics
                    .session()
                    .declare_publisher(buffer.zenoh_key.clone())
                    .priority(buffer.priority)
                    .await
                    .map_err(|error| {
                        UStatus::fail_with_code(
                            UCode::Internal,
                            format!("failed to declare readiness publisher: {error}"),
                        )
                    })?;
                loop {
                    if publisher
                        .matching_status()
                        .await
                        .map_err(|error| {
                            UStatus::fail_with_code(
                                UCode::Internal,
                                format!("failed to query subscriber matching: {error}"),
                            )
                        })?
                        .matching()
                    {
                        return Ok::<_, UStatus>(publisher);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| {
                UStatus::fail_with_code(
                    UCode::DeadlineExceeded,
                    "native Zenoh publisher readiness: no matching subscriber before deadline",
                )
            })??;
            publisher
                .put(buffer.payload.into_zbytes())
                .attachment(buffer.attachment)
                .await
                .map_err(|error| {
                    UStatus::fail_with_code(
                        UCode::Internal,
                        format!("failed to put matched Zenoh frame: {error}"),
                    )
                })?;
            trace!(
                "putting subscriber-matched zero-copy frame with key: {}",
                buffer.zenoh_key
            );
            return Ok(());
        }
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

    /// Receives an allocating-control sample retaining its native SHM lease.
    /// One pending operation has one nonblocking result slot; dropping its future
    /// retires the native subscription. There is no general-purpose receive FIFO.
    ///
    /// # Errors
    /// Reports declaration/receive failure or a non-SHM payload.
    pub async fn receive_encoded(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
    ) -> Result<ZenohRxFrame, UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        let _subscriber = self
            .mechanics
            .session()
            .declare_subscriber(&zenoh_key)
            .callback_mut(move |sample: Sample| {
                if sender.is_none() {
                    return;
                }
                let Some(attachment) = sample.attachment() else {
                    warn!(
                        "Ignoring Zenoh sample without zero-copy attachment [key expr: {}]",
                        sample.key_expr()
                    );
                    return;
                };
                let attachment = attachment.clone();
                let result = ensure_strict_shm_payload(&sample)
                    .map(|()| ZenohRxFrame::new(attachment, sample));
                if let Some(sender) = sender.take() {
                    let _ = sender.send(result);
                }
            })
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to declare Zenoh subscriber: {err}"),
                )
            })?;

        receiver.await.map_err(|error| {
            UStatus::fail_with_code(
                UCode::Internal,
                format!("failed to receive Zenoh sample: {error}"),
            )
        })?
    }

    /// Attaches an encoded sink to the dynamic allocating control.
    /// Duplicate registrations share pending setup without holding the registry
    /// lock across native declaration. Cancellation closes only that generation.
    ///
    /// # Errors
    /// Reports failure to declare the native subscriber.
    ///
    /// # Panics
    /// Panics if internal listener registration state was poisoned.
    pub async fn attach_encoded(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn FrameSink<ZenohRxFrame>>,
    ) -> Result<(), UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let comparable_listener = ComparableZeroCopyListener::new(listener);
        let key = (zenoh_key.clone(), comparable_listener.clone());
        let (entry, creating) = {
            let mut listeners = self.subscriber_map.lock().unwrap();
            if let Some(entry) = listeners.get(&key) {
                (Arc::clone(entry), false)
            } else {
                let entry = Arc::new(DynamicRegistration {
                    listener: comparable_listener,
                    activity: Arc::new(ListenerAdmission::new()),
                    subscriber: Mutex::new(None),
                    outcome: Mutex::new(None),
                    ready: tokio::sync::Notify::new(),
                });
                listeners.insert(key.clone(), Arc::clone(&entry));
                (entry, true)
            }
        };
        if !creating {
            return entry.wait_ready().await;
        }
        let mut creation = CreatingDynamic {
            entries: &self.subscriber_map,
            key,
            entry: Arc::clone(&entry),
            armed: true,
        };
        match self.declare_dynamic(&zenoh_key, &entry).await {
            Ok(subscriber) => {
                let rejected = {
                    let mut outcome = entry.outcome.lock().unwrap();
                    if outcome.is_some() {
                        Some(subscriber)
                    } else {
                        *entry.subscriber.lock().unwrap() = Some(subscriber);
                        *outcome = Some(Ok(()));
                        None
                    }
                };
                entry.ready.notify_waiters();
                if let Some(subscriber) = rejected {
                    subscriber.undeclare().await.map_err(|error| {
                        UStatus::fail_with_code(UCode::Internal, error.to_string())
                    })?;
                    return Err(UStatus::fail_with_code(
                        UCode::Cancelled,
                        "dynamic listener removed during creation",
                    ));
                }
                creation.armed = false;
                entry.wait_ready().await
            }
            Err(error) => {
                creation.fail(error.clone());
                Err(error)
            }
        }
    }

    async fn declare_dynamic(
        &self,
        key: &str,
        entry: &DynamicRegistration,
    ) -> Result<zenoh::pubsub::Subscriber<()>, UStatus> {
        let callback_listener = entry.listener.clone();
        let callback_activity = Arc::clone(&entry.activity);
        let callback = crate::native_callback::NativeCallback::new(self.mechanics.session(), key)
            .map_err(|()| {
            UStatus::fail_with_code(UCode::InvalidArgument, "invalid native callback key")
        })?;
        let subscriber = self
            .mechanics
            .session()
            .declare_subscriber(key)
            .callback_mut(move |sample: Sample| {
                let receive = || {
                    let _native = callback.enter();
                    let Some(_callback) = callback_activity.enter_native_callback() else {
                        return;
                    };
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
                    let frame = ZenohRxFrame::new(attachment.clone(), sample);
                    callback_listener.push(frame);
                };
                // Zenoh can call a local subscriber inline from an async put.
                // The SDK native guard is synchronous; enter Tokio's supported
                // blocking region so its admission lock is legal and other
                // runtime workers can make progress during bounded sink work.
                if tokio::runtime::Handle::try_current().is_ok() {
                    tokio::task::block_in_place(receive);
                } else {
                    receive();
                }
            })
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to register zero-copy listener: {err}"),
                )
            })?;
        Ok(subscriber)
    }

    /// Closes and detaches the exact encoded dynamic sink.
    ///
    /// # Errors
    /// Reports missing registration, native retirement failure or quiescence timeout.
    ///
    /// # Panics
    /// Panics if the internal listener registry was poisoned.
    pub async fn detach_encoded(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn FrameSink<ZenohRxFrame>>,
    ) -> Result<(), UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let entry = self
            .subscriber_map
            .lock()
            .unwrap()
            .remove(&(
                zenoh_key,
                ComparableZeroCopyListener::new(Arc::clone(&listener)),
            ))
            .ok_or_else(|| {
                UStatus::fail_with_code(UCode::NotFound, "zero-copy listener not registered")
            })?;
        let subscriber = entry.close(
            UStatus::fail_with_code(UCode::Cancelled, "dynamic listener removed"),
            true,
        );
        let activity = &entry.activity;
        let deadline = activity
            .closed_at()
            .and_then(|closed| closed.checked_add(self.listener_quiesce_timeout))
            .ok_or_else(|| {
                UStatus::fail_with_code(UCode::InvalidArgument, "quiescence deadline overflow")
            })?;
        let quiescence = if activity.is_current_callback() {
            Ok(())
        } else {
            activity.quiesce(deadline).await
        };
        if let Some(subscriber) = subscriber {
            tokio::time::timeout_at(deadline, subscriber.undeclare())
                .await
                .map_err(|_| {
                    UStatus::fail_with_code(
                        UCode::DeadlineExceeded,
                        "Zenoh SHM listener retirement: QuiesceTimedOut",
                    )
                })?
                .map_err(|err| {
                    UStatus::fail_with_code(
                        UCode::Internal,
                        format!("failed to undeclare zero-copy listener: {err}"),
                    )
                })?;
        }
        quiescence.map_err(|error| {
            UStatus::fail_with_code(
                if matches!(error, up_rust::ListenerQuiesceError::QuiesceTimedOut) {
                    UCode::DeadlineExceeded
                } else {
                    UCode::FailedPrecondition
                },
                format!("Zenoh SHM listener quiescence: {error:?}"),
            )
        })?;
        Ok(())
    }
}

impl ZenohZeroCopyCore {
    /// Creates an allocating SHM control loan for typed payload initialization.
    ///
    /// # Errors
    /// Rejects invalid payload presence/alignment or unavailable native SHM storage.
    pub fn loan_encoded_uninit(
        &self,
        metadata: UFrameMetadata,
        encoded_metadata: &[u8],
        payload_len: usize,
        alignment: usize,
    ) -> Result<ZenohUninitTxBuffer, UStatus> {
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(
            &self.mechanics,
            &metadata,
            encoded_metadata,
            payload_len,
            alignment,
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

fn reserve_tx_parts(
    transport: &ZenohWireMechanics,
    metadata: &UFrameMetadata,
    encoded_metadata: &[u8],
    payload_len: usize,
    alignment: usize,
) -> Result<(String, ZBytes, zenoh::qos::Priority, ZenohTxPayload), UStatus> {
    if metadata.payload_encoding().is_none() && payload_len != 0 {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "message payload is present but payload encoding is absent",
        ));
    }
    let zenoh_key = transport.to_zenoh_key_string(metadata.source(), metadata.sink());
    let attachment = ZBytes::from(encoded_metadata.to_vec());
    let priority = crate::mechanics::map_zenoh_priority(
        metadata.priority().unwrap_or(up_rust::UPriority::CS1),
    );
    let payload = reserve_payload(transport, payload_len, alignment)?;
    Ok((zenoh_key, attachment, priority, payload))
}

fn reserve_payload(
    transport: &ZenohWireMechanics,
    payload_len: usize,
    alignment: usize,
) -> Result<ZenohTxPayload, UStatus> {
    if !alignment.is_power_of_two() {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "payload alignment must be a nonzero power of two",
        ));
    }
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
    if !address.is_multiple_of(alignment) {
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
    listener: Arc<dyn FrameSink<ZenohRxFrame>>,
}

impl ComparableZeroCopyListener {
    fn new(listener: Arc<dyn FrameSink<ZenohRxFrame>>) -> Self {
        Self { listener }
    }

    fn pointer_address(&self) -> usize {
        Arc::as_ptr(&self.listener).cast::<()>() as usize
    }
}

impl Deref for ComparableZeroCopyListener {
    type Target = dyn FrameSink<ZenohRxFrame>;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{poll_fn, Future},
        sync::atomic::{AtomicBool, Ordering},
        task::Poll,
        time::Duration,
    };
    use up_rust::{
        OverflowPolicy, PayloadEncoding, ProtobufWire, RegistrationConfig, RegistrationInbox,
        UFrameFieldsMetadataCodec, UWireMetadataCodec, UWireMetadataContext,
    };

    fn topic() -> UUri {
        UUri::try_from("//dynamic-lifecycle/4210/1/9000").unwrap()
    }

    async fn core() -> ZenohZeroCopyCore {
        let mut config = zenoh::Config::default();
        config.insert_json5("listen/endpoints", "[]").unwrap();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        config
            .insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        ZenohZeroCopyCore::builder("//dynamic-lifecycle/4210/1/0")
            .with_config(config)
            .with_shm_segment_size(1024 * 1024)
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    fn buffer(core: &ZenohZeroCopyCore, value: u64) -> ZenohTxBuffer {
        let metadata = UFrameMetadata::publish(topic())
            .with_payload_encoding(PayloadEncoding::RAW)
            .build()
            .unwrap();
        let encoded = UFrameFieldsMetadataCodec
            .encode_frame_metadata(
                UWireMetadataContext::from_wire::<ProtobufWire, UFrameFieldsMetadataCodec>(),
                &metadata,
            )
            .unwrap();
        let mut buffer = core.loan_encoded(metadata, &encoded, 8, 8).unwrap();
        buffer.payload_mut().copy_from_slice(&value.to_le_bytes());
        buffer
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn dynamic_pull_retains_first_shm_result_without_blocking_native_burst() {
        let core = core().await;
        let source = topic();
        let mut receive = Box::pin(core.receive_encoded(&source, None));
        assert!(poll_fn(|cx| Poll::Ready(receive.as_mut().poll(cx)))
            .await
            .is_pending());
        for value in 0..512 {
            core.publish_encoded(buffer(&core, value)).await.unwrap();
        }
        let frame = receive.await.unwrap();
        assert!(frame.sample().payload().as_shm().is_some());
        let publisher = core
            .mechanics
            .session()
            .declare_publisher(core.mechanics.to_zenoh_key_string(&source, None))
            .await
            .unwrap();
        assert!(!publisher.matching_status().await.unwrap().matching());
        let mut cancelled = Box::pin(core.receive_encoded(&source, None));
        assert!(poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx)))
            .await
            .is_pending());
        drop(cancelled);
        assert!(!publisher.matching_status().await.unwrap().matching());
        drop(publisher);
        drop(core);
        assert_eq!(
            up_rust::Frame::payload(&frame).unwrap().bytes(),
            0u64.to_le_bytes()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn dynamic_duplicate_registration_has_one_native_delivery_and_retirement() {
        let core = core().await;
        let inbox = Arc::new(
            RegistrationInbox::new(RegistrationConfig {
                capacity: 1,
                overflow: OverflowPolicy::DropNewest,
            })
            .unwrap(),
        );
        let listener: Arc<dyn FrameSink<ZenohRxFrame>> = inbox.clone();
        let source = topic();
        let (left, right) = tokio::join!(
            core.attach_encoded(&source, None, Arc::clone(&listener)),
            core.attach_encoded(&source, None, Arc::clone(&listener)),
        );
        left.unwrap();
        right.unwrap();
        core.publish_encoded(buffer(&core, 0)).await.unwrap();
        let frame = inbox.recv().await.unwrap();
        assert_eq!(inbox.stats().admitted.load(Ordering::SeqCst), 1);
        assert_eq!(inbox.stats().full.load(Ordering::SeqCst), 0);
        core.detach_encoded(&source, None, listener).await.unwrap();
        assert!(core.subscriber_map.lock().unwrap().is_empty());
        drop(core);
        assert_eq!(
            up_rust::Frame::payload(&frame).unwrap().bytes(),
            0u64.to_le_bytes()
        );
    }

    struct ClosingSink(AtomicBool);
    impl FrameSink<ZenohRxFrame> for ClosingSink {
        fn push(&self, _: ZenohRxFrame) {}
        fn close(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn failed_dynamic_setup_releases_entry_without_closing_callers_sink() {
        let core = core().await;
        let sink = Arc::new(ClosingSink(AtomicBool::new(false)));
        core.mechanics.session().close().await.unwrap();
        assert!(core
            .attach_encoded(&topic(), None, sink.clone())
            .await
            .is_err());
        assert!(core.subscriber_map.lock().unwrap().is_empty());
        assert!(!sink.0.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn dynamic_core_drop_closes_a_blocked_native_sink_without_revoking_prior_sample() {
        let core = core().await;
        let inbox = Arc::new(
            RegistrationInbox::new(RegistrationConfig {
                capacity: 1,
                overflow: OverflowPolicy::Block {
                    deadline: Duration::from_secs(1),
                },
            })
            .unwrap(),
        );
        core.attach_encoded(&topic(), None, inbox.clone())
            .await
            .unwrap();
        core.publish_encoded(buffer(&core, 0)).await.unwrap();
        let session = Arc::clone(core.mechanics.session());
        let next = buffer(&core, 1);
        let (finished, received) = tokio::sync::oneshot::channel();
        let producer = std::thread::spawn(move || {
            session
                .put(next.zenoh_key, next.payload.into_zbytes())
                .attachment(next.attachment)
                .priority(next.priority)
                .wait()
                .unwrap();
            finished.send(()).unwrap();
        });
        tokio::time::timeout(Duration::from_millis(500), async {
            while inbox.stats().blocked.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(core);
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        assert_eq!(inbox.stats().after_close.load(Ordering::SeqCst), 1);
        assert_eq!(inbox.stats().deadline_expired.load(Ordering::SeqCst), 0);
        let frame = inbox.recv().await.unwrap();
        assert_eq!(
            up_rust::Frame::payload(&frame).unwrap().bytes(),
            0u64.to_le_bytes()
        );
    }

    struct Observe(Arc<Mutex<Vec<u64>>>);
    impl up_rust::RegistrationHandler<ZenohRxFrame> for Observe {
        async fn on_receive(&mut self, frame: ZenohRxFrame) {
            assert!(frame.sample().payload().as_shm().is_some());
            let value = u64::from_le_bytes(
                up_rust::Frame::payload(&frame)
                    .unwrap()
                    .bytes()
                    .try_into()
                    .unwrap(),
            );
            if value == 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            self.0.lock().unwrap().push(value);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn dynamic_shm_native_sender_reaches_two_sdk_workers_in_fifo_order() {
        const COUNT: usize = 1000;
        let core = core().await;
        let left = Arc::new(Mutex::new(Vec::with_capacity(COUNT)));
        let right = Arc::new(Mutex::new(Vec::with_capacity(COUNT)));
        let config = RegistrationConfig {
            capacity: 4,
            overflow: OverflowPolicy::Block {
                deadline: Duration::from_secs(1),
            },
        };
        let first = up_rust::Registration::new(config, Observe(Arc::clone(&left)), || {}).unwrap();
        let second =
            up_rust::Registration::new(config, Observe(Arc::clone(&right)), || {}).unwrap();
        let first_sink: Arc<dyn FrameSink<ZenohRxFrame>> = first.inbox();
        let second_sink: Arc<dyn FrameSink<ZenohRxFrame>> = second.inbox();
        core.attach_encoded(&topic(), None, Arc::clone(&first_sink))
            .await
            .unwrap();
        core.attach_encoded(&topic(), None, Arc::clone(&second_sink))
            .await
            .unwrap();
        for value in 0..u64::try_from(COUNT).unwrap() {
            core.publish_encoded(buffer(&core, value)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        core.detach_encoded(&topic(), None, first_sink)
            .await
            .unwrap();
        core.detach_encoded(&topic(), None, second_sink)
            .await
            .unwrap();
        first.stop().await.unwrap();
        second.stop().await.unwrap();
        let expected: Vec<_> = (0..u64::try_from(COUNT).unwrap()).collect();
        assert_eq!(*left.lock().unwrap(), expected);
        assert_eq!(*right.lock().unwrap(), expected);
    }

    struct DirectPublication {
        core: std::sync::Weak<ZenohZeroCopyCore>,
        refused: AtomicBool,
    }
    impl FrameSink<ZenohRxFrame> for DirectPublication {
        fn push(&self, frame: ZenohRxFrame) {
            let core = self.core.upgrade().unwrap();
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(core.publish_encoded(buffer(&core, 99)))
            });
            self.refused.store(
                matches!(result, Err(error) if error.code() == UCode::FailedPrecondition),
                Ordering::SeqCst,
            );
            assert_eq!(
                up_rust::Frame::payload(&frame).unwrap().bytes(),
                0u64.to_le_bytes()
            );
        }
        fn close(&self) {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn dynamic_native_sink_refuses_synchronous_self_publication_before_put() {
        let core = Arc::new(core().await);
        let sink = Arc::new(DirectPublication {
            core: Arc::downgrade(&core),
            refused: AtomicBool::new(false),
        });
        core.attach_encoded(&topic(), None, sink.clone())
            .await
            .unwrap();
        core.publish_encoded(buffer(&core, 0)).await.unwrap();
        assert!(sink.refused.load(Ordering::SeqCst));
        core.detach_encoded(&topic(), None, sink).await.unwrap();
    }
}
