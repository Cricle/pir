//! Regression pins for the TUI cache/queue lanes (wave 6f):
//!
//! 1. `Raw` renderer provenance: cached extension-tool output re-renders when a
//!    render input (width / theme generation / Ctrl+O expansion) moves, and is
//!    served as-is when none did. The provenance is also part of the retained
//!    child tag, so the `ChatContainer` diff rebuilds the block instead of
//!    keeping stale lines.
//! 2. `AssistantDone` finalization: every live `Streaming` segment of a turn is
//!    finalized exactly once (one `Assistant`, no `live` survivors), including
//!    the interleaved-tool-card shape and the failed-run path.
//! 3. Replacement caps: both the byte cap and the event cap are enforced
//!    BEFORE the payload is decoded, and a violation consumes the input
//!    without queueing anything (order preserved).
//! 4. `bound_pending_input`: the loop's decoded-input queue is capped.
//! 5. Native dialog TTL: a NATIVE dialog request id must not inherit the JS
//!    runtime's global 600 s queued-dialog deadline.
//!
//! Everything drives pure seams — no terminal, no TUI process, no clipboard.

use std::sync::Arc;

use pir::tui::app::{ChatContainer, ChatEntry, RawCustomRender, RawRender, RawToolRender};
use pir::tui::core::Component;
use pir::tui::terminal::{finalize_streaming_segments, PENDING_INPUT_MAX_EVENTS};
use pir::tui::terminal_input::{
    replacement_cap_violation, ReplacementCap, TERMINAL_INPUT_REPLACEMENT_MAX_BYTES,
    TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS,
};

use pir::ai::types::{AssistantMessage, AssistantRole, ContentBlock, StopReason, TextContent};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "tui-cache-lane".into(),
        provider: "tui-cache-lane".into(),
        model: "tui-cache-lane".into(),
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

fn streaming(live: bool, text: &str) -> ChatEntry {
    ChatEntry::Streaming {
        live,
        thinking: String::new(),
        text: text.to_string(),
    }
}

fn tool_provenance(width: usize, theme_generation: u64, expanded: bool) -> RawRender {
    RawRender::Tool(RawToolRender {
        call_id: Some("call-1".into()),
        tool_name: "read".into(),
        args: Arc::new(serde_json::json!({ "path": "src/lib.rs" })),
        output: Arc::from("body"),
        is_error: false,
        details: None,
        expanded,
        width,
        theme_generation,
        shell: None,
        pending: false,
        images: Arc::new(Vec::new()),
        image_lines: Arc::new(Vec::new()),
    })
}

fn custom_provenance(width: usize, theme_generation: u64, expanded: bool) -> RawRender {
    RawRender::Custom(RawCustomRender {
        payload: Arc::new(serde_json::json!({ "customType": "todo", "value": 1 })),
        custom_type: "todo".into(),
        is_message: false,
        expanded,
        width,
        theme_generation,
    })
}

// ---------------------------------------------------------------------------
// 1. Raw renderer provenance: answers() and the retained-child tag
// ---------------------------------------------------------------------------

#[test]
fn tool_provenance_answers_only_the_inputs_it_was_rendered_with() {
    let render = tool_provenance(80, 7, false);
    assert!(render.answers(80, 7, false), "identical inputs must hit");
    assert!(!render.answers(100, 7, false), "a width move must miss");
    assert!(!render.answers(80, 8, false), "a palette move must miss");
    assert!(!render.answers(80, 7, true), "an expansion move must miss");
}

#[test]
fn custom_provenance_answers_only_the_inputs_it_was_rendered_with() {
    let render = custom_provenance(80, 7, false);
    assert!(render.answers(80, 7, false));
    assert!(!render.answers(79, 7, false));
    assert!(!render.answers(80, 6, false));
    assert!(!render.answers(80, 7, true));
}

/// A width move must rebuild the block in the `ChatContainer` diff: the
/// provenance is in `EntryTag`, so the retained prefix stops at the stale
/// entry and its children are re-committed.
#[test]
fn raw_children_are_rebuilt_when_the_render_key_moves() {
    let mut chat = ChatContainer::new();
    let stale = vec![ChatEntry::Raw {
        call_id: Some("call-1".into()),
        lines: vec!["stale row".into()],
        render: Some(tool_provenance(80, 7, false)),
    }];
    chat.set_entries(&stale);
    let before = chat.render(80);
    assert!(
        before.iter().any(|row| row.contains("stale row")),
        "the initial frame must show the cached rows"
    );

    // Same provenance: the memoized children are retained (the bytes are the
    // same object, so a hit is observable as an unchanged render).
    chat.set_entries(&stale);
    assert_eq!(chat.render(80), before);

    // The provenance moved (a resize re-rendered the block): the tag differs,
    // so the children are rebuilt and the NEW lines are what renders.
    let fresh = vec![ChatEntry::Raw {
        call_id: Some("call-1".into()),
        lines: vec!["fresh row".into()],
        render: Some(tool_provenance(100, 7, false)),
    }];
    chat.set_entries(&fresh);
    let after = chat.render(100);
    assert!(
        after.iter().any(|row| row.contains("fresh row")),
        "the rebuilt block's rows must replace the stale ones"
    );
    assert!(
        !after.iter().any(|row| row.contains("stale row")),
        "no stale renderer output may survive the rebuild"
    );
}

/// Verbatim `Raw` blocks (image previews, the custom-entry JSON fallback) carry
/// no provenance and must keep the plain buffer/len identity.
#[test]
fn verbatim_raw_blocks_keep_the_plain_identity_tag() {
    let mut chat = ChatContainer::new();
    let entries = vec![ChatEntry::Raw {
        call_id: None,
        lines: vec!["verbatim".into()],
        render: None,
    }];
    chat.set_entries(&entries);
    let rows = chat.render(40);
    assert!(rows.iter().any(|row| row.contains("verbatim")));
    // Re-committing the identical entry is a hit (no provenance to compare).
    chat.set_entries(&entries);
    assert_eq!(chat.render(40), rows);
}

// ---------------------------------------------------------------------------
// 2. AssistantDone finalization: every Streaming segment settles exactly once
// ---------------------------------------------------------------------------

#[test]
fn assistant_done_replaces_the_first_live_segment_and_drops_later_ones() {
    let mut entries = vec![
        streaming(true, "alpha"),
        ChatEntry::Notice {
            kind: pir::tui::app::NoticeKind::Status,
            text: "a tool card between segments".into(),
        },
        streaming(true, "beta"),
    ];
    let finalized =
        finalize_streaming_segments(&mut entries, Some(assistant_message("alpha beta")));
    assert_eq!(finalized, 2, "both live segments must be finalized");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(entry, ChatEntry::Assistant(_)))
            .count(),
        1,
        "exactly one authoritative assistant message may remain"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, ChatEntry::Streaming { live: true, .. })),
        "no live segment may survive the turn"
    );
    // The tool card that sat between the segments stays where it was.
    assert!(entries.iter().any(|entry| matches!(
        entry,
        ChatEntry::Notice { text, .. } if text.contains("tool card")
    )));
    // The FIRST live slot is the one the message replaced.
    assert!(matches!(entries.first(), Some(ChatEntry::Assistant(_))));
}

#[test]
fn assistant_done_with_no_deltas_appends_the_message() {
    let mut entries = vec![ChatEntry::Notice {
        kind: pir::tui::app::NoticeKind::Status,
        text: "tool only".into(),
    }];
    let finalized = finalize_streaming_segments(&mut entries, Some(assistant_message("tool only")));
    assert_eq!(finalized, 0);
    assert_eq!(entries.len(), 2);
    assert!(matches!(entries.last(), Some(ChatEntry::Assistant(_))));
}

#[test]
fn failed_run_settles_every_live_segment_without_dropping_text() {
    let mut entries = vec![
        streaming(true, "partial one"),
        streaming(true, "partial two"),
    ];
    let finalized = finalize_streaming_segments(&mut entries, None);
    assert_eq!(finalized, 2);
    assert_eq!(entries.len(), 2, "the streamed text must stay on screen");
    for entry in &entries {
        match entry {
            ChatEntry::Streaming { live, text, .. } => {
                assert!(!live, "a failed run must clear the live flag");
                assert!(text.starts_with("partial"), "the text is preserved");
            }
            other => panic!("unexpected entry {other:?}"),
        }
    }
}

#[test]
fn finalization_is_idempotent() {
    let mut entries = vec![streaming(true, "once")];
    assert_eq!(
        finalize_streaming_segments(&mut entries, Some(assistant_message("once"))),
        1
    );
    // A second settle (the loop's Idle arm after AssistantDone) finds nothing.
    assert_eq!(finalize_streaming_segments(&mut entries, None), 0);
    assert_eq!(entries.len(), 1);
}

// ---------------------------------------------------------------------------
// 3. Replacement caps: checked before decode, over-cap consumed
// ---------------------------------------------------------------------------

#[test]
fn byte_cap_is_checked_before_the_decode() {
    // Boundary contract (audited): BOTH caps are INCLUSIVE maxima — a payload
    // of exactly `TERMINAL_INPUT_REPLACEMENT_MAX_BYTES` bytes, or exactly
    // `..._MAX_EVENTS` chars, is admitted; only `>` is a violation. That is
    // the rule every other bound in this module uses (the char precheck, the
    // post-decode event check, `bound_pending_input`), the rule the constant
    // docs state ("Upper bound on one handler `Replace` payload"), and the
    // rule the diagnostics spell ("exceeds the ... cap"). The earlier
    // expectation of `None` here was wrong for a subtler reason than the
    // operator: the fixture was ASCII, so it also tripped the 256-event cap.

    // OVER the byte cap: the byte check runs FIRST, so an oversized echo is
    // classified by its direct bound without ever scanning chars (this ASCII
    // payload would also trip the event cap).
    let over = "a".repeat(TERMINAL_INPUT_REPLACEMENT_MAX_BYTES + 1);
    assert_eq!(
        replacement_cap_violation(&over),
        Some(ReplacementCap::Bytes(over.len()))
    );

    // AT the byte cap: the byte check admits the payload; the event precheck
    // is what rejects it. That is not a fixture accident: UTF-8 encodes at
    // most 4 bytes per char, so 4096 bytes is at least 1024 chars — four
    // times the 256-event bound. No payload can be at the byte cap AND under
    // the event cap, so the byte cap's jobs are precedence and bounding the
    // decode input, not admitting a case the event cap would reject. The
    // precheck's scan stops one past its bound, hence the reported 257.
    let at_cap = "a".repeat(TERMINAL_INPUT_REPLACEMENT_MAX_BYTES);
    assert_eq!(
        replacement_cap_violation(&at_cap),
        Some(ReplacementCap::Events(
            TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS + 1
        )),
        "the byte cap is inclusive; the tighter event cap is what fires"
    );

    // The widest payload BOTH caps admit: 256 four-byte chars = 1 KiB, which
    // also proves the event precheck counts CHARS, not bytes.
    let widest = "\u{1F600}".repeat(TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS);
    assert_eq!(
        widest.len(),
        TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS * 4,
        "U+1F600 is a 4-byte char"
    );
    assert!(widest.len() < TERMINAL_INPUT_REPLACEMENT_MAX_BYTES);
    assert_eq!(replacement_cap_violation(&widest), None);
}

#[test]
fn event_cap_is_checked_before_the_decode() {
    // One byte per char, so the event cap is reachable well under the byte
    // cap — exactly the gap a byte-only check leaves open.
    let over = "a".repeat(TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS + 1);
    assert!(over.len() < TERMINAL_INPUT_REPLACEMENT_MAX_BYTES);
    assert_eq!(
        replacement_cap_violation(&over),
        Some(ReplacementCap::Events(
            TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS + 1
        ))
    );
    let at_cap = "a".repeat(TERMINAL_INPUT_REPLACEMENT_MAX_EVENTS);
    assert_eq!(replacement_cap_violation(&at_cap), None);
}

#[test]
fn a_multibyte_escape_sequence_stays_within_the_event_cap() {
    // A complete CSI key is ONE event built from many chars: the char-count
    // precheck must not reject a plausible sequence under the cap.
    assert_eq!(replacement_cap_violation("\x1b[1;2A"), None);
    assert_eq!(replacement_cap_violation(""), None);
}

#[test]
fn pending_input_cap_is_bounded() {
    // The cap constant the loop enforces; pinned so a future edit cannot
    // silently make the queue unbounded.
    assert_eq!(PENDING_INPUT_MAX_EVENTS, 8 * 1024);
}

// ---------------------------------------------------------------------------
// 4. Dialog TTL: native requests do not inherit the JS global deadline
// ---------------------------------------------------------------------------

/// The JS runtime's global dialog TTL (`extensions::runtime::DIALOG_TIMEOUT`).
/// It is crate-private, so the regression pins the public behaviour against
/// the documented value instead of importing it.
const JS_DIALOG_TTL: std::time::Duration = std::time::Duration::from_secs(600);

#[test]
fn native_dialog_requests_do_not_inherit_the_js_dialog_ttl() {
    use pir::plugins::host::NATIVE_DIALOG_ID_BASE;
    use pir::tui::dialogs::DialogOverlay;
    use std::time::Instant;

    // JS request (id below the native base): the runtime's global TTL bounds
    // its queue life.
    let js = DialogOverlay::from_request(1, "input", &serde_json::json!({ "title": "js" }));
    let now = Instant::now();
    assert!(
        js.queue_expired(now + JS_DIALOG_TTL + std::time::Duration::from_secs(1)),
        "a JS request past the global dialog TTL must be swept from the queue"
    );

    // NATIVE request: its waiter is NativeDialogWait, bounded only by the
    // request-level timeout — it must NOT be swept by the JS TTL.
    let native = DialogOverlay::from_request(
        NATIVE_DIALOG_ID_BASE + 1,
        "input",
        &serde_json::json!({ "title": "native" }),
    );
    assert!(
        !native.queue_expired(now + JS_DIALOG_TTL + std::time::Duration::from_secs(1)),
        "a native dialog must not inherit the JS 600 s queued-dialog deadline"
    );
}

#[test]
fn a_request_level_timeout_still_expires_native_and_js_alike() {
    use pir::tui::dialogs::DialogOverlay;
    use std::time::{Duration, Instant};

    for id in [1u64, pir::plugins::host::NATIVE_DIALOG_ID_BASE + 1] {
        let overlay = DialogOverlay::from_request(
            id,
            "confirm",
            &serde_json::json!({ "title": "t", "timeout": 5 }),
        );
        let now = Instant::now();
        assert!(!overlay.queue_expired(now));
        assert!(
            overlay.queue_expired(now + Duration::from_millis(10)),
            "an explicit request timeout bounds BOTH faces (id {id})"
        );
    }
}
