# -*- coding: utf-8 -*-
# Ported from r2b_s4_space.py. Intent: typing "XY " (word + space) must NOT pop
# the whole-cwd file listing; the text renders in the editor and nothing else.
# The scenario's agent dir is seeded with models.json/settings.json, so if the
# listing regresses those file names show up on screen and the check fails.
NAME = "s04_space_no_filelist"
OPS = [
    ("wait_quiet", 4),
    ("log", "type 'XY ' -> no cwd file listing"),
    ("send", b"XY "),
    ("wait", 1.5),
    ("snap", "s04_after"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 12),
    ("wait", 0.6),
    ("snap", "s04_cleared"),
]
CHECKS = [
    ("editor_contains", "s04_after", "XY"),
    ("screen_not_contains", "s04_after", "models.json"),
    ("screen_not_contains", "s04_after", "settings.json"),
    ("editor_not_contains", "s04_cleared", "XY"),
    ("restore_markers",),
]
