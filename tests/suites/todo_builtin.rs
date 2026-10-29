//! The `todo` builtin (`@juicesharp/rpiv-todo` 2.12.0 ported natively):
//! registration surface, the `/todos` command, transcript persistence, and
//! the CPU/memory invariants (unrelated events touch nothing; per-push cost is
//! one entries pass, never per-event).
//!
//! Unit coverage of the reducer/state machine/envelope/replay lives here
//! (migrated from the former inline `#[cfg(test)]` module in
//! `src/plugins/builtin/todo.rs` — the hygiene gate keeps tests out of
//! `src/`); the SDK-facing tests (declared tools and the session-file round
//! trip) live in `tests/suites/sdk.rs`.

#![cfg(feature = "todo")]

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_batch::{FinalizedToolCall, ToolResult};
use pir::agent::tool_execution::ToolContext;
use pir::extensions::SessionContextSnapshot;
use pir::plugins::builtin::todo::{
    apply_mutation, build_tool_result, derive_blocks, detect_cycle, format_content,
    is_transition_valid, replay_from_entries, sanitize_terminal_text, todos_command_outcome, Task,
    TaskAction, TaskStatus, TodoOp, TodoParams, TodoPlugin, TodoState, TodoStore, COMMAND_NAME,
    ERR_REQUIRES_INTERACTIVE, MAX_METADATA_BYTES, MAX_OWNER_CHARS, MAX_SESSION_SLOTS,
    MAX_SUBJECT_CHARS, MAX_TASKS_PER_SESSION, MSG_NO_TODOS, TOOL_NAME,
};
use pir::plugins::{AgentPlugin, EventInterest, PluginHost, ToolBackend};

fn plugin() -> Arc<TodoPlugin> {
    Arc::new(TodoPlugin::new())
}

fn host_with(plugin: Arc<TodoPlugin>) -> PluginHost {
    let as_plugin: Arc<dyn AgentPlugin> = plugin;
    PluginHost::build(None, vec![as_plugin])
}

/// A concise snapshot of the counters that must not move on unrelated events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counters {
    pushes: u64,
    scans: u64,
    replay_skips: u64,
    entries_visited: u64,
    state_touches: u64,
    tool_calls: u64,
    handled_events: u64,
}

fn counters(plugin: &TodoPlugin) -> Counters {
    let stats = plugin.stats().snapshot();
    Counters {
        pushes: stats.pushes,
        scans: stats.scans,
        replay_skips: stats.replay_skips,
        entries_visited: stats.entries_visited,
        state_touches: stats.state_touches,
        tool_calls: stats.tool_calls,
        handled_events: stats.handled_events,
    }
}

/// Run one tool call through the registered Rust backend and return the
/// result.
async fn run_tool(host: &PluginHost, args: Value) -> pir::agent::tool_batch::ToolResult {
    let registry = host.registry();
    let tool = registry
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == TOOL_NAME)
        .map(|(_, backend)| backend)
        .expect("todo tool registered");
    let ToolBackend::Rust(tool) = tool else {
        panic!("todo must be a native Rust tool");
    };
    let context = ToolContext::new(std::env::temp_dir());
    tool.run(&args, &context).await
}

fn snapshot(entries: Value, session_id: Option<&str>) -> SessionContextSnapshot {
    SessionContextSnapshot {
        entries,
        session_id: session_id.map(str::to_owned),
        ..SessionContextSnapshot::default()
    }
}

/// The transcript entries a passing tool call persists (the real
/// `ToolResultMessage` JSON, serialized through the model layer's type).
fn entry_from(result: &pir::agent::tool_batch::ToolResult, call_id: &str, timestamp: u64) -> Value {
    use pir::ai::types::{ContentBlock, Message, TextContent, ToolResultMessage, ToolResultRole};
    serde_json::to_value(Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.to_owned(),
        tool_name: TOOL_NAME.to_owned(),
        content: vec![ContentBlock::Text(TextContent {
            text: result.content.clone(),
            text_signature: None,
        })],
        details: result.details.clone(),
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: result.is_error,
        duration_ms: None,
        timestamp,
    }))
    .expect("entry serializes")
}

// ---------------------------------------------------------------------------
// Registration surface
// ---------------------------------------------------------------------------

/// `builtin:todo` exists, is replaceable, instantiates, and its plugin
/// registers exactly one tool, the `/todos` command, and the `ctrl+shift+t`
/// collapse definition.
#[test]
fn builtin_registration_surface_is_pinned() {
    let def = pir::plugins::builtin::builtin_extension_def("todo").expect("builtin table entry");
    assert!(
        def.replaceable,
        "a user `todo` tool/command must be able to replace it"
    );
    assert!(
        pir::plugins::instantiate_builtin("todo").is_some(),
        "builtin:todo must instantiate in this build"
    );
    assert!(pir::plugins::builtin::builtin_names().contains(&"todo"));

    let plugin = plugin();
    let host = host_with(plugin);

    let registry = host.registry();
    let tools: Vec<String> = registry
        .resolved_tools()
        .into_iter()
        .map(|(def, _)| def.name.clone())
        .collect();
    assert!(
        tools.iter().any(|name| name == TOOL_NAME),
        "the todo tool must be registered: {tools:?}"
    );

    let commands = registry.resolved_commands();
    let todos = commands
        .iter()
        .find(|command| command.def.name == COMMAND_NAME)
        .expect("todos command registered");
    assert_eq!(
        todos.def.description.as_deref(),
        Some("Show all todos on the current branch, grouped by status")
    );
    assert!(
        matches!(todos.backend, Some(pir::plugins::CommandBackend::Rust(_))),
        "the command must run natively"
    );

    let shortcuts: Vec<String> = registry
        .plugins()
        .iter()
        .flat_map(|plugin| {
            plugin
                .registrations
                .shortcuts
                .iter()
                .map(|(def, _)| def.key.clone())
        })
        .collect();
    assert!(
        shortcuts.iter().any(|key| key == "ctrl+shift+t"),
        "the collapse/expand definition must be registered: {shortcuts:?}"
    );

    // The identity strings are the persistence key: never rename.
    assert_eq!(TOOL_NAME, "todo");
    assert_eq!(COMMAND_NAME, "todos");
}

/// The tool definition the model sees: contract schema, execution mode, and
/// the prompt guidance.
#[test]
fn tool_definition_matches_the_contract() {
    let host = host_with(plugin());
    let registry = host.registry();
    let (def, _) = registry
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == TOOL_NAME)
        .expect("todo tool registered");

    let parameters = &def.parameters;
    assert_eq!(parameters["type"], json!("object"));
    assert_eq!(parameters["required"], json!(["action"]));
    assert_eq!(
        parameters["properties"]["action"]["enum"],
        json!(["create", "update", "list", "get", "delete", "clear"])
    );
    assert_eq!(
        parameters["properties"]["status"]["enum"],
        json!(["pending", "in_progress", "completed", "deleted"])
    );
    for key in [
        "action",
        "subject",
        "description",
        "activeForm",
        "status",
        "blockedBy",
        "addBlockedBy",
        "removeBlockedBy",
        "owner",
        "metadata",
        "id",
        "includeDeleted",
    ] {
        assert!(
            parameters["properties"].get(key).is_some(),
            "missing `{key}` in the schema: {parameters}"
        );
    }
    assert_eq!(def.execution_mode, "sequential");
    assert_eq!(def.exposure, "direct");
    assert!(
        def.default_active,
        "the todo tool is active on registration"
    );

    // The tool object's prompt contributions reach the system prompt builder
    // through `ToolInfo::from_agent_tool`.
    let ToolBackend::Rust(tool) = registry
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == TOOL_NAME)
        .map(|(_, backend)| backend)
        .expect("backend")
    else {
        panic!("todo must be a native Rust tool");
    };
    assert!(tool
        .prompt_snippet()
        .is_some_and(|snippet| !snippet.is_empty()));
    assert_eq!(tool.prompt_guidelines().len(), 8);
    assert!(
        !tool.supports_background(),
        "a detached todo call would land after its turn"
    );
    assert!(!tool.is_mutation_tool(), "the list is not a file mutation");
}

/// The host's `has_handlers` gate must keep this plugin off every per-delta or
/// high-frequency event: only `session_start` (once per session) is declared,
/// so `message_update` AND the per-batch `tool_result` fan-out never reach it
/// through the gated loop paths (zero per-delta and zero per-result work at
/// the dispatch layer, not just inside the handler). The live tool commit is
/// authoritative; declaring `tool_result` would make the agent loop materialize
/// and clone every result's payload just to re-apply the same state.
#[test]
fn events_declare_only_cheap_session_events() {
    let as_plugin: Arc<dyn AgentPlugin> = plugin();
    let events: EventInterest = as_plugin.events();
    assert_eq!(
        events,
        EventInterest::List(vec!["session_start".to_owned()])
    );
    assert!(events.matches("session_start"));
    assert!(
        !events.matches("tool_result"),
        "the result fan-out must stay off: the live commit is authoritative"
    );
    for unrelated in [
        "message_update",
        "message_end",
        "message_start",
        "tool_result",
        "tool_call",
        "tool_execution_update",
        "before_provider_request",
        "provider_stream_event",
        "entry_appended",
    ] {
        assert!(!events.matches(unrelated), "{unrelated} must not match");
    }

    let host = host_with(plugin());
    assert!(host.has_handlers("session_start"));
    assert!(
        !host.has_handlers("tool_result"),
        "a per-result event must not reach this plugin"
    );
    assert!(
        !host.has_handlers("message_update"),
        "a per-delta event must not reach this plugin"
    );
}

/// P1-5: with todo no longer declaring `tool_result`, a native-only host does
/// not switch the agent loop's per-batch hosted result path on
/// (`agent_loop.rs` gates it on `has_native_handlers("tool_result")`). The
/// builder in `emit_tool_results_skipping_with_host` is therefore never
/// reached for a non-todo result, so a 1 MiB `details` payload is not cloned.
///
/// The direct-dispatch branch is still correct (and still applies a matching
/// `todo` envelope) — pinned by
/// `tool_result_envelope_is_an_idempotent_second_source`.
#[test]
fn todo_does_not_enable_the_hosted_tool_result_path() {
    let plugin = plugin();
    let host = host_with(plugin);
    assert!(
        host.js_handle().is_none(),
        "this test pins the native-only (no JS runtime) session shape"
    );
    assert!(
        !host.has_native_handlers("tool_result"),
        "todo must not be the reason every batch builds a result payload"
    );
    assert!(
        !host.has_handlers("tool_result"),
        "no face of this host consumes tool results"
    );

    // The exact payload the hosted builder would clone (a non-todo result with
    // a 1 MiB details string). Nothing here reaches the builder: the gate
    // above is the agent loop's `wants_native_results`.
    let big = "x".repeat(1024 * 1024);
    let payload = json!({
        "type": "tool_result",
        "toolName": "bash",
        "content": [{ "type": "text", "text": "done" }],
        "details": { "output": big },
    });
    assert_eq!(
        payload["details"]["output"].as_str().map(str::len),
        Some(1024 * 1024)
    );
}

// ---------------------------------------------------------------------------
// CPU invariants
// ---------------------------------------------------------------------------

/// Emitting thousands of unrelated events through the unified dispatch must
/// not touch a single counter: no state access, no scan, no growth with the
/// event count.
#[tokio::test(flavor = "current_thread")]
async fn unrelated_events_touch_no_state_and_do_not_grow_cost() {
    let plugin = plugin();
    let host = host_with(Arc::clone(&plugin));
    // Seed a non-empty state so a stray replay would be observable.
    let store = Arc::clone(plugin.store());
    store.set_active_session("session-a");
    store.replace_state(
        "session-a",
        TodoState {
            tasks: vec![Task {
                id: 1,
                subject: "seed".to_owned(),
                status: TaskStatus::Pending,
                ..Task::default()
            }],
            next_id: 2,
        },
    );
    let before = counters(&plugin);

    for index in 0..2000 {
        let mut payload = json!({
            "type": "message_update",
            "delta": format!("token-{index}"),
        });
        host.emit("message_update", &mut payload)
            .await
            .expect("emit message_update");
        let mut payload = json!({ "type": "tool_result", "toolName": TOOL_NAME, "details": {} });
        host.emit("tool_result", &mut payload).await.expect("emit");
        let mut payload = json!({ "type": "tool_call", "toolName": TOOL_NAME, "input": {} });
        host.emit("tool_call", &mut payload).await.expect("emit");
    }

    assert_eq!(
        counters(&plugin),
        before,
        "unrelated events must not push, scan, replay, or touch state"
    );
    assert_eq!(plugin.state().tasks.len(), 1, "the seeded state stands");
}

/// The defensive `tool_result` seam: a well-formed `todo` envelope REPLACES
/// the active slot (idempotent — the same value the tool commit published),
/// while another tool's result and a shape-mismatched payload touch nothing.
///
/// VERDICT (audit): the product is right and the old expectation here was
/// stale — but for a fence reason, not an idempotence bug. `TodoPlugin::events`
/// declares only `session_start` (deliberate: the live tool commit already
/// published this state), and `PluginHost::emit` honors that declaration via
/// `native_plugin_wants_event`. So `host.emit("tool_result", ..)` NEVER reaches
/// the plugin's defensive arm; the zero here was the DECLARATION FENCE doing
/// its job, and the arm is reachable only through the one deliberately
/// unfiltered dispatcher, `agent::tool_execution::dispatch_hosted_event`
/// (pinned by `plugin_events_w16::hosted_tool_result_walk_ignores_the_declaration_fence`).
/// Driving the real seam is also what makes the idempotence claim meaningful:
/// it feeds exactly the payload the agent loop builds (`toolCallId`, `content`,
/// `isError`, `details`) through a `FinalizedToolCall`.
#[tokio::test(flavor = "current_thread")]
async fn tool_result_envelope_is_an_idempotent_second_source() {
    let plugin = plugin();
    let host = host_with(Arc::clone(&plugin));
    let store = Arc::clone(plugin.store());
    store.set_active_session("s");
    store.replace_state("s", TodoState::empty());

    // Produce a real envelope through the tool. `run_tool` already committed
    // it, so this snapshot is the live value the second source must reproduce.
    let result = run_tool(&host, json!({ "action": "create", "subject": "from tool" })).await;
    let details = result.details.clone().expect("details envelope");

    // Wipe the slot so the assertions below prove the envelope RESTORES it,
    // rather than riding on the tool's own commit.
    store.replace_state("s", TodoState::empty());
    assert!(plugin.state().tasks.is_empty());

    let envelope = |details: Value| FinalizedToolCall {
        index: 0,
        call_id: "call-envelope".to_owned(),
        tool_name: TOOL_NAME.to_owned(),
        result: ToolResult {
            details: Some(details),
            ..ToolResult::ok(result.content.clone())
        },
        usage: None,
        duration_ms: None,
    };

    // (1) The declaration fence: the ordered fan-out is NOT the second source.
    let mut payload =
        json!({ "type": "tool_result", "toolName": TOOL_NAME, "details": details.clone() });
    host.emit("tool_result", &mut payload).await.expect("emit");
    assert!(
        plugin.state().tasks.is_empty(),
        "emit() honors the declaration fence, so the defensive arm stays unreached"
    );

    // (2) The hosted walk DOES reach it, and the envelope snapshot is applied.
    deliver_hosted_result(&host, envelope(details.clone())).await;
    assert_eq!(
        plugin.state().tasks.len(),
        1,
        "the envelope snapshot must be applied through the hosted walk"
    );

    // (3) A duplicate is a whole-state REPLACE: one more state touch, still
    // one task — never an append (that is the idempotence the name claims).
    let touched = counters(&plugin).state_touches;
    deliver_hosted_result(&host, envelope(details.clone())).await;
    assert_eq!(
        counters(&plugin).state_touches,
        touched + 1,
        "a duplicate is an idempotent replace, never an append"
    );
    assert_eq!(plugin.state().tasks.len(), 1);

    // (4) Another tool's result, and a `todo` payload whose details do not
    // match the persistence shape (`nextId` missing), leave every counter
    // alone — the `toolName` and shape guards run before any state write.
    let before = counters(&plugin);
    deliver_hosted_result(
        &host,
        FinalizedToolCall {
            tool_name: "bash".to_owned(),
            ..envelope(details)
        },
    )
    .await;
    deliver_hosted_result(&host, envelope(json!({ "tasks": [] }))).await;
    assert_eq!(counters(&plugin), before);
}

/// Deliver one finalized result through the hosted `tool_result` walk — the
/// ONE dispatcher that reaches an undeclared plugin arm
/// (`dispatch_hosted_event` deliberately ignores `EventInterest`; see
/// `plugin_events_w16`). `host.emit` would be filtered out for `todo`.
async fn deliver_hosted_result(host: &PluginHost, finalized: FinalizedToolCall) {
    let _ = pir::agent::tool_execution::emit_tool_results_with_host(
        None,
        Some(host),
        &HashMap::new(),
        std::slice::from_ref(&finalized),
        None,
    )
    .await;
}

/// The per-push cost: an unchanged push is fingerprint-skipped (no state
/// touch), a changed push is one entries pass, and READ-ONLY state does not
/// grow with repeated identical pushes.
#[test]
fn repeated_pushes_are_fingerprint_skipped() {
    let store = TodoStore::new();
    store.set_active_session("s1");
    let entries = json!([]);
    let slice: Vec<Value> = entries.as_array().cloned().unwrap_or_default();
    assert!(
        store.replay("s1", &slice),
        "the first push installs the state"
    );
    for _ in 0..1000 {
        assert!(!store.replay("s1", &slice), "identical pushes skip");
    }
    let stats = store.stats().snapshot();
    assert_eq!(stats.state_touches, 1);
    assert_eq!(stats.replay_skips, 1000);
    // The cheap pass runs once per push (that IS the fingerprint), but the
    // entry-visit count stays proportional to the transcript, never to the
    // number of pushes: an empty transcript visits zero entries.
    assert_eq!(stats.entries_visited, 0);
}

/// P1-8: replay is two-phase. A transcript with 100 historical `todo` results
/// (each embedding the full 100-task snapshot) deserializes exactly ONE
/// snapshot on the authoritative push and ZERO on an unchanged re-push. An
/// edit to the last snapshot that keeps its `toolCallId`/`timestamp` is still
/// detected (the fingerprint hashes the snapshot content) and costs exactly
/// one more deserialization.
#[test]
fn replay_deserializes_only_the_last_snapshot() {
    // 100 tasks, so each historical result embeds the full O(tasks) snapshot.
    let mut state = TodoState::empty();
    for index in 0..100 {
        let op = call(
            &mut state,
            json!({ "action": "create", "subject": format!("task {index}") }),
        );
        assert_eq!(
            op.content,
            format!("Created #{}: task {index} (pending)", index + 1)
        );
    }
    let result = call(&mut state, json!({ "action": "list" }));
    let base = entry_from(&result, "call-0", 0);

    // 100 historical results with the SAME state (the id/timestamp stream is
    // what distinguishes them); the last write wins.
    let entries: Vec<Value> = (0..100)
        .map(|index| {
            let mut entry = base.clone();
            entry["toolCallId"] = json!(format!("call-{index}"));
            entry["timestamp"] = json!(index + 1);
            entry
        })
        .collect();

    let store = TodoStore::new();
    store.set_active_session("s1");
    assert!(
        store.replay("s1", &entries),
        "the first push installs state"
    );
    assert_eq!(store.state("s1"), state);
    assert_eq!(
        store.stats().snapshot().deserializations,
        1,
        "only the LAST matching snapshot is deserialized"
    );

    // Unchanged re-push: the fingerprint match is the whole cost.
    assert!(!store.replay("s1", &entries), "identical pushes skip");
    assert_eq!(
        store.stats().snapshot().deserializations,
        1,
        "an unchanged re-push deserializes nothing"
    );
    assert_eq!(store.state("s1"), state);

    // Edit the LAST snapshot in place, keeping its call id and timestamp:
    // only the content hash can move the fingerprint.
    let mut changed_state = state.clone();
    call(
        &mut changed_state,
        json!({ "action": "update", "id": 1, "status": "completed" }),
    );
    let changed_result = call(&mut changed_state, json!({ "action": "list" }));
    let mut changed = entry_from(&changed_result, "call-99", 100);
    changed["toolCallId"] = json!("call-99");
    changed["timestamp"] = json!(100);
    let mut edited = entries.clone();
    edited[99] = changed;

    assert!(
        store.replay("s1", &edited),
        "the edited snapshot must apply"
    );
    assert_eq!(
        store.stats().snapshot().deserializations,
        2,
        "a moved fingerprint costs exactly one more deserialization"
    );
    assert_eq!(store.state("s1"), changed_state);
}

/// Transcript persistence: the state a tool call persisted is restored in a
/// FRESH plugin instance from the entries alone (the `/reload` / `/resume` /
/// compaction path), and the replay is idempotent.
#[tokio::test(flavor = "current_thread")]
async fn transcript_entries_restore_state_in_a_fresh_instance() {
    let writer = plugin();
    let host = host_with(Arc::clone(&writer));
    let mut entries: Vec<Value> = Vec::new();

    let result = run_tool(&host, json!({ "action": "create", "subject": "first" })).await;
    assert_eq!(result.content, "Created #1: first (pending)");
    entries.push(entry_from(&result, "call-1", 100));

    let result = run_tool(
        &host,
        json!({ "action": "create", "subject": "second", "blockedBy": [1] }),
    )
    .await;
    entries.push(entry_from(&result, "call-2", 200));

    let result = run_tool(
        &host,
        json!({
            "action": "update",
            "id": 2,
            "status": "in_progress",
            "activeForm": "writing tests",
            "metadata": { "branch": "main" }
        }),
    )
    .await;
    assert_eq!(result.content, "Updated #2 (pending → in_progress)");
    entries.push(entry_from(&result, "call-3", 300));

    let expected = writer.state();
    assert_eq!(expected.tasks.len(), 2);
    assert_eq!(expected.next_id, 3);

    // A fresh instance (new session host) replays from the transcript alone.
    let reader = plugin();
    reader.on_session_context(&snapshot(json!(entries), Some("session-1")));
    assert_eq!(reader.state(), expected, "state must survive a reload");
    // Idempotent: the same push again scans once but replaces nothing.
    let before = counters(&reader);
    reader.on_session_context(&snapshot(json!(entries), Some("session-1")));
    let after = counters(&reader);
    assert_eq!(after.scans, before.scans + 1, "one entries pass per push");
    assert_eq!(
        after.state_touches, before.state_touches,
        "an unchanged fingerprint must not replace the state"
    );
    assert_eq!(after.replay_skips, before.replay_skips + 1);
    assert_eq!(reader.state(), expected);

    // ... and so does a second fresh instance on a compacted transcript whose
    // newest snapshot survives the cut.
    let compacted = json!([entries[2].clone()]);
    let reader = plugin();
    reader.on_session_context(&snapshot(compacted, Some("session-1")));
    assert_eq!(reader.state(), expected);
}

/// A model-only push (a host without a JS runtime pushes the model alone)
/// must not wipe live state; session ids key separate slots.
#[test]
fn model_only_pushes_keep_live_state_and_key_sessions() {
    let plugin = plugin();
    let host = host_with(Arc::clone(&plugin));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");
    // Session a's transcript starts empty; the live state lands in its slot.
    plugin.on_session_context(&snapshot(json!([]), Some("session-a")));
    runtime.block_on(async {
        run_tool(&host, json!({ "action": "create", "subject": "live" })).await;
    });
    assert_eq!(plugin.state().tasks.len(), 1);

    // No session id, no entries: nothing changes (a model-only host push).
    let pushed = counters(&plugin);
    plugin.on_session_context(&SessionContextSnapshot::default());
    assert_eq!(
        counters(&plugin).pushes,
        pushed.pushes + 1,
        "the push is counted"
    );
    assert_eq!(
        counters(&plugin).scans,
        pushed.scans,
        "no entries → no scan"
    );
    assert_eq!(plugin.state().tasks.len(), 1, "live state stands");

    // A different session id + its own (empty) transcript becomes the active
    // slot; the old slot is untouched, not merged.
    plugin.on_session_context(&snapshot(json!([]), Some("session-b")));
    assert!(plugin.state().tasks.is_empty(), "session-b starts empty");

    // Back on session a, the unchanged transcript is fingerprint-skipped, so
    // the live state survives.
    plugin.on_session_context(&snapshot(json!([]), Some("session-a")));
    assert_eq!(plugin.state().tasks.len(), 1, "session-a is still there");
}

// ---------------------------------------------------------------------------
// `/todos`
// ---------------------------------------------------------------------------

/// The command returns the grouped list on a UI session and the upstream
/// rejection without one.
#[tokio::test(flavor = "current_thread")]
async fn todos_command_groups_by_status_through_the_plugin_registry() {
    let plugin = plugin();
    let host = host_with(Arc::clone(&plugin));

    // No UI (an inline-only host): the upstream rejection.
    let outcome = host
        .dispatch_command(COMMAND_NAME, "")
        .await
        .expect("dispatch");
    assert!(!outcome.ok);
    assert_eq!(outcome.error.as_deref(), Some(ERR_REQUIRES_INTERACTIVE));

    run_tool(&host, json!({ "action": "create", "subject": "one" })).await;
    run_tool(&host, json!({ "action": "create", "subject": "two" })).await;
    run_tool(
        &host,
        json!({ "action": "update", "id": 2, "status": "completed" }),
    )
    .await;

    // With a UI session (`has_ui`), the text channel carries the list.
    let outcome = todos_command_outcome(plugin.store(), true);
    assert!(outcome.ok);
    let text = outcome.error.expect("command text");
    assert_eq!(
        text,
        "1/2 completed · 1 pending\n── Pending ──\n  ○ #1 one\n── Completed ──\n  ✓ #2 two"
    );

    // Empty list text.
    let empty_store = TodoStore::new();
    empty_store.set_active_session("s");
    assert_eq!(
        todos_command_outcome(&empty_store, true).error.as_deref(),
        Some(MSG_NO_TODOS)
    );
}

/// The real tool path for inputs the JSON Schema would reject: an unknown
/// action and a wrong-typed field both come back in-band, with the task state
/// untouched.
#[tokio::test(flavor = "current_thread")]
async fn schema_impossible_inputs_are_in_band_errors() {
    let plugin = plugin();
    let host = host_with(Arc::clone(&plugin));

    let result = run_tool(&host, json!({ "action": "explode" })).await;
    assert!(
        !result.is_error,
        "rejections ride the content, not is_error"
    );
    assert_eq!(
        result.content,
        "Error: invalid params: unknown action \"explode\""
    );
    let details = result.details.expect("details envelope");
    assert_eq!(details["action"], json!("explode"));
    assert_eq!(
        details["error"],
        json!("invalid params: unknown action \"explode\"")
    );
    assert_eq!(details["nextId"], json!(1));

    let result = run_tool(&host, json!({ "action": "create", "subject": 7 })).await;
    assert_eq!(
        result.content,
        "Error: invalid params: subject must be a string"
    );
    assert!(
        plugin.state().tasks.is_empty(),
        "a rejected call writes nothing"
    );

    // The supported actions still work afterwards (no state corruption).
    let result = run_tool(&host, json!({ "action": "create", "subject": "ok" })).await;
    assert_eq!(result.content, "Created #1: ok (pending)");
}

// ---------------------------------------------------------------------------
// Envelope / replay unit-level integration
// ---------------------------------------------------------------------------

/// The envelope's `details` round-trips through the replay filter: an error
/// envelope replays as the unchanged state it embeds, and the tool name is
/// the only filter key.
#[test]
fn envelope_replays_through_the_transcript_filter() {
    let mut state = TodoState::empty();
    let create = json!({ "action": "create", "subject": "a" });
    let params = TodoParams::parse(&create).expect("params");
    let op = apply_mutation(&mut state, TaskAction::Create, &params);
    let result = build_tool_result(TaskAction::Create, &create, &state, &op);
    let good = entry_from(&result, "call-1", 1);

    // A rejected call embeds the unchanged snapshot and replays to it.
    let bad_args = json!({ "action": "update", "id": 7 });
    let params = TodoParams::parse(&bad_args).expect("params");
    let op = apply_mutation(&mut state, TaskAction::Update, &params);
    assert!(matches!(op, TodoOp::Error { .. }));
    let rejected = build_tool_result(TaskAction::Update, &bad_args, &state, &op);
    let bad = entry_from(&rejected, "call-2", 2);

    let scan = replay_from_entries(&[good, bad]);
    assert_eq!(scan.state.as_ref(), Some(&state));
    assert_eq!(scan.matches, 2);

    // A `bash` result carrying a todo-shaped `details` never matches.
    let other = json!({
        "role": "toolResult",
        "toolName": "bash",
        "details": { "tasks": [], "nextId": 9 },
    });
    let scan = replay_from_entries(&[other]);
    assert_eq!(scan.state, None);
    assert_eq!(scan.matches, 0);
}

#[test]
fn state_serialization_matches_the_persisted_shape() {
    let state = TodoState {
        tasks: vec![Task {
            id: 1,
            subject: "first".to_owned(),
            description: Some("long form".to_owned()),
            active_form: Some("doing".to_owned()),
            status: TaskStatus::InProgress,
            blocked_by: Some(vec![2, 3]),
            owner: Some("agent".to_owned()),
            metadata: Some([("k".to_owned(), json!(1))].into_iter().collect()),
        }],
        next_id: 2,
    };
    let wire = serde_json::to_value(&state).expect("serialize");
    assert_eq!(
        wire,
        json!({
            "tasks": [{
                "id": 1,
                "subject": "first",
                "description": "long form",
                "activeForm": "doing",
                "status": "in_progress",
                "blockedBy": [2, 3],
                "owner": "agent",
                "metadata": { "k": 1 }
            }],
            "nextId": 2
        })
    );
    let round_tripped: TodoState = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(round_tripped, state);
}

// ---------------------------------------------------------------------------
// Reducer / state-machine / replay units (migrated verbatim from the former
// inline `#[cfg(test)] mod tests` in `src/plugins/builtin/todo.rs` — the
// hygiene gate keeps every test under `tests/`).
// ---------------------------------------------------------------------------

fn params(value: Value) -> TodoParams {
    TodoParams::parse(&value).expect("valid params")
}

fn state_with(subjects: &[&str]) -> TodoState {
    let mut state = TodoState::empty();
    for subject in subjects {
        let result = apply_mutation(
            &mut state,
            TaskAction::Create,
            &params(json!({ "subject": subject })),
        );
        assert!(matches!(result, TodoOp::Create { .. }), "{result:?}");
    }
    state
}

fn error_message(op: &TodoOp) -> String {
    match op {
        TodoOp::Error { message } => message.clone(),
        other => panic!("expected an error op, got {other:?}"),
    }
}

// -- status machine -----------------------------------------------------

#[test]
fn transition_table_matches_upstream() {
    for (from, allowed) in [
        (
            TaskStatus::Pending,
            vec![
                TaskStatus::Pending,
                TaskStatus::InProgress,
                TaskStatus::Completed,
                TaskStatus::Deleted,
            ],
        ),
        (
            TaskStatus::InProgress,
            vec![
                TaskStatus::InProgress,
                TaskStatus::Pending,
                TaskStatus::Completed,
                TaskStatus::Deleted,
            ],
        ),
        (
            TaskStatus::Completed,
            vec![TaskStatus::Completed, TaskStatus::Deleted],
        ),
        (TaskStatus::Deleted, vec![TaskStatus::Deleted]),
    ] {
        for to in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Deleted,
        ] {
            assert_eq!(
                is_transition_valid(from, to),
                allowed.contains(&to),
                "{from} → {to}"
            );
        }
    }
    assert_eq!(TaskStatus::InProgress.as_str(), "in_progress");
    assert_eq!(
        TaskStatus::parse("in_progress"),
        Some(TaskStatus::InProgress)
    );
    assert_eq!(TaskStatus::parse("bogus"), None);
}

#[test]
fn illegal_transition_is_rejected_and_reported_verbatim() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "completed" })),
    );
    assert!(matches!(op, TodoOp::Update { .. }));
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress" })),
    );
    assert_eq!(
        error_message(&op),
        "illegal transition completed → in_progress"
    );
    assert_eq!(state.tasks[0].status, TaskStatus::Completed);
}

#[test]
fn same_status_update_is_a_reported_no_op() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress" })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress" })),
    );
    match &op {
        TodoOp::Update { changed, .. } => assert!(!changed),
        other => panic!("expected update, got {other:?}"),
    }
    assert_eq!(
        format_content(&op, &state),
        "No change: #1 already matches the requested values (status: in_progress)"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "subject": "renamed" })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    assert_eq!(format_content(&op, &state), "Updated #1");
    assert_eq!(state.tasks[0].subject, "renamed");
    // A field re-sent unchanged is a no-op too.
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "subject": "renamed" })),
    );
    match &op {
        TodoOp::Update { changed, .. } => assert!(!changed),
        other => panic!("expected update, got {other:?}"),
    }
    assert_eq!(
        format_content(&op, &state),
        "No change: #1 already matches the requested values (status: in_progress)"
    );
}

// -- create / delete / clear -------------------------------------------

#[test]
fn create_assigns_sequential_ids_and_replays_content_string() {
    let mut state = TodoState::empty();
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "Write the parser" })),
    );
    assert_eq!(
        format_content(&op, &state),
        "Created #1: Write the parser (pending)"
    );
    apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "second" })),
    );
    apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "third" })),
    );
    assert_eq!(state.next_id, 4);
    assert_eq!(
        state.tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn create_requires_a_non_blank_subject() {
    let mut state = TodoState::empty();
    for value in [json!({}), json!({ "subject": "   " })] {
        let op = apply_mutation(&mut state, TaskAction::Create, &params(value));
        assert_eq!(error_message(&op), "subject required for create");
    }
    assert!(state.tasks.is_empty());
    assert_eq!(state.next_id, 1);
}

#[test]
fn delete_tombstones_and_clear_resets_the_id_counter() {
    let mut state = state_with(&["a", "b"]);
    let op = apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 1 })));
    assert_eq!(format_content(&op, &state), "Deleted #1: a");
    let op = apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 1 })));
    assert_eq!(error_message(&op), "#1 is already deleted");
    assert_eq!(state.tasks[0].status, TaskStatus::Deleted);
    // The tombstone still resolves for dependency edges.
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "c", "blockedBy": [1] })),
    );
    assert_eq!(error_message(&op), "blockedBy: #1 is deleted");

    let op = apply_mutation(&mut state, TaskAction::Clear, &params(json!({})));
    assert_eq!(format_content(&op, &state), "Cleared 2 tasks");
    assert!(state.tasks.is_empty());
    assert_eq!(state.next_id, 1);
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "fresh" })),
    );
    match op {
        TodoOp::Create { task_id } => assert_eq!(task_id, 1, "ids restart after clear"),
        other => panic!("expected create, got {other:?}"),
    }
}

#[test]
fn missing_ids_report_verbatim_errors() {
    let mut state = state_with(&["a"]);
    assert_eq!(
        error_message(&apply_mutation(
            &mut state,
            TaskAction::Get,
            &params(json!({ "id": 9 }))
        )),
        "#9 not found"
    );
    assert_eq!(
        error_message(&apply_mutation(
            &mut state,
            TaskAction::Update,
            &params(json!({ "id": 9, "status": "pending" }))
        )),
        "#9 not found"
    );
    assert_eq!(
        error_message(&apply_mutation(
            &mut state,
            TaskAction::Delete,
            &params(json!({ "id": 9 }))
        )),
        "#9 not found"
    );
    for (action, expected) in [
        (TaskAction::Update, "id required for update"),
        (TaskAction::Get, "id required for get"),
        (TaskAction::Delete, "id required for delete"),
    ] {
        assert_eq!(
            error_message(&apply_mutation(&mut state, action, &params(json!({})))),
            expected
        );
    }
}

#[test]
fn update_requires_a_mutable_field() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(&mut state, TaskAction::Update, &params(json!({ "id": 1 })));
    assert_eq!(
        error_message(&op),
        "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy"
    );
}

// -- dependencies -------------------------------------------------------

#[test]
fn create_blocked_by_validates_dangling_and_deleted() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "b", "blockedBy": [2] })),
    );
    assert_eq!(error_message(&op), "blockedBy: #2 not found");

    let mut state = state_with(&["a"]);
    apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 1 })));
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "b", "blockedBy": [1] })),
    );
    assert_eq!(error_message(&op), "blockedBy: #1 is deleted");
}

#[test]
fn update_add_and_remove_blocked_by_are_additive() {
    let mut state = state_with(&["a", "b", "c"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "addBlockedBy": [1, 2] })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    assert_eq!(state.tasks[2].blocked_by, Some(vec![1, 2]));
    // Re-adding an existing id is a no-op (dedup).
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "addBlockedBy": [1] })),
    );
    assert_eq!(state.tasks[2].blocked_by, Some(vec![1, 2]));
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "removeBlockedBy": [1] })),
    );
    assert_eq!(state.tasks[2].blocked_by, Some(vec![2]));
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "removeBlockedBy": [2] })),
    );
    assert_eq!(
        state.tasks[2].blocked_by, None,
        "an emptied set drops the field"
    );
}

#[test]
fn self_block_and_unknown_or_deleted_dependency_are_rejected() {
    let mut state = state_with(&["a", "b"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [1] })),
    );
    assert_eq!(error_message(&op), "cannot block #1 on itself");
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [7] })),
    );
    assert_eq!(error_message(&op), "addBlockedBy: #7 not found");
    apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 2 })));
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [2] })),
    );
    assert_eq!(error_message(&op), "addBlockedBy: #2 is deleted");
}

#[test]
fn cycles_are_rejected_before_state_changes() {
    let mut state = state_with(&["a", "b", "c"]);
    // 2 blockedBy 1
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "addBlockedBy": [1] })),
    );
    // 3 blockedBy 2
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "addBlockedBy": [2] })),
    );
    let before = state.clone();
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [3] })),
    );
    assert_eq!(
        error_message(&op),
        "addBlockedBy would create a cycle in the blockedBy graph"
    );
    assert_eq!(state, before, "a rejected call leaves the list untouched");
}

#[test]
fn detect_cycle_handles_long_chains_without_recursion() {
    let mut state = TodoState::empty();
    for index in 0..MAX_TASKS_PER_SESSION {
        // Task ids are assigned in order, so the task created at loop
        // index `i >= 1` has id `i + 1` and depends on id `i`: one long
        // chain, no cycles.
        let blocked = if index == 0 {
            json!({ "subject": format!("task {index}") })
        } else {
            json!({ "subject": format!("task {index}"), "blockedBy": [index] })
        };
        apply_mutation(&mut state, TaskAction::Create, &params(blocked));
    }
    // Closing the chain must be detected, and the depth must not overflow
    // the stack (iterative DFS).
    assert!(detect_cycle(
        &state.tasks,
        1,
        &[MAX_TASKS_PER_SESSION as u64]
    ));
    // A dependency on an EARLIER task (already reachable through the chain)
    // adds no path back to task 3: no cycle.
    assert!(!detect_cycle(&state.tasks, 3, &[1]));
    // Task 3 sits INSIDE the chain (4 depends on 3 … 500 depends on 499), so
    // making it depend on 500 closes a loop: 3 → 500 → … → 4 → 3.
    assert!(detect_cycle(&state.tasks, 3, &[500]));
}

#[test]
fn derive_blocks_reports_reverse_edges() {
    let mut state = state_with(&["a", "b"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "addBlockedBy": [1] })),
    );
    let blocks = derive_blocks(&state.tasks);
    assert_eq!(blocks.get(&1), Some(&vec![2]));
    assert_eq!(
        format_content(&TodoOp::Get { id: 1 }, &state),
        "#1 [pending] a\n  blocks: #2"
    );
}

// -- metadata -----------------------------------------------------------

#[test]
fn metadata_merges_key_by_key_and_null_deletes() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": "main", "pr": 42 } })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    assert_eq!(
        state.tasks[0].metadata,
        Some(
            [
                ("branch".to_owned(), json!("main")),
                ("pr".to_owned(), json!(42))
            ]
            .into_iter()
            .collect()
        )
    );
    // A null value deletes one key and keeps the other.
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "pr": null } })),
    );
    assert_eq!(
        state.tasks[0].metadata,
        Some([("branch".to_owned(), json!("main"))].into_iter().collect())
    );
    // Emptying the record drops the field entirely.
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": null } })),
    );
    assert_eq!(state.tasks[0].metadata, None);
    // A metadata-only update of the same values reports "no change".
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": "main" } })),
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": "main" } })),
    );
    match op {
        TodoOp::Update { changed, .. } => assert!(!changed),
        other => panic!("expected update, got {other:?}"),
    }
}

// -- bounds -------------------------------------------------------------

#[test]
fn record_and_session_bounds_are_enforced() {
    let mut state = TodoState::empty();
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "x".repeat(MAX_SUBJECT_CHARS + 1) })),
    );
    assert_eq!(
        error_message(&op),
        format!("subject too long (limit {MAX_SUBJECT_CHARS} characters)")
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "ok", "owner": "y".repeat(MAX_OWNER_CHARS + 1) })),
    );
    assert_eq!(
        error_message(&op),
        format!("owner too long (limit {MAX_OWNER_CHARS} characters)")
    );
    let big = "z".repeat(MAX_METADATA_BYTES + 1);
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "ok", "metadata": { "blob": big } })),
    );
    assert_eq!(
        error_message(&op),
        format!("metadata too large (limit {MAX_METADATA_BYTES} bytes per task)")
    );
    assert!(state.tasks.is_empty(), "a rejected create writes nothing");

    // The session cap: fill the list, then one more create is rejected.
    let mut state = TodoState::empty();
    state.tasks = (1..=MAX_TASKS_PER_SESSION as u64)
        .map(|id| Task {
            id,
            subject: "t".to_owned(),
            ..Task::default()
        })
        .collect();
    state.next_id = MAX_TASKS_PER_SESSION as u64 + 1;
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "one too many" })),
    );
    assert_eq!(
        error_message(&op),
        format!("task limit reached (limit {MAX_TASKS_PER_SESSION} tasks per session)")
    );
    assert_eq!(state.tasks.len(), MAX_TASKS_PER_SESSION);
}

// -- list / get envelope ------------------------------------------------

#[test]
fn list_filters_and_renders_rows() {
    let mut state = state_with(&["a", "b", "c"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "status": "in_progress", "activeForm": "writing tests" })),
    );
    apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 3 })));

    let op = apply_mutation(&mut state, TaskAction::List, &params(json!({})));
    assert_eq!(
        format_content(&op, &state),
        "[pending] #1 a\n[in_progress] #2 b (writing tests)"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::List,
        &params(json!({ "includeDeleted": true })),
    );
    assert_eq!(
        format_content(&op, &state),
        "[pending] #1 a\n[in_progress] #2 b (writing tests)\n[deleted] #3 c"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::List,
        &params(json!({ "status": "completed" })),
    );
    assert_eq!(format_content(&op, &state), "No tasks");
    let mut empty = TodoState::empty();
    let op = apply_mutation(&mut empty, TaskAction::List, &params(json!({})));
    assert_eq!(format_content(&op, &empty), "No tasks");
}

#[test]
fn list_rows_carry_the_dependency_chain() {
    let mut state = state_with(&["a", "b"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "addBlockedBy": [1] })),
    );
    let op = apply_mutation(&mut state, TaskAction::List, &params(json!({})));
    assert_eq!(
        format_content(&op, &state),
        "[pending] #1 a\n[pending] #2 b ⛓ #1"
    );
}

#[test]
fn get_renders_every_field_in_upstream_order() {
    let mut state = state_with(&["a", "b"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({
            "id": 1,
            "description": "long form",
            "activeForm": "working",
            "status": "in_progress",
            "owner": "agent-1",
            "addBlockedBy": [2]
        })),
    );
    let op = apply_mutation(&mut state, TaskAction::Get, &params(json!({ "id": 1 })));
    assert_eq!(
        format_content(&op, &state),
        "#1 [in_progress] a\n  description: long form\n  activeForm: working\n  blockedBy: #2\n  owner: agent-1"
    );
}

// -- envelope -----------------------------------------------------------

#[test]
fn envelope_carries_the_full_snapshot_and_the_bare_error() {
    let mut state = state_with(&["a"]);
    let args = json!({ "action": "create", "subject": "b" });
    let op = apply_mutation(&mut state, TaskAction::Create, &params(args.clone()));
    let result = build_tool_result(TaskAction::Create, &args, &state, &op);
    assert!(
        !result.is_error,
        "rejections are in-band, never transport errors"
    );
    assert_eq!(result.content, "Created #2: b (pending)");
    let details = result.details.expect("details envelope");
    assert_eq!(details["action"], json!("create"));
    assert_eq!(details["params"], args);
    assert_eq!(details["nextId"], json!(3));
    assert_eq!(details["tasks"].as_array().map(Vec::len), Some(2));
    assert_eq!(details["tasks"][0]["subject"], json!("a"));
    assert!(details.get("error").is_none());

    let args = json!({ "action": "update", "id": 1 });
    let op = apply_mutation(&mut state, TaskAction::Update, &params(args.clone()));
    let result = build_tool_result(TaskAction::Update, &args, &state, &op);
    let details = result.details.expect("details envelope");
    assert_eq!(
        details["error"],
        json!("update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy")
    );
    assert_eq!(
        result.content,
        "Error: update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy"
    );
    // The rejected call embedded the unchanged (two-task) snapshot.
    assert_eq!(details["tasks"].as_array().map(Vec::len), Some(2));
    assert_eq!(details["nextId"], json!(3));
}

#[test]
fn task_wire_shape_omits_absent_optionals() {
    let task = Task {
        id: 3,
        subject: "Write the parser".to_owned(),
        description: None,
        active_form: Some("writing the parser".to_owned()),
        status: TaskStatus::InProgress,
        blocked_by: Some(vec![1, 2]),
        owner: None,
        metadata: None,
    };
    let wire = serde_json::to_value(&task).expect("serialize");
    assert_eq!(
        wire,
        json!({
            "id": 3,
            "subject": "Write the parser",
            "activeForm": "writing the parser",
            "status": "in_progress",
            "blockedBy": [1, 2]
        })
    );
    let round_tripped: Task = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(round_tripped, task);
}

// -- sanitizer ----------------------------------------------------------

#[test]
fn sanitizer_drops_control_sequences_and_flattens_layout() {
    assert_eq!(sanitize_terminal_text("plain"), "plain");
    assert_eq!(sanitize_terminal_text("a\u{1b}[31mred"), "ared");
    assert_eq!(sanitize_terminal_text("a\u{1b}]0;title\u{7}bc"), "abc");
    assert_eq!(sanitize_terminal_text("a\u{1b}]unterminated"), "a");
    assert_eq!(sanitize_terminal_text("a\u{9b}31mb"), "ab");
    assert_eq!(sanitize_terminal_text("a\nb\tc\rd"), "a b c d");
    assert_eq!(sanitize_terminal_text("a\u{202e}b\u{200e}c"), "abc");
    // NUL and DEL are dropped like every other control character: a terminal
    // ignores NUL, and truncating there would be C-string semantics the JS
    // upstream's sanitizer does not have.
    assert_eq!(sanitize_terminal_text("a\u{7f}b\u{0}c"), "abc");
    assert_eq!(sanitize_terminal_text("a\u{2028}b"), "a b");
}

#[test]
fn list_rows_sanitize_model_text() {
    let mut state = state_with(&["a\u{1b}[31mred"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress", "activeForm": "x\u{1b}[0my" })),
    );
    let op = apply_mutation(&mut state, TaskAction::List, &params(json!({})));
    assert_eq!(format_content(&op, &state), "[in_progress] #1 ared (xy)");
}

// -- replay -------------------------------------------------------------

/// The transcript entry a plain user turn produces (replay noise).
fn user_entry(text: &str) -> Value {
    use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
    let message = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    });
    serde_json::to_value(message).expect("entry serializes")
}

fn call(state: &mut TodoState, args: Value) -> pir::agent::tool_batch::ToolResult {
    let action = TaskAction::parse(args["action"].as_str().unwrap_or("")).expect("action");
    let params = TodoParams::parse(&args).expect("params");
    let op = apply_mutation(state, action, &params);
    build_tool_result(action, &args, state, &op)
}

#[test]
fn replay_takes_the_last_snapshot_and_ignores_unrelated_entries() {
    let mut state = TodoState::empty();
    let first = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let second = call(
        &mut state,
        json!({ "action": "update", "id": 1, "status": "completed" }),
    );
    let entries = vec![
        user_entry("plan this"),
        entry_from(&first, "call-1", 100),
        entry_from(&second, "call-2", 200),
    ];
    let scan = replay_from_entries(&entries);
    assert_eq!(scan.matches, 2);
    assert_eq!(scan.entries_visited, 3);
    assert_eq!(
        scan.deserializations, 1,
        "a one-shot scan deserializes only the last matching snapshot"
    );
    assert_eq!(scan.state.as_ref(), Some(&state));

    // Non-todo tool results and assistant/user messages are inert.
    let mut with_noise = entries.clone();
    with_noise.push(json!({ "role": "toolResult", "toolName": "bash", "details": { "tasks": [], "nextId": 1 } }));
    with_noise.push(json!({ "role": "assistant", "content": [] }));
    assert_eq!(
        replay_from_entries(&with_noise).state.as_ref(),
        Some(&state)
    );
}

#[test]
fn replay_is_idempotent_across_repeated_pushes() {
    let mut state = TodoState::empty();
    let result = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let entries = vec![user_entry("go"), entry_from(&result, "call-1", 42)];
    let store = TodoStore::new();
    store.set_active_session("s1");
    assert!(store.replay("s1", &entries));
    assert_eq!(store.state("s1"), state);
    // A second identical push touches nothing.
    assert!(!store.replay("s1", &entries));
    assert_eq!(store.state("s1"), state);
    let stats = store.stats().snapshot();
    assert_eq!(stats.scans, 2);
    assert_eq!(stats.replay_skips, 1);
    assert_eq!(stats.state_touches, 1);
}

#[test]
fn replay_handles_compaction_truncation_and_reset() {
    let mut state = TodoState::empty();
    let first = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let first_state = state.clone();
    let second = call(&mut state, json!({ "action": "create", "subject": "b" }));
    let entries = vec![
        user_entry("go"),
        entry_from(&first, "call-1", 1),
        entry_from(&second, "call-2", 2),
    ];
    let store = TodoStore::new();
    store.set_active_session("s1");
    assert!(store.replay("s1", &entries));
    assert_eq!(store.state("s1"), state);

    // Compaction truncates the newest snapshot: the surviving (older)
    // snapshot wins, exactly like upstream's last-write-wins walk.
    let truncated = vec![user_entry("go"), entry_from(&first, "call-1", 1)];
    assert!(store.replay("s1", &truncated));
    assert_eq!(store.state("s1"), first_state);

    // Truncating everything (all todo results dropped) resets to empty.
    assert!(store.replay("s1", &[user_entry("go")]));
    assert_eq!(store.state("s1"), TodoState::empty());
    assert_eq!(store.state("s1").next_id, 1, "ids restart with the reset");
}

#[test]
fn replay_after_a_live_commit_keeps_the_newer_state() {
    // The live tool commit is the state source; a push whose entries are
    // unchanged must not roll it back.
    let mut state = TodoState::empty();
    let result = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let entries = vec![entry_from(&result, "call-1", 1)];
    let store = TodoStore::new();
    store.set_active_session("s1");
    store.replay("s1", &entries);
    assert_eq!(store.state("s1"), state);

    // One more create through the tool, `entries` still stale.
    let result = call(&mut state, json!({ "action": "create", "subject": "b" }));
    // The live commit lands as a whole-state replace (the slot's replay
    // fingerprint is untouched), which is exactly what the tool's commit
    // does through the store.
    store.replace_state("s1", state.clone());
    assert!(!store.replay("s1", &entries), "stale entries are skipped");
    assert_eq!(store.state("s1"), state);

    // Once the transcript catches up, the replay is a no-op too.
    assert!(store.replay(
        "s1",
        &[entries[0].clone(), entry_from(&result, "call-2", 2)]
    ));
    assert_eq!(store.state("s1"), state);
}

#[test]
fn invalid_snapshot_shapes_are_skipped() {
    let entries = vec![
        json!({ "role": "toolResult", "toolName": TOOL_NAME, "details": { "nextId": 1 } }),
        json!({ "role": "toolResult", "toolName": TOOL_NAME, "details": { "tasks": "nope", "nextId": 1 } }),
        json!({ "role": "toolResult", "toolName": TOOL_NAME, "details": { "tasks": [{ "id": "x" }], "nextId": 1 } }),
    ];
    let scan = replay_from_entries(&entries);
    assert_eq!(scan.state, None);
    assert_eq!(scan.matches, 1, "only the shape-matching entry counts");
}

#[test]
fn invalid_params_are_reported_in_band() {
    // `TodoParams::parse` rejects wrong types with a documented message
    // (the JSON Schema rejects them before the tool runs; the strings are
    // for raw/embedder callers).
    assert_eq!(
        TodoParams::parse(&json!({ "subject": 5 })).expect_err("type error"),
        "invalid params: subject must be a string"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "id": -1 })).expect_err("negative id"),
        "invalid params: id must be a non-negative integer"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "status": "bogus" })).expect_err("bad status"),
        "invalid params: status must be one of pending, in_progress, completed, deleted"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "blockedBy": ["x"] })).expect_err("bad ids"),
        "invalid params: blockedBy must be an array of task ids"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "metadata": 4 })).expect_err("bad metadata"),
        "invalid params: metadata must be an object"
    );
    assert_eq!(
        TodoParams::parse(&Value::Null).expect_err("non-object"),
        "invalid params: expected an object"
    );
    // Unknown keys are ignored (the open params bag upstream accepts).
    assert!(TodoParams::parse(&json!({ "action": "list", "extra": true })).is_ok());
}

#[test]
fn unknown_action_renders_an_in_band_error() {
    let store = TodoStore::new();
    let session = store.active_session();
    let state = store.state(&session);
    let args = json!({ "action": "explode" });
    let op = TodoOp::Error {
        message: "invalid params: unknown action \"explode\"".to_owned(),
    };
    let result = build_tool_result(TaskAction::List, &args, &state, &op);
    assert_eq!(
        result.content,
        "Error: invalid params: unknown action \"explode\""
    );
    let details = result.details.expect("details");
    assert_eq!(
        details["error"],
        json!("invalid params: unknown action \"explode\"")
    );
}

#[test]
fn todos_command_groups_by_status_and_gates_on_ui() {
    let store = TodoStore::new();
    store.set_active_session("s1");
    // Non-interactive: the upstream rejection, no list.
    let outcome = todos_command_outcome(&store, false);
    assert!(!outcome.ok);
    assert_eq!(outcome.error.as_deref(), Some(ERR_REQUIRES_INTERACTIVE));

    // Empty list on an interactive session.
    let outcome = todos_command_outcome(&store, true);
    assert!(outcome.ok);
    assert_eq!(outcome.error.as_deref(), Some(MSG_NO_TODOS));

    // Populated: header counts + sections in (pending, in progress,
    // completed) order; tombstones are never listed.
    let mut state = TodoState::empty();
    for subject in ["one", "two", "three", "four", "five"] {
        call(
            &mut state,
            json!({ "action": "create", "subject": subject }),
        );
    }
    call(
        &mut state,
        json!({ "action": "update", "id": 2, "status": "in_progress", "activeForm": "working" }),
    );
    call(
        &mut state,
        json!({ "action": "update", "id": 3, "status": "completed" }),
    );
    call(&mut state, json!({ "action": "delete", "id": 4 }));
    call(
        &mut state,
        json!({ "action": "update", "id": 5, "addBlockedBy": [1] }),
    );
    store.replace_state("s1", state);
    let outcome = todos_command_outcome(&store, true);
    let text = outcome.error.expect("command text");
    let expected = [
        "1/4 completed · 1 in progress · 2 pending",
        "── Pending ──",
        "  ○ #1 one",
        "  ○ #5 five    ⛓ #1",
        "── In Progress ──",
        "  ◐ #2 two (working)",
        "── Completed ──",
        "  ✓ #3 three",
    ]
    .join("\n");
    assert_eq!(text, expected);
    assert!(!text.contains("#4"), "tombstones are never listed: {text}");
}

#[test]
fn todo_stats_track_scans_skips_and_touches() {
    let store = TodoStore::new();
    store.set_active_session("s1");
    let empty: Vec<Value> = Vec::new();
    assert!(store.replay("s1", &empty));
    assert!(!store.replay("s1", &empty));
    let stats = store.stats().snapshot();
    assert_eq!(stats.scans, 2);
    assert_eq!(stats.replay_skips, 1);
    assert_eq!(stats.state_touches, 1);
    assert_eq!(stats.entries_visited, 0);
}

#[test]
fn session_slots_are_bounded_by_lru() {
    let store = TodoStore::new();
    for index in 0..(MAX_SESSION_SLOTS + 3) {
        let session = format!("s{index}");
        store.set_active_session(&session);
        store.replace_state(&session, TodoState::empty());
    }
    let slots = store.session_slot_count();
    assert!(slots <= MAX_SESSION_SLOTS, "{slots} slots");
}
