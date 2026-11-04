//! P0-3: file-level checkpoint/undo — new/modified/deleted/renamed files,
//! cross-turn restore, retention bounds and read-only refusal.

use std::path::{Path, PathBuf};

use pir::checkpoints::{CheckpointStore, DEFAULT_MAX_CHECKPOINTS};

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

#[tokio::test]
async fn write_tool_snapshots_the_file_before_mutating_it() {
    use pir::agent::tool_execution::{AgentTool, ToolContext, ToolSessionEnv};
    use pir::ai::provider::ProviderRegistry;
    use pir::ai::types::*;
    use pir::coding_agent::tools::WriteTool;

    let dir = temp_dir("write-hook");
    let session = session_file(&dir);
    let target = dir.join("hooked.txt");
    std::fs::write(&target, b"before-hook").expect("write");

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
    let mut ctx = ToolContext::new(&dir);
    ctx.session_env = Some(std::sync::Arc::new(ToolSessionEnv {
        provider_registry: std::sync::Arc::new(ProviderRegistry::default()),
        model,
        thinking: ThinkingLevel::Off,
        session_id: Some("test-session".to_owned()),
        session_file: Some(session.clone()),
        base_tools: std::sync::Arc::new(pir::agent::tool_execution::ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: None,
        agent_dir: Some(dir.clone()),
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background: None,
    }));
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
