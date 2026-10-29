//! Unit tests for provider configuration support migrated out of
//! `src/ai/providers/config_value.rs` and `src/ai/providers/models_store.rs`
//! inline `#[cfg(test)]` tests: config-value template resolution and the
//! models-store.json round trip.

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// src/ai/providers/config_value.rs
// ---------------------------------------------------------------------------

use pir::ai::providers::config_value::{
    get_config_value_env_var_name, get_config_value_env_var_names, is_command_config_value,
    is_config_value_configured, resolve_config_value, resolve_headers,
};

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

#[test]
fn templates_resolve_env_references() {
    let env = env(&[("TOKEN", "abc"), ("PREFIX", "x")]);
    assert_eq!(
        resolve_config_value("$TOKEN", Some(&env)).as_deref(),
        Some("abc")
    );
    assert_eq!(
        resolve_config_value("${TOKEN}", Some(&env)).as_deref(),
        Some("abc")
    );
    assert_eq!(
        resolve_config_value("$PREFIX-$TOKEN", Some(&env)).as_deref(),
        Some("x-abc")
    );
}

#[test]
fn escapes_and_literals() {
    let env = env(&[("TOKEN", "abc")]);
    assert_eq!(
        resolve_config_value("$$x$TOKEN", Some(&env)).as_deref(),
        Some("$xabc")
    );
    assert_eq!(resolve_config_value("$!", Some(&env)).as_deref(), Some("!"));
    assert_eq!(
        resolve_config_value("plain", Some(&env)).as_deref(),
        Some("plain")
    );
    // ${not an env name} stays verbatim.
    assert_eq!(
        resolve_config_value("${NOT A VAR}", Some(&env)).as_deref(),
        Some("${NOT A VAR}")
    );
}

#[test]
fn missing_env_returns_none() {
    assert_eq!(
        resolve_config_value("$PIR_DEFINITELY_UNSET_VAR_42", None),
        None
    );
}

#[test]
fn env_var_name_helpers() {
    assert_eq!(
        get_config_value_env_var_name("$ONLY"),
        Some("ONLY".to_string())
    );
    assert_eq!(get_config_value_env_var_name("a$B"), None);
    assert_eq!(
        get_config_value_env_var_names("a$B-$C"),
        vec!["B".to_string(), "C".to_string()]
    );
    assert!(is_command_config_value("!pass show x"));
    assert!(!is_command_config_value("$KEY"));
    assert!(is_config_value_configured(
        "$KEY",
        Some(&env(&[("KEY", "v")]))
    ));
    // The override map only supplies values; a key it lacks still falls back
    // to the process environment (deliberate upstream parity), so the
    // negative case must use a name the environment never carries.
    assert!(!is_config_value_configured(
        "$PIR_DEFINITELY_UNSET_VAR_42",
        Some(&env(&[]))
    ));
}

#[test]
fn headers_drop_unresolved_values() {
    let headers = BTreeMap::from([
        ("ok".to_string(), "$A".to_string()),
        // Same ambient-fallback hazard as above: `$B` would resolve if the
        // process environment happened to carry `B`.
        (
            "bad".to_string(),
            "$PIR_DEFINITELY_UNSET_VAR_42".to_string(),
        ),
    ]);
    let env = env(&[("A", "1")]);
    let resolved = resolve_headers(Some(&headers), Some(&env)).expect("headers");
    assert_eq!(resolved.get("ok").map(String::as_str), Some("1"));
    assert!(!resolved.contains_key("bad"));
}

// ---------------------------------------------------------------------------
// src/ai/providers/models_store.rs
// ---------------------------------------------------------------------------

use std::path::PathBuf;

use pir::ai::error::AiError;
use pir::ai::providers::models_store::{
    delete_models_store_entry, models_store_cache_stats, models_store_path,
    read_models_store_entry, write_models_store_entry, FileModelsStore, ModelsStore,
    ModelsStoreEntry,
};

fn temp_agent() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-models-store-tests-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn write_read_delete_round_trip() {
    let agent = temp_agent();
    let entry = ModelsStoreEntry {
        models: vec![serde_json::json!({ "id": "model", "api": "openai-completions" })],
        last_modified: Some(1234),
        checked_at: Some(5678),
        etag: Some("\"abc\"".to_string()),
    };
    write_models_store_entry(&agent, "openai", entry.clone()).expect("write");
    let read = read_models_store_entry(&agent, "openai").expect("entry");
    assert_eq!(read, entry);
    // Unknown providers read as None.
    assert!(read_models_store_entry(&agent, "other").is_none());
    delete_models_store_entry(&agent, "openai").expect("delete");
    assert!(read_models_store_entry(&agent, "openai").is_none());
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn repeated_reads_use_the_stamp_cache_instead_of_reparsing() {
    let agent = temp_agent();
    write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: vec![serde_json::json!({ "id": "model", "api": "openai-completions" })],
            ..ModelsStoreEntry::default()
        },
    )
    .expect("write");
    // The writer refreshes the cache with the bytes it just installed, so the
    // first read after a write is already a hit.
    let baseline = models_store_cache_stats(&agent);
    assert_eq!(
        baseline.parses, 0,
        "the write seeded the cache: {baseline:?}"
    );
    let entry = read_models_store_entry(&agent, "openai").expect("cached entry");
    assert_eq!(entry.models[0]["id"], "model");
    let after_first = models_store_cache_stats(&agent);
    assert_eq!(after_first.hits, baseline.hits + 1, "{after_first:?}");
    assert_eq!(
        after_first.parses, baseline.parses,
        "a per-prompt read must not re-parse the file: {after_first:?}"
    );
    assert!(read_models_store_entry(&agent, "openai").is_some());
    let after_second = models_store_cache_stats(&agent);
    assert_eq!(after_second.hits, after_first.hits + 1, "{after_second:?}");
    assert_eq!(after_second.parses, after_first.parses, "{after_second:?}");

    // An external rewrite changes the metadata stamp: exactly one re-parse,
    // then the new version is cached again.
    std::fs::write(
        models_store_path(&agent),
        serde_json::json!({
            "other": {
                "models": [{ "id": "external-model", "api": "openai-completions" }],
                "checkedAt": 9,
            }
        })
        .to_string(),
    )
    .expect("external rewrite");
    let before_rewrite_read = models_store_cache_stats(&agent);
    let entry = read_models_store_entry(&agent, "other").expect("external entry");
    assert_eq!(entry.models[0]["id"], "external-model");
    let after_rewrite_read = models_store_cache_stats(&agent);
    assert_eq!(
        after_rewrite_read.parses,
        before_rewrite_read.parses + 1,
        "the changed stamp must trigger one parse: {after_rewrite_read:?}"
    );
    assert_eq!(
        after_rewrite_read.hits, before_rewrite_read.hits,
        "a parse is not a cache hit"
    );
    assert!(read_models_store_entry(&agent, "other").is_some());
    let after_recache = models_store_cache_stats(&agent);
    assert_eq!(after_recache.hits, after_rewrite_read.hits + 1);
    assert_eq!(
        after_recache.parses, after_rewrite_read.parses,
        "the re-parsed version is cached: {after_recache:?}"
    );

    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn cached_parse_is_invalidated_by_a_corrupt_rewrite() {
    let agent = temp_agent();
    write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: vec![serde_json::json!({ "id": "model" })],
            ..ModelsStoreEntry::default()
        },
    )
    .expect("write");
    assert!(read_models_store_entry(&agent, "openai").is_some());

    // A cached healthy copy must not mask a later corrupt rewrite: the stamp
    // miss re-reads the file, the parse fails, reads report None and writes
    // keep refusing to clobber the recoverable bytes.
    let corrupt = b"{ this is definitely not json".to_vec();
    std::fs::write(models_store_path(&agent), &corrupt).expect("corrupt rewrite");
    assert!(
        read_models_store_entry(&agent, "openai").is_none(),
        "the corrupt rewrite must invalidate the cached parse"
    );
    assert!(
        write_models_store_entry(
            &agent,
            "openai",
            ModelsStoreEntry {
                models: Vec::new(),
                ..ModelsStoreEntry::default()
            },
        )
        .is_err(),
        "the corrupt file must still block rewrites"
    );
    assert_eq!(
        std::fs::read(models_store_path(&agent)).expect("read back"),
        corrupt
    );

    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn cached_catalog_is_invalidated_when_the_file_disappears() {
    let agent = temp_agent();
    write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: vec![serde_json::json!({ "id": "model" })],
            ..ModelsStoreEntry::default()
        },
    )
    .expect("write");
    assert!(read_models_store_entry(&agent, "openai").is_some());

    std::fs::remove_file(models_store_path(&agent)).expect("remove store");
    assert!(
        read_models_store_entry(&agent, "openai").is_none(),
        "a deleted file must not be served from the cache"
    );
    // A later write recreates the store and is immediately visible.
    write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: vec![serde_json::json!({ "id": "recreated" })],
            ..ModelsStoreEntry::default()
        },
    )
    .expect("rewrite");
    let entry = read_models_store_entry(&agent, "openai").expect("entry after recreate");
    assert_eq!(entry.models[0]["id"], "recreated");

    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn camel_case_wire_shape_matches_upstream() {
    let agent = temp_agent();
    write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: Vec::new(),
            last_modified: Some(1),
            checked_at: Some(2),
            etag: Some("\"e\"".to_string()),
        },
    )
    .expect("write");
    let text = std::fs::read_to_string(models_store_path(&agent)).expect("read");
    assert!(text.contains("\"lastModified\": 1"));
    assert!(text.contains("\"checkedAt\": 2"));
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn corrupt_store_is_reported_and_never_overwritten() {
    let agent = temp_agent();
    let path = models_store_path(&agent);
    let corrupt = b"{ this is definitely not json".to_vec();
    std::fs::write(&path, &corrupt).expect("seed corrupt file");

    // Reads stay best-effort: a corrupt cache must not take the session down.
    assert!(read_models_store_entry(&agent, "openai").is_none());

    // A write must refuse to clobber the (possibly recoverable) old file
    // instead of treating the parse failure as an empty store.
    let error = write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: vec![serde_json::json!({ "id": "model" })],
            ..ModelsStoreEntry::default()
        },
    )
    .expect_err("write over a corrupt store must fail");
    assert!(error.contains("cannot parse"), "{error}");
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        corrupt,
        "the corrupt file must be preserved byte-for-byte"
    );

    // Deleting is also a rewrite: it must not erase the old bytes.
    assert!(delete_models_store_entry(&agent, "openai").is_err());
    assert_eq!(std::fs::read(&path).expect("read back"), corrupt);

    // No temporary file is left behind by the refused writes.
    let leftovers: Vec<String> = std::fs::read_dir(&agent)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");

    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn file_models_store_failures_are_storage_errors_not_authentication() {
    let agent = temp_agent();
    // A parent that is a FILE cannot hold models-store.json: the write fails,
    // and must surface as a storage failure instead of "authentication
    // failure: cannot write ..." (the old mapping).
    let not_a_dir = agent.join("not-a-directory");
    std::fs::write(&not_a_dir, b"x").expect("seed file");
    let error = FileModelsStore::new(&not_a_dir)
        .write(
            "openai",
            ModelsStoreEntry {
                models: vec![serde_json::json!({ "id": "model" })],
                ..ModelsStoreEntry::default()
            },
        )
        .expect_err("a write under a file path must fail");
    assert!(
        matches!(error, AiError::ModelsStore(_)),
        "the failure is a models-store error, got {error:?}"
    );
    assert!(
        error.to_string().starts_with("models store error:"),
        "the message names the store, got {error}"
    );
    assert!(
        !error.to_string().contains("authentication"),
        "a failed catalog write must not claim an auth problem: {error}"
    );
    assert_eq!(
        error.turn_retry_class(),
        pir::ai::error::TurnRetryClass::NonRetryable
    );

    // A corrupt store blocks deletes too (a delete is a rewrite); the same
    // storage class covers it.
    std::fs::write(models_store_path(&agent), b"{ not json").expect("seed corrupt file");
    let error = FileModelsStore::new(&agent)
        .delete("openai")
        .expect_err("a delete over a corrupt store must fail");
    assert!(
        matches!(error, AiError::ModelsStore(_)),
        "the delete failure is a models-store error, got {error:?}"
    );

    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn empty_store_file_is_treated_as_empty_and_repaired_by_the_next_write() {
    let agent = temp_agent();
    std::fs::write(models_store_path(&agent), b"").expect("seed empty file");
    // Nothing to lose: a zero-length file reads as an empty store rather than
    // a corruption that would block every future refresh.
    assert!(read_models_store_entry(&agent, "openai").is_none());
    write_models_store_entry(
        &agent,
        "openai",
        ModelsStoreEntry {
            models: vec![serde_json::json!({ "id": "model" })],
            ..ModelsStoreEntry::default()
        },
    )
    .expect("write repairs the empty file");
    assert!(read_models_store_entry(&agent, "openai").is_some());
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn entry_wire_round_trip_preserves_metadata() {
    let entry = ModelsStoreEntry {
        models: vec![serde_json::json!({ "id": "model", "provider": "openai" })],
        last_modified: Some(11),
        checked_at: Some(22),
        etag: Some("\"etag\"".to_string()),
    };
    let wire = entry.to_wire();
    assert_eq!(wire["lastModified"], serde_json::json!(11));
    assert_eq!(wire["checkedAt"], serde_json::json!(22));
    assert_eq!(wire["etag"], serde_json::json!("\"etag\""));
    assert_eq!(ModelsStoreEntry::from_wire(&wire), Some(entry));
    // Metadata-only publications (no models key) decode with an empty list.
    assert_eq!(
        ModelsStoreEntry::from_wire(&serde_json::json!({ "checkedAt": 5 })),
        Some(ModelsStoreEntry {
            models: Vec::new(),
            checked_at: Some(5),
            ..ModelsStoreEntry::default()
        })
    );
    assert!(ModelsStoreEntry::from_wire(&serde_json::json!(["not", "an", "object"])).is_none());
    assert!(
        ModelsStoreEntry::from_wire(&serde_json::json!({ "models": "not-an-array" })).is_none(),
        "a malformed models field must not decode as an empty catalog"
    );
}

#[test]
fn concurrent_writes_keep_every_entry_and_readers_never_see_partial_json() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let agent = Arc::new(temp_agent());
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let agent = Arc::clone(&agent);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let path = models_store_path(&agent);
            let mut reads = 0usize;
            while !stop.load(Ordering::SeqCst) {
                match std::fs::read(&path) {
                    Ok(bytes) if !bytes.is_empty() => {
                        serde_json::from_slice::<serde_json::Value>(&bytes)
                            .unwrap_or_else(|error| panic!("partial write observed: {error}"));
                        reads += 1;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("read failed: {error}"),
                }
            }
            reads
        })
    };

    let writers: Vec<_> = (0..4)
        .map(|thread| {
            let agent = Arc::clone(&agent);
            std::thread::spawn(move || {
                for index in 0..12 {
                    let provider = format!("provider-{thread}-{index}");
                    write_models_store_entry(
                        &agent,
                        &provider,
                        ModelsStoreEntry {
                            models: vec![serde_json::json!({
                                "id": provider,
                                "padding": "x".repeat(4096),
                            })],
                            checked_at: Some(thread * 100 + index),
                            ..ModelsStoreEntry::default()
                        },
                    )
                    .expect("concurrent write");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer join");
    }
    stop.store(true, Ordering::SeqCst);
    let reads = reader.join().expect("reader join");
    assert!(reads > 0, "the reader never observed the store");

    let store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(models_store_path(&agent)).expect("final read"))
            .expect("final store parses");
    for thread in 0..4 {
        for index in 0..12 {
            let provider = format!("provider-{thread}-{index}");
            assert!(
                store.get(&provider).is_some(),
                "{provider} was lost by a concurrent writer"
            );
        }
    }
    // Every write replaced the file atomically; no temp files may remain.
    let leftovers: Vec<String> = std::fs::read_dir(&*agent)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");

    let _ = std::fs::remove_dir_all(&*agent);
}
