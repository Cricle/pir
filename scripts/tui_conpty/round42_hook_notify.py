# -*- coding: utf-8 -*-
"""Round 42 lane-tui: TUI host notification / hook completeness over pir36.exe.

One real TUI leg driven by a probe extension (`.play/evidence/lane-tui/
round42-hook-notify/ext/r42probe.ts`, copied into the isolated agent dir):
  * ctx.ui.notify info/warning/error -> transcript notices
  * ctx.ui.setStatus / setWidget (update + clear) / setFooter / setHeader
  * ctx.ui.confirm / select / input -> modal request/response round trips,
    both accepted (y / Down+Enter / typed text) and cancelled (Esc each)
  * todo panel create -> update -> clear
  * background bash call: bg: 1 start, in-place end flip; foreground bash
    abort with Esc
The extension also appends every dispatched hook event to events.jsonl so
ordering/payload can be checked beyond the screen.
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round42-hook-notify")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
SHOTS = os.path.join(ROOT, "shots")
EXT_SRC = os.path.join(ROOT, "ext", "r42probe.ts")


def rows_of(name):
    raw = open(os.path.join(SHOTS, name + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def run_leg(name, script, ops, cwd, screens, no_mojibake=True):
    base = os.path.join(ROOT, "runs", name)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    ext_dir = os.path.join(agent, "extensions")
    os.makedirs(ext_dir, exist_ok=True)
    shutil.copyfile(EXT_SRC, os.path.join(ext_dir, "r42probe.ts"))
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    result_json = os.path.join(SHOTS, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=cwd, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, timeout=180)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    leg = {"exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
           "elapsed_s": out.get("elapsed_s"), "checks": {}, "screen_stats": {}}
    for snap, needles in screens.items():
        rows, raw = rows_of(snap)
        stat = {}
        for needle, mode in needles:
            n = sum(r.count(needle) for r in rows)
            stat[needle] = n
            if mode == "present":
                ok = n >= 1
            elif mode == "absent":
                ok = n == 0
            elif mode == "once":
                ok = n == 1
            elif mode == "count1":
                ok = n == 1
            else:
                raise ValueError(mode)
            leg["checks"][f"{snap}:{mode}:{needle}"] = {"ok": ok, "count": n}
        leg["screen_stats"][snap] = stat
        if no_mojibake:
            bad = raw.count(b"\xef\xbf\xbd")
            leg["checks"][f"{snap}:no_mojibake"] = {"ok": bad == 0, "count": bad}
    return leg


def main():
    probe = os.path.join(ROOT, "ext", "r42probe.ts")
    if not os.path.isfile(probe):
        raise SystemExit(f"missing probe extension source: {probe}")
    with open(probe, encoding="utf-8") as f:
        probe_src = f.read()
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "ext"), exist_ok=True)
    with open(probe, "w", encoding="utf-8") as f:
        f.write(probe_src)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round42 hook notify\n")

    script = [
        {"text": "R42-BOOT-REPLY"},
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R42 todo alpha"}},
            {"name": "todo", "arguments": {"action": "create", "subject": "R42 todo beta"}},
        ]},
        {"text": "R42-TODO-CREATED"},
        {"tool_call": {"name": "todo", "arguments": {"action": "update", "id": 1, "status": "completed"}}},
        {"text": "R42-TODO-UPDATED"},
        {"tool_call": {"name": "todo", "arguments": {"action": "clear"}}},
        {"text": "R42-TODO-CLEARED"},
        {"tool_call": {"name": "bash", "arguments": {"command": "sleep 6", "background": True}}},
        {"text": "R42-BG-SCHEDULED"},
        {"tool_call": {"name": "bash", "arguments": {"command": "sleep 25; echo R42-ABORT-BASH-TAIL"}}},
        {"text": "R42-ABORT-TURN-DONE"},
        {"text": "R42-POST"},
    ]

    ops = [
        ("wait_quiet", 5.0), ("snap", "t42_boot"),
        ("log", "prompt 1: trigger session_start (bars + notify)"),
        ("send", b"hello\r"), ("wait_quiet", 4.0), ("snap", "t42_started"),
        ("log", "notify info/warning/error"),
        ("send", b"/uinotify\r"), ("wait", 1.5), ("snap", "t42_notify"),
        ("log", "status/widget/footer/header set; widget key replaced"),
        ("send", b"/uibars\r"), ("wait", 1.2), ("snap", "t42_bars"),
        ("log", "confirm dialog -> y"),
        ("send", b"/uiprobe\r"), ("wait", 1.5), ("snap", "t42_confirm"),
        ("send", b"y"), ("wait", 1.2), ("snap", "t42_confirm_yes"),
        ("log", "select dialog -> Down + Enter"),
        ("send", b"\x1b[B"), ("wait", 0.5), ("send", b"\r"),
        ("wait", 1.2), ("snap", "t42_select_two"),
        ("log", "input dialog -> typed text + Enter"),
        ("send", b"R42-TYPED-TEXT"), ("wait", 0.5), ("send", b"\r"),
        ("wait", 1.5), ("snap", "t42_input_text"),
        ("log", "cancel path: confirm/select/input each Esc"),
        ("send", b"/uiprobe\r"), ("wait", 1.2), ("snap", "t42_confirm2"),
        ("send", b"\x1b"), ("wait", 1.2), ("snap", "t42_cancel_confirm"),
        ("send", b"\x1b"), ("wait", 1.2), ("snap", "t42_cancel_select"),
        ("send", b"\x1b"), ("wait", 1.5), ("snap", "t42_cancel_input"),
        ("log", "clear status/widget/footer/header"),
        ("send", b"/uiclear\r"), ("wait", 1.5), ("snap", "t42_cleared"),
        ("log", "todo panel create -> update -> clear"),
        ("send", b"create todos\r"), ("wait_quiet", 4.0), ("snap", "t42_todo_create"),
        ("send", b"update todo\r"), ("wait_quiet", 4.0), ("snap", "t42_todo_update"),
        ("send", b"clear todos\r"), ("wait_quiet", 4.0), ("snap", "t42_todo_clear"),
        ("log", "background bash: start / in-place end"),
        ("send", b"bg\r"), ("wait", 2.5), ("snap", "t42_bg_running"),
        ("wait", 7.0), ("wait_quiet", 2.0), ("snap", "t42_bg_done"),
        ("log", "foreground bash abort with Esc"),
        ("send", b"abort\r"), ("wait", 2.5), ("snap", "t42_abort_running"),
        ("send", b"\x1b"), ("wait", 3.0), ("snap", "t42_aborted"),
        ("log", "UI responsive after abort"),
        ("send", b"/help\r"), ("wait", 2.0), ("snap", "t42_after_abort"),
    ]

    screens = {
        "t42_started": [
            ("R42-SESSION-START", "present"),
            ("R42-STATUS-READY", "present"),
        ],
        "t42_notify": [
            ("R42-WIDGET-LINE1", "once"),
            ("R42-WIDGET-LINE2", "once"),
            ("R42-NOTIFY-INFO", "once"),
            ("R42-NOTIFY-WARN", "once"),
            ("R42-NOTIFY-ERROR", "once"),
        ],
        "t42_bars": [
            ("R42-WIDGET-V2-A", "once"),
            ("R42-WIDGET-V2-B", "once"),
            ("R42-WIDGET-LINE1", "absent"),
            ("R42-BARS-SET", "once"),
            ("R42-FOOTER", "once"),
            ("R42-HEADER", "once"),
        ],
        "t42_confirm": [
            ("R42-CONFIRM-TITLE", "present"),
            ("R42-CONFIRM-MESSAGE", "present"),
        ],
        "t42_confirm_yes": [
            ("R42-CONFIRM-RESULT=true", "once"),
            ("R42-SELECT-TITLE", "present"),
            ("R42-OPT-ONE", "present"),
            ("R42-OPT-TWO", "present"),
        ],
        "t42_select_two": [
            ("R42-SELECT-RESULT=R42-OPT-TWO", "once"),
            ("R42-INPUT-TITLE", "present"),
            ("R42-INPUT-PLACEHOLDER", "absent"),
        ],
        "t42_input_text": [
            ("R42-INPUT-RESULT=R42-TYPED-TEXT", "once"),
        ],
        "t42_confirm2": [("R42-CONFIRM-TITLE", "present"), ("R42-CONFIRM-MESSAGE", "present")],
        "t42_cancel_confirm": [
            ("R42-CONFIRM-RESULT=undefined", "once"),
            ("R42-SELECT-TITLE", "present"),
        ],
        "t42_cancel_select": [
            ("R42-SELECT-RESULT=undefined", "once"),
            ("R42-INPUT-TITLE", "present"),
        ],
        "t42_cancel_input": [
            ("R42-INPUT-RESULT=undefined", "once"),
        ],
        "t42_cleared": [
            ("R42-CLEARED", "once"),
            ("R42-STATUS-READY", "absent"),
            ("R42-WIDGET-V2-A", "absent"),
            ("R42-FOOTER", "absent"),
            ("R42-HEADER", "absent"),
        ],
        "t42_todo_create": [
            ("R42 todo alpha", "present"),
            ("R42 todo beta", "present"),
        ],
        "t42_todo_update": [
            ("R42 todo alpha", "present"),
            ("1/2 completed", "present"),
        ],
        "t42_todo_clear": [
            ("○ #1 R42 todo alpha", "absent"),
            ("✓ #1 R42 todo alpha", "absent"),
            ("2 pending", "absent"),
            ("1/2 completed", "absent"),
            ("R42-TODO-CLEARED", "present"),
        ],
        "t42_bg_running": [
            ("bg: 1", "present"),
            ("$ sleep 6", "present"),
            ("R42-BG-SCHEDULED", "present"),
        ],
        "t42_bg_done": [
            ("bg: 1", "absent"),
            ("$ sleep 6", "present"),
            ("(no output)", "present"),
        ],
        "t42_abort_running": [
            ("$ sleep 25; echo R42-ABORT-BASH-TAIL", "present"),
            ("Running bash…", "present"),
        ],
        "t42_aborted": [
            ("Command aborted", "once"),
            ("Interrupted", "present"),
            ("Running bash…", "absent"),
        ],
        "t42_after_abort": [
            ("Commands: /model /thinking /clear /compact", "present"),
        ],
    }

    result = {"pir": PIR, "extension": EXT_SRC, "legs": {}}
    leg = run_leg("hook_notify", script, ops, project, screens)
    result["legs"]["hook_notify"] = leg

    # event log is written by the extension into ROOT/events.jsonl
    ev_path = os.path.join(ROOT, "events.jsonl")
    events = []
    if os.path.isfile(ev_path):
        with open(ev_path, encoding="utf-8") as f:
            events = [json.loads(line) for line in f if line.strip()]
    result["event_count"] = len(events)
    result["event_order"] = [e["event"] for e in events]
    os.makedirs(ROOT, exist_ok=True)
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    ok = sum(1 for c in leg["checks"].values() if c["ok"])
    total = len(leg["checks"])
    print(json.dumps({"exit_code": leg["exit_code"], "timed_out": leg["timed_out"],
                      "elapsed_s": leg["elapsed_s"], "checks_ok": ok, "checks_total": total,
                      "event_count": len(events)}, ensure_ascii=False))
    for key, chk in leg["checks"].items():
        if not chk["ok"]:
            print("FAIL", key, chk["count"])
    return 0 if ok == total and leg["exit_code"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
