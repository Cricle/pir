//! Unit tests for `pir::durable::session::observation`: retirement frames and
//! panicking listeners.

use pir::durable::session::observation::{
    retirement_operations, CommittedStateSource, CommittedWatch, WatchEnd,
};
use pir::durable::Context;
use serde_json::Map;

#[test]
fn advance_retirement_keeps_the_terminal_frame_and_records_retired() {
    let source = CommittedStateSource::new(Some(Map::new()), 1);
    source.advance_retirement(2, Context::background());

    let core = source.core();
    let mut core = core
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(core.ended(), Some(&WatchEnd::Retired));
    let frame = core.next_frame().expect("terminal frame");
    assert!(frame.value.is_none());
    assert_eq!(frame.ops, retirement_operations());
    assert!(core.next_frame().is_none());
}

#[test]
fn a_panicking_listener_still_ends_a_null_frame_as_retired() {
    let watch = CommittedWatch::new(Some(Map::new()), 1);
    watch.start(|_| panic!("listener exploded"));
    watch
        .core()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .advance(None, Vec::new(), 2, Context::background());
    watch.drain();

    assert!(watch.is_stopped());
    assert_eq!(
        watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended(),
        Some(&WatchEnd::Retired)
    );
}
