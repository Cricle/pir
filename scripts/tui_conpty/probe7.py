NAME = "probe7"
ENV = {"PI_TIMING": "1"}
OPS = [
    ("wait_quiet", 2.0),
    ("send", b"/quit\r"),
    ("wait", 2.0),
]
