//! Sidecar rewrite guard: an offset that is not a retained line start must
//! leave the file untouched rather than truncate it.
//!
//! Moved out of `src/storage/jsonl.rs` — unit tests do not live in `src/`.

use pir_durable::storage::jsonl::{replace_sidecar_suffix, RECLAIM_SUFFIX};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_directory(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pir-jsonl-{tag}-{}-{stamp}", std::process::id()))
}

/// `KeepFrom` offsets always point at a retained line start, so anything
/// at or past EOF is a corrupted/raced state. Rewriting then would truncate
/// the sidecar to an empty file; the guard must leave it untouched instead.
#[tokio::test]
async fn replace_sidecar_suffix_preserves_the_file_when_offset_reaches_eof() {
    let dir = temp_directory("suffix-eof");
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("create fixture directory");
    let file = "task-20.jsonl";
    let original = b"{\"format\":1,\"type\":\"record\",\"seq\":1}\n\
                     {\"format\":1,\"type\":\"record\",\"seq\":2}\n";
    tokio::fs::write(dir.join(file), original)
        .await
        .expect("write sidecar");

    for offset in [original.len() as u64, original.len() as u64 + 4096] {
        replace_sidecar_suffix(&dir, false, file, offset).await;
        assert_eq!(
            tokio::fs::read(dir.join(file)).await.expect("read sidecar"),
            original,
            "an out-of-range offset ({offset}) must not truncate the sidecar"
        );
    }
    assert!(
        !dir.join(format!("{file}{RECLAIM_SUFFIX}")).exists(),
        "the guard must not leave a rewrite temporary behind"
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// The normal path still copies exactly the bytes from a valid line-start
/// offset to EOF.
#[tokio::test]
async fn replace_sidecar_suffix_keeps_the_retained_tail() {
    let dir = temp_directory("suffix-tail");
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("create fixture directory");
    let file = "task-20.jsonl";
    let first = b"{\"format\":1,\"type\":\"record\",\"seq\":1}\n";
    let second = b"{\"format\":1,\"type\":\"record\",\"seq\":2}\n";
    let original = [first.as_slice(), second.as_slice()].concat();
    tokio::fs::write(dir.join(file), &original)
        .await
        .expect("write sidecar");

    replace_sidecar_suffix(&dir, false, file, first.len() as u64).await;
    assert_eq!(
        tokio::fs::read(dir.join(file)).await.expect("read sidecar"),
        second,
        "the retained suffix must be copied verbatim"
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}
