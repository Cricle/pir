# -*- coding: utf-8 -*-
# s40 — the native command surface reachable from inside the TUI: the shell
# tools put `PIR_BIN` on the child environment and prepend the binary's
# directory to the child `PATH`, so a model can run `pir grep` / `pir python`
# as plain commands (src/native_cli/mod.rs).
#
# HERMETIC (mock LLM): MOCK_SCRIPT drives one bash tool call that runs the two
# commands through the same shell the agent's `bash` tool uses:
#   * `"$PIR_BIN" grep --version`      -> the system-grep banner (the native
#     subset does not cover `--version`, so this exercises the transparent
#     fallback path);
#   * `"$PIR_BIN" python -c "print(42)"` -> `42` from the embedded RustPython.
# `PIR_BIN` is exported by the bash/powershell tools; `${PIR_BIN:-pir}` keeps
# the command meaningful if it is absent (then the PATH lookup still names the
# same binary).
#
# Assertions read the rendered screen, so they target OUTPUT lines, not the
# echoed command line: the grep banner is asserted with a line-anchored regex
# (the command line starts with `$ "…"`, not `grep`), and `42` as a standalone
# row (the command line contains `print(42)`, not a bare 42).
#
# This scenario needs the shell tool to be usable under the injected mock
# (it is: MOCK_SCRIPT `tool_call` steps are how s18/s39 drive bash/powershell).
NAME = "s40_native_cli_commands"
OFFLINE = False  # the app must reach the injected mock over HTTP
TIMEOUT = 120
MOCK_SCRIPT = [
    {"tool_call": {
        "name": "bash",
        "arguments": {"command":
                      'PIR_BIN="${PIR_BIN:-pir}"; '
                      '"$PIR_BIN" grep --version; '
                      '"$PIR_BIN" python -c "print(42)"'}}},
    {"text": "NATIVE-CLI-TURN-DONE"},
]
OPS = [
    ("wait_quiet", 4.0),
    ("snap", "s40_boot"),
    ("log", "prompt -> mock: one bash call running pir grep --version + pir python -c"),
    ("send", b"Run the native CLI checks.\r"),
    ("wait", 8.0),
    ("wait_quiet", 3.0),
    # `grep --version` prints four banner/copyright lines; the transcript
    # collapses all but the last into "(N earlier lines, ctrl+o to expand)",
    # so expand before asserting on the banner's first line.
    ("log", "expand the collapsed tool output (ctrl+o)"),
    ("send", b"\x0f"),
    ("wait", 1.5),
    ("wait_quiet", 1.5),
    ("snap", "s40_done"),
]
CHECKS = [
    # control: nothing from the commands before the prompt
    ("screen_not_contains", "s40_boot", "NATIVE-CLI-TURN-DONE"),
    # `grep --version` output: the GNU/BSD banner line starts with `grep`
    # (transcript rows carry a one-space gutter, hence the leading `\s*`).
    ("screen_regex", "s40_done", r"(?m)^\s*grep\b.*\d+\.\d+"),
    # `python -c "print(42)"` output: a standalone 42 row.
    ("screen_regex", "s40_done", r"(?m)^\s*42\s*$"),
    # neither command reported a missing/broken implementation
    ("screen_not_contains", "s40_done", "not implemented"),
    ("screen_not_contains", "s40_done", "command not found"),
    # the turn settled normally
    ("screen_contains", "s40_done", "NATIVE-CLI-TURN-DONE"),
    ("restore_markers",),
]
