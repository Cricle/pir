# -*- coding: utf-8 -*-
"""Round 57 lane-tui: settings remaining-key sampling + live behaviour."""
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
ROOT = os.environ.get("PIR_SETTINGS57_EVIDENCE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round57-settings")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round56-pir016-017", "pir56.exe"))


def shot_path(name):
    return os.path.join(ROOT, "shots", name + ".log")


def rows_of(shot):
    raw = open(shot_path(shot), "rb").read()
    return acc3_render.render(raw), raw


def find_row(rows, needle):
    return next((r for r in rows if needle in r), None)


def run_tui(name, ops, agent=None, timeout=240):
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
        tui_drive.run_scenario(name, ops, extra_env=env,
                               cwd=os.path.join(ROOT, "project"),
                               shots_dir=shots,
                               driver_log=os.path.join(shots, name + ".driver.log"),
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


def clip_read(name):
    path = os.path.join(ROOT, "shots", name + ".clip.txt")
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def drag_ops(row_1b, x1=2, x2=28):
    return [("send", b"\x1b[<0;%d;%dM" % (x1, row_1b)), ("wait", 0.15),
            ("send", b"\x1b[<32;%d;%dM" % (x2, row_1b)), ("wait", 0.15),
            ("send", b"\x1b[<0;%d;%dm" % (x2, row_1b)), ("wait", 0.9)]


PROMPT = ("send", b"round57 copy select\r")
WAIT_REPLY = [("wait", 5.0), ("wait_quiet", 1.5), ("snap", "s1_reply")]


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)
    with open(os.path.join(ROOT, "project", "note.txt"), "w", encoding="utf-8") as f:
        f.write("round57 settings live\n")

    # Probe leg: measure the reply row on a real 120x40 fullscreen frame.
    ops1 = [("wait_quiet", 5.0), ("snap", "s1_boot"), PROMPT] + WAIT_REPLY
    s1 = run_tui("s1_probe", ops1)
    rows = rows_of("s1_reply")[0]
    row = find_row(rows, "mock reply")
    if row is None:
        raise SystemExit("no 'mock reply' row on screen")
    y = rows.index(row) + 1

    # Real leg: same process paints the reply, then drag-selects it.
    ops2 = [("wait_quiet", 5.0), PROMPT] + WAIT_REPLY + [
            ("clipset", "PIR57-SENTINEL-ON"),
            ("log", "drag with copyOnSelect=true (default)")] + drag_ops(y) + [
            ("clipdump", "clip_on"), ("snap", "s1_copy_on"),
            ("log", "fullscreenCopyOnSelect true -> false")] + setting(
                "Fullscreen copy on select", "s1_copy_off_saved") + [
            ("clipset", "PIR57-SENTINEL-OFF"),
            ("log", "drag with copyOnSelect=false")] + drag_ops(y) + [
            ("clipdump", "clip_off"), ("snap", "s1_copy_off_drag"),
            ("log", "Ctrl+X copies the active selection"),
            ("send", b"\x18"), ("wait", 0.8), ("clipdump", "clip_ctrlx"),
            ("snap", "s1_ctrlx")]
    ops2 += [("log", "fullscreenExitOutput transcript -> resume-hint")] + setting(
        "Fullscreen exit output", "s1_exit_output")
    ops2 += [("log", "defaultProjectTrust ask -> always")] + setting(
        "Default project trust", "s1_trust")
    ops2 += [("log", "quietStartup false -> true")] + setting(
        "Quiet startup", "s1_quiet")
    ops2 += [("log", "terminal.clearOnShrink false -> true")] + setting(
        "Clear on shrink", "s1_clear_on_shrink")
    s2 = run_tui("s1_live", ops2, agent=s1["agent"])

    leg1 = {"exit_code": s2["exit_code"], "timed_out": s2["timed_out"],
            "elapsed_s": s2["elapsed_s"], "checks": {}, "rows": {}}

    def C(name, ok, **kw):
        leg1["checks"][name] = {"ok": bool(ok), **kw}

    clip_on = clip_read("clip_on")
    clip_off = clip_read("clip_off")
    clip_ctrlx = clip_read("clip_ctrlx")
    rows_ctrlx, _ = rows_of("s1_ctrlx")
    leg1["clipboard"] = {"on": clip_on, "off": clip_off, "ctrlx": clip_ctrlx}
    C("copy-on-select:default-on-copies-on-drag",
      "mock reply" in clip_on and "PIR57-SENTINEL-ON" not in clip_on,
      clipboard=clip_on[:120])
    C("copy-on-select:live-off-does-not-copy",
      clip_off.strip() == "PIR57-SENTINEL-OFF",
      clipboard=clip_off[:120])
    C("copy-on-select:live-off-ctrl-x-copies-selection",
      "mock reply" in clip_ctrlx,
      notice=find_row(rows_ctrlx, "Copied the selection to the clipboard"),
      clipboard=clip_ctrlx[:120])
    C("copy-on-select:ctrl-x-notice",
      find_row(rows_ctrlx, "Copied the selection to the clipboard") is not None)
    C("copy-on-select:saved", find_row(rows_of("s1_copy_off_saved")[0],
      "Saved setting: fullscreenCopyOnSelect") is not None)

    for key, snap, needle in [
            ("fullscreenExitOutput", "s1_exit_output", "Saved setting: fullscreenExitOutput"),
            ("defaultProjectTrust", "s1_trust", "Saved setting: defaultProjectTrust"),
            ("quietStartup", "s1_quiet", "Saved setting: quietStartup"),
            ("terminal.clearOnShrink", "s1_clear_on_shrink",
             "Saved setting: terminal.clearOnShrink")]:
        leg1["rows"][key] = find_row(rows_of(snap)[0], needle)
        C("saved:" + key, leg1["rows"][key] is not None, row=leg1["rows"][key])

    saved = json.load(open(os.path.join(s1["agent"], "settings.json"), encoding="utf-8"))
    leg1["settings"] = saved
    C("json:fullscreenCopyOnSelect", saved.get("fullscreenCopyOnSelect") is False)
    C("json:fullscreenExitOutput", saved.get("fullscreenExitOutput") == "resume-hint")
    C("json:defaultProjectTrust", saved.get("defaultProjectTrust") == "always")
    C("json:quietStartup", saved.get("quietStartup") is True)
    C("json:terminal.clearOnShrink",
      saved.get("terminal", {}).get("clearOnShrink") is True)

    ops3 = [("wait_quiet", 5.0), ("snap", "s3_boot")]
    for term, snap in [("Fullscreen exit output", "s3_exit_output"),
                       ("Default project trust", "s3_trust"),
                       ("Quiet startup", "s3_quiet"),
                       ("Clear on shrink", "s3_clear_on_shrink")]:
        ops3 += [("send", b"/settings\r"), ("wait", 1.1),
                 ("send", term.encode()), ("wait", 0.7), ("snap", snap)] + close_menu()
    s3 = run_tui("s3_restart", ops3, agent=s1["agent"])

    leg2 = {"exit_code": s3["exit_code"], "timed_out": s3["timed_out"],
            "elapsed_s": s3["elapsed_s"], "checks": {}, "rows": {}}

    def C2(name, ok, **kw):
        leg2["checks"][name] = {"ok": bool(ok), **kw}

    for key, snap, needle, want in [
            ("fullscreenExitOutput", "s3_exit_output", "Fullscreen exit output:", "resume-hint"),
            ("defaultProjectTrust", "s3_trust", "Default project trust:", "always"),
            ("quietStartup", "s3_quiet", "Quiet startup:", "true"),
            ("terminal.clearOnShrink", "s3_clear_on_shrink", "Clear on shrink:", "true")]:
        row = find_row(rows_of(snap)[0], needle)
        leg2["rows"][key] = row
        C2("restart:row:" + key, row is not None and want in (row or "").lower(),
           row=row)

    acc3_render.render_dir(os.path.join(ROOT, "shots"))
    json.dump({"binary": PIR, "sha256_note": "pir56 662eea64",
               "legs": {"s1_probe": {"exit_code": s1["exit_code"]},
                        "s1_live": leg1, "s3_restart": leg2}},
              open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8"),
              indent=2, ensure_ascii=False)

    def summary(leg):
        return {"exit": leg["exit_code"], "timed_out": leg["timed_out"],
                "ok": sum(1 for c in leg["checks"].values() if c["ok"]),
                "total": len(leg["checks"]),
                "failed": [k for k, c in leg["checks"].items() if not c["ok"]]}

    print(json.dumps({"s1_live": summary(leg1), "s3_restart": summary(leg2)},
                     indent=2, ensure_ascii=False))
    bad = [c for leg in (leg1, leg2) for c in leg["checks"].values() if not c["ok"]]
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
