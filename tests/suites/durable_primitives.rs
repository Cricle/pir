//! Tests for the durable top-level pure utility modules.
//!
//! * `src/durable/search.rs` — tokenization and BM25 ranking.
//! * `src/durable/truncate.rs` — head truncation, byte/length limits, sizes.
//! * `src/durable/timers.rs` — delay chunking and the injectable timer host.
//!
//! No storage, session or harness state is involved: every assertion here is
//! about a pure function or about a timer host the test drives itself.

use pir::durable::{
    delay_steps, format_size, native_timer, rank_documents, tokenize, truncate_head,
    truncate_head_of, utf8_byte_length, SearchDocument, SetTimer, SetTimerFn, TimerCallback,
    TimerCancel, TimerOptions, Timers, Totals, TruncatedBy, TruncationOptions, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES, MAX_TIMER_DELAY_MS,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── search: tokenization ───────────────────────────────────────────────────

fn document<'a>(name: &'a str, text: &'a str) -> SearchDocument<'a> {
    SearchDocument { name, text }
}

#[test]
fn tokenize_lowercases_splits_camel_case_and_drops_stop_words() {
    assert_eq!(
        tokenize("readFile and write_file issues"),
        vec!["read", "file", "write", "file", "issue"]
    );
    assert_eq!(tokenize("HTTPRequest"), vec!["http", "request"]);
    assert_eq!(tokenize("JSONParser"), vec!["json", "parser"]);
    assert_eq!(
        tokenize("read\tfile\nsearch!"),
        vec!["read", "file", "search"]
    );
    assert_eq!(tokenize("the of and"), Vec::<String>::new());
    assert_eq!(tokenize("   \t\n "), Vec::<String>::new());
    assert_eq!(tokenize(""), Vec::<String>::new());
}

#[test]
fn tokenize_applies_the_naive_singular_form() {
    // The implementation ports upstream's plural stripping, not a Porter
    // stemmer: "issues" matches "issue", and every `s`-suffix rule maps the
    // plural onto the same term as its singular.
    assert_eq!(tokenize("issues issue"), vec!["issue", "issue"]);
    assert_eq!(tokenize("searches search"), vec!["search", "search"]);
    assert_eq!(tokenize("boxes box"), vec!["box", "box"]);
    assert_eq!(tokenize("classes class"), vec!["class", "class"]);
    assert_eq!(tokenize("queries query"), vec!["query", "query"]);
    // `ss` and short terms are left alone.
    assert_eq!(tokenize("class"), vec!["class"]);
    assert_eq!(tokenize("bus"), vec!["bus"]);
    // There is no Porter stemming: "caching" and "cached" stay distinct.
    assert_eq!(tokenize("caching"), vec!["caching"]);
    assert_eq!(tokenize("cached"), vec!["cached"]);
    assert_ne!(tokenize("caching"), tokenize("cached"));
}

// ── search: BM25 ranking ───────────────────────────────────────────────────

#[test]
fn rank_returns_nothing_for_empty_unmatched_or_limited_queries() {
    let documents = [
        document("alpha", "alpha beta"),
        document("gamma", "gamma delta"),
    ];
    assert!(rank_documents("", &documents, 10).is_empty());
    assert!(rank_documents(" \t\n ", &documents, 10).is_empty());
    assert!(rank_documents("the of and", &documents, 10).is_empty());
    assert!(rank_documents("zeta", &documents, 10).is_empty());
    assert!(rank_documents("alpha", &documents, 0).is_empty());
    assert!(rank_documents("alpha", &[], 10).is_empty());
}

#[test]
fn rank_accumulates_scores_across_query_terms() {
    let documents = [
        document("both", "alpha beta"),
        document("alpha", "alpha"),
        document("beta", "beta"),
    ];
    // "both" matches two query terms; the one-term documents tie and keep
    // their input order.
    assert_eq!(
        rank_documents("alpha beta", &documents, 10),
        vec!["both", "alpha", "beta"]
    );

    let reordered = [
        document("both", "alpha beta"),
        document("beta", "beta"),
        document("alpha", "alpha"),
    ];
    assert_eq!(
        rank_documents("alpha beta", &reordered, 10),
        vec!["both", "beta", "alpha"],
        "equal scores keep document order"
    );
}

#[test]
fn rank_ties_keep_document_order() {
    let documents = [document("a", "alpha beta"), document("b", "beta alpha")];
    assert_eq!(rank_documents("alpha beta", &documents, 10), vec!["a", "b"]);

    let reversed = [document("b", "beta alpha"), document("a", "alpha beta")];
    assert_eq!(rank_documents("alpha beta", &reversed, 10), vec!["b", "a"]);
}

#[test]
fn rank_favors_the_shorter_document_at_equal_term_frequency() {
    // Both documents contain "alpha" once; BM25's length normalization makes
    // the short one score higher even though it is listed second.
    let documents = [
        document("long", "alpha filler filler filler"),
        document("short", "alpha"),
    ];
    assert_eq!(
        rank_documents("alpha", &documents, 10),
        vec!["short", "long"]
    );
}

#[test]
fn rank_favors_more_occurrences_at_equal_length() {
    let documents = [
        document("one", "alpha aaa bbb"),
        document("two", "alpha alpha bbb"),
    ];
    assert_eq!(rank_documents("alpha", &documents, 10), vec!["two", "one"]);
}

#[test]
fn rank_term_frequency_saturates_instead_of_growing_linearly() {
    // A document repeating "x" 100 times versus one containing "x" and the
    // rarer "y". A linear term-frequency sum would rank the 100-occurrence
    // document first; BM25's saturating tf component (plus length
    // normalization) ranks the two-term document first.
    let hundred = "x ".repeat(100);
    let documents = [document("hundred", &hundred), document("pair", "x y")];
    assert_eq!(
        rank_documents("x y", &documents, 10),
        vec!["pair", "hundred"]
    );
    assert_eq!(rank_documents("x y", &documents, 1), vec!["pair"]);
}

#[test]
fn rank_deduplicates_repeated_query_terms() {
    let documents = [document("a", "alpha"), document("b", "beta alpha")];
    assert_eq!(
        rank_documents("alpha alpha alpha", &documents, 10),
        rank_documents("alpha", &documents, 10)
    );
    assert_eq!(rank_documents("alpha", &documents, 10), vec!["a", "b"]);
}

#[test]
fn rank_single_document_matches_its_own_terms() {
    let documents = [document("only", "alpha")];
    assert_eq!(rank_documents("alpha", &documents, 10), vec!["only"]);
}

#[test]
fn rank_indexes_text_only_not_the_document_name() {
    // `SearchDocument.name` is the returned label, not a weighted title: the
    // port (like upstream `searchText`) expects the caller to fold the name
    // into `text`. A name-only match therefore finds nothing.
    let documents = [
        document("grep", "search text with a pattern"),
        document("read", "read a file from disk"),
    ];
    assert!(rank_documents("grep", &documents, 10).is_empty());
    assert_eq!(rank_documents("search", &documents, 10), vec!["grep"]);
    assert_eq!(rank_documents("read", &documents, 10), vec!["read"]);
}

// ── truncate: byte helpers and size formatting ─────────────────────────────

#[test]
fn utf8_byte_length_counts_encoded_bytes() {
    assert_eq!(utf8_byte_length(""), 0);
    assert_eq!(utf8_byte_length("ascii"), 5);
    assert_eq!(utf8_byte_length("é"), 2);
    assert_eq!(utf8_byte_length("你好"), 6);
    assert_eq!(utf8_byte_length("😀"), 4);
    assert_eq!(utf8_byte_length("aé😀"), 7);
    assert_eq!(utf8_byte_length("e\u{301}"), 3);
}

#[test]
fn format_size_matches_upstream_boundaries() {
    assert_eq!(format_size(0), "0B");
    assert_eq!(format_size(1), "1B");
    assert_eq!(format_size(1023), "1023B");
    assert_eq!(format_size(1024), "1.0KB");
    assert_eq!(format_size(1536), "1.5KB");
    assert_eq!(format_size(1024 * 1024 - 1), "1024.0KB");
    assert_eq!(format_size(1024 * 1024), "1.0MB");
    assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    // There is no GB unit: upstream formats everything past 1 MiB as MB.
    assert_eq!(format_size(1024 * 1024 * 1024), "1024.0MB");
}

// ── truncate: head truncation ──────────────────────────────────────────────

#[test]
fn default_options_are_all_none_and_resolve_to_the_default_constants() {
    let options = TruncationOptions::default();
    assert_eq!(options.max_lines, None);
    assert_eq!(options.max_bytes, None);

    let result = truncate_head("a\nb", options);
    assert!(!result.truncated);
    assert_eq!(result.max_lines, DEFAULT_MAX_LINES);
    assert_eq!(result.max_bytes, DEFAULT_MAX_BYTES);
    assert_eq!(result.content, "a\nb");
}

#[test]
fn truncate_head_keeps_everything_at_or_below_both_limits() {
    let content = "one\ntwo\nthree";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_lines: Some(3),
            max_bytes: Some(13),
        },
    );
    assert!(!result.truncated);
    assert_eq!(result.truncated_by, None);
    assert_eq!(result.content, content);
    assert_eq!(result.total_lines, 3);
    assert_eq!(result.total_bytes, 13);
    assert_eq!(result.output_lines, 3);
    assert_eq!(result.output_bytes, 13);
    assert!(!result.last_line_partial);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_stops_at_the_line_limit() {
    let content = "one\ntwo\nthree\nfour";
    let result = truncate_head(
        content,
        TruncationOptions {
            max_lines: Some(2),
            max_bytes: None,
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.content, "one\ntwo");
    assert_eq!(result.total_lines, 4);
    assert_eq!(result.total_bytes, 18);
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.output_bytes, 7);
    assert_eq!(result.max_lines, 2);
    assert_eq!(result.max_bytes, DEFAULT_MAX_BYTES);
    assert!(!result.last_line_partial);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_stops_at_the_byte_limit() {
    let result = truncate_head(
        "abcdef\nghijkl",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(10),
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "abcdef");
    assert_eq!(result.total_lines, 2);
    assert_eq!(result.output_lines, 1);
    assert_eq!(result.output_bytes, 6);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_keeps_a_line_that_exactly_fits_the_byte_limit() {
    let result = truncate_head(
        "one\ntwo\nthree",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(7),
        },
    );
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "one\ntwo");
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.output_bytes, 7);

    // A first line exactly at the limit is kept, not reported as exceeding.
    let first_line = truncate_head(
        "abcdef\nxyz",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(6),
        },
    );
    assert_eq!(first_line.content, "abcdef");
    assert_eq!(first_line.output_lines, 1);
    assert!(!first_line.first_line_exceeds_limit);
}

#[test]
fn truncate_head_reports_a_first_line_over_the_byte_limit() {
    let result = truncate_head(
        "0123456789abcdef",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(4),
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "");
    assert_eq!(result.total_lines, 1);
    assert_eq!(result.total_bytes, 16);
    assert_eq!(result.output_lines, 0);
    assert_eq!(result.output_bytes, 0);
    assert!(result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_handles_empty_content() {
    let result = truncate_head("", TruncationOptions::default());
    assert!(!result.truncated);
    assert_eq!(result.truncated_by, None);
    assert_eq!(result.content, "");
    assert_eq!(result.total_lines, 0);
    assert_eq!(result.total_bytes, 0);
    assert_eq!(result.output_lines, 0);
    assert_eq!(result.output_bytes, 0);

    let zero_limits = truncate_head(
        "",
        TruncationOptions {
            max_lines: Some(0),
            max_bytes: Some(0),
        },
    );
    assert!(!zero_limits.truncated);
    assert_eq!(zero_limits.content, "");
}

#[test]
fn truncate_head_does_not_count_a_trailing_newline_as_a_line() {
    let kept = truncate_head("a\nb\n", TruncationOptions::default());
    assert!(!kept.truncated);
    assert_eq!(kept.total_lines, 2);
    assert_eq!(kept.total_bytes, 4);
    assert_eq!(kept.content, "a\nb\n");

    // The trailing newline still counts toward bytes, so a 3-byte limit
    // truncates "abc\n" even though only the line content is returned.
    let trimmed = truncate_head(
        "abc\n",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(3),
        },
    );
    assert!(trimmed.truncated);
    assert_eq!(trimmed.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(trimmed.content, "abc");
    assert_eq!(trimmed.total_lines, 1);
    assert_eq!(trimmed.total_bytes, 4);
    assert_eq!(trimmed.output_lines, 1);
    assert_eq!(trimmed.output_bytes, 3);
}

#[test]
fn truncate_head_zero_line_limit_omits_everything() {
    let result = truncate_head(
        "a\nb",
        TruncationOptions {
            max_lines: Some(0),
            max_bytes: None,
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.content, "");
    assert_eq!(result.total_lines, 2);
    assert_eq!(result.output_lines, 0);
    assert_eq!(result.output_bytes, 0);
}

#[test]
fn truncate_head_counts_multibyte_characters_as_utf8_bytes() {
    // "é" is two bytes: the first line is 6 bytes, the second 4 with its
    // newline.
    let kept = truncate_head(
        "ééé\nbbb",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(6),
        },
    );
    assert_eq!(kept.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(kept.content, "ééé");
    assert_eq!(kept.output_bytes, 6);

    let none = truncate_head(
        "ééé\nbbb",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(5),
        },
    );
    assert_eq!(none.content, "");
    assert!(none.first_line_exceeds_limit);
}

#[test]
fn truncate_head_default_limits_apply_when_options_are_none() {
    // 2100 short lines: the 2000-line default is the first limit reached.
    let content = (0..2100)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let result = truncate_head(&content, TruncationOptions::default());
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.max_lines, DEFAULT_MAX_LINES);
    assert_eq!(result.total_lines, 2100);
    assert_eq!(result.output_lines, DEFAULT_MAX_LINES);
    assert!(result.content.starts_with("line 0"));
    assert!(result.content.ends_with("line 1999"));

    // 100 long lines: under the line default, over the 50 KiB byte default.
    let long_line = "x".repeat(600);
    let content = vec![long_line; 100].join("\n");
    let result = truncate_head(&content, TruncationOptions::default());
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.max_bytes, DEFAULT_MAX_BYTES);
    assert_eq!(result.total_lines, 100);
    assert_eq!(result.total_bytes, 60099);
    // 85 lines of 600 bytes plus 84 separators = 51084 bytes; one more line
    // (51685) would cross the 51200-byte limit.
    assert_eq!(result.output_lines, 85);
    assert_eq!(result.output_bytes, 51084);
}

#[test]
fn truncate_head_of_uses_the_supplied_totals() {
    let options = TruncationOptions {
        max_lines: Some(2),
        max_bytes: None,
    };
    let full = "a\nb\nc\nd";
    let totals = Totals { lines: 4, bytes: 7 };
    let result = truncate_head_of("a\nb\nc", totals, options);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.total_lines, 4);
    assert_eq!(result.total_bytes, 7);
    assert_eq!(result.content, "a\nb");
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.output_bytes, 3);
    assert_eq!(result, truncate_head(full, options));

    let untouched = truncate_head_of(
        "a\nb",
        Totals { lines: 2, bytes: 3 },
        TruncationOptions::default(),
    );
    assert!(!untouched.truncated);
    assert_eq!(untouched.content, "a\nb");
    assert_eq!(untouched.output_bytes, 3);
}

#[test]
fn truncate_head_of_prefix_longer_than_the_byte_limit_matches_truncate_head() {
    let full = "aaaa\nbbbb\ncccc";
    let totals = Totals {
        lines: 3,
        bytes: 14,
    };
    let options = TruncationOptions {
        max_lines: None,
        max_bytes: Some(10),
    };
    // The prefix is longer than max_bytes + 1 and holds lines past the limit,
    // which is the contract under which both functions agree.
    let prefix = "aaaa\nbbbb\ncccc\nDDDD";
    assert!(prefix.len() > 10 + 1);

    let via_prefix = truncate_head_of(prefix, totals, options);
    assert_eq!(via_prefix.content, "aaaa\nbbbb");
    assert_eq!(via_prefix.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(via_prefix.output_lines, 2);
    assert_eq!(via_prefix.output_bytes, 9);
    assert_eq!(via_prefix.total_lines, 3);
    assert_eq!(via_prefix.total_bytes, 14);
    assert_eq!(via_prefix, truncate_head(full, options));
}

// ── timers: delay chunking (pure) ──────────────────────────────────────────

#[test]
fn delay_steps_zero_negative_and_single_step() {
    assert_eq!(delay_steps(0).collect::<Vec<_>>(), vec![0]);
    assert_eq!(delay_steps(-1).collect::<Vec<_>>(), vec![0]);
    assert_eq!(delay_steps(-5).collect::<Vec<_>>(), vec![0]);
    assert_eq!(delay_steps(i64::MIN).collect::<Vec<_>>(), vec![0]);
    assert_eq!(delay_steps(1).collect::<Vec<_>>(), vec![1]);
    assert_eq!(delay_steps(1000).collect::<Vec<_>>(), vec![1000]);
    assert_eq!(
        delay_steps(MAX_TIMER_DELAY_MS).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS]
    );
}

#[test]
fn delay_steps_chunks_delays_above_the_settimeout_maximum() {
    assert_eq!(
        delay_steps(MAX_TIMER_DELAY_MS + 1).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS, 1]
    );
    assert_eq!(
        delay_steps(2 * MAX_TIMER_DELAY_MS).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS, MAX_TIMER_DELAY_MS]
    );
    assert_eq!(
        delay_steps(2 * MAX_TIMER_DELAY_MS + 7).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS, MAX_TIMER_DELAY_MS, 7]
    );

    let delay = 5 * MAX_TIMER_DELAY_MS + 12_345;
    let steps: Vec<i64> = delay_steps(delay).collect();
    assert_eq!(steps.len(), 6);
    assert!(steps[..5].iter().all(|step| *step == MAX_TIMER_DELAY_MS));
    assert_eq!(steps[5], 12_345);
    assert_eq!(steps.iter().sum::<i64>(), delay);
    assert!(steps.iter().all(|step| *step > 0));
    assert!(steps.iter().all(|step| *step <= MAX_TIMER_DELAY_MS));
}

// ── timers: injectable host / fake clock ───────────────────────────────────

/// One timer the fake host accepted; `fire` invokes it like the fake clock
/// reaching its due time.
struct FakeTimer {
    callback: TimerCallback,
    cancelled: Arc<AtomicBool>,
}

/// A hand-driven timer host: records every request, arms the accepted ones and
/// fires them only when the test says the clock reached the delay.
struct FakeClockInner {
    requests: Mutex<Vec<(i64, TimerOptions)>>,
    timers: Mutex<Vec<FakeTimer>>,
    disarms: AtomicUsize,
}

#[derive(Clone)]
struct FakeClock {
    inner: Arc<FakeClockInner>,
    decline: bool,
}

impl FakeClock {
    fn accepting() -> Self {
        Self::with_decline(false)
    }

    fn declining() -> Self {
        Self::with_decline(true)
    }

    fn with_decline(decline: bool) -> Self {
        Self {
            inner: Arc::new(FakeClockInner {
                requests: Mutex::new(Vec::new()),
                timers: Mutex::new(Vec::new()),
                disarms: AtomicUsize::new(0),
            }),
            decline,
        }
    }

    fn requests(&self) -> Vec<(i64, TimerOptions)> {
        self.inner.requests.lock().unwrap().clone()
    }

    fn disarm_count(&self) -> usize {
        self.inner.disarms.load(Ordering::SeqCst)
    }

    /// Move the clock to this timer's due time.
    fn fire(&self, index: usize) {
        let (callback, cancelled) = {
            let timers = self.inner.timers.lock().unwrap();
            let timer = &timers[index];
            (Arc::clone(&timer.callback), Arc::clone(&timer.cancelled))
        };
        if !cancelled.load(Ordering::SeqCst) {
            callback();
        }
    }
}

impl SetTimer for FakeClock {
    fn set(
        &self,
        callback: TimerCallback,
        delay_ms: i64,
        options: TimerOptions,
    ) -> Option<TimerCancel> {
        self.inner
            .requests
            .lock()
            .unwrap()
            .push((delay_ms, options));
        if self.decline {
            return None;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let inner = Arc::clone(&self.inner);
        self.inner.timers.lock().unwrap().push(FakeTimer {
            callback,
            cancelled,
        });
        Some(TimerCancel::new(Box::new(move || {
            inner.disarms.fetch_add(1, Ordering::SeqCst);
            flag.store(true, Ordering::SeqCst);
        })))
    }
}

fn timers_with(clock: &FakeClock, report: Arc<dyn Fn(String) + Send + Sync>) -> Timers {
    Timers::host_timers(Some(Arc::new(clock.clone()) as Arc<dyn SetTimer>), report)
}

fn no_report() -> Arc<dyn Fn(String) + Send + Sync> {
    Arc::new(|_| {})
}

type ReportSink = (Arc<Mutex<Vec<String>>>, Arc<dyn Fn(String) + Send + Sync>);

fn report_sink() -> ReportSink {
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&messages);
    let report: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |message: String| {
        sink.lock().unwrap().push(message);
    });
    (messages, report)
}

fn counting_callback() -> (Arc<AtomicUsize>, TimerCallback) {
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    let callback: TimerCallback = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    (fired, callback)
}

#[test]
fn timer_options_distinguish_foreground_from_background() {
    let options = [TimerOptions::FOREGROUND, TimerOptions::BACKGROUND];
    assert_eq!(
        options.map(|option| option.background),
        [false, true],
        "foreground timers pace work; background timers only free memory"
    );
    assert_ne!(TimerOptions::FOREGROUND, TimerOptions::BACKGROUND);
}

#[test]
fn fake_clock_fires_the_callback_at_its_due_time() {
    let clock = FakeClock::accepting();
    let timers = timers_with(&clock, no_report());
    let (fired, callback) = counting_callback();

    let handle = timers.foreground(callback, 250);
    assert!(!handle.is_cancelled());
    assert_eq!(
        clock.requests(),
        vec![(250, TimerOptions::FOREGROUND)],
        "the host sees the requested delay unchanged"
    );
    assert_eq!(fired.load(Ordering::SeqCst), 0, "nothing fires early");

    clock.fire(0);
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[test]
fn host_timers_receive_long_delays_verbatim() {
    // Chunking belongs to the native timer; a host that accepts the timer is
    // asked for the exact delay.
    let clock = FakeClock::accepting();
    let timers = timers_with(&clock, no_report());
    let handle = timers.foreground(Arc::new(|| {}), MAX_TIMER_DELAY_MS + 123);
    assert!(!handle.is_cancelled());
    assert_eq!(
        clock.requests(),
        vec![(MAX_TIMER_DELAY_MS + 123, TimerOptions::FOREGROUND)]
    );
}

#[test]
fn cancel_after_arming_prevents_the_callback_and_is_idempotent() {
    let clock = FakeClock::accepting();
    let timers = timers_with(&clock, no_report());
    let (fired, callback) = counting_callback();

    let handle = timers.foreground(callback, 100);
    handle.cancel();
    assert!(handle.is_cancelled());
    handle.cancel(); // no-op: the host disarm token runs once
    assert!(handle.is_cancelled());

    assert_eq!(clock.disarm_count(), 1);
    clock.fire(0);
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

#[test]
fn into_cancel_reaches_the_host_disarm_token_once() {
    let clock = FakeClock::accepting();
    let timers = timers_with(&clock, no_report());
    let (fired, callback) = counting_callback();

    let token = timers.foreground(callback, 100).into_cancel();
    token.cancel();

    assert_eq!(clock.disarm_count(), 1);
    clock.fire(0);
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

#[test]
fn timer_cancel_runs_its_disarm_function_once() {
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&runs);
    let token = TimerCancel::new(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    assert_eq!(format!("{token:?}"), "TimerCancel");
    token.cancel();
    // `TimerCancel::cancel` consumes the token, so a second call is a compile
    // error rather than a repeated host call.
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn declined_background_timer_returns_none_without_reporting() {
    let clock = FakeClock::declining();
    let (messages, report) = report_sink();
    let timers = timers_with(&clock, report);

    let handle = timers.background(Arc::new(|| {}), 25);
    assert!(handle.is_none());
    assert_eq!(clock.requests(), vec![(25, TimerOptions::BACKGROUND)]);
    assert!(
        messages.lock().unwrap().is_empty(),
        "only a declined foreground timer is reported"
    );
}

#[test]
fn set_timer_fn_forwards_to_the_closure() {
    let seen = Arc::new(Mutex::new(Vec::<(i64, TimerOptions)>::new()));
    let sink = Arc::clone(&seen);
    let host = SetTimerFn(
        move |_callback: TimerCallback, delay_ms: i64, options: TimerOptions| {
            sink.lock().unwrap().push((delay_ms, options));
            None
        },
    );
    let timers = Timers::new(Some(Arc::new(host) as Arc<dyn SetTimer>), no_report());

    assert!(timers.background(Arc::new(|| {}), 7).is_none());
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![(7, TimerOptions::BACKGROUND)]
    );
}

#[tokio::test]
async fn declined_foreground_timers_fall_back_to_native_and_report_once() {
    let clock = FakeClock::declining();
    let (messages, report) = report_sink();
    let timers = timers_with(&clock, report);
    let (fired, callback) = counting_callback();

    // A very long delay keeps the fallback native timers from firing during
    // the test; the runtime drops their tasks at the end.
    let first = timers.foreground(Arc::clone(&callback), 60_000);
    let second = timers.foreground(Arc::clone(&callback), 60_000);
    let clone = timers.clone();
    let third = clone.foreground(Arc::clone(&callback), 60_000);

    assert!(!first.is_cancelled());
    assert!(!second.is_cancelled());
    assert!(!third.is_cancelled());
    assert_eq!(clock.requests().len(), 3);

    let messages = messages.lock().unwrap();
    assert_eq!(messages.len(), 1, "the fallback is reported once");
    assert!(messages[0].contains("using the native timer"));
    drop(messages);
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_background_timer_is_armed() {
    // Rust has no `unref()`; the native implementation accepts background
    // timers instead of silently dropping the work.
    let timers = Timers::native(no_report());
    let (fired, callback) = counting_callback();
    let handle = timers.background(callback, 60_000);
    let handle = handle.expect("native background timer is armed");
    assert!(!handle.is_cancelled());
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_timer_fires_after_its_delay_and_only_once() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let notifier = Arc::clone(&notify);
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    let callback: TimerCallback = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        notifier.notify_one();
    });

    let handle = native_timer(callback, 5);
    assert!(!handle.is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .expect("the native timer fires within five seconds");
    assert_eq!(fired.load(Ordering::SeqCst), 1);

    // A timer is one-shot: no second callback arrives.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_a_native_timer_prevents_the_callback() {
    let (fired, callback) = counting_callback();
    let handle = native_timer(callback, 50);
    assert!(!handle.is_cancelled());
    handle.cancel();
    assert!(handle.is_cancelled());
    handle.cancel(); // idempotent

    // Wait past the original due time; a cancelled timer stays silent.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fired.load(Ordering::SeqCst), 0);
}
