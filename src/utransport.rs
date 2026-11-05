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
use up_rust::frame::metadata::try_project_frame_to_umessage;
use up_rust::{
    ComparableListener, UAttributes, UAttributesValidators, UCode, UListener, UMessage, UStatus,
    UTransport, UUri,
};
use zenoh::query::Query;

pub(crate) fn message_from_parts(
    attributes: &UAttributes,
    payload: Option<Bytes>,
) -> Result<UMessage, UStatus> {
    let metadata = attributes.try_project_to_frame_metadata().map_err(|err| {
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
    fn put_message(
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

        let reserved = crate::dispatch::reserve_publication(&self.session, zenoh_key)?;
        crate::dispatch::publish_reserved(reserved, || {
            use zenoh::Wait;
            self.session
                .put(zenoh_key, payload)
                .priority(priority)
                .attachment(attachment)
                .wait()
        })
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
        let attachment = Self::uattributes_to_attachment(attributes).map_err(|err| {
            UStatus::fail_with_code(
                UCode::InvalidArgument,
                format!("Unable to transform UAttributes to attachment: {err}"),
            )
        })?;
        let Some(query) = self.query_map.take_response(attributes)? else {
            return self.put_message(zenoh_key, payload, attributes);
        };
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

    pub(crate) async fn request_queryable(
        session: &zenoh::Session,
        zenoh_key: &str,
        dispatch: Arc<crate::dispatch::Dispatch<crate::pending_queries::MessageDelivery>>,
        query_map: Arc<crate::pending_queries::PendingQueries>,
    ) -> Result<zenoh::query::Queryable<()>, UStatus> {
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
            if attributes.type_() != up_rust::UMessageType::Request
                || UAttributesValidators::validator_for_attributes(&attributes)
                    .validate(&attributes)
                    .is_err()
                || attributes.check_expired().is_err()
            {
                return;
            }
            if attributes.payload_encoding().is_none()
                && query.payload().is_some_and(|payload| !payload.is_empty())
            {
                warn!("Ignoring Zenoh query with undeclared payload bytes");
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
            let Ok(admission) = query_map.insert(query, &attributes, dispatch.activity()) else {
                return;
            };
            // The ticket moves with the queue item. Rejection, discard and
            // cancellation drop it and roll back only this exact generation.
            let deadline = admission.deadline();
            dispatch.push_until(
                crate::pending_queries::MessageDelivery {
                    message,
                    query: Some(admission),
                },
                Some(deadline),
            );
        };

        session
            .declare_queryable(zenoh_key)
            .callback(callback)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("Unable to register Zenoh queryable: {err}"),
                )
            })
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
            self.put_message(&zenoh_key, payload, &attributes)
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
        let queries =
            is_rpc_request_filter(source_filter, sink_filter).then(|| Arc::clone(&self.query_map));
        self.subscribers
            .register_subscriber(zenoh_key, listener, queries)
            .await
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
            .unregister(&zenoh_key, ComparableListener::new(listener))
            .await
    }
}

#[cfg(test)]
mod malformed_presence_tests {
    use super::*;
    use std::time::Duration;
    use test_case::test_case;
    use up_rust::{PayloadEncoding, UMessageBuilder};

    struct Observe(tokio::sync::mpsc::UnboundedSender<UMessage>);
    #[async_trait]
    impl UListener for Observe {
        async fn on_receive(&self, message: UMessage) {
            self.0.send(message).unwrap();
        }
    }

    fn make_message(query: bool, source: &UUri, method: &UUri, payload: Option<Bytes>) -> UMessage {
        if query {
            let mut builder = UMessageBuilder::request(method.clone(), source.clone(), 10_000);
            match payload {
                None => builder.build(),
                Some(bytes) => builder.build_with_payload(bytes, PayloadEncoding::RAW),
            }
        } else {
            let mut builder = UMessageBuilder::publish(source.clone());
            match payload {
                None => builder.build(),
                Some(bytes) => builder.build_with_payload(bytes, PayloadEncoding::RAW),
            }
        }
        .unwrap()
    }

    #[test_case(false; "native classic subscriber")]
    #[test_case(true; "native classic request queryable")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonempty_bytes_without_encoding_are_rejected(query: bool) {
        let mut config = crate::zenoh_config::Config::default();
        config.insert_json5("listen/endpoints", "[]").unwrap();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        config
            .insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        let transport = UPTransportZenoh::builder("presence-regression")
            .unwrap()
            .with_session(zenoh::open(config).await.unwrap())
            .build()
            .unwrap();
        let source = UUri::try_from_parts(
            "presence-source",
            0x0001_4321,
            1,
            if query { 0 } else { 0x8001 },
        )
        .unwrap();
        let method = UUri::try_from_parts("presence-service", 0x0001_1234, 1, 1).unwrap();
        let sink = query.then_some(&method);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let listener: Arc<dyn UListener> = Arc::new(Observe(sender));
        transport
            .register_listener(&source, sink, Arc::clone(&listener))
            .await
            .unwrap();
        let key = transport.to_zenoh_key_string(&source, sink);
        let make = |payload| make_message(query, &source, &method, payload);
        let malformed = make(None);
        let absent = make(None);
        let empty = make(Some(Bytes::new()));
        let opaque = make(Some(Bytes::from_static(b"opaque")))
            .with_declared_payload_encoding(PayloadEncoding::from_id(0xFFFF).unwrap())
            .unwrap();
        // Native controls prove the listener path is active; malformed metadata
        // must never be transformed into a delivered payloadless message.
        for (message, bytes) in [
            (&malformed, &b"undeclared"[..]),
            (&absent, &b""[..]),
            (&empty, &b""[..]),
            (&opaque, &b"opaque"[..]),
        ] {
            let attachment =
                UPTransportZenoh::uattributes_to_attachment(message.attributes()).unwrap();
            if query {
                use zenoh::Wait;
                transport
                    .session
                    .get(&key)
                    .payload(bytes.to_vec())
                    .attachment(attachment)
                    .callback(|_| {})
                    .wait()
                    .unwrap();
            } else {
                transport
                    .session
                    .put(&key, bytes.to_vec())
                    .attachment(attachment)
                    .await
                    .unwrap();
            }
        }
        let mut delivered = Vec::new();
        for _ in 0..3 {
            delivered.push(
                tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        for expected in [&absent, &empty, &opaque] {
            let actual = delivered
                .iter()
                .find(|message| message.id() == expected.id())
                .expect("each valid native control must arrive");
            assert_eq!(actual.payload_encoding(), expected.payload_encoding());
            assert_eq!(actual.payload(), expected.payload());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), receiver.recv())
                .await
                .is_err()
        );
        transport
            .unregister_listener(&source, sink, listener)
            .await
            .unwrap();
    }
}
