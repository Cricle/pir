//! Unit tests migrated out of `src/tui/messages.rs` inline `#[cfg(test)]`
//! modules:
//!
//! - `tool_display_tests` → the diff-line parser, the shell/read/limited
//!   truncation warning wording, and the duplicated shell-footer stripper;
//! - `subagent_render_tests` → the `subagent` tool-result body: child
//!   transcript rows, parallel/chain call lines, per-step and total usage
//!   footers, and the collapsed visual-row cap.
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

/// One `!`/`!!` user-bash transcript entry, the shape the terminal's
/// `handle_bash` (pending) and its `ToolUpdate` handler (the settled
/// completion) build.
fn user_bash_entry(
    args: Arc<serde_json::Value>,
    output: &str,
    pending: bool,
    started_at: Option<std::time::Instant>,
) -> ChatEntry {
    ChatEntry::ToolExecution {
        call_id: None,
        tool_name: "bash".to_string(),
        args,
        output: Arc::from(output),
        is_error: false,
        duration_ms: None,
        pending,
        details: None,
        started_at,
        preview: None,
        images: Arc::new(Vec::new()),
    }
}

/// The terminal's live-snapshot step (its `ToolUpdate` handler with
/// `partial: true`): only the pending block's output moves — every other
/// field, including the tool-name/args/start-time allocations the in-place
/// patch gate matches on, stays untouched.
fn set_pending_output_in_place(entry: &mut ChatEntry, output: &str) {
    let ChatEntry::ToolExecution {
        output: block_output,
        pending,
        ..
    } = entry
    else {
        panic!("not a tool entry");
    };
    assert!(
        *pending,
        "a live snapshot only applies to the pending block"
    );
    *block_output = Arc::from(output);
}

/// PIR-TUI-034: the `!`/`!!` user-bash block must paint streamed output while
/// the command runs. Upstream (`BashExecutionComponent.appendOutput`) the
/// `onChunk` callback appends every chunk and the block repaints; pir's
/// worker forwards live tail snapshots as partial `ToolUpdate`s and the UI
/// refreshes the pending entry in place through p2's pending-safe
/// `set_output` seam. This drives that exact entry shape through the
/// transcript container: after the two snapshots the rendered block contains
/// chunk 1 AND chunk 2 while the Running… row stays (and the retained
/// component was patched, not rebuilt); the completion then replaces it with
/// the settled block.
#[test]
fn pending_user_bash_block_paints_each_live_snapshot() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let args: Arc<serde_json::Value> = Arc::new(serde_json::json!({
        "command": "for i in 1 2; do echo CHUNK-$i; sleep 1; done",
        "userBash": true,
        "excludeFromContext": false,
    }));
    let mut chat = ChatContainer::new();
    let mut entries = vec![user_bash_entry(
        Arc::clone(&args),
        "",
        true,
        Some(std::time::Instant::now()),
    )];
    chat.set_entries(&entries);
    let painted = strip_ansi(&chat.render(80).join("\n"));
    assert!(painted.contains("Running..."), "{painted}");
    assert!(
        !painted.contains("CHUNK-1"),
        "no output has arrived yet: {painted}"
    );

    // First live snapshot: the block stays pending and takes the chunk in
    // place — the retained component is PATCHED through `set_output`, not
    // rebuilt (the memo caches survive).
    set_pending_output_in_place(&mut entries[0], "CHUNK-1\n");
    chat.set_entries(&entries);
    assert_eq!(
        chat.last_diff().entries_patched,
        1,
        "a live snapshot must patch the retained pending block in place"
    );
    let painted = strip_ansi(&chat.render(80).join("\n"));
    assert!(painted.contains("Running..."), "{painted}");
    assert!(painted.contains("CHUNK-1"), "{painted}");

    // Second live snapshot: both chunks are visible, still running.
    set_pending_output_in_place(&mut entries[0], "CHUNK-1\nCHUNK-2\n");
    chat.set_entries(&entries);
    assert_eq!(chat.last_diff().entries_patched, 1);
    let painted = strip_ansi(&chat.render(80).join("\n"));
    assert!(painted.contains("Running..."), "{painted}");
    assert!(
        painted.contains("CHUNK-1") && painted.contains("CHUNK-2"),
        "the second snapshot must paint the whole accumulated tail: {painted}"
    );

    // Completion: the settled block drops the Running… row and keeps the
    // final output (the TUI replaces the entry with the completed one).
    let settled_args: Arc<serde_json::Value> = Arc::new(serde_json::json!({
        "command": "for i in 1 2; do echo CHUNK-$i; sleep 1; done",
        "userBash": true,
        "excludeFromContext": false,
        "exitCode": 0,
    }));
    entries[0] = user_bash_entry(settled_args, "CHUNK-1\nCHUNK-2\n", false, None);
    chat.set_entries(&entries);
    let painted = strip_ansi(&chat.render(80).join("\n"));
    assert!(!painted.contains("Running..."), "{painted}");
    assert!(
        painted.contains("CHUNK-1") && painted.contains("CHUNK-2"),
        "the settled block keeps the output: {painted}"
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

// ---------------------------------------------------------------------------
// src/tui/messages.rs — subagent_render_tests
// ---------------------------------------------------------------------------

#[test]
fn subagent_body_keeps_child_transcript_inside_the_tool_block() {
    let details = serde_json::json!({
        "mode": "single",
        "results": [{
            "agent": "reviewer",
            "task": "check parser",
            "exitCode": 0,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "look here"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "found the issue"},
                    {"type": "toolCall", "id": "t", "name": "read", "arguments": {"path": "a.rs"}}
                ]}
            ],
            "usage": {"input": 10, "output": 4, "turns": 1}
        }]
    });
    let body =
        pir::tui::messages::subagent_result_body(Some(&details), true, 120).expect("subagent body");
    assert!(body.contains("reviewer · done"));
    assert!(body.contains("user: look here"));
    assert!(body.contains("assistant: found the issue"));
    assert!(body.contains("→ read"));
}

#[test]
fn subagent_parallel_call_line_lists_tasks_scope_and_previews() {
    let args = serde_json::json!({
        "tasks": [
            {"agent": "alpha", "task": "Run the alpha smoke check."},
            {"agent": "beta", "task": "Run the beta smoke check."},
            {"agent": "gamma", "task": "This task description is deliberately longer than forty characters"},
            {"agent": "delta", "task": "fourth task"}
        ],
        "agentScope": "project"
    });
    let line = strip_ansi(&pir::tui::messages::format_tool_call_line(
        "subagent", &args, false,
    ));
    assert!(
        line.contains("subagent parallel (4 tasks) [project]"),
        "{line:?}"
    );
    assert!(
        line.contains("  alpha Run the alpha smoke check."),
        "{line:?}"
    );
    assert!(
        line.contains("  beta Run the beta smoke check."),
        "{line:?}"
    );
    assert!(
        line.contains("  gamma This task description is deliberately lo..."),
        "{line:?}"
    );
    assert!(
        !line.contains("delta"),
        "only the first three previews: {line:?}"
    );
    assert!(line.contains("... +1 more"), "{line:?}");
}

#[test]
fn subagent_chain_call_line_strips_previous_placeholder() {
    let args = serde_json::json!({
        "chain": [
            {"agent": "planner", "task": "Draft the plan"},
            {"agent": "builder", "task": "Implement {previous} carefully"}
        ]
    });
    let line = strip_ansi(&pir::tui::messages::format_tool_call_line(
        "subagent", &args, false,
    ));
    assert!(line.contains("subagent chain (2 steps) [user]"), "{line:?}");
    assert!(line.contains("  1. planner Draft the plan"), "{line:?}");
    assert!(
        line.contains("  2. builder Implement  carefully"),
        "{line:?}"
    );
    assert!(!line.contains("{previous}"), "{line:?}");
}

#[test]
fn subagent_parallel_result_shows_total_usage_after_settling() {
    let details = serde_json::json!({
        "mode": "parallel",
        "results": [
            {"agent": "alpha", "task": "a", "exitCode": 0,
             "usage": {"input": 1234, "output": 2, "cacheRead": 100, "cacheWrite": 0, "cost": 0.004, "turns": 1}},
            {"agent": "beta", "task": "b", "exitCode": 1,
             "usage": {"input": 1234, "output": 2, "cacheRead": 0, "cacheWrite": 5, "cost": 0.003464, "turns": 1}}
        ]
    });
    let body = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), true, 120)
            .expect("parallel body"),
    );
    assert!(
        body.contains("Total: 2 turns ↑2.5k ↓4 R100 W5 $0.0075"),
        "{body:?}"
    );
}

#[test]
fn subagent_parallel_result_omits_total_while_running() {
    let details = serde_json::json!({
        "mode": "parallel",
        "results": [{
            "agent": "alpha", "task": "a", "exitCode": -1, "pending": true,
            "usage": {"input": 10, "output": 0, "cost": 0.0, "turns": 1}
        }]
    });
    let body =
        pir::tui::messages::subagent_result_body(Some(&details), true, 120).expect("parallel body");
    assert!(!body.contains("Total:"), "{body:?}");
}

#[test]
fn subagent_chain_running_body_lists_step_entries() {
    let details = serde_json::json!({
        "mode": "chain",
        "results": [{
            "agent": "worker", "task": "step one", "step": 1,
            "exitCode": -1, "pending": true,
            "messages": [{"role": "assistant", "content": [
                {"type": "toolCall", "id": "t", "name": "bash", "arguments": {}}
            ]}],
            "usage": {"input": 10, "output": 2, "turns": 1}
        }, {
            "agent": "worker", "task": "step two", "step": 2,
            "exitCode": -1, "pending": true, "messages": []
        }]
    });
    let body = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120).expect("chain body"),
    );
    assert!(body.contains("─── Step 1: worker ⏳"), "{body:?}");
    assert!(body.contains("─── Step 2: worker ⏳"), "{body:?}");
    assert!(body.contains("(running...)"), "{body:?}");
    assert!(!body.contains("Total:"), "{body:?}");
}

#[test]
fn subagent_parallel_running_body_lists_each_task() {
    let details = serde_json::json!({
        "mode": "parallel",
        "results": [
            {"agent": "alpha", "task": "a", "exitCode": -1, "pending": true, "messages": []},
            {"agent": "beta", "task": "b", "exitCode": -1, "pending": true, "messages": []}
        ]
    });
    let body = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120)
            .expect("parallel body"),
    );
    assert!(body.contains("─── alpha ⏳"), "{body:?}");
    assert!(body.contains("─── beta ⏳"), "{body:?}");
    assert_eq!(body.matches("(running...)").count(), 2, "{body:?}");
}

#[test]
fn subagent_chain_settled_body_uses_upstream_step_headers() {
    let details = serde_json::json!({
        "mode": "chain",
        "results": [
            {"agent": "one", "task": "a", "step": 1, "exitCode": 0,
             "messages": [{"role": "assistant", "content": [{"type": "text", "text": "ok"}]}]},
            {"agent": "two", "task": "b", "step": 2, "exitCode": 1,
             "messages": [{"role": "assistant", "content": [{"type": "text", "text": "bad"}]}]}
        ]
    });
    let body = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120).expect("chain body"),
    );
    assert!(body.contains("─── Step 1: one ✓"), "{body:?}");
    assert!(body.contains("─── Step 2: two ✗"), "{body:?}");
    assert!(!body.contains("· done"), "{body:?}");
    assert!(!body.contains("· failed"), "{body:?}");
}

#[test]
fn subagent_parallel_settled_body_uses_upstream_task_headers() {
    let details = serde_json::json!({
        "mode": "parallel",
        "results": [
            {"agent": "alpha", "task": "a", "exitCode": 0,
             "messages": [{"role": "assistant", "content": [{"type": "text", "text": "ok"}]}]},
            {"agent": "beta", "task": "b", "exitCode": 1, "stopReason": "error",
             "errorMessage": "boom", "messages": []}
        ]
    });
    let body = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120)
            .expect("parallel body"),
    );
    assert!(body.contains("─── alpha ✓"), "{body:?}");
    assert!(body.contains("─── beta ✗"), "{body:?}");
    assert!(!body.contains("· done"), "{body:?}");
    assert!(!body.contains("· failed"), "{body:?}");
}

#[test]
fn subagent_expanded_chain_shows_per_step_usage_before_total() {
    let details = serde_json::json!({
        "mode": "chain",
        "results": [
            {"agent": "one", "task": "a", "step": 1, "exitCode": 0,
             "usage": {"input": 1234, "output": 10, "cost": 0.001, "turns": 1},
             "model": "mock-model"},
            {"agent": "two", "task": "b", "step": 2, "exitCode": 0,
             "usage": {"input": 2000, "output": 20, "cost": 0.002, "turns": 2},
             "model": "mock-model"}
        ]
    });
    let expanded = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), true, 120).expect("chain body"),
    );
    let step_lines: Vec<&str> = expanded
        .lines()
        .filter(|line| line.trim_start().starts_with(char::is_numeric))
        .collect();
    assert_eq!(step_lines.len(), 2, "{expanded:?}");
    assert!(
        step_lines[0].contains("1 turn ↑1.2k ↓10 $0.0010 mock-model"),
        "{expanded:?}"
    );
    assert!(
        step_lines[1].contains("2 turns ↑2.0k ↓20 $0.0020 mock-model"),
        "{expanded:?}"
    );
    assert!(
        expanded.contains("Total: 3 turns ↑3.2k ↓30 $0.0030"),
        "{expanded:?}"
    );

    let collapsed = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120).expect("chain body"),
    );
    assert_eq!(
        collapsed
            .lines()
            .filter(|line| line.trim_start().starts_with(char::is_numeric))
            .count(),
        0,
        "per-step usage is expanded-only: {collapsed:?}"
    );
    assert!(collapsed.contains("Total: 3 turns"), "{collapsed:?}");
}

#[test]
fn subagent_single_settled_result_shows_usage_in_collapsed_and_expanded() {
    let details = serde_json::json!({
        "mode": "single",
        "results": [{
            "agent": "j13-worker",
            "task": "Run the fork-history smoke check.",
            "exitCode": 0,
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "J13-CHILD-REPORT"}
            ]}],
            "usage": {"input": 2468, "output": 4, "cacheRead": 0, "cacheWrite": 0,
                      "cost": 0.0012, "turns": 2, "contextTokens": 8000},
            "model": "mock-model"
        }]
    });
    for expanded in [false, true] {
        let body = strip_ansi(
            &pir::tui::messages::subagent_result_body(Some(&details), expanded, 120)
                .expect("single body"),
        );
        assert!(
            body.contains("2 turns ↑2.5k ↓4 $0.0012 ctx:8.0k mock-model"),
            "expanded={expanded}: {body:?}"
        );
        assert!(
            !body.contains("Total:"),
            "single mode has no aggregate footer: {body:?}"
        );
    }
}

#[test]
fn subagent_single_running_result_omits_usage() {
    let details = serde_json::json!({
        "mode": "single",
        "results": [{
            "agent": "worker", "task": "still working",
            "exitCode": -1, "pending": true,
            "messages": [],
            "usage": {"input": 10, "output": 2, "turns": 1},
            "model": "mock-model"
        }]
    });
    let body =
        pir::tui::messages::subagent_result_body(Some(&details), true, 120).expect("single body");
    assert!(!body.contains("turn"), "{body:?}");
    assert!(!body.contains("↑"), "{body:?}");
    assert!(body.contains("(running...)"), "{body:?}");
}

#[test]
fn subagent_single_usage_survives_fork_replay_from_jsonl() {
    // `/fork` replays the toolResult `details` verbatim from the session
    // JSONL; the same settled single result must keep its usage line.
    let jsonl = r#"{"mode":"single","results":[{"agent":"j13-worker","task":"t","exitCode":0,"messages":[{"role":"assistant","content":[{"type":"text","text":"J13-CHILD-REPORT"}]}],"usage":{"input":2468,"output":4,"turns":2},"model":"mock-model"}]}"#;
    let details: serde_json::Value = serde_json::from_str(jsonl).expect("details");
    let body = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120).expect("single body"),
    );
    assert!(body.contains("2 turns ↑2.5k ↓4 mock-model"), "{body:?}");
}

#[test]
fn subagent_result_skips_tool_result_and_system_messages() {
    let long = "P15A row 0001: ".to_owned() + &"x".repeat(51_000);
    let details = serde_json::json!({
        "mode": "parallel",
        "results": [{
            "agent": "p15-alpha",
            "task": "Run the alpha long-output check.",
            "exitCode": 0,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "run it"}]},
                {"role": "system", "content": [{"type": "text", "text": "SYSTEM-PROMPT-SECRET"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "working"},
                    {"type": "toolCall", "id": "t", "name": "bash", "arguments": {}}
                ]},
                {"role": "toolResult", "toolCallId": "t", "toolName": "bash",
                 "content": [{"type": "text", "text": long}]},
                {"role": "assistant", "content": [{"type": "text", "text": "P15-ALPHA-REPORT"}]}
            ],
            "usage": {"input": 10, "output": 4, "turns": 2},
            "model": "mock-model"
        }]
    });
    for expanded in [false, true] {
        let body = strip_ansi(
            &pir::tui::messages::subagent_result_body(Some(&details), expanded, 120)
                .expect("parallel body"),
        );
        assert!(
            body.contains("─── p15-alpha ✓"),
            "expanded={expanded}: {body:?}"
        );
        assert!(body.contains("user: run it"), "{body:?}");
        assert!(body.contains("assistant: working"), "{body:?}");
        assert!(body.contains("→ bash"), "{body:?}");
        assert!(body.contains("P15-ALPHA-REPORT"), "{body:?}");
        assert!(
            !body.contains("toolResult"),
            "child tool result body must not render: {body:?}"
        );
        assert!(!body.contains("SYSTEM-PROMPT-SECRET"), "{body:?}");
        assert!(!body.contains("P15A row 0001"), "{body:?}");
    }
}

#[test]
fn subagent_collapsed_cap_counts_visual_rows_not_source_newlines() {
    let mut text = String::new();
    for i in 1..=40 {
        text.push_str(&format!("line {i:02}\n"));
    }
    // One over-wide source line: at width 120 it occupies ~43 display
    // rows on its own, so the collapsed cap must count the wrapped rows.
    let long_line = format!("LONG-A {} LONG-B", "y".repeat(5000));
    let details = serde_json::json!({
        "mode": "single",
        "results": [{
            "agent": "worker",
            "task": "long output",
            "exitCode": 0,
            "messages": [
                {"role": "assistant", "content": [{"type": "text", "text": text}]},
                {"role": "assistant", "content": [{"type": "text", "text": long_line}]}
            ],
            "usage": {"input": 10, "output": 4, "turns": 1},
            "model": "mock-model"
        }]
    });
    let collapsed = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), false, 120).expect("single body"),
    );
    assert!(
        collapsed.contains("more lines, ctrl+o to expand"),
        "collapsed preview must announce the cap: {collapsed:?}"
    );
    assert!(
        collapsed.lines().count() <= 33,
        "32 visual rows plus one hint line: {collapsed:?}"
    );
    assert!(!collapsed.contains("line 40"), "{collapsed:?}");
    assert!(
        !collapsed.contains("LONG-B"),
        "wrapped tail is capped: {collapsed:?}"
    );

    let expanded = strip_ansi(
        &pir::tui::messages::subagent_result_body(Some(&details), true, 120).expect("single body"),
    );
    assert!(expanded.contains("line 40"), "{expanded:?}");
    assert!(expanded.contains("LONG-B"), "{expanded:?}");
    assert!(!expanded.contains("ctrl+o to expand"), "{expanded:?}");
}
