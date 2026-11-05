//! P0-3: file-level checkpoint/undo — new/modified/deleted/renamed files,
//! cross-turn restore, retention bounds and read-only refusal.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::checkpoints::{
    CheckpointMeta, CheckpointStore, DEFAULT_MAX_CHECKPOINTS, DEFAULT_MAX_CHECKPOINT_FILE_BYTES,
};

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-p0-checkpoints-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn session_file(dir: &Path) -> PathBuf {
    let path = dir.join("1234567890_test-session.jsonl");
    std::fs::write(&path, b"{\"type\":\"session\",\"version\":3}\n").expect("session header");
    path
}

/// The blobs currently present in `store` (temp names excluded by the hash
/// shape check; the store only writes SHA-256 names there).
fn blob_files(store: &CheckpointStore) -> Vec<PathBuf> {
    let mut blobs: Vec<PathBuf> = std::fs::read_dir(store.root().join("blobs"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.len() == 64)
                        && path.is_file()
                })
                .collect()
        })
        .unwrap_or_default();
    blobs.sort();
    blobs
}

fn tool_context(dir: &Path, session: &Path) -> pir::agent::tool_execution::ToolContext {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry, ToolSessionEnv};
    use pir::ai::provider::ProviderRegistry;
    use pir::ai::types::*;

    let model = Model {
        id: "fake-model".into(),
        name: "Fake".into(),
        api: "fake".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    };
    let mut ctx = ToolContext::new(dir);
    ctx.session_env = Some(Arc::new(ToolSessionEnv {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model,
        thinking: ThinkingLevel::Off,
        session_id: Some("test-session".to_owned()),
        session_file: Some(session.to_path_buf()),
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: None,
        agent_dir: Some(dir.to_path_buf()),
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    }));
    ctx
}

#[test]
fn new_file_is_removed_by_restore() {
    let dir = temp_dir("new");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let target = dir.join("created.txt");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"new content").expect("write");
    assert!(target.exists());
    let report = store.restore(&cp.id, false).expect("restore");
    assert_eq!(report.deleted, 1, "report: {report:?}");
    assert!(
        !target.exists(),
        "a file created after the snapshot must be removed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn modified_file_gets_its_prior_bytes_back() {
    let dir = temp_dir("modified");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let target = dir.join("note.txt");
    std::fs::write(&target, b"version one").expect("write v1");
    let cp = store
        .snapshot("edit", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"version two").expect("write v2");
    let report = store.restore(&cp.id, false).expect("restore");
    assert_eq!(report.restored, 1, "report: {report:?}");
    assert_eq!(std::fs::read(&target).expect("read"), b"version one");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deleted_file_is_recreated() {
    let dir = temp_dir("deleted");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let target = dir.join("victim.txt");
    std::fs::write(&target, b"keep me").expect("write");
    let cp = store
        .snapshot("bash", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::remove_file(&target).expect("delete");
    let report = store.restore(&cp.id, false).expect("restore");
    assert_eq!(report.restored, 1, "report: {report:?}");
    assert_eq!(std::fs::read(&target).expect("read"), b"keep me");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_is_undone_when_both_paths_are_snapshotted() {
    let dir = temp_dir("rename");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let source = dir.join("source.txt");
    let destination = dir.join("destination.txt");
    std::fs::write(&source, b"payload").expect("write");
    let cp = store
        .snapshot("bash", &[source.clone(), destination.clone()])
        .expect("snapshot");
    std::fs::rename(&source, &destination).expect("rename");
    let report = store.restore(&cp.id, false).expect("restore");
    assert_eq!(report.restored, 1, "report: {report:?}");
    assert_eq!(report.deleted, 1, "report: {report:?}");
    assert_eq!(std::fs::read(&source).expect("source"), b"payload");
    assert!(
        !destination.exists(),
        "the rename destination must be removed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cross_turn_restore_walks_back_to_each_checkpoint() {
    let dir = temp_dir("cross-turn");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let target = dir.join("evolving.txt");
    std::fs::write(&target, b"turn-0").expect("write");
    let first = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot 1");
    std::fs::write(&target, b"turn-1").expect("write");
    let second = store
        .snapshot("edit", std::slice::from_ref(&target))
        .expect("snapshot 2");
    std::fs::write(&target, b"turn-2").expect("write");

    // Newest first, both turns visible.
    let listed = store.list(0);
    assert!(listed.iter().any(|cp| cp.id == first.id));
    assert!(listed.iter().any(|cp| cp.id == second.id));

    store.restore(&first.id, false).expect("restore first");
    assert_eq!(std::fs::read(&target).expect("read"), b"turn-0");
    store.restore(&second.id, false).expect("restore second");
    assert_eq!(std::fs::read(&target).expect("read"), b"turn-1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dry_run_changes_nothing() {
    let dir = temp_dir("dry-run");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let target = dir.join("dry.txt");
    std::fs::write(&target, b"before").expect("write");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"after").expect("write");
    let report = store.restore(&cp.id, true).expect("dry run");
    assert!(report.dry_run);
    assert_eq!(report.restored, 1);
    assert_eq!(std::fs::read(&target).expect("read"), b"after");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retention_keeps_only_the_newest_checkpoints() {
    let dir = temp_dir("retention");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_max_checkpoints(2);
    let target = dir.join("retained.txt");
    std::fs::write(&target, b"0").expect("write");
    let oldest = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("cp1");
    let second = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("cp2");
    let third = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("cp3");
    let listed = store.list(0);
    assert_eq!(listed.len(), 2, "listed: {listed:?}");
    assert!(listed.iter().any(|cp| cp.id == second.id));
    assert!(listed.iter().any(|cp| cp.id == third.id));
    assert!(!listed.iter().any(|cp| cp.id == oldest.id));
    assert!(!store.root().join(&oldest.id).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn read_only_or_missing_session_is_refused() {
    let dir = temp_dir("read-only");
    let missing = dir.join("missing.jsonl");
    assert!(CheckpointStore::for_session_file(&missing).is_none());
    assert!(CheckpointStore::open(&missing).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn default_retention_is_documented() {
    assert_eq!(DEFAULT_MAX_CHECKPOINTS, 20);
}

#[test]
fn retention_gc_deletes_unreferenced_blobs_with_distinct_contents() {
    let dir = temp_dir("blob-gc");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_max_checkpoints(1);
    let target = dir.join("evolving.txt");
    // Identical content would deduplicate to one blob and hide the bug; four
    // distinct contents must leave exactly the retained checkpoint's blob.
    for content in ["alpha", "bravo", "charlie", "delta"] {
        std::fs::write(&target, content).expect("write");
        store
            .snapshot("write", std::slice::from_ref(&target))
            .expect("snapshot");
    }
    let blobs = blob_files(&store);
    assert_eq!(
        blobs.len(),
        1,
        "only the retained checkpoint's blob may survive: {blobs:?}"
    );
    let retained = store.list(0);
    assert_eq!(retained.len(), 1);
    let hash = retained[0].files[0].hash.as_deref().expect("retained hash");
    assert!(
        store.root().join("blobs").join(hash).is_file(),
        "the retained checkpoint's blob must not be collected"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restoring_the_oldest_retained_checkpoint_protects_its_blobs() {
    let dir = temp_dir("restore-oldest");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_max_checkpoints(2);
    let target = dir.join("state.txt");
    std::fs::write(&target, b"one").expect("write one");
    let first = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("cp1");
    std::fs::write(&target, b"two").expect("write two");
    let second = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("cp2");
    std::fs::write(&target, b"three").expect("write three");
    store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("cp3");
    assert!(
        !store.root().join(&first.id).exists(),
        "cp1 is already outside retention"
    );
    assert!(store.root().join(&second.id).exists());

    // The safety snapshot pushes `second` out of the two-checkpoint window;
    // its blob must survive until this restore has read it.
    let report = store.restore(&second.id, false).expect("restore");
    assert_eq!(report.restored, 1, "report: {report:?}");
    assert!(report.errors.is_empty(), "report: {report:?}");
    assert_eq!(std::fs::read(&target).expect("read"), b"two");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_meta_rewrites_never_observe_a_missing_meta() {
    let dir = temp_dir("atomic-meta");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session).expect("store");
    let meta_dir = store.root().join("cp-atomic");
    std::fs::create_dir_all(&meta_dir).expect("meta dir");
    let mut meta = CheckpointMeta {
        version: 1,
        id: "cp-atomic".to_owned(),
        created_at_ms: 0,
        tool: "test".to_owned(),
        files: Vec::new(),
    };
    pir::checkpoints::write_meta_for_tests(&meta_dir, &meta).expect("initial meta");

    let reader_store = store.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader = std::thread::spawn(move || {
        let mut reads = 0usize;
        while !reader_stop.load(Ordering::Relaxed) {
            let metas = reader_store.list(0);
            assert_eq!(
                metas.len(),
                1,
                "read_meta returned None during an atomic rewrite (read {reads})"
            );
            assert_eq!(metas[0].id, "cp-atomic");
            reads += 1;
            std::hint::spin_loop();
        }
        reads
    });
    for revision in 0..300 {
        meta.created_at_ms = revision;
        pir::checkpoints::write_meta_for_tests(&meta_dir, &meta).expect("rewrite meta");
    }
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().expect("reader thread");
    assert!(reads > 0, "reader never observed the meta");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oversized_file_fails_snapshot_without_publishing_a_checkpoint() {
    let dir = temp_dir("oversized");
    let session = session_file(&dir);
    let target = dir.join("big.txt");
    std::fs::write(&target, vec![b'x'; 1024]).expect("write");
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_max_checkpoint_file_bytes(64);
    let error = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect_err("oversized snapshot must fail");
    assert!(error.contains("exceeds the 64 byte"), "{error}");
    assert!(
        store.list(0).is_empty(),
        "a failed snapshot must not publish a partial checkpoint"
    );
    assert!(
        blob_files(&store).is_empty(),
        "no blob may be written for an oversized file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn oversized_checkpoint_target_does_not_block_the_mutation() {
    use pir::agent::tool_execution::AgentTool;
    use pir::coding_agent::tools::WriteTool;

    let dir = temp_dir("oversized-tool");
    let session = session_file(&dir);
    let target = dir.join("huge.bin");
    let file = std::fs::File::create(&target).expect("create huge");
    file.set_len(DEFAULT_MAX_CHECKPOINT_FILE_BYTES + 1)
        .expect("size huge");
    drop(file);

    let ctx = tool_context(&dir, &session);
    let result = WriteTool::new()
        .run(
            &serde_json::json!({"path": "huge.bin", "content": "replaced"}),
            &ctx,
        )
        .await;
    assert!(!result.is_error, "write failed: {}", result.content);
    assert_eq!(std::fs::read(&target).expect("read"), b"replaced");
    let store = CheckpointStore::for_session_file(&session).expect("store");
    assert!(
        store.list(0).is_empty(),
        "the oversized checkpoint must not be published"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Determinism: the snapshot hashes + copies 32 MiB and `sync_all`s the blob,
/// which takes far longer than the 1 ms interval. On a single-threaded runtime
/// the ticker can only advance if the snapshot runs on the blocking pool.
#[tokio::test(flavor = "current_thread")]
async fn ticker_progresses_while_snapshot_runs_on_the_blocking_pool() {
    use pir::agent::tool_execution::AgentTool;
    use pir::coding_agent::tools::WriteTool;

    let dir = temp_dir("ticker");
    let session = session_file(&dir);
    let target = dir.join("large.bin");
    let file = std::fs::File::create(&target).expect("create large");
    file.set_len(32 * 1024 * 1024).expect("size large");
    drop(file);

    let ticks = Arc::new(AtomicUsize::new(0));
    let ticker = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(1));
            loop {
                interval.tick().await;
                ticks.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    // Let the interval deliver its immediate first tick before measuring.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let baseline = ticks.load(Ordering::Relaxed);

    let ctx = tool_context(&dir, &session);
    let result = WriteTool::new()
        .run(
            &serde_json::json!({"path": "large.bin", "content": "small"}),
            &ctx,
        )
        .await;
    let during = ticks.load(Ordering::Relaxed).saturating_sub(baseline);
    ticker.abort();
    assert!(!result.is_error, "write failed: {}", result.content);
    assert!(
        during > 0,
        "the 1 ms ticker made no progress while the snapshot ran ({during} ticks)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn write_tool_snapshots_the_file_before_mutating_it() {
    use pir::agent::tool_execution::AgentTool;
    use pir::coding_agent::tools::WriteTool;

    let dir = temp_dir("write-hook");
    let session = session_file(&dir);
    let target = dir.join("hooked.txt");
    std::fs::write(&target, b"before-hook").expect("write");

    let ctx = tool_context(&dir, &session);
    let result = WriteTool::new()
        .run(
            &serde_json::json!({"path": "hooked.txt", "content": "after-hook"}),
            &ctx,
        )
        .await;
    assert!(!result.is_error, "write failed: {}", result.content);
    assert_eq!(std::fs::read(&target).expect("read"), b"after-hook");

    let store = CheckpointStore::for_session_file(&session).expect("store");
    let checkpoints = store.list(0);
    assert_eq!(
        checkpoints.len(),
        1,
        "one pre-write checkpoint: {checkpoints:?}"
    );
    let report = store.restore("latest", false).expect("restore");
    assert_eq!(report.restored, 1);
    assert_eq!(std::fs::read(&target).expect("read"), b"before-hook");
    let _ = std::fs::remove_dir_all(&dir);
}
