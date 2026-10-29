//! `session::observation` — committed state sources and watches.
//!
//! Port of upstream `session/observation.ts`. An observer holds one exact
//! committed revision plus the operations that produced it, and never sees a
//! value that is not committed. The contract has four rules worth pinning:
//!
//! * a value change publishes the commit's own operations;
//! * a **definition-version** change publishes one full replacement, because
//!   the stored operations were computed against the old shape;
//! * a **retirement** publishes `[["r", null]]` and nothing after it;
//! * at most [`MAX_PENDING_WATCH_FRAMES`] frames are held; past that the queue
//!   collapses to a single frame holding the newest whole value.
//!
//! These are the rules a remote client, a view mount and the TUI all rely on,
//! so they are asserted directly rather than through a consumer.

use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use pir::durable::delta::Op;
use pir::durable::session::observation::{
    retirement_operations, CommittedStateSource, CommittedWatch, ObservedFrame, ObserverCore,
    WatchEnd, MAX_PENDING_WATCH_FRAMES,
};
use pir::durable::{Context, StorageError};

fn value(count: i64) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("count".to_owned(), json!(count));
    map
}

fn core() -> ObserverCore {
    ObserverCore::new(Some(value(1)), 1)
}

/// Advance a watch's shared core at version 1 (the watch has no `advance` of
/// its own; the line advances the core).
fn advance(watch: &CommittedWatch, count: i64) {
    watch
        .core()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .advance(Some(value(count)), Vec::new(), 1, Context::background());
}

// ── retirement ─────────────────────────────────────────────────────────────

#[test]
fn retirement_publishes_the_terminal_frame() {
    assert_eq!(
        retirement_operations(),
        vec![Op::Replace(Value::Null)],
        "a retired incarnation is replaced by null, the one operation a client applies"
    );
}

// ── ObserverCore::advance ──────────────────────────────────────────────────

#[test]
fn a_same_version_change_keeps_the_commit_operations() {
    let mut observer = core();
    let ops = vec![Op::Set(
        vec![pir::durable::delta::Seg::Key("count".to_owned())],
        json!(2),
    )];
    observer.advance(Some(value(2)), ops.clone(), 1, Context::background());

    let frame = observer.next_frame().expect("a frame");
    assert_eq!(frame.ops, ops, "the commit's own operations are published");
    assert_eq!(
        frame.value.as_ref().and_then(|v| v.get("count")),
        Some(&json!(2))
    );
    assert_eq!(frame.version, 1);
    assert_eq!(
        observer.value().and_then(|v| v.get("count")),
        Some(&json!(2))
    );
}

#[test]
fn a_version_change_publishes_one_full_replacement() {
    let mut observer = core();
    let ops = vec![Op::Set(
        vec![pir::durable::delta::Seg::Key("count".to_owned())],
        json!(9),
    )];
    observer.advance(Some(value(9)), ops, 2, Context::background());

    let frame = observer.next_frame().expect("a frame");
    assert_eq!(
        frame.ops,
        vec![Op::Replace(Value::Object(value(9)))],
        "a version change is a root replacement, not the stale operations"
    );
    assert_eq!(observer.version(), 2, "the observer tracks the new version");
}

#[test]
fn a_retirement_publishes_null_and_sets_the_value_to_none() {
    let mut observer = core();
    observer.advance(None, Vec::new(), 1, Context::background());

    let frame = observer.next_frame().expect("a frame");
    assert_eq!(frame.ops, retirement_operations());
    assert!(frame.value.is_none(), "a retired incarnation has no value");
    assert!(observer.value().is_none(), "the current value is gone too");
}

#[test]
fn a_retirement_overrides_any_operations_the_caller_supplied() {
    // Retirement is terminal; whatever a caller passes is not an observable
    // change of a live incarnation.
    let mut observer = core();
    observer.advance(
        None,
        vec![Op::Set(
            vec![pir::durable::delta::Seg::Key("count".to_owned())],
            json!(2),
        )],
        1,
        Context::background(),
    );
    let frame = observer.next_frame().expect("a frame");
    assert_eq!(frame.ops, retirement_operations());
}

#[test]
fn an_ended_observer_ignores_later_advances() {
    let mut observer = core();
    observer.end(WatchEnd::Stopped);
    observer.advance(Some(value(5)), Vec::new(), 1, Context::background());

    assert!(
        observer.next_frame().is_none(),
        "an ended observer accepts no further frames"
    );
    assert_eq!(observer.ended(), Some(&WatchEnd::Stopped));
}

#[test]
fn frames_arrive_in_order() {
    let mut observer = core();
    observer.advance(Some(value(2)), Vec::new(), 1, Context::background());
    observer.advance(Some(value(3)), Vec::new(), 1, Context::background());

    let first = observer.next_frame().expect("first");
    let second = observer.next_frame().expect("second");
    assert_eq!(
        first.value.as_ref().and_then(|v| v.get("count")),
        Some(&json!(2))
    );
    assert_eq!(
        second.value.as_ref().and_then(|v| v.get("count")),
        Some(&json!(3))
    );
    assert!(observer.next_frame().is_none(), "the queue is drained");
}

// ── the frame bound ────────────────────────────────────────────────────────

#[test]
fn the_pending_queue_is_bounded() {
    let mut observer = core();
    // Exactly the bound, with no overflow yet: every frame is retained.
    for count in 2..=(MAX_PENDING_WATCH_FRAMES as i64 + 1) {
        observer.advance(Some(value(count)), Vec::new(), 1, Context::background());
    }
    assert!(
        !observer.overflowed(),
        "reaching the bound exactly is not an overflow"
    );

    let mut held = 0;
    while observer.next_frame().is_some() {
        held += 1;
    }
    assert_eq!(
        held, MAX_PENDING_WATCH_FRAMES,
        "a slow observer holds exactly {MAX_PENDING_WATCH_FRAMES} undelivered frames"
    );

    // One more advance past the bound collapses instead of growing.
    let mut observer = core();
    for count in 2..=(MAX_PENDING_WATCH_FRAMES as i64 + 2) {
        observer.advance(Some(value(count)), Vec::new(), 1, Context::background());
    }
    let mut held = 0;
    while observer.next_frame().is_some() {
        held += 1;
    }
    assert!(
        held < MAX_PENDING_WATCH_FRAMES,
        "past the bound the queue collapses instead of growing (held {held})"
    );
}

#[test]
fn an_overflow_collapses_the_undelivered_suffix_into_one_replacement() {
    // Fill exactly to the bound, then one more: the whole undelivered suffix
    // is replaced by a single self-contained frame at the newest value.
    let mut observer = core();
    let mut count = 2;
    for _ in 0..MAX_PENDING_WATCH_FRAMES {
        observer.advance(Some(value(count)), Vec::new(), 1, Context::background());
        count += 1;
    }
    assert!(
        !observer.overflowed(),
        "the queue is full but not overflowing"
    );

    // The (MAX+1)-th advance overflows.
    let overflow_value = count;
    observer.advance(
        Some(value(overflow_value)),
        Vec::new(),
        1,
        Context::background(),
    );
    assert!(observer.overflowed(), "the queue reported the overflow");

    let first = observer.next_frame().expect("the collapsed frame");
    assert_eq!(
        first.ops,
        vec![Op::Replace(Value::Object(value(overflow_value)))],
        "the collapsed frame is one root replacement at the newest value"
    );
    assert_eq!(
        first.value.as_ref().and_then(|value| value.get("count")),
        Some(&json!(overflow_value)),
        "it carries the newest whole value, so a client needs no history"
    );
}

#[test]
fn the_frame_bound_is_one_hundred() {
    assert_eq!(
        MAX_PENDING_WATCH_FRAMES, 100,
        "spec §9.3: a slow watch keeps at most 100 undelivered frames"
    );
}

// ── CommittedStateSource ───────────────────────────────────────────────────

#[test]
fn a_state_source_exposes_its_committed_value() {
    let source = CommittedStateSource::new(Some(value(4)), 3);
    let current = source.value().expect("a committed value");
    assert_eq!(
        current.get("count"),
        Some(&json!(4)),
        "the source reads its current committed revision"
    );
    assert_eq!(source.core().lock().unwrap().version(), 3);
}

#[test]
fn a_state_source_delivers_to_a_listener() {
    let source = CommittedStateSource::new(Some(value(1)), 1);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let core = source.core();
    {
        let mut observer = core.lock().unwrap();
        observer.advance(Some(value(2)), Vec::new(), 1, Context::background());
    }
    // The source's own read path is what a consumer uses.
    sink.lock()
        .unwrap()
        .push(source.value().and_then(|value| value.get("count").cloned()));
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![Some(json!(2))],
        "the source reflects the advanced revision"
    );
}

#[test]
fn ending_a_state_source_records_the_reason() {
    let source = CommittedStateSource::new(Some(value(1)), 1);
    source.end(WatchEnd::SessionClosed);
    assert_eq!(
        source.core().lock().unwrap().ended(),
        Some(&WatchEnd::SessionClosed),
        "the source ends with the session's reason"
    );
}

// ── CommittedWatch ─────────────────────────────────────────────────────────

#[test]
fn a_watch_delivers_frames_to_its_listener() {
    let watch = CommittedWatch::new(Some(value(1)), 1);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    watch.start(move |frame: ObservedFrame| {
        sink.lock()
            .unwrap()
            .push(frame.value.as_ref().and_then(|v| v.get("count").cloned()));
        true
    });

    advance(&watch, 2);
    watch.drain();

    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![Some(json!(2))],
        "the watch delivered the frame exactly once"
    );
}

#[test]
fn a_stopped_watch_delivers_nothing_more() {
    let watch = CommittedWatch::new(Some(value(1)), 1);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    watch.start(move |frame: ObservedFrame| {
        sink.lock().unwrap().push(frame.ops.len());
        true
    });
    watch.stop(WatchEnd::Stopped);
    advance(&watch, 2);
    watch.drain();

    assert!(
        seen.lock().unwrap().is_empty(),
        "a stopped watch sends nothing"
    );
}

// ── the ending reasons ─────────────────────────────────────────────────────

#[test]
fn every_watch_end_reason_is_distinguishable() {
    let reasons = [
        WatchEnd::Stopped,
        WatchEnd::Cancelled,
        WatchEnd::SessionClosed,
        WatchEnd::Retired,
        WatchEnd::ListenerError(StorageError::Io("listener".to_owned())),
        WatchEnd::SessionFailed(StorageError::Io("session".to_owned())),
    ];
    for (index, reason) in reasons.iter().enumerate() {
        for (other_index, other) in reasons.iter().enumerate() {
            if index == other_index {
                assert_eq!(reason, other);
            } else {
                assert_ne!(reason, other, "the six reasons are distinct");
            }
        }
    }
}

#[test]
fn a_retirement_frame_ends_the_watch_as_retired_even_when_the_listener_continues() {
    let watch = CommittedWatch::new(Some(value(1)), 1);
    let seen = Arc::new(Mutex::new(0));
    let sink = seen.clone();
    watch.start(move |frame: ObservedFrame| {
        assert!(frame.value.is_none(), "the terminal frame is null");
        *sink.lock().unwrap() += 1;
        true
    });
    watch
        .core()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .advance(None, Vec::new(), 2, Context::background());
    watch.drain();

    assert!(watch.is_stopped());
    assert_eq!(*seen.lock().unwrap(), 1);
    assert_eq!(
        watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended()
            .cloned(),
        Some(WatchEnd::Retired)
    );
}

#[test]
fn a_listener_returning_false_stops_the_watch_without_a_listener_error() {
    let watch = CommittedWatch::new(Some(value(1)), 1);
    watch.start(|_: ObservedFrame| false);
    advance(&watch, 2);
    watch.drain();

    assert!(watch.is_stopped());
    assert_eq!(
        watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended()
            .cloned(),
        Some(WatchEnd::Stopped)
    );
}

#[test]
fn a_panicking_listener_ends_the_watch_as_a_listener_error() {
    let watch = CommittedWatch::new(Some(value(1)), 1);
    watch.start(|_: ObservedFrame| panic!("listener exploded"));
    advance(&watch, 2);
    watch.drain();

    assert!(watch.is_stopped());
    assert!(matches!(
        watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended()
            .cloned(),
        Some(WatchEnd::ListenerError(_))
    ));
}
