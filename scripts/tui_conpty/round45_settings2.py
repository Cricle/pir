# -*- coding: utf-8 -*-
"""Round 45/48 lane-tui: remaining /settings keys.

PIR-TUI-014 regression (R48): the `autocompleteMaxVisible` row and the `/`
popup must agree at the runtime default (5), after setting 10, after setting
15, after restart, and in the minimum variant 3.

  s0_ac_before      : baseline `/` autocomplete window on a fresh agent dir.
  s1_change         : autocompleteMaxVisible 5 -> 10 -> 15 (popup measured at
                      each step), fullscreenScrollbar, fullscreenWheelScrollLines.
  s2_restart        : fresh TUI on the same agent dir: rows + autocomplete 15.
  s3_ac_three       : fresh dir, row 5 -> Left -> 3, popup measured.
  s4_ac_three_restart: same agent dir restarted, popup stays 3.

Real ConPTY; mock LLM only supplies replies (no model call in these legs).
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
    REPO, ".play", "evidence", "lane-tui", "round48-pir014", "settings-regression")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def rows_of(shot):
    raw = open(os.path.join(ROOT, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def find_row(rows, needle):
    for r in rows:
        if needle in r and "→" in r:
            return r
    return next((r for r in rows if needle in r), None)


def row_value(row):
    """`→ Autocomplete max items: 10` -> 10."""
    match = re.search(r":\s*(\d+)\s*$", row or "")
    return int(match.group(1)) if match else None


def count_popup_items(rows):
    """Rows of the `/` autocomplete list: contiguous non-empty lines above the
    `(n/total)` counter, bounded by the separator rule above them."""
    idx = next((i for i, r in enumerate(rows) if re.search(r"\(\d+/\d+\)", r)), None)
    if idx is None:
        return 0
    n = 0
    i = idx - 1
    while i >= 0 and rows[i].strip() and not rows[i].lstrip().startswith("─"):
        n += 1
        i -= 1
    return n


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
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")
    try:
        tui_drive.run_scenario(
            name, ops, extra_env=env, cwd=os.path.join(ROOT, "project"),
            shots_dir=shots, driver_log=os.path.join(shots, name + ".driver.log"),
            result_json=result_json, agent_dir=agent, timeout=160)
    finally:
        mock.stop()
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    return {"agent": agent, "exit_code": out.get("exit_code"),
            "timed_out": out.get("timed_out"), "elapsed_s": out.get("elapsed_s")}


def close_settings_ops():
    return [("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.8)]


def clear_editor_ops():
    return [("send", b"\x7f"), ("wait", 0.4)]


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)

    s0 = run_tui("s0_ac_before", [
        ("wait_quiet", 5.0), ("snap", "s0_boot"),
        ("send", b"/"), ("wait", 1.0), ("snap", "s0_ac"),
        ("send", b"\x1b"), ("wait", 0.5),
    ], [{"text": "unused"}])
    base_items = count_popup_items(rows_of("s0_ac")[0])

    ops1 = [
        ("wait_quiet", 5.0), ("snap", "s1_boot"),
        ("log", "autocompleteMaxVisible: default row and popup"),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Autocomplete max items"), ("wait", 0.8), ("snap", "s1_ac_row_default"),
        ("send", b"\r"), ("wait", 0.8), ("send", b"\r"), ("wait", 1.2),
        ("snap", "s1_ac_10"),
    ] + close_settings_ops() + [
        ("send", b"/"), ("wait", 1.0), ("snap", "s1_ac_popup_10"),
        ("send", b"\x1b"), ("wait", 0.4),
    ] + clear_editor_ops() + [
        ("log", "autocompleteMaxVisible: 10 -> 15"),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Autocomplete max items"), ("wait", 0.8), ("snap", "s1_ac_row_10"),
        ("send", b"\r"), ("wait", 1.2), ("snap", "s1_ac_15"),
    ] + close_settings_ops() + [
        ("send", b"/"), ("wait", 1.0), ("snap", "s1_ac_popup_15"),
        ("send", b"\x1b"), ("wait", 0.4),
    ] + clear_editor_ops() + [
        ("log", "fullscreenScrollbar: cycle row"),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Fullscreen scrollbar"), ("wait", 0.8), ("snap", "s1_sb_row"),
        ("send", b"\r"), ("wait", 1.2), ("snap", "s1_sb_applied"),
    ] + close_settings_ops() + [
        ("log", "fullscreenWheelScrollLines: cycle row"),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Fullscreen wheel scrolling"), ("wait", 0.8), ("snap", "s1_wheel_row"),
        ("send", b"\r"), ("wait", 1.2), ("snap", "s1_wheel_applied"),
    ] + close_settings_ops()
    s1 = run_tui("s1_change", ops1, [{"text": "unused"}])
    leg1 = {"exit_code": s1["exit_code"], "timed_out": s1["timed_out"],
            "elapsed_s": s1["elapsed_s"], "checks": {}, "ac_items_default": base_items}
    for name, label, key in (
            ("s1_sb", "Fullscreen scrollbar", "fullscreenScrollbar"),
            ("s1_wheel", "Fullscreen wheel scrolling", "fullscreenWheelScrollLines")):
        rows_before = rows_of(name + "_row")[0]
        rows_after = rows_of(name + "_applied")[0]
        before = find_row(rows_before, label)
        after = find_row(rows_after, label)
        leg1[name + "_before"] = before
        leg1[name + "_after"] = after
        leg1["checks"][name + ":row-changed"] = {
            "ok": bool(before and after and before != after),
            "before": before, "after": after}
        leg1["checks"][name + ":saved-notice"] = {
            "ok": find_row(rows_after, "Saved setting: " + key) is not None}
        _, raw = rows_of(name + "_applied")
        leg1["checks"][name + ":no_mojibake"] = {"ok": b"\xef\xbf\xbd" not in raw}

    # PIR-TUI-014: menu row and popup must agree at every value, no fake success.
    row_default = find_row(rows_of("s1_ac_row_default")[0], "Autocomplete max items")
    row_10 = find_row(rows_of("s1_ac_row_10")[0], "Autocomplete max items")
    row_15 = find_row(rows_of("s1_ac_15")[0], "Autocomplete max items")
    popup_10 = count_popup_items(rows_of("s1_ac_popup_10")[0])
    popup_15 = count_popup_items(rows_of("s1_ac_popup_15")[0])
    leg1["ac_row_default"], leg1["ac_row_10"], leg1["ac_row_15"] = row_default, row_10, row_15
    leg1["ac_items_10"], leg1["ac_items_15"] = popup_10, popup_15
    leg1["checks"]["ac:default-row-is-runtime-default"] = {
        "ok": row_value(row_default) == 5, "row": row_default}
    leg1["checks"]["ac:row-10"] = {"ok": row_value(row_10) == 10, "row": row_10}
    leg1["checks"]["ac:popup-10"] = {"ok": popup_10 == 10, "items": popup_10}
    leg1["checks"]["ac:popup-10-matches-row"] = {
        "ok": popup_10 == row_value(row_10), "items": popup_10,
        "row": row_value(row_10)}
    leg1["checks"]["ac:row-15"] = {"ok": row_value(row_15) == 15, "row": row_15}
    leg1["checks"]["ac:popup-15"] = {"ok": popup_15 == 15, "items": popup_15}
    leg1["checks"]["ac:popup-15-matches-row"] = {
        "ok": popup_15 == row_value(row_15), "items": popup_15,
        "row": row_value(row_15)}

    settings_path = os.path.join(s1["agent"], "settings.json")
    with open(settings_path, encoding="utf-8") as f:
        saved = json.load(f)
    leg1["settings_path"] = settings_path
    leg1["settings_after"] = saved
    leg1["checks"]["settings.json:autocompleteMaxVisible"] = {
        "ok": saved.get("autocompleteMaxVisible") == 15,
        "value": saved.get("autocompleteMaxVisible")}
    leg1["checks"]["settings.json:fullscreenScrollbar"] = {
        "ok": saved.get("fullscreenScrollbar") in ("auto", "always", "hidden"),
        "value": saved.get("fullscreenScrollbar")}
    leg1["checks"]["settings.json:fullscreenWheelScrollLines"] = {
        "ok": saved.get("fullscreenWheelScrollLines") in ("auto", 1, 2, 3, 5, 10),
        "value": saved.get("fullscreenWheelScrollLines")}

    s2 = run_tui("s2_restart", [
        ("wait_quiet", 5.0), ("snap", "s2_boot"),
        ("send", b"\x1b"), ("wait", 0.5),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Autocomplete max items"), ("wait", 0.8), ("snap", "s2_ac_row"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.6),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Fullscreen scrollbar"), ("wait", 0.8), ("snap", "s2_sb_row"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.6),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Fullscreen wheel scrolling"), ("wait", 0.8), ("snap", "s2_wheel_row"),
        ("send", b"\x1b"), ("wait", 0.4), ("send", b"\x1b"), ("wait", 0.6),
    ], [{"text": "unused"}], agent=s1["agent"])
    leg2 = {"exit_code": s2["exit_code"], "timed_out": s2["timed_out"],
            "elapsed_s": s2["elapsed_s"], "checks": {}}
    for name, label, value in (
            ("s2_ac_row", "Autocomplete max items", str(saved.get("autocompleteMaxVisible"))),
            ("s2_sb_row", "Fullscreen scrollbar", str(saved.get("fullscreenScrollbar"))),
            ("s2_wheel_row", "Fullscreen wheel scrolling", str(saved.get("fullscreenWheelScrollLines")))):
        row = find_row(rows_of(name)[0], label)
        leg2["checks"][name + ":row-value"] = {
            "ok": bool(row and value in row), "row": row, "value": value}
    s3 = run_tui("s3_ac_restart", [
        ("wait_quiet", 5.0), ("send", b"/"), ("wait", 1.0), ("snap", "s3_ac"),
    ], [{"text": "unused"}], agent=s1["agent"])
    leg2["ac_items_restart"] = count_popup_items(rows_of("s3_ac")[0])
    leg1["ac_items_after"] = leg2["ac_items_restart"]
    leg1["checks"]["ac:live-window-grew"] = {
        "ok": leg1["ac_items_after"] > leg1["ac_items_default"],
        "default": leg1["ac_items_default"], "after": leg1["ac_items_after"]}
    leg1["checks"]["ac:restart-window-matches"] = {
        "ok": leg2["ac_items_restart"] == 15,
        "restart": leg2["ac_items_restart"], "expected": 15}

    # PIR-TUI-014 minimum variant: 5 -> Left -> 3, then restart.
    s4 = run_tui("s4_ac_three", [
        ("wait_quiet", 5.0), ("snap", "s4_boot"),
        ("send", b"/settings\r"), ("wait", 1.2),
        ("send", b"Autocomplete max items"), ("wait", 0.8), ("snap", "s4_ac_row_default"),
        ("send", b"\x1b[D"), ("wait", 1.2), ("snap", "s4_ac_row_3"),
    ] + close_settings_ops() + [
        ("send", b"/"), ("wait", 1.0), ("snap", "s4_ac_popup_3"),
        ("send", b"\x1b"), ("wait", 0.4),
    ], [{"text": "unused"}])
    leg3 = {"exit_code": s4["exit_code"], "timed_out": s4["timed_out"],
            "elapsed_s": s4["elapsed_s"], "checks": {}}
    row4_default = find_row(rows_of("s4_ac_row_default")[0], "Autocomplete max items")
    row4_three = find_row(rows_of("s4_ac_row_3")[0], "Autocomplete max items")
    popup4 = count_popup_items(rows_of("s4_ac_popup_3")[0])
    leg3["ac_row_default"], leg3["ac_row_3"], leg3["ac_items"] = (
        row4_default, row4_three, popup4)
    leg3["checks"]["ac3:default-row-is-runtime-default"] = {
        "ok": row_value(row4_default) == 5, "row": row4_default}
    leg3["checks"]["ac3:row-3"] = {"ok": row_value(row4_three) == 3, "row": row4_three}
    leg3["checks"]["ac3:popup-3"] = {"ok": popup4 == 3, "items": popup4}
    leg3["checks"]["ac3:popup-matches-row"] = {
        "ok": popup4 == row_value(row4_three), "items": popup4,
        "row": row_value(row4_three)}
    with open(os.path.join(s4["agent"], "settings.json"), encoding="utf-8") as f:
        saved4 = json.load(f)
    leg3["checks"]["ac3:settings.json"] = {
        "ok": saved4.get("autocompleteMaxVisible") == 3,
        "value": saved4.get("autocompleteMaxVisible")}

    s5 = run_tui("s5_ac_three_restart", [
        ("wait_quiet", 5.0), ("send", b"/"), ("wait", 1.0), ("snap", "s5_ac"),
    ], [{"text": "unused"}], agent=s4["agent"])
    popup5 = count_popup_items(rows_of("s5_ac")[0])
    leg3["ac_items_restart"] = popup5
    leg3["checks"]["ac3:restart-popup-3"] = {"ok": popup5 == 3, "items": popup5}

    result = {"legs": {"s0_ac_before": {"exit_code": s0["exit_code"],
                                        "timed_out": s0["timed_out"],
                                        "ac_items": base_items},
                       "s1_change": leg1, "s2_restart": leg2,
                       "s3_ac_three": leg3},
              "binary": PIR}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(os.path.join(ROOT, "shots"))

    def summary(leg):
        return {"exit": leg["exit_code"], "timed_out": leg["timed_out"],
                "ok": sum(1 for c in leg["checks"].values() if c["ok"]),
                "total": len(leg["checks"]),
                "failed": [k for k, c in leg["checks"].items() if not c["ok"]]}
    print(json.dumps({k: summary(v) for k, v in result["legs"].items()
                      if "checks" in v}, indent=2, ensure_ascii=False))
    return 0 if all(c["ok"] for leg in (leg1, leg2, leg3)
                    for c in leg["checks"].values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
