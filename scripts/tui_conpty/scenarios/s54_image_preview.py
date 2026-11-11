# -*- coding: utf-8 -*-
# Round 36 lane-tui Task B: /image local preview (terminal.showImages default
# true) must render a preview (halfblocks or the upstream text fallback line)
# without corrupting the session or leaking control sequences. Manual screen
# inspection is recorded in the round36 handoff; checks stay structural.
NAME = "s54_image_preview"
TIMEOUT = 120
MOCK_SCRIPT = [{"text": "IMAGE-REPLY-MARKER"}]

IMAGE = "C:/wp/empty/pir/.play/evidence/lane-tui/round36-paste-theme/img36.png"

OPS = [
    ("wait_quiet", 5.0),
    ("log", "/image local 2x2 PNG"),
    ("send", ("/image " + IMAGE + "\r").encode("utf-8")),
    ("wait", 4.0),
    ("snap", "s54_preview"),
    ("log", "normal turn after image preview"),
    ("send", b"IMAGE-PROMPT-MARKER\r"),
    ("wait", 8.0),
    ("wait_quiet", 3.0),
    ("snap", "s54_after"),
]

CHECKS = [
    ("screen_not_contains", "s54_preview", "[200~"),
    ("screen_not_contains", "s54_preview", "[201~"),
    ("screen_not_contains", "s54_preview", "\ufffd"),
    ("screen_contains", "s54_after", "IMAGE-PROMPT-MARKER"),
    ("screen_contains", "s54_after", "IMAGE-REPLY-MARKER"),
    ("restore_markers",),
]
