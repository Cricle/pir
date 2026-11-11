# -*- coding: utf-8 -*-
"""Round 49 lane-tui: mid-session `/sessions` / `/resume` switch regression.

Session A is seeded (real prompt, mock model) with two todos, one memory
record and one subagent result. Session B is seeded with one different todo.
Then switch back to A through the `/sessions` filter and (second leg) through
the full-screen `/resume` picker WITHOUT sending a prompt: the todo panel,
`/todos`, `/memory status|brief` and the replayed subagent block must be
present immediately, with no duplicated tasks. Session JSONL is parsed after
the run for duplicate/lost messages.
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round49-resume-switch")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def rows_of(root, shot):
    raw = open(os.path.join(root, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def rows_with(rows, needle):
    return [(i, r) for i, r in enumerate(rows) if needle in r]


def run_tui(name, root, ops, cwd, script, args=None, agent=None):
    base = os.path.join(root, "runs", name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    if agent is None:
        agent = os.path.join(home, ".pi", "agent")
        os.makedirs(agent, exist_ok=True)
        seed_agent_dir(agent, True)
        helper_dir = os.path.join(agent, "agents")
        os.makedirs(helper_dir, exist_ok=True)
        with open(os.path.join(helper_dir, "helper.md"), "w", encoding="utf-8") as f:
            f.write("---\nname: helper\ndescription: Round49 helper agent\n---\n"
                    "You are a careful helper. Do the task and report briefly.\n")
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    shots = os.path.join(root, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=cwd, shots_dir=shots,
            driver_log=os.path.join(shots, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, args=args, timeout=240)
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


def session_files(agent):
    found = []
    for dirpath, _dirs, names in os.walk(os.path.join(agent, "sessions")):
        for entry in names:
            if entry.endswith(".jsonl"):
                found.append(os.path.join(dirpath, entry))
    return found


def audit_jsonl(agent, tag):
    """Return per-session structural facts for A/B; no duplicate user turns."""
    out = {"sessions": [], "checks": {}}
    for path in session_files(agent):
        entries = []
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    entries.append({"_unparsed": line[:200]})
        blob = json.dumps(entries, ensure_ascii=False)
        if "R49-A-SEED" not in blob and "R49-B-SEED" not in blob:
            continue
        which = "A" if "R49-A-SEED" in blob else "B"
        messages = []
        for e in entries:
            m = e.get("message") if isinstance(e, dict) else None
            if isinstance(m, dict) and m.get("role") == "user":
                content = m.get("content")
                text = content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)
                messages.append(text)
        ids = [e.get("id") for e in entries if isinstance(e, dict) and e.get("id")]
        out["sessions"].append({
            "which": which, "path": path, "entries": len(entries),
            "user_messages": messages,
            "unique_ids": len(set(ids)) == len(ids),
        })
        if which == "A":
            out["checks"]["A:seed-user-once"] = {
                "ok": sum(1 for m in messages if "R49 alpha seed please" in m) == 1}
            out["checks"]["A:return-user-once"] = {
                "ok": sum(1 for m in messages if "hello after switch" in m) == 1}
            out["checks"]["A:has-subagent-child"] = {
                "ok": "SUBAGENT-R49-CHILD" in blob}
            out["checks"]["A:ids-unique"] = {
                "ok": out["sessions"][-1]["unique_ids"]}
        if which == "B":
            out["checks"]["B:seed-user-once"] = {
                "ok": sum(1 for m in messages if "R49 beta seed please" in m) == 1}
            out["checks"]["B:no-return-prompt"] = {
                "ok": "hello after switch" not in blob}
            out["checks"]["B:ids-unique"] = {
                "ok": out["sessions"][-1]["unique_ids"]}
    return out


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round49 resume switch\n")

    script = [
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R49-A todo one"}},
            {"name": "todo", "arguments": {"action": "create", "subject": "R49-A todo two"}},
            {"name": "memory", "arguments": {
                "action": "remember", "kind": "decision", "scope": "repo",
                "subject": "R49-A-MEMORY",
                "text": "Round49 A memory body: mid-session switch restored."}},
        ]},
        {"tool_call": {"name": "subagent", "arguments": {
            "agent": "helper",
            "task": "Create a file subagent_r49.txt containing the single line: helper r49"}}},
        {"tool_call": {"name": "bash", "arguments": {
            "command": "printf 'helper r49\\n' > subagent_r49.txt"}}},
        {"text": "SUBAGENT-R49-CHILD"},
        {"text": "R49-A-SEED-OK"},
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R49-B todo one"}},
        ]},
        {"text": "R49-B-SEED-OK"},
        {"text": "R49-A-RETURN-OK"},
    ]

    # ---------------- Leg 1: /sessions filter switch A -> B -> A ------------
    p1 = run_tui("sessions_switch", ROOT, [
        ("wait_quiet", 5.0), ("snap", "s1_boot"),
        ("log", "seed session A: todo x2 + memory + subagent"),
        ("send", b"R49 alpha seed please\r"), ("wait", 10.0), ("wait_quiet", 4.0),
        ("snap", "s1_seed"),
        ("send", b"/name alpha\r"), ("wait", 1.0),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "s1_a_todos"),
        ("send", b"/memory brief R49-A-MEMORY\r"), ("wait", 1.5), ("snap", "s1_a_membrief"),
        ("log", "start session B and seed one different todo"),
        ("send", b"/new\r"), ("wait", 2.5),
        ("send", b"R49 beta seed please\r"), ("wait", 10.0), ("wait_quiet", 4.0),
        ("snap", "s1_beta"),
        ("send", b"/name beta\r"), ("wait", 1.0),
        ("send", b"/sessions\r"), ("wait", 1.5), ("snap", "s1_sessions_open"),
        ("send", b"alpha"), ("wait", 1.2), ("snap", "s1_sessions_filtered"),
        ("send", b"\r"), ("wait", 4.0), ("wait_quiet", 2.0), ("snap", "s1_back_a"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "s1_back_a_todos"),
        ("send", b"/memory status\r"), ("wait", 1.5), ("snap", "s1_back_a_memstatus"),
        ("send", b"/memory brief R49-A-MEMORY\r"), ("wait", 1.5), ("snap", "s1_back_a_membrief"),
        ("log", "send one prompt after switch; todos must not duplicate"),
        ("send", b"hello after switch\r"), ("wait", 8.0), ("wait_quiet", 3.0),
        ("snap", "s1_a_return"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "s1_a_todos_after"),
        ("log", "switch to B through /sessions filter"),
        ("send", b"/sessions\r"), ("wait", 1.5),
        ("send", b"beta"), ("wait", 1.2), ("snap", "s1_sessions_beta"),
        ("send", b"\r"), ("wait", 4.0), ("wait_quiet", 2.0), ("snap", "s1_back_b"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "s1_back_b_todos"),
    ], project, script)
    leg1 = {"exit_code": p1["exit_code"], "timed_out": p1["timed_out"],
            "elapsed_s": p1["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(leg1, ROOT, "s1_seed",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("R49-A-SEED-OK", "present"), ("SUBAGENT-R49-CHILD", "present"),
                  ("subagent helper", "present")])
    check_screen(leg1, ROOT, "s1_a_todos",
                 [("2 pending", "present"), ("R49-A todo one", "present")])
    check_screen(leg1, ROOT, "s1_a_membrief", [("R49-A-MEMORY", "present")])
    check_screen(leg1, ROOT, "s1_beta",
                 [("1 pending", "present"), ("R49-B todo one", "present"),
                  ("R49-B-SEED-OK", "present")])
    check_screen(leg1, ROOT, "s1_sessions_filtered", [("alpha", "present")])
    check_screen(leg1, ROOT, "s1_back_a",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("R49-A-SEED-OK", "present"), ("SUBAGENT-R49-CHILD", "present")])
    check_screen(leg1, ROOT, "s1_back_a_todos",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("4 pending", "absent")])
    check_screen(leg1, ROOT, "s1_back_a_memstatus", [("doc_count", "present")])
    check_screen(leg1, ROOT, "s1_back_a_membrief", [("R49-A-MEMORY", "present")])
    check_screen(leg1, ROOT, "s1_a_return", [("R49-A-RETURN-OK", "present")])
    check_screen(leg1, ROOT, "s1_a_todos_after",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("4 pending", "absent")])
    check_screen(leg1, ROOT, "s1_back_b_todos",
                 [("1 pending", "present"), ("R49-B todo one", "present"),
                  ("R49-A todo one", "absent")])
    leg1["jsonl"] = audit_jsonl(p1["agent"], "sessions")
    leg1["checks"].update({"jsonl:" + k: v for k, v in leg1["jsonl"]["checks"].items()})

    # ---------------- Leg 2: mid-session /resume picker ----------------------
    p2 = run_tui("resume_switch", ROOT, [
        ("wait_quiet", 5.0), ("snap", "s2_boot"),
        ("log", "fresh session C; /resume -> filter alpha -> Enter, no prompt"),
        ("send", b"/resume\r"), ("wait", 2.0), ("snap", "s2_resume_picker"),
        ("send", b"alpha"), ("wait", 1.2), ("snap", "s2_resume_filtered"),
        ("send", b"\r"), ("wait", 5.0), ("wait_quiet", 2.0), ("snap", "s2_back_a"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "s2_a_todos"),
        ("send", b"/memory status\r"), ("wait", 1.5), ("snap", "s2_a_memstatus"),
        ("send", b"/memory brief R49-A-MEMORY\r"), ("wait", 1.5), ("snap", "s2_a_membrief"),
        ("send", b"resume path hello\r"), ("wait", 8.0), ("wait_quiet", 3.0),
        ("snap", "s2_a_return"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "s2_a_todos_after"),
    ], project, [{"text": "R49-RESUME-OK"}], agent=p1["agent"])
    leg2 = {"exit_code": p2["exit_code"], "timed_out": p2["timed_out"],
            "elapsed_s": p2["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(leg2, ROOT, "s2_resume_picker", [("Resume a session", "present")])
    check_screen(leg2, ROOT, "s2_back_a",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("SUBAGENT-R49-CHILD", "present")])
    check_screen(leg2, ROOT, "s2_a_todos",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("4 pending", "absent")])
    check_screen(leg2, ROOT, "s2_a_memstatus", [("doc_count", "present")])
    check_screen(leg2, ROOT, "s2_a_membrief", [("R49-A-MEMORY", "present")])
    check_screen(leg2, ROOT, "s2_a_return", [("R49-RESUME-OK", "present")])
    check_screen(leg2, ROOT, "s2_a_todos_after",
                 [("2 pending", "present"), ("R49-A todo one", "present"),
                  ("4 pending", "absent")])
    leg2["jsonl"] = audit_jsonl(p1["agent"], "resume")

    result = {"binary": PIR, "legs": {"sessions_switch": leg1, "resume_switch": leg2}}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(os.path.join(ROOT, "shots"))

    def summary(leg):
        return {"exit": leg["exit_code"], "timed_out": leg["timed_out"],
                "checks_ok": sum(1 for c in leg["checks"].values() if c["ok"]),
                "checks_total": len(leg["checks"]),
                "failed": [k for k, c in leg["checks"].items() if not c["ok"]]}

    print(json.dumps({"sessions_switch": summary(leg1), "resume_switch": summary(leg2)},
                     indent=2, ensure_ascii=False))
    if any(not c["ok"] for leg in (leg1, leg2) for c in leg["checks"].values()):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
