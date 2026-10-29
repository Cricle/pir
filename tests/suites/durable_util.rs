//! The engine-free harness helpers in `src/durable/harness/util.rs`.
//!
//! Covers the paginated scan folding, the closed-adapter error, the optional
//! extension record helper, the contained host callback, and the two ways a
//! session can expose its failure to `closed_error` (the `HasFailure` trait).
//!
//! The upstream `Waiters` class from the same `util.ts` is deliberately absent
//! from this module (`util.rs:4-7`): the scheduler owns its await registries as
//! private fields, so there is no public waiter API here to drive. The
//! scheduler's own waiter behaviour belongs to its suite, not this one.
//!
//! Tests live in an integration suite because the crate forbids inline tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pir::durable::errors::StorageError;
use pir::durable::harness::util::{
    closed_error, contained, optional_extension, scan_all, HasFailure, Report,
};
use pir::durable::session::guard::{FailureLatch, FailureLatchCore};
use pir::durable::types::{Cursor, Page, ScanOrder};

// ── helpers ────────────────────────────────────────────────────────────────

fn cursor(after: i64) -> Cursor {
    Cursor {
        after,
        order: Some(ScanOrder::Ascending),
    }
}

fn page(items: Vec<i64>, next: Option<Cursor>) -> Page<i64> {
    Page { items, next }
}

/// A report sink that records every rendered message.
fn recording_report() -> (Report, Arc<Mutex<Vec<String>>>) {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&messages);
    let report: Report = Arc::new(move |message: &str| {
        sink.lock()
            .expect("report sink lock")
            .push(message.to_owned());
    });
    (report, messages)
}

// ── scan_all ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn scan_all_concatenates_pages_in_order_and_passes_cursors_back() {
    // Three pages: a full first page, an empty middle page that still has a
    // cursor (so the loop must continue), and a terminal page.
    let pages = Mutex::new(VecDeque::from([
        Ok(page(vec![1, 2], Some(cursor(2)))),
        Ok(page(Vec::new(), Some(cursor(5)))),
        Ok(page(vec![7], None)),
    ]));
    let calls = Mutex::new(Vec::new());

    let scanned = scan_all(|next| {
        calls.lock().expect("calls lock").push(next);
        let page = pages
            .lock()
            .expect("pages lock")
            .pop_front()
            .expect("a queued page");
        std::future::ready(page)
    })
    .await;

    assert_eq!(scanned, Ok(vec![1, 2, 7]));
    assert_eq!(
        calls.into_inner().expect("calls lock"),
        vec![None, Some(cursor(2)), Some(cursor(5))],
        "the first read has no cursor; each later read receives the previous page's next"
    );
}

#[tokio::test]
async fn scan_all_returns_all_items_of_a_single_terminal_page() {
    let calls = Mutex::new(Vec::new());
    let scanned = scan_all(|next| {
        calls.lock().expect("calls lock").push(next);
        std::future::ready(Ok(page(vec![10, 20, 30], None)))
    })
    .await;

    assert_eq!(scanned, Ok(vec![10, 20, 30]));
    assert_eq!(calls.into_inner().expect("calls lock"), vec![None]);
}

#[tokio::test]
async fn scan_all_stops_at_the_first_page_without_a_cursor() {
    // The second page must never be read: a missing cursor is the stop signal.
    let pages = Mutex::new(VecDeque::from([
        Ok(page(vec![3], None)),
        Ok(page(vec![4], None)),
    ]));
    let calls = Mutex::new(Vec::new());

    let scanned = scan_all(|next| {
        calls.lock().expect("calls lock").push(next);
        let page = pages
            .lock()
            .expect("pages lock")
            .pop_front()
            .expect("a queued page");
        std::future::ready(page)
    })
    .await;

    assert_eq!(scanned, Ok(vec![3]));
    assert_eq!(calls.into_inner().expect("calls lock").len(), 1);
    assert_eq!(
        pages.into_inner().expect("pages lock").len(),
        1,
        "the terminal page ended the scan early"
    );
}

#[tokio::test]
async fn scan_all_of_an_empty_scan_is_an_empty_vec() {
    let scanned: Result<Vec<i64>, StorageError> =
        scan_all(|_| std::future::ready(Ok(page(Vec::new(), None)))).await;
    assert_eq!(scanned, Ok(Vec::new()));
}

#[tokio::test]
async fn scan_all_returns_the_first_page_error_and_stops_reading() {
    let pages = Mutex::new(VecDeque::from([
        Ok(page(vec![1], Some(cursor(1)))),
        Err(StorageError::Io("page two failed".to_owned())),
        Ok(page(vec![9], None)),
    ]));
    let calls = Mutex::new(Vec::new());

    let scanned = scan_all(|next| {
        calls.lock().expect("calls lock").push(next);
        let page = pages
            .lock()
            .expect("pages lock")
            .pop_front()
            .expect("a queued page");
        std::future::ready(page)
    })
    .await;

    assert_eq!(
        scanned,
        Err(StorageError::Io("page two failed".to_owned())),
        "the failing page's error is returned verbatim"
    );
    assert_eq!(
        calls.into_inner().expect("calls lock"),
        vec![None, Some(cursor(1))],
        "no read starts after the failing one"
    );
    assert_eq!(
        pages.into_inner().expect("pages lock").len(),
        1,
        "the queued third page was never read"
    );
}

// ── optional_extension ─────────────────────────────────────────────────────

#[test]
fn optional_extension_keeps_absence_and_presence_distinct() {
    assert_eq!(optional_extension(None), None, "absent stays absent");
    assert_eq!(
        optional_extension(Some("tools")),
        Some("tools".to_owned()),
        "present becomes an owned field value"
    );
    // The empty string is a present extension, not an absent one: upstream
    // spreads the key whenever the value is defined.
    assert_eq!(optional_extension(Some("")), Some(String::new()));
    assert_ne!(optional_extension(Some("")), None);
}

// ── closed_error ───────────────────────────────────────────────────────────

/// A source that never failed: the adapter closed normally.
struct NeverFailed;

impl HasFailure for NeverFailed {
    fn failure(&self) -> Option<StorageError> {
        None
    }
}

/// A source that already failed with `cause`.
struct FailedWith(StorageError);

impl HasFailure for FailedWith {
    fn failure(&self) -> Option<StorageError> {
        Some(self.0.clone())
    }
}

#[test]
fn closed_error_without_a_failure_is_the_plain_closed_error() {
    let error = closed_error(&NeverFailed);
    assert_eq!(error, StorageError::Closed("Harness is closed".to_owned()));
    assert!(matches!(error, StorageError::Closed(_)));
}

#[test]
fn closed_error_with_a_failure_wraps_that_cause_in_session_failed() {
    let cause = StorageError::Io("storage went away".to_owned());
    let error = closed_error(&FailedWith(cause.clone()));

    match &error {
        StorageError::SessionFailed(inner) => {
            assert_eq!(inner.cause, cause, "the original error is kept verbatim");
        }
        other => panic!("expected StorageError::SessionFailed, got {other:?}"),
    }
    assert!(
        error.to_string().contains("storage went away"),
        "the rendered error names the cause: {error}"
    );
}

#[test]
fn failure_latch_core_is_a_has_failure_source_and_keeps_the_first_error() {
    let latch = FailureLatchCore::new();

    // Before any failure the adapter reports a plain close.
    assert_eq!(
        closed_error(&latch),
        StorageError::Closed("Harness is closed".to_owned())
    );

    latch.fail(StorageError::Poisoned(
        pir::durable::JsonlStoragePoisonedError::new("first failure"),
    ));
    // The latch keeps the first failure; a later one does not replace it.
    latch.fail(StorageError::Io("second failure".to_owned()));

    match closed_error(&latch) {
        StorageError::SessionFailed(inner) => assert_eq!(
            inner.cause,
            StorageError::Poisoned(pir::durable::JsonlStoragePoisonedError::new(
                "first failure"
            ))
        ),
        other => panic!("expected StorageError::SessionFailed, got {other:?}"),
    }
}

// ── contained ──────────────────────────────────────────────────────────────

#[test]
fn contained_does_not_report_a_normal_callback() {
    let (report, messages) = recording_report();
    contained(&report, || {});
    assert!(messages.lock().expect("messages lock").is_empty());
}

#[test]
fn contained_reports_a_panic_and_returns_normally() {
    let (report, messages) = recording_report();
    let returned = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&returned);

    contained(&report, || panic!("listener exploded"));
    flag.store(true, Ordering::SeqCst);

    assert!(
        returned.load(Ordering::SeqCst),
        "the panic is contained; the caller continues"
    );
    assert_eq!(
        *messages.lock().expect("messages lock"),
        vec!["listener exploded".to_owned()]
    );
}

#[test]
fn contained_renders_string_and_opaque_panic_payloads() {
    let (report, messages) = recording_report();

    // A formatted panic carries a `String` payload.
    contained(&report, || panic!("boom {}", 7));
    // A non-string payload falls back to the scheduler's generic message.
    contained(&report, || std::panic::panic_any(42_u8));

    assert_eq!(
        *messages.lock().expect("messages lock"),
        vec!["boom 7".to_owned(), "task handler panicked".to_owned(),]
    );
}
