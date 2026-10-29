#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Hermetic OpenAI-compatible mock LLM server for the TUI battery (stdlib only).

Why: the model round-trip scenarios (s08_session_info / s08b_session_cost)
need a provider, but must not touch a real gateway. This server implements the
minimal surface pir's `openai-completions` adapter uses (see
src/ai/api/openai/mod.rs + sse.rs):

  * GET  /v1/models            -> any list: {"object":"list","data":[...]}
  * POST /v1/chat/completions
      - stream=true (SSE, text/event-stream), exactly the shape the adapter
        parses:
          data: {"id":...,"object":"chat.completion.chunk","created":...,
                 "model":<requested>,"choices":[{"index":0,
                 "delta":{"role":"assistant","content":""},"finish_reason":null}]}
          data: {... "delta":{"content":"mock reply"},"finish_reason":null}
          data: {... "delta":{},"finish_reason":"stop",
                 "usage":{"prompt_tokens":1234,"completion_tokens":2,
                          "total_tokens":1236}}
          data: [DONE]
        (the adapter reads `usage` from the chunk carrying it and maps
        finish_reason "stop" -> StopReason::Stop; "mock reply" is the fixed
        assistant text the scenarios are calibrated against)
      - stream missing/false: one non-streaming chat.completion JSON.

SCRIPTED RESPONSES (optional, additive): `MockServer(..., script=[...])`
serves a programmatic conversation instead of the fixed reply. One step per
`POST /v1/chat/completions` request, consumed in order; the LAST step repeats.
A step is one assistant message:

    {"text": "..."}                                  -> text reply
    {"tool_call":  {"name": "bash",
                    "arguments": {"command": "sleep 5",
                                  "background": true}}}        -> one call
    {"tool_calls": [ {...}, {...} ]}                 -> parallel calls

`arguments` is a dict (serialized to the JSON string the wire carries) or a
pre-serialized string; `id` is optional. `script=None` (the default) keeps the
old fixed-`mock reply` behaviour EXACTLY. The tool-call SSE shape is the one
`src/ai/api/openai/sse.rs` parses: chunks carry
`choices[0].delta.tool_calls = [{index, id, type: "function",
function: {name, arguments}}]`, the arguments string is split mid-JSON across
two chunks (the adapter accumulates fragments per wire index, sse.rs:434), and
the final chunk carries `finish_reason: "tool_calls"` + `usage` (sse.rs:226
reads the reason; `map_stop` maps "tool_calls" -> StopReason::ToolUse).

The server binds 127.0.0.1 with port 0 (OS-assigned free port) and is meant
to be embedded by run_battery.py as a daemon thread:

    from mock_llm import MockServer
    srv = MockServer(log_path=...)      # starts serving immediately
    srv = MockServer(log_path=..., script=[...])   # scripted conversation
    port = srv.port                     # http://127.0.0.1:<port>/v1
    ...
    srv.stop()

Every request is summarized as one JSONL line in `log_path` (method, path,
stream flag, requested model, message count, first user text, auth header
presence) so the battery report can prove the app really hit the mock; with a
script the line additionally carries `script_step` (the step index served).

Standalone use for manual probing:

    python mock_llm.py [--port 0] [--log-file FILE]
    # prints the bound port, then serves until Ctrl+C
"""
import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MOCK_REPLY_TEXT = "mock reply"
MOCK_USAGE = {"prompt_tokens": 1234, "completion_tokens": 2, "total_tokens": 1236}
# finish_reason of a scripted tool-call message; src/ai/api/openai/sse.rs
# `map_stop` maps it to StopReason::ToolUse (the agent loop then executes the
# batch and issues the follow-up request with the tool results).
MOCK_TOOL_CALL_FINISH = "tool_calls"
MAX_BODY_BYTES = 64 * 1024 * 1024  # cap request bodies so a stray upload cannot OOM


def _sse_event(payload):
    return b"data: " + json.dumps(payload, separators=(",", ":")).encode("utf-8") + b"\n\n"


def _chunk(completion_id, created, model, delta, finish_reason):
    """One `chat.completion.chunk` with the adapter's expected envelope
    (sse.rs reads `choices[0].delta` / `choices[0].finish_reason`)."""
    return {
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    }


def _split_arguments(text):
    """Split an arguments JSON string into two wire fragments (mid-string is
    fine: a provider streams arbitrary token boundaries and the adapter only
    concatenates, then parses/repairs at block end)."""
    if len(text) < 2:
        return text, ""
    half = len(text) // 2
    return text[:half], text[half:]


def _normalize_script(script):
    """Validate a MockServer script into `[("text", str) | ("tool_calls",
    [call, ...]), ...]`; `None`/empty -> `None` (the fixed-reply default).

    Raises ValueError on a malformed step, so a bad scenario script fails that
    scenario loudly (run_battery records it as a battery error) instead of
    silently serving the wrong conversation. Deterministic default ids keep
    re-runs comparable.
    """
    if not script:
        return None
    steps = []
    for i, step in enumerate(script):
        if not isinstance(step, dict):
            raise ValueError(f"script[{i}] must be a dict, got {type(step).__name__}")
        if "text" in step and ("tool_call" in step or "tool_calls" in step):
            raise ValueError(f"script[{i}]: 'text' and 'tool_call(s)' are exclusive")
        if "text" in step:
            text = step["text"]
            if not isinstance(text, str):
                raise ValueError(f"script[{i}].text must be a string")
            chunks = step.get("chunks")
            if chunks is not None and (not isinstance(chunks, int) or chunks < 1):
                raise ValueError(f"script[{i}].chunks must be a positive int")
            delay = step.get("chunk_delay_ms", 0)
            if not isinstance(delay, (int, float)) or delay < 0:
                raise ValueError(f"script[{i}].chunk_delay_ms must be >= 0")
            # `chunks > 1` splits the text into that many content deltas (with
            # optional inter-chunk pacing), so a scenario can emulate a real
            # token stream for latency probes.
            steps.append(("text", text, chunks or 1, float(delay)))
            continue
        if "tool_call" in step and "tool_calls" in step:
            raise ValueError(f"script[{i}]: use 'tool_call' or 'tool_calls', not both")
        raw_calls = step.get("tool_calls", step.get("tool_call"))
        if raw_calls is None:
            raise ValueError(f"script[{i}] needs 'text', 'tool_call' or 'tool_calls'")
        if isinstance(raw_calls, dict):
            raw_calls = [raw_calls]
        if not isinstance(raw_calls, list) or not raw_calls:
            raise ValueError(f"script[{i}].tool_calls must be a non-empty list/dict")
        calls = []
        for j, call in enumerate(raw_calls):
            if not isinstance(call, dict):
                raise ValueError(f"script[{i}].tool_calls[{j}] must be a dict")
            name = call.get("name")
            if not isinstance(name, str) or not name:
                raise ValueError(f"script[{i}].tool_calls[{j}] needs a non-empty name")
            arguments = call.get("arguments", {})
            if isinstance(arguments, str):
                arguments_text = arguments
            else:
                arguments_text = json.dumps(arguments, separators=(",", ":"))
            call_id = call.get("id")
            if not isinstance(call_id, str) or not call_id:
                call_id = f"call_mock_{i}_{j}"
            calls.append({"id": call_id, "name": name, "arguments": arguments_text})
        steps.append(("tool_calls", calls))
    return steps


class _Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 + explicit Connection: close on streams => the client sees the
    # body end on EOF (the streaming adapter reads until the socket closes).
    protocol_version = "HTTP/1.1"
    server_version = "pir-mock-llm/1.0"

    # -- helpers -----------------------------------------------------------
    @property
    def state(self):
        return self.server.mock_state  # type: ignore[attr-defined]

    def log_message(self, fmt, *args):  # silence the default stderr chatter
        pass

    def _read_json_body(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0:
            return {}
        if length > MAX_BODY_BYTES:
            return {}
        raw = self.rfile.read(length)
        try:
            return json.loads(raw.decode("utf-8", errors="replace"))
        except ValueError:
            return {}

    def _record(self, path, body, stream, script_step=None):
        first_user = ""
        messages = body.get("messages") if isinstance(body, dict) else None
        if isinstance(messages, list):
            for message in messages:
                if isinstance(message, dict) and message.get("role") == "user":
                    content = message.get("content")
                    if isinstance(content, str):
                        first_user = content
                    elif isinstance(content, list):
                        parts = [p.get("text", "") for p in content
                                 if isinstance(p, dict) and p.get("type") == "text"]
                        first_user = " ".join(parts)
                    break
        entry = {
            "ts": round(time.time(), 3),
            "method": self.command,
            "path": path,
            "stream": stream,
            "model": (body.get("model") if isinstance(body, dict) else None),
            "messages": len(messages) if isinstance(messages, list) else None,
            "authorization": bool(self.headers.get("Authorization")),
            "first_user": first_user[:200],
        }
        if script_step is not None:  # only when a script is active (default log unchanged)
            entry["script_step"] = script_step
        with self.state.lock:
            self.state.request_count += 1
            self.state.record(entry)

    def _next_step(self):
        """Pick the script step for one chat-completions request: steps are
        consumed in order and the last one repeats. `(step, index)`, or
        `(None, None)` when no script is configured (fixed `mock reply`)."""
        with self.state.lock:
            steps = self.state.script
            if not steps:
                return None, None
            index = min(self.state.script_index, len(steps) - 1)
            self.state.script_index += 1
            return steps[index], index

    def _send_json(self, status, payload):
        data = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass

    # -- endpoints ---------------------------------------------------------
    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.rstrip("/").endswith("/models"):
            self._record(path, {}, False)
            self._send_json(200, {
                "object": "list",
                "data": [
                    {"id": "mock-model", "object": "model", "owned_by": "pir-battery"},
                    {"id": "gpt-5", "object": "model", "owned_by": "pir-battery"},
                ],
            })
            return
        self._send_json(404, {"error": {"message": f"mock: no route {path}"}})

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        if not path.rstrip("/").endswith("/chat/completions"):
            self._send_json(404, {"error": {"message": f"mock: no route {path}"}})
            return
        body = self._read_json_body()
        stream = bool(body.get("stream")) if isinstance(body, dict) else False
        step, step_index = self._next_step()
        self._record(path, body, stream, step_index)
        model = (body.get("model") if isinstance(body, dict) else None) or "mock-model"
        completion_id = f"chatcmpl-mock-{self.state.request_count}"
        created = int(time.time())
        # No script -> the fixed text reply (byte-identical to the pre-script
        # mock); a scripted step -> its text or tool call(s).
        kind, payload = (step[0], step[1]) if step else ("text", MOCK_REPLY_TEXT)
        step_chunks = step[2] if step and step[0] == "text" else 1
        step_delay = step[3] if (step and step[0] == "text" and len(step) > 3) else 0.0
        if kind == "text":
            text = payload
            if not stream:
                self._send_json(200, {
                    "id": completion_id,
                    "object": "chat.completion",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": text},
                        "finish_reason": "stop",
                    }],
                    "usage": dict(MOCK_USAGE),
                })
                return
            events = [
                _sse_event(_chunk(completion_id, created, model,
                                  {"role": "assistant", "content": ""}, None)),
            ]
            if step_chunks <= 1:
                events.append(_sse_event(_chunk(completion_id, created, model,
                                                {"content": text}, None)))
            else:
                n = len(text)
                per = max(1, -(-n // step_chunks))
                for start in range(0, n, per):
                    events.append(_sse_event(_chunk(completion_id, created, model,
                                                    {"content": text[start:start + per]}, None)))
            final = _chunk(completion_id, created, model, {}, "stop")
            final["usage"] = dict(MOCK_USAGE)
            events += [_sse_event(final), b"data: [DONE]\n\n"]
        else:
            calls = payload
            if not stream:
                self._send_json(200, {
                    "id": completion_id,
                    "object": "chat.completion",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": None,
                            "tool_calls": [
                                {"id": call["id"], "type": "function",
                                 "function": {"name": call["name"],
                                              "arguments": call["arguments"]}}
                                for call in calls
                            ],
                        },
                        "finish_reason": MOCK_TOOL_CALL_FINISH,
                    }],
                    "usage": dict(MOCK_USAGE),
                })
                return
            # --- SSE: scripted tool call(s) -------------------------------
            events = [_sse_event(_chunk(completion_id, created, model,
                                        {"role": "assistant", "content": ""}, None))]
            for index, call in enumerate(calls):
                # Arguments arrive as fragments split across chunks; the
                # adapter keys blocks by `index`, appends each `arguments`
                # fragment (sse.rs:434) and takes id/name off the chunks that
                # carry them.
                first, second = _split_arguments(call["arguments"])
                events.append(_sse_event(_chunk(completion_id, created, model, {
                    "tool_calls": [{
                        "index": index,
                        "id": call["id"],
                        "type": "function",
                        "function": {"name": call["name"], "arguments": first},
                    }],
                }, None)))
                if second:
                    events.append(_sse_event(_chunk(completion_id, created, model, {
                        "tool_calls": [{"index": index,
                                        "function": {"arguments": second}}],
                    }, None)))
            final = _chunk(completion_id, created, model, {}, MOCK_TOOL_CALL_FINISH)
            final["usage"] = dict(MOCK_USAGE)
            events += [_sse_event(final), b"data: [DONE]\n\n"]

        # --- SSE streaming ------------------------------------------------
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        # Optional inter-chunk pacing (script step `chunk_delay_ms`): a real
        # token stream, so an app that cannot keep up shows a growing backlog
        # instead of measuring the socket's own burst rate.
        pace = step_delay if (step and step[0] == "text" and step_chunks > 1) else 0.0
        content_first, content_last = 2, len(events) - 3
        try:
            for index, event in enumerate(events):
                if pace > 0 and content_first <= index <= content_last:
                    time.sleep(pace / 1000.0)
                self.wfile.write(event)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass  # client aborted (esc/ctrl+c) - nothing to clean up


class _MockState:
    def __init__(self, log_path, script=None):
        self.lock = threading.Lock()
        self.request_count = 0
        self.script = script or []  # normalized steps; [] = fixed mock reply
        self.script_index = 0
        self.log_path = log_path
        self._log_fh = None
        if log_path:
            self._log_fh = open(log_path, "w", encoding="utf-8", buffering=1)

    def record(self, entry):
        if self._log_fh is None:
            return
        try:
            self._log_fh.write(json.dumps(entry, ensure_ascii=False) + "\n")
        except OSError:
            pass

    def close(self):
        if self._log_fh is not None:
            try:
                self._log_fh.close()
            except OSError:
                pass
            self._log_fh = None


class MockServer:
    """Daemon-thread OpenAI-compatible mock. `port` is live after __init__.

    `script` (optional): the scripted conversation documented at the top of
    the module — one step per `/v1/chat/completions` request, the last step
    repeats. `None` keeps the fixed-`mock reply` behaviour exactly."""

    def __init__(self, host="127.0.0.1", port=0, log_path=None, script=None):
        self._state = _MockState(log_path, _normalize_script(script))
        self._httpd = ThreadingHTTPServer((host, port), _Handler)
        self._httpd.daemon_threads = True
        self._httpd.mock_state = self._state  # type: ignore[attr-defined]
        self.host = host
        self.port = self._httpd.server_address[1]
        self._thread = threading.Thread(
            target=self._httpd.serve_forever, name="mock-llm", daemon=True)
        self._thread.start()

    @property
    def base_url(self):
        """Exactly what goes into the injected models.json `baseUrl`."""
        return f"http://{self.host}:{self.port}/v1"

    @property
    def request_count(self):
        with self._state.lock:
            return self._state.request_count

    def stop(self):
        try:
            self._httpd.shutdown()
        finally:
            self._httpd.server_close()
            self._thread.join(timeout=5)
            self._state.close()


def main(argv=None):
    ap = argparse.ArgumentParser(description="OpenAI-compatible mock LLM (stdlib)")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=0, help="0 = random free port")
    ap.add_argument("--log-file", default=None, help="JSONL request log")
    args = ap.parse_args(argv)
    srv = MockServer(host=args.host, port=args.port, log_path=args.log_file)
    print(f"mock-llm listening on {srv.base_url} (Ctrl+C to stop)", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass
    finally:
        srv.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
