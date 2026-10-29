//! Tests for the active-bash abort set: every `execute_bash` registers its
//! own abort signal and `abort_bash` cancels ALL of them (upstream
//! `_bashAbortControllers = Set<AbortController>` semantics).
//!
//! Regression context: the previous implementation kept a single
//! `Mutex<AbortSignal>` that each `execute_bash` replaced, so a concurrent
//! sibling (RPC `bash` vs extension-triggered bash) became uncancellable.
//!
//! Runs the real `execute_bash` path against the platform `bash` (same as
//! `tests/suites/shell_tools.rs`). That path resolves its shell through
//! `current_shell_prefs()` -> `agent_dir()`, so each test pins the
//! process-global `PIR_AGENT_DIR` at a fresh, empty directory for its
//! duration (see `HermeticAgentDir`): no ambient
//! `~/.pi/agent/settings.json` (`shellPath` / `shellCommandPrefix` /
//! `shellPool`) is consulted.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;

// ---------------------------------------------------------------------------
// Fixtures (same pattern as tests/suites/agent_session.rs, kept standalone)
// ---------------------------------------------------------------------------

/// Per-test temp working directory.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-bash-abort-set-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serializes the process-global `PIR_AGENT_DIR` pin across the tests in this
/// binary (the env is process-wide and `cargo test` runs the tests in parallel
/// threads of one process).
static AGENT_DIR_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Hermetic agent-dir pin for tests that run the real shell path.
///
/// `execute_bash` resolves its shell through `current_shell_prefs()` ->
/// `agent_dir()`, so an ambient `~/.pi/agent/settings.json` (`shellPath`,
/// `shellCommandPrefix`, `shellPool: false`) would silently change what these
/// tests execute. Holding this guard points the process-global `PIR_AGENT_DIR`
/// at a fresh, empty directory (no settings.json, no trust store) for the
/// test's duration, so the preferences fall back to the built-in defaults, and
/// restores the previous value (and removes the directory) on drop. The env
/// lock is held for the whole test, so no other test in this binary observes
/// the temporary value.
struct HermeticAgentDir {
    /// Dropped after `Drop::drop` restored the env, keeping the restore
    /// serialized with the pin itself.
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<OsString>,
    dir: PathBuf,
}

impl HermeticAgentDir {
    async fn new(label: &str) -> Self {
        let lock = AGENT_DIR_ENV_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!(
            "pir-bash-abort-set-agent-dir-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create pinned agent dir");
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

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
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
        context_window: 4096,
        max_tokens: 256,
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

/// An in-memory session scoped to `cwd` (no provider is ever called: these
/// tests only exercise `execute_bash` / `abort_bash`).
fn session_in(cwd: &Path) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }))
}

/// A command that touches `marker` (relative to the session cwd) and then
/// blocks long enough for the test to abort it.
fn blocker_command(marker: &str) -> String {
    format!("touch {marker}; sleep 30")
}

/// Poll for a side-effect file so cancellation targets a CONFIRMED-running
/// bash registration, not merely a spawned-but-maybe-registered one.
fn wait_for_marker(marker: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if marker.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    marker.exists()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two concurrent `execute_bash` calls; ONE `abort_bash` cancels both
/// (upstream aborts every controller in the Set).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_bash_all_cancelled_by_single_abort() {
    let _agent_dir = HermeticAgentDir::new("both").await;
    let temp = TempDir::new("both");
    let agent = session_in(&temp.0);

    let first = {
        let agent = agent.clone();
        let command = blocker_command("first.started");
        tokio::spawn(async move { agent.execute_bash(&command).await })
    };
    let second = {
        let agent = agent.clone();
        let command = blocker_command("second.started");
        tokio::spawn(async move { agent.execute_bash(&command).await })
    };

    assert!(
        wait_for_marker(&temp.0.join("first.started"), Duration::from_secs(15)),
        "first bash never started (is `bash` on PATH?)"
    );
    assert!(
        wait_for_marker(&temp.0.join("second.started"), Duration::from_secs(15)),
        "second bash never started (is `bash` on PATH?)"
    );

    agent.abort_bash();

    let (first, second) = tokio::join!(first, second);
    let first = first.expect("join first");
    let second = second.expect("join second");
    assert!(first.cancelled, "first concurrent bash was not cancelled");
    assert_eq!(first.exit_code, None, "first bash should have been killed");
    assert!(second.cancelled, "second concurrent bash was not cancelled");
    assert_eq!(
        second.exit_code, None,
        "second bash should have been killed"
    );
}

/// A finished command must not shield a still-running sibling: after the
/// short bash completes (and unregisters itself), `abort_bash` still
/// cancels the long-running one. Under the old single-signal model the
/// short command replaced the signal and the sibling became uncancellable.
#[tokio::test(flavor = "multi_thread")]
async fn finished_bash_keeps_sibling_cancellable() {
    let _agent_dir = HermeticAgentDir::new("sibling").await;
    let temp = TempDir::new("sibling");
    let agent = session_in(&temp.0);

    let long = {
        let agent = agent.clone();
        let command = blocker_command("long.started");
        tokio::spawn(async move { agent.execute_bash(&command).await })
    };
    assert!(
        wait_for_marker(&temp.0.join("long.started"), Duration::from_secs(15)),
        "long bash never started (is `bash` on PATH?)"
    );

    // A short command runs to completion and unregisters its own signal.
    let quick = agent.execute_bash("echo done").await;
    assert!(!quick.cancelled, "quick bash should finish normally");
    assert_eq!(quick.exit_code, Some(0), "quick bash should exit cleanly");

    // The sibling is still cancellable afterwards.
    agent.abort_bash();
    let long = long.await.expect("join long");
    assert!(
        long.cancelled,
        "long bash was not cancelled after sibling finished"
    );
    assert_eq!(long.exit_code, None, "long bash should have been killed");
}

/// Aborting with no active bash is a no-op (empty Set iteration).
#[tokio::test]
async fn abort_with_no_active_bash_is_noop() {
    let _agent_dir = HermeticAgentDir::new("noop").await;
    let temp = TempDir::new("noop");
    let agent = session_in(&temp.0);
    agent.abort_bash();
    agent.abort_bash();

    // The no-op contract: the empty aborts left no stale cancellation behind,
    // so a command started afterwards still runs to completion.
    let result = agent.execute_bash("echo alive").await;
    assert!(
        !result.cancelled,
        "an abort with no active bash must not cancel a later command"
    );
    assert_eq!(result.exit_code, Some(0), "echo should exit cleanly");
    assert_eq!(result.output.trim(), "alive", "output: {:?}", result.output);
}
