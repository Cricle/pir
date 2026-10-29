# -*- coding: utf-8 -*-
# Foreman probe: where does the app park after a single /quit? PI_TIMING prints
# the tui phase report only after cleanup; its absence localizes the hang.
NAME = "p22_quit_timing"
SEED_AGENT = False
TIMEOUT = 40
ENV = {"PI_TIMING": "1", "RUST_LOG": "debug"}
OPS = [
    ("wait_quiet", 4),
    ("send", b"/quit\r"),
    ("wait", 12.0),
    ("snap", "p22_after_quit"),
]
CHECKS = []
