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

mod test_lib;

use std::sync::Arc;

use async_trait::async_trait;
use protobuf::well_known_types::wrappers::StringValue;
use serial_test::serial;
use tokio::{sync::mpsc, time::Duration};
use up_rust::{
    ProtobufWire, UAttributes, UCode, UDeserializer, UEncoding, UFrameMetadata, UMessageType,
    UOwnedFrame, UOwnedListener, UOwnedTransport, UOwnedTransportExt, UPriority, USerializer, UUri,
    UWireError, WireFormat, UUID,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestReading {
    sensor_id: u16,
    counter: u32,
}

struct TestReadingWire;

impl WireFormat for TestReadingWire {
    fn name() -> &'static str {
        "test-reading-v1"
    }

    fn encoding() -> UEncoding {
        UEncoding::new(
            Self::name(),
            "application/x.up-test-reading",
            Some("urn:uprotocol:test:reading:v1"),
        )
    }
}

impl USerializer<TestReadingWire> for TestReading {
    fn encoded_len(&self) -> usize {
        6
    }

    fn serialize_into(&self, dst: &mut [u8]) -> Result<usize, UWireError> {
        let expected = self.encoded_len();
        let actual = dst.len();
        if actual < expected {
            return Err(UWireError::buffer_too_small(expected, actual));
        }
        let sensor_id = dst
            .get_mut(..2)
            .ok_or_else(|| UWireError::buffer_too_small(expected, actual))?;
        sensor_id.copy_from_slice(&self.sensor_id.to_be_bytes());
        let counter = dst
            .get_mut(2..6)
            .ok_or_else(|| UWireError::buffer_too_small(expected, actual))?;
        counter.copy_from_slice(&self.counter.to_be_bytes());
        Ok(expected)
    }
}

impl<'a> UDeserializer<'a, TestReadingWire> for TestReading {
    fn deserialize_from(src: &'a [u8]) -> Result<Self, UWireError> {
        if src.len() != 6 {
            return Err(UWireError::invalid_payload(format!(
                "expected 6 bytes, got {}",
                src.len()
            )));
        }
        Ok(Self {
            sensor_id: u16::from_be_bytes(
                src.get(..2)
                    .ok_or_else(|| UWireError::invalid_payload("missing sensor_id"))?
                    .try_into()
                    .map_err(|_| UWireError::invalid_payload("invalid sensor_id"))?,
            ),
            counter: u32::from_be_bytes(
                src.get(2..6)
                    .ok_or_else(|| UWireError::invalid_payload("missing counter"))?
                    .try_into()
                    .map_err(|_| UWireError::invalid_payload("invalid counter"))?,
            ),
        })
    }
}

struct FrameSender(mpsc::UnboundedSender<UOwnedFrame>);

#[async_trait]
impl UOwnedListener for FrameSender {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.0.send(frame).expect("failed to send received frame");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn owned_transport_round_trips_custom_wire_format() -> Result<(), Box<dyn std::error::Error>>
{
    test_lib::before_test();

    let topic = UUri::try_from_parts("ownedtest", 0x4210, 1, 0x9001)?;
    let transport = test_lib::create_up_transport_zenoh("ownedtest", None).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();

    transport
        .register_owned_listener(&topic, None, Arc::new(FrameSender(tx)))
        .await?;

    let reading = TestReading {
        sensor_id: 7,
        counter: 42,
    };
    transport
        .send_serialized::<TestReadingWire, _>(UFrameMetadata::publish(topic), &reading)
        .await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await?
        .expect("receiver closed");

    assert_eq!(frame.metadata().encoding(), &TestReadingWire::encoding());
    assert_eq!(
        frame.deserialize::<TestReadingWire, TestReading>()?,
        reading
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn owned_transport_round_trips_protobuf_wire_format() -> Result<(), Box<dyn std::error::Error>>
{
    test_lib::before_test();

    let topic = UUri::try_from_parts("ownedpbtest", 0x4210, 1, 0x9005)?;
    let transport = test_lib::create_up_transport_zenoh("ownedpbtest", None).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();

    transport
        .register_owned_listener(&topic, None, Arc::new(FrameSender(tx)))
        .await?;

    let mut payload = StringValue::new();
    payload.value = "protobuf over zenoh owned".to_string();

    transport
        .send_serialized::<ProtobufWire, _>(UFrameMetadata::publish(topic), &payload)
        .await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await?
        .expect("receiver closed");
    let decoded: StringValue = frame.deserialize::<ProtobufWire, _>()?;

    assert_eq!(frame.metadata().encoding(), &ProtobufWire::encoding());
    assert_eq!(decoded.value, payload.value);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn owned_transport_preserves_native_frame_metadata() -> Result<(), Box<dyn std::error::Error>>
{
    test_lib::before_test();

    let authority = "ownedmetadatatest";
    let source = UUri::try_from_parts(authority, 0x4210, 1, 0x9007)?;
    let sink = UUri::try_from_parts(authority, 0x4210, 1, 0)?;
    let transport = test_lib::create_up_transport_zenoh(authority, None).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();

    transport
        .register_owned_listener(&source, Some(&sink), Arc::new(FrameSender(tx)))
        .await?;

    let id = UUID::build();
    let request_id = UUID::build();
    let attributes = UAttributes::new(
        id.clone(),
        source.clone(),
        Some(sink.clone()),
        UMessageType::Notification,
    )
    .with_priority(UPriority::CS5)
    .with_ttl(5_000)
    .with_request_id(request_id.clone())
    .with_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
    .with_token("transport-auth-token")
    .with_permission_level(7)
    .with_comm_status(UCode::UNAVAILABLE);
    let reading = TestReading {
        sensor_id: 11,
        counter: 121,
    };

    transport
        .send_serialized::<TestReadingWire, _>(
            UFrameMetadata::new(attributes, TestReadingWire::encoding()),
            &reading,
        )
        .await?;

    let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await?
        .expect("receiver closed");
    let received = frame.metadata().attributes();

    assert_eq!(received.id(), &id);
    assert_eq!(received.source(), &source);
    assert_eq!(received.sink(), Some(&sink));
    assert_eq!(received.message_type(), UMessageType::Notification);
    assert_eq!(received.priority(), UPriority::CS5);
    assert_eq!(received.ttl(), Some(5_000));
    assert_eq!(received.request_id(), Some(&request_id));
    assert_eq!(
        received.traceparent(),
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00")
    );
    assert_eq!(received.token(), Some("transport-auth-token"));
    assert_eq!(received.permission_level(), Some(7));
    assert_eq!(received.commstatus(), Some(UCode::UNAVAILABLE));
    assert_eq!(frame.metadata().encoding(), &TestReadingWire::encoding());
    assert_eq!(
        frame.deserialize::<TestReadingWire, TestReading>()?,
        reading
    );
    Ok(())
}
