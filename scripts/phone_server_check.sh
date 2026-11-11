#!/usr/bin/env bash
# CI/local gate for the standalone pi-phone central relay.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo check -p pir-phone-server
cargo test -p pir-phone-server
cargo clippy -p pir-phone-server --all-targets -- -D warnings
