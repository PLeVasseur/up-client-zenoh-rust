use std::{collections::VecDeque, io::Cursor, sync::Arc};

use async_trait::async_trait;
use tokio::sync::Mutex;
use up_rust_userializer::{
    PreparedTxLoanSpec, UCode, UEncodedRxFrame, UEncodedZeroCopyListener, UFrameMetadata, UStatus,
    UTxBuffer, UUri, UVecTxBuffer, UWire, UWireTransport, UWithWire, UZeroCopyTransportCore,
};

/// Minimal Zenoh attachment proof core that consumes prepared metadata bytes.
#[derive(Clone, Default)]
pub struct ZenohWireCore {
    state: Arc<Mutex<ZenohWireState>>,
}

#[derive(Default)]
struct ZenohWireState {
    prepared: Vec<PreparedTxLoanSpec>,
    received: VecDeque<ZenohEncodedRxFrame>,
    listeners: Vec<Arc<dyn UEncodedZeroCopyListener<ZenohEncodedRxFrame>>>,
}

impl ZenohWireCore {
    /// Creates an empty smoke core.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps this core in the generic wire adapter.
    #[must_use]
    pub fn with_selected_wire<W>(self, wire: W) -> UWireTransport<Self, W>
    where
        W: UWire,
    {
        self.with_wire(wire)
    }

    /// Returns the last prepared request observed by the core.
    pub async fn last_prepared(&self) -> Option<PreparedTxLoanSpec> {
        self.state.lock().await.prepared.last().cloned()
    }

    /// Injects one encoded receive frame for pull receive tests.
    pub async fn push_encoded_rx(&self, frame: ZenohEncodedRxFrame) {
        self.state.lock().await.received.push_back(frame);
    }

    /// Delivers one encoded receive frame to registered raw listeners.
    pub async fn deliver_encoded_rx(&self, frame: ZenohEncodedRxFrame) {
        let listeners = self.state.lock().await.listeners.clone();
        for listener in listeners {
            listener.on_receive_encoded_zero_copy(frame.clone()).await;
        }
    }
}

/// Prepared metadata bytes carried in the Zenoh user attachment slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZenohPreparedAttachment {
    encoded_metadata: Vec<u8>,
}

impl ZenohPreparedAttachment {
    fn from_prepared(spec: &PreparedTxLoanSpec) -> Self {
        Self {
            encoded_metadata: spec.encoded_metadata().to_vec(),
        }
    }

    /// Creates an attachment from already prepared metadata bytes.
    #[must_use]
    pub fn from_encoded_metadata(encoded_metadata: Vec<u8>) -> Self {
        Self { encoded_metadata }
    }

    /// Returns the attachment bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.encoded_metadata
    }

    /// Returns the attachment length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.encoded_metadata.len()
    }

    /// Returns true when the attachment is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.encoded_metadata.is_empty()
    }
}

/// Transmit buffer returned by the Zenoh smoke core.
pub struct ZenohTxBuffer {
    attachment: ZenohPreparedAttachment,
    buffer: UVecTxBuffer,
}

impl ZenohTxBuffer {
    /// Returns the prepared attachment bytes.
    #[must_use]
    pub fn attachment(&self) -> &ZenohPreparedAttachment {
        &self.attachment
    }

    fn into_encoded_rx(self) -> ZenohEncodedRxFrame {
        ZenohEncodedRxFrame {
            attachment: self.attachment,
            payload: self.buffer.payload().to_vec(),
        }
    }
}

impl UTxBuffer for ZenohTxBuffer {
    fn metadata(&self) -> &UFrameMetadata {
        self.buffer.metadata()
    }

    fn payload(&self) -> &[u8] {
        self.buffer.payload()
    }

    fn payload_mut(&mut self) -> &mut [u8] {
        self.buffer.payload_mut()
    }
}

/// Raw encoded receive frame returned by the Zenoh smoke core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZenohEncodedRxFrame {
    attachment: ZenohPreparedAttachment,
    payload: Vec<u8>,
}

impl ZenohEncodedRxFrame {
    /// Creates a raw encoded receive frame from attachment bytes and payload.
    #[must_use]
    pub fn new(attachment: ZenohPreparedAttachment, payload: Vec<u8>) -> Self {
        Self {
            attachment,
            payload,
        }
    }

    /// Returns the prepared attachment bytes.
    #[must_use]
    pub fn attachment(&self) -> &ZenohPreparedAttachment {
        &self.attachment
    }
}

impl UEncodedRxFrame for ZenohEncodedRxFrame {
    type PayloadReader<'a>
        = Cursor<&'a [u8]>
    where
        Self: 'a;
    type PayloadSlices<'a>
        = std::iter::Once<&'a [u8]>
    where
        Self: 'a;

    fn encoded_metadata(&self) -> &[u8] {
        self.attachment.as_bytes()
    }

    fn payload_len(&self) -> usize {
        self.payload.len()
    }

    fn payload_reader(&self) -> Self::PayloadReader<'_> {
        Cursor::new(self.payload.as_slice())
    }

    fn payload_slices(&self) -> Self::PayloadSlices<'_> {
        std::iter::once(self.payload.as_slice())
    }

    fn try_contiguous_payload(&self) -> Option<&[u8]> {
        Some(&self.payload)
    }
}

#[async_trait]
impl UZeroCopyTransportCore for ZenohWireCore {
    type Tx = ZenohTxBuffer;
    type Rx = ZenohEncodedRxFrame;

    async fn loan_prepared_tx(&self, spec: PreparedTxLoanSpec) -> Result<Self::Tx, UStatus> {
        let attachment = ZenohPreparedAttachment::from_prepared(&spec);
        let buffer = UVecTxBuffer::with_alignment(
            spec.metadata().clone(),
            spec.payload_len(),
            spec.payload_alignment(),
        )?;
        self.state.lock().await.prepared.push(spec);
        Ok(ZenohTxBuffer { attachment, buffer })
    }

    async fn send_prepared_zero_copy(&self, buffer: Self::Tx) -> Result<(), UStatus> {
        self.state
            .lock()
            .await
            .received
            .push_back(buffer.into_encoded_rx());
        Ok(())
    }

    async fn receive_encoded_zero_copy(
        &self,
        _source_filter: &UUri,
        _sink_filter: Option<&UUri>,
    ) -> Result<Self::Rx, UStatus> {
        self.state
            .lock()
            .await
            .received
            .pop_front()
            .ok_or_else(|| UStatus::fail_with_code(UCode::NotFound, "no frame available"))
    }

    async fn register_encoded_zero_copy_listener(
        &self,
        _source_filter: &UUri,
        _sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedZeroCopyListener<Self::Rx>>,
    ) -> Result<(), UStatus> {
        self.state.lock().await.listeners.push(listener);
        Ok(())
    }

    async fn unregister_encoded_zero_copy_listener(
        &self,
        _source_filter: &UUri,
        _sink_filter: Option<&UUri>,
        listener: Arc<dyn UEncodedZeroCopyListener<Self::Rx>>,
    ) -> Result<(), UStatus> {
        let mut state = self.state.lock().await;
        let Some(index) = state
            .listeners
            .iter()
            .position(|registered| Arc::ptr_eq(registered, &listener))
        else {
            return Err(UStatus::fail_with_code(
                UCode::NotFound,
                "listener not registered",
            ));
        };
        state.listeners.remove(index);
        Ok(())
    }
}
