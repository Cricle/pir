//! Repository hygiene gate for the pure-Rust migration.
//!
//! Only tracked files are checked so ignored local build/session artifacts do
//! not affect the result. The source tree must not acquire upstream runtime or
//! package files by accident.

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
            // `include_bytes!`; they are product source, not upstream Node
            // artifacts (see the matching .gitignore exception).
            if path.starts_with("src/plugins/builtin/phone/assets/") {
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

#[test]
fn ignore_rules_cover_runtime_and_package_artifacts() {
    let ignore = std::fs::read_to_string(".gitignore").expect(".gitignore should exist");
    for rule in ["node_modules/", "package.json", "*.ts", "*.tsx", "*.js"] {
        assert!(ignore.contains(rule), ".gitignore omitted {rule}");
    }
}
