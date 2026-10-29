//! W17-A: the `mcp` builtin's config passes read through the manager's root.
//!
//! W16-C closed the manager-side half of the split-brain (env audit §D5): an
//! [`McpManager`] resolves its agent directory ONCE at construction and roots
//! its global `mcp.json`, `mcp.log` and `mcp-auth.json` there, and
//! [`McpManager::load_config_documents`] resolves a caller's request through
//! that same root. Two plugin paths still re-resolved `PIR_AGENT_DIR` live and
//! called `load_mcp_configs` directly, so they could discover `B/mcp.json`
//! while the log and credentials stayed at `A`:
//!
//! * `McpPlugin::sync_config_files` (run from `register()` on every registry
//!   rebuild), and
//! * `restore_config_server` (the `/mcp enable` path after `/mcp remove`).
//!
//! Both now load through [`McpManager::load_config_documents`] and compare the
//! global file against the manager-resolved root. The contract pinned here:
//!
//! * **(a)** the plugin's config pass registers exactly the documents the
//!   manager root holds, with the sources named from that root;
//! * **(b)** a `PIR_AGENT_DIR` change after construction does not move the
//!   plugin's discovery, and the divergence is surfaced (manager log +
//!   retained `configErrors`) instead of silently honoured;
//! * **(c)** in the normal case the parsed configs and the diagnostics are
//!   unchanged: same entries, same sources, same file-order error lines;
//! * **(d)** H19: the plugin stores ONE resolved directory (the same one its
//!   manager is built from), and `/reload` re-roots that directory, the
//!   `mcp.log`/`mcp-auth.json` paths and config discovery together — a live
//!   env move never splits discovery from sign-in state and diagnostics.
//!
//! Hermetic: temp directories only, fixtures are stdio entries that are parsed
//! but never spawned, and every manager is built with an explicit temp root, so
//! no real `~/.pi` is read or written. `PIR_AGENT_DIR` is process-wide, so each
//! test holds the shared [`crate::common::AgentDirEnvGuard`].

#![cfg(feature = "mcp")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pir::config::mcp::McpServerConfig;
use pir::mcp::registry::McpServerSource;
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A unique temporary directory removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(label: &str) -> Tree {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pir-w17a-{label}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tree(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn global_config(&self) -> PathBuf {
        self.path().join("mcp.json")
    }

    /// Write the whole `<dir>/mcp.json` fixture (wrapper shape unless the
    /// caller picks another shape).
    fn write_json(&self, document: serde_json::Value) {
        std::fs::write(
            self.global_config(),
            serde_json::to_vec_pretty(&document).expect("serialize fixture"),
        )
        .expect("write fixture mcp.json");
    }

    /// Write a one-server wrapper-shaped `mcp.json`.
    fn write_config(&self, server: &str, command: &str) {
        self.write_json(serde_json::json!({
            "mcpServers": { server: { "command": command } }
        }));
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

/// Build the plugin around an explicit manager root and run its first
/// `register()` pass. `PluginHost::build(None, ..)` keeps the inline plugin
/// (no JS plan to re-root it), so the manager built here is the one that
/// serves the pass.
fn build_plugin(root: &Path, cwd: &Path) -> (Arc<McpPlugin>, PluginHost) {
    let plugin = Arc::new(McpPlugin::with_cwd(root, cwd));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    (plugin, host)
}

fn stdio_command(config: &McpServerConfig) -> &str {
    match config {
        McpServerConfig::Stdio(stdio) => &stdio.command,
        McpServerConfig::Http(_) => unreachable!("fixture must stay a stdio entry"),
    }
}

fn is_rooted_diagnostic(line: &str) -> bool {
    line.contains("rooted at")
}

// ---------------------------------------------------------------------------
// (a) + (b): the config pass asks the manager for its root
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn the_config_pass_reads_the_manager_root_not_a_live_env_move() {
    let root = Tree::new("config-root");
    let live = Tree::new("config-live");
    root.write_config("alpha", "pir-w17a-root-alpha");
    live.write_config("beta", "pir-w17a-live-beta");

    // Construction-time resolution: the manager is rooted at `root`.
    let guard = crate::common::AgentDirEnvGuard::at(root.path(), &[]).await;
    let plugin = Arc::new(McpPlugin::with_cwd(root.path(), root.path()));
    assert_eq!(
        plugin.manager().agent_dir(),
        root.path(),
        "the manager keeps its construction-time root"
    );

    // The change that used to split the two: a later live env read.
    std::env::set_var("PIR_AGENT_DIR", live.path());
    assert_eq!(
        pir::coding_agent::context::agent_dir().as_path(),
        live.path(),
        "the env change must be visible to a live read"
    );

    // The plugin's config pass runs inside `PluginHost::build`.
    let _host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);

    // (a) Discovery came from the manager's root, with the source named from
    // it — not from the live directory.
    let entry = plugin
        .manager()
        .registry()
        .get("alpha")
        .expect("alpha must be discovered from the manager root");
    assert_eq!(
        entry.source,
        McpServerSource::GlobalConfig(root.global_config()),
        "the global source must be <manager root>/mcp.json"
    );
    assert_eq!(stdio_command(&entry.config), "pir-w17a-root-alpha");
    assert!(
        plugin.manager().registry().get("beta").is_none(),
        "config discovery followed the live env (split-brain)"
    );

    // (b) The divergent request is surfaced, not silently honoured: the
    // loader's line is retained for status/configErrors and written to the
    // manager's own log.
    let errors = plugin.manager().config_errors();
    assert!(
        errors.iter().any(|line| is_rooted_diagnostic(line)),
        "the divergent request was not reported: {errors:?}"
    );
    let log = root.read("mcp.log");
    assert!(
        log.contains("rooted at"),
        "divergence was not logged to the root: {log:?}"
    );
    assert!(
        log.contains(&root.global_config().display().to_string()),
        "the root log does not name the root's config: {log:?}"
    );
    assert!(
        !live.contains_file("mcp.log"),
        "diagnostics moved with the live env"
    );

    plugin.manager().shutdown().await;
    drop(guard);
}

// ---------------------------------------------------------------------------
// (c) The normal case is unchanged: same parsed configs, same ordering
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn the_normal_case_registers_exactly_the_parsed_documents() {
    let root = Tree::new("normal");
    // The wrapper shape exercises the flags this pass exists for: the
    // `builtinTools` merge (per-server applicability) and one file-level
    // error line whose position in `configErrors` must not move.
    root.write_json(serde_json::json!({
        "mcpServers": {
            "alpha": { "command": "pir-w17a-normal-alpha" },
            "beta": { "command": "pir-w17a-normal-beta", "exposure": "deferred" },
            "broken": { "commnd": "typo" }
        },
        "builtinTools": false
    }));

    let guard = crate::common::AgentDirEnvGuard::at(root.path(), &[]).await;
    let (plugin, _host) = build_plugin(root.path(), root.path());

    // No divergence: the request IS the manager root.
    let errors = plugin.manager().config_errors();
    assert!(
        !errors.iter().any(|line| is_rooted_diagnostic(line)),
        "an unchanged directory must not be reported as divergent: {errors:?}"
    );

    // The pass registered exactly what the loader parses from that file —
    // same names, same configs, same source.
    let loaded = pir::config::mcp::load_mcp_configs(root.path(), root.path(), false);
    assert_eq!(loaded.len(), 1, "one global document");
    assert_eq!(loaded[0].path, root.global_config());
    assert_eq!(
        loaded[0].servers.len(),
        2,
        "the invalid entry is not a server: {loaded:?}"
    );
    for (name, config) in &loaded[0].servers {
        let entry = plugin
            .manager()
            .registry()
            .get(name)
            .unwrap_or_else(|| panic!("server {name} was parsed but not registered"));
        assert_eq!(&entry.config, config, "parsed config changed for {name}");
        assert_eq!(
            entry.source,
            McpServerSource::GlobalConfig(root.global_config()),
            "source changed for {name}"
        );
    }
    assert_eq!(
        plugin.manager().registry().list().len(),
        loaded[0].servers.len(),
        "a server was registered that the file does not define"
    );
    assert!(
        !plugin.builtin_tools(),
        "the file-level flag stopped reaching the session state"
    );

    // Diagnostics: the per-file error, prefixed with the path it came from,
    // in file order and exactly once.
    assert_eq!(errors.len(), 1, "unexpected diagnostics: {errors:?}");
    let expected_prefix = format!("{}: ", root.global_config().display());
    assert!(
        errors[0].starts_with(&expected_prefix),
        "the file error lost its path prefix: {errors:?}"
    );
    assert!(
        errors[0].contains("broken"),
        "the file error does not name the entry: {errors:?}"
    );

    plugin.manager().shutdown().await;
    drop(guard);
}

// ---------------------------------------------------------------------------
// The `/mcp enable` restore path asks the manager for its root too
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn the_enable_path_restores_from_the_manager_root() {
    let root = Tree::new("restore-root");
    let live = Tree::new("restore-live");
    root.write_config("alpha", "pir-w17a-root-alpha");
    // The live directory defines the same name with a different command: a
    // restore that followed the env would register this one.
    live.write_config("alpha", "pir-w17a-live-alpha");

    let guard = crate::common::AgentDirEnvGuard::at(root.path(), &[]).await;
    let (plugin, host) = build_plugin(root.path(), root.path());
    assert!(plugin.manager().registry().get("alpha").is_some());

    // `/mcp remove` clears the registry entry and records the session
    // removal, so `/mcp enable` takes the `restore_config_server` path.
    let outcome = host
        .dispatch_command("mcp", "remove alpha")
        .await
        .expect("native command");
    assert!(outcome.ok, "{outcome:?}");
    assert!(plugin.manager().registry().get("alpha").is_none());

    // Move the env only now: the manager's root was already captured.
    std::env::set_var("PIR_AGENT_DIR", live.path());
    assert_eq!(
        pir::coding_agent::context::agent_dir().as_path(),
        live.path()
    );

    let outcome = host
        .dispatch_command("mcp", "enable alpha")
        .await
        .expect("native command");
    assert!(
        outcome.ok,
        "restore must still find the root's entry: {outcome:?}"
    );

    let entry = plugin
        .manager()
        .registry()
        .get("alpha")
        .expect("alpha restored from the manager root");
    assert_eq!(
        entry.source,
        McpServerSource::GlobalConfig(root.global_config()),
        "the restored source must be the manager root's file"
    );
    assert_eq!(
        stdio_command(&entry.config),
        "pir-w17a-root-alpha",
        "the enable path restored the live directory's definition (split-brain)"
    );

    // The divergence is surfaced on this path as well.
    let errors = plugin.manager().config_errors();
    assert!(
        errors.iter().any(|line| is_rooted_diagnostic(line)),
        "the divergent request was not retained: {errors:?}"
    );
    assert!(
        root.read("mcp.log").contains("rooted at"),
        "the divergence was not written to the manager log"
    );
    assert!(
        !live.contains_file("mcp.log"),
        "the restore path moved diagnostics to the live env"
    );

    plugin.manager().shutdown().await;
    drop(guard);
}

// ---------------------------------------------------------------------------
// H19: one resolved dir at construction, and `/reload` re-roots both faces
// ---------------------------------------------------------------------------

/// The plugin resolves its agent dir ONCE (`with_cwd`) and builds its manager
/// from that same value, so config discovery, `mcp.log` and `mcp-auth.json`
/// all come from one root even when `PIR_AGENT_DIR` moves afterwards. A reload
/// is the ONE place a move takes effect, and it must move both faces together.
///
/// This pins the regression the env audit §D5/C5 named: `on_reload` used to
/// call only `manager.shutdown()`, so after a reload the plugin re-read the
/// live environment for discovery while the manager kept logging and reading
/// credentials from its construction-time directory (servers from `B`,
/// sign-in state and diagnostics in `A`).
#[tokio::test(flavor = "current_thread")]
async fn reload_reroots_config_log_and_credentials_together() {
    let first = Tree::new("reroot-first");
    let second = Tree::new("reroot-second");
    first.write_config("alpha", "pir-h19-first-alpha");
    second.write_config("beta", "pir-h19-second-beta");

    let guard = crate::common::AgentDirEnvGuard::at(first.path(), &[]).await;

    // The explicit construction seam: the plugin stores this one resolution
    // and hands the same value to its manager.
    let plugin = Arc::new(McpPlugin::with_cwd(first.path(), first.path()));
    assert_eq!(plugin.manager().agent_dir(), first.path());
    assert_eq!(
        plugin.manager().oauth_store().path(),
        first.path().join("mcp-auth.json").as_path(),
        "the credential store must be derived from the resolved root"
    );

    // Move the LIVE environment BEFORE the first config pass. The plugin must
    // not follow it: discovery, log and credentials all stay on `first`.
    std::env::set_var("PIR_AGENT_DIR", second.path());
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);

    let entry = plugin
        .manager()
        .registry()
        .get("alpha")
        .expect("config discovery must read the plugin's resolved dir");
    assert_eq!(
        entry.source,
        McpServerSource::GlobalConfig(first.global_config()),
        "discovery followed the live env instead of the resolved dir"
    );
    assert!(
        plugin.manager().registry().get("beta").is_none(),
        "the live env's server leaked in (split-brain)"
    );
    assert_eq!(plugin.manager().agent_dir(), first.path());
    assert_eq!(
        plugin.manager().oauth_store().path(),
        first.path().join("mcp-auth.json").as_path()
    );
    plugin
        .manager()
        .log()
        .write_event("h19", "before-reload-marker");
    assert!(
        first.read("mcp.log").contains("before-reload-marker"),
        "the manager log is not the resolved root's log"
    );
    assert!(
        !second.contains_file("mcp.log"),
        "log output moved with the live env before any reload"
    );

    // `/reload` is the one place a move takes effect. The native hook and the
    // registry rebuild it triggers must re-root BOTH faces from one read.
    host.reload_native_plugins().await;

    assert_eq!(
        plugin.manager().agent_dir(),
        second.path(),
        "reload did not re-root the manager"
    );
    assert_eq!(
        plugin.manager().oauth_store().path(),
        second.path().join("mcp-auth.json").as_path(),
        "credentials stayed on the old root after reload"
    );
    assert!(
        plugin.manager().registry().get("alpha").is_none(),
        "the old root's server survived the re-root"
    );
    let entry = plugin
        .manager()
        .registry()
        .get("beta")
        .expect("config discovery must read the new root after reload");
    assert_eq!(
        entry.source,
        McpServerSource::GlobalConfig(second.global_config()),
        "discovery and the manager root diverged after reload"
    );
    assert_eq!(stdio_command(&entry.config), "pir-h19-second-beta");

    plugin
        .manager()
        .log()
        .write_event("h19", "after-reload-marker");
    assert!(
        second.read("mcp.log").contains("after-reload-marker"),
        "diagnostics did not move to the new root"
    );
    assert!(
        !first.read("mcp.log").contains("after-reload-marker"),
        "diagnostics stayed on the old root after reload (split-brain)"
    );

    plugin.manager().shutdown().await;
    drop(guard);
}
