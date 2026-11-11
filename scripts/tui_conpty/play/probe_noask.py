# -*- coding: utf-8 -*-
# Probe: same startup with PIR_TRUST_PROJECT=1 (operator override): type in the
# editor and check the text lands there (no competing startup prompt).
NAME = "probe_noask"
ENV = {"PIR_TRUST_PROJECT": "1"}
OPS = [
    ("wait_quiet", 5),
    ("snap", "probe_noask_boot"),
    ("send", b"hello-after-trust"),
    ("wait", 1.2),
    ("snap", "probe_noask_typed"),
]
