//! P6: the footer's git-branch watcher reads `.git/HEAD` directly instead of
//! spawning `git rev-parse` once per second. These tests pin the HEAD parser
//! shapes (attached ref, detached hex), the worktree/submodule `gitdir:`
//! indirection, and the one-shot `git` fallback rule, including the "HEAD
//! exists, so no subprocess is spawned" case.
#![cfg(feature = "tui")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::tui::terminal::{
    git_branch_poll, git_branch_poll_path, git_head_file, parse_git_head, read_git_head_branch,
    read_git_head_file, GIT_BRANCH_POLL_INTERVAL,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-git-head-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn parses_attached_and_detached_head_shapes() {
    assert_eq!(
        parse_git_head("ref: refs/heads/main\n").as_deref(),
        Some("main")
    );
    assert_eq!(
        parse_git_head("ref: refs/heads/feature/x").as_deref(),
        Some("feature/x")
    );

    // Detached HEAD: the file holds the commit id (SHA-1 or SHA-256), and the
    // footer shows the same "detached" text the `rev-parse` fallback produced.
    assert_eq!(parse_git_head(&"a".repeat(40)).as_deref(), Some("detached"));
    assert_eq!(
        parse_git_head(&"0123456789abcdef".repeat(4)).as_deref(),
        Some("detached")
    );

    // Shapes a direct read does not resolve keep the one-time git fallback.
    assert_eq!(parse_git_head(""), None);
    assert_eq!(parse_git_head("garbage\n"), None);
    assert_eq!(parse_git_head(&"a".repeat(39)), None);
    assert_eq!(parse_git_head("ref: refs/remotes/origin/main"), None);
    assert_eq!(parse_git_head("ref: refs/heads/"), None);
}

#[test]
fn reads_dot_git_head_directly() {
    let dir = TempDir::new("plain");
    std::fs::create_dir_all(dir.path().join(".git")).expect("create .git");
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

    assert_eq!(read_git_head_branch(dir.path()).as_deref(), Some("main"));

    // With a readable HEAD the poll never runs the subprocess.
    let fallback_calls = AtomicUsize::new(0);
    let mut fallback = || {
        fallback_calls.fetch_add(1, Ordering::SeqCst);
        Some("from-git".to_owned())
    };
    let mut fallback_cache: Option<Option<String>> = None;
    assert_eq!(
        git_branch_poll(dir.path(), &mut fallback_cache, &mut fallback).as_deref(),
        Some("main")
    );
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    assert!(fallback_cache.is_none());
}

#[test]
fn follows_gitdir_indirection_for_worktrees() {
    let dir = TempDir::new("worktree");
    let git_dir = dir.path().join("repo/.git/worktrees/wt");
    std::fs::create_dir_all(&git_dir).expect("create worktree gitdir");
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/wt-branch\n").expect("write HEAD");

    let checkout = dir.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("create checkout");
    std::fs::write(checkout.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n")
        .expect("write .git file");
    assert_eq!(
        read_git_head_branch(&checkout).as_deref(),
        Some("wt-branch")
    );

    // An absolute gitdir target works the same way.
    let absolute = dir.path().join("absolute");
    std::fs::create_dir_all(&absolute).expect("create checkout");
    std::fs::write(
        absolute.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .expect("write .git file");
    assert_eq!(
        read_git_head_branch(&absolute).as_deref(),
        Some("wt-branch")
    );
}

#[test]
fn walks_up_from_a_repository_subdirectory() {
    let dir = TempDir::new("nested");
    std::fs::create_dir_all(dir.path().join(".git")).expect("create .git");
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/root\n").expect("write HEAD");
    let nested = dir.path().join("src/deep");
    std::fs::create_dir_all(&nested).expect("create nested dirs");

    assert_eq!(read_git_head_branch(&nested).as_deref(), Some("root"));
}

#[test]
fn missing_head_uses_the_git_fallback_once() {
    let dir = TempDir::new("no-repo");
    let fallback_calls = AtomicUsize::new(0);
    let mut fallback = || {
        fallback_calls.fetch_add(1, Ordering::SeqCst);
        None
    };
    let mut fallback_cache: Option<Option<String>> = None;

    assert_eq!(
        git_branch_poll(dir.path(), &mut fallback_cache, &mut fallback),
        None
    );
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    assert!(fallback_cache.is_some());

    // The next polls in the same unreadable stretch do not spawn git again.
    assert_eq!(
        git_branch_poll(dir.path(), &mut fallback_cache, &mut fallback),
        None
    );
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);

    // A checkout that appears later is picked up by the direct read, which
    // re-arms the fallback without another subprocess.
    std::fs::create_dir_all(dir.path().join(".git")).expect("create .git");
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/late\n").expect("write HEAD");
    assert_eq!(
        git_branch_poll(dir.path(), &mut fallback_cache, &mut fallback).as_deref(),
        Some("late")
    );
    assert!(fallback_cache.is_none());
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_successful_git_fallback_stays_displayed() {
    let dir = TempDir::new("bare-like");
    let fallback_calls = AtomicUsize::new(0);
    let mut fallback = || {
        fallback_calls.fetch_add(1, Ordering::SeqCst);
        Some("bare-main".to_owned())
    };
    let mut fallback_cache: Option<Option<String>> = None;

    assert_eq!(
        git_branch_poll(dir.path(), &mut fallback_cache, &mut fallback).as_deref(),
        Some("bare-main")
    );
    // Later polls keep returning the cached fallback result: dropping it to
    // None after the one-shot spawn would clear the footer branch one tick
    // after the fallback produced it.
    assert_eq!(
        git_branch_poll(dir.path(), &mut fallback_cache, &mut fallback).as_deref(),
        Some("bare-main")
    );
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn git_branch_watcher_resolves_the_head_path_once_and_polls_slowly() {
    let dir = TempDir::new("resolved-once");
    std::fs::create_dir_all(dir.path().join(".git")).expect("create .git");
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

    // The watcher resolves the HEAD path once (ancestor walk + `.git`
    // indirection) and then polls only that file.
    let head = git_head_file(dir.path()).expect("the HEAD path resolves");
    assert_eq!(head, dir.path().join(".git/HEAD"));
    let mut fallback = || panic!("a resolved HEAD must never spawn git");
    let mut fallback_cache: Option<Option<String>> = None;
    assert_eq!(
        git_branch_poll_path(Some(&head), &mut fallback_cache, &mut fallback).as_deref(),
        Some("main")
    );

    // A branch switch is picked up by re-reading the resolved path (no new
    // discovery walk is involved).
    std::fs::write(&head, "ref: refs/heads/next\n").expect("rewrite HEAD");
    assert_eq!(read_git_head_file(&head).as_deref(), Some("next"));
    assert_eq!(
        git_branch_poll_path(Some(&head), &mut fallback_cache, &mut fallback).as_deref(),
        Some("next")
    );

    // A checkout that moves away falls back to the resolved-path decision
    // (the watcher re-resolves it on its next slow tick).
    std::fs::remove_dir_all(dir.path().join(".git")).expect("remove .git");
    let mut missing = || None;
    let mut missing_cache: Option<Option<String>> = None;
    assert_eq!(
        git_branch_poll_path(Some(&head), &mut missing_cache, &mut missing),
        None
    );

    // The idle cadence is a few seconds, not the former 1 s tick.
    assert!(
        GIT_BRANCH_POLL_INTERVAL >= std::time::Duration::from_secs(5)
            && GIT_BRANCH_POLL_INTERVAL <= std::time::Duration::from_secs(10),
        "the HEAD poll interval is outside the 5-10 s budget: {GIT_BRANCH_POLL_INTERVAL:?}"
    );
}
