//! Pure Codex quota derivation for the phone UI.
//!
//! The network request needs credentials and an HTTP client, so it stays
//! host-side. This module owns the response shaping: which rate-limit bucket
//! applies to a model, how one window is rendered, and how the "limited" flag
//! is derived. The host feeds it the decoded usage document plus a clock.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map as JsonMap, Value as JsonValue};

/// Model whose usage lives under `additional_rate_limits`.
pub const SPARK_MODEL_ID: &str = "gpt-5.3-codex-spark";
/// `limit_name` of the Spark bucket.
pub const SPARK_LIMIT_NAME: &str = "GPT-5.3-Codex-Spark";

/// The empty/unavailable quota document the PWA renders.
pub fn unavailable_quota() -> JsonValue {
    json!({
        "visible": false,
        "limited": false,
        "primaryWindow": JsonValue::Null,
        "secondaryWindow": JsonValue::Null,
    })
}

/// [`unavailable_quota`] with a diagnostic message attached.
pub fn unavailable_quota_with_error(error: impl Into<String>) -> JsonValue {
    let mut quota = unavailable_quota();
    if let Some(object) = quota.as_object_mut() {
        object.insert("error".to_owned(), JsonValue::String(error.into()));
    }
    quota
}

/// Whether the quota panel is shown for this provider/model pair.
pub fn quota_visible(provider: Option<&str>, model_id: Option<&str>) -> bool {
    provider == Some("openai-codex")
        && model_id
            .map(|id| id.to_ascii_lowercase().starts_with("gpt-"))
            .unwrap_or(false)
}

/// Derive the phone-facing quota document from a decoded Codex usage body.
pub fn quota_from_usage(usage: &JsonValue, model_id: &str, now_secs: f64) -> JsonValue {
    let bucket = select_rate_limit_bucket(usage, model_id);
    let primary = build_quota_window(
        "5h",
        bucket.and_then(|bucket| bucket.get("primary_window")),
        now_secs,
    );
    let secondary = build_quota_window(
        "7d",
        bucket.and_then(|bucket| bucket.get("secondary_window")),
        now_secs,
    );
    let limited = bucket
        .and_then(|bucket| bucket.get("limit_reached"))
        .and_then(JsonValue::as_bool)
        == Some(true)
        || bucket
            .and_then(|bucket| bucket.get("allowed"))
            .and_then(JsonValue::as_bool)
            == Some(false);
    json!({
        "visible": primary.is_some() || secondary.is_some(),
        "limited": limited,
        "primaryWindow": primary.unwrap_or(JsonValue::Null),
        "secondaryWindow": secondary.unwrap_or(JsonValue::Null),
    })
}

/// Wall clock in fractional epoch seconds (the `resetAfterSeconds` base).
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
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
        let entries: Vec<&JsonValue> = match additional {
            JsonValue::Array(array) => array.iter().collect(),
            JsonValue::Object(map) => map.values().collect(),
            _ => return None,
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

fn build_quota_window(label: &str, window: Option<&JsonValue>, now_secs: f64) -> Option<JsonValue> {
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
        "resetAfterSeconds": reset_seconds(window, now_secs),
        "text": format!("{}%", rounded_left as i64),
    }))
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn reset_seconds(window: Option<&JsonValue>, now_secs: f64) -> Option<f64> {
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
    Some((reset_at - now_secs).max(0.0))
}

/// Build an index of `sessionId -> lastActivityAt` from a catalog payload.
///
/// Hosts use it to decide which pool rows are still live without cloning the
/// whole catalog into the phone envelope.
pub fn catalog_activity_index(catalog: &JsonValue) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    let Some(rows) = catalog
        .get("data")
        .and_then(|data| data.get("sessions"))
        .and_then(JsonValue::as_array)
    else {
        return out;
    };
    for row in rows {
        let Some(id) = row.get("id").and_then(JsonValue::as_str) else {
            continue;
        };
        if id == "parent-session" {
            continue;
        }
        if let Some(activity) = row.get("lastActivityAt").and_then(JsonValue::as_i64) {
            out.insert(id.to_owned(), activity);
        }
    }
    out
}
