//! Concrete command catalog types (v1 session service payload layer).
//!
//! The `Command` enum is the typed construction surface for the session
//! commands (the client's `request` shim); command *results* are opaque JSON
//! values on the v8 wire and have no typed shape.

use serde::Serialize;

use super::types::ModelRef;

// ---------------------------------------------------------------------------
// Command types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    List,
    Create {
        #[serde(rename = "cwd", skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(rename = "name", skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(rename = "model", skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(rename = "thinkingLevel", skip_serializing_if = "Option::is_none")]
        thinking_level: Option<String>,
    },
    Attach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Prompt {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "text")]
        text: String,
    },
    Steer {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "text")]
        text: String,
    },
    Abort {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    SetModel {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "model")]
        model: ModelRef,
    },
    SetThinking {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    /// pir extension (upstream `SessionRouter.removeSession` has no v8 wire
    /// command); translated to the additive `pir.session` `remove_session`
    /// member, which requires the caller to hold the session's attachment.
    Remove {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
}
