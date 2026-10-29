//! gzip negotiation for the embedded pi-phone client (`features = "phone"`).
//!
//! The native server must not serve the 10 KB `index.html` (or the other
//! text assets) as identity to browsers that accept gzip. These tests cover
//! the compression primitive, the wire-visible negotiation, conditional
//! requests per representation, and HEAD parity. The request client is a copy
//! of the small buffered helper in `tests/phone_server.rs`; this file does not
//! share code with it so the two suites stay independent.
#![cfg(feature = "phone")]

use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use flate2::read::GzDecoder;
use pir::plugins::builtin::phone::assets::{gzip_encoded, gzip_representation, lookup};
use pir::plugins::builtin::phone::PhonePlugin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Compression primitive
// ---------------------------------------------------------------------------

#[test]
fn html_gzip_representation_is_well_under_half_the_identity_size() {
    let (content_type, identity) = lookup("index.html").expect("index.html is embedded");
    assert!(
        identity.len() >= 256,
        "the HTML asset must be worth compressing"
    );

    let compressed =
        gzip_encoded(content_type, identity).expect("text/html must have a gzip representation");

    eprintln!(
        "index.html: identity={} bytes, gzip={} bytes ({:.1}%)",
        identity.len(),
        compressed.len(),
        compressed.len() as f64 * 100.0 / identity.len() as f64
    );

    // Requirement: both proportionally (< 40%) and absolutely smaller.
    assert!(
        compressed.len() < identity.len(),
        "gzip {} bytes is not smaller than identity {} bytes",
        compressed.len(),
        identity.len()
    );
    assert!(
        compressed.len() * 100 < identity.len() * 40,
        "gzip {} bytes is not under 40% of identity {} bytes",
        compressed.len(),
        identity.len()
    );

    // The one-shot primitive and the per-process cache agree.
    let cached = gzip_representation("index.html").expect("index.html in the shared cache");
    assert_eq!(cached, compressed.as_slice());

    let mut decoded = Vec::new();
    GzDecoder::new(&compressed[..])
        .read_to_end(&mut decoded)
        .expect("gzip stream decodes");
    assert_eq!(decoded, identity);

    // Ineligible types stay identity: markdown is not in the compressible
    // list, and already-compressed/unknown types are never compressed.
    let (markdown_type, markdown) = lookup("PROTOCOL.md").expect("PROTOCOL.md is embedded");
    assert!(gzip_encoded(markdown_type, markdown).is_none());
    assert!(gzip_encoded("image/png", &[0u8; 4096]).is_none());
    assert!(gzip_encoded("application/wasm", &[0u8; 4096]).is_none());
    // Tiny text bodies are not worth the gzip envelope.
    assert!(gzip_encoded("text/plain", b"tiny").is_none());
}

// ---------------------------------------------------------------------------
// Tiny buffered test client (copy of the one in tests/phone_server.rs)
// ---------------------------------------------------------------------------

struct Conn {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Conn {
    async fn connect(port: u16) -> Self {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
            .await
            .expect("connect timed out")
            .expect("connect refused");
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    async fn send(&mut self, bytes: &[u8]) {
        tokio::time::timeout(IO_TIMEOUT, self.stream.write_all(bytes))
            .await
            .expect("write timed out")
            .expect("write failed");
        tokio::time::timeout(IO_TIMEOUT, self.stream.flush())
            .await
            .expect("flush timed out")
            .expect("flush failed");
    }

    async fn fill(&mut self) {
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(IO_TIMEOUT, self.stream.read(&mut chunk))
            .await
            .expect("read timed out")
            .expect("read failed");
        assert!(read > 0, "unexpected EOF");
        self.buffer.extend_from_slice(&chunk[..read]);
    }

    async fn read_through(&mut self, needle: &[u8]) -> Vec<u8> {
        loop {
            if let Some(position) = self
                .buffer
                .windows(needle.len())
                .position(|window| window == needle)
            {
                let end = position + needle.len();
                let out = self.buffer[..end].to_vec();
                self.buffer.drain(..end);
                return out;
            }
            self.fill().await;
        }
    }

    async fn read_exact(&mut self, count: usize) -> Vec<u8> {
        while self.buffer.len() < count {
            self.fill().await;
        }
        let out = self.buffer[..count].to_vec();
        self.buffer.drain(..count);
        out
    }
}

struct Response {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn http_request(
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> Response {
    let mut conn = Conn::connect(port).await;
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    conn.send(request.as_bytes()).await;

    let head = conn.read_through(b"\r\n\r\n").await;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body = if method == "HEAD" || status == 304 || length == 0 {
        Vec::new()
    } else {
        conn.read_exact(length).await
    };

    Response {
        status,
        headers,
        body,
    }
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response.headers.get(name).map(String::as_str)
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut decoded)
        .expect("gzip response body decodes");
    decoded
}

// ---------------------------------------------------------------------------
// Wire negotiation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_negotiates_gzip_per_representation() {
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    // --- baseline: no Accept-Encoding -> identity ------------------------
    let identity = http_request(port, "GET", "/", &[]).await;
    assert_eq!(identity.status, 200);
    assert_eq!(
        header(&identity, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&identity, "vary"), Some("Accept-Encoding"));
    assert_eq!(header(&identity, "content-encoding"), None);
    let identity_length: usize = header(&identity, "content-length")
        .expect("identity Content-Length")
        .parse()
        .expect("numeric length");
    assert_eq!(identity_length, identity.body.len());
    let identity_etag = header(&identity, "etag").expect("identity ETag").to_owned();

    // --- gzip: negotiated headers + round-trip ---------------------------
    let gzip = http_request(port, "GET", "/", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(gzip.status, 200);
    assert_eq!(header(&gzip, "content-encoding"), Some("gzip"));
    assert_eq!(header(&gzip, "vary"), Some("Accept-Encoding"));
    assert_eq!(
        header(&gzip, "content-type"),
        header(&identity, "content-type"),
        "the content type is unchanged by the encoding"
    );
    let gzip_length: usize = header(&gzip, "content-length")
        .expect("gzip Content-Length")
        .parse()
        .expect("numeric length");
    assert_eq!(
        gzip_length,
        gzip.body.len(),
        "Content-Length is the compressed size"
    );
    assert_eq!(gunzip(&gzip.body), identity.body);
    assert!(
        gzip_length < identity_length,
        "gzip {gzip_length} bytes must be smaller than identity {identity_length}"
    );

    let gzip_etag = header(&gzip, "etag").expect("gzip ETag").to_owned();
    assert_ne!(
        gzip_etag, identity_etag,
        "representations need distinct tags"
    );
    assert_eq!(
        gzip_etag,
        format!("\"{}-gzip\"", identity_etag.trim_matches('"')),
        "the gzip tag appends -gzip to the FNV tag"
    );

    // --- negotiation variants -------------------------------------------
    for accept in ["gzip, deflate, br", "br;q=1, gzip;q=0.5", "*", "GZIP"] {
        let response = http_request(port, "GET", "/", &[("Accept-Encoding", accept)]).await;
        assert_eq!(
            header(&response, "content-encoding"),
            Some("gzip"),
            "Accept-Encoding: {accept}"
        );
    }
    for accept in [
        "gzip;q=0",
        "gzip; q=0.000",
        "gzip;q=0, *;q=1",
        "br",
        "",
        "identity",
    ] {
        let response = http_request(port, "GET", "/", &[("Accept-Encoding", accept)]).await;
        assert_eq!(
            header(&response, "content-encoding"),
            None,
            "Accept-Encoding: {accept:?} must stay identity"
        );
        assert_eq!(response.body, identity.body, "Accept-Encoding: {accept:?}");
    }

    // --- MIME whitelist and size floor ----------------------------------
    let markdown = http_request(port, "GET", "/PROTOCOL.md", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(markdown.status, 200);
    assert_eq!(
        header(&markdown, "content-encoding"),
        None,
        "text/markdown is not in the compressible set"
    );
    let tiny_js = http_request(port, "GET", "/app.js", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(tiny_js.status, 200);
    assert_eq!(
        header(&tiny_js, "content-encoding"),
        None,
        "bodies under 256 bytes stay identity"
    );

    // Non-index assets come from the shared cache.
    let styles = http_request(port, "GET", "/styles.css", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(styles.status, 200);
    assert_eq!(header(&styles, "content-encoding"), Some("gzip"));
    assert_eq!(gunzip(&styles.body), {
        let plain = http_request(port, "GET", "/styles.css", &[]).await;
        assert_eq!(header(&plain, "content-encoding"), None);
        plain.body
    });

    // --- conditional requests are independent per representation --------
    let gzip_304 = http_request(
        port,
        "GET",
        "/",
        &[
            ("Accept-Encoding", "gzip"),
            ("If-None-Match", gzip_etag.as_str()),
        ],
    )
    .await;
    assert_eq!(gzip_304.status, 304);
    assert!(gzip_304.body.is_empty());
    assert_eq!(header(&gzip_304, "content-encoding"), Some("gzip"));
    assert_eq!(header(&gzip_304, "vary"), Some("Accept-Encoding"));
    assert_eq!(header(&gzip_304, "etag"), Some(gzip_etag.as_str()));

    let identity_304 = http_request(
        port,
        "GET",
        "/",
        &[("If-None-Match", identity_etag.as_str())],
    )
    .await;
    assert_eq!(identity_304.status, 304);
    assert!(identity_304.body.is_empty());
    assert_eq!(header(&identity_304, "content-encoding"), None);
    assert_eq!(header(&identity_304, "etag"), Some(identity_etag.as_str()));

    // Tags only match the representation they were issued for.
    let identity_tag_with_gzip = http_request(
        port,
        "GET",
        "/",
        &[
            ("Accept-Encoding", "gzip"),
            ("If-None-Match", identity_etag.as_str()),
        ],
    )
    .await;
    assert_eq!(identity_tag_with_gzip.status, 200);
    assert_eq!(
        header(&identity_tag_with_gzip, "content-encoding"),
        Some("gzip")
    );

    let gzip_tag_without_gzip =
        http_request(port, "GET", "/", &[("If-None-Match", gzip_etag.as_str())]).await;
    assert_eq!(gzip_tag_without_gzip.status, 200);
    assert_eq!(header(&gzip_tag_without_gzip, "content-encoding"), None);

    // --- HEAD mirrors GET headers, with no body -------------------------
    let head_gzip = http_request(port, "HEAD", "/", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(head_gzip.status, 200);
    assert!(head_gzip.body.is_empty(), "HEAD must not carry a body");
    assert_eq!(
        head_gzip.headers, gzip.headers,
        "HEAD and GET headers differ"
    );
    assert_eq!(
        header(&head_gzip, "content-length")
            .expect("HEAD Content-Length")
            .parse::<usize>()
            .expect("numeric length"),
        gzip_length
    );

    let head_identity = http_request(port, "HEAD", "/", &[]).await;
    assert_eq!(head_identity.status, 200);
    assert!(head_identity.body.is_empty());
    assert_eq!(head_identity.headers, identity.headers);

    assert!(plugin.stop_for_test().await);
}
