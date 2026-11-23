//! Persistent QuickJS bytecode cache for on-disk extension modules.
//!
//! Every extension file on disk is transpiled by SWC and then parsed/compiled
//! by QuickJS before it can run. Measured on a release build, a 73 KiB
//! TypeScript file costs ~5 ms of SWC plus ~2.3 ms of QuickJS compile; a
//! 295 KiB file ~26 ms + ~12 ms. Both are pure functions of the file's bytes
//! (plus the transpiler and engine builds), so the compiled module bytecode is
//! cached and reloaded with `Module::load` on later starts: ~0.4 ms and
//! ~1.6 ms for the same two files. `pir` starts, worker runtimes and extension
//! reloads all share the cache, so a module is compiled once per source
//! revision per user rather than once per runtime.
//!
//! Contract:
//!
//! - The key is `sha256(CACHE_SCHEMA, module name, raw source bytes)`, so an
//!   edited file always misses and a key never maps to another module's blob.
//! - Files live under `<agent_dir>/cache/ext-bytecode/<key>.qc` (next to the
//!   session/config state, so `PIR_AGENT_DIR` keeps test and alternate
//!   installs isolated). Writes are atomic (temp + rename), so concurrent
//!   `pir` processes cannot observe a partial blob.
//! - A blob that QuickJS refuses to load is evicted and the caller falls back
//!   to `Module::declare`; the cache is always best-effort and never gate.
//! - The directory is pruned once per process to at most 1024 files / 64 MiB
//!   (oldest mtime first), far above any real extension set, so stale
//!   revisions cannot grow without bound.
//! - `PIR_EXT_BYTECODE_CACHE=0` (also `false`/`off`) disables reads and
//!   writes; deleting the directory is always safe.
//!
//! Bump [`CACHE_SCHEMA`] whenever the transpiler's output for unchanged input
//! changes (SWC upgrade, transpile options). QuickJS bytecode additionally
//! encodes its own BC version, so an engine upgrade fails `Module::load` and
//! falls back rather than misreading an old blob. Bytecode is trusted input
//! (QuickJS does not validate it against hostile data), and the cache
//! directory is user-owned like the extensions it accelerates.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

/// Bump when transpile output for the same input can change.
///
/// v2: blobs are written with debug info kept (`strip_debug: false`). The v1
/// files were compiled without function filenames, which makes QuickJS reject
/// dynamic `import()` from a cached module with "no function filename for
/// import()". Changing the schema changes every key, so poisoned v1 entries are
/// never read again.
const CACHE_SCHEMA: &str = "pir-ext-bytecode-3";

/// Environment switch: `0`, `false` or `off` disables the cache.
pub const ENV_CACHE: &str = "PIR_EXT_BYTECODE_CACHE";

/// Largest blob the cache will read back (a ~12 MiB source image).
const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;

/// Soft in-memory cap: distinct modules per process, not blob size.
const MAX_MEMORY_ENTRIES: usize = 512;
/// Aggregate Rust heap budget for cached QuickJS blobs. The count cap remains
/// as a second bound; large plugins cannot pin an unbounded process heap.
const MAX_MEMORY_BYTES: usize = 32 * 1024 * 1024;

const MAX_DISK_FILES: usize = 1024;
const MAX_DISK_BYTES: u64 = 64 * 1024 * 1024;
const TARGET_DISK_FILES: usize = 768;
const TARGET_DISK_BYTES: u64 = 48 * 1024 * 1024;

fn disabled() -> bool {
    matches!(
        std::env::var(ENV_CACHE).ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

/// Whether the persistent bytecode cache is enabled for this process. Callers
/// use this to avoid hashing and serializing modules when get/put are no-ops.
pub fn enabled() -> bool {
    !disabled()
}

fn cache_dir() -> PathBuf {
    crate::ai::providers::agent_dir()
        .join("cache")
        .join("ext-bytecode")
}

struct MemoryCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    bytes: usize,
}

impl MemoryCache {
    fn new() -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new(), bytes: 0 }
    }

    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        let value = self.entries.get(key).cloned();
        if value.is_some() {
            self.order.retain(|item| item != key);
            self.order.push_back(key.to_owned());
        }
        value
    }

    fn insert(&mut self, key: String, value: Arc<Vec<u8>>) {
        let size = value.len();
        if size > MAX_MEMORY_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.len());
            self.order.retain(|item| item != &key);
        }
        while self.entries.len() >= MAX_MEMORY_ENTRIES
            || self.bytes.saturating_add(size) > MAX_MEMORY_BYTES
        {
            let Some(old_key) = self.order.pop_front() else { break };
            if let Some(old) = self.entries.remove(&old_key) {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
        }
        self.bytes = self.bytes.saturating_add(size);
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }

    fn remove(&mut self, key: &str) {
        if let Some(old) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(old.len());
        }
        self.order.retain(|item| item != key);
    }
}

fn memory() -> &'static Mutex<MemoryCache> {
    static MEMORY: OnceLock<Mutex<MemoryCache>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(MemoryCache::new()))
}

/// `sha256(CACHE_SCHEMA, name, raw source)` as lowercase hex.
pub fn key(name: &str, source: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CACHE_SCHEMA.as_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(source);
    format!("{:x}", hasher.finalize())
}

/// Path for `key`, or `None` when it is not a plain 64-char hex digest (the
/// key reaches this module from the loader, but a path join must never be
/// built from unchecked text).
fn path_for(key: &str) -> Option<PathBuf> {
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(cache_dir().join(format!("{key}.qc")))
}

/// Cached module bytecode, memory first then disk. A hit is a complete blob —
/// validation is `Module::load`'s job (the caller evicts on failure).
pub fn get(key: &str) -> Option<Arc<Vec<u8>>> {
    if disabled() {
        return None;
    }
    if let Some(bytes) = memory().lock().ok().and_then(|mut cache| cache.get(key)) {
        return Some(bytes);
    }
    let path = path_for(key)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };
    if bytes.is_empty() || bytes.len() as u64 > MAX_BLOB_BYTES {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bytes = Arc::new(bytes);
    if let Ok(mut cache) = memory().lock() {
        cache.insert(key.to_owned(), bytes.clone());
    }
    Some(bytes)
}

/// Store one compiled module. Best-effort: a read-only agent directory (or a
/// concurrent writer) never fails a module load.
pub fn put(key: &str, payload: &[u8]) {
    if disabled() || payload.is_empty() || payload.len() as u64 > MAX_BLOB_BYTES {
        return;
    }
    if let Ok(mut cache) = memory().lock() {
        cache.insert(key.to_owned(), Arc::new(payload.to_vec()));
    }
    let Some(path) = path_for(key) else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Unique temp name: two processes may compile the same key concurrently;
    // the rename is atomic and both payloads are valid for the key.
    let tmp = dir.join(format!(
        ".{key}.{}.{}.tmp",
        std::process::id(),
        format!("{:?}", std::thread::current().id()).replace([' ', '(', ')'], "")
    ));
    if std::fs::write(&tmp, payload).is_err() || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    prune_once();
}

/// Drop one blob (memory + disk) after `Module::load` rejected it.
pub fn evict(key: &str) {
    if let Ok(mut cache) = memory().lock() {
        cache.remove(key);
    }
    if let Some(path) = path_for(key) {
        let _ = std::fs::remove_file(path);
    }
}

/// Enforce the directory caps once per process.
fn prune_once() {
    static PRUNED: AtomicBool = AtomicBool::new(false);
    if PRUNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return;
    };
    let mut files: Vec<(Option<std::time::SystemTime>, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let is_blob = entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "qc");
        if !is_blob {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        total += meta.len();
        files.push((meta.modified().ok(), meta.len(), entry.path()));
    }
    if files.len() <= MAX_DISK_FILES && total <= MAX_DISK_BYTES {
        return;
    }
    files.sort_by_key(|(modified, ..)| *modified);
    let mut count = files.len();
    for (_, len, path) in files {
        if count <= TARGET_DISK_FILES && total <= TARGET_DISK_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            count -= 1;
            total = total.saturating_sub(len);
        }
    }
}
