// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Setup-owned native callback identities for prepublication synchronous reentry checks.

#[cfg(feature = "zero-copy")]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock, Weak};
use zenoh::{
    key_expr::{keyexpr, OwnedKeyExpr},
    session::ZenohId,
    Session,
};

fn callbacks() -> &'static Mutex<Vec<Weak<NativeCallback>>> {
    static CALLBACKS: OnceLock<Mutex<Vec<Weak<NativeCallback>>>> = OnceLock::new();
    CALLBACKS.get_or_init(Mutex::default)
}

pub(crate) struct NativeCallback {
    session: ZenohId,
    filter: OwnedKeyExpr,
    // Zenoh's mutable callback serializes entry for this native registration.
    // Only the short identity update holds this lock, never delivery or a wait.
    thread: Mutex<Option<std::thread::ThreadId>>,
}

impl NativeCallback {
    #[cfg(feature = "zero-copy")]
    pub(crate) fn new(session: &Session, filter: &str) -> Result<Arc<Self>, ()> {
        let filter = OwnedKeyExpr::try_from(filter.to_owned()).map_err(|_| ())?;
        let callback = Arc::new(Self {
            session: session.zid(),
            filter,
            thread: Mutex::new(None),
        });
        let mut callbacks = callbacks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        callbacks.retain(|entry| entry.strong_count() != 0);
        callbacks.push(Arc::downgrade(&callback));
        Ok(callback)
    }

    #[cfg(feature = "zero-copy")]
    pub(crate) fn enter(&self) -> NativeCallbackGuard<'_> {
        *self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::thread::current().id());
        NativeCallbackGuard(self)
    }
}

#[cfg(feature = "zero-copy")]
pub(crate) struct NativeCallbackGuard<'a>(&'a NativeCallback);
#[cfg(feature = "zero-copy")]
impl Drop for NativeCallbackGuard<'_> {
    fn drop(&mut self) {
        *self
            .0
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Checks every live registration, including an ancestor of a nested native
/// callback and another core sharing this session. No per-publication allocation,
/// callback TLS, or lock held across native publication is needed.
pub(crate) fn would_reenter(session: &Session, key: &keyexpr) -> bool {
    let session = session.zid();
    let current = std::thread::current().id();
    callbacks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .any(|callback| {
            callback.session == session
                && callback.filter.intersects(key)
                && *callback
                    .thread
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    == Some(current)
        })
}
