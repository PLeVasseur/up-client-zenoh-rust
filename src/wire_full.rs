/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
 *
 * SPDX-License-Identifier: Apache-2.0
 ********************************************************************************/

use std::{ops::Deref, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;
use tracing::{trace, warn};
use up_rust::UWire;
use up_rust::{EncodedOwnedFrame, PreparedOwnedFrame, UEncodedOwnedListener, UOwnedTransportCore};
use up_rust::{UCode, UStatus, UUri};
use up_rust::{UNativePrefixWireTransport, UWithNativePrefixWire};
use zenoh::{bytes::ZBytes, sample::Sample};

use crate::mechanics::ZenohWireMechanics;
use up_rust::ListenerAdmission;

/// Real Zenoh owned-frame selected-wire core for benchmark/support paths.
///
/// This feature-gated core carries already encoded selected-wire metadata as
/// Zenoh attachment bytes and transports owned payload bytes through real Zenoh
/// publish/subscribe mechanics. Product [`crate::UPTransportZenoh`] remains the
/// ordinary `UTransport` implementation and is not wrapped by this core.
#[derive(Clone)]
pub struct ZenohOwnedCore {
    mechanics: ZenohWireMechanics,
    listeners: Arc<Mutex<Vec<OwnedListenerRegistration>>>,
}

impl ZenohOwnedCore {
    pub(crate) fn from_mechanics(mechanics: ZenohWireMechanics) -> Self {
        Self {
            mechanics,
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }
    /// Creates a real Zenoh owned-frame core.
    ///
    /// # Errors
    ///
    /// Returns a status when the Zenoh session or local URI validation fails.
    pub async fn new(
        config: crate::zenoh_config::Config,
        uri: impl Into<String>,
    ) -> Result<Self, UStatus> {
        Ok(Self::from_mechanics(
            ZenohWireMechanics::new(config, uri).await?,
        ))
    }

    /// Wraps this core in the generic selected-wire adapter.
    #[must_use]
    pub fn with_selected_wire<W>(self, wire: W) -> UNativePrefixWireTransport<Self, W>
    where
        W: UWire,
    {
        self.into_native_prefix_wire_transport(wire)
    }
}

struct OwnedListenerRegistration {
    source_filter: UUri,
    sink_filter: Option<UUri>,
    owned_listener: Arc<dyn UEncodedOwnedListener>,
    zenoh_listener: ComparableOwnedListener,
    subscriber: zenoh::pubsub::Subscriber<()>,
    activity: Arc<ListenerAdmission>,
}

#[derive(Clone)]
struct ComparableOwnedListener {
    listener: Arc<dyn UEncodedOwnedListener>,
}

impl ComparableOwnedListener {
    fn new(listener: Arc<dyn UEncodedOwnedListener>) -> Self {
        Self { listener }
    }
}

impl Deref for ComparableOwnedListener {
    type Target = dyn UEncodedOwnedListener;

    fn deref(&self) -> &Self::Target {
        &*self.listener
    }
}

#[async_trait]
impl UOwnedTransportCore for ZenohOwnedCore {
    async fn send_prepared_owned(&self, frame: PreparedOwnedFrame) -> Result<(), UStatus> {
        let metadata = frame.metadata();
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(metadata.source(), metadata.sink());
        let priority = crate::mechanics::map_zenoh_priority(
            metadata.priority().unwrap_or(up_rust::UPriority::CS1),
        );
        let attachment = ZBytes::from(frame.encoded_metadata().to_vec());
        let payload = frame
            .payload()
            .map_or_else(ZBytes::new, |payload| ZBytes::from(payload.to_vec()));

        self.mechanics
            .session()
            .put(&zenoh_key, payload)
            .priority(priority)
            .attachment(attachment)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to put Zenoh owned frame: {err}"),
                )
            })?;
        trace!("putting owned frame with key: {zenoh_key}");
        Ok(())
    }

    async fn receive_encoded_owned(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
    ) -> Result<EncodedOwnedFrame, UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let subscriber = self
            .mechanics
            .session()
            .declare_subscriber(&zenoh_key)
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to declare Zenoh owned subscriber: {err}"),
                )
            })?;

        loop {
            let sample = subscriber.recv_async().await.map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to receive Zenoh owned sample: {err}"),
                )
            })?;
            if let Some(frame) = sample_to_encoded_owned(&sample) {
                return Ok(frame);
            }
        }
    }

    async fn register_encoded_owned_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedOwnedListener>,
    ) -> Result<(), UStatus> {
        let zenoh_key = self
            .mechanics
            .to_zenoh_key_string(source_filter, sink_filter);
        let comparable = ComparableOwnedListener::new(listener.clone());
        let callback_listener = comparable.clone();
        let activity = Arc::new(ListenerAdmission::new());
        let callback_activity = Arc::clone(&activity);
        let subscriber = self
            .mechanics
            .session()
            .declare_subscriber(&zenoh_key)
            .callback_mut(move |sample: Sample| {
                let Some(frame) = sample_to_encoded_owned(&sample) else {
                    return;
                };
                let listener = callback_listener.clone();
                let activity = Arc::clone(&callback_activity);
                tokio::spawn(async move {
                    activity
                        .dispatch(|| listener.on_receive_encoded_owned(frame))
                        .await;
                });
            })
            .await
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to register Zenoh owned listener: {err}"),
                )
            })?;

        self.listeners.lock().await.push(OwnedListenerRegistration {
            source_filter: source_filter.clone(),
            sink_filter: sink_filter.cloned(),
            owned_listener: listener,
            zenoh_listener: comparable,
            subscriber,
            activity,
        });
        Ok(())
    }

    async fn unregister_encoded_owned_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedOwnedListener>,
    ) -> Result<(), UStatus> {
        let registration = {
            let mut listeners = self.listeners.lock().await;
            let Some(index) = listeners.iter().position(|registration| {
                registration.source_filter == *source_filter
                    && registration.sink_filter.as_ref() == sink_filter
                    && Arc::ptr_eq(&registration.owned_listener, &listener)
            }) else {
                return Err(UStatus::fail_with_code(
                    UCode::NotFound,
                    "owned listener not registered",
                ));
            };
            listeners.remove(index)
        };
        registration.activity.stop().await;
        drop(registration.zenoh_listener);
        registration.subscriber.undeclare().await.map_err(|err| {
            UStatus::fail_with_code(
                UCode::Internal,
                format!("failed to undeclare Zenoh owned listener: {err}"),
            )
        })
    }
}

fn sample_to_encoded_owned(sample: &Sample) -> Option<EncodedOwnedFrame> {
    let Some(attachment) = sample.attachment() else {
        warn!(
            "Ignoring Zenoh owned sample without selected-wire attachment [key expr: {}]",
            sample.key_expr()
        );
        return None;
    };
    let payload = if payload_present(attachment.to_bytes().as_ref())? {
        Some(Bytes::copy_from_slice(sample.payload().to_bytes().as_ref()))
    } else {
        if !sample.payload().is_empty() {
            return None;
        }
        None
    };
    Some(EncodedOwnedFrame::new(
        attachment.to_bytes().to_vec(),
        payload,
    ))
}

// Locate the canonical field block to preserve absent versus present-empty
// carriage. This is not wire/family compatibility or typed native admission;
// the SDK selected-wire adapter independently validates the complete prefix.
fn payload_present(bytes: &[u8]) -> Option<bool> {
    fn identity_end(bytes: &[u8], start: usize) -> Option<usize> {
        let end = match *bytes.get(start)? {
            0 => start.checked_add(3)?,
            1 => {
                let word_end = start.checked_add(5)?;
                let len =
                    u32::from_le_bytes(bytes.get(start + 1..word_end)?.try_into().ok()?) as usize;
                word_end.checked_add(len)?
            }
            _ => return None,
        };
        bytes.get(..end)?;
        Some(end)
    }
    if bytes.get(..4)? != b"UPWM" {
        return None;
    }
    let mut end = identity_end(bytes, 4)?.checked_add(2)?;
    end = identity_end(bytes, end)?;
    end = identity_end(bytes, end)?.checked_add(2)?;
    let word_end = end.checked_add(4)?;
    let len = u32::from_le_bytes(bytes.get(end..word_end)?.try_into().ok()?) as usize;
    let field_end = word_end.checked_add(len)?;
    if field_end != bytes.len() {
        return None;
    }
    Some(
        up_rust::frame::codec::decode_frame_metadata_fields(bytes.get(word_end..field_end)?)
            .ok()?
            .payload_encoding()
            .is_some(),
    )
}
