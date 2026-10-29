#!/usr/bin/env python3
"""Regenerate the deterministic zlib blobs embedded for the extension JS shims.

The raw `src/extensions/api/js/*.js.txt` files remain the checked-in source of
truth (tests and the SDK-surface auditors read them). The Rust extension host
embeds `assets/shims/<name>.js.txt.deflate` via `include_bytes!` and inflates
each blob once per process on first use (`src/extensions/api/embedded.rs`).

Why the blobs live outside `src/`: the source-hygiene gate
(`tests/suites/source_hygiene.rs`) reads *every* file under `src/` as UTF-8
text, so binary assets cannot live there.

Format: RFC 1950 zlib, level 9, no dictionary. Python's zlib writes a fixed
header for a given level and no timestamp, so repeated runs on a checkout are
byte-identical. `--check` fails when a blob is missing, stale, or does not
round-trip to the current source bytes.

`script_boot.js.txt` is deliberately skipped: it is embedded by
`src/main_cli/js_mode.rs` (a different owner). C2's scope is the shims
`src/extensions/**` embeds.

Usage:
  python scripts/gen_shims_deflate.py           # write/refresh the blobs
  python scripts/gen_shims_deflate.py --check   # verify without writing
"""

from __future__ import annotations

import argparse
import glob
import os
import sys
import zlib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_DIR = os.path.join(REPO, "src", "extensions", "api", "js")
OUT_DIR = os.path.join(REPO, "assets", "shims")
SKIP = {"script_boot.js.txt"}  # embedded from src/main_cli (not this tranche)

LEVEL = 9


def sources() -> list[str]:
    files = sorted(glob.glob(os.path.join(SRC_DIR, "*.js.txt")))
    return [path for path in files if os.path.basename(path) not in SKIP]


def blob_path(name: str) -> str:
    return os.path.join(OUT_DIR, name + ".deflate")


def compress(data: bytes) -> bytes:
    return zlib.compress(data, LEVEL)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify the checked-in blobs instead of writing them",
    )
    args = parser.parse_args()

    os.makedirs(OUT_DIR, exist_ok=True)
    failures: list[str] = []
    rows: list[tuple[int, int, str]] = []
    total_raw = 0
    total_deflated = 0

    for src in sources():
        name = os.path.basename(src)
        # The canonical bytes are LF regardless of the checkout's
        # core.autocrlf/eol setting. Without this, a Windows checkout embeds
        # CRLF blobs while Linux CI checks out LF and the byte-exact shim gate
        # fails there.
        raw = open(src, "rb").read().replace(b"\r\n", b"\n")
        deflated = compress(raw)
        total_raw += len(raw)
        total_deflated += len(deflated)
        rows.append((len(raw), len(deflated), name))
        out = blob_path(name)

        if args.check:
            if not os.path.exists(out):
                failures.append(f"missing {os.path.relpath(out, REPO)}")
                continue
            stored = open(out, "rb").read()
            if not zlib.decompress(stored) == raw:
                failures.append(f"{name}: stored blob does not decode to the source")
            elif stored != deflated:
                failures.append(f"{name}: stored blob is stale (re-run the generator)")
        else:
            tmp = out + ".tmp"
            with open(tmp, "wb") as handle:
                handle.write(deflated)
            os.replace(tmp, out)

    expected = {os.path.basename(path) + ".deflate" for path in sources()}
    for path in sorted(glob.glob(os.path.join(OUT_DIR, "*.deflate"))):
        if os.path.basename(path) not in expected:
            failures.append(f"orphan {os.path.relpath(path, REPO)}")

    rows.sort(key=lambda row: row[1], reverse=True)
    for raw_len, deflated_len, name in rows:
        print(f"{raw_len:>8}  {deflated_len:>8}  {name}")
    print(
        f"\n{len(rows)} assets: {total_raw} raw -> {total_deflated} deflate "
        f"({total_raw / total_deflated:.3f}x, {total_raw - total_deflated} B saved)"
    )

    if failures:
        for failure in failures:
            print(f"ERROR: {failure}", file=sys.stderr)
        return 1
    print("blobs are current" if args.check else "blobs written")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
