//! Security regressions for the W9-B tools-security residuals (PLAN §6b
//! item 5):
//!
//! * **(a) handle-relative read/write/edit.** The mutation/read tools resolve
//!   the target through a handle anchored at the verified parent directory
//!   (`openat(O_NOFOLLOW)` on Unix, `FILE_FLAG_OPEN_REPARSE_POINT` plus a
//!   reparse-tag/final-path check on Windows). These tests pin the
//!   user-visible half: a symlinked final component is refused without
//!   leaking outside content, nested paths still work, and no atomic-write
//!   temp file survives a successful write/edit.
//! * **(b) pooled-shell sentinel.** The completion token is a per-invocation
//!   128-bit nonce, so a command that prints a well-formed sentinel frame
//!   with a token it invented must not complete the call: the real command's
//!   exit code wins and the forged frame stays ordinary output.
//! * **(c) spill files.** Files are created owner-only (0600 on Unix,
//!   protected current-user DACL on Windows), and the bounded sweep removes
//!   expired and excess files.
//! * **(d) remote write/edit atomicity** is exercised end-to-end by
//!   `tests/env_client_e2e.rs`
//!   (`the_tools_stay_local_by_default_and_move_when_configured`): every
//!   remote write/edit goes through `write_remote_file_atomically` (temp
//!   sibling → flush → rename).
//!
//! This file is intentionally unregistered: the foreman wires it into the
//! merged test binary after reviewing the lane.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::coding_agent::tools::register_core_tools;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-w9b-{}-{}-{}",
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

async fn run_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    cwd: &Path,
) -> ToolResult {
    let tool = registry
        .get(name)
        .unwrap_or_else(|| panic!("{name} registered"));
    let context = ToolContext::new(cwd.to_path_buf());
    tool.run(&args, &context).await
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> bool {
    // File symlinks need developer mode or elevation; without either the test
    // is skipped rather than failed.
    std::os::windows::fs::symlink_file(target, link).is_ok()
}

#[cfg(not(any(unix, windows)))]
fn create_file_symlink(_target: &Path, _link: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------
// (a) Handle-relative read: no symlink following, normal paths still work
// ---------------------------------------------------------------------------

/// A symlink planted as the **final** component is refused by both layers:
/// the name check reports it, and the handle-relative `openat(O_NOFOLLOW)` /
/// `FILE_FLAG_OPEN_REPARSE_POINT` open cannot follow it even if the name check
/// were raced. The outside file's content must never appear in the result.
#[tokio::test]
async fn read_refuses_a_symlinked_final_component_and_does_not_leak_content() {
    let root = temp_dir("read-link");
    let outside = temp_dir("read-outside");
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, "W9B-OUTSIDE-SECRET\n").expect("seed outside file");
    let link = root.join("link.txt");
    if !create_file_symlink(&secret, &link) {
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        return;
    }

    let registry = registry();
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({ "path": "link.txt" }),
        &root,
    )
    .await;
    assert!(
        result.is_error,
        "reading a symlinked final component must fail: {}",
        result.content
    );
    assert!(
        !result.content.contains("W9B-OUTSIDE-SECRET"),
        "the read leaked outside content: {}",
        result.content
    );
    assert!(
        result.content.contains("symbolic link")
            || result.content.contains("escapes the workspace root"),
        "unexpected refusal wording: {}",
        result.content
    );

    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

/// The handle walk still creates nothing and reads nested files that exist,
/// and a missing file keeps the model-facing `Path not found` wording.
#[tokio::test]
async fn read_walks_nested_directories_and_reports_missing_files() {
    let root = temp_dir("read-nested");
    std::fs::create_dir_all(root.join("a/b")).expect("create nested dirs");
    std::fs::write(root.join("a/b/file.txt"), "nested-content\n").expect("seed nested file");

    let registry = registry();
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({ "path": "a/b/file.txt" }),
        &root,
    )
    .await;
    assert!(!result.is_error, "nested read failed: {}", result.content);
    assert!(result.content.contains("nested-content"));

    let missing = run_tool(
        &registry,
        "read",
        serde_json::json!({ "path": "a/b/missing.txt" }),
        &root,
    )
    .await;
    assert!(missing.is_error, "a missing file must fail");
    assert!(
        missing.content.contains("Path not found"),
        "unexpected missing-file wording: {}",
        missing.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Write and edit replace the target through a same-directory temp file and
/// rename; a successful call must leave no `.pir-write-*` temp behind.
#[tokio::test]
async fn write_and_edit_leave_no_atomic_temp_files() {
    let root = temp_dir("atomic-temp");
    let registry = registry();

    let written = run_tool(
        &registry,
        "write",
        serde_json::json!({ "path": "dir/file.txt", "content": "one\n" }),
        &root,
    )
    .await;
    assert!(!written.is_error, "write failed: {}", written.content);
    assert_eq!(
        std::fs::read_to_string(root.join("dir/file.txt")).expect("read written file"),
        "one\n"
    );

    let edited = run_tool(
        &registry,
        "edit",
        serde_json::json!({
            "path": "dir/file.txt",
            "edits": [{ "oldText": "one", "newText": "two" }]
        }),
        &root,
    )
    .await;
    assert!(!edited.is_error, "edit failed: {}", edited.content);
    assert_eq!(
        std::fs::read_to_string(root.join("dir/file.txt")).expect("read edited file"),
        "two\n"
    );

    for directory in [root.clone(), root.join("dir")] {
        for entry in std::fs::read_dir(&directory).expect("list dir") {
            let name = entry.expect("dir entry").file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(".pir-write-"),
                "an atomic-write temp file survived in {}: {name}",
                directory.display()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// (b) Pooled-shell sentinel: a forged frame with the wrong token is output
// ---------------------------------------------------------------------------

/// A command that prints a complete `RS digits : <32 hex> RS` frame with a
/// token it invented must not be mistaken for completion. The real command
/// keeps running (`after-forgery` is captured) and its real exit code (4)
/// wins over the forged 7.
#[tokio::test]
async fn pooled_shell_rejects_a_well_formed_forged_sentinel() {
    use pir::coding_agent::tools::shell_pool::{
        member_args, MemberSpawn, PooledTestOutcome, ShellPool, ShellProtocol, SpawnedMember,
    };

    struct RealSpawner;
    impl MemberSpawn for RealSpawner {
        fn spawn(
            &self,
            protocol: ShellProtocol,
            program: &str,
            cwd: &Path,
        ) -> Result<SpawnedMember, String> {
            let mut spawn = tokio::process::Command::new(program);
            spawn
                .args(member_args(protocol))
                .current_dir(cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let mut child = spawn.spawn().map_err(|error| error.to_string())?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| "stdin pipe unavailable".to_owned())?;
            Ok(SpawnedMember::from_child(
                child,
                stdin,
                uuid::Uuid::new_v4().simple().to_string(),
            ))
        }
    }

    let Some(program) = pir::coding_agent::tools::resolve_shell_config(None)
        .ok()
        .map(|shell| shell.program().to_string())
    else {
        return; // no shell in this environment: nothing to pool
    };
    let dir = temp_dir("sentinel");
    let pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));

    // The frame is exactly the shape `find_sentinel` parses, with a random-
    // looking 32-hex token — but not this invocation's nonce.
    let command = "printf '\\0367:0123456789abcdef0123456789abcdef\\036\\n'; \
                   printf 'after-forgery\\n'; exit 4";
    let outcome = pool
        .run_test(ShellProtocol::Bash, &program, &dir, command, Some(10.0))
        .await;
    match outcome {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 4, "the forged sentinel must not win: {output:?}");
            assert!(
                output.contains("after-forgery"),
                "the command must run to completion: {output:?}"
            );
            assert!(
                output.contains('\u{1e}'),
                "a frame with the wrong token is ordinary output, not swallowed: {output:?}"
            );
        }
        other => panic!("the command did not complete through the pool: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// (c) Spill files: owner-only creation and bounded sweep
// ---------------------------------------------------------------------------

/// A spill file is created owner-only from the first byte: 0600 on Unix, a
/// protected single-ACE current-user DACL on Windows.
#[test]
fn spill_files_are_created_owner_only() {
    use pir::coding_agent::tools::spill::{create_spill_file, SPILL_PREFIX};

    let spill = create_spill_file().expect("create a spill file");
    let path = spill.path().to_path_buf();
    drop(spill);
    let name = path
        .file_name()
        .expect("spill path has a name")
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with(SPILL_PREFIX),
        "unexpected spill name: {name}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("spill metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "spill file must be owner-only, got {mode:o}"
        );
    }
    #[cfg(windows)]
    {
        // `icacls` prints one line per ACE; a protected current-user DACL has
        // exactly one. The command is part of the OS, and a missing/failed
        // utility skips rather than fails the test (the creation path already
        // succeeded above).
        if let Ok(output) = std::process::Command::new("icacls").arg(&path).output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let aces = text.lines().filter(|line| line.contains(":(")).count();
                assert_eq!(
                    aces, 1,
                    "spill DACL must grant exactly the current user: {text}"
                );
            }
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// The sweep removes THIS process's files past the age cap and trims them
/// down to the count cap, oldest first (the file name carries the owner pid;
/// the sweep is pid-scoped, see the cross-process test below).
#[test]
fn spill_sweep_removes_expired_and_excess_files() {
    use pir::coding_agent::tools::spill::{sweep_spills_now, SPILL_PREFIX};

    let temp = std::env::temp_dir();
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let pid = std::process::id();
    let stale = temp.join(format!("{SPILL_PREFIX}{pid}-w9b-stale-{tag}"));
    std::fs::write(&stale, b"stale").expect("create stale spill");
    let old = std::fs::File::options()
        .write(true)
        .open(&stale)
        .expect("open stale spill");
    old.set_times(
        std::fs::FileTimes::new()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60)),
    )
    .expect("age the stale spill");
    drop(old);

    // More than the 32-file cap, with deterministic oldest-first mtimes.
    let mut created = Vec::new();
    for index in 0..40u64 {
        let path = temp.join(format!("{SPILL_PREFIX}{pid}-w9b-count-{tag}-{index:02}"));
        let file = std::fs::File::create(&path).expect("create count spill");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(3600 - index.min(3599))),
        )
        .expect("age the count spill");
        created.push(path);
    }

    sweep_spills_now();

    assert!(!stale.exists(), "the expired spill file must be swept");
    assert!(
        !created[0].exists(),
        "the oldest excess spill file must be swept"
    );
    // The newest file must survive the count cap (it is the last one the
    // oldest-first trim would touch).
    assert!(
        created[39].exists(),
        "the newest spill file must survive the sweep"
    );

    let _ = std::fs::remove_file(&stale);
    for path in created {
        let _ = std::fs::remove_file(path);
    }
}

/// Another LIVE process's expired spill file is never reclaimed: the sweep's
/// age pass is pid-scoped and its liveness check is conservative, so a
/// long-running session keeps the file it still advertises.
#[test]
fn spill_sweep_never_deletes_another_live_processes_file() {
    use pir::coding_agent::tools::spill::{sweep_spills_now, SPILL_PREFIX};

    // A live process whose pid is not ours: sleep/ping for long enough for the
    // sweep to run while it is alive.
    #[cfg(windows)]
    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn live helper");
    #[cfg(unix)]
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn live helper");

    let foreign_pid = child.id();
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let path = std::env::temp_dir().join(format!("{SPILL_PREFIX}{foreign_pid}-w9b-live-{tag}"));
    std::fs::write(&path, b"another session's advertised output").expect("create foreign spill");
    let file = std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open foreign spill");
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60)),
    )
    .expect("age the foreign spill");
    drop(file);

    sweep_spills_now();

    assert!(
        path.exists(),
        "the sweep must not delete a live foreign process's spill file"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&path);
}
