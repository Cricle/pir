"""SDK trimming helper: count cross-module references that must be cfg-gated.

Read-only. Usage: python scripts/sdk_refs.py <pattern> [<pattern> ...]
Patterns are matched against the raw source text; the module prefix is
`crate::extensions` / `crate::env_client` / `crate::tui` / `crate::mcp`.
"""

import os
import sys

SKIP_DIRS = ("src/extensions/", "src/env_client/", "src/tui/", "src/mcp/")


def main() -> int:
    pats = sys.argv[1:] or ["crate::extensions"]
    totals = {p: 0 for p in pats}
    rows = []
    for root, _dirs, files in os.walk("src"):
        for fn in files:
            if not fn.endswith(".rs"):
                continue
            path = os.path.join(root, fn).replace(os.sep, "/")
            if any(path.startswith(d) for d in SKIP_DIRS):
                continue
            with open(path, encoding="utf-8", errors="replace") as fh:
                text = fh.read()
            counts = [text.count(p) for p in pats]
            if any(counts):
                rows.append((sum(counts), path, counts))
                for p, c in zip(pats, counts):
                    totals[p] += c
    rows.sort(reverse=True)
    for total, path, counts in rows:
        detail = ", ".join(f"{p}={c}" for p, c in zip(pats, counts) if c)
        print(f"{total:4d}  {path:62s} {detail}")
    print("TOTALS", " ".join(f"{p}={c}" for p, c in totals.items()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
