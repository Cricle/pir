//! The embedded pi-phone browser client.
//!
//! The upstream `@malinamnam/pi-phone` 0.0.13 package ships its mobile web UI
//! under `public/`; this module embeds that tree byte-for-byte with
//! `include_bytes!` so a native build needs no external asset directory.
//!
//! Provenance:
//! - Source: `https://github.com/MaliNamNam/pi-phone` (npm
//!   `@malinamnam/pi-phone` 0.0.13), copied from the local checkout
//!   `C:/Users/XPENG_USER/Desktop/github/pi-phone/public/`.
//! - 27 files, 308,354 bytes; relative paths preserved under `assets/`.
//! - The upstream package has **no LICENSE/COPYING file and no `license`
//!   field**; only `vendor/marked.LICENSE` (MIT, for the vendored markdown
//!   parser) ships with it. The client code itself carries no license grant;
//!   this port exists because the user owns/requested it.
//!
//! Wire contract: see `assets/PROTOCOL.md` (also embedded and served) for
//! every fetch path, WebSocket message and token placement the client uses.
//!
//! # API
//!
//! - [`lookup`] resolves a request path (query/fragment ignored, leading `/`
//!   optional, `/` -> `index.html`, `..`/backslash/`%`/NUL rejected) to
//!   `(content_type, bytes)`; [`lookup_with_path`] additionally returns the
//!   normalized asset path, which is the gzip cache key.
//! - [`etag`] returns a strong, quoted ETag derived from an FNV-1a 64-bit
//!   content hash, computed once and cached in a [`OnceLock`]. It is stable
//!   across runs and distinct per embedded file. [`gzip_etag`] derives the
//!   distinct tag of the gzip representation (`"<fnv>-gzip"`).
//! - [`content_type`] is the MIME table (html/css/js/json/svg/png/ico/
//!   webmanifest/map plus a few future-proof entries).
//! - [`gzip_encoded`] compresses a body when its MIME type is compressible and
//!   gzip actually shrinks it; [`gzip_representation`] returns the cached
//!   per-process representation of an embedded asset.
//!
//! `lookup` returns the unmodified `index.html`; the server lane must rewrite
//! `window.PI_PHONE_BASE = "/";` (`index.html:18`) when the app is mounted
//! under a reverse-proxy prefix such as `/phone-<port>`. Because that rewrite
//! happens per listener, the server compresses the rewritten `index.html`
//! itself and uses the raw-asset cache for every other file.

use std::collections::HashMap;
use std::io::Write;
use std::sync::OnceLock;

use flate2::write::GzEncoder;
use flate2::Compression;

/// One embedded file. Paths are normalized (no leading `/`, `/` is represented
/// by `index.html`) and are also the lookup keys.
struct EmbeddedAsset {
    path: &'static str,
    bytes: &'static [u8],
}

macro_rules! embedded_assets {
    ($(($path:literal, $file:literal $(,)?)),* $(,)?) => {
        static ASSETS: &[EmbeddedAsset] = &[
            $(EmbeddedAsset {
                path: $path,
                bytes: include_bytes!($file),
            }),*
        ];
    };
}

embedded_assets![
    ("index.html", "assets/index.html"),
    ("app.js", "assets/app.js"),
    ("styles.css", "assets/styles.css"),
    ("sw.js", "assets/sw.js"),
    ("manifest.webmanifest", "assets/manifest.webmanifest"),
    ("icon.svg", "assets/icon.svg"),
    ("vendor/marked.esm.js", "assets/vendor/marked.esm.js"),
    ("vendor/marked.LICENSE", "assets/vendor/marked.LICENSE"),
    ("app/attachments.js", "assets/app/attachments.js"),
    (
        "app/autocomplete-controller.js",
        "assets/app/autocomplete-controller.js",
    ),
    ("app/autocomplete.js", "assets/app/autocomplete.js"),
    ("app/bindings.js", "assets/app/bindings.js"),
    ("app/command-catalog.js", "assets/app/command-catalog.js"),
    ("app/commands.js", "assets/app/commands.js"),
    ("app/constants.js", "assets/app/constants.js"),
    ("app/formatters.js", "assets/app/formatters.js"),
    ("app/handlers.js", "assets/app/handlers.js"),
    ("app/main.js", "assets/app/main.js"),
    ("app/markdown.js", "assets/app/markdown.js"),
    ("app/messages.js", "assets/app/messages.js"),
    ("app/sheet-actions.js", "assets/app/sheet-actions.js"),
    ("app/sheet-navigation.js", "assets/app/sheet-navigation.js"),
    ("app/sheets-view.js", "assets/app/sheets-view.js"),
    ("app/state.js", "assets/app/state.js"),
    ("app/tool-rendering.js", "assets/app/tool-rendering.js"),
    ("app/transport.js", "assets/app/transport.js"),
    ("app/ui.js", "assets/app/ui.js"),
    ("PROTOCOL.md", "assets/PROTOCOL.md"),
];

/// The embedded files, in table order, as normalized paths.
pub fn paths() -> impl Iterator<Item = &'static str> {
    ASSETS.iter().map(|asset| asset.path)
}

/// Resolve a request path to `(content_type, bytes)`.
///
/// Normalization: query strings and fragments are ignored, leading slashes are
/// dropped, `.` segments are skipped, `/` (or an empty path) resolves to
/// `index.html`, and any `..` segment — or a path containing `\`, `%`, or NUL —
/// is rejected. Path comparison is case-sensitive, matching HTTP semantics.
pub fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    lookup_with_path(path).map(|(_, content_type, bytes)| (content_type, bytes))
}

/// Like [`lookup`], but also returns the normalized embedded path.
///
/// The extra path is the stable cache key for [`gzip_representation`] (the
/// request path can carry a leading slash, a query string, or `/`).
pub fn lookup_with_path(path: &str) -> Option<(&'static str, &'static str, &'static [u8])> {
    let normalized = normalize_path(path)?;
    let asset = ASSETS.iter().find(|asset| asset.path == normalized)?;
    Some((asset.path, content_type(asset.path), asset.bytes))
}

/// The strong, quoted ETag for a request path (`"<16 hex digits>"`).
///
/// `None` for the same inputs [`lookup`] rejects or cannot resolve. The hash is
/// FNV-1a over the file bytes and is cached after the first call.
pub fn etag(path: &str) -> Option<String> {
    let normalized = normalize_path(path)?;
    etags()
        .iter()
        .find(|(asset_path, _)| *asset_path == normalized)
        .map(|(_, tag)| tag.clone())
}

/// MIME type for a path, by extension. The table covers every extension in the
/// embedded tree plus the common image/map/font cases; unknown extensions are
/// `application/octet-stream`.
pub fn content_type(path: &str) -> &'static str {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let file_name = path.rsplit('/').next().unwrap_or_default();
    let extension = file_name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");

    if extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm") {
        return "text/html; charset=utf-8";
    }
    if extension.eq_ignore_ascii_case("css") {
        return "text/css; charset=utf-8";
    }
    if extension.eq_ignore_ascii_case("js") || extension.eq_ignore_ascii_case("mjs") {
        return "text/javascript; charset=utf-8";
    }
    if extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("map") {
        return "application/json; charset=utf-8";
    }
    if extension.eq_ignore_ascii_case("svg") {
        return "image/svg+xml";
    }
    if extension.eq_ignore_ascii_case("png") {
        return "image/png";
    }
    if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        return "image/jpeg";
    }
    if extension.eq_ignore_ascii_case("gif") {
        return "image/gif";
    }
    if extension.eq_ignore_ascii_case("webp") {
        return "image/webp";
    }
    if extension.eq_ignore_ascii_case("ico") {
        return "image/x-icon";
    }
    if extension.eq_ignore_ascii_case("webmanifest") {
        return "application/manifest+json";
    }
    if extension.eq_ignore_ascii_case("md") {
        return "text/markdown; charset=utf-8";
    }
    if extension.eq_ignore_ascii_case("txt") || extension.eq_ignore_ascii_case("license") {
        return "text/plain; charset=utf-8";
    }
    if extension.eq_ignore_ascii_case("woff") {
        return "font/woff";
    }
    if extension.eq_ignore_ascii_case("woff2") {
        return "font/woff2";
    }
    "application/octet-stream"
}

/// Minimum identity size worth a gzip representation.
///
/// Below this, the gzip header/footer overhead and the compression CPU cost
/// are not worth the saved bytes.
pub const GZIP_MIN_BYTES: usize = 256;

/// Whether a MIME type is worth gzip-compressing.
///
/// Only text-like payloads (and SVG, which is text) are compressed. Types that
/// already carry their own compression (PNG/JPEG/WebP/woff2/wasm) are skipped
/// by omission, as are unknown/octet-stream bodies.
pub fn is_compressible(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "text/html"
            | "text/css"
            | "application/javascript"
            | "text/javascript"
            | "application/json"
            | "application/manifest+json"
            | "image/svg+xml"
            | "text/plain"
    )
}

/// gzip-compress `bytes` for a response with `content_type`.
///
/// Returns `None` when the type is not compressible, the body is below
/// [`GZIP_MIN_BYTES`], or gzip would not actually shrink the body. The gzip
/// header carries no timestamp, so the output is deterministic for a given
/// input.
pub fn gzip_encoded(content_type: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < GZIP_MIN_BYTES || !is_compressible(content_type) {
        return None;
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(bytes).ok()?;
    let compressed = encoder.finish().ok()?;
    (compressed.len() < bytes.len()).then_some(compressed)
}

/// Derive the ETag of the gzip representation from the identity ETag.
///
/// The identity tag is a quoted strong tag (`"<16 hex digits>"`); `-gzip` is
/// appended to the opaque value while keeping the quotes, so the result stays
/// a valid strong entity tag and can never collide with the identity tag.
/// `None` when `etag` is not a quoted tag.
pub fn gzip_etag(etag: &str) -> Option<String> {
    let inner = etag.strip_prefix('"')?.strip_suffix('"')?;
    Some(format!("\"{inner}-gzip\""))
}

/// The cached gzip representation of an embedded asset, keyed by its
/// normalized path.
///
/// The cache is built once per process on first use; later calls are a hash
/// lookup. The bytes are the compressed *raw* asset: `index.html` is returned
/// in its unmounted form, while the server serves the per-listener mount
/// rewrite and compresses that separately. `None` when the asset is not
/// compressible, is too small, or gzip would not shrink it.
pub fn gzip_representation(path: &str) -> Option<&'static [u8]> {
    let normalized = normalize_path(path)?;
    gzip_representations()
        .get(normalized.as_str())
        .map(Vec::as_slice)
}

/// Build the shared gzip representation cache now.
///
/// The server calls this while starting a listener so the first compressed
/// request only performs a lookup.
pub fn warm_gzip_cache() {
    gzip_representations();
}

fn gzip_representations() -> &'static HashMap<&'static str, Vec<u8>> {
    static CACHE: OnceLock<HashMap<&'static str, Vec<u8>>> = OnceLock::new();

    CACHE.get_or_init(|| {
        ASSETS
            .iter()
            .filter_map(|asset| {
                let compressed = gzip_encoded(content_type(asset.path), asset.bytes)?;
                Some((asset.path, compressed))
            })
            .collect()
    })
}

fn normalize_path(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if path.contains('\0') || path.contains('\\') || path.contains('%') {
        return None;
    }

    let mut normalized = String::with_capacity(path.len());
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(segment);
    }

    if normalized.is_empty() {
        Some("index.html".to_string())
    } else {
        Some(normalized)
    }
}

/// `(path, etag)` pairs, built and hashed once on first use.
fn etags() -> &'static [(&'static str, String)] {
    static ETAGS: OnceLock<Vec<(&'static str, String)>> = OnceLock::new();

    ETAGS.get_or_init(|| {
        ASSETS
            .iter()
            .map(|asset| (asset.path, format!("\"{:016x}\"", fnv1a64(asset.bytes))))
            .collect()
    })
}

/// FNV-1a 64-bit, over the raw file bytes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
