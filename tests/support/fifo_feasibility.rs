// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Test-only reservation candidate for inline, copied-value callback dispatch.

use std::{
    cell::RefCell,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

thread_local! {
    static RESERVED: RefCell<Option<Permit>> = const { RefCell::new(None) };
}

/// One creation-owned queue credit, including an unpublished reserved item.
#[derive(Default)]
pub struct Credit {
    held: Mutex<bool>,
    released: Condvar,
}
/// A reserved queue slot; cancellation/drop releases it without an item.
pub struct Permit(Arc<Credit>);
impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.held.lock().unwrap() = false;
        self.0.released.notify_one();
    }
}
impl Credit {
    /// Refuses saturated callback-originated publication before entering Zenoh.
    pub fn try_reserve(self: &Arc<Self>) -> Option<Permit> {
        let mut held = self.held.lock().unwrap();
        if *held {
            return None;
        }
        *held = true;
        Some(Permit(Arc::clone(self)))
    }
    /// Receives an inline reservation or waits for an ordinary producer's credit.
    pub fn receive(self: &Arc<Self>) -> Permit {
        if let Some(permit) = RESERVED.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|permit| Arc::ptr_eq(&permit.0, self))
            {
                slot.take()
            } else {
                None
            }
        }) {
            return permit;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut held = self.held.lock().unwrap();
        while *held {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("candidate credit timeout");
            held = self.released.wait_timeout(held, remaining).unwrap().0;
        }
        *held = true;
        Permit(Arc::clone(self))
    }
}

/// Makes a reservation visible only during one synchronous native publication.
/// No thread-local state crosses an await; nested calls restore their predecessor.
pub fn publish_reserved<R>(permit: Permit, publish: impl FnOnce() -> R) -> R {
    struct Restore(Option<Permit>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RESERVED.with(|slot| {
                *slot.borrow_mut() = self.0.take();
            });
        }
    }
    let _restore = Restore(RESERVED.with(|slot| slot.replace(Some(permit))));
    publish()
}

/// A copied control value carrying the one reserved queue credit until dequeue.
pub struct Item {
    /// The test message sequence.
    pub value: u8,
    /// Credit retained across native callback return and SDK queue admission.
    pub permit: Permit,
}
