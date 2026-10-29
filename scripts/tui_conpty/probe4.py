NAME = "probe4"
ENV = {"PIR_DISABLE_TERMINAL_QUERY": "1"}
OPS = [
    ("wait_quiet", 2.0),
    ("log", "HATCH: no terminal query; /quit should submit and exit"),
    ("send", b"/quit\r"),
    ("wait", 2.5),
]
