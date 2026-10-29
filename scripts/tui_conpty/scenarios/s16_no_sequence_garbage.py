# -*- coding: utf-8 -*-
# New 2026-10-15 - DA1-LEAK REGRESSION LOCK.
# Bug locked: the terminal-colors query's DA1 reply (`ESC[?61;6;7;21;22;...c`)
# leaked into the input stream and was TYPED into the prompt as literal
# `[?61;...` garbage. Recorded evidence: session
# 2026-10-15T06-27-23-200Z_01a13e3e-75c0-70a2-a819-05f6363dbf81.jsonl whose
# first user message is the literal `[?61;6;7;21;22;23;24;28;32;42c/quit`.
# Fixed app-side (src/tui/theme/terminal_colors.rs + terminal_reply.rs);
# probe8 on the fixed build shows /help executing with no `[?61;` bytes.
# This scenario is the permanent lock and touches no keys: it boots, snapshots
# the settled screen, and asserts the screen stays clean and the footer is a
# normal idle status. It also locks the paste markers out of the boot screen.
# Observed 2026-10-15 on the fixed target-alt build: no sequence garbage; the
# footer reads `?/1.0M ... (openai) gpt-5 • max` (`?/1.0M` = the
# `?/`-style token-counter/context indicator).
NAME = "s16_no_sequence_garbage"
OPS = [
    ("wait_quiet", 4),
    ("log", "DA1-leak lock: settled boot screen must be clean"),
    ("snap", "s16_boot"),
    ("wait", 1.0),
    ("snap", "s16_idle"),
]
CHECKS = [
    ("screen_not_contains", "s16_boot", "[?61;"),
    ("screen_not_contains", "s16_boot", "[?62;"),
    ("screen_not_contains", "s16_boot", "[?63;"),
    ("screen_not_contains", "s16_boot", "[200~"),
    ("screen_not_contains", "s16_boot", "[201~"),
    # The type-in is asynchronous; the second, later snapshot must stay clean too.
    ("screen_not_contains", "s16_idle", "[?61;"),
    ("screen_not_contains", "s16_idle", "[?62;"),
    ("screen_not_contains", "s16_idle", "[?63;"),
    # Normal idle status: the `?/<tokens>` counter renders in the footer.
    ("screen_regex", "s16_idle", r"\?/\d+(?:\.\d+)?[kKmM]?"),
    ("restore_markers",),
]
