#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Minimal stdio MCP server for lane-tui round 50 (deferred tool_search).

No network: JSON-RPC over stdin/stdout. Three tools so a `deferred` server has
one searchable tool (`deploy_status`) and tools a no-match search must NOT
activate (`calendar_events`). Every request is appended to --log as JSONL for
the evidence audit.
"""
import argparse
import json
import sys

TOOLS = [
    ("deploy_status",
     "Show the deployment status of a production service: current release, "
     "rollout progress, health checks and recent deploy events.",
     {"type": "object",
      "properties": {"service": {"type": "string"},
                     "environment": {"type": "string"}},
      "additionalProperties": True}),
    ("calendar_events",
     "List upcoming calendar events for a team member.",
     {"type": "object",
      "properties": {"member": {"type": "string"}},
      "additionalProperties": True}),
    ("email_send",
     "Send a transactional email message to a customer address.",
     {"type": "object",
      "properties": {"to": {"type": "string"}, "subject": {"type": "string"}},
      "additionalProperties": True}),
]

LOG = None


def log(entry):
    if LOG is None:
        return
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")


def reply(request_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request_id,
                                 "result": result}) + "\n")
    sys.stdout.flush()


def main():
    global LOG
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default=None)
    args = ap.parse_args()
    LOG = args.log
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = message.get("method")
        log({"method": method, "id": message.get("id"),
             "params": message.get("params")})
        if method == "initialize":
            reply(message.get("id"), {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "round50-lab", "version": "1.0.0"},
            })
        elif method == "ping":
            reply(message.get("id"), {})
        elif method == "tools/list":
            reply(message.get("id"), {
                "tools": [{"name": name, "description": desc,
                           "inputSchema": schema}
                          for name, desc, schema in TOOLS]})
        elif method == "tools/call":
            params = message.get("params") or {}
            name = params.get("name")
            arguments = params.get("arguments") or {}
            if name == "deploy_status":
                text = ("MCP-DEPLOY-OK service={} environment={}".format(
                    arguments.get("service", "?"),
                    arguments.get("environment", "?")))
            elif name == "calendar_events":
                text = "MCP-CALENDAR-OK member={}".format(
                    arguments.get("member", "?"))
            elif name == "email_send":
                text = "MCP-EMAIL-OK to={}".format(arguments.get("to", "?"))
            else:
                reply(message.get("id"), {
                    "content": [{"type": "text",
                                 "text": "unknown tool: {}".format(name)}],
                    "isError": True})
                continue
            reply(message.get("id"), {
                "content": [{"type": "text", "text": text}], "isError": False})
        elif method == "resources/list":
            reply(message.get("id"), {"resources": []})
        elif method == "prompts/list":
            reply(message.get("id"), {"prompts": []})
        # notifications/initialized and other notifications carry no id.


if __name__ == "__main__":
    main()
