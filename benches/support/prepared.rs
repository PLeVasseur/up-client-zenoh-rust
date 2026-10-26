// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Typed benchmark routes/registrations reserved before Criterion iterations.

use super::{PayloadContractAck, PAYLOAD_CONTRACT_SEQUENCE, ZENOH_SHM_SEGMENT_SIZE};
use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
#[cfg(feature = "payload-contract-large-benchmarks")]
use up_rust::bench_fixtures::payload_contract::{
    CameraBayerRggb12pFrame8mpV1, LidarPointCloudHesaiAt128V1,
};
use up_rust::{
    bench_fixtures::payload_contract::{
        self as fixtures, CanClassicFrameV1, CanFdFrameV1, PayloadContractCase,
        PayloadContractCaseKind, RadarDetectionListArs548V1, SomeIpSignalBatchMtuV1,
        StablePayloadContractView, StreamChunk4kV1, StreamChunk64kV1,
    },
    HeaderLimits, InitializedStablePayload, MessageFields, NativeProfileAgreement, OverflowPolicy,
    PayloadInit, RegistrationConfig, RouteConfig, RouteDefinition, RouteLimits, RxAdmission,
    RxLease, StablePayloadInit, TxRoute, TxTemplate, UCode, UFrameMetadata, UMessageType, UStatus,
    UUri, WireRegistration, ZeroCopyHandler,
};
use up_transport_zenoh::{
    zenoh_config, ZenohPreparedConfig, ZenohPreparedCore, ZenohPreparedRx, ZenohRouteReservation,
};

pub struct Transport {
    core: Arc<ZenohPreparedCore>,
    source: UUri,
    profile: NativeProfileAgreement,
    route: Route,
    subscription: Mutex<Option<WireRegistration<ZenohPreparedCore>>>,
}
enum Route {
    Can(TxRoute<ZenohPreparedCore, CanClassicFrameV1>),
    CanFd(TxRoute<ZenohPreparedCore, CanFdFrameV1>),
    SomeIp(TxRoute<ZenohPreparedCore, SomeIpSignalBatchMtuV1>),
    Stream4k(TxRoute<ZenohPreparedCore, StreamChunk4kV1>),
    Radar(TxRoute<ZenohPreparedCore, RadarDetectionListArs548V1>),
    Stream64k(TxRoute<ZenohPreparedCore, StreamChunk64kV1>),
    #[cfg(feature = "payload-contract-large-benchmarks")]
    Lidar(TxRoute<ZenohPreparedCore, LidarPointCloudHesaiAt128V1>),
    #[cfg(feature = "payload-contract-large-benchmarks")]
    Camera(TxRoute<ZenohPreparedCore, CameraBayerRggb12pFrame8mpV1>),
}
pub trait Fixture: StablePayloadContractView + StablePayloadInit {
    fn route(transport: &Transport) -> &TxRoute<ZenohPreparedCore, Self>;
}
macro_rules! fixture {
    ($type:ty, $variant:ident) => {
        impl Fixture for $type {
            fn route(transport: &Transport) -> &TxRoute<ZenohPreparedCore, Self> {
                match &transport.route {
                    Route::$variant(route) => route,
                    _ => panic!("fixture does not match reserved benchmark route"),
                }
            }
        }
    };
}
fixture!(CanClassicFrameV1, Can);
fixture!(CanFdFrameV1, CanFd);
fixture!(SomeIpSignalBatchMtuV1, SomeIp);
fixture!(StreamChunk4kV1, Stream4k);
fixture!(RadarDetectionListArs548V1, Radar);
fixture!(StreamChunk64kV1, Stream64k);
#[cfg(feature = "payload-contract-large-benchmarks")]
fixture!(LidarPointCloudHesaiAt128V1, Lidar);
#[cfg(feature = "payload-contract-large-benchmarks")]
fixture!(CameraBayerRggb12pFrame8mpV1, Camera);

struct Ack<P> {
    tx: mpsc::UnboundedSender<PayloadContractAck>,
    contract: PayloadContractCase,
    payload: PhantomData<fn() -> P>,
}
impl<P: StablePayloadContractView> ZeroCopyHandler<P, ZenohPreparedRx> for Ack<P> {
    async fn on_receive(&mut self, frame: RxLease<P, ZenohPreparedRx>) {
        fixtures::validate_stable_payload(
            &self.contract,
            PAYLOAD_CONTRACT_SEQUENCE,
            frame.payload().expect("typed native fixture"),
        )
        .expect("native fixture semantics");
        let header = frame.header().expect("native benchmark header");
        self.tx
            .send(PayloadContractAck {
                id: header.id,
                message_type: header.kind,
                case_id: self.contract.case_id(),
                sequence: PAYLOAD_CONTRACT_SEQUENCE,
                semantic_reference_len: self.contract.semantic_reference_len(),
                transported_payload_len: std::mem::size_of::<P>(),
            })
            .expect("benchmark receive channel remains open");
    }
}

fn reserve<P: StablePayloadInit>(
    core: &Arc<ZenohPreparedCore>,
    profile: &NativeProfileAgreement,
    source: &UUri,
) -> TxRoute<ZenohPreparedCore, P> {
    TxTemplate::<P>::new(profile.clone())
        .expect("fixture agreement")
        .route(
            Arc::clone(core),
            RouteDefinition {
                kind: UMessageType::Publish,
                source: source.clone(),
                sink: None,
                priority: None,
                ttl: None,
                permission_level: None,
            },
            RouteConfig::default(),
        )
        .expect("reserved fixture route")
}

impl Transport {
    pub async fn new(
        authority: &str,
        source: UUri,
        profile: NativeProfileAgreement,
        contract: &PayloadContractCase,
    ) -> Self {
        let core = Arc::new(
            ZenohPreparedCore::new(
                zenoh_config::Config::default(),
                format!("//{authority}/4210/1/0"),
                &[ZenohRouteReservation {
                    source: source.clone(),
                    sink: None,
                    limits: RouteLimits {
                        max_metadata_len: 3072,
                        max_payload_len: fixtures::stable_payload_len(contract),
                        payload_alignment: 8,
                    },
                    transmit: true,
                }],
                ZenohPreparedConfig {
                    shm_segment_size: ZENOH_SHM_SEGMENT_SIZE,
                    route_capacity: 1,
                    registration_capacity: 1,
                    allocation_observer: None,
                },
            )
            .await
            .expect("native benchmark core"),
        );
        let route = match contract.kind() {
            PayloadContractCaseKind::CanClassicMax => Route::Can(reserve(&core, &profile, &source)),
            PayloadContractCaseKind::CanFdMax => Route::CanFd(reserve(&core, &profile, &source)),
            PayloadContractCaseKind::SomeIpSingleMtu => {
                Route::SomeIp(reserve(&core, &profile, &source))
            }
            PayloadContractCaseKind::Streamer4k => {
                Route::Stream4k(reserve(&core, &profile, &source))
            }
            PayloadContractCaseKind::RadarArs548DetectionList => {
                Route::Radar(reserve(&core, &profile, &source))
            }
            PayloadContractCaseKind::Streamer64k => {
                Route::Stream64k(reserve(&core, &profile, &source))
            }
            #[cfg(feature = "payload-contract-large-benchmarks")]
            PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
                Route::Lidar(reserve(&core, &profile, &source))
            }
            #[cfg(feature = "payload-contract-large-benchmarks")]
            PayloadContractCaseKind::Camera8mpBayerRggb12p => {
                Route::Camera(reserve(&core, &profile, &source))
            }
        };
        Self {
            core,
            source,
            profile,
            route,
            subscription: Mutex::new(None),
        }
    }
    async fn register<P: StablePayloadContractView>(
        &self,
        contract: PayloadContractCase,
        tx: mpsc::UnboundedSender<PayloadContractAck>,
    ) {
        let admission = RxAdmission::<P>::new(self.profile.clone(), HeaderLimits::default())
            .expect("native fixture admission");
        let worker = admission
            .register_filtered(
                self.source.clone(),
                None,
                RegistrationConfig {
                    capacity: 1,
                    overflow: OverflowPolicy::Block {
                        deadline: super::BENCH_TIMEOUT,
                    },
                },
                Ack::<P> {
                    tx,
                    contract,
                    payload: PhantomData,
                },
                || {},
            )
            .expect("native benchmark worker");
        let registration =
            WireRegistration::attach(Arc::clone(&self.core), &self.source, None, worker)
                .await
                .expect("native benchmark subscription");
        *self.subscription.lock().unwrap() = Some(registration);
    }
    pub async fn register_fixture(
        &self,
        contract: &PayloadContractCase,
        tx: mpsc::UnboundedSender<PayloadContractAck>,
    ) {
        match contract.kind() {
            PayloadContractCaseKind::CanClassicMax => {
                self.register::<CanClassicFrameV1>(*contract, tx).await;
            }
            PayloadContractCaseKind::CanFdMax => self.register::<CanFdFrameV1>(*contract, tx).await,
            PayloadContractCaseKind::SomeIpSingleMtu => {
                self.register::<SomeIpSignalBatchMtuV1>(*contract, tx).await;
            }
            PayloadContractCaseKind::Streamer4k => {
                self.register::<StreamChunk4kV1>(*contract, tx).await;
            }
            PayloadContractCaseKind::RadarArs548DetectionList => {
                self.register::<RadarDetectionListArs548V1>(*contract, tx)
                    .await;
            }
            PayloadContractCaseKind::Streamer64k => {
                self.register::<StreamChunk64kV1>(*contract, tx).await;
            }
            #[cfg(feature = "payload-contract-large-benchmarks")]
            PayloadContractCaseKind::LidarHesaiAt128PointCloud => {
                self.register::<LidarPointCloudHesaiAt128V1>(*contract, tx)
                    .await;
            }
            #[cfg(feature = "payload-contract-large-benchmarks")]
            PayloadContractCaseKind::Camera8mpBayerRggb12p => {
                self.register::<CameraBayerRggb12pFrame8mpV1>(*contract, tx)
                    .await;
            }
        }
        self.core
            .wait_for_matching(&self.source, None, super::BENCH_TIMEOUT)
            .await
            .expect("native benchmark subscriber matching");
    }
    pub async fn send_stable_payload<P: Fixture, F>(
        &self,
        metadata: UFrameMetadata,
        initialize: F,
    ) -> Result<(), UStatus>
    where
        F: for<'a> FnOnce(PayloadInit<'a, P>) -> InitializedStablePayload<'a, P>,
    {
        std::future::ready(
            P::route(self)
                .send(
                    MessageFields {
                        id: Some(metadata.id()),
                        ..MessageFields::default()
                    },
                    initialize,
                )
                .map_err(|error| UStatus::fail_with_code(UCode::Internal, error.to_string())),
        )
        .await
    }
}
