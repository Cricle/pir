# -*- coding: utf-8 -*-
import os

# W16-L / Task D part 1 (b) — "settings and theme survive a relaunch".
#
# The SECOND process of the settings-persistence pair. s50_settings_theme_
# warnings changed theme -> light and warnings.anthropicExtraUsage -> false
# through the real /settings menu and proved the values on disk with a real
# bash tool call. This scenario starts a FRESH pir process against the SAME
# agent dir and asserts the persisted values are in effect:
#   * the boot frame is drawn with the LIGHT palette — asserted on the raw
#     stream: the light accent SGR `ESC[38;5;97m` (xterm-256) is present and
#     the dark accent `ESC[38;5;140m` is not. Calibrated from the run that
#     produced this scenario: s50_boot.log (dark) has 140 x2 / 97 x0,
#     s50_disk.log (light) has 97 x32.
#   * /settings -> search `theme` -> Enter opens the theme submenu and the
#     cursor row is `→ light: light`, i.e. the persisted theme is the
#     selected value (the same row shows `→ dark: dark` in a dark session).
#   * /settings -> search `warnings` -> Enter shows
#     `Anthropic extra usage: false`.
#
# HOW IT REACHES s50'S AGENT DIR (harness contract, read not assumed):
#   * run_battery gives every scenario its own `<run>/<scenario>/agent` and
#     passes it as both --agent-dir (child PIR_AGENT_DIR) and the child cwd;
#     a scenario module cannot change --agent-dir.
#   * `ENV` is merged AFTER the driver sets PIR_AGENT_DIR
#     (tui_drive.py:323-327), so `ENV = {"PIR_AGENT_DIR": <s50 agent dir>}`
#     points the child's global settings store (and theme load) at s50's dir.
#     This is the s47b pattern (PIR_SESSION_DIR there; the settings store is
#     keyed by the agent dir here).
#   * The run id is taken from W16L_RUN_ID when exported, else from the newest
#     `out/*` directory whose s50 agent dir carries the marker (theme ==
#     light). With neither, the agent dir does not resolve and the content
#     checks fail loudly — never a silent pass.
#
# NOTE ordering: the scenario runs AFTER s50 because `s50_...` sorts before
# `s50b_...` and the battery runs the discovered list in order. The driver
# exec's this module in its own process at that point, so the marker written
# by s50 already exists on disk.
NAME = "s50b_settings_after_relaunch"
SEED_AGENT = False       # the child uses s50's agent dir (ENV), not this one
TIMEOUT = 90

import json  # noqa: E402  (scenario modules are exec'd, not imported)

S50_NAME = "s50_settings_theme_warnings"
PERSISTED_THEME = "light"
PERSISTED_WARN = False


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
    """Verbatim Win32 path (see s47b: this host has LongPathsEnabled=0)."""
    import os
    if os.name != "nt":
        return path
    absolute = os.path.abspath(path)
    if absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def _agent_dir(run_id):
    import os
    return os.path.join(_out_root(), run_id, S50_NAME, "agent")


def _persisted(agent):
    """The values s50 wrote, or None when the file is missing/unrelated."""
    import json
    import os
    try:
        with open(_win(os.path.join(agent, "settings.json")),
                  encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return None
    warnings = data.get("warnings") if isinstance(data, dict) else None
    warn = warnings.get("anthropicExtraUsage") if isinstance(warnings, dict) else None
    if data.get("theme") != PERSISTED_THEME or warn is not PERSISTED_WARN:
        return None
    return data


def _resolve_run_id():
    import os

    pinned = os.environ.get("W16L_RUN_ID", "").strip()
    if pinned and _persisted(_agent_dir(pinned)):
        return pinned
    out_root = _out_root()
    try:
        names = [n for n in os.listdir(out_root)
                 if not n.startswith("_")
                 and os.path.isdir(os.path.join(out_root, n))]
    except OSError:
        return None
    names.sort(key=lambda n: os.path.getmtime(os.path.join(out_root, n)),
               reverse=True)
    for name in names:
        if _persisted(_agent_dir(name)):
            return name
    return None


_RUN_DIR = _active_run_dir()
_RUN_ID = os.path.basename(_RUN_DIR) if _RUN_DIR else _resolve_run_id()
_AGENT = (os.path.join(_RUN_DIR, S50_NAME, "agent") if _RUN_DIR else
          (_agent_dir(_RUN_ID) if _RUN_ID
           else os.path.join(_out_root(), "UNRESOLVED", S50_NAME, "agent")))

ENV = {"PIR_AGENT_DIR": _AGENT}
# The persisted values, surfaced in the report and used by verify_on_disk.
PERSISTED = _persisted(_AGENT)

# Raw color markers (xterm-256 SGR). The light theme's accent is 38;5;97 and
# the dark theme's is 38;5;140 (both produced by build_palette -> ansi index).
CODES = {"light_accent": "\x1b[38;5;97m", "dark_accent": "\x1b[38;5;140m"}

OPS = [
    ("wait_quiet", 6.0),
    ("snap", "s50b_boot"),
    ("log", "open /settings in the relaunched process"),
    ("send", b"/settings\r"),
    ("wait", 1.5),
    ("log", "search 'theme' -> the Theme row"),
    ("send", b"theme"),
    ("wait", 1.2),
    ("snap", "s50b_theme_search"),
    ("log", "Enter opens the theme submenu; the persisted theme is selected"),
    ("send", b"\r"),
    ("wait", 1.2),
    ("snap", "s50b_theme_menu"),
    ("log", "Esc x2 leaves the submenu/menu"),
    ("send", b"\x1b"),
    ("wait", 0.6),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("log", "re-open /settings for the warnings row"),
    ("send", b"/settings\r"),
    ("wait", 1.2),
    ("send", b"warnings"),
    ("wait", 1.2),
    ("snap", "s50b_warnings_search"),
    ("send", b"\r"),
    ("wait", 1.2),
    ("snap", "s50b_warnings_menu"),
    ("send", b"\x1b"),
    ("wait", 0.6),
    ("send", b"\x1b"),
    ("wait", 0.8),
]

CHECKS = [
    # ---- the light palette is in effect from the very first frame: the light
    # accent SGR appears and the dark accent SGR does not. (The boot frame
    # draws the accent on the `pir` header / cursor rows; see the calibration
    # note in the header.) ----
    ("log_count", "s50b_boot", CODES["light_accent"], ">=1"),
    ("log_count", "s50b_boot", CODES["dark_accent"], 0),
    # ---- /settings opens and the Theme row's submenu marks `light` as the
    # current value (row render: `<cursor><name>: <value>`) ----
    ("screen_contains", "s50b_theme_search", "Search: theme"),
    ("screen_contains", "s50b_theme_menu", "→ light: light"),
    ("screen_not_contains", "s50b_theme_menu", "→ dark: dark"),
    # ---- the persisted warning toggle is visible in its submenu ----
    ("screen_contains", "s50b_warnings_menu", "Anthropic extra usage: false"),
    ("restore_markers",),
]


# ---------------------------------------------------------------------------
# On-disk verification helper (same shape as s44's `verify_on_disk`): the
# battery's CHECKS vocabulary is screen/log only, so the direct file assertion
# is callable by the reporter:
#
#   python -c "import runpy;m=runpy.run_path('scenarios/s50b_...py');\
#              print(m['verify_on_disk']('<run dir>/s50_settings_theme_warnings/agent'))"
# ---------------------------------------------------------------------------
def verify_on_disk(agent_dir_arg=None):
    import json
    import os
    agent = agent_dir_arg or _AGENT
    try:
        with open(_win(os.path.join(agent, "settings.json")),
                  encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError) as exc:
        return {"settings_theme": (False, f"cannot read settings.json: {exc}"),
                "settings_warning": (False, "cannot read settings.json")}
    warnings = data.get("warnings") if isinstance(data, dict) else None
    warn = warnings.get("anthropicExtraUsage") if isinstance(warnings, dict) else None
    return {
        "settings_theme": (data.get("theme") == PERSISTED_THEME,
                           f"theme={data.get('theme')!r} (want {PERSISTED_THEME!r})"),
        "settings_warning": (warn is PERSISTED_WARN,
                             f"warnings.anthropicExtraUsage={warn!r} "
                             f"(want {PERSISTED_WARN!r})"),
    }
