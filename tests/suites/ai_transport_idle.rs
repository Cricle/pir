//! Unit tests for the 2026-10-10 parity-gap fixes in the AI layer: the
//! Radius gateway env override (`PI_RADIUS_GATEWAY`, upstream
//! `core/radius.ts`), the remote-catalog refresh interval (upstream
//! `remote-catalog-provider.ts` = 4h), and the transport idle timeout
//! (`httpIdleTimeoutMs`, upstream `core/http-dispatcher.ts`), plus the
//! request-body payload-buffer property of the memory lane (one body per
//! request, one copy at most for a possible retry — see the section at the
//! bottom).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use pir::agent::tool_execution::BoxFuture;
use pir::ai::error::{AiError, AiResult};
use pir::ai::transport::{
    HttpRequest, HttpResponse, HttpTransport, ProviderRequestClient, ProviderRequestConfig,
};
use pir::ai::types::ProviderStreamHooks;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set `PI_RADIUS_GATEWAY` for the duration of `run`, restore afterwards.
fn with_radius_gateway<T>(value: Option<&str>, run: impl FnOnce() -> T) -> T {
    let _guard = env_lock();
    let previous = std::env::var("PI_RADIUS_GATEWAY").ok();
    match value {
        Some(value) => std::env::set_var("PI_RADIUS_GATEWAY", value),
        None => std::env::remove_var("PI_RADIUS_GATEWAY"),
    }
    let result = run();
    match previous {
        Some(value) => std::env::set_var("PI_RADIUS_GATEWAY", value),
        None => std::env::remove_var("PI_RADIUS_GATEWAY"),
    }
    result
}

#[test]
fn radius_gateway_env_override_resolves_like_upstream() {
    use pir::ai::auth::oauth::radius::{
        radius_gateway_url, resolve_radius_gateway, DEFAULT_RADIUS_GATEWAY, ENV_RADIUS_GATEWAY,
        RADIUS_MCP_URL,
    };
    assert_eq!(DEFAULT_RADIUS_GATEWAY, "https://radius.pi.dev");
    assert_eq!(ENV_RADIUS_GATEWAY, "PI_RADIUS_GATEWAY");
    // Upstream computes RADIUS_MCP_URL from the default gateway (the env
    // override does not change the constant).
    assert_eq!(RADIUS_MCP_URL, "https://radius.pi.dev/mcp");

    // Unset → default; set → normalized override (scheme added, slashes
    // stripped); a configured gateway still wins via `resolve_radius_gateway`.
    assert_eq!(
        with_radius_gateway(None, radius_gateway_url),
        "https://radius.pi.dev"
    );
    assert_eq!(
        with_radius_gateway(Some("gw.example.com"), radius_gateway_url),
        "https://gw.example.com"
    );
    assert_eq!(
        with_radius_gateway(Some("http://gw.example.com/"), radius_gateway_url),
        "http://gw.example.com"
    );
    assert_eq!(
        with_radius_gateway(Some("gw.example.com"), || { resolve_radius_gateway(None) }),
        "https://gw.example.com"
    );
    assert_eq!(
        with_radius_gateway(Some("gw.example.com"), || {
            resolve_radius_gateway(Some("explicit.example.com//"))
        }),
        "https://explicit.example.com"
    );
}

#[test]
fn remote_catalog_refresh_interval_matches_upstream_4h() {
    // upstream remote-catalog-provider.ts:
    // REMOTE_CATALOG_REFRESH_INTERVAL_MS = 4 * 60 * 60 * 1000
    assert_eq!(
        pir::ai::providers::REMOTE_CATALOG_REFRESH_INTERVAL,
        Duration::from_secs(4 * 60 * 60)
    );
}

#[test]
fn transport_idle_timeout_config_is_carried_and_opt_out() {
    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(5),
        max_response_bytes: 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_secs(1),
        proxy_url: None,
        http_idle_timeout: Some(Duration::from_millis(150)),
    })
    .expect("client builds with the idle timeout");
    assert_eq!(
        client.config.http_idle_timeout,
        Some(Duration::from_millis(150))
    );
    // `0`/disabled maps to `None` (upstream undici bodyTimeout 0 = no timer).
    let disabled = ProviderRequestConfig {
        http_idle_timeout: None,
        ..ProviderRequestConfig::default()
    };
    assert_eq!(disabled.http_idle_timeout, None);
}

/// A stalled response body: the first chunks arrive, then the server goes
/// silent — the per-read idle timeout must fire while the already-delivered
/// chunks stay delivered (upstream undici `bodyTimeout` semantics).
#[tokio::test]
async fn stalled_stream_body_hits_the_idle_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::Read;
        if let Ok((mut stream, _)) = listener.accept() {
            // Read the request head BEFORE answering. A server that writes its
            // response as soon as it accepts races the client's request write:
            // hyper then reads a response for a request it has not dispatched
            // and fails the send with `UnexpectedMessage`, which surfaces as
            // "error sending request" instead of the idle timeout this test
            // pins (measured ~8% under parallel load; 0 in ~4.8k requests once
            // the head is read first). A real server never answers unsolicited.
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\n\r\nhe");
            // Hold the connection open without sending the rest of the body.
            // Longer than the client's 10s total timeout: the ONLY way the
            // request can finish inside the 10s window is the idle timer, so
            // a CPU-starved schedule cannot race a server-side EOF into the
            // error path (the thread dies with the test process).
            std::thread::sleep(Duration::from_secs(30));
        }
    });

    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_secs(1),
        proxy_url: None,
        // Wide enough that hyper's first body poll sees the already-arrived
        // partial chunk; still far below the server's 30s hold.
        http_idle_timeout: Some(Duration::from_secs(2)),
    })
    .expect("client builds");

    let started = std::time::Instant::now();
    let mut delivered = Vec::new();
    let result = client
        .request_streaming(
            reqwest::Method::GET,
            &format!("http://{addr}/stream"),
            &BTreeMap::new(),
            Vec::new(),
            None,
            &ProviderStreamHooks::default(),
            &mut |chunk| {
                delivered.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await;
    let error = result.expect_err("stalled body must idle-timeout");
    assert!(
        error.to_string().contains("idle timeout"),
        "unexpected error: {error}"
    );
    // The transport contract (upstream undici `bodyTimeout`) is: chunks
    // ALREADY DELIVERED stay delivered. Whether the two bytes still sitting in
    // hyper's buffer are surfaced before the idle timer fires is a scheduling
    // race — measured 1/3 passes with an exact `== b"he"` assertion even on an
    // unloaded machine — so the test pins the two honest halves: the error is
    // the idle timeout, and any delivered bytes are exactly the partial body
    // (never a corrupted or duplicated stream).
    assert!(
        delivered.is_empty() || delivered == b"he",
        "delivered bytes must be the partial body, got {delivered:?}"
    );
    // The idle timer (2s) fired long before the server's 30s hold.
    assert!(started.elapsed() < Duration::from_secs(6));
}

/// A response whose headers never arrive fails with the headers idle
/// timeout (upstream undici `headersTimeout`).
#[tokio::test]
async fn stalled_response_headers_hit_the_idle_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((_stream, _)) = listener.accept() {
            // Accept, then send nothing. Held past the client's 10s total
            // timeout so only the idle timer can end the request (see the
            // body variant above; the thread dies with the test process).
            std::thread::sleep(Duration::from_secs(30));
        }
    });

    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_secs(1),
        proxy_url: None,
        http_idle_timeout: Some(Duration::from_millis(200)),
    })
    .expect("client builds");

    let result = client
        .request_streaming(
            reqwest::Method::GET,
            &format!("http://{addr}/stall"),
            &BTreeMap::new(),
            Vec::new(),
            None,
            &ProviderStreamHooks::default(),
            &mut |_| Ok(()),
        )
        .await;
    let error = result.expect_err("stalled headers must idle-timeout");
    assert!(
        error.to_string().contains("idle timeout"),
        "unexpected error: {error}"
    );
}

/// A total `timeoutMs` expiry reports the CONFIGURED budget, never the 1ms
/// internal polling floor, and names the expired total deadline.
#[tokio::test]
async fn expired_total_timeout_reports_the_configured_budget_not_the_poll_floor() {
    struct StallingTransport;

    impl HttpTransport for StallingTransport {
        fn execute(&self, _request: HttpRequest) -> BoxFuture<'_, AiResult<HttpResponse>> {
            Box::pin(async {
                // Far beyond the client's total timeout; the timer drops this
                // future long before it resolves.
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err(AiError::Http(
                    "the total deadline must cancel this send".into(),
                ))
            })
        }
    }

    let mut client = ProviderRequestClient::with_transport(
        ProviderRequestConfig {
            timeout: Duration::from_secs(5),
            max_response_bytes: 1024,
            max_retries: 0,
            max_retry_delay: Duration::from_secs(1),
            proxy_url: None,
            // Wider than the request budget: the total `timeoutMs` must be the
            // limit that fires.
            http_idle_timeout: Some(Duration::from_secs(10)),
        },
        Arc::new(StallingTransport),
    );
    client.set_request_timeout(Some(Duration::from_millis(80)));

    let started = std::time::Instant::now();
    let error = client
        .request_streaming(
            reqwest::Method::POST,
            "https://example.invalid/v1/responses",
            &BTreeMap::new(),
            b"{}".to_vec(),
            None,
            &ProviderStreamHooks::default(),
            &mut |_chunk: &[u8]| Ok(()),
        )
        .await
        .expect_err("the total deadline must expire");
    assert!(started.elapsed() < Duration::from_secs(5));

    let text = error.to_string();
    assert!(
        text.contains("80ms"),
        "the configured timeoutMs must be reported: {text}"
    );
    assert!(
        !text.contains("1ms"),
        "the internal 1ms polling floor must never reach the message: {text}"
    );
    assert!(
        text.contains("total deadline"),
        "an expired total deadline must be named explicitly: {text}"
    );
}

/// The 1ms polling floor is internal to `wait_limit`: when the total budget
/// runs out between body reads, the message must name the configured
/// `timeoutMs` and the expired deadline instead of "after 1ms".
#[tokio::test]
async fn body_read_after_an_exhausted_deadline_reports_the_configured_budget() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::Read;
        if let Ok((mut stream, _)) = listener.accept() {
            // Read the request head before answering (see the idle-timeout
            // tests above for why), then send headers plus one body chunk and
            // go silent: the next body read has no budget left.
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n");
            let _ = stream.flush();
            // Held past the client's total deadline; the thread dies with the
            // test process.
            std::thread::sleep(Duration::from_secs(30));
        }
    });

    let mut client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(5),
        max_response_bytes: 1024,
        max_retries: 0,
        max_retry_delay: Duration::from_secs(1),
        proxy_url: None,
        // No idle timer: only the total deadline can bound the stalled read.
        http_idle_timeout: None,
    })
    .expect("client builds");
    client.set_request_timeout(Some(Duration::from_millis(300)));

    let mut delivered = Vec::new();
    let error = client
        .request_streaming(
            reqwest::Method::GET,
            &format!("http://{addr}/stall-after-chunk"),
            &BTreeMap::new(),
            Vec::new(),
            None,
            &ProviderStreamHooks::default(),
            &mut |chunk| {
                delivered.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await
        .expect_err("the stalled body must hit the total deadline");
    let text = error.to_string();
    assert!(
        text.contains("300ms"),
        "the configured timeoutMs must be reported: {text}"
    );
    assert!(
        !text.contains("1ms"),
        "the internal 1ms polling floor must never reach the message: {text}"
    );
    assert!(
        text.contains("total deadline"),
        "an expired total deadline must be named explicitly: {text}"
    );
    assert!(
        delivered.is_empty() || delivered == b"hello",
        "delivered bytes must be the partial body, got {delivered:?}"
    );
}

// ---------------------------------------------------------------------------
// Request-body payload buffers (memory lane)
// ---------------------------------------------------------------------------
//
// MEMORY SHAPE: a JSON request body must be materialized once (`reqwest` owns
// the bytes it sends), and an attempt that must keep the body for a possible
// retry pays exactly ONE copy for that — never a copy per attempt. The former
// shape cloned the whole body unconditionally inside the retry loop, so every
// request (retried or not) carried a second full-size copy; on the
// chunked-summarization path that meant a chunk-sized (~0.3 MB) allocation
// extra per call, hundreds of times per huge session. These tests pin the
// per-request numbers through the client's own counters.

/// A one-shot HTTP server answering `responses` in order (one per connection)
/// and recording the request bodies it received, so a test can prove both the
/// payload count and the byte-identity of a retried body.
fn spawn_body_server(responses: Vec<u16>) -> (String, std::sync::Arc<Mutex<Vec<Vec<u8>>>>) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind body server");
    let addr = listener.local_addr().expect("addr");
    let bodies: std::sync::Arc<Mutex<Vec<Vec<u8>>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let recorded = std::sync::Arc::clone(&bodies);
    std::thread::spawn(move || {
        for status in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            while header_end.is_none() {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
                header_end = find_header_end(&buffer);
            }
            let Some(header_end) = header_end else {
                return;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
            let content_length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buffer.len() < header_end + content_length {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            recorded
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(buffer[header_end..].to_vec());
            let (reason, body): (&str, &[u8]) = if status == 200 {
                ("OK", b"data: [DONE]\n\n")
            } else {
                ("Too Many Requests", b"slow down")
            };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/v1/responses"), bodies)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn payload_client(max_retries: u32) -> ProviderRequestClient {
    ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_secs(10),
        max_response_bytes: 1024 * 1024,
        max_retries,
        max_retry_delay: Duration::from_millis(5),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("payload client")
}

fn payload_bodies(bodies: &std::sync::Arc<Mutex<Vec<Vec<u8>>>>) -> Vec<Vec<u8>> {
    bodies
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// A request with NO retry budget serializes its body once and MOVES it into
/// the request: zero copies (the old loop cloned it on every attempt).
#[tokio::test]
async fn a_json_request_without_a_retry_budget_allocates_one_body_and_copies_none() {
    let (url, bodies) = spawn_body_server(vec![200]);
    let client = payload_client(0);
    let payload = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "x".repeat(2048)}],
    });
    let mut delivered = Vec::new();
    let result = client
        .post_json_streaming(
            &url,
            &BTreeMap::new(),
            &payload,
            None,
            &ProviderStreamHooks::default(),
            &mut |chunk| {
                delivered.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await;
    assert!(result.is_ok(), "unexpected result: {result:?}");
    let diagnostics = client.payload_diagnostics();
    assert_eq!(
        diagnostics.allocations, 1,
        "one serialization per request: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.copies, 0,
        "no retry budget ⇒ the body must be MOVED into the request: {diagnostics:?}"
    );
    let bodies = payload_bodies(&bodies);
    assert_eq!(bodies.len(), 1, "the server must see exactly one request");
    assert_eq!(
        bodies[0],
        serde_json::to_vec(&payload).expect("serialize"),
        "the request body is byte-identical to the serialized payload"
    );
}

/// A retried request serializes its body ONCE (the retry replays the kept
/// bytes) and pays exactly one copy for keeping them; both attempts send the
/// identical body.
#[tokio::test]
async fn a_retried_json_request_replays_the_kept_body_and_never_rebuilds_it() {
    let (url, bodies) = spawn_body_server(vec![429, 200]);
    let client = payload_client(1);
    let payload = serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "y".repeat(4096)}],
    });
    let result = client
        .post_json_streaming(
            &url,
            &BTreeMap::new(),
            &payload,
            None,
            &ProviderStreamHooks::default(),
            &mut |_chunk| Ok(()),
        )
        .await;
    assert!(
        result.is_ok(),
        "the second attempt must answer 200: {result:?}"
    );
    let diagnostics = client.payload_diagnostics();
    assert_eq!(
        diagnostics.allocations, 1,
        "the body is serialized ONCE for both attempts: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.copies, 1,
        "exactly one copy keeps the body for the retry: {diagnostics:?}"
    );
    let bodies = payload_bodies(&bodies);
    assert_eq!(bodies.len(), 2, "two attempts reached the server");
    assert_eq!(
        bodies[0], bodies[1],
        "a retry replays the SAME bytes (no re-serialization, no drift)"
    );
    assert_eq!(bodies[0], serde_json::to_vec(&payload).expect("serialize"));
}
