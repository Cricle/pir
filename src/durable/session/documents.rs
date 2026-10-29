//! The loaded-document cache behind the mutation line.
//!
//! Port of upstream `session.ts`'s document cache (`#documents`,
//! `#idleDocuments`, `#attachedDocuments`, `#touchDocument`,
//! `#scheduleDocumentSweep`, `#pinDocument`, `:624-740`).
//!
//! The cache is **bounded by use, not by history** (spec §4): a document with
//! no watcher or state attached leaves memory once it was not read or written
//! for `documentRetentionMs`, and its next access reloads it from storage. An
//! observed incarnation is pinned and never evicted. Eviction runs on the
//! mutation line, so no transaction can ever lose a document it holds.
//!
//! The cache owns no lock: it is a field of [`super::line::SessionInner`] and is
//! only ever touched by the line.

use std::collections::HashMap;

use crate::durable::ids::{DocumentId, Seq};
use crate::durable::types::{DocumentAddress, DocumentRecord, JsonObject};
use serde_json::Map;

/// Default document retention: ten minutes (upstream `defaultRetention`).
pub const DEFAULT_DOCUMENT_RETENTION_MS: i64 = 600_000;

/// One committed incarnation held in memory, with its tracked value.
///
/// Upstream keeps a chord `Tracker` here so a draft can be diffed in
/// `O(changed)`. The Rust port keeps the immutable committed value and lets a
/// transaction fork a full draft, diffing it at assembly with
/// [`crate::durable::chord::diff_revisions`]. That is the documented trade-off
/// (see `src/durable/chord/mod.rs`): the tracker exists upstream to make proxy
/// mutation ergonomic in JavaScript; Rust's owned `Value` already gives
/// mutation, so only the "which leaves changed" answer is needed and the
/// differ supplies it directly.
#[derive(Clone, Debug)]
pub struct LoadedDocument {
    /// Stable string identity of the logical address.
    pub address_id: String,
    /// The incarnation record.
    pub record: DocumentRecord,
    /// Definition version the stored records were written at.
    pub stored_version: i64,
    /// Definition version whose shape `value` has (differs from
    /// `stored_version` only while an in-memory migration applies).
    pub value_version: i64,
    /// Stored deltas after the newest base.
    pub deltas_since_base: usize,
    /// The committed immutable value (detached from storage).
    pub value: Map<String, serde_json::Value>,
}

/// Retention normalized the upstream way: a non-negative number of
/// milliseconds as given, anything else (or an absent value) as the default.
#[must_use]
pub fn document_retention(ms: Option<i64>) -> i64 {
    match ms {
        Some(ms) if ms >= 0 => ms,
        _ => DEFAULT_DOCUMENT_RETENTION_MS,
    }
}

/// The millisecond form of a settings retention (`f64`), `None` when the value
/// is not a retention at all.
///
/// `settings.rs` keeps the retention as `f64` so `f64::INFINITY` can express
/// upstream's "keep documents forever"; the cache counts in `i64` milliseconds,
/// where "forever" is [`i64::MAX`]. The `as i64` cast saturates, so infinity and
/// any value beyond the `i64` range both land on [`i64::MAX`] instead of
/// overflowing. A negative or NaN value is not a retention (the caller keeps
/// the default).
#[must_use]
pub fn document_retention_millis(ms: f64) -> Option<i64> {
    if ms.is_nan() || ms < 0.0 {
        return None;
    }
    Some(ms as i64)
}

/// Bookkeeping for the loaded documents of one session.
#[derive(Debug)]
pub struct DocumentCache {
    documents: HashMap<String, LoadedDocument>,
    /// Unobserved cached documents, by address, with the time of their last use.
    idle: Vec<(String, i64)>,
    /// Observers attached per incarnation; a cached incarnation with any is
    /// never evicted.
    attached: HashMap<DocumentId, usize>,
    /// Pending eviction deadline, when one is armed.
    sweep_at: Option<i64>,
    retention_ms: i64,
}

impl DocumentCache {
    /// A cache with `retention_ms` (already normalized by
    /// [`document_retention`]).
    #[must_use]
    pub fn new(retention_ms: i64) -> Self {
        Self {
            documents: HashMap::new(),
            idle: Vec::new(),
            attached: HashMap::new(),
            sweep_at: None,
            retention_ms,
        }
    }

    /// The retention in milliseconds.
    #[must_use]
    pub fn retention_ms(&self) -> i64 {
        self.retention_ms
    }

    /// Change the retention (a live setting); a shortened retention moves the
    /// next sweep up.
    pub fn set_retention_ms(&mut self, retention_ms: i64, now: i64) {
        self.retention_ms = retention_ms;
        if let Some(since) = self.oldest_idle_since() {
            // Saturating: `i64::MAX` retention (an infinite setting) must not
            // overflow the deadline arithmetic.
            let at = since.saturating_add(retention_ms);
            if self.sweep_at.is_none_or(|current| current > at) {
                self.sweep_at = Some(at.max(now));
            }
        }
    }

    /// The cached incarnation at `address_id`.
    #[must_use]
    pub fn get(&self, address_id: &str) -> Option<&LoadedDocument> {
        self.documents.get(address_id)
    }

    /// Insert or replace one incarnation.
    pub fn insert(&mut self, document: LoadedDocument, now: i64) {
        let address_id = document.address_id.clone();
        let record_id = document.record.id;
        self.documents.insert(address_id.clone(), document);
        if !self.attached.contains_key(&record_id) {
            self.note_use(&address_id, now);
        }
    }

    /// Remove one incarnation from the cache.
    pub fn remove(&mut self, address_id: &str) {
        self.documents.remove(address_id);
        self.idle.retain(|(id, _)| id != address_id);
    }

    /// Note a use of a cached document: an unobserved one becomes the newest
    /// idle one and an eviction is scheduled.
    ///
    /// A hot document moves to the back at most once per granule (a tenth of
    /// the retention, capped at one second), matching upstream `#touchDocument`:
    /// it may then leave up to a granule early, which costs one reload, while a
    /// commit-heavy workload skips the reordering.
    pub fn note_use(&mut self, address_id: &str, now: i64) {
        let Some(document) = self.documents.get(address_id) else {
            return;
        };
        if self.attached.contains_key(&document.record.id) {
            return;
        }
        let granule = (self.retention_ms / 10).min(1_000);
        if let Some((_, since)) = self.idle.iter().find(|(id, _)| id == address_id) {
            if now - *since < granule {
                return;
            }
        }
        self.idle.retain(|(id, _)| id != address_id);
        self.idle.push((address_id.to_owned(), now));
        // Saturating: an infinite retention (`i64::MAX`) never expires and
        // must not overflow the deadline.
        if self
            .sweep_at
            .is_none_or(|at| at > now.saturating_add(self.retention_ms))
        {
            self.sweep_at = self
                .oldest_idle_since()
                .map(|since| since.saturating_add(self.retention_ms));
        }
    }

    /// The deadline of the armed sweep, if one is armed.
    #[must_use]
    pub fn sweep_deadline(&self) -> Option<i64> {
        self.sweep_at
    }

    /// Whether a sweep is armed.
    #[must_use]
    pub fn sweep_armed(&self) -> bool {
        self.sweep_at.is_some()
    }

    /// Evict the idle documents whose retention passed, oldest first, and arm
    /// the next sweep. Returns the evicted address ids.
    pub fn sweep(&mut self, now: i64) -> Vec<String> {
        // Saturating: a huge retention (`i64::MAX`, an infinite setting) means
        // nothing has expired; it must not overflow.
        let expired = now.saturating_sub(self.retention_ms);
        let mut evicted = Vec::new();
        while let Some((address_id, since)) = self.idle.first().cloned() {
            if since > expired {
                break;
            }
            self.idle.remove(0);
            self.documents.remove(&address_id);
            evicted.push(address_id);
        }
        self.sweep_at = self
            .oldest_idle_since()
            .map(|since| since.saturating_add(self.retention_ms));
        evicted
    }

    /// Pin one cached incarnation for an observer.
    ///
    /// The returned [`PinGuard`] releases it once; the incarnation is idle
    /// again after the last observer detaches. Upstream pins by incarnation ID.
    pub fn pin(&mut self, document: &LoadedDocument) -> PinGuard {
        *self.attached.entry(document.record.id).or_insert(0) += 1;
        self.idle.retain(|(id, _)| id != &document.address_id);
        PinGuard {
            record_id: document.record.id,
            address_id: document.address_id.clone(),
            released: false,
        }
    }

    /// Apply a pin guard's release. Called by [`PinGuard::drop`] through
    /// [`DocumentCache::unpin`] via the owning session (the guard cannot hold
    /// `&mut self`).
    pub fn unpin(&mut self, record_id: DocumentId, address_id: &str, now: i64) {
        let Some(count) = self.attached.get(&record_id).copied() else {
            return;
        };
        if count > 1 {
            self.attached.insert(record_id, count - 1);
            return;
        }
        self.attached.remove(&record_id);
        if self
            .documents
            .get(address_id)
            .is_some_and(|document| document.record.id == record_id)
        {
            self.note_use(address_id, now);
        }
    }

    /// Whether an incarnation has at least one observer.
    #[must_use]
    pub fn is_attached(&self, record_id: DocumentId) -> bool {
        self.attached.contains_key(&record_id)
    }

    /// Cached / idle / pinned / armed counts, mirroring upstream's
    /// `documentCacheSizes` test hook.
    #[must_use]
    pub fn sizes(&self) -> DocumentCacheSizes {
        DocumentCacheSizes {
            cached: self.documents.len(),
            idle: self.idle.len(),
            attached: self.attached.len(),
            sweep_armed: self.sweep_at.is_some(),
        }
    }

    /// Drop every loaded document; later access reloads from storage
    /// (upstream `unloadDocuments`).
    pub fn clear(&mut self) {
        self.documents.clear();
        self.idle.clear();
        self.sweep_at = None;
    }

    /// Resolve the current cached incarnation of `address`, if any.
    #[must_use]
    pub fn current_at(&self, address: &DocumentAddress) -> Option<&LoadedDocument> {
        let address_id = crate::durable::documents::address_id(address);
        self.documents.get(&address_id)
    }

    /// Mark a commit that changed the incarnation's stored provenance and
    /// install its committed value.
    ///
    /// `value` is the plan's publication value: the complete prepared value of
    /// a base or a delta. A copy carries no value in its commit, so `None`
    /// evicts the cached address instead of leaving the previous value behind.
    /// A deferred hold being stored passes the held value, which is already the
    /// newest in-memory state; its delta count and stored version advance here
    /// only because the write has just settled.
    pub fn apply_adopted(
        &mut self,
        address_id: &str,
        version: i64,
        content_is_base: bool,
        value: Option<&JsonObject>,
    ) {
        let Some(value) = value else {
            self.remove(address_id);
            return;
        };
        if let Some(document) = self.documents.get_mut(address_id) {
            document.value.clone_from(value);
            document.value_version = version;
            document.stored_version = version;
            if content_is_base {
                document.deltas_since_base = 0;
            } else {
                document.deltas_since_base += 1;
            }
        }
    }

    /// Stamp one incarnation retired at `seq` in the cache, if present.
    pub fn mark_retired(&mut self, address_id: &str, seq: Seq) {
        if let Some(document) = self.documents.get_mut(address_id) {
            document.record.retired_at = Some(seq);
        }
    }

    fn oldest_idle_since(&self) -> Option<i64> {
        self.idle.iter().map(|(_, since)| *since).min()
    }
}

/// A released-once pin for one incarnation.
///
/// The cache itself is owned by the line, so the guard only records the
/// release; the session applies it with [`DocumentCache::unpin`] when the
/// observer detaches. This keeps the guard `Send` and independent of the cache
/// lifetime.
#[derive(Debug)]
pub struct PinGuard {
    /// The pinned incarnation.
    pub record_id: DocumentId,
    /// Its logical address.
    pub address_id: String,
    released: bool,
}

impl PinGuard {
    /// Whether the pin is still held.
    #[must_use]
    pub fn is_held(&self) -> bool {
        !self.released
    }

    /// Mark the pin released (idempotent); the caller then applies
    /// [`DocumentCache::unpin`].
    pub fn release(&mut self) -> bool {
        if self.released {
            return false;
        }
        self.released = true;
        true
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        // The cache is not reachable from here; the session's observer wrapper
        // applies the release explicitly. Dropping without a release leaves the
        // pin to the session's own observer bookkeeping, which always runs its
        // detach path.
        self.released = true;
    }
}

/// The cache's counters, for diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentCacheSizes {
    /// Loaded documents.
    pub cached: usize,
    /// Unobserved documents eligible for eviction.
    pub idle: usize,
    /// Pinned incarnations.
    pub attached: usize,
    /// Whether an eviction is armed.
    pub sweep_armed: bool,
}
