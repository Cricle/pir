//! The `submit_plan` tool, available only in plan mode.
//!
//! It is the model-visible "exit plan mode" surface: the assistant finishes
//! investigation, writes a concrete plan into this tool, and the tool call is
//! intercepted by the mode plugin's `tool_call` gate (which creates the
//! approval record and blocks execution). The tool's own `run` is a defensive
//! fallback for hosts that do not dispatch the gate; the gate is the normal
//! path.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent::tool_batch::{ToolExecutionMode, ToolResult};
use crate::agent::tool_execution::BoxFuture;
use crate::agent::AgentTool;

/// Registration name of the plan-submission tool.
pub(super) const SUBMIT_PLAN_TOOL: &str = "submit_plan";

/// A no-op fallback tool; the real behavior lives in the mode plugin's
/// `tool_call` handler. Keeping a registered native tool means the model sees
/// a normal schema and the gate can intercept it like any other tool.
pub(super) struct SubmitPlanTool;

impl AgentTool for SubmitPlanTool {
    fn name(&self) -> &str {
        SUBMIT_PLAN_TOOL
    }

    fn label(&self) -> &str {
        "Submit plan"
    }

    fn description(&self) -> &str {
        "Submit the finished plan for user approval. Call this when investigation is complete."
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The concrete plan: ordered steps, files to change, risks, and verification."
                },
                "questions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional open questions that block a correct plan."
                }
            },
            "required": ["plan"]
        }))
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn run(
        &self,
        args: &Value,
        _ctx: &crate::agent::tool_execution::ToolContext,
    ) -> BoxFuture<'static, ToolResult> {
        let plan_len = args
            .get("plan")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .count();
        Box::pin(async move {
            ToolResult::ok(format!(
                "Plan submitted for approval ({plan_len} chars). Awaiting the user's decision. \
                 End your turn now; do not sleep or poll — the decision arrives as a follow-up message."
            ))
        })
    }
}

/// Register the tool on a plugin's registration context, sharing the mode
/// state so the gate can read it (the actual work is done in `on_event`).
pub(super) fn register_submit_plan_tool(
    ctx: &mut crate::plugins::RegistrationContext<'_>,
) -> anyhow::Result<()> {
    ctx.register_tool(Arc::new(SubmitPlanTool));
    Ok(())
}
