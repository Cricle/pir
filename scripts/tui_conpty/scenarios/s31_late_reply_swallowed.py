# -*- coding: utf-8 -*-
# s31 — LATE terminal replies must never be typed into the prompt.
#
# The ConPTY cannot be made to answer a colour query late on demand, so the
# scenario *types the reply bytes as input* — which is exactly the shape the
# defect produced (a real terminal's late reply reaching the input path). A
# DA1 answer (`ESC[?61;…c`) and an OSC 11 answer (`ESC]11;rgb:…BEL`) are fed,
# then a plain word as the positive control: the replies must be swallowed and
# the word must still be typed.
NAME = "s31_late_reply_swallowed"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("send", b"\x1b[?61;6;7;21;22;23;24;28;32;42c"),
    ("wait", 1.0),
    ("send", b"\x1b]11;rgb:1a1a/1a1a/2b2b\x07"),
    ("wait", 1.0),
    ("send", b"hello-late"),
    ("wait", 1.5),
    ("snap", "s31_after"),
]
CHECKS = [
    ("restore_markers",),
    ("editor_not_contains", "s31_after", "[?61;"),
    ("editor_not_contains", "s31_after", "rgb:1a1a"),
    ("editor_contains", "s31_after", "hello-late"),
]
