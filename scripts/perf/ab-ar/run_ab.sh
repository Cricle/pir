#!/usr/bin/env bash
# Thin launcher for run_ab.mjs (works from Git Bash on Windows and POSIX).
#
#   export PIR_AB_API_KEY=...
#   scripts/perf/ab-ar/run_ab.sh --task probe --runs 3 \
#       --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec node "$HERE/run_ab.mjs" "$@"
