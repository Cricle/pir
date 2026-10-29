# -*- coding: utf-8 -*-
# New (battery starter). Intent: typed text renders in the EDITOR (input box
# region between the two box rules) before any submit, exactly once, and the
# editor can be cleared again. Cheap, provider-free sanity for the editor path.
NAME = "s09_type_renders"
OPS = [
    ("wait_quiet", 4),
    ("log", "type plain text, no Enter"),
    ("send", b"hello-tui-battery"),
    ("wait", 1.2),
    ("snap", "s09_typed"),
    ("log", "clear editor"),
    ("send", b"\x7f" * 25),
    ("wait", 0.6),
    ("snap", "s09_cleared"),
]
CHECKS = [
    ("editor_contains", "s09_typed", "hello-tui-battery"),
    ("screen_count", "s09_typed", "hello-tui-battery", 1),
    ("editor_not_contains", "s09_cleared", "hello-tui-battery"),
    ("restore_markers",),
]
