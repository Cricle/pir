# -*- coding: utf-8 -*-
# pi 1.3.0 wave (2026-10-20) — startup update-version notice removed.
#
# The TUI used to render a transcript notice fetched at startup:
#   `New version 1.2.0 is available. Run `pir update` to upgrade.`
# (recorded in the 2026-10-10 acc3 snapshots under out/, rows 11-12). The
# whole startup network check was deleted (`src/tui/update_check.rs` keeps
# only the semver helpers; `pir update` remains an explicit command), so no
# version-available line and no `pir update` suggestion may reach the screen
# or the raw stream.
#
# The child stays online-gated (OFFLINE=False, no PI_OFFLINE), so a
# reintroduced startup check cannot be masked by the harness.
#
# Positive control: the boot footer's `ctrl+o more` hint must be present, so
# the negative checks cannot pass on a blank/crashed screen. The later idle
# snapshot covers an asynchronous check that lands after the first frame, and
# the RAW final log covers a notice that was painted and then erased.
NAME = "s42_no_startup_update_notice"
OFFLINE = False
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4.0),
    ("log", "boot settled: no startup update-version notice"),
    ("snap", "s42_boot"),
    ("wait", 1.0),
    ("snap", "s42_idle"),
]
CHECKS = [
    # Positive control: the boot frame really rendered.
    ("screen_contains", "s42_boot", "ctrl+o more"),
    # No notice on the settled boot screen (old shape: `New version <x.y.z>
    # is available. Run `pir update` to upgrade.`).
    ("screen_not_regex", "s42_boot", r"(?i)new version"),
    ("screen_not_regex", "s42_boot", r"(?i)update available"),
    ("screen_not_contains", "s42_boot", "pir update"),
    # ... and none arrives late either (a startup check may resolve async).
    ("screen_not_regex", "s42_idle", r"(?i)new version"),
    ("screen_not_regex", "s42_idle", r"(?i)update available"),
    ("screen_not_contains", "s42_idle", "pir update"),
    # Strongest form: the text may never be written to the stream at all,
    # even if a later repaint erased it from the visible screen.
    ("log_not_regex", "@final", r"(?i)new version"),
    ("log_not_regex", "@final", r"(?i)update available"),
    ("restore_markers",),
]
