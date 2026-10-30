# Prepared native shared-memory routes

With `zero-copy`, create `ZenohPreparedCore` using a validated local entity URI,
the real Zenoh configuration, a finite `ZenohRouteReservation` catalog and
`ZenohPreparedConfig`. The default provider is 64 MiB, with 128 live logical routes
and 100 live registrations. Construction eagerly initializes the POSIX provider;
its native mapping/locking behavior is described in [SHM prewarming](shm-prewarming.md).
Configured capacity is not a measured page census or proof that every slot saw traffic.

`from_session` accepts an already opened real session so configured endpoints can
share backend setup. The core retains the session and owns its independent SHM
provider/tables. Closing a shared session makes later puts fail; already retained
samples keep their native leases. Reserve each catalog before admitting traffic.

Catalog entries have exact source and optional sink, maximum metadata/payload
lengths/alignment and a transmit flag. Receiver-only entries declare no publisher.
Each transmit address declares seven native publishers during setup, preserving
SDK service-class overrides without declaration on the per-message path. Native
congestion control is Block. Optional `wait_for_matching` runs during setup and
checks presence of at least one matching subscriber, not an exact count or future
delivery. It does not warm RPC client admission or the server's client cache.

SDK `TxTemplate`, `OpaqueTemplate`, typed receiver admission and `WireRegistration`
provide shared identity, bounded token, correlation and initialization contracts.
Prepared routes cannot grow the catalog or their geometry. SHM exhaustion is a
typed loan error; full route/registration tables reject setup. Registration inbox
capacity and overflow policy belong to the SDK registration, with no additional
binding mailbox. Retained samples consume native provider capacity until released.

## Physical placement

The native SHM chunk contains a private 24-byte placement header, unchanged SDK
`UPWM` metadata bytes, alignment padding and unchanged application payload bytes.
The header is `UZSH`, placement version 1, payload-presence byte 0/1, two zero
reserved bytes, metadata length/payload offset/payload length as little-endian
u32 values and four zero reserved bytes. This is a binding-local representation,
not a normative wire or native-profile registry assignment. No Zenoh attachment
is used on this path. It is distinct from classic protobuf attachments and from
the allocating selected-wire SHM control; peers select the same representation.

The native lease is exclusive and immobile while TX writes disjoint metadata and
payload extents. SDK initialization proof is required for publication. RX checks
`sample.payload().as_shm()` before interpreting placement, checked bounds before
forming slices and the SDK admission before typed references. Absent payload and
present-empty payload remain distinct. Non-SHM, attachment-bearing and malformed
placement samples are rejected and counted; the binding never copies/coalesces
bytes to manufacture a successful native borrow.

## Delivery and ownership

Attach declares one real native subscriber and supplies its callback a retained
SDK `FrameSink`. The callback pushes one native frame synchronously; the generic
SDK worker owns the async handler future and preserves inbox order. There is no
task allocation per sample or hidden binding queue. Block may hold the native
delivery thread up to the SDK inbox deadline, including during reentrant sends;
synchronous publication alone does not prove end-to-end nonblocking progress.

A held native delivery thread also stalls the receiving transport behind it.
Published SHM chunks stay owned by the native transport until the receiving
transport has processed them, so frames already in transit remain valid during
such a stall and arrive in order once the callback returns; their validity does
not depend on how quickly the receiver maps each descriptor. This relies on the
SHM handoff that the pinned Zenoh release negotiates with its peers: every SHM peer
of a deployment must use a release negotiating the same SHM protocol. A peer
without that capability delivers no native SHM sample; the prepared receiver
rejects and counts such samples instead of copying them into a native borrow.

A put that would synchronously reenter an already-entered local native sink is
refused with a delivery error before native publication. Setup-owned callback
identities cover other cores sharing the session and ancestors of nested native
callbacks, without a per-message allocation or TLS stack. A different target and
SDK registration-worker forwarding keep their normal path; a worker publishing
into its own full application inbox still observes that inbox's configured
overflow/deadline policy. See [listener lifecycle](listener-lifecycle.md) for the
distinct admission, producer-wakeup and quiescence boundaries.

Detach closes the sink before retiring the subscriber. An already-entered native
callback retains the closed sink until its push returns. SDK stop/cancel governs
drain and worker quiescence. Frames already admitted retain their actual Zenoh
sample and SHM lease after detach/core drop; retirement cannot revoke a borrower.

Unpublished metadata is wiped when a TX loan is dropped. Published native chunks
are reclaimed by Zenoh; its public API does not provide a binding-owned last-reader
wiping hook. This limitation does not replace the SDK's sender token overwrite,
clear/drop or owned-pool reclaim wiping guarantees. Token debug output remains
redacted by the SDK.

## Allocation observations

`ZenohAllocationObserver` is an optional setup-installed, nonallocating per-thread
scope hook. Our prepared methods enter the counted scope, native calls suspend
it and native callbacks explicitly re-enter it even when called before a suspended
publication returns. Each scope restores the previous flag. Whole-process counters
remain active on every thread and include middleware and observer costs. Their
counts are reported separately from the binding-scoped zero-call/zero-byte gate;
no counter cost is subtracted. Classic messages and dynamic owned/SHM controls
are allocating comparisons, not prepared zero-allocation evidence.

Runtime workers and binding delivery threads require explicit setup before the
post-setup observation window. Unknown clients' first RPC calls remain counted.
Native counters distinguish publication failures, SHM receive, non-SHM rejection
and placement rejection from SDK admission/inbox/application drops.

## Receiver-only physical reservations

A prepared catalog containing only transmit=false entries receives retained
remote SHM samples without reserving an unused local provider. Its
prewarmed_shm_segment_size is zero. A catalog with any transmitting entry keeps
its configured, eagerly initialized provider and all existing geometry checks.
TX reservation remains invalid for receive-only addresses; receive never creates
a provider or copies/coalesces a payload. No queue or runtime limit changes.
