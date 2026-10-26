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
use std::{string::ToString, sync::Arc, time::Instant};
use tracing::error;
use up_rust::{
    communication::{CallOptions, RpcClient, ServiceInvocationError, UPayload},
    LocalUriProvider, UAttributes, UAttributesValidators, UCode, UMessageBuilder, UMessageType,
    UStatus, UUri,
};
use zenoh::query::{ConsolidationMode, QueryTarget};

/// SDK RPC client that sends requests as Zenoh queries through its retained transport.
/// Callback-originated queries reserve local copied-control queue credit across
/// transports sharing the session, returning an explicit error on saturation.
/// Reply type, request ID and endpoints are validated before exposing payload;
/// absent and present-empty payloads remain distinct. Reply waiting uses the
/// request's original deadline, including time spent entering native delivery.
/// Each call stores at most its first reply in a one-shot channel; duplicate
/// native replies are discarded without blocking a native callback.
pub struct ZenohRpcClient {
    transport: Arc<UPTransportZenoh>,
}

fn response_payload(
    request: &UAttributes,
    sample: &zenoh::sample::Sample,
) -> Result<Option<UPayload>, ServiceInvocationError> {
    let invalid = |message: &str| ServiceInvocationError::InvalidArgument(message.to_owned());
    let attachment = sample
        .attachment()
        .ok_or_else(|| invalid("Zenoh reply has no attributes"))?;
    let attributes = UPTransportZenoh::attachment_to_uattributes(attachment)
        .map_err(|error| invalid(&format!("invalid Zenoh reply attributes: {error}")))?;
    UAttributesValidators::validator_for_attributes(&attributes)
        .validate(&attributes)
        .map_err(|error| invalid(&error.to_string()))?;
    if attributes.type_() != UMessageType::Response
        || attributes.request_id() != Some(request.id())
        || Some(attributes.source()) != request.sink()
        || attributes.sink() != Some(request.source())
    {
        return Err(invalid("Zenoh reply does not match the RPC request"));
    }
    attributes
        .check_expired()
        .map_err(|_| ServiceInvocationError::DeadlineExceeded)?;
    let payload = match attributes.payload_encoding() {
        Some(_) => Some(Bytes::copy_from_slice(sample.payload().to_bytes().as_ref())),
        None if sample.payload().is_empty() => None,
        None => return Err(invalid("Zenoh reply carries undeclared payload bytes")),
    };
    let message = crate::utransport::message_from_parts(&attributes, payload)
        .map_err(ServiceInvocationError::from)?;
    if let Some(code) = attributes.commstatus().filter(|code| *code != UCode::Ok) {
        let status = message.extract_protobuf().unwrap_or_else(|_| {
            UStatus::fail_with_code(code, "failed to invoke service operation")
        });
        return Err(ServiceInvocationError::from(status));
    }
    Ok(message
        .payload()
        .zip(attributes.payload_encoding())
        .map(|(payload, encoding)| UPayload::new(payload, encoding)))
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
        let deadline = crate::pending_queries::request_deadline(&attributes)
            .map_err(ServiceInvocationError::from)?;
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
        .consolidation(ConsolidationMode::None)
        .timeout(deadline.saturating_duration_since(Instant::now()));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        let getbuilder = getbuilder.callback_mut(move |reply| {
            if let Some(sender) = sender.take() {
                // One RPC completion, not a general-purpose reply mailbox. The
                // native callback never waits for an unpolled/cancelled caller.
                let _ = sender.send(reply);
            }
        });
        let reserved = crate::dispatch::reserve_publication(&self.transport.session, &zenoh_key)
            .map_err(ServiceInvocationError::from)?;
        let replies = crate::dispatch::publish_reserved(reserved, || {
            use zenoh::Wait;
            getbuilder.wait()
        });
        let Ok(()) = replies else {
            let msg = "Error while sending Zenoh query".to_string();
            error!("{msg}");
            return Err(ServiceInvocationError::RpcError(Box::new(
                UStatus::fail_with_code(UCode::Internal, msg),
            )));
        };

        // Receive the reply
        let reply = tokio::time::timeout_at(deadline.into(), receiver)
            .await
            .map_err(|_| ServiceInvocationError::DeadlineExceeded)?;
        if Instant::now() >= deadline {
            return Err(ServiceInvocationError::DeadlineExceeded);
        }
        let Ok(reply) = reply else {
            let msg = "Error while receiving Zenoh reply".to_string();
            error!("{msg}");
            return Err(ServiceInvocationError::RpcError(Box::new(
                UStatus::fail_with_code(UCode::Internal, msg),
            )));
        };
        match reply.into_result() {
            Ok(sample) => response_payload(&attributes, &sample),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, Weak};
    use up_rust::{PayloadEncoding, UListener, UMessage, UTransport, UUID};

    #[derive(Clone, Copy)]
    enum ReplyKind {
        Absent,
        Empty,
        Body,
        WrongId,
        WrongSource,
        NotResponse,
        ErrorStatus,
        Duplicate,
    }

    async fn transport() -> Arc<UPTransportZenoh> {
        let mut config = zenoh::Config::default();
        config.insert_json5("listen/endpoints", "[]").unwrap();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        Arc::new(
            UPTransportZenoh::new(config, "//replies/1/1/0")
                .await
                .unwrap(),
        )
    }

    fn reply_message(kind: ReplyKind, request: &UAttributes) -> UMessage {
        if matches!(kind, ReplyKind::NotResponse) {
            return UMessageBuilder::publish(UUri::try_from("//replies/2/1/8001").unwrap())
                .build()
                .unwrap();
        }
        let mut builder = match kind {
            ReplyKind::WrongId => UMessageBuilder::response(
                request.source().clone(),
                UUID::build(),
                request.sink().unwrap().clone(),
            ),
            ReplyKind::WrongSource => UMessageBuilder::response(
                request.source().clone(),
                request.id().clone(),
                UUri::try_from("//foreign/2/1/1").unwrap(),
            ),
            _ => UMessageBuilder::response_for_request(request),
        };
        if matches!(kind, ReplyKind::ErrorStatus) {
            builder.with_comm_status(UCode::NotFound);
        }
        match kind {
            ReplyKind::Empty => builder.build_with_payload(Bytes::new(), PayloadEncoding::TEXT),
            ReplyKind::Body | ReplyKind::Duplicate => {
                builder.build_with_payload("reply", PayloadEncoding::TEXT)
            }
            _ => builder.build(),
        }
        .unwrap()
    }

    #[test_case::test_case(ReplyKind::Absent; "absent payload")]
    #[test_case::test_case(ReplyKind::Empty; "present empty payload")]
    #[test_case::test_case(ReplyKind::Body; "declared payload")]
    #[test_case::test_case(ReplyKind::WrongId; "foreign request ID")]
    #[test_case::test_case(ReplyKind::WrongSource; "foreign source endpoint")]
    #[test_case::test_case(ReplyKind::NotResponse; "wrong message type")]
    #[test_case::test_case(ReplyKind::ErrorStatus; "status only error")]
    #[test_case::test_case(ReplyKind::Duplicate; "first native reply wins without blocking")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn client_validates_real_native_reply_before_returning_payload(kind: ReplyKind) {
        let transport = transport().await;
        let queryable = transport
            .session
            .declare_queryable("up/**")
            .callback(move |query: zenoh::query::Query| {
                use zenoh::Wait;
                let request =
                    UPTransportZenoh::attachment_to_uattributes(query.attachment().unwrap())
                        .unwrap();
                let response = reply_message(kind, &request);
                query
                    .reply(
                        query.key_expr().clone(),
                        response.payload().unwrap_or_default(),
                    )
                    .attachment(
                        UPTransportZenoh::uattributes_to_attachment(response.attributes()).unwrap(),
                    )
                    .wait()
                    .unwrap();
                if matches!(kind, ReplyKind::Duplicate) {
                    for _ in 0..3 {
                        query
                            .reply(query.key_expr().clone(), "duplicate")
                            .attachment(
                                UPTransportZenoh::uattributes_to_attachment(response.attributes())
                                    .unwrap(),
                            )
                            .wait()
                            .unwrap();
                    }
                }
            })
            .await
            .unwrap();
        let result = ZenohRpcClient::new(Arc::clone(&transport))
            .invoke_method(
                UUri::try_from("//replies/2/1/1").unwrap(),
                CallOptions::for_rpc_request(5000, None, None, None),
                None,
            )
            .await;
        match kind {
            ReplyKind::Absent => assert!(result.unwrap().is_none()),
            ReplyKind::Empty => assert!(result.unwrap().unwrap().payload().is_empty()),
            ReplyKind::Body | ReplyKind::Duplicate => {
                let payload = result.unwrap().unwrap();
                assert_eq!(payload.payload().as_ref(), b"reply");
                assert_eq!(payload.payload_encoding(), PayloadEncoding::TEXT);
            }
            ReplyKind::WrongId | ReplyKind::WrongSource | ReplyKind::NotResponse => {
                assert!(matches!(
                    result,
                    Err(ServiceInvocationError::InvalidArgument(_))
                ));
            }
            ReplyKind::ErrorStatus => {
                assert!(matches!(result, Err(ServiceInvocationError::NotFound(_))));
            }
        }
        queryable.undeclare().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn native_reply_wait_expires_at_the_request_deadline() {
        let transport = transport().await;
        let queryable = transport
            .session
            .declare_queryable("up/**")
            .with(zenoh::handlers::FifoChannel::new(1))
            .await
            .unwrap();
        let started = Instant::now();
        let result = ZenohRpcClient::new(transport.clone())
            .invoke_method(
                UUri::try_from("//replies/2/1/1").unwrap(),
                CallOptions::for_rpc_request(100, None, None, None),
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(ServiceInvocationError::DeadlineExceeded)
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        queryable.undeclare().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cancelled_reply_wait_does_not_block_late_native_delivery() {
        use zenoh::Wait;
        let transport = transport().await;
        let queryable = transport
            .session
            .declare_queryable("up/**")
            .with(zenoh::handlers::FifoChannel::new(1))
            .await
            .unwrap();
        let client = ZenohRpcClient::new(Arc::clone(&transport));
        let calling = tokio::spawn(async move {
            client
                .invoke_method(
                    UUri::try_from("//replies/2/1/1").unwrap(),
                    CallOptions::for_rpc_request(5000, None, None, None),
                    None,
                )
                .await
        });
        let query = tokio::time::timeout(std::time::Duration::from_secs(2), queryable.recv_async())
            .await
            .unwrap()
            .unwrap();
        calling.abort();
        assert!(calling.await.unwrap_err().is_cancelled());
        let attributes =
            UPTransportZenoh::attachment_to_uattributes(query.attachment().unwrap()).unwrap();
        let response = reply_message(ReplyKind::Body, &attributes);
        query
            .reply(query.key_expr().clone(), response.payload().unwrap())
            .attachment(UPTransportZenoh::uattributes_to_attachment(response.attributes()).unwrap())
            .wait()
            .unwrap();
        drop(query);
        queryable.undeclare().await.unwrap();
    }

    struct OrderedEcho {
        transport: Weak<UPTransportZenoh>,
        requests: Arc<Mutex<Vec<u64>>>,
    }

    #[async_trait]
    impl UListener for OrderedEcho {
        async fn on_receive(&self, message: UMessage) {
            let payload = message.payload().unwrap();
            let sequence = u64::from_le_bytes(payload.as_ref().try_into().unwrap());
            if sequence == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            self.requests.lock().unwrap().push(sequence);
            let response = UMessageBuilder::response_for_request(message.attributes())
                .build_with_payload(payload, PayloadEncoding::RAW)
                .unwrap();
            self.transport
                .upgrade()
                .unwrap()
                .send(response)
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn native_queries_and_correlated_replies_remain_fifo_under_backlog() {
        const COUNT: usize = 1000;
        let transport = transport().await;
        let source = transport.get_source_uri();
        let method = UUri::try_from("//replies/2/1/1").unwrap();
        let key = transport.to_zenoh_key_string(&source, Some(&method));
        let requests = Arc::new(Mutex::new(Vec::with_capacity(COUNT)));
        let replies = Arc::new(Mutex::new(Vec::with_capacity(COUNT)));
        let listener: Arc<dyn UListener> = Arc::new(OrderedEcho {
            transport: Arc::downgrade(&transport),
            requests: Arc::clone(&requests),
        });
        transport
            .register_listener(&source, Some(&method), Arc::clone(&listener))
            .await
            .unwrap();
        for sequence in 0..u64::try_from(COUNT).unwrap() {
            let message = UMessageBuilder::request(method.clone(), source.clone(), 10_000)
                .build_with_payload(sequence.to_le_bytes().to_vec(), PayloadEncoding::RAW)
                .unwrap();
            let id = message.attributes().id().clone();
            let received = Arc::clone(&replies);
            transport
                .session
                .get(&key)
                .payload(message.payload().unwrap())
                .attachment(
                    UPTransportZenoh::uattributes_to_attachment(message.attributes()).unwrap(),
                )
                .consolidation(ConsolidationMode::None)
                .callback(move |reply: zenoh::query::Reply| {
                    let sample = reply.into_result().unwrap();
                    let attributes =
                        UPTransportZenoh::attachment_to_uattributes(sample.attachment().unwrap())
                            .unwrap();
                    assert_eq!(attributes.request_id(), Some(&id));
                    let value = u64::from_le_bytes(
                        sample.payload().to_bytes().as_ref().try_into().unwrap(),
                    );
                    assert_eq!(value, sequence);
                    received.lock().unwrap().push(value);
                })
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while replies.lock().unwrap().len() != COUNT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let expected: Vec<_> = (0..u64::try_from(COUNT).unwrap()).collect();
        assert_eq!(*requests.lock().unwrap(), expected);
        assert_eq!(*replies.lock().unwrap(), expected);
        transport
            .unregister_listener(&source, Some(&method), listener)
            .await
            .unwrap();
    }
}
