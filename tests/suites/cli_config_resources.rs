//! R lane: interactive/scriptable `pi config` resource management (§2 #6).
//!
//! Upstream `handleConfigCommand` opens `ConfigSelectorComponent`, whose
//! space/enter toggle writes a `+pattern`/`-pattern` entry into the package's
//! settings object (config-selector.ts:586-643). pir's subcommand path has no
//! TUI session, so the same write lives behind a line-oriented selection plus
//! the scriptable `--enable`/`--disable`/`--json` surface; these tests pin the
//! discovery, the filter write, the ambiguity/trust gates, and the
//! confirmation text.

use std::path::{Path, PathBuf};

use clap::Parser;
use pir::coding_agent::cli::{Cli, Command};
use pir::coding_agent::package_manager::{
    config_json, config_resource_rows, run_config_interactive_with, set_config_resource_enabled,
    ConfigResource,
};
use pir::config::settings::SettingsManager;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-config-resources-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    std::fs::write(path, contents).expect("write file");
}

/// One installed user-scope npm package with every resource kind.
fn install_user_package(agent: &Path, name: &str) {
    let root = agent.join("npm").join("node_modules").join(name);
    write_file(&root, "extensions/a.js", "export default {};");
    write_file(&root, "skills/s1/SKILL.md", "# skill");
    write_file(&root, "prompts/p.md", "# prompt");
    write_file(&root, "themes/t.json", "{}");
}

fn settings_with_packages(agent: &Path, cwd: &Path, packages: &[&str]) -> SettingsManager {
    write_file(
        agent,
        "settings.json",
        &serde_json::json!({ "packages": packages }).to_string(),
    );
    SettingsManager::load(agent, &cwd.join(".pi"), cwd, false)
}

fn row<'a>(rows: &'a [ConfigResource], kind: &str, pattern: &str) -> &'a ConfigResource {
    rows.iter()
        .find(|row| row.kind == kind && row.pattern == pattern)
        .unwrap_or_else(|| panic!("missing {kind}:{pattern} in {rows:?}"))
}

#[test]
fn config_discovers_package_resources_and_writes_toggle_filters() {
    let agent = temp_dir("toggle");
    let cwd = temp_dir("cwd");
    install_user_package(&agent, "foo");
    let mut settings = settings_with_packages(&agent, &cwd, &["npm:foo"]);

    let rows = config_resource_rows(&settings, &agent, &cwd, false);
    assert_eq!(rows.len(), 4, "one row per resource kind: {rows:?}");
    for (kind, pattern) in [
        ("extensions", "extensions/a.js"),
        ("skills", "skills/s1"),
        ("prompts", "prompts/p.md"),
        ("themes", "themes/t.json"),
    ] {
        let row = row(&rows, kind, pattern);
        assert!(row.enabled, "{kind} starts enabled");
        assert_eq!(row.source, "npm:foo");
        assert_eq!(row.scope, "user");
        assert_eq!(row.selector(), format!("{kind}:{pattern}"));
    }

    // Disable the extension: the package entry becomes the object form with a
    // `-extensions/a.js` filter (upstream `togglePackageResource`).
    let message = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions:extensions/a.js",
        None,
        false,
    )
    .expect("disable");
    assert_eq!(
        message,
        "Disabled extensions resource \"extensions/a.js\" for npm:foo (global settings)"
    );
    let packages = &settings.global_raw["packages"];
    assert_eq!(packages[0]["source"], "npm:foo");
    assert_eq!(
        packages[0]["extensions"],
        serde_json::json!(["-extensions/a.js"])
    );

    // A fresh load sees the filter; the other resources stay enabled.
    let settings = SettingsManager::load(&agent, &cwd.join(".pi"), &cwd, false);
    let rows = config_resource_rows(&settings, &agent, &cwd, false);
    assert!(!row(&rows, "extensions", "extensions/a.js").enabled);
    assert!(row(&rows, "skills", "skills/s1").enabled);

    // Re-enable: the entry is replaced (never duplicated) with `+`.
    let mut settings = settings;
    set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions:extensions/a.js",
        None,
        true,
    )
    .expect("enable");
    assert_eq!(
        settings.global_raw["packages"][0]["extensions"],
        serde_json::json!(["+extensions/a.js"])
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn config_json_reports_scope_packages_and_resource_state() {
    let agent = temp_dir("json");
    let cwd = temp_dir("json-cwd");
    install_user_package(&agent, "foo");
    let mut settings = settings_with_packages(&agent, &cwd, &["npm:foo"]);
    set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions:extensions/a.js",
        None,
        false,
    )
    .expect("disable");

    let value = config_json(&settings, &agent, &cwd, false, false);
    assert_eq!(value["writeScope"], "global");
    assert_eq!(value["projectTrusted"], false);
    assert_eq!(value["packages"][0]["source"], "npm:foo");
    assert_eq!(value["packages"][0]["installed"], true);
    let resources = value["resources"].as_array().expect("resources array");
    let extension = resources
        .iter()
        .find(|resource| resource["selector"] == "extensions:extensions/a.js")
        .expect("extension row");
    assert_eq!(extension["enabled"], false);
    assert_eq!(extension["selector"], "extensions:extensions/a.js");
    assert_eq!(extension["scope"], "user");
    assert_eq!(
        resources
            .iter()
            .filter(|resource| resource["enabled"] == true)
            .count(),
        3
    );

    // `--local` changes the reported write scope (no project packages here).
    let local = config_json(&settings, &agent, &cwd, true, true);
    assert_eq!(local["writeScope"], "project");
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn config_toggle_requires_a_source_when_two_packages_share_a_pattern() {
    let agent = temp_dir("ambiguous");
    let cwd = temp_dir("ambiguous-cwd");
    for name in ["foo", "bar"] {
        let root = agent.join("npm").join("node_modules").join(name);
        write_file(&root, "extensions/shared.js", "export default {};");
    }
    let mut settings = settings_with_packages(&agent, &cwd, &["npm:foo", "npm:bar"]);

    let error = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions:extensions/shared.js",
        None,
        false,
    )
    .expect_err("ambiguous selector must fail");
    let message = error.to_string();
    assert!(message.contains("ambiguous"), "got: {message}");
    assert!(message.contains("npm:foo"), "got: {message}");
    assert!(message.contains("npm:bar"), "got: {message}");

    // Naming the package resolves it.
    let message = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions:extensions/shared.js",
        Some("npm:bar"),
        false,
    )
    .expect("source-qualified toggle");
    assert!(message.contains("npm:bar"), "got: {message}");
    let packages = settings.global_raw["packages"]
        .as_array()
        .expect("packages");
    assert_eq!(packages[0], serde_json::json!("npm:foo"));
    assert_eq!(
        packages[1]["extensions"],
        serde_json::json!(["-extensions/shared.js"])
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn config_toggle_errors_are_explicit() {
    let agent = temp_dir("errors");
    let cwd = temp_dir("errors-cwd");
    install_user_package(&agent, "foo");
    let mut settings = settings_with_packages(&agent, &cwd, &["npm:foo"]);

    let bad_kind = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "widgets:extensions/a.js",
        None,
        false,
    )
    .expect_err("unknown kind");
    assert!(bad_kind.to_string().contains("Invalid resource kind"));
    let no_colon = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions/a.js",
        None,
        false,
    )
    .expect_err("missing selector separator");
    assert!(no_colon.to_string().contains("expected <kind>:<pattern>"));
    let missing = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        "extensions:extensions/nope.js",
        None,
        false,
    )
    .expect_err("missing resource");
    assert!(missing.to_string().contains("No extensions resource"));

    // A project-scope write is refused while the project is untrusted.
    let untrusted = set_config_resource_enabled(
        &mut settings,
        &agent,
        &cwd,
        false,
        true,
        "extensions:extensions/a.js",
        None,
        false,
    )
    .expect_err("untrusted project write");
    assert!(untrusted.to_string().contains("not trusted"));
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn config_interactive_toggle_writes_and_confirms() {
    let agent = temp_dir("interactive");
    let cwd = temp_dir("interactive-cwd");
    install_user_package(&agent, "foo");
    let mut settings = settings_with_packages(&agent, &cwd, &["npm:foo"]);

    // Row 1 is the extension; toggling it disables the resource, then Enter
    // finishes the selection.
    let mut reader = std::io::Cursor::new(b"1\n\n".to_vec());
    let mut writer: Vec<u8> = Vec::new();
    run_config_interactive_with(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        &mut reader,
        &mut writer,
    )
    .expect("interactive toggle");
    let output = String::from_utf8(writer).expect("utf-8 output");
    assert!(
        output.contains("Resource configuration (global settings):"),
        "got: {output}"
    );
    assert!(
        output.contains("Disabled extensions resource \"extensions/a.js\""),
        "a toggle confirms what it wrote: {output}"
    );
    assert!(
        output.contains("Toggle resource number"),
        "the next selection is offered: {output}"
    );
    assert_eq!(
        settings.global_raw["packages"][0]["extensions"],
        serde_json::json!(["-extensions/a.js"])
    );

    // EOF finishes without writing anything more.
    let mut reader = std::io::Cursor::new(Vec::new());
    let mut writer: Vec<u8> = Vec::new();
    run_config_interactive_with(
        &mut settings,
        &agent,
        &cwd,
        false,
        false,
        &mut reader,
        &mut writer,
    )
    .expect("eof finishes");
    assert_eq!(
        settings.global_raw["packages"][0]["extensions"],
        serde_json::json!(["-extensions/a.js"]),
        "no toggle on EOF"
    );
    let _ = std::fs::remove_dir_all(&agent);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn config_cli_flags_parse() {
    let cli = Cli::try_parse_from([
        "pir",
        "config",
        "--local",
        "--json",
        "--enable",
        "extensions:extensions/a.js",
        "--source",
        "npm:foo",
    ])
    .expect("config flags parse");
    match cli.command.expect("config command") {
        Command::Config {
            local,
            json,
            enable,
            disable,
            source,
        } => {
            assert!(local);
            assert!(json);
            assert_eq!(enable.as_deref(), Some("extensions:extensions/a.js"));
            assert!(disable.is_none());
            assert_eq!(source.as_deref(), Some("npm:foo"));
        }
        other => panic!("expected config, got {other:?}"),
    }

    let cli = Cli::try_parse_from(["pir", "config", "--disable", "themes:themes/t.json"])
        .expect("disable flag parses");
    match cli.command.expect("config command") {
        Command::Config {
            local,
            enable,
            disable,
            ..
        } => {
            assert!(!local);
            assert!(enable.is_none());
            assert_eq!(disable.as_deref(), Some("themes:themes/t.json"));
        }
        other => panic!("expected config, got {other:?}"),
    }
}
