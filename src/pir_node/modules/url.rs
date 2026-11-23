//! WHATWG URL parsing and serialization for the `url` fast path.
//!
//! Node's `new URL()` is the WHATWG URL Standard; the `url` crate implements
//! the same specification, so the native tier can hand the JS shim a parsed
//! component set instead of re-running the shim's JS parser on every
//! construction.
//!
//! The module also owns the two text kernels behind the shims' hot paths:
//!
//! * [`parse_form_pairs`] — the `application/x-www-form-urlencoded` parser
//!   (`URLSearchParams`' string init);
//! * [`query_parse`] / [`query_stringify`] — Node's `querystring.parse` /
//!   `stringify` state machine and its table-driven escaper.
//!
//! Everything here is a pure function over `&str`: no state, no panics on
//! untrusted input (all slicing goes through explicit bounds checks).

use url::Url;

use crate::pir_node::error::{NodeError, NodeResult};

/// The components Node exposes on a `URL` instance, already serialized the
/// way `URL.prototype` getters return them (percent-encoded, lowercased
/// scheme/host, default ports elided).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub href: String,
    pub protocol: String,
    pub username: String,
    pub password: String,
    pub host: String,
    pub hostname: String,
    pub port: String,
    pub pathname: String,
    pub search: String,
    pub hash: String,
    pub origin: String,
}

fn invalid_url() -> NodeError {
    // The shim never surfaces this message (it answers `undefined` from the
    // native entry so its own failure path keeps Node's exact TypeError), but
    // the contract stays honest for direct Rust callers.
    NodeError::new("ERR_INVALID_URL", "Invalid URL")
}

fn parse_url(input: &str, base: Option<&str>) -> NodeResult<Url> {
    match base {
        Some(base) => {
            let base_url = Url::parse(base).map_err(|_| invalid_url())?;
            base_url.join(input).map_err(|_| invalid_url())
        }
        None => Url::parse(input).map_err(|_| invalid_url()),
    }
}

/// The WHATWG "file path state" maps a drive-letter pipe (`file:///C|/`) to a
/// colon; the `url` crate keeps the pipe, so the serializer would disagree
/// with Node. Returns the rewritten path when the rule applies.
fn normalize_file_drive(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'|' {
        let mut fixed = path.to_owned();
        fixed.replace_range(2..3, ":");
        Some(fixed)
    } else {
        None
    }
}

/// Parse `input` (resolved against `base` when given). `Err` means the WHATWG
/// parser rejected the input — Node throws a `TypeError` there, and the shim
/// keeps the exact message.
pub fn parse(input: &str, base: Option<&str>) -> NodeResult<UrlParts> {
    let parsed = parse_url(input, base)?;

    // File URLs need the pipe→colon drive normalization before the href and
    // pathname are compared with Node's serialization.
    let mut href = parsed.as_str().to_owned();
    let mut pathname = parsed.path().to_owned();
    if parsed.scheme() == "file" {
        if let Some(fixed) = normalize_file_drive(&pathname) {
            href = href.replacen(&pathname, &fixed, 1);
            pathname = fixed;
        }
    }

    let protocol = format!("{}:", parsed.scheme());
    let hostname = parsed.host_str().unwrap_or("").to_owned();
    let port = parsed
        .port()
        .map(|port| port.to_string())
        .unwrap_or_default();
    let host = if port.is_empty() {
        hostname.clone()
    } else {
        format!("{hostname}:{port}")
    };
    // Node's `search`/`hash` getters return "" both when the component is
    // absent and when it is present-but-empty; the serialized `href` keeps the
    // bare "?" / "#" (the `url` crate preserves that too).
    let search = match parsed.query() {
        Some(query) if !query.is_empty() => format!("?{query}"),
        _ => String::new(),
    };
    let hash = match parsed.fragment() {
        Some(fragment) if !fragment.is_empty() => format!("#{fragment}"),
        _ => String::new(),
    };

    // The tuple origin is exactly scheme://host[:port], and `parsed.port()`
    // already elides the scheme's default port, so this avoids `Origin`'s
    // host clone + formatter. `blob:` (whose origin is the inner URL's) keeps
    // the crate implementation; everything else is opaque.
    let origin = match parsed.scheme() {
        scheme @ ("ftp" | "http" | "https" | "ws" | "wss") => {
            if port.is_empty() {
                format!("{scheme}://{hostname}")
            } else {
                format!("{scheme}://{hostname}:{port}")
            }
        }
        "blob" => parsed.origin().ascii_serialization(),
        _ => "null".to_owned(),
    };

    Ok(UrlParts {
        href,
        protocol,
        username: parsed.username().to_owned(),
        password: parsed.password().unwrap_or("").to_owned(),
        host,
        hostname,
        port,
        pathname,
        search,
        hash,
        origin,
    })
}

/// The eleven components joined by U+0000, for the shim's one-string C-ABI
/// entry. The WHATWG serializer percent-encodes U+0000 in every component
/// (and forbids it in a host), so the separator can never occur inside a
/// field. Order: href, protocol, username, password, host, hostname, port,
/// pathname, search, hash, origin.
pub fn parse_payload(input: &str, base: Option<&str>) -> NodeResult<String> {
    use std::fmt::Write as _;

    let parsed = parse_url(input, base)?;
    let scheme = parsed.scheme();
    let hostname = parsed.host_str().unwrap_or("");
    let port = parsed.port();
    let mut href = parsed.as_str().to_owned();
    let mut pathname = parsed.path().to_owned();
    if scheme == "file" {
        if let Some(fixed) = normalize_file_drive(&pathname) {
            href = href.replacen(&pathname, &fixed, 1);
            pathname = fixed;
        }
    }
    let mut out = String::with_capacity(parsed.as_str().len() * 2 + 64);

    out.push_str(&href);
    out.push('\0');
    out.push_str(scheme);
    out.push(':');
    out.push('\0');
    out.push_str(parsed.username());
    out.push('\0');
    out.push_str(parsed.password().unwrap_or(""));
    out.push('\0');
    out.push_str(hostname);
    if let Some(port) = port {
        let _ = write!(out, ":{port}");
    }
    out.push('\0');
    out.push_str(hostname);
    out.push('\0');
    if let Some(port) = port {
        let _ = write!(out, "{port}");
    }
    out.push('\0');
    out.push_str(&pathname);
    out.push('\0');
    // Node's `search`/`hash` getters are "" for absent *and* empty components;
    // the serialized href keeps the bare marker (checked by the shim).
    if let Some(query) = parsed.query() {
        if !query.is_empty() {
            out.push('?');
            out.push_str(query);
        }
    }
    out.push('\0');
    if let Some(fragment) = parsed.fragment() {
        if !fragment.is_empty() {
            out.push('#');
            out.push_str(fragment);
        }
    }
    out.push('\0');
    match scheme {
        scheme @ ("ftp" | "http" | "https" | "ws" | "wss") => {
            out.push_str(scheme);
            out.push_str("://");
            out.push_str(hostname);
            if let Some(port) = port {
                let _ = write!(out, ":{port}");
            }
        }
        "blob" => out.push_str(&parsed.origin().ascii_serialization()),
        _ => out.push_str("null"),
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// application/x-www-form-urlencoded
// ---------------------------------------------------------------------------

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Percent-decode a form component: `+` becomes a space, valid `%XX` escapes
/// decode byte-wise and everything else is literal. The bytes are then decoded
/// as UTF-8 with replacement, exactly the WHATWG form parser (it never throws,
/// unlike `decodeURIComponent`).
pub fn form_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high << 4) | low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an `application/x-www-form-urlencoded` string into ordered pairs,
/// skipping empty sequences. A single leading "?" is stripped (that is what
/// `URLSearchParams`' string init does; `URL.search` already excludes it).
pub fn parse_form_pairs(text: &str) -> Vec<(String, String)> {
    let text = text.strip_prefix('?').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    let mut pairs = Vec::new();
    for sequence in text.split('&') {
        if sequence.is_empty() {
            continue;
        }
        let (name, value) = match sequence.split_once('=') {
            Some((name, value)) => (name, value),
            None => (sequence, ""),
        };
        pairs.push((form_decode(name), form_decode(value)));
    }
    pairs
}

/// `URLSearchParams.toString` component encoder: the
/// `application/x-www-form-urlencoded` percent-encode set keeps ASCII
/// alphanumerics and `*-._`, maps space to `+`, and escapes everything else
/// (notably `~ ! ' ( )`, which `encodeURIComponent` leaves alone).
pub fn form_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b' ' => out.push('+'),
            b if b < 0x80
                && (b.is_ascii_alphanumeric() || matches!(b, b'*' | b'-' | b'.' | b'_')) =>
            {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Serialize flat `[k1, v1, ...]` pairs the way `URLSearchParams.toString`
/// does. `None` for an odd pair count (the shim never produces one).
pub fn form_stringify(pairs: &[String]) -> Option<String> {
    if pairs.len() % 2 != 0 {
        return None;
    }
    let mut out = String::new();
    let mut first = true;
    for chunk in pairs.chunks_exact(2) {
        if !first {
            out.push('&');
        }
        first = false;
        out.push_str(&form_escape(&chunk[0]));
        out.push('=');
        out.push_str(&form_escape(&chunk[1]));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// querystring: escaper (internal/querystring.js `encodeStr`)
// ---------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// The `noEscape` table: `! ' ( ) * - . _ ~`, digits and ASCII letters pass
/// through untouched.
fn no_escape(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'\'' | b'(' | b')' | b'*' | b'-' | b'.' | b'_' | b'~'
        )
}

/// Node's `encodeStr(str, noEscape, hexTable)`: unreserved ASCII passes
/// through, every other *byte* becomes `%XX`. Input arrives as valid UTF-8, so
/// the byte-wise walk produces the same UTF-8 octets Node emits for a code
/// point. (Node additionally pairs UTF-16 surrogates; a lone surrogate throws
/// `ERR_INVALID_URI`, but it cannot cross the C-ABI as a Rust string, so the
/// shim's JS escaper keeps that error path.)
pub fn query_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte < 0x80 && no_escape(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// querystring: decoder (internal/querystring.js `unescape`/`unescapeBuffer`)
// ---------------------------------------------------------------------------

/// `decodeURIComponent`: every `%` must be a complete escape and the decoded
/// bytes must be valid UTF-8. `None` is Node's "throws" signal.
fn strict_percent_decode(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = hex_value(bytes.next()?)?;
            let low = hex_value(bytes.next()?)?;
            out.push((high << 4) | low);
        } else {
            out.push(byte);
        }
    }
    String::from_utf8(out).ok()
}

/// Node's `unescapeBuffer` fallback: walks UTF-16 code units, decodes each
/// valid `%XX` into one byte and truncates every other code unit to its low
/// byte (`out[i] = charCode`), then UTF-8-decodes the buffer lossily.
fn unescape_buffer_bytes(text: &str, decode_spaces: bool) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out: Vec<u8> = Vec::with_capacity(units.len());
    let max_length = units.len().saturating_sub(2);
    let mut index = 0usize;
    while index < units.len() {
        let mut current = units[index];
        if current == u16::from(b'+') && decode_spaces {
            out.push(b' ');
            index += 1;
            continue;
        }
        if current == u16::from(b'%') && index < max_length {
            index += 1;
            current = units[index];
            let high = if current < 128 {
                hex_value(current as u8)
            } else {
                None
            };
            match high {
                None => {
                    out.push(b'%');
                    continue;
                }
                Some(high) => {
                    index += 1;
                    let next = units[index];
                    let low = if next < 128 {
                        hex_value(next as u8)
                    } else {
                        None
                    };
                    match low {
                        None => {
                            out.push(b'%');
                            index -= 1;
                        }
                        Some(low) => {
                            current = u16::from((high << 4) | low);
                        }
                    }
                }
            }
        }
        out.push((current & 0xff) as u8);
        index += 1;
    }
    // Node's `allocUnsafe(len)` buffer ends up with one byte per processed
    // code unit either way, so `slice(0, outIndex)` and the full buffer agree.
    out
}

/// `QueryString.unescape`: `decodeURIComponent` when the string is clean,
/// otherwise the byte-wise fallback. Never throws.
pub fn query_unescape(text: &str, decode_spaces: bool) -> String {
    match strict_percent_decode(text) {
        Some(decoded) => decoded,
        None => String::from_utf8_lossy(&unescape_buffer_bytes(text, decode_spaces)).into_owned(),
    }
}

/// `decodeStr` (parse's error-isolating wrapper): default decoder failure falls
/// back to `QueryString.unescape(s, true)`, which is the byte-wise path.
fn decode_query_str(text: &str) -> String {
    query_unescape(text, true)
}

fn is_hex(code: char) -> bool {
    code.is_ascii_hexdigit()
}

fn slice_chars(chars: &[char], start: usize, end: usize) -> String {
    chars[start.min(chars.len())..end.min(chars.len())]
        .iter()
        .collect()
}

// ---------------------------------------------------------------------------
// querystring.parse (Node v24 lib/querystring.js, default decoder)
// ---------------------------------------------------------------------------

fn add_key_val(
    out: &mut Vec<(String, String)>,
    key: &mut String,
    value: &mut String,
    key_encoded: bool,
    val_encoded: bool,
) {
    let key = if !key.is_empty() && key_encoded {
        decode_query_str(key)
    } else {
        std::mem::take(key)
    };
    let value = if !value.is_empty() && val_encoded {
        decode_query_str(value)
    } else {
        std::mem::take(value)
    };
    out.push((key, value));
}

/// The default-decoder `querystring.parse` state machine. `max_keys` is the
/// numeric `pairs` counter Node keeps (`options.maxKeys > 0 ? maxKeys : -1`,
/// as an `f64` so fractional values never hit the `=== 0` stop, exactly like
/// JavaScript). Returns the decoded pairs in order (duplicates preserved);
/// the JS shim builds the null-prototype object and its array-for-duplicates.
pub fn query_parse(text: &str, sep: &str, eq: &str, mut max_keys: f64) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if text.is_empty() {
        return out;
    }
    let sep_codes: Vec<char> = sep.chars().collect();
    let eq_codes: Vec<char> = eq.chars().collect();
    let sep_len = sep_codes.len();
    let eq_len = eq_codes.len();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    let mut last_pos = 0usize;
    let mut sep_idx = 0usize;
    let mut eq_idx = 0usize;
    let mut key = String::new();
    let mut value = String::new();
    let mut key_encoded = false;
    let mut val_encoded = false;
    let mut encode_check = 0u8;
    let mut index = 0usize;

    while index < len {
        let code = chars[index];

        // Try matching the key/value pair separator (e.g. '&').
        if sep_codes.get(sep_idx) == Some(&code) {
            sep_idx += 1;
            if sep_idx == sep_len {
                let end = index + 1 - sep_idx;
                if eq_idx < eq_len {
                    // No (complete) key/value separator: the substring is
                    // part of the key.
                    if last_pos < end {
                        key.push_str(&slice_chars(&chars, last_pos, end));
                    } else if key.is_empty() {
                        // An empty substring between separators still counts
                        // against `maxKeys`, as in Node.
                        max_keys -= 1.0;
                        if max_keys == 0.0 {
                            return out;
                        }
                        last_pos = index + 1;
                        sep_idx = 0;
                        eq_idx = 0;
                        index += 1;
                        continue;
                    }
                } else if last_pos < end {
                    value.push_str(&slice_chars(&chars, last_pos, end));
                }

                add_key_val(&mut out, &mut key, &mut value, key_encoded, val_encoded);

                max_keys -= 1.0;
                if max_keys == 0.0 {
                    return out;
                }
                key_encoded = false;
                val_encoded = false;
                key.clear();
                value.clear();
                encode_check = 0;
                last_pos = index + 1;
                sep_idx = 0;
                eq_idx = 0;
            }
        } else {
            sep_idx = 0;
            // Try matching the key/value separator (e.g. '=') until it has
            // matched in full.
            if eq_idx < eq_len {
                if eq_codes.get(eq_idx) == Some(&code) {
                    eq_idx += 1;
                    if eq_idx == eq_len {
                        let end = index + 1 - eq_idx;
                        if last_pos < end {
                            key.push_str(&slice_chars(&chars, last_pos, end));
                        }
                        encode_check = 0;
                        last_pos = index + 1;
                    }
                    index += 1;
                    continue;
                }
                eq_idx = 0;
                if !key_encoded {
                    // Notice one valid escape to avoid decoding needlessly.
                    if code == '%' {
                        encode_check = 1;
                        index += 1;
                        continue;
                    } else if encode_check > 0 {
                        if is_hex(code) {
                            encode_check += 1;
                            if encode_check == 3 {
                                key_encoded = true;
                            }
                            index += 1;
                            continue;
                        }
                        encode_check = 0;
                    }
                }
                if code == '+' {
                    if last_pos < index {
                        key.push_str(&slice_chars(&chars, last_pos, index));
                    }
                    key.push(' ');
                    last_pos = index + 1;
                    index += 1;
                    continue;
                }
            }
            if code == '+' {
                if last_pos < index {
                    value.push_str(&slice_chars(&chars, last_pos, index));
                }
                value.push(' ');
                last_pos = index + 1;
            } else if !val_encoded {
                if code == '%' {
                    encode_check = 1;
                } else if encode_check > 0 {
                    if is_hex(code) {
                        encode_check += 1;
                        if encode_check == 3 {
                            val_encoded = true;
                        }
                    } else {
                        encode_check = 0;
                    }
                }
            }
        }
        index += 1;
    }

    // Leftover key/value data.
    if last_pos < len {
        if eq_idx < eq_len {
            key.push_str(&slice_chars(&chars, last_pos, len));
        } else if sep_idx < sep_len {
            value.push_str(&slice_chars(&chars, last_pos, len));
        }
    } else if eq_idx == 0 && key.is_empty() {
        // Ended on an empty substring.
        return out;
    }

    add_key_val(&mut out, &mut key, &mut value, key_encoded, val_encoded);
    out
}

// ---------------------------------------------------------------------------
// querystring.stringify (Node v24 lib/querystring.js, default encoder)
// ---------------------------------------------------------------------------

/// Encode flat `[k, v, k, v, ...]` pairs (already stringified by the JS shim's
/// `encodeStringified`) with Node's escaper and join them. `None` for an odd
/// pair count (the shim never produces one).
pub fn query_stringify(pairs: &[String], sep: &str, eq: &str) -> Option<String> {
    if pairs.len() % 2 != 0 {
        return None;
    }
    let mut out = String::new();
    let mut first = true;
    for chunk in pairs.chunks_exact(2) {
        if !first {
            out.push_str(sep);
        }
        first = false;
        out.push_str(&query_escape(&chunk[0]));
        out.push_str(eq);
        out.push_str(&query_escape(&chunk[1]));
    }
    Some(out)
}

