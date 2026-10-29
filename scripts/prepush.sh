#!/usr/bin/env bash
# scripts/prepush.sh — the manual "is this tree safe to push?" gate.
#
# It runs the same *checks* CI runs (fmt, check, clippy, tests, doctests,
# packaging) locally, in the order that fails fastest. It never runs a git
# command, never commits and never pushes: it only tells you whether the tree
# is ready. Run it with Git Bash on Windows (`bash scripts/prepush.sh`).
#
# Usage:
#   bash scripts/prepush.sh                 # full check (tests + packaging)
#   bash scripts/prepush.sh --fast          # fmt + check + clippy only
#                                           # (packaging still runs unless
#                                           #  --no-package is also given)
#   bash scripts/prepush.sh --parallel      # run the suite via
#                                           # scripts/test_parallel.ps1
#                                           # (PowerShell; local speed tool)
#   bash scripts/prepush.sh --matrix        # also run scripts/feature_matrix.sh
#   bash scripts/prepush.sh --publish-dry-run
#                                           # also `cargo publish --dry-run`
#                                           # (packages + builds the .crate; slow)
#   bash scripts/prepush.sh --no-package    # skip the packaging checks
#   bash scripts/prepush.sh --help
#
# Environment:
#   CARGO_TARGET_DIR   default: target-alt (repo discipline: the eval harness
#                      owns the default target/). Never target-lane-*/.
#   RUST_MIN_STACK     default: 8388608 (the agent tests nest in-process).
#
# Exit code: 0 = every check passed, 1 = the first failing check (the script
# stops there: a red check makes everything after it meaningless).
set -u

cd "$(dirname "$0")/.." || { echo "prepush: cannot cd to the repository root" >&2; exit 1; }

FAST=0
MATRIX=0
PACKAGE=1
PUBLISH_DRY_RUN=0
PARALLEL=0
for arg in "$@"; do
  case "$arg" in
    --fast) FAST=1 ;;
    --matrix) MATRIX=1 ;;
    --no-package) PACKAGE=0 ;;
    --publish-dry-run) PUBLISH_DRY_RUN=1 ;;
    --parallel) PARALLEL=1 ;;
    -h|--help)
      # Print the header comment block, not a fixed line range (it grew).
      sed -n '2,/^set -u$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "prepush: unknown argument: $arg (try --help)" >&2
      exit 1
      ;;
  esac
done

if [ "$PUBLISH_DRY_RUN" -eq 1 ] && [ "$PACKAGE" -eq 0 ]; then
  echo "prepush: --publish-dry-run needs the packaging step; drop --no-package" >&2
  exit 1
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-alt}"
case "$CARGO_TARGET_DIR" in
  *target-lane-*)
    echo "prepush: refusing CARGO_TARGET_DIR=$CARGO_TARGET_DIR (target-lane-* belongs to another lane)" >&2
    exit 1
    ;;
esac
if [ -z "${RUST_MIN_STACK:-}" ]; then export RUST_MIN_STACK=8388608; fi
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

STEP=0
step() {
  STEP=$((STEP + 1))
  printf '\n=== [%d] %s ===\n' "$STEP" "$1"
}
fail() {
  printf '\nprepush: FAILED: %s\n' "$1" >&2
  exit 1
}
warn() { printf 'prepush: warning: %s\n' "$1" >&2; }

echo "prepush: CARGO_TARGET_DIR=$CARGO_TARGET_DIR"

# --- 1. manifest sanity -----------------------------------------------------
step "manifest / release metadata"
cargo metadata --no-deps --format-version 1 >/dev/null 2>&1 ||
  fail "cargo metadata failed: Cargo.toml does not parse"

pkg_version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
[ -n "$pkg_version" ] || fail "could not read [package] version from Cargo.toml"
# The *package* name (pir-sdk), not the lib/bin name (pir). The first bare
# `name = "..."` in the manifest is [package].name; [[lib]]/[[bin]] names come
# later.
pkg_name="$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
[ -n "$pkg_name" ] || fail "could not read [package] name from Cargo.toml"
echo "package: $pkg_name $pkg_version"

# crates.io requires this metadata (missing keys are rejected at publish time,
# not at `cargo build` time).
for key in description license repository readme documentation keywords categories; do
  grep -Eq "^${key}[[:space:]]*=" Cargo.toml ||
    fail "Cargo.toml [package] is missing '$key' (needed by crates.io)"
done
echo "package metadata: description/license/repository/readme/documentation/keywords/categories present"

# Cargo.toml and Cargo.lock must agree; `--locked` builds (CI, release) fail
# otherwise. This is the check the old release workflow did against the tag.
lock_version="$(
  awk -v pkg="$pkg_name" '
    BEGIN { RS = ""; FS = "\n" }
    {
      name = ""; ver = "";
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^name = "/) { name = $i; sub(/^name = "/, "", name); sub(/"$/, "", name) }
        if ($i ~ /^version = "/) { ver = $i; sub(/^version = "/, "", ver); sub(/"$/, "", ver) }
      }
      if (name == pkg) print ver
    }
  ' Cargo.lock | head -n 1
)"
if [ -z "$lock_version" ]; then
  warn "Cargo.lock has no [[package]] named $pkg_name (expected for an unpublished lock)"
elif [ "$lock_version" != "$pkg_version" ]; then
  fail "Cargo.toml version $pkg_version != Cargo.lock version $lock_version (run: cargo update -p $pkg_name --precise $pkg_version, or cargo metadata to refresh the lock)"
else
  echo "version consistency: Cargo.toml == Cargo.lock == $pkg_version"
fi

# The release workflow derives the version from the tag, so a release needs
# only an Unreleased (or current-version) section at the top of CHANGELOG.md.
changelog_top="$(grep -m 1 '^## ' CHANGELOG.md 2>/dev/null || true)"
case "$changelog_top" in
  *Unreleased*|*"$pkg_version"*) echo "changelog: top section: $changelog_top" ;;
  "") warn "CHANGELOG.md has no '## ' section" ;;
  *) warn "CHANGELOG.md top section is '$changelog_top': add an '## Unreleased' entry (or the current version) before releasing" ;;
esac

# --- 2. formatting ----------------------------------------------------------
step "cargo fmt --all -- --check"
cargo fmt --all -- --check || fail "formatting (run: cargo fmt --all)"

# --- 3. compile -------------------------------------------------------------
step "cargo check --all-targets --locked"
cargo check --all-targets --locked || fail "cargo check"

# --- 4. lints ---------------------------------------------------------------
step "cargo clippy --all-targets --all-features --locked -- -D warnings"
cargo clippy --all-targets --all-features --locked -- -D warnings || fail "clippy"

# --- 5. tests ---------------------------------------------------------------
if [ "$FAST" -eq 0 ]; then
  # Fail closed: the pre-push gate must not substitute a different runner when
  # the CI runner is missing. `cargo test --tests` would omit the lib/bin unit
  # harnesses that CI shard 1 selects, and it has no nextest `ci` slow-test
  # bound.
  if ! cargo nextest --version >/dev/null 2>&1; then
    fail "cargo-nextest is not installed; install it with 'cargo install cargo-nextest --locked' (the pre-push gate will not substitute cargo test)"
  fi

  if [ "$PARALLEL" -eq 1 ]; then
    step "scripts/test_parallel.ps1 (local parallel runner)"
    # Opt-in local speed path. It is NOT the CI command: CI runs nextest in
    # eight isolated shards on eight runners; this runs N shards against the
    # same target dir. --no-fail-fast keeps every shard running so the gate
    # reports the whole picture. -NoBuild is deliberately not passed: the
    # runner's single warm build avoids cargo target-lock serialization.
    if command -v pwsh >/dev/null 2>&1; then
      PS_BIN=pwsh
    elif command -v powershell >/dev/null 2>&1; then
      PS_BIN=powershell
    else
      fail "--parallel needs PowerShell (pwsh/powershell) on PATH; run without --parallel"
    fi
    "$PS_BIN" -NoProfile -File "scripts/test_parallel.ps1" \
      -TargetDir "$CARGO_TARGET_DIR" -NoFailFast -UseNextest ||
      fail "test_parallel.ps1"
  else
    step "cargo nextest run (profile ci)"
    # Profile `ci` is the CI gate: fail-fast=false, 4 threads, 120s/360s
    # slow timeout (.config/nextest.toml). A green local run therefore means
    # the same tests CI will run, not a laxer subset.
    cargo nextest run --locked --profile ci || fail "nextest"
  fi

  step "cargo test --doc --locked"
  cargo test --doc --locked || fail "doctests"
else
  echo ""
  echo "(--fast: skipping nextest + doctests)"
fi

# --- 6. full feature matrix (opt-in) ---------------------------------------
if [ "$MATRIX" -eq 1 ]; then
  step "scripts/feature_matrix.sh (81 cargo invocations)"
  bash scripts/feature_matrix.sh || fail "feature matrix"
else
  echo ""
  echo "(feature matrix skipped; pass --matrix to run it — master CI runs it anyway)"
fi

# --- 7. packaging -----------------------------------------------------------
if [ "$PACKAGE" -eq 1 ]; then
  step "cargo package --list (files shipped to crates.io)"
  # stderr is left attached: a packaging error must be readable, not swallowed.
  # --allow-dirty is required because this runs on a worktree with uncommitted
  # edits (that is the whole point of a pre-push check).
  if ! pkg_list="$(cargo package --list --allow-dirty --locked)"; then
    fail "cargo package --list failed (error above)"
  fi
  printf 'packaged files: %s\n' "$(printf '%s\n' "$pkg_list" | grep -c .)"
  # The include list in Cargo.toml must keep build trees, logs, tests and docs
  # out of the .crate. Fail closed: an unexpected top-level entry is a
  # packaging bug, not a style issue.
  leaked="$(printf '%s\n' "$pkg_list" | grep -E '^(target|logs|tests|docs|scripts|target-lane)' || true)"
  [ -z "$leaked" ] || fail "unexpected files in the package (fix [package] include):
$leaked"

  step "cargo package --no-verify (size guard)"
  cargo package --no-verify --allow-dirty --locked >/dev/null || fail "cargo package --no-verify"
  crate_file="$CARGO_TARGET_DIR/package/${pkg_name}-${pkg_version}.crate"
  [ -f "$crate_file" ] || fail "cargo package did not produce $crate_file (looked in $CARGO_TARGET_DIR/package)"
  crate_bytes="$(wc -c < "$crate_file" | tr -d ' ')"
  echo "crate: $crate_file ($crate_bytes bytes)"
  # crates.io rejects a .crate at or above 10,000,000 bytes.
  [ "$crate_bytes" -lt 10000000 ] ||
    fail "the .crate is $crate_bytes bytes; the crates.io limit is 10,000,000 (trim [package] include)"

  if [ "$PUBLISH_DRY_RUN" -eq 1 ]; then
    step "cargo publish --dry-run"
    # Full packaging + build-from-the-.crate verification, no upload. This is
    # the closest local equivalent of what `publish` in release.yml does.
    cargo publish --dry-run --locked --allow-dirty || fail "cargo publish --dry-run"
  fi
else
  echo ""
  echo "(packaging checks skipped: --no-package)"
fi

printf '\nprepush: OK — %d checks passed (nothing was committed, nothing was pushed)\n' "$STEP"
