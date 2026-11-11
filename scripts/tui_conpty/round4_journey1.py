# -*- coding: utf-8 -*-
"""Round-4 lane-tui journey 1: a real TUI fix-the-failing-test task.

Creates a throwaway Python project (real files, real unittest), starts the
hermetic loopback mock LLM, and drives `pir interactive` through the ConPTY
driver. The model is scripted (mock side only); every file mutation and bash
execution is the real product path.

Usage:
  PYTHONIOENCODING=utf-8 python round4_journey1.py
"""
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
os.environ["PIR_TUI_EXE"] = os.path.join(REPO, "target-gnu", "debug", "pir.exe")

import run_battery  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402

EVID = os.path.join(REPO, ".play", "evidence", "lane-tui", "round4-journey1")
PROJECT = os.path.join(EVID, "project")
PY = "C:/wp/empty/tools/python312/python.exe"

CALC = "def add(a, b):\n    return a - b\n\n\ndef mul(a, b):\n    return a * b\n"
TEST = (
    "import unittest\n"
    "from calc import add\n\n\n"
    "class CalcTests(unittest.TestCase):\n"
    "    def test_add(self):\n"
    "        self.assertEqual(add(2, 3), 5)\n"
    "        self.assertEqual(add(-1, 1), 0)\n\n\n"
    "if __name__ == \"__main__\":\n"
    "    unittest.main()\n"
)

MOCK_SCRIPT = [
    {"tool_call": {"name": "read", "arguments": {"path": "test_calc.py"}}},
    {"tool_call": {"name": "read", "arguments": {"path": "calc.py"}}},
    {"tool_call": {"name": "edit", "arguments": {
        "path": "calc.py",
        "oldText": "def add(a, b):\n    return a - b",
        "newText": "def add(a, b):\n    return a + b",
    }}},
    {"tool_call": {"name": "bash", "arguments": {
        "command": f'"{PY}" -m unittest -v test_calc',
    }}},
    {"text": "Fixed add() to return a + b. The unittest suite passes."},
]

OPS = [
    ("wait_quiet", 4),
    ("snap", "r4j1_boot"),
    ("log", "TUI /todos must dispatch natively (zero model requests)"),
    ("send", b"/todos\r"),
    ("wait", 1.5),
    ("snap", "r4j1_todos"),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("log", "prompt: fix the failing test using read/edit/bash"),
    ("send", b"Fix the failing Python test in this project. Read the test and the implementation, "
             b"fix the bug with the edit tool, run the unittest suite with bash, and summarize.\r"),
    ("wait", 6.0),
    ("wait_quiet", 3.0),
    ("snap", "r4j1_after"),
    ("send", b"\x7f" * 20),
    ("wait", 0.5),
]

CHECKS = [
    ("screen_contains", "r4j1_boot", "ctrl+o more"),
    ("restore_markers",),
]


def main():
    os.makedirs(EVID, exist_ok=True)
    if os.path.isdir(PROJECT):
        import shutil
        shutil.rmtree(PROJECT)
    os.makedirs(PROJECT, exist_ok=True)
    with open(os.path.join(PROJECT, "calc.py"), "w", encoding="utf-8", newline="\n") as f:
        f.write(CALC)
    with open(os.path.join(PROJECT, "test_calc.py"), "w", encoding="utf-8", newline="\n") as f:
        f.write(TEST)

    # Real pre-run failing test (outside the TUI), saved as evidence.
    before = subprocess.run([PY, "-m", "unittest", "-v", "test_calc"], cwd=PROJECT,
                            capture_output=True, text=True)
    with open(os.path.join(EVID, "test_before.log"), "w", encoding="utf-8") as f:
        f.write(f"exit={before.returncode}\n--- stdout ---\n{before.stdout}\n--- stderr ---\n{before.stderr}\n")

    run_battery.seed_agent_dir(PROJECT, True)
    srv = MockServer(log_path=os.path.join(EVID, "mock_requests.jsonl"), script=MOCK_SCRIPT)
    try:
        injected = run_battery.inject_mock_models(PROJECT, srv.base_url)
        with open(os.path.join(EVID, "setup.json"), "w", encoding="utf-8") as f:
            json.dump({"project": PROJECT, "mock": srv.base_url, "injected": injected,
                       "pir": os.environ["PIR_TUI_EXE"], "script_steps": len(MOCK_SCRIPT)}, f, indent=2)
        result_json = os.path.join(EVID, "run.result.json")
        tui_drive.run_scenario(
            "round4_journey1",
            OPS,
            shots_dir=EVID,
            driver_log=os.path.join(EVID, "round4_journey1.driver.log"),
            result_json=result_json,
            agent_dir=PROJECT,
            timeout=90,
        )
        rec = {"requests": srv.request_count}
        with open(os.path.join(EVID, "mock_count.json"), "w", encoding="utf-8") as f:
            json.dump(rec, f)
        print(f"mock requests: {srv.request_count}")
    finally:
        srv.stop()

    after = subprocess.run([PY, "-m", "unittest", "-v", "test_calc"], cwd=PROJECT,
                           capture_output=True, text=True)
    with open(os.path.join(EVID, "test_after.log"), "w", encoding="utf-8") as f:
        f.write(f"exit={after.returncode}\n--- stdout ---\n{after.stdout}\n--- stderr ---\n{after.stderr}\n")
    print(f"after exit={after.returncode}")


if __name__ == "__main__":
    main()
