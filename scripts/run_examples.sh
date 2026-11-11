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

# Durable core (no cargo feature): host timer + report + native fallback.
run dev_durable_timers ""

# Durable core: on_report vs timer report, set_timer phase lifecycle.
run dev_durable_reports ""

# The only example that persists/attaches transcript files.
run sessions "sdk,session"

# Real file edit + real Python test. Needs PIR_PYTHON pointing at a Python 3
# binary. It does NOT skip when the variable is unset: the example falls back
# to `python` on PATH, so on a host where `python` is the Windows Store
# placeholder (or absent) this step fails with a nonzero exit instead of
# silently passing. That is deliberate — a skipped repair step would make the
# only end-to-end edit+test example vacuous.
run mini_coding "sdk,tools"

# Persistent-session extras: compaction evidence, and lifecycle subcommands
# (start|fork|inspect take args, so this one is compile-checked only).
run session_compaction "sdk,session"
check session_lifecycle "sdk,session"

# Builtin matrix: real todo replay, memory autoRecall/isolation, tool-search,
# tool-repair, subagent.
run builtins_matrix "sdk,session,tools,todo,memory,tool-search,tool-repair,subagent"

# Typed SDK error paths (no model/context/tool/concurrency/shutdown).
run sdk_error_paths "sdk"

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
