//! Consumer liveness: a producer must never park forever when the consumer
//! abandons the stream (W13-N).
//!
//! `AssistantMessageEventStream` is a shared bounded queue: providers apply
//! backpressure through `push_await`, which parks while the queue is full.
//! A producer task holds its own handle, so before consumer liveness existed a
//! consumer that was dropped (abort, teardown, error path) left the producer
//! parked with nobody left to pop an event or terminate the stream — a leaked
//! task (and, through it, the transport read loop).
//!
//! These tests pin the fix and every contract it must not weaken:
//!
//! 1. dropping the last consumer handle wakes a parked producer, which then
//!    reports closure (`push_await` -> `false`) within one wake;
//! 2. a full queue still parks the producer and a pop still resumes it
//!    (backpressure intact, byte-identical event order);
//! 3. a terminal event still squeezes in past the capacity bound;
//! 4. `drain_and_result` still resolves the terminal result of a full stream;
//! 5. producer handles do not count as consumers, while cloning a consumer
//!    handle keeps the stream alive.
//!
//! Every wait is bounded by a `tokio::time::timeout` so a regression FAILS
//! loudly instead of hanging the harness.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use pir::ai::event_stream::AssistantMessageEventStream;
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

fn done_event(text: &str) -> AssistantMessageEvent {
    AssistantMessageEventStream::done_event(TerminalReason::Stop, message(text))
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

/// (a) The bug: the producer is parked on a full queue when the consumer is
/// dropped. It must wake, observe closure and stop — bounded by one wake.
#[tokio::test]
async fn dropped_consumer_wakes_a_parked_producer() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    // The producer task drives its own (non-counting) handle; `stream` is the
    // only consumer handle and is dropped mid-stream below.
    let producer = stream.producer();
    assert_eq!(stream.consumer_count(), 1);

    let handle = tokio::spawn(async move {
        let mut delivered = 0usize;
        let mut refused = false;
        for index in 0..10 {
            if producer.push_await(delta(index)).await {
                delivered += 1;
            } else {
                refused = true;
                break;
            }
        }
        let abandoned = producer.is_abandoned();
        let pending = producer.pending_count();
        (delivered, refused, abandoned, pending)
    });

    assert!(
        wait_until(|| stream.pending_count() == 2).await,
        "the producer fills the queue to capacity"
    );
    assert!(
        wait_until(|| stream.push_wait_count() >= 1).await,
        "the producer parks on the full queue"
    );

    drop(stream);

    let (delivered, refused, abandoned, pending) =
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("a producer parked in push_await must observe the dropped consumer")
            .expect("producer task");

    assert!(
        refused,
        "push_await must report closure instead of parking forever"
    );
    assert!(abandoned, "the stream is marked abandoned");
    assert_eq!(delivered, 2, "only the events that fit were delivered");
    assert_eq!(pending, 2, "the abandon path does not corrupt the queue");
}

/// (a, second shape): the consumer is already gone when the producer starts —
/// the very first `push_await` reports closure without parking at all.
#[tokio::test]
async fn push_await_after_every_consumer_dropped_reports_closure() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    let producer = stream.producer();
    drop(stream);
    assert!(producer.is_abandoned());
    assert_eq!(producer.consumer_count(), 0);

    let pushed = tokio::time::timeout(Duration::from_secs(2), producer.push_await(delta(0)))
        .await
        .expect("push_await must return promptly once no consumer remains");
    assert!(!pushed, "the event was not delivered");
    assert_eq!(producer.pending_count(), 0, "nothing was enqueued");
    assert!(!producer.is_done(), "abandonment is not termination");

    // A terminal push still finishes the stream (harmless: no consumer reads
    // it), so producers that end after the consumer vanished never error out.
    assert!(tokio::time::timeout(
        Duration::from_secs(2),
        producer.push_await(done_event("late"))
    )
    .await
    .expect("a terminal push never blocks"));
    assert!(producer.is_done());
}

/// (b) Backpressure is intact: a full queue still parks the producer, one pop
/// still wakes exactly one push, and every event arrives exactly once in order.
#[tokio::test]
async fn queue_still_parks_the_producer_and_a_drain_resumes_it() {
    let mut stream = AssistantMessageEventStream::with_capacity(3);
    let producer = stream.producer();
    let handle = tokio::spawn(async move {
        let mut delivered = 0usize;
        for index in 0..10 {
            assert!(
                producer.push_await(delta(index)).await,
                "a live consumer must accept every event"
            );
            delivered += 1;
        }
        delivered
    });

    assert!(
        wait_until(|| stream.pending_count() == 3).await,
        "the producer stops at the capacity bound"
    );
    // Hand a would-be runaway producer a turn (no wall-clock wait): with the
    // bound intact it stays parked, so the depth cannot grow.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        stream.pending_count(),
        3,
        "the 4th push must not exceed the bound"
    );
    assert!(
        stream.push_wait_count() >= 1,
        "the producer recorded a backpressure wait"
    );
    assert!(!stream.is_done());

    assert_eq!(delta_text(&stream.try_next().expect("first event")), "d0 ");
    assert!(
        wait_until(|| stream.pending_count() == 3).await,
        "one pop wakes exactly one parked push"
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
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the producer task finishes")
            .expect("producer task"),
        10
    );
    assert!(
        stream.max_queue_depth() <= 3,
        "capacity held throughout (peak {})",
        stream.max_queue_depth()
    );
    assert!(!stream.is_abandoned(), "a live consumer never abandons");
}

/// (b, liveness): the producer handle itself is not a consumer, so a stream
/// with no consumer handles is abandoned even while a producer still holds a
/// handle; the count never goes negative on repeated drops.
#[tokio::test]
async fn producer_handle_does_not_count_as_a_consumer() {
    let stream = AssistantMessageEventStream::with_capacity(4);
    assert_eq!(stream.consumer_count(), 1);
    let producer = stream.producer();
    let producer_clone = producer.clone();
    assert!(producer_clone.push_await(delta(0)).await);
    assert_eq!(
        stream.consumer_count(),
        1,
        "producer handles never count as consumers"
    );

    let consumer_clone = stream.clone();
    assert_eq!(stream.consumer_count(), 2, "cloning a consumer counts");
    drop(consumer_clone);
    assert_eq!(stream.consumer_count(), 1);
    assert!(
        !stream.is_abandoned(),
        "one live consumer keeps the stream open"
    );

    // A producer handle can be cloned and dropped freely without closing it.
    drop(producer_clone);
    assert!(!stream.is_abandoned());
    assert_eq!(stream.consumer_count(), 1);
    assert!(producer.push_await(delta(1)).await);
    assert_eq!(stream.pending_count(), 2);
}

/// (c) A terminal event is never blocked by a full queue: it squeezes past the
/// bound, ends the stream, and releases a parked producer.
#[tokio::test]
async fn terminal_event_still_squeezes_past_the_bound() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    let producer = stream.producer();
    let handle = tokio::spawn(async move {
        for index in 0..6 {
            if !producer.push_await(delta(index)).await {
                return false;
            }
        }
        true
    });
    assert!(wait_until(|| stream.pending_count() == 2).await);
    assert!(wait_until(|| stream.push_wait_count() >= 1).await);

    let final_message = message("final");
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
    assert!(
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("a parked producer is released by the terminal event")
            .expect("producer task"),
        "the parked producer resumes (true), it is not an abandon signal"
    );

    let mut events = Vec::new();
    while let Some(event) = stream.try_next() {
        events.push(event);
    }
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], AssistantMessageEvent::TextDelta { .. }));
    assert!(matches!(events[1], AssistantMessageEvent::TextDelta { .. }));
    assert!(matches!(events[2], AssistantMessageEvent::Done { .. }));
    assert_eq!(stream.max_queue_depth(), 3);
}

/// (d) The completion consumer still resolves: `drain_and_result` pops while
/// waiting, so a result-only consumer releases a parked producer and returns
/// the terminal message.
#[tokio::test]
async fn drain_and_result_returns_the_terminal_result() {
    let stream = AssistantMessageEventStream::with_capacity(2);
    let producer = stream.producer();
    let handle = tokio::spawn(async move {
        for index in 0..8 {
            assert!(producer.push_await(delta(index)).await);
        }
        assert!(producer.push_await(done_event("terminal")).await);
    });

    let resolved = tokio::time::timeout(Duration::from_secs(5), stream.drain_and_result())
        .await
        .expect("a result-only consumer must not park on a full queue")
        .expect("terminal result");
    assert_eq!(resolved.content.len(), 1);
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the drained queue released the producer")
        .expect("producer task");
    assert!(stream.is_done());
    assert_eq!(stream.pending_count(), 0, "draining consumed the queue");
    assert!(!stream.is_abandoned(), "draining is not abandonment");
    assert!(
        stream.max_queue_depth() <= 3,
        "capacity plus the terminal event at most"
    );
}

/// `try_push` refuses on an abandoned stream (the event is handed back), and a
/// stream that terminated normally keeps its result after every consumer goes.
#[tokio::test]
async fn try_push_refuses_after_the_consumer_vanished() {
    let stream = AssistantMessageEventStream::with_capacity(4);
    let producer = stream.producer();
    drop(stream);

    let refused = producer
        .try_push(delta(0))
        .expect_err("an abandoned stream refuses try_push");
    assert!(matches!(refused, AssistantMessageEvent::TextDelta { .. }));

    // Normal completion first: the terminal result survives consumer drop.
    let completed = AssistantMessageEventStream::with_capacity(4);
    let producer = completed.producer();
    let final_message = message("kept");
    producer.push(AssistantMessageEventStream::done_event(
        TerminalReason::Stop,
        final_message.clone(),
    ));
    let consumer = completed.clone();
    drop(completed);
    assert_eq!(
        producer.pending_count(),
        1,
        "the terminal event stayed queued by the producer handle"
    );
    assert_eq!(
        consumer.drain_and_result().await.as_deref(),
        Some(&final_message),
        "the result survives every other handle being dropped"
    );
}
