# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/acc3_run.py (copied 2026-10-15) into
# scripts/tui_conpty/. Only change: td.PIR is no longer hard-coded; it keeps
# the driver default (<repo>/target-alt/debug/pir.exe) and honors PIR_TUI_EXE.
# Runs the s03 dialog/tool scenario against the CURRENT target-alt build.
import sys, os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tui_drive as td
td.PIR = os.environ.get("PIR_TUI_EXE") or td.PIR  # default=target-alt; env wins
here = os.path.dirname(os.path.abspath(__file__))
ns = {}
exec(open(os.path.join(here, "acc3_s03.py"), encoding="utf-8").read(), ns)
td.run_scenario(ns["NAME"], ns["OPS"], extra_env=ns.get("ENV"), args=ns.get("ARGS"))
