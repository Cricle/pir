# -*- coding: utf-8 -*-
"""Child for probe_alt_passthrough.py: write the alt-screen sequences."""
import sys
import time

sys.stdout.write("\x1b[?1049h")
sys.stdout.write("ALT-PROBE-OK\n")
sys.stdout.flush()
time.sleep(0.5)
sys.stdout.write("\x1b[?1049l")
sys.stdout.flush()
time.sleep(0.2)
