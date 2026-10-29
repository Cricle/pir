//! W16-A / env-audit §D1: a live session has an agent-directory IDENTITY.
//!
//! D1 (HIGH): `AgentSession` had no agent-dir field, so every agent-dir read
//! re-resolved the process environment. A mid-process `PIR_AGENT_DIR` change
//! therefore split one live session across two directories: `auth.json`,
//! settings and the sessions root followed the NEW directory while the
//! session's storage, plugin state and telemetry kept the OLD one.
//!
//! The contract pinned here:
//!
//! 1. a session built with directory A keeps reading A's `auth.json`, A's
//!    settings and A's sessions root after the environment moves to B;
//! 2. a session built AFTER the change follows B (fresh-process semantics —
//!    no behavior changes for a normal startup);
//! 3. with no explicit override the directory still follows the environment,
//!    resolved ONCE at build time, so a later change cannot re-point it.
//!
//! Every test holds `crate::common::AgentDirEnvGuard`, the process-wide lock
//! for `PIR_AGENT_DIR` mutations in the merged test binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::{FileAuthStorage, StoredCredential};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{ApiKeyCredential, Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::config::settings::SteeringMode;

/// A scratch directory this module owns and removes on drop (the env guard
/// owns and removes its own pin, but these tests need a SECOND directory for
/// the post-change environment).
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-w16a-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

/// A minimal facade configuration: no session storage, no extensions, no
/// event stream. The system prompt is explicit so the build never runs
/// context-file discovery (which is an ambient read of its own and would make
/// these assertions indirect).
fn config(cwd: &Path) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: Some("w16a agent-dir identity".to_owned()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn write_settings(dir: &Path, steering: &str) {
    std::fs::create_dir_all(dir).expect("create agent dir");
    std::fs::write(
        dir.join("settings.json"),
        format!(r#"{{ "steeringMode": "{steering}" }}"#),
    )
    .expect("write settings.json");
}

fn write_auth(dir: &Path, provider: &str, key: &str) {
    FileAuthStorage::in_agent_dir(dir)
        .store(
            provider,
            StoredCredential::ApiKey(ApiKeyCredential::new(Some(key.to_owned()))),
        )
        .expect("write auth.json");
}

/// The stored api key for `provider` (the OAuth shape is not produced here).
fn stored_key(storage: &FileAuthStorage, provider: &str) -> Option<String> {
    match storage.read_stored(provider).expect("read auth.json")? {
        StoredCredential::ApiKey(credential) => credential.key,
        StoredCredential::OAuth(_) => panic!("expected an api-key credential"),
    }
}

// ---------------------------------------------------------------------------
// (a) the live session keeps its own directory after the env moves
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_agent_dir_keeps_reading_a_after_env_moves_to_b() {
    let guard = crate::common::AgentDirEnvGuard::scratch("w16a-explicit-a", None).await;
    let dir_a = guard.dir().expect("scratch agent dir").to_path_buf();
    write_settings(&dir_a, "all");
    write_auth(&dir_a, "w16a-provider", "key-a");

    let dir_b = ScratchDir::new("w16a-explicit-b");
    write_settings(dir_b.path(), "one-at-a-time");
    write_auth(dir_b.path(), "w16a-provider", "key-b");

    // An explicit identity: the session is pinned to A, not to whatever the
    // environment says later.
    let session = AgentSession::new_with_agent_dir(config(&dir_a), dir_a.as_path());
    assert_eq!(session.agent_dir(), dir_a.as_path());

    // The D1 trigger: the process environment moves to B while the session is
    // live (the guard still restores the original value on drop).
    std::env::set_var("PIR_AGENT_DIR", dir_b.path());
    assert_eq!(
        pir::ai::providers::agent_dir(),
        dir_b.path(),
        "the environment really changed (otherwise this test proves nothing)"
    );

    // Every session-scoped read still resolves A.
    assert_eq!(session.agent_dir(), dir_a.as_path());
    assert_eq!(
        session.session_settings().steering_mode(),
        SteeringMode::All,
        "session settings must come from A, not the live environment"
    );
    assert_eq!(
        stored_key(&session.auth_storage(), "w16a-provider").as_deref(),
        Some("key-a"),
        "session credentials must come from A/auth.json, not B/auth.json"
    );
    assert_eq!(session.sessions_dir(), dir_a.join("sessions"));

    // The override also wins over an environment that ALREADY points
    // elsewhere: building explicitly at A while the process says B yields an
    // A session (an override, not a deferral of env resolution).
    let explicit_a = AgentSession::new_with_agent_dir(config(&dir_a), dir_a.as_path());
    assert_eq!(explicit_a.agent_dir(), dir_a.as_path());
    assert_eq!(
        stored_key(&explicit_a.auth_storage(), "w16a-provider").as_deref(),
        Some("key-a")
    );
}

// ---------------------------------------------------------------------------
// (b) a session built afterwards follows B (fresh-process semantics)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fresh_session_picks_up_the_new_agent_dir() {
    let guard = crate::common::AgentDirEnvGuard::scratch("w16a-fresh-a", None).await;
    let dir_a = guard.dir().expect("scratch agent dir").to_path_buf();
    write_settings(&dir_a, "all");

    let dir_b = ScratchDir::new("w16a-fresh-b");
    write_settings(dir_b.path(), "one-at-a-time");
    write_auth(dir_b.path(), "w16a-provider", "key-b");

    std::env::set_var("PIR_AGENT_DIR", dir_b.path());

    let session = AgentSession::new(config(dir_b.path()));
    assert_eq!(session.agent_dir(), dir_b.path());
    assert_eq!(
        session.session_settings().steering_mode(),
        SteeringMode::OneAtATime
    );
    assert_eq!(
        stored_key(&session.auth_storage(), "w16a-provider").as_deref(),
        Some("key-b")
    );
    assert_eq!(session.sessions_dir(), dir_b.path().join("sessions"));
}

// ---------------------------------------------------------------------------
// (c) no explicit override -> the environment, resolved once at build
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_dir_follows_the_env_and_is_resolved_once() {
    let guard = crate::common::AgentDirEnvGuard::scratch("w16a-default-a", None).await;
    let dir_a = guard.dir().expect("scratch agent dir").to_path_buf();
    write_settings(&dir_a, "all");

    // No override: the build takes the environment as it is right now.
    let session = AgentSession::new(config(&dir_a));
    assert_eq!(session.agent_dir(), dir_a.as_path());
    assert_eq!(
        session.session_settings().steering_mode(),
        SteeringMode::All
    );

    let dir_b = ScratchDir::new("w16a-default-b");
    write_settings(dir_b.path(), "one-at-a-time");
    std::env::set_var("PIR_AGENT_DIR", dir_b.path());

    // Resolved ONCE: the live session is unaffected by the later change.
    assert_eq!(session.agent_dir(), dir_a.as_path());
    assert_eq!(
        session.session_settings().steering_mode(),
        SteeringMode::All
    );
}

// ---------------------------------------------------------------------------
// D6 residual: `/reload` keeps the session's stored agent and package dirs
// ---------------------------------------------------------------------------

/// Like [`config`], but leaves the prompt to the context builder
/// (`system_prompt: None`) so `set_prompt_resources_shared` (the `/reload`
/// seam) actually rebuilds it.
fn context_config(cwd: &Path) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn normalized(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// D6 (second residual): `/reload` used to rebuild the context-derived prompt
/// through `context_system_prompt_with_skills`, whose boundary re-reads
/// `PIR_AGENT_DIR` and `PI_PACKAGE_DIR` from the process environment. That
/// contradicts the D1 pin (`self.agent_dir`) and could split a live session's
/// prompt across two agent/package directories when the environment changed
/// mid-process. The reload now passes [`ContextOptions`] built from the
/// session's stored values.
#[tokio::test]
async fn reload_keeps_the_sessions_agent_and_package_dir() {
    let dir_a = ScratchDir::new("d6-reload-a");
    write_settings(dir_a.path(), "all");
    std::fs::write(dir_a.path().join("AGENTS.md"), "GLOBAL-A-MARKER").expect("write A context");
    let pkg_a = ScratchDir::new("d6-reload-pkg-a");
    std::fs::write(pkg_a.path().join("README.md"), "package A").expect("write A package");
    let pkg_a_env = pkg_a.path().to_string_lossy().into_owned();

    let dir_b = ScratchDir::new("d6-reload-b");
    write_settings(dir_b.path(), "one-at-a-time");
    std::fs::write(dir_b.path().join("AGENTS.md"), "GLOBAL-B-MARKER").expect("write B context");
    let pkg_b = ScratchDir::new("d6-reload-pkg-b");
    std::fs::write(pkg_b.path().join("README.md"), "package B").expect("write B package");
    let pkg_b_env = pkg_b.path().to_string_lossy().into_owned();

    let cwd = ScratchDir::new("d6-reload-cwd");
    // Pin the build-time environment to A for BOTH ambient inputs.
    let _env =
        crate::common::AgentDirEnvGuard::at(dir_a.path(), &[("PI_PACKAGE_DIR", &pkg_a_env)]).await;

    let session = AgentSession::new_with_agent_dir(context_config(cwd.path()), dir_a.path());
    let initial = session
        .system_prompt_snapshot()
        .expect("the global context file produces the context-derived prompt");
    assert!(
        initial.contains("GLOBAL-A-MARKER"),
        "the build reads A's context file: {initial}"
    );
    let docs_a = normalized(&pkg_a.path().join("README.md"));
    assert!(
        initial.contains(&docs_a),
        "the build resolves the package dir once (A): {initial}"
    );

    // The D1 trigger: the environment moves to B while the session is live.
    std::env::set_var("PIR_AGENT_DIR", dir_b.path());
    std::env::set_var("PI_PACKAGE_DIR", &pkg_b_env);
    assert_eq!(
        pir::ai::providers::agent_dir(),
        dir_b.path(),
        "the environment really changed (otherwise this test proves nothing)"
    );

    // `/reload` (resource hot refresh) re-reads context FILES but must keep
    // the session's build-time agent/package identity.
    session.set_prompt_resources_shared(Vec::new(), Vec::new());
    let reloaded = session
        .system_prompt_snapshot()
        .expect("the reload keeps the context-derived prompt");

    assert!(
        reloaded.contains("GLOBAL-A-MARKER"),
        "the reload must keep reading the session's stored agent dir (A): {reloaded}"
    );
    assert!(
        !reloaded.contains("GLOBAL-B-MARKER"),
        "the reload must not adopt the new ambient agent dir (B): {reloaded}"
    );
    assert!(
        reloaded.contains(&docs_a),
        "the reload must keep the build-time package dir (A): {reloaded}"
    );
    assert!(
        !reloaded.contains(&normalized(&pkg_b.path().join("README.md"))),
        "the reload must not adopt the new ambient package dir (B): {reloaded}"
    );
}
