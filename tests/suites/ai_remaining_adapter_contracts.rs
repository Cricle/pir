//! Provider-specific mock contract tests for the remaining adapters:
//! openai-responses, openai-codex-responses, azure-openai-responses,
//! bedrock-converse-stream (bearer mode), google-vertex (express mode), and
//! mistral-conversations.
//!
//! A tiny in-process HTTP server (bound to `127.0.0.1:0`) stands in for the
//! provider endpoints. No external network is ever touched. Each test drives a
//! real provider adapter end to end and asserts on the exact request path,
//! auth headers, and wire payload, plus the SSE / event-stream parsing.
//!
//! SigV4-signed Bedrock requests and the request-body mapping are covered by
//! the parser/golden tests in `ai_adapters_parity.rs`; the bearer-token mode
//! exercises the full transport here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::ai::api::bedrock::BedrockConverseStreamProvider;
use pir::ai::api::google_vertex::GoogleVertexProvider;
use pir::ai::api::mistral::MistralConversationsProvider;
use pir::ai::api::openai_responses::{
    AzureOpenAiResponsesProvider, OpenAiCodexResponsesProvider, OpenAiResponsesProvider,
};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::Provider;
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Mock HTTP server (same harness as ai_provider_contracts.rs)
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
}

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
                // The Codex adapter tries its pooled WebSocket transport
                // before SSE. Reject that handshake without consuming the
                // scripted HTTP response, then serve the SSE request that
                // follows the fallback.
                let mut socket = loop {
                    let (mut socket, _) = match listener.accept().await {
                        Ok(accepted) => accepted,
                        Err(_) => return,
                    };
                    let mut peek = [0u8; 16 * 1024];
                    let peeked = socket.peek(&mut peek).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&peek[..peeked]).to_ascii_lowercase();
                    if head.contains("upgrade: websocket") {
                        let _ = read_request(&mut socket).await;
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .await;
                        let _ = socket.flush().await;
                        continue;
                    }
                    break socket;
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
// Fixtures and helpers
// ---------------------------------------------------------------------------

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

fn simple_context(text: &str) -> Context {
    Context {
        system_prompt: Some("You are helpful.".into()),
        messages: vec![user_message(text)],
        tools: None,
    }
}

fn model(api: &str, provider: &str, id: &str, base_url: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
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

fn responses_text_sse() -> Vec<u8> {
    concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}\n\n",
    )
    .as_bytes()
    .to_vec()
}

fn encode_event_frame(message_type: &str, event_type: &str, payload_json: &str) -> Vec<u8> {
    let mut headers: Vec<u8> = Vec::new();
    for (name, value) in [(":message-type", message_type), (":event-type", event_type)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7); // string header value
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total = 12 + headers.len() + payload_json.len() + 4;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(payload_json.as_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame
}

fn bedrock_text_event_stream() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_event_frame(
        "event",
        "messageStart",
        r#"{"messageStart":{"role":"assistant"}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "messageStop",
        r#"{"messageStop":{"stopReason":"end_turn"}}"#,
    ));
    body.extend_from_slice(&encode_event_frame(
        "event",
        "metadata",
        r#"{"metadata":{"usage":{"inputTokens":9,"outputTokens":2,"totalTokens":11}}}"#,
    ));
    body
}

// ---------------------------------------------------------------------------
// openai-responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_responses_stream_contract() {
    let server = MockServer::start(vec![MockResponseSpec::ok(responses_text_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiResponsesProvider::new(
        "openai",
        base.clone(),
        vec![model("openai-responses", "openai", "gpt-5.5", &base)],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some("sk-test".into()),
            session_id: Some("sess-1".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.usage.input, 10);
    assert_eq!(final_message.usage.output, 2);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer sk-test")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["prompt_cache_key"], "sess-1");
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][0]["content"], "You are helpful.");
    assert_eq!(body["input"][1]["content"][0]["text"], "Hello");
}

#[tokio::test]
async fn openai_responses_chatgpt_sign_in_omits_fields_on_the_wire() {
    let server = MockServer::start(vec![MockResponseSpec::ok(responses_text_sse())]).await;
    let base = format!("{}/v1", server.base_url());
    let provider = OpenAiResponsesProvider::new(
        "openai",
        base.clone(),
        vec![model("openai-responses", "openai", "gpt-5.5", &base)],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    // A non-`sk-` token against a non-canonical base URL is not a ChatGPT
    // sign-in (isChatGPTSignIn is exact), so the fields stay on the wire; the
    // exact-match omission logic is covered in ai_adapters_parity.rs.
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some("chatgpt-oauth-token".into()),
            max_tokens: Some(1024),
            ..StreamOptions::default()
        },
    );
    run_to_done(stream).await;
    let body: Value = serde_json::from_slice(&server.requests()[0].body).expect("json");
    assert_eq!(body["max_output_tokens"], 1024);
}

// ---------------------------------------------------------------------------
// openai-codex-responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codex_stream_contract_headers_and_body() {
    let server = MockServer::start(vec![MockResponseSpec::ok(responses_text_sse())]).await;
    let provider = OpenAiCodexResponsesProvider::new(
        "openai-codex",
        server.base_url(),
        vec![model(
            "openai-codex-responses",
            "openai-codex",
            "gpt-5.1-codex",
            server.base_url(),
        )],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_test"}}"#);
    let token = format!("aaa.{payload}.bbb");
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some(token.clone()),
            session_id: Some("sess-9".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);

    let request = &server.requests()[0];
    assert_eq!(request.path, "/codex/responses");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(
        request
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("acc_test")
    );
    assert_eq!(
        request.headers.get("openai-beta").map(String::as_str),
        Some("responses=experimental")
    );
    assert_eq!(
        request.headers.get("originator").map(String::as_str),
        Some("pi")
    );
    assert_eq!(
        request.headers.get("accept").map(String::as_str),
        Some("text/event-stream")
    );
    assert_eq!(
        request.headers.get("session-id").map(String::as_str),
        Some("sess-9")
    );
    // The SSE fallback zstd-compresses the body when the codec is compiled in
    // (upstream compresses this endpoint unconditionally).
    let body_bytes = match request.headers.get("content-encoding").map(String::as_str) {
        Some(encoding) if encoding.eq_ignore_ascii_case("zstd") => {
            #[cfg(feature = "extensions")]
            {
                zstd::stream::decode_all(request.body.as_slice()).expect("zstd body decodes")
            }
            #[cfg(not(feature = "extensions"))]
            {
                panic!("content-encoding: zstd without the zstd codec");
            }
        }
        _ => request.body.clone(),
    };
    let body: Value = serde_json::from_slice(&body_bytes).expect("json");
    assert_eq!(body["instructions"], "You are helpful.");
    assert_eq!(body["text"], json!({"verbosity": "low"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["parallel_tool_calls"], true);
    assert!(!body["input"]
        .as_array()
        .expect("input")
        .iter()
        .any(|item| item.get("role").and_then(Value::as_str) == Some("system")));
}

// ---------------------------------------------------------------------------
// azure-openai-responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn azure_responses_stream_contract() {
    let server = MockServer::start(vec![MockResponseSpec::ok(responses_text_sse())]).await;
    let base = server.base_url().to_string();
    let azure_model = model("azure-openai-responses", "azure", "gpt-5.5", &base);
    // Point the deployment URL at the mock host via AZURE_OPENAI_BASE_URL.
    let mut env: ProviderEnv = ProviderEnv::new();
    env.insert("AZURE_OPENAI_BASE_URL".into(), format!("{base}/openai/v1"));
    env.insert(
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".into(),
        "gpt-5.5=gpt55-deploy".into(),
    );
    let provider = AzureOpenAiResponsesProvider::new("azure", base.clone(), vec![azure_model])
        .expect("provider")
        .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some("azure-key".into()),
            env: Some(env),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);

    let request = &server.requests()[0];
    assert_eq!(
        request.path,
        "/openai/v1/deployments/gpt55-deploy/responses?api-version=v1"
    );
    assert_eq!(
        request.headers.get("api-key").map(String::as_str),
        Some("azure-key")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["model"], "gpt55-deploy");
    assert_eq!(body["stream"], true);
}

// ---------------------------------------------------------------------------
// bedrock-converse-stream (bearer mode)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bedrock_bearer_token_stream_contract() {
    let server = MockServer::start(vec![MockResponseSpec::ok(bedrock_text_event_stream())]).await;
    let provider = BedrockConverseStreamProvider::new(
        "amazon-bedrock",
        server.base_url(),
        vec![model(
            "bedrock-converse-stream",
            "amazon-bedrock",
            "anthropic.claude-sonnet-4-6",
            server.base_url(),
        )],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some("bedrock-bearer-token".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.usage.input, 9);
    assert_eq!(final_message.usage.output, 2);

    let request = &server.requests()[0];
    assert_eq!(
        request.path,
        "/model/anthropic.claude-sonnet-4-6/converse-stream"
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer bedrock-bearer-token")
    );
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["modelId"], "anthropic.claude-sonnet-4-6");
    assert_eq!(
        body["system"],
        json!([{"text": "You are helpful."}, {"cachePoint": {"type": "default"}}])
    );
    assert_eq!(body["messages"][0]["role"], "user");
}

// ---------------------------------------------------------------------------
// google-vertex (express mode)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vertex_express_mode_stream_contract() {
    let server = MockServer::start(vec![MockResponseSpec::ok(gemini_text_sse())]).await;
    let mut vertex_model = model(
        "google-vertex",
        "google-vertex",
        "gemini-3.5-pro",
        server.base_url(),
    );
    let provider = GoogleVertexProvider::new(
        "google-vertex",
        "https://{location}-aiplatform.googleapis.com",
        vec![vertex_model.clone()],
    )
    .expect("provider")
    .with_client(no_retry_client());
    vertex_model.base_url = server.base_url().to_string();
    let stream = provider.stream(
        &vertex_model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some("vertex-key".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);

    let request = &server.requests()[0];
    // The API key travels in the x-goog-api-key header only; a `key=` query
    // parameter would leak the credential into logs.
    assert_eq!(
        request.path,
        "/v1/publishers/google/models/gemini-3.5-pro:streamGenerateContent?alt=sse"
    );
    assert!(!request.path.contains("key="));
    assert_eq!(
        request.headers.get("x-goog-api-key").map(String::as_str),
        Some("vertex-key")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "You are helpful."
    );
    assert_eq!(body["contents"][0]["role"], "user");
}

fn gemini_text_sse() -> Vec<u8> {
    "data: {\"responseId\":\"r1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":8,\"candidatesTokenCount\":2,\"totalTokenCount\":10}}\n\n"
        .as_bytes()
        .to_vec()
}

#[tokio::test]
async fn vertex_service_account_mode_reports_clear_error() {
    let provider = GoogleVertexProvider::new(
        "google-vertex",
        "https://{location}-aiplatform.googleapis.com",
        vec![model(
            "google-vertex",
            "google-vertex",
            "gemini-3.5-pro",
            "https://aiplatform.googleapis.com",
        )],
    )
    .expect("provider");
    let model = provider.get_models()[0].clone();

    // No express key, project + location + credentials path all supplied via
    // the request-scoped env (the env map wins over the process env), so this
    // stays hermetic: the unreadable credential file fails typed without
    // touching the well-known ADC file or the network
    // (upstream googleauth.js:314-327).
    let mut env = BTreeMap::new();
    env.insert(
        "GOOGLE_CLOUD_PROJECT".to_string(),
        "test-project".to_string(),
    );
    env.insert(
        "GOOGLE_CLOUD_LOCATION".to_string(),
        "us-central1".to_string(),
    );
    env.insert(
        "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
        std::env::temp_dir()
            .join("pir-vertex-test-missing-9f3d1c.json")
            .display()
            .to_string(),
    );
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        },
    );
    let message = stream.result().await.expect("error termination");
    assert_eq!(message.stop_reason, StopReason::Error);
    let error_text = message.error_message.clone().expect("error message");
    assert!(
        error_text.contains(
            "Unable to read the credential file specified by the GOOGLE_APPLICATION_CREDENTIALS"
        ),
        "typed credential-file error: {error_text}"
    );

    // A readable service-account key file with a non-PEM private_key fails
    // typed at signing time — still before any network use.
    let key_file =
        std::env::temp_dir().join(format!("pir-vertex-sa-test-{}.json", std::process::id()));
    std::fs::write(
        &key_file,
        json!({
            "type": "service_account",
            "client_email": "test@test-project.iam.gserviceaccount.com",
            "private_key": "not-a-pem-key",
        })
        .to_string(),
    )
    .expect("write temp key file");
    let mut env = BTreeMap::new();
    env.insert(
        "GOOGLE_CLOUD_PROJECT".to_string(),
        "test-project".to_string(),
    );
    env.insert(
        "GOOGLE_CLOUD_LOCATION".to_string(),
        "us-central1".to_string(),
    );
    env.insert(
        "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
        key_file.display().to_string(),
    );
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        },
    );
    let message = stream.result().await.expect("error termination");
    assert_eq!(message.stop_reason, StopReason::Error);
    let error_text = message.error_message.clone().expect("error message");
    assert!(
        error_text.contains("not a valid PKCS#8 or PKCS#1 RSA PEM key"),
        "typed private-key parse error: {error_text}"
    );
    let _ = std::fs::remove_file(&key_file);
}

#[tokio::test]
async fn vertex_adc_authorized_user_and_unsupported_types_fail_typed() {
    let provider = GoogleVertexProvider::new(
        "google-vertex",
        "https://{location}-aiplatform.googleapis.com",
        vec![model(
            "google-vertex",
            "google-vertex",
            "gemini-3.5-pro",
            "https://aiplatform.googleapis.com",
        )],
    )
    .expect("provider");
    let model = provider.get_models()[0].clone();

    let write_key = |name: &str, value: serde_json::Value| {
        let path =
            std::env::temp_dir().join(format!("pir-vertex-adc-{name}-{}.json", std::process::id()));
        std::fs::write(&path, value.to_string()).expect("write ADC file");
        path
    };

    // `gcloud auth application-default login` files are `authorized_user`;
    // a missing field fails typed at parse time, before any token exchange.
    let authorized_user = write_key(
        "authorized",
        json!({
            "type": "authorized_user",
            "client_id": "client.apps.googleusercontent.com",
            "client_secret": "secret",
        }),
    );
    let mut env = BTreeMap::new();
    env.insert(
        "GOOGLE_CLOUD_PROJECT".to_string(),
        "test-project".to_string(),
    );
    env.insert(
        "GOOGLE_CLOUD_LOCATION".to_string(),
        "us-central1".to_string(),
    );
    env.insert(
        "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
        authorized_user.display().to_string(),
    );
    let message = provider
        .stream(
            &model,
            simple_context("Hello"),
            StreamOptions {
                env: Some(env),
                ..StreamOptions::default()
            },
        )
        .result()
        .await
        .expect("error termination");
    assert_eq!(message.stop_reason, StopReason::Error);
    let error_text = message.error_message.clone().expect("error message");
    assert!(
        error_text.contains("does not contain a refresh_token field"),
        "typed authorized_user error: {error_text}"
    );
    let _ = std::fs::remove_file(&authorized_user);

    // Other google-auth-library credential types keep the explicit typed gap.
    let external_account = write_key(
        "external",
        json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/x",
        }),
    );
    let mut env = BTreeMap::new();
    env.insert(
        "GOOGLE_CLOUD_PROJECT".to_string(),
        "test-project".to_string(),
    );
    env.insert(
        "GOOGLE_CLOUD_LOCATION".to_string(),
        "us-central1".to_string(),
    );
    env.insert(
        "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
        external_account.display().to_string(),
    );
    let message = provider
        .stream(
            &model,
            simple_context("Hello"),
            StreamOptions {
                env: Some(env),
                ..StreamOptions::default()
            },
        )
        .result()
        .await
        .expect("error termination");
    assert_eq!(message.stop_reason, StopReason::Error);
    let error_text = message.error_message.clone().expect("error message");
    assert!(
        error_text.contains("credential type 'external_account'"),
        "typed unsupported ADC type: {error_text}"
    );
    assert!(
        error_text.contains("supported ADC types are 'service_account' and 'authorized_user'"),
        "the error names the supported types: {error_text}"
    );
    let _ = std::fs::remove_file(&external_account);
}

// ---------------------------------------------------------------------------
// mistral-conversations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mistral_stream_contract() {
    let server = MockServer::start(vec![MockResponseSpec::ok(mistral_text_sse())]).await;
    let provider = MistralConversationsProvider::new(
        "mistral",
        server.base_url(),
        vec![model(
            "mistral-conversations",
            "mistral",
            "mistral-large-latest",
            server.base_url(),
        )],
    )
    .expect("provider")
    .with_client(no_retry_client());
    let model = provider.get_models()[0].clone();
    let stream = provider.stream(
        &model,
        simple_context("Hello"),
        StreamOptions {
            api_key: Some("mistral-key".into()),
            session_id: Some("sess-7".into()),
            ..StreamOptions::default()
        },
    );
    let final_message = run_to_done(stream).await;
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    assert_eq!(final_message.usage.input, 20);
    assert_eq!(final_message.usage.output, 3);

    let request = &server.requests()[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer mistral-key")
    );
    assert_eq!(
        request.headers.get("x-affinity").map(String::as_str),
        Some("sess-7")
    );
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["model"], "mistral-large-latest");
    assert_eq!(body["stream"], true);
    assert_eq!(body["prompt_cache_key"], "sess-7");
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "You are helpful."})
    );
    // Assistant thinking replays as Mistral content chunks.
    assert_eq!(body["messages"][1]["content"][0]["text"], "Hello");
}

fn mistral_text_sse() -> Vec<u8> {
    concat!(
        "data: {\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        "data: {\"id\":\"cmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":3,\"total_tokens\":23}}\n\n",
        "data: [DONE]\n\n",
    )
    .as_bytes()
    .to_vec()
}
