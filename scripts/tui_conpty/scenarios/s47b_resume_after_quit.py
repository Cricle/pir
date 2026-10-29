# -*- coding: utf-8 -*-
# W13-M / Task B part 2b — "resume after quit": a SECOND pir process, launched
# after s47_session_tabs_quit quit, must restore that session's transcript
# (tool block included) from the JSONL on disk.
#
# HOW THE TWO RUNS SHARE A SESSION DIRECTORY (harness contract, read not
# assumed):
#   * run_battery gives each scenario its own `<run>/<scenario>/agent` and passes
#     it as BOTH `--agent-dir` (child `PIR_AGENT_DIR`) AND the child cwd
#     (tui_drive.py:323-329), so a session written by s47 is not reachable
#     through this scenario's default session lookup.
#   * `PIR_SESSION_DIR` is honored by `resolve_session_dir` (second source after
#     `--session-dir`, src/main_cli/cli_preprocess.rs:294-310) and resolved
#     BEFORE the TUI starts (run_mode.rs:297); `ENV` is merged over the driver's
#     BASE_ENV (tui_drive.py:326-327). That is what lets this scenario open the
#     session directory s47 wrote.
#   * The directory name comes from pir's `encode_cwd_dir_name`
#     (session_manager/mod.rs:205-217), mirrored exactly as run_battery.py does
#     it for its own fixture (run_battery.py:522-527).
#
# WHICH SESSION: s47 leaves TWO files in that directory (tab 1 with the tool
# block, tab 2 with only a later prompt). The resumed one is selected by
# CONTENT — the file whose JSONL contains `ALPHA-TOOL-47` — not by mtime
# (tab 2 is written last, so "newest" would pick the wrong session).
#
# WHY `--session <path>` AND NOT `-r` AT STARTUP (product behavior, read in
# source): the `-r/--resume` startup fast path lists only sessions whose header
# cwd equals the process cwd (`resolve_run_session_plan` ->
# `SessionManager::list_sessions(session_dir, Some(&cwd))`,
# cli_preprocess.rs:279-282); this process's cwd is s47b's own agent dir, so `-r`
# would find nothing and silently start a fresh session. `--session <path>` is
# the documented "open a specific session file" resume op
# (session_resolve.rs:57/221-243: a literal path is used as-is, no cwd check) —
# exactly the "relaunch straight into this session" action. The in-TUI
# `/resume` PICKER is exercised afterwards in the same run: the picker always
# holds the cwd-scoped AND the full list and Tab switches scope
# (`pick_session_with_all`, session_pick.rs:348-352 + the `KeyCode::Tab` arm),
# and typing filters by id/cwd/name/first-message (`filter_rows`,
# session_pick.rs:173-201) — so `Tab`, `alpha`, `Enter` resumes it even though
# its header cwd is another directory and the scoped list is empty.
#
# RUN-ID RESOLUTION: the runner's `--run-id` is not visible to a scenario
# module. `W13B_RUN_ID` (exported by the operator, inherited by run_battery and
# therefore by the driver) pins it; otherwise the newest `out/*` directory that
# already holds a session containing ALPHA-TOOL-47 is used. If neither
# resolves, `PIR_SESSION_DIR` names a nonexistent directory and the content
# checks FAIL loudly (never a silent pass).
#
# Windows note: the session path is ~290 chars, so every filesystem call here
# goes through the verbatim `\\?\` prefix (`_win`, the same reason
# run_battery.py has `_win_long_path`; this machine has LongPathsEnabled=0).
NAME = "s47b_resume_after_quit"
OFFLINE = False
TIMEOUT = 150
SEED_SESSION = False  # no harness fixture: the resumed session must be s47's
MARKER = "ALPHA-TOOL-47"

import os  # noqa: E402  (scenario modules are exec'd, not imported as packages)


def _out_root():
    import os
    return os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                        "out")


def _active_run_dir():
    """The run dir this invocation writes into (Q58).

    The driver execs this module with `--shots-dir
    <out-root>/<run-id>/<scenario>`; its parent is the active run dir even
    when --out-root is non-default. At discovery time the runner's argv
    carries --out-root/--run-id instead. None -> legacy default root.
    """
    import os
    import sys
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


def _win(path):
    """Verbatim Win32 path (see the Windows note in the header)."""
    import os
    if os.name != "nt":
        return path
    absolute = os.path.abspath(path)
    if absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def _plain(path):
    """`_win`'s inverse: strip the verbatim prefix (arguments to pir stay plain)."""
    return path[4:] if path.startswith("\\\\?\\") else path


def _client_sessions_dir(run_id):
    """s47's `PIR_SESSION_DIR`: `<agent>/sessions/<encode_cwd_dir_name(agent)>`."""
    import os

    agent = os.path.join(_out_root(), run_id, "s47_session_tabs_quit", "agent")
    return _sessions_dir_for_agent(agent)


def _sessions_dir_for_agent(agent):
    import os
    import re

    resolved = os.path.abspath(agent)  # pir resolves the cwd before encoding
    stripped = resolved[1:] if resolved[:1] in ("/", "\\") else resolved
    safe = re.sub(r"[/\\:]", "-", stripped)
    return os.path.join(agent, "sessions", f"--{safe}--")


def _session_files(sessions_dir):
    import os
    found = []
    try:
        for root, _dirs, files in os.walk(_win(sessions_dir)):
            for name in files:
                if name.endswith(".jsonl"):
                    found.append(os.path.join(root, name))
    except OSError:
        return []
    return found


def _contains(path, needle):
    try:
        with open(_win(path), "rb") as fh:
            return needle.encode("utf-8") in fh.read()
    except OSError:
        return False


def _resolve_run_id():
    """W13B_RUN_ID if set, else the newest `out/*` holding our marker."""
    import os

    pinned = os.environ.get("W13B_RUN_ID", "").strip()
    if pinned:
        return pinned
    out_root = _out_root()
    try:
        names = [n for n in os.listdir(out_root)
                 if os.path.isdir(os.path.join(out_root, n))]
    except OSError:
        return None
    names.sort(key=lambda n: os.path.getmtime(os.path.join(out_root, n)), reverse=True)
    for name in names:
        if any(_contains(p, MARKER) for p in _session_files(_client_sessions_dir(name))):
            return name
    return None


_RUN_DIR = _active_run_dir()
_RUN_ID = os.path.basename(_RUN_DIR) if _RUN_DIR else _resolve_run_id()
_SESSIONS_DIR = (_sessions_dir_for_agent(os.path.join(
                     _RUN_DIR, "s47_session_tabs_quit", "agent"))
                 if _RUN_DIR else
                 (_client_sessions_dir(_RUN_ID) if _RUN_ID
                  else os.path.join(_out_root(), "UNRESOLVED", "sessions")))
# The ALPHA session (content-selected; see the header note).
_ALPHA = next((p for p in _session_files(_SESSIONS_DIR) if _contains(p, MARKER)), None)

ENV = {"PIR_SESSION_DIR": _SESSIONS_DIR}
# `--session <path>`: the documented open-a-specific-session resume op. The
# PLAIN absolute path is passed (no `\\?\` prefix): pir's Rust file reads handle
# long paths themselves, and a verbatim prefix would be rendered verbatim in the
# startup `Session:` row. `_win` is only for this module's own Python reads.
ARGS = ["--session", _plain(_ALPHA) if _ALPHA
        else os.path.join(_SESSIONS_DIR, "missing.jsonl")]

OPS = [
    ("wait_quiet", 6.0),
    ("snap", "s47b_boot"),
    # ---- phase 1: the startup `--session` attach replayed the transcript ----
    ("log", "phase 1: startup --session attach must have replayed s47's session"),
    # ---- phase 2: the same session through the in-TUI /resume PICKER ----
    ("log", "/resume opens the picker (cwd scope empty); Tab = All dirs; filter 'alpha'"),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("snap", "s47b_picker"),
    ("send", b"\t"),
    ("wait", 1.5),
    ("snap", "s47b_picker_all"),
    ("send", b"alpha"),
    ("wait", 1.5),
    ("snap", "s47b_picker_filtered"),
    ("send", b"\r"),
    ("wait", 4.0),
    ("wait_quiet", 3.0),
    ("snap", "s47b_after_picker_resume"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]

CHECKS = [
    # ---- phase 1: after relaunch the transcript is back on screen, read from
    # the JSONL: the prompt, the tool CALL line, the tool RESULT row and the
    # turn's answer. The result row is line-anchored because the call line
    # contains the same marker (prefixed by `$ `). ----
    ("screen_contains", "s47b_boot", "Run the alpha check."),
    ("screen_contains", "s47b_boot", "$ echo ALPHA-TOOL-47"),
    ("screen_regex", "s47b_boot", r"(?m)^\s*ALPHA-TOOL-47\s*$"),
    ("screen_contains", "s47b_boot", "ALPHA-TURN-47"),
    # the startup attach announced the session path (not a blank fresh session)
    ("screen_contains", "s47b_boot", "Session:"),
    # nothing from the OTHER tab leaks into the resumed transcript
    ("screen_not_contains", "s47b_boot", "BETA-TURN-47"),

    # ---- phase 2: the picker path. The frame renders; Tab exposes the All-dirs
    # list (the cwd-scoped one is empty — the session's header cwd is s47's
    # agent dir); the 'alpha' filter narrows the list to the single ALPHA
    # session, identified by its FIRST MESSAGE (`Run the alpha check.`; a picker
    # row shows name > first message > id, never tool output — verified on the
    # rendered row `› Run the alpha check.  5 now`); Enter replays it. ----
    ("screen_contains", "s47b_picker", "Resume a session"),
    # the cwd scope holds nothing (proves the Tab switch is doing real work)
    ("screen_contains", "s47b_picker", "No sessions found"),
    ("screen_not_contains", "s47b_picker", "seeded battery fixture session"),
    ("screen_contains", "s47b_picker_all", "Scope: All dirs"),
    ("screen_contains", "s47b_picker_all", "Beta prompt only."),
    ("screen_contains", "s47b_picker_all", "Run the alpha check."),
    # after the 'alpha' filter exactly the ALPHA session remains
    ("screen_contains", "s47b_picker_filtered", "Run the alpha check."),
    ("screen_not_contains", "s47b_picker_filtered", "Beta prompt only."),
    ("screen_contains", "s47b_after_picker_resume", "Resumed session"),
    ("screen_contains", "s47b_after_picker_resume", "$ echo ALPHA-TOOL-47"),
    ("screen_regex", "s47b_after_picker_resume", r"(?m)^\s*ALPHA-TOOL-47\s*$"),
    ("screen_contains", "s47b_after_picker_resume", "ALPHA-TURN-47"),
    # ... and the OTHER session's content is still not in the replayed view
    ("screen_not_contains", "s47b_after_picker_resume", "BETA-TURN-47"),

    # Alt-screen cycles are not observable through ConPTY (it consumes
    # ESC[?1049h/l); the clean exit is the gate.
    ("restore_markers",),
]
