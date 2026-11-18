# -*- coding: utf-8 -*-
# s31 — terminal replies on the input path: what the gate can and cannot claim.
#
# The ConPTY cannot be made to answer a colour query late on demand, so the
# scenario *types the reply bytes as input* — the shape a real terminal's late
# reply reaches the input path. Two replies are fed plus a plain word:
#
#   * OSC 11 (`ESC ] 11 ; rgb:… BEL`) is a terminated control string whose header
#     the gate recognizes; it is CLAIMED and never reaches the editor.
#   * DA1 (`ESC [ ? 61 ; … c`) loses its ESC record in the Windows console
#     (VK 0, no character); the remaining `[?61;…c` bytes are indistinguishable
#     from a user typing them, so the gate deliberately PASSES them through.
#     This is the documented, unit-tested behaviour
#     (`stdin_gate_leaves_an_esc_stripped_reply_tail_alone` in
#     tests/suites/tui_stdin_decoding.rs). Claiming it would require console
#     record-level ownership that the ConPTY WriteFile injection cannot provide.
#
# The assertions therefore pin the REAL contract: the OSC reply is swallowed,
# the DA1 tail is present (not a defect), and the plain word still types.
NAME = "s31_late_reply_swallowed"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("send", b"\x1b[?61;6;7;21;22;23;24;28;32;42c"),
    ("wait", 1.0),
    ("send", b"\x1b]11;rgb:1a1a/1a1a/2b2b\x07"),
    ("wait", 1.0),
    ("send", b"hello-late"),
    ("wait", 1.5),
    ("snap", "s31_after"),
]
CHECKS = [
    ("restore_markers",),
    # The OSC 11 control string is claimed by the gate: never typed.
    ("editor_not_contains", "s31_after", "rgb:1a1a"),
    ("screen_not_contains", "s31_after", "rgb:1a1a"),
    # The ESC-stripped DA1 tail is deliberately passed through as typing (see
    # header); the old assertion demanding its absence contradicted the design.
    ("editor_contains", "s31_after", "[?61;"),
    # Positive control: ordinary typing after the replies still lands.
    ("editor_contains", "s31_after", "hello-late"),
]
