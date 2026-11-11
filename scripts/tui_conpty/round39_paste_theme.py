# -*- coding: utf-8 -*-
"""Round 39 lane-tui: bracketed-paste editing/session fidelity, /image across
light<->dark theme switches, and transcript/scroll/todo/subagent preservation.

Three fresh-TUI legs over the fixed pir36.exe snapshot:
  A paste_edit      : multiline CJK/tab bracketed paste -> edits -> submit;
                      screen + session JSONL exact-text/line-count checks.
  B image_theme     : /image local PNG in dark, live theme switch to light
                      (preview re-rendered), /image again in light, back to dark.
  C panels_scroll   : todo + subagent panels, 3 turns, PageUp scroll position
                      compared before/after two live theme switches, then an
                      unsent editor draft after the switch.
"""
import json
import os
import shutil
import struct
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round39-paste-theme")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
SHOTS = os.path.join(ROOT, "shots")
IMG = os.path.join(ROOT, "img39.png")


def write_png(path):
    # 4x4 RGB PNG: red/green quadrants, no external deps.
    w = h = 4
    raw = b""
    for y in range(h):
        raw += b"\x00" + b"".join(
            (b"\xff\x00\x00" if (x < 2) == (y < 2) else b"\x00\x00\xff") for x in range(w))
    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data +
                struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))
    png = (b"\x89PNG\r\n\x1a\n" +
           chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) +
           chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
    with open(path, "wb") as f:
        f.write(png)


def rows_of(name):
    raw = open(os.path.join(SHOTS, name + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def find_lines(rows, needle):
    return [(i, r) for i, r in enumerate(rows) if needle in r]


def run_leg(name, script, ops, cwd, checks):
    base = os.path.join(ROOT, "runs", name)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    agent_md = os.path.join(agent, "agents", "helper.md")
    os.makedirs(os.path.dirname(agent_md), exist_ok=True)
    with open(agent_md, "w", encoding="utf-8") as f:
        f.write("---\nname: helper\ndescription: Round39 helper agent\n---\n"
                "You are a careful helper. Do the task and report briefly.\n")
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
            result_json=result_json, agent_dir=agent, timeout=150)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    leg = {"exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
           "elapsed_s": out.get("elapsed_s"), "session_file": None,
           "first_user_message": None, "checks": {}, "screen_stats": {}}
    sessions_root = os.path.join(agent, "sessions")
    for dirpath, _dirs, names in os.walk(sessions_root):
        for entry in names:
            if entry.endswith(".jsonl"):
                leg["session_file"] = os.path.join(dirpath, entry)
                break
        if leg["session_file"]:
            break
    if leg["session_file"]:
        with open(leg["session_file"], encoding="utf-8") as f:
            for line in f:
                try:
                    obj = json.loads(line)
                except ValueError:
                    continue
                msg = obj.get("message") or {}
                if msg.get("role") != "user":
                    continue
                content = msg.get("content")
                if isinstance(content, list):
                    text = "".join(p.get("text", "") for p in content if isinstance(p, dict))
                else:
                    text = str(content)
                if leg["first_user_message"] is None:
                    leg["first_user_message"] = text
                if checks.get("session_needle") and checks["session_needle"] in text:
                    leg["paste_user_message"] = text
                    leg["paste_user_message_count"] = leg.get("paste_user_message_count", 0) + 1
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
        if checks.get("no_paste_markers"):
            for marker in ("[200~", "[201~"):
                bad = raw.count(marker.encode())
                leg["checks"][f"{snap}:raw_no_{marker}"] = {"ok": bad == 0, "count": bad}
    return leg


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    write_png(IMG)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round39\n")
    result = {"pir": PIR, "image": IMG, "legs": {}}

    # ---- Leg A: multiline paste, post-paste edits, submit, session exact ----
    paste = "R39-L1\tTAB\nR39-L2 中文富文本\nR39-L3 tail"
    # The editor presents tabs as 4 spaces (soft-tab rendering) before insert.
    expected = paste.replace("\t", "    ") + " EDIT-X-EDITED"
    ops_a = [
        ("wait_quiet", 5.0),
        ("log", "bracketed multiline CJK/tab paste"),
        ("send", ("\x1b[200~" + paste + "\x1b[201~").encode("utf-8")),
        ("wait", 1.5), ("snap", "a_pasted"),
        ("log", "edit after paste: append EDIT-X, type -TMP, backspace it, type -EDITED"),
        ("send", b" EDIT-X"), ("wait", 0.6),
        ("send", b"-TMP"), ("wait", 0.4),
        ("send", b"\x7f\x7f\x7f\x7f"), ("wait", 0.4),
        ("send", b"-EDITED"), ("wait", 0.6),
        ("snap", "a_edited"),
        ("log", "submit the edited multiline prompt"),
        ("send", b"\r"), ("wait", 8.0), ("wait_quiet", 3.0),
        ("snap", "a_submitted"),
    ]
    leg_a = run_leg("a_paste_edit", [{"text": "R39-PASTE-REPLY"}], ops_a, project, {
        "session_needle": "R39-L1",
        "screens": {
            "a_pasted": [("R39-L1", "present"), ("R39-L2", "present"),
                         ("R39-L3", "present"), ("[200~", "absent"), ("[201~", "absent")],
            "a_edited": [("R39-L3 tail EDIT-X-EDITED", "present"),
                         ("-TMP", "absent"), ("[200~", "absent"), ("[201~", "absent")],
            "a_submitted": [("R39-PASTE-REPLY", "present"),
                            ("R39-L1", "present"), ("R39-L2", "present"),
                            ("R39-L3 tail EDIT-X-EDITED", "present")],
        },
        "no_mojibake": True, "no_paste_markers": True,
    })
    rows, _ = rows_of("a_edited")
    leg_a["paste_line_hits"] = {"R39-L%d" % i: len(find_lines(rows, "R39-L%d" % i))
                               for i in (1, 2, 3)}
    leg_a["expected_session_text"] = expected
    got = leg_a.get("paste_user_message")
    leg_a["session_exact"] = (got == expected)
    leg_a["session_exact_detail"] = {
        "expected_repr": repr(expected), "got_repr": repr(got),
        "got_lines": len(got.splitlines()) if got else 0,
    }
    result["legs"]["a_paste_edit"] = leg_a

    # ---- Leg B: /image in dark -> light (re-render) -> light -> dark ----
    image_ops = lambda: ("/image " + IMG + "\r").encode("utf-8")
    def theme_ops(direction):
        return [("send", b"/settings\r"), ("wait", 1.5), ("send", b"theme"),
                ("wait", 1.2), ("send", direction), ("wait", 1.5)]
    ops_b = [
        ("wait_quiet", 5.0), ("snap", "b_boot_dark"),
        ("log", "/image in dark"), ("send", image_ops()), ("wait", 4.0),
        ("snap", "b_image_dark"),
        ("log", "dark -> light while preview is in the transcript"),
    ] + theme_ops(b"\x1b[C") + [
        ("snap", "b_claim_light"),
        ("send", b"\x1b"), ("wait", 0.8), ("send", b"\x1b"), ("wait", 1.2),
        ("snap", "b_image_light_after_switch"),
        ("log", "/image again in light"), ("send", image_ops()), ("wait", 4.0),
        ("snap", "b_image_light_new"),
        ("log", "light -> dark"),
    ] + theme_ops(b"\x1b[D") + [
        ("send", b"\x1b"), ("wait", 0.8), ("send", b"\x1b"), ("wait", 1.2),
        ("snap", "b_image_dark_back"),
    ]
    leg_b = run_leg("b_image_theme", None, ops_b, project, {
        "screens": {
            "b_boot_dark": [("\u2580", "absent")],
            "b_image_dark": [("/image: rendering", "present"), ("\u2580", "present"),
                             ("[200~", "absent"), ("[201~", "absent")],
            "b_claim_light": [("Theme: light", "present")],
            "b_image_light_after_switch": [("\u2580", "present"),
                                           ("[200~", "absent"), ("[201~", "absent")],
            "b_image_light_new": [("/image: rendering", "present"), ("\u2580", "present")],
            "b_image_dark_back": [("Theme: dark", "present"), ("\u2580", "present")],
        },
        "no_mojibake": True, "no_paste_markers": True,
    })
    _, raw_boot = rows_of("b_boot_dark")
    _, raw_light = rows_of("b_claim_light")
    leg_b["light_accent_boot"] = raw_boot.count(b"\x1b[38;5;97m")
    leg_b["light_accent_after_switch"] = raw_light.count(b"\x1b[38;5;97m")
    result["legs"]["b_image_theme"] = leg_b

    # ---- Leg C: todo + subagent panels, scroll position across theme ------
    ops_c = [
        ("wait_quiet", 5.0),
        ("log", "turn 1: two todos"),
        ("send", b"R39-USER-1 plan two todos\r"), ("wait", 8.0), ("wait_quiet", 3.0),
        ("snap", "c_todo"),
        ("log", "turn 2: helper subagent"),
        ("send", b"R39-USER-2 use the helper subagent to write subagent_r39.txt."),
        ("wait", 0.6), ("send", b"\r"), ("wait", 10.0), ("wait_quiet", 3.0),
        ("snap", "c_subagent"),
        ("log", "turn 3: grows the transcript"),
        ("send", b"R39-USER-3 third turn\r"), ("wait", 6.0), ("wait_quiet", 2.0),
        ("snap", "c_turn3"),
        ("log", "scroll up before the theme switches"),
        ("send", b"\x1b[5~"), ("wait", 0.4), ("send", b"\x1b[5~"), ("wait", 0.8),
        ("snap", "c_scrolled"),
        ("log", "dark -> light with panels/scroll present"),
    ] + theme_ops(b"\x1b[C") + [
        ("send", b"\x1b"), ("wait", 0.8), ("send", b"\x1b"), ("wait", 1.2),
        ("snap", "c_light"),
        ("log", "light -> dark again"),
    ] + theme_ops(b"\x1b[D") + [
        ("send", b"\x1b"), ("wait", 0.8), ("send", b"\x1b"), ("wait", 1.2),
        ("snap", "c_dark"),
        ("log", "jump back to the latest message after both switches"),
        ("send", b"\x1b[1;5F"), ("wait", 1.2), ("snap", "c_bottom"),
        ("log", "unsent editor draft after the switches"),
        ("send", "EDITOR-R39-草稿".encode("utf-8")), ("wait", 1.0),
        ("snap", "c_editor_draft"),
    ]
    script_c = [
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R39 todo alpha"}},
            {"name": "todo", "arguments": {"action": "create", "subject": "R39 todo beta"}},
        ]},
        {"text": "TODO-R39-READY"},
        {"tool_call": {"name": "subagent", "arguments": {
            "agent": "helper",
            "task": "Create a file subagent_r39.txt containing the single line: helper r39"}}},
        {"tool_call": {"name": "bash", "arguments": {
            "command": "printf 'helper r39\\n' > subagent_r39.txt"}}},
        {"text": "SUBAGENT-R39-CHILD"},
        {"text": "SUBAGENT-R39-OK"},
        {"text": "TURN3-R39-DONE"},
        {"text": "FINAL-R39"},
    ]
    leg_c = run_leg("c_panels_scroll", script_c, ops_c, project, {
        "screens": {
            "c_todo": [("2 pending", "present"), ("#1 R39 todo alpha", "present")],
            "c_subagent": [("subagent helper", "present"), ("SUBAGENT-R39-OK", "present"),
                           ("2 pending", "present")],
            "c_turn3": [("TURN3-R39-DONE", "present")],
            "c_scrolled": [("R39-USER-1", "present")],
            "c_light": [("Theme: light", "present"), ("#1 R39 todo alpha", "present")],
            "c_dark": [("Theme: dark", "present"), ("#1 R39 todo alpha", "present")],
            "c_bottom": [("SUBAGENT-R39-OK", "present"), ("TURN3-R39-DONE", "present"),
                         ("TODO-R39-READY", "present")],
            "c_editor_draft": [("EDITOR-R39-草稿", "present")],
        },
        "no_mojibake": True, "no_paste_markers": True,
    })
    def rowpos(snap, needle):
        rows, _ = rows_of(snap)
        hits = find_lines(rows, needle)
        return hits[0][0] if hits else None
    leg_c["scroll_rows"] = {}
    for snap in ("c_scrolled", "c_light", "c_dark"):
        leg_c["scroll_rows"][snap] = {n: rowpos(snap, n) for n in (
            "R39-USER-1", "R39-USER-2", "TODO-R39-READY", "TURN3-R39-DONE")}
    before = leg_c["scroll_rows"]["c_scrolled"]
    after = leg_c["scroll_rows"]["c_light"]
    common = [n for n in before if before[n] is not None and after.get(n) is not None]
    leg_c["scroll_preserved"] = bool(common) and all(abs(before[n] - after[n]) <= 1 for n in common)
    result["legs"]["c_panels_scroll"] = leg_c

    acc3_render.render_dir(SHOTS)
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, default=str)
    # Summary + pass/fail
    failed = []
    for name, leg in result["legs"].items():
        bad = [k for k, v in leg["checks"].items() if not v["ok"]]
        if leg.get("exit_code") != 0 or leg.get("timed_out"):
            bad.append("exit/timed_out")
        if name == "a_paste_edit":
            if not leg.get("session_exact"):
                bad.append("session_exact")
            if any(v != 1 for v in leg.get("paste_line_hits", {}).values()):
                bad.append("paste_line_hits")
        if name == "b_image_theme":
            if not (leg["light_accent_boot"] == 0 and leg["light_accent_after_switch"] >= 1):
                bad.append("light_accent")
        if name == "c_panels_scroll" and not leg.get("scroll_preserved"):
            bad.append("scroll_preserved")
        leg["failed_checks"] = bad
        if bad:
            failed.append(name)
        print(name, "exit=", leg.get("exit_code"), "timed_out=", leg.get("timed_out"),
              "failed=", bad)
    print("RESULT:", "ALL_PASS" if not failed else "FAIL:" + ",".join(failed))


if __name__ == "__main__":
    main()
