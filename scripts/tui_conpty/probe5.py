NAME = "probe5"
OPS = [
    ("wait_quiet", 2.0),
    ("send", b"abc"),
    ("wait", 0.8),
    ("snap", "acc3_probe5_abc"),
    ("send", b"\x7f"),
    ("wait", 0.8),
    ("snap", "acc3_probe5_bs"),
    ("send", b"\x04"),
    ("wait", 1.5),
]
