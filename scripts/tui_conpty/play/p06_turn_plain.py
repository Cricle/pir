# -*- coding: utf-8 -*-
# PLAY p06 — a plain hermetic turn with a PACED long stream (chunks +
# chunk_delay_ms): does the text stream in progressively, does the Working
# indicator clear at the end, and does the editor stay usable mid-stream.
NAME = "p06_turn_plain"
OFFLINE = False
TIMEOUT = 120
MOCK_SCRIPT = [
    {"text": "p06-stream " * 30, "chunks": 30, "chunk_delay_ms": 120},
]
OPS = [
    ("wait_quiet", 4),
    ("snap", "p06_boot"),
    ("send", b"say p06\r"),
    ("wait", 1.2),
    ("snap", "p06_midstream"),
    ("send", b"typed-mid-stream"),   # editor must stay usable during the stream
    ("wait", 1.0),
    ("snap", "p06_midstream_typed"),
    ("wait", 4.0),
    ("wait_quiet", 3),
    ("snap", "p06_done"),
    ("send", b"\x03"),
    ("wait", 0.6),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
