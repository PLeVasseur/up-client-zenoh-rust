# SHM provider prewarming

Transmit loans normally create the Zenoh SHM provider lazily. A deployment that
needs construction-time page materialization can explicitly opt in:

```rust,no_run
# async fn example() -> Result<(), up_rust::UStatus> {
use up_transport_zenoh::ZenohZeroCopyCore;

let bytes = 48 * 1024 * 1024;
let core = ZenohZeroCopyCore::builder("//vehicle/4210/1/1/0")
    .with_shm_segment_size(bytes)?
    .with_shm_prewarm(true)
    .build()
    .await?;
assert_eq!(core.prewarmed_shm_segment_size(), Some(bytes));
# Ok(())
# }
```

Prewarming is disabled by default. When enabled, the binding creates the actual
configured provider through Zenoh's safe API before returning. The pinned POSIX
implementation maps the complete segment and calls `mlock` on that mapping;
success therefore materializes its pages without a temporary payload allocation
or redundant writes. It adds construction-time work and resident/locked-memory
demand; it does not add a payload copy to publication or reception. Invalid
layouts return `InvalidArgument`; provider initialization, including native
mapping or locking failure, returns `Internal`. A failure returns no usable core.
There is no smaller-provider or non-SHM fallback.

The pinned POSIX backend keeps allocator metadata inside its mapping. Configured
size and page-rounded physical mapping size can therefore be different quantities.
The setup receipt identifies the configured provider, not a count of application-
written bytes or an OS observation. Linux native tests locate the provider mapping
through a subsequent real transmit loan and independently check its `Size`, `Rss`
and `Locked` in `/proc/self/smaps`, including allocator overhead. Repeated near-
capacity loans prove setup retained no temporary payload allocation. Residency on
other platforms requires separate native proof. Remote mappings and their locking
remain owned by Zenoh, not by this builder option.

Pool capacity, matching-subscriber readiness, listener quiescence and retained
sample lifetimes remain separate contracts. Choose sufficient pool capacity for
all simultaneously retained loans before enabling prewarming.

## Prepared endpoint runtime initialization

`ZenohPreparedCore::new` and `ZenohPreparedCore::from_session` explicitly invoke
Zenoh's SHM runtime initialization during prepared setup, even if an externally
opened session selected lazy initialization. Creating a payload provider alone
does not initialize all upstream SHM services. Prepared setup starts the global
client/metadata, cleanup and watchdog services without allocating or publishing a
priming sample. The opt-in builder prewarming path initializes these services too.

Transmit-capable prepared catalogs still reserve their configured payload
provider. Receive-only catalogs initialize the runtime needed to borrow remote
samples but reserve no local payload provider; their reported prewarmed provider
capacity remains zero. Runtime initialization is not a watchdog-thread scheduling
barrier, a matching check or proof that a remote application received a successful
local publication. Those remain separate lifecycle and conservation checks.
