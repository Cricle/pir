# -*- coding: utf-8 -*-
# W16-L / Task D part 1 (a) — "settings and theme actually take effect".
#
# WHAT IT DRIVES (all real product paths; the hermetic mock only supplies the
# final bash tool call that prints the on-disk settings.json):
#   1. `/settings` opens the real Settings overlay.
#   2. Type-to-search `theme` narrows the list to the Theme row; the row's
#      LEFT/RIGHT handler (`SettingsMenu::handle_key` -> `cycle` on the MAIN
#      list) cycles dark -> light and the host applies + persists it
#      (`apply_menu_action` -> `set_theme` + `set_global_field("theme")`).
#      The status notice is `Theme: light` (NOT `Saved setting:` — that prefix
#      is only used by `apply_settings_action`, i.e. non-theme rows).
#   3. Esc clears the search query, `warnings` narrows to the Warnings
#      submenu; Enter opens it; RIGHT flips `Anthropic extra usage`
#      true -> false and the notice is `Saved setting:
#      warnings.anthropicExtraUsage`.
#   4. One mock turn runs a REAL bash command that json-loads
#      `settings.json` (the child cwd == PIR_AGENT_DIR == the per-scenario
#      agent dir, so the relative path is the persisted global file) and
#      prints `THEME=...` / `WARN=...`. The printed values are concatenated in
#      the source (`'THEME' + '='`) so the echoed call line cannot satisfy the
#      screen assertion — only the real file contents can.
#
# The second half (a fresh process reading the same agent dir) is
# s50b_settings_after_relaunch; it resolves THIS scenario's agent dir the same
# way s47b_resume_after_quit resolves s47's session dir (W16L_RUN_ID or the
# newest out/* carrying the marker below).
NAME = "s50_settings_theme_warnings"
OFFLINE = False          # the bash-print turn goes through the mock provider
TIMEOUT = 150
# Marker s50b looks for in this run's settings.json (theme persisted to light).
PERSISTED_THEME = "light"

_SETTINGS_PRINT = (
    "python -c '"
    "import json\n"
    'd = json.load(open("settings.json"))\n'
    'print("THEME" + "=" + str(d.get("theme")))\n'
    'print("WARN" + "=" + str((d.get("warnings") or {}).get("anthropicExtraUsage")))\n'
    "'"
)

MOCK_SCRIPT = [
    {"tool_call": {"name": "bash",
                   "arguments": {"command": _SETTINGS_PRINT}}},
    {"text": "S50-DONE"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s50_boot"),
    ("log", "open /settings"),
    ("send", b"/settings\r"),
    ("wait", 1.5),
    ("snap", "s50_open"),
    ("log", "type-to-search 'theme' -> the Theme row"),
    ("send", b"theme"),
    ("wait", 1.2),
    ("snap", "s50_search"),
    ("log", "RIGHT on the main Theme row: dark -> light (apply + persist)"),
    ("send", b"\x1b[C"),
    ("wait", 1.5),
    ("snap", "s50_theme_changed"),
    ("log", "Esc clears the search query"),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("snap", "s50_search_cleared"),
    ("log", "type-to-search 'warnings' -> the Warnings submenu"),
    ("send", b"warnings"),
    ("wait", 1.2),
    ("snap", "s50_warnings_search"),
    ("log", "Enter opens the warnings submenu"),
    ("send", b"\r"),
    ("wait", 1.2),
    ("snap", "s50_warnings_menu"),
    ("log", "RIGHT flips Anthropic extra usage true -> false"),
    ("send", b"\x1b[C"),
    ("wait", 1.2),
    ("snap", "s50_warning_changed"),
    ("log", "Esc backs out of the submenu, Esc closes the menu"),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "s50_menu_closed"),
    ("log", "mock turn: a real bash call prints the on-disk settings.json"),
    ("send", b"Check the settings file on disk.\r"),
    ("wait", 8.0),
    ("wait_quiet", 4.0),
    ("snap", "s50_disk"),
]

CHECKS = [
    # ---- the dialog opens ----
    ("screen_contains", "s50_open", "Settings"),
    ("screen_contains", "s50_open", "enter select"),
    # ---- type-to-search really filtered and the Theme row is selected ----
    ("screen_contains", "s50_search", "Search: theme"),
    ("screen_contains", "s50_search", "Theme"),
    # ---- theme changed (notice text; theme rows use `Theme: <name>`) ----
    ("screen_contains", "s50_theme_changed", "Theme: light"),
    # ... and the palette really swapped: the light accent SGR appears after
    # the change and never before (dark accent is 140, light accent is 97).
    ("log_count", "s50_boot", "\x1b[38;5;97m", 0),
    ("log_count", "s50_theme_changed", "\x1b[38;5;97m", ">=1"),
    # ---- warning submenu opened on the seeded default, then flipped ----
    ("screen_contains", "s50_warnings_menu", "Anthropic extra usage"),
    ("screen_contains", "s50_warnings_menu", "true"),
    ("screen_contains", "s50_warning_changed", "Anthropic extra usage: false"),
    # ---- the exact "Saved setting" notice the warning row emits ----
    ("screen_contains", "s50_warning_changed", "Saved setting: warnings.anthropicExtraUsage"),
    # ---- after closing the overlay both notices are in the transcript ----
    ("screen_contains", "s50_menu_closed", "Theme: light"),
    ("screen_contains", "s50_menu_closed", "Saved setting: warnings.anthropicExtraUsage"),
    # ---- the on-disk file (read by the real bash tool) carries both values ----
    ("screen_contains", "s50_disk", "THEME=light"),
    ("screen_contains", "s50_disk", "WARN=False"),
    ("screen_contains", "s50_disk", "S50-DONE"),
    ("restore_markers",),
]
