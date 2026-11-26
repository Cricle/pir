//! Session-search corpus caches: bounded budgets, FIFO eviction, lazy
//! normalized-view charging, and oversized-entry rejection.
//!
//! Moved out of `src/session_manager/search.rs` — unit tests do not live in
//! `src/`. The caches are private and their `with_limits` constructor is not
//! reachable from the production singletons, so the crate exposes the seams
//! through `search::test_api`, behind opaque wrappers.

use pir_core::session_manager::search::test_api::{
    allocation_key, content_key, corpus, corpus_cache, corpus_cache_max_bytes,
    corpus_cache_max_entries, descriptor_key, fingerprint_cache, fingerprint_cache_max_entries,
    lazy_cache, lazy_cache_max_bytes, lazy_cache_max_entries, normalized, normalized_is_built,
};

#[test]
fn search_corpus_cache_rejects_oversized_entry_and_enforces_budget() {
    let mut cache = corpus_cache(2, 12);

    // Each three-byte corpus is charged its lowercase bytes (1x).
    cache.insert(content_key(1, 1), corpus("abc"));
    cache.insert(content_key(2, 2), corpus("def"));
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.stats().bytes, 6);

    // A third entry evicts exactly the oldest one, keeping bytes at 6.
    cache.insert(content_key(3, 3), corpus("ghi"));
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(cache.stats().bytes, 6);
    assert!(!cache.contains(&content_key(1, 1)));
    assert!(cache.contains(&content_key(3, 3)));

    // Thirteen characters exceed 12: normalized for the match, never
    // cached, and the resident entries stay untouched.
    cache.insert(content_key(4, 4), corpus("abcdefghijklm"));
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.stats().bytes, 6);
    assert_eq!(cache.stats().rejected, 1);

    // A single entry exactly at the whole budget is allowed.
    cache.insert(content_key(5, 5), corpus("abcdefghijkl"));
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(cache.stats().bytes, 12);
}

#[test]
fn search_corpus_cache_charges_normalized_lazily_and_evicts_on_growth() {
    let mut cache = corpus_cache(4, 16);
    let first = corpus("aa  bb"); // lower: 6 B
    let second = corpus("cc   dd"); // lower: 7 B
    let first_key = content_key(1, 1);
    let second_key = content_key(2, 2);

    cache.insert(first_key.clone(), first);
    cache.insert(second_key.clone(), second.clone());
    assert_eq!(cache.stats().bytes, 13, "only lowercase is charged");
    assert!(!normalized_is_built(&second));

    // A phrase query builds the normalized view lazily; the growth is
    // reported back to the cache and forces out the oldest entry.
    assert_eq!(normalized(&second), "cc dd");
    cache.recharge(&second_key, &second);
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(
        cache.stats().bytes,
        7 + 5,
        "lowercase plus the actual normalized view"
    );
    assert!(!cache.contains(&first_key));
    assert!(cache.contains(&second_key));

    // Re-reporting an already charged entry is a no-op.
    cache.recharge(&second_key, &second);
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(cache.stats().bytes, 12);
    assert_eq!(cache.stats().evictions, 1);

    // An oversized normalized growth can evict the entry itself; the
    // caller still uses its Rc for the current match.
    let mut tiny = corpus_cache(4, 10);
    let huge = corpus("aa  bb");
    let huge_key = content_key(3, 3);
    tiny.insert(huge_key.clone(), huge.clone());
    assert_eq!(normalized(&huge), "aa bb");
    tiny.recharge(&huge_key, &huge);
    assert_eq!(tiny.stats().entries, 0);
    assert_eq!(tiny.stats().bytes, 0);
    assert_eq!(tiny.stats().evictions, 1);
}

#[test]
fn lazy_corpus_cache_rejects_oversized_entry_and_evicts_fifo() {
    let mut cache = lazy_cache(2, 10);

    cache.insert("k1", "12345");
    cache.insert("k2", "abcde");
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.stats().bytes, 10);

    cache.insert("k3", "vwxyz");
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(cache.stats().bytes, 10);
    assert!(!cache.contains("k1"));
    assert!(cache.contains("k3"));

    // A single transcript larger than the whole budget is not cached.
    cache.insert("k4", "0123456789X");
    assert_eq!(cache.stats().rejected, 1);
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.stats().bytes, 10);
}

#[test]
fn literal_fingerprint_cache_is_bounded_fifo_and_detects_reuse() {
    let mut cache = fingerprint_cache(2);
    let key_a = allocation_key(0x1000, 3);
    let key_b = allocation_key(0x2000, 3);
    let key_c = allocation_key(0x3000, 3);

    cache.insert(&key_a, 1, 11);
    cache.insert(&key_b, 2, 22);
    assert_eq!(cache.stats(), (2, 0));
    cache.insert(&key_c, 3, 33);
    assert_eq!(cache.stats(), (2, 1));
    assert_eq!(cache.fingerprint(&key_a), None);
    assert_eq!(cache.fingerprint(&key_c), Some(33));

    // Re-inserting an existing key replaces the value without growing.
    cache.insert(&key_c, 4, 44);
    assert_eq!(cache.stats(), (2, 1));
    assert_eq!(cache.fingerprint(&key_c), Some(44));
}

#[test]
fn default_cache_budgets_cover_b8_in_both_tiers() {
    // 1,000 x 100 KB ~= 95.4 MiB fits in each 128 MiB tier (lowercase is
    // charged 1x; the normalized view is only added by phrase queries).
    assert_eq!(lazy_cache_max_bytes(), 128 * 1024 * 1024);
    assert_eq!(corpus_cache_max_bytes(), 128 * 1024 * 1024);
    assert!(corpus_cache_max_bytes() >= 1_000 * 100 * 1024);
    assert_eq!(lazy_cache_max_entries(), 1024);
    assert!(corpus_cache_max_entries() >= 1_000);
    assert_eq!(fingerprint_cache_max_entries(), 4096);
}

#[test]
fn descriptor_and_content_cache_keys_are_distinct() {
    let content = content_key(7, 7);
    let descriptor = descriptor_key(7, 7);
    // Distinct keys: a cache holding one must not answer for the other.
    let mut cache = corpus_cache(4, 64);
    cache.insert(content.clone(), corpus("abc"));
    assert!(cache.contains(&content));
    assert!(!cache.contains(&descriptor));
}
