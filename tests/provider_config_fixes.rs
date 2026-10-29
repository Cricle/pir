//! Regression tests for the provider-config audit fixes (P1-1..P1-4, P2-5..P2-14):
//! chatgpt callback client_id forwarding, models.json layering over the
//! built-in catalog, agent-dir env overrides, the full env-var auth map,
//! bounded `!command` execution, defaultTools merging, auth-check authType,
//! radius structured OAuth errors, the GitHub Copilot model-catalog step, and
//! the concurrent OAuth callback server.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use pir::ai::auth::oauth::github_copilot::parse_github_copilot_model_catalog;
use pir::ai::auth::oauth::openai_chatgpt::{
    credential_from_token_response, redirect_uri, result_from_callback_query,
};
use pir::ai::auth::oauth::radius::{device_poll_decision, RadiusOAuthError, RadiusTokenError};
use pir::ai::auth::{env_api_keys, ApiKeyAuth, MapAuthContext};
use pir::ai::providers::config_value::{
    clear_config_value_cache, execute_command_with_timeout, resolve_config_value,
};
use pir::ai::providers::{
    catalog, load_pi_config_with, merge_pi_config, parse_pi_config, CatalogProviderEntry,
};
use pir::ai::types::{Model, ModelCost, ModelCostRates};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn env_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn set_env(key: &str, value: Option<&str>) {
    match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-provider-fixes-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn map_env(pairs: &[(&str, &str)]) -> MapAuthContext {
    MapAuthContext::new(
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
    )
}

fn sample_model(provider: &str, id: &str, api: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: "https://example.invalid/v1".to_string(),
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

// ---------------------------------------------------------------------------
// P1-1: the chatgpt callback's issued client_id travels through the channel
// into the stored credential
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chatgpt_callback_forwards_client_id_into_credential() {
    let server = pir::ai::auth::callback_server::OAuthCallbackServer::start(
        "OpenAI",
        "127.0.0.1",
        0,
        "/auth/callback",
        None,
        Some("st1"),
    )
    .await
    .expect("callback server starts");
    let port = server.port();

    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
    let mut stream = tokio::time::timeout(std::time::Duration::from_secs(5), connect)
        .await
        .expect("connect")
        .expect("tcp connect");
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(
            b"GET /auth/callback?code=c1&state=st1&client_id=issued-client HTTP/1.1\r\n\
              Host: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write callback");

    let pairs = match server.wait(Some(std::time::Duration::from_secs(5))).await {
        Ok(pir::ai::auth::callback_server::CallbackWait::Completed(pairs)) => pairs,
        other => panic!("callback wait failed: {other:?}"),
    };

    // The full query — including the issued client id — reached the flow.
    let authorization = result_from_callback_query(&pairs, "st1").expect("authorization result");
    assert_eq!(authorization.code, "c1");
    assert_eq!(authorization.client_id, "issued-client");

    // The exchanged credential carries the issued client id for refresh.
    let token = serde_json::json!({
        "access_token": "at",
        "refresh_token": "rt",
        "scope": "openid chatgpt.tokens.use.direct",
        "expires_in": 3600,
        "id_token": "jwt",
    });
    let credential =
        credential_from_token_response(&token, &authorization.client_id).expect("credential");
    assert_eq!(credential.extra_str("clientId"), Some("issued-client"));
}

// ---------------------------------------------------------------------------
// P2-13: spawned per-connection handlers; idle spare connections cannot stall
// the accept loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn callback_server_handles_concurrent_connections() {
    let server = pir::ai::auth::callback_server::OAuthCallbackServer::start(
        "Test",
        "127.0.0.1",
        0,
        "/auth/callback",
        None,
        Some("st2"),
    )
    .await
    .expect("callback server starts");
    let port = server.port();

    // A browser spare connection: opened ahead of the callback, sends nothing.
    let idle = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("idle connect");
    let idle_alive = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("second idle connect");

    // The real callback arrives over a third connection while the spares are
    // still open.
    let mut callback = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("callback connect");
    use tokio::io::AsyncWriteExt;
    callback
        .write_all(b"GET /auth/callback?code=c2&state=st2 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .expect("write callback");

    let wait = server.wait(Some(std::time::Duration::from_secs(5)));
    let pairs = tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .expect("wait must resolve while idle connections stay open")
        .expect("wait ok");
    match pairs {
        pir::ai::auth::callback_server::CallbackWait::Completed(pairs) => {
            assert_eq!(
                pir::ai::auth::callback_server::query_param(&pairs, "code"),
                Some("c2")
            );
        }
        other => panic!("unexpected wait result: {other:?}"),
    }
    // The idle spares are still connected (not closed by the handler):
    // `peer_addr()` stays `Ok` after the peer's FIN, so observe the socket
    // itself. No readability within the window means no FIN/data arrived;
    // readability with `Ok(0)` would be the server closing the spare.
    let mut buf = [0u8; 1];
    match tokio::time::timeout(std::time::Duration::from_millis(200), idle.readable()).await {
        // No data and no EOF: the server kept the idle connection open.
        Err(_) => {}
        Ok(Ok(())) => match idle.try_read(&mut buf) {
            Ok(0) => panic!("server closed the idle spare connection after the callback"),
            Ok(read) => panic!("idle spare received unexpected data ({read} byte(s))"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("idle connection read failed: {error}"),
        },
        Ok(Err(error)) => panic!("idle connection readiness failed: {error}"),
    }
    drop(idle_alive);
}

// ---------------------------------------------------------------------------
// P2-5: PI_OAUTH_CALLBACK_HOST drives the chatgpt redirect URI
// ---------------------------------------------------------------------------

#[test]
fn chatgpt_redirect_uri_follows_callback_host_env() {
    let _guard = env_lock().lock().unwrap();
    set_env("PI_OAUTH_CALLBACK_HOST", None);
    assert_eq!(redirect_uri(), "http://127.0.0.1:1455/auth/callback");
    set_env("PI_OAUTH_CALLBACK_HOST", Some("localhost"));
    assert_eq!(redirect_uri(), "http://localhost:1455/auth/callback");
    set_env("PI_OAUTH_CALLBACK_HOST", None);
}

// ---------------------------------------------------------------------------
// P1-2: models.json provider entries layer onto the built-in catalog
// ---------------------------------------------------------------------------

#[test]
fn models_json_api_key_only_entry_keeps_openai_catalog_models() {
    let config = parse_pi_config(r#"{ "providers": { "openai": { "apiKey": "sk-test" } } }"#)
        .expect("parse");
    let builtin = catalog::load_provider("openai")
        .expect("catalog")
        .expect("openai");
    let merged = merge_pi_config(&config).expect("merge");

    let builtin_ids: std::collections::BTreeSet<&str> = builtin
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    let merged_ids: std::collections::BTreeSet<&str> =
        merged.iter().map(|model| model.id.as_str()).collect();
    assert!(
        merged_ids.is_superset(&builtin_ids),
        "models.json apiKey-only entry must keep the openai catalog models; missing: {:?}",
        builtin_ids.difference(&merged_ids).collect::<Vec<_>>()
    );
    // No bogus synthesized "default" model replaces the catalog.
    assert!(
        !merged_ids.contains("default"),
        "synthesized default model leaked into the merged list"
    );
    for model in &merged {
        assert_eq!(model.provider, "openai");
    }
}

#[test]
fn models_json_model_overrides_adjust_builtin_models() {
    let builtin = catalog::load_provider("openai")
        .expect("catalog")
        .expect("openai");
    let target_id = builtin.models[0].id.clone();
    let original = &builtin.models[0];

    let contents = format!(
        r#"{{
            "providers": {{
                "openai": {{
                    "apiKey": "sk-test",
                    "modelOverrides": {{
                        "{target_id}": {{
                            "name": "Renamed builtin",
                            "contextWindow": 123457,
                            "cost": {{ "input": 7.5 }}
                        }}
                    }}
                }}
            }}
        }}"#
    );
    let config = parse_pi_config(&contents).expect("parse");
    let merged = merge_pi_config(&config).expect("merge");
    let overridden = merged
        .iter()
        .find(|model| model.id == target_id)
        .expect("override target present");

    assert_eq!(overridden.name, "Renamed builtin");
    assert_eq!(overridden.context_window, 123_457);
    // Cost merges per field: only `input` was overridden.
    assert_eq!(overridden.cost.rates.input, 7.5);
    assert_eq!(
        overridden.cost.rates.output, original.cost.rates.output,
        "override must not zero out untouched cost fields"
    );
    assert_eq!(
        overridden.cost.rates.cache_read, original.cost.rates.cache_read,
        "override must not zero out untouched cost fields"
    );
    // Other builtin models are untouched.
    if builtin.models.len() > 1 {
        let other_id = &builtin.models[1].id;
        let other = merged.iter().find(|model| model.id == *other_id).unwrap();
        assert_eq!(other.name, builtin.models[1].name);
        assert_eq!(other.context_window, builtin.models[1].context_window);
    }
}

#[test]
fn models_json_entry_upserts_replace_same_id_builtin_model() {
    let builtin = catalog::load_provider("openai")
        .expect("catalog")
        .expect("openai");
    let target_id = builtin.models[0].id.clone();
    let contents = format!(
        r#"{{
            "providers": {{
                "openai": {{
                    "apiKey": "sk-test",
                    "models": [{{ "id": "{target_id}", "baseUrl": "https://proxy.example/v1" }}]
                }}
            }}
        }}"#
    );
    let config = parse_pi_config(&contents).expect("parse");
    let merged = merge_pi_config(&config).expect("merge");
    let matches: Vec<&Model> = merged
        .iter()
        .filter(|model| model.id == target_id)
        .collect();
    assert_eq!(matches.len(), 1, "entry upserts replace the builtin by id");
    assert_eq!(matches[0].base_url, "https://proxy.example/v1");
    // Unspecified fields seed from the builtin defaults (api/baseUrl chain).
    assert_eq!(matches[0].api, builtin.models[0].api);
}

// ---------------------------------------------------------------------------
// P2-6/P2-7: override partial merge and ["text"] input default
// ---------------------------------------------------------------------------

#[test]
fn model_overrides_merge_cost_and_thinking_level_map_partially() {
    let contents = r#"{
        "providers": {
            "acme": {
                "name": "Acme",
                "baseUrl": "https://acme.example/v1",
                "models": [
                    {
                        "id": "acme-large",
                        "reasoning": true,
                        "thinkingLevelMap": { "off": null, "high": "max" },
                        "cost": { "input": 1.0, "output": 2.0, "cacheRead": 3.0, "cacheWrite": 4.0 }
                    }
                ],
                "modelOverrides": {
                    "acme-large": {
                        "thinkingLevelMap": { "low": "medium", "high": "xhigh" },
                        "cost": { "input": 9.0 },
                        "contextWindow": 42
                    }
                }
            }
        }
    }"#;
    let config = parse_pi_config(contents).expect("parse");
    let merged = merge_pi_config(&config).expect("merge");
    let model = merged
        .iter()
        .find(|model| model.id == "acme-large")
        .expect("model");

    // thinkingLevelMap merges per key; the entry's other keys survive.
    let map = model.thinking_level_map.as_ref().expect("map");
    assert_eq!(map.get("off"), Some(&None));
    assert_eq!(map.get("high"), Some(&Some("xhigh".to_string())));
    assert_eq!(map.get("low"), Some(&Some("medium".to_string())));

    // cost merges per field; untouched rates keep the entry's values.
    assert_eq!(model.cost.rates.input, 9.0);
    assert_eq!(model.cost.rates.output, 2.0);
    assert_eq!(model.cost.rates.cache_read, 3.0);
    assert_eq!(model.cost.rates.cache_write, 4.0);
    assert_eq!(model.context_window, 42);
}

#[test]
fn models_json_entries_default_to_text_only_input() {
    let contents = r#"{
        "providers": {
            "acme": {
                "name": "Acme",
                "baseUrl": "https://acme.example/v1",
                "models": [{ "id": "acme-large" }]
            }
        }
    }"#;
    let config = parse_pi_config(contents).expect("parse");
    let merged = merge_pi_config(&config).expect("merge");
    let model = merged
        .iter()
        .find(|model| model.id == "acme-large")
        .unwrap();
    assert_eq!(model.input, vec!["text".to_string()]);
}

// ---------------------------------------------------------------------------
// P1-3: the agent-dir env overrides drive models.json resolution
// ---------------------------------------------------------------------------

#[test]
fn models_json_resolves_under_pir_coding_agent_dir() {
    let _guard = env_lock().lock().unwrap();
    let previous = std::env::var("PIR_CODING_AGENT_DIR").ok();
    let dir = temp_dir("agent-dir");
    std::fs::create_dir_all(&dir).expect("agent dir");
    std::fs::write(
        dir.join("models.json"),
        r#"{
            "providers": {
                "acme": {
                    "name": "Acme",
                    "baseUrl": "https://acme.example/v1",
                    "apiKey": "sk-acme",
                    "models": [{ "id": "acme-large" }]
                }
            }
        }"#,
    )
    .expect("write models.json");

    set_env("PIR_CODING_AGENT_DIR", Some(dir.to_str().unwrap()));
    let configured = load_pi_config_with(None)
        .expect("load")
        .expect("models.json resolved under PIR_CODING_AGENT_DIR");
    set_env("PIR_CODING_AGENT_DIR", previous.as_deref());

    assert_eq!(configured.model.provider, "acme");
    assert_eq!(configured.model.id, "acme-large");
    assert_eq!(configured.api_key.as_deref(), Some("sk-acme"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_dir_honors_every_override_variable() {
    let _guard = env_lock().lock().unwrap();
    let dir = temp_dir("agent-dir-vars");
    let dir_str = dir.to_str().unwrap();

    set_env("PIR_AGENT_DIR", None);
    set_env("PIR_CODING_AGENT_DIR", None);
    set_env("PI_CODING_AGENT_DIR", None);
    assert!(
        pir::ai::providers::agent_dir().ends_with(".pi\\agent")
            || pir::ai::providers::agent_dir().ends_with(".pi/agent")
    );

    set_env("PI_CODING_AGENT_DIR", Some(dir_str));
    assert_eq!(pir::ai::providers::agent_dir(), dir);

    set_env("PIR_CODING_AGENT_DIR", Some(dir_str));
    assert_eq!(pir::ai::providers::agent_dir(), dir);

    set_env("PIR_AGENT_DIR", Some(dir_str));
    assert_eq!(pir::ai::providers::agent_dir(), dir);

    for key in [
        "PIR_AGENT_DIR",
        "PIR_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ] {
        set_env(key, None);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every consumer of the agent dir must agree on the alias precedence:
/// `PIR_AGENT_DIR` alone drives the session dir (SDK + discovery), the
/// extension-context `getAgentDir`, and the TUI global dir (custom themes +
/// keybindings settings).
#[test]
fn session_and_theme_dirs_honor_pir_agent_dir_only() {
    let _guard = env_lock().lock().unwrap();
    let dir = temp_dir("agent-dir-alias");
    let dir_str = dir.to_str().unwrap();

    set_env("PIR_CODING_AGENT_DIR", None);
    set_env("PI_CODING_AGENT_DIR", None);
    set_env("PIR_AGENT_DIR", Some(dir_str));

    // Session dir (feeds the SDK + session discovery).
    assert_eq!(pir::session_manager::agent_dir(), dir);
    assert_eq!(pir::session_manager::sessions_dir(), dir.join("sessions"));

    // TUI global dir (custom themes `<dir>/themes` + keybindings settings).
    assert_eq!(pir::tui::terminal::global_agent_dir(), dir);

    set_env("PIR_AGENT_DIR", None);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P1-4: the full env-var auth map
// ---------------------------------------------------------------------------

#[test]
fn env_auth_resolution_for_extended_providers() {
    let cases: &[(&str, &str, &str)] = &[
        ("moonshotai", "MOONSHOT_API_KEY", "sk-moon"),
        ("zai", "ZAI_API_KEY", "sk-zai"),
        ("zai-coding-cn", "ZAI_CODING_CN_API_KEY", "sk-zaicn"),
        ("kimi-coding", "KIMI_API_KEY", "sk-kimi"),
        ("minimax", "MINIMAX_API_KEY", "sk-mini"),
        ("minimax-cn", "MINIMAX_CN_API_KEY", "sk-minicn"),
        ("meta", "META_API_KEY", "sk-meta"),
        ("github-copilot", "COPILOT_GITHUB_TOKEN", "ghp-x"),
        ("radius", "RADIUS_API_KEY", "sk-radius"),
        ("qwen-token-plan", "QWEN_TOKEN_PLAN_API_KEY", "sk-qwen"),
        ("cloudflare-workers-ai", "CLOUDFLARE_API_KEY", "sk-cf"),
        ("cerebras", "CEREBRAS_API_KEY", "sk-cerebras"),
        ("vercel-ai-gateway", "AI_GATEWAY_API_KEY", "sk-gw"),
        ("baseten", "BASETEN_API_KEY", "sk-baseten"),
        ("opencode", "OPENCODE_API_KEY", "sk-oc"),
        ("xiaomi", "XIAOMI_API_KEY", "sk-mi"),
    ];
    for (provider_id, env_var, value) in cases {
        let entries: Vec<_> = env_api_keys::all()
            .into_iter()
            .filter(|(id, _)| id == provider_id)
            .collect();
        assert!(
            entries.iter().any(|(_, keys)| keys.contains(env_var)),
            "{provider_id} is missing {env_var} in env_api_keys::all()"
        );
        let auth = ApiKeyAuth::new(*provider_id, vec![(*env_var).to_string()]);
        let resolved = auth
            .resolve(&map_env(&[(env_var, value)]), None)
            .expect("resolve")
            .expect("env var set");
        assert_eq!(resolved.auth.api_key.as_deref(), Some(*value));
        assert_eq!(resolved.source.as_deref(), Some(*env_var));
    }
}

#[test]
fn anthropic_auth_token_resolves_to_bearer_header() {
    // The built-in anthropic provider wires ANTHROPIC_AUTH_TOKEN with
    // Bearer-header semantics (upstream providers/anthropic.ts).
    let builtin = catalog::load_provider("anthropic")
        .expect("catalog")
        .expect("anthropic");
    let provider = CatalogProviderEntry::from_catalog(&builtin).expect("provider");
    let auths = provider.api_key_auth();
    assert!(!auths.is_empty(), "anthropic provider exposes env auth");

    let bearer = map_env(&[("ANTHROPIC_AUTH_TOKEN", "tok-123")]);
    let mut resolved = None;
    for auth in &auths {
        if let Some(result) = auth.resolve(&bearer, None).expect("resolve") {
            resolved = Some(result);
            break;
        }
    }
    let resolved = resolved.expect("ANTHROPIC_AUTH_TOKEN must resolve");
    assert!(resolved.auth.api_key.is_none());
    let headers = resolved.auth.headers.expect("bearer headers");
    assert_eq!(
        headers.get("Authorization").map(String::as_str),
        Some("Bearer tok-123")
    );
    assert_eq!(resolved.source.as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));

    // Plain api keys still resolve as api keys.
    let plain = map_env(&[("ANTHROPIC_API_KEY", "sk-ant")]);
    let mut resolved = None;
    for auth in &auths {
        if let Some(result) = auth.resolve(&plain, None).expect("resolve") {
            resolved = Some(result);
            break;
        }
    }
    let resolved = resolved.expect("ANTHROPIC_API_KEY must resolve");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("sk-ant"));
    assert!(resolved.auth.headers.is_none());
}

#[test]
fn catalog_provider_env_auth_falls_back_to_default_env_var() {
    let entry = CatalogProviderEntry::from_catalog(&catalog::BuiltinProvider {
        id: "acme-cloud".to_string(),
        display_name: "Acme Cloud".to_string(),
        base_url: None,
        models: vec![sample_model("acme-cloud", "acme-large", "openai-chat")],
    })
    .expect("provider");
    let auths = entry.api_key_auth();
    assert_eq!(auths.len(), 1);
    assert!(auths[0]
        .env_vars
        .contains(&"ACME_CLOUD_API_KEY".to_string()));
}

// ---------------------------------------------------------------------------
// P2-8: bounded `!command` execution with a per-command cache
// ---------------------------------------------------------------------------

#[test]
fn command_values_time_out_instead_of_hanging() {
    #[cfg(windows)]
    let command = "ping -n 30 127.0.0.1";
    #[cfg(not(windows))]
    let command = "sleep 30";
    let started = std::time::Instant::now();
    let resolved = execute_command_with_timeout(command, std::time::Duration::from_millis(250));
    assert_eq!(resolved, None, "timed-out commands resolve to None");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "execution must be bounded, took {:?}",
        started.elapsed()
    );
}

#[test]
fn command_values_over_the_output_cap_resolve_to_none() {
    // Upstream `execSync` throws when `maxBuffer` (1 MiB) is exceeded and the
    // caller maps that to `undefined`; a truncated value must never become a
    // credential or header. Pure bash builtins, so the command is valid in the
    // configured POSIX shell on every platform.
    let command = "i=0; while [ $i -lt 210000 ]; do printf 'xxxxxxxxxx'; i=$((i+1)); done";
    let resolved = execute_command_with_timeout(command, std::time::Duration::from_secs(30));
    assert_eq!(
        resolved, None,
        "stdout above the 1 MiB cap must resolve to undefined, not a truncated value"
    );
}

#[test]
fn command_values_are_cached_per_command() {
    // The cache is keyed by (resolved agent dir, command) and validated against
    // that dir's config stamps, so a concurrent `PIR_AGENT_DIR` mutation from a
    // sibling test would show up as a MISS and re-run this command. Hold the
    // same lock the env-mutating tests use.
    let _guard = env_lock().lock().unwrap();
    clear_config_value_cache();
    let path = temp_dir("command-cache").join("hits.txt");
    // The command appends a marker to a file (each execution is observable)
    // and echoes a value on stdout.
    // `execute_command_with_timeout` always runs a POSIX shell, so the command
    // must use POSIX redirection and a forward-slash path on Windows too.
    let path_arg = path.to_string_lossy().replace('\\', "/");
    let command = format!("echo run >> {path_arg}; echo ok");

    let config = format!("!{command}");
    assert_eq!(resolve_config_value(&config, None).as_deref(), Some("ok"));
    // Second resolution hits the cache: no second execution.
    assert_eq!(resolve_config_value(&config, None).as_deref(), Some("ok"));
    let contents = std::fs::read_to_string(&path).expect("command ran");
    assert_eq!(
        contents.matches("run").count(),
        1,
        "the command must run exactly once while cached"
    );

    // Clearing the cache re-executes.
    clear_config_value_cache();
    assert_eq!(resolve_config_value(&config, None).as_deref(), Some("ok"));
    let contents = std::fs::read_to_string(&path).expect("command ran again");
    assert_eq!(contents.matches("run").count(), 2);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// ---------------------------------------------------------------------------
// P2-9: an empty project defaultTools list keeps the inherited selection
// ---------------------------------------------------------------------------

#[test]
fn empty_default_tools_override_keeps_inherited_list() {
    use pir::config::settings::deep_merge_settings_values;
    let mut merged = serde_json::json!({ "defaultTools": ["read", "bash"] });
    deep_merge_settings_values(&mut merged, &serde_json::json!({ "defaultTools": [] }));
    assert_eq!(
        merged["defaultTools"],
        serde_json::json!(["read", "bash"]),
        "a pure-modifier (empty) list must keep the inherited selection"
    );

    // Modifier lists still append (the modifiers are resolved later, by
    // `resolve_default_tools`).
    let mut merged = serde_json::json!({ "defaultTools": ["read", "bash"] });
    deep_merge_settings_values(
        &mut merged,
        &serde_json::json!({ "defaultTools": ["-bash", "+edit"] }),
    );
    assert_eq!(
        merged["defaultTools"],
        serde_json::json!(["read", "bash", "-bash", "+edit"])
    );

    // Plain lists still replace.
    let mut merged = serde_json::json!({ "defaultTools": ["read", "bash"] });
    deep_merge_settings_values(
        &mut merged,
        &serde_json::json!({ "defaultTools": ["read"] }),
    );
    assert_eq!(merged["defaultTools"], serde_json::json!(["read"]));
}

// ---------------------------------------------------------------------------
// P2-10: `pir auth check --json` derives authType from the resolved source
// ---------------------------------------------------------------------------

fn run_pir(args: &[&str], envs: &[(&str, &str)], agent_dir: &std::path::Path) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command.args(args).env("PIR_AGENT_DIR", agent_dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    for (provider, keys) in env_api_keys::all() {
        if !envs.iter().any(|(key, _)| keys.contains(key)) {
            for key in keys {
                command.env_remove(key);
            }
        }
        let _ = provider;
    }
    let output = command.output().expect("pir binary should start");
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn auth_check_json_reports_api_key_for_env_sourced_keys() {
    let agent = temp_dir("auth-check-api");
    let stdout = run_pir(
        &["auth", "check", "--provider", "openai", "--json"],
        &[("OPENAI_API_KEY", "sk-env-test")],
        &agent,
    );
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("auth check must print JSON ({error}): {stdout}"));
    // Upstream `AuthCheckResult` shape (cli/auth-check.ts): status/provider/
    // reason/authType; the resolved env key is not echoed.
    assert_eq!(json["status"], serde_json::json!("ready"));
    assert_eq!(json["authType"], serde_json::json!("api_key"));
    let _ = std::fs::remove_dir_all(&agent);
}

#[test]
fn auth_check_json_reports_oauth_for_stored_oauth_credentials() {
    let agent = temp_dir("auth-check-oauth");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let expires = (pir::ai::util::now_ms() + 60 * 60 * 1000) as i64;
    let storage = pir::ai::auth::FileAuthStorage::new(agent.join("auth.json"));
    storage
        .store(
            "kimi-coding",
            pir::ai::auth::StoredCredential::OAuth(pir::ai::auth::OAuthCredential::new(
                "access-token",
                "refresh-token",
                expires,
            )),
        )
        .expect("store oauth credential");

    let stdout = run_pir(
        &[
            "auth",
            "check",
            "--provider",
            "kimi-coding",
            "--json",
            "--no-refresh",
        ],
        &[],
        &agent,
    );
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("auth check must print JSON ({error}): {stdout}"));
    // Upstream `AuthCheckResult` shape: an OAuth credential reports `oauth`.
    assert_eq!(json["status"], serde_json::json!("ready"));
    assert_eq!(json["authType"], serde_json::json!("oauth"));
    let _ = std::fs::remove_dir_all(&agent);
}

// ---------------------------------------------------------------------------
// P2-11: radius device polling switches on the structured OAuth error
// ---------------------------------------------------------------------------

#[test]
fn radius_device_poll_switches_on_structured_oauth_error() {
    let oauth_error = |code: &str| {
        RadiusTokenError::OAuth(RadiusOAuthError {
            status: 400,
            oauth_error: Some(code.to_string()),
            message: format!("Radius OAuth token request failed: {code}"),
        })
    };
    assert!(matches!(
        device_poll_decision::<String>(&oauth_error("authorization_pending")),
        Some(pir::ai::auth::device_code::DevicePoll::Pending)
    ));
    // slow_down keeps the server interval.
    assert!(matches!(
        device_poll_decision::<String>(&oauth_error("slow_down")),
        Some(pir::ai::auth::device_code::DevicePoll::SlowDown {
            interval_seconds: None
        })
    ));
    assert!(matches!(
        device_poll_decision::<String>(&oauth_error("expired_token")),
        Some(pir::ai::auth::device_code::DevicePoll::Failed { .. })
    ));
    assert!(matches!(
        device_poll_decision::<String>(&oauth_error("access_denied")),
        Some(pir::ai::auth::device_code::DevicePoll::Failed { .. })
    ));
    // Unknown codes and transport failures are not poll outcomes.
    assert!(device_poll_decision::<String>(&oauth_error("invalid_grant")).is_none());
    assert!(device_poll_decision::<String>(&RadiusTokenError::Transport(
        pir::ai::error::AiError::Authentication("connection refused".to_string())
    ))
    .is_none());
}

// ---------------------------------------------------------------------------
// P2-12: GitHub Copilot model-catalog fetch + policy enable
// ---------------------------------------------------------------------------

#[test]
fn copilot_model_catalog_parsing_matches_upstream_semantics() {
    let raw = serde_json::json!({
        "data": [
            { "id": "picker-on", "model_picker_enabled": true, "policy": { "state": "unconfigured" }, "capabilities": { "supports": {} } },
            { "id": "picker-off-disabled", "model_picker_enabled": false, "policy": { "state": "disabled" } },
            { "id": "no-tools", "model_picker_enabled": true, "capabilities": { "supports": { "tool_calls": false } } }
        ]
    });
    let parsed = parse_github_copilot_model_catalog(&raw, false).expect("parse");
    assert_eq!(parsed.available_model_ids, vec!["picker-on".to_string()]);
    assert!(parsed.policy_model_ids.is_empty());

    // Individual-account fallback: no picker flags, policy-enabled entries win.
    let raw = serde_json::json!({
        "data": [
            { "id": "policy-enabled", "model_picker_enabled": false, "policy": { "state": "enabled" } }
        ]
    });
    let parsed = parse_github_copilot_model_catalog(&raw, true).expect("parse");
    assert_eq!(
        parsed.available_model_ids,
        vec!["policy-enabled".to_string()]
    );

    // Malformed responses are rejected.
    assert!(parse_github_copilot_model_catalog(&serde_json::json!({}), false).is_err());
}

#[tokio::test]
async fn copilot_fetches_model_catalog_and_enables_policy_models() {
    let builtin = catalog::load_provider("github-copilot")
        .expect("catalog")
        .expect("github-copilot");
    let builtin_id = builtin.models[0].id.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let port = listener.local_addr().expect("addr").port();
    let models_body = serde_json::json!({
        "data": [
            {
                "id": builtin_id,
                "model_picker_enabled": true,
                "policy": { "state": "unconfigured" },
                "capabilities": { "supports": {} }
            },
            {
                "id": "other-model",
                "model_picker_enabled": true,
                "policy": { "state": "enabled" },
                "capabilities": { "supports": {} }
            }
        ]
    })
    .to_string();
    let policy_hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let hits = policy_hits.clone();
    let server = tokio::spawn(async move {
        // Serve the fetch plus one policy POST per call.
        for _ in 0..3 {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).await.expect("read");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buffer).to_string();
            let (response_body, status) = if head.starts_with("GET /models") {
                (models_body.clone(), "200 OK")
            } else if head.starts_with("POST /models/") && head.contains("/policy") {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (r#"{"state":"enabled"}"#.to_string(), "200 OK")
            } else {
                (r#"{"error":"not found"}"#.to_string(), "404 Not Found")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    let base_url = format!("http://127.0.0.1:{port}");
    let catalog =
        pir::ai::auth::oauth::github_copilot::fetch_models_at(&base_url, "copilot-token", 2)
            .await
            .expect("models fetch");
    assert_eq!(
        catalog.available_model_ids,
        vec![builtin_id.clone(), "other-model".to_string()]
    );
    // Only the unconfigured entry that exists in the built-in catalog is
    // queued for policy enable.
    assert_eq!(catalog.policy_model_ids, vec![builtin_id.clone()]);

    let enabled = pir::ai::auth::oauth::github_copilot::enable_model_at(
        &base_url,
        "copilot-token",
        &catalog.policy_model_ids[0],
    )
    .await
    .expect("policy enable");
    assert!(enabled);
    server.abort();

    // availableModelIds lands on the credential.
    let mut credential = pir::ai::auth::OAuthCredential::new("a", "r", 1);
    pir::ai::auth::oauth::github_copilot::set_available_model_ids(
        &mut credential,
        vec!["x".to_string(), "x".to_string(), "y".to_string()],
    );
    assert_eq!(
        credential
            .extra
            .get("availableModelIds")
            .and_then(serde_json::Value::as_array)
            .map(|ids| ids.len()),
        Some(2)
    );
}
