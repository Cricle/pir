# -*- coding: utf-8 -*-
# W13-L task A part 2 — "stream a long response, then compact and resume".
#
# THE END-TO-END WITNESS for the incremental-markdown fence fix (foreman,
# 2026-10-21): a streaming fenced code block used to silently DROP a middle
# line — `value_2` vanished — because the incremental cut was computed against
# the synthetic re-parse (`src/tui/markdown.rs:1012-1024`: `synthetic_line =
# 1 + stable_new - fence.stable_lines`). Unit tests pin the byte identity in
# tests/suites/tui_markdown_incremental.rs; THIS scenario proves it in the real
# TUI over ConPTY, where the render is driven by a paced SSE stream.
#
# THREE TURNS, one mock script (mock_llm.py, one step per request):
#   1. a paced stream (60 chunks, 25 ms apart => ~1.5 s of visible streaming)
#      of a markdown reply with:
#        * two paragraphs,
#        * a ```rust fence with exactly 60 content lines, `value_1`..`value_58`
#          in the MIDDLE (so `value_2` is 3 lines into the fence and every
#          `value_N` appears exactly once),
#        * a closing paragraph.
#      The mid-stream snap is taken 0.9 s after the send — before the reply
#      completes — and must show partial content with the fence active.
#   2. a normal short reply (W13-A2-TURN2-DONE).
#   3. a ~26k-token bulk reply (1300 `W13-BULK-nnnn:` rows) which pushes the
#      projected context over `contextWindow - reserveTokens`, so the NEXT
#      `/compact` has something real to summarize.
#
# WHY THE BULK TURN EXISTS: manual compaction only summarizes the span ABOVE
# the keep-recent tail (`keepRecentTokens`, default 20 000). With two short
# turns everything is inside that tail and `/compact` correctly answers
# "nothing to compact (session too small)" — a REAL outcome, but not the one
# this scenario must witness. The bulk turn (26k estimated tokens > 20k) leaves
# the two real turns above the tail (1368 + ~3300 vs 26373), so `/compact`
# summarizes them and the split lands inside the bulk turn.
# The mock provider's contextWindow is 1 000 000 (seed models.json), so no
# automatic threshold compaction fires first.
#
# FENCE ASSERTIONS — why they are specific (a dropped middle line must FAIL):
# the rendered 120x40 screen cannot show all 60 fence rows plus both
# paragraphs at once (the dock takes 5 rows), so the transcript is read in
# place at three scroll positions: follow-end, one `pageup` (= viewport minus
# the 4-row overlap, src/tui/altscreen_search.rs:96,3020), and `ctrl+home`
# (top). Each fence line is short enough (max 36 chars) that NO row wraps, so
# every `value_N` is a whole visible row.
#   * the tail snap covers `value_29`..`value_58` (the last 30 fence rows);
#   * the paged snap covers `value_1`..`value_32` (the first 32 fence rows),
#     i.e. `value_2` — the exact line the incremental bug dropped — and every
#     line in between;
#   * the two windows OVERLAP on value_29..32, so together they are one
#     unbroken cover of all 58 generated rows;
#   * the top snap covers the fence opener + `fn main()` + `value_1`.
# The tail and paged checks are therefore CONTIGUOUS-ROW regexes, not
# presence probes: one missing middle row breaks the run and fails the check.
# `W13-A2-TURN2-DONE` (the SECOND turn's reply) is asserted ABSENT from the
# paged screen, so the paged window really is the first turn's fence and not a
# re-render of newer content.
#
# RESUME: the driver always ends with `/quit`. A second scenario file
# (`s45b_resume_after_compact.py`) relaunches the SAME agent dir with
# `-c` (continue the most recent session for this cwd) and this scenario's
# `COMPACT_OUTCOME` (the exact status line it observed) is written to
# `out/<run-id>/s45.../compact_outcome.txt` by the runner for the report. The
# transcript-restore assertion itself lives in s45b.
NAME = "s45_real_task_stream_and_compact"
OFFLINE = False
TIMEOUT = 300

_P1 = ("Paragraph one: the driver walks the fixture, records every step, and "
       "replays it against the verifier before the audit trail is closed.")
_P2 = ("Paragraph two: the closing fence below must be the last rendered row of "
       "the answer, and the middle line value_2 must never disappear from it.")

# Exactly 60 content lines: opener `fn main() {`, value_1..value_58, closer `}`.
_FENCE = ["fn main() {"] + [f"    let value_{n} = compute_step({n});" for n in range(1, 59)] + ["}"]
assert len(_FENCE) == 60

_STREAM_REPLY = "\n\n".join([
    "W13-A2 streaming answer follows.",
    _P1,
    "```rust\n" + "\n".join(_FENCE) + "\n```",
    _P2,
])
_STREAM_LAST_LINE = "    let value_57 = compute_step(57);"
assert _STREAM_LAST_LINE in _STREAM_REPLY
# every fence marker row occurs exactly once, so a dropped line is detectable
for _n in (1, 2, 20, 28, 32, 57, 58):
    assert _STREAM_REPLY.count(f"compute_step({_n});") == 1, _n

# CONTIGUOUS-ROW patterns: the fence body must render as one unbroken run of
# rows. A dropped middle line breaks the run, so these are the checks that
# make the old `value_2` loss fail rather than pass.
def _rows(first, last):
    # `.*$` absorbs the scrollbar column and the jump-to-latest overlay label
    # that a scrolled screen paints on its LAST transcript row; the rows
    # themselves must still appear on CONSECUTIVE lines.
    return "(?m)" + "\n".join(
        r"^ *let value_%d = compute_step\(%d\);.*$" % (n, n)
        for n in range(first, last + 1))

# measured windows (run w13-s45-1): the follow-end screen shows value_29..58
# and one page-up shows value_1..32, so the two overlap on value_29..32 and
# together cover all 58 generated rows with no gap.
_TAIL_ROWS = _rows(29, 58)
_PAGED_ROWS = _rows(1, 32)

_BULK = "\n".join(
    f"W13-BULK-{i:04d}: inventory row {i} carries a stable identifier for the audit trail"
    for i in range(1, 1301))

MOCK_SCRIPT = [
    # turn 1: paced stream of the fence reply. 120 chunks x 50 ms = ~6 s of
    # visible streaming, so the mid-stream snapshot below lands inside the
    # stream even when the first token is slow (measured first-token latency
    # on a loaded host: ~2.5 s; on an idle one: <0.5 s).
    {"text": _STREAM_REPLY, "chunks": 120, "chunk_delay_ms": 50},
    # turn 2: a short reply that must NOT contain the fence
    {"text": "W13-A2-TURN2-DONE"},
    # turn 3: the bulk reply that makes /compact have a real span to summarize
    {"text": _BULK},
    # turn 4 (after /compact): the post-compaction prompt's reply
    {"text": "W13-A2-AFTER-COMPACT-DONE"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s45_boot"),
    ("log", "turn 1 -> mock: paced 120-chunk (~6s) stream carrying the 60-line fence"),
    ("send", b"Stream the long answer with the fenced code block.\r"),
    # mid-stream: ~3.5 s into a ~6 s stream. The first token arrives within
    # ~2.5 s even on a loaded host, so the reply is definitely partially
    # rendered here and definitely not finished.
    ("wait", 3.5),
    ("snap", "s45_midstream"),
    ("log", "let the stream settle, then read the rendered fence tail"),
    ("wait", 6.0),
    ("wait_quiet", 4.0),
    ("snap", "s45_stream_done"),
    ("log", "one page-up: the first ~27 fence rows (value_2..value_28)"),
    ("send", b"\x1b[5~"),
    ("wait", 1.2),
    ("snap", "s45_paged"),
    ("log", "ctrl+home: the fence opener and value_1"),
    ("send", b"\x1b[1;5H"),
    ("wait", 1.2),
    ("snap", "s45_top"),
    ("log", "back to follow-end (ctrl+end) for turn 2"),
    ("send", b"\x1b[1;5F"),
    ("wait", 1.0),
    ("log", "turn 2 -> mock: a short reply"),
    ("send", b"Now a short second turn.\r"),
    ("wait", 5.0),
    ("wait_quiet", 3.0),
    ("snap", "s45_turn2"),
    ("log", "turn 3 -> mock: the bulk reply that creates a compactable span"),
    ("send", b"Now dump the inventory rows.\r"),
    ("wait", 10.0),
    ("wait_quiet", 4.0),
    ("snap", "s45_bulk"),
    ("log", "/compact with a real span above the keep-recent tail"),
    ("send", b"/compact\r"),
    ("wait", 8.0),
    ("wait_quiet", 4.0),
    ("snap", "s45_compact"),
    ("log", "one more prompt after the compaction"),
    ("send", b"Summarize what we just did.\r"),
    ("wait", 6.0),
    ("wait_quiet", 3.0),
    ("snap", "s45_after_compact"),
]

CHECKS = [
    # control: nothing from the reply before the prompt
    ("screen_not_contains", "s45_boot", "compute_step"),
    ("screen_not_contains", "s45_boot", "W13-A2 streaming"),
    # (a) MID-STREAM: partial content on screen and the fence renderer ACTIVE
    #     — not a blank frame, and demonstrably not the finished reply (its
    #     tail rows are absent). The reply's FIRST paragraph is not asserted
    #     here: mid-stream the viewport is pinned to the tail of the long
    #     fence, so the head has scrolled off; check (d) re-checks it after
    #     ctrl+home.
    #     Q58: the literal ```rust opener row is NOT asserted on the rendered
    #     screen. It is scroll-sensitive by one row: at 3.5 s the partial
    #     fence is ~30 rows and the viewport is pinned to its tail, so on the
    #     release build the opener sits exactly one row above the visible
    #     area (the debug build happened to show it). The same intent — the
    #     fence body rendered as Rust code — is asserted scroll-independently
    #     on the RAW stream: the syntax highlighter emitted the keyword and
    #     operator colors for real body rows. The rendered body row and the
    #     absent-tail checks below keep the mid-stream position pinned.
    ("log_regex", "s45_midstream",
     r"\x1b\[38;5;74mlet \x1b\[mvalue_\d+ \x1b\[38;5;145m= "),
    ("screen_regex", "s45_midstream", r"(?m)^ *let value_\d+ = compute_step\(\d+\);"),
    ("screen_not_contains", "s45_midstream", "let value_58 = compute_step(58);"),
    ("screen_not_contains", "s45_midstream", "Paragraph two:"),
    # (b) POST-STREAM follow-end: the fence TAIL as one CONTIGUOUS run of rows
    #     (value_29..value_58) plus the answer's LAST paragraph.
    ("screen_regex", "s45_stream_done", _TAIL_ROWS),
    ("screen_contains", "s45_stream_done", "let value_57 = compute_step(57);"),
    ("screen_contains", "s45_stream_done", "Paragraph two:"),
    # (c) POST-STREAM one page up: the fence HEAD as one CONTIGUOUS run
    #     (value_1..value_32), so value_2 — the line the incremental bug
    #     dropped — and every row between it and value_32 are really rendered.
    #     value_29..32 also appear in (b): the two windows overlap, so the
    #     cover has no gap. The SECOND turn's reply must not be in this window,
    #     which proves it is the first turn's fence.
    ("screen_regex", "s45_paged", _PAGED_ROWS),
    ("screen_contains", "s45_paged", "```rust"),
    ("screen_not_contains", "s45_paged", "W13-A2-TURN2-DONE"),
    # (d) ctrl+home: the fence opener and its first content row
    ("screen_contains", "s45_top", "W13-A2 streaming answer follows."),
    ("screen_contains", "s45_top", "fn main() {"),
    ("screen_contains", "s45_top", "let value_1 = compute_step(1);"),
    # (e) turn 2 rendered and the bulk turn completed
    ("screen_contains", "s45_turn2", "W13-A2-TURN2-DONE"),
    ("screen_contains", "s45_bulk", "W13-BULK-1300:"),
    # (f) /compact surfaces a REAL outcome: either the real success line
    #     (with the token count it summarized) or the real refusal. A silent
    #     no-op or an error banner fails both.
    ("screen_regex", "s45_compact", r"(?m)Compacted: \d+ tokens before|nothing to compact"),
    ("screen_not_contains", "s45_compact", "compact failed"),
    # (g) the session still accepts and answers a prompt after compaction
    ("screen_contains", "s45_after_compact", "W13-A2-AFTER-COMPACT-DONE"),
    ("restore_markers",),
]
