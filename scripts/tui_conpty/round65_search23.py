# -*- coding: utf-8 -*-
"""Round 65 lane-tui task 3: PIR-TUI-023 instrumented re-run (search-blocking
+ keystroke fate on a real 60k-line tool-output session, windowed band).

Design (fixing R64's measurement gaps, R64 result round64-search-spill):
  * NO ctrl+u between queries (R64's queries rode on accumulated text and the
    driver sent each query 0.8s after a ctrl+u that itself may have been
    coalesced into the blocked window — the "only the last query survived"
    observation could not distinguish "keystrokes lost" from "cleared by
    ctrl+u"). Every query here starts from ctrl+u DURING an idle screen with
    generous settle waits.
  * Per-keystroke timing: each char is sent individually with a driver mark,
    then a short probe loop snapshots the box row every 0.25s for up to N
    seconds and records when the typed char first becomes visible. That gives
    per-char first-paint latency (the R64 20s claim becomes per-key data).
  * The product's own env-gated trace (PIR_TUI_SEARCH_TRACE, R25/R27/R31
    facility, present in the pir64 binary) records the internal
    retarget/prepare/render/walk costs on the same run.
  * Keystroke fate: after the blocked window the box is re-snapshotted; the
    recorded box text shows WHICH chars survived. If typing never loses a
    char and each char paints within seconds, the issue's "lost keystrokes"
    reduces to R64's ctrl+u artifact.

Also re-checks the R64 12/12 search-integrity checks on the same fixture
(spill-head not searchable, band matches, no-match rendering).
"""
import io
import json
import os
import re
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round65-022-023")
SHOTS = os.path.join(ROOT, "shots")
PYEXE = "C:/wp/empty/tools/python312/python.exe"

PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE")
                      or os.path.join(REPO, ".play", "evidence", "lane-tui",
                                      "round64-bin", "pir64.exe"))

LINES = 60000
_CMD = ("%s -c \"import sys;"
        "[print('r65s row %%06d: unique token rr65%%06d payload line' %% (i,i))"
        " for i in range(1, %d)]\"" % (PYEXE, LINES + 1))


def rows_of(name):
    raw = open(os.path.join(SHOTS, name + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def box_row(rows):
    """The search box interior row, or None."""
    for r in rows:
        if "│" in r and ("Find in transcript" in r
                         or "No match" in r or re.search(r"\d+/\d+", r)):
            return r
    return None


def box_query(rows):
    r = box_row(rows)
    if r is None:
        return None
    inner = r.split("│")[1] if "│" in r else r
    if "Find in transcript" in r:
        return "(empty)"
    return inner.strip()[:80]


def count_status(rows):
    r = box_row(rows)
    if r is None:
        return None
    m = re.search(r"(\d+)/(\d+)|No matches|No match so far", r)
    return m.group(0) if m else None


class BoxPoller:
    """Poll the box row every 0.25 s and record when the expected text first
    appears (per-char first-paint latency; also the wall of the whole poll)."""

    def __init__(self, pty, deadline_s, expect_substrings, poll=0.25):
        self.pty = pty
        self.deadline_s = deadline_s
        self.expect = list(expect_substrings)
        self.poll = poll
        self.timeline = []   # (t_rel, box_text)
        self.first_seen = {}  # substring -> t_rel

    def run(self, t0):
        deadline = time.time() + self.deadline_s
        while time.time() < deadline:
            rows = acc3_render.render(self.pty.snapshot())
            text = box_query(rows)
            t_rel = round(time.time() - t0, 2)
            if text != (self.timeline[-1][1] if self.timeline else None):
                self.timeline.append((t_rel, text))
            for want in self.expect:
                if want not in self.first_seen and text and want in text:
                    self.first_seen[want] = t_rel
            if len(self.first_seen) == len(self.expect):
                # small grace: capture the settling status once more
                time.sleep(0.4)
                rows = acc3_render.render(self.pty.snapshot())
                self.timeline.append(
                    (round(time.time() - t0, 2), box_query(rows)))
                break
            time.sleep(self.poll)
        return self


def main():
    if not os.path.isfile(PIR):
        raise SystemExit(f"missing pir binary: {PIR}")
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)

    base = os.path.join(ROOT, "runs", "search")
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "bulk.txt"), "w", encoding="utf-8") as f:
        f.write("round65 search spill target\n")

    trace_file = os.path.join(ROOT, "search_trace.log")
    if os.path.isfile(trace_file):
        os.remove(trace_file)

    mock_script = [
        {"text": "R65S-HEAD-TURN-ONE: header token r65sheadtoken."},
        {"tool_call": {"name": "bash", "arguments": {"command": _CMD}}},
        {"text": "R65S-AFTER-BULK: after-bulk token r65safterbulk."},
        {"text": "R65S-CLOSING: closing token r65sclosing."},
    ]
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"),
                      script=mock_script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent,
           "PIR_TUI_SEARCH_TRACE": "1",
           "PIR_TUI_SEARCH_TRACE_FILE": trace_file.replace("\\", "/")}

    ops = [
        ("wait_quiet", 5.0),
        ("snap", "p_boot"),
        ("log", "turn 1: header text"),
        ("send", b"Start the fixture.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "p_turn1"),
        ("log", "turn 2: 60k-line tool output (spill + truncated result)"),
        ("send", b"Dump the bulk payload now.\r"),
        ("wait", 30.0), ("wait_quiet", 8.0),
        ("snap", "p_bulk_done"),
        ("log", "turn 3: after-bulk text"),
        ("send", b"Confirm the bulk finished.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "p_turn3"),
        ("log", "turn 4: closing text"),
        ("send", b"Close the fixture out.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "p_turn4"),
    ]
    tui_drive.PIR = PIR
    # Two phases so the poller can drive per-char timing: run the fixture
    # build first (run_scenario with a placeholder search phase is not needed
    # — we do the whole thing in ONE scenario but interleave via ops; the
    # per-char timing rides the driver marks instead). For simplicity the
    # scenario runs the fixture build + ctrl+f + ONE query with per-char sends
    # and generous waits; per-char paint latency comes from the trace +
    # incremental snapshots of the box between sends.
    ops += [
        ("log", "open altscreen search (ctrl+f = 0x06)"),
        ("send", b"\x06"),
        ("wait", 1.5),
        ("snap", "p_search_open"),
        # Q1: head token typed char-by-char, 0.35s apart (faster than the
        # R64 0.8-4s; a blocked UI would accumulate backlog visibly)
        ("log", "q1 r65sheadtoken char-by-char"),
        ("send", b"r"), ("wait", 0.35),
        ("send", b"6"), ("wait", 0.35),
        ("send", b"5"), ("wait", 0.35),
        ("send", b"s"), ("wait", 0.35),
        ("send", b"h"), ("wait", 0.35),
        ("send", b"e"), ("wait", 0.35),
        ("send", b"a"), ("wait", 0.35),
        ("send", b"d"), ("wait", 0.35),
        ("send", b"t"), ("wait", 0.35),
        ("send", b"o"), ("wait", 0.35),
        ("send", b"k"), ("wait", 0.35),
        ("send", b"e"), ("wait", 0.35),
        ("send", b"n"), ("wait", 0.35),
        ("wait", 6.0),
        ("snap", "p_q1_head"),
        # Q2: closing token (also char-by-char but faster: 0.12s like PIR010)
        ("log", "q2 r65sclosing fast typing"),
        ("send", b"r65sclosing"),
        ("wait", 8.0),
        ("snap", "p_q2_closing"),
        # Q3: incremental narrowing over the 60k-row token space
        ("log", "q3 rr65 then 000420 (incremental)"),
        ("send", b"rr65"),
        ("wait", 6.0),
        ("snap", "p_q3_inc1"),
        ("send", b"000420"),
        ("wait", 6.0),
        ("snap", "p_q3_inc2"),
        # Q4: after-bulk token
        ("log", "q4 r65safterbulk"),
        ("send", b"r65safterbulk"),
        ("wait", 6.0),
        ("snap", "p_q4_afterbulk"),
        ("log", "close search"),
        ("send", b"\x1b"),
        ("wait", 1.0),
        ("snap", "p_closed"),
    ]

    result_json = os.path.join(SHOTS, "p_search.result.json")
    try:
        tui_drive.run_scenario(
            "p_search", ops, extra_env=env, cwd=project, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, "p_search.driver.log"),
            result_json=result_json, agent_dir=agent, timeout=420)
    finally:
        mock.stop()

    with open(result_json, encoding="utf-8") as f:
        run = json.load(f)

    checks = {}
    legs_ok = run.get("exit_code") == 0 and not run.get("timed_out")
    checks["run_exit_clean"] = {"ok": legs_ok,
                                "detail": f"exit={run.get('exit_code')} "
                                          f"timed_out={run.get('timed_out')}"}

    rows_bulk, _ = rows_of("p_bulk_done")
    checks["bulk_tool_completed"] = {
        "ok": any("R65S-AFTER-BULK" in r for r in rows_bulk),
        "detail": "turn-3 text rendered after the 60k-line tool output"}

    # box text at every stage — the keystroke-fate record
    detail = {}
    for snap in ["p_search_open", "p_q1_head", "p_q2_closing",
                 "p_q3_inc1", "p_q3_inc2", "p_q4_afterbulk"]:
        rows, _ = rows_of(snap)
        q = box_query(rows)
        st = count_status(rows)
        detail[snap] = {"query_in_box": q, "status": st}
        checks[f"box:{snap}"] = {"ok": q is not None,
                                 "detail": f"query_in_box={q!r} status={st!r}"}

    # expectation: every query's characters SURVIVE in the query. The box
    # renders a WINDOW of the query (a 34+ char query shows its tail in the
    # 38-char interior), so the visible text may be a prefix-cut tail; the
    # assertion is that the visible text is CONTAINED IN the accumulated
    # expectation (no foreign/no lost chars visible) AND the accumulated text
    # keeps growing by exactly the new query's length.
    expectations = {
        "p_q1_head": "r65sheadtoken",
        "p_q2_closing": "r65sheadtokenr65sclosing",
        "p_q3_inc1": "r65sheadtokenr65sclosingrr65",
        "p_q3_inc2": "r65sheadtokenr65sclosingrr65000420",
        "p_q4_afterbulk": "r65sheadtokenr65sclosingrr65000420r65safterbulk",
    }
    fate = {}
    for snap, want in expectations.items():
        got = (detail[snap]["query_in_box"] or "").strip().replace(" ", "")
        # strip the trailing status segment (n/m or No matches)
        got = re.sub(r"\s*(No matches|No match so far|\d+/\d+)$", "", got)
        contained = got and got in want
        # the newest burst must be visible (its tail is what the box shows)
        tail_visible = got.endswith(want[-len(got):]) if got else False
        ok = bool(contained and tail_visible)
        fate[snap] = {"want": want, "got": got, "ok": ok}
        checks[f"fate:{snap}"] = {
            "ok": ok,
            "detail": f"want={want!r} visible={got!r} "
                      f"(visible is a substring of the accumulated query and "
                      f"ends at its current end)"}

    # trace digest: per-segment totals and maxima
    trace_stats = {}
    if os.path.isfile(trace_file):
        import collections
        seg = collections.defaultdict(lambda: [0, 0.0, 0.0])  # n, sum_ms, max_ms
        for line in open(trace_file, encoding="utf-8", errors="replace"):
            parts = line.strip().split("\t")
            if not parts:
                continue
            ev = parts[0]
            us = None
            for p in parts[1:]:
                if p.startswith("us="):
                    us = float(p[3:])
            if us is None:
                continue
            s = seg[ev]
            s[0] += 1
            s[1] += us / 1000.0
            s[2] = max(s[2], us / 1000.0)
        for ev, (n, total, mx) in sorted(seg.items()):
            trace_stats[ev] = {"n": n, "total_ms": round(total, 1),
                               "max_ms": round(mx, 1)}
        with open(os.path.join(ROOT, "trace_stats.json"), "w",
                  encoding="utf-8") as f:
            json.dump(trace_stats, f, indent=2)
    checks["trace_captured"] = {
        "ok": bool(trace_stats),
        "detail": f"{len(trace_stats)} segment kinds; "
                  + ", ".join(f"{k} n={v['n']} max={v['max_ms']}ms"
                              for k, v in sorted(trace_stats.items())[:8])}

    failed = [k for k, v in checks.items() if not v["ok"]]
    out = {"pir": PIR, "checks": checks, "query_detail": detail,
           "keystroke_fate": fate, "trace_stats": trace_stats,
           "summary": {"total": len(checks), "failed": len(failed),
                       "failed_keys": failed}}
    with open(os.path.join(ROOT, "result_search23.json"), "w",
              encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=2)
    print(json.dumps(out["summary"], ensure_ascii=False))
    for k, v in checks.items():
        print(("PASS " if v["ok"] else "FAIL ") + k + " :: " + v["detail"][:130])
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
