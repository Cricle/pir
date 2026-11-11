# -*- coding: utf-8 -*-
"""Round 65 lane-tui task 2+1: PIR-TUI-022 reproduction with the FIXED probe
(r65probe.ts adds the `session_before_switch` subscription R64's probe was
missing) and the PIR-TUI-021 final re-run on the SAME real ConPTY session.

One isolated agent dir, three legs through the REAL user paths:
  leg s1: multi-turn + /compact + /fork (in-place) + post-fork prompt + /quit
  leg s2: `--continue` resume (session_start reason=startup parity) + /quit
  leg s3: `/sessions` picker switch through replace_session_storage
          (session_before_switch -> shutdown{switch} -> start{resume})

The probe records every dispatched hook via pi.appendEntry (session JSONL,
HOST order = the authoritative channel) and a powershell arrival timeline
(r65-events.jsonl, informational). Audits:
  * s3 must now carry FOUR hook entries: before_switch -> shutdown(switch) ->
    queue_update -> start(resume) — PIR-TUI-022 decision data.
  * s1 fork order before_fork -> shutdown(fork) -> start(fork) — PIR-TUI-021.
  * per-segment seq monotonic + no dup + contiguous (zero loss/dup).
Everything lands in result.json; screens under shots/.
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round65-022-023")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE")
                      or os.path.join(REPO, ".play", "evidence", "lane-tui",
                                      "round64-bin", "pir64.exe"))
SHOTS = os.path.join(ROOT, "shots")
EXT_SRC = os.path.join(HERE, "ext", "r65probe.ts")
PYEXE = "C:/wp/empty/tools/python312/python.exe"


def rows_of(name):
    raw = open(os.path.join(SHOTS, name + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def read_events(agent):
    path = os.path.join(agent, "r65-events.jsonl")
    if not os.path.isfile(path):
        return []
    out = []
    with open(path, encoding="utf-8-sig", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except ValueError:
                out.append({"event": "MALFORMED", "raw": line[:120]})
    return out


def read_session_jsonl(session_path):
    entries = []
    with open(session_path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                entries.append(json.loads(line))
            except ValueError:
                entries.append({"type": "MALFORMED"})
    return entries


def all_session_files(agent):
    import glob
    return sorted(glob.glob(os.path.join(agent, "sessions", "*", "*.jsonl")))


def newest_session(agent):
    import time
    base = os.path.join(agent, "sessions")
    for _ in range(10):
        cands = []
        for dirpath, _dirnames, filenames in os.walk(base):
            for fn in filenames:
                if fn.endswith(".jsonl"):
                    p = os.path.join(dirpath, fn)
                    cands.append((os.path.getmtime(p), p))
        if cands:
            return max(cands)[1]
        time.sleep(0.5)
    return None


def run_leg(name, script, ops, cwd, agent, screens, timeout=240, args=None):
    mark = os.path.join(ROOT, "runs", name, "events_pre.jsonl")
    ev_path = os.path.join(agent, "r65-events.jsonl")
    os.makedirs(os.path.dirname(mark), exist_ok=True)
    if os.path.isfile(ev_path):
        shutil.copyfile(ev_path, mark)
    n_pre = sum(1 for _ in open(mark, encoding="utf-8", errors="replace")) \
        if os.path.isfile(mark) else 0
    mock = MockServer(log_path=os.path.join(ROOT, "runs", name, "mock_llm.log"),
                      script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    home = os.path.dirname(os.path.dirname(agent))
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent, "R65_EVENTS": ev_path}
    tui_drive.PIR = PIR
    result_json = os.path.join(SHOTS, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, args=args, cwd=cwd, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, timeout=timeout)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    leg = {"exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
           "elapsed_s": out.get("elapsed_s"), "checks": {},
           "events_offset": n_pre}
    all_events = read_events(agent)
    leg_events = all_events[n_pre:]
    with open(os.path.join(SHOTS, name + ".events.jsonl"), "w",
              encoding="utf-8") as f:
        for e in leg_events:
            f.write(json.dumps(e, ensure_ascii=False) + "\n")
    leg["events"] = leg_events
    for snap, needles in screens.items():
        rows, raw = rows_of(snap)
        for needle, mode in needles:
            n = sum(r.count(needle) for r in rows)
            if mode == "present":
                ok = n >= 1
            elif mode == "absent":
                ok = n == 0
            elif mode == "once":
                ok = n == 1
            else:
                raise ValueError(mode)
            leg["checks"][f"{snap}:{mode}:{needle[:48]}"] = {"ok": ok, "count": n}
        bad = raw.count(b"\xef\xbf\xbd")
        leg["checks"][f"{snap}:no_mojibake"] = {"ok": bad == 0, "count": bad}
    return leg


def check(name, ok, detail):
    return {"check": name, "ok": bool(ok), "detail": detail}


def split_runs(seqs):
    """Split a seq list into strictly-increasing runs (each pir process
    restarts the probe counter at 1)."""
    runs, cur, prev = [], [], None
    for s in seqs:
        if prev is not None and s <= prev:
            runs.append(cur)
            cur = []
        cur.append(s)
        prev = s
    if cur:
        runs.append(cur)
    return runs


def audit_all_sessions(agent):
    """Authoritative post-run audit over EVERY session JSONL in the agent dir.

    Expected segments: seg1 = s1 process (prompts + compact + fork lifecycle
    + post-fork prompt), seg2 = s2 --continue process, seg3 = s3 /sessions
    switch process (NOW FOUR hooks if PIR-TUI-022 is confirmed fixed by the
    probe subscription alone: before_switch -> shutdown(switch) ->
    queue_update -> start(resume)).
    """
    checks = {}
    files = all_session_files(agent)
    hooks_by_file = {}
    for fp in files:
        entries = read_session_jsonl(fp)
        hooks = [e for e in entries if e.get("type") == "custom"
                 and e.get("customType") == "r65hook"]
        if hooks:
            hooks_by_file[fp] = hooks
    checks["sessions:files_with_hooks"] = check(
        "session files carry r65hook entries", bool(hooks_by_file),
        f"{len(hooks_by_file)} file(s): "
        + "; ".join(os.path.basename(fp)[-40:] + f" n={len(h)}"
                    for fp, h in hooks_by_file.items()))
    all_hooks = [h for fp in files for h in hooks_by_file.get(fp, [])]
    seqs = [(h.get("data") or {}).get("seq") for h in all_hooks]
    ints = [s for s in seqs if isinstance(s, int)]
    runs = split_runs(ints)
    checks["segments:count"] = check(
        "three process segments (s1 / s2 / s3)", len(runs) == 3,
        f"{len(runs)} segment(s), lengths={[len(r) for r in runs]}")
    dup_total = sum(len(r) - len(set(r)) for r in runs)
    checks["segments:no_dup_seq"] = check(
        "no duplicate seq within any segment", dup_total == 0,
        f"dups={dup_total}")
    # contiguity: each segment starts at 1 and counts up with no gaps
    gaps = []
    for idx, r in enumerate(runs):
        if r and (r[0] != 1 or r != list(range(r[0], r[0] + len(r)))):
            gaps.append((idx, r[0], r[-1], len(r)))
    checks["segments:contiguous_no_gap"] = check(
        "each segment counts 1..n with no missing seq", not gaps,
        f"gaps={gaps}")

    def seg_event_names(idx):
        lo = sum(len(r) for r in runs[:idx])
        return [((all_hooks[lo + i].get("data") or {}).get("event"))
                for i in range(len(runs[idx]))]

    def seg_reasons(idx, event):
        lo = sum(len(r) for r in runs[:idx])
        return [((all_hooks[lo + i].get("data") or {}).get("brief") or {}).get("reason")
                for i in range(len(runs[idx]))
                if (all_hooks[lo + i].get("data") or {}).get("event") == event]

    if len(runs) >= 2:
        names2 = seg_event_names(1)
        checks["s2:startup_reason"] = check(
            "s2 --continue: one session_start, reason=startup (upstream parity)",
            seg_reasons(1, "session_start") == ["startup"]
            and names2.count("session_start") == 1,
            f"reasons={seg_reasons(1, 'session_start')}")
        checks["s2:prompt_lifecycle"] = check(
            "s2 prompt: before_agent_start -> ... -> agent_settled, no loss",
            "before_agent_start" in names2 and "agent_settled" in names2
            and names2.index("before_agent_start")
            < names2.index("agent_settled"),
            f"events={names2}")
        checks["s2:tool_pair_balanced"] = check(
            "s2 tool_call count == tool_result count",
            names2.count("tool_call") == names2.count("tool_result"),
            f"calls={names2.count('tool_call')} results={names2.count('tool_result')}")
    if len(runs) >= 3:
        names3 = seg_event_names(2)
        # PIR-TUI-022: the pre-swap session_before_switch appendEntry must be
        # FIRST in the s3 segment, before shutdown{switch}.
        bs = names3.index("session_before_switch") \
            if "session_before_switch" in names3 else -1
        sh = names3.index("session_shutdown") if "session_shutdown" in names3 else -1
        st = names3.index("session_start") if "session_start" in names3 else -1
        checks["s3:before_switch_first"] = check(
            "s3 switch: session_before_switch is the FIRST hook (pre-swap)",
            bs == 0,
            f"index={bs} events={names3}")
        checks["s3:switch_lifecycle_order"] = check(
            "s3 switch: before_switch -> shutdown(switch) -> start(resume)",
            0 <= bs < sh < st,
            f"before_switch@{bs} shutdown@{sh} start@{st} events={names3}")
        reasons3 = seg_reasons(2, "session_shutdown")
        checks["s3:shutdown_reason_switch"] = check(
            "s3 shutdown reason=switch", reasons3 == ["switch"],
            f"reasons={reasons3}")
        checks["s3:start_reason_resume"] = check(
            "s3 session_start reason=resume",
            seg_reasons(2, "session_start") == ["resume"],
            f"reasons={seg_reasons(2, 'session_start')}")
    return checks


def audit_timeline(events):
    checks = []
    malformed = [e for e in events if isinstance(e, dict) and e.get("event") == "MALFORMED"]
    checks.append(check("timeline:no_malformed_lines", not malformed,
                        f"{len(malformed)} malformed of {len(events)}"))
    seen_events = {e.get("event") for e in events if isinstance(e, dict)}
    checks.append(check("timeline:events_seen", len(seen_events) >= 15,
                        f"{len(seen_events)} distinct events on the timeline"))
    return checks


def audit_session_file(session_path):
    """AUTHORITATIVE audit over the boot session JSONL (s1's whole run
    INCLUDING the in-place fork)."""
    checks = []
    entries = read_session_jsonl(session_path)
    hooks = [e for e in entries if e.get("type") == "custom"
             and e.get("customType") == "r65hook"]
    checks.append(check("jsonl:r65hook_entries_persisted", len(hooks) >= 40,
                        f"{len(hooks)} r65hook entries"))
    seqs = [(e.get("data") or {}).get("seq") for e in hooks]
    ints = [s for s in seqs if isinstance(s, int)]
    runs = split_runs(ints)
    mono = all(r == sorted(r) for r in runs)
    checks.append(check("jsonl:r65hook_seq_monotonic_per_run", mono,
                        f"{len(runs)} run segment(s), lengths={[len(r) for r in runs]}, "
                        f"first-run range={runs[0][0] if runs else None}.."
                        f"{runs[0][-1] if runs else None}"))
    dup_total = sum(len(r) - len(set(r)) for r in runs)
    checks.append(check("jsonl:r65hook_no_dup_seq", dup_total == 0,
                        f"dups={dup_total} across {len(runs)} run segment(s)"))
    ev_names = [(e.get("data") or {}).get("event") for e in hooks]

    def sub(seq_names, names, after=0):
        i = after
        for want in seq_names:
            while i < len(names) and names[i] != want:
                i += 1
            if i >= len(names):
                return -1
            i += 1
        return i

    seq_probe = ["before_agent_start", "agent_start", "turn_start",
                 "message_start", "tool_call", "tool_result",
                 "tool_execution_start", "tool_execution_end",
                 "turn_end", "agent_before_settle", "agent_settled"]
    checks.append(check("order:prompt1_core_sequence",
                        sub(seq_probe, ev_names) >= 0,
                        "before_agent_start -> agent_start -> turn_start -> "
                        "message_start -> tool_call/result -> execution start/end "
                        "-> turn_end -> settle (order-tolerant scan)"))
    calls = ev_names.count("tool_call")
    results = ev_names.count("tool_result")
    checks.append(check("completeness:tool_call_count==tool_result_count",
                        calls == results and calls >= 3,
                        f"tool_call={calls} tool_result={results}"))
    first_call = ev_names.index("tool_call") if "tool_call" in ev_names else -1
    checks.append(check("order:tool_call_before_tool_result",
                        0 <= first_call < ev_names.index("tool_result")
                        if "tool_result" in ev_names else False,
                        "tool_call precedes tool_result"))
    ci = ev_names.index("compaction_start") if "compaction_start" in ev_names else -1
    ce = ev_names.index("compaction_end") if "compaction_end" in ev_names else -1
    checks.append(check("order:compaction_start_before_end", 0 <= ci < ce,
                        f"compaction_start@{ci} compaction_end@{ce}"))
    # fork lifecycle (PIR-TUI-021): before_fork -> shutdown(fork) ->
    # session_start(fork), in this order, in the SAME file (in-place branch)
    bi = len(ev_names) - 1 - ev_names[::-1].index("session_before_fork") \
        if "session_before_fork" in ev_names else -1
    sh = len(ev_names) - 1 - ev_names[::-1].index("session_shutdown") \
        if "session_shutdown" in ev_names else -1
    st = len(ev_names) - 1 - ev_names[::-1].index("session_start") \
        if "session_start" in ev_names else -1
    checks.append(check("order:fork_before_fork->shutdown->start",
                        0 <= bi < sh < st,
                        f"before_fork@{bi} shutdown@{sh} start@{st} (all in the "
                        "boot file: in-place branch)"))
    reasons = [(e.get("data") or {}).get("brief", {}).get("reason")
               for e in hooks if (e.get("data") or {}).get("event") == "session_start"]
    checks.append(check("payload:session_start_reasons", "fork" in reasons,
                        f"reasons={reasons}"))
    key_expect = {
        "tool_call": ["toolCallId", "toolName"],
        "tool_result": ["toolCallId", "toolName"],
        "session_start": ["reason"],
        "session_before_fork": ["entryId"],
        "before_agent_start": ["prompt"],
        "session_shutdown": ["reason", "targetSessionFile"],
        "session_before_switch": ["targetSessionFile"],
    }
    for ev, keys in key_expect.items():
        got = None
        for e in hooks:
            d = e.get("data") or {}
            if d.get("event") == ev:
                got = d.get("payloadKeys") or []
                break
        if got is None:
            checks.append(check(f"payload:{ev}_seen", False, "event never dispatched"))
            continue
        missing = [k for k in keys if k not in got]
        checks.append(check(f"payload:{ev}_keys", not missing,
                            f"keys={got} missing={missing}"))
    return checks, hooks


def main():
    if not os.path.isfile(PIR):
        raise SystemExit(f"missing pir binary: {PIR}")
    with open(EXT_SRC, encoding="utf-8") as f:
        probe_src = f.read()
    shutil.rmtree(SHOTS, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)

    base = os.path.join(ROOT, "runs", "main")
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    ext_dir = os.path.join(agent, "extensions")
    os.makedirs(ext_dir, exist_ok=True)
    shutil.copyfile(EXT_SRC, os.path.join(ext_dir, "r65probe.ts"))
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round65 hook switch probe\n")

    result = {"pir": PIR, "legs": {}, "audits": {}, "checks": {}}

    # ---------------- leg 1: multi-turn + /compact + /fork ----------------
    script1 = [
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R65 alpha"}},
            {"name": "todo", "arguments": {"action": "create", "subject": "R65 beta"}},
        ]},
        {"text": "R65-TODO-CREATED"},
        {"tool_call": {"name": "todo", "arguments": {"action": "update", "id": 1, "status": "completed"}}},
        {"text": "R65-TODO-UPDATED"},
        {"text": "R65-PRE-COMPACT filler paragraph for the compaction span. " * 30},
        {"text": "R65-AFTER-COMPACT"},
        {"text": "R65-POST-FORK"},
    ]
    ops1 = [
        ("wait_quiet", 5.0),
        ("snap", "r65_boot"),
        ("log", "turn 1: two todo creates"),        ("send", b"Create two todos alpha and beta.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "r65_turn1"),
        ("log", "turn 2: todo update"),
        ("send", b"Complete the first todo.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "r65_turn2"),
        ("log", "/compact with a real span"),
        ("send", b"/compact\r"),
        ("wait", 10.0), ("wait_quiet", 5.0),
        ("snap", "r65_compact"),
        ("log", "turn after compact"),
        ("send", b"Say the compaction worked.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "r65_after_compact"),
        ("log", "/fork via the real picker"),
        ("send", b"/fork\r"),
        ("wait", 3.0),
        ("snap", "r65_fork_picker"),
        ("send", b"\r"),
        ("wait", 5.0),
        ("snap", "r65_forked"),
        ("log", "post-fork prompt"),
        ("send", b"Say the fork worked.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "r65_post_fork"),
        ("log", "/quit"),
        ("send", b"/quit\r"),
        ("wait", 3.0),
    ]
    screens1 = {
        "r65_turn1": [("R65-TODO-CREATED", "present")],
        "r65_turn2": [("R65-TODO-UPDATED", "present")],
        "r65_compact": [("Compacted:", "present")],
        "r65_after_compact": [("R65-AFTER-COMPACT", "present")],
        "r65_fork_picker": [("Fork from message", "present")],
        "r65_post_fork": [("R65-POST-FORK", "present")],
    }
    leg1 = run_leg("s1_main", script1, ops1, project, agent, screens1)
    result["legs"]["s1_main"] = leg1
    events1 = leg1["events"]
    sess1 = newest_session(agent)
    result["audits"]["session_path_s1"] = sess1
    if sess1:
        j_checks, hooks = audit_session_file(sess1)
        result["checks"].update({f"s1.{c['check']}": c for c in j_checks})
        result["audits"]["r65hook_count_s1"] = len(hooks)
    t_checks = audit_timeline(events1)
    result["checks"].update({f"s1.{c['check']}": c for c in t_checks})

    # ---------------- leg 2: --continue resume of the same agent ----------
    script2 = [
        {"text": "R65-RESUMED-OK"},
    ]
    ops2 = [
        ("wait_quiet", 4.0),
        ("snap", "r65_resume_boot"),
        ("log", "resume prompt on --continue"),
        ("send", b"Confirm resume works.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "r65_resumed"),
        ("log", "/quit"),
        ("send", b"/quit\r"),
        ("wait", 3.0),
    ]
    screens2 = {
        "r65_resumed": [("R65-RESUMED-OK", "present")],
    }
    leg2 = run_leg("s2_resume", script2, ops2, project, agent, screens2,
                   timeout=240, args=["--continue"])
    result["legs"]["s2_resume"] = leg2
    events2 = leg2["events"]
    jsonl_audits = audit_all_sessions(agent)
    for key, chk in jsonl_audits.items():
        result["checks"][key] = chk

    # ---------------- leg 3: /sessions switch ----------------
    ops3 = [
        ("wait_quiet", 4.0),
        ("snap", "r65_sessions_open"),
        ("log", "/sessions picker"),
        ("send", b"/sessions\r"),
        ("wait", 3.0),
        ("snap", "r65_sessions_list"),
        ("log", "Down twice (skip '+ New session' and the active row), Enter switches"),
        ("send", b"\x1b[B"),
        ("wait", 0.6),
        ("send", b"\x1b[B"),
        ("wait", 0.6),
        ("send", b"\r"),
        ("wait", 5.0),
        ("snap", "r65_switched"),
        ("log", "/quit"),
        ("send", b"/quit\r"),
        ("wait", 3.0),
    ]
    screens3 = {
        "r65_sessions_list": [("Resume a session", "present")],
        "r65_switched": [("Resumed session", "present")],
    }
    leg3 = run_leg("s3_sessions_switch", [], ops3, project, agent, screens3)
    result["legs"]["s3_sessions_switch"] = leg3
    events3 = leg3["events"]
    jsonl_audits = audit_all_sessions(agent)
    for key, chk in jsonl_audits.items():
        result["checks"][key] = chk

    # summary
    all_checks = result["checks"]
    failed = [k for k, v in all_checks.items() if not v["ok"]]
    result["summary"] = {
        "total": len(all_checks), "failed": len(failed), "failed_keys": failed,
    }
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print(json.dumps(result["summary"], ensure_ascii=False))
    for k, v in result["legs"].items():
        print(f"leg {k}: exit={v['exit_code']} timed_out={v['timed_out']}")
    for k, v in sorted(all_checks.items()):
        print(("PASS " if v["ok"] else "FAIL ") + k + " :: " + v["detail"][:140])
    return 0 if (not failed and all(l["exit_code"] == 0 and not l["timed_out"]
                                    for l in result["legs"].values())) else 1


if __name__ == "__main__":
    sys.exit(main())
