# -*- coding: utf-8 -*-
# Raw terminal-input hook (`ctx.ui.onTerminalInput`) on the production ConPTY
# input path.
#
# The extension registers a hook on `session_start` and replaces a typed `!`
# with `bang`. The TUI must dispatch the key through the plugin host BEFORE
# its built-in handling, apply the replacement, and never type the original
# `!`. Plain typing must still work afterwards (the hook only sees the one
# chunk).
#
# The first prompt is what fires `session_start` in pir (the same lifecycle
# the TUI uses), so the scenario submits a minimal `hi` first; the mock LLM
# keeps it hermetic.

import os

_HERE = os.path.dirname(os.path.abspath(__file__))
_EXT_DIR = os.path.join(_HERE, "..", "out")
os.makedirs(_EXT_DIR, exist_ok=True)
_EXT_PATH = os.path.join(_EXT_DIR, "terminal_input_ext.js")
with open(_EXT_PATH, "w", encoding="utf-8") as _f:
    _f.write(
        """
        export default function (pi) {
            pi.on("session_start", (event, ctx) => {
                ctx.ui.onTerminalInput((data) => {
                    if (data === "!") return { data: "bang" };
                    return undefined;
                });
            });
        }
        """
    )

NAME = "s37_terminal_input_replace"
OFFLINE = False
ARGS = ["-e", _EXT_PATH]
OPS = [
    ("wait_quiet", 4),
    ("log", "start a run so session_start registers the terminal-input hook"),
    ("send", b"hi\r"),
    ("wait", 3.0),
    ("snap", "s37_after_run"),
    ("log", "type '!': the hook replaces it with 'bang'"),
    ("send", b"!"),
    ("wait", 1.0),
    ("snap", "s37_replaced"),
    ("log", "plain typing still reaches the editor"),
    ("send", b"ok"),
    ("wait", 0.6),
    ("snap", "s37_plain"),
]
CHECKS = [
    ("editor_contains", "s37_replaced", "bang"),
    ("editor_not_contains", "s37_replaced", "!"),
    ("editor_contains", "s37_plain", "bangok"),
    ("restore_markers",),
]
