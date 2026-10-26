// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Real two-session native SHM values, placement, typed admission and retention.

#[path = "support/prepared_native.rs"]
mod common;

use common::{Foreign, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use up_rust::{
    Frame, HeaderAdmission, HeaderLimits, MessageFields, OpaqueTemplate, PayloadEncoding,
    PreparedTransportCore, RegistrationInbox, RouteConfig, RxAdmission, RxLease, TxHeaderParts,
    TxTemplate, UFrameMetadata, WireRegistration, ZeroCopyHandler,
};
use up_transport_zenoh::ZenohPreparedRx;

type Observation = (u32, Option<String>, Option<String>);
struct Observe {
    count: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Observation>>>,
}
impl ZeroCopyHandler<Value, ZenohPreparedRx> for Observe {
    async fn on_receive(&mut self, lease: RxLease<Value, ZenohPreparedRx>) {
        let value = lease.payload().unwrap();
        assert!(value
            .bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| usize::from(*byte) == index));
        let header = lease.header().unwrap();
        self.seen.lock().unwrap().push((
            value.sequence,
            header.token().map(str::to_owned),
            header.traceparent().map(str::to_owned),
        ));
        drop(lease);
        self.count.fetch_add(1, Ordering::Release);
    }
}
async fn wait(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::Acquire) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native delivery did not complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_shm_values_tokens_traceparent_fifo_and_foreign_type_rejection() {
    let (sender, receiver) = common::pair(std::mem::size_of::<Value>(), 1).await;
    let agreement = common::agreement();
    let admission = RxAdmission::<Value>::new(agreement.clone(), HeaderLimits::default()).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let worker = admission
        .register_filtered(
            common::source(),
            None,
            common::registration(),
            Observe {
                count: Arc::clone(&count),
                seen: Arc::clone(&seen),
            },
            || {},
        )
        .unwrap();
    let subscription =
        WireRegistration::attach(Arc::clone(&receiver), &common::source(), None, worker)
            .await
            .unwrap();
    sender
        .wait_for_matching(&common::source(), None, Duration::from_secs(5))
        .await
        .unwrap();
    let route = TxTemplate::<Value>::new(agreement.clone())
        .unwrap()
        .route(
            Arc::clone(&sender),
            common::definition(common::source()),
            RouteConfig {
                max_token_len: 2048,
                allow_traceparent: true,
            },
        )
        .unwrap();
    let token = "t".repeat(1536);
    for sequence in 0..300 {
        route
            .send(
                MessageFields {
                    token: (sequence % 3 != 0).then_some(token.as_str()),
                    traceparent: (sequence % 5 == 0).then_some(common::TRACE),
                    ..MessageFields::default()
                },
                |init| {
                    init.into_initializer()
                        .sequence(sequence)
                        .bytes_fill_with(|i| u8::try_from(i).unwrap())
                        .finish()
                },
            )
            .unwrap();
        wait(&count, usize::try_from(sequence).unwrap() + 1).await;
    }
    let foreign = TxTemplate::<Foreign>::new(agreement)
        .unwrap()
        .route(
            Arc::clone(&sender),
            common::definition(common::source()),
            RouteConfig::default(),
        )
        .unwrap();
    foreign
        .send(MessageFields::default(), |init| {
            init.into_initializer().sequence(999).bytes_fill(0).finish()
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while admission.stats().rejected.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    subscription.stop().await.unwrap();
    assert_eq!(count.load(Ordering::Acquire), 300);
    assert_eq!(receiver.stats().received_shm.load(Ordering::Acquire), 301);
    assert_eq!(receiver.stats().rejected_non_shm.load(Ordering::Acquire), 0);
    for (index, (sequence, carried, trace)) in seen.lock().unwrap().iter().enumerate() {
        assert_eq!(usize::try_from(*sequence).unwrap(), index);
        assert_eq!(
            carried.as_deref(),
            (sequence % 3 != 0).then_some(token.as_str())
        );
        assert_eq!(
            trace.as_deref(),
            (sequence % 5 == 0).then_some(common::TRACE)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_and_present_empty_both_arrive_in_native_shm_without_attachment() {
    let (sender, receiver) = common::pair(0, 1).await;
    let inbox =
        Arc::new(RegistrationInbox::<ZenohPreparedRx>::new(common::registration()).unwrap());
    let mut subscription = receiver
        .attach(&common::source(), None, inbox.clone())
        .await
        .unwrap();
    sender
        .wait_for_matching(&common::source(), None, Duration::from_secs(5))
        .await
        .unwrap();
    let route = OpaqueTemplate::for_wire::<up_rust::ProtobufWire>()
        .unwrap()
        .route(
            Arc::clone(&sender),
            common::source(),
            None,
            common::limits(0),
            RouteConfig::default(),
        )
        .unwrap();
    let admission =
        HeaderAdmission::for_wire::<up_rust::ProtobufWire>(HeaderLimits::default()).unwrap();
    for present in [false, true] {
        let builder = UFrameMetadata::publish(common::source());
        let metadata = if present {
            builder.with_payload_encoding(PayloadEncoding::PROTOBUF)
        } else {
            builder
        }
        .build()
        .unwrap();
        route
            .send_bytes(TxHeaderParts::from(&metadata), present.then_some(&[][..]))
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(frame.sample().payload().as_shm().is_some());
        assert!(frame.sample().attachment().is_none());
        assert_eq!(Frame::payload(&frame).is_some(), present);
        let frame = admission.admit(frame).unwrap();
        assert_eq!(frame.payload().is_some(), present);
        assert_eq!(frame.header().unwrap().payload_encoding.is_some(), present);
        if present {
            assert!(frame.payload().unwrap().bytes().is_empty());
        }
    }
    receiver.detach(&mut subscription).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_subscribers_retain_native_samples_after_detach_and_core_drop() {
    let (sender, receiver) = common::pair(std::mem::size_of::<Value>(), 2).await;
    let a = Arc::new(RegistrationInbox::<ZenohPreparedRx>::new(common::registration()).unwrap());
    let b = Arc::new(RegistrationInbox::<ZenohPreparedRx>::new(common::registration()).unwrap());
    let mut first = receiver
        .attach(&common::source(), None, a.clone())
        .await
        .unwrap();
    let mut second = receiver
        .attach(&common::source(), None, b.clone())
        .await
        .unwrap();
    sender
        .wait_for_matching(&common::source(), None, Duration::from_secs(5))
        .await
        .unwrap();
    let agreement = common::agreement();
    let route = TxTemplate::<Value>::new(agreement.clone())
        .unwrap()
        .route(
            Arc::clone(&sender),
            common::definition(common::source()),
            RouteConfig::default(),
        )
        .unwrap();
    route
        .send(MessageFields::default(), |init| {
            init.into_initializer().sequence(17).bytes_fill(42).finish()
        })
        .unwrap();
    let a = tokio::time::timeout(Duration::from_secs(5), a.recv())
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(Duration::from_secs(5), b.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(a.sample().payload().as_shm().is_some());
    assert!(b.sample().payload().as_shm().is_some());
    let admission = RxAdmission::<Value>::new(agreement, HeaderLimits::default()).unwrap();
    let a = admission.admit(a).unwrap();
    let b = admission.admit(b).unwrap();
    let address_a = a.payload().unwrap().bytes.as_ptr();
    let address_b = b.payload().unwrap().bytes.as_ptr();
    receiver.detach(&mut first).unwrap();
    receiver.detach(&mut second).unwrap();
    drop(first);
    drop(second);
    drop(route);
    drop(sender);
    drop(receiver);
    assert_eq!(a.payload().unwrap().sequence, 17);
    assert_eq!(b.payload().unwrap().sequence, 17);
    assert_eq!(a.payload().unwrap().bytes.as_ptr(), address_a);
    assert_eq!(b.payload().unwrap().bytes.as_ptr(), address_b);
    assert_eq!(a.payload().unwrap().bytes, [42; 64]);
    drop(a);
    assert_eq!(b.payload().unwrap().bytes, [42; 64]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_detached_handle_does_not_release_a_reused_registration_slot() {
    let (_sender, receiver) = common::pair(68, 1).await;
    let make_inbox =
        || Arc::new(RegistrationInbox::<ZenohPreparedRx>::new(common::registration()).unwrap());
    let mut old = receiver
        .attach(&common::source(), None, make_inbox())
        .await
        .unwrap();
    receiver.detach(&mut old).unwrap();
    let mut replacement = receiver
        .attach(&common::source(), None, make_inbox())
        .await
        .unwrap();
    drop(old);
    assert!(matches!(
        receiver.attach(&common::source(), None, make_inbox()).await,
        Err(up_rust::SetupError::TableFull)
    ));
    receiver.detach(&mut replacement).unwrap();
    let mut final_registration = receiver
        .attach(&common::source(), None, make_inbox())
        .await
        .unwrap();
    receiver.detach(&mut final_registration).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_only_catalog_reserves_no_unused_local_shm_provider() {
    use std::collections::BTreeSet;
    let pools = || -> BTreeSet<(String, String)> {
        std::fs::read_to_string("/proc/self/maps")
            .unwrap()
            .lines()
            .filter_map(|line| {
                let f: Vec<_> = line.split_whitespace().collect();
                let (start, end) = f[0].split_once('-').unwrap();
                let bytes = usize::from_str_radix(end, 16).unwrap()
                    - usize::from_str_radix(start, 16).unwrap();
                // Count the exact configured payload arena, not Zenoh's small
                // protocol/watchdog shared-memory files with the same suffix.
                (bytes == 1024 * 1024
                    && f.len() >= 6
                    && f[5].starts_with("/dev/shm/")
                    && std::path::Path::new(f[5]).extension()
                        == Some(std::ffi::OsStr::new("zenoh")))
                .then(|| (f[3].to_string(), f[4].to_string()))
            })
            .collect()
    };
    let before = pools();
    let (sender, receiver) = common::pair(0, 1).await;
    assert_eq!(sender.prewarmed_shm_segment_size(), 1024 * 1024);
    assert_eq!(receiver.prewarmed_shm_segment_size(), 0);
    assert_eq!(pools().difference(&before).count(), 1);
    assert!(receiver
        .prepare_route(&common::source(), None, common::limits(0))
        .is_err());
}
