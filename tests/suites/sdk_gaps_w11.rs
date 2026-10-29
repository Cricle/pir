//! W11-D SDK residual-closure tests.
//!
//! Covers the PLAN §0e/§6b.4 SDK rows:
//!
//! - `streamProxy` parity: the `pir::sdk::stream_proxy` export and
//!   [`ProxyProvider`], exercised against a scripted in-process HTTP
//!   transport (no sockets, no network) on the upstream proxy protocol:
//!   tool-call metadata arriving only on `toolcall_end`, a terminal event
//!   that is not newline-terminated, a clean EOF without a terminal event,
//!   HTTP error wording, the injected-client seam (`with_client`), and the
//!   per-call retry overrides;
//! - `AgentBuilder` settings defaults (`defaultProvider`/`defaultModel`/
//!   `defaultTools`): settings apply when the caller sets nothing, explicit
//!   builder choices win, scoped `enabledModels` outranks the saved defaults,
//!   `.no_tools()` suppresses the settings tool set, and modifier-only
//!   `defaultTools` lists resolve against the built-in default names;
//! - the SDK-owned `on_init` call path: the build runs every plugin's
//!   `on_init` exactly once, before the first dispatched event (the test
//!   plugin declares a non-`project_trust` interest so no lazy path can
//!   satisfy the assertion);
//! - paged transcript/usage accessors over a synthetic large session file:
//!   `is_paged_session` / `paged_window` / `transcript_cursor` /
//!   `older_transcript_messages` / `older_transcript_json` / `session_stats` /
//!   `usage_totals` cover the full skipped prefix without widening the
//!   session, and `transcript_json` stays the resident window;
//! - `SdkEvent` payload shapes: `args` on tool start/update, `result` on tool
//!   end, and the terminal `AgentOutcome` event (including its upstream
//!   `agent_end.messages` slice).
//!
//! The settings-default tests redirect `PIR_AGENT_DIR` at a scratch directory
//! through `tests/common/mod.rs::AgentDirEnvGuard`, which holds the ONE
//! binary-wide agent-dir env lock (a module-local lock cannot exclude the other
//! suites of the merged `g_agent` binary).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::BoxFuture;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::error::AiResult;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::transport::{
    HttpBodyChunk, HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient,
    ProviderRequestConfig,
};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, ToolCall, Usage,
};
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginInfo,
};
use pir::sdk::{
    AgentBuilder, ProxyEndpoint, ProxyProvider, ProxyStreamOptions, SdkEvent, SessionMode,
    PROXY_CONNECTION_CLOSED_MESSAGE,
};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-gaps-w11-{label}-{}-{}",
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

fn model(provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: "Test model".into(),
        api: "openai-responses".into(),
        provider: provider.into(),
        base_url: "https://api.example.com/v1".into(),
        reasoning: true,
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

fn user_message(text: impl Into<String>) -> Message {
    Message::User(pir::ai::types::UserMessage {
        role: pir::ai::types::UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

fn assistant_message(text: &str, tokens: u64) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 0,
        usage: Usage {
            input: tokens / 2,
            output: tokens - tokens / 2,
            total_tokens: tokens,
            ..Default::default()
        },
        ..Default::default()
    })
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

/// The text of every `role: "user"` message in an `AgentMessage` JSON list
/// (the wire shape `transcript_json` returns).
fn json_user_texts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| {
            let content = message.get("content")?.as_array()?;
            let text: String = content
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            Some(text)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Scripted HTTP transport (the transport's public test seam; no sockets)
// ---------------------------------------------------------------------------

struct ScriptedTransport {
    status: u16,
    body: String,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl ScriptedTransport {
    fn new(status: u16, body: impl Into<String>) -> (Self, Arc<Mutex<Vec<HttpRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                status,
                body: body.into(),
                requests: Arc::clone(&requests),
            },
            requests,
        )
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        let status = self.status;
        let body = self.body.clone();
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: HeaderMap::new(),
                body: Box::pin(futures_util::stream::iter(vec![Ok::<
                    HttpBodyChunk,
                    pir::ai::error::AiError,
                >(
                    HttpBodyChunk::new(body.into_bytes()),
                )])),
            })
        })
    }
}

fn scripted_client(transport: ScriptedTransport) -> ProviderRequestClient {
    // No retry sleeps: every scripted case must resolve immediately.
    scripted_client_with_retries(transport, 0)
}

/// The same scripted client with a non-zero retry budget: only the per-call
/// `StreamOptions` overrides may lower it back to zero.
fn scripted_client_with_retries(
    transport: ScriptedTransport,
    max_retries: u32,
) -> ProviderRequestClient {
    ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(5),
            max_response_bytes: 1 << 20,
            max_retries,
            max_retry_delay: Duration::ZERO,
            proxy_url: None,
            http_idle_timeout: Some(Duration::from_secs(5)),
        },
        Arc::new(transport),
    )
}

/// Render proxy events as the SSE body the server sends.
fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn proxy_usage() -> Value {
    json!({
        "input": 11,
        "output": 7,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 18,
        "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
    })
}

fn endpoint() -> ProxyEndpoint {
    ProxyEndpoint::new("https://proxy.example.com", "test-token")
}

async fn drain_stream(
    stream: AssistantMessageEventStream,
) -> (
    Vec<pir::ai::types::AssistantMessageEvent>,
    Arc<AssistantMessage>,
) {
    let mut stream = Box::pin(stream);
    let mut events = Vec::new();
    while let Some(event) = stream.as_mut().next().await {
        events.push(event);
    }
    let result = stream.result().await.expect("terminal result");
    (events, result)
}

fn event_labels(events: &[pir::ai::types::AssistantMessageEvent]) -> Vec<&'static str> {
    use pir::ai::types::AssistantMessageEvent as Event;
    events
        .iter()
        .map(|event| match event {
            Event::Start { .. } => "start",
            Event::TextStart { .. } => "text_start",
            Event::TextDelta { .. } => "text_delta",
            Event::TextEnd { .. } => "text_end",
            Event::ThinkingStart { .. } => "thinking_start",
            Event::ThinkingDelta { .. } => "thinking_delta",
            Event::ThinkingEnd { .. } => "thinking_end",
            Event::ToolCallStart { .. } => "toolcall_start",
            Event::ToolCallDelta { .. } => "toolcall_delta",
            Event::ToolCallEnd { .. } => "toolcall_end",
            Event::Done { .. } => "done",
            Event::Error { .. } => "error",
        })
        .collect()
}

// ---------------------------------------------------------------------------
// (1) streamProxy export
// ---------------------------------------------------------------------------

/// `stream_proxy` is part of the public SDK surface with the upstream
/// three-argument shape. Referencing the function pointer proves the export
/// without issuing a request.
#[test]
fn stream_proxy_is_exported() {
    let _: fn(&Model, Context, ProxyStreamOptions) -> AssistantMessageEventStream =
        pir::sdk::stream_proxy;
    let options = ProxyStreamOptions::new("https://proxy.example.com", "token");
    assert_eq!(options.proxy_url, "https://proxy.example.com");
    assert_eq!(options.auth_token, "token");
    assert_eq!(
        ProxyEndpoint::new("https://proxy.example.com/", "t").stream_url(),
        "https://proxy.example.com/api/stream"
    );
}

#[tokio::test]
async fn proxy_provider_reconstructs_tool_calls_and_terminal_metadata() {
    // Upstream proxy.test.ts: tool-call metadata that only arrives on
    // `toolcall_end` must survive, and a terminal event without a trailing
    // newline must still be processed.
    let start = json!({ "type": "start" });
    let tool_start = json!({
        "type": "toolcall_start",
        "contentIndex": 0,
        "id": "call_test|fc_test",
        "toolName": "lookup"
    });
    let tool_delta = json!({
        "type": "toolcall_delta",
        "contentIndex": 0,
        "delta": "{\"value\":\"hello\"}"
    });
    let tool_end = json!({
        "type": "toolcall_end",
        "contentIndex": 0,
        "toolCall": {
            "type": "toolCall",
            "id": "call_test|fc_test",
            "name": "lookup",
            "arguments": { "value": "hello" },
            "namespace": "dynamic_tools"
        }
    });
    let done = json!({
        "type": "done",
        "reason": "toolUse",
        "usage": proxy_usage(),
        "providerThinkingLevel": "high"
    });
    let mut body = sse_body(&[start, tool_start, tool_delta, tool_end]);
    // The terminal event is deliberately NOT newline-terminated (upstream
    // regression #8996).
    body.push_str(&format!("data: {done}"));

    let (transport, requests) = ScriptedTransport::new(200, body);
    let provider = ProxyProvider::new(endpoint(), vec![model("proxy", "gpt-test")])
        .expect("provider")
        .with_client(scripted_client(transport));

    let mut options = StreamOptions {
        session_id: Some("session-1".to_owned()),
        temperature: Some(0.5),
        max_tokens: Some(64),
        thinking_budgets: None,
        ..Default::default()
    };
    options.metadata = Some(BTreeMap::from([("trace".to_owned(), json!("abc"))]));
    let (events, result) =
        drain_stream(provider.stream(&model("proxy", "gpt-test"), Context::default(), options))
            .await;

    assert_eq!(
        event_labels(&events),
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ],
        "the trailing terminal event must be flushed"
    );

    let Some(pir::ai::types::AssistantMessageEvent::ToolCallEnd { tool_call, .. }) =
        events.iter().find(|event| {
            matches!(
                event,
                pir::ai::types::AssistantMessageEvent::ToolCallEnd { .. }
            )
        })
    else {
        panic!("missing toolcall_end");
    };
    assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));
    assert_eq!(tool_call.arguments.get("value"), Some(&json!("hello")));

    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert_eq!(result.provider_thinking_level.as_deref(), Some("high"));
    assert_eq!(result.usage.total_tokens, 18);
    let Some(ContentBlock::ToolCall(call)) = result.content.first() else {
        panic!("terminal message lost the tool call: {:?}", result.content);
    };
    assert_eq!(call.namespace.as_deref(), Some("dynamic_tools"));
    assert_eq!(call.arguments.get("value"), Some(&json!("hello")));

    // Request shape: proxy URL, bearer token, model/context/options body.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, reqwest::Method::POST);
    assert_eq!(request.url, "https://proxy.example.com/api/stream");
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer test-token")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("request JSON");
    assert_eq!(body["model"]["id"], "gpt-test");
    assert_eq!(body["model"]["provider"], "proxy");
    assert!(body["context"]["messages"]
        .as_array()
        .is_some_and(Vec::is_empty));
    assert_eq!(body["options"]["temperature"], 0.5);
    assert_eq!(body["options"]["maxTokens"], 64);
    assert_eq!(body["options"]["sessionId"], "session-1");
    assert_eq!(body["options"]["metadata"]["trace"], "abc");
    // Local-only knobs never reach the server.
    assert!(body["options"].get("apiKey").is_none());
    assert!(body["options"].get("signal").is_none());
}

#[tokio::test]
async fn proxy_provider_clean_eof_without_terminal_event_is_an_error() {
    // Upstream: a clean EOF without done/error must not leave the consumer
    // waiting for a result that never arrives.
    let body = sse_body(&[json!({ "type": "start" })]);
    let (transport, _requests) = ScriptedTransport::new(200, body);
    let provider = ProxyProvider::new(endpoint(), vec![model("proxy", "gpt-test")])
        .expect("provider")
        .with_client(scripted_client(transport));

    let (events, result) = drain_stream(provider.stream(
        &model("proxy", "gpt-test"),
        Context::default(),
        StreamOptions::default(),
    ))
    .await;

    assert_eq!(event_labels(&events), vec!["start", "error"]);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some(PROXY_CONNECTION_CLOSED_MESSAGE)
    );
}

#[tokio::test]
async fn proxy_provider_http_error_uses_proxy_wording() {
    let (transport, _requests) = ScriptedTransport::new(502, r#"{"error":"upstream down"}"#);
    let provider = ProxyProvider::new(endpoint(), vec![model("proxy", "gpt-test")])
        .expect("provider")
        .with_client(scripted_client(transport));

    let (events, result) = drain_stream(provider.stream(
        &model("proxy", "gpt-test"),
        Context::default(),
        StreamOptions::default(),
    ))
    .await;

    assert_eq!(event_labels(&events), vec!["error"]);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Proxy error: upstream down")
    );
}

#[tokio::test]
async fn proxy_provider_streams_text_through_the_builder() {
    // End-to-end on the SDK path: AgentBuilder::proxy registers the provider,
    // the agent loop drives it, and the terminal text arrives intact.
    let body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "hello " }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "from proxy" }),
        json!({ "type": "text_end", "contentIndex": 0 }),
        json!({ "type": "done", "reason": "stop", "usage": proxy_usage() }),
    ]);
    let (transport, requests) = ScriptedTransport::new(200, body);
    let provider = ProxyProvider::new(endpoint(), vec![model("proxy", "gpt-test")])
        .expect("provider")
        .with_client(scripted_client(transport));
    let agent = AgentBuilder::new()
        .proxy(provider, model("proxy", "gpt-test"))
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build proxy agent");

    let outcome = agent.prompt("hi").await.expect("prompt");
    assert_eq!(outcome.message_text().as_deref(), Some("hello from proxy"));
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.usage.total_tokens, 18);

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://proxy.example.com/api/stream");
}

/// The standalone `stream_proxy` entry point must accept the same injected
/// transport the provider does (`ProxyStreamOptions::with_client`): without the
/// seam the only client is the process-wide default, which would issue a real
/// network request here.
#[tokio::test]
async fn stream_proxy_uses_the_injected_client() {
    let body = sse_body(&[
        json!({ "type": "start" }),
        json!({ "type": "text_start", "contentIndex": 0 }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "direct " }),
        json!({ "type": "text_delta", "contentIndex": 0, "delta": "entry" }),
        json!({ "type": "text_end", "contentIndex": 0 }),
        json!({ "type": "done", "reason": "stop", "usage": proxy_usage() }),
    ]);
    let (transport, requests) = ScriptedTransport::new(200, body);
    let options = ProxyStreamOptions::new("https://proxy.example.com", "test-token")
        .with_stream_options(StreamOptions {
            temperature: Some(0.25),
            session_id: Some("direct-session".to_owned()),
            ..Default::default()
        })
        .with_client(scripted_client(transport));

    let (events, result) = drain_stream(pir::sdk::stream_proxy(
        &model("proxy", "gpt-test"),
        Context::default(),
        options,
    ))
    .await;

    assert_eq!(event_labels(&events).last(), Some(&"done"));
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.usage.total_tokens, 18);
    let text: String = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "direct entry");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://proxy.example.com/api/stream");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-token")
    );
    let request: Value = serde_json::from_slice(&requests[0].body).expect("request JSON");
    assert_eq!(request["options"]["temperature"], 0.25);
    assert_eq!(request["options"]["sessionId"], "direct-session");
}

/// `stream_proxy` must honour the per-call retry overrides every adapter
/// applies (`StreamOptions::max_retries` / `max_retry_delay_ms`) on the client
/// it uses. With a client configured for two retries, `max_retries: Some(0)`
/// must still make exactly ONE request; without the override the retryable 502
/// is attempted three times before the terminal error.
#[tokio::test]
async fn stream_proxy_honours_per_call_retry_overrides() {
    let (transport, requests) = ScriptedTransport::new(502, r#"{"error":"upstream down"}"#);
    let options = ProxyStreamOptions::new("https://proxy.example.com", "test-token")
        .with_stream_options(StreamOptions {
            max_retries: Some(0),
            max_retry_delay_ms: Some(0),
            ..Default::default()
        })
        .with_client(scripted_client_with_retries(transport, 2));

    let (events, result) = drain_stream(pir::sdk::stream_proxy(
        &model("proxy", "gpt-test"),
        Context::default(),
        options,
    ))
    .await;

    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "the per-call retry budget must reach the injected client"
    );
    assert_eq!(event_labels(&events), vec!["error"]);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Proxy error: upstream down")
    );
}

#[test]
fn proxy_provider_rejects_mixed_providers() {
    let mixed = vec![model("a", "one"), model("b", "two")];
    // `Result::expect_err` needs `Debug` on the success type, which the
    // provider deliberately does not expose (it owns an HTTP client).
    let error = match ProxyProvider::new(endpoint(), mixed) {
        Ok(_) => panic!("mixed providers must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("share one provider id"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// (2) AgentBuilder settings defaults
// ---------------------------------------------------------------------------

/// A models.json with one authenticated provider, plus settings.json naming the
/// saved default model and a `defaultTools` list.
fn write_default_settings(agent_dir: &Path) {
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "w11d": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "w11d-test-key",
                    "models": [
                        { "id": "w11d-saved" },
                        { "id": "w11d-other" }
                    ]
                }
            }
        }))
        .expect("serialize models.json"),
    )
    .expect("write models.json");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "defaultProvider": "w11d",
            "defaultModel": "w11d-saved",
            "defaultTools": ["beta"]
        }))
        .expect("serialize settings.json"),
    )
    .expect("write settings.json");
}

/// The same fixture with a caller-supplied `settings.json` overlay (the model
/// catalog stays identical), for the precedence cases.
fn write_settings_overlay(agent_dir: &Path, settings: Value) {
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&json!({
            "providers": {
                "w11d": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "w11d-test-key",
                    "models": [
                        { "id": "w11d-saved" },
                        { "id": "w11d-other" }
                    ]
                }
            }
        }))
        .expect("serialize models.json"),
    )
    .expect("write models.json");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("serialize settings.json"),
    )
    .expect("write settings.json");
}

#[tokio::test(flavor = "current_thread")]
async fn builder_settings_defaults_apply_and_explicit_choices_win() {
    let agent_dir = TempDir::new("agent-defaults");
    let cwd = TempDir::new("cwd-defaults");
    write_default_settings(&agent_dir.0);
    crate::common::ensure_offline_mode();

    let with_tools = |builder: AgentBuilder| {
        builder
            .agent_dir(&agent_dir.0)
            .no_session()
            .no_compaction()
            .cwd(cwd.0.clone())
            .thinking_level(ThinkingLevel::Off)
            .tool(
                "alpha",
                "alpha tool",
                json!({ "type": "object" }),
                |_args| async { ToolResult::ok("alpha") },
            )
            .tool(
                "beta",
                "beta tool",
                json!({ "type": "object" }),
                |_args| async { ToolResult::ok("beta") },
            )
    };

    // No explicit model/tools: defaultProvider/defaultModel select the model
    // and settings defaultTools selects the declared tool set.
    let defaulted = with_tools(AgentBuilder::new())
        .build()
        .await
        .expect("build with settings defaults");
    assert_eq!(defaulted.model().provider, "w11d");
    assert_eq!(defaulted.model().id, "w11d-saved");
    let names: Vec<String> = defaulted
        .tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        names,
        vec!["beta".to_owned()],
        "settings defaultTools must be the active set when the caller did not call .tools()"
    );

    // Explicit model + explicit tool list beat both settings defaults.
    let explicit = with_tools(
        AgentBuilder::new()
            .model_in_provider("w11d", "w11d-other")
            .tools(["alpha"]),
    )
    .build()
    .await
    .expect("build with explicit choices");
    assert_eq!(explicit.model().provider, "w11d");
    assert_eq!(explicit.model().id, "w11d-other");
    let names: Vec<String> = explicit.tools().into_iter().map(|tool| tool.name).collect();
    assert_eq!(names, vec!["alpha".to_owned()]);

    // An explicit denylist applies on top of the settings defaultTools set.
    let excluded = with_tools(AgentBuilder::new().exclude_tools(["beta"]))
        .build()
        .await
        .expect("build with a settings-derived denylist");
    assert_eq!(
        excluded
            .tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        Vec::<String>::new(),
        "the settings default set minus the excluded tool must be empty"
    );

    // A registry build is documented as environment-free: the settings
    // default provider is never consulted, and the explicit model wins.
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    let hermetic = AgentBuilder::new()
        .with_registry(Arc::new(registry), model("fake", "fake-model"))
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("hermetic registry build");
    assert_eq!(hermetic.model().provider, "fake");
    assert_eq!(hermetic.model().id, "fake-model");
}

/// Settings precedence beyond the plain defaults: a scoped `enabledModels`
/// list outranks `defaultProvider`/`defaultModel`, `.no_tools()` suppresses
/// the settings `defaultTools` set entirely, and a modifier-only
/// `defaultTools` list (`+name`/`-name`) is resolved against the built-in
/// default names before it becomes the active set.
#[tokio::test(flavor = "current_thread")]
async fn builder_settings_precedence_scope_no_tools_and_modifier_defaults() {
    let cwd = TempDir::new("cwd-precedence");
    crate::common::ensure_offline_mode();
    let with_tools = |builder: AgentBuilder| {
        builder
            .no_session()
            .no_compaction()
            .cwd(cwd.0.clone())
            .thinking_level(ThinkingLevel::Off)
            .tool(
                "alpha",
                "alpha tool",
                json!({ "type": "object" }),
                |_args| async { ToolResult::ok("alpha") },
            )
            .tool(
                "beta",
                "beta tool",
                json!({ "type": "object" }),
                |_args| async { ToolResult::ok("beta") },
            )
    };

    // 1. `enabledModels` (the CLI's scoped list) beats the saved defaults.
    {
        let agent_dir = TempDir::new("agent-scoped");
        write_settings_overlay(
            &agent_dir.0,
            json!({
                "defaultProvider": "w11d",
                "defaultModel": "w11d-saved",
                "enabledModels": ["w11d/w11d-other"]
            }),
        );
        let scoped = with_tools(AgentBuilder::new().agent_dir(&agent_dir.0))
            .build()
            .await
            .expect("build with a scoped model list");
        assert_eq!(
            (scoped.model().provider.as_str(), scoped.model().id.as_str()),
            ("w11d", "w11d-other"),
            "the scoped enabledModels list must outrank defaultProvider/defaultModel"
        );
    }

    // 2. `.no_tools()` suppresses the settings `defaultTools` set: the same
    //    settings select `beta` without the switch, and nothing with it.
    {
        let agent_dir = TempDir::new("agent-no-tools");
        write_settings_overlay(
            &agent_dir.0,
            json!({
                "defaultProvider": "w11d",
                "defaultModel": "w11d-saved",
                "defaultTools": ["beta"]
            }),
        );
        let defaulted = with_tools(AgentBuilder::new().agent_dir(&agent_dir.0))
            .build()
            .await
            .expect("build with the settings defaultTools set");
        assert_eq!(
            defaulted
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            vec!["beta".to_owned()],
            "the settings defaultTools set is the control for the no_tools case"
        );
        let none = with_tools(AgentBuilder::new().agent_dir(&agent_dir.0).no_tools())
            .build()
            .await
            .expect("build with no tools");
        assert!(
            none.tools().is_empty(),
            "no_tools must clear the settings defaultTools set, got {:?}",
            none.tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        );
    }

    // 3. A modifier-only `defaultTools` list applies to the built-in default
    //    names, then the active set is matched against the real registry.
    {
        let agent_dir = TempDir::new("agent-modifier-defaults");
        write_settings_overlay(
            &agent_dir.0,
            json!({
                "defaultProvider": "w11d",
                "defaultModel": "w11d-saved",
                "defaultTools": ["+alpha", "-bash"]
            }),
        );
        let modified = with_tools(AgentBuilder::new().agent_dir(&agent_dir.0))
            .build()
            .await
            .expect("build with modifier defaults");
        assert_eq!(
            modified
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>(),
            vec!["alpha".to_owned()],
            "`+alpha -bash` must resolve against DEFAULT_TOOL_NAMES, then keep only \
             the declared tools"
        );
    }
}

/// Minimal provider for the hermetic `with_registry` precedence check.
struct IdleProvider;

impl Provider for IdleProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![model("fake", "fake-model")]
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
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake-api".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        ));
        stream
    }
}

// ---------------------------------------------------------------------------
// (3) on_init call path
// ---------------------------------------------------------------------------

struct InitPlugin {
    info: PluginInfo,
    init_calls: Arc<AtomicUsize>,
    /// `true` when the plugin was dispatched an event.
    saw_event: Arc<AtomicUsize>,
    /// `init_calls` observed at the first dispatched event.
    init_seen_before_event: Arc<Mutex<Option<usize>>>,
}

impl InitPlugin {
    fn new() -> Self {
        Self {
            info: PluginInfo::inline("w11d-init"),
            init_calls: Arc::new(AtomicUsize::new(0)),
            saw_event: Arc::new(AtomicUsize::new(0)),
            init_seen_before_event: Arc::new(Mutex::new(None)),
        }
    }
}

impl AgentPlugin for InitPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        // NOT `All`: an interest list without `project_trust` keeps the
        // session's native `project_trust` fan-out from running the init hook
        // lazily in the background, so an `init_calls == 1` observation right
        // after `build()` can only come from the SDK build's own call site.
        // `PluginHost::emit` still hands every event to every native plugin
        // regardless of declaration, so `saw_event` keeps counting.
        EventInterest::List(vec!["input".to_owned()])
    }

    fn on_init(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        self.init_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        self.saw_event.fetch_add(1, Ordering::SeqCst);
        let mut seen = self
            .init_seen_before_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if seen.is_none() {
            *seen = Some(self.init_calls.load(Ordering::SeqCst));
        }
        Box::pin(async { PluginEventOutcome::None })
    }
}

#[tokio::test]
async fn on_init_runs_once_during_build_before_the_first_event() {
    let plugin = Arc::new(InitPlugin::new());
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(InitPluginProvider));
    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), model("init", "init-model"))
        .with_plugin(plugin.clone())
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");

    // The SDK build is the documented on_init call site: it runs after
    // registration/context binding and before any event dispatch.
    assert_eq!(
        plugin.init_calls.load(Ordering::SeqCst),
        1,
        "build() must run on_init exactly once"
    );

    agent.prompt("hello").await.expect("prompt");
    assert_eq!(
        plugin.init_calls.load(Ordering::SeqCst),
        1,
        "on_init must stay one-shot across runs"
    );
    assert!(
        plugin.saw_event.load(Ordering::SeqCst) > 0,
        "no event dispatched"
    );
    assert_eq!(
        *plugin
            .init_seen_before_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some(1),
        "the first dispatched event must observe a completed on_init"
    );
}

/// Provider for the on_init test (one text turn per request).
struct InitPluginProvider;

impl Provider for InitPluginProvider {
    fn id(&self) -> &str {
        "init"
    }
    fn name(&self) -> &str {
        "Init provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![model("init", "init-model")]
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
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "openai-responses".into(),
                provider: "init".into(),
                model: "init-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        ));
        stream
    }
}

// ---------------------------------------------------------------------------
// (4) Paged transcript/usage accessors
// ---------------------------------------------------------------------------

/// Turns in the synthetic large session. Two entries per turn, so the file
/// carries 2 800 entries: well past the 1 024-entry tail page, which leaves a
/// prefix large enough to need more than one "load older" page.
const LARGE_SESSION_TURNS: usize = 1_400;
const LARGE_SESSION_TOKENS: u64 = 5;

/// Build a persisted session whose entry count exceeds the tail page's entry
/// cap (1024), so `open_session_tail_first` really pages it.
fn build_large_session(dir: &Path) -> PathBuf {
    let mut manager = SessionManager::create("C:\\repo", dir).expect("create session");
    let path = manager.path().to_path_buf();
    for index in 0..LARGE_SESSION_TURNS {
        manager
            .append_message(user_message(format!("turn {index:04}")))
            .expect("append user");
        manager
            .append_message(assistant_message("answer", LARGE_SESSION_TOKENS))
            .expect("append assistant");
    }
    path
}

#[tokio::test]
async fn paged_accessors_cover_the_skipped_prefix() {
    let dir = TempDir::new("paged");
    let path = build_large_session(&dir.0);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), model("fake", "fake-model"))
        .session(SessionMode::Existing(path.clone()))
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .cwd(dir.0.clone())
        .build()
        .await
        .expect("build paged agent");

    assert!(
        agent.is_paged_session().await,
        "a {} entry session must open tail-first",
        LARGE_SESSION_TURNS * 2
    );
    let window = agent.paged_window().await.expect("paged window");
    assert!(window.file_bytes > window.page_start);
    assert!(window.parsed_entries > 0);

    // Walk the skipped prefix through the public cursor API, newest page
    // first, collecting every older user message in chronological order. The
    // first page uses the default-cap convenience accessor (`next_older_page`)
    // and the rest the explicit-cap entry, so both are exercised.
    let mut cursor = agent.transcript_cursor().await.expect("prefix cursor");
    let mut older_users: Vec<String> = Vec::new();
    let mut pages = 0usize;
    loop {
        let page = if pages == 0 {
            agent.next_older_page(cursor).await.expect("older page")
        } else {
            agent
                .older_transcript_messages(cursor, 64 * 1024, 8)
                .await
                .expect("older page")
        };
        let Some((messages, next)) = page else {
            break;
        };
        assert!(next < cursor, "the cursor must move strictly older");
        let mut texts = user_texts(&messages);
        texts.extend(older_users);
        older_users = texts;
        cursor = next;
        pages += 1;
        assert!(pages <= LARGE_SESSION_TURNS, "paging did not converge");
    }
    assert!(pages > 1, "the fixture must span several pages");

    // Resident transcript + streamed prefix == the full session, in order.
    let resident = agent.transcript().await;
    let mut all_users = older_users;
    all_users.extend(user_texts(&resident));
    let expected: Vec<String> = (0..LARGE_SESSION_TURNS)
        .map(|index| format!("turn {index:04}"))
        .collect();
    assert_eq!(
        all_users, expected,
        "paged pages plus the resident window must cover every message once"
    );

    // The JSON companion walks the same prefix: `transcript_json` alone only
    // reports the resident window, so a JSON consumer needs
    // `older_transcript_json` to reach the rest.
    let resident_json = agent.transcript_json().await;
    let resident_json_users = json_user_texts(&resident_json);
    assert!(
        resident_json_users.len() < LARGE_SESSION_TURNS,
        "transcript_json must stay the resident window ({} users)",
        resident_json_users.len()
    );
    let mut cursor = agent.transcript_cursor().await.expect("prefix cursor");
    let mut older_json_users: Vec<String> = Vec::new();
    let mut json_pages = 0usize;
    loop {
        let page = agent
            .older_transcript_json(cursor, 64 * 1024, 8)
            .await
            .expect("older JSON page");
        let Some((messages, next)) = page else {
            break;
        };
        assert!(next < cursor, "the JSON cursor must move strictly older");
        let mut texts = json_user_texts(&messages);
        texts.extend(older_json_users);
        older_json_users = texts;
        cursor = next;
        json_pages += 1;
        assert!(json_pages <= LARGE_SESSION_TURNS, "paging did not converge");
    }
    assert!(json_pages > 1, "the JSON fixture must span several pages");
    let mut all_json_users = older_json_users;
    all_json_users.extend(resident_json_users);
    assert_eq!(
        all_json_users, expected,
        "JSON pages plus transcript_json must cover every message once"
    );

    // Usage totals include the skipped prefix (turns and tokens), exactly the
    // values a full load reports.
    let totals = agent.usage_totals();
    assert_eq!(totals.turns, LARGE_SESSION_TURNS);
    assert_eq!(
        totals.total_tokens,
        LARGE_SESSION_TURNS as u64 * LARGE_SESSION_TOKENS
    );

    // Session stats stream the prefix too.
    let stats = agent.session_stats().await;
    assert_eq!(stats.total_messages, LARGE_SESSION_TURNS * 2);
    assert_eq!(stats.user_messages, LARGE_SESSION_TURNS);
    assert_eq!(stats.assistant_messages, LARGE_SESSION_TURNS);
    assert_eq!(stats.tokens.input, LARGE_SESSION_TURNS as u64 * 2);
    assert_eq!(stats.tokens.output, LARGE_SESSION_TURNS as u64 * 3);
    assert_eq!(
        stats.tokens.total,
        LARGE_SESSION_TURNS as u64 * LARGE_SESSION_TOKENS
    );

    // A fully resident agent reports no paged window and no cursor.
    let resident_agent = AgentBuilder::new()
        .with_registry(
            {
                let mut registry = ProviderRegistry::default();
                registry.set_provider(Arc::new(IdleProvider));
                Arc::new(registry)
            },
            model("fake", "fake-model"),
        )
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build in-memory agent");
    assert!(!resident_agent.is_paged_session().await);
    assert!(resident_agent.paged_window().await.is_none());
    assert!(resident_agent.transcript_cursor().await.is_none());
}

// ---------------------------------------------------------------------------
// (5) SdkEvent payload shapes
// ---------------------------------------------------------------------------

/// Two-turn provider: first a tool call, then a final answer.
struct ToolCallProvider {
    calls: AtomicUsize,
}

impl Provider for ToolCallProvider {
    fn id(&self) -> &str {
        "events"
    }
    fn name(&self) -> &str {
        "Event provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![model("events", "event-model")]
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = if index == 0 {
            AssistantMessage {
                api: "openai-responses".into(),
                provider: "events".into(),
                model: "event-model".into(),
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "echo".into(),
                    arguments: BTreeMap::from([("text".to_owned(), json!("hi"))]),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            }
        } else {
            AssistantMessage {
                api: "openai-responses".into(),
                provider: "events".into(),
                model: "event-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "done".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                ..Default::default()
            }
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            if index == 0 {
                TerminalReason::ToolUse
            } else {
                TerminalReason::Stop
            },
            message,
        ));
        stream
    }
}

#[tokio::test]
async fn sdk_event_payloads_carry_args_result_and_outcome() {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(ToolCallProvider {
        calls: AtomicUsize::new(0),
    }));
    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), model("events", "event-model"))
        .tool(
            "echo",
            "Echo its input",
            json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
            |args| async move {
                ToolResult::ok(
                    args.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                )
            },
        )
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");

    let events: Vec<SdkEvent> = agent
        .prompt_stream_with_outcome("go")
        .collect::<Vec<_>>()
        .await;

    let start = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::ToolExecutionStart {
                call_id,
                tool_name,
                args,
                parent_call_id,
            } => Some((call_id, tool_name, args, parent_call_id)),
            _ => None,
        })
        .expect("ToolExecutionStart");
    assert_eq!(start.0, "call-1");
    assert_eq!(start.1, "echo");
    assert_eq!(start.2, &json!({ "text": "hi" }));
    assert!(start.3.is_none());

    let end = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::ToolExecutionEnd {
                call_id,
                tool_name,
                is_error,
                duration_ms,
                result,
                ..
            } => Some((call_id, tool_name, is_error, duration_ms, result)),
            _ => None,
        })
        .expect("ToolExecutionEnd");
    assert_eq!(end.0, "call-1");
    assert_eq!(end.1, "echo");
    assert!(!end.2);
    assert!(end.3.is_some(), "a ran tool must report a duration");
    assert_eq!(end.4["content"][0]["type"], "text");
    assert_eq!(end.4["content"][0]["text"], "hi");

    let outcome = events
        .iter()
        .find_map(|event| match event {
            SdkEvent::AgentOutcome(outcome) => Some(outcome),
            _ => None,
        })
        .expect("AgentOutcome");
    assert_eq!(outcome.turns, 2);
    assert!(!outcome.aborted);
    assert_eq!(outcome.message_text().as_deref(), Some("done"));
    assert_eq!(outcome.tool_result_texts(), vec!["hi".to_owned()]);

    // Upstream `agent_end.messages`: the run's own transcript slice in append
    // order. `message` alone is the terminal assistant message, so without
    // this field the intermediate tool-calling assistant message would be
    // unreachable from the outcome. The builder declares one tool (a
    // `declareToolChanges` system message precedes the assistant messages), so
    // the slice is [system declaration, assistant(toolCall), toolResult,
    // assistant(final)].
    let roles: Vec<&'static str> = outcome
        .messages
        .iter()
        .map(pir::ai::types::Message::role)
        .collect();
    assert_eq!(
        roles,
        vec!["system", "assistant", "toolResult", "assistant"],
        "unexpected outcome message slice: {:?}",
        outcome.messages
    );
    let Message::Assistant(first) = &outcome.messages[1] else {
        panic!("second outcome message must be the tool-calling assistant message");
    };
    assert!(matches!(
        first.content.first(),
        Some(ContentBlock::ToolCall(call))
            if call.id == "call-1" && call.name == "echo"
    ));
    let Message::Assistant(last) = &outcome.messages[3] else {
        panic!("last outcome message must be the terminal assistant message");
    };
    assert_eq!(
        last.content,
        vec![ContentBlock::Text(TextContent {
            text: "done".into(),
            text_signature: None,
        })]
    );
    assert_eq!(
        outcome.messages.last(),
        Some(&Message::Assistant(outcome.message.clone())),
        "the terminal message is the slice's last element"
    );

    // The outcome arrives immediately before the terminal settled event.
    assert!(matches!(
        events.last(),
        Some(SdkEvent::AgentSettled { aborted: false })
    ));
    let outcome_index = events
        .iter()
        .position(|event| matches!(event, SdkEvent::AgentOutcome(_)))
        .expect("outcome index");
    assert_eq!(outcome_index + 1, events.len() - 1);
}
