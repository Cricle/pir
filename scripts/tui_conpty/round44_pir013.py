# -*- coding: utf-8 -*-
"""Round 44 lane-tui: PIR-TUI-013 regression.

TUI `--continue` must push the session-context snapshot on attach, before the
first prompt, so native plugins replay persisted state:

  p1_seed     : create two todos + one memory record through one real prompt.
  p2_continue : `--continue`, send NO prompt; `/todos` must show the two
                historical tasks, the panel must already show `2 pending`,
                `/memory status` + `/memory brief` must see the record.
  p3_after    : `--continue`, send one prompt, then `/todos`; state must stay
                exactly `2 pending` (no duplicate replay).

All legs use the real ConPTY harness + mock LLM (model side scripted only).
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round44-pir013")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def rows_of(root, shot):
    raw = open(os.path.join(root, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def rows_with(rows, needle):
    return [(i, r) for i, r in enumerate(rows) if needle in r]


def run_tui(name, root, ops, cwd, script, args=None, agent=None, extra_env=None):
    base = os.path.join(root, "runs", name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    if agent is None:
        agent = os.path.join(home, ".pi", "agent")
        os.makedirs(agent, exist_ok=True)
        seed_agent_dir(agent, True)
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    if extra_env:
        env.update(extra_env)
    tui_drive.PIR = PIR
    shots = os.path.join(root, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=cwd, shots_dir=shots,
            driver_log=os.path.join(shots, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, args=args, timeout=180)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    return {"agent": agent, "exit_code": out.get("exit_code"),
            "timed_out": out.get("timed_out"), "elapsed_s": out.get("elapsed_s"),
            "result_json": result_json}


def check_screen(leg, root, snap, needles):
    rows, raw = rows_of(root, snap)
    for needle, mode in needles:
        n = sum(r.count(needle) for r in rows)
        ok = (n >= 1) if mode == "present" else (n == 0)
        leg["checks"][f"{snap}:{mode}:{needle}"] = {"ok": ok, "count": n}
    bad = raw.count(b"\xef\xbf\xbd")
    leg["checks"][f"{snap}:no_mojibake"] = {"ok": bad == 0, "count": bad}
    return rows


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round44 pir013\n")

    script = [
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R44 todo alpha"}},
            {"name": "todo", "arguments": {"action": "create", "subject": "R44 todo beta"}},
            {"name": "memory", "arguments": {
                "action": "remember", "kind": "decision", "scope": "repo",
                "subject": "R44-MEMORY",
                "text": "Round44 memory body: attach snapshot push verified before first prompt."}},
        ]},
        {"text": "R44-SEED-OK"},
    ]
    p1 = run_tui("p1_seed", ROOT, [
        ("wait_quiet", 5.0), ("snap", "p1_boot"),
        ("log", "seed todo x2 + memory through one prompt"),
        ("send", b"R44 seed todos and memory please\r"), ("wait", 10.0),
        ("wait_quiet", 4.0), ("snap", "p1_turn"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "p1_todos"),
        ("send", b"/memory status\r"), ("wait", 1.5), ("snap", "p1_memstatus"),
        ("send", b"/memory brief R44-MEMORY\r"), ("wait", 1.5), ("snap", "p1_membrief"),
    ], project, script)
    leg1 = {"exit_code": p1["exit_code"], "timed_out": p1["timed_out"],
            "elapsed_s": p1["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(leg1, ROOT, "p1_turn", [("R44-SEED-OK", "present")])
    check_screen(leg1, ROOT, "p1_todos",
                 [("2 pending", "present"), ("R44 todo alpha", "present")])
    check_screen(leg1, ROOT, "p1_membrief", [("R44-MEMORY", "present")])

    # p2: --continue, NO prompt at all.
    p2 = run_tui("p2_continue", ROOT, [
        ("wait_quiet", 7.0), ("snap", "p2_boot"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "p2_todos"),
        ("send", b"/memory status\r"), ("wait", 1.5), ("snap", "p2_memstatus"),
        ("send", b"/memory brief R44-MEMORY\r"), ("wait", 1.5), ("snap", "p2_membrief"),
    ], project, [{"text": "unused-no-prompt"}], args=["--continue"], agent=p1["agent"])
    leg2 = {"exit_code": p2["exit_code"], "timed_out": p2["timed_out"],
            "elapsed_s": p2["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(leg2, ROOT, "p2_boot", [("2 pending", "present")])
    check_screen(leg2, ROOT, "p2_todos",
                 [("2 pending", "present"), ("R44 todo alpha", "present")])
    check_screen(leg2, ROOT, "p2_memstatus", [("active_count", "present")])
    check_screen(leg2, ROOT, "p2_membrief", [("R44-MEMORY", "present")])

    # p3: --continue, one prompt then /todos; no duplicate state.
    p3 = run_tui("p3_after", ROOT, [
        ("wait_quiet", 7.0), ("snap", "p3_boot"),
        ("send", b"hello again\r"), ("wait", 6.0), ("wait_quiet", 3.0),
        ("snap", "p3_reply"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "p3_todos"),
    ], project, [{"text": "R44-P3-OK"}], args=["--continue"], agent=p1["agent"])
    leg3 = {"exit_code": p3["exit_code"], "timed_out": p3["timed_out"],
            "elapsed_s": p3["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(leg3, ROOT, "p3_boot", [("2 pending", "present")])
    check_screen(leg3, ROOT, "p3_reply", [("R44-P3-OK", "present")])
    rows = check_screen(leg3, ROOT, "p3_todos",
                        [("2 pending", "present"), ("R44 todo alpha", "present"),
                         ("4 pending", "absent")])
    leg3["todos_rows"] = [r for _, r in rows_with(rows, "pending")]

    # p4: same resumed transcript copied to a DIFFERENT agent-dir path; the
    # attach push must not depend on the original storage location.
    alt_agent = os.path.join(ROOT, "runs", "p4_alt", "alt-agent")
    shutil.copytree(p1["agent"], alt_agent)
    p4 = run_tui("p4_alt", ROOT, [
        ("wait_quiet", 7.0), ("snap", "p4_boot"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "p4_todos"),
        ("send", b"/memory brief R44-MEMORY\r"), ("wait", 1.5), ("snap", "p4_membrief"),
    ], project, [{"text": "unused-alt"}], args=["--continue"], agent=alt_agent)
    leg4 = {"exit_code": p4["exit_code"], "timed_out": p4["timed_out"],
            "elapsed_s": p4["elapsed_s"], "checks": {}, "rows": {},
            "agent_dir": alt_agent}
    check_screen(leg4, ROOT, "p4_boot", [("2 pending", "present")])
    check_screen(leg4, ROOT, "p4_todos",
                 [("2 pending", "present"), ("R44 todo alpha", "present")])
    check_screen(leg4, ROOT, "p4_membrief", [("R44-MEMORY", "present")])

    result = {"legs": {"p1_seed": leg1, "p2_continue": leg2,
                       "p3_after": leg3, "p4_alt": leg4},
              "binary": PIR}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(os.path.join(ROOT, "shots"))

    def summary(leg):
        return {"exit": leg["exit_code"], "timed_out": leg["timed_out"],
                "checks_ok": sum(1 for c in leg["checks"].values() if c["ok"]),
                "checks_total": len(leg["checks"]),
                "failed": [k for k, c in leg["checks"].items() if not c["ok"]]}
    print(json.dumps({k: summary(v) for k, v in result["legs"].items()},
                     indent=2, ensure_ascii=False))
    return 1 if any(not c["ok"] for leg in result["legs"].values()
                    for c in leg["checks"].values()) else 0


if __name__ == "__main__":
    raise SystemExit(main())
