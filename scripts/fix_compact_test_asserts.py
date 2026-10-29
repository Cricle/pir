"""Fix the six new compaction E2E assertions: pin the real composed summary.

The new tests drive compaction on a transcript whose cut point splits a turn, so
the compactor composes `summary` + the upstream split-turn continuation section
("**Turn Context (split turn):**"); the mock summarizer answers both calls with
the same text. The assertions expected the bare summary text, which only holds
for transcripts that do not split a turn (the older test at ~line 2670).
"""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "tests/suites/agent_session.rs"
TARGET_LINES = [2720, 2789, 2848, 2900, 3009, 3095]
OLD = '    assert_eq!(result.summary, "COMPACT SUMMARY");'

HELPER = '''/// The compactor composes the main summary with upstream's split-turn
/// continuation section when the cut point splits a turn (the transcript these
/// tests compact is mid-turn by construction); the mock summarizer answers both
/// calls with the same text, so the composed result is exactly this.
fn assert_split_turn_summary(summary: &str) {
    assert_eq!(
        summary,
        "COMPACT SUMMARY\\n\\n---\\n\\n**Turn Context (split turn):**\\n\\nCOMPACT SUMMARY",
        "compaction must compose the main summary + the split-turn context section, once"
    );
}

'''


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    if "assert_split_turn_summary" in text:
        print("already patched")
        return 0
    nl = "\r\n" if "\r\n" in text else "\n"
    newline = "    assert_split_turn_summary(&result.summary);"
    lines = text.split(nl)
    for lineno in TARGET_LINES:
        actual = lines[lineno - 1]
        if actual.strip() != OLD.strip():
            print(f"line {lineno} is not the expected assertion:\n{actual!r}", file=sys.stderr)
            return 1
        lines[lineno - 1] = newline
    # Insert the helper just above the mock-summarizer doc comment that names
    # `COMPACT SUMMARY` (the helper block the new tests share).
    anchor = None
    for i, line in enumerate(lines):
        if line.strip() == "/// `COMPACT SUMMARY`.":
            anchor = i
            break
    if anchor is None:
        print("mock helper anchor not found", file=sys.stderr)
        return 1
    helper_lines = HELPER.rstrip("\n").split("\n")
    lines[anchor:anchor] = helper_lines + [""]
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(nl.join(lines))
    print(f"patched {len(TARGET_LINES)} assertions + inserted the helper")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
