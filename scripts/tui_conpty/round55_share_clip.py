# -*- coding: utf-8 -*-
"""Round 55 lane-tui: /share (no credentials), /import (missing/bad/good
JSONL) and real clipboard text + image paste journeys.

All runs use an isolated HOME/PIR_AGENT_DIR and the loopback mock LLM. No
GitHub/Radius credentials exist in the child environment, so /share must fail
gracefully (status notice + error, no panic/hang) while the TUI stays usable.

Evidence layout:
  .play/evidence/lane-tui/round55-share-clip/
    result.json, shots/, runs/<leg>/{home,.pi/agent,...}, clip.png
"""
import glob
import json
import os
import shutil
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.environ.get("PIR_R55_EVIDENCE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round55-share-clip")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round53-settings", "pir53.exe"))
SHOTS = os.path.join(ROOT, "shots")
TEXT = "R55-CLIP 中文剪贴板 line2"


def write_png(path, w=48, h=32):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w):
            if x in (0, w - 1) or y in (0, h - 1):
                r, g, b = 255, 255, 255
            elif x < w // 2 and y < h // 2:
                r, g, b = 220, 40, 40
            elif x >= w // 2 and y < h // 2:
                r, g, b = 40, 160, 60
            elif x < w // 2:
                r, g, b = 40, 80, 220
            else:
                r, g, b = 230, 190, 40
            raw += bytes((r, g, b))

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def set_clipboard_image(path):
    ps = (
        "Add-Type -AssemblyName System.Windows.Forms; "
        "Add-Type -AssemblyName System.Drawing; "
        f"$img=[System.Drawing.Image]::FromFile('{path}'); "
        "[System.Windows.Forms.Clipboard]::SetImage($img); $img.Dispose()"
    )
    r = subprocess.run(["powershell", "-STA", "-NoProfile", "-Command", ps],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError("clipboard image set failed: " + r.stderr[:300])


def run_tui(name, ops, agent, cwd, timeout=120, mock_log=None):
    home = os.path.dirname(os.path.dirname(agent))  # <base>/home/.pi/agent
    mock = MockServer(log_path=mock_log or os.path.join(ROOT, "runs", name, "mock_llm.log"),
                      script=None)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    rj = os.path.join(SHOTS, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=cwd, shots_dir=SHOTS,
            driver_log=os.path.join(SHOTS, name + ".driver.log"),
            result_json=rj, agent_dir=agent, timeout=timeout)
    finally:
        mock.stop()
    with open(rj, encoding="utf-8") as f:
        return json.load(f)


def rows_of(snap):
    raw = open(os.path.join(SHOTS, snap + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def screen_text(snap):
    return "\n".join(rows_of(snap)[0])


def add_screen_checks(checks, snap, specs, no_mojibake=True):
    rows, raw = rows_of(snap)
    text = "\n".join(rows)
    for needle, mode in specs:
        n = sum(r.count(needle) for r in rows)
        checks[f"{snap}:{mode}:{needle[:60]}"] = {
            "ok": (n >= 1 if mode == "present" else n == 0), "count": n}
    if no_mojibake:
        bad = raw.count(b"\xef\xbf\xbd")
        checks[f"{snap}:no_mojibake"] = {"ok": bad == 0, "count": bad}
    return text


def newest_session(agent):
    # The session writer can still be rotating the final JSONL name when the
    # TUI process exits (first R56 run: glob listed the path, getsize raced the
    # rename). Poll briefly instead of failing the leg on that timing window.
    import time
    deadline = time.time() + 5.0
    while True:
        files = glob.glob(os.path.join(agent, "sessions", "**", "*.jsonl"), recursive=True)
        files = [p for p in files if os.path.exists(p) and os.path.getsize(p) > 0]
        if files:
            return max(files, key=os.path.getmtime)
        if time.time() >= deadline:
            return None
        time.sleep(0.2)


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    png = os.path.join(ROOT, "clip.png")
    write_png(png)
    result = {"pir": PIR, "pir_sha256": None, "legs": {}}
    try:
        import hashlib
        result["pir_sha256"] = hashlib.sha256(open(PIR, "rb").read()).hexdigest()
    except OSError:
        pass

    # ---------------- leg 1: clipboard text ----------------
    base = os.path.join(ROOT, "runs", "clip_text")
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    open(os.path.join(project, "note.txt"), "w", encoding="utf-8").write("round55\n")
    ops = [
        ("wait_quiet", 5.0),
        ("log", "clipboard text paste via alt+v"),
        ("clipset", TEXT),
        ("send", b"\x1bv"), ("wait", 6.0), ("wait_quiet", 1.0),
        ("snap", "clip_text_pasted"),
        ("log", "submit pasted text"),
        ("send", b"\r"), ("wait", 6.0), ("wait_quiet", 2.0),
        ("snap", "clip_text_submitted"),
    ]
    out = run_tui("clip_text", ops, agent, project)
    checks = {}
    add_screen_checks(checks, "clip_text_pasted", [(TEXT, "present")])
    add_screen_checks(checks, "clip_text_submitted", [("R55-CLIP", "present")])
    session = newest_session(agent)
    raw_session = open(session, encoding="utf-8").read() if session else ""
    checks["session:text-present"] = {"ok": TEXT in raw_session, "path": session}
    # no bracketed-paste escapes leaked into the session
    checks["session:no-paste-escapes"] = {"ok": "[200~" not in raw_session and "[201~" not in raw_session}
    result["legs"]["clip_text"] = {
        "exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
        "elapsed_s": out.get("elapsed_s"), "session": session, "checks": checks}
    text_session = session

    # ---------------- leg 2: clipboard image ----------------
    base = os.path.join(ROOT, "runs", "clip_image")
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    open(os.path.join(project, "note.txt"), "w", encoding="utf-8").write("round55 image\n")
    set_clipboard_image(png)
    ops = [
        ("wait_quiet", 5.0),
        ("log", "clipboard image paste via alt+v"),
        ("send", b"\x1bv"), ("wait", 5.0),
        ("snap", "clip_image_pasted"),
        ("log", "submit pasted image reference"),
        ("send", b"\r"), ("wait", 6.0), ("wait_quiet", 2.0),
        ("snap", "clip_image_submitted"),
    ]
    out = run_tui("clip_image", ops, agent, project)
    checks = {}
    pasted = add_screen_checks(checks, "clip_image_pasted",
                               [("pi-clipboard-", "present"), (".png", "present")])
    checks["screen:image-reference-quoted"] = {"ok": '@"' in pasted}
    add_screen_checks(checks, "clip_image_submitted", [("R55-CLIP", "absent")])
    session = newest_session(agent)
    raw_session = open(session, encoding="utf-8").read() if session else ""
    checks["session:image-mime"] = {"ok": "image/png" in raw_session, "path": session}
    checks["session:image-block"] = {"ok": '"type":"image"' in raw_session.replace(" ", "")}
    mock_log = os.path.join(ROOT, "runs", "clip_image", "mock_llm.log")
    mock_raw = open(mock_log, encoding="utf-8", errors="replace").read() if os.path.exists(mock_log) else ""
    # The mock provider logs the expanded prompt reference; the scripted model
    # has no vision, so the harness records "(image omitted: model does not
    # support images)" — the attachment itself is proven by the session JSONL.
    checks["mock:image-reference"] = {
        "ok": "pi-clipboard-" in mock_raw or "image omitted" in mock_raw}
    result["legs"]["clip_image"] = {
        "exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
        "elapsed_s": out.get("elapsed_s"), "session": session, "checks": checks}

    # ---------------- leg 3: /share + /import ----------------
    base = os.path.join(ROOT, "runs", "share_import")
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    open(os.path.join(project, "note.txt"), "w", encoding="utf-8").write("round55 import\n")
    open(os.path.join(project, "bad.jsonl"), "w", encoding="utf-8").write("{this is not a session}\n")
    good = os.path.join(project, "good-session.jsonl")
    if text_session:
        shutil.copyfile(text_session, good)
    else:
        open(good, "w", encoding="utf-8").write("")
    ops = [
        ("wait_quiet", 5.0),
        ("log", "/share with no credentials"),
        ("send", b"/share\r"), ("wait", 8.0),
        ("snap", "share"),
        ("log", "/import no argument -> usage"),
        ("send", b"/import\r"), ("wait", 1.2), ("snap", "import_usage"),
        ("log", "/import missing file -> error before modal"),
        ("send", b"/import missing-import.jsonl\r"), ("wait", 1.2),
        ("snap", "import_missing"),
        ("log", "/import bad.jsonl -> confirm No"),
        ("send", b"/import bad.jsonl\r"), ("wait", 1.2), ("snap", "import_confirm_bad"),
        ("send", b"n"), ("wait", 1.0), ("snap", "import_cancel"),
        ("log", "/import bad.jsonl -> confirm Yes -> parse error"),
        ("send", b"/import bad.jsonl\r"), ("wait", 1.2),
        ("send", b"y"), ("wait", 3.0), ("snap", "import_bad_yes"),
        ("log", "/import good-session.jsonl -> confirm Yes -> imported"),
        ("send", b"/import good-session.jsonl\r"), ("wait", 1.2),
        ("snap", "import_confirm_good"),
        ("send", b"y"), ("wait", 6.0), ("snap", "import_good"),
    ]
    out = run_tui("share_import", ops, agent, project, timeout=120)
    checks = {}
    share_text = add_screen_checks(checks, "share", [])
    low = share_text.lower()
    checks["share:status-notice"] = {
        "ok": ("sharing the session" in low or "creating a secret gist" in low
               or "uploading the session" in low)}
    checks["share:graceful-error"] = {
        "ok": any(k in low for k in ("github credentials not found", "failed to create gist",
                                     "failed to upload", "error", "failed")),
        "screen_excerpt": "\n".join([l for l in share_text.splitlines()
                                     if "credential" in l.lower() or "failed" in l.lower()
                                     or "error" in l.lower() or "gist" in l.lower()])[:1000]}
    checks["share:no-panic"] = {"ok": "panic" not in low and "thread '" not in low}
    add_screen_checks(checks, "import_usage", [("Usage: /import <path.jsonl>", "present")])
    add_screen_checks(checks, "import_missing", [
        ("Failed to import session: file not found or not a file: missing-import.jsonl", "present"),
        ("Replace current session", "absent")])
    add_screen_checks(checks, "import_confirm_bad", [
        ("Replace current session with bad.jsonl?", "present")])
    add_screen_checks(checks, "import_cancel", [("Import cancelled", "present")])
    add_screen_checks(checks, "import_bad_yes", [("Failed to import session", "present")])
    add_screen_checks(checks, "import_confirm_good", [
        ("Replace current session with good-session.jsonl?", "present")])
    add_screen_checks(checks, "import_good", [
        ("Session imported from: good-session.jsonl", "present"),
        ("R55-CLIP", "present")])
    result["legs"]["share_import"] = {
        "exit_code": out.get("exit_code"), "timed_out": out.get("timed_out"),
        "elapsed_s": out.get("elapsed_s"), "checks": checks}

    acc3_render.render_dir(SHOTS)
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)

    failed = []
    for leg_name, leg in result["legs"].items():
        for name, check in leg["checks"].items():
            if not check["ok"]:
                failed.append(f"{leg_name}:{name}")
        print(f"{leg_name}: exit={leg['exit_code']} timed_out={leg['timed_out']} "
              f"checks={sum(1 for c in leg['checks'].values() if c['ok'])}/{len(leg['checks'])}")
    print("FAILED:", failed or "none")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
