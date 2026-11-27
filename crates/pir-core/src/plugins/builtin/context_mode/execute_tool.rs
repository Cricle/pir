//! Restricted generic tool proxy for context-mode.
//!
//! The proxy deliberately uses the current call's `NestedToolExecutor`, so it
//! inherits the scheduler's callable exposure gate, mutation locks, hooks,
//! abort signal, nested-depth limit and JS/MCP integration. It is not a host
//! bypass and refuses to invoke itself or any context-mode tool.

use serde_json::json;

use crate::agent::nested_tool_calls::ExecuteToolOptions;
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolReach};

use super::CTX_EXECUTE_TOOL_PROXY_NAME;

/// The proxy's own tool name. Deliberately NOT called `CTX_EXECUTE_TOOL_NAME`:
/// that name in the parent module is the JS sandbox tool (`ctx_execute`), and
/// two same-named constants meaning different tools already caused a
/// registration collision once.
pub const CTX_EXECUTE_PROXY_TOOL_NAME: &str = CTX_EXECUTE_TOOL_PROXY_NAME;

/// Stateless: everything the proxy needs arrives through the call's
/// [`ToolContext`] (the nested executor, the abort signal). It keeps no index
/// or runtime handle, so there is nothing to hold here.
pub struct CtxExecuteTool;

impl CtxExecuteTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CtxExecuteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool for CtxExecuteTool {
    fn name(&self) -> &'static str {
        CTX_EXECUTE_PROXY_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Execute one registered callable tool through the normal scheduler. Use this for bounded, composable analysis; context-mode tools cannot invoke themselves."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Execute one registered tool through the normal scheduler so sandbox code can \
compose analysis steps"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Registered callable tool name" },
                "args": { "type": "object", "description": "Arguments passed to the target tool" },
                "background": { "type": "boolean", "description": "Detach the target call when supported" }
            },
            "required": ["name", "args"],
            "additionalProperties": false
        }))
    }

    fn reaches_tools(&self, input: &serde_json::Value) -> Option<ToolReach> {
        input
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(|name| {
                if name.contains('*') || name == "all" {
                    ToolReach::All
                } else {
                    ToolReach::Names(vec![name.to_owned()])
                }
            })
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let input = args.get("args").cloned().unwrap_or(serde_json::Value::Null);
        let background = args
            .get("background")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let nested = ctx.nested.clone();
        Box::pin(async move {
            if name == CTX_EXECUTE_PROXY_TOOL_NAME || name.starts_with("ctx_") {
                return ToolResult::error("ctx_execute_tool cannot invoke context-mode tools");
            }
            let Some(nested) = nested else {
                return ToolResult::error(
                    "ctx_execute_tool is only available inside a running tool call",
                );
            };
            let outcome = nested
                .execute_with_options(&name, input, ExecuteToolOptions { background })
                .await;
            let mut result = outcome.result;
            // Always carry the correlation id: the target may have no details
            // of its own, and the caller still needs the nested call id.
            let entry = result
                .details
                .get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            match entry.as_object_mut() {
                Some(object) => {
                    object.insert(
                        "nestedToolCallId".to_owned(),
                        serde_json::Value::String(outcome.tool_call_id),
                    );
                }
                None => {
                    // A non-object details value is replaced, not dropped.
                    *entry = serde_json::json!({
                        "nestedToolCallId": outcome.tool_call_id,
                    });
                }
            }
            result
        })
    }
}
