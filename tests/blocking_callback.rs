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

use async_trait::async_trait;
use bytes::{Buf, BufMut};
use tokio::{
    sync::mpsc::Sender,
    time::{sleep, Duration},
};
use up_rust::{UMessageBuilder, UOwnedFrame, UOwnedListener, UOwnedTransport, UUri, UUID};

struct DelayListener(Sender<UUID>);

#[async_trait]
impl UOwnedListener for DelayListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        let msg_id = frame.metadata().attributes().id().clone();
        let mut payload = frame.payload_bytes();
        if payload.len() >= 4 {
            let delay_millis = payload.get_u32();
            if delay_millis > 0 {
                sleep(Duration::from_millis(u64::from(delay_millis))).await;
            }
        }
        self.0
            .send(msg_id)
            .await
            .expect("failed to acknowledge received frame");
    }
}

// Checks that a slow user callback does not block delivery of later messages.
#[tokio::test(flavor = "multi_thread")]
async fn blocking_user_callback_does_not_block_frame_reception() {
    test_lib::before_test();

    let topic = UUri::try_from_parts("vehicle", 0x0aa0, 1, 0x8500).expect("invalid topic");
    let (tx, mut rx) = tokio::sync::mpsc::channel(5);
    let transport = test_lib::create_up_transport_zenoh(topic.authority_name().as_str(), None)
        .await
        .expect("failed to create transport");
    transport
        .register_owned_listener(&topic, None, std::sync::Arc::new(DelayListener(tx)))
        .await
        .expect("failed to register listener");

    let mut buf = vec![];
    buf.put_u32(1000);
    let delayed_id = UUID::build();
    let delayed_frame = UMessageBuilder::publish(topic.clone())
        .with_message_id(delayed_id.clone())
        .build_with_raw_payload(buf)
        .expect("failed to create delayed frame");
    transport
        .send_owned(delayed_frame)
        .await
        .expect("failed to send delayed frame");

    let immediate_id = UUID::build();
    let immediate_frame = UMessageBuilder::publish(topic)
        .with_message_id(immediate_id.clone())
        .build()
        .expect("failed to create immediate frame");
    transport
        .send_owned(immediate_frame)
        .await
        .expect("failed to send immediate frame");

    let received = tokio::time::timeout(Duration::from_secs(5), async {
        let first = rx.recv().await;
        let second = rx.recv().await;
        (first, second)
    })
    .await
    .expect("did not receive frame UUIDs in time");

    assert_eq!(received.0, Some(immediate_id));
    assert_eq!(received.1, Some(delayed_id));
}
