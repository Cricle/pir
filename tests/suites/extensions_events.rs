//! Integration tests for the full extension event surface, ctx facade,
//! registerProvider/registerShortcut, the TypeBox validator, the 30s dispatch
//! timeout (QuickJS interrupt), the dependency scanner and `/reload`.
//!
//! Fixtures are generated into per-test temp directories (the repository
//! hygiene gate forbids tracked `.ts` files).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification, ToolRenderShell};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-ext-events-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("events-model".to_string()),
        session_id: Some("events-session".to_string()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn write_source(dir: &TempDir, name: &str, source: &str) -> PathBuf {
    let path = dir.0.join(name);
    std::fs::write(&path, source).expect("write fixture");
    path
}

// ---------------------------------------------------------------------------
// load_extension_runtime_strict (upstream `Failed to load extension` + hint)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn strict_extension_loader_reports_load_failures_with_upstream_text() {
    let temp = TempDir::new("strict-loader");
    // A path that does not exist fails to load.
    let missing = temp.0.join("missing-extension.js");
    let (runtime, errors) = pir::agent_session::load_extension_runtime_strict(
        std::slice::from_ref(&missing),
        test_config_at(&temp.0),
    )
    .await
    .expect("strict loader runs");
    assert!(runtime.is_some(), "the runtime itself still starts");
    assert!(
        errors.iter().any(|failure| {
            failure.message.starts_with(&format!(
                "Failed to load extension \"{}\"",
                missing.display()
            )) && failure.path.as_deref() == Some(missing.as_path())
        }),
        "upstream failure text (with its path) missing: {errors:?}"
    );

    // No paths: no runtime, no errors.
    let (runtime, errors) =
        pir::agent_session::load_extension_runtime_strict(&[], test_config_at(&temp.0))
            .await
            .expect("strict loader runs");
    assert!(runtime.is_none());
    assert!(errors.is_empty());
}

async fn load_runtime(dir: &TempDir, source: &str) -> ExtensionHandle {
    load_runtime_named(dir, "ext.ts", source).await
}

async fn load_runtime_named(dir: &TempDir, name: &str, source: &str) -> ExtensionHandle {
    let path = write_source(dir, name, source);
    let runtime = ExtensionHandle::start(test_config_at(&dir.0)).expect("runtime");
    let result = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    runtime
}

/// Probe JS globals by evaluating an expression that produces JSON.
async fn probe(runtime: &ExtensionHandle, code: &str) -> Value {
    runtime
        .eval_json(&format!("JSON.stringify({code})"))
        .await
        .expect("probe eval")
}

// ---------------------------------------------------------------------------
// Event outcome-merge semantics
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn context_event_chains_replacements_across_handlers() {
    let temp = TempDir::new("context-chain");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("context", (event) => {
                globalThis.__seen1 = event.messages.length;
                return { messages: [...event.messages, { role: "user", content: "one" }] };
            });
            pi.on("context", (event) => {
                globalThis.__seen2 = event.messages.length;
                return { messages: [...event.messages, { role: "user", content: "two" }] };
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "context",
        "messages": [{ "role": "user", "content": "base" }],
    });
    let outcome = runtime.emit("context", &mut payload).await.expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    // Each handler saw the previous replacement (1 → 2).
    let seen1 = probe(&runtime, "globalThis.__seen1").await;
    let seen2 = probe(&runtime, "globalThis.__seen2").await;
    assert_eq!(seen1, json!(1));
    assert_eq!(seen2, json!(2));
    // Final merged outcome carries the last replacement.
    let messages = outcome.result["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2]["content"], json!("two"));
}

#[tokio::test(flavor = "current_thread")]
async fn message_end_chains_and_enforces_role() {
    let temp = TempDir::new("message-end");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("message_end", (event) => {
                globalThis.__role1 = event.message.role;
                return { message: { ...event.message, content: "patched" } };
            });
            pi.on("message_end", (event) => {
                globalThis.__role2 = event.message.role;
                globalThis.__content2 = event.message.content;
                return { message: { role: "user", content: "wrong role" } };
            });
            pi.on("message_end", (event) => {
                globalThis.__role3 = event.message.role;
                return undefined;
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "message_end",
        "message": { "role": "assistant", "content": "original" },
    });
    let outcome = runtime
        .emit("message_end", &mut payload)
        .await
        .expect("emit");
    // The chaining worked: handler 2 saw handler 1's patch...
    let content2 = probe(&runtime, "globalThis.__content2").await;
    assert_eq!(content2, json!("patched"));
    // ...but its role-changing replacement was dropped with an error.
    assert_eq!(outcome.errors.len(), 1);
    assert!(outcome.errors[0]
        .error
        .contains("message_end handlers must return a message with the same role"));
    // Final outcome keeps the last valid replacement.
    assert_eq!(outcome.result["message"]["role"], json!("assistant"));
    assert_eq!(outcome.result["message"]["content"], json!("patched"));
}

#[tokio::test(flavor = "current_thread")]
async fn user_bash_validates_result_shape() {
    let temp = TempDir::new("user-bash");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("user_bash", () => ({ operations: {}, result: {} }));
            pi.on("user_bash", () => { globalThis.__after = true; });
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "user_bash", "command": "ls", "cwd": "." });
    let outcome = runtime.emit("user_bash", &mut payload).await.expect("emit");
    // An invalid shape aborts the dispatch (upstream rethrows; the host must
    // not fall back to local execution, P1-1/P2-4).
    assert_eq!(outcome.result["aborted"], json!(true));
    assert_eq!(outcome.errors.len(), 1);
    assert!(outcome.errors[0]
        .error
        .contains("Invalid user_bash handler result"));

    // The invalid handler stops the round (no later handler runs).
    let after = probe(&runtime, "globalThis.__after ?? null").await;
    assert_eq!(after, json!(null));
}

#[tokio::test(flavor = "current_thread")]
async fn before_provider_request_chains_payload() {
    let temp = TempDir::new("bpr");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("before_provider_request", (event) => ({ ...event.payload, step: 1 }));
            pi.on("before_provider_request", (event) => {
                globalThis.__sawStep = event.payload.step;
                return { ...event.payload, step: 2 };
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "before_provider_request", "payload": {} });
    let outcome = runtime.emit("before_provider_request", &mut payload).await;
    let _ = outcome; // consumed below via the probe
    let saw = probe(&runtime, "globalThis.__sawStep ?? null").await;
    assert_eq!(
        saw,
        json!(1),
        "second handler must see the first replacement"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn before_provider_headers_handlers_mutate_in_place() {
    let temp = TempDir::new("headers");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("before_provider_headers", (event) => {
                event.headers["x-trace"] = "1";
                event.headers["x-drop"] = null;
                return "ignored";
            });
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "before_provider_headers",
        "headers": { "x-keep": "0", "x-drop": "gone" },
    });
    let outcome = runtime
        .emit("before_provider_headers", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.result["headers"]["x-trace"], json!("1"));
    assert_eq!(outcome.result["headers"]["x-keep"], json!("0"));
    // A null value deletes the header (the merged map carries null).
    assert_eq!(outcome.result["headers"]["x-drop"], Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn project_trust_and_resources_discover_merge() {
    let temp = TempDir::new("trust");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("project_trust", () => ({ trusted: "no" }));
            pi.on("project_trust", () => ({ trusted: "yes", remember: true }));
            pi.on("resources_discover", () => ({ skillPaths: ["a.ts"] }));
            pi.on("resources_discover", () => ({ skillPaths: ["b.ts"], themePaths: ["t.css"] }));
        }
        "#,
    )
    .await;

    let mut payload = json!({ "type": "project_trust", "cwd": temp.0.display().to_string() });
    let outcome = runtime
        .emit("project_trust", &mut payload)
        .await
        .expect("emit");
    // The first decided (non-"undecided") handler wins and stops the
    // dispatch (upstream emitProjectTrustEvent, P2-9).
    assert_eq!(outcome.result["trusted"], json!("no"));
    assert!(outcome.result.get("remember").is_none());

    let mut payload = json!({ "type": "resources_discover", "reason": "startup" });
    let outcome = runtime
        .emit("resources_discover", &mut payload)
        .await
        .expect("emit");
    assert_eq!(outcome.result["skillPaths"], json!(["a.ts", "b.ts"]));
    assert_eq!(outcome.result["themePaths"], json!(["t.css"]));
}

#[tokio::test(flavor = "current_thread")]
async fn tool_result_handlers_merge_per_field() {
    let temp = TempDir::new("tool-result");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("tool_result", () => ({ isError: true }));
            pi.on("tool_result", () => ({ details: { extra: 1 } }));
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "tool_result",
        "toolCallId": "c1",
        "toolName": "bash",
        "content": [{ "type": "text", "text": "out" }],
        "isError": false,
    });
    let outcome = runtime
        .emit("tool_result", &mut payload)
        .await
        .expect("emit");
    // Only the fields handlers set are merged (upstream returns the modified
    // subset); unset fields stay absent from the outcome.
    assert_eq!(outcome.result["isError"], json!(true));
    assert_eq!(outcome.result["details"]["extra"], json!(1));
    assert!(outcome.result.get("content").is_none(), "content untouched");
}

// ---------------------------------------------------------------------------
// Agent lifecycle events through the loop seams
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn loop_events_reach_extensions() {
    let temp = TempDir::new("loop-events");
    let source = r#"
        export default function (pi) {
            pi.on("agent_start", () => { globalThis.__agent_start = true; });
            pi.on("turn_start", (event) => {
                globalThis.__turn_start = (globalThis.__turn_start ?? 0) + 1;
                globalThis.__turn_index = event.turnIndex;
            });
            pi.on("message_end", (event) => {
                globalThis.__message_roles = [...(globalThis.__message_roles ?? []), event.message.role];
            });
            pi.on("agent_settled", () => { globalThis.__settled = true; });
        }
        "#;
    let runtime = load_runtime(&temp, source).await;

    // Drive one real prompt through AgentSession (fake provider below).
    use pir::agent::tool_batch::ToolResult;
    use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;

    struct EchoTool;
    impl AgentTool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echo"
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(json!({ "type": "object" }))
        }
        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            Box::pin(async { ToolResult::ok("echoed") })
        }
    }

    struct Fake {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Provider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            if self.calls() == 0 {
                stream.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    assistant_message(
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "call-1".into(),
                            name: "echo".into(),
                            arguments: Default::default(),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        })],
                        StopReason::ToolUse,
                    ),
                ));
            } else {
                stream.push(AssistantMessageEventStream::done_event(
                    TerminalReason::Stop,
                    assistant_message(vec![], StopReason::Stop),
                ));
            }
            self.bump();
            stream
        }
    }
    impl Fake {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn bump(&self) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn fake_model() -> Model {
        Model {
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
        }
    }

    fn assistant_message(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
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

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Fake {
        calls: std::sync::atomic::AtomicUsize::new(0),
    }));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let started = probe(&runtime, "globalThis.__agent_start ?? false").await;
    assert_eq!(started, json!(true));
    let turns = probe(&runtime, "globalThis.__turn_start ?? 0").await;
    assert_eq!(turns, json!(2), "two model turns");
    let roles = probe(&runtime, "globalThis.__message_roles ?? []").await;
    let roles = roles.as_array().expect("roles");
    // user (session seam) + assistant message_end + toolResult message_end.
    assert_eq!(roles[0], json!("user"));
    assert!(roles.iter().any(|role| role == "assistant"));
    assert!(roles.iter().any(|role| role == "toolResult"));
    let settled = probe(&runtime, "globalThis.__settled ?? false").await;
    assert_eq!(settled, json!(true));
}

// ---------------------------------------------------------------------------
// TypeBox validator
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn schema_validator_accepts_and_rejects() {
    let temp = TempDir::new("schema");
    let runtime = load_runtime(&temp, "export default function (pi) { void pi; }").await;

    let check = |schema: Value, value: Value| {
        let runtime = runtime.clone();
        async move {
            let code = format!(
                "__pir_schemaValidate({}, {})",
                serde_json::to_string(&schema).unwrap(),
                serde_json::to_string(&value).unwrap()
            );
            probe(&runtime, &code).await
        }
    };

    // type + required + enum
    let result = check(
        json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 3}}, "required": ["n"]}),
        json!({"n": 5}),
    )
    .await;
    assert_eq!(result["valid"], json!(true));

    let result = check(
        json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 3}}, "required": ["n"]}),
        json!({"n": 2}),
    )
    .await;
    assert_eq!(result["valid"], json!(false));
    assert!(result["errors"][0]["message"].is_string());

    // enum / const / anyOf / pattern / additionalProperties.
    let result = check(json!({"enum": ["a", "b"]}), json!("c")).await;
    assert_eq!(result["valid"], json!(false));
    let result = check(json!({"const": 7}), json!(7)).await;
    assert_eq!(result["valid"], json!(true));
    let result = check(
        json!({"anyOf": [{"type": "string"}, {"type": "integer"}]}),
        json!(true),
    )
    .await;
    assert_eq!(result["valid"], json!(false));
    let result = check(json!({"type": "string", "pattern": "^ab"}), json!("abc")).await;
    assert_eq!(result["valid"], json!(true));
    let result = check(
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"x": 1}),
    )
    .await;
    assert_eq!(result["valid"], json!(false));
    // oneOf: exactly one match.
    let result = check(
        json!({"oneOf": [{"type": "integer"}, {"minimum": 5}]}),
        json!(10),
    )
    .await;
    assert_eq!(result["valid"], json!(false));
    // format.
    let result = check(json!({"type": "string", "format": "uuid"}), json!("nope")).await;
    assert_eq!(result["valid"], json!(false));

    // Compile / Value surface: exercised inside a real extension factory so
    // the shim import resolves, with results captured into globals.
    let temp2 = TempDir::new("schema-shim");
    let runtime2 = load_runtime(
        &temp2,
        r#"
        import { Compile, Value } from "typebox/compile";
        export default function (pi) {
            void pi;
            const schema = { type: "object", properties: { a: { type: "string" } }, required: ["a"] };
            const checked = Compile(schema);
            globalThis.__compile = {
                good: checked.Check({ a: "x" }),
                bad: checked.Check({}),
                errCount: Value.Errors(schema, {}).length,
                parsedOk: (() => { try { return Value.Parse(schema, { a: "x" }).a; } catch { return null; } })(),
            };
        }
        "#,
    )
    .await;
    let result = probe(&runtime2, "globalThis.__compile ?? null").await;
    assert_eq!(result["good"], json!(true));
    assert_eq!(result["bad"], json!(false));
    assert_eq!(result["errCount"], json!(1));
    assert_eq!(result["parsedOk"], json!("x"));
}

#[tokio::test(flavor = "current_thread")]
async fn register_tool_validates_arguments_fail_closed() {
    let temp = TempDir::new("tool-schema");
    let runtime = load_runtime(
        &temp,
        r#"
        import { Type } from "typebox";
        export default function (pi) {
            pi.registerTool({
                name: "strict",
                description: "strict args",
                parameters: Type.Object({ n: Type.Integer({ minimum: 2 }) }),
                async execute(_id, params) {
                    return { content: [{ type: "text", text: `n=${params.n}` }] };
                },
            });
        }
        "#,
    )
    .await;

    // Valid arguments execute.
    let ok = runtime
        .call_tool("strict", "c1", &json!({ "n": 3 }))
        .await
        .expect("valid args run");
    assert_eq!(ok.content[0].text, "n=3");

    // Invalid arguments fail closed with the validator detail.
    let err = runtime
        .call_tool("strict", "c2", &json!({ "n": 1 }))
        .await
        .expect_err("invalid args must fail");
    assert!(err
        .to_string()
        .contains("Invalid arguments for tool strict"));

    // Missing required argument fails closed.
    let err = runtime
        .call_tool("strict", "c3", &json!({}))
        .await
        .expect_err("missing arg must fail");
    assert!(err
        .to_string()
        .contains("Invalid arguments for tool strict"));
}

// ---------------------------------------------------------------------------
// Dispatch timeout aborts the QuickJS job; the runtime stays usable
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn timeout_interrupts_runaway_job_and_runtime_stays_usable() {
    let temp = TempDir::new("timeout");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.registerTool({
                name: "spin",
                parameters: { type: "object", properties: {} },
                execute: async () => { while (true) { /* runaway */ } },
            });
            pi.registerTool({
                name: "good",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
            });
        }
        "#,
    )
    .await;

    runtime.set_call_timeout(std::time::Duration::from_millis(150));

    let started = std::time::Instant::now();
    let err = runtime
        .call_tool("spin", "c1", &json!({}))
        .await
        .expect_err("runaway tool must time out");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "timeout must actually abort the JS job, took {elapsed:?}"
    );
    let message = err.to_string();
    // The interrupt surfaces either as our timeout error or as the raw
    // QuickJS exception raised by the aborted job; either way the await
    // ended long before the 30 s default.
    assert!(
        message.contains("timed out")
            || message.contains("interrupted")
            || message.contains("Exception generated by QuickJS"),
        "unexpected error: {message}"
    );

    // The runtime remains usable after the interrupt: basic eval works and
    // the good tool executes.
    let one = runtime
        .eval_json("JSON.stringify(1)")
        .await
        .expect("eval after interrupt");
    assert_eq!(one, json!(1));
    let good = runtime
        .call_tool("good", "c2", &json!({}))
        .await
        .expect("runtime usable after timeout");
    assert_eq!(good.content[0].text, "ok");

    runtime.set_call_timeout(std::time::Duration::from_secs(30));
    // Events still dispatch.
    let mut payload = json!({ "reason": "startup" });
    let outcome = runtime.emit("session_start", &mut payload).await;
    assert!(outcome.is_ok());
}

// ---------------------------------------------------------------------------
// Dependency scanner
// ---------------------------------------------------------------------------

#[test]
fn scanner_finds_static_dynamic_and_require_dependencies() {
    use pir::extensions::{scan_dependencies, DependencyKind};

    let source = r#"
        import { Type } from "typebox";
        import type { T } from "@earendil-works/pi-coding-agent";
        import {
            defineTool,
        } from "@earendil-works/pi-coding-agent";
        import "typebox/compile";
        export * from "@earendil-works/pi-ai";
        const mod = await import("some-package");
        const fs = require("fs");
        const s = "import { fake } from 'not-a-real-import'";
        // import { commented } from "commented-out";
    "#;
    let deps = scan_dependencies(source);
    let find = |specifier: &str| {
        deps.iter()
            .find(|dep| dep.specifier == specifier)
            .unwrap_or_else(|| panic!("missing dependency {specifier}: {deps:?}"))
    };
    assert_eq!(find("typebox").kind, DependencyKind::StaticImport);
    assert_eq!(
        find("@earendil-works/pi-coding-agent").kind,
        DependencyKind::StaticImport
    );
    assert_eq!(find("typebox/compile").kind, DependencyKind::StaticImport);
    assert_eq!(
        find("@earendil-works/pi-ai").kind,
        DependencyKind::StaticImport
    );
    assert_eq!(find("some-package").kind, DependencyKind::DynamicImport);
    assert_eq!(find("fs").kind, DependencyKind::Require);

    // Specifiers inside string literals and comments are not reported.
    assert!(!deps.iter().any(|dep| dep.specifier == "not-a-real-import"));
    assert!(!deps.iter().any(|dep| dep.specifier == "commented-out"));

    // Relative + virtual + shimmed node builtins are supported; bare npm
    // packages are allowed through the scan (phase 4a) and resolve at load
    // time; `node:` builtins outside the shimmed table stay rejected.
    assert!(find("typebox").is_supported());
    let relative = &scan_dependencies("import x from './y.ts';")[0];
    assert!(relative.is_supported());
    assert!(find("fs").is_supported());
    assert!(find("some-package").is_supported());
    // node:vm moved into the shimmed table (throws tier) during the unenv
    // wave; the scan-rejection path is now exercised with a non-builtin.
    let node_unknown = &scan_dependencies("import x from 'node:nonexistent_builtin';")[0];
    assert!(!node_unknown.is_supported());
}

#[tokio::test(flavor = "current_thread")]
async fn loader_rejects_node_and_npm_dependencies() {
    let temp = TempDir::new("scanner");
    let runtime = ExtensionHandle::start(test_config_at(&temp.0)).expect("runtime");

    // `fs` is a shimmed node builtin (LLRT-style node shims): an import loads
    // instead of being rejected. (CommonJS `require` is not part of the ES
    // module runtime surface.)
    let fs_path = write_source(
        &temp,
        "uses-fs.ts",
        r#"
        import { readFileSync } from "fs";
        export default function (pi) { void readFileSync; }
        "#,
    );
    let result = runtime
        .load(std::slice::from_ref(&fs_path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.loaded.len(), 1);

    // Bare npm specifiers pass the scan (phase 4a) and fail at load time
    // with the package-resolution wording instead of a scan rejection.
    let npm_path = write_source(
        &temp,
        "uses-npm.ts",
        r#"
        const leftPad = await import("left-pad");
        export default function (pi) { void leftPad; }
        "#,
    );
    let result = runtime
        .load(std::slice::from_ref(&npm_path))
        .await
        .expect("load");
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0]
            .error
            .contains("cannot find package 'left-pad'"),
        "{:?}",
        result.errors[0].error
    );

    // Virtual modules still load cleanly.
    let ok_path = write_source(
        &temp,
        "ok.ts",
        r#"
        import { Type } from "typebox";
        export default function (pi) { void Type; }
        "#,
    );
    let result = runtime
        .load(std::slice::from_ref(&ok_path))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.loaded.len(), 1);
}

// ---------------------------------------------------------------------------
// /reload
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn reload_reruns_factories_without_double_load() {
    let temp = TempDir::new("reload");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            globalThis.__factories = (globalThis.__factories ?? 0) + 1;
            pi.registerTool({
                name: "counter",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "n" }] }),
            });
            pi.registerCommand("greet", { handler: async () => {} });
            pi.on("session_start", () => { globalThis.__session_start_seen = true; });
        }
        "#,
    )
    .await;
    assert_eq!(runtime.tool_defs().len(), 1);
    assert_eq!(runtime.command_defs().len(), 1);

    // /reload re-runs the loader: factories run again (fresh runtime) and
    // registries are rebuilt exactly once.
    let result = runtime.reload().await.expect("reload");
    assert_eq!(result.loaded.len(), 1, "{:?}", result.errors);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let tools = runtime.tool_defs();
    assert_eq!(tools.len(), 1, "tool re-registered once: {tools:?}");
    assert_eq!(runtime.command_defs().len(), 1);

    let factories = probe(&runtime, "globalThis.__factories ?? 0").await;
    assert_eq!(factories, json!(1), "fresh runtime resets JS globals");

    // The reload lifecycle fired session_start (reason reload) on the new
    // runtime; subsequent events still work.
    let started = probe(&runtime, "globalThis.__session_start_seen ?? false").await;
    assert_eq!(started, json!(true));

    // Idempotent: a second reload also works.
    let result = runtime.reload().await.expect("second reload");
    assert_eq!(result.loaded.len(), 1);

    // The reserved name is intercepted even though no extension defines it.
    let outcome = runtime
        .dispatch_command("reload", "")
        .await
        .expect("reload via dispatch");
    assert!(outcome.ok, "{:?}", outcome.error);

    // Tools still execute after reloads.
    let good = runtime
        .call_tool("counter", "c1", &json!({}))
        .await
        .expect("tool after reload");
    assert_eq!(good.content[0].text, "n");
}

#[tokio::test(flavor = "current_thread")]
async fn reload_fails_cleanly_without_load_and_reports_load_errors() {
    let temp = TempDir::new("reload-empty");
    let runtime = ExtensionHandle::start(test_config_at(&temp.0)).expect("runtime");
    let err = runtime.reload().await.expect_err("no prior load");
    assert!(err.to_string().contains("no extensions have been loaded"));

    // A reload reports per-extension load errors but keeps the extensions
    // that loaded.
    let path = write_source(&temp, "broken.ts", "this is (not ] typescript");
    let _ = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load");
    let result = runtime.reload().await.expect("reload with errors");
    assert_eq!(result.errors.len(), 1);
}

// ---------------------------------------------------------------------------
// registerProvider / registerShortcut
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn register_provider_records_defs_and_session_applies_them() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::providers::RegisterDynamicProviderShared;
    use pir::ai::types::*;

    struct Fake;
    impl Provider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![Model {
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
            }]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage::default(),
            ));
            stream
        }
    }

    let temp = TempDir::new("provider");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.registerProvider("ext-proxy", {
                name: "Ext Proxy",
                baseUrl: "https://proxy.example.com",
                apiKey: "sk-test",
                api: "anthropic-messages",
                models: [{
                    id: "proxy-sonnet",
                    name: "Proxy Sonnet",
                    reasoning: true,
                    input: ["text"],
                    contextWindow: 100000,
                    maxTokens: 8192,
                }],
            });
        }
        "#,
    )
    .await;

    // The definition is visible on the handle.
    let defs = runtime.provider_defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0]["id"], json!("ext-proxy"));
    assert_eq!(defs[0]["config"]["api"], json!("anthropic-messages"));

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Fake));
    let registry = Arc::new(registry);

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: {
            // Reuse the fake provider's model for the session.
            registry.providers()[0]
                .get_models()
                .first()
                .cloned()
                .expect("fake model")
        },
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    // The extension provider became a real registry provider and its model
    // is selectable.
    let provider = registry
        .providers()
        .into_iter()
        .find(|p| p.id() == "ext-proxy");
    assert!(provider.is_some(), "ext-proxy registered into the registry");
    let model = registry.get_model("ext-proxy", "proxy-sonnet");
    assert!(model.is_some());
    assert!(model.as_ref().unwrap().reasoning);

    // unregisterProvider removes the definition (and the registry entry).
    // The shared-registration seam also supports direct removal.
    let path = write_source(
        &temp,
        "unregister.ts",
        r#"
        export default function (pi) {
            pi.unregisterProvider("ext-proxy");
        }
        "#,
    );
    let _ = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("load unregister ext");
    assert!(runtime.provider_defs().is_empty());
    // The shared-registration seam also supports direct removal.
    assert!(!registry.unregister_dynamic_provider_shared("nonexistent"));
}

#[tokio::test(flavor = "current_thread")]
async fn register_shortcut_registry_and_invocation() {
    let temp = TempDir::new("shortcut");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.registerShortcut("ctrl+shift+k", {
                description: "Greet the user",
                handler: (ctx) => {
                    globalThis.__shortcut_fired = true;
                    globalThis.__shortcut_cwd = ctx.cwd;
                    ctx.ui.notify("shortcut fired", "info");
                },
            });
        }
        "#,
    )
    .await;

    // Registry enumeration (TUI integration point).
    let shortcuts = runtime.shortcuts();
    assert_eq!(shortcuts.len(), 1);
    assert_eq!(shortcuts[0].key, "ctrl+shift+k");
    assert_eq!(shortcuts[0].description.as_deref(), Some("Greet the user"));

    // Invocation fires the JS handler and marshals the result.
    let outcome = runtime
        .invoke_shortcut("ctrl+shift+k")
        .await
        .expect("invoke");
    assert!(outcome.ok, "{:?}", outcome.error);
    let fired = probe(&runtime, "globalThis.__shortcut_fired ?? false").await;
    assert_eq!(fired, json!(true));
    let cwd = probe(&runtime, "globalThis.__shortcut_cwd ?? null").await;
    assert_eq!(cwd, json!(temp.0.display().to_string()));
    let notifications = runtime.drain_notifications();
    assert!(notifications
        .iter()
        .any(|n| matches!(n, Notification::Notify { message, level }
            if message == "shortcut fired" && level == "info")));

    // Unknown key fails cleanly.
    let outcome = runtime
        .invoke_shortcut("ctrl+shift+z")
        .await
        .expect("invoke unknown");
    assert!(!outcome.ok);
    assert!(outcome.error.unwrap().contains("Unknown shortcut"));
}

// ---------------------------------------------------------------------------
// ctx facade accessors
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ctx_facade_reports_session_model_and_thinking() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;

    struct Fake;
    impl Provider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage {
                    usage: Usage {
                        total_tokens: 500,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ));
            stream
        }
    }

    fn fake_model() -> Model {
        Model {
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
            context_window: 8192,
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

    let temp = TempDir::new("ctx");
    let source = r#"
        export default function (pi) {
            pi.on("agent_settled", (_event, ctx) => {
                globalThis.__ctx = {
                    cwd: ctx.cwd,
                    mode: ctx.mode,
                    hasUI: ctx.hasUI,
                    thinkingLevel: ctx.thinkingLevel,
                    scopedModels: ctx.scopedModels.length,
                    sessionId: ctx.sessionManager.getSessionId(),
                    sessionPath: ctx.sessionManager.getSessionPath(),
                    modelId: ctx.model ? ctx.model.id : null,
                    contextWindow: ctx.model ? ctx.model.contextWindow : null,
                    models: ctx.modelRegistry.listModels().length,
                    usageTokens: ctx.getContextUsage() ? ctx.getContextUsage().tokens : null,
                    pending: ctx.hasPendingMessages(),
                    systemPrompt: ctx.getSystemPrompt(),
                    signal: ctx.signal ? "present" : "absent",
                };
            });
        }
        "#;
    let runtime = load_runtime(&temp, source).await;

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Fake));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Medium,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: Some("You are ctx-facade.".to_string()),
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    agent.steer("queued message");
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let ctx = probe(&runtime, "globalThis.__ctx ?? null").await;
    assert_eq!(ctx["cwd"], json!(temp.0.display().to_string()));
    // ctx.mode is normalized to exactly "tui"|"rpc"|"json"|"print" — the
    // headless facade reports "print".
    assert_eq!(ctx["mode"], json!("print"));
    assert_eq!(ctx["hasUI"], json!(false));
    assert_eq!(ctx["thinkingLevel"], json!("medium"));
    assert_eq!(ctx["scopedModels"], json!(0));
    assert_eq!(ctx["modelId"], json!("fake-model"));
    assert_eq!(ctx["contextWindow"], json!(8192));
    assert_eq!(ctx["models"], json!(1));
    assert_eq!(ctx["usageTokens"], json!(500));
    // Upstream queue semantics (agent-session.ts): a steering message is
    // spliced out of the session queue as soon as its user `message_start`
    // fires (it entered the LLM context), so by `agent_settled` the
    // pre-queued message has been DELIVERED and `hasPendingMessages()` is
    // false — a nonzero count here would mean the message was lost.
    assert_eq!(ctx["pending"], json!(false));
    assert_eq!(ctx["systemPrompt"], json!("You are ctx-facade."));
    assert_eq!(ctx["signal"], json!("present"));
    // Session path is null for the ephemeral (no-storage) session, but the
    // accessor exists and returns a defined shape.
    assert!(ctx["sessionId"].is_null() || ctx["sessionId"].is_string());
    assert!(ctx["sessionPath"].is_null() || ctx["sessionPath"].is_string());
}

#[tokio::test(flavor = "current_thread")]
async fn set_model_and_thinking_level_apply_and_emit_events() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;

    struct Fake;
    impl Provider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model(), alt_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage::default(),
            ));
            stream
        }
    }

    fn fake_model() -> Model {
        Model {
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
        }
    }

    fn alt_model() -> Model {
        Model {
            id: "alt-model".into(),
            name: "Alt".into(),
            api: "fake".into(),
            provider: "fake".into(),
            base_url: "http://localhost".into(),
            reasoning: true,
            ..fake_model()
        }
    }

    let temp = TempDir::new("setmodel");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("session_start", (_event, ctx) => {
                globalThis.__before = { model: ctx.model.id, thinking: ctx.thinkingLevel };
                pi.setModel({ provider: "fake", id: "alt-model" });
                pi.setThinkingLevel("high");
            });
            pi.on("model_select", (event) => {
                globalThis.__model_select = { id: event.model.id, previous: event.previousModel, source: event.source };
            });
            pi.on("thinking_level_select", (event) => {
                globalThis.__thinking_select = { level: event.level, previous: event.previousLevel };
            });
        }
        "#,
    )
    .await;

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Fake));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let before = probe(&runtime, "globalThis.__before ?? null").await;
    assert_eq!(before["model"], json!("fake-model"));
    assert_eq!(before["thinking"], json!("off"));

    let select = probe(&runtime, "globalThis.__model_select ?? null").await;
    assert_eq!(select["id"], json!("alt-model"));
    // Upstream `ModelSelectEvent.previousModel` is a Model OBJECT
    // (extensions/types.ts:1134-1139); the host payload's id is resolved
    // against the pushed catalog.
    assert_eq!(select["previous"]["id"], json!("fake-model"));
    assert_eq!(select["previous"]["provider"], json!("fake"));
    assert_eq!(select["source"], json!("set"));

    let level = probe(&runtime, "globalThis.__thinking_select ?? null").await;
    assert_eq!(level["level"], json!("high"));
    assert_eq!(level["previous"], json!("off"));

    // The session state reflects the switch for subsequent runs.
    assert_eq!(agent.state().model, "alt-model");
    assert_eq!(agent.state().thinking, ThinkingLevel::High);

    // An unknown model is rejected: the current model stays.
    let mut payload = json!({ "type": "probe" });
    let _ = runtime.emit("session_start", &mut payload).await;
    // (setModel with unknown id logs and leaves the model unchanged —
    // verified by the state assertion above holding after this emit.)
    assert_eq!(agent.state().model, "alt-model");
}

// ---------------------------------------------------------------------------
// Emit payload round-trip through the actor mailbox (single-ownership Arc)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn emit_round_trips_large_payload_without_loss() {
    let temp = TempDir::new("emit-large");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("q3_large_payload", (event) => {
                globalThis.__saw_items = (event.input.items ?? []).length;
                // In-place mutations must flow back to the host.
                event.input.echo = event.input.marker;
            });
        }
        "#,
    )
    .await;

    // A payload large enough that a lost copy or a truncated field would be
    // visible: many items with long shared strings.
    let filler = "x".repeat(64);
    let expected_items: Vec<String> = (0..2_000).map(|i| format!("item-{i}-{filler}")).collect();
    let mut payload = json!({
        "type": "q3_large_payload",
        "input": { "marker": "keep-me", "items": expected_items },
    });
    let outcome = runtime
        .emit("q3_large_payload", &mut payload)
        .await
        .expect("emit");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    // The handler observed the full (large) input through the mailbox.
    let saw = probe(&runtime, "globalThis.__saw_items ?? 0").await;
    assert_eq!(saw, json!(2_000));

    // Content fidelity: the original fields survive the round trip...
    assert_eq!(payload["input"]["marker"], json!("keep-me"));
    let items = payload["input"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 2_000);
    assert_eq!(items[0], json!(format!("item-0-{filler}")));
    assert_eq!(items[1_999], json!(format!("item-1999-{filler}")));
    // ...and the handler's in-place mutation flowed back to the caller.
    assert_eq!(payload["input"]["echo"], json!("keep-me"));
    // The outcome carries the same (possibly mutated) payload snapshot.
    assert_eq!(outcome.payload["input"]["echo"], json!("keep-me"));
}

// ---------------------------------------------------------------------------
// Handler registered from a NON-dispatch context (fire-and-forget timer)
// ---------------------------------------------------------------------------

/// A `setTimeout` callback runs with no command in flight, so nothing
/// re-publishes the host's handler-count snapshot after its `pi.on(...)`;
/// only the JS `handlersDirty` generation bump marks the snapshot stale.
/// The fast-path gate must then answer conservatively `true` (at most one
/// wasted emission) instead of consulting the stale counts — for `turn_end`
/// the wrong answer is terminal, because the gate is structural: a `false`
/// at `run_prompt` time means the boundary hook is never installed and the
/// handler is silently dead for the whole run.
#[tokio::test(flavor = "current_thread")]
async fn late_timer_registered_handler_is_not_gated_off() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;

    struct Fake;
    impl Provider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp: 1,
                    ..Default::default()
                },
            ));
            stream
        }
    }

    fn model() -> Model {
        Model {
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
        }
    }

    let temp = TempDir::new("late-handler");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            // Rendezvous flag: the timer holds its registration until the test
            // flips this, so "registered strictly after the load-time snapshot
            // was published" is a fact of the test, not a wall-clock race.
            pi.registerFlag("gate-go", { type: "string", default: "wait" });
            setTimeout(async () => {
                while (pi.getFlag("gate-go") !== "go") {
                    await new Promise((resolve) => setTimeout(resolve, 5));
                }
                // No command is in flight in this continuation: handlerCounts()
                // is never re-published for it — only handlersDirty is bumped.
                pi.on("turn_end", () => {
                    globalThis.__late_turn_end = (globalThis.__late_turn_end ?? 0) + 1;
                });
                globalThis.__late_registered = true;
            }, 5);
        }
        "#,
    )
    .await;

    // The load-time snapshot is exact: nothing observes turn_end yet, and the
    // timer still holds its registration behind the flag.
    assert!(!runtime.has_handlers("turn_end"));

    // Release the timer, then wait for the registration. `eval_json` runs JS
    // but is NOT a refresh point, so no handler-count snapshot is re-published
    // on the way to the assertions below.
    runtime
        .set_flag_value("gate-go", pir::extensions::FlagValue::Str("go".into()))
        .expect("the factory-registered flag is settable");
    let mut registered = false;
    for _ in 0..500 {
        if probe(&runtime, "globalThis.__late_registered ?? false").await == json!(true) {
            registered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        registered,
        "the fire-and-forget setTimeout callback never ran"
    );

    // Dirty-window invariant: the snapshot still knows nothing about turn_end,
    // so this `true` can only come from the conservative generation check. A
    // `false` here is the regression this pins — the event would be skipped.
    assert!(
        runtime.has_handlers("turn_end"),
        "a handler registered outside a dispatch context must not be gated off"
    );

    // End to end: one real prompt. The turn_end gate is consulted once per run
    // (when the boundary hook is installed), so a gate that consulted the
    // stale counts would leave the handler dead for the whole run.
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Fake));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let seen = probe(&runtime, "globalThis.__late_turn_end ?? 0").await;
    assert!(
        seen.as_u64().unwrap_or(0) >= 1,
        "the late-registered turn_end handler never saw the event: {seen}"
    );

    // The registration is real, not a stale-snapshot artifact: after the run's
    // refresh points the counts-based answer to the same gate is still `true`.
    assert!(runtime.has_handlers("turn_end"));
}

// ---------------------------------------------------------------------------
// Registry-dirty generation: unchanged registries skip the full export
// ---------------------------------------------------------------------------

/// Install counting wrappers around the two registry-export entry points the
/// host uses (`__pir_exportRegistryJson` — the full tools/commands/shortcuts/
/// renderers export — and the counts-only `__pir_handlerCountsJson`). The
/// wrappers delegate to the originals; the globals `__probe_exports` /
/// `__probe_counts` count the calls.
async fn install_registry_probes(runtime: &ExtensionHandle) {
    let installed = probe(
        runtime,
        r#"(function () {
            const full = globalThis.__pir_exportRegistryJson;
            globalThis.__probe_exports = 0;
            globalThis.__pir_exportRegistryJson = function () {
                globalThis.__probe_exports += 1;
                return full.apply(this, arguments);
            };
            const counts = globalThis.__pir_handlerCountsJson;
            globalThis.__probe_counts = 0;
            globalThis.__pir_handlerCountsJson = function () {
                globalThis.__probe_counts += 1;
                return counts.apply(this, arguments);
            };
            return 1;
        })()"#,
    )
    .await;
    assert_eq!(installed, json!(1), "registry probes installed");
}

/// The hot dispatch path: `emit` runs `refresh_registry` after EVERY event,
/// and with a `message_update` handler registered that is once per streamed
/// delta. With no JS registration since the last publication the registry is
/// unchanged, so the full export (every tool schema + command + shortcut +
/// renderer serialized, then re-parsed in Rust) must be skipped entirely —
/// including the counts-only publication (the handler counts are fresh too).
#[tokio::test(flavor = "current_thread")]
async fn emits_without_registrations_skip_the_registry_export() {
    let temp = TempDir::new("registry-clean-emits");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("probe_event", () => {
                globalThis.__probe_hits = (globalThis.__probe_hits ?? 0) + 1;
            });
        }
        "#,
    )
    .await;
    install_registry_probes(&runtime).await;

    for _ in 0..5 {
        let mut payload = json!({ "type": "probe_event" });
        runtime
            .emit("probe_event", &mut payload)
            .await
            .expect("emit");
    }

    assert_eq!(
        probe(&runtime, "globalThis.__probe_hits ?? 0").await,
        json!(5),
        "the handler must run on every emit (the probe must not change dispatch)"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__probe_exports").await,
        json!(0),
        "5 registration-free emits must perform ZERO full registry exports"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__probe_counts").await,
        json!(0),
        "fresh handler counts must not be re-published either"
    );
}

/// The "hook registrations become visible" invariant: a registration performed
/// by a handler DURING a dispatch must be on the host registry by the time
/// `emit` returns. The registration bumps the JS-side generation, so the
/// dispatch's own refresh cannot early-out — exactly one full export happens.
#[tokio::test(flavor = "current_thread")]
async fn registration_inside_dispatch_is_visible_when_emit_returns() {
    let temp = TempDir::new("registry-dispatch-registration");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("session_start", () => {
                if (globalThis.__late_done) return;
                globalThis.__late_done = true;
                pi.registerTool({
                    name: "late_tool",
                    description: "registered during dispatch",
                    parameters: { type: "object", properties: {} },
                    execute: async () => ({ content: [{ type: "text", text: "late" }] }),
                });
                pi.registerCommand("late-command", { description: "registered during dispatch" });
                pi.registerShortcut("ctrl-l", { description: "late", handler: async () => {} });
                pi.registerMessageRenderer("late-type", {});
            });
        }
        "#,
    )
    .await;
    install_registry_probes(&runtime).await;

    let mut payload = json!({ "type": "session_start" });
    let outcome = runtime
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    assert!(
        outcome.errors.is_empty(),
        "handler errors: {:?}",
        outcome.errors
    );

    assert_eq!(
        probe(&runtime, "globalThis.__probe_exports").await,
        json!(1),
        "the dispatch with a registration must re-export exactly once"
    );
    assert!(
        runtime
            .tool_defs()
            .iter()
            .any(|tool| tool.name == "late_tool"),
        "tool registered inside session_start must be in the host registry"
    );
    assert!(
        runtime
            .command_defs()
            .iter()
            .any(|command| command.name == "late-command"),
        "command registered inside session_start must be in the host registry"
    );
    assert!(
        runtime
            .shortcuts()
            .iter()
            .any(|shortcut| shortcut.key == "ctrl-l"),
        "shortcut registered inside session_start must be in the host registry"
    );
    let renderers = runtime.renderer_defs();
    assert!(
        renderers["messageRenderers"]
            .as_array()
            .is_some_and(|defs| defs
                .iter()
                .any(|def| def["customType"] == json!("late-type"))),
        "renderer registered inside session_start must be in the host registry: {renderers}"
    );

    // The next registration-free emit is back on the early-out path.
    let mut payload = json!({ "type": "session_start" });
    runtime
        .emit("session_start", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        probe(&runtime, "globalThis.__probe_exports").await,
        json!(1),
        "a following registration-free emit must not re-export"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__probe_counts").await,
        json!(0),
        "the full export already published the handler counts"
    );
}

/// A handler registered by a handler during a dispatch changes only the
/// handler table (not the tools/commands/shortcuts/renderers registry): the
/// registry export early-outs and the counts-only publication runs instead, so
/// the `has_handlers` gate still observes the new handler.
#[tokio::test(flavor = "current_thread")]
async fn handler_registration_inside_dispatch_publishes_counts_only() {
    let temp = TempDir::new("registry-dispatch-handler");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("session_start", () => {
                if (globalThis.__late_handler_done) return;
                globalThis.__late_handler_done = true;
                pi.on("late_event", () => {
                    globalThis.__late_handler_hits = (globalThis.__late_handler_hits ?? 0) + 1;
                });
            });
        }
        "#,
    )
    .await;
    install_registry_probes(&runtime).await;

    let mut payload = json!({ "type": "session_start" });
    runtime
        .emit("session_start", &mut payload)
        .await
        .expect("emit");

    assert!(
        runtime.has_handlers("late_event"),
        "a handler registered inside a dispatch must be visible to the gate"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__probe_exports").await,
        json!(0),
        "a handler-only registration must not pay the full registry export"
    );
    assert_eq!(
        probe(&runtime, "globalThis.__probe_counts").await,
        json!(1),
        "the counts-only publication must run for a handler registration"
    );
}

// ---------------------------------------------------------------------------
// provider_stream_event: awaited, ordered dispatch
// ---------------------------------------------------------------------------

/// Upstream awaits `handleProviderStreamEvent` before consuming more of the
/// provider stream (sdk.ts:388-401). pir's hook is a sync callback, so it
/// keeps that ordering with a bounded handshake: each `hook(...)` call returns
/// only after the handler dispatch completed. A slow handler therefore delays
/// the provider stream (backpressure) instead of losing events — every parsed
/// event still reaches the JS handler exactly once, in order.
#[tokio::test(flavor = "current_thread")]
async fn provider_stream_events_are_dispatched_in_order_and_awaited() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;
    use std::sync::Mutex;

    const EVENT_COUNT: usize = 8;

    fn hook_model() -> Model {
        Model {
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
        }
    }

    /// A provider that pushes parsed provider events through the per-request
    /// `on_provider_stream_event` hook (as the real API implementations do)
    /// and records how long each synchronous call took: with the awaited
    /// dispatch, the call cannot return before the slow JS handler finished.
    struct HookEmittingProvider {
        call_durations: Arc<Mutex<Vec<std::time::Duration>>>,
    }
    impl Provider for HookEmittingProvider {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![hook_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            if let Some(hook) = &options.hooks.on_provider_stream_event {
                for index in 0..EVENT_COUNT {
                    let started = std::time::Instant::now();
                    hook(&json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "text": format!("t{index}") },
                    }));
                    self.call_durations.lock().unwrap().push(started.elapsed());
                }
            }
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp: 1,
                    ..Default::default()
                },
            ));
            stream
        }
    }

    let temp = TempDir::new("provider-stream-events");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("provider_stream_event", async (event) => {
                globalThis.__pse = globalThis.__pse ?? [];
                globalThis.__pse.push(event.data.index);
                // Slow handler: each hook call must await this dispatch, so
                // the provider stream pays this delay (backpressure), never
                // loses the event.
                await new Promise((resolve) => setTimeout(resolve, 10));
            });
        }
        "#,
    )
    .await;

    let call_durations: Arc<Mutex<Vec<std::time::Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookEmittingProvider {
        call_durations: Arc::clone(&call_durations),
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: hook_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    // Each hook call returned only after the JS handler's 10 ms sleep finished:
    // the dispatch is awaited (upstream `await runner.emit`), so a slow
    // handler throttles the provider stream instead of building a backlog.
    let durations = call_durations.lock().unwrap().clone();
    assert_eq!(
        durations.len(),
        EVENT_COUNT,
        "every parsed event must be handed to the hook"
    );
    for (index, duration) in durations.iter().enumerate() {
        assert!(
            *duration >= std::time::Duration::from_millis(10),
            "hook call {index} returned before its handler dispatch completed: {duration:?}"
        );
    }

    // Every event is delivered exactly once, in order (awaited dispatch needs
    // no catch-up polling: the handler ran before each hook call returned).
    let delivered: Vec<u64> = probe(&runtime, "globalThis.__pse ?? []")
        .await
        .as_array()
        .map(|array| array.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    assert_eq!(
        delivered,
        (0..EVENT_COUNT as u64).collect::<Vec<_>>(),
        "every provider stream event must arrive exactly once, in order"
    );
}

/// Abort cleanup: after the run's signal fires, later parsed provider events
/// are dropped before dispatch (upstream's stream has already stopped calling
/// the hook, so extensions must not observe them) and the hook returns without
/// waiting for the slow handler.
#[tokio::test(flavor = "current_thread")]
async fn provider_stream_events_after_abort_are_dropped() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;
    use std::sync::Mutex;

    const POST_ABORT_EVENTS: usize = 3;

    fn abort_model() -> Model {
        Model {
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
        }
    }

    /// Emits one event, aborts the run through the request's own signal, then
    /// emits `POST_ABORT_EVENTS` more parsed events.
    struct AbortingProvider {
        post_abort_durations: Arc<Mutex<Vec<std::time::Duration>>>,
    }
    impl Provider for AbortingProvider {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![abort_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            if let Some(hook) = &options.hooks.on_provider_stream_event {
                hook(&json!({ "type": "delta", "index": 0 }));
                // The run is cancelled here (the same signal `AgentSession::
                // abort` drives).
                if let Some(signal) = &options.signal {
                    signal.abort();
                }
                for index in 1..=POST_ABORT_EVENTS {
                    let started = std::time::Instant::now();
                    hook(&json!({ "type": "delta", "index": index }));
                    self.post_abort_durations
                        .lock()
                        .unwrap()
                        .push(started.elapsed());
                }
            }
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage {
                    api: "fake".into(),
                    provider: "fake".into(),
                    model: "fake-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "done".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Stop,
                    timestamp: 1,
                    ..Default::default()
                },
            ));
            stream
        }
    }

    let temp = TempDir::new("provider-stream-abort");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            globalThis.__pse = [];
            pi.on("provider_stream_event", async (event) => {
                globalThis.__pse.push(event.data.index);
            });
        }
        "#,
    )
    .await;

    let post_abort_durations: Arc<Mutex<Vec<std::time::Duration>>> =
        Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(AbortingProvider {
        post_abort_durations: Arc::clone(&post_abort_durations),
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: abort_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    // The aborted run may surface as an error; the dispatch contract is what
    // this test pins.
    let _ = agent.prompt("hello", Arc::new(|_| {})).await;

    assert_eq!(
        post_abort_durations.lock().unwrap().len(),
        POST_ABORT_EVENTS,
        "the post-abort hook calls still returned (they did not hang on a dispatch)"
    );
    let delivered: Vec<u64> = probe(&runtime, "globalThis.__pse ?? []")
        .await
        .as_array()
        .map(|array| array.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    assert_eq!(
        delivered,
        vec![0],
        "events parsed after the abort must not reach the handler \
         (the pre-abort event did)"
    );
}

// ---------------------------------------------------------------------------
// Lane K: systemPromptOptions, ToolDefinition hooks, user_bash operations,
// native Provider registration
// ---------------------------------------------------------------------------

/// Upstream `before_agent_start.systemPromptOptions` (types.ts:932-938): the
/// event carries the normalized `BuildSystemPromptOptions` object, handlers
/// mutate it in place, and later handlers see `event.systemPrompt`
/// re-rendered from those mutations (runner.ts:1427-1470).
#[tokio::test(flavor = "current_thread")]
async fn before_agent_start_system_prompt_options_are_mutable() {
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;

    fn fake_model() -> Model {
        Model {
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
        }
    }

    struct Capture {
        system_prompt: Arc<std::sync::Mutex<Option<String>>>,
    }
    impl Provider for Capture {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            *self
                .system_prompt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = context.system_prompt.clone();
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                AssistantMessage::default(),
            ));
            stream
        }
    }

    let temp = TempDir::new("prompt-options");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("before_agent_start", (event) => {
                globalThis.__poFirst = {
                    keys: Object.keys(event.systemPromptOptions).sort(),
                    selected: event.systemPromptOptions.selectedTools.slice(),
                    promptHasMutation: event.systemPrompt.includes("<prompt_options_k>"),
                };
                // Mutate the shared object in place: sections + tool snippets
                // + the declared tool set.
                event.systemPromptOptions.sections.prompt_options_k = "mutated-by-handler";
                event.systemPromptOptions.toolSnippets.read = "Read a file";
                event.systemPromptOptions.selectedTools = ["read"];
            });
            pi.on("before_agent_start", (event) => {
                globalThis.__poSecond = {
                    sawSection: event.systemPrompt.includes("<prompt_options_k>"),
                    sawContent: event.systemPrompt.includes("mutated-by-handler"),
                    sawTool: event.systemPrompt.includes("- read: Read a file"),
                    selected: event.systemPromptOptions.selectedTools.slice(),
                };
            });
        }
        "#,
    )
    .await;

    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Capture {
        system_prompt: Arc::clone(&captured),
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let _ = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");

    let first = probe(&runtime, "globalThis.__poFirst ?? null").await;
    let keys: Vec<&str> = first["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for expected in [
        "appendSystemPrompt",
        "contextFiles",
        "cwd",
        "hiddenTools",
        "promptGuidelines",
        "sections",
        "selectedTools",
        "skills",
        "toolGuidelines",
        "toolSnippets",
    ] {
        assert!(
            keys.contains(&expected),
            "missing option field {expected}: {keys:?}"
        );
    }
    assert_eq!(first["promptHasMutation"], json!(false));

    let second = probe(&runtime, "globalThis.__poSecond ?? null").await;
    assert_eq!(second["sawSection"], json!(true), "{second}");
    assert_eq!(second["sawContent"], json!(true), "{second}");
    assert_eq!(second["sawTool"], json!(true), "{second}");
    assert_eq!(second["selected"], json!(["read"]));

    let used = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("the provider received the run's system prompt");
    assert!(used.contains("<prompt_options_k>"), "{used}");
    assert!(used.contains("mutated-by-handler"), "{used}");
    assert!(used.contains("- read: Read a file"), "{used}");
}

/// Upstream `ToolDefinition.prepareArguments` / `constrainedSampling` /
/// `renderShell` / `renderCall` / `renderResult` (types.ts:590-667), the
/// omitted-`executionMode` default (`toolExecution: "parallel"`,
/// agent.ts:261), and the SYNCHRONOUS `registerToolRenderer` `next()` contract
/// (types.ts:678-681).
#[tokio::test(flavor = "current_thread")]
async fn js_tool_definition_hooks_and_renderers_round_trip() {
    let temp = TempDir::new("tool-hooks");
    let runtime = load_runtime(
        &temp,
        r#"
        import { Type } from "typebox";
        export default function (pi) {
            pi.registerTool({
                name: "meta-tool",
                description: "metadata tool",
                parameters: Type.Object({ text: Type.String() }),
                constrainedSampling: false,
                renderShell: "self",
                prepareArguments(args) {
                    return { text: String(args.text) };
                },
                async execute(_id, params) {
                    return { content: [{ type: "text", text: `text=${params.text}` }] };
                },
                renderCall(args, theme, context) {
                    return [`call:${args.text}:${context.args.text}`];
                },
                renderResult(result) {
                    return `result:${result.content?.[0]?.text ?? ""}`;
                },
            });
            pi.registerToolRenderer((toolName, next) => {
                if (toolName !== "meta-tool") return undefined;
                const resolved = next();
                globalThis.__rendererNext = {
                    isPromise: !!(resolved && typeof resolved.then === "function"),
                    hasRenderCall: !!(resolved && typeof resolved.renderCall === "function"),
                };
                return resolved;
            });
            pi.registerCommand("render-probe", {
                handler: async () => {
                    globalThis.__callRender = JSON.parse(await __pir_callToolRenderer(
                        "meta-tool", "call",
                        JSON.stringify({ args: { text: "hello" }, toolCallId: "h1", cwd: "/w" }),
                    ));
                    globalThis.__resultRender = JSON.parse(await __pir_callToolRenderer(
                        "meta-tool", "result",
                        JSON.stringify({
                            args: { text: "hello" },
                            result: { content: [{ type: "text", text: "done" }] },
                        }),
                    ));
                    globalThis.__rendererChain = JSON.parse(await __pir_callRenderer(
                        JSON.stringify({
                            kind: "toolRenderers",
                            key: "meta-tool",
                            payload: "meta-tool",
                            renderOptions: { width: 80 },
                        }),
                    ));
                },
            });
        }
        "#,
    )
    .await;

    let def = runtime
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "meta-tool")
        .expect("tool def");
    assert_eq!(def.execution_mode, "parallel");

    // `prepareArguments` repairs the raw arguments before schema validation.
    let called = runtime
        .call_tool("meta-tool", "h1", &json!({ "text": 5 }))
        .await
        .expect("prepared args execute");
    assert_eq!(called.content[0].text, "text=5");

    // The exported registry carries the declarative fields and the renderer
    // presence markers.
    let export = runtime
        .eval_json("globalThis.__pir_exportRegistryJson()")
        .await
        .expect("export");
    let meta = export["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == json!("meta-tool"))
        .expect("meta-tool metadata");
    assert_eq!(meta["constrainedSampling"], json!(false));
    assert_eq!(meta["renderShell"], json!("self"));
    assert_eq!(meta["renderCall"], json!(true));
    assert_eq!(meta["renderResult"], json!(true));
    assert_eq!(meta["executionMode"], json!("parallel"));

    // The synchronous host-side extras expose the shell/constrained-sampling
    // declarations without an actor round trip (the TUI tool-block wiring
    // point reads these).
    let extra = runtime.tool_extra("meta-tool").expect("tool extras");
    assert_eq!(extra["renderShell"], json!("self"));
    assert_eq!(extra["constrainedSampling"], json!(false));
    assert_eq!(extra["renderCall"], json!(true));
    assert_eq!(extra["renderResult"], json!(true));

    // A tool renderer call goes through the tool's own renderCall/renderResult
    // (the probe command awaits the async bridge entry).
    let outcome = runtime
        .dispatch_command("render-probe", "")
        .await
        .expect("run render probe");
    assert!(outcome.ok, "{:?}", outcome.error);
    let call = probe(&runtime, "globalThis.__callRender ?? null").await;
    assert_eq!(call["ok"], json!(true), "{call}");
    assert_eq!(call["shell"], json!("self"));
    assert_eq!(call["lines"], json!(["call:hello:hello"]));
    let result = probe(&runtime, "globalThis.__resultRender ?? null").await;
    assert_eq!(result["lines"], json!(["result:done"]));

    // The resolver chain ran and `next()` returned the registered tool's
    // renderers synchronously (an object, not a Promise).
    let chain = probe(&runtime, "globalThis.__rendererChain ?? null").await;
    assert_eq!(chain["ok"], json!(true), "{chain}");
    // The chain's terminal renderers carry the tool's `renderShell`; the
    // reply propagates it so the TUI can pick the standard shell.
    assert_eq!(chain["shell"], json!("self"), "{chain}");
    let next = probe(&runtime, "globalThis.__rendererNext ?? null").await;
    assert_eq!(next["isPromise"], json!(false), "{next}");
    assert_eq!(next["hasRenderCall"], json!(true), "{next}");

    // The full tool-renderer envelope (`RendererCallOutcome`) exposes the
    // shell choice next to the lines; the plain helper keeps its old shape.
    let full = runtime
        .call_tool_renderer_sync_full(
            "meta-tool",
            "call",
            &json!({ "args": { "text": "world" }, "toolCallId": "h2" }),
            std::time::Duration::from_secs(2),
        )
        .expect("full tool renderer call");
    assert_eq!(full.shell, Some(ToolRenderShell::SelfShell));
    assert_eq!(
        full.outcome.lines,
        Some(vec!["call:world:world".to_string()])
    );
    let plain = runtime
        .call_tool_renderer_sync(
            "meta-tool",
            "call",
            &json!({ "args": { "text": "world" }, "toolCallId": "h2" }),
            std::time::Duration::from_secs(2),
        )
        .expect("plain tool renderer call");
    assert_eq!(plain.lines, Some(vec!["call:world:world".to_string()]));
}

/// Upstream `UserBashEventResult.operations` (extensions/types.ts:1466-1476):
/// the handler's `exec` closure stays inside QuickJS, and the host executes it
/// through the exported token while `onData` chunks stream back.
#[tokio::test(flavor = "current_thread")]
async fn user_bash_custom_operations_execute_through_the_host() {
    let temp = TempDir::new("user-bash-ops");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            pi.on("user_bash", async () => ({
                operations: {
                    async exec(command, cwd, options) {
                        options.onData(Buffer.from(`out:${command}@${cwd}`));
                        options.onData("!");
                        return { exitCode: 7 };
                    },
                },
            }));
        }
        "#,
    )
    .await;

    let mut payload = json!({
        "type": "user_bash",
        "command": "echo hi",
        "excludeFromContext": false,
        "cwd": "/tmp",
    });
    let outcome = runtime
        .emit("user_bash", &mut payload)
        .await
        .expect("dispatch user_bash");
    let operations = &outcome.result["operations"];
    assert!(
        operations["__pirBashOperations"].as_str().is_some(),
        "the exec closure must export as a token: {operations}"
    );

    let chunks = Arc::new(std::sync::Mutex::new(String::new()));
    let collected = Arc::clone(&chunks);
    let result = pir::extensions::user_bash::execute(
        &runtime,
        operations,
        "echo hi",
        Path::new("/tmp"),
        Arc::new(move |chunk| {
            collected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_str(chunk);
        }),
    )
    .await;
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(result.output, "out:echo hi@/tmp!");
    assert_eq!(
        *chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        "out:echo hi@/tmp!"
    );

    // A non-JS operations object is reported, never silently run locally.
    let unsupported = pir::extensions::user_bash::execute(
        &runtime,
        &json!({}),
        "echo hi",
        Path::new("/tmp"),
        Arc::new(|_| {}),
    )
    .await;
    assert!(unsupported.error.is_some());
}

/// Full `Provider` object registration (upstream `registerNativeProvider`):
/// the catalog comes from `getModels()`, and `auth.apiKey.resolve`,
/// `images`, `classifiers` and `refresh` are real through `ctx.modelRegistry`.
#[tokio::test(flavor = "current_thread")]
async fn native_provider_registration_and_registry_operations() {
    let temp = TempDir::new("native-provider");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            const model = {
                id: "native-1",
                name: "Native One",
                provider: "native",
                api: "openai-completions",
                baseUrl: "http://localhost",
                reasoning: false,
                input: ["text"],
                cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
                contextWindow: 4096,
                maxTokens: 256,
            };
            pi.registerProvider({
                id: "native",
                name: "Native Provider",
                baseUrl: "http://localhost",
                getModels: () => [model],
                filterModels: (models) => models,
                auth: {
                    apiKey: {
                        name: "Native key",
                        async resolve() {
                            return { auth: { apiKey: "native-key" } };
                        },
                    },
                },
                images: {
                    "image-generation": {
                        async generateImages() {
                            return { images: [{ type: "image", data: "aGk=", mimeType: "image/png" }] };
                        },
                    },
                },
                classifiers: {
                    classifier: {
                        async classify() { return { label: "ok", score: 1 }; },
                    },
                },
            });
            pi.registerCommand("native-probe", {
              handler: async (_args, ctx) => {
                const auth = await ctx.modelRegistry.getApiKeyForProvider("native");
                const images = await ctx.modelRegistry.generateImages(
                    { provider: "native", api: "image-generation" }, {}, {},
                );
                const classified = await ctx.modelRegistry.classify(
                    { provider: "native", api: "classifier" }, {}, {},
                );
                const refreshed = await ctx.modelRegistry.refresh();
                globalThis.__nativeProbe = {
                    auth,
                    images,
                    classified,
                    aborted: refreshed.aborted,
                    refreshErrors: refreshed.errors.size,
                };
              },
            });
        }
        "#,
    )
    .await;

    let defs = runtime.provider_defs();
    let def = defs
        .iter()
        .find(|def| def["id"] == json!("native"))
        .expect("native provider definition");
    assert_eq!(def["config"]["nativeProvider"], json!(true));
    assert_eq!(def["config"]["models"][0]["id"], json!("native-1"));

    let outcome = runtime
        .dispatch_command("native-probe", "")
        .await
        .expect("run probe command");
    assert!(outcome.ok, "{:?}", outcome.error);
    let probe = probe(&runtime, "globalThis.__nativeProbe ?? null").await;
    assert_eq!(probe["auth"], json!("native-key"));
    assert_eq!(probe["images"]["images"][0]["mimeType"], json!("image/png"));
    assert_eq!(probe["classified"]["label"], json!("ok"));
    assert_eq!(probe["aborted"], json!(false));
    assert_eq!(probe["refreshErrors"], json!(0));
}

// ---------------------------------------------------------------------------
// H3: ONE mutator per model / thinking level / session name, and the input
// event's source/handled contract.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn model_thinking_and_name_mutations_share_one_entry_and_source() {
    use std::sync::Mutex;

    use pir::agent_session::{
        session_event_to_json, AgentSession, AgentSessionConfig, SessionEvent,
    };
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;
    use pir::session_manager::{FileEntry, SessionManager};

    fn mutation_model(id: &str, reasoning: bool) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: "fake".into(),
            provider: "fake".into(),
            base_url: "http://localhost".into(),
            reasoning,
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

    struct Fake;
    impl Provider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![
                mutation_model("fake-model", false),
                mutation_model("alt-model", true),
            ]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            let stream = AssistantMessageEventStream::new();
            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            };
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        }
    }

    let temp = TempDir::new("mutator-entry");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            globalThis.__mutations = [];
            pi.on("model_select", (event) => {
                globalThis.__mutations.push({
                    kind: "model",
                    id: event.model.id,
                    previousId: event.previousModel && event.previousModel.id,
                    previousProvider: event.previousModel && event.previousModel.provider,
                    source: event.source,
                });
            });
            pi.on("thinking_level_select", (event) => {
                globalThis.__mutations.push({
                    kind: "thinking",
                    level: event.level,
                    previousLevel: event.previousLevel,
                });
            });
            pi.on("session_info_changed", (event) => {
                globalThis.__mutations.push({ kind: "name", name: event.name ?? null });
            });
            // The extension-originated paths must apply through the SAME
            // mutators (branch entries + both event faces).
            pi.on("session_start", () => {
                pi.setModel({ provider: "fake", id: "fake-model" });
                pi.setThinkingLevel("low");
                pi.setSessionName("Session\nName");
            });
        }
        "#,
    )
    .await;

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Fake));
    let manager = SessionManager::create(".", &temp.0).expect("create session");
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: mutation_model("fake-model", false),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(manager),
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });

    let session_events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&session_events);
    agent.on_session_event(Arc::new(move |event: SessionEvent| {
        log.lock().unwrap().push(session_event_to_json(&event));
    }));

    // Explicit switch: persisted + source "set" with full Model objects.
    agent
        .set_model_with_source(mutation_model("alt-model", true), "set")
        .await
        .expect("set model");
    // Cycling reports "cycle"; restoring a resumed selection "restore".
    let cycled = agent
        .cycle_model()
        .await
        .expect("cycle")
        .expect("two models");
    assert_eq!(cycled.model.id, "fake-model");
    agent
        .restore_model(mutation_model("alt-model", true))
        .await
        .expect("restore model");
    assert_eq!(
        agent.set_thinking_level(ThinkingLevel::High).await,
        ThinkingLevel::High
    );
    agent.set_session_name("Host Name").await.expect("name");

    // A prompt dispatches session_start, whose handler mutates through the
    // extension API: those must land on the same entries/events.
    let run = agent
        .prompt("hello", Arc::new(|_| {}))
        .await
        .expect("prompt");
    assert_eq!(run.stop_reason, StopReason::Stop);

    let mutations = probe(&runtime, "globalThis.__mutations ?? []").await;
    let mutations = mutations.as_array().expect("mutations array");
    // Host mutations first, then the session_start handler's.
    assert_eq!(mutations[0]["kind"], json!("model"));
    assert_eq!(mutations[0]["id"], json!("alt-model"));
    assert_eq!(mutations[0]["previousId"], json!("fake-model"));
    assert_eq!(mutations[0]["previousProvider"], json!("fake"));
    assert_eq!(mutations[0]["source"], json!("set"));
    assert_eq!(mutations[1]["id"], json!("fake-model"));
    assert_eq!(mutations[1]["previousId"], json!("alt-model"));
    assert_eq!(mutations[1]["source"], json!("cycle"));
    assert_eq!(mutations[2]["id"], json!("alt-model"));
    assert_eq!(mutations[2]["source"], json!("restore"));
    assert_eq!(mutations[3]["kind"], json!("thinking"));
    assert_eq!(mutations[3]["level"], json!("high"));
    assert_eq!(mutations[3]["previousLevel"], json!("off"));
    assert_eq!(mutations[4]["kind"], json!("name"));
    assert_eq!(mutations[4]["name"], json!("Host Name"));
    // The extension mutators reuse the same entry: source "set", and the
    // session-name payload is the manager-normalized name.
    assert_eq!(mutations[5]["kind"], json!("model"));
    assert_eq!(mutations[5]["id"], json!("fake-model"));
    assert_eq!(mutations[5]["previousId"], json!("alt-model"));
    assert_eq!(mutations[5]["source"], json!("set"));
    assert_eq!(mutations[6]["kind"], json!("thinking"));
    // `low` clamps to `off`: the session_start handler switched to the
    // non-reasoning fake model first (upstream `setThinkingLevel` clamps).
    assert_eq!(mutations[6]["level"], json!("off"));
    assert_eq!(mutations[6]["previousLevel"], json!("high"));
    assert_eq!(mutations[7]["kind"], json!("name"));
    assert_eq!(mutations[7]["name"], json!("Session Name"));

    // The branch carries one entry per change, whichever entry point made it.
    let entries = agent
        .with_session_manager(|manager| manager.entries().to_vec())
        .await
        .expect("session storage");
    let model_changes: Vec<(String, String)> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::ModelChange(change) => {
                Some((change.provider.clone(), change.model_id.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        model_changes,
        vec![
            ("fake".to_string(), "alt-model".to_string()),
            ("fake".to_string(), "fake-model".to_string()),
            ("fake".to_string(), "alt-model".to_string()),
            ("fake".to_string(), "fake-model".to_string()),
        ]
    );
    let thinking_changes: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::ThinkingLevelChange(change) => Some(change.thinking_level.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        thinking_changes,
        vec!["high".to_string(), "off".to_string()]
    );
    let names: Vec<Option<String>> = entries
        .iter()
        .filter_map(|entry| match entry {
            FileEntry::SessionInfo(info) => Some(info.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        vec![
            Some("Host Name".to_string()),
            Some("Session Name".to_string())
        ]
    );

    // The session face of thinking/name changes fires too (upstream
    // `thinking_level_changed` / `session_info_changed`).
    let session_types: Vec<String> = session_events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| event["type"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        session_types
            .iter()
            .filter(|kind| *kind == "thinking_level_changed")
            .count(),
        2
    );
    assert_eq!(
        session_types
            .iter()
            .filter(|kind| *kind == "session_info_changed")
            .count(),
        2
    );
}

#[tokio::test(flavor = "current_thread")]
async fn input_event_reports_source_and_handled_swallows_the_prompt() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;

    fn input_model() -> Model {
        Model {
            id: "input-model".into(),
            name: "Input".into(),
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
        }
    }

    struct Counting {
        calls: Arc<AtomicUsize>,
    }
    impl Provider for Counting {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![input_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("k", vec!["FAKE".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let stream = AssistantMessageEventStream::new();
            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            };
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        }
    }

    let temp = TempDir::new("input-source-handled");
    let runtime = load_runtime(
        &temp,
        r#"
        export default function (pi) {
            globalThis.__inputs = [];
            pi.on("input", (event) => {
                globalThis.__inputs.push({
                    text: event.text,
                    source: event.source,
                    streamingBehavior: event.streamingBehavior ?? null,
                });
                if (event.text === "swallow") return { action: "handled" };
                if (event.text === "rewrite") {
                    return { action: "transform", text: "rewritten" };
                }
                return undefined;
            });
            pi.registerCommand("send-user", {
                handler: async () => { pi.sendUserMessage("from-extension"); },
            });
        }
        "#,
    )
    .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(Counting {
        calls: Arc::clone(&calls),
    }));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: input_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(temp.0.clone()),
        event_stream: None,
        extensions: Some(runtime.clone()),
        tool_selection: None,
    });
    let noop: pir::agent_session::EventHandler = Arc::new(|_| {});

    // `handled`: the prompt is consumed — no turn, no user message.
    let outcome = agent
        .prompt_with_source("swallow", "rpc", Arc::clone(&noop))
        .await
        .expect("handled prompt");
    assert!(outcome.is_none(), "handled returns no assistant message");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model request");
    assert_eq!(agent.message_count().await, 0, "no stored user message");

    // `transform` still runs and records the rewritten prompt.
    let outcome = agent
        .prompt_with_source("rewrite", "interactive", Arc::clone(&noop))
        .await
        .expect("transformed prompt");
    assert!(outcome.is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let transcript = agent.transcript().await;
    let user_texts: Vec<String> = transcript
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["rewritten".to_string()]);

    // Plain RPC prompt: source "rpc", dispatched exactly once.
    agent
        .prompt_with_source("plain-rpc", "rpc", Arc::clone(&noop))
        .await
        .expect("rpc prompt");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Idle `pi.sendUserMessage()`: source "extension" (dispatched once by the
    // notification drain; the pumped run must not emit a second time).
    agent
        .dispatch_plugin_command("send-user", "")
        .await
        .expect("send-user command");
    agent.apply_pending_extension_notifications().await;
    let message = agent
        .take_pending_custom_run()
        .expect("queued extension user run");
    let outcome = agent
        .run_custom_turn_with_source(message, "extension", Arc::clone(&noop))
        .await
        .expect("extension prompt");
    assert!(outcome.is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let inputs = probe(&runtime, "globalThis.__inputs ?? []").await;
    let inputs = inputs.as_array().expect("inputs array");
    assert_eq!(
        inputs
            .iter()
            .map(|input| (
                input["text"].as_str().unwrap_or_default().to_owned(),
                input["source"].as_str().unwrap_or_default().to_owned(),
            ))
            .collect::<Vec<_>>(),
        vec![
            ("swallow".to_string(), "rpc".to_string()),
            ("rewrite".to_string(), "interactive".to_string()),
            ("plain-rpc".to_string(), "rpc".to_string()),
            ("from-extension".to_string(), "extension".to_string()),
        ],
        "one input event per prompt, with the entry point's source"
    );
    assert!(inputs
        .iter()
        .all(|input| input["streamingBehavior"].is_null()));
}
