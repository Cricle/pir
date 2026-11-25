//! JSON-RPC 2.0 message model (upstream
//! `packages/mcp/src/protocol/jsonrpc.ts`).

use serde_json::Value as JsonValue;

use super::error::{JsonRpcErrorCodes, McpError};

/// A JSON-RPC id: string or finite number (upstream `JsonRpcId`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    String(String),
    Number(i64),
}

impl RequestId {
    /// Whether a raw JSON value is a valid id (string or finite number).
    pub fn from_value(value: &JsonValue) -> Option<Self> {
        match value {
            JsonValue::String(text) => Some(RequestId::String(text.clone())),
            JsonValue::Number(number) => number.as_i64().map(RequestId::Number),
            _ => None,
        }
    }

    /// The id as a JSON value (response echo).
    pub fn to_value(&self) -> JsonValue {
        match self {
            RequestId::String(text) => JsonValue::String(text.clone()),
            RequestId::Number(number) => serde_json::json!(number),
        }
    }
}

/// One inbound JSON-RPC message, classified by shape (upstream
/// `JsonRpcMessage`: request | notification | response).
#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    /// Server-to-client request (has an id and needs an answer).
    Request {
        /// The request id.
        id: RequestId,
        /// Method name.
        method: String,
        /// Optional params.
        params: JsonValue,
    },
    /// Notification (no id, no answer expected).
    Notification {
        /// Method name.
        method: String,
        /// Optional params.
        params: JsonValue,
    },
    /// Success response.
    Success {
        /// The answered request id.
        id: RequestId,
        /// The `result` payload.
        result: JsonValue,
    },
    /// Error response.
    Failure {
        /// The answered request id.
        id: RequestId,
        /// JSON-RPC error code.
        code: i64,
        /// Error message.
        message: String,
        /// Optional `data` payload.
        data: Option<JsonValue>,
    },
}

impl JsonRpcMessage {
    /// Classify one parsed JSON value (upstream `parseJsonRpcMessage`).
    pub fn parse(value: JsonValue) -> Result<Self, McpError> {
        let Some(object) = value.as_object() else {
            return Err(McpError::Protocol(
                "JSON-RPC message must be an object".into(),
            ));
        };
        if object.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
            return Err(McpError::Protocol(
                "JSON-RPC message is not jsonrpc 2.0".into(),
            ));
        }
        let id = object.get("id").and_then(RequestId::from_value);
        let method = object.get("method").and_then(JsonValue::as_str);
        match (id, method) {
            (Some(id), Some(method)) => Ok(JsonRpcMessage::Request {
                id,
                method: method.to_owned(),
                params: object.get("params").cloned().unwrap_or(JsonValue::Null),
            }),
            (None, Some(method)) => Ok(JsonRpcMessage::Notification {
                method: method.to_owned(),
                params: object.get("params").cloned().unwrap_or(JsonValue::Null),
            }),
            (Some(id), None) => {
                if let Some(error) = object.get("error") {
                    let code = error
                        .get("code")
                        .and_then(JsonValue::as_i64)
                        .ok_or_else(|| {
                            McpError::Protocol("JSON-RPC error response without a code".into())
                        })?;
                    let message = error
                        .get("message")
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| {
                            McpError::Protocol("JSON-RPC error response without a message".into())
                        })?
                        .to_owned();
                    return Ok(JsonRpcMessage::Failure {
                        id,
                        code,
                        message,
                        data: error.get("data").cloned(),
                    });
                }
                if let Some(result) = object.get("result") {
                    return Ok(JsonRpcMessage::Success {
                        id,
                        result: result.clone(),
                    });
                }
                Err(McpError::Protocol(
                    "JSON-RPC response needs a result or an error".into(),
                ))
            }
            (None, None) => Err(McpError::Protocol(
                "JSON-RPC message has neither a method nor an id".into(),
            )),
        }
    }

    /// Build a client request message.
    pub fn request(id: &RequestId, method: &str, params: JsonValue) -> JsonValue {
        let mut message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_value(),
            "method": method,
        });
        if !params.is_null() {
            message["params"] = params;
        }
        message
    }

    /// Build a client notification message.
    pub fn notification(method: &str, params: JsonValue) -> JsonValue {
        let mut message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if !params.is_null() {
            message["params"] = params;
        }
        message
    }

    /// Build a server-request answer (`result` form).
    pub fn response_ok(id: &RequestId, result: JsonValue) -> JsonValue {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_value(),
            "result": result,
        })
    }

    /// Build a server-request answer (error form).
    pub fn response_error(id: &RequestId, code: i64, message: &str) -> JsonValue {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_value(),
            "error": { "code": code, "message": message },
        })
    }
}

/// The method-not-found code, used to tolerate servers without
/// `resources/templates/list` (upstream `withoutTemplates`).
pub const METHOD_NOT_FOUND: i64 = JsonRpcErrorCodes::METHOD_NOT_FOUND;
