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
use crate::UPTransportZenoh;
use async_trait::async_trait;
use std::{string::ToString, sync::Arc, time::Duration};
use tracing::error;
use up_rust::{
    communication::{CallOptions, RpcClient, ServiceInvocationError, UPayload},
    LocalUriProvider, UCode, UMessageBuilder, UStatus, UUri,
};
use zenoh::query::QueryTarget;

pub struct ZenohRpcClient {
    transport: Arc<UPTransportZenoh>,
}
impl ZenohRpcClient {
    /// Creates a new RPC client for the Zenoh transport.
    ///
    /// # Arguments
    ///
    /// * `transport` - The Zenoh uProtocol Transport Layer.
    #[must_use]
    pub fn new(transport: Arc<UPTransportZenoh>) -> Self {
        ZenohRpcClient { transport }
    }
}

#[async_trait]
impl RpcClient for ZenohRpcClient {
    async fn invoke_method(
        &self,
        method: UUri,
        call_options: CallOptions,
        payload: Option<UPayload>,
    ) -> Result<Option<UPayload>, ServiceInvocationError> {
        // Get source UUri
        let source_uri = self.transport.get_source_uri();

        let mut builder =
            UMessageBuilder::request(method.clone(), source_uri.clone(), call_options.ttl());
        if let Some(message_id) = call_options.message_id() {
            builder.with_message_id(message_id.clone());
        }
        if let Some(priority) = call_options.priority() {
            builder.with_priority(priority);
        }
        if let Some(token) = call_options.token() {
            builder.with_token(token.clone());
        }
        let message = if let Some(payload) = payload {
            let payload_encoding = payload.payload_encoding();
            builder.build_with_payload(payload.payload().clone(), payload_encoding)
        } else {
            builder.build()
        }
        .map_err(|err| ServiceInvocationError::Internal(err.to_string()))?;
        let attributes = message.attributes().clone();
        let payload_data = message.payload().map(|payload| payload.to_vec());

        // Get Zenoh key
        let zenoh_key = self
            .transport
            .to_zenoh_key_string(&source_uri, Some(&method));

        // Put UAttributes into Zenoh user attachment
        let Ok(attachment) = UPTransportZenoh::uattributes_to_attachment(&attributes) else {
            let msg = "Unable to transform UAttributes to user attachment in Zenoh".to_string();
            error!("{msg}");
            return Err(ServiceInvocationError::Internal(msg));
        };

        // Send the query
        let mut getbuilder = self.transport.session.get(&zenoh_key);
        getbuilder = match payload_data {
            Some(data) => getbuilder.payload(data),
            None => getbuilder,
        }
        .attachment(attachment)
        .target(QueryTarget::BestMatching)
        .timeout(Duration::from_millis(u64::from(call_options.ttl())));
        let Ok(replies) = getbuilder.await else {
            let msg = "Error while sending Zenoh query".to_string();
            error!("{msg}");
            return Err(ServiceInvocationError::RpcError(Box::new(
                UStatus::fail_with_code(UCode::Internal, msg),
            )));
        };

        // Receive the reply
        let Ok(reply) = replies.recv_async().await else {
            let msg = "Error while receiving Zenoh reply".to_string();
            error!("{msg}");
            return Err(ServiceInvocationError::RpcError(Box::new(
                UStatus::fail_with_code(UCode::Internal, msg),
            )));
        };
        match reply.into_result() {
            Ok(sample) => {
                let Some(payload_encoding) = sample
                    .attachment()
                    .and_then(|a| UPTransportZenoh::attachment_to_uattributes(a).ok())
                    .and_then(|attr| attr.payload_encoding())
                else {
                    return Err(ServiceInvocationError::InvalidArgument(
                        "Zenoh reply carries a payload without a declared payload encoding"
                            .to_string(),
                    ));
                };
                Ok(Some(UPayload::new(
                    sample.payload().to_bytes().to_vec(),
                    payload_encoding,
                )))
            }
            Err(e) => {
                let msg = format!("Error while parsing Zenoh reply: {e:?}");
                error!("{msg}");
                Err(ServiceInvocationError::RpcError(Box::new(
                    UStatus::fail_with_code(UCode::Internal, msg),
                )))
            }
        }
    }
}
