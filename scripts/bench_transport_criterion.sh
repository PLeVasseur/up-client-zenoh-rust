#!/usr/bin/env bash
#
# Copyright (c) 2026 Contributors to the Eclipse Foundation
#
# See the NOTICE file(s) distributed with this work for additional
# information regarding copyright ownership.
#
# This program and the accompanying materials are made available under the
# terms of the Apache License Version 2.0 which is available at
# https://www.apache.org/licenses/LICENSE-2.0
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

readonly TRANSPORT_NAME="zenoh"
readonly DEFAULT_REPORT_DIR="target/transport-perf/$TRANSPORT_NAME"
readonly DEFAULT_CRITERION_ARGS="--output-format bencher --sample-size 10 --warm-up-time 1 --measurement-time 2 --noise-threshold 0.05"
readonly BASELINE_NAME="payload_contract_representative_v1"

TRANSPORT_BENCH_SUITE="${TRANSPORT_BENCH_SUITE:-payload-contract}"
TRANSPORT_BENCH_PROFILE="${TRANSPORT_BENCH_PROFILE:-all}"
TRANSPORT_BENCH_REPORT_DIR="${TRANSPORT_BENCH_REPORT_DIR:-$DEFAULT_REPORT_DIR}"
CRITERION_ARGS="${CRITERION_ARGS:-$DEFAULT_CRITERION_ARGS}"
BENCH_PIN_PREFIX="${BENCH_PIN_PREFIX:-}"
CARGO_BIN="${CARGO_BIN:-cargo}"

usage() {
    cat <<'USAGE'
Usage:
  scripts/bench_transport_criterion.sh baseline
  scripts/bench_transport_criterion.sh candidate <phase_candidate>
  scripts/bench_transport_criterion.sh guardrail <phase_candidate> <report_path>
  scripts/bench_transport_criterion.sh export

Environment:
  TRANSPORT_BENCH_REPORT_DIR  Report output directory. Default: target/transport-perf/zenoh
  TRANSPORT_BENCH_SUITE       payload-contract. Default: payload-contract
  TRANSPORT_BENCH_PROFILE     core, camera, or all. Default: all
  CRITERION_ARGS              Criterion args. Default matches USR-10B1 C1.
  BENCH_PIN_PREFIX            Optional command prefix for CPU pinning, etc.
  CARGO_BIN                   Cargo command. Default: cargo
USAGE
}

cargo_features() {
    case "$TRANSPORT_BENCH_SUITE" in
        payload-contract)
            printf '%s\n' "zero-copy,benchmark-owned,payload-contract-large-benchmarks"
            ;;
        *)
            printf 'TRANSPORT_BENCH_SUITE must be payload-contract\n' >&2
            exit 2
            ;;
    esac
}

validate_profile() {
    case "$TRANSPORT_BENCH_PROFILE" in
        core | camera | all) ;;
        *)
            printf 'TRANSPORT_BENCH_PROFILE must be one of core, camera, all\n' >&2
            exit 2
            ;;
    esac
}

run_cargo_bench() {
    validate_profile

    local features
    features="$(cargo_features)"

    read -r -a criterion_parts <<<"$CRITERION_ARGS"
    read -r -a cargo_parts <<<"$CARGO_BIN"
    if [[ -n "$BENCH_PIN_PREFIX" ]]; then
        read -r -a pin_parts <<<"$BENCH_PIN_PREFIX"
        TRANSPORT_BENCH_SUITE="$TRANSPORT_BENCH_SUITE" \
            TRANSPORT_BENCH_PROFILE="$TRANSPORT_BENCH_PROFILE" \
            "${pin_parts[@]}" "${cargo_parts[@]}" bench --features "$features" --bench transport_criterion -- "${criterion_parts[@]}" "$@"
    else
        TRANSPORT_BENCH_SUITE="$TRANSPORT_BENCH_SUITE" \
            TRANSPORT_BENCH_PROFILE="$TRANSPORT_BENCH_PROFILE" \
            "${cargo_parts[@]}" bench --features "$features" --bench transport_criterion -- "${criterion_parts[@]}" "$@"
    fi
}

git_value() {
    git "$@" 2>/dev/null || printf '%s\n' "unknown"
}

write_summary() {
    local report_dir="$1"
    local raw_output="$2"
    local summary="$report_dir/README.md"
    local features
    features="$(cargo_features)"
    read -r -a cargo_parts <<<"$CARGO_BIN"

    cat >"$summary" <<SUMMARY
# Zenoh Userializer Payload-Contract Representative Benchmarks

## Command

\`\`\`bash
TRANSPORT_BENCH_SUITE=$TRANSPORT_BENCH_SUITE TRANSPORT_BENCH_PROFILE=$TRANSPORT_BENCH_PROFILE CARGO_BIN="$CARGO_BIN" scripts/bench_transport_criterion.sh export
\`\`\`

## Environment

- Transport: Zenoh
- Phase: USR-10B1X
- Git head: \`$(git_value rev-parse HEAD)\`
- Git branch: \`$(git_value branch --show-current)\`
- Default Rust: \`$(rustc --version)\`
- Default Cargo: \`$(cargo --version)\`
- Cargo command: \`$CARGO_BIN\`
- Cargo command version: \`$("${cargo_parts[@]}" --version)\`
- OS: \`$(uname -srmo)\`
- Suite: \`$TRANSPORT_BENCH_SUITE\`
- Profile: \`$TRANSPORT_BENCH_PROFILE\`
- Features: \`$features\`
- Criterion args: \`$CRITERION_ARGS\`
- Pinning prefix: \`${BENCH_PIN_PREFIX:-none}\`
- Raw output: \`$raw_output\`

## Required Labels

- \`stable_zc_nozero_full\`
- \`protobuf_owned_full\`
- \`stable_owned_bytes_full\`

## Claim Boundary

This script is the USR-10B1X authority wrapper for the Zenoh selected-wire and owned-core benchmark command shape. It writes artifacts only under the caller-selected report directory. The generated guardrail is a blocker marker, not aggregate USR-10 guard authority.
SUMMARY
}

export_results() {
    local report_dir="$TRANSPORT_BENCH_REPORT_DIR"
    local bench_data_dir="$report_dir/bench-data"
    local raw_output="$bench_data_dir/transport-criterion-bencher.txt"

    mkdir -p "$bench_data_dir"
    run_cargo_bench | tee "$raw_output"

    rm -rf "$report_dir/criterion-html"
    mkdir -p "$report_dir/criterion-html"
    if [[ -d target/criterion ]]; then
        cp -a target/criterion/. "$report_dir/criterion-html/"
    fi

    cat >"$report_dir/guardrail.json" <<JSON
    {"status":"blocked","phase":"USR-10B1X","reason":"real aggregate Zenoh guard comparison is not implemented in this branch"}
JSON

    write_summary "$report_dir" "$raw_output"
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

subcommand="$1"
shift

case "$subcommand" in
    baseline)
        run_cargo_bench --save-baseline "$BASELINE_NAME"
        ;;
    candidate)
        if [[ $# -ne 1 ]]; then
            usage
            exit 1
        fi
        run_cargo_bench --save-baseline "$1"
        ;;
    guardrail)
        if [[ $# -ne 2 ]]; then
            usage
            exit 1
        fi
        mkdir -p "$(dirname "$2")"
        cat >"$2" <<JSON
{"status":"blocked","phase":"USR-10B1X","candidate":"$1","reason":"real aggregate Zenoh guard comparison is not implemented in this branch"}
JSON
        ;;
    export)
        export_results
        ;;
    *)
        usage
        exit 2
        ;;
esac
