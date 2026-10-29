//! P1-2 regression pin: `ShellCapture::full_output` must read the spill file
//! like upstream `readFullOutput` — `stat` the file, and for an oversized
//! stream read ONLY `headBytes` at offset 0 and `tailBytes` at
//! `size - tailBytes` — instead of `std::fs::read`-ing the whole stream to
//! keep a 1 MiB window. The spill file holds the entire stream by design, so
//! the old read allocated the stream size to keep the cap.
//!
//! This standalone test binary measures with the same thread-gated counting
//! allocator `tests/backend_copy_audit.rs` uses (never the merged `g_*`
//! harnesses, whose suites must not pay for a `GlobalAlloc` wrapper):
//!
//! 1. byte identity — a stream below the 1 MiB cap (but past the 100 KiB
//!    rolling window, so it spills) comes back verbatim with
//!    `truncated: false`, and a ~16 MiB stream equals the exact
//!    head + marker + tail reconstruction, with the advertised spill file
//!    holding the complete stream;
//! 2. bounded cost — growing the stream ~40x must not grow the run's
//!    allocations by anywhere near the stream delta: the removed whole-file
//!    read was exactly that delta.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::coding_agent::register_core_tools;

// ---------------------------------------------------------------------------
// Test-only allocation counter (bytes + events, per thread)
// ---------------------------------------------------------------------------

struct CountingAllocator;

thread_local! {
    /// Whether this thread is inside a measurement section.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Allocation events seen on this thread since the section opened.
    static ALLOCATION_EVENTS: Cell<u64> = const { Cell::new(0) };
    /// Bytes requested by those allocation events.
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper: delegates straight to System and \
              adds per-thread counters; the TLS slots are const-initialized so \
              reading them can never allocate (no allocator recursion)"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = ALLOCATION_EVENTS.try_with(|events| events.set(events.get() + 1));
                let _ =
                    ALLOCATED_BYTES.try_with(|bytes| bytes.set(bytes.get() + layout.size() as u64));
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

/// Allocation cost of one measured section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AllocStats {
    events: u64,
    bytes: u64,
}

async fn measure_async<T>(future: impl Future<Output = T>) -> (T, AllocStats) {
    ALLOCATION_EVENTS.with(|events| events.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = future.await;
    COUNTING.with(|counting| counting.set(false));
    let stats = AllocStats {
        events: ALLOCATION_EVENTS.with(Cell::get),
        bytes: ALLOCATED_BYTES.with(Cell::get),
    };
    (value, stats)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `structuredContent.output` cap (upstream STRUCTURED_OUTPUT_MAX_BYTES).
const CAP: usize = 1024 * 1024;
/// Small stream: past the 2 x 50 KiB rolling window (so it spills) but below
/// the 1 MiB structured cap.
const SMALL_LINES: usize = 60_000;
/// Large stream: ~16 MiB, far past the cap (the old code read all of it).
const LARGE_LINES: usize = 2_000_000;

fn temp_workspace() -> PathBuf {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pir-bash-full-output-{nonce}-{sequence}-{}",
        std::process::id()
    ))
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

async fn run_bash(registry: &ToolRegistry, lines: usize, root: &std::path::Path) -> ToolResult {
    let context = ToolContext::new(root.to_path_buf());
    registry
        .get("bash")
        .expect("bash tool should be registered")
        .run(
            &serde_json::json!({ "command": format!("seq 1 {lines}") }),
            &context,
        )
        .await
}

/// The exact bytes `seq 1 <lines>` writes to stdout.
fn expected_output(lines: usize) -> String {
    let mut out = String::new();
    for index in 1..=lines {
        out.push_str(&index.to_string());
        out.push('\n');
    }
    out
}

/// Skip guard for hosts without a resolvable bash.
fn bash_available() -> bool {
    pir::coding_agent::tools::resolve_shell_config(None).is_ok()
}

#[tokio::test(flavor = "current_thread")]
async fn full_output_reads_are_bounded_by_the_window_not_the_spill_size() {
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();

    let expected_small = expected_output(SMALL_LINES);
    let expected_large = expected_output(LARGE_LINES);
    assert!(
        expected_small.len() > 2 * pir::coding_agent::truncate::DEFAULT_MAX_BYTES,
        "the small fixture must evict past the rolling window and spill"
    );
    assert!(
        expected_small.len() < CAP,
        "the small fixture must stay below the structured cap"
    );
    assert!(
        expected_large.len() > 8 * CAP,
        "the large fixture must dwarf the 1 MiB view, got {}",
        expected_large.len()
    );

    // Small leg: whole output fits the cap; it is returned verbatim.
    let (small_result, small_stats) = measure_async(run_bash(&registry, SMALL_LINES, &root)).await;
    assert!(!small_result.is_error, "{}", small_result.content);
    let small_structured = small_result.structured_content.expect("structured content");
    assert_eq!(
        small_structured["truncated"],
        serde_json::json!(false),
        "below the cap nothing is omitted"
    );
    assert_eq!(
        small_structured["output"].as_str(),
        Some(expected_small.as_str()),
        "below the cap the full output is returned verbatim"
    );

    // Large leg: exact upstream head + marker + tail view.
    let (large_result, large_stats) = measure_async(run_bash(&registry, LARGE_LINES, &root)).await;
    assert!(!large_result.is_error, "{}", large_result.content);
    let large_structured = large_result.structured_content.expect("structured content");
    assert_eq!(large_structured["truncated"], serde_json::json!(true));
    let expected_view = format!(
        "{}\n\n[... {} bytes omitted ...]\n\n{}",
        &expected_large[..CAP / 2],
        expected_large.len() - CAP,
        &expected_large[expected_large.len() - CAP / 2..]
    );
    assert_eq!(
        large_structured["output"].as_str(),
        Some(expected_view.as_str()),
        "the bounded head/tail read must reproduce the whole-file view exactly"
    );
    let path = large_structured["full_output_path"]
        .as_str()
        .expect("full output path");
    assert_eq!(
        std::fs::metadata(path).expect("spill metadata").len() as usize,
        expected_large.len(),
        "the advertised spill file holds the complete stream"
    );

    // Bounded cost: the ~16 MiB stream may not grow the run's allocations by
    // anywhere near the stream delta. Under the old whole-file read the large
    // leg paid the full stream size (>= 15 MiB above the small leg); the
    // bounded read pays the 1 MiB view plus its buffers. The threshold is the
    // removed read's own scale (half the stream), so the assertion is derived
    // from the mechanism, not from a recorded run.
    let stream_delta = (expected_large.len() - expected_small.len()) as u64;
    assert!(
        large_stats.bytes <= small_stats.bytes + stream_delta / 2,
        "large run allocated {:?} vs small {:?} for a stream delta of {stream_delta} bytes: \
         full_output must read head/tail slices, not the whole spill file",
        large_stats,
        small_stats
    );
    // Sanity: the measurement is live (the large leg must at least build the
    // 1 MiB view).
    assert!(
        large_stats.bytes >= CAP as u64 / 2,
        "the large leg's measurement looks dead: {:?}",
        large_stats
    );

    let _ = std::fs::remove_dir_all(&root);
}
