# -*- coding: utf-8 -*-
# W13-M / Task B part 1 — "multi-tool batch + abort", driven through the REAL
# TUI over ConPTY (no unit-test shortcuts: every assertion is a pyte render of
# the actual screen).
#
# WHAT IT DRIVES (one hermetic mock conversation, three requests):
#   req 1 (prompt 1) -> THREE tool calls in ONE assistant message:
#                         ls   (path=".")                      [in-process, fast]
#                         grep (pattern="sk-mock-battery")     [in-process, fast]
#                         bash `sleep 25; echo BATCH-BASH-DONE`[shell pool, slow]
#   req 2 (the three tool results) -> text "BATCH-TURN-DONE"
#   req 3 (prompt 2) -> a PACED 600-token stream ending in "ABORT-TAIL-46"
#                       (Esc lands ~4 s in, i.e. mid-stream)
#
# WHY THE ORDER IS ls -> grep -> bash (design, not calibration): `bash` is
# `ToolExecutionMode::Sequential` (src/coding_agent/tools/bash.rs:1429-1431),
# and the scheduler degrades a batch that contains a non-detached sequential
# call to source order (src/agent/tool_scheduler/execute.rs:50-53). So the two
# fast in-process calls run and settle BEFORE the shell call even starts —
# which makes the mid-run window unambiguous: at the mid snapshot the ls and
# grep RESULTS must already be on screen while bash is still asleep. Before the
# per-call progress fix (agent_loop.rs `with_progress` + the scheduler's
# completion-order `end` events) the whole batch emitted nothing until
# `join_all` returned, so this snapshot would be empty (s39 proves the same
# contract with powershell+bash; this scenario needs it for a three-call batch
# that mixes in-process tools with a shell tool).
#
# WHY 25 s AND NOT "A BRIEF SLEEP" (calibration, measured on this host): a
# pooled bash member can take up to ~10 s to spawn before the command runs
# (s39's measurement), and the mid snapshot must land after the two fast
# results exist and before bash's output exists. With `sleep 25` the window is
# [t_fast_results, t_bash_start+25] — at least ~15 s wide whichever way the
# spawn goes; the snapshot is taken 18 s after the send. If a future machine is
# slower, widen the sleep / shift the snapshot; never weaken the assertions.
#
# ABORT TURN assertions:
#   * the paced stream really was interrupted mid-flight (its tail sentinel
#     "ABORT-TAIL-46" never rendered -> Esc cut it, the turn had not finished);
#   * exactly ONE abort notice ("Operation aborted", upstream's rendered
#     label — the raw "Request was aborted" is filtered) — the p09/F4
#     duplicate regression (terminal.rs:19168 note) must not come back;
#   * the "Working" editor-border label is gone (idle, no lingering spinner);
#   * the app is still responsive: a following `/help` executes and renders the
#     command list, and the editor is cleared by the command (it would still
#     hold "/help" if the UI were stuck).
NAME = "s46_multi_tool_batch_abort"
OFFLINE = False  # the app must really reach the injected mock over HTTP
TIMEOUT = 180

MOCK_SCRIPT = [
    {"tool_calls": [
        {"name": "ls", "arguments": {"path": "."}},
        {"name": "grep", "arguments": {"pattern": "sk-mock-battery", "path": "."}},
        {"name": "bash", "arguments": {"command": "sleep 25; echo BATCH-BASH-DONE"}},
    ]},
    {"text": "BATCH-TURN-DONE"},
    # Paced so the abort lands mid-stream: 300 content deltas x 25 ms = ~7.5 s.
    {"text": " ".join(f"w{i}" for i in range(600)) + " ABORT-TAIL-46",
     "chunks": 300, "chunk_delay_ms": 25},
]

OPS = [
    ("wait_quiet", 4.0),
    ("snap", "s46_boot"),
    ("log", "prompt 1 -> mock: ls + grep + bash(sleep 25) in ONE assistant message"),
    ("send", b"Run the three checks in one batch.\r"),
    # t+18: ls/grep have settled, bash is still asleep (see the timing note).
    ("wait", 18.0),
    ("snap", "s46_mid_batch"),
    ("log", "wait for the slow bash call and the turn's own answer to settle"),
    ("wait", 14.0),
    ("wait_quiet", 6.0),
    ("snap", "s46_batch_done"),
    ("log", "prompt 2 -> mock: paced long stream, abort with Esc mid-stream"),
    ("send", b"Now stream a long answer.\r"),
    ("wait", 4.0),
    ("log", "Esc mid-stream (the stream is ~7.5s long, so this lands inside it)"),
    ("send", b"\x1b"),
    ("wait", 3.0),
    ("snap", "s46_aborted"),
    ("log", "the UI must still be responsive: /help executes and renders"),
    ("send", b"/help\r"),
    ("wait", 2.5),
    ("snap", "s46_after_abort_help"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]

CHECKS = [
    # ---- control: nothing from the batch or the stream before the prompt ----
    ("screen_not_contains", "s46_boot", "BATCH-TURN-DONE"),
    ("screen_not_regex", "s46_boot", r"(?m)^\s*sessions/\s*$"),
    ("screen_not_regex", "s46_boot", r"(?m)^\s*BATCH-BASH-DONE\s*$"),

    # ---- mid-batch: all three blocks render, the fast RESULTS are already
    # visible, the slow bash result is not (this is the regression assertion).
    ("screen_contains", "s46_mid_batch", "ls ."),
    ("screen_contains", "s46_mid_batch", "grep /sk-mock-battery/"),
    ("screen_contains", "s46_mid_batch", "$ sleep 25; echo BATCH-BASH-DONE"),
    # ls result: the agent dir contains a `sessions/` entry (the call line is
    # `ls .`, so this row can only come from the result section).
    ("screen_regex", "s46_mid_batch", r"(?m)^\s*sessions/\s*$"),
    # grep result rows are `models.json:<line>: <text>` (grep.rs:270-278); the
    # call line is `grep /sk-mock-battery/ in .`, so this pattern is unique to
    # the result.
    ("screen_regex", "s46_mid_batch", r"(?m)^\s*models\.json:\d+:"),
    # the slow call has NOT produced its output yet (line-anchored: the echoed
    # command line legitimately contains the marker text).
    ("screen_not_regex", "s46_mid_batch", r"(?m)^\s*BATCH-BASH-DONE\s*$"),
    # the run is still live (positive control for the "Working" check below)
    ("screen_contains", "s46_mid_batch", "Working"),

    # ---- settled batch: the slow result and the turn's answer arrive ----
    ("screen_regex", "s46_batch_done", r"(?m)^\s*BATCH-BASH-DONE\s*$"),
    ("screen_contains", "s46_batch_done", "BATCH-TURN-DONE"),

    # ---- abort: the stream was cut mid-flight, one notice, UI idle ----
    ("screen_not_contains", "s46_aborted", "ABORT-TAIL-46"),
    ("screen_count", "s46_aborted", "Operation aborted", "==1"),
    ("screen_contains", "s46_aborted", "Interrupted"),
    ("screen_not_contains", "s46_aborted", "Working"),

    # ---- responsive after the abort: /help runs, the editor cleared ----
    ("screen_contains", "s46_after_abort_help",
     "Commands: /model /thinking /clear /compact"),
    ("editor_not_contains", "s46_after_abort_help", "/help"),
    # no duplicate abort notice appears afterwards ("<2" tolerates the row
    # having scrolled off the 40-row screen; a repeated notice would add one).
    ("screen_count", "s46_after_abort_help", "Operation aborted", "<2"),

    ("restore_markers",),
]
