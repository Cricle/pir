//! Real-git tests for the optional subagent worktree isolation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pir::plugins::subagent::prepare_in;

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pir-worktree-{label}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0),
        TEST_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

fn init_repo() -> PathBuf {
    let repo = temp_dir("repo");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join("base.txt"), "base\n").expect("write base");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

#[tokio::test]
async fn worktree_merges_a_worker_change_and_cleans_up() {
    let repo = init_repo();
    let root = temp_dir("root");
    let lease = prepare_in(&repo, "worker", &root).await.expect("prepare");
    std::fs::write(lease.path.join("worker.txt"), "WORKER_OK\n").expect("write worker");
    let result = lease.merge().await;
    assert!(result.merged, "{result:?}");
    assert!(result.cleaned, "{result:?}");
    assert!(std::fs::read_to_string(repo.join("worker.txt"))
        .expect("merged file")
        .contains("WORKER_OK"));
    assert!(!lease.path.exists(), "worktree should be removed");
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn worktree_refuses_a_dirty_repository() {
    let repo = init_repo();
    std::fs::write(repo.join("dirty.txt"), "dirty\n").expect("dirty file");
    let root = temp_dir("dirty-root");
    let error = prepare_in(&repo, "worker", &root)
        .await
        .expect_err("dirty repo must be refused");
    assert!(error.contains("clean git working tree"), "{error}");
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn worktree_conflict_keeps_the_lease_for_inspection() {
    let repo = init_repo();
    let root = temp_dir("conflict-root");
    let lease = prepare_in(&repo, "worker", &root).await.expect("prepare");
    std::fs::write(lease.path.join("base.txt"), "worker\n").expect("worker edit");
    std::fs::write(repo.join("base.txt"), "main\n").expect("main edit");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "main edit"]);
    let result = lease.merge().await;
    assert!(!result.merged, "{result:?}");
    assert!(result.error.is_some(), "{result:?}");
    assert!(lease.path.exists(), "conflicted worktree must be retained");
    lease.cleanup().await.expect("cleanup");
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}
