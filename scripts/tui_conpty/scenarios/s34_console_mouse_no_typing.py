# -*- coding: utf-8 -*-
# Console-mouse-report lock (2026-10-16).
#
# Bug locked: on Windows the console drops the record that carries a mouse
# report's ESC, so the report's remaining bytes (`[<64;40;20M`) arrive as
# plain per-byte key records — and used to be TYPED into the prompt as the
# literal text `[<64;40;20M`. The stdin gate now claims that tail
# (`StdinGate::claim_console_mouse` in src/tui/stdin_buffer.rs) and turns it
# into the same `Event::Mouse` the ESC-prefixed shape gets, including the
# key-up record that pairs with the byte that ended the run
# (`pending_byte_release` — the trailing `M` release used to leak as a key
# event). Scrollbar/wheel/selection all sit on that path.
#
# The reports are written the way a real console delivers them: raw bytes
# with the ESC record omitted. ConPTY synthesizes the per-byte console key
# records (with `VkKeyScan`'s SHIFT state on `<`/`M`), so this scenario
# exercises the production input path, not a unit stub.
#
# Window note: the console enqueues the whole report in ~4 ms (measured with
# `probe_paste_records.py`, whose dump now carries per-record `t_ms`), but the
# records reach the gate one crossterm poll at a time — the 10 ms escape window
# was too short and the `[` was flushed as typing (this scenario failed), so a
# plausible report prefix waits out the SEQUENCE window (50 ms). Re-run this
# scenario whenever the stdin gate's windows change.
#
# Assertions are deliberately about *survival*, not about scroll offsets: the
# boot screen's transcript is empty, so a wheel notch cannot move anything.
# What must hold is that no report byte ever reaches the editor or the screen,
# and that ordinary typing still lands afterwards (the claim must not eat
# real keys).

NAME = "s34_console_mouse_no_typing"
OPS = [
    ("wait_quiet", 4),
    ("snap", "s34_boot"),
    ("log", "ESC-less wheel-up report tail (the console dropped the ESC record)"),
    ("send", b"[<64;40;20M"),
    ("wait", 1.5),
    ("snap", "s34_after_esc_less_wheel"),
    ("log", "ESC-less left-button press/drag/release across 5,3 -> 10,3"),
    ("send", b"[<0;5;3M[<32;10;3M[<0;10;3m"),
    ("wait", 1.5),
    ("snap", "s34_after_esc_less_drag"),
    ("log", "the ESC-prefixed shape, which crossterm parses itself"),
    ("send", b"\x1b[<64;40;20M"),
    ("wait", 1.5),
    ("snap", "s34_after_esc_wheel"),
    ("log", "typing still works after the reports"),
    ("send", b"ok"),
    ("wait", 1.0),
    ("snap", "s34_typing"),
]
CHECKS = [
    # The pre-fix bug typed the report into the prompt; the claim must leave
    # the editor untouched.
    ("editor_not_contains", "s34_after_esc_less_wheel", "64;"),
    ("editor_not_contains", "s34_after_esc_less_drag", "[<"),
    ("editor_not_contains", "s34_after_esc_wheel", "[<"),
    # …and no report text may be painted anywhere either.
    ("screen_not_contains", "s34_after_esc_less_wheel", "[<64;"),
    ("screen_not_contains", "s34_after_esc_less_drag", "[<0;5;3M"),
    ("screen_not_contains", "s34_after_esc_wheel", "[<64;"),
    # The claim is byte-exact: plain typing still lands in the editor.
    ("editor_contains", "s34_typing", "ok"),
    ("screen_regex", "s34_typing", r"\?/\d+(?:\.\d+)?[kKmM]?"),
    ("restore_markers",),
]
