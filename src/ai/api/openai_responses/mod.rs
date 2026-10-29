//! OpenAI Responses API adapters: `openai-responses`, `openai-codex-responses`
//! (ChatGPT account endpoint) and `azure-openai-responses` (deployment-based
//! endpoint), all sharing the wire codec in [`shared`].
//!
//! The three providers and their wire modules (`azure`/`codex`/`copilot`/
//! `responses_compat`) ride `provider-openai`. `shared` stays core — other
//! adapters and the TUI use its partial-JSON reader — with only the
//! openai-dependent pieces inside it (`build_responses_params`) gated.

#[cfg(feature = "provider-openai")]
pub mod azure;
#[cfg(feature = "provider-openai")]
pub mod codex;
#[cfg(feature = "provider-openai")]
pub mod copilot;
#[cfg(feature = "provider-openai")]
pub mod responses_compat;
pub mod shared;

#[cfg(feature = "provider-openai")]
pub use azure::{
    azure_responses_url, normalize_azure_base_url, resolve_azure_base_url, resolve_deployment_name,
};
#[cfg(feature = "provider-openai")]
pub use codex::{build_codex_body, extract_account_id, resolve_codex_url};
#[cfg(feature = "provider-openai")]
pub use copilot::{
    build_copilot_dynamic_headers, has_copilot_vision_input, infer_copilot_initiator,
    is_github_copilot_model,
};
#[cfg(feature = "provider-openai")]
pub use responses_compat::ResponsesCompat;
#[cfg(feature = "provider-openai")]
pub use shared::build_responses_params;
pub use shared::{
    convert_responses_messages, convert_responses_tools, convert_responses_tools_with,
    get_responses_tool_support, parse_responses_sse, resolve_responses_request_tools,
    ResponsesSseParser, ResponsesToolSupport, ResponsesTranscriptTools,
    OPENAI_RESPONSES_MIN_OUTPUT_TOKENS,
};

#[cfg(feature = "provider-openai")]
use std::sync::Arc;

use serde_json::Value;

#[cfg(feature = "provider-openai")]
use crate::ai::auth::ApiKeyAuth;
#[cfg(feature = "provider-openai")]
use crate::ai::error::{AiError, AiResult};
#[cfg(feature = "provider-openai")]
use crate::ai::event_stream::AssistantMessageEventStream;
#[cfg(feature = "provider-openai")]
use crate::ai::provider::Provider;
#[cfg(feature = "provider-openai")]
use crate::ai::transport::ProviderRequestClient;
#[cfg(feature = "provider-openai")]
use crate::ai::types::*;

// ---------------------------------------------------------------------------
// Shared streaming plumbing
// ---------------------------------------------------------------------------

/// Wire a parse result into the sink, mapping parser failures onto the error
/// termination protocol (mirrors the other pir adapters). Producer
/// backpressure applies (`push_await`): a full event queue parks this flush
/// rather than growing the queue.
#[cfg(feature = "provider-openai")]
async fn emit_parser_output(
    sink: &AssistantMessageEventStream,
    ready: Vec<AssistantMessageEvent>,
) -> AiResult<()> {
    for event in ready {
        if !sink.push_await(event).await {
            // The consumer abandoned the stream: stop forwarding instead of
            // parking/dropping the rest of the batch.
            return Ok(());
        }
    }
    Ok(())
}

/// Run one streaming request: POST the JSON body, feed the response body into
/// the incremental parser, then flush and finish.
#[cfg(feature = "provider-openai")]
async fn stream_responses(
    client: &ProviderRequestClient,
    url: &str,
    headers: &ProviderHeaders,
    body: Value,
    options: &StreamOptions,
    parser: &mut ResponsesSseParser,
    sink: &AssistantMessageEventStream,
) -> AiResult<()> {
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
    let payload = serde_json::to_vec(&body)?;
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
            .request_streaming_with(
                reqwest::Method::POST,
                url,
                headers,
                payload,
                options.signal.as_ref(),
                &options.hooks,
                &mut on_chunk,
            )
            .await?
    };
    // Upstream fires `onResponse` when the HTTP response arrives, before the
    // body is consumed (openai-responses.ts:191, openai-codex-responses.ts:414,
    // azure-openai-responses.ts:94); pir's transport only surfaces the status
    // and headers once streaming completes, so the hook runs there.
    if let Some(on_response) = &options.hooks.on_response {
        on_response(status, &response_headers);
    }
    let mut tail = Vec::new();
    parser.finish(&mut tail)?;
    emit_parser_output(sink, tail).await
}

/// Upstream `hasHeader`: true when `headers` carries a non-empty value for
/// `name` (case-insensitive). Public for tests/.
#[cfg(feature = "provider-openai")]
pub fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    let Some(headers) = headers else {
        return false;
    };
    let expected = name.to_ascii_lowercase();
    headers
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case(&expected) && !value.trim().is_empty())
}

/// Resolve the effective api key (upstream `getClientApiKey`): an explicit
/// key wins; without one, a caller-supplied `authorization` or
/// `cf-aig-authorization` header means the credential rides in the headers
/// (Cloudflare AI Gateway) and a `"unused"` placeholder keeps the request
/// shape; anything else fails. Public for tests/.
#[cfg(feature = "provider-openai")]
pub fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> AiResult<String> {
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        return Ok(key.to_string());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    Err(AiError::Authentication(format!(
        "No API key for provider: {provider}"
    )))
}

/// Whether a model routes through the Cloudflare AI Gateway (upstream
/// `cloudflare-ai-gateway` provider; the host check also covers dynamically
/// configured gateway base URLs).
#[cfg(feature = "provider-openai")]
fn is_cloudflare_ai_gateway_model(model: &Model) -> bool {
    model.provider == "cloudflare-ai-gateway"
        || model.base_url.contains("gateway.ai.cloudflare.com")
}

#[cfg(feature = "provider-openai")]
fn base_headers(model: &Model, options: &StreamOptions) -> ProviderHeaders {
    // Upstream `createClient` (openai-responses.ts:267-291): defaults first,
    // then model headers, then caller (request) headers last so they can
    // override everything.
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
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    headers
}

/// Spawn the stream task with the shared error-termination protocol.
///
/// The terminal classification rides the shared
/// [`terminal_error_classification`](crate::ai::api::terminal_error_classification):
/// the abort SIGNAL decides between `aborted` and `error`, exactly like the
/// `finish_provider_stream` seam the other adapters use (upstream
/// `output.stopReason = options?.signal?.aborted ? "aborted" : "error"`). The
/// error VALUE alone is not enough: a body read in flight when the caller
/// aborts surfaces whatever the transport/parser yields at that instant.
#[cfg(feature = "provider-openai")]
fn spawn_stream<F>(
    model: Model,
    sink: &AssistantMessageEventStream,
    error_prefix: &'static str,
    signal: Option<AbortSignal>,
    body: F,
) where
    F: FnOnce(
            AssistantMessageEventStream,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AiResult<()>> + Send>>
        + Send
        + 'static,
{
    // Producer handle, not a consumer clone: dropping the last consumer must
    // be observable to the stream task (`push_await` then returns `false`).
    let events = sink.producer();
    tokio::spawn(async move {
        let result = body(events.clone()).await;
        if let Err(error) = result {
            // An abandoned stream has no reader left: stopping is a normal
            // abandon, not an error worth reporting.
            if events.is_abandoned() {
                events.end(None);
                return;
            }
            let message = crate::ai::api::prefixed_terminal_error_message(
                &model,
                &error,
                error_prefix,
                signal.as_ref(),
            );
            let (reason, _) =
                crate::ai::api::terminal_error_classification(&error, signal.as_ref());
            // The payload carries the same classification as the event; the
            // registry forwarding loop relies on both agreeing. The message
            // also carries the structural retry verdict the agent loop takes
            // (prefixed text cannot hide it).
            events.push(AssistantMessageEvent::Error {
                reason,
                error: message,
            });
        }
        if !events.is_done() {
            events.end(None);
        }
    });
}

// ---------------------------------------------------------------------------
// openai-responses
// ---------------------------------------------------------------------------

/// OpenAI `/v1/responses` provider (upstream `openai-responses.ts`).
#[cfg(feature = "provider-openai")]
#[derive(Clone)]
pub struct OpenAiResponsesProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
}

#[cfg(feature = "provider-openai")]
impl OpenAiResponsesProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "OpenAI".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            api_key_env: vec!["OPENAI_API_KEY".into()],
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

#[cfg(feature = "provider-openai")]
impl Provider for OpenAiResponsesProvider {
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
        let client = self.client.clone();
        let model = model.clone();
        let signal = options.signal.clone();
        spawn_stream(
            model.clone(),
            &output,
            "OpenAI API error: ",
            signal,
            move |sink| {
                Box::pin(async move {
                    let mut client = client.clone();
                    crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
                    // Cloudflare AI Gateway endpoint placeholders (upstream
                    // `resolveCloudflareModel`): materialize the account/gateway
                    // ids from the request env before anything reads the URL.
                    let model = match options.env.as_ref() {
                        Some(env) => {
                            let base_url =
                                crate::ai::auth::resolve_cloudflare_base_url(&model.base_url, env);
                            if base_url == model.base_url {
                                model
                            } else {
                                Model { base_url, ..model }
                            }
                        }
                        None => model,
                    };
                    // Cloudflare AI Gateway credential (upstream
                    // `cloudflareAIGatewayAuth.resolve`): the key resolves from
                    // the credential/env into a `cf-aig-authorization` header,
                    // with `Authorization`/`x-api-key` cleared — pir never emits
                    // those on this path, so only the gateway header goes out.
                    let mut gateway_env_headers = ProviderHeaders::new();
                    if is_cloudflare_ai_gateway_model(&model)
                        && !has_header(options.headers.as_ref(), "cf-aig-authorization")
                    {
                        if let Some(env) = options.env.as_ref() {
                            let ctx = crate::ai::auth::MapAuthContext::new(env.clone());
                            if let Ok(Some(auth)) =
                                crate::ai::auth::cloudflare_ai_gateway_auth(&ctx, None)
                            {
                                if let Some(headers) = auth.auth.headers {
                                    gateway_env_headers = headers;
                                }
                            }
                        }
                    }
                    let compat = ResponsesCompat::from_model(&model);
                    let api_key = if gateway_env_headers.contains_key("cf-aig-authorization") {
                        // The credential rides in the gateway header (upstream
                        // `getClientApiKey` sees the merged auth headers and uses
                        // the "unused" placeholder).
                        "unused".to_string()
                    } else {
                        get_client_api_key(
                            &model.provider,
                            options.api_key.as_deref(),
                            options.headers.as_ref(),
                        )?
                    };
                    // Sign in with ChatGPT access tokens reject these fields.
                    let omit = shared::is_chatgpt_sign_in(&model, Some(&api_key));
                    // Cache-preserving thinking level changes (upstream
                    // `getConfigurationUpdateEffort`): only when the model opts in,
                    // the caller requested a non-off level, and the credential is
                    // not Sign in with ChatGPT (which rejects the items).
                    let requested_level = options
                        .reasoning
                        .filter(|level| !matches!(level, ThinkingLevel::Off));
                    let configuration_update_effort = if compat.supports_mid_convo_effort
                        && !shared::is_chatgpt_sign_in(&model, options.api_key.as_deref())
                    {
                        requested_level
                            .map(|level| shared::responses_effort_for_level(&model, level))
                    } else {
                        None
                    };
                    let mut body = shared::build_responses_params(
                        &model,
                        &context,
                        &options,
                        &compat,
                        &model.id,
                        shared::OPENAI_TOOL_CALL_PROVIDERS,
                        omit,
                        configuration_update_effort.as_deref(),
                    )?;
                    // Upstream `options.onPayload` (openai-responses.ts:174):
                    // inspect (or replace, by mutating in place) the request params
                    // before they are sent.
                    if let Some(on_payload) = &options.hooks.on_payload {
                        on_payload(&mut body);
                    }
                    let headers = assemble_request_headers(
                        &model,
                        &context,
                        &options,
                        &compat,
                        &api_key,
                        gateway_env_headers,
                    );
                    let url = format!("{}/responses", model.base_url.trim_end_matches('/'));
                    let mut parser = ResponsesSseParser::new(&model)
                        .with_service_tier(options.service_tier.clone())
                        .with_provider_event_hook(options.hooks.on_provider_stream_event.clone());
                    if let Some(effort) = &configuration_update_effort {
                        parser = parser.with_provider_thinking_level(effort.clone());
                    }
                    stream_responses(&client, &url, &headers, body, &options, &mut parser, &sink)
                        .await
                })
            },
        );
        output
    }
}

/// Assemble the request headers for one `openai-responses` request (upstream
/// `createClient` header assembly): defaults, Copilot dynamic headers, env-
/// resolved gateway headers, the bearer credential, then session-affinity
/// headers. Pure so tests can assert the wire headers without a request.
/// Public for tests/.
#[cfg(feature = "provider-openai")]
pub fn assemble_request_headers(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &ResponsesCompat,
    api_key: &str,
    gateway_env_headers: ProviderHeaders,
) -> ProviderHeaders {
    let mut headers = base_headers(model, options);
    // GitHub Copilot dynamic headers (upstream `createClient`): initiator /
    // intent always, vision marker when images ride along. Inserted over the
    // defaults but under explicit request headers, mirroring the upstream
    // merge order.
    if copilot::is_github_copilot_model(model) {
        let has_images = copilot::has_copilot_vision_input(&context.messages);
        for (name, value) in copilot::build_copilot_dynamic_headers(&context.messages, has_images) {
            headers.entry(name).or_insert(value);
        }
    }
    // SDK bearer credential (upstream `new OpenAI({ apiKey })`). A
    // caller-supplied `authorization` header wins over it, and a gateway
    // `cf-aig-authorization` header replaces it entirely (upstream
    // cloudflare-auth.ts resolves the gateway credential into
    // `cf-aig-authorization` and nulls `Authorization`/`x-api-key`).
    for (name, value) in gateway_env_headers {
        headers.entry(name).or_insert(value);
    }
    let gateway_auth = headers.contains_key("cf-aig-authorization");
    let caller_authorization = has_header(Some(&headers), "authorization");
    if !caller_authorization && !gateway_auth {
        headers.insert("authorization".into(), format!("Bearer {api_key}"));
    }
    // Sign in with ChatGPT access tokens reject the session fields.
    let omit = shared::is_chatgpt_sign_in(model, Some(api_key));
    if !omit {
        // Session affinity (upstream `createClient`): the header set follows
        // the compat `sessionAffinityFormat`, gated on `cacheRetention` like
        // the cache key.
        let cache_session = if options.cache_retention == Some(CacheRetention::None) {
            None
        } else {
            options.session_id.clone()
        };
        if let Some(session_id) = cache_session {
            match compat.session_affinity_format {
                SessionAffinityFormat::Openrouter => {
                    headers
                        .entry("x-session-id".into())
                        .or_insert_with(|| session_id.clone());
                }
                SessionAffinityFormat::Openai => {
                    headers
                        .entry("session_id".into())
                        .or_insert_with(|| session_id.clone());
                    headers
                        .entry("x-client-request-id".into())
                        .or_insert_with(|| session_id.clone());
                }
                SessionAffinityFormat::OpenaiNosession => {
                    headers
                        .entry("x-client-request-id".into())
                        .or_insert_with(|| session_id.clone());
                }
            }
        }
    }
    headers
}

// ---------------------------------------------------------------------------
// openai-codex-responses
// ---------------------------------------------------------------------------

/// OpenAI Codex (ChatGPT account) Responses provider (upstream
/// `openai-codex-responses.ts`, SSE transport).
#[cfg(feature = "provider-openai")]
#[derive(Clone)]
pub struct OpenAiCodexResponsesProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
}

#[cfg(feature = "provider-openai")]
impl OpenAiCodexResponsesProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        let base_url = if base_url.trim().is_empty() {
            codex::DEFAULT_CODEX_BASE_URL.to_string()
        } else {
            base_url
        };
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "OpenAI Codex".into(),
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

#[cfg(feature = "provider-openai")]
impl Provider for OpenAiCodexResponsesProvider {
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
            "OpenAI Codex access token".to_string(),
            vec!["OPENAI_API_KEY".to_string()],
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
        let client = self.client.clone();
        let model = model.clone();
        let signal = options.signal.clone();
        spawn_stream(model.clone(), &output, "", signal, move |sink| {
            Box::pin(async move {
                let mut client = client.clone();
                crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
                let api_key = options.api_key.clone().ok_or_else(|| {
                    AiError::Authentication(format!("No API key for provider: {}", model.provider))
                })?;
                let account_id =
                    codex::extract_account_id(&api_key).map_err(AiError::Authentication)?;
                let cache_session = if options.cache_retention == Some(CacheRetention::None) {
                    None
                } else {
                    options.session_id.clone()
                };
                let mut body = codex::build_codex_body(&model, &context, &options)?;
                // Upstream `options.onPayload` (openai-codex-responses.ts:278)
                // applies to the codex request body.
                if let Some(on_payload) = &options.hooks.on_payload {
                    on_payload(&mut body);
                }
                let headers = codex::build_codex_headers(
                    model.headers.as_ref(),
                    options.headers.as_ref(),
                    &account_id,
                    &api_key,
                    cache_session.as_deref(),
                );
                let url = codex::resolve_codex_url(&model.base_url);
                // WebSocket-first transport (pooled, continuation-cached)
                // with the SSE path as its fallback; see codex::codex_ws.
                let result = codex::stream_codex_responses(
                    &client,
                    &model,
                    &options,
                    &url,
                    &headers,
                    body,
                    &account_id,
                    &api_key,
                    &sink,
                )
                .await;
                // Upstream `parseErrorResponse` (openai-codex-responses.ts:440):
                // a failed codex HTTP response (429 usage limits included) is
                // parsed into the friendly ChatGPT message before it surfaces.
                result.map_err(|error| codex::enhance_codex_error(error, crate::ai::util::now_ms()))
            })
        });
        output
    }
}

// ---------------------------------------------------------------------------
// azure-openai-responses
// ---------------------------------------------------------------------------

/// Azure OpenAI Responses provider (upstream `azure-openai-responses.ts`).
#[cfg(feature = "provider-openai")]
#[derive(Clone)]
pub struct AzureOpenAiResponsesProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
}

#[cfg(feature = "provider-openai")]
impl AzureOpenAiResponsesProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Azure".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            api_key_env: vec!["AZURE_OPENAI_API_KEY".into()],
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

#[cfg(feature = "provider-openai")]
impl Provider for AzureOpenAiResponsesProvider {
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
        let client = self.client.clone();
        let model = model.clone();
        spawn_stream(
            model.clone(),
            &output,
            "Azure OpenAI API error: ",
            options.signal.clone(),
            move |sink| {
                Box::pin(async move {
                    let mut client = client.clone();
                    crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
                    let api_key = options.api_key.clone().ok_or_else(|| {
                        AiError::Authentication(format!(
                            "No API key for provider: {}",
                            model.provider
                        ))
                    })?;
                    let mut body = azure::build_azure_params(&model, &context, &options)?;
                    // Upstream `options.onPayload` (azure-openai-responses.ts:77):
                    // inspect (or replace, by mutating in place) the request
                    // params before they are sent.
                    if let Some(on_payload) = &options.hooks.on_payload {
                        on_payload(&mut body);
                    }
                    let url = azure::azure_responses_url_with_options(
                        &model,
                        options.env.as_ref(),
                        options.azure.as_ref(),
                    )
                    .map_err(AiError::NonRetryable)?;
                    let mut headers = base_headers(&model, &options);
                    headers.insert("api-key".into(), api_key);
                    let mut parser = ResponsesSseParser::new(&model)
                        .with_provider_event_hook(options.hooks.on_provider_stream_event.clone());
                    stream_responses(&client, &url, &headers, body, &options, &mut parser, &sink)
                        .await
                })
            },
        );
        output
    }
}
