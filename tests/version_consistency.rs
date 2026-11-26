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

// ---------------------------------------------------------------------------
// Workspace-wide version and publishability consistency
// ---------------------------------------------------------------------------
//
// The support crates (`pir-plugin-api`, the builtin implementations, the
// catalog, the engine, the TUI) are published in dependency order, so every one
// of them must carry the SAME version and every `version = "x.y.z"` path
// dependency must match it. A crate that drifts is rejected by crates.io at
// publish time — after the tag is already pushed.

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The `[workspace] members` list, as strings.
fn workspace_members(manifest: &str) -> Vec<String> {
    let mut members = Vec::new();
    let mut in_members = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("members") && trimmed.contains('[') {
            in_members = !trimmed.contains(']');
            continue;
        }
        if in_members {
            if let Some(rest) = trimmed.strip_prefix(']') {
                let _ = rest;
                in_members = false;
                continue;
            }
            if let Some(value) = trimmed.strip_prefix('"') {
                if let Some(name) = value.split('"').next() {
                    members.push(name.to_owned());
                }
            }
        }
    }
    members
}

/// One dependency declared by a manifest, in whichever TOML shape it used.
///
/// Cargo accepts three spellings for the same dependency, and a scanner that
/// understands only the inline one misses real manifests (the sub-table shape
/// already exists in-tree at `crates/pir-native-shell/Cargo.toml`):
///
/// ```toml
/// pir-core = { path = "../pir-core", version = "0.3.2" }   # inline
/// pir-core = {                                             # multi-line inline
///     path = "../pir-core",
///     version = "0.3.2",
/// }
/// [dependencies.pir-core]                                  # sub-table
/// path = "../pir-core"
/// version = "0.3.2"
/// ```
#[derive(Debug, Clone)]
struct DeclaredDependency {
    /// The `[package] name` the dependency resolves to: the key, or its
    /// `package = "..."` rename.
    package: String,
    path: Option<String>,
    version: Option<String>,
    /// `[dev-dependencies]` / `[build-dependencies]` are not published, so a
    /// path dependency there may legitimately point at an unpublished crate.
    is_development: bool,
}

/// Which dependency table a `[section]` header names, if any.
///
/// Returns the `is_development` flag. `target.'cfg(..)'.dependencies` and the
/// `[dependencies.x]` sub-table form are both recognised.
fn dependency_section(header: &str) -> Option<bool> {
    let normalized = header.replace('"', "");
    let is_dev =
        normalized.contains("dev-dependencies") || normalized.contains("build-dependencies");
    let is_any = is_dev || normalized.contains("dependencies");
    is_any.then_some(is_dev)
}

/// Parse every declared dependency of `manifest`, for all three TOML shapes.
///
/// One pass, one state machine: the current section decides whether a
/// `key = value` line declares a dependency, belongs to a `[dependencies.x]`
/// sub-table, or is ignored.
fn declared_dependencies(manifest: &str) -> Vec<DeclaredDependency> {
    let mut out: Vec<DeclaredDependency> = Vec::new();
    let mut is_development = false;
    // Whether the current `[section]` is a dependency table at all.
    let mut in_dependencies = false;
    // `Some(index into out)` while inside a `[dependencies.<name>]` sub-table.
    let mut sub_table: Option<usize> = None;
    // `Some(index into out)` while inside a multi-line inline table
    // (`name = {` with no closing `}` on that line).
    let mut open_inline: Option<usize> = None;

    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }

        // A table header closes any open inline table and resets the section.
        if let Some(header) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            open_inline = None;
            sub_table = None;
            is_development = false;
            in_dependencies = false;
            let header = header.trim();
            let Some(is_dev) = dependency_section(header) else {
                continue;
            };
            in_dependencies = true;
            is_development = is_dev;
            // `[dependencies.<name>]`: seed the entry the field lines fill in.
            if header.matches('.').count() == 1 {
                let name = header
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .trim_matches('"');
                out.push(DeclaredDependency {
                    package: name.to_owned(),
                    path: None,
                    version: None,
                    is_development: is_dev,
                });
                sub_table = Some(out.len() - 1);
            }
            continue;
        }

        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let rest = rest.trim();

        // Inside a `[dependencies.x]` sub-table every line is a field.
        if let Some(index) = sub_table {
            let entry = &mut out[index];
            match key {
                "package" => entry.package = rest.trim_matches('"').to_owned(),
                "path" => entry.path = Some(rest.trim_matches('"').to_owned()),
                "version" => entry.version = Some(rest.trim_matches('"').to_owned()),
                _ => {}
            }
            continue;
        }

        // Inside a multi-line inline table: fields until the closing brace.
        if let Some(index) = open_inline {
            let entry = &mut out[index];
            match key {
                "package" => entry.package = rest.trim_matches('"').to_owned(),
                "path" => entry.path = Some(rest.trim_matches('"').to_owned()),
                "version" => entry.version = Some(rest.trim_matches('"').to_owned()),
                _ => {}
            }
            if rest.ends_with('}') || line.ends_with('}') {
                open_inline = None;
            }
            continue;
        }

        if !in_dependencies {
            continue;
        }

        // `key = value` in a dependency section.
        let mut entry = DeclaredDependency {
            package: key.trim_matches('"').to_owned(),
            path: None,
            version: None,
            is_development,
        };
        if let Some(inner) = rest.strip_prefix('{') {
            let closed = inner.contains('}');
            for field in inner.trim_end_matches('}').split(',') {
                let Some((field, value)) = field.split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches('"');
                match field.trim() {
                    "package" => entry.package = value.to_owned(),
                    "path" => entry.path = Some(value.to_owned()),
                    "version" => entry.version = Some(value.to_owned()),
                    _ => {}
                }
            }
            out.push(entry);
            if !closed {
                open_inline = Some(out.len() - 1);
            }
        } else {
            // `name = "1.2"`: a version-only dependency.
            entry.version = Some(rest.trim_matches('"').to_owned());
            out.push(entry);
        }
    }

    out
}
/// Every workspace member inherits `[workspace.package] version` instead of
/// restating a literal, so a release bump stays one edit in the root manifest.
///
/// NOTE: this test was missing its `#[test]` attribute, so the whole body ran
/// zero times and the inheritance rule was unguarded. The member-count guard is
/// deliberately a non-empty check rather than a literal minimum: a hardcoded
/// count turns every crate extraction into an unrelated assertion failure (it
/// had drifted to ">= 16" while the workspace held 18 members).
#[test]
fn every_workspace_member_inherits_the_workspace_version() {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");
    let members = workspace_members(&manifest);
    assert!(
        !members.is_empty(),
        "the root manifest lists no workspace members; the parser is broken, not the workspace"
    );
    let workspace_version = section_value(&manifest, "workspace.package", "version")
        .expect("[workspace.package] version");
    for member in &members {
        let path = root.join(member).join("Cargo.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let inherits = section_value(&text, "package", "version.workspace")
            .is_some_and(|value| value == "true");
        assert!(
            inherits,
            "{member}/Cargo.toml must use `version.workspace = true` so a release bump is one edit"
        );
        // A stray literal version next to the inherited one is a silent drift.
        assert!(
            section_value(&text, "package", "version").is_none(),
            "{member}/Cargo.toml sets a literal [package] version; remove it and inherit instead"
        );
        let _ = &workspace_version;
    }
}

/// The packages extracted out of the engine, and the ones allowed to depend on
/// the engine back.
///
/// The rule these enforce: an extracted crate must be usable on its own — its
/// dependency direction is `extracted -> third party (+ pir-plugin-api)`, never
/// `extracted -> pir-core`. A back edge would recreate the monolith: the
/// extraction only pays off if `pir-core` can be dropped from a consumer's
/// build (and if `cargo publish -p pir-core` stays ordered *after* the support
/// crates rather than forming a cycle).
///
/// Two crates legitimately point the other way and are excluded by name:
/// `pir-tui` is the terminal front end and `pir-native-shell` the experimental
/// desktop shell — both are *consumers* of the engine, not extracted halves of
/// it. `pir-sdk` (the facade) and `pir-core` itself are the direction owners.
const ENGINE_DEPENDENT_BY_DESIGN: [&str; 2] = ["pir-tui", "pir-native-shell"];

/// The engine package names an extracted crate must not depend on.
const ENGINE_PACKAGES: [&str; 4] = ["pir-core", "pir-sdk", "pir-builtins", "pir-tui"];

#[test]
fn extracted_crates_never_depend_on_the_engine() {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");

    let mut checked = 0usize;
    for (member, text) in manifests_to_check(&root, &manifest) {
        let name = section_value(&text, "package", "name")
            .unwrap_or_else(|| panic!("{member}/Cargo.toml has no [package] name"));
        // The engine, the facade and the by-design consumers are the ones that
        // MAY point at the engine; everyone else is an extracted crate.
        if matches!(name.as_str(), "pir-core" | "pir-sdk") {
            continue;
        }
        if ENGINE_DEPENDENT_BY_DESIGN.contains(&name.as_str()) {
            continue;
        }
        checked += 1;
        for dep in declared_dependencies(&text) {
            // A dev-dependency on the engine is acceptable for a crate whose
            // own integration tests drive an engine session; a *normal* one is
            // the back edge this gate exists to forbid.
            if dep.is_development {
                continue;
            }
            assert!(
                !ENGINE_PACKAGES.contains(&dep.package.as_str()),
                "`{name}` is an extracted crate but depends on `{}`; extracted crates must stay \
                 independent of the engine (allowed engine-dependent crates: \
                 {ENGINE_DEPENDENT_BY_DESIGN:?})",
                dep.package
            );
        }
    }
    assert!(
        checked > 0,
        "the extraction gate checked no crate; the member parser is broken"
    );
}

/// Every manifest a gate must read: the workspace members plus the ROOT
/// package.
///
/// The root `pir-sdk` is a real workspace member (cargo names it the sole
/// default member), but `[workspace] members` does not list it, so a gate that
/// walks only `workspace_members()` silently skips the facade's own
/// dependencies.
fn manifests_to_check(root: &std::path::Path, manifest: &str) -> Vec<(String, String)> {
    let mut paths: Vec<String> = workspace_members(manifest);
    paths.push(".".to_owned());
    paths
        .into_iter()
        .map(|member| {
            let path = root.join(&member).join("Cargo.toml");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            (member, text)
        })
        .collect()
}

#[test]
fn every_path_dependency_version_matches_the_workspace_version() {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");
    let workspace_version = section_value(&manifest, "workspace.package", "version")
        .expect("[workspace.package] version");
    for (member, text) in manifests_to_check(&root, &manifest) {
        for dep in declared_dependencies(&text) {
            // Only PUBLISHED dependencies need a version pin: a dev/build path
            // dep may point at an unpublished helper (and cargo never indexes
            // it for the published crate).
            if dep.is_development || dep.path.is_none() {
                continue;
            }
            let version = dep.version.as_deref().unwrap_or_else(|| {
                panic!(
                    "{member}: path dependency `{}` has no version; crates.io requires a \
                     versioned path dependency for a published crate",
                    dep.package
                )
            });
            assert_eq!(
                version, workspace_version,
                "{member}: `{}` path dependency pins version {version}, but the workspace version \
                 is {workspace_version}",
                dep.package
            );
        }
    }
}

#[test]
fn publishable_crates_never_depend_on_unpublished_ones() {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");

    // Package name -> is it published?
    let mut published: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
    let manifests = manifests_to_check(&root, &manifest);
    for (member, text) in &manifests {
        // `[package] name = "..."` (every manifest uses the literal form).
        let name = section_value(text, "package", "name")
            .unwrap_or_else(|| panic!("{member}/Cargo.toml has no [package] name"));
        let publish = section_value(text, "package", "publish").map(|value| value != "false");
        published.insert(name, publish.unwrap_or(true));
    }

    for (member, text) in &manifests {
        let name = section_value(text, "package", "name").expect("package name");
        if !published.get(&name).copied().unwrap_or(true) {
            continue;
        }
        for dep in declared_dependencies(text) {
            // Only published dependencies are indexed; a dev-dependency on an
            // unpublished helper is legitimate.
            if dep.is_development || dep.path.is_none() {
                continue;
            }
            if published.get(&dep.package) == Some(&false) {
                panic!(
                    "`{name}` is publishable but depends on `{}`, which is publish=false; \
                     `cargo publish -p {name}` would fail on the crates.io index",
                    dep.package
                );
            }
        }
    }
}
