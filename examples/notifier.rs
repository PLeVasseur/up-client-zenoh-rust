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

//! Sends native notification frames over Zenoh.

mod common;

use std::str::FromStr;
use up_rust::{LocalUriProvider, StaticUriProvider, UFrameBuilder, UOwnedTransport, UUri};
use up_transport_zenoh::UPTransportZenoh;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    println!("uProtocol Zenoh native notifier example");
    let uri_provider = StaticUriProvider::new("notification", 0xa1b2, 1);
    let transport = UPTransportZenoh::builder(uri_provider.get_authority())
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;
    let source = uri_provider.get_resource_uri(0x8001);
    let sink = UUri::from_str("//receiver/10AB10/1/0")?;

    for count in 1..=100 {
        let data = format!("notification {count}");
        println!(
            "Sending notification [from: {}, to: {}, payload: {data}]",
            source.to_uri(false),
            sink.to_uri(false)
        );
        let frame = UFrameBuilder::notification(source.clone(), sink.clone())
            .build_with_raw_payload(data)?;
        transport.send_owned(frame).await?;
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;
    }
    Ok(())
}
