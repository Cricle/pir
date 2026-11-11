#!/usr/bin/env bash
# Repeatable pi-phone server benchmark (release profile).
#
#   bash scripts/phone_server_bench.sh
#   bash scripts/phone_server_bench.sh --iterations 10
#
# Each scenario prints one Markdown row and one `BENCH_JSON {...}` line.
set -euo pipefail
cd "$(dirname "$0")/.."

exec cargo run --release -p pir-phone-server --example bench -- "$@"
