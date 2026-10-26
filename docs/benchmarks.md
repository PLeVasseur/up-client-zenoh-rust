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

Aggregate guardrail comparison is not implemented. The `guardrail` and `export`
commands report `status: blocked`, a descriptive `suite: zenoh-payload-contract`
and a reason. This marker must not be interpreted as a passing comparison.
