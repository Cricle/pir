//! DRIFT W1 D30 (upstream `34a8bb38b` / #10794): Anthropic rejects error tool
//! results whose content is not all text, so the adapter replaces every image
//! block of an error tool result with the upstream note
//! `[image omitted: error tool results cannot contain images]` before content
//! conversion. Successful tool results keep their image blocks.
//!
//! Most tests assert the request shape via `build_anthropic_request` (the
//! provider's exact payload); the `*_on_the_wire` test streams through
//! `AnthropicMessagesProvider` against a local mock endpoint and asserts the
//! captured HTTP body.
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::ai::api::anthropic::{build_anthropic_request, AnthropicMessagesProvider};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::Provider;
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The upstream replacement note, verbatim (upstream `convertToolResult`,
/// `34a8bb38b`). Spelled out here so the test pins the wire byte string
/// independently of the adapter's own constant.
const UPSTREAM_NOTE: &str = "(image attached after the tool results)";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn anthropic_model(input: &[&str]) -> Model {
    Model {
        id: "claude-sonnet-4-5".into(),
        name: "Test Claude".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "https://example.invalid".into(),
        reasoning: false,
        thinking_level_map: None,
        input: input.iter().map(|value| value.to_string()).collect(),
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

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn image_block() -> ContentBlock {
    ContentBlock::Image(ImageContent {
        data: "aGk=".into(),
        mime_type: "image/png".into(),
    })
}

fn user_message(text: &str, timestamp: u64) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp,
        sections: None,
    })
}

fn assistant_tool_call(call_id: &str, tool_name: &str) -> Message {
    Message::Assistant(AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: call_id.into(),
            name: tool_name.into(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    })
}

fn tool_result_message(
    call_id: &str,
    tool_name: &str,
    content: Vec<ContentBlock>,
    is_error: bool,
) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: tool_name.into(),
        content,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error,
        duration_ms: None,
        timestamp: 3,
    })
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: None,
        messages,
        tools: None,
    }
}

fn options() -> StreamOptions {
    StreamOptions {
        api_key: Some("test-key".into()),
        ..StreamOptions::default()
    }
}

/// Every content block of every message in a request payload.
fn message_blocks(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// Every `tool_result` block in a request payload, in transcript order.
fn tool_result_blocks(body: &Value) -> Vec<Value> {
    message_blocks(body)
        .into_iter()
        .filter(|block| block["type"] == "tool_result")
        .collect()
}

// ---------------------------------------------------------------------------
// Mock HTTP endpoint (captures the provider's request body)
// ---------------------------------------------------------------------------

/// The minimal success fixture: message_start / message_delta / message_stop.
const SUCCESS_SSE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

struct MockServer {
    base_url: String,
    payloads: Arc<Mutex<Vec<Value>>>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let payloads: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = payloads.clone();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            if let Some(body) = read_request_body(&mut socket).await {
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    recorded
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(value);
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                SUCCESS_SSE.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(SUCCESS_SSE.as_bytes()).await;
            let _ = socket.flush().await;
        });
        Self {
            base_url: format!("http://{addr}"),
            payloads,
        }
    }

    /// The single captured request body.
    fn payload(&self) -> Value {
        let payloads = self
            .payloads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(payloads.len(), 1, "exactly one captured request");
        payloads[0].clone()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn read_request_body(socket: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let read = socket.read(&mut tmp).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..read]);
        if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let mut content_length = 0usize;
            for line in head.lines().skip(1) {
                if let Some((key, value)) = line.split_once(':') {
                    if key.trim().eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let body_start = header_end + 4;
            while buf.len().saturating_sub(body_start) < content_length {
                let read = socket.read(&mut tmp).await.ok()?;
                if read == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..read]);
            }
            let end = (body_start + content_length).min(buf.len());
            return Some(buf[body_start..end].to_vec());
        }
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

async fn run(
    provider: &AnthropicMessagesProvider,
    model: &Model,
    context: Context,
) -> Arc<AssistantMessage> {
    let stream: AssistantMessageEventStream = provider.stream(model, context, options());
    stream.result().await.expect("stream terminated")
}

// ---------------------------------------------------------------------------
// D30 — error tool results drop their images (upstream 34a8bb38b)
// ---------------------------------------------------------------------------

/// The failing-before case: an error tool result that carries an image used to
/// go out as a content-block array containing the image, which Anthropic
/// rejects with a 400.
#[tokio::test]
async fn d30_error_tool_result_image_is_note_on_the_wire() {
    let server = MockServer::start().await;
    let provider = AnthropicMessagesProvider::new(
        "anthropic",
        server.base_url.clone(),
        vec![anthropic_model(&["text", "image"])],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let message = run(
        &provider,
        &model,
        context(vec![
            user_message("take a screenshot", 1),
            assistant_tool_call("toolu_img", "screenshot"),
            tool_result_message("toolu_img", "screenshot", vec![image_block()], true),
        ]),
    )
    .await;
    assert_eq!(message.stop_reason, StopReason::Stop);

    let body = server.payload();
    let results = tool_result_blocks(&body);
    assert_eq!(results.len(), 1, "one tool result on the wire: {body}");
    // Upstream 5ab7fe74c: the image leaves the (rejected) error result and is
    // re-attached after all tool results, captioned with the call it came from.
    assert_eq!(results[0]["content"], json!(UPSTREAM_NOTE), "body: {body}");
    assert_eq!(results[0]["is_error"], json!(true), "body: {body}");
    let blocks = message_blocks(&body);
    let result_index = blocks
        .iter()
        .position(|block| block["type"] == "tool_result")
        .expect("tool result present");
    assert_eq!(
        blocks[result_index + 1],
        json!({
            "type": "text",
            "text": "Images from the error result of tool call toolu_img (screenshot):",
        }),
        "body: {body}"
    );
    assert_eq!(blocks[result_index + 2]["type"], "image", "body: {body}");
    assert_eq!(
        blocks
            .iter()
            .filter(|block| block["type"] == "image")
            .count(),
        1,
        "the image moved, not vanished: {body}"
    );
}

/// Consecutive images collapse to one placeholder per run; the error result's
/// content collapses to the joined string and both images move after it.
#[test]
fn d30_error_tool_result_images_are_replaced_in_place() {
    let model = anthropic_model(&["text", "image"]);
    let context = context(vec![
        user_message("go", 1),
        assistant_tool_call("toolu_1", "screenshot"),
        tool_result_message(
            "toolu_1",
            "screenshot",
            vec![image_block(), text_block("boom"), image_block()],
            true,
        ),
    ]);
    let (body, _) = build_anthropic_request(&model, &context, &options()).expect("request");
    let results = tool_result_blocks(&body);
    assert_eq!(results.len(), 1, "one tool result: {body}");
    assert_eq!(
        results[0]["content"],
        json!(format!("{UPSTREAM_NOTE}\nboom\n{UPSTREAM_NOTE}")),
        "body: {body}"
    );
    assert_eq!(results[0]["is_error"], json!(true));
    let blocks = message_blocks(&body);
    let result_index = blocks
        .iter()
        .position(|block| block["type"] == "tool_result")
        .expect("tool result present");
    assert_eq!(
        blocks[result_index + 1],
        json!({
            "type": "text",
            "text": "Images from the error result of tool call toolu_1 (screenshot):",
        }),
        "body: {body}"
    );
    assert_eq!(
        blocks[result_index + 2..]
            .iter()
            .filter(|block| block["type"] == "image")
            .count(),
        2,
        "both images follow the tool result: {body}"
    );
}

/// An error result without images is untouched (no note).
#[test]
fn d30_error_tool_result_without_images_is_unchanged() {
    let model = anthropic_model(&["text", "image"]);
    let context = context(vec![
        user_message("go", 1),
        assistant_tool_call("toolu_1", "screenshot"),
        tool_result_message("toolu_1", "screenshot", vec![text_block("boom")], true),
    ]);
    let (body, _) = build_anthropic_request(&model, &context, &options()).expect("request");
    let results = tool_result_blocks(&body);
    assert_eq!(results[0]["content"], json!("boom"), "body: {body}");
    assert_eq!(results[0]["is_error"], json!(true));
    assert!(
        !body.to_string().contains("image omitted"),
        "no note for an image-free error result: {body}"
    );
}

/// A successful tool result keeps its image blocks and the `(see attached
/// image)` placeholder (upstream `convertContentBlocks`), and carries no
/// `is_error` marker.
#[test]
fn d30_success_tool_result_with_image_keeps_image_blocks() {
    let model = anthropic_model(&["text", "image"]);
    let context = context(vec![
        user_message("go", 1),
        assistant_tool_call("toolu_1", "screenshot"),
        tool_result_message("toolu_1", "screenshot", vec![image_block()], false),
    ]);
    let (body, _) = build_anthropic_request(&model, &context, &options()).expect("request");
    let results = tool_result_blocks(&body);
    assert_eq!(results.len(), 1, "one tool result: {body}");
    let blocks = results[0]["content"]
        .as_array()
        .expect("successful image results stay a content-block array");
    assert_eq!(
        blocks[0],
        json!({"type": "text", "text": "(see attached image)"}),
        "blocks: {blocks:?}"
    );
    assert_eq!(
        blocks[1],
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "aGk="},
        }),
        "blocks: {blocks:?}"
    );
    assert_eq!(blocks.len(), 2, "blocks: {blocks:?}");
    assert!(
        !results[0].as_object().unwrap().contains_key("is_error"),
        "successful results carry no is_error: {body}"
    );
    assert!(
        !body.to_string().contains("image omitted"),
        "successful results get no note: {body}"
    );
}

/// A non-vision model never reaches the note: `transformMessages` downgrades
/// tool-result images to the non-vision placeholder first (so the error path
/// sees no image). Both texts are kept distinct on purpose.
#[test]
fn d30_error_tool_result_image_on_non_vision_model_uses_transform_placeholder() {
    let model = anthropic_model(&["text"]);
    let context = context(vec![
        user_message("go", 1),
        assistant_tool_call("toolu_1", "screenshot"),
        tool_result_message("toolu_1", "screenshot", vec![image_block()], true),
    ]);
    let (body, _) = build_anthropic_request(&model, &context, &options()).expect("request");
    let results = tool_result_blocks(&body);
    assert_eq!(
        results[0]["content"],
        json!("(tool image omitted: model does not support images)"),
        "body: {body}"
    );
    assert!(
        !body.to_string().contains(UPSTREAM_NOTE),
        "the D30 note only applies to images that reach the adapter: {body}"
    );
}
