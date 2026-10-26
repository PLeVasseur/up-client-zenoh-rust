// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)] // Each native/runtime observer uses a different subset.

use std::{sync::Arc, time::Duration};
use up_rust::{
    NativeProfile, NativeProfileAgreement, NativeProfileMode, NativeProfileTable, OverflowPolicy,
    PayloadEncoding, RegistrationConfig, RouteDefinition, RouteLimits, StablePayload, UMessageType,
    UUri,
};
use up_transport_zenoh::{
    zenoh_config, ZenohPreparedConfig, ZenohPreparedCore, ZenohRouteReservation,
};

#[repr(C)]
#[derive(up_rust::StablePayload, up_rust::StablePayloadInit)]
#[stable_payload(type_name = "uprotocol.test.ZenohPreparedValue")]
pub struct Value {
    pub sequence: u32,
    pub bytes: [u8; 64],
}

#[repr(C)]
#[derive(up_rust::StablePayload, up_rust::StablePayloadInit)]
#[stable_payload(type_name = "uprotocol.test.ZenohForeignValue")]
pub struct Foreign {
    pub sequence: u32,
    pub bytes: [u8; 64],
}

pub const TRACE: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

pub fn agreement() -> NativeProfileAgreement {
    let table = NativeProfileTable::new([
        (
            PayloadEncoding::private_use(0xF101),
            Value::native_representation(),
        ),
        (
            PayloadEncoding::private_use(0xF102),
            Foreign::native_representation(),
        ),
    ])
    .unwrap();
    let profile =
        NativeProfile::new("zenoh-prepared-tests", 1, NativeProfileMode::Table(table)).unwrap();
    NativeProfileAgreement::new(Arc::new(profile.clone()), &profile).unwrap()
}
pub fn source() -> UUri {
    UUri::try_from_parts("prepared", 0x4210, 1, 0x9000).unwrap()
}
pub fn definition(source: UUri) -> RouteDefinition {
    RouteDefinition {
        kind: UMessageType::Publish,
        source,
        sink: None,
        priority: None,
        ttl: None,
        permission_level: None,
    }
}
pub fn limits(payload: usize) -> RouteLimits {
    RouteLimits {
        max_metadata_len: 3072,
        max_payload_len: payload,
        payload_alignment: 8,
    }
}
pub fn registration() -> RegistrationConfig {
    RegistrationConfig {
        capacity: 4,
        overflow: OverflowPolicy::Block {
            deadline: Duration::from_secs(5),
        },
    }
}

pub fn config(endpoint: &str, listen: bool) -> zenoh_config::Config {
    let mut config = zenoh_config::Config::default();
    if listen {
        config.insert_json5("mode", "\"router\"").unwrap();
    }
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    let endpoints = serde_json::to_string(&[endpoint]).unwrap();
    config
        .insert_json5(
            if listen {
                "listen/endpoints"
            } else {
                "connect/endpoints"
            },
            &endpoints,
        )
        .unwrap();
    if !listen {
        config.insert_json5("listen/endpoints", "[]").unwrap();
    }
    config
}

pub async fn pair(
    payload: usize,
    registrations: usize,
) -> (Arc<ZenohPreparedCore>, Arc<ZenohPreparedCore>) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = format!("tcp/127.0.0.1:{port}");
    let reservation = ZenohRouteReservation {
        source: source(),
        sink: None,
        limits: limits(payload),
        transmit: true,
    };
    let sender = ZenohPreparedCore::new(
        config(&endpoint, true),
        "//prepared/4210/1/0",
        std::slice::from_ref(&reservation),
        ZenohPreparedConfig {
            shm_segment_size: 1024 * 1024,
            route_capacity: 16,
            registration_capacity: registrations,
            allocation_observer: None,
        },
    )
    .await
    .unwrap();
    let receiver = ZenohPreparedCore::new(
        config(&endpoint, false),
        "//prepared/4210/1/0",
        &[ZenohRouteReservation {
            transmit: false,
            ..reservation
        }],
        ZenohPreparedConfig {
            shm_segment_size: 1024 * 1024,
            route_capacity: 16,
            registration_capacity: registrations,
            allocation_observer: None,
        },
    )
    .await
    .unwrap();
    (Arc::new(sender), Arc::new(receiver))
}

pub async fn catalog_pair(
    catalog: &[ZenohRouteReservation],
    registrations: usize,
    observer: &'static dyn up_transport_zenoh::ZenohAllocationObserver,
) -> (
    Arc<ZenohPreparedCore>,
    Arc<ZenohPreparedCore>,
    zenoh::Session,
) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = format!("tcp/127.0.0.1:{port}");
    let prepared = || ZenohPreparedConfig {
        shm_segment_size: 1024 * 1024,
        route_capacity: 64,
        registration_capacity: registrations,
        allocation_observer: Some(observer),
    };
    let session = zenoh::open(config(&endpoint, true)).await.unwrap();
    let sender = ZenohPreparedCore::from_session(
        session.clone(),
        "//prepared/4210/1/0",
        catalog,
        prepared(),
    )
    .await
    .unwrap();
    let receiver = ZenohPreparedCore::new(
        config(&endpoint, false),
        "//prepared/4210/1/0",
        catalog,
        prepared(),
    )
    .await
    .unwrap();
    (Arc::new(sender), Arc::new(receiver), session)
}
