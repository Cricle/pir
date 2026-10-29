//! Remote-catalog overlay install path (HANDOFF §1 open item #1).
//!
//! Verifies the production wiring of `install_remote_catalog_overlays`:
//! - the offline gate (`PI_OFFLINE` -> 0 spawns, zero network traffic);
//! - the install count across every built-in catalog provider;
//! - the request path of a spawned overlay against a local mock HTTP server
//!   (URL shape, 200 swap registered into the registry, ETag echoed back as
//!   `If-None-Match` on the next revalidation);
//! - the delayed first hop through a REAL install point
//!   (`ServerState::default()` spawns 41 overlays, none of which may fire
//!   within a test window — the first revalidation is one full interval out,
//!   mirroring upstream's `stored.checkedAt < interval -> skip`).
//!
//! Standalone binary: tests/ and tests/suites/ are another agent's conflict
//! zone, so nothing is registered into any `g_*.rs` suite. One `#[test]` fn
//! keeps the process-global env-var mutations (`PI_OFFLINE`,
//! `PI_CATALOG_BASE_URL`, `PIR_AGENT_DIR`) race-free.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pir::ai::provider::ProviderRegistry;
use pir::ai::providers::{
    install_remote_catalog_overlays, RemoteCatalogOverlay, RemoteCatalogOverlayConfig,
};

/// The mock catalog body: one chat model for the overlay to merge over the
/// built-in `anthropic` baseline (api id must be one of the supported
/// adapters so `CatalogProviderEntry::from_models` can register it).
const CATALOG_BODY: &str = r#"[
    {
        "id": "overlay-probe-model",
        "api": "anthropic-messages",
        "name": "Overlay Probe",
        "baseUrl": "http://127.0.0.1:1",
        "input": ["text"],
        "cost": { "input": 1.0, "output": 2.0 }
    }
]"#;

/// One request seen by the mock server.
#[derive(Debug, Clone)]
struct RecordedRequest {
    path: String,
    if_none_match: Option<String>,
}

/// A local HTTP server answering every request with `200` + `ETag: "v1"` and
/// the catalog body above, recording request paths and validators.
struct MockCatalogServer {
    base: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    count: Arc<AtomicUsize>,
}

impl MockCatalogServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock catalog listener");
        let port = listener.local_addr().expect("local addr").port();
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let thread_requests = Arc::clone(&requests);
        let thread_count = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Read the request head (one HTTP/1.1 GET per connection).
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
                    if buffer.len() > 64 * 1024 {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buffer).to_string();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let if_none_match = head.lines().find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .starts_with("if-none-match:")
                        .then(|| {
                            line.split_once(':')
                                .map(|(_, value)| value.trim().to_string())
                        })
                        .flatten()
                });
                thread_count.fetch_add(1, Ordering::SeqCst);
                thread_requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(RecordedRequest {
                        path,
                        if_none_match,
                    });
                let body = CATALOG_BODY;
                // The Last-Modified header is newer than the embedded catalog's
                // pinned generatedAt (2026-10-25T00:00:00Z), so the overlay
                // passes the upstream `isNewer` gate.
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"v1\"\r\nLast-Modified: Sun, 25 Oct 2026 12:00:00 GMT\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                // The connection closes when `stream` drops (Connection: close).
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            requests,
            count,
        }
    }

    fn request_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn requests_snapshot(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wait until `predicate` holds over the recorded requests or `limit`
    /// elapses (bounded polling; the test fails loudly on timeout).
    fn wait_for(&self, limit: Duration, predicate: impl Fn(&[RecordedRequest]) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if predicate(&self.requests_snapshot()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        predicate(&self.requests_snapshot())
    }
}

/// A throwaway agent dir so `models-store.json` persistence never touches the
/// developer's real `~/.pi/agent`.
fn temp_agent_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-catalog-overlay-install-{}-{tag}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp agent dir");
    dir
}

/// Save/restore helper for the process-global env vars this test mutates.
struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn take(vars: &'static [&'static str]) -> Self {
        let saved = vars
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        Self { saved }
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

#[test]
fn catalog_overlay_install_end_to_end() {
    let env = EnvGuard::take(&["PI_OFFLINE", "PI_CATALOG_BASE_URL", "PIR_AGENT_DIR"]);
    let agent_dir = temp_agent_dir("e2e");
    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::remove_var("PI_OFFLINE");
    std::env::remove_var("PI_CATALOG_BASE_URL");
    let mock = MockCatalogServer::start();
    let expected_path = "/api/models/providers/anthropic?types=chat,image,classifier";

    // --- offline gate: PI_OFFLINE=1 must spawn nothing and touch no network.
    std::env::set_var("PI_OFFLINE", "1");
    let registry = Arc::new(ProviderRegistry::default());
    let offline_count = install_remote_catalog_overlays(&registry);
    assert_eq!(
        offline_count, 0,
        "PI_OFFLINE=1 must gate the overlay install entirely"
    );
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        mock.request_count(),
        0,
        "offline install must not hit the catalog endpoint"
    );
    std::env::remove_var("PI_OFFLINE");

    let runtime = tokio::runtime::Runtime::new().expect("test tokio runtime");
    runtime.block_on(async {
        // --- request path: a spawned overlay with a small interval hits the
        // mock, registers the merged provider into the registry, and echoes
        // the stored ETag back as `If-None-Match` on the next pass.
        let registry = Arc::new(ProviderRegistry::default());
        let overlay = Arc::new(
            RemoteCatalogOverlay::new(
                RemoteCatalogOverlayConfig {
                    provider_id: "anthropic".to_string(),
                    catalog_url: format!(
                        "{}/api/models/providers/anthropic?types=chat,image,classifier",
                        mock.base
                    ),
                    agent_dir: agent_dir.clone(),
                    // Small but real interval: the delayed first hop is one
                    // interval out, so the first request lands ~50ms in.
                    interval: Some(Duration::from_millis(50)),
                },
                Arc::clone(&registry),
            )
            .expect("overlay construction"),
        );
        assert!(
            overlay.spawn_refresh_task(),
            "spawn must succeed inside a tokio runtime"
        );
        assert!(
            mock.wait_for(Duration::from_secs(10), |requests| !requests.is_empty()),
            "the spawned overlay must eventually hit the catalog endpoint"
        );
        let first = &mock.requests_snapshot()[0];
        assert_eq!(
            first.path, expected_path,
            "the catalog URL must follow the upstream shape"
        );
        assert!(
            first.if_none_match.is_none(),
            "the first pass has no cached body, so it must not revalidate"
        );
        // 200 swap: the overlay model is registered into the registry's
        // dynamic map and wins over the built-in baseline.
        assert!(
            mock.wait_for(Duration::from_secs(10), |_| {
                registry
                    .get_model("anthropic", "overlay-probe-model")
                    .is_some()
            }),
            "the 200 catalog must be merged into the registry"
        );
        assert!(
            mock.wait_for(Duration::from_secs(10), |requests| {
                requests
                    .iter()
                    .any(|request| request.if_none_match.as_deref() == Some("\"v1\""))
            }),
            "the next pass must revalidate with the stored ETag"
        );

        // --- install function: every built-in catalog provider spawns an
        // overlay (no runtime -> spawn guard skips, so this stays inside
        // block_on), pointing at the mock base URL. A SECOND mock server
        // isolates this phase: the 50ms probe overlay above keeps ticking
        // for its own lifetime and would permanently pollute the request
        // counter the quiet-window assertion below reads.
        let install_mock = MockCatalogServer::start();
        std::env::set_var("PI_CATALOG_BASE_URL", &install_mock.base);
        let registry = Arc::new(ProviderRegistry::default());
        let installed = install_remote_catalog_overlays(&registry);
        let catalog_providers = pir::ai::providers::catalog::CATALOG_FILES.len();
        assert_eq!(
            installed, catalog_providers,
            "one overlay per built-in catalog provider"
        );
        // --- delayed first hop through a REAL install point: the overlays
        // installed via `ServerState::default()` (upstream
        // `stored.checkedAt < interval -> skip`) must not fire within the
        // test window; their first revalidation is one 6h interval out.
        std::thread::sleep(Duration::from_millis(300));
        let baseline = install_mock.request_count();
        let _state = pir::server::ServerState::default();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            install_mock.request_count(),
            baseline,
            "install-point overlays must delay their first revalidation by a full interval"
        );
    });

    runtime.shutdown_timeout(Duration::from_secs(1));
    drop(env);
}
