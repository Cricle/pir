# -*- coding: utf-8 -*-
# s32 — parallel tool calls must surface each result AS IT COMPLETES, not
# when the slowest sibling (and the batch) finally settles (2026-10-15 user
# report: "并行调用工具，会相互等待，而不是完成立刻显示").
#
# Two foreground calls in one assistant message: a fast powershell
# (`Start-Sleep 0.2` -> PARALLEL-FAST) and a slow bash (`sleep 8` ->
# PARALLEL-SLOW). They take different shell-pool slots (s18 established
# that two `bash` calls share one serialized persistent slot), so they really
# overlap. The mid-run snap at ~t+3.5s must ALREADY show the fast output
# while the slow one is still running (its marker must be absent); the final
# snap shows both and the turn's own answer.
#
# Before the fix the event stream emitted every ToolExecutionStart/End only
# after `join_all` returned, so neither block could render before ~t+8s.
# If this scenario ever misses on a slow machine, widen the sleeps (never
# the assertions): the fast call lands seconds before the slow one by
# construction.
NAME = "s39_parallel_tool_stream"
OFFLINE = False
TIMEOUT = 180
# DECLARE `powershell` (see s18): it is registered but `default_active: false`
# (src/coding_agent/tools/powershell.rs:118) and therefore not in the upstream
# default active set (settings-manager.ts:85), so an undeclared call is
# "Tool not found in registry" under the W16-D11 executable-set gate
# (src/agent/tool_scheduler/prepare.rs:110-120; upstream agent-loop.ts:716).
ARGS = ["--tools", "read,bash,edit,write,powershell"]
# Shell tools are `ToolExecutionMode::Sequential`, so a batch containing a
# bash/powershell call runs its calls in source order (upstream semantics) —
# the two calls below never overlap. That still exercises the regression this
# scenario was written for: the FAST first call's result must render as soon
# as it completes, while the slow second call is still running. Pre-fix, the
# scheduler emitted every start/end only after `join_all` returned, so nothing
# appeared until the slow call settled.
MOCK_SCRIPT = [
    {"tool_calls": [
        {"name": "powershell",
         "arguments": {"command": "Start-Sleep -Seconds 0.2; Write-Output PARALLEL-FAST"}},
        {"name": "bash",
         "arguments": {"command": "sleep 25; echo PARALLEL-SLOW"}},
    ]},
    {"text": "PARALLEL-TURN-DONE"},
]
OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s39_boot"),
    ("send", b"Run both checks one after the other.\r"),
    ("log", "fast powershell result must be rendered; slow bash (25s) still running"),
    # 20 s = the ~10 s pool-member startup + 0.2 s execution measured on this
    # host, plus margin; the slow bash call starts only after the fast one
    # settles (sequential shell batch), so it is still asleep here.
    ("wait", 20.0),
    ("snap", "s39_fast_done_slow_running"),
    ("log", "wait for the slow bash call and the turn to settle"),
    ("wait", 20.0),
    ("wait_quiet", 2.0),
    ("snap", "s39_both_done"),
]
CHECKS = [
    # control: nothing from either call before the prompt
    ("screen_not_contains", "s39_boot", "PARALLEL-FAST"),
    ("screen_not_contains", "s39_boot", "PARALLEL-SLOW"),
    # Every marker is also present in the ECHOED command line (`$ sleep 25;
    # echo PARALLEL-SLOW`, `Write-Output PARALLEL-FAST`), so a bare substring
    # check is either satisfied or broken by the echo itself. The result
    # assertions are therefore line-anchored: only a bare output row counts.
    # mid-run: the fast result is visible while the slow call is pending.
    # This is the regression assertion — pre-fix the batch emitted nothing
    # until the slow call settled.
    ("screen_regex", "s39_fast_done_slow_running", r"(?m)^\s*PARALLEL-FAST\s*$"),
    ("screen_not_regex", "s39_fast_done_slow_running", r"(?m)^\s*PARALLEL-SLOW\s*$"),
    # settled: both outputs and the final answer.
    ("screen_regex", "s39_both_done", r"(?m)^\s*PARALLEL-FAST\s*$"),
    ("screen_regex", "s39_both_done", r"(?m)^\s*PARALLEL-SLOW\s*$"),
    ("screen_contains", "s39_both_done", "PARALLEL-TURN-DONE"),
    ("restore_markers",),
]
