# -*- coding: utf-8 -*-
# W16-L / Task D part 2 — "an error path stays usable".
#
# A REAL model turn (hermetic mock supplies the calls; the tools, scheduler,
# rendering and session persistence are the product) in which two tool calls
# fail for realistic reasons:
#
#   step 1  write  w16l_notes.txt      -> success (creates the edit target)
#   step 2  read   missing_w16l.txt    -> FAIL: "Path not found: <path>"
#           (src/coding_agent/tools/read.rs:235-240)
#   step 3  edit   w16l_notes.txt with an `oldText` that does not exist
#                                      -> FAIL: "Could not find the exact text
#           in w16l_notes.txt. The old text must match exactly including all
#           whitespace and newlines." (src/coding_agent/tools/edit.rs:314-323)
#   step 4  text   S51-ERRORS-HANDLED  -> the turn CONTINUES to a final answer
#   step 5  bash   echo S51-RECOVERED  -> a later turn's tool still works
#   step 6  text   S51-RECOVERY-OK
#
# Assertions (the scenario's point):
#   * each failure renders in its OWN tool block: the read call line and
#     result row are on screen, as are the edit call line and its error, and
#     each error text occurs EXACTLY ONCE (no duplicate notice);
#   * the failure is not a global banner: no `Error: ` prefixed notice row
#     exists in the transcript (that is the ChatEntry::Notice Error format,
#     src/tui/app.rs:2004-2006 — tool errors stay inside their blocks);
#   * the turn continues (`S51-ERRORS-HANDLED`) instead of aborting;
#   * the app is responsive (`/help` renders its command list right after);
#   * the session keeps working: the follow-up turn runs a real bash tool and
#     answers (`echo S51-RECOVERED` -> S51-RECOVERED, then S51-RECOVERY-OK).
NAME = "s51_error_path_stays_usable"
OFFLINE = False  # the turns go through the injected mock provider
TIMEOUT = 180

_OLD = "THIS-TEXT-IS-NOT-IN-THE-FILE-W16L"

MOCK_SCRIPT = [
    {"tool_call": {"name": "write",
                   "arguments": {"path": "w16l_notes.txt",
                                 "content": "alpha\nbeta\n"}}},
    {"tool_call": {"name": "read",
                   "arguments": {"path": "missing_w16l.txt"}}},
    {"tool_call": {"name": "edit",
                   "arguments": {"path": "w16l_notes.txt",
                                 "edits": [{"oldText": _OLD,
                                            "newText": "replacement"}]}}},
    {"text": "S51-ERRORS-HANDLED"},
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "echo S51-RECOVERED"}}},
    {"text": "S51-RECOVERY-OK"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s51_boot"),
    ("log", "prompt -> write, read(missing), edit(stale oldText), final text"),
    ("send", b"Tidy the notes file and report what went wrong.\r"),
    ("wait", 10.0),
    ("wait_quiet", 4.0),
    ("snap", "s51_after_errors"),
    ("log", "/help immediately after the failed turn: the app must respond"),
    ("send", b"/help\r"),
    ("wait", 2.0),
    ("snap", "s51_after_help"),
    ("log", "a follow-up turn: a real bash call must still execute"),
    ("send", b"Run the recovery check.\r"),
    ("wait", 9.0),
    ("wait_quiet", 4.0),
    ("snap", "s51_recovery"),
]

CHECKS = [
    # ---- each failure renders in its OWN block, above the turn's answer:
    # the call line precedes the error inside that block (log_ordered proves
    # the transcript order: write, then the read call + its error, then the
    # edit call + its error, then the final text) ----
    ("screen_contains", "s51_after_errors", "read missing_w16l.txt"),
    ("screen_count", "s51_after_errors", "Path not found:", 1),
    ("screen_contains", "s51_after_errors", "edit w16l_notes.txt"),
    ("screen_count", "s51_after_errors",
     "Could not find the exact text in w16l_notes.txt", 1),
    # the transcript order (rendered text, not the raw stream: the call line
    # carries SGR between the tool name and the path, so a raw substring
    # search for `write w16l_notes.txt` cannot match)
    ("screen_regex", "s51_after_errors",
     r"(?s)write .*?w16l_notes\.txt.*?read missing_w16l\.txt.*?Path not found:"
     r".*?edit w16l_notes\.txt.*?Could not find the exact text in w16l_notes\.txt"
     r".*?S51-ERRORS-HANDLED"),
    # ---- ... inside their own blocks, NOT a global error notice ----
    ("screen_not_contains", "s51_after_errors", "Error: "),
    # the blocks really flipped to the error background (dark theme
    # `toolErrorBg` = SGR 48;5;52; `toolSuccessBg` = 48;5;23 for the write).
    # A pyte render drops colors, so this is asserted on the raw stream.
    ("log_count", "s51_after_errors", "\x1b[48;5;52m", ">=2"),
    ("log_count", "s51_after_errors", "\x1b[48;5;23m", ">=1"),
    # ---- the turn continued to a final answer ----
    ("screen_contains", "s51_after_errors", "S51-ERRORS-HANDLED"),
    # ---- the app is responsive right after ----
    ("screen_contains", "s51_after_help",
     "Commands: /model /thinking /clear /compact"),
    # ---- and the session continues with a successful turn ----
    ("screen_contains", "s51_recovery", "S51-RECOVERY-OK"),
    ("screen_contains", "s51_recovery", "S51-RECOVERED"),
    ("screen_not_contains", "s51_recovery", "Error: "),
    ("restore_markers",),
]
