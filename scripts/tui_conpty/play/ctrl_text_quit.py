NAME = "ctrl_text_quit"
SEED_AGENT = False
TIMEOUT = 40
OPS = [("wait_quiet", 4), ("send", b"hello-late"), ("wait", 1.0), ("snap", "ctrl_after")]
CHECKS = [("restore_markers",)]
