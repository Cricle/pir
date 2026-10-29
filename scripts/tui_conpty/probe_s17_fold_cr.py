# -*- coding: utf-8 -*-
# Probe (2026-10-15, s17 recalibration, app-level, CR-only). Same question as
# probe_s17_fold_crlf.py with every LF removed: the lines are joined by bare
# CRs inside the bracketed markers. If the gate collected a paste, the CRs
# would be payload and the editor would fold; if the console swallowed the
# markers, each CR is a real Enter and each line is submitted.
# OBSERVED (out/probe_s17_cr/): 12 separate submissions, zero `[paste` on
# screen — the cleanest proof that no paste event reaches the app on this
# ConPTY (probe_paste_records.py has the record-level mechanism).
NAME = "probe_s17_fold_cr"
OPS = [
    ("wait_quiet", 4),
    ("log", "CR-only 12-line paste -> does a paste event exist at all?"),
    ("send", b"\x1b[200~" + ("\r".join("fold-line-%02d" % i for i in range(1, 13))).encode("ascii") + b"\x1b[201~"),
    ("wait", 1.5),
    ("snap", "probe_cr_after"),
    ("log", "cleanup: ctrl+c clears the editor, backspaces safety net"),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "probe_cr_cleared"),
]
CHECKS = []
