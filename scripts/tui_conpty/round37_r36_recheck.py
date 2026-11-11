# -*- coding: utf-8 -*-
"""Round 35 lane-tui: PIR-TUI-012 space-path @file completion verification.

Six fresh-TUI legs over a project with space files/directories, CJK and deep
paths. The session JSONL records the expanded first user message
(coding_agent::context::expand_prompt_file_args), which must contain a
`<file name="…">` block for every accepted `@` attachment.
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
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round37-r36-recheck", "run")
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
        "src dir/nested dir/note file.txt": "SPACEDIR-MARKER\n",
        "中文目录/你好.txt": "ZH-MARKER\n",
        "docs/README.md": "# DOCS-MARKER\n",
    }
    for rel, content in files.items():
        path = os.path.join(project, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(content)
    return project


def leg_ops(*ops):
    return [("wait", 4), ("wait_quiet", 2)] + list(ops)


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)
    project = setup_project()
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"))
    result = {"pir": PIR, "project": project, "legs": {}}
    try:
        legs = [
            # Core PIR-TUI-012: space file via fuzzy `@hel`.
            ("leg1_at_space", leg_ops(
                ("send", b"@hel"), ("wait", 1.5), ("snap", "leg1_at_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg1_inserted"),
                ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg1_submitted"))),
            # Already-quoted prefix `@"dir one/hel` must not double the quote.
            ("leg2_quoted_prefix", leg_ops(
                ("send", "@\"dir one/hel".encode("utf-8")), ("wait", 1.5),
                ("snap", "leg2_at_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg2_inserted"),
                ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg2_submitted"))),
            # Space directory: `@src` fuzzy list, Down to `src dir/`,
            # Enter inserts `@"src dir/"`, then `note` completes the file.
            ("leg3_space_dir", leg_ops(
                ("send", b"@src"), ("wait", 1.5), ("snap", "leg3_dir_list"),
                ("send", b"\x1b[B"), ("wait", 0.5), ("snap", "leg3_dir_select"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg3_dir_inserted"),
                ("send", b"note"), ("wait", 1.5), ("snap", "leg3_file_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg3_inserted"),
                ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg3_submitted"))),
            # CJK regression.
            ("leg4_at_zh", leg_ops(
                ("send", "@你好".encode("utf-8")), ("wait", 1.5), ("snap", "leg4_at_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg4_inserted"),
                ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg4_submitted"))),
            # Multi-file: existing mention + spaced second attachment.
            ("leg5_multi", leg_ops(
                ("send", b"@alpha.py @hel"), ("wait", 1.5), ("snap", "leg5_at_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg5_inserted"),
                ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg5_submitted"))),
            # Baseline directory flow unaffected.
            ("leg6_at_dir", leg_ops(
                ("send", b"@src"), ("wait", 1.5), ("snap", "leg6_at_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg6_dir_inserted"),
                ("send", b"main"), ("wait", 1.0), ("snap", "leg6_file_list"),
                ("send", b"\r"), ("wait", 0.8), ("snap", "leg6_inserted"),
                ("send", b"\r"), ("wait_quiet", 8), ("snap", "leg6_submitted"))),
        ]
        for name, ops in legs:
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
        print(name, "exit=", leg["exit_code"], "msg=", repr((leg["first_user_message"] or "")[:160]))


if __name__ == "__main__":
    main()
