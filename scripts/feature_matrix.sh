#!/usr/bin/env bash
# Feature-matrix gate: every documented SDK configuration must compile and the
# bin profiles must run.
#
# Rows (count is asserted below so this comment cannot drift):
#   core   57 library rows: the boundary cases `docs/FEATURES.md` promises first
#          (bare minimal core, minimal + CLI, tui/extensions/...), then each
#          non-provider feature alone — every selectable feature in Cargo.toml
#          except `default`/`full` has its own row (including the seven
#          per-tool leaves `tool-read`/`tool-bash`/`tool-powershell`/
#          `tool-edit`/`tool-write`/`tool-ls`/`tool-background`, their
#          `tool-leaves` union, the `tools` meta-feature, `grep-find`,
#          `print`, `rpc`, `export-html`, `env`, `env-daemon`, `session`,
#          `skills`, `sdk`, `client`), then each `provider-*` family alone,
#          then high-risk pairs/triples (`tui,extensions`, `tui,tool-edit`
#          for the edit tool + preview seam, `tui,image-tools` for the read
#          attach path + the preview, `tools,grep-find` for the two
#          independent tool families, `grep-find,env-daemon` for the shared
#          `ignore` walker, `extensions,provider-*` for the shared
#          `rsa`/`hmac` backends, `server,client` for both wire ends,
#          `server,phone`, `subagent,tool-search,todo`, `sdk,provider-openai`
#          for the documented sdk-headless profile, `sdk,session,skills`),
#          the documented slim profiles (`no-js`, `tui-slim`), and finally
#          `--all-features` and the default `full` build. `rpc` alone
#          deliberately does NOT imply `export-html` (see the smoke row
#          below); `env-daemon` implies `env`, and `env` alone is a
#          dependency-only row (no daemon, no ops). `tui-slim` deliberately
#          omits `env-daemon` (the documented loss: adding it would pull
#          `ignore` and the 4.9k-line daemon into the measured slim profile);
#          `no-js` keeps it. `tools` is the meta-feature over the seven
#          leaves; `grep-find` is an INDEPENDENT leaf (it does not imply
#          `tools` or any `tool-*`; the `tool-grep`/`tool-find` split is
#          deferred).
#   bin     14 binary rows: the CLI binary's manifest minimum is `cli`
#          (Cargo.toml `required-features`); `cli,tool-read` (the partial
#          leaf build), `cli,tools`, `cli,tools,image-tools` (the inline image
#          attach path), `cli,grep-find` (the independent search leaf),
#          `cli,sdk` (the embedding facade alongside the binary), `cli,mcp`,
#          `cli,mcp-sd`, `cli,brush`, `cli,client`, `cli,env-daemon`,
#          `cli,provider-openai` (headless), `cli,tui,provider-openai`
#          (tui-slim), and the default `full` build.
#          The `cli` row is the real toolless binary; `cli` itself implies
#          `print`, `rpc` and `export-html`, so every bin row compiles all
#          three. `bin(cli)` is also the `env-daemon` off-arm;
#          `bin(cli,env-daemon)` is the on-arm.
#   sdk     2 consumer rows: the `tests/sdk_consumer_*.rs` public-surface pins
#          (lane regression suites excepted), which an embedder compiles
#          against the bare crate plus the optional `sdk` feature
#          (`--no-default-features --features sdk`). The bare-crate
#          `minimal` row deliberately does NOT compile them: `pir::sdk` does
#          not exist without the feature (no stub).
#   smoke   8 runtime rows: one per runtime-tested bin profile (`cli`, `cli,tool-read`,
#          `cli,tools`, `cli,mcp`, `cli,provider-openai`,
#          `cli,tui,provider-openai`, `full`), plus `off(rpc,export-html)`,
#          which runs `tests/rpc_export_off.rs` in an
#          `rpc`-without-`export-html` build and pins the exact documented
#          `export_html` off-error (the in-process RPC server answers it before
#          attaching a session). Smoke rows may carry an optional third
#          `|test-target` field; it defaults to `feature_off_surface`.
#          `cargo test --test feature_off_surface` builds the `pir` bin for
#          that exact profile (via CARGO_BIN_EXE_pir), then checks `--version`,
#          the documented missing-feature error for an absent surface (the
#          `full` row has none absent, so it runs `--version` + the print mock
#          only), and a print-mode run against a local OpenAI-SSE mock (the
#          `cli` row has no wire adapter and asserts the documented provider
#          error instead). `cli,mcp` also covers the `mcp`-without-`mcp-sd`
#          arm; the slim `--tools grep`/`--tools <core>` diagnostics live in
#          the same target (`#[cfg(not(feature = "grep-find"))]` and the
#          per-leaf `#[cfg(not(feature = "tool-*"))]` arms), because `g_tools`
#          pulls sibling suites that need `tui`/`extensions`.
#          `smoke(cli,tool-read)` pins the partial build: `read` is available
#          while every other leaf reports its own missing feature.
#   TOTAL  81 cargo invocations.
#
# It runs in the cargo target dir the caller selects (`CARGO_TARGET_DIR`,
# default `target-alt`); lanes run it in their own `target-lane-*` dir so an
# 81-row sweep never touches the foreman's `target/`. ALL rows run even after
# a failure — the point of a matrix is the complete list of broken
# configurations — and any failed row makes the script exit 1 with the failing
# rows named. Exit 2 = wrong working directory / missing manifest.
set -u
cd "$(dirname "$0")/.." || exit 2

TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target-alt}"
export CARGO_TARGET_DIR="$TARGET_DIR"
export CARGO_INCREMENTAL=0
LOG="$TARGET_DIR/feature-matrix.log"
mkdir -p "$TARGET_DIR" || exit 2

CONFIGS=(
  "minimal|"
  "minimal+cli|--features cli"
  "tui|--features tui"
  "extensions|--features extensions"
  "python|--features python"
  "context-mode|--features context-mode"
  "mcp|--features mcp"
  "mcp-sd|--features mcp-sd"
  "env|--features env"
  "env-daemon|--features env-daemon"
  "server|--features server"
  "client|--features client"
  "subagent|--features subagent"
  "tool-repair|--features tool-repair"
  "tool-search|--features tool-search"
  "todo|--features todo"
  "phone|--features phone"
  "tool-read|--features tool-read"
  "tool-bash|--features tool-bash"
  "tool-powershell|--features tool-powershell"
  "tool-edit|--features tool-edit"
  "tool-write|--features tool-write"
  "tool-ls|--features tool-ls"
  "tool-background|--features tool-background"
  "tool-leaves|--features tool-read,tool-bash,tool-powershell,tool-edit,tool-write,tool-ls,tool-background"
  "tools|--features tools"
  "grep-find|--features grep-find"
  "brush|--features brush"
  "image-tools|--features image-tools"
  "image-protocol|--features image-protocol"
  "print|--features print"
  "rpc|--features rpc"
  "export-html|--features export-html"
  "session|--features session"
  "skills|--features skills"
  "sdk|--features sdk"
  "provider-openai|--features provider-openai"
  "provider-anthropic|--features provider-anthropic"
  "provider-google|--features provider-google"
  "provider-bedrock|--features provider-bedrock"
  "provider-mistral|--features provider-mistral"
  "providers-rest|--features provider-anthropic,provider-google,provider-bedrock,provider-mistral"
  "tui+extensions|--features tui,extensions"
  "tui+tool-edit|--features tui,tool-edit"
  "tui+image-tools|--features tui,image-tools"
  "tools+grep-find|--features tools,grep-find"
  "grep-find+env-daemon|--features grep-find,env-daemon"
  "extensions+providers|--features extensions,provider-anthropic,provider-google,provider-bedrock,provider-mistral"
  "server+client|--features server,client"
  "server+phone|--features server,phone"
  "subagent+tool-search+todo|--features subagent,tool-search,todo"
  "sdk+provider-openai|--features sdk,provider-openai"
  "sdk+session+skills|--features sdk,session,skills"
  "no-js|--features cli,tui,env,env-daemon,server,client,subagent,tool-repair,tool-search,todo,python,context-mode,phone,provider-openai,provider-anthropic,provider-google,provider-bedrock,provider-mistral,tools,grep-find,image-tools,session,skills"
  "tui-slim|--features cli,tui,tools,provider-openai,provider-anthropic,session,skills"
  "all-features|"
  "full|"
)
# The first two rows are spelled explicitly: "minimal" is the bare crate and
# "minimal+cli" is the bare crate plus the CLI argument surface. The "no-js"
# and "tui-slim" rows mirror the documented library profiles (docs/FEATURES.md
# §"slimming profiles"); "full" is the default build (it includes
# `image-protocol`); "all-features" is `--all-features`.
# The `tui` row (no `image-protocol`) compiles the text-fallback arm of
# `render_image_with_settings`; the `image-protocol` row compiles the
# halfblocks renderer.
BIN_CONFIGS=(
  "bin(cli)|--features cli"
  "bin(cli,tool-read)|--features cli,tool-read"
  "bin(cli,tools)|--features cli,tools"
  "bin(cli,tools,image-tools)|--features cli,tools,image-tools"
  "bin(cli,grep-find)|--features cli,grep-find"
  "bin(cli,sdk)|--features cli,sdk"
  "bin(cli,env-daemon)|--features cli,env-daemon"
  "bin(cli,client)|--features cli,client"
  "bin(cli,mcp)|--features cli,mcp"
  "bin(cli,mcp-sd)|--features cli,mcp-sd"
  "bin(cli,brush)|--features cli,brush"
  "bin(cli,provider-openai)|--features cli,provider-openai"
  "bin(cli,tui,provider-openai)|--features cli,tui,provider-openai"
  "bin(full)|"
)
SMOKE_CONFIGS=(
  "smoke(cli)|--features cli"
  "smoke(cli,tool-read)|--features cli,tool-read"
  "smoke(cli,tools)|--features cli,tools"
  "smoke(cli,mcp)|--features cli,mcp"
  "smoke(cli,provider-openai)|--features cli,provider-openai"
  "smoke(cli,tui,provider-openai)|--features cli,tui,provider-openai"
  "smoke(full)|"
  "off(rpc,export-html)|--features rpc|rpc_export_off"
)

# The lib/bin/smoke counts in the header are asserted, not asserted-by-nobody:
# a row added without updating the header (or a stale header) fails the gate
# before it spends any time compiling. The consumer count is asserted in the
# consumer phase below (its glob is only known there).
EXPECTED_LIB_ROWS=57
EXPECTED_BIN_ROWS=14
EXPECTED_CONSUMER_ROWS=2
EXPECTED_SMOKE_ROWS=8
if [ "${#CONFIGS[@]}" -ne "$EXPECTED_LIB_ROWS" ] ||
   [ "${#BIN_CONFIGS[@]}" -ne "$EXPECTED_BIN_ROWS" ] ||
   [ "${#SMOKE_CONFIGS[@]}" -ne "$EXPECTED_SMOKE_ROWS" ]; then
  echo "FEATURE MATRIX: row-count mismatch (lib ${#CONFIGS[@]}/$EXPECTED_LIB_ROWS, bin ${#BIN_CONFIGS[@]}/$EXPECTED_BIN_ROWS, smoke ${#SMOKE_CONFIGS[@]}/$EXPECTED_SMOKE_ROWS); update the header with the rows" >&2
  exit 2
fi

FAILED_ROWS=()
RUN=0

# run_row NAME ARGS... — one cargo invocation, its full output into $LOG.
# A failed row prints the tail of its own log (overwritten per row), so the
# failure is readable without hunting through the script's stdout.
run_row() {
  label="$1"; shift
  printf '%-28s ' "$label"
  RUN=$((RUN + 1))
  if cargo "$@" --locked >"$LOG" 2>&1; then
    echo "OK"
  else
    rc=$?
    echo "FAIL (rc=$rc)"
    tail -20 "$LOG"
    FAILED_ROWS+=("$label")
  fi
}

for row in "${CONFIGS[@]}"; do
  name="${row%%|*}"
  flags="${row#*|}"
  # `full` is the default build, `all-features` passes the cargo flag; every
  # other row is the bare crate plus the listed feature(s).
  args="--no-default-features"
  [ "$name" = "full" ] && args=""
  [ "$name" = "all-features" ] && args="--all-features"
  if [ -n "$flags" ]; then
    # shellcheck disable=SC2086 # the flags string is a deliberate word list
    run_row "$name" check --lib $args $flags
  else
    run_row "$name" check --lib $args
  fi
done

for row in "${BIN_CONFIGS[@]}"; do
  name="${row%%|*}"
  flags="${row#*|}"
  args="--no-default-features"
  [ "$name" = "bin(full)" ] && args=""
  if [ -n "$flags" ]; then
    # shellcheck disable=SC2086
    run_row "$name" check --bin pir $args $flags
  else
    run_row "$name" check --bin pir $args
  fi
done

# Consumer pins: each `tests/sdk_consumer_*.rs` is checked against the bare
# crate PLUS the optional `sdk` feature — `pir::sdk` is not compiled without it
# (there is no stub), and that is the embedder shape the docs advertise. The
# bare-crate `minimal` row above deliberately does not compile these targets.
# Discovery is a glob so a new pin row needs no edit here. Lane regression
# suites (`*_lane_*`, e.g. sdk_consumer_lane_f.rs) are not public-surface pins;
# the ordinary test shards still compile and run them, but they do not add a
# matrix row (the documented pins are named in docs/sdk.md).
shopt -s nullglob
consumers=()
for f in tests/sdk_consumer_*.rs; do
  case "$(basename "$f")" in
    *_lane_*) continue ;;
  esac
  consumers+=("$f")
done
shopt -u nullglob
if [ "${#consumers[@]}" -eq 0 ]; then
  printf '%-28s ' "consumer:*"
  echo "MISSING (no tests/sdk_consumer_*.rs — the surface pins disappeared)"
  FAILED_ROWS+=("consumer:*")
else
  if [ "${#consumers[@]}" -ne "$EXPECTED_CONSUMER_ROWS" ]; then
    printf '%-28s ' "consumer:count"
    echo "MISMATCH (${#consumers[@]}/$EXPECTED_CONSUMER_ROWS tests/sdk_consumer_*.rs — update the header)"
    FAILED_ROWS+=("consumer:count")
  fi
  for t in "${consumers[@]}"; do
    run_row "consumer:$(basename "$t" .rs)" check --no-default-features --features sdk --test "$(basename "$t" .rs)"
  done
fi

# Runtime smoke: a profile that only compiles is not usable (north star R5).
# `cargo test --test feature_off_surface` builds the `pir` binary for the same
# feature set through CARGO_BIN_EXE_pir, then the test runs `--version`, the
# documented off-error for a surface the profile lacks, and the local-mock
# print turn when a wire adapter is compiled in. The slim `--tools grep`
# diagnostic is a `#[cfg(not(feature = "grep-find"))]` test in the same target.
# A row may carry an optional third `|test-target` field (default:
# `feature_off_surface`); `off(rpc,export-html)` uses it to run the
# `rpc`-without-`export-html` in-process pin, `tests/rpc_export_off.rs`.
for row in "${SMOKE_CONFIGS[@]}"; do
  name="${row%%|*}"
  rest="${row#*|}"
  flags="${rest%%|*}"
  target="feature_off_surface"
  if [ "$rest" != "${rest#*|}" ]; then
    target="${rest#*|}"
  fi
  args="--no-default-features"
  [ "$name" = "smoke(full)" ] && args=""
  # shellcheck disable=SC2086
  run_row "$name" test $args $flags --test "$target"
done

echo ""
if [ "${#FAILED_ROWS[@]}" -eq 0 ]; then
  echo "FEATURE MATRIX: all $RUN configurations compile (target dir: $TARGET_DIR)"
  exit 0
fi
echo "FEATURE MATRIX: FAILED — ${#FAILED_ROWS[@]} of $RUN row(s) did not compile:"
for r in "${FAILED_ROWS[@]}"; do echo "  FAIL $r"; done
exit 1
