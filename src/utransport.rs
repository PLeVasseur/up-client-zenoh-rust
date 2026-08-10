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
use bytes::Bytes;
use std::sync::Arc;
use tracing::warn;
use up_rust::frame::metadata::{
    try_project_attributes_to_frame_metadata, try_project_frame_to_umessage,
};
use up_rust::{
    ComparableListener, UAttributes, UAttributesValidators, UCode, UListener, UMessage, UStatus,
    UTransport, UUri,
};
use zenoh::query::Query;

pub(crate) fn message_from_parts(
    attributes: &UAttributes,
    payload: Option<Bytes>,
) -> Result<UMessage, UStatus> {
    let metadata = try_project_attributes_to_frame_metadata(attributes, None).map_err(|err| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!("Unable to create UFrameMetadata from Zenoh sample: {err}"),
        )
    })?;
    try_project_frame_to_umessage(metadata, payload).map_err(|err| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!("Unable to create UMessage from Zenoh sample: {err}"),
        )
    })
}

fn is_rpc_request_filter(source_filter: &UUri, sink_filter: Option<&UUri>) -> bool {
    let Some(sink_filter) = sink_filter else {
        return false;
    };
    let source_is_reply_to =
        source_filter.resource_id() == 0 || source_filter.has_wildcard_resource_id();
    let sink_resource = sink_filter.resource_id();
    let sink_is_method =
        (1..0x8000).contains(&sink_resource) || sink_filter.has_wildcard_resource_id();
    source_is_reply_to && sink_is_method
}

impl UPTransportZenoh {
    async fn put_message(
        &self,
        zenoh_key: &str,
        payload: Bytes,
        attributes: &UAttributes,
    ) -> Result<(), UStatus> {
        let attachment = Self::uattributes_to_attachment(attributes).map_err(|err| {
            UStatus::fail_with_code(
                UCode::InvalidArgument,
                format!("Unable to transform UAttributes to attachment: {err}"),
            )
        })?;
        let priority =
            Self::map_zenoh_priority(attributes.priority().unwrap_or(up_rust::UPriority::CS1));

        self.session
            .put(zenoh_key, payload)
            .priority(priority)
            .attachment(attachment)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to put Zenoh message: {err}"),
                )
            })
    }

    async fn send_response(
        &self,
        zenoh_key: &str,
        payload: Bytes,
        attributes: &UAttributes,
    ) -> Result<(), UStatus> {
        let query = attributes.request_id().and_then(|request_id| {
            self.query_map
                .lock()
                .unwrap()
                .remove(&request_id.to_string())
        });

        let Some(query) = query else {
            return self.put_message(zenoh_key, payload, attributes).await;
        };

        let attachment = Self::uattributes_to_attachment(attributes).map_err(|err| {
            UStatus::fail_with_code(
                UCode::InvalidArgument,
                format!("Unable to transform UAttributes to attachment: {err}"),
            )
        })?;
        query
            .reply(query.key_expr().clone(), payload)
            .attachment(attachment)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("Unable to reply with Zenoh: {err}"),
                )
            })
    }

    async fn register_request_queryable(
        &self,
        zenoh_key: &str,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        let comparable_listener = ComparableListener::new(listener.clone());
        let map_key = (zenoh_key.to_string(), comparable_listener.clone());
        if self.queryable_map.lock().unwrap().contains_key(&map_key) {
            return Ok(());
        }

        let query_map = self.query_map.clone();
        let callback = move |query: Query| {
            let Some(attachment) = query.attachment() else {
                warn!("Ignoring Zenoh query without UAttributes attachment");
                return;
            };
            let attributes = match Self::attachment_to_uattributes(attachment) {
                Ok(attributes) => attributes,
                Err(err) => {
                    warn!("Unable to transform query attachment to UAttributes: {err}");
                    return;
                }
            };
            if attributes.check_expired().is_err() {
                return;
            }
            let message = match message_from_parts(
                &attributes,
                attributes.payload_encoding().and_then(|_| {
                    query
                        .payload()
                        .map(|payload| Bytes::copy_from_slice(payload.to_bytes().as_ref()))
                }),
            ) {
                Ok(message) => message,
                Err(err) => {
                    warn!("Unable to create UMessage from Zenoh query: {err}");
                    return;
                }
            };
            query_map
                .lock()
                .unwrap()
                .insert(attributes.id().to_string(), query);
            let listener = listener.clone();
            tokio::spawn(async move {
                listener.on_receive(message).await;
            });
        };

        let queryable = self
            .session
            .declare_queryable(zenoh_key)
            .callback_mut(callback)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("Unable to register Zenoh queryable: {err}"),
                )
            })?;
        self.queryable_map
            .lock()
            .unwrap()
            .insert(map_key, queryable);
        Ok(())
    }

    fn unregister_request_queryable(
        &self,
        zenoh_key: &str,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        if self
            .queryable_map
            .lock()
            .unwrap()
            .remove(&(zenoh_key.to_string(), ComparableListener::new(listener)))
            .is_none()
        {
            return Err(UStatus::fail_with_code(
                UCode::NotFound,
                format!("No RPC listener registered for key expression: {zenoh_key}"),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl UTransport for UPTransportZenoh {
    async fn send(&self, message: UMessage) -> Result<(), UStatus> {
        let attributes = message.attributes().clone();
        UAttributesValidators::validator_for_attributes(&attributes)
            .validate(&attributes)
            .map_err(|err| UStatus::fail_with_code(UCode::InvalidArgument, err.to_string()))?;

        let zenoh_key = self.to_zenoh_key_string(attributes.source(), attributes.sink());
        let payload = message.payload().unwrap_or_default();
        if attributes.type_() == up_rust::UMessageType::Response {
            self.send_response(&zenoh_key, payload, &attributes).await
        } else {
            self.put_message(&zenoh_key, payload, &attributes).await
        }
    }

    async fn receive(
        &self,
        _source_filter: &UUri,
        _sink_filter: Option<&UUri>,
    ) -> Result<UMessage, UStatus> {
        Err(UStatus::fail_with_code(
            UCode::Unimplemented,
            "not implemented",
        ))
    }

    async fn register_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        up_rust::verify_filter_criteria(source_filter, sink_filter).map_err(|err| *err)?;
        let zenoh_key = self.to_zenoh_key_string(source_filter, sink_filter);
        self.subscribers
            .register_subscriber(zenoh_key.clone(), listener.clone())
            .await?;

        if is_rpc_request_filter(source_filter, sink_filter) {
            if let Err(err) = self
                .register_request_queryable(&zenoh_key, listener.clone())
                .await
            {
                let _ = self
                    .subscribers
                    .unregister(&zenoh_key, ComparableListener::new(listener))
                    .await;
                return Err(err);
            }
        }
        Ok(())
    }

    async fn unregister_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        up_rust::verify_filter_criteria(source_filter, sink_filter).map_err(|err| *err)?;
        let zenoh_key = self.to_zenoh_key_string(source_filter, sink_filter);
        self.subscribers
            .unregister(&zenoh_key, ComparableListener::new(listener.clone()))
            .await?;
        if is_rpc_request_filter(source_filter, sink_filter) {
            self.unregister_request_queryable(&zenoh_key, listener)?;
        }
        Ok(())
    }
}
