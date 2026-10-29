//! Built-in model catalog integration tests: embedded resources load and
//! validate, sample entries match the upstream provider definitions, and the
//! registry wiring selects catalog providers with adapter dispatch.

use pir::ai::provider::ProviderRegistry;
use pir::ai::providers::{catalog, register_builtin_catalog};
use pir::ai::types::StreamOptions;

#[test]
fn catalog_covers_all_embedded_providers() {
    let providers = catalog::load_all().unwrap();
    let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
    for required in [
        "amazon-bedrock",
        "anthropic",
        "ant-ling",
        "azure",
        "baseten",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "deepseek",
        "fireworks",
        "github-copilot",
        "google",
        "google-vertex",
        "groq",
        "huggingface",
        "kimi-coding",
        "meta",
        "minimax",
        "minimax-cn",
        "mistral",
        "moonshotai",
        "moonshotai-cn",
        "nvidia",
        "openai",
        "openai-codex",
        "opencode",
        "opencode-go",
        "openrouter",
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "radius",
        "together",
        "typesafe",
        "vercel-ai-gateway",
        "xai",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "zai",
        "zai-coding-cn",
    ] {
        assert!(ids.contains(&required), "catalog missing {required}");
    }
}

#[test]
fn every_catalog_model_is_valid() {
    for provider in catalog::load_all().unwrap() {
        for model in &provider.models {
            assert_eq!(model.provider, provider.id, "{}", model.id);
            assert!(!model.id.is_empty());
            assert!(!model.name.is_empty());
            assert!(!model.api.is_empty());
            // A model either inherits the provider base URL or carries its
            // own; providers without any base URL (account-scoped endpoints
            // like Cloudflare or Azure) keep both empty.
            if !model.base_url.is_empty() {
                assert!(
                    model.base_url.starts_with("http://") || model.base_url.starts_with("https://"),
                    "{}.{} base url {}",
                    provider.id,
                    model.id,
                    model.base_url
                );
            } else {
                assert!(
                    provider.base_url.is_none(),
                    "{}.{} should inherit the provider base url",
                    provider.id,
                    model.id
                );
            }
            // Upstream copies the raw provider limits into the generated
            // catalog; OpenRouter reports some completion caps below 4096
            // (e.g. cohere/command-r-08-2024 at 4000) and one 4095-token
            // context window, so only positivity is guaranteed.
            assert!(model.context_window > 0, "{}", model.id);
            assert!(model.max_tokens > 0, "{}", model.id);
            assert!(!model.input.is_empty());
            assert!(model.cost.rates.input.is_finite() && model.cost.rates.input >= 0.0);
            assert!(model.cost.rates.output.is_finite() && model.cost.rates.output >= 0.0);
        }
    }
}

#[test]
fn sample_entries_match_upstream_definitions() {
    // Exact values from the upstream provider definitions (committed at HEAD).
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "anthropic",
            "claude-opus-4-8",
            "anthropic-messages",
            "https://api.anthropic.com",
        ),
        (
            "openai",
            "gpt-5.6-luna",
            "openai-responses",
            "https://api.openai.com/v1",
        ),
        (
            "openai-codex",
            "gpt-5.3-codex",
            "openai-codex-responses",
            "https://chatgpt.com/backend-api",
        ),
        (
            "google",
            "gemini-3.5-pro",
            "google-generative-ai",
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        ("xai", "grok-5", "openai-responses", "https://api.x.ai/v1"),
        (
            "groq",
            "llama-3.3-70b-versatile",
            "openai-completions",
            "https://api.groq.com/openai/v1",
        ),
        (
            "deepseek",
            "deepseek-reasoner",
            "openai-completions",
            "https://api.deepseek.com",
        ),
        (
            "mistral",
            "mistral-large-latest",
            "mistral-conversations",
            "https://api.mistral.ai",
        ),
        (
            "moonshotai",
            "kimi-k3",
            "openai-completions",
            "https://api.moonshot.ai/v1",
        ),
        (
            "moonshotai-cn",
            "kimi-k3",
            "openai-completions",
            "https://api.moonshot.cn/v1",
        ),
        (
            "zai",
            "glm-5.2",
            "openai-completions",
            "https://api.z.ai/api/coding/paas/v4",
        ),
        (
            "zai-coding-cn",
            "glm-5.2",
            "openai-completions",
            "https://open.bigmodel.cn/api/coding/paas/v4",
        ),
        (
            "qwen-token-plan",
            "qwen3.8-max",
            "openai-completions",
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        ),
        (
            "amazon-bedrock",
            "anthropic.claude-opus-4-8",
            "bedrock-converse-stream",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        ),
        (
            "fireworks",
            "accounts/fireworks/models/kimi-k3",
            "openai-completions",
            "https://api.fireworks.ai/inference/v1",
        ),
        (
            "together",
            "openai/gpt-oss-120b",
            "openai-completions",
            "https://api.together.ai/v1",
        ),
        (
            "nvidia",
            "deepseek-ai/deepseek-r1",
            "openai-completions",
            "https://integrate.api.nvidia.com/v1",
        ),
        (
            "huggingface",
            "deepseek-ai/DeepSeek-V4-Pro",
            "openai-completions",
            "https://router.huggingface.co/v1",
        ),
        (
            "kimi-coding",
            "kimi-for-coding",
            "openai-completions",
            "https://api.kimi.com/coding/v1",
        ),
        (
            "meta",
            "llama-4-maverick",
            "openai-responses",
            "https://api.meta.ai/v1",
        ),
        (
            "minimax",
            "MiniMax-M2.7",
            "anthropic-messages",
            "https://api.minimax.io/anthropic",
        ),
        (
            "minimax-cn",
            "MiniMax-M2.7",
            "anthropic-messages",
            "https://api.minimaxi.com/anthropic",
        ),
        (
            "github-copilot",
            "gpt-5.5",
            "openai-responses",
            "https://api.individual.githubcopilot.com",
        ),
    ];
    for (provider_id, model_id, api, base_url) in cases {
        let provider = catalog::load_provider(provider_id)
            .unwrap()
            .unwrap_or_else(|| panic!("provider {provider_id} missing"));
        let model = provider
            .models
            .iter()
            .find(|model| model.id == *model_id)
            .unwrap_or_else(|| panic!("{provider_id} is missing model {model_id}"));
        assert_eq!(model.api, *api, "{provider_id}/{model_id}");
        assert_eq!(model.base_url, *base_url, "{provider_id}/{model_id}");
    }
}

#[test]
fn registry_registers_builtin_catalog() {
    let mut registry = ProviderRegistry::default();
    let count = register_builtin_catalog(&mut registry).unwrap();
    assert_eq!(count, catalog::load_all().unwrap().len());
    assert_eq!(registry.providers().len(), count);
    // The registry can resolve catalog models by provider + id.
    let model = registry
        .get_model("anthropic", "claude-opus-4-8")
        .expect("anthropic catalog model");
    assert_eq!(model.api, "anthropic-messages");
    // Provider-level env-var auth metadata is attached.
    let provider = registry
        .providers()
        .into_iter()
        .find(|p| p.id() == "anthropic")
        .unwrap();
    assert!(!provider.api_key_auth().is_empty());
    // OAuth-capable providers advertise their flow.
    let flow = provider.oauth_flow().expect("anthropic oauth flow");
    assert_eq!(flow.name(), "Anthropic (Claude Pro/Max)");
    assert!(flow.is_subscription());
}

#[test]
fn registry_oauth_provider_ids_all_resolve_flows() {
    for provider_id in pir::ai::auth::OAUTH_PROVIDERS {
        let flow = pir::ai::auth::oauth_flow_for(provider_id, None)
            .unwrap_or_else(|| panic!("flow missing for {provider_id}"));
        assert!(!flow.name().is_empty());
    }
}

#[tokio::test]
async fn openai_responses_streams_without_adapter_gate() {
    let mut registry = ProviderRegistry::default();
    register_builtin_catalog(&mut registry).unwrap();
    // openai-responses now has a real adapter; streaming without an api key
    // must still yield a terminal Error message (auth failure), not a panic
    // or the former "adapter not available" gate.
    let model = registry
        .get_model("openai", "gpt-5.6-luna")
        .expect("openai catalog model");
    let stream = registry.stream_unchecked(
        model,
        pir::ai::types::Context::default(),
        StreamOptions::default(),
    );
    let message = stream.result().await.expect("terminal message");
    assert_eq!(message.stop_reason, pir::ai::types::StopReason::Error);
    let error = message.error_message.clone().unwrap_or_default();
    assert!(
        error.contains("No API key for provider: openai"),
        "unexpected error text: {error}"
    );
}
