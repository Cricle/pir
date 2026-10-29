"""Make scripts/sdk_gate_rest.py tolerant of already-applied edits, then rerun it.

Two changes:
- `edit()` skips a pair whose replacement is already present.
- the `src/text.rs` guard accepts an existing file with the intended content.
"""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "scripts", "sdk_gate_rest.py")

OLD_EDIT = """def edit(rel: str, pairs) -> None:
    text = read(rel)
    for old, new in pairs:
        if old not in text:
            print(f"MISSING anchor in {rel}:\\n{old[:240]}", file=sys.stderr)
            raise SystemExit(1)
        text = text.replace(old, new, 1)
    write(rel, text)"""

NEW_EDIT = """def edit(rel: str, pairs) -> None:
    text = read(rel)
    for old, new in pairs:
        if new in text:
            continue  # already applied
        if old not in text:
            print(f"MISSING anchor in {rel}:\\n{old[:240]}", file=sys.stderr)
            raise SystemExit(1)
        text = text.replace(old, new, 1)
    write(rel, text)"""

OLD_GUARD = '''    if os.path.exists(os.path.join(ROOT, "src", "text.rs")):
        print("refusing to run: src/text.rs already exists", file=sys.stderr)
        return 1

    # ---- 1. crate::text ---------------------------------------------------
    write("src/text.rs", TEXT_RS)'''

NEW_GUARD = '''    text_rs = os.path.join(ROOT, "src", "text.rs")
    if os.path.exists(text_rs) and read("src/text.rs") != TEXT_RS:
        print("refusing to run: src/text.rs exists with different content", file=sys.stderr)
        return 1
    if not os.path.exists(text_rs):
        write("src/text.rs", TEXT_RS)'''

OLD_MOD = '    gate("src/coding_agent/mod.rs", [("pub mod session_pick;\\n", "tui")])\n'


def main() -> int:
    with open(SCRIPT, encoding="utf-8") as fh:
        text = fh.read()
    if NEW_EDIT in text:
        print("script already patched")
        return 0
    for old, new, what in [
        (OLD_EDIT, NEW_EDIT, "edit()"),
        (OLD_GUARD, NEW_GUARD, "text.rs guard"),
        (OLD_MOD, "    # (coding_agent/mod.rs is CRLF: patched by sdk_gate_crlf.py)\n", "CRLF module gate"),
    ]:
        if old not in text:
            print(f"anchor missing: {what}", file=sys.stderr)
            return 1
        text = text.replace(old, new, 1)
    with open(SCRIPT, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)
    print("patched sdk_gate_rest.py")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
