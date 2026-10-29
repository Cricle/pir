//! Integration tests for the full pi extension API surface on QuickJS.
//!
//! Covers the P0/P1/P2 gap stream: `tool_result` merge onto the transcript,
//! `ctx.executeTool` nesting from JS, `structuredContent`/`outputSchema`,
//! the newly dispatched events (`before_provider_request`, `user_bash`,
//! `context_with_system`), boundary events (`turn_end`/`agent_before_settle`
//! with entries + continue), `getSettings`/`getCommands`, flag CLI wiring,
//! the new `ctx.sessionManager` methods, dialog request/reply and the
//! payload-parity fields.

use std::sync::{Arc, Mutex};

use pir::agent::nested_tool_calls::{NestedShared, NestedToolExecutor};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    apply_tool_result_merge, emit_tool_results, merge_extension_tools, ToolRegistry,
};
use pir::agent_session::{dispatch_user_bash, AgentSession, AgentSessionConfig, UserBashOutcome};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::extensions::{
    CommandDef, ExtensionConfig, ExtensionHandle, Notification, SessionContextSnapshot, SourceInfo,
    ToolResult as ExtensionToolResult,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_dir(test_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-ext-api-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn handle_config(cwd: &std::path::Path, has_ui: bool) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn started_handle(test_name: &str) -> (ExtensionHandle, std::path::PathBuf) {
    let dir = fixture_dir(test_name);
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("runtime start");
    (handle, dir)
}

async fn seed_and_load(handle: &ExtensionHandle, dir: &std::path::Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let path = dir.join(name);
    let result = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

// ---------------------------------------------------------------------------
// P0-2: tool_result handler merge applied onto the tool result
// ---------------------------------------------------------------------------

const TOOL_RESULT_MERGE_TS: &str = r#"
export default function (pi) {
  pi.on("tool_result", (event) => {
    if (event.toolName !== "merge_me") return undefined;
    return {
      content: [{ type: "text", text: "rewritten" }],
      details: { handled: true },
      structuredContent: { answer: 42 },
      isError: true,
      usage: { input: 10, output: 5 },
    };
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn tool_result_handler_merge_applies_to_finalized_results() {
    let (handle, dir) = started_handle("merge").await;
    seed_and_load(&handle, &dir, "merge.ts", TOOL_RESULT_MERGE_TS).await;

    let mut inputs = std::collections::HashMap::new();
    inputs.insert("call-1".to_string(), json!({ "text": "original" }));
    let results = vec![finalized("call-1", "merge_me", "original output")];
    let merges = emit_tool_results(&handle, &inputs, &results, None).await;
    let merge = merges.get("call-1").expect("merge recorded");
    assert_eq!(merge.content.as_deref(), Some("rewritten"));
    assert_eq!(merge.is_error, Some(true));
    assert!(merge.usage.is_some());

    let mut result = ToolResult::ok("original output");
    let usage = apply_tool_result_merge(&mut result, merge);
    assert_eq!(result.content, "rewritten");
    assert!(result.is_error);
    assert_eq!(result.structured_content, Some(json!({ "answer": 42 })));
    let usage = usage.expect("usage carried");
    let parsed = pir::agent::tool_execution::parse_usage_json(&usage).expect("usage parses");
    assert_eq!(parsed.input, 10);
    assert_eq!(parsed.output, 5);
    assert_eq!(parsed.total_tokens, 15);
}

fn finalized(
    call_id: &str,
    tool_name: &str,
    content: &str,
) -> pir::agent::tool_batch::FinalizedToolCall {
    pir::agent::tool_batch::FinalizedToolCall {
        index: 0,
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        result: ToolResult::ok(content),
        usage: None,
        duration_ms: Some(1),
    }
}

// ---------------------------------------------------------------------------
// P1-3: ctx.executeTool nesting from JS (round-trip through the scheduler)
// ---------------------------------------------------------------------------

const NESTED_TOOLS_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "inner",
    description: "inner tool",
    parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    async execute(_toolCallId, params) {
      return { content: [{ type: "text", text: "inner:" + params.n }] };
    },
  });
  pi.registerTool({
    name: "outer",
    description: "calls inner through ctx.executeTool",
    parameters: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    async execute(toolCallId, params, signal, onUpdate, ctx) {
      const outcome = await ctx.executeTool("inner", { n: params.n });
      if (outcome.isError) throw new Error("nested call failed");
      const text = outcome.result.content.map((c) => c.text).join("");
      return {
        content: [{ type: "text", text: "outer[" + toolCallId + "]:" + text }],
        details: { nestedToolCallId: outcome.toolCall.id },
      };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn ctx_execute_tool_round_trips_nested_calls() {
    let (handle, dir) = started_handle("nested").await;
    seed_and_load(&handle, &dir, "nested.ts", NESTED_TOOLS_TS).await;

    // Build the registry the scheduler would use: extension tools wrapped as
    // AgentTools plus a nested executor for the outer call.
    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &handle);
    let sink: pir::agent::nested_tool_calls::NestedEventSink =
        Arc::new(|_event| Box::pin(async {}));
    let shared = NestedShared::new(
        Arc::new(registry),
        pir::agent::tool_scheduler::ToolScheduler::new(Default::default()),
        None,
        Some(handle.clone()),
        dir.clone(),
        None,
        false,
        sink,
        None,
        None,
    );
    let executor = NestedToolExecutor::root(Arc::clone(&shared)).for_call("call-parent");
    handle.bind_nested_executor("call-parent", Arc::new(executor));

    let result = handle
        .call_tool("outer", "call-parent", &json!({ "n": 7 }))
        .await
        .expect("outer executes");
    let text = result
        .content
        .iter()
        .map(|c| c.text.as_str())
        .collect::<String>();
    assert_eq!(text, "outer[call-parent]:inner:7", "{result:?}");
    assert_eq!(result.details["nestedToolCallId"], "call-parent/0");
    handle.unbind_nested_executor("call-parent");
}

// ---------------------------------------------------------------------------
// P2-11: nested-call usage is summed onto the parent tool result
// ---------------------------------------------------------------------------

const NESTED_USAGE_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "inner",
    description: "inner tool",
    parameters: { type: "object", properties: {} },
    async execute(_toolCallId, _params) {
      return { content: [{ type: "text", text: "inner" }] };
    },
  });
  pi.registerTool({
    name: "outer",
    description: "calls inner through ctx.executeTool",
    parameters: { type: "object", properties: {} },
    async execute(toolCallId, _params, _signal, _onUpdate, ctx) {
      const outcome = await ctx.executeTool("inner", {});
      if (outcome.isError) throw new Error("nested call failed");
      return { content: [{ type: "text", text: "outer:" + outcome.toolCall.id }] };
    },
  });
  // Nested results are not persisted, so their usage is counted through the
  // recorder via this handler (upstream nested-tool-calls.ts:237-239).
  pi.on("tool_result", (event) => {
    if (event.toolName !== "inner") return undefined;
    return { usage: { input: 5, output: 2, totalTokens: 7 } };
  });
}
"#;

/// The nested calls' reported usage is summed in the recorder and surfaces
/// through `take_record`, so the agent loop can fold it into the parent tool
/// result message (upstream agent-session.ts:1110-1112).
#[tokio::test(flavor = "current_thread")]
async fn nested_call_usage_is_summed_in_the_recorder() {
    let (handle, dir) = started_handle("nested-usage").await;
    seed_and_load(&handle, &dir, "nested-usage.ts", NESTED_USAGE_TS).await;

    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &handle);
    let sink: pir::agent::nested_tool_calls::NestedEventSink =
        Arc::new(|_event| Box::pin(async {}));
    let shared = NestedShared::new(
        Arc::new(registry),
        pir::agent::tool_scheduler::ToolScheduler::new(Default::default()),
        None,
        Some(handle.clone()),
        dir.clone(),
        None,
        false,
        sink,
        None,
        None,
    );
    let executor = NestedToolExecutor::root(Arc::clone(&shared)).for_call("call-parent");
    handle.bind_nested_executor("call-parent", Arc::new(executor));

    let result = handle
        .call_tool("outer", "call-parent", &json!({}))
        .await
        .expect("outer executes");
    let text = result
        .content
        .iter()
        .map(|c| c.text.as_str())
        .collect::<String>();
    assert_eq!(text, "outer:call-parent/0", "{result:?}");

    // The recorder combined the nested calls' usage.
    let summary = shared
        .take_record("call-parent")
        .expect("nested call summary recorded");
    let usage = summary.usage.expect("usage recorded");
    assert_eq!(usage.input, 5);
    assert_eq!(usage.output, 2);
    assert_eq!(usage.total_tokens, 7);
    handle.unbind_nested_executor("call-parent");
}

// ---------------------------------------------------------------------------
// P1-4: structuredContent / outputSchema end-to-end
// ---------------------------------------------------------------------------

const STRUCTURED_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "structured",
    description: "returns structured content",
    parameters: { type: "object", properties: { invalid: { type: "boolean" } } },
    outputSchema: { type: "object", properties: { answer: { type: "number" } }, required: ["answer"] },
    async execute(_id, params) {
      if (params.invalid) {
        return { content: [{ type: "text", text: "bad" }], structuredContent: { answer: "nope" } };
      }
      return {
        content: [{ type: "text", text: "ok" }],
        structuredContent: { answer: 42 },
      };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn structured_content_round_trips_and_validates_output_schema() {
    let (handle, dir) = started_handle("structured").await;
    seed_and_load(&handle, &dir, "structured.ts", STRUCTURED_TS).await;

    // The declared outputSchema is published on the tool definition.
    let def = handle
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "structured")
        .expect("tool registered");
    assert_eq!(def.output_schema["type"], "object");

    let ok: ExtensionToolResult = handle
        .call_tool("structured", "call-1", &json!({}))
        .await
        .expect("valid result");
    assert_eq!(ok.structured_content, json!({ "answer": 42 }));

    // Structured content is passed through UNVALIDATED: upstream never
    // enforces `outputSchema` at runtime (types.ts:475 documents it as a
    // "should always set" convention; agent-loop.ts forwards
    // `structuredContent` as-is), so a schema-violating value round-trips.
    let bad: ExtensionToolResult = handle
        .call_tool("structured", "call-2", &json!({ "invalid": true }))
        .await
        .expect("structured content passes through");
    assert_eq!(bad.structured_content, json!({ "answer": "nope" }));
}

// ---------------------------------------------------------------------------
// P1-5: event dispatch — before_provider_request chaining, user_bash,
// context_with_system
// ---------------------------------------------------------------------------

const EVENTS_TS: &str = r#"
const seen = { beforeProvider: 0, userBash: null, contextWithSystem: 0 };
export default function (pi) {
  pi.on("before_provider_request", (event) => {
    seen.beforeProvider += 1;
    if (event.payload.url === "pass") return { url: "replaced", extra: true };
    return undefined;
  });
  pi.on("user_bash", (event) => {
    seen.userBash = event.command;
    if (event.command === "handled") {
      return {
        result: { output: "from extension", exitCode: 0, cancelled: false, truncated: false },
      };
    }
    return undefined;
  });
  pi.on("context_with_system", (event) => {
    seen.contextWithSystem += 1;
    return { messages: event.messages };
  });
  pi.registerTool({
    name: "report",
    description: "report observed events",
    parameters: { type: "object" },
    async execute() {
      return { content: [{ type: "text", text: JSON.stringify(seen) }] };
    },
  });
  globalThis.__reportSeen = () => JSON.stringify(seen);
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn before_provider_request_user_bash_and_context_with_system_dispatch() {
    let (handle, dir) = started_handle("events").await;
    seed_and_load(&handle, &dir, "events.ts", EVENTS_TS).await;

    // before_provider_request: handler replaces the payload (chained merge).
    let mut payload = json!({ "type": "before_provider_request", "payload": { "url": "pass" } });
    let outcome = handle
        .emit("before_provider_request", &mut payload)
        .await
        .expect("emit");
    assert_eq!(outcome.result["url"], "replaced");
    assert_eq!(outcome.result["extra"], true);

    // user_bash: no decision → local execution; a {result} object is handled.
    let outcome = dispatch_user_bash(&handle, "handled", false).await;
    match outcome {
        UserBashOutcome::Handled { output, is_error } => {
            assert_eq!(output, "from extension");
            assert!(!is_error);
        }
        other => panic!("expected Handled, got {other:?}"),
    }
    let local = dispatch_user_bash(&handle, "local-run", false).await;
    assert!(matches!(local, UserBashOutcome::Local));

    // context_with_system dispatch: payload parity + replacement honored.
    let mut payload = json!({
        "type": "context_with_system",
        "messages": [{ "role": "system", "content": "sys" }, { "role": "user", "content": "hi" }],
    });
    let outcome = handle
        .emit("context_with_system", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result["messages"].as_array().map(Vec::len), Some(2));

    let seen: Value = handle
        .eval_json("globalThis.__reportSeen()")
        .await
        .expect("eval");
    assert_eq!(seen["beforeProvider"], 1);
    assert_eq!(seen["userBash"], "local-run");
    assert_eq!(seen["contextWithSystem"], 1);
}

// ---------------------------------------------------------------------------
// P1-6: getSettings + getCommands (builtin commands included) + flags
// ---------------------------------------------------------------------------

const SETTINGS_COMMANDS_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("ext-cmd", { description: "extension command", handler: async () => {} });
  pi.registerFlag("report-flag", { type: "string", description: "report flag", default: "unset" });
  pi.registerTool({
    name: "report_surface",
    description: "report settings/commands/flag",
    parameters: { type: "object" },
    async execute() {
      return {
        content: [{
          type: "text",
          text: JSON.stringify({
            settings: pi.getSettings(),
            commands: pi.getCommands(),
            flag: pi.getFlag("report-flag"),
          }),
        }],
      };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn get_settings_get_commands_and_flags_surface() {
    let (handle, dir) = started_handle("surface").await;
    handle.update_settings(json!({ "theme": "dark", "tools": { "enabled": ["read"] } }));
    handle.update_builtin_commands(vec![CommandDef {
        name: "builtin-cmd".to_string(),
        description: Some("builtin command".to_string()),
        source: "extension".to_string(),
        source_info: SourceInfo {
            path: "builtin:test".to_string(),
            ..Default::default()
        },
        argument_completions: false,
    }]);
    seed_and_load(&handle, &dir, "surface.ts", SETTINGS_COMMANDS_TS).await;

    let result: ExtensionToolResult = handle
        .call_tool("report_surface", "call-1", &json!({}))
        .await
        .expect("tool runs");
    let report: Value = serde_json::from_str(&result.content[0].text).expect("report parses");
    assert_eq!(report["settings"]["theme"], "dark");
    let commands: Vec<&str> = report["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .map(|command| command["name"].as_str().expect("name"))
        .collect();
    assert!(commands.contains(&"builtin-cmd"), "{report:?}");
    assert!(commands.contains(&"ext-cmd"), "{report:?}");
    assert_eq!(report["flag"], "unset");

    // Flag values flow: the CLI wiring path sets the value on the runtime.
    handle
        .set_flag_value(
            "report-flag",
            pir::extensions::FlagValue::Str("cli".to_string()),
        )
        .expect("set flag");
    let result: ExtensionToolResult = handle
        .call_tool("report_surface", "call-2", &json!({}))
        .await
        .expect("tool runs");
    let report: Value = serde_json::from_str(&result.content[0].text).expect("report parses");
    assert_eq!(report["flag"], "cli");
}

// ---------------------------------------------------------------------------
// P2-9: ctx.sessionManager methods backed by the snapshot seam
// ---------------------------------------------------------------------------

const SESSION_MANAGER_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "session_report",
    description: "report sessionManager surface",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      const sm = ctx.sessionManager;
      const leaf = sm.getLeafEntry();
      return {
        content: [{
          type: "text",
          text: JSON.stringify({
            cwd: sm.getCwd(),
            header: sm.getHeader(),
            leafId: sm.getLeafId(),
            leafType: leaf ? leaf.type : null,
            entry: sm.getEntry("entry-2") ? sm.getEntry("entry-2").type : null,
            branchLen: sm.getBranch().length,
            treeLen: sm.getTree().length,
            label: sm.getLabel("entry-2"),
            projectionEntries: sm.buildSessionProjection().entries.length,
            contextEntries: sm.buildContextEntries().length,
            sessionFile: sm.getSessionFile(),
          }),
        }],
      };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn session_manager_methods_backed_by_snapshot() {
    let (handle, dir) = started_handle("sm").await;
    let tree = json!([
        { "type": "session", "id": "entry-1" },
        { "type": "message", "id": "entry-2", "label": "pinned" },
    ]);
    // The snapshot carries the tree as PRE-RENDERED JSON TEXT (the host
    // writes it with `TreeNode::forest_to_json_iterative` and
    // `getSessionManagerJson` splices it): a nested `Value` would recurse per
    // entry when serde_json serializes — or drops — it.
    let tree_json = serde_json::to_string(&tree).expect("tree text");
    handle.update_session_context(SessionContextSnapshot {
        model: json!({ "id": "test-model" }),
        entries: json!([{ "role": "user" }, { "role": "assistant" }]),
        sessions: json!([]),
        session_id: Some("s-1".to_string()),
        session_path: Some("/tmp/session.jsonl".to_string()),
        context_tokens: None,
        context_window: Some(4096),
        has_pending_messages: false,
        project_trusted: true,
        cwd: Some("/tmp".to_string()),
        header: json!({ "id": "s-1", "cwd": "/tmp" }),
        branch: tree,
        tree_json,
        leaf_id: Some("entry-2".to_string()),
        labels: json!({ "entry-2": "pinned" }),
        projection: json!({ "entries": [ { "entry": {}, "messages": [] }, { "entry": {}, "messages": [] } ], "messages": [] }),
    });
    seed_and_load(&handle, &dir, "sm.ts", SESSION_MANAGER_TS).await;

    let result: ExtensionToolResult = handle
        .call_tool("session_report", "call-1", &json!({}))
        .await
        .expect("tool runs");
    let report: Value = serde_json::from_str(&result.content[0].text).expect("report");
    assert_eq!(report["cwd"], "/tmp");
    assert_eq!(report["header"]["id"], "s-1");
    assert_eq!(report["leafId"], "entry-2");
    assert_eq!(report["leafType"], "message");
    assert_eq!(report["entry"], "message");
    assert_eq!(report["branchLen"], 2);
    assert_eq!(report["treeLen"], 2);
    assert_eq!(report["label"], "pinned");
    assert_eq!(report["projectionEntries"], 2);
    assert_eq!(report["contextEntries"], 2);
    assert_eq!(report["sessionFile"], "/tmp/session.jsonl");
}

// ---------------------------------------------------------------------------
// P2-10: ctx.ui dialogs — headless no-op and the TUI request/reply channel
// ---------------------------------------------------------------------------

const DIALOG_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "ask",
    description: "asks through ctx.ui",
    parameters: { type: "object", properties: { kind: { type: "string" } }, required: ["kind"] },
    async execute(_id, params, _signal, _onUpdate, ctx) {
      if (params.kind === "confirm") {
        const answer = await ctx.ui.confirm("Proceed?", "Are you sure?");
        return { content: [{ type: "text", text: "confirm:" + answer }] };
      }
      if (params.kind === "select") {
        const answer = await ctx.ui.select("Pick", ["a", "b"]);
        return { content: [{ type: "text", text: "select:" + answer }] };
      }
      return { content: [{ type: "text", text: "unknown" }] };
    },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn dialogs_noop_headless_and_reply_via_channel() {
    // Headless (has_ui=false): resolves undefined immediately (no wedge).
    let (headless, dir) = started_handle("dialog-headless").await;
    seed_and_load(&headless, &dir, "dialog.ts", DIALOG_TS).await;
    let result: ExtensionToolResult = headless
        .call_tool("ask", "call-1", &json!({ "kind": "confirm" }))
        .await
        .expect("headless dialog resolves");
    assert_eq!(result.content[0].text, "confirm:undefined");

    // TUI side (has_ui=true): the dialog request/notification is answered
    // through respond_dialog; ui_prompt_start/end fire around it.
    let dir = fixture_dir("dialog-ui");
    let ui_handle = ExtensionHandle::start(handle_config(&dir, true)).expect("start");
    seed_and_load(&ui_handle, &dir, "dialog.ts", DIALOG_TS).await;
    let worker = tokio::spawn({
        let handle = ui_handle.clone();
        async move {
            handle
                .call_tool("ask", "call-2", &json!({ "kind": "select" }))
                .await
                .expect("ui dialog resolves")
        }
    });
    // Wait for the dialog request notification, then reply like the TUI
    // drain would (pure logic: first option wins).
    let mut answered = false;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let requests: Vec<_> = ui_handle
            .take_notifications(|notification| {
                matches!(notification, Notification::DialogRequest { .. })
            })
            .into_iter()
            .collect();
        if let Some(request) = requests.into_iter().last() {
            let Notification::DialogRequest {
                id, kind, options, ..
            } = request
            else {
                continue;
            };
            assert_eq!(kind, "select");
            assert_eq!(options["options"][0]["value"], "a");
            ui_handle.respond_dialog(&id.to_string(), json!("a"));
            answered = true;
            break;
        }
    }
    assert!(answered, "dialog request was not observed");
    let result = worker.await.expect("worker");
    assert_eq!(result.content[0].text, "select:a");
}

// ---------------------------------------------------------------------------
// P1-8: payload parity — input images/streamingBehavior, tool_execution_update
// fields, ctx.mode normalization, ui_prompt events, renderer registries
// ---------------------------------------------------------------------------

const PARITY_TS: &str = r#"
const seen = { input: null, updates: [], promptEvents: [], mode: null, renderers: null };
export default function (pi) {
  pi.on("input", (event) => {
    seen.input = { hasImages: Array.isArray(event.images), streamingBehavior: event.streamingBehavior === null };
    return undefined;
  });
  pi.on("ui_prompt_start", (event) => { seen.promptEvents.push(event.type + ":" + event.kind); });
  pi.on("ui_prompt_end", (event) => { seen.promptEvents.push(event.type + ":" + event.kind); });
  pi.on("tool_execution_update", (event) => {
    seen.updates.push(event);
  });
  pi.registerMessageRenderer("custom-rendered", {});
  pi.registerEntryRenderer("custom-entry", {});
  pi.registerMarkdownTransformer("shrink", {});
  pi.registerToolRenderer("inner", {});
  pi.registerTool({
    name: "parity",
    description: "parity probe",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, onUpdate, ctx) {
      seen.mode = ctx.mode;
      await ctx.ui.custom("factory", {});
      onUpdate({ progress: 1 });
      const outcome = await ctx.executeTool("inner_nested", {});
      seen.renderers = outcome.isError === false;
      return { content: [{ type: "text", text: "ok" }] };
    },
  });
  pi.registerTool({
    name: "inner_nested",
    description: "nested probe",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, onUpdate) {
      onUpdate({ progress: 2 });
      return { content: [{ type: "text", text: "nested" }] };
    },
  });
  globalThis.__paritySeen = () => JSON.stringify(seen);
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn payload_parity_and_renderers() {
    let (handle, dir) = started_handle("parity").await;
    seed_and_load(&handle, &dir, "parity.ts", PARITY_TS).await;

    // input payload parity (the host adds images/streamingBehavior; mirror
    // the host shape here).
    let mut payload = json!({
        "type": "input",
        "text": "hello",
        "images": [],
        "streamingBehavior": null,
    });
    let _ = handle.emit("input", &mut payload).await.expect("emit");
    // Renderer registrations are real (exported through the registry).
    let renderers = handle.renderer_defs();
    assert_eq!(
        renderers["messageRenderers"][0]["customType"],
        "custom-rendered"
    );
    assert_eq!(renderers["entryRenderers"][0]["customType"], "custom-entry");
    assert_eq!(renderers["markdownTransformers"][0]["name"], "shrink");
    assert_eq!(renderers["toolRenderers"][0]["toolName"], "inner");

    // ctx.executeTool from the tool context: nested executor bound by the
    // scheduler path (bound manually here for the runtime-level probe).
    let mut registry = ToolRegistry::new();
    merge_extension_tools(&mut registry, &handle);
    let sink: pir::agent::nested_tool_calls::NestedEventSink =
        Arc::new(|_event| Box::pin(async {}));
    let shared = NestedShared::new(
        Arc::new(registry),
        pir::agent::tool_scheduler::ToolScheduler::new(Default::default()),
        None,
        Some(handle.clone()),
        dir.clone(),
        None,
        false,
        sink,
        None,
        None,
    );
    handle.bind_nested_executor(
        "call-p",
        Arc::new(NestedToolExecutor::root(Arc::clone(&shared)).for_call("call-p")),
    );
    let _ = handle
        .call_tool("parity", "call-p", &json!({}))
        .await
        .expect("parity tool");

    let seen: Value = handle
        .eval_json("globalThis.__paritySeen()")
        .await
        .expect("eval");
    assert_eq!(seen["mode"], "tui", "ctx.mode normalized to tui");
    assert_eq!(seen["input"]["hasImages"], true);
    assert_eq!(seen["input"]["streamingBehavior"], true);
    assert!(
        seen["promptEvents"]
            .as_array()
            .expect("prompt events")
            .iter()
            .any(|event| event == "ui_prompt_start:custom"),
        "{seen:?}"
    );
    assert!(
        seen["promptEvents"]
            .as_array()
            .expect("prompt events")
            .iter()
            .any(|event| event == "ui_prompt_end:custom"),
        "{seen:?}"
    );

    // tool_execution_update parity: toolName/args/parentToolCallId.
    let notifications = handle.drain_notifications();
    let updates: Vec<&Notification> = notifications
        .iter()
        .filter(|notification| matches!(notification, Notification::ToolUpdate { .. }))
        .collect();
    assert!(updates.len() >= 2, "{notifications:?}");
    // The top-level call's update carries no parent; the nested call's update
    // (id `<parent>/<n>`) derives `parentToolCallId` from its id.
    let parity_update = updates
        .iter()
        .find(|notification| match notification {
            Notification::ToolUpdate { tool_name, .. } => tool_name.as_deref() == Some("parity"),
            _ => false,
        })
        .expect("parity tool update");
    match parity_update {
        Notification::ToolUpdate {
            tool_call_id,
            tool_name,
            args,
            parent_call_id,
            partial,
        } => {
            assert_eq!(tool_call_id, "call-p");
            assert_eq!(tool_name.as_deref(), Some("parity"));
            assert_eq!(args.as_ref(), Some(&json!({})));
            assert_eq!(parent_call_id.as_deref(), None);
            assert_eq!(
                partial.as_ref().and_then(|partial| partial.get("progress")),
                Some(&json!(1))
            );
        }
        _ => unreachable!("filtered above"),
    }
    let nested_update = updates
        .iter()
        .find(|notification| match notification {
            Notification::ToolUpdate { tool_call_id, .. } => tool_call_id == "call-p/0",
            _ => false,
        })
        .expect("nested tool update");
    match nested_update {
        Notification::ToolUpdate {
            tool_name,
            parent_call_id,
            partial,
            ..
        } => {
            assert_eq!(tool_name.as_deref(), Some("inner_nested"));
            assert_eq!(parent_call_id.as_deref(), Some("call-p"));
            assert_eq!(
                partial.as_ref().and_then(|partial| partial.get("progress")),
                Some(&json!(2))
            );
        }
        _ => unreachable!("filtered above"),
    }
    assert!(seen["renderers"] == true, "{seen:?}");
}

// ---------------------------------------------------------------------------
// ctx.ui.custom: headless no-UI contract (undefined, no warning)
// ---------------------------------------------------------------------------

const UI_CUSTOM_STUB_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "custom_probe",
    description: "call ctx.ui.custom twice without a host UI",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      const first = await ctx.ui.custom("factory", {});
      const second = await ctx.ui.custom("factory", {});
      const bothUndefined = first === undefined && second === undefined;
      return {
        content: [{
          type: "text",
          text: `first=${String(first)} second=${String(second)} bothUndefined=${bothUndefined}`,
        }],
      };
    },
  });
}
"#;

/// `ctx.ui.custom` follows the same headless contract as `select`/`confirm`/
/// `input`/`editor`: with `hasUI == false` the prelude's
/// `withUIPrompt("custom", undefined, ...)` returns `undefined` before any
/// factory validation or warning. The one-shot `customOverlayWarnings` latch
/// only covers genuinely unsupported component shapes while a UI is present
/// (`options.onHandle`, or a factory result without `render(width)`), so a
/// headless call must stay silent.
#[tokio::test(flavor = "current_thread")]
async fn ui_custom_without_ui_returns_undefined_without_warning() {
    let (handle, dir) = started_handle("ui-custom-stub").await;
    seed_and_load(&handle, &dir, "custom.ts", UI_CUSTOM_STUB_TS).await;

    let result = handle
        .call_tool("custom_probe", "call-custom", &json!({}))
        .await
        .expect("ctx.ui.custom must not throw");
    assert_eq!(
        result.content[0].text,
        "first=undefined second=undefined bothUndefined=true"
    );

    // The console shim routes `console.warn` to `Notification::Notify` with
    // level "warning"; the no-UI path must emit none.
    let warnings: Vec<String> = handle
        .drain_notifications()
        .into_iter()
        .filter_map(|notification| match notification {
            Notification::Notify { message, level } if level == "warning" => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        warnings.is_empty(),
        "ctx.ui.custom must resolve undefined without a warning when no UI is attached: {warnings:?}"
    );
}

// ---------------------------------------------------------------------------
// ctx.ui.custom: `options.onHandle` (upstream OverlayHandle, tui.ts:309-321)
// ---------------------------------------------------------------------------

const UI_CUSTOM_HANDLE_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "custom_handle_probe",
    description: "exercise ctx.ui.custom onHandle",
    parameters: { type: "object" },
    async execute(_id, _params, _signal, _onUpdate, ctx) {
      const seen = {
        called: false,
        types: null,
        hiddenAfterSet: null,
        hiddenAfterFocus: null,
        focusedAfterFocus: null,
        bounds: "unset",
        result: null,
      };
      const result = await ctx.ui.custom(
        (_tui, _theme, _keybindings, done) => ({
          render: () => ["panel row"],
          handleInput: () => {},
          dispose: () => {},
        }),
        {
          onHandle: (handle) => {
            seen.called = true;
            seen.types = [
              typeof handle.hide,
              typeof handle.setHidden,
              typeof handle.isHidden,
              typeof handle.focus,
              typeof handle.unfocus,
              typeof handle.isFocused,
              typeof handle.getBounds,
            ].join(",");
            seen.isFocusedBefore = handle.isFocused();
            handle.setHidden(true);
            seen.hiddenAfterSet = handle.isHidden();
            handle.focus();
            seen.hiddenAfterFocus = handle.isHidden();
            seen.focusedAfterFocus = handle.isFocused();
            seen.bounds = String(handle.getBounds());
          },
        },
      );
      seen.result = result;
      globalThis.__customHandle = seen;
      return { content: [{ type: "text", text: "done" }] };
    },
  });
}
"#;

/// `onHandle` is invoked with the upstream `OverlayHandle` shape; visibility
/// changes are real (host-directed `hidden` ops) and `focus` re-shows a
/// temporarily hidden overlay. The host resolves the dialog as usual.
#[tokio::test(flavor = "current_thread")]
async fn ui_custom_on_handle_reports_visibility_changes() {
    let dir = fixture_dir("custom-handle");
    let handle = ExtensionHandle::start(handle_config(&dir, true)).expect("start");
    seed_and_load(&handle, &dir, "custom-handle.ts", UI_CUSTOM_HANDLE_TS).await;

    let worker = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .call_tool("custom_handle_probe", "call-handle", &json!({}))
                .await
                .expect("custom tool resolves")
        }
    });

    let mut hidden_ops = Vec::new();
    let mut request_id = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let notifications = handle.drain_notifications();
        for notification in notifications {
            match notification {
                Notification::Status { key, text } if key == "__pir_custom_overlay" => {
                    let event: serde_json::Value =
                        serde_json::from_str(text.as_deref().unwrap_or("null"))
                            .unwrap_or(serde_json::Value::Null);
                    if event["op"] == json!("hidden") {
                        hidden_ops.push(event["hidden"].clone());
                    }
                }
                Notification::DialogRequest { id, kind, .. } if kind == "custom" => {
                    request_id = Some(id);
                }
                _ => {}
            }
        }
        if let Some(id) = request_id {
            // The `onHandle` callback ran before the request was pushed: the
            // opening payload carries the current (visible) state.
            handle.respond_dialog(&id.to_string(), json!("finished"));
            break;
        }
    }
    assert!(
        request_id.is_some(),
        "the custom dialog request was not seen"
    );
    let result = worker.await.expect("worker");
    assert_eq!(result.content[0].text, "done");

    let seen: serde_json::Value = handle
        .eval_json("JSON.stringify(globalThis.__customHandle ?? null)")
        .await
        .expect("probe");
    assert_eq!(seen["called"], json!(true), "{seen}");
    assert_eq!(
        seen["types"],
        json!("function,function,function,function,function,function,function"),
        "the handle exposes every upstream method: {seen}"
    );
    assert_eq!(seen["isFocusedBefore"], json!(true));
    assert_eq!(seen["hiddenAfterSet"], json!(true));
    assert_eq!(seen["hiddenAfterFocus"], json!(false));
    assert_eq!(seen["focusedAfterFocus"], json!(true));
    assert_eq!(seen["bounds"], json!("undefined"));
    assert_eq!(seen["result"], json!("finished"));
    // Two host-directed visibility ops, in order: hide then show (focus).
    assert_eq!(
        hidden_ops,
        vec![json!(true), json!(false)],
        "{hidden_ops:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-7: boundary events end-to-end (entries + continue) through AgentSession
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
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
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

struct FakeProvider {
    calls: std::sync::atomic::AtomicUsize,
    respond: Arc<dyn Fn(usize) -> AssistantMessage + Send + Sync>,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let message = (self.respond)(index);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

const BOUNDARY_TS: &str = r#"
let continued = false;
export default function (pi) {
  pi.registerTool({
    name: "noop",
    description: "no-op tool",
    parameters: { type: "object" },
    async execute() {
      return { content: [{ type: "text", text: "done" }] };
    },
  });
  pi.on("turn_end", () => {
    if (continued) return undefined;
    continued = true;
    return {
      continue: true,
      entries: [{ type: "custom", customType: "boundary-note", data: { at: "turn_end" } }],
    };
  });
  pi.on("agent_before_settle", (event) => {
    if (event.continue) return undefined;
    return {
      entries: [{ type: "custom", customType: "settle-note", data: {} }],
    };
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn boundary_events_commit_entries_and_honor_continue() {
    let dir = fixture_dir("boundary");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("start");
    seed_and_load(&handle, &dir, "boundary.ts", BOUNDARY_TS).await;

    let temp = std::env::temp_dir().join(format!("pir-ext-api-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).expect("session dir");
    let session = pir::session_manager::SessionManager::create(".", &temp).expect("create session");

    // Call 0: a tool call; call 1 (forced by the turn_end continue): plain stop.
    let calls: Arc<std::sync::atomic::AtomicUsize> = Default::default();
    let respond_calls = Arc::clone(&calls);
    let provider = Arc::new(FakeProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
        respond: Arc::new(move |index: usize| {
            respond_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if index == 0 {
                assistant(
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "noop".into(),
                        arguments: Default::default(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })],
                    StopReason::ToolUse,
                )
            } else {
                assistant(vec![text_block("settled")], StopReason::Stop)
            }
        }),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider);
    let registry = Arc::new(registry);

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 8,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle.clone()),
        tool_selection: None,
    });
    let noop: Arc<Mutex<Vec<()>>> = Default::default();
    let _ = noop;
    agent
        .prompt("run", Arc::new(|_event| {}))
        .await
        .expect("prompt");

    // The turn_end `continue` forced exactly one more provider request
    // (tool turn + continued turn = 2 calls).
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "continue must force exactly one more provider request"
    );

    // The boundary entry drafts were committed to session storage.
    let mut committed = String::new();
    for entry in std::fs::read_dir(&temp).expect("session dir").flatten() {
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            committed.push_str(&content);
        }
    }
    assert!(
        committed.contains("boundary-note"),
        "turn_end draft not committed: {committed}"
    );
    assert!(
        committed.contains("settle-note"),
        "agent_before_settle draft not committed: {committed}"
    );
}

// ---------------------------------------------------------------------------
// #1: `session_compact.compactionEntry` (upstream SessionCompactEvent)
// ---------------------------------------------------------------------------

const COMPACT_ENTRY_TS: &str = r#"
export default function (pi) {
  pi.on("session_compact", (event) => {
    globalThis.__seenCompact = event;
  });
}
"#;

fn session_user(body: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(body)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn session_assistant(body: &str) -> Message {
    Message::Assistant(assistant(vec![text_block(body)], StopReason::Stop))
}

/// The extension handler reads the upstream `compactionEntry` (the persisted
/// `CompactionEntry`: type/id/parentId/timestamp/summary/firstKeptEntryId/
/// tokensBefore/details/usage) and the legacy `compaction` result still rides
/// the same payload.
#[tokio::test(flavor = "current_thread")]
async fn session_compact_carries_the_persisted_compaction_entry() {
    let dir = fixture_dir("compact-entry");
    let handle = ExtensionHandle::start(handle_config(&dir, false)).expect("start");
    seed_and_load(&handle, &dir, "compact-entry.ts", COMPACT_ENTRY_TS).await;

    let session_dir = dir.join("session");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let mut session =
        pir::session_manager::SessionManager::create(dir.to_string_lossy().as_ref(), &session_dir)
            .expect("create session");
    for index in 0..3 {
        session
            .append_message(session_user(&format!("user turn {index}")))
            .expect("append user");
        session
            .append_message(session_assistant(&format!("assistant turn {index}")))
            .expect("append assistant");
    }

    let provider = Arc::new(FakeProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
        respond: Arc::new(|_index: usize| {
            assistant(vec![text_block("COMPACTED SUMMARY")], StopReason::Stop)
        }),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider);
    let registry = Arc::new(registry);

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry,
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 1,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle.clone()),
        tool_selection: None,
    });
    let result = agent.compact(None).await.expect("manual compact");

    let seen: Value = handle
        .eval_json("JSON.stringify(globalThis.__seenCompact ?? null)")
        .await
        .expect("probe the captured session_compact event");
    assert_eq!(seen["type"], json!("session_compact"), "{seen}");
    assert_eq!(seen["reason"], json!("manual"), "{seen}");
    assert_eq!(seen["fromExtension"], json!(false), "{seen}");
    assert_eq!(seen["willRetry"], json!(false), "{seen}");

    // The upstream field: a complete CompactionEntry object.
    let entry = &seen["compactionEntry"];
    assert!(
        entry.is_object(),
        "compactionEntry must be an object: {seen}"
    );
    assert_eq!(entry["type"], json!("compaction"), "{seen}");
    let entry_id = entry["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| panic!("compactionEntry.id must be a non-empty string: {seen}"))
        .to_owned();
    assert!(
        entry["timestamp"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "compactionEntry.timestamp must be a non-empty string: {seen}"
    );
    assert!(
        entry.get("parentId").is_some(),
        "compactionEntry.parentId is part of the entry shape: {seen}"
    );
    assert!(
        entry["firstKeptEntryId"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "compactionEntry.firstKeptEntryId must be a non-empty string: {seen}"
    );
    assert_eq!(
        entry["tokensBefore"].as_u64(),
        Some(result.tokens_before),
        "compactionEntry.tokensBefore: {seen}"
    );
    assert!(
        entry["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("COMPACTED SUMMARY"),
        "compactionEntry.summary: {seen}"
    );
    assert!(
        entry["usage"].is_object(),
        "compactionEntry.usage carries the summarizer usage: {seen}"
    );
    assert!(
        entry["details"]["readFiles"].is_array(),
        "compactionEntry.details carries the file lists: {seen}"
    );

    // The historical field stays available for existing consumers.
    let compaction = &seen["compaction"];
    assert!(
        compaction["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("COMPACTED SUMMARY"),
        "legacy compaction field: {seen}"
    );
    assert!(
        compaction["usage"].is_object(),
        "legacy compaction field keeps the full result: {seen}"
    );

    // The entry is the one that was actually persisted to session storage.
    let persisted = agent
        .with_session_manager(|manager| manager.entry(&entry_id).cloned())
        .await
        .expect("session storage");
    match persisted {
        Some(pir::session_manager::FileEntry::Compaction(persisted)) => {
            assert_eq!(persisted.id, entry_id);
            assert_eq!(persisted.tokens_before, result.tokens_before);
            assert_eq!(persisted.summary, result.summary);
            assert_eq!(
                persisted.first_kept_entry_id,
                entry["firstKeptEntryId"].as_str().unwrap()
            );
        }
        other => panic!("compactionEntry must name a persisted compaction entry: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// #2: `tool_result` content blocks + usage, and image/text handler merges
// ---------------------------------------------------------------------------

const TOOL_RESULT_TS: &str = r#"
export default function (pi) {
  pi.on("tool_result", (event) => {
    globalThis.__toolResult = event;
    if (event.toolName === "return_image") {
      return {
        content: [
          { type: "text", text: "replaced text" },
          { type: "image", data: "TkVX", mimeType: "image/jpeg" },
        ],
      };
    }
    if (event.toolName === "return_text") {
      return { content: [{ type: "text", text: "text only" }] };
    }
    if (event.toolName === "return_legacy") {
      return { content: "legacy text" };
    }
    return undefined;
  });
}
"#;

fn finalized_tool_result(
    call_id: &str,
    tool_name: &str,
    result: ToolResult,
    usage: Option<Usage>,
) -> pir::agent::tool_batch::FinalizedToolCall {
    pir::agent::tool_batch::FinalizedToolCall {
        index: 0,
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        result,
        usage,
        duration_ms: Some(1),
    }
}

/// The event payload carries the complete `(TextContent | ImageContent)[]`
/// (the tool result's images included) and the execution `usage`.
#[tokio::test(flavor = "current_thread")]
async fn tool_result_event_carries_image_blocks_and_usage() {
    let (handle, dir) = started_handle("tool-result-payload").await;
    seed_and_load(&handle, &dir, "tool-result.ts", TOOL_RESULT_TS).await;

    let mut result = ToolResult::ok("see attached");
    result.images.push(ImageContent {
        data: "aW1n".into(),
        mime_type: "image/png".into(),
    });
    let usage = Usage {
        input: 5,
        output: 2,
        total_tokens: 7,
        ..Usage::default()
    };
    let results = vec![finalized_tool_result(
        "call-img",
        "observe",
        result,
        Some(usage),
    )];
    let mut inputs = std::collections::HashMap::new();
    inputs.insert("call-img".to_string(), json!({ "path": "a.png" }));
    let merges = emit_tool_results(&handle, &inputs, &results, None).await;
    assert!(
        merges.is_empty(),
        "the observability handler returns nothing"
    );

    let seen: Value = handle
        .eval_json("JSON.stringify(globalThis.__toolResult ?? null)")
        .await
        .expect("probe the captured tool_result event");
    assert_eq!(seen["type"], json!("tool_result"), "{seen}");
    assert_eq!(seen["toolName"], json!("observe"), "{seen}");
    assert_eq!(seen["input"], json!({ "path": "a.png" }), "{seen}");
    assert_eq!(seen["isError"], json!(false), "{seen}");
    let content = seen["content"].as_array().expect("content block array");
    assert_eq!(content.len(), 2, "text block + image block: {seen}");
    assert_eq!(
        content[0],
        json!({ "type": "text", "text": "see attached" }),
        "{seen}"
    );
    assert_eq!(
        content[1],
        json!({ "type": "image", "data": "aW1n", "mimeType": "image/png" }),
        "{seen}"
    );
    assert_eq!(seen["usage"]["input"], json!(5), "{seen}");
    assert_eq!(seen["usage"]["output"], json!(2), "{seen}");
    assert_eq!(seen["usage"]["totalTokens"], json!(7), "{seen}");
}

/// A handler's returned `content` array replaces the whole content: images
/// ride through, a text-only return drops the old image, and the legacy bare
/// string form keeps its text-only behavior.
#[tokio::test(flavor = "current_thread")]
async fn tool_result_handler_images_and_text_replace_content() {
    let (handle, dir) = started_handle("tool-result-merge").await;
    seed_and_load(&handle, &dir, "tool-result.ts", TOOL_RESULT_TS).await;

    // Handler returns text + image: both replace the original text/image.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "T0xE".into(),
        mime_type: "image/png".into(),
    });
    result.structured_content = Some(json!({ "old": true }));
    let results = vec![finalized_tool_result(
        "call-image",
        "return_image",
        result.clone(),
        None,
    )];
    let merges =
        emit_tool_results(&handle, &std::collections::HashMap::new(), &results, None).await;
    let merge = merges.get("call-image").expect("image merge recorded");
    assert_eq!(
        merge.content.as_deref(),
        Some("replaced text"),
        "the flattened text view stays available"
    );
    assert!(
        merge.content_blocks.is_some(),
        "the full block array is parsed"
    );
    let mut applied = result;
    apply_tool_result_merge(&mut applied, merge);
    assert_eq!(applied.content, "replaced text");
    assert_eq!(
        applied.images,
        vec![ImageContent {
            data: "TkVX".into(),
            mime_type: "image/jpeg".into(),
        }]
    );
    assert!(
        applied.structured_content.is_none(),
        "replacing content drops the stale structured content"
    );

    // Handler returns only text: the whole array is replaced, so the old
    // image must not survive next to the new text.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "T0xE".into(),
        mime_type: "image/png".into(),
    });
    let results = vec![finalized_tool_result(
        "call-text",
        "return_text",
        result.clone(),
        None,
    )];
    let merges =
        emit_tool_results(&handle, &std::collections::HashMap::new(), &results, None).await;
    let merge = merges.get("call-text").expect("text merge recorded");
    let mut applied = result;
    apply_tool_result_merge(&mut applied, merge);
    assert_eq!(applied.content, "text only");
    assert!(
        applied.images.is_empty(),
        "text-only replacement must not leave the original image behind"
    );

    // Legacy bare-string return: text replaced, images kept.
    let mut result = ToolResult::ok("original");
    result.images.push(ImageContent {
        data: "S0VQVA".into(),
        mime_type: "image/png".into(),
    });
    let results = vec![finalized_tool_result(
        "call-legacy",
        "return_legacy",
        result.clone(),
        None,
    )];
    let merges =
        emit_tool_results(&handle, &std::collections::HashMap::new(), &results, None).await;
    let merge = merges.get("call-legacy").expect("legacy merge recorded");
    let mut applied = result;
    apply_tool_result_merge(&mut applied, merge);
    assert_eq!(applied.content, "legacy text");
    assert_eq!(
        applied.images.len(),
        1,
        "the legacy text-only form keeps existing images"
    );
    assert_eq!(applied.images[0].data, "S0VQVA");
}
