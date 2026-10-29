//! Lane NN: `write` replacement semantics.
//!
//! Pins the two properties the write path promises across platforms:
//!
//! * an ordinary file is replaced through a same-directory temp file and a
//!   rename (new inode, permission bits carried over), while a target with
//!   more than one hard link is rewritten in place so every link observes the
//!   new bytes and the inode does not change;
//! * a successful write reports success only after the whole payload is in
//!   the target. `sync_all` itself is not observable from user space and there
//!   is deliberately no settings knob for it (see the `confine` module docs);
//!   this test pins the visible contract around that durable default.
//!
//! Filesystem-specific assertions are gated: Windows has no stable inode /
//! link-count accessor in `std`, so identity there is proven through the
//! second hard link seeing the same bytes.

use std::path::{Path, PathBuf};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::coding_agent::tools::register_core_tools;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-nn-{}-{}-{}",
        tag,
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

async fn write(registry: &ToolRegistry, root: &Path, path: &str, content: &str) -> ToolResult {
    let tool = registry.get("write").expect("write tool registered");
    let context = ToolContext::new(root.to_path_buf());
    tool.run(
        &serde_json::json!({ "path": path, "content": content }),
        &context,
    )
    .await
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read target")
}

fn assert_no_atomic_temp(directory: &Path) {
    for entry in std::fs::read_dir(directory).expect("list directory") {
        let name = entry.expect("directory entry").file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".pir-write-"),
            "an atomic-write temp file survived in {}: {name}",
            directory.display()
        );
    }
}

/// Create `link` as a hard link to `target`; `false` means the filesystem or
/// OS refused (the caller skips the case rather than failing).
fn try_hard_link(target: &Path, link: &Path) -> bool {
    match std::fs::hard_link(target, link) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "skipping hard-link case: cannot link {} -> {}: {error}",
                target.display(),
                link.display()
            );
            false
        }
    }
}

#[cfg(unix)]
fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).expect("target metadata").ino()
}

#[cfg(unix)]
fn unix_metadata(path: &Path) -> (u32, u64) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::metadata(path).expect("target metadata");
    (metadata.permissions().mode() & 0o7777, metadata.nlink())
}

// ---------------------------------------------------------------------------
// Hard links: in-place rewrite keeps the inode and updates every link
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hard_linked_write_updates_every_link_and_keeps_the_inode() {
    let root = temp_dir("hardlink");
    let first = root.join("first.txt");
    let second = root.join("second.txt");
    std::fs::write(&first, "before-hard-link\n").expect("seed target");
    if !try_hard_link(&first, &second) {
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o640)).expect("set mode");
    }
    #[cfg(unix)]
    let before_inode = inode(&first);

    let registry = registry();
    let result = write(&registry, &root, "first.txt", "after-hard-link\n").await;
    assert!(
        !result.is_error,
        "hard-linked write failed: {}",
        result.content
    );

    assert_eq!(
        read(&first),
        "after-hard-link\n",
        "the written name must have the new content"
    );
    assert_eq!(
        read(&second),
        "after-hard-link\n",
        "the other hard link must observe the same inode content"
    );

    #[cfg(unix)]
    {
        let (mode, links) = unix_metadata(&first);
        assert_eq!(
            inode(&first),
            before_inode,
            "an in-place rewrite must not replace the inode"
        );
        assert_eq!(links, 2, "the hard link must still share the inode");
        assert_eq!(mode, 0o640, "in-place writing preserves the mode");
    }
    assert_no_atomic_temp(&root);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Ordinary files: temp + rename, permissions copied, old inode replaced
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ordinary_write_replaces_the_inode_and_preserves_mode() {
    let root = temp_dir("atomic");
    let target = root.join("file.txt");
    std::fs::write(&target, "before\n").expect("seed target");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
            .expect("set mode");
    }
    #[cfg(unix)]
    let before_inode = inode(&target);

    let registry = registry();
    let result = write(&registry, &root, "file.txt", "after\n").await;
    assert!(
        !result.is_error,
        "ordinary write failed: {}",
        result.content
    );
    assert_eq!(read(&target), "after\n");

    #[cfg(unix)]
    {
        let (mode, links) = unix_metadata(&target);
        assert_ne!(
            inode(&target),
            before_inode,
            "temp+rename must install a new inode"
        );
        assert_eq!(links, 1, "an ordinary replacement has one link");
        assert_eq!(
            mode, 0o640,
            "the permission bits are copied to the temp file"
        );
    }
    assert_no_atomic_temp(&root);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Default durability: success is reported only after the whole payload landed
// ---------------------------------------------------------------------------

/// `sync_all` has no user-space observable, so this pins the contract that the
/// durable default exists to provide: when `write` reports success, the entire
/// payload is already in the target (including the tail) and no temp file is
/// left. A regression that returned success before the final flush/chunk would
/// show up as a short or tail-less read. The module docs record that every
/// write fsyncs and that no settings switch exists for it.
#[tokio::test]
async fn default_write_durability_puts_the_whole_payload_in_the_target() {
    let root = temp_dir("durability");
    let content = format!("{}{}", "x".repeat(1024 * 1024), "TAIL-MARKER");
    let registry = registry();

    let result = write(&registry, &root, "large.txt", &content).await;
    assert!(!result.is_error, "large write failed: {}", result.content);

    let bytes = std::fs::read(root.join("large.txt")).expect("read written file");
    assert_eq!(
        bytes.len(),
        content.len(),
        "the target must have every byte"
    );
    assert!(
        bytes.ends_with(b"TAIL-MARKER"),
        "the final chunk must be present when success is reported"
    );
    assert_no_atomic_temp(&root);
    let _ = std::fs::remove_dir_all(&root);
}
