"""Foreman tool: verify the line-scoped `.expect(` allowlist in
tests/suites/source_hygiene.rs against the current src/ tree.

Usage: python scripts/check_expect_allowlist.py [--write]

Without --write it reports stale entries (file:line no longer holds an expect)
and unguarded sites (an expect that no allowlist entry covers).
With --write it rewrites the line numbers of stale entries in place, keeping
the tuple order, then re-verifies.
"""
import re
import sys
import pathlib

MARKERS = (".expect(", ".expect::<", ".expect (")
ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
HYGIENE = ROOT / "tests" / "suites" / "source_hygiene.rs"


def expect_lines(text):
    return [i + 1 for i, l in enumerate(text.splitlines()) if any(m in l for m in MARKERS)]


def main():
    write = "--write" in sys.argv
    hygiene_text = HYGIENE.read_text(encoding="utf-8")
    m = re.search(
        r"const EXPECT_SITE_ALLOWLIST: \[\(&str, u32\); (\d+)\] = \[(.*?)\n\];",
        hygiene_text,
        re.S,
    )
    if not m:
        sys.exit("cannot find EXPECT_SITE_ALLOWLIST")
    declared = int(m.group(1))
    body = m.group(2)
    entries = re.findall(r'\("([^"]+)",\s*(\d+)\)', body)
    print(f"declared {declared}, parsed {len(entries)}")

    # Cache each file's expect line numbers once.
    cache = {}
    for rel, _ in entries:
        p = SRC / rel
        if rel not in cache:
            cache[rel] = expect_lines(p.read_text(encoding="utf-8")) if p.exists() else []

    stale = []
    for rel, line in entries:
        n = int(line)
        if n not in cache[rel]:
            stale.append((rel, n, cache[rel]))
    print(f"\nSTALE entries: {len(stale)}")
    for rel, n, actual in stale:
        print(f"  {rel}:{n} -> file has expects at {actual}")

    unguarded = []
    for p in SRC.rglob("*.rs"):
        rel = p.relative_to(SRC).as_posix()
        allowed = {int(l) for r, l in entries if r == rel}
        for n in expect_lines(p.read_text(encoding="utf-8", errors="replace")):
            if n not in allowed:
                unguarded.append(f"{rel}:{n}")
    print(f"\nUNGUARDED sites: {len(unguarded)}")
    for u in unguarded:
        print("  ", u)

    if write and stale:
        new_body = body
        for rel, n, actual in stale:
            if not actual:
                print(f"  skip {rel}:{n} (no expect site left in file)")
                continue
            # Pick the nearest actual line to the stale one.
            nearest = min(actual, key=lambda a: abs(a - n))
            new_body = new_body.replace(f'("{rel}", {n})', f'("{rel}", {nearest})', 1)
            print(f"  rewrote {rel}:{n} -> {nearest}")
        HYGIENE.write_text(
            hygiene_text[: m.start(2)] + new_body + hygiene_text[m.end(2) :], encoding="utf-8"
        )
        print("\nrewritten; re-run without --write to verify")

    sys.exit(1 if (stale or unguarded) else 0)


if __name__ == "__main__":
    main()
