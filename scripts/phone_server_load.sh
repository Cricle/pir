#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
exec cargo run --release -p pir-phone-server --example load_test -- "$@"
