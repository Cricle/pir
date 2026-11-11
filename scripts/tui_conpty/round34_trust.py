# -*- coding: utf-8 -*-
"""Round 34 lane-tui: real TUI project-trust selector journey.

Isolated fake HOME + PIR_AGENT_DIR. The project has .pi/mcp.json with a local
stdio MCP server (`pir mcp-sd`), so the first interactive start is
trust-requiring. The startup prompt renders BEFORE the TUI owns the terminal
(main_cli::cli_preprocess::resolve_project_trust_for_run -> read_trust_choice).

Legs:
  1 trust_yes          fresh agent: prompt -> "1" -> TUI; /mcp; real MCP tool call
  2 restart_trusted    same agent: no prompt, mcp still active
  3 trust_no           fresh agent: prompt -> "4" (persistent Do not trust)
  4 restart_untrusted  same agent: no prompt (stored false), mcp inactive
  5 session_only       fresh agent: prompt -> "5" (Trust this session only)
  6 restart_session    same agent: prompt AGAIN (nothing written)
  7 esc_cancel         fresh agent: prompt -> Esc -> TUI starts untrusted, no write
  8 restart_esc        same agent: prompt AGAIN
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round34-trust")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
SHOTS = os.path.join(ROOT, "shots")

RESULT = {"pir": PIR, "legs": {}}


def fresh_tree(name):
    base = os.path.join(ROOT, "runs", name)
    shutil.rmtree(base, ignore_errors=True)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    project = os.path.join(base, "project")
    for path in (home, agent, project):
        os.makedirs(path, exist_ok=True)
    with open(os.path.join(project, "hello.txt"), "w", encoding="utf-8") as f:
        f.write("hello world\n")
    mcp = {"mcpServers": {"sd": {
        "type": "stdio",
        "command": PIR.replace("\\", "/"),
        "args": ["mcp-sd", "--root", project.replace("\\", "/")],
    }}}
    os.makedirs(os.path.join(project, ".pi"), exist_ok=True)
    with open(os.path.join(project, ".pi", "mcp.json"), "w", encoding="utf-8") as f:
        json.dump(mcp, f, indent=2)
    seed_agent_dir(agent, True)
    return home, agent, project


def make_env(home, agent):
    return {
        "PIR_TRUST_PROJECT": "0",  # restore store/default policy (BASE sets "1")
        "HOME": home,
        "USERPROFILE": home,
        "PIR_AGENT_DIR": agent,
    }


def run_leg(name, pid, agent, project, ops, env, timeout=60):
    tui_drive.PIR = PIR
    result_json = os.path.join(SHOTS, name + ".result.json")
    tui_drive.run_scenario(
        name, ops, extra_env=env, cwd=project, shots_dir=SHOTS,
        driver_log=os.path.join(SHOTS, name + ".driver.log"),
        result_json=result_json, agent_dir=agent, timeout=timeout,
    )
    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    RESULT["legs"][name] = out
    return out


def read_text(path):
    try:
        with open(path, "rb") as f:
            return f.read().decode("utf-8", "replace")
    except OSError:
        return None


def record_trust(agent, project, label):
    trust_path = os.path.join(agent, "trust.json")
    RESULT["legs"][label + "_trust"] = {
        "path": trust_path,
        "exists": os.path.exists(trust_path),
        "content": read_text(trust_path),
        "project": project,
    }


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"), script=[
        {"tool_call": {"name": "mcp__sd__sd", "arguments": {
            "pattern": "hello", "replacement": "HELLO", "dry_run": True}}},
        {"text": "SD-DONE"},
    ])
    try:
        # ---- legs 1/2: persistent yes ------------------------------------
        home, agent, project = fresh_tree("yes")
        seed_agent_dir(agent, True)
        inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
        env = make_env(home, agent)
        run_leg("leg1_trust_yes", None, agent, project, [
            ("wait", 4),
            ("snap", "leg1_prompt"),
            ("send", b"1\r"),
            ("wait", 5),
            ("wait_quiet", 3),
            ("snap", "leg1_tui"),
            ("send", b"/mcp\r"),
            ("wait", 2),
            ("snap", "leg1_mcp_cmd"),
            ("send", b"\x1b"),
            ("wait", 1),
            ("send", b"use the sd tool on hello.txt\r"),
            ("wait_quiet", 10),
            ("snap", "leg1_sd_tool"),
        ], env)
        record_trust(agent, project, "leg1")
        run_leg("leg2_restart_trusted", None, agent, project, [
            ("wait", 4),
            ("wait_quiet", 2),
            ("snap", "leg2_boot"),
            ("send", b"/mcp\r"),
            ("wait", 2),
            ("snap", "leg2_mcp_cmd"),
        ], env)

        # ---- legs 3/4: persistent no -------------------------------------
        home, agent, project = fresh_tree("no")
        seed_agent_dir(agent, True)
        inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
        env = make_env(home, agent)
        run_leg("leg3_trust_no", None, agent, project, [
            ("wait", 4),
            ("snap", "leg3_prompt"),
            ("send", b"4\r"),
            ("wait", 5),
            ("wait_quiet", 3),
            ("snap", "leg3_tui"),
            ("send", b"/mcp\r"),
            ("wait", 2),
            ("snap", "leg3_mcp_cmd"),
        ], env)
        record_trust(agent, project, "leg3")
        run_leg("leg4_restart_untrusted", None, agent, project, [
            ("wait", 4),
            ("wait_quiet", 2),
            ("snap", "leg4_boot"),
        ], env)

        # ---- legs 5/6: session-only yes ----------------------------------
        home, agent, project = fresh_tree("session")
        seed_agent_dir(agent, True)
        inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
        env = make_env(home, agent)
        run_leg("leg5_session_only", None, agent, project, [
            ("wait", 4),
            ("snap", "leg5_prompt"),
            ("send", b"5\r"),
            ("wait", 5),
            ("wait_quiet", 3),
            ("snap", "leg5_tui"),
        ], env)
        record_trust(agent, project, "leg5")
        run_leg("leg6_restart_session", None, agent, project, [
            ("wait", 4),
            ("wait_quiet", 2),
            ("snap", "leg6_prompt"),
        ], env)

        # ---- legs 7/8: Esc cancel ----------------------------------------
        home, agent, project = fresh_tree("esc")
        seed_agent_dir(agent, True)
        inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
        env = make_env(home, agent)
        run_leg("leg7_esc_cancel", None, agent, project, [
            ("wait", 4),
            ("snap", "leg7_prompt"),
            ("send", b"\x1b"),
            ("wait", 5),
            ("wait_quiet", 3),
            ("snap", "leg7_tui"),
        ], env)
        record_trust(agent, project, "leg7")
        run_leg("leg8_restart_esc", None, agent, project, [
            ("wait", 4),
            ("wait_quiet", 2),
            ("snap", "leg8_prompt"),
        ], env)
    finally:
        mock.stop()

    for log_path, screen_path, _lines in acc3_render.render_dir(SHOTS):
        pass
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(RESULT, f, indent=2)
    print(json.dumps({k: {"exit": v.get("exit_code"), "elapsed": v.get("elapsed_s"),
                          "timeout": v.get("timed_out")}
                      for k, v in RESULT["legs"].items() if "exit_code" in v}, indent=2))


if __name__ == "__main__":
    main()
