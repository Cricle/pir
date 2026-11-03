#!/usr/bin/env bash
# Manual benchmark runner — dev/debug profile only.
#
# Benchmarks are deliberately NOT wired into CI: criterion measures wall-clock
# time, and a shared runner (plus debug builds) makes those numbers
# meaningless. Run them on a quiet machine, before/after a change, and quote
# the pair together with the load conditions.
#
# Usage:
#   scripts/run_benchmarks.sh                # both benches, criterion defaults
#   scripts/run_benchmarks.sh core           # benches/benchmark.rs only
#   scripts/run_benchmarks.sh sdk            # benches/sdk_benchmark.rs only
#   scripts/run_benchmarks.sh sdk -- --sample-size 10 \
#       --warm-up-time 1 --measurement-time 1 --noplot
#
# Extra arguments after the selector are forwarded to the benchmark binary.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/target}"
export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"

selector="${1:-all}"
if [ "$#" -gt 0 ]; then
    shift
fi

run_core() {
    cargo bench --bench benchmark --profile dev -- "$@"
}

run_sdk() {
    cargo bench --bench sdk_benchmark --profile dev -- "$@"
}

case "$selector" in
    core) run_core "$@" ;;
    sdk) run_sdk "$@" ;;
    all)
        run_core "$@"
        run_sdk "$@"
        ;;
    *)
        echo "usage: $0 [all|core|sdk] [-- <benchmark args>]" >&2
        exit 2
        ;;
esac
