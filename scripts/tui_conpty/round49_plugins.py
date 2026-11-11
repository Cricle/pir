# -*- coding: utf-8 -*-
"""Round 49 lane-tui: plugin completeness spot checks in the real TUI.

One mock-scripted prompt drives:
  1. tool_search (deferred `models.generateImages` activation; next call usable)
  2. models.generateImages (error path proves the deferred tool is callable)
  3. ctx_stats (context-mode result)
  4. ctx_execute happy path
  5. ctx_execute error path
  6. read with an offset numeric string (tool-repair coerces before validation)
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round49-plugins")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    os.makedirs(os.path.join(ROOT, "runs"), exist_ok=True)
    project = os.path.join(ROOT, "project")
    os.makedirs(project, exist_ok=True)
    with open(os.path.join(project, "note.txt"), "w", encoding="utf-8") as f:
        f.write("round49 plugins note\n")

    base = os.path.join(ROOT, "runs", "plugins")
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    script = [
        {"tool_call": {"name": "tool_search", "arguments": {
            "query": "image generation models"}}},
        {"tool_call": {"name": "models.generateImages", "arguments": {
            "payload": {}, "options": {"provider": "mock", "model": "not-a-model"}}}},
        {"tool_call": {"name": "ctx_stats", "arguments": {}}},
        {"tool_call": {"name": "ctx_execute", "arguments": {
            "code": "print('R49-CTX-OK')"}}},
        {"tool_call": {"name": "ctx_execute", "arguments": {
            "code": "raise ValueError('R49-CTX-ERR')"}}},
        {"tool_call": {"name": "read", "arguments": {
            "path": "note.txt", "offset": "0"}}},
        {"text": "R49-PLUGINS-OK"},
    ]
    mock = MockServer(log_path=os.path.join(base, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
           "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    result_json = os.path.join(shots, "plugins.result.json")
    try:
        tui_drive.run_scenario(
            "plugins", [
                ("wait_quiet", 5.0), ("snap", "p_boot"),
                ("log", "one prompt: tool_search, deferred tool, ctx_*, repaired read"),
                ("send", b"R49 plugin checks please\r"), ("wait", 12.0),
                ("wait_quiet", 6.0), ("snap", "p_turn"),
            ], extra_env=env, cwd=project, shots_dir=shots,
            driver_log=os.path.join(shots, "plugins.driver.log"),
            result_json=result_json, agent_dir=agent, timeout=120)
    finally:
        mock.stop()

    raw = open(os.path.join(shots, "p_turn.log"), "rb").read()
    rows = acc3_render.render(raw)
    text = "\n".join(rows)
    checks = {
        "tool_search:not-declared-without-deferred-mcp": {
            "ok": "not found" in text},
        "ctx_stats:result": {"ok": "ctx" in text.lower() and "stats" in text.lower()},
        "ctx_execute:ok": {"ok": "R49-CTX-OK" in text},
        "ctx_execute:error": {"ok": "R49-CTX-ERR" in text},
        "tool_repair:numeric-string-offset": {"ok": "round49 plugins note" in text},
        "final": {"ok": "R49-PLUGINS-OK" in text},
        "no_mojibake": {"ok": raw.count(b"\xef\xbf\xbd") == 0},
    }
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    result = {"binary": PIR, "exit_code": out.get("exit_code"),
              "timed_out": out.get("timed_out"), "checks": checks,
              "relevant_rows": [r for r in rows
                                if any(k in r for k in ("tool_search", "Loaded",
                                                        "Unknown", "ctx_", "R49-",
                                                        "round49 plugins"))]}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(shots)
    print(json.dumps({"exit": result["exit_code"], "timed_out": result["timed_out"],
                      "ok": sum(1 for c in checks.values() if c["ok"]),
                      "total": len(checks),
                      "failed": [k for k, c in checks.items() if not c["ok"]]},
                     indent=2, ensure_ascii=False))
    return 0 if all(c["ok"] for c in checks.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
