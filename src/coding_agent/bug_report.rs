//! Bug-report bundle assembly + Radius upload — port of upstream
//! `core/bug-report.ts` + `core/bug-report-upload.ts` (the `/bug` flow's
//! non-TUI half; the consent dialogs live in `tui/terminal.rs`).
//!
//! Upstream assembles `report.json` (redacted metadata) +
//! `diagnostics.json` + optional `session.jsonl` + `summary.md`, then either
//! uploads them as multipart form data to the Radius gateway
//! (`POST /v1/bug-reports`, anonymous or Bearer-attributed) or packs them
//! into a zip. pir keeps the same file set; the local fallback (the
//! markdown bundle `tui/slash.rs` writes) stays for offline / no-Radius
//! runs.

use std::collections::BTreeMap;
use std::time::Duration;

/// `BUG_REPORT_SCHEMA_VERSION` (upstream core/bug-report.ts:18).
pub const BUG_REPORT_SCHEMA_VERSION: u32 = 1;

/// `REDACTED` (upstream core/bug-report.ts:19).
pub const REDACTED: &str = "<redacted>";

/// The `pi.bug-report` custom entry type (upstream core/bug-report.ts:17).
pub const BUG_REPORT_CUSTOM_ENTRY_TYPE: &str = "pi.bug-report";

/// Keys whose values never leave the machine (upstream `SENSITIVE_KEY`,
/// core/bug-report.ts:20 — the case-insensitive regex
/// `api[-_]key|secret|token|password|passwd|credential|authorization|cookie`).
/// Public for the display-layer tests.
pub fn is_sensitive_key(key: &str) -> bool {
    // Normalize camelCase to snake_case the way upstream's
    // `key.replace(/([a-z0-9])([A-Z])/g, "$1_$2")` does.
    let mut normalized = String::with_capacity(key.len() + 4);
    for (index, ch) in key.char_indices() {
        if ch.is_ascii_uppercase() {
            let prev_is_alnum = key[..index]
                .chars()
                .next_back()
                .is_some_and(|previous| previous.is_ascii_alphanumeric());
            if prev_is_alnum {
                normalized.push('_');
            }
            normalized.extend(ch.to_lowercase());
        } else {
            normalized.push(ch);
        }
    }
    let segments: Vec<String> = normalized
        .split(['-', '_'])
        .map(|segment| segment.trim().replace([' ', '-'], "").to_ascii_lowercase())
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.iter().any(|segment| {
        matches!(
            segment.as_str(),
            "secret" | "token" | "password" | "passwd" | "credential" | "authorization" | "cookie"
        )
    }) {
        return true;
    }
    // `api[-_]key`: adjacent `api`/`key` segments (api_key, api-key) or the
    // folded whole (apiKey normalizes to api_key; a joined `apikey` spelling
    // still redacts conservatively).
    segments.windows(2).any(|window| window == ["api", "key"]) || segments.concat() == "apikey"
}

/// Strip credentials and secret-looking query parameters from a URL
/// (upstream `redactUrl`, core/bug-report.ts:27-48). pir redacts with a
/// conservative string pass: split off the query, blank userinfo, and
/// replace every sensitive query VALUE (the parameter NAMES are kept so the
/// report still shows what was configured).
pub fn redact_url(value: &str) -> String {
    let (head, query) = match value.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (value, None),
    };
    // Userinfo credentials (`scheme://user:pass@host`).
    let head = match head.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_userinfo, host)) => format!("{scheme}://{host}"),
            None => head.to_string(),
        },
        None => head.to_string(),
    };
    let Some(query) = query else {
        return head;
    };
    let redacted: Vec<String> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_sensitive_key(key) => format!("{key}={REDACTED}"),
            Some((key, value)) => format!("{key}={value}"),
            None => String::new(),
        })
        .collect();
    if redacted.is_empty() {
        head
    } else {
        format!("{head}?{}", redacted.join("&"))
    }
}

/// Copy a JSON value while redacting sensitive keys and credential-bearing
/// URLs (upstream `redactJsonValue`, core/bug-report.ts:51-59).
pub fn redact_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, child)| {
                    if is_sensitive_key(key) {
                        (key.clone(), serde_json::Value::String(REDACTED.to_string()))
                    } else {
                        (key.clone(), redact_json_value(child))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(list) => {
            serde_json::Value::Array(list.iter().map(redact_json_value).collect())
        }
        serde_json::Value::String(text) => serde_json::Value::String(redact_url(text)),
        other => other.clone(),
    }
}

/// One bundle file (upstream `BugReportFile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugReportFile {
    pub name: String,
    pub content_type: String,
    pub data: String,
}

/// The bundle pir can collect without the LLM summarization leg (upstream
/// `BugReportBundle`): `report.json` + `diagnostics.json` + optional
/// `session.jsonl` + optional `summary.md`.
#[derive(Debug, Clone)]
pub struct BugReportBundle {
    pub metadata: serde_json::Value,
    /// Failed assistant turns + crashes without conversation content
    /// (upstream `collectBugReportDiagnostics`).
    pub diagnostics: serde_json::Value,
    pub session_jsonl: Option<String>,
    /// The user's `/bug <hint>` text (the local summary section).
    pub hint: Option<String>,
    /// The markdown summary written when the transcript is NOT shared.
    /// Upstream generates it with the session model
    /// (`generateBugReportSummary`, bug-report.ts:330-375); pir has no
    /// consent-to-share-transcript LLM flow, so the user's hint stands in
    /// verbatim and the file is only attached when a hint exists.
    pub summary: Option<String>,
}

/// Files shared by upload and export (upstream `bugReportFiles`,
/// core/bug-report.ts:252-272): report.json, diagnostics.json, then the
/// optional session.jsonl and summary.md.
pub fn bug_report_files(bundle: &BugReportBundle) -> Vec<BugReportFile> {
    let mut files = vec![
        BugReportFile {
            name: "report.json".to_string(),
            content_type: "application/json".to_string(),
            data: format!("{}\n", bundle.metadata),
        },
        BugReportFile {
            name: "diagnostics.json".to_string(),
            content_type: "application/json".to_string(),
            data: format!("{}\n", bundle.diagnostics),
        },
    ];
    if let Some(session_jsonl) = &bundle.session_jsonl {
        files.push(BugReportFile {
            name: "session.jsonl".to_string(),
            content_type: "application/x-ndjson".to_string(),
            data: session_jsonl.clone(),
        });
    }
    if let Some(summary) = &bundle.summary {
        files.push(BugReportFile {
            name: "summary.md".to_string(),
            content_type: "text/markdown".to_string(),
            data: format!("{summary}\n"),
        });
    }
    files
}

/// The bundle metadata pir collects (upstream `collectBugReportMetadata`
/// reduced to pir's surface: environment, session identity, model,
/// provider auth status, extension paths — settings redacted upstream are
/// not included because pir's Settings carry no secret-shaped fields beyond
/// the redaction pass).
pub struct BugReportInputs {
    pub pir_version: String,
    pub os: String,
    pub arch: String,
    pub cwd: String,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
    pub include_session: bool,
    pub message_count: usize,
    pub model_provider: String,
    pub model_id: String,
    pub thinking_level: String,
    pub extension_paths: Vec<String>,
    pub hint: Option<String>,
}

/// Assemble `report.json` (upstream `collectBugReportMetadata` shape,
/// camelCase).
pub fn collect_bug_report_metadata(inputs: &BugReportInputs) -> serde_json::Value {
    let pi_env_names: BTreeMap<String, serde_json::Value> = std::env::vars()
        .filter(|(name, _)| name.starts_with("PI_"))
        .map(|(name, _)| (name, serde_json::Value::Null))
        .collect();
    serde_json::json!({
        "schemaVersion": BUG_REPORT_SCHEMA_VERSION,
        "id": uuid::Uuid::new_v4().to_string(),
        "createdAt": crate::session_manager::types::iso_from_ms(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
        ),
        "hint": inputs.hint,
        "environment": {
            "version": inputs.pir_version,
            "runtime": "rust".to_string(),
            "platform": inputs.os,
            "arch": inputs.arch,
            // Names help diagnose configuration; values never leave the
            // machine (upstream `piEnvironmentVariables`).
            "piEnvironmentVariables": pi_env_names.keys().cloned().collect::<Vec<_>>(),
        },
        "session": {
            "id": inputs.session_id,
            "path": inputs.session_path,
            "included": inputs.include_session,
            "messageCount": inputs.message_count,
        },
        "model": {
            "provider": inputs.model_provider,
            "id": inputs.model_id,
        },
        "thinkingLevel": inputs.thinking_level,
        "extensions": inputs.extension_paths.iter().map(|path| serde_json::json!({
            "path": path,
        })).collect::<Vec<_>>(),
    })
}

/// Collect failed assistant turns + crashes WITHOUT conversation content
/// (upstream `collectBugReportDiagnostics`, core/bug-report.ts:183-223): an
/// assistant entry lands when it carries diagnostics, a `error`/`aborted`
/// stop reason, or an error message. `crashes` are the caller's pre-serialized
/// crash records (the `notified` flag already stripped).
pub fn collect_bug_report_diagnostics(
    entries: &[crate::session_manager::types::FileEntry],
    session_id: Option<&str>,
    crashes: &[serde_json::Value],
) -> serde_json::Value {
    use crate::ai::types::StopReason;
    use crate::session_manager::types::{FileEntry, StoredMessage};
    let mut assistant: Vec<serde_json::Value> = Vec::new();
    let mut assistant_message_count = 0usize;
    for entry in entries {
        let FileEntry::Message(entry) = entry else {
            continue;
        };
        let StoredMessage::Assistant(message) = &entry.message else {
            continue;
        };
        assistant_message_count += 1;
        if message.diagnostics.is_empty()
            && message.stop_reason != StopReason::Error
            && message.stop_reason != StopReason::Aborted
            && message.error_message.is_none()
        {
            continue;
        }
        let mut record = serde_json::json!({
            "entryId": entry.id,
            "timestamp": entry.timestamp,
            "provider": message.provider,
            "model": message.model,
            "api": message.api,
            "stopReason": message.stop_reason,
            "diagnostics": message.diagnostics,
        });
        if let Some(object) = record.as_object_mut() {
            if let Some(raw) = &message.raw_stop_reason {
                object.insert("rawStopReason".to_string(), serde_json::json!(raw));
            }
            if let Some(error) = &message.error_message {
                object.insert("errorMessage".to_string(), serde_json::json!(error));
            }
        }
        assistant.push(record);
    }
    serde_json::json!({
        "schemaVersion": BUG_REPORT_SCHEMA_VERSION,
        "sessionId": session_id,
        "entryCount": entries.len(),
        "assistantMessageCount": assistant_message_count,
        "assistant": assistant,
        "crashes": crashes,
    })
}

/// The gateway base URL for the upload (upstream `getRadiusGatewayUrl`).
fn gateway_url() -> String {
    crate::ai::auth::oauth::radius::radius_gateway_url()
}

/// Whether a Radius credential is available for an attributed upload
/// (upstream `getAuthCredential(modelRuntime.getAuth("radius"))`); the
/// upload itself also works anonymously.
fn radius_token() -> Option<String> {
    let storage = crate::ai::auth::FileAuthStorage::default_storage();
    let credential = storage.read_stored("radius").ok().flatten()?;
    match credential {
        crate::ai::auth::StoredCredential::OAuth(oauth) => Some(oauth.access),
        crate::ai::auth::StoredCredential::ApiKey(api_key) => api_key.key,
    }
}

/// Upload the bundle as multipart form data to the Radius bug-report
/// endpoint (upstream `uploadBugReport`, bug-report-upload.ts:12-37).
/// Returns the created report id; a missing Radius credential is the
/// caller's fallback signal (`Ok(None)`).
pub async fn upload_bug_report(bundle: &BugReportBundle) -> Result<Option<String>, String> {
    let token = radius_token();
    // Hand-rolled multipart/form-data body (the shared reqwest feature set
    // has no `multipart`); the boundary is random per request.
    let boundary = uuid::Uuid::new_v4().simple().to_string();
    let mut body = Vec::new();
    for file in bug_report_files(bundle) {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                file.name, file.name
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", file.content_type).as_bytes());
        body.extend_from_slice(file.data.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    let endpoint = format!("{}/v1/bug-reports", gateway_url().trim_end_matches('/'));
    let mut request = client
        .post(&endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body);
    if let Some(token) = &token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status();
    let json: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if status.is_success() && json.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        if let Some(id) = json
            .pointer("/bug_report/id")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(Some(id.to_string()));
        }
    }
    let detail = json
        .get("description")
        .or_else(|| json.get("error"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("unknown"));
    Err(format!("Bug report upload failed: {detail}"))
}
