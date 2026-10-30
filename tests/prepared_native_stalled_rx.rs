// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Real two-session SHM frames survive a stalled native receive path.
//!
//! The receiving native callback blocks while later explicit SHM frames are
//! already in transit on the same reliable link. Once the callback returns,
//! every frame must still reach it intact and in order: the stall spans several
//! native SHM watchdog validation periods, so in-transit ownership must not
//! depend on how quickly the receiver maps each descriptor.
#![cfg(all(feature = "zero-copy", feature = "benchmark-owned"))]

#[path = "support/prepared_native.rs"]
mod common;

use std::{
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};
use up_rust::{
    Frame, FrameSink, OpaqueTemplate, PayloadEncoding, PreparedTransportCore, ProtobufWire,
    RouteConfig, TxHeaderParts, UFrameMetadata,
};
use up_transport_zenoh::ZenohPreparedRx;

const FRAMES: u64 = 32;
/// Longer than several native SHM watchdog validation periods.
const STALL: Duration = Duration::from_secs(1);

/// Records each delivered sequence; optionally blocks the native callback on
/// the first frame, which stalls the receiving transport behind it.
struct StallingSink {
    delivered: Arc<Mutex<Vec<u64>>>,
    stall_first: bool,
}

impl FrameSink<ZenohPreparedRx> for StallingSink {
    fn push(&self, frame: ZenohPreparedRx) {
        let sequence = u64::from_le_bytes(frame.payload().unwrap().bytes().try_into().unwrap());
        drop(frame);
        let first = {
            let mut delivered = self.delivered.lock().unwrap();
            delivered.push(sequence);
            delivered.len() == 1
        };
        if first && self.stall_first {
            std::thread::sleep(STALL);
        }
    }

    fn close(&self) {}
}

async fn wait_for(delivered: &Mutex<Vec<u64>>, count: usize, within: Duration) -> Vec<u64> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let observed = delivered.lock().unwrap().clone();
        if observed.len() >= count || tokio::time::Instant::now() >= deadline {
            return observed;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn deliver_frames_across_receive_path(stall_first: bool) {
    let (sender, receiver) = common::pair(8, 1).await;
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let mut subscription = receiver
        .attach(
            &common::source(),
            None,
            Arc::new(StallingSink {
                delivered: Arc::clone(&delivered),
                stall_first,
            }),
        )
        .await
        .unwrap();
    sender
        .wait_for_matching(&common::source(), None, Duration::from_secs(5))
        .await
        .unwrap();
    let route = OpaqueTemplate::for_wire::<ProtobufWire>()
        .unwrap()
        .route(
            Arc::clone(&sender),
            common::source(),
            None,
            common::limits(8),
            RouteConfig::default(),
        )
        .unwrap();
    let metadata = UFrameMetadata::publish(common::source())
        .with_payload_encoding(PayloadEncoding::RAW)
        .build()
        .unwrap();
    let send = |sequence: u64| {
        route
            .send_bytes(
                TxHeaderParts::from(&metadata),
                Some(&sequence.to_le_bytes()),
            )
            .unwrap();
    };

    // The first callback has been entered before any later frame is published,
    // so those frames wait in transit for the whole stall.
    send(0);
    assert_eq!(wait_for(&delivered, 1, Duration::from_secs(5)).await, [0]);
    for sequence in 1..FRAMES {
        send(sequence);
    }
    let expected = usize::try_from(FRAMES).unwrap();
    let observed = wait_for(&delivered, expected, STALL + Duration::from_secs(5)).await;
    receiver.detach(&mut subscription).unwrap();

    assert_eq!(
        observed,
        (0..FRAMES).collect::<Vec<_>>(),
        "every in-transit SHM frame must reach the native callback in order"
    );
    let stats = receiver.stats();
    assert_eq!(stats.received_shm.load(Ordering::Acquire), FRAMES);
    assert_eq!(stats.rejected_non_shm.load(Ordering::Acquire), 0);
    assert_eq!(stats.rejected_layout.load(Ordering::Acquire), 0);
    assert_eq!(sender.stats().published.load(Ordering::Acquire), FRAMES);
    assert_eq!(sender.stats().publish_failed.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_transit_shm_frames_arrive_in_order_without_a_receive_stall() {
    deliver_frames_across_receive_path(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_transit_shm_frames_survive_a_stalled_native_receive_path() {
    deliver_frames_across_receive_path(true).await;
}
