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

use std::{sync::Arc, sync::Mutex as StdMutex};

use async_trait::async_trait;
use bytes::Bytes;
use up_rust::{
    EncodedOwnedFrame, PayloadEncoding, PayloadFormat, ProtobufWire, UCode, UFrameMetadata,
    UFrameView, UMessageBuilder, UOwnedFrame, UOwnedListener, UOwnedTransport, UPayloadFormat,
    UProtocolNativeWire, UUri, UWireMetadata,
};
use up_transport_zenoh::ZenohOwnedCore;
use up_wire_xcdrv2::{XcdrV2Wire, VEHICLE_SIGNAL_V1_GOLDEN_BYTES};

fn topic() -> UUri {
    UUri::try_from_parts("vehicle", 0x4210, 0x01, 0x9000).expect("topic URI")
}

fn metadata(payload_encoding: Option<PayloadEncoding>) -> UFrameMetadata {
    let message = UMessageBuilder::publish(topic()).build().expect("message");
    UFrameMetadata::new(message.attributes().clone(), payload_encoding).expect("metadata")
}

fn source_filter() -> UUri {
    topic()
}

#[tokio::test]
async fn owned_core_carries_prepared_metadata_bytes_for_core_wires() {
    assert_owned_prepared_metadata::<UProtocolNativeWire>(None, None).await;
    assert_owned_prepared_metadata::<ProtobufWire>(
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
        Some(Bytes::from_static(b"data")),
    )
    .await;
}

#[tokio::test]
async fn owned_core_carries_prepared_metadata_bytes_for_external_xcdrv2() {
    assert_owned_prepared_metadata::<XcdrV2Wire>(
        Some(XcdrV2Wire::encoding()),
        Some(Bytes::copy_from_slice(&VEHICLE_SIGNAL_V1_GOLDEN_BYTES)),
    )
    .await;
}

async fn assert_owned_prepared_metadata<W>(
    payload_encoding: Option<PayloadEncoding>,
    payload: Option<Bytes>,
) where
    W: UWireMetadata + Default + Send + Sync + 'static,
{
    let core = ZenohOwnedCore::new();
    let transport = core.clone().with_selected_wire(W::default());
    let frame_metadata = metadata(payload_encoding);
    let frame = if let Some(payload) = payload {
        UOwnedFrame::with_payload(frame_metadata.clone(), payload).expect("owned frame")
    } else {
        UOwnedFrame::without_payload(frame_metadata.clone()).expect("owned frame")
    };

    transport.send_owned(frame).await.expect("send owned");

    let sent = core.last_sent().await.expect("prepared owned frame");
    assert_eq!(
        W::decode_frame_metadata(sent.encoded_metadata()).expect("decode metadata"),
        frame_metadata
    );
}

#[tokio::test]
async fn owned_core_protobuf_payload_bytes_round_trip_through_pull_receive() {
    let core = ZenohOwnedCore::new();
    let metadata = metadata(Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)));
    core.push_encoded_owned(EncodedOwnedFrame::new(
        ProtobufWire::encode_frame_metadata(&metadata).expect("metadata"),
        Some(Bytes::from_static(b"data")),
    ))
    .await;
    let transport = core.with_selected_wire(ProtobufWire);

    let rx = transport
        .receive_owned(&source_filter(), None)
        .await
        .expect("receive");
    assert_eq!(rx.try_contiguous_payload(), Some(&b"data"[..]));
}

#[tokio::test]
async fn owned_core_external_xcdrv2_bytes_round_trip_through_pull_receive() {
    let core = ZenohOwnedCore::new();
    let metadata = metadata(Some(XcdrV2Wire::encoding()));
    core.push_encoded_owned(EncodedOwnedFrame::new(
        XcdrV2Wire::encode_frame_metadata(&metadata).expect("metadata"),
        Some(Bytes::copy_from_slice(&VEHICLE_SIGNAL_V1_GOLDEN_BYTES)),
    ))
    .await;
    let transport = core.with_selected_wire(XcdrV2Wire);

    let rx = transport
        .receive_owned(&source_filter(), None)
        .await
        .expect("receive");
    assert_eq!(
        rx.try_contiguous_payload(),
        Some(&VEHICLE_SIGNAL_V1_GOLDEN_BYTES[..])
    );
}

#[tokio::test]
async fn owned_core_rejects_wrong_wire_before_pull_receive_exposes_frame() {
    let core = ZenohOwnedCore::new();
    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(Some(
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
    )))
    .expect("wrong metadata");
    core.push_encoded_owned(EncodedOwnedFrame::new(
        wrong_metadata,
        Some(Bytes::from_static(b"drop")),
    ))
    .await;
    let transport = core.with_selected_wire(UProtocolNativeWire);

    let result = transport.receive_owned(&source_filter(), None).await;
    let error = result.err().expect("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[tokio::test]
async fn owned_core_malformed_listener_metadata_is_not_delivered() {
    let core = ZenohOwnedCore::new();
    let listener = Arc::new(CountingOwnedListener::default());
    let transport = core.clone().with_selected_wire(UProtocolNativeWire);
    transport
        .register_owned_listener(&source_filter(), None, listener.clone())
        .await
        .expect("register");

    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(Some(
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
    )))
    .expect("wrong metadata");
    core.deliver_encoded_owned(EncodedOwnedFrame::new(
        wrong_metadata,
        Some(Bytes::from_static(b"drop")),
    ))
    .await;

    assert_eq!(listener.payloads(), Vec::<Vec<u8>>::new());
}

#[tokio::test]
async fn owned_core_external_xcdrv2_wrong_wire_metadata_is_rejected_before_pull_receive_exposes_frame(
) {
    let core = ZenohOwnedCore::new();
    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(Some(
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
    )))
    .expect("wrong metadata");
    core.push_encoded_owned(EncodedOwnedFrame::new(
        wrong_metadata,
        Some(Bytes::from_static(b"drop")),
    ))
    .await;
    let transport = core.with_selected_wire(XcdrV2Wire);

    let result = transport.receive_owned(&source_filter(), None).await;
    let error = result.err().expect("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[tokio::test]
async fn owned_core_external_xcdrv2_payload_family_mismatch_is_rejected_before_pull_receive_exposes_frame(
) {
    let core = ZenohOwnedCore::new();
    let mut mismatched = XcdrV2Wire::encode_frame_metadata(&metadata(Some(XcdrV2Wire::encoding())))
        .expect("metadata to corrupt");
    mismatched[15..18].copy_from_slice(&[0x00, 0x02, 0x00]);
    core.push_encoded_owned(EncodedOwnedFrame::new(
        mismatched,
        Some(Bytes::from_static(b"drop")),
    ))
    .await;
    let transport = core.with_selected_wire(XcdrV2Wire);

    let result = transport.receive_owned(&source_filter(), None).await;
    let error = result.err().expect("mismatch rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[tokio::test]
async fn owned_core_wrong_wire_metadata_is_rejected_before_pull_receive_exposes_frame() {
    let core = ZenohOwnedCore::new();
    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(Some(
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
    )))
    .expect("wrong metadata");
    core.push_encoded_owned(EncodedOwnedFrame::new(
        wrong_metadata,
        Some(Bytes::from_static(b"drop")),
    ))
    .await;
    let transport = core.with_selected_wire(UProtocolNativeWire);

    let result = transport.receive_owned(&source_filter(), None).await;
    let error = result.err().expect("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[tokio::test]
async fn owned_core_payload_family_mismatch_is_rejected_before_pull_receive_exposes_frame() {
    let core = ZenohOwnedCore::new();
    let mut mismatched =
        UProtocolNativeWire::encode_frame_metadata(&metadata(None)).expect("metadata to corrupt");
    mismatched[15..18].copy_from_slice(&[0x00, 0x02, 0x00]);
    core.push_encoded_owned(EncodedOwnedFrame::new(
        mismatched,
        Some(Bytes::from_static(b"drop")),
    ))
    .await;
    let transport = core.with_selected_wire(UProtocolNativeWire);

    let result = transport.receive_owned(&source_filter(), None).await;
    let error = result.err().expect("mismatch rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[derive(Default)]
struct CountingOwnedListener {
    payloads: StdMutex<Vec<Vec<u8>>>,
}

impl CountingOwnedListener {
    fn payloads(&self) -> Vec<Vec<u8>> {
        self.payloads.lock().expect("payload lock").clone()
    }
}

#[async_trait]
impl UOwnedListener for CountingOwnedListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.payloads.lock().expect("payload lock").push(
            frame
                .payload()
                .map_or_else(Vec::new, |payload| payload.to_vec()),
        );
    }
}
