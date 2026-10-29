# -*- coding: utf-8 -*-
# W13-L task A part 1 — "fix a broken file, verify, and continue".
#
# REAL TASK, HERMETIC PROVIDER. The injected mock LLM (mock_llm.py) decides the
# tool calls; the TOOLS, the TUI rendering, the bash subprocess and the session
# persistence are all the real product paths. Five model steps:
#
#   1. write  -> create `w13_broken.json` in the harness work dir (the child
#                cwd == the per-scenario agent dir) with a DELIBERATE JSON
#                syntax error: a trailing comma before the closing brace.
#   2. read   -> read the fixture back (real read tool; `read w13_broken.json`).
#   3. edit   -> fix it with a real exact-text replacement (real edit tool,
#                real file on disk).
#   4. bash   -> a REAL python verification whose output is 41 lines: it
#                prints 24 progress lines, then json.load()s the file and
#                prints `PARSE-OK items=2`, then 16 more progress lines.
#                >= 40 lines is what makes the TUI collapse the block to the
#                last BASH_PREVIEW_LINES (=5) rows plus the
#                `... (36 earlier lines, ctrl+o to expand)` hint
#                (src/tui/messages.rs:3072, :3215).
#   5. text   -> W13-A1-DONE.
#
# Why the fixture is created by the agent's own `write` tool rather than by
# this module: the per-scenario agent dir / cwd path is only known at run time
# (`out/<run-id>/<NAME>/agent`), and a scenario module is exec'd before that
# dir exists. Creating it with the real write tool keeps the whole task inside
# the harness work dir with no absolute paths.
#
# PARSE-OK is deliberately at output line 25 of 41: the collapsed view shows
# only the last 5 rows, so it must be ABSENT before ctrl+o and PRESENT after —
# that is what makes the expansion assertion mean something. (It is 16 rows
# above the block end, so the expanded 40-row screen still shows it.)
#
# The on-disk outcomes (edited bytes + the session JSONL tool entries) are
# asserted by the module's `verify_on_disk` helper at the bottom of this file:
# the battery's check vocabulary is screen/log only, so the two on-disk
# assertions run in the DRIVER process (after the app has quit) via
# `--verify-script`, and record their verdict in `s44_ondisk.json` next to the
# other artifacts. See the W13 ConPTY lane logs under logs/w18-help/ for the recorded result.
NAME = "s44_real_task_fix_and_verify"
OFFLINE = False  # the app must reach the injected mock over HTTP
TIMEOUT = 180

# The broken fixture: invalid JSON (trailing comma after the last member).
_BROKEN = (
    '{\n'
    '  "name": "w13-fixture",\n'
    '  "version": 1,\n'
    '  "items": [\n'
    '    {"id": 1, "value": "alpha"},\n'
    '    {"id": 2, "value": "beta"}\n'
    '  ],\n'
    '  "enabled": true,\n'
    '}\n'
)
_OLD = '  "enabled": true,\n}'
_NEW = '  "enabled": true\n}'

# 41 output lines with the parse result at line 25 (see header). The shell
# quoting is single-quote-outer so the embedded double quotes need no escapes;
# the marker itself is CONCATENATED in the source (`"PARSE-" + "OK ..."`) so the
# echoed call line (`$ <command>`, format_tool_call_line) cannot contain the
# contiguous string the expansion check looks for — the assertion can then only
# be satisfied by real program OUTPUT.
_VERIFY = (
    "python -c '"
    "import json\n"
    'for i in range(1, 25): print("case %02d: fixture item check" % i)\n'
    'd = json.load(open("w13_broken.json"))\n'
    'print("PARSE-" + "OK items=%d" % len(d["items"]))\n'
    'for i in range(25, 41): print("case %02d: trailing check" % i)\n'
    "'"
)

MOCK_SCRIPT = [
    {"tool_call": {"name": "write",
                   "arguments": {"path": "w13_broken.json", "content": _BROKEN}}},
    {"tool_call": {"name": "read",
                   "arguments": {"path": "w13_broken.json"}}},
    {"tool_call": {"name": "edit",
                   "arguments": {"path": "w13_broken.json",
                                 "edits": [{"oldText": _OLD, "newText": _NEW}]}}},
    {"tool_call": {"name": "bash",
                   "arguments": {"command": _VERIFY}}},
    {"text": "W13-A1-DONE"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s44_boot"),
    ("log", "prompt -> mock: write fixture, read it, edit the fix, bash verify, text"),
    ("send", b"Fix the broken fixture, verify it, then tell me what you did.\r"),
    # The tool rounds complete in ~1-3 s (local mock); the bash step is a real
    # python spawn. wait_quiet then settles on the final text turn.
    ("wait", 8.0),
    ("wait_quiet", 3.0),
    ("snap", "s44_tools_done"),
    ("log", "ctrl+home: the transcript head (write/read call headers)"),
    ("send", b"\x1b[1;5H"),
    ("wait", 1.2),
    ("snap", "s44_head"),
    ("log", "ctrl+end: back to the transcript end"),
    ("send", b"\x1b[1;5F"),
    ("wait", 1.2),
    ("snap", "s44_tail"),
    ("log", "ctrl+o expands the collapsed 41-line bash output"),
    ("send", b"\x0f"),
    ("wait", 1.5),
    ("wait_quiet", 1.5),
    ("snap", "s44_expanded"),
]

CHECKS = [
    # control: nothing from the task before the prompt
    ("screen_not_contains", "s44_boot", "PARSE-OK"),
    ("screen_not_contains", "s44_boot", "W13-A1-DONE"),
    # the transcript END after all five model steps: the last tool call's
    # result (the 41-line bash block, COLLAPSED) and the model's final text.
    # The 40-row screen cannot hold the whole transcript, so the call headers
    # at its head are read in the s44_head snapshot below (same rule s39 uses).
    ("screen_contains", "s44_tools_done", "edit w13_broken.json"),
    ("screen_contains", "s44_tools_done", "W13-A1-DONE"),
    ("screen_contains", "s44_tools_done", "earlier lines"),
    ("screen_contains", "s44_tools_done", "to expand"),
    # the collapse is REAL: the parse marker is output line 25 of 41 and the
    # collapsed preview keeps only the last 5 rows
    ("screen_not_contains", "s44_tools_done", "PARSE-OK"),
    # transcript head (ctrl+home): the first two tool call headers
    ("screen_contains", "s44_head", "write w13_broken.json"),
    ("screen_contains", "s44_head", "read w13_broken.json"),
    # ctrl+end is back at the live end
    ("screen_contains", "s44_tail", "W13-A1-DONE"),
    # ctrl+o expands the block: the real verification marker becomes visible
    ("screen_contains", "s44_expanded", "PARSE-OK items=2"),
    # no error banner anywhere in the finished transcript
    ("screen_not_contains", "s44_expanded", "Error:"),
    ("screen_not_contains", "s44_expanded", "Could not find the exact text"),
    ("screen_not_contains", "s44_expanded", "command not found"),
    ("restore_markers",),
]


# ---------------------------------------------------------------------------
# On-disk verification (task requirement: "the edit landed on disk — check the
# file bytes from Python after the run" and "the session JSONL on disk contains
# the tool entries").
#
# run_battery.py's CHECKS vocabulary is screen/log only (CHECK_KINDS), and its
# scenario modules are plain data, so the two file-level assertions live here as
# a helper the reporter (or a later lane) can call:
#
#   python -c "import runpy,sys; m=runpy.run_path('scenarios/s44_...py'); \
#              print(m['verify_on_disk']('<artifacts dir>'))"
#
# It returns a dict of assertion name -> (bool, detail) and never raises, so it
# is safe to call from a verification script.
# ---------------------------------------------------------------------------
import glob as _glob
import json as _json
import os as _os


def _long(path):
    """Verbatim Win32 path: a battery session path is ~300 chars and this host
    has LongPathsEnabled=0 (the same reason run_battery._win_long_path exists)."""
    absolute = _os.path.abspath(path)
    if _os.name != "nt" or absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def verify_on_disk(scen_dir):
    """Assert the REAL file outcomes of the s44 task inside `scen_dir`.

    Returns {name: (ok, detail)}:
      edited_file_valid   the fixture parses as JSON and still has the original
                          two items (i.e. the edit fixed the comma, not the data)
      edited_file_no_comma the trailing comma is gone from the bytes on disk
      session_tool_entries the session JSONL holds the four tool call entries
                          (write/read/edit/bash) plus their toolResult messages
    """
    out = {}
    agent = _os.path.join(scen_dir, "agent")
    fixture = _os.path.join(agent, "w13_broken.json")
    try:
        raw = open(_long(fixture), "rb").read().decode("utf-8")
    except OSError as exc:
        return {"edited_file_valid": (False, f"cannot read {fixture}: {exc}")}
    try:
        data = _json.loads(raw)
        ok = (isinstance(data, dict) and data.get("enabled") is True
              and len(data.get("items") or []) == 2)
        out["edited_file_valid"] = (ok, f"parsed: {sorted(data) if isinstance(data, dict) else data!r}")
    except ValueError as exc:
        out["edited_file_valid"] = (False, f"still invalid JSON: {exc}")
    out["edited_file_no_comma"] = (
        '"enabled": true,\n}' not in raw,
        "trailing comma removed" if '"enabled": true,\n}' not in raw
        else "trailing comma STILL on disk")

    entries, messages = [], 0
    pattern = _os.path.join(agent, "sessions", "*", "*.jsonl")
    files = _glob.glob(pattern)
    for path in files:
        try:
            text = open(_long(path), encoding="utf-8").read()
        except OSError:
            continue
        for line in text.splitlines():
            try:
                entry = _json.loads(line)
            except ValueError:
                continue
            if entry.get("type") != "message":
                continue
            message = entry.get("message") or {}
            content = message.get("content")
            if message.get("role") == "assistant" and isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "toolCall":
                        entries.append(block.get("name"))
            elif message.get("role") == "toolResult":
                messages += 1
    want = ["write", "read", "edit", "bash"]
    out["session_tool_entries"] = (
        entries == want and messages == len(want),
        f"tool calls {entries}, toolResult messages {messages} "
        f"in {len(files)} session file(s)")
    return out
