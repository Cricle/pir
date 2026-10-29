//! Lock-scope regressions around shared session storage and the SQLite host:
//!
//! - L3: the print host must NOT hold the shared `SessionManager` mutex across
//!   the paged compaction summarizer. The provider below blocks inside
//!   `complete`; a second task appends a background result through
//!   `SharedSession` and must finish while the summarizer is still blocked
//!   (before the fix it could not acquire the lock at all).
//! - L5: the extension session-handle table must not be held across a deep
//!   snapshot. Handle A (large session) builds its snapshot while handle B's
//!   cheap snapshot proceeds on another runtime; B's latency must not track
//!   A's. This is a timing assertion with a wide margin (A's debug-build JSON
//!   build is far slower than B's empty in-memory session); the structural
//!   guarantee is the split in `ext_session_ops` — the process-global table
//!   lock only protects the handle map; the snapshot/rewrite run under the
//!   per-handle mutex.
//! - L6: SQLite statements hold only their own connection's mutex. A blocked
//!   INSERT on database A (a write transaction held by a second host handle)
//!   must not delay a `SELECT` on database B.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    ProviderStreamHooks, StopReason, TerminalReason, TextContent, ThinkingLevel, Usage,
    UserMessage, UserRole,
};
use pir::coding_agent::print::{print_compaction_hook, SharedSession};
use pir::compaction::CompactionConfig;
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use pir::session_manager::{
    FileEntry, SessionManager, DEFAULT_TAIL_PAGE_BYTES, DEFAULT_TAIL_PAGE_ENTRIES,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-session-lock-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_model(context_window: u64) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window,
        max_tokens: 4_096,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn extension_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("lock-scope-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

type RuntimeFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + 'static>>;

/// Run one `ExtensionRuntime` on its own OS thread with its own
/// current-thread tokio runtime. The runtime (and its QuickJS context) is
/// `!Send`, so it is constructed INSIDE the thread; concurrent host calls from
/// two runtimes then overlap for real (the extension runtime itself serializes
/// JS calls).
fn spawn_runtime<T, F>(cwd: PathBuf, f: F) -> std::thread::JoinHandle<T>
where
    T: Send + 'static,
    F: FnOnce(ExtensionRuntime) -> RuntimeFuture<T> + Send + 'static,
{
    // Production extension runtimes live on the actor thread, which is created
    // with a 64 MiB stack (`extensions/handle/mailbox.rs`), and their QuickJS
    // stack limit is 16 MiB. A default 2 MiB std thread overflows while the
    // 13 MB / 1100-deep session snapshot is parsed and freed in JS, so the
    // harness matches production instead of pinning a smaller budget.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime");
            rt.block_on(async move {
                let runtime = ExtensionRuntime::new(extension_config(&cwd))
                    .await
                    .expect("extension runtime");
                f(runtime).await
            })
        })
        .expect("spawn runtime thread")
}

fn js_path(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy().replace('\\', "/")).expect("path literal")
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    path.exists()
}

// ---------------------------------------------------------------------------
// L3 — paged compaction must summarize with the SharedSession lock released
// ---------------------------------------------------------------------------

/// Blocks every summarizer call until the gate opens; the first call signals
/// the test that the provider is inside `complete`.
struct BlockingSummaryProvider {
    model: Model,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    gate: Arc<(StdMutex<bool>, Condvar)>,
    calls: Arc<AtomicUsize>,
}

impl Provider for BlockingSummaryProvider {
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &str {
        &self.model.provider
    }

    fn name(&self) -> &str {
        "blocking compaction summary double"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: pir::ai::types::StreamOptions,
    ) -> AssistantMessageEventStream {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let output = stream.clone();
        let gate = Arc::clone(&self.gate);
        let started = self.started.clone();
        let model = self.model.clone();
        std::thread::spawn(move || {
            let _ = started.send(());
            let (lock, condvar) = &*gate;
            let mut open = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*open {
                open = condvar
                    .wait(open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            drop(open);
            output.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage {
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: format!("COMPACTION SUMMARY {call}"),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    usage: Usage {
                        total_tokens: 5,
                        ..Usage::default()
                    },
                    timestamp: 1,
                    ..AssistantMessage::default()
                },
            ));
        });
        stream
    }
}

fn user_message(text: String, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp,
        sections: None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paged_compaction_releases_the_shared_session_lock() {
    let dir = TempDir::new("paged-compaction");
    let cwd = dir.0.to_string_lossy().into_owned();
    let session_dir = dir.0.join("sessions");

    // A session large enough to open TAIL-FIRST (more than the entry cap), so
    // the print hook takes the paged fast path.
    let mut manager = SessionManager::create(&cwd, &session_dir).expect("create session");
    for index in 0..1100u64 {
        manager
            .append_message(user_message(format!("turn {index}"), index))
            .expect("append message");
    }
    manager.sync_now().expect("sync session");
    let session_file = manager.session_file().expect("session file").to_path_buf();
    drop(manager);

    let paged = SessionManager::open_paged(
        &session_file,
        DEFAULT_TAIL_PAGE_BYTES,
        DEFAULT_TAIL_PAGE_ENTRIES,
    )
    .expect("open paged");
    assert!(
        paged.is_paged(),
        "fixture must exceed the tail-page entry cap"
    );
    let session: SharedSession = Arc::new(tokio::sync::Mutex::new(Some(paged)));

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(BlockingSummaryProvider {
        model: test_model(10),
        started: started_tx,
        gate: Arc::clone(&gate),
        calls: Arc::clone(&calls),
    }));
    let hook = print_compaction_hook(
        CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 1,
        },
        test_model(10),
        Arc::new(registry),
        None,
        ThinkingLevel::Off,
        ProviderStreamHooks::default(),
        Arc::clone(&session),
        None,
        None,
        None,
    );
    let context = Arc::new(Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    });
    let task = tokio::spawn(hook(Arc::clone(&context)));

    // The summarizer is now blocked inside `complete` — the shared session
    // lock is free exactly when the print host released it before the call.
    tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
        .await
        .expect("the summarizer must start")
        .expect("summarizer start signal");
    assert!(
        !task.is_finished(),
        "the summarizer is expected to be blocked here"
    );

    // A concurrent background-result append through SharedSession must
    // complete while the summarizer is still blocked. Before the fix this
    // future could not acquire the mutex until the gate opened.
    let appended = tokio::time::timeout(Duration::from_secs(2), async {
        let mut guard = session.lock().await;
        guard
            .as_mut()
            .expect("attached session")
            .append_custom_message_entry(
                "backgroundToolResult",
                json!("late background result"),
                false,
                None,
            )
    })
    .await
    .expect("the SharedSession lock must be free during the summarizer call");
    assert!(appended.is_ok(), "append failed: {appended:?}");

    {
        let (lock, condvar) = &*gate;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        condvar.notify_all();
    }
    let rebuilt = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("compaction must finish after the gate opens")
        .expect("compaction task");
    assert!(
        !Arc::ptr_eq(&context, &rebuilt),
        "a successful compaction returns a rebuilt context"
    );
    assert!(calls.load(Ordering::SeqCst) > 0, "summarizer never ran");

    let guard = session.lock().await;
    let manager = guard.as_ref().expect("attached session");
    let compactions = manager
        .entries()
        .iter()
        .filter(|entry| matches!(entry, FileEntry::Compaction(_)))
        .count();
    assert_eq!(
        compactions, 1,
        "the compaction entry is appended exactly once"
    );
    assert!(
        manager
            .entries()
            .iter()
            .any(|entry| matches!(entry, FileEntry::CustomMessage(message)
                if message.custom_type == "backgroundToolResult")),
        "the concurrent background result was persisted"
    );
}

// ---------------------------------------------------------------------------
// L5 — the session-handle table lock is not held across a deep snapshot
// ---------------------------------------------------------------------------

#[test]
fn session_snapshot_on_one_handle_does_not_block_another_handle() {
    let dir = TempDir::new("session-handles");
    let cwd = dir.0.to_string_lossy().into_owned();
    let session_dir = dir.0.join("sessions");

    // A deliberately large session: the snapshot builds entries + branch +
    // tree JSON, which is the work that used to run under the global table
    // lock. 1500 messages x ~4 KiB keeps the debug-build snapshot busy for
    // far longer than the cheap counterpart.
    let mut manager = SessionManager::create(&cwd, &session_dir).expect("create session");
    let body = "x".repeat(4096);
    for index in 0..1500u64 {
        manager
            .append_message(user_message(format!("{index}:{body}"), index))
            .expect("append message");
    }
    manager.sync_now().expect("sync session");
    let session_file = manager.session_file().expect("session file").to_path_buf();
    drop(manager);

    let marker = dir.0.join("snapshot-started.flag");
    let session_literal = js_path(&session_file);
    let marker_literal = js_path(&marker);

    let (ready_a_tx, ready_a_rx) = std::sync::mpsc::channel::<u64>();
    let (start_a_tx, start_a_rx) = std::sync::mpsc::channel::<()>();
    let (result_a_tx, result_a_rx) = std::sync::mpsc::channel::<(Duration, Value)>();
    let thread_a = spawn_runtime(dir.0.clone(), move |rt| -> RuntimeFuture<()> {
        Box::pin(async move {
            // Return only the handle: the open reply carries the whole
            // session (13 MB for this fixture), and `eval_json` parses its
            // result with serde_json, whose 128-level limit the nested tree
            // trips. JS `JSON.parse` has no such limit.
            let opened = rt
                .eval_json(&format!(
                    "JSON.stringify({{handle: JSON.parse(__pir_host.pirSessionOpenJson(JSON.stringify({{kind:\"open\", path:{session_literal}}}))).handle}})"
                ))
                .await
                .expect("open the large session");
            let handle = opened["handle"].as_u64().expect("large handle");
            ready_a_tx.send(handle).expect("ready A");
            start_a_rx.recv().expect("start A");
            let started = Instant::now();
            // The marker is written immediately before the (synchronous)
            // snapshot host call, so its appearance proves the snapshot is
            // entering/inside its deep work.
            let value = rt
                .eval_json(&format!(
                    "(() => {{ __pir_host.node(\"fs\",\"writeFile\", JSON.stringify({{path:{marker_literal}, dataText:\"1\"}})); \
                     const s = __pir_host.pirSessionSnapshotJson({handle}); \
                     return JSON.stringify({{len: s.length}}); }})()"
                ))
                .await
                .expect("large snapshot");
            let elapsed = started.elapsed();
            result_a_tx.send((elapsed, value)).expect("result A");
        })
    });

    let (ready_b_tx, ready_b_rx) = std::sync::mpsc::channel::<u64>();
    let (go_b_tx, go_b_rx) = std::sync::mpsc::channel::<()>();
    let (result_b_tx, result_b_rx) = std::sync::mpsc::channel::<(Duration, Value)>();
    let thread_b = spawn_runtime(dir.0.clone(), move |rt| -> RuntimeFuture<()> {
        Box::pin(async move {
            let opened = rt
                .eval_json(
                    "__pir_host.pirSessionOpenJson(JSON.stringify({kind:\"inMemory\", cwd:\".\"}))",
                )
                .await
                .expect("open the small session");
            let handle = opened["handle"].as_u64().expect("small handle");
            ready_b_tx.send(handle).expect("ready B");
            go_b_rx.recv().expect("go B");
            let started = Instant::now();
            let value = rt
                .eval_json(&format!("__pir_host.pirSessionSnapshotJson({handle})"))
                .await
                .expect("small snapshot");
            result_b_tx
                .send((started.elapsed(), value))
                .expect("result B");
        })
    });

    ready_a_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("large handle ready");
    ready_b_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("small handle ready");
    start_a_tx.send(()).expect("start A");
    assert!(
        wait_for_file(&marker, Duration::from_secs(30)),
        "the large snapshot never started"
    );

    go_b_tx.send(()).expect("go B");
    let (b_elapsed, b_value) = result_b_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("handle B's snapshot must not wait for handle A's deep snapshot");
    let (a_elapsed, a_value) = result_a_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("large snapshot result");
    thread_a.join().expect("large snapshot thread");
    thread_b.join().expect("small snapshot thread");

    assert_eq!(b_value["ok"], json!(true), "small snapshot: {b_value}");
    assert!(
        a_value["len"].as_u64().unwrap_or(0) > 0,
        "large snapshot returned no text: {a_value}"
    );
    // Structural assertion, stated explicitly: the table lock only protects
    // the handle map. B's cheap op must not track A's deep snapshot. The
    // factor-4 margin absorbs scheduler noise; under the old shape B's
    // snapshot waited for A's full JSON build (ratio ≈ 1).
    assert!(
        a_elapsed >= Duration::from_millis(20),
        "fixture too small to be conclusive: large snapshot took {a_elapsed:?}"
    );
    assert!(
        b_elapsed.as_millis() * 4 < a_elapsed.as_millis(),
        "handle B waited for handle A's snapshot: b={b_elapsed:?} a={a_elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// L6 — SQLite statements on different databases do not serialize
// ---------------------------------------------------------------------------

#[test]
fn sqlite_statement_on_one_database_does_not_block_another_database() {
    let dir = TempDir::new("sqlite-lock");
    let db_path = dir.0.join("blocked.db");
    let db_literal = js_path(&db_path);
    let locker_db_literal = db_literal.clone();
    let marker = dir.0.join("insert-started.flag");
    let marker_literal = js_path(&marker);

    // Locker: holds a write transaction on the database through a host handle.
    let (locker_ready_tx, locker_ready_rx) = std::sync::mpsc::channel::<u64>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let locker = spawn_runtime(dir.0.clone(), move |rt| -> RuntimeFuture<()> {
        Box::pin(async move {
            let opened = rt
                .eval_json(&format!(
                    "__pir_host.node(\"sqlite\",\"open\", JSON.stringify({{path:{locker_db_literal}}}))"
                ))
                .await
                .expect("open locker database");
            let id = opened["value"]["id"].as_u64().expect("locker handle id");
            let result = rt
                .eval_json(&format!(
                    "__pir_host.node(\"sqlite\",\"exec\", JSON.stringify({{id:{id}, \
                     sql:\"CREATE TABLE IF NOT EXISTS t(v); BEGIN IMMEDIATE; INSERT INTO t VALUES (0);\"}}))"
                ))
                .await
                .expect("locker transaction");
            assert_eq!(result["ok"], json!(true), "locker exec: {result}");
            locker_ready_tx.send(id).expect("locker ready");
            release_rx.recv().expect("release signal");
            let rollback = rt
                .eval_json(&format!(
                    "__pir_host.node(\"sqlite\",\"exec\", JSON.stringify({{id:{id}, sql:\"ROLLBACK\"}}))"
                ))
                .await
                .expect("rollback");
            assert_eq!(rollback["ok"], json!(true), "rollback: {rollback}");
        })
    });
    locker_ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("locker holds the write transaction");

    // A: same database, an INSERT that must wait for the locker's lock.
    let (a_ready_tx, a_ready_rx) = std::sync::mpsc::channel::<()>();
    let (a_start_tx, a_start_rx) = std::sync::mpsc::channel::<()>();
    let (a_result_tx, a_result_rx) = std::sync::mpsc::channel::<Value>();
    let writer = spawn_runtime(dir.0.clone(), move |rt| -> RuntimeFuture<()> {
        Box::pin(async move {
            let opened = rt
                .eval_json(&format!(
                    "__pir_host.node(\"sqlite\",\"open\", JSON.stringify({{path:{db_literal}, timeout:5000}}))"
                ))
                .await
                .expect("open blocked database");
            let id = opened["value"]["id"].as_u64().expect("writer handle id");
            let prepared = rt
                .eval_json(&format!(
                    "__pir_host.node(\"sqlite\",\"prepare\", JSON.stringify({{id:{id}, \
                     sql:\"INSERT INTO t VALUES (1)\"}}))"
                ))
                .await
                .expect("prepare blocked insert");
            assert_eq!(prepared["ok"], json!(true), "prepare: {prepared}");
            a_ready_tx.send(()).expect("writer ready");
            a_start_rx.recv().expect("writer start");
            // Marker first: the very next host call is the blocking INSERT,
            // so the marker's appearance proves this statement is in flight.
            let result = rt
                .eval_json(&format!(
                    "(() => {{ __pir_host.node(\"fs\",\"writeFile\", JSON.stringify({{path:{marker_literal}, dataText:\"1\"}})); \
                     return __pir_host.node(\"sqlite\",\"run\", JSON.stringify({{id:{id}, \
                     sql:\"INSERT INTO t VALUES (1)\", binds: []}})); }})()"
                ))
                .await
                .expect("blocked insert");
            a_result_tx.send(result).expect("writer result");
        })
    });

    // B: a DIFFERENT database (in-memory), one cheap statement.
    let (b_ready_tx, b_ready_rx) = std::sync::mpsc::channel::<()>();
    let (b_go_tx, b_go_rx) = std::sync::mpsc::channel::<()>();
    let (b_result_tx, b_result_rx) = std::sync::mpsc::channel::<(Duration, Value)>();
    let reader = spawn_runtime(dir.0.clone(), move |rt| -> RuntimeFuture<()> {
        Box::pin(async move {
            let opened = rt
                .eval_json(
                    "__pir_host.node(\"sqlite\",\"open\", JSON.stringify({path:\":memory:\"}))",
                )
                .await
                .expect("open quick database");
            let id = opened["value"]["id"].as_u64().expect("reader handle id");
            b_ready_tx.send(()).expect("reader ready");
            b_go_rx.recv().expect("reader go");
            let started = Instant::now();
            let value = rt
                .eval_json(&format!(
                    "__pir_host.node(\"sqlite\",\"get\", JSON.stringify({{id:{id}, sql:\"SELECT 1 AS x\"}}))"
                ))
                .await
                .expect("quick get");
            b_result_tx
                .send((started.elapsed(), value))
                .expect("reader result");
        })
    });

    a_ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("writer ready");
    b_ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("reader ready");
    a_start_tx.send(()).expect("start writer");
    assert!(
        wait_for_file(&marker, Duration::from_secs(10)),
        "the blocked INSERT never started"
    );
    // Let the INSERT enter SQLite's busy wait before the quick statement.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        a_result_rx.try_recv().is_err(),
        "the INSERT on database A must still be blocked by the locker's transaction"
    );

    b_go_tx.send(()).expect("go reader");
    let quick = b_result_rx.recv_timeout(Duration::from_secs(3));
    // Release the lock regardless, so the writer/locker threads can finish.
    let _ = release_tx.send(());
    let (quick_elapsed, quick_value) =
        quick.expect("a statement on database B must not wait for database A's blocked statement");
    let blocked_value = a_result_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("blocked insert result");
    locker.join().expect("locker thread");
    writer.join().expect("writer thread");
    reader.join().expect("reader thread");

    assert_eq!(quick_value["ok"], json!(true), "quick get: {quick_value}");
    assert_eq!(
        quick_value["value"]["x"],
        json!(1),
        "quick get returned the wrong row: {quick_value}"
    );
    assert_eq!(
        blocked_value["ok"],
        json!(true),
        "blocked insert: {blocked_value}"
    );
    assert!(
        quick_elapsed < Duration::from_secs(3),
        "quick statement took {quick_elapsed:?}"
    );
}
