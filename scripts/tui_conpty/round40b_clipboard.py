# -*- coding: utf-8 -*-
"""Round 40b lane-tui: real system-clipboard text paste (alt+v) into the editor."""
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round40-clipboard")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
SHOTS = os.path.join(ROOT, "shots")
TEXT = "R40-CLIP 中文剪贴板 line2"


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    home = os.path.join(ROOT, "runs", "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"), script=None)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    ops = [
        ("wait_quiet", 5.0),
        ("log", "clipset + alt+v paste"),
        ("clipset", TEXT),
        ("send", b"\x1bv"), ("wait", 1.5),
        ("snap", "c_pasted"),
        ("log", "submit pasted text"),
        ("send", b"\r"), ("wait", 6.0), ("wait_quiet", 2.0),
        ("snap", "c_submitted"),
    ]
    tui_drive.PIR = PIR
    rj = os.path.join(SHOTS, "c_clipboard.result.json")
    try:
        tui_drive.run_scenario("c_clipboard", ops, extra_env={
            "PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
            "PIR_AGENT_DIR": agent}, cwd=project, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, "c_clipboard.driver.log"),
            result_json=rj, agent_dir=agent, timeout=90)
    finally:
        mock.stop()
    with open(rj, encoding="utf-8") as f:
        out = json.load(f)
    result = {"pir": PIR, "exit_code": out.get("exit_code"),
              "timed_out": out.get("timed_out"), "checks": {}}
    for snap, needle, mode in [("c_pasted", TEXT, "present"),
                               ("c_pasted", "R40-CLIP", "present"),
                               ("c_submitted", "R40-CLIP", "present")]:
        raw = open(os.path.join(SHOTS, snap + ".log"), "rb").read()
        rows = acc3_render.render(raw)
        n = sum(r.count(needle) for r in rows)
        result["checks"][f"{snap}:{mode}:{needle}"] = {
            "ok": n >= 1 if mode == "present" else n == 0, "count": n}
        bad = raw.count(b"\xef\xbf\xbd")
        result["checks"][f"{snap}:no_mojibake"] = {"ok": bad == 0, "count": bad}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    ok = sum(1 for v in result["checks"].values() if v["ok"])
    print(f"exit={result['exit_code']} timed_out={result['timed_out']} checks={len(result['checks'])} ok={ok}")
    return 0 if ok == len(result["checks"]) and result["exit_code"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
