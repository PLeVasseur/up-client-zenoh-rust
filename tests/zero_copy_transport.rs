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
    payload::{PayloadLayout, PlacementDefault, RawBytes, StableContainerPayload},
    test_util::zero_copy_conformance,
    zero_copy::{
        PayloadLoanProvenance, UFrameView, ULoanedContiguousZeroCopyRxFrame, UTxBuffer,
        UTxLoanSpec, UZeroCopyListener, UZeroCopyPayloadCopyExt, UZeroCopyTransport,
        UZeroCopyTransportExt, UZeroCopyUninitTransportExt,
    },
    UAttributes, UCode, UFrameMetadata, UMessageType, UOwnedFrame, UOwnedTransport, UUri, UUID,
};
use up_transport_zenoh::ZenohRxFrame;

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    PlacementDefault,
    up_rust::StablePayload,
    up_rust::ByteBackedStablePayload,
)]
#[stable_payload(type_name = "example.vehicle.VehiclePose")]
struct VehiclePose {
    x: u64,
    y: u64,
}

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    up_rust::StablePayload,
    up_rust::ByteBackedStablePayload,
    up_rust::StablePayloadInit,
)]
#[stable_payload(type_name = "org.eclipse.uprotocol.transport.example.NoZeroSensorHeader")]
struct NoZeroSensorHeader {
    case_id: u32,
    sequence: u32,
    logical_payload_len: u32,
}

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    up_rust::StablePayload,
    up_rust::ByteBackedStablePayload,
    up_rust::StablePayloadInit,
)]
#[stable_payload(type_name = "org.eclipse.uprotocol.transport.example.NoZeroSensorFrame")]
struct NoZeroSensorFrame {
    header: NoZeroSensorHeader,
    checksum: u32,
    payload: [u8; 4096],
}

struct ZeroCopyFrameSender(mpsc::UnboundedSender<UOwnedFrame>);

#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for ZeroCopyFrameSender {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        self.0
            .send(
                UOwnedFrame::try_with_payload(
                    frame.metadata().clone(),
                    frame
                        .try_payload_to_vec()
                        .expect("zero-copy payload slices should match payload_len"),
                )
                .expect("zero-copy frame should be valid"),
            )
            .expect("zero-copy receive channel should be open");
    }
}

struct StablePoseSender(mpsc::UnboundedSender<VehiclePose>);

#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for StablePoseSender {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        assert_eq!(
            frame.metadata().encoding(),
            Some(&StableContainerPayload::<VehiclePose>::encoding())
        );
        zero_copy_conformance::verify_loaned_rx_payload_layout_for(
            &frame,
            std::mem::size_of::<VehiclePose>(),
            std::mem::align_of::<VehiclePose>(),
        )
        .expect("stable-container payload should satisfy loaned layout");
        assert_eq!(
            frame
                .payload_loan_provenance()
                .expect("stable-container payload should report loan provenance"),
            PayloadLoanProvenance::SharedMemory
        );
        let pose = frame
            .borrow_stable_payload::<VehiclePose>()
            .expect("stable-container payload should be SHM-backed and typed");
        self.0
            .send(*pose)
            .expect("stable pose receive channel should be open");
    }
}

struct NoZeroSensorFrameSender(mpsc::UnboundedSender<(u32, u32, u32, u32, u8, u8)>);

#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for NoZeroSensorFrameSender {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        assert_eq!(
            frame.metadata().encoding(),
            Some(&StableContainerPayload::<NoZeroSensorFrame>::encoding())
        );
        zero_copy_conformance::verify_loaned_rx_payload_layout_for(
            &frame,
            std::mem::size_of::<NoZeroSensorFrame>(),
            std::mem::align_of::<NoZeroSensorFrame>(),
        )
        .expect("stable-container payload should satisfy loaned layout");
        assert_eq!(
            frame
                .payload_loan_provenance()
                .expect("stable-container payload should report loan provenance"),
            PayloadLoanProvenance::SharedMemory
        );
        let frame = frame
            .borrow_stable_payload::<NoZeroSensorFrame>()
            .expect("stable-container payload should be SHM-backed and typed");
        self.0
            .send((
                frame.header.case_id,
                frame.header.sequence,
                frame.header.logical_payload_len,
                frame.checksum,
                frame.payload[0],
                frame.payload[4095],
            ))
            .expect("no-zero stable frame receive channel should be open");
    }
}

fn topic(authority: &str, resource: u16) -> UUri {
    UUri::try_from_parts(authority, 0x4210, 1, resource).expect("valid topic")
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_stable_container_rejects_wrong_metadata_from_shm(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-wrong-stable-metadata-{}", std::process::id());
    let transport = Arc::new(test_lib::create_up_transport_zenoh(&authority, None).await?);
    let source = topic(&authority, 0x9305);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = [0_u8; std::mem::size_of::<VehiclePose>()];
    transport
        .send_serialized_zero_copy::<RawBytes, _>(
            UFrameMetadata::try_publish(source)?,
            &payload.as_slice(),
        )
        .await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), receive_task).await???;
    assert_eq!(
        frame.payload_loan_provenance()?,
        PayloadLoanProvenance::SharedMemory
    );
    assert!(frame.borrow_stable_payload::<VehiclePose>().is_err());
    Ok(())
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
async fn zero_copy_loan_tx_allocates_shm_payload() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-align-{}", std::process::id());
    let transport = test_lib::create_up_transport_zenoh(&authority, None).await?;
    let source = topic(&authority, 0x9300);
    let mut loan = transport
        .loan_tx(UTxLoanSpec::payload(
            UFrameMetadata::try_publish(source)?.with_encoding(RawBytes::encoding()),
            PayloadLayout::new(8, 1)?,
        )?)
        .await?;

    loan.payload_mut().copy_from_slice(b"shm-test");
    assert_eq!(loan.payload(), b"shm-test");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_loan_spec_rejects_payload_without_encoding(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-missing-encoding-{}", std::process::id());
    let source = topic(&authority, 0x9306);

    let result = UTxLoanSpec::payload(
        UFrameMetadata::try_publish(source)?,
        PayloadLayout::new(1, 1)?,
    );

    match result {
        Ok(_) => panic!("payload bytes without encoding must be rejected"),
        Err(err) => assert_eq!(err.get_code(), UCode::INVALID_ARGUMENT),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_preserves_present_empty_payload() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-present-empty-{}", std::process::id());
    let transport = Arc::new(test_lib::create_up_transport_zenoh(&authority, None).await?);
    let source = topic(&authority, 0x9307);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let loan = transport
        .loan_tx(UTxLoanSpec::present_empty_payload(
            UFrameMetadata::try_publish(source)?.with_encoding(RawBytes::encoding()),
        )?)
        .await?;
    transport.send_zero_copy(loan).await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), receive_task).await???;
    assert!(frame.has_payload());
    assert_eq!(frame.payload_len(), 0);
    assert_eq!(frame.metadata().encoding(), Some(&RawBytes::encoding()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_preserves_no_payload() -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-no-payload-{}", std::process::id());
    let transport = Arc::new(test_lib::create_up_transport_zenoh(&authority, None).await?);
    let source = topic(&authority, 0x9308);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let loan = transport
        .loan_tx(UTxLoanSpec::no_payload(UFrameMetadata::try_publish(
            source,
        )?)?)
        .await?;
    transport.send_zero_copy(loan).await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), receive_task).await???;
    assert!(!frame.has_payload());
    assert_eq!(frame.payload_len(), 0);
    assert!(frame.metadata().encoding().is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_stable_container_payload_borrows_from_shm(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-stable-{}", std::process::id());
    let transport = test_lib::create_up_transport_zenoh(&authority, None).await?;
    let source = topic(&authority, 0x9303);
    let (tx, mut rx) = mpsc::unbounded_channel();

    transport
        .register_zero_copy_listener(&source, None, Arc::new(StablePoseSender(tx)))
        .await?;

    transport
        .send_uninit_loaned_payload_as::<StableContainerPayload<VehiclePose>, VehiclePose>(
            UFrameMetadata::try_publish(source)?,
            |slot| Ok(slot.write(VehiclePose { x: 11, y: 22 })),
        )
        .await?;

    let pose = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await?
        .expect("stable-container listener result channel should remain open");

    assert_eq!(pose, VehiclePose { x: 11, y: 22 });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_no_zero_stable_payload_borrows_from_shm(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-no-zero-stable-{}", std::process::id());
    let transport = test_lib::create_up_transport_zenoh(&authority, None).await?;
    let source = topic(&authority, 0x9309);
    let (tx, mut rx) = mpsc::unbounded_channel();

    transport
        .register_zero_copy_listener(&source, None, Arc::new(NoZeroSensorFrameSender(tx)))
        .await?;

    transport
        .send_uninit_stable_payload_as::<NoZeroSensorFrame>(
            UFrameMetadata::try_publish(source)?,
            |frame| {
                frame
                    .header(|header| {
                        header
                            .case_id(1)
                            .sequence(2)
                            .logical_payload_len(4096)
                            .finish()
                    })?
                    .checksum(0x5eed_cafe)
                    .payload_fill(0x5a)
                    .finish()
            },
        )
        .await?;

    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await?
        .expect("stable-container listener result channel should remain open");

    assert_eq!(received, (1, 2, 4096, 0x5eed_cafe, 0x5a, 0x5a));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_stable_container_rejects_owned_payload_as_loaned_rx(
) -> Result<(), Box<dyn std::error::Error>> {
    test_lib::before_test();

    let authority = format!("zenoh-zc-owned-stable-{}", std::process::id());
    let transport = Arc::new(test_lib::create_up_transport_zenoh(&authority, None).await?);
    let source = topic(&authority, 0x9304);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let frame = UOwnedFrame::from_payload_as::<StableContainerPayload<VehiclePose>, VehiclePose>(
        UFrameMetadata::try_publish(source)?,
        &VehiclePose { x: 11, y: 22 },
    )?;
    transport.send_owned(frame).await?;

    let result = tokio::time::timeout(Duration::from_secs(5), receive_task).await??;
    let Err(error) = result else {
        panic!("strict zero-copy receive should reject non-SHM payloads");
    };
    assert_eq!(error.get_code(), UCode::FAILED_PRECONDITION);
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
    let attributes = UAttributes::try_new(id.clone(), source.clone(), None, UMessageType::Publish)?;
    transport
        .send_serialized_zero_copy::<RawBytes, _>(
            UFrameMetadata::try_new(attributes, RawBytes::encoding())?,
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
    let attributes = UAttributes::try_new(
        id.clone(),
        source.clone(),
        Some(sink.clone()),
        UMessageType::Notification,
    )?;
    let payload = b"zenoh zc targeted fanout";
    transport
        .send_serialized_zero_copy::<RawBytes, _>(
            UFrameMetadata::try_new(attributes, RawBytes::encoding())?,
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
