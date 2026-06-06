/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
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

use std::{mem::MaybeUninit, num::NonZeroUsize};

use async_trait::async_trait;
use tracing::trace;
#[cfg(test)]
use up_rust_zc::UPayloadFormat;
use up_rust_zc::{
    PayloadEncoding, ProtobufMappable, UCode, UFrameMetadata, UPriority, UStatus, UTxBuffer,
    UUninitTxBuffer, UVecRxLease, UZeroCopyTransportImpl, UZeroCopyUninitTransportImpl,
    ValidatedTxLoanSpec,
};
use zenoh::{
    bytes::ZBytes,
    shm::{GarbageCollect, MemoryLayout, OwnedShmBuf, ZShmMut},
    Wait as _,
};

use crate::UPTransportZenoh;

const FRAME_ATTACHMENT_MAGIC: &[u8; 4] = b"UFRM";

/// Zenoh shared-memory transmit loan used by the `zero-copy` feature.
pub struct ZenohTxBuffer {
    metadata: UFrameMetadata,
    zenoh_key: String,
    attachment: ZBytes,
    priority: zenoh::qos::Priority,
    payload: ZenohTxPayload,
}

/// Zenoh shared-memory transmit loan with uninitialized application payload bytes.
pub struct ZenohUninitTxBuffer {
    metadata: UFrameMetadata,
    zenoh_key: String,
    attachment: ZBytes,
    priority: zenoh::qos::Priority,
    payload: ZenohTxPayload,
}

enum ZenohTxPayload {
    Empty,
    Shm(ZShmMut),
}

impl ZenohTxPayload {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Shm(payload) => payload.as_ref(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Empty => &mut [],
            Self::Shm(payload) => payload.as_mut(),
        }
    }

    fn as_uninit_mut_slice(&mut self) -> &mut [MaybeUninit<u8>] {
        match self {
            Self::Empty => &mut [],
            Self::Shm(payload) => {
                let payload = payload.as_mut();
                // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`, and
                // the original mutable slice is exclusively borrowed here.
                unsafe {
                    std::slice::from_raw_parts_mut(
                        payload.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                        payload.len(),
                    )
                }
            }
        }
    }

    fn into_zbytes(self) -> ZBytes {
        match self {
            Self::Empty => ZBytes::new(),
            Self::Shm(payload) => ZBytes::from(payload),
        }
    }
}

impl UTxBuffer for ZenohTxBuffer {
    fn metadata(&self) -> &UFrameMetadata {
        &self.metadata
    }

    fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    fn payload_mut(&mut self) -> &mut [u8] {
        self.payload.as_mut_slice()
    }
}

impl UUninitTxBuffer for ZenohUninitTxBuffer {
    type Initialized = ZenohTxBuffer;

    fn metadata(&self) -> &UFrameMetadata {
        &self.metadata
    }

    fn payload_len(&self) -> usize {
        self.payload.as_slice().len()
    }

    fn payload_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.payload.as_uninit_mut_slice()
    }

    unsafe fn assume_payload_init(self) -> Self::Initialized {
        ZenohTxBuffer {
            metadata: self.metadata,
            zenoh_key: self.zenoh_key,
            attachment: self.attachment,
            priority: self.priority,
            payload: self.payload,
        }
    }
}

#[async_trait]
impl UZeroCopyTransportImpl for UPTransportZenoh {
    type Tx = ZenohTxBuffer;
    type Rx = UVecRxLease;

    async fn loan_validated_tx(&self, spec: ValidatedTxLoanSpec) -> Result<Self::Tx, UStatus> {
        let metadata = spec.metadata().clone();
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(
            self,
            &metadata,
            spec.payload_len(),
            spec.payload_alignment(),
        )?;
        Ok(ZenohTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }

    async fn send_validated_zero_copy(&self, buffer: Self::Tx) -> Result<(), UStatus> {
        let payload = if buffer.metadata.payload_encoding().is_some() {
            buffer.payload.into_zbytes()
        } else {
            ZBytes::new()
        };

        self.session
            .put(&buffer.zenoh_key, payload)
            .priority(buffer.priority)
            .attachment(buffer.attachment)
            .await
            .inspect(|()| trace!("putting zero-copy frame with key: {}", buffer.zenoh_key))
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to put Zenoh frame: {err}"),
                )
            })?;
        Ok(())
    }
}

#[async_trait]
impl UZeroCopyUninitTransportImpl for UPTransportZenoh {
    type UninitTx = ZenohUninitTxBuffer;

    async fn loan_validated_uninit_tx(
        &self,
        spec: ValidatedTxLoanSpec,
    ) -> Result<Self::UninitTx, UStatus> {
        let metadata = spec.metadata().clone();
        let (zenoh_key, attachment, priority, payload) = reserve_tx_parts(
            self,
            &metadata,
            spec.payload_len(),
            spec.payload_alignment(),
        )?;
        Ok(ZenohUninitTxBuffer {
            metadata,
            zenoh_key,
            attachment,
            priority,
            payload,
        })
    }
}

fn reserve_tx_parts(
    transport: &UPTransportZenoh,
    metadata: &UFrameMetadata,
    payload_len: usize,
    alignment: usize,
) -> Result<(String, ZBytes, zenoh::qos::Priority, ZenohTxPayload), UStatus> {
    validate_alignment(alignment)?;
    if metadata.payload_encoding().is_none() && payload_len != 0 {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "message payload is present but payload encoding is absent",
        ));
    }

    let zenoh_key = to_zenoh_key_string(
        metadata.attributes().source(),
        metadata.attributes().sink(),
        transport.local_authority.as_str(),
    );
    let attachment = frame_to_attachment(metadata)?;
    let priority = map_zenoh_priority(metadata.attributes().priority().unwrap_or(UPriority::CS1));
    let payload = reserve_payload(transport, payload_len, alignment)?;
    Ok((zenoh_key, attachment, priority, payload))
}

fn reserve_payload(
    transport: &UPTransportZenoh,
    payload_len: usize,
    alignment: usize,
) -> Result<ZenohTxPayload, UStatus> {
    if payload_len == 0 {
        return Ok(ZenohTxPayload::Empty);
    }

    let provider = transport.shm_provider()?;
    let aligned_len = align_len(payload_len, alignment)?;
    let layout = MemoryLayout::try_from(aligned_len).map_err(|err| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!("invalid Zenoh SHM allocation layout: {err}"),
        )
    })?;
    let mut payload = provider
        .alloc(layout)
        .with_policy::<GarbageCollect>()
        .wait()
        .map_err(|err| {
            UStatus::fail_with_code(
                UCode::ResourceExhausted,
                format!("failed to allocate Zenoh SHM payload buffer: {err}"),
            )
        })?;

    if aligned_len != payload_len {
        let Some(payload_len) = NonZeroUsize::new(payload_len) else {
            unreachable!("zero-length payloads are handled before SHM allocation")
        };
        payload.try_resize(payload_len).ok_or_else(|| {
            UStatus::fail_with_code(
                UCode::Internal,
                "failed to resize Zenoh SHM payload buffer after aligned allocation",
            )
        })?;
    }

    let address = payload.as_ref().as_ptr() as usize;
    if address % alignment != 0 {
        return Err(UStatus::fail_with_code(
            UCode::Internal,
            format!(
                "Zenoh SHM payload address 0x{address:x} does not satisfy requested alignment {alignment}"
            ),
        ));
    }

    Ok(ZenohTxPayload::Shm(payload))
}

fn validate_alignment(alignment: usize) -> Result<(), UStatus> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "payload alignment must be a non-zero power of two",
        ));
    }
    Ok(())
}

fn align_len(payload_len: usize, alignment: usize) -> Result<usize, UStatus> {
    let remainder = payload_len % alignment;
    if remainder == 0 {
        return Ok(payload_len);
    }
    payload_len
        .checked_add(alignment - remainder)
        .ok_or_else(|| UStatus::fail_with_code(UCode::InvalidArgument, "payload length overflow"))
}

fn map_zenoh_priority(priority: UPriority) -> zenoh::qos::Priority {
    match priority {
        UPriority::CS0 => zenoh::qos::Priority::Background,
        UPriority::CS1 => zenoh::qos::Priority::DataLow,
        UPriority::CS2 => zenoh::qos::Priority::Data,
        UPriority::CS3 => zenoh::qos::Priority::DataHigh,
        UPriority::CS4 => zenoh::qos::Priority::InteractiveLow,
        UPriority::CS5 => zenoh::qos::Priority::InteractiveHigh,
        UPriority::CS6 => zenoh::qos::Priority::RealTime,
    }
}

fn to_zenoh_key_string(
    src_uri: &up_rust_zc::UUri,
    dst_uri: Option<&up_rust_zc::UUri>,
    fallback_authority: &str,
) -> String {
    let src = uri_to_zenoh_key(src_uri, fallback_authority);
    let dst = dst_uri.map_or_else(
        || "{}/{}/{}/{}/{}".to_string(),
        |uuri| uri_to_zenoh_key(uuri, fallback_authority),
    );
    format!("up/{src}/{dst}")
}

fn uri_to_zenoh_key(uri: &up_rust_zc::UUri, fallback_authority: &str) -> String {
    let authority = if uri.authority_name().is_empty() {
        fallback_authority.to_string()
    } else {
        uri.authority_name().to_string()
    };
    let ue_type = if uri.has_wildcard_entity_type() {
        "*".to_string()
    } else {
        format!("{:X}", uri.uentity_type_id())
    };
    let ue_instance = if uri.has_wildcard_entity_instance() {
        "*".to_string()
    } else {
        format!("{:X}", uri.uentity_instance_id())
    };
    let ue_version_major = if uri.has_wildcard_version() {
        "*".to_string()
    } else {
        format!("{:X}", uri.uentity_major_version())
    };
    let resource_id = if uri.has_wildcard_resource_id() {
        "*".to_string()
    } else {
        format!("{:X}", uri.resource_id())
    };
    format!("{authority}/{ue_type}/{ue_instance}/{ue_version_major}/{resource_id}")
}

fn frame_to_attachment(metadata: &UFrameMetadata) -> Result<ZBytes, UStatus> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&crate::UPROTOCOL_MAJOR_VERSION.to_le_bytes());
    bytes.extend_from_slice(FRAME_ATTACHMENT_MAGIC);
    let attributes = metadata
        .attributes()
        .write_to_protobuf_bytes()
        .map_err(|err| {
            UStatus::fail_with_code(
                UCode::InvalidArgument,
                format!("failed to encode UFrameMetadata attributes: {err}"),
            )
        })?;
    append_bytes(&mut bytes, &attributes)?;
    match metadata.payload_encoding() {
        None => bytes.push(0),
        Some(PayloadEncoding::Standard(format)) => {
            bytes.push(1);
            write_i32(&mut bytes, format.as_i32());
        }
        Some(PayloadEncoding::Custom { id, content_type }) => {
            bytes.push(2);
            append_string(&mut bytes, id)?;
            append_string(&mut bytes, content_type)?;
        }
    }
    Ok(ZBytes::from(bytes))
}

#[cfg(test)]
fn attachment_to_frame_metadata(attachment: &ZBytes) -> Result<UFrameMetadata, UStatus> {
    let attachment_bytes = attachment.to_bytes();
    let mut bytes = attachment_bytes.as_ref();
    let version = take_u8(&mut bytes)?;
    if version != crate::UPROTOCOL_MAJOR_VERSION {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!(
                "Expected frame metadata version {} but found version {version}",
                crate::UPROTOCOL_MAJOR_VERSION
            ),
        ));
    }
    let magic = take_bytes(&mut bytes, FRAME_ATTACHMENT_MAGIC.len())?;
    if magic != FRAME_ATTACHMENT_MAGIC {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "invalid frame metadata",
        ));
    }
    let attributes = up_rust_zc::UAttributes::parse_from_protobuf_bytes(take_len_bytes(
        &mut bytes,
    )?)
    .map_err(|err| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!("failed to decode UFrameMetadata attributes: {err}"),
        )
    })?;
    let payload_encoding = match take_u8(&mut bytes)? {
        0 => None,
        1 => Some(PayloadEncoding::Standard(
            UPayloadFormat::from_i32(take_i32(&mut bytes)?).ok_or_else(|| {
                UStatus::fail_with_code(UCode::InvalidArgument, "invalid standard payload format")
            })?,
        )),
        2 => Some(
            PayloadEncoding::custom(take_string(&mut bytes)?, take_string(&mut bytes)?)
                .map_err(|err| UStatus::fail_with_code(UCode::InvalidArgument, err.to_string()))?,
        ),
        _ => {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "invalid payload encoding presence flag",
            ))
        }
    };
    if !bytes.is_empty() {
        return Err(UStatus::fail_with_code(
            UCode::InvalidArgument,
            "trailing frame metadata bytes",
        ));
    }
    UFrameMetadata::new(attributes, payload_encoding)
        .map_err(|err| UStatus::fail_with_code(UCode::InvalidArgument, err.to_string()))
}

fn append_bytes(dst: &mut Vec<u8>, value: &[u8]) -> Result<(), UStatus> {
    let len = u32::try_from(value.len()).map_err(|_| {
        UStatus::fail_with_code(UCode::InvalidArgument, "attachment field is too large")
    })?;
    dst.extend_from_slice(&len.to_le_bytes());
    dst.extend_from_slice(value);
    Ok(())
}

fn append_string(dst: &mut Vec<u8>, value: &str) -> Result<(), UStatus> {
    append_bytes(dst, value.as_bytes())
}

fn write_i32(dst: &mut Vec<u8>, value: i32) {
    dst.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
fn take_u8(src: &mut &[u8]) -> Result<u8, UStatus> {
    let (value, remaining) = src
        .split_first()
        .ok_or_else(|| UStatus::fail_with_code(UCode::InvalidArgument, "invalid attachment"))?;
    *src = remaining;
    Ok(*value)
}

#[cfg(test)]
fn take_i32(src: &mut &[u8]) -> Result<i32, UStatus> {
    let bytes = take_bytes(src, 4)?;
    Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| {
        UStatus::fail_with_code(UCode::InvalidArgument, "invalid attachment")
    })?))
}

#[cfg(test)]
fn take_len_bytes<'a>(src: &mut &'a [u8]) -> Result<&'a [u8], UStatus> {
    let len_bytes = take_bytes(src, 4)?;
    let len = u32::from_le_bytes(
        len_bytes
            .try_into()
            .map_err(|_| UStatus::fail_with_code(UCode::InvalidArgument, "invalid attachment"))?,
    );
    take_bytes(
        src,
        usize::try_from(len).map_err(|_| {
            UStatus::fail_with_code(UCode::InvalidArgument, "invalid attachment length")
        })?,
    )
}

#[cfg(test)]
fn take_string(src: &mut &[u8]) -> Result<String, UStatus> {
    String::from_utf8(take_len_bytes(src)?.to_vec()).map_err(|err| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            format!("attachment field is not valid UTF-8: {err}"),
        )
    })
}

#[cfg(test)]
fn take_bytes<'a>(src: &mut &'a [u8], len: usize) -> Result<&'a [u8], UStatus> {
    let value = src
        .get(..len)
        .ok_or_else(|| UStatus::fail_with_code(UCode::InvalidArgument, "invalid attachment"))?;
    *src = src
        .get(len..)
        .ok_or_else(|| UStatus::fail_with_code(UCode::InvalidArgument, "invalid attachment"))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use up_rust_zc::{
        try_project_umessage_to_frame_metadata, UMessageBuilder, UPayloadFormat, UTxBuffer as _,
        UTxLoanSpec, UUninitTxBuffer as _, UUri, UZeroCopyTransport as _,
        UZeroCopyUninitTransport as _,
    };
    use zenoh::Config;

    use super::*;

    fn payload_metadata(len: usize) -> UFrameMetadata {
        let message = UMessageBuilder::publish(UUri::try_from("//vehicle/4210/1/8000").unwrap())
            .build_with_payload(Bytes::from(vec![0_u8; len]), UPayloadFormat::Raw)
            .expect("message");
        try_project_umessage_to_frame_metadata(&message).expect("metadata")
    }

    #[test]
    fn frame_attachment_round_trips_metadata() {
        let message = UMessageBuilder::publish(UUri::try_from("//vehicle/4210/1/8000").unwrap())
            .build()
            .expect("message");
        let metadata = UFrameMetadata::new(
            message.attributes().clone(),
            Some(PayloadEncoding::custom("native", "application/vnd.example.native").unwrap()),
        )
        .expect("metadata");

        let attachment = frame_to_attachment(&metadata).expect("attachment");
        let decoded = attachment_to_frame_metadata(&attachment).expect("decoded metadata");

        assert_eq!(decoded, metadata);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loan_tx_exposes_shm_payload_bytes() {
        let transport = UPTransportZenoh::builder("vehicle")
            .expect("builder")
            .with_config(Config::default())
            .with_shm_segment_size(1024 * 1024)
            .expect("shm segment size")
            .build()
            .await
            .expect("transport");
        let metadata = payload_metadata(4);
        let mut buffer = transport
            .loan_tx(UTxLoanSpec::payload(metadata.clone(), 4, 1).expect("spec"))
            .await
            .expect("loan");

        buffer.payload_mut().copy_from_slice(b"loan");

        assert_eq!(buffer.metadata(), &metadata);
        assert_eq!(buffer.payload(), b"loan");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loan_uninit_tx_commits_without_zero_fill() {
        let transport = UPTransportZenoh::builder("vehicle")
            .expect("builder")
            .with_config(Config::default())
            .with_shm_segment_size(1024 * 1024)
            .expect("shm segment size")
            .build()
            .await
            .expect("transport");
        let metadata = payload_metadata(4);
        let mut buffer = transport
            .loan_uninit_tx(UTxLoanSpec::payload(metadata.clone(), 4, 1).expect("spec"))
            .await
            .expect("loan");

        for (slot, byte) in buffer.payload_uninit_mut().iter_mut().zip(*b"init") {
            slot.write(byte);
        }
        // SAFETY: every visible byte in the loan was initialized above.
        let buffer = unsafe { buffer.assume_payload_init() };

        assert_eq!(buffer.metadata(), &metadata);
        assert_eq!(buffer.payload(), b"init");
    }
}
