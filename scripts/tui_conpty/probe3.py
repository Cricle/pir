NAME = "probe3"
OPS = [
    ("wait_quiet", 2.0),
    ("log", "LIVENESS: ctrl+d exits?"),
    ("send", b"\x04"),
    ("wait", 2.5),
]
