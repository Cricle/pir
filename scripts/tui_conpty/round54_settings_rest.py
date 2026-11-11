# -*- coding: utf-8 -*-
"""Round 54 lane-tui: remaining /settings keys, live vs restart.

Keys: outputPad (live indent), showHardwareCursor (hardware cursor escape),
terminal-progress (OSC 9;4), clear-on-shrink (persist/row), fullscreen-copy-
on-select (live cell + persist). Expected live semantics follow upstream
interactive-mode.ts:5059-5090 (onShowHardwareCursorChange /
onOutputPadChange / onClearOnShrinkChange) and the per-run
getShowTerminalProgress() gate.
"""
import json
import os
import re
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.environ.get("PIR_SETTINGS2_EVIDENCE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round54-settings2")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def rows_of(shot):
    raw = open(os.path.join(ROOT, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def find_row(rows, needle):
    return next((r for r in rows if needle in r), None)


def reply_indent(raw):
    rows = acc3_render.render(raw)
    hits = [len(r) - len(r.lstrip(" ")) for r in rows if r.strip() == "mock reply"]
    return hits[-1] if hits else None


def run_tui(name, ops, agent=None, timeout=200):
    base = os.path.join(ROOT, "runs", name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    if agent is None:
        agent = os.path.join(home, ".pi", "agent")
        os.makedirs(agent, exist_ok=True)
        seed_agent_dir(agent, True)
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=None)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    os.makedirs(shots, exist_ok=True)
    rj = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(name, ops, extra_env=env, cwd=os.path.join(ROOT, "project"),
                               shots_dir=shots, driver_log=os.path.join(shots, name + ".driver.log"),
                               result_json=rj, agent_dir=agent, timeout=timeout)
    finally:
        mock.stop()
    out = json.load(open(rj, encoding="utf-8"))
    return {"agent": agent, "exit_code": out.get("exit_code"),
            "timed_out": out.get("timed_out"), "elapsed_s": out.get("elapsed_s")}


def close_menu():
    return [("send", b"\x1b"), ("wait", 0.5), ("send", b"\x1b"), ("wait", 0.8)]


def setting(term, snap):
    return [("send", b"/settings\r"), ("wait", 1.1),
            ("send", term.encode()), ("wait", 0.7),
            ("send", b"\r"), ("wait", 0.9), ("snap", snap)] + close_menu()


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)
    with open(os.path.join(ROOT, "project", "note.txt"), "w", encoding="utf-8") as f:
        f.write("round54 settings rest\n")

    ops1 = [("wait_quiet", 5.0), ("snap", "s1_boot"),
            ("send", b"first message\r"), ("wait", 5.0), ("wait_quiet", 1.5),
            ("snap", "s1_reply_boot")]
    ops1 += [("log", "outputPad 1 -> 0")] + setting("Output padding", "s1_outpad_saved")
    ops1 += [("send", b"second message\r"), ("wait", 5.0), ("wait_quiet", 1.5),
             ("snap", "s1_reply_after")]
    ops1 += [("log", "showHardwareCursor false -> true")] + setting("Show hardware", "s1_hwc_saved") + [("snap", "s1_hwc_after")]
    ops1 += [("log", "terminal-progress false -> true")] + setting("Terminal progress", "s1_progress_saved")
    ops1 += [("send", b"third message\r"), ("wait", 5.0), ("wait_quiet", 1.5), ("snap", "s1_progress_run")]
    ops1 += [("log", "clear-on-shrink false -> true")] + setting("Clear on shrink", "s1_cos_saved")
    ops1 += [("log", "fullscreen-copy-on-select true -> false")] + setting("Fullscreen copy on select", "s1_copy_saved")
    s1 = run_tui("s1_rest", ops1)
    leg1 = {"exit_code": s1["exit_code"], "timed_out": s1["timed_out"],
            "elapsed_s": s1["elapsed_s"], "checks": {}, "rows": {}}
    def C(name, ok, **kw):
        leg1["checks"][name] = {"ok": bool(ok), **kw}

    _, raw_boot = rows_of("s1_boot")
    _, raw_reply_boot = rows_of("s1_reply_boot")
    leg1["reply_boot_indent"] = reply_indent(raw_reply_boot)
    rows = rows_of("s1_outpad_saved")[0]
    leg1["outpad_notice"] = find_row(rows, "Saved setting: outputPad")
    C("outputPad:saved", leg1["outpad_notice"] is not None, row=leg1["outpad_notice"])
    _, raw_after = rows_of("s1_reply_after")
    leg1["reply_after_indent"] = reply_indent(raw_after)
    C("outputPad:live-indent", leg1["reply_after_indent"] == 0,
      boot=leg1["reply_boot_indent"], after=leg1["reply_after_indent"])
    rows = rows_of("s1_hwc_saved")[0]
    leg1["hwc_notice"] = find_row(rows, "Saved setting: showHardwareCursor")
    C("showHardwareCursor:saved", leg1["hwc_notice"] is not None, row=leg1["hwc_notice"])
    _, raw_hwc = rows_of("s1_hwc_after")
    leg1["hwc_boot"] = raw_boot.count(b"\x1b[?25h")
    leg1["hwc_after"] = raw_hwc.count(b"\x1b[?25h")
    C("showHardwareCursor:live-visible", leg1["hwc_after"] > leg1["hwc_boot"],
      boot=leg1["hwc_boot"], after=leg1["hwc_after"])
    rows = rows_of("s1_progress_saved")[0]
    leg1["progress_notice"] = find_row(rows, "Saved setting: terminal.showTerminalProgress")
    C("terminal-progress:saved", leg1["progress_notice"] is not None, row=leg1["progress_notice"])
    _, raw_prog = rows_of("s1_progress_run")
    leg1["osc_live"] = raw_prog.count(b"\x1b]9;4;3;0\x1b\\")
    C("terminal-progress:live-osc", leg1["osc_live"] > 0, count=leg1["osc_live"])
    rows = rows_of("s1_cos_saved")[0]
    leg1["cos_notice"] = find_row(rows, "Saved setting: terminal.clearOnShrink")
    C("clear-on-shrink:saved", leg1["cos_notice"] is not None, row=leg1["cos_notice"])
    rows = rows_of("s1_copy_saved")[0]
    leg1["copy_notice"] = find_row(rows, "Saved setting: fullscreenCopyOnSelect")
    C("fullscreen-copy-on-select:saved", leg1["copy_notice"] is not None, row=leg1["copy_notice"])

    saved = json.load(open(os.path.join(s1["agent"], "settings.json"), encoding="utf-8"))
    leg1["settings"] = saved
    for key, ok in [
        ("outputPad", saved.get("outputPad") == 0),
        ("showHardwareCursor", saved.get("showHardwareCursor") is True),
        ("terminal.showTerminalProgress", saved.get("terminal", {}).get("showTerminalProgress") is True),
        ("terminal.clearOnShrink", saved.get("terminal", {}).get("clearOnShrink") is True),
        ("fullscreenCopyOnSelect", saved.get("fullscreenCopyOnSelect") is False),
    ]:
        C("settings.json:" + key, ok)

    # ---- restart ----
    ops2 = [("wait_quiet", 5.0), ("snap", "s2_boot")]
    for term, name in [("Output padding", "s2_outpad"), ("Show hardware", "s2_hwc"),
                       ("Terminal progress", "s2_progress"), ("Clear on shrink", "s2_cos"),
                       ("Fullscreen copy on select", "s2_copy")]:
        ops2 += [("send", b"/settings\r"), ("wait", 1.1), ("send", term.encode()),
                 ("wait", 0.7), ("snap", name)] + close_menu()
    ops2 += [("send", b"restart message\r"), ("wait", 5.0), ("wait_quiet", 1.5),
             ("snap", "s2_reply")]
    s2 = run_tui("s2_restart", ops2, agent=s1["agent"])
    leg2 = {"exit_code": s2["exit_code"], "timed_out": s2["timed_out"],
            "elapsed_s": s2["elapsed_s"], "checks": {}, "rows": {}}
    def C2(name, ok, **kw):
        leg2["checks"][name] = {"ok": bool(ok), **kw}

    for name, needle, want in [
            ("outpad", "Output padding:", "0"),
            ("hwc", "Show hardware cursor:", "true"),
            ("progress", "Terminal progress:", "true"),
            ("cos", "Clear on shrink:", "true"),
            ("copy", "Fullscreen copy on select:", "false")]:
        row = find_row(rows_of("s2_" + name)[0], needle)
        leg2["rows"][name] = row
        C2("restart:row:" + needle.strip(":"), row is not None and want.lower() in (row or "").lower(), row=row)
    _, raw2 = rows_of("s2_boot")
    _, raw2_reply = rows_of("s2_reply")
    leg2["reply_restart_indent"] = reply_indent(raw2_reply)
    C2("restart:outputPad-effective", leg2["reply_restart_indent"] == 0,
       indent=leg2["reply_restart_indent"])
    leg2["hwc_boot"] = raw2.count(b"\x1b[?25h")
    C2("restart:showHardwareCursor-effective", leg2["hwc_boot"] > 0, count=leg2["hwc_boot"])
    leg2["osc_restart"] = raw2_reply.count(b"\x1b]9;4;3;0\x1b\\")
    C2("restart:terminal-progress-effective", leg2["osc_restart"] > 0, count=leg2["osc_restart"])

    acc3_render.render_dir(os.path.join(ROOT, "shots"))
    json.dump({"binary": PIR, "legs": {"s1_rest": leg1, "s2_restart": leg2}},
              open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8"),
              indent=2, ensure_ascii=False)

    def summary(leg):
        return {"exit": leg["exit_code"], "timed_out": leg["timed_out"],
                "ok": sum(1 for c in leg["checks"].values() if c["ok"]),
                "total": len(leg["checks"]),
                "failed": [k for k, c in leg["checks"].items() if not c["ok"]]}
    print(json.dumps({"s1": summary(leg1), "s2": summary(leg2)}, indent=2, ensure_ascii=False))
    return 1 if any(not c["ok"] for leg in (leg1, leg2) for c in leg["checks"].values()) else 0


if __name__ == "__main__":
    raise SystemExit(main())
