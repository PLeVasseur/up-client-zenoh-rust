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

readonly DEFAULT_CRITERION_ARGS="--sample-size 60 --warm-up-time 3 --measurement-time 12 --noise-threshold 0.02"
readonly DEFAULT_LARGE_SENSOR_CRITERION_ARGS="--sample-size 20 --warm-up-time 2 --measurement-time 8 --noise-threshold 0.03"
readonly BASELINE_NAME="transport_owned_zc_baseline"
readonly TRANSPORT_NAME="zenoh"
readonly DEFAULT_REPORT_DIR="target/transport-perf/$TRANSPORT_NAME"

CRITERION_ARGS="${CRITERION_ARGS:-$DEFAULT_CRITERION_ARGS}"
LARGE_SENSOR_CRITERION_ARGS="${LARGE_SENSOR_CRITERION_ARGS:-$DEFAULT_LARGE_SENSOR_CRITERION_ARGS}"
BENCH_PIN_PREFIX="${BENCH_PIN_PREFIX:-}"
TRANSPORT_BENCH_PROFILE="${TRANSPORT_BENCH_PROFILE:-all}"
TRANSPORT_BENCH_REPORT_DIR="${TRANSPORT_BENCH_REPORT_DIR:-$DEFAULT_REPORT_DIR}"

usage() {
    cat <<'USAGE'
Usage:
  scripts/bench_transport_criterion.sh baseline
  scripts/bench_transport_criterion.sh candidate <phase_candidate>
  scripts/bench_transport_criterion.sh guardrail <phase_candidate> <report_path>
  scripts/bench_transport_criterion.sh export
USAGE
}

run_cargo_bench() {
    local profile="$1"
    local criterion_args="$2"
    shift 2

    if [[ -n "$BENCH_PIN_PREFIX" ]]; then
        read -r -a pin_parts <<<"$BENCH_PIN_PREFIX"
        TRANSPORT_BENCH_PROFILE="$profile" "${pin_parts[@]}" cargo bench --features zero-copy --bench transport_criterion -- $criterion_args "$@"
    else
        TRANSPORT_BENCH_PROFILE="$profile" cargo bench --features zero-copy --bench transport_criterion -- $criterion_args "$@"
    fi
}

run_selected_profiles() {
    local baseline_flag="$1"
    local baseline_value="$2"
    shift 2
    case "$TRANSPORT_BENCH_PROFILE" in
        core)
            run_cargo_bench core "$CRITERION_ARGS" "$baseline_flag" "$baseline_value" "$@"
            ;;
        camera)
            run_cargo_bench camera "$LARGE_SENSOR_CRITERION_ARGS" "$baseline_flag" "$baseline_value" "$@"
            ;;
        all)
            run_cargo_bench core "$CRITERION_ARGS" "$baseline_flag" "$baseline_value" "$@"
            run_cargo_bench camera "$LARGE_SENSOR_CRITERION_ARGS" "$baseline_flag" "$baseline_value" "$@"
            ;;
        *)
            echo "TRANSPORT_BENCH_PROFILE must be one of core, camera, all" >&2
            exit 2
            ;;
    esac
}

write_summary() {
    local report_dir="$1"
    local summary="$report_dir/README.md"
    local rust_version
    local cpu_model
    rust_version="$(rustc --version)"
    cpu_model="$(awk -F': ' '/model name/ { print $2; exit }' /proc/cpuinfo 2>/dev/null || true)"
    cpu_model="${cpu_model:-unknown}"

    cat >"$summary" <<SUMMARY
# Zenoh Owned vs Zero-Copy Transport Benchmarks

## Environment

- Transport: Zenoh with feature flags \`zero-copy\`
- Rust: \`$rust_version\`
- OS: \`$(uname -srmo)\`
- CPU: \`$cpu_model\`
- Core Criterion args: \`$CRITERION_ARGS\`
- Large sensor Criterion args: \`$LARGE_SENSOR_CRITERION_ARGS\`
- Pinning prefix: \`${BENCH_PIN_PREFIX:-none}\`

## Methodology

Benchmarks use deterministic RawBytes payloads and wait for the matching uProtocol frame ID for send/receive measurements. Path labels are \`owned\`, \`zero_copy_loan_copy\`, and \`zero_copy_uninit_direct\`. The loan-copy path copies precomputed bytes into a Zenoh SHM transmit loan; only \`zero_copy_uninit_direct\` generates bytes directly in an uninitialized transmit loan.

Core payload cases: \`empty_present\` 0 B, \`can_classic_max\` 8 B, \`can_fd_max\` 64 B, \`someip_single_mtu\` 1456 B, \`streamer_4k\` 4096 B, \`radar_ars548_detection_list\` 35336 B, and \`streamer_64k\` 65536 B.

Large sensor payload case: \`camera_8mp_3840x2160_raw12_packed\` 12441600 B.

Message types: \`publish\`, \`notification\`, \`request\`, and \`response\`.

## Ratio Table

Use \`bench-data/criterion-compare-bencher.txt\` and \`criterion-html/\` for the exported Criterion measurements and plots. The ratio table is intentionally curated from exported data so copying adapters or loan-copy transmit are not described as direct true-zero-copy.

## Interpretation

Interpret \`owned\` vs \`zero_copy_loan_copy\` as the primary apples-to-apples transport-boundary comparison. Interpret \`zero_copy_uninit_direct\` as the best-case direct true-zero-copy transmit data.

## Caveats

Zenoh zero-copy receive is strict SHM-backed for payload-bearing frames. If SHM support is unavailable, the benchmark fails instead of reporting fallback data.
SUMMARY
}

export_results() {
    local report_dir="$TRANSPORT_BENCH_REPORT_DIR"
    local bench_data_dir="$report_dir/bench-data"
    local report_path="$bench_data_dir/criterion-compare-bencher.txt"
    mkdir -p "$bench_data_dir"
    run_selected_profiles --baseline "$BASELINE_NAME" --output-format bencher | tee "$report_path"
    rm -rf "$report_dir/criterion-html"
    mkdir -p "$report_dir/criterion-html"
    if [[ -d target/criterion ]]; then
        cp -a target/criterion/. "$report_dir/criterion-html/"
    fi
    if [[ ! -f "$report_dir/guardrail.json" ]]; then
        cat >"$report_dir/guardrail.json" <<JSON
{"status":"unavailable","reason":"criterion-guardrail utility is not available in this standalone repository"}
JSON
    fi
    write_summary "$report_dir"
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

subcommand="$1"
shift

case "$subcommand" in
    baseline)
        run_selected_profiles --save-baseline "$BASELINE_NAME"
        ;;
    candidate)
        if [[ $# -ne 1 ]]; then
            usage
            exit 1
        fi
        run_selected_profiles --save-baseline "$1"
        ;;
    guardrail)
        if [[ $# -ne 2 ]]; then
            usage
            exit 1
        fi
        mkdir -p "$(dirname "$2")"
        cat >"$2" <<JSON
{"status":"unavailable","candidate":"$1","reason":"criterion-guardrail utility is not available in this standalone repository"}
JSON
        ;;
    export)
        export_results
        ;;
    *)
        usage
        exit 1
        ;;
esac
