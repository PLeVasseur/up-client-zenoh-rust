/********************************************************************************
 * Copyright (c) 2026 Contributors to the Eclipse Foundation
 *
 * SPDX-License-Identifier: Apache-2.0
 ********************************************************************************/

use std::{str::FromStr, sync::Arc};

#[cfg(feature = "zero-copy")]
use std::sync::OnceLock;
use tracing::error;
use up_rust::{UCode, UPriority, UStatus, UUri};
use zenoh::{qos::Priority, Session};
#[cfg(feature = "zero-copy")]
use zenoh::{
    shm::{PosixShmProviderBackend, ShmProvider, ShmProviderBuilder},
    Wait,
};

#[cfg(feature = "zero-copy")]
type ZenohShmProvider = ShmProvider<PosixShmProviderBackend>;
#[cfg(feature = "zero-copy")]
type ZenohShmProviderInit = Result<Arc<ZenohShmProvider>, String>;

#[cfg(feature = "zero-copy")]
const DEFAULT_SHM_SEGMENT_SIZE: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ZenohWireMechanics {
    session: Arc<Session>,
    uri: UUri,
    #[cfg(feature = "zero-copy")]
    shm_segment_size: usize,
    #[cfg(feature = "zero-copy")]
    shm_provider: Arc<OnceLock<ZenohShmProviderInit>>,
}

impl ZenohWireMechanics {
    pub(crate) async fn new(
        config: crate::zenoh_config::Config,
        uri: impl Into<String>,
    ) -> Result<Self, UStatus> {
        let Ok(session) = zenoh::open(config).await else {
            let msg = "Unable to open Zenoh session".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::Internal, msg));
        };
        Self::from_session(session, uri)
    }

    pub(crate) fn from_session(session: Session, uri: impl Into<String>) -> Result<Self, UStatus> {
        let uri = parse_local_uri(uri)?;
        Ok(Self {
            session: Arc::new(session),
            uri,
            #[cfg(feature = "zero-copy")]
            shm_segment_size: DEFAULT_SHM_SEGMENT_SIZE,
            #[cfg(feature = "zero-copy")]
            shm_provider: Arc::new(OnceLock::new()),
        })
    }

    pub(crate) fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub(crate) fn to_zenoh_key_string(&self, src_uri: &UUri, dst_uri: Option<&UUri>) -> String {
        to_zenoh_key_string(&self.uri, src_uri, dst_uri)
    }

    #[cfg(feature = "zero-copy")]
    pub(crate) fn set_shm_segment_size(&mut self, shm_segment_size: usize) -> Result<(), UStatus> {
        if shm_segment_size == 0 {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Zenoh SHM segment size must be non-zero",
            ));
        }
        self.shm_segment_size = shm_segment_size;
        Ok(())
    }

    #[cfg(feature = "zero-copy")]
    pub(crate) fn shm_provider(&self) -> Result<Arc<ZenohShmProvider>, UStatus> {
        self.shm_provider
            .get_or_init(|| {
                ShmProviderBuilder::default_backend(self.shm_segment_size)
                    .wait()
                    .map(Arc::new)
                    .map_err(|err| err.to_string())
            })
            .clone()
            .map_err(|err| {
                UStatus::fail_with_code(
                    UCode::Internal,
                    format!("failed to initialize Zenoh SHM provider: {err}"),
                )
            })
    }
}

pub(crate) fn parse_local_uri(uri: impl Into<String>) -> Result<UUri, UStatus> {
    let uri = UUri::from_str(&uri.into()).map_err(|_| {
        let msg = "Unable to transform the uri to UUri".to_string();
        error!("{msg}");
        UStatus::fail_with_code(UCode::InvalidArgument, msg)
    })?;
    if uri.has_empty_authority() {
        let msg = "Empty authority is not allowed".to_string();
        error!("{msg}");
        return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg));
    }
    if uri.resource_id() != 0 {
        let msg = "Resource ID should always be 0".to_string();
        error!("{msg}");
        return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg));
    }
    Ok(uri)
}

pub(crate) fn to_zenoh_key_string(
    local_uri: &UUri,
    src_uri: &UUri,
    dst_uri: Option<&UUri>,
) -> String {
    let src = uri_to_zenoh_key(local_uri, src_uri);
    let dst = if let Some(dst) = dst_uri {
        uri_to_zenoh_key(local_uri, dst)
    } else {
        "{}/{}/{}/{}".to_string()
    };
    format!("up/{src}/{dst}")
}

fn uri_to_zenoh_key(local_uri: &UUri, uri: &UUri) -> String {
    let authority = if uri.authority_name().is_empty() {
        local_uri.authority_name().to_string()
    } else {
        uri.authority_name().to_string()
    };
    let ue_id = if uri.has_wildcard_entity_type() || uri.has_wildcard_entity_instance() {
        "*".to_string()
    } else {
        format!(
            "{:X}",
            (u32::from(uri.uentity_instance_id()) << 16) | u32::from(uri.uentity_type_id())
        )
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
    format!("{authority}/{ue_id}/{ue_version_major}/{resource_id}")
}

#[allow(clippy::match_same_arms)]
pub(crate) fn map_zenoh_priority(upriority: UPriority) -> Priority {
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
