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
    UCode, UFrameMetadata, UListener, UMessage, UMessageBuilder, UMessageType, UPayloadFormat,
    UStatus, UTransport, UUri, UUID,
};
#[cfg(feature = "payload-contract-benchmarks")]
use up_rust::{UZeroCopyListener, UZeroCopyTransport, UZeroCopyUninitTransportExt};
use up_transport_zenoh::{zenoh_config, UPTransportZenoh, ZenohRxFrame};

const BENCH_TIMEOUT: Duration = Duration::from_secs(5);
const LARGE_SENSOR_BENCH_TIMEOUT: Duration = Duration::from_secs(30);
const ZENOH_SHM_SEGMENT_SIZE: usize = 64 * 1_024 * 1_024;
const UUID_LSB_BASE: u64 = 0x8000_0000_0000_0000;
#[cfg(feature = "payload-contract-benchmarks")]
const PAYLOAD_CONTRACT_SEQUENCE: u32 = 1;

#[derive(Clone, Copy)]
enum BenchSuite {
    Raw,
    PayloadContract,
    All,
}

impl BenchSuite {
    fn from_env() -> Self {
        match std::env::var("TRANSPORT_BENCH_SUITE")
            .unwrap_or_else(|_| "raw".to_string())
            .as_str()
        {
            "raw" => Self::Raw,
            "payload-contract" => Self::PayloadContract,
            "all" => Self::All,
            other => panic!(
                "TRANSPORT_BENCH_SUITE must be one of raw, payload-contract, all; got {other}"
            ),
        }
    }

    fn includes_payload_contract(self) -> bool {
        matches!(self, Self::PayloadContract | Self::All)
    }

    fn includes_raw(self) -> bool {
        matches!(self, Self::Raw | Self::All)
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
}

struct BenchCase {
    authority: String,
    source: UUri,
}

impl BenchCase {
    fn new(payload_case_id: &str) -> Self {
        let sequence = next_sequence();
        let authority = format!(
            "zenoh-bench-{}-{payload_case_id}-{sequence}",
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
            .expect("valid payload-contract benchmark metadata message");
        UFrameMetadata::new(message.attributes().clone(), None)
            .expect("valid payload-contract benchmark metadata")
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
struct ProtobufPayloadContractAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UListener for ProtobufPayloadContractAckListener {
    async fn on_receive(&self, message: UMessage) {
        self.tx
            .send(protobuf_payload_contract_ack(&message, &self.contract))
            .expect("protobuf payload-contract benchmark receive channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct StableOwnedPayloadContractAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
    encoding: PayloadEncoding,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UListener for StableOwnedPayloadContractAckListener {
    async fn on_receive(&self, message: UMessage) {
        self.tx
            .send(stable_owned_payload_contract_ack(
                &message,
                &self.contract,
                &self.encoding,
            ))
            .expect("stable owned payload-contract benchmark receive channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct StablePayloadContractAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for StablePayloadContractAckListener {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        self.tx
            .send(stable_payload_contract_ack(&frame, &self.contract))
            .expect("stable payload-contract benchmark receive channel should remain open");
    }
}

async fn build_transport(authority: &str) -> Arc<UPTransportZenoh> {
    let transport = UPTransportZenoh::builder(authority.to_string())
        .expect("valid benchmark authority")
        .with_config(zenoh_config::Config::default())
        .with_max_listeners(512)
        .with_shm_segment_size(ZENOH_SHM_SEGMENT_SIZE)
        .expect("valid Zenoh SHM segment size")
        .build()
        .await
        .expect("Zenoh benchmark transport should build");
    Arc::new(transport)
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn register_payload_contract_listener(
    transport: &Arc<UPTransportZenoh>,
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
) {
    match path {
        PayloadContractPath::ProtobufOwned => transport
            .register_listener(
                &case.source,
                None,
                Arc::new(ProtobufPayloadContractAckListener {
                    tx,
                    contract: *contract,
                }),
            )
            .await
            .expect("protobuf payload-contract benchmark listener should register"),
        PayloadContractPath::StableOwnedBytes => {
            let encoding =
                payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                    .expect("stable owned fixture should be available")
                    .encoding;
            transport
                .register_listener(
                    &case.source,
                    None,
                    Arc::new(StableOwnedPayloadContractAckListener {
                        tx,
                        contract: *contract,
                        encoding,
                    }),
                )
                .await
                .expect("stable owned payload-contract benchmark listener should register");
        }
        PayloadContractPath::StableZcNoZero => transport
            .register_zero_copy_listener(
                &case.source,
                None,
                Arc::new(StablePayloadContractAckListener {
                    tx,
                    contract: *contract,
                }),
            )
            .await
            .expect("stable payload-contract benchmark listener should register"),
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn send_payload_contract_path(
    transport: &Arc<UPTransportZenoh>,
    path: PayloadContractPath,
    case: &BenchCase,
    id: UUID,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    match path {
        PayloadContractPath::ProtobufOwned => {
            let payload =
                payload_contract::protobuf_encoded_bytes_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                    .map_err(|error| invalid_argument(error.to_string()))?;
            let message = case.message(id, payload, UPayloadFormat::Protobuf)?;
            transport.send(message).await
        }
        PayloadContractPath::StableZcNoZero => {
            let metadata = case.metadata(id);
            send_stable_payload_contract(transport, metadata, contract).await
        }
        PayloadContractPath::StableOwnedBytes => {
            let fixture =
                payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                    .map_err(|error| invalid_argument(error.to_string()))?;
            let message = case.message(id, fixture.bytes, UPayloadFormat::Raw)?;
            transport.send(message).await
        }
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn send_stable_payload_contract(
    transport: &Arc<UPTransportZenoh>,
    metadata: UFrameMetadata,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    match contract.kind() {
        PayloadContractCaseKind::CanClassicMax => {
            transport
                .send_uninit_stable_payload_as::<CanClassicFrameV1>(metadata, |payload| {
                    payload_contract::init_can_classic_max(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::CanFdMax => {
            transport
                .send_uninit_stable_payload_as::<CanFdFrameV1>(metadata, |payload| {
                    payload_contract::init_can_fd_max(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::SomeIpSingleMtu => {
            transport
                .send_uninit_stable_payload_as::<SomeIpSignalBatchMtuV1>(metadata, |payload| {
                    payload_contract::init_someip_single_mtu(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::Streamer4k => {
            transport
                .send_uninit_stable_payload_as::<StreamChunk4kV1>(metadata, |payload| {
                    payload_contract::init_streamer_4k(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        PayloadContractCaseKind::RadarArs548DetectionList => {
            transport
                .send_uninit_stable_payload_as::<RadarDetectionListArs548V1>(metadata, |payload| {
                    payload_contract::init_radar_ars548_detection_list(
                        payload,
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                })
                .await
        }
        PayloadContractCaseKind::Streamer64k => {
            transport
                .send_uninit_stable_payload_as::<StreamChunk64kV1>(metadata, |payload| {
                    payload_contract::init_streamer_64k(payload, PAYLOAD_CONTRACT_SEQUENCE)
                })
                .await
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
            transport
                .send_uninit_stable_payload_as::<LidarPointCloudHesaiAt128V1>(metadata, |payload| {
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
                .send_uninit_stable_payload_as::<CameraBayerRggb12pFrame8mpV1>(
                    metadata,
                    |payload| {
                        payload_contract::init_camera_8mp_bayer_rggb12p(
                            payload,
                            PAYLOAD_CONTRACT_SEQUENCE,
                        )
                    },
                )
                .await
        }
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
async fn wait_for_payload_contract_ack(
    rx: &mut mpsc::UnboundedReceiver<PayloadContractAck>,
    expected_id: &UUID,
    contract: &PayloadContractCase,
    expected_transported_payload_len: usize,
    timeout: Duration,
) -> PayloadContractAck {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for matching Zenoh payload-contract frame"
        );
        let ack = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for payload-contract benchmark receive")
            .expect("payload-contract benchmark receive channel should remain open");
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
        assert_eq!(
            ack.transported_payload_len,
            expected_transported_payload_len
        );
        return ack;
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
fn protobuf_payload_contract_ack(
    message: &UMessage,
    contract: &PayloadContractCase,
) -> PayloadContractAck {
    assert_eq!(message.payload_format(), Some(UPayloadFormat::Protobuf));
    let payload = message
        .payload()
        .expect("protobuf payload-contract UMessage should carry payload bytes");
    payload_contract::validate_protobuf_bytes(contract, PAYLOAD_CONTRACT_SEQUENCE, payload)
        .expect("protobuf payload-contract frame should validate");
    PayloadContractAck {
        id: message.id().clone(),
        message_type: message.type_(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: payload.len(),
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
fn stable_owned_payload_contract_ack(
    message: &UMessage,
    contract: &PayloadContractCase,
    encoding: &PayloadEncoding,
) -> PayloadContractAck {
    assert_eq!(message.payload_format(), Some(UPayloadFormat::Raw));
    let payload = message
        .payload()
        .expect("stable owned payload-contract UMessage should carry payload bytes");
    // Ordinary UTransport cannot carry native custom stable-container encoding;
    // this harness validates the raw copied bytes against the known fixture encoding.
    payload_contract::validate_stable_owned_bytes(
        contract,
        PAYLOAD_CONTRACT_SEQUENCE,
        Some(encoding),
        payload,
    )
    .expect("stable owned payload-contract frame should validate");
    PayloadContractAck {
        id: message.id().clone(),
        message_type: message.type_(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: payload.len(),
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
fn stable_payload_contract_ack(
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

#[cfg(feature = "payload-contract-benchmarks")]
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

#[cfg(feature = "payload-contract-benchmarks")]
fn payload_contract_transported_len(
    path: PayloadContractPath,
    contract: &PayloadContractCase,
) -> usize {
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
fn bench_payload_contract_matrix(
    c: &mut Criterion,
    group_name: &'static str,
    payload_cases: &[PayloadContractCase],
    timeout: Duration,
) {
    let runtime = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group(group_name);
    for contract in payload_cases {
        for path in [
            PayloadContractPath::ProtobufOwned,
            PayloadContractPath::StableZcNoZero,
            PayloadContractPath::StableOwnedBytes,
        ] {
            let case = BenchCase::new(contract.name());
            let transported_payload_len = payload_contract_transported_len(path, contract);
            let transport = runtime.block_on(build_transport(&case.authority));
            let (tx, mut rx) = mpsc::unbounded_channel();
            runtime.block_on(register_payload_contract_listener(
                &transport, path, &case, contract, tx,
            ));

            group.bench_function(
                BenchmarkId::new(
                    path.label(),
                    format!(
                        "publish/{}/{}/{}",
                        contract.name(),
                        contract.semantic_reference_len(),
                        transported_payload_len
                    ),
                ),
                |b| {
                    b.iter(|| {
                        runtime.block_on(async {
                            let id = next_uuid();
                            send_payload_contract_path(
                                &transport,
                                path,
                                &case,
                                id.clone(),
                                contract,
                            )
                            .await
                            .expect("payload-contract benchmark send should succeed");
                            let ack = wait_for_payload_contract_ack(
                                &mut rx,
                                &id,
                                contract,
                                transported_payload_len,
                                timeout,
                            )
                            .await;
                            black_box(ack.semantic_reference_len);
                            black_box(ack.transported_payload_len);
                            black_box(contract.name());
                        });
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_transport(c: &mut Criterion) {
    UPTransportZenoh::try_init_log_from_env();
    let suite_explicit = std::env::var_os("TRANSPORT_BENCH_SUITE").is_some();
    let suite = BenchSuite::from_env();
    let profile = BenchProfile::from_env();
    if suite.includes_raw() {
        assert!(
            !suite_explicit,
            "Phase 06C1 Zenoh harness supports TRANSPORT_BENCH_SUITE=payload-contract only"
        );
        return;
    }
    if suite.includes_payload_contract() {
        bench_payload_contract(c, profile);
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
fn bench_payload_contract(c: &mut Criterion, profile: BenchProfile) {
    if profile.includes_core() {
        bench_payload_contract_matrix(
            c,
            "transport_payload_contract_core",
            payload_contract::core_cases(),
            BENCH_TIMEOUT,
        );
    }
    if profile.includes_camera() {
        bench_payload_contract_matrix(
            c,
            "transport_payload_contract_large_sensor",
            payload_contract::large_sensor_cases(),
            LARGE_SENSOR_BENCH_TIMEOUT,
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
