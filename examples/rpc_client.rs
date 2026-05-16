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

//! Performs a native request/response exchange over Zenoh.

mod common;

use async_trait::async_trait;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::sync::Notify;
use up_rust::{
    LocalUriProvider, StaticUriProvider, UFrameBuilder, UOwnedFrame, UOwnedListener,
    UOwnedTransport, UUri,
};
use up_transport_zenoh::UPTransportZenoh;

const REQUEST_TTL: u32 = 1000;

struct ResponseListener(Arc<Notify>);

#[async_trait]
impl UOwnedListener for ResponseListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        let value = String::from_utf8_lossy(frame.payload_bytes());
        let uri = frame.metadata().attributes().source().to_uri(false);
        println!("Received RPC response [from: {uri}, payload: {value}]");
        self.0.notify_one();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    println!("uProtocol Zenoh native RPC client example");
    let uri_provider = StaticUriProvider::new("l1-rpc-client", 0xdd00, 2);
    let transport = UPTransportZenoh::builder(uri_provider.get_authority())
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;
    let method = UUri::from_str("//rpc-server/AAA/1/6A10")?;
    let reply_to = uri_provider.get_source_uri();

    let notify = Arc::new(Notify::new());
    transport
        .register_owned_listener(
            &method,
            Some(&reply_to),
            Arc::new(ResponseListener(notify.clone())),
        )
        .await?;

    let request = UFrameBuilder::request(method.clone(), reply_to.clone(), REQUEST_TTL)
        .build_with_raw_payload("GetCurrentTime")?;
    println!(
        "Sending RPC request [from: {}, to: {}]",
        reply_to.to_uri(false),
        method.to_uri(false)
    );
    transport.send_owned(request).await?;

    tokio::time::timeout(
        Duration::from_millis(u64::from(REQUEST_TTL * 2)),
        notify.notified(),
    )
    .await
    .map_err(Box::from)
}
