//! DRIFT W1 D12 (upstream `ee409a85f` / PR #10672): with an OpenRouter key,
//! only the chat models that key may use are listed, with the key's context
//! size, max output and prices from `GET /models/user`; the combined list is
//! persisted and used offline. Negative prices (router models such as
//! `openrouter/auto`) become 0.
//!
//! The HTTP pieces run against a local one-shot server; the overlay test uses
//! an injected personalizer so the offline-cache path is covered without the
//! network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::ai::provider::ProviderRegistry;
use pir::ai::providers::catalog::openrouter::{
    configured_base_url, model_wire_value, openrouter_cost, per_million,
    personalize_openrouter_catalog, OPENROUTER_BASE_URL,
};
use pir::ai::providers::{
    read_models_store_entry, PersonalizeCatalog, RemoteCatalogOverlay, RemoteCatalogOverlayConfig,
};
use pir::ai::types::{Model, ModelCost, ModelCostRates};

fn chat_model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-chat".to_string(),
        provider: "openrouter".to_string(),
        base_url: OPENROUTER_BASE_URL.to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 200_000,
        max_tokens: 50_000,
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

/// Serializes the tests that mutate process-global env (`PIR_AGENT_DIR`,
/// `PI_CATALOG_BASE_URL`, provider keys).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-w1-d12-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A local HTTP server answering every request with `status` and `body`,
/// recording each request. Returns the base URL.
fn serve(status: &str, body: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let status = status.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            recorded.lock().unwrap().push(request);
            // Newer than the embedded catalog's pinned generatedAt
            // (2026-10-25T00:00:00Z), so the upstream `isNewer` gate lets the
            // served catalog participate in the merge.
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nlast-modified: Sun, 25 Oct 2026 12:00:00 GMT\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}"), requests)
}

/// Upstream `openRouterCost` rules: $/token to $/1M, missing/invalid/negative
/// base prices are 0, `min_prompt_tokens` overrides become tiers, a rate a
/// tier does not list keeps the base price, time-of-day overrides are skipped.
#[test]
fn prices_convert_with_negative_placeholders_zeroed() {
    // `openrouter/auto` reports -1; it means "depends on the picked model".
    let router = openrouter_cost(Some(&json!({ "prompt": "-1", "completion": "-1" })));
    assert_eq!(
        router.rates,
        ModelCostRates {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0
        }
    );
    assert_eq!(
        per_million(Some(&json!("not-a-number")), 3.0),
        3.0,
        "invalid prices keep the fallback"
    );
    assert_eq!(per_million(None, 0.0), 0.0);

    let cost = openrouter_cost(Some(&json!({
        "prompt": "0.000003",
        "completion": "0.000015",
        "input_cache_read": "0.0000003",
        "overrides": [
            { "min_prompt_tokens": 100000, "prompt": "0.000006" },
            { "min_prompt_tokens": 5, "utc_start": 0, "prompt": "0.5" },
            { "utc_start": 1, "prompt": "0.9" },
            { "min_prompt_tokens": 200000, "completion": "-1" }
        ]
    })));
    assert_eq!(
        cost.rates,
        ModelCostRates {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 0.0
        }
    );
    assert_eq!(cost.tiers.len(), 2, "time-of-day overrides are skipped");
    let tier = &cost.tiers[0];
    assert_eq!(tier.input_tokens_above, 100_000);
    assert_eq!(tier.rates.input, 6.0);
    assert_eq!(tier.rates.output, 15.0, "unlisted tier rates keep the base");
    assert_eq!(tier.rates.cache_read, 0.3);
    // Negative tier rates fall back to the base rate, not to zero.
    let negative_tier = &cost.tiers[1];
    assert_eq!(negative_tier.input_tokens_above, 200_000);
    assert_eq!(negative_tier.rates.output, 15.0);
}

/// Keep only the key's chat models, take its limits and prices, and address
/// `GET {base}/models/user` with the bearer key.
#[tokio::test]
async fn models_are_filtered_to_the_keys_list() {
    let (base_url, requests) = serve(
        "200 OK",
        &json!({
            "data": [
                {
                    "id": "listed",
                    "context_length": 1_000_000,
                    "top_provider": { "context_length": 262_144, "max_completion_tokens": 32_768 },
                    "pricing": { "prompt": "0.000003", "completion": "0.000015" }
                },
                // A missing limit keeps the catalog value.
                { "id": "no-limits", "context_length": 0, "top_provider": { "context_length": null, "max_completion_tokens": null } },
                // Router models report -1.
                { "id": "router", "pricing": { "prompt": "-1", "completion": "-1" } }
            ]
        })
        .to_string(),
    );
    let catalog = vec![
        chat_model("listed"),
        chat_model("no-limits"),
        chat_model("router"),
        chat_model("blocked"),
    ];
    let personalized = personalize_openrouter_catalog(
        catalog,
        &base_url,
        Some("sk-or-test"),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("personalize");

    let ids: Vec<&str> = personalized.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec!["listed", "no-limits", "router"]);
    assert_eq!(personalized[0].context_window, 262_144);
    assert_eq!(personalized[0].max_tokens, 32_768);
    assert_eq!(personalized[0].cost.rates.input, 3.0);
    assert_eq!(personalized[0].cost.rates.output, 15.0);
    assert_eq!(personalized[0].cost.rates.cache_read, 0.0);
    assert_eq!(personalized[1].context_window, 200_000);
    assert_eq!(personalized[1].max_tokens, 50_000);
    assert_eq!(personalized[1].cost.rates.input, 1.0);
    assert_eq!(personalized[2].cost.rates.input, 0.0, "negative price → 0");

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0].starts_with("GET /models/user "),
        "the user model list is requested: {}",
        recorded[0]
    );
    assert!(
        recorded[0]
            .to_lowercase()
            .contains("authorization: bearer sk-or-test"),
        "the key is sent as a bearer token: {}",
        recorded[0]
    );
}

/// A key the endpoint rejects (401/403) or an endpoint without `/models/user`
/// (404) keeps the catalog; other failures surface; a missing key never
/// requests.
#[tokio::test]
async fn rejected_or_missing_endpoints_keep_the_catalog() {
    let catalog = || vec![chat_model("a"), chat_model("b")];
    for (status, body) in [
        ("401 Unauthorized", "{}"),
        ("403 Forbidden", "{}"),
        ("404 Not Found", "{}"),
    ] {
        let (base_url, requests) = serve(status, body);
        let result = personalize_openrouter_catalog(
            catalog(),
            &base_url,
            Some("sk-or-test"),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("unchanged");
        let ids: Vec<&str> = result.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "status {status}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    let (base_url, _) = serve("500 Internal Server Error", "{}");
    assert!(personalize_openrouter_catalog(
        catalog(),
        &base_url,
        Some("sk-or-test"),
        std::time::Duration::from_secs(5),
    )
    .await
    .is_err());

    let (base_url, _) = serve("200 OK", "\"not a model list\"");
    assert!(personalize_openrouter_catalog(
        catalog(),
        &base_url,
        Some("sk-or-test"),
        std::time::Duration::from_secs(5),
    )
    .await
    .is_err());

    // No key: the catalog passes through without a request.
    let (base_url, requests) = serve("200 OK", &json!({ "data": [] }).to_string());
    let result = personalize_openrouter_catalog(
        catalog(),
        &base_url,
        None,
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("unchanged");
    assert_eq!(result.len(), 2);
    assert!(requests.lock().unwrap().is_empty());
}

/// The persisted wire shape is camelCase (the models-store contract), so a
/// personalized overlay restores its list offline.
#[tokio::test]
async fn a_personalized_catalog_survives_offline() {
    let root = unique_dir("offline");
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let personalized = {
        let mut model = chat_model("picked");
        model.name = "Picked (personalized)".to_string();
        model.context_window = 262_144;
        model.max_tokens = 32_768;
        model
    };
    let personalizer: Arc<PersonalizeCatalog> = Arc::new(move |_catalog: Vec<Model>| {
        let models = vec![personalized.clone()];
        Box::pin(async move {
            let values = vec![model_wire_value(&models[0])];
            Ok((models, values))
        })
    });

    let registry = Arc::new(ProviderRegistry::default());
    // A local catalog server: the personalized list replaces whatever it
    // serves, and the result lands in the models-store.
    let (base_url, _) = serve("200 OK", &json!({ "models": [] }).to_string());
    let overlay = RemoteCatalogOverlay::new_personalized(
        RemoteCatalogOverlayConfig {
            provider_id: "openrouter".to_string(),
            catalog_url: format!("{base_url}/api/models/providers/openrouter"),
            agent_dir: agent_dir.clone(),
            interval: None,
        },
        Arc::clone(&registry),
        Some(Arc::clone(&personalizer)),
    )
    .expect("overlay");
    overlay.refresh_once().await.expect("refresh");
    let served: Vec<String> = overlay
        .remote_models()
        .iter()
        .map(|model| model.id.clone())
        .collect();
    // The personalized list replaces the built-in catalog (upstream
    // `withPersonalizedCatalog`'s `getAllModels`).
    assert_eq!(served, vec!["picked".to_string()]);

    let stored = read_models_store_entry(&agent_dir, "openrouter").expect("persisted entry");
    assert!(
        stored.models[0].get("contextWindow").is_some(),
        "wire shape is camelCase: {}",
        stored.models[0]
    );

    // A fresh overlay against a dead endpoint restores the persisted list
    // (the offline cache) without any personalizer.
    let offline = RemoteCatalogOverlay::new(
        RemoteCatalogOverlayConfig {
            provider_id: "openrouter".to_string(),
            catalog_url: "http://127.0.0.1:1/api/models/providers/openrouter".to_string(),
            agent_dir: agent_dir.clone(),
            interval: None,
        },
        Arc::clone(&registry),
    )
    .expect("offline overlay")
    // The saved entry is a personalized list (no `lastModified`); upstream
    // `withPersonalizedCatalog` restores it without the generation gate.
    .with_local_generated_at(None);
    offline.refresh_once().await.expect("offline refresh");
    let restored = offline.remote_models();
    let restored_ids: Vec<String> = restored.iter().map(|model| model.id.clone()).collect();
    assert_eq!(
        restored_ids,
        vec!["picked".to_string()],
        "the personalized list restores offline"
    );
    // The key's limits and prices survive the offline round trip.
    assert_eq!(restored[0].context_window, 262_144);
    assert_eq!(restored[0].max_tokens, 32_768);
    assert_eq!(restored[0].cost.rates.input, 1.0);
    assert_eq!(restored[0].cost.rates.output, 2.0);
}

/// Regional endpoints: a `baseUrl` configured in `models.json` is used for
/// the user-model request (upstream `RefreshModelsContext.baseUrl`).
#[test]
fn a_configured_base_url_wins() {
    let _env = env_lock();
    let root = unique_dir("base-url");
    // Holds the env lock: other tests in this binary also override the
    // agent dir and provider keys.
    let previous = std::env::var_os("PIR_AGENT_DIR");
    std::env::set_var("PIR_AGENT_DIR", &root);
    std::fs::write(
        root.join("models.json"),
        r#"{ "providers": { "openrouter": { "baseUrl": "https://us.openrouter.ai/api/v1" } } }"#,
    )
    .expect("models.json");
    let resolved = configured_base_url();
    match previous {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
    assert_eq!(resolved, "https://us.openrouter.ai/api/v1");
}

/// Live-network evidence (`cargo test --test w1_d12_openrouter -- --ignored`):
/// the real endpoint answers a bogus key with 401 — the documented
/// "rejected key keeps the catalog" path — and the real `/models` payload
/// carries the exact fields the personalizer reads.
#[tokio::test]
#[ignore = "hits the live OpenRouter API"]
async fn live_openrouter_endpoint_shapes() {
    let catalog = vec![chat_model("anthropic/claude-opus-5")];
    let rejected = personalize_openrouter_catalog(
        catalog.clone(),
        OPENROUTER_BASE_URL,
        Some("sk-or-not-a-real-key"),
        std::time::Duration::from_secs(20),
    )
    .await
    .expect("a rejected key keeps the catalog");
    let ids: Vec<&str> = rejected.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, vec!["anthropic/claude-opus-5"]);

    let live: Value = reqwest::Client::new()
        .get(format!("{OPENROUTER_BASE_URL}/models"))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .expect("live model list")
        .json()
        .await
        .expect("json");
    let first = live["data"]
        .as_array()
        .and_then(|entries| entries.first())
        .expect("entries");
    assert!(first.get("context_length").is_some());
    assert!(first
        .get("top_provider")
        .and_then(|top| top.get("max_completion_tokens"))
        .is_some());
    assert!(
        first
            .get("pricing")
            .and_then(|pricing| pricing.get("prompt"))
            .is_some(),
        "prices are the strings the personalizer converts: {first:?}"
    );
}

/// Saves the process-global vars a strict-refresh test mutates and restores
/// them on drop, so an assertion failure cannot leak into the next test.
struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn capture(vars: &[&'static str]) -> Self {
        Self {
            saved: vars
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// `pir update --models` must apply the same OpenRouter key-based
/// personalization as the session-runtime install path: the strict refresh
/// builds every overlay through the shared provider personalizer, so the
/// catalog is filtered to the key's models (with the key's limits) and the
/// personalized list is persisted.
#[test]
fn strict_update_applies_the_openrouter_personalizer() {
    let _env = env_lock();
    let _guard = EnvGuard::capture(&["PIR_AGENT_DIR", "PI_CATALOG_BASE_URL", "OPENROUTER_API_KEY"]);

    let root = unique_dir("strict-personalized");
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    // The OpenRouter API base (a `models.json` regional-endpoint override)
    // answers `GET /models/user` with the key's single allowed model.
    let (user_base, user_requests) = serve(
        "200 OK",
        &json!({
            "data": [
                {
                    "id": "listed",
                    "context_length": 999_999,
                    "top_provider": { "context_length": 262_144, "max_completion_tokens": 32_768 },
                    "pricing": { "prompt": "0.000003", "completion": "0.000015" }
                }
            ]
        })
        .to_string(),
    );
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": { "openrouter": { "baseUrl": user_base } } }).to_string(),
    )
    .expect("models.json");

    // The pi.dev catalog server serves two models; the personalizer must keep
    // only the key's one.
    let (catalog_base, catalog_requests) = serve(
        "200 OK",
        &json!({
            "models": [
                {
                    "id": "listed",
                    "type": "chat",
                    "api": "openai-chat",
                    "name": "Listed",
                    "contextWindow": 1_000,
                    "maxTokens": 100,
                    "cost": { "input": 1.0, "output": 2.0 }
                },
                {
                    "id": "blocked",
                    "type": "chat",
                    "api": "openai-chat",
                    "name": "Blocked",
                    "contextWindow": 1_000,
                    "maxTokens": 100,
                    "cost": { "input": 1.0, "output": 2.0 }
                }
            ]
        })
        .to_string(),
    );

    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::set_var("PI_CATALOG_BASE_URL", &catalog_base);
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-test");

    let result = pir::coding_agent::package_manager::update_model_catalogs(&agent_dir);
    assert!(
        result.is_ok(),
        "the strict refresh with a mock catalog must succeed: {result:?}"
    );

    let catalog_lines: Vec<String> = catalog_requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| request.lines().next().map(str::to_string))
        .collect();
    assert!(
        catalog_lines
            .iter()
            .any(|line| line.starts_with("GET /api/models/providers/openrouter")),
        // The request target carries the upstream `?types=chat,image,classifier`
        // query, so the assertion anchors on the path alone.
        "the strict path requests the openrouter catalog: {catalog_lines:?}"
    );

    let user_lines = user_requests.lock().unwrap().clone();
    assert!(
        user_lines
            .iter()
            .any(|request| request.starts_with("GET /models/user ")),
        "the strict path runs the personalizer's user-model request: {user_lines:?}"
    );
    assert!(
        user_lines.iter().any(|request| request
            .to_lowercase()
            .contains("authorization: bearer sk-or-test")),
        "the personalizer sends the key as a bearer token: {user_lines:?}"
    );

    let stored =
        read_models_store_entry(&agent_dir, "openrouter").expect("persisted openrouter entry");
    let ids: Vec<&str> = stored
        .models
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(
        ids,
        vec!["listed"],
        "the personalized list replaces the raw catalog"
    );
    assert_eq!(
        stored.models[0]
            .get("contextWindow")
            .and_then(Value::as_u64),
        Some(262_144),
        "the key's context size lands in the persisted list"
    );
    assert_eq!(
        stored.models[0].get("maxTokens").and_then(Value::as_u64),
        Some(32_768),
        "the key's max output lands in the persisted list"
    );
}

/// A provider without a personalizer is stored unchanged by the strict
/// refresh: the raw catalog values land in the models-store verbatim and no
/// user-model request is made.
#[test]
fn strict_update_leaves_non_personalized_providers_unchanged() {
    let _env = env_lock();
    let _guard = EnvGuard::capture(&[
        "PIR_AGENT_DIR",
        "PI_CATALOG_BASE_URL",
        "OPENROUTER_API_KEY",
        "ANTHROPIC_API_KEY",
    ]);

    let root = unique_dir("strict-plain");
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    // A user-model endpoint that must stay untouched: anthropic has no
    // personalizer and openrouter has no credential in this test.
    let (user_base, user_requests) = serve("200 OK", &json!({ "data": [] }).to_string());
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": { "openrouter": { "baseUrl": user_base } } }).to_string(),
    )
    .expect("models.json");

    let (catalog_base, _) = serve(
        "200 OK",
        &json!({
            "models": [
                {
                    "id": "unchanged-probe",
                    "type": "chat",
                    "api": "anthropic-messages",
                    "name": "Probe",
                    "baseUrl": "http://127.0.0.1:1",
                    "contextWindow": 12_345,
                    "maxTokens": 678,
                    "cost": { "input": 1.5, "output": 2.5 }
                }
            ]
        })
        .to_string(),
    );

    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::set_var("PI_CATALOG_BASE_URL", &catalog_base);
    std::env::remove_var("OPENROUTER_API_KEY");
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");

    let result = pir::coding_agent::package_manager::update_model_catalogs(&agent_dir);
    assert!(
        result.is_ok(),
        "the strict refresh with a mock catalog must succeed: {result:?}"
    );

    let stored =
        read_models_store_entry(&agent_dir, "anthropic").expect("persisted anthropic entry");
    let probe = stored
        .models
        .iter()
        .find(|model| model.get("id").and_then(Value::as_str) == Some("unchanged-probe"))
        .expect("the served anthropic model is stored");
    assert_eq!(
        probe.get("contextWindow").and_then(Value::as_u64),
        Some(12_345),
        "no personalizer rewrites the context window"
    );
    assert_eq!(
        probe.get("maxTokens").and_then(Value::as_u64),
        Some(678),
        "no personalizer rewrites the max output"
    );
    assert!(
        user_requests.lock().unwrap().is_empty(),
        "a non-personalized provider makes no user-model request"
    );
}
