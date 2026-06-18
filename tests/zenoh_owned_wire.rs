/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
 *
 * SPDX-License-Identifier: Apache-2.0
 ********************************************************************************/

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use serial_test::serial;
use tokio::sync::mpsc;
use up_rust::{
    PayloadEncoding, PayloadFormat, ProtobufWire, UCode, UFrameMetadata, UMessageBuilder,
    UOwnedFrame, UOwnedListener, UOwnedTransport, UPayloadFormat, UProtocolNativeWire, UUri,
    UWireMetadata,
};
use up_transport_zenoh::{zenoh_config, ZenohOwnedCore};
use up_wire_xcdrv2::{XcdrV2Wire, VEHICLE_SIGNAL_V1_GOLDEN_BYTES};
use zenoh::bytes::ZBytes;

type TestError = Box<dyn std::error::Error + Send + Sync>;

fn topic_for(authority: &str, resource_id: u16) -> UUri {
    UUri::try_from_parts(authority, 0x4210, 0x01, resource_id).expect("topic URI")
}

fn source_wildcard(authority: &str) -> UUri {
    UUri::try_from_parts(authority, 0xFFFF_FFFF, 0xFF, 0xFFFF).expect("wildcard URI")
}

fn metadata(source: UUri, payload_encoding: Option<PayloadEncoding>) -> UFrameMetadata {
    let message = UMessageBuilder::publish(source).build().expect("message");
    UFrameMetadata::new(message.attributes().clone(), payload_encoding).expect("metadata")
}

async fn owned_transport<W>(authority: &str) -> Arc<up_rust::UWireTransport<ZenohOwnedCore, W>>
where
    W: up_rust::UWire + Default,
{
    let core = ZenohOwnedCore::new(
        zenoh_config::Config::default(),
        format!("//{authority}/4210/1/0"),
    )
    .await
    .expect("owned core");
    Arc::new(core.with_selected_wire(W::default()))
}

async fn allow_subscriber_matching() {
    tokio::time::sleep(Duration::from_millis(100)).await;
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
    session
        .put(zenoh_key(source, sink), ZBytes::from(payload.to_vec()))
        .attachment(ZBytes::from(attachment))
        .await?;
    Ok(())
}

async fn assert_owned_round_trip<W>(
    authority: &str,
    payload_encoding: PayloadEncoding,
    payload: Bytes,
) -> Result<(), TestError>
where
    W: up_rust::UWire + UWireMetadata + Default + Send + Sync + 'static,
{
    let transport = owned_transport::<W>(authority).await;
    let source = topic_for(authority, 0x9000);
    let rx_transport = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { rx_transport.receive_owned(&receive_source, None).await });
    allow_subscriber_matching().await;

    transport
        .send_owned(UOwnedFrame::with_payload(
            metadata(source, Some(payload_encoding)),
            payload.clone(),
        )?)
        .await?;
    let received = receive_task.await??;
    assert_eq!(received.payload(), Some(&payload));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn owned_core_round_trips_protobuf_and_external_xcdrv2() -> Result<(), TestError> {
    let prefix = format!("zenoh-owned-rt-{}", std::process::id());
    assert_owned_round_trip::<ProtobufWire>(
        &format!("{prefix}-protobuf"),
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
        Bytes::from_static(b"data"),
    )
    .await?;
    assert_owned_round_trip::<XcdrV2Wire>(
        &format!("{prefix}-xcdrv2"),
        XcdrV2Wire::encoding(),
        Bytes::copy_from_slice(&VEHICLE_SIGNAL_V1_GOLDEN_BYTES),
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn owned_core_rejects_wrong_wire_before_pull_receive_exposes_frame() -> Result<(), TestError>
{
    let authority = format!("zenoh-owned-wrong-wire-{}", std::process::id());
    let transport = owned_transport::<UProtocolNativeWire>(&authority).await;
    let source = topic_for(&authority, 0x9001);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_owned(&receive_source, None).await });
    allow_subscriber_matching().await;

    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(
        source.clone(),
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    ))?;
    publish_raw_zenoh(&source, None, wrong_metadata, b"drop").await?;

    let error = receive_task.await?.expect_err("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn owned_core_malformed_listener_metadata_is_not_delivered() -> Result<(), TestError> {
    let authority = format!("zenoh-owned-listener-drop-{}", std::process::id());
    let transport = owned_transport::<UProtocolNativeWire>(&authority).await;
    let source = topic_for(&authority, 0x9002);
    let listener = Arc::new(ChannelOwnedListener::default());
    transport
        .register_owned_listener(&source_wildcard(&authority), None, listener.clone())
        .await?;
    allow_subscriber_matching().await;

    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(
        source.clone(),
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    ))?;
    publish_raw_zenoh(&source, None, wrong_metadata, b"drop").await?;

    if let Ok(Some(payload)) =
        tokio::time::timeout(Duration::from_millis(300), listener.recv()).await
    {
        panic!("received unexpected payload {payload:?}");
    }
    Ok(())
}

struct ChannelOwnedListener {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl Default for ChannelOwnedListener {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: tokio::sync::Mutex::new(rx),
        }
    }
}

impl ChannelOwnedListener {
    async fn recv(&self) -> Option<Vec<u8>> {
        self.rx.lock().await.recv().await
    }
}

#[async_trait]
impl UOwnedListener for ChannelOwnedListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.tx
            .send(frame.payload_bytes().to_vec())
            .expect("listener channel open");
    }
}
