# -*- coding: utf-8 -*-
"""Round 40 lane-tui: /help + /hotkeys (live keybinding override) and the
/import dialog error paths over the fixed pir36.exe snapshot.

Leg A help_hotkeys : /help command notice, then /hotkeys table; a settings.json
                     keybinding override (tui.editor.pageUp: ctrl+u) must appear
                     in the live table.
Leg B import_paths : /import with no arg -> usage; missing file -> error
                     (no confirm); invalid JSON file -> confirm -> No cancels,
                     confirm -> Yes reports the parse failure; a real session
                     JSONL -> confirm -> Yes imports and replays it.
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round40-help-import")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
SHOTS = os.path.join(ROOT, "shots")

GOOD_SRC = os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round39-paste-theme", "runs",
    "a_paste_edit", "home", ".pi", "agent", "sessions",
    "--C--wp-empty-pir-.play-evidence-lane-tui-round39-paste-theme-project--",
    "2026-11-10T05-29-10-095Z_01a1c3ee-80ce-7217-947d-e55ecdcd6a3f.jsonl")


def rows_of(name):
    raw = open(os.path.join(SHOTS, name + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def run_leg(name, script, ops, cwd, checks):
    base = os.path.join(ROOT, "runs", name)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    if checks.get("keybindings"):
        settings_path = os.path.join(agent, "settings.json")
        with open(settings_path, encoding="utf-8") as f:
            settings = json.load(f)
        settings["keybindings"] = checks["keybindings"]
        with open(settings_path, "w", encoding="utf-8") as f:
            json.dump(settings, f, indent=2)
    mocks = os.path.join(base, "mock_llm.log")
    mock = MockServer(log_path=mocks, script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    result_json = os.path.join(SHOTS, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=cwd, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, timeout=120)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    leg = {"exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
           "elapsed_s": out.get("elapsed_s"), "checks": {}, "screen_stats": {}}
    for snap, needles in checks.get("screens", {}).items():
        rows, raw = rows_of(snap)
        stat = {}
        for needle, mode in needles:
            n = sum(r.count(needle) for r in rows)
            stat[needle] = n
            leg["checks"][f"{snap}:{mode}:{needle}"] = {
                "ok": (n >= 1 if mode == "present" else n == 0), "count": n}
        leg["screen_stats"][snap] = stat
        if checks.get("no_mojibake"):
            bad = raw.count(b"\xef\xbf\xbd")
            leg["checks"][f"{snap}:no_mojibake"] = {"ok": bad == 0, "count": bad}
    return leg


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round40\n")
    with open(os.path.join(project, "bad.jsonl"), "w", encoding="utf-8") as f:
        f.write("{this is not a session}\n")
    shutil.copyfile(GOOD_SRC, os.path.join(project, "good-session.jsonl"))
    result = {"pir": PIR, "legs": {}}

    # ---- Leg A: /help + /hotkeys with a live keybinding override ----
    ops_a = [
        ("wait_quiet", 5.0), ("snap", "a_boot"),
        ("log", "/help executes and renders the command notice"),
        ("send", b"/help\r"), ("wait", 1.5), ("snap", "a_help"),
        ("log", "/hotkeys shows the live table incl. settings override"),
        ("send", b"/hotkeys\r"), ("wait", 2.0), ("snap", "a_hotkeys"),
    ]
    leg_a = run_leg("a_help_hotkeys", None, ops_a, project, {
        "keybindings": {"tui.editor.pageUp": "ctrl+u"},
        "screens": {
            "a_boot": [("Commands:", "absent")],
            "a_help": [("Commands: /model /thinking /clear /compact", "present"),
                       ("\u2192 help", "absent")],
            "a_hotkeys": [("Keyboard Shortcuts", "present"),
                          ("Scroll by page", "present"),
                          ("ctrl+u", "present")],
        },
        "no_mojibake": True,
    })
    result["legs"]["a_help_hotkeys"] = leg_a

    # ---- Leg B: /import usage / missing / bad / good paths ----
    ops_b = [
        ("wait_quiet", 5.0),
        ("log", "/import with no argument -> usage"),
        ("send", b"/import\r"), ("wait", 1.2), ("snap", "b_usage"),
        ("log", "/import missing file -> error, no confirm dialog"),
        ("send", b"/import missing-import.jsonl\r"), ("wait", 1.2), ("snap", "b_missing"),
        ("log", "/import bad.jsonl -> confirm dialog, answer No"),
        ("send", b"/import bad.jsonl\r"), ("wait", 1.2), ("snap", "b_confirm_bad"),
        ("send", b"n"), ("wait", 1.0), ("snap", "b_cancel"),
        ("log", "/import bad.jsonl again -> confirm, answer Yes -> parse error"),
        ("send", b"/import bad.jsonl\r"), ("wait", 1.2),
        ("send", b"y"), ("wait", 3.0), ("snap", "b_bad_yes"),
        ("log", "/import good-session.jsonl -> confirm, answer Yes -> imported"),
        ("send", b"/import good-session.jsonl\r"), ("wait", 1.2), ("snap", "b_confirm_good"),
        ("send", b"y"), ("wait", 5.0), ("snap", "b_imported"),
    ]
    leg_b = run_leg("b_import_paths", None, ops_b, project, {
        "screens": {
            "b_usage": [("Usage: /import <path.jsonl>", "present")],
            "b_missing": [("Failed to import session: file not found or not a file: missing-import.jsonl", "present"),
                          ("Replace current session", "absent")],
            "b_confirm_bad": [("Import session", "present"),
                              ("Replace current session with bad.jsonl?", "present")],
            "b_cancel": [("Import cancelled", "present")],
            "b_bad_yes": [("Failed to import session", "present")],
            "b_confirm_good": [("Replace current session with good-session.jsonl?", "present")],
            "b_imported": [("Session imported from: good-session.jsonl", "present"),
                           ("R39-L1", "present")],
        },
        "no_mojibake": True,
    })
    result["legs"]["b_import_paths"] = leg_b

    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)

    total = ok = 0
    for leg in result["legs"].values():
        for check in leg["checks"].values():
            total += 1
            ok += 1 if check["ok"] else 0
    print(f"legs={list(result['legs'])} checks={total} ok={ok} "
          f"exit={[l['exit_code'] for l in result['legs'].values()]}")
    return 0 if ok == total else 1


if __name__ == "__main__":
    sys.exit(main())
