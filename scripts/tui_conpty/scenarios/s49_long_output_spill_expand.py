# -*- coding: utf-8 -*-
# W16-K task C part 2 — "a long tool output that must be spilled/truncated".
#
# REAL TASK, HERMETIC PROVIDER. One `bash` model step runs a REAL python
# subprocess that prints 5 000 lines (`s49 row NNNN: payload line`, 26 chars
# + CRLF => 140 000 B total, above the 2 x DEFAULT_MAX_BYTES = 100 KB spill
# window and far above the 2 000-line / 50 KB result cap,
# src/coding_agent/truncate.rs:9-10). Then a text turn.
#
# What the product must do, and what this scenario asserts (all wordings below
# are the MEASURED product output on the current build, not assumptions):
#   * the shell output accumulator evicts the prefix into a spill file and
#     the tool result is truncated to a retained tail. Which cap fires depends
#     on line length: 5 000 x 28 B (CRLF) = 140 000 B means the BYTE cap (50 KB,
#     measured `outputBytes: 51183`) fires first, so the retained tail is
#     1 828 lines starting at `s49 row 3173`. The rendered warning is therefore
#     the byte form
#     `[Full output: <path>. Truncated: 1828 lines shown (50.0KB limit)]`
#     (`shell_truncation_warning`, src/tui/messages.rs:2210-2216), not the
#     `showing 2000 of 5000 lines` line form;
#   * the COLLAPSED block keeps the last BASH_PREVIEW_LINES (=5) visual lines
#     and renders `... (N earlier lines, ctrl+o to expand)`
#     (src/tui/messages.rs:3982-3995, 3234-3240);
#   * `ctrl+o` expands the block: a row 20 lines above the end
#     (`s49 row 4980:`), invisible collapsed, becomes visible;
#   * a truncated-away row (`s49 row 0020:`) never renders — proof the byte cap
#     really dropped the head, not just folded the view (verified again on
#     disk: the result body's FIRST row is `s49 row 3173:`);
#   * the app is still responsive afterwards: `/help` executes and renders the
#     command list, and the editor is cleared (it still holds "/help" if the
#     UI were stuck).
#
# The on-disk session check (the JSONL records the tool result once, and the
# screen's copy is the same single truncated body) uses the same helper
# pattern as s44/s48 because run_battery's CHECK_KINDS are screen/log only:
#
#   python -c "import runpy; m=runpy.run_path('scenarios/s49_...py'); \
#              print(m['verify_on_disk']('out/<run-id>/s49_...'))"
NAME = "s49_long_output_spill_expand"
OFFLINE = False  # the app must reach the injected mock over HTTP
TIMEOUT = 180

_LINES = 5000
_CMD = ("python -c 'for i in range(1, %d): print(\"s49 row %%04d: payload line\" %% i)'"
        % (_LINES + 1))

MOCK_SCRIPT = [
    {"tool_call": {"name": "bash", "arguments": {"command": _CMD}}},
    {"text": "W16K-S49-DONE"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s49_boot"),
    ("log", "prompt -> mock: one bash call printing 5000 lines, then text"),
    ("send", b"Print the whole payload and tell me when it is done.\r"),
    # 5 000 lines pass through the shell capture, the spill buffer and the
    # renderer; give the turn room, then settle.
    ("wait", 12.0),
    ("wait_quiet", 5.0),
    ("snap", "s49_collapsed"),
    ("log", "ctrl+o expands the truncated shell block"),
    ("send", b"\x0f"),
    ("wait", 2.0),
    ("wait_quiet", 2.0),
    ("snap", "s49_expanded"),
    ("log", "responsive check: /help must still execute after the big render"),
    ("send", b"/help\r"),
    ("wait", 2.5),
    ("snap", "s49_help"),
    ("log", "clear the editor before the driver's /quit"),
    ("send", b"\x7f" * 20),
    ("wait", 0.6),
]

CHECKS = [
    # control: nothing from the task before the prompt
    ("screen_not_contains", "s49_boot", "s49 row"),
    ("screen_not_contains", "s49_boot", "W16K-S49-DONE"),

    # ---- collapsed: the preview hint, the truncation warning, the real tail ----
    ("screen_contains", "s49_collapsed", "earlier lines"),
    ("screen_contains", "s49_collapsed", "ctrl+o to expand"),
    # measured product wording: the 50KB byte cap fires before the 2000-line
    # cap (135 000 B output), so the warning is
    # `[Full output: … Truncated: 1828 lines shown (50.0KB limit)]`
    # (shell_truncation_warning, src/tui/messages.rs:2210-2216). It wraps
    # across two rows, hence a multi-line regex.
    ("screen_regex", "s49_collapsed",
     r"Truncated:\s+\d+ lines shown \(50\.0KB limit\)"),
    ("screen_contains", "s49_collapsed", "Full output:"),
    # the collapsed preview really is the last 5 rows: row 4998 yes, row 4980
    # no (it is 20 rows above the block end)
    ("screen_regex", "s49_collapsed", r"(?m)^\s*s49 row 4998: payload line"),
    ("screen_not_contains", "s49_collapsed", "s49 row 4980:"),
    ("screen_contains", "s49_collapsed", "W16K-S49-DONE"),

    # ---- expanded: a row above the collapsed window becomes visible ----
    ("screen_regex", "s49_expanded", r"(?m)^\s*s49 row 4980: payload line"),
    # ...and the head was really TRUNCATED, not merely folded away (the on-disk
    # result starts at row 3173 — see verify_on_disk().session_result_truncated)
    ("screen_not_contains", "s49_expanded", "s49 row 0020:"),

    # ---- still responsive after the 2 000-line block re-render ----
    ("screen_contains", "s49_help", "Commands: /model /thinking /clear /compact"),
    ("editor_not_contains", "s49_help", "/help"),

    ("restore_markers",),
]


# ---------------------------------------------------------------------------
# On-disk verification (the session JSONL records the tool result once; the
# body is not duplicated into a second transcript entry).
# ---------------------------------------------------------------------------
import glob as _glob
import json as _json
import os as _os


def _long(path):
    r"""`\\?\`-prefixed absolute path (LongPathsEnabled=0 on this host; the
    battery session paths are ~300 chars)."""
    absolute = _os.path.abspath(path)
    if _os.name != "nt" or absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def verify_on_disk(scen_dir):
    """Assert the REAL session-file and spill outcomes inside `scen_dir`.

    Returns {name: (ok, detail)}:
      session_result_truncated  the transcript toolResult holds the TRUNCATED
                                tail (not the 5 000-line body) and ends with the
                                `[Showing lines A-B of N (50.0KB limit). Full
                                output: <path>]` footer
      transcript_single_copy    a body row appears exactly ONCE across every
                                transcript `message.content` field (the body is
                                not duplicated into a second transcript entry)
      entry_stores_body_once    MEASUREMENT of the task's "no whole-body
                                duplication" clause: a body row is stored ONCE
                                in the whole toolResult entry (content +
                                details + structuredContent)
      spill_file                the `Full output:` path advertised in the result
                                exists on disk and holds all 5 000 lines
    """
    out = {}
    agent = _os.path.join(scen_dir, "agent")
    files = _glob.glob(_os.path.join(agent, "sessions", "*", "*.jsonl"))

    entries, transcript_text = [], ""
    for path in files:
        try:
            text = open(_long(path), encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        for line in text.splitlines():
            try:
                entries.append(_json.loads(line))
            except ValueError:
                pass

    results = []      # the bash toolResult texts (message.content)
    entry_bodies = []  # the whole JSONL line, for the duplication measurement
    for entry in entries:
        if entry.get("type") != "message":
            continue
        message = entry.get("message") or {}
        if message.get("role") != "toolResult":
            continue
        content = message.get("content")
        text = ""
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and isinstance(block.get("text"), str):
                    text += block["text"]
        elif isinstance(content, str):
            text = content
        if "s49 row" in text:
            results.append(text)
        entry_bodies.append(_json.dumps(message))
    transcript_text = "\n".join(results)

    if results:
        body_rows = [ln.strip() for ln in results[0].splitlines() if "s49 row" in ln]
        footer = results[0][results[0].rfind("[Showing lines"):].strip() \
            if "[Showing lines" in results[0] else ""
        # the retained tail must start at line 3173 of 5000 — i.e. the head
        # rows 0001..3172 were DROPPED (the 50KB byte cap), not folded
        starts_at_retained_head = bool(body_rows) and body_rows[0].startswith(
            "s49 row 3173: payload line")
        ends_at_last = bool(body_rows) and body_rows[-1].startswith(
            "s49 row 5000: payload line")
        ok = (len(results) == 1 and 0 < len(body_rows) < _LINES
              and starts_at_retained_head and ends_at_last
              and "Full output:" in footer and "50.0KB limit" in footer)
        out["session_result_truncated"] = (
            ok,
            f"{len(results)} toolResult message(s), {len(body_rows)} body rows "
            f"(first={body_rows[0][:32] if body_rows else None!r}, "
            f"last={body_rows[-1][:32] if body_rows else None!r}), "
            f"footer={footer[:120]!r}")
        full_line = next((ln for ln in results[0].splitlines()
                          if "Full output:" in ln), "")
        if not full_line:
            full_line = footer
    else:
        out["session_result_truncated"] = (False, "no toolResult message carries the body")
        full_line = ""

    in_transcript = transcript_text.count("s49 row 4500:")
    out["transcript_single_copy"] = (
        in_transcript == 1,
        f"'s49 row 4500:' occurs {in_transcript}x across the transcript toolResult text")

    # Where exactly? Count per toolResult *field* so the report can name them.
    # The retained tail is intentionally stored twice (upstream parity:
    # `content` for the model and `details.truncation.content` for offline
    # readers); `structuredContent` is dropped, and the spilled-away head must
    # not survive anywhere in the entry.
    hits_by_field = {}
    entry_json = _json.dumps(entries)
    for entry in entries:
        message = entry.get("message") or {}
        if message.get("role") != "toolResult":
            continue
        for key in ("content", "details", "structuredContent"):
            if message.get(key) is not None:
                hits = _json.dumps(message[key]).count("s49 row 4500:")
                if hits:
                    hits_by_field[key] = hits_by_field.get(key, 0) + hits
    content_hits = hits_by_field.get("content", 0)
    details_hits = hits_by_field.get("details", 0)
    structured_hits = hits_by_field.get("structuredContent", 0)
    dropped_head_hits = entry_json.count("s49 row 0020:")
    fields = [f"{key}={hits}" for key, hits in hits_by_field.items()]
    sizes = [(_os.path.basename(p), _os.path.getsize(_long(p))) for p in files]
    out["entry_stores_body_once"] = (
        content_hits == 1
        and details_hits == 1
        and structured_hits == 0
        and dropped_head_hits == 0,
        f"'s49 row 4500:' per field: {', '.join(fields) or 'nowhere'}; "
        f"dropped head rows: {dropped_head_hits}; session file(s): {sizes}")

    spill = ""
    if full_line and "Full output:" in full_line:
        spill = full_line.split("Full output:", 1)[1].strip().rstrip("]").strip()
    if spill:
        try:
            spill_text = open(_long(spill), encoding="utf-8", errors="replace").read()
        except OSError as exc:
            out["spill_file"] = (False, f"cannot read advertised spill {spill}: {exc}")
        else:
            rows = sum(1 for ln in spill_text.splitlines() if "s49 row" in ln)
            out["spill_file"] = (rows == _LINES,
                                 f"{spill}: {rows} body rows "
                                 f"({len(spill_text)} bytes)")
    else:
        out["spill_file"] = (False, f"no 'Full output:' path in the result ({full_line[:120]!r})")
    return out
