// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Real prepared SHM delivery with four inbox slots, one handler and one callback.
#![cfg(feature = "zero-copy")]

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
use up_rust::{
    Frame, FrameSink, OpaqueRoute, OpaqueTemplate, OverflowPolicy, PayloadEncoding,
    PreparedTransportCore, ProtobufWire, Registration, RegistrationConfig, RegistrationHandler,
    RegistrationInbox, RouteConfig, RouteLimits, TxHeaderParts, UFrameMetadata, UUri,
};
use up_transport_zenoh::{
    ZenohPreparedConfig, ZenohPreparedCore, ZenohPreparedRx, ZenohRouteReservation,
};

#[derive(Default)]
struct Evidence {
    callbacks: Arc<AtomicUsize>,
    callback_peak: Arc<AtomicUsize>,
    processing: AtomicUsize,
    processing_peak: AtomicUsize,
    queue_peak: AtomicUsize,
    aborted: AtomicUsize,
    entered: Notify,
    resume: Notify,
    exited: Notify,
    reentered: Notify,
    reentry_ok: AtomicBool,
    completed: Mutex<Vec<u64>>,
    callback_threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}

struct Active<'a>(&'a AtomicUsize);
impl<'a> Active<'a> {
    fn enter(current: &'a AtomicUsize, peak: &AtomicUsize) -> Self {
        let active = current.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(active, Ordering::SeqCst);
        Self(current)
    }
}
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct CallbackSink {
    inbox: Arc<RegistrationInbox<ZenohPreparedRx>>,
    evidence: Arc<Evidence>,
}
impl FrameSink<ZenohPreparedRx> for CallbackSink {
    fn push(&self, frame: ZenohPreparedRx) {
        let _active = Active::enter(&self.evidence.callbacks, &self.evidence.callback_peak);
        self.evidence
            .callback_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        // This is the SDK application inbox, not a second binding mailbox.
        // Its counters distinguish closure and application overflow from binding loss.
        let _ = self.inbox.push(frame);
        self.evidence
            .queue_peak
            .fetch_max(self.inbox.len(), Ordering::SeqCst);
    }
    fn close(&self) {
        self.inbox.close();
    }
}

struct Completion<'a> {
    evidence: &'a Evidence,
    complete: bool,
}
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.evidence.aborted.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct Handler {
    evidence: Arc<Evidence>,
    reenter: Option<Arc<OpaqueRoute<ZenohPreparedCore>>>,
    detach: Option<Arc<Detachment>>,
}

struct Detachment {
    core: Arc<ZenohPreparedCore>,
    subscription: Mutex<Option<<ZenohPreparedCore as PreparedTransportCore>::Subscription>>,
}
impl Drop for Handler {
    fn drop(&mut self) {
        // Registration's detach callback only closes admission. Destruction of
        // this handler follows destruction of its borrowed in-flight future.
        self.evidence.exited.notify_one();
    }
}
impl RegistrationHandler<ZenohPreparedRx> for Handler {
    async fn on_receive(&mut self, frame: ZenohPreparedRx) {
        let _active = Active::enter(&self.evidence.processing, &self.evidence.processing_peak);
        let mut completion = Completion {
            evidence: &self.evidence,
            complete: false,
        };
        let value = u64::from_le_bytes(frame.payload().unwrap().bytes().try_into().unwrap());
        if value == 0 {
            self.evidence.entered.notify_one();
            self.evidence.resume.notified().await;
            if let Some(detach) = &self.detach {
                let subscription = detach.subscription.lock().unwrap().take();
                if let Some(mut subscription) = subscription {
                    detach.core.detach(&mut subscription).unwrap();
                }
                self.evidence.reentered.notify_one();
            }
            if let Some(sender) = &self.reenter {
                let metadata = metadata();
                let result =
                    sender.send_bytes(TxHeaderParts::from(&metadata), Some(&99u64.to_le_bytes()));
                self.evidence
                    .reentry_ok
                    .store(result.is_ok(), Ordering::SeqCst);
                self.evidence.reentered.notify_one();
            }
        }
        // Detachment/closure while suspended must not revoke this native sample.
        assert_eq!(frame.payload().unwrap().bytes(), value.to_le_bytes());
        self.evidence.completed.lock().unwrap().push(value);
        completion.complete = true;
    }
}

fn topic() -> UUri {
    UUri::try_from("//prepared-dispatch/4321/1/1/8001").unwrap()
}
fn metadata() -> UFrameMetadata {
    UFrameMetadata::publish(topic())
        .with_payload_encoding(PayloadEncoding::RAW)
        .build()
        .unwrap()
}

async fn setup() -> (Arc<ZenohPreparedCore>, Arc<OpaqueRoute<ZenohPreparedCore>>) {
    let mut config = zenoh::Config::default();
    config.insert_json5("listen/endpoints", "[]").unwrap();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    let limits = RouteLimits {
        max_metadata_len: 3072,
        max_payload_len: 8,
        payload_alignment: 8,
    };
    let core = Arc::new(
        ZenohPreparedCore::from_session(
            zenoh::open(config).await.unwrap(),
            "//prepared-dispatch/4321/1/1/0",
            &[ZenohRouteReservation {
                source: topic(),
                sink: None,
                limits,
                transmit: true,
            }],
            ZenohPreparedConfig {
                shm_segment_size: 1024 * 1024,
                route_capacity: 2,
                registration_capacity: 2,
                allocation_observer: None,
            },
        )
        .await
        .unwrap(),
    );
    let sender = Arc::new(
        OpaqueTemplate::for_wire::<ProtobufWire>()
            .unwrap()
            .route(
                Arc::clone(&core),
                topic(),
                None,
                limits,
                RouteConfig::default(),
            )
            .unwrap(),
    );
    (core, sender)
}

fn worker(
    evidence: &Arc<Evidence>,
    overflow: OverflowPolicy,
    reenter: Option<Arc<OpaqueRoute<ZenohPreparedCore>>>,
) -> Registration<ZenohPreparedRx> {
    Registration::new(
        RegistrationConfig {
            capacity: 4,
            overflow,
        },
        Handler {
            evidence: Arc::clone(evidence),
            reenter,
            detach: None,
        },
        || {},
    )
    .unwrap()
}

fn send(sender: &OpaqueRoute<ZenohPreparedCore>, value: u64) {
    sender
        .send_bytes(TxHeaderParts::from(&metadata()), Some(&value.to_le_bytes()))
        .unwrap();
}

async fn notified(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(2), notify.notified())
        .await
        .unwrap();
}

fn assert_bounds(evidence: &Evidence) {
    assert_eq!(evidence.callback_peak.load(Ordering::SeqCst), 1);
    assert_eq!(evidence.processing_peak.load(Ordering::SeqCst), 1);
    assert_eq!(evidence.queue_peak.load(Ordering::SeqCst), 4);
    assert_eq!(evidence.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(evidence.processing.load(Ordering::SeqCst), 0);
}

#[derive(Clone, Copy)]
enum Finish {
    Drain,
    Detach,
    Cancel,
}

#[test_case::test_case(Finish::Drain; "independent worker drains blocked native producer")]
#[test_case::test_case(Finish::Detach; "detach wakes native producer before drain")]
#[test_case::test_case(Finish::Cancel; "cancellation releases queued and callback native frames")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn prepared_native_backlog_preserves_shared_callback_credit(finish: Finish) {
    let (core, sender) = setup().await;
    let evidence = Arc::new(Evidence::default());
    let worker = worker(
        &evidence,
        OverflowPolicy::Block {
            deadline: Duration::from_secs(1),
        },
        None,
    );
    let inbox = worker.inbox();
    let mut subscription = core
        .attach(
            &topic(),
            None,
            Arc::new(CallbackSink {
                inbox: Arc::clone(&inbox),
                evidence: Arc::clone(&evidence),
            }),
        )
        .await
        .unwrap();
    core.wait_for_matching(&topic(), None, Duration::from_secs(2))
        .await
        .unwrap();
    let first_thread = std::thread::current().id();
    send(&sender, 0);
    notified(&evidence.entered).await;
    for value in 1..=4 {
        send(&sender, value);
    }
    assert_eq!(inbox.len(), 4);
    let publishing = Arc::clone(&sender);
    let (finished, received) = tokio::sync::oneshot::channel();
    let producer = std::thread::spawn(move || {
        send(&publishing, 5);
        finished.send(std::thread::current().id()).unwrap();
    });
    tokio::time::timeout(Duration::from_millis(500), async {
        while inbox.stats().blocked.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(evidence.callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(evidence.processing.load(Ordering::SeqCst), 1);
    assert_eq!(inbox.len(), 4);
    match finish {
        Finish::Drain => evidence.resume.notify_one(),
        Finish::Detach | Finish::Cancel => core.detach(&mut subscription).unwrap(),
    }
    let producer_thread = tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap();
    producer.join().unwrap();
    match finish {
        Finish::Cancel => {
            drop(worker);
            notified(&evidence.exited).await;
        }
        Finish::Drain | Finish::Detach => {
            evidence.resume.notify_one();
            if matches!(finish, Finish::Drain) {
                core.detach(&mut subscription).unwrap();
            }
            worker.stop().await.unwrap();
        }
    }
    assert_bounds(&evidence);
    let threads = evidence.callback_threads.lock().unwrap();
    assert!(threads.iter().take(5).all(|thread| *thread == first_thread));
    assert_eq!(threads.last(), Some(&producer_thread));
    assert_ne!(first_thread, producer_thread);
    assert_eq!(inbox.stats().full.load(Ordering::SeqCst), 0);
    assert_eq!(inbox.stats().deadline_expired.load(Ordering::SeqCst), 0);
    if matches!(finish, Finish::Cancel) {
        assert!(evidence.completed.lock().unwrap().is_empty());
        assert_eq!(evidence.aborted.load(Ordering::SeqCst), 1);
        assert_eq!(inbox.stats().discarded.load(Ordering::SeqCst), 4);
    } else {
        let count = if matches!(finish, Finish::Drain) {
            6
        } else {
            5
        };
        assert_eq!(
            *evidence.completed.lock().unwrap(),
            (0..count).collect::<Vec<_>>()
        );
        assert_eq!(evidence.aborted.load(Ordering::SeqCst), 0);
    }
    assert_eq!(
        inbox.stats().after_close.load(Ordering::SeqCst),
        u64::from(!matches!(finish, Finish::Drain))
    );
}

#[test_case::test_case(false; "explicit application DropNewest")]
#[test_case::test_case(true; "explicit application Block deadline")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn prepared_callback_publication_to_full_application_inbox_is_bounded(block: bool) {
    let (core, sender) = setup().await;
    let evidence = Arc::new(Evidence::default());
    let worker = worker(
        &evidence,
        if block {
            OverflowPolicy::Block {
                deadline: Duration::from_millis(20),
            }
        } else {
            OverflowPolicy::DropNewest
        },
        Some(Arc::clone(&sender)),
    );
    let inbox = worker.inbox();
    let mut subscription = core
        .attach(
            &topic(),
            None,
            Arc::new(CallbackSink {
                inbox: Arc::clone(&inbox),
                evidence: Arc::clone(&evidence),
            }),
        )
        .await
        .unwrap();
    core.wait_for_matching(&topic(), None, Duration::from_secs(2))
        .await
        .unwrap();
    send(&sender, 0);
    notified(&evidence.entered).await;
    for value in 1..=4 {
        send(&sender, value);
    }
    assert_eq!(inbox.len(), 4);
    evidence.resume.notify_one();
    notified(&evidence.reentered).await;
    assert!(evidence.reentry_ok.load(Ordering::SeqCst));
    core.detach(&mut subscription).unwrap();
    worker.stop().await.unwrap();
    assert_bounds(&evidence);
    assert_eq!(*evidence.completed.lock().unwrap(), [0, 1, 2, 3, 4]);
    assert_eq!(inbox.stats().admitted.load(Ordering::SeqCst), 5);
    // Explicit application overflow/deadline, not binding loss or zero-loss acceptance.
    assert_eq!(inbox.stats().full.load(Ordering::SeqCst), u64::from(!block));
    assert_eq!(
        inbox.stats().deadline_expired.load(Ordering::SeqCst),
        u64::from(block)
    );
    assert_eq!(
        inbox.stats().blocked.load(Ordering::SeqCst),
        u64::from(block)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn prepared_handler_detaches_itself_while_a_native_producer_is_blocked() {
    let (core, sender) = setup().await;
    let evidence = Arc::new(Evidence::default());
    let detach = Arc::new(Detachment {
        core: Arc::clone(&core),
        subscription: Mutex::new(None),
    });
    let worker = Registration::new(
        RegistrationConfig {
            capacity: 4,
            overflow: OverflowPolicy::Block {
                deadline: Duration::from_secs(1),
            },
        },
        Handler {
            evidence: Arc::clone(&evidence),
            reenter: None,
            detach: Some(Arc::clone(&detach)),
        },
        || {},
    )
    .unwrap();
    let inbox = worker.inbox();
    let subscription = core
        .attach(
            &topic(),
            None,
            Arc::new(CallbackSink {
                inbox: Arc::clone(&inbox),
                evidence: Arc::clone(&evidence),
            }),
        )
        .await
        .unwrap();
    *detach.subscription.lock().unwrap() = Some(subscription);
    core.wait_for_matching(&topic(), None, Duration::from_secs(2))
        .await
        .unwrap();
    send(&sender, 0);
    notified(&evidence.entered).await;
    for value in 1..=4 {
        send(&sender, value);
    }
    let (finished, received) = tokio::sync::oneshot::channel();
    let producer = std::thread::spawn(move || {
        send(&sender, 5);
        finished.send(()).unwrap();
    });
    tokio::time::timeout(Duration::from_millis(500), async {
        while inbox.stats().blocked.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(evidence.callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(inbox.len(), 4);
    evidence.resume.notify_one();
    notified(&evidence.reentered).await;
    tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap();
    producer.join().unwrap();
    worker.stop().await.unwrap();
    assert_bounds(&evidence);
    assert_eq!(*evidence.completed.lock().unwrap(), [0, 1, 2, 3, 4]);
    assert!(detach.subscription.lock().unwrap().is_none());
    assert_eq!(inbox.stats().after_close.load(Ordering::SeqCst), 1);
    assert_eq!(inbox.stats().deadline_expired.load(Ordering::SeqCst), 0);
    assert_eq!(inbox.stats().full.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn prepared_native_fanout_shares_one_callback_credit_across_two_listeners() {
    let (core, sender) = setup().await;
    let left = Arc::new(Evidence::default());
    let right = Arc::new(Evidence {
        callbacks: Arc::clone(&left.callbacks),
        callback_peak: Arc::clone(&left.callback_peak),
        callback_threads: Arc::clone(&left.callback_threads),
        ..Evidence::default()
    });
    let policy = OverflowPolicy::Block {
        deadline: Duration::from_secs(1),
    };
    let first = worker(&left, policy, None);
    let second = worker(&right, policy, None);
    let first_inbox = first.inbox();
    let second_inbox = second.inbox();
    let mut first_subscription = core
        .attach(
            &topic(),
            None,
            Arc::new(CallbackSink {
                inbox: Arc::clone(&first_inbox),
                evidence: Arc::clone(&left),
            }),
        )
        .await
        .unwrap();
    let mut second_subscription = core
        .attach(
            &topic(),
            None,
            Arc::new(CallbackSink {
                inbox: Arc::clone(&second_inbox),
                evidence: Arc::clone(&right),
            }),
        )
        .await
        .unwrap();
    core.wait_for_matching(&topic(), None, Duration::from_secs(2))
        .await
        .unwrap();
    let original_thread = std::thread::current().id();
    send(&sender, 0);
    notified(&left.entered).await;
    notified(&right.entered).await;
    for value in 1..=4 {
        send(&sender, value);
    }
    assert_eq!(first_inbox.len(), 4);
    assert_eq!(second_inbox.len(), 4);
    let (finished, received) = tokio::sync::oneshot::channel();
    let producer = std::thread::spawn(move || {
        send(&sender, 5);
        finished.send(std::thread::current().id()).unwrap();
    });
    tokio::time::timeout(Duration::from_millis(500), async {
        while first_inbox.stats().blocked.load(Ordering::SeqCst)
            + second_inbox.stats().blocked.load(Ordering::SeqCst)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(left.callbacks.load(Ordering::SeqCst), 1);
    left.resume.notify_one();
    right.resume.notify_one();
    let producer_thread = tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap();
    producer.join().unwrap();
    core.detach(&mut first_subscription).unwrap();
    core.detach(&mut second_subscription).unwrap();
    first.stop().await.unwrap();
    second.stop().await.unwrap();
    for (evidence, inbox) in [(&left, &first_inbox), (&right, &second_inbox)] {
        assert_bounds(evidence);
        assert_eq!(*evidence.completed.lock().unwrap(), [0, 1, 2, 3, 4, 5]);
        assert_eq!(inbox.stats().admitted.load(Ordering::SeqCst), 6);
        assert_eq!(inbox.stats().deadline_expired.load(Ordering::SeqCst), 0);
        assert_eq!(inbox.stats().full.load(Ordering::SeqCst), 0);
        assert_eq!(inbox.stats().after_close.load(Ordering::SeqCst), 0);
    }
    let threads = left.callback_threads.lock().unwrap();
    assert_eq!(threads.len(), 12);
    assert!(threads[..10]
        .iter()
        .all(|thread| *thread == original_thread));
    assert!(threads[10..]
        .iter()
        .all(|thread| *thread == producer_thread));
}

struct DirectPublication {
    sender: Arc<OpaqueRoute<ZenohPreparedCore>>,
    source: UUri,
    calls: AtomicUsize,
    sent: AtomicBool,
    refused: AtomicBool,
}
impl FrameSink<ZenohPreparedRx> for DirectPublication {
    fn push(&self, frame: ZenohPreparedRx) {
        assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
        let before = frame.payload().unwrap().bytes().to_vec();
        let metadata = UFrameMetadata::publish(self.source.clone())
            .with_payload_encoding(PayloadEncoding::RAW)
            .build()
            .unwrap();
        let result = self
            .sender
            .send_bytes(TxHeaderParts::from(&metadata), Some(&99u64.to_le_bytes()));
        self.sent.store(result.is_ok(), Ordering::SeqCst);
        self.refused.store(
            matches!(result, Err(up_rust::SendError::Delivery)),
            Ordering::SeqCst,
        );
        assert_eq!(frame.payload().unwrap().bytes(), before);
    }
    fn close(&self) {}
}

async fn callback_core(session: zenoh::Session, sources: &[UUri]) -> Arc<ZenohPreparedCore> {
    let limits = RouteLimits {
        max_metadata_len: 3072,
        max_payload_len: 8,
        payload_alignment: 8,
    };
    let catalog: Vec<_> = sources
        .iter()
        .map(|source| ZenohRouteReservation {
            source: source.clone(),
            sink: None,
            limits,
            transmit: true,
        })
        .collect();
    Arc::new(
        ZenohPreparedCore::from_session(
            session,
            "//prepared-dispatch/4321/1/1/0",
            &catalog,
            ZenohPreparedConfig {
                shm_segment_size: 1024 * 1024,
                route_capacity: 4,
                registration_capacity: 2,
                allocation_observer: None,
            },
        )
        .await
        .unwrap(),
    )
}

fn callback_route(
    core: &Arc<ZenohPreparedCore>,
    source: UUri,
) -> Arc<OpaqueRoute<ZenohPreparedCore>> {
    Arc::new(
        OpaqueTemplate::for_wire::<ProtobufWire>()
            .unwrap()
            .route(
                Arc::clone(core),
                source,
                None,
                RouteLimits {
                    max_metadata_len: 3072,
                    max_payload_len: 8,
                    payload_alignment: 8,
                },
                RouteConfig::default(),
            )
            .unwrap(),
    )
}

async fn callback_session() -> zenoh::Session {
    let mut config = zenoh::Config::default();
    config.insert_json5("listen/endpoints", "[]").unwrap();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    zenoh::open(config).await.unwrap()
}

#[test_case::test_case(false; "same prepared core")]
#[test_case::test_case(true; "another prepared core sharing session")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn direct_native_callback_refuses_recursive_publication_before_put(another_core: bool) {
    let session = callback_session().await;
    let core = callback_core(session.clone(), &[topic()]).await;
    let publishing = if another_core {
        callback_core(session, &[topic()]).await
    } else {
        Arc::clone(&core)
    };
    let sink = Arc::new(DirectPublication {
        sender: callback_route(&publishing, topic()),
        source: topic(),
        calls: AtomicUsize::new(0),
        sent: AtomicBool::new(false),
        refused: AtomicBool::new(false),
    });
    let mut subscription = core.attach(&topic(), None, sink.clone()).await.unwrap();
    send(&callback_route(&core, topic()), 0);
    assert!(sink.refused.load(Ordering::SeqCst));
    assert!(!sink.sent.load(Ordering::SeqCst));
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert_eq!(core.stats().received_shm.load(Ordering::SeqCst), 1);
    assert_eq!(publishing.stats().publish_failed.load(Ordering::SeqCst), 1);
    core.detach(&mut subscription).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn nested_native_callback_can_forward_but_refuses_an_entered_ancestor() {
    let second_source = UUri::try_from("//prepared-dispatch/4321/1/1/8002").unwrap();
    let core = callback_core(callback_session().await, &[topic(), second_source.clone()]).await;
    let first = Arc::new(DirectPublication {
        sender: callback_route(&core, second_source.clone()),
        source: second_source.clone(),
        calls: AtomicUsize::new(0),
        sent: AtomicBool::new(false),
        refused: AtomicBool::new(false),
    });
    let second = Arc::new(DirectPublication {
        sender: callback_route(&core, topic()),
        source: topic(),
        calls: AtomicUsize::new(0),
        sent: AtomicBool::new(false),
        refused: AtomicBool::new(false),
    });
    let mut first_subscription = core.attach(&topic(), None, first.clone()).await.unwrap();
    let mut second_subscription = core
        .attach(&second_source, None, second.clone())
        .await
        .unwrap();
    send(&callback_route(&core, topic()), 0);
    assert!(first.sent.load(Ordering::SeqCst));
    assert!(second.refused.load(Ordering::SeqCst));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    assert_eq!(core.stats().received_shm.load(Ordering::SeqCst), 2);
    assert_eq!(core.stats().publish_failed.load(Ordering::SeqCst), 1);
    core.detach(&mut first_subscription).unwrap();
    core.detach(&mut second_subscription).unwrap();
}
