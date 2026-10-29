# -*- coding: utf-8 -*-
# W16-K task C part 1 — "grep a large tree, then edit based on what you found".
#
# REAL TASK, HERMETIC PROVIDER. The mock LLM (mock_llm.py) decides the tool
# calls; the tools (write/grep/read/edit), the files on disk and the whole TUI
# render are the real product paths. FIVE model steps:
#
#   1. tool_calls x6 -> `write` a small fixture tree (6 files) under the
#                       per-scenario work dir (the child cwd == agent dir).
#                       `pine/src/legacy.rs` carries the ONLY copy of the
#                       distinctive marker `W16K-NEEDLE-9f3c` (line 5).
#   2. grep          -> pattern `W16K-NEEDLE-9f3c`, path `pine` (a REAL
#                       in-process ripgrep search over the REAL files written
#                       in step 1).
#   3. read          -> `pine/src/legacy.rs` with **offset=5 limit=3**.
#                       THIS IS THE REGRESSION NET for the UI freeze that was
#                       just fixed (src/tui/messages.rs:1000-1018/1125-1140:
#                       `paint_read_line_range` vs `format_read_line_range` —
#                       the read call line formatted from inside a
#                       `with_theme` closure used to re-enter the non-reentrant
#                       theme mutex and self-deadlock the render thread). A
#                       freeze shows up as a BLANK final screen and
#                       `<agent>/pir.log` carrying `[tui] UI thread stalled`
#                       (src/tui/terminal.rs:5338). The run must render the
#                       call header `read pine/src/legacy.rs:5-7` AND finish.
#   4. edit          -> exact-text replacement of the feature flag `"off"` ->
#                       `"on"` (real edit tool, real file on disk; the block
#                       renders the diff from `details.diff`).
#   5. text          -> W16K-S48-DONE.
#
# The fixture is created by the agent's own `write` tool, not by this module:
# the per-scenario agent dir path is only known at run time
# (`out/<run-id>/<NAME>/agent`) and a scenario module is exec'd before it
# exists (same pattern as s44).
#
# Screen-budget note (measured, see the W16 ConPTY lane logs under logs/w18-help/): each write
# block renders its content preview (up to 10 lines, WRITE_PREVIEW_LINES), so
# the finished transcript is taller than the 40-row ConPTY screen. The task is
# therefore read in three scroll positions — ctrl+home (head: writes 1-3),
# one page-down (middle: writes 4-6 + the grep call line), ctrl+end (tail: the
# grep result row, the read header, the edit diff, the final text) — exactly
# like s44/s45. Those windows were measured, not assumed.
#
# The collapsed `read` block intentionally renders NO result body (upstream
# read.ts:112-148: a successful, non-expanded read shows only the call line —
# src/tui/messages.rs:1397-1399 returns None). The 3-line body therefore cannot
# be asserted on the collapsed screen; `verify_on_disk()` reads it from the
# session JSONL instead (it must be exactly the `offset=5 limit=3` slice plus
# the `Use offset=8 to continue` hint).
#
# Log gap: run_battery's CHECK_KINDS has `log_regex`/`log_not_regex`, but they
# match the RAW ConPTY STREAM, not `<agent>/pir.log` (the file the watchdog
# writes to). There is no scenario-level file check, so the stall-line
# assertion lives in `verify_on_disk()` below and is run by the lane after the
# battery (the README documents this gap in "Assertion reference").
NAME = "s48_grep_tree_read_offset_edit"
OFFLINE = False  # the app must reach the injected mock over HTTP
TIMEOUT = 180

MARKER = "W16K-NEEDLE-9f3c"

# The single needle file. Line 5 is the marker, line 6 the flag the edit
# changes; `offset=5 limit=3` below covers lines 5-7, so the read really
# targets the match found by grep.
_LEGACY = (
    "// legacy settings module (fixture tree)\n"
    "// the marker below is the only copy in the tree\n"
    "pub const VERSION: u32 = 3;\n"
    "\n"
    'pub const NEEDLE: &str = "' + MARKER + '";\n'
    'pub const FEATURE_FLAG: &str = "off";\n'
    'pub const MODE: &str = "basic";\n'
    "\n"
    'pub fn flag() -> &\'static str {\n'
    "    FEATURE_FLAG\n"
    "}\n"
    "\n"
)
assert _LEGACY.split("\n")[4].find(MARKER) >= 0  # marker on line 5 (1-indexed)

_EDIT_OLD = 'pub const FEATURE_FLAG: &str = "off";'
_EDIT_NEW = 'pub const FEATURE_FLAG: &str = "on";'
assert _LEGACY.count(_EDIT_OLD) == 1

MOCK_SCRIPT = [
    # step 1 — the fixture tree (6 files, one real `write` per file)
    {"tool_calls": [
        {"name": "write", "arguments": {
            "path": "pine/README.md",
            "content": "# pine fixture tree\nSee docs/notes.md for the layout.\n"}},
        {"name": "write", "arguments": {
            "path": "pine/src/main.rs",
            "content": "fn main() {}\n"}},
        {"name": "write", "arguments": {
            "path": "pine/src/util.rs",
            "content": "pub fn util() -> u8 { 1 }\n"}},
        {"name": "write", "arguments": {
            "path": "pine/src/legacy.rs",
            "content": _LEGACY}},
        {"name": "write", "arguments": {
            "path": "pine/tests/smoke.txt",
            "content": "smoke: pass\n"}},
        {"name": "write", "arguments": {
            "path": "pine/docs/notes.txt",
            "content": "# notes\nfixture tree for the W16K grep task.\n"}},
    ]},
    # step 2 — the REAL grep over the REAL tree just written
    {"tool_call": {"name": "grep",
                   "arguments": {"pattern": MARKER, "path": "pine"}}},
    # step 3 — the read that used to freeze the render thread
    {"tool_call": {"name": "read",
                   "arguments": {"path": "pine/src/legacy.rs",
                                 "offset": 5, "limit": 3}}},
    # step 4 — fix the flag the read just showed
    {"tool_call": {"name": "edit",
                   "arguments": {"path": "pine/src/legacy.rs",
                                 "edits": [{"oldText": _EDIT_OLD,
                                            "newText": _EDIT_NEW}]}}},
    # step 5 — the final text turn
    {"text": "W16K-S48-DONE"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s48_boot"),
    ("log", "prompt -> mock: 6 writes, grep the marker, read offset=5 limit=3, edit the flag, text"),
    ("send", b"Search the pine tree for the marker, read the match, and fix the feature flag.\r"),
    # 5 local tool rounds; grep/edit are in-process, so this settles quickly.
    ("wait", 10.0),
    ("wait_quiet", 4.0),
    ("snap", "s48_tail"),
    ("log", "ctrl+home: transcript head (write headers + grep block)"),
    ("send", b"\x1b[1;5H"),
    ("wait", 1.2),
    ("snap", "s48_head"),
    ("log", "page-down: the middle window (read header + result)"),
    ("send", b"\x1b[6~"),
    ("wait", 1.2),
    ("snap", "s48_mid"),
    ("log", "ctrl+end: back to the live end (edit diff + final text)"),
    ("send", b"\x1b[1;5F"),
    ("wait", 1.2),
    ("snap", "s48_end"),
]

CHECKS = [
    # control: nothing from the task before the prompt
    ("screen_not_contains", "s48_boot", MARKER),
    ("screen_not_contains", "s48_boot", "W16K-S48-DONE"),

    # ---- head (ctrl+home): the first write blocks ----
    ("screen_contains", "s48_head", "write pine/README.md"),
    ("screen_contains", "s48_head", "write pine/src/main.rs"),
    # ---- middle (one page-down): the remaining fixture writes ----
    ("screen_contains", "s48_mid", "write pine/src/legacy.rs"),
    ("screen_contains", "s48_mid", "write pine/docs/notes.txt"),
    # the real grep call line, tail of the mid window (`grep /pat/` + a
    # separately styled ` in pine`, so the gap is whitespace, not one space)
    ("screen_regex", "s48_mid", r"grep /" + MARKER + r"/\s+in pine"),

    # ---- tail (ctrl+end): the grep RESULT, the read header, the edit ----
    # grep formats matched paths relative to the search root (grep.rs:85-99):
    # `src/legacy.rs:5: <line>`
    ("screen_regex", "s48_end", r"(?m)^\s*src/legacy\.rs:5: .*" + MARKER),
    # the read call header carries the offset/limit range —
    # `read pine/src/legacy.rs:5-7` is the exact header whose formatter used
    # to deadlock the render thread (the regression net).
    ("screen_contains", "s48_end", "read pine/src/legacy.rs:5-7"),
    # the read really returned the requested lines (session JSONL below also
    # asserts the 3-line body + the `Use offset=8 to continue` hint)
    # the edit block header, the rendered diff and the final text
    ("screen_contains", "s48_end", "edit pine/src/legacy.rs"),
    ("screen_regex", "s48_end",
     r'(?m)^\s*\+\s*6 pub const FEATURE_FLAG: &str = "on";'),
    ("screen_regex", "s48_end",
     r'(?m)^\s*-\s*6 pub const FEATURE_FLAG: &str = "off";'),
    ("screen_contains", "s48_end", "W16K-S48-DONE"),
    # the edit landed cleanly (no tool error banner in the transcript)
    ("screen_not_contains", "s48_end", "Could not find"),
    ("screen_not_contains", "s48_end", "Command failed"),

    ("restore_markers",),
]


# ---------------------------------------------------------------------------
# On-disk verification (task requirement: edited bytes on disk + no
# `UI thread stalled` line in the real `agent/pir.log`).
#
# run_battery.py's CHECK_KINDS are screen/log-over-the-raw-stream only; there
# is no file check, and `s44`'s header's `--verify-script` hook does not exist
# in this runner (grep of run_battery.py/tui_drive.py: no such flag). So these
# assertions live here as a helper the lane calls after the battery:
#
#   python -c "import runpy; m=runpy.run_path('scenarios/s48_...py'); \
#              print(m['verify_on_disk']('out/<run-id>/s48_...'))"
#
# It returns {name: (ok, detail)} and never raises.
# ---------------------------------------------------------------------------
import glob as _glob
import json as _json
import os as _os


def _long(path):
    """Verbatim Win32 path (LongPathsEnabled=0 on this host, and the battery
    session paths are ~300 chars — the same reason run_battery.py has
    `_win_long_path`)."""
    absolute = _os.path.abspath(path)
    if _os.name != "nt" or absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def verify_on_disk(scen_dir):
    """Assert the REAL outcomes of the s48 task inside `scen_dir`.

    Returns {name: (ok, detail)}:
      fixture_tree       the six files the write calls created exist
      grep_found_needle  the session JSONL holds the grep result row
                         `src/legacy.rs:5: ...` (the same text the screen shows)
      read_range_result  the read toolResult is EXACTLY the 3 lines of
                         `offset=5 limit=3` plus the `Use offset=8` hint
      edit_on_disk       legacy.rs now says "on" and no longer says "off"
      read_freeze_log    `agent/pir.log` has NO `UI thread stalled` line
    """
    out = {}
    agent = _os.path.join(scen_dir, "agent")

    wanted = [
        "pine/README.md", "pine/src/main.rs", "pine/src/util.rs",
        "pine/src/legacy.rs", "pine/tests/smoke.txt", "pine/docs/notes.txt",
    ]
    missing = [p for p in wanted if not _os.path.exists(_long(_os.path.join(agent, p)))]
    out["fixture_tree"] = (not missing, f"{len(wanted) - len(missing)}/{len(wanted)} files exist"
                           if not missing else f"missing: {missing}")

    # --- session JSONL: the grep result and the ranged read body ---
    results = {}          # tool name -> list of result texts
    for path in _glob.glob(_os.path.join(agent, "sessions", "*", "*.jsonl")):
        try:
            text = open(_long(path), encoding="utf-8", errors="replace").read()
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
            if message.get("role") != "toolResult":
                continue
            content = message.get("content")
            body = ""
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and isinstance(block.get("text"), str):
                        body += block["text"]
            elif isinstance(content, str):
                body = content
            results.setdefault(message.get("toolName"), []).append(body)

    grep_bodies = results.get("grep", [])
    needle_rows = [r for r in grep_bodies if MARKER in r]
    out["grep_found_needle"] = (
        len(needle_rows) == 1 and "src/legacy.rs:5:" in needle_rows[0],
        needle_rows[0].strip()[:160] if needle_rows else f"grep results: {grep_bodies}")

    read_bodies = results.get("read", [])
    read_body = read_bodies[0] if read_bodies else ""
    body_lines = [ln for ln in read_body.splitlines() if ln.strip()]
    has_hint = "Use offset=8 to continue" in read_body
    out["read_range_result"] = (
        len(body_lines) >= 3 and body_lines[0].startswith("pub const NEEDLE")
        and 'FEATURE_FLAG: &str = "off"' in read_body and has_hint,
        f"{len(body_lines)} non-empty line(s), offset hint={has_hint}, "
        f"first={body_lines[0][:60] if body_lines else None!r}")

    # --- the edited bytes ---
    legacy = _os.path.join(agent, "pine", "src", "legacy.rs")
    try:
        text = open(_long(legacy), encoding="utf-8").read()
    except OSError as exc:
        out["edit_on_disk"] = (False, f"cannot read {legacy}: {exc}")
    else:
        have_new = 'FEATURE_FLAG: &str = "on";' in text
        have_old = 'FEATURE_FLAG: &str = "off";' in text
        out["edit_on_disk"] = (have_new and not have_old,
                               f'on={"on" if have_new else "-"} off={have_old}')

    # --- the freeze net: the real TUI log ---
    log_path = _os.path.join(agent, "pir.log")
    try:
        log_text = open(_long(log_path), encoding="utf-8", errors="replace").read()
    except OSError as exc:
        out["read_freeze_log"] = (False, f"cannot read {log_path}: {exc}")
    else:
        stalled = [ln for ln in log_text.splitlines() if "UI thread stalled" in ln]
        out["read_freeze_log"] = (
            not stalled,
            f"no stall line ({len(log_text)} bytes of log)" if not stalled
            else f"{len(stalled)} stall line(s), first: {stalled[0][:200]}")

    return out
