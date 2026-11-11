//! Durable search memory-copy benchmark (v5, cycle68).
//!
//! Opt-in (`#[ignore]`), standalone test binary so the thread-gated counting
//! allocator never taxes other suites. The same synthetic corpus and the same
//! `rank_documents`/`tokenize` calls run before and after the tokenizer
//! rewrite; the benchmark prints allocation events/bytes, wall time and a
//! stable digest of the ranking result so both legs can be compared.
//!
//! Run:
//!   cargo test --test durable_memcopy_bench -- --ignored --nocapture
//!
//! Peak RSS is sampled externally (PowerShell `WorkingSet64`) around the same
//! binary; see `.play/evidence/lane-builtin/cycle68-memcopy/`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

use pir::durable::{rank_documents, tokenize, SearchDocument};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_EVENTS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper: delegates straight to System and \
              counts per-thread while a measurement section is open"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = ALLOCATION_EVENTS.try_with(|events| events.set(events.get() + 1));
                let _ = ALLOCATED_BYTES.try_with(|bytes| {
                    bytes.set(bytes.get() + layout.size() as u64);
                });
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
    bytes: u64,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, AllocStats) {
    ALLOCATION_EVENTS.with(|events| events.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = f();
    COUNTING.with(|counting| counting.set(false));
    let stats = ALLOCATION_EVENTS.with(|events| AllocStats {
        events: events.get(),
        bytes: ALLOCATED_BYTES.with(Cell::get),
    });
    (value, stats)
}

/// [`measure`] for a future: the counting flag stays set across awaits.
async fn measure_async<T>(future: impl std::future::Future<Output = T>) -> (T, AllocStats, u128) {
    ALLOCATION_EVENTS.with(|events| events.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let started = Instant::now();
    let value = future.await;
    let elapsed = started.elapsed().as_millis();
    COUNTING.with(|counting| counting.set(false));
    let stats = ALLOCATION_EVENTS.with(|events| AllocStats {
        events: events.get(),
        bytes: ALLOCATED_BYTES.with(Cell::get),
    });
    (value, stats, elapsed)
}

/// Deterministic corpus: camelCase identifiers, snake_case names, stop words,
/// punctuation. 8 documents of ~384 KiB each (~3 MiB per search call).
fn corpus() -> Vec<String> {
    const WORDS: &[&str] = &[
        "readFile",
        "write_file",
        "HTTPRequest",
        "JSONParser",
        "issue",
        "issues",
        "searchIndex",
        "ToolResult",
        "cacheSize",
        "the",
        "and",
        "alpha",
        "beta",
        "parseJSON",
        "taskGraph",
        "durable",
        "checkpoint",
        "reconcile",
        "scheduler",
        "tokenize",
        "searchResults",
        "memory",
        "sessionLine",
        "items",
        "boxes",
        "queries",
    ];
    let target = 384 * 1024;
    (0..8)
        .map(|document| {
            let mut text = String::with_capacity(target + 64);
            let mut index = document;
            while text.len() < target {
                text.push_str(WORDS[index % WORDS.len()]);
                text.push(if index % 11 == 0 { ',' } else { ' ' });
                index += 1;
            }
            text
        })
        .collect()
}

fn digest(names: &[String]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for name in names {
        for byte in name.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[test]
#[ignore = "manual v5 memory-copy benchmark"]
fn search_large_corpus_memcopy() {
    let texts = corpus();
    let documents: Vec<SearchDocument<'_>> = texts
        .iter()
        .enumerate()
        .map(|(index, text)| SearchDocument {
            name: if index % 2 == 0 { "alpha" } else { "beta" },
            text: text.as_str(),
        })
        .collect();

    // Warmup so allocator state and caches are comparable between legs.
    for _ in 0..3 {
        let _ = rank_documents("readFile searchIndex issues", &documents, 10);
    }

    let iterations = 20_u32;
    let (names, stats) = measure(|| {
        let mut last = Vec::new();
        for _ in 0..iterations {
            last = rank_documents("readFile searchIndex issues", &documents, 10);
        }
        last
    });

    let (token_count, token_stats) = measure(|| tokenize(&texts[0]).len());
    let (_, tokenize_stats) = measure(|| {
        let mut total = 0_usize;
        for _ in 0..10 {
            total += tokenize(&texts[0]).len();
        }
        total
    });

    println!(
        "search_memcopy iterations={iterations} docs={} bytes_per_doc={} \
         alloc_events={} alloc_bytes={} wall_ms={} digest={:#018x} names={:?}",
        documents.len(),
        texts[0].len(),
        stats.events,
        stats.bytes,
        {
            // Re-run one timed leg so the printed wall excludes the allocation
            // section bookkeeping.
            let start = Instant::now();
            for _ in 0..iterations {
                let _ = rank_documents("readFile searchIndex issues", &documents, 10);
            }
            start.elapsed().as_millis()
        },
        digest(&names),
        names,
    );
    println!(
        "tokenize_memcopy tokens={token_count} alloc_events={} alloc_bytes={} \
         ten_calls_events={} ten_calls_bytes={}",
        token_stats.events, token_stats.bytes, tokenize_stats.events, tokenize_stats.bytes,
    );
}

// ─── Scheduler TaskRecord-copy benchmark (v5, cycle69) ─────────────────────

mod scheduler_bench {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use pir::durable::harness::scheduler::runtime::TaskRuntime;
    use pir::durable::harness::scheduler::{NextTaskState, TaskScheduler, TaskSchedulerOptions};
    use pir::durable::harness::settings::Settings;
    use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
    use pir::durable::ids::ConversationId;
    use pir::durable::session::facade::ConversationOwnership;
    use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
    use pir::durable::storage::memory::MemoryStorage;
    use pir::durable::types::{TaskRecord, TaskState};
    use pir::durable::Context;

    use super::{measure_async, AllocStats};

    const TASK_COUNT: usize = 40;
    const PHASES: usize = 6;

    fn payload() -> String {
        "x".repeat(8 * 1024)
    }

    fn multi_phase_task() -> Task {
        let mut builder = define_task("bench_multi", 1, |_input| json!({ "phase": "p0" }));
        for index in 0..PHASES {
            let name: &'static str = Box::leak(format!("p{index}").into_boxed_str());
            if index + 1 == PHASES {
                builder = builder.phase(
                    name,
                    phase_handler(|_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                        let runtime = runtime.clone();
                        Box::pin(async move {
                            runtime
                                .commit(
                                    move |_tx, _current| {
                                        Box::pin(async move {
                                            Ok(Some(NextTaskState::Terminal {
                                                outcome: pir::durable::types::TaskOutcome::Completed {
                                                    result: json!({ "ok": true }),
                                                },
                                            }))
                                        })
                                    },
                                    context,
                                )
                                .await
                                .expect("terminal");
                        })
                    }),
                );
            } else {
                let next: &'static str = Box::leak(format!("p{}", index + 1).into_boxed_str());
                let blob = payload();
                builder = builder.phase(
                    name,
                    phase_handler(
                        move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                            let runtime = runtime.clone();
                            let next = next.to_owned();
                            let blob = blob.clone();
                            Box::pin(async move {
                                runtime
                                .commit(
                                    move |_tx, _current| {
                                        Box::pin(async move {
                                            Ok(Some(NextTaskState::Running {
                                                checkpoint: json!({ "phase": next, "blob": blob }),
                                            }))
                                        })
                                    },
                                    context,
                                )
                                .await
                                .expect("advance");
                            })
                        },
                    ),
                );
            }
        }
        builder.build().expect("valid task")
    }

    async fn run_once() -> (Vec<TaskRecord>, u64) {
        let storage = Arc::new(MemoryStorage::new());
        let clock = Arc::new(AtomicI64::new(1_000));
        let clock_now = clock.clone();
        let options = TaskSchedulerOptions {
            now: Some(Arc::new(move || clock_now.load(Ordering::SeqCst))),
            settings: Some(Arc::new(Settings::default)),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, storage, vec![multi_phase_task()])
            .await
            .expect("open scheduler");
        let conversation: ConversationId = scheduler
            .session()
            .create_conversation(
                ConversationOwnership::Ownerless,
                None,
                Context::background(),
            )
            .await
            .expect("conversation")
            .id;
        let catalogue = scheduler.catalogue().snapshot();
        let definition = catalogue.task("bench_multi").expect("definition");
        let mut ids = Vec::with_capacity(TASK_COUNT);
        for index in 0..TASK_COUNT {
            let id = scheduler
                .create_task(
                    &definition,
                    json!({ "n": index, "inputBlob": payload() }),
                    TaskOptions {
                        trace: None,
                        conversation_id: Some(conversation),
                        ownership: TaskOwnership::Conversation,
                        background: false,
                        abandon_on_restart: false,
                        submission_ids: Vec::new(),
                    },
                )
                .await
                .expect("create task");
            ids.push(id);
        }
        scheduler.resume();
        let mut records = Vec::with_capacity(TASK_COUNT);
        for id in ids {
            let record = tokio::time::timeout(Duration::from_secs(60), scheduler.wait_for_task(id))
                .await
                .expect("task settled")
                .expect("wait");
            records.push(record);
        }
        scheduler.join().await;
        let mut digest = 0xcbf2_9ce4_8422_2325_u64;
        for record in &records {
            digest ^= u64::from(matches!(record.state, TaskState::Terminal { .. }));
            digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
            digest ^= record.version as u64;
        }
        (records, digest)
    }

    #[test]
    #[ignore = "manual v5 scheduler memory-copy benchmark"]
    fn scheduler_multi_task_phases_memcopy() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        // Warmup, then the measured leg on a fresh scheduler.
        let _warm = runtime.block_on(measure_async(run_once()));
        let (value, stats, elapsed): ((Vec<TaskRecord>, u64), AllocStats, u128) =
            runtime.block_on(measure_async(run_once()));
        let records = value.0;
        let digest = value.1;
        assert_eq!(records.len(), TASK_COUNT);
        assert!(records
            .iter()
            .all(|r| matches!(r.state, TaskState::Terminal { .. })));
        println!(
            "scheduler-bench tasks={TASK_COUNT} phases={PHASES} commits={} alloc_events={} alloc_bytes={} digest=0x{digest:016x} elapsed_ms={}",
            TASK_COUNT * PHASES,
            stats.events,
            stats.bytes,
            elapsed
        );
    }
}

// ─── Overlay staged-record view (v5, cycle70) ──────────────────────────────

mod overlay_bench {
    use std::collections::HashMap;
    use std::hint::black_box;

    use serde_json::json;

    use pir::durable::ids::{ConversationId, TaskId};
    use pir::durable::session::transaction::{Transaction, TransactionScope};
    use pir::durable::types::{TaskRecord, TaskState};
    use pir::durable::Context;

    use super::{measure, AllocStats};

    const RECORDS: usize = 32;
    const COMMITS: usize = 200;

    fn staged_transaction() -> Transaction {
        let mut tx = Transaction::new(Context::background(), TransactionScope::default());
        let blob = "x".repeat(8 * 1024);
        for index in 0..RECORDS {
            let record = TaskRecord {
                id: TaskId::from_raw(index as i64 + 1),
                conversation_id: ConversationId::from_raw(1),
                kind: "bench".to_owned(),
                version: 1,
                input: json!({ "blob": blob }),
                owner: None,
                background: false,
                abort_requested: false,
                abort_reason: None,
                abandon_on_restart: None,
                started_at: None,
                ended_at: None,
                restarts: None,
                state: TaskState::Running {
                    checkpoint: json!({ "phase": "p1", "blob": blob }),
                },
                memos: None,
                trace: None,
                links: Vec::new(),
            };
            tx.stage_task(record, false);
        }
        tx
    }

    /// What `Overlay::of` did before cycle70: one owned `TaskRecord` per
    /// staged record.
    fn old_overlay(tx: &Transaction) -> HashMap<TaskId, TaskRecord> {
        let mut tasks = HashMap::with_capacity(RECORDS);
        for record in tx.staged_tasks() {
            tasks.insert(record.id, record.clone());
        }
        tasks
    }

    /// What `Overlay::of` does now: borrowed staged records.
    fn new_overlay(tx: &Transaction) -> HashMap<TaskId, &TaskRecord> {
        let mut tasks = HashMap::with_capacity(RECORDS);
        for record in tx.staged_tasks() {
            tasks.insert(record.id, record);
        }
        tasks
    }

    #[test]
    #[ignore = "manual v5 overlay memory-copy benchmark"]
    fn overlay_staged_records_memcopy() {
        let tx = staged_transaction();
        assert_eq!(tx.staged_tasks().len(), RECORDS);

        let _ = old_overlay(&tx);
        let _ = new_overlay(&tx);

        let (old_view, old_stats): (HashMap<TaskId, TaskRecord>, AllocStats) = measure(|| {
            let mut last = HashMap::new();
            for _ in 0..COMMITS {
                last = old_overlay(&tx);
                black_box(last.len());
            }
            last
        });
        let old_bytes: usize = old_view
            .values()
            .map(|record| record.input.to_string().len())
            .sum();
        black_box(old_bytes);

        let (new_view, new_stats): (HashMap<TaskId, &TaskRecord>, AllocStats) = measure(|| {
            let mut last = HashMap::new();
            for _ in 0..COMMITS {
                last = new_overlay(&tx);
                black_box(last.len());
            }
            last
        });
        assert_eq!(new_view.len(), RECORDS);
        assert_eq!(old_view.len(), RECORDS);

        println!(
            "overlay-bench records={RECORDS} commits={COMMITS} record_bytes={} \
             old_events={} old_bytes_alloc={} new_events={} new_bytes_alloc={} \
             saved_events={} saved_bytes={}",
            RECORDS * 16 * 1024,
            old_stats.events,
            old_stats.bytes,
            new_stats.events,
            new_stats.bytes,
            old_stats.events.saturating_sub(new_stats.events),
            old_stats.bytes.saturating_sub(new_stats.bytes),
        );
    }
}

// ─── v6 mmap prototype (cycle71) ────────────────────────────────────────────

mod mmap_proto {
    use super::{measure, AllocStats};
    use std::borrow::Cow;
    use std::fs::File;
    use std::io::{BufRead, BufReader, BufWriter, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

    fn fnv(bytes: &[u8], hash: &mut u64) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn fixture_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("pir-mmap-proto-{}-{nanos}", std::process::id()))
    }

    /// One JSONL line of ~512 bytes; the shape mirrors a main commit marker
    /// (the parser is not part of this benchmark, only the byte access path).
    fn write_fixture(path: &Path, lines: usize) {
        let padding = "p".repeat(420);
        let file = File::create(path).expect("create fixture");
        let mut out = BufWriter::with_capacity(64 * 1024, file);
        for number in 0..lines {
            writeln!(
                out,
                "{{\"format\":1,\"type\":\"commit\",\"seq\":{number},\"writes\":[],\"pad\":\"{padding}\"}}"
            )
            .expect("write fixture line");
        }
        out.flush().expect("flush fixture");
    }

    /// Baseline the v6 rule replaces: read the whole log into a `String`.
    fn scan_whole_file(path: &Path) -> (usize, u64) {
        let text = std::fs::read_to_string(path).expect("read whole file");
        let mut hash = OFFSET;
        let mut lines = 0;
        for line in text.split_inclusive('\n') {
            let body = line.strip_suffix('\n').unwrap_or(line);
            fnv(body.as_bytes(), &mut hash);
            lines += 1;
        }
        (lines, hash)
    }

    /// Current production reader: one reusable heap buffer.
    fn scan_stream(path: &Path) -> (usize, u64) {
        let file = File::open(path).expect("open stream");
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut buffer = Vec::new();
        let mut hash = OFFSET;
        let mut lines = 0;
        loop {
            buffer.clear();
            let count = reader.read_until(b'\n', &mut buffer).expect("read line");
            if count == 0 {
                break;
            }
            let body = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
            fnv(body, &mut hash);
            lines += 1;
        }
        (lines, hash)
    }

    /// Prototype: read-only map + borrowed offset slices, shared through
    /// `Arc<Mmap>`; the `Cow` prefix is the logical fork prefix and copies
    /// nothing.
    fn scan_mmap(path: &Path) -> (usize, u64, bool, usize) {
        let file = File::open(path).expect("open mapped file");
        #[expect(
            unsafe_code,
            reason = "benchmark-only read-only map of an immutable fixture"
        )]
        // SAFETY: the fixture is not modified while this function runs.
        let map = unsafe { memmap2::Mmap::map(&file) }.expect("map fixture");
        let map = Arc::new(map);
        // A forked view shares the parent prefix instead of copying it.
        let snapshot = Arc::clone(&map);
        let prefix: Cow<'_, [u8]> = Cow::Borrowed(&snapshot[..snapshot.len() / 2]);
        let shares_prefix = prefix.as_ptr() == map.as_ptr();
        let mut position = 0usize;
        let mut hash = OFFSET;
        let mut lines = 0usize;
        while position < snapshot.len() {
            let rest = &snapshot[position..];
            let Some(newline) = memchr::memchr(b'\n', rest) else {
                break;
            };
            fnv(&rest[..newline], &mut hash);
            position += newline + 1;
            lines += 1;
        }
        (lines, hash, shares_prefix, Arc::strong_count(&map))
    }

    fn report(leg: &str, stats: AllocStats, elapsed_us: u128, result: (usize, u64), repeat: usize) {
        println!(
            "mmap-bench leg={leg} repeats={repeat} lines={} digest={:#018x} \
             alloc_events={} alloc_bytes={} wall_us={elapsed_us}",
            result.0, result.1, stats.events, stats.bytes
        );
    }

    #[test]
    #[ignore = "benchmark: run explicitly with --ignored --nocapture"]
    fn mmap_jsonl_scan_memcopy() {
        const LINES: usize = 18_000;
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("main.jsonl");
        write_fixture(&path, LINES);
        assert!(std::fs::metadata(&path).expect("metadata").len() >= 8 * 1024 * 1024);

        let leg = std::env::var("PIR_MMAP_LEG").unwrap_or_default();
        // Longer-lived processes for the external RSS/page-fault sampler; the
        // default single pass keeps the alloc/wall numbers one workload.
        let repeat: usize = std::env::var("PIR_MMAP_REPEAT")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value: &usize| *value > 0)
            .unwrap_or(1);
        let mut expected: Option<(usize, u64)> = None;
        if leg.is_empty() || leg == "whole" {
            let started = Instant::now();
            let (result, stats) = measure(|| {
                let mut result = (0, 0);
                for _ in 0..repeat {
                    result = scan_whole_file(&path);
                }
                result
            });
            report(
                "whole",
                stats,
                started.elapsed().as_micros(),
                result,
                repeat,
            );
            expected = Some(result);
        }
        if leg.is_empty() || leg == "stream" {
            let started = Instant::now();
            let (result, stats) = measure(|| {
                let mut result = (0, 0);
                for _ in 0..repeat {
                    result = scan_stream(&path);
                }
                result
            });
            report(
                "stream",
                stats,
                started.elapsed().as_micros(),
                result,
                repeat,
            );
            if let Some(expected) = expected {
                assert_eq!(result, expected, "stream digest must match");
            }
            expected = Some(result);
        }
        if leg.is_empty() || leg == "mmap" {
            let started = Instant::now();
            let (result, stats) = measure(|| {
                let mut result = (0, 0);
                for _ in 0..repeat {
                    let (lines, digest, shares, refs) = scan_mmap(&path);
                    assert!(shares, "the Cow prefix must borrow the map, not copy it");
                    assert!(refs >= 2);
                    result = (lines, digest);
                }
                result
            });
            report("mmap", stats, started.elapsed().as_micros(), result, repeat);
            if let Some(expected) = expected {
                assert_eq!(result, expected, "mmap digest must match");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows append/remap/truncate/lock semantics of the exact primitives
    /// the storage reader uses. A map has a fixed length and is not a lock:
    /// another handle may append while it is alive, the old map cannot see the
    /// new bytes, and a reopen (remap) does.
    #[test]
    fn mmap_windows_append_remap_truncate_semantics() {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("probe.jsonl");
        std::fs::write(&path, b"{\"n\":1}\n{\"n\":2}\n").expect("write");
        let count = |map: &memmap2::Mmap| {
            map.split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .count()
        };
        let file = File::open(&path).expect("open for map");
        #[expect(unsafe_code, reason = "read-only map of a test fixture")]
        // SAFETY: read-only and not written through.
        let first = unsafe { memmap2::Mmap::map(&file) }.expect("first map");
        assert_eq!(count(&first), 2);
        // Appending while the map is alive must not be blocked on Windows.
        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append while mapped");
        append.write_all(b"{\"n\":3}\n").expect("append");
        append.flush().expect("flush");
        assert_eq!(count(&first), 2, "the old map keeps its snapshot length");
        drop(first);
        let file = File::open(&path).expect("reopen for remap");
        #[expect(unsafe_code, reason = "read-only map of a test fixture")]
        // SAFETY: read-only and not written through.
        let second = unsafe { memmap2::Mmap::map(&file) }.expect("remap");
        assert_eq!(count(&second), 3, "a remap sees the appended line");
        drop(second);
        std::fs::write(&path, b"{\"n\":1}\n").expect("truncate");
        let file = File::open(&path).expect("open after truncate");
        #[expect(unsafe_code, reason = "read-only map of a test fixture")]
        // SAFETY: read-only and not written through.
        let third = unsafe { memmap2::Mmap::map(&file) }.expect("map after truncate");
        assert_eq!(count(&third), 1);
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ─── finalize overlay finals sharing (R60, PIR-BIN residual) ────────────────

mod finalize_bench {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use pir::durable::harness::scheduler::runtime::TaskRuntime;
    use pir::durable::harness::scheduler::{NextTaskState, TaskScheduler, TaskSchedulerOptions};
    use pir::durable::harness::settings::Settings;
    use pir::durable::harness::task::{define_task, phase_handler, RunningTask, Task};
    use pir::durable::ids::ConversationId;
    use pir::durable::session::facade::ConversationOwnership;
    use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
    use pir::durable::storage::memory::MemoryStorage;
    use pir::durable::types::TaskState;
    use pir::durable::Context;

    use super::{measure_async, AllocStats};

    const PARENTS: usize = 32;
    const CHILDREN: usize = 8;
    const PAYLOAD: usize = 8 * 1024;

    fn payload() -> String {
        "x".repeat(PAYLOAD)
    }

    /// A child that commits one progress state, then terminal: the terminal
    /// commit runs `finalize` while the parent still holds as `completing`.
    fn child_task() -> Task {
        let payload = payload();
        define_task("finalize_child", 1, |_input| json!({ "phase": "hold" }))
            .phase(
                "hold",
                phase_handler(
                    move |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                        let runtime = runtime.clone();
                        let blob = payload.clone();
                        Box::pin(async move {
                            runtime
                                .commit(
                                    move |_tx, _current| {
                                        let blob = blob.clone();
                                        Box::pin(async move {
                                            Ok(Some(NextTaskState::Running {
                                                checkpoint: json!({ "held": blob }),
                                            }))
                                        })
                                    },
                                    context.clone(),
                                )
                                .await
                                .expect("child progress");
                            runtime
                                .commit(
                                    move |_tx, _current| {
                                        Box::pin(async move {
                                            Ok(Some(NextTaskState::Terminal {
                                                outcome:
                                                    pir::durable::types::TaskOutcome::Completed {
                                                        result: json!({ "child": true }),
                                                    },
                                            }))
                                        })
                                    },
                                    context,
                                )
                                .await
                                .expect("child terminal");
                        })
                    },
                ),
            )
            .build()
            .expect("valid child")
    }

    /// A parent that spawns CHILDREN, then commits terminal; the terminal
    /// holds as `completing` until every child settles, so each child's
    /// terminal commit runs the multi-round `finalize` — the path the R60
    /// clone removal targets.
    fn parent_task() -> Task {
        define_task("finalize_parent", 1, |_input| json!({ "phase": "spawn" }))
            .phase(
                "spawn",
                phase_handler(
                    |_t: &RunningTask, runtime: &TaskRuntime, context: Context| {
                        let runtime = runtime.clone();
                        Box::pin(async move {
                            let catalogue = runtime.registry();
                            for index in 0..CHILDREN {
                                runtime
                                    .create_task(
                                        &catalogue.task("finalize_child").expect("installed"),
                                        json!({ "index": index, "inputBlob": payload() }),
                                        context.clone(),
                                    )
                                    .await
                                    .expect("create child");
                            }
                            runtime
                                .commit(
                                    move |_tx, _current| {
                                        Box::pin(async move {
                                            Ok(Some(NextTaskState::Terminal {
                                                outcome:
                                                    pir::durable::types::TaskOutcome::Completed {
                                                        result: json!({ "parent": true }),
                                                    },
                                            }))
                                        })
                                    },
                                    context,
                                )
                                .await
                                .expect("parent terminal");
                        })
                    },
                ),
            )
            .build()
            .expect("valid parent")
    }

    async fn run_once() -> u64 {
        let storage = Arc::new(MemoryStorage::new());
        let options = TaskSchedulerOptions {
            settings: Some(Arc::new(Settings::default)),
            ..TaskSchedulerOptions::default()
        };
        let scheduler =
            TaskScheduler::open_with(options, storage, vec![child_task(), parent_task()])
                .await
                .expect("open scheduler");
        let conversation: ConversationId = scheduler
            .session()
            .create_conversation(
                ConversationOwnership::Ownerless,
                None,
                Context::background(),
            )
            .await
            .expect("conversation")
            .id;
        let catalogue = scheduler.catalogue().snapshot();
        let definition = catalogue.task("finalize_parent").expect("definition");
        let mut ids = Vec::with_capacity(PARENTS);
        for index in 0..PARENTS {
            let id = scheduler
                .create_task(
                    &definition,
                    json!({ "parent": index, "inputBlob": payload() }),
                    TaskOptions {
                        trace: None,
                        conversation_id: Some(conversation),
                        ownership: TaskOwnership::Conversation,
                        background: false,
                        abandon_on_restart: false,
                        submission_ids: Vec::new(),
                    },
                )
                .await
                .expect("create parent");
            ids.push(id);
        }
        scheduler.resume();
        let mut digest = 0xcbf2_9ce4_8422_2325_u64;
        for id in ids {
            let record = tokio::time::timeout(Duration::from_secs(60), scheduler.wait_for_task(id))
                .await
                .expect("parent settled")
                .expect("wait");
            digest ^= u64::from(matches!(record.state, TaskState::Terminal { .. }));
            digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
        }
        scheduler.join().await;
        digest
    }

    #[test]
    #[ignore = "manual R60 finalize memory-copy benchmark"]
    fn scheduler_finalize_memcopy() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _warm = runtime.block_on(measure_async(run_once()));
        let (digest, stats, elapsed): (u64, AllocStats, u128) =
            runtime.block_on(measure_async(run_once()));
        println!(
            "finalize-bench parents={PARENTS} children={CHILDREN} alloc_events={} alloc_bytes={} digest=0x{digest:016x} elapsed_ms={}",
            stats.events, stats.bytes, elapsed
        );
        assert_eq!(digest, 0x38df9853f4efb1c5);
    }
}
