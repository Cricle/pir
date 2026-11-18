//! Optional git-worktree isolation for one subagent task.
//!
//! The lease is created from the repository's current HEAD, used as the
//! child's cwd, then committed and merged back on success. A failed merge
//! aborts the merge and leaves the worktree in place for inspection; the
//! caller records its path in `SingleResult.worktree`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

static WORKTREE_SEQ: AtomicU64 = AtomicU64::new(0);
/// Serializes repository metadata mutations. Parallel workers can finish at
/// the same time; git's index/worktree locks are not reentrant across
/// processes, so merge/cleanup and `worktree add` take this lock.
static GIT_METADATA_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct WorktreeLease {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
#[doc(hidden)]
pub struct WorktreeResult {
    pub merged: bool,
    pub cleaned: bool,
    pub error: Option<String>,
}

impl WorktreeLease {
    pub(super) fn outcome(&self, result: &WorktreeResult) -> super::results::WorktreeOutcome {
        super::results::WorktreeOutcome {
            repo: self.repo_root.display().to_string(),
            path: self.path.display().to_string(),
            branch: self.branch.clone(),
            base: self.base.clone(),
            merged: result.merged,
            cleaned: result.cleaned,
            error: result.error.clone(),
        }
    }
}

/// Create a clean worktree for one task. Returns a human-readable error when
/// the cwd is not a git repository or the working tree is dirty.
#[doc(hidden)]
pub async fn prepare(cwd: &Path, label: &str) -> Result<WorktreeLease, String> {
    let root = crate::coding_agent::context::agent_dir()
        .join("foreman")
        .join("worktrees");
    prepare_in(cwd, label, &root).await
}

#[doc(hidden)]
pub async fn prepare_in(cwd: &Path, label: &str, root: &Path) -> Result<WorktreeLease, String> {
    let repo_root = git_output(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let repo_root = PathBuf::from(repo_root.trim());
    if repo_root.as_os_str().is_empty() {
        return Err("Worktree isolation requires a git repository.".to_owned());
    }
    let _guard = GIT_METADATA_LOCK.lock().await;
    let status = git_output(
        &repo_root,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .await?;
    if !status.trim().is_empty() {
        return Err(
            "Worktree isolation requires a clean git working tree; commit or stash changes first."
                .to_owned(),
        );
    }
    let base = git_output(&repo_root, &["rev-parse", "HEAD"]).await?;
    let base = base.trim().to_owned();
    if base.is_empty() {
        return Err("Cannot resolve the repository HEAD for worktree isolation.".to_owned());
    }
    let seq = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe_label = sanitize_label(label);
    let branch = format!("foreman/{}-{}-{seq}", safe_label, std::process::id());
    std::fs::create_dir_all(root).map_err(|error| {
        format!(
            "Cannot create the foreman worktree root {}: {error}",
            root.display()
        )
    })?;
    let path = root.join(format!("{}-{seq}", std::process::id()));
    let path_text = path.display().to_string();
    git_status(
        &repo_root,
        &["worktree", "add", "-b", &branch, &path_text, &base],
    )
    .await
    .map_err(|error| format!("Cannot create worktree {branch}: {error}"))?;
    Ok(WorktreeLease {
        repo_root,
        path,
        branch,
        base,
        label: safe_label,
    })
}

impl WorktreeLease {
    /// Commit the worker's changes and merge the temporary branch back into
    /// the repository. On conflict the merge is aborted and the worktree is
    /// left in place.
    pub async fn merge(&self) -> WorktreeResult {
        let _guard = GIT_METADATA_LOCK.lock().await;
        self.merge_locked().await
    }

    async fn merge_locked(&self) -> WorktreeResult {
        let mut result = WorktreeResult::default();
        if let Err(error) = git_status(&self.path, &["add", "-A"]).await {
            result.error = Some(format!("Cannot stage worker changes: {error}"));
            return result;
        }
        let changed = match git_status(&self.path, &["diff", "--cached", "--quiet"]).await {
            Ok(()) => false,
            Err(_) => true,
        };
        if !changed {
            result.merged = true;
            result.cleaned = self.cleanup_locked().await.is_ok();
            return result;
        }
        let message = format!("foreman: {}", self.label);
        if let Err(error) = git_status(
            &self.path,
            &[
                "-c",
                "user.name=Pir Foreman",
                "-c",
                "user.email=foreman@localhost",
                "commit",
                "--no-gpg-sign",
                "-m",
                &message,
            ],
        )
        .await
        {
            result.error = Some(format!("Cannot commit worker changes: {error}"));
            return result;
        }
        if let Err(error) = git_status(
            &self.repo_root,
            &[
                "-c",
                "user.name=Pir Foreman",
                "-c",
                "user.email=foreman@localhost",
                "merge",
                "--no-ff",
                "--no-edit",
                &self.branch,
            ],
        )
        .await
        {
            let _ = git_status(&self.repo_root, &["merge", "--abort"]).await;
            result.error = Some(format!(
                "Worker changes conflict with the main workspace; worktree retained at {}: {error}",
                self.path.display()
            ));
            return result;
        }
        result.merged = true;
        result.cleaned = self.cleanup_locked().await.is_ok();
        result
    }

    /// Remove the worktree and its temporary branch. Safe to call twice.
    pub async fn cleanup(&self) -> Result<(), String> {
        let _guard = GIT_METADATA_LOCK.lock().await;
        self.cleanup_locked().await
    }

    async fn cleanup_locked(&self) -> Result<(), String> {
        let path_text = self.path.display().to_string();
        let _ = git_status(
            &self.repo_root,
            &["worktree", "remove", "--force", &path_text],
        )
        .await;
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = git_status(&self.repo_root, &["branch", "-D", &self.branch]).await;
        Ok(())
    }
}

fn sanitize_label(label: &str) -> String {
    let mut out = String::new();
    for ch in label.chars().take(24) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | '.') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        "worker".to_owned()
    } else {
        out
    }
}

async fn git_output(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_command(cwd, args).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn git_status(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = git_command(cwd, args).await?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        })
    }
}

async fn git_command(cwd: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("cannot run git: {error}"))
}
