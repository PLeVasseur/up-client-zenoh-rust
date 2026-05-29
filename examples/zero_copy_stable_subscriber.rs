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

use std::sync::Arc;

use up_rust::{
    zero_copy::{
        UFrameView, ULoanedContiguousZeroCopyRxFrame, UZeroCopyListener, UZeroCopyTransport,
    },
    UUri,
};
use up_transport_zenoh::{UPTransportZenoh, ZenohRxFrame};

#[repr(C)]
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, up_rust::StablePayload, up_rust::ByteBackedStablePayload,
)]
#[stable_payload(type_name = "example.vehicle.VehiclePose")]
struct VehiclePose {
    x: u64,
    y: u64,
}

struct StablePoseListener;

#[async_trait::async_trait]
impl UZeroCopyListener<ZenohRxFrame> for StablePoseListener {
    async fn on_receive_zero_copy(&self, frame: ZenohRxFrame) {
        match frame.borrow_stable_payload::<VehiclePose>() {
            Ok(pose) => println!(
                "Received stable SHM pose [source: {}, loan provenance: {:?}, pose: {:?}]",
                frame.metadata().source().to_uri(false),
                frame
                    .payload_loan_provenance()
                    .expect("stable SHM payload should report loan provenance"),
                pose
            ),
            Err(error) => println!("Dropped non-stable or non-SHM Zenoh payload: {error}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    let transport = UPTransportZenoh::builder("subscriber")
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;
    let source_filter = UUri::try_from("//publisher/3B1DA/1/8001")?;

    transport
        .register_zero_copy_listener(&source_filter, None, Arc::new(StablePoseListener))
        .await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
