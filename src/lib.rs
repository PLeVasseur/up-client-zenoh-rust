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
mod listener_registry;
mod mechanics;
pub mod rpc;
pub mod uri_provider;
pub mod utransport;
#[cfg(feature = "benchmark-owned")]
pub mod wire_full;
#[cfg(feature = "zero-copy")]
mod zero_copy;

pub use rpc::ZenohRpcClient;
#[cfg(feature = "benchmark-owned")]
pub use wire_full::ZenohOwnedCore;
#[cfg(feature = "zero-copy")]
pub use zero_copy::{
    ZenohRxFrame, ZenohTxBuffer, ZenohUninitTxBuffer, ZenohZeroCopyCore, ZenohZeroCopyCoreBuilder,
};

use listener_registry::ListenerRegistry;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tracing::{error, info, warn};
use up_rust::{ComparableListener, ProtobufMappable, UAttributes, UCode, UPriority, UStatus, UUri};
use zenoh::Config;
// Re-export Zenoh config
pub use zenoh::config as zenoh_config;
use zenoh::{
    bytes::ZBytes,
    qos::Priority,
    query::{Query, Queryable},
    Session,
};

const UPROTOCOL_MAJOR_VERSION: u8 = 1;
const DEFAULT_MAX_LISTENERS: usize = 100;
const EXISTING_SESSION_USAGE_MESSAGE_PREFIX: &str =
    "Using an existing Zenoh session for the transport.";
const COMPRESSION_ENABLED_FOR_UNICAST_MESSAGE_PREFIX: &str =
    "Compression for the unicast transport is enabled in the Zenoh configuration";
const COMPRESSION_ENABLED_FOR_MULTICAST_MESSAGE_PREFIX: &str =
    "Compression for the multicast transport is enabled in the Zenoh configuration";

type QueryableMap = Arc<
    Mutex<HashMap<(String, ComparableListener), (Queryable<()>, Arc<up_rust::ListenerAdmission>)>>,
>;
type QueryMap = Arc<Mutex<HashMap<String, Query>>>;
pub struct UPTransportZenoh {
    session: Arc<Session>,
    subscribers: ListenerRegistry,
    // Able to unregister Queryable
    queryable_map: QueryableMap,
    // Save the reqid to be able to send back response
    query_map: QueryMap,
    // URI
    uri: UUri,
}

impl UPTransportZenoh {
    /// Creates a builder for the local authority.
    ///
    /// # Errors
    ///
    /// Rejects an empty, wildcard or invalid authority name.
    pub fn builder<U: Into<String>>(
        local_authority: U,
    ) -> Result<UPTransportZenohBuilder<InitialBuilderState>, UStatus> {
        let authority_name = local_authority.into();
        if authority_name.is_empty() || &authority_name == "*" {
            return Err(UStatus::fail_with_code(
                UCode::InvalidArgument,
                "Authority name must be non-empty and must not be the wildcard authority name",
            ));
        }

        UUri::verify_authority(&authority_name).map_err(|err| {
            UStatus::fail_with_code(
                UCode::InvalidArgument,
                format!("Invalid authority name: {err}"),
            )
        })?;

        Ok(UPTransportZenohBuilder {
            common: Box::new(CommonProperties {
                local_authority: authority_name,
                max_listeners: DEFAULT_MAX_LISTENERS,
            }),
            extra: InitialBuilderState,
        })
    }

    /// Creates a transport with an explicit local entity URI and Zenoh configuration.
    ///
    /// # Errors
    /// Rejects an invalid local URI or a failed Zenoh session.
    pub async fn new(config: Config, uri: impl Into<String>) -> Result<Self, UStatus> {
        let uri = mechanics::parse_local_uri(uri)?;
        let mut transport =
            Self::init_with_config(config, uri.authority_name(), DEFAULT_MAX_LISTENERS).await?;
        transport.uri = uri;
        Ok(transport)
    }

    fn read_bool_config(config: &Config, key: &str) -> Result<bool, UStatus> {
        let Ok(value) = config.get_json(key) else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                format!("Failed to read Zenoh config value for {key}"),
            ));
        };
        let Ok(bool_value) = value.parse::<bool>() else {
            return Err(UStatus::fail_with_code(
                UCode::Internal,
                format!("Failed to parse Zenoh config value for {key} into a boolean"),
            ));
        };
        Ok(bool_value)
    }

    async fn init_with_config(
        config: Config,
        local_authority: &str,
        max_listeners: usize,
    ) -> Result<UPTransportZenoh, UStatus> {
        if Self::read_bool_config(&config, "/transport/unicast/compression/enabled")? {
            warn!(
                "{} (\"/transport/unicast/compression/enabled\"). Note that Zenoh uses the lz4_flex crate for compression in a version that is affected by RUSTSEC-2026-0041, which may result in data leakage.",
                COMPRESSION_ENABLED_FOR_UNICAST_MESSAGE_PREFIX
            );
        }

        if Self::read_bool_config(&config, "/transport/multicast/compression/enabled")? {
            warn!(
                "{} (\"/transport/multicast/compression/enabled\"). Note that Zenoh uses the lz4_flex crate for compression in a version that is affected by RUSTSEC-2026-0041, which may result in data leakage.",
                COMPRESSION_ENABLED_FOR_MULTICAST_MESSAGE_PREFIX
            );
        }

        let session = zenoh::open(config).await.map_err(|err| {
            let msg = "Failed to open Zenoh session";
            error!("{msg}: {err}");
            UStatus::fail_with_code(UCode::Internal, msg)
        })?;
        Ok(Self::init_with_session(
            session,
            local_authority,
            max_listeners,
            // no need to warn about compression here, since we already did that above
            false,
        ))
    }

    fn init_with_session(
        session: Session,
        local_authority: &str,
        max_listeners: usize,
        warn_on_compression_enabled: bool,
    ) -> UPTransportZenoh {
        if warn_on_compression_enabled {
            info!(
                "{} Please be aware that Zenoh uses the lz4_flex crate in a version that is affected by RUST-SEC-2026-0041, which may result in data leakage when compression is enabled for Zenoh. It is therefore strongly recommended to disable compression in the Zenoh configuration when using this transport.",
                EXISTING_SESSION_USAGE_MESSAGE_PREFIX
            );
        }
        let session_to_use = Arc::new(session);
        UPTransportZenoh {
            session: session_to_use.clone(),
            subscribers: ListenerRegistry::new(session_to_use, max_listeners),
            queryable_map: Arc::new(Mutex::new(HashMap::new())),
            query_map: Arc::new(Mutex::new(HashMap::new())),
            // The authority-only builder does not select an application entity.
            // Applications can use a separate URI provider or new with a full URI.
            uri: UUri::try_from_parts(local_authority, 0, 0, 0).expect("validated authority"),
        }
    }

    /// Enables a tracing formatter subscriber that is initialized from the `RUST_LOG` environment variable.
    pub fn try_init_log_from_env() {
        zenoh::init_log_from_env_or("");
    }
}

struct CommonProperties {
    local_authority: String,
    max_listeners: usize,
}

pub struct InitialBuilderState;
pub struct ConfigBuilderState {
    config: zenoh_config::Config,
}
pub struct ConfigPathBuilderState {
    config_path: String,
}

pub struct SessionBuilderState {
    zenoh_session: Session,
}

pub trait BuilderState {}
impl BuilderState for InitialBuilderState {}
impl BuilderState for ConfigBuilderState {}
impl BuilderState for ConfigPathBuilderState {}
impl BuilderState for SessionBuilderState {}

pub struct UPTransportZenohBuilder<S: BuilderState> {
    common: Box<CommonProperties>,
    extra: S,
}

impl UPTransportZenohBuilder<InitialBuilderState> {
    /// Sets the Zenoh configuration to use for the transport.
    ///
    /// Please refer to the [Zenoh documentation](https://zenoh.io/docs/manual/configuration/) for details.
    ///
    /// **Note**: Zenoh uses the `lz4_flex` crate in a version that is affected by
    /// [RUSTSEC-2026-0041](https://rustsec.org/advisories/RUSTSEC-2026-0041),
    /// which may result in data leakage when compression is enabled for Zenoh.
    /// It is therefore strongly recommended to disable compression in the Zenoh configuration when
    /// using this transport.
    #[must_use]
    pub fn with_config(
        self,
        config: zenoh_config::Config,
    ) -> UPTransportZenohBuilder<ConfigBuilderState> {
        UPTransportZenohBuilder {
            common: self.common,
            extra: ConfigBuilderState { config },
        }
    }

    /// Sets the path to a Zenoh configuration file to use for the transport.
    ///
    /// Please refer to the [Zenoh documentation](https://zenoh.io/docs/manual/configuration/) for details.
    ///
    /// **Note**: Zenoh uses the `lz4_flex` crate in a version that is affected by
    /// [RUSTSEC-2026-0041](https://rustsec.org/advisories/RUSTSEC-2026-0041),
    /// which may result in data leakage when compression is enabled for Zenoh.
    /// It is therefore strongly recommended to disable compression in the Zenoh configuration when
    /// using this transport.
    #[must_use]
    pub fn with_config_path(
        self,
        config_path: String,
    ) -> UPTransportZenohBuilder<ConfigPathBuilderState> {
        UPTransportZenohBuilder {
            common: self.common,
            extra: ConfigPathBuilderState { config_path },
        }
    }

    /// Sets an existing Zenoh session to use for the transport.
    ///
    /// **Note**: Zenoh uses the `lz4_flex` crate in a version that is affected by
    /// [RUSTSEC-2026-0041](https://rustsec.org/advisories/RUSTSEC-2026-0041),
    /// which may result in data leakage when compression is enabled for Zenoh.
    /// It is therefore strongly recommended to disable compression in the Zenoh configuration when
    /// using this transport.
    #[must_use]
    pub fn with_session(
        self,
        zenoh_session: Session,
    ) -> UPTransportZenohBuilder<SessionBuilderState> {
        UPTransportZenohBuilder {
            common: self.common,
            extra: SessionBuilderState { zenoh_session },
        }
    }
}

impl UPTransportZenohBuilder<ConfigBuilderState> {
    /// Creates the transport based on the provided configuration properties.
    ///
    /// # Returns
    ///
    /// The newly created transport instance. Note that the builder consumes itself.
    ///
    /// # Errors
    ///
    /// Returns an error if the transport cannot be created.
    ///
    /// # Examples
    ///
    /// ```
    /// #[tokio::main]
    /// # async fn main() {
    /// use up_transport_zenoh::{zenoh_config, UPTransportZenoh};
    /// let transport = UPTransportZenoh::builder("vehicle1").unwrap()
    ///     .with_config(zenoh_config::Config::default()).with_max_listeners(10)
    ///     .build().await.unwrap();
    /// # }
    /// ```
    pub async fn build(self) -> Result<UPTransportZenoh, UStatus> {
        UPTransportZenoh::init_with_config(
            self.extra.config,
            &self.common.local_authority,
            self.common.max_listeners,
        )
        .await
    }
}

impl UPTransportZenohBuilder<ConfigPathBuilderState> {
    /// Reads the configuration file and creates a transport.
    ///
    /// # Errors
    /// Returns an error for an unreadable/invalid configuration or a failed session.
    pub async fn build(self) -> Result<UPTransportZenoh, UStatus> {
        let config = Config::from_file(self.extra.config_path)
            .map_err(|error| UStatus::fail_with_code(UCode::InvalidArgument, error.to_string()))?;
        UPTransportZenoh::init_with_config(
            config,
            &self.common.local_authority,
            self.common.max_listeners,
        )
        .await
    }
}

impl UPTransportZenohBuilder<SessionBuilderState> {
    /// Creates the transport based on the provided configuration file.
    ///
    /// # Returns
    ///
    /// The newly created transport instance. Note that the builder consumes itself.
    ///
    /// # Errors
    ///
    /// Returns an error if the transport cannot be created.
    ///
    /// # Examples
    ///
    /// ```
    /// #[tokio::main]
    /// # async fn main() {
    /// use up_transport_zenoh::UPTransportZenoh;
    /// use zenoh::{Config, Session};
    ///
    /// let zenoh_session = zenoh::open(Config::default()).await.expect("Failed to open Zenoh session");
    /// assert!(UPTransportZenoh::builder("local_authority")
    ///    .expect("Invalid authority name")
    ///    .with_session(zenoh_session)
    ///    .with_max_listeners(10)
    ///    .build()
    ///    .is_ok());
    /// # }
    /// ```
    pub fn build(self) -> Result<UPTransportZenoh, UStatus> {
        Ok(UPTransportZenoh::init_with_session(
            self.extra.zenoh_session,
            &self.common.local_authority,
            self.common.max_listeners,
            true, // warn about compression enabled in existing session
        ))
    }
}

impl<S: BuilderState> UPTransportZenohBuilder<S> {
    /// Sets the maximum number of listeners; the default is 100.
    #[must_use]
    pub fn with_max_listeners(mut self, max_listeners: usize) -> Self {
        self.common.max_listeners = max_listeners;
        self
    }
}

impl UPTransportZenoh {
    // The format of Zenoh key should be
    // up/[src.authority]/[src.ue_type]/[src.ue_instance]/[src.ue_version_major]/[src.resource_id]/[sink.authority]/[sink.ue_type]/[sink.ue_instance]/[sink.ue_version_major]/[sink.resource_id]
    fn to_zenoh_key_string(&self, src_uri: &UUri, dst_uri: Option<&UUri>) -> String {
        mechanics::to_zenoh_key_string(&self.uri, src_uri, dst_uri)
    }

    #[allow(clippy::match_same_arms)]
    fn map_zenoh_priority(upriority: UPriority) -> Priority {
        mechanics::map_zenoh_priority(upriority)
    }

    fn uattributes_to_attachment(uattributes: &UAttributes) -> anyhow::Result<ZBytes> {
        let mut attachment = Vec::new();
        attachment.push(UPROTOCOL_MAJOR_VERSION);
        attachment.extend_from_slice(&uattributes.write_to_protobuf_bytes()?);
        Ok(ZBytes::from(attachment))
    }

    fn attachment_to_uattributes(attachment: &ZBytes) -> anyhow::Result<UAttributes> {
        let attachment = attachment.to_bytes();
        let Some((&version, bytes)) = attachment.as_ref().split_first() else {
            let msg = "Unable to get the UAttributes version".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg).into());
        };
        if version != UPROTOCOL_MAJOR_VERSION {
            let msg =
                format!("UAttributes version is {version} (should be {UPROTOCOL_MAJOR_VERSION})");
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg).into());
        }
        if bytes.is_empty() {
            let msg = "Unable to get the UAttributes".to_string();
            error!("{msg}");
            return Err(UStatus::fail_with_code(UCode::InvalidArgument, msg).into());
        }
        UAttributes::parse_from_protobuf_bytes(bytes).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use test_case::test_case;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct SharedLogBuffer {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl SharedLogBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.bytes.lock().expect("log buffer poisoned").clone())
                .expect("log output should be valid UTF-8")
        }
    }

    struct SharedLogWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .expect("log buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedLogBuffer {
        type Writer = SharedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedLogWriter {
                bytes: Arc::clone(&self.bytes),
            }
        }
    }

    #[test_case(
        "/transport/unicast/compression/enabled",
        COMPRESSION_ENABLED_FOR_UNICAST_MESSAGE_PREFIX;
        "emits warning for unicast compression"
    )]
    #[test_case(
        "/transport/multicast/compression/enabled",
        COMPRESSION_ENABLED_FOR_MULTICAST_MESSAGE_PREFIX;
        "emits warning for multicast compression"
    )]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[serial_test::serial]
    async fn test_builder_emits_warning_for_config_with_compression_enabled(
        compression_key_expr: &str,
        expected_message: &str,
    ) {
        let logs = SharedLogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);

        let mut config = zenoh_config::Config::default();
        config
            .insert_json5(compression_key_expr, "true")
            .expect("Failed to set compression enabled in config");

        let result = UPTransportZenoh::builder("local_authority")
            .expect("failed to create builder")
            .with_config(config)
            .build()
            .await;

        assert!(result.is_ok(), "builder should succeed and emit a warning");

        let output = logs.contents();
        assert!(
            output.contains(expected_message),
            "captured logs:\n{output}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[serial_test::serial]
    async fn test_builder_emits_info_when_using_existing_session() {
        let logs = SharedLogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);

        let session = zenoh::open(zenoh_config::Config::default())
            .await
            .expect("Failed to open Zenoh session");

        let result = UPTransportZenoh::builder("local_authority")
            .expect("failed to create builder")
            .with_session(session)
            .build();

        assert!(
            result.is_ok(),
            "builder should succeed and emit an info message"
        );

        let output = logs.contents();
        assert!(
            output.contains(EXISTING_SESSION_USAGE_MESSAGE_PREFIX),
            "captured logs:\n{output}"
        );
    }

    #[test_case("vehicle1" => true; "valid authority")]
    #[test_case("This is not an authority name" => false; "invalid authority")]
    #[test_case("" => false; "empty authority")]
    #[test_case("*" => false; "wildcard authority")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_getting_a_builder(local_authority: &str) -> bool {
        match UPTransportZenoh::builder(local_authority) {
            Ok(builder) => builder.with_config(Config::default()).build().await.is_ok(),
            Err(_) => false,
        }
    }

    #[test_case("//vehicle1/AABB/7/0", true; "succeeds with valid UUri")]
    #[test_case("This is not UUri", false; "fails with invalid UUri")]
    #[test_case("/AABB/7/0", false; "fails with empty UAuthority")]
    #[test_case("//vehicle1/AABB/7/1", false; "fails with non-zero resource ID")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_new_up_transport_zenoh(uri: &str, expected_result: bool) {
        let up_transport_zenoh = UPTransportZenoh::new(zenoh_config::Config::default(), uri).await;
        assert_eq!(up_transport_zenoh.is_ok(), expected_result);
    }

    // Mapping with the examples in Zenoh spec
    #[test_case("/10AB/3/80CD", None, "up/192.168.1.100/10AB/0/3/80CD/{}/{}/{}/{}/{}"; "Send Publish")]
    #[test_case("//192.168.1.100/10AB/3/80CD", None, "up/192.168.1.100/10AB/0/3/80CD/{}/{}/{}/{}/{}"; "Subscribe messages")]
    #[test_case("//192.168.1.100/10AB/3/80CD", Some("//192.168.1.101/20EF/4/0"), "up/192.168.1.100/10AB/0/3/80CD/192.168.1.101/20EF/0/4/0"; "Send Notification")]
    #[test_case("//*/FFFF/FF/FFFF", Some("//192.168.1.101/20EF/4/0"), "up/*/*/0/*/*/192.168.1.101/20EF/0/4/0"; "Receive all Notifications")]
    #[test_case("//my-host1/10AB/3/0", Some("//my-host2/20EF/4/B"), "up/my-host1/10AB/0/3/0/my-host2/20EF/0/4/B"; "Send Request")]
    #[test_case("//*/FFFF/FF/FFFF", Some("//my-host2/20EF/4/B"), "up/*/*/0/*/*/my-host2/20EF/0/4/B"; "Receive all Requests")]
    #[test_case("//*/FFFF/FF/FFFF", Some("//[::1]/FFFF/FF/FFFF"), "up/*/*/0/*/*/[::1]/*/0/*/*"; "Receive all messages to a device")]
    fn test_to_zenoh_key_string(src_uri: &str, sink_uri: Option<&str>, zenoh_key: &str) {
        let local_uri = UUri::from_str("//192.168.1.100/10AB/3/0").unwrap();
        let src = UUri::from_str(src_uri).unwrap();
        if let Some(sink) = sink_uri {
            let sink = UUri::from_str(sink).unwrap();
            assert_eq!(
                mechanics::to_zenoh_key_string(&local_uri, &src, Some(&sink)),
                zenoh_key.to_string()
            );
        } else {
            assert_eq!(
                mechanics::to_zenoh_key_string(&local_uri, &src, None),
                zenoh_key.to_string()
            );
        }
    }
}
