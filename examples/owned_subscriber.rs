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
    LocalUriProvider, StaticUriProvider, UOwnedFrame, UOwnedListener, UOwnedTransport, UUri,
};
use up_transport_zenoh::UPTransportZenoh;

struct SubscriberListener;

#[async_trait::async_trait]
impl UOwnedListener for SubscriberListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        println!(
            "Received owned frame [source: {}, encoding: {}, payload: {}]",
            frame.metadata().source().to_uri(false),
            frame
                .metadata()
                .encoding()
                .and_then(up_rust::PayloadEncoding::content_type)
                .unwrap_or("none"),
            String::from_utf8_lossy(frame.payload_bytes())
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    let uri_provider = StaticUriProvider::new("subscriber", 0x3_b1db, 1);
    let transport = UPTransportZenoh::builder(uri_provider.get_authority())
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;

    let source_filter = UUri::try_from("//publisher/3B1DA/1/8001")?;
    transport
        .register_owned_listener(&source_filter, None, Arc::new(SubscriberListener))
        .await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
