# -*- coding: utf-8 -*-
# Ported from r2b_s6_settings.py. Intent: /settings opens the Settings dialog
# ("Settings" title + the "enter select" hint row); navigating to a value row
# and pressing right changes it and shows the exact key path notice
# ("Saved setting: <path>"); Esc closes the dialog again.
NAME = "s06_settings_dialog"
OPS = [
    ("wait_quiet", 4),
    ("log", "open /settings"),
    ("send", b"/settings\r"),
    ("wait", 1.5),
    ("snap", "s06_open"),
    ("log", "down down -> a value row"),
    ("send", b"\x1b[B\x1b[B"),
    ("wait", 0.6),
    ("snap", "s06_nav"),
    ("log", "right -> change the value"),
    ("send", b"\x1b[C"),
    ("wait", 1.2),
    ("snap", "s06_changed"),
    ("log", "esc -> close the dialog"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "s06_closed"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 24),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s06_open", "Settings"),
    ("screen_contains", "s06_open", "enter select"),
    ("screen_contains", "s06_changed", "Saved setting: "),
    ("screen_not_contains", "s06_closed", "enter select"),
    ("restore_markers",),
]
