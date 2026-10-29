# -*- coding: utf-8 -*-
# s17 — LARGE paste folds into the paste marker instead of inserting raw text.
# Intent (unchanged): a paste above the fold thresholds is replaced by the
# `[paste #N +L lines]` marker in the editor, the payload itself never renders,
# and no sequence garbage leaks. Cleanup: ctrl+c clears the editor
# (TuiAction::Clear, src/tui/keybindings.rs:126), then backspaces as a safety
# net.
#
# WHY THE CLIPBOARD AND NOT `ESC [ 2 0 0 ~` (recalibrated 2026-10-15):
# the fold only runs on the editor's paste path (`Editor::paste_text`,
# src/tui/editor.rs:334+). On Unix crossterm turns the bracketed-paste markers
# into `Event::Paste` and that path is reached; on this ConPTY it is NOT
# reachable at all, because the console translates a bracketed paste itself:
# `probe_paste_records.py` (raw INPUT_RECORD dump, same console mode as the
# TUI) shows zero records for `ESC [ 2 0 0 ~` on its own and a REAL Up arrow
# for an `ESC [ A` written between the markers — the markers are swallowed and
# the payload is delivered as ordinary synthesized keystrokes (LF arrives as
# Ctrl+Enter). Verified at the app level too: the CR-separated variant of this
# payload (probe_s17_fold_cr.py) renders as separately submitted lines, i.e.
# no paste event exists. The app's own clipboard paste (alt+v on Windows,
# TuiAction::Paste, src/tui/keybindings.rs:141 → `paste_from_clipboard`,
# src/tui/terminal.rs:5016) calls `paste_text` directly and IS live, so this
# scenario drives the fold through it. The bracketed-paste route keeps its own
# coverage in s17b_paste_lines (line breaks must survive; no fold is possible
# there on this platform).
#
# CALIBRATED 2026-10-15 by observation on the then-current binary (target-alt
# mtime 2026-10-15 16:29): out/probe_s17_clip/ renders the editor row exactly
# ` [paste #1 +12 lines]` for this payload, and ctrl+c clears it. Confirmed by
# the battery run out/20261015-172010 (editor region rows 12..12 hold the
# marker; 20/20 scenarios PASS). Marker shape cross-checked against the fold
# code: >PASTE_FOLD_MAX_LINES=10 lines
# (src/tui/editor.rs:67) formats as `[paste #{id} +{lines} lines]`
# (src/tui/editor.rs:363, :379-383); 12 lines × 12 chars + 11 newlines = 155
# chars stays under PASTE_FOLD_MAX_CHARS=1000, so the lines form is the one to
# expect. Editor window: with multi-line content the box scrolls to the cursor
# and shows the last 3 rows (observed with a 10-line clipboard payload,
# out/probe_s17_clip10/) — the single marker row is all this paste leaves.
NAME = "s17_paste_fold"
TIMEOUT = 90
_PAYLOAD = "\n".join("fold-line-%02d" % i for i in range(1, 13))
OPS = [
    ("wait_quiet", 4),
    ("log", "clipboard 12-line payload -> alt+v -> folded marker"),
    ("clipset", _PAYLOAD),
    ("send", b"\x1bv"),
    ("wait", 1.5),
    ("snap", "s17_after"),
    ("log", "cleanup: ctrl+c clears the editor, backspaces safety net"),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "s17_cleared"),
]
CHECKS = [
    ("editor_contains", "s17_after", "[paste #1 +12 lines]"),
    # The folded payload is never inserted: none of it may render.
    ("screen_not_contains", "s17_after", "fold-line-"),
    ("screen_not_contains", "s17_after", "[200~"),
    ("screen_not_contains", "s17_after", "[201~"),
    ("editor_not_contains", "s17_cleared", "fold-line-"),
    ("editor_not_contains", "s17_cleared", "[paste #"),
    ("restore_markers",),
]

# The fold is driven through the real system clipboard (alt+v), which a
# locked desktop / stuck clipboard owner makes unreachable: the runner SKIPs
# with the reason recorded instead of failing on an environment property.
NEEDS_CLIPBOARD = True
