# -*- coding: utf-8 -*-
# s41 — the shell pool's embedded brush member: with the `brush` cargo feature
# (in `full`) and no settings `shellPath`, the pooled `bash` call runs through
# the hidden `pir __brush` instead of spawning Git Bash (src/shell_embed/).
#
# HERMETIC (mock LLM): MOCK_SCRIPT drives one bash tool call that runs
#   echo $((1+1))          -> 2      (arithmetic expansion)
#   echo abc | tr a-z A-Z  -> ABC    (a pipeline through an external builtin)
# Both are shell features a plain `cmd.exe` cannot run, so seeing the outputs
# proves a real bash-compatible interpreter answered; the `command not found`
# guard proves the call did not fail because the embedded shell was missing.
#
# Assertions read the rendered screen and target OUTPUT lines: `2` is matched
# as a standalone row (the echoed command line contains `1+1`, not a bare 2),
# `ABC` is unique, and the boot screen is the control.
NAME = "s41_brush_shell"
OFFLINE = False  # the app must reach the injected mock over HTTP
TIMEOUT = 120
MOCK_SCRIPT = [
    {"tool_call": {
        "name": "bash",
        "arguments": {"command":
                      "echo $((1+1))\n"
                      "echo abc | tr a-z A-Z"}}},
    {"text": "BRUSH-SHELL-TURN-DONE"},
]
OPS = [
    ("wait_quiet", 4.0),
    ("snap", "s41_boot"),
    ("log", "prompt -> mock: one bash call running arithmetic + a pipeline"),
    ("send", b"Run the shell checks.\r"),
    ("wait", 8.0),
    ("wait_quiet", 3.0),
    ("snap", "s41_done"),
]
CHECKS = [
    # control: no shell output before the prompt
    ("screen_not_contains", "s41_boot", "BRUSH-SHELL-TURN-DONE"),
    ("screen_not_contains", "s41_boot", "ABC"),
    # arithmetic expansion produced a standalone 2
    ("screen_regex", "s41_done", r"(?m)^\s*2\s*$"),
    # the pipeline produced ABC
    ("screen_contains", "s41_done", "ABC"),
    # the shell did not fail because brush/the member was unavailable
    ("screen_not_contains", "s41_done", "command not found"),
    ("screen_not_contains", "s41_done", "not implemented"),
    # the turn settled normally
    ("screen_contains", "s41_done", "BRUSH-SHELL-TURN-DONE"),
    ("restore_markers",),
]
