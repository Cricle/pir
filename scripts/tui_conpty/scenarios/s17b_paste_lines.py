# -*- coding: utf-8 -*-
# s17b — a multi-line BRACKETED paste must keep its line breaks (Windows).
#
# Why this exists (2026-10-15): on this ConPTY the bracketed-paste markers
# never reach the app (the console translates the paste itself and swallows
# them — see s17_paste_fold's header and probe_paste_records.py), so pasted
# text arrives as synthesized key records and there is no `Event::Paste` to
# fold. What the app CAN get wrong is the payload itself: a raw `\n` is
# encoded by the console as a Ctrl+Enter chord (VK_CONTROL down/up around a
# VK_RETURN record with `uChar` `\n` and LEFT_CTRL_PRESSED), which crossterm
# reports as `Enter` + CONTROL. Before the fix the gate dropped that shape and
# the editor received one concatenated line
# (out/calib-1/s17_paste_fold: rows `fold-line-01fold-line-02…`), i.e. a pasted
# 12-line payload lost every line break. The fix resolves the chord to the LF
# key shape in the gate (`src/tui/stdin_buffer.rs`, `resolve_console_line_feed`
# called from `StdinGate::push`), so each `\n` inserts a line break.
#
# EXPECTED and CONFIRMED after the fix (battery run out/20261015-172010 on the
# binary with `resolve_console_line_feed`, target-alt mtime 17:19:05, 20/20
# overall): the editor holds the 12 lines and shows the last three
# (editor region rows 12..14), the row ` fold-line-12` stands alone, nothing is
# submitted, and ctrl+c clears it. The 3-row window is observed input-box
# behaviour for >3 lines (out/probe_s17_clip10/: `↑ 7 more` plus the last
# three rows).
#
# Pre-fix run for contrast (out/20261015-171248, same payload/binary minus the
# gate fix): BOTH discriminating checks failed — the row regex because the
# whole payload was one wrapped line, and `screen_not_contains(...)` because
# that line reads `fold-line-01fold-line-02…` (calib-1 showed the same
# concatenation). Cleanup: ctrl+c clears the editor, backspaces as a safety
# net.
NAME = "s17b_paste_lines"
TIMEOUT = 90
_PAYLOAD = "\n".join("fold-line-%02d" % i for i in range(1, 13))
OPS = [
    ("wait_quiet", 4),
    ("log", "bracketed 12-line paste -> line breaks must survive in the editor"),
    ("send", b"\x1b[200~" + _PAYLOAD.encode("ascii") + b"\x1b[201~"),
    ("wait", 1.5),
    ("snap", "s17b_after"),
    ("log", "cleanup: ctrl+c clears the editor, backspaces safety net"),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "s17b_cleared"),
]
CHECKS = [
    # The last pasted line must be its own editor row (pre-fix the whole
    # payload is a single wrapped line, so this row holds
    # `10fold-line-11fold-line-12`).
    ("screen_regex", "s17b_after", r"(?m)^\s*fold-line-12\s*$"),
    # Discriminating check for the lost newlines: never concatenated.
    ("screen_not_contains", "s17b_after", "fold-line-01fold-line-02"),
    # Nothing was submitted: the payload is still the editor's content.
    ("editor_contains", "s17b_after", "fold-line-12"),
    # The paste markers must never be typed or decoded.
    ("screen_not_contains", "s17b_after", "[200~"),
    ("screen_not_contains", "s17b_after", "[201~"),
    ("editor_not_contains", "s17b_cleared", "fold-line-"),
    ("restore_markers",),
]
