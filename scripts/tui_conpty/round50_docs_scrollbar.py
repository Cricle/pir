# -*- coding: utf-8 -*-
"""Round 50 lane-tui: `fullscreenScrollbar` in regular vs fullscreen mode.

Docs bullet (docs/tui.md, "Mouse & selection") describes the scrollbar without
saying it is alt-screen-only. This drives a real TUI twice with
`fullscreenScrollbar: "always"`, overflows the transcript, and records whether
any `┃` is painted in regular mode (tuiMode: regular) and in fullscreen mode.
Also captures `/hotkeys` for the docs/keybindings.md spot check.
"""
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round50-docs")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
LONG = "\n".join("R50-DOC line %02d padding padding padding" % i
                 for i in range(100))


def run_leg(name, mode):
    base = os.path.join(ROOT, "runs", name)
    agent = os.path.join(base, "home", ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    settings_path = os.path.join(agent, "settings.json")
    with open(settings_path, encoding="utf-8") as f:
        settings = json.load(f)
    settings["tuiMode"] = mode
    settings["fullscreenScrollbar"] = "always"
    with open(settings_path, "w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"),
                      script=[{"text": LONG}])
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": os.path.join(base, "home"),
           "USERPROFILE": os.path.join(base, "home"), "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, [
                ("wait_quiet", 5.0), ("snap", name + "_boot"),
                ("send", b"R50 docs scrollbar please\r"), ("wait", 3.0),
                ("wait_quiet", 5.0), ("snap", name + "_turn"),
                ("send", b"/hotkeys\r"), ("wait", 2.0), ("wait_quiet", 3.0),
                ("snap", name + "_hotkeys"),
            ], extra_env=env, cwd=os.path.join(ROOT, "project"),
            shots_dir=shots, driver_log=os.path.join(shots, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, timeout=90)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    raw = open(os.path.join(shots, name + "_turn.log"), "rb").read()
    rows = acc3_render.render(raw)
    hk_raw = open(os.path.join(shots, name + "_hotkeys.log"), "rb").read()
    hk_rows = acc3_render.render(hk_raw)
    bar_rows = [r for r in rows if "┃" in r]
    return {"exit_code": out.get("exit_code"),
            "timed_out": out.get("timed_out"),
            "bar_rows": bar_rows[:5], "bar_count": len(bar_rows),
            "hotkeys": [r for r in hk_rows if "—" in r][:60],
            "no_mojibake": raw.count(b"\xef\xbf\xbd") == 0}


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)
    regular = run_leg("regular", "regular")
    full = run_leg("fullscreen", "fullscreen")
    checks = {
        "regular:no-scrollbar": {"ok": regular["bar_count"] == 0,
                                 "count": regular["bar_count"],
                                 "rows": regular["bar_rows"]},
        "fullscreen:scrollbar-painted": {"ok": full["bar_count"] > 0,
                                         "count": full["bar_count"],
                                         "rows": full["bar_rows"]},
        "regular:exit": {"ok": regular["exit_code"] == 0 and not regular["timed_out"]},
        "fullscreen:exit": {"ok": full["exit_code"] == 0 and not full["timed_out"]},
        "regular:hotkeys": {"ok": len(regular["hotkeys"]) >= 10},
    }
    result = {"binary": PIR, "checks": checks,
              "regular": regular, "fullscreen": full}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(os.path.join(ROOT, "shots"))
    print(json.dumps({"checks": {k: v["ok"] for k, v in checks.items()},
                      "regular_bars": regular["bar_count"],
                      "fullscreen_bars": full["bar_count"]}, indent=2))
    return 0 if all(v["ok"] for v in checks.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
