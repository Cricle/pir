//! Transcript content, item, and progress validation.

use serde_json::Value;

use super::error::SchemaError;
use super::validate::{
    field, field_opt, require_array, require_bool, require_id, require_literal,
    require_nonempty_string, require_object, require_string, require_uint, validate_json_value,
    validate_model_ref, validate_strict, validate_strict_object, validate_usage,
};

fn validate_content_text(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["type", "text"], path)?;
    require_literal(field(obj, "type", path)?, "text", &format!("{path}.type"))?;
    require_string(field(obj, "text", path)?, &format!("{path}.text"))?;
    Ok(())
}

fn validate_content_thinking(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["type", "thinking", "redacted"], path)?;
    require_literal(
        field(obj, "type", path)?,
        "thinking",
        &format!("{path}.type"),
    )?;
    require_string(field(obj, "thinking", path)?, &format!("{path}.thinking"))?;
    if let Some(opt) = field_opt(obj, "redacted") {
        require_bool(opt, &format!("{path}.redacted"))?;
    }
    Ok(())
}

fn validate_content_image(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["type", "data", "mimeType"], path)?;
    require_literal(field(obj, "type", path)?, "image", &format!("{path}.type"))?;
    require_string(field(obj, "data", path)?, &format!("{path}.data"))?;
    require_nonempty_string(field(obj, "mimeType", path)?, &format!("{path}.mimeType"))?;
    Ok(())
}

fn validate_content_tool_call(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["type", "toolCallId", "toolName", "input"], path)?;
    require_literal(
        field(obj, "type", path)?,
        "toolCall",
        &format!("{path}.type"),
    )?;
    require_id(
        field(obj, "toolCallId", path)?,
        &format!("{path}.toolCallId"),
    )?;
    require_id(field(obj, "toolName", path)?, &format!("{path}.toolName"))?;
    validate_json_value(field(obj, "input", path)?, &format!("{path}.input"))?;
    Ok(())
}

fn validate_user_content(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let t = require_string(field(obj, "type", path)?, &format!("{path}.type"))?;
    match t {
        "text" => validate_content_text(v, path),
        "image" => validate_content_image(v, path),
        _ => Err(SchemaError::new(format!(
            "{path}.type: expected \"text\" or \"image\""
        ))),
    }
}

fn validate_assistant_content(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let t = require_string(field(obj, "type", path)?, &format!("{path}.type"))?;
    match t {
        "text" => validate_content_text(v, path),
        "thinking" => validate_content_thinking(v, path),
        "toolCall" => validate_content_tool_call(v, path),
        _ => Err(SchemaError::new(format!(
            "{path}.type: expected \"text\", \"thinking\", or \"toolCall\""
        ))),
    }
}

fn validate_tool_content(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let t = require_string(field(obj, "type", path)?, &format!("{path}.type"))?;
    match t {
        "text" => validate_content_text(v, path),
        "image" => validate_content_image(v, path),
        _ => Err(SchemaError::new(format!(
            "{path}.type: expected \"text\" or \"image\""
        ))),
    }
}

fn validate_user_content_array(v: &Value, path: &str) -> Result<(), SchemaError> {
    let arr = require_array(v, path)?;
    for (i, item) in arr.iter().enumerate() {
        validate_user_content(item, &format!("{path}[{i}]"))?;
    }
    Ok(())
}

fn validate_assistant_content_array(v: &Value, path: &str) -> Result<(), SchemaError> {
    let arr = require_array(v, path)?;
    for (i, item) in arr.iter().enumerate() {
        validate_assistant_content(item, &format!("{path}[{i}]"))?;
    }
    Ok(())
}

fn validate_tool_content_array(v: &Value, path: &str) -> Result<(), SchemaError> {
    let arr = require_array(v, path)?;
    for (i, item) in arr.iter().enumerate() {
        validate_tool_content(item, &format!("{path}[{i}]"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcript items
// ---------------------------------------------------------------------------

fn validate_user_transcript_item(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_strict(obj, &["id", "role", "content", "timestamp"], path)?;
    require_id(field(obj, "id", path)?, &format!("{path}.id"))?;
    require_literal(field(obj, "role", path)?, "user", &format!("{path}.role"))?;
    validate_user_content_array(field(obj, "content", path)?, &format!("{path}.content"))?;
    require_uint(field(obj, "timestamp", path)?, &format!("{path}.timestamp"))?;
    Ok(())
}

fn validate_assistant_item_base(
    obj: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), SchemaError> {
    validate_strict_object(
        obj,
        &[
            "id",
            "role",
            "content",
            "model",
            "responseModel",
            "usage",
            "status",
            "stopReason",
            "errorMessage",
            "timestamp",
        ],
        path,
    )?;
    require_id(field(obj, "id", path)?, &format!("{path}.id"))?;
    require_literal(
        field(obj, "role", path)?,
        "assistant",
        &format!("{path}.role"),
    )?;
    validate_assistant_content_array(field(obj, "content", path)?, &format!("{path}.content"))?;
    validate_model_ref(field(obj, "model", path)?, &format!("{path}.model"))?;
    if let Some(opt) = field_opt(obj, "responseModel") {
        require_nonempty_string(opt, &format!("{path}.responseModel"))?;
    }
    if let Some(opt) = field_opt(obj, "usage") {
        validate_usage(opt, &format!("{path}.usage"))?;
    }
    require_uint(field(obj, "timestamp", path)?, &format!("{path}.timestamp"))?;
    Ok(())
}

fn validate_assistant_transcript_item(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_assistant_item_base(obj, path)?;
    let status = require_string(field(obj, "status", path)?, &format!("{path}.status"))?;
    match status {
        "streaming" => {
            if let Some(_stop) = field_opt(obj, "stopReason") {
                return Err(SchemaError::new(format!(
                    "{path}: streaming item must not have stopReason"
                )));
            }
        }
        "complete" => {
            let stop_reason = require_string(
                field(obj, "stopReason", path)?,
                &format!("{path}.stopReason"),
            )?;
            if !["stop", "length", "toolUse"].contains(&stop_reason) {
                return Err(SchemaError::new(format!(
                    "{path}.stopReason: expected \"stop\", \"length\", or \"toolUse\""
                )));
            }
        }
        "error" => {
            require_literal(
                field(obj, "stopReason", path)?,
                "error",
                &format!("{path}.stopReason"),
            )?;
        }
        "aborted" => {
            require_literal(
                field(obj, "stopReason", path)?,
                "aborted",
                &format!("{path}.stopReason"),
            )?;
        }
        _ => {
            return Err(SchemaError::new(format!(
                "{path}.status: expected streaming/complete/error/aborted"
            )))
        }
    }
    Ok(())
}

fn validate_tool_item_base(
    obj: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), SchemaError> {
    validate_strict_object(
        obj,
        &[
            "id",
            "role",
            "toolCallId",
            "toolName",
            "input",
            "content",
            "details",
            "usage",
            "status",
            "isError",
            "timestamp",
        ],
        path,
    )?;
    require_id(field(obj, "id", path)?, &format!("{path}.id"))?;
    require_literal(field(obj, "role", path)?, "tool", &format!("{path}.role"))?;
    require_id(
        field(obj, "toolCallId", path)?,
        &format!("{path}.toolCallId"),
    )?;
    require_id(field(obj, "toolName", path)?, &format!("{path}.toolName"))?;
    validate_json_value(field(obj, "input", path)?, &format!("{path}.input"))?;
    validate_tool_content_array(field(obj, "content", path)?, &format!("{path}.content"))?;
    if let Some(opt) = field_opt(obj, "details") {
        validate_json_value(opt, &format!("{path}.details"))?;
    }
    if let Some(opt) = field_opt(obj, "usage") {
        validate_usage(opt, &format!("{path}.usage"))?;
    }
    require_uint(field(obj, "timestamp", path)?, &format!("{path}.timestamp"))?;
    Ok(())
}

fn validate_tool_transcript_item(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    validate_tool_item_base(obj, path)?;
    let status = require_string(field(obj, "status", path)?, &format!("{path}.status"))?;
    let is_error = require_bool(field(obj, "isError", path)?, &format!("{path}.isError"))?;
    match status {
        "running" => {
            if is_error {
                return Err(SchemaError::new(format!(
                    "{path}: running item must have isError=false"
                )));
            }
        }
        "complete" => {
            if is_error {
                return Err(SchemaError::new(format!(
                    "{path}: complete item must have isError=false"
                )));
            }
        }
        "error" => {
            if !is_error {
                return Err(SchemaError::new(format!(
                    "{path}: error item must have isError=true"
                )));
            }
        }
        _ => {
            return Err(SchemaError::new(format!(
                "{path}.status: expected running/complete/error"
            )))
        }
    }
    Ok(())
}

fn validate_transcript_item(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let role = require_string(field(obj, "role", path)?, &format!("{path}.role"))?;
    match role {
        "user" => validate_user_transcript_item(v, path),
        "assistant" => validate_assistant_transcript_item(v, path),
        "tool" => validate_tool_transcript_item(v, path),
        _ => Err(SchemaError::new(format!(
            "{path}.role: expected user/assistant/tool"
        ))),
    }
}

pub(crate) fn validate_transcript_item_array(v: &Value, path: &str) -> Result<(), SchemaError> {
    let arr = require_array(v, path)?;
    for (i, item) in arr.iter().enumerate() {
        validate_transcript_item(item, &format!("{path}[{i}]"))?;
    }
    Ok(())
}

pub(crate) fn validate_user_transcript_item_array(
    v: &Value,
    path: &str,
) -> Result<(), SchemaError> {
    let arr = require_array(v, path)?;
    for (i, item) in arr.iter().enumerate() {
        validate_user_transcript_item(item, &format!("{path}[{i}]"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcript progress
// ---------------------------------------------------------------------------

pub(crate) fn validate_transcript_progress(v: &Value, path: &str) -> Result<(), SchemaError> {
    let obj = require_object(v, path)?;
    let t = require_string(field(obj, "type", path)?, &format!("{path}.type"))?;
    match t {
        "item_started" => {
            validate_strict(obj, &["type", "item"], path)?;
            validate_transcript_item(field(obj, "item", path)?, &format!("{path}.item"))?;
        }
        "assistant_delta" => {
            validate_strict(
                obj,
                &["type", "messageId", "contentIndex", "kind", "delta"],
                path,
            )?;
            require_id(field(obj, "messageId", path)?, &format!("{path}.messageId"))?;
            require_uint(
                field(obj, "contentIndex", path)?,
                &format!("{path}.contentIndex"),
            )?;
            let kind = require_string(field(obj, "kind", path)?, &format!("{path}.kind"))?;
            if !["text", "thinking", "toolCall"].contains(&kind) {
                return Err(SchemaError::new(format!(
                    "{path}.kind: expected text/thinking/toolCall"
                )));
            }
            require_string(field(obj, "delta", path)?, &format!("{path}.delta"))?;
        }
        "item_updated" => {
            validate_strict(obj, &["type", "item"], path)?;
            let item = field(obj, "item", path)?;
            let item_obj = require_object(item, &format!("{path}.item"))?;
            let status = require_string(
                field(item_obj, "status", &format!("{path}.item"))?,
                &format!("{path}.item.status"),
            )?;
            if !["streaming", "running"].contains(&status) {
                return Err(SchemaError::new(format!(
                    "{path}.item.status: expected streaming or running for item_updated"
                )));
            }
            let role = require_string(
                field(item_obj, "role", &format!("{path}.item"))?,
                &format!("{path}.item.role"),
            )?;
            match role {
                "assistant" => validate_assistant_transcript_item(item, &format!("{path}.item"))?,
                "tool" => validate_tool_transcript_item(item, &format!("{path}.item"))?,
                _ => {
                    return Err(SchemaError::new(format!(
                        "{path}.item.role: expected assistant or tool"
                    )))
                }
            }
        }
        "item_finished" => {
            validate_strict(obj, &["type", "item"], path)?;
            let item = field(obj, "item", path)?;
            let item_obj = require_object(item, &format!("{path}.item"))?;
            let status = require_string(
                field(item_obj, "status", &format!("{path}.item"))?,
                &format!("{path}.item.status"),
            )?;
            if !["complete", "error", "aborted"].contains(&status) {
                return Err(SchemaError::new(format!(
                    "{path}.item.status: expected complete/error/aborted for item_finished"
                )));
            }
            let role = require_string(
                field(item_obj, "role", &format!("{path}.item"))?,
                &format!("{path}.item.role"),
            )?;
            match role {
                "assistant" => validate_assistant_transcript_item(item, &format!("{path}.item"))?,
                "tool" => validate_tool_transcript_item(item, &format!("{path}.item"))?,
                _ => {
                    return Err(SchemaError::new(format!(
                        "{path}.item.role: expected assistant or tool"
                    )))
                }
            }
        }
        _ => {
            return Err(SchemaError::new(format!(
                "{path}.type: expected item_started/assistant_delta/item_updated/item_finished"
            )))
        }
    }
    Ok(())
}
