# -*- coding: utf-8 -*-
# Timeline probe: answer the trust prompt, then snap every ~0.5s for 12s.
NAME = "probe_trust_timeline"
OPS = [("wait_quiet", 4), ("send", b"1\r")]
for i in range(24):
    OPS.append(("wait", 0.5))
    OPS.append(("snap", "tl%02d" % i))
