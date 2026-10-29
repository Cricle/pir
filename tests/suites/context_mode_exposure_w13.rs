//! W13-K: `context-mode`'s tools must reach the model, not merely register.
//!
//! The user report was "context-mode 不会使用" (context-mode is never used).
//! The foreman captured a real first request body from a full debug build with
//! a local mock provider (`scripts/perf/body_probe.py --only pir`) and found:
//!
//! ```text
//! tools=9 toolsBytes=7899
//! toolNames=['background_status', 'background_abort', 'ls', 'find', 'write',
//!            'bash', 'read', 'grep', 'edit']
//! ```
//!
//! Nine tools: exactly `register_core_tools`'s set minus `powershell`
//! (registered, but `default_active: false`). Every `ctx_*` tool was absent —
//! and so was every other plugin tool (`todo`, `python`, `tool_search`,
//! `subagent`) — even though `builtin:context-mode` is `default_enabled` and
//! the build carries the `context-mode` feature.
//!
//! The break was never registration: `ContextModePlugin::register` registers
//! all nine tools, and `PluginHost::build` runs it. The break was the MERGE.
//! Print mode builds the unified plugin host (for the MCP reach wait hook and
//! the builtin lifecycle) but its own tool registry only ever received
//! `register_core_tools` + `merge_extension_tools` (the JS face). The Rust
//! builtins' tools therefore stopped at the host's registry, which nothing on
//! the print path reads — `merge_plugin_tools`, the ONE function that carries
//! plugin tools into a run's registry, had exactly one caller
//! (`AgentSession::merged_tool_registry`).
//!
//! These tests pin the restored contract at both ends:
//!
//! - the plugin-host seam: a DEFAULT plan (the real `order_extension_paths`
//!   builtin insertion, not a hand-written path list) yields a host whose
//!   registry resolves all nine `ctx_*` tools as `Direct` + `default_active`,
//!   i.e. ACTIVE — not merely registered;
//! - the merge seam: `merge_plugin_tools` (the function the print host now
//!   calls) makes those nine tools active in the run registry, so
//!   `declared_tool_definitions` — literally the request's `tools` list —
//!   carries them;
//! - the droppability contract is unchanged: `disabledBuiltinExtensions:
//!   ["context-mode"]` and `--no-extensions` still remove them.
//!
//! The tests use only public APIs (`order_extension_paths`,
//! `filter_disabled_builtins`, `PluginHost::build`, `merge_plugin_tools`,
//! `ToolRegistry`) so they do not depend on a built binary.

#![cfg(feature = "context-mode")]

use std::path::PathBuf;
use std::sync::Arc;

use pir::agent::tool_execution::{
    declared_tool_definitions, declared_tools, merge_plugin_tools, AgentTool, ToolExposure,
    ToolRegistry,
};
use pir::extensions::{ExtensionConfig, SessionContextSnapshot};
use pir::plugins::builtin::{
    ContextModePlugin, CTX_BATCH_EXECUTE_TOOL_NAME, CTX_DOCTOR_TOOL_NAME,
    CTX_EXECUTE_FILE_TOOL_NAME, CTX_EXECUTE_TOOL_NAME, CTX_FETCH_AND_INDEX_TOOL_NAME,
    CTX_INDEX_TOOL_NAME, CTX_PURGE_TOOL_NAME, CTX_SEARCH_TOOL_NAME, CTX_STATS_TOOL_NAME,
};
use pir::plugins::{AgentPlugin, PluginHost};

/// The nine tool names `ContextModePlugin::register` installs, in the order
/// the plugin registers them.
const CTX_TOOL_NAMES: [&str; 9] = [
    CTX_EXECUTE_TOOL_NAME,
    CTX_EXECUTE_FILE_TOOL_NAME,
    CTX_BATCH_EXECUTE_TOOL_NAME,
    CTX_INDEX_TOOL_NAME,
    CTX_SEARCH_TOOL_NAME,
    CTX_STATS_TOOL_NAME,
    CTX_PURGE_TOOL_NAME,
    CTX_DOCTOR_TOOL_NAME,
    CTX_FETCH_AND_INDEX_TOOL_NAME,
];

/// A self-cleaning temp directory: the host's cwd, so the `mcp` builtin's
/// config scan never reads this repository's `.pi/mcp.json`.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pir-ctx-exposure-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The host under test plus the things that must outlive it (the extension
/// handle it was built on, and its temp cwd).
struct HostFixture {
    host: Arc<PluginHost>,
    _handle: pir::extensions::ExtensionHandle,
    _dir: TempDir,
}

/// Build the host the way the print host does, but with the DEFAULT plan a
/// normal CLI run resolves: `order_extension_paths` with no CLI `-e` paths and
/// no settings paths inserts every `default_enabled` builtin (context-mode
/// among them).
fn default_host() -> HostFixture {
    let plan = pir::plugins::order_extension_paths(&[], &[], false);
    assert!(
        plan.iter().any(|path| pir::plugins::builtin_name(
            &path.to_string_lossy()
        ) == Some("context-mode")),
        "a default plan must carry `builtin:context-mode`: {plan:?}"
    );
    let dir = TempDir::new("host");
    // A builtin-only plan: the plan-only handle — no QuickJS actor, no
    // prelude. This is exactly the shape `load_extension_runtime` builds
    // before `PluginHost::build` in print mode.
    let handle = pir::extensions::ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "print".to_owned(),
        has_ui: false,
        ..Default::default()
    });
    handle.set_path_plan(plan);
    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));
    HostFixture {
        host,
        _handle: handle,
        _dir: dir,
    }
}

/// The run registry as a host merges it: core tools first (the session/print
/// base), then the plugin host's tools through the one merge function.
fn merged_registry(host: &PluginHost) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    merge_plugin_tools(&mut registry, host, None);
    registry.remove_exposure(ToolExposure::Hidden);
    registry
}

fn resolved_tool_names(host: &PluginHost) -> Vec<String> {
    // Bind the registry `Arc` first: `resolved_tools` borrows it.
    let registry = host.registry();
    registry
        .resolved_tools()
        .into_iter()
        .map(|(def, _)| def.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. The plugin is constructed and registers its tools (the layer that was
//    already correct — pinned so a regression names the right seam).
// ---------------------------------------------------------------------------

#[test]
fn default_plan_constructs_the_context_mode_builtin_and_registers_nine_tools() {
    let fixture = default_host();
    let host = &fixture.host;
    let plugin_paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert!(
        plugin_paths
            .iter()
            .any(|path| path == "builtin:context-mode"),
        "the default plan must instantiate `builtin:context-mode`: {plugin_paths:?}"
    );

    let registered = resolved_tool_names(host);
    for name in CTX_TOOL_NAMES {
        assert!(
            registered.iter().any(|entry| entry == name),
            "`{name}` is missing from the plugin registry: {registered:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The tools are ACTIVE, not merely registered: each declares the default
//    `Direct` exposure and `default_active: true`, which
//    `ToolExposure::active_on_registration` turns into "declared to the
//    model". This is the assertion the user's bug violated.
// ---------------------------------------------------------------------------

#[test]
fn every_context_mode_tool_is_active_on_registration() {
    let fixture = default_host();
    let registry = fixture.host.registry();
    let tools: Vec<(String, Arc<dyn AgentTool>)> = registry
        .resolved_tools()
        .into_iter()
        .filter_map(|(def, backend)| match backend {
            pir::plugins::ToolBackend::Rust(tool) => Some((def.name.clone(), tool.clone())),
            pir::plugins::ToolBackend::Js { .. } => None,
        })
        .collect();
    for name in CTX_TOOL_NAMES {
        let tool = tools
            .iter()
            .find(|(tool_name, _)| tool_name == name)
            .map(|(_, tool)| Arc::clone(tool))
            .unwrap_or_else(|| panic!("`{name}` is not a Rust-backed registered tool"));
        assert_eq!(
            tool.exposure(),
            ToolExposure::Direct,
            "`{name}` must be a Direct (model-facing) tool, not deliberately deferred"
        );
        assert!(
            tool.default_active(),
            "`{name}` must be active on registration (upstream registers the \
             context-mode tools as ordinary model-facing tools)"
        );
        assert!(
            tool.exposure()
                .active_on_registration(tool.default_active()),
            "`{name}` must activate on registration"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The merge seam: the nine tools reach the run registry and the request's
//    declaration list. This is the fix — before it, a print-mode run's
//    registry held only core tools.
// ---------------------------------------------------------------------------

#[test]
fn merged_run_registry_declares_all_nine_context_mode_tools() {
    let fixture = default_host();
    let registry = merged_registry(&fixture.host);

    let active = registry.active_names(None);
    for name in CTX_TOOL_NAMES {
        assert!(
            active.iter().any(|entry| entry == name),
            "`{name}` is not in the active set after the merge: {active:?}"
        );
        assert!(
            registry.get(name).is_some(),
            "`{name}` did not merge into the run registry"
        );
    }

    // The model-facing declarations — the request body's `tools` list.
    let declared = declared_tools(&registry, &active);
    let definitions = declared_tool_definitions(
        &declared,
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    );
    let declared_names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    for name in CTX_TOOL_NAMES {
        assert!(
            declared_names.contains(&name),
            "`{name}` is missing from the model-facing tool list: {declared_names:?}"
        );
    }
    // The core tools are still there: the merge is additive, and CORE keeps
    // first-wins precedence.
    for core in ["read", "bash", "edit", "write"] {
        assert!(
            declared_names.contains(&core),
            "the merge must not drop core tool `{core}`: {declared_names:?}"
        );
    }
    // The pre-fix shape the probe captured: nine tools. The merge must have
    // moved past it.
    assert!(
        definitions.len() > 9,
        "the declared set is still the core-only shape the bug produced \
         ({} tools): {declared_names:?}",
        definitions.len()
    );
}

// ---------------------------------------------------------------------------
// 4. Registration survives a registry rebuild (a JS `session_start` handler or
//    `/reload` re-runs every `register()`); the merged view must keep the
//    tools after `refresh_registry_if_changed`, not just at build time.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn rebuilt_host_registry_still_resolves_the_context_mode_tools() {
    let fixture = default_host();
    let host = &fixture.host;
    host.rebuild_registry().await;
    let registered = resolved_tool_names(host);
    for name in CTX_TOOL_NAMES {
        assert!(
            registered.iter().any(|entry| entry == name),
            "`{name}` disappeared after a registry rebuild: {registered:?}"
        );
    }
    let registry = merged_registry(host);
    for name in CTX_TOOL_NAMES {
        assert!(
            registry.get(name).is_some(),
            "`{name}` is not merged after a registry rebuild"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. The builtin stays droppable: `disabledBuiltinExtensions: ["context-mode"]`
//    and `--no-extensions` remove it exactly as before.
// ---------------------------------------------------------------------------

#[test]
fn disabled_builtin_extensions_still_drops_context_mode() {
    let plan = pir::plugins::order_extension_paths(&[], &[], false);
    let disabled: std::collections::BTreeSet<String> =
        std::iter::once("context-mode".to_owned()).collect();
    let filtered = pir::plugins::filter_disabled_builtins(&plan, &disabled);
    assert!(
        !filtered.iter().any(|path| pir::plugins::builtin_name(
            &path.to_string_lossy()
        ) == Some("context-mode")),
        "`disabledBuiltinExtensions: [\"context-mode\"]` must drop the builtin: {filtered:?}"
    );
    // Whatever else the plan carries survives the filter: dropping one
    // builtin must not collapse the rest (a feature-trimmed build has any
    // subset of the factory list, so compare against the plan rather than
    // naming another builtin).
    let expected: Vec<String> = plan
        .iter()
        .filter(|path| pir::plugins::builtin_name(&path.to_string_lossy()) != Some("context-mode"))
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let actual: Vec<String> = filtered
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        actual, expected,
        "dropping `context-mode` must leave every other plan entry intact"
    );

    // And the host built on the filtered plan carries no ctx tools.
    let handle = pir::extensions::ExtensionHandle::start_plan_only(ExtensionConfig {
        mode: "print".to_owned(),
        has_ui: false,
        ..Default::default()
    });
    handle.set_path_plan(filtered);
    let host = PluginHost::build(Some(handle), Vec::new());
    let registered = resolved_tool_names(&host);
    for name in CTX_TOOL_NAMES {
        assert!(
            !registered.iter().any(|entry| entry == name),
            "`{name}` must not be registered once the builtin is disabled: {registered:?}"
        );
    }
}

#[test]
fn no_extensions_plan_omits_the_builtin_entirely() {
    // `--no-extensions` keeps ONLY explicit `-e` paths.
    let plan = pir::plugins::order_extension_paths(&[], &[], true);
    assert!(
        !plan.iter().any(|path| pir::plugins::builtin_name(
            &path.to_string_lossy()
        ) == Some("context-mode")),
        "`--no-extensions` must not insert `builtin:context-mode`: {plan:?}"
    );
    assert!(
        plan.is_empty(),
        "`--no-extensions` with no `-e` path is an empty plan: {plan:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Registration is lazy: constructing the builtin (which every default run
//    now does) must not start the Python worker or open the SQLite index. The
//    cost question the plugin documents, pinned where the exposure contract
//    lives.
// ---------------------------------------------------------------------------

#[test]
fn default_plan_registration_stays_lazy() {
    let plugin = ContextModePlugin::new();
    let mut registrations = pir::plugins::PluginRegistrations::default();
    plugin
        .register(&mut pir::plugins::RegistrationContext::new(
            &mut registrations,
        ))
        .expect("context-mode registers");
    assert_eq!(
        registrations.tools.len(),
        CTX_TOOL_NAMES.len(),
        "the plugin must register exactly its nine tools"
    );
    assert!(
        !plugin.has_python_worker(),
        "registering must not start the Python worker"
    );
    assert!(
        !plugin.index_is_open(),
        "registering must not open the SQLite index"
    );
}

/// The default plan is stable between two calls (dedup is by canonical key),
/// so two hosts built from it resolve the same tool names — the property that
/// makes "the default plan exposes ctx_*" a fact about the build rather than
/// about one lucky resolution order.
#[test]
fn default_plan_resolves_the_same_tools_twice() {
    let first: Vec<PathBuf> = pir::plugins::order_extension_paths(&[], &[], false);
    let second: Vec<PathBuf> = pir::plugins::order_extension_paths(&[], &[], false);
    assert_eq!(first, second, "the default plan must be deterministic");

    let first_fixture = default_host();
    let second_fixture = default_host();
    let mut first_names = resolved_tool_names(&first_fixture.host);
    let mut second_names = resolved_tool_names(&second_fixture.host);
    first_names.sort();
    second_names.sort();
    assert_eq!(
        first_names, second_names,
        "two hosts from the same plan must resolve the same tool names"
    );
    assert!(
        first_names.iter().any(|name| name == CTX_EXECUTE_TOOL_NAME),
        "the default host resolves `ctx_execute`: {first_names:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. The default DB path follows its inputs. `IndexStore` memoizes the
//    resolution on `(cwd, base)` (env-audit D10: it used to be a `OnceLock`
//    keyed by first use, so a mid-process cwd or agent-dir change never moved
//    the database). A host that pushes a new session cwd must move to that
//    project's database, while a stable cwd keeps the same path — the old
//    resolve-once behavior is preserved for a stable input, not a stale one.
// ---------------------------------------------------------------------------

fn push_cwd(plugin: &ContextModePlugin, cwd: &std::path::Path) {
    plugin.on_session_context(&SessionContextSnapshot {
        cwd: Some(cwd.to_string_lossy().into_owned()),
        ..SessionContextSnapshot::default()
    });
}

#[test]
fn default_database_path_follows_a_changed_session_cwd() {
    let dir = TempDir::new("db-cwd-follow");
    let base = dir.0.join("agent-base");
    let cwd_a = dir.0.join("project-a");
    let cwd_b = dir.0.join("project-b");
    for cwd in [&cwd_a, &cwd_b] {
        std::fs::create_dir_all(cwd).expect("project cwd");
    }
    let plugin = ContextModePlugin::with_db_base_dir(&base);
    let expected_dir = base.join("context-mode");

    push_cwd(&plugin, &cwd_a);
    let path_a = plugin.resolved_db_path();
    assert!(path_a.starts_with(&expected_dir), "{path_a:?}");

    // A stable cwd keeps the same path: still resolved once per input.
    push_cwd(&plugin, &cwd_a);
    assert_eq!(
        plugin.resolved_db_path(),
        path_a,
        "a stable session cwd must not move the database"
    );

    // A changed cwd moves the database instead of keeping the first
    // resolution for the process lifetime.
    push_cwd(&plugin, &cwd_b);
    let path_b = plugin.resolved_db_path();
    assert_ne!(
        path_b, path_a,
        "a changed session cwd must move the database"
    );
    assert!(path_b.starts_with(&expected_dir), "{path_b:?}");
    assert_eq!(
        path_b.extension().and_then(|value| value.to_str()),
        Some("db")
    );

    // Returning to the earlier cwd comes back to the same file (the path is a
    // pure function of `(cwd, base)`, not a history of visits).
    push_cwd(&plugin, &cwd_a);
    assert_eq!(
        plugin.resolved_db_path(),
        path_a,
        "re-pushing an earlier cwd must resolve to the same database"
    );
}

/// The base is a full input, not a fixed first-use value: the same cwd under
/// two bases yields two databases, each rooted at its own base.
#[test]
fn default_database_path_is_rooted_at_the_agent_dir_base() {
    let dir = TempDir::new("db-base-follow");
    let cwd = dir.0.join("project");
    std::fs::create_dir_all(&cwd).expect("project cwd");
    let base_a = dir.0.join("agent-a");
    let base_b = dir.0.join("agent-b");

    let plugin_a = ContextModePlugin::with_db_base_dir(&base_a);
    let plugin_b = ContextModePlugin::with_db_base_dir(&base_b);
    push_cwd(&plugin_a, &cwd);
    push_cwd(&plugin_b, &cwd);
    let path_a = plugin_a.resolved_db_path();
    let path_b = plugin_b.resolved_db_path();

    assert!(
        path_a.starts_with(base_a.join("context-mode")),
        "{} must be rooted at {}",
        path_a.display(),
        base_a.display()
    );
    assert!(
        path_b.starts_with(base_b.join("context-mode")),
        "{} must be rooted at {}",
        path_b.display(),
        base_b.display()
    );
    assert_ne!(
        path_a, path_b,
        "a changed agent-dir base must move the database"
    );
}

/// The real D10 scenario: the DEFAULT store reads the agent dir per
/// resolution, so a mid-process `PIR_AGENT_DIR` change moves the database.
/// Serialized through the binary-wide [`crate::common::AgentDirEnvGuard`] lock,
/// the same guard the sibling suites in this binary pin with.
#[tokio::test(flavor = "current_thread")]
async fn default_database_path_follows_a_changed_agent_dir() {
    let dir = TempDir::new("db-agent-dir-follow");
    let agent_a = dir.0.join("agent-a");
    let agent_b = dir.0.join("agent-b");
    let cwd = dir.0.join("project");
    std::fs::create_dir_all(&cwd).expect("project cwd");
    let _env = crate::common::AgentDirEnvGuard::at(&agent_a, &[]).await;

    let plugin = ContextModePlugin::new();
    push_cwd(&plugin, &cwd);
    let path_a = plugin.resolved_db_path();
    assert!(
        path_a.starts_with(agent_a.join("context-mode")),
        "{} must be rooted at {}",
        path_a.display(),
        agent_a.display()
    );

    std::env::set_var("PIR_AGENT_DIR", &agent_b);
    let path_b = plugin.resolved_db_path();
    assert_ne!(
        path_b, path_a,
        "a changed agent dir must move the database on the same store"
    );
    assert!(
        path_b.starts_with(agent_b.join("context-mode")),
        "{} must be rooted at {}",
        path_b.display(),
        agent_b.display()
    );

    std::env::set_var("PIR_AGENT_DIR", &agent_a);
    assert_eq!(
        plugin.resolved_db_path(),
        path_a,
        "the resolved path is a pure function of `(cwd, base)`"
    );
}
