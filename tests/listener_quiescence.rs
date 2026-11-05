// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Real Zenoh SHM listener quiescence and retained-sample ownership.
#![cfg(feature = "zero-copy")]
use async_trait::async_trait;
use serial_test::serial;
use std::{
    future::{poll_fn, Future},
    sync::{Arc, Mutex, Weak},
    task::Poll,
    time::Duration,
};
use tokio::sync::Notify;
#[path = "support/allocating_control.rs"]
mod control;
use control::{Listener, WithControlWire};
use up_rust::{
    PayloadEncoding, ProtobufWire, UCode, UFrameMetadata, UFrameView, UStatus, UTxBuffer,
    UTxLoanSpec, UUri,
};
use up_transport_zenoh::ZenohZeroCopyCore;

type Transport = control::Transport<ProtobufWire>;
type Frame = control::Received<ProtobufWire>;

async fn transport(name: &str, timeout: Duration) -> (Arc<Transport>, UUri) {
    let authority = format!("zenoh-q-{name}-{}", std::process::id());
    let core = ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/1/0"))
        .with_shm_segment_size(1024 * 1024)
        .unwrap()
        .with_listener_quiesce_timeout(timeout)
        .unwrap()
        .build()
        .await
        .unwrap();
    (
        Arc::new(core.with_control_wire(ProtobufWire)),
        UUri::try_from_parts(&authority, 0x0001_4210, 1, 0x9000).unwrap(),
    )
}

async fn send(transport: &Transport, source: &UUri) {
    // Match the existing real-SHM test setup's bounded peer-declaration wait.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let metadata = UFrameMetadata::publish(source.clone())
        .with_payload_encoding(PayloadEncoding::PROTOBUF)
        .build()
        .unwrap();
    let mut buffer = transport
        .loan_validated_tx(UTxLoanSpec::payload(metadata, 4, 1).unwrap())
        .await
        .unwrap();
    buffer.payload_mut().copy_from_slice(b"live");
    transport.send_validated_zero_copy(buffer).await.unwrap();
}

#[derive(Default)]
struct Blocking {
    entered: Notify,
    release: Notify,
    done: Notify,
    retained: Mutex<Option<Frame>>,
}

#[async_trait]
impl Listener<Frame> for Blocking {
    async fn on_frame(&self, frame: Frame) {
        *self.retained.lock().unwrap() = Some(frame);
        self.entered.notify_one();
        self.release.notified().await;
        assert_eq!(
            self.retained
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .try_contiguous_payload(),
            Some(b"live".as_slice())
        );
        self.done.notify_one();
    }
}

async fn check_quiescence(timed_out: bool) {
    let timeout = if timed_out {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(5)
    };
    let (transport, source) =
        transport(if timed_out { "timeout" } else { "normal" }, timeout).await;
    let listener = Arc::new(Blocking::default());
    let registration: Arc<dyn Listener<Frame>> = listener.clone();
    transport
        .register_validated_zero_copy_listener(&source, None, registration.clone())
        .await
        .unwrap();
    send(&transport, &source).await;
    tokio::time::timeout(Duration::from_secs(5), listener.entered.notified())
        .await
        .unwrap();
    {
        let mut unregister = std::pin::pin!(transport.unregister_validated_zero_copy_listener(
            &source,
            None,
            registration
        ));
        if timed_out {
            assert_eq!(
                unregister.await.unwrap_err().code(),
                UCode::DeadlineExceeded
            );
            assert_eq!(
                listener
                    .retained
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .try_contiguous_payload(),
                Some(b"live".as_slice())
            );
            listener.release.notify_one();
        } else {
            assert!(poll_fn(|cx| Poll::Ready(unregister.as_mut().poll(cx)))
                .await
                .is_pending());
            listener.release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), unregister)
                .await
                .unwrap()
                .unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(5), listener.done.notified())
        .await
        .unwrap();
    let frame = listener.retained.lock().unwrap().take().unwrap();
    drop(transport);
    assert_eq!(frame.try_contiguous_payload(), Some(b"live".as_slice()));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn shm_unregister_waits_for_entered_callback() {
    check_quiescence(false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn shm_quiescence_timeout_does_not_revoke_sample() {
    check_quiescence(true).await;
}

struct SelfRemoving {
    own: Weak<Self>,
    transport: Weak<Transport>,
    source: UUri,
    result: Mutex<Option<tokio::sync::oneshot::Sender<Result<(), UStatus>>>>,
}

#[async_trait]
impl Listener<Frame> for SelfRemoving {
    async fn on_frame(&self, frame: Frame) {
        let listener: Arc<dyn Listener<Frame>> = self.own.upgrade().unwrap();
        let result = self
            .transport
            .upgrade()
            .unwrap()
            .unregister_validated_zero_copy_listener(&self.source, None, listener)
            .await;
        assert_eq!(frame.try_contiguous_payload(), Some(b"live".as_slice()));
        self.result
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(result)
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn shm_callback_unregisters_itself_without_self_wait() {
    let (transport, source) = transport("self", Duration::from_secs(5)).await;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let listener = Arc::new_cyclic(|own| SelfRemoving {
        own: own.clone(),
        transport: Arc::downgrade(&transport),
        source: source.clone(),
        result: Mutex::new(Some(sender)),
    });
    transport
        .register_validated_zero_copy_listener(&source, None, listener)
        .await
        .unwrap();
    send(&transport, &source).await;
    tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
