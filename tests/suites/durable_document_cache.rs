//! `session::documents` — the loaded-document cache.
//!
//! Port of upstream `session.ts`'s `#documents` / `#idleDocuments` /
//! `#attachedDocuments` / `#touchDocument` / `#scheduleDocumentSweep` /
//! `#pinDocument`. The cache is **bounded by use, not by history** (spec §4):
//! a document with no watcher leaves memory once it was unused for
//! `documentRetentionMs`, and its next access reloads it from storage.
//!
//! Two rules carry the weight:
//!
//! * an **observed** incarnation is pinned and never evicted, and
//! * eviction only ever removes documents that are not being held, so a
//!   transaction cannot lose a document it is working on.

use serde_json::{json, Map, Value};

use pir::durable::session::documents::{
    document_retention, DocumentCache, DocumentCacheSizes, LoadedDocument, PinGuard,
    DEFAULT_DOCUMENT_RETENTION_MS,
};
use pir::durable::{
    decode_id, DocumentAddress, DocumentFork, DocumentIdKind, DocumentRecord, DocumentScope, Seq,
};

fn conversation(id: i64) -> DocumentScope {
    DocumentScope::Conversation {
        conversation_id: decode_id(id),
    }
}

fn record(id: i64, scope: DocumentScope) -> DocumentRecord {
    DocumentRecord {
        id: decode_id::<DocumentIdKind>(id),
        kind: "pi.test".to_owned(),
        key: None,
        created_at: Seq::from_raw(1),
        retired_at: None,
        scope,
        history: None,
        fork: Some(DocumentFork::Initial),
    }
}

fn value(count: i64) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("count".to_owned(), json!(count));
    value
}

fn loaded(address_id: &str, id: i64, count: i64) -> LoadedDocument {
    LoadedDocument {
        address_id: address_id.to_owned(),
        record: record(id, conversation(1)),
        stored_version: 1,
        value_version: 1,
        deltas_since_base: 0,
        value: value(count),
    }
}

fn address(kind: &str, conversation_id: i64) -> DocumentAddress {
    DocumentAddress {
        kind: kind.to_owned(),
        scope: conversation(conversation_id),
        key: None,
    }
}

fn address_id(kind: &str, conversation_id: i64) -> String {
    pir::durable::documents::address_id(&address(kind, conversation_id))
}

fn cache() -> DocumentCache {
    DocumentCache::new(DEFAULT_DOCUMENT_RETENTION_MS)
}

// ── retention normalization ────────────────────────────────────────────────

#[test]
fn retention_defaults_to_ten_minutes() {
    assert_eq!(
        DEFAULT_DOCUMENT_RETENTION_MS, 600_000,
        "the documented default is ten minutes"
    );
    assert_eq!(document_retention(None), DEFAULT_DOCUMENT_RETENTION_MS);
    assert_eq!(
        document_retention(Some(-1)),
        DEFAULT_DOCUMENT_RETENTION_MS,
        "a negative retention is not a retention; it falls back to the default"
    );
}

#[test]
fn a_non_negative_retention_is_taken_as_given() {
    assert_eq!(
        document_retention(Some(0)),
        0,
        "zero means evict immediately"
    );
    assert_eq!(document_retention(Some(5_000)), 5_000);
}

// ── insert / get / remove ──────────────────────────────────────────────────

#[test]
fn an_inserted_document_is_cached_and_readable() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 7), 0);

    let document = cache.get(&id).expect("the cached document");
    assert_eq!(document.value.get("count"), Some(&json!(7)));
    assert_eq!(cache.sizes().cached, 1);
}

#[test]
fn an_insert_of_the_same_address_replaces_the_incarnation() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);
    cache.insert(loaded(&id, 1, 2), 0);

    assert_eq!(cache.sizes().cached, 1, "one address, one slot");
    assert_eq!(
        cache.get(&id).and_then(|d| d.value.get("count")),
        Some(&json!(2)),
        "the newer value wins"
    );
    assert_eq!(
        cache.sizes().idle,
        1,
        "the address moved, it did not duplicate"
    );
}

#[test]
fn a_removed_document_is_gone_from_both_the_map_and_the_idle_list() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);
    assert_eq!(cache.sizes().idle, 1);

    cache.remove(&id);
    assert!(cache.get(&id).is_none());
    assert_eq!(cache.sizes().cached, 0);
    assert_eq!(
        cache.sizes().idle,
        0,
        "removal also drops the idle bookkeeping, so no ghost address is swept"
    );
}

#[test]
fn an_unknown_address_reads_as_absent() {
    let cache = cache();
    assert!(cache.get("nothing").is_none());
    assert!(cache.current_at(&address("counter", 9)).is_none());
}

#[test]
fn current_at_resolves_the_same_address_id_the_cache_uses() {
    // `current_at` takes a structured address; `get` takes the derived id.
    // They must agree or the line would look up the wrong slot.
    let mut cache = cache();
    let address = address("pi.live", 3);
    let id = pir::durable::documents::address_id(&address);
    cache.insert(loaded(&id, 5, 1), 0);

    let found = cache.current_at(&address).expect("the cached document");
    assert_eq!(found.record.id, decode_id::<DocumentIdKind>(5));
}

// ── idle tracking and sweeping ─────────────────────────────────────────────

#[test]
fn an_inserted_document_is_idle_and_arms_a_sweep() {
    let mut cache = DocumentCache::new(1_000);
    cache.insert(loaded(&address_id("counter", 1), 1, 1), 100);

    assert_eq!(cache.sizes().idle, 1);
    assert!(cache.sweep_armed());
    assert_eq!(
        cache.sweep_deadline(),
        Some(1_100),
        "the sweep is armed one retention past the last use"
    );
}

#[test]
fn a_sweep_before_the_deadline_evicts_nothing() {
    let mut cache = DocumentCache::new(1_000);
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 100);

    assert!(
        cache.sweep(1_099).is_empty(),
        "nothing has been idle for a whole retention yet"
    );
    assert!(cache.get(&id).is_some(), "the document survives");
    assert_eq!(cache.sweep_deadline(), Some(1_100), "the sweep stays armed");
}

#[test]
fn a_sweep_at_the_deadline_evicts() {
    let mut cache = DocumentCache::new(1_000);
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 100);

    assert_eq!(cache.sweep(1_100), vec![id.clone()]);
    assert!(cache.get(&id).is_none(), "an unused document leaves memory");
    assert!(
        !cache.sweep_armed(),
        "the last idle document went, so nothing is left to sweep"
    );
}

#[test]
fn a_sweep_evicts_oldest_first_and_stops_at_the_first_live_one() {
    let mut cache = DocumentCache::new(1_000);
    let first = address_id("counter", 1);
    let second = address_id("counter", 2);
    cache.insert(loaded(&first, 1, 1), 0);
    cache.insert(loaded(&second, 2, 1), 500);

    let evicted = cache.sweep(1_200);
    assert_eq!(
        evicted,
        vec![first],
        "only the document idle for a full retention is evicted"
    );
    assert!(cache.get(&second).is_some());
    assert_eq!(
        cache.sweep_deadline(),
        Some(1_500),
        "the next sweep is re-armed from the surviving idle document"
    );
}

#[test]
fn a_zero_retention_sweeps_every_idle_document_immediately() {
    let mut cache = DocumentCache::new(0);
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 42);
    assert_eq!(
        cache.sweep(42),
        vec![id],
        "retention 0 means the document is idle the moment it is inserted"
    );
}

#[test]
fn a_use_within_one_granule_does_not_postpone_eviction() {
    // `note_use` moves a hot document to the back at most once per granule
    // (retention/10, capped at one second). Within the granule the timestamp
    // is left alone: one reload too early is cheaper than reordering on every
    // commit.
    let mut cache = DocumentCache::new(1_000); // granule = 100
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    cache.note_use(&id, 50);
    assert_eq!(
        cache.sweep(1_000),
        vec![id],
        "a use inside the granule did not move the timestamp, so the document still expires on time"
    );
}

#[test]
fn a_use_past_the_granule_postpones_eviction() {
    let mut cache = DocumentCache::new(1_000); // granule = 100
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    cache.note_use(&id, 950);
    assert!(
        cache.sweep(1_000).is_empty(),
        "the timestamp moved, so the document is still inside its retention"
    );
    assert_eq!(
        cache.sweep(1_950),
        vec![id],
        "and it expires one retention after the use that moved it"
    );
}

#[test]
fn noting_a_use_of_an_unknown_document_does_nothing() {
    let mut cache = cache();
    cache.note_use("never-loaded", 0);
    assert_eq!(cache.sizes().idle, 0);
    assert!(!cache.sweep_armed());
}

#[test]
fn a_shortened_retention_moves_the_next_sweep_up() {
    let mut cache = DocumentCache::new(10_000);
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);
    assert_eq!(cache.sweep_deadline(), Some(10_000));

    cache.set_retention_ms(1_000, 0);
    assert_eq!(cache.retention_ms(), 1_000);
    assert_eq!(
        cache.sweep_deadline(),
        Some(1_000),
        "the deadline is recomputed from the oldest use"
    );
    assert_eq!(cache.sweep(1_000), vec![id]);
}

#[test]
fn a_lengthened_retention_leaves_the_armed_sweep_alone() {
    // `set_retention_ms` only ever moves the armed sweep *earlier*. A longer
    // retention leaves the deadline where it is: the sweep fires early, finds
    // every idle document still inside the new retention, and re-arms from
    // there. One wasted wake is cheaper than tracking deadlines on every
    // setting change.
    let mut cache = DocumentCache::new(1_000);
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    cache.set_retention_ms(10_000, 0);
    assert_eq!(
        cache.sweep_deadline(),
        Some(1_000),
        "the old deadline stands"
    );
    assert!(
        cache.sweep(1_000).is_empty(),
        "nothing has been idle for the longer retention yet"
    );
    assert_eq!(
        cache.sweep_deadline(),
        Some(10_000),
        "the wasted wake re-armed the sweep against the new retention"
    );
}

// ── pins ───────────────────────────────────────────────────────────────────

#[test]
fn a_pinned_document_is_attached_and_no_longer_idle() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    let document = loaded(&id, 1, 1);
    cache.insert(document.clone(), 0);
    assert_eq!(cache.sizes().idle, 1);

    let guard = cache.pin(&document);
    assert!(guard.is_held());
    assert!(cache.is_attached(decode_id::<DocumentIdKind>(1)));
    assert_eq!(
        cache.sizes().idle,
        0,
        "an observed document is not eligible for eviction"
    );
    assert_eq!(cache.sizes().attached, 1);
}

#[test]
fn an_attached_document_is_never_swept() {
    let mut cache = DocumentCache::new(1_000);
    let id = address_id("counter", 1);
    let document = loaded(&id, 1, 1);
    cache.insert(document.clone(), 0);
    let _guard = cache.pin(&document);

    assert!(
        cache.sweep(1_000_000).is_empty(),
        "an observed incarnation is pinned regardless of age"
    );
    assert!(cache.get(&id).is_some());
}

#[test]
fn a_use_of_an_attached_document_does_not_make_it_idle() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    let document = loaded(&id, 1, 1);
    cache.insert(document.clone(), 0);
    let _guard = cache.pin(&document);

    cache.note_use(&id, 10);
    assert_eq!(
        cache.sizes().idle,
        0,
        "a pinned document never enters the idle list"
    );
}

#[test]
fn unpinning_the_last_observer_makes_the_document_idle_again() {
    let mut cache = DocumentCache::new(1_000);
    let id = address_id("counter", 1);
    let document = loaded(&id, 1, 1);
    cache.insert(document.clone(), 0);
    let mut guard = cache.pin(&document);

    assert!(guard.release());
    assert!(!guard.release(), "release is idempotent");
    cache.unpin(decode_id::<DocumentIdKind>(1), &id, 500);

    assert!(!cache.is_attached(decode_id::<DocumentIdKind>(1)));
    assert_eq!(cache.sizes().idle, 1, "it is evictable again");
    assert_eq!(
        cache.sweep(1_499).len(),
        0,
        "the last observer left at 500, so half a retention is not enough"
    );
    assert_eq!(
        cache.sweep(1_500),
        vec![id],
        "the sweep re-armed from the moment the last observer left"
    );
}

#[test]
fn two_observers_need_two_releases() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    let document = loaded(&id, 1, 1);
    cache.insert(document.clone(), 0);
    let _first = cache.pin(&document);
    let _second = cache.pin(&document);
    let record = decode_id::<DocumentIdKind>(1);

    cache.unpin(record, &id, 10);
    assert!(cache.is_attached(record), "one observer is still watching");
    assert_eq!(cache.sizes().idle, 0);

    cache.unpin(record, &id, 20);
    assert!(!cache.is_attached(record));
    assert_eq!(cache.sizes().idle, 1);
}

#[test]
fn unpinning_an_unknown_incarnation_is_harmless() {
    let mut cache = cache();
    cache.unpin(decode_id::<DocumentIdKind>(99), "nothing", 0);
    assert_eq!(cache.sizes().attached, 0);
}

#[test]
fn unpinning_does_not_make_a_replaced_incarnation_idle() {
    // The guard pins an *incarnation*. If the cache slot was replaced by a new
    // incarnation in the meantime, releasing the old pin must not mark the new
    // one idle — it was never observed, but it is also not what was released.
    let mut cache = cache();
    let id = address_id("counter", 1);
    let old = loaded(&id, 1, 1);
    cache.insert(old.clone(), 0);
    let _guard = cache.pin(&old);

    cache.insert(loaded(&id, 2, 2), 0);
    cache.unpin(decode_id::<DocumentIdKind>(1), &id, 10);

    assert!(
        !cache.is_attached(decode_id::<DocumentIdKind>(2)),
        "the new incarnation was never pinned"
    );
    assert_eq!(
        cache.sizes().idle,
        1,
        "the replacement carries its own idle entry from insertion"
    );
}

// ── adoption bookkeeping ───────────────────────────────────────────────────

#[test]
fn adopting_a_delta_bumps_the_version_and_the_delta_count() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    cache.apply_adopted(&id, 2, false, Some(&value(2)));
    let document = cache.get(&id).expect("cached");
    assert_eq!(document.stored_version, 2);
    assert_eq!(document.value_version, 2);
    assert_eq!(
        document.value.get("count"),
        Some(&json!(2)),
        "the cache installs the committed value"
    );
    assert_eq!(
        document.deltas_since_base, 1,
        "a non-base commit adds one stored delta"
    );

    cache.apply_adopted(&id, 3, false, Some(&value(3)));
    assert_eq!(
        cache.get(&id).map(|d| d.deltas_since_base),
        Some(2),
        "deltas accumulate until a base is written"
    );
}

#[test]
fn adopting_a_base_resets_the_delta_count() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);
    cache.apply_adopted(&id, 2, false, Some(&value(2)));
    cache.apply_adopted(&id, 3, false, Some(&value(3)));

    cache.apply_adopted(&id, 4, true, Some(&value(4)));
    let document = cache.get(&id).expect("cached");
    assert_eq!(document.stored_version, 4);
    assert_eq!(
        document.value.get("count"),
        Some(&json!(4)),
        "a base installs its complete value"
    );
    assert_eq!(
        document.deltas_since_base, 0,
        "a base commit resets the count the definition uses to checkpoint"
    );
}

#[test]
fn adopting_an_unloaded_address_is_a_no_op() {
    let mut cache = cache();
    cache.apply_adopted("never-loaded", 9, true, Some(&value(1)));
    cache.mark_retired("never-loaded", Seq::from_raw(9));
    assert_eq!(cache.sizes().cached, 0);
}

#[test]
fn adopting_without_a_value_evicts_the_cached_address() {
    // A copy publishes no value, so the cached incarnation cannot be updated
    // in place; dropping it forces the next read to reload the new one.
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    cache.apply_adopted(&id, 2, true, None);
    assert!(cache.get(&id).is_none(), "the stale value is gone");
    assert_eq!(cache.sizes().cached, 0);
    assert_eq!(cache.sizes().idle, 0);
}

#[test]
fn marking_retired_stamps_the_cached_record() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    cache.mark_retired(&id, Seq::from_raw(17));
    assert_eq!(
        cache.get(&id).and_then(|d| d.record.retired_at),
        Some(Seq::from_raw(17)),
        "a retired incarnation is marked so a later access reloads instead of reviving"
    );
}

// ── clearing ───────────────────────────────────────────────────────────────

#[test]
fn clearing_drops_every_document_and_disarms_the_sweep() {
    let mut cache = cache();
    cache.insert(loaded(&address_id("counter", 1), 1, 1), 0);
    cache.insert(loaded(&address_id("counter", 2), 2, 2), 0);
    assert!(cache.sweep_armed());

    cache.clear();
    assert_eq!(
        cache.sizes(),
        DocumentCacheSizes {
            cached: 0,
            idle: 0,
            attached: 0,
            sweep_armed: false,
        },
        "a cleared cache reloads everything from storage on next access"
    );
}

// ── the pin guard ──────────────────────────────────────────────────────────

#[test]
fn dropping_a_guard_leaves_the_release_to_the_session() {
    let mut cache = cache();
    let id = address_id("counter", 1);
    let document = loaded(&id, 1, 1);
    cache.insert(document.clone(), 0);

    let guard = cache.pin(&document);
    drop(guard);
    // The guard cannot reach the cache (it is owned by the line), so dropping
    // it only stops *claiming* the pin; the session's detach path is what
    // calls `unpin`. A guard whose release was never applied leaves the
    // incarnation pinned until then.
    assert!(cache.is_attached(decode_id::<DocumentIdKind>(1)));
}

#[test]
fn a_guard_carries_the_incarnation_and_address_it_pinned() {
    let mut cache = cache();
    let id = address_id("pi.live", 4);
    let document = loaded(&id, 12, 1);
    cache.insert(document.clone(), 0);

    let guard: PinGuard = cache.pin(&document);
    assert_eq!(guard.record_id, decode_id::<DocumentIdKind>(12));
    assert_eq!(guard.address_id, id);
    assert!(guard.is_held());
}

// ── counters ───────────────────────────────────────────────────────────────

#[test]
fn the_sizes_hook_reports_all_four_counts() {
    let mut cache = DocumentCache::new(1_000);
    let pinned = loaded(&address_id("counter", 1), 1, 1);
    cache.insert(pinned.clone(), 0);
    let _guard = cache.pin(&pinned);
    cache.insert(loaded(&address_id("counter", 2), 2, 1), 0);

    let sizes = cache.sizes();
    assert_eq!(sizes.cached, 2);
    assert_eq!(sizes.idle, 1, "only the unpinned one is idle");
    assert_eq!(sizes.attached, 1);
    assert!(sizes.sweep_armed);
}

#[test]
fn the_cache_remembers_its_retention() {
    let cache = DocumentCache::new(4_242);
    assert_eq!(cache.retention_ms(), 4_242);
}

#[test]
fn a_value_is_a_plain_map_the_caller_owns() {
    // The cache holds an immutable committed value, detached from storage: a
    // caller mutating what it read must not reach the cache.
    let mut cache = cache();
    let id = address_id("counter", 1);
    cache.insert(loaded(&id, 1, 1), 0);

    let mut copy: Map<String, Value> = cache.get(&id).expect("cached").value.clone();
    copy.insert("count".to_owned(), json!(99));
    assert_eq!(
        cache.get(&id).and_then(|d| d.value.get("count")),
        Some(&json!(1)),
        "reads are detached copies, not views into the cache"
    );
}
