# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/acc3_s03.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# Scenario 2+3: real dialog, streaming render, tool call, ctrl+o, /session, /copy
NAME = "acc3_dialog_tool"
OPS = [
    ("wait_quiet", 2.0),
    ("log", "prompt 1: plain reply (streaming render)"),
    ("send", b"Reply with exactly one short sentence: hello from pir test.\r"),
    ("wait_quiet", 3.0),
    ("snap", "acc3_s03_after_reply"),
    ("log", "ctrl+o toggle tool expansion (no-op visual)"),
    ("send", b"\x0f"),
    ("wait", 0.8),
    ("snap", "acc3_s03_ctrl_o"),
    ("log", "prompt 2: tool call bash"),
    ("send", b"Use the bash tool to run: echo pir-tool-ok\r"),
    ("wait_quiet", 4.0),
    ("snap", "acc3_s03_after_tool"),
    ("log", "ctrl+o expand/collapse tool output"),
    ("send", b"\x0f"),
    ("wait", 0.8),
    ("snap", "acc3_s03_tool_expanded"),
    ("send", b"\x0f"),
    ("wait", 0.8),
    ("snap", "acc3_s03_tool_collapsed"),
    ("log", "/session"),
    ("send", b"/session\r"),
    ("wait_quiet", 2.0),
    ("snap", "acc3_s03_session"),
    ("log", "/copy (status notice expected but P1 not rendered)"),
    ("send", b"/copy\r"),
    ("wait_quiet", 1.5),
    ("snap", "acc3_s03_after_copy"),
]
