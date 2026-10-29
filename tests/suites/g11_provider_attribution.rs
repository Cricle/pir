//! G11 / P1-12: provider attribution headers (upstream
//! `provider-attribution.ts` + `telemetry.ts`). Pure-function tests for the
//! per-provider header injection and the install-telemetry gate.

use std::collections::BTreeMap;

use pir::ai::provider::{
    default_attribution_headers, merge_provider_attribution_headers, session_headers,
};
use pir::ai::types::{Model, ModelCost, ModelCostRates};

fn model(provider: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "openai-chat".into(),
        provider: provider.into(),
        base_url: base_url.into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 16_000,
        max_tokens: 2_000,
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

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn telemetry_disabled_injects_no_attribution() {
    let m = model("other", "https://openrouter.ai/api/v1");
    assert_eq!(default_attribution_headers(&m, false), None);
    // Session headers are independent of the telemetry gate (upstream
    // `getSessionHeaders` runs before the gate in the merge) — but they are
    // still opencode-scoped (provider id or opencode.ai host), so an
    // opencode model keeps them while telemetry is off.
    let opencode = model("opencode", "https://example.com");
    assert_eq!(
        session_headers(&opencode, Some("session-1")),
        Some(headers(&[
            ("x-opencode-session", "session-1"),
            ("x-opencode-client", "pi"),
        ]))
    );
    // A non-opencode model never carries session headers (upstream
    // `getSessionHeaders` returns undefined outside opencode), and the merge
    // drops it entirely when telemetry is off and nothing else is sent.
    let m = model("openrouter", "https://openrouter.ai/api/v1");
    assert_eq!(session_headers(&m, Some("session-1")), None);
    assert_eq!(
        merge_provider_attribution_headers(&m, false, Some("session-1"), None),
        None
    );
}

#[test]
fn openrouter_attribution_headers() {
    // Host match on any provider id.
    let m = model("custom", "https://openrouter.ai/api/v1");
    assert_eq!(
        default_attribution_headers(&m, true),
        Some(headers(&[
            ("HTTP-Referer", "https://pi.dev"),
            ("X-OpenRouter-Title", "pi"),
            ("X-OpenRouter-Categories", "cli-agent"),
        ]))
    );
    // Provider-id match on any base URL.
    let m = model("openrouter", "https://proxy.example/v1");
    assert!(default_attribution_headers(&m, true).is_some());
    // Unrelated provider + host: nothing.
    let m = model("custom", "https://api.example.com/v1");
    assert_eq!(default_attribution_headers(&m, true), None);
    assert_eq!(default_attribution_headers(&m, false), None);
}

#[test]
fn nvidia_nim_attribution_header() {
    let m = model("custom", "https://integrate.api.nvidia.com/v1");
    assert_eq!(
        default_attribution_headers(&m, true),
        Some(headers(&[("X-BILLING-INVOKE-ORIGIN", "Pi")]))
    );
    let m = model("nvidia", "https://example.com/v1");
    assert_eq!(
        default_attribution_headers(&m, true),
        Some(headers(&[("X-BILLING-INVOKE-ORIGIN", "Pi")]))
    );
}

#[test]
fn cloudflare_user_agent_header() {
    for (provider, base_url) in [
        ("cloudflare-workers-ai", "https://example.com"),
        ("cloudflare-ai-gateway", "https://example.com"),
        ("custom", "https://api.cloudflare.com/client/v4"),
        (
            "custom",
            "https://gateway.ai.cloudflare.com/v1/x/workers-ai",
        ),
    ] {
        let m = model(provider, base_url);
        assert_eq!(
            default_attribution_headers(&m, true),
            Some(headers(&[("User-Agent", "pi-coding-agent")])),
            "provider={provider} base_url={base_url}"
        );
    }
}

#[test]
fn opencode_session_headers_only_for_opencode() {
    for (provider, base_url) in [
        ("opencode", "https://example.com"),
        ("opencode-go", "https://example.com"),
        ("custom", "https://opencode.ai/zen"),
    ] {
        let m = model(provider, base_url);
        assert_eq!(
            session_headers(&m, Some("s-1")),
            Some(headers(&[
                ("x-opencode-session", "s-1"),
                ("x-opencode-client", "pi"),
            ])),
            "provider={provider} base_url={base_url}"
        );
    }
    let m = model("anthropic", "https://api.anthropic.com");
    assert_eq!(session_headers(&m, Some("s-1")), None);
    let m = model("opencode", "https://opencode.ai/zen");
    assert_eq!(session_headers(&m, None), None);
}

#[test]
fn merge_precedence_session_attribution_then_request() {
    // Upstream `mergeProviderAttributionHeaders` merges session headers under
    // attribution defaults under the request headers, each winning per header
    // name. An opencode model carries the session layer; an openrouter model
    // the attribution layer; the caller's explicit header wins over both.
    let opencode = model("opencode", "https://opencode.ai/zen");
    let merged = merge_provider_attribution_headers(
        &opencode,
        false,
        Some("sess-7"),
        Some(&headers(&[("x-opencode-session", "override-9")])),
    );
    assert_eq!(
        merged,
        Some(headers(&[
            // The caller's explicit header wins over the session default.
            ("x-opencode-session", "override-9"),
            ("x-opencode-client", "pi"),
        ]))
    );

    let m = model("openrouter", "https://openrouter.ai/api/v1");
    let merged = merge_provider_attribution_headers(
        &m,
        true,
        Some("sess-7"),
        Some(&headers(&[("HTTP-Referer", "https://override.example")])),
    );
    assert_eq!(
        merged,
        Some(headers(&[
            // The caller's explicit header wins over the default.
            ("HTTP-Referer", "https://override.example"),
            ("X-OpenRouter-Title", "pi"),
            ("X-OpenRouter-Categories", "cli-agent"),
            // OpenRouter is not an opencode provider/host: no session layer.
        ]))
    );
}

#[test]
fn merge_returns_none_when_nothing_to_send() {
    // Non-attribution provider, no session, no request headers.
    let m = model("anthropic", "https://api.anthropic.com");
    assert_eq!(
        merge_provider_attribution_headers(&m, true, None, None),
        None
    );
    // Telemetry off removes the attribution defaults; request headers keep it.
    let m = model("openrouter", "https://openrouter.ai/api/v1");
    assert_eq!(
        merge_provider_attribution_headers(&m, false, None, None),
        None
    );
    let kept =
        merge_provider_attribution_headers(&m, false, None, Some(&headers(&[("x-custom", "1")])));
    assert_eq!(kept, Some(headers(&[("x-custom", "1")])));
}
