//! Large-history whole-transcript read probe (R54, PROTOCOL v5).
//!
//! Builds a persistent session with N messages (default 8000; file kept under
//! `.play/evidence/...` or a temp dir) and measures the three public
//! whole-history reads on a fresh process:
//!
//! - `transcript()`         (messages-only projection; first call widens)
//! - `transcript_json()`    (JSON view, serializes stored records by reference)
//! - `full_messages_json()` (complete `session.messages` view, metadata only)
//!
//! For each phase it reports wall time, allocation count/bytes (counting
//! global allocator) and process memory: Private Bytes / Working Set / page
//! faults (Win32_PerfRawData_PerfProc_Process), so the owned `Vec<Message>` /
//! `Vec<JsonValue>` API cost per message is visible.
//!
//! ```text
//! cargo run --example transcript_large_probe -- make <dir> [messages]
//! cargo run --example transcript_large_probe -- read <session.jsonl> [iterations]
//! ```

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ThinkingLevel, Usage,
    UserMessage, UserRole,
};
use pir::sdk::{create_agent_session, AgentSession, CreateAgentSessionOptions, ModelRegistry};
use pir::session_manager::SessionManager;
use serde_json::Value as JsonValue;

struct CountingAlloc;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: probe-only counting allocator; every method delegates to
// `std::alloc::System` unchanged. The atomics only observe the stream.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAlloc {
    #[allow(unsafe_code)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegate with the same layout.
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
        // SAFETY: delegate with the same arguments.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[derive(Clone, Copy, Debug, Default)]
struct ProcMem {
    private_bytes: u64,
    working_set: u64,
    page_faults: u64,
}

fn proc_mem() -> ProcMem {
    let pid = std::process::id();
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\"; \
         if ($p) {{ '{{0}},{{1}},{{2}}' -f $p.PrivatePageCount,$p.WorkingSetSize,$p.PageFaults }}"
    );
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    let Ok(output) = output else {
        return ProcMem::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let values: Vec<u64> = text
        .trim()
        .split(',')
        .filter_map(|value| value.trim().parse().ok())
        .collect();
    if values.len() == 3 {
        ProcMem {
            private_bytes: values[0],
            working_set: values[1],
            page_faults: values[2],
        }
    } else {
        ProcMem::default()
    }
}

fn print_mem(label: &str, before: ProcMem, after: ProcMem) {
    println!(
        "MEM,{label},private_bytes={},private_delta_bytes={},working_set_bytes={},\
         working_set_delta_bytes={},page_faults={},page_fault_delta={}",
        after.private_bytes,
        after.private_bytes as i64 - before.private_bytes as i64,
        after.working_set,
        after.working_set as i64 - before.working_set as i64,
        after.page_faults,
        after.page_faults.saturating_sub(before.page_faults),
    );
}

#[derive(Clone, Copy)]
enum ReadKind {
    Transcript,
    TranscriptJson,
    FullMessagesJson,
}

async fn run_read(agent: &AgentSession, kind: ReadKind) -> usize {
    match kind {
        ReadKind::Transcript => agent.transcript().await.len(),
        ReadKind::TranscriptJson => agent.transcript_json().await.len(),
        ReadKind::FullMessagesJson => agent.full_messages_json().await.len(),
    }
}

async fn measure(agent: &AgentSession, label: &str, kind: ReadKind, iterations: u32) {
    // Warm-up excluded from the averages (allocator/page caches).
    let warm = run_read(agent, kind).await;
    let count0 = ALLOC_COUNT.load(Ordering::Relaxed);
    let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
    let mem0 = proc_mem();
    let started = std::time::Instant::now();
    let mut len = 0;
    for _ in 0..iterations {
        len = run_read(agent, kind).await;
    }
    let elapsed = started.elapsed();
    let mem1 = proc_mem();
    let count = ALLOC_COUNT.load(Ordering::Relaxed) - count0;
    let bytes = ALLOC_BYTES.load(Ordering::Relaxed) - bytes0;
    println!(
        "PHASE,{label},len={warm},iterations={iterations},wall_ms={},per_call_ms={:.2},\
         alloc_count={},alloc_bytes={},per_call_alloc_count={:.1},per_call_alloc_bytes={:.0}",
        elapsed.as_millis(),
        elapsed.as_micros() as f64 / f64::from(iterations) / 1000.0,
        count,
        bytes,
        count as f64 / f64::from(iterations),
        bytes as f64 / f64::from(iterations),
    );
    print_mem(label, mem0, mem1);
    assert_eq!(len, warm, "message count stays stable");
}

fn user_message(text: String) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn assistant_message(text: String) -> Message {
    Message::Assistant(AssistantMessage {
        api: "mock-api".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        usage: Usage::default(),
        ..Default::default()
    })
}

fn make_session(dir: &Path, messages: usize) -> pir::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut session = SessionManager::create("C:/probe-repo", dir)?;
    let payload = "lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(4);
    for index in 0..messages {
        let message = if index % 2 == 0 {
            user_message(format!("user {index}: {payload}"))
        } else {
            assistant_message(format!("assistant {index}: {payload}"))
        };
        session.append_message(message)?;
    }
    let path = session.path().to_path_buf();
    drop(session);
    Ok(path)
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    match mode {
        "make" => {
            let dir = PathBuf::from(args.get(1).ok_or("usage: make <dir> [messages]")?);
            let messages: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(8000);
            let path = make_session(&dir, messages)?;
            println!(
                "MADE,file={},bytes={},messages={}",
                path.display(),
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                messages
            );
            Ok(())
        }
        "read" => {
            let path = PathBuf::from(args.get(1).ok_or("usage: read <session.jsonl> [iters]")?);
            let iterations: u32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(3);
            let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!(
                "PROBE,file={},bytes={file_bytes},iterations={iterations},pid={}",
                path.display(),
                std::process::id()
            );
            let mem_start = proc_mem();
            let (registry, model) = common::ScriptedProvider::registry(Vec::new());
            let agent_dir = path
                .parent()
                .map(|parent| parent.join("probe-agent"))
                .unwrap_or_else(|| PathBuf::from("probe-agent"));
            std::fs::create_dir_all(&agent_dir)?;
            let manager = SessionManager::open(&path)?;
            let created = create_agent_session(CreateAgentSessionOptions {
                cwd: Some(PathBuf::from("C:/probe-repo")),
                agent_dir: Some(agent_dir),
                model_registry: Some(ModelRegistry::from_registry(Arc::clone(&registry))),
                model: Some(model),
                thinking_level: Some(ThinkingLevel::Off),
                session: Some(manager),
                no_tools: true,
                system_prompt: Some("You are a large-transcript read probe.".to_owned()),
                discover_system_prompt: Some(false),
                compaction: Some(pir::compaction::CompactionConfig {
                    enabled: false,
                    reserve_tokens: 16_384,
                    keep_recent_tokens: 1,
                }),
                ..Default::default()
            })
            .await?;
            let agent = created.session;
            let mem_open = proc_mem();
            print_mem("after_build", mem_start, mem_open);

            // First whole-history read: includes the paged widen.
            let count0 = ALLOC_COUNT.load(Ordering::Relaxed);
            let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
            let mem_first0 = proc_mem();
            let started = std::time::Instant::now();
            let first_len = agent.transcript().await.len();
            let first_wall = started.elapsed();
            let mem_first1 = proc_mem();
            println!(
                "PHASE,transcript-first,len={first_len},wall_ms={},alloc_count={},alloc_bytes={}",
                first_wall.as_millis(),
                ALLOC_COUNT.load(Ordering::Relaxed) - count0,
                ALLOC_BYTES.load(Ordering::Relaxed) - bytes0,
            );
            print_mem("transcript-first", mem_first0, mem_first1);

            measure(&agent, "transcript", ReadKind::Transcript, iterations).await;
            measure(
                &agent,
                "transcript_json",
                ReadKind::TranscriptJson,
                iterations,
            )
            .await;
            measure(
                &agent,
                "full_messages_json",
                ReadKind::FullMessagesJson,
                iterations,
            )
            .await;

            let mem_end = proc_mem();
            print_mem("process_total", mem_start, mem_end);
            Ok(())
        }
        // PIR-SDK-027 A/B: the per-prompt session-context snapshot push on a
        // large session. Each iteration appends one entry (the fingerprint
        // misses, as it does between the pre-/post-run pushes of a real
        // prompt) and then forces a full snapshot rebuild through the same
        // seam the prompt loop uses. Reports per-push wall/allocs; RSS is
        // sampled before/after the loop to show what STAYS resident.
        //
        // R56: a phase split (pre-push context-usage walk vs the snapshot
        // payload build) to attribute the per-push cost before caching.
        "snapshot" => {
            let path = PathBuf::from(
                args.get(1)
                    .ok_or("usage: snapshot <session.jsonl> [iters]")?,
            );
            let iterations: u32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(10);
            let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!(
                "PROBE,file={},bytes={file_bytes},iterations={iterations},pid={}",
                path.display(),
                std::process::id()
            );
            let mem_start = proc_mem();
            let (registry, model) = common::ScriptedProvider::registry(Vec::new());
            let agent_dir = path
                .parent()
                .map(|parent| parent.join("probe-agent"))
                .unwrap_or_else(|| PathBuf::from("probe-agent"));
            std::fs::create_dir_all(&agent_dir)?;
            let manager = SessionManager::open(&path)?;
            let created = create_agent_session(CreateAgentSessionOptions {
                cwd: Some(PathBuf::from("C:/probe-repo")),
                agent_dir: Some(agent_dir),
                model_registry: Some(ModelRegistry::from_registry(Arc::clone(&registry))),
                model: Some(model),
                thinking_level: Some(ThinkingLevel::Off),
                session: Some(manager),
                no_tools: true,
                system_prompt: Some("You are a snapshot push probe.".to_owned()),
                discover_system_prompt: Some(false),
                ..Default::default()
            })
            .await?;
            let agent = created.session;
            // First push (builds the baseline cache state).
            let rebuilt_first = agent.push_session_context_for_test().await;
            let (fb0, fe0, fc0) = pir::agent_session::fragment_cache_stats_for_test();
            let mut total_allocs = 0_u64;
            let mut total_bytes = 0_u64;
            let mut usage_allocs = 0_u64;
            let mut usage_bytes = 0_u64;
            let mut build_allocs = 0_u64;
            let mut build_bytes = 0_u64;
            let started = std::time::Instant::now();
            for index in 0..iterations {
                // Append one entry: the fingerprint misses exactly like the
                // pre-/post-run push pair of a real prompt (each sees at
                // least one new entry).
                if let Some(manager) = agent.session_manager() {
                    let text = format!("snapshot probe entry {index}: {}", "padding ".repeat(8));
                    manager.lock().await.append_message(user_message(text))?;
                }
                let count0 = ALLOC_COUNT.load(Ordering::Relaxed);
                let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
                // Phase 1: the context-usage walk the push performs before the
                // payload build (metadata + possible character re-estimate).
                let usage = agent.context_usage().await.and_then(|usage| usage.tokens);
                usage_allocs += ALLOC_COUNT.load(Ordering::Relaxed) - count0;
                usage_bytes += ALLOC_BYTES.load(Ordering::Relaxed) - bytes0;
                let count1 = ALLOC_COUNT.load(Ordering::Relaxed);
                let bytes1 = ALLOC_BYTES.load(Ordering::Relaxed);
                let rebuilt = agent.push_session_context_for_test().await;
                build_allocs += ALLOC_COUNT.load(Ordering::Relaxed) - count1;
                build_bytes += ALLOC_BYTES.load(Ordering::Relaxed) - bytes1;
                total_allocs += ALLOC_COUNT.load(Ordering::Relaxed) - count0;
                total_bytes += ALLOC_BYTES.load(Ordering::Relaxed) - bytes0;
                debug_assert!(rebuilt || !rebuilt);
                let _ = (rebuilt_first, usage);
            }
            let (fb1, fe1, fc1) = pir::agent_session::fragment_cache_stats_for_test();
            println!(
                "PHASE,fragment-cache,full_builds={},extends={},extends_cloned={}",
                fb1 - fb0,
                fe1 - fe0,
                fc1 - fc0,
            );
            let elapsed = started.elapsed();
            let mem_end = proc_mem();
            println!(
                "PHASE,snapshot-push,iterations={iterations},wall_ms={},per_push_ms={:.2},\
                 alloc_count={total_allocs},alloc_bytes={total_bytes},\
                 per_push_alloc_count={:.1},per_push_alloc_bytes={:.0}",
                elapsed.as_millis(),
                elapsed.as_micros() as f64 / f64::from(iterations) / 1000.0,
                total_allocs as f64 / f64::from(iterations),
                total_bytes as f64 / f64::from(iterations),
            );
            println!(
                "PHASE,ctx-usage-walk,iterations={iterations},per_push_alloc_count={:.1},per_push_alloc_bytes={:.0}",
                usage_allocs as f64 / f64::from(iterations),
                usage_bytes as f64 / f64::from(iterations),
            );
            println!(
                "PHASE,snapshot-build,iterations={iterations},per_push_alloc_count={:.1},per_push_alloc_bytes={:.0}",
                build_allocs as f64 / f64::from(iterations),
                build_bytes as f64 / f64::from(iterations),
            );
            print_mem("snapshot-total", mem_start, mem_end);
            Ok(())
        }
        // R56 cost breakdown for the per-entry projection text cache: how much
        // of one snapshot push's allocations is (a) the native-contract
        // `entries` aggregate `to_value` (must stay — the todo plugin's typed
        // contract), (b) a deep `Value` clone of that aggregate (what a cached
        // aggregate clone costs), and (c) the per-entry spliced text. The
        // project must own the aggregate before a clone-based design is viable.
        "breakdown" => {
            let path = PathBuf::from(args.get(1).ok_or("usage: breakdown <session.jsonl>")?);
            let iterations: u32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(3);
            println!(
                "PROBE,file={},bytes={},iterations={iterations},pid={}",
                path.display(),
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                std::process::id()
            );
            let manager = SessionManager::open(&path)?;
            // One full projection through the manager (same walk the snapshot
            // push uses), then measure the three cost terms on it.
            let projection = manager.build_session_projection()?;
            println!(
                "BREAKDOWN,projection,entries={},messages={}",
                projection.entries.len(),
                projection.messages.len()
            );

            let measure = |label: &str, run: &dyn Fn() -> u64| {
                let count0 = ALLOC_COUNT.load(Ordering::Relaxed);
                let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
                let started = std::time::Instant::now();
                let mut checksum = 0_u64;
                for _ in 0..iterations {
                    checksum = checksum.wrapping_add(run());
                }
                let elapsed = started.elapsed();
                println!(
                    "BREAKDOWN,{label},iterations={iterations},wall_ms={},\
                     per_call_ms={:.2},alloc_count={},alloc_bytes={},\
                     per_call_alloc_count={:.1},per_call_alloc_bytes={:.0},checksum={checksum}",
                    elapsed.as_millis(),
                    elapsed.as_micros() as f64 / f64::from(iterations) / 1000.0,
                    ALLOC_COUNT.load(Ordering::Relaxed) - count0,
                    ALLOC_BYTES.load(Ordering::Relaxed) - bytes0,
                    (ALLOC_COUNT.load(Ordering::Relaxed) - count0) as f64 / f64::from(iterations),
                    (ALLOC_BYTES.load(Ordering::Relaxed) - bytes0) as f64 / f64::from(iterations),
                );
            };
            // (a) the aggregate `to_value` the snapshot push must keep paying
            // (native `entries` contract: `Arc<JsonValue>`).
            measure(
                "aggregate_to_value",
                &|| match serde_json::to_value(&projection.messages) {
                    Ok(value) => {
                        let mut probe = 0_u64;
                        if let JsonValue::Array(items) = &value {
                            probe = items.len() as u64;
                        }
                        std::mem::forget(value);
                        probe
                    }
                    Err(_) => 0,
                },
            );
            // (b) the deep clone a cached aggregate copy costs (the old
            // `message_values.clone()` term, for comparison with any cache).
            {
                let cached = serde_json::to_value(&projection.messages).unwrap_or(JsonValue::Null);
                measure("aggregate_value_clone", &|| {
                    std::mem::forget(cached.clone());
                    0
                });
            }
            // (b2) fragment-vec clone costs: one `Arc` clone (extend path) and
            // one deep clone of a whole fragment set (the shared-set path).
            {
                let fragments: Vec<serde_json::Value> = projection
                    .messages
                    .iter()
                    .map(|m| serde_json::to_value(m).unwrap_or(JsonValue::Null))
                    .collect();
                let arc = std::sync::Arc::new(fragments);
                measure("fragment_arc_clone", &|| {
                    let copy = std::sync::Arc::clone(&arc);
                    std::mem::forget(copy);
                    0
                });
                measure("fragment_vec_deep_clone", &|| {
                    std::mem::forget((*arc).clone());
                    0
                });
            }
            // (b3) the aggregate rebuild from per-entry fragment arrays: the
            // element-wise concat (clone per message) + one serialization.
            {
                let per_entry: Vec<Vec<serde_json::Value>> = projection
                    .entries
                    .iter()
                    .map(|entry| {
                        entry
                            .messages_in(&projection.messages)
                            .iter()
                            .map(|m| serde_json::to_value(m).unwrap_or(JsonValue::Null))
                            .collect()
                    })
                    .collect();
                measure("aggregate_from_fragments", &|| {
                    let items: Vec<serde_json::Value> = per_entry
                        .iter()
                        .flat_map(|slot| slot.iter().cloned())
                        .collect();
                    let text = serde_json::Value::Array(items).to_string();
                    text.len() as u64
                });
            }
            // (c) the borrowed per-entry splice (`serialize_projection_snapshot`'s
            // projection half): per-entry `to_string` of the message slices.
            measure("projection_splice", &|| {
                let mut len_sum = 0_u64;
                let mut out = String::new();
                for entry in &projection.entries {
                    let slice = entry.messages_in(&projection.messages);
                    if let Ok(text) = serde_json::to_string(slice) {
                        len_sum += text.len() as u64;
                    }
                    let _ = &mut out;
                }
                len_sum
            });
            Ok(())
        }
        other => Err(format!("unknown mode {other:?} (make|read|snapshot|breakdown)").into()),
    }
}

fn main() -> std::process::ExitCode {
    common::run_big_stack(async {
        if let Err(error) = run().await {
            eprintln!("error: {error}");
            return std::process::ExitCode::FAILURE;
        }
        std::process::ExitCode::SUCCESS
    })
}
