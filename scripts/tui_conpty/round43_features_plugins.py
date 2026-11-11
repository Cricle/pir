# -*- coding: utf-8 -*-
"""Round 43 lane-tui: /settings real-usability pass + todo/memory/subagent
builtin TUI surface (commands, panels, restart recovery, error paths).

Fixed snapshot pir36.exe (sha256 76f2eb67...). Two areas, four real TUI legs:
  settings1 : /settings changes theme, terminal.showImages (PI_IMAGE_PROTOCOL
              force-on so the row exists), warnings.anthropicExtraUsage,
              editorPaddingX and tuiMode; live effects checked in the same run.
  settings2 : fresh TUI on the same agent dir; values/effects must persist.
  plugins1  : one prompt drives todo create/update-error, memory remember,
              subagent helper, then /todos, /memory status, /memory brief and
              an invalid /memory action.
  plugins2  : --continue on the same agent dir; todo panel, memory store and
              subagent transcript must come back.
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
FEAT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round43-features")
PLUG = os.path.join(REPO, ".play", "evidence", "lane-tui", "round43-plugins")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
IMG = os.path.join(FEAT, "img43.png")


def rows_of(root, shot):
    raw = open(os.path.join(root, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def rows_with(rows, needle):
    return [(i, r) for i, r in enumerate(rows) if needle in r]


def run_tui(name, root, ops, cwd, script, extra_env=None, args=None, agent=None):
    base = os.path.join(root, "runs", name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    if agent is None:
        agent = os.path.join(home, ".pi", "agent")
        os.makedirs(agent, exist_ok=True)
        seed_agent_dir(agent, True)
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    if extra_env:
        env.update(extra_env)
    tui_drive.PIR = PIR
    shots = os.path.join(root, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=cwd, shots_dir=shots,
            driver_log=os.path.join(shots, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, args=args, timeout=160)
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


def find_row(rows, needle):
    hits = rows_with(rows, needle)
    return hits[0][1] if hits else None


def main():
    for root in (FEAT, PLUG):
        shutil.rmtree(root, ignore_errors=True)
        os.makedirs(os.path.join(root, "shots"), exist_ok=True)
        os.makedirs(os.path.join(root, "runs"), exist_ok=True)
    os.makedirs(FEAT, exist_ok=True)
    # 4x4 PNG reused from round39's generator (already on disk, local only).
    src_img = os.path.join(REPO, ".play", "evidence", "lane-tui", "round39-paste-theme", "img39.png")
    shutil.copyfile(src_img, IMG)
    project = os.path.join(FEAT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round43 settings\n")
    image_arg = IMG.replace("\\", "/")

    # ---------------- Settings leg 1: change five rows, live effects ----------
    ops1 = [
        ("wait_quiet", 5.0), ("snap", "s1_boot"),
        ("log", "open /settings and switch theme"),
        ("send", b"/settings\r"), ("wait", 1.2), ("snap", "s1_menu"),
        ("send", b"theme"), ("wait", 0.8), ("snap", "s1_theme_filter"),
        ("send", b"\r"), ("wait", 0.8), ("snap", "s1_theme_sub"),
        ("send", b"\x1b[B"), ("wait", 0.4), ("send", b"\r"),
        ("wait", 1.2), ("snap", "s1_theme_applied"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.8),
        ("log", "Show images true -> false, then /image must fall back"),
        ("send", b"/settings\r"), ("wait", 1.0), ("send", b"Show images"), ("wait", 0.8),
        ("snap", "s1_images_row"), ("send", b"\r"), ("wait", 1.2), ("snap", "s1_images_off"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.8),
        ("send", ("/image " + image_arg).encode("utf-8")), ("wait", 0.4),
        ("send", b"\r"), ("wait", 1.5), ("snap", "s1_image_fallback"),
        ("log", "Warnings submenu toggle"),
        ("send", b"/settings\r"), ("wait", 1.0), ("send", b"Warnings"), ("wait", 0.8),
        ("send", b"\r"), ("wait", 0.8), ("snap", "s1_warn_sub"),
        ("send", b"\r"), ("wait", 1.2), ("snap", "s1_warn_off"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.8),
        ("log", "Editor padding 0 -> 1"),
        ("send", b"/settings\r"), ("wait", 1.0), ("send", b"Editor padding"), ("wait", 0.8),
        ("send", b"\r"), ("wait", 1.2), ("snap", "s1_padding"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.8),
        ("log", "TUI mode fullscreen -> regular (live alt-screen leave)"),
        ("send", b"/settings\r"), ("wait", 1.0), ("send", b"TUI mode"), ("wait", 0.8),
        ("snap", "s1_tuimode_row"), ("send", b"\r"), ("wait", 1.5), ("snap", "s1_tui_regular"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 1.0),
        ("log", "a normal prompt still works after the mode switch"),
        ("send", b"R43-SETTINGS-OK\r"), ("wait", 5.0), ("wait_quiet", 2.0),
        ("snap", "s1_after_prompt"),
    ]
    l1 = run_tui("s1_settings", FEAT, ops1, project, [{"text": "R43-SETTINGS-OK"}],
                 extra_env={"PI_IMAGE_PROTOCOL": "kitty"})
    leg1 = {"exit_code": l1["exit_code"], "timed_out": l1["timed_out"],
            "elapsed_s": l1["elapsed_s"], "checks": {}, "rows": {}, "raw_counts": {}}
    check_screen(leg1, FEAT, "s1_menu", [("Settings", "present")])
    check_screen(leg1, FEAT, "s1_theme_filter", [("Theme", "present")])
    rows = check_screen(leg1, FEAT, "s1_theme_applied", [("Theme:", "present")])
    leg1["theme_notice"] = find_row(rows, "Theme:")
    check_screen(leg1, FEAT, "s1_images_row", [("Show images", "present")])
    check_screen(leg1, FEAT, "s1_images_off", [("Saved setting: terminal.showImages", "present")])
    check_screen(leg1, FEAT, "s1_image_fallback", [("[Image:", "present")])
    check_screen(leg1, FEAT, "s1_warn_off",
                 [("Saved setting: warnings.anthropicExtraUsage", "present")])
    check_screen(leg1, FEAT, "s1_padding", [("Saved setting: editorPaddingX", "present")])
    check_screen(leg1, FEAT, "s1_tuimode_row", [("TUI mode", "present")])
    check_screen(leg1, FEAT, "s1_tui_regular", [("Saved setting: tuiMode", "present")])
    check_screen(leg1, FEAT, "s1_after_prompt", [("R43-SETTINGS-OK", "present")])
    _, raw_regular = rows_of(FEAT, "s1_tui_regular")
    _, raw_boot = rows_of(FEAT, "s1_boot")
    leg1["raw_counts"]["s1_boot_1049h"] = raw_boot.count(b"\x1b[?1049h")
    leg1["raw_counts"]["s1_regular_1049l"] = raw_regular.count(b"\x1b[?1049l")
    before_row = find_row(rows_of(FEAT, "s1_tuimode_row")[0], "TUI mode:")
    after_row = find_row(rows_of(FEAT, "s1_tui_regular")[0], "TUI mode:")
    leg1["tuimode_before"] = before_row
    leg1["tuimode_after"] = after_row
    leg1["checks"]["tui-mode:live-effective-row"] = {
        "ok": bool(before_row and after_row and before_row != after_row
                   and "fullscreen" in after_row.lower()),
        "before": before_row, "after": after_row}
    settings_path = os.path.join(l1["agent"], "settings.json")
    with open(settings_path, encoding="utf-8") as f:
        saved = json.load(f)
    leg1["settings_path"] = settings_path
    leg1["settings_after"] = saved
    expect = [("theme", isinstance(saved.get("theme"), str) and saved.get("theme") != "dark"),
              ("terminal.showImages", saved.get("terminal", {}).get("showImages") is False),
              ("warnings.anthropicExtraUsage",
               saved.get("warnings", {}).get("anthropicExtraUsage") is False),
              ("editorPaddingX", saved.get("editorPaddingX") == 1),
              ("tuiMode", saved.get("tuiMode") == "fullscreen")]
    leg1["settings_expect"] = dict(expect)
    for key, ok in expect:
        leg1["checks"][f"settings.json:{key}"] = {"ok": bool(ok)}
    # remember the theme name actually saved (for the restart leg row)
    leg1["theme_saved"] = saved.get("theme")

    # ---------------- Settings leg 2: restart persistence ---------------------
    ops2 = [
        ("wait_quiet", 5.0), ("snap", "s2_boot"),
        ("send", b"/settings\r"), ("wait", 1.0), ("send", b"Show images"), ("wait", 0.8),
        ("snap", "s2_showimages_row"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.6),
        ("send", ("/image " + image_arg).encode("utf-8")), ("wait", 0.4),
        ("send", b"\r"), ("wait", 1.5), ("snap", "s2_image_fallback"),
        ("send", b"/settings\r"), ("wait", 1.0), ("send", b"TUI mode"), ("wait", 0.8),
        ("snap", "s2_tuimode_row"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.6),
    ]
    l2 = run_tui("s2_restart", FEAT, ops2, project, [{"text": "R43-RESTART-OK"}],
                 extra_env={"PI_IMAGE_PROTOCOL": "kitty"}, agent=l1["agent"])
    leg2 = {"exit_code": l2["exit_code"], "timed_out": l2["timed_out"],
            "elapsed_s": l2["elapsed_s"], "checks": {}, "rows": {}, "raw_counts": {}}
    _, raw2 = rows_of(FEAT, "s2_boot")
    leg2["raw_counts"]["s2_boot_1049h"] = raw2.count(b"\x1b[?1049h")
    leg2["checks"]["restart:tuiMode-regular-no-altscreen-enter"] = {
        "ok": leg2["raw_counts"]["s2_boot_1049h"] == 0,
        "count": leg2["raw_counts"]["s2_boot_1049h"]}
    rows = check_screen(leg2, FEAT, "s2_showimages_row", [("Show images", "present")])
    leg2["showimages_row"] = find_row(rows, "Show images:")
    leg2["checks"]["restart:showImages-row-false"] = {
        "ok": bool(leg2["showimages_row"] and "Show images:" in leg2["showimages_row"] and "false" in leg2["showimages_row"].lower()),
        "row": leg2["showimages_row"]}
    check_screen(leg2, FEAT, "s2_image_fallback", [("[Image:", "present")])
    rows = check_screen(leg2, FEAT, "s2_tuimode_row", [("TUI mode", "present")])
    leg2["tuimode_row"] = find_row(rows, "TUI mode:")
    leg2["checks"]["restart:tuiMode-row-fullscreen"] = {
        "ok": bool(leg2["tuimode_row"] and "TUI mode:" in leg2["tuimode_row"] and "fullscreen" in leg2["tuimode_row"].lower()),
        "row": leg2["tuimode_row"]}

    with open(os.path.join(FEAT, "result.json"), "w", encoding="utf-8") as f:
        json.dump({"legs": {"settings1": leg1, "settings2": leg2},
                   "binary": PIR}, f, indent=2, ensure_ascii=False)

    # ---------------- Plugins leg 1 ------------------------------------------
    pdir = os.path.join(PLUG, "project")
    os.makedirs(pdir, exist_ok=True)
    with open(os.path.join(pdir, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round43 plugins\n")
    plug_agent = os.path.join(PLUG, "runs", "p1_plugins", "home", ".pi", "agent")
    os.makedirs(plug_agent, exist_ok=True)
    seed_agent_dir(plug_agent, True)
    helper_dir = os.path.join(plug_agent, "agents")
    os.makedirs(helper_dir, exist_ok=True)
    with open(os.path.join(helper_dir, "helper.md"), "w", encoding="utf-8") as f:
        f.write("---\nname: helper\ndescription: Round43 helper agent\n---\n"
                "You are a careful helper. Do the task and report briefly.\n")
    script = [
        {"tool_calls": [
            {"name": "todo", "arguments": {"action": "create", "subject": "R43 todo alpha"}},
            {"name": "todo", "arguments": {"action": "create", "subject": "R43 todo beta"}},
            {"name": "memory", "arguments": {"action": "remember", "kind": "decision",
                                              "scope": "repo", "subject": "R43-MEMORY",
                                              "text": "Round43 memory body: todo/memory/subagent verified."}},
        ]},
        {"tool_call": {"name": "todo", "arguments": {"action": "update", "id": 99, "status": "completed"}}},
        {"tool_call": {"name": "subagent", "arguments": {
            "agent": "helper",
            "task": "Create a file subagent_r43.txt containing the single line: helper r43"}}},
        {"tool_call": {"name": "bash", "arguments": {
            "command": "printf 'helper r43\\n' > subagent_r43.txt"}}},
        {"text": "SUBAGENT-R43-CHILD"},
        {"text": "SUBAGENT-R43-OK"},
    ]
    p1 = run_tui("p1_plugins", PLUG, [
        ("wait_quiet", 5.0), ("snap", "p1_boot"),
        ("log", "one prompt: todo create, memory remember, todo update error, subagent"),
        ("send", b"R43 plugin surfaces please\r"), ("wait", 10.0), ("wait_quiet", 4.0),
        ("snap", "p1_turn"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "p1_todos"),
        ("send", b"/memory status\r"), ("wait", 1.5), ("snap", "p1_memstatus"),
        ("send", b"/memory brief R43-MEMORY\r"), ("wait", 1.5), ("snap", "p1_membrief"),
        ("send", b"/memory bogus\r"), ("wait", 1.5), ("snap", "p1_memerr"),
    ], pdir, script, agent=plug_agent)
    legp1 = {"exit_code": p1["exit_code"], "timed_out": p1["timed_out"],
             "elapsed_s": p1["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(legp1, PLUG, "p1_turn",
                 [("2 pending", "present"), ("R43 todo alpha", "present"),
                  ("not found", "present"), ("SUBAGENT-R43-OK", "present"),
                  ("subagent helper", "present")])
    check_screen(legp1, PLUG, "p1_todos", [("2 pending", "present"), ("R43 todo alpha", "present")])
    rows = check_screen(legp1, PLUG, "p1_memstatus", [("doc_count", "present")])
    legp1["memstatus_rows"] = [r for _, r in rows_with(rows, "doc_count")] + \
        [r for _, r in rows_with(rows, "exists")]
    check_screen(legp1, PLUG, "p1_membrief", [("R43-MEMORY", "present")])
    check_screen(legp1, PLUG, "p1_memerr", [("unknown memory action", "present")])
    # persistence artifacts
    mem_files = []
    for dirpath, _dirs, names in os.walk(os.path.join(p1["agent"], "sessions")):
        for entry in names:
            if entry == "cm":
                mem_files.append(os.path.join(dirpath, entry))
    legp1["memory_store_files"] = mem_files
    legp1["checks"]["memory:store-file-exists"] = {"ok": bool(mem_files), "files": mem_files}

    # ---------------- Plugins leg 2: restart/--continue recovery --------------
    p2 = run_tui("p2_continue", PLUG, [
        ("wait_quiet", 6.0), ("snap", "p2_boot"),
        ("send", b"/todos\r"), ("wait", 1.5), ("snap", "p2_todos"),
        ("send", b"/memory status\r"), ("wait", 1.5), ("snap", "p2_memstatus"),
        ("send", b"/memory brief R43-MEMORY\r"), ("wait", 1.5), ("snap", "p2_membrief"),
    ], pdir, [{"text": "R43-CONTINUE-OK"}], args=["--continue"], agent=p1["agent"])
    legp2 = {"exit_code": p2["exit_code"], "timed_out": p2["timed_out"],
             "elapsed_s": p2["elapsed_s"], "checks": {}, "rows": {}}
    check_screen(legp2, PLUG, "p2_boot",
                 [("R43 todo alpha", "present"), ("SUBAGENT-R43-OK", "present")])
    check_screen(legp2, PLUG, "p2_todos", [("2 pending", "present"), ("R43 todo alpha", "present")])
    check_screen(legp2, PLUG, "p2_memstatus", [("doc_count", "present")])
    check_screen(legp2, PLUG, "p2_membrief", [("R43-MEMORY", "present")])

    with open(os.path.join(PLUG, "result.json"), "w", encoding="utf-8") as f:
        json.dump({"legs": {"plugins1": legp1, "plugins2": legp2},
                   "binary": PIR}, f, indent=2, ensure_ascii=False)

    for root in (FEAT, PLUG):
        acc3_render.render_dir(os.path.join(root, "shots"))

    def summary(leg):
        return {"exit": leg["exit_code"], "timed_out": leg["timed_out"],
                "checks_ok": sum(1 for c in leg["checks"].values() if c["ok"]),
                "checks_total": len(leg["checks"]),
                "failed": [k for k, c in leg["checks"].items() if not c["ok"]]}
    print(json.dumps({"settings1": summary(leg1), "settings2": summary(leg2),
                      "plugins1": summary(legp1), "plugins2": summary(legp2)},
                     indent=2, ensure_ascii=False))
    if any(not c["ok"] for leg in (leg1, leg2, legp1, legp2)
           for c in leg["checks"].values()):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
