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

use std::{
    io::Read,
    sync::{Arc, Once},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;
use up_rust_zc::{
    try_project_umessage_to_frame_metadata, PayloadLoanProvenance, UFrameView as _,
    ULoanedContiguousZeroCopyRxFrame as _, UMessageBuilder, UPayloadFormat, UTxBuffer as _,
    UTxLoanSpec, UUri, UZeroCopyListener, UZeroCopyTransport as _,
};
use up_transport_zenoh::{UPTransportZenoh, ZenohRxFrame};

type TestError = Box<dyn std::error::Error + Send + Sync>;

static INIT: Once = Once::new();

fn before_test() {
    INIT.call_once(UPTransportZenoh::try_init_log_from_env);
}

fn topic(authority: &str, resource: u16) -> UUri {
    UUri::try_from_parts(authority, 0x4210, 1, resource).expect("topic")
}

fn source_wildcard(authority: &str) -> UUri {
    UUri::try_from_parts(authority, 0xFFFF_FFFF, 0xFF, 0xFFFF).expect("wildcard")
}

fn payload_metadata(source: UUri, len: usize) -> up_rust_zc::UFrameMetadata {
    let message = UMessageBuilder::publish(source)
        .build_with_payload(Bytes::from(vec![0_u8; len]), UPayloadFormat::Raw)
        .expect("message");
    try_project_umessage_to_frame_metadata(&message).expect("metadata")
}

async fn test_transport(authority: &str) -> UPTransportZenoh {
    UPTransportZenoh::builder(authority)
        .expect("builder")
        .with_config(zenoh::Config::default())
        .with_shm_segment_size(1024 * 1024)
        .expect("shm segment size")
        .build()
        .await
        .expect("transport")
}

struct PayloadSender(mpsc::UnboundedSender<Vec<u8>>);

#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for PayloadSender {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        let mut payload = Vec::new();
        frame
            .payload_reader()
            .read_to_end(&mut payload)
            .expect("payload reader should succeed");
        self.0.send(payload).expect("receiver should be open");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn receive_zero_copy_returns_shm_payload_lease() -> Result<(), TestError> {
    before_test();

    let authority = format!("zenoh-zc-rx-it-{}", std::process::id());
    let transport = Arc::new(test_transport(&authority).await);
    let source = topic(&authority, 0x9300);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = b"rx-shm";
    let metadata = payload_metadata(source, payload.len());
    let mut buffer = transport
        .loan_tx(UTxLoanSpec::payload(metadata.clone(), payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    transport.send_zero_copy(buffer).await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), receive_task).await???;
    let mut observed = Vec::new();
    frame.payload_reader().read_to_end(&mut observed)?;

    assert_eq!(frame.metadata(), &metadata);
    assert_eq!(observed, payload);
    assert_eq!(frame.try_contiguous_payload(), Some(payload.as_slice()));
    assert_eq!(
        frame.payload_loan_provenance()?,
        PayloadLoanProvenance::OpaqueTransportLoan
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn zero_copy_listener_fanout_delivers_rx_leases() -> Result<(), TestError> {
    before_test();

    let authority = format!("zenoh-zc-listener-it-{}", std::process::id());
    let transport = Arc::new(test_transport(&authority).await);
    let source = topic(&authority, 0x9301);
    let wildcard = source_wildcard(&authority);
    let (exact_tx, mut exact_rx) = mpsc::unbounded_channel();
    let (wildcard_tx, mut wildcard_rx) = mpsc::unbounded_channel();

    transport
        .register_zero_copy_listener(&source, None, Arc::new(PayloadSender(exact_tx)))
        .await?;
    transport
        .register_zero_copy_listener(&wildcard, None, Arc::new(PayloadSender(wildcard_tx)))
        .await?;

    let payload = b"fanout";
    let metadata = payload_metadata(source, payload.len());
    let mut buffer = transport
        .loan_tx(UTxLoanSpec::payload(metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    transport.send_zero_copy(buffer).await?;

    let exact = tokio::time::timeout(Duration::from_secs(5), exact_rx.recv())
        .await?
        .expect("exact listener should receive");
    let wildcard = tokio::time::timeout(Duration::from_secs(5), wildcard_rx.recv())
        .await?
        .expect("wildcard listener should receive");

    assert_eq!(exact, payload);
    assert_eq!(wildcard, payload);
    Ok(())
}
