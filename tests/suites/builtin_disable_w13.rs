//! W13-W: the settings-driven builtin switches must actually work.
//!
//! Two defects found by lane W13-S, both verified in code:
//!
//! 1. `disabledBuiltinExtensions` was ADVERTISED but never read. `docs/mcp.md`
//!    and `docs/todo-builtin.md` (plus several code comments) tell users to
//!    set it; the only value ever passed to `filter_disabled_builtins` was
//!    `disabled_from_flags(cli.no_mcp)` — i.e. `--no-mcp` only. There was no
//!    `Settings` field, no `SettingsManager` getter and no raw-key reader, and
//!    the schema's `additionalProperties: true` made the array silently
//!    accepted and dropped.
//! 2. `pythonTool: false` was honored only by
//!    `AgentSession::build_plugin_host`; `pir print` built its host directly
//!    and never applied the gate.
//!
//! These tests pin the fixed contract through public APIs only:
//!
//! - (a) `disabledBuiltinExtensions: ["context-mode"]` removes the builtin
//!   from a default plan;
//! - (b) `--no-mcp` and the settings key compose (neither widens the other);
//! - (c) `pythonTool: false` removes `python` from BOTH a session host and a
//!   print host (the two now share one helper);
//! - (d) an unknown name is harmless, and junk values are tolerated the way
//!   every neighbour setting is;
//! - the scope rule: a project may ADD a name but never re-enable a builtin
//!   the global scope (or another layer) disabled;
//! - the settings schema declares the key (so it is no longer silently
//!   dropped) and the generator reproduces the committed artifact.
//!
//! NOTE: this file is registered by the foreman (`#[path = "suites/
//! builtin_disable_w13.rs"]` in a `g_*` merged binary). It uses
//! `crate::common::AgentDirEnvGuard` for the process-wide `PIR_AGENT_DIR`
//! pin, so register it in a binary that includes `mod common;`
//! (`tests/g_tools.rs` / `tests/g_agent.rs`).

use std::path::{Path, PathBuf};
#[cfg(feature = "python")]
use std::sync::Arc;

use pir::config::settings::SettingsManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A self-cleaning temp directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pir-builtin-disable-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
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

/// Write `<dir>/settings.json` with `json`.
fn write_settings(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).expect("settings dir");
    std::fs::write(dir.join("settings.json"), json).expect("write settings");
}

/// A manager with an explicit global dir and `<cwd>/.pi` project dir (no
/// process environment involved, so the scope tests are hermetic).
fn load_manager(global: &Path, cwd: &Path, trusted: bool) -> SettingsManager {
    SettingsManager::load(global, &cwd.join(".pi"), cwd, trusted)
}

/// The default builtin plan a normal CLI run resolves: every
/// `default_enabled` factory builtin, no CLI `-e` paths, no settings paths.
fn default_plan() -> Vec<PathBuf> {
    pir::plugins::order_extension_paths(&[], &[], false)
}

fn has_builtin(plan: &[PathBuf], name: &str) -> bool {
    plan.iter()
        .any(|path| pir::plugins::builtin_name(&path.to_string_lossy()) == Some(name))
}

fn builtin_names(plan: &[PathBuf]) -> Vec<String> {
    plan.iter()
        .filter_map(|path| pir::plugins::builtin_name(&path.to_string_lossy()).map(str::to_owned))
        .collect()
}

/// The `builtin:<name>` paths of `plan` with `dropped` removed — the plugin
/// paths a host built on the filtered plan must carry (a builtin's
/// `PluginInfo::path` is exactly its plan path).
#[cfg(feature = "python")]
fn builtin_paths_without(plan: &[PathBuf], dropped: &str) -> Vec<String> {
    plan.iter()
        .filter(|path| pir::plugins::builtin_name(&path.to_string_lossy()) != Some(dropped))
        .filter(|path| pir::plugins::builtin_name(&path.to_string_lossy()).is_some())
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// A plan-only extension handle rooted at `cwd` with `plan` recorded — the
/// shape both hosts build their `PluginHost` from (no QuickJS actor).
#[cfg(feature = "python")]
fn plan_only_handle(cwd: &Path, plan: &[PathBuf]) -> pir::extensions::ExtensionHandle {
    let handle =
        pir::extensions::ExtensionHandle::start_plan_only(pir::extensions::ExtensionConfig {
            cwd: cwd.to_path_buf(),
            mode: "print".to_owned(),
            has_ui: false,
            ..Default::default()
        });
    handle.set_path_plan(plan.to_vec());
    handle
}

#[cfg(feature = "python")]
fn fake_model() -> pir::ai::types::Model {
    serde_json::from_value(serde_json::json!({
        "id": "fake-model",
        "name": "Fake",
        "api": "fake",
        "provider": "fake",
        "baseUrl": "http://localhost",
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0 },
        "contextWindow": 4096,
        "maxTokens": 256
    }))
    .expect("fake model JSON")
}

#[cfg(feature = "python")]
fn session_config(
    cwd: &Path,
    extensions: &pir::extensions::ExtensionHandle,
) -> pir::agent_session::AgentSessionConfig {
    pir::agent_session::AgentSessionConfig {
        provider_registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: pir::ai::types::ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: Some(extensions.clone()),
        tool_selection: None,
    }
}

// ---------------------------------------------------------------------------
// (a) The settings key removes the builtin from a default plan.
// ---------------------------------------------------------------------------

#[cfg(feature = "context-mode")]
#[test]
fn disabled_builtin_extensions_removes_the_builtin_from_a_default_plan() {
    let global = TempDir::new("global-a");
    write_settings(
        global.path(),
        r#"{ "disabledBuiltinExtensions": ["context-mode"] }"#,
    );
    let cwd = TempDir::new("cwd-a");
    let manager = load_manager(global.path(), cwd.path(), false);

    // The key is parsed into the typed surface (it used to land in `extra`
    // and be dropped).
    assert_eq!(
        manager.settings.disabled_builtin_extensions.as_deref(),
        Some(&["context-mode".to_owned()][..]),
        "the typed settings snapshot must carry the key"
    );
    assert!(
        manager
            .disabled_builtin_extensions()
            .contains("context-mode"),
        "the manager getter must expose the configured name"
    );

    let plan = default_plan();
    assert!(
        has_builtin(&plan, "context-mode"),
        "the fixture assumption is a default plan carrying `builtin:context-mode`: {plan:?}"
    );

    // The composition the CLI plan builder uses (`--no-mcp` absent here).
    let disabled = manager.disabled_builtins_with_flags(&pir::plugins::disabled_from_flags(false));
    assert_eq!(
        disabled,
        std::collections::BTreeSet::from(["context-mode".to_owned()]),
        "no flag is set, so only the settings name is disabled"
    );

    let filtered = pir::plugins::filter_disabled_builtins(&plan, &disabled);
    assert!(
        !has_builtin(&filtered, "context-mode"),
        "`disabledBuiltinExtensions: [\"context-mode\"]` must drop the builtin: {filtered:?}"
    );
    // Dropping one builtin must leave every other plan entry intact.
    let expected: Vec<String> = builtin_names(&plan)
        .into_iter()
        .filter(|name| name != "context-mode")
        .collect();
    assert_eq!(
        builtin_names(&filtered),
        expected,
        "only the named builtin may disappear"
    );
}

// ---------------------------------------------------------------------------
// (b) `--no-mcp` and the settings key compose.
// ---------------------------------------------------------------------------

#[test]
fn no_mcp_flag_and_the_settings_key_compose() {
    let global = TempDir::new("global-b");
    write_settings(
        global.path(),
        r#"{ "disabledBuiltinExtensions": ["context-mode"] }"#,
    );
    let cwd = TempDir::new("cwd-b");
    let manager = load_manager(global.path(), cwd.path(), false);

    // `--no-mcp` alone.
    let flag_only: std::collections::BTreeSet<String> = pir::plugins::disabled_from_flags(true)
        .into_iter()
        .collect();
    assert_eq!(
        flag_only,
        std::collections::BTreeSet::from(["mcp".to_owned()]),
        "the flag maps to the `mcp` builtin name"
    );

    // Flag + settings: the UNION of both restrictions.
    let both = manager.disabled_builtins_with_flags(&pir::plugins::disabled_from_flags(true));
    assert!(both.contains("mcp"), "the flag restriction survives");
    assert!(
        both.contains("context-mode"),
        "the settings restriction survives"
    );

    let filtered = pir::plugins::filter_disabled_builtins(&default_plan(), &both);
    assert!(
        !has_builtin(&filtered, "mcp") && !has_builtin(&filtered, "context-mode"),
        "`--no-mcp` and the settings key must both apply: {filtered:?}"
    );
    // And neither restriction was widened into "drop everything".
    assert!(
        filtered.len() >= default_plan().len().saturating_sub(2),
        "composing two disabled names may drop at most two builtins: {filtered:?}"
    );
}

#[test]
fn project_scope_can_add_but_never_lift_a_global_restriction() {
    let global = TempDir::new("global-scope");
    write_settings(
        global.path(),
        r#"{ "disabledBuiltinExtensions": ["context-mode"] }"#,
    );
    let cwd = TempDir::new("cwd-scope");
    write_settings(
        &cwd.path().join(".pi"),
        r#"{ "disabledBuiltinExtensions": ["mcp"] }"#,
    );

    let trusted = load_manager(global.path(), cwd.path(), true);
    let names = trusted.disabled_builtin_extensions();
    assert!(
        names.contains("context-mode"),
        "the global name must survive a project list (arrays replace in the deep merge): {names:?}"
    );
    assert!(
        names.contains("mcp"),
        "the project name is added: {names:?}"
    );

    // A project that restates the list EMPTY must not re-enable the builtin
    // the user disabled globally.
    write_settings(
        &cwd.path().join(".pi"),
        r#"{ "disabledBuiltinExtensions": [] }"#,
    );
    let reset = load_manager(global.path(), cwd.path(), true);
    assert!(
        reset.disabled_builtin_extensions().contains("context-mode"),
        "a project `[]` must not loosen the global restriction"
    );

    // An untrusted project contributes nothing.
    let untrusted = load_manager(global.path(), cwd.path(), false);
    let names = untrusted.disabled_builtin_extensions();
    assert!(
        names.contains("context-mode") && !names.contains("mcp"),
        "an untrusted project scope is not read: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// (c) `pythonTool: false` removes `python` from BOTH hosts.
// ---------------------------------------------------------------------------

#[cfg(feature = "python")]
fn default_plan_with_python() -> Vec<PathBuf> {
    let plan = default_plan();
    assert!(
        has_builtin(&plan, "python"),
        "the fixture assumption is a default plan carrying `builtin:python`: {plan:?}"
    );
    plan
}

#[cfg(feature = "python")]
#[tokio::test(flavor = "current_thread")]
async fn python_tool_false_removes_python_from_a_session_host() {
    let _env = crate::common::AgentDirEnvGuard::scratch(
        "builtin-disable-session",
        Some(r#"{ "pythonTool": false }"#),
    )
    .await;

    let cwd = TempDir::new("session-cwd");
    let plan = default_plan_with_python();
    let handle = plan_only_handle(cwd.path(), &plan);
    let session = pir::agent_session::AgentSession::new_with_plugins(
        session_config(cwd.path(), &handle),
        Vec::new(),
    );

    let host = session
        .plugin_host()
        .expect("an extensions handle builds a host");
    let paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert!(
        !paths.iter().any(|path| path == "builtin:python"),
        "`pythonTool: false` must keep the python builtin out of the session host: {paths:?}"
    );
    // The gate mutated the recorded plan, which is what every later consumer
    // of the handle reads.
    assert!(!has_builtin(&handle.path_plan(), "python"));
    // Every other builtin the plan carried is still instantiated.
    let expected = builtin_paths_without(&plan, "python");
    for path in &expected {
        assert!(
            paths.contains(path),
            "the gate must not drop unrelated builtins: {path} missing from {paths:?}"
        );
    }
}

#[cfg(feature = "python")]
#[tokio::test(flavor = "current_thread")]
async fn python_tool_false_removes_python_from_a_print_host() {
    let _env = crate::common::AgentDirEnvGuard::scratch(
        "builtin-disable-print",
        Some(r#"{ "pythonTool": false }"#),
    )
    .await;

    let cwd = TempDir::new("print-cwd");
    let plan = default_plan_with_python();
    let handle = plan_only_handle(cwd.path(), &plan);
    let host = pir::coding_agent::print::print_plugin_host(Some(&handle))
        .expect("the runtime builds the print host");

    let paths: Vec<String> = host
        .plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect();
    assert!(
        !paths.iter().any(|path| path == "builtin:python"),
        "`pir print` must honor `pythonTool: false` exactly like the TUI: {paths:?}"
    );
    assert!(!has_builtin(&handle.path_plan(), "python"));
    let expected = builtin_paths_without(&plan, "python");
    for path in &expected {
        assert!(
            paths.contains(path),
            "unrelated builtins must stay: {path} missing from {paths:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// (d) Unknown names are harmless; junk values are tolerated.
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_name_in_the_array_is_harmless() {
    let global = TempDir::new("global-d");
    write_settings(
        global.path(),
        r#"{ "disabledBuiltinExtensions": ["not-a-real-builtin"] }"#,
    );
    let cwd = TempDir::new("cwd-d");
    let manager = load_manager(global.path(), cwd.path(), false);

    let disabled = manager.disabled_builtins_with_flags(&pir::plugins::disabled_from_flags(false));
    assert_eq!(
        disabled,
        std::collections::BTreeSet::from(["not-a-real-builtin".to_owned()])
    );

    let plan = default_plan();
    let filtered = pir::plugins::filter_disabled_builtins(&plan, &disabled);
    assert_eq!(
        filtered, plan,
        "a name that matches no builtin must not change the plan"
    );
}

#[test]
fn junk_values_are_tolerated_like_every_neighbour_field() {
    // A bare string is accepted as a one-name list.
    let global = TempDir::new("global-string");
    write_settings(global.path(), r#"{ "disabledBuiltinExtensions": "mcp" }"#);
    let cwd = TempDir::new("cwd-string");
    let manager = load_manager(global.path(), cwd.path(), false);
    assert_eq!(
        manager.disabled_builtin_extensions(),
        std::collections::BTreeSet::from(["mcp".to_owned()])
    );
    assert!(manager.error_messages().is_empty());

    // A non-list value is dropped with ONE per-field warning (the manager's
    // neighbour behaviour), and the rest of the document still applies.
    let global = TempDir::new("global-junk");
    write_settings(
        global.path(),
        r#"{ "disabledBuiltinExtensions": { "oops": true }, "theme": "night" }"#,
    );
    let cwd = TempDir::new("cwd-junk");
    let manager = load_manager(global.path(), cwd.path(), false);
    assert!(
        manager.disabled_builtin_extensions().is_empty(),
        "a wrong-typed value falls back to `unset`"
    );
    assert_eq!(
        manager.settings.theme.as_deref(),
        Some("night"),
        "the rest of the settings document survives the bad field"
    );
    assert!(
        manager
            .warning_messages()
            .iter()
            .any(|message| message.contains("disabledBuiltinExtensions")),
        "the bad field must be reported: {:?}",
        manager.warning_messages()
    );
    // Non-string entries inside an array are ignored, the strings still apply.
    let global = TempDir::new("global-mixed");
    write_settings(
        global.path(),
        r#"{ "disabledBuiltinExtensions": ["mcp", 7, null] }"#,
    );
    let mixed_cwd = TempDir::new("cwd-mixed");
    let manager = load_manager(global.path(), mixed_cwd.path(), false);
    assert_eq!(
        manager.disabled_builtin_extensions(),
        std::collections::BTreeSet::from(["mcp".to_owned()])
    );
}

#[test]
fn python_tool_false_is_one_name_in_the_same_disabled_set() {
    let global = TempDir::new("global-python");
    write_settings(global.path(), r#"{ "pythonTool": false }"#);
    let cwd = TempDir::new("cwd-python");
    let manager = load_manager(global.path(), cwd.path(), false);

    assert!(!manager.python_tool_enabled());
    assert!(
        manager.disabled_builtins().contains("python"),
        "the key is expressed as the same `python` disabled name: {:?}",
        manager.disabled_builtins()
    );

    // A project that sets `true` must not re-enable a globally disabled tool.
    write_settings(&cwd.path().join(".pi"), r#"{ "pythonTool": true }"#);
    let project_wins = load_manager(global.path(), cwd.path(), true);
    assert!(
        !project_wins.python_tool_enabled(),
        "settings never widen a restriction: a project cannot re-enable the tool"
    );

    // Unset keeps it enabled. (Bind the temp dirs first: borrowing
    // `TempDir::new(..).0` would drop the directory inside the call.)
    let plain_global = TempDir::new("global-plain");
    let plain_cwd = TempDir::new("cwd-plain");
    let plain = load_manager(plain_global.path(), plain_cwd.path(), false);
    assert!(plain.python_tool_enabled());
    assert!(!plain.disabled_builtins().contains("python"));
}

// ---------------------------------------------------------------------------
// Schema: the key is declared (no longer silently accepted and dropped).
// ---------------------------------------------------------------------------

#[test]
fn settings_schema_declares_disabled_builtin_extensions() {
    let schema = pir::config::schema::settings_schema();
    let property = &schema["properties"]["disabledBuiltinExtensions"];
    assert_eq!(
        property["type"], "array",
        "the key must be declared as an array in the settings schema"
    );
    assert_eq!(property["items"]["type"], "string");
    let description = property["description"].as_str().expect("description");
    assert!(
        description.contains("mcp"),
        "the description names the flag-equivalent example: {description}"
    );

    // The committed artifact carries the same declaration, so
    // `config_schemas::committed_schemas_match_generated` stays green.
    let committed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("schemas/settings.schema.json"),
        )
        .expect("committed settings schema"),
    )
    .expect("committed settings schema is valid JSON");
    assert_eq!(
        committed["properties"]["disabledBuiltinExtensions"], *property,
        "the committed schema must declare the settings key exactly like the generator"
    );
}
