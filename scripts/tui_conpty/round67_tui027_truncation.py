# -*- coding: utf-8 -*-
"""Round 67 lane-fix-tui027: PIR-TUI-027 real-ConPTY screen evidence.

What this proves, on the REAL TUI (ConPTY + pyte text dumps, no screenshots):

  leg_under   a session below TRANSCRIPT_WINDOW_MAX_ENTRIES (500 entries):
              no truncation row anywhere, the transcript starts at entry 1.
  leg_over    a session above the cap (900 entries): in fullscreen(follow) the
              materialized band starts at entry 389, and after Ctrl+Home
              (tui.altScreen.top) the band is extended until it starts at
              entry 1 — i.e. the older history is REACHABLE, and the lead row
              reports exactly the dropped count.
  leg_scroll  the same 900-entry session in regular mode: the raw write stream
              (PI_TUI_WRITE_LOG) must NOT contain `ESC[3J` (scrollback erase)
              after boot, and pyte's HISTORY buffer must still hold the
              transcript rows the window released.

Run (from the repo root):
  PIR_TUI_EXE=<binary> python scripts/tui_conpty/round67_tui027_truncation.py
The binary is the only input that differs between the pre-fix and post-fix
runs, so the diff of the produced `result.json` IS the fix's evidence.
"""
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from run_battery import _win_long_path, encode_cwd_dir_name, seed_agent_dir  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.abspath(
    os.environ.get("PIR_TUI027_EVIDENCE")
    or os.path.join(REPO, ".play", "evidence", "ab-round1", "lane-fix-tui027", "conpty")
)
PIR = os.path.abspath(
    os.environ.get("PIR_TUI_EXE")
    or os.path.join(REPO, ".play", "evidence", "ab-round1", "lane-fix-tui027", "bin", "pir.exe")
)

# Regular mode truncates on the ENTRY cap with hysteresis: the window keeps
# its current start until the exact tail drifts a FULL window (512), i.e. a
# session has to exceed 1024 entries before `regular_window_start` moves off 0.
# Fullscreen truncates on the 1 MiB TAIL byte budget, so its leg needs fat
# entries (~4 KiB each) rather than many tiny ones.
UNDER = 500
OVER = 1300
OVER_PAYLOAD = 1200
CTRL_HOME = b"\x1b[1;5H"


def marker(index):
    return f"ENTRYMARK{index:04d}"


def seed_session(agent, cwd, count, payload_bytes=0):
    """Write one v3 session JSONL with `count` user messages under the agent
    dir's encoded-cwd sessions folder, and return its path.

    Each message is a single text line carrying `marker(index)`, so a pyte
    screen dump can prove WHICH entry is on screen (reachability) rather than
    trusting a row count.
    """
    session_id = f"00000000-0000-4000-8000-{count:012d}"
    stamp = "2026-01-01T00:00:00.000Z"
    session_dir = os.path.join(agent, "sessions", encode_cwd_dir_name(cwd))
    os.makedirs(_win_long_path(session_dir), exist_ok=True)
    path = os.path.join(session_dir, f"2026-01-01T00-00-00-000Z_{session_id}.jsonl")
    with open(_win_long_path(path), "w", encoding="utf-8") as f:
        f.write(json.dumps({
            "type": "session", "version": 3, "id": session_id,
            "timestamp": stamp, "cwd": cwd,
        }) + "\n")
        for index in range(count):
            f.write(json.dumps({
                "type": "message",
                "id": f"m{index:05d}",
                "parentId": None if index == 0 else f"m{index - 1:05d}",
                "timestamp": stamp,
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": (
                        f"{marker(index)} payload entry {index}"
                        + (
                            "\n" + ("filler line for the byte budget\n"
                                    * (payload_bytes // 32))
                            if payload_bytes else ""
                        )
                    )}],
                    "timestamp": 0,
                },
            }) + "\n")
    return path


def run_leg(name, count, mode, ops, extra_env=None, payload_bytes=0):
    base = os.path.join(ROOT, name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    session = seed_session(agent, project, count, payload_bytes)
    env = {
        "PIR_TRUST_PROJECT": "1",
        "HOME": home,
        "USERPROFILE": home,
        "PIR_AGENT_DIR": agent,
        "PIR_NO_UPDATE_CHECK": "1",
    }
    env.update(extra_env or {})
    tui_drive.PIR = PIR
    shots = os.path.join(base, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    tui_drive.run_scenario(
        name,
        ops,
        extra_env=env,
        cwd=project,
        args=["--session", session, "--tui-mode", mode],
        shots_dir=shots,
        driver_log=os.path.join(shots, name + ".driver.log"),
        result_json=result_json,
        agent_dir=agent,
        timeout=300,
    )
    with open(result_json, encoding="utf-8") as f:
        return json.load(f), shots


def screen_of(shots, snap):
    """Replay one snapshot through pyte and return the 40x120 text rows."""
    raw = open(os.path.join(shots, snap + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def history_rows(raw, rows, cols=120):
    """The rows that scrolled OUT of the viewport (pyte's history buffer).

    This is the terminal's native scrollback as the user would find it by
    scrolling up: the proof that a repaint did (or did not) erase it.
    """
    import pyte
    screen = pyte.HistoryScreen(cols, rows, history=20000)
    pyte.ByteStream(screen).feed(raw)
    out = []
    for line in screen.history.top:
        out.append("".join(line[x].data for x in range(cols)).rstrip())
    # The live display follows the history in reading order.
    out.extend(l.rstrip() for l in screen.display)
    return out


def find_rows(rows, needle):
    return [i for i, row in enumerate(rows) if needle in row]


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT, exist_ok=True)

    result = {
        "binary": PIR,
        "cap": {"entries": 512, "bytes": 4 * 1024 * 1024},
        "under_entries": UNDER,
        "over_entries": OVER,
        "legs": {},
    }

    # ---- leg_under: 500 entries, fullscreen, follow end -------------------
    under_ops = [("wait_quiet", 6.0), ("wait", 1.5), ("snap", "under_follow")]
    out, shots = run_leg("leg_under", UNDER, "fullscreen", under_ops)
    rows, raw = screen_of(shots, "under_follow")
    hint_rows = find_rows(rows, "earlier entr")
    result["legs"]["leg_under"] = {
        "exit_code": out.get("exit_code"),
        "timed_out": out.get("timed_out"),
        "last_marker_on_screen": max(
            (i for i, r in enumerate(rows) if "ENTRYMARK" in r), default=None
        ),
        "marker_rows": [r.strip() for r in rows if "ENTRYMARK" in r][:3],
        "truncation_rows": hint_rows,
        "has_truncation_row": bool(hint_rows),
        "screen": rows,
    }

    # ---- leg_over: 900 entries, fullscreen, follow then Ctrl+Home ---------
    over_ops = [
        ("wait_quiet", 6.0), ("wait", 1.5), ("snap", "over_follow"),
        ("send", CTRL_HOME), ("wait_quiet", 45.0), ("wait", 3.0),
        ("snap", "over_top"),
    ]
    out, shots = run_leg("leg_over", OVER, "fullscreen", over_ops,
                    payload_bytes=OVER_PAYLOAD)
    follow_rows, follow_raw = screen_of(shots, "over_follow")
    top_rows, top_raw = screen_of(shots, "over_top")
    follow_hint = find_rows(follow_rows, "earlier entr")
    top_hint = find_rows(top_rows, "earlier entr")
    # Which ENTRYMARK the top-of-band view shows: 0000 means the whole history
    # became reachable; a non-zero first mark means the band still starts above.
    top_marks = [r.strip() for r in top_rows if "ENTRYMARK" in r]
    result["legs"]["leg_over"] = {
        "exit_code": out.get("exit_code"),
        "timed_out": out.get("timed_out"),
        "follow": {
            "first_marker_on_screen": (follow_rows and next(
                (r.strip() for r in follow_rows if "ENTRYMARK" in r), None)),
            "truncation_rows": follow_hint,
            "has_truncation_row": bool(follow_hint),
            "hint_text": follow_rows[follow_hint[0]] if follow_hint else None,
        },
        "top": {
            "first_marker_on_screen": top_marks[0] if top_marks else None,
            "entry_0000_visible": any("ENTRYMARK0000" in r for r in top_rows),
            "truncation_rows": top_hint,
            "has_truncation_row": bool(top_hint),
            "hint_text": top_rows[top_hint[0]] if top_hint else None,
        },
        # The exact bytes the renderer wrote, for the 3J audit below.
        "follow_raw_len": len(follow_raw),
        "top_raw_len": len(top_raw),
        "screen": top_rows,
        "follow_screen": follow_rows,
    }

    # ---- leg_scroll: 900 entries, regular mode, scrollback audit ----------
    scroll_log = os.path.join(ROOT, "leg_scroll", "tui_writes.log")
    os.makedirs(os.path.dirname(scroll_log), exist_ok=True)
    scroll_ops = [("wait_quiet", 6.0), ("wait", 1.5), ("snap", "scroll_boot"),
                  ("send", b"/quit\r"), ("wait", 1.0), ("snap", "scroll_quit")]
    out, shots = run_leg("leg_scroll", OVER, "regular", scroll_ops,
                         extra_env={"PI_TUI_WRITE_LOG": scroll_log})
    raw = open(scroll_log, "rb").read() if os.path.isfile(scroll_log) else b""
    boot_rows, boot_raw = screen_of(shots, "scroll_boot")
    history = history_rows(boot_raw, 40)
    result["legs"]["leg_scroll"] = {
        "exit_code": out.get("exit_code"),
        "timed_out": out.get("timed_out"),
        "write_log_bytes": len(raw),
        "full_clear_2j": raw.count(b"\x1b[2J"),
        "scrollback_erase_3j": raw.count(b"\x1b[3J"),
        "boot_truncation_rows": find_rows(boot_rows, "earlier entr"),
        "history_rows_total": len(history),
        "history_has_entrymarks": sum(1 for r in history if "ENTRYMARK" in r),
        "history_live_view_entrymarks": sum(1 for r in boot_rows if "ENTRYMARK" in r),
        "history_sample": [r for r in history if "ENTRYMARK" in r][:3]
        + [r for r in history if "ENTRYMARK" in r][-3:],
    }

    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)

    summary = {
        name: {
            "exit": leg["exit_code"],
            "trunc_row": leg.get("has_truncation_row",
                                 bool(leg.get("boot_truncation_rows"))),
            "3J": leg.get("scrollback_erase_3j"),
        }
        for name, leg in result["legs"].items()
    }
    print(json.dumps(summary, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())