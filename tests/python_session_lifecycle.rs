//! Lifecycle tests for the embedded-Python consumers (`python` and
//! `context-mode`), covering the two gaps from the builtin audit:
//!
//! - **P1-1 / session ownership.** `AgentPlugin::register` runs again on every
//!   registry rebuild (MCP discovery, JS registrations, `/reload`). The
//!   `python` plugin must hand each new tool object the SAME
//!   `Arc<PythonSession>`, so the persistent namespace survives; an unrelated
//!   plugin instance must get its own session. `ContextModePlugin` already
//!   owns its lazy state — asserted here (worker spawn count + indexed rows
//!   across a rebuild).
//! - **P2-8 / explicit shutdown.** `on_shutdown` interrupts a running job
//!   through the existing user-signal channel (not the 120s deadline), stops
//!   the worker, closes the SQLite index, and is immediate + idempotent when
//!   nothing was started. Session-level errors carry the consumer's label
//!   (`python tool` vs `ctx_execute`).
//!
//! All tests are hermetic: the knowledge base lives in a per-test temp
//! directory; the interpreter is the embedded RustPython build.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::builtin::{
    ContextModePlugin, PythonPlugin, CTX_EXECUTE_TOOL_NAME, CTX_INDEX_TOOL_NAME,
    CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME, PYTHON_TOOL_NAME,
};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};
use pir::plugins::PluginHost;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// Self-cleaning temp directory.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-python-lifecycle-{tag}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap_or_else(|error| panic!("temp dir: {error}"));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// A working directory inside the temp dir (also the DB's parent).
    fn cwd(&self) -> PathBuf {
        let cwd = self.0.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap_or_else(|error| panic!("cwd: {error}"));
        cwd
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Resolve a registered Rust tool from the host's CURRENT registry (what the
/// agent would call after a rebuild).
fn host_tool(host: &PluginHost, name: &str) -> Arc<dyn AgentTool> {
    let registry = host.registry();
    let entries = registry.resolved_tools();
    entries
        .iter()
        .find(|entry| entry.0.name == name)
        .and_then(|entry| match &entry.1 {
            ToolBackend::Rust(tool) => Some(Arc::clone(tool)),
            ToolBackend::Js { .. } => None,
        })
        .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
}

/// Register a plugin directly and resolve one of its tools (the registration
/// path both the host and the tests' harness use).
fn only_tool(plugin: &dyn AgentPlugin, name: &str) -> Arc<dyn AgentTool> {
    let mut registrations = PluginRegistrations::default();
    plugin
        .register(&mut RegistrationContext::new(&mut registrations))
        .unwrap_or_else(|error| panic!("register: {error}"));
    registrations
        .tools
        .into_iter()
        .find(|(definition, _)| definition.name == name)
        .and_then(|(_, backend)| match backend {
            ToolBackend::Rust(tool) => Some(tool),
            ToolBackend::Js { .. } => None,
        })
        .unwrap_or_else(|| panic!("tool `{name}` was not registered"))
}

fn parse_json(result: &pir::agent::tool_batch::ToolResult) -> Value {
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("expected JSON result, got `{}` ({error})", result.content))
}

/// Wait until `probe` exists (the runaway loop demonstrably started), or the
/// task finishes early. Returns whether the loop started.
async fn wait_for_probe(
    probe: &Path,
    task: &tokio::task::JoinHandle<pir::agent::tool_batch::ToolResult>,
) -> bool {
    for _ in 0..400 {
        if probe.exists() {
            return true;
        }
        if task.is_finished() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

// ---------------------------------------------------------------------------
// P1-1: the session is owned by the plugin, not the tool object
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_session_survives_registry_rebuild() {
    let plugin = Arc::new(PythonPlugin::with_config(true));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));

    let first_tool = host_tool(&host, PYTHON_TOOL_NAME);
    let warm = first_tool.run(&json!({ "code": "x = 41" }), &ctx).await;
    assert!(!warm.is_error, "{}", warm.content);
    assert_eq!(
        plugin.python_spawn_count(),
        1,
        "the first call starts exactly one worker"
    );

    // The host's rebuild path (MCP discovery, JS registration, /reload) re-runs
    // register(), which replaces the tool object the agent calls next.
    host.rebuild_registry().await;
    let rebuilt_tool = host_tool(&host, PYTHON_TOOL_NAME);
    assert_eq!(
        plugin.python_spawn_count(),
        1,
        "registration must not touch the session"
    );

    let result = rebuilt_tool.run(&json!({ "code": "x + 1" }), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        result.content, "42",
        "globals defined before the rebuild must stay visible after it"
    );
    assert_eq!(
        plugin.python_spawn_count(),
        1,
        "the rebuilt tool must reuse the existing worker, not spawn a new one"
    );
    assert!(plugin.has_python_worker());

    plugin.on_shutdown().await.expect("shutdown");
    assert!(!plugin.has_python_worker());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_mode_state_survives_registry_rebuild() {
    let dir = TestDir::new("ctx-rebuild");
    let plugin = Arc::new(ContextModePlugin::with_db_path(
        dir.path().join("context-mode.db"),
    ));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    let ctx = ToolContext::new(dir.cwd());

    // Index a row and warm the interpreter BEFORE the rebuild.
    let index_tool = host_tool(&host, CTX_INDEX_TOOL_NAME);
    let indexed = index_tool
        .run(
            &json!({
                "content": "# Rebuild\n\nregistry-rebuild-marker survives",
                "source": "rebuild",
            }),
            &ctx,
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);

    let exec_tool = host_tool(&host, CTX_EXECUTE_TOOL_NAME);
    let warm = exec_tool
        .run(&json!({ "code": "print('warm')" }), &ctx)
        .await;
    assert!(!warm.is_error, "{}", warm.content);
    assert_eq!(plugin.python_spawn_count(), 1);

    host.rebuild_registry().await;

    // The rebuilt tools share the plugin's state: no second worker...
    let rebuilt_exec = host_tool(&host, CTX_EXECUTE_TOOL_NAME);
    let again = rebuilt_exec
        .run(&json!({ "code": "print('again')" }), &ctx)
        .await;
    assert!(!again.is_error, "{}", again.content);
    assert_eq!(
        plugin.python_spawn_count(),
        1,
        "the rebuild must reuse the worker"
    );
    assert!(plugin.has_python_worker());

    // ...and the rows indexed before the rebuild are still in the knowledge
    // base.
    let stats_tool = host_tool(&host, CTX_STATS_TOOL_NAME);
    let stats = stats_tool.run(&json!({}), &ctx).await;
    assert!(!stats.is_error, "{}", stats.content);
    let stats = parse_json(&stats);
    assert!(
        stats["chunks"].as_u64().unwrap_or(0) >= 1,
        "indexed rows must survive the rebuild: {stats}"
    );

    let search_tool = host_tool(&host, CTX_SEARCH_TOOL_NAME);
    let search = search_tool
        .run(&json!({ "queries": ["registry-rebuild-marker"] }), &ctx)
        .await;
    assert!(!search.is_error, "{}", search.content);
    let search = parse_json(&search);
    assert!(
        !search["queries"][0]["results"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "the rebuilt search tool must still find the indexed source: {search}"
    );

    plugin.on_shutdown().await.expect("shutdown");
    assert!(!plugin.has_python_worker());
    assert!(
        !plugin.index_is_open(),
        "shutdown closes the SQLite index connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_plugin_instance_starts_a_fresh_session() {
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));

    let first = PythonPlugin::with_config(true);
    let first_tool = only_tool(&first, PYTHON_TOOL_NAME);
    let set = first_tool
        .run(&json!({ "code": "leaked = 41" }), &ctx)
        .await;
    assert!(!set.is_error, "{}", set.content);
    assert_eq!(first.python_spawn_count(), 1);

    // A second plugin instance owns its own session: no process-global
    // sharing of the namespace (or of the worker).
    let second = PythonPlugin::with_config(true);
    assert!(!second.has_python_worker());
    let second_tool = only_tool(&second, PYTHON_TOOL_NAME);
    let check = second_tool
        .run(&json!({ "code": "'leaked' in globals()" }), &ctx)
        .await;
    assert!(!check.is_error, "{}", check.content);
    assert_eq!(
        check.content, "False",
        "a new plugin instance must not see another instance's globals"
    );
    assert_eq!(second.python_spawn_count(), 1);

    first.on_shutdown().await.expect("shutdown");
    second.on_shutdown().await.expect("shutdown");
    assert!(!first.has_python_worker());
    assert!(!second.has_python_worker());
}

// ---------------------------------------------------------------------------
// P2-8: shutdown interrupts, stops the worker, and closes the DB
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_mode_shutdown_interrupts_a_running_job() {
    let dir = TestDir::new("ctx-shutdown");
    let plugin = Arc::new(ContextModePlugin::with_db_path(
        dir.path().join("context-mode.db"),
    ));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    let ctx = ToolContext::new(dir.cwd());

    // Open the index connection so shutdown has something to close, and warm
    // the interpreter so the runaway job starts immediately.
    let index_tool = host_tool(&host, CTX_INDEX_TOOL_NAME);
    let indexed = index_tool
        .run(
            &json!({
                "content": "# Shutdown\n\nshutdown-db-marker",
                "source": "shutdown",
            }),
            &ctx,
        )
        .await;
    assert!(!indexed.is_error, "{}", indexed.content);
    assert!(plugin.index_is_open());

    let exec_tool = host_tool(&host, CTX_EXECUTE_TOOL_NAME);
    let warm = exec_tool
        .run(&json!({ "code": "print('warm')" }), &ctx)
        .await;
    assert!(!warm.is_error, "{}", warm.content);
    assert_eq!(plugin.python_spawn_count(), 1);

    let probe = dir.path().join("running.txt");
    let probe_path = probe.to_string_lossy().replace('\\', "/");
    let code = format!("open(\"{probe_path}\", \"w\").write(\"started\")\nwhile True:\n    pass");
    let run_tool = Arc::clone(&exec_tool);
    let run_ctx = ctx.clone();
    let run_task =
        tokio::spawn(async move { run_tool.run(&json!({ "code": code }), &run_ctx).await });
    assert!(
        wait_for_probe(&probe, &run_task).await,
        "the runaway ctx_execute loop never started"
    );

    let started = Instant::now();
    plugin.on_shutdown().await.expect("shutdown");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "shutdown must interrupt the running job instead of waiting for the deadline: {elapsed:?}"
    );
    assert!(
        !plugin.has_python_worker(),
        "shutdown must remove the worker handle"
    );
    assert!(
        !plugin.index_is_open(),
        "shutdown must close the SQLite index connection"
    );

    let result = run_task.await.expect("run task join");
    assert!(
        result.is_error,
        "the interrupted call reports an error, got: {}",
        result.content
    );
    assert!(
        result.content.contains("KeyboardInterrupt"),
        "the interrupt reached the eval loop, got: {}",
        result.content
    );
    assert!(
        result.content.contains("ctx_execute"),
        "context-mode errors must name ctx_execute, got: {}",
        result.content
    );

    // A later call starts cleanly on a fresh worker; the index reopens
    // lazily and still holds the row indexed before shutdown.
    let after = exec_tool
        .run(&json!({ "code": "print('after shutdown')" }), &ctx)
        .await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(after.content.trim_end(), "after shutdown");
    assert_eq!(
        plugin.python_spawn_count(),
        2,
        "the next call spawns a fresh worker"
    );
    assert!(plugin.has_python_worker());

    let search_tool = host_tool(&host, CTX_SEARCH_TOOL_NAME);
    let search = search_tool
        .run(&json!({ "queries": ["shutdown-db-marker"] }), &ctx)
        .await;
    assert!(!search.is_error, "{}", search.content);
    let search = parse_json(&search);
    assert!(
        !search["queries"][0]["results"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "the index reopens cleanly after shutdown: {search}"
    );

    plugin.on_shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_tool_shutdown_interrupts_a_running_job() {
    let plugin = Arc::new(PythonPlugin::with_config(true));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));

    let tool = host_tool(&host, PYTHON_TOOL_NAME);
    let warm = tool.run(&json!({ "code": "x = 1" }), &ctx).await;
    assert!(!warm.is_error, "{}", warm.content);
    assert_eq!(plugin.python_spawn_count(), 1);

    let dir = TestDir::new("python-shutdown");
    let probe = dir.path().join("running.txt");
    let probe_path = probe.to_string_lossy().replace('\\', "/");
    let code = format!("open(\"{probe_path}\", \"w\").write(\"started\")\nwhile True:\n    pass");
    let run_tool = Arc::clone(&tool);
    let run_ctx = ctx.clone();
    let run_task =
        tokio::spawn(async move { run_tool.run(&json!({ "code": code }), &run_ctx).await });
    assert!(
        wait_for_probe(&probe, &run_task).await,
        "the runaway python loop never started"
    );

    let started = Instant::now();
    plugin.on_shutdown().await.expect("shutdown");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "shutdown must interrupt the running job instead of waiting for the deadline: {elapsed:?}"
    );
    assert!(
        !plugin.has_python_worker(),
        "shutdown must remove the worker handle"
    );

    let result = run_task.await.expect("run task join");
    assert!(
        result.is_error,
        "the interrupted call reports an error, got: {}",
        result.content
    );
    assert!(
        result.content.contains("KeyboardInterrupt"),
        "the interrupt reached the eval loop, got: {}",
        result.content
    );
    assert!(
        result.content.contains("python tool"),
        "python errors must name the python tool, got: {}",
        result.content
    );

    // The same tool object (same session) starts a fresh worker on demand.
    let after = tool.run(&json!({ "code": "1 + 1" }), &ctx).await;
    assert!(!after.is_error, "{}", after.content);
    assert_eq!(after.content, "2");
    assert_eq!(plugin.python_spawn_count(), 2);
    assert!(plugin.has_python_worker());

    plugin.on_shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_mode_timeout_errors_name_ctx_execute() {
    let dir = TestDir::new("ctx-label");
    let plugin = ContextModePlugin::with_db_path(dir.path().join("context-mode.db"));
    let tool = only_tool(&plugin, CTX_EXECUTE_TOOL_NAME);
    let ctx = ToolContext::new(dir.cwd());

    // Warm the interpreter first so the deadline lands on the RUNNING loop
    // (the path whose message is produced by the session, not the queue).
    let warm = tool.run(&json!({ "code": "print('warm')" }), &ctx).await;
    assert!(!warm.is_error, "{}", warm.content);

    let result = tool
        .run(
            &json!({ "code": "while True:\n    pass", "timeout": 1 }),
            &ctx,
        )
        .await;
    assert!(result.is_error, "expected a timeout error");
    assert!(
        result.content.contains("ctx_execute: execution timed out"),
        "context-mode timeout text must name ctx_execute: {}",
        result.content
    );

    plugin.on_shutdown().await.expect("shutdown");
    assert!(!plugin.has_python_worker());
}

#[tokio::test]
async fn shutdown_of_unused_plugins_is_immediate_and_idempotent() {
    let dir = TestDir::new("unused-shutdown");
    let python = PythonPlugin::with_config(true);
    let context_mode = ContextModePlugin::with_db_path(dir.path().join("context-mode.db"));

    // Construction is lazy: no worker, no DB connection.
    assert!(!python.has_python_worker());
    assert!(!context_mode.has_python_worker());
    assert!(!context_mode.index_is_open());

    let started = Instant::now();
    python.on_shutdown().await.expect("shutdown");
    context_mode.on_shutdown().await.expect("shutdown");
    // Idempotent: a second teardown pass is still a no-op.
    python.on_shutdown().await.expect("shutdown");
    context_mode.on_shutdown().await.expect("shutdown");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(100),
        "shutdown without a worker or DB must be immediate: {elapsed:?}"
    );
    assert_eq!(python.python_spawn_count(), 0);
    assert_eq!(context_mode.python_spawn_count(), 0);
    assert!(!python.has_python_worker());
    assert!(!context_mode.has_python_worker());
    assert!(!context_mode.index_is_open());
}
