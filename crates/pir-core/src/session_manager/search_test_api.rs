//! Test surface for the session-search corpus caches.
//!
//! The cache tests are white-box: they build caches with tiny `with_limits`
//! budgets so eviction/rejection happen inside a handful of inserts, and they
//! read charging accounting directly. Those budgets are not reachable through
//! the thread-local singletons the production path uses, so this module
//! re-exposes the three caches — behind **opaque wrappers**, so the cache types
//! stay private to the crate.
//!
//! Shape follows `pir-telemetry::testing` (one named module holding the whole
//! test surface) rather than scattering `__test_*` functions through
//! production code.

use std::rc::Rc;

use super::{
    CachedSearchCorpus, CorpusAllocationKey, CorpusCacheKey, LazyCorpusCache,
    LiteralFingerprintCache, LiteralFingerprintEntry, SearchCorpusCache, SessionSearchCacheStats,
};

/// A key for the normalized-corpus / lazy caches.
pub struct Key(CorpusCacheKey);

impl Clone for Key {
    fn clone(&self) -> Self {
        // `CorpusCacheKey` is `Copy`.
        Key(self.0)
    }
}

/// One cached corpus (an `Rc` so a test can hold it across cache eviction).
pub struct Corpus(Rc<CachedSearchCorpus>);

impl Clone for Corpus {
    fn clone(&self) -> Self {
        Corpus(Rc::clone(&self.0))
    }
}

/// The normalized-corpus cache.
pub struct CorpusCache(SearchCorpusCache);

/// The raw lazy-corpus cache (keys are plain strings there).
pub struct LazyCache(LazyCorpusCache);

/// The literal fingerprint cache.
pub struct FingerprintCache(LiteralFingerprintCache);

/// A raw-allocation key for the literal fingerprint cache.
pub struct AllocationKey(CorpusAllocationKey);

impl Clone for AllocationKey {
    fn clone(&self) -> Self {
        AllocationKey(self.0)
    }
}

/// A content-fingerprint key (the in-memory-corpus form).
pub fn content_key(fingerprint: u64, len: usize) -> Key {
    Key(CorpusCacheKey::Content { fingerprint, len })
}

/// A listing-descriptor key.
pub fn descriptor_key(fingerprint: u64, len: usize) -> Key {
    Key(CorpusCacheKey::Descriptor { fingerprint, len })
}

/// Build a cached corpus from raw text.
pub fn corpus(text: &str) -> Corpus {
    Corpus(Rc::new(CachedSearchCorpus::build(text)))
}

impl CorpusCache {
    /// Insert one corpus.
    pub fn insert(&mut self, key: Key, corpus: Corpus) {
        self.0.insert(key.0, Rc::clone(&corpus.0));
    }

    /// Whether an entry is resident.
    pub fn contains(&self, key: &Key) -> bool {
        self.0.get(&key.0).is_some()
    }

    /// Re-report a corpus's charged bytes (after a lazily built view).
    pub fn recharge(&mut self, key: &Key, corpus: &Corpus) {
        self.0.recharge(&key.0, &corpus.0);
    }

    /// Resident entries, charged bytes, limits, evictions, rejections.
    pub fn stats(&self) -> SessionSearchCacheStats {
        self.0.stats()
    }
}

/// A normalized-corpus cache with the tiny budgets the tests use.
pub fn corpus_cache(max_entries: usize, max_bytes: usize) -> CorpusCache {
    CorpusCache(SearchCorpusCache::with_limits(max_entries, max_bytes))
}

/// The default normalized-corpus budgets (B8 sizing).
pub fn corpus_cache_max_bytes() -> usize {
    SearchCorpusCache::MAX_BYTES
}

pub fn corpus_cache_max_entries() -> usize {
    SearchCorpusCache::MAX_ENTRIES
}

/// Whether the lazily built normalized view exists yet.
pub fn normalized_is_built(corpus: &Corpus) -> bool {
    corpus.0.normalized_is_built()
}

/// The whitespace-normalized view, building it on first use (which is what
/// makes the cache re-charge the entry).
pub fn normalized(corpus: &Corpus) -> &str {
    corpus.0.normalized()
}

impl LazyCache {
    pub fn insert(&mut self, key: &str, corpus: &str) {
        self.0.insert(key.to_owned(), Rc::from(corpus));
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.get(key).is_some()
    }

    pub fn stats(&self) -> SessionSearchCacheStats {
        self.0.stats()
    }
}

/// A raw lazy-corpus cache with the tiny budgets the tests use.
pub fn lazy_cache(max_entries: usize, max_bytes: usize) -> LazyCache {
    LazyCache(LazyCorpusCache::with_limits(max_entries, max_bytes))
}

pub fn lazy_cache_max_bytes() -> usize {
    LazyCorpusCache::MAX_BYTES
}

pub fn lazy_cache_max_entries() -> usize {
    LazyCorpusCache::MAX_ENTRIES
}

impl FingerprintCache {
    pub fn insert(&mut self, key: &AllocationKey, sample: u64, fingerprint: u64) {
        self.0.insert(
            key.0,
            LiteralFingerprintEntry {
                sample,
                fingerprint,
            },
        );
    }

    /// The recorded fingerprint for `key`, if resident.
    pub fn fingerprint(&self, key: &AllocationKey) -> Option<u64> {
        self.0.get(&key.0).map(|entry| entry.fingerprint)
    }

    /// `(resident, evictions)`.
    pub fn stats(&self) -> (usize, u64) {
        self.0.stats()
    }
}

/// A literal fingerprint cache with the tiny budget the tests use.
pub fn fingerprint_cache(max_entries: usize) -> FingerprintCache {
    FingerprintCache(LiteralFingerprintCache::with_limits(max_entries))
}

pub fn fingerprint_cache_max_entries() -> usize {
    LiteralFingerprintCache::MAX_ENTRIES
}

/// A raw-allocation key for the literal fingerprint cache.
pub fn allocation_key(ptr: usize, len: usize) -> AllocationKey {
    AllocationKey(CorpusAllocationKey { ptr, len })
}
