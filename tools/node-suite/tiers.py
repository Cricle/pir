#!/usr/bin/env python3
"""Rebuild the Node-suite tier manifests from a corpus checkout.

The partition is a *first-match* rule set over ``test/parallel/*.js``:

    Tier A  does not require '../common' (runs against stock pir).
    Tier D  out of scope: internal modules / --expose-internals, inspector,
            test-v8-*, addon/napi/wasi/sea.
    Tier C  needs a runtime or harness feature pir does not have yet:
            node:test, worker_threads, child_process/process.execPath, or a
            CLI flag (other than --no-warnings/--pending-deprecation).
    Tier B  everything else: runnable with only the vendored common shim.

The audit in docs/node-test-suite-audit.md quotes category *sums* (the same
file can be counted by several blockers), so its tier table is not a disjoint
partition.  This script reports a disjoint partition; small deltas in B/C/D
are expected and are overlap, not drift.

``tier-b-api.txt`` is the canonical 438-test public-API subset used as the
harness baseline.  It is derivable from the rules below:

    * name starts with one of the 11 public-API families,
    * no internal/--expose-internals, node:test, worker_threads,
      child_process/process.execPath, inspector, v8, addon/napi/wasi/sea,
    * no net/cluster/dgram/http/https/tls/http2 mention,
    * no `// Flags:` other than --no-warnings,
    * one manual drop: test-buffer-includes.js (kept out of the original
      curated baseline; no functional reason found).

Usage:
    python tools/node-suite/tiers.py --corpus <node-clone> --out tools/node-suite/baseline
    python tools/node-suite/tiers.py --corpus <node-clone> --out /tmp/tiers --check tools/node-suite/baseline
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

FAMILIES = (
    "test-fs-",
    "test-stream-",
    "test-zlib-",
    "test-buffer-",
    "test-path-",
    "test-util-",
    "test-events-",
    "test-assert-",
    "test-querystring-",
    "test-url-",
    "test-string-decoder-",
)

FLAGS_RE = re.compile(r"^// Flags: *(.*)$", re.M)
NET_RE = re.compile(r"\b(?:cluster|dgram|net|http|https|tls|http2)\b")
ADDON_RE = re.compile(r"addon|napi|wasi|sea\b")

# Flags that may be ignored without changing public-API semantics.
IGNORABLE_FLAGS = {"--no-warnings", "--pending-deprecation"}

# Files excluded from tier-b-api even though the mechanical rules keep them.
MANUAL_API_DROPS = {
    "test-buffer-includes.js": "omitted from the original curated 438; retained "
    "here so regenerated lists match the published baseline",
}


def flags_of(source: str) -> list[str]:
    match = FLAGS_RE.search(source)
    return match.group(1).split() if match else []


def is_tier_d(name: str, source: str, flags: list[str]) -> bool:
    return bool(
        "require('internal/" in source
        or 'require("internal/' in source
        or "--expose-internals" in flags
        or "inspector" in name
        or name.startswith("test-v8-")
        or ADDON_RE.search(name)
        # A few tests mention test/addons only in a comment; they are still
        # out of scope for this surface (e.g. test-fs-stream-windows-handle).
        or ADDON_RE.search(source)
    )


def is_tier_c(name: str, source: str, flags: list[str]) -> bool:
    if any(flag not in IGNORABLE_FLAGS for flag in flags):
        return True
    return bool(
        "node:test" in source
        or "worker_threads" in source
        or "child_process" in source
        or "process.execPath" in source
    )


def is_api_candidate(name: str, source: str) -> bool:
    """The stricter rule set behind baseline/tier-b-api.txt."""
    if not name.startswith(FAMILIES):
        return False
    if name in MANUAL_API_DROPS:
        return False
    if NET_RE.search(source):
        return False
    flags = flags_of(source)
    if any(flag != "--no-warnings" for flag in flags):
        return False
    if is_tier_d(name, source, flags) or is_tier_c(name, source, flags):
        return False
    return True


def classify(name: str, source: str) -> str:
    flags = flags_of(source)
    if "require('../common')" not in source:
        return "a"
    if is_tier_d(name, source, flags):
        return "d"
    if is_tier_c(name, source, flags):
        return "c"
    return "b"


def write_list(path: Path, names: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(f"{name}\n" for name in names), encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--corpus", required=True, type=Path, help="Node checkout (contains test/parallel)")
    parser.add_argument("--out", required=True, type=Path, help="directory for tier-*.txt")
    parser.add_argument("--check", type=Path, help="compare regenerated tier-b-api.txt with this file")
    args = parser.parse_args(argv)

    parallel = args.corpus / "test" / "parallel"
    files = sorted(parallel.glob("*.js"))
    if not files:
        raise SystemExit(f"no .js files under {parallel}")

    tiers: dict[str, list[str]] = {"a": [], "b": [], "c": [], "d": []}
    api: list[str] = []
    for path in files:
        source = path.read_text(encoding="utf-8", errors="replace")
        tiers[classify(path.name, source)].append(path.name)
        if is_api_candidate(path.name, source):
            api.append(path.name)

    for tier, names in tiers.items():
        write_list(args.out / f"tier-{tier}.txt", names)
    write_list(args.out / "tier-b-api.txt", api)

    print("tier-a: %d" % len(tiers["a"]))
    print("tier-b: %d" % len(tiers["b"]))
    print("tier-c: %d" % len(tiers["c"]))
    print("tier-d: %d" % len(tiers["d"]))
    print("tier-b-api: %d" % len(api))

    if args.check:
        expected = [line.strip() for line in args.check.read_text(encoding="utf-8").splitlines() if line.strip()]
        if expected == api:
            print(f"check OK: matches {args.check}")
        else:
            missing = sorted(set(expected) - set(api))
            extra = sorted(set(api) - set(expected))
            print(f"check FAILED against {args.check}", file=sys.stderr)
            print(f"  missing ({len(missing)}): {', '.join(missing[:20])}", file=sys.stderr)
            print(f"  extra   ({len(extra)}): {', '.join(extra[:20])}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
