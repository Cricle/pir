//! The opt-in `tools.commandTimeoutMs` default for foreground shell commands.
//!
//! Tested contract:
//! 1. unset resolves to the pir default (120 s, a deliberate deviation from
//!    upstream's "optional, no default timeout"), and explicit `0` disables
//!    the cap (upstream semantics);
//! 2. an explicit per-call `timeout` always wins over the setting — both
//!    smaller (kills earlier) and larger (is honored);
//! 3. a command exceeding the cap with no per-call timeout is killed (the
//!    existing process tree kill) and the tool returns an error that names
//!    the elapsed limit AND the setting, so the model/user can find the knob;
//! 4. `0` lets a command that outlives what a small default would allow run
//!    to completion;
//! 5. a detached/background call is exempt from the settings default (it was
//!    started to outlive the turn) but an explicit per-call timeout still
//!    caps it.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::coding_agent::register_core_tools;
use pir::coding_agent::tools::resolve_shell_config;
use pir::config::settings::{shell_prefs_from, Settings};

/// Serializes the process-global `PIR_AGENT_DIR` pin (the env is process-wide;
/// this file's tests each pin it to their own scratch directory).
static AGENT_DIR_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pin `PIR_AGENT_DIR` at a scratch directory holding `settings.json`.
struct HermeticAgentDir {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<OsString>,
    dir: PathBuf,
}

impl HermeticAgentDir {
    async fn new(label: &str, settings_json: &str) -> Self {
        let lock = AGENT_DIR_ENV_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!(
            "pir-shell-command-timeout-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create pinned agent dir");
        std::fs::write(dir.join("settings.json"), settings_json).expect("write pinned settings");
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
            "pir-shell-command-timeout-cwd-{label}-{}-{}",
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

async fn run_bash(settings_json: &str, label: &str, args: serde_json::Value) -> (bool, String) {
    let (is_error, content, _elapsed) = run_bash_timed(settings_json, label, args, false).await;
    (is_error, content)
}

/// The same run with the detached/background posture the scheduler sets on an
/// adopted call (`ToolContext::background`).
async fn run_bash_with_background(
    settings_json: &str,
    label: &str,
    args: serde_json::Value,
    background: bool,
) -> (bool, String) {
    let (is_error, content, _elapsed) =
        run_bash_timed(settings_json, label, args, background).await;
    (is_error, content)
}

/// Run and time ONLY the tool call: the harness serializes on the process-wide
/// agent-dir env lock, so a clock started before the fixture would measure the
/// queue behind other tests, not the behavior under test.
async fn run_bash_timed(
    settings_json: &str,
    label: &str,
    args: serde_json::Value,
    background: bool,
) -> (bool, String, std::time::Duration) {
    let _agent_dir = HermeticAgentDir::new(label, settings_json).await;
    let cwd = TempDir::new(label);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let tool = registry.get("bash").expect("bash tool registered");
    let mut context = ToolContext::new(cwd.0.clone());
    context.background = background;
    let started = Instant::now();
    let result = tool.run(&args, &context).await;
    (result.is_error, result.content, started.elapsed())
}

/// Unset applies the pir default (120 s, the deliberate deviation from
/// upstream); explicit `0` restores upstream's no-default-timeout semantics.
#[test]
fn command_timeout_setting_resolves_default_and_zero() {
    let settings = Settings::default();
    let prefs = shell_prefs_from(&settings, None);
    assert_eq!(prefs.command_timeout_ms, None);
    assert_eq!(prefs.command_timeout(), Some(Duration::from_secs(120)));
    assert_eq!(prefs.effective_command_timeout_ms(), 120_000);

    let settings: Settings = serde_json::from_value(serde_json::json!({
        "tools": { "commandTimeoutMs": 1500 }
    }))
    .expect("tools.commandTimeoutMs parses");
    let prefs = shell_prefs_from(&settings, None);
    assert_eq!(prefs.command_timeout_ms, Some(1500));
    assert_eq!(prefs.command_timeout(), Some(Duration::from_millis(1500)));

    // Explicit `0` disables the cap (upstream semantics).
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "tools": { "commandTimeoutMs": 0 }
    }))
    .expect("0 parses");
    let prefs = shell_prefs_from(&settings, None);
    assert_eq!(prefs.command_timeout_ms, Some(0));
    assert!(prefs.command_timeout().is_none());
    assert_eq!(prefs.effective_command_timeout_ms(), 0);
}

/// An omitted per-call timeout uses the configured cap: the command is killed
/// (process tree included) and the error reports the limit plus the setting
/// that supplied it.
#[tokio::test(flavor = "current_thread")]
async fn omitted_timeout_kills_the_command_with_an_honest_error() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let (is_error, content, elapsed) = run_bash_timed(
        r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 300 } }"#,
        "cap",
        serde_json::json!({ "command": "sleep 30" }),
        false,
    )
    .await;

    assert!(
        is_error,
        "a timed-out command is an error result: {content}"
    );
    assert!(
        content.contains("Command timed out after 0.3 seconds"),
        "error names the configured timeout: {content}"
    );
    assert!(
        content.contains("tools.commandTimeoutMs"),
        "error names the setting: {content}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the 30 s command was killed early (elapsed {elapsed:?})"
    );
}

/// An explicit per-call `timeout` wins over the settings default.
#[tokio::test(flavor = "current_thread")]
async fn explicit_timeout_wins_over_the_setting() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let (is_error, content, elapsed) = run_bash_timed(
        r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 5000 } }"#,
        "explicit",
        serde_json::json!({ "command": "sleep 30", "timeout": 0.1 }),
        false,
    )
    .await;

    assert!(
        is_error,
        "a timed-out command is an error result: {content}"
    );
    assert!(
        content.contains("Command timed out after 0.1 seconds"),
        "the per-call timeout is the one reported: {content}"
    );
    assert!(
        !content.contains("tools.commandTimeoutMs"),
        "an explicit timeout is not attributed to the setting: {content}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the per-call timeout killed the 30 s command early (elapsed {elapsed:?})"
    );
}

/// `tools.commandTimeoutMs: 0` restores upstream's no-default-timeout
/// behavior: a command that outlives a small default runs to completion.
#[tokio::test(flavor = "current_thread")]
async fn zero_disables_the_default_timeout() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let (is_error, content) = run_bash(
        r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 0 } }"#,
        "zero",
        serde_json::json!({ "command": "sleep 0.4; echo finished" }),
    )
    .await;
    assert!(
        !is_error,
        "a disabled cap must not kill the command: {content}"
    );
    assert!(
        content.contains("finished"),
        "the command ran to completion: {content}"
    );
}

/// An explicit per-call timeout LARGER than the configured default is honored:
/// the configured cap never shortens a caller's own deadline.
#[tokio::test(flavor = "current_thread")]
async fn explicit_larger_timeout_is_honored() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let (is_error, content, elapsed) = run_bash_timed(
        r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 300 } }"#,
        "explicit-larger",
        serde_json::json!({ "command": "sleep 0.6; echo finished", "timeout": 5 }),
        false,
    )
    .await;

    assert!(
        !is_error,
        "an explicit larger timeout must win over the 300 ms default: {content}"
    );
    assert!(
        content.contains("finished"),
        "the command ran to completion: {content}"
    );
    // Wall-clock only proves it was not killed at the 300 ms setting; the
    // 0.6 s command can take seconds to spawn under a parallel test load.
    assert!(
        elapsed < Duration::from_secs(10),
        "the command finished without hitting either deadline (elapsed {elapsed:?})"
    );
}

/// A detached background call was started to outlive the turn: the settings
/// default must not kill it at 120 s (or at a small configured cap). Only an
/// explicit per-call timeout may cap it.
#[tokio::test(flavor = "current_thread")]
async fn background_calls_skip_the_settings_default() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let (is_error, content) = run_bash_with_background(
        r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 300 } }"#,
        "background-exempt",
        serde_json::json!({ "command": "sleep 0.8; echo finished" }),
        true,
    )
    .await;
    assert!(
        !is_error,
        "the 300 ms setting must not cap a background call: {content}"
    );
    assert!(
        content.contains("finished"),
        "the background command ran to completion: {content}"
    );
}

/// An explicit per-call timeout still applies to a background call: the
/// exemption covers the setting, not the caller's own deadline.
#[tokio::test(flavor = "current_thread")]
async fn background_calls_honor_an_explicit_timeout() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let (is_error, content, elapsed) = run_bash_timed(
        r#"{ "shellPool": false, "tools": { "commandTimeoutMs": 5000 } }"#,
        "background-explicit",
        serde_json::json!({ "command": "sleep 30", "timeout": 0.1 }),
        true,
    )
    .await;
    assert!(
        is_error,
        "an explicit timeout caps a background call: {content}"
    );
    assert!(
        content.contains("Command timed out after 0.1 seconds"),
        "the explicit deadline is the one reported: {content}"
    );
    assert!(
        !content.contains("tools.commandTimeoutMs"),
        "the setting is not attributed: {content}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the explicit timeout killed the 30 s background command early (elapsed {elapsed:?})"
    );
}

/// The post-exit pipe drain is bounded by an absolute cap: a descendant that
/// inherits stdout and keeps writing (a chatty daemon) must not keep the tool
/// call alive past the drain window. The quiet window re-arms on every chunk,
/// so without the cap the call would wait for the writer to finish (~10 s
/// here) instead of returning the moment the command itself exited.
#[tokio::test(flavor = "current_thread")]
async fn a_chatty_inherited_descendant_cannot_extend_the_drain() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    // The writer publishes its pid so the test can kill it after the call
    // returns: leaving a 30 s writer running would load the machine for the
    // timing-sensitive tests that follow.
    let pid_dir = TempDir::new("chatty-pid");
    let pid_file = pid_dir.0.join("writer.pid");
    let (is_error, content, elapsed) = run_bash_timed(
        r#"{ "shellPool": false }"#,
        "chatty-descendant",
        serde_json::json!({
            "command": format!(
                "(echo $$ > '{}'; for i in $(seq 1 600); do echo tick; sleep 0.05; done) & echo started",
                pid_file.to_string_lossy().replace('\\', "/")
            )
        }),
        false,
    )
    .await;
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        if let Ok(shell) = resolve_shell_config(None) {
            let _ = std::process::Command::new(shell.program())
                .args(shell.args())
                .arg(format!("kill -9 {} 2>/dev/null", pid.trim()))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    assert!(!is_error, "the command itself succeeded: {content}");
    assert!(
        content.contains("started"),
        "the command's own output is captured: {content}"
    );
    assert!(
        elapsed < Duration::from_secs(12),
        "the drain must return within the absolute cap, not wait out the writer \
         (~30 s): elapsed {elapsed:?}"
    );
}

/// An abort that fires while a call waits for its pooled member must not start
/// the command afterwards: the post-acquire gate returns the abort result
/// without writing the script (previously the command was written, started,
/// and killed — and the turn could settle after the abort budget).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_while_waiting_for_a_pool_member_starts_nothing() {
    if resolve_shell_config(None).is_err() {
        return; // no shell on this host
    }
    let _agent_dir = HermeticAgentDir::new("abort-pool-wait", r#"{ "shellPool": true }"#).await;
    let cwd = TempDir::new("abort-pool-wait-shared");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let tool = registry.get("bash").expect("bash tool registered");

    // One shared cwd: both calls contend for the same pool member. The holder
    // only has to outlive the target's 250 ms arrival + 100 ms abort (1.5 s
    // leaves >4x margin); the old 3 s sleep was pure wall time.
    let holder = {
        let tool = Arc::clone(&tool);
        let context = ToolContext::new(cwd.0.clone());
        tokio::spawn(async move {
            tool.run(&serde_json::json!({ "command": "sleep 1.5" }), &context)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;

    let signal = pir::ai::types::AbortSignal::new();
    let abort_signal = signal.clone();
    let abort_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        abort_signal.abort();
    });

    let marker = cwd.0.join("target-ran");
    let mut target_context = ToolContext::new(cwd.0.clone());
    target_context.signal = Some(signal);
    let command = format!(
        "touch '{}'; echo ran",
        marker.to_string_lossy().replace('\\', "/")
    );
    let started = Instant::now();
    let result = tool
        .run(&serde_json::json!({ "command": command }), &target_context)
        .await;
    let elapsed = started.elapsed();

    assert!(
        result.is_error,
        "an aborted call is an error: {}",
        result.content
    );
    assert_eq!(result.content, "Command aborted");
    assert!(
        !marker.exists(),
        "the aborted command must never start: {}",
        marker.display()
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "the abort settles once the member is free without running the command (elapsed {elapsed:?})"
    );

    let _ = holder.await;
    abort_task.abort();
}
