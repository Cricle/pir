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
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
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

/// How long an unreferenced blob must have been untouched before the GC may
/// remove it.
///
/// `SNAPSHOT_LOCKS` only serializes snapshots *within one process*. Another
/// `pir` process may have just written a content-addressed blob and not yet
/// published the meta that references it; a prune running in this process
/// would otherwise mistake it for an orphan. The grace period makes that
/// cross-process write -> publish window invisible to GC without requiring an
/// OS file lock. Fresh blobs are kept, and a reused blob has its timestamp
/// refreshed at snapshot time, so a long-lived identical blob is protected
/// while it is being (re)published.
pub const DEFAULT_ORPHAN_BLOB_GRACE: Duration = Duration::from_secs(60 * 60);

/// Process-wide sequence for atomic-write temp names.
static ATOMIC_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-root snapshot locks, shared by every [`CheckpointStore`] value in the
/// process. Tools construct a fresh store per call, so a lock owned by one
/// store value would not serialize concurrent snapshots of the same session.
/// Entries are weak and dead ones are reaped whenever the table is touched, so
/// a process that opens many one-shot sessions does not leak one map entry per
/// root (A-08).
static SNAPSHOT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

fn snapshot_lock_for(root: &Path) -> Arc<Mutex<()>> {
    let mut locks = SNAPSHOT_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reap_dead_snapshot_locks(&mut locks);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn reap_dead_snapshot_locks(locks: &mut HashMap<PathBuf, Weak<Mutex<()>>>) {
    locks.retain(|_, lock| lock.strong_count() > 0);
}

/// Test seam: reap dead per-root lock entries and report how many live roots
/// remain (A-08 regression coverage).
#[doc(hidden)]
pub fn snapshot_lock_table_len_for_tests() -> usize {
    let mut locks = SNAPSHOT_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reap_dead_snapshot_locks(&mut locks);
    locks.len()
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
    /// Grace period before an unreferenced blob may be collected.
    orphan_blob_grace: Duration,
    /// Explicit restore containment root (`pir checkpoint restore --root
    /// <dir>`, or [`Self::with_workspace_root`] for library/embedding
    /// callers). Restore has no implicit root: a session header `cwd` is
    /// untrusted input and is never used for authorization.
    workspace_root: Option<PathBuf>,
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
            orphan_blob_grace: DEFAULT_ORPHAN_BLOB_GRACE,
            workspace_root: None,
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

    /// Override how long an unreferenced blob must stay untouched before GC
    /// may delete it. Tests use `Duration::ZERO` to assert immediate
    /// collection; production keeps [`DEFAULT_ORPHAN_BLOB_GRACE`].
    pub fn with_orphan_blob_grace(mut self, grace: Duration) -> Self {
        self.orphan_blob_grace = grace;
        self
    }

    /// Override the workspace root that confines `restore` targets. This is
    /// the trusted root behind `pir checkpoint restore --root <dir>`; restore
    /// cannot run without it (or another explicit [`Self::with_workspace_root`]
    /// call from a library/SDK embedder).
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
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
            } else {
                // A deduplicated blob may be arbitrarily old; refreshing its
                // timestamp keeps the cross-process GC grace period meaningful
                // for the meta we are about to publish.
                touch_blob(&blob);
            }
            files.push(CheckpointFile {
                path: path.to_string_lossy().into_owned(),
                existed: true,
                hash: Some(hash),
                bytes: bytes.len() as u64,
            });
        }
        self.publish_snapshot(tool, files, protect_id)
    }

    /// Snapshot one existing regular file from bytes the caller already read
    /// and validated. This is the `edit` fast path: the tool reads the target
    /// once, rejects invalid content, and only then snapshots the exact
    /// pre-mutation bytes it based the edit on. It avoids a second read that
    /// could observe a file changed in between, and a rejected edit never
    /// reaches this call.
    ///
    /// The same per-file ceiling as [`Self::snapshot`] applies: an oversized
    /// payload fails the snapshot as a whole and publishes nothing.
    pub fn snapshot_with_content(
        &self,
        tool: &str,
        path: &Path,
        content: &[u8],
    ) -> Result<CheckpointMeta, String> {
        if content.len() as u64 > self.max_file_bytes {
            return Err(checkpoint_size_error(
                path,
                content.len() as u64,
                self.max_file_bytes,
            ));
        }
        // Serialize with prune/GC like `snapshot`; the blob written below must
        // never be mistaken for an orphan before its meta is published.
        let _guard = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::fs::create_dir_all(self.root.join("blobs"))
            .map_err(|error| format!("create checkpoint store: {error}"))?;
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let hash = sha256_hex(content);
        let blob = self.root.join("blobs").join(&hash);
        if !blob.exists() {
            write_atomic(&blob, content)
                .map_err(|error| format!("write blob {}: {error}", blob.display()))?;
        } else {
            touch_blob(&blob);
        }
        let files = vec![CheckpointFile {
            path: absolute.to_string_lossy().into_owned(),
            existed: true,
            hash: Some(hash),
            bytes: content.len() as u64,
        }];
        self.publish_snapshot(tool, files, None)
    }

    /// Publish one checkpoint from already-materialized file entries and run
    /// retention. Callers must hold [`Self::snapshot_lock`].
    fn publish_snapshot(
        &self,
        tool: &str,
        files: Vec<CheckpointFile>,
        protect_id: Option<&str>,
    ) -> Result<CheckpointMeta, String> {
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
    ///
    /// An explicit root is mandatory: callers pass one through
    /// [`Self::with_workspace_root`] (`pir checkpoint restore --root <dir>`).
    /// Without one this returns `restore requires --root <dir>`; the session
    /// header `cwd` is never used as a fallback root.
    pub fn restore(&self, id: &str, dry_run: bool) -> Result<RestoreReport, String> {
        // Hold the snapshot lock across the safety snapshot AND the blob reads:
        // a concurrent snapshot/prune must not evict the target checkpoint
        // while this restore is using it.
        let _guard = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Resolve the containment root once, before any lookup or side
        // effect. Restore has no implicit root: the session header is
        // untrusted input, so a missing explicit `--root` is a hard error even
        // for transcripts inside the agent's own sessions directory (F2c).
        let canonical_root = self.workspace_root()?;
        let meta = self
            .list(0)
            .into_iter()
            .find(|meta| id == "latest" || meta.id == id)
            .ok_or_else(|| format!("checkpoint not found: {id}"))?;
        let mut report = RestoreReport {
            id: meta.id.clone(),
            dry_run,
            ..RestoreReport::default()
        };

        // Validate every recorded path BEFORE any side effect. A meta file is
        // untrusted input: it may come from another machine or a hand-edited
        // archive. Reject anything that is not an absolute, symlink-free
        // regular file inside the session's workspace (D-A-02).
        let mut targets: Vec<(&CheckpointFile, PathBuf)> = Vec::with_capacity(meta.files.len());
        for file in &meta.files {
            match self.resolve_restore_target(&file.path, &canonical_root) {
                Ok(path) => targets.push((file, path)),
                Err(error) => report.errors.push(error),
            }
        }

        if !dry_run && !targets.is_empty() {
            // The restore itself is undoable: snapshot the current state of
            // the same paths under a new checkpoint before writing anything.
            // A failure here must abort the restore: continuing would destroy
            // the current content with no way back (D-A-01).
            //
            // The target being restored is protected from the retention pass
            // even when it is the oldest checkpoint: otherwise the safety
            // snapshot would evict it and its blobs before they are read.
            let paths: Vec<PathBuf> = targets.iter().map(|(_, path)| path.clone()).collect();
            self.snapshot_locked("restore-preimage", &paths, Some(&meta.id))
                .map_err(|error| {
                    format!("refusing to restore without a safety checkpoint: {error}")
                })?;
        }
        for (file, path) in targets {
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
                    // Check containment before create_dir_all: if an ancestor
                    // was swapped for a symlink pointing outside the root, do
                    // not create any directories out there first.
                    if let Err(error) = precheck_parent_within_root(parent, &canonical_root) {
                        report.errors.push(error);
                        continue;
                    }
                    if let Err(error) = std::fs::create_dir_all(parent) {
                        report
                            .errors
                            .push(format!("create {}: {error}", parent.display()));
                        continue;
                    }
                    // Re-resolve right before the write: an ancestor swapped
                    // for a symlink after validation must not redirect it.
                    if let Err(error) = recheck_parent_within_root(parent, &canonical_root) {
                        report.errors.push(error);
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
                    Ok(metadata) if metadata.is_file() => {
                        if dry_run {
                            report.deleted += 1;
                        } else {
                            if let Some(parent) = path.parent() {
                                if let Err(error) =
                                    recheck_parent_within_root(parent, &canonical_root)
                                {
                                    report.errors.push(error);
                                    continue;
                                }
                            }
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

    /// Resolve one `meta.files[].path` to the path restore may touch.
    ///
    /// The recorded path is untrusted. It must be absolute, contain no `..`,
    /// resolve (with any not-yet-created tail) inside the session workspace,
    /// and — when it exists — be a regular file that is not a symlink. The
    /// returned path is the canonical form used for the actual read/write;
    /// the parent is re-resolved again immediately before the write/delete so
    /// swapping an ancestor for a symlink after validation cannot redirect it.
    fn resolve_restore_target(&self, raw: &str, canonical_root: &Path) -> Result<PathBuf, String> {
        let path = PathBuf::from(raw);
        let outside = || format!("refusing checkpoint path outside the workspace: {raw}");
        if !path.is_absolute()
            || path
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(outside());
        }
        // Check the final component before canonicalizing: canonicalize would
        // follow a symlink and hide that it ever was one.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "refusing checkpoint path that is a symbolic link: {raw}"
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "refusing checkpoint path that is not a regular file: {raw}"
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("cannot stat checkpoint path {raw}: {error}"));
            }
        }
        let resolved = canonicalize_allow_missing(&path)
            .map_err(|error| format!("cannot resolve checkpoint path {raw}: {error}"))?;
        if !resolved.starts_with(canonical_root) {
            return Err(outside());
        }
        Ok(resolved)
    }

    /// The workspace root restore targets must stay inside, canonicalized.
    ///
    /// Restore requires an explicit root (`pir checkpoint restore --root
    /// <dir>`, or [`Self::with_workspace_root`] for library/embedding
    /// callers). The session header `cwd` is untrusted input: it appears in
    /// the error below as a diagnostic hint only and never authorizes a
    /// containment root, regardless of where the transcript is stored. Every
    /// chosen root must be an existing directory and must not be a filesystem
    /// root.
    fn workspace_root(&self) -> Result<PathBuf, String> {
        if let Some(root) = &self.workspace_root {
            return validate_workspace_root(root);
        }
        let session_file =
            std::path::absolute(&self.session_file).unwrap_or_else(|_| self.session_file.clone());
        let header_hint = read_session_cwd(&session_file)
            .map(|cwd| {
                format!(
                    "; session header cwd {} is untrusted and was not used",
                    cwd.display()
                )
            })
            .unwrap_or_default();
        Err(format!(
            "restore requires --root <dir>: pass the workspace directory to restore into{header_hint}"
        ))
    }

    /// Drop expired checkpoints, then garbage-collect blobs no longer
    /// referenced by any retained meta. `protect_id` is kept even when it has
    /// fallen outside retention (the checkpoint a restore is currently
    /// reading: its safety snapshot must not evict its blobs).
    ///
    /// The caller (`snapshot`) holds [`Self::snapshot_lock`], which is what
    /// makes the blob-write -> meta-publish window invisible to this pass: no
    /// other snapshot for this root can have written a blob without having
    /// published its meta before the lock is released. That lock is only
    /// process-local, though: blobs younger than
    /// [`DEFAULT_ORPHAN_BLOB_GRACE`] are left alone so a concurrent process
    /// mid-publish cannot lose its content. Blob files are content-addressed
    /// by SHA-256, so only files named like a hash are candidates; temp files
    /// from an interrupted atomic write are left for the writer's own failure
    /// cleanup.
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
            let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
            if is_file && blob_is_expired(&entry, self.orphan_blob_grace) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// `true` when an unreferenced blob is old enough to be collected. A missing
/// or unreadable timestamp (or one in the future) keeps the blob: GC must
/// never guess in the direction that deletes data.
fn blob_is_expired(entry: &std::fs::DirEntry, grace: Duration) -> bool {
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age >= grace)
        .unwrap_or(false)
}

/// Best-effort refresh of a reused blob's mtime so the cross-process GC grace
/// period covers the meta publication that is about to reference it.
fn touch_blob(blob: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(blob) {
        let _ = file.set_modified(SystemTime::now());
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

/// Canonicalize `path`, tolerating a not-yet-created tail: the deepest
/// existing ancestor is canonicalized and the missing components are appended
/// verbatim. This is how restore checks containment for a file it is about to
/// create.
fn canonicalize_allow_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(mut resolved) => {
                for component in tail.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (cursor.file_name(), cursor.parent()) else {
                    return Err(error);
                };
                tail.push(name.to_os_string());
                cursor = parent.to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }
}

/// Validate the containment root shared by every target of a restore.
///
/// A root is never allowed to be a filesystem root (`/`, `C:\`, a UNC share
/// root): `starts_with` is vacuously true for every absolute path below it,
/// which is exactly the V2a escape. The root must exist and be a directory;
/// it is returned canonicalized so all containment checks use one form.
fn validate_workspace_root(root: &Path) -> Result<PathBuf, String> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::path::absolute(root)
            .map_err(|error| format!("cannot resolve workspace root {}: {error}", root.display()))?
    };
    if is_filesystem_root(&absolute) {
        return Err(format!(
            "refusing filesystem root as the workspace root: {}; pass --root <dir> below it",
            absolute.display()
        ));
    }
    let canonical = std::fs::canonicalize(&absolute).map_err(|error| {
        format!(
            "workspace root is missing or not accessible: {}: {error}",
            absolute.display()
        )
    })?;
    if is_filesystem_root(&canonical) {
        return Err(format!(
            "refusing filesystem root as the workspace root: {} (resolved from {}); pass --root <dir> below it",
            canonical.display(),
            absolute.display()
        ));
    }
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        format!(
            "cannot stat workspace root {}: {error}",
            canonical.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "workspace root is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// Whether `path` is a filesystem root (`/`, `C:\`, `\\server\share`), i.e.
/// has no component below its prefix/root.
fn is_filesystem_root(path: &Path) -> bool {
    let mut components = path.components();
    match components.next() {
        Some(Component::RootDir) => components.next().is_none(),
        Some(Component::Prefix(_)) => match components.next() {
            Some(Component::RootDir) => components.next().is_none(),
            None => true,
            Some(_) => false,
        },
        _ => false,
    }
}

/// Narrow the ancestor-symlink TOCTOU window: verify the deepest existing
/// ancestor of the target's parent is already inside the root *before*
/// `create_dir_all` is allowed to create anything.
fn precheck_parent_within_root(parent: &Path, canonical_root: &Path) -> Result<(), String> {
    let resolved = canonicalize_allow_missing(parent).map_err(|error| {
        format!(
            "cannot resolve restore parent {}: {error}",
            parent.display()
        )
    })?;
    if !resolved.starts_with(canonical_root) {
        return Err(format!(
            "refusing checkpoint path outside the workspace: {} (parent resolved outside the root)",
            parent.display()
        ));
    }
    Ok(())
}

/// Narrow the ancestor-symlink TOCTOU window: resolve the target's parent
/// again immediately before the actual write/delete and require it to still
/// be inside the root captured before validation.
fn recheck_parent_within_root(parent: &Path, canonical_root: &Path) -> Result<(), String> {
    let resolved = std::fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot re-resolve restore parent {}: {error}",
            parent.display()
        )
    })?;
    if !resolved.starts_with(canonical_root) {
        return Err(format!(
            "refusing checkpoint path outside the workspace: {} (parent moved after validation)",
            parent.display()
        ));
    }
    Ok(())
}

/// Path buffer for the first-line session header (`SessionHeader.cwd`).
#[derive(Deserialize)]
struct SessionHeaderCwd {
    #[serde(default)]
    cwd: Option<String>,
}

/// Read the `cwd` from the session header (line 1), bounded so a malformed or
/// huge first line cannot make restore allocate unbounded memory. A missing or
/// non-JSON header yields `None`; callers fall back to the session directory.
fn read_session_cwd(session_file: &Path) -> Option<PathBuf> {
    const SESSION_HEADER_MAX_BYTES: u64 = 1024 * 1024;
    let file = std::fs::File::open(session_file).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file)
        .take(SESSION_HEADER_MAX_BYTES)
        .read_line(&mut line)
        .ok()?;
    let header: SessionHeaderCwd = serde_json::from_str(line.trim_end()).ok()?;
    let cwd = header.cwd?;
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
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
