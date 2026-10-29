# -*- coding: utf-8 -*-
# PLAY p03 — the `/` and `@` autocompletes: open, type a prefix, Tab to
# complete, Esc to close, reopen; and `@` file completion + Esc.
NAME = "p03_autocomplete"
SEED_AGENT = False
TIMEOUT = 120
OPS = [
    ("wait_quiet", 4),
    ("send", b"/"),
    ("wait", 1.2),
    ("snap", "p03_slash_popup"),
    ("send", b"he"),
    ("wait", 1.0),
    ("snap", "p03_slash_he"),
    ("send", b"\t"),
    ("wait", 1.0),
    ("snap", "p03_tab_complete"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("snap", "p03_esc_popup"),
    ("send", b"\x03"),
    ("wait", 0.6),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("send", b"@"),
    ("wait", 1.5),
    ("snap", "p03_at_popup"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("snap", "p03_at_esc"),
    ("send", b"\x15"),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
