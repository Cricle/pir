//! Parity tests for the openai-responses surface: Cloudflare AI Gateway auth
//! semantics, GitHub Copilot dynamic headers, and the session-affinity /
//! prompt-cache compat branches. All assertions run against pure functions on
//! fake requests; nothing opens a network connection.

use std::collections::BTreeMap;

use pir::ai::api::openai_responses::{
    copilot, get_client_api_key, has_header, responses_compat::ResponsesCompat, shared,
};
use pir::ai::auth::{
    cloudflare, cloudflare_ai_gateway_auth, cloudflare_workers_ai_auth,
    resolve_cloudflare_base_url, resolve_cloudflare_env, CloudflareAuthKind, MapAuthContext,
};
use pir::ai::error::AiError;
use pir::ai::types::{
    CacheRetention, ContentBlock, Context, ImageContent, Message, Model, ModelCost, ModelCostRates,
    OpenAiCompat, ProviderEnv, ProviderHeaders, SessionAffinityFormat, StreamOptions, UserMessage,
    UserRole,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn sample_model(provider: &str, id: &str, base_url: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-responses".to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 128_000,
        max_tokens: 16_384,
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

fn empty_context() -> Context {
    Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    }
}

fn user_with_image() -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Image(ImageContent {
            data: "aGk=".to_string(),
            mime_type: "image/png".to_string(),
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

// ---------------------------------------------------------------------------
// getClientApiKey: the "unused" placeholder (upstream openai-responses.ts:58-62)
// ---------------------------------------------------------------------------

#[test]
fn client_api_key_prefers_the_explicit_key() {
    let resolved =
        get_client_api_key("openai", Some("sk-test"), None).expect("explicit key resolves");
    assert_eq!(resolved, "sk-test");
}

#[test]
fn client_api_key_falls_back_to_unused_on_credential_headers() {
    let mut headers = ProviderHeaders::new();
    headers.insert("cf-aig-authorization".to_string(), "Bearer cf".to_string());
    let resolved =
        get_client_api_key("cloudflare-ai-gateway", None, Some(&headers)).expect("header counts");
    assert_eq!(resolved, "unused");

    let mut headers = ProviderHeaders::new();
    headers.insert("Authorization".to_string(), "Bearer someone".to_string());
    let resolved = get_client_api_key("openai", None, Some(&headers)).expect("header counts");
    assert_eq!(resolved, "unused");
}

#[test]
fn client_api_key_ignores_blank_header_values() {
    // Upstream hasHeader: a present-but-blank value does not count.
    let mut headers = ProviderHeaders::new();
    headers.insert("authorization".to_string(), "   ".to_string());
    let error =
        get_client_api_key("openai", None, Some(&headers)).expect_err("blank header is absent");
    assert!(matches!(error, AiError::Authentication(_)));

    let error = get_client_api_key("openai", None, None).expect_err("nothing to authenticate");
    assert!(matches!(error, AiError::Authentication(_)));
}

#[test]
fn has_header_is_case_insensitive() {
    let mut headers = ProviderHeaders::new();
    headers.insert("CF-AIG-Authorization".to_string(), "Bearer cf".to_string());
    assert!(has_header(Some(&headers), "cf-aig-authorization"));
    assert!(has_header(Some(&headers), "CF-AIG-AUTHORIZATION"));
    assert!(!has_header(None, "authorization"));
    assert!(!has_header(Some(&headers), "authorization"));
}

// ---------------------------------------------------------------------------
// Cloudflare AI Gateway auth (upstream cloudflare-auth.ts)
// ---------------------------------------------------------------------------

fn env_of(pairs: &[(&str, &str)]) -> MapAuthContext {
    MapAuthContext::new(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn gateway_auth_resolves_key_account_and_gateway_ids() {
    let ctx = env_of(&[
        ("CLOUDFLARE_API_KEY", "cf-key"),
        ("CLOUDFLARE_ACCOUNT_ID", "acct"),
        ("CLOUDFLARE_GATEWAY_ID", "gw"),
    ]);
    let resolved =
        resolve_cloudflare_env(CloudflareAuthKind::AiGateway, &ctx, None).expect("resolves");
    let resolved = resolved.expect("present");
    assert_eq!(resolved.api_key, "cf-key");
    assert_eq!(
        resolved.env.get(cloudflare::ACCOUNT_ID),
        Some(&"acct".to_string())
    );
    assert_eq!(
        resolved.env.get(cloudflare::GATEWAY_ID),
        Some(&"gw".to_string())
    );
    // Ambient-only resolution is attributed to the env var (upstream `source`).
    assert_eq!(resolved.source, cloudflare::API_KEY);
}

#[test]
fn gateway_auth_requires_the_gateway_id() {
    let ctx = env_of(&[
        ("CLOUDFLARE_API_KEY", "cf-key"),
        ("CLOUDFLARE_ACCOUNT_ID", "acct"),
    ]);
    let resolved =
        resolve_cloudflare_env(CloudflareAuthKind::AiGateway, &ctx, None).expect("no error");
    assert!(resolved.is_none(), "gateway id missing");
    // Workers-AI only needs key + account id.
    let resolved = resolve_cloudflare_env(CloudflareAuthKind::WorkersAi, &ctx, None)
        .expect("no error")
        .expect("workers-ai present");
    assert_eq!(resolved.api_key, "cf-key");
}

#[test]
fn gateway_auth_merges_credential_fields_with_ambient_env() {
    // Upstream resolveValue: the credential key carries the api key; the
    // account/gateway ids still fall back to the environment.
    let mut credential = pir::ai::types::ApiKeyCredential::new(Some("stored-key".to_string()));
    let mut credential_env = ProviderEnv::new();
    credential_env.insert(
        cloudflare::ACCOUNT_ID.to_string(),
        "stored-acct".to_string(),
    );
    credential.env = Some(credential_env);
    let ctx = env_of(&[("CLOUDFLARE_GATEWAY_ID", "ambient-gw")]);
    let resolved = resolve_cloudflare_env(CloudflareAuthKind::AiGateway, &ctx, Some(&credential))
        .expect("no error")
        .expect("present");
    assert_eq!(resolved.api_key, "stored-key");
    assert_eq!(
        resolved.env.get(cloudflare::ACCOUNT_ID),
        Some(&"stored-acct".to_string())
    );
    assert_eq!(
        resolved.env.get(cloudflare::GATEWAY_ID),
        Some(&"ambient-gw".to_string())
    );
    assert_eq!(resolved.source, "stored credential");
}

#[test]
fn gateway_auth_result_uses_cf_aig_authorization_header() {
    let ctx = env_of(&[
        ("CLOUDFLARE_API_KEY", "cf-key"),
        ("CLOUDFLARE_ACCOUNT_ID", "acct"),
        ("CLOUDFLARE_GATEWAY_ID", "gw"),
    ]);
    let auth = cloudflare_ai_gateway_auth(&ctx, None)
        .expect("no error")
        .expect("present");
    // The key rides as `cf-aig-authorization`; the default credential headers
    // are cleared (pir never emits `authorization`/`x-api-key` alongside it).
    let headers = auth.auth.headers.expect("gateway headers");
    assert_eq!(
        headers.get("cf-aig-authorization").map(String::as_str),
        Some("Bearer cf-key")
    );
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("x-api-key"));
    assert!(auth.auth.api_key.is_none());

    // Workers-AI keeps the plain api-key shape.
    let auth = cloudflare_workers_ai_auth(&ctx, None)
        .expect("no error")
        .expect("present");
    assert_eq!(auth.auth.api_key.as_deref(), Some("cf-key"));
    assert!(auth.auth.headers.is_none());
}

#[test]
fn gateway_base_url_placeholders_materialize_from_env() {
    let mut env = ProviderEnv::new();
    env.insert(cloudflare::ACCOUNT_ID.to_string(), "acct".to_string());
    env.insert(cloudflare::GATEWAY_ID.to_string(), "gw".to_string());
    let resolved = resolve_cloudflare_base_url(
        "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/pir/{CLOUDFLARE_GATEWAY_ID}/openai/v1",
        &env,
    );
    assert_eq!(
        resolved,
        "https://gateway.ai.cloudflare.com/v1/acct/pir/gw/openai/v1"
    );
    // Missing values keep their placeholder (upstream resolveCloudflareModel):
    // a fresh env without the gateway id.
    let mut partial_env = ProviderEnv::new();
    partial_env.insert(cloudflare::ACCOUNT_ID.to_string(), "acct".to_string());
    let partial = resolve_cloudflare_base_url(
        "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/pir/{CLOUDFLARE_GATEWAY_ID}",
        &partial_env,
    );
    assert_eq!(
        partial,
        "https://gateway.ai.cloudflare.com/v1/acct/pir/{CLOUDFLARE_GATEWAY_ID}"
    );
}

// ---------------------------------------------------------------------------
// GitHub Copilot dynamic headers (upstream github-copilot-headers.ts)
// ---------------------------------------------------------------------------

#[test]
fn copilot_detection_matches_provider_and_host() {
    assert!(copilot::is_github_copilot_model(&sample_model(
        "github-copilot",
        "gpt-5.5",
        "https://api.individual.githubcopilot.com"
    )));
    assert!(copilot::is_github_copilot_model(&sample_model(
        "custom",
        "gpt-5.5",
        "https://api.business.githubcopilot.com"
    )));
    assert!(!copilot::is_github_copilot_model(&sample_model(
        "openai",
        "gpt-5.5",
        "https://api.openai.com/v1"
    )));
}

#[test]
fn copilot_request_headers_carry_initiator_intent_and_vision() {
    let model = sample_model(
        "github-copilot",
        "gpt-5.5",
        "https://api.individual.githubcopilot.com",
    );
    let compat = ResponsesCompat::from_model(&model);
    let options = StreamOptions {
        api_key: Some("gh-token".to_string()),
        ..StreamOptions::default()
    };

    // Plain user request: user-initiated, no vision marker.
    let context = empty_context();
    let headers = pir::ai::api::openai_responses::assemble_request_headers(
        &model,
        &context,
        &options,
        &compat,
        "gh-token",
        ProviderHeaders::new(),
    );
    assert_eq!(headers.get("X-Initiator").map(String::as_str), Some("user"));
    assert_eq!(
        headers.get("Openai-Intent").map(String::as_str),
        Some("conversation-edits")
    );
    assert!(!headers.contains_key("Copilot-Vision-Request"));
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer gh-token")
    );

    // Image in the transcript: vision marker set.
    let mut context = empty_context();
    context.messages.push(user_with_image());
    let headers = pir::ai::api::openai_responses::assemble_request_headers(
        &model,
        &context,
        &options,
        &compat,
        "gh-token",
        ProviderHeaders::new(),
    );
    assert_eq!(
        headers.get("Copilot-Vision-Request").map(String::as_str),
        Some("true")
    );
}

// ---------------------------------------------------------------------------
// sessionAffinityFormat + prompt_cache_options (upstream
// openai-responses.ts:64-66, 106-114, 277-286)
// ---------------------------------------------------------------------------

#[test]
fn session_affinity_format_auto_detects_openrouter() {
    assert_eq!(
        ResponsesCompat::from_model(&sample_model(
            "openrouter",
            "x",
            "https://openrouter.ai/api/v1"
        ))
        .session_affinity_format,
        SessionAffinityFormat::Openrouter
    );
    assert_eq!(
        ResponsesCompat::from_model(&sample_model("custom", "x", "https://openrouter.ai/api/v1"))
            .session_affinity_format,
        SessionAffinityFormat::Openrouter
    );
    assert_eq!(
        ResponsesCompat::from_model(&sample_model("openai", "x", "https://api.openai.com/v1"))
            .session_affinity_format,
        SessionAffinityFormat::Openai
    );
    // An explicit model compat wins over the detection.
    let mut model = sample_model("openrouter", "x", "https://openrouter.ai/api/v1");
    model.compat = Some(OpenAiCompat {
        session_affinity_format: Some(SessionAffinityFormat::OpenaiNosession),
        ..Default::default()
    });
    assert_eq!(
        ResponsesCompat::from_model(&model).session_affinity_format,
        SessionAffinityFormat::OpenaiNosession
    );
}

#[test]
fn session_headers_follow_the_affinity_format() {
    let options = StreamOptions {
        api_key: Some("sk-test".to_string()),
        session_id: Some("sess-1".to_string()),
        ..StreamOptions::default()
    };

    let openai = sample_model("openai", "gpt-5.5", "https://api.openai.com/v1");
    let compat = ResponsesCompat::from_model(&openai);
    let headers = pir::ai::api::openai_responses::assemble_request_headers(
        &openai,
        &empty_context(),
        &options,
        &compat,
        "sk-test",
        ProviderHeaders::new(),
    );
    assert_eq!(
        headers.get("session_id").map(String::as_str),
        Some("sess-1")
    );
    assert_eq!(
        headers.get("x-client-request-id").map(String::as_str),
        Some("sess-1")
    );
    assert!(!headers.contains_key("x-session-id"));

    let openrouter = sample_model("openrouter", "x", "https://openrouter.ai/api/v1");
    let compat = ResponsesCompat::from_model(&openrouter);
    let headers = pir::ai::api::openai_responses::assemble_request_headers(
        &openrouter,
        &empty_context(),
        &options,
        &compat,
        "unused",
        ProviderHeaders::new(),
    );
    assert_eq!(
        headers.get("x-session-id").map(String::as_str),
        Some("sess-1")
    );
    assert!(!headers.contains_key("session_id"));
    assert!(!headers.contains_key("x-client-request-id"));

    // Nosession format: upstream only reaches it with an explicit
    // `sessionAffinityFormat: "openai-nosession"` compat (a bare custom
    // model detects the OpenAI format and DOES send session_id).
    let nosession = Model {
        compat: Some(OpenAiCompat {
            session_affinity_format: Some(SessionAffinityFormat::OpenaiNosession),
            ..OpenAiCompat::default()
        }),
        ..sample_model("custom", "x", "https://example.invalid/v1")
    };
    let compat = ResponsesCompat::from_model(&nosession);
    let headers = pir::ai::api::openai_responses::assemble_request_headers(
        &nosession,
        &empty_context(),
        &options,
        &compat,
        "sk-test",
        ProviderHeaders::new(),
    );
    assert!(!headers.contains_key("session_id"));
    assert!(!headers.contains_key("x-session-id"));
    assert_eq!(
        headers.get("x-client-request-id").map(String::as_str),
        Some("sess-1")
    );
}

#[test]
fn cache_retention_none_drops_the_session_headers() {
    let options = StreamOptions {
        api_key: Some("sk-test".to_string()),
        session_id: Some("sess-1".to_string()),
        cache_retention: Some(CacheRetention::None),
        ..StreamOptions::default()
    };

    let model = sample_model("openai", "gpt-5.5", "https://api.openai.com/v1");
    let compat = ResponsesCompat::from_model(&model);
    let headers = pir::ai::api::openai_responses::assemble_request_headers(
        &model,
        &empty_context(),
        &options,
        &compat,
        "sk-test",
        ProviderHeaders::new(),
    );
    assert!(!headers.contains_key("session_id"));
    assert!(!headers.contains_key("x-client-request-id"));
}

#[test]
fn explicit_prompt_cache_mode_uses_prompt_cache_options() {
    let model = sample_model("openai", "gpt-5.6", "https://api.openai.com/v1");
    let compat = ResponsesCompat {
        supports_explicit_prompt_cache_mode: true,
        supports_long_cache_retention: true,
        ..ResponsesCompat::default()
    };
    let context = empty_context();

    // long + long-retention support => ttl 30m, no prompt_cache_retention.
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let body = shared::build_responses_params(
        &model,
        &context,
        &options,
        &compat,
        "gpt-5.6",
        shared::OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("body");
    assert_eq!(
        body.get("prompt_cache_options").map(|v| v.to_string()),
        Some(r#"{"ttl":"30m"}"#.to_string())
    );
    assert!(body.get("prompt_cache_retention").is_none());

    // none => explicit mode-off marker, no session cache key.
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::None),
        session_id: Some("sess-1".to_string()),
        ..StreamOptions::default()
    };
    let body = shared::build_responses_params(
        &model,
        &context,
        &options,
        &compat,
        "gpt-5.6",
        shared::OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("body");
    assert_eq!(
        body.get("prompt_cache_options").map(|v| v.to_string()),
        Some(r#"{"mode":"explicit"}"#.to_string())
    );
    assert!(body.get("prompt_cache_key").is_none());
}

#[test]
fn legacy_models_keep_prompt_cache_retention_24h() {
    // Default compat (supportsExplicitPromptCacheMode false): long retention
    // still goes out as prompt_cache_retention "24h" (upstream
    // getPromptCacheRetention), and no prompt_cache_options is sent.
    let model = sample_model("openai", "gpt-5.5", "https://api.openai.com/v1");
    let compat = ResponsesCompat::from_model(&model);
    let options = StreamOptions {
        cache_retention: Some(CacheRetention::Long),
        ..StreamOptions::default()
    };
    let body = shared::build_responses_params(
        &model,
        &empty_context(),
        &options,
        &compat,
        "gpt-5.5",
        shared::OPENAI_TOOL_CALL_PROVIDERS,
        false,
        None,
    )
    .expect("body");
    assert_eq!(
        body.get("prompt_cache_retention").and_then(|v| v.as_str()),
        Some("24h")
    );
    assert!(body.get("prompt_cache_options").is_none());
}

#[test]
fn compat_defaults_mirror_upstream_getcompat() {
    let model = sample_model("openai", "gpt-5.5", "https://api.openai.com/v1");
    let compat = ResponsesCompat::from_model(&model);
    assert!(compat.supports_developer_role);
    assert!(!compat.supports_mid_convo_system_messages);
    assert!(compat.supports_long_cache_retention);
    assert!(!compat.supports_strict_mode);
    assert!(!compat.supports_openai_grammar_tools);
    assert!(!compat.supports_additional_tools);
    assert!(!compat.supports_tool_search);
    assert!(!compat.supports_explicit_prompt_cache_mode);
    assert!(compat.supports_max_output_tokens);

    // Explicit model.compat fields win.
    let mut model = sample_model("custom", "x", "https://example.invalid/v1");
    model.compat = Some(OpenAiCompat {
        supports_developer_role: Some(false),
        supports_mid_convo_system_messages: Some(true),
        supports_long_cache_retention: Some(false),
        supports_strict_mode: Some(true),
        supports_openai_grammar_tools: Some(true),
        supports_additional_tools: Some(true),
        supports_tool_search: Some(true),
        supports_explicit_prompt_cache_mode: Some(true),
        supports_max_output_tokens: Some(false),
        ..Default::default()
    });
    let compat = ResponsesCompat::from_model(&model);
    assert!(!compat.supports_developer_role);
    assert!(compat.supports_mid_convo_system_messages);
    assert!(!compat.supports_long_cache_retention);
    assert!(compat.supports_strict_mode);
    assert!(compat.supports_openai_grammar_tools);
    assert!(compat.supports_additional_tools);
    assert!(compat.supports_tool_search);
    assert!(compat.supports_explicit_prompt_cache_mode);
    assert!(!compat.supports_max_output_tokens);
}
