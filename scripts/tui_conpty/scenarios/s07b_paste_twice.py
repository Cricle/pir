# -*- coding: utf-8 -*-
# New (battery starter; r2b_s7b sibling). Intent: repeated bracketed pastes
# each render exactly once - two distinct tokens pasted back-to-back must both
# appear once in the editor, then the editor is cleared for a clean /quit.
# EXTENDED 2026-10-15 (paste-gate fix hand-off): markers must not leak, and
# the editor must hold exactly the 14-char concatenated payload
# (observed row ` TOK1_QQTOK2_ZZ` between the box rules).
NAME = "s07b_paste_twice"
OPS = [
    ("wait_quiet", 4),
    ("log", "paste TOK1_QQ"),
    ("send", b"\x1b[200~TOK1_QQ\x1b[201~"),
    ("wait", 1.0),
    ("log", "paste TOK2_ZZ"),
    ("send", b"\x1b[200~TOK2_ZZ\x1b[201~"),
    ("wait", 1.0),
    ("snap", "s07b_after"),
    ("log", "clear editor"),
    ("send", b"\x7f" * 26),
    ("wait", 0.8),
    ("snap", "s07b_cleared"),
]
CHECKS = [
    ("screen_count", "s07b_after", "TOK1_QQ", 1),
    ("screen_count", "s07b_after", "TOK2_ZZ", 1),
    ("editor_contains", "s07b_after", "TOK1_QQ"),
    ("editor_contains", "s07b_after", "TOK2_ZZ"),
    ("screen_regex", "s07b_after", r"(?m)^[ \t]*TOK1_QQTOK2_ZZ[ \t]*$"),
    ("screen_not_contains", "s07b_after", "[200~"),
    ("screen_not_contains", "s07b_after", "[201~"),
    ("editor_not_contains", "s07b_cleared", "TOK"),
    ("restore_markers",),
]
