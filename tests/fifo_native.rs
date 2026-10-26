// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Real ordered-sender FIFO and independent listener observations on three APIs.
#![cfg(all(feature = "zero-copy", feature = "benchmark-owned"))]

use async_trait::async_trait;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use up_rust::{
    Frame, FrameSink, OpaqueTemplate, PayloadEncoding, PreparedTransportCore, ProtobufWire,
    RouteConfig, RouteLimits, TxHeaderParts, UFrameMetadata, UListener, UMessage, UMessageBuilder,
    UOwnedFrame, UOwnedListener, UOwnedTransport, UTransport, UUri,
};
use up_transport_zenoh::{
    zenoh_config, UPTransportZenoh, ZenohOwnedCore, ZenohPreparedConfig, ZenohPreparedCore,
    ZenohPreparedRx, ZenohRouteReservation,
};

const COUNT: u64 = 1000;
type Observed = Arc<Mutex<Vec<u64>>>;
fn observed() -> Observed {
    Arc::new(Mutex::new(Vec::with_capacity(
        usize::try_from(COUNT).unwrap(),
    )))
}
fn record(target: &Observed, bytes: &[u8]) {
    target
        .lock()
        .unwrap()
        .push(u64::from_le_bytes(bytes.try_into().unwrap()));
}
fn topic(name: &str) -> UUri {
    UUri::try_from_parts(name, 0x4321, 1, 0x8001).unwrap()
}
fn config() -> zenoh_config::Config {
    let mut config = zenoh_config::Config::default();
    config.insert_json5("listen/endpoints", "[]").unwrap();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}
async fn verify(left: &Observed, right: &Observed) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while left.lock().unwrap().len() != usize::try_from(COUNT).unwrap()
            || right.lock().unwrap().len() != usize::try_from(COUNT).unwrap()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*left.lock().unwrap(), (0..COUNT).collect::<Vec<_>>());
    assert_eq!(*right.lock().unwrap(), (0..COUNT).collect::<Vec<_>>());
}
struct Classic(Observed);
#[async_trait]
impl UListener for Classic {
    async fn on_receive(&self, message: UMessage) {
        record(&self.0, &message.payload().unwrap());
    }
}
struct Owned(Observed);
#[async_trait]
impl UOwnedListener for Owned {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        record(&self.0, frame.payload_bytes());
    }
}
struct Native(Observed);
impl FrameSink<ZenohPreparedRx> for Native {
    fn push(&self, frame: ZenohPreparedRx) {
        record(&self.0, frame.payload().unwrap().bytes());
    }
    fn close(&self) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn classic_ordered_sender_reaches_each_listener_in_fifo_order() {
    let source = topic("fifo-classic");
    let transport = UPTransportZenoh::builder("fifo-classic")
        .unwrap()
        .with_session(zenoh::open(config()).await.unwrap())
        .build()
        .unwrap();
    let left = observed();
    let right = observed();
    let listeners: [Arc<dyn UListener>; 2] = [
        Arc::new(Classic(Arc::clone(&left))),
        Arc::new(Classic(Arc::clone(&right))),
    ];
    for listener in &listeners {
        transport
            .register_listener(&source, None, Arc::clone(listener))
            .await
            .unwrap();
    }
    for sequence in 0..COUNT {
        transport
            .send(
                UMessageBuilder::publish(source.clone())
                    .build_with_payload(sequence.to_le_bytes().to_vec(), PayloadEncoding::RAW)
                    .unwrap(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    verify(&left, &right).await;
    for listener in listeners {
        transport
            .unregister_listener(&source, None, listener)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn owned_ordered_sender_reaches_each_listener_in_fifo_order() {
    let source = topic("fifo-owned");
    let transport = ZenohOwnedCore::new(config(), "//fifo-owned/4321/1/0")
        .await
        .unwrap()
        .with_selected_wire(ProtobufWire);
    let left = observed();
    let right = observed();
    let listeners: [Arc<dyn UOwnedListener>; 2] = [
        Arc::new(Owned(Arc::clone(&left))),
        Arc::new(Owned(Arc::clone(&right))),
    ];
    for listener in &listeners {
        transport
            .register_owned_listener(&source, None, Arc::clone(listener))
            .await
            .unwrap();
    }
    for sequence in 0..COUNT {
        let metadata = UFrameMetadata::publish(source.clone())
            .with_payload_encoding(PayloadEncoding::RAW)
            .build()
            .unwrap();
        transport
            .send_owned(
                UOwnedFrame::with_payload(metadata, sequence.to_le_bytes().to_vec()).unwrap(),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    verify(&left, &right).await;
    for listener in listeners {
        transport
            .unregister_owned_listener(&source, None, listener)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn prepared_shm_ordered_sender_reaches_each_native_listener_in_fifo_order() {
    let source = topic("fifo-native");
    let limits = RouteLimits {
        max_metadata_len: 3072,
        max_payload_len: 8,
        payload_alignment: 8,
    };
    let core = Arc::new(
        ZenohPreparedCore::from_session(
            zenoh::open(config()).await.unwrap(),
            "//fifo-native/4321/1/0",
            &[ZenohRouteReservation {
                source: source.clone(),
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
    let left = observed();
    let right = observed();
    let mut first = core
        .attach(&source, None, Arc::new(Native(Arc::clone(&left))))
        .await
        .unwrap();
    let mut second = core
        .attach(&source, None, Arc::new(Native(Arc::clone(&right))))
        .await
        .unwrap();
    core.wait_for_matching(&source, None, Duration::from_secs(5))
        .await
        .unwrap();
    let sender = OpaqueTemplate::for_wire::<ProtobufWire>()
        .unwrap()
        .route(
            Arc::clone(&core),
            source.clone(),
            None,
            limits,
            RouteConfig::default(),
        )
        .unwrap();
    let metadata = UFrameMetadata::publish(source)
        .with_payload_encoding(PayloadEncoding::RAW)
        .build()
        .unwrap();
    for sequence in 0..COUNT {
        sender
            .send_bytes(
                TxHeaderParts::from(&metadata),
                Some(&sequence.to_le_bytes()),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    verify(&left, &right).await;
    core.detach(&mut first).unwrap();
    core.detach(&mut second).unwrap();
}
