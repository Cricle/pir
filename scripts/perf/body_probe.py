#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Capture and compare the FIRST provider request body of two CLIs.

Why: the A/B showed pir sending ~1.5x the input tokens of upstream pi per turn
(pir r2: 146k vs pi r2: 95k) and a much lower prompt-cache read ratio. A fixed
per-request prefix (system prompt + tool schemas) that is bigger than pi's, or
one that changes between turns, would explain both. This probe answers it
directly: it points both CLIs at a LOCAL mock that logs the exact JSON body, so
the comparison needs no gateway.

Usage:
    python scripts/perf/body_probe.py --cli <pir.exe> --label pir --out logs/perf/body-pir.json
    python scripts/perf/body_probe.py --cli <pi.cmd>  --label pi  --out logs/perf/body-pi.json
Then compare the two JSONs (the driver `--both` mode does both and prints a
side-by-side summary).
"""

import argparse
import http.server
import json
import os
import re
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

PROMPT = "reply with the single word OK"
REPLY = "OK"

BODIES = []
REQUESTS = 0
TOOL_CALLS = 0  # mock answers the first N requests with a bash tool call


def _sse(payload: dict) -> bytes:
    return ("data: " + json.dumps(payload) + "\n\n").encode("utf-8")


class _Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def do_POST(self):
        global REQUESTS
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) or b"{}"
        try:
            BODIES.append(json.loads(raw))
        except Exception:
            BODIES.append({"__unparsed__": raw[:400].decode("utf-8", "replace")})
        # `--multi-turn N`: answer the first N requests with a bash tool call so
        # the CLI issues a follow-up request, and only then a final text answer.
        # This exposes prefix stability (tools + system) across turns.
        with_tool_call = REQUESTS < TOOL_CALLS
        REQUESTS += 1
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        created = int(time.time())
        chunks = [
            {"role": "assistant", "content": ""},
            {"content": REPLY},
        ]
        try:
            if with_tool_call:
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                                 "finish_reason": None}],
                }))
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0, "id": "call_mock_1", "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\": \"echo mock\"}"},
                    }]}, "finish_reason": None}],
                }))
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }))
                self.wfile.write(b"data: [DONE]\n\n")
                return
            for delta in chunks:
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
                }))
            self.wfile.write(_sse({
                "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }))
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            return


class Mock(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def build_agent_dir(port: int, reasoning: bool = False) -> str:
    """Isolated agent dir: models.json pointing at the mock, minimal settings.

    `reasoning=True` mirrors the user's real openai model config (reasoning true
    + the same thinkingLevelMap) so the `--thinking low` wire value each CLI
    sends can be compared without a gateway.
    """
    agent = tempfile.mkdtemp(prefix="pir-body-probe-")
    model = {
        "id": "mock-model", "name": "Mock", "reasoning": reasoning,
        "input": ["text"], "contextWindow": 128000, "maxTokens": 4096,
    }
    if reasoning:
        model["thinkingLevelMap"] = {
            "minimal": None, "low": "low", "medium": None,
            "high": "high", "xhigh": None, "max": "max",
        }
    models = {"providers": {"mock": {
        "name": "Mock",
        "baseUrl": f"http://127.0.0.1:{port}/v1",
        "api": "openai-completions",
        "apiKey": "sk-mock-body-probe",
        "models": [model],
    }}}
    with open(os.path.join(agent, "models.json"), "w", encoding="utf-8") as fh:
        json.dump(models, fh, indent=2)
    settings = {"defaultProvider": "mock", "defaultModel": "mock-model",
                "defaultThinkingLevel": "low"}
    with open(os.path.join(agent, "settings.json"), "w", encoding="utf-8") as fh:
        json.dump(settings, fh, indent=2)
    return agent


def _sha(text: str) -> str:
    import hashlib
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


def summarize(body: dict) -> dict:
    tools = body.get("tools") or []
    messages = body.get("messages") or []
    system_text = ""
    for message in messages:
        if message.get("role") == "system":
            content = message.get("content")
            system_text = content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)
            break
    for key in ("instructions", "system"):
        if not system_text and isinstance(body.get(key), str):
            system_text = body[key]
    return {
        "prefixHash": _sha(json.dumps(tools, ensure_ascii=False) + "\u0000" + system_text),
        "systemHash": _sha(system_text),
        "toolsHash": _sha(json.dumps(tools, ensure_ascii=False)),
        "totalBodyBytes": len(json.dumps(body, ensure_ascii=False).encode("utf-8")),
        "toolCount": len(tools),
        "toolNames": [t.get("function", {}).get("name") or t.get("name") for t in tools],
        "toolsBytes": len(json.dumps(tools, ensure_ascii=False).encode("utf-8")) if tools else 0,
        "systemPromptBytes": len(system_text.encode("utf-8")),
        "messageCount": len(messages),
        "messageBytes": len(json.dumps(messages, ensure_ascii=False).encode("utf-8")),
        "reasoningEffort": body.get("reasoning_effort"),
        "otherKeys": sorted(k for k in body.keys() if k not in ("model", "messages", "tools", "stream")),
    }


def run_cli(cli: str, label: str, prompt: str, port: int, timeout: float,
            reasoning: bool = False, workdir: str = ""):
    agent = build_agent_dir(port, reasoning)
    env = dict(os.environ)
    env["PIR_AGENT_DIR"] = agent
    env["PI_CODING_AGENT_DIR"] = agent
    is_cmd = os.name == "nt" and cli.lower().endswith((".cmd", ".bat"))
    program = (os.environ.get("ComSpec") or "cmd.exe") if is_cmd else cli
    args = ["--mode", "json", "--print", "--model", "mock/mock-model", "--thinking", "low", prompt]
    argv = (["/d", "/c", cli] + args) if is_cmd else args
    out_dir = os.path.join(REPO, "logs", "perf", "body-probe")
    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, f"{label}.stdout.txt")
    err_path = os.path.join(out_dir, f"{label}.stderr.txt")
    cwd = workdir or agent
    if workdir and not os.path.isdir(workdir):
        os.makedirs(workdir, exist_ok=True)
    started = time.time()
    with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
        proc = subprocess.Popen([program] + argv, cwd=cwd, env=env,
                                stdin=subprocess.DEVNULL, stdout=out_fh, stderr=err_fh)
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
    shutil.rmtree(agent, ignore_errors=True)
    return {"label": label, "exit": proc.returncode, "wallS": round(time.time() - started, 2),
            "stderrTail": open(err_path, "r", encoding="utf-8", errors="replace").read().strip().splitlines()[-2:]}


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target-alt", "release", "pir.exe"))
    ap.add_argument("--pi-cli", default=os.path.join(os.environ.get("APPDATA", ""), "npm", "pi.cmd"))
    ap.add_argument("--prompt", default=PROMPT)
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "perf", "body-probe", "summary.json"))
    ap.add_argument("--only", default="", help="pir | pi (default: both)")
    ap.add_argument("--reasoning", action="store_true",
                    help="mock model declares reasoning=true + the openai thinkingLevelMap, "
                         "so the --thinking low wire mapping can be compared")
    ap.add_argument("--multi-turn", type=int, default=0, metavar="N",
                    help="mock answers the first N requests with a bash tool call, forcing "
                         "follow-up requests; per-request prefix hashes are printed")
    ap.add_argument("--workdir", default="",
                    help="cwd for the CLI (default: the temp agent dir)")
    ap.add_argument("--dump-bodies", action="store_true",
                    help="write every captured request body next to the summary")
    args = ap.parse_args(argv)

    global TOOL_CALLS, REQUESTS
    TOOL_CALLS = max(0, args.multi_turn)
    server = Mock(("127.0.0.1", 0), _Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    targets = []
    if args.only != "pi":
        targets.append(("pir", args.pir))
    if args.only != "pir":
        targets.append(("pi", args.pi_cli))

    results = []
    for label, cli in targets:
        if not os.path.exists(cli):
            print(f"ERROR: {label} CLI not found: {cli}", file=sys.stderr)
            return 2
        BODIES.clear()
        REQUESTS = 0
        run = run_cli(cli, label, args.prompt, port, args.timeout,
                      reasoning=args.reasoning, workdir=args.workdir)
        per_request = [summarize(body) for body in BODIES]
        first = per_request[0] if per_request else None
        request_count = len(BODIES)
        if args.dump_bodies:
            dump_dir = os.path.join(os.path.dirname(args.out), "bodies")
            os.makedirs(dump_dir, exist_ok=True)
            for index, body in enumerate(BODIES):
                with open(os.path.join(dump_dir, f"{label}-{index}.json"), "w", encoding="utf-8") as fh:
                    json.dump(body, fh, indent=2, ensure_ascii=False)
        results.append({"run": run, "requests": request_count, "first": first,
                        "perRequest": per_request})
        print(f"[body-probe] {label}: exit={run['exit']} wall={run['wallS']}s requests={request_count}")
        if first:
            print(f"    tools={first['toolCount']} toolsBytes={first['toolsBytes']} "
                  f"systemBytes={first['systemPromptBytes']} messages={first['messageCount']} "
                  f"bodyBytes={first['totalBodyBytes']} reasoning={first['reasoningEffort']} "
                  f"prefixHash={first['prefixHash']}")
            print(f"    toolNames={first['toolNames']}")
            print(f"    otherKeys={first['otherKeys']}")
        for index, row in enumerate(per_request):
            print(f"    req{index}: msgs={row['messageCount']} bodyBytes={row['totalBodyBytes']} "
                  f"prefixHash={row['prefixHash']} systemHash={row['systemHash']} "
                  f"toolsHash={row['toolsHash']}")

    server.shutdown()
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump({"prompt": args.prompt, "results": results}, fh, indent=2)
    print(f"[body-probe] summary: {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
