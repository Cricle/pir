# -*- coding: utf-8 -*-
# Probe: what does a real user see/experience when the trust selector is
# outstanding at startup? Answer option 1 ("Trust"), then type in the editor.
NAME = "probe_trust"
OPS = [
    ("wait_quiet", 4),
    ("snap", "probe_trust_prompt"),
    ("send", b"1\r"),
    ("wait", 4),
    ("snap", "probe_trust_after_choice"),
    ("send", b"hello-after-trust"),
    ("wait", 1.2),
    ("snap", "probe_trust_typed"),
]
