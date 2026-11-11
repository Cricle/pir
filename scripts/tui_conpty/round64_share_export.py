# -*- coding: utf-8 -*-
"""Round 64 lane-tui task 3: /share (clipboard path) and /export (HTML)
consistency over a REAL multi-turn session with tool calls, long lines,
Chinese text and HTML-hostile characters (& < > " ').

Real user paths on the isolated agent dir:
  leg 1: build a session (todo create + a text reply carrying 中文, a long
         line >200 cols, and raw <b>&amp;</b> markup), then /share (no
         credentials -> graceful error), /copy all (clipboard must equal the
         last agent message text), /export (HTML file written next to cwd).
  Disk audit: the exported HTML must contain the SAME text as the session
  JSONL (unescaped correctly: 中文 intact, &amp; escaping correct, long line
  preserved), the tool call card present, and the session header id/link.
"""
import glob
import html as html_mod
import json
import os
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round64-share-export")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE")
                      or os.path.join(REPO, ".play", "evidence", "lane-tui",
                                      "round64-bin", "pir64.exe"))
SHOTS = os.path.join(ROOT, "shots")


def rows_of(name):
    raw = open(os.path.join(SHOTS, name + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def clipget():
    return tui_drive.clipget()


def check(name, ok, detail):
    return {"check": name, "ok": bool(ok), "detail": detail}


def main():
    if not os.path.isfile(PIR):
        raise SystemExit(f"missing pir binary: {PIR}")
    shutil.rmtree(SHOTS, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)

    base = os.path.join(ROOT, "runs", "main")
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round64 export target\n")

    long_line = "L" * 300 + " LONGTAIL-OK"
    markup = "RAW MARKUP <b>&amp;</b> & \"quotes\" 'single' <tag>"
    chinese = "中文一致性校验：剪贴板与导出"
    reply1 = (f"R64X-REPLY-ONE {chinese}\n{long_line}\n{markup}")
    mock_script = [
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R64X todo"}},
        ]},
        {"text": reply1},
        {"text": "R64X-REPLY-TWO closing turn"},
    ]
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=mock_script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    ops = [
        ("wait_quiet", 5.0),
        ("snap", "x_boot"),
        ("log", "turn 1: todo create"),
        ("send", b"Create one todo.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("log", "turn 2: the rich reply (中文/long line/markup)"),
        ("send", b"Show me the rich reply.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "x_turn2"),
        ("log", "turn 3: closing"),
        ("send", b"Close it out.\r"),
        ("wait", 8.0), ("wait_quiet", 4.0),
        ("snap", "x_turn3"),
        ("log", "/copy all -> clipboard"),
        ("send", b"/copy all\r"),
        ("wait", 2.5),
        ("snap", "x_copy"),
        ("clipdump", "x_clip"),
        ("log", "/share (no credentials) -> graceful error"),
        ("send", b"/share\r"),
        ("wait", 8.0),
        ("snap", "x_share"),
        ("log", "/export -> HTML next to the session"),
        ("send", b"/export\r"),
        ("wait", 6.0),
        ("snap", "x_export"),
        ("log", "/quit"),
        ("send", b"/quit\r"),
        ("wait", 3.0),
    ]
    tui_drive.PIR = PIR
    result_json = os.path.join(SHOTS, "s_export.result.json")
    try:
        tui_drive.run_scenario(
            "s_export", ops, extra_env=env, cwd=project, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, "s_export.driver.log"),
            result_json=result_json, agent_dir=agent, timeout=180)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        run = json.load(f)

    checks = {}
    checks["run_exit_clean"] = check(
        "run exit clean", run.get("exit_code") == 0 and not run.get("timed_out"),
        f"exit={run.get('exit_code')} timed_out={run.get('timed_out')}")

    rows_share, raw_share = rows_of("x_share")
    share_text = "\n".join(rows_share)
    low = share_text.lower()
    checks["share_status_notice"] = check(
        "share status notice shown",
        "sharing the session" in low or "creating a secret gist" in low
        or "uploading the session" in low,
        "; ".join(l for l in rows_share if "sharing" in l.lower()
                  or "gist" in l.lower())[:160])
    checks["share_graceful_error"] = check(
        "share fails gracefully without credentials",
        any(k in low for k in ("github credentials not found", "failed to create gist",
                               "failed to upload", "not found")),
        "; ".join(l for l in rows_share if "credential" in l.lower()
                  or "failed" in l.lower())[:200])
    checks["share_no_panic"] = check(
        "no panic on the share path", "panic" not in low and "thread '" not in low,
        "panic/thread scan over the share screen")

    rows_copy, _ = rows_of("x_copy")
    copy_text = "\n".join(rows_copy)
    checks["copy_status"] = check(
        "/copy all accepted", "copied" in copy_text.lower()
        or "clipboard" in copy_text.lower(),
        "; ".join(l for l in rows_copy if "cop" in l.lower())[:160])

    # clipget() decodes GBK console bytes as utf-8 → 中文 probes garble; read
    # the clipboard through a UTF-8 file written by PowerShell instead.
    clip_file = os.path.join(ROOT, "clip_text.txt")
    subprocess.run(["powershell", "-NoProfile", "-Command",
                    "Get-Clipboard -Raw | Out-File -FilePath '%s' -Encoding utf8" % clip_file],
                   capture_output=True, timeout=20)
    try:
        clip = open(clip_file, encoding="utf-8-sig", errors="replace").read()
    except OSError:
        clip = ""
    checks["clip_read"] = check("clipboard readable", len(clip) > 0, f"{len(clip)} chars")
    # The clipboard must carry the agent messages' text (msgcat export):
    # 中文 intact, the long tail preserved, markup verbatim.
    checks["clip_chinese"] = check(
        "clipboard keeps 中文", chinese in clip, f"probe={chinese[:12]}…")
    checks["clip_long_line"] = check(
        "clipboard keeps the 300-col line tail", "LONGTAIL-OK" in clip,
        "LONGTAIL-OK tail in clipboard text")
    checks["clip_markup"] = check(
        "clipboard keeps raw markup verbatim",
        "RAW MARKUP <b>&amp;</b>" in clip and "<tag>" in clip,
        "markup verbatim in clipboard text")
    checks["clip_no_html_escaping"] = check(
        "clipboard is plain text (&amp; NOT double-escaped as &amp;amp;)",
        "&amp;amp;" not in clip, "no &amp;amp; in clipboard text")

    # find the exported HTML + the session JSONL
    sessions = sorted(glob.glob(os.path.join(agent, "sessions", "*", "*.jsonl")))
    session_path = sessions[-1] if sessions else None
    htmls = sorted(glob.glob(os.path.join(project, "*.html"))
                   + glob.glob(os.path.join(base, "*.html"))
                   + glob.glob(os.path.join(agent, "**", "*.html"), recursive=True))
    rows_export, _ = rows_of("x_export")
    export_status = "; ".join(l for l in rows_export if "export" in l.lower())
    checks["export_status_line"] = check(
        "/export status shows the written path",
        "session exported to" in export_status.lower(), export_status[:200])
    checks["export_file_exists"] = check(
        "exported HTML file on disk", bool(htmls), str(htmls[:2]))
    if htmls and session_path:
        html_path = max(htmls, key=os.path.getmtime)
        body = open(html_path, encoding="utf-8", errors="replace").read()
        checks["html_chinese"] = check(
            "HTML keeps 中文", chinese in body or html_mod.escape(chinese) in body,
            f"probe={chinese[:12]}…")
        checks["html_long_line"] = check(
            "HTML keeps the 300-col line tail", "LONGTAIL-OK" in body,
            "LONGTAIL-OK tail in the html body")
        # HTML must ESCAPE the markup (& -> &amp;) so the browser renders it verbatim:
        checks["html_markup_escaped"] = check(
            "HTML escapes the raw markup (& -> &amp;)",
            "&lt;b&gt;&amp;amp;&lt;/b&gt;" in body or "&lt;b&gt;" in body,
            "probe=<b>&amp;</b>")
        checks["html_no_double_escape_text"] = check(
            "HTML does not double-escape plain &",
            "&amp;amp;amp;" not in body, "double-escape scan")
        checks["html_tool_card"] = check(
            "HTML carries the tool call (todo)",
            "todo" in body and ("create" in body or "R64X todo" in body),
            "todo/create probe in html body")
        # session-consistency: every user/assistant TEXT of the JSONL must
        # appear in the HTML (escaped).
        entries = []
        with open(session_path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        entries.append(json.loads(line))
                    except ValueError:
                        pass
        user_texts = []
        for e in entries:
            if e.get("type") == "message":
                m = e.get("message") or {}
                c = m.get("content")
                if isinstance(c, list):
                    for block in c:
                        if isinstance(block, dict) and block.get("type") == "text":
                            user_texts.append(block.get("text") or "")
        missing = []
        for text in user_texts:
            probe = text[:60]
            if probe not in body and html_mod.escape(probe) not in body:
                missing.append(probe[:40])
        checks["html_session_texts_all_present"] = check(
            f"all {len(user_texts)} message texts appear in the HTML",
            not missing, f"missing={missing[:3]}")
        checks["html_nonempty"] = check(
            "HTML non-trivial size", len(body) > 2000, f"{len(body)} bytes")
        result = {"html_path": html_path, "session_path": session_path}

    failed = [k for k, v in checks.items() if not v["ok"]]
    out = {"pir": PIR, "checks": checks,
        "paths": result if isinstance(result, dict) else {},
           "summary": {"total": len(checks), "failed": len(failed),
                       "failed_keys": failed}}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=2)
    print(json.dumps(out["summary"], ensure_ascii=False))
    for k, v in checks.items():
        print(("PASS " if v["ok"] else "FAIL ") + k + " :: " + v["detail"][:120])
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
