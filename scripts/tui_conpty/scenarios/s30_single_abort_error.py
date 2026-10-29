# Promoted from the 2026-10-16 foreman verification (p25_f4_assert.py): an
# assertion-level regression net for a REAL fix landed that day (WAVE-REPORT.md §6b).
# -*- coding: utf-8 -*-
# F4 assertion: one abort -> one error row.
NAME = "s30_single_abort_error"
# Paced so the stream is still running when Esc lands (a single-chunk
# reply would complete first and abort nothing).
MOCK_SCRIPT = [{"text": " ".join(f"word{i}" for i in range(400)),
                "chunks": 200, "chunk_delay_ms": 20}]
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4),
    ("send", b"stream\r"),
    ("wait", 3.0),
    ("send", b"\x1b"),
    ("wait", 3.0),
    ("snap", "p25_aborted"),
]
CHECKS = [
    # Upstream renders an aborted turn as "Operation aborted" (the raw
    # "Request was aborted" error message is filtered, assistant-message.ts:
    # 190-192; interactive-mode.ts:3572/4066), with "Interrupted" as the
    # status row.
    ("screen_count", "p25_aborted", "Operation aborted", "==1"),
]
