//! Tests for the image / classifier model port (upstream `images.ts`,
//! `images-api-registry.ts`, `ClassifierModelCatalog`, and the codemode
//! `models.generateImages()` / `models.classify()` namespace):
//!
//! 1. catalog: the embedded image/classifier entries deserialize into the
//!    typed models and stay out of the chat catalog;
//! 2. request assembly / response parsing: the pure wire-shape functions
//!    against the upstream shapes (`openrouter-images`, System One,
//!    OpenAI Decisions);
//! 3. HTTP: the adapters end-to-end against a local canned server;
//! 4. codemode: scripts calling `models.*` through the real sandbox
//!    (fixture pattern from tests/suites/codemode_builtin.rs; network-free
//!    deterministic paths — unknown model, missing key, image-input gate).
//!
//! Independent file: the `g_*.rs` / `tests/suites/` files belong to another
//! workstream and are untouched.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as JsonValue;

use pir::agent::agent_loop::AgentEvent;
use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::classify;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::images;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::catalog;
use pir::ai::types::{
    AssistantMessage, ClassifierAnswer, ClassifierApi, ClassifierBoolCriteria, ClassifierContext,
    ClassifierModel, ClassifierOptions, ClassifierQuestion, ClassifierStopReason, ContentBlock,
    ImageApi, ImageContent, ImageModel, ImagesContent, ImagesContext, ImagesOptions,
    ImagesStopReason, InputModality, Model, ModelCost, ModelCostRates, OutputModality, StopReason,
    TerminalReason, TextContent, ThinkingLevel, ToolCall,
};

type Responder = Arc<dyn Fn(usize, &pir::ai::types::Context) -> AssistantMessage + Send + Sync>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cost(input: f64, output: f64) -> ModelCost {
    ModelCost {
        rates: ModelCostRates {
            input,
            output,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        tiers: Vec::new(),
    }
}

fn image_model(base_url: &str, output: Vec<OutputModality>) -> ImageModel {
    ImageModel {
        id: "test/image-model".into(),
        name: "Test image model".into(),
        api: ImageApi::OpenRouterImages,
        provider: "openrouter".into(),
        base_url: base_url.into(),
        input: vec![InputModality::Text, InputModality::Image],
        cost: cost(1.0, 2.0),
        headers: None,
        output,
    }
}

fn classifier_model(base_url: &str, api: ClassifierApi) -> ClassifierModel {
    ClassifierModel {
        id: "test/classifier".into(),
        name: "Test classifier".into(),
        api,
        provider: "typesafe".into(),
        base_url: base_url.into(),
        input: vec![InputModality::Text],
        cost: cost(0.042, 0.0),
        headers: None,
        context_window: 32000,
    }
}

fn bool_question() -> (String, ClassifierQuestion) {
    (
        "approved".to_owned(),
        ClassifierQuestion::Bool {
            instructions: "Does this express approval?".into(),
            criteria: ClassifierBoolCriteria {
                true_meaning: "Approval".into(),
                false_meaning: "No approval".into(),
            },
        },
    )
}

fn context_with(questions: Vec<(String, ClassifierQuestion)>) -> ClassifierContext {
    ClassifierContext {
        state: serde_json::json!({ "text": "yes" }),
        images: Vec::new(),
        questions: questions.into_iter().collect(),
    }
}

/// Spawn a local HTTP server that answers every request with `body` and
/// returns its base URL. Records request bodies for assertions.
async fn spawn_json_server(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("local addr");
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&requests);
    tokio::spawn(async move {
        for _ in 0..8 {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read until the end of the headers, then the body per
            // content-length.
            let header_end = loop {
                if let Some(index) = find_subsequence(&buffer, b"\r\n\r\n") {
                    break index + 4;
                }
                match tokio::io::AsyncReadExt::read(&mut socket, &mut chunk).await {
                    Ok(0) | Err(_) => break buffer.len(),
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            };
            let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buffer.len() < header_end + content_length {
                match tokio::io::AsyncReadExt::read(&mut socket, &mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            if let Ok(request_body) = std::str::from_utf8(&buffer[header_end..]) {
                sink.lock().expect("requests").push(request_body.to_owned());
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;
        }
    });
    (format!("http://{address}"), requests)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// 1. Catalog entries
// ---------------------------------------------------------------------------

#[test]
fn openrouter_image_entries_deserialize() {
    let flux = catalog::load_image_model("openrouter", "black-forest-labs/flux.2-pro")
        .expect("catalog parse")
        .expect("flux.2-pro entry");
    assert_eq!(flux.api, ImageApi::OpenRouterImages);
    assert_eq!(flux.api.wire_name(), "openrouter-images");
    assert_eq!(flux.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(flux.output, vec![OutputModality::Image]);
    assert!(flux.accepts_image_input());

    let gemini = catalog::load_image_model("openrouter", "google/gemini-3-pro-image")
        .expect("catalog parse")
        .expect("gemini image entry");
    assert_eq!(
        gemini.output,
        vec![OutputModality::Text, OutputModality::Image]
    );
}

#[test]
fn classifier_entries_deserialize() {
    let jev = catalog::load_classifier_model("typesafe", "jev-latest")
        .expect("catalog parse")
        .expect("typesafe jev-latest");
    assert_eq!(jev.api, ClassifierApi::TypesafeSystemOne);
    assert_eq!(jev.api.wire_name(), "typesafe-system-one");
    assert_eq!(jev.context_window, 64000);
    assert_eq!(jev.base_url, "https://api.typesafe.ai/v1");

    let vercel_jev = catalog::load_classifier_model("vercel-ai-gateway", "typesafe-ai/jev")
        .expect("catalog parse")
        .expect("vercel jev");
    // Routes to the TypeSafe-compatible System One endpoint (upstream
    // AI_GATEWAY_TYPESAFE_BASE_URL), overriding the provider base URL.
    assert_eq!(
        vercel_jev.base_url,
        "https://ai-gateway.vercel.sh/typesafe/v1"
    );
    assert_eq!(vercel_jev.context_window, 32000);

    for (id, input_cost) in [("jev-1.13", 0.042), ("jev-1.13-free", 0.0)] {
        let model = catalog::load_classifier_model("opencode", id)
            .expect("catalog parse")
            .expect(id);
        assert_eq!(model.base_url, "https://opencode.ai/inference/v1");
        assert_eq!(model.context_window, 32000);
        assert!((model.cost.rates.input - input_cost).abs() < f64::EPSILON);
    }

    let luna = catalog::load_classifier_model("openai", "gpt-6-luna")
        .expect("catalog parse")
        .expect("gpt-6-luna");
    assert_eq!(luna.api, ClassifierApi::OpenAiDecisions);
    assert!(luna.accepts_image_input());
    assert_eq!(luna.context_window, 922000);

    let openrouter_jev = catalog::load_classifier_model("openrouter", "typesafe/jev-1.13")
        .expect("catalog parse")
        .expect("openrouter jev");
    assert_eq!(openrouter_jev.api, ClassifierApi::TypesafeSystemOne);
    assert_eq!(openrouter_jev.base_url, "https://openrouter.ai/api/v1");

    for id in [
        "@cf/cloudflare/clef",
        "@cf/cloudflare/clef-flash",
        "typesafe/jev",
    ] {
        let model = catalog::load_classifier_model("cloudflare-workers-ai", id)
            .expect("catalog parse")
            .unwrap_or_else(|| panic!("{id}"));
        assert_eq!(model.api, ClassifierApi::CloudflareWorkersAiSystemOne);
        assert!(model.base_url.contains("{CLOUDFLARE_ACCOUNT_ID}"));
    }
}

#[test]
fn non_chat_entries_stay_out_of_the_chat_catalog() {
    let provider = catalog::load_provider("openrouter")
        .expect("catalog parse")
        .expect("openrouter");
    let chat_ids: Vec<&str> = provider
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert!(
        !chat_ids.contains(&"black-forest-labs/flux.2-pro"),
        "image entries must not leak into the chat catalog"
    );
    assert!(
        !chat_ids.contains(&"typesafe/jev-1.13"),
        "classifier entries must not leak into the chat catalog"
    );
    assert!(chat_ids.contains(&"anthropic/claude-opus-5"));
}

#[test]
fn unknown_model_lookups_return_none() {
    assert!(catalog::load_image_model("openrouter", "missing")
        .expect("parse")
        .is_none());
    assert!(catalog::load_classifier_model("typesafe", "missing")
        .expect("parse")
        .is_none());
    assert!(catalog::load_image_model("no-such-provider", "x")
        .expect("parse")
        .is_none());
    // An id that only exists as a chat model is not an image model.
    assert!(
        catalog::load_image_model("openrouter", "anthropic/claude-opus-5")
            .expect("parse")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// 2. Wire shapes (pure)
// ---------------------------------------------------------------------------

#[test]
fn openrouter_images_request_assembly() {
    let model = image_model("https://openrouter.ai/api/v1/", vec![OutputModality::Image]);
    let context = ImagesContext {
        input: vec![
            ImagesContent::Text {
                text: "a red circle".into(),
            },
            ImagesContent::Image {
                data: "aGk=".into(),
                mime_type: "image/png".into(),
            },
        ],
    };
    let (url, headers, body) =
        images::build_openrouter_images_request(&model, &context, "secret").expect("request");
    assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer secret")
    );
    // Image-only output requests `modalities: ["image"]`.
    assert_eq!(body["modalities"], serde_json::json!(["image"]));
    assert_eq!(body["model"], "test/image-model");
    assert_eq!(body["stream"], false);
    let content = body["messages"][0]["content"].as_array().expect("content");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "a red circle");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,aGk=");

    // Text-capable output requests both modalities.
    let text_capable = image_model(
        "https://x/v1",
        vec![OutputModality::Text, OutputModality::Image],
    );
    let (_, _, body) =
        images::build_openrouter_images_request(&text_capable, &context, "k").expect("request");
    assert_eq!(body["modalities"], serde_json::json!(["image", "text"]));
}

#[test]
fn openrouter_images_response_parsing() {
    let body: JsonValue = serde_json::json!({
        "id": "resp-1",
        "choices": [{
            "message": {
                "content": "here is your image",
                "images": [
                    { "image_url": { "url": "data:image/png;base64,aGk=" } },
                    { "image_url": "data:image/jpeg;base64,aGkx" },
                    { "image_url": { "url": "https://cdn.example/x.png" } }
                ]
            }
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "prompt_tokens_details": { "cached_tokens": 4, "cache_write_tokens": 1 }
        }
    });
    let (output, response_id, usage) =
        images::parse_openrouter_images_response(&body).expect("parse");
    assert_eq!(response_id.as_deref(), Some("resp-1"));
    assert_eq!(
        output.len(),
        3,
        "text + two data-URL images; remote URLs skipped"
    );
    assert_eq!(
        output[0],
        ImagesContent::Text {
            text: "here is your image".into()
        }
    );
    assert_eq!(
        output[1],
        ImagesContent::Image {
            data: "aGk=".into(),
            mime_type: "image/png".into()
        }
    );
    assert_eq!(
        output[2],
        ImagesContent::Image {
            data: "aGkx".into(),
            mime_type: "image/jpeg".into()
        }
    );
    // Usage (upstream parseUsage, openrouter-images.ts:176-187):
    // cache_read = max(0, cached - cache_write) = 3;
    // input = max(0, prompt - cache_read - cache_write) = 10 - 3 - 1 = 6
    // (and total 11 = 6 + 2 + 3, as asserted below).
    let usage = usage.expect("usage");
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write
        ),
        (6, 2, 3, 1)
    );
    // total = input + output + cache_read + cache_write (upstream
    // openrouter-images.ts:187) = 6 + 2 + 3 + 1.
    assert_eq!(usage.total_tokens, 12);
}

#[test]
fn system_one_wire_request_maps_bool_to_noul() {
    let (id, question) = bool_question();
    let context = context_with(vec![
        (
            "sentiment".into(),
            ClassifierQuestion::Choice {
                instructions: "pick one".into(),
                criteria: BTreeMap::from([("yes".to_owned(), "affirmative".to_owned())]),
            },
        ),
        (id, question),
    ]);
    let wire = classify::system_one_wire_request(&context);
    assert_eq!(wire["state"], context.state);
    assert_eq!(wire["questions"]["approved"]["type"], "noul");
    assert_eq!(
        wire["questions"]["sentiment"]["type"], "choice",
        "only bool maps to noul"
    );
}

#[test]
fn system_one_answers_parse() {
    let (id, question) = bool_question();
    let context = context_with(vec![(id, question)]);
    let answers: JsonValue = serde_json::json!({
        "approved": { "type": "noul", "noul": 0.8 }
    });
    let parsed =
        classify::parse_system_one_answers("System One API", &answers, &context).expect("parse");
    assert_eq!(
        parsed.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.8 })
    );

    // Missing answer is a typed error (upstream "did not return an answer").
    let missing =
        classify::parse_system_one_answers("System One API", &serde_json::json!({}), &context);
    assert!(missing.is_err());

    // A wrong wire type is a typed error too.
    let wrong: JsonValue = serde_json::json!({ "approved": { "type": "choice", "choice": "x" } });
    assert!(classify::parse_system_one_answers("System One API", &wrong, &context).is_err());
}

#[test]
fn decisions_wire_shapes() {
    let (_, bool_q) = bool_question();
    let predicate = classify::decisions_wire_question("approved", &bool_q);
    // Predicates carry no criteria; the meanings append to the instructions.
    assert_eq!(predicate["type"], "predicate");
    assert!(predicate["instructions"]
        .as_str()
        .unwrap()
        .contains("True means: Approval"));
    assert!(predicate["instructions"]
        .as_str()
        .unwrap()
        .contains("False means: No approval"));

    let context = ClassifierContext {
        state: serde_json::json!({ "text": "yes" }),
        images: vec![pir::ai::types::ImageContent {
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        }],
        questions: [bool_question()].into_iter().collect(),
    };
    let input = classify::decisions_wire_input(&context).expect("input");
    let content = input[0]["content"].as_array().expect("content");
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[1]["image_url"], "data:image/png;base64,aW1hZ2U=");

    // Text-only state stays a plain JSON string (upstream).
    let text_only =
        classify::decisions_wire_input(&context_with(vec![bool_question()])).expect("input");
    assert!(text_only.is_string());

    let answers: JsonValue = serde_json::json!([
        { "type": "predicate", "name": "approved", "probability": 0.8 }
    ]);
    let parsed = classify::parse_decisions_answers(&answers, &context).expect("parse");
    assert_eq!(
        parsed.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.8 })
    );
}

// ---------------------------------------------------------------------------
// 3. HTTP end-to-end against a local canned server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openrouter_images_http_round_trip() {
    let (base_url, _requests) = spawn_json_server(
        r#"{"id":"resp-9","choices":[{"message":{"content":"done","images":[{"image_url":{"url":"data:image/png;base64,aGk="}}]}}],"usage":{"prompt_tokens":100,"completion_tokens":0}}"#,
    )
    .await;
    let model = image_model(&base_url, vec![OutputModality::Text, OutputModality::Image]);
    let result = images::generate_images(
        &model,
        &ImagesContext::from_prompt("a red circle"),
        &ImagesOptions {
            api_key: Some("test-key".into()),
            headers: BTreeMap::new(),
        },
        None,
    )
    .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(result.response_id.as_deref(), Some("resp-9"));
    assert_eq!(result.output.len(), 2);
    assert_eq!(
        result.output[1],
        ImagesContent::Image {
            data: "aGk=".into(),
            mime_type: "image/png".into()
        }
    );
    // Usage priced from the model's catalog rates (1.0 / 1e6 * 100).
    let usage = result.usage.expect("usage");
    assert_eq!(usage.input, 100);
    assert!((usage.cost.input - 0.0001).abs() < 1e-12);
}

#[tokio::test]
async fn system_one_http_round_trip() {
    let (base_url, _requests) = spawn_json_server(
        r#"{"answers":{"approved":{"type":"noul","noul":0.8}},"usage":{"input_tokens":5,"output_tokens":2}}"#,
    )
    .await;
    let model = classifier_model(&base_url, ClassifierApi::TypesafeSystemOne);
    let result = classify::classify(
        &model,
        &context_with(vec![bool_question()]),
        // Upstream requires an API key before the request
        // (classifier-shared.ts:68), so the round trip carries a test key.
        &ClassifierOptions {
            api_key: Some("test-key".into()),
            ..ClassifierOptions::default()
        },
        None,
    )
    .await;
    assert_eq!(
        result.stop_reason,
        ClassifierStopReason::Stop,
        "system_one round trip: {:?}",
        result.error_message
    );
    assert_eq!(
        result.answers.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.8 })
    );
    let usage = result.usage.expect("usage");
    assert_eq!((usage.input, usage.output), (5, 2));
    assert!((usage.cost.input - 0.042 / 1_000_000.0 * 5.0).abs() < 1e-12);
}

#[tokio::test]
async fn cloudflare_envelope_round_trip() {
    let (base_url, _requests) = spawn_json_server(
        r#"{"success":true,"result":{"state":"Completed","result":{"answers":{"approved":{"type":"noul","noul":0.55}}}}}"#,
    )
    .await;
    let model = classifier_model(&base_url, ClassifierApi::CloudflareWorkersAiSystemOne);
    let result = classify::classify(
        &model,
        &context_with(vec![bool_question()]),
        &ClassifierOptions {
            api_key: Some("test-key".into()),
            ..ClassifierOptions::default()
        },
        None,
    )
    .await;
    assert_eq!(
        result.stop_reason,
        ClassifierStopReason::Stop,
        "{:?}",
        result.error_message
    );
    assert_eq!(
        result.answers.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.55 })
    );
}

#[tokio::test]
async fn decisions_http_round_trip() {
    let (base_url, _requests) = spawn_json_server(
        r#"{"answers":[{"type":"predicate","name":"approved","probability":0.8}]}"#,
    )
    .await;
    let model = classifier_model(&base_url, ClassifierApi::OpenAiDecisions);
    let result = classify::classify(
        &model,
        &context_with(vec![bool_question()]),
        &ClassifierOptions {
            api_key: Some("test-key".into()),
            ..ClassifierOptions::default()
        },
        None,
    )
    .await;
    assert_eq!(
        result.stop_reason,
        ClassifierStopReason::Stop,
        "{:?}",
        result.error_message
    );
    assert_eq!(
        result.answers.get("approved"),
        Some(&ClassifierAnswer::Bool { probability: 0.8 })
    );
}

#[tokio::test]
async fn classifier_rejects_image_input_for_text_only_models() {
    // No server, no key: the input gate fires before any request.
    let model = classifier_model("https://127.0.0.1:1", ClassifierApi::TypesafeSystemOne);
    let context = ClassifierContext {
        state: serde_json::json!({}),
        images: vec![ImageContent {
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        }],
        questions: [bool_question()].into_iter().collect(),
    };
    let result = classify::classify(&model, &context, &ClassifierOptions::default(), None).await;
    assert_eq!(result.stop_reason, ClassifierStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Model typesafe/test/classifier does not accept image input")
    );
}

// ---------------------------------------------------------------------------
// 4. codemode `models` namespace end-to-end (fixture from
//    tests/suites/codemode_builtin.rs)
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
        cost: cost(0.0, 0.0),
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

fn text(text: &str) -> ContentBlock {
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

fn tool_call(id: &str, arguments: BTreeMap<String, JsonValue>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "codemode".into(),
        arguments,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
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
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: pir::ai::types::Context,
        _options: pir::ai::types::StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// Per-test temp dir (session storage + cwd).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-image-classifier-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Activates `codemode` through the session's live `setActiveTools` cell: the
/// tool is upstream `model-only` + `defaultActive: false`, and the models
/// bridges keep their codemode exposure (callable from the script without
/// being declared).
struct ActivateCodemode;

impl pir::plugins::AgentPlugin for ActivateCodemode {
    fn info(&self) -> &pir::plugins::PluginInfo {
        static INFO: std::sync::OnceLock<pir::plugins::PluginInfo> = std::sync::OnceLock::new();
        INFO.get_or_init(|| pir::plugins::PluginInfo::inline("activate-codemode"))
    }

    fn on_context(&self, ctx: &pir::plugins::plugin::PluginContext) {
        if let Some(session) = ctx.session() {
            session.set_active_tools(&[Arc::from("codemode")]);
        }
    }
}

/// Run one prompt whose first provider response calls `codemode` with the
/// given script, returning the codemode tool result message.
async fn run_codemode_script(code: &str) -> pir::ai::types::ToolResultMessage {
    let temp = TempDir::new("script");
    let mut arguments = BTreeMap::new();
    arguments.insert(
        "code".to_owned(),
        serde_json::Value::String(code.to_owned()),
    );
    let mut registry = Arc::new(ProviderRegistry::default());
    let registry_ref = Arc::get_mut(&mut registry).expect("fresh registry");
    registry_ref.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond: Arc::new(move |index, _context| {
            if index == 0 {
                assistant(
                    vec![tool_call("call-1", arguments.clone())],
                    StopReason::ToolUse,
                )
            } else {
                assistant(vec![text("done")], StopReason::Stop)
            }
        }),
    }));
    let tools = ToolRegistry::new();
    let agent = Arc::new(AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("test system prompt".into()),
            cwd: Some(temp.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![
            pir::plugins::codemode::new_codemode_plugin(),
            // `codemode` is upstream `model-only` + `defaultActive: false`;
            // the session's live override activates it before the first
            // loadout. The models bridges keep their codemode exposure and
            // stay callable from the script.
            Arc::new(ActivateCodemode),
        ],
    ));
    let log: Arc<Mutex<Vec<Arc<pir::agent::agent_loop::LoopResult>>>> = Arc::default();
    let sink = Arc::clone(&log);
    agent
        .prompt(
            "compose",
            Arc::new(move |event: AgentEvent| {
                if let AgentEvent::AgentEnd { result } = event {
                    sink.lock().expect("log").push(result);
                }
            }),
        )
        .await
        .expect("prompt");
    let result = log.lock().expect("log").pop().expect("agent end");
    Arc::unwrap_or_clone(result)
        .tool_results
        .into_iter()
        .find(|message| message.tool_name == "codemode")
        .expect("codemode tool result message")
}

fn text_of(message: &pir::ai::types::ToolResultMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn script_generate_images_unknown_model_fails_the_script() {
    let result = run_codemode_script(
        "return await models.generateImages('a red circle', { provider: 'openrouter', model: 'openai/nope' });",
    )
    .await;
    let content = text_of(&result);
    assert!(result.is_error, "content: {content}");
    assert!(
        content.contains("Unknown image model \"openrouter/openai/nope\""),
        "content: {content}"
    );
}

#[tokio::test]
async fn script_generate_images_missing_key_returns_error_result() {
    // An empty apiKey is treated as missing (upstream `if (!apiKey)`), so the
    // call returns the upstream error-result shape instead of a rejection.
    let result = run_codemode_script(
        "const r = await models.generateImages('a red circle', { provider: 'openrouter', model: 'black-forest-labs/flux.2-pro', apiKey: '' });\ntext(JSON.stringify(r));\nreturn r;",
    )
    .await;
    let content = text_of(&result);
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(
        content.contains("\"stopReason\":\"error\""),
        "content: {content}"
    );
    assert!(
        content.contains("No API key for provider: openrouter"),
        "content: {content}"
    );
    // The call surfaced as a nested row named like upstream.
    let nested = result.nested_calls.as_ref().expect("nested calls");
    assert!(
        nested
            .calls
            .iter()
            .any(|call| call.name == "models.generateImages"),
        "nested rows: {:?}",
        nested
            .calls
            .iter()
            .map(|call| &call.name)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn script_classify_missing_key_returns_error_result() {
    let result = run_codemode_script(
        "const r = await models.classify({ state: { text: 'yes' }, questions: { approved: { type: 'bool', instructions: 'Approval?', criteria: { true: 'Approval', false: 'No approval' } } } }, { provider: 'typesafe', model: 'jev-latest', apiKey: '' });\nreturn r;",
    )
    .await;
    let content = text_of(&result);
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(
        content.contains("\"stopReason\":\"error\""),
        "content: {content}"
    );
    assert!(
        content.contains("No API key for provider: typesafe"),
        "content: {content}"
    );
}

#[tokio::test]
async fn script_classify_rejects_images_for_text_only_model() {
    let result = run_codemode_script(
        "const r = await models.classify({ state: {}, images: [{ type: 'image', data: 'aW1hZ2U=', mimeType: 'image/png' }], questions: { approved: { type: 'bool', instructions: 'Approval?', criteria: { true: 'Approval', false: 'No approval' } } } }, { provider: 'typesafe', model: 'jev-latest', apiKey: '' });\nreturn r.errorMessage;",
    )
    .await;
    let content = text_of(&result);
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(
        content.contains("does not accept image input"),
        "content: {content}"
    );
}

#[tokio::test]
async fn script_generate_images_rejects_malformed_context() {
    let result = run_codemode_script(
        "try { await models.generateImages({ prompt: 'a red circle' }, { provider: 'openrouter', model: 'black-forest-labs/flux.2-pro' }); return 'no error'; } catch (error) { return error.message; }",
    )
    .await;
    let content = text_of(&result);
    assert!(
        content.starts_with("Script completed"),
        "content: {content}"
    );
    assert!(
        content.contains("models.generateImages() expects a prompt string, a blocks array"),
        "content: {content}"
    );
}
