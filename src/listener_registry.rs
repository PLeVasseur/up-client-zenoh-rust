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

use std::{collections::HashMap, sync::Arc};

use crate::dispatch::Dispatch;
use crate::pending_queries::{MessageDelivery, PendingQueries};
use bytes::Bytes;
use std::sync::Mutex;
use tokio::sync::Notify;
use tracing::{debug, enabled, info, warn, Level};
use up_rust::{
    ComparableListener, RegistrationHandler, UAttributes, UAttributesValidators, UCode, UListener,
    UStatus,
};
use zenoh::{bytes::ZBytes, pubsub::Subscriber, query::Queryable, sample::Sample, Session};

fn attachment_to_uattributes(attachment: &ZBytes) -> anyhow::Result<UAttributes> {
    if attachment.len() < 2 {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "message has no/invalid attachment",
        )
        .into());
    }

    let attachment_bytes = attachment.to_bytes();
    let ver = attachment_bytes[0];
    if ver != crate::UPROTOCOL_MAJOR_VERSION {
        let msg = format!(
            "Expected UAttributes version {} but found version {}",
            crate::UPROTOCOL_MAJOR_VERSION,
            ver
        );
        info!("{msg}");
        return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg).into());
    }
    let uattributes = crate::UPTransportZenoh::attachment_to_uattributes(attachment)?;
    // [impl->dsn~utransport-registerlistener-discard-invalid-messages~1]
    let validator = UAttributesValidators::validator_for_attributes(&uattributes);
    Ok(validator.validate(&uattributes).map(|()| uattributes)?)
}

fn dispatch_sample(dispatch: &Dispatch<MessageDelivery>, sample: Sample) {
    let Some(attachment) = sample.attachment() else {
        warn!(
            "Ignoring Zenoh Sample without attachment [key expr: {}]",
            sample.key_expr()
        );
        return;
    };
    let attributes = match attachment_to_uattributes(attachment) {
        Ok(attributes) => attributes,
        Err(error) => {
            warn!("Unable to transform attachment to valid UAttributes: {error:?}");
            return;
        }
    };
    // [impl->dsn~up-attributes-ttl~1]
    // [impl->dsn~up-attributes-ttl-timeout~1]
    if attributes.check_expired().is_ok() {
        let Ok(message) = crate::utransport::message_from_parts(
            &attributes,
            attributes
                .payload_encoding()
                .map(|_| Bytes::copy_from_slice(sample.payload().to_bytes().as_ref())),
        ) else {
            warn!("Unable to create UMessage from Zenoh sample");
            return;
        };
        // The compatibility message owns copied bytes. Release the native Sample
        // before waiting for the one copied-value queue credit.
        drop(sample);
        dispatch.push(message.into());
    } else if enabled!(Level::DEBUG) {
        let id = attributes.id();
        if let Some(ttl) = attributes.ttl() {
            let ts = id.time();
            debug!(
                "discarding expired message [id: {}, created: {ts}, ttl: {ttl}]",
                id.to_hyphenated_string()
            );
        }
    }
}

// mapping of (Zenoh Key expression, Message Listener) ->  Zenoh Subscriber
type SubscriberMap = Mutex<HashMap<(String, ComparableListener), Arc<SubscriberEntry>>>;

struct SubscriberEntry {
    dispatch: Arc<Dispatch<MessageDelivery>>,
    native: Mutex<Option<NativeRegistration>>,
    queries: Option<Arc<PendingQueries>>,
    outcome: Mutex<Option<Result<(), UStatus>>>,
    ready: Notify,
}

impl Drop for SubscriberEntry {
    fn drop(&mut self) {
        // Close before the native handle drops, including transport destruction
        // while a native callback still holds a dispatch reference.
        self.dispatch.close();
        if let Some(queries) = &self.queries {
            queries.remove_owner(self.dispatch.activity());
        }
    }
}

struct NativeRegistration {
    subscriber: Subscriber<()>,
    queryable: Option<Queryable<()>>,
}

impl NativeRegistration {
    async fn new(session: &Session, key: &str, entry: &SubscriberEntry) -> Result<Self, UStatus> {
        entry.dispatch.bind(session, key).await?;
        let dispatch = Arc::clone(&entry.dispatch);
        // [impl->dsn~supported-message-delivery-methods~1]
        // [impl->dsn~utransport-registerlistener-start-invoking-listeners~1]
        let subscriber = session
            .declare_subscriber(key)
            .callback(move |sample| dispatch_sample(&dispatch, sample))
            .await
            .map_err(|error| UStatus::fail_with_code(UCode::Internal, error.to_string()))?;
        let queryable = if let Some(queries) = &entry.queries {
            Some(
                crate::UPTransportZenoh::request_queryable(
                    session,
                    key,
                    Arc::clone(&entry.dispatch),
                    Arc::clone(queries),
                )
                .await?,
            )
        } else {
            None
        };
        Ok(Self {
            subscriber,
            queryable,
        })
    }

    async fn undeclare(self) -> Result<(), UStatus> {
        let subscriber = self.subscriber.undeclare().await;
        let queryable = if let Some(queryable) = self.queryable {
            queryable.undeclare().await
        } else {
            Ok(())
        };
        subscriber.and(queryable).map_err(|error| {
            UStatus::fail_with_code(
                UCode::Internal,
                format!("Failed to undeclare listener: {error}"),
            )
        })
    }
}
impl SubscriberEntry {
    async fn wait_ready(&self) -> Result<(), UStatus> {
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if let Some(result) = self.outcome.lock().unwrap().clone() {
                return result;
            }
            ready.await;
        }
    }
    fn retire(&self, status: UStatus) -> Option<NativeRegistration> {
        self.dispatch.close();
        let mut outcome = self.outcome.lock().unwrap();
        *outcome = Some(Err(status));
        let native = self.native.lock().unwrap().take();
        drop(outcome);
        self.ready.notify_waiters();
        if let Some(queries) = &self.queries {
            queries.remove_owner(self.dispatch.activity());
        }
        native
    }
}
struct Creating<'a> {
    registry: &'a ListenerRegistry,
    key: (String, ComparableListener),
    entry: Arc<SubscriberEntry>,
    armed: bool,
}
impl Drop for Creating<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut entries = self.registry.subscribers.lock().unwrap();
            if entries
                .get(&self.key)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
            {
                entries.remove(&self.key);
            }
            drop(entries);
            drop(self.entry.retire(UStatus::fail_with_code(
                UCode::Cancelled,
                "listener creation cancelled",
            )));
        }
    }
}

struct MessageHandler(ComparableListener);
impl RegistrationHandler<MessageDelivery> for MessageHandler {
    async fn on_receive(&mut self, message: MessageDelivery) {
        message.deliver(&*self.0).await;
    }
}

pub(crate) struct ListenerRegistry {
    subscribers: SubscriberMap,
    session: Arc<Session>,
    max_subscribers: usize,
}

impl Drop for ListenerRegistry {
    fn drop(&mut self) {
        // Wake every copied-control producer before retiring any native handle.
        for entry in self.subscribers.get_mut().unwrap().values() {
            entry.dispatch.close();
        }
    }
}

impl ListenerRegistry {
    /// Creates a new registry with a given capacity.
    ///
    /// # Arguments
    ///
    /// * `zenoh_session` - The Zenoh session to use for creating subscribers.
    /// * `max_subscribers` - The maximum number of subscribers that can be registered.
    // [impl->req~utransport-registerlistener-max-listeners~1]
    pub fn new(zenoh_session: Arc<Session>, max_subscribers: usize) -> Self {
        Self {
            subscribers: Mutex::new(HashMap::new()),
            session: zenoh_session,
            max_subscribers,
        }
    }

    pub async fn register_subscriber(
        &self,
        zenoh_key: String,
        listener: Arc<dyn UListener>,
        queries: Option<Arc<PendingQueries>>,
    ) -> Result<(), UStatus> {
        let comparable_listener = ComparableListener::new(listener);
        let key = (zenoh_key.clone(), comparable_listener.clone());
        let (entry, creating) = {
            let mut entries = self.subscribers.lock().unwrap();
            if let Some(entry) = entries.get(&key) {
                (Arc::clone(entry), false)
            } else {
                if entries.len() >= self.max_subscribers {
                    return Err(UStatus::fail_with_code(
                        UCode::ResourceExhausted,
                        format!(
                            "Maximum number of listeners reached: {}",
                            self.max_subscribers
                        ),
                    ));
                }
                let entry = Arc::new(SubscriberEntry {
                    dispatch: Dispatch::new(MessageHandler(comparable_listener.clone()))?,
                    native: Mutex::new(None),
                    queries,
                    outcome: Mutex::new(None),
                    ready: Notify::new(),
                });
                entries.insert(key.clone(), Arc::clone(&entry));
                (entry, true)
            }
        };
        // Concurrent duplicates share the pending result without holding the map
        // lock over native declaration, another registration, or user work.
        if !creating {
            return entry.wait_ready().await;
        }
        let mut creation = Creating {
            registry: self,
            key,
            entry: Arc::clone(&entry),
            armed: true,
        };
        match NativeRegistration::new(&self.session, &zenoh_key, &entry).await {
            Ok(native) => {
                let rejected = {
                    let mut outcome = entry.outcome.lock().unwrap();
                    if outcome.is_some() {
                        Some(native)
                    } else {
                        *entry.native.lock().unwrap() = Some(native);
                        *outcome = Some(Ok(()));
                        None
                    }
                };
                entry.ready.notify_waiters();
                if let Some(native) = rejected {
                    native.undeclare().await?;
                    return Err(UStatus::fail_with_code(
                        UCode::Cancelled,
                        "listener removed during creation",
                    ));
                }
                creation.armed = false;
                entry.wait_ready().await
            }
            Err(error) => {
                // Every duplicate waiter observes the creator's actual error.
                // Remove only this pending generation before permitting a retry.
                let mut entries = self.subscribers.lock().unwrap();
                if entries
                    .get(&creation.key)
                    .is_some_and(|value| Arc::ptr_eq(value, &entry))
                {
                    entries.remove(&creation.key);
                }
                drop(entries);
                drop(entry.retire(error.clone()));
                creation.armed = false;
                Err(error)
            }
        }
    }

    pub async fn unregister(
        &self,
        key_expr: &str,
        listener: ComparableListener,
    ) -> Result<(), UStatus> {
        // Close the copied-value queue before undeclaring native delivery, waking
        // a blocked producer without retaining the registry lock across drain.
        // [impl->dsn~utransport-unregisterlistener-stop-invoking-listeners~1]
        let Some(entry) = self
            .subscribers
            .lock()
            .unwrap()
            .remove(&(key_expr.to_string(), listener.clone()))
        else {
            // [impl->dsn~utransport-unregisterlistener-error-notfound~1]
            return Err(UStatus::fail_with_code(
                UCode::NotFound,
                format!("No such listener registered for key expression: {key_expr}"),
            ));
        };
        if let Some(native) = entry.retire(UStatus::fail_with_code(
            UCode::Cancelled,
            "listener removed",
        )) {
            native.undeclare().await?;
        }
        entry.dispatch.stop().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::{
        future::{poll_fn, Future},
        task::Poll,
    };
    use up_rust::{MockUListener, UMessageBuilder, UUri};

    fn entry(
        registry: &ListenerRegistry,
        key: &str,
        listener: &Arc<dyn UListener>,
    ) -> Arc<SubscriberEntry> {
        Arc::clone(
            registry
                .subscribers
                .lock()
                .unwrap()
                .get(&(key.into(), ComparableListener::new(Arc::clone(listener))))
                .unwrap(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    // [utest->dsn~utransport-registerlistener-idempotent~1]
    async fn test_register_ignores_duplicate_registration() {
        let session = zenoh::open(zenoh::config::Config::default())
            .await
            .expect("Failed to open Zenoh session");
        let registry = ListenerRegistry::new(Arc::new(session), 10);
        let listener = Arc::new(MockUListener::new());

        assert!(registry
            .register_subscriber("key1".to_string(), listener.clone(), None)
            .await
            .is_ok());
        // Registering the same listener again should not fail
        assert!(registry
            .register_subscriber("key1".to_string(), listener.clone(), None)
            .await
            .is_ok());

        {
            let locked_subscribers = registry.subscribers.lock().unwrap();
            assert!(locked_subscribers
                .contains_key(&("key1".to_string(), ComparableListener::new(listener))));
        }
    }

    #[test_case::test_case(false; "cancel pending creator")]
    #[test_case::test_case(true; "remove pending creator")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn pending_rpc_setup_releases_duplicates_and_allows_a_new_generation(remove: bool) {
        let mut config = zenoh::Config::default();
        config.insert_json5("listen/endpoints", "[]").unwrap();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        let session = Arc::new(zenoh::open(config).await.unwrap());
        let registry = ListenerRegistry::new(Arc::clone(&session), 3);
        let listener: Arc<dyn UListener> = Arc::new(MockUListener::new());
        registry
            .register_subscriber("existing".into(), Arc::clone(&listener), None)
            .await
            .unwrap();
        let activity = Arc::clone(entry(&registry, "existing", &listener).dispatch.activity());
        let queries = PendingQueries::new().unwrap();
        activity
            .dispatch(|| async {
                // The actual publication guard excludes new native dispatch targets
                // until this synchronous publication's reservations have retired.
                let publication =
                    crate::dispatch::reserve_publication(&session, "existing").unwrap();
                let mut creating = Box::pin(registry.register_subscriber(
                    "pending".into(),
                    Arc::clone(&listener),
                    Some(Arc::clone(&queries)),
                ));
                assert!(poll_fn(|cx| Poll::Ready(creating.as_mut().poll(cx)))
                    .await
                    .is_pending());
                let mut duplicate = Box::pin(registry.register_subscriber(
                    "pending".into(),
                    Arc::clone(&listener),
                    Some(Arc::clone(&queries)),
                ));
                assert!(poll_fn(|cx| Poll::Ready(duplicate.as_mut().poll(cx)))
                    .await
                    .is_pending());
                if remove {
                    registry
                        .unregister("pending", ComparableListener::new(Arc::clone(&listener)))
                        .await
                        .unwrap();
                } else {
                    drop(creating);
                    assert_eq!(duplicate.await.unwrap_err().code(), UCode::Cancelled);
                    drop(publication);
                    registry
                        .register_subscriber(
                            "pending".into(),
                            Arc::clone(&listener),
                            Some(Arc::clone(&queries)),
                        )
                        .await
                        .unwrap();
                    return;
                }
                assert_eq!(duplicate.await.unwrap_err().code(), UCode::Cancelled);
                drop(publication);
                // Install a replacement before the old creator resumes. Its eventual
                // rollback must not delete the replacement's native handles.
                registry
                    .register_subscriber(
                        "pending".into(),
                        Arc::clone(&listener),
                        Some(Arc::clone(&queries)),
                    )
                    .await
                    .unwrap();
                assert_eq!(creating.await.unwrap_err().code(), UCode::Cancelled);
            })
            .await;
        assert_eq!(registry.subscribers.lock().unwrap().len(), 2);
        assert!(entry(&registry, "pending", &listener)
            .native
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .queryable
            .is_some());
        registry
            .unregister("pending", ComparableListener::new(Arc::clone(&listener)))
            .await
            .unwrap();
        registry
            .unregister("existing", ComparableListener::new(listener))
            .await
            .unwrap();
    }

    #[test]
    // [utest->dsn~utransport-registerlistener-discard-invalid-messages~1]
    fn test_attachment_to_uattributes_accepts_valid_attributes() {
        let attribs = UMessageBuilder::publish(
            UUri::try_from_parts("source", 0xAA1, 0x01, 0x9000).expect("failed to create source"),
        )
        .build()
        .expect("failed to create message")
        .attributes()
        .clone();
        let attachment = crate::UPTransportZenoh::uattributes_to_attachment(&attribs)
            .expect("failed to create attachment from UAttributes");
        assert_eq!(
            attachment_to_uattributes(&attachment).expect("failed to parse attachment"),
            attribs
        );
    }
}
