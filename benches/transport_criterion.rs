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

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

use std::{
    cmp,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion,
};
use tokio::{runtime::Runtime, sync::mpsc, time::Instant};
#[cfg(feature = "payload-contract-benchmarks")]
use up_rust::{
    bench_fixtures::payload_contract::{self as payload_contract, *},
    zero_copy::ULoanedContiguousZeroCopyRxFrame,
    ProtobufPayload,
};
use up_rust::{
    payload::{PayloadLayout, RawBytes, UWireError},
    zero_copy::{
        LoanedUninitByteWriter, UFrameView, UTxBuffer, UTxLoanSpec, UUninitTxBuffer,
        UZeroCopyListener, UZeroCopyTransport, UZeroCopyUninitTransport,
        UZeroCopyUninitTransportExt,
    },
    UFrameBuilder, UFrameMetadata, UMessageType, UOwnedFrame, UOwnedListener, UOwnedTransport,
    UStatus, UUri, UUID,
};
use up_transport_zenoh::{zenoh_config, UPTransportZenoh, ZenohRxFrame};

const CORE_PAYLOAD_CASES: &[(&str, usize)] = &[
    ("empty_present", 0),
    ("can_classic_max", 8),
    ("can_fd_max", 64),
    ("someip_single_mtu", 1_456),
    ("streamer_4k", 4 * 1_024),
    ("radar_ars548_detection_list", 35_336),
    ("streamer_64k", 64 * 1_024),
];
const LARGE_SENSOR_PAYLOAD_CASES: &[(&str, usize)] =
    &[("camera_8mp_3840x2160_raw12_packed", 12_441_600)];
const BENCH_TIMEOUT: Duration = Duration::from_secs(5);
const LARGE_SENSOR_BENCH_TIMEOUT: Duration = Duration::from_secs(30);
const ZENOH_SHM_SEGMENT_SIZE: usize = 64 * 1_024 * 1_024;
const DIRECT_WRITE_CHUNK: usize = 8 * 1_024;
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

    fn includes_raw(self) -> bool {
        matches!(self, Self::Raw | Self::All)
    }

    fn includes_payload_contract(self) -> bool {
        matches!(self, Self::PayloadContract | Self::All)
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

#[derive(Clone, Copy)]
enum BenchPath {
    Owned,
    ZeroCopyLoanCopy,
    ZeroCopyUninitDirect,
}

impl BenchPath {
    fn label(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::ZeroCopyLoanCopy => "zero_copy_loan_copy",
            Self::ZeroCopyUninitDirect => "zero_copy_uninit_direct",
        }
    }

    fn needs_zero_copy_listener(self) -> bool {
        matches!(self, Self::ZeroCopyLoanCopy | Self::ZeroCopyUninitDirect)
    }
}

#[derive(Clone, Copy)]
enum BenchMessageType {
    Publish,
    Notification,
    Request,
    Response,
}

impl BenchMessageType {
    fn label(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Notification => "notification",
            Self::Request => "request",
            Self::Response => "response",
        }
    }

    fn message_type(self) -> UMessageType {
        match self {
            Self::Publish => UMessageType::Publish,
            Self::Notification => UMessageType::Notification,
            Self::Request => UMessageType::Request,
            Self::Response => UMessageType::Response,
        }
    }
}

struct BenchCase {
    message_type: BenchMessageType,
    payload_case_id: &'static str,
    payload_len: usize,
    authority: String,
    source: UUri,
    sink: Option<UUri>,
    request_id: Option<UUID>,
}

impl BenchCase {
    fn new(
        message_type: BenchMessageType,
        payload_case_id: &'static str,
        payload_len: usize,
    ) -> Self {
        let sequence = next_sequence();
        let authority = format!("zenoh-bench-{}-{sequence}", std::process::id());
        let source_resource = resource_id(0x9000, sequence);
        let method_resource = resource_id(0x1000, sequence);
        match message_type {
            BenchMessageType::Publish => {
                let source = uri(&authority, 0x4210, source_resource);
                Self {
                    message_type,
                    payload_case_id,
                    payload_len,
                    authority,
                    source,
                    sink: None,
                    request_id: None,
                }
            }
            BenchMessageType::Notification => {
                let source = uri(&authority, 0x4211, source_resource);
                let sink = uri(&authority, 0x4220, 0);
                Self {
                    message_type,
                    payload_case_id,
                    payload_len,
                    authority,
                    source,
                    sink: Some(sink),
                    request_id: None,
                }
            }
            BenchMessageType::Request => {
                let reply_to = uri(&authority, 0x4300, 0);
                let method = uri(&authority, 0x4310, method_resource);
                Self {
                    message_type,
                    payload_case_id,
                    payload_len,
                    authority,
                    source: reply_to,
                    sink: Some(method),
                    request_id: None,
                }
            }
            BenchMessageType::Response => {
                let invoked_method = uri(&authority, 0x4310, method_resource);
                let reply_to = uri(&authority, 0x4300, 0);
                Self {
                    message_type,
                    payload_case_id,
                    payload_len,
                    authority,
                    source: invoked_method,
                    sink: Some(reply_to),
                    request_id: Some(uuid_for(sequence.saturating_add(10_000))),
                }
            }
        }
    }

    fn benchmark_id(&self, path: BenchPath) -> BenchmarkId {
        BenchmarkId::new(
            path.label(),
            format!(
                "{}/{}/{}",
                self.message_type.label(),
                self.payload_case_id,
                self.payload_len
            ),
        )
    }

    fn no_payload_benchmark_id(&self, path: BenchPath) -> BenchmarkId {
        BenchmarkId::new(path.label(), self.message_type.label())
    }

    fn builder(&self, id: UUID) -> UFrameBuilder {
        match self.message_type {
            BenchMessageType::Publish => UFrameBuilder::publish(self.source.clone()),
            BenchMessageType::Notification => UFrameBuilder::notification(
                self.source.clone(),
                self.sink.clone().expect("notification sink"),
            ),
            BenchMessageType::Request => UFrameBuilder::request(
                self.sink.clone().expect("request method"),
                self.source.clone(),
                5_000,
            ),
            BenchMessageType::Response => UFrameBuilder::response(
                self.sink.clone().expect("response reply-to"),
                self.request_id.clone().expect("response request id"),
                self.source.clone(),
            ),
        }
        .with_message_id(id)
    }

    fn metadata(&self, id: UUID, payload_present: bool) -> UFrameMetadata {
        let builder = self.builder(id);
        if payload_present {
            builder.with_encoding(RawBytes::encoding()).build_metadata()
        } else {
            builder.build_metadata()
        }
        .expect("valid benchmark metadata")
    }

    fn owned_frame(
        &self,
        id: UUID,
        payload: &PreparedPayload,
        payload_present: bool,
    ) -> UOwnedFrame {
        let builder = self.builder(id);
        if payload_present {
            builder
                .build_with_raw_payload(payload.bytes().expect("precomputed payload bytes").clone())
                .expect("valid owned benchmark frame")
        } else {
            builder.build().expect("valid no-payload benchmark frame")
        }
    }
}

struct PreparedPayload {
    bytes: Option<Bytes>,
    len: usize,
    checksum: u64,
}

impl PreparedPayload {
    fn precomputed(len: usize) -> Self {
        let mut payload = vec![0_u8; len];
        fill_pattern(&mut payload, 0);
        let checksum = checksum_bytes(0, &payload);
        Self {
            bytes: Some(Bytes::from(payload)),
            len,
            checksum,
        }
    }

    fn direct(len: usize) -> Self {
        Self {
            bytes: None,
            len,
            checksum: checksum_for_len(len),
        }
    }

    fn no_payload_for(path: BenchPath) -> Self {
        match path {
            BenchPath::ZeroCopyUninitDirect => Self::direct(0),
            BenchPath::Owned | BenchPath::ZeroCopyLoanCopy => Self::precomputed(0),
        }
    }

    fn for_path(path: BenchPath, len: usize) -> Self {
        match path {
            BenchPath::ZeroCopyUninitDirect => Self::direct(len),
            BenchPath::Owned | BenchPath::ZeroCopyLoanCopy => Self::precomputed(len),
        }
    }

    fn bytes(&self) -> Option<&Bytes> {
        self.bytes.as_ref()
    }
}

struct ReceivedAck {
    id: UUID,
    message_type: UMessageType,
    has_payload: bool,
    payload_len: usize,
    checksum: u64,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[derive(Clone, Copy)]
enum PayloadContractPath {
    ProtobufOwnedFull,
    StableZcNoZeroFull,
    StableOwnedBytesFull,
}

#[cfg(feature = "payload-contract-benchmarks")]
impl PayloadContractPath {
    fn label(self) -> &'static str {
        match self {
            Self::ProtobufOwnedFull => "protobuf_owned_full",
            Self::StableZcNoZeroFull => "stable_zc_nozero_full",
            Self::StableOwnedBytesFull => "stable_owned_bytes_full",
        }
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

struct OwnedAckListener(mpsc::UnboundedSender<ReceivedAck>);

#[async_trait]
impl UOwnedListener for OwnedAckListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        let checksum = checksum_bytes(0, frame.payload_bytes());
        self.0
            .send(ReceivedAck {
                id: frame.metadata().attributes().id().clone(),
                message_type: frame.metadata().attributes().message_type(),
                has_payload: frame.has_payload(),
                payload_len: frame.payload_bytes().len(),
                checksum,
            })
            .expect("owned benchmark receive channel should remain open");
    }
}

struct ZeroCopyAckListener(mpsc::UnboundedSender<ReceivedAck>);

#[async_trait]
impl UZeroCopyListener<ZenohRxFrame> for ZeroCopyAckListener {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        let checksum = frame.payload_slices().fold(0_u64, checksum_bytes);
        self.0
            .send(ReceivedAck {
                id: frame.metadata().attributes().id().clone(),
                message_type: frame.metadata().attributes().message_type(),
                has_payload: frame.has_payload(),
                payload_len: frame.payload_len(),
                checksum,
            })
            .expect("zero-copy benchmark receive channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct ProtobufPayloadContractAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UOwnedListener for ProtobufPayloadContractAckListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.tx
            .send(protobuf_payload_contract_ack(frame, &self.contract))
            .expect("protobuf payload-contract benchmark receive channel should remain open");
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
struct StableOwnedPayloadContractAckListener {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
}

#[cfg(feature = "payload-contract-benchmarks")]
#[async_trait]
impl UOwnedListener for StableOwnedPayloadContractAckListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.tx
            .send(stable_owned_payload_contract_ack(frame, &self.contract))
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

async fn register_listener(
    transport: &Arc<UPTransportZenoh>,
    path: BenchPath,
    case: &BenchCase,
    tx: mpsc::UnboundedSender<ReceivedAck>,
) {
    if path.needs_zero_copy_listener() {
        transport
            .register_zero_copy_listener(
                &case.source,
                case.sink.as_ref(),
                Arc::new(ZeroCopyAckListener(tx)),
            )
            .await
            .expect("zero-copy benchmark listener should register");
    } else {
        transport
            .register_owned_listener(
                &case.source,
                case.sink.as_ref(),
                Arc::new(OwnedAckListener(tx)),
            )
            .await
            .expect("owned benchmark listener should register");
    }
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
        PayloadContractPath::ProtobufOwnedFull => transport
            .register_owned_listener(
                &case.source,
                case.sink.as_ref(),
                Arc::new(ProtobufPayloadContractAckListener {
                    tx,
                    contract: *contract,
                }),
            )
            .await
            .expect("protobuf payload-contract benchmark listener should register"),
        PayloadContractPath::StableOwnedBytesFull => transport
            .register_owned_listener(
                &case.source,
                case.sink.as_ref(),
                Arc::new(StableOwnedPayloadContractAckListener {
                    tx,
                    contract: *contract,
                }),
            )
            .await
            .expect("stable owned payload-contract benchmark listener should register"),
        PayloadContractPath::StableZcNoZeroFull => transport
            .register_zero_copy_listener(
                &case.source,
                case.sink.as_ref(),
                Arc::new(StablePayloadContractAckListener {
                    tx,
                    contract: *contract,
                }),
            )
            .await
            .expect("stable payload-contract benchmark listener should register"),
    }
}

async fn send_path(
    transport: &Arc<UPTransportZenoh>,
    path: BenchPath,
    case: &BenchCase,
    id: UUID,
    payload: &PreparedPayload,
    payload_present: bool,
) -> Result<(), UStatus> {
    match path {
        BenchPath::Owned => {
            let frame = case.owned_frame(id, payload, payload_present);
            transport.send_owned(frame).await
        }
        BenchPath::ZeroCopyLoanCopy => {
            let metadata = case.metadata(id, payload_present);
            let mut loan = transport
                .loan_tx(loan_spec(metadata, payload.len, payload_present)?)
                .await?;
            if payload_present {
                loan.payload_mut()
                    .copy_from_slice(payload.bytes().expect("precomputed payload bytes"));
            }
            transport.send_zero_copy(loan).await
        }
        BenchPath::ZeroCopyUninitDirect => {
            let metadata = case.metadata(id, payload_present);
            if payload_present {
                transport
                    .send_uninit_loaned_bytes_as::<RawBytes>(metadata, payload.len, 1, |writer| {
                        write_pattern_to_uninit_writer(writer)
                    })
                    .await
            } else {
                let loan = transport
                    .loan_uninit_tx(UTxLoanSpec::no_payload(metadata)?)
                    .await?;
                // SAFETY: a no-payload loan has an empty visible payload range, so there are
                // no application bytes left for the benchmark to initialize before commit.
                let loan = unsafe { loan.assume_payload_init() };
                transport.send_zero_copy(loan).await
            }
        }
    }
}

async fn wait_for_ack(
    rx: &mut mpsc::UnboundedReceiver<ReceivedAck>,
    expected_id: &UUID,
    expected_type: UMessageType,
    payload_present: bool,
    expected_len: usize,
    expected_checksum: u64,
    timeout: Duration,
) -> ReceivedAck {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for matching benchmark frame"
        );
        let ack = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for benchmark receive")
            .expect("benchmark receive channel should remain open");
        if &ack.id != expected_id {
            continue;
        }
        assert_eq!(ack.message_type, expected_type);
        assert_eq!(ack.has_payload, payload_present);
        assert_eq!(ack.payload_len, expected_len);
        assert_eq!(ack.checksum, expected_checksum);
        return ack;
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
        PayloadContractPath::ProtobufOwnedFull => {
            let payload =
                payload_contract::protobuf_encoded_bytes_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                    .map_err(UStatus::from)?;
            let metadata = case.builder(id).build_metadata().expect("valid metadata");
            let frame = UOwnedFrame::with_payload_unchecked(
                metadata.with_encoding(ProtobufPayload::encoding()),
                payload,
            );
            transport.send_owned(frame).await
        }
        PayloadContractPath::StableZcNoZeroFull => {
            let metadata = case.builder(id).build_metadata().expect("valid metadata");
            send_stable_payload_contract(transport, metadata, contract).await
        }
        PayloadContractPath::StableOwnedBytesFull => {
            let fixture =
                payload_contract::stable_owned_fixture_for(contract, PAYLOAD_CONTRACT_SEQUENCE)
                    .map_err(UStatus::from)?;
            let metadata = case.builder(id).build_metadata().expect("valid metadata");
            let frame = UOwnedFrame::with_payload_unchecked(
                metadata.with_encoding(fixture.encoding),
                fixture.bytes,
            );
            transport.send_owned(frame).await
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
    frame: UOwnedFrame,
    contract: &PayloadContractCase,
) -> PayloadContractAck {
    let transported_payload_len = frame.payload_bytes().len();
    let id = frame.metadata().attributes().id().clone();
    let message_type = frame.metadata().attributes().message_type();
    payload_contract::validate_protobuf_bytes(
        contract,
        PAYLOAD_CONTRACT_SEQUENCE,
        frame.payload_bytes(),
    )
    .expect("protobuf payload-contract frame should validate");
    PayloadContractAck {
        id,
        message_type,
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len,
    }
}

#[cfg(feature = "payload-contract-benchmarks")]
fn stable_owned_payload_contract_ack(
    frame: UOwnedFrame,
    contract: &PayloadContractCase,
) -> PayloadContractAck {
    payload_contract::validate_stable_owned_bytes(
        contract,
        PAYLOAD_CONTRACT_SEQUENCE,
        frame.metadata().encoding(),
        frame.payload_bytes(),
    )
    .expect("stable owned payload-contract frame should validate");
    PayloadContractAck {
        id: frame.metadata().attributes().id().clone(),
        message_type: frame.metadata().attributes().message_type(),
        case_id: contract.case_id(),
        sequence: PAYLOAD_CONTRACT_SEQUENCE,
        semantic_reference_len: contract.semantic_reference_len(),
        transported_payload_len: frame.payload_bytes().len(),
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
        message_type: frame.metadata().attributes().message_type(),
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
        PayloadContractPath::ProtobufOwnedFull => {
            payload_contract::protobuf_encoded_len(contract, PAYLOAD_CONTRACT_SEQUENCE)
        }
        PayloadContractPath::StableZcNoZeroFull | PayloadContractPath::StableOwnedBytesFull => {
            payload_contract::stable_payload_len(contract)
        }
    }
}

fn loan_spec(
    metadata: UFrameMetadata,
    payload_len: usize,
    payload_present: bool,
) -> Result<UTxLoanSpec, UStatus> {
    if !payload_present {
        return UTxLoanSpec::no_payload(metadata);
    }
    if payload_len == 0 {
        return UTxLoanSpec::present_empty_payload(metadata);
    }
    let layout = PayloadLayout::new(payload_len, 1).map_err(UStatus::from)?;
    UTxLoanSpec::payload(metadata, layout)
}

fn write_pattern_to_uninit_writer<'a>(
    mut writer: LoanedUninitByteWriter<'a>,
) -> Result<LoanedUninitByteWriter<'a>, UWireError> {
    let mut offset = 0;
    let mut chunk = [0_u8; DIRECT_WRITE_CHUNK];
    while offset < writer.len() {
        let take = cmp::min(chunk.len(), writer.len() - offset);
        fill_pattern(&mut chunk[..take], offset);
        writer.write_all(&chunk[..take])?;
        offset += take;
    }
    Ok(writer)
}

fn bench_payload_matrix(
    c: &mut Criterion,
    group_name: &'static str,
    payload_cases: &[(&'static str, usize)],
    timeout: Duration,
    send_receive: bool,
) {
    let runtime = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group(group_name);
    for (payload_case_id, payload_len) in payload_cases {
        for path in [
            BenchPath::Owned,
            BenchPath::ZeroCopyLoanCopy,
            BenchPath::ZeroCopyUninitDirect,
        ] {
            for message_type in [
                BenchMessageType::Publish,
                BenchMessageType::Notification,
                BenchMessageType::Request,
                BenchMessageType::Response,
            ] {
                if send_receive {
                    bench_send_receive_case(
                        &runtime,
                        &mut group,
                        path,
                        message_type,
                        payload_case_id,
                        *payload_len,
                        timeout,
                    );
                } else {
                    bench_tx_only_case(
                        &runtime,
                        &mut group,
                        path,
                        message_type,
                        payload_case_id,
                        *payload_len,
                    );
                }
            }
        }
    }
    group.finish();
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
            PayloadContractPath::ProtobufOwnedFull,
            PayloadContractPath::StableZcNoZeroFull,
            PayloadContractPath::StableOwnedBytesFull,
        ] {
            let case = BenchCase::new(
                BenchMessageType::Publish,
                contract.name(),
                contract.semantic_reference_len(),
            );
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

fn bench_send_receive_case(
    runtime: &Runtime,
    group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>,
    path: BenchPath,
    message_type: BenchMessageType,
    payload_case_id: &'static str,
    payload_len: usize,
    timeout: Duration,
) {
    let case = BenchCase::new(message_type, payload_case_id, payload_len);
    let payload = PreparedPayload::for_path(path, payload_len);
    let transport = runtime.block_on(build_transport(&case.authority));
    let (tx, mut rx) = mpsc::unbounded_channel();
    runtime.block_on(register_listener(&transport, path, &case, tx));

    group.bench_function(case.benchmark_id(path), |b| {
        b.iter(|| {
            runtime.block_on(async {
                let id = next_uuid();
                send_path(&transport, path, &case, id.clone(), &payload, true)
                    .await
                    .expect("benchmark send should succeed");
                let ack = wait_for_ack(
                    &mut rx,
                    &id,
                    case.message_type.message_type(),
                    true,
                    payload.len,
                    payload.checksum,
                    timeout,
                )
                .await;
                black_box(ack.payload_len);
                black_box(ack.checksum);
                black_box(case.payload_len);
                black_box(case.message_type.label());
            });
        });
    });
}

fn bench_tx_only_case(
    runtime: &Runtime,
    group: &mut BenchmarkGroup<'_, criterion::measurement::WallTime>,
    path: BenchPath,
    message_type: BenchMessageType,
    payload_case_id: &'static str,
    payload_len: usize,
) {
    let case = BenchCase::new(message_type, payload_case_id, payload_len);
    let payload = PreparedPayload::for_path(path, payload_len);
    let transport = runtime.block_on(build_transport(&case.authority));

    group.bench_function(case.benchmark_id(path), |b| {
        b.iter(|| {
            runtime.block_on(async {
                let id = next_uuid();
                send_path(&transport, path, &case, id, &payload, true)
                    .await
                    .expect("benchmark send should succeed");
                black_box(payload.len);
                black_box(payload.checksum);
                black_box(case.message_type.label());
            });
        });
    });
}

fn bench_no_payload_smoke(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("transport_no_payload_smoke");
    for path in [
        BenchPath::Owned,
        BenchPath::ZeroCopyLoanCopy,
        BenchPath::ZeroCopyUninitDirect,
    ] {
        for message_type in [
            BenchMessageType::Publish,
            BenchMessageType::Notification,
            BenchMessageType::Request,
            BenchMessageType::Response,
        ] {
            let case = BenchCase::new(message_type, "no_payload", 0);
            let payload = PreparedPayload::no_payload_for(path);
            let transport = runtime.block_on(build_transport(&case.authority));
            let (tx, mut rx) = mpsc::unbounded_channel();
            runtime.block_on(register_listener(&transport, path, &case, tx));
            group.bench_function(case.no_payload_benchmark_id(path), |b| {
                b.iter(|| {
                    runtime.block_on(async {
                        let id = next_uuid();
                        send_path(&transport, path, &case, id.clone(), &payload, false)
                            .await
                            .expect("benchmark no-payload send should succeed");
                        let ack = wait_for_ack(
                            &mut rx,
                            &id,
                            case.message_type.message_type(),
                            false,
                            0,
                            0,
                            BENCH_TIMEOUT,
                        )
                        .await;
                        black_box(ack.message_type);
                    });
                });
            });
        }
    }
    group.finish();
}

fn bench_transport(c: &mut Criterion) {
    UPTransportZenoh::try_init_log_from_env();
    let suite = BenchSuite::from_env();
    let profile = BenchProfile::from_env();
    if suite.includes_raw() && profile.includes_core() {
        bench_payload_matrix(
            c,
            "transport_send_receive",
            CORE_PAYLOAD_CASES,
            BENCH_TIMEOUT,
            true,
        );
        bench_payload_matrix(
            c,
            "transport_tx_only",
            CORE_PAYLOAD_CASES,
            BENCH_TIMEOUT,
            false,
        );
        bench_no_payload_smoke(c);
    }
    if suite.includes_raw() && profile.includes_camera() {
        bench_payload_matrix(
            c,
            "transport_large_sensor_send_receive",
            LARGE_SENSOR_PAYLOAD_CASES,
            LARGE_SENSOR_BENCH_TIMEOUT,
            true,
        );
        bench_payload_matrix(
            c,
            "transport_large_sensor_tx_only",
            LARGE_SENSOR_PAYLOAD_CASES,
            LARGE_SENSOR_BENCH_TIMEOUT,
            false,
        );
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

fn fill_pattern(dst: &mut [u8], start: usize) {
    for (offset, byte) in dst.iter_mut().enumerate() {
        let value = (start + offset) % 251;
        *byte = u8::try_from(value).expect("pattern byte fits in u8");
    }
}

fn checksum_for_len(len: usize) -> u64 {
    let mut checksum = 0_u64;
    let mut offset = 0;
    let mut chunk = [0_u8; DIRECT_WRITE_CHUNK];
    while offset < len {
        let take = cmp::min(chunk.len(), len - offset);
        fill_pattern(&mut chunk[..take], offset);
        checksum = checksum_bytes(checksum, &chunk[..take]);
        offset += take;
    }
    checksum
}

fn checksum_bytes(checksum: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(checksum, |checksum, byte| {
        checksum.wrapping_mul(16_777_619) ^ u64::from(*byte)
    })
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

criterion_group!(transport_criterion, bench_transport);
criterion_main!(transport_criterion);
