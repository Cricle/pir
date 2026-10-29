//! Image-generation APIs and their registry (upstream
//! `packages/ai/src/images.ts` + `images-api-registry.ts` + the
//! `api/openrouter-images.ts` adapter).
//!
//! Upstream keeps a runtime `Map<ImageApi, provider>` and dispatches on the
//! `model.api` string. pir's catalog entries carry the typed
//! [`ImageApi`] enum, so the "registry" is a plain `match` — exhaustive,
//! string-free, and compile-checked: adding an [`ImageApi`] variant without
//! an adapter fails to compile here.
//!
//! Result semantics mirror upstream: failures come back as
//! [`AssistantImages`] with `stop_reason: Error|Aborted` and
//! `error_message`, not as `Err` — but every fallible internal step (request
//! assembly, response parsing) is a typed `AiResult`, so the error text is
//! produced from real errors instead of stringly plumbing.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::ai::error::{AiError, AiResult};
use crate::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use crate::ai::types::{
    AssistantImages, ImageApi, ImageModel, ImagesContent, ImagesContext, ImagesOptions,
    ImagesRequestOptions, ImagesStopReason, OutputModality, ProviderEnv, Usage,
};

/// Dispatch one image generation on the model's typed `api` (upstream
/// `resolveImagesApiProvider` + the images-api registry). With a single known
/// image API the match arms document the registry shape; unknown-by-construction
/// variants cannot reach here.
pub async fn generate_images(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    signal: Option<&crate::ai::types::AbortSignal>,
) -> AssistantImages {
    let mut extended = ImagesRequestOptions::from(options);
    extended.signal = signal.cloned();
    generate_images_with(model, context, &extended).await
}

/// [`generate_images`] with the extended request surface (upstream
/// `ImagesOptions` extending `ProviderRequestOptions`): cancellation,
/// provider-scoped env overrides, metadata, and the timeout/retry budget ride
/// on the options instead of separate arguments.
pub async fn generate_images_with(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesRequestOptions,
) -> AssistantImages {
    let signal = options.signal.as_ref();
    match model.api {
        ImageApi::OpenRouterImages => {
            generate_openrouter_images(model, context, options, signal).await
        }
    }
}

/// The api keys each provider falls back to (upstream `env-api-keys.ts`;
/// shared with `crate::ai::auth::env_api_keys`). Re-declared here because the
/// image/classifier path resolves auth outside the provider registry.
fn provider_env_keys(provider: &str) -> &'static [&'static str] {
    match provider {
        "openrouter" => &["OPENROUTER_API_KEY"],
        "typesafe" => &["TYPESAFE_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        _ => &[],
    }
}

/// Resolve the request API key: the explicit option wins, then the provider's
/// env key (upstream `models.generateImages` merges provider-resolved auth
/// with explicit options; explicit wins).
pub fn resolve_api_key(provider: &str, options: &ImagesOptions) -> Option<String> {
    resolve_api_key_with_env(provider, options.api_key.as_deref(), None)
}

/// [`resolve_api_key`] with a provider-scoped env map: an explicit key wins,
/// then the provider's env var names in `env`, then the process environment
/// (upstream `getProviderEnvValue`: request env before process env).
pub fn resolve_api_key_with_env(
    provider: &str,
    api_key: Option<&str>,
    env: Option<&ProviderEnv>,
) -> Option<String> {
    api_key
        .map(str::to_string)
        .or_else(|| {
            env.and_then(|env| {
                provider_env_keys(provider)
                    .iter()
                    .find_map(|name| env.get(*name).cloned().filter(|value| !value.is_empty()))
            })
        })
        .or_else(|| {
            provider_env_keys(provider)
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        })
}

/// The transport config for one image/classifier request: the shared defaults
/// with the request-scoped timeout/retry overrides applied.
pub(crate) fn request_config(
    timeout_ms: Option<u64>,
    max_retries: Option<u32>,
    max_retry_delay_ms: Option<u64>,
) -> ProviderRequestConfig {
    let mut config = ProviderRequestConfig::default();
    if let Some(timeout_ms) = timeout_ms {
        config.timeout = std::time::Duration::from_millis(timeout_ms.max(1));
    }
    if let Some(max_retries) = max_retries {
        config.max_retries = max_retries;
    }
    if let Some(max_retry_delay_ms) = max_retry_delay_ms {
        config.max_retry_delay = std::time::Duration::from_millis(max_retry_delay_ms);
    }
    config
}

/// Build the request client shared by the image and classifier dispatchers.
///
/// `config.timeout` keeps its pre-seam meaning — the reqwest CONNECT timeout —
/// while `timeout_ms` additionally becomes the per-attempt TOTAL deadline
/// (upstream `ProviderRequestOptions.timeoutMs`, applied per request/attempt in
/// `classifier-shared.ts`). Without it a server that accepts the connection
/// but never answers would only be bounded by the 5-minute HTTP idle timeout.
/// Public for tests/.
pub fn request_client(
    timeout_ms: Option<u64>,
    max_retries: Option<u32>,
    max_retry_delay_ms: Option<u64>,
) -> AiResult<ProviderRequestClient> {
    let mut client =
        ProviderRequestClient::new(request_config(timeout_ms, max_retries, max_retry_delay_ms))?;
    client.set_request_timeout(
        timeout_ms.map(|timeout_ms| std::time::Duration::from_millis(timeout_ms.max(1))),
    );
    Ok(client)
}

/// Classifier-request variant of [`resolve_api_key`] (same env map).
pub fn resolve_api_key_for_classifier(
    provider: &str,
    options: &crate::ai::types::ClassifierOptions,
) -> Option<String> {
    resolve_api_key_with_env(provider, options.api_key.as_deref(), None)
}

/// [`resolve_api_key_for_classifier`] with a provider-scoped env map.
pub fn resolve_classifier_api_key_with_env(
    provider: &str,
    api_key: Option<&str>,
    env: Option<&ProviderEnv>,
) -> Option<String> {
    resolve_api_key_with_env(provider, api_key, env)
}

// ---------------------------------------------------------------------------
// OpenRouter images (upstream `api/openrouter-images.ts`)
// ---------------------------------------------------------------------------

/// Build the OpenRouter chat-completions request for one image generation
/// (upstream `buildParams`). Pure: URL + headers + body, so tests assert the
/// wire shape without a network. Public for tests/.
pub fn build_openrouter_images_request(
    model: &ImageModel,
    context: &ImagesContext,
    api_key: &str,
) -> AiResult<(String, BTreeMap<String, String>, JsonValue)> {
    if model.base_url.is_empty() {
        return Err(AiError::UnknownProvider(format!(
            "image model {}/{model_id} has no base URL",
            model.provider,
            model_id = model.id
        )));
    }
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_owned(), format!("Bearer {api_key}"));
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    for (name, value) in &model.headers.clone().unwrap_or_default() {
        headers.insert(name.clone(), value.clone());
    }
    // Caller headers override model defaults (upstream merges options.headers
    // last); carried via `ImagesOptions` at the call site, not here, to keep
    // this builder a pure function of (model, context, key).
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let content: Vec<JsonValue> = context
        .input
        .iter()
        .map(|block| match block {
            ImagesContent::Text { text } => serde_json::json!({
                "type": "text",
                "text": sanitize_surrogates(text),
            }),
            ImagesContent::Image { data, mime_type } => serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
            }),
        })
        .collect();
    let modalities: Vec<&str> = if model.output.contains(&OutputModality::Text) {
        vec!["image", "text"]
    } else {
        vec!["image"]
    };
    let body = serde_json::json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": modalities,
    });
    Ok((url, headers, body))
}

/// Upstream `sanitizeSurrogates`: drop unpaired UTF-16 surrogates that would
/// make the JSON body invalid. pir's strings are UTF-8 and serde never emits
/// lone surrogates, so this is the identity — kept as the documented seam.
fn sanitize_surrogates(text: &str) -> String {
    text.to_owned()
}

/// Parse the OpenRouter image-generation response (upstream's response
/// handling): text content first, then `message.images` data URLs. Public
/// for tests/.
pub fn parse_openrouter_images_response(
    body: &JsonValue,
) -> AiResult<(Vec<ImagesContent>, Option<String>, Option<Usage>)> {
    let response_id = body
        .get("id")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let usage = body.get("usage").and_then(parse_openrouter_usage);
    let mut output = Vec::new();
    let Some(choice) = body.pointer("/choices/0") else {
        return Ok((output, response_id, usage));
    };
    if let Some(JsonValue::String(text)) = choice.pointer("/message/content") {
        if !text.is_empty() {
            output.push(ImagesContent::Text { text: text.clone() });
        }
    }
    if let Some(images) = choice
        .pointer("/message/images")
        .and_then(JsonValue::as_array)
    {
        for image in images {
            let url = match image.get("image_url") {
                Some(JsonValue::String(url)) => Some(url.clone()),
                Some(value) => value
                    .get("url")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned),
                None => None,
            };
            let Some(url) = url.filter(|url| url.starts_with("data:")) else {
                continue;
            };
            let Some((mime_type, data)) = parse_data_url(&url) else {
                continue;
            };
            output.push(ImagesContent::Image { data, mime_type });
        }
    }
    Ok((output, response_id, usage))
}

/// Split a `data:<mime>;base64,<data>` URL (upstream's inline regex).
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(";base64,")?;
    if meta.is_empty() || data.is_empty() {
        return None;
    }
    Some((meta.to_owned(), data.to_owned()))
}

/// Usage from OpenRouter's chat-completions shape (upstream `parseUsage`).
/// Cost fields are left zero; the call site prices from the catalog model.
fn parse_openrouter_usage(raw: &JsonValue) -> Option<Usage> {
    if !raw.is_object() {
        return None;
    }
    let prompt_tokens = raw
        .get("prompt_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let details = raw.get("prompt_tokens_details");
    let reported_cached = details
        .and_then(|details| details.get("cached_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let cache_write = details
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let cache_read = if cache_write > 0 {
        reported_cached.saturating_sub(cache_write)
    } else {
        reported_cached
    };
    let input = prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let output = raw
        .get("completion_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: Default::default(),
    };
    Some(usage)
}

/// One OpenRouter image generation over the chat-completions endpoint
/// (upstream `generateImages`).
async fn generate_openrouter_images(
    model: &ImageModel,
    context: &ImagesContext,
    options: &ImagesRequestOptions,
    signal: Option<&crate::ai::types::AbortSignal>,
) -> AssistantImages {
    let mut result = AssistantImages {
        api: model.api,
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: unix_millis(),
    };
    let Some(api_key) = resolve_api_key_with_env(
        &model.provider,
        options.api_key.as_deref(),
        options.env.as_ref(),
    )
    .filter(|key| !key.is_empty()) else {
        // Upstream treats an empty key as missing (`if (!apiKey)`), so a
        // script passing `apiKey: ""` fails here instead of on the wire.
        return error_result(
            &mut result,
            options_aborted(signal),
            format!("No API key for provider: {}", model.provider),
        );
    };
    let (url, mut headers, body) = match build_openrouter_images_request(model, context, &api_key) {
        Ok(request) => request,
        Err(error) => {
            return error_result(&mut result, options_aborted(signal), error.user_message());
        }
    };
    // Caller headers override model defaults (upstream merges
    // options.headers last).
    for (name, value) in &options.headers {
        headers.insert(name.clone(), value.clone());
    }

    let client = match request_client(
        options.timeout_ms,
        options.max_retries,
        options.max_retry_delay_ms,
    ) {
        Ok(client) => client,
        Err(error) => {
            return error_result(&mut result, options_aborted(signal), error.user_message());
        }
    };
    let mut received = Vec::new();
    let mut on_chunk = |chunk: &[u8]| {
        received.extend_from_slice(chunk);
        Ok(())
    };
    // Utility HTTP (not an AI provider request): no transport hooks.
    let no_hooks = crate::ai::types::ProviderStreamHooks::default();
    let send = client.post_json_streaming(&url, &headers, &body, signal, &no_hooks, &mut on_chunk);
    match send.await {
        Ok((status, _headers)) => {
            // post_json_streaming only returns Ok for 2xx; a non-2xx arrives
            // as a NonRetryable error below. Defensive status check anyway.
            if !(200..300).contains(&status) {
                return error_result(
                    &mut result,
                    options_aborted(signal),
                    format!("OpenRouter images returned HTTP {status}"),
                );
            }
        }
        Err(error) => {
            let aborted = matches!(error, AiError::Aborted);
            return error_result(
                &mut result,
                aborted || options_aborted(signal),
                error.user_message(),
            );
        }
    }
    let parsed: JsonValue = match serde_json::from_slice(&received) {
        Ok(parsed) => parsed,
        Err(error) => {
            return error_result(
                &mut result,
                options_aborted(signal),
                format!("OpenRouter images returned an unreadable response: {error}"),
            );
        }
    };
    match parse_openrouter_images_response(&parsed) {
        Ok((output, response_id, mut usage)) => {
            result.output = output;
            result.response_id = response_id;
            if let Some(usage) = usage.as_mut() {
                price_usage(usage, model.cost.rates.input, model.cost.rates.output);
            }
            result.usage = usage;
            result
        }
        Err(error) => error_result(&mut result, options_aborted(signal), error.user_message()),
    }
}

/// Price a parsed usage record from the model's catalog rates (upstream
/// `parseUsage` cost block; per-million rates).
pub(crate) fn price_usage(usage: &mut Usage, input_rate: f64, output_rate: f64) {
    usage.cost.input = input_rate / 1_000_000.0 * usage.input as f64;
    usage.cost.output = output_rate / 1_000_000.0 * usage.output as f64;
    usage.cost.total = usage.cost.input + usage.cost.output;
    // Catalog-derived, never provider-billed (upstream openrouter-images
    // reports `totalFromProvider: false`).
    usage.cost.total_from_provider = Some(false);
}

fn options_aborted(signal: Option<&crate::ai::types::AbortSignal>) -> bool {
    signal.is_some_and(|signal| signal.is_aborted())
}

/// Fill `result` as a failure (upstream's catch block) and return it.
fn error_result(result: &mut AssistantImages, aborted: bool, message: String) -> AssistantImages {
    result.stop_reason = if aborted {
        ImagesStopReason::Aborted
    } else {
        ImagesStopReason::Error
    };
    result.error_message = Some(message);
    result.clone()
}

/// Unix timestamp in milliseconds (`Date.now()`).
pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
