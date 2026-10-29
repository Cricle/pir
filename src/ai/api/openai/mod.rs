//! OpenAI-compatible chat/completions adapter.

use serde::ser::{SerializeMap, SerializeSeq};
use serde_json::{json, Value};

use std::sync::Arc;

use crate::ai::api::constrained_sampling::{
    create_grammar_tool_input_properties, get_json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;

mod compat_detect;
mod reasoning;
mod sse;

pub use compat_detect::{detect_compat, resolve_compat};
use reasoning::apply_assistant_reasoning;
pub use reasoning::{
    append_openai_reasoning_detail, apply_reasoning_effort, apply_thinking_budget,
    clamp_thinking_budget_to_answer_room, is_openai_reasoning_detail,
    parse_openai_reasoning_details, resolve_sampling_params, MIN_ANSWER_TOKENS,
};
// The thinking-budget and cache-retention resolvers are shared with other
// adapters (bedrock, the Responses codec), so they live in the core
// (`crate::ai::api`) and keep compiling when `provider-openai` is off; the
// re-exports preserve the `openai::…` paths of the enabled build.
use crate::ai::api::resolve_cache_retention;
pub use crate::ai::api::{default_thinking_budget, thinking_budget_for_level};
pub use sse::{parse_openai_sse, OpenAiSseParser};

/// OpenAI-compatible `/chat/completions` provider.
#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
    pub api_key_header: String,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "OpenAI-compatible".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            api_key_env: vec!["OPENAI_API_KEY".into()],
            api_key_header: "authorization".into(),
        })
    }

    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = client;
        self
    }
    pub fn with_api_key_env(mut self, env: Vec<String>) -> Self {
        self.api_key_env = env;
        self
    }

    /// Override the human-readable family name while retaining the same
    /// OpenAI-compatible wire protocol.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }
}

impl Provider for OpenAiCompatibleProvider {
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
        let api_key_header = self.api_key_header.clone();
        tokio::spawn(async move {
            let result = openai_stream(
                &client,
                &model,
                &context,
                &options,
                &api_key_header,
                &events,
            )
            .await;
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

/// Compose the default request headers (upstream `createClient`): the pir
/// user agent first (upstream 87af49dec), then model headers, then
/// session-affinity headers, then the credential, then caller headers last so
/// they can override everything. Public for tests/.
pub fn openai_request_headers(
    model: &Model,
    options: &StreamOptions,
    compat: &OpenAiCompat,
    api_key_header: &str,
) -> ProviderHeaders {
    openai_request_headers_with_dynamic(model, options, compat, api_key_header, None)
}

/// [`openai_request_headers`] with the adapter-computed dynamic headers
/// (GitHub Copilot initiator/intent/vision), merged over model headers and
/// under session-affinity, credential, and caller headers (upstream
/// `createClient` order).
pub fn openai_request_headers_with_dynamic(
    model: &Model,
    options: &StreamOptions,
    compat: &OpenAiCompat,
    api_key_header: &str,
    dynamic_headers: Option<&ProviderHeaders>,
) -> ProviderHeaders {
    let mut headers = ProviderHeaders::new();
    headers
        .entry("user-agent".to_string())
        .or_insert_with(|| format!("pir ({})", std::env::consts::OS));
    for (name, value) in model.headers.iter().flatten() {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    for (name, value) in dynamic_headers.into_iter().flatten() {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    // Session-affinity headers (upstream createClient:776-786), keyed off the
    // request `session_id` and the compat knobs.
    if let Some(session_id) = options.session_id.as_deref() {
        if compat.send_session_affinity_headers {
            match compat.session_affinity_format.unwrap_or_default() {
                SessionAffinityFormat::Openrouter => {
                    headers
                        .entry("x-session-id".to_string())
                        .or_insert_with(|| session_id.to_string());
                }
                SessionAffinityFormat::Openai => {
                    headers.insert("session_id".to_string(), session_id.to_string());
                    headers.insert("x-client-request-id".to_string(), session_id.to_string());
                    headers.insert("x-session-affinity".to_string(), session_id.to_string());
                }
                SessionAffinityFormat::OpenaiNosession => {
                    headers.insert("x-client-request-id".to_string(), session_id.to_string());
                    headers.insert("x-session-affinity".to_string(), session_id.to_string());
                }
            }
        }
    }
    if let Some(key) = &options.api_key {
        if api_key_header.eq_ignore_ascii_case("api-key") {
            headers.insert(api_key_header.to_ascii_lowercase(), key.clone());
        } else {
            headers.insert(api_key_header.to_ascii_lowercase(), format!("Bearer {key}"));
        }
    }
    for (name, value) in options.headers.iter().flatten() {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    headers
}

/// The `cache_control` marker for the anthropic cache-control format (upstream
/// `getCompatCacheControl`): `None` retention disables markers; `long` adds
/// `ttl: "1h"` when the provider supports long retention.
fn compat_cache_control(compat: &OpenAiCompat, cache_retention: CacheRetention) -> Option<Value> {
    if compat.cache_control_format != Some(CacheControlFormat::Anthropic)
        || cache_retention == CacheRetention::None
    {
        return None;
    }
    let long_ttl = cache_retention == CacheRetention::Long
        && compat.supports_long_cache_retention.unwrap_or(true);
    Some(if long_ttl {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    })
}

async fn openai_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    api_key_header: &str,
    sink: &AssistantMessageEventStream,
) -> Result<(), AiError> {
    let compat = resolve_compat(model);
    // Payload mutations keep the value-based body: `on_payload` receives a
    // `Value` to inspect/replace, and the anthropic-style cache-control
    // markers are attached by mutating message objects. Without either, the
    // streamed body serializes byte-identically straight from the transcript
    // (no transcript-sized `Value` tree).
    let cache_control = compat_cache_control(&compat, resolve_cache_retention(options));
    let streamed = options.hooks.on_payload.is_none() && cache_control.is_none();
    let mut value_body = if streamed {
        None
    } else {
        let mut body = build_openai_body(model, context, options, &compat)?;
        // Upstream `options.onPayload`: inspect or replace the request payload
        // before it is sent (mutating in place models both).
        if let Some(on_payload) = &options.hooks.on_payload {
            on_payload(&mut body);
        }
        Some(body)
    };
    let streamed_body = if streamed {
        Some(build_openai_body_streamed(
            model, context, options, &compat,
        )?)
    } else {
        None
    };
    // Upstream `createClient` default headers: UA, then model headers, then
    // session-affinity; caller headers merge last so they can override.
    let copilot_dynamic_headers = if crate::ai::api::copilot_headers::is_github_copilot_model(model)
    {
        Some(
            crate::ai::api::copilot_headers::build_copilot_dynamic_headers(
                &context.messages,
                crate::ai::api::copilot_headers::has_copilot_vision_input(&context.messages),
            ),
        )
    } else {
        None
    };
    let mut headers = openai_request_headers_with_dynamic(
        model,
        options,
        &compat,
        api_key_header,
        copilot_dynamic_headers.as_ref(),
    );
    headers
        .entry("content-type".to_string())
        .or_insert_with(|| "application/json".to_string());
    // Incremental path: parse SSE as chunks arrive and push events into the
    // stream immediately (first-token latency is no longer bound to the full
    // generation). The parser state machine is shared with the batch path.
    let mut parser = OpenAiSseParser::new(model)
        .with_stream_hook(options.hooks.on_provider_stream_event.clone());
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
        let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
        match (&streamed_body, value_body.as_mut()) {
            (Some(body), _) => {
                client
                    .post_json_streaming_with(
                        &url,
                        &headers,
                        body,
                        options.signal.as_ref(),
                        &options.hooks,
                        &mut on_chunk,
                    )
                    .await?
            }
            (None, Some(body)) => {
                client
                    .post_json_streaming_with(
                        &url,
                        &headers,
                        body,
                        options.signal.as_ref(),
                        &options.hooks,
                        &mut on_chunk,
                    )
                    .await?
            }
            (None, None) => unreachable!("one of the two body shapes is always built"),
        }
    };
    // Upstream fires `onResponse` right after the HTTP response arrives,
    // before the body is consumed; pir's transport only surfaces the status
    // and headers once streaming completes, so the hook runs there.
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

/// The `/chat/completions` body with a `messages` PLACEHOLDER: every other
/// field is built exactly like [`build_openai_body`] does, in the same
/// insertion order (the placeholder keeps the key's position under
/// serde_json's `preserve_order`), so the value-based and the streamed body
/// assemblers can never drift apart.
struct OpenAiBodyParts {
    body: Value,
    /// Whether the body carries a non-empty top-level `tools` array (the
    /// cache-control marker only needs the fact, not a second copy of the
    /// converted definitions — the array itself lives in `body`).
    has_tools: bool,
    /// The anthropic-style `cache_control` marker to attach, when the compat
    /// and the retention ask for one.
    cache_control: Option<Value>,
}

fn build_openai_body_parts(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &OpenAiCompat,
) -> AiResult<OpenAiBodyParts> {
    let cache_retention = resolve_cache_retention(options);
    let cache_control = compat_cache_control(compat, cache_retention);
    let mut body = json!({"model": model.id, "messages": Value::Null, "stream": true});
    // Prompt-cache session key (upstream buildParams:826-831): standard
    // OpenAI keys every cacheable request on the session; other providers
    // only with explicit long retention.
    let use_prompt_cache_key = (model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long
            && compat.supports_long_cache_retention.unwrap_or(true));
    if use_prompt_cache_key {
        if let Some(key) = options
            .session_id
            .as_deref()
            .and_then(clamp_prompt_cache_key)
        {
            body["prompt_cache_key"] = json!(key);
        }
    }
    if cache_retention == CacheRetention::Long
        && compat.supports_long_cache_retention.unwrap_or(true)
    {
        body["prompt_cache_retention"] = json!("24h");
    }
    if compat.supports_usage_in_streaming {
        body["stream_options"] = json!({"include_usage": true});
    }
    if compat.supports_store.unwrap_or(false) {
        body["store"] = json!(false);
    }
    if let Some(value) = options.temperature {
        body["temperature"] = json!(value);
    }
    // Response token cap (upstream `buildParams`: `options.maxTokens &&
    // (compat.maxTokensField === "max_tokens" ? params.max_tokens :
    // params.max_completion_tokens)`). Upstream's `streamSimple`/`buildBaseOptions`
    // fills `options.maxTokens` with the effective `options?.maxTokens ??
    // model.maxTokens` before `buildParams`, so the model default is sent when
    // the caller did not override it; a zero (JS-falsy) value sends nothing.
    let max_tokens = options
        .max_tokens
        .or((model.max_tokens > 0).then_some(model.max_tokens));
    if let Some(value) = max_tokens {
        // Upstream defaults to `max_completion_tokens` for standard OpenAI and
        // most compatibles (deprecated `max_tokens` is the exception path,
        // selected per compat).
        let field = compat
            .max_tokens_field
            .as_deref()
            .unwrap_or("max_completion_tokens");
        body[field] = json!(value);
    }
    let mut converted_tools: Option<Vec<Value>> = None;
    match context.tools.as_ref().filter(|tools| !tools.is_empty()) {
        Some(tools) => {
            converted_tools = Some(
                tools
                    .iter()
                    .map(|tool| convert_tool(tool, compat))
                    .collect::<AiResult<Vec<_>>>()?,
            );
            if compat.zai_tool_stream.unwrap_or(false) {
                body["tool_stream"] = json!(true);
            }
        }
        None => {
            // Anthropic (via LiteLLM/proxy) requires the tools param when the
            // conversation has tool_calls/tool_results even though no tool is
            // declared now; with no tool history at all the field is omitted
            // entirely (several gateways reject an empty `tools: []`).
            if has_tool_history(&context.messages) {
                converted_tools = Some(Vec::new());
            }
        }
    }
    // WHY (move): the converted definitions were cloned into `body` while the
    // original vector stayed alive only to answer one `is_empty` check in
    // `build_openai_body` — a second full copy of every tool JSON schema per
    // request. Move the vector into the body and carry the boolean.
    let has_tools = converted_tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty());
    if let Some(tools) = converted_tools {
        body["tools"] = Value::Array(tools);
    }
    if let Some(tool_choice) = options.tool_choice.as_ref() {
        body["tool_choice"] = tool_choice.openai_chat_json();
    }
    if let Some(priority) = compat.vllm_priority {
        body["priority"] = json!(priority);
    }
    // OpenRouter provider routing preferences (upstream reads the RAW
    // model.compat for these, so only explicit entries send them).
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.open_router_routing.as_ref())
        .filter(|routing| routing.is_object())
    {
        body["provider"] = routing.clone();
    }
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.vercel_gateway_routing.as_ref())
    {
        let mut gateway_options = serde_json::Map::new();
        if let Some(only) = routing.get("only").filter(|value| value.is_array()) {
            gateway_options.insert("only".to_string(), only.clone());
        }
        if let Some(order) = routing.get("order").filter(|value| value.is_array()) {
            gateway_options.insert("order".to_string(), order.clone());
        }
        if !gateway_options.is_empty() {
            body["providerOptions"] = json!({ "gateway": gateway_options });
        }
    }
    apply_reasoning_effort(&mut body, model, options.reasoning);
    apply_thinking_budget(
        &mut body,
        model,
        options.reasoning,
        options.thinking_budgets.as_ref(),
    );
    // Last, so model and request sampling parameters override named request
    // fields (upstream `resolveSamplingParams` merge chain:
    // model.samplingParams -> samplingParamsByThinkingLevel[level] -> request).
    if let Some(sampling_params) =
        resolve_sampling_params(model, options.reasoning, options.sampling_params.as_ref())
    {
        for (name, value) in sampling_params {
            body[name] = value;
        }
    }
    Ok(OpenAiBodyParts {
        body,
        has_tools,
        cache_control,
    })
}

/// Build the `/chat/completions` request body (upstream `buildParams`):
/// messages, `stream_options` gated on the compat knob, `prompt_cache_key` /
/// `prompt_cache_retention`, the response token cap (`max_completion_tokens`,
/// or `max_tokens` per compat, defaulting to the model's `maxTokens`), tools
/// with strict support, thinking emission, and the sampling-parameters merge
/// chain last. Public for tests/.
pub fn build_openai_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &OpenAiCompat,
) -> AiResult<Value> {
    let parts = build_openai_body_parts(model, context, options, compat)?;
    let mut body = parts.body;
    body["messages"] = Value::Array(convert_messages(model, context, compat));
    // Anthropic-style cache_control markers (upstream
    // `applyAnthropicCacheControl`): the system prompt, the last tool
    // definition, and the last conversation message get breakpoints.
    if let Some(cache_control) = parts.cache_control {
        apply_anthropic_cache_control(&mut body, parts.has_tools, &cache_control);
    }
    Ok(body)
}

/// Clamp a prompt-cache session key to the 64-char OpenAI limit (upstream
/// `clampOpenAIPromptCacheKey`).
pub(crate) fn clamp_prompt_cache_key(key: &str) -> Option<String> {
    let clamped: String = key.chars().take(64).collect();
    (!clamped.is_empty()).then_some(clamped)
}

/// Whether the transcript contains tool calls or tool results (upstream
/// `hasToolHistory`).
pub(crate) fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_))),
        _ => false,
    })
}

/// The `/chat/completions` request body with its `messages` array serialized
/// **straight from the typed transcript**: byte-for-byte the JSON
/// [`build_openai_body`] produces, but without materializing the
/// transcript-sized `Value` tree first (one item at a time is converted and
/// dropped while `serde_json::to_vec` writes it).
///
/// Memory-lane shape (2026-10-16): a 100 MB session's `Value` tree of
/// converted messages measured hundreds of MB (the tree plus the serialized
/// bytes plus the adapter's replay copy); this path holds one message's tree
/// plus the output buffer.
///
/// Not usable when the payload must be MUTATED before sending: an
/// `options.hooks.on_payload` handler inspects/replaces a `Value`, and the
/// anthropic-style cache-control markers are attached by mutating message
/// objects — both callers keep the [`build_openai_body`] path (`openai_stream`
/// picks between them).
pub struct OpenAiStreamedBody<'a> {
    body: Value,
    model: &'a Model,
    context: &'a Context,
    compat: &'a OpenAiCompat,
}

impl OpenAiStreamedBody<'_> {
    /// The completed body's non-message fields (tests asserting byte equality
    /// against [`build_openai_body`] may want the same overview).
    pub fn body(&self) -> &Value {
        &self.body
    }
}

/// Build the streamed twin of [`build_openai_body`] (see
/// [`OpenAiStreamedBody`]). The non-message fields are built by the SAME code
/// path ([`build_openai_body_parts`]), in the same insertion order, so the two
/// bodies serialize to identical bytes.
pub fn build_openai_body_streamed<'a>(
    model: &'a Model,
    context: &'a Context,
    options: &StreamOptions,
    compat: &'a OpenAiCompat,
) -> AiResult<OpenAiStreamedBody<'a>> {
    let parts = build_openai_body_parts(model, context, options, compat)?;
    Ok(OpenAiStreamedBody {
        body: parts.body,
        model,
        context,
        compat,
    })
}

impl serde::Serialize for OpenAiStreamedBody<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let plan = ReplayPlan::prepare(self.model, self.context, self.compat, false);
        // The body is always a JSON object (`build_openai_body_parts` starts
        // from an object literal). Degrade to an empty object instead of
        // panicking if that invariant ever breaks: a malformed request is a
        // provider error, never a crash (the hygiene gate forbids panicking
        // accessors in production code).
        let Some(map) = self.body.as_object() else {
            return serializer.serialize_map(Some(0))?.end();
        };
        let mut body = serializer.serialize_map(Some(map.len()))?;
        for (key, value) in map {
            if key == "messages" {
                // The placeholder is replaced IN PLACE, so the key order (and
                // therefore the bytes) matches the value-based body.
                body.serialize_entry(
                    key,
                    &StreamedMessages {
                        model: self.model,
                        plan: &plan,
                        compat: self.compat,
                    },
                )?;
            } else {
                body.serialize_entry(key, value)?;
            }
        }
        body.end()
    }
}

/// The `messages` array of [`OpenAiStreamedBody`], converted one message at a
/// time. The per-message conversion is [`OpenAiMessageConverter`] — the same
/// state machine [`convert_messages`] drives — so the sequence (including the
/// `requiresAssistantAfterToolResult` insertions and the skipped messages) is
/// identical to the materialized array.
struct StreamedMessages<'a> {
    model: &'a Model,
    plan: &'a ReplayPlan<'a>,
    compat: &'a OpenAiCompat,
}

impl serde::Serialize for StreamedMessages<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let transformed = self.plan.transformed(self.model);
        let mut converter = OpenAiMessageConverter::new(self.model, self.plan, self.compat);
        let mut sequence = serializer.serialize_seq(None)?;
        // One message's items at a time — never a transcript-sized item list.
        // The list is per message (instead of one reused Vec) because a lazy
        // text item borrows THAT message's text; the list itself is tiny, and
        // what used to dominate (a `Value` per message, its text copied into
        // it) is gone.
        {
            let mut items: Vec<WireItem<'_>> = Vec::new();
            converter.emit_leading_system(&self.plan.leading_system, &mut items);
            for item in &items {
                sequence.serialize_element(item)?;
            }
        }
        for message in &transformed {
            let mut items: Vec<WireItem<'_>> = Vec::new();
            converter.convert(message, &mut items);
            for item in &items {
                sequence.serialize_element(item)?;
            }
        }
        sequence.end()
    }
}

/// Apply Anthropic-style cache_control markers (upstream
/// `applyAnthropicCacheControl`): the first system/developer message, the last
/// tool definition, and the last user/assistant/tool message's last text
/// content get the marker.
fn apply_anthropic_cache_control(body: &mut Value, has_tools: bool, marker: &Value) {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        // First instruction message (system or developer).
        if let Some(message) = messages.iter_mut().find(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system") | Some("developer")
            )
        }) {
            add_cache_control_to_text_content(message, marker);
        }
        // Last conversation message (user / assistant / tool).
        if let Some(message) = messages.iter_mut().rev().find(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("user") | Some("assistant") | Some("tool")
            )
        }) {
            add_cache_control_to_text_content(message, marker);
        }
    }
    if has_tools {
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            if let Some(slot) = tools.last_mut() {
                slot["cache_control"] = marker.clone();
            }
        }
    }
}

/// Attach the marker to a message's text content (upstream
/// `addCacheControlToTextContent`): string content becomes a text-parts array;
/// array content marks the last text part. Returns whether a marker landed.
fn add_cache_control_to_text_content(message: &mut Value, marker: &Value) -> bool {
    let Some(message_object) = message.as_object_mut() else {
        return false;
    };
    match message_object.get_mut("content") {
        Some(Value::String(text)) => {
            if text.is_empty() {
                return false;
            }
            let text = text.clone();
            message_object.insert(
                "content".to_string(),
                json!([{ "type": "text", "text": text, "cache_control": marker }]),
            );
            true
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part["cache_control"] = marker.clone();
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    crate::ai::api::joined_text_blocks(blocks, "")
}

/// Convert one tool definition (upstream `convertTools`): grammar-configured
/// tools convert to OpenAI custom tools when the provider supports the format;
/// function tools carry the strict marker only when the provider supports
/// strict mode (default `false` — OpenAI compatibility alone does not imply
/// strict JSON-schema tool support). A "prefer" tool whose schema contains
/// keywords strict mode rejects falls back to non-strict. Public for tests/.
pub fn convert_tool(tool: &Tool, compat: &OpenAiCompat) -> AiResult<Value> {
    if let Some(grammar) = resolve_grammar_constrained_sampling(
        tool,
        compat.supports_openai_grammar_tools.unwrap_or(false),
    )? {
        return Ok(json!({
            "type": "custom",
            "custom": {
                "name": tool.name,
                "description": tool.description,
                "format": {
                    "type": "grammar",
                    "grammar": {
                        "syntax": grammar.format,
                        "definition": grammar.definition,
                    },
                },
            },
        }));
    }
    let supports_strict_mode = compat.supports_strict_mode.unwrap_or(false);
    let strict =
        resolve_json_schema_strict_sampling(tool, supports_strict_mode, None)?.unwrap_or(false);
    let parameters = get_json_schema_tool_parameters(tool, strict)?;
    let mut function = json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": parameters,
    });
    // Only include strict when the provider supports it; some reject unknown
    // fields (upstream: `...(compat.supportsStrictMode !== false && { strict: strict ?? false })`).
    if supports_strict_mode {
        function["strict"] = json!(strict);
    }
    Ok(json!({"type": "function", "function": function}))
}

/// Normalize a tool call id for replay into Chat Completions (upstream
/// `normalizeToolCallId` in `convertMessages`): Responses-pipe ids
/// (`{call_id}|{item_id}`) are sanitized, combined for item-level uniqueness,
/// and short-hashed when over the 40-char OpenAI limit; plain ids are only
/// truncated on the standard OpenAI provider.
fn normalize_openai_tool_call_id(model: &Model, id: &str) -> String {
    if let Some((call_id, item_id)) = id.split_once('|') {
        let sanitize = crate::ai::api::transform::sanitize_id_part;
        let call_id = sanitize(call_id);
        let item_id = sanitize(item_id);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if combined.chars().count() <= 40 {
            return combined;
        }
        let hash: String = crate::ai::api::transform::short_hash(id)
            .chars()
            .take(8)
            .collect();
        let keep = (40usize).saturating_sub(hash.chars().count() + 1).max(1);
        let prefix: String = call_id.chars().take(keep).collect();
        return format!("{prefix}_{hash}");
    }
    if model.provider == "openai" {
        return id.chars().take(40).collect();
    }
    id.to_string()
}

/// Build the `messages` array for a `/chat/completions` request (upstream
/// `convertMessages`). [`transform_messages`](crate::ai::api::transform::
/// transform_messages) runs first (pipe-id normalization, errored/aborted
/// assistant skip, synthetic `No result provided` tool results, cross-model
/// thinking→text lowering, image downgrade), then converted payloads carry
/// `image_url` content parts for user images and tool-result images when the
/// model accepts image input. Empty text parts are omitted from payloads; user
/// messages with no remaining content, and assistant messages with neither
/// content nor tool calls, are skipped entirely. Public for tests/.
pub fn convert_messages(model: &Model, context: &Context, compat: &OpenAiCompat) -> Vec<Value> {
    convert_messages_inner(model, context, compat, false)
}

/// [`convert_messages`] with the replay list FORCED through the former
/// clone-every-message shape. `#[doc(hidden)]` test seam (like the parsers'
/// `copy_count()`): the borrow-clone audit uses it as the A/B reference leg to
/// pin that the borrowed replay path is byte-identical to the cloned one and
/// that it skips the transcript-sized allocation (see
/// `tests/backend_copy_audit.rs`). Not part of the public contract.
#[doc(hidden)]
pub fn convert_messages_with_cloned_replay(
    model: &Model,
    context: &Context,
    compat: &OpenAiCompat,
) -> Vec<Value> {
    convert_messages_inner(model, context, compat, true)
}

fn convert_messages_inner(
    model: &Model,
    context: &Context,
    compat: &OpenAiCompat,
    force_cloned_replay: bool,
) -> Vec<Value> {
    let plan = ReplayPlan::prepare(model, context, compat, force_cloned_replay);
    let transformed = plan.transformed(model);
    let mut converter = OpenAiMessageConverter::new(model, &plan, compat);
    // The VALUE path materializes every item (its callers inspect or mutate the
    // body as `Value`s, e.g. `on_payload`), so the lazy text items are turned
    // into the identical objects here — the lazy shape only saves copies on the
    // serializing path.
    let mut messages: Vec<Value> = Vec::with_capacity(transformed.len() + 1);
    {
        let mut wire: Vec<WireItem<'_>> = Vec::new();
        converter.emit_leading_system(&plan.leading_system, &mut wire);
        messages.extend(wire.into_iter().map(WireItem::into_value));
    }
    for message in &transformed {
        let mut wire: Vec<WireItem<'_>> = Vec::new();
        converter.convert(message, &mut wire);
        messages.extend(wire.into_iter().map(WireItem::into_value));
    }
    messages
}

/// The replay shape of one request: the transcript as it enters the adapter
/// (borrowed when possible), the leading system prompt with any mid-convo
/// system text folded in, and the per-request conversion flags.
struct ReplayPlan<'a> {
    /// The transcript as replayed: borrowed from the context, or the former
    /// clone-every-message shape for the mid-convo collapse and the
    /// `#[doc(hidden)]` audit seam.
    replay: ReplayInput<'a>,
    /// The folded leading system prompt (`""` when there is none). Borrowed
    /// from the context unless a mid-convo system message was folded in.
    leading_system: std::borrow::Cow<'a, str>,
    /// Instruction-role selection (upstream `instructionRole`): reasoning
    /// models on developer-role providers use `developer`.
    instruction_role: &'static str,
    /// Grammar-tool input property per tool name (grammar tools replay their
    /// input through the custom tool-call shape).
    grammar_input_properties: std::collections::BTreeMap<String, String>,
    supports_images: bool,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content: bool,
    /// Whether later system messages carry their own `toolsAdded` as in-place
    /// additions instead of the top-level tool list changing (upstream
    /// `resolveTranscriptTools(...).anchorsAdditions`).
    anchors_tool_additions: bool,
    /// Names already declared by the top-level request field
    /// (`context.tools`). Upstream takes the request tools from the leading
    /// system message and later system messages only carry NEW definitions;
    /// pir keeps `context.tools` top-level, so an inline `toolsAdded` entry
    /// with one of these names is already declared and must not be repeated.
    request_tool_names: std::collections::BTreeSet<String>,
}

/// How the plan's transcript is carried. The borrowed form is the live one;
/// the filtered form carries the mid-convo collapse by reference, and the
/// owned form survives only for `convert_messages_with_cloned_replay` (the
/// `tests/backend_copy_audit.rs` A/B reference leg).
enum ReplayInput<'a> {
    Borrowed(&'a [Message]),
    /// The collapse kept messages by REFERENCE (one pointer per kept message)
    /// rather than cloning every transcript message into an owned list.
    Filtered(Vec<&'a Message>),
    /// Owned clone shape, kept only as the `#[doc(hidden)]` A/B reference leg
    /// (`convert_messages_with_cloned_replay`) for the allocation audit test.
    Owned(Vec<Message>),
}

impl<'a> ReplayPlan<'a> {
    fn prepare(
        model: &Model,
        context: &'a Context,
        compat: &OpenAiCompat,
        force_cloned_replay: bool,
    ) -> Self {
        // Upstream resolveTranscript/collapseSystemMessages (transcript.ts:99-112):
        // when the API cannot take mid-conversation system messages
        // (`supportsMidConvoSystemMessages`, default false), all mid-transcript
        // system/developer text is replayed into the leading system prompt and the
        // messages are dropped from the transcript.
        let supports_mid_convo_system_messages =
            compat.supports_mid_convo_system_messages.unwrap_or(false);
        // COW: the leading prompt is copied only when a mid-convo system
        // message is actually folded into it.
        let mut leading_system: std::borrow::Cow<'a, str> =
            std::borrow::Cow::Borrowed(context.system_prompt.as_deref().unwrap_or(""));
        let has_mid_convo =
            !supports_mid_convo_system_messages && context.messages.iter().any(is_system_message);
        // INVARIANT (borrowed replay): the mid-convo collapse is the IDENTITY on a
        // transcript with no system/developer message — the only branch that needs
        // a filtered list is the collapse itself (it drops those messages and
        // folds their text into `leading_system`). The kept messages are
        // referenced, never cloned: `transform_messages_lazy_refs` only rebuilds
        // the messages that actually change, so a transcript-sized request no
        // longer pays a second copy of every tool-result file content. The
        // borrow is a plain read of `context.messages`; nothing mutates `context`
        // in between.
        let replay = if !force_cloned_replay && !has_mid_convo {
            ReplayInput::Borrowed(context.messages.as_slice())
        } else {
            let mut collapsed = String::new();
            let mut kept: Vec<&'a Message> = Vec::with_capacity(context.messages.len());
            for message in &context.messages {
                match message {
                    Message::System(system) | Message::Developer(system) => {
                        let text = crate::ai::types::system_message_text(system);
                        if !text.trim().is_empty() {
                            if !collapsed.is_empty() {
                                collapsed.push_str("\n\n");
                            }
                            collapsed.push_str(&text);
                        }
                    }
                    other => kept.push(other),
                }
            }
            if !collapsed.is_empty() {
                let mut leading = leading_system.into_owned();
                if !leading.is_empty() {
                    leading.push_str("\n\n");
                }
                leading.push_str(&collapsed);
                leading_system = std::borrow::Cow::Owned(leading);
            }
            if force_cloned_replay {
                // Audit reference leg only: the former deep-clone shape.
                ReplayInput::Owned(kept.into_iter().cloned().collect())
            } else {
                ReplayInput::Filtered(kept)
            }
        };
        let grammar_input_properties = create_grammar_tool_input_properties(
            context.tools.as_deref().unwrap_or(&[]),
            compat.supports_openai_grammar_tools.unwrap_or(false),
        )
        .unwrap_or_default();
        // Upstream `resolveTranscriptTools`: in-place tool additions are only
        // anchored when the transport accepts system messages after the start
        // and no tool was removed or redeclared (`hasNonAdditiveToolChanges`).
        let anchors_tool_additions = supports_mid_convo_system_messages
            && compat.supports_mid_convo_tool_additions.unwrap_or(false)
            && !has_non_additive_tool_changes(&context.messages);
        // Upstream `resolveTranscriptTools` derives the request tools from the
        // leading system message's `toolsAdded`; later system messages only
        // carry definitions not already in that list. pir's request tools are
        // `context.tools`, so an inline addition duplicating one of them is
        // filtered out at conversion time.
        let request_tool_names = if anchors_tool_additions {
            context
                .tools
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|tool| tool.name.clone())
                .collect()
        } else {
            std::collections::BTreeSet::new()
        };
        Self {
            replay,
            leading_system,
            instruction_role: if model.reasoning && compat.supports_developer_role.unwrap_or(false)
            {
                "developer"
            } else {
                "system"
            },
            grammar_input_properties,
            supports_images: model.input.iter().any(|input| input == "image"),
            requires_tool_result_name: compat.requires_tool_result_name.unwrap_or(false),
            requires_assistant_after_tool_result: compat
                .requires_assistant_after_tool_result
                .unwrap_or(false),
            requires_thinking_as_text: compat.requires_thinking_as_text.unwrap_or(false),
            requires_reasoning_content: compat
                .requires_reasoning_content_on_assistant_messages
                .unwrap_or(false),
            anchors_tool_additions,
            request_tool_names,
        }
    }

    /// The transformed transcript (upstream `transformMessages`): pass 1
    /// normalization plus pass 2 (orphaned-call synthesis, errored/aborted
    /// assistant skip, system messages held across pending calls), with every
    /// untouched message BORROWED rather than cloned.
    fn transformed(&self, model: &Model) -> Vec<std::borrow::Cow<'_, Message>> {
        let normalize =
            |id: &str, _source: &AssistantMessage| normalize_openai_tool_call_id(model, id);
        match &self.replay {
            ReplayInput::Borrowed(messages) => crate::ai::api::transform::transform_messages_lazy(
                messages,
                model,
                Some(&normalize),
            ),
            ReplayInput::Filtered(messages) => {
                crate::ai::api::transform::transform_messages_lazy_refs(
                    messages,
                    model,
                    Some(&normalize),
                )
            }
            ReplayInput::Owned(messages) => crate::ai::api::transform::transform_messages_lazy(
                messages.as_slice(),
                model,
                Some(&normalize),
            ),
        }
    }
}

/// Whether a message is an instruction-role message (system or developer).
fn is_system_message(message: &Message) -> bool {
    matches!(message, Message::System(_) | Message::Developer(_))
}

/// Whether tool history contains a removal or same-name redeclaration that an
/// addition-only transport cannot replay (upstream
/// `hasNonAdditiveToolChanges`).
fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    let mut declared: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        if system
            .tools_removed
            .as_ref()
            .is_some_and(|removed| !removed.is_empty())
        {
            return true;
        }
        for tool in system.tools_added.iter().flatten() {
            if !declared.insert(tool.name.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Convert one `toolsAdded` definition carried by a mid-conversation system
/// message (upstream `convertTools` in the `KimiToolSystemMessageParam`
/// branch). A definition the normal converter rejects falls back to the plain
/// non-strict function shape: the transcript already declared it, so dropping
/// it would silently change the request.
fn convert_inline_tool(tool: &Tool, compat: &OpenAiCompat) -> Value {
    convert_tool(tool, compat).unwrap_or_else(|_| {
        json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            },
        })
    })
}

/// Per-message `/chat/completions` conversion state (upstream
/// `convertMessages`'s loop state). ONE instance drives both shapes — the
/// materialized `Vec<Value>` of [`convert_messages`] and the streamed
/// serializer of [`OpenAiStreamedBody`] — so a message converts identically
/// on both paths.
/// One wire item of the `messages` array: either a materialized JSON object
/// (every non-trivial conversion) or a text-only message serialized STRAIGHT
/// from the borrowed text.
///
/// MEMORY SHAPE (pir extension): a single-text message was built as a part
/// `Value` plus an item `Value` — two full copies of the text per request.
/// A summarizer request is exactly one such message (~0.3 MB per chunked call,
/// hundreds of calls on a huge session), and a real turn's leading system
/// prompt is another. The lazy variant emits the same two-key object from the
/// borrowed `&str`, so the text is copied exactly once: into the serialized
/// request body. [`Self::into_value`] materializes the identical object for
/// the value-based body path, which is what keeps the two paths
/// byte-identical.
enum WireItem<'a> {
    /// A finished JSON object (the general conversion).
    Value(Value),
    /// `{"role": <role>, "content": <text>}` — the exact bytes the value path
    /// produces for a single text part.
    Text { role: &'a str, text: &'a str },
}

impl WireItem<'_> {
    /// The materialized form (the value-based body path). The lazy text item
    /// becomes the same two-key object, in the same key order.
    fn into_value(self) -> Value {
        match self {
            WireItem::Value(value) => value,
            WireItem::Text { role, text } => json!({ "role": role, "content": text }),
        }
    }
}

impl serde::Serialize for WireItem<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        match self {
            WireItem::Value(value) => value.serialize(serializer),
            WireItem::Text { role, text } => {
                // Same shape and key order as `json!({"role": .., "content": ..})`
                // (serde_json preserves insertion order), same string escaping.
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("role", role)?;
                map.serialize_entry("content", text)?;
                map.end()
            }
        }
    }
}

struct OpenAiMessageConverter<'a> {
    compat: &'a OpenAiCompat,
    instruction_role: &'static str,
    grammar_input_properties: &'a std::collections::BTreeMap<String, String>,
    supports_images: bool,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content: bool,
    /// The model reasons (DeepSeek's `reasoning_content` gate).
    model_reasoning: bool,
    /// Later system messages carry their own `toolsAdded` (upstream
    /// `transcriptTools.anchorsAdditions`).
    anchors_tool_additions: bool,
    /// Names already declared by the top-level request field, filtered out of
    /// inline `toolsAdded` items (upstream's leading system message owns the
    /// request's initial tools).
    request_tool_names: &'a std::collections::BTreeSet<String>,
    /// Whether a leading system prompt item was emitted before the transcript
    /// (so every replayed system message sits after index 0 upstream).
    has_leading_system: bool,
    /// Index of the message being converted in the replayed transcript.
    message_index: usize,
    /// Upstream tracks the previous wire role to bridge toolResult→user gaps.
    last_role: String,
}

impl<'a> OpenAiMessageConverter<'a> {
    fn new(model: &Model, plan: &'a ReplayPlan<'_>, compat: &'a OpenAiCompat) -> Self {
        Self {
            compat,
            instruction_role: plan.instruction_role,
            grammar_input_properties: &plan.grammar_input_properties,
            supports_images: plan.supports_images,
            requires_tool_result_name: plan.requires_tool_result_name,
            requires_assistant_after_tool_result: plan.requires_assistant_after_tool_result,
            requires_thinking_as_text: plan.requires_thinking_as_text,
            requires_reasoning_content: plan.requires_reasoning_content,
            model_reasoning: model.reasoning,
            anchors_tool_additions: plan.anchors_tool_additions,
            request_tool_names: &plan.request_tool_names,
            has_leading_system: !plan.leading_system.is_empty(),
            message_index: 0,
            last_role: String::new(),
        }
    }

    /// Emit the folded leading system prompt as the first item (upstream
    /// pushes it before the transcript when non-empty).
    fn emit_leading_system<'m>(&mut self, leading_system: &'m str, out: &mut Vec<WireItem<'m>>) {
        if leading_system.is_empty() {
            return;
        }
        // Lazy item: the (potentially large) system prompt is serialized from
        // the borrowed text instead of being copied into a `Value` first.
        self.push_text_item(self.instruction_role, leading_system, out);
    }

    /// Convert one (transformed) message into the wire items it contributes:
    /// zero, one, or — for `requiresAssistantAfterToolResult` insertions — a
    /// small run of items, which are pushed onto `out` in wire order.
    fn convert<'m>(&mut self, message: &'m Message, out: &mut Vec<WireItem<'m>>) {
        let message_index = self.message_index;
        self.message_index += 1;
        match message {
            Message::User(m) | Message::System(m) | Message::Developer(m) => {
                // Mid-conversation tool additions (upstream
                // `KimiToolSystemMessageParam`): the added definitions ride in
                // their own system message so the top-level tool list keeps
                // the prompt cache. The leading system message is excluded
                // (upstream `i > 0`); pir emits it from the plan instead.
                // Definitions already in the top-level request are not
                // repeated (upstream: the leading system message declares
                // them, later messages only add NEW tools).
                if m.role == UserRole::System
                    && self.anchors_tool_additions
                    && (message_index > 0 || self.has_leading_system)
                {
                    let added: Vec<Value> = m
                        .tools_added
                        .iter()
                        .flatten()
                        .filter(|tool| !self.request_tool_names.contains(tool.name.as_str()))
                        .map(|tool| convert_inline_tool(tool, self.compat))
                        .collect();
                    if !added.is_empty() {
                        self.push_item(json!({"role": "system", "tools": added}), out);
                    }
                }
                // FAST PATH (memory lane): one non-empty text block and no
                // image — the shape a user prompt, a system message and a
                // summarizer prompt all have. It serializes straight from the
                // borrowed text (`WireItem::Text`), skipping both the part
                // `Value` and the item `Value` the general path builds; the
                // emitted bytes are the same two-key object (pinned by
                // `tests/ai_request_snapshots.rs`).
                let role = match m.role {
                    UserRole::System | UserRole::Developer => self.instruction_role,
                    UserRole::User => "user",
                };
                let has_images = m
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image(_)));
                let mut text_blocks = m.content.iter().filter_map(|block| match block {
                    ContentBlock::Text(text) if !text.text.is_empty() => Some(text.text.as_str()),
                    _ => None,
                });
                if let Some(text) = text_blocks.next() {
                    if text_blocks.next().is_none() && !has_images {
                        // Some providers require an assistant message between
                        // tool results and the next user message (upstream
                        // `requiresAssistantAfterToolResult`).
                        if self.requires_assistant_after_tool_result
                            && self.last_role == "toolResult"
                            && m.role == UserRole::User
                        {
                            self.push_item(
                                json!({
                                    "role": "assistant",
                                    "content": "I have processed the tool results.",
                                }),
                                out,
                            );
                        }
                        self.push_text_item(role, text, out);
                        return;
                    }
                }
                // Text parts are omitted when empty; image parts become
                // `image_url` data URLs (transform already downgraded images
                // for models without image input). A message with no
                // remaining parts is skipped entirely.
                let mut parts: Vec<Value> = Vec::new();
                for block in &m.content {
                    match block {
                        ContentBlock::Text(text) if !text.text.is_empty() => {
                            parts.push(json!({"type": "text", "text": text.text}));
                        }
                        ContentBlock::Image(image) => {
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", image.mime_type, image.data),
                                },
                            }));
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    return;
                }
                // Some providers require an assistant message between tool
                // results and the next user message (upstream
                // `requiresAssistantAfterToolResult`).
                if self.requires_assistant_after_tool_result
                    && self.last_role == "toolResult"
                    && m.role == UserRole::User
                {
                    self.push_item(
                        json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }),
                        out,
                    );
                }
                // Plain string content stays the standard single-text shape;
                // only image-bearing payloads need the parts array.
                let item = if parts.len() == 1
                    && parts[0].get("type").and_then(Value::as_str) == Some("text")
                {
                    json!({
                        "role": match m.role {
                            UserRole::System | UserRole::Developer => self.instruction_role,
                            UserRole::User => "user",
                        },
                        "content": parts[0]["text"].as_str().unwrap_or_default(),
                    })
                } else {
                    json!({
                        "role": match m.role {
                            UserRole::System | UserRole::Developer => self.instruction_role,
                            UserRole::User => "user",
                        },
                        "content": Value::Array(parts),
                    })
                };
                self.push_item(item, out);
            }
            Message::Assistant(m) => {
                // Replay tool calls so tool-result turns have a matching
                // `tool_calls` entry in the conversation history. Empty text
                // parts (whitespace-only) are omitted.
                let text = m
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) if !text.text.trim().is_empty() => {
                            Some(text.text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let thinking_blocks: Vec<&ThinkingContent> = m
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking(thinking)
                            if !thinking.thinking.trim().is_empty() =>
                        {
                            Some(thinking)
                        }
                        _ => None,
                    })
                    .collect();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in &m.content {
                    let ContentBlock::ToolCall(call) = block else {
                        continue;
                    };
                    if let Some(input_property) = self.grammar_input_properties.get(&call.name) {
                        // Grammar tool: replay as a custom tool call
                        // carrying the raw string input (upstream
                        // convertMessages grammar branch).
                        let input = call
                            .arguments
                            .get(input_property)
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        tool_calls.push(json!({
                            "id": call.id,
                            "type": "custom",
                            "custom": { "name": call.name, "input": input },
                        }));
                        continue;
                    }
                    tool_calls.push(json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": serde_json::to_string(&call.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                        }
                    }));
                }
                // Reasoning replay is content too: a thinking-only assistant
                // turn still contributes `reasoning_details` / the raw
                // reasoning field. Only truly empty messages are skipped
                // (e.g. aborted responses with no content).
                let has_reasoning_replay = !thinking_blocks.is_empty()
                    || m.content.iter().any(|block| match block {
                        ContentBlock::Thinking(thinking) => {
                            parse_openai_reasoning_details(thinking.thinking_signature.as_deref())
                                .is_some()
                        }
                        _ => false,
                    });
                if text.is_empty() && tool_calls.is_empty() && !has_reasoning_replay {
                    // Skip assistant messages that have no content and no
                    // tool calls (e.g. aborted responses with no content).
                    return;
                }
                let mut item = json!({"role": "assistant"});
                if self.requires_thinking_as_text && !thinking_blocks.is_empty() {
                    // Thinking blocks replayed as plain text, no delimiters
                    // (upstream `requiresThinkingAsText`).
                    let thinking_text = thinking_blocks
                        .iter()
                        .map(|block| block.thinking.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let mut content = vec![json!({"type": "text", "text": thinking_text})];
                    if !text.is_empty() {
                        content.push(json!({"type": "text", "text": text}));
                    }
                    item["content"] = Value::Array(content);
                } else if text.is_empty() {
                    // Some providers don't accept null content; but with tool
                    // calls present null content is the standard shape.
                    item["content"] = Value::Null;
                } else {
                    item["content"] = json!(text);
                }
                if !tool_calls.is_empty() {
                    item["tool_calls"] = Value::Array(tool_calls);
                }
                apply_assistant_reasoning(&mut item, m);
                // DeepSeek requires an (empty) reasoning_content on every
                // replayed assistant message when reasoning is enabled
                // (upstream `requiresReasoningContentOnAssistantMessages`).
                if self.requires_reasoning_content
                    && self.model_reasoning
                    && item.get("reasoning_content").is_none()
                {
                    item["reasoning_content"] = json!("");
                }
                self.push_item(item, out);
            }
            Message::ToolResult(m) => {
                // Always send tool result with text (or a placeholder when the
                // result carried only images or nothing at all). Images also
                // ride as a follow-up user message with `image_url` parts
                // (upstream convertMessages toolResult branch).
                let text = blocks_text(&m.content);
                let image_parts: Vec<Value> = if self.supports_images {
                    m.content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Image(image) => Some(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", image.mime_type, image.data),
                                },
                            })),
                            _ => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let has_images = !image_parts.is_empty();
                // Whitespace-only output is the empty placeholder (#8720, D35).
                let tool_result_text = if !text.trim().is_empty() {
                    text
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    "(no tool output)".to_string()
                };
                let mut tool_message = json!({
                    "role": "tool",
                    "content": tool_result_text,
                    "tool_call_id": m.tool_call_id,
                });
                // Some providers require the `name` field in tool results
                // (upstream `requiresToolResultName`).
                if self.requires_tool_result_name && !m.tool_name.is_empty() {
                    tool_message["name"] = json!(m.tool_name);
                }
                self.push_item(tool_message, out);
                if has_images {
                    if self.requires_assistant_after_tool_result {
                        self.push_item(
                            json!({
                                "role": "assistant",
                                "content": "I have processed the tool results.",
                            }),
                            out,
                        );
                    }
                    let mut content = vec![json!({
                        "type": "text",
                        "text": "Attached image(s) from tool result:",
                    })];
                    content.extend(image_parts);
                    self.push_item(
                        json!({
                            "role": "user",
                            "content": content,
                        }),
                        out,
                    );
                    // The role the next message sees after an image follow-up
                    // (upstream sets `lastRole` at the end of the branch, not
                    // from the wire role of the pushed item).
                    self.last_role = "user".to_string();
                } else {
                    self.last_role = "toolResult".to_string();
                }
            }
        }
    }

    /// Push one finished item, keeping the running wire role in sync
    /// (upstream's `lastRole` update).
    fn push_item(&mut self, item: Value, out: &mut Vec<WireItem<'_>>) {
        self.last_role = match item.get("role").and_then(Value::as_str) {
            Some("system") | Some("developer") => "system".to_string(),
            Some(role) => role.to_string(),
            None => self.last_role.clone(),
        };
        out.push(WireItem::Value(item));
    }

    /// [`Self::push_item`] for the lazy text item: the SAME role bookkeeping,
    /// with the text serialized from the borrow.
    fn push_text_item<'m>(&mut self, role: &'m str, text: &'m str, out: &mut Vec<WireItem<'m>>) {
        self.last_role = match role {
            "system" | "developer" => "system".to_string(),
            other => other.to_string(),
        };
        out.push(WireItem::Text { role, text });
    }
}
