//! L9: the `/logout` and API-key dialog writes must not block the TUI on the
//! auth file lock (the bounded acquire retries for up to 30 s).
//!
//! The TUI now runs those writes off the render thread through the async lock
//! path (`FileAuthStorage::{store_async,delete_async}`). This test holds the
//! lock file and proves the async path yields (a timeout fires promptly while
//! the retry delay sleeps on the runtime), then releases the lock and checks
//! the on-disk result — the same semantics as the sync methods.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir::ai::auth::{FileAuthStorage, StoredCredential};
use pir::ai::types::ApiKeyCredential;

fn temp_storage() -> (PathBuf, FileAuthStorage) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-auth-async-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("auth.json");
    (dir, FileAuthStorage::new(path))
}

fn api_key(value: &str) -> StoredCredential {
    StoredCredential::ApiKey(ApiKeyCredential::new(Some(value.to_string())))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_async_waits_on_the_lock_without_blocking_and_completes_after_release() {
    let (dir, storage) = temp_storage();
    let lock_path = PathBuf::from(format!("{}.lock", storage.path().display()));

    storage
        .store_async("provider", api_key("first"))
        .await
        .expect("initial store");
    assert!(storage.read_stored("provider").expect("read").is_some());

    // Hold the cross-process lock as another writer would.
    std::fs::write(&lock_path, b"held").expect("lock file");

    // The async acquire sleeps between retries: a bounded wait must elapse
    // promptly instead of the call blocking the runtime for the 30 s acquire
    // window. (The sync `delete` would block this worker thread outright.)
    let started = std::time::Instant::now();
    let timed_out =
        tokio::time::timeout(Duration::from_millis(250), storage.delete_async("provider")).await;
    assert!(
        timed_out.is_err(),
        "delete_async must still be waiting while the lock is held"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the bounded wait returned promptly"
    );

    // Release the lock: the same call now succeeds and removes the entry.
    std::fs::remove_file(&lock_path).expect("release lock");
    storage
        .delete_async("provider")
        .await
        .expect("delete after release");
    assert!(
        storage.read_stored("provider").expect("read").is_none(),
        "the credential is removed"
    );

    // Store has the same async shape (the API-key dialog path).
    storage
        .store_async("provider-2", api_key("second"))
        .await
        .expect("store_async");
    assert!(matches!(
        storage.read_stored("provider-2").expect("read"),
        Some(StoredCredential::ApiKey(_))
    ));

    let _ = std::fs::remove_dir_all(&dir);
}
