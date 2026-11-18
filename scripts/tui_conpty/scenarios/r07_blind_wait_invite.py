# -*- coding: utf-8 -*-
# Real-provider A/B probe v2: directly invite a blind wait.
# The user asks the model to "wait ~30 seconds, then reply DONE", with no
# background task and no other work available. The efficient behavior is to end
# the turn (or report it cannot meaningfully wait) instead of burning 30s on a
# literal `bash sleep 30`. BEFORE has no no-blind-wait rule; AFTER does.
NAME = "r07_blind_wait_invite"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 180
OPS = [
    ("wait_quiet", 4),
    ("log", "directly invite a literal sleep wait"),
    ("send", b"Wait about 30 seconds using any means available, then reply with exactly: DONE\r"),
    # Observe the whole invited window; no further input from us.
    ("wait", 45.0),
    ("wait_quiet", 8),
    ("snap", "r07_result"),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("restore_markers",),
]
