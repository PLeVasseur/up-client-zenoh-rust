// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Allocating control adapter for the inherited attachment/lifetime corpus.
#![allow(dead_code)] // Different control targets use different method subsets.

use std::{
    marker::PhantomData,
    sync::{Arc, Mutex},
};
use up_rust::{
    FrameSink, ListenerAdmission, OverflowPolicy, Registration, RegistrationConfig,
    RegistrationHandler, RegistrationInbox, UCode, UFrameFieldsMetadataCodec, UFrameView, UStatus,
    UTxLoanSpec, UUri, UWire, UWireMetadataCodec, UWireMetadataContext, UWireRx,
};
use up_transport_zenoh::{ZenohRxFrame, ZenohTxBuffer, ZenohUninitTxBuffer, ZenohZeroCopyCore};

pub type Received<W> = UWireRx<ZenohRxFrame, W, UFrameFieldsMetadataCodec>;
#[async_trait::async_trait]
pub trait Listener<F>: Send + Sync {
    async fn on_frame(&self, frame: F);
}

pub trait WithControlWire {
    fn with_control_wire<W: UWire + Send + Sync + 'static>(self, wire: W) -> Transport<W>;
}
impl WithControlWire for ZenohZeroCopyCore {
    fn with_control_wire<W: UWire + Send + Sync + 'static>(self, _wire: W) -> Transport<W> {
        Transport {
            core: Arc::new(self),
            entries: Mutex::new(Vec::new()),
            workers: Mutex::new(Vec::new()),
            wire: PhantomData,
        }
    }
}

struct Entry<W: UWire + Send + Sync + 'static> {
    source: UUri,
    sink: Option<UUri>,
    listener: Arc<dyn Listener<Received<W>>>,
    raw: Arc<dyn FrameSink<ZenohRxFrame>>,
    active: Arc<ListenerAdmission>,
}
struct Handler<W: UWire + Send + Sync + 'static> {
    source: UUri,
    sink: Option<UUri>,
    listener: Arc<dyn Listener<Received<W>>>,
    active: Arc<ListenerAdmission>,
}
impl<W: UWire + Send + Sync + 'static> RegistrationHandler<ZenohRxFrame> for Handler<W> {
    async fn on_receive(&mut self, raw: ZenohRxFrame) {
        if let Ok(frame) = Received::try_from_encoded(raw, &UFrameFieldsMetadataCodec) {
            if matches(&frame, &self.source, self.sink.as_ref()) {
                self.active.dispatch(|| self.listener.on_frame(frame)).await;
            }
        }
    }
}
struct Sink {
    inbox: Arc<RegistrationInbox<ZenohRxFrame>>,
    active: Arc<ListenerAdmission>,
}
impl FrameSink<ZenohRxFrame> for Sink {
    fn push(&self, frame: ZenohRxFrame) {
        if self.active.closed_at().is_none() {
            let _ = self.inbox.push(frame);
        }
    }
    fn close(&self) {
        self.active.cancel();
        self.inbox.close();
    }
}
fn matches<W: UWire>(frame: &Received<W>, source: &UUri, sink: Option<&UUri>) -> bool {
    source.matches(frame.metadata().source())
        && match (sink, frame.metadata().sink()) {
            (None, None) => true,
            (Some(filter), Some(actual)) => filter.matches(actual),
            _ => false,
        }
}

pub struct Transport<W: UWire + Send + Sync + 'static> {
    core: Arc<ZenohZeroCopyCore>,
    entries: Mutex<Vec<Entry<W>>>,
    // Closed workers are retained until control drop, so a quiescence timeout
    // does not abort an entered callback or revoke its native sample.
    workers: Mutex<Vec<Registration<ZenohRxFrame>>>,
    wire: PhantomData<W>,
}
impl<W: UWire + Send + Sync + 'static> Transport<W> {
    pub async fn loan_validated_tx(&self, spec: UTxLoanSpec) -> Result<ZenohTxBuffer, UStatus> {
        let bytes = UFrameFieldsMetadataCodec.encode_frame_metadata(
            UWireMetadataContext::from_wire::<W, UFrameFieldsMetadataCodec>(),
            spec.metadata(),
        )?;
        std::future::ready(self.core.loan_encoded(
            spec.metadata().clone(),
            &bytes,
            spec.payload_len(),
            spec.payload_alignment_proof().as_usize(),
        ))
        .await
    }
    pub async fn loan_validated_uninit_tx(
        &self,
        spec: UTxLoanSpec,
    ) -> Result<ZenohUninitTxBuffer, UStatus> {
        let bytes = UFrameFieldsMetadataCodec.encode_frame_metadata(
            UWireMetadataContext::from_wire::<W, UFrameFieldsMetadataCodec>(),
            spec.metadata(),
        )?;
        std::future::ready(self.core.loan_encoded_uninit(
            spec.metadata().clone(),
            &bytes,
            spec.payload_len(),
            spec.payload_alignment_proof().as_usize(),
        ))
        .await
    }
    pub async fn send_validated_zero_copy(&self, loan: ZenohTxBuffer) -> Result<(), UStatus> {
        self.core.publish_encoded(loan).await
    }
    pub async fn receive_validated_zero_copy(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
    ) -> Result<Received<W>, UStatus> {
        let frame = Received::try_from_encoded(
            self.core.receive_encoded(source, sink).await?,
            &UFrameFieldsMetadataCodec,
        )?;
        if !matches(&frame, source, sink) {
            return Err(UStatus::fail_with_code(
                UCode::NotFound,
                "decoded control filter mismatch",
            ));
        }
        Ok(frame)
    }
    pub async fn register_validated_zero_copy_listener(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
        listener: Arc<dyn Listener<Received<W>>>,
    ) -> Result<(), UStatus> {
        let active = Arc::new(ListenerAdmission::new());
        let worker = Registration::new(
            RegistrationConfig {
                capacity: 1,
                overflow: OverflowPolicy::Block {
                    deadline: std::time::Duration::from_secs(5),
                },
            },
            Handler::<W> {
                source: source.clone(),
                sink: sink.cloned(),
                listener: Arc::clone(&listener),
                active: Arc::clone(&active),
            },
            || {},
        )
        .map_err(|error| UStatus::fail_with_code(UCode::ResourceExhausted, error.to_string()))?;
        let raw: Arc<dyn FrameSink<ZenohRxFrame>> = Arc::new(Sink {
            inbox: worker.inbox(),
            active: Arc::clone(&active),
        });
        self.core
            .attach_encoded(source, sink, Arc::clone(&raw))
            .await?;
        self.workers.lock().unwrap().push(worker);
        self.entries.lock().unwrap().push(Entry {
            source: source.clone(),
            sink: sink.cloned(),
            listener,
            raw,
            active,
        });
        Ok(())
    }
    pub async fn unregister_validated_zero_copy_listener(
        &self,
        source: &UUri,
        sink: Option<&UUri>,
        listener: Arc<dyn Listener<Received<W>>>,
    ) -> Result<(), UStatus> {
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            let index = entries
                .iter()
                .position(|entry| {
                    &entry.source == source
                        && entry.sink.as_ref() == sink
                        && Arc::ptr_eq(&entry.listener, &listener)
                })
                .ok_or_else(|| {
                    UStatus::fail_with_code(UCode::NotFound, "control listener not found")
                })?;
            entries.remove(index)
        };
        entry.active.cancel();
        self.core.detach_encoded(source, sink, entry.raw).await?;
        if !entry.active.is_current_callback() {
            let deadline = entry
                .active
                .closed_at()
                .unwrap()
                .checked_add(self.core.listener_quiesce_timeout())
                .unwrap();
            entry.active.quiesce(deadline).await.map_err(|error| {
                UStatus::fail_with_code(
                    UCode::DeadlineExceeded,
                    format!("control quiescence: {error:?}"),
                )
            })?;
        }
        Ok(())
    }
}
