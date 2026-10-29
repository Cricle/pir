# Promoted from the 2026-10-15 play session (p09_hostile.py) into the
# acceptance battery: a real-user regression scenario. Its evidence
# lives in scripts/tui_conpty/play/ and WAVE-REPORT.md §6b.
# -*- coding: utf-8 -*-
# PLAY p09 — hostile input around a running turn: key spam and junk escape
# sequences while a paced stream is in flight, ctrl+c mid-stream, then esc.
NAME = "s24_hostile_input"
OFFLINE = False
TIMEOUT = 150
MOCK_SCRIPT = [
    {"text": "p09-stream " * 40, "chunks": 40, "chunk_delay_ms": 150},
    {"text": "p09-second-turn"},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"start a long p09 stream\r"),
    ("wait", 1.5),
    ("snap", "p09_streaming"),
    # key spam while streaming
    ("send", b"zzzz" * 20),
    ("wait", 0.8),
    ("snap", "p09_spam_typed"),
    ("send", b"\x7f" * 80),
    ("wait", 0.6),
    # junk escape sequences (unknown CSI/SS3 + OSC)
    ("send", b"\x1b[99~" + b"\x1bOP" + b"\x1b]0;junk\x07" + b"\x1b[H" + b"\x1b[2J"),
    ("wait", 0.8),
    ("snap", "p09_junk_escapes"),
    # ctrl+c during the live stream
    ("send", b"\x03"),
    ("wait", 1.5),
    ("snap", "p09_ctrl_c"),
    # esc afterwards
    ("send", b"\x1b"),
    ("wait", 1.5),
    ("snap", "p09_esc"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("send", b"second p09 prompt\r"),
    ("wait", 3.0),
    ("wait_quiet", 3),
    ("snap", "p09_second_turn"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
