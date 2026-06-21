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

#![allow(clippy::missing_panics_doc, clippy::too_many_lines)]

use std::{sync::Arc, time::Duration, time::SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tokio::{runtime::Runtime, sync::mpsc, time::Instant};
#[cfg(all(
    feature = "payload-contract-benchmarks",
    feature = "payload-contract-large-benchmarks"
))]
use up_rust::bench_fixtures::payload_contract::{
    CameraBayerRggb12pFrame8mpV1, LidarPointCloudHesaiAt128V1,
};
#[cfg(feature = "payload-contract-benchmarks")]
use up_rust::{
    bench_fixtures::payload_contract::{
        self as payload_contract, CanClassicFrameV1, CanFdFrameV1, PayloadContractCase,
        PayloadContractCaseKind, RadarDetectionListArs548V1, SomeIpSignalBatchMtuV1,
        StreamChunk4kV1, StreamChunk64kV1,
    },
    PayloadEncoding, ULoanedContiguousZeroCopyRxFrame,
};
use up_rust::{
    try_project_umessage_to_frame_metadata, NativePrefixProtobufMetadataCodec,
    StableContainerWireFormat, UCode, UFrameMetadata, UMessage, UMessageBuilder, UMessageType,
    UOwnedFrame, UOwnedListener, UOwnedTransport, UPayloadFormat, UStatus, UUri, UWire,
    UWireMetadataCodec, UWireRx, UZeroCopyListener, UZeroCopyTransport,
    UZeroCopyUninitTransportExt, UUID,
};
use up_transport_zenoh::{
    zenoh_config, UPTransportZenoh, ZenohOwnedCore, ZenohRxFrame, ZenohZeroCopyCore,
};

const BENCH_TIMEOUT: Duration = Duration::from_secs(5);
const LARGE_SENSOR_BENCH_TIMEOUT: Duration = Duration::from_secs(30);
const ZENOH_SHM_SEGMENT_SIZE: usize = 64 * 1_024 * 1_024;
const UUID_LSB_BASE: u64 = 0x8000_0000_0000_0000;
#[cfg(feature = "payload-contract-benchmarks")]
const PAYLOAD_CONTRACT_SEQUENCE: u32 = 1;

#[derive(Clone, Copy)]
enum BenchProfile {
    Core,
    Camera,
    All,
}

impl BenchProfile {
    fn from_env() -> Self {
        match std::env::var("TRANSPORT_BENCH_PROFILE")
            .unwrap_or_else(|_| "all".to_string())
            .as_str()
        {
            "core" => Self::Core,
            "camera" => Self::Camera,
            "all" => Self::All,
            other => {
                panic!("TRANSPORT_BENCH_PROFILE must be one of core, camera, all; got {other}")
            }
        }
    }

    fn includes_core(self) -> bool {
        matches!(self, Self::Core | Self::All)
    }

    fn includes_camera(self) -> bool {
        matches!(self, Self::Camera | Self::All)
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum DiagnosticMode {
    FullLoop,
    PrebuiltPayload,
    MetadataOnly,
    TxOnly,
    RxOnly,
    CopyLedger,
    ZcInitOnly,
    ZcSendOnly,
    ZcRxOnly,
    ZcValidationOnly,
    ZcFilterOnly,
    ZcCopyLedger,
    ZcLoanProvenanceCheck,
}

#[cfg(feature = "payload-contract-benchmarks")]
impl DiagnosticMode {
    fn from_env() -> Self {
        match std::env::var("TRANSPORT_BENCH_DIAGNOSTIC")
            .unwrap_or_else(|_| "full-loop".to_string())
            .as_str()
        {
            "full-loop" | "full" => Self::FullLoop,
            "prebuilt-payload" => Self::PrebuiltPayload,
            "metadata-only" => Self::MetadataOnly,
            "tx-only" => Self::TxOnly,
            "rx-only" | "listener-only" => Self::RxOnly,
            "copy-ledger" => Self::CopyLedger,
            "zc-init-only" => Self::ZcInitOnly,
            "zc-send-only" => Self::ZcSendOnly,
            "zc-rx-only" => Self::ZcRxOnly,
            "zc-validation-only" => Self::ZcValidationOnly,
            "zc-filter-only" => Self::ZcFilterOnly,
            "zc-copy-ledger" => Self::ZcCopyLedger,
            "zc-loan-provenance-check" => Self::ZcLoanProvenanceCheck,
            other => panic!("unsupported TRANSPORT_BENCH_DIAGNOSTIC selector: {other}"),
        }
    }

    fn group_suffix(self) -> &'static str {
        match self {
            Self::FullLoop => "full_loop",
            Self::PrebuiltPayload => "prebuilt_payload",
            Self::MetadataOnly => "metadata_only",
            Self::TxOnly => "tx_only",
            Self::RxOnly => "rx_only",
            Self::CopyLedger => "copy_ledger",
            Self::ZcInitOnly => "zc_init_only",
            Self::ZcSendOnly => "zc_send_only",
            Self::ZcRxOnly => "zc_rx_only",
            Self::ZcValidationOnly => "zc_validation_only",
            Self::ZcFilterOnly => "zc_filter_only",
            Self::ZcCopyLedger => "zc_copy_ledger",
            Self::ZcLoanProvenanceCheck => "zc_loan_provenance_check",
        }
    }

    fn includes_path(self, path: PayloadContractPath) -> bool {
        match self {
            Self::PrebuiltPayload | Self::CopyLedger => !path.is_zero_copy(),
            Self::ZcInitOnly
            | Self::ZcSendOnly
            | Self::ZcRxOnly
            | Self::ZcValidationOnly
            | Self::ZcFilterOnly
            | Self::ZcCopyLedger
            | Self::ZcLoanProvenanceCheck => path.is_zero_copy(),
            Self::FullLoop | Self::MetadataOnly | Self::TxOnly | Self::RxOnly => true,
        }
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
#[derive(Clone, Copy)]
enum PayloadContractPath {
    ProtobufOwned,
    StableZcNoZero,
    StableOwnedBytes,
}

#[cfg(feature = "payload-contract-benchmarks")]
impl PayloadContractPath {
    fn label(self) -> &'static str {
        match self {
            Self::ProtobufOwned => "protobuf_owned_full",
            Self::StableZcNoZero => "stable_zc_nozero_full",
            Self::StableOwnedBytes => "stable_owned_bytes_full",
        }
    }

    fn is_zero_copy(self) -> bool {
        matches!(self, Self::StableZcNoZero)
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct PrebuiltOwnedPayload {
    bytes: Bytes,
    format: UPayloadFormat,
}

struct BenchCase {
    authority: String,
    source: UUri,
}

impl BenchCase {
    fn new(fixture_name: &str) -> Self {
        let sequence = next_sequence();
        let authority = format!(
            "zenoh-userializer-bench-{}-{fixture_name}-{sequence}",
            std::process::id()
        );
        let source = uri(&authority, 0x4210, resource_id(0x9000, sequence));
        Self { authority, source }
    }

    fn message_builder(&self, id: UUID) -> UMessageBuilder {
        let mut builder = UMessageBuilder::publish(self.source.clone());
        builder.with_message_id(id);
        builder
    }

    fn metadata(&self, id: UUID) -> UFrameMetadata {
        let message = self
            .message_builder(id)
            .build()
            .expect("valid benchmark metadata message");
        UFrameMetadata::new(message.attributes().clone(), None).expect("valid benchmark metadata")
    }

    fn message(
        &self,
        id: UUID,
        payload: impl Into<Bytes>,
        format: UPayloadFormat,
    ) -> Result<UMessage, UStatus> {
        self.message_builder(id)
            .build_with_payload(payload, format)
            .map_err(|error| invalid_argument(error.to_string()))
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct PayloadContractAck {
    id: UUID,
    message_type: UMessageType,
    case_id: u32,
    sequence: u32,
    semantic_reference_len: usize,
    transported_payload_len: usize,
}

#[cfg(feature = "payload-contract-benchmarks")]
struct OwnedAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
    path: PayloadContractPath,
    encoding: Option<PayloadEncoding>,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UOwnedListener for OwnedAckListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.tx
            .send(owned_ack(
                &frame,
                &self.contract,
                self.path,
                self.encoding.as_ref(),
            ))
            .expect("owned benchmark receive channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct SelectedWireAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl
    UZeroCopyListener<
        UWireRx<ZenohRxFrame, StableContainerWireFormat, NativePrefixProtobufMetadataCodec>,
    > for SelectedWireAckListener
{
    async fn on_receive_zero_copy(
        &self,
        frame: UWireRx<ZenohRxFrame, StableContainerWireFormat, NativePrefixProtobufMetadataCodec>,
    ) {
        self.tx
            .send(selected_wire_ack(&frame, &self.contract))
            .expect("selected-wire benchmark receive channel should remain open");
    }
}

async fn build_owned_transport(
    authority: &str,
) -> Arc<
    up_rust::UWireTransport<
        ZenohOwnedCore,
        StableContainerWireFormat,
        NativePrefixProtobufMetadataCodec,
    >,
> {
    let core = ZenohOwnedCore::new(
        zenoh_config::Config::default(),
        format!("//{authority}/4210/1/0"),
    )
    .await
    .expect("Zenoh owned benchmark core should build");
    Arc::new(core.with_selected_wire(StableContainerWireFormat))
}

async fn build_selected_wire_transport(
    authority: &str,
) -> Arc<
    up_rust::UWireTransport<
        ZenohZeroCopyCore,
        StableContainerWireFormat,
        NativePrefixProtobufMetadataCodec,
    >,
> {
    let core = ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/0"))
        .with_config(zenoh_config::Config::default())
        .with_shm_segment_size(ZENOH_SHM_SEGMENT_SIZE)
        .expect("valid Zenoh SHM segment size")
        .build()
        .await
        .expect("Zenoh selected-wire benchmark core should build");
    Arc::new(core.with_selected_wire(StableContainerWireFormat))
}

async fn register_owned_listener(
    transport: &Arc<
        up_rust::UWireTransport<
            ZenohOwnedCore,
            StableContainerWireFormat,
            NativePrefixProtobufMetadataCodec,
        >,
    >,
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
) {
    let encoding = match path {
        PayloadContractPath::StableOwnedBytes => Some(
            payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                .expect("stable owned fixture should be available")
                .encoding,
        ),
        PayloadContractPath::ProtobufOwned => None,
        PayloadContractPath::StableZcNoZero => {
            unreachable!("selected-wire path uses zero-copy listener")
        }
    };
    transport
        .register_owned_listener(
            &case.source,
            None,
            Arc::new(OwnedAckListener {
                tx,
                contract: *contract,
                path,
                encoding,
            }),
        )
        .await
        .expect("owned benchmark listener should register");
}

async fn register_selected_wire_listener(
    transport: &Arc<
        up_rust::UWireTransport<
            ZenohZeroCopyCore,
            StableContainerWireFormat,
            NativePrefixProtobufMetadataCodec,
        >,
    >,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
) {
    transport
        .register_zero_copy_listener(
            &case.source,
            None,
            Arc::new(SelectedWireAckListener {
                tx,
                contract: *contract,
            }),
        )
        .await
        .expect("selected-wire benchmark listener should register");
}

async fn send_owned(
    transport: &Arc<
        up_rust::UWireTransport<
            ZenohOwnedCore,
            StableContainerWireFormat,
            NativePrefixProtobufMetadataCodec,
        >,
    >,
    path: PayloadContractPath,
    case: &BenchCase,
    id: UUID,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    let (payload, format) = match path {
        PayloadContractPath::ProtobufOwned => (
            payload_contract::protobuf_encoded_bytes_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                .map_err(|error| invalid_argument(error.to_string()))?,
            UPayloadFormat::Protobuf,
        ),
        PayloadContractPath::StableOwnedBytes => (
            payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                .map_err(|error| invalid_argument(error.to_string()))?
                .bytes,
            UPayloadFormat::Raw,
        ),
        PayloadContractPath::StableZcNoZero => {
            unreachable!("selected-wire path uses zero-copy send")
        }
    };
    let message = case.message(id, payload, format)?;
    let metadata = try_project_umessage_to_frame_metadata(&message)
        .map_err(|error| invalid_argument(error.to_string()))?;
    let frame = if let Some(payload) = message.payload() {
        UOwnedFrame::with_payload(metadata, payload)
            .map_err(|error| invalid_argument(error.to_string()))?
    } else {
        UOwnedFrame::without_payload(metadata)
            .map_err(|error| invalid_argument(error.to_string()))?
    };
    transport.send_owned(frame).await
}

#[cfg(feature = "payload-contract-benchmarks")]
fn prebuilt_owned_payload(
    path: PayloadContractPath,
    contract: &PayloadContractCase,
) -> Option<PrebuiltOwnedPayload> {
    match path {
        PayloadContractPath::ProtobufOwned => Some(PrebuiltOwnedPayload {
            bytes: payload_contract::protobuf_encoded_bytes_for(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
            )
            .expect("protobuf payload-contract bytes should build")
            .into(),
            format: UPayloadFormat::Protobuf,
        }),
        PayloadContractPath::StableOwnedBytes => {
            let fixture =
                payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                    .expect("stable owned fixture should build");
            Some(PrebuiltOwnedPayload {
                bytes: fixture.bytes.into(),
                format: UPayloadFormat::Raw,
            })
        }
        PayloadContractPath::StableZcNoZero => None,
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn send_owned_prebuilt(
    transport: &Arc<
        up_rust::UWireTransport<
            ZenohOwnedCore,
            StableContainerWireFormat,
            NativePrefixProtobufMetadataCodec,
        >,
    >,
    case: &BenchCase,
    id: UUID,
    payload: &PrebuiltOwnedPayload,
) -> Result<(), UStatus> {
    let message = case.message(id, payload.bytes.clone(), payload.format)?;
    let metadata = try_project_umessage_to_frame_metadata(&message)
        .map_err(|error| invalid_argument(error.to_string()))?;
    let frame = UOwnedFrame::with_payload(metadata, payload.bytes.clone())
        .map_err(|error| invalid_argument(error.to_string()))?;
    transport.send_owned(frame).await
}

async fn send_selected_wire(
    transport: &Arc<
        up_rust::UWireTransport<
            ZenohZeroCopyCore,
            StableContainerWireFormat,
            NativePrefixProtobufMetadataCodec,
        >,
    >,
    metadata: UFrameMetadata,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    match contract.kind() {
        PayloadContractCaseKind::CanClassicMax => {
            transport
                .send_uninit_stable_payload::<CanClassicFrameV1>(metadata, |payload| {
                    payload_contract::init_can_classic_max(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::CanFdMax => {
            transport
                .send_uninit_stable_payload::<CanFdFrameV1>(metadata, |payload| {
                    payload_contract::init_can_fd_max(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::SomeIpSingleMtu => {
            transport
                .send_uninit_stable_payload::<SomeIpSignalBatchMtuV1>(metadata, |payload| {
                    payload_contract::init_someip_single_mtu(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::Streamer4k => {
            transport
                .send_uninit_stable_payload::<StreamChunk4kV1>(metadata, |payload| {
                    payload_contract::init_streamer_4k(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::RadarArs548DetectionList => {
            transport
                .send_uninit_stable_payload::<RadarDetectionListArs548V1>(metadata, |payload| {
                    payload_contract::init_radar_ars548_detection_list(
                        payload,
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                })
                .await
        }
        PayloadContractCaseKind::Streamer64k => {
            transport
                .send_uninit_stable_payload::<StreamChunk64kV1>(metadata, |payload| {
                    payload_contract::init_streamer_64k(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
            transport
                .send_uninit_stable_payload::<LidarPointCloudHesaiAt128V1>(metadata, |payload| {
                    payload_contract::init_lidar_hesai_at128_point_cloud(
                        payload,
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                })
                .await
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::Camera8mpBayerRggb12p => {
            transport
                .send_uninit_stable_payload::<CameraBayerRggb12pFrame8mpV1>(metadata, |payload| {
                    payload_contract::init_camera_8mp_bayer_rggb12p(
                        payload,
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                })
                .await
        }
    }
}

async fn wait_for_ack(
    rx: &mut mpsc::UnboundedReceiver<PayloadContractAck>,
    expected_id: &UUID,
    contract: &PayloadContractCase,
    expected_len: usize,
) -> PayloadContractAck {
    let deadline = Instant::now() + BENCH_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for benchmark frame"
        );
        let ack = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for benchmark receive")
            .expect("benchmark receive channel should remain open");
        if &ack.id != expected_id {
            continue;
        }
        assert_eq!(ack.message_type, UMessageType::Publish);
        assert_eq!(ack.case_id, contract.case_id());
        assert_eq!(ack.sequence, PAYLOAD_CONTRACT_SEQUENCE);
        assert_eq!(
            ack.semantic_reference_len,
            contract.semantic_reference_len()
        );
        assert_eq!(ack.transported_payload_len, expected_len);
        return ack;
    }
}

fn owned_ack(
    frame: &UOwnedFrame,
    contract: &PayloadContractCase,
    path: PayloadContractPath,
    encoding: Option<&PayloadEncoding>,
) -> PayloadContractAck {
    let payload = frame.payload().expect("owned benchmark frame payload");
    match path {
        PayloadContractPath::ProtobufOwned => {
            payload_contract::validate_protobuf_bytes(contract, PAYLOAD_CONTRACT_SEQUENCE, payload)
                .expect("protobuf payload-contract frame should validate");
        }
        PayloadContractPath::StableOwnedBytes => {
            payload_contract::validate_stable_owned_bytes(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                encoding,
                payload,
            )
            .expect("stable owned payload-contract frame should validate");
        }
        PayloadContractPath::StableZcNoZero => {
            unreachable!("selected-wire path uses zero-copy listener")
        }
    }
    PayloadContractAck {
        id: frame.metadata().attributes().id().clone(),
        message_type: frame.metadata().attributes().type_(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: payload.len(),
    }
}

fn selected_wire_ack(
    frame: &impl ULoanedContiguousZeroCopyRxFrame,
    contract: &PayloadContractCase,
) -> PayloadContractAck {
    black_box(
        frame
            .payload_loan_provenance()
            .expect("stable payload should be loan-backed"),
    );
    validate_stable_payload_for_case(frame, contract);
    PayloadContractAck {
        id: frame.metadata().attributes().id().clone(),
        message_type: frame.metadata().attributes().type_(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: frame.payload_len(),
    }
}

fn validate_stable_payload_for_case(
    frame: &impl ULoanedContiguousZeroCopyRxFrame,
    contract: &PayloadContractCase,
) {
    match contract.kind() {
        PayloadContractCaseKind::CanClassicMax => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<CanClassicFrameV1>()
                .expect("CAN Classic stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::CanFdMax => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<CanFdFrameV1>()
                .expect("CAN FD stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::SomeIpSingleMtu => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<SomeIpSignalBatchMtuV1>()
                .expect("SOME/IP stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::Streamer4k => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<StreamChunk4kV1>()
                .expect("stream 4K stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::RadarArs548DetectionList => {
            payload_contract::validate_stable_payload(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                frame
                    .borrow_stable_payload::<RadarDetectionListArs548V1>()
                    .expect("radar stable payload-contract frame should borrow"),
            )
        }
        PayloadContractCaseKind::Streamer64k => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<StreamChunk64kV1>()
                .expect("stream 64K stable payload-contract frame should borrow"),
        ),
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
            payload_contract::validate_stable_payload(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                frame
                    .borrow_stable_payload::<LidarPointCloudHesaiAt128V1>()
                    .expect("LiDAR stable payload-contract frame should borrow"),
            )
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::Camera8mpBayerRggb12p => {
            payload_contract::validate_stable_payload(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                frame
                    .borrow_stable_payload::<CameraBayerRggb12pFrame8mpV1>()
                    .expect("camera stable payload-contract frame should borrow"),
            )
        }
    }
    .expect("stable payload-contract frame should validate");
}

fn transported_len(path: PayloadContractPath, contract: &PayloadContractCase) -> usize {
    match path {
        PayloadContractPath::ProtobufOwned => {
            payload_contract::protobuf_encoded_len(contract, PAYLOAD_CONTRACT_SEQUENCE)
        }
        PayloadContractPath::StableZcNoZero | PayloadContractPath::StableOwnedBytes => {
            payload_contract::stable_payload_len(contract)
        }
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
fn benchmark_id(
    path: PayloadContractPath,
    mode: DiagnosticMode,
    contract: &PayloadContractCase,
    expected_len: usize,
) -> BenchmarkId {
    BenchmarkId::new(
        format!("{}_{}", path.label(), mode.group_suffix()),
        format!(
            "publish/{}/{}/{}",
            contract.name(),
            contract.semantic_reference_len(),
            expected_len
        ),
    )
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_metadata_only(path: PayloadContractPath, case: &BenchCase, contract: &PayloadContractCase) {
    let id = next_uuid();
    let metadata = case.metadata(id);
    let encoded = NativePrefixProtobufMetadataCodec
        .encode_frame_metadata(StableContainerWireFormat::metadata_context(), &metadata)
        .expect("selected-wire metadata should encode");
    let decoded = NativePrefixProtobufMetadataCodec
        .decode_frame_metadata(StableContainerWireFormat::metadata_context(), &encoded)
        .expect("selected-wire metadata should decode");
    black_box(decoded);
    black_box(encoded.len());
    black_box(path.label());
    black_box(contract.name());
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_copy_ledger(path: PayloadContractPath, contract: &PayloadContractCase) {
    let metadata_len = NativePrefixProtobufMetadataCodec
        .encode_frame_metadata(
            StableContainerWireFormat::metadata_context(),
            &BenchCase::new(contract.name()).metadata(next_uuid()),
        )
        .expect("selected-wire metadata should encode")
        .len();
    let payload_copied = match path {
        PayloadContractPath::ProtobufOwned | PayloadContractPath::StableOwnedBytes => {
            transported_len(path, contract) * 2
        }
        PayloadContractPath::StableZcNoZero => 0,
    };
    black_box(metadata_len * 2);
    black_box(payload_copied);
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_zc_validation_only(contract: &PayloadContractCase) {
    let fixture = payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
        .expect("stable fixture should be available");
    black_box(fixture.bytes.len());
    black_box(fixture.encoding);
    black_box(contract.semantic_reference_len());
}

fn bench_payload_contract_matrix(
    c: &mut Criterion,
    group_name: &'static str,
    payload_cases: &[PayloadContractCase],
    measurement_time: Duration,
    diagnostic_mode: DiagnosticMode,
) {
    let runtime = Runtime::new().expect("tokio runtime");
    let group_name = if diagnostic_mode == DiagnosticMode::FullLoop {
        group_name.to_string()
    } else {
        format!("{group_name}_{}", diagnostic_mode.group_suffix())
    };
    let mut group = c.benchmark_group(group_name);
    group.measurement_time(measurement_time);
    for contract in payload_cases {
        for path in [
            PayloadContractPath::ProtobufOwned,
            PayloadContractPath::StableZcNoZero,
            PayloadContractPath::StableOwnedBytes,
        ] {
            if !diagnostic_mode.includes_path(path) {
                continue;
            }
            let case = BenchCase::new(contract.name());
            let expected_len = transported_len(path, contract);
            let (tx, mut rx) = mpsc::unbounded_channel();
            let owned_transport = match path {
                PayloadContractPath::ProtobufOwned | PayloadContractPath::StableOwnedBytes => {
                    let transport = runtime.block_on(build_owned_transport(&case.authority));
                    runtime.block_on(register_owned_listener(
                        &transport,
                        path,
                        &case,
                        contract,
                        tx.clone(),
                    ));
                    Some(transport)
                }
                PayloadContractPath::StableZcNoZero => None,
            };
            let selected_transport = match path {
                PayloadContractPath::StableZcNoZero => {
                    let transport =
                        runtime.block_on(build_selected_wire_transport(&case.authority));
                    runtime.block_on(register_selected_wire_listener(
                        &transport, &case, contract, tx,
                    ));
                    Some(transport)
                }
                PayloadContractPath::ProtobufOwned | PayloadContractPath::StableOwnedBytes => None,
            };
            let prebuilt_payload = prebuilt_owned_payload(path, contract);

            group.bench_function(
                benchmark_id(path, diagnostic_mode, contract, expected_len),
                |b| {
                    b.iter(|| match diagnostic_mode {
                        DiagnosticMode::MetadataOnly | DiagnosticMode::ZcFilterOnly => {
                            run_metadata_only(path, &case, contract);
                        }
                        DiagnosticMode::CopyLedger | DiagnosticMode::ZcCopyLedger => {
                            run_copy_ledger(path, contract);
                        }
                        DiagnosticMode::ZcValidationOnly => {
                            run_zc_validation_only(contract);
                        }
                        DiagnosticMode::ZcInitOnly => {
                            black_box(case.metadata(next_uuid()));
                            black_box(transported_len(path, contract));
                            black_box(contract.semantic_reference_len());
                        }
                        DiagnosticMode::FullLoop
                        | DiagnosticMode::PrebuiltPayload
                        | DiagnosticMode::TxOnly
                        | DiagnosticMode::RxOnly
                        | DiagnosticMode::ZcSendOnly
                        | DiagnosticMode::ZcRxOnly
                        | DiagnosticMode::ZcLoanProvenanceCheck => {
                            runtime.block_on(async {
                                let id = next_uuid();
                                match path {
                                    PayloadContractPath::ProtobufOwned
                                    | PayloadContractPath::StableOwnedBytes => {
                                        if diagnostic_mode == DiagnosticMode::PrebuiltPayload {
                                            send_owned_prebuilt(
                                                owned_transport.as_ref().expect("owned transport"),
                                                &case,
                                                id.clone(),
                                                prebuilt_payload
                                                    .as_ref()
                                                    .expect("prebuilt owned payload"),
                                            )
                                            .await
                                            .expect("owned prebuilt benchmark send should succeed");
                                        } else {
                                            send_owned(
                                                owned_transport.as_ref().expect("owned transport"),
                                                path,
                                                &case,
                                                id.clone(),
                                                contract,
                                            )
                                            .await
                                            .expect("owned benchmark send should succeed");
                                        }
                                    }
                                    PayloadContractPath::StableZcNoZero => {
                                        send_selected_wire(
                                            selected_transport
                                                .as_ref()
                                                .expect("selected-wire transport"),
                                            case.metadata(id.clone()),
                                            contract,
                                        )
                                        .await
                                        .expect("selected-wire benchmark send should succeed");
                                    }
                                }
                                if !matches!(
                                    diagnostic_mode,
                                    DiagnosticMode::TxOnly | DiagnosticMode::ZcSendOnly
                                ) {
                                    let ack =
                                        wait_for_ack(&mut rx, &id, contract, expected_len).await;
                                    black_box(ack.semantic_reference_len);
                                    black_box(ack.transported_payload_len);
                                }
                                black_box(contract.name());
                            });
                        }
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_transport(c: &mut Criterion) {
    UPTransportZenoh::try_init_log_from_env();
    match std::env::var("TRANSPORT_BENCH_SUITE")
        .unwrap_or_else(|_| "payload-contract".to_string())
        .as_str()
    {
        "payload-contract" => {}
        other => panic!("TRANSPORT_BENCH_SUITE must be payload-contract; got {other}"),
    }
    bench_payload_contract(c, BenchProfile::from_env());
}

#[cfg(feature = "payload-contract-benchmarks")]
fn bench_payload_contract(c: &mut Criterion, profile: BenchProfile) {
    let diagnostic_mode = DiagnosticMode::from_env();
    if profile.includes_core() {
        bench_payload_contract_matrix(
            c,
            "transport_payload_contract_core",
            payload_contract::core_cases(),
            BENCH_TIMEOUT,
            diagnostic_mode,
        );
    }
    if profile.includes_camera() {
        bench_payload_contract_matrix(
            c,
            "transport_payload_contract_large_sensor",
            payload_contract::large_sensor_cases(),
            LARGE_SENSOR_BENCH_TIMEOUT,
            diagnostic_mode,
        );
    }
}

#[cfg(not(feature = "payload-contract-benchmarks"))]
fn bench_payload_contract(_c: &mut Criterion, _profile: BenchProfile) {
    panic!("TRANSPORT_BENCH_SUITE=payload-contract requires feature payload-contract-benchmarks");
}

fn next_sequence() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn next_uuid() -> UUID {
    uuid_for(next_sequence())
}

fn uuid_for(sequence: u64) -> UUID {
    let timestamp_millis = u64::try_from(
        SystemTime::UNIX_EPOCH
            .elapsed()
            .expect("system time should be after UNIX epoch")
            .as_millis(),
    )
    .expect("timestamp millis should fit in u64");
    let msb = (timestamp_millis << 16) | 0x7000 | (sequence & 0x0fff);
    let lsb = UUID_LSB_BASE | (sequence & 0x3fff_ffff_ffff_ffff);
    UUID::from_u64_pair(msb, lsb).expect("benchmark UUID should be valid UUIDv7")
}

fn resource_id(base: u16, sequence: u64) -> u16 {
    let offset = u16::try_from(sequence % 0x0fff).expect("resource offset fits in u16");
    base.checked_add(offset)
        .expect("benchmark resource id fits")
}

fn uri(authority: &str, entity_type: u32, resource: u16) -> UUri {
    UUri::try_from_parts(authority, entity_type, 1, resource).expect("valid benchmark URI")
}

fn invalid_argument(error: String) -> UStatus {
    UStatus::fail_with_code(UCode::InvalidArgument, error)
}

criterion_group!(transport_criterion, bench_transport);
criterion_main!(transport_criterion);
