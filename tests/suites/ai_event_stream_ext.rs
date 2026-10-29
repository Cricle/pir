//! Extended edge case tests for AI event stream.

use futures_util::StreamExt;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::*;
use std::sync::Arc;

fn msg(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    }
}

#[tokio::test]
async fn empty_stream_has_no_result() {
    let stream = AssistantMessageEventStream::new();
    // Stream with no events should return None for result
    stream.end(None);
    assert!(stream.result().await.is_none());
}

#[tokio::test]
async fn end_with_result_after_push() {
    let stream = AssistantMessageEventStream::new();
    let message = Arc::new(msg("final"));
    stream.end(Some(message.clone()));
    let result = stream.result().await;
    assert!(result.is_some());
    assert_eq!(result.as_ref().unwrap().as_ref(), message.as_ref());
}

#[tokio::test]
async fn done_event_sets_result() {
    let stream = AssistantMessageEventStream::new();
    let message = msg("done");
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        message.clone(),
    ));
    let result = stream.result().await;
    assert!(result.is_some());
    assert_eq!(result.as_ref().unwrap().as_ref(), &message);
}

#[tokio::test]
async fn error_event_sets_result() {
    let stream = AssistantMessageEventStream::new();
    let mut message = msg("error");
    message.stop_reason = StopReason::Error;
    message.error_message = Some("boom".into());
    stream.push(AssistantMessageEventStream::error_event(
        ErrorReason::Error,
        message.clone(),
    ));
    let result = stream.result().await;
    assert!(result.is_some());
    assert_eq!(result.as_ref().unwrap().stop_reason, StopReason::Error);
}

#[tokio::test]
async fn multiple_consumers_both_get_result() {
    let stream = AssistantMessageEventStream::new();
    let message = msg("shared");
    let arc_msg = Arc::new(message.clone());

    // Spawn two concurrent consumers
    let s1 = stream.clone();
    let s2 = stream.clone();
    let h1 = tokio::spawn(async move { s1.result().await });
    let h2 = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        s2.result().await
    });

    // Trigger after a short delay
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    stream.end(Some(arc_msg.clone()));

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();
    assert!(r1.is_some());
    assert!(r2.is_some());
    assert_eq!(r1.unwrap(), arc_msg);
}

#[tokio::test]
async fn events_pushed_after_terminal_are_ignored() {
    let stream = AssistantMessageEventStream::new();
    let message = msg("done");
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        message.clone(),
    ));
    stream.push(AssistantMessageEvent::Start {
        partial: Arc::new(msg("ignored")),
    });
    // Only the first event (done) should be in the queue
    assert_eq!(stream.pending_count(), 1);
}

#[tokio::test]
async fn stream_iterates_events() {
    let stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEvent::Start {
        partial: Arc::new(msg("start")),
    });
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        msg("done"),
    ));

    let mut count = 0;
    let mut pinned = std::pin::pin!(stream);
    while let Some(_event) = pinned.next().await {
        count += 1;
    }
    assert_eq!(count, 2);
}

#[tokio::test]
async fn result_expect_returns_result() {
    let stream = AssistantMessageEventStream::new();
    let message = Arc::new(msg("ok"));
    stream.end(Some(message.clone()));
    let result = stream.result_expect().await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), message);
}

#[tokio::test]
async fn result_expect_returns_error_for_empty_stream() {
    let stream = AssistantMessageEventStream::new();
    stream.end(None);
    let result = stream.result_expect().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn done_event_type_renders_correctly() {
    let event = AssistantMessageEventStream::done_event(TerminalReason::Stop, msg("test"));
    let s = format!("{event:?}");
    assert!(s.contains("done") || s.contains("Done"), "got: {s}");
}

#[tokio::test]
async fn error_event_type_renders_correctly() {
    let mut m = msg("test");
    m.stop_reason = StopReason::Error;
    m.error_message = Some("fail".into());
    let event = AssistantMessageEventStream::error_event(ErrorReason::Error, m);
    let s = format!("{event:?}");
    assert!(s.contains("error") || s.contains("Error"), "got: {s}");
}
