//! Pure delegation/approval-domain helpers.
//!
//! Everything here operates on `serde_json::Value` payloads only. It is the
//! engine's `mode` plugin logic with the host seam removed: no session,
//! service, or `pir-core` types.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

use crate::approvals::ApprovalRecord;
use crate::journal::Verification;
use crate::mode::Mode;
use crate::settings::{ApprovalMode, ModeSettings, RouterMode};

/// Direct tools a foreman/plan session may call. Everything else is blocked.
pub const MODE_ALLOWED_TOOLS: &[&str] = &[
    "submit_plan",
    "subagent",
    "read",
    "grep",
    "find",
    "ls",
    "tool_search",
    "todo",
    "bg_status",
    "bg_abort",
];

/// Hard read-only tool set for plan-mode subagents.
pub const PLAN_CHILD_TOOLS: &[&str] = &["read", "grep", "find", "ls"];

/// Reviewer children keep inspection tools plus `bash` (the smoke check the
/// reviewer contract requires) but lose every write-capable tool.
pub const REVIEWER_CHILD_TOOLS: &[&str] = &["read", "grep", "find", "ls", "bash"];

/// Registration name of the plan-submission tool.
pub const SUBMIT_PLAN_TOOL: &str = "submit_plan";

/// Every agent named by one `subagent` call: `agent` for single mode plus
/// each `tasks[]` / `chain[]` item.
pub fn subagent_agents(input: &Value) -> Vec<&str> {
    let mut agents = Vec::new();
    if let Some(agent) = input.get("agent").and_then(Value::as_str) {
        agents.push(agent);
    }
    for key in ["tasks", "chain"] {
        if let Some(items) = input.get(key).and_then(Value::as_array) {
            agents.extend(
                items
                    .iter()
                    .filter_map(|item| item.get("agent").and_then(Value::as_str)),
            );
        }
    }
    agents
}

/// Whether a delegation has at least one non-reviewer (worker) agent.
pub fn has_worker(agents: &[impl AsRef<str>]) -> bool {
    agents.iter().any(|agent| agent.as_ref() != "reviewer")
}

/// Whether a delegation has at least one reviewer agent.
pub fn has_reviewer(agents: &[impl AsRef<str>]) -> bool {
    agents.iter().any(|agent| agent.as_ref() == "reviewer")
}

/// Scan one transcript message for `subagent` calls, advancing `generation`
/// for each worker call and remembering the latest worker/reviewer call ids.
pub fn scan_subagent_calls(
    message: &Value,
    generation: &mut u32,
    last_reviewer: &mut Option<String>,
    last_worker: &mut Option<String>,
) {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("toolCall")
            || block.get("name").and_then(Value::as_str) != Some("subagent")
        {
            continue;
        }
        let input = block.get("arguments").cloned().unwrap_or(Value::Null);
        let agents = subagent_agents(&input);
        if has_worker(&agents) {
            *generation += 1;
            *last_reviewer = None;
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    *last_worker = Some(id.to_owned());
                }
            }
        }
        if has_reviewer(&agents) {
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    *last_reviewer = Some(id.to_owned());
                }
            }
        }
    }
}

/// Stable approval id for a worker delegation input.
pub fn approval_id(input: &Value) -> String {
    let mut hasher = DefaultHasher::new();
    // Every authority-relevant field: a worker approved with a read-only tool
    // list, a different cwd, or a different isolation/executor must not reuse
    // that approval for a broader request.
    for key in [
        "agent",
        "task",
        "tasks",
        "chain",
        "cwd",
        "tools",
        "isolation",
        "worktreeMerge",
        "executor",
        "agentScope",
        "confirmProjectAgents",
    ] {
        if let Some(value) = input.get(key) {
            key.hash(&mut hasher);
            value.to_string().hash(&mut hasher);
        }
    }
    format!("a{:016x}", hasher.finish())
}

/// One-line human summary of a worker delegation input.
pub fn approval_summary(input: &Value) -> String {
    let agents = subagent_agents(input);
    let task = input
        .get("task")
        .and_then(Value::as_str)
        .or_else(|| {
            input
                .get("tasks")
                .and_then(Value::as_array)
                .and_then(|tasks| tasks.first())
                .and_then(|task| task.get("task"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            input
                .get("chain")
                .and_then(Value::as_array)
                .and_then(|chain| chain.first())
                .and_then(|step| step.get("task"))
                .and_then(Value::as_str)
        })
        .unwrap_or("(no task text)");
    let agents = if agents.is_empty() {
        "subagent".to_owned()
    } else {
        agents.join(", ")
    };
    format!("{agents}: {}", truncate_chars(task, 120))
}

/// Stable approval id for a submitted plan. Changing the verification
/// contract is a material change, so it produces a fresh id.
pub fn plan_approval_id(plan: &str, verification: Option<&Verification>) -> String {
    let mut hasher = DefaultHasher::new();
    "plan".hash(&mut hasher);
    plan.hash(&mut hasher);
    if let Some(verification) = verification {
        "verification".hash(&mut hasher);
        verification.hash_key().hash(&mut hasher);
    }
    format!("p{:016x}", hasher.finish())
}

/// One-line human summary of a submitted plan.
pub fn plan_summary(plan: &str) -> String {
    let first = plan
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("submitted plan");
    format!("plan: {}", truncate_chars(first, 120))
}

/// Truncate on char boundaries, appending an ellipsis when shortened.
///
/// ```
/// use pir_builtin_mode::truncate_chars;
///
/// assert_eq!(truncate_chars("abcdef", 3), "abc…");
/// assert_eq!(truncate_chars("abc", 3), "abc");     // exactly max: unchanged
/// assert_eq!(truncate_chars("日本語です", 3), "日本語…"); // never splits a char
/// ```
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Block reason shown to the model while a plan approval is pending.
pub fn plan_approval_block_reason(id: &str) -> String {
    format!(
        "Plan approval pending ({id}). Run /approvals in the TUI, or `pir foreman approve {id}` \
         for a detached session. STOP now and end your turn: do not sleep, poll, or keep working. \
         The decision arrives as a follow-up message; after approval the session switches to \
         normal mode and you will be asked to implement the plan."
    )
}

/// Block reason shown to the model while a worker approval is pending.
pub fn approval_block_reason(id: &str, settings: ModeSettings) -> String {
    let approvals = if settings.approvals == ApprovalMode::Manual {
        "manual"
    } else {
        "auto"
    };
    format!(
        "Foreman approval pending ({id}, {approvals}). Run /approvals in the TUI, or \
         `pir foreman approve {id}` for a detached session. STOP now and end your turn: do not \
         sleep, poll, or keep working. After approval a follow-up message starts a new run; retry \
         the same delegation there."
    )
}

/// Apply the configured router/worktree policy to a subagent input.
pub fn apply_foreman_routing(input: &mut Value, settings: ModeSettings, worker: bool) {
    if worker && settings.worktree {
        input["isolation"] = Value::String("worktree".to_owned());
        input["worktreeMerge"] = Value::String("auto".to_owned());
    } else if !worker {
        input["isolation"] = Value::String("none".to_owned());
    }
    if settings.router == RouterMode::Process {
        input["executor"] = Value::String("process".to_owned());
    }
}

/// Force read-only tools (and no isolation/merge) on a plan-mode subagent.
pub fn force_readonly_subagent_tools(input: &mut Value) {
    let tools = Value::Array(
        PLAN_CHILD_TOOLS
            .iter()
            .map(|name| Value::String((*name).to_owned()))
            .collect(),
    );
    let force_one = |object: &mut serde_json::Map<String, Value>| {
        object.insert("tools".to_owned(), tools.clone());
        object.insert("isolation".to_owned(), Value::String("none".to_owned()));
        object.insert("worktreeMerge".to_owned(), Value::String("none".to_owned()));
    };
    if let Some(object) = input.as_object_mut() {
        force_one(object);
        for key in ["tasks", "chain"] {
            if let Some(items) = object.get_mut(key).and_then(Value::as_array_mut) {
                for item in items {
                    if let Some(item) = item.as_object_mut() {
                        force_one(item);
                    }
                }
            }
        }
    }
}

/// Force the reviewer child tool set on a reviewer-only subagent call.
pub fn force_reviewer_subagent_tools(input: &mut Value) {
    let tools = Value::Array(
        REVIEWER_CHILD_TOOLS
            .iter()
            .map(|name| Value::String((*name).to_owned()))
            .collect(),
    );
    let force_one = |object: &mut serde_json::Map<String, Value>| {
        object.insert("tools".to_owned(), tools.clone());
    };
    if let Some(object) = input.as_object_mut() {
        force_one(object);
        for key in ["tasks", "chain"] {
            if let Some(items) = object.get_mut(key).and_then(Value::as_array_mut) {
                for item in items {
                    if let Some(item) = item.as_object_mut() {
                        force_one(item);
                    }
                }
            }
        }
    }
}

/// Whether a tool is exposed to the model in `mode`.
///
/// `submit_plan` is only visible in plan mode; the rest of
/// [`MODE_ALLOWED_TOOLS`] are allowed in foreman and plan. Normal sessions
/// gate nothing except the plan-exit tool.
///
/// ```
/// use pir_builtin_mode::{tool_visible_in_mode, Mode};
///
/// // Foreman is read-only: `edit` disappears, `read` and `subagent` stay.
/// assert!(!tool_visible_in_mode(Mode::Foreman, "edit"));
/// assert!(tool_visible_in_mode(Mode::Foreman, "read"));
/// // `submit_plan` exists only in plan mode.
/// assert!(tool_visible_in_mode(Mode::Plan, "submit_plan"));
/// assert!(!tool_visible_in_mode(Mode::Foreman, "submit_plan"));
/// ```
pub fn tool_visible_in_mode(mode: Mode, tool: &str) -> bool {
    match mode {
        Mode::Normal => tool != SUBMIT_PLAN_TOOL,
        Mode::Foreman => MODE_ALLOWED_TOOLS.contains(&tool) && tool != SUBMIT_PLAN_TOOL,
        Mode::Plan => MODE_ALLOWED_TOOLS.contains(&tool),
    }
}

/// Whether a direct tool call is permitted in `mode` (the execution fence).
///
/// Enforcement mirrors visibility: a call the model cannot see is also
/// refused if it is issued anyway.
///
/// ```
/// use pir_builtin_mode::{tool_allowed_in_mode, Mode};
///
/// assert!(tool_allowed_in_mode(Mode::Normal, "edit"));
/// assert!(!tool_allowed_in_mode(Mode::Foreman, "edit"));
/// ```
pub fn tool_allowed_in_mode(mode: Mode, tool: &str) -> bool {
    match mode {
        Mode::Normal => true,
        Mode::Foreman | Mode::Plan => {
            MODE_ALLOWED_TOOLS.contains(&tool) && (tool != SUBMIT_PLAN_TOOL || mode == Mode::Plan)
        }
    }
}

/// Human label for an approval record's kind.
pub fn approval_kind_label(record: &ApprovalRecord) -> &'static str {
    match record.kind {
        crate::approvals::ApprovalKind::Plan => "plan",
        crate::approvals::ApprovalKind::Worker => "worker",
    }
}
