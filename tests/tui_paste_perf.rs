//! TUI-P3 paste allocation bounds.
//!
//! Standalone target: one counting global allocator measures the bytes and
//! allocation events a 10k-line `Editor::paste_text` costs, so the single-pass
//! cleaner/insert path cannot regress back to multi-buffer copies unnoticed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use pir::tui::core::Component;
use pir::tui::editor::Editor;
use pir::tui::markdown::Markdown;
use pir::tui::messages::UserMessageComponent;
use pir::tui::strip_ansi;

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static EVENTS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<i64> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper delegating to System; counters are \
              const-initialized TLS cells so reads cannot recurse into alloc"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = EVENTS.try_with(|events| events.set(events.get() + 1));
                let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + layout.size() as i64));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Debug, Clone, Copy)]
struct AllocStats {
    events: u64,
    bytes: i64,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocStats) {
    EVENTS.with(|events| events.set(0));
    BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = f();
    COUNTING.with(|counting| counting.set(false));
    (
        value,
        AllocStats {
            events: EVENTS.with(Cell::get),
            bytes: BYTES.with(Cell::get),
        },
    )
}

fn ten_thousand_lines(separator: &str, line: impl Fn(usize) -> String) -> String {
    (0..10_000).map(line).collect::<Vec<_>>().join(separator)
}

/// Plain 10k-line paste: the cleaner borrows (zero cleaning allocation), so
/// the retained registry copy plus the returned String are the only full-size
/// buffers. One undo snapshot; small fixed allocation count.
#[test]
fn ten_thousand_line_paste_allocates_at_most_two_copies_plus_fixed_overhead() {
    let editor = Editor::new();
    let input = ten_thousand_lines("\n", |index| format!("line {index:05} payload"));
    let input_len = input.len();

    let (inserted, stats) = measure(|| editor.paste_text(&input));
    assert_eq!(inserted, input);
    assert_eq!(editor.expanded_text(), input);
    assert_eq!(editor.undo_depth(), 1);

    assert!(
        stats.bytes <= (input_len * 2 + 64 * 1024) as i64,
        "plain 10k paste allocated {} bytes for a {} byte payload ({} events)",
        stats.bytes,
        input_len,
        stats.events
    );
    assert!(
        stats.events <= 32,
        "plain 10k paste caused {} allocation events (expected a small fixed \
         count, sizes bounded by 2x + 64KiB)",
        stats.events
    );
}

/// CRLF + tab normalization necessarily builds one cleaning buffer; even then
/// the total stays within four expected-output copies plus fixed overhead and
/// a small allocation-event count.
#[test]
fn ten_thousand_line_crlf_tab_paste_stays_within_four_output_copies() {
    let editor = Editor::new();
    let input = ten_thousand_lines("\r\n", |index| format!("row {index:05}\tvalue"));
    let expected = ten_thousand_lines("\n", |index| format!("row {index:05}    value"));
    let expected_len = expected.len();

    let (inserted, stats) = measure(|| editor.paste_text(&input));
    assert_eq!(inserted, expected);
    assert_eq!(editor.expanded_text(), expected);

    assert!(
        stats.bytes <= (expected_len * 4 + 64 * 1024) as i64,
        "normalizing 10k paste allocated {} bytes for a {} byte payload ({} events)",
        stats.bytes,
        expected_len,
        stats.events
    );
    assert!(
        stats.events <= 32,
        "normalizing 10k paste caused {} allocation events",
        stats.events
    );
}

// ---------------------------------------------------------------------------
// Single-line paste submit render bound (A20-F01)
// ---------------------------------------------------------------------------

/// A 64 KiB bracketed paste with no newline folds to one `[paste #N …]`
/// marker and expands to a single 64 KiB line on submit. The markdown inline
/// pass used to rescan the rest of an alphanumeric run from every character
/// (looking for a bare-email `@`), i.e. O(n²): QA measured 13.7 s to paint
/// the reply at 64 KiB and >60 s at 1 MiB. The user-message render must stay
/// bounded and byte-complete. (The companion user-message component renders
/// this shape without Markdown, but the markdown path is exercised directly
/// so the inline scanner cannot regress unnoticed.)
#[test]
fn sixty_four_kib_single_line_markdown_renders_in_bounded_time() {
    let payload = "Q".repeat(64 * 1024);
    let started = Instant::now();
    let lines = Markdown::new(payload.clone(), 0, 0).render(100);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "64 KiB single-line markdown took {elapsed:?} (expected well under 10 s)"
    );
    let text: String = lines.iter().map(|line| line.trim_end()).collect();
    assert_eq!(text, payload);
}

/// The same shape at 1 MiB: with the linear scanner this is a few hundred
/// milliseconds in a debug build; the old quadratic scan is effectively
/// unbounded (hours, not seconds).
#[test]
fn one_mib_single_line_markdown_renders_in_bounded_time() {
    let payload = "Q".repeat(1024 * 1024);
    let started = Instant::now();
    let lines = Markdown::new(payload.clone(), 0, 0).render(100);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "1 MiB single-line markdown took {elapsed:?} (expected well under 30 s)"
    );
    let text: String = lines.iter().map(|line| line.trim_end()).collect();
    assert_eq!(text, payload);
}

/// End-to-end composer path for the QA repro: paste 64 KiB as one line, submit
/// (marker expansion), then render the optimistic user bubble. Bounded and
/// content-complete.
#[test]
fn sixty_four_kib_single_line_paste_submit_renders_in_bounded_time() {
    let payload = "Q".repeat(64 * 1024);
    let editor = Editor::new();
    editor.paste_text(&payload);
    assert!(editor.get_text().contains("[paste #"));
    let submitted = editor.submit().expect("non-empty submit");
    assert_eq!(submitted, payload);

    let started = Instant::now();
    let component = UserMessageComponent::new(&submitted, 1);
    let lines = component.render(100);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "64 KiB single-line user-message render took {elapsed:?} (expected well under 10 s)"
    );
    let painted: String = lines.iter().map(|line| strip_ansi(line)).collect();
    assert_eq!(painted.matches('Q').count(), payload.len());
}
