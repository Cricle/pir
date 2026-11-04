//! Session retention / garbage collection (P0-4).
//!
//! The session store is append-only and currently grows without bound: only
//! the manual single-session delete and the crash-log cap ever remove files.
//! This module adds a conservative, testable retention pass:
//!
//! - `maxAgeDays`: delete sessions whose mtime is older than the bound;
//! - `maxSessions`: keep at most N sessions per directory (newest first);
//! - `maxBytes`: keep the newest sessions until the byte budget is reached.
//!
//! The ACTIVE session is always retained, even when every bound is exceeded.
//! Files are only removed after proving they are real pi session files (a
//! valid header), and known sidecars (checkpoint directories, temp files,
//! lock files) are removed together with their transcript. The policy is
//! opt-in (`enabled: false` by default); the explicit `pir sessions prune`
//! command can apply it without persisting the opt-in.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Retention policy for persisted sessions. `enabled` only gates automatic
/// startup pruning; `pir sessions prune` can run a one-shot pass regardless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionRetentionPolicy {
    pub enabled: bool,
    /// Delete sessions older than this many days.
    pub max_age_days: Option<u64>,
    /// Keep at most this many sessions per directory (the active session does
    /// not consume a deletable slot; it is always retained).
    pub max_sessions: Option<usize>,
    /// Keep the newest sessions until this many bytes are retained.
    pub max_bytes: Option<u64>,
}

impl Default for SessionRetentionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_age_days: Some(30),
            max_sessions: Some(500),
            max_bytes: Some(2 * 1024 * 1024 * 1024),
        }
    }
}

impl SessionRetentionPolicy {
    /// A pass with no bounds configured is a no-op.
    pub fn is_noop(&self) -> bool {
        self.max_age_days.is_none() && self.max_sessions.is_none() && self.max_bytes.is_none()
    }
}

/// One candidate session file (already filtered to regular `*.jsonl` files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFileInfo {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
}

/// Why a session is scheduled for deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PruneReason {
    Expired,
    OverMaxSessions,
    OverMaxBytes,
}

impl PruneReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PruneReason::Expired => "expired",
            PruneReason::OverMaxSessions => "over-max-sessions",
            PruneReason::OverMaxBytes => "over-max-bytes",
        }
    }
}

/// One planned deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedPrune {
    pub path: PathBuf,
    pub bytes: u64,
    pub reason: PruneReason,
}

/// Deterministic retention decision before any filesystem mutation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    pub remove: Vec<PlannedPrune>,
    pub kept: usize,
}

impl PrunePlan {
    pub fn bytes_to_free(&self) -> u64 {
        self.remove.iter().map(|entry| entry.bytes).sum()
    }
}

/// Result of an executed prune pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub removed: Vec<PlannedPrune>,
    pub kept: usize,
    pub bytes_freed: u64,
    /// Per-file failures (unreadable/non-session files are never deleted).
    pub errors: Vec<String>,
}

impl PruneReport {
    pub fn merge(&mut self, other: PruneReport) {
        self.removed.extend(other.removed);
        self.kept += other.kept;
        self.bytes_freed = self.bytes_freed.saturating_add(other.bytes_freed);
        self.errors.extend(other.errors);
    }
}

fn normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_active(path: &Path, active: Option<&Path>) -> bool {
    active.is_some_and(|active| normalized(path) == normalized(active))
}

/// Scan one directory for regular `*.jsonl` session candidates. Symlinks are
/// followed for metadata but non-files (FIFOs/devices/dirs) are rejected.
pub fn scan_session_files(dir: &Path) -> Vec<SessionFileInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<SessionFileInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                return None;
            }
            let metadata = path.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some(SessionFileInfo {
                path,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                bytes: metadata.len(),
            })
        })
        .collect();
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    files
}

/// Compute a retention plan. Pure: no filesystem mutation, so every boundary
/// (age, count, bytes, active protection) is directly testable.
pub fn plan_session_prune(
    mut files: Vec<SessionFileInfo>,
    policy: &SessionRetentionPolicy,
    now: SystemTime,
    active: Option<&Path>,
) -> PrunePlan {
    // Newest first; ties are path-stable so the plan is deterministic.
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });

    let active_present = files.iter().any(|file| is_active(&file.path, active));
    let mut remove: Vec<PlannedPrune> = Vec::new();
    let mut kept: Vec<&SessionFileInfo> = Vec::new();

    // 1. Age. The active session is exempt.
    for file in &files {
        if is_active(&file.path, active) {
            kept.push(file);
            continue;
        }
        let expired = policy.max_age_days.is_some_and(|days| {
            now.duration_since(file.modified).unwrap_or(Duration::ZERO)
                > Duration::from_secs(days.saturating_mul(86_400))
        });
        if expired {
            remove.push(PlannedPrune {
                path: file.path.clone(),
                bytes: file.bytes,
                reason: PruneReason::Expired,
            });
        } else {
            kept.push(file);
        }
    }

    // 2. Count. `kept` is newest-first; the active session reserves its slot
    // even when it sorts beyond `max_sessions`.
    if let Some(max_sessions) = policy.max_sessions {
        let mut count = usize::from(active_present);
        let mut survivors: Vec<&SessionFileInfo> = Vec::with_capacity(kept.len());
        for file in kept {
            if is_active(&file.path, active) {
                survivors.push(file);
                continue;
            }
            if count >= max_sessions {
                remove.push(PlannedPrune {
                    path: file.path.clone(),
                    bytes: file.bytes,
                    reason: PruneReason::OverMaxSessions,
                });
            } else {
                count += 1;
                survivors.push(file);
            }
        }
        kept = survivors;
    }

    // 3. Bytes. Newest-first accumulation; the active session is retained and
    // its bytes count against the budget.
    if let Some(max_bytes) = policy.max_bytes {
        let mut bytes = kept
            .iter()
            .filter(|file| is_active(&file.path, active))
            .map(|file| file.bytes)
            .sum::<u64>();
        let mut survivors: Vec<&SessionFileInfo> = Vec::with_capacity(kept.len());
        for file in kept {
            if is_active(&file.path, active) {
                survivors.push(file);
                continue;
            }
            if bytes.saturating_add(file.bytes) > max_bytes {
                remove.push(PlannedPrune {
                    path: file.path.clone(),
                    bytes: file.bytes,
                    reason: PruneReason::OverMaxBytes,
                });
            } else {
                bytes = bytes.saturating_add(file.bytes);
                survivors.push(file);
            }
        }
        kept = survivors;
    }

    PrunePlan {
        remove,
        kept: kept.len(),
    }
}

/// Known sidecars stored next to a session transcript. Keep this list in one
/// place so retention and the checkpoint store cannot drift.
pub fn session_sidecar_paths(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return Vec::new();
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    vec![
        parent.join(format!("{name}.checkpoints")),
        parent.join(format!("{name}.tmp")),
        parent.join(format!("{name}.bak")),
        parent.join(format!(".{name}.lock")),
    ]
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Execute a prune pass over one session directory.
pub fn prune_session_dir(
    dir: &Path,
    policy: &SessionRetentionPolicy,
    active: Option<&Path>,
) -> PruneReport {
    let files = scan_session_files(dir);
    let plan = plan_session_prune(files, policy, SystemTime::now(), active);
    execute_plan(plan, active)
}

fn execute_plan(plan: PrunePlan, active: Option<&Path>) -> PruneReport {
    let mut report = PruneReport {
        kept: plan.kept,
        ..PruneReport::default()
    };
    for entry in plan.remove {
        if is_active(&entry.path, active) {
            // Defense in depth: the planner already protects it.
            report.kept += 1;
            continue;
        }
        // Never delete a random `.jsonl`: prove the header parses.
        if super::read_session_header_for_discovery(&entry.path).is_none() {
            report.errors.push(format!(
                "refusing to prune non-session file: {}",
                entry.path.display()
            ));
            continue;
        }
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                report.bytes_freed = report.bytes_freed.saturating_add(entry.bytes);
                for sidecar in session_sidecar_paths(&entry.path) {
                    if sidecar.exists() {
                        if let Err(error) = remove_path(&sidecar) {
                            report.errors.push(format!(
                                "removed {} but could not remove sidecar {}: {error}",
                                entry.path.display(),
                                sidecar.display()
                            ));
                        }
                    }
                }
                report.removed.push(entry);
            }
            Err(error) => report.errors.push(format!(
                "failed to remove {}: {error}",
                entry.path.display()
            )),
        }
    }
    report
}

/// Recursively prune a sessions ROOT (`<agent-dir>/sessions`) or a single
/// per-cwd directory. Only directories containing `*.jsonl` files are
/// visited; symlinked directories are not followed.
pub fn prune_sessions_root(
    root: &Path,
    policy: &SessionRetentionPolicy,
    active: Option<&Path>,
) -> PruneReport {
    let mut report = PruneReport::default();
    prune_dir_recursive(root, policy, active, &mut report, 0);
    report
}

fn prune_dir_recursive(
    dir: &Path,
    policy: &SessionRetentionPolicy,
    active: Option<&Path>,
    report: &mut PruneReport,
    depth: usize,
) {
    if depth > 8 {
        report.errors.push(format!(
            "session tree deeper than 8 levels at {}; stopped",
            dir.display()
        ));
        return;
    }
    report.merge(prune_session_dir(dir, policy, active));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            prune_dir_recursive(&path, policy, active, report, depth + 1);
        }
    }
}

/// Human-readable one-line summary used by the CLI.
pub fn format_prune_report(report: &PruneReport) -> String {
    let mut out = format!(
        "pruned {} session(s), kept {}, freed {} bytes",
        report.removed.len(),
        report.kept,
        report.bytes_freed
    );
    for entry in &report.removed {
        out.push_str(&format!(
            "\n  removed {} ({}, {} bytes)",
            entry.path.display(),
            entry.reason.as_str(),
            entry.bytes
        ));
    }
    for error in &report.errors {
        out.push_str(&format!("\n  warning: {error}"));
    }
    out
}
