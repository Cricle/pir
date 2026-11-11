//! Env-gated allocation counters for the long-session RSS investigation
//! (PIR-TUI-019): a delegating global allocator that records cumulative
//! allocated/freed bytes and the live-byte high-water mark.
//!
//! Off by default. `PIR_ALLOC_TRACE=1` turns the counters on; the TUI prompt
//! pump then appends one JSON line per run to `PIR_ALLOC_TRACE_FILE` (or
//! `pir-alloc-trace-<pid>.jsonl` in the temp directory). When disabled the
//! hooks are two relaxed atomic loads per allocation, no locks and no I/O.
//!
//! The `#[global_allocator]` registration lives in the `pir` *bin* crate
//! (`src/main_alloc.rs`, PIR-TUI-020): a library must never install a
//! process-wide allocator, or every test binary / SDK embedder that declares
//! its own allocator stops linking. The counters stay here (no unsafe code of
//! their own); only the `GlobalAlloc` impl below is unsafe.

// The `GlobalAlloc` trait methods are unsafe by contract; every body calls the
// matching `System` method with the caller's original layout/pointer.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

const STATE_UNKNOWN: u8 = 0;
const STATE_OFF: u8 = 1;
const STATE_ON: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(STATE_UNKNOWN);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);
static CURRENT: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);

pub struct TracingAllocator;

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocStats {
    pub allocated_bytes: u64,
    pub freed_bytes: u64,
    pub current_bytes: u64,
    pub peak_bytes: u64,
    pub allocations: u64,
    pub frees: u64,
}

pub fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        STATE_ON => true,
        STATE_OFF => false,
        _ => {
            // Claim the initialization before reading the environment: the
            // `var_os` call itself allocates and re-enters this function, and
            // the nested calls must observe OFF (not recurse forever).
            if STATE
                .compare_exchange(
                    STATE_UNKNOWN,
                    STATE_OFF,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                return false;
            }
            let on = std::env::var_os("PIR_ALLOC_TRACE")
                .map(|value| value != std::ffi::OsStr::new("0"))
                .unwrap_or(false);
            if on {
                STATE.store(STATE_ON, Ordering::Relaxed);
            }
            on
        }
    }
}

pub fn snapshot() -> AllocStats {
    AllocStats {
        allocated_bytes: ALLOCATED.load(Ordering::Relaxed),
        freed_bytes: FREED.load(Ordering::Relaxed),
        current_bytes: CURRENT.load(Ordering::Relaxed),
        peak_bytes: PEAK.load(Ordering::Relaxed),
        allocations: ALLOCS.load(Ordering::Relaxed),
        frees: FREES.load(Ordering::Relaxed),
    }
}

/// Start a new high-water window at the current live bytes.
pub fn reset_peak() {
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn record_alloc(size: u64) {
    ALLOCATED.fetch_add(size, Ordering::Relaxed);
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    let current = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(current, Ordering::Relaxed);
}

fn record_free(size: u64) {
    FREED.fetch_add(size, Ordering::Relaxed);
    FREES.fetch_add(1, Ordering::Relaxed);
    CURRENT.fetch_sub(size, Ordering::Relaxed);
}

/// Dangling-but-aligned pointer for a zero-sized layout, the same
/// representation `<System as GlobalAlloc>::alloc` would return for
/// `size == 0` (`std/src/alloc.rs` `0 => dangling_ptr`).
#[inline]
fn dangling_for(layout: Layout) -> *mut u8 {
    layout.align() as *mut u8
}

unsafe impl GlobalAlloc for TracingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // PIR-BIN-022: `alloc::alloc(Layout{size:0})` is a contract violation
        // that `System` now asserts on inside the core blanket GlobalAlloc
        // impl (`assert_unchecked(size != 0)` in debug, `assume` in release).
        // Callers do reach it (RustPython's `BoxVec::new(0)` for frozen
        // modules with `max_stackdepth == 0`), so return the same dangling
        // pointer `System::alloc_impl` uses for zero sizes instead of
        // forwarding an invalid layout. Zero bytes are never recorded.
        if layout.size() == 0 {
            return dangling_for(layout);
        }
        let ptr = System.alloc(layout);
        if !ptr.is_null() && enabled() {
            record_alloc(layout.size() as u64);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Same zero-size guard as `alloc`: zeroed bytes need no `System` call.
        if layout.size() == 0 {
            return dangling_for(layout);
        }
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() && enabled() {
            record_alloc(layout.size() as u64);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // PIR-BIN-022: `realloc` safety requires `layout.size() != 0`, and the
        // `new_size == 0` direction does not exist in the `GlobalAlloc`
        // contract either. Handle both without touching `System`: a previous
        // zero-sized block is one of our dangling pointers (nothing to free),
        // and a zero new size frees the old block and returns a dangling one.
        if layout.size() == 0 {
            if new_size == 0 {
                return dangling_for(layout);
            }
            return self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
        }
        if new_size == 0 {
            self.dealloc(ptr, layout);
            return dangling_for(layout);
        }
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() && enabled() {
            // The old block is consumed by realloc: count the delta live.
            let old = layout.size() as u64;
            let new = new_size as u64;
            ALLOCATED.fetch_add(new, Ordering::Relaxed);
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            FREED.fetch_add(old, Ordering::Relaxed);
            let current = CURRENT.load(Ordering::Relaxed);
            let adjusted = current.saturating_sub(old).saturating_add(new);
            CURRENT.store(adjusted, Ordering::Relaxed);
            PEAK.fetch_max(adjusted, Ordering::Relaxed);
        }
        new_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // PIR-BIN-022: the matching free of a zero-sized allocation. `ptr` is
        // a dangling pointer that was never handed to `System`; forwarding
        // the zero-size layout would hit the same `assert_unchecked`.
        if layout.size() == 0 {
            return;
        }
        System.dealloc(ptr, layout);
        if enabled() {
            record_free(layout.size() as u64);
        }
    }
}

/// Append one per-run sample. Best effort: a missing/unwritable trace file
/// must never disturb the TUI.
pub fn write_sample(label: &str, before: AllocStats, after: AllocStats) {
    use std::io::Write;
    let path = std::env::var_os("PIR_ALLOC_TRACE_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("pir-alloc-trace-{}.jsonl", std::process::id()))
        });
    let peak_window = after.peak_bytes.saturating_sub(before.current_bytes);
    let line = format!(
        "{{\"label\":\"{label}\",\"allocated_bytes\":{},\"freed_bytes\":{},\"allocations\":{},\"frees\":{},\"live_start\":{},\"live_end\":{},\"peak_live\":{},\"window_peak_delta\":{}}}\n",
        after.allocated_bytes.saturating_sub(before.allocated_bytes),
        after.freed_bytes.saturating_sub(before.freed_bytes),
        after.allocations.saturating_sub(before.allocations),
        after.frees.saturating_sub(before.frees),
        before.current_bytes,
        after.current_bytes,
        after.peak_bytes,
        peak_window,
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}
