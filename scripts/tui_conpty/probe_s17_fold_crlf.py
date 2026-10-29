# -*- coding: utf-8 -*-
# Probe (2026-10-15, s17 recalibration, app-level). Purpose: decide whether the
# `ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~` route ever produces an `Event::Paste` in this
# environment. The payload is separated by CRLF instead of LF, so the paste
# content reaches the editor through the CR path (a plain `Enter`, which the
# gate maps to `\r` and the editor normalizes to `\n`) even if the LF chord is
# unmapped — if the gate had collected a paste, the 12 lines would fold into a
# `[paste #1 +12 lines]` marker.
# OBSERVED (out/probe_s17_crlf/, binary mtime 16:29): NO fold. The 12 lines were
# submitted as separate user messages, i.e. every CR was dispatched as an
# ordinary Enter — the console swallows the paste markers and synthesizes key
# records (see probe_paste_records.py), so the app never sees a paste and its
# collector stays dormant. This is what moved s17 to the clipboard path (which
# calls `Editor::paste_text` directly) and into s17b_paste_lines.
NAME = "probe_s17_fold_crlf"
OPS = [
    ("wait_quiet", 4),
    ("log", "CRLF paste of 12 lines -> does a paste event exist at all?"),
    ("send", b"\x1b[200~" + ("\r\n".join("fold-line-%02d" % i for i in range(1, 13))).encode("ascii") + b"\x1b[201~"),
    ("wait", 1.5),
    ("snap", "probe_crlf_after"),
    ("log", "cleanup: ctrl+c clears the editor, backspaces safety net"),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "probe_crlf_cleared"),
]
CHECKS = []
