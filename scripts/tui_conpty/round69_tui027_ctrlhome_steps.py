# -*- coding: utf-8 -*-
"""Round 69 (foreman): does repeated Ctrl+Home converge to entry 0?

lane-fix-historyreset measured "14 presses to reach the earliest content" before
the PIR-TUI-028 spin fix. This probe presses Ctrl+Home N times with a settle in
between and records which ENTRYMARK the top of the view shows after each press,
so the convergence (or lack of it) is a table, not an anecdote.

Run: python scripts/tui_conpty/round69_tui027_ctrlhome_steps.py
Env: PIR_TUI_EXE, PIR_CH_OUT (evidence dir), PIR_CH_PRESSES (default 16)
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from run_battery import _win_long_path, encode_cwd_dir_name, seed_agent_dir  # noqa: E402
from round67_tui027_truncation import seed_session, marker  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.abspath(
    os.environ.get("PIR_CH_OUT")
    or os.path.join(REPO, ".play", "evidence", "ab-round1", "lane-fix-tui027", "ctrlhome")
)
PIR = os.path.abspath(
    os.environ.get("PIR_TUI_EXE")
    or os.path.join(REPO, ".play", "build", "gate-foreman", "debug", "pir.exe")
)
CTRL_HOME = b"\x1b[1;5H"
COUNT = 900
PAYLOAD = 1200
PRESSES = int(os.environ.get("PIR_CH_PRESSES", "16"))


def screen_rows(raw, rows=40, cols=120):
    return acc3_render.render(raw)


def first_marker(rows):
    for row in rows:
        if "ENTRYMARK" in row:
            i = row.index("ENTRYMARK")
            digits = row[i + 9:i + 13]
            return int(digits) if digits.isdigit() else None
    return None


def main():
    import os as _os
    _os.makedirs(OUT, exist_ok=True)
    home = os.path.join(OUT, "home")
    agent = os.path.join(home, ".pi", "agent")
    _os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(OUT, "project")
    _os.makedirs(project, exist_ok=True)
    session = seed_session(agent, project, COUNT, PAYLOAD)
    env = {
        "PIR_TRUST_PROJECT": "1",
        "HOME": home,
        "USERPROFILE": home,
        "PIR_AGENT_DIR": agent,
        "PIR_NO_UPDATE_CHECK": "1",
    }
    tui_drive.PIR = PIR
    shots = os.path.join(OUT, "shots")
    _os.makedirs(shots, exist_ok=True)

    steps = []
    ops = [("wait_quiet", 6.0), ("wait", 1.0), ("snap", "s00")]
    for n in range(1, PRESSES + 1):
        ops.append(("send", CTRL_HOME))
        ops.append(("wait_quiet", 8.0))
        ops.append(("wait", 1.0))
        ops.append(("snap", f"s{n:02d}"))
    result = tui_drive.run_scenario(
        "ctrlhome", ops, extra_env=env, cwd=project,
        args=["--session", session],
        shots_dir=shots,
        driver_log=os.path.join(shots, "ctrlhome.driver.log"),
        result_json=os.path.join(shots, "ctrlhome.result.json"),
        agent_dir=agent, timeout=600,
    )
    for n in range(0, PRESSES + 1):
        raw = open(os.path.join(shots, f"s{n:02d}.log"), "rb").read()
        rows = screen_rows(raw)
        steps.append({
            "press": n,
            "first_marker": first_marker(rows),
            "has_hint": any("earlier entr" in r for r in rows),
            "hint": next((r.strip() for r in rows if "earlier entr" in r), None),
        })
    with open(os.path.join(OUT, "result.json"), "w", encoding="utf-8") as f:
        json.dump({"binary": PIR, "count": COUNT, "payload": PAYLOAD,
                   "steps": steps, "exit_code": result.get("exit_code")}, f, indent=2)
    for row in steps:
        print(json.dumps(row, ensure_ascii=False))


if __name__ == "__main__":
    main()