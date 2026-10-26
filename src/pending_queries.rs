// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Classic query ownership from enqueue through response, cancellation or expiry.

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Notify, task::JoinHandle};
use up_rust::{ListenerAdmission, UAttributes, UCode, UMessage, UStatus, UUID};
use zenoh::query::Query;

pub(crate) fn request_deadline(request: &UAttributes) -> Result<Instant, UStatus> {
    let ttl = request.ttl().filter(|ttl| *ttl > 0).ok_or_else(|| {
        UStatus::fail_with_code(
            UCode::InvalidArgument,
            "query requires a positive request TTL",
        )
    })?;
    let until = request
        .id()
        .time()
        .checked_add(u64::from(ttl))
        .ok_or_else(|| {
            UStatus::fail_with_code(UCode::InvalidArgument, "query expiry is unrepresentable")
        })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| UStatus::fail_with_code(UCode::Internal, "system clock precedes the epoch"))?
        .as_millis();
    let remaining = u128::from(until)
        .checked_sub(now)
        .filter(|value| *value > 0)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| UStatus::fail_with_code(UCode::DeadlineExceeded, "query expired"))?;
    Instant::now()
        .checked_add(Duration::from_millis(remaining))
        .ok_or_else(|| {
            UStatus::fail_with_code(UCode::InvalidArgument, "query deadline is unrepresentable")
        })
}

struct Pending {
    query: Query,
    request: UAttributes,
    owner: Weak<ListenerAdmission>,
    deadline: Instant,
    generation: Arc<()>,
}

#[derive(Default)]
struct State {
    entries: Mutex<HashMap<UUID, Pending>>,
    changed: Arc<Notify>,
}

impl State {
    // Dropping a Query can enter middleware. Always move it out of the map lock
    // before releasing it, including expiry and cancellation paths.
    fn expire(&self) -> Option<Instant> {
        let now = Instant::now();
        let (expired, next) = {
            let mut entries = self.entries.lock().unwrap();
            let keys: Vec<_> = entries
                .iter()
                .filter(|(_, entry)| {
                    entry.deadline <= now
                        || entry
                            .owner
                            .upgrade()
                            .is_none_or(|owner| owner.closed_at().is_some())
                })
                .map(|(id, _)| id.clone())
                .collect();
            let expired: Vec<_> = keys.iter().filter_map(|id| entries.remove(id)).collect();
            (expired, entries.values().map(|entry| entry.deadline).min())
        };
        drop(expired);
        next
    }

    fn remove_generation(&self, id: &UUID, generation: &Arc<()>) {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            if entries
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.generation, generation))
            {
                entries.remove(id)
            } else {
                None
            }
        };
        drop(removed);
        self.changed.notify_one();
    }
}

/// One setup-owned expiry task for the allocating classic RPC path.
pub(crate) struct PendingQueries {
    state: Arc<State>,
    worker: JoinHandle<()>,
}

impl PendingQueries {
    pub(crate) fn new() -> Result<Arc<Self>, UStatus> {
        let state = Arc::new(State::default());
        let weak = Arc::downgrade(&state);
        let changed = Arc::clone(&state.changed);
        let worker = crate::dispatch::runtime()?.spawn(async move {
            loop {
                let Some(state) = weak.upgrade() else { return };
                let next = state.expire();
                drop(state);
                if let Some(next) = next {
                    tokio::select! {
                        () = changed.notified() => {},
                        () = tokio::time::sleep_until(next.into()) => {},
                    }
                } else {
                    changed.notified().await;
                }
            }
        });
        Ok(Arc::new(Self { state, worker }))
    }

    pub(crate) fn insert(
        &self,
        query: Query,
        request: &UAttributes,
        owner: &Arc<ListenerAdmission>,
    ) -> Result<QueryAdmission, UStatus> {
        let deadline = request_deadline(request)?;
        let id = request.id().clone();
        let generation = Arc::new(());
        let pending = Pending {
            query,
            request: request.clone(),
            owner: Arc::downgrade(owner),
            deadline,
            generation: Arc::clone(&generation),
        };
        // Declare pending outside the lock scope so refusal also drops its native
        // query after unlocking. A duplicate cannot replace the original query.
        {
            let mut entries = self.state.entries.lock().unwrap();
            if owner.closed_at().is_some() {
                return Err(UStatus::fail_with_code(
                    UCode::Cancelled,
                    "query listener closed",
                ));
            }
            match entries.entry(id.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(pending);
                }
                Entry::Occupied(_) => {
                    return Err(UStatus::fail_with_code(
                        UCode::AlreadyExists,
                        "request ID is already pending",
                    ))
                }
            }
        }
        self.state.changed.notify_one();
        Ok(QueryAdmission {
            state: Arc::downgrade(&self.state),
            id,
            generation,
            complete: false,
            deadline,
        })
    }

    pub(crate) fn take_response(&self, response: &UAttributes) -> Result<Option<Query>, UStatus> {
        let Some(id) = response.request_id() else {
            return Ok(None);
        };
        let removed = {
            let mut entries = self.state.entries.lock().unwrap();
            let Some(entry) = entries.get(id) else {
                return Ok(None);
            };
            if entry.request.sink() != Some(response.source())
                || response.sink() != Some(entry.request.source())
            {
                return Err(UStatus::fail_with_code(
                    UCode::InvalidArgument,
                    "response does not match request endpoints",
                ));
            }
            entries
                .remove(id)
                .expect("entry checked under the same lock")
        };
        self.state.changed.notify_one();
        if removed.deadline <= Instant::now()
            || removed
                .owner
                .upgrade()
                .is_none_or(|owner| owner.closed_at().is_some())
        {
            return Err(UStatus::fail_with_code(
                UCode::DeadlineExceeded,
                "query is no longer live",
            ));
        }
        Ok(Some(removed.query))
    }

    pub(crate) fn remove_owner(&self, owner: &Arc<ListenerAdmission>) {
        let removed: Vec<_> = {
            let mut entries = self.state.entries.lock().unwrap();
            let keys: Vec<_> = entries
                .iter()
                .filter(|(_, entry)| entry.owner.ptr_eq(&Arc::downgrade(owner)))
                .map(|(id, _)| id.clone())
                .collect();
            keys.iter().filter_map(|id| entries.remove(id)).collect()
        };
        drop(removed);
        self.state.changed.notify_one();
    }
}

impl Drop for PendingQueries {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

pub(crate) struct QueryAdmission {
    state: Weak<State>,
    id: UUID,
    generation: Arc<()>,
    complete: bool,
    deadline: Instant,
}

impl QueryAdmission {
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    fn live(&self) -> bool {
        self.state.upgrade().is_some_and(|state| {
            state
                .entries
                .lock()
                .unwrap()
                .get(&self.id)
                .is_some_and(|entry| {
                    Arc::ptr_eq(&entry.generation, &self.generation)
                        && entry.deadline > Instant::now()
                })
        })
    }
}

impl Drop for QueryAdmission {
    fn drop(&mut self) {
        if !self.complete {
            if let Some(state) = self.state.upgrade() {
                state.remove_generation(&self.id, &self.generation);
            }
        }
    }
}

pub(crate) struct MessageDelivery {
    pub(crate) message: UMessage,
    pub(crate) query: Option<QueryAdmission>,
}

impl MessageDelivery {
    pub(crate) async fn deliver(mut self, listener: &dyn up_rust::UListener) {
        if self.message.attributes().check_expired().is_err()
            || self.query.as_ref().is_some_and(|query| !query.live())
        {
            return;
        }
        listener.on_receive(self.message).await;
        // A returned listener may have delegated its reply. Keep that correlation
        // until response/expiry, but cancellation during the callback rolls back.
        if let Some(query) = self.query.as_mut() {
            query.complete = true;
        }
    }
}

impl From<UMessage> for MessageDelivery {
    fn from(message: UMessage) -> Self {
        Self {
            message,
            query: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{poll_fn, Future},
        task::Poll,
    };
    use up_rust::{UListener, UMessageBuilder, UTransport, UUri};
    use zenoh::{
        handlers::FifoChannelHandler,
        query::{Queryable, Reply},
        Session,
    };

    struct Native {
        session: Session,
        queries: Queryable<FifoChannelHandler<Query>>,
    }

    impl Native {
        async fn new() -> Self {
            let mut config = zenoh::Config::default();
            config.insert_json5("listen/endpoints", "[]").unwrap();
            config
                .insert_json5("scouting/multicast/enabled", "false")
                .unwrap();
            let session = zenoh::open(config).await.unwrap();
            let queries = session
                .declare_queryable("pending/query")
                .with(zenoh::handlers::FifoChannel::new(1))
                .await
                .unwrap();
            Self { session, queries }
        }

        async fn query(&self) -> (Query, FifoChannelHandler<Reply>) {
            let replies = self.session.get("pending/query").await.unwrap();
            (self.queries.recv_async().await.unwrap(), replies)
        }
    }

    fn request(ttl: u32) -> UMessage {
        UMessageBuilder::request(
            UUri::try_from("//queries/2/1/1").unwrap(),
            UUri::try_from("//queries/1/1/0").unwrap(),
            ttl,
        )
        .build()
        .unwrap()
    }

    async fn wait_empty(table: &PendingQueries) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if table.state.entries.lock().unwrap().is_empty() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn expiry_releases_native_query_without_later_traffic() {
        let native = Native::new().await;
        let table = PendingQueries::new().unwrap();
        let owner = Arc::new(ListenerAdmission::new());
        let (query, replies) = native.query().await;
        let message = request(100);
        let ticket = table.insert(query, message.attributes(), &owner).unwrap();
        assert!(ticket.live());
        wait_empty(&table).await;
        assert!(!ticket.live());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), replies.recv_async())
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn duplicate_and_stale_ticket_preserve_the_live_generation() {
        let native = Native::new().await;
        let table = PendingQueries::new().unwrap();
        let owner = Arc::new(ListenerAdmission::new());
        let message = request(5000);
        let (query, _reply1) = native.query().await;
        let old = table.insert(query, message.attributes(), &owner).unwrap();
        let (query, _reply2) = native.query().await;
        assert!(matches!(table.insert(query, message.attributes(), &owner),
            Err(error) if error.code() == UCode::AlreadyExists));
        assert!(old.live());
        let response = UMessageBuilder::response_for_request(message.attributes())
            .build()
            .unwrap();
        let original = table.take_response(response.attributes()).unwrap().unwrap();
        original.reply("pending/query", "first").await.unwrap();
        drop(original);
        let (query, _reply3) = native.query().await;
        let new = table.insert(query, message.attributes(), &owner).unwrap();
        drop(old);
        assert!(new.live());
        drop(new);
        assert!(table.state.entries.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn failed_enqueue_and_owner_retirement_clear_only_their_queries() {
        let native = Native::new().await;
        let table = PendingQueries::new().unwrap();
        let owner = Arc::new(ListenerAdmission::new());
        let other = Arc::new(ListenerAdmission::new());
        let (query, _replies) = native.query().await;
        let message = request(5000);
        let ticket = table.insert(query, message.attributes(), &owner).unwrap();
        let inbox = up_rust::RegistrationInbox::new(up_rust::RegistrationConfig {
            capacity: 1,
            overflow: up_rust::OverflowPolicy::DropNewest,
        })
        .unwrap();
        inbox.close();
        assert!(inbox
            .push(MessageDelivery {
                message,
                query: Some(ticket)
            })
            .is_err());
        assert!(table.state.entries.lock().unwrap().is_empty());

        let (query, _replies) = native.query().await;
        let own_ticket = table
            .insert(query, request(5000).attributes(), &owner)
            .unwrap();
        let (query, _replies) = native.query().await;
        let other_ticket = table
            .insert(query, request(5000).attributes(), &other)
            .unwrap();
        owner.cancel();
        table.remove_owner(&owner);
        assert!(!own_ticket.live());
        assert!(other_ticket.live());
        let (query, _replies) = native.query().await;
        assert!(
            matches!(table.insert(query, request(5000).attributes(), &owner),
            Err(error) if error.code() == UCode::Cancelled)
        );
    }

    struct PendingListener(Arc<Notify>);
    #[async_trait::async_trait]
    impl UListener for PendingListener {
        async fn on_receive(&self, _: UMessage) {
            self.0.notify_one();
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cancelled_listener_rolls_back_its_entered_query() {
        let native = Native::new().await;
        let table = PendingQueries::new().unwrap();
        let owner = Arc::new(ListenerAdmission::new());
        let (query, _replies) = native.query().await;
        let message = request(5000);
        let ticket = table.insert(query, message.attributes(), &owner).unwrap();
        let entered = Arc::new(Notify::new());
        let listener = PendingListener(Arc::clone(&entered));
        let worker = tokio::spawn(async move {
            MessageDelivery {
                message,
                query: Some(ticket),
            }
            .deliver(&listener)
            .await;
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(table.state.entries.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn saturated_native_query_admission_ends_at_its_original_ttl() {
        let native = Native::new().await;
        let transport = crate::UPTransportZenoh::builder("queries")
            .unwrap()
            .with_session(native.session.clone())
            .build()
            .unwrap();
        let first = request(5000);
        let source = first.attributes().source().clone();
        let sink = first.attributes().sink().cloned();
        let key = transport.to_zenoh_key_string(&source, sink.as_ref());
        let entered = Arc::new(Notify::new());
        let listener: Arc<dyn UListener> = Arc::new(PendingListener(Arc::clone(&entered)));
        transport
            .register_listener(&source, sink.as_ref(), Arc::clone(&listener))
            .await
            .unwrap();
        let send = |message: &UMessage| {
            native.session.get(&key).attachment(
                crate::UPTransportZenoh::uattributes_to_attachment(message.attributes()).unwrap(),
            )
        };
        let _first = send(&first).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let _queued = send(&request(5000)).await.unwrap();
        let expiring = request(100);
        let start = Instant::now();
        let _expired = send(&expiring).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!transport
            .query_map
            .state
            .entries
            .lock()
            .unwrap()
            .contains_key(expiring.attributes().id()));
        let mut stopping =
            Box::pin(transport.unregister_listener(&source, sink.as_ref(), listener));
        assert!(poll_fn(|cx| Poll::Ready(stopping.as_mut().poll(cx)))
            .await
            .is_pending());
        drop(stopping);
        assert!(transport.query_map.state.entries.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn mismatched_response_does_not_consume_valid_correlation() {
        let native = Native::new().await;
        let table = PendingQueries::new().unwrap();
        let owner = Arc::new(ListenerAdmission::new());
        let (query, replies) = native.query().await;
        let message = request(5000);
        let ticket = table.insert(query, message.attributes(), &owner).unwrap();
        let wrong = UMessageBuilder::response(
            UUri::try_from("//other/1/1/0").unwrap(),
            message.attributes().id().clone(),
            message.attributes().sink().unwrap().clone(),
        )
        .build()
        .unwrap();
        assert!(matches!(table.take_response(wrong.attributes()),
            Err(error) if error.code() == UCode::InvalidArgument));
        assert!(ticket.live());
        let correct = UMessageBuilder::response_for_request(message.attributes())
            .build()
            .unwrap();
        let query = table.take_response(correct.attributes()).unwrap().unwrap();
        query.reply("pending/query", "valid").await.unwrap();
        drop(query);
        let reply = tokio::time::timeout(Duration::from_secs(2), replies.recv_async())
            .await
            .unwrap()
            .unwrap()
            .into_result()
            .unwrap();
        assert_eq!(reply.payload().to_bytes().as_ref(), b"valid");
        assert!(table.take_response(correct.attributes()).unwrap().is_none());
    }
}
