//! Strict remote-catalog refresh for `update --models` (upstream
//! `remote-catalog-provider.ts` + `models.ts` `refresh`): non-OK statuses
//! error with `Model catalog request failed for {provider}: {status}`
//! (404/501 skip), transport errors surface into the aggregate, and providers
//! without a resolvable credential are skipped silently. The production
//! background overlay path keeps its documented lenient (silent-degradation)
//! behavior.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::ai::provider::ProviderRegistry;
use pir::ai::providers::{RemoteCatalogOverlay, RemoteCatalogOverlayConfig};

/// A valid one-model catalog body (same shape the other catalog suites use).
const CATALOG_BODY: &str = r#"[
    {
        "id": "strict-refresh-probe",
        "api": "anthropic-messages",
        "name": "Strict Refresh Probe",
        "baseUrl": "http://127.0.0.1:1",
        "input": ["text"],
        "cost": { "input": 1.0, "output": 2.0 }
    }
]"#;

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-catalog-strict-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the mock server answers with.
#[derive(Clone, Copy)]
enum Reply {
    /// `200` + `ETag: "v1"` + the catalog body.
    Ok,
    /// `304` when the request carries `if-none-match`, `200` + ETag + body
    /// otherwise (a real revalidation cycle).
    Revalidate,
    /// A fixed empty-body status (`404`, `501`, `503`, ...).
    Status(u16),
    /// Close the connection without answering (transport-level failure).
    Drop,
}

/// A local HTTP server answering every request with the configured reply.
struct MockCatalogServer {
    base: String,
    count: Arc<AtomicUsize>,
}

impl MockCatalogServer {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let port = listener.local_addr().expect("local addr").port();
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                thread_count.fetch_add(1, Ordering::SeqCst);
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buffer.extend_from_slice(&chunk[..n]);
                            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let head = String::from_utf8_lossy(&buffer);
                let revalidating = head.to_lowercase().contains("if-none-match:");
                let (status, body, etag) = match reply {
                    Reply::Ok => (200u16, Some(CATALOG_BODY), true),
                    Reply::Revalidate if revalidating => (304, None, false),
                    Reply::Revalidate => (200, Some(CATALOG_BODY), true),
                    Reply::Status(status) => (status, None, false),
                    Reply::Drop => {
                        // Drop the connection without answering: transport
                        // error.
                        drop(stream);
                        continue;
                    }
                };
                let mut response =
                    format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\n");
                if etag {
                    response.push_str("etag: \"v1\"\r\n");
                }
                if status == 200 {
                    // Newer than the embedded catalog's pinned generatedAt
                    // (2026-10-25T00:00:00Z), so the overlay applies.
                    response.push_str("last-modified: Sun, 25 Oct 2026 12:00:00 GMT\r\n");
                }
                let body = body.unwrap_or_default();
                response.push_str(&format!(
                    "content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                ));
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            count,
        }
    }

    fn request_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

fn overlay_for(base: &str, agent_dir: &Path) -> (Arc<ProviderRegistry>, Arc<RemoteCatalogOverlay>) {
    let registry = Arc::new(ProviderRegistry::default());
    let overlay = RemoteCatalogOverlay::new(
        RemoteCatalogOverlayConfig {
            provider_id: "anthropic".to_string(),
            catalog_url: format!(
                "{base}/api/models/providers/anthropic?types=chat,image,classifier"
            ),
            agent_dir: agent_dir.to_path_buf(),
            interval: None,
        },
        Arc::clone(&registry),
    )
    .expect("overlay builds");
    (registry, Arc::new(overlay))
}

/// A non-OK status (other than 404/501) errors on the strict path with the
/// upstream text, while the production background path keeps degrading
/// silently with the same server.
#[tokio::test]
async fn strict_refresh_errors_on_non_ok_status_and_lenient_keeps_silent() {
    let dir = TempDir::new("status-503");
    let server = MockCatalogServer::start(Reply::Status(503));

    let (registry, overlay) = overlay_for(&server.base, dir.path());
    let error = overlay
        .refresh_once_strict()
        .await
        .expect_err("503 must fail the strict refresh");
    assert!(
        error
            .to_string()
            .contains("Model catalog request failed for anthropic: 503"),
        "upstream failure text, got: {error}"
    );
    assert_eq!(server.request_count(), 1);
    assert!(
        registry.get_provider_models("anthropic").is_empty(),
        "a failing refresh registers nothing"
    );

    // The same kind of server on the background path: silent degradation, no
    // error.
    let (_registry, overlay) = overlay_for(&server.base, dir.path());
    overlay
        .refresh_once()
        .await
        .expect("the background path degrades silently");
    assert_eq!(server.request_count(), 2);
}

/// 404 and 501 skip (upstream: the catalog no longer serves this provider):
/// the strict refresh reports success and drops the previously seeded
/// overlay.
#[tokio::test]
async fn strict_refresh_skips_404_and_501() {
    for status in [404u16, 501] {
        let dir = TempDir::new(&format!("status-{status}"));
        // Seed pass: 200 registers the overlay and persists it into the
        // agent dir's models-store.json.
        let seed = MockCatalogServer::start(Reply::Ok);
        let (registry, overlay) = overlay_for(&seed.base, dir.path());
        overlay.refresh_once_strict().await.expect("seed 200 pass");
        assert!(
            registry
                .get_provider_models("anthropic")
                .iter()
                .any(|model| model.id == "strict-refresh-probe"),
            "the 200 pass registers the overlay models"
        );

        // Skip pass: a fresh overlay over the same agent dir restores the
        // persisted overlay, then the skip status clears it.
        let server = MockCatalogServer::start(Reply::Status(status));
        let (_registry, overlay) = overlay_for(&server.base, dir.path());
        let result = overlay.refresh_once_strict().await;
        assert!(result.is_ok(), "{status} is a skip, got: {result:?}");

        // And the overlay is gone from a registry sharing the same dir.
        let (registry, overlay) = overlay_for(&server.base, dir.path());
        overlay.refresh_once_strict().await.expect("skip again");
        assert!(
            !registry
                .get_provider_models("anthropic")
                .iter()
                .any(|model| model.id == "strict-refresh-probe"),
            "{status} must clear the overlay"
        );
    }
}

/// A transport-level failure (connection closed without a response) errors on
/// the strict path and degrades silently on the background path.
#[tokio::test]
async fn strict_refresh_surfaces_transport_errors() {
    let dir = TempDir::new("transport");
    let server = MockCatalogServer::start(Reply::Drop);

    let (_registry, overlay) = overlay_for(&server.base, dir.path());
    let error = overlay
        .refresh_once_strict()
        .await
        .expect_err("a dropped connection must fail the strict refresh");
    assert!(!error.to_string().is_empty(), "{error}");
    assert_eq!(server.request_count(), 1);

    let (_registry, overlay) = overlay_for(&server.base, dir.path());
    overlay
        .refresh_once()
        .await
        .expect("the background path degrades silently");
    assert_eq!(server.request_count(), 2);
}

/// The 304 revalidation path stays a success on the strict path (the ETag is
/// echoed back and the overlay survives).
#[tokio::test]
async fn strict_refresh_keeps_304_as_success() {
    let dir = TempDir::new("status-304");
    let server = MockCatalogServer::start(Reply::Revalidate);

    let (registry, overlay) = overlay_for(&server.base, dir.path());
    overlay.refresh_once_strict().await.expect("first 200 pass");
    assert!(registry
        .get_provider_models("anthropic")
        .iter()
        .any(|model| model.id == "strict-refresh-probe"));

    // Second pass revalidates: the mock answers 304 and the strict refresh
    // keeps the overlay.
    overlay
        .refresh_once_strict()
        .await
        .expect("304 is a success on the strict path");
    assert!(
        registry
            .get_provider_models("anthropic")
            .iter()
            .any(|model| model.id == "strict-refresh-probe"),
        "304 keeps the overlay"
    );
    assert_eq!(server.request_count(), 2);
}

// ---------------------------------------------------------------------------
// End-to-end `update --models` (process-global env, hence the lock)
// ---------------------------------------------------------------------------

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The explicit `update --models` path aggregates strict per-provider
/// failures into the upstream error text: a 503 from the catalog server
/// yields `Could not refresh model catalogs: anthropic: Model catalog
/// request failed for anthropic: 503`.
#[test]
fn update_models_aggregates_strict_failures() {
    let _env = env_lock();
    let previous_base = std::env::var("PI_CATALOG_BASE_URL").ok();
    let previous_anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");

    let server = MockCatalogServer::start(Reply::Status(503));
    std::env::set_var("PI_CATALOG_BASE_URL", &server.base);
    let dir = TempDir::new("update-strict-fail");
    let agent_dir = dir.path().to_path_buf();

    let error = pir::coding_agent::package_manager::update_model_catalogs(&agent_dir)
        .expect_err("a 503 catalog server must fail the refresh");
    let message = error.to_string();
    assert!(
        message.contains("Could not refresh model catalogs: anthropic: Model catalog request failed for anthropic: 503"),
        "upstream aggregated error text, got: {message}"
    );
    assert!(server.request_count() >= 1, "the refresh hit the server");

    match previous_base {
        Some(value) => std::env::set_var("PI_CATALOG_BASE_URL", value),
        None => std::env::remove_var("PI_CATALOG_BASE_URL"),
    }
    match previous_anthropic_key {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
}

/// Providers without a resolvable credential are skipped silently (upstream
/// models.ts:581-583): with no credentials configured the refresh makes zero
/// requests and still prints the upstream success line.
#[test]
fn update_models_skips_providers_without_credentials() {
    let _env = env_lock();
    let previous_base = std::env::var("PI_CATALOG_BASE_URL").ok();
    // The catalog server is unreachable: any refresh attempt would fail (and
    // a transport failure aggregates), so success proves nothing refreshed.
    std::env::set_var("PI_CATALOG_BASE_URL", "http://127.0.0.1:1");
    // "No credentials" must mean no credentials for ANY provider, so the test
    // cannot depend on the machine's ambient environment (a developer shell
    // with `ANTHROPIC_AUTH_TOKEN` exported would otherwise refresh anthropic
    // and fail on the unreachable URL). Every provider env var the credential
    // gate consults is cleared here and restored at the end.
    let cleared: Vec<(&'static str, Option<std::ffi::OsString>)> =
        pir::ai::auth::env_api_keys::all()
            .iter()
            .flat_map(|(_, vars)| vars.iter())
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
    for (name, _) in &cleared {
        std::env::remove_var(name);
    }
    let dir = TempDir::new("update-no-creds");
    let agent_dir = dir.path().to_path_buf();

    let result = pir::coding_agent::package_manager::update_model_catalogs(&agent_dir);
    assert!(
        result.is_ok(),
        "no credentials -> silent skip, got: {result:?}"
    );

    for (name, value) in cleared {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    match previous_base {
        Some(value) => std::env::set_var("PI_CATALOG_BASE_URL", value),
        None => std::env::remove_var("PI_CATALOG_BASE_URL"),
    }
}
