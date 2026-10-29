# -*- coding: utf-8 -*-
# Ported from r2b_s3_bug.py. Intent: typing "/bug" selects /bug (not /debug) in
# the completion popup; Enter opens the transcript-consent dialog
# ("Include the session transcript?"); Enter #2 picks the default ("Yes") and
# the bug-report flow finishes with a notice. The battery runs this scenario
# with PI_OFFLINE=1 (default for provider-free scenarios), so the upload leg
# short-circuits to the deterministic notice "Bug reports upload in online
# mode only; exporting locally." - no network, no model.
# /share and /import must register in the completion popup as well.
NAME = "s03_bug_flow"
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4),
    ("log", "type /bug -> popup must select bug, not debug"),
    ("send", b"/bug"),
    ("wait", 1.2),
    ("snap", "s03_bug_popup"),
    ("log", "enter #1 -> consent dialog"),
    ("send", b"\r"),
    ("wait", 1.0),
    ("snap", "s03_bug_consent"),
    ("log", "enter #2 -> default Yes -> offline notice"),
    ("send", b"\r"),
    ("wait", 2.0),
    ("snap", "s03_bug_after"),
    ("log", "type /sh -> share completion"),
    ("send", b"/sh"),
    ("wait", 1.0),
    ("snap", "s03_share_popup"),
    ("log", "clear, type /import"),
    ("send", b"\x7f\x7f\x7f/import"),
    ("wait", 1.0),
    ("snap", "s03_import_popup"),
    ("log", "cleanup editor before the driver's /quit"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_regex", "s03_bug_popup", r"\u2192\s*bug\b"),
    ("screen_not_contains", "s03_bug_popup", "\u2192 debug"),
    ("screen_contains", "s03_bug_consent", "Include the session transcript?"),
    # CALIBRATED 2026-10-15: the offline short-circuit notice is rendered
    # verbatim (observed row 14 of s03_bug_after); the old guessed regex
    # `Bug report (uploaded:|exported to:|reports upload)` could never match
    # "Bug reports upload in online mode only; exporting locally.".
    ("screen_contains", "s03_bug_after",
     "Bug reports upload in online mode only; exporting locally."),
    ("screen_regex", "s03_share_popup", r"\u2192\s*share\b"),
    ("screen_regex", "s03_import_popup", r"\u2192\s*import\b"),
    ("restore_markers",),
]
