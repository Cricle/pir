//! Regression test for the shell abort path: aborting a live command must
//! settle promptly.
//!
//! `ProcessTreeKill::terminate` runs the MSYS subtree sweep and `taskkill`
//! synchronously (they MUST complete before the caller's `start_kill`; an
//! async offload was tried and reverted because it raced the kill and left
//! descendants alive — see `background_tool_calls`). The sweep's shell config
//! is cached, so the abort still returns well within the test's bound.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::ai::types::AbortSignal;
use pir::coding_agent::register_core_tools;
use pir::coding_agent::tools::resolve_shell_config;

/// Serializes the process-global `PIR_AGENT_DIR` pin (only this file's test
/// uses it, but the env is process-wide).
static AGENT_DIR_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pin `PIR_AGENT_DIR` at a scratch directory with `shellPool: false`, so the
/// bash tool takes its per-call spawn path (the `terminate` call under test)
/// and no ambient `~/.pi/agent/settings.json` changes the shell.
struct HermeticAgentDir {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<OsString>,
    dir: PathBuf,
}

impl HermeticAgentDir {
    async fn new(label: &str) -> Self {
        let lock = AGENT_DIR_ENV_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!(
            "pir-bash-tree-kill-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create pinned agent dir");
        std::fs::write(dir.join("settings.json"), r#"{ "shellPool": false }"#)
            .expect("write pinned settings");
        let previous = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", &dir);
        Self {
            _lock: lock,
            previous,
            dir,
        }
    }
}

impl Drop for HermeticAgentDir {
    fn drop(&mut self) {
        match self.previous.as_deref() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-bash-tree-kill-cwd-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp cwd");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bash_abort_returns_without_waiting_for_the_tree_kill_sweep() {
    let _agent_dir = HermeticAgentDir::new("prompt").await;
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let cwd = TempDir::new("prompt");

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let tool = registry.get("bash").expect("bash tool registered");

    let signal = AbortSignal::new();
    let context = ToolContext {
        signal: Some(signal.clone()),
        ..ToolContext::new(cwd.0.clone())
    };

    let marker = cwd.0.join("started");
    let marker_unix = marker.to_string_lossy().replace('\\', "/");
    let command = format!("touch '{marker_unix}'; sleep 30");
    let run = tokio::spawn(async move {
        tool.run(&serde_json::json!({ "command": command }), &context)
            .await
    });

    // Wait for the command to prove it actually started, so the abort drives
    // the live tree-kill path rather than a pre-spawn cancel.
    let mut started = false;
    for _ in 0..600 {
        if marker.exists() {
            started = true;
            break;
        }
        if run.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(started, "the bash command never started");

    let abort_started = Instant::now();
    signal.abort();
    // The tree-kill sweep is synchronous by design (`ProcessTreeKill::terminate`
    // documents why: the MSYS walk needs the root's process-table entry, so it
    // must finish before `start_kill`). It therefore costs 150–300 ms idle and
    // seconds under parallel test load (measured 4.2 s), so the bound here is
    // "settles long before the 30 s command would finish", not "under 1 s".
    let result = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("abort must settle")
        .expect("run task join");
    let abort_elapsed = abort_started.elapsed();

    assert!(result.is_error, "an aborted bash call is an error result");
    assert!(
        result.content.contains("aborted"),
        "expected the abort result, got: {}",
        result.content
    );
    assert!(
        abort_elapsed < Duration::from_secs(15),
        "abort did not settle: it blocked on the command instead of the tree-kill sweep: {abort_elapsed:?}"
    );
}
