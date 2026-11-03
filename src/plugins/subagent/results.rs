use super::*;

// Results (upstream `SingleResult` / `SubagentDetails`)
// ---------------------------------------------------------------------------

/// Aggregated child-run usage (upstream `UsageStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub context_tokens: u64,
    pub turns: u64,
}

/// One task's outcome (upstream `SingleResult`). Failure is
/// `stop_reason` `error`/`aborted` or a child run error (`stderr`).
#[derive(Debug, Clone)]
pub struct SingleResult {
    pub agent: String,
    pub agent_source: &'static str,
    pub task: String,
    /// Full child transcript (upstream collects the JSON-mode
    /// `message_end`/`tool_result_end` events; pir reads the child's
    /// ephemeral transcript).
    pub messages: Vec<Message>,
    pub usage: UsageTotals,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    /// stderr-equivalent: the child run error text (see the module doc).
    pub stderr: Option<String>,
    pub step: Option<usize>,
    /// Parallel placeholder still running (upstream `exitCode: -1`).
    pub pending: bool,
}

impl SingleResult {
    /// A failed result without a child run (unknown agent, unresolvable
    /// model, ...).
    pub fn failed(agent: &str, task: &str, message: String, step: Option<usize>) -> Self {
        Self {
            agent: agent.to_owned(),
            agent_source: "unknown",
            task: task.to_owned(),
            messages: Vec::new(),
            usage: UsageTotals::default(),
            model: None,
            stop_reason: Some("error".to_owned()),
            error_message: Some(message.clone()),
            stderr: Some(message),
            step,
            pending: false,
        }
    }

    /// A still-running parallel placeholder (upstream `exitCode: -1`).
    pub fn placeholder(agent: &str, task: &str) -> Self {
        Self {
            agent: agent.to_owned(),
            agent_source: "unknown",
            task: task.to_owned(),
            messages: Vec::new(),
            usage: UsageTotals::default(),
            model: None,
            stop_reason: None,
            error_message: None,
            stderr: None,
            step: None,
            pending: true,
        }
    }

    /// Wire shape of the `details.results[]` entry (upstream `SingleResult`).
    /// `exitCode` mirrors upstream: `0` on a successful child run, `1` on a
    /// failed one, `-1` for a still-running parallel placeholder (pir has no
    /// process exit code; the child run error plays that role). `pending` is
    /// pir's extra flag for the same state (upstream readers key off
    /// `exitCode === -1`).
    pub fn to_json(&self) -> JsonValue {
        let exit_code = if self.pending {
            -1
        } else if is_failed(self) {
            1
        } else {
            0
        };
        // Build the object by inserts and MOVE the serialized child
        // transcript: `json!` re-serializes every `Value` expression, copying
        // the full child transcript a second time.
        let mut value = serde_json::Map::with_capacity(10);
        value.insert("agent".to_owned(), JsonValue::String(self.agent.clone()));
        value.insert(
            "agentSource".to_owned(),
            JsonValue::String(self.agent_source.to_owned()),
        );
        value.insert("task".to_owned(), JsonValue::String(self.task.clone()));
        value.insert("exitCode".to_owned(), JsonValue::from(exit_code));
        value.insert(
            "messages".to_owned(),
            serde_json::to_value(&self.messages).unwrap_or(JsonValue::Null),
        );
        value.insert(
            "usage".to_owned(),
            serde_json::json!({
                "input": self.usage.input,
                "output": self.usage.output,
                "cacheRead": self.usage.cache_read,
                "cacheWrite": self.usage.cache_write,
                "cost": self.usage.cost,
                "contextTokens": self.usage.context_tokens,
                "turns": self.usage.turns,
            }),
        );
        value.insert(
            "stderr".to_owned(),
            self.stderr
                .as_ref()
                .map_or(JsonValue::Null, |text| JsonValue::String(text.clone())),
        );
        if let Some(model) = &self.model {
            value.insert("model".to_owned(), JsonValue::String(model.clone()));
        }
        if let Some(stop_reason) = &self.stop_reason {
            value.insert(
                "stopReason".to_owned(),
                JsonValue::String(stop_reason.clone()),
            );
        }
        if let Some(error_message) = &self.error_message {
            value.insert(
                "errorMessage".to_owned(),
                JsonValue::String(error_message.clone()),
            );
        }
        if let Some(step) = self.step {
            value.insert("step".to_owned(), JsonValue::from(step));
        }
        if self.pending {
            value.insert("pending".to_owned(), JsonValue::Bool(true));
        }
        JsonValue::Object(value)
    }
}

/// Whether a result counts as failed (upstream `isFailedResult`; pir has no
/// exit code — the child run error plays that role).
pub fn is_failed(result: &SingleResult) -> bool {
    matches!(
        result.stop_reason.as_deref(),
        Some("error") | Some("aborted")
    ) || result.stderr.is_some()
}

/// The last assistant message's first text block (upstream `getFinalOutput`).
pub fn get_final_output(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        let Message::Assistant(assistant) = message else {
            return None;
        };
        assistant.content.iter().find_map(|block| match block {
            crate::ai::types::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
    })
}

/// Model-visible output of one result with the upstream error-text
/// precedence: errorMessage → stderr → final output → "(no output)".
pub fn result_output(result: &SingleResult) -> String {
    if is_failed(result) {
        return result
            .error_message
            .clone()
            .or_else(|| result.stderr.clone())
            .or_else(|| get_final_output(&result.messages))
            .unwrap_or_else(|| "(no output)".to_owned());
    }
    get_final_output(&result.messages).unwrap_or_else(|| "(no output)".to_owned())
}

/// UTF-8-bounded 50 KB truncation of parallel per-task output (upstream
/// `truncateParallelOutput`).
pub fn truncate_parallel_output(output: &str) -> String {
    let byte_length = output.len();
    if byte_length <= PER_TASK_OUTPUT_CAP {
        return output.to_owned();
    }
    let mut truncated: String = output
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= PER_TASK_OUTPUT_CAP)
        .last()
        .and_then(|index| output.get(..index))
        .unwrap_or_default()
        .to_owned();
    while truncated.len() > PER_TASK_OUTPUT_CAP {
        truncated.pop();
    }
    let omitted = byte_length - truncated.len();
    format!(
        "{truncated}\n\n[Output truncated: {omitted} bytes omitted. Full output preserved in tool details.]"
    )
}

/// Replace every `{previous}` placeholder with the prior chain step's final
/// output (upstream `step.task.replace(/\{previous\}/g, previousOutput)`).
pub fn apply_previous(task: &str, previous: &str) -> String {
    task.replace("{previous}", previous)
}

/// The aggregate parallel result text (upstream summaries format).
pub fn aggregate_parallel_text(results: &[SingleResult]) -> String {
    let success = results
        .iter()
        .filter(|result| !result.pending && !is_failed(result))
        .count();
    let blocks: Vec<String> = results
        .iter()
        .map(|result| {
            let status = if result.pending {
                "running".to_owned()
            } else if is_failed(result) {
                let reason = result
                    .stop_reason
                    .as_deref()
                    .filter(|reason| *reason != "stop")
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default();
                format!("failed{reason}")
            } else {
                "completed".to_owned()
            };
            format!(
                "### [{}] {}\n\n{}",
                result.agent,
                status,
                truncate_parallel_output(&result_output(result))
            )
        })
        .collect();
    format!(
        "Parallel: {}/{} succeeded\n\n{}",
        success,
        results.len(),
        blocks.join("\n\n---\n\n")
    )
}

/// Builder for the persisted `details` payload (upstream `SubagentDetails`).
#[derive(Debug, Clone)]
pub(super) struct DetailsBuilder {
    pub(super) mode: &'static str,
    pub(super) agent_scope: AgentScope,
    pub(super) project_agents_dir: Option<PathBuf>,
}

impl DetailsBuilder {
    pub(super) fn build(&self, results: &[SingleResult]) -> JsonValue {
        // Move the per-result objects: `json!` would re-serialize the whole
        // `Vec<Value>` (each result carries its child transcript) again.
        let mut value = serde_json::Map::with_capacity(4);
        value.insert("mode".to_owned(), JsonValue::String(self.mode.to_owned()));
        value.insert(
            "agentScope".to_owned(),
            JsonValue::String(self.agent_scope.as_str().to_owned()),
        );
        value.insert(
            "projectAgentsDir".to_owned(),
            self.project_agents_dir
                .as_ref()
                .map_or(JsonValue::Null, |path| {
                    JsonValue::String(path.display().to_string())
                }),
        );
        value.insert(
            "results".to_owned(),
            JsonValue::Array(results.iter().map(SingleResult::to_json).collect()),
        );
        JsonValue::Object(value)
    }
}
