/********************************************************************************
 * Copyright (c) 2024 Contributors to the Eclipse Foundation
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

//! Publishes native owned frames over Zenoh.

mod common;

use up_rust::{LocalUriProvider, StaticUriProvider, UFrameBuilder, UOwnedTransport};
use up_transport_zenoh::UPTransportZenoh;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    println!("uProtocol Zenoh native publisher example");
    let uri_provider = StaticUriProvider::new("publisher", 0x3_b1da, 1);
    let transport = UPTransportZenoh::builder(uri_provider.get_authority())
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;
    let topic = uri_provider.get_resource_uri(0x8001);

    for count in 1..=100 {
        let data = format!("event {count}");
        println!(
            "Publishing frame [topic: {}, payload: {data}]",
            topic.to_uri(false)
        );
        let frame = UFrameBuilder::publish(topic.clone()).build_with_raw_payload(data)?;
        transport.send_owned(frame).await?;
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;
    }
    Ok(())
}
