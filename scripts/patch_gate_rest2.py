"""Remove the src/lib.rs edit block from scripts/sdk_gate_rest.py.

lib.rs is CRLF and already patched by scripts/sdk_gate_crlf.py, so the LF
anchors in the main script must not run for it.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "scripts", "sdk_gate_rest.py")


def main() -> int:
    with open(SCRIPT, encoding="utf-8") as fh:
        text = fh.read()
    start = text.find('    edit(\n        "src/lib.rs",')
    if start == -1:
        print("lib.rs block already removed")
        return 0
    end = text.index('    )\n', start) + len('    )\n')
    text = text[:start] + "    // src/lib.rs is CRLF: patched by sdk_gate_crlf.py.\n" + text[end:]
    with open(SCRIPT, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)
    print(f"removed {end - start} bytes of lib.rs edits")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
