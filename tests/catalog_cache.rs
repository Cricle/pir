//! Startup parse cache for the built-in provider catalog.
//!
//! Verifies the parse-once `OnceLock`/`Arc` cache in
//! `src/ai/providers/catalog.rs` (kept as a standalone test binary so it does
//! not touch the grouped `tests/g_*.rs` suites):
//!
//! 1. `load_all()` returns identical content on repeated calls (the cached
//!    clone is byte-identical to what re-parsing would produce).
//! 2. `load_all_shared()` hands out the *same* `Arc` on every call after the
//!    first — proving the embedded JSON is parsed at most once per process.

use std::sync::Arc;

use pir::ai::providers::catalog::{load_all, load_all_shared};

#[test]
fn load_all_twice_returns_identical_content() {
    let first = load_all().unwrap();
    let second = load_all().unwrap();

    // The embedded snapshot ships exactly the providers in `CATALOG_FILES`
    // (42 at this revision). Pinning the count and representative ids makes a
    // parse that silently drops a provider fail here instead of being masked
    // by the second call agreeing with the first.
    let ids: Vec<&str> = first.iter().map(|provider| provider.id.as_str()).collect();
    assert_eq!(ids.len(), 42, "catalog provider count: {ids:?}");
    assert_eq!(
        ids.first(),
        Some(&"amazon-bedrock"),
        "catalog order: {ids:?}"
    );
    assert_eq!(ids.last(), Some(&"zai-coding-cn"), "catalog order: {ids:?}");
    for expected in [
        "anthropic",
        "azure",
        "deepseek",
        "google",
        "openai",
        "openrouter",
        "xiaomi-token-plan-sgp",
    ] {
        assert!(
            ids.contains(&expected),
            "catalog must contain the {expected} provider: {ids:?}"
        );
    }
    let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate provider ids: {ids:?}");

    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.display_name, b.display_name);
        assert_eq!(a.base_url, b.base_url);
        assert_eq!(a.models, b.models);
    }
}

#[test]
fn load_all_matches_a_fresh_single_provider_parse() {
    let providers = load_all().unwrap();
    let anthropic = providers
        .iter()
        .find(|provider| provider.id == "anthropic")
        .expect("catalog must contain the anthropic provider");
    assert!(!anthropic.models.is_empty());
    assert!(anthropic
        .models
        .iter()
        .all(|model| model.provider == "anthropic"));
}

#[test]
fn shared_handle_is_parse_once() {
    let first = load_all_shared().unwrap();
    let second = load_all_shared().unwrap();
    assert_eq!(first.len(), second.len());
    assert!(
        Arc::ptr_eq(&first, &second),
        "the second call must return the cached Arc, not re-parse the catalog"
    );
}

/// `models-store.json` caches provider catalogs, validators and internal base
/// URLs. On Unix the temp file backing the atomic write is created 0600, so
/// both the first write and a later refresh replace the file with a private
/// mode even when the agent dir is shared.
#[cfg(unix)]
#[test]
fn models_store_write_back_is_private() {
    use std::os::unix::fs::PermissionsExt;

    use pir::ai::providers::ModelsStoreEntry;

    let dir = std::env::temp_dir().join(format!(
        "pir-models-store-mode-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let entry = ModelsStoreEntry {
        models: Vec::new(),
        last_modified: Some(1),
        checked_at: Some(1),
        etag: Some("\"one\"".to_string()),
    };
    pir::ai::providers::write_models_store_entry(&dir, "provider", entry.clone())
        .expect("first store write");
    let path = dir.join("models-store.json");
    let first = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(first, 0o600, "first models-store write must be private");

    let refreshed = ModelsStoreEntry {
        etag: Some("\"two\"".to_string()),
        ..entry
    };
    pir::ai::providers::write_models_store_entry(&dir, "provider", refreshed)
        .expect("refresh store write");
    let second = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(second, 0o600, "refresh write must stay private");

    let _ = std::fs::remove_dir_all(&dir);
}
