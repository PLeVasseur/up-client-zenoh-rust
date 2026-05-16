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

//! Implements a native RPC server over Zenoh.

mod common;

use async_trait::async_trait;
use chrono::Utc;
use std::{str::FromStr, sync::Arc};
use up_rust::{
    LocalUriProvider, StaticUriProvider, UFrameBuilder, UOwnedFrame, UOwnedListener,
    UOwnedTransport, UUri,
};
use up_transport_zenoh::UPTransportZenoh;

struct RpcListener(Arc<UPTransportZenoh>);

#[async_trait]
impl UOwnedListener for RpcListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        let request_value = String::from_utf8_lossy(frame.payload_bytes());
        println!(
            "Processing request [from: {}, to: {}, payload: {request_value}]",
            frame.metadata().attributes().source().to_uri(false),
            frame
                .metadata()
                .attributes()
                .sink()
                .map_or_else(|| "<none>".to_string(), |sink| sink.to_uri(false))
        );

        let response = UFrameBuilder::response_for_request(frame.metadata().attributes())
            .build_with_raw_payload(format!("{}", Utc::now()))
            .expect("failed to build response frame");
        let _ = self.0.send_owned(response).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    println!("uProtocol Zenoh native RPC server example");
    let method = UUri::from_str("//rpc-server/AAA/1/6A10")?;
    let uri_provider = StaticUriProvider::try_from(&method)?;
    let transport = UPTransportZenoh::builder(uri_provider.get_authority())
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await
        .map(Arc::new)?;
    let source_filter = UUri::from_str("//*/FFFFFFFF/FF/0")?;

    println!(
        "Registering RPC request handler [source filter: {}, sink filter: {}]",
        source_filter.to_uri(false),
        method.to_uri(false)
    );
    transport
        .register_owned_listener(
            &source_filter,
            Some(&method),
            Arc::new(RpcListener(transport.clone())),
        )
        .await?;

    tokio::signal::ctrl_c().await.map_err(Box::from)
}
