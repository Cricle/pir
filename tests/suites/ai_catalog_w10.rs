//! OpenCode Go catalog sync (W10-B).
//!
//! The embedded catalog mirrors upstream pi v1.3.0's generated OpenCode Go
//! provider data (`packages/ai/src/providers/data/opencode-go.json`, built
//! from models.dev by `scripts/generate-models.ts`): the Gemini entry routes
//! through Google Generative AI, MiniMax M2.7/M3 and the Qwen entries route
//! through Anthropic Messages, and the remaining chat models stay on the
//! OpenAI-compatible protocols. These checks pin the upstream model set, the
//! per-model wire API, and representative cost/limit/compat values.

use std::collections::BTreeSet;

use pir::ai::providers::catalog::{load_provider, supported_apis, BuiltinProvider};
use pir::ai::types::{Model, SessionAffinityFormat};

/// Upstream pi v1.3.0 OpenCode Go chat models and their wire APIs (flattened
/// from the generated catalog's API groups, in model-id order).
const UPSTREAM_MODELS: &[(&str, &str)] = &[
    ("cat-jam-free", "openai-completions"),
    ("claude-haiku-5-5", "anthropic-messages"),
    ("deepseek-v4-pro", "openai-completions"),
    ("deepseek-v4.1-flash", "openai-completions"),
    ("gemini-4-argon-contributor", "google-generative-ai"),
    ("glm-5.2", "openai-completions"),
    ("glm-5.3", "openai-completions"),
    ("glm-5.3-flash", "openai-completions"),
    ("gpt-5.6-luna", "openai-responses"),
    ("gpt-6-luna", "openai-responses"),
    ("grok-4.7", "openai-responses"),
    ("hy3", "openai-completions"),
    ("hy4-preview", "openai-completions"),
    ("kimi-k2.7-code", "openai-completions"),
    ("kimi-k3", "openai-completions"),
    ("kimi-k3.1-pro", "openai-completions"),
    ("longcat-2.0", "openai-completions"),
    ("longcat-2.5-preview-free", "openai-completions"),
    ("mimo-v2.5", "openai-completions"),
    ("mimo-v2.5-pro", "openai-completions"),
    ("mimo-v2.6-flash", "openai-completions"),
    ("mimo-v2.6-pro", "openai-completions"),
    ("minimax-m2.7", "anthropic-messages"),
    ("minimax-m3", "anthropic-messages"),
    ("muse-spark-1.2-contributor", "openai-responses"),
    ("muse-spark-1.3-contributor", "openai-responses"),
    ("qwen3.7-plus", "anthropic-messages"),
    ("qwen3.8-flash", "anthropic-messages"),
    ("qwen3.8-max", "anthropic-messages"),
    ("space-bunny", "openai-completions"),
    ("step-5-preview-free", "openai-completions"),
];

const OPENCODE_GO_JSON: &str = include_str!("../../src/ai/providers/catalog/opencode-go.json");

fn opencode_go() -> BuiltinProvider {
    load_provider("opencode-go")
        .expect("opencode-go.json parses through the production catalog loader")
        .expect("opencode-go is an embedded provider")
}

fn model<'a>(provider: &'a BuiltinProvider, id: &str) -> &'a Model {
    provider
        .models
        .iter()
        .find(|model| model.id == id)
        .unwrap_or_else(|| panic!("opencode-go/{id} missing from the catalog"))
}

#[test]
fn catalog_loads_with_upstream_identity() {
    let provider = opencode_go();
    assert_eq!(provider.id, "opencode-go");
    assert_eq!(provider.display_name, "OpenCode Go");
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://opencode.ai/inference/go/v1")
    );
    assert_eq!(provider.models.len(), UPSTREAM_MODELS.len());

    for model in &provider.models {
        assert_eq!(model.provider, "opencode-go");
        assert!(!model.id.is_empty());
        assert!(!model.name.is_empty());
        assert!(!model.api.is_empty());
        assert!(!model.input.is_empty(), "{} input modalities", model.id);
        assert!(model.context_window >= 4096, "{} context", model.id);
        assert!(model.max_tokens >= 4096, "{} max tokens", model.id);
        assert!(
            supported_apis().contains(&model.api.as_str()),
            "{} uses unsupported api {}",
            model.id,
            model.api
        );
    }
}

#[test]
fn every_upstream_model_id_is_present() {
    let provider = opencode_go();
    let actual: BTreeSet<&str> = provider
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    let expected: BTreeSet<&str> = UPSTREAM_MODELS.iter().map(|(id, _)| *id).collect();
    assert_eq!(actual, expected);

    for (id, _) in UPSTREAM_MODELS {
        let model = model(&provider, id);
        assert_eq!(model.id, *id);
    }
}

#[test]
fn protocol_selection_matches_upstream_per_model() {
    let provider = opencode_go();
    for (id, api) in UPSTREAM_MODELS {
        let model = model(&provider, id);
        assert_eq!(model.api, *api, "{id} api");
        if *api == "anthropic-messages" {
            // The Anthropic SDK appends /v1/messages; upstream keeps the
            // provider root without the OpenAI-compatible /v1 suffix.
            assert_eq!(
                model.base_url, "https://opencode.ai/inference/go",
                "{id} anthropic base url"
            );
        } else {
            assert_eq!(
                model.base_url, "https://opencode.ai/inference/go/v1",
                "{id} base url"
            );
        }
    }

    let count = |api: &str| {
        provider
            .models
            .iter()
            .filter(|model| model.api == api)
            .count()
    };
    assert_eq!(count("anthropic-messages"), 6);
    assert_eq!(count("google-generative-ai"), 1);
    assert_eq!(count("openai-completions"), 19);
    assert_eq!(count("openai-responses"), 5);
}

#[test]
fn catalog_has_no_duplicate_models() {
    let provider = opencode_go();
    let ids: Vec<&str> = provider
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "duplicate model ids: {ids:?}");

    // The JSON map key must equal the entry's id, so a model cannot be
    // listed twice under different keys or API groups.
    let raw: serde_json::Value = serde_json::from_str(OPENCODE_GO_JSON).expect("valid JSON");
    let models = raw["models"].as_object().expect("models object");
    assert_eq!(models.len(), UPSTREAM_MODELS.len());
    for (key, entry) in models {
        assert_eq!(entry["id"].as_str(), Some(key.as_str()), "key/id mismatch");
    }
}

#[test]
fn representative_entries_match_upstream_values() {
    let provider = opencode_go();

    // Google Generative AI entry added in v1.3.0 (models.dev @ai-sdk/google).
    let gemini = model(&provider, "gemini-4-argon-contributor");
    assert_eq!(gemini.name, "Gemini 4 Argon Contributor");
    assert!(gemini.reasoning);
    assert_eq!(gemini.input, vec!["text".to_string(), "image".to_string()]);
    assert_eq!(gemini.cost.rates.input, 4.0);
    assert_eq!(gemini.cost.rates.output, 20.0);
    assert_eq!(gemini.cost.rates.cache_read, 0.2);
    assert_eq!(gemini.cost.rates.cache_write, 0.0);
    assert_eq!(gemini.context_window, 1_048_576);
    assert_eq!(gemini.max_tokens, 65_536);
    let map = gemini.thinking_level_map.as_ref().expect("thinking map");
    assert_eq!(map.get("low").cloned().flatten().as_deref(), Some("low"));
    assert_eq!(
        map.get("medium").cloned().flatten().as_deref(),
        Some("medium")
    );
    assert_eq!(map.get("high").cloned().flatten().as_deref(), Some("high"));
    assert_eq!(map.get("max"), Some(&None));

    // MiniMax M2.7 on OpenCode Go uses Anthropic Messages (v1.3.0 fix);
    // models.dev still reports the model as @ai-sdk/anthropic.
    let minimax = model(&provider, "minimax-m2.7");
    assert_eq!(minimax.name, "MiniMax-M2.7");
    assert!(minimax.reasoning);
    assert_eq!(minimax.input, vec!["text".to_string()]);
    assert_eq!(minimax.cost.rates.input, 0.3);
    assert_eq!(minimax.cost.rates.output, 1.2);
    assert_eq!(minimax.cost.rates.cache_read, 0.06);
    assert_eq!(minimax.cost.rates.cache_write, 0.375);
    assert_eq!(minimax.context_window, 204_800);
    assert_eq!(minimax.max_tokens, 131_072);

    // Claude Haiku 5.5 carries the 100k prompt-length pricing tier and the
    // adaptive-thinking/temperature compat flags.
    let haiku = model(&provider, "claude-haiku-5-5");
    assert_eq!(haiku.cost.tiers.len(), 1);
    assert_eq!(haiku.cost.tiers[0].input_tokens_above, 100_000);
    assert_eq!(haiku.cost.tiers[0].rates.input, 0.5);
    assert_eq!(haiku.cost.tiers[0].rates.output, 2.5);
    let compat = haiku.compat.as_ref().expect("haiku compat");
    assert_eq!(compat.force_adaptive_thinking, Some(true));
    assert_eq!(compat.supports_temperature, Some(false));

    // Kimi K3 stays on OpenAI Chat Completions with the upstream mid-convo
    // system-message/tool-addition flags.
    let kimi = model(&provider, "kimi-k3");
    assert_eq!(kimi.name, "Kimi K3");
    assert_eq!(kimi.cost.rates.input, 3.0);
    assert_eq!(kimi.cost.rates.output, 15.0);
    assert_eq!(kimi.cost.rates.cache_read, 0.3);
    assert_eq!(kimi.context_window, 1_048_576);
    assert_eq!(kimi.max_tokens, 131_072);
    let compat = kimi.compat.as_ref().expect("kimi compat");
    assert_eq!(compat.supports_mid_convo_system_messages, Some(true));
    assert_eq!(compat.supports_mid_convo_tool_additions, Some(true));

    // Kimi K3.1 Pro follows the same Chat Completions path with the
    // 2/10/0.2 rates and the low/high/max effort map.
    let kimi_pro = model(&provider, "kimi-k3.1-pro");
    assert_eq!(kimi_pro.name, "Kimi K3.1 Pro");
    assert_eq!(kimi_pro.cost.rates.input, 2.0);
    assert_eq!(kimi_pro.cost.rates.output, 10.0);
    assert_eq!(kimi_pro.cost.rates.cache_read, 0.2);
    assert_eq!(kimi_pro.cost.rates.cache_write, 0.0);
    assert_eq!(kimi_pro.context_window, 1_048_576);
    assert_eq!(kimi_pro.max_tokens, 131_072);
    let map = kimi_pro
        .thinking_level_map
        .as_ref()
        .expect("k3.1 thinking map");
    assert_eq!(map.get("low").cloned().flatten().as_deref(), Some("low"));
    assert_eq!(map.get("medium"), Some(&None));
    assert_eq!(map.get("high").cloned().flatten().as_deref(), Some("high"));
    assert_eq!(map.get("max").cloned().flatten().as_deref(), Some("max"));
    assert_eq!(map.get("off"), Some(&None));
    assert!(
        kimi_pro
            .compat
            .as_ref()
            .is_some_and(|compat| compat.supports_reasoning_effort),
        "Kimi K3.1 Pro sends reasoning_effort"
    );

    // GPT-5.6 Luna uses OpenAI Responses, including the 272k pricing tier
    // and the no-session affinity format.
    let luna = model(&provider, "gpt-5.6-luna");
    assert_eq!(luna.cost.rates.input, 0.2);
    assert_eq!(luna.cost.rates.output, 1.2);
    assert_eq!(luna.cost.tiers.len(), 1);
    assert_eq!(luna.cost.tiers[0].input_tokens_above, 272_000);
    assert_eq!(luna.context_window, 1_050_000);
    assert_eq!(luna.max_tokens, 128_000);
    let compat = luna.compat.as_ref().expect("luna compat");
    assert_eq!(
        compat.session_affinity_format,
        Some(SessionAffinityFormat::OpenaiNosession)
    );
    assert_eq!(compat.supports_additional_tools, Some(true));
}
