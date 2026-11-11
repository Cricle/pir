NAME = "probe_trust3"
ENV = {"RUST_LOG": "pir=debug,trace"}
OPS = [("wait_quiet", 4), ("send", b"1\r"), ("wait", 5), ("snap", "p3_after")]
