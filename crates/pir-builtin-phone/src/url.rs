//! Phone server URL normalization.
//!
//! The central relay accepts the WebSocket endpoint only. Users paste an
//! `http(s)://` origin or omit the trailing `/agent` path, so the raw value is
//! normalized before it is handed to the WebSocket client.

/// Normalize a user-supplied server URL to the relay's agent endpoint.
///
/// `http(s)://` is upgraded to `ws(s)://`, trailing slashes are dropped, and
/// `/agent` is appended unless the URL already ends with it.
pub fn normalize_url(raw: &str) -> String {
    let mut url = raw.trim().to_owned();
    if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    } else if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    }
    let url = url.trim_end_matches('/').to_owned();
    if url.ends_with("/agent") {
        url
    } else {
        format!("{url}/agent")
    }
}

/// Whether a positional argument looks like a phone server URL.
pub fn is_server_url(value: &str) -> bool {
    value.starts_with("ws://")
        || value.starts_with("wss://")
        || value.starts_with("http://")
        || value.starts_with("https://")
}
