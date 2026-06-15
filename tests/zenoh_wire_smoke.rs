use std::{sync::Arc, sync::Mutex as StdMutex};

use async_trait::async_trait;
use up_rust_userializer::{
    PayloadEncoding, ProtobufWire, UCode, UFrameMetadata, UFrameView, UMessageBuilder,
    UPayloadFormat, UProtocolNativeWire, UTxBuffer, UTxLoanSpec, UUri, UWireMetadata, UWithWire,
    UZeroCopyListener, UZeroCopyTransport,
};
use up_transport_zenoh::{ZenohEncodedRxFrame, ZenohPreparedAttachment, ZenohWireCore};

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
async fn prepared_attachment_bytes_pass_through_for_smoke_wires() {
    assert_prepared_attachment::<UProtocolNativeWire>(None).await;
    assert_prepared_attachment::<ProtobufWire>(Some(PayloadEncoding::Standard(
        UPayloadFormat::Protobuf,
    )))
    .await;
}

async fn assert_prepared_attachment<W>(payload_encoding: Option<PayloadEncoding>)
where
    W: UWireMetadata + Default + Send + Sync + 'static,
{
    let core = ZenohWireCore::new();
    let transport = core.clone().with_wire(W::default());
    let frame_metadata = metadata(payload_encoding);
    let payload_len = if frame_metadata.payload_encoding().is_some() {
        4
    } else {
        0
    };
    let loan_spec = if payload_len == 0 {
        UTxLoanSpec::no_payload(frame_metadata.clone()).expect("loan spec")
    } else {
        UTxLoanSpec::payload(frame_metadata.clone(), payload_len, 1).expect("loan spec")
    };
    let mut tx = transport.loan_tx(loan_spec).await.expect("loan");
    if payload_len != 0 {
        tx.payload_mut().copy_from_slice(b"data");
    }

    let prepared = core.last_prepared().await.expect("prepared request");
    assert_eq!(prepared.metadata(), &frame_metadata);
    assert_eq!(prepared.encoded_metadata(), tx.attachment().as_bytes());
    assert!(tx.attachment().len() <= 164);

    let decoded = W::decode_frame_metadata(tx.attachment().as_bytes()).expect("decode");
    assert_eq!(decoded, frame_metadata);
}

#[tokio::test]
async fn protobuf_payload_bytes_round_trip_through_pull_receive() {
    let core = ZenohWireCore::new();
    let transport = core.clone().with_wire(ProtobufWire);
    let frame_metadata = metadata(Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)));
    let mut tx = transport
        .loan_tx(UTxLoanSpec::payload(frame_metadata, 4, 1).expect("loan spec"))
        .await
        .expect("loan");
    tx.payload_mut().copy_from_slice(b"data");
    transport.send_zero_copy(tx).await.expect("send");

    let rx = transport
        .receive_zero_copy(&source_filter(), None)
        .await
        .expect("receive");
    assert_eq!(rx.try_contiguous_payload(), Some(&b"data"[..]));
}

#[tokio::test]
async fn wrong_wire_metadata_is_rejected_before_pull_receive_exposes_frame() {
    let core = ZenohWireCore::new();
    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(Some(
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
    )))
    .expect("wrong metadata");
    core.push_encoded_rx(ZenohEncodedRxFrame::new(
        ZenohPreparedAttachment::from_encoded_metadata(wrong_metadata),
        b"drop".to_vec(),
    ))
    .await;
    let transport = core.with_wire(UProtocolNativeWire);

    let result = transport.receive_zero_copy(&source_filter(), None).await;
    let error = result.err().expect("wrong metadata rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[tokio::test]
async fn payload_family_mismatch_is_rejected_before_pull_receive_exposes_frame() {
    let core = ZenohWireCore::new();
    let mut mismatched =
        UProtocolNativeWire::encode_frame_metadata(&metadata(None)).expect("metadata to corrupt");
    mismatched[15..18].copy_from_slice(&[0x00, 0x02, 0x00]);
    core.push_encoded_rx(ZenohEncodedRxFrame::new(
        ZenohPreparedAttachment::from_encoded_metadata(mismatched),
        b"drop".to_vec(),
    ))
    .await;
    let transport = core.with_wire(UProtocolNativeWire);

    let result = transport.receive_zero_copy(&source_filter(), None).await;
    let error = result.err().expect("mismatch rejected");
    assert_eq!(error.get_code(), UCode::InvalidArgument);
}

#[tokio::test]
async fn malformed_listener_metadata_is_not_delivered() {
    let core = ZenohWireCore::new();
    let listener = Arc::new(CountingListener::default());
    let transport = core.clone().with_wire(UProtocolNativeWire);
    transport
        .register_zero_copy_listener(&source_filter(), None, listener.clone())
        .await
        .expect("register");

    let wrong_metadata = ProtobufWire::encode_frame_metadata(&metadata(Some(
        PayloadEncoding::Standard(UPayloadFormat::Protobuf),
    )))
    .expect("wrong metadata");
    core.deliver_encoded_rx(ZenohEncodedRxFrame::new(
        ZenohPreparedAttachment::from_encoded_metadata(wrong_metadata),
        b"drop".to_vec(),
    ))
    .await;

    assert_eq!(listener.payloads(), Vec::<Vec<u8>>::new());
}

#[derive(Default)]
struct CountingListener {
    payloads: StdMutex<Vec<Vec<u8>>>,
}

impl CountingListener {
    fn payloads(&self) -> Vec<Vec<u8>> {
        self.payloads.lock().expect("payload lock").clone()
    }
}

#[async_trait]
impl<W> UZeroCopyListener<up_rust_userializer::UWireRx<ZenohEncodedRxFrame, W>> for CountingListener
where
    W: UWireMetadata + Send + Sync + 'static,
{
    async fn on_receive_zero_copy(
        &self,
        frame: up_rust_userializer::UWireRx<ZenohEncodedRxFrame, W>,
    ) {
        self.payloads
            .lock()
            .expect("payload lock")
            .push(frame.try_contiguous_payload().unwrap_or_default().to_vec());
    }
}
