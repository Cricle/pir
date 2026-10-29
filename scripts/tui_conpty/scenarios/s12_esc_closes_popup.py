# -*- coding: utf-8 -*-
# New (battery starter). Intent: the command-completion popup opened by typing
# "/help" is closed by Esc WITHOUT executing or destroying the typed text
# (hotkeys table: "esc - Cancel autocomplete"); a later clear removes it.
NAME = "s12_esc_closes_popup"
OPS = [
    ("wait_quiet", 4),
    ("log", "type /help -> completion popup open"),
    ("send", b"/help"),
    ("wait", 1.5),
    ("snap", "s12_popup"),
    ("log", "esc must close the popup, not submit"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "s12_after_esc"),
    ("log", "clear editor"),
    ("send", b"\x7f" * 12),
    ("wait", 0.6),
    ("snap", "s12_cleared"),
]
CHECKS = [
    ("screen_contains", "s12_popup", "Show available commands"),
    ("screen_not_contains", "s12_after_esc", "Show available commands"),
    ("editor_contains", "s12_after_esc", "/help"),
    ("editor_not_contains", "s12_cleared", "/help"),
    ("restore_markers",),
]
