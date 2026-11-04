//! File-level checkpoints / undo (P0-3).
//!
//! Every mutating `write`/`edit` call snapshots its target file(s) BEFORE the
//! mutation, under the session's sidecar directory:
//!
//! ```text
//! <session>.jsonl.checkpoints/
//!   cp-<uuid>/meta.json       tool, timestamp, per-file prior state
//!   cp-<uuid>/blobs/<sha256>  deduplicated content-addressed prior bytes
//! ```
//!
//! The newest [`DEFAULT_MAX_CHECKPOINTS`] checkpoints are retained. Restoring
//! a checkpoint writes the prior bytes back (or deletes a file that did not
//! exist before), and first records a safety checkpoint of the CURRENT state
//! so the restore itself is undoable. A session without a session file (an
//! in-memory/read-only transcript) cannot be checkpointed: callers skip
//! snapshots, and `pir checkpoint` refuses with an explicit error.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default number of recent checkpoints retained per session.
pub const DEFAULT_MAX_CHECKPOINTS: usize = 20;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Prior state of one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointFile {
    /// Absolute path as resolved by the tool.
    pub path: String,
    /// Whether the path existed before the mutation.
    pub existed: bool,
    /// SHA-256 of the prior bytes (hex), when `existed`.
    pub hash: Option<String>,
    pub bytes: u64,
}

/// One checkpoint record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointMeta {
    pub version: u32,
    pub id: String,
    pub created_at_ms: u64,
    /// Tool that triggered the snapshot (`write`, `edit`, `restore-preimage`).
    pub tool: String,
    pub files: Vec<CheckpointFile>,
}

/// Outcome of restoring one checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub id: String,
    pub restored: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub dry_run: bool,
    pub errors: Vec<String>,
}

/// Store rooted at one session's sidecar directory.
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    session_file: PathBuf,
    root: PathBuf,
    max_checkpoints: usize,
}

impl CheckpointStore {
    /// Open the store for a session file that already exists. Returns `None`
    /// when the session has no file (in-memory / read-only transcript), which
    /// callers treat as "checkpointing unavailable" instead of failing the
    /// mutation.
    pub fn for_session_file(session_file: &Path) -> Option<Self> {
        if !session_file.is_file() {
            return None;
        }
        Some(Self {
            session_file: session_file.to_path_buf(),
            root: sidecar_root(session_file),
            max_checkpoints: DEFAULT_MAX_CHECKPOINTS,
        })
    }

    /// Strict variant for restore CLI: a missing session file is an error.
    pub fn open(session_file: &Path) -> Result<Self, String> {
        Self::for_session_file(session_file).ok_or_else(|| {
            format!(
                "session file not found or read-only (checkpointing unavailable): {}",
                session_file.display()
            )
        })
    }

    pub fn with_max_checkpoints(mut self, max: usize) -> Self {
        self.max_checkpoints = max.max(1);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn session_file(&self) -> &Path {
        &self.session_file
    }

    /// Snapshot `paths` before a mutation. A path that does not exist yet is
    /// recorded as `existed: false`, so restoring the checkpoint deletes a
    /// file created later.
    pub fn snapshot(&self, tool: &str, paths: &[PathBuf]) -> Result<CheckpointMeta, String> {
        if paths.is_empty() {
            return Err("checkpoint requires at least one path".to_owned());
        }
        std::fs::create_dir_all(self.root.join("blobs"))
            .map_err(|error| format!("create checkpoint store: {error}"))?;
        let mut unique: Vec<PathBuf> = Vec::new();
        for path in paths {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(path)
            };
            if !unique.contains(&absolute) {
                unique.push(absolute);
            }
        }
        unique.sort();
        let mut files = Vec::with_capacity(unique.len());
        for path in unique {
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    files.push(CheckpointFile {
                        path: path.to_string_lossy().into_owned(),
                        existed: false,
                        hash: None,
                        bytes: 0,
                    });
                    continue;
                }
                Err(error) => {
                    return Err(format!("checkpoint stat {}: {error}", path.display()));
                }
            };
            if !metadata.is_file() {
                return Err(format!(
                    "checkpoint supports regular files only: {}",
                    path.display()
                ));
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let hash = sha256_hex(&bytes);
            let blob = self.root.join("blobs").join(&hash);
            if !blob.exists() {
                write_atomic(&blob, &bytes)
                    .map_err(|error| format!("write blob {}: {error}", blob.display()))?;
            }
            files.push(CheckpointFile {
                path: path.to_string_lossy().into_owned(),
                existed: true,
                hash: Some(hash),
                bytes: bytes.len() as u64,
            });
        }
        let id = format!("cp-{}", uuid::Uuid::new_v4());
        let meta = CheckpointMeta {
            version: 1,
            id: id.clone(),
            created_at_ms: now_ms(),
            tool: tool.to_owned(),
            files,
        };
        let dir = self.root.join(&id);
        std::fs::create_dir_all(&dir).map_err(|error| format!("create checkpoint: {error}"))?;
        write_meta(&dir, &meta)?;
        self.prune_old();
        Ok(meta)
    }

    /// Checkpoints, newest first, capped at `limit` (0 = no cap).
    pub fn list(&self, limit: usize) -> Vec<CheckpointMeta> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut metas: Vec<CheckpointMeta> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir()
                    || !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("cp-"))
                {
                    return None;
                }
                read_meta(&path)
            })
            .collect();
        metas.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        if limit > 0 && metas.len() > limit {
            metas.truncate(limit);
        }
        metas
    }

    /// Restore `id` (or `latest`). `dry_run` computes the same report without
    /// touching the filesystem and without the safety snapshot.
    pub fn restore(&self, id: &str, dry_run: bool) -> Result<RestoreReport, String> {
        let meta = self
            .list(0)
            .into_iter()
            .find(|meta| id == "latest" || meta.id == id)
            .ok_or_else(|| format!("checkpoint not found: {id}"))?;
        if !dry_run {
            // The restore itself is undoable: snapshot the current state of
            // the same paths under a new checkpoint before writing anything.
            let paths: Vec<PathBuf> = meta
                .files
                .iter()
                .map(|file| PathBuf::from(&file.path))
                .collect();
            let _ = self.snapshot("restore-preimage", &paths);
        }
        let mut report = RestoreReport {
            id: meta.id.clone(),
            dry_run,
            ..RestoreReport::default()
        };
        for file in &meta.files {
            let path = PathBuf::from(&file.path);
            if file.existed {
                let Some(hash) = file.hash.as_deref() else {
                    report
                        .errors
                        .push(format!("checkpoint has no hash for {}", path.display()));
                    continue;
                };
                let blob = self.root.join("blobs").join(hash);
                let prior = match std::fs::read(&blob) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        report
                            .errors
                            .push(format!("read blob {}: {error}", blob.display()));
                        continue;
                    }
                };
                let current = std::fs::read(&path).ok();
                if current.as_deref() == Some(prior.as_slice()) {
                    report.unchanged += 1;
                    continue;
                }
                if dry_run {
                    report.restored += 1;
                    continue;
                }
                if let Some(parent) = path.parent() {
                    if let Err(error) = std::fs::create_dir_all(parent) {
                        report
                            .errors
                            .push(format!("create {}: {error}", parent.display()));
                        continue;
                    }
                }
                match write_atomic(&path, &prior) {
                    Ok(()) => report.restored += 1,
                    Err(error) => report
                        .errors
                        .push(format!("restore {}: {error}", path.display())),
                }
            } else {
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                        if dry_run {
                            report.deleted += 1;
                        } else {
                            match std::fs::remove_file(&path) {
                                Ok(()) => report.deleted += 1,
                                Err(error) => report
                                    .errors
                                    .push(format!("delete {}: {error}", path.display())),
                            }
                        }
                    }
                    Ok(_) => report.errors.push(format!(
                        "refusing to delete non-file path {}",
                        path.display()
                    )),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        report.unchanged += 1
                    }
                    Err(error) => report
                        .errors
                        .push(format!("stat {}: {error}", path.display())),
                }
            }
        }
        Ok(report)
    }

    fn prune_old(&self) {
        let metas = self.list(0);
        for meta in metas.into_iter().skip(self.max_checkpoints) {
            let _ = std::fs::remove_dir_all(self.root.join(&meta.id));
        }
    }
}

/// `<session>.jsonl.checkpoints` (also the sidecar name retention removes).
pub fn sidecar_root(session_file: &Path) -> PathBuf {
    let name = session_file
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("session.jsonl");
    session_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.checkpoints"))
}

fn write_meta(dir: &Path, meta: &CheckpointMeta) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(meta).map_err(|error| error.to_string())?;
    write_atomic(&dir.join("meta.json"), &json)
        .map_err(|error| format!("publish checkpoint meta: {error}"))
}

fn read_meta(dir: &Path) -> Option<CheckpointMeta> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("checkpoint"),
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Test-only helper: `true` when the given path is under the store root.
#[doc(hidden)]
pub fn is_checkpoint_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("cp-"))
}

/// Age used by cleanup tools that prune old checkpoints independently of the
/// session retention pass.
pub const CHECKPOINT_DEFAULT_AGE: Duration = Duration::from_secs(30 * 86_400);
