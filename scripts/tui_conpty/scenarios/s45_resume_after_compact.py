# -*- coding: utf-8 -*-
# W13-L task A part 2, SECOND HALF — "/quit + relaunch with the transcript
# restored".
#
# This scenario is CHAINED: it must run AFTER
# `s45_real_task_stream_and_compact` in the same battery invocation (the
# battery sorts by file name, so `s45_real...` runs first). At driver-exec
# time it locates the newest session JSONL that scenario wrote and relaunches
# the REAL TUI on it with `--session <absolute path>`.
#
# Why an absolute --session path: `resolve_session_ref` treats a
# path-shaped argument as `Found` without the cwd filter
# (src/coding_agent/session_resolve.rs:97-105), so the resumed process may run
# in ITS own per-scenario agent dir while still opening the first scenario's
# session file. `-c/--continue` cannot be used: it filters by cwd
# (session_resolve.rs:254-262) and every battery scenario gets a fresh agent
# dir.
#
# The assertions are about the RESTORED transcript: the last turn's prompt and
# reply of the previous scenario must be replayed into the view, then a NEW
# prompt must run against the reopened session.
#
# If no prior session file exists (this file run alone), the scenario falls
# back to a plain boot: ARGS is empty, the prior-transcript checks are dropped
# and only the clean-quit invariants run, so it can never silently claim a
# restore it did not observe.

import glob
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_OUT = os.path.join(os.path.dirname(_HERE), "out")


def _active_run_dir():
    """The run directory this battery invocation is actually writing into.

    Q58: the driver execs this module with `--shots-dir
    <out-root>/<run-id>/<scenario>` (tui_drive.py CLI), so its parent is the
    active run dir even when the operator passes a non-default --out-root
    (the Q58 command does: logs/w18-help/q58/full-*). At discovery time
    (run_battery.load_scenario) the runner's argv instead carries
    --out-root/--run-id. Neither present -> None (legacy default root).
    """
    argv = list(sys.argv)
    if "--shots-dir" in argv:
        i = argv.index("--shots-dir")
        if i + 1 < len(argv):
            return os.path.dirname(os.path.abspath(argv[i + 1]))
    if "--out-root" in argv and "--run-id" in argv:
        i = argv.index("--out-root")
        j = argv.index("--run-id")
        if i + 1 < len(argv) and j + 1 < len(argv):
            return os.path.abspath(os.path.join(argv[i + 1], argv[j + 1]))
    return None


_RUN_DIR = _active_run_dir()


def _long(path):
    """Verbatim Win32 path: a battery session path is ~300 chars and this host
    has LongPathsEnabled=0, so a plain stat/open raises FileNotFoundError
    (the same reason run_battery.py has `_win_long_path`)."""
    absolute = os.path.abspath(path)
    if os.name != "nt" or absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def _plain(path):
    """Strip the verbatim prefix: `--session` must be a plain absolute path."""
    if path.startswith("\\\\?\\"):
        return path[4:]
    return path


def _jsonl_under(root):
    """Every *.jsonl under root, walked through the verbatim prefix.

    Q58: with the fixwave2 out-root the session path is ~293 chars, and this
    host has LongPathsEnabled=0, so `glob.glob(pattern)` silently returns
    nothing (a plain stat/open fails the same way; see `_long`). An os.walk
    over the `\\\\?\\` form enumerates it. The caller keeps the newest-mtime
    rule unchanged.
    """
    found = []
    for dirpath, _dirs, files in os.walk(_long(root)):
        for name in files:
            if name.endswith(".jsonl"):
                found.append(os.path.join(dirpath, name))
    return found


def _newest_session():
    """Newest session JSONL written by the part-1 scenario, or None."""
    if _RUN_DIR:
        roots = [os.path.join(_RUN_DIR, "s45_real_task_stream_and_compact",
                              "agent", "sessions")]
    else:
        roots = glob.glob(os.path.join(_OUT, "*",
                                       "s45_real_task_stream_and_compact",
                                       "agent", "sessions"))
    found = []
    for root in roots:
        for path in _jsonl_under(root):
            try:
                found.append((os.path.getmtime(_long(path)), path))
            except OSError:
                continue
    if not found:
        return None
    found.sort()
    # A verbatim path is for this module's own filesystem calls only; pir's
    # `--session` gets the plain absolute form (s47b documents the same rule).
    return _plain(found[-1][1])


_SESSION = _newest_session()

NAME = "s45_resume_after_compact"
OFFLINE = False
TIMEOUT = 150
# The mock is still injected (the new agent dir gets its own provider), so a
# fresh prompt after the resume can be answered hermetically.
MOCK_SCRIPT = [{"text": "W13-RESUME-AFTER-QUIT-DONE"}]

ARGS = ["--session", _SESSION] if _SESSION else []

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s45r_boot"),
    ("log", "resumed session: the previous scenario's last turn must be replayed"),
    ("send", b"Say hello from the resumed session.\r"),
    ("wait", 6.0),
    ("wait_quiet", 3.0),
    ("snap", "s45r_after_prompt"),
]

# These are UNCONDITIONAL: a resume that did not happen must FAIL, not quietly
# shrink the check list. `--session` is resolved at DRIVER-exec time, i.e. after
# part 1 has already written its session in the same battery run.
CHECKS = [
    # the replayed transcript: the previous scenario's final prompt + reply
    ("screen_contains", "s45r_boot", "Summarize what we just did."),
    ("screen_contains", "s45r_boot", "W13-A2-AFTER-COMPACT-DONE"),
    # the compaction really happened in part 1: the BULK tail is what the
    # restored view shows above the post-compaction turn
    ("screen_contains", "s45r_boot", "W13-BULK-1300:"),
    # the reopened session still runs a prompt to completion
    ("screen_contains", "s45r_after_prompt", "W13-RESUME-AFTER-QUIT-DONE"),
    ("restore_markers",),
]
