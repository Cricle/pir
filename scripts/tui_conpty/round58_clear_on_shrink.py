# -*- coding: utf-8 -*-
"""Round 58 lane-tui: real-ConPTY regression for PIR-TUI-018
(`terminal.clearOnShrink` startup + live).

The renderer now consumes the setting: with it on, a frame that shrinks below
the working-area high-water mark takes the full repaint path
(`ESC[2J ESC[H ESC[3J`); with it off (default) the diff path runs and only the
previous frame's own tail rows are cleared. The test drives a real tool block:
Ctrl+O expands a 30-line `seq` output (frame grows), Ctrl+O again collapses it
(frame shrinks) — the minimal natural shrink in this TUI.

Legs:
  s1_false        default settings: shrink must NOT emit 2J/3J; diff tail clear
  s2_true_startup settings.json terminal.clearOnShrink=true before boot: shrink
                  must emit 2J/3J (startup read)
  s3_live         start false, `/settings` toggles Clear on shrink on, then the
                  same shrink must emit 2J/3J after the save notice (live read)

Raw terminal bytes come from PI_TUI_WRITE_LOG (the app's own write log), so the
assertions are about what the renderer actually wrote, not a screenshot proxy.
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
ROOT = os.path.abspath(os.environ.get("PIR_CLEAR58_EVIDENCE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round58-pir018"))
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(
    REPO, "target-gnu", "debug", "pir.exe"))
FULL_CLEAR = b"\x1b[2J\x1b[H\x1b[3J"
CTRL_O = b"\x0f"


def run_tui(name, ops, agent=None, write_log=None, timeout=240):
    base = os.path.join(ROOT, "runs", name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    if agent is None:
        agent = os.path.join(home, ".pi", "agent")
        os.makedirs(agent, exist_ok=True)
        seed_agent_dir(agent, True)
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=[
        {"tool_calls": [{"name": "bash", "arguments": {"command": "seq 1 30"}}]},
        {"text": "R58-SHRINK-DONE"},
    ])
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    if write_log:
        env["PI_TUI_WRITE_LOG"] = write_log
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    os.makedirs(shots, exist_ok=True)
    rj = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(name, ops, extra_env=env,
                               args=["--tui-mode", "regular"],
                               cwd=os.path.join(ROOT, "project"),
                               shots_dir=shots,
                               driver_log=os.path.join(shots, name + ".driver.log"),
                               result_json=rj, agent_dir=agent, timeout=timeout)
    finally:
        mock.stop()
    out = json.load(open(rj, encoding="utf-8"))
    return {"agent": agent, "exit_code": out.get("exit_code"),
            "timed_out": out.get("timed_out"), "elapsed_s": out.get("elapsed_s")}


def shrink_ops(prefix):
    """Prompt → wait → expand → collapse, snapshots per step."""
    return [
        ("wait_quiet", 5.0), ("snap", prefix + "_boot"),
        ("send", b"run the seq command\r"),
        ("wait_quiet", 4.0), ("wait", 1.0), ("snap", prefix + "_reply"),
        ("send", CTRL_O), ("wait", 1.0), ("snap", prefix + "_expanded"),
        ("send", CTRL_O), ("wait", 1.0), ("snap", prefix + "_collapsed"),
    ]


def read_bytes(path):
    try:
        with open(path, "rb") as f:
            return f.read()
    except FileNotFoundError:
        return b""


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)
    with open(os.path.join(ROOT, "project", "note.txt"), "w", encoding="utf-8") as f:
        f.write("round58 clearOnShrink live\n")

    legs = {}
    checks = {}

    def check(leg, name, ok, **kw):
        checks.setdefault(leg, {})[name] = {"ok": bool(ok), **kw}

    # s1: default false. Write log lives inside the run dir (not the agent).
    s1_dir = os.path.join(ROOT, "runs", "s1_false")
    os.makedirs(s1_dir, exist_ok=True)
    s1_log = os.path.join(s1_dir, "tui_writes.log")
    s1 = run_tui("s1_false", shrink_ops("s1"), write_log=s1_log)
    raw = read_bytes(s1_log)
    reply_at = raw.find(b"R58-SHRINK-DONE")
    shrink_raw = raw[reply_at:] if reply_at >= 0 else b""
    legs["s1_false"] = {**s1, "write_log": s1_log, "bytes": len(raw),
                        "reply_marker_at": reply_at,
                        "full_clears_after_reply": shrink_raw.count(FULL_CLEAR),
                        "tail_clears_after_reply": shrink_raw.count(b"\r\n\x1b[2K")}
    check("s1_false", "exit-clean", s1["exit_code"] == 0 and not s1["timed_out"],
          exit_code=s1["exit_code"], timed_out=s1["timed_out"])
    check("s1_false", "reply-seen", reply_at >= 0)
    check("s1_false", "default-no-full-clear", shrink_raw.count(FULL_CLEAR) == 0,
          full_clears=shrink_raw.count(FULL_CLEAR))
    check("s1_false", "shrink-clears-via-diff", shrink_raw.count(b"\r\n\x1b[2K") > 0,
          tail_clears=shrink_raw.count(b"\r\n\x1b[2K"))

    # s2: startup true — settings.json written before the first frame.
    s2_agent = os.path.join(ROOT, "runs", "s2_true_startup", "agent")
    os.makedirs(s2_agent, exist_ok=True)
    seed_agent_dir(s2_agent, True)
    with open(os.path.join(s2_agent, "settings.json"), "w", encoding="utf-8") as f:
        json.dump({"terminal": {"clearOnShrink": True}}, f)
    s2_log = os.path.join(ROOT, "runs", "s2_true_startup", "tui_writes.log")
    os.makedirs(os.path.dirname(s2_log), exist_ok=True)
    s2 = run_tui("s2_true_startup", shrink_ops("s2"), agent=s2_agent,
                 write_log=s2_log)
    raw2 = read_bytes(s2_log)
    reply_at2 = raw2.find(b"R58-SHRINK-DONE")
    shrink_raw2 = raw2[reply_at2:] if reply_at2 >= 0 else b""
    legs["s2_true_startup"] = {**s2, "write_log": s2_log, "bytes": len(raw2),
                               "reply_marker_at": reply_at2,
                               "full_clears_after_reply": shrink_raw2.count(FULL_CLEAR)}
    check("s2_true_startup", "exit-clean",
          s2["exit_code"] == 0 and not s2["timed_out"],
          exit_code=s2["exit_code"], timed_out=s2["timed_out"])
    check("s2_true_startup", "reply-seen", reply_at2 >= 0)
    check("s2_true_startup", "startup-full-clear-on-shrink",
          shrink_raw2.count(FULL_CLEAR) >= 1,
          full_clears=shrink_raw2.count(FULL_CLEAR))

    # s3: live toggle false → true in one session.
    s3_log = os.path.join(ROOT, "runs", "s3_live", "tui_writes.log")
    os.makedirs(os.path.dirname(s3_log), exist_ok=True)
    ops3 = shrink_ops("s3_pre") + [
        ("send", b"/settings\r"), ("wait", 1.1),
        ("send", b"Clear on shrink"), ("wait", 0.7),
        ("send", b"\r"), ("wait", 0.9), ("snap", "s3_saved"),
        ("send", b"\x1b"), ("wait", 0.5), ("send", b"\x1b"), ("wait", 0.8),
        ("send", CTRL_O), ("wait", 1.0), ("snap", "s3_expand2"),
        ("send", CTRL_O), ("wait", 1.0), ("snap", "s3_collapse2"),
    ]
    s3 = run_tui("s3_live", ops3, write_log=s3_log)
    raw3 = read_bytes(s3_log)
    marker = raw3.find(b"Saved setting: terminal.clearOnShrink")
    after = raw3[marker:] if marker >= 0 else b""
    legs["s3_live"] = {**s3, "write_log": s3_log, "bytes": len(raw3),
                       "saved_marker_at": marker,
                       "full_clears_after_save": after.count(FULL_CLEAR)}
    check("s3_live", "exit-clean", s3["exit_code"] == 0 and not s3["timed_out"],
          exit_code=s3["exit_code"], timed_out=s3["timed_out"])
    check("s3_live", "saved", marker >= 0)
    check("s3_live", "live-full-clear-on-shrink", after.count(FULL_CLEAR) >= 1,
          full_clears_after_save=after.count(FULL_CLEAR))

    result = {"binary": PIR, "full_clear_bytes": FULL_CLEAR.decode("latin-1"),
              "legs": legs, "checks": checks}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(os.path.join(ROOT, "shots"))

    summary = {}
    for leg, items in checks.items():
        summary[leg] = {"ok": sum(1 for c in items.values() if c["ok"]),
                        "total": len(items),
                        "failed": [k for k, c in items.items() if not c["ok"]]}
    print(json.dumps(summary, indent=2, ensure_ascii=False))
    return 0 if all(not s["failed"] for s in summary.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
