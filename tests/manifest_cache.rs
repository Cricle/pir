//! Bounded, fingerprint-keyed package-manifest discovery cache.
//!
//! The cache must never serve a stale manifest (a changed length/mtime
//! invalidates the entry), must not reuse an entry for a different path, and
//! must stay bounded.

use std::path::PathBuf;
use std::sync::Arc;

use pir::extensions::loader::npm_resolve::ManifestCache;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-manifest-cache-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn write_manifest(&self, name: &str, source: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).expect("create package dir");
        let path = dir.join("package.json");
        std::fs::write(&path, source).expect("write manifest");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cache_reuses_until_the_fingerprint_changes() {
    let temp = TempDir::new("fingerprint");
    let path = temp.write_manifest("pkg", r#"{"name":"pkg","main":"a.js"}"#);
    let mut cache = ManifestCache::new(8);

    let first = cache.get_or_load(&path).expect("first load");
    assert_eq!(first["main"], serde_json::json!("a.js"));
    let second = cache.get_or_load(&path).expect("cached load");
    assert!(
        Arc::ptr_eq(&first, &second),
        "an unchanged manifest must be served from the cache"
    );
    assert_eq!(cache.len(), 1);

    // Rewrite with a different length: the fingerprint (len/mtime) changes
    // and the stale value must not survive.
    std::fs::write(&path, r#"{"name":"pkg","main":"b.js","extra":true}"#)
        .expect("rewrite manifest");
    let third = cache.get_or_load(&path).expect("reload after edit");
    assert_eq!(third["main"], serde_json::json!("b.js"));
    assert!(
        !Arc::ptr_eq(&first, &third),
        "an edited manifest must be re-read"
    );
}

#[test]
fn cache_is_keyed_by_path_and_bounded() {
    let temp = TempDir::new("bounded");
    let mut cache = ManifestCache::new(2);
    assert_eq!(cache.capacity(), 2);

    let one = temp.write_manifest("one", r#"{"name":"one"}"#);
    let two = temp.write_manifest("two", r#"{"name":"two"}"#);
    let three = temp.write_manifest("three", r#"{"name":"three"}"#);

    assert_eq!(
        cache.get_or_load(&one).expect("one")["name"],
        serde_json::json!("one")
    );
    assert_eq!(
        cache.get_or_load(&two).expect("two")["name"],
        serde_json::json!("two")
    );
    assert_eq!(
        cache.get_or_load(&three).expect("three")["name"],
        serde_json::json!("three")
    );
    assert_eq!(cache.len(), 2, "the cache never exceeds its bound");
    assert!(
        cache.capacity() == 2 && cache.len() <= cache.capacity(),
        "bounded by construction"
    );

    // A different path is a different key, never a false hit.
    assert_eq!(
        cache.get_or_load(&one).expect("one again")["name"],
        serde_json::json!("one")
    );

    cache.clear();
    assert!(cache.is_empty());
}

#[test]
fn missing_or_invalid_manifest_is_an_error_not_a_stale_hit() {
    let temp = TempDir::new("errors");
    let path = temp.write_manifest("pkg", r#"{"name":"ok"}"#);
    let mut cache = ManifestCache::new(4);
    cache.get_or_load(&path).expect("initial load");

    std::fs::write(&path, "{not json").expect("write invalid manifest");
    assert!(
        cache.get_or_load(&path).is_err(),
        "a broken manifest must error instead of returning the cached value"
    );

    std::fs::remove_file(&path).expect("remove manifest");
    assert!(cache.get_or_load(&path).is_err());
}
