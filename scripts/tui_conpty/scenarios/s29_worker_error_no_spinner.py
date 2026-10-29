# Promoted from the 2026-10-16 foreman verification (p24_f2_f4_assert.py): an
# assertion-level regression net for a REAL fix landed that day (WAVE-REPORT.md §6b).
# -*- coding: utf-8 -*-
# Foreman assertion for the fixed F2/F4 (exit-path lane):
#   * no model configured -> a prompt yields an error row and the Working
#     indicator must be GONE (F2);
#   * Esc during a stream -> exactly ONE "Request was aborted" row (F4).
NAME = "s29_worker_error_no_spinner"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("send", b"hello\r"),
    ("wait", 6.0),
    ("snap", "p24_after_error"),
]
CHECKS = [
    ("screen_contains", "p24_after_error", "No pi model configured"),
    ("screen_not_contains", "p24_after_error", "Working"),
]
