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
    payload::StableContainerPayload, zero_copy::UZeroCopyUninitTransportExt, LocalUriProvider,
    StaticUriProvider, UFrameMetadata,
};
use up_transport_zenoh::UPTransportZenoh;

#[repr(C)]
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, up_rust::StablePayload, up_rust::ByteBackedStablePayload,
)]
#[stable_payload(type_name = "example.vehicle.VehiclePose")]
struct VehiclePose {
    x: u64,
    y: u64,
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

    for count in 1_u64..=100 {
        let pose = VehiclePose {
            x: count,
            y: count * 10,
        };
        println!(
            "Publishing stable SHM pose [topic: {}, pose: {:?}]",
            topic.to_uri(false),
            pose
        );
        transport
            .send_uninit_loaned_payload_as::<StableContainerPayload<VehiclePose>, VehiclePose>(
                UFrameMetadata::try_publish(topic.clone())?,
                |slot| Ok(slot.write(pose)),
            )
            .await?;
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;
    }
    Ok(())
}
