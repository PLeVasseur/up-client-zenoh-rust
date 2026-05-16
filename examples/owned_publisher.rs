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
    wire::RawBytes, LocalUriProvider, StaticUriProvider, UFrameMetadata, UOwnedTransportExt,
};
use up_transport_zenoh::UPTransportZenoh;

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

    for cnt in 1..=100 {
        let data = format!("owned event {cnt}");
        println!(
            "Publishing owned frame [topic: {}, payload: {data}]",
            topic.to_uri(false)
        );
        transport
            .send_serialized::<RawBytes, _>(
                UFrameMetadata::publish(topic.clone()),
                &data.as_bytes(),
            )
            .await?;
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;
    }
    Ok(())
}
