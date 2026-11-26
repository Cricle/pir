#!/usr/bin/env bash
# Measure test build/link wall time and target directory size without changing
# test features or target selection. Use a private target directory per run.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
target_dir="${CARGO_TARGET_DIR:-$root/target-bench}"
case "$target_dir" in
  *target-lane-*)
    echo "refusing shared target-lane directory: $target_dir" >&2
    exit 1
    ;;
esac
mkdir -p "$target_dir"
export CARGO_TARGET_DIR="$target_dir"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"

start_ns="$(date +%s%N 2>/dev/null || true)"
if [[ -z "$start_ns" || "$start_ns" == *N* ]]; then
  start_ns=0
fi
cargo test --no-run --locked "$@"
end_ns="$(date +%s%N 2>/dev/null || true)"
if [[ "$start_ns" != 0 && -n "$end_ns" && "$end_ns" != *N* ]]; then
  elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
else
  elapsed_ms=-1
fi
bytes="$(du -sk "$target_dir" 2>/dev/null | cut -f1 || true)"
bytes="${bytes:-0}"
bytes=$((bytes * 1024))
json_target_dir="$(printf '%s' "$target_dir" | sed 's/\\/\\\\/g; s/"/\\"/g')"
json_incremental="$(printf '%s' "$CARGO_INCREMENTAL" | sed 's/\\/\\\\/g; s/"/\\"/g')"
printf '{"target_dir":"%s","incremental":"%s","elapsed_ms":%s,"target_bytes":%s}\n' \
  "$json_target_dir" "$json_incremental" "$elapsed_ms" "$bytes"
