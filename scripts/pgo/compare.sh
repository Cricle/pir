#!/usr/bin/env bash
# A/B the 51-operation hot-path corpus in tests/perf_parity between the normal
# release-fast build and the PGO (profile-use) build produced by pgo.sh.
#
# It runs the SAME test twice on the SAME machine:
#   baseline: target/pgo/baseline  (no profile flags; isolated from target/)
#   pgo:      target/pgo/product   (the -Cprofile-use/-fprofile-use build)
# Each invocation sets PIR_PERF_RUNS (=3 by default) and the harness takes the
# min of those runs per operation. Both engines (pir and node) are measured in
# both invocations; the table reports absolute pir ns/op plus the PIR speedup,
# so a drifting node column cannot masquerade as a PIR win.
#
# Nothing is built here beyond what `cargo test` needs. This script is not run
# as part of the pipeline; run it manually after `scripts/pgo/pgo.sh`.
#
# Env:
#   PIR_PERF_RUNS=3                 runs per engine inside the harness
#   PIR_PERF_ONLY=<substr>          filter operations (passed through)
#   PIR_PERF_THRESHOLD=0            harness gate (0 = report only)
#   PIR_PGO_AB_MIN_SPEEDUP=0        fail if geomean speedup is below this
#   PIR_PGO_AB_EXPECTED=51          expected operation count
#   PIR_PGO_BASELINE_TARGET_DIR=target/pgo/baseline
#   PIR_PGO_TOOLCHAIN=stable-x86_64-pc-windows-gnu
#   NODE_BIN_DIR=<dir with node.exe>
#   MINGW_BIN=<mingw64/bin>
#
# Usage: scripts/pgo/compare.sh [--dry-run] [--help]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CARGO="${CARGO:-cargo}"
TOOLCHAIN="${PIR_PGO_TOOLCHAIN:-stable-x86_64-pc-windows-gnu}"
PGO_ROOT="$ROOT/target/pgo"
PRODUCT_DIR="$PGO_ROOT/product"
BASELINE_DIR="${PIR_PGO_BASELINE_TARGET_DIR:-$PGO_ROOT/baseline}"
C_PROF="$PGO_ROOT/c-prof"
MERGED="$PGO_ROOT/out/merged.profdata"
PIR_EXE="$PRODUCT_DIR/release-fast/pir.exe"
AB_DIR="$PGO_ROOT/ab"
RUNS="${PIR_PERF_RUNS:-3}"
EXPECTED="${PIR_PGO_AB_EXPECTED:-51}"
MIN_SPEEDUP="${PIR_PGO_AB_MIN_SPEEDUP:-0}"
SKIP_RUST="${PIR_PGO_SKIP_RUST:-0}"
SKIP_C="${PIR_PGO_SKIP_C:-0}"
if [[ "$TOOLCHAIN" == *msvc* ]]; then SKIP_C=1; fi
MINGW_BIN="${MINGW_BIN:-$ROOT/../tools/mingw/mingw64/bin}"
NODE_BIN_DIR="${NODE_BIN_DIR:-/c/wp/empty/tools/node-v24.22.1-win-x64}"

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run|-n) DRY_RUN=1 ;;
    --help|-h)
      sed -n '1,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "compare.sh: unknown argument '$arg' (try --help)" >&2; exit 2 ;;
  esac
done

to_win() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi
}
note() { printf '    %s\n' "$*"; }
die() { printf '\ncompare.sh: %s\n' "$*" >&2; exit 2; }

command -v "$CARGO" >/dev/null 2>&1 || die "cargo not found: $CARGO"

if [ -x "$MINGW_BIN/gcc.exe" ]; then
  export PATH="$MINGW_BIN:$PATH"
fi
if [ -d "$NODE_BIN_DIR" ] && [ -f "$NODE_BIN_DIR/node.exe" ]; then
  export PATH="$NODE_BIN_DIR:$PATH"
fi
if ! command -v node >/dev/null 2>&1 && [ "$DRY_RUN" -eq 0 ]; then
  die "node not found on PATH; set NODE_BIN_DIR to the directory containing node.exe"
fi

printf '\n==> compare: baseline vs PGO (release-fast, features=extensions)\n'
note "baseline target: $BASELINE_DIR"
note "pgo target:      $PRODUCT_DIR"
note "PIR_PERF_RUNS:   $RUNS"
note "node:            $(command -v node 2>/dev/null || echo '<missing>')"
note "cargo:           $(command -v "$CARGO") +$TOOLCHAIN"

if [ "$DRY_RUN" -eq 0 ]; then
  [ -f "$PIR_EXE" ] || die "PGO binary missing: $PIR_EXE (run scripts/pgo/pgo.sh stage4)"
  if [ "$SKIP_C" != "1" ]; then
    [ -d "$C_PROF" ] || die "C profile dir missing: $C_PROF"
  fi
  if [ "$SKIP_RUST" != "1" ] && [ -z "${PIR_PERF_ONLY:-}" ]; then
    [ -s "$MERGED" ] || die "merged Rust profile missing: $MERGED (run pgo.sh stage3)"
  fi
  mkdir -p "$AB_DIR"
fi

# Never let an inherited profile flag leak into the baseline, and never let
# CARGO_ENCODED_RUSTFLAGS override the flags set here (it wins over RUSTFLAGS).
unset CARGO_ENCODED_RUSTFLAGS

BASE_LOG="$AB_DIR/baseline.txt"
PGO_LOG="$AB_DIR/pgo.txt"

run_baseline() {
  local cmd=(
    "$CARGO" "+$TOOLCHAIN" test --profile release-fast --features extensions
    --test perf_parity -- --ignored --nocapture
  )
  printf '    + (baseline)'
  printf ' %q' "${cmd[@]}"
  printf '\n'
  if [ "$DRY_RUN" -eq 1 ]; then return 0; fi
  (
    # The baseline must be a plain release-fast build. Profile flags are
    # cleared explicitly; the isolated target dir keeps it away from target/.
    unset RUSTFLAGS CFLAGS LDFLAGS CFLAGS_x86_64_pc_windows_gnu
    export CARGO_TARGET_DIR="$(to_win "$BASELINE_DIR")"
    export PIR_PERF_RUNS="$RUNS"
    export PIR_PERF_THRESHOLD="${PIR_PERF_THRESHOLD:-0}"
    env -u 'CFLAGS_x86_64-pc-windows-gnu' "${cmd[@]}" 2>&1 | tee "$BASE_LOG"
  )
}

run_pgo() {
  local cflags="-fprofile-use=$(to_win "$C_PROF") -fprofile-correction -O3"
  local rustflags="-Cprofile-use=$(to_win "$MERGED") -Cllvm-args=-pgo-warn-missing-function"
  if [ -n "${PIR_PGO_EXTRA_RUSTFLAGS:-}" ]; then
    rustflags="$rustflags $PIR_PGO_EXTRA_RUSTFLAGS"
  fi
  local cmd=(
    "$CARGO" "+$TOOLCHAIN" test --profile release-fast --features extensions
    --test perf_parity -- --ignored --nocapture
  )
  local envargs=()
  printf '    + (pgo)'
  if [ "$SKIP_RUST" != "1" ]; then
    printf ' RUSTFLAGS=%q' "$rustflags"
    envargs+=("RUSTFLAGS=$rustflags")
  fi
  if [ "$SKIP_C" != "1" ]; then
    printf ' CFLAGS=%q' "$cflags"
    envargs+=("CFLAGS=$cflags" "CFLAGS_x86_64_pc_windows_gnu=$cflags" "CFLAGS_x86_64-pc-windows-gnu=$cflags")
  fi
  printf ' CARGO_TARGET_DIR=%q' "$(to_win "$PRODUCT_DIR")"
  printf ' %q' "${cmd[@]}"
  printf '\n'
  if [ "$DRY_RUN" -eq 1 ]; then return 0; fi
  (
    # Same flags as pgo.sh stage4, so cargo reuses the PGO-built pir.exe and
    # CARGO_BIN_EXE_pir in the test points at it.
    export CARGO_TARGET_DIR="$(to_win "$PRODUCT_DIR")"
    export PIR_PERF_RUNS="$RUNS"
    export PIR_PERF_THRESHOLD="${PIR_PERF_THRESHOLD:-0}"
    env "${envargs[@]}" "${cmd[@]}" 2>&1 | tee "$PGO_LOG"
  )
}

run_baseline
suffix=""
if [ "$DRY_RUN" -eq 1 ]; then suffix=" (dry-run)"; fi
printf '\n==> baseline done%s\n' "$suffix"
run_pgo
suffix=""
if [ "$DRY_RUN" -eq 1 ]; then suffix=" (dry-run)"; fi
printf '\n==> pgo done%s\n' "$suffix"

if [ "$DRY_RUN" -eq 1 ]; then
  printf '\n    + parse %q and %q -> 51-row speedup table\n' "$BASE_LOG" "$PGO_LOG"
  exit 0
fi

parse_rows() {
  awk '
    $1 ~ /^[A-Za-z]/ && $2 ~ /^[0-9]+(\.[0-9]+)?$/ &&
    $3 ~ /^[0-9]+(\.[0-9]+)?$/ && $4 ~ /^[0-9]+(\.[0-9]+)?x$/ {
      ratio = $4; sub(/x$/, "", ratio);
      print $1, $2, $3, ratio
    }
  ' "$1"
}

parse_rows "$BASE_LOG" >"$AB_DIR/baseline.rows"
parse_rows "$PGO_LOG" >"$AB_DIR/pgo.rows"
base_count=$(wc -l <"$AB_DIR/baseline.rows" | tr -d ' ')
pgo_count=$(wc -l <"$AB_DIR/pgo.rows" | tr -d ' ')
note "parsed rows: baseline=$base_count pgo=$pgo_count (expected $EXPECTED)"

if [ "$base_count" -eq 0 ] || [ "$pgo_count" -eq 0 ]; then
  die "no PERF rows parsed; inspect $BASE_LOG and $PGO_LOG"
fi

{
  echo "# operation                    base ns/op    pgo ns/op   speedup   base/node  pgo/node"
  awk '
    FNR == NR {
      # baseline rows: name pir node ratio
      order[++n] = $1; bp[$1] = $2; bn[$1] = $3; br[$1] = $4; next
    }
    {
      pp[$1] = $2; pn[$1] = $3; pr[$1] = $4
    }
    END {
      sumlog = 0; improved = 0; regressed = 0; missing = 0;
      for (i = 1; i <= n; i++) {
        name = order[i];
        if (!(name in pp)) { missing++; continue }
        speed = bp[name] / pp[name];
        sumlog += log(speed);
        if (speed > 1.02) improved++;
        if (speed < 0.98) regressed++;
        printf "%-30s %11.1f %12.1f %8.3fx %9.2fx %9.2fx\n", name, bp[name], pp[name], speed, br[name], pr[name];
      }
      count = n - missing;
      if (count > 0) {
        geo = exp(sumlog / count);
        printf "%s\n", "------------------------------------------------------------------------------------------";
        printf "%-30s %11s %12s %8.3fx %9s %9s\n", "GEOMEAN", "", "", geo, "", "";
        printf "ops=%d improved(>2%%)=%d regressed(>2%%)=%d missing=%d\n", count, improved, regressed, missing;
      }
    }
  ' "$AB_DIR/baseline.rows" "$AB_DIR/pgo.rows"
} | tee "$AB_DIR/ratio-table.txt"

parsed=$(awk 'END { print NR }' "$AB_DIR/baseline.rows")
if [ "$parsed" -ne "$EXPECTED" ]; then
  printf '\ncompare.sh: warning: expected %s operations, parsed %s. The corpus may have changed; update PIR_PGO_AB_EXPECTED.\n' "$EXPECTED" "$parsed" >&2
fi

geo=$(awk '
  FNR == NR { bp[$1] = $2; n++; next }
  { if ($1 in bp) { s += log(bp[$1] / $2); c++ } }
  END { if (c > 0) printf "%.6f", exp(s / c); else printf "0" }
' "$AB_DIR/baseline.rows" "$AB_DIR/pgo.rows")

if [ "$MIN_SPEEDUP" != "0" ]; then
  if awk -v g="$geo" -v m="$MIN_SPEEDUP" 'BEGIN { exit !(g >= m) }'; then
    printf '\ncompare.sh: geomean %.3fx >= required %.3fx\n' "$geo" "$MIN_SPEEDUP"
  else
    printf '\ncompare.sh: geomean %.3fx < required %.3fx\n' "$geo" "$MIN_SPEEDUP" >&2
    exit 3
  fi
else
  printf '\ncompare.sh: report-only (set PIR_PGO_AB_MIN_SPEEDUP=1.05 to gate)\n'
fi

printf 'Raw logs: %s, %s\nTable: %s\n' "$BASE_LOG" "$PGO_LOG" "$AB_DIR/ratio-table.txt"