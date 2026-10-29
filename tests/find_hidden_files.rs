//! P1-3 regression pin: `find` must include hidden files and directories like
//! upstream fd `--hidden` (`pi/packages/coding-agent/src/core/tools/find.ts:182`).
//!
//! The `ignore` walker's `hidden(true)` means "IGNORE hidden files" ("Enables
//! ignoring hidden files. This is enabled by default." — ignore-0.4.33
//! `src/walk.rs`), so passing `true` was the opposite of fd's `--hidden`;
//! `grep` already used the correct `hidden(false)`. This pins `.github/**`
//! and dotfiles as findable, with the visible-file case as the control.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::coding_agent::register_core_tools;

fn temp_workspace() -> PathBuf {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "pir-find-hidden-{nonce}-{sequence}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp workspace");
    path
}

async fn find(
    registry: &ToolRegistry,
    root: &std::path::Path,
    pattern: &str,
) -> pir::agent::tool_batch::ToolResult {
    registry
        .get("find")
        .expect("find tool should be registered")
        .run(
            &serde_json::json!({ "pattern": pattern, "path": "." }),
            &ToolContext::new(root.to_path_buf()),
        )
        .await
}

#[tokio::test]
async fn find_includes_hidden_files_and_directories() {
    let root = temp_workspace();
    std::fs::create_dir_all(root.join(".github").join("workflows")).expect("create .github tree");
    std::fs::create_dir_all(root.join("nested")).expect("create nested");
    std::fs::write(
        root.join(".github").join("workflows").join("x.yml"),
        "name: ci\n",
    )
    .expect("write hidden workflow");
    std::fs::write(root.join("nested").join("y.yml"), "name: nested\n").expect("write nested");
    std::fs::write(root.join("visible.txt"), "hello\n").expect("write visible");
    std::fs::write(root.join(".secret.txt"), "hidden\n").expect("write dotfile");

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    // The hidden workflow under a hidden directory is a result (before the
    // fix the walker skipped `.github` entirely).
    let result = find(&registry, &root, "*.yml").await;
    assert!(!result.is_error, "{}", result.content);
    let lines: Vec<&str> = result.content.lines().collect();
    assert!(
        lines.contains(&".github/workflows/x.yml"),
        "hidden directory entries must match: {}",
        result.content
    );
    assert!(
        lines.contains(&"nested/y.yml"),
        "visible entries keep matching: {}",
        result.content
    );

    // A root-level dotfile is findable, and the visible control still is.
    let result = find(&registry, &root, ".secret.txt").await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.lines().any(|line| line == ".secret.txt"),
        "dotfiles must match: {}",
        result.content
    );
    let result = find(&registry, &root, "visible.txt").await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.lines().any(|line| line == "visible.txt"),
        "visible files keep matching: {}",
        result.content
    );

    let _ = std::fs::remove_dir_all(&root);
}
