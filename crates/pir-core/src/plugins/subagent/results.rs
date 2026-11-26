use super::*;

// Results (upstream `SingleResult` / `SubagentDetails`)
// ---------------------------------------------------------------------------

// `UsageTotals` / `WorktreeOutcome` are the child-run DTOs the engine-free
// crate owns (its result formatters take them); re-exported so the host and
// the crate cannot describe one run differently.
pub use pir_builtin_subagent::{UsageTotals, WorktreeOutcome};

/// Serialize the child transcript for `details` with a bounded size. Returns
/// the value and how many messages were dropped.
fn bounded_messages_json(messages: &[Message]) -> (JsonValue, usize) {
    // The bounding policy (which blocks are transcript evidence, the
    // per-string cap, the per-result budget and the system-message
    // preference) lives in the engine-free crate; this only turns the
    // typed messages into the JSON rows it walks.
    let rows: Vec<JsonValue> = messages
        .iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .collect();
    pir_builtin_subagent::bounded_messages_json(&rows)
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
    /// Worktree isolation metadata, when this task used one.
    pub worktree: Option<WorktreeOutcome>,
    /// `inline` or `process`.
    pub executor: &'static str,
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
            worktree: None,
            executor: "inline",
        }
    }

    /// A still-running chain-step placeholder. The step number is known when
    /// the step starts (upstream renders `─── Step N: agent ⏳` immediately;
    /// without it the panel lags one step behind, PIR-TUI-005).
    pub fn placeholder_for_step(agent: &str, task: &str, step: usize) -> Self {
        Self {
            step: Some(step),
            ..Self::placeholder(agent, task)
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
            worktree: None,
            executor: "inline",
        }
    }

    /// Wire shape of the `details.results[]` entry (upstream `SingleResult`).
    /// `exitCode` mirrors upstream: `0` on a successful child run, `1` on a
    /// failed one, `-1` for a still-running parallel placeholder (pir has no
    /// process exit code; the child run error plays that role). `pending` is
    /// pir's extra flag for the same state (upstream readers key off
    /// `exitCode === -1`).
    pub fn to_json(&self) -> JsonValue {
        self.to_json_with(&self.messages)
    }

    /// Live-update shape: the final (settled) result keeps the bounded full
    /// transcript via [`Self::to_json`]; a per-turn partial only needs the
    /// latest assistant message for the panel preview. Serializing the whole
    /// transcript on every child turn was O(n²) per subagent run — and every
    /// byte of it was thrown away again on the next turn.
    pub(super) fn to_json_partial(&self) -> JsonValue {
        let last_assistant = self
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::Assistant(_)));
        match last_assistant {
            Some(index) => self.to_json_with(&self.messages[index..=index]),
            None => self.to_json_with(&[]),
        }
    }

    fn to_json_with(&self, messages: &[Message]) -> JsonValue {
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
        // Bound the retained transcript: the model only reads the final
        // output, the TUI renders roles/text/tool names, and the session file
        // keeps this `details` value for the rest of the run. A child that
        // wrote megabytes through a tool call would otherwise pin all of it in
        // the parent. The typed `self.messages` stays untouched, so chain
        // `{previous}`, error text and final output are unaffected.
        let (messages, omitted) = bounded_messages_json(messages);
        value.insert("messages".to_owned(), messages);
        if omitted > 0 {
            value.insert("messagesTruncated".to_owned(), JsonValue::Bool(true));
            value.insert("messagesOmitted".to_owned(), JsonValue::from(omitted));
        }
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
        if self.executor != "inline" {
            value.insert(
                "executor".to_owned(),
                JsonValue::String(self.executor.to_owned()),
            );
        }
        if let Some(worktree) = &self.worktree {
            value.insert(
                "worktree".to_owned(),
                serde_json::json!({
                    "repo": worktree.repo,
                    "path": worktree.path,
                    "branch": worktree.branch,
                    "base": worktree.base,
                    "merged": worktree.merged,
                    "cleaned": worktree.cleaned,
                    "error": worktree.error,
                }),
            );
        }
        JsonValue::Object(value)
    }
}

/// Whether a result counts as failed (upstream `isFailedResult`; pir has no
/// exit code — the child run error plays that role).
///
/// The rule lives in the engine-free `pir-builtin-subagent` crate. `length` is
/// output truncation: the child did not finish the task it was given, so it
/// must not be treated as a successful worker or a passing reviewer.
pub fn is_failed(result: &SingleResult) -> bool {
    pir_builtin_subagent::is_failed(&to_crate_result(result))
}

/// Project the engine's result row onto the crate's host-neutral row.
///
/// The only structural difference is `messages`: the engine holds typed
/// [`Message`]s, the crate holds their JSON rows (it must not depend on the
/// engine's message model). The engine's `Message` serializer is hand-written
/// precisely so a JSON hop round-trips losslessly, and the result-formatting
/// helpers only ever read `role`/`content`, so the projection is exact.
fn to_crate_result(result: &SingleResult) -> pir_builtin_subagent::SingleResult {
    pir_builtin_subagent::SingleResult {
        agent: result.agent.clone(),
        agent_source: result.agent_source,
        task: result.task.clone(),
        messages: result
            .messages
            .iter()
            .map(|message| serde_json::to_value(message).unwrap_or(serde_json::Value::Null))
            .collect(),
        usage: pir_builtin_subagent::UsageTotals {
            input: result.usage.input,
            output: result.usage.output,
            cache_read: result.usage.cache_read,
            cache_write: result.usage.cache_write,
            cost: result.usage.cost,
            context_tokens: result.usage.context_tokens,
            turns: result.usage.turns,
        },
        model: result.model.clone(),
        stop_reason: result.stop_reason.clone(),
        error_message: result.error_message.clone(),
        stderr: result.stderr.clone(),
        step: result.step,
        pending: result.pending,
        worktree: result
            .worktree
            .as_ref()
            .map(|worktree| pir_builtin_subagent::WorktreeOutcome {
                repo: worktree.repo.clone(),
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                base: worktree.base.clone(),
                merged: worktree.merged,
                cleaned: worktree.cleaned,
                error: worktree.error.clone(),
            }),
        executor: result.executor,
    }
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
    pir_builtin_subagent::result_output(&to_crate_result(result))
}

/// UTF-8-bounded 50 KB truncation of parallel per-task output (upstream
/// `truncateParallelOutput`). The rule lives in the engine-free crate.
pub fn truncate_parallel_output(output: &str) -> String {
    pir_builtin_subagent::truncate_parallel_output(output)
}

/// Replace every `{previous}` placeholder with the prior chain step's final
/// output (upstream `step.task.replace(/\{previous\}/g, previousOutput)`).
pub fn apply_previous(task: &str, previous: &str) -> String {
    pir_builtin_subagent::apply_previous(task, previous)
}

/// The aggregate parallel result text (upstream summaries format).
///
/// The format lives in the engine-free crate; this projects the engine rows
/// onto its neutral rows (see [`to_crate_result`]).
pub fn aggregate_parallel_text(results: &[SingleResult]) -> String {
    let rows: Vec<pir_builtin_subagent::SingleResult> =
        results.iter().map(to_crate_result).collect();
    pir_builtin_subagent::aggregate_parallel_text(&rows)
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
        let mut value = self.header();
        value.insert(
            "results".to_owned(),
            JsonValue::Array(results.iter().map(SingleResult::to_json).collect()),
        );
        JsonValue::Object(value)
    }

    /// Settled details when the caller keeps its results as `Arc` (parallel/
    /// chain live lists): identical bytes to [`Self::build`].
    pub(super) fn build_arcs(&self, results: &[Arc<SingleResult>]) -> JsonValue {
        let mut value = self.header();
        value.insert(
            "results".to_owned(),
            JsonValue::Array(results.iter().map(|result| result.to_json()).collect()),
        );
        JsonValue::Object(value)
    }

    /// Live-update details: identical wrapper, but each result carries only
    /// its latest assistant message. The settled result uses [`Self::build`].
    pub(super) fn build_light(&self, results: &[Arc<SingleResult>]) -> JsonValue {
        let mut value = self.header();
        value.insert(
            "results".to_owned(),
            JsonValue::Array(
                results
                    .iter()
                    .map(|result| result.to_json_partial())
                    .collect(),
            ),
        );
        JsonValue::Object(value)
    }

    fn header(&self) -> serde_json::Map<String, JsonValue> {
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
        value
    }
}
