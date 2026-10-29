//! Resource conflict and dependency analysis for tool calls.
//!
//! The upstream agent loop intentionally keeps no global locks: tool calls
//! self-serialize per environment + canonical path through
//! `withFileMutationQueue`. This module provides the Rust equivalent: a
//! canonical resource identity and per-resource async lock table with bounded
//! memory, so mutations to the same file serialize while distinct files run
//! concurrently.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use derive_more::Display;

/// Canonical identity of a mutation target.
///
/// Keys are derived from canonical absolute paths so that `./a`, `a`,
/// `sub/../a`, and case variants resolve to the same key where supported.
/// `Display` forwards to the underlying key string (derive_more).
#[derive(Debug, Clone, Display, PartialEq, Eq, Hash)]
pub struct ResourceKey(String);

impl ResourceKey {
    /// The underlying key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build a canonical mutation key from a path, resolving against `cwd`.
    ///
    /// Existing paths are canonicalized via the filesystem. Missing paths fall
    /// back to logical normalization (absolute join, `.`/`..` collapse, and
    /// Windows case folding) so concurrent creates of a new file still share
    /// one key.
    pub fn canonical(path: &Path, cwd: &Path) -> std::io::Result<Self> {
        let absolute = resolve_absolute(path, cwd);
        let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        Ok(Self(normalize_key_string(canonical)))
    }

    /// Async wrapper over [`ResourceKey::canonical`] for async call sites.
    ///
    /// `std::fs::canonicalize` is a blocking filesystem syscall; awaiting it
    /// directly on a tokio worker stalls that worker for the duration of the
    /// filesystem round-trip. The preflight awaits this wrapper so the
    /// canonicalization runs on the blocking pool instead. The sync routine
    /// computes the key, so the resolved path and dependency grouping are
    /// exactly the same as [`ResourceKey::canonical`]'s.
    pub async fn canonical_async(path: &Path, cwd: &Path) -> std::io::Result<Self> {
        let path = path.to_path_buf();
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || Self::canonical(&path, &cwd))
            .await
            .map_err(|error| {
                std::io::Error::other(format!("canonicalize blocking task failed: {error}"))
            })?
    }

    /// Logical-only key: no filesystem access. Used where canonicalization is
    /// unavailable (tests, purely synthetic paths).
    pub fn logical(path: &Path, cwd: &Path) -> Self {
        Self(normalize_key_string(resolve_absolute(path, cwd)))
    }
}

/// Resolve `path` against `cwd` when relative.
fn resolve_absolute(path: &Path, cwd: &Path) -> PathBuf {
    // Windows drive-letter paths (`C:/…`) are absolute on their target
    // platform even when pir itself is built for Unix (WSL builds over
    // /mnt/c, where `Path::is_absolute` returns false for them).
    if path.is_absolute() || has_windows_drive_prefix(path) {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Detect a leading Windows drive prefix (`X:` followed by a separator or
/// end-of-string). `Component::Prefix` only parses on Windows targets, so a
/// textual check keeps Unix builds correct for Windows paths.
fn has_windows_drive_prefix(path: &Path) -> bool {
    path.to_str().is_some_and(|s| {
        let bytes = s.as_bytes();
        bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\')
    })
}

/// Collapse `.`/`..` components lexically and apply case folding on Windows.
fn normalize_key_string(path: PathBuf) -> String {
    let mut collapsed = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !collapsed.pop() {
                    collapsed.push(component.as_os_str());
                }
            }
            other => collapsed.push(other.as_os_str()),
        }
    }
    let mut key = collapsed.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    key
}

/// Per-resource async lock table with bounded memory.
///
/// Locks are lazily created per key, removed again when their last user
/// finishes (see [`IdleEntryEviction`]), and the table additionally evicts
/// idle entries at capacity as a backstop for cancelled/aborted calls.
#[derive(Debug)]
pub struct MutationQueues {
    inner: std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    max_entries: usize,
}

/// Removes this call's keys from the table once no other caller references
/// their lock. Declared before the acquired slots/guards in [`run_locked`] so
/// it drops after them (locals drop in reverse declaration order); a waiter
/// holds its own `Arc`, so `strong_count > 1` keeps the entry.
struct IdleEntryEviction<'a> {
    queues: &'a MutationQueues,
    keys: Vec<String>,
}

impl Drop for IdleEntryEviction<'_> {
    fn drop(&mut self) {
        let mut inner = self
            .queues
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in &self.keys {
            if inner
                .get(key)
                .is_some_and(|lock| Arc::strong_count(lock) == 1)
            {
                inner.remove(key);
            }
        }
    }
}

impl Default for MutationQueues {
    fn default() -> Self {
        Self::with_max_entries(4096)
    }
}

impl MutationQueues {
    /// Create a lock table holding at most `max_entries` idle keys.
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            max_entries: max_entries.max(1),
        }
    }

    /// Run `f` while holding the locks for every provided key. Keys are
    /// canonicalized to a sorted, de-duplicated set before acquisition so
    /// concurrent multi-path calls always lock in the same global order (a
    /// caller-order acquisition can deadlock ABBA: call A holds [a] waiting
    /// for [b] while call B holds [b] waiting for [a]). Distinct keys run
    /// concurrently; the same key serializes.
    pub async fn run_locked<T>(
        &self,
        keys: &[ResourceKey],
        f: impl std::future::Future<Output = T>,
    ) -> T {
        // Resolve every key to a shared lock first, keeping the `Arc`s alive
        // for the whole scope so the guards below never outlive their mutex.
        let mut sorted_keys: Vec<&str> = keys.iter().map(|key| key.as_str()).collect();
        sorted_keys.sort_unstable();
        sorted_keys.dedup();
        // Declared before `slots` so it drops after them; see the drop impl.
        let _eviction = IdleEntryEviction {
            queues: self,
            keys: sorted_keys.iter().map(|key| (*key).to_owned()).collect(),
        };
        let slots: Vec<Arc<AsyncMutex<()>>> = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if inner.len() >= self.max_entries {
                self.evict_idle(&mut inner);
            }
            sorted_keys
                .iter()
                .map(|key| {
                    inner
                        .entry((*key).to_owned())
                        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                        .clone()
                })
                .collect()
        };
        let mut guards = Vec::with_capacity(slots.len());
        for slot in &slots {
            guards.push(slot.lock().await);
        }
        f.await
    }

    /// Drop idle entries (no in-flight users) to bound table growth.
    fn evict_idle(&self, inner: &mut HashMap<String, Arc<AsyncMutex<()>>>) {
        inner.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
}
