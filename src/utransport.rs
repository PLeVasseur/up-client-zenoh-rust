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
use std::sync::Arc;
use tracing::{error, trace};
use up_rust::{
    ComparableOwnedListener, UAttributes, UCode, UEncoding, UFrameMetadata, UMessageType,
    UOwnedFrame, UOwnedListener, UOwnedTransport, UPriority, UStatus, UUri, UUID,
};
use zenoh::{bytes::ZBytes, qos::Priority};

const FRAME_ATTACHMENT_MAGIC: &[u8; 4] = b"UFRM";

fn frame_to_attachment(header: &UFrameMetadata) -> anyhow::Result<ZBytes> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&crate::UPROTOCOL_MAJOR_VERSION.to_le_bytes());
    bytes.extend_from_slice(FRAME_ATTACHMENT_MAGIC);
    write_u64(&mut bytes, header.attributes().id().msb());
    write_u64(&mut bytes, header.attributes().id().lsb());
    bytes.push(message_type_to_byte(header.attributes().message_type()));
    bytes.push(priority_to_byte(header.attributes().priority()));
    write_optional_u32(&mut bytes, header.attributes().ttl());
    append_string(&mut bytes, &header.attributes().source().to_uri(false))?;
    append_string(
        &mut bytes,
        header
            .attributes()
            .sink()
            .map(|uri| uri.to_uri(false))
            .as_deref()
            .unwrap_or_default(),
    )?;
    append_string(&mut bytes, header.encoding().format_id())?;
    append_string(&mut bytes, header.encoding().content_type())?;
    append_string(
        &mut bytes,
        header.encoding().schema_ref().unwrap_or_default(),
    )?;
    write_optional_uuid(&mut bytes, header.attributes().request_id());
    write_optional_string(&mut bytes, header.attributes().traceparent())?;
    write_optional_string(&mut bytes, header.attributes().token())?;
    write_optional_u32(&mut bytes, header.attributes().permission_level());
    write_optional_code(&mut bytes, header.attributes().commstatus());
    Ok(ZBytes::from(bytes))
}

pub(crate) fn attachment_to_frame_metadata(attachment: &ZBytes) -> anyhow::Result<UFrameMetadata> {
    let attachment_bytes = attachment.to_bytes();
    let mut bytes = attachment_bytes.as_ref();
    let version = take_u8(&mut bytes)?;
    if version != crate::UPROTOCOL_MAJOR_VERSION {
        return Err(UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            format!(
                "Expected frame metadata version {} but found version {}",
                crate::UPROTOCOL_MAJOR_VERSION,
                version
            ),
        )
        .into());
    }
    let magic = take_bytes(&mut bytes, FRAME_ATTACHMENT_MAGIC.len())?;
    if magic != FRAME_ATTACHMENT_MAGIC {
        return Err(
            UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid frame metadata").into(),
        );
    }

    let id = UUID::from_u64_pair(take_u64(&mut bytes)?, take_u64(&mut bytes)?)?;
    let message_type = byte_to_message_type(take_u8(&mut bytes)?)?;
    let priority = byte_to_priority(take_u8(&mut bytes)?)?;
    let ttl = take_optional_u32(&mut bytes)?;
    let source = UUri::try_from(take_string(&mut bytes)?.as_str())?;
    let sink = {
        let sink = take_string(&mut bytes)?;
        if sink.is_empty() {
            None
        } else {
            Some(UUri::try_from(sink.as_str())?)
        }
    };
    let format_id = take_string(&mut bytes)?;
    let content_type = take_string(&mut bytes)?;
    let schema_ref = take_string(&mut bytes)?;
    let schema_ref = if schema_ref.is_empty() {
        None
    } else {
        Some(schema_ref)
    };
    let request_id = take_optional_uuid(&mut bytes)?;
    let traceparent = take_optional_string(&mut bytes)?;
    let token = take_optional_string(&mut bytes)?;
    let permission_level = take_optional_u32(&mut bytes)?;
    let commstatus = take_optional_code(&mut bytes)?;

    let mut attributes = UAttributes::new(id, source, sink, message_type).with_priority(priority);
    if let Some(ttl) = ttl {
        attributes = attributes.with_ttl(ttl);
    }
    if let Some(request_id) = request_id {
        attributes = attributes.with_request_id(request_id);
    }
    if let Some(traceparent) = traceparent {
        attributes = attributes.with_traceparent(traceparent);
    }
    if let Some(token) = token {
        attributes = attributes.with_token(token);
    }
    if let Some(permission_level) = permission_level {
        attributes = attributes.with_permission_level(permission_level);
    }
    if let Some(commstatus) = commstatus {
        attributes = attributes.with_commstatus(commstatus);
    }
    Ok(UFrameMetadata::new(
        attributes,
        UEncoding::new(format_id, content_type, schema_ref),
    ))
}

fn write_u64(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(&value.to_le_bytes());
}

fn write_optional_u32(dst: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(value) => {
            dst.push(1);
            dst.extend_from_slice(&value.to_le_bytes());
        }
        None => dst.push(0),
    }
}

fn write_optional_uuid(dst: &mut Vec<u8>, value: Option<&UUID>) {
    match value {
        Some(value) => {
            dst.push(1);
            write_u64(dst, value.msb());
            write_u64(dst, value.lsb());
        }
        None => dst.push(0),
    }
}

fn write_optional_string(dst: &mut Vec<u8>, value: Option<&str>) -> anyhow::Result<()> {
    match value {
        Some(value) => {
            dst.push(1);
            append_string(dst, value)?;
        }
        None => dst.push(0),
    }
    Ok(())
}

fn write_optional_code(dst: &mut Vec<u8>, value: Option<UCode>) {
    match value {
        Some(value) => {
            dst.push(1);
            dst.push(value.as_u8());
        }
        None => dst.push(0),
    }
}

fn append_string(dst: &mut Vec<u8>, value: &str) -> anyhow::Result<()> {
    let len = u32::try_from(value.len()).map_err(|_| {
        UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "attachment field is too large")
    })?;
    dst.extend_from_slice(&len.to_le_bytes());
    dst.extend_from_slice(value.as_bytes());
    Ok(())
}

fn take_u8(src: &mut &[u8]) -> anyhow::Result<u8> {
    let (value, remaining) = src
        .split_first()
        .ok_or_else(|| UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid attachment"))?;
    *src = remaining;
    Ok(*value)
}

fn take_u64(src: &mut &[u8]) -> anyhow::Result<u64> {
    let bytes = take_bytes(src, 8)?;
    Ok(u64::from_le_bytes(bytes.try_into()?))
}

fn take_optional_u32(src: &mut &[u8]) -> anyhow::Result<Option<u32>> {
    match take_u8(src)? {
        0 => Ok(None),
        1 => {
            let bytes = take_bytes(src, 4)?;
            Ok(Some(u32::from_le_bytes(bytes.try_into()?)))
        }
        _ => Err(UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid optional value").into()),
    }
}

fn take_optional_uuid(src: &mut &[u8]) -> anyhow::Result<Option<UUID>> {
    match take_u8(src)? {
        0 => Ok(None),
        1 => Ok(Some(UUID::from_u64_pair(take_u64(src)?, take_u64(src)?)?)),
        _ => Err(UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid optional value").into()),
    }
}

fn take_optional_string(src: &mut &[u8]) -> anyhow::Result<Option<String>> {
    match take_u8(src)? {
        0 => Ok(None),
        1 => Ok(Some(take_string(src)?)),
        _ => Err(UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid optional value").into()),
    }
}

fn take_optional_code(src: &mut &[u8]) -> anyhow::Result<Option<UCode>> {
    match take_u8(src)? {
        0 => Ok(None),
        1 => UCode::from_u8(take_u8(src)?).map(Some).ok_or_else(|| {
            UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid status code").into()
        }),
        _ => Err(UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid optional value").into()),
    }
}

fn take_string(src: &mut &[u8]) -> anyhow::Result<String> {
    let len_bytes = take_bytes(src, 4)?;
    let len = usize::try_from(u32::from_le_bytes(len_bytes.try_into()?))?;
    let bytes = take_bytes(src, len)?;
    String::from_utf8(bytes.to_vec()).map_err(|e| {
        UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            format!("attachment field is not valid UTF-8: {e}"),
        )
        .into()
    })
}

fn take_bytes<'a>(src: &mut &'a [u8], len: usize) -> anyhow::Result<&'a [u8]> {
    let value = src
        .get(..len)
        .ok_or_else(|| UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid attachment"))?;
    *src = src
        .get(len..)
        .ok_or_else(|| UStatus::fail_with_code(UCode::INVALID_ARGUMENT, "invalid attachment"))?;
    Ok(value)
}

fn message_type_to_byte(message_type: UMessageType) -> u8 {
    match message_type {
        UMessageType::Publish => 1,
        UMessageType::Notification => 2,
        UMessageType::Request => 3,
        UMessageType::Response => 4,
    }
}

fn byte_to_message_type(value: u8) -> Result<UMessageType, UStatus> {
    match value {
        1 => Ok(UMessageType::Publish),
        2 => Ok(UMessageType::Notification),
        3 => Ok(UMessageType::Request),
        4 => Ok(UMessageType::Response),
        _ => Err(UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            "invalid message type",
        )),
    }
}

fn priority_to_byte(priority: UPriority) -> u8 {
    match priority {
        UPriority::CS0 => 0,
        UPriority::CS1 => 1,
        UPriority::CS2 => 2,
        UPriority::CS3 => 3,
        UPriority::CS4 => 4,
        UPriority::CS5 => 5,
        UPriority::CS6 => 6,
    }
}

fn byte_to_priority(value: u8) -> Result<UPriority, UStatus> {
    match value {
        0 => Ok(UPriority::CS0),
        1 => Ok(UPriority::CS1),
        2 => Ok(UPriority::CS2),
        3 => Ok(UPriority::CS3),
        4 => Ok(UPriority::CS4),
        5 => Ok(UPriority::CS5),
        6 => Ok(UPriority::CS6),
        _ => Err(UStatus::fail_with_code(
            UCode::INVALID_ARGUMENT,
            "invalid priority",
        )),
    }
}

fn map_zenoh_priority(upriority: UPriority) -> Priority {
    match upriority {
        UPriority::CS0 => Priority::Background,
        UPriority::CS1 => Priority::DataLow,
        UPriority::CS2 => Priority::Data,
        UPriority::CS3 => Priority::DataHigh,
        UPriority::CS4 => Priority::InteractiveLow,
        UPriority::CS5 => Priority::InteractiveHigh,
        UPriority::CS6 => Priority::RealTime,
    }
}

fn uri_to_zenoh_key(uri: &UUri, fallback_authority: &str) -> String {
    let authority = if uri.authority_name().is_empty() {
        fallback_authority.to_string()
    } else {
        uri.authority_name()
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

fn to_zenoh_key_string(src_uri: &UUri, dst_uri: Option<&UUri>, fallback_authority: &str) -> String {
    let src = uri_to_zenoh_key(src_uri, fallback_authority);
    let dst = dst_uri.map_or_else(
        || "{}/{}/{}/{}/{}".to_string(),
        |uuri| uri_to_zenoh_key(uuri, fallback_authority),
    );
    format!("up/{src}/{dst}")
}

#[async_trait]
impl UOwnedTransport for UPTransportZenoh {
    async fn send_owned(&self, frame: UOwnedFrame) -> Result<(), UStatus> {
        let header = frame.metadata();
        let zenoh_key = to_zenoh_key_string(
            header.attributes().source(),
            header.attributes().sink(),
            self.local_authority.as_str(),
        );
        let attachment = frame_to_attachment(header).map_err(|e| {
            let msg = format!("Unable to transform UFrameMetadata to attachment: {e}");
            error!("{msg}");
            UStatus::fail_with_code(UCode::INVALID_ARGUMENT, msg)
        })?;
        let priority = map_zenoh_priority(header.attributes().priority());

        self.session
            .put(&zenoh_key, frame.payload().clone())
            .priority(priority)
            .attachment(attachment)
            .await
            .inspect(|()| trace!("putting owned frame with key: {zenoh_key}"))
            .map_err(|e| {
                UStatus::fail_with_code(UCode::INTERNAL, format!("failed to put Zenoh frame: {e}"))
            })?;
        Ok(())
    }

    async fn register_owned_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UOwnedListener>,
    ) -> Result<(), UStatus> {
        up_rust::verify_filter_criteria(source_filter, sink_filter)?;
        let zenoh_key =
            to_zenoh_key_string(source_filter, sink_filter, self.local_authority.as_str());
        self.subscribers
            .register_owned_subscriber(zenoh_key, listener)
            .await
    }

    async fn unregister_owned_listener(
        &self,
        source_filter: &UUri,
        sink_filter: Option<&UUri>,
        listener: Arc<dyn UOwnedListener>,
    ) -> Result<(), UStatus> {
        up_rust::verify_filter_criteria(source_filter, sink_filter)?;
        let zenoh_key =
            to_zenoh_key_string(source_filter, sink_filter, self.local_authority.as_str());
        self.subscribers
            .unregister_owned(zenoh_key.as_str(), ComparableOwnedListener::new(listener))
            .await
    }
}
