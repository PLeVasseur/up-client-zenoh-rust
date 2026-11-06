// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Real provider page residency, prewarming failure and released-capacity checks.

#![cfg(feature = "zero-copy")]

#[cfg(target_os = "linux")]
mod prepared_startup {

    use std::{
        process::Command,
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };
    use up_rust::{RouteLimits, UUri};
    use up_transport_zenoh::{
        zenoh_config, ZenohPreparedConfig, ZenohPreparedCore, ZenohRouteReservation,
    };

    const CHILD_CASE: &str = "UP_ZENOH_PREPARED_STARTUP_CASE";

    fn watchdog_names() -> Vec<String> {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .map(|entry| {
                std::fs::read_to_string(entry.unwrap().path().join("comm"))
                    .unwrap()
                    .trim()
                    .to_owned()
            })
            .filter(|name| name.starts_with("Watchdog "))
            .collect()
    }

    async fn startup_case(case: &str) {
        let transmit = case.ends_with("-tx");
        let mut config = zenoh_config::Config::default();
        for (key, value) in [
            ("mode", "\"peer\""),
            ("listen/endpoints", "[]"),
            ("scouting/multicast/enabled", "false"),
            ("scouting/gossip/enabled", "false"),
            ("transport/shared_memory/mode", "\"lazy\""),
        ] {
            config.insert_json5(key, value).unwrap();
        }
        let catalog = [ZenohRouteReservation {
            source: UUri::try_from_parts("prepared", 0x0001_4210, 1, 0x9000).unwrap(),
            sink: None,
            limits: RouteLimits {
                max_metadata_len: 3072,
                max_payload_len: 64,
                payload_alignment: 8,
            },
            transmit,
        }];
        let prepared = ZenohPreparedConfig {
            shm_segment_size: 1024 * 1024,
            route_capacity: 1,
            registration_capacity: 1,
            allocation_observer: None,
        };
        let core = if case.starts_with("from-session-") {
            let session = zenoh::open(config).await.unwrap();
            ZenohPreparedCore::from_session(session, "//prepared/4210/1/1/0", &catalog, prepared)
                .await
                .unwrap()
        } else {
            ZenohPreparedCore::new(config, "//prepared/4210/1/1/0", &catalog, prepared)
                .await
                .unwrap()
        };
        assert_eq!(
            core.prewarmed_shm_segment_size(),
            if transmit { 1024 * 1024 } else { 0 },
            "receive-only setup must not allocate a local payload provider"
        );
        assert_eq!(core.stats().published.load(Ordering::SeqCst), 0);
        assert_eq!(core.stats().received_shm.load(Ordering::SeqCst), 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let names = watchdog_names();
            if names.iter().any(|name| name.starts_with("Watchdog Confir"))
                && names.iter().any(|name| name.starts_with("Watchdog Valida"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{case}: watchdogs absent before traffic: {names:?}"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn prepared_shm_runtime_is_initialized_without_traffic() {
        if let Ok(case) = std::env::var(CHILD_CASE) {
            assert!(
                watchdog_names().is_empty(),
                "startup case must use a cold process"
            );
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), startup_case(&case))
                    .await
                    .unwrap();
            });
            return;
        }
        let mut failures = Vec::new();
        for case in ["new-tx", "new-rx", "from-session-tx", "from-session-rx"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "prepared_startup::prepared_shm_runtime_is_initialized_without_traffic",
                    "--nocapture",
                ])
                .env(CHILD_CASE, case)
                .status()
                .unwrap();
            if !status.success() {
                failures.push(case);
            }
        }
        assert!(
            failures.is_empty(),
            "cold-process startup failures: {failures:?}"
        );
    }
}

#[path = "support/allocating_control.rs"]
mod control;
use control::WithControlWire;
use up_rust::{UCode, UFrameMetadata, UTxBuffer, UTxLoanSpec, UUri};
use up_transport_zenoh::ZenohZeroCopyCore;

#[tokio::test]
async fn prewarming_rejects_unrepresentable_size_before_opening_a_session() {
    let result = ZenohZeroCopyCore::builder("//prewarm-invalid/4210/1/1/0")
        .with_shm_segment_size(usize::MAX)
        .unwrap()
        .with_shm_prewarm(true)
        .build()
        .await;
    match result {
        Err(error) => assert_eq!(error.code(), UCode::InvalidArgument),
        Ok(_) => panic!("an unrepresentable prewarming size must fail construction"),
    }
}

#[cfg(target_os = "linux")]
fn mapping_residency(address: usize) -> (usize, usize, usize, String) {
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut selected = false;
    let mut size = None;
    let mut rss = None;
    let mut locked = None;
    let mut header = String::new();
    for line in smaps.lines() {
        let first = line.split_whitespace().next().unwrap_or_default();
        if let Some((start, end)) = first.split_once('-') {
            if selected {
                break;
            }
            let start = usize::from_str_radix(start, 16).unwrap();
            let end = usize::from_str_radix(end, 16).unwrap();
            selected = (start..end).contains(&address);
            if selected {
                line.clone_into(&mut header);
            }
        } else if selected {
            if let Some(value) = line.strip_prefix("Size:") {
                size = Some(
                    value
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                        * 1024,
                );
            }
            if let Some(value) = line.strip_prefix("Rss:") {
                rss = Some(
                    value
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                        * 1024,
                );
            }
            if let Some(value) = line.strip_prefix("Locked:") {
                locked = Some(
                    value
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                        * 1024,
                );
            }
        }
    }
    (
        size.expect("mapping size"),
        rss.expect("mapping RSS"),
        locked.expect("locked mapping bytes"),
        header,
    )
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn real_provider_prewarming_materializes_full_mapping_and_releases_capacity() {
    for bytes in [1024 * 1024, 48 * 1024 * 1024, 96 * 1024 * 1024] {
        for enabled in [false, true] {
            let authority = format!("prewarm-{}-{bytes}-{enabled}", std::process::id());
            let core = ZenohZeroCopyCore::builder(format!("//{authority}/4210/1/1/0"))
                .with_shm_segment_size(bytes)
                .unwrap()
                .with_shm_prewarm(enabled)
                .build()
                .await
                .unwrap();
            assert_eq!(core.prewarmed_shm_segment_size(), enabled.then_some(bytes));
            let transport = core.with_control_wire(up_rust::ProtobufWire);
            let source = UUri::try_from_parts(&authority, 0x0001_4210, 1, 0x9000).unwrap();
            for iteration in 0..3 {
                let metadata = UFrameMetadata::publish(source.clone())
                    .with_payload_encoding(up_rust::PayloadEncoding::PROTOBUF)
                    .build()
                    .unwrap();
                let spec = UTxLoanSpec::payload(metadata, bytes - 4096, 8).unwrap();
                let buffer = transport.loan_validated_tx(spec).await.unwrap();
                let (size, rss, locked, mapping) =
                    mapping_residency(buffer.payload().as_ptr() as usize);
                assert!(
                    mapping.contains("/dev/shm/"),
                    "unexpected provider mapping: {mapping}"
                );
                assert!(
                    size >= bytes,
                    "provider mapping smaller than configured data region"
                );
                assert_eq!(
                    rss, size,
                    "provider mapping is not fully resident: {mapping}"
                );
                assert_eq!(
                    locked, size,
                    "provider mapping is not fully locked: {mapping}"
                );
                println!("SHM_PROVIDER_PAGE_PROOF enabled={enabled} configured_bytes={bytes} iteration={iteration} mapping_bytes={size} resident_bytes={rss} locked_bytes={locked} mapping={mapping}");
                drop(buffer);
            }
        }
    }
}
