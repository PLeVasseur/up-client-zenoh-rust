// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0

use std::{
    future::{poll_fn, Future},
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};
use tokio::sync::Mutex;

pub(crate) struct ListenerActivity {
    active: AtomicBool,
    entry: Mutex<()>,
}

impl ListenerActivity {
    pub(crate) fn new() -> Self {
        Self {
            active: AtomicBool::new(true),
            entry: Mutex::new(()),
        }
    }

    pub(crate) async fn stop(&self) {
        let _entry = self.entry.lock().await;
        self.active.store(false, Ordering::Release);
    }

    pub(crate) async fn dispatch<F: Future<Output = ()>>(&self, callback: impl FnOnce() -> F) {
        let entry = self.entry.lock().await;
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let mut callback = std::pin::pin!(callback());
        // Future construction alone is not callback entry. Serialize its first
        // poll with unregister, releasing at the first yield for self-unregister.
        let pending = poll_fn(|cx| Poll::Ready(callback.as_mut().poll(cx).is_pending())).await;
        drop(entry);
        if pending {
            callback.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn queued_task_cannot_enter_after_unregister() {
        let activity = Arc::new(ListenerActivity::new());
        let ready = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let activity = Arc::clone(&activity);
            let ready = Arc::clone(&ready);
            async move {
                ready.notified().await;
                activity
                    .dispatch(|| async { panic!("queued callback entered after unregister") })
                    .await;
            }
        });
        activity.stop().await;
        ready.notify_one();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn callback_can_unregister_itself() {
        let activity = ListenerActivity::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            activity.dispatch(|| async {
                activity.stop().await;
            }),
        )
        .await
        .expect("self-unregister must not deadlock");
    }
}
