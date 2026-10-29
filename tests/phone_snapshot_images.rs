//! Snapshot image extraction (the reference `phone-images.ts`).
//!
//! Inline base64 images in the published snapshot are stored once in the
//! `/api/image` store and rewritten to short `previewUrl` references, so a
//! snapshot carrying screenshots stays far below the WebSocket frame cap and
//! the raw bytes remain fetchable from the running server.

#![cfg(feature = "phone")]

use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use pir::plugins::builtin::phone::server::MAX_WS_FRAME_BYTES;
use pir::plugins::builtin::phone::PhonePlugin;

const WAIT: Duration = Duration::from_secs(10);

/// A deterministic ~330 KB payload (the reference's ~440 KB base64
/// screenshot): comfortably larger than any URL that replaces it.
fn image_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 330_000];
    let mut state: u64 = 0x5_DE_EC_E6_6D;
    for byte in &mut bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *byte = (state >> 33) as u8;
    }
    bytes
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

async fn http_get(port: u16, path: &str) -> HttpResponse {
    let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .expect("connect timed out")
        .expect("connect refused");
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request write");
    stream.flush().await.expect("request flush");
    let mut bytes = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut bytes))
        .await
        .expect("response read timed out")
        .expect("response read");
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP head terminator");
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let body = bytes[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    HttpResponse {
        status,
        headers,
        body,
    }
}

/// Two identical images become one cache entry and one URL; the snapshot
/// drops the base64 and the server still serves the exact bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_images_share_one_url_and_serve_the_raw_bytes() {
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    let bytes = image_bytes();
    let data = STANDARD.encode(&bytes);
    assert!(
        data.len() > 400_000,
        "the fixture must be a real screenshot size"
    );

    plugin.publish_snapshot(json!({
        "state": { "isStreaming": false },
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "look at this" },
                { "type": "image", "data": data, "mimeType": "image/png" },
                { "type": "image", "data": data, "mimeType": "image/png" },
            ],
        }],
    }));

    let stored = plugin.published_snapshot().expect("snapshot published");
    let content = stored["messages"][0]["content"]
        .as_array()
        .expect("content array");
    assert_eq!(content.len(), 3);
    assert_eq!(
        content[0],
        json!({ "type": "text", "text": "look at this" })
    );
    for part in &content[1..] {
        assert_eq!(part["type"], "image");
        assert_eq!(part["mimeType"], "image/png");
        assert!(
            part.get("data").is_none(),
            "the base64 body must be dropped: {part}"
        );
        assert!(
            part["previewUrl"]
                .as_str()
                .is_some_and(|url| url.starts_with("/api/image/")),
            "a previewUrl must replace the body: {part}"
        );
    }
    assert_eq!(
        content[1]["previewUrl"], content[2]["previewUrl"],
        "identical image bytes must share one content-addressed key"
    );
    assert_eq!(
        plugin.image_stats(),
        (1, bytes.len()),
        "one entry holding the decoded bytes"
    );

    // The whole point: the frame no longer carries the image bytes.
    let serialized = serde_json::to_vec(&stored).expect("serialize snapshot");
    assert!(
        serialized.len() < MAX_WS_FRAME_BYTES,
        "snapshot must fit the WS frame cap: {} bytes",
        serialized.len()
    );
    assert!(
        serialized.len() < 4096,
        "the rewritten snapshot must be URL-sized, got {} bytes",
        serialized.len()
    );

    let url = content[1]["previewUrl"].as_str().expect("previewUrl");
    let response = http_get(port, url).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body, bytes, "the raw image bytes must be served");
    assert_eq!(response.header("content-type"), Some("image/png"));
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    assert!(plugin.stop_for_test().await);
}

/// The producer nests some message lists under `state`; those are rewritten
/// too.
#[test]
fn nested_state_messages_are_rewritten() {
    let plugin = PhonePlugin::new();
    let data = STANDARD.encode(b"nested image");
    plugin.publish_snapshot(json!({
        "state": {
            "messages": [{
                "content": [{ "type": "image", "data": data, "mimeType": "image/webp" }],
            }],
        },
    }));

    let stored = plugin.published_snapshot().expect("snapshot published");
    let part = &stored["state"]["messages"][0]["content"][0];
    assert_eq!(part["type"], "image");
    assert_eq!(part["mimeType"], "image/webp");
    assert!(part.get("data").is_none());
    assert!(part["previewUrl"]
        .as_str()
        .is_some_and(|url| url.starts_with("/api/image/")));
    assert_eq!(plugin.image_stats(), (1, b"nested image".len()));
}

/// Bad base64 and empty data keep the original part: extraction must never
/// lose an image.
#[test]
fn unstorable_images_keep_their_data() {
    let plugin = PhonePlugin::new();
    plugin.publish_snapshot(json!({
        "messages": [{
            "content": [
                { "type": "image", "data": "!!!not-base64!!!", "mimeType": "image/png" },
                { "type": "image", "data": "", "mimeType": "image/png" },
            ],
        }],
    }));

    let stored = plugin.published_snapshot().expect("snapshot published");
    for part in stored["messages"][0]["content"]
        .as_array()
        .expect("content array")
    {
        assert_eq!(part["type"], "image");
        assert!(
            part.get("data").is_some(),
            "an unstorable image keeps its bytes: {part}"
        );
        assert!(part.get("previewUrl").is_none());
    }
    assert_eq!(plugin.image_stats(), (0, 0));
}

/// The TTL is enforced on reads too: an idle store (no new image stored)
/// releases entries past the two-hour TTL at the next `get`, instead of
/// retaining them until the next `store_bytes`.
#[test]
#[cfg_attr(target_os = "linux", ignore = "Windows-only phone TTL fixture")]
fn an_idle_image_store_releases_entries_past_the_ttl() {
    let plugin = PhonePlugin::new();
    let key = plugin
        .store_image_for_test(&STANDARD.encode(b"expiring"), "image/png")
        .expect("store image");
    assert_eq!(plugin.image_stats(), (1, b"expiring".len()));

    // Past the TTL (the real wait is two hours) with no further store: the
    // next read must prune and answer 404.
    plugin.age_images_for_test(Duration::from_secs(3 * 60 * 60));
    assert!(
        plugin.image_for_test(&key).is_none(),
        "an expired entry must not be served"
    );
    assert_eq!(
        plugin.image_stats(),
        (0, 0),
        "the read path must release expired entries"
    );
}

/// A snapshot with no inline image is returned untouched (the fast path).
#[test]
fn images_free_snapshots_are_unchanged() {
    let plugin = PhonePlugin::new();
    let snapshot: Value = json!({
        "state": { "isStreaming": false },
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
        ],
    });
    plugin.publish_snapshot(snapshot.clone());
    assert_eq!(plugin.published_snapshot().expect("snapshot"), snapshot);
    assert_eq!(plugin.image_stats(), (0, 0));
}
