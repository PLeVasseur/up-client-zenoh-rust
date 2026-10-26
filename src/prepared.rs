// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0

//! Setup-reserved Zenoh publishers and metadata inside native SHM leases.

use std::{
    mem::MaybeUninit,
    num::NonZeroUsize,
    ops::Range,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use up_rust::{
    Frame, FrameSink, InitializedTx, LoanError, LoanedPayload, PayloadLoanProvenance,
    PreparedTransportCore, PreparedTxStorage, RouteLimits, SendError, SetupError, TxFrameLayout,
    UCode, UPriority, UStatus, UUri, WrittenHeader,
};
use zenoh::{
    pubsub::{Publisher, Subscriber},
    qos::CongestionControl,
    sample::Sample,
    shm::{GarbageCollect, OwnedShmBuf, ZShmMut},
    Wait,
};

use crate::mechanics::{ZenohShmProvider, ZenohWireMechanics};

const PLACEMENT_LEN: usize = 24;

/// Optional allocation observation hook installed at transport setup.
///
/// Implementations keep a per-thread scope flag without allocating. The binding
/// enters its own code, suspends only around Zenoh calls, and re-enters callbacks
/// even when invoked before a suspended call returns. Whole-process allocation
/// counters remain independent and include middleware and observer costs.
pub trait ZenohAllocationObserver: Send + Sync {
    /// Replaces the calling thread's counted flag and returns its previous value.
    fn replace_scope(&self, counted: bool) -> bool;
}

struct Scope {
    observer: Option<&'static dyn ZenohAllocationObserver>,
    previous: bool,
}
impl Scope {
    fn enter(observer: Option<&'static dyn ZenohAllocationObserver>) -> Self {
        Self {
            observer,
            previous: observer.is_some_and(|value| value.replace_scope(true)),
        }
    }
    fn middleware<T>(&self, call: impl FnOnce() -> T) -> T {
        let suspension = Self {
            observer: self.observer,
            previous: self
                .observer
                .is_some_and(|value| value.replace_scope(false)),
        };
        let result = call();
        drop(suspension);
        result
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(observer) = self.observer {
            observer.replace_scope(self.previous);
        }
    }
}

/// Exact logical address and maximum native frame reserved during setup.
#[derive(Clone, Debug)]
pub struct ZenohRouteReservation {
    /// Exact source carried by this logical route.
    pub source: UUri,
    /// Exact destination, or None for sinkless publications.
    pub sink: Option<UUri>,
    /// Maximum metadata/payload lengths and payload alignment.
    pub limits: RouteLimits,
    /// Whether this process declares native publishers for this address.
    pub transmit: bool,
}

/// Finite setup capacities for a prepared native Zenoh core.
pub struct ZenohPreparedConfig {
    /// Size of the actual POSIX SHM provider, initialized before traffic.
    pub shm_segment_size: usize,
    /// Maximum simultaneously prepared logical SDK routes.
    pub route_capacity: usize,
    /// Maximum simultaneously attached native registrations.
    pub registration_capacity: usize,
    /// Optional source-scoped observer; never subtract its allocation costs.
    pub allocation_observer: Option<&'static dyn ZenohAllocationObserver>,
}
impl Default for ZenohPreparedConfig {
    fn default() -> Self {
        Self {
            shm_segment_size: 64 * 1024 * 1024,
            route_capacity: 128,
            registration_capacity: 100,
            allocation_observer: None,
        }
    }
}

struct Endpoint {
    definition: ZenohRouteReservation,
    publishers: Box<[Publisher<'static>]>,
}
struct Inner {
    mechanics: ZenohWireMechanics,
    provider: Arc<ZenohShmProvider>,
    endpoints: Box<[Endpoint]>,
    routes: Box<[AtomicBool]>,
    registrations: Box<[AtomicBool]>,
    observer: Option<&'static dyn ZenohAllocationObserver>,
    stats: ZenohPreparedStats,
    segment_size: usize,
}

/// Physical native observations, separate from SDK admission and inbox drops.
#[derive(Default)]
pub struct ZenohPreparedStats {
    /// Successfully published SHM frames.
    pub published: AtomicU64,
    /// Failed native publications after an initialized loan was supplied.
    pub publish_failed: AtomicU64,
    /// Actual SHM-backed received samples, including malformed placement.
    pub received_shm: AtomicU64,
    /// Samples rejected because the native payload was not SHM-backed.
    pub rejected_non_shm: AtomicU64,
    /// Samples rejected for invalid physical placement.
    pub rejected_layout: AtomicU64,
}

/// Native Zenoh core using finite routes, declared publishers and in-SHM headers.
///
/// Each registration creates one actual subscriber whose callback synchronously
/// pushes a retained native sample into the SDK's generic inbox. There is no
/// binding queue, per-message task, attachment or payload coalescing/copying.
#[derive(Clone)]
pub struct ZenohPreparedCore {
    inner: Arc<Inner>,
}

fn setup_error(message: &'static str) -> UStatus {
    UStatus::fail_with_code(UCode::FailedPrecondition, message)
}
fn table(capacity: usize) -> Result<Box<[AtomicBool]>, UStatus> {
    if capacity == 0 {
        return Err(setup_error("prepared table capacity must be positive"));
    }
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(capacity)
        .map_err(|_| setup_error("prepared table allocation failed"))?;
    slots.resize_with(capacity, || AtomicBool::new(false));
    Ok(slots.into_boxed_slice())
}
fn reserve(slots: &[AtomicBool]) -> Result<usize, SetupError> {
    slots
        .iter()
        .position(|slot| {
            slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
        .ok_or(SetupError::TableFull)
}

impl ZenohPreparedCore {
    /// Opens the native session, initializes SHM and declares the complete TX catalog.
    ///
    /// Seven publishers per TX address preserve every SDK priority without declaring
    /// a publisher per frame. They use native reliable/block congestion semantics.
    /// Receiver-only entries require no publisher. Attach and optional matching
    /// waits must finish before admission; unknown first RPC clients are not warmed.
    ///
    /// # Errors
    /// Rejects duplicate/inexact addresses, invalid or overflowing geometry, empty
    /// capacities, native session/provider failures or failed publisher declarations.
    pub async fn new(
        config: crate::zenoh_config::Config,
        uri: impl Into<String>,
        catalog: &[ZenohRouteReservation],
        prepared: ZenohPreparedConfig,
    ) -> Result<Self, UStatus> {
        if catalog.is_empty() || prepared.shm_segment_size == 0 {
            return Err(setup_error(
                "prepared catalog and SHM capacity must be non-empty",
            ));
        }
        let mechanics = ZenohWireMechanics::new(config, uri).await?;
        Self::reserve_native(mechanics, catalog, prepared).await
    }

    /// Reserves prepared routes and SHM on an already opened native session.
    ///
    /// The core retains the session; clones may share it with other configured
    /// endpoints. Each core owns its own SHM provider and finite tables. Closing
    /// the shared session makes subsequent native publications fail with Delivery;
    /// it does not revoke retained samples. Finish all reservation before traffic.
    ///
    /// # Errors
    /// Rejects invalid URI/catalog/capacities, native provider failure or publisher
    /// declaration failure, including a session already closed by its owner.
    pub async fn from_session(
        session: zenoh::Session,
        uri: impl Into<String>,
        catalog: &[ZenohRouteReservation],
        prepared: ZenohPreparedConfig,
    ) -> Result<Self, UStatus> {
        Self::reserve_native(
            ZenohWireMechanics::from_session(session, uri)?,
            catalog,
            prepared,
        )
        .await
    }

    async fn reserve_native(
        mut mechanics: ZenohWireMechanics,
        catalog: &[ZenohRouteReservation],
        prepared: ZenohPreparedConfig,
    ) -> Result<Self, UStatus> {
        if catalog.is_empty() || prepared.shm_segment_size == 0 {
            return Err(setup_error(
                "prepared catalog and SHM capacity must be non-empty",
            ));
        }
        let routes = table(prepared.route_capacity)?;
        let registrations = table(prepared.registration_capacity)?;
        mechanics.set_shm_segment_size(prepared.shm_segment_size)?;
        let segment_size = mechanics.prewarm_shm()?;
        let provider = mechanics.shm_provider()?;
        let mut endpoints: Vec<Endpoint> = Vec::new();
        endpoints
            .try_reserve_exact(catalog.len())
            .map_err(|_| setup_error("catalog allocation failed"))?;
        for definition in catalog {
            definition
                .source
                .verify_no_wildcards()
                .map_err(|_| setup_error("catalog source must be exact"))?;
            if let Some(sink) = &definition.sink {
                sink.verify_no_wildcards()
                    .map_err(|_| setup_error("catalog sink must be exact"))?;
            }
            if endpoints.iter().any(|entry| {
                entry.definition.source == definition.source
                    && entry.definition.sink == definition.sink
            }) {
                return Err(setup_error("duplicate prepared logical route"));
            }
            let limits = definition.limits;
            let largest = PLACEMENT_LEN
                .checked_add(limits.max_metadata_len)
                .and_then(|len| len.checked_add(limits.payload_alignment.checked_sub(1)?))
                .and_then(|len| len.checked_add(limits.max_payload_len));
            if limits.max_metadata_len == 0
                || !limits.payload_alignment.is_power_of_two()
                || largest.is_none_or(|len| len > segment_size || len > u32::MAX as usize)
            {
                return Err(setup_error("prepared frame exceeds SHM geometry"));
            }
            let mut publishers = Vec::new();
            if definition.transmit {
                let key =
                    mechanics.to_zenoh_key_string(&definition.source, definition.sink.as_ref());
                for priority in [
                    UPriority::CS0,
                    UPriority::CS1,
                    UPriority::CS2,
                    UPriority::CS3,
                    UPriority::CS4,
                    UPriority::CS5,
                    UPriority::CS6,
                ] {
                    publishers.push(
                        mechanics
                            .session()
                            .declare_publisher(key.clone())
                            .priority(crate::mechanics::map_zenoh_priority(priority))
                            .congestion_control(CongestionControl::Block)
                            .await
                            .map_err(|_| setup_error("native publisher declaration failed"))?,
                    );
                }
            }
            endpoints.push(Endpoint {
                definition: definition.clone(),
                publishers: publishers.into_boxed_slice(),
            });
        }
        Ok(Self {
            inner: Arc::new(Inner {
                mechanics,
                provider,
                endpoints: endpoints.into_boxed_slice(),
                routes,
                registrations,
                observer: prepared.allocation_observer,
                stats: ZenohPreparedStats::default(),
                segment_size,
            }),
        })
    }

    /// Borrows native counters without conflating native rejection and SDK drops.
    #[must_use]
    pub fn stats(&self) -> &ZenohPreparedStats {
        &self.inner.stats
    }

    /// Returns actual eagerly initialized provider capacity, not a page census.
    #[must_use]
    pub fn prewarmed_shm_segment_size(&self) -> usize {
        self.inner.segment_size
    }

    /// Waits for at least one native matching subscriber for an exact TX address.
    ///
    /// This is a setup readiness observation, not a receiver count or delivery
    /// guarantee. Cancellation releases only the wait; the reserved publisher stays.
    ///
    /// # Errors
    /// Rejects an unknown/receiver-only route, invalid interval, native matching
    /// failure or a deadline reached without a match.
    pub async fn wait_for_matching(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
        timeout: Duration,
    ) -> Result<(), UStatus> {
        let publisher = self
            .inner
            .endpoints
            .iter()
            .find(|entry| {
                &entry.definition.source == source && entry.definition.sink.as_ref() == sink
            })
            .and_then(|entry| entry.publishers.first())
            .ok_or_else(|| setup_error("native TX address was not reserved"))?;
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
            .ok_or_else(|| setup_error("invalid matching interval"))?;
        tokio::time::timeout_at(deadline, async {
            loop {
                if publisher
                    .matching_status()
                    .await
                    .map_err(|_| setup_error("native matching query failed"))?
                    .matching()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            UStatus::fail_with_code(UCode::DeadlineExceeded, "native matching deadline expired")
        })?
    }
}

/// Instance-bound route backed by already declared native publishers.
pub struct ZenohPreparedRoute {
    inner: Arc<Inner>,
    endpoint: usize,
    slot: usize,
    limits: RouteLimits,
}
impl Drop for ZenohPreparedRoute {
    fn drop(&mut self) {
        self.inner.routes[self.slot].store(false, Ordering::Release);
    }
}

/// Exclusive immobile SHM loan with disjoint metadata and payload extents.
pub struct ZenohPreparedTx<'a> {
    route: &'a ZenohPreparedRoute,
    sample: Option<ZShmMut>,
    metadata: Range<usize>,
    payload: Option<Range<usize>>,
    written: Option<WrittenHeader>,
}
fn uninitialized(bytes: &mut [u8]) -> &mut [MaybeUninit<u8>] {
    // SAFETY: MaybeUninit<u8> has byte layout; the returned slice preserves the
    // exclusive borrow and length of the immobile native SHM buffer.
    unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast(), bytes.len()) }
}
// SAFETY: one exclusive ZShmMut retains an immobile native chunk. Checked placement
// produces disjoint byte extents, and neither accessor moves or resizes storage.
unsafe impl PreparedTxStorage for ZenohPreparedTx<'_> {
    fn metadata_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        let _scope = Scope::enter(self.route.inner.observer);
        uninitialized(
            &mut self.sample.as_mut().expect("live native loan").as_mut()[self.metadata.clone()],
        )
    }
    fn payload_uninit_mut(&mut self) -> Option<&mut [MaybeUninit<u8>]> {
        let _scope = Scope::enter(self.route.inner.observer);
        let range = self.payload.clone()?;
        Some(uninitialized(
            &mut self.sample.as_mut().expect("live native loan").as_mut()[range],
        ))
    }
    fn record_header(&mut self, header: WrittenHeader) -> Result<(), SendError> {
        let _scope = Scope::enter(self.route.inner.observer);
        if header.len != self.metadata.len()
            || header.token.is_some_and(|(offset, len)| {
                offset.checked_add(len).is_none_or(|end| end > header.len)
            })
        {
            return Err(SendError::PayloadStorage);
        }
        self.written = Some(header);
        Ok(())
    }
}
impl Drop for ZenohPreparedTx<'_> {
    fn drop(&mut self) {
        let scope = Scope::enter(self.route.inner.observer);
        if let Some(mut sample) = self.sample.take() {
            sample.as_mut()[self.metadata.clone()].fill(0);
            scope.middleware(|| drop(sample));
        }
    }
}

/// Immutable received native sample proving SHM backing and retaining its lease.
pub struct ZenohPreparedRx {
    sample: Option<Sample>,
    metadata: Range<usize>,
    payload: Option<Range<usize>>,
    observer: Option<&'static dyn ZenohAllocationObserver>,
}
impl ZenohPreparedRx {
    /// Borrows the retained actual Zenoh sample for this frame's lifetime.
    ///
    /// # Panics
    /// Panics if the internal live-frame storage invariant is violated. Safe
    /// callers cannot take the native sample out of a live frame.
    #[must_use]
    pub fn sample(&self) -> &Sample {
        let _scope = Scope::enter(self.observer);
        self.sample.as_ref().expect("retained native sample")
    }
}
impl Frame for ZenohPreparedRx {
    fn encoded_metadata(&self) -> &[u8] {
        let _scope = Scope::enter(self.observer);
        &self
            .sample()
            .payload()
            .as_shm()
            .expect("validated native backing")
            .as_ref()[self.metadata.clone()]
    }
    fn payload(&self) -> Option<LoanedPayload<'_>> {
        let _scope = Scope::enter(self.observer);
        Some(LoanedPayload::new(
            &self
                .sample()
                .payload()
                .as_shm()
                .expect("validated native backing")
                .as_ref()[self.payload.clone()?],
            PayloadLoanProvenance::OpaqueTransportLoan,
        ))
    }
}

impl Drop for ZenohPreparedRx {
    fn drop(&mut self) {
        let scope = Scope::enter(self.observer);
        scope.middleware(|| drop(self.sample.take()));
    }
}

/// Exact attached native subscriber and its closed-admission state.
pub struct ZenohPreparedSubscription {
    inner: Arc<Inner>,
    subscriber: Option<Subscriber<()>>,
    sink: Arc<dyn FrameSink<ZenohPreparedRx>>,
    slot: Option<usize>,
}
impl Drop for ZenohPreparedSubscription {
    fn drop(&mut self) {
        self.sink.close();
        let scope = Scope::enter(self.inner.observer);
        scope.middleware(|| drop(self.subscriber.take()));
        if let Some(slot) = self.slot.take() {
            self.inner.registrations[slot].store(false, Ordering::Release);
        }
    }
}

struct ReceivedLayout {
    metadata: Range<usize>,
    payload: Option<Range<usize>>,
}

fn receive_layout(sample: &Sample) -> Option<ReceivedLayout> {
    if sample.attachment().is_some() {
        return None;
    }
    let bytes = sample.payload().as_shm()?.as_ref();
    if bytes.get(..4)? != b"UZSH"
        || *bytes.get(4)? != 1
        || *bytes.get(5)? > 1
        || bytes.get(6..8)? != [0; 2]
        || bytes.get(20..24)? != [0; 4]
    {
        return None;
    }
    let present = bytes[5] == 1;
    let metadata_len = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
    let payload_offset = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?) as usize;
    let payload_len = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?) as usize;
    let metadata_end = PLACEMENT_LEN.checked_add(metadata_len)?;
    let payload_end = payload_offset.checked_add(payload_len)?;
    if metadata_len == 0
        || payload_offset < metadata_end
        || payload_end != bytes.len()
        || (!present && payload_len != 0)
    {
        return None;
    }
    Some(ReceivedLayout {
        metadata: PLACEMENT_LEN..metadata_end,
        payload: present.then_some(payload_offset..payload_end),
    })
}

pub(crate) fn header_view(metadata: &[u8]) -> Option<up_rust::RxHeader<'_>> {
    fn identity_end(bytes: &[u8], start: usize) -> Option<usize> {
        let end = match *bytes.get(start)? {
            0 => start.checked_add(3)?,
            1 => {
                let word_end = start.checked_add(5)?;
                let len =
                    u32::from_le_bytes(bytes.get(start + 1..word_end)?.try_into().ok()?) as usize;
                word_end.checked_add(len)?
            }
            _ => return None,
        };
        bytes.get(..end)?;
        Some(end)
    }
    if metadata.get(..4)? != b"UPWM" {
        return None;
    }
    let mut end = identity_end(metadata, 4)?.checked_add(2)?;
    end = identity_end(metadata, end)?;
    end = identity_end(metadata, end)?.checked_add(2)?;
    up_rust::parse_header(
        metadata.get(..end)?,
        metadata,
        &up_rust::HeaderLimits {
            max_token_len: u16::MAX as usize,
        },
    )
    .ok()
}

fn priority_index(metadata: &[u8]) -> Option<usize> {
    let header = header_view(metadata)?;
    Some(match header.priority.unwrap_or(UPriority::CS1) {
        UPriority::CS0 => 0,
        UPriority::CS1 => 1,
        UPriority::CS2 => 2,
        UPriority::CS3 => 3,
        UPriority::CS4 => 4,
        UPriority::CS5 => 5,
        UPriority::CS6 => 6,
    })
}

impl PreparedTransportCore for ZenohPreparedCore {
    type Route = ZenohPreparedRoute;
    type Tx<'a> = ZenohPreparedTx<'a>;
    type Rx = ZenohPreparedRx;
    type Subscription = ZenohPreparedSubscription;

    fn prepare_route(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
        limits: RouteLimits,
    ) -> Result<Self::Route, SetupError> {
        let _scope = Scope::enter(self.inner.observer);
        let endpoint = self
            .inner
            .endpoints
            .iter()
            .position(|entry| {
                &entry.definition.source == source
                    && entry.definition.sink.as_ref() == sink
                    && !entry.publishers.is_empty()
            })
            .ok_or(SetupError::InvalidRoute(
                "native TX address was not reserved",
            ))?;
        let reserved = self.inner.endpoints[endpoint].definition.limits;
        if limits.max_metadata_len == 0
            || limits.max_metadata_len > reserved.max_metadata_len
            || limits.max_payload_len > reserved.max_payload_len
            || !limits.payload_alignment.is_power_of_two()
            || limits.payload_alignment > reserved.payload_alignment
        {
            return Err(SetupError::InvalidConfig(
                "route exceeds physical reservation",
            ));
        }
        let slot = reserve(&self.inner.routes)?;
        Ok(ZenohPreparedRoute {
            inner: Arc::clone(&self.inner),
            endpoint,
            slot,
            limits,
        })
    }

    fn loan<'a>(
        &'a self,
        route: &'a Self::Route,
        layout: TxFrameLayout,
    ) -> Result<Self::Tx<'a>, LoanError> {
        let scope = Scope::enter(self.inner.observer);
        if !Arc::ptr_eq(&self.inner, &route.inner) {
            return Err(LoanError::InvalidRoute);
        }
        if layout.metadata_len == 0
            || layout.metadata_len > route.limits.max_metadata_len
            || layout.payload_len.unwrap_or(0) > route.limits.max_payload_len
            || !layout.payload_alignment.is_power_of_two()
            || layout.payload_alignment > route.limits.payload_alignment
        {
            return Err(LoanError::SlotTooSmall);
        }
        let metadata_end = PLACEMENT_LEN
            .checked_add(layout.metadata_len)
            .ok_or(LoanError::SlotTooSmall)?;
        let payload_start = metadata_end
            .checked_add(layout.payload_alignment - 1)
            .map(|len| len & !(layout.payload_alignment - 1))
            .ok_or(LoanError::SlotTooSmall)?;
        let payload_end = payload_start
            .checked_add(layout.payload_len.unwrap_or(0))
            .ok_or(LoanError::SlotTooSmall)?;
        let aligned_len = payload_end
            .checked_add(layout.payload_alignment - 1)
            .map(|len| len & !(layout.payload_alignment - 1))
            .ok_or(LoanError::SlotTooSmall)?;
        let metadata_word =
            u32::try_from(layout.metadata_len).map_err(|_| LoanError::SlotTooSmall)?;
        let offset_word = u32::try_from(payload_start).map_err(|_| LoanError::SlotTooSmall)?;
        let payload_word =
            u32::try_from(layout.payload_len.unwrap_or(0)).map_err(|_| LoanError::SlotTooSmall)?;
        let native_layout = scope
            .middleware(|| self.inner.provider.alloc_layout(aligned_len))
            .map_err(|_| LoanError::SlotTooSmall)?;
        let mut sample = scope
            .middleware(|| native_layout.alloc().with_policy::<GarbageCollect>().wait())
            .map_err(|_| LoanError::PoolExhausted)?;
        if aligned_len != payload_end {
            let size = NonZeroUsize::new(payload_end).ok_or(LoanError::SlotTooSmall)?;
            scope
                .middleware(|| sample.try_resize(size))
                .ok_or(LoanError::SlotTooSmall)?;
        }
        if (sample.as_ref().as_ptr() as usize)
            .checked_add(payload_start)
            .is_none_or(|address| !address.is_multiple_of(layout.payload_alignment))
        {
            scope.middleware(|| drop(sample));
            return Err(LoanError::SlotTooSmall);
        }
        let bytes = sample.as_mut();
        bytes[..PLACEMENT_LEN].fill(0);
        bytes[..4].copy_from_slice(b"UZSH");
        bytes[4] = 1;
        bytes[5] = u8::from(layout.payload_len.is_some());
        bytes[8..12].copy_from_slice(&metadata_word.to_le_bytes());
        bytes[12..16].copy_from_slice(&offset_word.to_le_bytes());
        bytes[16..20].copy_from_slice(&payload_word.to_le_bytes());
        bytes[metadata_end..payload_start].fill(0);
        Ok(ZenohPreparedTx {
            route,
            sample: Some(sample),
            metadata: PLACEMENT_LEN..metadata_end,
            payload: layout.payload_len.map(|_| payload_start..payload_end),
            written: None,
        })
    }

    fn publish<'a>(
        &'a self,
        route: &'a Self::Route,
        frame: InitializedTx<Self::Tx<'a>>,
    ) -> Result<(), SendError> {
        let scope = Scope::enter(self.inner.observer);
        let mut tx = frame.into_storage();
        if !Arc::ptr_eq(&self.inner, &route.inner)
            || !std::ptr::eq(tx.route, route)
            || tx.written.is_none()
        {
            return Err(SendError::PayloadStorage);
        }
        let sample = tx.sample.as_ref().ok_or(SendError::PayloadStorage)?;
        let priority = priority_index(&sample.as_ref()[tx.metadata.clone()])
            .ok_or(SendError::PayloadStorage)?;
        let sample = tx.sample.take().ok_or(SendError::PayloadStorage)?;
        scope
            .middleware(|| {
                self.inner.endpoints[route.endpoint].publishers[priority]
                    .put(sample)
                    .wait()
            })
            .map_err(|_| {
                self.inner
                    .stats
                    .publish_failed
                    .fetch_add(1, Ordering::Relaxed);
                SendError::Delivery
            })?;
        self.inner.stats.published.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn attach(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
        inbox: Arc<dyn FrameSink<Self::Rx>>,
    ) -> Result<Self::Subscription, SetupError> {
        let slot = reserve(&self.inner.registrations)?;
        let key = self.inner.mechanics.to_zenoh_key_string(source, sink);
        let inner = Arc::clone(&self.inner);
        let callback_sink = Arc::clone(&inbox);
        let subscriber = self
            .inner
            .mechanics
            .session()
            .declare_subscriber(key)
            .callback_mut(move |sample: Sample| {
                let scope = Scope::enter(inner.observer);
                if sample.payload().as_shm().is_none() {
                    inner.stats.rejected_non_shm.fetch_add(1, Ordering::Relaxed);
                    scope.middleware(|| drop(sample));
                    return;
                }
                inner.stats.received_shm.fetch_add(1, Ordering::Relaxed);
                if let Some(layout) = receive_layout(&sample) {
                    callback_sink.push(ZenohPreparedRx {
                        sample: Some(sample),
                        metadata: layout.metadata,
                        payload: layout.payload,
                        observer: inner.observer,
                    });
                } else {
                    inner.stats.rejected_layout.fetch_add(1, Ordering::Relaxed);
                    scope.middleware(|| drop(sample));
                }
            })
            .await
            .map_err(|_| {
                self.inner.registrations[slot].store(false, Ordering::Release);
                SetupError::Worker
            })?;
        Ok(ZenohPreparedSubscription {
            inner: Arc::clone(&self.inner),
            subscriber: Some(subscriber),
            sink: inbox,
            slot: Some(slot),
        })
    }

    fn detach(&self, subscription: &mut Self::Subscription) -> Result<(), SetupError> {
        let scope = Scope::enter(self.inner.observer);
        if !Arc::ptr_eq(&self.inner, &subscription.inner) || subscription.subscriber.is_none() {
            return Err(SetupError::InvalidConfig("foreign or detached subscriber"));
        }
        subscription.sink.close();
        // Dropping the owning Subscriber closes native registration. Its callback
        // retains the closed sink while an already-entered push finishes; retained
        // SHM samples remain independently owned. No wait on a reentrant callback.
        scope.middleware(|| drop(subscription.subscriber.take()));
        if let Some(slot) = subscription.slot.take() {
            self.inner.registrations[slot].store(false, Ordering::Release);
        }
        Ok(())
    }
}
