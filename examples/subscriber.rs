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

//! Receives native publish frames over Zenoh.

mod common;

use async_trait::async_trait;
use std::{str::FromStr, sync::Arc};
use up_rust::{UOwnedFrame, UOwnedListener, UOwnedTransport, UUri};
use up_transport_zenoh::UPTransportZenoh;

struct SubscriberListener(tokio::runtime::Runtime);

#[async_trait]
impl UOwnedListener for SubscriberListener {
    async fn on_receive_owned(&self, frame: UOwnedFrame) {
        self.0.spawn(async move {
            let value = String::from_utf8_lossy(frame.payload_bytes());
            let uri = frame.metadata().attributes().source().to_uri(false);
            println!("Received frame [topic: {uri}, payload: {value}]");
        });
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    UPTransportZenoh::try_init_log_from_env();

    println!("uProtocol Zenoh native subscriber example");
    let transport = UPTransportZenoh::builder("subscriber")
        .expect("invalid authority name")
        .with_config(common::get_zenoh_config())
        .build()
        .await?;
    let source_filter = UUri::from_str("//*/FFFFB1DA/1/8001")?;

    println!(
        "Registering publish listener [source filter: {}]",
        source_filter.to_uri(false)
    );
    let message_processing_rt = tokio::runtime::Builder::new_multi_thread()
        .thread_name("message-processing")
        .worker_threads(1)
        .build()?;
    transport
        .register_owned_listener(
            &source_filter,
            None,
            Arc::new(SubscriberListener(message_processing_rt)),
        )
        .await?;

    tokio::signal::ctrl_c().await.map_err(Box::from)
}
