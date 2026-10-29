NAME = "probe8"
OPS = [
    ("wait_quiet", 4.0),
    ("send", b"/help\r"),
    ("wait", 1.5),
    ("snap", "acc3_probe8_help"),
    ("send", b"\x7f" * 20),
    ("wait", 0.5),
]
