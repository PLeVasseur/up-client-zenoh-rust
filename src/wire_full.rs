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
use up_rust::{
    EncodedOwnedFrame, PreparedOwnedFrame, UCode, UEncodedOwnedListener, UOwnedTransportCore,
    UStatus, UUri, UWire, UWireTransport, UWithWire,
};
use zenoh::{bytes::ZBytes, sample::Sample};

use crate::mechanics::ZenohWireMechanics;

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
    /// Creates a real Zenoh owned-frame core.
    ///
    /// # Errors
    ///
    /// Returns a status when the Zenoh session or local URI validation fails.
    pub async fn new(
        config: crate::zenoh_config::Config,
        uri: impl Into<String>,
    ) -> Result<Self, UStatus> {
        Ok(Self {
            mechanics: ZenohWireMechanics::new(config, uri).await?,
            listeners: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Wraps this core in the generic selected-wire adapter.
    #[must_use]
    pub fn with_selected_wire<W>(self, wire: W) -> UWireTransport<Self, W>
    where
        W: UWire,
    {
        self.with_wire(wire)
    }
}

struct OwnedListenerRegistration {
    source_filter: UUri,
    sink_filter: Option<UUri>,
    owned_listener: Arc<dyn UEncodedOwnedListener>,
    zenoh_listener: ComparableOwnedListener,
    subscriber: zenoh::pubsub::Subscriber<()>,
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
            .to_zenoh_key_string(metadata.attributes().source(), metadata.attributes().sink());
        let priority = crate::mechanics::map_zenoh_priority(
            metadata
                .attributes()
                .priority()
                .unwrap_or(up_rust::UPriority::CS1),
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
        let subscriber = self
            .mechanics
            .session()
            .declare_subscriber(&zenoh_key)
            .callback_mut(move |sample: Sample| {
                let Some(frame) = sample_to_encoded_owned(&sample) else {
                    return;
                };
                let listener = callback_listener.clone();
                tokio::spawn(async move {
                    listener.on_receive_encoded_owned(frame).await;
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
    let payload = if sample.payload().is_empty() {
        None
    } else {
        Some(Bytes::copy_from_slice(sample.payload().to_bytes().as_ref()))
    };
    Some(EncodedOwnedFrame::new(
        attachment.to_bytes().to_vec(),
        payload,
    ))
}
