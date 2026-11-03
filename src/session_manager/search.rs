//! Session-selector search, a Rust port of upstream
//! `modes/interactive/components/session-selector-search.ts`.
//!
//! The selector filters a session listing with:
//!
//! - **token mode** (the default): whitespace-separated tokens, ALL of which
//!   must match (multi-word AND). `"quoted phrases"` match with whitespace
//!   normalized; unbalanced quotes fall back to plain whitespace splitting.
//! - **regex mode** (`re:<pattern>`): a case-insensitive regular expression
//!   over the name, id, every user/assistant message, and the cwd.
//! - **tiered scoring**: an exact (case-insensitive) hit in the session NAME
//!   beats an exact hit in the id, message text, or cwd, which in turn beats a
//!   scattered FUZZY subsequence in the name or cwd. Each token is scored by
//!   its best field, and tier offsets dominate the fuzzy score, so an exact
//!   hit anywhere beats a short scattered match. Lower total score is better.
//! - **case-insensitivity** everywhere (upstream lowercases every field).
//!
//! The searchable text for a session includes `all_messages_text` 鈥?every
//! user and assistant message's text joined with a space (upstream
//! `buildSessionInfo`) 鈥?so a query can hit a message from any point in the
//! conversation, not just the first one.
//!
//! A listing does not materialize that corpus (R1-L3-013): its
//! `all_messages_text` is a small lazy descriptor naming the session file.
//! This module streams the file on the first query that actually has to scan
//! the messages and caches the extracted text, so repeated keystrokes do not
//! re-read the transcript.
//!
//! # Corpus caching
//!
//! Lowercasing (and, for quoted phrases, whitespace-collapsing) the whole
//! message corpus is expensive: one interactive keystroke parses the query
//! once and then matches it against EVERY listed session. Doing the transform
//! inside the per-token matcher therefore allocated a full copy of every
//! corpus for every token on every keystroke (sessions x tokens x corpus).
//!
//! Instead, the lowercase corpus and its whitespace-normalized view are built
//! lazily, once per unique corpus source (a content fingerprint for literal
//! text, the small lazy descriptor for listed sessions), and only when a token
//! actually has to scan the messages: exact name/id/cwd hits return before the
//! corpus is resolved, hashed or lowercased. Small fields (name/id/cwd) are
//! lowercased once per `match_session_search` call rather than once per token.
//! A lazily-listed transcript is keyed by its descriptor, so repeated
//! keystrokes never re-hash the whole corpus. A literal corpus is keyed by its
//! content fingerprint, but that fingerprint is itself memoized per string
//! allocation (base pointer + length, with a small content sample guarding
//! against allocator address reuse), so a stable listing hashes each literal
//! corpus once instead of once per keystroke. Both tiers keep a 128 MiB text
//! budget (enough for B8's 1,000 x 100 KB working set): the normalized tier is
//! charged its actual lowercase bytes on insert and only pays for the
//! whitespace-normalized view when a phrase query builds it. A single entry
//! that cannot fit its tier is used for the current match but never cached.
//! [`session_search_corpus_builds`], [`session_search_corpus_bytes`],
//! [`session_search_corpus_fingerprints`] and
//! [`session_search_corpus_cache_stats`] expose the counters as test seams:
//! one summary matched by any number of corpus-scanning tokens/queries
//! contributes ONE build while its cache entry stays resident, so allocation
//! volume is independent of the token count.
//!
//! [`SessionSearchQuery`] is the parsed form (parse once per keystroke, match
//! against every session); [`match_session_search`] returns the relevance
//! score when the session matches. The selector uses these directly in
//! `coding_agent::session_pick`.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use regex::{Regex, RegexBuilder};

use super::SessionSummary;

/// Exact-hit tier: the query text appears in the session name.
pub const TIER_NAME: f64 = 0.0;
/// Exact-hit tier: the query text appears in the id, messages, or cwd.
pub const TIER_TEXT: f64 = 1.0;
/// Scattered-match tier: the query characters appear in order in name/cwd.
pub const TIER_FUZZY: f64 = 2.0;
/// Tier offset applied before the within-tier score (upstream
/// `TIER_WEIGHT`). Tier offsets dominate fuzzy scores.
pub const TIER_WEIGHT: f64 = 1_000_000.0;

/// Whether a token is an exact-match phrase or a fuzzy token
/// (upstream `ParsedSearchQuery.tokens[].kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSearchTokenKind {
    Fuzzy,
    Phrase,
}

/// One parsed query token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchToken {
    pub kind: SessionSearchTokenKind,
    /// Raw fuzzy token or raw trimmed phrase (see [`Self::lower`] for the
    /// match form).
    pub value: String,
    /// Precomputed match form, so matching never re-normalizes the query per
    /// session: the lowercase fuzzy token, or the whitespace-normalized
    /// lowercase phrase.
    pub lower: String,
}

/// A parsed session search query (upstream `ParsedSearchQuery`).
///
/// `error` is set when the query was `re:` with an empty or invalid pattern;
/// such a query matches nothing (upstream `filterAndSortSessions` returns an
/// empty list).
#[derive(Debug, Clone, Default)]
pub struct SessionSearchQuery {
    pub tokens: Vec<SessionSearchToken>,
    /// Compiled case-insensitive pattern for `re:<pattern>`.
    pub regex: Option<Regex>,
    pub error: Option<String>,
}

/// Lowercase and collapse every whitespace run (upstream
/// `normalizeWhitespaceLower`): `" Foo\t Bar "` 鈫?`"foo bar"`.
pub fn normalize_whitespace_lower(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fields the search matches against (upstream `SessionInfo` facets).
#[derive(Debug, Clone, Copy)]
pub struct SessionSearchFields<'a> {
    pub id: &'a str,
    pub cwd: &'a str,
    pub name: Option<&'a str>,
    /// The searchable message corpus: either the literal joined text or a
    /// listing's lazy descriptor (resolved by [`match_session_search`]).
    pub all_messages_text: &'a str,
}

impl SessionSummary {
    /// The searchable facets of this listing record.
    pub fn search_fields(&self) -> SessionSearchFields<'_> {
        SessionSearchFields {
            id: &self.id,
            cwd: &self.cwd,
            name: self.name.as_deref(),
            all_messages_text: &self.all_messages_text,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-corpus normalization cache
// ---------------------------------------------------------------------------

/// The one-time lowercase / whitespace-normalized views of one message corpus.
///
/// `normalized` is built lazily from `lower`: `split_whitespace` on an already
/// lowercased string is exactly `normalize_whitespace_lower` of the original,
/// but phrase queries only pay for the second allocation when they occur.
struct CachedSearchCorpus {
    lower: String,
    normalized: OnceCell<String>,
}

impl CachedSearchCorpus {
    fn build(text: &str) -> Self {
        Self {
            lower: text.to_lowercase(),
            normalized: OnceCell::new(),
        }
    }

    fn normalized(&self) -> &str {
        self.normalized
            .get_or_init(|| self.lower.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    fn normalized_is_built(&self) -> bool {
        self.normalized.get().is_some()
    }

    /// Resident bytes charged to the cache budget: the lowercase text plus the
    /// whitespace-normalized view only once a phrase query has actually built
    /// it (`split_whitespace().join(" ")` never grows the text). Charging the
    /// actual views keeps the byte cap a real bound without reserving a second
    /// copy for corpora that are only ever matched by literal tokens.
    fn charged_bytes(&self) -> usize {
        self.lower.len().saturating_add(
            self.normalized
                .get()
                .map_or(0, |normalized| normalized.len()),
        )
    }
}

/// Cache key for one normalized corpus.
///
/// A literal corpus (an in-memory `SessionSummary` / test fixture) is keyed by
/// its content fingerprint. A listing's lazy descriptor is keyed by the small
/// descriptor itself instead: it already embeds the path plus the file's
/// length/mtime snapshot, so repeated keystrokes can reuse the normalized text
/// without re-hashing the whole transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CorpusCacheKey {
    Content { fingerprint: u64, len: usize },
    Descriptor { fingerprint: u64, len: usize },
}

/// Chunked FNV-1a: stable, allocation-free, and much cheaper than
/// lowercasing/allocating a corpus.
///
/// Literal corpora call it once per allocation through
/// [`literal_corpus_fingerprint`]; lazy-listed corpora call it on the small
/// descriptor only. The fingerprint seam counts invocations and hashed bytes
/// so tests can prove that exact name/id/cwd hits never pay an O(corpus) hash
/// and that repeated keystrokes do not re-hash a literal corpus.
fn corpus_fingerprint(text: &str) -> u64 {
    SEARCH_CORPUS_FINGERPRINTS.with(|count| count.set(count.get().saturating_add(1)));
    SEARCH_CORPUS_FINGERPRINT_BYTES.with(|bytes| {
        bytes.set(bytes.get().saturating_add(text.len() as u64));
    });
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET ^ (text.len() as u64).rotate_left(32);
    let (words, remainder) = text.as_bytes().as_chunks::<8>();
    for word in words {
        hash ^= u64::from_le_bytes(*word);
        hash = hash.wrapping_mul(PRIME).rotate_left(17);
    }
    for byte in remainder {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Identity of one in-memory literal-corpus allocation: the base pointer and
/// byte length of the `SessionSummary::all_messages_text` string.
///
/// Moving a `String` keeps its heap buffer address stable, so the same
/// listing summary hits this key across keystrokes. A cloned summary gets a
/// new buffer (and is deduplicated one level down by the content fingerprint).
/// `len` keeps empty strings (Rust's shared dangling pointer) harmless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CorpusAllocationKey {
    ptr: usize,
    len: usize,
}

impl CorpusAllocationKey {
    fn of(text: &str) -> Self {
        Self {
            ptr: text.as_ptr() as usize,
            len: text.len(),
        }
    }
}

/// A constant-size content sample (head/quarter/middle/three-quarter/tail)
/// used to detect a recycled heap address.
///
/// Allocation identity alone cannot distinguish a freed buffer from a new
/// string allocated at the same address with the same length. Comparing a few
/// fixed windows makes accidental aliasing vanishingly unlikely without
/// turning the memo lookup back into an O(n) whole-corpus hash. This is a
/// correctness guard only: the full fingerprint is still computed once per
/// real corpus allocation.
fn corpus_sample_signature(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const WINDOW: usize = 8;

    fn feed(hash: &mut u64, slice: &[u8]) {
        for &byte in slice {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(PRIME);
        }
    }

    let bytes = text.as_bytes();
    let mut hash = OFFSET ^ (bytes.len() as u64).rotate_left(32);
    let starts = [
        0,
        bytes.len() / 4,
        bytes.len() / 2,
        bytes.len().saturating_mul(3) / 4,
        bytes.len().saturating_sub(WINDOW),
    ];
    for start in starts {
        let end = (start + WINDOW).min(bytes.len());
        feed(&mut hash, &bytes[start..end]);
        hash = hash.wrapping_mul(PRIME) ^ start as u64;
    }
    hash
}

#[derive(Debug, Clone, Copy)]
struct LiteralFingerprintEntry {
    /// Sample of the original bytes; a mismatch means the address was reused.
    sample: u64,
    fingerprint: u64,
}

/// Bounded FIFO memo of literal-corpus fingerprints keyed by allocation.
///
/// Hashing a 100 KB literal corpus on every keystroke is the remaining
/// R1-L3-012 cost: the content-addressed normalized cache hit alone does not
/// help because finding the key needs the O(n) hash. This memo turns later
/// lookups into O(1) pointer/len probes. It is deliberately small and bounded
/// (a few hundred KB at most) so it cannot retain transcript data: only the
/// key and two `u64`s are stored, never the corpus itself.
struct LiteralFingerprintCache {
    entries: HashMap<CorpusAllocationKey, LiteralFingerprintEntry>,
    order: VecDeque<CorpusAllocationKey>,
    max_entries: usize,
    evictions: u64,
}

impl LiteralFingerprintCache {
    /// Four times B8's 1,000-summary fixture; the memo is tiny (~32 B/entry
    /// plus allocator overhead), so this covers a full working set plus two
    /// more listings without unbounded growth.
    const MAX_ENTRIES: usize = 4_096;

    fn new() -> Self {
        Self::with_limits(Self::MAX_ENTRIES)
    }

    fn with_limits(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries,
            evictions: 0,
        }
    }

    fn get(&self, key: &CorpusAllocationKey) -> Option<LiteralFingerprintEntry> {
        self.entries.get(key).copied()
    }

    fn insert(&mut self, key: CorpusAllocationKey, entry: LiteralFingerprintEntry) {
        // Address reuse with a different sample replaces in place; keep the
        // original FIFO position (the key itself is still the oldest identity).
        if let Some(slot) = self.entries.get_mut(&key) {
            *slot = entry;
            return;
        }
        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if self.entries.remove(&oldest).is_some() {
                self.evictions = self.evictions.saturating_add(1);
            }
        }
        self.entries.insert(key, entry);
        self.order.push_back(key);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.evictions = 0;
    }

    #[allow(dead_code)] // exercised by the unit-test seam only
    fn stats(&self) -> (usize, u64) {
        (self.entries.len(), self.evictions)
    }
}

/// Fingerprint a literal corpus at most once per allocation.
///
/// The happy path (same `SessionSummary` across keystrokes) is a pointer/len
/// hash-map probe plus a bounded content sample. Only a new allocation (or a
/// sample mismatch after allocator reuse) pays the O(n) [`corpus_fingerprint`].
fn literal_corpus_fingerprint(text: &str) -> u64 {
    let key = CorpusAllocationKey::of(text);
    let sample = corpus_sample_signature(text);
    if let Some(entry) = LITERAL_FINGERPRINT_CACHE.with(|cache| cache.borrow().get(&key)) {
        if entry.sample == sample {
            return entry.fingerprint;
        }
    }
    let fingerprint = corpus_fingerprint(text);
    LITERAL_FINGERPRINT_CACHE.with(|cache| {
        cache.borrow_mut().insert(
            key,
            LiteralFingerprintEntry {
                sample,
                fingerprint,
            },
        );
    });
    fingerprint
}

/// Read-only view of one corpus cache's accounting (test seam).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSearchCacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub max_entries: usize,
    pub max_bytes: usize,
    pub evictions: u64,
    pub rejected: u64,
}

/// One resident normalized corpus plus the bytes currently charged for it.
struct SearchCorpusCacheEntry {
    corpus: Rc<CachedSearchCorpus>,
    /// Lowercase bytes at insert, growing by the normalized length when a
    /// phrase query builds that view (see [`SearchCorpusCache::recharge`]).
    charged: usize,
}

/// Bounded FIFO cache of normalized corpora (thread-local: the selector is a
/// single-threaded UI loop, and keeping it thread-local avoids locking on the
/// per-keystroke hot path).
struct SearchCorpusCache {
    entries: HashMap<CorpusCacheKey, SearchCorpusCacheEntry>,
    order: VecDeque<CorpusCacheKey>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    evictions: u64,
    rejected: u64,
}

impl SearchCorpusCache {
    const MAX_ENTRIES: usize = 1024;
    /// Budget for resident corpus text. Each entry is charged its actual
    /// [`CachedSearchCorpus::charged_bytes`]: the lowercase text on insert plus
    /// the whitespace-normalized view only if/when a phrase query builds it.
    /// 128 MiB holds B8's 1,000 x 100 KB lowercase working set (~95.4 MiB), so
    /// a sequential second keystroke round does not rebuild any lower view.
    const MAX_BYTES: usize = 128 * 1024 * 1024;

    fn new() -> Self {
        Self::with_limits(Self::MAX_ENTRIES, Self::MAX_BYTES)
    }

    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            evictions: 0,
            rejected: 0,
        }
    }

    fn get(&self, key: &CorpusCacheKey) -> Option<Rc<CachedSearchCorpus>> {
        self.entries.get(key).map(|entry| entry.corpus.clone())
    }

    fn insert(&mut self, key: CorpusCacheKey, corpus: Rc<CachedSearchCorpus>) {
        let needed = corpus.charged_bytes();
        if needed > self.max_bytes {
            // A corpus that cannot fit even in an empty cache is normalized
            // for this match and dropped instead of bypassing the budget.
            self.rejected = self.rejected.saturating_add(1);
            return;
        }
        debug_assert!(!self.entries.contains_key(&key));
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self.bytes.saturating_add(needed) > self.max_bytes)
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.charged);
                self.evictions = self.evictions.saturating_add(1);
            }
        }
        self.bytes = self.bytes.saturating_add(needed);
        self.entries.insert(
            key,
            SearchCorpusCacheEntry {
                corpus,
                charged: needed,
            },
        );
        self.order.push_back(key);
    }

    /// Account for a lazily built normalized view on a resident entry.
    ///
    /// `CachedSearchCorpus::normalized` takes `&self` and cannot reach the
    /// cache (the entry may not even be resident), so the caller reports the
    /// growth after the build. FIFO eviction restores the byte bound; an entry
    /// whose normalized view pushes its own total over the budget is evicted
    /// too and simply used for the current match without being cached again.
    fn recharge(&mut self, key: &CorpusCacheKey, corpus: &Rc<CachedSearchCorpus>) {
        let Some(entry) = self.entries.get(key) else {
            return;
        };
        if !Rc::ptr_eq(&entry.corpus, corpus) {
            return;
        }
        let previous = entry.charged;
        let current = corpus.charged_bytes();
        if current <= previous {
            return;
        }
        if let Some(entry) = self.entries.get_mut(key) {
            entry.charged = current;
        }
        self.bytes = self.bytes.saturating_add(current - previous);
        while self.bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.charged);
                self.evictions = self.evictions.saturating_add(1);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
        self.evictions = 0;
        self.rejected = 0;
    }

    fn stats(&self) -> SessionSearchCacheStats {
        SessionSearchCacheStats {
            entries: self.entries.len(),
            bytes: self.bytes,
            max_entries: self.max_entries,
            max_bytes: self.max_bytes,
            evictions: self.evictions,
            rejected: self.rejected,
        }
    }
}

/// Bounded FIFO cache of corpora extracted from lazily-listed session files
/// (R1-L3-013). Keyed by the listing descriptor (which embeds the file's
/// listing fingerprint), so a re-listing after an append misses instead of
/// serving stale text. The extracted text is charged at its full length; the
/// downstream [`SearchCorpusCache`] holds the lowercased/normalized views.
///
/// `entries` + `order` give O(1) lookup and O(1) FIFO eviction: no eviction
/// path scans the map or the queue.
///
/// # Budget
///
/// Raw transcripts and normalized corpora each get 128 MiB. At 100 KB per
/// session that holds B8's full 1,000-session working set (~95.4 MiB) in both
/// tiers: the raw text is read once and the lowercase view is charged 1x (the
/// whitespace-normalized view only when a phrase query builds it), so two
/// consecutive query rounds are read-free and rebuild-free. The tradeoff is
/// explicit: charged corpus text is bounded at 128 + 128 = 256 MiB per
/// selector thread, with descriptor keys, `Rc`/`HashMap`/`VecDeque` overhead
/// and the transient `Vec<&str>` used by `normalized()` outside that charge.
/// A single transcript larger than 128 MiB is still matched but never cached.
struct LazyCorpusCache {
    entries: HashMap<String, Rc<str>>,
    order: VecDeque<String>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    evictions: u64,
    rejected: u64,
}

impl LazyCorpusCache {
    /// Match [`SearchCorpusCache::MAX_ENTRIES`]: the byte budget is the real
    /// limiter, while a smaller entry cap made sequential scans over 257+
    /// sessions evict exactly the entries the next keystroke needed
    /// (near-100% miss / full re-read thrash).
    const MAX_ENTRIES: usize = SearchCorpusCache::MAX_ENTRIES;
    /// Raw-text budget: enough for 1,000 x 100 KB (B8) with headroom, so a
    /// sequential scan over the whole working set does not evict entries the
    /// next keystroke immediately needs. The normalized tier also gets 128 MiB
    /// and charges actual lowercase bytes, so the same working set stays
    /// resident there too; see the type-level budget notes for the total-memory
    /// tradeoff.
    const MAX_BYTES: usize = 128 * 1024 * 1024;

    fn new() -> Self {
        Self::with_limits(Self::MAX_ENTRIES, Self::MAX_BYTES)
    }

    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            evictions: 0,
            rejected: 0,
        }
    }

    fn get(&self, key: &str) -> Option<Rc<str>> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: String, corpus: Rc<str>) {
        if corpus.len() > self.max_bytes {
            // A single transcript larger than the whole raw-corpus budget is
            // used for this match but never cached (same rule as the
            // normalized-corpus cache).
            self.rejected = self.rejected.saturating_add(1);
            return;
        }
        debug_assert!(!self.entries.contains_key(&key));
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self.bytes.saturating_add(corpus.len()) > self.max_bytes)
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.len());
                self.evictions = self.evictions.saturating_add(1);
            }
        }
        self.bytes = self.bytes.saturating_add(corpus.len());
        self.entries.insert(key.clone(), corpus);
        self.order.push_back(key);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
        self.evictions = 0;
        self.rejected = 0;
    }

    fn stats(&self) -> SessionSearchCacheStats {
        SessionSearchCacheStats {
            entries: self.entries.len(),
            bytes: self.bytes,
            max_entries: self.max_entries,
            max_bytes: self.max_bytes,
            evictions: self.evictions,
            rejected: self.rejected,
        }
    }
}

thread_local! {
    static SEARCH_CORPUS_CACHE: RefCell<SearchCorpusCache> =
        RefCell::new(SearchCorpusCache::new());
    /// Corpora streamed from lazily-listed session files on this thread.
    static LAZY_CORPUS_CACHE: RefCell<LazyCorpusCache> =
        RefCell::new(LazyCorpusCache::new());
    /// Literal-corpus fingerprint memo keyed by string allocation. Bounded
    /// FIFO, so repeated keystrokes avoid the O(n) hash without retaining any
    /// transcript bytes.
    static LITERAL_FINGERPRINT_CACHE: RefCell<LiteralFingerprintCache> =
        RefCell::new(LiteralFingerprintCache::new());
    /// Test seam (R1-L3-012): number of corpus normalizations on this thread.
    static SEARCH_CORPUS_BUILDS: Cell<u64> = const { Cell::new(0) };
    /// Test seam (R1-L3-012): source bytes covered by those normalizations.
    static SEARCH_CORPUS_BYTES: Cell<u64> = const { Cell::new(0) };
    /// Test seam (R1-L3-012/P2): `corpus_fingerprint` invocations on this
    /// thread. Exact name/id/cwd hits must leave this at zero.
    static SEARCH_CORPUS_FINGERPRINTS: Cell<u64> = const { Cell::new(0) };
    /// Test seam (R1-L3-012/P2): bytes fed to `corpus_fingerprint` on this
    /// thread (a lazy descriptor is ~hundreds of bytes, not the transcript).
    static SEARCH_CORPUS_FINGERPRINT_BYTES: Cell<u64> = const { Cell::new(0) };
    /// Test seam (R1-L3-013): sessions whose transcript was streamed from disk
    /// on this thread because the listing stored only a lazy descriptor.
    static SEARCH_LAZY_CORPUS_LOADS: Cell<u64> = const { Cell::new(0) };
}

/// Resolve a search corpus field: a literal corpus is used as-is; a listing's
/// lazy descriptor is streamed from its session file on first use and cached
/// (thread-local) for later keystrokes.
fn lazy_corpus_for(text: &str) -> Option<Rc<str>> {
    let path = super::lazy_session_corpus_path(text)?;
    if let Some(cached) = LAZY_CORPUS_CACHE.with(|cache| cache.borrow().get(text)) {
        return Some(cached);
    }
    let corpus: Rc<str> = super::read_session_corpus_for_search(path)?.into();
    SEARCH_LAZY_CORPUS_LOADS.with(|loads| loads.set(loads.get().saturating_add(1)));
    LAZY_CORPUS_CACHE.with(|cache| cache.borrow_mut().insert(text.to_owned(), corpus.clone()));
    Some(corpus)
}

fn cached_corpus_with_key(key: CorpusCacheKey, text: &str) -> Rc<CachedSearchCorpus> {
    if let Some(cached) = SEARCH_CORPUS_CACHE.with(|cache| cache.borrow().get(&key)) {
        return cached;
    }
    let built = Rc::new(CachedSearchCorpus::build(text));
    SEARCH_CORPUS_BUILDS.with(|builds| builds.set(builds.get().saturating_add(1)));
    SEARCH_CORPUS_BYTES.with(|bytes| bytes.set(bytes.get().saturating_add(text.len() as u64)));
    SEARCH_CORPUS_CACHE.with(|cache| cache.borrow_mut().insert(key, built.clone()));
    built
}

/// Normalize a literal corpus. Literal corpora have no caller-provided stable
/// identity, so the content-addressed key uses a fingerprint that is memoized
/// per allocation ([`literal_corpus_fingerprint`]): the first query over a
/// `SessionSummary` hashes it once, later keystrokes are an O(1) probe.
///
/// Returns the cache key alongside the entry so a phrase query can report the
/// lazily built normalized view back to the cache accounting.
fn cached_corpus(text: &str) -> (Rc<CachedSearchCorpus>, CorpusCacheKey) {
    let key = CorpusCacheKey::Content {
        fingerprint: literal_corpus_fingerprint(text),
        len: text.len(),
    };
    (cached_corpus_with_key(key, text), key)
}

/// Normalize a corpus extracted from a lazy listing descriptor. The descriptor
/// already identifies the file snapshot, so only the tiny descriptor is
/// fingerprinted: repeated keystrokes never re-hash the transcript to hit the
/// normalization cache.
fn cached_lazy_corpus(descriptor: &str, text: &str) -> (Rc<CachedSearchCorpus>, CorpusCacheKey) {
    let key = CorpusCacheKey::Descriptor {
        fingerprint: corpus_fingerprint(descriptor),
        len: descriptor.len(),
    };
    (cached_corpus_with_key(key, text), key)
}

/// Number of corpus normalizations performed on the current thread (test seam,
/// R1-L3-012). While its cache entry stays resident, one summary contributes
/// ONE build no matter how many corpus-scanning tokens or queries match it; a
/// query resolved entirely by an exact name/id/cwd hit contributes none, and a
/// later query rebuilds only after FIFO eviction (or an oversized corpus that
/// is never cached).
#[doc(hidden)]
pub fn session_search_corpus_builds() -> u64 {
    SEARCH_CORPUS_BUILDS.with(Cell::get)
}

/// Total source-corpus bytes normalized on the current thread (test seam,
/// R1-L3-012).
#[doc(hidden)]
pub fn session_search_corpus_bytes() -> u64 {
    SEARCH_CORPUS_BYTES.with(Cell::get)
}

/// Number of session files streamed from disk to resolve a lazy listing
/// descriptor on the current thread (test seam, R1-L3-013). A name/id-only
/// hit contributes zero; the first corpus-scanning query contributes one per
/// distinct listing descriptor, and later keystrokes reuse the cache.
#[doc(hidden)]
pub fn session_search_lazy_corpus_loads() -> u64 {
    SEARCH_LAZY_CORPUS_LOADS.with(Cell::get)
}

/// Number of full-corpus fingerprints computed on the current thread (test
/// seam, R1-L3-012/P2). Exact name/id/cwd hits and regex name/id/cwd hits must
/// leave this at zero; a lazy descriptor hit only fingerprints the small
/// descriptor, never the transcript.
#[doc(hidden)]
pub fn session_search_corpus_fingerprints() -> u64 {
    SEARCH_CORPUS_FINGERPRINTS.with(Cell::get)
}

/// Bytes fed to [`corpus_fingerprint`] on the current thread (test seam,
/// R1-L3-012/P2).
#[doc(hidden)]
pub fn session_search_corpus_fingerprint_bytes() -> u64 {
    SEARCH_CORPUS_FINGERPRINT_BYTES.with(Cell::get)
}

/// Accounting for the normalized-corpus cache on the current thread (test
/// seam, R1-L3-012/P2): resident entries, charged bytes (lowercase plus any
/// lazily built normalized view), limits, evictions and rejected oversized
/// entries.
#[doc(hidden)]
pub fn session_search_corpus_cache_stats() -> SessionSearchCacheStats {
    SEARCH_CORPUS_CACHE.with(|cache| cache.borrow().stats())
}

/// Accounting for the raw lazy-corpus cache on the current thread (test seam,
/// R1-L3-012/P2 / R1-L3-013).
#[doc(hidden)]
pub fn session_search_lazy_corpus_cache_stats() -> SessionSearchCacheStats {
    LAZY_CORPUS_CACHE.with(|cache| cache.borrow().stats())
}

/// Clear the current thread's corpus caches and reset the normalization/load
/// counters (test seam, R1-L3-012 / R1-L3-013).
#[doc(hidden)]
pub fn reset_session_search_corpus_builds() {
    SEARCH_CORPUS_CACHE.with(|cache| cache.borrow_mut().clear());
    LAZY_CORPUS_CACHE.with(|cache| cache.borrow_mut().clear());
    LITERAL_FINGERPRINT_CACHE.with(|cache| cache.borrow_mut().clear());
    SEARCH_CORPUS_BUILDS.with(|builds| builds.set(0));
    SEARCH_CORPUS_BYTES.with(|bytes| bytes.set(0));
    SEARCH_CORPUS_FINGERPRINTS.with(|count| count.set(0));
    SEARCH_CORPUS_FINGERPRINT_BYTES.with(|bytes| bytes.set(0));
    SEARCH_LAZY_CORPUS_LOADS.with(|loads| loads.set(0));
}

/// The borrowed, per-query view of a session's fields: the small fields
/// normalized once per `match_session_search` call (NOT once per token), plus
/// the message corpus fetched lazily only when a token has to scan it.
struct PreparedFields<'a> {
    corpus_text: &'a str,
    /// The cached corpus plus its cache key, so a lazily built normalized view
    /// can be reported back to the cache for byte accounting.
    corpus: OnceCell<(Rc<CachedSearchCorpus>, CorpusCacheKey)>,
    id_lower: String,
    cwd_lower: String,
    cwd_normalized: String,
    name_lower: Option<String>,
    name_normalized: Option<String>,
}

impl<'a> PreparedFields<'a> {
    fn new(fields: &SessionSearchFields<'a>) -> Self {
        Self {
            corpus_text: fields.all_messages_text,
            corpus: OnceCell::new(),
            id_lower: fields.id.to_lowercase(),
            cwd_lower: fields.cwd.to_lowercase(),
            cwd_normalized: normalize_whitespace_lower(fields.cwd),
            name_lower: fields.name.map(str::to_lowercase),
            name_normalized: fields.name.map(normalize_whitespace_lower),
        }
    }

    /// The cached lowercase corpus, hashed/normalized on first actual use.
    ///
    /// Every small field (name, id, cwd) is checked before messages, so exact
    /// small-field hits never pay the O(corpus) fingerprint/lowercase cost.
    /// Only a token that misses all of them reaches this accessor.
    ///
    /// A listing descriptor is resolved here too, so a session file is only
    /// read when the query genuinely needs its message text (R1-L3-013).
    fn corpus_entry(&self) -> &(Rc<CachedSearchCorpus>, CorpusCacheKey) {
        self.corpus
            .get_or_init(|| match lazy_corpus_for(self.corpus_text) {
                // Key the normalization cache by the small descriptor: a
                // keystroke that hits it must not re-fingerprint the whole
                // transcript (P2-01).
                Some(text) => cached_lazy_corpus(self.corpus_text, &text),
                None => cached_corpus(self.corpus_text),
            })
    }

    fn corpus(&self) -> &CachedSearchCorpus {
        &self.corpus_entry().0
    }

    /// The whitespace-normalized corpus view, built (and charged to the cache
    /// budget) only when a phrase token actually needs it.
    fn corpus_normalized(&self) -> &str {
        let (corpus, key) = self.corpus_entry();
        let was_built = corpus.normalized_is_built();
        let normalized = corpus.normalized();
        if !was_built {
            SEARCH_CORPUS_CACHE.with(|cache| cache.borrow_mut().recharge(key, corpus));
        }
        normalized
    }
}

impl SessionSearchQuery {
    /// Parse a selector query (upstream `parseSearchQuery`):
    ///
    /// - an empty/whitespace query has no tokens and matches everything;
    /// - `re:<pattern>` is regex mode (case-insensitive; an empty or invalid
    ///   pattern sets `error` and matches nothing);
    /// - otherwise whitespace tokenizes with `"quoted phrases"`. An unclosed
    ///   quote falls back to plain whitespace splitting (every token fuzzy).
    pub fn parse(query: &str) -> Self {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Self::default();
        }

        if let Some(rest) = trimmed.strip_prefix("re:") {
            let pattern = rest.trim();
            if pattern.is_empty() {
                return Self {
                    tokens: Vec::new(),
                    regex: None,
                    error: Some("Empty regex".to_owned()),
                };
            }
            return match RegexBuilder::new(pattern).case_insensitive(true).build() {
                Ok(regex) => Self {
                    tokens: Vec::new(),
                    regex: Some(regex),
                    error: None,
                },
                Err(error) => Self {
                    tokens: Vec::new(),
                    regex: None,
                    error: Some(error.to_string()),
                },
            };
        }

        // Token mode with quote support: `foo "node cve" bar`.
        let mut tokens: Vec<SessionSearchToken> = Vec::new();
        let mut buffer = String::new();
        let mut in_quote = false;
        let mut had_unclosed_quote = false;

        fn token(kind: SessionSearchTokenKind, value: &str) -> SessionSearchToken {
            // The match form is computed once per keystroke here, never once
            // per session/token during matching.
            let lower = match kind {
                SessionSearchTokenKind::Phrase => normalize_whitespace_lower(value),
                SessionSearchTokenKind::Fuzzy => value.to_lowercase(),
            };
            SessionSearchToken {
                kind,
                value: value.to_owned(),
                lower,
            }
        }

        fn flush(
            tokens: &mut Vec<SessionSearchToken>,
            buffer: &mut String,
            kind: SessionSearchTokenKind,
        ) {
            let value = buffer.trim();
            if !value.is_empty() {
                tokens.push(token(kind, value));
            }
            buffer.clear();
        }

        for character in trimmed.chars() {
            if character == '"' {
                if in_quote {
                    flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Phrase);
                    in_quote = false;
                } else {
                    flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Fuzzy);
                    in_quote = true;
                }
                continue;
            }
            if !in_quote && character.is_whitespace() {
                flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Fuzzy);
                continue;
            }
            buffer.push(character);
        }
        if in_quote {
            had_unclosed_quote = true;
        }

        if had_unclosed_quote {
            // Upstream falls back to plain whitespace tokenization, EVERY
            // token fuzzy (a stray quote is not a phrase delimiter).
            return Self {
                tokens: trimmed
                    .split_whitespace()
                    .filter(|token| !token.is_empty())
                    .map(|value| token(SessionSearchTokenKind::Fuzzy, value))
                    .collect(),
                regex: None,
                error: None,
            };
        }

        flush(&mut tokens, &mut buffer, SessionSearchTokenKind::Fuzzy);

        Self {
            tokens,
            regex: None,
            error: None,
        }
    }

    /// Whether this query imposes no constraint (matches every session).
    pub fn is_empty(&self) -> bool {
        self.regex.is_none() && self.tokens.is_empty() && self.error.is_none()
    }
}

// ---------------------------------------------------------------------------
// Fuzzy matching (the port of upstream `packages/tui/src/fuzzy.ts`)
// ---------------------------------------------------------------------------

/// One fuzzy match (upstream `FuzzyMatch`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

impl FuzzyMatch {
    fn miss() -> Self {
        Self {
            matches: false,
            score: 0.0,
        }
    }
}

fn is_word_boundary(previous: char) -> bool {
    previous.is_whitespace() || matches!(previous, '-' | '_' | '.' | '/' | ':')
}

fn is_word_boundary_byte(previous: u8) -> bool {
    previous.is_ascii_whitespace() || matches!(previous, b'-' | b'_' | b'.' | b'/' | b':')
}

fn score_positions(text: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match: Option<usize> = None;
    let mut consecutive = 0i64;
    for &index in positions {
        match last_match {
            Some(previous) if previous + 1 == index => {
                consecutive += 1;
                score -= (consecutive * 5) as f64;
            }
            _ => {
                consecutive = 0;
                if let Some(previous) = last_match {
                    score += ((index - previous - 1) * 2) as f64;
                }
            }
        }
        if index == 0 || is_word_boundary(text[index - 1]) {
            score -= 10.0;
        }
        score += index as f64 * 0.1;
        last_match = Some(index);
    }
    score
}

/// ASCII counterpart of [`score_positions`]: byte indices are char indices for
/// ASCII text, and `is_ascii_whitespace`/the boundary set are identical, so the
/// score is bit-for-bit the same as the char implementation.
fn score_positions_bytes(text: &[u8], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match: Option<usize> = None;
    let mut consecutive = 0i64;
    for &index in positions {
        match last_match {
            Some(previous) if previous + 1 == index => {
                consecutive += 1;
                score -= (consecutive * 5) as f64;
            }
            _ => {
                consecutive = 0;
                if let Some(previous) = last_match {
                    score += ((index - previous - 1) * 2) as f64;
                }
            }
        }
        if index == 0 || is_word_boundary_byte(text[index - 1]) {
            score -= 10.0;
        }
        score += index as f64 * 0.1;
        last_match = Some(index);
    }
    score
}

fn greedy_positions(query: &[char], text: &[char]) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(query.len());
    let mut cursor = 0usize;
    for &wanted in query {
        let found = text[cursor..]
            .iter()
            .position(|&candidate| candidate == wanted)?
            + cursor;
        positions.push(found);
        cursor = found + 1;
    }
    Some(positions)
}

fn greedy_positions_bytes(query: &[u8], text: &[u8]) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(query.len());
    let mut cursor = 0usize;
    for &wanted in query {
        let found = text[cursor..]
            .iter()
            .position(|&candidate| candidate.to_ascii_lowercase() == wanted)?
            + cursor;
        positions.push(found);
        cursor = found + 1;
    }
    Some(positions)
}

fn match_query_chars(query: &[char], text: &[char]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch::miss();
    }

    let Some(mut positions) = greedy_positions(query, text) else {
        return FuzzyMatch::miss();
    };
    let mut score = score_positions(text, &positions);

    // The greedy alignment can scatter the query over an earlier, unrelated
    // part of the text; also score every contiguous occurrence and keep the
    // best alignment (upstream `matchQuery`).
    let mut start = 0usize;
    while start + query.len() <= text.len() {
        if text[start..start + query.len()] == *query {
            for (offset, position) in positions.iter_mut().enumerate() {
                *position = start + offset;
            }
            score = score.min(score_positions(text, &positions));
        }
        start += 1;
    }

    if query == text {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

fn match_query_bytes(query: &[u8], text: &[u8]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return FuzzyMatch::miss();
    }

    let Some(mut positions) = greedy_positions_bytes(query, text) else {
        return FuzzyMatch::miss();
    };
    let mut score = score_positions_bytes(text, &positions);

    let mut start = 0usize;
    while start + query.len() <= text.len() {
        if text[start..start + query.len()].eq_ignore_ascii_case(query) {
            for (offset, position) in positions.iter_mut().enumerate() {
                *position = start + offset;
            }
            score = score.min(score_positions_bytes(text, &positions));
        }
        start += 1;
    }

    if text.eq_ignore_ascii_case(query) {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// Swap the letters/digits runs of an all-alphanumeric lowercase query:
/// `"abc123"` 鈫?`"123abc"` and vice versa (upstream `fuzzyMatch` fallback).
fn swap_alpha_numeric(query: &[char]) -> Vec<char> {
    if query.is_empty()
        || !query
            .iter()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Vec::new();
    }
    let Some(split) = (1..query.len()).find(|split| {
        let (head, tail) = query.split_at(*split);
        let head_letters = head[0].is_ascii_lowercase();
        let head_uniform = head.iter().all(|c| c.is_ascii_lowercase() == head_letters);
        let tail_letters = tail[0].is_ascii_lowercase();
        let tail_uniform = tail.iter().all(|c| c.is_ascii_lowercase() == tail_letters);
        head_uniform && tail_uniform && head_letters != tail_letters
    }) else {
        return Vec::new();
    };
    let (head, tail) = query.split_at(split);
    tail.iter().chain(head.iter()).copied().collect()
}

fn swap_alpha_numeric_bytes(query: &[u8]) -> Vec<u8> {
    if query.is_empty() || !query.iter().all(u8::is_ascii_alphanumeric) {
        return Vec::new();
    }
    let Some(split) = (1..query.len()).find(|split| {
        let (head, tail) = query.split_at(*split);
        let head_letters = head[0].is_ascii_lowercase();
        let head_uniform = head.iter().all(|c| c.is_ascii_lowercase() == head_letters);
        let tail_letters = tail[0].is_ascii_lowercase();
        let tail_uniform = tail.iter().all(|c| c.is_ascii_lowercase() == tail_letters);
        head_uniform && tail_uniform && head_letters != tail_letters
    }) else {
        return Vec::new();
    };
    let (head, tail) = query.split_at(split);
    tail.iter().chain(head.iter()).copied().collect()
}

/// ASCII fast path: no `Vec<char>` and no lowercase copy of `text`. `query` is
/// already ASCII-lowercase; comparisons fold `text` byte-by-byte.
fn fuzzy_match_ascii(query_lower: &str, text: &str) -> FuzzyMatch {
    let text_bytes = text.as_bytes();
    let primary = match_query_bytes(query_lower.as_bytes(), text_bytes);
    if primary.matches {
        return primary;
    }

    let swapped = swap_alpha_numeric_bytes(query_lower.as_bytes());
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query_bytes(&swapped, text_bytes);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Unicode path: both inputs are already lowercase. Only this path collects
/// the text into `Vec<char>`; ASCII name/cwd/corpora never hit it.
fn fuzzy_match_unicode(query_lower: &str, text_lower: &str) -> FuzzyMatch {
    let query_chars: Vec<char> = query_lower.chars().collect();
    let text_chars: Vec<char> = text_lower.chars().collect();

    let primary = match_query_chars(&query_chars, &text_chars);
    if primary.matches {
        return primary;
    }

    let swapped = swap_alpha_numeric(&query_chars);
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query_chars(&swapped, &text_chars);
    if !swapped_match.matches {
        return primary;
    }
    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Fuzzy match `query` against `text` (upstream `fuzzyMatch`). Lower score is
/// better; falls back to the swapped letters/digits form with a +5 penalty.
///
/// ASCII text is matched on bytes (no whole-string `Vec<char>` and no
/// lowercase copy of `text`); non-ASCII input keeps the Unicode char path.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    if query.is_ascii() && text.is_ascii() {
        return fuzzy_match_ascii(&query.to_ascii_lowercase(), text);
    }
    let query_lower = query.to_lowercase();
    let text_lower = text.to_lowercase();
    fuzzy_match_unicode(&query_lower, &text_lower)
}

/// [`fuzzy_match`] for inputs already lowercased by the caller (the search
/// hot path: cached corpus/name/cwd and pre-lowercased tokens). Avoids
/// re-lowercasing the text for every token.
fn fuzzy_match_lower(query_lower: &str, text_lower: &str) -> FuzzyMatch {
    if query_lower.is_ascii() && text_lower.is_ascii() {
        fuzzy_match_ascii(query_lower, text_lower)
    } else {
        fuzzy_match_unicode(query_lower, text_lower)
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

fn tier_score(tier: f64, score: f64) -> f64 {
    tier * TIER_WEIGHT + score
}

/// Whether the session carries a non-blank display name (upstream
/// `hasSessionName`).
pub fn has_session_name(name: Option<&str>) -> bool {
    name.is_some_and(|name| !name.trim().is_empty())
}

fn match_literal_token(fields: &PreparedFields<'_>, value: &str) -> Option<f64> {
    if let Some(name) = fields.name_lower.as_deref() {
        if name.contains(value) {
            return Some(tier_score(TIER_NAME, fuzzy_match_lower(value, name).score));
        }
    }
    if fields.id_lower.contains(value) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    // All remaining literal hits share TIER_TEXT/0.0, so checking the small
    // cwd before the corpus is score-identical and keeps cwd-only hits from
    // reading/normalizing a transcript (P2-01 field laziness).
    if fields.cwd_lower.contains(value) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if fields.corpus().lower.contains(value) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

fn match_fuzzy_token(fields: &PreparedFields<'_>, value: &str) -> Option<f64> {
    let literal = match_literal_token(fields, value);
    if literal.is_some() {
        return literal;
    }

    let name = fields.name_lower.as_deref().unwrap_or("");
    let mut best: Option<f64> = None;
    for field in [name, fields.cwd_lower.as_str()] {
        let matched = fuzzy_match_lower(value, field);
        if matched.matches && best.is_none_or(|score| matched.score < score) {
            best = Some(matched.score);
        }
    }
    best.map(|score| tier_score(TIER_FUZZY, score))
}

fn match_phrase_token(fields: &PreparedFields<'_>, normalized: &str) -> Option<f64> {
    if normalized.is_empty() {
        return Some(0.0);
    }
    if let Some(name) = fields.name_normalized.as_deref() {
        if name.contains(normalized) {
            return Some(tier_score(
                TIER_NAME,
                fuzzy_match_lower(normalized, name).score,
            ));
        }
    }
    if fields.id_lower.contains(normalized) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    // See `match_literal_token`: cwd and corpus are both TIER_TEXT, so the
    // cheap small field is checked first and a cwd hit never touches corpus.
    if fields.cwd_normalized.contains(normalized) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    if fields.corpus_normalized().contains(normalized) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

fn match_regex(fields: &SessionSearchFields<'_>, regex: &Regex) -> Option<f64> {
    if fields
        .name
        .is_some_and(|name| !name.is_empty() && regex.is_match(name))
    {
        return Some(tier_score(TIER_NAME, 0.0));
    }
    if regex.is_match(fields.id) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    // A cwd hit is TIER_TEXT like a message hit, so test the small field
    // first: a regex matching only the cwd never resolves the lazy corpus.
    if regex.is_match(fields.cwd) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    // Resolve a lazy listing descriptor only after all small fields missed: a
    // name/id/cwd hit never reads the transcript (R1-L3-013).
    let lazy = lazy_corpus_for(fields.all_messages_text);
    let corpus = lazy.as_deref().unwrap_or(fields.all_messages_text);
    if regex.is_match(corpus) {
        return Some(tier_score(TIER_TEXT, 0.0));
    }
    None
}

/// Match one session, returning `Some(score)` (lower is better) when it
/// matches, `None` otherwise (upstream `matchSession`).
///
/// Each field is matched separately, so a token never matches across field
/// boundaries; different tokens may match different fields. An empty query
/// matches with score `0`; a query whose parse failed matches nothing.
///
/// The corpus normalization is cached once per unique corpus source and is only
/// fetched when a token actually scans the messages (exact name/id/cwd hits
/// never touch it); the small fields are normalized once per call. Matching N
/// corpus-scanning tokens therefore allocates the same amount as matching one
/// (the bug this replaces normalized the whole corpus per token).
pub fn match_session_search(
    fields: &SessionSearchFields<'_>,
    query: &SessionSearchQuery,
) -> Option<f64> {
    if query.error.is_some() {
        return None;
    }
    if let Some(regex) = query.regex.as_ref() {
        return match_regex(fields, regex);
    }
    if query.tokens.is_empty() {
        return Some(0.0);
    }
    let prepared = PreparedFields::new(fields);
    let mut total = 0.0;
    for token in &query.tokens {
        let score = match token.kind {
            SessionSearchTokenKind::Phrase => match_phrase_token(&prepared, &token.lower),
            SessionSearchTokenKind::Fuzzy => match_fuzzy_token(&prepared, &token.lower),
        }?;
        total += score;
    }
    Some(total)
}

#[cfg(test)]
mod cache_unit_tests {
    use super::*;

    fn content_key(value: u64) -> CorpusCacheKey {
        CorpusCacheKey::Content {
            fingerprint: value,
            len: value as usize,
        }
    }

    #[test]
    fn search_corpus_cache_rejects_oversized_entry_and_enforces_budget() {
        let mut cache = SearchCorpusCache::with_limits(2, 12);
        let entry = |text: &str| Rc::new(CachedSearchCorpus::build(text));

        // Each three-byte corpus is charged its lowercase bytes (1x).
        cache.insert(content_key(1), entry("abc"));
        cache.insert(content_key(2), entry("def"));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().bytes, 6);

        // A third entry evicts exactly the oldest one, keeping bytes at 6.
        cache.insert(content_key(3), entry("ghi"));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.stats().bytes, 6);
        assert!(cache.get(&content_key(1)).is_none());
        assert!(cache.get(&content_key(3)).is_some());

        // Thirteen characters exceed 12: normalized for the match, never
        // cached, and the resident entries stay untouched.
        cache.insert(content_key(4), entry("abcdefghijklm"));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().bytes, 6);
        assert_eq!(cache.stats().rejected, 1);

        // A single entry exactly at the whole budget is allowed.
        cache.insert(content_key(5), entry("abcdefghijkl"));
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().bytes, 12);
    }

    #[test]
    fn search_corpus_cache_charges_normalized_lazily_and_evicts_on_growth() {
        let mut cache = SearchCorpusCache::with_limits(4, 16);
        let first = Rc::new(CachedSearchCorpus::build("aa  bb")); // lower: 6 B
        let second = Rc::new(CachedSearchCorpus::build("cc   dd")); // lower: 7 B
        let first_key = content_key(1);
        let second_key = content_key(2);

        cache.insert(first_key, first.clone());
        cache.insert(second_key, second.clone());
        assert_eq!(cache.stats().bytes, 13, "only lowercase is charged");
        assert!(!second.normalized_is_built());

        // A phrase query builds the normalized view lazily; the growth is
        // reported back to the cache and forces out the oldest entry.
        assert_eq!(second.normalized(), "cc dd");
        cache.recharge(&second_key, &second);
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(
            cache.stats().bytes,
            7 + 5,
            "lowercase plus the actual normalized view"
        );
        assert!(cache.get(&first_key).is_none());
        assert!(cache.get(&second_key).is_some());

        // Re-reporting an already charged entry is a no-op.
        cache.recharge(&second_key, &second);
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().bytes, 12);
        assert_eq!(cache.stats().evictions, 1);

        // An oversized normalized growth can evict the entry itself; the
        // caller still uses its Rc for the current match.
        let mut tiny = SearchCorpusCache::with_limits(4, 10);
        let huge = Rc::new(CachedSearchCorpus::build("aa  bb"));
        let huge_key = content_key(3);
        tiny.insert(huge_key, huge.clone());
        assert_eq!(huge.normalized(), "aa bb");
        tiny.recharge(&huge_key, &huge);
        assert_eq!(tiny.stats().entries, 0);
        assert_eq!(tiny.stats().bytes, 0);
        assert_eq!(tiny.stats().evictions, 1);
    }

    #[test]
    fn lazy_corpus_cache_rejects_oversized_entry_and_evicts_fifo() {
        let mut cache = LazyCorpusCache::with_limits(2, 10);
        let entry = |text: &str| -> Rc<str> { Rc::from(text) };

        cache.insert("k1".to_owned(), entry("12345"));
        cache.insert("k2".to_owned(), entry("abcde"));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().bytes, 10);

        cache.insert("k3".to_owned(), entry("vwxyz"));
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.stats().bytes, 10);
        assert!(cache.get("k1").is_none());
        assert!(cache.get("k3").is_some());

        // A single transcript larger than the whole budget is not cached.
        cache.insert("k4".to_owned(), entry("0123456789X"));
        assert_eq!(cache.stats().rejected, 1);
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().bytes, 10);
    }

    #[test]
    fn literal_fingerprint_cache_is_bounded_fifo_and_detects_reuse() {
        let mut cache = LiteralFingerprintCache::with_limits(2);
        let key_a = CorpusAllocationKey {
            ptr: 0x1000,
            len: 3,
        };
        let key_b = CorpusAllocationKey {
            ptr: 0x2000,
            len: 3,
        };
        let key_c = CorpusAllocationKey {
            ptr: 0x3000,
            len: 3,
        };
        let entry = |sample, fingerprint| LiteralFingerprintEntry {
            sample,
            fingerprint,
        };

        cache.insert(key_a, entry(1, 11));
        cache.insert(key_b, entry(2, 22));
        assert_eq!(cache.stats(), (2, 0));
        cache.insert(key_c, entry(3, 33));
        assert_eq!(cache.stats(), (2, 1));
        assert!(cache.get(&key_a).is_none());
        assert_eq!(cache.get(&key_c).map(|entry| entry.fingerprint), Some(33));

        // Re-inserting an existing key replaces the value without growing.
        cache.insert(key_c, entry(4, 44));
        assert_eq!(cache.stats(), (2, 1));
        assert_eq!(cache.get(&key_c).map(|entry| entry.fingerprint), Some(44));
    }

    #[test]
    fn default_cache_budgets_cover_b8_in_both_tiers() {
        // 1,000 x 100 KB ~= 95.4 MiB fits in each 128 MiB tier (lowercase is
        // charged 1x; the normalized view is only added by phrase queries).
        assert_eq!(LazyCorpusCache::MAX_BYTES, 128 * 1024 * 1024);
        assert_eq!(SearchCorpusCache::MAX_BYTES, 128 * 1024 * 1024);
        const { assert!(SearchCorpusCache::MAX_BYTES >= 1_000 * 100 * 1024) };
        assert_eq!(LazyCorpusCache::MAX_ENTRIES, 1024);
        const { assert!(SearchCorpusCache::MAX_ENTRIES >= 1_000) };
        assert_eq!(LiteralFingerprintCache::MAX_ENTRIES, 4096);
    }

    #[test]
    fn descriptor_and_content_cache_keys_are_distinct() {
        assert_ne!(
            CorpusCacheKey::Content {
                fingerprint: 7,
                len: 7
            },
            CorpusCacheKey::Descriptor {
                fingerprint: 7,
                len: 7
            }
        );
    }
}
