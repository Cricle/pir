//! Repository hygiene gate for the pure-Rust migration.
//!
//! Only tracked files are checked so ignored local build/session artifacts do
//! not affect the result. The source tree must not acquire upstream runtime or
//! package files by accident.

use std::collections::HashSet;
use std::path::{Component, Path};
use std::process::Command;

#[test]
fn tracked_repository_contains_no_forbidden_upstream_artifacts() {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .output()
        .expect("git must be available for the repository hygiene gate");
    assert!(
        output.status.success(),
        "git ls-files failed: {:?}",
        output.stderr
    );

    let forbidden_extensions = [".ts", ".tsx", ".js"];
    let forbidden_names = [
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
    ];
    let files: Vec<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();

    let forbidden: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|path| {
            // The built-in phone UI ships hand-written JS assets embedded by
            // `include_bytes!`/`include_dir!`; they are product source, not
            // upstream Node artifacts. The tree moved from
            // `src/plugins/builtin/phone/assets/` to `clients/pi-phone-pwa/`
            // (built into `pir-phone-server` via `include_dir!`), so exempt
            // both roots; the matching `.gitignore` exception keeps them
            // tracked.
            if path.starts_with("src/plugins/builtin/phone/assets/")
                || path.starts_with("clients/pi-phone-pwa/")
            {
                return false;
            }
            forbidden_names.iter().any(|name| path.ends_with(name))
                || forbidden_extensions
                    .iter()
                    .any(|extension| path.ends_with(extension))
                || path.split('/').any(|part| part == "node_modules")
        })
        .collect();
    assert!(
        forbidden.is_empty(),
        "forbidden upstream artifacts are tracked: {forbidden:?}"
    );
}

/// Lexically normalize a relative path into slash-separated components
/// (`a/b/../c` -> `a/c`), so an `include_dir!` literal written relative to a
/// crate manifest can be compared with `git ls-files` output.
fn normalized_relative(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::Prefix(_) | Component::RootDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
        }
    }
    parts.join("/")
}

fn files_with_extension(root: &Path, extension: &str, out: &mut Vec<String>) {
    let entries = std::fs::read_dir(root)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", root.display()));
    for entry in entries {
        let entry = entry.expect("asset directory entry");
        let path = entry.path();
        if path.is_dir() {
            files_with_extension(&path, extension, out);
        } else if path
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case(extension))
        {
            out.push(normalized_relative(&path));
        }
    }
}

/// The phone UI is hand-written product source embedded with `include_dir!`,
/// not generated output. Untracking it (or moving it without updating the
/// embed) would ship a script-less client and nothing else would notice: this
/// guard resolves the macro target from `http.rs` and requires every JS file
/// on disk there to be tracked.
#[test]
fn embedded_phone_client_assets_exist_and_are_tracked() {
    let server_source = Path::new("crates/pi-phone-server/src/http.rs");
    let source = std::fs::read_to_string(server_source)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", server_source.display()));
    let marker = "include_dir!(\"";
    let start = source.find(marker).unwrap_or_else(|| {
        panic!(
            "{} must embed the client with include_dir!",
            server_source.display()
        )
    });
    let rest = &source[start + marker.len()..];
    let end = rest
        .find('"')
        .expect("include_dir! path literal must be terminated");
    // cargo expands `$CARGO_MANIFEST_DIR` to the invoking crate's directory.
    let literal = rest[..end].replace("$CARGO_MANIFEST_DIR", "crates/pi-phone-server");
    let assets_dir = Path::new(&literal);
    assert!(
        assets_dir.is_dir(),
        "embedded client directory {} does not exist",
        assets_dir.display()
    );
    assert!(
        assets_dir.join("index.html").is_file(),
        "embedded client directory {} has no index.html",
        assets_dir.display()
    );
    assert!(
        assets_dir.join("app.js").is_file(),
        "embedded client directory {} has no entry script app.js",
        assets_dir.display()
    );

    let embedded_root = normalized_relative(assets_dir);
    let mut on_disk = Vec::new();
    files_with_extension(assets_dir, "js", &mut on_disk);
    assert!(
        !on_disk.is_empty(),
        "embedded client directory {embedded_root} has no JavaScript sources"
    );

    let output = Command::new("git")
        .args(["ls-files", "-z", "--", &embedded_root])
        .output()
        .expect("git must be available for the repository hygiene gate");
    assert!(
        output.status.success(),
        "git ls-files failed: {:?}",
        output.stderr
    );
    let tracked: HashSet<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    let untracked: Vec<&String> = on_disk
        .iter()
        .filter(|path| !tracked.contains(*path))
        .collect();
    assert!(
        untracked.is_empty(),
        "embedded phone client scripts are not tracked: {untracked:?}"
    );
}

#[test]
fn ignore_rules_cover_runtime_and_package_artifacts() {
    let ignore = std::fs::read_to_string(".gitignore").expect(".gitignore should exist");
    for rule in ["node_modules/", "package.json", "*.ts", "*.tsx", "*.js"] {
        assert!(ignore.contains(rule), ".gitignore omitted {rule}");
    }
}
