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

use tokio::sync::Mutex;
use tracing::{debug, enabled, warn, Level};
use up_rust::{ComparableOwnedListener, UCode, UOwnedFrame, UOwnedListener, UStatus};
use zenoh::{pubsub::Subscriber, sample::Sample, Session};

type OwnedSubscriberMap = Mutex<HashMap<(String, ComparableOwnedListener), Subscriber<()>>>;

pub(crate) struct ListenerRegistry {
    owned_subscribers: OwnedSubscriberMap,
    session: Arc<Session>,
    max_subscribers: usize,
}

impl ListenerRegistry {
    pub fn new(zenoh_session: Arc<Session>, max_subscribers: usize) -> Self {
        Self {
            owned_subscribers: Mutex::new(HashMap::new()),
            session: zenoh_session,
            max_subscribers,
        }
    }

    pub async fn register_owned_subscriber(
        &self,
        zenoh_key: String,
        listener: Arc<dyn UOwnedListener>,
    ) -> Result<(), UStatus> {
        let mut locked_subscribers = self.owned_subscribers.lock().await;
        let comparable_listener = ComparableOwnedListener::new(listener);

        if locked_subscribers.contains_key(&(zenoh_key.clone(), comparable_listener.clone())) {
            debug!("Owned listener already registered");
            return Ok(());
        }
        if locked_subscribers.len() >= self.max_subscribers {
            return Err(UStatus::fail_with_code(
                UCode::RESOURCE_EXHAUSTED,
                format!(
                    "Maximum number of owned listeners reached: {}",
                    self.max_subscribers
                ),
            ));
        }

        let listener_to_invoke_in_callback = comparable_listener.clone();
        let callback = move |sample: Sample| {
            let listener_cloned = listener_to_invoke_in_callback.clone();
            let Some(attachment) = sample.attachment() else {
                warn!(
                    "Ignoring Zenoh Sample without attachment [key expr: {}]",
                    sample.key_expr()
                );
                return;
            };
            let header = match crate::utransport::attachment_to_frame_metadata(attachment) {
                Ok(header) => header,
                Err(e) => {
                    warn!("Unable to transform attachment to valid UFrameMetadata: {e:?}");
                    return;
                }
            };
            if !header.attributes().is_expired() {
                let frame = UOwnedFrame::new(header, sample.payload().to_bytes().to_vec());
                tokio::spawn(async move {
                    listener_cloned.on_receive_owned(frame).await;
                });
            } else if enabled!(Level::DEBUG) {
                let id = header.attributes().id();
                debug!(
                    "discarding expired frame [id: {}]",
                    id.to_hyphenated_string(),
                );
            }
        };

        match self
            .session
            .declare_subscriber(&zenoh_key)
            .callback_mut(callback)
            .await
        {
            Ok(subscriber) => {
                locked_subscribers.insert((zenoh_key, comparable_listener), subscriber);
                Ok(())
            }
            Err(e) => {
                let msg = "Failed to register owned listener";
                warn!("{msg}: {e}");
                Err(UStatus::fail_with_code(UCode::INTERNAL, msg))
            }
        }
    }

    pub async fn unregister_owned(
        &self,
        key_expr: &str,
        listener: ComparableOwnedListener,
    ) -> Result<(), UStatus> {
        if self
            .owned_subscribers
            .lock()
            .await
            .remove(&(key_expr.to_string(), listener.clone()))
            .is_none()
        {
            return Err(UStatus::fail_with_code(
                UCode::NOT_FOUND,
                format!("No such owned listener registered for key expression: {key_expr}"),
            ));
        }
        Ok(())
    }
}
