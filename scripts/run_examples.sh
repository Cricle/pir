#!/usr/bin/env bash
# Build and run the offline pir SDK examples in their documented feature shapes.
#
# - Offline examples run with the scripted provider: no network, no API key.
# - The two network examples (`real_endpoint`, `proxy`) are compile-checked
#   only; this script never sends a request.
# - Each example is built with the MINIMAL feature set it declares in
#   `Cargo.toml` (`required-features`), plus the optional feature its own run
#   command documents (`tools`, `subagent`, `extensions`).
# - Exits nonzero on the first failure (`set -e`).
#
# Usage:
#   scripts/run_examples.sh
#   CARGO_TARGET_DIR=/path/to/target scripts/run_examples.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/target}"

step() {
    printf '\n==> %s\n' "$*"
    "$@"
}

run() {
    local example="$1"
    local features="$2"
    step cargo run --example "$example" --no-default-features --features "$features"
}

check() {
    local example="$1"
    local features="$2"
    step cargo check --example "$example" --no-default-features --features "$features"
}

# One pass over every example whose declared requirements this shape meets
# (the session example and the per-builtin/runtime examples are checked with
# their own features below).
step cargo check --examples --no-default-features --features "sdk"

# Core offline examples: `sdk` alone.
run quickstart "sdk"
run streaming "sdk"
run plugin "sdk"
run control "sdk"

# The only example that persists/attaches transcript files.
run sessions "sdk,session"

# Proxy is network-only at runtime; compile-check it in its minimal shape.
check proxy "sdk"

# Built-in coding tools (optional for the `tools` example).
run tools "sdk,tools"

# Compiled-in builtin.
run builtin "sdk,subagent"

# JS extension runtime.
run extension "sdk,extensions"

# Real provider: network-only at runtime and needs a wire adapter then;
# compile-check the example's declared (adapter-free) shape.
check real_endpoint "sdk,session,skills"

printf '\nAll offline examples passed; network examples compile.\n'
