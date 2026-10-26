// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Setup-owned ordered dispatch for copied classic and owned-frame controls.

use std::{
    cell::RefCell,
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
    time::Instant,
};
use up_rust::{
    ListenerAdmission, OverflowPolicy, Registration, RegistrationConfig, RegistrationHandler,
    RegistrationInbox, UCode, UStatus,
};

thread_local! {
    static RESERVED: RefCell<Vec<Permit>> = const { RefCell::new(Vec::new()) };
}

struct CreditState {
    held: bool,
    closed: bool,
}
struct Credit {
    state: Mutex<CreditState>,
    changed: Condvar,
}
impl Credit {
    fn new() -> Self {
        Self {
            state: Mutex::new(CreditState {
                held: false,
                closed: false,
            }),
            changed: Condvar::new(),
        }
    }
    fn try_reserve(self: &Arc<Self>) -> Option<Permit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.held {
            return None;
        }
        state.held = true;
        Some(Permit(Arc::clone(self)))
    }
    fn receive(self: &Arc<Self>, deadline: Option<Instant>) -> Option<Permit> {
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return None;
        }
        if let Some(permit) = RESERVED.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.iter()
                .position(|permit| Arc::ptr_eq(&permit.0, self))
                .map(|index| slot.swap_remove(index))
        }) {
            return Some(permit);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.closed || deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                return None;
            }
            if !state.held {
                state.held = true;
                return Some(Permit(Arc::clone(self)));
            }
            state = if let Some(deadline) = deadline {
                self.changed
                    .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            } else {
                self.changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            };
        }
    }
    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.changed.notify_all();
    }
}
struct Permit(Arc<Credit>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .held = false;
        self.0.changed.notify_one();
    }
}

struct Target {
    session: String,
    filter: zenoh::key_expr::OwnedKeyExpr,
    credit: Arc<Credit>,
    activity: Arc<ListenerAdmission>,
}

#[derive(Default)]
struct Targets {
    entries: Vec<Weak<Target>>,
    publications: usize,
}

fn targets() -> &'static Mutex<Targets> {
    static TARGETS: OnceLock<Mutex<Targets>> = OnceLock::new();
    TARGETS.get_or_init(Mutex::default)
}

fn topology_changed() -> &'static tokio::sync::Notify {
    static CHANGED: OnceLock<tokio::sync::Notify> = OnceLock::new();
    CHANGED.get_or_init(tokio::sync::Notify::new)
}

pub(crate) struct Publication {
    permits: Vec<Permit>,
    protected: bool,
}

impl Drop for Publication {
    fn drop(&mut self) {
        if self.protected {
            targets().lock().unwrap().publications -= 1;
            topology_changed().notify_waiters();
        }
    }
}

/// Reserve all matching local copied-control destinations, including registrations
/// made by another core sharing this session. Detect the invoking callback across
/// all cores, rather than only the transport through which it publishes.
pub(crate) fn reserve_publication(
    session: &zenoh::Session,
    key: &str,
) -> Result<Publication, UStatus> {
    let key = zenoh::key_expr::keyexpr::new(key)
        .map_err(|error| UStatus::fail_with_code(UCode::InvalidArgument, error.to_string()))?;
    if crate::native_callback::would_reenter(session, key) {
        return Err(UStatus::fail_with_code(
            UCode::FailedPrecondition,
            "publication would reenter an active native sink",
        ));
    }
    let mut targets = targets().lock().unwrap();
    targets.entries.retain(|entry| entry.strong_count() != 0);
    let entries: Vec<_> = targets.entries.iter().filter_map(Weak::upgrade).collect();
    if !entries
        .iter()
        .any(|entry| entry.activity.is_current_callback())
    {
        return Ok(Publication {
            permits: Vec::new(),
            protected: false,
        });
    }
    let session = session.zid().to_string();
    let permits = entries
        .iter()
        .filter(|entry| {
            entry.session == session
                && entry.filter.intersects(key)
                && entry.activity.closed_at().is_none()
        })
        .map(|entry| {
            entry.credit.try_reserve().ok_or_else(|| {
                UStatus::fail_with_code(
                    UCode::ResourceExhausted,
                    "local callback dispatch is saturated",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    targets.publications += 1;
    Ok(Publication {
        permits,
        protected: true,
    })
}

/// Used only by allocating compatibility sends, never the prepared native path.
pub(crate) fn publish_reserved<R>(mut publication: Publication, publish: impl FnOnce() -> R) -> R {
    struct Restore(Vec<Permit>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RESERVED.with(|slot| {
                *slot.borrow_mut() = std::mem::take(&mut self.0);
            });
        }
    }
    let _restore =
        Restore(RESERVED.with(|slot| slot.replace(std::mem::take(&mut publication.permits))));
    publish()
}

struct Item<T> {
    value: T,
    permit: Permit,
}
struct Admitted<H> {
    handler: H,
    activity: Arc<ListenerAdmission>,
    // Keep callback-context discovery alive through self-unregister until this
    // worker finishes; no per-message clone or allocation is needed.
    _target: Arc<OnceLock<Arc<Target>>>,
}
impl<T: Send + 'static, H: RegistrationHandler<T>> RegistrationHandler<Item<T>> for Admitted<H> {
    async fn on_receive(&mut self, item: Item<T>) {
        let Item { value, permit } = item;
        // The copied item has left the one-slot binding inbox. Native samples
        // were released by its producer before this copied-control boundary.
        drop(permit);
        self.activity
            .dispatch(|| self.handler.on_receive(value))
            .await;
    }
}

pub(crate) fn runtime() -> Result<&'static tokio::runtime::Runtime, UStatus> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("zenoh-dispatch")
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| {
            UStatus::fail_with_code(UCode::Unavailable, format!("dispatch runtime: {error}"))
        })
}

pub(crate) struct Dispatch<T> {
    credit: Arc<Credit>,
    activity: Arc<ListenerAdmission>,
    inbox: Arc<RegistrationInbox<Item<T>>>,
    worker: Mutex<Option<Registration<Item<T>>>>,
    target: Arc<OnceLock<Arc<Target>>>,
}
impl<T: Send + 'static> Dispatch<T> {
    pub(crate) fn new<H: RegistrationHandler<T>>(handler: H) -> Result<Arc<Self>, UStatus> {
        let activity = Arc::new(ListenerAdmission::new());
        let target = Arc::new(OnceLock::new());
        let worker = {
            let _entered = runtime()?.enter();
            Registration::new(
                RegistrationConfig {
                    capacity: 1,
                    overflow: OverflowPolicy::DropNewest,
                },
                Admitted {
                    handler,
                    activity: Arc::clone(&activity),
                    _target: Arc::clone(&target),
                },
                || {},
            )
            .map_err(|error| UStatus::fail_with_code(UCode::ResourceExhausted, error.to_string()))?
        };
        Ok(Arc::new(Self {
            credit: Arc::new(Credit::new()),
            activity,
            inbox: worker.inbox(),
            worker: Mutex::new(Some(worker)),
            target,
        }))
    }

    pub(crate) async fn bind(&self, session: &zenoh::Session, key: &str) -> Result<(), UStatus> {
        let target = Arc::new(Target {
            session: session.zid().to_string(),
            filter: zenoh::key_expr::OwnedKeyExpr::try_from(key.to_owned()).map_err(|error| {
                UStatus::fail_with_code(UCode::InvalidArgument, error.to_string())
            })?,
            credit: Arc::clone(&self.credit),
            activity: Arc::clone(&self.activity),
        });
        loop {
            let changed = topology_changed().notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut targets = targets().lock().unwrap();
                if self.activity.closed_at().is_some() {
                    return Err(UStatus::fail_with_code(
                        UCode::Cancelled,
                        "dispatch closed during setup",
                    ));
                }
                if targets.publications == 0 {
                    self.target.set(Arc::clone(&target)).map_err(|_| {
                        UStatus::fail_with_code(UCode::AlreadyExists, "dispatch already bound")
                    })?;
                    targets.entries.retain(|entry| entry.strong_count() != 0);
                    targets.entries.push(Arc::downgrade(&target));
                    return Ok(());
                }
            }
            // A registration cannot become a new unreserved callback in the
            // middle of a protected native put. No map lock crosses this wait.
            changed.await;
        }
    }
    pub(crate) fn push(&self, value: T) -> bool {
        self.push_until(value, None)
    }
    pub(crate) fn push_until(&self, value: T, deadline: Option<Instant>) -> bool {
        let Some(permit) = self.credit.receive(deadline) else {
            return false;
        };
        self.inbox.push(Item { value, permit }).is_ok()
    }
    pub(crate) fn activity(&self) -> &Arc<ListenerAdmission> {
        &self.activity
    }
    pub(crate) fn close(&self) {
        self.activity.cancel();
        self.credit.close();
        self.inbox.close();
    }
    pub(crate) async fn stop(&self) -> Result<(), UStatus> {
        self.close();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            worker
                .stop()
                .await
                .map_err(|error| UStatus::fail_with_code(UCode::Internal, error.to_string()))?;
        }
        Ok(())
    }
}
impl<T> Drop for Dispatch<T> {
    fn drop(&mut self) {
        self.activity.cancel();
        self.credit.close();
        self.inbox.close();
    }
}
