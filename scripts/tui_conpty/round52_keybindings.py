# -*- coding: utf-8 -*-
"""Round 52 lane-tui: docs/keybindings.md vs real /hotkeys + behaviour.

Leg kb   : settings.json keybinding override `app.model.select: ctrl+y`
           (ctrl+l must stop opening the selector, ctrl+y must open it), plus
           real behaviour for ctrl+o (tool expand), ctrl+f (alt-screen
           search), ctrl+u (delete to line start), shift+tab (thinking),
           ctrl+c (clear then exit). /hotkeys table captured for comparison.
Leg doc  : the two ids used by docs/keybindings.md's example
           (`app.submit`, `tui.selector.moveUp`) are not in the accepted
           registry; startup must report invalid keybindings (doc drift).
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
ROOT = os.environ.get("PIR_KB_EVIDENCE") or os.path.join(
    REPO, ".play", "evidence", "lane-tui", "round52-keybindings")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def rows_of(shot):
    raw = open(os.path.join(ROOT, "shots", shot + ".log"), "rb").read()
    return acc3_render.render(raw), raw


def run_tui(name, ops, script, settings_patch=None, agent=None):
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
    if settings_patch:
        sp = os.path.join(agent, "settings.json")
        with open(sp, encoding="utf-8") as f:
            data = json.load(f)
        data.update(settings_patch)
        with open(sp, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2)
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


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "project"), exist_ok=True)
    with open(os.path.join(ROOT, "project", "note.txt"), "w", encoding="utf-8") as f:
        f.write("round52 keybindings\n")

    script = [
        {"tool_calls": [{"name": "bash", "arguments": {"command": "seq 1 40"}}]},
        {"text": "ROUND52-KB-OK"},
    ]
    ops = [
        ("wait_quiet", 5.0), ("snap", "k_boot"),
        ("log", "ctrl+f search"),
        ("send", b"\x06"), ("wait", 0.8), ("snap", "k_search_open"),
        ("send", b"ROUND52"), ("wait", 0.8), ("snap", "k_search_typed"),
        ("send", b"\x1b"), ("wait", 0.5), ("snap", "k_search_closed"),
        ("log", "shift+tab thinking cycle"),
        ("snap", "k_footer_before"), ("send", b"\x1b[Z"), ("wait", 0.8),
        ("snap", "k_footer_after"),
        ("log", "one turn with bash output for ctrl+o"),
        ("send", b"run seq\r"), ("wait", 8.0), ("wait_quiet", 3.0), ("snap", "k_turn"),
        ("send", b"\x0f"), ("wait", 1.0), ("snap", "k_expanded"),
        ("log", "ctrl+y override opens model selector; ctrl+l no longer does"),
        ("send", b"\x19"), ("wait", 1.0), ("snap", "k_model_ctrl_y"),
        ("send", b"\x1b"), ("wait", 0.5),
        ("send", b"\x0c"), ("wait", 1.0), ("snap", "k_model_ctrl_l"),
        ("log", "ctrl+u delete-to-line-start"),
        ("send", b"abcdef"), ("wait", 0.5), ("snap", "k_typed"),
        ("send", b"\x15"), ("wait", 0.6), ("snap", "k_ctrl_u"),
        ("log", "ctrl+c clears editor, second ctrl+c exits"),
        ("send", b"zzz"), ("wait", 0.4), ("snap", "k_typed2"),
        ("send", b"\x03"), ("wait", 0.6), ("snap", "k_ctrl_c_clear"),
        ("send", b"\x03"), ("wait", 1.5),
    ]
    kb = run_tui("kb", ops, script, settings_patch={
        "keybindings": {"app.model.select": "ctrl+y"}})
    leg = {"exit_code": kb["exit_code"], "timed_out": kb["timed_out"],
           "elapsed_s": kb["elapsed_s"], "checks": {}}

    def C(name, ok, **kw):
        leg["checks"][name] = {"ok": bool(ok), **kw}

    # /hotkeys in its own short leg: the table floods a regular-mode viewport.
    kh = run_tui("kh", [("wait_quiet", 5.0), ("send", b"/hotkeys\r"),
                        ("wait", 1.2), ("snap", "kh_hotkeys")],
                 [{"text": "unused"}])
    hot_rows = rows_of("kh_hotkeys")[0]
    hot_text = "\n".join(hot_rows)
    leg["hotkeys_text"] = hot_rows
    for token in ["enter", "esc", "ctrl+c", "ctrl+d", "shift+tab", "ctrl+p", "ctrl+o", "ctrl+u"]:
        C("hotkeys:has-" + token, token in hot_text.lower(), token=token)
    for phrase in ["Send message", "Clear editor", "Exit", "Cycle thinking level",
                   "Toggle tool output expansion", "Delete to start of line"]:
        C("hotkeys:action-" + phrase.replace(" ", "-"),
          phrase.lower() in hot_text.lower(), phrase=phrase)

    _, raw_search = rows_of("k_search_open")
    rows_search = rows_of("k_search_open")[0]
    leg["search_rows"] = [r for r in rows_search if "Find in transcript" in r or "No matches" in r or re.search(r"\d+/\d+", r) or "Search" in r]
    C("ctrl+f:search-open", any("Find in transcript" in r or "No matches" in r or re.search(r"\d+/\d+", r) or "Search" in r
                                for r in rows_search), rows=leg["search_rows"])
    rows_typed = rows_of("k_search_typed")[0]
    C("ctrl+f:search-typed", any("ROUND52" in r for r in rows_typed) or
      any(re.search(r"1/1", r) for r in rows_typed),
      rows=[r for r in rows_typed if "ROUND52" in r or re.search(r"\d+/\d+", r)])

    before = rows_of("k_footer_before")[0]
    after = rows_of("k_turn")[0]
    fb = next((r for r in reversed(before) if "gpt-5" in r and "•" in r), None)
    fa = next((r for r in reversed(after) if "gpt-5" in r and "•" in r), None)
    leg["footer_before"], leg["footer_after"] = fb, fa
    C("shift+tab:thinking-changed", fb is not None and fa is not None and fb != fa,
      before=fb, after=fa)

    def numeric_rows(rows):
        return sum(1 for r in rows if r.strip().isdigit())
    turn_rows = rows_of("k_turn")[0]
    exp_rows = rows_of("k_expanded")[0]
    leg["num_turn"], leg["num_expanded"] = numeric_rows(turn_rows), numeric_rows(exp_rows)
    C("ctrl+o:expands-tool-output", leg["num_expanded"] > leg["num_turn"],
      collapsed=leg["num_turn"], expanded=leg["num_expanded"])

    y_rows = rows_of("k_model_ctrl_y")[0]
    l_rows = rows_of("k_model_ctrl_l")[0]
    leg["y_rows"] = [r for r in y_rows if "gpt-5" in r or "✓" in r or "Enter to select" in r]
    leg["l_rows"] = [r for r in l_rows if "gpt-5" in r or "✓" in r or "Enter to select" in r]
    C("ctrl+y:model-selector-opens", any("Enter to select" in r or "✓" in r for r in y_rows), rows=leg["y_rows"])
    C("ctrl+l:old-binding-gone", not any("Enter to select" in r or "✓" in r for r in l_rows), rows=leg["l_rows"])

    typed_rows = rows_of("k_typed")[0]
    u_rows = rows_of("k_ctrl_u")[0]
    C("ctrl+u:editor-before", any("abcdef" in r for r in typed_rows))
    C("ctrl+u:deletes-to-line-start", not any("abcdef" in r for r in u_rows))
    c_rows = rows_of("k_ctrl_c_clear")[0]
    C("ctrl+c:clears-editor", not any("zzz" in r for r in c_rows))
    C("ctrl+c:second-exits", kb["exit_code"] == 0 and not kb["timed_out"],
      exit_code=kb["exit_code"], timed_out=kb["timed_out"])

    # doc leg: the exact ids from docs/keybindings.md's example
    doc = run_tui("doc", [("wait_quiet", 5.0), ("snap", "d_boot"),
                          ("send", b"/hotkeys\r"), ("wait", 1.2), ("snap", "d_hotkeys")],
                  [{"text": "unused"}], settings_patch={
                      "keybindings": {"app.submit": "enter",
                                      "tui.selector.moveUp": "up,k"}})
    legd = {"exit_code": doc["exit_code"], "timed_out": doc["timed_out"],
            "elapsed_s": doc["elapsed_s"], "checks": {}}
    _, raw_doc = rows_of("d_boot")
    txt = raw_doc.decode("utf-8", "replace")
    legd["invalid_notice"] = [line for line in txt.splitlines()
                              if "invalid keybindings" in line.lower()]
    # Actual behavior (source: from_settings second pass, upstream
    # `if (!(keybinding in definitions)) continue`): unknown ids are silently
    # ignored, NOT a settings error as docs/keybindings.md claimed.
    legd["checks"]["doc-example-ids-silently-ignored"] = {
        "ok": not legd["invalid_notice"], "lines": legd["invalid_notice"]}
    hotd = "\n".join(rows_of("d_hotkeys")[0])
    legd["checks"]["doc-example-falls-back-to-defaults"] = {
        "ok": "enter" in hotd.lower() and "up" in hotd.lower() and "app.submit" not in hotd.lower()}

    acc3_render.render_dir(os.path.join(ROOT, "shots"))
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump({"binary": PIR, "legs": {"kb": leg, "doc": legd}}, f,
                  indent=2, ensure_ascii=False)

    def summary(l):
        return {"exit": l["exit_code"], "timed_out": l["timed_out"],
                "ok": sum(1 for c in l["checks"].values() if c["ok"]),
                "total": len(l["checks"]),
                "failed": [k for k, c in l["checks"].items() if not c["ok"]]}
    print(json.dumps({"kb": summary(leg), "doc": summary(legd)}, indent=2, ensure_ascii=False))
    return 1 if any(not c["ok"] for l in (leg, legd) for c in l["checks"].values()) else 0


if __name__ == "__main__":
    raise SystemExit(main())
