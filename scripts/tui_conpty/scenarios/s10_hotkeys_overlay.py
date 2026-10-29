# -*- coding: utf-8 -*-
# New (battery starter). Intent: /hotkeys is a real command - Enter executes it
# and the shortcut table renders. The notice is taller than the transcript
# viewport, so assert rows near its TAIL (follow-end keeps the end visible):
# the ctrl+o row and the external-editor row.
NAME = "s10_hotkeys_overlay"
OPS = [
    ("wait_quiet", 4),
    ("log", "/hotkeys executes"),
    ("send", b"/hotkeys\r"),
    ("wait", 2.0),
    ("snap", "s10_after"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 24),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s10_after", "Toggle tool output expansion"),
    ("screen_contains", "s10_after", "Edit message in external editor"),
    ("restore_markers",),
]
