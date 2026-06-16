use up_rust::{
    PayloadEncoding, ProtobufWire, UFrameMetadata, UMessageBuilder, UPayloadFormat, UTxBuffer,
    UTxLoanSpec, UUri, UWireMetadata, UWithWire, UZeroCopyTransport,
};
use up_transport_zenoh::{zenoh_config, UPTransportZenoh};

fn topic() -> UUri {
    UUri::try_from_parts("vehicle", 0x4210, 0x01, 0x9000).expect("topic URI")
}

#[tokio::test(flavor = "multi_thread")]
async fn zenoh_zero_copy_loan_uses_shm_and_selected_wire_attachment() {
    let transport = UPTransportZenoh::new(zenoh_config::Config::default(), "//vehicle/4210/1/0")
        .await
        .expect("transport");
    let transport = transport.with_wire(ProtobufWire);

    let message = UMessageBuilder::publish(topic()).build().expect("message");
    let metadata = UFrameMetadata::new(
        message.attributes().clone(),
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    )
    .expect("metadata");
    let mut tx = transport
        .loan_tx(UTxLoanSpec::payload(metadata.clone(), 16, 8).expect("loan spec"))
        .await
        .expect("loan");

    assert_eq!(tx.payload().len(), 16);
    assert_eq!((tx.payload().as_ptr() as usize) % 8, 0);
    tx.payload_mut().copy_from_slice(b"0123456789abcdef");

    let attachment = tx.attachment_bytes();
    assert_eq!(
        ProtobufWire::decode_frame_metadata(&attachment).expect("decode metadata"),
        metadata
    );
}
