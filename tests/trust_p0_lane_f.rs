//! P0 trust-lane regression tests: a resolved `--no-approve` must remain the
//! single source of project trust for every post-gate consumer, even when the
//! ambient `PIR_TRUST_PROJECT=1` operator override is set and an old
//! `trust.json` entry says the project is trusted.
//!
//! Covered consumers:
//! - the extension plan (project auto-discovery + project settings extensions),
//! - package-contributed resources,
//! - shell preferences (`shellCommandPrefix` / `shellPath`),
//! - MCP project-local `mcp.json` (`pir mcp list` and the builtin MCP plugin).
//!
//! The binary tests exercise the real startup gate (`run_mode`), which pins
//! `false` before any extension/package/MCP consumer runs.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, PoisonError};

use pir::config::trust::ProjectTrustStore;
use pir::extensions::project_trust::{clear_recorded_decisions, pin_decision};

/// Serializes the in-process tests that pin process-wide trust decisions or
/// change the process cwd. The binary tests never read the test process's cwd
/// (they spawn `pir` with an explicit `current_dir`), so they stay parallel.
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn cwd_lock() -> MutexGuard<'static, ()> {
    CWD_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unique_root(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "pir-trust-p0-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create fixture root");
    root
}

/// Project + agent dir pair with the `.pi` directory created.
fn project_fixture(label: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = unique_root(label);
    let project = root.join("project");
    let agent = root.join("agent");
    std::fs::create_dir_all(project.join(".pi")).expect("project dir");
    std::fs::create_dir_all(&agent).expect("agent dir");
    (root, project, agent)
}

/// Run the real `pir` binary against the fixture. `PIR_TRUST_PROJECT` is
/// removed unless the caller supplies it through `extra_env`.
fn run_pir(
    home: &Path,
    project: &Path,
    agent: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(project)
        .args(args)
        .env("PIR_AGENT_DIR", agent)
        .env("PI_CODING_AGENT_DIR", agent)
        // Hermetic home: no user `.agents/skills` or real user settings.
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PIR_TRUST_PROJECT");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("pir binary should start")
}

fn write_project_extension(project: &Path, flag: &str) {
    let path = project
        .join(".pi")
        .join("extensions")
        .join("project-ext.js");
    std::fs::create_dir_all(path.parent().expect("extensions dir")).expect("extensions dir");
    std::fs::write(
        &path,
        format!(
            "export default function (pi) {{ pi.registerFlag({flag:?}, {{ type: \"string\", description: \"project flag\" }}); }}\n"
        ),
    )
    .expect("write project extension");
}

/// A local package whose `pi` manifest contributes one extension path.
fn write_project_package(root: &Path, flag: &str) -> PathBuf {
    let package = root.join("fixture-package");
    std::fs::create_dir_all(package.join("extensions")).expect("package dirs");
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"fixture-package","version":"1.0.0","pi":{"extensions":["extensions/package-ext.js"]}}"#,
    )
    .expect("write package manifest");
    std::fs::write(
        package.join("extensions").join("package-ext.js"),
        format!(
            "export default function (pi) {{ pi.registerFlag({flag:?}, {{ type: \"string\", description: \"package flag\" }}); }}\n"
        ),
    )
    .expect("write package extension");
    package
}

fn seed_project_settings(project: &Path, packages: &[PathBuf]) {
    let settings = serde_json::json!({
        "packages": packages
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    });
    std::fs::write(
        project.join(".pi").join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("serialize settings"),
    )
    .expect("write project settings");
}

fn seed_old_trust_entry(agent: &Path, project: &Path) {
    ProjectTrustStore::new(agent)
        .set(project, Some(true))
        .expect("seed the old trust entry");
}

/// `--no-approve` + `PIR_TRUST_PROJECT=1` + an old `trust.json` grant must
/// leave BOTH the project auto-discovered extension and the project
/// package-contributed extension unloaded.
#[test]
fn no_approve_keeps_project_and_package_extensions_unloaded() {
    let (root, project, agent) = project_fixture("extensions");
    write_project_extension(&project, "project-flag");
    let package = write_project_package(&root, "package-flag");
    seed_project_settings(&project, &[package]);
    seed_old_trust_entry(&agent, &project);

    let denied = run_pir(
        &root,
        &project,
        &agent,
        &["--help-extensions", "--no-approve"],
        &[("PIR_TRUST_PROJECT", "1")],
    );
    assert!(
        denied.status.success(),
        "--help-extensions stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let denied_text = String::from_utf8_lossy(&denied.stdout);
    assert!(
        !denied_text.contains("--project-flag"),
        "--no-approve must keep the project auto-discovered extension unloaded: {denied_text}"
    );
    assert!(
        !denied_text.contains("--package-flag"),
        "--no-approve must keep project package resources unloaded: {denied_text}"
    );

    // Control: without `--no-approve` the same env override trusts the project
    // (both extensions load), proving the fixtures are valid and that only the
    // explicit denial suppresses them.
    let allowed = run_pir(
        &root,
        &project,
        &agent,
        &["--help-extensions"],
        &[("PIR_TRUST_PROJECT", "1")],
    );
    assert!(
        allowed.status.success(),
        "--help-extensions control stderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let allowed_text = String::from_utf8_lossy(&allowed.stdout);
    assert!(
        allowed_text.contains("--project-flag"),
        "the control run must load the project extension: {allowed_text}"
    );
    assert!(
        allowed_text.contains("--package-flag"),
        "the control run must load the package extension: {allowed_text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `--no-approve` + `PIR_TRUST_PROJECT=1` + an old `trust.json` grant must
/// keep the project-local `mcp.json` out of `pir mcp list`.
#[test]
fn no_approve_keeps_project_mcp_config_unloaded() {
    let (root, project, agent) = project_fixture("mcp");
    std::fs::write(
        project.join(".pi").join("mcp.json"),
        r#"{"mcpServers":{"local-guard":{"command":"definitely-not-a-real-mcp-command","enabled":false}}}"#,
    )
    .expect("write project mcp.json");
    seed_old_trust_entry(&agent, &project);

    let denied = run_pir(
        &root,
        &project,
        &agent,
        &["mcp", "list", "--json", "--no-approve"],
        &[("PIR_TRUST_PROJECT", "1")],
    );
    assert!(
        denied.status.success(),
        "pir mcp list stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let denied_text = String::from_utf8_lossy(&denied.stdout);
    assert!(
        !denied_text.contains("local-guard"),
        "--no-approve must keep the project mcp.json unloaded: {denied_text}"
    );

    // Control: the same project config IS visible when the run is trusted.
    let allowed = run_pir(
        &root,
        &project,
        &agent,
        &["mcp", "list", "--json"],
        &[("PIR_TRUST_PROJECT", "1")],
    );
    assert!(
        allowed.status.success(),
        "pir mcp list control stderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let allowed_text = String::from_utf8_lossy(&allowed.stdout);
    assert!(
        allowed_text.contains("local-guard"),
        "the control run must see the project mcp.json server: {allowed_text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The in-run builtin MCP plugin is the other project-`mcp.json` consumer.
/// Once the startup gate pinned a denial, building the plugin host (which
/// runs `McpPlugin::register`) must not register project-local servers.
#[cfg(feature = "mcp")]
#[allow(clippy::await_holding_lock)] // the process-global cwd lock intentionally serializes this test
#[tokio::test(flavor = "current_thread")]
async fn no_approve_keeps_builtin_mcp_plugin_off_project_config() {
    use std::sync::Arc;

    use pir::plugins::builtin::McpPlugin;
    use pir::plugins::{AgentPlugin, PluginHost};

    let (root, project, agent) = project_fixture("mcp-plugin");
    std::fs::write(
        project.join(".pi").join("mcp.json"),
        r#"{"mcpServers":{"local-guard":{"command":"definitely-not-a-real-mcp-command","enabled":false}}}"#,
    )
    .expect("write project mcp.json");
    seed_old_trust_entry(&agent, &project);

    let _lock = cwd_lock();
    let _decisions = DecisionsGuard::new();
    pin_decision(&project, false);
    let _env = TrustEnvGuard::pin("1");

    let plugin = Arc::new(McpPlugin::with_cwd(&agent, &project));
    let _host = PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>]);
    let names: Vec<String> = plugin
        .manager()
        .registry()
        .list()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(
        !names.iter().any(|name| name == "local-guard"),
        "the pinned denial must keep project mcp.json out of the plugin registry: {names:?}"
    );
    plugin.manager().shutdown().await;

    // Control: without the pin the same env/store decision loads the project
    // config, proving the fixture and the plugin pass are otherwise active.
    clear_recorded_decisions();
    let allowed = Arc::new(McpPlugin::with_cwd(&agent, &project));
    let _host = PluginHost::build(None, vec![allowed.clone() as Arc<dyn AgentPlugin>]);
    let allowed_names: Vec<String> = allowed
        .manager()
        .registry()
        .list()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(
        allowed_names.iter().any(|name| name == "local-guard"),
        "the control plugin pass must register the project mcp.json server: {allowed_names:?}"
    );
    allowed.manager().shutdown().await;

    let _ = std::fs::remove_dir_all(&root);
}

/// Panic-safe `PIR_TRUST_PROJECT` pin for the in-process shell-prefs test.
struct TrustEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl TrustEnvGuard {
    fn pin(value: &str) -> Self {
        let previous = std::env::var_os("PIR_TRUST_PROJECT");
        std::env::set_var("PIR_TRUST_PROJECT", value);
        Self { previous }
    }
}

impl Drop for TrustEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("PIR_TRUST_PROJECT", value),
            None => std::env::remove_var("PIR_TRUST_PROJECT"),
        }
    }
}

/// Restores the process cwd on drop (panic included).
struct CwdGuard {
    previous: PathBuf,
}

impl CwdGuard {
    fn enter(dir: &Path) -> Self {
        let previous = std::env::current_dir().expect("read cwd");
        std::env::set_current_dir(dir).expect("enter fixture cwd");
        Self { previous }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

/// Clears the process-wide pinned/recorded decisions before and after the
/// in-process scenario so a previous test cannot leak trust into this one.
struct DecisionsGuard;

impl DecisionsGuard {
    fn new() -> Self {
        clear_recorded_decisions();
        Self
    }
}

impl Drop for DecisionsGuard {
    fn drop(&mut self) {
        clear_recorded_decisions();
    }
}

/// The run's pinned denial must keep the project shell preferences
/// (`shellCommandPrefix` / `shellPath`) unloaded even with
/// `PIR_TRUST_PROJECT=1` and an old stored grant. `shell_prefs_in` is the
/// shell-tool consumer of `resolve_cwd_trust`.
#[test]
fn no_approve_keeps_project_shell_prefs_unloaded() {
    let (root, project, agent) = project_fixture("shell-prefs");
    std::fs::write(
        project.join(".pi").join("settings.json"),
        r#"{"shellCommandPrefix":"echo SHOULD_NOT_APPEAR","shellPath":"/should/not/be/used"}"#,
    )
    .expect("write project settings");
    seed_old_trust_entry(&agent, &project);

    let _cwd_lock = cwd_lock();
    let _decisions = DecisionsGuard::new();
    // What the CLI startup gate does after resolving an explicit
    // `--no-approve`; the ambient env and old store entry must not reopen it.
    pin_decision(&project, false);
    let _env = TrustEnvGuard::pin("1");

    assert!(
        !pir::config::trust::resolve_cwd_trust(&project, &agent),
        "the pinned denial must outrank PIR_TRUST_PROJECT=1 and the old trust entry"
    );

    let _cwd = CwdGuard::enter(&project);
    let prefs = pir::config::settings::shell_prefs_in(&agent);
    assert_eq!(
        prefs.command_prefix, None,
        "--no-approve must keep the project shellCommandPrefix unloaded"
    );
    assert_eq!(
        prefs.shell_path, None,
        "--no-approve must keep the project shellPath unloaded"
    );
    drop(_cwd);

    let _ = std::fs::remove_dir_all(&root);
}
