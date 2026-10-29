use futures_util::StreamExt;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::types::*;
use std::sync::Arc;
use std::time::Duration;

fn msg(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-1".into(),
        response_model: None,
        response_id: None,
        diagnostics: vec![],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        provider_thinking_level: None,
        thinking_level: None,
        duration_ms: None,
        timestamp: 0,
    }
}

#[tokio::test]
async fn terminal_result_and_post_terminal_events() {
    let stream = AssistantMessageEventStream::new();
    let message = msg("done");
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        message.clone(),
    ));
    stream.push(AssistantMessageEvent::Start {
        partial: Arc::new(message.clone()),
    });
    assert_eq!(stream.result().await.unwrap().as_ref(), &message);
    assert_eq!(stream.pending_count(), 1);
    assert!(stream.try_next().is_some());
    assert!(stream.try_next().is_none());
}

#[tokio::test]
async fn stream_iteration_and_end_result() {
    let stream = AssistantMessageEventStream::new();
    let message = msg("final");
    stream.push(AssistantMessageEvent::Start {
        partial: Arc::new(message.clone()),
    });
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        message,
    ));
    let mut count = 0;
    let mut pinned = std::pin::pin!(stream);
    while pinned.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, 2);
    let other = AssistantMessageEventStream::new();
    let final_message = Arc::new(msg("end"));
    other.end(Some(final_message.clone()));
    assert_eq!(other.result().await.unwrap(), final_message);
}

// ---------------------------------------------------------------------------
// durationMs: upstream AssistantMessageEventStream#time
// ---------------------------------------------------------------------------

/// A message that starts after the stream was created (the normal provider
/// case: the terminal message is built inside the stream).
fn fresh_message(text: &str) -> AssistantMessage {
    let mut message = msg(text);
    message.timestamp = pir::ai::util::now_ms();
    message
}

#[tokio::test]
async fn done_event_times_the_terminal_message() {
    let stream = AssistantMessageEventStream::new();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        fresh_message("done"),
    ));

    let event = stream.try_next().expect("terminal event");
    let event_message = match &event {
        AssistantMessageEvent::Done { message, .. } => message,
        other => panic!("expected done, got {other:?}"),
    };
    let duration = event_message
        .duration_ms
        .expect("terminal event message carries durationMs");
    assert!(
        duration >= 10,
        "durationMs covers the response: got {duration}"
    );
    assert!(duration < 10_000, "durationMs is monotonic ms: {duration}");

    let result = stream.result().await.expect("final result");
    assert_eq!(
        result.duration_ms,
        Some(duration),
        "the event and every result() caller observe one stamped value"
    );
}

#[tokio::test]
async fn error_event_times_the_terminal_message() {
    let stream = AssistantMessageEventStream::new();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut error = fresh_message("boom");
    error.stop_reason = StopReason::Error;
    stream.push(AssistantMessageEventStream::error_event(
        ErrorReason::Error,
        error,
    ));
    let result = stream.result().await.expect("final result");
    assert!(
        result.duration_ms.is_some_and(|duration| duration >= 10),
        "error terminal messages are timed like done ones: {:?}",
        result.duration_ms
    );
}

#[tokio::test]
async fn end_result_is_timed_when_it_started_inside_the_stream() {
    let stream = AssistantMessageEventStream::new();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stream.end(Some(Arc::new(fresh_message("end"))));
    let result = stream.result().await.expect("final result");
    assert!(
        result.duration_ms.is_some_and(|duration| duration >= 10),
        "end(result) times the result: {:?}",
        result.duration_ms
    );
}

#[tokio::test]
async fn try_push_and_push_await_time_terminal_messages() {
    let try_stream = AssistantMessageEventStream::new();
    tokio::time::sleep(Duration::from_millis(5)).await;
    try_stream
        .try_push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            fresh_message("try"),
        ))
        .expect("terminal try_push always succeeds");
    assert!(try_stream
        .result()
        .await
        .expect("result")
        .duration_ms
        .is_some());

    let await_stream = AssistantMessageEventStream::new();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(
        await_stream
            .push_await(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                fresh_message("await"),
            ))
            .await
    );
    assert!(await_stream
        .result()
        .await
        .expect("result")
        .duration_ms
        .is_some());
}

#[tokio::test]
async fn message_that_predates_the_stream_is_not_timed() {
    // A response that started elsewhere (a deferred result fetched later, or a
    // message forwarded from an earlier request) keeps its timestamp older
    // than this stream's creation and must stay untimed.
    let stream = AssistantMessageEventStream::new();
    let mut old = msg("old");
    old.timestamp = pir::ai::util::now_ms().saturating_sub(1);
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        old,
    ));
    assert_eq!(stream.result().await.expect("result").duration_ms, None);

    // Same rule through `end(result)`.
    let ended = AssistantMessageEventStream::new();
    let mut old = msg("old-end");
    old.timestamp = pir::ai::util::now_ms().saturating_sub(60_000);
    ended.end(Some(Arc::new(old)));
    assert_eq!(ended.result().await.expect("result").duration_ms, None);
}

#[tokio::test]
async fn message_without_a_timestamp_is_not_timed() {
    // The Rust message always carries a `timestamp`, so its "missing" form is
    // the default 0: it predates every stream and stays untimed (upstream's
    // `undefined`/0 boundary behaves the same way).
    let stream = AssistantMessageEventStream::new();
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        msg("no-timestamp"),
    ));
    let result = stream.result().await.expect("result");
    assert_eq!(result.timestamp, 0);
    assert_eq!(result.duration_ms, None);
}

#[tokio::test]
async fn existing_duration_ms_is_never_overwritten() {
    let stream = AssistantMessageEventStream::new();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let mut message = fresh_message("pre-timed");
    message.duration_ms = Some(42);
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        message,
    ));
    assert_eq!(
        stream.result().await.expect("result").duration_ms,
        Some(42),
        "a message that already carries durationMs keeps it"
    );
}
