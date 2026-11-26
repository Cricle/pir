//! Pure payload/path helpers shared by all node modules. Migrated from the
//! `node_fns.rs` helpers so the crate speaks only `serde_json::Value` and
//! `Path`, with no rquickjs or pir dependency.

use std::path::{Component, Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::pir_node::error::{NodeError, NodeResult};

/// Read a required string field, failing with `EINVAL` when absent.
pub fn payload_text<'a>(payload: &'a JsonValue, name: &str) -> NodeResult<&'a str> {
    payload
        .get(name)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing `{name}` payload field")))
}

/// Read a string field from an op payload, defaulting to empty string.
pub fn payload_str<'a>(payload: &'a JsonValue, name: &str) -> &'a str {
    payload.get(name).and_then(JsonValue::as_str).unwrap_or("")
}

/// Read an optional string field.
pub fn payload_opt_str<'a>(payload: &'a JsonValue, name: &str) -> Option<&'a str> {
    payload.get(name).and_then(JsonValue::as_str)
}

/// Read a boolean field, defaulting to false.
pub fn payload_bool(payload: &JsonValue, name: &str) -> bool {
    payload
        .get(name)
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

/// Decode a payload data field: either a base64 string (`encoding:"base64"`)
/// or a UTF-8 string / array of byte numbers. Mirrors `node_fns.rs` closely
/// enough for the first migration tranche; module ops can override.
pub fn payload_bytes(payload: &JsonValue) -> NodeResult<Vec<u8>> {
    if let Some(text) = payload.as_str() {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(text) = payload.get("data").and_then(JsonValue::as_str) {
        let encoding = payload
            .get("encoding")
            .and_then(JsonValue::as_str)
            .unwrap_or("utf8");
        if encoding == "base64" {
            use base64::Engine;
            return base64::engine::general_purpose::STANDARD
                .decode(text.as_bytes())
                .map_err(|e| NodeError::new("EINVAL", format!("invalid base64 data: {e}")));
        }
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(array) = payload.get("data").and_then(JsonValue::as_array) {
        let mut out = Vec::with_capacity(array.len());
        for value in array {
            let byte = value
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| NodeError::new("EINVAL", "byte array element out of range"))?;
            out.push(byte);
        }
        return Ok(out);
    }
    Ok(Vec::new())
}

/// Normalize a path for sandbox checks.
///
/// Prefers the canonical filesystem path (resolving symlinks, `..` and Windows
/// 8.3 names), then strips the `\\?\` extended prefix; a path that does not
/// exist canonicalizes its deepest existing ancestor and appends the rest
/// lexically, so a symlinked directory inside an allowed root cannot smuggle a
/// target outside it. Port of `node_fns.rs::normalize_for_check`.
pub fn normalize_for_check(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_extended_prefix(canonical);
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            let mut joined = strip_extended_prefix(canonical_parent);
            joined.push(name);
            return lexical_normalize(&joined);
        }
    }
    lexical_normalize(path)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `\\?\C:\...` -> `C:\...` (and `\\?\UNC\a\b` -> `\\a\b`) so canonical and
/// lexical forms display and compare identically.
fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// Whether `path` is within any of `roots`.
///
/// Component-wise, case-insensitive on Windows (NTFS is), exactly like
/// `node_fns.rs::path_within`; both sides are check-normalized first.
pub fn path_within(path: &Path, roots: &[PathBuf]) -> bool {
    let candidate = normalize_for_check(path);
    roots
        .iter()
        .any(|root| path_within_root(&candidate, &normalize_for_check(root)))
}

/// Component-wise prefix test between two already-normalized paths.
///
/// Streams both component iterators instead of materializing two
/// `Vec<String>` (one per root, on every fenced path): same comparison,
/// no per-call allocation beyond the lossy text of each component.
pub fn path_within_root(candidate: &Path, root: &Path) -> bool {
    let mut candidate_components = candidate.components();
    let mut matched = 0usize;
    for root_component in root.components() {
        match candidate_components.next() {
            Some(candidate_component) => {
                if !component_eq(candidate_component, root_component) {
                    return false;
                }
                matched += 1;
            }
            None => return false,
        }
    }
    // An empty root has no components and never matches (as before).
    matched > 0
}

/// Component equality with the old `component_key` semantics: lossy text,
/// lowercased on Windows (case-insensitive NTFS). Comparing the lowercased
/// character streams is exactly what comparing two `str::to_lowercase()`
/// strings did, without building either string.
fn component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    let left = left.as_os_str().to_string_lossy();
    let right = right.as_os_str().to_string_lossy();
    if cfg!(windows) {
        left.chars()
            .flat_map(char::to_lowercase)
            .eq(right.chars().flat_map(char::to_lowercase))
    } else {
        left.as_ref() == right.as_ref()
    }
}
