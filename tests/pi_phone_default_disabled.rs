//! Default-disabled package policy: `@malinamnam/pi-phone`.
//!
//! The package stays installed and listed in `settings.packages`, but pir
//! skips its extension paths unless the user opts in: it starts a local web
//! server and costs about 14 MB of QuickJS heap on every startup. These tests
//! pin the whole seam in hermetic agent dirs (no network, no real npm):
//!
//! 1. an `npm:@malinamnam/pi-phone@<version>` entry resolves to
//!    `<agent>/npm/node_modules/@malinamnam/pi-phone` and is skipped by
//!    default, while another package keeps loading;
//! 2. a local checkout path (recognized through its `package.json` `"name"`)
//!    is skipped before the runtime starts — a plan left with builtins never
//!    creates the QuickJS actor;
//! 3. `defaultDisabledPackagesEnabled: true` in the global settings scope
//!    loads the real JS package (a minimal fixture with the package's
//!    manifest shape);
//! 4. an explicit CLI `-e` path naming the package (entry file or checkout
//!    directory) wins over the default policy;
//! 5. the skip emits exactly one `[ext]` diagnostic carrying the exact
//!    opt-in (captured through the same file-writing tracing subscriber the
//!    extension stdio-routing suite uses).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pir::config::settings::SettingsManager;
use pir::extensions::ExtensionConfig;
use pir::plugins::{
    default_disabled_package_name, emit_default_disabled_package_diagnostics,
    filter_default_disabled_packages, order_extension_paths, DefaultDisabledPackageSkip,
};
use serde_json::json;

const PHONE_NAME: &str = "@malinamnam/pi-phone";
const PHONE_MARKER: &str = "__pi_phone_loaded";
const OTHER_NAME: &str = "other-pkg";
const OTHER_MARKER: &str = "__other_pkg_loaded";

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-pi-phone-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: Some("pi-phone-policy".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        // Some(...) keeps limit resolution hermetic (no ambient agent dir).
        limits: Some(Default::default()),
    }
}

/// A minimal package with the real manifest shape: `pi.extensions` pointing
/// at an ESM entry whose factory sets a global marker when it loads.
fn write_package(root: &Path, name: &str, marker: &str) {
    write_package_version(root, name, marker, "0.0.0");
}

/// [`write_package`] with the installed `package.json` version the startup
/// resolver checks against a configured `npm:<name>@<version>` range.
fn write_package_version(root: &Path, name: &str, marker: &str, version: &str) {
    std::fs::create_dir_all(root).expect("package dir");
    std::fs::write(
        root.join("package.json"),
        serde_json::to_string_pretty(&json!({
            "name": name,
            "version": version,
            "type": "module",
            "pi": { "extensions": ["./index.js"] },
        }))
        .expect("manifest json"),
    )
    .expect("manifest");
    std::fs::write(
        root.join("index.js"),
        format!("export default (pi) => {{ globalThis.{marker} = true; }};\n"),
    )
    .expect("entry");
}

fn write_global_settings(agent_dir: &Path, packages: &[String], opt_in: bool) {
    let mut settings = json!({ "packages": packages });
    if opt_in {
        settings["defaultDisabledPackagesEnabled"] = json!(true);
    }
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("settings json"),
    )
    .expect("settings");
}

fn load_settings(agent_dir: &Path, cwd: &Path) -> SettingsManager {
    SettingsManager::load(agent_dir, &cwd.join(".pi"), cwd, false)
}

/// Resolve `settings.packages` into the ordered extension plan, exactly like
/// the CLI host does (package paths rank below settings/auto discovery and
/// above builtins).
fn resolved_plan(settings: &SettingsManager, agent_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let resources = pir::coding_agent::package_manager::resolve_package_resources_with_home(
        settings, agent_dir, cwd, false, None,
    );
    order_extension_paths(&[], &resources.extensions, false)
}

fn has_prefix(paths: &[PathBuf], prefix: &Path) -> bool {
    paths.iter().any(|path| path.starts_with(prefix))
}

#[test]
fn local_checkout_package_json_name_is_recognized() {
    // Hermetic stand-in for a real pi-phone working copy (no machine-specific
    // path): the package is identified by its `package.json` name, never by
    // the directory name alone.
    let temp = TempDir::new("checkout-recognized");
    let checkout = temp.0.join("pi-phone");
    write_package(&checkout, PHONE_NAME, PHONE_MARKER);
    assert_eq!(
        default_disabled_package_name(&checkout),
        Some(PHONE_NAME),
        "the local checkout's package.json name must identify it"
    );

    // A differently-named checkout is not the default-disabled package.
    let renamed = temp.0.join("not-pi-phone");
    write_package(&renamed, "some-other-phone-ui", OTHER_MARKER);
    assert_eq!(default_disabled_package_name(&renamed), None);
}

#[tokio::test(flavor = "current_thread")]
async fn npm_spec_package_is_skipped_by_default_while_others_load() {
    let temp = TempDir::new("npm-spec");
    let agent_dir = temp.0.join("agent");
    let cwd = temp.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let phone_root = agent_dir
        .join("npm")
        .join("node_modules")
        .join("@malinamnam")
        .join("pi-phone");
    let other_root = agent_dir.join("npm").join("node_modules").join(OTHER_NAME);
    // The installed version must satisfy the configured `@0.0.13` spec so the
    // startup resolver does not treat it as version drift and reinstall.
    write_package_version(&phone_root, PHONE_NAME, PHONE_MARKER, "0.0.13");
    write_package(&other_root, OTHER_NAME, OTHER_MARKER);
    write_global_settings(
        &agent_dir,
        &[
            format!("npm:{PHONE_NAME}@0.0.13"),
            format!("npm:{OTHER_NAME}"),
        ],
        false,
    );

    let settings = load_settings(&agent_dir, &cwd);
    assert!(!settings.default_disabled_packages_enabled());
    let ordered = resolved_plan(&settings, &agent_dir, &cwd);
    assert!(
        has_prefix(&ordered, &phone_root),
        "the npm spec must resolve to its node_modules entry: {ordered:?}"
    );

    let (effective, skips) = filter_default_disabled_packages(&ordered, &[], false);
    assert_eq!(skips.len(), 1, "one skip per package: {skips:?}");
    assert_eq!(skips[0].package_name, PHONE_NAME);
    assert!(skips[0].path.starts_with(&phone_root), "{skips:?}");
    assert!(!has_prefix(&effective, &phone_root), "{effective:?}");
    assert!(
        has_prefix(&effective, &other_root),
        "other packages are unaffected: {effective:?}"
    );

    // Real runtime load: pi-phone's factory never runs, the other package's
    // does.
    let handle = pir::agent_session::load_extension_runtime(&effective, test_config(&cwd))
        .await
        .expect("the other package keeps the JS runtime alive");
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__pi_phone_loaded ?? null)")
            .await
            .expect("probe"),
        json!(null),
        "the skipped package must not evaluate"
    );
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__other_pkg_loaded ?? null)")
            .await
            .expect("probe"),
        json!(true),
        "unrelated packages still load"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn local_checkout_is_skipped_before_the_runtime_starts() {
    let temp = TempDir::new("local-path");
    let agent_dir = temp.0.join("agent");
    let cwd = temp.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    // A local checkout (the shape of the pi-phone working copy): package.json
    // name identifies it, the directory name alone is not trusted.
    let checkout = temp.0.join("pi-phone");
    write_package(&checkout, PHONE_NAME, PHONE_MARKER);
    write_global_settings(
        &agent_dir,
        &[checkout.to_string_lossy().into_owned()],
        false,
    );

    let settings = load_settings(&agent_dir, &cwd);
    let ordered = resolved_plan(&settings, &agent_dir, &cwd);
    assert!(
        has_prefix(&ordered, &checkout),
        "the local package must contribute its entry: {ordered:?}"
    );

    let (effective, skips) = filter_default_disabled_packages(&ordered, &[], false);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(skips[0].package_name, PHONE_NAME);
    assert!(!has_prefix(&effective, &checkout), "{effective:?}");

    // Builtin-only plan: the handle is plan-only (no actor thread, no QuickJS
    // engine), so the skipped package cannot have started one.
    if let Some(handle) =
        pir::agent_session::load_extension_runtime(&effective, test_config(&cwd)).await
    {
        assert!(
            !handle.has_js_runtime(),
            "a plan without the package must stay plan-only"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn opt_in_setting_loads_the_real_js_package() {
    let temp = TempDir::new("opt-in");
    let agent_dir = temp.0.join("agent");
    let cwd = temp.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let phone_root = agent_dir
        .join("npm")
        .join("node_modules")
        .join("@malinamnam")
        .join("pi-phone");
    write_package(&phone_root, PHONE_NAME, PHONE_MARKER);
    write_global_settings(&agent_dir, &[format!("npm:{PHONE_NAME}")], true);

    let settings = load_settings(&agent_dir, &cwd);
    assert!(
        settings.default_disabled_packages_enabled(),
        "the global opt-in key must be read"
    );
    let ordered = resolved_plan(&settings, &agent_dir, &cwd);
    let (effective, skips) = filter_default_disabled_packages(
        &ordered,
        &[],
        settings.default_disabled_packages_enabled(),
    );
    assert!(skips.is_empty(), "{skips:?}");
    assert!(has_prefix(&effective, &phone_root), "{effective:?}");

    let handle = pir::agent_session::load_extension_runtime(&effective, test_config(&cwd))
        .await
        .expect("the opted-in package loads a real JS runtime");
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__pi_phone_loaded ?? null)")
            .await
            .expect("probe"),
        json!(true),
        "the package's JS factory must run when opted in"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_cli_extension_wins_over_the_default_policy() {
    let temp = TempDir::new("cli-opt-in");
    let agent_dir = temp.0.join("agent");
    let cwd = temp.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let phone_root = agent_dir
        .join("npm")
        .join("node_modules")
        .join("@malinamnam")
        .join("pi-phone");
    write_package(&phone_root, PHONE_NAME, PHONE_MARKER);
    write_global_settings(&agent_dir, &[format!("npm:{PHONE_NAME}")], false);
    let settings = load_settings(&agent_dir, &cwd);
    let defaults = pir::coding_agent::package_manager::resolve_package_resources_with_home(
        &settings, &agent_dir, &cwd, false, None,
    )
    .extensions;

    // (a) `-e <entry file>`: exact spelling.
    let entry = phone_root.join("index.js");
    let cli = vec![entry.clone()];
    let ordered = order_extension_paths(&cli, &defaults, false);
    let (effective, skips) = filter_default_disabled_packages(&ordered, &cli, false);
    assert!(skips.is_empty(), "explicit -e must not skip: {skips:?}");
    assert!(has_prefix(&effective, &phone_root), "{effective:?}");

    let handle = pir::agent_session::load_extension_runtime(&effective, test_config(&cwd))
        .await
        .expect("explicit -e loads the package");
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__pi_phone_loaded ?? null)")
            .await
            .expect("probe"),
        json!(true)
    );

    // (b) `-e <checkout directory>`: the package it contains is opted in even
    // though the default plan names the inner entry file.
    let cli = vec![phone_root.clone()];
    let ordered = order_extension_paths(&cli, &defaults, false);
    let (effective, skips) = filter_default_disabled_packages(&ordered, &cli, false);
    assert!(
        skips.is_empty(),
        "an -e directory containing the package opts it in: {skips:?}"
    );
    assert!(has_prefix(&effective, &phone_root), "{effective:?}");
}

// ---------------------------------------------------------------------------
// Diagnostic emission
// ---------------------------------------------------------------------------

fn capture_log_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("pir-pi-phone-policy-{}.log", std::process::id()))
    })
}

/// Install the capture subscriber once per test process (the shape the TUI
/// host installs, writing to a file).
fn install_capture_subscriber() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let path = capture_log_path();
        let _ = std::fs::remove_file(path);
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return false;
        };
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::sync::Arc::new(file))
            .finish();
        tracing::subscriber::set_global_default(subscriber).is_ok()
    })
}

#[test]
fn diagnostic_names_the_exact_opt_in_once() {
    assert!(
        install_capture_subscriber(),
        "a global tracing subscriber must be installable in this test process"
    );

    let temp = TempDir::new("diagnostic");
    let agent_dir = temp.0.join("agent");
    let cwd = temp.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let phone_root = agent_dir
        .join("npm")
        .join("node_modules")
        .join("@malinamnam")
        .join("pi-phone");
    write_package(&phone_root, PHONE_NAME, PHONE_MARKER);
    write_global_settings(&agent_dir, &[format!("npm:{PHONE_NAME}")], false);

    let settings = load_settings(&agent_dir, &cwd);
    let ordered = resolved_plan(&settings, &agent_dir, &cwd);
    let (_, skips) = filter_default_disabled_packages(&ordered, &[], false);
    assert_eq!(skips.len(), 1, "{skips:?}");
    let skip: &DefaultDisabledPackageSkip = &skips[0];
    assert!(
        skip.message.contains("disabled by default"),
        "{}",
        skip.message
    );
    assert!(
        skip.message
            .contains("\"defaultDisabledPackagesEnabled\": true"),
        "the exact settings opt-in must be named: {}",
        skip.message
    );
    assert!(
        skip.message.contains("-e \"") && skip.message.contains("index.js"),
        "the exact CLI opt-in must be named: {}",
        skip.message
    );

    emit_default_disabled_package_diagnostics(&skips);

    let log = std::fs::read_to_string(capture_log_path()).expect("capture log");
    let needle = format!("[ext] {PHONE_NAME} is disabled by default");
    let matches: Vec<&str> = log.lines().filter(|line| line.contains(&needle)).collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one diagnostic per skipped package; log was:\n{log}"
    );
    assert!(
        matches[0].contains("defaultDisabledPackagesEnabled"),
        "{}",
        matches[0]
    );
}

#[test]
fn project_scope_cannot_opt_in() {
    let temp = TempDir::new("project-opt-in");
    let agent_dir = temp.0.join("agent");
    let cwd = temp.0.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(cwd.join(".pi")).expect("project settings dir");
    std::fs::write(
        cwd.join(".pi").join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultDisabledPackagesEnabled": true,
        }))
        .expect("project settings json"),
    )
    .expect("project settings");

    // Even a TRUSTED project scope must not turn the expensive package on:
    // the accessor reads the global scope only.
    let settings = SettingsManager::load(&agent_dir, &cwd.join(".pi"), &cwd, true);
    assert!(
        !settings.default_disabled_packages_enabled(),
        "project settings must not opt in to default-disabled packages"
    );
}

/// Native `builtin:phone` and the JS package never load together: with the
/// builtin on the plan, the on-disk JS copy is dropped (one phone server, one
/// UI), and the builtin itself stays on the builtin half of the partition.
#[test]
fn native_phone_builtin_wins_over_the_js_package() {
    // Hermetic JS package path (a temp checkout identified by its
    // package.json name), so the precedence check never depends on a
    // machine-specific checkout or a relative `logs/` directory.
    let temp = TempDir::new("builtin-precedence");
    let js_phone = temp.0.join("pi-phone");
    write_package(&js_phone, PHONE_NAME, PHONE_MARKER);
    let paths = vec![PathBuf::from("builtin:phone"), js_phone];
    let (builtins, files) = pir::plugins::partition_paths(&paths);
    assert_eq!(builtins, vec!["phone".to_owned()]);
    assert!(
        files.is_empty(),
        "the JS pi-phone path must be dropped when the native builtin is on: {files:?}"
    );
}
