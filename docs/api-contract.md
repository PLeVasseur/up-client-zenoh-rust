# Public transport and buffer APIs

`UPTransportZenoh` retains its session, listener registrations and RPC correlation
state. Its typestate builder accepts a configuration value, configuration-file path
or existing session. `ZenohRpcClient` implements the SDK RPC client contract using
queries and replies through that transport; it does not choose a payload codec
from incoming bytes.
It verifies reply message type, request ID and reversed source/sink before exposing
payload, preserves absent versus present-empty replies and maps response status
codes to SDK invocation errors. Native query submission and reply waiting share
the original request deadline. A call has one nonblocking one-shot reply slot;
the first native reply completes it and later duplicates are discarded. There is
no separate default middleware reply FIFO or per-reply task.
[Listener lifecycle](listener-lifecycle.md) covers
server-side pending query cancellation and expiry.

The dynamic allocating SHM control separates initialized `ZenohTxBuffer` and uninitialized
`ZenohUninitTxBuffer` publication paths. Each owns its payload allocation and
metadata attachment through publication. `ZenohRxFrame` retains the native sample
and encoded metadata; `sample()` returns a borrow bounded by that frame's lifetime.
SDK selected-wire validation remains above the physical carriage layer.

Native prepared applications instead reserve `ZenohPreparedCore` routes, provider
and registration capacities before traffic. `new` opens a session; `from_session`
retains an existing shared native session. `ZenohPreparedTx` exposes checked,
disjoint in-SHM extents; `ZenohPreparedRx` retains the actual native sample.
Synchronous TX and generic SDK inbox/worker delivery replace per-message tasks.
See [prepared SHM routes](prepared.md) for placement, capacities, source-scoped
allocation observations, shared-session closure and native token wiping limits.
The binding's `zero-copy` feature enables SDK inline URI storage. Default classic
builds and owned-only support retain String storage and require no SHM provider.

`ZenohZeroCopyCoreBuilder::with_shm_prewarm(true)` optionally initializes the actual
provider before returning the core, delegating mapping and locking to Zenoh.
`prewarmed_shm_segment_size()` reports the configured size, not measured residency.
See [SHM provider prewarming](shm-prewarming.md) for failure and memory costs.

Both frame and message paths use the SDK's shared `UPriority` before the existing
Zenoh priority mapping. Classic sample attributes project their own encoding and
are checked together with payload presence when rebuilding the message. Selected
wire/family and codec-layout identities are combined explicitly for metadata.
Safe loan-view construction uses the retained SHM sample; it does not copy or
coalesce serialized payload slices into a claimed native loan.

See [frame binding](uframe-binding.md) for placement and
[listener lifecycle](listener-lifecycle.md) for close/quiesce/retained ownership.
The public API, examples and test targets use `missing_docs = "deny"`, with strict
Rustdoc/doctest checks. Benchmark entry functions are private harness implementation
details and retain Criterion's normal configuration and final-summary behavior.
