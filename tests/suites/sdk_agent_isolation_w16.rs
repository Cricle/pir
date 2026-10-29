//! W16-B — SDK agent-directory identity (audit §D2) and build-bound session
//! roots (§D9).
//!
//! Contracts pinned here:
//!
//! * **(a) Two agents, two directories.** `AgentBuilder::agent_dir` gives each
//!   agent its own config directory: the settings (`settings.json`) and the
//!   stored credentials (`auth.json`) resolved for a build come from THAT
//!   directory, so two agents built in one process with different directories
//!   resolve different models.
//! * **(b) No cache leakage.** A `!command` credential resolved for agent 1 is
//!   cached under `(agent dir, command)`: agent 2, whose directory cannot
//!   produce that value, must not be served agent 1's cached result.
//! * **(c) Default resolution unchanged.** A builder without `.agent_dir(..)`
//!   resolves the process environment exactly like the CLI, and the session
//!   root is `<resolved agent dir>/sessions/...`.
//!
//! The environment-pinned cases take the binary-wide agent-dir lock
//! (`tests/common/mod.rs::AgentDirEnvGuard`), shared with every other
//! agent-dir suite in the merged `g_agent` binary.

use std::path::{Path, PathBuf};

use pir::ai::providers::construction::model;
use pir::sdk::{AgentBuilder, SdkError, SessionMode};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-isolation-w16-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `models.json` declaring one custom provider with two models (the
/// openai-completions wire shape the tests select, never streamed).
fn write_models(agent_dir: &Path, provider: &str, api_key: Option<&str>) {
    let mut entry = serde_json::Map::new();
    entry.insert(
        "api".to_owned(),
        serde_json::Value::String("openai-completions".to_owned()),
    );
    entry.insert(
        "baseUrl".to_owned(),
        serde_json::Value::String("http://127.0.0.1:9/v1".to_owned()),
    );
    entry.insert(
        "models".to_owned(),
        serde_json::json!([{ "id": "iso-a" }, { "id": "iso-b" }]),
    );
    if let Some(api_key) = api_key {
        entry.insert(
            "apiKey".to_owned(),
            serde_json::Value::String(api_key.to_owned()),
        );
    }
    let mut providers = serde_json::Map::new();
    providers.insert(provider.to_owned(), serde_json::Value::Object(entry));
    let config = serde_json::json!({ "providers": providers });
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&config).expect("serialize models.json"),
    )
    .expect("write models.json");
}

/// `settings.json` selecting `defaultProvider`/`defaultModel` without an
/// explicit builder model.
fn write_settings(agent_dir: &Path, provider: &str, model_id: &str) {
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "defaultProvider": provider,
            "defaultModel": model_id
        }))
        .expect("serialize settings.json"),
    )
    .expect("write settings.json");
}

/// `auth.json` with one stored api-key credential.
fn write_auth(agent_dir: &Path, provider: &str) {
    std::fs::write(
        agent_dir.join("auth.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            provider: { "type": "api_key", "key": "stored-in-this-dir" }
        }))
        .expect("serialize auth.json"),
    )
    .expect("write auth.json");
}

fn hermetic_model() -> pir::ai::types::Model {
    model(
        "w16-hermetic",
        "w16-hermetic-model",
        "w16-hermetic-model",
        "openai-chat",
        "http://127.0.0.1:9/v1",
        128_000,
        8_192,
        vec!["text"],
    )
}

/// An agent with no model resolution at all (a caller-supplied registry), so
/// only the directory plumbing is exercised.
fn hermetic_builder(agent_dir: &Path) -> AgentBuilder {
    AgentBuilder::new()
        .agent_dir(agent_dir)
        .with_registry(
            std::sync::Arc::new(pir::ai::provider::ProviderRegistry::default()),
            hermetic_model(),
        )
        .no_compaction()
}

// ---------------------------------------------------------------------------
// (a) Two agents, two directories
// ---------------------------------------------------------------------------

/// The settings scope of each build is the builder's `agent_dir`, so the same
/// `models.json` with different `defaultModel` values resolves differently.
#[tokio::test(flavor = "current_thread")]
async fn agents_with_different_dirs_read_different_settings() {
    let dir_a = TempDir::new("settings-a");
    let dir_b = TempDir::new("settings-b");
    // `PI_OFFLINE=1` keeps the remote-catalog overlays from spawning refresh
    // tasks (the directory pin itself is irrelevant to these builds: both
    // agents carry an explicit `.agent_dir(..)`). Set it once without holding
    // the binary-wide PIR_AGENT_DIR lock across the builds.
    crate::common::ensure_offline_mode();
    for dir in [dir_a.path(), dir_b.path()] {
        write_models(dir, "w16iso", Some("literal-key"));
        write_auth(dir, "w16iso");
    }
    write_settings(dir_a.path(), "w16iso", "iso-a");
    write_settings(dir_b.path(), "w16iso", "iso-b");

    let agent_a = AgentBuilder::new()
        .agent_dir(dir_a.path())
        .no_session()
        .no_compaction()
        .build()
        .await
        .expect("build agent A");
    let agent_b = AgentBuilder::new()
        .agent_dir(dir_b.path())
        .no_session()
        .no_compaction()
        .build()
        .await
        .expect("build agent B");

    assert_eq!(agent_a.agent_dir(), dir_a.path());
    assert_eq!(agent_b.agent_dir(), dir_b.path());
    assert_eq!(
        (
            agent_a.model().provider.as_str(),
            agent_a.model().id.as_str()
        ),
        ("w16iso", "iso-a"),
        "agent A must resolve the defaultModel of ITS directory"
    );
    assert_eq!(
        (
            agent_b.model().provider.as_str(),
            agent_b.model().id.as_str()
        ),
        ("w16iso", "iso-b"),
        "agent B must resolve the defaultModel of ITS directory"
    );
}

/// `auth.json` is read from the build's directory: the same `models.json` and
/// the same `settings.json` resolve a model only where the credential exists.
#[tokio::test(flavor = "current_thread")]
async fn agents_with_different_dirs_read_different_auth() {
    let dir_a = TempDir::new("auth-a");
    let dir_b = TempDir::new("auth-b");
    crate::common::ensure_offline_mode();
    for dir in [dir_a.path(), dir_b.path()] {
        write_models(dir, "w16auth", None);
        write_settings(dir, "w16auth", "iso-a");
    }
    // Only A stores the credential.
    write_auth(dir_a.path(), "w16auth");

    let agent_a = AgentBuilder::new()
        .agent_dir(dir_a.path())
        .no_session()
        .no_compaction()
        .build()
        .await
        .expect("agent A has a stored credential");
    assert_eq!(
        (
            agent_a.model().provider.as_str(),
            agent_a.model().id.as_str()
        ),
        ("w16auth", "iso-a"),
        "the stored credential of A's auth.json must make the saved default usable"
    );

    // B's directory has no credential: the saved default is unusable, so an
    // environment-authed provider might be picked instead — never `w16auth`.
    match AgentBuilder::new()
        .agent_dir(dir_b.path())
        .no_session()
        .no_compaction()
        .build()
        .await
    {
        Ok(agent_b) => assert_ne!(
            agent_b.model().provider,
            "w16auth",
            "B must not see A's auth.json"
        ),
        Err(SdkError::NoModel { .. }) => {}
        Err(other) => panic!("unexpected build error for B: {other}"),
    }
}

// ---------------------------------------------------------------------------
// (b) A cache populated by agent 1 does not leak to agent 2
// ---------------------------------------------------------------------------

/// A `!command` credential is cached per `(agent dir, command)`. The same
/// command text is declared in two directories; only the first can produce a
/// value (its `key.txt` exists and the process env points at it while it is
/// built). A command-only cache would serve agent 1's value to agent 2.
#[tokio::test(flavor = "current_thread")]
async fn command_credential_cache_does_not_leak_between_agents() {
    let dir_a = TempDir::new("cache-a");
    let dir_b = TempDir::new("cache-b");
    // One shared command text; its OUTPUT depends on the live agent dir.
    //
    // `!command` credentials execute like upstream `executeCommandUncached`
    // (`resolve-config-value.ts:198-206`): on Windows through the CONFIGURED
    // shell (Git Bash here), falling back to the default shell only when that
    // shell cannot be STARTED; elsewhere through the POSIX default shell.
    // Both are POSIX shells, so one quoted `$VAR` expansion works on every
    // platform — no `cmd`-specific `%VAR%` / `type` syntax.
    let command = "!cat \"$PIR_AGENT_DIR/key.txt\"";
    for dir in [dir_a.path(), dir_b.path()] {
        write_models(dir, "w16cmd", Some(command));
        write_settings(dir, "w16cmd", "iso-a");
    }
    // Only A has the file the command reads.
    std::fs::write(dir_a.path().join("key.txt"), "command-key").expect("write key.txt");

    // Agent 1: the environment and the builder agree on A.
    {
        let _env = crate::common::AgentDirEnvGuard::at(dir_a.path(), &[("PI_OFFLINE", "1")]).await;
        let agent_a = AgentBuilder::new()
            .agent_dir(dir_a.path())
            .no_session()
            .no_compaction()
            .build()
            .await
            .expect("agent A resolves its command credential");
        assert_eq!(
            agent_a.model().provider,
            "w16cmd",
            "the command credential of A must make A's saved default usable"
        );
    }

    // Agent 2: same command text, a directory that cannot produce it.
    {
        let _env = crate::common::AgentDirEnvGuard::at(dir_b.path(), &[("PI_OFFLINE", "1")]).await;
        match AgentBuilder::new()
            .agent_dir(dir_b.path())
            .no_session()
            .no_compaction()
            .build()
            .await
        {
            Ok(agent_b) => assert_ne!(
                agent_b.model().provider,
                "w16cmd",
                "agent 2 must not be served agent 1's cached `!command` result"
            ),
            Err(SdkError::NoModel { .. }) => {}
            Err(other) => panic!("unexpected build error for B: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// (c) The default builder resolves the environment like today
// ---------------------------------------------------------------------------

/// Without `.agent_dir(..)` the agent keeps the process resolution, and the
/// default session root sits under it.
#[tokio::test(flavor = "current_thread")]
async fn default_builder_resolves_the_process_agent_dir() {
    let env_dir = TempDir::new("default-env");
    let _env = crate::common::AgentDirEnvGuard::at(env_dir.path(), &[("PI_OFFLINE", "1")]).await;

    let agent = AgentBuilder::new()
        .with_registry(
            std::sync::Arc::new(pir::ai::provider::ProviderRegistry::default()),
            hermetic_model(),
        )
        .no_compaction()
        .build()
        .await
        .expect("hermetic build");

    assert_eq!(
        agent.agent_dir(),
        pir::ai::providers::agent_dir(),
        "the default builder must keep resolving the process agent dir"
    );
    assert_eq!(agent.agent_dir(), env_dir.path());
    let session_dir = agent
        .session_dir()
        .expect("the default session mode has a directory");
    assert!(
        session_dir.starts_with(env_dir.path().join("sessions")),
        "the default session root must live under the resolved agent dir, got {}",
        session_dir.display()
    );
}

/// D9: the session root is build-bound — it follows the agent's OWN directory,
/// not the process environment, and an environment change after the build does
/// not re-point it. `attach_session` remains the explicit escape hatch.
#[tokio::test(flavor = "current_thread")]
async fn session_root_is_build_bound_to_the_agents_own_dir() {
    let agent_dir = TempDir::new("root-agent");
    let other_dir = TempDir::new("root-other");

    let agent = hermetic_builder(agent_dir.path())
        .session(SessionMode::DefaultDir)
        .build()
        .await
        .expect("build agent");

    let bound = agent
        .session_dir()
        .expect("persistent agent has a session dir")
        .to_path_buf();
    assert!(
        bound.starts_with(agent_dir.path().join("sessions")),
        "the session root must come from the builder's agent_dir, got {}",
        bound.display()
    );

    // A later environment change must not re-point the built agent.
    let _env = crate::common::AgentDirEnvGuard::at(other_dir.path(), &[("PI_OFFLINE", "1")]).await;
    agent.new_session().await.expect("new session still works");
    assert_eq!(
        agent.session_dir(),
        Some(bound.as_path()),
        "the session root is build-bound (D9)"
    );
    assert!(
        agent
            .session_path()
            .is_some_and(|path| path.starts_with(agent_dir.path().join("sessions"))),
        "a new session file must be created under the bound root"
    );
}

// ---------------------------------------------------------------------------
// (d) `!command` HEADER values follow the explicit agent dir
// ---------------------------------------------------------------------------

/// A provider header `!command` is cached under `(agent dir, command)` and
/// invalidated by THAT dir's config stamps. The ambient `resolve_headers`
/// keys it under the process env's dir, so an SDK agent built with
/// `.agent_dir(A)` while the env names B would keep serving a stale header
/// after A's `models.json` changed. The SDK loader calls
/// `merge_pi_config_in(A, ..)`, whose header pass is `resolve_headers_in(A,
/// ..)`; pin that contract directly.
#[test]
fn command_header_values_are_cached_under_the_explicit_agent_dir() {
    let dir = TempDir::new("header-cache");
    let models_json = dir.path().join("models.json");
    let header_txt = dir.path().join("header.txt");
    std::fs::write(&models_json, "{}").expect("write models.json");
    std::fs::write(&header_txt, "one").expect("write header");

    // A literal path keeps the command independent of the process
    // environment; the point under test is the CACHE KEY/config stamp, not
    // the shell's env.
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "x-test-header".to_string(),
        format!("!cat \"{}\"", header_txt.display()),
    );

    let resolve = || {
        pir::ai::providers::config_value::resolve_headers_in(dir.path(), Some(&headers), None)
            .expect("header resolves")
    };

    assert_eq!(
        resolve().get("x-test-header").map(String::as_str),
        Some("one")
    );

    // The command output changes, but the agent dir's config stamp does not:
    // the cache still serves the first value.
    std::fs::write(&header_txt, "two").expect("rewrite header");
    assert_eq!(
        resolve().get("x-test-header").map(String::as_str),
        Some("one"),
        "an unchanged agent-dir config serves the cached command result"
    );

    // Editing the agent dir's `models.json` invalidates the entry and the
    // command re-runs against the new file content.
    std::fs::write(&models_json, "{\"x\":1}").expect("rewrite models.json");
    assert_eq!(
        resolve().get("x-test-header").map(String::as_str),
        Some("two"),
        "a config stamp change must re-run the command under the explicit dir"
    );
}
