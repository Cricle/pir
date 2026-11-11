# -*- coding: utf-8 -*-
"""Round-4 lane-tui journey 2: interruption + editor/terminal behaviours.

Real TUI session: Esc aborts a running bash tool, Ctrl+O toggles output
expansion, a 3-line bracketed paste keeps its line breaks, the console is
resized, the /resume picker opens/cancels, and /compact on a small session
surfaces the real error. Mock LLM is loopback; all product paths are real.

Usage:
  PYTHONIOENCODING=utf-8 python round4_journey2.py
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
os.environ["PIR_TUI_EXE"] = os.path.join(REPO, "target-gnu", "debug", "pir.exe")

import run_battery  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402

EVID = os.path.join(REPO, ".play", "evidence", "lane-tui", "round4-journey2")
WORK = os.path.join(EVID, "work")

MOCK_SCRIPT = [
    {"tool_call": {"name": "bash", "arguments": {"command": "sleep 20"}}},
    {"text": "Long task finished."},
]

PAYLOAD = "\n".join(["alpha line 1", "beta line 2", "gamma line 3"])

OPS = [
    ("wait_quiet", 4),
    ("snap", "r4j2_boot"),
    ("log", "start a long bash tool; Esc must abort it"),
    ("send", b"Run the long check.\r"),
    ("wait", 2.0),
    ("snap", "r4j2_running"),
    ("send", b"\x1b"),
    ("wait", 2.5),
    ("wait_quiet", 2),
    ("snap", "r4j2_interrupted"),
    ("log", "ctrl+o toggle on/off"),
    ("send", b"\x0f"),
    ("wait", 0.8),
    ("snap", "r4j2_expanded"),
    ("send", b"\x0f"),
    ("wait", 0.8),
    ("snap", "r4j2_collapsed"),
    ("log", "3-line bracketed paste keeps line breaks"),
    ("send", b"\x1b[200~" + PAYLOAD.encode("ascii") + b"\x1b[201~"),
    ("wait", 1.2),
    ("snap", "r4j2_pasted"),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("log", "resize console 100x30"),
    ("resize", 100, 30),
    ("wait", 1.0),
    ("snap", "r4j2_resized"),
    ("log", "resize back to 120x40 (the pyte canvas size) to separate app redraw from harness cropping"),
    ("resize", 120, 40),
    ("wait", 1.0),
    ("snap", "r4j2_size_back"),
    ("log", "/resume picker open + cancel"),
    ("send", b"/resume\r"),
    ("wait", 2.0),
    ("snap", "r4j2_resume_picker"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("log", "/compact on a small session"),
    ("send", b"/compact\r"),
    ("wait", 2.5),
    ("snap", "r4j2_compact"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]

CHECKS = [
    ("screen_contains", "r4j2_running", "$ sleep 20"),
    ("screen_contains", "r4j2_interrupted", "aborted"),
    ("screen_contains", "r4j2_expanded", "ctrl+o more"),
    ("screen_contains", "r4j2_collapsed", "ctrl+o more"),
    ("screen_regex", "r4j2_pasted", r"(?m)^\s*gamma line 3\s*$"),
    ("screen_not_contains", "r4j2_pasted", "alpha line 1beta line 2"),
    ("screen_not_contains", "r4j2_pasted", "[200~"),
    ("screen_contains", "r4j2_resized", "ctrl+o more"),
    ("screen_contains", "r4j2_compact", "nothing to compact"),
    ("restore_markers",),
]


def main():
    os.makedirs(EVID, exist_ok=True)
    os.makedirs(WORK, exist_ok=True)
    run_battery.seed_agent_dir(WORK, True)
    srv = MockServer(log_path=os.path.join(EVID, "mock_requests.jsonl"), script=MOCK_SCRIPT)
    try:
        injected = run_battery.inject_mock_models(WORK, srv.base_url)
        with open(os.path.join(EVID, "setup.json"), "w", encoding="utf-8") as f:
            json.dump({"work": WORK, "mock": srv.base_url, "injected": injected,
                       "pir": os.environ["PIR_TUI_EXE"]}, f, indent=2)
        tui_drive.run_scenario(
            "round4_journey2",
            OPS,
            shots_dir=EVID,
            driver_log=os.path.join(EVID, "round4_journey2.driver.log"),
            result_json=os.path.join(EVID, "run.result.json"),
            agent_dir=WORK,
            timeout=90,
        )
        with open(os.path.join(EVID, "mock_count.json"), "w", encoding="utf-8") as f:
            json.dump({"requests": srv.request_count}, f)
        print(f"mock requests: {srv.request_count}")
    finally:
        srv.stop()


if __name__ == "__main__":
    main()
