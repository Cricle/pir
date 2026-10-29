"""Append the recorded residuals to HANDOFF.md §9."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "HANDOFF.md"

BLOCK = """**Recorded residuals (known, deliberate)**
10. Compaction state machine (2026-10-16): `navigate_tree` still checks only "is a
    run streaming", so a leaf move can race an in-flight manual compaction (upstream
    additionally rejects with "Wait for the current compaction or tree navigation to
    finish…"); Esc does not cancel an in-flight manual compaction (pir has no
    `abortCompaction` seam); `ctx.compact` notifications are skipped (diag only)
    while a run is streaming *or* settling instead of queueing behind it; and the
    window between the loop's `agent_end` and `mark_settling` still rejects
    compaction honestly (a run may issue a continuation request there).
11. Streaming SSE framing (`src/ai/api/mod.rs::sse_feed_lines`) drains the consumed
    prefix after the line callback (the callback borrows the buffer), so a panic
    inside a parser would re-deliver that one line if the parser were reused — it is
    not (a panic unwinds the stream task).
12. Perf residuals quantified in `WAVE-REPORT.md` §5b/§7: per-turn owned `Context`
    clones in `PrepareRequestContext`/`ProviderRequest`/`FinishTurnContext` (API
    shape), the fetch worker's short-lived tokio runtime + the base64 bridge, and the
    per-session TUI frame copies.

"""


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    if "Recorded residuals" in text:
        print("already present")
        return 0
    nl = "\r\n" if "\r\n" in text else "\n"
    anchor = "## 10. Committing"
    if nl == "\r\n":
        anchor = anchor.replace("\n", "\r\n")
    if anchor not in text:
        print("anchor missing", file=sys.stderr)
        return 1
    block = BLOCK if nl == "\n" else BLOCK.replace("\n", "\r\n")
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text.replace(anchor, block + anchor, 1))
    print("HANDOFF §9 residuals appended")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
