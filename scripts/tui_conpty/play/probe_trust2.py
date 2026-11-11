# -*- coding: utf-8 -*-
# Probe: answer the trust prompt (ask path) with a model configured; then type
# in the editor. Distinguishes a trust-flow defect from a no-model blank UI.
NAME = "probe_trust2"
OPS = [
    ("wait_quiet", 4),
    ("send", b"1\r"),
    ("wait", 5),
    ("snap", "probe_trust2_after_choice"),
    ("send", b"hello-after-trust"),
    ("wait", 1.2),
    ("snap", "probe_trust2_typed"),
]
