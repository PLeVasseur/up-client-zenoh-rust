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

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashSet,
    io::Cursor,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
    time::SystemTime,
};

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
    try_project_umessage_to_frame_metadata, NativePrefixFrameMetadataCodec,
    StableContainerWireFormat, StableContainerWireTransport, UCode, UEncodedRxFrame,
    UEncodedZeroCopyListener, UFrameMetadata, UFrameView, UMessage, UMessageBuilder, UMessageType,
    UOwnedFrame, UOwnedListener, UOwnedTransport, UPayloadFormat, UStatus, UUri, UWire,
    UWireMetadataCodec, UWireRx, UZeroCopyListener, UZeroCopyTransport, UZeroCopyTransportCore,
    UZeroCopyUninitTransportExt, UUID,
};
use up_transport_zenoh::{
    zenoh_config, UPTransportZenoh, ZenohOwnedCore, ZenohRxFrame, ZenohZeroCopyCore,
};
use zenoh::bytes::ZBytes;

const BENCH_TIMEOUT: Duration = Duration::from_secs(5);
const LARGE_SENSOR_BENCH_TIMEOUT: Duration = Duration::from_secs(30);
const ZENOH_SHM_SEGMENT_SIZE: usize = 64 * 1_024 * 1_024;
const UUID_LSB_BASE: u64 = 0x8000_0000_0000_0000;
#[cfg(feature = "payload-contract-benchmarks")]
const PAYLOAD_CONTRACT_SEQUENCE: u32 = 1;

type StableZenohOwnedTransport = StableContainerWireTransport<ZenohOwnedCore>;
type StableZenohZeroCopyTransport = StableContainerWireTransport<ZenohZeroCopyCore>;
type StableZenohRx =
    UWireRx<ZenohRxFrame, StableContainerWireFormat, NativePrefixFrameMetadataCodec>;
#[cfg(feature = "payload-contract-benchmarks")]
type StableBenchRx =
    UWireRx<BenchEncodedRxFrame, StableContainerWireFormat, NativePrefixFrameMetadataCodec>;

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[derive(Clone, Copy, Default)]
struct AllocationSample {
    allocations: usize,
    bytes: usize,
}

fn reset_allocations() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
}

fn allocation_sample() -> AllocationSample {
    AllocationSample {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
    }
}

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
    ZcRxZenohDeliveryOnly,
    ZcRxAttachmentDecodeOnly,
    ZcRxAdapterFilterDropOnly,
    ZcRxListenerDispatchOnly,
    ZcLoanProvenanceCheck,
    OwnedPayloadBuildOnly,
    OwnedFrameBuildOnly,
    OwnedRxNoValidation,
    ZcPayloadInitOnly,
    ZcRxNoValidation,
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
            "zc-rx-zenoh-delivery-only" => Self::ZcRxZenohDeliveryOnly,
            "zc-rx-attachment-decode-only" => Self::ZcRxAttachmentDecodeOnly,
            "zc-rx-adapter-filter-drop-only" => Self::ZcRxAdapterFilterDropOnly,
            "zc-rx-listener-dispatch-only" => Self::ZcRxListenerDispatchOnly,
            "zc-loan-provenance-check" => Self::ZcLoanProvenanceCheck,
            "owned-payload-build-only" => Self::OwnedPayloadBuildOnly,
            "owned-frame-build-only" => Self::OwnedFrameBuildOnly,
            "owned-rx-no-validation" => Self::OwnedRxNoValidation,
            "zc-payload-init-only" => Self::ZcPayloadInitOnly,
            "zc-rx-no-validation" => Self::ZcRxNoValidation,
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
            Self::ZcRxZenohDeliveryOnly => "zc_rx_zenoh_delivery_only",
            Self::ZcRxAttachmentDecodeOnly => "zc_rx_attachment_decode_only",
            Self::ZcRxAdapterFilterDropOnly => "zc_rx_adapter_filter_drop_only",
            Self::ZcRxListenerDispatchOnly => "zc_rx_listener_dispatch_only",
            Self::ZcLoanProvenanceCheck => "zc_loan_provenance_check",
            Self::OwnedPayloadBuildOnly => "owned_payload_build_only",
            Self::OwnedFrameBuildOnly => "owned_frame_build_only",
            Self::OwnedRxNoValidation => "owned_rx_no_validation",
            Self::ZcPayloadInitOnly => "zc_payload_init_only",
            Self::ZcRxNoValidation => "zc_rx_no_validation",
        }
    }

    fn includes_path(self, path: PayloadContractPath) -> bool {
        match self {
            Self::PrebuiltPayload
            | Self::CopyLedger
            | Self::OwnedPayloadBuildOnly
            | Self::OwnedFrameBuildOnly
            | Self::OwnedRxNoValidation => !path.is_zero_copy(),
            Self::ZcInitOnly
            | Self::ZcSendOnly
            | Self::ZcRxOnly
            | Self::ZcValidationOnly
            | Self::ZcFilterOnly
            | Self::ZcCopyLedger
            | Self::ZcRxZenohDeliveryOnly
            | Self::ZcRxAttachmentDecodeOnly
            | Self::ZcRxAdapterFilterDropOnly
            | Self::ZcRxListenerDispatchOnly
            | Self::ZcLoanProvenanceCheck
            | Self::ZcPayloadInitOnly
            | Self::ZcRxNoValidation => path.is_zero_copy(),
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
        UFrameMetadata::publish(self.source.clone())
            .with_id(id)
            .build()
            .expect("valid benchmark metadata")
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
#[derive(Clone)]
struct BenchEncodedRxFrame {
    encoded_metadata: Vec<u8>,
    payload: Bytes,
}

#[cfg(feature = "payload-contract-benchmarks")]
impl BenchEncodedRxFrame {
    fn new(encoded_metadata: Vec<u8>, payload_len: usize) -> Self {
        Self {
            encoded_metadata,
            payload: Bytes::from(vec![0; payload_len]),
        }
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
impl UEncodedRxFrame for BenchEncodedRxFrame {
    type PayloadReader<'a>
        = Cursor<&'a [u8]>
    where
        Self: 'a;
    type PayloadSlices<'a>
        = std::iter::Once<&'a [u8]>
    where
        Self: 'a;

    fn encoded_metadata(&self) -> &[u8] {
        &self.encoded_metadata
    }

    fn payload_len(&self) -> usize {
        self.payload.len()
    }

    fn payload_reader(&self) -> Self::PayloadReader<'_> {
        Cursor::new(self.payload.as_ref())
    }

    fn payload_slices(&self) -> Self::PayloadSlices<'_> {
        std::iter::once(self.payload.as_ref())
    }

    fn try_contiguous_payload(&self) -> Option<&[u8]> {
        Some(self.payload.as_ref())
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct RawDeliveryAck {
    id: UUID,
    payload_len: usize,
}

#[cfg(feature = "payload-contract-benchmarks")]
struct RawDeliveryListener {
    tx: mpsc::UnboundedSender<RawDeliveryAck>,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UEncodedZeroCopyListener<ZenohRxFrame> for RawDeliveryListener {
    async fn on_receive_encoded_zero_copy(&self, frame: ZenohRxFrame) {
        let decoded = NativePrefixFrameMetadataCodec
            .decode_frame_metadata(
                StableContainerWireFormat::metadata_context(),
                frame.encoded_metadata(),
            )
            .expect("raw Zenoh delivery metadata should decode");
        self.tx
            .send(RawDeliveryAck {
                id: decoded.id().clone(),
                payload_len: frame.payload_len(),
            })
            .expect("raw Zenoh delivery channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
#[derive(Default)]
struct ZenohAttributionSample {
    publish_attempts: usize,
    exact_deliveries: usize,
    wildcard_deliveries: usize,
    zenoh_prefiltered_count: usize,
    adapter_dropped_count: usize,
    listener_dispatched_count: usize,
    attachment_metadata_bytes: usize,
    metadata_copy_bytes: usize,
    payload_copy_bytes: usize,
    attachment_encode_allocations: usize,
    attachment_encode_bytes: usize,
    attachment_decode_allocations: usize,
    attachment_decode_bytes: usize,
    receive_drop_allocations: usize,
    receive_drop_bytes: usize,
}

#[cfg(feature = "payload-contract-benchmarks")]
struct OwnedAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
    path: PayloadContractPath,
    encoding: Option<PayloadEncoding>,
    validate_payload: bool,
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
                self.validate_payload,
            ))
            .expect("owned benchmark receive channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct SelectedWireAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
    validate_payload: bool,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UZeroCopyListener<StableZenohRx> for SelectedWireAckListener {
    async fn on_receive_zero_copy(&self, frame: StableZenohRx) {
        self.tx
            .send(selected_wire_ack(
                &frame,
                &self.contract,
                self.validate_payload,
            ))
            .expect("selected-wire benchmark receive channel should remain open");
    }
}

async fn build_owned_transport(authority: &str) -> Arc<StableZenohOwnedTransport> {
    let core = ZenohOwnedCore::new(
        zenoh_config::Config::default(),
        format!("//{authority}/4210/1/0"),
    )
    .await
    .expect("Zenoh owned benchmark core should build");
    Arc::new(core.with_selected_wire(StableContainerWireFormat))
}

async fn build_selected_wire_transport(authority: &str) -> Arc<StableZenohZeroCopyTransport> {
    let core = ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/0"))
        .with_config(zenoh_config::Config::default())
        .with_shm_segment_size(ZENOH_SHM_SEGMENT_SIZE)
        .expect("valid Zenoh SHM segment size")
        .build()
        .await
        .expect("Zenoh selected-wire benchmark core should build");
    Arc::new(core.with_selected_wire(StableContainerWireFormat))
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn build_raw_selected_wire_core(authority: &str) -> Arc<ZenohZeroCopyCore> {
    let core = ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/0"))
        .with_config(zenoh_config::Config::default())
        .with_shm_segment_size(ZENOH_SHM_SEGMENT_SIZE)
        .expect("valid Zenoh SHM segment size")
        .build()
        .await
        .expect("raw Zenoh selected-wire benchmark core should build");
    Arc::new(core)
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn register_raw_delivery_listener(
    core: &Arc<ZenohZeroCopyCore>,
    source_filter: &UUri,
    tx: mpsc::UnboundedSender<RawDeliveryAck>,
) {
    core.register_encoded_zero_copy_listener(
        source_filter,
        None,
        Arc::new(RawDeliveryListener { tx }),
    )
    .await
    .expect("raw Zenoh delivery listener should register");
}

async fn register_owned_listener(
    transport: &Arc<StableZenohOwnedTransport>,
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    validate_payload: bool,
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
                validate_payload,
            }),
        )
        .await
        .expect("owned benchmark listener should register");
}

async fn register_selected_wire_listener(
    transport: &Arc<StableZenohZeroCopyTransport>,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    validate_payload: bool,
) {
    transport
        .register_zero_copy_listener(
            &case.source,
            None,
            Arc::new(SelectedWireAckListener {
                tx,
                contract: *contract,
                validate_payload,
            }),
        )
        .await
        .expect("selected-wire benchmark listener should register");
}

async fn send_owned(
    transport: &Arc<StableZenohOwnedTransport>,
    path: PayloadContractPath,
    case: &BenchCase,
    id: UUID,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    let frame = build_owned_frame(path, case, id, contract)?;
    transport.send_owned(frame).await
}

fn owned_payload(
    path: PayloadContractPath,
    contract: &PayloadContractCase,
) -> Result<(Bytes, UPayloadFormat), UStatus> {
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
    Ok((payload.into(), format))
}

fn build_owned_frame(
    path: PayloadContractPath,
    case: &BenchCase,
    id: UUID,
    contract: &PayloadContractCase,
) -> Result<UOwnedFrame, UStatus> {
    let (payload, format) = owned_payload(path, contract)?;
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
    Ok(frame)
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
    transport: &Arc<StableZenohOwnedTransport>,
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
    transport: &Arc<StableZenohZeroCopyTransport>,
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
    validate_payload: bool,
) -> PayloadContractAck {
    let payload = frame.payload().expect("owned benchmark frame payload");
    if validate_payload {
        match path {
            PayloadContractPath::ProtobufOwned => {
                payload_contract::validate_protobuf_bytes(
                    contract,
                    PAYLOAD_CONTRACT_SEQUENCE,
                    payload,
                )
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
    }
    PayloadContractAck {
        id: frame.metadata().id().clone(),
        message_type: frame.metadata().kind().to_legacy_type(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: payload.len(),
    }
}

fn selected_wire_ack(
    frame: &impl ULoanedContiguousZeroCopyRxFrame,
    contract: &PayloadContractCase,
    validate_payload: bool,
) -> PayloadContractAck {
    black_box(
        frame
            .payload_loan_provenance()
            .expect("stable payload should be loan-backed"),
    );
    if validate_payload {
        validate_stable_payload_for_case(frame, contract);
    }
    PayloadContractAck {
        id: frame.metadata().id().clone(),
        message_type: frame.metadata().kind().to_legacy_type(),
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
fn case_filter_allows(contract: &PayloadContractCase) -> bool {
    let Ok(filter) = std::env::var("TRANSPORT_BENCH_CASE_FILTER") else {
        return true;
    };
    if filter.trim().is_empty() {
        return true;
    }
    filter
        .split(',')
        .map(str::trim)
        .filter(|case| !case.is_empty())
        .any(|case| case == contract.name())
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_owned_payload_build_only(path: PayloadContractPath, contract: &PayloadContractCase) {
    let (payload, format) = owned_payload(path, contract).expect("owned payload should build");
    black_box(payload);
    black_box(format);
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_owned_frame_build_only(
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
) {
    let frame =
        build_owned_frame(path, case, next_uuid(), contract).expect("owned frame should build");
    black_box(frame);
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_zc_payload_init_only(contract: &PayloadContractCase) {
    let fixture = payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
        .expect("stable fixture should initialize");
    black_box(fixture.bytes);
    black_box(fixture.encoding);
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_metadata_only(path: PayloadContractPath, case: &BenchCase, contract: &PayloadContractCase) {
    let id = next_uuid();
    let metadata = case.metadata(id);
    let encoded = NativePrefixFrameMetadataCodec
        .encode_frame_metadata(StableContainerWireFormat::metadata_context(), &metadata)
        .expect("selected-wire metadata should encode");
    let decoded = NativePrefixFrameMetadataCodec
        .decode_frame_metadata(StableContainerWireFormat::metadata_context(), &encoded)
        .expect("selected-wire metadata should decode");
    black_box(decoded);
    black_box(encoded.len());
    black_box(path.label());
    black_box(contract.name());
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_copy_ledger(path: PayloadContractPath, contract: &PayloadContractCase) {
    let metadata = BenchCase::new(contract.name()).metadata(next_uuid());
    reset_allocations();
    let encoded = NativePrefixFrameMetadataCodec
        .encode_frame_metadata(StableContainerWireFormat::metadata_context(), &metadata)
        .expect("selected-wire metadata should encode");
    let encode_allocations = allocation_sample();
    let attachment_metadata_bytes = encoded.len();
    reset_allocations();
    let attachment = ZBytes::from(encoded.clone());
    let attachment_encode_allocations = allocation_sample();
    reset_allocations();
    let decoded_attachment = attachment.to_bytes().to_vec();
    let attachment_decode_allocations = allocation_sample();
    let payload_copied = match path {
        PayloadContractPath::ProtobufOwned | PayloadContractPath::StableOwnedBytes => {
            transported_len(path, contract) * 2
        }
        PayloadContractPath::StableZcNoZero => 0,
    };
    emit_zenoh_sample(
        path,
        DiagnosticMode::ZcCopyLedger,
        contract,
        ZenohAttributionSample {
            attachment_metadata_bytes,
            metadata_copy_bytes: attachment_metadata_bytes * 2,
            payload_copy_bytes: payload_copied,
            attachment_encode_allocations: encode_allocations.allocations
                + attachment_encode_allocations.allocations,
            attachment_encode_bytes: encode_allocations.bytes + attachment_encode_allocations.bytes,
            attachment_decode_allocations: attachment_decode_allocations.allocations,
            attachment_decode_bytes: attachment_decode_allocations.bytes,
            ..ZenohAttributionSample::default()
        },
    );
    black_box(decoded_attachment);
    black_box(attachment_metadata_bytes * 2);
    black_box(payload_copied);
}

#[cfg(feature = "payload-contract-benchmarks")]
fn encoded_metadata_for(case: &BenchCase, id: UUID) -> Vec<u8> {
    NativePrefixFrameMetadataCodec
        .encode_frame_metadata(
            StableContainerWireFormat::metadata_context(),
            &case.metadata(id),
        )
        .expect("selected-wire metadata should encode")
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_zc_attachment_decode_only(
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
) {
    let encoded = encoded_metadata_for(case, next_uuid());
    let attachment = ZBytes::from(encoded.clone());
    reset_allocations();
    let decoded = attachment.to_bytes().to_vec();
    let sample = allocation_sample();
    emit_zenoh_sample(
        path,
        DiagnosticMode::ZcRxAttachmentDecodeOnly,
        contract,
        ZenohAttributionSample {
            attachment_metadata_bytes: encoded.len(),
            attachment_decode_allocations: sample.allocations,
            attachment_decode_bytes: sample.bytes,
            ..ZenohAttributionSample::default()
        },
    );
    black_box(decoded);
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_zc_adapter_filter_drop_only(
    path: PayloadContractPath,
    matching_case: &BenchCase,
    contract: &PayloadContractCase,
) {
    let nonmatching_case = BenchCase {
        authority: matching_case.authority.clone(),
        source: uri(
            &matching_case.authority,
            0x4210,
            resource_id(0xA000, next_sequence()),
        ),
    };
    let raw = BenchEncodedRxFrame::new(encoded_metadata_for(&nonmatching_case, next_uuid()), 0);
    reset_allocations();
    let frame = StableBenchRx::try_from_encoded(raw, &NativePrefixFrameMetadataCodec)
        .expect("benchmark encoded frame should decode");
    let matches = matching_case.source.matches(frame.metadata().source());
    let sample = allocation_sample();
    assert!(!matches, "nonmatching frame should be adapter-dropped");
    emit_zenoh_sample(
        path,
        DiagnosticMode::ZcRxAdapterFilterDropOnly,
        contract,
        ZenohAttributionSample {
            adapter_dropped_count: 1,
            receive_drop_allocations: sample.allocations,
            receive_drop_bytes: sample.bytes,
            ..ZenohAttributionSample::default()
        },
    );
    black_box(frame.payload_len());
}

#[cfg(feature = "payload-contract-benchmarks")]
fn run_zc_listener_dispatch_only(
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
) {
    let raw = BenchEncodedRxFrame::new(encoded_metadata_for(case, next_uuid()), 0);
    let frame = StableBenchRx::try_from_encoded(raw, &NativePrefixFrameMetadataCodec)
        .expect("benchmark encoded frame should decode");
    let ack = PayloadContractAck {
        id: frame.metadata().id().clone(),
        message_type: frame.metadata().kind().to_legacy_type(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: frame.payload_len(),
    };
    emit_zenoh_sample(
        path,
        DiagnosticMode::ZcRxListenerDispatchOnly,
        contract,
        ZenohAttributionSample {
            listener_dispatched_count: 1,
            ..ZenohAttributionSample::default()
        },
    );
    black_box(ack);
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn run_zc_rx_zenoh_delivery_only(
    transport: &Arc<StableZenohZeroCopyTransport>,
    exact_case: &BenchCase,
    nonmatching_case: &BenchCase,
    contract: &PayloadContractCase,
    exact_rx: &mut mpsc::UnboundedReceiver<RawDeliveryAck>,
    wildcard_rx: &mut mpsc::UnboundedReceiver<RawDeliveryAck>,
) {
    let exact_id = next_uuid();
    let nonmatching_id = next_uuid();
    send_selected_wire(transport, exact_case.metadata(exact_id.clone()), contract)
        .await
        .expect("exact selected-wire benchmark send should succeed");
    send_selected_wire(
        transport,
        nonmatching_case.metadata(nonmatching_id.clone()),
        contract,
    )
    .await
    .expect("nonmatching selected-wire benchmark send should succeed");

    let mut exact_deliveries = 0;
    let mut wildcard_deliveries = 0;
    let deadline = Instant::now() + BENCH_TIMEOUT;
    loop {
        while let Ok(ack) = exact_rx.try_recv() {
            if ack.id == exact_id || ack.id == nonmatching_id {
                exact_deliveries += 1;
                black_box(ack.payload_len);
            }
        }
        while let Ok(ack) = wildcard_rx.try_recv() {
            if ack.id == exact_id || ack.id == nonmatching_id {
                wildcard_deliveries += 1;
                black_box(ack.payload_len);
            }
        }
        if exact_deliveries >= 1 && wildcard_deliveries >= 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
            while let Ok(ack) = exact_rx.try_recv() {
                if ack.id == exact_id || ack.id == nonmatching_id {
                    exact_deliveries += 1;
                    black_box(ack.payload_len);
                }
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for raw Zenoh delivery attribution"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let zenoh_prefiltered_count = 2usize.saturating_sub(exact_deliveries);
    emit_zenoh_sample(
        PayloadContractPath::StableZcNoZero,
        DiagnosticMode::ZcRxZenohDeliveryOnly,
        contract,
        ZenohAttributionSample {
            publish_attempts: 2,
            exact_deliveries,
            wildcard_deliveries,
            zenoh_prefiltered_count,
            ..ZenohAttributionSample::default()
        },
    );
}

#[cfg(feature = "payload-contract-benchmarks")]
fn wildcard_source_filter() -> UUri {
    UUri::try_from_parts("*", u32::MAX, u8::MAX, u16::MAX)
        .expect("valid benchmark wildcard source filter")
}

#[cfg(feature = "payload-contract-benchmarks")]
fn emit_zenoh_sample(
    path: PayloadContractPath,
    mode: DiagnosticMode,
    contract: &PayloadContractCase,
    sample: ZenohAttributionSample,
) {
    static EMITTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

    let line = format!(
        "P51_ZENOH_SAMPLE selector={}_{} fixture={} publish_attempts={} exact_deliveries={} wildcard_deliveries={} zenoh_prefiltered_count={} adapter_dropped_count={} listener_dispatched_count={} attachment_metadata_bytes={} metadata_copy_bytes={} payload_copy_bytes={} attachment_encode_allocations={} attachment_encode_bytes={} attachment_decode_allocations={} attachment_decode_bytes={} receive_drop_allocations={} receive_drop_bytes={}",
        path.label(),
        mode.group_suffix(),
        contract.name(),
        sample.publish_attempts,
        sample.exact_deliveries,
        sample.wildcard_deliveries,
        sample.zenoh_prefiltered_count,
        sample.adapter_dropped_count,
        sample.listener_dispatched_count,
        sample.attachment_metadata_bytes,
        sample.metadata_copy_bytes,
        sample.payload_copy_bytes,
        sample.attachment_encode_allocations,
        sample.attachment_encode_bytes,
        sample.attachment_decode_allocations,
        sample.attachment_decode_bytes,
        sample.receive_drop_allocations,
        sample.receive_drop_bytes
    );
    if EMITTED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("P51 Zenoh sample emission lock should not be poisoned")
        .insert(line.clone())
    {
        eprintln!("{line}");
    }
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
        if !case_filter_allows(contract) {
            continue;
        }
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
                        diagnostic_mode != DiagnosticMode::OwnedRxNoValidation,
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
                        &transport,
                        &case,
                        contract,
                        tx,
                        diagnostic_mode != DiagnosticMode::ZcRxNoValidation,
                    ));
                    Some(transport)
                }
                PayloadContractPath::ProtobufOwned | PayloadContractPath::StableOwnedBytes => None,
            };
            let prebuilt_payload = prebuilt_owned_payload(path, contract);
            let nonmatching_case = BenchCase {
                authority: case.authority.clone(),
                source: uri(
                    &case.authority,
                    0x4210,
                    resource_id(0xA000, next_sequence()),
                ),
            };
            let mut raw_delivery_receivers =
                if diagnostic_mode == DiagnosticMode::ZcRxZenohDeliveryOnly {
                    let raw_core = runtime.block_on(build_raw_selected_wire_core(&case.authority));
                    let (exact_tx, exact_rx) = mpsc::unbounded_channel();
                    let (wildcard_tx, wildcard_rx) = mpsc::unbounded_channel();
                    runtime.block_on(register_raw_delivery_listener(
                        &raw_core,
                        &case.source,
                        exact_tx,
                    ));
                    runtime.block_on(register_raw_delivery_listener(
                        &raw_core,
                        &wildcard_source_filter(),
                        wildcard_tx,
                    ));
                    Some((raw_core, exact_rx, wildcard_rx))
                } else {
                    None
                };

            group.bench_function(
                benchmark_id(path, diagnostic_mode, contract, expected_len),
                |b| {
                    b.iter(|| match diagnostic_mode {
                        DiagnosticMode::MetadataOnly => {
                            run_metadata_only(path, &case, contract);
                        }
                        DiagnosticMode::ZcFilterOnly
                        | DiagnosticMode::ZcRxAdapterFilterDropOnly => {
                            run_zc_adapter_filter_drop_only(path, &case, contract);
                        }
                        DiagnosticMode::CopyLedger | DiagnosticMode::ZcCopyLedger => {
                            run_copy_ledger(path, contract);
                        }
                        DiagnosticMode::ZcRxAttachmentDecodeOnly => {
                            run_zc_attachment_decode_only(path, &case, contract);
                        }
                        DiagnosticMode::ZcRxListenerDispatchOnly => {
                            run_zc_listener_dispatch_only(path, &case, contract);
                        }
                        DiagnosticMode::ZcValidationOnly => {
                            run_zc_validation_only(contract);
                        }
                        DiagnosticMode::OwnedPayloadBuildOnly => {
                            run_owned_payload_build_only(path, contract);
                        }
                        DiagnosticMode::OwnedFrameBuildOnly => {
                            run_owned_frame_build_only(path, &case, contract);
                        }
                        DiagnosticMode::ZcPayloadInitOnly => {
                            run_zc_payload_init_only(contract);
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
                        | DiagnosticMode::OwnedRxNoValidation
                        | DiagnosticMode::ZcSendOnly
                        | DiagnosticMode::ZcRxOnly
                        | DiagnosticMode::ZcLoanProvenanceCheck
                        | DiagnosticMode::ZcRxNoValidation
                        | DiagnosticMode::ZcRxZenohDeliveryOnly => {
                            runtime.block_on(async {
                                if diagnostic_mode == DiagnosticMode::ZcRxZenohDeliveryOnly {
                                    let (_raw_core, exact_rx, wildcard_rx) = raw_delivery_receivers
                                        .as_mut()
                                        .expect("raw delivery receivers");
                                    run_zc_rx_zenoh_delivery_only(
                                        selected_transport
                                            .as_ref()
                                            .expect("selected-wire transport"),
                                        &case,
                                        &nonmatching_case,
                                        contract,
                                        exact_rx,
                                        wildcard_rx,
                                    )
                                    .await;
                                    return;
                                }
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
