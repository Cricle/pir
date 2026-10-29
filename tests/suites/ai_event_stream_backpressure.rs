//! Bounded provider event queue + producer backpressure.
//!
//! Every non-terminal `AssistantMessageEvent` owns a full cumulative
//! `partial` snapshot, so the queue depth IS the frozen-snapshot retention.
//! These tests pin the three properties the memory fix depends on:
//!
//! 1. a producer using `push_await` stops at the queue capacity (no consumer,
//!    no unbounded retention) and resumes when a consumer pops (wake wiring);
//! 2. a terminal event is never blocked by a full queue — it enqueues past the
//!    bound, ends the stream and releases parked producers;
//! 3. the synchronous entries are explicit: `try_push` refuses when full,
//!    `push` is the lenient compatibility/test entry (documented as such).
//!
//! Plus the completion-consumer liveness case: `drain_and_result` (used by
//! result-only callers such as `ProviderRegistry::complete`) resolves against
//! a full queue instead of deadlocking on a parked producer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use pir::ai::event_stream::{AssistantMessageEventStream, DEFAULT_EVENT_QUEUE_CAPACITY};
use pir::ai::types::*;

fn message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    }
}

/// One `text_delta` event carrying `token` and a (frozen) partial snapshot.
fn delta(index: usize) -> AssistantMessageEvent {
    let token = format!("d{index} ");
    AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: token.clone(),
        partial: Arc::new(message(&token)),
    }
}

fn delta_text(event: &AssistantMessageEvent) -> String {
    match event {
        AssistantMessageEvent::TextDelta { delta, .. } => delta.clone(),
        other => panic!("expected a text delta, got {other:?}"),
    }
}

fn done_event() -> AssistantMessageEvent {
    AssistantMessageEventStream::done_event(TerminalReason::Stop, message("done"))
}

/// Poll `condition` until it holds or the guard expires (never waits forever).
async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return condition();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// (a) + (b): with no consumer the producer parks at the bound; popping one
/// event wakes it for exactly one more push, and every event still arrives
/// exactly once in FIFO order.
#[tokio::test]
async fn producer_parks_at_the_bound_and_a_pop_wakes_it() {
    let mut stream = AssistantMessageEventStream::with_capacity(3);
    assert_eq!(stream.capacity(), 3);
    let pushed = Arc::new(AtomicUsize::new(0));
    let producer = stream.clone();
    let counter = Arc::clone(&pushed);
    let handle = tokio::spawn(async move {
        for index in 0..10 {
            producer.push_await(delta(index)).await;
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });

    assert!(
        wait_until(|| stream.pending_count() == 3).await,
        "the producer fills the queue to capacity"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        pushed.load(Ordering::SeqCst),
        3,
        "the 4th push must park on the full queue instead of enqueueing"
    );
    assert_eq!(stream.pending_count(), 3);
    assert!(!stream.is_done());

    assert_eq!(delta_text(&stream.try_next().expect("first event")), "d0 ");
    assert!(
        wait_until(|| pushed.load(Ordering::SeqCst) == 4).await,
        "one pop must wake exactly one parked push"
    );
    assert_eq!(stream.pending_count(), 3);
    assert!(
        stream.push_wait_count() >= 1,
        "the producer recorded at least one backpressure wait"
    );

    let mut seen = vec!["d0 ".to_string()];
    tokio::time::timeout(Duration::from_secs(5), async {
        while seen.len() < 10 {
            match stream.next().await {
                Some(event) => seen.push(delta_text(&event)),
                None => break,
            }
        }
    })
    .await
    .expect("draining must not stall the producer");
    let expected: Vec<String> = (0..10).map(|index| format!("d{index} ")).collect();
    assert_eq!(seen, expected, "every event arrives exactly once, in order");
    handle.await.expect("producer task finishes");
    assert_eq!(pushed.load(Ordering::SeqCst), 10);
    assert!(stream.max_queue_depth() <= 3, "capacity held throughout");
    assert!(!stream.is_done(), "nothing terminated the stream");
}

/// (c): a terminal event is never blocked by a full queue. It enqueues past
/// the bound (so a draining consumer still observes it in FIFO order), ends
/// the stream, resolves `result()`, and releases parked producers.
#[tokio::test]
async fn terminal_event_squeezes_into_a_full_queue_and_releases_producers() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    let producer = stream.clone();
    let handle = tokio::spawn(async move {
        for index in 0..6 {
            producer.push_await(delta(index)).await;
        }
    });
    assert!(wait_until(|| stream.pending_count() == 2).await);

    let final_message = message("done");
    stream.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        final_message.clone(),
    ));

    assert!(stream.is_done());
    assert_eq!(
        stream.pending_count(),
        3,
        "the terminal event enters past the capacity bound"
    );
    assert_eq!(stream.result().await.as_deref(), Some(&final_message));
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("a parked producer must be released by the terminal event")
        .expect("producer task");

    let mut events = Vec::new();
    while let Some(event) = stream.try_next() {
        events.push(event);
    }
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], AssistantMessageEvent::TextDelta { .. }));
    assert!(matches!(events[1], AssistantMessageEvent::TextDelta { .. }));
    assert!(matches!(events[2], AssistantMessageEvent::Done { .. }));
    assert_eq!(stream.max_queue_depth(), 3);
    assert!(stream.try_next().is_none());
}

/// The synchronous entries are explicit: `try_push` refuses when the queue is
/// full or the stream is done (returning the event), while `push` is the
/// lenient compatibility/test entry — it never blocks, never drops, and may
/// exceed the bound. Production producers use `push_await`.
#[tokio::test]
async fn try_push_refuses_when_full_and_push_is_lenient() {
    let stream = AssistantMessageEventStream::with_capacity(1);
    assert!(stream.try_push(delta(0)).is_ok());
    let refused = stream
        .try_push(delta(1))
        .expect_err("a full queue refuses try_push");
    assert!(matches!(refused, AssistantMessageEvent::TextDelta { .. }));
    stream.push(delta(1));
    assert_eq!(stream.pending_count(), 2, "legacy push exceeds the bound");

    stream.push(done_event());
    assert!(stream.is_done());
    assert!(
        stream.try_push(delta(2)).is_err(),
        "a terminated stream refuses try_push"
    );
    stream.push(delta(2));
    assert_eq!(stream.pending_count(), 3, "post-terminal push is ignored");
}

/// A completion consumer (only the final message matters) resolves against a
/// full queue: `drain_and_result` pops-and-discards while waiting, so the
/// parked producer is released. Without the drain this test would time out.
#[tokio::test]
async fn drain_and_result_resolves_for_a_result_only_consumer() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    let producer = stream.clone();
    let handle = tokio::spawn(async move {
        for index in 0..8 {
            producer.push_await(delta(index)).await;
        }
        producer.push(done_event());
    });

    let resolved = tokio::time::timeout(Duration::from_secs(5), stream.drain_and_result())
        .await
        .expect("a result-only consumer must not park on a full queue")
        .expect("terminal result");
    assert_eq!(resolved.content.len(), 1);
    handle.await.expect("producer task");
    assert!(stream.is_done());
    assert_eq!(stream.pending_count(), 0, "draining consumed the queue");
    assert!(
        stream.max_queue_depth() <= 3,
        "capacity plus the terminal event at most"
    );
}

/// A slow consumer (polling with yields) still sees every event exactly once
/// and in order, while the queue depth stays at the capacity bound.
#[tokio::test]
async fn slow_consumer_sees_every_event_in_order_with_a_bounded_depth() {
    let mut stream = AssistantMessageEventStream::new();
    assert_eq!(stream.capacity(), DEFAULT_EVENT_QUEUE_CAPACITY);
    let producer = stream.clone();
    let handle = tokio::spawn(async move {
        for index in 0..200 {
            producer.push_await(delta(index)).await;
        }
        producer.push(done_event());
    });

    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = stream.next().await {
            if !matches!(event, AssistantMessageEvent::TextDelta { .. }) {
                continue;
            }
            seen.push(delta_text(&event));
            // Slow consumer: hand the producer a turn between pops.
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the stream must terminate once drained");
    let expected: Vec<String> = (0..200).map(|index| format!("d{index} ")).collect();
    assert_eq!(seen, expected);
    handle.await.expect("producer task");
    assert!(
        stream.max_queue_depth() <= DEFAULT_EVENT_QUEUE_CAPACITY + 1,
        "capacity plus the single terminal event"
    );
    assert!(stream.push_wait_count() > 0);
}
