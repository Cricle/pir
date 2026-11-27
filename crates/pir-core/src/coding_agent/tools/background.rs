//! Model-facing background tool-call control: `bg_status` and `bg_abort`
//! (the user-approved provider-visible surface; see
//! `TOOL-BACKGROUND-SCOPE.md` §"边界与开放项").
//!
//! The names are short on purpose. Every declared tool name is echoed twice
//! per request — in the `<tools>` prompt listing and in the provider's tool
//! array — and the long `background_*` prefix bought nothing the tool's own
//! description does not say. [`LEGACY_STATUS_NAME`] / [`LEGACY_ABORT_NAME`]
//! are still recognized as arguments and in `--tools` lists so an existing
//! settings file or a resumed transcript keeps working.
//!
//! Both tools read and act through [`ToolContext::session_env`]'s
//! `background` handle, so the same implementations serve the session host
//! (`AgentSession`'s registry) and print mode (`PrintBackground`). A host that
//! has no background registry reports that explicitly instead of pretending
//! the call surface is empty.

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};

/// The status tool's model-facing name.
pub const STATUS_TOOL_NAME: &str = "bg_status";
/// The abort tool's model-facing name.
pub const ABORT_TOOL_NAME: &str = "bg_abort";
/// The pre-rename name of the status tool, still accepted where a tool name
/// is written by hand (a settings `defaultTools` list, `--tools`, a resumed
/// transcript).
pub const LEGACY_STATUS_NAME: &str = "background_status";
/// The pre-rename name of the abort tool (same reason).
pub const LEGACY_ABORT_NAME: &str = "background_abort";

/// Whether `name` names the status tool under either spelling.
pub fn is_status_name(name: &str) -> bool {
    name == STATUS_TOOL_NAME || name == LEGACY_STATUS_NAME
}

/// Whether `name` names the abort tool under either spelling.
pub fn is_abort_name(name: &str) -> bool {
    name == ABORT_TOOL_NAME || name == LEGACY_ABORT_NAME
}

/// The exact answer for a host without a background registry (session-less
/// tool execution, a bare embedder). A legal host shape, never an error: the
/// model learns the surface is absent instead of seeing a fabricated empty
/// list.
pub const NO_BACKGROUND_REGISTRY: &str = "No background call registry in this host.";

/// The argument error of `bg_abort` when neither selector is given.
const ABORT_REQUIRES_SELECTOR: &str = "bg_abort requires either \"callId\" or \"all\": true";

/// The model-facing status tool.
pub struct BackgroundStatusTool;

/// The model-facing abort tool.
pub struct BackgroundAbortTool;

impl AgentTool for BackgroundStatusTool {
    fn name(&self) -> &'static str {
        STATUS_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "List the background tool calls still running in this session. Background calls are started by passing `background: true` to another tool; their results arrive later as a backgroundToolResult entry."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("List in-flight background tool calls".to_owned())
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {}
        }))
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let background = ctx
            .session_env
            .as_ref()
            .and_then(|env| env.background.clone());
        Box::pin(async move {
            let Some(control) = background else {
                return ToolResult::ok(NO_BACKGROUND_REGISTRY);
            };
            let calls = control.calls();
            let count = calls.len();
            let calls: Vec<serde_json::Value> = calls
                .iter()
                .map(|call| {
                    serde_json::json!({
                        "callId": call.call_id,
                        "toolName": call.tool_name,
                        "parentToolCallId": call.parent_call_id,
                    })
                })
                .collect();
            let payload = serde_json::json!({ "calls": calls, "count": count });
            match serde_json::to_string_pretty(&payload) {
                Ok(text) => ToolResult::ok(text),
                Err(error) => ToolResult::error(format!(
                    "failed to serialize background call status: {error}"
                )),
            }
        })
    }
}

impl AgentTool for BackgroundAbortTool {
    fn name(&self) -> &'static str {
        ABORT_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Abort one background tool call by id, or every in-flight background call. Returns how many calls were cancelled."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some("Abort background tool calls".to_owned())
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "callId": { "type": "string", "description": "Provider tool-call id to abort (from bg_status)" },
                "all": { "type": "boolean", "description": "Abort every in-flight background call (default: false)" }
            }
        }))
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let call_id = args
            .get("callId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let all = args
            .get("all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // A malformed request is an error even on a registry-less host: the
        // tool surface is the same everywhere, so its argument contract is
        // too.
        if !all && call_id.is_none() {
            return Box::pin(async { ToolResult::error(ABORT_REQUIRES_SELECTOR) });
        }
        let background = ctx
            .session_env
            .as_ref()
            .and_then(|env| env.background.clone());
        Box::pin(async move {
            let Some(control) = background else {
                return ToolResult::ok(NO_BACKGROUND_REGISTRY);
            };
            if all {
                let aborted = control.abort_all();
                return ToolResult::ok(serde_json::json!({ "aborted": aborted }).to_string());
            }
            // The selector is present (checked above).
            let call_id = call_id.unwrap_or_default();
            // An unknown or already-settled id is a successful no-op, the
            // same contract as the `abort_background` RPC command.
            let aborted = usize::from(control.abort(&call_id));
            ToolResult::ok(serde_json::json!({ "aborted": aborted, "callId": call_id }).to_string())
        })
    }
}
