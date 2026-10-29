//! Extension command-context session actions (`ctx.newSession` / `ctx.fork` /
//! `ctx.navigateTree` / `ctx.switchSession` / `ctx.reload` — upstream
//! print-mode.ts:78-100 `commandContextActions`) and the per-dialog
//! `timeout` execution (upstream `ExtensionUIDialogOptions.timeout`).
//!
//! The ctx-action tests drive the print host's command path
//! (`pir::coding_agent::print::try_extension_command`) against a temporary
//! session fixture through a fake extension registry: every command handler
//! calls one ctx action and the test asserts the effect on the shared session
//! storage. The dialog tests use `ExtensionHandle::set_dialog_ttl` for the
//! global TTL and verify a request-level `timeout` overrides it in both
//! directions (shorter and longer), with the unchanged resolve-null semantics.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files). The command-context action
//! queue is crate-global, so every ctx-action test holds a shared lock around
//! its dispatch+drain window to stay isolated from sibling tests.

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Value};

use pir::agent_session::SessionCommandAction;
use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::coding_agent::print::try_extension_command;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::session_manager::SessionManager;

/// The ctx-action tests' shared dispatch window (the recorded-action queue is
/// process-global; only one test may dispatch+drain at a time).
static ACTION_WINDOW: StdMutex<()> = StdMutex::new(());

/// Per-test temp directory removed at the end of each test body.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-ctx-actions-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn js_config(cwd: &std::path::Path, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        // These tests drive the print host's command path
        // (`try_extension_command`); the runtime mode is what the
        // `commandAction` host fn stamps onto recorded actions, so it must
        // say "print" for the print host to claim them.
        mode: "print".to_string(),
        has_ui,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// One fake extension exercising the command-ctx session actions (commands)
/// and the dialog bridge with request-level timeouts (tools).
const FIXTURE_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("act-new", { handler: async (args, ctx) => { await ctx.newSession(); } });
  pi.registerCommand("act-new-parent", { handler: async (args, ctx) => {
    await ctx.newSession({ parentSession: args });
  } });
  pi.registerCommand("act-fork", { handler: async (args, ctx) => {
    await ctx.fork(args === "" ? undefined : args);
  } });
  pi.registerCommand("act-nav", { handler: async (args, ctx) => {
    const parsed = JSON.parse(args);
    await ctx.navigateTree(parsed.targetId, { summarize: parsed.summarize === true });
  } });
  pi.registerCommand("act-switch", { handler: async (args, ctx) => {
    await ctx.switchSession(args);
  } });
  pi.registerCommand("act-reload", { handler: async (args, ctx) => { await ctx.reload(); } });
  pi.registerCommand("act-new-with-session", { handler: async (args, ctx) => {
    await ctx.newSession({ withSession: async () => {} });
  } });
  // Captured results (result shapes are asserted through eval_json).
  pi.registerCommand("capture-new", { handler: async (args, ctx) => {
    globalThis.__pir_last = await ctx.newSession();
  } });
  pi.registerCommand("capture-fork", { handler: async (args, ctx) => {
    globalThis.__pir_last = await ctx.fork(args === "" ? undefined : args);
  } });
  pi.registerCommand("capture-fork-at", { handler: async (args, ctx) => {
    globalThis.__pir_last = await ctx.fork(args, { position: "at" });
  } });
  // Invalid usage: the prelude must reject before any host fn call.
  pi.registerCommand("capture-nav-missing", { handler: async (args, ctx) => {
    globalThis.__pir_last = await ctx.navigateTree(undefined);
  } });
  pi.registerCommand("capture-switch-missing", { handler: async (args, ctx) => {
    globalThis.__pir_last = await ctx.switchSession();
  } });
  // Dialog bridge: a tool whose execute awaits ctx.ui.input with an optional
  // request-level timeout, reporting the outcome in the tool result text.
  pi.registerTool({
    name: "askInput",
    description: "asks for input through the dialog bridge",
    parameters: {
      type: "object",
      properties: { timeout: { type: "number" } },
    },
    execute: async (id, params, signal, onUpdate, ctx) => {
      const result = await ctx.ui.input(
        "title",
        "placeholder",
        params.timeout != null ? { timeout: params.timeout } : undefined,
      );
      return {
        content: [{ type: "text", text: result === undefined ? "TIMEOUT" : String(result) }],
      };
    },
  });
}
"#;

async fn load_fixture(label: &str, has_ui: bool) -> (std::path::PathBuf, ExtensionHandle) {
    let dir = temp_dir(label);
    let handle = ExtensionHandle::start(js_config(&dir, has_ui)).unwrap();
    let path = dir.join("ext.ts");
    std::fs::write(&path, FIXTURE_EXTENSION_TS).unwrap();
    let result = handle.load(std::slice::from_ref(&path)).await.unwrap();
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
    (dir, handle)
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
        timestamp: pir::ai::util::now_ms(),
        sections: None,
    })
}

/// A persisted three-message chain (A → B → C, leaf C) in `session_dir`,
/// with the entry ids returned in append order.
fn chain_fixture(session_dir: &std::path::Path) -> (SessionManager, Vec<String>) {
    let mut session = SessionManager::create(&session_dir.display().to_string(), session_dir)
        .expect("create session fixture");
    let mut ids = Vec::new();
    for index in 0..3 {
        ids.push(
            session
                .append_message(user_message(&format!("message {index}")))
                .expect("append fixture message"),
        );
    }
    (session, ids)
}

/// Shared print-host session storage (print.rs `SharedSession`).
type SharedStorage = Arc<tokio::sync::Mutex<Option<SessionManager>>>;

async fn dispatch(
    handle: &ExtensionHandle,
    prompt: &str,
    storage: &SharedStorage,
    session_dir: &std::path::Path,
) -> bool {
    try_extension_command(handle, prompt, storage, session_dir, None).await
}

// ---------------------------------------------------------------------------
// Command-ctx session actions through the print host path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn ctx_new_session_replaces_storage() {
    let (dir, handle) = load_fixture("new", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (session, ids) = chain_fixture(&session_dir);
    let old_id = session.id().to_owned();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(Some(session)));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(dispatch(&handle, "/act-new", &storage, &session_dir).await);

    let guard = storage.lock().await;
    let session = guard.as_ref().expect("new session storage");
    assert_ne!(
        session.id(),
        old_id,
        "ctx.newSession swapped in fresh storage"
    );
    assert_eq!(session.leaf_id(), None, "fresh storage has no leaf");
    assert!(
        session.entry(&ids[0]).is_none(),
        "fresh storage carries none of the replaced transcript"
    );
    assert!(
        take_queue_is_empty(),
        "the print host drained every recorded action"
    );
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn ctx_new_session_honors_parent_session() {
    let (dir, handle) = load_fixture("new-parent", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (parent, _) = chain_fixture(&session_dir);
    let parent_path = parent.path().to_string_lossy().into_owned();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(None));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        dispatch(
            &handle,
            &format!("/act-new-parent {parent_path}"),
            &storage,
            &session_dir
        )
        .await
    );
    let guard = storage.lock().await;
    let session = guard.as_ref().expect("new session storage");
    assert_eq!(
        session.parent_session(),
        Some(parent_path.as_str()),
        "ctx.newSession({{parentSession}}) records the parent link"
    );
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn ctx_fork_at_entry_moves_the_leaf() {
    let (dir, handle) = load_fixture("fork-entry", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (session, ids) = chain_fixture(&session_dir);
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(Some(session)));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        dispatch(
            &handle,
            // The capture variant stores the resolved value in
            // `globalThis.__pir_last` (the plain `/act-fork` discards it).
            &format!("/capture-fork {}", ids[1]),
            &storage,
            &session_dir
        )
        .await
    );
    let guard = storage.lock().await;
    let session = guard
        .as_ref()
        .expect("storage stays in place for fork-at-entry");
    // Upstream fork(entryId) defaults to position "before": the entry must
    // be a user message and the leaf lands on its PARENT.
    assert_eq!(
        session.leaf_id(),
        Some(ids[0].as_str()),
        "ctx.fork(entryId) branches at the entry's PARENT (before position)"
    );
    drop(guard);
    // The JS surface answers with the upstream runner-default shape.
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__pir_last)")
            .await
            .unwrap(),
        json!({ "cancelled": false }),
        "ctx.fork resolves {{cancelled: false}} (accepted)"
    );
    // position "at" branches at the entry itself (the options ride the
    // commandAction bridge).
    assert!(
        dispatch(
            &handle,
            &format!("/capture-fork-at {}", ids[1]),
            &storage,
            &session_dir
        )
        .await
    );
    let guard = storage.lock().await;
    assert_eq!(
        guard.as_ref().expect("storage").leaf_id(),
        Some(ids[1].as_str()),
        "ctx.fork(entryId, {{position: 'at'}}) branches at the entry"
    );
    drop(guard);
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn ctx_fork_current_creates_a_child_session_file() {
    let (dir, handle) = load_fixture("fork-current", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (session, _) = chain_fixture(&session_dir);
    let source_path = session.path().to_path_buf();
    let source_id = session.id().to_owned();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(Some(session)));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(dispatch(&handle, "/act-fork", &storage, &session_dir).await);
    let guard = storage.lock().await;
    let session = guard.as_ref().expect("forked storage");
    assert_ne!(
        session.id(),
        source_id,
        "fork-at-current created a new session id"
    );
    assert_eq!(
        session.parent_session(),
        Some(source_path.to_string_lossy().as_ref()),
        "the forked file links back to the source session"
    );
    assert_ne!(
        session.path(),
        source_path.as_path(),
        "the fork lives in its own file"
    );
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn ctx_switch_session_swaps_storage() {
    let (dir, handle) = load_fixture("switch", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (original, original_ids) = chain_fixture(&session_dir);
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(Some(original)));

    // The switch target: its own session file with one message.
    let mut target = SessionManager::create(&session_dir.display().to_string(), &session_dir)
        .expect("create target session");
    let target_id = target
        .append_message(user_message("target session message"))
        .expect("append target message");
    let target_path = target.path().to_path_buf();
    drop(target);

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        dispatch(
            &handle,
            &format!("/act-switch {}", target_path.display()),
            &storage,
            &session_dir
        )
        .await
    );
    let guard = storage.lock().await;
    let session = guard.as_ref().expect("switched storage");
    assert_eq!(
        session.leaf_id(),
        Some(target_id.as_str()),
        "the target is live"
    );
    assert!(
        session.entry(&original_ids[0]).is_none(),
        "the original transcript is gone after the switch"
    );
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn ctx_navigate_tree_moves_the_leaf_to_the_targets_parent() {
    let (dir, handle) = load_fixture("nav", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (session, ids) = chain_fixture(&session_dir);
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(Some(session)));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Every fixture entry is a user message, so upstream semantics put the
    // leaf on the target's PARENT: navigating to B lands on A.
    let payload = json!({ "targetId": ids[1] }).to_string();
    assert!(
        dispatch(
            &handle,
            &format!("/act-nav {payload}"),
            &storage,
            &session_dir
        )
        .await
    );
    {
        let guard = storage.lock().await;
        assert_eq!(
            guard.as_ref().expect("storage").leaf_id(),
            Some(ids[0].as_str()),
            "navigateTree(B) moved the leaf onto B's parent A"
        );
    }
    // Navigating to the leaf's CURRENT location is the upstream no-op
    // (agent-session.ts:3979-3982: `targetId === oldLeafId` →
    // `{cancelled: false}`): after the first navigation the leaf already
    // sits on ids[0], so this navigation changes nothing.
    let payload = json!({ "targetId": ids[0] }).to_string();
    assert!(
        dispatch(
            &handle,
            &format!("/act-nav {payload}"),
            &storage,
            &session_dir
        )
        .await
    );
    {
        let guard = storage.lock().await;
        assert_eq!(
            guard.as_ref().expect("storage").leaf_id(),
            Some(ids[0].as_str()),
            "navigating to the current leaf is the upstream no-op"
        );
    }
    // Navigating to an unknown entry is an application error.
    let payload = json!({ "targetId": "no-such-entry" }).to_string();
    assert!(
        dispatch(
            &handle,
            &format!("/act-nav {payload}"),
            &storage,
            &session_dir
        )
        .await
    );
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn ctx_navigate_tree_rejects_unknown_entries_directly() {
    let (dir, _handle) = load_fixture("nav-direct", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut storage = Some(chain_fixture(&session_dir).0);
    let action = SessionCommandAction::NavigateTree {
        target_id: "no-such-entry".to_owned(),
        summarize: false,
        custom_instructions: None,
        replace_instructions: false,
        label: None,
    };
    let error = pir::agent_session::apply_session_command_to_storage(
        &mut storage,
        &session_dir,
        &action,
        None,
    )
    .await
    .expect_err("an unknown target entry is rejected");
    assert!(
        error.contains("no-such-entry"),
        "the navigation error names the target: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn headless_storage_level_rejections() {
    let (dir, _handle) = load_fixture("reject", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();

    // No storage (`--no-session`): the leaf-bound actions are rejected like
    // the upstream runtimeHost without session storage, while newSession /
    // switchSession can ADOPT storage (the print host has none until one is
    // opened).
    let mut storage: Option<SessionManager> = None;
    let fork_result = pir::agent_session::apply_session_command_to_storage(
        &mut storage,
        &session_dir,
        &SessionCommandAction::Fork {
            entry_id: None,
            position: None,
            with_session: None,
        },
        None,
    )
    .await;
    assert!(fork_result.is_err(), "fork without storage is rejected");
    let nav_result = pir::agent_session::apply_session_command_to_storage(
        &mut storage,
        &session_dir,
        &SessionCommandAction::NavigateTree {
            target_id: "root".to_owned(),
            summarize: false,
            custom_instructions: None,
            replace_instructions: false,
            label: None,
        },
        None,
    )
    .await;
    assert!(
        nav_result.is_err(),
        "navigateTree without storage is rejected"
    );
    assert!(
        storage.is_none(),
        "rejected actions leave storage untouched"
    );

    // Reload: not a storage action. A host with an extension runtime applies
    // it through `AgentSession::reload`; this storage-only print host has no
    // resource surface, so applying it is refused (storage untouched).
    let mut storage = Some(chain_fixture(&session_dir).0);
    let reload_result = pir::agent_session::apply_session_command_to_storage(
        &mut storage,
        &session_dir,
        &SessionCommandAction::Reload,
        None,
    )
    .await;
    assert!(
        reload_result.is_err(),
        "a storage-only host must not pretend it reloaded resources"
    );

    // Summarized navigation without a provider binding: rejected, leaf
    // unchanged.
    let (session, ids) = chain_fixture(&session_dir);
    let mut storage = Some(session);
    let action = SessionCommandAction::NavigateTree {
        target_id: ids[1].clone(),
        summarize: true,
        custom_instructions: None,
        replace_instructions: false,
        label: None,
    };
    let result = pir::agent_session::apply_session_command_to_storage(
        &mut storage,
        &session_dir,
        &action,
        None,
    )
    .await;
    assert!(result.is_err(), "summarized navigation needs a binding");
    assert_eq!(
        storage.as_ref().expect("storage kept").leaf_id(),
        Some(ids[2].as_str()),
        "the failed navigation left the leaf unchanged"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn js_surface_rejects_invalid_action_arguments() {
    let (dir, handle) = load_fixture("js-invalid", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(None));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outcome = handle
        .dispatch_command("capture-nav-missing", "")
        .await
        .unwrap();
    assert!(
        !outcome.ok,
        "ctx.navigateTree(undefined) must reject: {:?}",
        outcome.error
    );
    let error = outcome.error.expect("error text");
    assert!(
        error.contains("navigateTree requires a target entry id"),
        "navigation without a target is rejected: {error}"
    );

    let outcome = handle
        .dispatch_command("capture-switch-missing", "")
        .await
        .unwrap();
    assert!(
        !outcome.ok,
        "ctx.switchSession() without a path must reject"
    );
    let error = outcome.error.expect("error text");
    assert!(
        error.contains("switchSession requires a session path"),
        "switch without a path is rejected: {error}"
    );
    assert!(
        take_queue_is_empty(),
        "rejected arguments never record a session action"
    );
    let _ = storage;
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn js_capture_reports_runner_default_result_shapes() {
    let (dir, handle) = load_fixture("js-capture", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(None));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(handle.dispatch_command("capture-new", "").await.unwrap().ok);
    assert_eq!(
        handle
            .eval_json("JSON.stringify(globalThis.__pir_last)")
            .await
            .unwrap(),
        json!({ "cancelled": false }),
        "ctx.newSession resolves the upstream runner-default shape"
    );
    // ctx.reload() is accepted at the JS surface (upstream runner default is
    // a no-op success); the print host reports the unsupported application
    // on stderr.
    assert!(handle.dispatch_command("act-reload", "").await.unwrap().ok);
    // `typeof` yields the bare word `object`, which is not JSON — wrap it so
    // eval_json can parse the string result.
    assert_eq!(
        handle
            .eval_json("JSON.stringify(typeof globalThis.__pir_last)")
            .await
            .unwrap(),
        json!("object"),
        "reload left the captured newSession result in place (no throw)"
    );
    assert!(!take_queue_is_empty(), "reload was recorded for the host");
    assert!(take_queue_is_empty(), "the drain consumed the queue");
    let _ = storage;
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The print host has raw storage but no replacement-session callback plane:
/// an action carrying a `withSession` callback is refused BEFORE the storage
/// is touched (and the callback token is dropped with the process).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // the action-queue lock must span the runtime awaits
async fn print_host_refuses_with_session_actions_without_replacing_storage() {
    let (dir, handle) = load_fixture("with-session-print", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let (session, ids) = chain_fixture(&session_dir);
    let old_id = session.id().to_owned();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(Some(session)));

    let _window = ACTION_WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        dispatch(&handle, "/act-new-with-session", &storage, &session_dir).await,
        "the command was handled"
    );

    let guard = storage.lock().await;
    let session = guard.as_ref().expect("storage kept");
    assert_eq!(
        session.id(),
        old_id,
        "the refused withSession action left the session alone"
    );
    assert!(
        session.entry(&ids[2]).is_some(),
        "the transcript is untouched"
    );
    assert!(
        take_queue_is_empty(),
        "the print host consumed (and refused) the recorded action"
    );
    drop(guard);
    drop(_window);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn unmatched_slash_prompts_fall_through() {
    let (dir, handle) = load_fixture("fallthrough", false).await;
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let storage: SharedStorage = Arc::new(tokio::sync::Mutex::new(None));

    assert!(
        !dispatch(&handle, "/no-such-command", &storage, &session_dir).await,
        "an unregistered command falls through to the model"
    );
    assert!(
        !dispatch(&handle, "plain prompt", &storage, &session_dir).await,
        "a non-command prompt falls through"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Per-dialog `timeout` execution (upstream ExtensionUIDialogOptions.timeout)
// ---------------------------------------------------------------------------

fn tool_text(result: &pir::extensions::ToolResult) -> String {
    result
        .content
        .first()
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_request_timeout_overrides_a_long_global_ttl() {
    let (dir, handle) = load_fixture("dialog-short", true).await;
    // Global TTL far beyond the test budget; the request-level timeout must
    // win.
    handle.set_dialog_ttl(std::time::Duration::from_secs(60));

    let started = std::time::Instant::now();
    let result = handle
        .call_tool("askInput", "d1", &json!({ "timeout": 250 }))
        .await
        .expect("tool call completes");
    let elapsed = started.elapsed();

    assert_eq!(
        tool_text(&result),
        "TIMEOUT",
        "an unanswered dialog resolves null"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "the request timeout ({elapsed:?}) honored the 250ms request value"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "the request timeout fired well before the 60s global TTL ({elapsed:?})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_request_timeout_extends_past_a_short_global_ttl() {
    let (dir, handle) = load_fixture("dialog-long", true).await;
    // Global TTL short; the request-level timeout EXTENDS the wait (upstream
    // semantics: opts.timeout overrides the dialog wait duration).
    handle.set_dialog_ttl(std::time::Duration::from_millis(300));

    let started = std::time::Instant::now();
    let pending = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .call_tool("askInput", "d2", &json!({ "timeout": 8000 }))
                .await
                .expect("tool call completes")
        }
    });
    // Outlive the 300ms global TTL, then answer: the request timeout must
    // keep the dialog pending until the reply lands. The runtime assigns
    // its own numeric dialog id, so take the real id from the notification
    // queue instead of fabricating one (a fabricated id would store a reply
    // no waiter ever reads, and the dialog would resolve null at timeout).
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let dialog_id = handle
        .drain_notifications()
        .into_iter()
        .find_map(|notification| match notification {
            pir::extensions::Notification::DialogRequest { id, .. } => Some(id.to_string()),
            _ => None,
        })
        .expect("the pending dialog announced itself");
    handle.respond_dialog(&dialog_id, Value::String("hello".to_owned()));
    let result = pending.await.expect("tool task joins");
    let elapsed = started.elapsed();

    assert_eq!(
        tool_text(&result),
        "hello",
        "the dialog stayed pending past the global TTL and took the reply"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(650),
        "the wait was bounded by the request timeout, not the 300ms global TTL ({elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the reply resolved the dialog promptly ({elapsed:?})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn dialog_global_ttl_applies_without_a_request_timeout() {
    let (dir, handle) = load_fixture("dialog-global", true).await;
    handle.set_dialog_ttl(std::time::Duration::from_millis(300));

    let started = std::time::Instant::now();
    let result = handle
        .call_tool("askInput", "d3", &json!({}))
        .await
        .expect("tool call completes");
    let elapsed = started.elapsed();

    assert_eq!(
        tool_text(&result),
        "TIMEOUT",
        "no request timeout: global TTL applies"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(250)
            && elapsed < std::time::Duration::from_secs(5),
        "the global TTL bounded the wait ({elapsed:?})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn take_queue_is_empty() -> bool {
    pir::agent_session::take_session_command_actions().is_empty()
}
