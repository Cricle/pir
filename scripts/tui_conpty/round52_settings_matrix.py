# -*- coding: utf-8 -*-
"""Round 52 lane-tui: /settings full-key matrix (>=10 keys) on real ConPTY.

Keys covered in one live session (fullscreen default):
  theme, terminal.showImages, warnings.anthropicExtraUsage, editorPaddingX,
  autocompleteMaxVisible, fullscreenScrollbar, fullscreenWheelScrollLines,
  tuiMode, modelThinkingLevels (per-model thinking).
Plus a restart leg verifying persistence/effective values, and a live-vs-
restart check for editorPaddingX (upstream applies it live via
onEditorPaddingXChange).

Mock LLM only supplies a provider/model; no model round-trip is needed.
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
ROOT = os.environ.get("PIR_SETTINGS_EVIDENCE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round52-settings-matrix")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def rows_of(shot):
    raw = open(os.path.join(ROOT, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def find_row(rows, needle):
    return next((r for r in rows if needle in r), None)


def row_value(row):
    m = re.search(r":\s*(-?\d+)\s*$", row or "")
    return int(m.group(1)) if m else None


def count_popup_items(rows):
    idx = next((i for i, r in enumerate(rows) if re.search(r"\(\d+/\d+\)", r)), None)
    if idx is None:
        return 0
    n = 0
    i = idx - 1
    while i >= 0 and rows[i].strip() and not rows[i].lstrip().startswith("─"):
        n += 1
        i -= 1
    return n


def editor_pad(raw):
    """Leading spaces before the reverse-video cursor cell (last occurrence)."""
    hits = re.findall(rb"( +)\x1b\[7m \x1b\[27m", raw)
    return len(hits[-1]) if hits else None


def run_tui(name, ops, script, agent=None):
    base = os.path.join(ROOT, "runs", name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    seeded = agent is None
    if agent is None:
        agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    if seeded:
        seed_agent_dir(agent, True)
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent, "PI_IMAGE_PROTOCOL": "kitty"}
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=os.path.join(ROOT, "project"),
            shots_dir=shots, driver_log=os.path.join(shots, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, timeout=220)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    return {"agent": agent, "exit_code": out.get("exit_code"),
            "timed_out": out.get("timed_out"), "elapsed_s": out.get("elapsed_s")}


def close_menu():
    return [("send", b"\x1b"), ("wait", 0.5), ("send", b"\x1b"), ("wait", 0.8)]


def open_setting(term):
    return [("send", b"/settings\r"), ("wait", 1.1),
            ("send", term.encode()), ("wait", 0.7)]


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)
    with open(os.path.join(ROOT, "project", "note.txt"), "w", encoding="utf-8") as f:
        f.write("round52 settings matrix\n")
    img = os.path.join(REPO, ".play", "evidence", "lane-tui", "round39-paste-theme", "img39.png")
    image_arg = img.replace("\\", "/")

    ops1 = [("wait_quiet", 5.0), ("snap", "s1_boot")]
    # theme (submenu: down once -> light, enter applies)
    ops1 += [("log", "theme dark -> light")] + open_setting("theme") + [
        ("snap", "s1_theme_filter"), ("send", b"\r"), ("wait", 0.7),
        ("snap", "s1_theme_sub"), ("send", b"\x1b[B"), ("wait", 0.4),
        ("send", b"\r"), ("wait", 1.0), ("snap", "s1_theme_applied")] + close_menu()
    # showImages true -> false, then /image must fall back
    ops1 += [("log", "showImages true -> false")] + open_setting("Show images") + [
        ("snap", "s1_images_row"), ("send", b"\r"), ("wait", 1.0),
        ("snap", "s1_images_off")] + close_menu() + [
        ("send", ("/image " + image_arg).encode()), ("wait", 0.3), ("send", b"\r"),
        ("wait", 1.2), ("snap", "s1_image_fallback")]
    # warnings submenu
    ops1 += [("log", "warnings.anthropicExtraUsage true -> false")] + open_setting("Warnings") + [
        ("send", b"\r"), ("wait", 0.7), ("snap", "s1_warn_sub"),
        ("send", b"\r"), ("wait", 1.0), ("snap", "s1_warn_off")] + close_menu()
    # editorPaddingX 0 -> 1
    ops1 += [("log", "editorPaddingX 0 -> 1 (live indent measured)")] + open_setting("Editor padding") + [
        ("send", b"\r"), ("wait", 1.0), ("snap", "s1_padding_saved")] + close_menu() + [
        ("snap", "s1_padding_after")]
    # autocompleteMaxVisible 5 -> 10
    ops1 += [("log", "autocompleteMaxVisible default 5 -> 10")] + open_setting("Autocomplete max items") + [
        ("snap", "s1_ac_default"), ("send", b"\r"), ("wait", 0.6), ("send", b"\r"),
        ("wait", 1.0), ("snap", "s1_ac_10")] + close_menu() + [
        ("send", b"/"), ("wait", 1.0), ("snap", "s1_ac_popup"), ("send", b"\x1b"), ("wait", 0.4),
        ("send", b"\x7f"), ("wait", 0.3)]
    # fullscreenScrollbar auto -> always (live right-edge bar)
    ops1 += [("log", "fullscreenScrollbar auto -> always")] + open_setting("Fullscreen scrollbar") + [
        ("snap", "s1_sb_row"), ("send", b"\r"), ("wait", 1.0),
        ("snap", "s1_sb_applied")] + close_menu()
    # fullscreenWheelScrollLines auto -> 1
    ops1 += [("log", "fullscreenWheelScrollLines auto -> 1")] + open_setting("Fullscreen wheel scrolling") + [
        ("snap", "s1_wheel_row"), ("send", b"\r"), ("wait", 1.0),
        ("snap", "s1_wheel_applied")] + close_menu()
    # tuiMode row: unset displays "regular" but the runtime default is
    # fullscreen (menu index 0 bug); Enter #1 writes explicit fullscreen,
    # Enter #2 cycles to regular and live-switches.
    ops1 += [("log", "tuiMode -> regular (two cycles from the menu default)")] + open_setting("TUI mode") + [
        ("snap", "s1_tuimode_row"), ("send", b"\r"), ("wait", 0.8),
        ("send", b"\r"), ("wait", 1.2), ("snap", "s1_tui_regular")] + close_menu()
    # modelThinkingLevels submenu: model -> level down once
    ops1 += [("log", "modelThinkingLevels override")] + open_setting("Default thinking level per model") + [
        ("send", b"\r"), ("wait", 0.8), ("snap", "s1_mt_models"),
        ("send", b"\r"), ("wait", 0.8), ("snap", "s1_mt_levels"),
        ("send", b"\x1b[B"), ("wait", 0.4), ("send", b"\r"), ("wait", 1.0),
        ("snap", "s1_mt_saved")] + close_menu()
    s1 = run_tui("s1_matrix", ops1, [{"text": "unused"}])
    leg1 = {"exit_code": s1["exit_code"], "timed_out": s1["timed_out"],
            "elapsed_s": s1["elapsed_s"], "checks": {}, "rows": {}}

    def C(name, ok, **kw):
        leg1["checks"][name] = {"ok": bool(ok), **kw}

    rows, raw_boot = rows_of("s1_boot")
    leg1["pad_boot"] = editor_pad(raw_boot)
    rows, raw_theme = rows_of("s1_theme_applied")
    leg1["theme_notice"] = find_row(rows, "Theme:")
    leg1["accent_boot"] = raw_boot.count(b"\x1b[38;5;97m")
    leg1["accent_theme"] = raw_theme.count(b"\x1b[38;5;97m")
    C("theme:live-notice", leg1["theme_notice"] is not None, row=leg1["theme_notice"])
    C("theme:live-accent-changed", leg1["accent_theme"] != leg1["accent_boot"],
      before=leg1["accent_boot"], after=leg1["accent_theme"])
    rows = rows_of("s1_images_off")[0]
    leg1["images_notice"] = find_row(rows, "Saved setting: terminal.showImages")
    C("showImages:saved", leg1["images_notice"] is not None, row=leg1["images_notice"])
    rows = rows_of("s1_image_fallback")[0]
    leg1["image_fallback"] = find_row(rows, "[Image:")
    C("showImages:live-fallback", leg1["image_fallback"] is not None, row=leg1["image_fallback"])
    rows = rows_of("s1_warn_off")[0]
    leg1["warn_notice"] = find_row(rows, "Saved setting: warnings.anthropicExtraUsage")
    C("warnings:saved", leg1["warn_notice"] is not None, row=leg1["warn_notice"])
    rows = rows_of("s1_padding_saved")[0]
    leg1["padding_notice"] = find_row(rows, "Saved setting: editorPaddingX")
    C("editorPaddingX:saved", leg1["padding_notice"] is not None, row=leg1["padding_notice"])
    _, raw_pad = rows_of("s1_padding_after")
    leg1["pad_live"] = editor_pad(raw_pad)
    C("editorPaddingX:live-indent", leg1["pad_live"] == 2,
      boot=leg1["pad_boot"], live=leg1["pad_live"])
    rows = rows_of("s1_ac_default")[0]
    leg1["ac_default_row"] = find_row(rows, "Autocomplete max items:")
    rows = rows_of("s1_ac_10")[0]
    leg1["ac_10_row"] = find_row(rows, "Autocomplete max items:")
    leg1["ac_popup_items"] = count_popup_items(rows_of("s1_ac_popup")[0])
    C("autocompleteMaxVisible:row-10", row_value(leg1["ac_10_row"]) == 10,
      default=leg1["ac_default_row"], after=leg1["ac_10_row"])
    C("autocompleteMaxVisible:live-window", leg1["ac_popup_items"] == 10,
      items=leg1["ac_popup_items"])
    _, raw_sb = rows_of("s1_sb_applied")
    leg1["scrollbar_bars"] = raw_sb.decode("utf-8", "replace").count("┃")
    C("fullscreenScrollbar:live-bars", leg1["scrollbar_bars"] > 0, count=leg1["scrollbar_bars"])
    rows = rows_of("s1_wheel_applied")[0]
    leg1["wheel_row"] = find_row(rows, "Fullscreen wheel scrolling:")
    C("fullscreenWheelScrollLines:row-1", row_value(leg1["wheel_row"]) == 1,
      row=leg1["wheel_row"])
    rows, raw_tui = rows_of("s1_tui_regular")
    leg1["tui_notice"] = next((r for r in reversed(rows) if "TUI mode:" in r), None)
    leg1["alt_leave"] = raw_tui.count(b"\x1b[?1049l")
    C("tuiMode:live-switch", leg1["tui_notice"] is not None and "regular" in (leg1["tui_notice"] or "").lower(),
      row=leg1["tui_notice"], alt_leave=leg1["alt_leave"])
    rows = rows_of("s1_mt_saved")[0]
    leg1["mt_notice"] = find_row(rows, "Saved setting: modelThinkingLevels")
    C("modelThinkingLevels:saved", leg1["mt_notice"] is not None, row=leg1["mt_notice"])

    settings_path = os.path.join(s1["agent"], "settings.json")
    with open(settings_path, encoding="utf-8") as f:
        saved = json.load(f)
    leg1["settings"] = saved
    leg1["settings_path"] = settings_path
    expect = [
        ("theme", isinstance(saved.get("theme"), str) and saved.get("theme") == "light"),
        ("terminal.showImages", saved.get("terminal", {}).get("showImages") is False),
        ("warnings.anthropicExtraUsage", saved.get("warnings", {}).get("anthropicExtraUsage") is False),
        ("editorPaddingX", saved.get("editorPaddingX") == 1),
        ("autocompleteMaxVisible", saved.get("autocompleteMaxVisible") == 10),
        ("fullscreenScrollbar", saved.get("fullscreenScrollbar") == "always"),
        ("fullscreenWheelScrollLines", saved.get("fullscreenWheelScrollLines") == 1),
        ("tuiMode", saved.get("tuiMode") == "regular"),
        ("modelThinkingLevels", isinstance(saved.get("modelThinkingLevels"), dict)
         and len(saved.get("modelThinkingLevels")) >= 1),
    ]
    leg1["settings_expect"] = dict(expect)
    for key, ok in expect:
        C("settings.json:" + key, ok)

    # -------- restart leg --------
    def settings_snap(term, name):
        return open_setting(term) + [("snap", name)] + close_menu()

    ops2 = [("wait_quiet", 5.0), ("snap", "s2_boot")]
    for term, name in [
            ("theme", "s2_theme"), ("Show images", "s2_images"),
            ("Editor padding", "s2_padding"), ("Autocomplete max items", "s2_ac"),
            ("Fullscreen scrollbar", "s2_sb"), ("TUI mode", "s2_tui"),
            ("Default thinking level per model", "s2_mt")]:
        ops2 += settings_snap(term, name)
    ops2 += [("send", b"/"), ("wait", 1.0), ("snap", "s2_ac_popup"),
             ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x7f"), ("wait", 0.3),
             ("send", ("/image " + image_arg).encode()), ("wait", 0.3), ("send", b"\r"),
             ("wait", 1.2), ("snap", "s2_image")]
    s2 = run_tui("s2_restart", ops2, [{"text": "unused"}], agent=s1["agent"])
    leg2 = {"exit_code": s2["exit_code"], "timed_out": s2["timed_out"],
            "elapsed_s": s2["elapsed_s"], "checks": {}, "rows": {}}

    def C2(name, ok, **kw):
        leg2["checks"][name] = {"ok": bool(ok), **kw}

    _, raw2 = rows_of("s2_boot")
    leg2["pad_restart"] = editor_pad(raw2)
    leg2["alt_enter_boot"] = raw2.count(b"\x1b[?1049h")
    C2("restart:tuiMode-regular-no-altscreen", leg2["alt_enter_boot"] == 0,
       count=leg2["alt_enter_boot"])
    C2("restart:editorPaddingX-effective", leg2["pad_restart"] == 2,
       boot=leg1["pad_boot"], live=leg1["pad_live"], restart=leg2["pad_restart"])
    checks = [
        ("theme", "Theme", ""),
        ("images", "Show images:", "false"),
        ("padding", "Editor padding:", "1"),
        ("ac", "Autocomplete max items:", "10"),
        ("sb", "Fullscreen scrollbar:", "always"),
        ("tui", "TUI mode:", "regular"),
        ("mt", "Default thinking level per model", ""),
    ]
    for name, needle, want in checks:
        row = find_row(rows_of("s2_" + name)[0], needle)
        leg2["rows"][name] = row
        C2("restart:row:" + needle.strip(":"), row is not None and want.lower() in (row or "").lower(),
           row=row)
    leg2["ac_popup_items"] = count_popup_items(rows_of("s2_ac_popup")[0])
    C2("restart:autocomplete-window", leg2["ac_popup_items"] == 10, items=leg2["ac_popup_items"])
    C2("restart:image-fallback", find_row(rows_of("s2_image")[0], "[Image:") is not None)
    with open(settings_path, encoding="utf-8") as f:
        leg2["settings"] = json.load(f)
    C2("restart:settings-theme-light", leg2["settings"].get("theme") == "light",
       value=leg2["settings"].get("theme"))
    C2("restart:settings-editorPaddingX", leg2["settings"].get("editorPaddingX") == 1)

    acc3_render.render_dir(os.path.join(ROOT, "shots"))
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump({"binary": PIR, "legs": {"s1_matrix": leg1, "s2_restart": leg2}}, f,
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
