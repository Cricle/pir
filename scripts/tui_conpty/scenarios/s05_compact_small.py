# -*- coding: utf-8 -*-
# Ported from r2b_s5_compact.py. Intent: /compact on a fresh (small) session
# must surface the real error ("nothing to compact (session too small)"), not
# a false success ("Compacted: ..."). Provider-free.
NAME = "s05_compact_small"
OPS = [
    ("wait_quiet", 4),
    ("log", "/compact on a fresh session"),
    ("send", b"/compact\r"),
    ("wait", 2.5),
    ("snap", "s05_after"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 24),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s05_after", "nothing to compact"),
    ("screen_not_contains", "s05_after", "Compacted:"),
    ("restore_markers",),
]
