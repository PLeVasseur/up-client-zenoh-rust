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
    ULoanedContiguousZeroCopyRxFrame,
};
use up_rust::{
    NativePrefixFrameMetadataCodec, NativeProfile, NativeProfileAgreement, NativeProfileMode,
    NativeProfileTable, PayloadEncoding, PublishBuilderState, StableContainerWireFormat, UCode,
    UFrameMetadata, UHasWire, UListener, UMessage, UMessageBuilder, UMessageType, UStatus,
    UTransport, UUri, UWireRx, UWireTransport, UWithNativePrefixWire, UZeroCopyListener,
    UZeroCopyTransportImpl, UUID,
};
use up_transport_zenoh::{zenoh_config, UPTransportZenoh, ZenohRxFrame, ZenohZeroCopyCore};

const BENCH_TIMEOUT: Duration = Duration::from_secs(5);
const LARGE_SENSOR_BENCH_TIMEOUT: Duration = Duration::from_secs(30);
const ZENOH_SHM_SEGMENT_SIZE: usize = 64 * 1_024 * 1_024;
const UUID_LSB_BASE: u64 = 0x8000_0000_0000_0000;
#[cfg(feature = "payload-contract-benchmarks")]
const PAYLOAD_CONTRACT_SEQUENCE: u32 = 1;

type StableZenohTransport =
    UWireTransport<ZenohZeroCopyCore, StableContainerWireFormat, NativePrefixFrameMetadataCodec>;
type StableZenohRx =
    UWireRx<ZenohRxFrame, StableContainerWireFormat, NativePrefixFrameMetadataCodec>;

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
    native_profile: NativeProfileAgreement,
}

impl BenchCase {
    fn new(contract: &PayloadContractCase) -> Self {
        let fixture_name = contract.name();
        let sequence = next_sequence();
        let authority = format!(
            "zenoh-userializer-bench-{}-{fixture_name}-{sequence}",
            std::process::id()
        );
        let source = uri(&authority, 0x4210, resource_id(0x9000, sequence));
        // This same-process benchmark configures both peers from one explicit
        // deployment table, outside the measured send/receive operation.
        let encoding =
            PayloadEncoding::from_id(0xF100 + contract.case_id()).expect("private benchmark ID");
        let representation = payload_contract::stable_payload_representation(contract)
            .expect("native fixture representation");
        let profile = NativeProfile::new(
            format!("zenoh-payload-benchmark.{fixture_name}"),
            1,
            NativeProfileMode::Table(
                NativeProfileTable::new([(encoding, representation)]).unwrap(),
            ),
        )
        .unwrap();
        let native_profile =
            NativeProfileAgreement::new(Arc::new(profile.clone()), &profile).unwrap();
        Self {
            authority,
            source,
            native_profile,
        }
    }

    fn message_builder(&self, id: UUID) -> UMessageBuilder<PublishBuilderState> {
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
        format: PayloadEncoding,
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
    native_profile: Option<NativeProfileAgreement>,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UListener for OwnedAckListener {
    async fn on_receive(&self, message: UMessage) {
        self.tx
            .send(owned_ack(
                &message,
                &self.contract,
                self.path,
                self.native_profile.as_ref(),
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
impl UZeroCopyListener<StableZenohRx> for SelectedWireAckListener {
    async fn on_receive_zero_copy(&self, frame: StableZenohRx) {
        self.tx
            .send(selected_wire_ack(&frame, &self.contract))
            .expect("selected-wire benchmark receive channel should remain open");
    }
}

async fn build_owned_transport(authority: &str) -> Arc<UPTransportZenoh> {
    Arc::new(
        UPTransportZenoh::new(
            zenoh_config::Config::default(),
            format!("//{authority}/4210/1/0"),
        )
        .await
        .expect("Zenoh owned benchmark transport should build"),
    )
}

async fn build_selected_wire_transport(
    authority: &str,
    profile: &NativeProfileAgreement,
) -> Arc<StableZenohTransport> {
    let core = ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/0"))
        .with_config(zenoh_config::Config::default())
        .with_shm_segment_size(ZENOH_SHM_SEGMENT_SIZE)
        .expect("valid Zenoh SHM segment size")
        .build()
        .await
        .expect("Zenoh selected-wire benchmark core should build");
    Arc::new(core.into_stable_container_transport(profile.clone()))
}

async fn register_owned_listener(
    transport: &Arc<UPTransportZenoh>,
    path: PayloadContractPath,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
) {
    let native_profile = match path {
        PayloadContractPath::StableOwnedBytes => Some(case.native_profile.clone()),
        PayloadContractPath::ProtobufOwned => None,
        PayloadContractPath::StableZcNoZero => {
            unreachable!("selected-wire path uses zero-copy listener")
        }
    };
    transport
        .register_listener(
            &case.source,
            None,
            Arc::new(OwnedAckListener {
                tx,
                contract: *contract,
                path,
                native_profile,
            }),
        )
        .await
        .expect("owned benchmark listener should register");
}

async fn register_selected_wire_listener(
    transport: &Arc<StableZenohTransport>,
    case: &BenchCase,
    contract: &PayloadContractCase,
    tx: mpsc::UnboundedSender<PayloadContractAck>,
) {
    transport
        .register_validated_zero_copy_listener(
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
    transport: &Arc<UPTransportZenoh>,
    path: PayloadContractPath,
    case: &BenchCase,
    id: UUID,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    let (payload, format) = match path {
        PayloadContractPath::ProtobufOwned => (
            payload_contract::protobuf_encoded_bytes_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                .map_err(|error| invalid_argument(error.to_string()))?,
            PayloadEncoding::PROTOBUF,
        ),
        PayloadContractPath::StableOwnedBytes => {
            let fixture = payload_contract::stable_owned_fixture_for(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                &case.native_profile,
            )
            .map_err(|error| invalid_argument(error.to_string()))?;
            (fixture.bytes, fixture.identity.encoding())
        }
        PayloadContractPath::StableZcNoZero => {
            unreachable!("selected-wire path uses zero-copy send")
        }
    };
    transport.send(case.message(id, payload, format)?).await
}

async fn send_selected_wire(
    transport: &Arc<StableZenohTransport>,
    metadata: UFrameMetadata,
    contract: &PayloadContractCase,
) -> Result<(), UStatus> {
    let profile = transport
        .native_profile()
        .expect("configured native benchmark profile");
    let representation = payload_contract::stable_payload_representation(contract)
        .map_err(|error| invalid_argument(error.to_string()))?;
    let identity = profile
        .identity_for(&representation)
        .map_err(|error| invalid_argument(error.to_string()))?;
    let metadata = metadata
        .with_native_payload_identity(identity)
        .map_err(|error| invalid_argument(error.to_string()))?;
    match contract.kind() {
        PayloadContractCaseKind::CanClassicMax => {
            transport
                .send_stable_payload::<CanClassicFrameV1, _>(metadata, |payload| {
                    payload_contract::init_can_classic_max(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("CAN classic fixture initialization")
                })
                .await
        }
        PayloadContractCaseKind::CanFdMax => {
            transport
                .send_stable_payload::<CanFdFrameV1, _>(metadata, |payload| {
                    payload_contract::init_can_fd_max(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("CAN FD fixture initialization")
                })
                .await
        }
        PayloadContractCaseKind::SomeIpSingleMtu => {
            transport
                .send_stable_payload::<SomeIpSignalBatchMtuV1, _>(metadata, |payload| {
                    payload_contract::init_someip_single_mtu(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("SOME/IP fixture initialization")
                })
                .await
        }
        PayloadContractCaseKind::Streamer4k => {
            transport
                .send_stable_payload::<StreamChunk4kV1, _>(metadata, |payload| {
                    payload_contract::init_streamer_4k(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("4K stream fixture initialization")
                })
                .await
        }
        PayloadContractCaseKind::RadarArs548DetectionList => {
            transport
                .send_stable_payload::<RadarDetectionListArs548V1, _>(metadata, |payload| {
                    payload_contract::init_radar_ars548_detection_list(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("radar fixture initialization")
                })
                .await
        }
        PayloadContractCaseKind::Streamer64k => {
            transport
                .send_stable_payload::<StreamChunk64kV1, _>(metadata, |payload| {
                    payload_contract::init_streamer_64k(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("64K stream fixture initialization")
                })
                .await
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
            transport
                .send_stable_payload::<LidarPointCloudHesaiAt128V1, _>(metadata, |payload| {
                    payload_contract::init_lidar_hesai_at128_point_cloud(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("lidar fixture initialization")
                })
                .await
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::Camera8mpBayerRggb12p => {
            transport
                .send_stable_payload::<CameraBayerRggb12pFrame8mpV1, _>(metadata, |payload| {
                    payload_contract::init_camera_8mp_bayer_rggb12p(
                        payload.into_initializer(),
                        PAYLOAD_CONTRACT_SEQUENCE,
                    )
                    .expect("camera fixture initialization")
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
    message: &UMessage,
    contract: &PayloadContractCase,
    path: PayloadContractPath,
    native_profile: Option<&NativeProfileAgreement>,
) -> PayloadContractAck {
    let payload = message.payload().expect("owned benchmark frame payload");
    match path {
        PayloadContractPath::ProtobufOwned => {
            assert_eq!(message.payload_encoding(), Some(PayloadEncoding::PROTOBUF));
            payload_contract::validate_protobuf_bytes(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                &payload,
            )
            .expect("protobuf payload-contract frame should validate");
        }
        PayloadContractPath::StableOwnedBytes => {
            let profile = native_profile.expect("configured native benchmark agreement");
            let metadata = up_rust::frame::metadata::try_project_umessage_to_frame_metadata_with_native_profile(message, profile)
                .expect("recover identity from actual carried ID and agreed profile");
            payload_contract::validate_stable_owned_bytes(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                &metadata,
                profile,
                &payload,
            )
            .expect("stable owned payload-contract frame should validate");
        }
        PayloadContractPath::StableZcNoZero => {
            unreachable!("selected-wire path uses zero-copy listener")
        }
    }
    PayloadContractAck {
        id: message.id().clone(),
        message_type: message.type_(),
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
    let profile = frame
        .native_profile()
        .expect("receive view retained its native profile");
    match contract.kind() {
        PayloadContractCaseKind::CanClassicMax => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<CanClassicFrameV1>(profile)
                .expect("CAN Classic stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::CanFdMax => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<CanFdFrameV1>(profile)
                .expect("CAN FD stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::SomeIpSingleMtu => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<SomeIpSignalBatchMtuV1>(profile)
                .expect("SOME/IP stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::Streamer4k => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<StreamChunk4kV1>(profile)
                .expect("stream 4K stable payload-contract frame should borrow"),
        ),
        PayloadContractCaseKind::RadarArs548DetectionList => {
            payload_contract::validate_stable_payload(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                frame
                    .borrow_stable_payload::<RadarDetectionListArs548V1>(profile)
                    .expect("radar stable payload-contract frame should borrow"),
            )
        }
        PayloadContractCaseKind::Streamer64k => payload_contract::validate_stable_payload(
            contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame
                .borrow_stable_payload::<StreamChunk64kV1>(profile)
                .expect("stream 64K stable payload-contract frame should borrow"),
        ),
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
            payload_contract::validate_stable_payload(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                frame
                    .borrow_stable_payload::<LidarPointCloudHesaiAt128V1>(profile)
                    .expect("LiDAR stable payload-contract frame should borrow"),
            )
        }
        #[cfg(feature = "payload-contract-large-benchmarks")]
        PayloadContractCaseKind::Camera8mpBayerRggb12p => {
            payload_contract::validate_stable_payload(
                contract,
                PAYLOAD_CONTRACT_SEQUENCE,
                frame
                    .borrow_stable_payload::<CameraBayerRggb12pFrame8mpV1>(profile)
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

fn bench_payload_contract_matrix(
    c: &mut Criterion,
    group_name: &'static str,
    payload_cases: &[PayloadContractCase],
    measurement_time: Duration,
) {
    let runtime = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group(group_name);
    group.measurement_time(measurement_time);
    for contract in payload_cases {
        for path in [
            PayloadContractPath::ProtobufOwned,
            PayloadContractPath::StableZcNoZero,
            PayloadContractPath::StableOwnedBytes,
        ] {
            let case = BenchCase::new(contract);
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
                    let transport = runtime.block_on(build_selected_wire_transport(
                        &case.authority,
                        &case.native_profile,
                    ));
                    runtime.block_on(register_selected_wire_listener(
                        &transport, &case, contract, tx,
                    ));
                    Some(transport)
                }
                PayloadContractPath::ProtobufOwned | PayloadContractPath::StableOwnedBytes => None,
            };

            group.bench_function(
                BenchmarkId::new(
                    path.label(),
                    format!(
                        "publish/{}/{}/{}",
                        contract.name(),
                        contract.semantic_reference_len(),
                        expected_len
                    ),
                ),
                |b| {
                    b.iter(|| {
                        runtime.block_on(async {
                            let id = next_uuid();
                            match path {
                                PayloadContractPath::ProtobufOwned
                                | PayloadContractPath::StableOwnedBytes => {
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
                            let ack = wait_for_ack(&mut rx, &id, contract, expected_len).await;
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
