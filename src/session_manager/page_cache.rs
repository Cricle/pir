//! Bounded page cache and background prefetch for tail-first session reads.
//!
//! The paged reader ([`super::SessionTailPage`]) is deliberately stateless:
//! every `read_older_page` step re-opens, re-maps and re-parses a tail window.
//! Scrolling up then costs one parse per page even when the user scrolls down
//! and back up over the same range. This module keeps the parsed pages (not
//! their mmaps) in a small LRU, and lets the TUI prefetch the next older page
//! on a background thread while the previous one is still being read.
//!
//! Design rules:
//!
//! - **Bounded.** At most [`DEFAULT_PAGE_CACHE_PAGES`] pages and
//!   [`DEFAULT_PAGE_CACHE_BYTES`] parsed bytes are retained (one page is kept
//!   even when a single page exceeds the byte budget — a page is never split).
//!   A 100 MB session therefore holds a handful of tail pages, not a
//!   transcript.
//! - **Coalesced.** One load per key is in flight; a concurrent reader of the
//!   same key waits for the leader's result instead of parsing the same page
//!   twice. A prefetch for an already-cached or in-flight key is a no-op, so
//!   the UI may call it once per frame near the scroll-up threshold.
//! - **Never on the UI thread.** [`PageCache::prefetch`] spawns a named
//!   background thread and returns immediately; [`PageCache::get_or_load`] is
//!   the blocking read the worker uses when a page is actually needed.
//! - **Append-only key.** The key is `(path, end, caps)` where `end` is the
//!   byte cursor. Session files are append-only, so bytes before `end` never
//!   change and a cached page stays valid while the file grows.
//!
//! The cache stores [`CachedTailPage`], a plain parsed snapshot: entries are
//! shared with the manager/caller through `Arc<FileEntry>`, and no mmap is
//! retained beyond the load (a 100 MB map would otherwise keep its virtual
//! range alive per cached page).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use super::tail_page::{SessionTailPage, TailPageStats};
use super::types::{FileEntry, SessionHeader};

/// Default number of parsed pages retained by [`PageCache`].
///
/// Four pages cover a normal scroll-up excursion (the resident tail plus three
/// older pages ≈ 4 MiB of raw JSONL at the default caps) while keeping the
/// cache's byte budget bounded independently of the session size.
pub const DEFAULT_PAGE_CACHE_PAGES: usize = 4;

/// Default parsed-page byte budget. Eviction starts above this; at least one
/// page is always kept (see the module docs).
pub const DEFAULT_PAGE_CACHE_BYTES: u64 = 8 * 1024 * 1024;

/// Identity of one page read. `end` is the byte cursor (exclusive) the page
/// covers; `max_bytes`/`max_entries` are part of the key because a caller may
/// read the same range with different caps and expect a different page.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageCacheKey {
    path: PathBuf,
    end: u64,
    max_bytes: usize,
    max_entries: usize,
}

impl PageCacheKey {
    /// Key for `[0, end)` of `path` at the given caps.
    pub fn new(path: PathBuf, end: u64, max_bytes: usize, max_entries: usize) -> Self {
        Self {
            path,
            end,
            max_bytes,
            max_entries,
        }
    }

    /// The file this page belongs to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The exclusive byte cursor of the page.
    pub fn end(&self) -> u64 {
        self.end
    }
}

/// A parsed page snapshot safe to keep without the mmap (see the module docs).
pub struct CachedTailPage {
    path: PathBuf,
    page_start: u64,
    window_end: u64,
    file_len: u64,
    entries: Vec<Arc<FileEntry>>,
    unparsed_lines: Vec<String>,
    header: Option<SessionHeader>,
    file_header: Option<SessionHeader>,
    stats: TailPageStats,
}

impl std::fmt::Debug for CachedTailPage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedTailPage")
            .field("path", &self.path)
            .field("page_start", &self.page_start)
            .field("window_end", &self.window_end)
            .field("stats", &self.stats)
            .finish()
    }
}

impl CachedTailPage {
    /// Snapshot one freshly parsed page.
    pub(crate) fn from_page(page: &SessionTailPage) -> Self {
        Self {
            path: page.path().to_path_buf(),
            page_start: page.page_start(),
            window_end: page.window_end(),
            file_len: page.file_len(),
            entries: page.entries().to_vec(),
            unparsed_lines: page.unparsed_lines().to_vec(),
            header: page.header().cloned(),
            file_header: page.file_header(),
            stats: page.stats(),
        }
    }

    /// The file this page came from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// First byte of the parsed window.
    pub fn page_start(&self) -> u64 {
        self.page_start
    }

    /// First byte of the region this page covered.
    pub fn window_end(&self) -> u64 {
        self.window_end
    }

    /// File length at load time.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Parsed entries, oldest first.
    pub fn entries(&self) -> &[Arc<FileEntry>] {
        &self.entries
    }

    /// Lines that failed to parse, preserved verbatim.
    pub fn unparsed_lines(&self) -> &[String] {
        &self.unparsed_lines
    }

    /// Header when the page covers the file head.
    pub fn header(&self) -> Option<&SessionHeader> {
        self.header.as_ref()
    }

    /// Header from the file's first line (bounded probe).
    pub fn file_header(&self) -> Option<&SessionHeader> {
        self.file_header.as_ref()
    }

    /// The read counters of the page.
    pub fn stats(&self) -> TailPageStats {
        self.stats
    }
}

/// Counters of the cache (test/profiling evidence). All are monotonic for the
/// lifetime of the cache except `cached_pages`/`cached_bytes`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageCacheStats {
    /// `get_or_load` calls served from the LRU.
    pub hits: u64,
    /// `get_or_load` calls that found no cached page.
    pub misses: u64,
    /// Misses that waited for another caller's in-flight load instead of
    /// starting their own parse.
    pub coalesced: u64,
    /// Pages actually parsed (including prefetches).
    pub loads: u64,
    /// Background prefetch threads started.
    pub prefetches: u64,
    /// Pages evicted by the page/byte bound.
    pub evictions: u64,
    /// Pages currently retained.
    pub cached_pages: usize,
    /// Parsed bytes currently retained.
    pub cached_bytes: u64,
}

#[derive(Default)]
struct Inflight {
    result: Mutex<Option<Result<Option<Arc<CachedTailPage>>, String>>>,
    ready: Condvar,
}

impl Inflight {
    fn publish(&self, result: Result<Option<Arc<CachedTailPage>>, String>) {
        let mut slot = self.result.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = Some(result);
        self.ready.notify_all();
    }

    fn wait(&self) -> Result<Option<Arc<CachedTailPage>>, String> {
        let mut slot = self.result.lock().unwrap_or_else(PoisonError::into_inner);
        while slot.is_none() {
            slot = self
                .ready
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
        slot.clone()
            .unwrap_or_else(|| Err("page load ended without a result".into()))
    }
}

#[derive(Default)]
struct CacheState {
    /// LRU: oldest at the front, newest at the back.
    pages: VecDeque<(PageCacheKey, Arc<CachedTailPage>)>,
    bytes: u64,
    inflight: HashMap<PageCacheKey, Arc<Inflight>>,
}

impl CacheState {
    fn get(&mut self, key: &PageCacheKey) -> Option<Arc<CachedTailPage>> {
        let position = self.pages.iter().position(|(cached, _)| cached == key)?;
        let entry = self.pages.remove(position)?;
        let page = Arc::clone(&entry.1);
        self.pages.push_back(entry);
        Some(page)
    }

    fn put(
        &mut self,
        key: PageCacheKey,
        page: Arc<CachedTailPage>,
        capacity_pages: usize,
        capacity_bytes: u64,
    ) -> u64 {
        if let Some(position) = self.pages.iter().position(|(cached, _)| cached == &key) {
            if let Some((_, old)) = self.pages.remove(position) {
                self.bytes = self.bytes.saturating_sub(old.stats().page_bytes);
            }
        }
        self.bytes = self.bytes.saturating_add(page.stats().page_bytes);
        self.pages.push_back((key, page));
        let mut evicted = 0u64;
        // Keep at least one page: a single page larger than the byte budget is
        // still the useful result of this load (and is never split).
        while self.pages.len() > capacity_pages
            || (self.bytes > capacity_bytes && self.pages.len() > 1)
        {
            let Some((_, old)) = self.pages.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.stats().page_bytes);
            evicted += 1;
        }
        evicted
    }
}

/// Bounded, coalescing cache of parsed tail pages (see the module docs).
pub struct PageCache {
    capacity_pages: usize,
    capacity_bytes: u64,
    state: Mutex<CacheState>,
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
    loads: AtomicU64,
    prefetches: AtomicU64,
    evictions: AtomicU64,
}

impl std::fmt::Debug for PageCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PageCache")
            .field("capacity_pages", &self.capacity_pages)
            .field("capacity_bytes", &self.capacity_bytes)
            .field("stats", &self.stats())
            .finish()
    }
}

impl PageCache {
    /// Create a cache bounded by `capacity_pages` and `capacity_bytes`
    /// (`0` falls back to the corresponding default).
    pub fn new(capacity_pages: usize, capacity_bytes: u64) -> Self {
        Self {
            capacity_pages: capacity_pages.max(1),
            capacity_bytes: if capacity_bytes == 0 {
                DEFAULT_PAGE_CACHE_BYTES
            } else {
                capacity_bytes
            },
            state: Mutex::new(CacheState::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            prefetches: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Snapshot of the counters.
    pub fn stats(&self) -> PageCacheStats {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        PageCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            loads: self.loads.load(Ordering::Relaxed),
            prefetches: self.prefetches.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            cached_pages: state.pages.len(),
            cached_bytes: state.bytes,
        }
    }

    /// Whether `key` is currently retained (not counting an in-flight load).
    pub fn contains(&self, key: &PageCacheKey) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pages
            .iter()
            .any(|(cached, _)| cached == key)
    }

    /// Load `key` from the cache, or parse it now while coalescing concurrent
    /// readers of the same key onto this leader. This is the blocking read
    /// used by the session worker (never by the UI thread).
    pub fn get_or_load(&self, key: PageCacheKey) -> crate::Result<Option<Arc<CachedTailPage>>> {
        let inflight = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(page) = state.get(&key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(page));
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
            if let Some(flight) = state.inflight.get(&key) {
                self.coalesced.fetch_add(1, Ordering::Relaxed);
                Some(Arc::clone(flight))
            } else {
                let flight = Arc::new(Inflight::default());
                state.inflight.insert(key.clone(), Arc::clone(&flight));
                None
            }
        };
        if let Some(flight) = inflight {
            return flight.wait().map_err(anyhow::Error::msg);
        }
        let result = self.load(&key);
        self.complete(&key, result.clone());
        result.map_err(anyhow::Error::msg)
    }

    /// Start parsing `key` on a background thread unless it is already cached
    /// or in flight. Returns whether a thread was started (test/diagnostic
    /// signal; the UI does not need it).
    pub fn prefetch(self: &Arc<Self>, key: PageCacheKey) -> bool {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.get(&key).is_some() {
                return false;
            }
            if state.inflight.contains_key(&key) {
                return false;
            }
            // One read-ahead thread per cache slot at most: a caller looping
            // over distinct cursors cannot grow the worker set without bound.
            if state.inflight.len() >= self.capacity_pages {
                return false;
            }
            state
                .inflight
                .insert(key.clone(), Arc::new(Inflight::default()));
        }
        let cache = Arc::clone(self);
        let thread_key = key.clone();
        let spawned = std::thread::Builder::new()
            .name("pir-session-page-prefetch".to_owned())
            .spawn(move || {
                let result = cache.load(&thread_key);
                cache.complete(&thread_key, result);
            });
        match spawned {
            Ok(_) => {
                self.prefetches.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(error) => {
                self.complete(&key, Err(format!("cannot spawn page prefetch: {error}")));
                false
            }
        }
    }

    /// Parse one page. Called by the leader of `get_or_load` or a prefetch
    /// thread; never holds the cache lock.
    fn load(&self, key: &PageCacheKey) -> Result<Option<Arc<CachedTailPage>>, String> {
        let page =
            SessionTailPage::open_window(&key.path, Some(key.end), key.max_bytes, key.max_entries)
                .map_err(|error| format!("cannot read older page: {error}"))?;
        let Some(page) = page else {
            return Ok(None);
        };
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(Some(Arc::new(CachedTailPage::from_page(&page))))
    }

    /// Publish a leader's result to its waiters and the LRU.
    fn complete(&self, key: &PageCacheKey, result: Result<Option<Arc<CachedTailPage>>, String>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(Some(page)) = &result {
            let evicted = state.put(
                key.clone(),
                Arc::clone(page),
                self.capacity_pages,
                self.capacity_bytes,
            );
            if evicted > 0 {
                self.evictions.fetch_add(evicted, Ordering::Relaxed);
            }
        }
        if let Some(flight) = state.inflight.remove(key) {
            drop(state);
            flight.publish(result);
        }
    }
}
