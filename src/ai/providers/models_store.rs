//! `models-store.json` support (upstream `models-store.ts` + the pi-ai
//! `ModelsStoreEntry` shape): the locked JSON file caching dynamically
//! refreshed remote provider catalogs, keyed by provider id.
//!
//! Reads and writes take the same `<file>.lock` (plus an in-process mutex), so
//! readers never observe a half-written store. Writes replace the file
//! atomically through a same-directory temp file + `rename`; a crash mid-write
//! leaves the previous catalog intact. A pre-existing file that fails to parse
//! is preserved (reads report `None`, writes/deletes fail with a diagnostic)
//! instead of being overwritten with an empty document.
//!
//! Reads are served through a per-path parse cache validated by the file's
//! metadata stamp plus a content hash of the bytes: the per-prompt provider
//! refresh reads one entry, so the whole store is parsed once per file version
//! instead of once per prompt, and a same-length in-place rewrite inside one
//! timestamp tick cannot be served from a stale parse.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One cached provider catalog (upstream `ModelsStoreEntry`): the persisted
/// models of every type plus the remote-check metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelsStoreEntry {
    /// Persisted models in the upstream `Model` wire shape.
    pub models: Vec<Value>,
    /// Remote catalog `Last-Modified` in Unix **milliseconds** (upstream
    /// `Date.parse` output, comparable to the manifest's `generatedAt`).
    /// `None` for entries written before the generation timestamp existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    /// Unix timestamp (milliseconds) of the last completed remote check,
    /// matching upstream's `Date.now()`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    /// Opaque validator from the remote catalog's `ETag` header, stored
    /// verbatim (quotes included) and echoed back as `If-None-Match`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl ModelsStoreEntry {
    /// Decode an upstream `ModelsStoreEntry` wire object (the shape a
    /// `refreshModels` publication carries in `persist`).
    ///
    /// Missing optional metadata is `None`; `models` defaults to an empty
    /// list when the publication omitted it (a provider may publish metadata
    /// only). Returns `None` when the value is not an object at all.
    pub fn from_wire(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let models = match object.get("models") {
            None => Vec::new(),
            Some(Value::Array(models)) => models.clone(),
            // Malformed publications are refused rather than decoded as an
            // empty catalog (which would erase a healthy provider entry).
            Some(_) => return None,
        };
        Some(Self {
            models,
            last_modified: object.get("lastModified").and_then(Value::as_u64),
            checked_at: object.get("checkedAt").and_then(Value::as_u64),
            etag: object
                .get("etag")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Encode the entry in the upstream camelCase wire shape (the
    /// `RefreshModelsContext.stored` object and the `models-store.json`
    /// value).
    pub fn to_wire(&self) -> Value {
        self.clone().into_wire()
    }

    /// Consuming form of [`Self::to_wire`]: the (possibly large) model list is
    /// MOVED into the wire object instead of deep-copied. The per-prompt
    /// refresh path owns the entry it reads, so this halves the JSON copies
    /// between the store and the JS payload.
    pub fn into_wire(self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("models".to_owned(), Value::Array(self.models));
        if let Some(last_modified) = self.last_modified {
            object.insert("lastModified".to_owned(), Value::from(last_modified));
        }
        if let Some(checked_at) = self.checked_at {
            object.insert("checkedAt".to_owned(), Value::from(checked_at));
        }
        if let Some(etag) = self.etag {
            object.insert("etag".to_owned(), Value::String(etag));
        }
        Value::Object(object)
    }
}

/// The `models-store.json` file shape: `{ "<providerId>": entry }`.
type ModelsStoreFile = BTreeMap<String, ModelsStoreEntry>;

/// The filesystem metadata identity that makes a cached parse a candidate: one
/// metadata probe answers "might the cached copy still describe the file?".
/// Content identity is verified separately by [`content_hash`], because a
/// same-length in-place rewrite can keep every field below unchanged inside
/// one filesystem timestamp tick. A missing file is its own stamp (a missing
/// store is an empty store).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreStamp {
    Missing,
    Present {
        len: u64,
        /// Last-write time. Together with `created` this catches both in-place
        /// rewrites and the temp-file + rename replacements this module (and
        /// any other writer) performs.
        modified: Option<SystemTime>,
        /// Birth time. A replacing rename installs a new file, so this moves
        /// even when a same-length rewrite lands inside one timestamp tick.
        created: Option<SystemTime>,
    },
}

/// Probe the file's current identity. Metadata failures (permissions, a
/// transiently missing path) are reported as `Missing`; the subsequent read
/// produces the real diagnostic.
fn store_stamp(path: &Path) -> StoreStamp {
    match std::fs::metadata(path) {
        Ok(metadata) => StoreStamp::Present {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        },
        Err(_) => StoreStamp::Missing,
    }
}

/// One cached parse and the identity of the bytes it was produced from.
struct CachedStore {
    stamp: StoreStamp,
    /// FNV-1a of the file bytes this parse was produced from; `None` for a
    /// missing file. The metadata stamp alone cannot distinguish an in-place
    /// rewrite that keeps the length and lands inside one filesystem timestamp
    /// tick, so a stamp hit is only a candidate: the bytes are hashed again
    /// before the cached parse is served.
    content_hash: Option<u64>,
    store: Arc<ModelsStoreFile>,
    /// Cache hits served from this parse (diagnostic/test seam).
    hits: u64,
    /// Parses performed for this path (diagnostic/test seam).
    parses: u64,
}

/// Per-path parse cache behind [`read_models_store_entry`].
///
/// `sync_plugin_providers` reads one provider's catalog on every prompt it
/// runs (upstream refreshes only when the directory is stale). Re-reading and
/// re-parsing the WHOLE store for one entry each time is pure overhead, so a
/// successful parse is kept until the file's metadata stamp changes OR the
/// stamp still matches but the bytes hash differently (the same-length
/// in-place rewrite case). Our own writes refresh the cache in place; a write
/// by another process changes the stamp and invalidates it. A parse failure is
/// never cached, so a repaired file is picked up on the next read.
static STORE_CACHE: OnceLock<Mutex<BTreeMap<PathBuf, CachedStore>>> = OnceLock::new();

fn store_cache() -> &'static Mutex<BTreeMap<PathBuf, CachedStore>> {
    STORE_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// FNV-1a 64-bit hash of the store bytes. The cache uses it as the content
/// identity next to the metadata stamp: the hash is exact for the bytes read,
/// cheap relative to the JSON parse it avoids, and does not need a new
/// dependency.
fn content_hash(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// The cached parse for `path` when its stamp and content identity still
/// describe the file. Callers that only have a metadata stamp first look up
/// [`cached_content_hash`] and re-hash the bytes on a stamp hit.
fn cached_store(
    path: &Path,
    stamp: StoreStamp,
    content: Option<u64>,
) -> Option<Arc<ModelsStoreFile>> {
    let mut cache = store_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = cache.get_mut(path)?;
    if entry.stamp != stamp || entry.content_hash != content {
        return None;
    }
    entry.hits += 1;
    Some(Arc::clone(&entry.store))
}

/// The content identity cached for `stamp`, when the cached entry's metadata
/// stamp still matches. `Some(None)` means the cached file is missing.
fn cached_content_hash(path: &Path, stamp: StoreStamp) -> Option<Option<u64>> {
    let cache = store_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = cache.get(path)?;
    (entry.stamp == stamp).then_some(entry.content_hash)
}

/// Install (or replace) the cached parse for `path`. `parsed` distinguishes a
/// fresh parse from a write-path refresh so the diagnostics stay meaningful.
fn put_cached_store(
    path: &Path,
    stamp: StoreStamp,
    content_hash: Option<u64>,
    store: Arc<ModelsStoreFile>,
    parsed: bool,
) {
    let mut cache = store_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = cache
        .entry(path.to_path_buf())
        .or_insert_with(|| CachedStore {
            stamp: StoreStamp::Missing,
            content_hash: None,
            store: Arc::new(BTreeMap::new()),
            hits: 0,
            parses: 0,
        });
    entry.stamp = stamp;
    entry.content_hash = content_hash;
    entry.store = store;
    if parsed {
        entry.parses += 1;
    }
}

/// Cache activity for one agent dir (diagnostic/test seam).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelsStoreCacheStats {
    /// Reads answered from the cached parse.
    pub hits: u64,
    /// Parses of the store file (a miss followed by a successful read).
    pub parses: u64,
}

/// Snapshot the cache counters for `agent_dir`'s store.
pub fn models_store_cache_stats(agent_dir: &Path) -> ModelsStoreCacheStats {
    let path = models_store_path(agent_dir);
    let cache = store_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .get(&path)
        .map_or_else(ModelsStoreCacheStats::default, |entry| {
            ModelsStoreCacheStats {
                hits: entry.hits,
                parses: entry.parses,
            }
        })
}

/// Persistence for dynamically refreshed provider catalogs (upstream
/// `ModelsStore`): one entry per provider id.
///
/// Operations are synchronous; the async refresh API awaits them inside its
/// provider tasks, which keeps the trait object-safe and lets the default
/// implementation stay in memory.
pub trait ModelsStore: Send + Sync {
    /// Read one provider's entry, `None` when nothing is stored.
    fn read(&self, provider_id: &str) -> crate::ai::error::AiResult<Option<ModelsStoreEntry>>;

    /// Write (upsert) one provider's entry.
    fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> crate::ai::error::AiResult<()>;

    /// Delete one provider's entry (no-op when absent).
    fn delete(&self, provider_id: &str) -> crate::ai::error::AiResult<()>;

    /// Whether this store already persists to the agent dir's
    /// `models-store.json`. A session host that mirrors extension publications
    /// into `<agent_dir>/models-store.json` uses this to avoid writing the same
    /// file twice when the registry was built with [`FileModelsStore`].
    fn is_file_backed(&self) -> bool {
        false
    }
}

/// Default in-memory [`ModelsStore`] (upstream `InMemoryModelsStore`).
#[derive(Debug, Default)]
pub struct InMemoryModelsStore {
    entries: std::sync::Mutex<BTreeMap<String, ModelsStoreEntry>>,
}

impl InMemoryModelsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// All stored entries (test/diagnostic seam).
    pub fn entries(&self) -> BTreeMap<String, ModelsStoreEntry> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ModelsStore for InMemoryModelsStore {
    fn read(&self, provider_id: &str) -> crate::ai::error::AiResult<Option<ModelsStoreEntry>> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider_id)
            .cloned())
    }

    fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> crate::ai::error::AiResult<()> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider_id.to_string(), entry);
        Ok(())
    }

    fn delete(&self, provider_id: &str) -> crate::ai::error::AiResult<()> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider_id);
        Ok(())
    }
}

/// File-backed [`ModelsStore`] over `<agent_dir>/models-store.json` (the same
/// locked file the remote-catalog overlay uses).
#[derive(Debug, Clone)]
pub struct FileModelsStore {
    agent_dir: PathBuf,
}

impl FileModelsStore {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
        }
    }
}

impl ModelsStore for FileModelsStore {
    fn read(&self, provider_id: &str) -> crate::ai::error::AiResult<Option<ModelsStoreEntry>> {
        Ok(read_models_store_entry(&self.agent_dir, provider_id))
    }

    fn write(&self, provider_id: &str, entry: ModelsStoreEntry) -> crate::ai::error::AiResult<()> {
        // A failed catalog write is a storage failure, never an authentication
        // one: callers prefix their own context ("models-store write failed")
        // instead of inheriting a misleading "authentication failure" label.
        write_models_store_entry(&self.agent_dir, provider_id, entry)
            .map_err(crate::ai::error::AiError::ModelsStore)
    }

    fn delete(&self, provider_id: &str) -> crate::ai::error::AiResult<()> {
        delete_models_store_entry(&self.agent_dir, provider_id)
            .map_err(crate::ai::error::AiError::ModelsStore)
    }

    fn is_file_backed(&self) -> bool {
        true
    }
}

/// The `<agentDir>/models-store.json` path.
pub fn models_store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models-store.json")
}

/// Serialize every models-store operation in this process. The on-disk lock
/// ([`crate::config::settings::with_settings_file_lock`]) covers other
/// processes, but its acquisition is best-effort (it proceeds unlocked after
/// its retry budget), so this guard is what actually makes concurrent threads
/// read-modify-write the same file safely.
fn store_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read and parse the whole store, returning the parse plus the content hash
/// of the bytes it came from (`None` for a missing file). A read or parse
/// failure is an error the callers turn into a diagnostic. They must never
/// treat it as empty and write the result back (that would erase every entry
/// the corrupt file still contains).
fn read_store(path: &Path) -> Result<(ModelsStoreFile, Option<u64>), String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((BTreeMap::new(), None));
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let hash = content_hash(&bytes);
    let contents = std::str::from_utf8(&bytes)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    // A zero-length/whitespace file holds no entries: there is nothing to
    // preserve, so it reads as an empty store (and the next atomic write
    // repairs it). Any other parse failure keeps the old bytes.
    if contents.trim().is_empty() {
        return Ok((BTreeMap::new(), Some(hash)));
    }
    serde_json::from_str(contents)
        .map(|store| (store, Some(hash)))
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

/// A sibling temporary path used for the atomic replacement (same directory,
/// so the final `rename` never crosses a volume). The sequence suffix keeps
/// concurrent writers/tests from colliding on the temp name.
fn temp_store_path(path: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "models-store.json".to_string());
    path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()))
}

/// `rename` the temp file over the destination, retrying briefly. On Windows
/// a reader that opened the target with an incompatible sharing mode can make
/// `rename` fail for a moment; the destination is never touched until the
/// rename succeeds.
fn replace_store_file(temp: &Path, path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..10 {
        match std::fs::rename(temp, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = Some(error);
                if attempt < 9 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("models-store rename failed")))
}

/// Write the whole store via temp-file + rename: a crash mid-write leaves the
/// previous catalog intact instead of a truncated file. Returns the content
/// hash of the exact bytes written, which the parse cache installs alongside
/// the post-write metadata stamp.
fn write_store(path: &Path, store: &ModelsStoreFile) -> Result<u64, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }
    // Serialization failures propagate: writing an empty document here would
    // silently delete the catalog.
    let serialized = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("cannot serialize {}: {error}", path.display()))?;
    let hash = content_hash(&serialized);
    let temp = temp_store_path(path);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&serialized)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace_store_file(&temp, path)
    })();
    match written {
        Ok(()) => Ok(hash),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(format!("cannot write {}: {error}", path.display()))
        }
    }
}

/// Read one provider's cached catalog entry (upstream `ModelsStore.read`).
///
/// The first read of a file version parses the store under the same lock the
/// writers hold, so it can never observe a half-replaced file; later reads of
/// the unchanged file are served from that parse after a metadata probe and a
/// content-hash verification. A parse failure is reported and yields `None`;
/// the file is left exactly as it was.
pub fn read_models_store_entry(agent_dir: &Path, provider_id: &str) -> Option<ModelsStoreEntry> {
    let path = models_store_path(agent_dir);
    match read_store_cached(&path) {
        Ok(store) => store.get(provider_id).cloned(),
        Err(error) => {
            crate::ext_diag!("[models-store] {error}; keeping the existing file untouched");
            None
        }
    }
}

/// The store through the parse cache: a metadata stamp hit is only a
/// candidate, because a same-length in-place rewrite can land inside one
/// filesystem timestamp tick. The bytes are hashed again and the cached parse
/// is served only when the content identity matches too. A mismatch (or a
/// changed/deleted file) re-reads under the same in-process mutex and
/// best-effort file lock the writers use.
fn read_store_cached(path: &Path) -> Result<Arc<ModelsStoreFile>, String> {
    let stamp = store_stamp(path);
    if let Some(store) = cached_store_for_stamp(path, stamp) {
        return Ok(store);
    }
    let _lock = store_lock();
    // Another thread may have parsed the same version while this one waited.
    let stamp = store_stamp(path);
    if let Some(store) = cached_store_for_stamp(path, stamp) {
        return Ok(store);
    }
    crate::config::settings::with_settings_file_lock(path, || {
        let before = store_stamp(path);
        let (store, content_hash) = read_store(path)?;
        // Only publish the parse when the file did not change underneath it;
        // caching a stamp that describes different bytes would pin stale
        // content until the next rewrite.
        let after = store_stamp(path);
        let store = Arc::new(store);
        if before == after {
            put_cached_store(path, after, content_hash, Arc::clone(&store), true);
        }
        Ok(store)
    })
}

/// Serve the cached parse for `stamp` after verifying the content identity.
/// `None` means "re-read and re-parse"; a present file must hash to the
/// cached value, so a same-stamp content change cannot be served stale.
fn cached_store_for_stamp(path: &Path, stamp: StoreStamp) -> Option<Arc<ModelsStoreFile>> {
    match cached_content_hash(path, stamp)? {
        // Missing file: the stamp itself is the identity, no bytes to hash.
        None => cached_store(path, stamp, None),
        Some(expected) => {
            let bytes = std::fs::read(path).ok()?;
            if content_hash(&bytes) == expected {
                cached_store(path, stamp, Some(expected))
            } else {
                None
            }
        }
    }
}

/// Write one provider's cached catalog entry under a file lock (upstream
/// `ModelsStore.write`). This is the refresh hook the remote-catalog fetcher
/// calls after a successful download.
///
/// A pre-existing file that cannot be read or parsed is NOT overwritten: the
/// write fails (with a diagnostic) so the old catalog survives for a later
/// recovery/manual repair.
pub fn write_models_store_entry(
    agent_dir: &Path,
    provider_id: &str,
    entry: ModelsStoreEntry,
) -> Result<(), String> {
    let path = models_store_path(agent_dir);
    let _lock = store_lock();
    crate::config::settings::with_settings_file_lock(&path, || {
        let (mut store, _) = read_store(&path).map_err(|error| {
            crate::ext_diag!("[models-store] {error}; refusing to overwrite the existing catalog");
            error
        })?;
        store.insert(provider_id.to_string(), entry);
        let hash = write_store(&path, &store)?;
        // The just-written bytes are the current file: refresh the parse cache
        // instead of making the next reader parse them again. The hash is of
        // those exact bytes, so a later same-stamp external rewrite is still
        // detected by the content check.
        put_cached_store(
            &path,
            store_stamp(&path),
            Some(hash),
            Arc::new(store),
            false,
        );
        Ok(())
    })
}

/// Delete one provider's cached catalog entry under a file lock (upstream
/// `ModelsStore.delete`).
pub fn delete_models_store_entry(agent_dir: &Path, provider_id: &str) -> Result<(), String> {
    let path = models_store_path(agent_dir);
    let _lock = store_lock();
    if !path.exists() {
        // Keep the cache honest about the absent file (its stamp is `Missing`).
        put_cached_store(
            &path,
            StoreStamp::Missing,
            None,
            Arc::new(BTreeMap::new()),
            false,
        );
        return Ok(());
    }
    crate::config::settings::with_settings_file_lock(&path, || {
        let (mut store, content_hash) = read_store(&path).map_err(|error| {
            crate::ext_diag!("[models-store] {error}; refusing to rewrite the existing catalog");
            error
        })?;
        if store.remove(provider_id).is_none() {
            put_cached_store(
                &path,
                store_stamp(&path),
                content_hash,
                Arc::new(store),
                false,
            );
            return Ok(());
        }
        let hash = write_store(&path, &store)?;
        put_cached_store(
            &path,
            store_stamp(&path),
            Some(hash),
            Arc::new(store),
            false,
        );
        Ok(())
    })
}
