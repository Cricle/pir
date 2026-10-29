#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Cold-start attribution for `pir` (Windows, debug or release binary).

Why this exists: `mode_scorecard.py` reports one number per mode.  When a
cold-start number looks slow, the next question is always "which phase paid
it?" — and the answer must be measured, not guessed.  This script runs the
mode-level legs AND the phase-isolating A/B pairs that separate app work from
harness/host noise:

  version/help        fresh-process floor (no settings, no catalog)
  list-models         settings + full builtin catalog (+ one models.json
                      provider), A/B: tiny provider vs OpenRouter entry —
                      the delta is the duplicated per-provider catalog parse
  models-list         settings + one models.json provider only (no catalog)
  print               A/B: default builtin plan vs --no-extensions (extension
                      runtime + serialized model catalog), and vs
                      --no-skills/--no-prompt-templates (resource discovery)
  tui                 first frame + the TUI's own PI_TIMING phase report.
                      A/B: resolvable model (catalog serialized for the
                      extension runtime) vs no resolvable model (skipped)
  server              spawn -> "listening on" line

Every leg uses `big_session_probe`'s mock OpenAI server and sanitized agent
dir; TUI legs use the real ConPTY driver (`scripts/tui_conpty/tui_drive.py`).
Median/min/max and every sample are printed and written to JSON.

Usage:
  python scripts/perf/cold_start_attribution.py [--pir target/debug/pir.exe]
      [--runs 6] [--tui-runs 3] [--out logs/lane-cold-start/attribution]

The output is comparable before/after a rebuild as long as the binary path and
the runs count stay the same.
"""
import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "scripts", "tui_conpty"))

import big_session_probe as big  # noqa: E402
import tui_drive  # noqa: E402

FRAME_END = b"\x1b[?2026l"
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


def stats(samples):
    return {
        "n": len(samples),
        "medianMs": round(statistics.median(samples), 1),
        "minMs": round(min(samples), 1),
        "maxMs": round(max(samples), 1),
        "samplesMs": [round(s, 1) for s in samples],
    }


def run_timed(cmd, env, cwd=None):
    started = time.perf_counter()
    proc = subprocess.run(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, env=env, cwd=cwd, timeout=300)
    return (time.perf_counter() - started) * 1000.0, proc


def child_env(agent_dir, extra=None):
    env = dict(os.environ)
    env["PIR_AGENT_DIR"] = agent_dir
    env["PI_CODING_AGENT_DIR"] = agent_dir
    env["PIR_NO_UPDATE_CHECK"] = "1"
    env["RUST_LOG"] = "error"
    if extra:
        env.update(extra)
    return env


def write_models(agent_dir, cfg):
    os.makedirs(agent_dir, exist_ok=True)
    with open(os.path.join(agent_dir, "models.json"), "w", encoding="utf-8") as fh:
        json.dump(cfg, fh)


def mock_models_cfg(port, provider="mock", api_key=None):
    provider_cfg = {
        "baseUrl": f"http://127.0.0.1:{port}/v1",
        "api": "openai-completions",
        "model": "mock-model",
    }
    if api_key is not None:
        provider_cfg["apiKey"] = api_key
    return {"providers": {provider: provider_cfg}}


OPENROUTER_CFG = {"providers": {"openrouter": {"apiKey": "sk-test", "model": "openai/gpt-4o"}}}


def bench_cmd(label, cmd, env, runs, out, cwd=None, warmup=1):
    for _ in range(warmup):
        run_timed(cmd, env, cwd)
    samples = []
    last = None
    for _ in range(runs):
        ms, last = run_timed(cmd, env, cwd)
        samples.append(ms)
    entry = stats(samples)
    entry.update({"label": label, "cmd": cmd, "exit": last.returncode if last else None,
                  "stdoutHead": (last.stdout[:120].decode("utf-8", "replace") if last else "")})
    out["legs"][label] = entry
    print(f"[{label:22}] median {entry['medianMs']:7.1f}  min {entry['minMs']:7.1f}  "
          f"max {entry['maxMs']:7.1f}  exit {entry['exit']}")
    return entry


def run_tui_once(pir, agent_dir, work_dir, out_path, timeout=60):
    cmdline = f'"{pir}" interactive --no-session'
    env = dict(tui_drive.BASE_ENV)
    env.update({"PIR_AGENT_DIR": agent_dir, "PI_CODING_AGENT_DIR": agent_dir,
                "PIR_NO_UPDATE_CHECK": "1", "RUST_LOG": "error", "PI_TIMING": "1"})
    started = time.perf_counter()
    pty = tui_drive.Pty(cmdline, work_dir, env)
    setup_ms = (time.perf_counter() - started) * 1000.0
    frame = None
    first_byte = None
    while time.perf_counter() - started < timeout:
        data = bytes(pty.snapshot())
        if first_byte is None and data:
            first_byte = (time.perf_counter() - started) * 1000.0
        if FRAME_END in data:
            frame = (time.perf_counter() - started) * 1000.0
            break
        if not pty.running():
            break
        time.sleep(0.01)
    time.sleep(0.25)
    try:
        pty.write(b"/quit\r")
        if not pty.wait(8):
            pty.write(b"\x03")
            pty.wait(3)
    except OSError:
        pass
    time.sleep(0.2)
    text = ANSI.sub("", bytes(pty.snapshot()).decode("utf-8", "replace"))
    phases = {}
    for line in text.splitlines():
        match = re.match(r"\s*(tui\.[A-Za-z]+):\s*(\d+)ms", line)
        if match:
            phases[match.group(1)] = int(match.group(2))
    try:
        with open(out_path, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)
    except OSError:
        pass
    try:
        pty.terminate()
        pty.close()
    except Exception:
        pass
    return setup_ms, first_byte, frame, phases


def run_tui_leg(label, pir, agent_dir, runs, out_dir, out, start):
    work = tempfile.mkdtemp(prefix=f"pir-attr-work-{label}-")
    frames = []
    phase_rows = []
    for index in range(runs):
        log_path = os.path.join(out_dir, f"{label}_tui_{index + 1}.txt")
        setup_ms, first_byte, frame, phases = run_tui_once(pir, agent_dir, work, log_path)
        if frame is not None:
            frames.append(frame)
        phase_rows.append({"setupMs": round(setup_ms, 1),
                           "firstByteMs": round(first_byte, 1) if first_byte else None,
                           "firstFrameMs": round(frame, 1) if frame else None,
                           "phases": phases})
    entry = stats(frames) if frames else {"n": 0}
    entry.update({"label": label, "runs": phase_rows})
    out["legs"][label] = entry
    print(f"[{label:22}] frames {entry.get('samplesMs')} phases "
          f"{[row['phases'] for row in phase_rows]}")
    return entry


def run_server_leg(pir, runs, out, out_dir):
    samples = []
    for index in range(runs):
        agent = tempfile.mkdtemp(prefix="pir-attr-srv-")
        sessions = os.path.join(agent, "sessions")
        os.makedirs(sessions, exist_ok=True)
        env = child_env(agent, {"PIR_SESSION_DIR": sessions,
                                "PIR_SERVER_ADDRESS": "127.0.0.1:0"})
        started = time.perf_counter()
        proc = subprocess.Popen([pir, "server", "--no-extensions"],
                                stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE, env=env)
        line = b""
        while time.perf_counter() - started < 30:
            line = proc.stderr.readline()
            if b"listening on" in line:
                break
            if proc.poll() is not None:
                break
        samples.append((time.perf_counter() - started) * 1000.0)
        try:
            proc.kill()
            proc.wait(timeout=10)
        except subprocess.SubprocessError:
            pass
        try:
            proc.stderr.close()
        except OSError:
            pass
    entry = stats(samples)
    entry.update({"label": "server-listening", "runs": runs})
    out["legs"]["server-listening"] = entry
    print(f"[{'server-listening':22}] median {entry['medianMs']:7.1f}  "
          f"min {entry['minMs']:7.1f}  samples {entry['samplesMs']}")
    return entry


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--pir", default=os.path.join(REPO, "target", "debug", "pir.exe"))
    ap.add_argument("--runs", type=int, default=6)
    ap.add_argument("--tui-runs", type=int, default=3)
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "lane-cold-start",
                                                  "attribution"))
    args = ap.parse_args(argv)
    if os.name != "nt":
        print("Windows-only (ConPTY)", file=sys.stderr)
        return 2
    pir = os.path.abspath(args.pir)
    out_dir = os.path.abspath(args.out)
    os.makedirs(out_dir, exist_ok=True)
    identity = big.binary_identity(pir)
    mock = big.Mock(("127.0.0.1", 0), big._Mock)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    port = mock.server_address[1]
    mock_agent = tempfile.mkdtemp(prefix="pir-attr-mock-")
    write_models(mock_agent, mock_models_cfg(port, api_key="sk-test"))
    # Control: same provider but NO credential, so model resolution fails and
    # the TUI leaves `configured` as the models.json-only snapshot. That is the
    # exact shape in which the extension-runtime model catalog is not
    # serialized at all; the frame/phase delta against `mock_agent` measures
    # what the catalog costs.
    nokey_agent = tempfile.mkdtemp(prefix="pir-attr-nokey-")
    write_models(nokey_agent, mock_models_cfg(port))
    or_agent = tempfile.mkdtemp(prefix="pir-attr-or-")
    write_models(or_agent, OPENROUTER_CFG)

    out = {"pir": identity, "generated": time.strftime("%Y-%m-%d %H:%M:%S"),
           "legs": {}}
    t0 = time.perf_counter()
    try:
        bench_cmd("version", [pir, "--version"], child_env(mock_agent), 10, out)
        bench_cmd("help", [pir, "--help"], child_env(mock_agent), 10, out)
        bench_cmd("models-list-mock", [pir, "models", "list"], child_env(mock_agent), args.runs, out)
        bench_cmd("models-list-or", [pir, "models", "list"], child_env(or_agent), args.runs, out)
        bench_cmd("list-models-mock", [pir, "--list-models"], child_env(mock_agent), args.runs, out)
        bench_cmd("list-models-or", [pir, "--list-models"], child_env(or_agent), args.runs, out)
        base_print = ["print", "--no-session", "--model", "mock/mock-model", "Reply with OK"]
        bench_cmd("print-default", [pir] + base_print, child_env(mock_agent), args.runs, out)
        bench_cmd("print-no-extensions", [pir] + base_print + ["--no-extensions"],
                  child_env(mock_agent), args.runs, out)
        bench_cmd("print-no-skills", [pir] + base_print + ["--no-skills", "--no-prompt-templates"],
                  child_env(mock_agent), args.runs, out)
        run_tui_leg("tui-model", pir, mock_agent, args.tui_runs, out_dir, out, t0)
        run_tui_leg("tui-no-model", pir, nokey_agent, args.tui_runs, out_dir, out, t0)
        run_server_leg(pir, 5, out, out_dir)
    finally:
        mock.shutdown()
    out["elapsedS"] = round(time.perf_counter() - t0, 1)
    path = os.path.join(out_dir, "attribution.json")
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(out, fh, indent=2)
    print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
