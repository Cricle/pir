//! Codex quota lookup used by the phone UI. The query runs in the local agent
//! process, which owns the provider credentials; the central relay only proxies
//! the result.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map as JsonMap, Value as JsonValue};

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const SPARK_MODEL_ID: &str = "gpt-5.3-codex-spark";
const SPARK_LIMIT_NAME: &str = "GPT-5.3-Codex-Spark";
const MISSING_AUTH_PREFIX: &str = "Missing openai-codex OAuth access/accountId";

pub(crate) async fn quota_for_model(provider: Option<&str>, model_id: Option<&str>) -> JsonValue {
    let visible = provider == Some("openai-codex")
        && model_id
            .map(|id| id.to_ascii_lowercase().starts_with("gpt-"))
            .unwrap_or(false);
    if !visible {
        return json!({
            "visible": false,
            "limited": false,
            "primaryWindow": JsonValue::Null,
            "secondaryWindow": JsonValue::Null,
        });
    }

    match request_codex_usage(model_id.unwrap_or_default()).await {
        Ok(quota) => quota,
        Err(error) => json!({
            "visible": false,
            "limited": false,
            "primaryWindow": JsonValue::Null,
            "secondaryWindow": JsonValue::Null,
            "error": error,
        }),
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

    let bucket = select_rate_limit_bucket(&usage, model_id);
    let primary = build_quota_window("5h", bucket.and_then(|bucket| bucket.get("primary_window")));
    let secondary = build_quota_window(
        "7d",
        bucket.and_then(|bucket| bucket.get("secondary_window")),
    );
    let limited = bucket
        .and_then(|bucket| bucket.get("limit_reached"))
        .and_then(JsonValue::as_bool)
        == Some(true)
        || bucket
            .and_then(|bucket| bucket.get("allowed"))
            .and_then(JsonValue::as_bool)
            == Some(false);

    Ok(json!({
        "visible": primary.is_some() || secondary.is_some(),
        "limited": limited,
        "primaryWindow": primary.unwrap_or(JsonValue::Null),
        "secondaryWindow": secondary.unwrap_or(JsonValue::Null),
    }))
}

fn normalize_rate_limit_bucket(value: &JsonValue) -> Option<&JsonMap<String, JsonValue>> {
    let record = value.as_object()?;
    let has_field = [
        "primary_window",
        "secondary_window",
        "limit_reached",
        "allowed",
    ]
    .iter()
    .any(|field| record.contains_key(*field));
    has_field.then_some(record)
}

fn select_rate_limit_bucket<'a>(
    usage: &'a JsonValue,
    model_id: &str,
) -> Option<&'a JsonMap<String, JsonValue>> {
    if model_id == SPARK_MODEL_ID {
        let additional = usage.get("additional_rate_limits")?;
        let entries: Vec<&JsonValue> = if let Some(array) = additional.as_array() {
            array.iter().collect()
        } else {
            additional.as_object()?.values().collect()
        };
        for entry in entries {
            let Some(record) = entry.as_object() else {
                continue;
            };
            if record
                .get("limit_name")
                .and_then(JsonValue::as_str)
                .map(str::trim)
                != Some(SPARK_LIMIT_NAME)
            {
                continue;
            }
            if let Some(bucket) = record
                .get("rate_limit")
                .and_then(normalize_rate_limit_bucket)
            {
                return Some(bucket);
            }
        }
        return None;
    }
    usage
        .get("rate_limit")
        .and_then(normalize_rate_limit_bucket)
}

fn build_quota_window(label: &str, window: Option<&JsonValue>) -> Option<JsonValue> {
    let used_percent = window
        .and_then(|window| window.get("used_percent"))
        .and_then(JsonValue::as_f64)
        .filter(|value| value.is_finite())?;
    let left_percent = clamp_percent(100.0 - used_percent);
    let rounded_left = left_percent.round();
    let rounded_used = clamp_percent(used_percent).round();
    Some(json!({
        "label": label,
        "leftPercent": rounded_left,
        "usedPercent": rounded_used,
        "resetAfterSeconds": get_reset_seconds(window),
        "text": format!("{}%", rounded_left as i64),
    }))
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn get_reset_seconds(window: Option<&JsonValue>) -> Option<f64> {
    let window = window?;
    if let Some(seconds) = window
        .get("reset_after_seconds")
        .and_then(JsonValue::as_f64)
    {
        return Some(seconds);
    }
    let reset_at = window.get("reset_at").and_then(JsonValue::as_f64)?;
    let reset_at = if reset_at > 100_000_000_000.0 {
        reset_at / 1000.0
    } else {
        reset_at
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    Some((reset_at - now).max(0.0))
}
