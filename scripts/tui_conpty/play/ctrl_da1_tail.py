NAME = "ctrl_da1_tail"
SEED_AGENT = False
TIMEOUT = 40
# The leak shape as it was observed on screen (crossterm drops the leading ESC
# of a queued reply record, leaving the tail as plain characters).
OPS = [("wait_quiet", 4), ("send", b"[?61;6;7c"), ("wait", 1.0), ("snap", "tail_after")]
CHECKS = []
