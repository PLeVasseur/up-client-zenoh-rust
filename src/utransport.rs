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
use crate::{MessageFlag, UPTransportZenoh, CB_RUNTIME};
use async_trait::async_trait;
use bytes::Bytes;
use std::{
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    runtime::{Handle, Runtime},
    task,
};
use tracing::{error, warn};
use up_rust::{
    try_project_attributes_to_frame_metadata, try_project_frame_to_umessage, ComparableListener,
    UAttributes, UAttributesValidators, UCode, UListener, UMessage, UMessageType, UStatus,
    UTransport, UUri,
};
use zenoh::{
    key_expr::keyexpr,
    query::{Query, QueryTarget, Reply},
    sample::Sample,
};

static TOKIO_RUNTIME: LazyLock<Mutex<Runtime>> =
    LazyLock::new(|| Mutex::new(Runtime::new().unwrap()));

#[inline]
fn invoke_block_callback(listener: &Arc<dyn UListener>, resp_msg: UMessage) {
    match Handle::try_current() {
        Ok(handle) => {
            task::block_in_place(|| {
                handle.block_on(listener.on_receive(resp_msg));
            });
        }
        Err(_) => {
            TOKIO_RUNTIME
                .lock()
                .unwrap()
                .block_on(listener.on_receive(resp_msg));
        }
    }
}

#[inline]
fn spawn_nonblock_callback(listener: &Arc<dyn UListener>, listener_msg: UMessage) {
    let listener = listener.clone();
    CB_RUNTIME.spawn(async move {
        listener.on_receive(listener_msg).await;
    });
}

fn message_from_parts(
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

impl UPTransportZenoh {
    async fn send_publish_notification(
        &self,
        zenoh_key: &str,
        payload: &[u8],
        attributes: UAttributes,
    ) -> Result<(), UStatus> {
        // Transform UAttributes to user attachment in Zenoh
        let Ok(attachment) = UPTransportZenoh::uattributes_to_attachment(&attributes) else {
            let msg = "Unable to transform UAttributes to attachment".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg));
        };

        // Map the priority to Zenoh
        let priority = UPTransportZenoh::map_zenoh_priority(
            attributes.priority().unwrap_or(up_rust::UPriority::CS1),
        );

        // Send data
        let putbuilder = self
            .session
            .put(zenoh_key, payload)
            .priority(priority)
            .attachment(attachment);
        putbuilder
            .await
            .map_err(|_| UStatus::fail_with_code(UCode::Internal, "Unable to send with Zenoh"))?;

        Ok(())
    }

    async fn send_request(
        &self,
        zenoh_key: &str,
        payload: &[u8],
        attributes: UAttributes,
    ) -> Result<(), UStatus> {
        // Transform UAttributes to user attachment in Zenoh
        let Ok(attachment) = UPTransportZenoh::uattributes_to_attachment(&attributes) else {
            let msg = "Unable to transform UAttributes to attachment".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg));
        };

        // Retrieve the callback
        let zenoh_key = keyexpr::new(zenoh_key).map_err(|err| {
            UStatus::fail_with_code(UCode::InvalidArgument, format!("invalid Zenoh key: {err}"))
        })?;
        let resp_callback =
            self.rpc_callback_map
                .lock()
                .unwrap()
                .iter()
                .find_map(|(saved_key, callback)| {
                    zenoh_key.intersects(saved_key).then(|| callback.clone())
                });
        let Some(resp_callback) = resp_callback else {
            let msg = "Unable to get callback".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::Internal, msg));
        };
        let zenoh_callback = move |reply: Reply| {
            match reply.into_result() {
                Ok(sample) => {
                    // Get UAttribute from the attachment
                    let Some(attachment) = sample.attachment() else {
                        warn!("Unable to get the attachment");
                        return;
                    };
                    let u_attribute = match UPTransportZenoh::attachment_to_uattributes(attachment)
                    {
                        Ok(uattr) => uattr,
                        Err(e) => {
                            warn!("Unable to transform attachment to UAttributes: {e:?}");
                            return;
                        }
                    };
                    let Ok(message) = message_from_parts(
                        &u_attribute,
                        Some(Bytes::copy_from_slice(sample.payload().to_bytes().as_ref())),
                    ) else {
                        warn!("Unable to create UMessage from Zenoh reply");
                        return;
                    };
                    invoke_block_callback(&resp_callback, message);
                }
                Err(e) => {
                    warn!("Unable to parse Zenoh reply: {e:?}");
                }
            }
        };

        // Send query
        let getbuilder = self
            .session
            .get(zenoh_key)
            .payload(payload.to_vec())
            .attachment(attachment)
            .target(QueryTarget::BestMatching)
            .timeout(Duration::from_millis(u64::from(
                attributes.ttl().unwrap_or(1000),
            )))
            .callback(zenoh_callback);
        getbuilder.await.map_err(|e| {
            let msg = format!("Unable to send get with Zenoh: {e:?}");
            error!("{msg}");
            UStatus::fail_with_code(UCode::Internal, msg)
        })?;

        Ok(())
    }

    async fn send_response(&self, payload: &[u8], attributes: UAttributes) -> Result<(), UStatus> {
        // Transform UAttributes to user attachment in Zenoh
        let Ok(attachment) = UPTransportZenoh::uattributes_to_attachment(&attributes) else {
            let msg = "Unable to transform UAttributes to attachment".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg));
        };

        // Find out the corresponding query from HashMap
        let reqid = attributes
            .request_id()
            .ok_or_else(|| {
                UStatus::fail_with_code(UCode::InvalidArgument, "response attributes missing reqid")
            })?
            .to_string();
        let query = self
            .query_map
            .lock()
            .unwrap()
            .remove(&reqid)
            .ok_or_else(|| {
                let msg = "query doesn't exist".to_string();
                error!("{msg}");
                UStatus::fail_with_code(UCode::Internal, msg)
            })?
            .clone();

        // Send back the query
        query
            .reply(query.key_expr().clone(), payload.to_vec())
            .attachment(attachment)
            .await
            .map_err(|e| {
                let msg = format!("Unable to reply with Zenoh: {e:?}");
                error!("{msg}");
                UStatus::fail_with_code(UCode::Internal, msg)
            })?;

        Ok(())
    }

    async fn register_publish_notification_listener(
        &self,
        zenoh_key: &String,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        // Setup callback
        let listener_cloned = listener.clone();
        let callback = move |sample: Sample| {
            // Get the UAttribute from Zenoh user attachment
            let Some(attachment) = sample.attachment() else {
                warn!("Unable to get attachment");
                return;
            };
            let u_attribute = match UPTransportZenoh::attachment_to_uattributes(attachment) {
                Ok(uattributes) => uattributes,
                Err(e) => {
                    warn!("Unable to transform attachement to UAttributes: {e:?}");
                    return;
                }
            };
            // Create UMessage
            let Ok(msg) = message_from_parts(
                &u_attribute,
                Some(Bytes::copy_from_slice(sample.payload().to_bytes().as_ref())),
            ) else {
                warn!("Unable to create UMessage from Zenoh sample");
                return;
            };
            spawn_nonblock_callback(&listener_cloned, msg);
        };

        // Create Zenoh subscriber
        if let Ok(subscriber) = self
            .session
            .declare_subscriber(zenoh_key)
            .callback_mut(callback)
            .await
        {
            self.subscriber_map.lock().unwrap().insert(
                (zenoh_key.clone(), ComparableListener::new(listener)),
                subscriber,
            );
        } else {
            let msg = "Unable to register callback with Zenoh";
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::Internal, msg));
        }

        Ok(())
    }

    async fn register_request_listener(
        &self,
        zenoh_key: &String,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        // Setup callback
        let listener_cloned = listener.clone();
        let query_map = self.query_map.clone();
        let callback = move |query: Query| {
            // Create UAttribute from Zenoh user attachment
            let Some(attachment) = query.attachment() else {
                warn!("Unable to get attachment");
                return;
            };
            let u_attribute = match UPTransportZenoh::attachment_to_uattributes(attachment) {
                Ok(uattributes) => uattributes,
                Err(e) => {
                    warn!("Unable to transform user attachment to UAttributes: {e:?}");
                    return;
                }
            };
            // Create UMessage and store the query into HashMap (Will be used in send_response)
            let Ok(msg) = message_from_parts(
                &u_attribute,
                query
                    .payload()
                    .map(|payload| Bytes::copy_from_slice(payload.to_bytes().as_ref())),
            ) else {
                warn!("Unable to create UMessage from Zenoh query");
                return;
            };
            query_map
                .lock()
                .unwrap()
                .insert(u_attribute.id().to_string(), query);
            spawn_nonblock_callback(&listener_cloned, msg);
        };

        // Create Zenoh queryable
        if let Ok(queryable) = self
            .session
            .declare_queryable(zenoh_key)
            .callback_mut(callback)
            .await
        {
            self.queryable_map.lock().unwrap().insert(
                (zenoh_key.clone(), ComparableListener::new(listener)),
                queryable,
            );
        } else {
            let msg = "Unable to register callback with Zenoh".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::Internal, msg));
        }

        Ok(())
    }

    fn register_response_listener(&self, zenoh_key: &str, listener: Arc<dyn UListener>) {
        // Store the response callback (Will be used in send_request)
        if let Ok(zenoh_key) = keyexpr::new(zenoh_key) {
            self.rpc_callback_map
                .lock()
                .unwrap()
                .insert(zenoh_key.to_owned(), listener);
        }
    }
}

#[async_trait]
impl UTransport for UPTransportZenoh {
    async fn send(&self, message: UMessage) -> Result<(), UStatus> {
        let attributes = message.attributes().clone();

        // Get Zenoh key
        let source = attributes.source().clone();
        let zenoh_key = if let Some(sink) = attributes.sink().cloned() {
            self.to_zenoh_key_string(&source, Some(&sink))
        } else {
            self.to_zenoh_key_string(&source, None)
        };

        // Get payload
        let payload = if let Some(payload) = message.payload() {
            payload.to_vec()
        } else {
            vec![]
        };

        // Check the type of UAttributes (Publish / Notification / Request / Response)
        match attributes.type_() {
            UMessageType::Publish => {
                UAttributesValidators::Publish
                    .validator()
                    .validate(&attributes)
                    .map_err(|e| {
                        let msg = format!("Wrong Publish UAttributes: {e:?}");
                        error!("{msg}");
                        UStatus::fail_with_code(UCode::InvalidArgument, msg)
                    })?;
                // Send Publish
                self.send_publish_notification(&zenoh_key, &payload, attributes)
                    .await
            }
            UMessageType::Notification => {
                UAttributesValidators::Notification
                    .validator()
                    .validate(&attributes)
                    .map_err(|e| {
                        let msg = format!("Wrong Notification UAttributes: {e:?}");
                        error!("{msg}");
                        UStatus::fail_with_code(UCode::InvalidArgument, msg)
                    })?;
                // Send Publish
                self.send_publish_notification(&zenoh_key, &payload, attributes)
                    .await
            }
            UMessageType::Request => {
                UAttributesValidators::Request
                    .validator()
                    .validate(&attributes)
                    .map_err(|e| {
                        let msg = format!("Wrong Request UAttributes: {e:?}");
                        error!("{msg}");
                        UStatus::fail_with_code(UCode::InvalidArgument, msg)
                    })?;
                // Send Request
                self.send_request(&zenoh_key, &payload, attributes).await
            }
            UMessageType::Response => {
                UAttributesValidators::Response
                    .validator()
                    .validate(&attributes)
                    .map_err(|e| {
                        let msg = format!("Wrong Response UAttributes: {e:?}");
                        error!("{msg}");
                        UStatus::fail_with_code(UCode::InvalidArgument, msg)
                    })?;
                // Send Response
                self.send_response(&payload, attributes).await
            }
        }
    }

    async fn receive(
        &self,
        _source_filter: &UUri,
        _sink_filter: Option<&UUri>,
    ) -> Result<UMessage, UStatus> {
        let msg = "Not implemented".to_string();
        error!("{msg}");
        Err(UStatus::fail_with_code(UCode::Unimplemented, msg))
    }

    async fn register_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UListener>,
    ) -> Result<(), UStatus> {
        let flag = UPTransportZenoh::get_listener_message_type(source_filter, sink_filter)?;
        // Publish & Notification
        if flag.contains(MessageFlag::Publish) || flag.contains(MessageFlag::Notification) {
            // Get Zenoh key
            let zenoh_key = self.to_zenoh_key_string(source_filter, sink_filter);
            self.register_publish_notification_listener(&zenoh_key, listener.clone())
                .await?;
        }
        // RPC request
        if flag.contains(MessageFlag::Request) {
            // Get Zenoh key
            let zenoh_key = self.to_zenoh_key_string(source_filter, sink_filter);
            self.register_request_listener(&zenoh_key, listener.clone())
                .await?;
        }
        // RPC response
        if flag.contains(MessageFlag::Response) {
            if let Some(sink_filter) = sink_filter {
                // Get Zenoh key
                let zenoh_key = self.to_zenoh_key_string(sink_filter, Some(source_filter));
                self.register_response_listener(&zenoh_key, listener.clone());
            } else {
                return Err(UStatus::fail_with_code(
                    UCode::InvalidArgument,
                    "Sink should not be None in Response",
                ));
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
        let flag = UPTransportZenoh::get_listener_message_type(source_filter, sink_filter)?;
        // Publish & Notification
        if flag.contains(MessageFlag::Publish) || flag.contains(MessageFlag::Notification) {
            // Get Zenoh key
            let zenoh_key = self.to_zenoh_key_string(source_filter, sink_filter);
            if self
                .subscriber_map
                .lock()
                .unwrap()
                .remove(&(zenoh_key.clone(), ComparableListener::new(listener.clone())))
                .is_none()
            {
                let msg = "Publish / Notifcation listener doesn't exist".to_string();
                warn!("{msg}");
                return Err(UStatus::fail_with_code(UCode::NotFound, msg));
            }
        }
        // RPC request
        if flag.contains(MessageFlag::Request) {
            // Get Zenoh key
            let zenoh_key = self.to_zenoh_key_string(source_filter, sink_filter);
            if self
                .queryable_map
                .lock()
                .unwrap()
                .remove(&(zenoh_key.clone(), ComparableListener::new(listener.clone())))
                .is_none()
            {
                let msg = "RPC request listener doesn't exist".to_string();
                warn!("{msg}");
                return Err(UStatus::fail_with_code(UCode::NotFound, msg));
            }
        }
        // RPC response
        if flag.contains(MessageFlag::Response) {
            if let Some(sink_filter) = sink_filter {
                // Get Zenoh key
                let zenoh_key = self.to_zenoh_key_string(sink_filter, Some(source_filter));
                let zenoh_key = keyexpr::new(&zenoh_key).map_err(|err| {
                    UStatus::fail_with_code(
                        UCode::InvalidArgument,
                        format!("invalid Zenoh key: {err}"),
                    )
                })?;
                if self
                    .rpc_callback_map
                    .lock()
                    .unwrap()
                    .remove(zenoh_key)
                    .is_none()
                {
                    let msg = "RPC response callback doesn't exist".to_string();
                    warn!("{msg}");
                    return Err(UStatus::fail_with_code(UCode::NotFound, msg));
                }
            } else {
                return Err(UStatus::fail_with_code(
                    UCode::InvalidArgument,
                    "Sink should not be None in Response",
                ));
            }
        }

        Ok(())
    }
}
