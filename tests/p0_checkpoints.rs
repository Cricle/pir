//! P0-3: file-level checkpoint/undo — new/modified/deleted/renamed files,
//! cross-turn restore, retention bounds and read-only refusal.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pir::checkpoints::{
    CheckpointFile, CheckpointMeta, CheckpointStore, DEFAULT_MAX_CHECKPOINTS,
    DEFAULT_MAX_CHECKPOINT_FILE_BYTES, DEFAULT_ORPHAN_BLOB_GRACE,
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

/// A session whose header records `cwd`, the workspace restore containment is
/// derived from in production.
fn session_file_with_cwd(dir: &Path, cwd: &Path) -> PathBuf {
    let path = dir.join("1234567890_test-session.jsonl");
    let header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": "test-session",
        "timestamp": "2026-01-01T00:00:00Z",
        "cwd": cwd.to_string_lossy(),
    });
    std::fs::write(&path, format!("{header}\n")).expect("session header");
    path
}

/// The filesystem root for the host (the V2a escape root).
fn filesystem_root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\")
    } else {
        PathBuf::from("/")
    }
}

/// A session transcript stored under `<agent_dir>/sessions`, the only place
/// whose header `cwd` restore trusts without an explicit `--root`.
fn trusted_session_file(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let dir = agent_dir.join("sessions").join("trusted-project");
    std::fs::create_dir_all(&dir).expect("trusted sessions dir");
    let path = dir.join("1234567890_trusted-session.jsonl");
    let header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": "trusted-session",
        "timestamp": "2026-01-01T00:00:00Z",
        "cwd": cwd.to_string_lossy(),
    });
    std::fs::write(&path, format!("{header}\n")).expect("session header");
    path
}

fn write_forged_meta(store: &CheckpointStore, id: &str, files: Vec<CheckpointFile>) {
    let meta = CheckpointMeta {
        version: 1,
        id: id.to_owned(),
        created_at_ms: 0,
        tool: "forged".to_owned(),
        files,
    };
    pir::checkpoints::write_meta_for_tests(&store.root().join(id), &meta).expect("forged meta");
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
    tool_context_with_mode(
        dir,
        session,
        pir::agent::read_ledger::ReadBeforeEditMode::Enforce,
    )
}

/// The checkpoint tests mutate pre-existing files to exercise snapshotting, not
/// the read-before-edit gate. Binary/oversized fixtures cannot be read through
/// the read tool at all (binary detection / output budget), so those cases run
/// with an ungated ledger; text fixtures read their target first instead.
fn tool_context_ungated(dir: &Path, session: &Path) -> pir::agent::tool_execution::ToolContext {
    tool_context_with_mode(
        dir,
        session,
        pir::agent::read_ledger::ReadBeforeEditMode::Off,
    )
}

fn tool_context_with_mode(
    dir: &Path,
    session: &Path,
    mode: pir::agent::read_ledger::ReadBeforeEditMode,
) -> pir::agent::tool_execution::ToolContext {
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
        read_ledger: Arc::new(pir::agent::read_ledger::ReadLedger::new(mode)),
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

/// Seed the read-before-edit ledger for a text fixture the checkpoint test is
/// about to mutate.
async fn read_target(ctx: &pir::agent::tool_execution::ToolContext, path: &str) {
    use pir::agent::tool_execution::AgentTool;
    let result = pir::coding_agent::tools::ReadTool::new()
        .run(&serde_json::json!({ "path": path }), ctx)
        .await;
    assert!(!result.is_error, "setup read failed: {}", result.content);
}

#[test]
fn new_file_is_removed_by_restore() {
    let dir = temp_dir("new");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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

/// Regression (F2c): restore has no implicit root, not even for dry runs and
/// not even when a checkpoint exists. The exact error names the missing flag.
#[test]
fn restore_requires_explicit_root() {
    let dir = temp_dir("requires-root");
    let session = session_file(&dir);
    let store = CheckpointStore::open(&session).expect("store");
    let target = dir.join("note.txt");
    std::fs::write(&target, b"value").expect("target");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    for dry_run in [true, false] {
        let error = store
            .restore(&cp.id, dry_run)
            .expect_err("restore without --root must be refused");
        assert_eq!(
            error, "restore requires --root <dir>: pass the workspace directory to restore into",
            "dry_run={dry_run}"
        );
    }
    assert_eq!(std::fs::read(&target).expect("read"), b"value");
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
        .with_max_checkpoints(1)
        .with_orphan_blob_grace(Duration::ZERO);
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
        .with_max_checkpoints(2)
        .with_workspace_root(dir.clone());
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

    let ctx = tool_context_ungated(&dir, &session);
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

    let ctx = tool_context_ungated(&dir, &session);
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
    read_target(&ctx, "hooked.txt").await;
    let result = WriteTool::new()
        .run(
            &serde_json::json!({"path": "hooked.txt", "content": "after-hook"}),
            &ctx,
        )
        .await;
    assert!(!result.is_error, "write failed: {}", result.content);
    assert_eq!(std::fs::read(&target).expect("read"), b"after-hook");

    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
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

/// Regression (A-01): the edit tool used to snapshot before reading and
/// validating the file, so a rejected edit still published a `cp-*` and
/// consumed one of the 20 retention slots. A failed edit must leave the
/// checkpoint store untouched.
#[tokio::test]
async fn invalid_edit_does_not_publish_a_checkpoint() {
    use pir::agent::tool_execution::AgentTool;
    use pir::coding_agent::tools::EditTool;

    let dir = temp_dir("invalid-edit");
    let session = session_file(&dir);
    let target = dir.join("note.txt");
    let original = b"original content\n";
    std::fs::write(&target, original).expect("write");

    let ctx = tool_context(&dir, &session);
    read_target(&ctx, "note.txt").await;
    let result = EditTool::new()
        .run(
            &serde_json::json!({
                "path": "note.txt",
                "edits": [{"oldText": "text that is not present", "newText": "replacement"}]
            }),
            &ctx,
        )
        .await;
    assert!(
        result.is_error,
        "the invalid edit must fail: {}",
        result.content
    );
    assert_eq!(
        std::fs::read(&target).expect("read"),
        original,
        "a rejected edit must not modify the file"
    );

    let store = CheckpointStore::for_session_file(&session).expect("store");
    assert!(
        store.list(0).is_empty(),
        "a rejected edit must not publish a checkpoint"
    );
    assert!(
        blob_files(&store).is_empty(),
        "a rejected edit must not write a checkpoint blob"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A successful edit snapshots the exact bytes read for validation (BOM and
/// CRLF included), not a re-read or a normalized rendering. Restoring the
/// checkpoint therefore returns the byte-identical pre-edit file.
#[tokio::test]
async fn successful_edit_checkpoints_the_exact_raw_pre_edit_bytes() {
    use pir::agent::tool_execution::AgentTool;
    use pir::coding_agent::tools::EditTool;

    let dir = temp_dir("edit-raw-bytes");
    let session = session_file(&dir);
    let target = dir.join("bom-crlf.txt");
    let original = b"\xEF\xBB\xBFalpha\r\nbeta\r\n";
    std::fs::write(&target, original).expect("write");

    let ctx = tool_context(&dir, &session);
    read_target(&ctx, "bom-crlf.txt").await;
    let result = EditTool::new()
        .run(
            &serde_json::json!({
                "path": "bom-crlf.txt",
                "edits": [{"oldText": "beta", "newText": "BETA"}]
            }),
            &ctx,
        )
        .await;
    assert!(!result.is_error, "edit failed: {}", result.content);
    assert_eq!(
        std::fs::read(&target).expect("read"),
        b"\xEF\xBB\xBFalpha\r\nBETA\r\n"
    );

    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_workspace_root(dir.clone());
    let checkpoints = store.list(0);
    assert_eq!(
        checkpoints.len(),
        1,
        "one pre-edit checkpoint: {checkpoints:?}"
    );
    assert_eq!(checkpoints[0].files.len(), 1);
    assert_eq!(checkpoints[0].files[0].bytes, original.len() as u64);

    let report = store.restore("latest", false).expect("restore");
    assert_eq!(report.restored, 1, "report: {report:?}");
    assert_eq!(
        std::fs::read(&target).expect("read"),
        original,
        "the checkpoint must hold the exact pre-edit bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (A-02): GC must not delete a blob another process wrote but has
/// not published yet. Fresh orphans survive; only orphans older than the
/// grace period are collected.
#[test]
fn orphan_blob_gc_keeps_fresh_blobs_and_collects_aged_ones() {
    let dir = temp_dir("blob-grace");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_max_checkpoints(1);
    let target = dir.join("state.txt");
    std::fs::write(&target, b"one").expect("write one");
    let first = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("first snapshot");
    let old_blob = store
        .root()
        .join("blobs")
        .join(first.files[0].hash.as_deref().expect("hash"));

    // This snapshot prunes `first`; its blob is an orphan now, but young.
    std::fs::write(&target, b"two").expect("write two");
    store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("second snapshot");
    assert!(
        old_blob.is_file(),
        "a fresh orphan blob must survive the cross-process grace period"
    );

    // Age the orphan past the grace window, then trigger another prune.
    let aged = SystemTime::now()
        .checked_sub(DEFAULT_ORPHAN_BLOB_GRACE + Duration::from_secs(60))
        .expect("aged timestamp");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&old_blob)
        .expect("open blob");
    file.set_times(std::fs::FileTimes::new().set_modified(aged))
        .expect("age blob");
    drop(file);
    std::fs::write(&target, b"three").expect("write three");
    store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("third snapshot");
    assert!(
        !old_blob.exists(),
        "an orphan older than the grace period must be collected"
    );
    let retained = store.list(0);
    assert_eq!(retained.len(), 1);
    let hash = retained[0].files[0].hash.as_deref().expect("retained hash");
    assert!(
        store.root().join("blobs").join(hash).is_file(),
        "the retained checkpoint's blob must survive"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (D-A-01): when the restore-preimage (safety) snapshot fails, the
/// restore must refuse before writing anything. Otherwise the current content
/// has no undo copy.
#[test]
fn restore_refuses_when_the_safety_snapshot_fails() {
    let dir = temp_dir("safety-fail");
    let session = session_file(&dir);
    let store = CheckpointStore::for_session_file(&session)
        .expect("store")
        .with_max_checkpoint_file_bytes(64)
        .with_workspace_root(dir.clone());
    let small = dir.join("small.txt");
    let large = dir.join("large.txt");
    std::fs::write(&small, b"small-original").expect("write small");
    std::fs::write(&large, b"large-original").expect("write large");
    let cp = store
        .snapshot("write", &[small.clone(), large.clone()])
        .expect("checkpoint");
    std::fs::write(&small, b"small-current").expect("mutate small");
    std::fs::write(&large, vec![b'x'; 128]).expect("grow large");

    let error = store
        .restore(&cp.id, false)
        .expect_err("restore must refuse without a safety snapshot");
    assert!(
        error.contains("safety checkpoint"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(&small).expect("read small"),
        b"small-current",
        "no target may be modified when the safety snapshot fails"
    );
    assert_eq!(
        std::fs::read(&large).expect("read large").len(),
        128,
        "no target may be modified when the safety snapshot fails"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (D-A-02): a checkpoint meta is untrusted input. An absolute path
/// outside the session header's cwd must be refused, not written.
#[test]
fn restore_refuses_meta_paths_outside_the_session_workspace() {
    let dir = temp_dir("path-escape");
    let workspace = dir.join("workspace");
    let outside = dir.join("outside.txt");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(&outside, b"outside-original").expect("outside file");
    let session = session_file_with_cwd(&dir, &workspace);
    // The explicit root is the operator's trust anchor; the untrusted session
    // header must not be the only source (V2a).
    let store = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(workspace.clone());
    // Simulate a forged sidecar: a legitimate snapshot whose recorded path is
    // outside the workspace the session was confined to.
    let cp = store
        .snapshot("write", std::slice::from_ref(&outside))
        .expect("snapshot");
    std::fs::write(&outside, b"outside-mutated").expect("mutate outside");
    let report = store.restore(&cp.id, false).expect("restore report");
    assert_eq!(report.restored, 0, "report: {report:?}");
    assert_eq!(report.deleted, 0, "report: {report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("outside the workspace")),
        "report: {report:?}"
    );
    assert_eq!(
        std::fs::read(&outside).expect("read outside"),
        b"outside-mutated",
        "an out-of-workspace target must not be touched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (D-A-02): `..` components and non-regular targets are refused
/// even when the lexical path starts inside the workspace.
#[test]
fn restore_refuses_parent_components_and_non_regular_targets() {
    let dir = temp_dir("path-shape");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outside = dir.join("outside.txt");
    std::fs::write(&outside, b"outside-original").expect("outside file");
    let session = session_file_with_cwd(&dir, &workspace);
    let store = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(workspace.clone());

    let escaped = workspace.join("..").join("outside.txt");
    write_forged_meta(
        &store,
        "cp-parent-dir",
        vec![CheckpointFile {
            path: escaped.to_string_lossy().into_owned(),
            existed: false,
            hash: None,
            bytes: 0,
        }],
    );
    let report = store
        .restore("cp-parent-dir", false)
        .expect("restore report");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("outside the workspace")),
        "report: {report:?}"
    );
    assert_eq!(
        std::fs::read(&outside).expect("read outside"),
        b"outside-original",
        "a `..` escape must not delete anything"
    );

    let directory_target = workspace.join("a-directory");
    std::fs::create_dir_all(&directory_target).expect("directory target");
    write_forged_meta(
        &store,
        "cp-directory",
        vec![CheckpointFile {
            path: directory_target.to_string_lossy().into_owned(),
            existed: false,
            hash: None,
            bytes: 0,
        }],
    );
    let report = store
        .restore("cp-directory", false)
        .expect("restore report");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("not a regular file")),
        "report: {report:?}"
    );
    assert!(directory_target.is_dir(), "the directory must survive");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (D-A-02): a symlink target inside the workspace must be refused
/// rather than followed (which would let a forged meta write/delete outside).
#[cfg(unix)]
#[test]
fn restore_refuses_symlink_targets() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("symlink-target");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outside = dir.join("outside.txt");
    std::fs::write(&outside, b"outside-original").expect("outside file");
    let link = workspace.join("link.txt");
    symlink(&outside, &link).expect("symlink");
    let session = session_file_with_cwd(&dir, &workspace);
    let store = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(workspace.clone());
    write_forged_meta(
        &store,
        "cp-symlink",
        vec![CheckpointFile {
            path: link.to_string_lossy().into_owned(),
            existed: false,
            hash: None,
            bytes: 0,
        }],
    );
    let report = store.restore("cp-symlink", false).expect("restore report");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("symbolic link")),
        "report: {report:?}"
    );
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "the symlink itself must not be removed"
    );
    assert_eq!(
        std::fs::read(&outside).expect("read outside"),
        b"outside-original"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c): even a session inside the agent's trusted sessions root
/// must not use its header `cwd` as the restore root. Without an explicit
/// `--root`, restore is refused before any target is touched.
#[test]
fn trusted_session_without_explicit_root_is_refused() {
    let dir = temp_dir("trusted-no-root");
    let agent_dir = dir.join("agent");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let session = trusted_session_file(&agent_dir, &workspace);
    let target = workspace.join("victim.txt");
    std::fs::write(&target, b"original").expect("target");
    let store = CheckpointStore::open(&session).expect("store");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"mutated").expect("mutate");
    let error = store
        .restore(&cp.id, false)
        .expect_err("restore without --root must be refused");
    assert!(
        error.contains("restore requires --root <dir>"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(&target).expect("read target"),
        b"mutated",
        "a header cwd must not authorize any write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c/R1): a forged session header `cwd` pointing at a harmless
/// non-root directory (the user's HOME in the report) must not authorize
/// restore even when the transcript sits under `<agent_dir>/sessions`. With an
/// explicit `--root`, a forged meta target outside that root is still refused
/// by containment.
#[test]
fn malicious_header_cwd_home_cannot_escape_trusted_session() {
    let dir = temp_dir("header-home");
    let agent_dir = dir.join("agent");
    let workspace = dir.join("workspace");
    let home = dir.join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let victim = home.join("important.txt");
    std::fs::write(&victim, b"original-secret").expect("victim");
    let session = trusted_session_file(&agent_dir, &home);
    let store = CheckpointStore::open(&session).expect("store");
    let cp = store
        .snapshot("write", std::slice::from_ref(&victim))
        .expect("snapshot");
    std::fs::write(&victim, b"mutated-by-attacker").expect("mutate");

    let error = store
        .restore(&cp.id, false)
        .expect_err("the header cwd must not become the root");
    assert!(
        error.contains("restore requires --root <dir>"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("header cwd") && error.contains("untrusted"),
        "the header cwd must only appear as a diagnostic: {error}"
    );
    assert_eq!(
        std::fs::read(&victim).expect("read victim"),
        b"mutated-by-attacker",
        "the malicious header cwd must not authorize any write"
    );

    // Explicit root: the forged target (HOME) is outside it, so containment
    // still refuses the restore.
    let confined = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(workspace);
    let report = confined.restore(&cp.id, false).expect("restore report");
    assert_eq!(report.restored, 0, "report: {report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("outside the workspace")),
        "report: {report:?}"
    );
    assert_eq!(
        std::fs::read(&victim).expect("read victim"),
        b"mutated-by-attacker",
        "a target outside the explicit root must not be touched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c): a filesystem-root header cwd is likewise diagnostic
/// input only; it never becomes a containment root.
#[test]
fn malicious_header_cwd_filesystem_root_cannot_escape_trusted_session() {
    let dir = temp_dir("header-fs-root");
    let agent_dir = dir.join("agent");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let session = trusted_session_file(&agent_dir, &filesystem_root());
    let store = CheckpointStore::open(&session).expect("store");
    let target = workspace.join("victim.txt");
    std::fs::write(&target, b"original").expect("target");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"mutated").expect("mutate");
    let error = store
        .restore(&cp.id, false)
        .expect_err("a filesystem root header cwd must be refused");
    assert!(
        error.contains("restore requires --root <dir>"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(&target).expect("read target"),
        b"mutated",
        "the malicious root must not authorize any write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c): a session outside `<agent_dir>/sessions` is rejected the
/// same way as any other session without `--root`; the error tells the
/// operator to pass `--root`.
#[test]
fn untrusted_session_without_explicit_root_is_refused() {
    let dir = temp_dir("untrusted-header");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let session = session_file_with_cwd(&dir, &workspace);
    let store = CheckpointStore::open(&session).expect("store");
    let target = workspace.join("victim.txt");
    std::fs::write(&target, b"original").expect("target");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"mutated").expect("mutate");
    let error = store
        .restore(&cp.id, false)
        .expect_err("an untrusted header cwd must not become the root");
    assert!(
        error.contains("restore requires --root <dir>"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(&target).expect("read target"),
        b"mutated",
        "an untrusted session must not authorize any write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (V2a): an explicit `--root` is trusted, but it still confines
/// every recorded target. A forged meta path outside it must be refused.
#[test]
fn restore_with_explicit_root_refuses_target_outside_root() {
    let dir = temp_dir("explicit-root-escape");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outside = dir.join("outside.txt");
    std::fs::write(&outside, b"outside-original").expect("outside file");
    let session = session_file(&dir);
    let store = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(workspace);
    let cp = store
        .snapshot("write", std::slice::from_ref(&outside))
        .expect("snapshot");
    std::fs::write(&outside, b"outside-mutated").expect("mutate outside");
    let report = store.restore(&cp.id, false).expect("restore report");
    assert_eq!(report.restored, 0, "report: {report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("outside the workspace")),
        "report: {report:?}"
    );
    assert_eq!(
        std::fs::read(&outside).expect("read outside"),
        b"outside-mutated",
        "a target outside the explicit root must not be touched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c): the legitimate path keeps working — an explicit
/// `--root`/`with_workspace_root` restores targets inside that root.
#[test]
fn restore_with_explicit_root_restores_inside_root() {
    let dir = temp_dir("explicit-root");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let session = session_file(&dir);
    let store = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(workspace.clone());
    let target = workspace.join("note.txt");
    std::fs::write(&target, b"version one").expect("write v1");
    let cp = store
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    std::fs::write(&target, b"version two").expect("write v2");
    let report = store.restore(&cp.id, false).expect("restore");
    assert_eq!(report.restored, 1, "report: {report:?}");
    assert!(report.errors.is_empty(), "report: {report:?}");
    assert_eq!(std::fs::read(&target).expect("read"), b"version one");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c): every root is validated — it must exist, be a
/// directory, and never be a filesystem root.
#[test]
fn workspace_root_must_be_existing_directory_and_not_filesystem_root() {
    let dir = temp_dir("root-validation");
    let session = session_file(&dir);
    let target = dir.join("target.txt");
    std::fs::write(&target, b"value").expect("target");
    let cp = CheckpointStore::open(&session)
        .expect("store")
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");

    let open = |root: PathBuf| {
        CheckpointStore::open(&session)
            .expect("store")
            .with_workspace_root(root)
    };

    let error = open(dir.join("missing"))
        .restore(&cp.id, false)
        .expect_err("a missing root must be refused");
    assert!(
        error.contains("missing or not accessible"),
        "unexpected error: {error}"
    );

    let not_a_dir = dir.join("not-a-dir");
    std::fs::write(&not_a_dir, b"file").expect("not-a-dir file");
    let error = open(not_a_dir)
        .restore(&cp.id, false)
        .expect_err("a file root must be refused");
    assert!(
        error.contains("not a directory"),
        "unexpected error: {error}"
    );

    let error = open(filesystem_root())
        .restore(&cp.id, false)
        .expect_err("a filesystem root must be refused");
    assert!(
        error.contains("filesystem root"),
        "unexpected error: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression (F2c): UNC share roots (`\\server\share`) are filesystem roots
/// too; their prefix leaves every absolute path "contained".
#[cfg(windows)]
#[test]
fn workspace_root_rejects_unc_share_root() {
    let dir = temp_dir("unc-root");
    let session = session_file(&dir);
    let target = dir.join("target.txt");
    std::fs::write(&target, b"value").expect("target");
    let cp = CheckpointStore::open(&session)
        .expect("store")
        .snapshot("write", std::slice::from_ref(&target))
        .expect("snapshot");
    let error = CheckpointStore::open(&session)
        .expect("store")
        .with_workspace_root(PathBuf::from(r"\\server\share"))
        .restore(&cp.id, false)
        .expect_err("a UNC share root must be refused");
    assert!(
        error.contains("filesystem root"),
        "unexpected error: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `pir checkpoint restore --help` must advertise the required `--root`
/// (F2c: it is a real clap argument, not a hidden unknown flag).
#[test]
fn checkpoint_restore_help_mentions_root() {
    let agent_dir = temp_dir("help-agent");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args(["checkpoint", "restore", "--help"])
        .env("PIR_AGENT_DIR", &agent_dir)
        .output()
        .expect("pir binary should start");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("--root <DIR>"),
        "restore --help must mention --root <DIR>: {text}"
    );
    let _ = std::fs::remove_dir_all(&agent_dir);
}

/// `preprocess_args` must leave `--root` in argv for clap (a missing
/// `KNOWN_LONG_FLAGS` entry would silently swallow it as an extension flag).
#[test]
fn preprocess_args_keeps_checkpoint_restore_root() {
    let args = |root: &str| {
        vec![
            "checkpoint".to_owned(),
            "restore".to_owned(),
            "--root".to_owned(),
            root.to_owned(),
            "--dry-run".to_owned(),
            "latest".to_owned(),
        ]
    };
    let (cleaned, unknown) = pir::coding_agent::cli::preprocess_args(args("workspace"));
    assert_eq!(
        cleaned,
        vec![
            "checkpoint",
            "restore",
            "--root",
            "workspace",
            "--dry-run",
            "latest"
        ]
    );
    assert!(
        !unknown.contains_key("root"),
        "root must not be collected as an unknown extension flag: {unknown:?}"
    );
    assert!(
        !unknown.contains_key("dry-run"),
        "dry-run must not be collected as an unknown extension flag: {unknown:?}"
    );

    let (cleaned, unknown) = pir::coding_agent::cli::preprocess_args(vec![
        "checkpoint".to_owned(),
        "restore".to_owned(),
        "--root=workspace".to_owned(),
        "latest".to_owned(),
    ]);
    assert_eq!(
        cleaned,
        vec!["checkpoint", "restore", "--root=workspace", "latest"]
    );
    assert!(!unknown.contains_key("root"));
}

/// Regression (A-08): one-shot stores must not leave one dead entry per root
/// in the process-wide lock table.
#[test]
fn per_root_lock_table_reaps_dead_entries() {
    let before = pir::checkpoints::snapshot_lock_table_len_for_tests();
    let dir = temp_dir("lock-table");
    for index in 0..64 {
        let subdir = dir.join(format!("session-{index}"));
        std::fs::create_dir_all(&subdir).expect("subdir");
        let session = subdir.join("session.jsonl");
        std::fs::write(&session, b"{\"type\":\"session\"}\n").expect("header");
        let store = CheckpointStore::for_session_file(&session).expect("store");
        drop(store);
    }
    let after = pir::checkpoints::snapshot_lock_table_len_for_tests();
    assert!(
        after.saturating_sub(before) < 16,
        "dead per-root locks were retained: before {before}, after {after}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
