# -*- coding: utf-8 -*-
"""Round 34 lane-tui: real `@file` completion + prompt-expansion journey.

Project tree covers multiple files, a directory, a space path, a CJK path and
a deep path. Each leg is a fresh TUI over the same project; the mock LLM
receives the turned prompt, and the session JSONL records the expanded user
message (coding_agent::context::expand_prompt_file_args).
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round34-atfile")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
SHOTS = os.path.join(ROOT, "shots")


def setup_project():
    project = os.path.join(ROOT, "project")
    shutil.rmtree(project, ignore_errors=True)
    files = {
        "alpha.py": "print('ALPHA-MARKER')\n",
        "src/main.rs": "fn main() { println!(\"SRC-MARKER\"); }\n",
        "src/nested/deep/leaf.txt": "DEEP-MARKER\n",
        "dir one/hello world.txt": "SPACE-MARKER\n",
        "中文目录/你好.txt": "ZH-MARKER\n",
        "docs/README.md": "# DOCS-MARKER\n",
    }
    for rel, content in files.items():
        path = os.path.join(project, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(content)
    return project


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    project = setup_project()
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"))
    result = {"pir": PIR, "project": project, "legs": {}}
    try:
        legs = [
            ("leg1_at_alpha", [b"@", b"al"], [b"", b""],
             [("wait", 4), ("wait_quiet", 2), ("snap", "leg1_boot"),
              ("send", b"@"), ("wait", 1.5), ("snap", "leg1_at_list"),
              ("send", b"al"), ("wait", 1.0), ("snap", "leg1_at_filtered"),
              ("send", b"\r"), ("wait", 0.8), ("snap", "leg1_inserted"),
              ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg1_submitted")]),
            ("leg2_at_space", None, None,
             [("wait", 4), ("wait_quiet", 2),
              ("send", b"@hel"), ("wait", 1.5), ("snap", "leg2_at_list"),
              ("send", b"\r"), ("wait", 0.8), ("snap", "leg2_inserted"),
              ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg2_submitted")]),
            ("leg3_at_zh", None, None,
             [("wait", 4), ("wait_quiet", 2),
              ("send", "@你好".encode("utf-8")), ("wait", 1.5), ("snap", "leg3_at_list"),
              ("send", b"\r"), ("wait", 0.8), ("snap", "leg3_inserted"),
              ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg3_submitted")]),
            ("leg4_at_deep", None, None,
             [("wait", 4), ("wait_quiet", 2),
              ("send", b"@leaf"), ("wait", 1.5), ("snap", "leg4_at_list"),
              ("send", b"\r"), ("wait", 0.8), ("snap", "leg4_inserted"),
              ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg4_submitted")]),
            ("leg5_at_dir", None, None,
             [("wait", 4), ("wait_quiet", 2),
              ("send", b"@src"), ("wait", 1.5), ("snap", "leg5_at_list"),
              ("send", b"\r"), ("wait", 0.8), ("snap", "leg5_dir_inserted"),
              ("send", b"main"), ("wait", 1.0), ("snap", "leg5_file_list"),
              ("send", b"\r"), ("wait", 0.8), ("snap", "leg5_inserted"),
              ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg5_submitted")]),
            ("leg6_at_multi", None, None,
             [("wait", 4), ("wait_quiet", 2),
              ("send", b"@alpha.py @src/main.rs explain both"), ("wait", 1.0),
              ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg6_submitted")]),
        ]
        for name, _unused, _unused2, ops in legs:
            base = os.path.join(ROOT, "runs", name)
            home = os.path.join(base, "home")
            agent = os.path.join(home, ".pi", "agent")
            os.makedirs(agent, exist_ok=True)
            seed_agent_dir(agent, True)
            inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
            env = {"PIR_TRUST_PROJECT": "1", "HOME": home, "USERPROFILE": home,
                   "PIR_AGENT_DIR": agent}
            tui_drive.PIR = PIR
            result_json = os.path.join(SHOTS, name + ".result.json")
            tui_drive.run_scenario(
                name, ops, extra_env=env, cwd=project, shots_dir=SHOTS,
                driver_log=os.path.join(SHOTS, name + ".driver.log"),
                result_json=result_json, agent_dir=agent, timeout=60)
            with open(result_json, encoding="utf-8") as f:
                out = json.load(f)
            # Extract the first user message recorded in this leg's session.
            sessions_root = os.path.join(agent, "sessions")
            user_text = None
            session_file = None
            for dirpath, _dirs, names in os.walk(sessions_root):
                for entry in names:
                    if not entry.endswith(".jsonl"):
                        continue
                    path = os.path.join(dirpath, entry)
                    session_file = path
                    try:
                        with open(path, encoding="utf-8") as f:
                            for line in f:
                                obj = json.loads(line)
                                msg = obj.get("message") or {}
                                if msg.get("role") == "user":
                                    content = msg.get("content")
                                    if isinstance(content, list):
                                        user_text = "".join(
                                            part.get("text", "") for part in content
                                            if isinstance(part, dict))
                                    else:
                                        user_text = str(content)
                                    break
                    except (OSError, ValueError):
                        pass
                    if user_text:
                        break
                if user_text:
                    break
            result["legs"][name] = {
                "exit_code": out.get("exit_code"),
                "elapsed_s": out.get("elapsed_s"),
                "timed_out": out.get("timed_out"),
                "session_file": session_file,
                "first_user_message": user_text,
            }
    finally:
        mock.stop()
    acc3_render.render_dir(SHOTS)
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, default=str)
    for name, leg in result["legs"].items():
        print(name, "exit=", leg["exit_code"], "msg=", repr((leg["first_user_message"] or "")[:120]))


if __name__ == "__main__":
    main()
