# -*- coding: utf-8 -*-
# s31 — console hygiene on TUI enter and exit (2026-10-15 user report:
# "进入交互页面和退出的时候没有清除控制台").
#
# The TUI must start from a blank console in BOTH modes (the alternate
# buffer may be reused by the terminal; the main screen still shows the
# shell's earlier output) and must leave a blank console on the way out
# (before the transcript dump). Product anchors:
#   * enter: `EnterAlternateScreen` -> `Clear(All)` -> `MoveTo(0,0)`
#     (terminal.rs, right after the alt-screen switch);
#   * exit: `LeaveAlternateScreen` -> `Clear(All)` -> `MoveTo(0,0)` ->
#     transcript dump (terminal.rs `cleanup`, plus the panic-path
#     `cleanup_terminal`).
# pyte erases these sequences, so the assertion reads the RAW captured
# stream (`log_ordered`). LIMIT (2026-10-29): Windows ConPTY consumes the
# child's ESC[?1049h/ESC[?1049l and never forwards them (see
# run_battery.restore_markers + probe_alt_passthrough.py), so the
# alt-screen switches themselves cannot be asserted here. What ConPTY does
# preserve is asserted instead: at boot a clear-all precedes the first
# frame's banner, and at exit `restore_markers` owns exit 0 + clean quit +
# bracketed-paste-off + cursor-show.
NAME = "s38_console_clear"

OPS = [
    ("wait_quiet", 4.0),
    ("snap", "s38_boot"),
]

CHECKS = [
    # Entry: the console is cleared before the first frame is painted
    # (the banner text must not be preceded by shell residue).
    ("log_ordered", "s38_boot",
     ["\x1b[2J", " v0.0.1"]),
    # Boot really clears more than once: the startup clear plus the
    # clear-all that follows the surface takeover.
    ("log_count", "s38_boot", "\x1b[2J", ">=2"),
    # Exit: exit 0 + clean quit + bracketed paste off x1 + cursor shown.
    # (1049l cannot be asserted through ConPTY; see the header note.)
    ("restore_markers",),
]
TIMEOUT = 90
