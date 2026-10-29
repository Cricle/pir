//! Full built-in catalog snapshot lint (W10-F).
//!
//! The catalog is generated from an upstream pi-ai data directory by
//! `scripts/gen_catalog.mjs` and checked with `--check`; these tests validate
//! the checked-in result independently of the generator:
//!
//! 1. every embedded provider file parses through the production loader and
//!    is canonical compact JSON (no insignificant whitespace, LF, one trailing
//!    newline);
//! 2. provider ids are globally unique and model ids are unique per provider
//!    and model kind (chat / image / classifier namespaces are separate, as
//!    upstream keys them by type);
//! 3. every `api` value names a protocol pir ships (or a known image /
//!    classifier protocol);
//! 4. representative provider counts cover the drift list W10-B measured;
//! 5. no entry is an obvious placeholder and every chat entry has usable
//!    limits and non-negative costs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use pir::ai::providers::catalog::{self, CATALOG_FILES};

const IMAGE_APIS: &[&str] = &["openrouter-images"];
const CLASSIFIER_APIS: &[&str] = &[
    "typesafe-system-one",
    "cloudflare-workers-ai-system-one",
    "openai-decisions",
];

fn catalog_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ai/providers/catalog")
}

fn read_catalog(file: &str) -> (String, Value) {
    let path = catalog_dir().join(file);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    (text, value)
}

fn models_of<'a>(raw: &'a Value, provider: &str) -> &'a serde_json::Map<String, Value> {
    raw["models"]
        .as_object()
        .unwrap_or_else(|| panic!("{provider}: models is not an object"))
}

fn kind_of(entry: &Value) -> &str {
    entry.get("type").and_then(Value::as_str).unwrap_or("chat")
}

fn id_of<'a>(entry: &'a Value, provider: &str, key: &str) -> &'a str {
    entry["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{provider}/{key}: id is not a string"))
}

#[test]
fn every_catalog_file_parses_and_is_canonical() {
    let mut files = BTreeSet::new();
    let providers = catalog::load_all().expect("the embedded catalog parses");
    assert_eq!(
        providers.len(),
        CATALOG_FILES.len(),
        "load_all returns one provider per embedded file"
    );

    for (provider_id, file) in CATALOG_FILES {
        assert!(files.insert(*file), "duplicate catalog file {file}");
        let (text, raw) = read_catalog(file);
        assert!(
            raw["name"].as_str().is_some_and(|name| !name.is_empty()),
            "{file}: provider name"
        );
        assert!(
            raw["models"].is_object(),
            "{file}: models must be an object"
        );
        assert!(
            text.ends_with('\n') && !text.ends_with("\n\n"),
            "{file}: exactly one trailing newline"
        );

        let provider = catalog::load_provider(provider_id)
            .unwrap_or_else(|error| panic!("{file}: production loader failed: {error}"))
            .unwrap_or_else(|| panic!("{file}: provider {provider_id} is not embedded"));
        assert_eq!(provider.id, *provider_id);
        assert_eq!(
            provider.models.len(),
            models_of(&raw, provider_id)
                .values()
                .filter(|entry| kind_of(entry) == "chat")
                .count(),
            "{file}: chat entries load 1:1"
        );
    }

    assert_eq!(
        files.len(),
        CATALOG_FILES.len(),
        "every CATALOG_FILES entry names a distinct file"
    );
}

#[test]
fn provider_and_model_ids_are_unique() {
    let mut provider_ids = BTreeSet::new();
    for (provider_id, file) in CATALOG_FILES {
        assert!(
            provider_ids.insert(*provider_id),
            "duplicate provider id {provider_id}"
        );

        let (_, raw) = read_catalog(file);
        let models = models_of(&raw, provider_id);
        let mut typed_ids: BTreeMap<(String, String), String> = BTreeMap::new();

        for (key, entry) in models {
            let id = id_of(entry, provider_id, key);
            let kind = kind_of(entry);
            assert!(!id.is_empty(), "{provider_id}/{key}: empty id");
            assert!(
                !entry["name"].as_str().unwrap_or_default().trim().is_empty(),
                "{provider_id}/{key}: empty name"
            );

            match kind {
                "chat" => assert_eq!(key, id, "{provider_id}: chat key must equal id"),
                "image" | "classifier" => assert_eq!(
                    key,
                    &format!("{kind}:{id}"),
                    "{provider_id}: non-chat key convention"
                ),
                other => panic!("{provider_id}/{key}: unknown model type {other}"),
            }

            assert!(
                typed_ids
                    .insert((kind.to_string(), id.to_string()), key.clone())
                    .is_none(),
                "{provider_id}: duplicate {kind} model id {id}"
            );
        }

        let chat_ids: BTreeSet<&str> = models
            .iter()
            .filter(|(_, entry)| kind_of(entry) == "chat")
            .map(|(_, entry)| id_of(entry, provider_id, "chat"))
            .collect();
        assert_eq!(
            chat_ids.len(),
            models
                .values()
                .filter(|entry| kind_of(entry) == "chat")
                .count(),
            "{provider_id}: chat model ids must be unique"
        );
    }
}

#[test]
fn protocol_values_are_known() {
    for (provider_id, file) in CATALOG_FILES {
        let (_, raw) = read_catalog(file);
        for (key, entry) in models_of(&raw, provider_id) {
            let api = entry["api"]
                .as_str()
                .unwrap_or_else(|| panic!("{provider_id}/{key}: api is not a string"));
            assert!(!api.is_empty(), "{provider_id}/{key}: empty api");
            match kind_of(entry) {
                "chat" => {
                    assert!(
                        catalog::api_feature(api).is_some(),
                        "{provider_id}/{key}: unknown chat api '{api}'"
                    );
                    assert!(
                        !IMAGE_APIS.contains(&api) && !CLASSIFIER_APIS.contains(&api),
                        "{provider_id}/{key}: non-chat api '{api}' on a chat entry"
                    );
                }
                "image" => assert!(
                    IMAGE_APIS.contains(&api),
                    "{provider_id}/{key}: unknown image api '{api}'"
                ),
                "classifier" => assert!(
                    CLASSIFIER_APIS.contains(&api),
                    "{provider_id}/{key}: unknown classifier api '{api}'"
                ),
                other => panic!("{provider_id}/{key}: unknown model type {other}"),
            }
        }
    }
}

#[test]
fn representative_provider_counts_match_snapshot() {
    // The upstream W10-B drift list, plus the largest refreshed providers.
    // `total` counts every catalog entry (chat + image + classifier); `chat`
    // counts only entries the chat registry exposes. Legacy-only entries kept
    // in scripts/catalog-overrides.json are included in both numbers.
    let expected: &[(&str, usize, usize)] = &[
        ("amazon-bedrock", 197, 197),
        ("anthropic", 17, 17),
        ("google", 24, 24),
        ("google-vertex", 16, 16),
        ("kimi-coding", 6, 6),
        ("meta", 7, 7),
        ("minimax", 4, 4),
        ("openai", 45, 44),
        ("opencode", 86, 84),
        ("opencode-go", 31, 31),
        ("xai", 7, 7),
        ("zai", 7, 7),
    ];

    for (provider_id, total, chat) in expected {
        let (_, file) = CATALOG_FILES
            .iter()
            .find(|(id, _)| id == provider_id)
            .unwrap_or_else(|| panic!("{provider_id} missing from CATALOG_FILES"));
        let (_, raw) = read_catalog(file);
        let models = models_of(&raw, provider_id);
        assert_eq!(models.len(), *total, "{provider_id}: total catalog entries");
        assert_eq!(
            models
                .values()
                .filter(|entry| kind_of(entry) == "chat")
                .count(),
            *chat,
            "{provider_id}: chat entries"
        );
    }

    let total_models: usize = CATALOG_FILES
        .iter()
        .map(|(provider_id, file)| {
            let (_, raw) = read_catalog(file);
            models_of(&raw, provider_id).len()
        })
        .sum();
    assert_eq!(total_models, 1756, "whole catalog model count");
}

/// Upstream 1.4.0 removed `gpt-5.5` and `gpt-5.3-codex-spark` from
/// `openai-codex`: the Codex backend rejects both for ChatGPT accounts
/// (pi#11173). `openai` keeps its own entries; only this provider drops them.
///
/// The 1.4.0 additions come from the published `@earendil-works/pi-ai@1.4.0`
/// `dist/providers/data/*.json` (the MiniMax rows, including the CN rates).
/// The Kimi K3.1 Pro / K3.1 Pro Highspeed family is an `[Unreleased]`
/// upstream change after 1.4.0, so its rows follow the checked-in
/// `scripts/generate-models.ts` rules applied to the models.dev payload that
/// generator fetches: Kimi Coding uses `KIMI_CODING_IMPLIED_COSTS`, Moonshot
/// zeroes Kimi cache writes, and the CN rows use the CN payload's rates.
/// Metadata that is not in either source is left out rather than guessed.
#[test]
fn openai_codex_excludes_the_backend_rejected_models() {
    let (_, raw) = read_catalog("openai-codex.json");
    let models = models_of(&raw, "openai-codex");
    for removed in ["gpt-5.5", "gpt-5.3-codex-spark"] {
        assert!(
            !models.contains_key(removed),
            "openai-codex must not list `{removed}` (rejected by the Codex backend)"
        );
    }
    for kept in ["gpt-5.3-codex", "gpt-5.6-sol"] {
        assert!(
            models.contains_key(kept),
            "openai-codex keeps the accepted model `{kept}`"
        );
    }
}

#[test]
fn upstream_140_additions_match_the_generator_metadata() {
    fn model<'a>(raw: &'a Value, provider: &str, id: &str) -> &'a Value {
        models_of(raw, provider)
            .get(id)
            .unwrap_or_else(|| panic!("{provider}/{id} missing from the catalog"))
    }
    fn rate(model: &Value, field: &str) -> f64 {
        model["cost"][field]
            .as_f64()
            .unwrap_or_else(|| panic!("cost.{field} missing"))
    }
    fn thinking(model: &Value, level: &str) -> Option<String> {
        model["thinkingLevelMap"][level].as_str().map(str::to_owned)
    }

    // MiniMax-M3.1-Flash: effort low..max (never off), adaptive thinking,
    // 1M context / 512k output. The CN row carries the CN rates.
    for (provider, input, output, cache_read) in [
        ("minimax", 0.15, 0.6, 0.015),
        ("minimax-cn", 0.1494, 0.5976, 0.0149),
    ] {
        let (_, raw) = read_catalog(&format!("{provider}.json"));
        let flash = model(&raw, provider, "MiniMax-M3.1-Flash");
        assert_eq!(flash["api"], "anthropic-messages");
        assert_eq!(flash["reasoning"], true);
        assert_eq!(flash["input"], serde_json::json!(["text", "image"]));
        assert_eq!(rate(flash, "input"), input, "{provider} input");
        assert_eq!(rate(flash, "output"), output, "{provider} output");
        assert_eq!(rate(flash, "cacheRead"), cache_read, "{provider} cacheRead");
        assert_eq!(rate(flash, "cacheWrite"), 0.0);
        assert_eq!(flash["contextWindow"], 1_000_000);
        assert_eq!(flash["maxTokens"], 524_288);
        assert_eq!(flash["compat"]["forceAdaptiveThinking"], true);
        assert_eq!(thinking(flash, "off"), None);
        assert_eq!(thinking(flash, "minimal"), None);
        for level in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                thinking(flash, level).as_deref(),
                Some(level),
                "MiniMax-M3.1-Flash thinking level {level}"
            );
        }
    }

    // Kimi Coding: KIMI_CODING_IMPLIED_COSTS (2/10 Pro, 4/20 Highspeed),
    // 1M context and the backend's 128k output cap.
    let (_, raw) = read_catalog("kimi-coding.json");
    for (id, input, output) in [("k3.1-pro", 2.0, 10.0), ("k3.1-pro-highspeed", 4.0, 20.0)] {
        let kimi = model(&raw, "kimi-coding", id);
        assert_eq!(kimi["api"], "openai-completions");
        assert_eq!(rate(kimi, "input"), input);
        assert_eq!(rate(kimi, "output"), output);
        assert_eq!(rate(kimi, "cacheRead"), 0.2);
        assert_eq!(rate(kimi, "cacheWrite"), 0.0);
        assert_eq!(kimi["contextWindow"], 1_048_576);
        assert_eq!(kimi["maxTokens"], 131_072);
        assert_eq!(thinking(kimi, "low").as_deref(), Some("low"));
        assert_eq!(thinking(kimi, "medium"), None);
        assert_eq!(thinking(kimi, "high").as_deref(), Some("high"));
        assert_eq!(thinking(kimi, "max").as_deref(), Some("max"));
        assert_eq!(kimi["compat"]["supportsMidConvoToolAdditions"], true);
    }

    // Moonshot global + CN: the same K3-family request metadata, with cache
    // writes zeroed (Moonshot does not bill them). The CN rows carry the CN
    // payload's rates.
    let moonshot_cases = [
        ("moonshotai", "kimi-k3.1-pro", 2.0, 10.0, 0.2),
        ("moonshotai", "kimi-k3.1-pro-highspeed", 4.0, 20.0, 0.2),
        ("moonshotai-cn", "kimi-k3.1-pro", 1.9437, 10.1671, 0.1944),
        (
            "moonshotai-cn",
            "kimi-k3.1-pro-highspeed",
            3.8874,
            20.3342,
            0.1944,
        ),
    ];
    for (provider, id, input, output, cache_read) in moonshot_cases {
        let (_, raw) = read_catalog(&format!("{provider}.json"));
        let kimi = model(&raw, provider, id);
        assert_eq!(rate(kimi, "input"), input, "{provider}/{id} input");
        assert_eq!(rate(kimi, "output"), output, "{provider}/{id} output");
        assert_eq!(
            rate(kimi, "cacheRead"),
            cache_read,
            "{provider}/{id} cacheRead"
        );
        assert_eq!(rate(kimi, "cacheWrite"), 0.0);
        assert_eq!(kimi["contextWindow"], 1_048_576);
        assert_eq!(kimi["maxTokens"], 1_048_576);
        assert_eq!(thinking(kimi, "low").as_deref(), Some("low"));
        assert_eq!(thinking(kimi, "medium"), None);
        assert_eq!(thinking(kimi, "high").as_deref(), Some("high"));
        assert_eq!(thinking(kimi, "max").as_deref(), Some("max"));
        assert_eq!(kimi["compat"]["supportsReasoningEffort"], true);
    }

    // OpenCode Go carries the Pro entry only, at its 128k output cap.
    let (_, raw) = read_catalog("opencode-go.json");
    let kimi = model(&raw, "opencode-go", "kimi-k3.1-pro");
    assert_eq!(rate(kimi, "input"), 2.0);
    assert_eq!(rate(kimi, "output"), 10.0);
    assert_eq!(rate(kimi, "cacheRead"), 0.2);
    assert_eq!(rate(kimi, "cacheWrite"), 0.0);
    assert_eq!(kimi["contextWindow"], 1_048_576);
    assert_eq!(kimi["maxTokens"], 131_072);
    assert_eq!(thinking(kimi, "low").as_deref(), Some("low"));
    assert_eq!(thinking(kimi, "high").as_deref(), Some("high"));
    assert_eq!(thinking(kimi, "max").as_deref(), Some("max"));
    assert_eq!(kimi["compat"]["supportsMidConvoToolAdditions"], true);
    assert_eq!(kimi["compat"]["supportsReasoningEffort"], true);
}
#[test]
fn no_placeholder_models() {
    const PLACEHOLDER_MARKERS: &[&str] = &[
        "placeholder",
        "dummy",
        "todo",
        "fixme",
        "tbd",
        "example model",
        "sample model",
        "test model",
        "unknown model",
    ];

    for (provider_id, file) in CATALOG_FILES {
        let (_, raw) = read_catalog(file);
        for (key, entry) in models_of(&raw, provider_id) {
            let id = id_of(entry, provider_id, key);
            let name = entry["name"].as_str().unwrap_or_default();
            let haystack = format!("{} {}", id.to_ascii_lowercase(), name.to_ascii_lowercase());
            for marker in PLACEHOLDER_MARKERS {
                assert!(
                    !haystack.contains(marker),
                    "{provider_id}/{key}: placeholder marker '{marker}' in '{name}'"
                );
            }

            let cost = entry
                .get("cost")
                .and_then(Value::as_object)
                .unwrap_or_else(|| panic!("{provider_id}/{key}: cost missing"));
            for field in ["input", "output", "cacheRead", "cacheWrite"] {
                let rate = cost
                    .get(field)
                    .and_then(Value::as_f64)
                    .unwrap_or_else(|| panic!("{provider_id}/{key}: cost.{field} missing"));
                assert!(
                    rate.is_finite() && rate >= 0.0,
                    "{provider_id}/{key}: cost.{field} = {rate}"
                );
            }
            if let Some(tiers) = cost.get("tiers").and_then(Value::as_array) {
                for (index, tier) in tiers.iter().enumerate() {
                    let above = tier
                        .get("inputTokensAbove")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| {
                            panic!("{provider_id}/{key}: tier {index} inputTokensAbove")
                        });
                    assert!(above > 0, "{provider_id}/{key}: tier {index} boundary");
                }
            }

            if kind_of(entry) == "chat" {
                assert!(
                    entry
                        .get("contextWindow")
                        .and_then(Value::as_u64)
                        .is_some_and(|value| value > 0),
                    "{provider_id}/{key}: chat contextWindow"
                );
                assert!(
                    entry
                        .get("maxTokens")
                        .and_then(Value::as_u64)
                        .is_some_and(|value| value > 0),
                    "{provider_id}/{key}: chat maxTokens"
                );
                assert!(
                    entry
                        .get("input")
                        .and_then(Value::as_array)
                        .is_some_and(|input| !input.is_empty()),
                    "{provider_id}/{key}: chat input modalities"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minified-snapshot golden (W19-Q4)
// ---------------------------------------------------------------------------

/// SHA-256 of each embedded catalog file with insignificant JSON whitespace
/// removed, captured from the pretty-printed snapshot immediately before the
/// W19-Q4 minification. Whitespace is stripped lexically, so the digest pins
/// key order, numeric spelling and every string byte while ignoring only the
/// bytes the minifier removes.
const CATALOG_COMPACT_SHA256: &[(&str, &str)] = &[
    (
        "amazon-bedrock.json",
        "5091f89c35b5e4c83ce905dce57ffc298a4693e935e0b1335510f35b768ace79",
    ),
    (
        "ant-ling.json",
        "3c200b5a6cdf386c110324e26417c659977b92da696498218ba159e502b95bb5",
    ),
    (
        "anthropic.json",
        "01e4ea3ff2a3e92112d52c653892819aac4b69315186cee176c3673ef3bddae5",
    ),
    (
        "azure.json",
        "7b6d1414a46ed98b3d39586779e08466c83562f12e4b89df52db3f44b8a3a7cd",
    ),
    (
        "baseten.json",
        "b7797f885f390979c2c284c49da0249387d929ec29185533cfba5a49f83410ce",
    ),
    (
        "cerebras.json",
        "048573b69f24c067a88544747e0328752322669cdb51389c22113f3ac006599c",
    ),
    (
        "cloudflare-ai-gateway.json",
        "04f506fc9ad5ec8cf78d61dcb34574c5727a691765d5498f634f2345db4759b7",
    ),
    (
        "cloudflare-workers-ai.json",
        "64222af57d1bf3a61c2f75c92b11831b82acc69ae02a374d1ff831a49fbc9464",
    ),
    (
        "deepseek.json",
        "b78cb0f447a6476531d1fe6d76f560ed214b428fae1302915bd70dc6e13e67fb",
    ),
    (
        "fireworks.json",
        "738f39ab41849cc51c774a99b7c133b18c6fc0e92e14c411cee849ac292a6e46",
    ),
    (
        "github-copilot.json",
        "245f2ccb0d58f5edde86137aed2e848406f7302dec4ff22e0b3d37763763d738",
    ),
    (
        "google-vertex.json",
        "79a66a58a9b9f46d0a197eec2599c92e32d0522ef8b9524b36b90fb430b1e955",
    ),
    (
        "google.json",
        "458cba0b7fc16676b7017252356deb92a81bd15a4680a2b763d847f03ed69847",
    ),
    (
        "groq.json",
        "128f2ca152b538d57fe3e242c955466574cd48e2bce477170620d5e8dfd2e4fd",
    ),
    (
        "huggingface.json",
        "bc152596a2b96a49eb0fc3f9d24e1e855321e0137e18c533efdff8441fa08a72",
    ),
    (
        "kimi-coding.json",
        "47045ea6f0ba6ec9382354c285cac2725bc71fd63a3d82b4f73b2b0dafffd378",
    ),
    (
        "meta.json",
        "627b79bbb2f04670b57b908d0414ad74bfcc9834b47ae576686b0e92e334b534",
    ),
    (
        "minimax-cn.json",
        "d3afe4da9358ba7b04979d07e78164e19caf69b188d3cd8743142d40d36ef298",
    ),
    (
        "minimax.json",
        "414c56c4bcb520a24d808ea87a6247f5f2859ed16bff8de2659620ac33a0732f",
    ),
    (
        "mistral.json",
        "e91827e5ce204428766b46459d819d8d72ecfc3fdb407d1f776f1a8de837adb0",
    ),
    (
        "moonshotai-cn.json",
        "528cb3446322ecca8fb2ef45522897728c75e394e1b2cb0df2493b298aa458d8",
    ),
    (
        "moonshotai.json",
        "e35f1689cbaf8dabf106670b8e2cfe8f9da39585f79ee1c739be092942bc7144",
    ),
    (
        "nvidia.json",
        "43d4faba6b7431dca50449eddfad47fbd460bed79bcb3ee6266976b93c6bb47c",
    ),
    (
        "openai-codex.json",
        "eb76782563ad736c20fd7a9918cbd7918008c76bdb088482c9242baea4be5aa2",
    ),
    (
        "openai.json",
        "c31ab22b2509671d666124275b41ccf63c43eb6782527629e24ea171fd3a24d6",
    ),
    (
        "opencode-go.json",
        "456c957449a266817201463c841841beb0361478443a240e576b5d2c30b78ee9",
    ),
    (
        "opencode.json",
        "696b3b40140864fd505c6a4ad81ce08ce285d27a61716ac841c0e2d720b8ea57",
    ),
    (
        "openrouter.json",
        "e4ff445c114a936e8e8ffd7853f6adac097472d78a45ed1a79d3c2665ea68391",
    ),
    (
        "qwen-token-plan-cn.json",
        "1a0f4764689d9dbdea418bf43a69453781e30b2d7a699a478607691c406a359b",
    ),
    (
        "qwen-token-plan-individual.json",
        "c3ad99862a62e3b36215a3cdb0eee0a963227b35f1cd0ae9448727fb0ca115a0",
    ),
    (
        "qwen-token-plan.json",
        "0bcb793fa4aee3b71a7ffa635eb7f1a4ff376a508b6e7c60bbc0ed4313fab417",
    ),
    (
        "radius.json",
        "efe0fedbac2924eb74bad298095a4810971a0bf2ea5e440ab73b90da9f055d09",
    ),
    (
        "together.json",
        "6d3becc9578339735a79611983777433477de48ec408294e0be7a24f67f86978",
    ),
    (
        "typesafe.json",
        "46458afb503c345261d0d684c1d18b70640cbf3c70270033202caf9b2f976e0d",
    ),
    (
        "vercel-ai-gateway.json",
        "bc9d5b44f513833f65a8a14c38f1cb39dbc7d952a7fbec8f2f22f40a7eab6fcd",
    ),
    (
        "xai.json",
        "ac9d85e1cf2f7b2afcfec473af4d929208b5abe30289f4137202499c20b3906b",
    ),
    (
        "xiaomi-token-plan-ams.json",
        "f7b31fac841dc1f4222902c9c40edf308cb032ed6b476ae3983f3b16701eff50",
    ),
    (
        "xiaomi-token-plan-cn.json",
        "34cc7abf8100852f012e6542b3f8cc1f2a2b15a17bff1b830b95f1730c3443da",
    ),
    (
        "xiaomi-token-plan-sgp.json",
        "9a6c45882bda13bcb1b291b7db96cde052abf69e0ac284a00a5a26fe9bdfc8e2",
    ),
    (
        "xiaomi.json",
        "ea70717591e28177169d13787f41748e04873f34b3765bea6df5584fb772b090",
    ),
    (
        "zai-coding-cn.json",
        "61f0ef0de1e18cdb41e8b9399daf486c6d6d6645b18ec8b18a8cb85ab9424530",
    ),
    (
        "zai.json",
        "8899e6e2deca75827b2c4a1443b1bcbc3efac128d833d8270749381f2d2082ae",
    ),
];

/// Strip JSON whitespace outside strings. `include_str!` embeds catalog bytes
/// verbatim, so this is exactly the text the release binary carries.
fn compact_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if !matches!(ch, ' ' | '\t' | '\n' | '\r') {
            out.push(ch);
        }
    }
    out
}

/// The embedded snapshot must be stored minified and its data must be
/// byte-identical (whitespace aside) to the pre-minification snapshot.
#[test]
fn embedded_catalog_is_minified_and_matches_pre_minification_golden() {
    use sha2::{Digest, Sha256};

    let golden: BTreeMap<&str, &str> = CATALOG_COMPACT_SHA256.iter().copied().collect();
    assert_eq!(
        golden.len(),
        CATALOG_COMPACT_SHA256.len(),
        "duplicate file in CATALOG_COMPACT_SHA256"
    );
    assert_eq!(
        golden.len(),
        CATALOG_FILES.len(),
        "golden must cover every embedded catalog file"
    );

    for (provider_id, file) in CATALOG_FILES {
        let text = catalog::embedded_catalog(file)
            .unwrap_or_else(|error| panic!("{file}: embedded catalog: {error}"));
        let compact = compact_json(text);
        let expected = golden
            .get(file)
            .unwrap_or_else(|| panic!("{file}: missing from CATALOG_COMPACT_SHA256"));
        let digest: String = Sha256::digest(compact.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            digest, *expected,
            "{provider_id} ({file}): catalog data differs from the pre-minification golden"
        );
        let body = text
            .strip_suffix('\n')
            .unwrap_or_else(|| panic!("{file}: missing the single trailing newline"));
        assert_eq!(
            compact, body,
            "{provider_id} ({file}): embedded catalog is not minified"
        );
    }
}

/// The generated snapshot carries `inputLimits`/`promptCache` inside the
/// compat block; the production loader mirrors them onto the runtime model's
/// upstream top-level fields (and strips them from compat) so both the
/// image-resize consumer and the wire shape see them.
#[test]
fn catalog_metadata_moves_to_top_level_model_fields() {
    let provider = catalog::load_provider("anthropic")
        .expect("anthropic catalog")
        .expect("embedded provider");
    let model = provider
        .models
        .iter()
        .find(|model| model.id == "claude-fable-5")
        .expect("claude-fable-5 in the catalog");

    let input_limits = model.input_limits.as_ref().expect("inputLimits mirrored");
    assert!(
        input_limits.get("images").is_some(),
        "image resize limits survive: {input_limits}"
    );
    let prompt_cache = model.prompt_cache.as_ref().expect("promptCache mirrored");
    assert!(prompt_cache.is_object(), "{prompt_cache}");

    let compat = model.compat.as_ref().expect("compat retained");
    assert!(
        compat.input_limits.is_none() && compat.prompt_cache.is_none(),
        "the loader moves the metadata out of compat: {compat:?}"
    );
}
