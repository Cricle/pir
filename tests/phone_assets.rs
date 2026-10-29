//! The embedded phone web client: every copied file is reachable and typed,
//! `/` serves `index.html`, traversal is rejected, and ETags are stable and
//! unique per file.
//!
//! The asset tree lives next to the module (`src/plugins/builtin/phone/assets`)
//! and is embedded with `include_bytes!`; this test walks the on-disk copy so a
//! newly added client file that was not wired into `assets.rs` fails loudly.

#![cfg(feature = "phone")]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use pir::plugins::builtin::phone::assets::{content_type, etag, lookup, paths};

fn assets_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugins/builtin/phone/assets")
}

fn collect_files(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read asset directory")
        .map(|entry| entry.expect("read asset entry").path())
        .collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            collect_files(&path, root, out);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("asset inside root")
                .to_string_lossy()
                .replace('\\', "/");
            out.push(relative);
        }
    }
}

fn asset_files() -> Vec<String> {
    let root = assets_root();
    let mut files = Vec::new();
    collect_files(&root, &root, &mut files);
    files
}

fn assert_sane_content_type(path: &str, content_type: &str) {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let expected = match extension.as_str() {
        "html" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "svg" => "image/svg+xml",
        "webmanifest" => "application/manifest+json",
        "md" => "text/markdown",
        "license" => "text/plain",
        other => panic!("{path}: unexpected extension {other:?}"),
    };
    assert!(
        content_type.starts_with(expected),
        "{path}: expected a {expected} type, got {content_type:?}"
    );
    assert_ne!(
        content_type, "application/octet-stream",
        "{path}: embedded asset must have a real content type"
    );
}

#[test]
fn every_copied_file_is_reachable() {
    let files = asset_files();
    assert!(!files.is_empty(), "asset directory is empty");

    let mut embedded: Vec<String> = paths().map(str::to_owned).collect();
    embedded.sort();
    let mut on_disk = files.clone();
    on_disk.sort();
    assert_eq!(
        embedded, on_disk,
        "assets.rs and the copied asset tree disagree"
    );

    for path in &files {
        let (served_type, bytes) =
            lookup(path).unwrap_or_else(|| panic!("{path}: not reachable through lookup"));
        assert!(!bytes.is_empty(), "{path}: embedded file is empty");
        assert_sane_content_type(path, served_type);
        assert_eq!(
            served_type,
            content_type(path),
            "{path}: lookup and the MIME table disagree"
        );

        // Query strings and a leading slash are request-plumbing, not identity.
        let request_path = format!("/{path}?v=20260323-2");
        let (query_type, query_bytes) = lookup(&request_path)
            .unwrap_or_else(|| panic!("{request_path}: not reachable through lookup"));
        assert_eq!(query_type, served_type);
        assert_eq!(query_bytes, bytes);
    }
}

#[test]
fn root_serves_index_html() {
    let (content_type, bytes) = lookup("/").expect("/ must serve index.html");
    assert!(content_type.starts_with("text/html"));
    let (_, index_bytes) = lookup("index.html").expect("index.html must be embedded");
    assert_eq!(bytes, index_bytes);
    assert!(String::from_utf8_lossy(bytes).contains("window.PI_PHONE_BASE"));
}

#[test]
fn traversal_is_rejected() {
    for path in [
        "..",
        "../index.html",
        "/../index.html",
        "app/../../etc/passwd",
        "/app/../index.html",
        "app\\..\\index.html",
        "%2e%2e/index.html",
        "app/%2e%2e/index.html",
        "/app/main.js/../../index.html",
    ] {
        assert!(lookup(path).is_none(), "{path:?} must not resolve");
        assert!(etag(path).is_none(), "{path:?} must not have an ETag");
    }
}

#[test]
fn etags_are_stable_and_distinct() {
    let files = asset_files();
    let mut seen = HashSet::new();

    for path in &files {
        let first = etag(path).unwrap_or_else(|| panic!("{path}: missing ETag"));
        let second = etag(&format!("/{path}?v=1"))
            .unwrap_or_else(|| panic!("{path}: missing ETag for query form"));
        assert_eq!(first, second, "{path}: ETag is not stable");
        assert!(
            first.starts_with('"') && first.ends_with('"'),
            "{path}: ETag must be a quoted strong entity tag, got {first:?}"
        );
        assert!(
            seen.insert(first),
            "{path}: ETag collides with another file"
        );
    }

    assert_eq!(seen.len(), files.len());
    assert_eq!(etag("/"), etag("index.html"));
    assert_eq!(etag("/"), etag("/index.html?v=1"));
}
