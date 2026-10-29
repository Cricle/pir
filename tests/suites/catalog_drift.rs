//! Catalog drift fixes (Q6): the drift-repaired catalog entries validate
//! against the upstream generator constants, and the periodic remote catalog
//! overlay (upstream `withRemoteCatalog`) revalidates with `If-None-Match`
//! against a local mock catalog server: 304 keeps the local overlay, 200
//! swaps it in and persists the ETag, and network failures degrade silently.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::ai::auth::{InMemoryCredentialStore, MapAuthContext};
use pir::ai::provider::ProviderRegistry;
use pir::ai::providers::catalog::load_provider;
use pir::ai::providers::models_store::{read_models_store_entry, write_models_store_entry};
use pir::ai::providers::{
    remote_catalog_is_newer, ModelsStoreEntry, PersonalizeCatalog, RemoteCatalogOverlay,
    RemoteCatalogOverlayConfig, REMOTE_CATALOG_REFRESH_INTERVAL, REMOTE_CATALOG_REQUEST_TIMEOUT,
};
use pir::ai::types::Model;

// ---------------------------------------------------------------------------
// Drift-repaired catalog entries (upstream generate-models constants)
// ---------------------------------------------------------------------------

fn catalog_model(provider: &pir::ai::providers::catalog::BuiltinProvider, id: &str) -> Model {
    provider
        .models
        .iter()
        .find(|model| model.id == id)
        .cloned()
        .unwrap_or_else(|| panic!("{}/{id} missing from the catalog", provider.id))
}

#[test]
fn anthropic_haiku_5_5_matches_upstream_generator() {
    let anthropic = load_provider("anthropic").unwrap().unwrap();
    let haiku = catalog_model(&anthropic, "claude-haiku-5-5");
    assert_eq!(haiku.api, "anthropic-messages");
    assert_eq!(haiku.base_url, "https://api.anthropic.com");
    assert!(haiku.reasoning);
    assert_eq!(haiku.input, vec!["text".to_string(), "image".to_string()]);
    assert_eq!(haiku.context_window, 1_000_000);
    assert_eq!(haiku.max_tokens, 128_000);
    // Base rates plus the >100k prompt-length tier (5x input / 5x output).
    assert_eq!(haiku.cost.rates.input, 0.1);
    assert_eq!(haiku.cost.rates.output, 0.5);
    assert_eq!(haiku.cost.rates.cache_read, 0.01);
    assert_eq!(haiku.cost.rates.cache_write, 0.125);
    assert_eq!(haiku.cost.tiers.len(), 1);
    assert_eq!(haiku.cost.tiers[0].input_tokens_above, 100_000);
    assert_eq!(haiku.cost.tiers[0].rates.input, 0.5);
    assert_eq!(haiku.cost.tiers[0].rates.output, 2.5);
    assert_eq!(haiku.cost.tiers[0].rates.cache_read, 0.05);
    assert_eq!(haiku.cost.tiers[0].rates.cache_write, 0.625);
    // Adaptive thinking with native xhigh/max (commit f76c1db66). Haiku 5.5
    // accepts disabled thinking, so upstream deliberately omits the `off` key
    // (missing = provider default) instead of marking it unsupported (`null`):
    // generate-models.ts spreads `{}` for claude-haiku-5-5 when merging the
    // Claude 5.5 thinking map.
    let map = haiku.thinking_level_map.as_ref().unwrap();
    assert_eq!(map.get("off"), None);
    assert_eq!(
        map.get("xhigh").cloned().flatten().as_deref(),
        Some("xhigh")
    );
    assert_eq!(map.get("max").cloned().flatten().as_deref(), Some("max"));
}

#[test]
fn openai_gpt_5_chat_latest_matches_upstream_generator() {
    let openai = load_provider("openai").unwrap().unwrap();
    let chat = catalog_model(&openai, "gpt-5-chat-latest");
    assert_eq!(chat.api, "openai-responses");
    assert!(!chat.reasoning);
    assert_eq!(chat.context_window, 128_000);
    assert_eq!(chat.max_tokens, 16_384);
    assert_eq!(chat.cost.rates.input, 1.25);
    assert_eq!(chat.cost.rates.output, 10.0);
    assert_eq!(chat.cost.rates.cache_read, 0.125);
    assert_eq!(chat.cost.rates.cache_write, 0.0);
}

#[test]
fn deepseek_flash_matches_upstream_generator() {
    let deepseek = load_provider("deepseek").unwrap().unwrap();
    let flash = catalog_model(&deepseek, "deepseek-flash");
    assert_eq!(flash.name, "DeepSeek V4.1 Flash");
    assert!(flash.reasoning);
    assert_eq!(flash.input, vec!["text".to_string(), "image".to_string()]);
    assert_eq!(flash.context_window, 1_000_000);
    assert_eq!(flash.max_tokens, 384_000);
    assert_eq!(flash.cost.rates.input, 0.3);
    assert_eq!(flash.cost.rates.output, 1.2);
    assert_eq!(flash.cost.rates.cache_read, 0.006);
    // DEEPSEEK_V4_FLASH_THINKING_LEVEL_MAP: low enabled on top of the V4 map.
    let map = flash.thinking_level_map.as_ref().unwrap();
    assert_eq!(map.get("low").cloned().flatten().as_deref(), Some("low"));
    assert_eq!(map.get("medium"), Some(&None));
    assert_eq!(map.get("high").cloned().flatten().as_deref(), Some("high"));
    assert_eq!(map.get("max").cloned().flatten().as_deref(), Some("max"));
    let compat = flash.compat.as_ref().unwrap();
    assert!(!compat.supports_reasoning_effort);
    assert_eq!(
        compat.thinking_format,
        pir::ai::types::ThinkingFormat::Deepseek
    );
}

#[test]
fn ant_ling_ling_models_match_upstream_generator() {
    let ant_ling = load_provider("ant-ling").unwrap().unwrap();
    for (id, name, input_cost, output_cost) in [
        ("Ling-2.6-flash", "Ling 2.6 Flash", 0.01, 0.02),
        ("Ling-2.6-1T", "Ling 2.6 1T", 0.06, 0.25),
    ] {
        let model = catalog_model(&ant_ling, id);
        assert_eq!(model.name, name);
        assert!(!model.reasoning);
        assert_eq!(model.context_window, 262_144);
        assert_eq!(model.max_tokens, 65_536);
        assert_eq!(model.cost.rates.input, input_cost);
        assert_eq!(model.cost.rates.output, output_cost);
    }
}

#[test]
fn github_copilot_missing_models_match_upstream_generator() {
    let copilot = load_provider("github-copilot").unwrap().unwrap();
    let opus = catalog_model(&copilot, "claude-opus-5.5");
    assert!(opus.reasoning);
    assert_eq!(opus.context_window, 1_000_000);
    assert_eq!(opus.max_tokens, 128_000);
    assert_eq!(opus.cost.rates.input, 4.0);
    assert_eq!(opus.cost.rates.output, 20.0);
    assert_eq!(opus.cost.rates.cache_read, 0.2);
    assert_eq!(opus.cost.rates.cache_write, 5.0);
    let map = opus.thinking_level_map.as_ref().unwrap();
    assert_eq!(
        map.get("xhigh").cloned().flatten().as_deref(),
        Some("xhigh")
    );
    assert_eq!(map.get("max").cloned().flatten().as_deref(), Some("max"));

    let luna = catalog_model(&copilot, "gpt-6-luna");
    assert_eq!(luna.api, "openai-responses");
    assert_eq!(luna.cost.rates.input, 0.1);
    assert_eq!(luna.cost.rates.output, 0.5);
    assert_eq!(luna.context_window, 1_000_000);
}

#[test]
fn openai_codex_models_match_upstream_generator() {
    let codex = load_provider("openai-codex").unwrap().unwrap();
    for (id, input, output, context_window) in [
        ("gpt-5.6-terra", 2.0, 12.0, 272_000),
        ("gpt-5.6-luna", 0.2, 1.2, 272_000),
        ("gpt-6-luna", 0.1, 0.5, 272_000),
        ("gpt-6-astra", 10.0, 50.0, 272_000),
    ] {
        let model = catalog_model(&codex, id);
        assert_eq!(model.api, "openai-codex-responses");
        assert_eq!(model.context_window, context_window);
        assert_eq!(model.max_tokens, 128_000);
        assert_eq!(model.cost.rates.input, input);
        assert_eq!(model.cost.rates.output, output);
    }
    // Upstream 1.4.0 (pi#11173) removed both models from this provider: the
    // Codex backend rejects them for ChatGPT accounts.
    for removed in ["gpt-5.5", "gpt-5.3-codex-spark"] {
        assert!(
            codex.models.iter().all(|model| model.id != removed),
            "openai-codex must not list `{removed}`"
        );
    }
}

#[test]
fn mistral_medium_3_5_matches_upstream_generator() {
    let mistral = load_provider("mistral").unwrap().unwrap();
    let model = catalog_model(&mistral, "mistral-medium-3.5");
    assert_eq!(model.api, "mistral-conversations");
    assert!(model.reasoning);
    assert_eq!(model.context_window, 262_144);
    assert_eq!(model.max_tokens, 262_144);
    assert_eq!(model.cost.rates.input, 1.5);
    assert_eq!(model.cost.rates.output, 7.5);
    // Effort values ["none", "high"].
    let map = model.thinking_level_map.as_ref().unwrap();
    assert_eq!(map.get("off").cloned().flatten().as_deref(), Some("none"));
    assert_eq!(map.get("low"), Some(&None));
    assert_eq!(map.get("high").cloned().flatten().as_deref(), Some("high"));
}

#[test]
fn openrouter_router_aliases_match_upstream_generator() {
    let openrouter = load_provider("openrouter").unwrap().unwrap();
    let auto = catalog_model(&openrouter, "auto");
    assert!(auto.reasoning);
    assert_eq!(auto.context_window, 2_000_000);
    assert_eq!(auto.max_tokens, 30_000);
    assert_eq!(auto.cost.rates.input, 0.0);
    let fusion = catalog_model(&openrouter, "openrouter/fusion");
    assert_eq!(fusion.input, vec!["text".to_string()]);
    assert_eq!(fusion.context_window, 1_000_000);
    assert_eq!(fusion.max_tokens, 30_000);
}

// ---------------------------------------------------------------------------
// Mock catalog server (bounded local HTTP endpoint, mirrors tests/common)
// ---------------------------------------------------------------------------

/// A local catalog server. Answers 304 when the request's `If-None-Match`
/// matches the current ETag, otherwise the configured status with the current
/// body, ETag and `Last-Modified`. Every request's validator is recorded for
/// assertions.
struct MockCatalogServer {
    url: String,
    requests: Arc<Mutex<Vec<Option<String>>>>,
    etag: Arc<Mutex<Option<String>>>,
    body: Arc<Mutex<String>>,
    status: Arc<Mutex<u16>>,
    last_modified: Arc<Mutex<Option<String>>>,
}

/// The embedded catalog's pinned manifest timestamp (Unix ms) — the local
/// `generatedAt` the overlay gate compares `Last-Modified` against.
const LOCAL_GENERATED_AT_MS: u64 = pir::ai::providers::catalog::CATALOG_GENERATED_AT_MS;

/// 12h after the pinned `generatedAt`: the remote catalog counts as newer.
const LAST_MODIFIED_NEWER: &str = "Sun, 25 Oct 2026 12:00:00 GMT";
const LAST_MODIFIED_NEWER_MS: u64 = LOCAL_GENERATED_AT_MS + 12 * 60 * 60 * 1000;
/// Exactly the pinned `generatedAt`: not newer (the gate is strict).
const LAST_MODIFIED_EQUAL: &str = "Sun, 25 Oct 2026 00:00:00 GMT";
/// 12h before the pinned `generatedAt`.
const LAST_MODIFIED_OLDER: &str = "Sat, 24 Oct 2026 12:00:00 GMT";
const LAST_MODIFIED_OLDER_MS: u64 = LOCAL_GENERATED_AT_MS - 12 * 60 * 60 * 1000;

impl MockCatalogServer {
    fn start(body: &str, etag: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock catalog listener");
        let port = listener.local_addr().expect("local addr").port();
        let requests: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let shared_etag: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some(etag.to_string())));
        let shared_body: Arc<Mutex<String>> = Arc::new(Mutex::new(body.to_string()));
        let shared_status: Arc<Mutex<u16>> = Arc::new(Mutex::new(200));
        // Default: newer than the embedded catalog, so the pre-gate tests keep
        // exercising the "remote wins" path.
        let shared_last_modified: Arc<Mutex<Option<String>>> =
            Arc::new(Mutex::new(Some(LAST_MODIFIED_NEWER.to_string())));
        let thread_requests = Arc::clone(&requests);
        let thread_etag = Arc::clone(&shared_etag);
        let thread_body = Arc::clone(&shared_body);
        let thread_status = Arc::clone(&shared_status);
        let thread_last_modified = Arc::clone(&shared_last_modified);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request_head(&mut stream) else {
                    continue;
                };
                let if_none_match = extract_header(&request, "if-none-match");
                thread_requests
                    .lock()
                    .expect("mock catalog requests lock")
                    .push(if_none_match.clone());
                let current_etag = thread_etag.lock().expect("mock catalog etag lock").clone();
                let current_body = thread_body.lock().expect("mock catalog body lock").clone();
                let status = *thread_status.lock().expect("mock catalog status lock");
                let current_last_modified = thread_last_modified
                    .lock()
                    .expect("mock catalog last-modified lock")
                    .clone();
                let last_modified_header = current_last_modified
                    .as_deref()
                    .map(|value| format!("Last-Modified: {value}\r\n"))
                    .unwrap_or_default();
                let not_modified =
                    status == 200 && if_none_match.is_some() && if_none_match == current_etag;
                let response = if not_modified {
                    format!(
                        "HTTP/1.1 304 Not Modified\r\nETag: {}\r\n{last_modified_header}Content-Length: 0\r\nConnection: close\r\n\r\n",
                        current_etag.expect("etag for 304")
                    )
                } else {
                    format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nETag: {}\r\n{last_modified_header}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        current_etag.unwrap_or_default(),
                        current_body.len(),
                        current_body
                    )
                };
                use std::io::Write;
                let _ = stream.write_all(response.as_bytes());
                // The connection closes when `stream` drops.
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}/api/models/providers/test"),
            requests,
            etag: shared_etag,
            body: shared_body,
            status: shared_status,
            last_modified: shared_last_modified,
        }
    }

    /// Change the `Last-Modified` header (`None` omits it, like an older
    /// catalog server or an entry written before the timestamp existed).
    fn set_last_modified(&self, value: Option<&str>) {
        *self
            .last_modified
            .lock()
            .expect("mock catalog last-modified lock") = value.map(str::to_string);
    }

    fn request_validators(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .expect("mock catalog requests lock")
            .clone()
    }
}

fn read_request_head(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&buffer[..position]).to_string());
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 || buffer.len() > 16_384 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn extract_header(head: &str, name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(&lower)
            .then(|| value.trim().to_string())
    })
}

// ---------------------------------------------------------------------------
// Remote catalog overlay lifecycle
// ---------------------------------------------------------------------------

fn overlay_agent_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-overlay-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn test_registry() -> Arc<ProviderRegistry> {
    Arc::new(ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::default()),
        Arc::new(MapAuthContext::new(BTreeMap::new())),
    ))
}

fn overlay_config(
    provider_id: &str,
    url: String,
    agent_dir: PathBuf,
) -> RemoteCatalogOverlayConfig {
    RemoteCatalogOverlayConfig {
        provider_id: provider_id.to_string(),
        catalog_url: url,
        agent_dir,
        interval: None,
    }
}

const BODY_V1: &str = r#"[{"id":"remote-test-model","name":"Remote Test Model","api":"openai-completions","baseUrl":"https://remote.example/v1","reasoning":true,"input":["text"],"cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0.2},"contextWindow":100000,"maxTokens":8000}]"#;
const BODY_V2: &str = r#"[{"id":"remote-test-model-2","name":"Remote Test Model 2","api":"openai-completions","baseUrl":"https://remote.example/v1","reasoning":false,"input":["text"],"cost":{"input":3,"output":4},"contextWindow":200000,"maxTokens":9000}]"#;

#[test]
fn overlay_200_updates_overlay_and_persists_etag_then_304_keeps_it() {
    let server = MockCatalogServer::start(BODY_V1, "\"catalog-1\"");
    let agent_dir = overlay_agent_dir("update");
    let registry = test_registry();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = Arc::new(
            RemoteCatalogOverlay::new(
                overlay_config("overlay-provider", server.url.clone(), agent_dir.clone()),
                Arc::clone(&registry),
            )
            .expect("overlay"),
        );

        // First pass: 200 -> overlay registered with the remote model.
        assert!(overlay.refresh_once().await.expect("first refresh"));
        assert_eq!(overlay.etag().as_deref(), Some("\"catalog-1\""));
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model");
        assert_eq!(models[0].context_window, 100_000);
        // ETag + models + Last-Modified + checkedAt persisted for the next cycle.
        let entry = read_models_store_entry(&agent_dir, "overlay-provider").expect("store entry");
        assert_eq!(entry.etag.as_deref(), Some("\"catalog-1\""));
        assert_eq!(entry.models.len(), 1);
        assert_eq!(
            entry.last_modified,
            Some(LAST_MODIFIED_NEWER_MS),
            "the response Last-Modified is persisted (Unix ms)"
        );
        assert!(entry.checked_at.is_some());

        // Second pass with an updated document: still a 200 (the stored
        // validator is echoed back, but the server now serves catalog-2).
        *server.etag.lock().expect("etag lock") = Some("\"catalog-2\"".to_string());
        *server.body.lock().expect("body lock") = BODY_V2.to_string();
        assert!(overlay.refresh_once().await.expect("second refresh"));
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model-2");
        assert_eq!(overlay.etag().as_deref(), Some("\"catalog-2\""));
        let entry = read_models_store_entry(&agent_dir, "overlay-provider").expect("store entry");
        assert_eq!(entry.etag.as_deref(), Some("\"catalog-2\""));
        assert_eq!(entry.models.len(), 1);

        // Third pass: the server now serves catalog-2, the stored validator
        // matches -> 304 keeps the local overlay untouched.
        assert!(!overlay.refresh_once().await.expect("third refresh"));
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model-2");
        assert_eq!(overlay.etag().as_deref(), Some("\"catalog-2\""));
    });

    // The validator was absent on the first request, then echoed verbatim.
    let requests = server.request_validators();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0], None);
    assert_eq!(requests[1].as_deref(), Some("\"catalog-1\""));
    assert_eq!(requests[2].as_deref(), Some("\"catalog-2\""));
    let _ = std::fs::remove_dir_all(&agent_dir);
}

#[test]
fn overlay_200_merges_over_the_builtin_baseline() {
    let server = MockCatalogServer::start(
        r#"[{"id":"claude-opus-4-8","name":"Claude Opus 4.8 (remote)","api":"anthropic-messages","baseUrl":"https://api.anthropic.com","reasoning":true,"input":["text","image"],"cost":{"input":99,"output":99},"contextWindow":1,"maxTokens":1},{"id":"claude-haiku-5-5","name":"Claude Haiku 5.5 (remote)","api":"anthropic-messages","baseUrl":"https://api.anthropic.com","reasoning":true,"input":["text"],"cost":{"input":0.1,"output":0.5},"contextWindow":1000000,"maxTokens":128000}]"#,
        "\"merge-1\"",
    );
    let agent_dir = overlay_agent_dir("merge");
    let registry = test_registry();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = Arc::new(
            RemoteCatalogOverlay::new(
                overlay_config("anthropic", server.url.clone(), agent_dir.clone()),
                Arc::clone(&registry),
            )
            .expect("overlay"),
        );
        assert!(overlay.refresh_once().await.expect("refresh"));
        let models = registry.get_provider_models("anthropic");
        // Baseline entries stay, the remote override wins per id, the new
        // remote-only entry is appended.
        let opus = models
            .iter()
            .find(|model| model.id == "claude-opus-4-8")
            .expect("opus override");
        assert_eq!(opus.name, "Claude Opus 4.8 (remote)");
        assert_eq!(opus.cost.rates.input, 99.0);
        assert!(models
            .iter()
            .any(|model| model.id == "claude-sonnet-5" && model.cost.rates.input == 2.0));
        assert!(models.iter().any(|model| model.id == "claude-haiku-5-5"));
    });
    let _ = std::fs::remove_dir_all(&agent_dir);
}

#[test]
fn overlay_error_status_and_network_failure_degrade_silently() {
    // Error status: the cached body and its validator stay valid.
    let server = MockCatalogServer::start(BODY_V1, "\"catalog-1\"");
    *server.status.lock().expect("status lock") = 500;
    let agent_dir = overlay_agent_dir("degrade");
    let registry = test_registry();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = Arc::new(
            RemoteCatalogOverlay::new(
                overlay_config("overlay-provider", server.url.clone(), agent_dir.clone()),
                Arc::clone(&registry),
            )
            .expect("overlay"),
        );
        // Seed the overlay from a previously persisted store entry, then hit
        // the failing server: the overlay must survive both passes.
        let seed = pir::ai::providers::ModelsStoreEntry {
            models: serde_json::from_str::<Vec<serde_json::Value>>(BODY_V1).expect("seed models"),
            etag: Some("\"catalog-1\"".to_string()),
            checked_at: Some(1),
            // Newer than the embedded catalog, so the offline restore applies.
            last_modified: Some(LAST_MODIFIED_NEWER_MS),
        };
        pir::ai::providers::write_models_store_entry(&agent_dir, "overlay-provider", seed)
            .expect("seed store");

        // First pass restores the persisted overlay (offline phase, no
        // network content change is counted against the 500 response).
        overlay.refresh_once().await.expect("restore pass");
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model");

        // 500: silent degradation, overlay + validator preserved.
        assert!(!overlay.refresh_once().await.expect("degraded pass"));
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model");
        assert_eq!(overlay.etag().as_deref(), Some("\"catalog-1\""));

        // Network failure (connection refused): same silent degradation.
        let offline = Arc::new(
            RemoteCatalogOverlay::new(
                overlay_config(
                    "offline-provider",
                    "http://127.0.0.1:1/v1".to_string(),
                    agent_dir.clone(),
                ),
                Arc::clone(&registry),
            )
            .expect("offline overlay"),
        );
        assert!(!offline.refresh_once().await.expect("offline pass"));
        assert_eq!(offline.etag(), None);
        assert!(registry.get_provider_models("offline-provider").is_empty());
    });
    let _ = std::fs::remove_dir_all(&agent_dir);
}

#[test]
fn overlay_background_task_converges_and_spawns_once() {
    let server = MockCatalogServer::start(BODY_V1, "\"catalog-1\"");
    let agent_dir = overlay_agent_dir("background");
    let registry = test_registry();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let overlay = Arc::new(
            RemoteCatalogOverlay::new(
                RemoteCatalogOverlayConfig {
                    provider_id: "overlay-provider".to_string(),
                    catalog_url: server.url.clone(),
                    agent_dir: agent_dir.clone(),
                    // Tight interval for the test; production default is the
                    // conservative 6h constant.
                    interval: Some(Duration::from_millis(25)),
                },
                Arc::clone(&registry),
            )
            .expect("overlay"),
        );
        assert!(overlay.spawn_refresh_task());
        // A second spawn is refused (one background task per overlay).
        assert!(!overlay.spawn_refresh_task());

        // The interval's first tick fires immediately; wait for convergence.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let models = registry.get_provider_models("overlay-provider");
            if models.iter().any(|model| model.id == "remote-test-model") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "background overlay refresh did not converge"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(overlay.etag().as_deref(), Some("\"catalog-1\""));
    });
    let _ = std::fs::remove_dir_all(&agent_dir);
}

// ---------------------------------------------------------------------------
// Upstream `Last-Modified` / `generatedAt` gate (`isNewer`)
// ---------------------------------------------------------------------------

#[test]
fn remote_catalog_gate_matches_upstream_is_newer() {
    // upstream remote-catalog-provider.ts:
    // localGeneratedAt === undefined || (lastModified !== undefined && lastModified > localGeneratedAt)
    assert!(remote_catalog_is_newer(None, None));
    assert!(
        remote_catalog_is_newer(Some(0), None),
        "no local timestamp: any remote catalog applies"
    );
    assert!(
        !remote_catalog_is_newer(None, Some(5)),
        "missing Last-Modified is not newer"
    );
    assert!(
        !remote_catalog_is_newer(Some(5), Some(5)),
        "equal timestamps are not newer"
    );
    assert!(remote_catalog_is_newer(Some(6), Some(5)));
    assert!(!remote_catalog_is_newer(Some(4), Some(5)));
    // The pinned local timestamp is the manifest generatedAt in milliseconds.
    assert_eq!(LOCAL_GENERATED_AT_MS, 1_792_886_400_000);
    assert_eq!(
        LAST_MODIFIED_NEWER_MS - LAST_MODIFIED_OLDER_MS,
        24 * 60 * 60 * 1000
    );
}

#[test]
fn overlay_gate_applies_a_newer_catalog_and_skips_an_older_one() {
    let server = MockCatalogServer::start(BODY_V1, "\"gate-1\"");
    server.set_last_modified(Some(LAST_MODIFIED_OLDER));
    let agent_dir = overlay_agent_dir("gate-order");
    let registry = test_registry();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = Arc::new(
            RemoteCatalogOverlay::new(
                overlay_config("overlay-provider", server.url.clone(), agent_dir.clone()),
                Arc::clone(&registry),
            )
            .expect("overlay"),
        );

        // Remote catalog older than the shipped model data: it is cached but
        // never registered (the built-in catalog stays authoritative).
        assert!(
            !overlay.refresh_once().await.expect("older refresh"),
            "a rejected catalog does not change the overlay"
        );
        assert!(overlay.remote_models().is_empty());
        assert!(registry.get_provider_models("overlay-provider").is_empty());
        let entry = read_models_store_entry(&agent_dir, "overlay-provider").expect("cached entry");
        assert_eq!(entry.models.len(), 1, "the downloaded body is still cached");
        assert_eq!(entry.last_modified, Some(LAST_MODIFIED_OLDER_MS));

        // The server catches up: the same catalog now counts as newer and the
        // overlay is swapped in.
        server.set_last_modified(Some(LAST_MODIFIED_NEWER));
        assert!(overlay.refresh_once().await.expect("newer refresh"));
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model");
        assert_eq!(overlay.remote_models().len(), 1);
        let entry = read_models_store_entry(&agent_dir, "overlay-provider").expect("cached entry");
        assert_eq!(entry.last_modified, Some(LAST_MODIFIED_NEWER_MS));
    });
    let _ = std::fs::remove_dir_all(&agent_dir);
}

#[test]
fn overlay_gate_skips_an_equal_or_missing_last_modified() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    // Equal timestamp: the gate is strict (`lastModified > localGeneratedAt`).
    let equal = MockCatalogServer::start(BODY_V1, "\"gate-2\"");
    equal.set_last_modified(Some(LAST_MODIFIED_EQUAL));
    let equal_dir = overlay_agent_dir("gate-equal");
    let equal_registry = test_registry();
    runtime.block_on(async {
        let overlay = RemoteCatalogOverlay::new(
            overlay_config("overlay-provider", equal.url.clone(), equal_dir.clone()),
            Arc::clone(&equal_registry),
        )
        .expect("overlay");
        assert!(!overlay.refresh_once().await.expect("equal refresh"));
        assert!(overlay.remote_models().is_empty());
        assert!(equal_registry
            .get_provider_models("overlay-provider")
            .is_empty());
        let entry = read_models_store_entry(&equal_dir, "overlay-provider").expect("cached entry");
        assert_eq!(entry.last_modified, Some(LOCAL_GENERATED_AT_MS));
    });
    let _ = std::fs::remove_dir_all(&equal_dir);

    // Missing header: upstream `readCatalog` records 0, which is never newer
    // than a known local generatedAt.
    let missing = MockCatalogServer::start(BODY_V1, "\"gate-3\"");
    missing.set_last_modified(None);
    let missing_dir = overlay_agent_dir("gate-missing");
    let missing_registry = test_registry();
    runtime.block_on(async {
        let overlay = RemoteCatalogOverlay::new(
            overlay_config("overlay-provider", missing.url.clone(), missing_dir.clone()),
            Arc::clone(&missing_registry),
        )
        .expect("overlay");
        assert!(!overlay
            .refresh_once()
            .await
            .expect("missing-header refresh"));
        assert!(overlay.remote_models().is_empty());
        assert!(missing_registry
            .get_provider_models("overlay-provider")
            .is_empty());
        let entry =
            read_models_store_entry(&missing_dir, "overlay-provider").expect("cached entry");
        assert_eq!(entry.last_modified, Some(0), "missing header is 0");
    });
    let _ = std::fs::remove_dir_all(&missing_dir);
}

#[test]
fn overlay_gate_compatibility_path_without_local_generated_at() {
    // An overlay with no local generatedAt (upstream `localGeneratedAt ===
    // undefined`) applies the remote catalog even without a `Last-Modified`
    // header.
    let server = MockCatalogServer::start(BODY_V1, "\"compat-1\"");
    server.set_last_modified(None);
    let agent_dir = overlay_agent_dir("gate-compat");
    let registry = test_registry();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = RemoteCatalogOverlay::new(
            overlay_config("overlay-provider", server.url.clone(), agent_dir.clone()),
            Arc::clone(&registry),
        )
        .expect("overlay")
        .with_local_generated_at(None);
        assert!(overlay.refresh_once().await.expect("compat refresh"));
        let models = registry.get_provider_models("overlay-provider");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-test-model");
    });
    let _ = std::fs::remove_dir_all(&agent_dir);
}

/// Seed a `models-store.json` entry for `overlay-provider` and run an offline
/// overlay over it: the dead catalog URL can only contribute through the
/// restore path, so the returned ids isolate the restore gate.
fn restored_offline(
    agent_dir: &std::path::Path,
    last_modified: Option<u64>,
    local_generated_at: Option<u64>,
) -> Vec<String> {
    write_models_store_entry(
        agent_dir,
        "overlay-provider",
        ModelsStoreEntry {
            models: serde_json::from_str::<Vec<serde_json::Value>>(BODY_V1).expect("seed models"),
            etag: Some("\"seed-1\"".to_string()),
            last_modified,
            checked_at: Some(1),
        },
    )
    .expect("seed store");
    let registry = test_registry();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = RemoteCatalogOverlay::new(
            overlay_config(
                "overlay-provider",
                "http://127.0.0.1:1/v1".to_string(),
                agent_dir.to_path_buf(),
            ),
            Arc::clone(&registry),
        )
        .expect("overlay")
        .with_local_generated_at(local_generated_at);
        overlay
            .refresh_once()
            .await
            .expect("offline refresh degrades silently");
        overlay
            .remote_models()
            .iter()
            .map(|model| model.id.clone())
            .collect()
    })
}

#[test]
fn overlay_restore_honors_the_generated_at_gate() {
    let newer = overlay_agent_dir("restore-newer");
    assert_eq!(
        restored_offline(
            &newer,
            Some(LAST_MODIFIED_NEWER_MS),
            Some(LOCAL_GENERATED_AT_MS)
        ),
        vec!["remote-test-model".to_string()],
        "a persisted catalog newer than the built-in data restores"
    );
    let _ = std::fs::remove_dir_all(&newer);

    let older = overlay_agent_dir("restore-older");
    assert!(
        restored_offline(
            &older,
            Some(LAST_MODIFIED_OLDER_MS),
            Some(LOCAL_GENERATED_AT_MS)
        )
        .is_empty(),
        "an older persisted catalog is ignored"
    );
    let _ = std::fs::remove_dir_all(&older);

    let equal = overlay_agent_dir("restore-equal");
    assert!(
        restored_offline(
            &equal,
            Some(LOCAL_GENERATED_AT_MS),
            Some(LOCAL_GENERATED_AT_MS)
        )
        .is_empty(),
        "an equal timestamp is not newer"
    );
    let _ = std::fs::remove_dir_all(&equal);

    let missing = overlay_agent_dir("restore-missing");
    assert!(
        restored_offline(&missing, None, Some(LOCAL_GENERATED_AT_MS)).is_empty(),
        "an entry without lastModified (written by an older build) is ignored while the local generatedAt is known"
    );
    let _ = std::fs::remove_dir_all(&missing);

    let compat = overlay_agent_dir("restore-compat");
    assert_eq!(
        restored_offline(&compat, None, None),
        vec!["remote-test-model".to_string()],
        "without a local generatedAt the legacy response applies (compatibility path)"
    );
    let _ = std::fs::remove_dir_all(&compat);
}

#[test]
fn personalized_overlay_restores_a_saved_list_without_the_gate() {
    // `withPersonalizedCatalog` restores its saved key-specific list as is;
    // the generation gate only applies to fresh pi.dev catalogs.
    let agent_dir = overlay_agent_dir("personalized-restore");
    write_models_store_entry(
        &agent_dir,
        "openrouter",
        ModelsStoreEntry {
            models: serde_json::from_str::<Vec<serde_json::Value>>(BODY_V1).expect("seed models"),
            checked_at: Some(1),
            ..ModelsStoreEntry::default()
        },
    )
    .expect("seed store");
    let registry = test_registry();
    let personalize: Arc<PersonalizeCatalog> =
        Arc::new(|catalog: Vec<Model>| Box::pin(async move { Ok((catalog, Vec::new())) }));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let overlay = RemoteCatalogOverlay::new_personalized(
            overlay_config(
                "openrouter",
                "http://127.0.0.1:1/v1".to_string(),
                agent_dir.clone(),
            ),
            Arc::clone(&registry),
            Some(personalize),
        )
        .expect("overlay");
        overlay
            .refresh_once()
            .await
            .expect("offline personalized restore");
        assert_eq!(
            overlay.remote_models().len(),
            1,
            "the saved list restores without a lastModified gate"
        );
    });
    let _ = std::fs::remove_dir_all(&agent_dir);
}

#[test]
fn remote_catalog_defaults_are_conservative() {
    // Upstream remote-catalog-provider.ts refresh cadence.
    assert_eq!(
        REMOTE_CATALOG_REFRESH_INTERVAL,
        Duration::from_secs(4 * 60 * 60)
    );
    assert_eq!(REMOTE_CATALOG_REQUEST_TIMEOUT, Duration::from_secs(4));
}

// ---------------------------------------------------------------------------
// Registry-registered catalog provider sanity for the repaired entries
// ---------------------------------------------------------------------------

#[test]
fn repaired_entries_register_into_a_fresh_registry() {
    let mut registry = ProviderRegistry::new(
        Arc::new(InMemoryCredentialStore::default()),
        Arc::new(MapAuthContext::new(BTreeMap::new())),
    );
    let count = pir::ai::providers::register_builtin_catalog(&mut registry).expect("register");
    assert_eq!(count, pir::ai::providers::catalog::CATALOG_FILES.len());
    let haiku = registry
        .get_model("anthropic", "claude-haiku-5-5")
        .expect("haiku 5.5 registered");
    assert_eq!(haiku.api, "anthropic-messages");
    let auto = registry
        .get_model("openrouter", "auto")
        .expect("auto registered");
    assert_eq!(auto.max_tokens, 30_000);
}
