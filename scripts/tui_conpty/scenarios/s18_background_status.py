# -*- coding: utf-8 -*-
# s18 — background tool-call UI: the `bg: N` status marker lifecycle and the
# in-place block flip (TOOL-BACKGROUND-SCOPE.md §5 test-plan item 4, the half
# the tui_units status-string test cannot reach).
#
# HERMETIC (mock LLM, no gateway): MOCK_SCRIPT drives the injected mock
# (mock_llm.py, run_battery injects it into the seeded agent dir) through a
# fixed conversation:
#   req 1 (prompt 1)                    -> ONE bash tool call `sleep 5`
#                                          with "background": true
#   req 2 (the placeholder tool result) -> text "s18-turn1-done"
#   req 3 (prompt 2)                    -> TWO parallel backgrounded calls:
#                                          bash `sleep 4` + powershell
#                                          `Start-Sleep -Seconds 10`
#   req 4 (both placeholders)           -> text "s18-turn2-done" (then repeats)
# The placeholder result the model sees is never waited on, so the second
# request of each turn answers while the backgrounded sleeps still run.
#
# Anchors (product side, read not run):
#   * marker text: `background_status_message` (src/tui/terminal.rs:1848-1858)
#     -> `Working · bg: N` while a run is active, `bg: N` while idle; rendered
#     as `{spinner} {message}` (src/tui/app.rs:1091-1093) INTO the editor top
#     border row (terminal.rs:2265-2284), so `bg: 1` is a plain substring of
#     the visible screen. Count source: `WorkerEvent::BackgroundCalls`
#     (terminal.rs:3399), sent at every tool lifecycle edge (:12307) and on
#     each background completion (:10513, registry settled first).
#   * block: the pending bash block renders `$ sleep 5` (call-line formatter,
#     src/tui/messages.rs:735-746); the turn-end placeholder tool result
#     ("Tool 'bash' is running in the background (callId: …)…", execute.rs:183)
#     then flips it to a settled block, and `SessionEvent::BackgroundToolEnd`
#     arrives out of band and REPLACES the block in place with the real output
#     (terminal.rs:10469-10516). A completed `sleep 5` with empty stdout has
#     result text `(no output)` (bash.rs `finalize_shell_result`; the
#     powershell tool renders the same tail, powershell.rs:431-436) — that
#     string is the flip evidence; while a call is still pending it cannot
#     render.
#   * keep-alive: the placeholder carves out the background request BEFORE the
#     tool sees it (`prepare.rs` strips `background`), `supports_background`
#     defaults true (tool_execution.rs:318), and the TUI session installs the
#     detach sink (agent_session/build.rs:196).
#
# WHY TWO DIFFERENT SHELLS IN RUN 2 (design, not calibration): the shell pool
# keeps ONE persistent member PER SHELL KIND, each serialized behind its own
# mutex held for the whole command (shell_pool.rs:752-757, :811-824), so two
# concurrent `bash` calls execute one-after-the-other and the intermediate
# "exactly one in flight" instant depends on which future reached the pool
# first. bash + powershell take different pool slots (and separate per-call
# spawns in the pool's fallback path), so both really run concurrently and the
# `bg: 1` window is [t_bash, t_powershell] by construction (~4s..~10s after
# the detach). This also makes the run independent of pool health.
#
# Timing (local mock, git-bash `sleep`; snap angles chosen with >=2s margin
# to the nearest settle):
#   run 1: detach ~t+0.5s, settles ~t+5.5s; snap running at t+2.5, after at
#          ~t+10 (wait 4.5 + wait_quiet 3).
#   run 2: detach ~t+0.5s; bash settles ~t+4.5, powershell ~t+10.5; snaps at
#          t+2.0 (`bg: 2`), t+7.5 (`bg: 1`), and after settle ~t+14.
# If a window ever misses, the machine was >2s slower than the mock's
# sub-100ms round-trip / powershell spawn; widen the sleeps, never the
# assertions.
#
# With --real-provider the script does NOT apply (real model, real replies) and
# these checks fail like s08's `mock reply` check — run it hermetic.
NAME = "s18_background_status"
OFFLINE = False  # the app must really reach the mock over HTTP
TIMEOUT = 150  # boot 4 + run1 ~10 + run2 ~14 + quit ~5, wide headroom
# DECLARE `powershell`. The default active set is upstream `DEFAULT_TOOL_NAMES`
# = ["read", "bash", "edit", "write"] (settings-manager.ts:85,
# agent-session.ts:3679-3684): `powershell` is registered but
# `default_active: false` (src/coding_agent/tools/powershell.rs:118), so it is
# neither declared to the model nor callable — upstream resolves a call only in
# the declared set (agent-loop.ts:716), and pir's W16-D11 gate now enforces
# the same rule for registered-but-undeclared `direct` tools
# (src/agent/tool_scheduler/prepare.rs:110-120). Without this flag the
# model-emitted `powershell` call is answered "Tool not found in registry"
# (it executed until the D11 gate landed 2026-10-22 21:14), so the run-2
# two-shell concurrency this scenario is built around never happens.
ARGS = ["--tools", "read,bash,edit,write,powershell"]
MOCK_SCRIPT = [
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "sleep 5", "background": True}}},
    {"text": "s18-turn1-done"},
    {"tool_calls": [
        {"name": "bash", "arguments": {"command": "sleep 4", "background": True}},
        {"name": "powershell",
         "arguments": {"command": "Start-Sleep -Seconds 10", "background": True}},
    ]},
    {"text": "s18-turn2-done"},
]
OPS = [
    ("wait_quiet", 4),
    ("snap", "s18_boot"),
    ("log", "prompt 1 -> mock: one backgrounded bash call (sleep 5)"),
    ("send", b"Run the first background check.\r"),
    ("wait", 2.5),
    ("snap", "s18_bg_running"),
    ("log", "wait for the 5s call to settle (~t+7s since the send)"),
    ("wait", 4.5),
    ("wait_quiet", 3),
    ("snap", "s18_bg_done"),
    ("log", "prompt 2 -> mock: bash sleep 4 + powershell Start-Sleep 10, both backgrounded"),
    ("send", b"Run two background checks at once.\r"),
    ("wait", 2.0),
    ("snap", "s18_two_running"),
    ("log", "bash 4s settled, powershell 10s still running"),
    ("wait", 5.5),
    ("snap", "s18_one_running"),
    ("log", "wait for both to settle"),
    ("wait", 3.5),
    ("wait_quiet", 3),
    ("snap", "s18_two_done"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    # control: no marker before any background call exists
    ("screen_not_contains", "s18_boot", "bg:"),
    # run 1, in flight: marker present, block still pending (no real result),
    # and the turn's own answer is already rendered -> the run did not block
    # on the backgrounded call.
    ("screen_contains", "s18_bg_running", "bg: 1"),
    ("screen_contains", "s18_bg_running", "$ sleep 5"),
    ("screen_contains", "s18_bg_running", "s18-turn1-done"),
    ("screen_not_contains", "s18_bg_running", "(no output)"),
    # run 1 settled: count back to zero and the block flipped to the real
    # result in place ("(no output)" is what a settled empty-output shell run
    # renders; the placeholder text never contains it).
    ("screen_not_contains", "s18_bg_done", "bg:"),
    ("screen_contains", "s18_bg_done", "(no output)"),
    # run 2: two parallel backgrounded calls -> bg: 2; when the 4s bash call
    # settles first the marker drops to bg: 1 while the 10s powershell call
    # still runs; afterwards no marker and a settled block.
    ("screen_contains", "s18_two_running", "bg: 2"),
    ("screen_contains", "s18_two_running", "s18-turn2-done"),
    ("screen_contains", "s18_one_running", "bg: 1"),
    ("screen_not_contains", "s18_two_done", "bg:"),
    ("screen_contains", "s18_two_done", "(no output)"),
    ("restore_markers",),
]
