/********************************************************************************
 * Copyright (c) 2024 Contributors to the Eclipse Foundation
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

mod test_lib;

use std::{str::FromStr, sync::Arc};

use async_trait::async_trait;
use tokio::{sync::Notify, time::Duration};
use tracing::info;
use up_rust::{
    UCode, UMessageBuilder, UOwnedFrame, UOwnedListener, UOwnedTransport, UPriority, UStatus, UUri,
    UUID,
};

const MESSAGE_DATA: &str = "Hello World!";

struct MessageHandler(UOwnedFrame, Arc<Notify>);

#[async_trait]
impl UOwnedListener for MessageHandler {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        assert_eq!(self.0, frame);
        self.1.notify_one();
    }
}

struct NotifyListener(tokio::sync::mpsc::Sender<UUID>);

#[async_trait]
impl UOwnedListener for NotifyListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        let _ = self
            .0
            .send(frame.metadata().attributes().id().clone())
            .await;
    }
}

async fn register_listener_and_send(
    authority: &str,
    frame: UOwnedFrame,
    source_filter: &UUri,
    sink_filter: Option<&UUri>,
) -> Result<(), Box<dyn std::error::Error>> {
    let transport = test_lib::create_up_transport_zenoh(authority, None).await?;

    let notify = Arc::new(Notify::new());
    let listener = Arc::new(MessageHandler(frame.clone(), notify.clone()));
    transport
        .register_owned_listener(source_filter, sink_filter, listener)
        .await?;

    info!(
        "sending frame: [id: {}, type: {:?}]",
        frame.metadata().attributes().id().to_hyphenated_string(),
        frame.metadata().attributes().message_type()
    );
    transport.send_owned(frame).await?;
    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .map_err(|_| {
            UStatus::fail_with_code(UCode::DEADLINE_EXCEEDED, "did not receive frame in time")
        })?;
    Ok(())
}

#[test_case::test_case("vehicle1", 12_000, "//vehicle1/10A10B/1/CA5D", "//vehicle1/10A10B/1/CA5D"; "specific source filter")]
#[test_case::test_case("vehicle1", 0, "/D5A/3/9999", "//vehicle1/D5A/3/FFFF"; "wildcard resource filter")]
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn publish_frame_gets_delivered_to_listener(
    authority: &str,
    ttl: u32,
    topic_uri: &str,
    source_filter_uri: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let topic = UUri::from_str(topic_uri)?;
    let source_filter = UUri::from_str(source_filter_uri)?;
    let mut builder = UMessageBuilder::publish(topic)
        .with_priority(UPriority::CS5)
        .with_traceparent("traceparent");
    if ttl > 0 {
        builder = builder.with_ttl(ttl);
    }
    let frame = builder.build_with_raw_payload(MESSAGE_DATA)?;

    register_listener_and_send(authority, frame, &source_filter, None).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn notification_frame_gets_delivered_to_listener() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let source = UUri::from_str("//vehicle1/10A10B/1/CA5D")?;
    let sink = UUri::from_str("//vehicle1/55A1/2/0")?;
    let source_filter = UUri::from_str("//vehicle1/10A10B/1/CA5D")?;
    let sink_filter = UUri::from_str("//vehicle1/FFFFFFFF/FF/0")?;
    let frame = UMessageBuilder::notification(source, sink)
        .with_priority(UPriority::CS2)
        .with_traceparent("traceparent")
        .with_ttl(12_000)
        .build_with_raw_payload(MESSAGE_DATA)?;

    register_listener_and_send("vehicle1", frame, &source_filter, Some(&sink_filter)).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn rpc_request_frame_gets_delivered_to_listener() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let reply_to = UUri::from_str("//vehicle1/10A10B/1/0")?;
    let method_to_invoke = UUri::from_str("//vehicle1/55A1/2/A1")?;
    let source_filter = UUri::from_str("//vehicle1/10A10B/1/0")?;
    let sink_filter = UUri::from_str("//vehicle1/55A1/2/A1")?;
    let frame = UMessageBuilder::request(method_to_invoke, reply_to, 5_000)
        .with_priority(UPriority::CS5)
        .with_token("token")
        .with_traceparent("traceparent")
        .with_permission_level(15)
        .build_with_raw_payload(MESSAGE_DATA)?;

    register_listener_and_send("vehicle1", frame, &source_filter, Some(&sink_filter)).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn rpc_response_frame_gets_delivered_to_listener() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let reply_to = UUri::from_str("//vehicle1/10A10B/1/0")?;
    let invoked_method = UUri::from_str("//vehicle1/55A1/2/A1")?;
    let source_filter = UUri::from_str("//vehicle1/55A1/2/A1")?;
    let sink_filter = UUri::from_str("//vehicle1/10A10B/1/0")?;
    let frame = UMessageBuilder::response(reply_to, UUID::build(), invoked_method)
        .with_priority(UPriority::CS5)
        .with_traceparent("traceparent")
        .with_commstatus(UCode::NOT_FOUND)
        .build_with_raw_payload(MESSAGE_DATA)?;

    register_listener_and_send("vehicle1", frame, &source_filter, Some(&sink_filter)).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn expired_rpc_request_frame_is_not_delivered_to_listener() {
    test_lib::before_test();

    let reply_to = UUri::from_str("//vehicle1/10A10B/1/0").expect("invalid URI");
    let method_to_invoke = UUri::from_str("//vehicle1/55A1/2/A1").expect("invalid URI");
    let source_filter = UUri::from_str("//vehicle1/10A10B/1/0").expect("invalid URI");
    let sink_filter = UUri::from_str("//vehicle1/55A1/2/A1").expect("invalid URI");
    let expired_uuid = UUID::from_u64_pair(0x018D_548E_A8E0_7000, 0x8000_0000_0000_0000)
        .expect("valid expired UUID");
    let frame = UMessageBuilder::request(method_to_invoke, reply_to, 5_000)
        .with_message_id(expired_uuid)
        .with_priority(UPriority::CS5)
        .with_token("token")
        .with_traceparent("traceparent")
        .with_permission_level(15)
        .build_with_raw_payload(MESSAGE_DATA)
        .expect("failed to create frame");

    assert!(
        register_listener_and_send("vehicle1", frame, &source_filter, Some(&sink_filter))
            .await
            .is_err_and(|e| {
                e.downcast_ref::<UStatus>()
                    .is_some_and(|status| status.get_code() == UCode::DEADLINE_EXCEEDED)
            })
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn unregister_listener_stops_processing_frames() {
    test_lib::before_test();
    let transport = test_lib::create_up_transport_zenoh("vehicle", None)
        .await
        .expect("failed to create transport");

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let listener = Arc::new(NotifyListener(tx));
    let topic = UUri::from_str("//vehicle/123/1/9000").expect("invalid topic");
    let first_id = UUID::build();
    let second_id = UUID::build();
    let first_frame = UMessageBuilder::publish(topic.clone())
        .with_message_id(first_id.clone())
        .build()
        .expect("failed to create frame");
    let second_frame = UMessageBuilder::publish(topic)
        .with_message_id(second_id.clone())
        .build()
        .expect("failed to create frame");

    transport
        .register_owned_listener(&UUri::any(), None, listener.clone())
        .await
        .expect("failed to register listener");
    transport
        .send_owned(first_frame)
        .await
        .expect("failed to send frame");
    assert!(tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(id) = rx.recv().await {
            if id == first_id {
                return Some(id);
            }
        }
        None
    })
    .await
    .is_ok_and(|received| received.is_some()));
    while tokio::time::timeout(Duration::from_millis(50), rx.recv())
        .await
        .is_ok_and(|received| received.is_some())
    {}

    transport
        .unregister_owned_listener(&UUri::any(), None, listener)
        .await
        .expect("failed to unregister listener");
    tokio::time::sleep(Duration::from_millis(100)).await;
    transport
        .send_owned(second_frame)
        .await
        .expect("failed to send frame");
    let received_after_unregister = tokio::time::timeout(Duration::from_millis(500), async {
        while let Some(id) = rx.recv().await {
            if id == second_id {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(!received_after_unregister);
}
