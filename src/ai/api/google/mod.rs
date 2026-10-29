//! Google Generative AI (Gemini) adapter.
//!
//! Rust equivalent of the upstream
//! `packages/ai/src/api/google-generative-ai.ts` (+ its shared helpers in
//! `google-shared.ts`). Talks raw REST instead of the `@google/genai` SDK:
//! `POST {baseUrl}/models/{model}:streamGenerateContent?alt=sse` with the
//! `x-goog-api-key` header, SSE `data:` lines as responses.

use std::sync::Arc;

use serde_json::{json, Map, Value};

pub use shared::{
    clamp_google_thinking_level, convert_google_contents, disabled_google_thinking_config,
    gemini_major_version, google_thinking_budget, google_thinking_budget_with, is_thinking_part,
    map_stop_reason, requires_tool_call_id, resolve_google_thinking_level,
    resolve_thought_signature, retain_thought_signature, supports_google_strict_tool_sampling,
    uses_google_thinking_level, ResolvedGoogleLevel,
};
use shared::{convert_google_tools, google_thinking_config_with};
pub use sse::{parse_google_sse, GoogleSseParser};

pub mod shared;
pub mod sse;

use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;

/// Default Google Generative API base URL (upstream `providers/google.ts`).
pub const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The upstream api id for this adapter (upstream `google-generative-ai`).
pub const GOOGLE_API_ID: &str = "google-generative-ai";

/// Google Generative AI (Gemini) provider.
#[derive(Clone)]
pub struct GoogleGenerativeAiProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
}

impl GoogleGenerativeAiProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        let base_url = if base_url.trim().is_empty() {
            GOOGLE_BASE_URL.to_string()
        } else {
            base_url
        };
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Google".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            // Upstream auth uses GEMINI_API_KEY (providers/google.ts); the
            // GOOGLE_API_KEY alias is accepted as a fallback.
            api_key_env: vec!["GEMINI_API_KEY".into(), "GOOGLE_API_KEY".into()],
        })
    }

    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = client;
        self
    }

    /// Override the human-readable family name while retaining the same
    /// Google wire protocol.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    pub fn with_api_key_env(mut self, env: Vec<String>) -> Self {
        self.api_key_env = env;
        self
    }
}

impl Provider for GoogleGenerativeAiProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }
    fn name(&self) -> &str {
        &self.display_name
    }
    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            format!("{} API key", self.display_name),
            self.api_key_env.clone(),
        )]
    }
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_shared(model, Arc::new(context), options)
    }

    fn stream_shared(
        &self,
        model: &Model,
        context: Arc<Context>,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let output = AssistantMessageEventStream::new();
        // Producer handle, not a consumer clone: a consumer that abandons the
        // stream must be observable so this task cannot park forever.
        let events = output.producer();
        let mut client = self.client.clone();
        crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
        let model = model.clone();
        tokio::spawn(async move {
            let result = google_stream(&client, &model, &context, &options, &events).await;
            if events.is_abandoned() {
                // Normal abandon: nobody is reading, so report nothing.
                events.end(None);
                return;
            }
            crate::ai::api::finish_provider_stream(
                &events,
                result,
                &model,
                options.signal.as_ref(),
            );
        });
        output
    }
}

async fn google_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> Result<(), AiError> {
    let mut body = build_google_body(model, context, options)?;
    // Upstream `options.onPayload`: inspect (or replace, by mutating in place)
    // the request body before it is sent.
    if let Some(on_payload) = &options.hooks.on_payload {
        on_payload(&mut body);
    }
    // Upstream `createClient`: default UA first, then model headers, then
    // caller (request) headers last so they can override everything (the
    // api key default is inserted before the caller merge as well).
    let mut headers = ProviderHeaders::new();
    headers.insert(
        "user-agent".into(),
        format!("pir ({})", std::env::consts::OS),
    );
    headers.insert("content-type".into(), "application/json".into());
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    if let Some(key) = &options.api_key {
        headers.insert("x-goog-api-key".into(), key.clone());
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    // Incremental path: parse SSE as chunks arrive and push events into the
    // stream immediately (first-token latency is no longer bound to the full
    // generation). The parser state machine is shared with the batch path.
    let mut parser = GoogleSseParser::new(model)
        .with_provider_event_hook(options.hooks.on_provider_stream_event.clone());
    // `false`: the consumer is gone — skip the request whose events nobody can
    // read (the transport is never opened).
    if !sink
        .push_await(AssistantMessageEvent::Start {
            partial: parser.snapshot(),
        })
        .await
    {
        return Ok(());
    }
    let url = format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        model.base_url.trim_end_matches('/'),
        model.id
    );
    let (status, response_headers) = {
        let mut on_chunk = crate::ai::api::EventChunkSink::new(
            |chunk: &[u8], ready: &mut Vec<AssistantMessageEvent>| {
                // Every consumer handle is gone: release the HTTP response
                // instead of draining a body nobody reads (normal abandon).
                if sink.is_abandoned() {
                    return Err(AiError::Aborted);
                }
                parser.feed(chunk, ready)
            },
            sink,
        );
        client
            .post_json_streaming_with(
                &url,
                &headers,
                &body,
                options.signal.as_ref(),
                &options.hooks,
                &mut on_chunk,
            )
            .await?
    };
    // Upstream fires `onResponse` when the HTTP response arrives, before the
    // body is consumed (the same seam every other adapter exposes);
    // pir's transport only surfaces the status and headers once streaming
    // completes, so the hook runs there.
    if let Some(on_response) = &options.hooks.on_response {
        on_response(status, &response_headers);
    }
    let mut tail = Vec::new();
    parser.finish(&mut tail)?;
    for event in tail {
        if !sink.push_await(event).await {
            break;
        }
    }
    Ok(())
}

/// Build the `GenerateContentRequest` REST body (upstream `buildParams` via
/// the `@google/genai` SDK's REST serialization): `contents` plus top-level
/// `systemInstruction` / `tools` / `toolConfig` and a `generationConfig`
/// holding `temperature` / `maxOutputTokens` / `thinkingConfig`. Public for
/// tests/.
pub fn build_google_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<Value> {
    let (contents, folded_system) = convert_google_contents(model, context);
    let mut body = json!({ "contents": contents });
    // systemInstruction carries the leading prompt plus every folded
    // mid-conversation system update (Gemini has no mid-conversation system
    // role).
    // `&str` parts: `join` copies the prompt exactly once into the final
    // `text`; `system.clone()` here was a second full copy of a large prompt.
    let mut system_parts: Vec<&str> = Vec::new();
    if let Some(system) = &context.system_prompt {
        if !system.trim().is_empty() {
            system_parts.push(system);
        }
    }
    if let Some(folded) = &folded_system {
        system_parts.push(folded);
    }
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
    }
    let mut generation_config = Map::new();
    if let Some(temperature) = options.temperature {
        generation_config.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        generation_config.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if let Some(thinking) = google_thinking_config_with(
        model,
        options.reasoning,
        options.thinking_budgets.as_ref(),
        false,
    )? {
        generation_config.insert("thinkingConfig".into(), thinking);
    }
    if !generation_config.is_empty() {
        body["generationConfig"] = Value::Object(generation_config);
    }
    if let Some(tools) = &context.tools {
        if let Some((tools_value, tool_config)) =
            convert_google_tools(model, tools, options.tool_choice.as_ref())?
        {
            body["tools"] = tools_value;
            if let Some(tool_config) = tool_config {
                body["toolConfig"] = tool_config;
            }
        }
    }
    Ok(body)
}
