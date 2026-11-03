//! Regression: Pi-managed auxiliary HTTP clients must use the configured
//! egress proxy.
//!
//! reqwest is built with `default-features = false` (no `system-proxy`), so a
//! bare `Client::builder()` silently ignores env/OS proxies. OAuth token
//! endpoints and the OpenRouter per-key catalog request used such builders and
//! went direct, which fails in proxied deployments. The dead `.invalid` host
//! can only be reached through the proxy; a recorded absolute-URI request
//! proves the proxy was used. No external network is touched.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::ai::api::bedrock::credentials::{
    fetch_ecs_credentials, imds_credentials, sts_assume_role, AwsCredentials,
};
use pir::ai::provider::ProviderRegistry;
use pir::ai::providers::{RemoteCatalogOverlay, RemoteCatalogOverlayConfig};
use serde_json::json;

/// Serializes env mutation in this test binary.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A minimal HTTP proxy: records the absolute-form request line and answers a
/// JSON document itself. Keeps running until the process exits.
fn spawn_proxy(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let first_line = request.lines().next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(first_line);
            let body = r#"{"data":[],"ok":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

struct ProxyEnv {
    saved: Vec<(&'static str, Option<String>)>,
    agent_dir: std::path::PathBuf,
}

impl ProxyEnv {
    fn set(port: u16) -> Self {
        let names = [
            "PI_HTTP_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "NO_PROXY",
            "no_proxy",
            "PIR_AGENT_DIR",
            "AWS_EC2_METADATA_SERVICE_ENDPOINT",
            "AWS_EC2_METADATA_DISABLED",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
        ];
        let saved = names
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect::<Vec<_>>();
        // Pin the global settings scope to an empty directory so a user-level
        // `httpProxy` setting cannot take precedence over the test proxy.
        let agent_dir = std::env::temp_dir().join(format!(
            "pir-ai-proxy-agent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::env::set_var("PIR_AGENT_DIR", &agent_dir);
        std::env::set_var("PI_HTTP_PROXY", format!("http://127.0.0.1:{port}"));
        // No exclusions: the proxy must be the only route to `.invalid`.
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
        Self { saved, agent_dir }
    }
}

impl Drop for ProxyEnv {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let _ = std::fs::remove_dir_all(&self.agent_dir);
    }
}

fn recorded_lines(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().unwrap().clone()
}

/// One-request direct HTTP endpoint (bypasses any configured proxy): records
/// the request line and answers with `body`.
fn spawn_json_endpoint(body: &'static str, seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind direct endpoint");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            seen.lock()
                .unwrap()
                .push(request.lines().next().unwrap_or_default().to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

/// Minimal IMDSv2 metadata service: token PUT, role listing, role document.
/// The endpoint is loopback, so it is only reachable directly — a proxied
/// client would send it to the proxy instead.
fn spawn_imds(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind imds");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let first_line = request.lines().next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(first_line.clone());
            let body = if first_line.starts_with("PUT /latest/api/token") {
                "mock-token".to_string()
            } else if first_line.starts_with("GET /latest/meta-data/iam/security-credentials/ ") {
                "mock-role\n".to_string()
            } else if first_line
                .starts_with("GET /latest/meta-data/iam/security-credentials/mock-role")
            {
                r#"{"AccessKeyId":"AKID-IMDS","SecretAccessKey":"imds-secret","Token":"imds-token","Expiration":"2026-10-09T00:00:00Z"}"#.to_string()
            } else {
                String::new()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

#[tokio::test]
async fn auth_http_post_json_goes_through_configured_proxy() {
    let _guard = ENV_LOCK.lock().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_proxy(Arc::clone(&seen));
    let _env = ProxyEnv::set(port);

    let response = pir::ai::auth::http::post_json("http://dead.invalid/token", &json!({"a": 1}))
        .await
        .expect("post_json through the proxy");
    assert_eq!(response.status, 200);
    assert!(response.body.contains("\"ok\":true"));

    let lines = recorded_lines(&seen);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("POST http://dead.invalid/token ")),
        "the OAuth request must reach the proxy in absolute form: {lines:?}"
    );
}

#[tokio::test]
async fn openrouter_personalize_goes_through_configured_proxy() {
    let _guard = ENV_LOCK.lock().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_proxy(Arc::clone(&seen));
    let _env = ProxyEnv::set(port);

    let models = pir::ai::providers::catalog::openrouter::personalize_openrouter_catalog(
        Vec::new(),
        "http://dead.invalid",
        Some("test-key"),
        Duration::from_secs(5),
    )
    .await
    .expect("personalize through the proxy");
    assert!(models.is_empty());

    let lines = recorded_lines(&seen);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("GET http://dead.invalid/models/user ")),
        "the OpenRouter catalog request must reach the proxy in absolute form: {lines:?}"
    );
}

/// A dynamic provider's remote catalog refresh (`RemoteCatalogOverlay`) must
/// use the configured proxy: the dead `.invalid` host only resolves there.
#[tokio::test]
async fn dynamic_provider_catalog_refresh_goes_through_configured_proxy() {
    let _guard = ENV_LOCK.lock().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_proxy(Arc::clone(&seen));
    let _env = ProxyEnv::set(port);

    let agent_dir = std::env::temp_dir().join(format!(
        "pir-ai-proxy-catalog-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&agent_dir).unwrap();
    let registry = Arc::new(ProviderRegistry::default());
    let overlay = RemoteCatalogOverlay::new(
        RemoteCatalogOverlayConfig {
            provider_id: "anthropic".to_string(),
            catalog_url: "http://dead.invalid/api/models/providers/anthropic".to_string(),
            agent_dir: agent_dir.clone(),
            interval: None,
        },
        registry,
    )
    .expect("overlay builds");
    overlay
        .refresh_once()
        .await
        .expect("the catalog refresh degrades gracefully after the proxied request");

    let lines = recorded_lines(&seen);
    assert!(
        lines.iter().any(|line| line
            .starts_with("GET http://dead.invalid/api/models/providers/anthropic ")),
        "the remote catalog request must reach the proxy in absolute form: {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&agent_dir);
}

/// AWS credential endpoints that are not link-local metadata (STS here) must
/// use the configured proxy: the dead `.invalid` endpoint is proxy-only.
#[tokio::test]
async fn bedrock_sts_credentials_request_goes_through_configured_proxy() {
    let _guard = ENV_LOCK.lock().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_proxy(Arc::clone(&seen));
    let _env = ProxyEnv::set(port);

    let source = AwsCredentials {
        access_key_id: "AKID-SOURCE".to_string(),
        secret_access_key: "source-secret".to_string(),
        session_token: Some("source-token".to_string()),
        source: "test".to_string(),
    };
    // The proxy answers JSON, not STS XML, so the call itself fails to parse;
    // the request line still proves the proxied client was used.
    let _ = sts_assume_role(
        "http://dead.invalid/sts",
        "us-east-1",
        "arn:aws:iam::123:role/target",
        "pir-session",
        &source,
    )
    .await;

    let lines = recorded_lines(&seen);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("POST http://dead.invalid/sts ")),
        "the STS credentials request must reach the proxy in absolute form: {lines:?}"
    );
}

/// ECS container credentials are loopback/link-local metadata: the direct
/// client must bypass a configured proxy (the endpoint is not reachable
/// through it, and the authorization token must not leave the host).
#[tokio::test]
async fn ecs_credentials_endpoint_stays_direct() {
    let _guard = ENV_LOCK.lock().await;
    let proxy_seen = Arc::new(Mutex::new(Vec::new()));
    let proxy_port = spawn_proxy(Arc::clone(&proxy_seen));
    let _env = ProxyEnv::set(proxy_port);

    // Ambient container tokens must not make the direct request fail before
    // it reaches the local mock.
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");

    let direct_seen = Arc::new(Mutex::new(Vec::new()));
    let direct_port = spawn_json_endpoint(
        r#"{"AccessKeyId":"AKID-ECS","SecretAccessKey":"ecs-secret","Token":"ecs-token","Expiration":"2026-10-09T00:00:00Z"}"#,
        Arc::clone(&direct_seen),
    );
    let credentials =
        fetch_ecs_credentials(&format!("http://127.0.0.1:{direct_port}/v2/credentials"))
            .await
            .expect("the direct ECS fetch succeeds");
    assert_eq!(credentials.access_key_id, "AKID-ECS");

    let direct_lines = recorded_lines(&direct_seen);
    assert!(
        direct_lines
            .iter()
            .any(|line| line.starts_with("GET /v2/credentials ")),
        "the ECS endpoint must be reached directly: {direct_lines:?}"
    );
    let proxy_lines = recorded_lines(&proxy_seen);
    assert!(
        proxy_lines.is_empty(),
        "ECS metadata traffic must not use the configured proxy: {proxy_lines:?}"
    );
}

/// IMDS (169.254.169.254 / an overridden loopback endpoint) is link-local
/// metadata with the same direct-only rule as ECS.
#[tokio::test]
async fn imds_credentials_endpoint_stays_direct() {
    let _guard = ENV_LOCK.lock().await;
    let proxy_seen = Arc::new(Mutex::new(Vec::new()));
    let proxy_port = spawn_proxy(Arc::clone(&proxy_seen));
    let _env = ProxyEnv::set(proxy_port);

    let direct_seen = Arc::new(Mutex::new(Vec::new()));
    let imds_port = spawn_imds(Arc::clone(&direct_seen));
    std::env::set_var(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        format!("http://127.0.0.1:{imds_port}"),
    );
    std::env::remove_var("AWS_EC2_METADATA_DISABLED");

    let credentials = imds_credentials()
        .await
        .expect("the mocked IMDSv2 flow resolves directly");
    assert_eq!(credentials.access_key_id, "AKID-IMDS");

    let direct_lines = recorded_lines(&direct_seen);
    assert_eq!(
        direct_lines.len(),
        3,
        "token PUT, role listing and role document: {direct_lines:?}"
    );
    let proxy_lines = recorded_lines(&proxy_seen);
    assert!(
        proxy_lines.is_empty(),
        "IMDS traffic must not use the configured proxy: {proxy_lines:?}"
    );
}
