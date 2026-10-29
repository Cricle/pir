"""Make `gate()` in scripts/sdk_gate_rest.py tolerate already-applied gates
(including CRLF files patched elsewhere)."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "scripts", "sdk_gate_rest.py")

OLD = """    for snippet, feature in snippets:
        if f"#[cfg(feature = \\"{feature}\\")]\\n{snippet}" in text:
            continue
        if snippet not in text:
            print(f"MISSING snippet in {rel}:\\n{snippet[:240]}", file=sys.stderr)
            raise SystemExit(1)
        text = text.replace(snippet, f"#[cfg(feature = \\"{feature}\\")]\\n{snippet}", 1)"""

NEW = """    for snippet, feature in snippets:
        attribute = f'#[cfg(feature = "{feature}")]'
        if (
            f"{attribute}\\n{snippet}" in text
            or f"{attribute}\\r\\n{snippet.replace(chr(10), chr(13) + chr(10))}" in text
        ):
            continue  # already applied (possibly by the CRLF pass)
        if snippet not in text:
            print(f"MISSING snippet in {rel}:\\n{snippet[:240]}", file=sys.stderr)
            raise SystemExit(1)
        sep = "\\r\\n" if "\\r\\n" in text else "\\n"
        text = text.replace(snippet, f"{attribute}{sep}{snippet}", 1)"""


def main() -> int:
    with open(SCRIPT, encoding="utf-8") as fh:
        text = fh.read()
    if "already applied (possibly by the CRLF pass)" in text:
        print("gate() already patched")
        return 0
    if OLD not in text:
        print("gate() anchor missing", file=sys.stderr)
        return 1
    with open(SCRIPT, "w", encoding="utf-8", newline="") as fh:
        fh.write(text.replace(OLD, NEW, 1))
    print("patched gate()")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
