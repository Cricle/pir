"""Add the real-world extension legs (read-role, read-content) to the harness."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "scripts/perf/ext_delta_throughput.py"

OLD = '''EXTENSIONS = {
    "none": None,
    "noop": (
        "export default (pi) => {\\n"
        "  pi.on(\\"message_update\\", () => {});\\n"
        "};\\n"
    ),'''

NEW = '''EXTENSIONS = {
    "none": None,
    "noop": (
        "export default (pi) => {\\n"
        "  pi.on(\\"message_update\\", () => {});\\n"
        "};\\n"
    ),
    # The real-world shape (pi-phone): scalar reads on the cumulative message
    # plus forwarding the per-delta event. Must cost O(delta), not O(reply).
    "read-role": (
        "export default (pi) => {\\n"
        "  let last = null;\\n"
        "  pi.on(\\"message_update\\", (event) => {\\n"
        "    if (event.message?.role === \\"assistant\\") last = event.message.timestamp;\\n"
        "    globalThis.__last = event.assistantMessageEvent?.type ?? last;\\n"
        "  });\\n"
        "};\\n"
    ),
    # Reads the accumulated content array every delta (no stringify): the
    # heaviest legitimate observer pattern.
    "read-content": (
        "export default (pi) => {\\n"
        "  let seen = 0;\\n"
        "  pi.on(\\"message_update\\", (event) => {\\n"
        "    const c = event.message?.content;\\n"
        "    if (Array.isArray(c)) seen = c.length;\\n"
        "    globalThis.__seen = seen;\\n"
        "  });\\n"
        "};\\n"
    ),'''


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    if "read-role" in text:
        print("already added")
        return 0
    if OLD not in text:
        print("anchor missing", file=sys.stderr)
        return 1
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text.replace(OLD, NEW, 1))
    print("legs added: read-role, read-content")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
