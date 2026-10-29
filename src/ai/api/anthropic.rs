//! Anthropic Messages API adapter.
//!
//! Rust equivalent of upstream `packages/ai/src/api/anthropic-messages.ts`
//! (subset relevant to pir): request building with extended/adaptive thinking,
//! thinking replay (including redacted blocks), mid-conversation inline tools
//! via `tool_addition` blocks under the `inline-tools-2026-09-15` beta, strict
//! tool schemas with unsupported-keyword fallback plus the retry-once
//! preferred-strict-tool downgrade for strict-tool capacity rejections
//! (upstream `d4d53612b`), and SSE parsing with thinking/redacted-thinking
//! content blocks.
use crate::ai::api::constrained_sampling::{
    anthropic_strict_unsupported_keyword, get_json_schema_tool_parameters, make_strict_json_schema,
    resolve_json_schema_strict_sampling,
};
use crate::ai::api::transform::{split_error_tool_result_images, transform_messages_lazy};
use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Stealth mode: mimic Claude Code's client identity on OAuth tokens
/// (upstream `claudeCodeVersion`).
pub const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_SYSTEM_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";
/// Server-side model fallbacks (upstream `SERVER_SIDE_FALLBACK_BETA`).
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Effort-only mid-conversation system messages (upstream
/// `MID_CONVERSATION_OUTPUT_CONFIG_BETA`).
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
/// Thinking block-binding controls (upstream `THINKING_BINDING_CONTROLS_BETA`;
/// also mirrored on the Bedrock path).
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Claude Code 2.x tool names (canonical casing); OAuth requests map matching
/// tool names to this casing on the wire and back on tool calls.
const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Stable deferred tool declared whenever native tool changes are in use, so
/// Anthropic's hidden mid-conversation tool scaffolding stays in the cached
/// prefix (upstream `DEFERRED_TOOL_PLACEHOLDER`).
fn deferred_tool_placeholder() -> Value {
    json!({
        "name": "__pi_deferred_placeholder__",
        "description": "Reserved placeholder. Never available. Never call this.",
        "input_schema": { "type": "object", "properties": {}, "required": [] },
        "defer_loading": true,
    })
}

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
        .map(|tool| (*tool).to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    if !tools.is_empty() {
        if let Some(tool) = tools
            .iter()
            .find(|tool| tool.name.eq_ignore_ascii_case(name))
        {
            return tool.name.clone();
        }
    }
    name.to_string()
}

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

// ---------------------------------------------------------------------------
// Request credential resolution (Claude Code environment compatibility)
// ---------------------------------------------------------------------------

/// The wire header an Anthropic credential uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicAuthKind {
    /// `x-api-key` (a regular Anthropic API key).
    ApiKey,
    /// `Authorization: Bearer` (OAuth tokens and Claude Code's
    /// `ANTHROPIC_AUTH_TOKEN` gateway tokens).
    Bearer,
}

#[derive(Debug, Clone)]
struct AnthropicRequestAuth {
    value: String,
    kind: AnthropicAuthKind,
}

impl AnthropicRequestAuth {
    /// Claude Code's stealth mode applies only to OAuth access tokens (the
    /// token content check is the same one upstream uses).
    fn is_oauth(&self) -> bool {
        self.kind == AnthropicAuthKind::Bearer && is_oauth_token(&self.value)
    }
}

/// Read a process environment variable, treating empty/whitespace-only values
/// as unset (the same rule as [`crate::ai::auth::EnvAuthContext`]).
fn auth_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// True when the model or request explicitly configures an authentication
/// header. Those headers are applied after the resolved credential and must
/// win over the ambient environment fallback.
fn explicit_auth_header(model: &Model, options: &StreamOptions) -> bool {
    model
        .headers
        .iter()
        .chain(options.headers.iter())
        .flatten()
        .any(|(name, _)| {
            name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
        })
}

/// Resolve the credential for one Anthropic Messages request.
///
/// Precedence (Claude Code compatibility, documented in `docs/models.md`):
/// 1. an explicit request/model credential: `options.api_key` (a models.json
///    `apiKey`, `--api-key`, or a stored credential) is sent as `x-api-key`,
///    OAuth tokens as `Authorization: Bearer`;
/// 2. `ANTHROPIC_AUTH_TOKEN`, then `ANTHROPIC_OAUTH_TOKEN` as
///    `Authorization: Bearer`;
/// 3. `ANTHROPIC_API_KEY` as `x-api-key`.
///
/// Explicit `authorization`/`x-api-key` headers on the model or request
/// suppress the environment fallback entirely (`None`), because those headers
/// are applied later and win.
fn resolve_request_auth(model: &Model, options: &StreamOptions) -> Option<AnthropicRequestAuth> {
    if let Some(key) = options
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        return Some(AnthropicRequestAuth {
            value: key.to_string(),
            kind: if is_oauth_token(key) {
                AnthropicAuthKind::Bearer
            } else {
                AnthropicAuthKind::ApiKey
            },
        });
    }
    if explicit_auth_header(model, options) {
        return None;
    }
    if let Some(token) =
        auth_env("ANTHROPIC_AUTH_TOKEN").or_else(|| auth_env("ANTHROPIC_OAUTH_TOKEN"))
    {
        return Some(AnthropicRequestAuth {
            value: token,
            kind: AnthropicAuthKind::Bearer,
        });
    }
    auth_env("ANTHROPIC_API_KEY").map(|key| AnthropicRequestAuth {
        value: key,
        kind: AnthropicAuthKind::ApiKey,
    })
}

// ---------------------------------------------------------------------------
// Compat resolution (upstream `getAnthropicCompat`)
// ---------------------------------------------------------------------------

/// Resolved Anthropic-specific compatibility knobs with upstream defaults.
#[derive(Debug, Clone, Copy)]
struct AnthropicCompat {
    supports_eager_tool_input_streaming: bool,
    supports_temperature: bool,
    allow_empty_signature: bool,
    supports_strict_tools: bool,
    force_adaptive_thinking: bool,
    supports_mid_convo_effort: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_changes: bool,
    supports_cache_control_on_tools: bool,
    has_fallback_models: bool,
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    session_affinity_format: Option<crate::ai::types::SessionAffinityFormat>,
}

fn anthropic_compat(model: &Model) -> AnthropicCompat {
    let compat = model.compat.as_ref();
    let option = |name: fn(&OpenAiCompat) -> Option<bool>, default: bool| {
        compat.and_then(name).unwrap_or(default)
    };
    AnthropicCompat {
        supports_eager_tool_input_streaming: option(
            |c| c.supports_eager_tool_input_streaming,
            true,
        ),
        supports_temperature: option(|c| c.supports_temperature, true),
        allow_empty_signature: option(|c| c.allow_empty_signature, false),
        supports_strict_tools: option(|c| c.supports_strict_tools, false),
        force_adaptive_thinking: option(|c| c.force_adaptive_thinking, false),
        supports_mid_convo_effort: option(|c| c.supports_mid_convo_effort, false),
        supports_mid_convo_system_messages: option(|c| c.supports_mid_convo_system_messages, false),
        supports_mid_convo_tool_changes: option(|c| c.supports_mid_convo_tool_changes, false),
        supports_cache_control_on_tools: option(|c| c.supports_cache_control_on_tools, true),
        has_fallback_models: compat
            .and_then(|c| c.allowed_fallback_models.as_ref())
            .is_some_and(|models| !models.is_empty()),
        supports_long_cache_retention: compat
            .and_then(|c| c.supports_long_cache_retention)
            .unwrap_or(true),
        // Upstream defaults session affinity on for OpenRouter only; an
        // explicit compat flag can enable it for other Anthropic-compatible
        // gateways too (upstream `getAnthropicCompat`).
        send_session_affinity_headers: is_openrouter(model)
            || compat.is_some_and(|c| c.send_session_affinity_headers),
        session_affinity_format: compat.and_then(|c| c.session_affinity_format).or(
            if is_openrouter(model) {
                Some(crate::ai::types::SessionAffinityFormat::Openrouter)
            } else {
                None
            },
        ),
    }
}

/// Upstream `getAnthropicCompat`'s OpenRouter detection.
fn is_openrouter(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains("openrouter.ai")
}

/// Same-provider fallback models with a catalog cost, keyed by model id
/// (upstream `allowedFallbackModels.find(fallback => fallback.provider ===
/// model.provider && fallback.model === responseModel)?.cost`). The entries
/// ride in `compat.allowedFallbackModels` as raw JSON; malformed ones are
/// ignored so a catalog typo never fails a stream.
fn allowed_fallback_costs(
    model: &Model,
) -> std::collections::BTreeMap<String, crate::ai::types::ModelCost> {
    let Some(fallbacks) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.allowed_fallback_models.as_ref())
    else {
        return std::collections::BTreeMap::new();
    };
    fallbacks
        .iter()
        .filter_map(|fallback| {
            let provider = fallback.get("provider").and_then(Value::as_str)?;
            if provider != model.provider {
                return None;
            }
            let id = fallback.get("model").and_then(Value::as_str)?;
            let cost = serde_json::from_value::<crate::ai::types::ModelCost>(
                fallback.get("cost")?.clone(),
            )
            .ok()?;
            Some((id.to_string(), cost))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AnthropicMessagesProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
}

impl AnthropicMessagesProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Anthropic".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            api_key_env: vec!["ANTHROPIC_API_KEY".into()],
        })
    }
    pub fn with_client(mut self, client: ProviderRequestClient) -> Self {
        self.client = client;
        self
    }
}

impl Provider for AnthropicMessagesProvider {
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
        vec![
            ApiKeyAuth::new("Anthropic API key", self.api_key_env.clone())
                // Claude Code gateway tokens are sent as
                // `Authorization: Bearer` instead of `x-api-key`
                // (`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_OAUTH_TOKEN`).
                .with_bearer_env_vars(
                    crate::ai::auth::env_api_keys::bearer_env_vars("anthropic")
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                )
                // Workload-identity federation fallback (upstream
                // providers/anthropic.ts:49-70), resolved after stored keys and
                // env vars.
                .with_federation(crate::ai::auth::anthropic_workload_identity_federation),
        ]
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
        // stream must be observable, otherwise this task (and the transport
        // body read loop behind it) could park in `push_await` forever.
        let events = output.producer();
        let mut client = self.client.clone();
        crate::ai::api::apply_stream_retry_overrides(&mut client, &options);
        let model = model.clone();
        tokio::spawn(async move {
            let result = anthropic_stream(&client, &model, &context, &options, &events).await;
            if events.is_abandoned() {
                // Every consumer handle is gone: stopping early is a normal
                // abandon, not an error worth reporting to anyone.
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

async fn anthropic_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> Result<(), AiError> {
    // Incremental path: parse SSE as chunks arrive and push events into the
    // stream immediately. The parser state machine is shared with the batch
    // path (`parse_anthropic_sse`).
    let request_auth = resolve_request_auth(model, options);
    if request_auth.is_none() && !explicit_auth_header(model, options) {
        // Fail before the request is built/sent instead of letting the
        // gateway answer an unauthenticated call with an opaque 401/403.
        return Err(AiError::Authentication(
            "Anthropic authentication is not configured: set ANTHROPIC_AUTH_TOKEN \
             (Authorization: Bearer), ANTHROPIC_API_KEY, or an apiKey / authentication \
             header in models.json"
                .to_string(),
        ));
    }
    let oauth = request_auth
        .as_ref()
        .is_some_and(AnthropicRequestAuth::is_oauth);
    let mut parser = AnthropicSseParser::new(model)
        .with_provider_event_hook(options.hooks.on_provider_stream_event.clone());
    // Managed-effort models record the effort this reply was generated with so
    // a later request replays it as an effort-only system marker (upstream
    // `output.providerThinkingLevel = built.providerThinkingLevel`).
    let compat = anthropic_compat(model);
    let thinking_disabled = model.reasoning
        && matches!(options.reasoning, Some(ThinkingLevel::Off))
        && !off_map_marks_unsupported(model);
    if compat.supports_mid_convo_effort && !thinking_disabled {
        parser = parser.with_provider_thinking_level(ANTHROPIC_MANAGED_EFFORT);
    }
    if oauth {
        parser = parser.with_declared_tools(context.tools.clone().unwrap_or_default());
    }
    // Set by the first delivered body chunk: a request that already started
    // streaming is never retried (`on_chunk` is only reached for 2xx bodies,
    // so this also covers mid-body read failures after the response began).
    let mut delivered = false;
    // The `Start` event is pushed once, after the first attempt's request
    // built successfully and before its body is consumed (the incremental
    // parser pushes deltas from inside the request, so `Start` must already be
    // queued). A request that fails to build never starts the stream
    // (upstream pushes `start` only after the SDK call returned a response).
    let mut start_pushed = false;
    // Retry-once loop for the Anthropic strict-tool capacity rejections
    // (upstream anthropic-messages.ts:648-689): a 400 `invalid_request_error`
    // naming one of the known strict-tool limits is retried once with the
    // `"prefer"` strict schemas downgraded to their original non-strict form.
    // `onPayload` and request construction run inside the loop, once per
    // attempt. The `Start` event keeps a pristine partial, so the retry
    // continues the same stream rather than restarting it.
    let mut disable_preferred_strict_tools = false;
    let (status, response_headers) = loop {
        if options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            return Err(AiError::Aborted);
        }
        let (mut body, headers, has_preferred_strict_tools) =
            build_anthropic_request_with_strict_sampling(
                model,
                context,
                options,
                disable_preferred_strict_tools,
            )?;
        if !start_pushed {
            // `false`: every consumer handle is gone. Do not issue a request
            // whose events nobody can read.
            if !sink
                .push_await(AssistantMessageEvent::Start {
                    partial: parser.snapshot(),
                })
                .await
            {
                return Ok(());
            }
            start_pushed = true;
        }
        // Upstream `options.onPayload` runs once per attempt, before the
        // request is sent (anthropic-messages.ts:640-643/652-656), and the
        // resolved params always force `stream: true`
        // (`{ ...nextParams, stream: true }`, anthropic-messages.ts:655-657):
        // pir's hook mutates in place, so the forced flag is re-applied here.
        if let Some(on_payload) = &options.hooks.on_payload {
            on_payload(&mut body);
        }
        body["stream"] = json!(true);
        let outcome = {
            // Fresh chunk sink per attempt: it borrows the parser mutably, and
            // the borrow must end before a retry inspects the parser
            // (diagnostic) and resends.
            let mut on_chunk = crate::ai::api::EventChunkSink::new(
                |chunk: &[u8], ready: &mut Vec<AssistantMessageEvent>| {
                    // Every consumer handle gone: stop parsing at the next
                    // chunk. The `Err` closes the HTTP response through the
                    // transport's chunk-sink contract, and the spawn site
                    // reports it as a normal abandon (never as an error).
                    if sink.is_abandoned() {
                        return Err(AiError::Aborted);
                    }
                    delivered = true;
                    parser.feed(chunk, ready)
                },
                sink,
            );
            client
                .post_json_streaming_with(
                    &format!("{}/v1/messages", model.base_url.trim_end_matches('/')),
                    &headers,
                    &body,
                    options.signal.as_ref(),
                    &options.hooks,
                    &mut on_chunk,
                )
                .await
        };
        match outcome {
            Ok(response) => break response,
            Err(error) => {
                // Only downgrade a rejected request, never an already-started
                // stream. A caller abort returns the abort error directly so
                // the stream reports the aborted reason (pir's error channel
                // carries this; upstream rethrows the provider error and maps
                // the reason from the signal).
                if options
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                {
                    return Err(AiError::Aborted);
                }
                if delivered
                    || disable_preferred_strict_tools
                    || !has_preferred_strict_tools
                    || !is_anthropic_strict_tools_limit_error(&error)
                {
                    return Err(error);
                }
                disable_preferred_strict_tools = true;
                Arc::make_mut(&mut parser.message)
                    .diagnostics
                    .push(strict_tools_fallback_diagnostic(&error));
            }
        }
    };
    // Upstream fires `onResponse` right after the HTTP response arrives
    // (anthropic-messages.ts:690), before the body is consumed; pir's
    // transport only surfaces the status and headers once streaming
    // completes, so the hook runs there.
    if let Some(on_response) = &options.hooks.on_response {
        on_response(status, &response_headers);
    }
    let mut tail = Vec::new();
    parser.finish(&mut tail)?;
    for event in tail {
        // `false`: the consumer abandoned the stream; drop the remaining
        // parser events instead of parking here.
        if !sink.push_await(event).await {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Preferred-strict-tool fallback (upstream d4d53612b / anthropic-messages.ts)
// ---------------------------------------------------------------------------

/// The Anthropic rejections that mean the preferred strict tool set exceeded
/// a server-side capacity limit (upstream message list,
/// anthropic-messages.ts:670-674).
const ANTHROPIC_STRICT_TOOL_LIMIT_MESSAGES: [&str; 3] = [
    "The compiled grammar is too large",
    "Too many strict tools (",
    "Schemas contains too many parameters with union types (",
];

/// True when `error` is the shared transport's `NonRetryable("HTTP {status}:
/// {body}")` for an Anthropic `400 invalid_request_error` carrying one of the
/// known strict-tool capacity rejections (upstream `error instanceof
/// Anthropic.APIError && error.status === 400 && error.type ===
/// "invalid_request_error"` plus the message list). Anything else — including
/// an SSE-level `event: error` frame, which carries the same JSON shape but no
/// HTTP status prefix — is not eligible.
fn is_anthropic_strict_tools_limit_error(error: &AiError) -> bool {
    let AiError::NonRetryable(message) = error else {
        return false;
    };
    let Some(body) = message.strip_prefix("HTTP 400: ") else {
        return false;
    };
    let parsed: Option<Value> = body
        .find('{')
        .map(|start| &body[start..])
        .and_then(|json| serde_json::from_str(json).ok());
    match parsed {
        Some(value) => {
            let error_object = value.get("error");
            let error_type = error_object
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str);
            let error_message = error_object
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            error_type == Some("invalid_request_error")
                && ANTHROPIC_STRICT_TOOL_LIMIT_MESSAGES
                    .iter()
                    .any(|fragment| error_message.contains(fragment))
        }
        // The transport caps error bodies at 500 chars, so a truncated body
        // cannot be parsed; fall back to scanning the raw text for the error
        // type and a known message fragment.
        None => {
            body.contains("invalid_request_error")
                && ANTHROPIC_STRICT_TOOL_LIMIT_MESSAGES
                    .iter()
                    .any(|fragment| body.contains(fragment))
        }
    }
}

/// One retry diagnostic for the strict-tool fallback (upstream
/// `appendAssistantMessageDiagnostic(output, {type:
/// "anthropic_strict_tools_fallback", error: {message}, details: {reason:
/// "strict_tool_limits_exceeded"}})`). The message is the transport's
/// `NonRetryable` text (upstream carries the SDK error's `message`).
fn strict_tools_fallback_diagnostic(error: &AiError) -> DiagnosticInfo {
    let mut details = BTreeMap::new();
    details.insert(
        "reason".to_string(),
        Value::String("strict_tool_limits_exceeded".to_string()),
    );
    DiagnosticInfo {
        diag_type: "anthropic_strict_tools_fallback".to_string(),
        timestamp: crate::ai::util::now_ms(),
        error_message: Some(error.to_string()),
        error_name: None,
        error_stack: None,
        error_code: None,
        details: Some(details),
    }
}

/// Per-request strict-sampling state (upstream `buildParams`'s
/// `disablePreferredStrictTools` argument plus the `hasPreferredStrictTools`
/// flag accumulated by its `convertToolDefinitions` closure): one shared
/// accumulator covers the top-level tool list and the inline `tool_addition`
/// definitions.
#[derive(Debug, Default, Clone, Copy)]
struct StrictToolSampling {
    /// The retry attempt sends `"prefer"` tools with their original
    /// non-strict schemas.
    disable_preferred_strict_tools: bool,
    /// At least one converted definition went out strict because of a
    /// `"prefer"` config, so the request is eligible for the fallback retry.
    has_preferred_strict_tools: bool,
}

// ---------------------------------------------------------------------------
// Request building (upstream `buildParams` + `getBetaFeatures` + headers)
// ---------------------------------------------------------------------------

/// Build the full request for the Anthropic Messages API: the JSON body and
/// the request headers. Public for tests/.
pub fn build_anthropic_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<(Value, BTreeMap<String, String>)> {
    let (body, headers, _has_preferred_strict_tools) =
        build_anthropic_request_with_strict_sampling(model, context, options, false)?;
    Ok((body, headers))
}

/// [`build_anthropic_request`] plus the upstream retry state (upstream
/// `buildParams`): `disable_preferred_strict_tools` sends tools whose
/// `constrainedSampling.strict` is `"prefer"` with their original non-strict
/// schemas, and the returned flag reports whether any converted definition
/// (top-level tool list or inline `tool_addition`) went out strict because of
/// a `"prefer"` config — i.e. whether the request is eligible for the
/// retry-once fallback. Public for tests/.
pub fn build_anthropic_request_with_strict_sampling(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    disable_preferred_strict_tools: bool,
) -> AiResult<(Value, BTreeMap<String, String>, bool)> {
    let mut sampling = StrictToolSampling {
        disable_preferred_strict_tools,
        has_preferred_strict_tools: false,
    };
    let compat = anthropic_compat(model);
    let request_auth = resolve_request_auth(model, options);
    let oauth = request_auth
        .as_ref()
        .is_some_and(AnthropicRequestAuth::is_oauth);
    let requested_level = options
        .reasoning
        .filter(|level| !matches!(level, ThinkingLevel::Off));
    let thinking_enabled = model.reasoning && requested_level.is_some();

    // Prompt-cache marker (upstream getCacheControl): "short" retention (the
    // default) marks system blocks, the last tool, and the last conversation
    // message with `{type: ephemeral}`; "long" retention adds `ttl: "1h"` when
    // the model supports long retention (upstream
    // `supportsLongCacheRetention ?? true`). "none" disables all markers.
    // Retention resolves upstream-style: explicit request value first, then
    // `PI_CACHE_RETENTION=long`, then "short".
    let cache_retention = crate::ai::api::resolve_cache_retention(options);
    let cache_control = match cache_retention {
        CacheRetention::None => None,
        CacheRetention::Long if compat.supports_long_cache_retention => {
            Some(json!({"type": "ephemeral", "ttl": "1h"}))
        }
        CacheRetention::Long | CacheRetention::Short => Some(json!({"type": "ephemeral"})),
    };

    // Native tool changes keep the request-level tool list fixed and define
    // every later tool by value in a `tool_addition` block. Anthropic rejects
    // a tool list where every tool is deferred, so there must be an initial
    // active tool to anchor the placeholder.
    // WHY (borrow): the request only reads the tool definitions (an
    // emptiness probe and one conversion pass), so cloning the transcript's
    // `Vec<Tool>` — every JSON schema included — on every request was pure
    // overhead. `convert_tools` takes a slice.
    let initial_tools: &[Tool] = context.tools.as_deref().unwrap_or(&[]);
    let native_tool_changes = compat.supports_mid_convo_system_messages
        && compat.supports_mid_convo_tool_changes
        && !initial_tools.is_empty();

    let thinking_disabled = model.reasoning
        && matches!(options.reasoning, Some(ThinkingLevel::Off))
        && !off_map_marks_unsupported(model);
    // Effort cannot change between messages while thinking is disabled, so
    // "off" sends no effort markers (upstream `effortMessages`).
    let effort_messages = compat.supports_mid_convo_effort && !thinking_disabled;

    let (mut messages, system_prompt_tail, assistant_levels) = convert_messages(
        model,
        context,
        options,
        oauth,
        native_tool_changes,
        effort_messages.then_some(model.provider.as_str()),
        &mut sampling,
    )?;

    // Cache breakpoint on the last block of the last conversation message
    // (upstream convertMessages tail: the last message when it is a `user` or
    // `system` role, which covers the held mid-conversation system block that
    // native tool changes emit). Applied BEFORE the effort markers are
    // inserted, exactly like upstream (`insertThinkingLevelMessages` runs on
    // the already-marked list).
    if let Some(cache_control) = &cache_control {
        if let Some(last_message) = messages.last_mut() {
            if last_message
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| role == "user" || role == "system")
            {
                if let Some(last_block) = last_message
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .and_then(|blocks| blocks.last_mut())
                {
                    if last_block
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| {
                            matches!(
                                kind,
                                "text" | "image" | "tool_result" | "tool_addition" | "tool_removal"
                            )
                        })
                    {
                        last_block["cache_control"] = cache_control.clone();
                    }
                }
            }
        }
    }

    // Per-message effort markers (upstream `insertThinkingLevelMessages`):
    // managed-effort models replay each assistant reply's recorded effort as
    // an effort-only system message before it, and pin the active effort in a
    // trailing one, so the thinking level can change mid-conversation without
    // invalidating the prompt cache.
    if effort_messages {
        let active_effort = ANTHROPIC_MANAGED_EFFORT;
        let levels: BTreeMap<usize, String> = assistant_levels.into_iter().collect();
        let mut with_markers: Vec<Value> = Vec::with_capacity(messages.len() + levels.len() + 1);
        for (index, message) in messages.into_iter().enumerate() {
            if let Some(effort) = levels.get(&index) {
                with_markers.push(json!({
                    "role": "system",
                    "content": [],
                    "output_config": {"effort": effort},
                }));
            }
            with_markers.push(message);
        }
        with_markers.push(json!({
            "role": "system",
            "content": [],
            "output_config": {"effort": active_effort},
        }));
        messages = with_markers;
    }

    // Upstream `getBetaFeatures` consults the current tool list; the
    // fine-grained-tool-streaming beta only applies when tools are present.
    let has_tools = native_tool_changes || !initial_tools.is_empty();
    let betas = beta_features(
        model,
        options,
        oauth,
        thinking_enabled,
        has_tools,
        native_tool_changes,
    );

    let (thinking, output_config, max_tokens) =
        thinking_request_config(model, options, requested_level, &compat)?;

    let mut body = json!({
        "model": model.id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });
    if !betas.is_empty() {
        body["betas"] = json!(betas);
    }

    // System prompt: OAuth tokens MUST carry the Claude Code identity first.
    let mut system_blocks: Vec<Value> = Vec::new();
    if oauth {
        let mut identity = json!({"type": "text", "text": CLAUDE_CODE_SYSTEM_TEXT});
        if let Some(cache_control) = &cache_control {
            identity["cache_control"] = cache_control.clone();
        }
        system_blocks.push(identity);
    }
    if let Some(system) = &context.system_prompt {
        if !system.trim().is_empty() {
            let mut block = json!({"type": "text", "text": system});
            if let Some(cache_control) = &cache_control {
                block["cache_control"] = cache_control.clone();
            }
            system_blocks.push(block);
        }
    }
    // Fold mid-conversation system updates into the top-level system prompt
    // when the provider does not accept them natively (upstream collapses
    // them into the leading system message).
    for tail in system_prompt_tail {
        if !tail.trim().is_empty() {
            system_blocks.push(json!({"type": "text", "text": tail}));
        }
    }
    // Cache breakpoint on the last system block (the tail of the prompt).
    if let (Some(cache_control), Some(last)) = (&cache_control, system_blocks.last_mut()) {
        last["cache_control"] = cache_control.clone();
    }
    if !system_blocks.is_empty() {
        body["system"] = Value::Array(system_blocks);
    }

    // Temperature is incompatible with extended thinking and unsupported on
    // Claude Opus 4.7+ (upstream conditions).
    if let Some(temperature) = options.temperature {
        if !thinking_enabled && !compat.supports_mid_convo_effort && compat.supports_temperature {
            body["temperature"] = json!(temperature);
        }
    }

    if native_tool_changes {
        // Initial tools stay active with the placeholder appended; the list
        // never changes afterwards — later tools are defined by value in
        // `tool_addition` blocks and withdrawn by `tool_removal`.
        let mut tools = convert_tools(
            initial_tools,
            oauth,
            compat.supports_eager_tool_input_streaming,
            compat.supports_strict_tools,
            tool_cache_control(&cache_control, compat.supports_cache_control_on_tools),
            &mut sampling,
        )?;
        tools.push(deferred_tool_placeholder());
        body["tools"] = Value::Array(tools);
    } else if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            body["tools"] = Value::Array(convert_tools(
                tools,
                oauth,
                compat.supports_eager_tool_input_streaming,
                compat.supports_strict_tools,
                tool_cache_control(&cache_control, compat.supports_cache_control_on_tools),
                &mut sampling,
            )?);
        }
    }

    // The last-conversation-message cache breakpoint was applied to
    // `messages` above, before the body was built.
    let _ = &messages;

    // Tool choice and request metadata (upstream
    // anthropic-messages.ts:1266-1279): `tool_choice` maps straight to the
    // `{type: ...}` shape and `metadata.user_id` is forwarded when present.
    if let Some(tool_choice) = options.tool_choice.as_ref() {
        body["tool_choice"] = tool_choice.anthropic_json();
    }
    if let Some(user_id) = options
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("user_id"))
    {
        body["metadata"] = json!({"user_id": user_id});
    }
    // Server-side model fallbacks (upstream `params.fallbacks =
    // allowedFallbackModels.map((fallback) => ({ model: fallback.model }))`).
    // The beta header alone is inert without this list.
    if let Some(fallbacks) = model
        .compat
        .as_ref()
        .and_then(|compat| compat.allowed_fallback_models.as_ref())
        .map(|fallbacks| {
            fallbacks
                .iter()
                .filter_map(|fallback| {
                    fallback
                        .get("model")
                        .and_then(Value::as_str)
                        .map(|model| json!({"model": model}))
                })
                .collect::<Vec<Value>>()
        })
        .filter(|fallbacks| !fallbacks.is_empty())
    {
        body["fallbacks"] = Value::Array(fallbacks);
    }

    if let Some(thinking) = thinking {
        body["thinking"] = thinking;
    }
    if let Some(output_config) = output_config {
        body["output_config"] = output_config;
    }

    // GitHub Copilot dynamic headers (upstream `createClient`'s
    // `dynamicHeaders`): initiator/intent plus the vision marker when images
    // ride along, merged over model headers but under caller headers.
    let copilot_dynamic_headers = if crate::ai::api::copilot_headers::is_github_copilot_model(model)
    {
        crate::ai::api::copilot_headers::build_copilot_dynamic_headers(
            &context.messages,
            crate::ai::api::copilot_headers::has_copilot_vision_input(&context.messages),
        )
    } else {
        BTreeMap::new()
    };
    let headers = request_headers(
        model,
        options,
        oauth,
        &betas,
        request_auth.as_ref(),
        &cache_retention,
        &compat,
        &copilot_dynamic_headers,
    );
    Ok((body, headers, sampling.has_preferred_strict_tools))
}

/// Request headers (upstream `createClient` default headers + SDK auth).
/// Caller-provided headers always win; model headers merge before them
/// (upstream `mergeClientHeaders(..., model.headers, optionsHeaders)`,
/// anthropic-messages.ts:1021-1030/1043-1051).
// The header inputs map one-to-one onto the upstream `createClient` call; a
// parameter struct would be a pure reshuffle of this single call site.
#[allow(clippy::too_many_arguments)]
fn request_headers(
    model: &Model,
    options: &StreamOptions,
    oauth: bool,
    betas: &[String],
    request_auth: Option<&AnthropicRequestAuth>,
    cache_retention: &CacheRetention,
    compat: &AnthropicCompat,
    dynamic_headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("anthropic-version".into(), "2023-06-01".into());
    headers.insert("content-type".into(), "application/json".into());
    // The SDK sends these two default headers on every request (upstream
    // `createClient` defaultHeaders); the caller's later merge can override.
    headers.insert("accept".into(), "application/json".into());
    headers.insert(
        "anthropic-dangerous-direct-browser-access".into(),
        "true".into(),
    );
    if oauth {
        if let Some(auth) = request_auth {
            headers.insert("authorization".into(), format!("Bearer {}", auth.value));
        }
        headers.insert(
            "user-agent".into(),
            format!("claude-cli/{CLAUDE_CODE_VERSION}"),
        );
        headers.insert("x-app".into(), "cli".into());
    } else {
        if let Some(auth) = request_auth {
            match auth.kind {
                AnthropicAuthKind::ApiKey => {
                    headers.insert("x-api-key".into(), auth.value.clone());
                }
                AnthropicAuthKind::Bearer => {
                    headers.insert("authorization".into(), format!("Bearer {}", auth.value));
                }
            }
        }
        headers
            .entry("user-agent".into())
            .or_insert_with(|| format!("pir ({})", std::env::consts::OS));
        // Session-affinity headers (upstream `createClient`): keyed off the
        // cache session id (the session id unless retention is "none"), and
        // only for models whose compat enables them (OpenRouter by default).
        if *cache_retention != CacheRetention::None {
            if let Some(session_id) = options.session_id.as_deref() {
                if compat.send_session_affinity_headers {
                    let header = match compat.session_affinity_format {
                        Some(crate::ai::types::SessionAffinityFormat::Openrouter) => "x-session-id",
                        _ => "x-session-affinity",
                    };
                    headers.insert(header.to_string(), session_id.to_string());
                }
            }
        }
    }
    if !betas.is_empty() {
        headers.insert("anthropic-beta".into(), betas.join(","));
    }
    for (name, value) in model.headers.iter().flatten() {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    for (name, value) in dynamic_headers {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    for (name, value) in options.headers.iter().flatten() {
        headers.insert(name.to_ascii_lowercase(), value.clone());
    }
    headers
}

/// The `cache_control` marker for tool definitions: only when prompt caching
/// is enabled and the provider accepts markers on tools (upstream
/// `toolCacheControl`).
fn tool_cache_control(cache_control: &Option<Value>, supports_on_tools: bool) -> Option<Value> {
    if supports_on_tools {
        cache_control.clone()
    } else {
        None
    }
}

/// Compose the `anthropic-beta` feature list (upstream `getBetaFeatures`).
/// An explicit caller `anthropic-beta` header replaces the defaults entirely;
/// model headers are consulted too, with request headers winning
/// (anthropic-messages.ts:1088-1092).
fn beta_features(
    model: &Model,
    options: &StreamOptions,
    oauth: bool,
    thinking_enabled: bool,
    has_tools: bool,
    native_tool_changes: bool,
) -> Vec<String> {
    let compat = anthropic_compat(model);
    // Upstream iterates model headers first and request headers last, so the
    // request header wins when both configure `anthropic-beta`.
    let mut configured: Option<&String> = None;
    for headers in [&model.headers, &options.headers] {
        for (name, value) in headers.iter().flatten() {
            if name.eq_ignore_ascii_case("anthropic-beta") {
                configured = Some(value);
            }
        }
    }
    if let Some(configured) = configured {
        if configured == "null" {
            return Vec::new();
        }
        let mut features: Vec<String> = configured
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .map(str::to_string)
            .collect();
        features.sort();
        features.dedup();
        return features;
    }

    let mut features: Vec<String> = Vec::new();
    if oauth {
        features.push("claude-code-20250219".into());
        features.push("oauth-2025-04-20".into());
    }
    if has_tools && !compat.supports_eager_tool_input_streaming {
        features.push(FINE_GRAINED_TOOL_STREAMING_BETA.into());
    }
    if model.reasoning && thinking_enabled && !compat.force_adaptive_thinking {
        // Upstream `interleavedThinking` (default true); adaptive models have
        // interleaved thinking built in, so non-adaptive ones only.
        if options.interleaved_thinking.unwrap_or(true) {
            features.push(INTERLEAVED_THINKING_BETA.into());
        }
    }
    if compat.has_fallback_models {
        features.push(SERVER_SIDE_FALLBACK_BETA.into());
    }
    if compat.supports_mid_convo_effort {
        features.push(MID_CONVERSATION_OUTPUT_CONFIG_BETA.into());
        features.push(THINKING_BINDING_CONTROLS_BETA.into());
    }
    if native_tool_changes {
        features.push(INLINE_TOOLS_BETA.into());
    }
    features.sort();
    features.dedup();
    features
}

/// Map a pi thinking level to an Anthropic adaptive effort level (upstream
/// `mapThinkingLevelToEffort`): explicit string `thinkingLevelMap` entries win,
/// otherwise minimal/low→low, medium→medium, everything else→high.
fn map_thinking_level_to_effort(model: &Model, level: Option<ThinkingLevel>) -> String {
    crate::ai::api::map_thinking_level_to_effort(model, level)
}

/// The effort managed-effort models are pinned at while
/// `configuration_update`-style markers carry the level (upstream
/// `options?.effort ?? "high"` in `buildParams`).
const ANTHROPIC_MANAGED_EFFORT: &str = "high";

/// Upstream `thinkingLevelMap?.off !== null`: true when the model has no
/// thinking-level map or its "off" entry is a value (not an explicit null).
fn off_map_marks_unsupported(model: &Model) -> bool {
    matches!(
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("off")),
        Some(None)
    )
}

fn is_anthropic_effort(value: &str) -> bool {
    matches!(value, "low" | "medium" | "high" | "xhigh" | "max")
}

/// The `display` value for thinking requests (upstream `thinkingDisplay`,
/// default `"summarized"` to stay consistent with older Claude 4 models).
fn thinking_display_value(options: &StreamOptions) -> &'static str {
    options
        .thinking_display
        .map(|display| display.as_str())
        .unwrap_or("summarized")
}

/// Resolve the `thinking` request field, optional `output_config`, and the
/// effective `max_tokens` (upstream `buildParams` thinking branch +
/// `streamSimple` budget math).
fn thinking_request_config(
    model: &Model,
    options: &StreamOptions,
    requested_level: Option<ThinkingLevel>,
    compat: &AnthropicCompat,
) -> AiResult<(Option<Value>, Option<Value>, u64)> {
    if !model.reasoning {
        return Ok((None, None, options.max_tokens.unwrap_or(model.max_tokens)));
    }
    // Claude models that accept `thinking: {type: "disabled"}` alongside
    // per-message effort may turn thinking off; models whose catalog level map
    // marks "off" unsupported (Opus 5.5, Sonnet 5.5, …) cannot (upstream
    // `thinkingDisabled`, #10753).
    let off_unsupported = matches!(
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("off")),
        Some(None)
    );
    if matches!(options.reasoning, Some(ThinkingLevel::Off)) && !off_unsupported {
        // No effort is sent, so the model default applies. Anthropic rejects
        // `block_binding` while thinking is disabled.
        return Ok((
            Some(json!({ "type": "disabled" })),
            None,
            options.max_tokens.unwrap_or(model.max_tokens),
        ));
    }
    // Managed-effort models always use adaptive thinking so prefix mismatches
    // can be dropped instead of surfacing as persistent 400 responses. Their
    // top-level effort is fixed at "low" (upstream #10063: organizations can
    // cap effort and reject any level above the cap; the thinking level does
    // not map onto output_config.effort for these — per-message markers carry
    // the active effort instead).
    if compat.supports_mid_convo_effort {
        let thinking = json!({
            "type": "adaptive",
            "display": thinking_display_value(options),
            "block_binding": { "prefix_mismatch_behavior": "drop_block" },
        });
        return Ok((
            Some(thinking),
            Some(json!({ "effort": "low" })),
            options.max_tokens.unwrap_or(model.max_tokens),
        ));
    }
    if compat.force_adaptive_thinking {
        // Adaptive thinking: Claude decides when and how much to think.
        let thinking = json!({
            "type": "adaptive",
            "display": thinking_display_value(options),
        });
        let effort = requested_level.map(|_| map_thinking_level_to_effort(model, requested_level));
        let output_config = effort
            .filter(|effort| is_anthropic_effort(effort))
            .map(|effort| json!({ "effort": effort }));
        return Ok((
            Some(thinking),
            output_config,
            options.max_tokens.unwrap_or(model.max_tokens),
        ));
    }
    match requested_level {
        Some(level) => {
            // Budget-based thinking for older models (upstream
            // `adjustMaxTokensForThinking` + `clampMaxTokensToContext`): the
            // budget comes from the per-request `thinkingBudgets` override
            // ladder first, then the defaults. The thinking budget shares the
            // response ceiling: at least MIN_ANSWER_TOKENS (1024) must remain
            // for the answer.
            const MIN_ANSWER_TOKENS: u64 = 1024;
            let budget =
                crate::ai::api::thinking_budget_for_level(level, options.thinking_budgets.as_ref());
            let base = options.max_tokens;
            // Undefined caller cap: the model cap holds the answer plus the
            // thinking budget (upstream `baseMaxTokens === undefined` branch).
            let max_tokens = match base {
                Some(base) => base.saturating_add(budget).min(model.max_tokens),
                None => model.max_tokens,
            };
            // Floor at 1024 (upstream `options.thinkingBudgetTokens || 1024`):
            // a tiny max_tokens clamp must not send a zero budget.
            let budget_tokens = if max_tokens <= budget {
                budget
                    .min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS))
                    .max(MIN_ANSWER_TOKENS)
            } else {
                budget
            };
            let thinking = json!({
                "type": "enabled",
                "budget_tokens": budget_tokens,
                "display": thinking_display_value(options),
            });
            Ok((Some(thinking), None, max_tokens))
        }
        None => {
            // "off": send an explicit disabled marker unless the model's map
            // marks "off" as unsupported (upstream `thinkingLevelMap.off`).
            let off_unsupported = matches!(
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get("off")),
                Some(None)
            );
            if off_unsupported {
                Ok((None, None, options.max_tokens.unwrap_or(model.max_tokens)))
            } else {
                Ok((
                    Some(json!({ "type": "disabled" })),
                    None,
                    options.max_tokens.unwrap_or(model.max_tokens),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message conversion (upstream `convertMessages`)
// ---------------------------------------------------------------------------

fn normalize_tool_call_id(id: &str) -> String {
    crate::ai::api::transform::sanitize_id_part(id)
        .chars()
        .take(64)
        .collect()
}

/// Convert an image content block to Anthropic's base64 image JSON
/// (upstream `convertImageBlock`).
fn convert_image_block(image: &ImageContent) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": image.mime_type,
            "data": image.data,
        },
    })
}

/// Convert user-visible content blocks to Anthropic block JSON. Only-text
/// content collapses to a joined string (upstream `convertContentBlocks`).
fn convert_content_blocks(content: &[ContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)));
    if !has_images {
        let text = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return json!(text);
    }
    let mut blocks: Vec<Value> = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text(text) => blocks.push(json!({"type": "text", "text": text.text})),
            ContentBlock::Image(image) => blocks.push(convert_image_block(image)),
            _ => {}
        }
    }
    if !blocks.iter().any(|block| block["type"] == "text") {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    Value::Array(blocks)
}

fn convert_tool_result(
    model: &Model,
    message: &ToolResultMessage,
    oauth: bool,
    content: &[ContentBlock],
) -> Value {
    let mut result = json!({
        "type": "tool_result",
        "tool_use_id": normalize_tool_call_id(&message.tool_call_id),
        "content": convert_content_blocks(content),
    });
    if message.is_error {
        result["is_error"] = json!(true);
    }
    let _ = (model, oauth);
    result
}

/// Convert a tool definition to Anthropic `BetaTool` JSON (upstream
/// `convertTools`): strict schemas get the strict transform plus a `strict`
/// marker; schemas with keywords strict mode rejects fall back to non-strict.
/// A `"prefer"` tool is sent non-strict on the fallback attempt
/// (`sampling.disable_preferred_strict_tools`), and going strict because of a
/// `"prefer"` config marks the request eligible for that fallback.
fn convert_tool(
    tool: &Tool,
    oauth: bool,
    eager_input_streaming: bool,
    supports_strict_tools: bool,
    sampling: &mut StrictToolSampling,
) -> AiResult<Value> {
    let preferred = matches!(
        &tool.constrained_sampling,
        Some(ConstrainedSamplingConfig::JsonSchema {
            strict: StrictPreference::Prefer
        })
    );
    let strict = if sampling.disable_preferred_strict_tools && preferred {
        // Upstream passes `undefined` for the strict flag here, skipping the
        // `resolveJsonSchemaStrictSampling`/`getJsonSchemaToolParameters`
        // strict path entirely: the original required list and property
        // schemas go out unchanged, and the shared tail (eager input
        // streaming, cache breakpoint) still applies.
        false
    } else {
        resolve_json_schema_strict_sampling(
            tool,
            supports_strict_tools,
            Some(anthropic_strict_unsupported_keyword),
        )?
        .unwrap_or(false)
    };
    if strict && preferred {
        sampling.has_preferred_strict_tools = true;
    }
    let parameters = get_json_schema_tool_parameters(tool, strict)?;
    let input_schema = if strict {
        // Strict transform output plus the legacy required fields.
        let mut schema = make_strict_json_schema(&tool.parameters, None)?;
        schema["type"] = json!("object");
        if schema.get("properties").is_none() {
            schema["properties"] = json!({});
        }
        if schema.get("required").is_none() {
            schema["required"] = json!([]);
        }
        schema
    } else {
        legacy_input_schema(&parameters)
    };
    let mut converted = json!({
        "name": if oauth { to_claude_code_name(&tool.name) } else { tool.name.clone() },
        "description": tool.description,
        "input_schema": input_schema,
    });
    if eager_input_streaming {
        converted["eager_input_streaming"] = json!(true);
    }
    if strict {
        converted["strict"] = json!(true);
    }
    Ok(converted)
}

fn convert_tools(
    tools: &[Tool],
    oauth: bool,
    eager_input_streaming: bool,
    supports_strict_tools: bool,
    cache_control: Option<Value>,
    sampling: &mut StrictToolSampling,
) -> AiResult<Vec<Value>> {
    let last_index = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut converted = convert_tool(
                tool,
                oauth,
                eager_input_streaming,
                supports_strict_tools,
                sampling,
            )?;
            // Cache breakpoint on the last tool definition (upstream
            // convertTools: `index === tools.length - 1`).
            if let (Some(cache_control), true) = (&cache_control, index == last_index) {
                converted["cache_control"] = cache_control.clone();
            }
            Ok(converted)
        })
        .collect()
}

/// Legacy `input_schema` shape: `{type, properties, required}` extracted from
/// the parameters JSON schema (upstream `legacyInputSchema`).
fn legacy_input_schema(parameters: &Value) -> Value {
    json!({
        "type": "object",
        "properties": parameters.get("properties").cloned().unwrap_or_else(|| json!({})),
        "required": parameters.get("required").cloned().unwrap_or_else(|| json!([])),
    })
}

fn user_message_blocks(message: &UserMessage) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(text) => {
                if text.text.trim().is_empty() {
                    continue;
                }
                blocks.push(json!({"type": "text", "text": text.text}));
            }
            ContentBlock::Image(image) => blocks.push(convert_image_block(image)),
            _ => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }
    Some(json!({"role": "user", "content": Value::Array(blocks)}))
}

/// Replay assistant content (upstream `convertMessages` assistant branch):
/// text, thinking with signature rules (redacted → `redacted_thinking`;
/// missing signature → plain text unless `allowEmptySignature`), and tool_use.
fn assistant_message_blocks(
    model: &Model,
    message: &AssistantMessage,
    oauth: bool,
    allow_empty_signature: bool,
) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(text) => {
                if text.text.trim().is_empty() {
                    continue;
                }
                blocks.push(json!({"type": "text", "text": text.text}));
            }
            ContentBlock::Thinking(thinking) => {
                if thinking.redacted {
                    // Redacted thinking: pass the opaque payload back as
                    // `redacted_thinking` so the API can decrypt it.
                    if let Some(data) = &thinking.thinking_signature {
                        blocks.push(json!({"type": "redacted_thinking", "data": data}));
                    }
                    continue;
                }
                let signature = thinking.thinking_signature.as_deref().unwrap_or("");
                let has_signature = !signature.trim().is_empty();
                if thinking.thinking.trim().is_empty() && !has_signature {
                    continue;
                }
                if !has_signature {
                    // Missing signature (e.g. from an aborted stream): convert
                    // to plain text unless the provider accepts empty ones.
                    if allow_empty_signature {
                        blocks.push(
                            json!({"type": "thinking", "thinking": thinking.thinking, "signature": ""}),
                        );
                    } else {
                        blocks.push(json!({"type": "text", "text": thinking.thinking}));
                    }
                } else {
                    blocks.push(
                        json!({"type": "thinking", "thinking": thinking.thinking, "signature": signature}),
                    );
                }
            }
            ContentBlock::ToolCall(call) => {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": normalize_tool_call_id(&call.id),
                    "name": if oauth { to_claude_code_name(&call.name) } else { call.name.clone() },
                    "input": call.arguments,
                }));
            }
            ContentBlock::Image(_) => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }
    let _ = model;
    Some(json!({"role": "assistant", "content": Value::Array(blocks)}))
}

fn system_message_text(message: &UserMessage) -> String {
    // Include structured `sections` (durable/compaction system messages carry
    // their prompt there with an empty content vector).
    crate::ai::types::system_message_text(message)
}

/// Convert pir messages to Anthropic `messages` (upstream `convertMessages`).
/// Later system messages are folded into the top-level system prompt unless
/// native tool changes are in use, in which case they are sent as system-role
/// messages carrying `tool_addition`/`tool_removal` blocks just before the
/// next assistant message (Anthropic requires `tool_result` blocks to
/// immediately follow their `tool_use`). Returns the converted messages plus
/// the folded mid-conversation system texts for the top-level system prompt.
/// `managed_provider` (upstream `managedProvider`) enables the per-message
/// effort record: when set, an assistant reply from that provider that carries
/// a recorded `providerThinkingLevel` reports its message index with that
/// effort so [`build_anthropic_request_with_strict_sampling`] can insert the
/// effort-only system marker before it.
#[allow(clippy::type_complexity)]
fn convert_messages(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    oauth: bool,
    native_tool_changes: bool,
    managed_provider: Option<&str>,
    sampling: &mut StrictToolSampling,
) -> AiResult<(Vec<Value>, Vec<String>, Vec<(usize, String)>)> {
    let _ = options;
    let allow_empty_signature = anthropic_compat(model).allow_empty_signature;
    // Upstream runs transformMessages before conversion (replaying aborted
    // assistant turns, synthetic tool results for orphaned calls, cross-model
    // thinking lowering, tool-call id normalization).
    let transformed = transform_messages_lazy(
        &context.messages,
        model,
        Some(&|id, _source| normalize_tool_call_id(id)),
    );
    let mut params: Vec<Value> = Vec::new();
    let mut pending_system: Vec<Value> = Vec::new();
    let mut system_prompt_tail: Vec<String> = Vec::new();
    let mut assistant_levels: Vec<(usize, String)> = Vec::new();
    let flush_pending = |params: &mut Vec<Value>, pending: &mut Vec<Value>| {
        params.append(pending);
    };

    let mut index = 0;
    while index < transformed.len() {
        match transformed[index].as_ref() {
            Message::System(system) | Message::Developer(system) => {
                let mut blocks: Vec<Value> = Vec::new();
                let text = system_message_text(system);
                if !text.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if native_tool_changes {
                    // WHY (borrow): these definitions are only read to collect
                    // names and convert them into this request's JSON; cloning
                    // the Vec<Tool> copied every schema a second time.
                    let added: &[Tool] = system.tools_added.as_deref().unwrap_or(&[]);
                    let redefined: std::collections::BTreeSet<&str> =
                        added.iter().map(|tool| tool.name.as_str()).collect();
                    for reference in system.tools_removed.iter().flatten() {
                        // A new definition under the same name replaces the
                        // old one, so no removal is needed.
                        if redefined.contains(reference.name.as_str()) {
                            continue;
                        }
                        blocks.push(json!({
                            "type": "tool_removal",
                            "tool": {"type": "tool_reference", "name": reference.name},
                        }));
                    }
                    for tool in added {
                        blocks.push(json!({
                            "type": "tool_addition",
                            "tool": {
                                "type": "tool_definition",
                                "definition": convert_tool(
                                    tool,
                                    oauth,
                                    anthropic_compat(model).supports_eager_tool_input_streaming,
                                    anthropic_compat(model).supports_strict_tools,
                                    sampling,
                                )?,
                            },
                        }));
                    }
                }
                if native_tool_changes && !blocks.is_empty() {
                    pending_system.push(json!({"role": "system", "content": Value::Array(blocks)}));
                } else if !native_tool_changes {
                    let text = system_message_text(system);
                    if !text.trim().is_empty() {
                        system_prompt_tail.push(text);
                    }
                }
            }
            Message::User(user) => {
                if let Some(block) = user_message_blocks(user) {
                    params.push(block);
                }
            }
            Message::Assistant(assistant) => {
                if let Some(block) =
                    assistant_message_blocks(model, assistant, oauth, allow_empty_signature)
                {
                    // Upstream flushes held system updates only after the
                    // empty-assistant check (#11091): an assistant that
                    // transforms to no blocks is dropped, so a pending update
                    // must not land where it was and end up directly before a
                    // user message, which Anthropic rejects as an illegal
                    // `system` -> `user` order.
                    flush_pending(&mut params, &mut pending_system);
                    // Upstream records the message index that receives the
                    // effort marker: the position this message is about to
                    // take in `params` (markers are inserted afterwards, so
                    // the index stays stable until `insertThinkingLevelMessages`
                    // walks the list).
                    if let Some(managed_provider) = managed_provider {
                        if assistant.api == "anthropic-messages"
                            && assistant.provider == managed_provider
                            && assistant
                                .provider_thinking_level
                                .as_deref()
                                .is_some_and(is_anthropic_effort)
                        {
                            assistant_levels.push((
                                params.len(),
                                assistant
                                    .provider_thinking_level
                                    .clone()
                                    .unwrap_or_default(),
                            ));
                        }
                    }
                    params.push(block);
                }
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages into one user
                // message (required for z.ai's Anthropic endpoint).
                let mut tool_results: Vec<Value> = Vec::new();
                // Anthropic rejects error tool results that contain anything
                // but text, so their images follow all tool results of the
                // turn, which must come first in the user message (#10794).
                let mut error_images: Vec<Value> = Vec::new();
                let mut cursor = index;
                // Bounded by the POST-transform transcript: transform drops
                // errored/aborted assistant messages, so `context.messages`
                // can be longer and the old bound allowed an out-of-bounds
                // index (upstream anthropic-messages.ts:1466).
                while let Message::ToolResult(next) = transformed[cursor].as_ref() {
                    let split = split_error_tool_result_images(next);
                    let content = split
                        .as_ref()
                        .map(|split| split.content.as_slice())
                        .unwrap_or(next.content.as_slice());
                    tool_results.push(convert_tool_result(model, next, oauth, content));
                    if let Some(split) = split {
                        error_images.push(json!({"type": "text", "text": split.caption}));
                        error_images.extend(split.images.iter().map(convert_image_block));
                    }
                    cursor += 1;
                    if cursor >= transformed.len() {
                        break;
                    }
                }
                index = cursor - 1;
                tool_results.extend(error_images);
                params.push(json!({"role": "user", "content": Value::Array(tool_results)}));
            }
        }
        index += 1;
    }
    flush_pending(&mut params, &mut pending_system);
    Ok((params, system_prompt_tail, assistant_levels))
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

fn base(model: &Model) -> AssistantMessage {
    crate::ai::api::base_message(model)
}
/// Parse a full SSE response body into assistant message events.
/// Public for tests/ and non-streaming callers; feeds the same
/// [`AnthropicSseParser`] state machine as the incremental streaming path.
pub fn parse_anthropic_sse(
    body: &[u8],
    model: &Model,
) -> Result<Vec<AssistantMessageEvent>, AiError> {
    let mut parser = AnthropicSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    parser.feed(body, &mut out)?;
    parser.finish(&mut out)?;
    Ok(out)
}

/// One in-flight provider content block, keyed by wire index.
#[derive(Debug, Clone)]
enum StreamBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
        redacted: bool,
    },
    Tool {
        id: String,
        name: String,
        partial_json: String,
        /// Bounded-cadence refresh of the parsed arguments mirrored into the
        /// block (`partial_json` stays the accumulation source of truth).
        mirror: crate::ai::api::partial_mirror::MirrorCadence,
    },
}

impl StreamBlock {
    fn kind(&self) -> &'static str {
        match self {
            StreamBlock::Text { .. } => "text",
            StreamBlock::Thinking { .. } => "thinking",
            StreamBlock::Tool { .. } => "tool",
        }
    }
}

/// Incremental SSE parser for Anthropic Messages API streams.
///
/// Both the batch path ([`parse_anthropic_sse`]) and the streaming path drive
/// the same state machine: [`feed`](Self::feed) splits raw body chunks into
/// SSE lines (buffering partial lines across chunk boundaries), and
/// [`handle_data`](Self::handle_data) maps one data payload to events
/// (text/thinking/redacted_thinking/tool_use blocks, signature deltas, stop
/// reasons, usage). [`finish`](Self::finish) flushes a trailing event block
/// and emits the block end / done events. Public for tests/.
pub struct AnthropicSseParser {
    /// Body bytes received but not yet forming a complete line.
    buffer: Vec<u8>,
    /// `event:` field of the current SSE block, if any.
    event_field: Option<String>,
    /// `data:` field lines accumulated for the current event block
    /// (multi-line data payloads are valid SSE; they join with `\n`).
    data_lines: Vec<String>,
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// (`Arc::make_mut(&mut self.message)`) so partial snapshots are O(1)
    /// hand-outs and a delta nobody snapshotted is applied in place — instead
    /// of the previous deep clone of the whole accumulated message per delta.
    message: Arc<AssistantMessage>,
    /// In-flight blocks in start order (upstream appends blocks to
    /// `output.content` at `content_block_start`).
    blocks: Vec<(usize, StreamBlock)>,
    /// Content indices whose end event was already emitted at
    /// `content_block_stop` (upstream finalizes blocks in place there).
    stopped: std::collections::BTreeSet<usize>,
    /// Declared tools for Claude Code name de-normalization on OAuth requests
    /// (upstream maps tool-call names back through the current tool list).
    declared_tools: Vec<Tool>,
    reason: StopReason,
    error_message: Option<String>,
    /// The rates used to price usage as it arrives (upstream `usageModel`).
    /// Replaced by a fallback model's rates when the server reports that it
    /// fell back (`compat.allowedFallbackModels`).
    model_cost: crate::ai::types::ModelCost,
    /// Catalog costs of the same-provider fallback models the server may
    /// transparently switch to, keyed by model id (upstream
    /// `allowedFallbackModels.find(...).cost`).
    fallback_costs: std::collections::BTreeMap<String, crate::ai::types::ModelCost>,
    /// Whether the model routes through OpenRouter (upstream
    /// `isOpenRouterModel`): OpenRouter adds billed cost accounting to
    /// streamed usage.
    openrouter: bool,
    /// Attached upstream `onProviderStreamEvent` observer (each parsed SSE
    /// event before pir normalization; read-only).
    provider_event_hook: Option<crate::ai::types::StreamHook>,
}

impl AnthropicSseParser {
    pub fn new(model: &Model) -> Self {
        Self {
            buffer: Vec::new(),
            event_field: None,
            data_lines: Vec::new(),
            message: Arc::new(base(model)),
            blocks: Vec::new(),
            stopped: std::collections::BTreeSet::new(),
            declared_tools: Vec::new(),
            // Upstream `output.stopReason` starts as "pending": a stream that
            // ends without a stop reason is an error, not a silent stop
            // (anthropic-messages.ts:865-866).
            reason: StopReason::Pending,
            error_message: None,
            model_cost: model.cost.clone(),
            fallback_costs: allowed_fallback_costs(model),
            openrouter: is_openrouter(model),
            provider_event_hook: None,
        }
    }

    /// Declare the current tool list so tool-call names arriving in Claude
    /// Code casing (OAuth stealth mode) are mapped back to pir's names.
    fn with_declared_tools(mut self, tools: Vec<Tool>) -> Self {
        self.declared_tools = tools;
        self
    }

    /// Attach the upstream `onProviderStreamEvent` observer (each parsed
    /// provider event before pir normalization). Public for tests/.
    pub fn with_provider_event_hook(mut self, hook: Option<crate::ai::types::StreamHook>) -> Self {
        self.provider_event_hook = hook;
        self
    }

    /// Record the provider-native effort this reply is generated with
    /// (upstream `output.providerThinkingLevel`), replayed by later requests
    /// as an effort-only system marker.
    pub fn with_provider_thinking_level(mut self, effort: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.message).provider_thinking_level = Some(effort.into());
        self
    }

    /// Snapshot of the message accumulated so far (e.g. for the Start event).
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`. Every later
    /// mutation goes through `Arc::make_mut`, which copies-on-write, so the
    /// handed-out value stays the snapshot taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    /// Snapshot handed to delta events: the same O(1) `Arc` hand-out as
    /// [`Self::snapshot`]. Consecutive deltas whose provider payload leaves
    /// the message unchanged hand out the same `Arc` zero-copy, and a delta
    /// that does mutate copies-on-write (so the previous snapshot's value is
    /// frozen) only while some reader still holds it.
    fn partial_reuse(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    /// Feed one raw body chunk; pushes events for every complete SSE event
    /// block. Partial lines (and multi-byte characters split across chunks)
    /// are buffered until a newline arrives.
    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result =
            crate::ai::api::sse_feed_lines(chunk, &mut buffer, |line| self.feed_line(line, events));
        self.buffer = buffer;
        result
    }

    /// Feed one complete SSE line (newline and trailing CR already stripped).
    pub fn feed_line(
        &mut self,
        line: &str,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if line.is_empty() {
            // Blank line: end of an event block, dispatch accumulated data.
            let event = self.event_field.take();
            if self.data_lines.is_empty() {
                return Ok(());
            }
            let data = std::mem::take(&mut self.data_lines).join("\n");
            if event.as_deref() == Some("error") {
                return Err(anthropic_error_frame(&data, false));
            }
            return self.handle_data(&data, events);
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event_field = Some(rest.trim().to_string());
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.trim().to_string());
        }
        // Other SSE fields (`id:`, comments) carry no data for this format.
        Ok(())
    }

    fn block_position(&self, wire_index: usize) -> Option<usize> {
        self.blocks
            .iter()
            .position(|(index, _)| *index == wire_index)
    }

    /// Map one SSE data payload to events (shared with the batch path).
    fn handle_data(&mut self, data: &str, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let value: Value = serde_json::from_str(data)?;
        // Upstream `options.onProviderStreamEvent`: the parsed provider event
        // is read-only here (anthropic-messages.ts:664).
        if let Some(hook) = &self.provider_event_hook {
            hook(&value);
        }
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            // A `{"type":"error","error":{...}}` payload without an `event:
            // error` field (some proxies/SSE relays omit the event name) must
            // fail the stream instead of being swallowed by the catch-all.
            "error" => {
                return Err(anthropic_error_frame(data, false));
            }
            "message_start" => {
                if let Some(id) = value
                    .get("message")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                {
                    Arc::make_mut(&mut self.message).response_id = Some(id.into());
                }
                if let Some(response_model) = value
                    .get("message")
                    .and_then(|v| v.get("model"))
                    .and_then(Value::as_str)
                {
                    if response_model != self.message.model {
                        Arc::make_mut(&mut self.message).response_model =
                            Some(response_model.into());
                        // Upstream: a server-side fallback response is priced
                        // with the fallback model's catalog cost when known.
                        if let Some(cost) = self.fallback_costs.get(response_model) {
                            self.model_cost = cost.clone();
                        }
                    }
                }
                if let Some(u) = value.get("message").and_then(|v| v.get("usage")) {
                    let model_cost = &self.model_cost;
                    let message = Arc::make_mut(&mut self.message);
                    merge_anthropic_usage(&mut message.usage, u);
                    crate::ai::types::calculate_cost_with(model_cost, &mut message.usage);
                }
            }
            "content_block_start" => {
                if let Some(block) = value.get("content_block") {
                    let wire_index =
                        value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    // Upstream anthropic-messages.ts: a `fallback` block is a
                    // server-side model fallback that pir cannot faithfully
                    // replay; a leading one (index 0, before any content) is
                    // tolerated, a mid-output one is an unsupported stream.
                    if block.get("type").and_then(Value::as_str) == Some("fallback") {
                        if wire_index != 0 {
                            return Err(AiError::NonRetryable(
                                "Anthropic performed an unsupported mid-output model fallback"
                                    .to_string(),
                            ));
                        }
                        return Ok(());
                    }
                    let stream_block = match block.get("type").and_then(Value::as_str) {
                        Some("text") => Some(StreamBlock::Text {
                            text: block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        }),
                        Some("thinking") => Some(StreamBlock::Thinking {
                            thinking: block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            signature: block
                                .get("signature")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            redacted: false,
                        }),
                        Some("redacted_thinking") => Some(StreamBlock::Thinking {
                            thinking: "[Reasoning redacted]".into(),
                            signature: block
                                .get("data")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            redacted: true,
                        }),
                        Some("tool_use") => {
                            let wire_name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let name = if self.declared_tools.is_empty() {
                                wire_name
                            } else {
                                from_claude_code_name(&wire_name, &self.declared_tools)
                            };
                            Some(StreamBlock::Tool {
                                id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                name,
                                partial_json: String::new(),
                                mirror: crate::ai::api::partial_mirror::MirrorCadence::new(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(stream_block) = stream_block {
                        let content_index = self.blocks.len();
                        let kind = stream_block.kind();
                        // Upstream appends the block to `output.content` at
                        // `content_block_start` so partial events carry the
                        // content accumulated so far.
                        let content_block = match &stream_block {
                            StreamBlock::Text { text } => ContentBlock::Text(TextContent {
                                text: text.clone(),
                                text_signature: None,
                            }),
                            StreamBlock::Thinking {
                                thinking,
                                signature,
                                redacted,
                            } => ContentBlock::Thinking(ThinkingContent {
                                thinking: thinking.clone(),
                                thinking_signature: if signature.is_empty() {
                                    None
                                } else {
                                    Some(signature.clone())
                                },
                                redacted: *redacted,
                            }),
                            StreamBlock::Tool { id, name, .. } => {
                                ContentBlock::ToolCall(ToolCall {
                                    id: normalize_tool_call_id(id),
                                    name: name.clone(),
                                    arguments: BTreeMap::new(),
                                    thought_signature: None,
                                    namespace: None,
                                    arguments_error: None,
                                })
                            }
                        };
                        Arc::make_mut(&mut self.message).content.push(content_block);
                        self.blocks.push((wire_index, stream_block));
                        out.push(match kind {
                            "text" => AssistantMessageEvent::TextStart {
                                content_index,
                                partial: Arc::clone(&self.message),
                            },
                            "thinking" => AssistantMessageEvent::ThinkingStart {
                                content_index,
                                partial: Arc::clone(&self.message),
                            },
                            _ => AssistantMessageEvent::ToolCallStart {
                                content_index,
                                partial: Arc::clone(&self.message),
                            },
                        });
                    }
                }
            }
            "content_block_delta" => {
                let wire_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(position) = self.block_position(wire_index) else {
                    return Ok(());
                };
                if let Some(delta) = value.get("delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(v) = delta.get("text").and_then(Value::as_str) {
                                if let Some((_, StreamBlock::Text { text })) =
                                    self.blocks.get_mut(position)
                                {
                                    text.push_str(v);
                                    if let Some(ContentBlock::Text(block)) =
                                        Arc::make_mut(&mut self.message).content.get_mut(position)
                                    {
                                        block.text.push_str(v);
                                    }
                                    out.push(AssistantMessageEvent::TextDelta {
                                        content_index: position,
                                        delta: v.into(),
                                        partial: Arc::clone(&self.message),
                                    });
                                }
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(v) = delta.get("thinking").and_then(Value::as_str) {
                                if let Some((_, StreamBlock::Thinking { thinking, .. })) =
                                    self.blocks.get_mut(position)
                                {
                                    thinking.push_str(v);
                                    if let Some(ContentBlock::Thinking(block)) =
                                        Arc::make_mut(&mut self.message).content.get_mut(position)
                                    {
                                        block.thinking.push_str(v);
                                    }
                                    out.push(AssistantMessageEvent::ThinkingDelta {
                                        content_index: position,
                                        delta: v.into(),
                                        partial: Arc::clone(&self.message),
                                    });
                                }
                            }
                        }
                        Some("signature_delta") => {
                            if let Some(v) = delta.get("signature").and_then(Value::as_str) {
                                if let Some((_, StreamBlock::Thinking { signature, .. })) =
                                    self.blocks.get_mut(position)
                                {
                                    signature.push_str(v);
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(v) = delta.get("partial_json").and_then(Value::as_str) {
                                // In-flight parsed arguments to mirror back into
                                // the message block (accumulated only when a
                                // usable parse exists).
                                let mut in_flight: Option<BTreeMap<String, Value>> = None;
                                if let Some((
                                    _,
                                    StreamBlock::Tool {
                                        partial_json,
                                        mirror,
                                        ..
                                    },
                                )) = self.blocks.get_mut(position)
                                {
                                    partial_json.push_str(v);
                                    // Upstream parses the accumulated partial
                                    // JSON on every argument delta and updates
                                    // the block in place (`block.arguments =
                                    // parseStreamingJson(block.partialJson)`,
                                    // anthropic-messages.ts:767). Rebuilding the
                                    // whole value per fragment is O(args²) per
                                    // call, so the mirror is refreshed at a
                                    // bounded cadence instead (first fragment,
                                    // then every 8 KiB); the finalized call is
                                    // parsed from the complete raw buffer.
                                    if mirror.record(v) {
                                        in_flight = parse_streaming_tool_arguments(partial_json);
                                    }
                                }
                                // Mirror the best-effort in-flight parse; a
                                // fragment with no usable parse keeps the last
                                // written-back value (the initial empty object
                                // for the leading fragments). An unchanged
                                // parse keeps the snapshot cache (Arc reuse).
                                if let Some(arguments) = in_flight {
                                    if let Some(ContentBlock::ToolCall(call)) =
                                        Arc::make_mut(&mut self.message).content.get_mut(position)
                                    {
                                        if call.arguments != arguments {
                                            call.arguments = arguments;
                                        }
                                    }
                                }
                                out.push(AssistantMessageEvent::ToolCallDelta {
                                    content_index: position,
                                    delta: v.into(),
                                    partial: self.partial_reuse(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let wire_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(position) = self.block_position(wire_index) {
                    // Upstream finalizes the block in place at content_block_stop
                    // and emits its end event there — including `toolcall_end`,
                    // which error-terminated streams therefore never carry.
                    self.stopped.insert(position);
                    match &self.blocks[position].1 {
                        StreamBlock::Text { text } => {
                            out.push(AssistantMessageEvent::TextEnd {
                                content_index: position,
                                content: text.clone(),
                                partial: Arc::clone(&self.message),
                            });
                        }
                        StreamBlock::Thinking {
                            thinking,
                            signature,
                            redacted,
                        } => {
                            let finalized =
                                Arc::make_mut(&mut self.message).content.get_mut(position);
                            if let Some(ContentBlock::Thinking(block)) = finalized {
                                block.thinking_signature = if signature.is_empty() {
                                    None
                                } else {
                                    Some(signature.clone())
                                };
                                block.redacted = *redacted;
                            }
                            out.push(AssistantMessageEvent::ThinkingEnd {
                                content_index: position,
                                content: thinking.clone(),
                                partial: Arc::clone(&self.message),
                            });
                        }
                        StreamBlock::Tool {
                            id: _,
                            name: _,
                            partial_json,
                            ..
                        } => {
                            if let Some(ContentBlock::ToolCall(call)) =
                                Arc::make_mut(&mut self.message).content.get_mut(position)
                            {
                                crate::ai::api::tool_args::apply_final_tool_arguments(
                                    call,
                                    partial_json,
                                );
                                let call = call.clone();
                                out.push(AssistantMessageEvent::ToolCallEnd {
                                    content_index: position,
                                    tool_call: call,
                                    partial: Arc::clone(&self.message),
                                });
                            }
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = value.get("delta") {
                    // Upstream `if (event.delta.stop_reason)`
                    // (anthropic-messages.ts:818): an empty stop_reason is not
                    // a terminal signal, so it must not map to an "unhandled
                    // stop reason" error on intermediate deltas.
                    if let Some(stop) = delta
                        .get("stop_reason")
                        .and_then(Value::as_str)
                        .filter(|stop| !stop.is_empty())
                    {
                        let explanation = delta
                            .get("stop_details")
                            .and_then(|details| details.get("explanation"))
                            .and_then(Value::as_str);
                        match map_stop_reason(stop, explanation) {
                            Ok((reason, error_message)) => {
                                self.reason = reason;
                                self.error_message = error_message;
                            }
                            Err(error) => return Err(error),
                        }
                        Arc::make_mut(&mut self.message).raw_stop_reason = Some(stop.into());
                    }
                }
                if let Some(u) = value.get("usage") {
                    let model_cost = &self.model_cost;
                    let openrouter = self.openrouter;
                    let message = Arc::make_mut(&mut self.message);
                    merge_anthropic_usage(&mut message.usage, u);
                    crate::ai::types::calculate_cost_with(model_cost, &mut message.usage);
                    // OpenRouter adds billing fields to the streamed
                    // `message_delta` usage at runtime (upstream applies the
                    // reported cost right after `calculateCost`).
                    if openrouter && !u.is_null() {
                        crate::ai::util::apply_openrouter_reported_cost(&mut message.usage, u);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Flush a trailing event block not terminated by a blank line, emit end
    /// events for any block the provider never stopped (truncated streams),
    /// and terminate with the `Done` event. Finalized tool calls already
    /// emitted their `toolcall_end` at `content_block_stop` (upstream
    /// behavior), so error-terminated streams never carry them.
    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        // Flush a trailing event block not terminated by a blank line.
        if !self.data_lines.is_empty() || self.event_field.is_some() {
            let event = self.event_field.take();
            if !self.data_lines.is_empty() {
                let data = std::mem::take(&mut self.data_lines).join("\n");
                if event.as_deref() == Some("error") {
                    return Err(anthropic_error_frame(&data, true));
                }
                self.handle_data(&data, out)
                    .map_err(crate::ai::api::trailing_event_error)?;
            }
        }
        // Fallback flush for blocks whose content_block_stop never arrived.
        let blocks = std::mem::take(&mut self.blocks);
        for (content_index, (_, block)) in blocks.into_iter().enumerate() {
            if self.stopped.contains(&content_index) {
                continue;
            }
            match block {
                StreamBlock::Text { text } => {
                    out.push(AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text.clone(),
                        partial: Arc::clone(&self.message),
                    });
                }
                StreamBlock::Thinking {
                    thinking,
                    signature,
                    redacted,
                } => {
                    if let Some(ContentBlock::Thinking(block)) = Arc::make_mut(&mut self.message)
                        .content
                        .get_mut(content_index)
                    {
                        block.thinking_signature = if signature.is_empty() {
                            None
                        } else {
                            Some(signature.clone())
                        };
                        block.redacted = redacted;
                    }
                    out.push(AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content: thinking,
                        partial: Arc::clone(&self.message),
                    });
                }
                StreamBlock::Tool {
                    id: _,
                    name: _,
                    partial_json,
                    ..
                } => {
                    if let Some(ContentBlock::ToolCall(call)) = Arc::make_mut(&mut self.message)
                        .content
                        .get_mut(content_index)
                    {
                        crate::ai::api::tool_args::apply_final_tool_arguments(call, &partial_json);
                        let call = call.clone();
                        out.push(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call: call,
                            partial: Arc::clone(&self.message),
                        });
                    }
                }
            }
        }
        // Upstream anthropic-messages.ts:865-866: a stream that ends while the
        // stop reason is still "pending" is a stream error. A clean EOF here
        // means the terminal event never arrived (a truncated stream), which
        // is retryable at the agent level.
        if self.reason == StopReason::Pending {
            return Err(AiError::StreamTruncated(
                "Anthropic stream ended without a stop reason".to_string(),
            ));
        }
        // An error stop reason (refusal / sensitive / unhandled) terminates the
        // stream as an error carrying the provider message (upstream throws so
        // the catch path emits the stream error event).
        if self.reason == StopReason::Error {
            return Err(AiError::NonRetryable(
                self.error_message
                    .take()
                    .unwrap_or_else(|| "An unknown error occurred".into()),
            ));
        }
        let error_message = self.error_message.take();
        {
            // The end events above may still hold the previous snapshot, so
            // this finalization copies-on-write once; the terminal event then
            // reclaims the accumulator without another copy.
            let message = Arc::make_mut(&mut self.message);
            message.stop_reason = self.reason;
            message.error_message = error_message;
        }
        let terminal = match self.reason {
            StopReason::Length => TerminalReason::Length,
            StopReason::ToolUse => TerminalReason::ToolUse,
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

/// Typed error for an Anthropic in-band error frame.
///
/// The provider text (the raw `data` payload, upstream parity) is preserved;
/// the nested `error.type`/`message` decide the retry class: standard
/// Anthropic transient types (`overloaded_error`, `rate_limit_error`,
/// `api_error`) are retryable, while authentication/permission/
/// invalid_request errors are not.
///
/// `trailing` marks the flush at a clean EOF: a malformed trailing payload is
/// a truncated stream (retryable), not a non-retryable protocol verdict.
fn anthropic_error_frame(data: &str, trailing: bool) -> AiError {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return if trailing {
            AiError::StreamTruncated(
                "stream ended with an incomplete trailing error event".to_string(),
            )
        } else {
            AiError::NonRetryable(data.to_string())
        };
    };
    let error = value.get("error").unwrap_or(&value);
    crate::ai::api::provider_error_frame_from_value(error, data.to_string())
}

/// Best-effort parse of the accumulated tool-call argument JSON while the
/// block is still streaming: a direct parse first, then the shared
/// partial-JSON repair. `None` when no JSON object can be produced yet (the
/// fragment is still incomplete, e.g. a missing value or a trailing comma),
/// so the caller keeps the arguments from the last usable parse instead of
/// clearing them.
fn parse_streaming_tool_arguments(raw: &str) -> Option<BTreeMap<String, Value>> {
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        return match parsed {
            Value::Object(map) => Some(map.into_iter().collect()),
            _ => None,
        };
    }
    match crate::ai::api::openai_responses::shared::parse_partial_json(raw) {
        Value::Object(map) => Some(map.into_iter().collect()),
        _ => None,
    }
}

/// Map an Anthropic stop reason to a pir stop reason (upstream `mapStopReason`).
fn map_stop_reason(
    reason: &str,
    stop_details_explanation: Option<&str>,
) -> AiResult<(StopReason, Option<String>)> {
    match reason {
        "end_turn" => Ok((StopReason::Stop, None)),
        "max_tokens" => Ok((StopReason::Length, None)),
        "tool_use" => Ok((StopReason::ToolUse, None)),
        "refusal" => Ok((
            StopReason::Error,
            Some(
                stop_details_explanation
                    .unwrap_or("The model refused to complete the request")
                    .to_string(),
            ),
        )),
        // Stop is good enough -> resubmit.
        "pause_turn" => Ok((StopReason::Stop, None)),
        // We don't supply stop sequences, so this should never happen.
        "stop_sequence" => Ok((StopReason::Stop, None)),
        // Content flagged by safety filters.
        "sensitive" => Ok((
            StopReason::Error,
            Some("Provider stopped with: sensitive".into()),
        )),
        other => Err(AiError::NonRetryable(format!(
            "Unhandled stop reason: {other}"
        ))),
    }
}

/// Merge provider usage fields that are present, so partial usage chunks
/// (e.g. `message_delta` carrying only `output_tokens`) never reset fields
/// already captured from `message_start`.
fn merge_anthropic_usage(usage: &mut Usage, value: &Value) {
    if value.is_null() {
        return;
    }
    if let Some(v) = value.get("input_tokens").and_then(Value::as_u64) {
        usage.input = v;
    }
    if let Some(v) = value.get("output_tokens").and_then(Value::as_u64) {
        usage.output = v;
    }
    if let Some(v) = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        usage.cache_write = v;
    }
    if let Some(v) = value.get("cache_read_input_tokens").and_then(Value::as_u64) {
        usage.cache_read = v;
    }
    // Vercel AI Gateway includes the TTL breakdown in deltas; the SDK only
    // types it on message_start.
    if let Some(v) = value
        .get("cache_creation")
        .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
    {
        usage.cache_write_1h = Some(v);
    }
    // Anthropic reports reasoning tokens as a subset of output tokens.
    if let Some(v) = value
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        usage.reasoning = Some(v);
    }
    // Anthropic doesn't provide total_tokens, compute from components
    // (upstream: input + output + cacheRead + cacheWrite).
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}
