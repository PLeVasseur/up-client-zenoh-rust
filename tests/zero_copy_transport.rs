use std::{io::Read, marker::PhantomData, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::mpsc;
use up_rust::{
    PayloadEncoding, PayloadFormat, PayloadLoanProvenance, ProtobufWire, UCode, UFrameMetadata,
    UFrameView, ULoanedContiguousZeroCopyRxFrame, UMessageBuilder, UPayloadFormat,
    UProtocolNativeWire, UTxBuffer, UTxLoanSpec, UUninitTxBuffer, UUri, UWireMetadata, UWireRx,
    UZeroCopyListener, UZeroCopyTransport, UZeroCopyUninitTransport,
};
use up_transport_zenoh::{zenoh_config, ZenohRxFrame, ZenohZeroCopyCore};
use up_wire_xcdrv2::{XcdrV2Wire, VEHICLE_SIGNAL_V1_GOLDEN_BYTES};

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

fn metadata(source: UUri, payload_encoding: Option<PayloadEncoding>) -> UFrameMetadata {
    let message = UMessageBuilder::publish(source).build().expect("message");
    UFrameMetadata::new(message.attributes().clone(), payload_encoding).expect("metadata")
}

async fn test_core(authority: &str) -> ZenohZeroCopyCore {
    ZenohZeroCopyCore::new(
        zenoh_config::Config::default(),
        format!("//{authority}/4210/1/0"),
    )
    .await
    .expect("transport")
}

#[tokio::test(flavor = "multi_thread")]
async fn zenoh_zero_copy_loan_uses_shm_and_selected_wire_attachment() -> Result<(), TestError> {
    assert_zero_copy_prepared_metadata::<UProtocolNativeWire>(None, &[]).await?;
    assert_zero_copy_prepared_metadata::<ProtobufWire>(
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
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

async fn assert_zero_copy_prepared_metadata<W>(
    payload_encoding: Option<PayloadEncoding>,
    payload: &[u8],
) -> Result<(), TestError>
where
    W: UWireMetadata + Default + Send + Sync + 'static,
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
        W::decode_frame_metadata(&attachment).expect("decode metadata"),
        metadata
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn receive_zero_copy_returns_shm_payload_lease() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-rx-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x9300);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = b"rx-shm";
    let metadata = metadata(
        source,
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    );
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

    let payload = b"fanout";
    let metadata = metadata(
        source,
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn zero_copy_uninit_transmit_uses_selected_wire_metadata() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-uninit-{}", std::process::id());
    let transport = Arc::new(test_core(&authority).await.with_selected_wire(ProtobufWire));
    let source = topic_for(&authority, 0x9302);
    let receiver = transport.clone();
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = b"uninit";
    let metadata = metadata(
        source,
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    );
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
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = b"wrong";
    let metadata = metadata(
        source,
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    );
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

    let payload = b"drop";
    let metadata = metadata(
        source,
        Some(PayloadEncoding::Standard(UPayloadFormat::Protobuf)),
    );
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
async fn zero_copy_external_xcdrv2_wrong_wire_metadata_is_rejected() -> Result<(), TestError> {
    let authority = format!("zenoh-zcs-xcdr-wrong-wire-{}", std::process::id());
    let sender = Arc::new(test_core(&authority).await.with_selected_wire(XcdrV2Wire));
    let receiver = test_core(&authority).await.with_selected_wire(ProtobufWire);
    let source = topic_for(&authority, 0x9305);
    let receive_source = source.clone();
    let receive_task =
        tokio::spawn(async move { receiver.receive_zero_copy(&receive_source, None).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

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

impl<W> PayloadSender<W> {
    fn new(sender: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            sender,
            _wire: PhantomData,
        }
    }
}

#[async_trait]
impl<W> UZeroCopyListener<UWireRx<ZenohRxFrame, W>> for PayloadSender<W>
where
    W: UWireMetadata + Send + Sync + 'static,
{
    async fn on_receive_zero_copy(&self, frame: UWireRx<ZenohRxFrame, W>) {
        let mut payload = Vec::new();
        frame
            .payload_reader()
            .read_to_end(&mut payload)
            .expect("payload reader should succeed");
        self.sender.send(payload).expect("receiver should be open");
    }
}
