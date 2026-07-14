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

use std::{io::Read, marker::PhantomData, sync::Arc, time::Duration};

use async_trait::async_trait;
use serial_test::serial;
use tokio::sync::mpsc;
use up_rust::selected_wire_user_api::UWireRx;
use up_rust::wire_implementer_api::{
    NativePrefixFrameMetadataCodec, ProtobufWire, UProtocolNativeWire, UWire, UWireMetadataCodec,
    WireIdentity, NATIVE_EXPLICIT_PAYLOAD_FAMILY_ID, PROTOBUF_WIRE_ID,
    UFRAME_FIELDS_METADATA_LAYOUT_ID,
};
use up_rust::{
    PayloadEncoding, PayloadFormat, PayloadLoanProvenance, UCode, UFrameMetadata, UFrameView,
    ULoanedContiguousZeroCopyRxFrame, UTxBuffer, UTxLoanSpec, UUninitTxBuffer, UUri,
    UZeroCopyListener, UZeroCopyTransport, UZeroCopyUninitTransport,
};
use up_transport_zenoh::{zenoh_config, ZenohRxFrame, ZenohZeroCopyCore};
use up_wire_xcdrv2::{XcdrV2Wire, VEHICLE_SIGNAL_V1_GOLDEN_BYTES};
use zenoh::bytes::ZBytes;

type TestError = Box<dyn std::error::Error + Send + Sync>;

fn topic() -> UUri {
    UUri::try_from_parts("vehicle", 0x4210, 0x01, 0x9000).expect("topic URI")
}

fn topic_for(authority: &str, resource_id: u16) -> UUri {
    UUri::try_from_parts(authority, 0x4210, 0x01, resource_id).expect("topic URI")
}

fn source_wildcard(authority: &str) -> UUri {
    UUri::try_from_parts(authority, 0xFFFF_FFFF, 0xFF, 0xFFFF).expect("wildcard URI")
}

fn sink(authority: &str, resource_id: u16) -> UUri {
    UUri::try_from_parts(authority, 0x4220, 0x01, resource_id).expect("sink URI")
}

fn metadata(source: UUri, payload_encoding: Option<PayloadEncoding>) -> UFrameMetadata {
    let mut builder = UFrameMetadata::publish(source);
    if let Some(payload_encoding) = payload_encoding {
        builder = builder.with_payload_encoding(payload_encoding);
    }
    builder.build().expect("metadata")
}

fn notification_metadata(
    source: UUri,
    sink: UUri,
    payload_encoding: Option<PayloadEncoding>,
) -> UFrameMetadata {
    let mut builder = UFrameMetadata::notification(source, sink);
    if let Some(payload_encoding) = payload_encoding {
        builder = builder.with_payload_encoding(payload_encoding);
    }
    builder.build().expect("metadata")
}

async fn test_core(authority: &str) -> ZenohZeroCopyCore {
    ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/0"))
        .with_config(zenoh_config::Config::default())
        .with_shm_segment_size(1024 * 1024)
        .expect("shm segment size")
        .build()
        .await
        .expect("transport")
}

// Zenoh subscriber declarations are local futures; peer matching is propagated
// asynchronously by Zenoh. Keep one bounded wait point so tests do not race the
// first publication while still failing quickly on real delivery problems.
async fn allow_subscriber_matching() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn assert_no_payload_received(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>, message: &str) {
    if let Ok(Some(payload)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        panic!("{message}: received unexpected payload {payload:?}");
    }
}

fn zenoh_key(source: &UUri, sink: Option<&UUri>) -> String {
    fn part(uri: &UUri) -> String {
        let ue_id = if uri.has_wildcard_entity_type() || uri.has_wildcard_entity_instance() {
            "*".to_string()
        } else {
            format!(
                "{:X}",
                (u32::from(uri.uentity_instance_id()) << 16) | u32::from(uri.uentity_type_id())
            )
        };
        let version = if uri.has_wildcard_version() {
            "*".to_string()
        } else {
            format!("{:X}", uri.uentity_major_version())
        };
        let resource = if uri.has_wildcard_resource_id() {
            "*".to_string()
        } else {
            format!("{:X}", uri.resource_id())
        };
        format!("{}/{ue_id}/{version}/{resource}", uri.authority_name())
    }

    let destination = sink.map_or_else(|| "{}/{}/{}/{}".to_string(), part);
    format!("up/{}/{destination}", part(source))
}

async fn publish_raw_zenoh(
    source: &UUri,
    sink: Option<&UUri>,
    attachment: Vec<u8>,
    payload: &[u8],
) -> Result<(), TestError> {
    let session = zenoh::open(zenoh_config::Config::default()).await?;
    allow_subscriber_matching().await;
    session
        .put(zenoh_key(source, sink), ZBytes::from(payload.to_vec()))
        .attachment(ZBytes::from(attachment))
        .await?;
    allow_subscriber_matching().await;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProtobufWireWithNativePayloadFamily;

impl UWire for ProtobufWireWithNativePayloadFamily {
    const WIRE_ID: WireIdentity = PROTOBUF_WIRE_ID;
    const PAYLOAD_FAMILY_ID: WireIdentity = NATIVE_EXPLICIT_PAYLOAD_FAMILY_ID;
    const METADATA_LAYOUT_ID: WireIdentity = UFRAME_FIELDS_METADATA_LAYOUT_ID;
    const FORMAT_VERSION: u16 = ProtobufWire::FORMAT_VERSION;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zenoh_zero_copy_loan_uses_shm_and_selected_wire_attachment() -> Result<(), TestError> {
    assert_zero_copy_prepared_metadata::<UProtocolNativeWire>(None, &[]).await?;
    assert_zero_copy_prepared_metadata::<ProtobufWire>(
        Some(PayloadEncoding::PROTOBUF),
        b"0123456789abcdef",
    )
    .await?;
    assert_zero_copy_prepared_metadata::<XcdrV2Wire>(
        Some(XcdrV2Wire::encoding()),
        &VEHICLE_SIGNAL_V1_GOLDEN_BYTES,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_builder_rejects_zero_shm_segment_size() -> Result<(), TestError> {
    let Err(error) =
        ZenohZeroCopyCore::builder("//builder-invalid/4210/1/0").with_shm_segment_size(0)
    else {
        panic!("zero segment size should be rejected");
    };
    assert_eq!(error.get_code(), UCode::InvalidArgument);
    Ok(())
}

async fn assert_zero_copy_prepared_metadata<W>(
    payload_encoding: Option<PayloadEncoding>,
    payload: &[u8],
) -> Result<(), TestError>
where
    W: UWire + Default + Send + Sync + 'static,
{
    let authority = format!("zenoh-zcs-loan-{}-{}", std::process::id(), payload.len());
    let transport = test_core(&authority).await.with_selected_wire(W::default());

    let metadata = metadata(topic(), payload_encoding);
    let spec = if payload.is_empty() {
        UTxLoanSpec::no_payload(metadata.clone())?
    } else {
        UTxLoanSpec::payload(metadata.clone(), payload.len(), 8)?
    };
    let mut tx = transport.loan_tx(spec).await?;

    assert_eq!(tx.payload().len(), payload.len());
    if !payload.is_empty() {
        assert_eq!((tx.payload().as_ptr() as usize) % 8, 0);
        tx.payload_mut().copy_from_slice(payload);
    }

    let attachment = tx.attachment_bytes();
    assert_eq!(
        NativePrefixFrameMetadataCodec
            .decode_frame_metadata(W::metadata_context(), &attachment)
            .expect("decode metadata"),
        metadata
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn receive_zero_copy_returns_shm_payload_lease() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-rx-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x9300);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    allow_subscriber_matching().await;

    let payload = b"rx-shm";
    let metadata = metadata(source, Some(PayloadEncoding::PROTOBUF));
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
#[serial]
async fn zero_copy_listener_fanout_delivers_rx_leases() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-listener-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x9301);
    let wildcard = source_wildcard(&authority);
    let (exact_tx, mut exact_rx) = mpsc::unbounded_channel();
    let (wildcard_tx, mut wildcard_rx) = mpsc::unbounded_channel();

    transport
        .register_zero_copy_listener(
            &source,
            None,
            Arc::new(PayloadSender::<ProtobufWire>::new(exact_tx)),
        )
        .await?;
    transport
        .register_zero_copy_listener(
            &wildcard,
            None,
            Arc::new(PayloadSender::<ProtobufWire>::new(wildcard_tx)),
        )
        .await?;
    allow_subscriber_matching().await;

    let payload = b"fanout";
    let second_metadata = metadata(source, Some(PayloadEncoding::PROTOBUF));
    let mut buffer = transport
        .loan_tx(UTxLoanSpec::payload(second_metadata, payload.len(), 1)?)
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

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_unregister_listener_removes_exact_and_wildcard_listeners(
) -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-unregister-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x9306);
    let wildcard = source_wildcard(&authority);
    let (exact_tx, mut exact_rx) = mpsc::unbounded_channel();
    let (wildcard_tx, mut wildcard_rx) = mpsc::unbounded_channel();
    let exact_listener = Arc::new(PayloadSender::<ProtobufWire>::new(exact_tx));
    let wildcard_listener = Arc::new(PayloadSender::<ProtobufWire>::new(wildcard_tx));

    transport
        .register_zero_copy_listener(&source, None, exact_listener.clone())
        .await?;
    transport
        .register_zero_copy_listener(&wildcard, None, wildcard_listener.clone())
        .await?;
    allow_subscriber_matching().await;

    transport
        .unregister_zero_copy_listener(&source, None, exact_listener)
        .await?;
    allow_subscriber_matching().await;
    let payload = b"wildcard-only";
    let first_metadata = metadata(source.clone(), Some(PayloadEncoding::PROTOBUF));
    let mut buffer = transport
        .loan_tx(UTxLoanSpec::payload(first_metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    transport.send_zero_copy(buffer).await?;

    assert_no_payload_received(
        &mut exact_rx,
        "unregistered exact listener should not receive",
    )
    .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), wildcard_rx.recv())
            .await?
            .expect("wildcard listener should receive"),
        payload
    );

    transport
        .unregister_zero_copy_listener(&wildcard, None, wildcard_listener)
        .await?;
    allow_subscriber_matching().await;
    let payload = b"no-listeners";
    let second_metadata = metadata(source, Some(PayloadEncoding::PROTOBUF));
    let mut buffer = transport
        .loan_tx(UTxLoanSpec::payload(second_metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    transport.send_zero_copy(buffer).await?;

    assert_no_payload_received(
        &mut wildcard_rx,
        "unregistered wildcard listener should not receive",
    )
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_uninit_transmit_uses_selected_wire_metadata() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-uninit-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x9302);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    allow_subscriber_matching().await;

    let payload = b"uninit";
    let metadata = metadata(source, Some(PayloadEncoding::PROTOBUF));
    let mut buffer = transport
        .loan_uninit_tx(UTxLoanSpec::payload(metadata.clone(), payload.len(), 1)?)
        .await?;
    for (slot, byte) in buffer.payload_uninit_mut().iter_mut().zip(payload) {
        slot.write(*byte);
    }
    // SAFETY: every payload byte returned by the uninit loan was written above.
    let buffer = unsafe { buffer.assume_payload_init() };
    transport.send_zero_copy(buffer).await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), receive_task).await???;
    assert_eq!(frame.metadata(), &metadata);
    assert_eq!(frame.try_contiguous_payload(), Some(payload.as_slice()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_wrong_wire_metadata_is_rejected_before_pull_receive_exposes_frame(
) -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-wrong-wire-{}", std::process::id());
    let sender = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let receiver = Arc::new(
        test_core(&authority)
            .await
            .with_selected_wire(UProtocolNativeWire),
    );
    let source = topic_for(&authority, 0x9303);
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    allow_subscriber_matching().await;

    let payload = b"wrong";
    let metadata = metadata(source, Some(PayloadEncoding::PROTOBUF));
    let mut buffer = sender
        .loan_tx(UTxLoanSpec::payload(metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    sender.send_zero_copy(buffer).await?;

    let error = tokio::time::timeout(Duration::from_secs(5), receive_task)
        .await??
        .err()
        .expect("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_payload_family_mismatch_is_rejected_before_pull_receive_exposes_frame(
) -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-family-mismatch-{}", std::process::id());
    let sender = Arc::new(
        test_core(&authority)
            .await
            .with_selected_wire(ProtobufWireWithNativePayloadFamily),
    );
    let receiver = test_core(&authority).await.with_selected_wire(ProtobufWire);
    let source = topic_for(&authority, 0x9307);
    let metadata = metadata(source.clone(), Some(PayloadEncoding::PROTOBUF));
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    allow_subscriber_matching().await;

    let payload = b"family-mismatch";
    let mut buffer = sender
        .loan_tx(UTxLoanSpec::payload(metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    sender.send_zero_copy(buffer).await?;

    let error = tokio::time::timeout(Duration::from_secs(5), receive_task)
        .await??
        .err()
        .expect("payload-family mismatch rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_non_shm_payload_is_rejected_before_pull_receive_exposes_frame(
) -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-non-shm-{}", std::process::id());
    let receiver = test_core(&authority).await.with_selected_wire(ProtobufWire);
    let source = topic_for(&authority, 0x9308);
    let metadata = metadata(source.clone(), Some(PayloadEncoding::PROTOBUF));
    let attachment = NativePrefixFrameMetadataCodec
        .encode_frame_metadata(ProtobufWire::metadata_context(), &metadata)?;
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    allow_subscriber_matching().await;

    publish_raw_zenoh(&source, None, attachment, b"not-shm").await?;

    let error = tokio::time::timeout(Duration::from_secs(5), receive_task)
        .await??
        .err()
        .expect("non-SHM payload rejected");
    assert_eq!(error.get_code(), UCode::FailedPrecondition);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_malformed_listener_metadata_is_not_delivered() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-listener-drop-{}", std::process::id());
    let sender = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let receiver = Arc::new(
        test_core(&authority)
            .await
            .with_selected_wire(UProtocolNativeWire),
    );
    let source = topic_for(&authority, 0x9304);
    let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();

    receiver
        .register_zero_copy_listener(
            &source,
            None,
            Arc::new(PayloadSender::<UProtocolNativeWire>::new(dropped_tx)),
        )
        .await?;
    allow_subscriber_matching().await;

    let payload = b"drop";
    let metadata = metadata(source, Some(PayloadEncoding::PROTOBUF));
    let mut buffer = sender
        .loan_tx(UTxLoanSpec::payload(metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    sender.send_zero_copy(buffer).await?;

    assert!(
        tokio::time::timeout(Duration::from_millis(300), dropped_rx.recv())
            .await
            .is_err(),
        "wrong-wire listener frame should be dropped"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_non_shm_listener_payload_is_not_delivered() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-listener-non-shm-{}", std::process::id());
    let receiver = test_core(&authority).await.with_selected_wire(ProtobufWire);
    let source = topic_for(&authority, 0x9309);
    let (dropped_tx, mut dropped_rx) = mpsc::unbounded_channel();

    receiver
        .register_zero_copy_listener(
            &source,
            None,
            Arc::new(PayloadSender::<ProtobufWire>::new(dropped_tx)),
        )
        .await?;
    allow_subscriber_matching().await;

    let metadata = metadata(source.clone(), Some(PayloadEncoding::PROTOBUF));
    publish_raw_zenoh(
        &source,
        None,
        NativePrefixFrameMetadataCodec
            .encode_frame_metadata(ProtobufWire::metadata_context(), &metadata)?,
        b"not-shm-listener",
    )
    .await?;

    assert!(
        tokio::time::timeout(Duration::from_millis(300), dropped_rx.recv())
            .await
            .is_err(),
        "non-SHM listener frame should be dropped"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_sink_filter_mismatch_is_not_delivered_to_pull_receive() -> Result<(), TestError>
{
    let authority = format!("zenoh-zcs-sink-filter-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x930A);
    let matching_sink = sink(&authority, 0);
    let nonmatching_sink = sink(&format!("{authority}-other"), 0);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_sink = nonmatching_sink.clone();
    let receive_task = tokio::spawn(async move {
        receiver
            .receive_zero_copy(&receive_source, Some(&receive_sink))
            .await
    });
    allow_subscriber_matching().await;

    let payload = b"sink-filter";
    let metadata = notification_metadata(source, matching_sink, Some(PayloadEncoding::PROTOBUF));
    let mut buffer = transport
        .loan_tx(UTxLoanSpec::payload(metadata, payload.len(), 1)?)
        .await?;
    buffer.payload_mut().copy_from_slice(payload);
    transport.send_zero_copy(buffer).await?;

    assert!(
        tokio::time::timeout(Duration::from_millis(300), receive_task)
            .await
            .is_err(),
        "nonmatching sink filter should not receive the frame"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn zero_copy_external_xcdrv2_wrong_wire_metadata_is_rejected() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-xcdr-wrong-wire-{}", std::process::id());
    let sender = Arc::new(test_core(&authority).await.with_selected_wire(XcdrV2Wire));
    let receiver = test_core(&authority).await.with_selected_wire(ProtobufWire);
    let source = topic_for(&authority, 0x9305);
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    allow_subscriber_matching().await;

    let metadata = metadata(source, Some(XcdrV2Wire::encoding()));
    let mut buffer = sender
        .loan_tx(UTxLoanSpec::payload(
            metadata,
            VEHICLE_SIGNAL_V1_GOLDEN_BYTES.len(),
            1,
        )?)
        .await?;
    buffer
        .payload_mut()
        .copy_from_slice(&VEHICLE_SIGNAL_V1_GOLDEN_BYTES);
    sender.send_zero_copy(buffer).await?;

    let error = tokio::time::timeout(Duration::from_secs(5), receive_task)
        .await??
        .err()
        .expect("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
    Ok(())
}

struct PayloadSender<W> {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    _wire: PhantomData<W>,
}

type NativePrefixRx<W> = UWireRx<ZenohRxFrame, W, NativePrefixFrameMetadataCodec>;

impl<W> PayloadSender<W> {
    fn new(sender: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            sender,
            _wire: PhantomData,
        }
    }
}

#[async_trait]
impl<W> UZeroCopyListener<NativePrefixRx<W>> for PayloadSender<W>
where
    W: UWire + Send + Sync + 'static,
{
    async fn on_receive_zero_copy(&self, frame: NativePrefixRx<W>) {
        let mut payload = Vec::new();
        frame
            .payload_reader()
            .read_to_end(&mut payload)
            .expect("payload reader should succeed");
        let _ = self.sender.send(payload);
    }
}
