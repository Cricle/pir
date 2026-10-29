//! Version metadata gate: `Cargo.toml`'s `[package] version` is the single
//! source of every version pir reports. The consumers under test all use
//! `env!("CARGO_PKG_VERSION")` (identity `User-Agent`, MCP client
//! implementation, env daemon `hello`/`--version`, crash log); this suite
//! fails if one of them ever drifts to a hardcoded value.

use std::path::Path;

/// Parse `[package] version` from the manifest (a full TOML parser would be a
/// new dependency for one scalar).
fn manifest_package_version() -> String {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest_path.display()));
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = line.strip_prefix("version =") {
                return rest.trim().trim_matches('"').to_owned();
            }
        }
    }
    panic!("Cargo.toml has no [package] version");
}

#[test]
fn package_version_matches_identity_and_mcp_versions() {
    let manifest_version = manifest_package_version();
    assert_eq!(
        manifest_version,
        env!("CARGO_PKG_VERSION"),
        "Cargo.toml [package] version and CARGO_PKG_VERSION must agree"
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
