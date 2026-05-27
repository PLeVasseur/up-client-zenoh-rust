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

/*!
This crate provides an implementation of the Eclipse Zenoh &trade; uProtocol Transport.
The transport uses Zenoh's publish-subscribe mechanism to exchange native
uProtocol frames from the [up-rust](https://crates.io/crates/up_rust) crate.

`UPTransportZenoh` always implements `up_rust::UOwnedTransport`. The owned path
stores `UFrameMetadata` in Zenoh attachments and publishes the application
payload as Zenoh payload bytes.

When the `zero-copy` feature is enabled, `UPTransportZenoh` also implements
`up_rust::zero_copy::UZeroCopyTransport`. The transmit path reserves Zenoh
shared-memory payload buffers using Zenoh's `shared-memory` and `unstable`
features. Frame metadata is fixed when the loan is reserved and mapped to the
Zenoh key, priority, and attachment before callers write payload bytes. The
receive path exposes Zenoh `ZBytes` through ordered readers and slice iterators
and does not coalesce segmented payloads into an owned buffer unless callers
explicitly cross an owned-frame adapter boundary.

The transport is designed to run in the context of a [tokio `Runtime`] which
needs to be configured outside of the transport according to the
processing requirements of the use case at hand. The transport does
not make any implicit assumptions about the number of threads available
and does not spawn any threads itself.

[tokio `Runtime`]: https://docs.rs/tokio/latest/tokio/runtime/index.html
*/

#![warn(rustdoc::bare_urls, rustdoc::broken_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod listener_registry;
pub(crate) mod utransport;
#[cfg(feature = "zero-copy")]
mod zero_copy;

use std::sync::Arc;
#[cfg(feature = "zero-copy")]
use std::sync::OnceLock;

use listener_registry::ListenerRegistry;
use tracing::error;
use up_rust::{UCode, UStatus, UUri};
#[cfg(feature = "zero-copy")]
use zenoh::{
    shm::{PosixShmProviderBackend, ShmProvider, ShmProviderBuilder},
    Wait,
};
use zenoh::{Config, Session};
// Re-export Zenoh config
pub use zenoh::config as zenoh_config;
#[cfg(feature = "zero-copy")]
#[cfg_attr(docsrs, doc(cfg(feature = "zero-copy")))]
pub use zero_copy::{ZenohRxFrame, ZenohTxBuffer};

const UPROTOCOL_MAJOR_VERSION: u8 = 1;
const DEFAULT_MAX_LISTENERS: usize = 100;
#[cfg(feature = "zero-copy")]
const DEFAULT_SHM_SEGMENT_SIZE: usize = 64 * 1024 * 1024;

#[cfg(feature = "zero-copy")]
type ZenohShmProvider = ShmProvider<PosixShmProviderBackend>;
#[cfg(feature = "zero-copy")]
type ZenohShmProviderInit = Result<Arc<ZenohShmProvider>, String>;

/// An Eclipse Zenoh &trade; based uProtocol transport implementation.
///
/// The transport implements [`up_rust::UOwnedTransport`] in all builds. With the
/// `zero-copy` feature enabled, it also implements
/// [`up_rust::zero_copy::UZeroCopyTransport`] using Zenoh shared-memory payload
/// buffers on transmit and `ZBytes` lease views on receive.
///
/// Listener registrations are push-oriented. The owned listener path registers
/// callbacks on the Zenoh runtime for listeners registered through
/// [`up_rust::UOwnedTransport::register_owned_listener`]. The zero-copy listener
/// path, when enabled, registers independent subscribers for each listener so
/// exact and wildcard registrations can receive the same matching frame without
/// consuming a single shared receive value.
///
/// <div class="warning">
///
/// The registered listeners are being invoked sequentially on the **same thread**
/// that the callback is being executed on. Implementers of listeners are therefore
/// **strongly advised** to move non-trivial processing logic to **another/dedicated
/// thread**, if necessary. Please refer to `subscriber` and `notification_receiver`
/// in the examples directory for how this can be done.
///
/// </div>
pub struct UPTransportZenoh {
    session: Arc<Session>,
    subscribers: ListenerRegistry,
    local_authority: String,
    #[cfg(feature = "zero-copy")]
    shm_segment_size: usize,
    #[cfg(feature = "zero-copy")]
    shm_provider: OnceLock<ZenohShmProviderInit>,
}

impl UPTransportZenoh {
    /// Gets a builder for creating a new Zenoh transport.
    ///
    /// # Arguments
    ///
    /// * `local_uri` - The URI identifying the (local) uEntity that the transport runs on.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI contains an empty or wildcard authority name
    /// or has a non-zero resource ID.
    pub fn builder<U: Into<String>>(
        local_authority: U,
    ) -> Result<UPTransportZenohBuilder<InitialBuilderState>, UStatus> {
        let authority_name = local_authority.into();
        if authority_name.is_empty() || &authority_name == "*" {
            return Err(UStatus::fail_with_code(
                UCode::INVALID_ARGUMENT,
                "Authority name must be non-empty and must not be the wildcard authority name",
            ));
        }

        UUri::verify_authority(&authority_name).map_err(|err| {
            UStatus::fail_with_code(
                UCode::INVALID_ARGUMENT,
                format!("Invalid authority name: {err}"),
            )
        })?;

        Ok(UPTransportZenohBuilder {
            common: Box::new(CommonProperties {
                local_authority: authority_name,
                max_listeners: DEFAULT_MAX_LISTENERS,
                #[cfg(feature = "zero-copy")]
                shm_segment_size: DEFAULT_SHM_SEGMENT_SIZE,
            }),
            extra: InitialBuilderState,
        })
    }

    async fn init_with_config(
        config: Config,
        common: CommonProperties,
    ) -> Result<UPTransportZenoh, UStatus> {
        let session = zenoh::open(config).await.map_err(|err| {
            let msg = "Failed to open Zenoh session";
            error!("{msg}: {err}");
            UStatus::fail_with_code(UCode::INTERNAL, msg)
        })?;
        Ok(Self::init_with_session(session, common))
    }

    fn init_with_session(session: Session, common: CommonProperties) -> UPTransportZenoh {
        let session_to_use = Arc::new(session);
        UPTransportZenoh {
            session: session_to_use.clone(),
            subscribers: ListenerRegistry::new(session_to_use, common.max_listeners),
            local_authority: common.local_authority,
            #[cfg(feature = "zero-copy")]
            shm_segment_size: common.shm_segment_size,
            #[cfg(feature = "zero-copy")]
            shm_provider: OnceLock::new(),
        }
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
                    UCode::INTERNAL,
                    format!("failed to initialize Zenoh SHM provider: {err}"),
                )
            })
    }

    /// Enables a tracing formatter subscriber that is initialized from the `RUST_LOG` environment variable.
    pub fn try_init_log_from_env() {
        zenoh::init_log_from_env_or("");
    }
}

struct CommonProperties {
    local_authority: String,
    max_listeners: usize,
    #[cfg(feature = "zero-copy")]
    shm_segment_size: usize,
}

/// Initial builder state before a Zenoh configuration source has been selected.
pub struct InitialBuilderState;

/// Builder state that owns an in-memory Zenoh configuration.
pub struct ConfigBuilderState {
    config: zenoh_config::Config,
}

/// Builder state that owns the path to a Zenoh configuration file.
pub struct ConfigPathBuilderState {
    config_path: String,
}

/// Builder state that wraps an already-open Zenoh session.
pub struct SessionBuilderState {
    zenoh_session: Session,
}

/// Marker trait for typestate builder states.
pub trait BuilderState {}
impl BuilderState for InitialBuilderState {}
impl BuilderState for ConfigBuilderState {}
impl BuilderState for ConfigPathBuilderState {}
impl BuilderState for SessionBuilderState {}

/// Typestate builder for [`UPTransportZenoh`].
///
/// Start with [`UPTransportZenoh::builder`], choose one configuration source,
/// optionally set the maximum listener count, and then call `build` on the
/// resulting state.
///
/// ```no_run
/// # async fn build() -> Result<(), up_rust::UStatus> {
/// use up_transport_zenoh::UPTransportZenoh;
///
/// let transport = UPTransportZenoh::builder("vehicle")?
///     .with_config(Default::default())
///     .with_max_listeners(256)
///     .build()
///     .await?;
/// # let _ = transport;
/// # Ok(())
/// # }
/// ```
pub struct UPTransportZenohBuilder<S: BuilderState> {
    common: Box<CommonProperties>,
    extra: S,
}

impl UPTransportZenohBuilder<InitialBuilderState> {
    /// Sets the Zenoh configuration to use for the transport.
    ///
    /// Please refer to the [Zenoh documentation](https://zenoh.io/docs/manual/configuration/) for details.
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
    ///
    /// assert!(UPTransportZenoh::builder("local_authority")
    ///    .expect("Invalid authority name")
    ///    .with_config(zenoh_config::Config::default())
    ///    .with_max_listeners(10)
    ///    .build()
    ///    .await
    ///    .is_ok());
    /// # }
    /// ```
    pub async fn build(self) -> Result<UPTransportZenoh, UStatus> {
        UPTransportZenoh::init_with_config(self.extra.config, *self.common).await
    }
}

impl UPTransportZenohBuilder<ConfigPathBuilderState> {
    /// Creates the transport based on the provided configuration file.
    ///
    /// # Returns
    ///
    /// The newly created transport instance. Note that the builder consumes itself.
    ///
    /// # Errors
    ///
    /// Returns an error if the transport cannot be created, e.g. because the configuration
    /// file cannot be read or is invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// #[tokio::main]
    /// # async fn main() {
    /// use up_transport_zenoh::UPTransportZenoh;
    ///
    /// assert!(UPTransportZenoh::builder("local_authority")
    ///    .expect("Invalid authority name")
    ///    .with_config_path("non-existing-config.json5".to_string())
    ///    .build()
    ///    .await
    ///    .is_err_and(|e| e.get_code() == up_rust::UCode::INVALID_ARGUMENT));
    /// # }
    /// ```
    pub async fn build(self) -> Result<UPTransportZenoh, UStatus> {
        let config = zenoh_config::Config::from_file(self.extra.config_path).map_err(|e| {
            error!("Failed to load Zenoh config from file: {e}");
            UStatus::fail_with_code(UCode::INVALID_ARGUMENT, e.to_string())
        })?;
        UPTransportZenoh::init_with_config(config, *self.common).await
    }
}

impl UPTransportZenohBuilder<SessionBuilderState> {
    /// Creates the transport around the provided Zenoh session.
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
    /// use zenoh::config::Config;
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
            *self.common,
        ))
    }
}

impl<S: BuilderState> UPTransportZenohBuilder<S> {
    /// Sets the maximum number of listeners that can be registered with this transport.
    /// If not set explicitly, the default value is 100.
    #[must_use]
    pub fn with_max_listeners(mut self, max_listeners: usize) -> Self {
        self.common.max_listeners = max_listeners;
        self
    }

    /// Sets the Zenoh shared-memory provider segment size in bytes.
    ///
    /// The value is used lazily when the first zero-copy transmit loan is
    /// reserved. If not set explicitly, the default is 64 MiB.
    #[cfg(feature = "zero-copy")]
    #[cfg_attr(docsrs, doc(cfg(feature = "zero-copy")))]
    #[must_use]
    pub fn with_shm_segment_size(mut self, shm_segment_size: usize) -> Self {
        self.common.shm_segment_size = shm_segment_size;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("vehicle1" => true; "succeeds for valid authority name")]
    #[test_case("This is not an authority name" => false; "fails for invalid authority name")]
    #[test_case("" => false; "fails for empty authority name")]
    #[test_case("*" => false; "fails for wildcard authority name")]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_getting_a_builder<S: Into<String>>(local_authority: S) -> bool {
        if let Ok(builder) = UPTransportZenoh::builder(local_authority) {
            builder
                .with_config(zenoh_config::Config::default())
                .build()
                .await
                .is_ok()
        } else {
            false
        }
    }

    #[cfg(feature = "zero-copy")]
    #[test]
    fn builder_sets_shm_segment_size() {
        let builder = UPTransportZenoh::builder("local_authority")
            .expect("valid authority")
            .with_config(zenoh_config::Config::default())
            .with_shm_segment_size(1024 * 1024);

        assert_eq!(builder.common.shm_segment_size, 1024 * 1024);
    }
}
