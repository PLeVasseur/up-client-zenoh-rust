// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Native baseline observations used before choosing bounded dispatch mechanics.
#![cfg(feature = "zero-copy")]

#[path = "support/fifo_feasibility.rs"]
mod reservation;

use async_trait::async_trait;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use up_rust::{
    Frame, FrameSink, OpaqueTemplate, OverflowPolicy, PayloadEncoding, PreparedTransportCore,
    ProtobufWire, RegistrationConfig, RegistrationInbox, RouteConfig, RouteLimits, TxHeaderParts,
    UFrameMetadata, UListener, UMessage, UMessageBuilder, UTransport, UUri,
};
use up_transport_zenoh::{
    zenoh_config, UPTransportZenoh, ZenohPreparedConfig, ZenohPreparedCore, ZenohPreparedRx,
    ZenohRouteReservation,
};

fn config() -> zenoh_config::Config {
    let mut value = zenoh_config::Config::default();
    value.insert_json5("listen/endpoints", "[]").unwrap();
    value
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    value
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    value
}
fn topic() -> UUri {
    UUri::try_from_parts("fifo-probe", 0x4321, 1, 0x8001).unwrap()
}
fn limits() -> RouteLimits {
    RouteLimits {
        max_metadata_len: 3072,
        max_payload_len: 8,
        payload_alignment: 8,
    }
}
async fn core() -> Arc<ZenohPreparedCore> {
    let session = zenoh::open(config()).await.unwrap();
    Arc::new(
        ZenohPreparedCore::from_session(
            session,
            "//fifo-probe/4321/1/0",
            &[ZenohRouteReservation {
                source: topic(),
                sink: None,
                limits: limits(),
                transmit: true,
            }],
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
fn metadata() -> UFrameMetadata {
    UFrameMetadata::publish(topic())
        .with_payload_encoding(PayloadEncoding::PROTOBUF)
        .build()
        .unwrap()
}

struct InlineSink {
    caller: std::thread::ThreadId,
    inline: Arc<AtomicBool>,
}
impl FrameSink<ZenohPreparedRx> for InlineSink {
    fn push(&self, frame: ZenohPreparedRx) {
        assert_eq!(frame.payload().unwrap().bytes(), b"native");
        self.inline.store(
            std::thread::current().id() == self.caller,
            Ordering::Release,
        );
    }
    fn close(&self) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn native_local_put_invokes_the_sink_on_the_publishing_thread() {
    let core = core().await;
    let inline = Arc::new(AtomicBool::new(false));
    let mut registration = core
        .attach(
            &topic(),
            None,
            Arc::new(InlineSink {
                caller: std::thread::current().id(),
                inline: Arc::clone(&inline),
            }),
        )
        .await
        .unwrap();
    core.wait_for_matching(&topic(), None, Duration::from_secs(5))
        .await
        .unwrap();
    let route = OpaqueTemplate::for_wire::<ProtobufWire>()
        .unwrap()
        .route(
            Arc::clone(&core),
            topic(),
            None,
            limits(),
            RouteConfig::default(),
        )
        .unwrap();
    route
        .send_bytes(TxHeaderParts::from(&metadata()), Some(b"native"))
        .unwrap();
    assert!(inline.load(Ordering::Acquire));
    assert_eq!(core.stats().received_shm.load(Ordering::Acquire), 1);
    core.detach(&mut registration).unwrap();
    println!("FEASIBILITY native_callback_inline=true application_frames=1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn full_sdk_block_inbox_cannot_drain_on_its_blocked_publisher_thread() {
    let core = core().await;
    let inbox = Arc::new(
        RegistrationInbox::new(RegistrationConfig {
            capacity: 1,
            overflow: OverflowPolicy::Block {
                deadline: Duration::from_millis(20),
            },
        })
        .unwrap(),
    );
    let mut registration = core.attach(&topic(), None, inbox.clone()).await.unwrap();
    core.wait_for_matching(&topic(), None, Duration::from_secs(5))
        .await
        .unwrap();
    let route = OpaqueTemplate::for_wire::<ProtobufWire>()
        .unwrap()
        .route(
            Arc::clone(&core),
            topic(),
            None,
            limits(),
            RouteConfig::default(),
        )
        .unwrap();
    let header = metadata();
    route
        .send_bytes(TxHeaderParts::from(&header), Some(b"first"))
        .unwrap();
    let started = Instant::now();
    route
        .send_bytes(TxHeaderParts::from(&header), Some(b"second"))
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(20));
    assert_eq!(inbox.stats().deadline_expired.load(Ordering::Acquire), 1);
    assert_eq!(inbox.len(), 1);
    assert_eq!(
        inbox.recv().await.unwrap().payload().unwrap().bytes(),
        b"first"
    );
    core.detach(&mut registration).unwrap();
    println!("FEASIBILITY inline_full_block=deadline_expired published=2 admitted=1 binding_fifo_solution=false");
}

struct DelayedFirst {
    completed: Arc<Mutex<Vec<u8>>>,
}
#[async_trait]
impl UListener for DelayedFirst {
    async fn on_receive(&self, message: UMessage) {
        let value = message.payload().unwrap()[0];
        if value == 0 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        self.completed.lock().unwrap().push(value);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn classic_ordered_worker_completes_the_first_callback_before_the_next() {
    let session = zenoh::open(config()).await.unwrap();
    let transport = UPTransportZenoh::builder("fifo-probe")
        .unwrap()
        .with_session(session)
        .build()
        .unwrap();
    let completed = Arc::new(Mutex::new(Vec::new()));
    let listener: Arc<dyn UListener> = Arc::new(DelayedFirst {
        completed: Arc::clone(&completed),
    });
    transport
        .register_listener(&topic(), None, Arc::clone(&listener))
        .await
        .unwrap();
    for value in [0u8, 1] {
        transport
            .send(
                UMessageBuilder::publish(topic())
                    .build_with_payload(vec![value], PayloadEncoding::PROTOBUF)
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while completed.lock().unwrap().len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*completed.lock().unwrap(), [0, 1]);
    transport
        .unregister_listener(&topic(), None, listener)
        .await
        .unwrap();
    println!("DISPATCH classic_callback_completion=[0,1] worker_ordered=true");
}

struct OrderedBytes(Arc<Mutex<Vec<u8>>>);
impl up_rust::RegistrationHandler<u8> for OrderedBytes {
    async fn on_receive(&mut self, value: u8) {
        tokio::time::sleep(Duration::from_millis(2)).await;
        self.0.lock().unwrap().push(value);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn setup_owned_worker_runtime_preserves_fifo_while_inline_producers_wait() {
    let session = zenoh::open(config()).await.unwrap();
    let runtime = tokio::task::spawn_blocking(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
    .await
    .unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let worker = {
        let _entered = runtime.enter();
        up_rust::Registration::new(
            RegistrationConfig {
                capacity: 1,
                overflow: OverflowPolicy::Block {
                    deadline: Duration::from_secs(5),
                },
            },
            OrderedBytes(Arc::clone(&received)),
            || {},
        )
        .unwrap()
    };
    let inbox = worker.inbox();
    let callback_inbox = Arc::clone(&inbox);
    let subscriber = session
        .declare_subscriber("fifo/ordered-worker")
        .callback_mut(move |sample: zenoh::sample::Sample| {
            let value = sample.payload().to_bytes()[0];
            // Classic/owned dispatch releases the native sample before queueing
            // its copied value. This probe makes no native-lease budget claim.
            drop(sample);
            callback_inbox.push(value).unwrap();
        })
        .await
        .unwrap();
    for value in 0u8..32 {
        session
            .put("fifo/ordered-worker", vec![value])
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.lock().unwrap().len() != 32 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*received.lock().unwrap(), (0u8..32).collect::<Vec<_>>());
    assert!(inbox.stats().blocked.load(Ordering::Acquire) > 0);
    assert_eq!(inbox.stats().deadline_expired.load(Ordering::Acquire), 0);
    worker.stop().await.unwrap();
    subscriber.undeclare().await.unwrap();
    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
    println!("FEASIBILITY independent_worker_runtime=true capacity=1 ordered=32 overflow=0");
}

struct ReentrantProbe {
    activity: Arc<up_rust::ListenerAdmission>,
    inbox: Arc<std::sync::OnceLock<Arc<RegistrationInbox<u8>>>>,
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
    refused: Arc<AtomicBool>,
    completed: Arc<Mutex<Vec<u8>>>,
}
impl up_rust::RegistrationHandler<u8> for ReentrantProbe {
    async fn on_receive(&mut self, value: u8) {
        let activity = Arc::clone(&self.activity);
        activity
            .dispatch(|| async {
                if value == 0 {
                    self.entered.notify_one();
                    self.resume.notified().await;
                    // Candidate admission: refuse before native publication when
                    // this handler would synchronously wait on its own full inbox.
                    // No sample is created or discarded by a binding overflow path.
                    let refused =
                        self.activity.is_current_callback() && self.inbox.get().unwrap().len() == 1;
                    assert!(refused);
                    self.refused.store(true, Ordering::Release);
                }
                self.completed.lock().unwrap().push(value);
            })
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn callback_context_can_refuse_saturated_reentry_before_native_publication() {
    let runtime = tokio::task::spawn_blocking(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
    .await
    .unwrap();
    let session = zenoh::open(config()).await.unwrap();
    let activity = Arc::new(up_rust::ListenerAdmission::new());
    let holder = Arc::new(std::sync::OnceLock::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let refused = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(Mutex::new(Vec::new()));
    let worker = {
        let _entered = runtime.enter();
        up_rust::Registration::new(
            RegistrationConfig {
                capacity: 1,
                overflow: OverflowPolicy::Block {
                    deadline: Duration::from_secs(5),
                },
            },
            ReentrantProbe {
                activity: Arc::clone(&activity),
                inbox: Arc::clone(&holder),
                entered: Arc::clone(&entered),
                resume: Arc::clone(&resume),
                refused: Arc::clone(&refused),
                completed: Arc::clone(&completed),
            },
            || {},
        )
        .unwrap()
    };
    let inbox = worker.inbox();
    holder.set(Arc::clone(&inbox)).unwrap();
    let callback_inbox = Arc::clone(&inbox);
    let subscriber = session
        .declare_subscriber("fifo/reentrant-admission")
        .callback_mut(move |sample: zenoh::sample::Sample| {
            let value = sample.payload().to_bytes()[0];
            drop(sample);
            callback_inbox.push(value).unwrap();
        })
        .await
        .unwrap();
    session
        .put("fifo/reentrant-admission", vec![0u8])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    session
        .put("fifo/reentrant-admission", vec![1u8])
        .await
        .unwrap();
    assert_eq!(inbox.len(), 1);
    resume.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while completed.lock().unwrap().len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(refused.load(Ordering::Acquire));
    assert_eq!(*completed.lock().unwrap(), [0, 1]);
    assert_eq!(inbox.stats().deadline_expired.load(Ordering::Acquire), 0);
    activity.stop().await;
    worker.stop().await.unwrap();
    subscriber.undeclare().await.unwrap();
    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
    println!("FEASIBILITY callback_context_detected=true saturated_reentry_prepublication_refusal=true overflow=0");
}

struct ReservedReentry {
    session: zenoh::Session,
    credit: Arc<reservation::Credit>,
    full: bool,
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
    received: Arc<Mutex<Vec<u8>>>,
    refused: Arc<AtomicBool>,
}
impl up_rust::RegistrationHandler<reservation::Item> for ReservedReentry {
    async fn on_receive(&mut self, item: reservation::Item) {
        let value = item.value;
        drop(item.permit);
        self.received.lock().unwrap().push(value);
        if value == 0 {
            self.entered.notify_one();
            self.resume.notified().await;
            if let Some(permit) = self.credit.try_reserve() {
                assert!(!self.full);
                reservation::publish_reserved(permit, || {
                    use zenoh::Wait;
                    self.session
                        .put("fifo/reserved-reentry", vec![2u8])
                        .wait()
                        .unwrap();
                });
            } else {
                assert!(self.full);
                self.refused.store(true, Ordering::Release);
            }
        }
    }
}

#[test_case::test_case(false; "available queue reserves before native reentry")]
#[test_case::test_case(true; "full queue refuses before native reentry")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn reserved_native_publication_delivers_or_refuses_without_callback_overflow(full: bool) {
    let session = zenoh::open(config()).await.unwrap();
    let runtime = tokio::task::spawn_blocking(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
    .await
    .unwrap();
    let credit = Arc::new(reservation::Credit::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let received = Arc::new(Mutex::new(Vec::new()));
    let refused = Arc::new(AtomicBool::new(false));
    let worker = {
        let _entered = runtime.enter();
        up_rust::Registration::new(
            RegistrationConfig {
                capacity: 1,
                overflow: OverflowPolicy::DropNewest,
            },
            ReservedReentry {
                session: session.clone(),
                credit: Arc::clone(&credit),
                full,
                entered: Arc::clone(&entered),
                resume: Arc::clone(&resume),
                received: Arc::clone(&received),
                refused: Arc::clone(&refused),
            },
            || {},
        )
        .unwrap()
    };
    let inbox = worker.inbox();
    let callback_inbox = Arc::clone(&inbox);
    let callback_credit = Arc::clone(&credit);
    let subscriber = session
        .declare_subscriber("fifo/reserved-reentry")
        .callback(move |sample: zenoh::sample::Sample| {
            let value = sample.payload().to_bytes()[0];
            drop(sample);
            let permit = callback_credit.receive();
            callback_inbox
                .push(reservation::Item { value, permit })
                .unwrap();
        })
        .await
        .unwrap();
    session
        .put("fifo/reserved-reentry", vec![0u8])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    if full {
        session
            .put("fifo/reserved-reentry", vec![1u8])
            .await
            .unwrap();
    }
    resume.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.lock().unwrap().len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *received.lock().unwrap(),
        if full { vec![0, 1] } else { vec![0, 2] }
    );
    assert_eq!(refused.load(Ordering::Acquire), full);
    assert_eq!(inbox.stats().full.load(Ordering::Acquire), 0);
    assert_eq!(inbox.stats().deadline_expired.load(Ordering::Acquire), 0);
    worker.stop().await.unwrap();
    subscriber.undeclare().await.unwrap();
    assert!(credit.try_reserve().is_some());
    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
    println!("FEASIBILITY reserved_reentry=true full={full} binding_overflow=0 capacity=1");
}

struct ClassicReentry {
    transport: std::sync::Weak<UPTransportZenoh>,
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
    received: Arc<Mutex<Vec<u8>>>,
    result: Arc<Mutex<Option<Result<(), up_rust::UCode>>>>,
}
#[async_trait]
impl UListener for ClassicReentry {
    async fn on_receive(&self, message: UMessage) {
        let value = message.payload().unwrap()[0];
        self.received.lock().unwrap().push(value);
        if value == 0 {
            self.entered.notify_one();
            self.resume.notified().await;
            let result = self
                .transport
                .upgrade()
                .unwrap()
                .send(
                    UMessageBuilder::publish(topic())
                        .build_with_payload(vec![2u8], PayloadEncoding::PROTOBUF)
                        .unwrap(),
                )
                .await
                .map_err(|error| error.code());
            *self.result.lock().unwrap() = Some(result);
        }
    }
}

#[test_case::test_case(false, false; "production available local reentry")]
#[test_case::test_case(true, false; "production saturated local reentry")]
#[test_case::test_case(false, true; "production available shared session reentry")]
#[test_case::test_case(true, true; "production saturated shared session reentry")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn classic_dispatch_reserves_before_reentrant_put(full: bool, shared: bool) {
    let session = zenoh::open(config()).await.unwrap();
    let transport = Arc::new(
        UPTransportZenoh::builder("fifo-probe")
            .unwrap()
            .with_session(session.clone())
            .build()
            .unwrap(),
    );
    let publisher = if shared {
        Arc::new(
            UPTransportZenoh::builder("fifo-probe")
                .unwrap()
                .with_session(session)
                .build()
                .unwrap(),
        )
    } else {
        Arc::clone(&transport)
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let received = Arc::new(Mutex::new(Vec::new()));
    let result = Arc::new(Mutex::new(None));
    let listener: Arc<dyn UListener> = Arc::new(ClassicReentry {
        transport: Arc::downgrade(&publisher),
        entered: Arc::clone(&entered),
        resume: Arc::clone(&resume),
        received: Arc::clone(&received),
        result: Arc::clone(&result),
    });
    transport
        .register_listener(&topic(), None, Arc::clone(&listener))
        .await
        .unwrap();
    transport
        .send(
            UMessageBuilder::publish(topic())
                .build_with_payload(vec![0u8], PayloadEncoding::PROTOBUF)
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    if full {
        transport
            .send(
                UMessageBuilder::publish(topic())
                    .build_with_payload(vec![1u8], PayloadEncoding::PROTOBUF)
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    resume.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.lock().unwrap().len() != 2 || result.lock().unwrap().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *received.lock().unwrap(),
        if full { vec![0, 1] } else { vec![0, 2] }
    );
    assert_eq!(
        *result.lock().unwrap(),
        Some(if full {
            Err(up_rust::UCode::ResourceExhausted)
        } else {
            Ok(())
        })
    );
    transport
        .unregister_listener(&topic(), None, listener)
        .await
        .unwrap();
}
