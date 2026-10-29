# -*- coding: utf-8 -*-
# PLAY p02 — typing edges in the editor (no submit): long line, CJK/emoji,
# arrows, ctrl+a/e/u/k/w, tab, shift+tab, ctrl+o, esc, ctrl+c.
NAME = "p02_typing_edges"
SEED_AGENT = False
TIMEOUT = 120
OPS = [
    ("wait_quiet", 4),
    # 1) long line: 300 ASCII chars -> wrapping in the editor box
    ("send", b"x" * 300),
    ("wait", 1.0),
    ("snap", "p02_long"),
    ("send", b"\x15"),          # ctrl+u clear line
    ("wait", 0.6),
    ("snap", "p02_ctrl_u"),
    # 2) CJK + emoji (utf-8 bytes) and arrow navigation
    ("send", "中文测试".encode("utf-8") + "a".encode() + "🙂".encode("utf-8") + "b".encode()),
    ("wait", 1.0),
    ("snap", "p02_cjk"),
    ("send", b"\x1b[D" * 2),    # left x2
    ("wait", 0.5),
    ("send", b"\x01\x0b"),      # ctrl+a then ctrl+k (kill to end)
    ("wait", 0.8),
    ("snap", "p02_ctrl_a_k"),
    ("send", b"\x17"),          # ctrl+w (delete word)
    ("wait", 0.6),
    ("snap", "p02_ctrl_w"),
    ("send", b"\x19"),          # ctrl+y (yank)
    ("wait", 0.6),
    ("snap", "p02_ctrl_y"),
    ("send", b"\x15"),          # clear
    ("wait", 0.5),
    # 3) tab / shift+tab / ctrl+o / esc on an empty editor
    ("snap", "p02_before_keys"),
    ("send", b"\t"),
    ("wait", 0.8),
    ("snap", "p02_tab"),
    ("send", b"\x1b[Z"),
    ("wait", 0.8),
    ("snap", "p02_shift_tab"),
    ("send", b"\x0f"),
    ("wait", 0.8),
    ("snap", "p02_ctrl_o"),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("snap", "p02_esc"),
    ("send", b"\x03"),
    ("wait", 0.6),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
