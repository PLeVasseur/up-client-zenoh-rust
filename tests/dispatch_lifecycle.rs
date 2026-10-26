// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Native copied-control fan-out, saturated reentry and shutdown regressions.

use async_trait::async_trait;
use std::{
    future::{poll_fn, Future},
    sync::{Arc, Mutex, Weak},
    task::Poll,
    time::Duration,
};
use tokio::sync::Notify;
use up_rust::{PayloadEncoding, UCode, UListener, UMessage, UMessageBuilder, UTransport, UUri};
use up_transport_zenoh::UPTransportZenoh;

async fn session() -> zenoh::Session {
    let mut config = zenoh::Config::default();
    config.insert_json5("listen/endpoints", "[]").unwrap();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    zenoh::open(config).await.unwrap()
}

fn transport(session: &zenoh::Session) -> Arc<UPTransportZenoh> {
    Arc::new(
        UPTransportZenoh::builder("dispatch")
            .unwrap()
            .with_session(session.clone())
            .build()
            .unwrap(),
    )
}

fn topic(resource: u16) -> UUri {
    UUri::try_from_parts("dispatch", 0x4321, 1, resource).unwrap()
}

fn message(resource: u16, value: u8) -> UMessage {
    UMessageBuilder::publish(topic(resource))
        .build_with_payload(vec![value], PayloadEncoding::RAW)
        .unwrap()
}

#[derive(Default)]
struct Paused {
    entered: Notify,
    release: Notify,
    received: Mutex<Vec<u8>>,
}

#[async_trait]
impl UListener for Paused {
    async fn on_receive(&self, message: UMessage) {
        let value = message.payload().unwrap()[0];
        self.received.lock().unwrap().push(value);
        if value == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

async fn entered(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .unwrap();
}

async fn received(values: &Mutex<Vec<u8>>, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while values.lock().unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

struct Reentrant {
    publisher: Weak<UPTransportZenoh>,
    entered: Notify,
    release: Notify,
    received: Mutex<Vec<u8>>,
    outcomes: Mutex<Vec<Result<(), UCode>>>,
    completed: Notify,
}

#[async_trait]
impl UListener for Reentrant {
    async fn on_receive(&self, message: UMessage) {
        let value = message.payload().unwrap()[0];
        self.received.lock().unwrap().push(value);
        if value == 0 || value == 3 {
            self.entered.notify_one();
            self.release.notified().await;
            let result = self
                .publisher
                .upgrade()
                .unwrap()
                .send(crate::message(0x8001, if value == 0 { 2 } else { 4 }))
                .await;
            self.outcomes
                .lock()
                .unwrap()
                .push(result.map_err(|error| error.code()));
            self.completed.notify_one();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn one_saturated_fanout_target_refuses_before_any_native_publication() {
    let session = session().await;
    let first = transport(&session);
    let second = transport(&session);
    let publisher = transport(&session);
    let reentrant = Arc::new(Reentrant {
        publisher: Arc::downgrade(&publisher),
        entered: Notify::new(),
        release: Notify::new(),
        completed: Notify::new(),
        received: Mutex::new(Vec::new()),
        outcomes: Mutex::new(Vec::new()),
    });
    let paused = Arc::new(Paused::default());
    let first_listener: Arc<dyn UListener> = reentrant.clone();
    let second_listener: Arc<dyn UListener> = paused.clone();
    first
        .register_listener(&topic(0x8001), None, Arc::clone(&first_listener))
        .await
        .unwrap();
    second
        .register_listener(&UUri::any(), None, Arc::clone(&second_listener))
        .await
        .unwrap();
    let raw = Arc::new(Mutex::new(Vec::new()));
    let raw_callback = Arc::clone(&raw);
    let witness = session
        .declare_subscriber("up/**")
        .callback(move |sample: zenoh::sample::Sample| {
            raw_callback
                .lock()
                .unwrap()
                .push(sample.payload().to_bytes()[0]);
        })
        .await
        .unwrap();

    publisher.send(message(0x8001, 0)).await.unwrap();
    entered(&reentrant.entered).await;
    entered(&paused.entered).await;
    // Only the wildcard listener's one-slot queue is filled. The first target's
    // reservation must roll back when the later fan-out destination refuses.
    publisher.send(message(0x8002, 1)).await.unwrap();
    reentrant.release.notify_one();
    entered(&reentrant.completed).await;
    assert_eq!(
        *reentrant.outcomes.lock().unwrap(),
        [Err(UCode::ResourceExhausted)]
    );
    assert_eq!(*raw.lock().unwrap(), [0, 1]);
    paused.release.notify_one();
    received(&paused.received, 2).await;

    publisher.send(message(0x8001, 3)).await.unwrap();
    entered(&reentrant.entered).await;
    received(&paused.received, 3).await;
    reentrant.release.notify_one();
    entered(&reentrant.completed).await;
    received(&reentrant.received, 3).await;
    received(&paused.received, 4).await;
    assert_eq!(
        *reentrant.outcomes.lock().unwrap(),
        [Err(UCode::ResourceExhausted), Ok(())]
    );
    assert_eq!(*reentrant.received.lock().unwrap(), [0, 3, 4]);
    assert_eq!(*paused.received.lock().unwrap(), [0, 1, 3, 4]);
    assert_eq!(*raw.lock().unwrap(), [0, 1, 3, 4]);
    first
        .unregister_listener(&topic(0x8001), None, first_listener)
        .await
        .unwrap();
    second
        .unregister_listener(&UUri::any(), None, second_listener)
        .await
        .unwrap();
    witness.undeclare().await.unwrap();
}

#[test_case::test_case(false; "drain entered callback")]
#[test_case::test_case(true; "cancel unregister while draining")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn unregister_wakes_a_blocked_native_producer_before_draining_the_handler(cancel: bool) {
    let transport = transport(&session().await);
    let paused = Arc::new(Paused::default());
    let listener: Arc<dyn UListener> = paused.clone();
    transport
        .register_listener(&topic(0x8001), None, Arc::clone(&listener))
        .await
        .unwrap();
    transport.send(message(0x8001, 0)).await.unwrap();
    entered(&paused.entered).await;
    transport.send(message(0x8001, 1)).await.unwrap();
    let publishing = Arc::clone(&transport);
    let runtime = tokio::runtime::Handle::current();
    let (started, start) = std::sync::mpsc::sync_channel(1);
    let (finished, finish) = std::sync::mpsc::sync_channel(1);
    let producer = std::thread::spawn(move || {
        started.send(()).unwrap();
        let result = runtime.block_on(publishing.send(message(0x8001, 2)));
        finished.send(result).unwrap();
    });
    start.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        finish.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    let source = topic(0x8001);
    let mut unregister = Box::pin(transport.unregister_listener(&source, None, listener));
    assert!(poll_fn(|cx| Poll::Ready(unregister.as_mut().poll(cx)))
        .await
        .is_pending());
    finish
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    producer.join().unwrap();
    assert_eq!(*paused.received.lock().unwrap(), [0]);
    if cancel {
        drop(unregister);
        return;
    }
    paused.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), unregister)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*paused.received.lock().unwrap(), [0]);
}

struct SelfRemoving {
    own: Weak<Self>,
    transport: Weak<UPTransportZenoh>,
    result: Mutex<Option<tokio::sync::oneshot::Sender<Result<(), UCode>>>>,
}

#[async_trait]
impl UListener for SelfRemoving {
    async fn on_receive(&self, _: UMessage) {
        let listener: Arc<dyn UListener> = self.own.upgrade().unwrap();
        let result = self
            .transport
            .upgrade()
            .unwrap()
            .unregister_listener(&topic(0x8001), None, listener)
            .await
            .map_err(|error| error.code());
        self.result
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(result)
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn classic_callback_unregisters_its_own_worker() {
    let transport = transport(&session().await);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let listener = Arc::new_cyclic(|own| SelfRemoving {
        own: own.clone(),
        transport: Arc::downgrade(&transport),
        result: Mutex::new(Some(sender)),
    });
    transport
        .register_listener(&topic(0x8001), None, listener)
        .await
        .unwrap();
    transport.send(message(0x8001, 0)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), receiver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn dropping_receiver_closes_all_fanout_credits_before_native_retirement() {
    let session = session().await;
    let receiver = transport(&session);
    let publisher = transport(&session);
    let left = Arc::new(Paused::default());
    let right = Arc::new(Paused::default());
    let listeners: [Arc<dyn UListener>; 2] = [left.clone(), right.clone()];
    for listener in listeners {
        receiver
            .register_listener(&topic(0x8001), None, listener)
            .await
            .unwrap();
    }
    publisher.send(message(0x8001, 0)).await.unwrap();
    entered(&left.entered).await;
    entered(&right.entered).await;
    publisher.send(message(0x8001, 1)).await.unwrap();
    let runtime = tokio::runtime::Handle::current();
    let (started, start) = std::sync::mpsc::sync_channel(1);
    let (finished, finish) = std::sync::mpsc::sync_channel(1);
    let producer = std::thread::spawn(move || {
        started.send(()).unwrap();
        finished
            .send(runtime.block_on(publisher.send(message(0x8001, 2))))
            .unwrap();
    });
    start.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        finish.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    // The publisher retains the shared native session, not this receiver's registry.
    drop(receiver);
    finish
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    producer.join().unwrap();
    assert_eq!(*left.received.lock().unwrap(), [0]);
    assert_eq!(*right.received.lock().unwrap(), [0]);
}
