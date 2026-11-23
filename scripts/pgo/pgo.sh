#!/usr/bin/env bash
# PGO pipeline for pir: instrumented build -> workload -> merge -> optimized
# rebuild, with the QuickJS C runtime (GCC gcov) and the Rust host (LLVM
# instrprof) profiled independently.
#
# Why two formats: rquickjs-sys compiles QuickJS with the system C compiler, so
# its counters are GCC gcov (.gcda); the Rust crates are compiled by rustc, so
# their counters are LLVM .profraw. The two cannot be merged into one file and
# GCC reads its own directory in place. See docs/pgo.md.
#
# All output stays under target/pgo/ (gitignored). The normal `target/` tree and
# the default `cargo build` are never touched.
#
# Usage (Git Bash):
#   scripts/pgo/pgo.sh                 # stage0..stage4
#   scripts/pgo/pgo.sh stage1          # one stage (or stage0/stage2/stage3/stage4)
#   scripts/pgo/pgo.sh --dry-run       # print commands and environment only
#   scripts/pgo/pgo.sh --help
#
# Env knobs:
#   PIR_PGO_TOOLCHAIN     rustup toolchain (stable-x86_64-pc-windows-gnu)
#   PIR_PGO_RUNS          workload repetitions in stage2 (3)
#   PIR_PGO_WORKLOAD_MS   wall time per workload run (3000)
#   PIR_PGO_PROFILER_RT   libprofiler_rt.a for windows-gnu (see below)
#   PIR_PGO_SKIP_RUST=1   C-only PGO (Rust profile flags omitted)
#   PIR_PGO_SKIP_C=1      Rust-only PGO (GCC flags omitted)
#   PIR_PGO_KEEP_PRODUCT=1 keep target/pgo/product across stage0
#   MINGW_BIN             mingw64 bin dir (../tools/mingw/mingw64/bin)
#   CARGO                 cargo binary (cargo)
#
# windows-gnu note: the rustup GNU target does not ship libprofiler_builtins,
# so -Cprofile-generate fails with E0463 unless PIR_PGO_PROFILER_RT points at a
# compatible libprofiler_rt.a (build from compiler-builtins). With it the script
# uses the experimental -Zno-profiler-runtime path (RUSTC_BOOTSTRAP=1). The
# MSVC toolchain ships profiler_builtins but cannot use GCC C flags; use it for
# Rust-only PGO on a machine with the Windows SDK.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CARGO="${CARGO:-cargo}"
TOOLCHAIN="${PIR_PGO_TOOLCHAIN:-stable-x86_64-pc-windows-gnu}"
if [ -n "${PIR_PGO_TARGET_TRIPLE:-}" ]; then
  TARGET_TRIPLE="$PIR_PGO_TARGET_TRIPLE"
elif [[ "$TOOLCHAIN" == *msvc* ]]; then
  TARGET_TRIPLE="x86_64-pc-windows-msvc"
else
  TARGET_TRIPLE="x86_64-pc-windows-gnu"
fi
RUNS="${PIR_PGO_RUNS:-3}"
WORKLOAD_MS="${PIR_PGO_WORKLOAD_MS:-3000}"
KEEP_PRODUCT="${PIR_PGO_KEEP_PRODUCT:-0}"

PGO_ROOT="$ROOT/target/pgo"
PRODUCT_DIR="$PGO_ROOT/product"
RUST_PROF="$PGO_ROOT/rust-prof"
C_PROF="$PGO_ROOT/c-prof"
OUT="$PGO_ROOT/out"
WORKLOAD="$ROOT/scripts/pgo/workload.js"
PIR_EXE="$PRODUCT_DIR/release-fast/pir.exe"
MERGED="$OUT/merged.profdata"
MINGW_BIN="${MINGW_BIN:-$ROOT/../tools/mingw/mingw64/bin}"

DRY_RUN=0
ONLY_STAGE="all"
for arg in "$@"; do
  case "$arg" in
    --dry-run|-n) DRY_RUN=1 ;;
    --help|-h)
      sed -n '1,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    stage0|stage1|stage2|stage3|stage4|all) ONLY_STAGE="$arg" ;;
    *)
      echo "pgo.sh: unknown argument '$arg' (try --help)" >&2
      exit 2
      ;;
  esac
done

# MSYS/Git Bash: native tools (cargo, gcc, llvm-profdata) receive environment
# values verbatim, so turn /c/... into C:/... before putting a path in an env
# var. Command-line arguments are converted by MSYS itself; these are for env.
to_win() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -m "$1"
  else
    printf '%s' "$1"
  fi
}
say() { printf '\n==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() { printf '\npgo.sh: %s\n' "$*" >&2; exit 2; }

print_env() {
  local key val
  for key in "$@"; do
    case "$key" in
      *-*) val="<set via env, hyphenated name>" ;;
      *) val="${!key-<unset>}" ;;
    esac
    printf '    %-30s %s\n' "$key" "$val"
  done
}

run_cmd() {
  printf '    +'
  printf ' %q' "$@"
  printf '\n'
  if [ "$DRY_RUN" -eq 0 ]; then
    "$@"
  fi
}

on_error() {
  local rc=$?
  printf '\npgo.sh: failed at line %s (exit %s)\n' "${BASH_LINENO[0]:-?}" "$rc" >&2
  printf 'Artifacts: %s\n' "$PGO_ROOT" >&2
  printf 'Diagnostics: run `scripts/pgo/pgo.sh --dry-run`, and see docs/pgo.md.\n' >&2
  exit "$rc"
}
trap on_error ERR

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

command -v "$CARGO" >/dev/null 2>&1 || die "cargo not found: $CARGO"
command -v rustup >/dev/null 2>&1 || die "rustup not found"
case "$ROOT" in
  *" "*) die "the repository path contains a space ($ROOT); RUSTFLAGS/CFLAGS split on whitespace. Move the checkout or use CARGO_ENCODED_RUSTFLAGS manually." ;;
esac

if [[ "$TOOLCHAIN" == *gnu* ]]; then
  if [ -x "$MINGW_BIN/gcc.exe" ]; then
    export PATH="$MINGW_BIN:$PATH"
  elif ! command -v gcc >/dev/null 2>&1; then
    die "mingw gcc not found (looked in $MINGW_BIN). Set MINGW_BIN to mingw64/bin."
  fi
fi

RUSTC_SYSROOT="$(rustup run "$TOOLCHAIN" rustc --print sysroot)"
HOST_TRIPLE="$(rustup run "$TOOLCHAIN" rustc -vV | awk '/^host:/{print $2}')"
RUSTC_VERSION="$(rustup run "$TOOLCHAIN" rustc --version)"
LLVM_VERSION="$(rustup run "$TOOLCHAIN" rustc -vV | awk '/^LLVM version:/{print $3}')"
PROFDATA="$RUSTC_SYSROOT/lib/rustlib/$HOST_TRIPLE/bin/llvm-profdata.exe"
if [ ! -x "$PROFDATA" ]; then
  PROFDATA="$(command -v llvm-profdata || true)"
fi
[ -n "$PROFDATA" ] && [ -x "$PROFDATA" ] || \
  die "llvm-profdata not found. Run: rustup component add llvm-tools-$HOST_TRIPLE"

SKIP_RUST="${PIR_PGO_SKIP_RUST:-0}"
SKIP_C="${PIR_PGO_SKIP_C:-0}"
RUST_PROFILE_RUNTIME_ARGS=()
if [[ "$TOOLCHAIN" == *msvc* ]]; then
  # cl.exe has no -fprofile-generate; GCC's flag would be rejected. Rust PGO
  # still works because MSVC rustup targets ship profiler_builtins.
  if [ "$SKIP_C" != "1" ]; then
    note "toolchain is MSVC: C/QuickJS PGO disabled (use windows-gnu + mingw gcc for it)"
  fi
  SKIP_C=1
fi

if [ "$SKIP_RUST" != "1" ]; then
  PROFILER_RLIB=""
  for candidate in "$RUSTC_SYSROOT/lib/rustlib/$TARGET_TRIPLE/lib"/libprofiler_builtins-*.rlib; do
    if [ -e "$candidate" ]; then
      PROFILER_RLIB="$candidate"
      break
    fi
  done
  if [ -z "$PROFILER_RLIB" ]; then
    if [ -n "${PIR_PGO_PROFILER_RT:-}" ]; then
      [ -f "$PIR_PGO_PROFILER_RT" ] || die "PIR_PGO_PROFILER_RT not found: $PIR_PGO_PROFILER_RT"
      RUST_PROFILE_RUNTIME_ARGS=(-Zno-profiler-runtime "-Clink-arg=$(to_win "$PIR_PGO_PROFILER_RT")")
      export RUSTC_BOOTSTRAP=1
      note "profiler_builtins: missing; using PIR_PGO_PROFILER_RT=$PIR_PGO_PROFILER_RT (-Zno-profiler-runtime)"
    else
      cat >&2 <<EOF

pgo.sh: Rust PGO is not possible with $TOOLCHAIN as installed.

  $TARGET_TRIPLE does not ship libprofiler_builtins (rustc would fail with
  error[E0463]: can't find crate for \`profiler_builtins\`).

Pick one:
  1) Run this pipeline on MSVC (Windows SDK + VS Build Tools installed):
       PIR_PGO_TOOLCHAIN=stable-x86_64-pc-windows-msvc PIR_PGO_SKIP_C=1 scripts/pgo/pgo.sh
     MSVC ships profiler_builtins; C/QuickJS gcov PGO is skipped because cl.exe
     has no -fprofile-generate.
  2) Keep windows-gnu and point PIR_PGO_PROFILER_RT at a compatible
     libprofiler_rt.a built from compiler-builtins:
       PIR_PGO_PROFILER_RT=C:/path/libprofiler_rt.a scripts/pgo/pgo.sh
     (The script then adds -Zno-profiler-runtime with RUSTC_BOOTSTRAP=1.)
  3) Do C-only PGO now, Rust PGO later:
       PIR_PGO_SKIP_RUST=1 scripts/pgo/pgo.sh

Not falling through to a build that is guaranteed to fail.
EOF
      exit 2
    fi
  else
    RUST_PROFILE_RUNTIME_ARGS=()
    note "profiler_builtins: $PROFILER_RLIB"
  fi
fi

GCOV_LIB=""
if [ "$SKIP_C" != "1" ]; then
  GCOV_LIB="$(find "$(dirname "$(command -v gcc)")/../lib" -name libgcov.a 2>/dev/null | head -n 1 || true)"
  [ -n "$GCOV_LIB" ] || die "libgcov.a not found next to the mingw gcc; cannot link the GCC profile runtime"
  GCOV_LIB_DIR="$(dirname "$GCOV_LIB")"
  GCOV_VERSION="$("$(command -v gcc)" --version | head -n 1)"
  note "gcov runtime: $GCOV_LIB"
fi

note "repo:        $ROOT"
note "toolchain:   $TOOLCHAIN ($RUSTC_VERSION)"
note "LLVM:        $LLVM_VERSION"
note "llvm-profdata: $PROFDATA"
note "C compiler:  $(command -v gcc 2>/dev/null || echo '<none>')"
note "artifact dir: $PGO_ROOT"

# C flags. rquickjs-sys's build.rs overwrites CFLAGS on Windows (it sets
# -DWIN32_LEAN_AND_MEAN -std=c11 in-process), so the generic variable alone is
# NOT enough. cc 1.4 also reads CFLAGS_<target>; those survive and are applied
# last by the cc crate.
TARGET_ENV_TAG="${TARGET_TRIPLE//-/_}"
C_GEN_FLAGS="-fprofile-generate=$(to_win "$C_PROF") -O3"
C_USE_FLAGS="-fprofile-use=$(to_win "$C_PROF") -fprofile-correction -O3"
CFLAGS_EXTRA_HYPHEN="CFLAGS_$TARGET_TRIPLE"
CFLAGS_EXTRA_UNDERSCORE="CFLAGS_$TARGET_ENV_TAG"

# Rust flags. LLVM's instrprof runtime and GCC's gcov runtime are separate;
# -lgcov is needed because instrumented QuickJS/SQLite objects reference
# __gcov_* symbols, and rustc (not gcc) performs the final link.
RUST_GEN_FLAGS="-Cprofile-generate=$(to_win "$RUST_PROF") -Cllvm-args=-pgo-warn-missing-function"
RUST_USE_FLAGS="-Cprofile-use=$(to_win "$MERGED") -Cllvm-args=-pgo-warn-missing-function"
if [ "${#RUST_PROFILE_RUNTIME_ARGS[@]}" -gt 0 ]; then
  RUST_GEN_FLAGS="$RUST_GEN_FLAGS ${RUST_PROFILE_RUNTIME_ARGS[*]}"
fi
GCOV_LINK_FLAGS=""
if [ "$SKIP_C" != "1" ]; then
  GCOV_LINK_FLAGS="-Clink-arg=-L$(to_win "$GCOV_LIB_DIR") -Clink-arg=-lgcov"
  RUST_GEN_FLAGS="$RUST_GEN_FLAGS $GCOV_LINK_FLAGS"
fi
if [ -n "${PIR_PGO_EXTRA_RUSTFLAGS:-}" ]; then
  RUST_GEN_FLAGS="$RUST_GEN_FLAGS $PIR_PGO_EXTRA_RUSTFLAGS"
  RUST_USE_FLAGS="$RUST_USE_FLAGS $PIR_PGO_EXTRA_RUSTFLAGS"
fi

# ---------------------------------------------------------------------------
# Stage 0: clean artifact directories
# ---------------------------------------------------------------------------

stage0() {
  say "stage0: clean target/pgo"
  note "rust profiles: $RUST_PROF"
  note "C profiles:    $C_PROF"
  note "outputs:       $OUT"
  if [ "$DRY_RUN" -eq 0 ]; then
    rm -rf "$RUST_PROF" "$C_PROF" "$OUT"
    mkdir -p "$RUST_PROF" "$C_PROF" "$OUT/agent" "$OUT/workload-tmp"
    if [ "$KEEP_PRODUCT" != "1" ]; then
      rm -rf "$PRODUCT_DIR"
    fi
  else
    printf '    + rm -rf %q %q %q\n' "$RUST_PROF" "$C_PROF" "$OUT"
    [ "$KEEP_PRODUCT" = "1" ] || printf '    + rm -rf %q\n' "$PRODUCT_DIR"
  fi
  note "stage0 done"
}

# ---------------------------------------------------------------------------
# Stage 1: instrumented build
# ---------------------------------------------------------------------------

stage1() {
  say "stage1: instrumented build (release-fast, features=extensions)"
  print_env RUSTFLAGS CFLAGS "$CFLAGS_EXTRA_HYPHEN" "$CFLAGS_EXTRA_UNDERSCORE" \
    LDFLAGS CARGO_TARGET_DIR RUSTC_BOOTSTRAP LLVM_PROFILE_FILE
  if [ "$SKIP_RUST" = "1" ]; then
    note "RUSTFLAGS=${GCOV_LINK_FLAGS:-<unset>} (C-only: libgcov link only)"
  else
    note "RUSTFLAGS=$RUST_GEN_FLAGS"
  fi
  note "CFLAGS=$C_GEN_FLAGS"
  note "LDFLAGS=$( [ "$SKIP_C" = "1" ] && echo '<unset>' || echo "-fprofile-generate=$(to_win "$C_PROF")" )"
  note "RUSTC_BOOTSTRAP=${RUSTC_BOOTSTRAP:-<unset>}"
  note "CARGO_TARGET_DIR=$(to_win "$PRODUCT_DIR")"
  if [ "$SKIP_RUST" = "1" ]; then
    note "PIR_PGO_SKIP_RUST=1: omitting -Cprofile-generate (C-only PGO)"
  fi
  if [ "$SKIP_C" = "1" ]; then
    note "PIR_PGO_SKIP_C=1: omitting GCC -fprofile-generate"
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    printf '    + cargo +%s build --profile release-fast --bin pir --features extensions\n' "$TOOLCHAIN"
    return
  fi

  # One subshell keeps these flags out of stages 2-4.
  (
    if [ "$SKIP_RUST" != "1" ]; then
      export RUSTFLAGS="$RUST_GEN_FLAGS"
    elif [ -n "$GCOV_LINK_FLAGS" ]; then
      # C-only PGO: the C objects still reference __gcov_*, so the final
      # rustc link needs libgcov even without Rust instrumentation.
      export RUSTFLAGS="$GCOV_LINK_FLAGS"
    fi
    if [ "$SKIP_C" != "1" ]; then
      export CFLAGS="$C_GEN_FLAGS"
      export "$CFLAGS_EXTRA_UNDERSCORE=$C_GEN_FLAGS"
      # Env names with '-' cannot be exported by bash; pass the hyphen form
      # explicitly on the command line below.
      export LDFLAGS="-fprofile-generate=$(to_win "$C_PROF")"
    fi
    export CARGO_TARGET_DIR="$(to_win "$PRODUCT_DIR")"
    if [ "$SKIP_C" = "1" ]; then
      run_cmd "$CARGO" "+$TOOLCHAIN" build --profile release-fast --bin pir --features extensions
    else
      run_cmd env "$CFLAGS_EXTRA_HYPHEN=$C_GEN_FLAGS" \
        "$CARGO" "+$TOOLCHAIN" build --profile release-fast --bin pir --features extensions
    fi
  )

  [ -f "$PIR_EXE" ] || die "instrumented binary missing: $PIR_EXE"
  note "instrumented binary: $PIR_EXE ($(stat -c %s "$PIR_EXE" 2>/dev/null || echo '?') bytes)"
}

# ---------------------------------------------------------------------------
# Stage 2: profile collection
# ---------------------------------------------------------------------------

stage2() {
  say "stage2: workload x$RUNS (>= ${WORKLOAD_MS}ms each)"
  [ "$DRY_RUN" -eq 1 ] || [ -f "$PIR_EXE" ] || die "missing instrumented binary $PIR_EXE (run stage1)"
  note "workload: $WORKLOAD"
  note "agent dir: $OUT/agent"
  note "scratch:   $OUT/workload-tmp"
  print_env PIR_AGENT_DIR PIR_PGO_TMP LLVM_PROFILE_FILE

  local i log ok=0
  for i in $(seq 1 "$RUNS"); do
    log="$OUT/workload-$i.log"
    note "run $i/$RUNS -> $log"
    if [ "$DRY_RUN" -eq 1 ]; then
      printf '    + PIR_AGENT_DIR=%q PIR_PGO_TMP=%q LLVM_PROFILE_FILE=%q %q js %q\n' \
        "$(to_win "$OUT/agent")" "$(to_win "$OUT/workload-tmp")" \
        "$(to_win "$RUST_PROF")/pir-%p-%m.profraw" "$PIR_EXE" "$WORKLOAD"
      continue
    fi
    printf '    + PIR_AGENT_DIR=%q PIR_PGO_TMP=%q LLVM_PROFILE_FILE=%q %q js %q\n' \
      "$(to_win "$OUT/agent")" "$(to_win "$OUT/workload-tmp")" \
      "$(to_win "$RUST_PROF")/pir-%p-%m.profraw" "$PIR_EXE" "$WORKLOAD"
    (
      export PIR_AGENT_DIR="$(to_win "$OUT/agent")"
      export PIR_PGO_TMP="$(to_win "$OUT/workload-tmp")"
      export PIR_PGO_WORKLOAD_MS="$WORKLOAD_MS"
      export LLVM_PROFILE_FILE="$(to_win "$RUST_PROF")/pir-%p-%m.profraw"
      "$PIR_EXE" js "$WORKLOAD" >"$log" 2>&1
    )
    if grep -q '^PGO_WORKLOAD_OK ' "$log"; then
      ok=$((ok + 1))
      grep '^PGO_WORKLOAD_OK ' "$log" | tail -n 1 | sed 's/^/    /'
    else
      tail -n 20 "$log" >&2 || true
      die "workload run $i did not print PGO_WORKLOAD_OK (log: $log)"
    fi
  done
  if [ "$DRY_RUN" -eq 1 ]; then
    if [ -n "${PIR_PGO_EXTRA_JS:-}" ]; then
      printf '    + PIR_AGENT_DIR=%q LLVM_PROFILE_FILE=%q %q js %q\n' \
        "$(to_win "$OUT/agent")" "$(to_win "$RUST_PROF")/extra-%p-%m.profraw" \
        "$PIR_EXE" "$PIR_PGO_EXTRA_JS"
    fi
    return 0
  fi

  # Optional real entry point (for example an extracted on-disk plugin fixture).
  # The repository has no standalone plugin file (tests build their entries
  # from inline Rust strings), so this stays opt-in. It runs under the same
  # instrumented binary and contributes to both profiles.
  if [ -n "${PIR_PGO_EXTRA_JS:-}" ]; then
    [ -f "$PIR_PGO_EXTRA_JS" ] || die "PIR_PGO_EXTRA_JS not found: $PIR_PGO_EXTRA_JS"
    note "extra JS entry: $PIR_PGO_EXTRA_JS"
    (
      export PIR_AGENT_DIR="$(to_win "$OUT/agent")"
      export PIR_PGO_TMP="$(to_win "$OUT/workload-tmp")"
      export LLVM_PROFILE_FILE="$(to_win "$RUST_PROF")/extra-%p-%m.profraw"
      "$PIR_EXE" js "$PIR_PGO_EXTRA_JS" >"$OUT/extra-js.log" 2>&1
    ) || die "extra JS entry failed (log: $OUT/extra-js.log)"
  fi

  note "successful workload runs: $ok/$RUNS"

  local profraw_count gcda_count
  profraw_count=$(find "$RUST_PROF" -name '*.profraw' | wc -l | tr -d ' ')
  gcda_count=$(find "$C_PROF" -name '*.gcda' | wc -l | tr -d ' ')
  note "rust .profraw files: $profraw_count"
  note "gcc .gcda files:     $gcda_count"

  if [ "$SKIP_RUST" != "1" ] && [ "$profraw_count" -eq 0 ]; then
    die "no Rust .profraw produced under $RUST_PROF. The instrumented binary was not used (or crashed before exit); check $OUT/workload-*.log."
  fi
  if [ "$SKIP_C" != "1" ] && [ "$gcda_count" -eq 0 ]; then
    die "no GCC .gcda produced under $C_PROF. CFLAGS was overridden (remember rquickjs-sys clobbers CFLAGS; CFLAGS_$TARGET_ENV_TAG must be set) or the process did not exit normally."
  fi
  note "stage2 done"
}

# ---------------------------------------------------------------------------
# Stage 3: merge / validate profiles
# ---------------------------------------------------------------------------

stage3() {
  say "stage3: merge and validate profiles"
  if [ "$SKIP_RUST" != "1" ]; then
    print_env PIR_PGO_PROFILER_RT
    note "llvm-profdata: $PROFDATA"
    if [ "$DRY_RUN" -eq 1 ]; then
      printf '    + %q merge -o %q %q/*.profraw\n' "$PROFDATA" "$MERGED" "$RUST_PROF"
    else
      shopt -s nullglob
      local profraws=("$RUST_PROF"/*.profraw)
      shopt -u nullglob
      [ "${#profraws[@]}" -gt 0 ] || die "no .profraw files to merge under $RUST_PROF"
      note "merging ${#profraws[@]} file(s)"
      run_cmd "$PROFDATA" merge -o "$MERGED" "${profraws[@]}"
      [ -s "$MERGED" ] || die "merged profile is empty: $MERGED"
      note "merged: $MERGED ($(stat -c %s "$MERGED" 2>/dev/null || echo '?') bytes)"
      if "$PROFDATA" show --detailed-summary "$MERGED" 2>/dev/null | head -n 12 | sed 's/^/    /'; then
        :
      fi
    fi
  else
    note "PIR_PGO_SKIP_RUST=1: no Rust merge"
  fi

  if [ "$SKIP_C" != "1" ]; then
    note "GCC profiles stay in place; -fprofile-use=$C_PROF reads them directly."
    if [ "$DRY_RUN" -eq 0 ]; then
      local gcda_count
      gcda_count=$(find "$C_PROF" -name '*.gcda' | wc -l | tr -d ' ')
      [ "$gcda_count" -gt 0 ] || die "no .gcda under $C_PROF"
      note "gcda files: $gcda_count"
      if command -v gcov-tool >/dev/null 2>&1; then
        note "gcov-tool: $(gcov-tool --version 2>&1 | head -n 1)"
      fi
    fi
  fi
  note "stage3 done"
}

# ---------------------------------------------------------------------------
# Stage 4: profile-use rebuild
# ---------------------------------------------------------------------------

stage4() {
  say "stage4: optimized rebuild (profile-use)"
  if [ "$SKIP_RUST" != "1" ]; then
    [ "$DRY_RUN" -eq 1 ] || [ -s "$MERGED" ] || die "missing merged profile $MERGED (run stage3)"
  fi
  print_env RUSTFLAGS CFLAGS "$CFLAGS_EXTRA_HYPHEN" "$CFLAGS_EXTRA_UNDERSCORE" \
    LDFLAGS CARGO_TARGET_DIR
  note "RUSTFLAGS=$RUST_USE_FLAGS"
  note "CFLAGS=$C_USE_FLAGS"
  if [ "$SKIP_RUST" = "1" ]; then
    note "PIR_PGO_SKIP_RUST=1: no -Cprofile-use (C-only PGO)"
  fi
  note "CARGO_TARGET_DIR=$(to_win "$PRODUCT_DIR") (same as stage1: GCC .gcda names encode object paths)"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf '    + cargo +%s build --profile release-fast --bin pir --features extensions\n' "$TOOLCHAIN"
    return
  fi

  # Deliberately NOT the same CARGO_TARGET_DIR as target/: the normal
  # release-fast artifact must stay untouched.
  (
    if [ "$SKIP_RUST" != "1" ]; then
      export RUSTFLAGS="$RUST_USE_FLAGS"
    fi
    if [ "$SKIP_C" != "1" ]; then
      export CFLAGS="$C_USE_FLAGS"
      export "$CFLAGS_EXTRA_UNDERSCORE=$C_USE_FLAGS"
    fi
    export CARGO_TARGET_DIR="$(to_win "$PRODUCT_DIR")"
    if [ "$SKIP_C" = "1" ]; then
      run_cmd "$CARGO" "+$TOOLCHAIN" build --profile release-fast --bin pir --features extensions
    else
      run_cmd env "$CFLAGS_EXTRA_HYPHEN=$C_USE_FLAGS" \
        "$CARGO" "+$TOOLCHAIN" build --profile release-fast --bin pir --features extensions
    fi
  )

  [ -f "$PIR_EXE" ] || die "PGO binary missing after build: $PIR_EXE"
  cp -f "$PIR_EXE" "$OUT/pir-pgo.exe"
  note "PGO binary: $OUT/pir-pgo.exe ($(stat -c %s "$OUT/pir-pgo.exe" 2>/dev/null || echo '?') bytes)"

  # Manifest: profiles are only valid for this exact compiler + source + flags.
  {
    printf 'pir PGO manifest\n'
    printf 'generated_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'git_head=%s\n' "$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
    if [ -n "$(git -C "$ROOT" status --porcelain 2>/dev/null)" ]; then
      printf 'git_dirty=yes\n'
    else
      printf 'git_dirty=no\n'
    fi
    printf 'toolchain=%s\n' "$TOOLCHAIN"
    printf 'rustc=%s\n' "$RUSTC_VERSION"
    printf 'llvm=%s\n' "$LLVM_VERSION"
    printf 'llvm_profdata=%s\n' "$PROFDATA"
    printf 'gcc=%s\n' "${GCOV_VERSION:-<not used>}"
    printf 'host=%s\n' "$HOST_TRIPLE"
    printf 'target=%s\n' "$TARGET_TRIPLE"
    printf 'cpu=%s\n' "${PROCESSOR_IDENTIFIER:-$(uname -m)}"
    printf 'profile=release-fast\n'
    printf 'features=extensions\n'
    printf 'rustflags_generate=%s\n' "$RUST_GEN_FLAGS"
    printf 'rustflags_use=%s\n' "$RUST_USE_FLAGS"
    printf 'cflags_generate=%s\n' "$C_GEN_FLAGS"
    printf 'cflags_use=%s\n' "$C_USE_FLAGS"
    printf 'workload_runs=%s\n' "$RUNS"
    printf 'workload_ms=%s\n' "$WORKLOAD_MS"
    printf 'profraw_count=%s\n' "$(find "$RUST_PROF" -name '*.profraw' | wc -l | tr -d ' ')"
    printf 'gcda_count=%s\n' "$(find "$C_PROF" -name '*.gcda' | wc -l | tr -d ' ')"
    printf 'pir_sha256=%s\n' "$(sha256sum "$OUT/pir-pgo.exe" 2>/dev/null | cut -d' ' -f1 || echo unknown)"
  } >"$OUT/profile-manifest.txt"
  note "manifest: $OUT/profile-manifest.txt"
  note "A/B: scripts/pgo/compare.sh"
  note "stage4 done"
}

case "$ONLY_STAGE" in
  stage0) stage0 ;;
  stage1) stage1 ;;
  stage2) stage2 ;;
  stage3) stage3 ;;
  stage4) stage4 ;;
  all) stage0; stage1; stage2; stage3; stage4 ;;
esac
