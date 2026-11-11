//! Whole-history transcript projection A/B probe (PROTOCOL v5).
//!
//! Opens a real session JSONL (e.g. the 300-round live-tail soak file) and runs
//! two whole-history message paths over the SAME resident entries:
//!
//! - `projection`: `build_session_projection()` — the R52 `transcript()` shape:
//!   one flat `Vec<Message>` PLUS one `ProjectedSessionEntry` (Arc bump +
//!   Range) per retained entry.
//! - `messages`: `build_session_messages()` — the R53 messages-only builder
//!   `transcript()`/continuation/cache-warm now use: the same walk and the one
//!   required `Vec<Message>`, no provenance metadata vec.
//!
//! It reports wall time, allocation COUNT and allocated BYTES per iteration
//! (counting global allocator) and this process's WorkingSet after each phase.
//!
//! ```text
//! cargo run --example transcript_copy_probe -- <session.jsonl> [iterations]
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pir::ai::types::Message;
use pir::session_manager::SessionManager;

struct CountingAlloc;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: probe-only counting allocator. Every method delegates to
// `std::alloc::System` unchanged; the atomics only observe the allocation
// stream. No pointer is fabricated, retained, or freed twice.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAlloc {
    #[allow(unsafe_code)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegate to the system allocator with the same layout.
        unsafe { System.alloc(layout) }
    }

    #[allow(unsafe_code)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc`/`System.realloc` above.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[allow(unsafe_code)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: delegate to the system allocator with the same arguments.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn rss_kib(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| line.contains(&pid.to_string()))?;
    let memory = line.rsplit("\",\"").next()?.trim_matches('"');
    let digits: String = memory.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn measure(label: &str, iterations: u32, mut run: impl FnMut() -> Vec<Message>) {
    // Warm-up (allocator/page caches) is excluded from the averages.
    let warm = run();
    let warm_len = warm.len();
    drop(warm);
    let count0 = ALLOC_COUNT.load(Ordering::Relaxed);
    let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
    let started = std::time::Instant::now();
    let mut len = 0;
    for _ in 0..iterations {
        len = run().len();
    }
    let elapsed = started.elapsed();
    let count = ALLOC_COUNT.load(Ordering::Relaxed) - count0;
    let bytes = ALLOC_BYTES.load(Ordering::Relaxed) - bytes0;
    println!(
        "PHASE,{label},len={warm_len},iterations={iterations},wall_ms={},alloc_count={},alloc_bytes={},per_iter_alloc_count={:.1},per_iter_alloc_bytes={:.0}",
        elapsed.as_millis(),
        count,
        bytes,
        count as f64 / f64::from(iterations),
        bytes as f64 / f64::from(iterations),
    );
    assert_eq!(len, warm_len, "message count stays stable");
}

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: transcript_copy_probe <session.jsonl> [iterations]");
        return std::process::ExitCode::FAILURE;
    };
    let iterations: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let pid = std::process::id();
    let rss0 = rss_kib(pid).unwrap_or(0);
    println!(
        "PROBE,file={},bytes={},iterations={iterations},rss_kib_start={rss0}",
        path.display(),
        std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
    );

    let session = match SessionManager::open(&path) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("open failed: {error:?}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let rss_loaded = rss_kib(pid).unwrap_or(0);
    println!(
        "RSS,kib_after_open={rss_loaded},delta_kib={}",
        rss_loaded as i64 - rss0 as i64
    );

    // Old shape (R52 transcript): full provenance projection.
    measure("projection", iterations, || {
        session
            .build_session_projection()
            .map(|p| p.messages)
            .unwrap_or_default()
    });
    let rss_projection = rss_kib(pid).unwrap_or(0);
    println!("RSS,kib_after_projection={rss_projection}");

    // New shape (R53 transcript): messages only.
    measure("messages", iterations, || session.build_session_messages());
    let rss_messages = rss_kib(pid).unwrap_or(0);
    println!("RSS,kib_after_messages={rss_messages}");
    println!(
        "RSS,projection_vs_messages_delta_kib={}",
        rss_messages as i64 - rss_projection as i64
    );

    // Interleaved order control (cache/allocator order must not decide it).
    measure("projection-again", iterations, || {
        session
            .build_session_projection()
            .map(|p| p.messages)
            .unwrap_or_default()
    });
    measure("messages-again", iterations, || {
        session.build_session_messages()
    });
    let rss_end = rss_kib(pid).unwrap_or(0);
    println!(
        "RSS,kib_end={rss_end},delta_total_kib={}",
        rss_end as i64 - rss0 as i64
    );
    std::process::ExitCode::SUCCESS
}
