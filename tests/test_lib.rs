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
use std::sync::Once;
use up_rust::UStatus;
use up_transport_zenoh::UPTransportZenoh;

static INIT: Once = Once::new();

pub fn before_test() {
    INIT.call_once(UPTransportZenoh::try_init_log_from_env);
}

/// # Errors
/// Will return `Err` if unable to create `UPTransportZenoh`
pub async fn create_up_transport_zenoh(
    local_authority_name: &str,
    config: Option<zenoh::config::Config>,
) -> Result<UPTransportZenoh, UStatus> {
    let local_uri = if local_authority_name.starts_with("//") {
        local_authority_name.to_string()
    } else {
        format!("//{local_authority_name}/1/1/0")
    };
    UPTransportZenoh::new(config.unwrap_or_default(), local_uri).await
}
