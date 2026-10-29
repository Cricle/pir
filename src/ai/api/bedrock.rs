//! Amazon Bedrock ConverseStream adapter.
//!
//! Rust equivalent of upstream `packages/ai/src/api/bedrock-converse-stream.ts`
//! (subset relevant to pir): ConverseStream request conversion (system,
//! messages, toolConfig, additionalModelRequestFields for thinking), AWS
//! event-stream response framing, and usage/stop-reason mapping. Auth covers
//! the bearer-token API key mode (`AWS_BEARER_TOKEN_BEDROCK` or the request
//! api key) and SigV4 request signing with credentials from the AWS chain
//! (`credentials` submodule): static env (`AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`), shared profile files
//! (`AWS_PROFILE`), web identity token files, and ECS container roles, with
//! the region from `AWS_REGION` / `AWS_DEFAULT_REGION` / a shared profile /
//! the endpoint host.

use std::collections::BTreeMap;
use std::sync::Arc;

/// AWS credential chain (shared profile INI, ECS container roles, web
/// identity) feeding the SigV4 path.
pub mod credentials;

use serde_json::{json, Map, Value};

use crate::ai::api::provider_env_value;
use crate::ai::api::transform::{
    get_initial_system_prompt, normalize_id_part, split_error_tool_result_images,
    transform_messages_lazy,
};
use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;

/// Placeholder the Anthropic path uses for empty required text
/// (upstream `EMPTY_TEXT_PLACEHOLDER`).
const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

/// Anthropic beta for the thinking block-binding controls (upstream
/// `THINKING_BINDING_CONTROLS_BETA`).
pub const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Placeholder for redacted reasoning content (upstream
/// `REDACTED_THINKING_PLACEHOLDER`).
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";

/// Human-readable prefixes for Bedrock SDK exception names (upstream
/// `BEDROCK_ERROR_PREFIXES`).
fn bedrock_error_prefix(name: &str) -> Option<&'static str> {
    match name {
        "InternalServerException" => Some("Internal server error"),
        "ModelStreamErrorException" => Some("Model stream error"),
        "ValidationException" => Some("Validation error"),
        "ThrottlingException" => Some("Throttling error"),
        "ServiceUnavailableException" => Some("Service unavailable"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Amazon Bedrock ConverseStream provider.
#[derive(Clone)]
pub struct BedrockConverseStreamProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
}

impl BedrockConverseStreamProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Amazon Bedrock".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
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
}

impl Provider for BedrockConverseStreamProvider {
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
            "Amazon Bedrock bearer token".to_string(),
            vec!["AWS_BEARER_TOKEN_BEDROCK".to_string()],
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
        // Producer handle, not a consumer clone: dropping the last consumer
        // must be observable here (see `push_await`'s `bool`).
        let events = output.producer();
        let mut client = self.client.clone();
        crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
        let model = model.clone();
        tokio::spawn(async move {
            let result = bedrock_stream(&client, &model, &context, &options, &events).await;
            if events.is_abandoned() {
                // Normal abandon: nobody is reading, so there is nothing to
                // report and nothing to wait for.
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

async fn bedrock_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> AiResult<()> {
    // Request-level profile override (upstream `BedrockOptions.profile`):
    // expose it as `AWS_PROFILE` for this request's credential resolution.
    // COW: only a request that actually carries a profile override clones the
    // (map-bearing) `StreamOptions`; the common request borrows them.
    let profile_override = options
        .bedrock
        .as_ref()
        .and_then(|bedrock| bedrock.profile.as_deref())
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_owned);
    let options_cow: std::borrow::Cow<'_, StreamOptions> = match profile_override {
        Some(profile) => {
            let mut owned = options.clone();
            owned
                .env
                .get_or_insert_with(BTreeMap::new)
                .insert("AWS_PROFILE".to_string(), profile);
            std::borrow::Cow::Owned(owned)
        }
        None => std::borrow::Cow::Borrowed(options),
    };
    let options = options_cow.as_ref();
    let env = options.env.as_ref();
    let mut body = build_bedrock_body(model, context, options)?;
    // Upstream `options.onPayload`: inspect (or replace, by mutating in place)
    // the ConverseStream command input before it is sent
    // (bedrock-converse-stream.ts:282-285).
    if let Some(on_payload) = &options.hooks.on_payload {
        on_payload(&mut body);
    }
    let payload = serde_json::to_vec(&body)?;

    // Endpoint: Bedrock models ship a standard runtime endpoint in the
    // catalog; a configured base URL always wins.
    let base_url = model.base_url.trim_end_matches('/');
    if base_url.is_empty() {
        return Err(AiError::NonRetryable(
            "Bedrock endpoint URL is required (model baseUrl is empty)".to_string(),
        ));
    }
    let url = format!(
        "{}/model/{}/converse-stream",
        base_url,
        uri_encode(&model.id)
    );

    let mut headers: BTreeMap<String, String> = options.headers.clone().unwrap_or_default();
    headers
        .entry("content-type".into())
        .or_insert_with(|| "application/json".into());
    headers
        .entry("accept".into())
        .or_insert_with(|| "application/json".into());
    // Thinking block-binding models need the anthropic beta header on the
    // request (upstream `anthropic_beta` additional field is a header on the
    // ConverseStream wire).
    if let Some(fields) = body.get("additionalModelRequestFields") {
        if let Some(betas) = fields.get("anthropic_beta") {
            if let Some(list) = betas.as_array() {
                let joined = list
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined.is_empty() {
                    headers.insert("x-amzn-bedrock-anthropic-beta".into(), joined);
                }
            }
        }
    }

    // Auth: bearer token first (upstream authSchemePreference
    // httpBearerAuth), then the AWS credential chain (explicit profile ->
    // static env -> web identity -> ECS container -> default profile)
    // feeding the SigV4 path.
    let skip_auth = provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1");
    let bearer_token = options
        .bedrock
        .as_ref()
        .and_then(|bedrock| bedrock.bearer_token.clone())
        .or_else(|| options.api_key.clone())
        .or_else(|| provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env));
    let mut region = resolve_bedrock_region_with_options(model, env, options.bedrock.as_ref());
    if let Some(token) = bearer_token.filter(|_| !skip_auth) {
        headers.insert("authorization".into(), format!("Bearer {token}"));
    } else if skip_auth {
        headers.insert("authorization".into(), "Bearer dummy-token".to_string());
    } else {
        let resolved = credentials::resolve_credentials(env).await?;
        // A shared-profile region only fills the gap left by the env,
        // endpoint, and ARN regions (upstream lets the SDK profile region
        // apply when nothing more specific is configured).
        let explicit_region = provider_env_value("AWS_REGION", env).is_some()
            || provider_env_value("AWS_DEFAULT_REGION", env).is_some()
            || endpoint_region(&model.base_url).is_some()
            || arn_region(&model.id).is_some();
        if !explicit_region {
            if let Some(profile_region) = resolved.region {
                region = profile_region;
            }
        }
        let signed = sigv4_sign(
            "POST",
            &url,
            &headers,
            &payload,
            &resolved.credentials.access_key_id,
            &resolved.credentials.secret_access_key,
            resolved.credentials.session_token.as_deref(),
            &region,
            "bedrock",
        )?;
        for (name, value) in signed {
            headers.insert(name, value);
        }
    }

    let mut parser = BedrockEventStreamParser::new(model)
        .with_provider_event_hook(options.hooks.on_provider_stream_event.clone());
    // `false`: the consumer is gone — skip the request entirely instead of
    // issuing one whose events nobody can read.
    if !sink
        .push_await(AssistantMessageEvent::Start {
            partial: parser.snapshot(),
        })
        .await
    {
        return Ok(());
    }
    // The transport pre-push owns the Start event; messageStart must not
    // duplicate it.
    parser.started = true;
    let (status, response_headers) = {
        let mut on_chunk = crate::ai::api::EventChunkSink::new(
            |chunk: &[u8], ready: &mut Vec<AssistantMessageEvent>| {
                // Every consumer handle is gone: release the HTTP response now
                // instead of reading and discarding the rest of the body. The
                // spawn site reports this as a normal abandon, not an error.
                if sink.is_abandoned() {
                    return Err(AiError::Aborted);
                }
                parser.feed(chunk, ready)
            },
            sink,
        );
        client
            .request_streaming_with(
                reqwest::Method::POST,
                &url,
                &headers,
                payload,
                options.signal.as_ref(),
                &options.hooks,
                &mut on_chunk,
            )
            .await?
    };
    // Upstream fires `onResponse` when the HTTP response arrives, before the
    // body is consumed (bedrock-converse-stream.ts:295); pir's transport only
    // surfaces the status and headers once streaming completes, so the hook
    // runs there.
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

/// Resolve the signing region: explicit env region, else the endpoint host's
/// region, else us-east-1 (upstream region resolution subset).
pub fn resolve_bedrock_region(model: &Model, env: Option<&ProviderEnv>) -> String {
    resolve_bedrock_region_with_options(model, env, None)
}

/// [`resolve_bedrock_region`] with the request-level `BedrockOptions.region`
/// override first.
pub fn resolve_bedrock_region_with_options(
    model: &Model,
    env: Option<&ProviderEnv>,
    bedrock: Option<&crate::ai::types::BedrockRequestOptions>,
) -> String {
    if let Some(region) = bedrock
        .and_then(|bedrock| bedrock.region.as_deref())
        .map(str::trim)
        .filter(|region| !region.is_empty())
    {
        return region.to_string();
    }
    if let Some(region) = provider_env_value("AWS_REGION", env)
        .or_else(|| provider_env_value("AWS_DEFAULT_REGION", env))
    {
        return region;
    }
    if let Some(region) = endpoint_region(&model.base_url) {
        return region;
    }
    // ARN-embedded region (upstream arnRegionMatch).
    if let Some(region) = arn_region(&model.id) {
        return region;
    }
    "us-east-1".to_string()
}

fn endpoint_region(base_url: &str) -> Option<String> {
    let rest = base_url.split_once("://")?.1;
    let host = rest.split('/').next()?;
    let parts: Vec<&str> = host.split('.').collect();
    // bedrock-runtime[-fips].{region}.amazonaws.com[.cn]
    if parts.len() >= 4 && parts[0].starts_with("bedrock-runtime") && parts[2] == "amazonaws" {
        return Some(parts[1].to_ascii_lowercase());
    }
    None
}

fn arn_region(model_id: &str) -> Option<String> {
    // Upstream regex: `^arn:aws(?:-[a-z0-9-]+)?:bedrock:([a-z0-9-]+):`.
    let rest = model_id.strip_prefix("arn:aws")?;
    // Optional partition segment: "-us-gov" / "-cn" / absent (":bedrock:").
    let rest = if let Some(after_dash) = rest.strip_prefix('-') {
        after_dash.split_once(':')?.1
    } else {
        rest.strip_prefix(':')?
    };
    let rest = rest.strip_prefix("bedrock:")?;
    let region = rest.split(':').next()?;
    (!region.is_empty()).then(|| region.to_string())
}

// ---------------------------------------------------------------------------
// Request conversion (upstream buildParams + convertMessages)
// ---------------------------------------------------------------------------

/// Build the ConverseStream request body. Public for tests/.
pub fn build_bedrock_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<Value> {
    let env = options.env.as_ref();
    // Cache retention (upstream `resolveCacheRetention`,
    // bedrock-converse-stream.ts:837-841): the explicit request value first,
    // then `PI_CACHE_RETENTION=long`, then `short`. The resolver is shared
    // with the openai-family adapters and lives in the core (`crate::ai::api`)
    // so this adapter keeps it when `provider-openai` is off.
    let cache_retention = crate::ai::api::resolve_cache_retention(options);
    let supports_strict_mode = model
        .compat
        .as_ref()
        .and_then(|compat| compat.supports_strict_mode)
        .unwrap_or(false);
    let supports_images = model.input.iter().any(|input| input == "image");
    let cacheable = cache_retention != CacheRetention::None && supports_prompt_caching(model, env);

    let mut body = json!({
        "modelId": model.id,
        "messages": convert_bedrock_messages(
            context,
            model,
            cacheable,
            cache_retention == CacheRetention::Long,
            supports_images,
        ),
    });
    if let Some(system) = get_initial_system_prompt(context) {
        let mut blocks = vec![json!({"text": system})];
        if cacheable {
            let mut cache_point = json!({"type": "default"});
            if cache_retention == CacheRetention::Long {
                cache_point["ttl"] = json!("1h");
            }
            blocks.push(json!({"cachePoint": cache_point}));
        }
        body["system"] = Value::Array(blocks);
    }

    let mut inference_config = Map::new();
    let inference_max_tokens = options
        .max_tokens
        .or_else(|| is_anthropic_claude_model(model).then_some(model.max_tokens));
    if let Some(max_tokens) = inference_max_tokens {
        inference_config.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        inference_config.insert("temperature".into(), json!(temperature));
    }
    if !inference_config.is_empty() {
        body["inferenceConfig"] = Value::Object(inference_config);
    }

    if let Some(tools) = &context.tools {
        if let Some(tool_config) =
            convert_tool_config(tools, supports_strict_mode, options.tool_choice.as_ref())?
        {
            body["toolConfig"] = tool_config;
        }
    }

    if let Some(fields) =
        build_additional_model_request_fields_with_options(model, options.reasoning, options)?
    {
        body["additionalModelRequestFields"] = fields;
    }
    // Request-level cost-allocation tags (upstream `requestMetadata`).
    if let Some(metadata) = options
        .bedrock
        .as_ref()
        .and_then(|bedrock| bedrock.request_metadata.as_ref())
        .filter(|metadata| !metadata.is_empty())
    {
        body["requestMetadata"] = json!(metadata);
    }
    Ok(body)
}

/// Convert the transcript into Converse `messages` (upstream
/// `convertMessages`). Public for tests/.
pub fn convert_bedrock_messages(
    context: &Context,
    model: &Model,
    cacheable: bool,
    long_retention: bool,
    supports_images: bool,
) -> Vec<Value> {
    let transformed = transform_messages_lazy(
        &context.messages,
        model,
        Some(&|id, _source| normalize_id_part(id)),
    );

    let mut result: Vec<Value> = Vec::new();
    let mut index = 0usize;
    while index < transformed.len() {
        match transformed[index].as_ref() {
            Message::User(user) => {
                let mut content = Vec::new();
                for block in &user.content {
                    match block {
                        ContentBlock::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content.push(json!({"text": text.text}));
                            }
                        }
                        ContentBlock::Image(image) if supports_images => {
                            content.push(image_block(image));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
                }
                result.push(json!({"role": "user", "content": content}));
                index += 1;
            }
            Message::System(_) | Message::Developer(_) => {
                index += 1;
            }
            Message::Assistant(assistant) => {
                let mut content_blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text(text) => {
                            if !text.text.trim().is_empty() {
                                content_blocks.push(json!({"text": text.text}));
                            }
                        }
                        ContentBlock::ToolCall(call) => {
                            content_blocks.push(json!({
                                "toolUse": {
                                    "toolUseId": call.id,
                                    "name": call.name,
                                    "input": call.arguments,
                                }
                            }));
                        }
                        ContentBlock::Thinking(thinking) => {
                            if thinking.redacted {
                                // Decode (and thereby validate) the stored
                                // base64 payload; the AWS SDK re-encodes the
                                // bytes as base64 on the wire, so the value
                                // travels verbatim (upstream
                                // `decodeRedactedContent`).
                                if decode_redacted_content(thinking.thinking_signature.as_deref())
                                    .is_some()
                                {
                                    content_blocks.push(json!({
                                        "reasoningContent": {
                                            "redactedContent": thinking.thinking_signature,
                                        }
                                    }));
                                }
                                continue;
                            }
                            let thinking_text = thinking.thinking.trim();
                            if thinking_text.is_empty() {
                                continue;
                            }
                            if is_anthropic_claude_model(model) {
                                let signature = thinking
                                    .thinking_signature
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty());
                                match signature {
                                    None => {
                                        // Bedrock rejects replayed reasoning
                                        // blocks without a signature; fall
                                        // back to plain text (upstream).
                                        content_blocks.push(json!({"text": thinking.thinking}));
                                    }
                                    Some(signature) => {
                                        content_blocks.push(json!({
                                            "reasoningContent": {
                                                "reasoningText": {
                                                    "text": thinking.thinking,
                                                    "signature": signature,
                                                }
                                            }
                                        }));
                                    }
                                }
                            } else {
                                content_blocks.push(json!({
                                    "reasoningContent": {"reasoningText": {"text": thinking.thinking}}
                                }));
                            }
                        }
                        ContentBlock::Image(_) => {}
                    }
                }
                if content_blocks.is_empty() {
                    index += 1;
                    continue;
                }
                result.push(json!({"role": "assistant", "content": content_blocks}));
                index += 1;
            }
            Message::ToolResult(_) => {
                // All consecutive tool results go into a single user message
                // (upstream lookahead merge). Claude rejects error tool results
                // that contain anything but text, so for Claude their images
                // follow all tool results of the turn (#10794). Other models
                // keep them in place.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut error_images: Vec<Value> = Vec::new();
                let mut lookahead = index;
                while lookahead < transformed.len() {
                    let Message::ToolResult(next) = transformed[lookahead].as_ref() else {
                        break;
                    };
                    let split = if is_anthropic_claude_model(model) {
                        split_error_tool_result_images(next)
                    } else {
                        None
                    };
                    let content = split
                        .as_ref()
                        .map(|split| split.content.as_slice())
                        .unwrap_or(next.content.as_slice());
                    tool_results.push(tool_result_block(next, content));
                    if let Some(split) = split {
                        error_images.push(json!({"text": split.caption}));
                        error_images.extend(split.images.iter().map(image_block));
                    }
                    lookahead += 1;
                }
                index = lookahead;
                let mut content = tool_results;
                content.extend(error_images);
                result.push(json!({"role": "user", "content": content}));
            }
        }
    }

    if cacheable {
        if let Some(last) = result.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some("user") {
                let mut cache_point = json!({"type": "default"});
                if long_retention {
                    cache_point["ttl"] = json!("1h");
                }
                if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                    content.push(json!({"cachePoint": cache_point}));
                }
            }
        }
    }
    result
}

fn tool_result_block(
    result: &crate::ai::types::ToolResultMessage,
    blocks: &[ContentBlock],
) -> Value {
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                if !text.text.trim().is_empty() {
                    content.push(json!({"text": text.text}));
                }
            }
            ContentBlock::Image(image) => content.push(image_block(image)),
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    json!({
        "toolResult": {
            "toolUseId": result.tool_call_id,
            "content": content,
            "status": if result.is_error { "error" } else { "success" },
        }
    })
}

fn image_block(image: &ImageContent) -> Value {
    let format = match image.mime_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => other, // validated upstream; pass through the subtype
    };
    json!({
        "image": {
            "format": format,
            "source": {"bytes": image.data},
        }
    })
}

/// Decode a base64 redacted-reasoning payload; invalid payloads drop the block
/// (upstream `decodeRedactedContent`).
fn decode_redacted_content(signature: Option<&str>) -> Option<Vec<u8>> {
    use base64::Engine;
    let signature = signature?;
    base64::engine::general_purpose::STANDARD
        .decode(signature)
        .ok()
        .filter(|bytes| !bytes.is_empty())
}

/// Base64-encode bytes (upstream `bytesToBase64`).
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Convert tool definitions to the Converse `toolConfig` (upstream
/// `convertToolConfig`, bedrock-converse-stream.ts:1148-1183). `toolChoice`
/// `none` drops the whole `toolConfig` (tools are not sent); `auto` emits the
/// Converse `{auto: {}}` choice. Without a request tool choice the previous
/// default (`auto`) is kept (pir deviation: upstream omits the field when
/// unset). Public for tests/.
fn convert_tool_config(
    tools: &[Tool],
    supports_strict_mode: bool,
    tool_choice: Option<&ToolChoice>,
) -> AiResult<Option<Value>> {
    if tools.is_empty() {
        return Ok(None);
    }
    if matches!(tool_choice, Some(ToolChoice::None)) {
        return Ok(None);
    }
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = crate::ai::api::constrained_sampling::resolve_json_schema_strict_sampling(
            tool,
            supports_strict_mode,
            None,
        )?
        .unwrap_or(false);
        let parameters =
            crate::ai::api::constrained_sampling::get_json_schema_tool_parameters(tool, strict)?;
        let mut spec = json!({
            "name": tool.name,
            "description": tool.description,
            "inputSchema": {"json": parameters},
        });
        if strict {
            spec["strict"] = json!(true);
        }
        converted.push(json!({"toolSpec": spec}));
    }
    Ok(Some(json!({
        "tools": converted,
        "toolChoice": tool_choice
            .map(ToolChoice::bedrock_json)
            .unwrap_or_else(|| json!({"auto": {}})),
    })))
}

// ---------------------------------------------------------------------------
// Model probes (upstream helpers)
// ---------------------------------------------------------------------------

fn model_match_candidates(model: &Model) -> Vec<String> {
    let mut candidates = vec![model.id.to_lowercase()];
    let name = model.name.to_lowercase();
    if !name.is_empty() {
        candidates.push(name.clone());
        // Upstream also normalizes separators to dashes for the name.
        let normalized: String = name
            .chars()
            .map(|c| {
                if matches!(c, ' ' | '_' | '.' | ':') {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        candidates.push(normalized);
    }
    candidates
}

/// Check if the model is an Anthropic Claude model on Bedrock (upstream
/// `isAnthropicClaudeModel`). Public for tests/.
fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

/// Check whether the model supports adaptive thinking 鈥?Opus 4.6+, Sonnet 4.6+
/// (upstream `supportsAdaptiveThinking`). Public for tests/.
fn supports_adaptive_thinking(model: &Model) -> bool {
    let candidates = model_match_candidates(model);
    candidates.iter().any(|candidate| {
        candidate.contains("opus-4-6")
            || candidate.contains("opus-4-7")
            || candidate.contains("opus-4-8")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-4-6")
            || candidate.contains("sonnet-5")
            || candidate.contains("haiku-5")
            || candidate.contains("fable-5")
    })
}

/// Check whether the model accepts `thinking.block_binding` 鈥?Opus 4.7+ /
/// Sonnet 5+ (upstream `supportsThinkingBlockBinding`). Public for tests/.
fn supports_thinking_block_binding(model: &Model) -> bool {
    let candidates = model_match_candidates(model);
    candidates.iter().any(|candidate| {
        candidate.contains("opus-4-7")
            || candidate.contains("opus-4-8")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-5")
            || candidate.contains("haiku-5")
            || candidate.contains("fable-5")
    })
}

/// Check whether the model supports prompt caching (upstream
/// `supportsPromptCaching`). Public for tests/.
fn supports_prompt_caching(model: &Model, env: Option<&ProviderEnv>) -> bool {
    let candidates = model_match_candidates(model);
    let has_claude_ref = candidates
        .iter()
        .any(|candidate| candidate.contains("claude"));
    if !has_claude_ref {
        return provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    candidates.iter().any(|candidate| {
        candidate.contains("fable-5")
            || candidate.contains("opus-5")
            || candidate.contains("sonnet-5")
            || candidate.contains("haiku-5")
            || candidate.contains("-4-")
            || candidate.contains("claude-3-7-sonnet")
            || candidate.contains("claude-3-5-haiku")
    })
}

/// Map a thinking level to a Claude effort (upstream
/// `mapThinkingLevelToEffort` subset: the thinkingLevelMap wins; xhigh is
/// native only on the newest models).
fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    crate::ai::api::map_thinking_level_to_effort(model, Some(level))
}

/// Build `additionalModelRequestFields` for thinking (upstream
/// `buildAdditionalModelRequestFields`). Public for tests/.
pub fn build_additional_model_request_fields(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
) -> AiResult<Option<Value>> {
    build_additional_model_request_fields_with_options(model, reasoning, &StreamOptions::default())
}

/// [`build_additional_model_request_fields`] with request options: the
/// `thinking_budgets` overrides feed the budget table and `max_tokens` clamps
/// the budget (upstream bedrock-converse-stream.ts:554-570). Public for tests/.
pub fn build_additional_model_request_fields_with_options(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    options: &StreamOptions,
) -> AiResult<Option<Value>> {
    let Some(level) = reasoning else {
        return Ok(None);
    };
    if !model.reasoning {
        return Ok(None);
    }
    // `off` must omit the thinking fields entirely. The budget/effort tables
    // below intentionally have no `off` arm, so without this guard it would
    // fall through to their high/16k defaults.
    if level == ThinkingLevel::Off {
        return Ok(None);
    }

    if is_anthropic_claude_model(model) {
        // GovCloud Bedrock rejects the Claude thinking.display field; omit it
        // there until the GovCloud Converse schema catches up (upstream
        // `isGovCloudBedrockTarget`; pir detects via the model id shape).
        let gov_cloud = model.id.to_lowercase().starts_with("us-gov.")
            || model.id.to_lowercase().starts_with("arn:aws-us-gov:");
        // Thinking display (upstream `thinkingDisplay`, default "summarized").
        let display = if gov_cloud {
            None
        } else {
            Some(
                options
                    .thinking_display
                    .map(|display| display.as_str())
                    .unwrap_or("summarized"),
            )
        };
        if supports_adaptive_thinking(model) {
            let use_block_binding = supports_thinking_block_binding(model);
            let mut thinking = json!({"type": "adaptive"});
            if let Some(display) = display {
                thinking["display"] = json!(display);
            }
            if use_block_binding {
                thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
            }
            let mut fields = json!({
                "thinking": thinking,
                "output_config": {"effort": map_thinking_level_to_effort(model, level)},
            });
            if use_block_binding {
                fields["anthropic_beta"] = json!([THINKING_BINDING_CONTROLS_BETA]);
            }
            return Ok(Some(fields));
        }
        // Budget-based Claude: default budgets per level (upstream
        // defaultBudgets) with per-request `thinkingBudgets` overrides winning;
        // extended levels clamp to high; the budget shares the response
        // ceiling, keeping at least 1024 tokens for the answer (upstream
        // `Math.min(adjusted.thinkingBudget, Math.max(0, maxTokens - 1024))`,
        // bedrock-converse-stream.ts:554-570).
        let budget_level = match level {
            ThinkingLevel::XHigh | ThinkingLevel::Max => ThinkingLevel::High,
            other => other,
        };
        let budget = match budget_level {
            // Explicit "off" keeps the previous unmatched-arm default.
            ThinkingLevel::Off => 16384,
            level => {
                crate::ai::api::thinking_budget_for_level(level, options.thinking_budgets.as_ref())
            }
        };
        let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
        let budget = budget.min(max_tokens.saturating_sub(1024));
        let mut thinking = json!({"type": "enabled", "budget_tokens": budget});
        if let Some(display) = display {
            thinking["display"] = json!(display);
        }
        let mut fields = json!({ "thinking": thinking });
        // Upstream `interleavedThinking` (default true): the beta is only
        // needed for budget-based Claude models.
        if options.interleaved_thinking.unwrap_or(true) {
            fields["anthropic_beta"] = json!(["interleaved-thinking-2025-05-14"]);
        }
        return Ok(Some(fields));
    }

    let candidates = model_match_candidates(model);
    if candidates
        .iter()
        .any(|candidate| candidate.contains("gpt-oss"))
    {
        let effort = match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            _ => "high",
        };
        return Ok(Some(json!({"reasoning_effort": effort})));
    }
    if candidates
        .iter()
        .any(|candidate| candidate.contains("gpt-"))
    {
        let mapped = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(level.wire_key()))
            .cloned()
            .flatten();
        let effort = mapped.unwrap_or_else(|| match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low".to_string(),
            ThinkingLevel::Medium => "medium".to_string(),
            ThinkingLevel::High => "high".to_string(),
            ThinkingLevel::XHigh => "xhigh".to_string(),
            ThinkingLevel::Max => "max".to_string(),
            ThinkingLevel::Off => "low".to_string(),
        });
        return Ok(Some(json!({"reasoning": {"effort": effort}})));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Response event-stream parsing
// ---------------------------------------------------------------------------

/// Incremental AWS event-stream parser feeding the same state machine as the
/// batch path ([`parse_bedrock_event_stream`]).
pub struct BedrockEventStreamParser {
    buffer: Vec<u8>,
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// (`Arc::make_mut(&mut self.message)`) so partial snapshots are O(1)
    /// hand-outs and a delta nobody snapshotted is applied in place — instead
    /// of the previous deep clone of the whole accumulated message per delta.
    message: Arc<AssistantMessage>,
    /// Provider content-block index -> content index in `message.content`.
    blocks: BTreeMap<usize, usize>,
    /// Accumulated tool-call argument JSON per content index (upstream
    /// `partialJson` scratch).
    tool_json: BTreeMap<usize, String>,
    /// Bounded-cadence refresh of the parsed arguments mirrored into the
    /// block (the raw `tool_json` stays the accumulation source of truth).
    tool_mirror: BTreeMap<usize, crate::ai::api::partial_mirror::MirrorCadence>,
    /// Accumulated redacted-reasoning bytes per content index (upstream
    /// `redactedChunks`), base64-encoded into `thinkingSignature` at stop.
    redacted_bytes: BTreeMap<usize, Vec<u8>>,
    started: bool,
    saw_stop: bool,
    finish: StopReason,
    /// Attached upstream `onProviderStreamEvent` observer (each parsed
    /// provider stream item before pir normalization; read-only).
    provider_event_hook: Option<crate::ai::types::StreamHook>,
}

impl BedrockEventStreamParser {
    pub fn new(model: &Model) -> Self {
        Self {
            buffer: Vec::new(),
            message: Arc::new(AssistantMessage {
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                timestamp: crate::ai::util::now_ms(),
                ..AssistantMessage::default()
            }),
            blocks: BTreeMap::new(),
            tool_json: BTreeMap::new(),
            tool_mirror: BTreeMap::new(),
            redacted_bytes: BTreeMap::new(),
            started: false,
            saw_stop: false,
            finish: StopReason::Stop,
            provider_event_hook: None,
        }
    }

    /// Attach the upstream `onProviderStreamEvent` observer (each parsed
    /// provider stream item before pir normalization). Public for tests/.
    pub fn with_provider_event_hook(mut self, hook: Option<crate::ai::types::StreamHook>) -> Self {
        self.provider_event_hook = hook;
        self
    }

    /// Snapshot of the message accumulated so far (e.g. for the Start event).
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`; every later
    /// mutation copies-on-write, so the handed-out value stays the snapshot
    /// taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    /// Feed one raw body chunk; decodes complete event-stream frames.
    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        self.buffer.extend_from_slice(chunk);
        loop {
            if self.buffer.len() < 12 {
                return Ok(());
            }
            let total_len = u32::from_be_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]) as usize;
            if total_len < 16 {
                return Err(AiError::NonRetryable(
                    "Bedrock event stream: malformed frame prelude".to_string(),
                ));
            }
            if total_len > 16 * 1024 * 1024 {
                return Err(AiError::ResponseTooLarge {
                    limit: 16 * 1024 * 1024,
                });
            }
            if self.buffer.len() < total_len {
                return Ok(());
            }
            let frame: Vec<u8> = self.buffer.drain(..total_len).collect();
            let headers_len = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
            // The prelude (12) plus headers plus the trailing message CRC (4)
            // must fit inside the frame; anything else is a hostile/malformed
            // frame and must fail as a typed error, never panic on a
            // start-greater-than-end payload slice.
            if 12 + headers_len + 4 > total_len {
                return Err(AiError::NonRetryable(
                    "Bedrock event stream: headers exceed frame size".to_string(),
                ));
            }
            let headers = decode_event_headers(&frame[12..12 + headers_len])?;
            let payload = &frame[12 + headers_len..total_len - 4];
            let message_type = headers
                .get(":message-type")
                .map(String::as_str)
                .unwrap_or("");
            let event_type = headers.get(":event-type").map(String::as_str).unwrap_or("");
            match message_type {
                "event" => self.handle_event(event_type, payload, events)?,
                "exception" => {
                    let text = String::from_utf8_lossy(payload);
                    // Modeled Bedrock exceptions carry a JSON payload with a
                    // `Message`/`message` member; fall back to the raw body.
                    let message = serde_json::from_str::<Value>(text.trim())
                        .ok()
                        .and_then(|parsed| {
                            parsed
                                .get("Message")
                                .or_else(|| parsed.get("message"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| text.chars().take(500).collect());
                    let prefix = bedrock_error_prefix(event_type).unwrap_or(event_type);
                    return Err(crate::ai::api::provider_error_frame(
                        format!("{prefix}: {message}"),
                        None,
                        Some(event_type),
                        None,
                    ));
                }
                "error" => {
                    let text = String::from_utf8_lossy(payload);
                    let message = text.chars().take(500).collect::<String>();
                    return Err(crate::ai::api::provider_error_frame(
                        format!("Bedrock event stream error: {message}"),
                        None,
                        None,
                        None,
                    ));
                }
                _ => {
                    return Err(AiError::NonRetryable(
                        "Bedrock event stream: missing :message-type header".to_string(),
                    ));
                }
            }
        }
    }

    fn handle_event(
        &mut self,
        event_type: &str,
        payload: &[u8],
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if payload.is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_slice(payload)?;
        // Upstream `options.onProviderStreamEvent`: the parsed provider item
        // is read-only here (bedrock-converse-stream.ts:299).
        if let Some(hook) = &self.provider_event_hook {
            hook(&value);
        }
        match event_type {
            "messageStart" => {
                let role = value
                    .get("messageStart")
                    .and_then(|start| start.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("assistant");
                if role != "assistant" {
                    return Err(AiError::NonRetryable(
                        "Unexpected assistant message start but got user message start instead"
                            .to_string(),
                    ));
                }
                if !self.started {
                    self.started = true;
                    out.push(AssistantMessageEvent::Start {
                        partial: Arc::clone(&self.message),
                    });
                }
            }
            "contentBlockStart" => {
                let index = value
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let tool_use = value.get("start").and_then(|start| start.get("toolUse"));
                if let Some(tool_use) = tool_use {
                    let call = ToolCall {
                        id: tool_use
                            .get("toolUseId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: tool_use
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        arguments: Map::new().into_iter().collect(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    };
                    let content_index = self.message.content.len();
                    Arc::make_mut(&mut self.message)
                        .content
                        .push(ContentBlock::ToolCall(call));
                    self.blocks.insert(index, content_index);
                    out.push(AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: Arc::clone(&self.message),
                    });
                }
            }
            "contentBlockDelta" => {
                self.handle_delta(&value, out)?;
            }
            "contentBlockStop" => {
                let index = value
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                // Upstream deletes the wire index at stop
                // (`finalizeStreamingBlock`: `delete block.index`), so a later
                // block reusing the same wire index never writes back into the
                // stopped block.
                let Some(content_index) = self.blocks.remove(&index) else {
                    return Ok(());
                };
                match self.message.content.get(content_index) {
                    Some(ContentBlock::Text(_)) => {
                        let content = match self.message.content.get(content_index) {
                            Some(ContentBlock::Text(text)) => text.text.clone(),
                            _ => String::new(),
                        };
                        out.push(AssistantMessageEvent::TextEnd {
                            content_index,
                            content,
                            partial: Arc::clone(&self.message),
                        });
                    }
                    Some(ContentBlock::Thinking(_)) => {
                        // Flush buffered encrypted reasoning into the
                        // signature (upstream `flushRedactedContent`).
                        if let Some(bytes) = self.redacted_bytes.remove(&content_index) {
                            if let Some(ContentBlock::Thinking(block)) =
                                Arc::make_mut(&mut self.message)
                                    .content
                                    .get_mut(content_index)
                            {
                                block.thinking_signature = Some(base64_encode(&bytes));
                            }
                        }
                        let content = match self.message.content.get(content_index) {
                            Some(ContentBlock::Thinking(thinking)) => thinking.thinking.clone(),
                            _ => String::new(),
                        };
                        out.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: Arc::clone(&self.message),
                        });
                    }
                    Some(ContentBlock::ToolCall(_)) => {
                        // Finalize the accumulated argument JSON (upstream
                        // `parseToolCallArguments` at block stop). Malformed
                        // text keeps the call with empty arguments plus
                        // `argumentsError`; the stream completes so the model
                        // can correct it on the next turn.
                        let raw = self
                            .tool_json
                            .remove(&content_index)
                            .unwrap_or_else(|| "{}".to_string());
                        self.tool_mirror.remove(&content_index);
                        let call = if let Some(ContentBlock::ToolCall(call)) =
                            Arc::make_mut(&mut self.message)
                                .content
                                .get_mut(content_index)
                        {
                            crate::ai::api::tool_args::apply_final_tool_arguments(call, &raw);
                            call.clone()
                        } else {
                            return Ok(());
                        };
                        out.push(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call: call,
                            partial: Arc::clone(&self.message),
                        });
                    }
                    _ => {}
                }
            }
            "messageStop" => {
                self.saw_stop = true;
                let reason = value
                    .get("messageStop")
                    .and_then(|stop| stop.get("stopReason"))
                    .and_then(Value::as_str);
                Arc::make_mut(&mut self.message).raw_stop_reason = reason.map(str::to_string);
                let (stop, error_message) = map_bedrock_stop_reason(reason);
                self.finish = stop;
                Arc::make_mut(&mut self.message).error_message = error_message;
            }
            "metadata" => {
                if let Some(usage) = value
                    .get("metadata")
                    .and_then(|metadata| metadata.get("usage"))
                {
                    let input = usage
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let output = usage
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cacheReadInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_write = usage
                        .get("cacheWriteInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cache_write_1h = usage
                        .get("cacheDetails")
                        .and_then(Value::as_array)
                        .map(|details| {
                            details
                                .iter()
                                .filter(|detail| {
                                    detail.get("ttl").and_then(Value::as_str) == Some("1h")
                                })
                                .filter_map(|detail| {
                                    detail.get("inputTokens").and_then(Value::as_u64)
                                })
                                .sum::<u64>()
                        })
                        .filter(|total| *total > 0);
                    Arc::make_mut(&mut self.message).usage = Usage {
                        input,
                        output,
                        cache_read,
                        cache_write,
                        cache_write_1h,
                        total_tokens: usage
                            .get("totalTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(input + output),
                        ..Usage::default()
                    };
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_delta(
        &mut self,
        value: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let index = value
            .get("contentBlockIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let delta = value.get("delta").cloned().unwrap_or(Value::Null);
        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            let content_index = match self.blocks.get(&index).copied() {
                Some(existing) => existing,
                None => {
                    // No text block yet: contentBlockStart is not sent for
                    // text blocks (upstream creates one here).
                    let content_index = self.message.content.len();
                    Arc::make_mut(&mut self.message)
                        .content
                        .push(ContentBlock::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                    self.blocks.insert(index, content_index);
                    out.push(AssistantMessageEvent::TextStart {
                        content_index,
                        partial: Arc::clone(&self.message),
                    });
                    content_index
                }
            };
            if let Some(ContentBlock::Text(block)) = Arc::make_mut(&mut self.message)
                .content
                .get_mut(content_index)
            {
                block.text.push_str(text);
            }
            out.push(AssistantMessageEvent::TextDelta {
                content_index,
                delta: text.to_string(),
                partial: Arc::clone(&self.message),
            });
            return Ok(());
        }
        if let Some(tool_use) = delta.get("toolUse") {
            let Some(content_index) = self.blocks.get(&index).copied() else {
                return Ok(());
            };
            let chunk = tool_use.get("input").and_then(Value::as_str).unwrap_or("");
            // Upstream pushes the delta even when `input` is empty or missing
            // (`delta: delta.toolUse.input || ""`), so no chunk is swallowed.
            self.tool_json
                .entry(content_index)
                .or_default()
                .push_str(chunk);
            // Upstream parses the accumulated partial JSON on every delta
            // (`block.arguments = parseStreamingJson(block.partialJson)`,
            // bedrock-converse-stream.ts:630-631). Rebuilding the whole value
            // per fragment is O(args²) per call, so the mirror is refreshed at
            // a bounded cadence instead (first fragment, then every 8 KiB);
            // the finalized call is parsed from the complete raw buffer.
            let refresh = self
                .tool_mirror
                .entry(content_index)
                .or_default()
                .record(chunk);
            if refresh {
                let partial = self
                    .tool_json
                    .get(&content_index)
                    .cloned()
                    .unwrap_or_default();
                if let Some(ContentBlock::ToolCall(call)) = Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(content_index)
                {
                    call.arguments = serde_json::from_value(
                        crate::ai::api::openai_responses::shared::parse_partial_json(&partial),
                    )
                    .unwrap_or_default();
                }
            }
            out.push(AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta: chunk.to_string(),
                partial: Arc::clone(&self.message),
            });
            return Ok(());
        }
        if let Some(reasoning) = delta.get("reasoningContent") {
            let content_index = match self.blocks.get(&index).copied() {
                Some(existing) => existing,
                None => {
                    let content_index = self.message.content.len();
                    Arc::make_mut(&mut self.message)
                        .content
                        .push(ContentBlock::Thinking(ThinkingContent {
                            thinking: String::new(),
                            thinking_signature: None,
                            redacted: false,
                        }));
                    self.blocks.insert(index, content_index);
                    out.push(AssistantMessageEvent::ThinkingStart {
                        content_index,
                        partial: Arc::clone(&self.message),
                    });
                    content_index
                }
            };
            if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                if let Some(ContentBlock::Thinking(thinking)) = Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(content_index)
                {
                    thinking.thinking.push_str(text);
                }
                out.push(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: text.to_string(),
                    partial: Arc::clone(&self.message),
                });
            }
            if let Some(signature) = reasoning.get("signature").and_then(Value::as_str) {
                if let Some(ContentBlock::Thinking(thinking)) = Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(content_index)
                {
                    if !thinking.redacted {
                        let existing = thinking.thinking_signature.get_or_insert_with(String::new);
                        existing.push_str(signature);
                    }
                }
            }
            if let Some(redacted) = reasoning.get("redactedContent").and_then(Value::as_str) {
                if !redacted.is_empty() {
                    // Encrypted reasoning from non-Anthropic models on
                    // Bedrock (e.g. OpenAI GPT-5.6): keep the decoded payload
                    // buffered; the base64 encoding travels in
                    // `thinkingSignature` at block stop (upstream
                    // `redactedChunks`).
                    if let Some(bytes) = decode_redacted_content(Some(redacted)) {
                        self.redacted_bytes
                            .entry(content_index)
                            .or_default()
                            .extend(bytes);
                    }
                    let already_redacted = match self.message.content.get(content_index) {
                        Some(ContentBlock::Thinking(thinking)) => thinking.redacted,
                        _ => false,
                    };
                    if !already_redacted {
                        if let Some(ContentBlock::Thinking(thinking)) =
                            Arc::make_mut(&mut self.message)
                                .content
                                .get_mut(content_index)
                        {
                            thinking.redacted = true;
                            thinking.thinking_signature = None;
                            thinking.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                        }
                        out.push(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: REDACTED_THINKING_PLACEHOLDER.to_string(),
                            partial: Arc::clone(&self.message),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if !self.saw_stop {
            // A clean EOF without `messageStop` is a truncated stream (the
            // transport read a complete body, so no HTTP verdict applies):
            // retryable at the agent level.
            return Err(AiError::StreamTruncated(
                "Bedrock stream ended without a stop reason".to_string(),
            ));
        }
        if matches!(self.finish, StopReason::Error | StopReason::Aborted) {
            return Err(AiError::NonRetryable(
                self.message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "An unknown error occurred".to_string()),
            ));
        }
        // The end events above may still hold the previous snapshot, so this
        // finalization copies-on-write once; the terminal event then reclaims
        // the accumulator without another copy.
        Arc::make_mut(&mut self.message).stop_reason = self.finish;
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

/// Map a Bedrock stop reason to a pir stop reason (upstream `mapStopReason`).
pub fn map_bedrock_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("end_turn") | Some("stop_sequence") => (StopReason::Stop, None),
        Some("max_tokens") | Some("model_context_window_exceeded") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some(other) => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
        None => (StopReason::Error, None),
    }
}

/// Parse a full event-stream body into events. Public for tests/.
pub fn parse_bedrock_event_stream(
    body: &[u8],
    model: &Model,
) -> Result<Vec<AssistantMessageEvent>, AiError> {
    let mut parser = BedrockEventStreamParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    // The leading Start here mirrors the transport push; messageStart emits
    // its own only once.
    parser.started = true;
    parser.feed(body, &mut out)?;
    parser.finish(&mut out)?;
    Ok(out)
}

/// Decode AWS event-stream header block into string headers (only the
/// `:message-type` / `:event-type` string headers matter; other value types
/// are skipped). Bounded by the header-block length checked by the caller.
fn decode_event_headers(mut bytes: &[u8]) -> AiResult<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    while !bytes.is_empty() {
        if bytes.is_empty() {
            break;
        }
        let name_len = bytes[0] as usize;
        if bytes.len() < 1 + name_len + 1 {
            return Err(AiError::NonRetryable(
                "Bedrock event stream: truncated header name".to_string(),
            ));
        }
        let name = String::from_utf8_lossy(&bytes[1..1 + name_len]).to_string();
        bytes = &bytes[1 + name_len..];
        let value_type = bytes[0];
        bytes = &bytes[1..];
        match value_type {
            0 | 1 => {
                // true / false: no payload.
            }
            2 => {
                if bytes.is_empty() {
                    return Err(truncated_header());
                }
                bytes = &bytes[1..];
            }
            4 => {
                // INTEGER values are 4 bytes per the AWS event-stream spec.
                if bytes.len() < 4 {
                    return Err(truncated_header());
                }
                bytes = &bytes[4..];
            }
            3 => {
                if bytes.len() < 2 {
                    return Err(truncated_header());
                }
                bytes = &bytes[2..];
            }
            5 => {
                if bytes.len() < 8 {
                    return Err(truncated_header());
                }
                bytes = &bytes[8..];
            }
            8 => {
                // TIMESTAMP values are 8 bytes (int64 millis) per the AWS
                // event-stream spec; skipping only 4 desynchronized the
                // remaining header block.
                if bytes.len() < 8 {
                    return Err(truncated_header());
                }
                bytes = &bytes[8..];
            }
            6 | 7 => {
                if bytes.len() < 2 {
                    return Err(truncated_header());
                }
                let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                if bytes.len() < 2 + length {
                    return Err(truncated_header());
                }
                if value_type == 7 {
                    headers.insert(
                        name,
                        String::from_utf8_lossy(&bytes[2..2 + length]).to_string(),
                    );
                }
                bytes = &bytes[2 + length..];
            }
            other => {
                return Err(AiError::NonRetryable(format!(
                    "Bedrock event stream: unknown header value type {other}"
                )));
            }
        }
    }
    Ok(headers)
}

fn truncated_header() -> AiError {
    AiError::NonRetryable("Bedrock event stream: truncated header value".to_string())
}

// ---------------------------------------------------------------------------
// SigV4 signing
// ---------------------------------------------------------------------------

/// AWS SigV4 request signing. Returns the headers to add
/// (`x-amz-date`, optional `x-amz-security-token`, `authorization`).
/// All caller headers except `authorization` participate in the signature.
/// Public for tests/.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sigv4_sign(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    payload: &[u8],
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    service: &str,
) -> AiResult<Vec<(String, String)>> {
    let amz_date = amz_now();
    sigv4_sign_at(
        method,
        url,
        headers,
        payload,
        access_key,
        secret_key,
        session_token,
        region,
        service,
        &amz_date,
    )
}

/// [`sigv4_sign`] with an explicit `x-amz-date` (deterministic signature
/// vectors). Public for tests/.
#[allow(clippy::too_many_arguments)]
pub fn sigv4_sign_at(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    payload: &[u8],
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    region: &str,
    service: &str,
    amz_date: &str,
) -> AiResult<Vec<(String, String)>> {
    let (host, canonical_uri, canonical_query) = split_url(url)?;
    let payload_hash = sha256_hex(payload);

    // Canonical headers: every caller header (except authorization) plus host,
    // x-amz-date, x-amz-security-token, lower-cased, sorted, trimmed values.
    let mut canonical: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .filter(|(name, _)| name != "authorization")
        .collect();
    canonical.push(("host".to_string(), host));
    canonical.push(("x-amz-date".to_string(), amz_date.to_string()));
    if let Some(token) = session_token {
        canonical.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    canonical.sort_by(|a, b| a.0.cmp(&b.0));
    canonical.dedup_by(|a, b| a.0 == b.0);
    let canonical_headers = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed_headers = canonical
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{}/{region}/{service}/aws4_request", &amz_date[..8]);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signing_key = sigv4_signing_key(secret_key, &amz_date[..8], region, service)?;
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes())?);
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    let mut out = vec![
        ("x-amz-date".to_string(), amz_date.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash),
        ("authorization".to_string(), authorization),
    ];
    if let Some(token) = session_token {
        out.push(("x-amz-security-token".to_string(), token.to_string()));
    }
    Ok(out)
}

/// SigV4 signing key derivation (upstream uses the AWS SDK; this matches the
/// published algorithm). Public for tests/.
pub fn sigv4_signing_key(
    secret_key: &str,
    date: &str,
    region: &str,
    service: &str,
) -> AiResult<Vec<u8>> {
    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_sha256(&k_date, region.as_bytes())?;
    let k_service = hmac_sha256(&k_region, service.as_bytes())?;
    hmac_sha256(&k_service, b"aws4_request")
}

fn split_url(url: &str) -> AiResult<(String, String, String)> {
    let (_scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| AiError::Http(format!("invalid URL: {url}")))?;
    let (authority, path_and_query) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path_and_query, ""),
    };
    Ok((
        authority.to_ascii_lowercase(),
        canonical_uri_path(path),
        canonical_query_string(query),
    ))
}

/// URI-encode a path segment preserving `/` (AWS canonical URI rules).
fn canonical_uri_path(path: &str) -> String {
    path.split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// URI-encode everything except unreserved characters (AWS rules: `-`, `_`,
/// `.`, `~` plus alphanumerics; everything else hex-escaped upper-case).
fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Canonical query string: sorted by key then value, each URI-encoded.
fn canonical_query_string(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (uri_encode(key), uri_encode(value)),
            None => (uri_encode(pair), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(key, value)| {
            if value.is_empty() {
                key.clone()
            } else {
                format!("{key}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn amz_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_amz_date(now.as_secs())
}

/// Render `YYYYMMDDTHHMMSSZ` from Unix seconds. Public for tests/.
pub fn format_amz_date(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let secs_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> AiResult<Vec<u8>> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<sha2::Sha256>;
    // HMAC-SHA256 accepts any key length, including the empty key.
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| AiError::Http("HMAC-SHA256 key construction failed".to_string()))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(*byte >> 4) as usize] as char);
        out.push(HEX[(*byte & 0xf) as usize] as char);
    }
    out
}
