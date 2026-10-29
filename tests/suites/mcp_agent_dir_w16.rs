//! W16-C: the MCP agent-dir split-brain (env audit §D5).
//!
//! Before this lane, a manager's log and OAuth credential store were captured
//! from the agent dir at construction, while config discovery could use a
//! later, live resolution of the same environment variable. After an agent-dir
//! change, servers came from `B/mcp.json` while their sign-in state and
//! diagnostics went to `A` (the audit's reproduction).
//!
//! The manager is now the single source of truth: [`McpManager::new`] resolves
//! the root once and derives the global `mcp.json`, `mcp.log` and
//! `mcp-auth.json` from it, and [`McpManager::load_config_files`] resolves a
//! caller's requested directory through the same root instead of accepting a
//! second one. The runtime-change policy is documented in the module
//! (`src/mcp/manager.rs`, "One root per manager"): a live manager keeps its
//! construction-time root and reports a divergent request; a new root takes
//! effect through a new manager.
//!
//! These tests pin that behavior:
//!
//! * **(a)** config discovery, the log and the credential store come from the
//!   same resolved directory;
//! * **(b)** changing `PIR_AGENT_DIR` after construction does not silently
//!   split configuration from the log/credentials — the divergence is
//!   recorded;
//! * **(c)** a second manager for another directory shares none of the first
//!   one's files.
//!
//! Hermetic: temp directories only, no server process is ever spawned (the
//! fixtures are stdio entries that are only parsed, never connected), and no
//! real `~/.pi` is read or written (every manager is built with an explicit
//! temp root; the one rootless manager pins the env at a scratch directory).

#![cfg(feature = "mcp")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pir::mcp::manager::McpManager;
use pir::mcp::registry::McpServerSource;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A unique temporary directory removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(label: &str) -> Tree {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pir-w16c-{label}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("create temp agent dir");
        Tree(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Write `<dir>/mcp.json` declaring one stdio server (never spawned).
    fn write_config(&self, server: &str) {
        let body = serde_json::json!({
            "mcpServers": {
                server: { "command": format!("pir-w16c-{server}") }
            }
        });
        let bytes = serde_json::to_vec_pretty(&body).expect("serialize fixture");
        std::fs::write(self.path().join("mcp.json"), bytes).expect("write fixture mcp.json");
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.path().join(file)).unwrap_or_default()
    }

    fn contains_file(&self, file: &str) -> bool {
        self.path().join(file).exists()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// (a) One resolved directory for config, log and credentials
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn config_log_and_credentials_share_one_resolved_root() {
    let root = Tree::new("shared-root");
    let other = Tree::new("shared-other");
    root.write_config("alpha");
    other.write_config("beta");

    // The explicit construction directory is the root: nothing here reads the
    // process env.
    let manager = McpManager::new(root.path(), root.path());
    assert_eq!(manager.agent_dir(), root.path());
    assert_eq!(
        manager.oauth_store().path(),
        root.path().join("mcp-auth.json").as_path(),
        "the credential store must sit in the manager root"
    );

    // Config discovery through the manager resolves EVERY request through the
    // same root.
    assert_eq!(
        manager.resolve_config_root(other.path()).as_path(),
        root.path()
    );
    assert_eq!(
        manager.resolve_config_root(root.path()).as_path(),
        root.path()
    );

    let errors = manager.load_config_files(root.path(), false);
    assert!(errors.is_empty(), "unexpected config errors: {errors:?}");

    let entry = manager
        .registry()
        .get("alpha")
        .expect("alpha must be discovered from the root");
    assert_eq!(
        entry.source,
        McpServerSource::GlobalConfig(root.path().join("mcp.json")),
        "the global source must be <root>/mcp.json"
    );
    assert!(
        manager.registry().get("beta").is_none(),
        "config leaked in from a different directory"
    );

    // The log the manager owns is the root's log.
    manager.log().write_event("w16c", "shared-root-marker");
    let log = root.read("mcp.log");
    assert!(
        log.contains("registered from"),
        "registration diagnostics did not reach the root log: {log:?}"
    );
    assert!(
        log.contains("shared-root-marker"),
        "root log is not the one in use: {log:?}"
    );
    assert!(
        !other.contains_file("mcp.log"),
        "log written outside the root"
    );
}

// ---------------------------------------------------------------------------
// (b) A live env change does not silently split the two
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn changing_the_agent_dir_env_does_not_split_the_manager() {
    let first = Tree::new("env-first");
    let second = Tree::new("env-second");
    first.write_config("first");
    second.write_config("second");

    // The plugin host's construction-time resolution, pinned hermetically.
    let guard = crate::common::AgentDirEnvGuard::at(first.path(), &[]).await;
    let resolved = pir::coding_agent::context::agent_dir();
    assert_eq!(resolved.as_path(), first.path());
    let manager = McpManager::new(&resolved, first.path());
    assert_eq!(manager.agent_dir(), first.path());

    // The change that used to split the state: a later live read of the env.
    std::env::set_var("PIR_AGENT_DIR", second.path());
    let live = pir::coding_agent::context::agent_dir();
    assert_eq!(
        live.as_path(),
        second.path(),
        "the env change must be visible to a live read"
    );

    // A post-change config pass, exactly like the plugin's `register` doing
    // `load_config_files(&context::agent_dir(), ..)`.
    let errors = manager.load_config_files(&live, false);
    assert!(
        manager.registry().get("first").is_some(),
        "config discovery left the manager root"
    );
    assert!(
        manager.registry().get("second").is_none(),
        "config discovery followed the live env (split-brain)"
    );
    assert!(
        errors.iter().any(|error| error.contains("rooted at")),
        "the divergent request was not reported: {errors:?}"
    );

    // Log and credentials stayed with the config on the same root.
    manager.log().write_event("w16c", "split-check-marker");
    let log = first.read("mcp.log");
    assert!(log.contains("split-check-marker"));
    assert!(
        log.contains("rooted at"),
        "divergence was not logged: {log:?}"
    );
    assert_eq!(
        manager.oauth_store().path(),
        first.path().join("mcp-auth.json").as_path()
    );
    assert!(
        !second.contains_file("mcp.log"),
        "log moved with the live env"
    );

    drop(guard);
}

// ---------------------------------------------------------------------------
// (c) A second manager for another directory shares no files
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_second_manager_for_another_dir_shares_no_files() {
    let left = Tree::new("second-left");
    let right = Tree::new("second-right");
    left.write_config("alpha");
    right.write_config("beta");

    let left_manager = McpManager::new(left.path(), left.path());
    let right_manager = McpManager::new(right.path(), right.path());
    assert_ne!(
        left_manager.oauth_store().path(),
        right_manager.oauth_store().path(),
        "managers for different dirs must not share the credential store"
    );

    left_manager.load_config_files(left.path(), false);
    right_manager.load_config_files(right.path(), false);

    assert!(left_manager.registry().get("alpha").is_some());
    assert!(left_manager.registry().get("beta").is_none());
    assert!(right_manager.registry().get("beta").is_some());
    assert!(right_manager.registry().get("alpha").is_none());

    left_manager
        .log()
        .write_event("w16c", "left-manager-marker");
    right_manager
        .log()
        .write_event("w16c", "right-manager-marker");

    let left_log = left.read("mcp.log");
    let right_log = right.read("mcp.log");
    assert!(left_log.contains("left-manager-marker"));
    assert!(!left_log.contains("right-manager-marker"));
    assert!(right_log.contains("right-manager-marker"));
    assert!(!right_log.contains("left-manager-marker"));

    // Each log names only its own config file.
    let left_config = left.path().join("mcp.json").display().to_string();
    let right_config = right.path().join("mcp.json").display().to_string();
    assert!(left_log.contains(&left_config));
    assert!(!left_log.contains(&right_config));
    assert!(right_log.contains(&right_config));
    assert!(!right_log.contains(&left_config));
}

// ---------------------------------------------------------------------------
// The config-pass seam (documents, not registrations)
// ---------------------------------------------------------------------------

/// The pass that needs the per-file flags (the `mcp` builtin's config sync)
/// reads the documents through the manager too, so it cannot be the one path
/// that discovers from a second directory.
#[tokio::test(flavor = "current_thread")]
async fn the_config_pass_seam_loads_documents_from_the_manager_root() {
    let root = Tree::new("seam-root");
    let live = Tree::new("seam-live");
    root.write_config("alpha");
    live.write_config("beta");

    let manager = McpManager::new(root.path(), root.path());
    let (documents, diagnostics) = manager.load_config_documents(live.path(), false);

    let paths: Vec<PathBuf> = documents.iter().map(|file| file.path.clone()).collect();
    assert_eq!(paths, vec![root.path().join("mcp.json")]);
    assert!(documents
        .iter()
        .any(|file| file.servers.contains_key("alpha")));
    assert!(documents
        .iter()
        .all(|file| !file.servers.contains_key("beta")));
    assert!(
        diagnostics.iter().any(|line| line.contains("rooted at")),
        "the divergent request was not reported: {diagnostics:?}"
    );
}

// ---------------------------------------------------------------------------
// A manager without a root keeps its documented fallback
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_rootless_manager_follows_the_requested_dir() {
    let dir = Tree::new("rootless");
    dir.write_config("loose");
    // `without_log` builds the default credential store from the env; pin it at
    // a scratch dir so no real `~/.pi` is even named.
    let _guard = crate::common::AgentDirEnvGuard::scratch("w16c-rootless", None).await;

    let manager = McpManager::without_log(dir.path());
    assert!(manager.agent_dir().as_os_str().is_empty());
    assert_eq!(
        manager.resolve_config_root(dir.path()).as_path(),
        dir.path()
    );

    let errors = manager.load_config_files(dir.path(), false);
    assert!(errors.is_empty(), "unexpected config errors: {errors:?}");
    assert!(manager.registry().get("loose").is_some());
}
