/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
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

#![cfg(feature = "zero-copy")]

mod test_lib;

use std::sync::Arc;

use async_trait::async_trait;
use serial_test::serial;
use tokio::{sync::mpsc, time::Duration};
use up_rust::{
    payload::RawBytes,
    zero_copy::{
        UTxBuffer, UZeroCopyListener, UZeroCopyPayloadCopyExt, UZeroCopyRxFrame,
        UZeroCopyTransport, UZeroCopyTransportExt,
    },
    UAttributes, UFrameMetadata, UMessageType, UOwnedFrame, UUri, UUID,
};
use up_transport_zenoh::ZenohRxFrame;

struct ZeroCopyFrameSender(mpsc::UnboundedSender<UOwnedFrame>);

#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for ZeroCopyFrameSender {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        self.0
            .send(UOwnedFrame::new(
                frame.metadata().clone(),
                frame.payload_to_vec(),
            ))
            .expect("zero-copy receive channel should be open");
    }
}

fn topic(authority: &str, resource: u16) -> UUri {
    UUri::try_from_parts(authority, 0x4210, 1, resource).expect("valid topic")
}

fn authority_wildcard_source(authority: &str) -> UUri {
    UUri::try_from_parts(authority, 0xFFFF_FFFF, 0xFF, 0xFFFF).expect("valid source wildcard")
}

fn authority_notification_sink_wildcard(authority: &str) -> UUri {
    UUri::try_from_parts(authority, 0xFFFF_FFFF, 0xFF, 0).expect("valid sink wildcard")
}

async fn recv_frame(rx: &mut mpsc::UnboundedReceiver<UOwnedFrame>) -> UOwnedFrame {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("receive should not time out")
        .expect("receiver should remain open")
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_reserve_allocates_shm_payload() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-align-{}", std::process::id());
    let transport = test_lib::create_up_transport_zenoh(&authority, None).await?;
    let source = topic(&authority, 0x9300);
    let mut loan = transport
        .reserve(
            UFrameMetadata::publish(source).with_encoding(RawBytes::encoding()),
            8,
            1,
        )
        .await?;

    loan.payload_mut().copy_from_slice(b"shm-test");
    assert_eq!(loan.payload(), b"shm-test");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_publish_fanout_delivers_to_exact_and_source_wildcard_listeners(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-pub-fanout-{}", std::process::id());
    let transport = test_lib::create_up_transport_zenoh(&authority, None).await?;
    let source = topic(&authority, 0x9301);
    let source_wildcard = authority_wildcard_source(&authority);
    let (exact_tx, mut exact_rx) = mpsc::unbounded_channel();
    let (wildcard_tx, mut wildcard_rx) = mpsc::unbounded_channel();

    transport
        .register_zero_copy_listener(&source, None, Arc::new(ZeroCopyFrameSender(exact_tx)))
        .await?;
    transport
        .register_zero_copy_listener(
            &source_wildcard,
            None,
            Arc::new(ZeroCopyFrameSender(wildcard_tx)),
        )
        .await?;

    let payload = b"zenoh zc publish fanout";
    let id = UUID::build();
    let attributes = UAttributes::new(id.clone(), source.clone(), None, UMessageType::Publish);
    transport
        .send_serialized_zero_copy::<RawBytes, _>(
            UFrameMetadata::new(attributes, RawBytes::encoding()),
            &payload.as_slice(),
        )
        .await?;

    let exact = recv_frame(&mut exact_rx).await;
    let wildcard = recv_frame(&mut wildcard_rx).await;

    assert_eq!(exact.metadata().attributes().id(), &id);
    assert_eq!(wildcard.metadata().attributes().id(), &id);
    assert_eq!(exact.payload_bytes(), payload);
    assert_eq!(wildcard.payload_bytes(), payload);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_targeted_fanout_delivers_to_exact_and_sink_wildcard_listeners(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-p2p-fanout-{}", std::process::id());
    let transport = test_lib::create_up_transport_zenoh(&authority, None).await?;
    let source = topic(&authority, 0x9302);
    let sink = UUri::try_from_parts(&authority, 0x4220, 1, 0)?;
    let sink_wildcard = authority_notification_sink_wildcard(&authority);
    let (exact_tx, mut exact_rx) = mpsc::unbounded_channel();
    let (wildcard_tx, mut wildcard_rx) = mpsc::unbounded_channel();

    transport
        .register_zero_copy_listener(
            &source,
            Some(&sink),
            Arc::new(ZeroCopyFrameSender(exact_tx)),
        )
        .await?;
    transport
        .register_zero_copy_listener(
            &source,
            Some(&sink_wildcard),
            Arc::new(ZeroCopyFrameSender(wildcard_tx)),
        )
        .await?;

    let id = UUID::build();
    let attributes = UAttributes::new(
        id.clone(),
        source.clone(),
        Some(sink.clone()),
        UMessageType::Notification,
    );
    let payload = b"zenoh zc targeted fanout";
    transport
        .send_serialized_zero_copy::<RawBytes, _>(
            UFrameMetadata::new(attributes, RawBytes::encoding()),
            &payload.as_slice(),
        )
        .await?;

    let exact = recv_frame(&mut exact_rx).await;
    let wildcard = recv_frame(&mut wildcard_rx).await;

    assert_eq!(exact.metadata().attributes().id(), &id);
    assert_eq!(wildcard.metadata().attributes().id(), &id);
    assert_eq!(exact.metadata().attributes().sink(), Some(&sink));
    assert_eq!(wildcard.metadata().attributes().sink(), Some(&sink));
    assert_eq!(exact.payload_bytes(), payload);
    assert_eq!(wildcard.payload_bytes(), payload);
    Ok(())
}
