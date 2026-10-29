//! Claude Code environment compatibility (env-only configuration):
//!
//! ```text
//! ANTHROPIC_BASE_URL=https://gateway.example
//! ANTHROPIC_AUTH_TOKEN=fake-token
//! ANTHROPIC_MODEL=gpt-5
//! ANTHROPIC_DEFAULT_OPUS_MODEL=gpt-5
//! ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5
//! ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5
//! CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000
//! CLAUDE_CODE_MAX_OUTPUT_TOKENS=65536
//! ```
//!
//! The test is hermetic: a local TCP mock plays the gateway, the agent dir is
//! a fresh temp directory (no models.json for the env-only case), and every
//! token is a fake string. Environment variables are process-global, so all
//! cases run sequentially inside one test.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use pir::ai::providers::resolution::resolve_cli_model;
use pir::ai::types::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Minimal Anthropic SSE response: start, one stop reason, stop.
const SUCCESS_SSE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_env_test\",\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const ENV_NAMES: &[&str] = &[
    "PIR_AGENT_DIR",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
];

/// Restore every relevant environment variable when the test ends (including
/// on panic), so a developer/CI environment with real Anthropic variables
/// cannot leak into the assertions and the test leaves no trace.
struct EnvScope {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvScope {
    fn capture() -> Self {
        Self {
            saved: ENV_NAMES
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }

    /// Clear every Claude Code variable before one case sets exactly what it
    /// needs.
    fn clear(&self) {
        for name in ENV_NAMES {
            std::env::remove_var(name);
        }
    }

    fn set(&self, name: &str, value: &str) {
        std::env::set_var(name, value);
    }

    fn remove(&self, name: &str) {
        std::env::remove_var(name);
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A captured mock HTTP request.
#[derive(Clone)]
struct CapturedRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

struct MockServer {
    base_url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local addr");
        let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    if let Some(request) = read_request(&mut socket).await {
                        recorded
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push(request);
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        SUCCESS_SSE.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(SUCCESS_SSE.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn read_request(socket: &mut TcpStream) -> Option<CapturedRequest> {
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
            let mut headers: BTreeMap<String, String> = BTreeMap::new();
            let mut content_length = 0usize;
            for line in head.lines().skip(1) {
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
                let read = socket.read(&mut tmp).await.ok()?;
                if read == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..read]);
            }
            let end = (body_start + content_length).min(buf.len());
            let body = serde_json::from_slice(&buf[body_start..end]).ok()?;
            return Some(CapturedRequest { headers, body });
        }
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-claude-env-{}-{label}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp agent dir");
    dir
}

fn write_models_json(agent_dir: &Path, contents: &str) {
    std::fs::write(agent_dir.join("models.json"), contents).expect("write models.json");
}

fn user_context(text: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        tools: None,
    }
}

fn find_model<'a>(environment: &'a pir::ai::providers::PiEnvironment, id: &str) -> &'a Model {
    environment
        .models
        .iter()
        .find(|model| model.provider == "anthropic" && model.id == id)
        .unwrap_or_else(|| {
            panic!(
                "env model {id} missing from --list-models; catalog has: {:?}",
                environment
                    .models
                    .iter()
                    .map(|model| format!("{}/{}", model.provider, model.id))
                    .collect::<Vec<_>>()
            )
        })
}

#[tokio::test]
async fn claude_code_env_compat_end_to_end() {
    let scope = EnvScope::capture();

    // -----------------------------------------------------------------
    // 1. Env-only configuration: bearer auth + synthesized model.
    // -----------------------------------------------------------------
    let env_agent_dir = temp_dir("env-only");
    scope.clear();
    let server = MockServer::start().await;
    scope.set("PIR_AGENT_DIR", env_agent_dir.to_str().expect("utf-8 path"));
    scope.set("ANTHROPIC_BASE_URL", &server.base_url);
    scope.set("ANTHROPIC_AUTH_TOKEN", "fake-token");
    scope.set("ANTHROPIC_MODEL", "gpt-5");
    scope.set("ANTHROPIC_DEFAULT_OPUS_MODEL", "gpt-5");
    scope.set("ANTHROPIC_DEFAULT_SONNET_MODEL", "openai-sonnet");
    scope.set("ANTHROPIC_DEFAULT_HAIKU_MODEL", "gpt-5");
    scope.set("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "1000000");
    scope.set("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "65536");

    let environment =
        pir::ai::providers::load_pi_environment(Some(&env_agent_dir)).expect("env environment");
    let model = find_model(&environment, "gpt-5");
    assert_eq!(model.base_url, server.base_url);
    assert!(model.reasoning);
    assert_eq!(model.input, vec!["text".to_string(), "image".to_string()]);
    assert_eq!(model.context_window, 1_000_000);
    assert_eq!(model.max_tokens, 65_536);
    // Deduplicated aliases still resolve through --model/`/model`.
    assert!(environment
        .models
        .iter()
        .any(|model| model.provider == "anthropic" && model.id == "openai-sonnet"));
    assert!(environment
        .registry
        .get_model("anthropic", "gpt-5")
        .is_some());
    let (resolved, _, warning) =
        resolve_cli_model(Some("anthropic"), "gpt-5", &environment.models)
            .expect("cli model");
    assert_eq!(
        resolved.map(|model| model.id),
        Some("gpt-5".to_string())
    );
    assert!(warning.is_none());
    // The env primary model fills the default when no explicit default exists.
    assert_eq!(
        environment.settings.default_provider.as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        environment.settings.default_model.as_deref(),
        Some("gpt-5")
    );

    // The print/TUI path streams through `stream_unchecked`; the adapter
    // itself must resolve the bearer token from the environment.
    let stream = environment.registry.stream_unchecked(
        model.clone(),
        user_context("hi"),
        StreamOptions::default(),
    );
    let message = stream.result().await.expect("stream terminated");
    assert_eq!(message.stop_reason, StopReason::Stop);

    let captured = server.captured();
    assert_eq!(captured.len(), 1, "exactly one request");
    let request = &captured[0];
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer fake-token")
    );
    assert!(
        !request.headers.contains_key("x-api-key"),
        "ANTHROPIC_AUTH_TOKEN must not be sent as x-api-key: {:?}",
        request.headers
    );
    assert_eq!(request.body["model"], json!("gpt-5"));
    assert_eq!(request.body["max_tokens"], json!(65_536));
    assert_eq!(request.body["stream"], json!(true));

    std::fs::remove_dir_all(&env_agent_dir).ok();

    // -----------------------------------------------------------------
    // 2. Explicit models.json apiKey/baseUrl/model wins over the env, and
    //    the env-defined model id is not duplicated.
    // -----------------------------------------------------------------
    let explicit_agent_dir = temp_dir("explicit");
    let explicit_server = MockServer::start().await;
    write_models_json(
        &explicit_agent_dir,
        &format!(
            r#"{{
  "providers": {{
    "anthropic": {{
      "baseUrl": "{}",
      "apiKey": "sk-explicit",
      "models": [
        {{
          "id": "gpt-5",
          "name": "Explicit OpenAI",
          "reasoning": true,
          "input": ["text", "image"],
          "contextWindow": 424242,
          "maxTokens": 12345
        }}
      ]
    }}
  }}
}}"#,
            explicit_server.base_url
        ),
    );
    scope.clear();
    scope.set(
        "PIR_AGENT_DIR",
        explicit_agent_dir.to_str().expect("utf-8 path"),
    );
    // These must all lose to the explicit models.json values.
    scope.set("ANTHROPIC_BASE_URL", "http://env.invalid");
    scope.set("ANTHROPIC_AUTH_TOKEN", "fake-env-token");
    scope.set("ANTHROPIC_MODEL", "gpt-5");
    scope.set("ANTHROPIC_DEFAULT_OPUS_MODEL", "gpt-5");
    scope.set("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "1000000");
    scope.set("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "65536");

    let environment =
        pir::ai::providers::load_pi_environment(Some(&explicit_agent_dir)).expect("explicit env");
    assert_eq!(
        environment.api_keys.get("anthropic").map(String::as_str),
        Some("sk-explicit")
    );
    let occurrences = environment
        .models
        .iter()
        .filter(|model| model.provider == "anthropic" && model.id == "gpt-5")
        .count();
    assert_eq!(occurrences, 1, "models.json model must not be duplicated");
    let model = find_model(&environment, "gpt-5");
    assert_eq!(model.name, "Explicit OpenAI");
    assert_eq!(model.base_url, explicit_server.base_url);
    assert_eq!(model.context_window, 424_242);
    assert_eq!(model.max_tokens, 12_345);

    let api_key = environment.api_keys.get("anthropic").cloned();
    let print_options = StreamOptions {
        api_key: api_key.clone(),
        ..StreamOptions::default()
    };
    let stream = environment.registry.stream_unchecked(
        model.clone(),
        user_context("hi"),
        print_options.clone(),
    );
    let message = stream.result().await.expect("stream terminated");
    assert_eq!(message.stop_reason, StopReason::Stop);

    // The registry auth path (`ProviderRegistry::stream`) also resolves the
    // bearer env token; the explicit request key must suppress that header.
    let stream =
        environment
            .registry
            .stream(model.clone(), user_context("hi again"), print_options);
    let message = stream.result().await.expect("stream terminated");
    assert_eq!(message.stop_reason, StopReason::Stop);

    let captured = explicit_server.captured();
    assert_eq!(captured.len(), 2, "one request per streaming path");
    for request in &captured {
        assert_eq!(
            request.headers.get("x-api-key").map(String::as_str),
            Some("sk-explicit"),
            "explicit models.json apiKey must win: {:?}",
            request.headers
        );
        assert!(
            !request.headers.contains_key("authorization"),
            "explicit apiKey must not be combined with the env bearer token: {:?}",
            request.headers
        );
        assert_eq!(request.body["model"], json!("gpt-5"));
        assert_eq!(request.body["max_tokens"], json!(12_345));
    }

    std::fs::remove_dir_all(&explicit_agent_dir).ok();

    // -----------------------------------------------------------------
    // 3. No token and no apiKey: a clear auth error before any request.
    // -----------------------------------------------------------------
    let no_auth_agent_dir = temp_dir("no-auth");
    let no_auth_server = MockServer::start().await;
    scope.clear();
    scope.set(
        "PIR_AGENT_DIR",
        no_auth_agent_dir.to_str().expect("utf-8 path"),
    );
    scope.set("ANTHROPIC_BASE_URL", &no_auth_server.base_url);
    scope.set("ANTHROPIC_MODEL", "gpt-5");
    scope.remove("ANTHROPIC_AUTH_TOKEN");
    scope.remove("ANTHROPIC_OAUTH_TOKEN");
    scope.remove("ANTHROPIC_API_KEY");

    let environment =
        pir::ai::providers::load_pi_environment(Some(&no_auth_agent_dir)).expect("no-auth env");
    let model = find_model(&environment, "gpt-5").clone();
    assert!(
        !pir::ai::providers::has_configured_auth("anthropic", &environment.api_keys),
        "no credential must be reported as unconfigured"
    );
    let stream =
        environment
            .registry
            .stream_unchecked(model, user_context("hi"), StreamOptions::default());
    let message = stream.result().await.expect("stream terminated");
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.as_deref().unwrap_or_default();
    assert!(
        error.contains("authentication") && error.contains("ANTHROPIC_AUTH_TOKEN"),
        "clear auth error expected, got: {error}"
    );
    assert!(
        no_auth_server.captured().is_empty(),
        "an unauthenticated request must not reach the gateway"
    );

    std::fs::remove_dir_all(&no_auth_agent_dir).ok();
}
