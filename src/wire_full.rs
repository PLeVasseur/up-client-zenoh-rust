/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
 *
 * SPDX-License-Identifier: Apache-2.0
 ********************************************************************************/

use std::{
    ops::Deref,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;
use tracing::{trace, warn};
use up_rust::UWire;
use up_rust::{EncodedOwnedFrame, PreparedOwnedFrame, UEncodedOwnedListener, UOwnedTransportCore};
use up_rust::{UCode, UStatus, UUri};
use up_rust::{UNativePrefixWireTransport, UWithNativePrefixWire};
use zenoh::{bytes::ZBytes, sample::Sample};

use crate::dispatch::Dispatch;
use crate::mechanics::ZenohWireMechanics;

/// Real Zenoh owned-frame selected-wire core for benchmark/support paths.
///
/// This feature-gated core carries already encoded selected-wire metadata as
/// Zenoh attachment bytes and transports owned payload bytes through real Zenoh
/// publish/subscribe mechanics. Product [`crate::UPTransportZenoh`] remains the
/// ordinary `UTransport` implementation and is not wrapped by this core.
/// Repeated registration of the same listener and filters shares one native
/// subscriber. Pending setup and retirement never hold the registry lock across
/// a wait. Pull receive uses one nonblocking result slot per operation.
#[derive(Clone)]
pub struct ZenohOwnedCore {
    // Registry destruction must precede the last session owner.
    listeners: Arc<OwnedListeners>,
    mechanics: ZenohWireMechanics,
}

#[derive(Default)]
struct OwnedListeners(Mutex<Vec<Arc<OwnedListenerRegistration>>>);

impl Deref for OwnedListeners {
    type Target = Mutex<Vec<Arc<OwnedListenerRegistration>>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for OwnedListeners {
    fn drop(&mut self) {
        for entry in self.0.get_mut().unwrap().iter() {
            entry.dispatch.close();
        }
    }
}

impl ZenohOwnedCore {
    pub(crate) fn from_mechanics(mechanics: ZenohWireMechanics) -> Self {
        Self {
            mechanics,
            listeners: Arc::new(OwnedListeners::default()),
        }
    }
    /// Creates a real Zenoh owned-frame core.
    ///
    /// # Errors
    ///
    /// Returns a status when the Zenoh session or local URI validation fails.
    pub async fn new(
        config: crate::zenoh_config::Config,
        uri: impl Into<String>,
    ) -> Result<Self, UStatus> {
        Ok(Self::from_mechanics(
            ZenohWireMechanics::new(config, uri).await?,
        ))
    }

    /// Wraps this core in the generic selected-wire adapter.
    #[must_use]
    pub fn with_selected_wire<W>(self, wire: W) -> UNativePrefixWireTransport<Self, W>
    where
        W: UWire,
    {
        self.into_native_prefix_wire_transport(wire)
    }

    async fn declare_listener(
        &self,
        entry: &OwnedListenerRegistration,
    ) -> Result<zenoh::pubsub::Subscriber<()>, UStatus> {
        let key = self
            .mechanics
            .to_zenoh_key_string(&entry.source_filter, entry.sink_filter.as_ref());
        entry.dispatch.bind(self.mechanics.session(), &key).await?;
        let dispatch = Arc::clone(&entry.dispatch);
        self.mechanics
            .session()
            .declare_subscriber(key)
            .callback(move |sample: Sample| {
                let Some(frame) = sample_to_encoded_owned(&sample) else {
                    return;
                };
                drop(sample);
                dispatch.push(frame);
            })
            .await
            .map_err(|error| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to register Zenoh owned listener: {error}"),
                )
            })
    }
}

struct OwnedListenerRegistration {
    source_filter: UUri,
    sink_filter: Option<UUri>,
    owned_listener: Arc<dyn UEncodedOwnedListener>,
    dispatch: Arc<Dispatch<EncodedOwnedFrame>>,
    subscriber: Mutex<Option<zenoh::pubsub::Subscriber<()>>>,
    outcome: Mutex<Option<Result<(), UStatus>>>,
    ready: Notify,
}

impl Drop for OwnedListenerRegistration {
    fn drop(&mut self) {
        self.dispatch.close();
    }
}

impl OwnedListenerRegistration {
    fn matches(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
        listener: &Arc<dyn UEncodedOwnedListener>,
    ) -> bool {
        self.source_filter == *source
            && self.sink_filter.as_ref() == sink
            && Arc::ptr_eq(&self.owned_listener, listener)
    }

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

    fn retire(&self, error: UStatus) -> Option<zenoh::pubsub::Subscriber<()>> {
        self.dispatch.close();
        let mut outcome = self.outcome.lock().unwrap();
        *outcome = Some(Err(error));
        let subscriber = self.subscriber.lock().unwrap().take();
        drop(outcome);
        self.ready.notify_waiters();
        subscriber
    }
}

struct CreatingOwned<'a> {
    core: &'a ZenohOwnedCore,
    entry: Arc<OwnedListenerRegistration>,
    armed: bool,
}

impl CreatingOwned<'_> {
    fn fail(&mut self, error: UStatus) {
        let mut entries = self.core.listeners.lock().unwrap();
        if let Some(index) = entries
            .iter()
            .position(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            entries.remove(index);
        }
        drop(entries);
        drop(self.entry.retire(error));
        self.armed = false;
    }
}

impl Drop for CreatingOwned<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.fail(UStatus::fail_with_code(
                UCode::Cancelled,
                "owned listener creation cancelled",
            ));
        }
    }
}

struct OwnedHandler(ComparableOwnedListener);
impl up_rust::RegistrationHandler<EncodedOwnedFrame> for OwnedHandler {
    async fn on_receive(&mut self, frame: EncodedOwnedFrame) {
        self.0.on_receive_encoded_owned(frame).await;
    }
}

#[derive(Clone)]
struct ComparableOwnedListener {
    listener: Arc<dyn UEncodedOwnedListener>,
}

impl ComparableOwnedListener {
    fn new(listener: Arc<dyn UEncodedOwnedListener>) -> Self {
        Self { listener }
    }
}

impl Deref for ComparableOwnedListener {
    type Target = dyn UEncodedOwnedListener;

    fn deref(&self) -> &Self::Target {
        &*self.listener
    }
}

#[async_trait]
impl UOwnedTransportCore for ZenohOwnedCore {
    async fn send_prepared_owned(&self, frame: PreparedOwnedFrame) -> Result<(), UStatus> {
        let metadata = frame.metadata();
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(metadata.source(), metadata.sink());
        let priority = crate::mechanics::map_zenoh_priority(
            metadata.priority().unwrap_or(up_rust::UPriority::CS1),
        );
        let attachment = ZBytes::from(frame.encoded_metadata().to_vec());
        let payload = frame
            .payload()
            .map_or_else(ZBytes::new, |payload| ZBytes::from(payload.to_vec()));

        let reserved = crate::dispatch::reserve_publication(self.mechanics.session(), &zenoh_key)?;
        crate::dispatch::publish_reserved(reserved, || {
            use zenoh::Wait;
            self.mechanics
                .session()
                .put(&zenoh_key, payload)
                .priority(priority)
                .attachment(attachment)
                .wait()
        })
        .map_err(|err| {
            UStatus::fail_with_code(
                UCode::Internal,
                format!("failed to put Zenoh owned frame: {err}"),
            )
        })?;
        trace!("putting owned frame with key: {zenoh_key}");
        Ok(())
    }

    async fn receive_encoded_owned(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
    ) -> Result<EncodedOwnedFrame, UStatus> {
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
                let Some(frame) = sample_to_encoded_owned(&sample) else {
                    return;
                };
                drop(sample);
                if let Some(sender) = sender.take() {
                    let _ = sender.send(frame);
                }
            })
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to declare Zenoh owned subscriber: {err}"),
                )
            })?;

        receiver.await.map_err(|error| {
            UStatus::fail_with_code(
                UCode::Internal,
                format!("failed to receive Zenoh owned sample: {error}"),
            )
        })
    }

    async fn register_encoded_owned_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedOwnedListener>,
    ) -> Result<(), UStatus> {
        let (entry, creating) = {
            let mut entries = self.listeners.lock().unwrap();
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.matches(source_filter, sink_filter, &listener))
            {
                (Arc::clone(entry), false)
            } else {
                let entry = Arc::new(OwnedListenerRegistration {
                    source_filter: source_filter.clone(),
                    sink_filter: sink_filter.cloned(),
                    dispatch: Dispatch::new(OwnedHandler(ComparableOwnedListener::new(
                        Arc::clone(&listener),
                    )))?,
                    owned_listener: listener,
                    subscriber: Mutex::new(None),
                    outcome: Mutex::new(None),
                    ready: Notify::new(),
                });
                entries.push(Arc::clone(&entry));
                (entry, true)
            }
        };
        if !creating {
            return entry.wait_ready().await;
        }
        let mut creation = CreatingOwned {
            core: self,
            entry: Arc::clone(&entry),
            armed: true,
        };
        match self.declare_listener(&entry).await {
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
                        "owned listener removed during creation",
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

    async fn unregister_encoded_owned_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedOwnedListener>,
    ) -> Result<(), UStatus> {
        let registration = {
            let mut listeners = self.listeners.lock().unwrap();
            let Some(index) = listeners.iter().position(|registration| {
                registration.matches(source_filter, sink_filter, &listener)
            }) else {
                return Err(UStatus::fail_with_code(
                    UCode::NotFound,
                    "owned listener not registered",
                ));
            };
            listeners.remove(index)
        };
        if let Some(subscriber) = registration.retire(UStatus::fail_with_code(
            UCode::Cancelled,
            "owned listener removed",
        )) {
            subscriber.undeclare().await.map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to undeclare Zenoh owned listener: {err}"),
                )
            })?;
        }
        registration.dispatch.stop().await
    }
}

fn sample_to_encoded_owned(sample: &Sample) -> Option<EncodedOwnedFrame> {
    let Some(attachment) = sample.attachment() else {
        warn!(
            "Ignoring Zenoh owned sample without selected-wire attachment [key expr: {}]",
            sample.key_expr()
        );
        return None;
    };
    let payload = if payload_present(attachment.to_bytes().as_ref())? {
        Some(Bytes::copy_from_slice(sample.payload().to_bytes().as_ref()))
    } else {
        if !sample.payload().is_empty() {
            return None;
        }
        None
    };
    Some(EncodedOwnedFrame::new(
        attachment.to_bytes().to_vec(),
        payload,
    ))
}

// Locate the canonical field block to preserve absent versus present-empty
// carriage. This is not wire/family compatibility or typed native admission;
// the SDK selected-wire adapter independently validates the complete prefix.
fn payload_present(bytes: &[u8]) -> Option<bool> {
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
    if bytes.get(..4)? != b"UPWM" {
        return None;
    }
    let mut end = identity_end(bytes, 4)?.checked_add(2)?;
    end = identity_end(bytes, end)?;
    end = identity_end(bytes, end)?.checked_add(2)?;
    let word_end = end.checked_add(4)?;
    let len = u32::from_le_bytes(bytes.get(end..word_end)?.try_into().ok()?) as usize;
    let field_end = word_end.checked_add(len)?;
    if field_end != bytes.len() {
        return None;
    }
    Some(
        up_rust::frame::codec::decode_frame_metadata_fields(bytes.get(word_end..field_end)?)
            .ok()?
            .payload_encoding()
            .is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{poll_fn, Future},
        task::Poll,
    };
    use up_rust::{
        PayloadEncoding, ProtobufWire, UFrameFieldsMetadataCodec, UFrameMetadata, UOwnedFrame,
    };

    async fn core() -> ZenohOwnedCore {
        let mut config = zenoh::Config::default();
        config.insert_json5("listen/endpoints", "[]").unwrap();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        ZenohOwnedCore::new(config, "//owned-lifecycle/4210/1/1/0")
            .await
            .unwrap()
    }

    fn topic(resource: u16) -> UUri {
        UUri::try_from_parts("owned-lifecycle", 0x0001_4210, 1, resource).unwrap()
    }

    struct Noop;
    #[async_trait]
    impl UEncodedOwnedListener for Noop {
        async fn on_receive_encoded_owned(&self, _: EncodedOwnedFrame) {}
    }

    #[test_case::test_case(false; "cancel owned pending creator")]
    #[test_case::test_case(true; "remove owned pending creator")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn owned_pending_registration_preserves_replacement_generation(remove: bool) {
        let core = core().await;
        let existing = topic(0x9000);
        let pending = topic(0x9001);
        let listener: Arc<dyn UEncodedOwnedListener> = Arc::new(Noop);
        core.register_encoded_owned_listener(&existing, None, Arc::clone(&listener))
            .await
            .unwrap();
        let activity = Arc::clone(core.listeners.lock().unwrap()[0].dispatch.activity());
        let key = core.mechanics.to_zenoh_key_string(&existing, None);
        activity
            .dispatch(|| async {
                let publication =
                    crate::dispatch::reserve_publication(core.mechanics.session(), &key).unwrap();
                let mut creating = Box::pin(core.register_encoded_owned_listener(
                    &pending,
                    None,
                    Arc::clone(&listener),
                ));
                assert!(poll_fn(|cx| Poll::Ready(creating.as_mut().poll(cx)))
                    .await
                    .is_pending());
                let mut duplicate = Box::pin(core.register_encoded_owned_listener(
                    &pending,
                    None,
                    Arc::clone(&listener),
                ));
                assert!(poll_fn(|cx| Poll::Ready(duplicate.as_mut().poll(cx)))
                    .await
                    .is_pending());
                if remove {
                    core.unregister_encoded_owned_listener(&pending, None, Arc::clone(&listener))
                        .await
                        .unwrap();
                } else {
                    drop(creating);
                    assert_eq!(duplicate.await.unwrap_err().code(), UCode::Cancelled);
                    drop(publication);
                    core.register_encoded_owned_listener(&pending, None, Arc::clone(&listener))
                        .await
                        .unwrap();
                    return;
                }
                assert_eq!(duplicate.await.unwrap_err().code(), UCode::Cancelled);
                drop(publication);
                core.register_encoded_owned_listener(&pending, None, Arc::clone(&listener))
                    .await
                    .unwrap();
                assert_eq!(creating.await.unwrap_err().code(), UCode::Cancelled);
            })
            .await;
        assert_eq!(core.listeners.lock().unwrap().len(), 2);
        core.unregister_encoded_owned_listener(&pending, None, Arc::clone(&listener))
            .await
            .unwrap();
        core.unregister_encoded_owned_listener(&existing, None, listener)
            .await
            .unwrap();
        assert!(core.listeners.lock().unwrap().is_empty());
    }

    fn frame(sequence: u64) -> PreparedOwnedFrame {
        let metadata = UFrameMetadata::publish(topic(0x9000))
            .with_payload_encoding(PayloadEncoding::RAW)
            .build()
            .unwrap();
        let frame =
            UOwnedFrame::with_payload(metadata, Bytes::copy_from_slice(&sequence.to_le_bytes()))
                .unwrap();
        PreparedOwnedFrame::from_validated::<ProtobufWire, _>(frame, &UFrameFieldsMetadataCodec)
            .unwrap()
    }

    async fn send(core: &ZenohOwnedCore, sequence: u64) {
        core.send_prepared_owned(frame(sequence)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn owned_pull_uses_one_result_without_blocking_a_native_burst() {
        let core = core().await;
        let source = topic(0x9000);
        let mut receive = Box::pin(core.receive_encoded_owned(&source, None));
        assert!(poll_fn(|cx| Poll::Ready(receive.as_mut().poll(cx)))
            .await
            .is_pending());
        // The receiver is deliberately not polled while native callbacks arrive.
        // A general native FIFO would eventually block this synchronous producer.
        for sequence in 0..512 {
            send(&core, sequence).await;
        }
        let frame = receive.await.unwrap();
        assert_eq!(frame.payload().unwrap().as_ref(), 0u64.to_le_bytes());
        let mut cancelled = Box::pin(core.receive_encoded_owned(&source, None));
        assert!(poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx)))
            .await
            .is_pending());
        drop(cancelled);
        send(&core, 512).await;
    }

    struct Reentrant {
        core: std::sync::Weak<ZenohOwnedCore>,
        entered: Notify,
        resume: Notify,
        finished: Notify,
        values: Mutex<Vec<u64>>,
        result: Mutex<Option<Result<(), UCode>>>,
    }

    #[async_trait]
    impl UEncodedOwnedListener for Reentrant {
        async fn on_receive_encoded_owned(&self, received: EncodedOwnedFrame) {
            let value =
                u64::from_le_bytes(received.payload().unwrap().as_ref().try_into().unwrap());
            self.values.lock().unwrap().push(value);
            if value == 0 {
                self.entered.notify_one();
                self.resume.notified().await;
                let result = self
                    .core
                    .upgrade()
                    .unwrap()
                    .send_prepared_owned(frame(2))
                    .await;
                *self.result.lock().unwrap() = Some(result.map_err(|error| error.code()));
                self.finished.notify_one();
            }
        }
    }

    #[test_case::test_case(false; "owned callback available credit")]
    #[test_case::test_case(true; "owned callback saturated credit")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn owned_callback_publication_reserves_or_refuses_before_native_put(full: bool) {
        let core = Arc::new(core().await);
        let listener = Arc::new(Reentrant {
            core: Arc::downgrade(&core),
            entered: Notify::new(),
            resume: Notify::new(),
            finished: Notify::new(),
            values: Mutex::new(Vec::new()),
            result: Mutex::new(None),
        });
        let registered: Arc<dyn UEncodedOwnedListener> = listener.clone();
        core.register_encoded_owned_listener(&topic(0x9000), None, Arc::clone(&registered))
            .await
            .unwrap();
        send(&core, 0).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            listener.entered.notified(),
        )
        .await
        .unwrap();
        if full {
            send(&core, 1).await;
        }
        listener.resume.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            listener.finished.notified(),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while listener.values.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *listener.values.lock().unwrap(),
            if full { vec![0, 1] } else { vec![0, 2] }
        );
        assert_eq!(
            *listener.result.lock().unwrap(),
            Some(if full {
                Err(UCode::ResourceExhausted)
            } else {
                Ok(())
            })
        );
        core.unregister_encoded_owned_listener(&topic(0x9000), None, registered)
            .await
            .unwrap();
    }

    struct SelfRemoving {
        own: std::sync::Weak<Self>,
        core: std::sync::Weak<ZenohOwnedCore>,
        finished: Notify,
        result: Mutex<Option<Result<(), UCode>>>,
    }

    #[async_trait]
    impl UEncodedOwnedListener for SelfRemoving {
        async fn on_receive_encoded_owned(&self, received: EncodedOwnedFrame) {
            let listener: Arc<dyn UEncodedOwnedListener> = self.own.upgrade().unwrap();
            let result = self
                .core
                .upgrade()
                .unwrap()
                .unregister_encoded_owned_listener(&topic(0x9000), None, listener)
                .await;
            assert_eq!(received.payload().unwrap().as_ref(), 0u64.to_le_bytes());
            *self.result.lock().unwrap() = Some(result.map_err(|error| error.code()));
            self.finished.notify_one();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn owned_callback_unregisters_itself_and_retains_its_copied_payload() {
        let core = Arc::new(core().await);
        let listener = Arc::new_cyclic(|own| SelfRemoving {
            own: own.clone(),
            core: Arc::downgrade(&core),
            finished: Notify::new(),
            result: Mutex::new(None),
        });
        core.register_encoded_owned_listener(&topic(0x9000), None, listener.clone())
            .await
            .unwrap();
        send(&core, 0).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            listener.finished.notified(),
        )
        .await
        .unwrap();
        assert_eq!(*listener.result.lock().unwrap(), Some(Ok(())));
        assert!(core.listeners.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn owned_receiver_drop_wakes_a_blocked_shared_session_publisher() {
        let receiver = Arc::new(core().await);
        let publisher = Arc::new(ZenohOwnedCore::from_mechanics(receiver.mechanics.clone()));
        let paused = || {
            Arc::new(Reentrant {
                core: Arc::downgrade(&receiver),
                entered: Notify::new(),
                resume: Notify::new(),
                finished: Notify::new(),
                values: Mutex::new(Vec::new()),
                result: Mutex::new(None),
            })
        };
        let left = paused();
        let right = paused();
        let listeners: [Arc<dyn UEncodedOwnedListener>; 2] = [left.clone(), right.clone()];
        for listener in listeners {
            receiver
                .register_encoded_owned_listener(&topic(0x9000), None, listener)
                .await
                .unwrap();
        }
        send(&publisher, 0).await;
        for listener in [&left, &right] {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                listener.entered.notified(),
            )
            .await
            .unwrap();
        }
        send(&publisher, 1).await;
        let runtime = tokio::runtime::Handle::current();
        let (started, start) = std::sync::mpsc::sync_channel(1);
        let (finished, finish) = std::sync::mpsc::sync_channel(1);
        let producer = std::thread::spawn(move || {
            started.send(()).unwrap();
            runtime.block_on(send(&publisher, 2));
            finished.send(()).unwrap();
        });
        start
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            finish.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(receiver);
        finish
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        producer.join().unwrap();
        assert_eq!(*left.values.lock().unwrap(), [0]);
        assert_eq!(*right.values.lock().unwrap(), [0]);
    }
}
