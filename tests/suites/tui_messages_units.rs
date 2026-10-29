//! Unit tests migrated out of `src/tui/messages.rs` inline `#[cfg(test)]`
//! module (`tool_display_tests`): the diff-line parser, the shell/read/limited
//! truncation warning wording, and the duplicated shell-footer stripper.
//!
//! These helpers have no other public surface to reach from `tests/`; the
//! source side exposes them as `#[doc(hidden)] pub` (see the notes in
//! `src/tui/messages.rs`) purely so the assertions below can run unchanged.

use pir::tui::ansi::strip_ansi;
use pir::tui::messages::{
    limited_result_warning, parse_diff_line, read_truncation_warning, render_diff_text,
    shell_truncation_warning, strip_shell_footer, LimitedResultKind,
};

// --- parse_diff_line (components/diff.ts:6-13) --------------------------

#[test]
fn parse_diff_line_reads_prefix_number_and_content() {
    let parsed = parse_diff_line(" 3 keep").expect("context");
    assert_eq!(parsed.prefix, ' ');
    assert_eq!(parsed.line_num, "3");
    assert_eq!(parsed.content, "keep");

    let parsed = parse_diff_line("+12 added").expect("added");
    assert_eq!(parsed.prefix, '+');
    assert_eq!(parsed.line_num, "12");
    assert_eq!(parsed.content, "added");

    let parsed = parse_diff_line("-7 gone").expect("removed");
    assert_eq!(parsed.prefix, '-');
    assert_eq!(parsed.line_num, "7");
    assert_eq!(parsed.content, "gone");
}

/// The ` ...` skip markers parse with an EMPTY line number (regex
/// backtracking: `(\s*\d*)` gives up and `\s` eats the space). Visible
/// output equals the verbatim line, so the tab in the content proves the
/// parsed path was taken (content goes through `replaceTabs`).
#[test]
fn parse_diff_line_skip_marker_parses_with_empty_line_number() {
    let parsed = parse_diff_line("  ...").expect("skip marker");
    assert_eq!(parsed.prefix, ' ');
    assert_eq!(parsed.line_num, "");
    assert_eq!(parsed.content, "...");

    // `     ...` (indent + marker) also backtracks to the empty number;
    // the re-render is visually identical to the verbatim fallback.
    let rendered = strip_ansi(&render_diff_text("     ..."));
    assert_eq!(rendered, "     ...");

    // The parsed (not verbatim) path: content tabs become 3 spaces.
    let rendered = strip_ansi(&render_diff_text("  x\ty"));
    assert_eq!(rendered, "  x   y");
}

#[test]
fn parse_diff_line_rejects_unprefixed_lines() {
    assert!(parse_diff_line("plain text").is_none());
    assert!(parse_diff_line("5 no prefix").is_none());
    assert!(parse_diff_line("").is_none());
}

// --- strip_shell_footer (renderers/bash.ts:52-60) -----------------------

fn shell_details(truncated: bool, path: &str) -> serde_json::Value {
    serde_json::json!({
        "truncation": { "truncated": truncated },
        "fullOutputPath": path,
    })
}

#[test]
fn strip_shell_footer_drops_duplicated_full_output_block() {
    let footer = "\n\n[Full output saved to /tmp/out.log. Truncated: showing 10 of 100 lines]";
    let output = format!("line one\nline two{footer}");
    let stripped = strip_shell_footer(&output, Some(&shell_details(true, "/tmp/out.log")));
    assert_eq!(stripped, "line one\nline two");
}

#[test]
fn strip_shell_footer_keeps_non_matching_or_non_truncated_footers() {
    let footer = "\n\n[Full output saved to /tmp/out.log. Truncated: showing 10 of 100 lines]";
    let output = format!("line one{footer}");
    // Footer that does not carry the recorded path stays.
    assert_eq!(
        strip_shell_footer(&output, Some(&shell_details(true, "/tmp/other.log"))),
        output
    );
    // Not truncated (final details absent) → nothing stripped.
    assert_eq!(
        strip_shell_footer(&output, Some(&shell_details(false, "/tmp/out.log"))),
        output
    );
    // No details at all → nothing stripped.
    assert_eq!(strip_shell_footer(&output, None), output);
}

// --- shell_truncation_warning (renderers/bash.ts:85-100) ----------------

#[test]
fn shell_truncation_warning_matches_upstream_wording() {
    let details = serde_json::json!({
        "fullOutputPath": "/tmp/out.log",
        "truncation": {
            "truncated": true,
            "truncatedBy": "lines",
            "outputLines": 10,
            "totalLines": 100,
        },
    });
    assert_eq!(
        strip_ansi(&shell_truncation_warning(Some(&details)).expect("warning")),
        "[Full output: /tmp/out.log. Truncated: showing 10 of 100 lines]"
    );

    let details = serde_json::json!({
        "truncation": {
            "truncated": true,
            "truncatedBy": "bytes",
            "outputLines": 10,
            "maxBytes": 1024,
        },
    });
    assert_eq!(
        strip_ansi(&shell_truncation_warning(Some(&details)).expect("warning")),
        "[Truncated: 10 lines shown (1.0KB limit)]"
    );

    // Path only, no truncation.
    let details = serde_json::json!({ "fullOutputPath": "/tmp/out.log" });
    assert_eq!(
        strip_ansi(&shell_truncation_warning(Some(&details)).expect("warning")),
        "[Full output: /tmp/out.log]"
    );

    // Nothing truncated, no path → no warning.
    assert!(shell_truncation_warning(None).is_none());
    assert!(shell_truncation_warning(Some(&serde_json::json!({}))).is_none());
}

// --- read_truncation_warning (renderers/read.ts:138-147) ----------------

#[test]
fn read_truncation_warning_matches_upstream_wording() {
    let lines = serde_json::json!({
        "truncation": {
            "truncated": true,
            "truncatedBy": "lines",
            "outputLines": 200,
            "totalLines": 5000,
            "maxLines": 2000,
        },
    });
    assert_eq!(
        strip_ansi(&read_truncation_warning(Some(&lines)).expect("warning")),
        "[Truncated: showing 200 of 5000 lines (2000 line limit)]"
    );

    let bytes = serde_json::json!({
        "truncation": {
            "truncated": true,
            "truncatedBy": "bytes",
            "outputLines": 12,
            "maxBytes": 1024,
        },
    });
    assert_eq!(
        strip_ansi(&read_truncation_warning(Some(&bytes)).expect("warning")),
        "[Truncated: 12 lines shown (1.0KB limit)]"
    );

    let first_line = serde_json::json!({
        "truncation": {
            "truncated": true,
            "firstLineExceedsLimit": true,
            "maxBytes": 1024,
        },
    });
    assert_eq!(
        strip_ansi(&read_truncation_warning(Some(&first_line)).expect("warning")),
        "[First line exceeds 1.0KB limit]"
    );

    assert!(read_truncation_warning(None).is_none());
    let not_truncated = serde_json::json!({
        "truncation": { "truncated": false }
    });
    assert!(read_truncation_warning(Some(&not_truncated)).is_none());
}

// --- limited_result_warning (grep/find/ls renderers) --------------------

#[test]
fn limited_result_warning_matches_upstream_wording() {
    let grep = serde_json::json!({ "matchLimitReached": 100 });
    assert_eq!(
        strip_ansi(&limited_result_warning(LimitedResultKind::Grep, Some(&grep)).expect("warning")),
        "[Truncated: 100 matches limit]"
    );

    let find = serde_json::json!({
        "resultLimitReached": 50,
        "truncation": { "truncated": true, "maxBytes": 1024 },
    });
    assert_eq!(
        strip_ansi(&limited_result_warning(LimitedResultKind::Find, Some(&find)).expect("warning")),
        "[Truncated: 50 results limit, 1.0KB limit]"
    );

    let ls = serde_json::json!({ "linesTruncated": true });
    assert_eq!(
        strip_ansi(&limited_result_warning(LimitedResultKind::Ls, Some(&ls)).expect("warning")),
        "[Truncated: some lines truncated]"
    );

    assert!(limited_result_warning(LimitedResultKind::Ls, None).is_none());
    assert!(
        limited_result_warning(LimitedResultKind::Grep, Some(&serde_json::json!({}))).is_none()
    );
}

// --- live-render memos (streaming + tool result bodies) ------------------
//
// The per-frame path re-renders every component of the transcript on each
// dirty frame. Two memos keep that linear-with-a-tiny-constant: the tool
// block's rendered result section (`messages.rs` `ResultRowsCache`) and the
// live thinking block (`StreamingThinking`). Both are keyed on the payload
// `Arc`s, so these tests pin the two failure modes a broken key would cause:
// a stale hit (old text left on screen) and a spurious miss that changes the
// rendered rows.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::core::Component;
use pir::tui::messages::{parse_skill_block, StreamingThinking, ToolExecutionComponent};
use pir::tui::theme::set_theme;

/// A settled shell block renders a collapsed tail preview; its memoized rows
/// must be byte-identical frame after frame and must follow a new result.
#[test]
fn tool_result_rows_are_memoized_and_follow_the_output() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let args = Arc::new(serde_json::json!({ "command": "printf" }));
    let expanded = Arc::new(AtomicBool::new(false));
    let block = ToolExecutionComponent::new("bash", args, Arc::clone(&expanded));
    let first: Arc<str> = (0..40)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n")
        .into();
    block.update_result(first, false, Some(1200));
    let once = block.render(60);
    let twice = block.render(60);
    assert_eq!(
        once, twice,
        "a clean block must re-render its rows identically"
    );
    let painted = strip_ansi(&once.join("\n"));
    assert!(
        painted.contains("line 39"),
        "collapsed preview keeps the tail: {painted}"
    );
    assert!(
        !painted.contains("line 0 "),
        "collapsed preview drops the head: {painted}"
    );

    // A new result arrives as a fresh `Arc`: the memo must miss and repaint.
    let second: Arc<str> = "fresh output marker".to_string().into();
    block.update_result(second, false, Some(1300));
    let third = block.render(60);
    let repainted = strip_ansi(&third.join("\n"));
    assert!(repainted.contains("fresh output marker"), "{repainted}");
    assert!(
        !repainted.contains("line 39"),
        "the stale body must be gone: {repainted}"
    );
}

/// The live thinking block follows the shared text buffer across deltas and
/// renders the hidden-thinking label (live-relabeled) when collapsed.
#[test]
fn streaming_thinking_lines_track_the_shared_text_and_label() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let label = Arc::new(Mutex::new(None));
    let block = StreamingThinking::new_shared(
        Arc::<str>::from("first thought"),
        1,
        false,
        Arc::clone(&label),
    );
    let once = block.render(40);
    let twice = block.render(40);
    assert_eq!(
        once, twice,
        "an unchanged thinking body re-renders identically"
    );
    assert!(strip_ansi(&once.join("\n")).contains("first thought"));

    // The per-delta update hands over a new shared buffer: the rows must
    // follow it (a stale cache hit would leave the old text on screen).
    block.set_text_shared(Arc::<str>::from("second thought"));
    let third = block.render(40);
    let painted = strip_ansi(&third.join("\n"));
    assert!(painted.contains("second thought"), "{painted}");
    assert!(!painted.contains("first thought"), "{painted}");
    assert_eq!(once.len(), third.len(), "same body, same row count");

    // `hideThinkingBlock`: one label line, re-labeled live through the shared
    // slot (the label is part of the cache key).
    let hidden =
        StreamingThinking::new_shared(Arc::<str>::from("body"), 1, true, Arc::clone(&label));
    let collapsed = hidden.render(40);
    assert_eq!(collapsed.len(), 1, "{collapsed:?}");
    assert!(strip_ansi(&collapsed[0]).contains("Thinking..."));
    *label
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("Reasoning...".to_string());
    let relabeled = hidden.render(40);
    assert!(
        strip_ansi(&relabeled[0]).contains("Reasoning..."),
        "{relabeled:?}"
    );
}

// --- transcript reset: the retained slots are released -------------------
//
// `/clear` (and every session switch: `/new`, `/resume`, fork, tree
// navigation) empties the transcript through `ChatContainer::set_entries(&[])`.
// Dropping the children frees the entry payloads and their per-entry render
// caches; the slot arrays behind the commit records and the child list are
// released in the same call, so a long session's per-entry bookkeeping does
// not live on for the rest of the process (the entries vector itself is
// released by `terminal.rs::reset_session_transcript`). A reset followed by
// the first entry of a new session must commit normally — the release must
// not leave the container unusable.

/// The transcript container releases its per-entry slot arrays on a reset and
/// commits the next session's entries normally.
#[test]
fn transcript_reset_releases_the_retained_slots() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let entries: Vec<ChatEntry> = (0..64)
        .map(|index| ChatEntry::User(format!("prompt {index}")))
        .collect();
    let mut chat = ChatContainer::new();
    chat.set_entries(&entries);
    let _ = chat.render(40);
    assert!(
        chat.committed_capacity() >= entries.len(),
        "the session's commit records must be live before the reset"
    );
    assert!(
        chat.children_capacity() >= entries.len(),
        "each user entry commits at least one child"
    );

    chat.set_entries(&[]);
    assert_eq!(
        chat.committed_capacity(),
        0,
        "the commit slots must be released on a transcript reset"
    );
    assert_eq!(
        chat.children_capacity(),
        0,
        "the child slots must be released on a transcript reset"
    );

    chat.set_entries(&[ChatEntry::User("first prompt of the new session".into())]);
    let rows = chat.render(40);
    assert!(
        rows.iter()
            .any(|row| strip_ansi(row).contains("first prompt")),
        "the container must commit again after the reset: {rows:?}"
    );
}

// --- `/skill:` invocation cards -------------------------------------------
//
// Upstream `parseSkillBlock` (agent-session.ts:177-186) recognizes a whole
// `<skill name="…" location="…">…</skill>` message with an optional
// `\n\n`-separated user message; the TUI renders it as a collapsible
// `[skill] name` card (components/skill-invocation-message.ts) and the user
// text as a SEPARATE user message. Ctrl+O (`app.tools.expand`) expands the
// card.

const SKILL_BLOCK: &str = "<skill name=\"lark-im\" location=\"/skills/lark-im.md\">\nReferences are relative to /skills.\n\nRead the group messages.\n</skill>";

#[test]
fn skill_blocks_parse_with_upstream_semantics() {
    let parsed = parse_skill_block(SKILL_BLOCK).expect("block parses");
    assert_eq!(parsed.name, "lark-im");
    assert_eq!(parsed.location, "/skills/lark-im.md");
    assert_eq!(
        parsed.content,
        "References are relative to /skills.\n\nRead the group messages."
    );
    assert_eq!(parsed.user_message, None);

    // The `\n\n`-separated user message is trimmed (upstream
    // `match[4]?.trim() || undefined`).
    let with_message = format!("{SKILL_BLOCK}\n\n  总结三天  ");
    let parsed = parse_skill_block(&with_message).expect("block + user message parses");
    assert_eq!(parsed.user_message.as_deref(), Some("总结三天"));

    // The block must span the WHOLE message (the upstream regex is
    // anchored), and one blank line alone after the block is not a user
    // message (`[\s\S]+` requires at least one non-blank-ish character run).
    assert!(parse_skill_block(&format!("prefix {SKILL_BLOCK}")).is_none());
    assert!(parse_skill_block(&format!("{SKILL_BLOCK}\n\n")).is_none());
    assert!(parse_skill_block(&format!("{SKILL_BLOCK}\nsuffix")).is_none());
    assert!(parse_skill_block("plain user message").is_none());
    // Upstream matches literal `\n` separators only, so CRLF text is a plain
    // user message rather than an (accidentally) parsed block.
    assert!(parse_skill_block(&SKILL_BLOCK.replace('\n', "\r\n")).is_none());
}

#[test]
fn skill_blocks_render_a_collapsed_card_and_the_user_message_separately() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let expanded = Arc::new(AtomicBool::new(false));
    let mut chat = ChatContainer::new();
    chat.set_tools_expanded(Arc::clone(&expanded));
    chat.set_entries(&[ChatEntry::User(format!("{SKILL_BLOCK}\n\n总结三天"))]);

    let collapsed = chat.render(60);
    let plain = strip_ansi(&collapsed.join("\n"));
    assert!(plain.contains("[skill] lark-im"), "{plain}");
    assert!(plain.contains("(ctrl+o to expand)"), "{plain}");
    assert!(
        !plain.contains("Read the group messages."),
        "the collapsed card hides the skill body: {plain}"
    );
    assert!(
        !plain.contains("<skill name="),
        "the raw markup must not leak into the transcript: {plain}"
    );
    assert!(
        plain.contains("总结三天"),
        "the user message after the block renders as its own message: {plain}"
    );

    // Ctrl+O expands the card (the shared `app.tools.expand` cell), revealing
    // the label + the skill body as markdown; the user message stays.
    expanded.store(true, std::sync::atomic::Ordering::SeqCst);
    let expanded_rows = chat.render(60);
    let plain = strip_ansi(&expanded_rows.join("\n"));
    assert!(
        plain.contains("Read the group messages."),
        "expanded card shows the body: {plain}"
    );
    assert!(
        !plain.contains("(ctrl+o to expand)"),
        "the expand hint is collapsed-only: {plain}"
    );
    assert!(plain.contains("lark-im"), "{plain}");
    assert!(plain.contains("总结三天"), "{plain}");
}
