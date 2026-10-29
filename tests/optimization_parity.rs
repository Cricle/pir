//! Parity guards for the performance-optimization wave.
//!
//! These tests pin the observable behavior that the optimizations must keep
//! byte-identical:
//! 1. The scrolled-render line diff (`tui::core::scrolled_changed_rows`) —
//!    the row set that the differential scrolled repaint rewrites.
//! 2. The assistant-message event protocol — the per-event `partial`
//!    snapshot semantics and serde shape (upstream `event.partial`), which
//!    the evaluated `Arc<AssistantMessage>` partial-sharing design would
//!    have had to preserve exactly (the evaluation rejected the change; the
//!    assertions stay as a regression net for the current design).

use std::sync::Arc;

use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, AssistantRole, ContentBlock, StopReason,
    TerminalReason, TextContent,
};
use pir::tui::core::{compensated_scroll_offset, scrolled_changed_rows};

// ---------------------------------------------------------------------------
// 1. Scrolled-render diff row sets
// ---------------------------------------------------------------------------

fn lines<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn scrolled_diff_identical_frames_change_nothing() {
    let previous = lines(["a", "b", "c"]);
    let painted = lines(["a", "b", "c"]);
    assert!(scrolled_changed_rows(&previous, &painted).is_empty());
}

#[test]
fn scrolled_diff_reports_only_changed_rows() {
    let previous = lines(["a", "b", "c", "d", "e"]);
    let painted = lines(["a", "b", "X", "d", "e"]);
    assert_eq!(scrolled_changed_rows(&previous, &painted), vec![2]);

    // Spinner-style: last row changes each frame.
    let painted = lines(["a", "b", "c", "d", "f"]);
    assert_eq!(scrolled_changed_rows(&previous, &painted), vec![4]);

    // First row changes (scrollbar thumb moves, window shifted by one).
    let painted = lines(["z", "c", "d", "e", "f"]);
    assert_eq!(
        scrolled_changed_rows(&previous, &painted),
        vec![0, 1, 2, 3, 4]
    );
}

#[test]
fn scrolled_diff_undiffable_frames_report_every_painted_row() {
    let previous = lines(["a", "b", "c"]);
    // Window grew/shrank: row mapping is invalid, repaint everything.
    let painted = lines(["a", "b", "c", "d"]);
    assert_eq!(scrolled_changed_rows(&previous, &painted), vec![0, 1, 2, 3]);

    // First scrolled frame after a follow frame: empty previous.
    assert_eq!(scrolled_changed_rows(&[], &painted), vec![0, 1, 2, 3]);

    // Both empty: nothing to paint.
    assert!(scrolled_changed_rows(&[], &[]).is_empty());
}

#[test]
fn scrolled_diff_compensation_stays_pinned() {
    // The pinning rule the diff path relies on: while scrolled, the offset
    // grows with transcript growth so the window content stays put (and the
    // diff only rewrites the dock/spinner rows).
    assert_eq!(compensated_scroll_offset(0, Some(10), 15, 100), 0);
    assert_eq!(compensated_scroll_offset(3, Some(10), 15, 100), 8);
    assert_eq!(compensated_scroll_offset(3, None, 15, 100), 3);
    // Clamped to the largest in-bounds offset.
    assert_eq!(compensated_scroll_offset(5, Some(10), 15, 6), 6);
}

// ---------------------------------------------------------------------------
// 2. Assistant-message event protocol parity
// ---------------------------------------------------------------------------

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "parity".into(),
        provider: "parity".into(),
        model: "parity".into(),
        stop_reason: StopReason::Pending,
        ..Default::default()
    }
}

#[test]
fn event_serde_shape_is_stable() {
    // The wire shape each delta event carries: tagged `type`, the delta, and
    // the full partial snapshot. An `Arc<AssistantMessage>` field would have
    // to serialize to exactly this.
    let event = AssistantMessageEvent::TextDelta {
        content_index: 2,
        delta: "he".into(),
        partial: Arc::new(assistant("the")),
    };
    let value = serde_json::to_value(&event).expect("serialize event");
    assert_eq!(value["type"], "text_delta");
    assert_eq!(value["content_index"], 2);
    assert_eq!(value["delta"], "he");
    assert_eq!(value["partial"]["role"], "assistant");
    assert_eq!(value["partial"]["content"][0]["text"], "the");
    assert_eq!(value["partial"]["stopReason"], "pending");

    // Round-trip: deserialize reproduces the event (PartialEq derive).
    let back: AssistantMessageEvent = serde_json::from_value(value).expect("deserialize event");
    assert_eq!(back, event);
}

#[tokio::test]
async fn event_stream_preserves_order_and_snapshot_independence() {
    // Producers push one snapshot per delta; the consumer observes the same
    // sequence with each event's partial exactly as pushed (the invariant
    // "each event carries the accumulated message at push time").
    let stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEvent::Start {
        partial: Arc::new(assistant("")),
    });
    stream.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "a".into(),
        partial: Arc::new(assistant("a")),
    });
    stream.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "b".into(),
        partial: Arc::new(assistant("ab")),
    });
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        assistant("ab"),
    ));

    let mut seen = Vec::new();
    while let Some(event) = stream.try_next() {
        seen.push(event);
    }
    assert_eq!(seen.len(), 4);

    let partials: Vec<String> = seen
        .iter()
        .map(|event| match event {
            AssistantMessageEvent::Start { partial } => partial_text(partial),
            AssistantMessageEvent::TextDelta { partial, .. } => partial_text(partial),
            AssistantMessageEvent::Done { message, .. } => partial_text(message),
            _ => panic!(),
        })
        .collect();
    assert_eq!(partials, vec!["", "a", "ab", "ab"]);
    assert_eq!(
        stream.result().await.map(|m| partial_text(&m)).as_deref(),
        Some("ab")
    );

    // Events pushed after termination are ignored.
    stream.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "c".into(),
        partial: Arc::new(assistant("abc")),
    });
    assert!(stream.try_next().is_none());
    assert!(stream.is_done());
}

fn partial_text(message: &AssistantMessage) -> String {
    match message.content.first() {
        Some(ContentBlock::Text(text)) => text.text.clone(),
        _ => String::new(),
    }
}
