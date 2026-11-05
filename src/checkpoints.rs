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

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default number of recent checkpoints retained per session.
pub const DEFAULT_MAX_CHECKPOINTS: usize = 20;

/// Default per-file ceiling for one checkpoint snapshot (64 MiB).
///
/// A snapshot that needs a larger file fails as a whole: no checkpoint is
/// published, so a mutation is never covered by a partial checkpoint that
/// silently omits a file. Callers treat snapshot failure as best-effort and
/// still perform the mutation (see `checkpoint_before_mutation`). The limit
/// matches the 64 MiB source-read ceiling of the mutating tools, but is
/// independent because a checkpoint must also account for the blob copy it
/// writes.
pub const DEFAULT_MAX_CHECKPOINT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Process-wide sequence for atomic-write temp names.
static ATOMIC_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-root snapshot locks, shared by every [`CheckpointStore`] value in the
/// process. Tools construct a fresh store per call, so a lock owned by one
/// store value would not serialize concurrent snapshots of the same session.
static SNAPSHOT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

fn snapshot_lock_for(root: &Path) -> Arc<Mutex<()>> {
    let mut locks = SNAPSHOT_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    lock
}

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
    max_file_bytes: u64,
    /// Serializes `snapshot`/`prune_old` for this root. The guard is held from
    /// the first blob write through meta publication and GC, so prune can
    /// never observe a blob whose meta is still being published.
    snapshot_lock: Arc<Mutex<()>>,
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
        let root = sidecar_root(session_file);
        Some(Self {
            session_file: session_file.to_path_buf(),
            snapshot_lock: snapshot_lock_for(&root),
            root,
            max_checkpoints: DEFAULT_MAX_CHECKPOINTS,
            max_file_bytes: DEFAULT_MAX_CHECKPOINT_FILE_BYTES,
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

    /// Override the per-file checkpoint size ceiling (tests and embedders; the
    /// production default is [`DEFAULT_MAX_CHECKPOINT_FILE_BYTES`]).
    pub fn with_max_checkpoint_file_bytes(mut self, max: u64) -> Self {
        self.max_file_bytes = max.max(1);
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
    ///
    /// A regular file larger than [`DEFAULT_MAX_CHECKPOINT_FILE_BYTES`] (or the
    /// configured override) makes the whole snapshot fail. Nothing is
    /// published: callers must not treat a failed snapshot as covering the
    /// mutation, and the mutation itself proceeds best-effort.
    pub fn snapshot(&self, tool: &str, paths: &[PathBuf]) -> Result<CheckpointMeta, String> {
        if paths.is_empty() {
            return Err("checkpoint requires at least one path".to_owned());
        }
        // Serialize with prune/GC: a blob written below must never be
        // considered an orphan before its meta is published.
        let _guard = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.snapshot_locked(tool, paths, None)
    }

    fn snapshot_locked(
        &self,
        tool: &str,
        paths: &[PathBuf],
        protect_id: Option<&str>,
    ) -> Result<CheckpointMeta, String> {
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
            let bytes = read_checkpoint_bytes(&path, metadata.len(), self.max_file_bytes)?;
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
        // `write_meta` -> `write_atomic` creates the checkpoint directory as
        // its parent; a second `create_dir_all(&dir)` here was redundant.
        write_meta(&dir, &meta)?;
        self.prune_old(protect_id);
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
                let name = path.file_name().and_then(|name| name.to_str())?;
                if !path.is_dir() || !name.starts_with("cp-") {
                    return None;
                }
                let meta = read_meta(&path)?;
                // The directory name is the trusted id; a forged `meta.id`
                // must never be joined to the store root by prune/restore.
                (meta.id == name).then_some(meta)
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
        // Hold the snapshot lock across the safety snapshot AND the blob reads:
        // a concurrent snapshot/prune must not evict the target checkpoint
        // while this restore is using it.
        let _guard = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let meta = self
            .list(0)
            .into_iter()
            .find(|meta| id == "latest" || meta.id == id)
            .ok_or_else(|| format!("checkpoint not found: {id}"))?;
        if !dry_run {
            // The restore itself is undoable: snapshot the current state of
            // the same paths under a new checkpoint before writing anything.
            // The target being restored is protected from the retention pass
            // even when it is the oldest checkpoint: otherwise the safety
            // snapshot would evict it and its blobs before they are read.
            let paths: Vec<PathBuf> = meta
                .files
                .iter()
                .map(|file| PathBuf::from(&file.path))
                .collect();
            let _ = self.snapshot_locked("restore-preimage", &paths, Some(&meta.id));
        }
        let mut report = RestoreReport {
            id: meta.id.clone(),
            dry_run,
            ..RestoreReport::default()
        };
        for file in &meta.files {
            let path = PathBuf::from(&file.path);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
            {
                report.errors.push(format!(
                    "refusing checkpoint path outside the recorded tree: {}",
                    path.display()
                ));
                continue;
            }
            if file.existed {
                let Some(hash) = file.hash.as_deref() else {
                    report
                        .errors
                        .push(format!("checkpoint has no hash for {}", path.display()));
                    continue;
                };
                if !is_blob_name(hash) {
                    report.errors.push(format!(
                        "refusing malformed checkpoint blob name for {}",
                        path.display()
                    ));
                    continue;
                }
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
                if sha256_hex(&prior) != hash {
                    report.errors.push(format!(
                        "checkpoint blob hash mismatch for {}",
                        path.display()
                    ));
                    continue;
                }
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

    /// Drop expired checkpoints, then garbage-collect blobs no longer
    /// referenced by any retained meta. `protect_id` is kept even when it has
    /// fallen outside retention (the checkpoint a restore is currently
    /// reading: its safety snapshot must not evict its blobs).
    ///
    /// The caller (`snapshot`) holds [`Self::snapshot_lock`], which is what
    /// makes the blob-write -> meta-publish window invisible to this pass: no
    /// other snapshot for this root can have written a blob without having
    /// published its meta before the lock is released. Blob files are
    /// content-addressed by SHA-256, so only files named like a hash are
    /// candidates; temp files from an interrupted atomic write are left for
    /// the writer's own failure cleanup.
    fn prune_old(&self, protect_id: Option<&str>) {
        // Fail closed on unreadable/corrupt metadata: `list()` drops entries
        // it cannot parse, and treating those as unreferenced would delete a
        // valid checkpoint's blobs. Retention/GC is best-effort and can wait
        // for a transient sharing violation or a manual repair.
        let metas = self.list(0);
        if metas.len() != checkpoint_dir_count(&self.root) {
            return;
        }
        for (index, meta) in metas.iter().enumerate() {
            if protect_id == Some(meta.id.as_str()) {
                continue;
            }
            if index >= self.max_checkpoints {
                let _ = std::fs::remove_dir_all(self.root.join(&meta.id));
            }
        }
        // Read the retained set after deletion: a directory that could not be
        // removed must keep its blobs referenced rather than losing them.
        let retained = self.list(0);
        if retained.len() != checkpoint_dir_count(&self.root) {
            return;
        }
        let mut referenced: HashSet<&str> = HashSet::new();
        for meta in &retained {
            for file in &meta.files {
                if let Some(hash) = file.hash.as_deref() {
                    referenced.insert(hash);
                }
            }
        }
        let Ok(entries) = std::fs::read_dir(self.root.join("blobs")) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_blob_name(&name) || referenced.contains(name.as_str()) {
                continue;
            }
            if entry.path().is_file() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Number of `cp-*` directories under the store root (readable or not).
fn checkpoint_dir_count(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("cp-"))
        })
        .count()
}

/// A content-addressed blob file name: exactly one SHA-256 digest in hex.
fn is_blob_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
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

/// Test seam for the `tests/` integration suites: publish `meta` into `dir`
/// through the same atomic writer `snapshot` uses.
#[doc(hidden)]
pub fn write_meta_for_tests(dir: &Path, meta: &CheckpointMeta) -> Result<(), String> {
    write_meta(dir, meta)
}

fn read_meta(dir: &Path) -> Option<CheckpointMeta> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read at most `limit` bytes, refusing (rather than silently truncating) a
/// file that is or grew past the checkpoint ceiling. The metadata check gives
/// the common oversized case a fast error; the bounded read also catches a
/// file that grew between the stat and the open/read.
fn read_checkpoint_bytes(path: &Path, size: u64, limit: u64) -> Result<Vec<u8>, String> {
    if size > limit {
        return Err(checkpoint_size_error(path, size, limit));
    }
    let file = std::fs::File::open(path)
        .map_err(|error| format!("checkpoint read {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("checkpoint read {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(checkpoint_size_error(path, bytes.len() as u64, limit));
    }
    Ok(bytes)
}

fn checkpoint_size_error(path: &Path, size: u64, limit: u64) -> String {
    format!(
        "checkpoint skipped: {} is {size} bytes, exceeds the {limit} byte per-file checkpoint limit",
        path.display()
    )
}

/// Replace `path` with `bytes` atomically.
///
/// The temp name includes the process id and a process-wide sequence so two
/// concurrent writers never choose the same name (and `create_new` enforces
/// it). `rename` replaces an existing destination atomically on Unix and on
/// Windows (`MOVEFILE_REPLACE_EXISTING`), so a concurrent reader observes
/// either the old or the new content, never a missing file. The destination
/// is never removed first. The parent directory is fsynced where the platform
/// exposes a directory handle; a temp file left by a failed write/rename is
/// removed best effort.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("checkpoint");
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}-{}",
        std::process::id(),
        ATOMIC_TEMP_SEQ.fetch_add(1, Ordering::Relaxed),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        // The handle is closed above: Windows cannot replace a file that is
        // still open, and on Unix closing before rename is equally harmless.
        std::fs::rename(&tmp, path)?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Persist the directory entry created by the rename (crash durability of the
/// name swap). Windows has no directory handle in `std`, so this is a no-op
/// there; file contents were already synced and the replacement is atomic.
#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
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
