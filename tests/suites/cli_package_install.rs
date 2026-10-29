//! F1 PKG lane (§5.2 #5/#9): startup auto-install of missing/version-drifted
//! npm packages and the `pi` manifest's `+`/`-`/`!` resource filters.
//!
//! Upstream `DefaultPackageManager.resolve()` installs a missing npm/git
//! package and reinstalls an npm package whose installed version no longer
//! satisfies the configured version/range (`installedNpmMatchesConfiguredVersion`,
//! package-manager.ts:935-951,1318-1366,1533-1539), skipping the package on
//! failure without aborting startup. The package `pi` manifest splits plain
//! entries (sources) from `+`/`-`/`!` overrides (filters) in
//! `addManifestEntries` (package-manager.ts:2384-2414).
//!
//! These tests drive the resolver directly with a fake npm command, so no
//! registry/network access happens.

use std::path::{Path, PathBuf};

use pir::coding_agent::package_manager::{
    resolve_package_resources_with_home_and_diagnostics, PackageInstallFailure,
    PackageResourceResolution, PackageResources,
};
use pir::config::settings::SettingsManager;
use serde_json::{json, Value};

fn unique_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-pkg-install-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    std::fs::write(path, contents).expect("write file");
}

/// A staged npm package tree the fake npm copies into its install target.
fn stage_package(root: &Path, name: &str, version: &str, marker: &str) -> PathBuf {
    let stage = root.join(format!("stage-{name}-{version}-{marker}"));
    write_file(
        &stage,
        "package.json",
        &json!({
            "name": name,
            "version": version,
            "pi": { "extensions": [format!("./extensions/{marker}.js")] },
        })
        .to_string(),
    );
    write_file(
        &stage,
        &format!("extensions/{marker}.js"),
        &format!("export default () => {{ globalThis.{marker} = true; }};\n"),
    );
    stage
}

/// Install settings for the project scope plus a fake `npmCommand`.
fn project_settings(cwd: &Path, packages: Value, npm_command: Vec<String>) -> SettingsManager {
    let agent = cwd.join("agent");
    let project_dir = cwd.join(".pi");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let settings = json!({
        "packages": packages,
        "npmCommand": npm_command,
    });
    std::fs::write(
        project_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("settings json"),
    )
    .expect("write settings");
    SettingsManager::load(&agent, &project_dir, cwd, true)
}

fn resolve_report(
    settings: &SettingsManager,
    cwd: &Path,
    offline: bool,
) -> PackageResourceResolution {
    resolve_package_resources_with_home_and_diagnostics(
        settings,
        &cwd.join("agent"),
        cwd,
        true,
        None,
        offline,
    )
}

fn resolve(
    settings: &SettingsManager,
    cwd: &Path,
    offline: bool,
) -> (PackageResources, Vec<PackageInstallFailure>) {
    let resolution = resolve_report(settings, cwd, offline);
    (resolution.resources, resolution.failures)
}

fn relative_paths(paths: &[PathBuf], root: &Path) -> Vec<String> {
    let mut relative: Vec<String> = paths
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    relative.sort();
    relative
}

/// A fake npm that logs every invocation and installs the staged package as
/// `<install root>/node_modules/<name>`. `fail_on` turns any invocation whose
/// arguments contain that token into an exit-1 failure (the "uninstallable
/// package" case).
fn fake_npm(
    root: &Path,
    install_root: &Path,
    name: &str,
    stage: &Path,
    fail_on: Option<&str>,
    noise: bool,
) -> Vec<String> {
    let log = root.join("npm-invocations.log");
    #[cfg(windows)]
    {
        let noise_line = if noise { "echo NOISE-FROM-NPM\r\n" } else { "" };
        let script = root.join("fake-npm.bat");
        let target = install_root.join("node_modules").join(name);
        let fail_line = fail_on.map_or_else(String::new, |token| {
            format!("echo %* | findstr /c:\"{token}\" >nul\r\nif not errorlevel 1 exit /b 1\r\n")
        });
        std::fs::write(
            &script,
            format!(
                "@echo off\r\n\
                 echo INVOKED %*>>\"{}\"\r\n\
                 {noise_line}\
                 {fail_line}\
                 mkdir \"{}\" 2>nul\r\n\
                 if exist \"{}\" rmdir /s /q \"{}\"\r\n\
                 xcopy \"{}\" \"{}\" /E /I /Y >nul\r\n\
                 exit /b 0\r\n",
                log.display(),
                install_root.join("node_modules").display(),
                target.display(),
                target.display(),
                stage.display(),
                target.display(),
            ),
        )
        .expect("fake npm script");
        vec![script.display().to_string()]
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("fake-npm.sh");
        let target = install_root.join("node_modules").join(name);
        let fail_case = fail_on.map_or_else(String::new, |token| {
            format!("case \"$*\" in *{token}*) exit 1 ;; esac\n")
        });
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo \"INVOKED $*\" >> \"{}\"\n\
                 {}\
                 {fail_case}\
                 mkdir -p \"{}\"\n\
                 rm -rf \"{}\"\n\
                 cp -R \"{}\" \"{}\"\n\
                 exit 0\n",
                log.display(),
                if noise { "echo NOISE-FROM-NPM\n" } else { "" },
                install_root.join("node_modules").display(),
                target.display(),
                stage.display(),
                target.display(),
            ),
        )
        .expect("fake npm script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake npm");
        vec![script.display().to_string()]
    }
}

fn installed_version(package_root: &Path) -> String {
    let text =
        std::fs::read_to_string(package_root.join("package.json")).expect("installed package.json");
    let value: Value = serde_json::from_str(&text).expect("package.json json");
    value["version"].as_str().expect("version").to_owned()
}

#[test]
fn missing_npm_package_is_installed_into_the_project_pi_root() {
    let cwd = unique_dir("missing");
    let stage = stage_package(&cwd, "auto-npm", "1.2.0", "auto_npm_loaded");
    let install_root = cwd.join(".pi").join("npm");
    let npm = fake_npm(&cwd, &install_root, "auto-npm", &stage, None, false);
    let settings = project_settings(&cwd, json!(["npm:auto-npm@^1.0.0"]), npm);

    let (resources, failures) = resolve(&settings, &cwd, false);

    assert!(failures.is_empty(), "install must succeed: {failures:?}");
    let package_root = install_root.join("node_modules").join("auto-npm");
    assert_eq!(installed_version(&package_root), "1.2.0");
    assert_eq!(
        relative_paths(&resources.extensions, &package_root),
        vec!["extensions/auto_npm_loaded.js"],
        "the installed package's manifest resources must be read"
    );
    // Project scope: everything lives under `<cwd>/.pi`, never the agent dir.
    assert!(
        !cwd.join("agent").join("npm").exists(),
        "project installs must not fall back to the user npm root"
    );
    assert_eq!(
        std::fs::read_to_string(install_root.join(".gitignore")).expect("npm .gitignore"),
        "*\n!.gitignore\n",
        "the managed npm root keeps upstream's ignore rule"
    );
    let package_json =
        std::fs::read_to_string(install_root.join("package.json")).expect("root package.json");
    assert!(
        package_json.contains("pi-extensions"),
        "got: {package_json}"
    );
    assert!(
        cwd.join("npm-invocations.log").exists(),
        "the configured npm command must have run"
    );
}

#[test]
fn npm_version_drift_reinstalls_only_when_the_range_no_longer_matches() {
    let cwd = unique_dir("drift");
    // Pre-installed 1.2.0: `^1.0.0` still matches it, an exact 2.0.0 does not.
    let package_root = cwd
        .join(".pi")
        .join("npm")
        .join("node_modules")
        .join("drift-pkg");
    write_file(
        &package_root,
        "package.json",
        &json!({
            "name": "drift-pkg",
            "version": "1.2.0",
            "pi": { "extensions": ["./extensions/old.js"] },
        })
        .to_string(),
    );
    write_file(&package_root, "extensions/old.js", "export default {};\n");

    let stage = stage_package(&cwd, "drift-pkg", "2.0.0", "drift_new");
    let install_root = cwd.join(".pi").join("npm");
    let npm = fake_npm(&cwd, &install_root, "drift-pkg", &stage, None, false);
    let log = cwd.join("npm-invocations.log");

    let settings = project_settings(&cwd, json!(["npm:drift-pkg@^1.0.0"]), npm.clone());
    let (resources, failures) = resolve(&settings, &cwd, false);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        !log.exists(),
        "an installed version inside the configured range must not reinstall"
    );
    assert_eq!(installed_version(&package_root), "1.2.0");
    assert_eq!(
        relative_paths(&resources.extensions, &package_root),
        vec!["extensions/old.js"]
    );

    // Same source identity, different configured version: reinstall.
    let settings = project_settings(&cwd, json!(["npm:drift-pkg@2.0.0"]), npm);
    let (resources, failures) = resolve(&settings, &cwd, false);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(log.exists(), "the version mismatch must trigger an install");
    assert_eq!(installed_version(&package_root), "2.0.0");
    assert_eq!(
        relative_paths(&resources.extensions, &package_root),
        vec!["extensions/drift_new.js"]
    );
}

#[test]
fn offline_mode_skips_missing_and_version_drifted_packages() {
    let cwd = unique_dir("offline");
    let drifted = cwd
        .join(".pi")
        .join("npm")
        .join("node_modules")
        .join("offline-pkg");
    write_file(
        &drifted,
        "package.json",
        &json!({ "name": "offline-pkg", "version": "1.0.0" }).to_string(),
    );
    let stage = stage_package(&cwd, "offline-pkg", "2.0.0", "offline_new");
    let install_root = cwd.join(".pi").join("npm");
    let npm = fake_npm(&cwd, &install_root, "offline-pkg", &stage, None, false);
    let settings = project_settings(
        &cwd,
        json!(["npm:offline-pkg@2.0.0", "npm:offline-missing@1.0.0"]),
        npm,
    );

    let (resources, failures) = resolve(&settings, &cwd, true);

    assert!(
        failures.is_empty(),
        "offline skips are not install failures"
    );
    assert!(
        !cwd.join("npm-invocations.log").exists(),
        "offline mode must never run the package manager"
    );
    assert_eq!(
        installed_version(&drifted),
        "1.0.0",
        "the drifted install is left untouched"
    );
    assert!(
        !install_root
            .join("node_modules")
            .join("offline-missing")
            .exists(),
        "a missing package stays uninstalled"
    );
    assert!(
        resources.extensions.is_empty(),
        "offline skips contribute no resources: {:?}",
        resources.extensions
    );
}

#[test]
fn install_failure_is_a_startup_diagnostic_and_does_not_block_other_packages() {
    let cwd = unique_dir("failure");
    let stage = stage_package(&cwd, "good-pkg", "1.0.0", "good_pkg_loaded");
    let install_root = cwd.join(".pi").join("npm");
    let npm = fake_npm(
        &cwd,
        &install_root,
        "good-pkg",
        &stage,
        Some("broken-pkg"),
        false,
    );
    let settings = project_settings(&cwd, json!(["npm:broken-pkg", "npm:good-pkg"]), npm);

    let (resources, failures) = resolve(&settings, &cwd, false);

    assert_eq!(
        failures.len(),
        1,
        "one diagnostic per failed install: {failures:?}"
    );
    let failure = &failures[0];
    assert_eq!(failure.source, "npm:broken-pkg");
    assert_eq!(failure.scope, "project");
    assert!(
        failure.message.contains("failed with code 1"),
        "the manager's error is captured: {}",
        failure.message
    );
    let warning = failure.warning();
    assert!(
        warning.contains("Extension package \"npm:broken-pkg\": failed to install"),
        "got: {warning}"
    );
    assert!(
        warning
            .contains("Run \"pir remove npm:broken-pkg --local\" to remove it from settings.json."),
        "the project-scope removal hint is included: {warning}"
    );

    // Startup continued past the failure: the next package installed and
    // contributed its resources.
    let good_root = install_root.join("node_modules").join("good-pkg");
    assert_eq!(installed_version(&good_root), "1.0.0");
    assert_eq!(
        relative_paths(&resources.extensions, &good_root),
        vec!["extensions/good_pkg_loaded.js"]
    );
}

/// R4-B P2-7: startup resolves packages several times per launch (untrusted
/// project plan, strict extension gate, resource discovery). A broken source
/// must be attempted at most once: a cached failure is replayed as the same
/// diagnostic without spawning npm again.
#[test]
fn a_broken_source_is_installed_at_most_once_per_process() {
    let cwd = unique_dir("single-attempt");
    let stage = stage_package(&cwd, "bad-pkg", "1.0.0", "bad_pkg_loaded");
    let install_root = cwd.join(".pi").join("npm");
    let npm = fake_npm(
        &cwd,
        &install_root,
        "bad-pkg",
        &stage,
        Some("bad-pkg"),
        false,
    );
    let settings = project_settings(&cwd, json!(["npm:bad-pkg"]), npm);
    let log = cwd.join("npm-invocations.log");

    let (resources, failures) = resolve(&settings, &cwd, false);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(resources.extensions.is_empty());
    let first = std::fs::read_to_string(&log).expect("the install attempt is logged");
    assert_eq!(
        first.lines().count(),
        1,
        "the first resolve attempts the install once: {first:?}"
    );

    // Second and third resolves of the same source in the same process: the
    // failure is replayed (startup still reports it) but npm never runs again.
    for attempt in 2..=3 {
        let (resources, failures) = resolve(&settings, &cwd, false);
        assert_eq!(
            failures.len(),
            1,
            "resolve #{attempt} still reports the failed package: {failures:?}"
        );
        assert_eq!(failures[0].source, "npm:bad-pkg");
        assert!(resources.extensions.is_empty());
        let recorded = std::fs::read_to_string(&log).expect("the install invocation log");
        assert_eq!(
            recorded.lines().count(),
            1,
            "resolve #{attempt} must reuse the cached failure, got: {recorded:?}"
        );
    }
}

#[test]
fn manifest_plus_minus_bang_filters_apply_to_the_plain_entries() {
    let cwd = unique_dir("manifest-filter");
    let package_root = cwd
        .join(".pi")
        .join("npm")
        .join("node_modules")
        .join("manifest-pkg");
    write_file(
        &package_root,
        "package.json",
        &json!({
            "name": "manifest-pkg",
            "version": "1.0.0",
            "pi": {
                "extensions": [
                    "extensions/*.js",
                    "!extensions/*.js",
                    "+extensions/keep.js",
                ],
                "skills": ["skills", "!skills/skip"],
                "prompts": ["prompts", "-prompts/drop.md"],
                "themes": ["themes", "!themes/drop.json"],
            },
        })
        .to_string(),
    );
    for relative in [
        "extensions/keep.js",
        "extensions/drop.js",
        "extensions/nested/hidden.js",
        "skills/keep/SKILL.md",
        "skills/skip/SKILL.md",
        "prompts/keep.md",
        "prompts/drop.md",
        "prompts/nested/nested.md",
        "themes/keep.json",
        "themes/drop.json",
    ] {
        write_file(&package_root, relative, "{}");
    }
    let settings = project_settings(&cwd, json!(["npm:manifest-pkg"]), Vec::new());

    let (resources, failures) = resolve(&settings, &cwd, false);

    assert!(failures.is_empty(), "{failures:?}");
    // `!extensions/*.js` excludes both plain glob matches; `+extensions/keep.js`
    // force-includes one back (and the nested file was never a plain match).
    assert_eq!(
        relative_paths(&resources.extensions, &package_root),
        vec!["extensions/keep.js"]
    );
    // `!skills/skip` drops one skill directory collected from the plain
    // `skills` directory.
    assert_eq!(
        relative_paths(&resources.skills, &package_root),
        vec!["skills/keep"]
    );
    // `-prompts/drop.md` force-excludes exactly that file; other prompts stay.
    assert_eq!(
        relative_paths(&resources.prompts, &package_root),
        vec!["prompts/keep.md", "prompts/nested/nested.md"]
    );
    // `!themes/drop.json` excludes the glob match.
    assert_eq!(
        relative_paths(&resources.themes, &package_root),
        vec!["themes/keep.json"]
    );
}

#[test]
fn manifest_only_overrides_contribute_nothing_without_a_plain_source() {
    // Upstream `addManifestEntries` derives the file set from the plain
    // entries; an overrides-only array has no files to enable.
    let cwd = unique_dir("manifest-overrides-only");
    let package_root = cwd
        .join(".pi")
        .join("npm")
        .join("node_modules")
        .join("overrides-pkg");
    write_file(
        &package_root,
        "package.json",
        &json!({
            "name": "overrides-pkg",
            "version": "1.0.0",
            "pi": { "extensions": ["!extensions/drop.js"] },
        })
        .to_string(),
    );
    write_file(&package_root, "extensions/drop.js", "export default {};\n");
    let settings = project_settings(&cwd, json!(["npm:overrides-pkg"]), Vec::new());

    let (resources, failures) = resolve(&settings, &cwd, false);

    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        resources.extensions.is_empty(),
        "an overrides-only manifest must not fall back to the convention dir: {:?}",
        resources.extensions
    );
}

/// R4-B P2-5: a partially invalid filter array. A non-string item used to make
/// the whole array unreadable (`collect::<Option<Vec<String>>>()`), which fell
/// back to "no filter" and loaded every resource the user meant to exclude.
/// The item must be diagnosed and the string patterns must still filter.
#[test]
fn non_string_filter_items_are_diagnosed_and_the_valid_patterns_still_filter() {
    let cwd = unique_dir("partial-filter");
    // A local package (never installed): its `pi` manifest names two skill
    // directories, and the settings entry narrows them to `allowed/*`.
    let package_root = cwd.join("local-skills-pkg");
    write_file(
        &package_root,
        "package.json",
        &json!({
            "name": "local-skills-pkg",
            "version": "1.0.0",
            "pi": { "skills": ["allowed", "blocked"] },
        })
        .to_string(),
    );
    write_file(&package_root, "allowed/keep-skill/SKILL.md", "# keep\n");
    write_file(&package_root, "blocked/drop-skill/SKILL.md", "# drop\n");
    let settings = project_settings(
        &cwd,
        json!([{
            "source": package_root.to_string_lossy(),
            "skills": ["allowed/*", 42],
        }]),
        Vec::new(),
    );

    let resolution = resolve_report(&settings, &cwd, false);

    assert!(
        resolution.failures.is_empty(),
        "a local package never installs: {:?}",
        resolution.failures
    );
    // The illegal item is reported exactly once, with its kind and position.
    assert_eq!(
        resolution.diagnostics.len(),
        1,
        "one diagnostic for the non-string item: {:?}",
        resolution.diagnostics
    );
    let diagnostic = &resolution.diagnostics[0];
    assert!(
        diagnostic.contains("\"skills\""),
        "the diagnostic names the resource kind: {diagnostic}"
    );
    assert!(
        diagnostic.contains("#2") && diagnostic.contains("42"),
        "the diagnostic names the offending item and its position: {diagnostic}"
    );
    // The valid pattern still applies: `blocked/drop-skill` stays excluded
    // instead of the whole array failing open.
    assert_eq!(
        relative_paths(&resolution.resources.skills, &package_root),
        vec!["allowed/keep-skill"],
        "a partially invalid array must not load every resource"
    );
}

/// The startup install runs inside hosts whose stdout is the protocol stream
/// (JSON/RPC): npm's output must be captured, never inherited.
#[test]
fn startup_auto_install_keeps_the_json_stdout_stream_clean() {
    let home = unique_dir("json-stdout");
    let agent = home.join("agent");
    let cwd = home.join("project");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let stage = stage_package(&home, "noisy-pkg", "1.0.0", "noisy_pkg_loaded");
    let install_root = agent.join("npm");
    let npm = fake_npm(&home, &install_root, "noisy-pkg", &stage, None, true);
    std::fs::write(
        agent.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "packages": ["npm:noisy-pkg"],
            "npmCommand": npm,
        }))
        .expect("settings json"),
    )
    .expect("settings");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args(["--mode", "json"])
        .current_dir(&cwd)
        .env("PIR_AGENT_DIR", &agent)
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("PI_OFFLINE");
    // Hermetic provider state: no credential may turn the empty run into a
    // model request.
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let output = command.output().expect("pir binary should start");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("NOISE-FROM-NPM"),
        "npm output leaked into the JSON protocol stream: {stdout}"
    );
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(!lines.is_empty(), "no JSON frames: {stdout}");
    for line in lines {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|error| panic!("non-JSON stdout line {line:?}: {error}"));
    }
    assert_eq!(
        installed_version(&install_root.join("node_modules").join("noisy-pkg")),
        "1.0.0",
        "the startup install must still have run"
    );
}

/// A PATH that cannot resolve `git` (the subprocess still starts: the
/// toolchain's runtime DLLs are copied along on Windows).
fn path_without_git(home: &Path) -> PathBuf {
    let dir = home.join("isolated-path");
    std::fs::create_dir_all(&dir).expect("isolated PATH dir");
    #[cfg(windows)]
    {
        let search = std::env::var_os("PATH").unwrap_or_default();
        for entry in std::env::split_paths(&search) {
            for dll in [
                "libgcc_s_seh-1.dll",
                "libstdc++-6.dll",
                "libwinpthread-1.dll",
            ] {
                let source = entry.join(dll);
                if source.is_file() && !dir.join(dll).exists() {
                    let _ = std::fs::copy(&source, dir.join(dll));
                }
            }
        }
    }
    dir
}

/// A missing git package in the PROJECT scope: the startup resolver prepares
/// `<cwd>/.pi/git` (upstream's `*\n!.gitignore\n` rule), records the failed
/// clone as a startup diagnostic, and still exits 0. PATH is stripped of git,
/// so no network clone can happen.
#[test]
fn missing_git_package_is_diagnosed_without_aborting_startup() {
    let home = unique_dir("git-missing");
    let agent = home.join("agent");
    let cwd = home.join("project");
    let project_dir = cwd.join(".pi");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    std::fs::write(
        project_dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "packages": ["git:example.com/org/repo"],
        }))
        .expect("settings json"),
    )
    .expect("settings");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args(["--mode", "json"])
        .current_dir(&cwd)
        .env("PIR_AGENT_DIR", &agent)
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_TRUST_PROJECT", "1")
        .env("PATH", path_without_git(&home))
        .env_remove("PI_OFFLINE");
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let output = command.output().expect("pir binary should start");

    assert!(
        output.status.success(),
        "an uninstallable package must not abort startup; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Extension package \"git:example.com/org/repo\": failed to install"),
        "expected the startup install diagnostic: {stderr}"
    );
    assert!(
        stderr.contains("Run \"pir remove git:example.com/org/repo --local\""),
        "the project-scope removal hint is included: {stderr}"
    );
    // The project scope prepares `<cwd>/.pi/git` with upstream's ignore rule
    // before attempting the clone; the clone itself never materializes.
    assert_eq!(
        std::fs::read_to_string(project_dir.join("git").join(".gitignore"))
            .expect("project git .gitignore"),
        "*\n!.gitignore\n"
    );
    assert!(
        !project_dir
            .join("git")
            .join("example.com")
            .join("org")
            .join("repo")
            .exists(),
        "the failed clone must not leave a package checkout"
    );
    // stdout stays a pure JSON stream.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|error| panic!("non-JSON stdout line {line:?}: {error}"));
    }
}

/// Spawn the real binary with an isolated agent dir, cwd/home, and a clean
/// auth environment (ambient provider keys are removed).
fn run_pir_in(cwd: &Path, agent: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args(args)
        .current_dir(cwd)
        .env("PIR_AGENT_DIR", agent)
        .env("PI_CODING_AGENT_DIR", agent)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("PI_OFFLINE")
        .env_remove("PIR_TRUST_PROJECT");
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    command.output().expect("pir binary should start")
}

/// Assert one logged npm invocation of `subcommand` carries `prefix` (the
/// extra `npmCommand` argv) BEFORE the subcommand, i.e. the configured
/// prefix is passed to the child like upstream
/// `runCommand(npmCommand[0], [...npmCommand.slice(1), ...args])`.
fn assert_prefixed_invocation(recorded: &str, subcommand: &str, prefix: &str) {
    let needle = format!(" {subcommand} ");
    let line = recorded
        .lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("no `{subcommand}` invocation in:\n{recorded}"));
    let prefix_at = line
        .find(prefix)
        .unwrap_or_else(|| panic!("the npmCommand prefix {prefix:?} was not passed:\n{recorded}"));
    let subcommand_at = line.find(&needle).expect("subcommand needle");
    assert!(
        prefix_at < subcommand_at,
        "the npmCommand prefix must precede `{subcommand}`:\n{line}"
    );
}

/// Every npm spawn uses the same `npmCommand` program + prefix argv assembly:
/// `install`, `remove` (uninstall) and `update` must all forward the extra
/// configured argv, not just `npm view` / git dependency installs.
#[test]
fn npm_command_prefix_argv_reaches_install_uninstall_and_update() {
    let home = unique_dir("npm-prefix");
    let agent = home.join("agent");
    let cwd = home.join("project");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let stage = stage_package(&home, "prefix-pkg", "1.0.0", "prefix_pkg_loaded");
    let install_root = agent.join("npm");
    let npm = fake_npm(&home, &install_root, "prefix-pkg", &stage, None, false);
    let prefix_tag = "--pir-prefix-tag";
    // A multi-entry prefix: on Windows the fake npm runs through `cmd /c`
    // (the same wrapper shape `install_git_dependencies` tests use), elsewhere
    // the script takes the extra argv directly.
    let npm_command = {
        #[cfg(windows)]
        {
            let mut parts = vec!["cmd".to_owned(), "/c".to_owned()];
            parts.extend(npm);
            parts.push(prefix_tag.to_owned());
            parts
        }
        #[cfg(not(windows))]
        {
            let mut parts = npm;
            parts.push(prefix_tag.to_owned());
            parts
        }
    };
    std::fs::write(
        agent.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "packages": [],
            "npmCommand": npm_command,
        }))
        .expect("settings json"),
    )
    .expect("settings");
    let log = home.join("npm-invocations.log");
    let package_root = install_root.join("node_modules").join("prefix-pkg");

    // `pir install` — the CLI install path (install_npm).
    let output = run_pir_in(&cwd, &agent, &home, &["install", "npm:prefix-pkg"]);
    assert!(
        output.status.success(),
        "install failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(installed_version(&package_root), "1.0.0");
    let recorded = std::fs::read_to_string(&log).expect("install invocation log");
    assert_prefixed_invocation(&recorded, "install", prefix_tag);
    std::fs::remove_file(&log).expect("reset invocation log");

    // `pir remove` — the uninstall path (uninstall_npm).
    let output = run_pir_in(&cwd, &agent, &home, &["remove", "npm:prefix-pkg"]);
    assert!(
        output.status.success(),
        "remove failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = std::fs::read_to_string(&log).expect("uninstall invocation log");
    assert_prefixed_invocation(&recorded, "uninstall", prefix_tag);
    std::fs::remove_file(&log).expect("reset invocation log");

    // `pir update` — update_configured_source reinstalls a missing managed
    // copy through the same prefixed argv.
    let output = run_pir_in(&cwd, &agent, &home, &["install", "npm:prefix-pkg"]);
    assert!(
        output.status.success(),
        "reinstall failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(&package_root).expect("drop installed copy");
    std::fs::remove_file(&log).expect("reset invocation log");
    let output = run_pir_in(&cwd, &agent, &home, &["update", "npm:prefix-pkg"]);
    assert!(
        output.status.success(),
        "update failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        installed_version(&package_root),
        "1.0.0",
        "update must reinstall the missing package"
    );
    let recorded = std::fs::read_to_string(&log).expect("update invocation log");
    assert_prefixed_invocation(&recorded, "install", prefix_tag);
    assert!(
        recorded.contains("prefix-pkg@latest"),
        "an unpinned update installs `<name>@latest`: {recorded}"
    );
}

/// The startup package scan belongs to mode hosts, not subcommands: `pir list`
/// must report the configured entries without first installing every missing
/// package (network + side effects before the command even runs).
/// A local source stored relative to the scope base (the project `.pi` dir)
/// must resolve against that base, not the process cwd: otherwise a
/// `packages: ["local-pkg"]` entry written by `pir install -l` points at
/// `<cwd>/local-pkg` instead of `<cwd>/.pi/local-pkg` and the package's
/// resources silently disappear.
#[test]
fn project_relative_local_source_resolves_against_the_pi_dir() {
    let cwd = unique_dir("local-relative");
    let package_root = cwd.join(".pi").join("local-pkg");
    write_file(
        &package_root,
        "package.json",
        &json!({
            "name": "local-pkg",
            "version": "1.0.0",
            "pi": { "extensions": ["./extensions/local.js"] },
        })
        .to_string(),
    );
    write_file(&package_root, "extensions/local.js", "export default {};\n");
    let settings = project_settings(&cwd, json!(["local-pkg"]), Vec::new());

    let (resources, failures) = resolve(&settings, &cwd, false);

    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        relative_paths(&resources.extensions, &package_root),
        vec!["extensions/local.js"],
        "a base-relative local source must be read from <cwd>/.pi/<entry>"
    );
}

#[test]
fn project_relative_local_install_round_trips_through_remove() {
    let home = unique_dir("local-relative-remove");
    let agent = home.join("agent");
    let cwd = home.join("project");
    let package_root = cwd.join(".pi").join("local-pkg");
    std::fs::create_dir_all(&agent).expect("agent dir");
    write_file(
        &package_root,
        "package.json",
        &json!({ "name": "local-pkg", "version": "1.0.0" }).to_string(),
    );

    let output = run_pir_in(
        &cwd,
        &agent,
        &home,
        &["--approve", "install", "-l", ".pi/local-pkg"],
    );
    assert!(
        output.status.success(),
        "install failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settings_path = cwd.join(".pi").join("settings.json");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("settings read"))
            .expect("settings json");
    assert_eq!(
        settings["packages"],
        json!(["local-pkg"]),
        "a project local source is stored relative to the .pi dir"
    );

    // The stored spelling (base-relative) and the bare CLI input (cwd-relative)
    // must still match for remove.
    let output = run_pir_in(
        &cwd,
        &agent,
        &home,
        &["--approve", "remove", "-l", "local-pkg"],
    );
    assert!(
        output.status.success(),
        "remove failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("settings read"))
            .expect("settings json");
    assert_eq!(settings["packages"], json!([]));
}

#[test]
fn package_subcommands_do_not_install_configured_packages() {
    let home = unique_dir("subcommand-scan");
    let agent = home.join("agent");
    let cwd = home.join("project");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let stage = stage_package(&home, "list-pkg", "1.0.0", "list_pkg_loaded");
    let install_root = agent.join("npm");
    let npm = fake_npm(&home, &install_root, "list-pkg", &stage, None, false);
    std::fs::write(
        agent.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "packages": ["npm:list-pkg"],
            "npmCommand": npm,
        }))
        .expect("settings json"),
    )
    .expect("settings");

    let output = run_pir_in(&cwd, &agent, &home, &["list"]);
    assert!(
        output.status.success(),
        "list failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home.join("npm-invocations.log").exists(),
        "`pir list` must not run the package manager"
    );
    assert!(
        !install_root.join("node_modules").join("list-pkg").exists(),
        "`pir list` must not install configured packages"
    );
}
