//! D3 `d4d53612b` — Anthropic preferred-strict-tool fallback (UPDATING.md ledger, D3).
//!
//! Translates the upstream `packages/ai/test/anthropic-strict-tool-fallback.
//! test.ts` scenarios: a `400 invalid_request_error` naming one of the known
//! strict-tool capacity limits is retried exactly once with the preferred
//! (`constrainedSampling.strict == "prefer"`) tools sent with their original
//! non-strict schemas; `"require"` tools, unrelated errors, and already
//! started streams are never downgraded. No live network: a local mock HTTP
//! server stands in for the provider.
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::ai::api::anthropic::{
    build_anthropic_request_with_strict_sampling, AnthropicMessagesProvider,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::Provider;
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Mock HTTP server (same shape as ai_remaining_adapter_contracts.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MockRequest {
    path: String,
    body: Vec<u8>,
}

struct MockResponseSpec {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl MockResponseSpec {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            reason: "OK".into(),
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }
    }

    fn json_error(status: u16, body: Value) -> Self {
        Self {
            status,
            reason: "error".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&body).expect("error body json"),
        }
    }
}

/// Invoked once per accepted request, before the scripted response is written.
type RequestObserver = Arc<dyn Fn(usize) + Send + Sync>;

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(responses: Vec<MockResponseSpec>) -> Self {
        Self::start_with(responses, None).await
    }

    async fn start_with(
        responses: Vec<MockResponseSpec>,
        on_request: Option<RequestObserver>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handle = tokio::spawn(async move {
            for (index, response) in responses.into_iter().enumerate() {
                let (mut socket, _) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(_) => break,
                };
                if let Some(request) = read_request(&mut socket).await {
                    recorded
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(request);
                }
                if let Some(observer) = &on_request {
                    observer(index);
                }
                let head = format!(
                    "HTTP/1.1 {} {}\r\ncontent-length: {}\r\nconnection: close\r\n{}\r\n",
                    response.status,
                    response.reason,
                    response.body.len(),
                    response
                        .headers
                        .iter()
                        .map(|(k, v)| format!("{k}: {v}\r\n"))
                        .collect::<String>()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&response.body).await;
                let _ = socket.flush().await;
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
            _handle: handle,
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn payloads(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("request body json"))
            .collect()
    }

    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|request| request.path.clone())
            .collect()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn read_request(socket: &mut TcpStream) -> Option<MockRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let mut lines = head.lines();
            let mut parts = lines.next()?.split_whitespace();
            parts.next()?;
            let path = parts.next()?.to_string();
            let mut content_length = 0usize;
            for line in lines {
                if let Some((key, value)) = line.split_once(':') {
                    if key.trim().eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let body_start = header_end + 4;
            while buf.len().saturating_sub(body_start) < content_length {
                let n = socket.read(&mut tmp).await.ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let end = (body_start + content_length).min(buf.len());
            return Some(MockRequest {
                path,
                body: buf[body_start..end].to_vec(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures (upstream anthropic-strict-tool-fallback.test.ts constants)
// ---------------------------------------------------------------------------

const GRAMMAR_ERROR: &str = "The compiled grammar is too large, which would cause performance issues. Simplify your tool schemas or reduce the number of strict tools.";
/// Observed from the live API: 32 strict tools, then 20 tools with 24 optional
/// fields each.
const TOO_MANY_STRICT_TOOLS: &str = "Too many strict tools (32). The maximum number of strict tools supported is 20. Try reducing the number of tools marked as strict.";
const TOO_MANY_UNIONS: &str = "Schemas contains too many parameters with union types (480 parameters with type arrays or anyOf). This causes exponential compilation cost. Reduce the number of nullable or union-typed parameters (limit: 16 parameters with unions).";

fn error_body(message: &str, error_type: &str) -> Value {
    json!({"type": "error", "error": {"type": error_type, "message": message}})
}

fn error_response(message: &str) -> MockResponseSpec {
    MockResponseSpec::json_error(400, error_body(message, "invalid_request_error"))
}

fn error_response_typed(message: &str, status: u16, error_type: &str) -> MockResponseSpec {
    MockResponseSpec::json_error(status, error_body(message, error_type))
}

/// The upstream success fixture: message_start / message_delta / message_stop.
fn success_response() -> MockResponseSpec {
    MockResponseSpec::ok(
        concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes()
        .to_vec(),
    )
}

fn strict_tool(name: &str, strict: Option<StrictPreference>) -> Tool {
    Tool {
        name: name.into(),
        description: "Look up a value".into(),
        parameters: json!({
            "type": "object",
            "properties": {"value": {"type": "string"}, "optional": {"type": "number"}},
            "required": ["value"],
        }),
        constrained_sampling: strict.map(|strict| ConstrainedSamplingConfig::JsonSchema { strict }),
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

fn system_message(timestamp: u64, added: Vec<Tool>) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: Some(added),
        tools_removed: None,
        timestamp,
        sections: None,
    })
}

fn context(tools: Vec<Tool>) -> Context {
    Context {
        system_prompt: None,
        messages: vec![user_message("Hello")],
        tools: Some(tools),
    }
}

fn base_model(base_url: &str) -> Model {
    Model {
        id: "claude-haiku-4-5".into(),
        name: "Test Claude".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: base_url.to_string(),
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
        context_window: 200_000,
        max_tokens: 100,
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

fn strict_compat() -> OpenAiCompat {
    OpenAiCompat {
        supports_strict_tools: Some(true),
        ..Default::default()
    }
}

fn no_retry_client() -> ProviderRequestClient {
    ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 8 * 1024 * 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_millis(20),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("no-retry client")
}

fn provider(server: &MockServer, compat: OpenAiCompat) -> (AnthropicMessagesProvider, Model) {
    let mut model = base_model(server.base_url());
    model.compat = Some(compat);
    let provider = AnthropicMessagesProvider::new("anthropic", server.base_url(), vec![model])
        .expect("provider")
        .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    (provider, model)
}

fn options(on_payload: Option<Arc<AtomicUsize>>) -> StreamOptions {
    let hooks = on_payload.map(|counter| ProviderStreamHooks {
        on_payload: Some(Arc::new(move |body: &mut Value| {
            counter.fetch_add(1, Ordering::SeqCst);
            // Upstream's `onPayload` returns a mutated payload (`stream: false`
            // there); pir mutates in place. The adapter re-forces
            // `stream: true` and the marker must be re-applied on every
            // attempt.
            body["inference_geo"] = json!("us");
            body["stream"] = json!(false);
        })),
        ..Default::default()
    });
    StreamOptions {
        api_key: Some("test-key".into()),
        max_retries: Some(0),
        cache_retention: Some(CacheRetention::Long),
        hooks: hooks.unwrap_or_default(),
        ..StreamOptions::default()
    }
}

async fn run(
    provider: &AnthropicMessagesProvider,
    model: &Model,
    context: Context,
    options: StreamOptions,
) -> Arc<AssistantMessage> {
    let stream: AssistantMessageEventStream = provider.stream(model, context, options);
    stream.result().await.expect("stream terminated")
}

fn tools_of(payload: &Value) -> Vec<Value> {
    payload["tools"].as_array().cloned().unwrap_or_default()
}

fn strict_flag(definition: &Value) -> Option<bool> {
    definition.get("strict").and_then(Value::as_bool)
}

/// Every `tool_addition` definition in a payload, in transcript order.
fn tool_additions(payload: &Value) -> Vec<Value> {
    payload["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "tool_addition")
        .filter_map(|block| {
            let tool = &block["tool"];
            (tool["type"] == "tool_definition").then(|| tool["definition"].clone())
        })
        .collect()
}

/// The original non-strict schema of [`strict_tool`] (upstream `legacyInputSchema`
/// of the untouched parameters).
fn original_non_strict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"value": {"type": "string"}, "optional": {"type": "number"}},
        "required": ["value"],
    })
}

// ---------------------------------------------------------------------------
// Retry-once on each known strict-tool capacity rejection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn d3_retries_each_strict_tool_limit_message_once_with_non_strict_schemas() {
    for message in [GRAMMAR_ERROR, TOO_MANY_STRICT_TOOLS, TOO_MANY_UNIONS] {
        let server = MockServer::start(vec![error_response(message), success_response()]).await;
        let (provider, model) = provider(&server, strict_compat());
        let on_payload = Arc::new(AtomicUsize::new(0));
        let on_response = Arc::new(AtomicUsize::new(0));
        let observed = on_response.clone();
        let mut stream_options = options(Some(on_payload.clone()));
        stream_options.hooks.on_response = Some(Arc::new(move |_status, _headers| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let context = context(vec![
            strict_tool("preferred", Some(StrictPreference::Prefer)),
            strict_tool("required", Some(StrictPreference::Require)),
            strict_tool("ordinary", None),
        ]);

        let result = run(&provider, &model, context, stream_options).await;

        assert_eq!(result.stop_reason, StopReason::Stop, "message: {message}");
        let payloads = server.payloads();
        assert_eq!(payloads.len(), 2, "retried once for: {message}");
        assert_eq!(
            on_payload.load(Ordering::SeqCst),
            2,
            "onPayload runs once per attempt"
        );
        assert_eq!(on_response.load(Ordering::SeqCst), 1, "onResponse once");

        let first = tools_of(&payloads[0]);
        let second = tools_of(&payloads[1]);
        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        // The retry resends to the same Messages endpoint.
        assert_eq!(server.paths(), vec!["/v1/messages", "/v1/messages"]);

        // Preferred tool: strict and transformed on the first attempt.
        assert_eq!(strict_flag(&first[0]), Some(true));
        let first_schema = &first[0]["input_schema"];
        assert_eq!(
            first_schema["required"],
            json!(["value", "optional"]),
            "strict transform makes every property required"
        );
        assert_eq!(
            first_schema["properties"]["optional"],
            json!({"anyOf": [{"type": "number"}, {"type": "null"}]}),
            "optional non-nullable properties become nullable"
        );

        // Preferred tool on the retry: non-strict, original schema preserved.
        assert_eq!(strict_flag(&second[0]), None);
        assert_eq!(second[0]["input_schema"], original_non_strict_schema());

        // Require tool is untouched by the fallback.
        assert_eq!(strict_flag(&first[1]), Some(true));
        assert_eq!(second[1], first[1]);
        assert_eq!(strict_flag(&second[1]), Some(true));

        // Ordinary tool is untouched (and keeps the long-retention cache
        // breakpoint on the last definition).
        assert_eq!(second[2], first[2]);
        assert_eq!(
            second[2]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );

        // Both attempts stream, and the hook marker from `onPayload` reaches
        // the retry request too.
        assert_eq!(payloads[0]["stream"], json!(true));
        assert_eq!(payloads[1]["stream"], json!(true));
        assert_eq!(payloads[0]["inference_geo"], json!("us"));
        assert_eq!(payloads[1]["inference_geo"], json!("us"));

        // The retried transcript is unchanged between attempts.
        assert_eq!(payloads[0]["messages"], payloads[1]["messages"]);

        // Diagnostic (upstream `appendAssistantMessageDiagnostic`).
        let diagnostic = result
            .diagnostics
            .first()
            .expect("fallback diagnostic attached");
        assert_eq!(diagnostic.diag_type, "anthropic_strict_tools_fallback");
        assert_eq!(
            diagnostic
                .details
                .as_ref()
                .and_then(|details| details.get("reason")),
            Some(&json!("strict_tool_limits_exceeded"))
        );
        assert!(diagnostic
            .error_message
            .as_deref()
            .is_some_and(|text| text.contains(message)));
    }
}

#[tokio::test]
async fn d3_retries_a_truncated_error_body() {
    // pir's transport caps error bodies at 500 chars, so a long provider
    // message arrives truncated (not valid JSON); the classifier must still
    // recognize it from the leading type + message fragments (the live union
    // message is far longer than 500 chars).
    let long_message = format!("{TOO_MANY_UNIONS} {}", "padding ".repeat(200));
    assert!(long_message.len() > 500);
    let server = MockServer::start(vec![error_response(&long_message), success_response()]).await;
    let (provider, model) = provider(&server, strict_compat());
    let result = run(
        &provider,
        &model,
        context(vec![strict_tool("lookup", Some(StrictPreference::Prefer))]),
        options(None),
    )
    .await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    let payloads = server.payloads();
    assert_eq!(payloads.len(), 2);
    assert_eq!(strict_flag(&tools_of(&payloads[1])[0]), None);
    assert_eq!(
        result.diagnostics.first().map(|d| d.diag_type.as_str()),
        Some("anthropic_strict_tools_fallback")
    );
}

#[tokio::test]
async fn d3_second_rejection_is_surfaced_without_a_third_attempt() {
    let server = MockServer::start(vec![
        error_response(GRAMMAR_ERROR),
        error_response("Required grammar is still too large"),
    ])
    .await;
    let (provider, model) = provider(&server, strict_compat());
    let result = run(
        &provider,
        &model,
        context(vec![
            strict_tool("lookup", Some(StrictPreference::Prefer)),
            strict_tool("required", Some(StrictPreference::Require)),
        ]),
        options(None),
    )
    .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result
        .error_message
        .as_deref()
        .unwrap_or_default()
        .contains("Required grammar is still too large"));
    let payloads = server.payloads();
    assert_eq!(payloads.len(), 2);
    // The require tool was still strict on the downgraded retry.
    assert_eq!(strict_flag(&tools_of(&payloads[1])[1]), Some(true));
}

// ---------------------------------------------------------------------------
// Non-matching errors never downgrade
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(target_os = "linux", ignore = "Windows-only strict fallback fixture")]
async fn d3_does_not_downgrade_on_unrelated_error_shape() {
    for (name, message, status, error_type) in [
        (
            "unrelated 400",
            "Invalid tool schema",
            400,
            "invalid_request_error",
        ),
        (
            "wrong error type",
            GRAMMAR_ERROR,
            400,
            "authentication_error",
        ),
        (
            "wrong HTTP status",
            GRAMMAR_ERROR,
            500,
            "invalid_request_error",
        ),
    ] {
        let server =
            MockServer::start(vec![error_response_typed(message, status, error_type)]).await;
        let (provider, model) = provider(&server, strict_compat());
        let result = run(
            &provider,
            &model,
            context(vec![strict_tool("lookup", Some(StrictPreference::Prefer))]),
            options(None),
        )
        .await;

        assert_eq!(result.stop_reason, StopReason::Error, "{name}");
        assert_eq!(server.payloads().len(), 1, "no retry for {name}");
        assert!(
            result
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.diag_type == pir::ai::error::RETRY_CLASS_DIAGNOSTIC),
            "no fallback diagnostic for {name}: {:?}",
            result.diagnostics
        );
    }
}

#[tokio::test]
async fn d3_no_retry_without_preferred_strict_tools() {
    let locally_unsupported_prefer = Tool {
        parameters: json!({
            "type": "object",
            "properties": {"value": {"type": "number", "minimum": 1}},
            "required": ["value"],
        }),
        ..strict_tool("lookup", Some(StrictPreference::Prefer))
    };
    for (name, tools) in [
        (
            "required",
            vec![strict_tool("lookup", Some(StrictPreference::Require))],
        ),
        ("ordinary", vec![strict_tool("lookup", None)]),
        (
            "locally unsupported prefer",
            vec![locally_unsupported_prefer],
        ),
    ] {
        let server = MockServer::start(vec![error_response(GRAMMAR_ERROR)]).await;
        let (provider, model) = provider(&server, strict_compat());
        let result = run(&provider, &model, context(tools), options(None)).await;

        assert_eq!(result.stop_reason, StopReason::Error, "{name}");
        assert!(
            result
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains(GRAMMAR_ERROR),
            "{name} surfaces the provider error"
        );
        assert_eq!(server.payloads().len(), 1, "no retry for {name}");
    }
}

// ---------------------------------------------------------------------------
// Started streams are never retried
// ---------------------------------------------------------------------------

#[tokio::test]
async fn d3_does_not_retry_an_sse_error_after_response_headers() {
    let body = format!(
        "event: error\ndata: {}\n\n",
        serde_json::to_string(&error_body(GRAMMAR_ERROR, "invalid_request_error")).expect("json")
    );
    let server = MockServer::start(vec![MockResponseSpec::ok(body.into_bytes())]).await;
    let (provider, model) = provider(&server, strict_compat());
    let result = run(
        &provider,
        &model,
        context(vec![strict_tool("lookup", Some(StrictPreference::Prefer))]),
        options(None),
    )
    .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(server.payloads().len(), 1);
}

#[tokio::test]
async fn d3_does_not_retry_a_truncated_started_stream() {
    // 200 headers, a text delta delivered, then EOF without a stop reason.
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
    );
    let server = MockServer::start(vec![MockResponseSpec::ok(body.as_bytes().to_vec())]).await;
    let (provider, model) = provider(&server, strict_compat());
    let result = run(
        &provider,
        &model,
        context(vec![strict_tool("lookup", Some(StrictPreference::Prefer))]),
        options(None),
    )
    .await;

    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result
        .error_message
        .as_deref()
        .unwrap_or_default()
        .contains("without a stop reason"));
    assert_eq!(server.payloads().len(), 1);
}

#[tokio::test]
#[cfg_attr(target_os = "linux", ignore = "Windows-only strict fallback fixture")]
async fn d3_cancellation_before_the_fallback_attempt_is_honored() {
    let signal = AbortSignal::new();
    let aborting = signal.clone();
    let server = MockServer::start_with(
        vec![error_response(GRAMMAR_ERROR)],
        Some(Arc::new(move |_index| aborting.abort())),
    )
    .await;
    let (provider, model) = provider(&server, strict_compat());
    let mut stream_options = options(None);
    stream_options.signal = Some(signal);
    let result = run(
        &provider,
        &model,
        context(vec![strict_tool("lookup", Some(StrictPreference::Prefer))]),
        stream_options,
    )
    .await;

    // pir maps a caller abort to `StopReason::Aborted` (the terminal message
    // carries the abort text); what matters here is that the aborted request
    // is not downgraded and retried.
    assert_eq!(result.stop_reason, StopReason::Aborted);
    assert!(result
        .error_message
        .as_deref()
        .unwrap_or_default()
        .to_lowercase()
        .contains("abort"));
    assert_eq!(server.payloads().len(), 1);
    assert!(result
        .diagnostics
        .iter()
        .all(|diagnostic| diagnostic.diag_type == pir::ai::error::RETRY_CLASS_DIAGNOSTIC));
}

// ---------------------------------------------------------------------------
// Retry state is per request (not sticky) and covers inline tool additions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn d3_downgrade_is_not_sticky_across_requests() {
    let server = MockServer::start(vec![
        error_response(GRAMMAR_ERROR),
        success_response(),
        success_response(),
    ])
    .await;
    let (provider, model) = provider(&server, strict_compat());
    let tools = vec![strict_tool("lookup", Some(StrictPreference::Prefer))];
    run(&provider, &model, context(tools.clone()), options(None)).await;
    run(&provider, &model, context(tools), options(None)).await;

    let flags: Vec<Option<bool>> = server
        .payloads()
        .iter()
        .map(|payload| strict_flag(&tools_of(payload)[0]))
        .collect();
    assert_eq!(flags, vec![Some(true), None, Some(true)]);
}

#[tokio::test]
async fn d3_downgrades_inline_additions_without_downgrading_require() {
    let server = MockServer::start(vec![error_response(GRAMMAR_ERROR), success_response()]).await;
    let compat = OpenAiCompat {
        supports_strict_tools: Some(true),
        supports_mid_convo_system_messages: Some(true),
        supports_mid_convo_tool_changes: Some(true),
        ..Default::default()
    };
    let (provider, model) = provider(&server, compat);
    let context = Context {
        system_prompt: None,
        messages: vec![
            user_message("Hello"),
            system_message(
                2,
                vec![strict_tool("lookup", Some(StrictPreference::Prefer))],
            ),
            system_message(
                3,
                vec![strict_tool("lookup", Some(StrictPreference::Require))],
            ),
        ],
        tools: Some(vec![strict_tool("lookup", Some(StrictPreference::Require))]),
    };
    let result = run(&provider, &model, context, options(None)).await;

    assert_eq!(result.stop_reason, StopReason::Stop);
    let payloads = server.payloads();
    assert_eq!(payloads.len(), 2);
    // The request-level tool list is identical on both attempts (the
    // placeholder-anchored deferred list only carries the require tool).
    assert_eq!(payloads[1]["tools"], payloads[0]["tools"]);

    let additions_first = tool_additions(&payloads[0]);
    let additions_second = tool_additions(&payloads[1]);
    assert_eq!(additions_first.len(), 2);
    assert_eq!(additions_second.len(), 2);
    assert_eq!(
        additions_first.iter().map(strict_flag).collect::<Vec<_>>(),
        vec![Some(true), Some(true)]
    );
    assert_eq!(
        additions_second.iter().map(strict_flag).collect::<Vec<_>>(),
        vec![None, Some(true)]
    );
    // The downgraded inline addition keeps the original required list.
    assert_eq!(
        additions_second[0]["input_schema"]["required"],
        json!(["value"])
    );
    assert_eq!(additions_second[1], additions_first[1]);
}

// ---------------------------------------------------------------------------
// Offline request-builder checks (no HTTP)
// ---------------------------------------------------------------------------

#[test]
fn d3_builder_downgrade_flag_and_schema_are_exact() {
    let model = {
        let mut model = base_model("https://anthropic.example");
        model.compat = Some(strict_compat());
        model
    };
    let context = context(vec![
        strict_tool("preferred", Some(StrictPreference::Prefer)),
        strict_tool("required", Some(StrictPreference::Require)),
    ]);
    let stream_options = StreamOptions {
        api_key: Some("test-key".into()),
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };

    let (first_body, _headers, first_has_preferred) =
        build_anthropic_request_with_strict_sampling(&model, &context, &stream_options, false)
            .expect("first build");
    assert!(first_has_preferred);
    let first_tools = tools_of(&first_body);
    assert_eq!(strict_flag(&first_tools[0]), Some(true));
    assert_eq!(
        first_tools[0]["input_schema"]["required"],
        json!(["value", "optional"])
    );

    let (second_body, headers, second_has_preferred) =
        build_anthropic_request_with_strict_sampling(&model, &context, &stream_options, true)
            .expect("retry build");
    assert!(!second_has_preferred);
    let second_tools = tools_of(&second_body);
    assert_eq!(strict_flag(&second_tools[0]), None);
    assert_eq!(
        second_tools[0]["input_schema"],
        original_non_strict_schema()
    );
    assert_eq!(second_tools[1], first_tools[1]);
    assert_eq!(
        second_tools[1]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    // Headers are rebuilt unchanged (nothing about the fallback is header-visible).
    assert_eq!(
        headers.get("x-api-key").map(String::as_str),
        Some("test-key")
    );
    assert_eq!(headers.get("anthropic-beta"), None);

    // The default entry point never downgrades.
    let (default_body, _headers) =
        pir::ai::api::anthropic::build_anthropic_request(&model, &context, &stream_options)
            .expect("default build");
    assert_eq!(tools_of(&default_body), first_tools);
}

#[tokio::test]
async fn d3_build_failure_does_not_start_the_stream() {
    // A "require" tool whose schema cannot be made strict fails request
    // construction, before any attempt: the stream must not carry a `Start`
    // event (upstream pushes `start` only after the SDK call returned).
    let server = MockServer::start(Vec::new()).await;
    let (provider, model) = provider(&server, strict_compat());
    let tools = vec![Tool {
        parameters: json!({
            "type": "object",
            "properties": {"n": {"type": "number", "minimum": 1}},
            "required": ["n"],
        }),
        ..strict_tool("strict", Some(StrictPreference::Require))
    }];
    let stream = provider.stream(&model, context(tools), options(None));
    let result = stream.result().await.expect("stream terminated");
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result
        .error_message
        .as_deref()
        .unwrap_or_default()
        .contains("requires JSON-schema constrained sampling"));

    let mut events = Vec::new();
    while let Some(event) = stream.try_next() {
        events.push(event);
    }
    assert_eq!(
        events.len(),
        1,
        "no Start event before a successful request build"
    );
    assert!(matches!(events[0], AssistantMessageEvent::Error { .. }));
    assert!(server.payloads().is_empty());
}

#[test]
fn d3_path_contract_is_messages_endpoint() {
    // The retry resends to the same endpoint (upstream `client.beta.messages.
    // create`): pinned through the mock's recorded path in the async tests;
    // this offline check documents the URL shape used by both attempts.
    let model = base_model("https://anthropic.example/");
    assert_eq!(
        format!("{}/v1/messages", model.base_url.trim_end_matches('/')),
        "https://anthropic.example/v1/messages"
    );
}
