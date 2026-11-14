#!/usr/bin/env bash
# which_harness.sh - map a test suite/module (or tests/ path) to the merged
# integration-test harness that compiles it, and print a ready-to-run command.
#
# The harnesses are the generated tests/zz_merged_1..8.rs files plus the
# standalone [[test]] targets declared at the end of Cargo.toml. See
# docs/test-harness-map.md for the full table.
#
# Usage:
#   scripts/which_harness.sh <module|suite-name|tests/path.rs>
#
# Examples (works in Git Bash on Windows; no python/nextest needed):
#   scripts/which_harness.sh agent_bootstrap
#   scripts/which_harness.sh durable_assemble
#   scripts/which_harness.sh tests/suites/durable_assemble.rs
#   scripts/which_harness.sh tests/agent_benchmark.rs
#   scripts/which_harness.sh zz_merged_3
#
# Exit codes: 0 = found, 2 = usage, 3 = not found.
set -u

here="$(cd "$(dirname "$0")" 2>/dev/null && pwd)"
repo="$(cd "$here/.." && pwd)"
cd "$repo" || exit 1

if [ "$#" -ne 1 ] || [ -z "${1:-}" ]; then
  echo "usage: $0 <module|suite-name|tests/path.rs>" >&2
  exit 2
fi

PKG="pir-sdk"
arg="$1"
stem="$(basename "$arg")"
stem="${stem%.rs}"
case "$arg" in
  *suites/*) want_suite=1 ;;
  *)         want_suite=0 ;;
esac

# find_root_in_merged <stem> -> "<harness-file>\t<mod>\t<path-basename>"
# Looks for an include of <stem>.rs (or zz_shadow_<stem>.rs) in a zz_merged file.
find_root_in_merged() {
  local s="$1" h pair
  for h in tests/zz_merged_*.rs; do
    [ -f "$h" ] || continue
    pair="$(awk -v stem="$s" '
      /#\[path = "/ {
        p=$0; sub(/.*#\[path = "/,"",p); sub(/".*/,"",p)
        getline nxt
        m=nxt; sub(/^[ \t]*(pub[ \t]+)?mod[ \t]+/,"",m); sub(/[ \t]*;.*/,"",m)
        base=p; sub(/.*\//,"",base); sub(/\.rs$/,"",base)
        short=base; sub(/^zz_shadow_/,"",short)
        if (base==stem || short==stem) { printf "%s\t%s\t%s\n", FILENAME, m, base; exit }
      }' "$h")"
    if [ -n "$pair" ]; then
      printf '%s\n' "$pair"
      return 0
    fi
  done
  return 1
}

# target_name_for_path <tests/foo.rs> -> explicit [[test]] name, or empty.
target_name_for_path() {
  awk -v want="path = \"$1\"" '
    /^\[\[test\]\]/ { inblk=1; name=""; next }
    inblk && /^name = / { name=$0; sub(/^name = "/,"",name); sub(/".*/,"",name) }
    inblk && /^path = / { if (index($0,want)>0) { print name; exit } inblk=0 }
    /^\[/ && !/^\[\[test\]\]/ { inblk=0 }
  ' Cargo.toml
}

# resolve_suite <stem> -> "<harness>\t<kind>\t<nextest-filter>" or empty.
# Finds the tests/*.rs wrapper that includes tests/suites/<stem>.rs and then the
# harness that includes that wrapper (or the wrapper's standalone [[test]] name).
resolve_suite() {
  local stem="$1" wrapper wstem t pair h wm
  wrapper="$(grep -rlF "#[path = \"suites/${stem}.rs\"]" tests/*.rs 2>/dev/null | head -n1)"
  [ -n "$wrapper" ] || return 1
  wstem="$(basename "$wrapper" .rs)"
  t="$(target_name_for_path "$wrapper")"
  if [ -n "$t" ]; then
    printf '%s\t%s\t%s\n' "$t" "standalone" "test(${stem}::)"
    return 0
  fi
  pair="$(find_root_in_merged "$wstem" || true)"
  if [ -n "$pair" ]; then
    h="$(printf '%s' "$pair" | cut -f1)"; h="$(basename "$h" .rs)"
    wm="$(printf '%s' "$pair" | cut -f2)"
    printf '%s\t%s\t%s\n' "$h" "merged" "test(${wm}::${stem}::)"
    return 0
  fi
  return 1
}

# 0) An explicit target name (e.g. zz_merged_3, g_durable).
if [ "$want_suite" -eq 0 ]; then
  for h in tests/zz_merged_*.rs; do
    [ -f "$h" ] || continue
    if [ "$(basename "$h" .rs)" = "$stem" ]; then
      echo "harness: $stem (merged harness)"
      echo "command: cargo nextest run -p $PKG --test $stem"
      exit 0
    fi
  done
  t="$(target_name_for_path "tests/$stem.rs")"
  if [ -n "$t" ]; then
    echo "harness: $t (standalone [[test]] target)"
    echo "command: cargo nextest run -p $PKG --test $t"
    exit 0
  fi
fi

# 1) A root tests/<stem>.rs file that a zz_merged harness includes directly.
if [ "$want_suite" -eq 0 ] && { [ -f "tests/$stem.rs" ] || [ -f "tests/zz_shadow_$stem.rs" ]; }; then
  pair="$(find_root_in_merged "$stem" || true)"
  if [ -n "$pair" ]; then
    h="$(printf '%s' "$pair" | cut -f1)"; h="$(basename "$h" .rs)"
    m="$(printf '%s' "$pair" | cut -f2)"
    echo "source:  tests/$stem.rs (mod $m in $h)"
    echo "harness: $h (merged harness)"
    echo "command: cargo nextest run -p $PKG --test $h -E 'test(${m}::)'"
    if [ -f "tests/suites/$stem.rs" ]; then
      suite="$(resolve_suite "$stem" || true)"
      sh="$(printf '%s' "$suite" | cut -f1)"
      if [ -n "$sh" ] && [ "$sh" != "$h" ]; then
        echo "note:    tests/suites/$stem.rs is a different file in $sh;"
        echo "         pass that path to map the suite instead."
      else
        echo "note:    tests/suites/$stem.rs is also pulled in by this harness."
      fi
    fi
    exit 0
  fi
fi

# 2) A tests/suites/<stem>.rs file (explicitly, or a bare suite name).
suite="$(resolve_suite "$stem" || true)"
if [ -n "$suite" ]; then
  h="$(printf '%s' "$suite" | cut -f1)"
  kind="$(printf '%s' "$suite" | cut -f2)"
  flt="$(printf '%s' "$suite" | cut -f3)"
  echo "source:  tests/suites/${stem}.rs"
  echo "harness: $h ($kind)"
  echo "command: cargo nextest run -p $PKG --test $h -E '${flt}'"
  exit 0
fi

echo "no harness found for '$arg'" >&2
echo "hint: pass a tests/*.rs or tests/suites/*.rs file, or its module name." >&2
exit 3
