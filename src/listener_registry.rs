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

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
#[cfg(feature = "zero-copy")]
use std::{
    hash::{Hash, Hasher},
    ops::Deref,
};

use tokio::sync::Mutex;
use tracing::{debug, enabled, warn, Level};
use up_rust::{
    transport::ComparableOwnedListener, validate_owned_frame_for_transport, UCode, UOwnedFrame,
    UOwnedListener, UStatus,
};
#[cfg(feature = "zero-copy")]
use up_rust::{validate_frame_metadata_for_payload, zero_copy::UZeroCopyListener};
use zenoh::{pubsub::Subscriber, sample::Sample, Session};

#[cfg(feature = "zero-copy")]
use crate::zero_copy::{is_strict_shm_payload, ZenohRxFrame};

type OwnedSubscriberMap = Mutex<HashMap<(String, ComparableOwnedListener), RegisteredSubscriber>>;
#[cfg(feature = "zero-copy")]
type ZeroCopySubscriberMap =
    Mutex<HashMap<(String, ComparableZeroCopyListener), RegisteredSubscriber>>;

struct RegisteredSubscriber {
    subscriber: Subscriber<()>,
    active: Arc<AtomicBool>,
}

pub(crate) struct ListenerRegistry {
    owned_subscribers: OwnedSubscriberMap,
    #[cfg(feature = "zero-copy")]
    zero_copy_subscribers: ZeroCopySubscriberMap,
    session: Arc<Session>,
    max_subscribers: usize,
}

impl ListenerRegistry {
    pub fn new(zenoh_session: Arc<Session>, max_subscribers: usize) -> Self {
        Self {
            owned_subscribers: Mutex::new(HashMap::new()),
            #[cfg(feature = "zero-copy")]
            zero_copy_subscribers: Mutex::new(HashMap::new()),
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
        let active = Arc::new(AtomicBool::new(true));
        let active_in_callback = active.clone();
        let callback = move |sample: Sample| {
            if !active_in_callback.load(Ordering::Acquire) {
                return;
            }
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
            if header.encoding().is_none() && !sample.payload().is_empty() {
                warn!(
                    "Ignoring Zenoh Sample with payload bytes but no payload encoding [key expr: {}]",
                    sample.key_expr()
                );
                return;
            }
            let frame = if header.encoding().is_some() {
                UOwnedFrame::new(header, sample.payload().to_bytes().to_vec())
            } else {
                UOwnedFrame::without_payload(header)
            };
            let is_valid = validate_owned_frame_for_transport(&frame).is_ok();
            if is_valid {
                tokio::spawn(async move {
                    listener_cloned.on_receive_owned(frame).await;
                });
            } else if enabled!(Level::DEBUG) {
                let id = frame.metadata().attributes().id();
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
                locked_subscribers.insert(
                    (zenoh_key, comparable_listener),
                    RegisteredSubscriber { subscriber, active },
                );
                Ok(())
            }
            Err(e) => {
                let msg = "Failed to register owned listener";
                warn!("{msg}: {e}");
                Err(UStatus::fail_with_code(UCode::INTERNAL, msg))
            }
        }
    }

    #[cfg(feature = "zero-copy")]
    pub async fn register_zero_copy_subscriber(
        &self,
        zenoh_key: String,
        listener: Arc<dyn UZeroCopyListener<ZenohRxFrame>>,
    ) -> Result<(), UStatus> {
        let mut locked_subscribers = self.zero_copy_subscribers.lock().await;
        let comparable_listener = ComparableZeroCopyListener::new(listener);

        if locked_subscribers.contains_key(&(zenoh_key.clone(), comparable_listener.clone())) {
            debug!("Zero-copy listener already registered");
            return Ok(());
        }
        if locked_subscribers.len() >= self.max_subscribers {
            return Err(UStatus::fail_with_code(
                UCode::RESOURCE_EXHAUSTED,
                format!(
                    "Maximum number of zero-copy listeners reached: {}",
                    self.max_subscribers
                ),
            ));
        }

        let listener_to_invoke_in_callback = comparable_listener.clone();
        let active = Arc::new(AtomicBool::new(true));
        let active_in_callback = active.clone();
        let callback = move |sample: Sample| {
            if !active_in_callback.load(Ordering::Acquire) {
                return;
            }
            let listener_cloned = listener_to_invoke_in_callback.clone();
            let Some(attachment) = sample.attachment() else {
                warn!(
                    "Ignoring Zenoh Sample without attachment [key expr: {}]",
                    sample.key_expr()
                );
                return;
            };
            let metadata = match crate::utransport::attachment_to_frame_metadata(attachment) {
                Ok(metadata) => metadata,
                Err(e) => {
                    warn!("Unable to transform attachment to valid UFrameMetadata: {e:?}");
                    return;
                }
            };
            let has_payload = metadata.encoding().is_some();
            if !has_payload && !sample.payload().is_empty() {
                warn!(
                    "Ignoring Zenoh Sample with payload bytes but no payload encoding [key expr: {}]",
                    sample.key_expr()
                );
                return;
            }
            if !is_strict_shm_payload(&metadata, &sample) {
                warn!(
                    "Dropping non-SHM Zenoh payload on strict zero-copy listener path [key expr: {}]",
                    sample.key_expr()
                );
                return;
            }
            if validate_frame_metadata_for_payload(&metadata, has_payload).is_ok() {
                tokio::spawn(async move {
                    listener_cloned
                        .on_receive_zero_copy(ZenohRxFrame::new(metadata, sample))
                        .await;
                });
            } else if enabled!(Level::DEBUG) {
                let id = metadata.attributes().id();
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
                locked_subscribers.insert(
                    (zenoh_key, comparable_listener),
                    RegisteredSubscriber { subscriber, active },
                );
                Ok(())
            }
            Err(e) => {
                let msg = "Failed to register zero-copy listener";
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
        let registered_subscriber = self
            .owned_subscribers
            .lock()
            .await
            .remove(&(key_expr.to_string(), listener.clone()))
            .ok_or_else(|| {
                UStatus::fail_with_code(
                    UCode::NOT_FOUND,
                    format!("No such owned listener registered for key expression: {key_expr}"),
                )
            })?;
        registered_subscriber.active.store(false, Ordering::Release);
        registered_subscriber
            .subscriber
            .undeclare()
            .await
            .map_err(|e| {
                UStatus::fail_with_code(
                    UCode::INTERNAL,
                    format!("failed to undeclare Zenoh subscriber: {e}"),
                )
            })?;
        Ok(())
    }

    #[cfg(feature = "zero-copy")]
    pub async fn unregister_zero_copy(
        &self,
        key_expr: &str,
        listener: ComparableZeroCopyListener,
    ) -> Result<(), UStatus> {
        let registered_subscriber = self
            .zero_copy_subscribers
            .lock()
            .await
            .remove(&(key_expr.to_string(), listener.clone()))
            .ok_or_else(|| {
                UStatus::fail_with_code(
                    UCode::NOT_FOUND,
                    format!("No such zero-copy listener registered for key expression: {key_expr}"),
                )
            })?;
        registered_subscriber.active.store(false, Ordering::Release);
        registered_subscriber
            .subscriber
            .undeclare()
            .await
            .map_err(|e| {
                UStatus::fail_with_code(
                    UCode::INTERNAL,
                    format!("failed to undeclare Zenoh subscriber: {e}"),
                )
            })?;
        Ok(())
    }
}

#[cfg(feature = "zero-copy")]
#[derive(Clone)]
pub(crate) struct ComparableZeroCopyListener {
    listener: Arc<dyn UZeroCopyListener<ZenohRxFrame>>,
}

#[cfg(feature = "zero-copy")]
impl ComparableZeroCopyListener {
    pub fn new(listener: Arc<dyn UZeroCopyListener<ZenohRxFrame>>) -> Self {
        Self { listener }
    }

    fn pointer_address(&self) -> usize {
        let ptr = Arc::as_ptr(&self.listener);
        let thin_ptr = ptr.cast::<()>();
        thin_ptr as usize
    }
}

#[cfg(feature = "zero-copy")]
impl Deref for ComparableZeroCopyListener {
    type Target = dyn UZeroCopyListener<ZenohRxFrame>;

    fn deref(&self) -> &Self::Target {
        &*self.listener
    }
}

#[cfg(feature = "zero-copy")]
impl Hash for ComparableZeroCopyListener {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.listener).hash(state);
    }
}

#[cfg(feature = "zero-copy")]
impl PartialEq for ComparableZeroCopyListener {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.listener, &other.listener)
    }
}

#[cfg(feature = "zero-copy")]
impl Eq for ComparableZeroCopyListener {}

#[cfg(feature = "zero-copy")]
impl std::fmt::Debug for ComparableZeroCopyListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ComparableZeroCopyListener: {}", self.pointer_address())
    }
}
