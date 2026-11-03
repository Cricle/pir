#![allow(unsafe_code)] // test-only counting allocator; every call forwards to System

//! Memory probe for tail-first session open.
//!
//! Compares `SessionManager::open_paged` at 10 MB and 100 MB. The paged open
//! must parse only the capped tail window, so its heap peak must stay flat
//! while the file grows 10x; a full open of the same files is measured as the
//! control (its peak tracks the transcript size).
//!
//! The counter is a test-local global allocator. It measures HEAP only;
//! mmap-backed pages are not heap and are not counted (the point of the
//! exercise: the tail page is parsed and its mapping is dropped before the
//! manager is returned). Run this binary with `--test-threads=1` for the
//! quietest numbers; the single test also holds a lock so a future addition
//! cannot overlap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pir::session_manager::{SessionManager, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES};

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn note_allocation(size: usize) {
    let current = CURRENT.fetch_add(size, Ordering::SeqCst) + size;
    PEAK.fetch_max(current, Ordering::SeqCst);
}

// SAFETY: every method forwards to `System`; the counters are plain atomics
// around allocations that `System` actually served.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            note_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

static MEASURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug)]
struct Measurement {
    file_bytes: u64,
    heap_peak_delta: u64,
    heap_live_after_open: u64,
    elapsed: Duration,
    resident_entries: usize,
    parsed_tail_entries: usize,
    page_bytes: u64,
    skipped_bytes: u64,
}

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-lazy-history-mem-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Write a JSONL session of at least `target_bytes`, returning its real size.
fn write_fixture(path: &Path, target_bytes: u64) -> u64 {
    const PAYLOAD: usize = 4096;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create session"));
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "memory-fixture",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "C:\\\\repo",
        })
    )
    .expect("header");
    let filler = "m".repeat(PAYLOAD);
    let mut written = 0u64;
    let mut index = 0usize;
    while written < target_bytes {
        let id = format!("m{index:07}");
        let line = serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": if index == 0 { None } else { Some(format!("m{:07}", index - 1)) },
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "user",
                "timestamp": 0,
                "content": [{"type": "text", "text": format!("turn {index} {filler}")}],
            },
        });
        let encoded = line.to_string();
        file.write_all(encoded.as_bytes()).expect("line");
        file.write_all(b"\n").expect("newline");
        written += encoded.len() as u64 + 1;
        index += 1;
    }
    file.flush().expect("flush");
    std::fs::metadata(path).expect("metadata").len()
}

fn reset_peak() -> u64 {
    let baseline = CURRENT.load(Ordering::SeqCst) as u64;
    PEAK.store(baseline as usize, Ordering::SeqCst);
    baseline
}

fn measure_paged(path: &Path) -> Measurement {
    let baseline = reset_peak();
    let start = Instant::now();
    let manager =
        SessionManager::open_paged(path, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES)
            .expect("paged open");
    let elapsed = start.elapsed();
    let peak = PEAK.load(Ordering::SeqCst) as u64;
    let live = CURRENT.load(Ordering::SeqCst) as u64;
    assert!(manager.is_paged(), "fixture must exceed the default caps");
    let window = manager.paged_window().expect("paged window");
    let file_bytes = std::fs::metadata(path).expect("metadata").len();
    let measurement = Measurement {
        file_bytes,
        heap_peak_delta: peak.saturating_sub(baseline),
        heap_live_after_open: live.saturating_sub(baseline),
        elapsed,
        resident_entries: manager.entries().len(),
        parsed_tail_entries: window.parsed_entries,
        page_bytes: file_bytes.saturating_sub(window.page_start),
        skipped_bytes: window.page_start,
    };
    drop(manager);
    measurement
}

fn measure_full(path: &Path) -> Measurement {
    let baseline = reset_peak();
    let start = Instant::now();
    let manager = SessionManager::open(path).expect("full open");
    let elapsed = start.elapsed();
    let peak = PEAK.load(Ordering::SeqCst) as u64;
    let live = CURRENT.load(Ordering::SeqCst) as u64;
    let file_bytes = std::fs::metadata(path).expect("metadata").len();
    let measurement = Measurement {
        file_bytes,
        heap_peak_delta: peak.saturating_sub(baseline),
        heap_live_after_open: live.saturating_sub(baseline),
        elapsed,
        resident_entries: manager.entries().len(),
        parsed_tail_entries: manager.entries().len(),
        page_bytes: file_bytes,
        skipped_bytes: 0,
    };
    drop(manager);
    measurement
}

#[test]
fn paged_open_heap_memory_is_flat_from_10mb_to_100mb() {
    let _guard = MEASURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_dir("probe");
    let small_path = dir.join("small.jsonl");
    let big_path = dir.join("big.jsonl");
    let small_bytes = write_fixture(&small_path, 10 * 1024 * 1024);
    let big_bytes = write_fixture(&big_path, 100 * 1024 * 1024);
    assert!(small_bytes >= 10 * 1024 * 1024);
    assert!(big_bytes >= 100 * 1024 * 1024);

    // Warm the reader paths once; the probe measures the actual open.
    let _ = SessionTailPageWarmup::open(&small_path);
    let _ = SessionTailPageWarmup::open(&big_path);

    let small_paged = measure_paged(&small_path);
    let big_paged = measure_paged(&big_path);
    let small_full = measure_full(&small_path);
    let big_full = measure_full(&big_path);

    println!(
        "[lazy-history-mem] paged 10MB  file={} page={} skipped={} peak_delta={} live_after={} elapsed={:?} resident={} tail={}",
        small_paged.file_bytes,
        small_paged.page_bytes,
        small_paged.skipped_bytes,
        small_paged.heap_peak_delta,
        small_paged.heap_live_after_open,
        small_paged.elapsed,
        small_paged.resident_entries,
        small_paged.parsed_tail_entries
    );
    println!(
        "[lazy-history-mem] paged 100MB file={} page={} skipped={} peak_delta={} live_after={} elapsed={:?} resident={} tail={}",
        big_paged.file_bytes,
        big_paged.page_bytes,
        big_paged.skipped_bytes,
        big_paged.heap_peak_delta,
        big_paged.heap_live_after_open,
        big_paged.elapsed,
        big_paged.resident_entries,
        big_paged.parsed_tail_entries
    );
    println!(
        "[lazy-history-mem] full  10MB  file={} peak_delta={} live_after={} elapsed={:?}",
        small_full.file_bytes,
        small_full.heap_peak_delta,
        small_full.heap_live_after_open,
        small_full.elapsed
    );
    println!(
        "[lazy-history-mem] full  100MB file={} peak_delta={} live_after={} elapsed={:?}",
        big_full.file_bytes,
        big_full.heap_peak_delta,
        big_full.heap_live_after_open,
        big_full.elapsed
    );

    assert!(
        small_paged.parsed_tail_entries <= DEFAULT_TAIL_PAGE_ENTRIES,
        "paged open parses only the entry-capped tail: {small_paged:?}"
    );
    assert!(
        big_paged.parsed_tail_entries <= DEFAULT_TAIL_PAGE_ENTRIES,
        "paged open parses only the entry-capped tail: {big_paged:?}"
    );
    // The decisive shape: 10x the file must not cost 10x the heap. Allow a
    // small fixed slack for allocator bookkeeping and the 100 MB fixture's
    // slightly longer live strings in the shared tail page.
    assert!(
        big_paged.heap_peak_delta <= small_paged.heap_peak_delta + 4 * 1024 * 1024,
        "paged open must not scale with file size: small={} big={}",
        small_paged.heap_peak_delta,
        big_paged.heap_peak_delta
    );
    assert!(
        big_paged.heap_peak_delta < big_bytes / 8,
        "paged open of a 100 MB session must stay far below 12.5 MB: {}",
        big_paged.heap_peak_delta
    );
    // Control: the full loader is the one that pays for the whole transcript.
    assert!(
        big_full.heap_peak_delta > small_full.heap_peak_delta * 4,
        "full open should scale with the file (control): small={} big={}",
        small_full.heap_peak_delta,
        big_full.heap_peak_delta
    );
    assert!(
        big_paged.heap_peak_delta < big_full.heap_peak_delta / 5,
        "paged open must be at least 5x smaller than the full open: paged={} full={}",
        big_paged.heap_peak_delta,
        big_full.heap_peak_delta
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Tiny helper that opens a raw tail page to warm the mmap/parser code path
/// (and to assert it does not hold a map alive after return).
struct SessionTailPageWarmup;

impl SessionTailPageWarmup {
    fn open(path: &Path) -> Option<pir::session_manager::SessionTailPage> {
        pir::session_manager::SessionTailPage::open(
            path,
            DEFAULT_TAIL_PAGE_BYTES,
            DEFAULT_TAIL_PAGE_ENTRIES,
        )
        .expect("warmup page")
    }
}
