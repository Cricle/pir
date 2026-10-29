//! Google Vertex AI adapter (`google-vertex`).
//!
//! Rust equivalent of upstream `packages/ai/src/api/google-vertex.ts`: the
//! same `GenerateContent` payload shape as the Gemini adapter (shared codecs
//! in [`crate::ai::api::google::shared`] and the same SSE response format),
//! with Vertex endpoint/auth semantics:
//!
//! - Express mode (API key): `POST https://aiplatform.googleapis.com/v1/
//!   publishers/google/models/{model}:streamGenerateContent?alt=sse` with the
//!   key from the request api key, `GOOGLE_CLOUD_API_KEY`, or `GOOGLE_API_KEY`
//!   env. Upstream keeps express mode to this global host
//!   (`(apiKey && !project) || location === 'global'`, @google/genai@2.21.0
//!   dist/node/index.cjs:13561-13564).
//! - Service-account / ADC mode: a `service_account` JSON key
//!   (`GOOGLE_APPLICATION_CREDENTIALS` or the gcloud well-known file) is
//!   signed into an RS256 OAuth2 JWT and exchanged for a Bearer token by
//!   [`service_account`]; project + location come from `GOOGLE_CLOUD_PROJECT`/
//!   `GCLOUD_PROJECT` + `GOOGLE_CLOUD_LOCATION` (upstream google-vertex.ts:441-
//!   460). Requests go to `https://{location}-aiplatform.googleapis.com/v1/
//!   projects/{project}/locations/{location}/publishers/google/models/{model}:
//!   streamGenerateContent?alt=sse` (the `global` location and the `us`/`eu`
//!   multi-regions have their own hosts, index.cjs:13564-13570; the project
//!   path is prepended whenever project+location are set, index.cjs:13679-
//!   13703). The full credential-resolution order, JWT shape and token-cache
//!   policy are documented in the `service_account` module.
//!
//! A custom model base URL (catalog `https://{location}-aiplatform.
//! googleapis.com` keeps its placeholder) overrides the derived host in both
//! modes and suppresses the project-path prefix (`baseUrlResourceScope =
//! COLLECTION`, upstream google-vertex.ts:388-397 + index.cjs:13679-13695).

mod service_account;

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::ai::api::google::shared::{
    convert_google_contents, convert_google_tools, google_thinking_config_with,
};
use crate::ai::api::google::sse::GoogleSseParser;
use crate::ai::auth::ApiKeyAuth;
use crate::ai::error::{AiError, AiResult};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::Provider;
use crate::ai::transport::ProviderRequestClient;
use crate::ai::types::*;

/// The upstream api id for this adapter.
pub const GOOGLE_VERTEX_API_ID: &str = "google-vertex";

/// Default Vertex express endpoint host (upstream `@google/genai` express
/// mode uses the global `aiplatform.googleapis.com` endpoint).
const VERTEX_EXPRESS_BASE_URL: &str = "https://aiplatform.googleapis.com";

/// Vertex API version (upstream `API_VERSION`).
const VERTEX_API_VERSION: &str = "v1";

/// Sentinel credential value meaning "use Application Default Credentials"
/// (upstream `GCP_VERTEX_CREDENTIALS_MARKER`).
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";

/// Google Vertex AI provider.
#[derive(Clone)]
pub struct GoogleVertexProvider {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<Model>,
    pub client: ProviderRequestClient,
    pub api_key_env: Vec<String>,
}

impl GoogleVertexProvider {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<Model>,
    ) -> Result<Self, AiError> {
        let base_url = base_url.into();
        Ok(Self {
            provider_id: provider_id.into(),
            display_name: "Google Vertex AI".into(),
            models: models
                .into_iter()
                .map(|m| Model {
                    base_url: base_url.clone(),
                    ..m
                })
                .collect(),
            client: ProviderRequestClient::with_default_config()?,
            api_key_env: vec!["GOOGLE_CLOUD_API_KEY".into(), "GOOGLE_API_KEY".into()],
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

impl Provider for GoogleVertexProvider {
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
            "Google Cloud API key (Vertex express mode)".to_string(),
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
            let result = vertex_stream(&client, &model, &context, &options, &events).await;
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

/// Resolve the express-mode API key (upstream `resolveApiKey`): request api
/// key first, then `GOOGLE_CLOUD_API_KEY`; placeholder markers fall back to
/// the service-account/ADC path (google-vertex.ts:429-439 and 99-103).
fn resolve_api_key(options: &StreamOptions) -> Option<String> {
    let candidate = options.api_key.clone().or_else(|| {
        crate::ai::api::provider_env_value("GOOGLE_CLOUD_API_KEY", options.env.as_ref())
    })?;
    let trimmed = candidate.trim();
    if trimmed.is_empty()
        || trimmed == GCP_VERTEX_CREDENTIALS_MARKER
        || (trimmed.starts_with('<') && trimmed.ends_with('>'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// A usable custom model base URL: non-empty and without the catalog
/// `{location}`/brace placeholder (upstream `resolveCustomBaseUrl`,
/// google-vertex.ts:407-413 — a placeholder returns `undefined` and the
/// derived host is used instead).
fn custom_vertex_base(model: &Model) -> Option<String> {
    let custom = model
        .base_url
        .trim()
        .to_string()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if custom.is_empty() || custom.contains("{location}") || custom.contains('{') {
        return None;
    }
    Some(custom.trim_end_matches('/').to_string())
}

/// Append the `/v1` API version (unless the base already carries one,
/// upstream `baseUrlIncludesApiVersion`, google-vertex.ts:415-422 and
/// 394-396) and the resource path.
fn vertex_url_with_path(base: &str, path: &str) -> String {
    let version = if base.ends_with(VERTEX_API_VERSION) {
        String::new()
    } else {
        format!("/{VERTEX_API_VERSION}")
    };
    format!("{base}{version}/{path}")
}

/// The express-mode resource path; `alt=sse` matches `streamGenerateContent`
/// (upstream index.cjs:15797).
fn vertex_resources_path(model: &Model) -> String {
    format!(
        "publishers/google/models/{}:streamGenerateContent?alt=sse",
        model.id
    )
}

/// Resolve the Vertex express stream URL: a concrete custom base URL wins,
/// otherwise the global express endpoint. The API key travels in the
/// `x-goog-api-key` header only — a `key=` query parameter would leak the
/// credential into logs. Public for tests/.
pub fn resolve_vertex_url(model: &Model) -> String {
    let base = custom_vertex_base(model).unwrap_or_else(|| VERTEX_EXPRESS_BASE_URL.to_string());
    vertex_url_with_path(&base, &vertex_resources_path(model))
}

/// Resolve the project id for the service-account path (upstream
/// `resolveProject`, google-vertex.ts:441-452). pir has no per-request
/// `project` option on the shared [`StreamOptions`] (upstream
/// `GoogleVertexOptions.project` is adapter-specific), so the env vars are the
/// only source — a documented deviation.
fn resolve_vertex_project(env: Option<&ProviderEnv>) -> Result<String, AiError> {
    for name in ["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"] {
        if let Some(project) =
            crate::ai::api::provider_env_value(name, env).filter(|value| !value.is_empty())
        {
            return Ok(project);
        }
    }
    Err(AiError::Authentication(
        "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project \
         in options."
            .to_string(),
    ))
}

/// Resolve the location for the service-account path (upstream
/// `resolveLocation`, google-vertex.ts:454-460).
fn resolve_vertex_location(env: Option<&ProviderEnv>) -> Result<String, AiError> {
    crate::ai::api::provider_env_value("GOOGLE_CLOUD_LOCATION", env)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AiError::Authentication(
                "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in \
                 options."
                    .to_string(),
            )
        })
}

/// Resolve the service-account stream URL. Without a custom base URL the host
/// is derived from the location (upstream index.cjs:13553-13580): `global` —
/// the express host, `us`/`eu` — `https://aiplatform.{location}.rep.
/// googleapis.com` (index.cjs:13446,13564-13566), anything else —
/// `https://{location}-aiplatform.googleapis.com`; the
/// `projects/{project}/locations/{location}/` prefix is prepended for POST
/// requests whenever project+location are set (index.cjs:13679-13703). With a
/// custom base URL the express-style path is used unchanged (COLLECTION
/// resource scope, index.cjs:13680-13683).
fn resolve_vertex_service_account_url(model: &Model, project: &str, location: &str) -> String {
    let Some(base) = custom_vertex_base(model) else {
        let host = if location == "global" {
            VERTEX_EXPRESS_BASE_URL.to_string()
        } else if location == "us" || location == "eu" {
            format!("https://aiplatform.{location}.rep.googleapis.com")
        } else {
            format!("https://{location}-aiplatform.googleapis.com")
        };
        let path = format!(
            "projects/{project}/locations/{location}/{}",
            vertex_resources_path(model)
        );
        return vertex_url_with_path(&host, &path);
    };
    vertex_url_with_path(&base, &vertex_resources_path(model))
}

async fn vertex_stream(
    client: &ProviderRequestClient,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &AssistantMessageEventStream,
) -> AiResult<()> {
    // Auth precedence mirrors upstream (google-vertex.ts:99-103): an express
    // API key selects the `x-goog-api-key` client, otherwise the ADC path
    // (service-account project+location + GoogleAuth) is used. Project and
    // location resolve here — upstream `createClient(model,
    // resolveProject(options), resolveLocation(options))` runs before
    // `buildParams` (google-vertex.ts:101-104, 441-460).
    enum AuthTarget {
        Express(String),
        ServiceAccount { project: String, location: String },
    }
    let auth = match resolve_api_key(options) {
        Some(api_key) => AuthTarget::Express(api_key),
        None => AuthTarget::ServiceAccount {
            project: resolve_vertex_project(options.env.as_ref())?,
            location: resolve_vertex_location(options.env.as_ref())?,
        },
    };

    let mut body = build_vertex_body(model, context, options)?;
    // Upstream `options.onPayload`: inspect (or replace, by mutating in place)
    // the request body before it is sent.
    if let Some(on_payload) = &options.hooks.on_payload {
        on_payload(&mut body);
    }
    // Upstream `buildHttpOptions`: default UA first, then model headers, then
    // caller (request) headers last so they can override everything.
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

    let url = match auth {
        AuthTarget::Express(api_key) => {
            headers
                .entry("x-goog-api-key".into())
                .or_insert_with(|| api_key.clone());
            resolve_vertex_url(model)
        }
        AuthTarget::ServiceAccount { project, location } => {
            let bearer = service_account::bearer_token(
                client,
                options.env.as_ref(),
                options.signal.as_ref(),
            )
            .await?;
            // Bearer auth + quota project: header defaults never override a
            // caller-supplied header (upstream `addGoogleAuthHeaders` skips
            // existing keys, index.cjs:23825-23840; `x-goog-user-project` is
            // skipped when set, authclient.js:145-152).
            insert_header_default(
                &mut headers,
                "authorization",
                format!("Bearer {}", bearer.access_token),
            );
            if let Some(quota_project_id) = bearer.quota_project_id {
                insert_header_default(&mut headers, "x-goog-user-project", quota_project_id);
            }
            resolve_vertex_service_account_url(model, &project, &location)
        }
    };

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
    {
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
            .await?;
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

/// Insert a header default only when no header with the same name exists,
/// case-insensitively (upstream Header.get is case-insensitive and user
/// headers win; index.cjs:23830-23836).
fn insert_header_default(
    headers: &mut std::collections::BTreeMap<String, String>,
    name: &str,
    value: String,
) {
    if headers.keys().any(|key| key.eq_ignore_ascii_case(name)) {
        return;
    }
    headers.insert(name.to_string(), value);
}

/// Build the `GenerateContentRequest` body for Vertex (upstream
/// `buildParams`): the same shape as the Gemini adapter. Public for tests/.
pub fn build_vertex_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> AiResult<Value> {
    let (contents, folded_system) = convert_google_contents(model, context);
    let mut body = json!({ "contents": contents });
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
        true,
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
