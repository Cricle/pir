//! Streaming x persistence review fixes (independent binary; the g_*.rs and
//! tests/suites/ files belong to the gate agent's conflict scope):
//!
//! * **P1**: steering/follow-up prompts injected by the agent loop go through
//!   the per-message persistence sink AND land in `new_messages`, so the
//!   post-run batch (`skip(already_persisted)`) neither double-appends nor
//!   loses them; a storage-projection rebuild from the persisted transcript
//!   keeps the injected message (settle-retry keeps its context).
//! * **P2-2**: `remove_session` deletes the durable file BEFORE evicting the
//!   in-memory record and broadcasting `closed` — a failed delete answers
//!   `storage` and leaves the session fully intact (registry + file).
//! * **P2-3**: `rewrite_file` is atomic (temp file + fsync + rename): the
//!   success path leaves no temp residue and reopens cleanly; a blocked
//!   rename (Windows: target locked without FILE_SHARE_DELETE) fails the
//!   rewrite and preserves the original file byte-for-byte.

use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use pir::agent::agent_loop::{run_with_options, LoopDeps, LoopOptions, ProviderRequest};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, StopReason, TextContent, UserMessage,
    UserRole,
};
use pir::server::ServerState;
use pir::session_manager::{FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-stream-persist-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn user_text(message: &Message) -> Option<&str> {
    match message {
        Message::User(user) => user.content.iter().find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

fn assistant_text(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// P1: injected steering persists and is counted in new_messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steering_injection_persists_through_sink_and_counts_in_new_messages() {
    // The fake provider answers every request with a terminal Stop message;
    // two requests happen because the first steering delivery schedules
    // another turn.
    let stream_calls = Arc::new(Mutex::new(0usize));
    let stream: pir::agent::agent_loop::StreamFn = {
        let calls = Arc::clone(&stream_calls);
        Box::new(
            move |_request: ProviderRequest| -> BoxFuture<'static, AssistantMessage> {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    let mut calls = calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *calls += 1;
                    let turn = *calls;
                    assistant_text(&format!("assistant-reply-{turn}"))
                })
            },
        )
    };

    // One queued steering prompt, delivered on the first poll only.
    let steered = Arc::new(Mutex::new(false));
    let steering: pir::agent::agent_loop::FollowUpFn = {
        let steered = Arc::clone(&steered);
        Box::new(move || {
            let steered = Arc::clone(&steered);
            Box::pin(async move {
                let mut steered = steered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *steered {
                    Vec::new()
                } else {
                    *steered = true;
                    vec!["steer-redirect-1".to_owned().into()]
                }
            })
        })
    };

    // The persistence sink (same seam agent_session installs): records every
    // message the loop hands over, in order.
    let persisted: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: pir::agent::agent_loop::PersistMessageFn = {
        let persisted = Arc::clone(&persisted);
        Arc::new(move |message: Message| {
            let persisted = Arc::clone(&persisted);
            Box::pin(async move {
                persisted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(message);
            })
        })
    };

    let deps = LoopDeps {
        stream,
        ..LoopDeps::default()
    };
    let options = LoopOptions {
        steering: Some(steering),
        persist_message: Some(sink),
        ..LoopOptions::default()
    };

    let mut context = Context::default();
    context.messages.push(user_message("initial prompt"));
    let result = run_with_options(deps, context, None, options).await;

    // The steering prompt was delivered exactly once, persisted through the
    // sink, recorded in new_messages, and kept in the context.
    let persisted = persisted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let persisted_steering = persisted
        .iter()
        .filter(|message| user_text(message) == Some("steer-redirect-1"))
        .count();
    assert_eq!(
        persisted_steering, 1,
        "the injected steering message must be persisted exactly once via the sink"
    );

    let new_messages_steering = result
        .new_messages
        .iter()
        .filter(|message| user_text(message) == Some("steer-redirect-1"))
        .count();
    assert_eq!(
        new_messages_steering, 1,
        "the injected steering message must be part of new_messages"
    );

    assert!(
        result
            .context
            .messages
            .iter()
            .any(|message| user_text(message) == Some("steer-redirect-1")),
        "the injected steering message must be part of the run context"
    );

    // Sink order == new_messages order, and the sink covered every message:
    // the post-run batch (`produced.into_iter().skip(already_persisted)`)
    // must append NOTHING (no duplicate steering entry in storage).
    let persisted_count = persisted.len();
    assert_eq!(
        persisted_count,
        result.new_messages.len(),
        "the sink must have covered every new_messages entry in order"
    );
    for (persisted_message, new_message) in persisted.iter().zip(&result.new_messages) {
        assert_eq!(
            persisted_message, new_message,
            "sink order must match new_messages order (post-run skip depends on it)"
        );
    }
    assert_eq!(
        result.new_messages.iter().skip(persisted_count).count(),
        0,
        "post-run batch skip(already_persisted) must be a no-op for this run"
    );

    // Storage-projection proxy (settle-retry rebuild): a context rebuilt from
    // the persisted transcript keeps the injected steering message — the
    // continuation does not lose the redirect.
    let rebuilt = Context {
        messages: persisted.clone(),
        ..Context::default()
    };
    assert!(
        rebuilt
            .messages
            .iter()
            .any(|message| user_text(message) == Some("steer-redirect-1")),
        "a storage rebuild after a crash/settle must keep the injected steering"
    );
}

// ---------------------------------------------------------------------------
// P2-2: remove_session deletes the file BEFORE eviction/broadcast
// ---------------------------------------------------------------------------

async fn seed_session_with_file(dir: &std::path::Path, id: &str, cwd: &str) -> std::path::PathBuf {
    // Materialize the durable file first (the file is created on the first
    // conversation message, like upstream).
    let mut manager = SessionManager::create_with_id(cwd, dir, id).expect("create session file");
    manager
        .append_message(user_message("seed message"))
        .expect("append seed message");
    let path = manager.path().to_path_buf();
    drop(manager);
    assert!(path.exists(), "seeded session file must exist");
    path
}

#[tokio::test]
async fn remove_session_deletes_file_then_evicts_and_broadcasts() {
    let dir = TempDir::new("remove-ok");
    let path = seed_session_with_file(dir.path(), "remove-ok-1", "C:\\proj").await;

    let state = ServerState::default().with_session_dir(dir.path());
    // The registry record is created in memory (the on-disk file already
    // exists, so the per-record storage handle degrades to memory — removal
    // locates the file by header id, not by the handle).
    state
        .create_session(
            "remove-ok-1".to_owned(),
            Some("C:\\proj".to_owned()),
            None,
            None,
            None,
            1,
        )
        .await
        .expect("create session record");
    state
        .attach_session("remove-ok-1", "conn-1")
        .await
        .expect("attach");

    state
        .remove_session("remove-ok-1", "conn-1")
        .await
        .expect("remove must succeed when the file deletes");

    // File gone, record evicted, re-attach answers not_found.
    assert!(!path.exists(), "the durable file must be deleted");
    assert!(
        state.get_session("remove-ok-1").await.is_none(),
        "the record must be evicted after a successful delete"
    );
    let error = state
        .attach_session("remove-ok-1", "conn-1")
        .await
        .expect_err("attach of a removed session must fail");
    assert!(
        error.to_string().contains("not found"),
        "re-attach must answer not_found, got: {error}"
    );
}

/// Windows-only delete-failure mock: a handle opened without FILE_SHARE_DELETE
/// blocks both `remove_file` and rename-over-target (sharing violation), which
/// is exactly the "storage delete failed" scenario.
#[cfg(windows)]
#[tokio::test]
async fn remove_session_failed_delete_keeps_session_intact() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = TempDir::new("remove-locked");
    let path = seed_session_with_file(dir.path(), "remove-locked-1", "C:\\proj").await;

    let state = ServerState::default().with_session_dir(dir.path());
    state
        .create_session(
            "remove-locked-1".to_owned(),
            Some("C:\\proj".to_owned()),
            None,
            None,
            None,
            1,
        )
        .await
        .expect("create session record");
    state
        .attach_session("remove-locked-1", "conn-1")
        .await
        .expect("attach");

    // Lock the file (read sharing only: no FILE_SHARE_DELETE).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ
        .open(&path)
        .expect("lock session file");
    let original = std::fs::read_to_string(&path).expect("read locked file");

    // The delete fails -> Storage error, and the session must remain FULLY
    // intact: file preserved AND record still in the registry (no eviction, no
    // half-removed state a later attach would resurrect as "deleted").
    let error = state
        .remove_session("remove-locked-1", "conn-1")
        .await
        .expect_err("remove must fail while the file cannot be deleted");
    assert!(
        error.to_string().contains("session storage error"),
        "a failed delete must answer Storage, got: {error}"
    );
    assert!(path.exists(), "the file must survive a failed delete");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap_or_default(),
        original,
        "the file content must be untouched by the failed remove"
    );
    assert!(
        state.get_session("remove-locked-1").await.is_some(),
        "the record must NOT be evicted when the delete fails"
    );
    drop(lock);

    // With the lock released the same call succeeds (order: delete -> evict).
    state
        .remove_session("remove-locked-1", "conn-1")
        .await
        .expect("remove succeeds after the lock is released");
    assert!(!path.exists());
    assert!(state.get_session("remove-locked-1").await.is_none());
}

// ---------------------------------------------------------------------------
// P2-3: rewrite_file is atomic (temp + fsync + rename)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rewrite_file_success_leaves_no_temp_residue_and_reopens() {
    let dir = TempDir::new("rewrite-ok");
    let mut manager =
        SessionManager::create("C:\\proj", dir.path()).expect("create session manager");
    let first_id = manager
        .append_message(user_message("first message"))
        .expect("append first");
    manager
        .append_message(user_message("second message"))
        .expect("append second");

    // `create_branched_session` ends in `rewrite_file` (whole-file rewrite of
    // the branched path into a new file).
    let new_path = manager
        .create_branched_session(&first_id)
        .expect("branch")
        .expect("branched session path");

    assert!(new_path.exists(), "the rewritten file must exist");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .map(|extension| extension != "jsonl")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp residue may remain after a successful rewrite: {leftovers:?}"
    );

    // The rewritten file reopens cleanly and keeps the branched path.
    let reopened = SessionManager::open(&new_path).expect("reopen rewritten file");
    let message_count = reopened
        .entries()
        .iter()
        .filter(|entry| matches!(entry, FileEntry::Message(_)))
        .count();
    assert_eq!(
        message_count, 1,
        "the branched file keeps only the root->leaf path (header + first message)"
    );
}

/// Windows-only rename-blocked mock: with the target file locked (no
/// FILE_SHARE_DELETE) neither the atomic rename nor the remove+rename
/// fallback can succeed — the rewrite must fail with the ORIGINAL file
/// byte-for-byte intact and no temp residue.
#[cfg(windows)]
#[tokio::test]
async fn rewrite_file_failure_preserves_original_file() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = TempDir::new("rewrite-locked");
    let mut manager =
        SessionManager::create("C:\\proj", dir.path()).expect("create session manager");
    manager
        .append_message(user_message("before rewrite"))
        .expect("append message");
    let path = manager.path().to_path_buf();
    drop(manager);

    // A header version below CURRENT_SESSION_VERSION makes the next `open`
    // run the in-place migration, which ends in `rewrite_file` over the SAME
    // file — the atomic-replace path under test.
    let original = std::fs::read_to_string(&path).expect("read session file");
    let mut lines = original.lines();
    let header_line = lines.next().expect("header line");
    let mut header: serde_json::Value = serde_json::from_str(header_line).expect("parse header");
    header["version"] = serde_json::json!(1);
    let migrated_source = format!(
        "{}\n{}",
        serde_json::to_string(&header).expect("serialize header"),
        lines.collect::<Vec<_>>().join("\n")
    );
    std::fs::write(&path, &migrated_source).expect("write v1 header");

    // Lock the target: rename-over and remove both fail (sharing violation).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ
        .open(&path)
        .expect("lock session file");

    let result = SessionManager::open(&path);

    drop(lock);

    assert!(
        result.is_err(),
        "the rewrite must fail while the target cannot be replaced"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap_or_default(),
        migrated_source,
        "the original file must survive a failed rewrite byte-for-byte"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .map(|extension| extension != "jsonl")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp residue may remain after a failed rewrite: {leftovers:?}"
    );
}
