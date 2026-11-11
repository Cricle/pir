# -*- coding: utf-8 -*-
# Round 36 lane-tui Task B: multiline/CJK paste + live light<->dark theme
# switching must not disturb session transcript, editor content or produce
# escape-sequence garbage. Real TUI, mock LLM supplies the two replies.
NAME = "s53_paste_theme_state"
TIMEOUT = 180
MOCK_SCRIPT = [
    {"text": "THEME-REPLY-MARKER"},
]

PASTE = "第一行 PASTE-A\n第二行\tPASTE-B\n中文注释 PASTE-C"

OPS = [
    ("wait_quiet", 5.0),
    ("log", "baseline turn"),
    ("send", b"THEME-BASE-MARKER\r"),
    ("wait", 8.0),
    ("wait_quiet", 3.0),
    ("snap", "s53_base"),
    ("log", "open /settings, switch dark -> light"),
    ("send", b"/settings\r"),
    ("wait", 1.5),
    ("snap", "s53_settings"),
    ("send", b"theme"),
    ("wait", 1.2),
    ("snap", "s53_search"),
    ("send", b"\x1b[C"),
    ("wait", 1.5),
    ("snap", "s53_light"),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "s53_light_closed"),
    ("log", "multiline CJK/tab paste into the editor (light theme)"),
    ("send", ("\x1b[200~" + PASTE + "\x1b[201~").encode("utf-8")),
    ("wait", 1.5),
    ("snap", "s53_pasted_light"),
    ("log", "submit the pasted prompt"),
    ("send", b"\r"),
    ("wait", 8.0),
    ("wait_quiet", 3.0),
    ("snap", "s53_submitted_light"),
    ("log", "switch light -> dark after the paste turn"),
    ("send", b"/settings\r"),
    ("wait", 1.5),
    ("send", b"theme"),
    ("wait", 1.2),
    ("send", b"\x1b[D"),
    ("wait", 1.5),
    ("snap", "s53_dark"),
    ("send", b"\x1b"),
    ("wait", 0.8),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "s53_dark_closed"),
]

CHECKS = [
    ("screen_contains", "s53_base", "THEME-BASE-MARKER"),
    ("screen_contains", "s53_base", "THEME-REPLY-MARKER"),
    ("screen_contains", "s53_light", "Theme: light"),
    ("log_count", "s53_base", "\x1b[38;5;97m", 0),
    ("log_count", "s53_light", "\x1b[38;5;97m", ">=1"),
    # transcript survives the theme change
    ("screen_contains", "s53_light_closed", "THEME-BASE-MARKER"),
    ("screen_contains", "s53_light_closed", "THEME-REPLY-MARKER"),
    # pasted rich text is intact in the editor, per line, with no bracketed
    # paste markers leaking onto the screen
    ("editor_contains", "s53_pasted_light", "第一行"),
    ("editor_contains", "s53_pasted_light", "PASTE-A"),
    ("editor_contains", "s53_pasted_light", "第二行"),
    ("editor_contains", "s53_pasted_light", "PASTE-B"),
    ("editor_contains", "s53_pasted_light", "中文注释"),
    ("editor_contains", "s53_pasted_light", "PASTE-C"),
    ("screen_not_contains", "s53_pasted_light", "[200~"),
    ("screen_not_contains", "s53_pasted_light", "[201~"),
    # submitted prompt and both turns stay in the transcript
    ("screen_contains", "s53_submitted_light", "PASTE-A"),
    ("screen_contains", "s53_submitted_light", "PASTE-B"),
    ("screen_contains", "s53_submitted_light", "PASTE-C"),
    ("screen_contains", "s53_submitted_light", "THEME-REPLY-MARKER"),
    # switch back to dark keeps the whole pasted/session content
    ("screen_contains", "s53_dark", "Theme: dark"),
    ("screen_contains", "s53_dark_closed", "PASTE-A"),
    ("screen_contains", "s53_dark_closed", "PASTE-B"),
    ("screen_contains", "s53_dark_closed", "PASTE-C"),
    ("screen_contains", "s53_dark_closed", "THEME-REPLY-MARKER"),
    ("screen_not_contains", "s53_dark_closed", "\ufffd"),
    ("screen_not_contains", "s53_dark_closed", "ï¿½"),
    ("restore_markers",),
]

# Disk verification: the request that reached the session must be the exact
# multiline paste (no escape markers, no mojibake) and must be preserved in
# the session JSONL across the later theme switch (same session file).
import json as _json
import os as _os


def VERIFY(scen_dir):
    checks = {}
    sessions = []
    for dirpath, _dirs, names in _os.walk(scen_dir):
        for entry in names:
            if entry.endswith(".jsonl"):
                sessions.append(_os.path.join(dirpath, entry))
    found = None
    for path in sessions:
        try:
            long_path = path if len(path) < 240 else ("\\\\?\\" + _os.path.abspath(path))
            with open(long_path, encoding="utf-8") as f:
                for line in f:
                    obj = _json.loads(line)
                    msg = obj.get("message") or {}
                    if msg.get("role") != "user":
                        continue
                    content = msg.get("content")
                    if isinstance(content, list):
                        text = "".join(p.get("text", "") for p in content
                                       if isinstance(p, dict))
                    else:
                        text = str(content)
                    if "PASTE-A" in text:
                        found = text
                        break
        except (OSError, ValueError):
            pass
        if found is not None:
            break
    checks["session_has_multiline_paste"] = (bool(found and "PASTE-A" in found and "PASTE-B" in found and "PASTE-C" in found), "user message contains PASTE-A/B/C" if found else "no pasted user message found")
    checks["session_paste_no_markers"] = (bool(found and "[200~" not in found and "[201~" not in found), "no bracketed-paste markers in session text")
    checks["session_paste_no_mojibake"] = (bool(found and "\ufffd" not in found), "no U+FFFD replacement char")
    return checks
