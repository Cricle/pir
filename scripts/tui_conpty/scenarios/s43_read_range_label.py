# -*- coding: utf-8 -*-
# pi 1.3.0 wave (2026-10-20) — read tool line-range label normalized.
#
# The read call header now goes through `format_read_line_range`
# (src/tui/messages.rs; upstream render-utils.ts:38-49) instead of the old
# inline matcher. The normalized behavior this scenario locks:
#   * offset + limit  -> `:<offset>-<offset+limit-1>`  (unchanged shape);
#   * limit ONLY      -> `:1-<limit>` — the old formatter emitted NO range
#                        when `offset` was absent, so `models.json:1-1` (the
#                        limit=1 case below) is the discriminating assertion;
#   * numeric-string offset/limit are accepted like numbers (upstream
#     `parseLineNumber`); malformed values omit the range.
#
# One hermetic mock turn carries TWO read calls with different args, so both
# labels render in one transcript. The per-scenario agent dir's `models.json`
# is written by the battery's mock injection before the app starts and the
# child cwd IS that agent dir, so the reads have a real file to open. The
# assertions deliberately target the call headers only: those labels are
# formatted from the request args, so they are deterministic and do not
# depend on the generated file's key/line ordering. `models.json` is not a
# compact-read resource (read.ts classification), so both headers use the
# standard call line.
#
# The reads are kept to ONE line each so both call headers stay inside the
# 40-row ConPTY screen's transcript viewport (follow-end would otherwise
# scroll the first block off the top).
#
# Assertions read the rendered call rows (substring form: the title/path/
# range spans are concatenated by pyte), plus the turn's final text as proof
# the tool round-trip completed. The boot check is the control: no read label
# exists before the prompt.
NAME = "s43_read_range_label"
OFFLINE = False
TIMEOUT = 120
MOCK_SCRIPT = [
    {"tool_calls": [
        {"name": "read",
         "arguments": {"path": "models.json", "offset": 2, "limit": 1}},
        {"name": "read",
         "arguments": {"path": "models.json", "limit": 1}},
    ]},
    {"text": "READ-RANGE-TURN-DONE"},
]
OPS = [
    ("wait_quiet", 4.0),
    ("snap", "s43_boot"),
    ("log", "prompt -> mock: read offset=2 limit=1, then read limit=1 only"),
    ("send", b"Read the provider config twice.\r"),
    ("wait", 6.0),
    ("wait_quiet", 3.0),
    ("snap", "s43_after"),
]
CHECKS = [
    # control: no read call header before the prompt
    ("screen_not_contains", "s43_boot", "models.json:"),
    # offset=2, limit=1 -> :2-2 (same shape as the old formatter); the
    # leading `read ` anchors the assertion to the rendered call header
    ("screen_contains", "s43_after", "read models.json:2-2"),
    # limit-only -> :1-1 (the normalized form; absent from the old output)
    ("screen_contains", "s43_after", "read models.json:1-1"),
    # the turn completed after both reads
    ("screen_contains", "s43_after", "READ-RANGE-TURN-DONE"),
    ("restore_markers",),
]
