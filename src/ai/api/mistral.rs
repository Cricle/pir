//! Mistral native Chat Completions adapter (`mistral-conversations`).
//!
//! Rust equivalent of upstream `packages/ai/src/api/mistral-conversations.ts`:
//! the Mistral chat-completions wire with camelCase request fields remapped to
//! snake_case on the wire, Mistral content chunks (`thinking` arrays,
//! `image_url`), 9-character tool-call id normalization, prompt caching via
//! the `x-affinity` header plus `prompt_cache_key`, and the chat-stream SSE
//! mapping (string or chunk-array `content` deltas, `tool_calls` with
//! partial-JSON arguments, Mistral finish reasons).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::ai::api::transform::short_hash;
use crate::ai::api::transform::{transform_messages_lazy, transform_messages_lazy_refs};
use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;

/// Tool-call ids must be exactly this many alphanumeric characters
/// (upstream `MISTRAL_TOOL_CALL_ID_LENGTH`).
pub const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Mistral provider.
#[derive(Clone)]
pub struct MistralConversationsProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
}

impl MistralConversationsProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Mistral".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            api_key_env: vec!["MISTRAL_API_KEY".into()],
        })
    }

    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = client;
        self
    }

    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    pub fn with_api_key_env(mut self, env: Vec<String>) -> Self {
        self.api_key_env = env;
        self
    }
}

impl Provider for MistralConversationsProvider {
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
            let result = mistral_stream(&client, &model, &context, &options, &events).await;
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

// ---------------------------------------------------------------------------
// Tool call id normalization (upstream createMistralToolCallIdNormalizer)
// ---------------------------------------------------------------------------

/// Deterministic 9-character tool-call id derivation with collision handling
/// (upstream `deriveMistralToolCallId` + the normalizer maps). Public for
/// tests/.
pub struct MistralToolCallIdNormalizer {
    id_map: HashMap<String, String>,
    reverse_map: HashMap<String, String>,
}

impl Default for MistralToolCallIdNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl MistralToolCallIdNormalizer {
    pub fn new() -> Self {
        Self {
            id_map: HashMap::new(),
            reverse_map: HashMap::new(),
        }
    }

    pub fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.id_map.get(id) {
            return existing.clone();
        }
        let mut attempt = 0u32;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            let owner_ok = match self.reverse_map.get(&candidate) {
                Some(owner) => owner == id,
                None => true,
            };
            if owner_ok {
                self.id_map.insert(id.to_string(), candidate.clone());
                self.reverse_map.insert(candidate.clone(), id.to_string());
                return candidate;
            }
            attempt += 1;
        }
    }
}

fn derive_mistral_tool_call_id(id: &str, attempt: u32) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if attempt == 0 && normalized.chars().count() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id
    } else {
        normalized.as_str()
    };
    let seed = if attempt == 0 {
        seed_base.to_string()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

// ---------------------------------------------------------------------------
// Request conversion (upstream buildChatPayload + toChatMessages)
// ---------------------------------------------------------------------------

fn supports_images(model: &Model) -> bool {
    model.input.iter().any(|input| input == "image")
}

/// Build the Mistral chat-completions payload (camelCase, remapped to
/// snake_case on the wire by [`to_mistral_wire_payload`]). Public for tests/.
pub fn build_mistral_payload(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    normalizer: &mut MistralToolCallIdNormalizer,
) -> AiResult<Value> {
    let normalizer = std::cell::RefCell::new(normalizer);
    // Upstream resolveTranscript/collapseSystemMessages (transcript.ts:99-112):
    // when the API cannot take mid-conversation system messages
    // (`supportsMidConvoSystemMessages`, default false), all mid-transcript
    // system text is replayed into the leading system prompt and the messages
    // are dropped from the transcript.
    let supports_mid_convo_system_messages = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_mid_convo_system_messages)
        .unwrap_or(false);
    // COW: the leading prompt is copied only when a mid-convo system message
    // actually has to be folded into it; the common request borrows it.
    let mut leading_system: Cow<'_, str> =
        Cow::Borrowed(context.system_prompt.as_deref().unwrap_or(""));
    // The transcript enters the transform BORROWED whenever it is replayed
    // unchanged (mid-conversation system messages supported). The collapse
    // branch keeps a Vec<&Message> (one pointer per message) instead of
    // cloning every surviving message: `transform_messages_lazy_refs` only
    // rebuilds the messages that actually change.
    let normalize = |id: &str, _source: &AssistantMessage| normalizer.borrow_mut().normalize(id);
    let transformed = if supports_mid_convo_system_messages {
        transform_messages_lazy(&context.messages, model, Some(&normalize))
    } else {
        let mut collapsed = String::new();
        let mut kept: Vec<&Message> = Vec::with_capacity(context.messages.len());
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
            leading_system = Cow::Owned(leading);
        }
        transform_messages_lazy_refs(&kept, model, Some(&normalize))
    };
    let images = supports_images(model);

    let mut messages: Vec<Value> = Vec::new();
    if !leading_system.is_empty() {
        messages.push(json!({"role": "system", "content": leading_system.as_ref()}));
    }
    for message in &transformed {
        match message.as_ref() {
            Message::System(system) => {
                let text = crate::ai::types::system_message_text(system);
                if !text.is_empty() {
                    messages.push(json!({"role": "system", "content": text}));
                }
            }
            Message::User(user) | Message::Developer(user) => {
                let mut content = Vec::new();
                for block in &user.content {
                    match block {
                        ContentBlock::Text(text) => {
                            content.push(json!({"type": "text", "text": text.text}));
                        }
                        ContentBlock::Image(image) if images => {
                            content.push(json!({
                                "type": "image_url",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    continue;
                }
                messages.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(assistant) => {
                let mut content_parts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content_parts.push(json!({"type": "text", "text": text.text}));
                            }
                        }
                        ContentBlock::Thinking(thinking) => {
                            if !thinking.thinking.trim().is_empty() {
                                content_parts.push(json!({
                                    "type": "thinking",
                                    "thinking": [{"type": "text", "text": thinking.thinking}],
                                }));
                            }
                        }
                        ContentBlock::ToolCall(call) => {
                            tool_calls.push(json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": serde_json::to_string(&call.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                },
                                "index": 0,
                            }));
                        }
                        ContentBlock::Image(_) => {}
                    }
                }
                if content_parts.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut item = json!({"role": "assistant", "prefix": false});
                if !content_parts.is_empty() {
                    item["content"] = Value::Array(content_parts);
                }
                if !tool_calls.is_empty() {
                    item["toolCalls"] = Value::Array(tool_calls);
                }
                messages.push(item);
            }
            Message::ToolResult(result) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = result
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image(_)));
                let tool_text = build_tool_result_text(&text, has_images, images, result.is_error);
                let mut content = vec![json!({"type": "text", "text": tool_text})];
                if images {
                    for block in &result.content {
                        if let ContentBlock::Image(image) = block {
                            content.push(json!({
                                "type": "image_url",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }));
                        }
                    }
                }
                messages.push(json!({
                    "role": "tool",
                    "toolCallId": result.tool_call_id,
                    "name": result.tool_name,
                    "content": content,
                }));
            }
        }
    }

    let mut payload = json!({
        "model": model.id,
        "stream": true,
        "messages": messages,
    });
    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            let mut converted = Vec::with_capacity(tools.len());
            for tool in tools {
                let strict =
                    crate::ai::api::constrained_sampling::resolve_json_schema_strict_sampling(
                        tool, true, None,
                    )?
                    .unwrap_or(false);
                let parameters =
                    crate::ai::api::constrained_sampling::get_json_schema_tool_parameters(
                        tool, strict,
                    )?;
                converted.push(json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                        "strict": strict,
                    }
                }));
            }
            payload["tools"] = Value::Array(converted);
        }
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(max_tokens) = options.max_tokens {
        payload["maxTokens"] = json!(max_tokens);
    }
    // Tool choice (upstream `buildChatPayload`: `if (options?.toolChoice)
    // payload.toolChoice = mapToolChoice(options.toolChoice)`); the typed
    // choice maps to Mistral's string/function forms, and
    // [`to_mistral_wire_payload`] renames the field to `tool_choice` on the
    // wire.
    if let Some(tool_choice) = options.tool_choice.as_ref() {
        payload["toolChoice"] = tool_choice.mistral_json();
    }
    // Models with a thinking level map use `reasoning_effort`; other reasoning
    // models use `prompt_mode` (upstream streamSimple dispatch).
    if model.reasoning {
        let effort_map = model.thinking_level_map.as_ref();
        match options.reasoning {
            Some(level) if !matches!(level, ThinkingLevel::Off) => {
                let key = level.wire_key();
                let mapped = effort_map
                    .and_then(|map| map.get(key))
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "high".to_string());
                if effort_map.is_some() {
                    payload["reasoningEffort"] = json!(mapped);
                } else {
                    payload["promptMode"] = json!("reasoning");
                    payload["reasoningEffort"] = json!(mapped);
                }
            }
            Some(ThinkingLevel::Off) => {
                if let Some(map) = effort_map {
                    if let Some(off) = map.get("off").cloned().flatten() {
                        payload["reasoningEffort"] = json!(off);
                    }
                }
            }
            _ => {}
        }
    }
    // Prompt caching: `promptCacheKey` plus the `x-affinity` header.
    if options.cache_retention != Some(CacheRetention::None) {
        if let Some(session_id) = &options.session_id {
            payload["promptCacheKey"] = json!(session_id);
        }
    }
    Ok(payload)
}

fn build_tool_result_text(text: &str, has_images: bool, supports: bool, is_error: bool) -> String {
    let trimmed = text.trim();
    let error_prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let image_suffix = if has_images && !supports {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{error_prefix}{trimmed}{image_suffix}");
    }
    if has_images {
        return if supports {
            format!("{error_prefix}(see attached image)")
        } else {
            format!("{error_prefix}(image omitted: model does not support images)")
        };
    }
    format!("{error_prefix}(no tool output)")
}

/// Remap camelCase request fields to the snake_case wire shape (upstream
/// `toMistralWirePayload`). Public for tests/.
pub fn to_mistral_wire_payload(payload: &Value) -> Value {
    into_mistral_wire_payload(payload.clone())
}

/// [`to_mistral_wire_payload`] that TAKES ownership of the adapter payload.
///
/// WHY (no full-request copy): the stream path builds the payload, lets the
/// `on_payload` hook mutate it, then remaps it once and drops it. The public
/// borrowing form cloned the entire `Value` tree (every message and tool
/// definition) just to rename a handful of keys; the owned form renames in
/// place and hands the same allocation to the transport.
fn into_mistral_wire_payload(mut wire: Value) -> Value {
    for (source, target) in [
        ("topP", "top_p"),
        ("maxTokens", "max_tokens"),
        ("randomSeed", "random_seed"),
        ("responseFormat", "response_format"),
        ("toolChoice", "tool_choice"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("reasoningEffort", "reasoning_effort"),
        ("promptMode", "prompt_mode"),
        ("promptCacheKey", "prompt_cache_key"),
        ("safePrompt", "safe_prompt"),
    ] {
        remap_property(&mut wire, source, target);
    }
    // `response_format.jsonSchema` → `json_schema`, and its nested
    // `schemaDefinition` → `schema` (upstream handles the nested record).
    if let Some(response_format) = wire
        .get_mut("response_format")
        .and_then(Value::as_object_mut)
    {
        if let Some(value) = response_format.remove("jsonSchema") {
            response_format.insert("json_schema".to_string(), value);
        }
        if let Some(json_schema) = response_format
            .get_mut("json_schema")
            .and_then(Value::as_object_mut)
        {
            if let Some(value) = json_schema.remove("schemaDefinition") {
                json_schema.insert("schema".to_string(), value);
            }
        }
    }
    if let Some(messages) = wire.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            remap_property(message, "toolCalls", "tool_calls");
            remap_property(message, "toolCallId", "tool_call_id");
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                for chunk in content.iter_mut() {
                    for (source, target) in [
                        ("imageUrl", "image_url"),
                        ("documentUrl", "document_url"),
                        ("documentName", "document_name"),
                        ("fileId", "file_id"),
                        ("referenceIds", "reference_ids"),
                        ("inputAudio", "input_audio"),
                    ] {
                        remap_property(chunk, source, target);
                    }
                }
            }
        }
    }
    wire
}

fn remap_property(record: &mut Value, source: &str, target: &str) {
    if let Some(object) = record.as_object_mut() {
        if let Some(value) = object.remove(source) {
            object.insert(target.to_string(), value);
        }
    }
}

// ---------------------------------------------------------------------------
// Stream consumption (upstream consumeChatStream)
// ---------------------------------------------------------------------------

fn mistral_stream_url(model: &Model) -> String {
    let base = model.base_url.trim_end_matches('/');
    format!("{base}/v1/chat/completions")
}

async fn mistral_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> AiResult<()> {
    let api_key = options.api_key.clone().ok_or_else(|| {
        AiError::Authentication(format!("No API key for provider: {}", model.provider))
    })?;
    let mut normalizer = MistralToolCallIdNormalizer::default();
    let mut payload = build_mistral_payload(model, context, options, &mut normalizer)?;
    // Upstream `options.onPayload`: inspect (or replace, by mutating in place)
    // the camelCase adapter payload before the snake_case wire remap.
    if let Some(on_payload) = &options.hooks.on_payload {
        on_payload(&mut payload);
    }
    // The payload is not read after the remap: move it through the in-place
    // conversion instead of cloning the whole request Value tree.
    let wire = into_mistral_wire_payload(payload);

    // Upstream `buildMistralHeaders`: defaults first, then model headers, then
    // caller (request) headers last so they can override everything.
    let mut headers = ProviderHeaders::new();
    headers.insert(
        "user-agent".into(),
        format!("pir ({})", std::env::consts::OS),
    );
    headers.insert("accept".into(), "text/event-stream".into());
    headers.insert("content-type".into(), "application/json".into());
    headers.insert("authorization".into(), format!("Bearer {api_key}"));
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    // Upstream `hasMistralHeaderOverride` compares override names
    // case-insensitively (mistral-conversations.ts:382-384). Model and option
    // headers were merged into the normalized map above with lowercased
    // names, so an explicit caller `X-Affinity` in any casing is honored and
    // is never overwritten by the session id.
    let has_explicit_affinity = headers.contains_key("x-affinity");
    if options.cache_retention != Some(CacheRetention::None) && !has_explicit_affinity {
        if let Some(session_id) = &options.session_id {
            headers.insert("x-affinity".into(), session_id.clone());
        }
    }

    let mut parser = MistralSseParser::new(model)
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
    let url = mistral_stream_url(model);
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
                &wire,
                options.signal.as_ref(),
                &options.hooks,
                &mut on_chunk,
            )
            .await?
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

/// Incremental Mistral SSE parser. Both the batch path
/// ([`parse_mistral_sse`]) and the streaming path drive the same state
/// machine. Public for tests/.
struct MistralSseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// (`Arc::make_mut(&mut self.message)`) so partial snapshots are O(1)
    /// hand-outs and a delta nobody snapshotted is applied in place — instead
    /// of the previous deep clone of the whole accumulated message per delta.
    message: Arc<AssistantMessage>,
    /// The in-flight text or thinking block (upstream `currentBlock`).
    current: Option<CurrentBlock>,
    /// Tool block content indices keyed by provider index or call id
    /// (upstream `toolBlocksByKey`).
    tool_blocks: HashMap<String, usize>,
    /// Accumulated raw argument JSON per tool content index.
    tool_json: HashMap<usize, String>,
    /// Bounded-cadence refresh of the parsed arguments mirrored into the
    /// block (the raw `tool_json` stays the accumulation source of truth).
    tool_mirror: HashMap<usize, crate::ai::api::partial_mirror::MirrorCadence>,
    finish: StopReason,
    error_message: Option<String>,
    /// Attached upstream `onProviderStreamEvent` observer (each parsed
    /// provider chunk before pir normalization; read-only).
    provider_event_hook: Option<crate::ai::types::StreamHook>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum BlockKind {
    Text,
    Thinking,
}

struct CurrentBlock {
    kind: BlockKind,
    index: usize,
}

impl MistralSseParser {
    pub fn new(model: &Model) -> Self {
        Self {
            buffer: Vec::new(),
            data_lines: Vec::new(),
            message: Arc::new(crate::ai::api::base_message(model)),
            current: None,
            tool_blocks: HashMap::new(),
            tool_json: HashMap::new(),
            tool_mirror: HashMap::new(),
            // Upstream `output.stopReason` starts as "pending": a stream that
            // ends without a finish reason is an error, not a silent stop
            // (mistral-conversations.ts:160).
            finish: StopReason::Pending,
            error_message: None,
            provider_event_hook: None,
        }
    }

    /// Attach the upstream `onProviderStreamEvent` observer (each parsed
    /// provider chunk before pir normalization). Public for tests/.
    pub fn with_provider_event_hook(mut self, hook: Option<crate::ai::types::StreamHook>) -> Self {
        self.provider_event_hook = hook;
        self
    }

    /// Snapshot of the message accumulated so far.
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`; every later
    /// mutation copies-on-write, so the handed-out value stays the snapshot
    /// taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result =
            crate::ai::api::sse_feed_lines(chunk, &mut buffer, |line| self.feed_line(line, events));
        self.buffer = buffer;
        result
    }

    pub fn feed_line(
        &mut self,
        line: &str,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if line.is_empty() {
            return self.flush_event(events);
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.trim_start().to_string());
        }
        Ok(())
    }

    fn flush_event(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if self.data_lines.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data_lines).join("\n");
        self.handle_data(data.trim(), events)
    }

    /// Map one SSE data payload (a Mistral `CompletionChunk`) to events.
    pub fn handle_data(
        &mut self,
        data: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)?;
        // Some Mistral-compatible gateways report mid-stream failures as an
        // in-band error object. Surface it (with the retry class the provider
        // signals) instead of the generic "invalid event" protocol error.
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            let text = mistral_error_frame_text(error);
            return Err(crate::ai::api::provider_error_frame_from_value(error, text));
        }
        if !value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty())
            && value.get("usage").is_none()
        {
            // A top-level `{message, code/type}` error object (Mistral's HTTP
            // error-body shape relayed as a stream event).
            if let Some(message) = value.get("message").and_then(Value::as_str) {
                let text = format!("Mistral API error: {message}");
                return Err(crate::ai::api::provider_error_frame_from_value(
                    &value, text,
                ));
            }
            return Err(AiError::NonRetryable(
                "Invalid Mistral streaming event".to_string(),
            ));
        }
        // Upstream `options.onProviderStreamEvent`: the parsed provider chunk
        // is read-only here (upstream wraps it as `{ data: parsed }`; the pir
        // hook receives the parsed payload itself).
        if let Some(hook) = &self.provider_event_hook {
            hook(&value);
        }
        // Keep the first non-empty stream id (upstream `output.responseId ||= chunk.id`).
        if self.message.response_id.is_none() {
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    Arc::make_mut(&mut self.message).response_id = Some(id.to_string());
                }
            }
        }
        if let Some(usage) = value.get("usage") {
            self.apply_usage(usage);
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        // Upstream `if (choice.finish_reason)`
        // (mistral-conversations.ts:633): an empty finish_reason is not a
        // terminal signal, so it must not map to an error stop (some
        // providers send `""` on keep-alive chunks); any non-empty value —
        // including unknown ones — is adopted and mapped like upstream's
        // default branch.
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            Arc::make_mut(&mut self.message).raw_stop_reason = Some(reason.to_string());
            let (stop, error) = map_mistral_stop_reason(reason);
            self.finish = stop;
            self.error_message = error;
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        self.handle_content_delta(delta, out)?;
        self.handle_tool_calls(delta, out)?;
        Ok(())
    }

    fn apply_usage(&mut self, usage: &Value) {
        let prompt_tokens = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = cached_prompt_tokens(usage);
        let cached = cached.min(prompt_tokens);
        let output = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let input = prompt_tokens.saturating_sub(cached);
        Arc::make_mut(&mut self.message).usage = Usage {
            input,
            output,
            cache_read: cached,
            cache_write: 0,
            total_tokens: usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(input + output + cached),
            ..Usage::default()
        };
    }

    fn handle_content_delta(
        &mut self,
        delta: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let content = match delta.get("content") {
            Some(Value::String(text)) => vec![Value::String(text.clone())],
            Some(Value::Array(items)) => items.clone(),
            _ => return Ok(()),
        };
        for item in &content {
            match item {
                Value::String(text) => {
                    if text.is_empty() {
                        // Empty text deltas around thinking/tool calls must
                        // not open blocks (upstream GLM-on-Mistral note).
                        continue;
                    }
                    self.push_text_delta(text, out)?;
                }
                Value::Object(chunk) => {
                    let kind = chunk.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "thinking" {
                        let delta_text = chunk
                            .get("thinking")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        if delta_text.is_empty() {
                            continue;
                        }
                        self.push_thinking_delta(&delta_text, out)?;
                    } else if kind == "text" {
                        let delta_text = chunk.get("text").and_then(Value::as_str).unwrap_or("");
                        if delta_text.is_empty() {
                            continue;
                        }
                        self.push_text_delta(delta_text, out)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn push_text_delta(
        &mut self,
        text: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let switch = !matches!(&self.current, Some(current) if current.kind == BlockKind::Text);
        if switch {
            self.close_current_block(out);
            let index = self.message.content.len();
            Arc::make_mut(&mut self.message)
                .content
                .push(ContentBlock::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
            self.current = Some(CurrentBlock {
                kind: BlockKind::Text,
                index,
            });
            out.push(AssistantMessageEvent::TextStart {
                content_index: index,
                partial: Arc::clone(&self.message),
            });
        }
        let index = match &self.current {
            Some(current) => current.index,
            None => return Ok(()),
        };
        if let Some(ContentBlock::Text(block)) =
            Arc::make_mut(&mut self.message).content.get_mut(index)
        {
            block.text.push_str(text);
        }
        out.push(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: text.to_string(),
            partial: Arc::clone(&self.message),
        });
        Ok(())
    }

    fn push_thinking_delta(
        &mut self,
        text: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let switch = !matches!(&self.current, Some(current) if current.kind == BlockKind::Thinking);
        if switch {
            self.close_current_block(out);
            let index = self.message.content.len();
            Arc::make_mut(&mut self.message)
                .content
                .push(ContentBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                }));
            self.current = Some(CurrentBlock {
                kind: BlockKind::Thinking,
                index,
            });
            out.push(AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial: Arc::clone(&self.message),
            });
        }
        let index = match &self.current {
            Some(current) => current.index,
            None => return Ok(()),
        };
        if let Some(ContentBlock::Thinking(block)) =
            Arc::make_mut(&mut self.message).content.get_mut(index)
        {
            block.thinking.push_str(text);
        }
        out.push(AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta: text.to_string(),
            partial: Arc::clone(&self.message),
        });
        Ok(())
    }

    fn handle_tool_calls(
        &mut self,
        delta: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let calls = match delta.get("tool_calls").and_then(Value::as_array) {
            Some(calls) => calls.clone(),
            None => return Ok(()),
        };
        for call in calls {
            if self.current.is_some() {
                self.close_current_block(out);
            }
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != "null");
            let index_key = call
                .get("index")
                .and_then(Value::as_u64)
                .map(|index| index.to_string());
            let call_id = id.map(str::to_string).unwrap_or_else(|| {
                derive_mistral_tool_call_id(
                    &format!("toolcall:{}", index_key.as_deref().unwrap_or("0")),
                    0,
                )
            });
            // Upstream keys blocks by `toolCall.index ?? callId`, so deltas
            // for the same index accumulate into one call even when only the
            // first delta carries the id.
            let key = match &index_key {
                Some(index) => index.clone(),
                None => call_id.clone(),
            };
            let content_index = match self.tool_blocks.get(&key).copied() {
                Some(existing) => existing,
                None => {
                    let content_index = self.message.content.len();
                    let name = call
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Arc::make_mut(&mut self.message)
                        .content
                        .push(ContentBlock::ToolCall(ToolCall {
                            id: call_id,
                            name,
                            arguments: BTreeMap::new(),
                            thought_signature: None,
                            namespace: None,
                            arguments_error: None,
                        }));
                    self.tool_blocks.insert(key.clone(), content_index);
                    out.push(AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: Arc::clone(&self.message),
                    });
                    content_index
                }
            };
            let args_delta = match call
                .get("function")
                .and_then(|function| function.get("arguments"))
            {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            self.tool_json
                .entry(content_index)
                .or_default()
                .push_str(&args_delta);
            // Rebuilding the parsed value on every fragment is O(args²) per
            // call, so the mirror is refreshed at a bounded cadence instead
            // (first fragment, then every 8 KiB); the finalized call is parsed
            // from the complete raw buffer.
            if !args_delta.is_empty()
                && self
                    .tool_mirror
                    .entry(content_index)
                    .or_default()
                    .record(&args_delta)
            {
                let partial = self
                    .tool_json
                    .get(&content_index)
                    .cloned()
                    .unwrap_or_default();
                if let Some(ContentBlock::ToolCall(tool_call)) = Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(content_index)
                {
                    tool_call.arguments = serde_json::from_value(
                        crate::ai::api::openai_responses::shared::parse_partial_json(&partial),
                    )
                    .unwrap_or_default();
                }
            }
            out.push(AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta: args_delta,
                partial: Arc::clone(&self.message),
            });
        }
        Ok(())
    }

    fn close_current_block(&mut self, out: &mut Vec<AssistantMessageEvent>) {
        let Some(current) = self.current.take() else {
            return;
        };
        match current.kind {
            BlockKind::Text => {
                let content = match self.message.content.get(current.index) {
                    Some(ContentBlock::Text(text)) => text.text.clone(),
                    _ => String::new(),
                };
                out.push(AssistantMessageEvent::TextEnd {
                    content_index: current.index,
                    content,
                    partial: Arc::clone(&self.message),
                });
            }
            BlockKind::Thinking => {
                let content = match self.message.content.get(current.index) {
                    Some(ContentBlock::Thinking(thinking)) => thinking.thinking.clone(),
                    _ => String::new(),
                };
                out.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: current.index,
                    content,
                    partial: Arc::clone(&self.message),
                });
            }
        }
    }

    /// Flush a trailing event block, finalize tool calls, and emit `Done`.
    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        self.flush_event(out)
            .map_err(crate::ai::api::trailing_event_error)?;
        self.close_current_block(out);
        // Finalize every tool block (upstream post-stream loop).
        let indices: Vec<usize> = {
            let mut indices: Vec<usize> = self.tool_blocks.values().copied().collect();
            indices.sort_unstable();
            indices
        };
        for content_index in indices {
            let partial = self
                .tool_json
                .get(&content_index)
                .cloned()
                .unwrap_or_default();
            let call = if let Some(ContentBlock::ToolCall(tool_call)) =
                Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(content_index)
            {
                // A finished call's arguments are parsed strictly (upstream
                // `parseToolCallArguments`, #10944); in-flight deltas keep the
                // lenient partial parse. Malformed text keeps the call with
                // empty arguments plus `argumentsError`, and the stream
                // completes so the model can correct it.
                crate::ai::api::tool_args::apply_final_tool_arguments(tool_call, &partial);
                tool_call.clone()
            } else {
                continue;
            };
            out.push(AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call: call,
                partial: Arc::clone(&self.message),
            });
        }
        // Upstream mistral-conversations.ts:160: a stream that ends while the
        // stop reason is still "pending" is a stream error. A clean EOF here
        // means the terminal event never arrived (a truncated stream), which
        // is retryable at the agent level.
        if matches!(self.finish, StopReason::Pending) {
            return Err(AiError::StreamTruncated(
                "Mistral stream ended without a finish reason".to_string(),
            ));
        }
        if matches!(self.finish, StopReason::Error) {
            let message = self
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider stopped with: error".to_string());
            // Mistral's `finish_reason: "error"` is a transient server
            // failure; other provider error reasons stay non-retryable.
            return Err(crate::ai::api::provider_error_frame(
                message, None, None, None,
            ));
        }
        // The end events above may still hold the previous snapshot, so this
        // finalization copies-on-write once; the terminal event then reclaims
        // the accumulator without another copy.
        {
            let message = Arc::make_mut(&mut self.message);
            message.stop_reason = self.finish;
            message.error_message = self.error_message.clone();
        }
        let terminal = match self.finish {
            StopReason::ToolUse => TerminalReason::ToolUse,
            StopReason::Length => TerminalReason::Length,
            _ => TerminalReason::Stop,
        };
        let message = std::mem::replace(&mut self.message, Arc::new(AssistantMessage::default()));
        let message = match Arc::try_unwrap(message) {
            Ok(message) => message,
            Err(shared) => (*shared).clone(),
        };
        out.push(AssistantMessageEvent::Done {
            reason: terminal,
            message,
        });
        Ok(())
    }
}

/// Extract cached prompt tokens from any of the Mistral usage spellings
/// (upstream `getMistralCachedPromptTokens`).
fn cached_prompt_tokens(usage: &Value) -> u64 {
    let candidates = [
        usage
            .get("promptTokensDetails")
            .and_then(|d| d.get("cachedTokens")),
        usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens")),
        usage
            .get("promptTokenDetails")
            .and_then(|d| d.get("cachedTokens")),
        usage
            .get("prompt_token_details")
            .and_then(|d| d.get("cached_tokens")),
        usage.get("numCachedTokens"),
        usage.get("num_cached_tokens"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(value) = candidate.as_u64() {
            return value;
        }
        if let Some(value) = candidate.as_f64() {
            if value.is_finite() && value >= 0.0 {
                return value as u64;
            }
        }
    }
    0
}

/// Display text for a Mistral in-band error object.
fn mistral_error_frame_text(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("Provider returned an error");
    let mut text = format!("Mistral API error: {message}");
    if let Some(code) = error.get("code").and_then(|code| match code {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }) {
        text.push_str(&format!(" ({code})"));
    }
    text
}

/// Map a Mistral finish reason (upstream `mapChatStopReason`).
pub fn map_mistral_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        // Mistral reports transient server failures this way; "server error"
        // makes the message retryable downstream.
        "error" => (
            StopReason::Error,
            Some("Provider stopped with: error (server error)".to_string()),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

/// Parse a full SSE body into events. Public for tests/.
pub fn parse_mistral_sse(
    body: &[u8],
    model: &Model,
) -> Result<Vec<AssistantMessageEvent>, AiError> {
    let mut parser = MistralSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for data in crate::ai::api::sse_data(body) {
        parser.handle_data(&data, &mut out)?;
    }
    parser.finish(&mut out)?;
    Ok(out)
}
