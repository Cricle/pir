//! Provider-specific mock contract tests for the OpenAI-compatible and
//! Anthropic provider adapters.
//!
//! A tiny in-process HTTP server (bound to `127.0.0.1:0`) stands in for the
//! provider endpoints. No external network is ever touched. Each test drives
//! a real provider adapter end to end and asserts on the exact request
//! payload, auth headers, retry behavior, and SSE parsing (including usage,
//! tool-call, and null/missing fixtures).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::agent::agent_loop::{registry_stream_unchecked, run, LoopDeps};
use pir::ai::api::anthropic::AnthropicMessagesProvider;
use pir::ai::api::openai::OpenAiCompatibleProvider;
use pir::ai::api::openai_responses::OpenAiResponsesProvider;
use pir::ai::auth::{InMemoryCredentialStore, MapAuthContext};
use pir::ai::classify;
use pir::ai::error::{take_turn_retry_class, TurnRetryClass};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::images;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::*;
use pir::ai::util::AgentRetryPolicy;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Mock HTTP server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MockRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
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

    fn error(status: u16) -> Self {
        Self {
            status,
            reason: match status {
                429 => "Too Many Requests".into(),
                500 => "Internal Server Error".into(),
                _ => "Error".into(),
            },
            headers: vec![],
            body: b"mock error".to_vec(),
        }
    }

    fn error_with_retry_after(seconds: u64) -> Self {
        Self {
            status: 429,
            reason: "Too Many Requests".into(),
            headers: vec![("retry-after".into(), seconds.to_string())],
            body: b"slow down".to_vec(),
        }
    }

    /// 429 with a `retry-after-ms` header (the upstream-preferred form).
    fn error_with_retry_after_ms(millis: u64) -> Self {
        Self {
            status: 429,
            reason: "Too Many Requests".into(),
            headers: vec![("retry-after-ms".into(), millis.to_string())],
            body: b"slow down".to_vec(),
        }
    }
}

/// Serves each response spec in order, one per accepted connection, and
/// records every request that arrives.
struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(responses: Vec<MockResponseSpec>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handle = tokio::spawn(async move {
            for response in responses {
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

    fn requests(&self) -> Vec<MockRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
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
            let method = parts.next()?.to_string();
            let path = parts.next()?.to_string();
            let mut headers = BTreeMap::new();
            let mut content_length = 0usize;
            for line in lines {
                if let Some((key, value)) = line.split_once(':') {
                    let key = key.trim().to_ascii_lowercase();
                    let value = value.trim().to_string();
                    if key == "content-length" {
                        content_length = value.parse().unwrap_or(0);
                    }
                    headers.insert(key, value);
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
            let body = buf[body_start..end].to_vec();
            return Some(MockRequest {
                method,
                path,
                headers,
                body,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn openai_text_sse() -> Vec<u8> {
    concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec()
}

fn openai_tool_call_sse() -> Vec<u8> {
    concat!(
        "data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"a.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec()
}

fn openai_null_and_missing_sse() -> Vec<u8> {
    concat!(
        "data: {}\n\n",
        "data: {\"choices\":[{}]}\n\n",
        "data: {\"choices\":[{\"delta\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":null}}]}\n\n",
        "data: {\"choices\":[{\"finish_reason\":null}]}\n\n",
        "data: {\"id\":null,\"usage\":null}\n\n",
        // Terminal finish reason (the supportsFinishReason default rejects
        // streams that end without one).
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec()
}

fn anthropic_text_sse() -> Vec<u8> {
    concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3-5-sonnet-latest\",\"stop_reason\":null,\"usage\":{\"input_tokens\":15,\"output_tokens\":1,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":5}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .as_bytes()
    .to_vec()
}

fn anthropic_tool_use_sse() -> Vec<u8> {
    concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"rust\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"read\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":55}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .as_bytes()
    .to_vec()
}

fn anthropic_null_and_missing_sse() -> Vec<u8> {
    concat!(
        "data: {}\n\n",
        "data: {\"type\":\"message_start\"}\n\n",
        "data: {\"type\":\"content_block_start\",\"content_block\":{}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .as_bytes()
    .to_vec()
}

// ---------------------------------------------------------------------------
// Model/context builders
// ---------------------------------------------------------------------------

fn openai_model(base_url: &str) -> Model {
    Model {
        id: "gpt-4o-mini".into(),
        name: "GPT-4o mini".into(),
        api: "openai-chat".into(),
        provider: "openai".into(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.15,
                output: 0.6,
                cache_read: 0.075,
                cache_write: 0.15,
            },
            tiers: Vec::new(),
        },
        context_window: 128_000,
        max_tokens: 16_384,
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

fn anthropic_model(base_url: &str) -> Model {
    Model {
        id: "claude-3-5-sonnet-latest".into(),
        name: "Claude 3.5 Sonnet".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            tiers: Vec::new(),
        },
        context_window: 200_000,
        max_tokens: 8_192,
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

fn fast_client() -> ProviderRequestClient {
    ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 8 * 1024 * 1024,
        max_retries: 1,
        max_retry_delay: Duration::from_millis(20),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("fast client")
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

async fn run_to_done(stream: AssistantMessageEventStream) -> Arc<AssistantMessage> {
    stream.result().await.expect("stream terminated")
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn call_block(id: &str, name: &str, arguments: BTreeMap<String, Value>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
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

fn simple_context(user_text: &str) -> Context {
    Context {
        messages: vec![user_message(user_text)],
        ..Context::default()
    }
}

fn search_tool() -> Tool {
    Tool {
        name: "search".into(),
        description: "Search the codebase".into(),
        parameters: json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        constrained_sampling: None,
    }
}

// ---------------------------------------------------------------------------
// OpenAI contract tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_payload_contract_sends_system_tools_and_stream_options() {
    let server = MockServer::start(vec![MockResponseSpec::ok(openai_text_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let context = Context {
        system_prompt: Some("You are helpful".into()),
        messages: vec![user_message("Hello")],
        tools: Some(vec![search_tool()]),
    };
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some("test-key".into()),
            temperature: Some(0.7),
            max_tokens: Some(512),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(
        req.headers.get("authorization").map(String::as_str),
        Some("Bearer test-key")
    );
    assert_eq!(
        req.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let body: Value = serde_json::from_slice(&req.body).expect("request body json");
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["temperature"], 0.7);
    // Upstream default: standard OpenAI endpoints take max_completion_tokens
    // (max_tokens is the per-compat exception path).
    assert_eq!(body["max_completion_tokens"], 512);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "You are helpful");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "Hello");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "search");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"]["query"]["type"],
        "string"
    );
}

#[tokio::test]
async fn openai_payload_replays_assistant_tool_calls_and_tool_results() {
    let server = MockServer::start(vec![MockResponseSpec::ok(openai_text_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();

    let mut args = BTreeMap::new();
    args.insert("q".to_string(), json!("x"));
    let assistant = Message::Assistant(AssistantMessage {
        content: vec![
            text_block("let me look"),
            call_block("call_1", "search", args),
        ],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    });
    let tool_result = Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call_1".into(),
        tool_name: "search".into(),
        content: vec![text_block("found")],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 2,
    });
    let context = Context {
        messages: vec![assistant, tool_result, user_message("continue")],
        ..Context::default()
    };
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    run_to_done(stream).await;

    let body: Value = serde_json::from_slice(&server.requests()[0].body).expect("request body");
    assert_eq!(body["messages"][0]["role"], "assistant");
    assert_eq!(body["messages"][0]["content"], "let me look");
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][0]["tool_calls"][0]["type"], "function");
    assert_eq!(
        body["messages"][0]["tool_calls"][0]["function"]["name"],
        "search"
    );
    assert_eq!(
        body["messages"][0]["tool_calls"][0]["function"]["arguments"],
        "{\"q\":\"x\"}"
    );
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
    assert_eq!(body["messages"][1]["content"], "found");
    assert_eq!(body["messages"][2]["role"], "user");
}

#[tokio::test]
async fn openai_sse_stream_aggregates_text_usage_and_response_id() {
    let server = MockServer::start(vec![MockResponseSpec::ok(openai_text_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("Hi"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(final_message.model, "gpt-4o-mini");
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.usage.input, 10);
    assert_eq!(final_message.usage.output, 2);
    assert_eq!(final_message.usage.total_tokens, 12);
    match &final_message.content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "Hello world"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_stream_applies_backpressure_at_the_queue_bound() {
    use futures_util::StreamExt;
    use pir::ai::event_stream::DEFAULT_EVENT_QUEUE_CAPACITY;

    // 200 deltas: far past the queue bound. Without producer backpressure the
    // adapter would enqueue every parsed event (each carrying a frozen
    // cumulative `partial`) before this test's first poll.
    let mut body = String::new();
    for index in 0..200 {
        body.push_str(&format!(
            "data: {{\"id\":\"chatcmpl-bp\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"t{index} \"}}}}]}}\n\n"
        ));
    }
    body.push_str(
        "data: {\"id\":\"chatcmpl-bp\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":200,\"total_tokens\":201}}\n\n",
    );
    body.push_str("data: [DONE]\n\n");

    let server = MockServer::start(vec![MockResponseSpec::ok(body.into_bytes())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let mut stream = provider.stream(
        &model,
        simple_context("Hi"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );

    // No consumer polls: the producer must stop exactly at the bound (it
    // parks on `push_await`; the transport body read pauses with it).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stream.pending_count() < DEFAULT_EVENT_QUEUE_CAPACITY
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        stream.pending_count(),
        DEFAULT_EVENT_QUEUE_CAPACITY,
        "the adapter must park at the queue bound instead of buffering the reply"
    );
    assert!(!stream.is_done(), "the stream is mid-reply, not finished");

    // Draining releases the producer; every event arrives exactly once and in
    // order, and the terminal still closes the stream.
    let deltas = tokio::time::timeout(Duration::from_secs(10), async {
        let mut deltas = Vec::new();
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                deltas.push(delta);
            }
        }
        deltas
    })
    .await
    .expect("the stream must complete once drained");
    assert_eq!(deltas.len(), 200);
    assert_eq!(deltas[0], "t0 ");
    assert_eq!(deltas[199], "t199 ");
    assert!(stream.max_queue_depth() <= DEFAULT_EVENT_QUEUE_CAPACITY);
    assert!(stream.push_wait_count() > 0, "the producer had to wait");
    let final_message = stream.result().await.expect("terminal result");
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.usage.output, 200);
}

#[tokio::test]
async fn openai_tool_call_stream_terminates_with_tool_use() {
    let server = MockServer::start(vec![MockResponseSpec::ok(openai_tool_call_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("read a.txt"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    match &final_message.content[0] {
        ContentBlock::ToolCall(t) => {
            assert_eq!(t.id, "call_1");
            assert_eq!(t.name, "read_file");
            assert_eq!(
                t.arguments.get("path").and_then(Value::as_str),
                Some("a.txt")
            );
        }
        other => panic!("expected tool call block, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_retries_transient_error_then_succeeds() {
    let server = MockServer::start(vec![
        MockResponseSpec::error(500),
        MockResponseSpec::ok(openai_text_sse()),
    ])
    .await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("retry me"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected one retry attempt");
    assert_eq!(
        requests[0].body, requests[1].body,
        "retry resends same payload"
    );
}

#[tokio::test]
async fn openai_retries_on_429_honoring_retry_after() {
    // `retry-after-ms` wins over `retry-after` (upstream provider-retry
    // order); 5ms fits inside the fast client's 20ms retry-delay cap.
    let server = MockServer::start(vec![
        MockResponseSpec::error_with_retry_after_ms(5),
        MockResponseSpec::ok(openai_text_sse()),
    ])
    .await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("rate limited"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(
        server.requests().len(),
        2,
        "expected one 429-triggered retry"
    );
}

#[tokio::test]
async fn openai_fails_fast_when_retry_after_exceeds_max_delay() {
    // Upstream provider-retry: a server-requested delay above the configured
    // cap fails the request immediately instead of clamping the sleep.
    let server = MockServer::start(vec![
        MockResponseSpec::error_with_retry_after(90),
        MockResponseSpec::ok(openai_text_sse()),
    ])
    .await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(
            ProviderRequestClient::new(ProviderRequestConfig {
                timeout: Duration::from_secs(10),
                max_response_bytes: 8 * 1024 * 1024,
                max_retries: 1,
                max_retry_delay: Duration::from_secs(60),
                proxy_url: None,
                http_idle_timeout: None,
            })
            .expect("client"),
        );
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("rate limited"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        message.contains("Server requested 90s retry delay (max: 60s)."),
        "error message should report the over-cap retry delay, got: {message}"
    );
    assert_eq!(
        server.requests().len(),
        1,
        "no retry attempt may follow an over-cap server delay"
    );
}

#[tokio::test]
async fn openai_exhausted_retries_encode_error_message() {
    let server = MockServer::start(vec![MockResponseSpec::error(500)]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("boom"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    assert_eq!(
        server.requests().len(),
        1,
        "the terminal status is sent exactly once (no hidden retry)"
    );
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        message.contains("HTTP 500"),
        "error message should surface the HTTP status, got: {message}"
    );
    // The REAL adapter finalizer also carried the structural verdict: a
    // status the transport refused to retry is NOT re-issued at agent level
    // even though "500" is retryable-looking text.
    let mut terminal = (*final_message).clone();
    assert_eq!(
        take_turn_retry_class(&mut terminal),
        Some(TurnRetryClass::NonRetryable),
        "exhausted status must be structurally non-retryable, got: {message}"
    );
    assert!(
        terminal.diagnostics.is_empty(),
        "taking the verdict removes the internal carrier"
    );
}

/// The adapter-prefixed Responses path: `"OpenAI API error: "` hides the
/// `"non-retryable provider error:"` marker from any text sniffing, but the
/// structural verdict survives and the user-facing text is unchanged.
#[tokio::test]
async fn openai_responses_prefixed_error_keeps_the_structural_class() {
    let server = MockServer::start(vec![MockResponseSpec::error(503)]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiResponsesProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("boom"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    assert_eq!(
        server.requests().len(),
        1,
        "the prefixed terminal status is sent exactly once (no hidden retry)"
    );
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        message.starts_with("OpenAI API error: "),
        "the adapter prefix must stay in the user-facing text, got: {message}"
    );
    assert!(
        message.contains("503"),
        "the flattened text stays retryable-looking, got: {message}"
    );
    let mut terminal = (*final_message).clone();
    assert_eq!(
        take_turn_retry_class(&mut terminal),
        Some(TurnRetryClass::NonRetryable),
        "the prefix must not change the structural verdict, got: {message}"
    );
}

/// A pre-response transport failure exhausts the transport budget and is
/// reported as `AiError::RetryExhausted`, whose `user_message()` drops the
/// `"retry exhausted after …"` marker. The terminal message must still carry
/// the structural `TransportExhausted` verdict so the agent loop never
/// re-issues the request.
#[tokio::test]
async fn openai_transport_exhausted_carries_the_transport_verdict() {
    // Reserve a port, then close it: every connection is refused, so the
    // transport's send fails before headers and it reports RetryExhausted.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    let base = format!("http://{addr}/v1");
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("boom"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        !message.starts_with("retry exhausted after"),
        "the flattened text lost the marker, got: {message}"
    );
    let mut terminal = (*final_message).clone();
    assert_eq!(
        take_turn_retry_class(&mut terminal),
        Some(TurnRetryClass::TransportExhausted),
        "the exhausted transport budget must be structural, got: {message}"
    );
}

/// END-TO-END budget separation (the CL21 defect): the transport retries a
/// retryable 5xx itself (2 retries = 3 requests) and then renders its
/// terminal `NonRetryable` verdict. The agent-level turn-retry policy (3
/// retries) must NOT re-issue the turn: the server sees exactly the
/// transport's 3 requests, not the 3 x 4 = 12 the textual classifier allowed
/// once the adapter/transport wording matched a retryable pattern.
#[tokio::test]
async fn transport_budget_is_not_multiplied_by_agent_turn_retry() {
    // Enough specs that a buggy re-issue would be observable (4 rounds x 3
    // transport attempts). A correct run consumes only the first three.
    let responses = (0..12).map(|_| MockResponseSpec::error(500)).collect();
    let server = MockServer::start(responses).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(retry_after_cap_client(2));
    let model = provider.get_models()[0].clone();

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));
    let registry = Arc::new(registry);
    let deps = LoopDeps {
        stream: registry_stream_unchecked(
            Arc::clone(&registry),
            model,
            StreamOptions {
                api_key: Some("test-key".into()),
                ..StreamOptions::default()
            },
        ),
        retry: Some(AgentRetryPolicy {
            max_retries: 3,
            base_delay_ms: 0,
            max_agent_delay_ms: 0,
        }),
        ..LoopDeps::default()
    };

    let result = run(deps, Context::default(), None).await;

    assert_eq!(
        server.requests().len(),
        3,
        "the transport's budget (initial + 2 retries) is the whole request count"
    );
    assert_eq!(result.turns, 1, "exactly one agent-level round");
    assert_eq!(result.terminal.stop_reason, StopReason::Error);
    assert!(
        result
            .terminal
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("HTTP 500")),
        "the terminal error is the transport verdict: {:?}",
        result.terminal.error_message
    );
    assert!(
        result.terminal.diagnostics.is_empty(),
        "the agent loop consumes the internal retry-class carrier"
    );
}

/// CL32 review D2, end-to-end: a 2xx stream body that ends cleanly WITHOUT the
/// provider terminal event reaches the agent loop as a retryable turn, so the
/// run re-issues the request exactly once and completes on the healthy reply.
/// The transport reports no read error here (the body was read in full), so
/// only the adapter's truncation classification can save the turn.
#[tokio::test]
async fn truncated_openai_stream_reissues_the_turn_exactly_once() {
    // First response: content, then `[DONE]` with no finish_reason chunk.
    let truncated = concat!(
        "data: {\"id\":\"chatcmpl-truncated\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"half a\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec();
    let server = MockServer::start(vec![
        MockResponseSpec::ok(truncated),
        MockResponseSpec::ok(openai_text_sse()),
    ])
    .await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));
    let registry = Arc::new(registry);
    let deps = LoopDeps {
        stream: registry_stream_unchecked(
            Arc::clone(&registry),
            model,
            StreamOptions {
                api_key: Some("test-key".into()),
                ..StreamOptions::default()
            },
        ),
        retry: Some(AgentRetryPolicy {
            max_retries: 3,
            base_delay_ms: 0,
            max_agent_delay_ms: 0,
        }),
        ..LoopDeps::default()
    };

    let result = run(deps, Context::default(), None).await;

    assert_eq!(
        server.requests().len(),
        2,
        "the truncation is re-issued exactly once (no transport retry budget)"
    );
    assert_eq!(result.turns, 1, "a retry is the SAME turn");
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert_eq!(
        result.terminal.content,
        vec![text_block("Hello world")],
        "the healthy reply is the terminal message"
    );
    assert!(
        result.terminal.diagnostics.is_empty(),
        "the retried turn's message carries no internal carrier"
    );
    assert!(
        !result.context.messages.iter().any(|message| matches!(
            message,
            Message::Assistant(a) if a.stop_reason == StopReason::Error
        )),
        "the truncated attempt never enters the transcript: {:?}",
        result.context.messages
    );
}

// ---------------------------------------------------------------------------
// Transport retry observability (upstream `auto_retry_start` / `auto_retry_end`,
// agent-session.ts:220-222): the transport reports every scheduled retry and
// the retried request's outcome, without changing retry behavior. This is what
// makes a JSON `--mode json` stream able to tell a 429 + backoff apart from a
// gateway that simply queued the request.
// ---------------------------------------------------------------------------

/// A retry recorder shared by the two transport retry observers.
#[derive(Default)]
struct RetryEventLog {
    starts: Mutex<Vec<AutoRetryStart>>,
    ends: Mutex<Vec<AutoRetryEnd>>,
}

impl RetryEventLog {
    fn hooks(self: &Arc<Self>) -> ProviderStreamHooks {
        let starts = Arc::clone(self);
        let ends = Arc::clone(self);
        ProviderStreamHooks {
            on_retry_start: Some(Arc::new(move |start| {
                starts
                    .starts
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(start.clone());
            })),
            on_retry_end: Some(Arc::new(move |end| {
                ends.ends
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(end.clone());
            })),
            ..Default::default()
        }
    }

    fn starts(&self) -> Vec<AutoRetryStart> {
        self.starts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn ends(&self) -> Vec<AutoRetryEnd> {
        self.ends.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// A client with the given retry budget and a 60s retry-delay cap, so a
/// `Retry-After: 1` fixture fits and the retry wait is actually performed.
fn retry_after_cap_client(max_retries: u32) -> ProviderRequestClient {
    ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 8 * 1024 * 1024,
        max_retries,
        max_retry_delay: Duration::from_secs(60),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("retry-after cap client")
}

/// 429 + `Retry-After: 1` → one scheduled retry: `auto_retry_start` carries the
/// 1-based attempt, the budget, the server-requested delay and the trigger
/// error; `auto_retry_end { success: true }` reports the retried request's
/// outcome; the stream still succeeds.
#[tokio::test]
async fn openai_429_retry_after_emits_auto_retry_start_and_success_end() {
    let server = MockServer::start(vec![
        MockResponseSpec::error_with_retry_after(1),
        MockResponseSpec::ok(openai_text_sse()),
    ])
    .await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(retry_after_cap_client(2));
    let model = provider.get_models()[0].clone();

    let log = Arc::new(RetryEventLog::default());
    let stream = provider.stream(
        &model,
        simple_context("rate limited"),
        StreamOptions {
            api_key: Some("test-key".into()),
            hooks: log.hooks(),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(
        server.requests().len(),
        2,
        "expected one 429-triggered retry"
    );

    assert_eq!(
        log.starts().as_slice(),
        &[AutoRetryStart {
            attempt: 1,
            max_attempts: 2,
            // The `Retry-After: 1` header wins over exponential backoff, and
            // `delayMs` reports exactly that server-requested wait.
            delay_ms: 1_000,
            error_message: "HTTP 429".into(),
        }],
        "one auto_retry_start with the post-Retry-After delay"
    );
    assert_eq!(
        log.ends().as_slice(),
        &[AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        }],
        "auto_retry_end reports the retried request's success"
    );
}

/// Retries exhausted (two 429s, budget 1): `auto_retry_start` once and
/// `auto_retry_end { success: false }` with the final error, while the error
/// path itself is unchanged (`HTTP 429` in the assistant message).
#[tokio::test]
async fn openai_exhausted_retries_emit_auto_retry_end_failure() {
    let server = MockServer::start(vec![
        MockResponseSpec::error_with_retry_after_ms(1),
        MockResponseSpec::error_with_retry_after_ms(1),
    ])
    .await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(retry_after_cap_client(1));
    let model = provider.get_models()[0].clone();

    let log = Arc::new(RetryEventLog::default());
    let stream = provider.stream(
        &model,
        simple_context("rate limited"),
        StreamOptions {
            api_key: Some("test-key".into()),
            hooks: log.hooks(),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    // The pre-existing error path is untouched: still a terminal error message
    // carrying the exhausted status.
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(message.contains("HTTP 429"), "got: {message}");
    assert_eq!(server.requests().len(), 2, "one retry, then exhaustion");

    assert_eq!(
        log.starts().as_slice(),
        &[AutoRetryStart {
            attempt: 1,
            max_attempts: 1,
            delay_ms: 1,
            error_message: "HTTP 429".into(),
        }]
    );
    let ends = log.ends();
    assert_eq!(ends.len(), 1, "exactly one auto_retry_end: {ends:?}");
    assert!(!ends[0].success);
    assert_eq!(ends[0].attempt, 1);
    assert!(
        ends[0]
            .final_error
            .as_deref()
            .unwrap_or_default()
            .contains("HTTP 429"),
        "finalError surfaces the exhausted status: {ends:?}"
    );
}

/// A non-retryable 4xx (400) fails fast: no retry is scheduled, so neither
/// `auto_retry_start` nor `auto_retry_end` may be emitted.
#[tokio::test]
async fn openai_non_retryable_status_emits_no_retry_events() {
    let server = MockServer::start(vec![MockResponseSpec::error(400)]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(retry_after_cap_client(2));
    let model = provider.get_models()[0].clone();

    let log = Arc::new(RetryEventLog::default());
    let stream = provider.stream(
        &model,
        simple_context("bad request"),
        StreamOptions {
            api_key: Some("test-key".into()),
            hooks: log.hooks(),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(message.contains("HTTP 400"), "got: {message}");
    assert_eq!(server.requests().len(), 1, "4xx must not retry");
    assert!(log.starts().is_empty(), "no retry was scheduled");
    assert!(log.ends().is_empty(), "no retried request to report");
}

/// A server-requested delay above the cap fails fast (unchanged) and schedules
/// no retry, so no retry events fire.
#[tokio::test]
async fn openai_over_cap_retry_after_emits_no_retry_events() {
    let server = MockServer::start(vec![MockResponseSpec::error_with_retry_after(90)]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(retry_after_cap_client(1));
    let model = provider.get_models()[0].clone();

    let log = Arc::new(RetryEventLog::default());
    let stream = provider.stream(
        &model,
        simple_context("rate limited"),
        StreamOptions {
            api_key: Some("test-key".into()),
            hooks: log.hooks(),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    assert_eq!(server.requests().len(), 1, "no retry may follow the cap");
    assert!(log.starts().is_empty());
    assert!(log.ends().is_empty());
}

#[tokio::test]
async fn openai_null_and_missing_sse_chunks_do_not_break_stream() {
    let server = MockServer::start(vec![MockResponseSpec::ok(openai_null_and_missing_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("sparse"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert!(final_message.content.is_empty());
    assert_eq!(final_message.usage.total_tokens, 0);
}

// ---------------------------------------------------------------------------
// Anthropic contract tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_payload_contract_includes_system_tools_and_version() {
    let server = MockServer::start(vec![MockResponseSpec::ok(anthropic_text_sse())]).await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let context = Context {
        system_prompt: Some("You are helpful".into()),
        messages: vec![user_message("Hello")],
        tools: Some(vec![search_tool()]),
    };
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some("test-key".into()),
            temperature: Some(0.3),
            max_tokens: Some(128),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(
        req.headers.get("x-api-key").map(String::as_str),
        Some("test-key")
    );
    assert_eq!(
        req.headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
    let body: Value = serde_json::from_slice(&req.body).expect("request body json");
    assert_eq!(body["model"], "claude-3-5-sonnet-latest");
    assert_eq!(body["max_tokens"], 128);
    assert_eq!(body["temperature"], 0.3);
    assert_eq!(body["stream"], true);
    // System prompt goes out as a text block (upstream buildParams).
    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(body["system"][0]["text"], "You are helpful");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "Hello");
    assert_eq!(body["tools"][0]["name"], "search");
    assert_eq!(body["tools"][0]["description"], "Search the codebase");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
}

#[tokio::test]
async fn anthropic_payload_replays_tool_use_and_tool_errors() {
    let server = MockServer::start(vec![MockResponseSpec::ok(anthropic_text_sse())]).await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();

    let mut args_a = BTreeMap::new();
    args_a.insert("q".to_string(), json!("rust"));
    let mut args_b = BTreeMap::new();
    args_b.insert("path".to_string(), json!("Cargo.toml"));
    let assistant = Message::Assistant(AssistantMessage {
        content: vec![
            call_block("toolu_1", "search", args_a),
            call_block("toolu_2", "read", args_b),
        ],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    });
    let ok_result = Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "toolu_1".into(),
        tool_name: "search".into(),
        content: vec![text_block("3 hits")],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 2,
    });
    let err_result = Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "toolu_2".into(),
        tool_name: "read".into(),
        content: Vec::new(),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: true,
        duration_ms: None,
        timestamp: 3,
    });
    let context = Context {
        messages: vec![assistant, ok_result, err_result, user_message("continue")],
        ..Context::default()
    };
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    run_to_done(stream).await;

    let body: Value = serde_json::from_slice(&server.requests()[0].body).expect("request body");
    let blocks = &body["messages"][0]["content"];
    assert_eq!(body["messages"][0]["role"], "assistant");
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["id"], "toolu_1");
    assert_eq!(blocks[0]["name"], "search");
    assert_eq!(blocks[0]["input"]["q"], "rust");
    assert_eq!(blocks[1]["type"], "tool_use");
    assert_eq!(blocks[1]["id"], "toolu_2");
    assert_eq!(blocks[1]["input"]["path"], "Cargo.toml");

    // Consecutive tool results merge into a single user message (upstream
    // convertMessages, required for z.ai's Anthropic endpoint).
    let first_result = &body["messages"][1]["content"][0];
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(first_result["type"], "tool_result");
    assert_eq!(first_result["tool_use_id"], "toolu_1");
    assert_eq!(first_result["content"], "3 hits");
    assert!(first_result.get("is_error").is_none());

    let second_result = &body["messages"][1]["content"][1];
    assert_eq!(second_result["tool_use_id"], "toolu_2");
    assert_eq!(second_result["is_error"], true);
    assert_eq!(second_result["content"], "");
}

#[tokio::test]
async fn anthropic_sse_stream_with_cache_usage() {
    let server = MockServer::start(vec![MockResponseSpec::ok(anthropic_text_sse())]).await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("cache me"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.response_id.as_deref(), Some("msg_1"));
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.usage.input, 15);
    assert_eq!(final_message.usage.output, 12);
    // Upstream computes totalTokens as input + output + cacheRead + cacheWrite.
    assert_eq!(final_message.usage.total_tokens, 42);
    assert_eq!(final_message.usage.cache_read, 5);
    assert_eq!(final_message.usage.cache_write, 10);
    match &final_message.content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, "Hello world"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_sse_multiple_tool_use_blocks() {
    let server = MockServer::start(vec![MockResponseSpec::ok(anthropic_tool_use_sse())]).await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("use tools"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::ToolUse);
    assert_eq!(final_message.content.len(), 2);
    match &final_message.content[0] {
        ContentBlock::ToolCall(t) => {
            assert_eq!(t.id, "toolu_1");
            assert_eq!(t.arguments.get("q").and_then(Value::as_str), Some("rust"));
        }
        other => panic!("expected tool call block, got {other:?}"),
    }
    match &final_message.content[1] {
        ContentBlock::ToolCall(t) => {
            assert_eq!(t.id, "toolu_2");
            assert_eq!(
                t.arguments.get("path").and_then(Value::as_str),
                Some("Cargo.toml")
            );
        }
        other => panic!("expected tool call block, got {other:?}"),
    }
    assert_eq!(final_message.usage.input, 42);
    assert_eq!(final_message.usage.output, 55);
    assert_eq!(final_message.usage.total_tokens, 97);
}

#[tokio::test]
async fn anthropic_retries_transient_error_then_succeeds() {
    let server = MockServer::start(vec![
        MockResponseSpec::error(500),
        MockResponseSpec::ok(anthropic_text_sse()),
    ])
    .await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("retry me"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected one retry attempt");
    assert_eq!(
        requests[0].body, requests[1].body,
        "retry resends same payload"
    );
}

#[tokio::test]
async fn anthropic_exhausted_retries_encode_error_message() {
    let server = MockServer::start(vec![MockResponseSpec::error(500)]).await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("boom"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let message = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        message.contains("HTTP 500"),
        "error message should surface the HTTP status, got: {message}"
    );
}

#[tokio::test]
async fn anthropic_null_and_missing_sse_chunks_do_not_break_stream() {
    let server =
        MockServer::start(vec![MockResponseSpec::ok(anthropic_null_and_missing_sse())]).await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url(),
        vec![anthropic_model(server.base_url())],
    )
    .expect("provider")
    .with_client(fast_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("sparse"),
        StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert!(final_message.content.is_empty());
    assert_eq!(final_message.usage.total_tokens, 0);
}

// ---------------------------------------------------------------------------
// Registry auth flow tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_resolves_env_api_key_auth_and_dispatches() {
    let server = MockServer::start(vec![MockResponseSpec::ok(openai_text_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiCompatibleProvider::new("openai", base.clone(), vec![openai_model(&base)])
        .expect("provider")
        .with_client(fast_client());
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(BTreeMap::from([(
            "OPENAI_API_KEY".to_string(),
            "env-key".to_string(),
        )]))),
    );
    registry.set_provider(Arc::new(provider));
    let model = registry.get_model("openai", "gpt-4o-mini").expect("model");
    let final_message = registry
        .complete(model, simple_context("Hi"), StreamOptions::default())
        .await
        .expect("complete should return a message");
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    let req = &server.requests()[0];
    assert_eq!(
        req.headers.get("authorization").map(String::as_str),
        Some("Bearer env-key"),
        "registry should resolve auth from env context"
    );
}

#[tokio::test]
async fn registry_encodes_unconfigured_provider_as_stream_error() {
    let provider = OpenAiCompatibleProvider::new(
        "openai",
        "https://example.invalid/v1",
        vec![openai_model("https://example.invalid/v1")],
    )
    .expect("provider");
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(MapAuthContext::new(BTreeMap::new())),
    );
    registry.set_provider(Arc::new(provider));
    let model = registry.get_model("openai", "gpt-4o-mini").expect("model");
    let final_message = registry
        .complete(model, simple_context("Hi"), StreamOptions::default())
        .await
        .expect("complete returns message even for auth errors");
    assert_eq!(final_message.stop_reason, StopReason::Error);
    let error_text = final_message.error_message.as_deref().unwrap_or("");
    assert!(
        error_text.contains("not configured"),
        "expected 'not configured', got: {error_text}"
    );
}

// ---------------------------------------------------------------------------
// Image generation / structured classification request options
// (upstream `ImagesOptions` / `ClassifierOptions` extending
// `ProviderRequestOptions`, plus System One's image-input forms)
// ---------------------------------------------------------------------------

fn json_ok(body: &str) -> MockResponseSpec {
    MockResponseSpec {
        status: 200,
        reason: "OK".into(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.as_bytes().to_vec(),
    }
}

fn classifier_contract_model(
    api: ClassifierApi,
    provider: &str,
    id: &str,
    base_url: &str,
) -> ClassifierModel {
    ClassifierModel {
        id: id.into(),
        name: id.into(),
        api,
        provider: provider.into(),
        base_url: base_url.into(),
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        headers: None,
        context_window: 4096,
    }
}

fn image_contract_model(base_url: &str) -> ImageModel {
    ImageModel {
        id: "openrouter/image-gen".into(),
        name: "Image Gen".into(),
        api: ImageApi::OpenRouterImages,
        provider: "openrouter".into(),
        base_url: base_url.into(),
        input: vec![InputModality::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        headers: None,
        output: vec![OutputModality::Image],
    }
}

fn bool_classifier_context(image: Option<ImageContent>) -> ClassifierContext {
    ClassifierContext {
        state: json!({"text": "judge this"}),
        images: image.into_iter().collect(),
        questions: BTreeMap::from([(
            "approved".to_string(),
            ClassifierQuestion::Bool {
                instructions: "Approve?".into(),
                criteria: ClassifierBoolCriteria {
                    true_meaning: "approved".into(),
                    false_meaning: "rejected".into(),
                },
            },
        )]),
    }
}

fn classifier_answer_body() -> &'static str {
    r#"{"answers":{"approved":{"type":"noul","noul":0.8}},"usage":{"input_tokens":5,"output_tokens":2}}"#
}

#[tokio::test]
async fn typesafe_system_one_images_use_assets_and_the_opt_in_header() {
    let server = MockServer::start(vec![json_ok(classifier_answer_body())]).await;
    let model = classifier_contract_model(
        ClassifierApi::TypesafeSystemOne,
        "typesafe",
        "system-one",
        server.base_url(),
    );
    let context = bool_classifier_context(Some(ImageContent {
        data: "aW1n".into(),
        mime_type: "image/png".into(),
    }));
    let options = ClassifierRequestOptions {
        // Extended options: the provider-scoped env map resolves the key.
        env: Some(BTreeMap::from([(
            "TYPESAFE_API_KEY".to_string(),
            "env-key".to_string(),
        )])),
        ..ClassifierRequestOptions::default()
    };
    let result = classify::classify_with(&model, &context, &options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(
        result.answers.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.8 })
    );
    let usage = result.usage.expect("usage parsed");
    assert_eq!(usage.cost.total_from_provider, Some(false));
    assert_eq!(usage.cost.total, usage.cost.input + usage.cost.output);

    let request = &server.requests()[0];
    assert_eq!(request.path, "/systemone");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer env-key")
    );
    assert_eq!(
        request
            .headers
            .get("typesafe-danger-zone-images")
            .map(String::as_str),
        Some("true")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("body json");
    assert_eq!(body["model"], "system-one");
    assert_eq!(
        body["assets"]["<image-1>"],
        json!({"type": "image", "format": "png", "data_base64": "aW1n"})
    );
    assert_eq!(body["state"], json!(["<image-1>", {"text": "judge this"}]));
    assert_eq!(body["questions"]["approved"]["type"], "noul");
}

#[tokio::test]
async fn openrouter_system_one_images_ride_in_the_state_parts() {
    let server = MockServer::start(vec![json_ok(classifier_answer_body())]).await;
    let model = classifier_contract_model(
        ClassifierApi::TypesafeSystemOne,
        "openrouter",
        "system-one",
        server.base_url(),
    );
    let context = bool_classifier_context(Some(ImageContent {
        data: "aW1n".into(),
        mime_type: "image/jpeg".into(),
    }));
    let options = ClassifierRequestOptions {
        api_key: Some("or-key".into()),
        ..ClassifierRequestOptions::default()
    };
    let result = classify::classify_with(&model, &context, &options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);

    let request = &server.requests()[0];
    assert!(
        !request.headers.contains_key("typesafe-danger-zone-images"),
        "OpenRouter does not take TypeSafe's opt-in header"
    );
    let body: Value = serde_json::from_slice(&request.body).expect("body json");
    assert!(body.get("assets").is_none());
    assert_eq!(
        body["state"],
        json!([
            {
                "type": "image_url",
                "image_url": {"url": "data:image/jpeg;base64,aW1n"}
            },
            {"text": "judge this"}
        ])
    );
}

#[tokio::test]
async fn clef_system_one_images_use_the_data_url_list() {
    let server = MockServer::start(vec![json_ok(
        r#"{"success":true,"result":{"state":"Completed","result":{"answers":{"approved":{"type":"noul","noul":0.6}},"usage":{"input_tokens":3,"output_tokens":1}}}}"#,
    )])
    .await;
    let model = classifier_contract_model(
        ClassifierApi::CloudflareWorkersAiSystemOne,
        "cloudflare-workers-ai",
        "@cf/cloudflare/clef",
        server.base_url(),
    );
    let context = ClassifierContext {
        state: json!({"text": "judge this"}),
        images: vec![
            ImageContent {
                data: "aW1n".into(),
                mime_type: "image/webp".into(),
            },
            ImageContent {
                data: "anBn".into(),
                mime_type: "image/jpeg".into(),
            },
        ],
        questions: bool_classifier_context(None).questions,
    };
    let options = ClassifierRequestOptions {
        api_key: Some("cf-key".into()),
        ..ClassifierRequestOptions::default()
    };
    let result = classify::classify_with(&model, &context, &options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    assert_eq!(
        result.answers.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.6 })
    );

    let request = &server.requests()[0];
    assert_eq!(request.path, "/run");
    let body: Value = serde_json::from_slice(&request.body).expect("body json");
    assert_eq!(
        body["input"]["images"],
        json!(["data:image/webp;base64,aW1n", "data:image/jpeg;base64,anBn"])
    );
    assert_eq!(body["input"]["state"], json!({"text": "judge this"}));
}

#[tokio::test]
async fn clef_rejects_more_than_four_images_before_any_request() {
    let model = classifier_contract_model(
        ClassifierApi::CloudflareWorkersAiSystemOne,
        "cloudflare-workers-ai",
        "@cf/cloudflare/clef",
        // Never contacted: the image gate runs before the URL is built.
        "https://example.invalid",
    );
    let context = ClassifierContext {
        state: json!({}),
        images: (0..5)
            .map(|_| ImageContent {
                data: "aW1n".into(),
                mime_type: "image/png".into(),
            })
            .collect(),
        questions: bool_classifier_context(None).questions,
    };
    let result = classify::classify_with(
        &model,
        &context,
        &ClassifierRequestOptions {
            api_key: Some("cf-key".into()),
            ..ClassifierRequestOptions::default()
        },
    )
    .await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.unwrap_or_default();
    assert!(
        message.contains("accepts at most 4 images"),
        "typed image-count error: {message}"
    );
}

#[tokio::test]
async fn images_request_options_resolve_env_keys_and_mark_estimate_costs() {
    let server = MockServer::start(vec![json_ok(
        r#"{"id":"resp-1","choices":[{"message":{"content":"done"}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
    )])
    .await;
    let model = image_contract_model(server.base_url());
    let options = ImagesRequestOptions {
        env: Some(BTreeMap::from([(
            "OPENROUTER_API_KEY".to_string(),
            "env-key".to_string(),
        )])),
        headers: BTreeMap::from([("x-caller".to_string(), "yes".to_string())]),
        metadata: Some(BTreeMap::from([("trace".to_string(), json!("t-1"))])),
        timeout_ms: Some(5_000),
        max_retries: Some(0),
        ..ImagesRequestOptions::default()
    };
    let result =
        images::generate_images_with(&model, &ImagesContext::from_prompt("draw a cat"), &options)
            .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(
        result.output,
        vec![ImagesContent::Text {
            text: "done".into()
        }]
    );
    let usage = result.usage.expect("usage parsed");
    assert_eq!(usage.cost.total_from_provider, Some(false));
    assert_eq!(usage.cost.total, usage.cost.input + usage.cost.output);

    let request = &server.requests()[0];
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer env-key")
    );
    assert_eq!(
        request.headers.get("x-caller").map(String::as_str),
        Some("yes")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("body json");
    assert_eq!(body["model"], "openrouter/image-gen");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["content"][0]["text"], "draw a cat");
}

#[tokio::test]
async fn openai_decisions_504_fails_after_one_request() {
    // Upstream `openai-decisions.ts:147-153`: Cloudflare answers 504 when an
    // oversized input exceeds its edge time limit, so a retry of the same
    // input would only spend the budget and hit the same gateway timeout.
    let server = MockServer::start(vec![
        MockResponseSpec::error(504),
        MockResponseSpec::error(504),
    ])
    .await;
    let model = classifier_contract_model(
        ClassifierApi::OpenAiDecisions,
        "openai",
        "gpt-decisions",
        server.base_url(),
    );
    let context = bool_classifier_context(None);
    let options = ClassifierRequestOptions {
        api_key: Some("test-key".into()),
        max_retries: Some(2),
        ..ClassifierRequestOptions::default()
    };
    let result = classify::classify_with(&model, &context, &options).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    let message = result.error_message.expect("error message");
    assert!(message.contains("HTTP 504"), "{message}");
    assert_eq!(
        server.requests().len(),
        1,
        "504 must not be retried for OpenAI Decisions"
    );
}
