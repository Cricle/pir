//! Codex quota lookup used by the phone UI. The query runs in the local agent
//! process, which owns the provider credentials; the central relay only proxies
//! the result.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value as JsonValue;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MISSING_AUTH_PREFIX: &str = "Missing openai-codex OAuth access/accountId";

pub async fn quota_for_model(provider: Option<&str>, model_id: Option<&str>) -> JsonValue {
    let visible = provider == Some("openai-codex")
        && model_id
            .map(|id| id.to_ascii_lowercase().starts_with("gpt-"))
            .unwrap_or(false);
    if !visible {
        // The "unavailable" document shape is the PWA contract, owned by the
        // engine-free crate.
        return pir_builtin_phone::unavailable_quota();
    }

    match request_codex_usage(model_id.unwrap_or_default()).await {
        Ok(quota) => quota,
        Err(error) => pir_builtin_phone::unavailable_quota_with_error(error),
    }
}

/// Credential file the Codex quota route authenticates with
/// (`<agent-dir>/auth.json`, entry `openai-codex`).
pub fn quota_auth_path() -> PathBuf {
    crate::ai::providers::agent_dir().join("auth.json")
}

async fn request_codex_usage(model_id: &str) -> Result<JsonValue, String> {
    let auth_path = quota_auth_path();
    let auth_raw = tokio::fs::read_to_string(&auth_path)
        .await
        .map_err(|error| format!("{MISSING_AUTH_PREFIX} in {}: {error}", auth_path.display()))?;
    let auth: JsonValue = serde_json::from_str(&auth_raw)
        .map_err(|error| format!("Failed to parse {}: {error}", auth_path.display()))?;
    let entry = auth.get("openai-codex").and_then(JsonValue::as_object);
    let oauth =
        entry.filter(|entry| entry.get("type").and_then(JsonValue::as_str) == Some("oauth"));
    let access = oauth
        .and_then(|entry| entry.get("access"))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let account = oauth
        .and_then(|entry| entry.get("accountId").or_else(|| entry.get("account_id")))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (Some(access), Some(account)) = (access, account) else {
        return Err(format!("{MISSING_AUTH_PREFIX} in {}", auth_path.display()));
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("Failed to create HTTP client: {error}"))?;
    let response = client
        .get(CODEX_USAGE_URL)
        .header("accept", "*/*")
        .header("authorization", format!("Bearer {access}"))
        .header("chatgpt-account-id", account)
        .header("content-type", "application/json")
        .header("user-agent", "codex-cli")
        .send()
        .await
        .map_err(|error| format!("Codex usage request failed: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("Codex usage request failed ({})", status.as_u16()));
    }
    let usage: JsonValue = response
        .json()
        .await
        .map_err(|error| format!("Codex usage response was not JSON: {error}"))?;

    // Response shaping is pure (bucket selection, window rendering, the
    // "limited" flag) and lives in the engine-free `pir-builtin-phone` crate;
    // only the credentialed request above stays here.
    Ok(pir_builtin_phone::quota_from_usage(
        &usage,
        model_id,
        pir_builtin_phone::now_secs(),
    ))
}
