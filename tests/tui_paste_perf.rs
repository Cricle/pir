//! TUI-P3 paste allocation bounds.
//!
//! Standalone target: one counting global allocator measures the bytes and
//! allocation events a 10k-line `Editor::paste_text` costs, so the single-pass
//! cleaner/insert path cannot regress back to multi-buffer copies unnoticed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pir::tui::editor::Editor;

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
