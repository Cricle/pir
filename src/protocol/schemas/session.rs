//! Session, model, and server snapshot validation; v8 session service update builders.

use serde_json::Value;

use super::error::SchemaError;
use super::server::TranscriptProgress;
use super::transcript::{validate_transcript_item_array, validate_user_transcript_item_array};
use super::types::SessionSnapshot;
use super::validate::{
    field, field_opt, require_array, require_bool, require_id, require_nonempty_string,
    require_num_nonneg, require_object, require_string, require_string_literal, require_uint,
    validate_model_ref, validate_strict,
};
use super::PROTOCOL_VERSION;

// ---------------------------------------------------------------------------
// Session types
// ---------------------------------------------------------------------------

fn validate_session_metadata(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(
        obj,
        &[
            "id",
            "createdAt",
            "updatedAt",
            "parentSessionId",
            "sessionName",
            "cwd",
        ],
        path,
    )?;
    require_id(field(obj, "id", path)?, &format!("{path}.id"))?;
    require_uint(field(obj, "createdAt", path)?, &format!("{path}.createdAt"))?;
    if let Some(opt) = field_opt(obj, "updatedAt") {
        require_uint(opt, &format!("{path}.updatedAt"))?;
    }
    if let Some(opt) = field_opt(obj, "parentSessionId") {
        require_id(opt, &format!("{path}.parentSessionId"))?;
    }
    // sessionName: optional string, any value
    // cwd: optional string >= 1 char
    if let Some(opt) = field_opt(obj, "cwd") {
        require_nonempty_string(opt, &format!("{path}.cwd"))?;
    }
    Ok(())
}

pub(crate) fn validate_session_snapshot(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(
        obj,
        &[
            "id",
            "name",
            "cwd",
            "createdAt",
            "updatedAt",
            "phase",
            "model",
            "thinkingLevel",
            "attached",
            "locked",
            "revision",
            "transcript",
            "queuedSteer",
            "queuedSteerCount",
        ],
        path,
    )?;
    require_id(field(obj, "id", path)?, &format!("{path}.id"))?;
    if let Some(opt) = field_opt(obj, "name") {
        require_string(opt, &format!("{path}.name"))?;
    }
    require_nonempty_string(field(obj, "cwd", path)?, &format!("{path}.cwd"))?;
    require_uint(field(obj, "createdAt", path)?, &format!("{path}.createdAt"))?;
    require_uint(field(obj, "updatedAt", path)?, &format!("{path}.updatedAt"))?;
    require_string_literal(
        field(obj, "phase", path)?,
        &["idle", "turn", "compaction", "branch_summary", "retry"],
        &format!("{path}.phase"),
    )?;
    validate_model_ref(field(obj, "model", path)?, &format!("{path}.model"))?;
    require_string_literal(
        field(obj, "thinkingLevel", path)?,
        &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
        &format!("{path}.thinkingLevel"),
    )?;
    require_bool(field(obj, "attached", path)?, &format!("{path}.attached"))?;
    require_bool(field(obj, "locked", path)?, &format!("{path}.locked"))?;
    require_uint(field(obj, "revision", path)?, &format!("{path}.revision"))?;
    validate_transcript_item_array(
        field(obj, "transcript", path)?,
        &format!("{path}.transcript"),
    )?;
    validate_user_transcript_item_array(
        field(obj, "queuedSteer", path)?,
        &format!("{path}.queuedSteer"),
    )?;
    require_uint(
        field(obj, "queuedSteerCount", path)?,
        &format!("{path}.queuedSteerCount"),
    )?;
    Ok(())
}

fn validate_model_cost(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["input", "output", "cacheRead", "cacheWrite"], path)?;
    require_num_nonneg(field(obj, "input", path)?, &format!("{path}.input"))?;
    require_num_nonneg(field(obj, "output", path)?, &format!("{path}.output"))?;
    require_num_nonneg(field(obj, "cacheRead", path)?, &format!("{path}.cacheRead"))?;
    require_num_nonneg(
        field(obj, "cacheWrite", path)?,
        &format!("{path}.cacheWrite"),
    )?;
    Ok(())
}

fn validate_model_metadata(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(
        obj,
        &[
            "provider",
            "id",
            "name",
            "api",
            "reasoning",
            "input",
            "contextWindow",
            "maxTokens",
            "cost",
            "supportedThinkingLevels",
            "authenticated",
        ],
        path,
    )?;
    require_nonempty_string(field(obj, "provider", path)?, &format!("{path}.provider"))?;
    require_nonempty_string(field(obj, "id", path)?, &format!("{path}.id"))?;
    require_nonempty_string(field(obj, "name", path)?, &format!("{path}.name"))?;
    require_nonempty_string(field(obj, "api", path)?, &format!("{path}.api"))?;
    require_bool(field(obj, "reasoning", path)?, &format!("{path}.reasoning"))?;
    let input_arr = require_array(field(obj, "input", path)?, &format!("{path}.input"))?;
    for (i, item) in input_arr.iter().enumerate() {
        require_string_literal(item, &["text", "image"], &format!("{path}.input[{i}]"))?;
    }
    require_uint(
        field(obj, "contextWindow", path)?,
        &format!("{path}.contextWindow"),
    )?;
    require_uint(field(obj, "maxTokens", path)?, &format!("{path}.maxTokens"))?;
    validate_model_cost(field(obj, "cost", path)?, &format!("{path}.cost"))?;
    let thinking_arr = require_array(
        field(obj, "supportedThinkingLevels", path)?,
        &format!("{path}.supportedThinkingLevels"),
    )?;
    if thinking_arr.is_empty() {
        return Err(SchemaError::new(format!(
            "{path}.supportedThinkingLevels: must have at least one item"
        )));
    }
    for (i, item) in thinking_arr.iter().enumerate() {
        require_string_literal(
            item,
            &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
            &format!("{path}.supportedThinkingLevels[{i}]"),
        )?;
    }
    require_bool(
        field(obj, "authenticated", path)?,
        &format!("{path}.authenticated"),
    )?;
    Ok(())
}

pub(crate) fn validate_server_snapshot(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(
        obj,
        &[
            "serverId",
            "protocolVersion",
            "revision",
            "sessions",
            "models",
        ],
        path,
    )?;
    require_id(field(obj, "serverId", path)?, &format!("{path}.serverId"))?;
    let version = require_uint(
        field(obj, "protocolVersion", path)?,
        &format!("{path}.protocolVersion"),
    )?;
    if version != PROTOCOL_VERSION {
        return Err(SchemaError::new(format!(
            "{path}.protocolVersion: expected {PROTOCOL_VERSION}, got {version}"
        )));
    }
    require_uint(field(obj, "revision", path)?, &format!("{path}.revision"))?;
    let sessions = require_array(field(obj, "sessions", path)?, &format!("{path}.sessions"))?;
    for (i, item) in sessions.iter().enumerate() {
        validate_session_metadata(item, &format!("{path}.sessions[{i}]"))?;
    }
    let models = require_array(field(obj, "models", path)?, &format!("{path}.models"))?;
    for (i, item) in models.iter().enumerate() {
        validate_model_metadata(item, &format!("{path}.models[{i}]"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Protocol v8: session service update builders
// ---------------------------------------------------------------------------

/// The wire vocabulary of a pir.session `state` update.
///
/// Both vocabularies are valid v8 `state` payloads and both are consumed by
/// [`StateUpdateDecoder`](crate::protocol::StateUpdateDecoder) /
/// [`parse_state_update`](crate::protocol::parse_state_update); the delta form
/// is the upstream chord vocabulary, the legacy form is pir's historical
/// `{op, value}` object shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateUpdateFormat {
    /// Chord delta tuples, e.g. `["r", <value>]` (the default).
    #[default]
    Delta,
    /// The legacy `{"op": ..., "value": ...}` object vocabulary.
    Legacy,
}

impl StateUpdateFormat {
    /// Stable name for logs/diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            StateUpdateFormat::Delta => "delta",
            StateUpdateFormat::Legacy => "legacy",
        }
    }
}

/// Build the pir.session `state` update carrying one transcript progress event.
///
/// Shape: `{"type":"state","member":"progress","sequence":n,"instance":{key,generation},"ops":[{"op":"append","value":<TranscriptProgress>}]}`.
pub fn session_progress_update(
    session_id: &str,
    progress: &TranscriptProgress,
    sequence: i64,
) -> Value {
    serde_json::json!({
        "type": "state",
        "member": "progress",
        "sequence": sequence,
        "instance": {"key": session_id, "generation": 1},
        "ops": [{"op": "append", "value": serde_json::to_value(progress).unwrap_or(Value::Null)}],
    })
}

/// [`session_progress_update`] in the chord delta vocabulary: the payload is
/// the base op `["r", <TranscriptProgress>]`, which an upstream chord client
/// decodes with its delta decoder.
pub fn session_progress_delta_update(
    session_id: &str,
    progress: &TranscriptProgress,
    sequence: i64,
) -> Value {
    session_state_delta_update(
        session_id,
        "progress",
        sequence,
        &[serde_json::json!([
            "r",
            serde_json::to_value(progress).unwrap_or(Value::Null)
        ])],
    )
}

/// Build the pir.session `state` update carrying a full session snapshot.
///
/// Shape: `{"type":"state","member":"snapshot","sequence":n,"instance":{key,generation},"ops":[{"op":"set","value":<SessionSnapshot>}]}`.
pub fn session_snapshot_update(
    session_id: &str,
    snapshot: &SessionSnapshot,
    sequence: i64,
) -> Value {
    serde_json::json!({
        "type": "state",
        "member": "snapshot",
        "sequence": sequence,
        "instance": {"key": session_id, "generation": 1},
        "ops": [{"op": "set", "value": serde_json::to_value(snapshot).unwrap_or(Value::Null)}],
    })
}

/// [`session_snapshot_update`] in the chord delta vocabulary: the full
/// snapshot is a base replacement (`["r", <SessionSnapshot>]`).
pub fn session_snapshot_delta_update(
    session_id: &str,
    snapshot: &SessionSnapshot,
    sequence: i64,
) -> Value {
    session_state_delta_update(
        session_id,
        "snapshot",
        sequence,
        &[serde_json::json!([
            "r",
            serde_json::to_value(snapshot).unwrap_or(Value::Null)
        ])],
    )
}

/// Build a pir.session `state` update with arbitrary chord delta wire ops
/// (base tuples, `["#", id, path]` definitions, numeric refs, or short
/// forms). The ops are validated by [`parse_service_provider_update`] on the
/// receiving side.
pub fn session_state_delta_update(
    session_id: &str,
    member: &str,
    sequence: i64,
    ops: &[Value],
) -> Value {
    serde_json::json!({
        "type": "state",
        "member": member,
        "sequence": sequence,
        "instance": {"key": session_id, "generation": 1},
        "ops": ops,
    })
}

/// [`session_progress_update`] / [`session_progress_delta_update`] selected by
/// `format`. Hosts that expose a compatibility switch call this instead of
/// choosing the builder at every call site.
pub fn session_progress_update_in(
    session_id: &str,
    progress: &TranscriptProgress,
    sequence: i64,
    format: StateUpdateFormat,
) -> Value {
    match format {
        StateUpdateFormat::Delta => session_progress_delta_update(session_id, progress, sequence),
        StateUpdateFormat::Legacy => session_progress_update(session_id, progress, sequence),
    }
}

/// [`session_snapshot_update`] / [`session_snapshot_delta_update`] selected by
/// `format` (see [`session_progress_update_in`]).
pub fn session_snapshot_update_in(
    session_id: &str,
    snapshot: &SessionSnapshot,
    sequence: i64,
    format: StateUpdateFormat,
) -> Value {
    match format {
        StateUpdateFormat::Delta => session_snapshot_delta_update(session_id, snapshot, sequence),
        StateUpdateFormat::Legacy => session_snapshot_update(session_id, snapshot, sequence),
    }
}

/// Validate a session snapshot value (public wrapper for update bodies).
pub fn parse_session_snapshot_value(value: &Value) -> Result<(), SchemaError> {
    validate_session_snapshot(value, "session snapshot")
}
