//! Version metadata gate: the manifest's package version (a literal
//! `[package] version`, or the `[workspace.package]` value it inherits via
//! `version.workspace = true`) is the single source of every version pir
//! reports. The consumers under test all use `env!("CARGO_PKG_VERSION")`
//! (identity `User-Agent`, MCP client implementation, env daemon
//! `hello`/`--version`, crash log); this suite fails if one of them ever
//! drifts to a hardcoded value.

use std::path::Path;

/// Read the first `key = "value"` assignment in one manifest section. A full
/// TOML parser would be a new dependency for one scalar, so this walks the
/// lines, tracks the current `[section]` header, and strips the quoted value.
fn section_value(manifest: &str, section: &str, key: &str) -> Option<String> {
    let mut current = "";
    for line in manifest.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            current = header.trim_end_matches(']').trim();
            continue;
        }
        if current != section {
            continue;
        }
        // Require the exact key: `version.workspace` must not be read as
        // `version` (and vice versa).
        let rest = match line.strip_prefix(key) {
            Some(rest) => rest.trim_start(),
            None => continue,
        };
        let rest = match rest.strip_prefix('=') {
            Some(rest) => rest.trim(),
            None => continue,
        };
        // Cargo requires a quoted string for a version. Strip the closing quote
        // and any trailing comment; a bare token is kept for readability.
        let value = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap_or(quoted),
            None => rest.split_whitespace().next().unwrap_or(rest),
        };
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

/// Resolve the package version the way cargo does: a literal
/// `[package] version = "x.y.z"`, or `version.workspace = true` inherited from
/// `[workspace.package] version = "x.y.z"`.
fn manifest_package_version() -> String {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest_path.display()));
    let inherits_workspace = section_value(&manifest, "package", "version.workspace")
        .is_some_and(|value| value == "true");
    if inherits_workspace {
        return section_value(&manifest, "workspace.package", "version").unwrap_or_else(|| {
            panic!(
                "Cargo.toml sets `[package] version.workspace = true` but \
                 [workspace.package] has no version"
            )
        });
    }
    section_value(&manifest, "package", "version").unwrap_or_else(|| {
        panic!(
            "Cargo.toml has no [package] version and does not inherit one from [workspace.package]"
        )
    })
}

#[test]
fn package_version_matches_identity_and_mcp_versions() {
    let manifest_version = manifest_package_version();
    assert_eq!(
        manifest_version,
        env!("CARGO_PKG_VERSION"),
        "the manifest package version and CARGO_PKG_VERSION must agree"
    );
    assert_eq!(
        pir::ai::types::PI_AI_VERSION,
        manifest_version,
        "the User-Agent version must follow the package version"
    );
    #[cfg(feature = "mcp")]
    assert_eq!(
        pir::mcp::client::CLIENT_VERSION,
        manifest_version,
        "the MCP client implementation version must follow the package version"
    );
}

/// The env daemon is a separate process entry; ask it (the same `VERSION`
/// constant backs both `hello.version` and `--version`).
#[cfg(feature = "env-daemon")]
#[test]
fn env_daemon_reports_the_package_version() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args(["env-daemon", "--version"])
        .output()
        .expect("run pir env-daemon --version");
    assert!(
        output.status.success(),
        "env-daemon --version failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("version output is UTF-8");
    assert_eq!(
        stdout.trim(),
        format!("pi-env {}", manifest_package_version()),
        "env daemon version must follow the package version"
    );
}
