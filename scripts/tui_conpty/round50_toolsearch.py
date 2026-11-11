# -*- coding: utf-8 -*-
"""Round 50 lane-tui: real-TUI `tool_search` over a local stdio MCP server.

Journey: a project `.pi/mcp.json` declares one server with
`exposure: "deferred"`. The MCP builtin auto-activates `tool_search`. One
scripted prompt then:
  1. `tool_search` finds `mcp__lab__deploy_status`;
  2. the very next request declares it and the call reaches the real MCP
     server (`MCP-DEPLOY-OK`);
  3. a no-match search returns `No matching tools found.` and loads nothing;
  4. `mcp__lab__calendar_events` is still undeclared -> `not found`;
  5. final text.
No network: mock LLM + mock MCP are both loopback/local stdio.
"""
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import acc3_render  # noqa: E402
import mock_llm  # noqa: E402
import tui_drive  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from run_battery import seed_agent_dir, inject_mock_models  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round50-toolsearch")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))
MOCK_MCP = os.path.join(HERE, "mock_mcp_deferred.py")

# Capture the declared tool names per chat request (mock_llm logs only message
# counts). This is the direct proof of "not declared until loaded".
DEFAULT_RECORD = mock_llm._Handler._record
TOOLS_LOG = None


def record_with_tools(self, path, body, stream, script_step=None):
    DEFAULT_RECORD(self, path, body, stream, script_step)
    if TOOLS_LOG and path.rstrip("/").endswith("/chat/completions"):
        names = []
        if isinstance(body, dict):
            for tool in body.get("tools") or []:
                if isinstance(tool, dict):
                    names.append((tool.get("function") or {}).get("name"))
        with open(TOOLS_LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps({"script_step": script_step,
                                "tools": names}, ensure_ascii=False) + "\n")


def collect_jsonl(agent):
    files = []
    for base, _, names in os.walk(os.path.join(agent, "sessions")):
        for name in names:
            if name.endswith(".jsonl") and not name.startswith("."):
                files.append(os.path.join(base, name))
    files.sort(key=os.path.getmtime)
    return files[-1] if files else None


def audit_jsonl(path):
    calls, results = [], []
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        message = entry.get("message") or {}
        role = message.get("role")
        if role == "assistant":
            for block in message.get("content") or []:
                if block.get("type") == "toolCall":
                    calls.append({"name": block.get("name"),
                                  "arguments": block.get("arguments")})
        elif role == "toolResult":
            text = "\n".join(b.get("text", "") for b in
                             message.get("content") or []
                             if isinstance(b, dict))
            results.append({"name": message.get("toolName"),
                            "text": text,
                            "isError": message.get("isError")})
        elif role == "assistant":
            pass
    return calls, results


def main():
    global TOOLS_LOG
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(os.path.join(ROOT, "shots"), exist_ok=True)
    project = os.path.join(ROOT, "project")
    agent = os.path.join(ROOT, "home", ".pi", "agent")
    os.makedirs(os.path.join(project, ".pi"), exist_ok=True)
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)

    mcp_log = os.path.join(ROOT, "mock_mcp_requests.jsonl")
    TOOLS_LOG = os.path.join(ROOT, "llm_tools_by_request.jsonl")
    mcp_json = {"mcpServers": {"lab": {
        "type": "stdio",
        "command": sys.executable.replace("\\", "/"),
        "args": [MOCK_MCP.replace("\\", "/"), "--log", mcp_log.replace("\\", "/")],
        "exposure": "deferred",
    }}}
    with open(os.path.join(project, ".pi", "mcp.json"), "w", encoding="utf-8") as f:
        json.dump(mcp_json, f, indent=2)

    script = [
        {"tool_call": {"name": "tool_search", "arguments": {
            "query": "deployment status rollout health"}}},
        {"tool_call": {"name": "mcp__lab__deploy_status", "arguments": {
            "service": "api", "environment": "prod"}}},
        {"tool_call": {"name": "tool_search", "arguments": {
            "query": "zebra quantumflux nonexistent"}}},
        {"tool_call": {"name": "mcp__lab__calendar_events", "arguments": {
            "member": "nobody"}}},
        {"text": "R50-TOOLSEARCH-OK"},
    ]
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"), script=script)
    inject_mock_models(agent, f"http://127.0.0.1:{mock.port}/v1")
    env = {"PIR_TRUST_PROJECT": "1", "HOME": os.path.join(ROOT, "home"),
           "USERPROFILE": os.path.join(ROOT, "home"), "PIR_AGENT_DIR": agent}
    tui_drive.PIR = PIR
    shots = os.path.join(ROOT, "shots")
    result_json = os.path.join(shots, "toolsearch.result.json")
    mock_llm._Handler._record = record_with_tools
    try:
        tui_drive.run_scenario(
            "toolsearch", [
                ("wait_quiet", 6.0), ("snap", "q_boot"),
                ("log", "one prompt: tool_search -> deferred call -> no-match -> unactivated call"),
                ("send", b"R50 tool search please\r"), ("wait", 4.0),
                ("wait_quiet", 8.0), ("snap", "q_turn"),
                ("send", b"/mcp\r"), ("wait", 2.0), ("wait_quiet", 3.0),
                ("snap", "q_mcp"),
            ], extra_env=env, cwd=project, shots_dir=shots,
            driver_log=os.path.join(shots, "toolsearch.driver.log"),
            result_json=result_json, agent_dir=agent, timeout=120)
    finally:
        mock_llm._Handler._record = DEFAULT_RECORD
        mock.stop()

    raw = open(os.path.join(shots, "q_turn.log"), "rb").read()
    rows = acc3_render.render(raw)
    text = "\n".join(rows)
    mcp_raw = open(os.path.join(shots, "q_mcp.log"), "rb").read()
    mcp_rows = acc3_render.render(mcp_raw)
    mcp_text = "\n".join(mcp_rows)

    with open(result_json, encoding="utf-8") as f:
        out = json.load(f)
    jsonl = collect_jsonl(agent)
    calls, results = audit_jsonl(jsonl)
    by_name = {}
    for item in results:
        by_name.setdefault(item["name"], []).append(item)
    tool_search_results = by_name.get("tool_search", [])
    deploy_results = by_name.get("mcp__lab__deploy_status", [])
    calendar_results = by_name.get("mcp__lab__calendar_events", [])

    tool_reqs = []
    if os.path.exists(TOOLS_LOG):
        tool_reqs = [json.loads(line) for line in open(TOOLS_LOG, encoding="utf-8")
                     if line.strip()]

    checks = {
        "llm:tool_search-declared-request0": {
            "ok": bool(tool_reqs) and "tool_search" in (tool_reqs[0]["tools"] or []),
            "tools": tool_reqs[0]["tools"] if tool_reqs else None},
        "llm:deferred-not-declared-request0": {
            "ok": bool(tool_reqs) and "mcp__lab__deploy_status" not in
            (tool_reqs[0]["tools"] or [])},
        "llm:deferred-declared-request1": {
            "ok": len(tool_reqs) > 1 and "mcp__lab__deploy_status" in
            (tool_reqs[1]["tools"] or []),
            "tools": tool_reqs[1]["tools"] if len(tool_reqs) > 1 else None},
        "llm:calendar-never-declared": {
            "ok": all("mcp__lab__calendar_events" not in (r["tools"] or [])
                      for r in tool_reqs)},
        "jsonl:tool_search-loaded": {
            "ok": bool(tool_search_results) and
            "Loaded 1 tool" in tool_search_results[0]["text"] and
            "mcp__lab__deploy_status" in tool_search_results[0]["text"],
            "text": tool_search_results[0]["text"][:300] if tool_search_results else None},
        "jsonl:deferred-call-ok": {
            "ok": bool(deploy_results) and not deploy_results[0]["isError"] and
            "MCP-DEPLOY-OK service=api environment=prod" in deploy_results[0]["text"],
            "text": deploy_results[0]["text"][:300] if deploy_results else None},
        "jsonl:no-match": {
            "ok": len(tool_search_results) > 1 and
            "No matching tools found." in tool_search_results[1]["text"],
            "text": tool_search_results[1]["text"][:300] if len(tool_search_results) > 1 else None},
        "jsonl:calendar-not-found": {
            "ok": bool(calendar_results) and calendar_results[0]["isError"] and
            "not found" in calendar_results[0]["text"].lower(),
            "text": calendar_results[0]["text"][:300] if calendar_results else None},
        "jsonl:final": {"ok": "R50-TOOLSEARCH-OK" in text},
        "tui:deploy-card": {"ok": "MCP-DEPLOY-OK" in text and "deploy_status" in text},
        "tui:no-match-card": {"ok": "No matching tools found" in text},
        "tui:mcp-command": {
            "ok": "lab" in mcp_text and "deferred" in mcp_text.lower(),
            "rows": [r for r in mcp_rows if "lab" in r or "deferred" in r.lower()][:10]},
        "tui:no-mojibake": {"ok": raw.count(b"\xef\xbf\xbd") == 0 and
                            mcp_raw.count(b"\xef\xbf\xbd") == 0},
        "mcp_server:tools-list": {
            "ok": any(e.get("method") == "tools/list" and e.get("id") is not None
                      for e in [json.loads(line) for line in open(mcp_log, encoding="utf-8")
                                 if line.strip()]) if os.path.exists(mcp_log) else False},
        "mcp_server:deploy-called": {
            "ok": any(e.get("method") == "tools/call" and
                      (e.get("params") or {}).get("name") == "deploy_status"
                      for e in [json.loads(line) for line in open(mcp_log, encoding="utf-8")
                                 if line.strip()]) if os.path.exists(mcp_log) else False},
    }
    result = {"binary": PIR, "exit_code": out.get("exit_code"),
              "timed_out": out.get("timed_out"), "checks": checks,
              "session_jsonl": jsonl, "tool_requests": tool_reqs,
              "relevant_rows": [r for r in rows
                                if any(k in r for k in ("tool_search", "Loaded",
                                                        "No matching", "deploy_status",
                                                        "not found", "R50-"))]}
    with open(os.path.join(ROOT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    acc3_render.render_dir(shots)
    failed = [k for k, c in checks.items() if not c["ok"]]
    print(json.dumps({"ok": sum(1 for c in checks.values() if c["ok"]),
                      "total": len(checks), "failed": failed,
                      "exit": out.get("exit_code"),
                      "timed_out": out.get("timed_out")}, indent=2))
    return 0 if not failed else 1


if __name__ == "__main__":
    raise SystemExit(main())
