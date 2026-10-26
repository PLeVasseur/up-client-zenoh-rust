# Transport benchmarks

`scripts/bench_transport_criterion.sh` runs the selected-wire and owned-core
payload-contract benchmarks and captures their command, toolchain, native
configuration, Git revision and raw output. `baseline` saves a Criterion baseline;
`candidate <name>` saves a comparison input. `export` writes artifacts under
`TRANSPORT_BENCH_REPORT_DIR`.

The default Criterion settings are ten samples, one second of warm-up, two seconds
of measurement and a noise threshold of 0.05. `CRITERION_ARGS` can override them.
`TRANSPORT_BENCH_PROFILE` selects core, camera or all fixtures; diagnostic selectors
are listed in the script's usage output. Full-loop selectors exercise complete
transport paths; component selectors attribute individual costs and do not prove
full-loop performance.

The stable native selector reserves its exact typed SDK TX route, native SHM
provider/publishers and generic typed receiver before Criterion iterations. It
fills payloads through the unchanged fixture initializer, sends synchronously
with in-SHM metadata, validates the actual typed received fixture and checks the
correlated benchmark acknowledgement. It creates no route, publisher or native
identity descriptor per frame. The acknowledgement/controller channel is measured
benchmark machinery, not the binding-scoped allocation oracle. The separate
harness-free native gate records all-thread scoped and whole-process Rust heap
counts, including token/RPC/rejection paths and an inline-callback positive control.

Aggregate guardrail comparison is not implemented. The `guardrail` and `export`
commands report `status: blocked`, a descriptive `suite: zenoh-payload-contract`
and a reason. This marker must not be interpreted as a passing comparison.
