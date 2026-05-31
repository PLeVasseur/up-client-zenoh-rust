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

mod common;

use up_rust::{
    zero_copy::UZeroCopyUninitTransportExt, LocalUriProvider, StaticUriProvider, UFrameMetadata,
};
use up_transport_zenoh::UPTransportZenoh;

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    up_rust::StablePayload,
    up_rust::ByteBackedStablePayload,
    up_rust::StablePayloadInit,
)]
#[stable_payload(type_name = "org.eclipse.uprotocol.transport.example.NoZeroSensorHeader")]
struct NoZeroSensorHeader {
    case_id: u32,
    sequence: u32,
    logical_payload_len: u32,
}

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    up_rust::StablePayload,
    up_rust::ByteBackedStablePayload,
    up_rust::StablePayloadInit,
)]
#[stable_payload(type_name = "org.eclipse.uprotocol.transport.example.NoZeroSensorFrame")]
struct NoZeroSensorFrame {
    header: NoZeroSensorHeader,
    checksum: u32,
    payload: [u8; 4096],
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    let uri_provider = StaticUriProvider::new("publisher", 0x3_b1da, 1);
    let transport = UPTransportZenoh::builder(uri_provider.get_authority())
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;
    let topic = uri_provider.get_resource_uri(0x8001);

    for sequence in 1_u32..=100 {
        let checksum = 0x5eed_0000 | sequence;
        println!(
            "Publishing no-zero stable SHM sensor frame [topic: {}, sequence: {}]",
            topic.to_uri(false),
            sequence
        );
        transport
            .send_uninit_stable_payload_as::<NoZeroSensorFrame>(
                UFrameMetadata::try_publish(topic.clone())?,
                |frame| {
                    frame
                        .header(|header| {
                            header
                                .case_id(1)
                                .sequence(sequence)
                                .logical_payload_len(4096)
                                .finish()
                        })?
                        .checksum(checksum)
                        .payload_fill(0x5a)
                        .finish()
                },
            )
            .await?;
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;
    }
    Ok(())
}
