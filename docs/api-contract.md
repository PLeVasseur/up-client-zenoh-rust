# Public transport and buffer APIs

`UPTransportZenoh` retains its session, listener registrations and RPC correlation
state. Its typestate builder accepts a configuration value, configuration-file path
or existing session. `ZenohRpcClient` implements the SDK RPC client contract using
queries and replies through that transport; it does not choose a payload codec
from incoming bytes.

The selected-wire SHM core separates initialized `ZenohTxBuffer` and uninitialized
`ZenohUninitTxBuffer` publication paths. Each owns its payload allocation and
metadata attachment through publication. `ZenohRxFrame` retains the native sample
and encoded metadata; `sample()` returns a borrow bounded by that frame's lifetime.
SDK selected-wire validation remains above the physical carriage layer.

See [frame binding](uframe-binding.md) for placement and
[listener lifecycle](listener-lifecycle.md) for close/quiesce/retained ownership.
The public API, examples and test targets use `missing_docs = "deny"`, with strict
Rustdoc/doctest checks. Benchmark entry functions are private harness implementation
details and retain Criterion's normal configuration and final-summary behavior.
