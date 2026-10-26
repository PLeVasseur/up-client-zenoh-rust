// Copyright (c) 2026 Contributors to the Eclipse Foundation
// SPDX-License-Identifier: Apache-2.0
//! Harness-free binding-scoped and whole-process Rust heap observations.

#[path = "support/prepared_native.rs"]
mod common;

use common::Value;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use up_rust::{
    Frame, FrameSink, HeaderLimits, InitializedStablePayload, LoanError, MessageFields,
    PreparedRpcClient, PreparedRpcServer, PreparedTransportCore, RequestHandler, RouteConfig,
    RpcClientConfig, RpcServerConfig, RxAdmission, RxLease, SendError, StablePayloadInit,
    TokenError, TxTemplate, UCode, UPriority, UUri, WireRegistration, ZeroCopyHandler, UUID,
};
use up_transport_zenoh::{
    ZenohAllocationObserver, ZenohPreparedCore, ZenohPreparedRx, ZenohRouteReservation,
};
use zenoh::{shm::ShmProviderBuilder, Wait};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PROCESS_CALLS: AtomicUsize = AtomicUsize::new(0);
static PROCESS_BYTES: AtomicUsize = AtomicUsize::new(0);
static OWN_CALLS: AtomicUsize = AtomicUsize::new(0);
static OWN_BYTES: AtomicUsize = AtomicUsize::new(0);
thread_local! { static OWN_SCOPE: Cell<bool> = const { Cell::new(false) }; }

struct Observer;
static OBSERVER: Observer = Observer;
impl ZenohAllocationObserver for Observer {
    fn replace_scope(&self, counted: bool) -> bool {
        OWN_SCOPE.with(|scope| scope.replace(counted))
    }
}
struct Counter;
fn charge(bytes: usize) {
    if ACTIVE.load(Ordering::SeqCst) {
        PROCESS_CALLS.fetch_add(1, Ordering::Relaxed);
        PROCESS_BYTES.fetch_add(bytes, Ordering::Relaxed);
        if OWN_SCOPE.with(Cell::get) {
            OWN_CALLS.fetch_add(1, Ordering::Relaxed);
            OWN_BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    }
}
// SAFETY: each operation forwards the allocator's original pointer/layout contract
// to System unchanged. Observation uses const TLS and static atomics on all threads.
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: forward the original valid allocation layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        // SAFETY: forward the original valid allocation layout.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        charge(size);
        // SAFETY: forward the live pointer, original layout and requested size.
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forward ownership of the live allocation exactly once.
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counter = Counter;

fn observe(name: &str, messages: u32, operation: impl FnOnce()) -> (usize, usize, usize, usize) {
    for counter in [&PROCESS_CALLS, &PROCESS_BYTES, &OWN_CALLS, &OWN_BYTES] {
        counter.store(0, Ordering::SeqCst);
    }
    assert!(!ACTIVE.swap(true, Ordering::SeqCst));
    operation();
    ACTIVE.store(false, Ordering::SeqCst);
    let own_calls = OWN_CALLS.load(Ordering::SeqCst);
    let own_bytes = OWN_BYTES.load(Ordering::SeqCst);
    let process_calls = PROCESS_CALLS.load(Ordering::SeqCst);
    let process_bytes = PROCESS_BYTES.load(Ordering::SeqCst);
    println!("subject={name}\tmessages={messages}\tbinding_allocations={own_calls}\tbinding_allocated_bytes={own_bytes}\trust_process_allocations={process_calls}\trust_process_allocated_bytes={process_bytes}\tall_threads=true\tcallback_scope_reentry=true\tobserver_subtracted=false");
    (own_calls, own_bytes, process_calls, process_bytes)
}
fn zero(name: &str, messages: u32, operation: impl FnOnce()) {
    let observed = observe(name, messages, operation);
    assert_eq!(
        (observed.0, observed.1),
        (0, 0),
        "binding allocation contract failed for {name}"
    );
}
fn wait(counter: &AtomicUsize, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::Acquire) != expected {
        assert!(
            Instant::now() < deadline,
            "native observation did not complete"
        );
        std::thread::yield_now();
    }
}
struct Observe {
    count: Arc<AtomicUsize>,
    sequence: u32,
    tokens: bool,
}
impl ZeroCopyHandler<Value, ZenohPreparedRx> for Observe {
    async fn on_receive(&mut self, lease: RxLease<Value, ZenohPreparedRx>) {
        let value = lease.payload().unwrap();
        assert_eq!(value.sequence, self.sequence);
        assert!(value
            .bytes
            .iter()
            .enumerate()
            .all(|(i, byte)| usize::from(*byte) == i));
        let header = lease.header().unwrap();
        assert_eq!(header.token().map(str::len), self.tokens.then_some(1536));
        assert_eq!(header.traceparent(), self.tokens.then_some(common::TRACE));
        drop(lease);
        self.sequence += 1;
        self.count.fetch_add(1, Ordering::Release);
    }
}
struct AllocatingCallback {
    count: Arc<AtomicUsize>,
    inline: Arc<AtomicBool>,
    caller: std::thread::ThreadId,
}
impl FrameSink<ZenohPreparedRx> for AllocatingCallback {
    fn push(&self, frame: ZenohPreparedRx) {
        assert!(
            OWN_SCOPE.with(Cell::get),
            "native callback did not re-enter the binding scope"
        );
        self.inline.store(
            std::thread::current().id() == self.caller,
            Ordering::Release,
        );
        assert!(frame.sample().payload().as_shm().is_some());
        let memory = std::hint::black_box(Box::new([0u8; 64]));
        std::hint::black_box(&memory);
        drop(memory);
        drop(frame);
        self.count.fetch_add(1, Ordering::Release);
    }
    fn close(&self) {}
}

struct ReplyHandler;
impl RequestHandler<Value, Value, ZenohPreparedRx> for ReplyHandler {
    type Reply = u32;
    async fn handle(&mut self, request: RxLease<Value, ZenohPreparedRx>) -> Result<u32, UCode> {
        let sequence = request.payload().unwrap().sequence;
        if let Some(token) = request.header().unwrap().token() {
            assert!(matches!(token.len(), 64 | 1536));
        }
        drop(request);
        tokio::task::yield_now().await;
        if sequence == u32::MAX {
            Err(UCode::PermissionDenied)
        } else {
            Ok(sequence + 1)
        }
    }
    fn write_reply<'a>(
        &mut self,
        sequence: u32,
        init: <Value as StablePayloadInit>::Initializer<'a>,
    ) -> InitializedStablePayload<'a, Value> {
        init.sequence(sequence)
            .bytes_fill_with(|i| u8::try_from(i).unwrap())
            .finish()
    }
}
fn response_accounting(server: &PreparedRpcServer<ZenohPreparedCore, Value>, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.stats().responded.load(Ordering::Acquire) != expected {
        assert!(
            Instant::now() < deadline,
            "native RPC accounting did not complete"
        );
        std::thread::yield_now();
    }
}
#[allow(
    clippy::too_many_lines,
    reason = "Keep setup, first-client admission and ordered measured RPC windows together."
)]
fn rpc(runtime: &tokio::runtime::Runtime) {
    let method = UUri::try_from_parts("rpc-service", 0x4210, 1, 0x1000).unwrap();
    let replies: Vec<_> = (0..9)
        .map(|id| UUri::try_from_parts("rpc-client", id + 1, 1, 0).unwrap())
        .collect();
    let mut catalog = Vec::new();
    for reply in &replies {
        catalog.push(ZenohRouteReservation {
            source: reply.clone(),
            sink: Some(method.clone()),
            limits: common::limits(68),
            transmit: true,
        });
        catalog.push(ZenohRouteReservation {
            source: method.clone(),
            sink: Some(reply.clone()),
            limits: common::limits(68),
            transmit: true,
        });
    }
    let (client_core, server_core, _) =
        runtime.block_on(common::catalog_pair(&catalog, 10, &OBSERVER));
    let agreement = common::agreement();
    let server = runtime
        .block_on(PreparedRpcServer::<_, Value>::start::<Value, _>(
            Arc::clone(&server_core),
            agreement.clone(),
            method.clone(),
            ReplyHandler,
            RpcServerConfig {
                registration: common::registration(),
                header: HeaderLimits::default(),
                max_clients: 8,
            },
        ))
        .unwrap();
    let clients: Vec<_> = replies
        .iter()
        .map(|reply| {
            runtime
                .block_on(PreparedRpcClient::<_, Value, Value>::new(
                    Arc::clone(&client_core),
                    agreement.clone(),
                    reply.clone(),
                    method.clone(),
                    RpcClientConfig {
                        max_in_flight: 1,
                        ttl: Duration::from_millis(100),
                        max_token_len: 2048,
                        priority: UPriority::CS4,
                    },
                ))
                .unwrap()
        })
        .collect();
    for reply in &replies {
        runtime
            .block_on(client_core.wait_for_matching(reply, Some(&method), Duration::from_secs(5)))
            .unwrap();
        runtime
            .block_on(server_core.wait_for_matching(&method, Some(reply), Duration::from_secs(5)))
            .unwrap();
    }
    let foreign = TxTemplate::<common::Foreign>::new(agreement)
        .unwrap()
        .route(
            Arc::clone(&client_core),
            up_rust::RouteDefinition {
                kind: up_rust::UMessageType::Request,
                source: catalog[0].source.clone(),
                sink: Some(method),
                priority: Some(UPriority::CS4),
                ttl: Some(Duration::from_secs(1)),
                permission_level: None,
            },
            RouteConfig::default(),
        )
        .unwrap();
    // Prime native paths/worker delivery with a rejected equal-size type, without
    // calling clients or admitting a single server response-cache entry.
    for sequence in 0..100 {
        foreign
            .send(MessageFields::default(), |init| {
                init.into_initializer()
                    .sequence(sequence)
                    .bytes_fill(0)
                    .finish()
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(server.stats().responded.load(Ordering::Acquire), 0);
    let token_long = "l".repeat(1536);
    let token_short = "s".repeat(64);
    let oversized = "x".repeat(2049);
    let mut total = 0;
    zero("native_rpc_first_contact_8_new_clients", 800, || {
        runtime.block_on(async {
            for client in clients.iter().take(8) {
                for sequence in 0..100 {
                    let reply = client
                        .call(|init| {
                            init.into_initializer()
                                .sequence(sequence)
                                .bytes_fill(0)
                                .finish()
                        })
                        .await
                        .unwrap();
                    assert_eq!(reply.payload().unwrap().sequence, sequence + 1);
                    assert!(reply.header().unwrap().reqid.is_some());
                    drop(reply);
                    total += 1;
                }
            }
        });
        response_accounting(&server, total);
    });
    zero("native_rpc_rotating_token", 5000, || {
        runtime.block_on(async {
            for sequence in 0..5000 {
                clients[0]
                    .set_token(if sequence % 2 == 0 {
                        &token_long
                    } else {
                        &token_short
                    })
                    .unwrap();
                let reply = clients[0]
                    .call(|init| {
                        init.into_initializer()
                            .sequence(sequence)
                            .bytes_fill(0)
                            .finish()
                    })
                    .await
                    .unwrap();
                assert_eq!(reply.payload().unwrap().sequence, sequence + 1);
                drop(reply);
                total += 1;
            }
        });
        response_accounting(&server, total);
    });
    zero("native_rpc_status_only_response", 1, || {
        let status = runtime.block_on(clients[0].call(|init| {
            init.into_initializer()
                .sequence(u32::MAX)
                .bytes_fill(0)
                .finish()
        }));
        assert!(matches!(
            status,
            Err(up_rust::CallError::Status(UCode::PermissionDenied))
        ));
        total += 1;
        response_accounting(&server, total);
    });
    zero("native_rpc_client_refusal_and_timeout", 1, || {
        let result = runtime.block_on(
            clients[8].call(|init| init.into_initializer().sequence(0).bytes_fill(0).finish()),
        );
        assert!(matches!(result, Err(up_rust::CallError::TimedOut)));
        assert_eq!(server.stats().refused.load(Ordering::Acquire), 1);
        assert_eq!(clients[8].in_flight(), 0);
    });
    zero("native_rpc_token_rejection", 1000, || {
        for _ in 0..1000 {
            assert!(matches!(
                clients[0].set_token(&oversized),
                Err(TokenError::TooLong)
            ));
        }
    });
    drop(clients);
    runtime.block_on(server.stop()).unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep setup/warmup and every chronological native allocation window explicit."
)]
fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    let messages: u32 = match arguments.as_slice() {
        [_] => 50_000,
        [_, flag, value] if flag == "--messages" => value.parse().unwrap(),
        _ => panic!("expected --messages COUNT or no arguments"),
    };
    assert!(messages > 0 && messages < u32::MAX - 1000);
    let positive = observe("heap_scope_positive_control", 1, || {
        let previous = OBSERVER.replace_scope(true);
        let memory = std::hint::black_box(Box::new([0u8; 64]));
        std::hint::black_box(&memory);
        drop(memory);
        OBSERVER.replace_scope(previous);
    });
    assert!(positive.0 > 0 && positive.1 >= 64 && positive.2 > 0 && positive.3 >= 64);
    let started = Arc::new(AtomicUsize::new(0));
    let threads = Arc::clone(&started);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(move || {
            std::hint::black_box(UUID::build());
            threads.fetch_add(1, Ordering::Release);
        })
        .enable_all()
        .build()
        .unwrap();
    let _runtime_context = runtime.enter();
    wait(&started, 2);
    std::hint::black_box(UUID::build());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let warmers: Vec<_> = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            runtime.spawn(async move {
                barrier.wait();
                tokio::task::yield_now().await;
            })
        })
        .collect();
    for warmer in warmers {
        runtime.block_on(warmer).unwrap();
    }
    for tokens in [false, true] {
        let catalog = [ZenohRouteReservation {
            source: common::source(),
            sink: None,
            limits: common::limits(68),
            transmit: true,
        }];
        let (sender, receiver, raw_session) =
            runtime.block_on(common::catalog_pair(&catalog, 3, &OBSERVER));
        let admission =
            RxAdmission::<Value>::new(common::agreement(), HeaderLimits::default()).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let worker = admission
            .register_filtered(
                common::source(),
                None,
                common::registration(),
                Observe {
                    count: Arc::clone(&count),
                    sequence: 0,
                    tokens,
                },
                || {},
            )
            .unwrap();
        let subscription = runtime
            .block_on(WireRegistration::attach(
                Arc::clone(&receiver),
                &common::source(),
                None,
                worker,
            ))
            .unwrap();
        runtime
            .block_on(sender.wait_for_matching(&common::source(), None, Duration::from_secs(5)))
            .unwrap();
        let route = TxTemplate::<Value>::new(common::agreement())
            .unwrap()
            .route(
                Arc::clone(&sender),
                common::definition(common::source()),
                RouteConfig {
                    max_token_len: 2048,
                    allow_traceparent: true,
                },
            )
            .unwrap();
        let token = "t".repeat(1536);
        let fields = MessageFields {
            token: tokens.then_some(token.as_str()),
            traceparent: tokens.then_some(common::TRACE),
            ..MessageFields::default()
        };
        let send = |sequence| {
            route
                .send(fields, |init| {
                    init.into_initializer()
                        .sequence(sequence)
                        .bytes_fill_with(|i| u8::try_from(i).unwrap())
                        .finish()
                })
                .unwrap();
            wait(&count, usize::try_from(sequence).unwrap() + 1);
        };
        for sequence in 0..500 {
            send(sequence);
        }
        zero(
            if tokens {
                "native_token_traceparent_send_receive"
            } else {
                "native_send_receive"
            },
            messages,
            || {
                for sequence in 500..500 + messages {
                    send(sequence);
                }
            },
        );
        let oversized = "t".repeat(2049);
        zero("token_rejection", 1, || {
            assert_eq!(
                route.send(
                    MessageFields {
                        token: Some(&oversized),
                        ..MessageFields::default()
                    },
                    |init| init.into_initializer().sequence(0).bytes_fill(0).finish()
                ),
                Err(SendError::TokenTooLong)
            );
        });
        let physical = sender
            .prepare_route(&common::source(), None, common::limits(68))
            .unwrap();
        zero("native_layout_rejection", 1, || {
            assert!(matches!(
                sender.loan(
                    &physical,
                    up_rust::TxFrameLayout {
                        metadata_len: 3073,
                        payload_len: Some(68),
                        payload_alignment: 8
                    }
                ),
                Err(LoanError::SlotTooSmall)
            ));
        });
        zero("native_idle", 0, || {
            std::thread::sleep(Duration::from_millis(100));
        });

        let inbox = Arc::new(
            up_rust::RegistrationInbox::<ZenohPreparedRx>::new(common::registration()).unwrap(),
        );
        let mut retained_subscription = runtime
            .block_on(receiver.attach(&common::source(), None, inbox.clone()))
            .unwrap();
        let mut retained = Vec::with_capacity(4);
        for sequence in 500 + messages..504 + messages {
            send(sequence);
            retained.push(runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), inbox.recv())
                    .await
                    .unwrap()
                    .unwrap()
            }));
        }
        zero("native_retained_sample_lifetime", 0, || {
            for frame in &retained {
                assert!(frame.sample().payload().as_shm().is_some());
                assert!(Frame::payload(frame).is_some());
            }
        });
        // Use an equally sized fresh native provider for physical exhaustion.
        // Earlier network publications can release middleware-retained chunks
        // asynchronously; an unpublished-only pool has no such reclaim race.
        let pressure = runtime
            .block_on(ZenohPreparedCore::from_session(
                raw_session.clone(),
                "//prepared/4210/1/0",
                &catalog,
                up_transport_zenoh::ZenohPreparedConfig {
                    shm_segment_size: 1024 * 1024,
                    route_capacity: 16,
                    registration_capacity: 1,
                    allocation_observer: Some(&OBSERVER),
                },
            ))
            .unwrap();
        let physical = pressure
            .prepare_route(&common::source(), None, common::limits(68))
            .unwrap();
        let mut loans = Vec::with_capacity(4096);
        let layout = up_rust::TxFrameLayout {
            metadata_len: 3072,
            payload_len: Some(68),
            payload_alignment: 8,
        };
        loop {
            match pressure.loan(&physical, layout) {
                Ok(loan) => {
                    assert!(
                        loans.len() < 4096,
                        "native pool did not exhaust within reserved test handle capacity"
                    );
                    loans.push(loan);
                }
                Err(LoanError::PoolExhausted) => break,
                Err(error) => panic!("unexpected native loan failure: {error}"),
            }
        }
        zero("native_physical_pool_exhaustion", 1, || {
            assert!(matches!(
                pressure.loan(&physical, layout),
                Err(LoanError::PoolExhausted)
            ));
        });
        drop(loans);
        let key = retained[0].sample().key_expr().to_string();
        println!("native_negative_target_key={key}");
        receiver.detach(&mut retained_subscription).unwrap();
        drop(retained);

        let raw_publisher = raw_session.declare_publisher(key).wait().unwrap();
        let matching_deadline = Instant::now() + Duration::from_secs(5);
        while !raw_publisher.matching_status().wait().unwrap().matching() {
            assert!(
                Instant::now() < matching_deadline,
                "raw negative publisher did not match the native receiver"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let native_provider = ShmProviderBuilder::default_backend(4096).wait().unwrap();
        let mut malformed = native_provider.alloc(64).wait().unwrap();
        malformed.as_mut().fill(0);
        let malformed = zenoh::bytes::ZBytes::from(malformed);
        let non_shm = zenoh::bytes::ZBytes::from(vec![0u8; 64]);
        raw_publisher.put(non_shm.clone()).wait().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        zero("native_non_shm_rejection", 1, || {
            let before = receiver.stats().rejected_non_shm.load(Ordering::Acquire);
            raw_publisher.put(non_shm.clone()).wait().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while receiver.stats().rejected_non_shm.load(Ordering::Acquire) == before {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        });
        zero("native_malformed_shm_rejection", 1, || {
            let before = receiver.stats().rejected_layout.load(Ordering::Acquire);
            raw_publisher.put(malformed.clone()).wait().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while receiver.stats().rejected_layout.load(Ordering::Acquire) == before {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        });
        runtime.block_on(subscription.stop()).unwrap();
        if !tokens {
            let callback_count = Arc::new(AtomicUsize::new(0));
            let inline = Arc::new(AtomicBool::new(false));
            let sink = Arc::new(AllocatingCallback {
                count: Arc::clone(&callback_count),
                inline: Arc::clone(&inline),
                caller: std::thread::current().id(),
            });
            let local_route = TxTemplate::<Value>::new(common::agreement())
                .unwrap()
                .route(
                    Arc::clone(&receiver),
                    common::definition(common::source()),
                    RouteConfig::default(),
                )
                .unwrap();
            let mut callback_subscription = runtime
                .block_on(receiver.attach(&common::source(), None, sink))
                .unwrap();
            let observed = observe("native_callback_scope_positive_control", 1, || {
                local_route
                    .send(fields, |init| {
                        init.into_initializer().sequence(0).bytes_fill(0).finish()
                    })
                    .unwrap();
                wait(&callback_count, 1);
                assert!(
                    inline.load(Ordering::Acquire),
                    "positive callback was not invoked inline during the suspended native put"
                );
                assert!(
                    !OWN_SCOPE.with(Cell::get),
                    "binding scope was not restored after native put"
                );
            });
            assert!(observed.0 > 0 && observed.1 >= 64 && observed.2 > 0 && observed.3 >= 64);
            receiver.detach(&mut callback_subscription).unwrap();
        }
    }
    rpc(&runtime);
    println!("prepared_native_allocation_complete=true\tprocess_counter_scope=Rust_GlobalAlloc_all_threads\tnative_C_allocator_observed=false");
}
