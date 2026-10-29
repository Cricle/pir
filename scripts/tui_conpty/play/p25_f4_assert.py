# -*- coding: utf-8 -*-
# F4 assertion: one abort -> one error row.
NAME = "p25_f4_assert"
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
    ("screen_count", "p25_aborted", "Error: Request was aborted", "==1"),
]
