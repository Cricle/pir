NAME = "ctrl_osc_quit"
SEED_AGENT = False
TIMEOUT = 40
OPS = [("wait_quiet", 4), ("send", b"\x1b]11;rgb:1a1a/1a1a/2b2b\x07"), ("wait", 1.0), ("send", b"hello-late"), ("wait", 1.0), ("snap", "ctrl_after")]
CHECKS = [("restore_markers",)]
