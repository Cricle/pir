//! Wire-format validation helpers and top-level message validation.

use serde_json::Value;

use super::error::SchemaError;
use super::rpc::{parse_rpc_target, parse_session_target};
use super::session::{validate_server_snapshot, validate_session_snapshot};
use super::transcript::validate_transcript_progress;
use super::PROTOCOL_VERSION;

// ---------------------------------------------------------------------------
// Wire-format validation helpers
// ---------------------------------------------------------------------------

/// Validate a value as a valid JSON value usable in protocol fields.
/// Rejects CBOR byte strings (which are not valid JSON values).
fn json_is_value(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(_) | Value::String(_) => true,
        Value::Number(_) => true,
        Value::Array(items) => items.iter().all(json_is_value),
        Value::Object(map) => map.values().all(json_is_value),
    }
}

pub(crate) fn require_string<'a>(v: &'a Value, path: &str) -> Result<&'a str, SchemaError> {
    match v {
        Value::String(s) => Ok(s.as_str()),
        _ => Err(SchemaError::new(format!("{path}: expected string"))),
    }
}

pub(crate) fn require_nonempty_string<'a>(
    v: &'a Value,
    path: &str,
) -> Result<&'a str, SchemaError> {
    let s = require_string(v, path)?;
    if s.is_empty() {
        Err(SchemaError::new(format!("{path}: must not be empty")))
    } else {
        Ok(s)
    }
}

pub(crate) fn require_id<'a>(v: &'a Value, path: &str) -> Result<&'a str, SchemaError> {
    require_nonempty_string(v, path)
}

pub(crate) fn require_integer(v: &Value, path: &str) -> Result<i64, SchemaError> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else {
                Err(SchemaError::new(format!("{path}: expected integer")))
            }
        }
        _ => Err(SchemaError::new(format!("{path}: expected integer"))),
    }
}

pub(crate) fn require_uint(v: &Value, path: &str) -> Result<i64, SchemaError> {
    let i = require_integer(v, path)?;
    if i < 0 {
        Err(SchemaError::new(format!("{path}: must be non-negative")))
    } else {
        Ok(i)
    }
}

pub(crate) fn require_bool(v: &Value, path: &str) -> Result<bool, SchemaError> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(SchemaError::new(format!("{path}: expected boolean"))),
    }
}

pub(crate) fn require_literal(v: &Value, expected: &str, path: &str) -> Result<(), SchemaError> {
    let s = require_string(v, path)?;
    if s != expected {
        Err(SchemaError::new(format!(
            "{path}: expected \"{expected}\", got \"{s}\""
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn require_string_literal<'a>(
    v: &'a Value,
    literals: &[&str],
    path: &str,
) -> Result<&'a str, SchemaError> {
    let s = require_string(v, path)?;
    if literals.contains(&s) {
        Ok(s)
    } else {
        Err(SchemaError::new(format!(
            "{path}: expected one of {literals:?}, got \"{s}\""
        )))
    }
}

pub(crate) fn require_array<'a>(v: &'a Value, path: &str) -> Result<&'a [Value], SchemaError> {
    match v {
        Value::Array(a) => Ok(a.as_slice()),
        _ => Err(SchemaError::new(format!("{path}: expected array"))),
    }
}

pub(crate) fn require_object<'a>(
    v: &'a Value,
    path: &str,
) -> Result<&'a serde_json::Map<String, Value>, SchemaError> {
    match v {
        Value::Object(o) => Ok(o),
        _ => Err(SchemaError::new(format!("{path}: expected object"))),
    }
}

/// Validate a strict object: reject unknown fields.
pub(crate) fn validate_strict_object(
    obj: &serde_json::Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), SchemaError> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(SchemaError::new(format!("{path}: unknown field \"{key}\"")));
        }
    }
    Ok(())
}

/// Validate a strict object with optional fields. Returns the validated object.
pub(crate) fn validate_strict<'a>(
    obj: &'a serde_json::Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<&'a serde_json::Map<String, Value>, SchemaError> {
    validate_strict_object(obj, allowed, path)?;
    Ok(obj)
}

/// Extract a required field from an object.
pub(crate) fn field<'a>(
    obj: &'a serde_json::Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<&'a Value, SchemaError> {
    obj.get(name)
        .ok_or_else(|| SchemaError::new(format!("{path}: missing required field \"{name}\"")))
}

/// Extract an optional field from an object.
pub(crate) fn field_opt<'a>(
    obj: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Option<&'a Value> {
    obj.get(name)
}

// ---------------------------------------------------------------------------
// Validation helpers for specific types
// ---------------------------------------------------------------------------

pub(crate) fn validate_json_value(v: &Value, path: &str) -> Result<(), SchemaError> {
    if !json_is_value(v) {
        return Err(SchemaError::new(format!("{path}: invalid JSON value")));
    }
    Ok(())
}

pub(crate) fn validate_model_ref(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["provider", "id"], path)?;
    require_nonempty_string(field(obj, "provider", path)?, &format!("{path}.provider"))?;
    require_nonempty_string(field(obj, "id", path)?, &format!("{path}.id"))?;
    Ok(())
}

pub(crate) fn validate_usage(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(
        obj,
        &[
            "input",
            "output",
            "cacheRead",
            "cacheWrite",
            "reasoning",
            "totalTokens",
            "cost",
        ],
        path,
    )?;
    require_uint(field(obj, "input", path)?, &format!("{path}.input"))?;
    require_uint(field(obj, "output", path)?, &format!("{path}.output"))?;
    require_uint(field(obj, "cacheRead", path)?, &format!("{path}.cacheRead"))?;
    require_uint(
        field(obj, "cacheWrite", path)?,
        &format!("{path}.cacheWrite"),
    )?;
    require_uint(
        field(obj, "totalTokens", path)?,
        &format!("{path}.totalTokens"),
    )?;
    if let Some(opt) = field_opt(obj, "reasoning") {
        require_uint(opt, &format!("{path}.reasoning"))?;
    }
    let cost = field(obj, "cost", path)?;
    let cost_obj = require_object(cost, &format!("{path}.cost"))?;
    validate_strict(
        cost_obj,
        &["input", "output", "cacheRead", "cacheWrite", "total"],
        &format!("{path}.cost"),
    )?;
    require_num_nonneg(
        field(cost_obj, "input", &format!("{path}.cost"))?,
        &format!("{path}.cost.input"),
    )?;
    require_num_nonneg(
        field(cost_obj, "output", &format!("{path}.cost"))?,
        &format!("{path}.cost.output"),
    )?;
    require_num_nonneg(
        field(cost_obj, "cacheRead", &format!("{path}.cost"))?,
        &format!("{path}.cost.cacheRead"),
    )?;
    require_num_nonneg(
        field(cost_obj, "cacheWrite", &format!("{path}.cost"))?,
        &format!("{path}.cost.cacheWrite"),
    )?;
    require_num_nonneg(
        field(cost_obj, "total", &format!("{path}.cost"))?,
        &format!("{path}.cost.total"),
    )?;
    Ok(())
}

pub(crate) fn require_num_nonneg(v: &Value, path: &str) -> Result<(), SchemaError> {
    match v {
        Value::Number(n) => {
            let negative = n.as_f64().is_some_and(|f| f < 0.0) || n.as_i64().is_some_and(|i| i < 0);
            if negative {
                Err(SchemaError::new(format!("{path}: must be non-negative")))
            } else {
                Ok(())
            }
        }
        _ => Err(SchemaError::new(format!("{path}: expected number"))),
    }
}

// ---------------------------------------------------------------------------
// Protocol error
// ---------------------------------------------------------------------------

fn validate_protocol_error(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    // Upstream `ProtocolErrorSchema` is a StrictObject of `code` (IdSchema:
    // any non-empty string) and `message`; `details` is not part of the wire
    // schema and is rejected.
    validate_strict(obj, &["code", "message"], path)?;
    require_nonempty_string(field(obj, "code", path)?, &format!("{path}.code"))?;
    require_string(field(obj, "message", path)?, &format!("{path}.message"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn validate_command(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let cmd = require_string(field(obj, "command", path)?, &format!("{path}.command"))?;
    match cmd {
        "list" => {
            validate_strict(obj, &["command"], path)?;
        }
        "create" => {
            validate_strict(
                obj,
                &["command", "cwd", "name", "model", "thinkingLevel"],
                path,
            )?;
            if let Some(opt) = field_opt(obj, "cwd") {
                require_nonempty_string(opt, &format!("{path}.cwd"))?;
            }
            if let Some(opt) = field_opt(obj, "name") {
                require_string(opt, &format!("{path}.name"))?;
            }
            if let Some(opt) = field_opt(obj, "model") {
                validate_model_ref(opt, &format!("{path}.model"))?;
            }
            if let Some(opt) = field_opt(obj, "thinkingLevel") {
                require_string_literal(
                    opt,
                    &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                    &format!("{path}.thinkingLevel"),
                )?;
            }
        }
        "attach" => {
            validate_strict(obj, &["command", "sessionId"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
        }
        "detach" => {
            validate_strict(obj, &["command", "sessionId"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
        }
        "prompt" => {
            validate_strict(obj, &["command", "sessionId", "text"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
            require_string(field(obj, "text", path)?, &format!("{path}.text"))?;
        }
        "steer" => {
            validate_strict(obj, &["command", "sessionId", "text"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
            require_string(field(obj, "text", path)?, &format!("{path}.text"))?;
        }
        "abort" => {
            validate_strict(obj, &["command", "sessionId"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
        }
        "set_model" => {
            validate_strict(obj, &["command", "sessionId", "model"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
            validate_model_ref(field(obj, "model", path)?, &format!("{path}.model"))?;
        }
        "set_thinking" => {
            validate_strict(obj, &["command", "sessionId", "thinkingLevel"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
            require_string_literal(
                field(obj, "thinkingLevel", path)?,
                &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                &format!("{path}.thinkingLevel"),
            )?;
        }
        _ => {
            return Err(SchemaError::new(format!(
                "{path}.command: unknown command \"{cmd}\""
            )))
        }
    }
    Ok(())
}

fn validate_server_event(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let t = require_string(field(obj, "type", path)?, &format!("{path}.type"))?;
    match t {
        "server_snapshot" => {
            validate_strict(obj, &["type", "snapshot"], path)?;
            validate_server_snapshot(field(obj, "snapshot", path)?, &format!("{path}.snapshot"))?;
        }
        "session_snapshot" => {
            validate_strict(obj, &["type", "snapshot"], path)?;
            validate_session_snapshot(field(obj, "snapshot", path)?, &format!("{path}.snapshot"))?;
        }
        "session_progress" => {
            validate_strict(obj, &["type", "sessionId", "progress"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
            validate_transcript_progress(field(obj, "progress", path)?, &format!("{path}.progress"))?;
        }
        "session_removed" => {
            validate_strict(obj, &["type", "sessionId"], path)?;
            require_id(field(obj, "sessionId", path)?, &format!("{path}.sessionId"))?;
        }
        _ => return Err(SchemaError::new(format!("{path}.type: expected server_snapshot/session_snapshot/session_progress/session_removed"))),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level message validation
// ---------------------------------------------------------------------------

/// Validate a client message value against the protocol v8 schema.
///
/// The v8 envelope (`hello` / `request {id, target, call}` / `cancel`) is the
/// live protocol; the concrete v1 `request {id, request}` shape is still
/// accepted for the stdio RPC transition.
pub fn parse_client_message(value: &Value) -> Result<(), SchemaError> {
    let obj = require_object(value, "")?;
    let t = require_string(field(obj, "type", "")?, ".type")?;
    match t {
        "hello" => {
            validate_strict(obj, &["type", "version"], "")?;
            require_uint(field(obj, "version", "")?, ".version")?;
            Ok(())
        }
        "request" => {
            if obj.contains_key("target") {
                // v8 generic RPC envelope.
                validate_strict(obj, &["type", "id", "target", "call"], "")?;
                require_id(field(obj, "id", "")?, ".id")?;
                parse_rpc_target(field(obj, "target", "")?, ".target")?;
                validate_json_value(field(obj, "call", "")?, ".call")?;
                Ok(())
            } else {
                // Legacy concrete request catalog.
                validate_strict(obj, &["type", "id", "request"], "")?;
                require_id(field(obj, "id", "")?, ".id")?;
                validate_command(field(obj, "request", "")?, ".request")?;
                Ok(())
            }
        }
        "cancel" => {
            validate_strict(obj, &["type", "id", "target"], "")?;
            require_id(field(obj, "id", "")?, ".id")?;
            parse_rpc_target(field(obj, "target", "")?, ".target")?;
            Ok(())
        }
        _ => Err(SchemaError::new(
            "client message: expected type \"hello\", \"request\", or \"cancel\"",
        )),
    }
}

/// Validate a server message value against the protocol v8 schema.
///
/// The v8 envelope (`hello {version, serverId}` / `hello_error` /
/// `response` / `service_update` / `attachment`) is the live protocol; the
/// concrete v1 shapes (`hello {connectionId, snapshot}`, `event`, command
/// results) are still accepted for the stdio RPC transition.
pub fn parse_server_message(value: &Value) -> Result<(), SchemaError> {
    let obj = require_object(value, "")?;
    let t = require_string(field(obj, "type", "")?, ".type")?;
    match t {
        "hello" => {
            if obj.contains_key("serverId") {
                // v8 server hello.
                validate_strict(obj, &["type", "version", "serverId"], "")?;
                let version = require_uint(field(obj, "version", "")?, ".version")?;
                if version != PROTOCOL_VERSION {
                    return Err(SchemaError::new(format!(
                        ".version: expected {PROTOCOL_VERSION}, got {version}"
                    )));
                }
                require_id(field(obj, "serverId", "")?, ".serverId")?;
                Ok(())
            } else {
                // Legacy server hello (stdio RPC transition).
                validate_strict(obj, &["type", "version", "connectionId", "snapshot"], "")?;
                let version = require_uint(field(obj, "version", "")?, ".version")?;
                if version != PROTOCOL_VERSION {
                    return Err(SchemaError::new(format!(
                        ".version: expected {PROTOCOL_VERSION}, got {version}"
                    )));
                }
                require_id(field(obj, "connectionId", "")?, ".connectionId")?;
                validate_server_snapshot(field(obj, "snapshot", "")?, ".snapshot")?;
                Ok(())
            }
        }
        "hello_error" => {
            validate_strict(obj, &["type", "error"], "")?;
            validate_protocol_error(field(obj, "error", "")?, ".error")?;
            Ok(())
        }
        "response" => {
            // v8 response envelope; the legacy command-result `result` values
            // are opaque JSON values and therefore accepted unchanged.
            validate_strict(obj, &["type", "id", "ok", "result", "error"], "")?;
            require_id(field(obj, "id", "")?, ".id")?;
            let ok = require_bool(field(obj, "ok", "")?, ".ok")?;
            if ok {
                if field_opt(obj, "error").is_some() {
                    return Err(SchemaError::new(
                        ".response: ok=true must not have error field",
                    ));
                }
                if let Some(result) = field_opt(obj, "result") {
                    validate_json_value(result, ".result")?;
                }
            } else {
                if field_opt(obj, "result").is_some() {
                    return Err(SchemaError::new(
                        ".response: ok=false must not have result field",
                    ));
                }
                validate_protocol_error(field(obj, "error", "")?, ".error")?;
            }
            Ok(())
        }
        "service_update" => {
            validate_strict(obj, &["type", "subscriptionId", "update"], "")?;
            require_id(field(obj, "subscriptionId", "")?, ".subscriptionId")?;
            validate_json_value(field(obj, "update", "")?, ".update")?;
            Ok(())
        }
        "attachment" => {
            validate_strict(obj, &["type", "attachment"], "")?;
            let attachment = field(obj, "attachment", "")?;
            if !attachment.is_null() {
                parse_session_target(attachment, ".attachment")?;
            }
            Ok(())
        }
        "event" => {
            validate_strict(obj, &["type", "event"], "")?;
            validate_server_event(field(obj, "event", "")?, ".event")?;
            Ok(())
        }
        _ => Err(SchemaError::new(
            "server message: expected type \"hello\"/\"hello_error\"/\"response\"/\"service_update\"/\"attachment\"",
        )),
    }
}
