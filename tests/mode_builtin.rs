//! `/mode foreman` / `/mode normal`: the user-triggered foreman mode.
//!
//! The mode is implemented as a separate native builtin; these tests pin the
//! four contracts that keep it isolated from the existing subagent tool:
//!
//! 1. it registers a command, never a model-visible tool;
//! 2. foreman adds a prompt section and blocks every non-allowlisted direct
//!    tool, while normal leaves the payload untouched;
//! 3. mode state is per-plugin-instance, so worker child sessions start
//!    normal and keep their full tool surface;
//! 4. busy sessions and sessions without `subagent` cannot enter foreman.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};

use pir::plugins::services::{
    CustomMessage, DeliverAs, DialogOptions, MessageService, NotifyLevel, PluginServices,
    SelectOption, SendMessageOptions, SendUserMessageOptions, ServiceFuture, ServiceResult,
    SessionService, StatusService, ToolInfo, UiService, UserContent,
};
use pir::plugins::{
    instantiate_builtin, AgentPlugin, EventInterest, PluginHost, PluginRegistrations,
    RegistrationContext,
};

#[derive(Default)]
struct FakeStatus {
    cells: Mutex<Vec<(String, String)>>,
}

impl FakeStatus {
    fn text(&self, key: &str) -> Option<String> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(cell, _)| cell == key)
            .map(|(_, text)| text.clone())
    }
}

impl StatusService for FakeStatus {
    fn set_status(&self, key: &str, text: Option<&str>) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match text {
            Some(text) => {
                if let Some((_, slot)) = cells.iter_mut().find(|(cell, _)| cell == key) {
                    *slot = text.to_owned();
                } else {
                    cells.push((key.to_owned(), text.to_owned()));
                }
            }
            None => cells.retain(|(cell, _)| cell != key),
        }
    }
}

struct FakeUi {
    notices: Mutex<Vec<String>>,
    /// Prefix of the option value to select (e.g. "approve" or "deny").
    select_prefix: Mutex<Option<String>>,
    /// Per-call overrides consumed before `select_prefix` (review → approve).
    select_script: Mutex<Vec<String>>,
    select_values: Mutex<Vec<String>>,
    last_select_title: Mutex<Option<String>>,
    /// `(title, prefill)` of every editor dialog opened.
    editor_requests: Mutex<Vec<(String, String)>>,
}

impl Default for FakeUi {
    fn default() -> Self {
        Self {
            notices: Mutex::new(Vec::new()),
            select_prefix: Mutex::new(None),
            select_script: Mutex::new(Vec::new()),
            select_values: Mutex::new(Vec::new()),
            last_select_title: Mutex::new(None),
            editor_requests: Mutex::new(Vec::new()),
        }
    }
}

impl FakeUi {
    fn notices(&self) -> Vec<String> {
        self.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Select the first option whose value starts with `prefix`.
    fn selecting(prefix: &str) -> Arc<Self> {
        let ui = Arc::new(Self::default());
        *ui.select_prefix.lock().unwrap() = Some(prefix.to_owned());
        ui
    }

    /// Queue one prefix per dialog call, e.g. ["review", "approve"].
    fn selecting_script(prefixes: &[&str]) -> Arc<Self> {
        let ui = Arc::new(Self::default());
        *ui.select_script.lock().unwrap() = prefixes.iter().map(|p| (*p).to_owned()).collect();
        ui
    }
}

impl UiService for FakeUi {
    fn notify(&self, _level: NotifyLevel, message: &str) {
        self.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message.to_owned());
    }

    fn select(
        &self,
        title: &str,
        options: &[SelectOption],
        _dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        *self.last_select_title.lock().unwrap() = Some(title.to_owned());
        *self.select_values.lock().unwrap() = options.iter().map(|o| o.value.clone()).collect();
        let scripted = {
            let mut script = self.select_script.lock().unwrap();
            if script.is_empty() {
                None
            } else {
                Some(script.remove(0))
            }
        };
        let prefix = scripted.or_else(|| self.select_prefix.lock().unwrap().clone());
        let choice = prefix.and_then(|prefix| {
            options
                .iter()
                .find(|o| o.value.starts_with(&prefix))
                .map(|o| o.value.clone())
        });
        Box::pin(async move { Ok(choice) })
    }

    fn editor(
        &self,
        title: &str,
        prefill: Option<&str>,
        _dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        self.editor_requests
            .lock()
            .unwrap()
            .push((title.to_owned(), prefill.unwrap_or_default().to_owned()));
        // A dismissed read-only review (Esc).
        Box::pin(async { Ok(None) })
    }
}

/// Captures follow-up user messages sent by the approval flow.
#[derive(Default)]
struct FakeMessages {
    sent: Mutex<Vec<(String, Option<DeliverAs>)>>,
}

impl FakeMessages {
    fn sent(&self) -> Vec<(String, Option<DeliverAs>)> {
        self.sent.lock().unwrap().clone()
    }
}

impl MessageService for FakeMessages {
    fn send_user_message(&self, content: UserContent, options: SendUserMessageOptions) {
        let text = match content {
            UserContent::Text(text) => text,
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        self.sent.lock().unwrap().push((text, options.deliver_as));
    }
}

fn host_with_plan(
    session: Arc<FakeSession>,
    status: Arc<FakeStatus>,
    ui: Arc<FakeUi>,
    messages: Arc<FakeMessages>,
) -> (PluginHost, Arc<dyn AgentPlugin>) {
    let plugin = instantiate_builtin("mode").expect("mode builtin is compiled in");
    let host = PluginHost::build(None, vec![Arc::clone(&plugin)]);
    host.bind_context(
        PluginServices::new()
            .with_session(session)
            .with_status(status)
            .with_ui(ui)
            .with_messages(messages)
            .build(),
    );
    (host, plugin)
}

struct FakeSession {
    idle: AtomicBool,
    active: Vec<String>,
    all: Vec<ToolInfo>,
    path: Option<PathBuf>,
    entries: RwLock<Option<Value>>,
}

const ALL_TOOLS: &[&str] = &[
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
    "bash",
    "edit",
    "write",
    "codemode",
    "memory",
];

impl FakeSession {
    fn new(idle: bool, active: &[&str]) -> Arc<Self> {
        Self::build(idle, active, None)
    }

    fn with_path(idle: bool, active: &[&str], path: PathBuf) -> Arc<Self> {
        Self::build(idle, active, Some(path))
    }

    fn build(idle: bool, active: &[&str], path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            idle: AtomicBool::new(idle),
            active: active.iter().map(|name| (*name).to_owned()).collect(),
            all: ALL_TOOLS
                .iter()
                .map(|name| {
                    serde_json::from_value(json!({ "name": name })).expect("tool info fixture")
                })
                .collect(),
            path,
            entries: RwLock::new(None),
        })
    }

    fn set_idle(&self, idle: bool) {
        self.idle.store(idle, Ordering::SeqCst);
    }

    fn set_entries(&self, entries: Value) {
        *self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(entries);
    }
}

impl SessionService for FakeSession {
    fn is_idle(&self) -> bool {
        self.idle.load(Ordering::SeqCst)
    }

    fn active_tools(&self) -> Arc<[Arc<str>]> {
        self.active
            .iter()
            .map(|name| Arc::<str>::from(name.as_str()))
            .collect::<Vec<_>>()
            .into()
    }

    fn all_tools(&self) -> Arc<[ToolInfo]> {
        self.all.clone().into()
    }

    fn session_path(&self) -> Option<Arc<std::path::Path>> {
        self.path.clone().map(Arc::from)
    }

    fn entries(&self) -> Option<Arc<Value>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(Arc::new)
    }
}

fn host_with(
    session: Arc<FakeSession>,
    status: Arc<FakeStatus>,
) -> (PluginHost, Arc<dyn AgentPlugin>) {
    let plugin = instantiate_builtin("mode").expect("mode builtin is compiled in");
    let host = PluginHost::build(None, vec![Arc::clone(&plugin)]);
    host.bind_context(
        PluginServices::new()
            .with_session(session)
            .with_status(status)
            .build(),
    );
    (host, plugin)
}

fn host_with_ui(
    session: Arc<FakeSession>,
    status: Arc<FakeStatus>,
    ui: Arc<FakeUi>,
) -> (PluginHost, Arc<dyn AgentPlugin>) {
    let plugin = instantiate_builtin("mode").expect("mode builtin is compiled in");
    let host = PluginHost::build(None, vec![Arc::clone(&plugin)]);
    host.bind_context(
        PluginServices::new()
            .with_session(session)
            .with_status(status)
            .with_ui(ui)
            .build(),
    );
    (host, plugin)
}

async fn emit_prompt(host: &PluginHost, with_options: bool) -> Value {
    let mut payload = if with_options {
        json!({
            "type": "before_agent_start",
            "messageType": "user",
            "prompt": "do the task",
            "systemPrompt": "base prompt",
            "systemPromptOptions": { "sections": {} }
        })
    } else {
        json!({
            "type": "before_agent_start",
            "messageType": "user",
            "prompt": "do the task",
            "systemPrompt": "base prompt"
        })
    };
    let outcome = host
        .emit("before_agent_start", &mut payload)
        .await
        .expect("emit before_agent_start");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    payload
}

/// The handler result, where `before_agent_start` injections land (they are
/// merged into the outcome, not the mutable event payload).
async fn emit_prompt_result(host: &PluginHost) -> Value {
    let mut payload = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "do the task",
        "systemPrompt": "base prompt",
        "systemPromptOptions": { "sections": {} }
    });
    let outcome = host
        .emit("before_agent_start", &mut payload)
        .await
        .expect("emit before_agent_start");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome.result
}

async fn emit_tool(host: &PluginHost, tool: &str) -> Value {
    emit_tool_input(host, tool, "call-1", json!({})).await
}

async fn emit_tool_input(host: &PluginHost, tool: &str, call_id: &str, input: Value) -> Value {
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": call_id,
        "toolName": tool,
        "input": input
    });
    let outcome = host
        .emit("tool_call", &mut payload)
        .await
        .expect("emit tool_call");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome.result
}

fn worker_input() -> Value {
    json!({ "agent": "worker", "task": "write the file" })
}

fn reviewer_input() -> Value {
    json!({ "agent": "reviewer", "task": "review the result" })
}

fn settle_payload(messages: Vec<Value>) -> Value {
    json!({
        "type": "agent_before_settle",
        "outcome": "completed",
        "entries": [],
        "continue": false,
        "context": {
            "contextEntries": [],
            "contextMessages": messages.clone(),
            "llmMessages": messages,
            "pendingMessages": [],
            "canContinue": false
        }
    })
}

async fn emit_settle(host: &PluginHost, messages: Vec<Value>) -> Value {
    let mut payload = settle_payload(messages);
    let outcome = host
        .emit("agent_before_settle", &mut payload)
        .await
        .expect("emit agent_before_settle");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome.result
}

fn completed_turn(tool_results: Vec<Value>) -> Vec<Value> {
    let mut messages = vec![json!({
        "role": "user",
        "content": [{ "type": "text", "text": "do the task" }]
    })];
    messages.extend(tool_results);
    messages.push(json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "done" }],
        "stopReason": "stop"
    }));
    messages
}

fn reviewer_tool_result(call_id: &str, verdict: &str) -> Value {
    json!({
        "role": "toolResult",
        "toolCallId": call_id,
        "toolName": "subagent",
        "isError": false,
        "content": [{ "type": "text", "text": format!("reviewer says {verdict}") }],
        "details": {
            "mode": "single",
            "agentScope": "user",
            "projectAgentsDir": null,
            "results": [{
                "agent": "reviewer",
                "agentSource": "builtin",
                "task": "review the result",
                "exitCode": 0,
                "messages": [
                    {
                        "role": "user",
                        "content": [{ "type": "text", "text": "review" }]
                    },
                    {
                        "role": "assistant",
                        "content": [{ "type": "text", "text": verdict }]
                    }
                ],
                "usage": {},
                "stderr": null,
                "stopReason": "stop"
            }]
        }
    })
}

async fn gate_after_reviewer_text(text: &str) -> bool {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;
    let result = emit_settle(
        &host,
        completed_turn(vec![reviewer_tool_result("call-review", text)]),
    )
    .await;
    result["continue"] == true
}

#[test]
fn mode_builtin_registers_a_command_not_a_tool() {
    let plugin = instantiate_builtin("mode").expect("mode builtin is compiled in");
    let mut registrations = PluginRegistrations::default();
    let mut ctx = RegistrationContext::new(&mut registrations);
    plugin.register(&mut ctx).expect("register mode plugin");

    // The mode/foreman/approvals surfaces are commands, never model tools.
    // The one registered tool is `submit_plan`, the explicit plan-mode exit
    // surface; it stays hidden from the model unless plan mode is active.
    let tool_names: Vec<&str> = registrations
        .tools
        .iter()
        .map(|(def, _)| def.name.as_str())
        .collect();
    assert_eq!(tool_names, vec!["submit_plan"], "{tool_names:?}");
    assert_eq!(registrations.commands.len(), 3);
    let names: Vec<&str> = registrations
        .commands
        .iter()
        .map(|(command, _)| command.name.as_str())
        .collect();
    assert!(names.contains(&"mode"), "{names:?}");
    assert!(names.contains(&"foreman"), "{names:?}");
    assert!(names.contains(&"approvals"), "{names:?}");
    assert!(registrations
        .commands
        .iter()
        .all(|(_, backend)| matches!(backend, Some(pir::plugins::CommandBackend::Rust(_)))));
    assert_eq!(
        plugin.events(),
        EventInterest::List(vec![
            "session_start".to_owned(),
            "before_agent_start".to_owned(),
            "tool_call".to_owned(),
            "agent_before_settle".to_owned(),
        ])
    );
    assert!(
        !plugin.handles_event_now("agent_before_settle"),
        "normal mode must keep the settle fast path"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_hides_non_allowlisted_tool_declarations() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, plugin) = host_with(session, status);

    assert_eq!(
        plugin.hidden_tool_declarations(),
        vec!["submit_plan".to_owned()],
        "normal mode only hides the plan-exit tool"
    );

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    let hidden = plugin.hidden_tool_declarations();
    let mut sorted = hidden.clone();
    sorted.sort();
    assert_eq!(
        host.hidden_tool_declarations(),
        sorted,
        "the host aggregate must expose the plugin's filter"
    );
    for tool in ["bash", "edit", "write", "codemode", "memory"] {
        assert!(
            hidden.iter().any(|name| name == tool),
            "foreman must hide `{tool}` from the model: {hidden:?}"
        );
    }
    for tool in [
        "subagent",
        "read",
        "grep",
        "find",
        "ls",
        "tool_search",
        "todo",
        "bg_status",
        "bg_abort",
    ] {
        assert!(
            !hidden.iter().any(|name| name == tool),
            "foreman must keep `{tool}`: {hidden:?}"
        );
    }

    host.dispatch_command("mode", "normal")
        .await
        .expect("disable foreman");
    assert_eq!(
        plugin.hidden_tool_declarations(),
        vec!["submit_plan".to_owned()],
        "normal mode hides only the plan-exit tool"
    );
    assert_eq!(
        host.hidden_tool_declarations(),
        vec!["submit_plan".to_owned()],
        "the host aggregate must expose the plugin's filter"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn plan_mode_hides_mutations_and_blocks_execution_subagents() {
    let session = FakeSession::new(true, &["subagent", "read", "bash"]);
    let status = Arc::new(FakeStatus::default());
    let (host, plugin) = host_with(session, Arc::clone(&status));

    let plan = host
        .dispatch_command("mode", "plan")
        .await
        .expect("enable plan");
    assert!(plan.ok, "{plan:?}");
    assert_eq!(status.text("mode").as_deref(), Some("MODE: PLAN"));
    assert_eq!(
        status.text("mode_phase").as_deref(),
        Some("PHASE: PLANNING")
    );
    assert!(
        !host.has_handlers("agent_before_settle"),
        "plan mode must not install the reviewer gate"
    );

    let hidden = plugin.hidden_tool_declarations();
    assert!(hidden.iter().any(|name| name == "bash"), "{hidden:?}");
    assert!(!hidden.iter().any(|name| name == "read"), "{hidden:?}");
    assert!(!hidden.iter().any(|name| name == "subagent"), "{hidden:?}");

    let blocked = emit_tool(&host, "bash").await;
    assert_eq!(blocked["block"], true, "{blocked}");
    assert!(
        blocked["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("Plan mode"),
        "{blocked}"
    );

    let worker = emit_tool_input(
        &host,
        "subagent",
        "call-worker",
        json!({ "agent": "worker", "task": "write" }),
    )
    .await;
    assert_eq!(worker["block"], true, "{worker}");
    assert!(
        worker["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not read-only"),
        "{worker}"
    );

    let scout = emit_tool_input(
        &host,
        "subagent",
        "call-scout",
        json!({ "agent": "scout", "task": "inspect" }),
    )
    .await;
    assert_ne!(scout["block"], true, "{scout}");

    // HARD read-only fence: even an allowed scout gets its child tool list
    // replaced with read-only tools, so a bash-capable agent cannot mutate.
    let mut scout_payload = json!({
        "type": "tool_call",
        "toolCallId": "call-scout-2",
        "toolName": "subagent",
        "input": {
            "agent": "scout", "task": "inspect",
            "isolation": "worktree", "worktreeMerge": "auto",
            "tasks": [
                { "agent": "reviewer", "task": "look", "isolation": "worktree", "worktreeMerge": "auto" }
            ]
        },
    });
    let outcome = host
        .emit("tool_call", &mut scout_payload)
        .await
        .expect("emit scout");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let tools = scout_payload["input"]["tools"]
        .as_array()
        .expect("read-only tools injected");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t.as_str()).collect();
    assert_eq!(tool_names, vec!["read", "grep", "find", "ls"]);
    assert!(
        !tool_names.contains(&"bash"),
        "bash must be removed: {tool_names:?}"
    );
    // Worktree isolation/merge mutate the HOST (git worktree add / commit /
    // merge) regardless of the child's tool list. The plan fence must strip
    // them at the top level AND in every task, or a plan-mode subagent can
    // write through the isolation path.
    assert_eq!(scout_payload["input"]["isolation"], "none");
    assert_eq!(scout_payload["input"]["worktreeMerge"], "none");
    assert_eq!(scout_payload["input"]["tasks"][0]["isolation"], "none");
    assert_eq!(scout_payload["input"]["tasks"][0]["worktreeMerge"], "none");
    assert_eq!(
        scout_payload["input"]["tasks"][0]["tools"][0], "read",
        "each task's tools are overridden too"
    );

    host.dispatch_command("mode", "normal")
        .await
        .expect("leave plan");
    assert_eq!(status.text("mode"), None);
    assert_eq!(status.text("mode_phase"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_reviewer_only_call_loses_write_capable_tools() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    // Reviewer-only calls are exempt from manual approval, so the plugin must
    // strip write-capable tools even when the caller asks for them.
    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "call-reviewer-tools",
        "toolName": "subagent",
        "input": {
            "agent": "reviewer",
            "task": "verify the delivered file",
            "tools": ["read", "write", "edit", "bash"],
        },
    });
    let outcome = host
        .emit("tool_call", &mut payload)
        .await
        .expect("emit reviewer call");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let tools: Vec<&str> = payload["input"]["tools"]
        .as_array()
        .expect("tools overridden")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(tools, vec!["read", "grep", "find", "ls", "bash"]);
    assert!(!tools.contains(&"write"), "{tools:?}");
    assert!(!tools.contains(&"edit"), "{tools:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_structured_verdicts_win_over_text() {
    assert!(
        !gate_after_reviewer_text(r#"{"verdict":"PASS","evidence":"all checks pass"}"#).await,
        "structured PASS must finish"
    );
    assert!(
        gate_after_reviewer_text(r#"{"verdict":"FAIL","evidence":"PASS was claimed but wrong"}"#)
            .await,
        "structured FAIL must win even when the evidence contains PASS"
    );
    assert!(
        !gate_after_reviewer_text("```json\n{\"verdict\":\"PASS\",\"evidence\":\"ok\"}\n```").await,
        "a fenced JSON verdict must parse"
    );
    assert!(
        !gate_after_reviewer_text("PASS - plain text fallback").await,
        "plain PASS remains accepted"
    );
    assert!(
        gate_after_reviewer_text("FAIL - plain text fallback").await,
        "plain FAIL remains rejected"
    );
    assert!(
        !gate_after_reviewer_text("PASS {not valid json").await,
        "malformed JSON must fall back to the text verdict"
    );
    assert!(
        gate_after_reviewer_text("The review would not PASS; see issues").await,
        "negated PASS must never be read as a successful verdict"
    );
    assert!(
        gate_after_reviewer_text("not PASS").await,
        "leading negation must be rejected"
    );
    assert!(
        gate_after_reviewer_text("PASS criteria not met, verdict FAIL").await,
        "a leading PASS token with reversal markers on the same line is a FAIL"
    );
    assert!(
        gate_after_reviewer_text("PASS verdict FAIL after recheck").await,
        "a PASS followed by an explicit FAIL verdict must not be read as success"
    );
    // `visual` reports the evidence available to the reviewer; it must never
    // override the verdict itself.
    assert!(
        !gate_after_reviewer_text(
            r#"{"verdict":"PASS","visual":"not_checked","evidence":"no UI artifact"}"#
        )
        .await,
        "visual=not_checked must not downgrade a structured PASS"
    );
    assert!(
        gate_after_reviewer_text(
            r#"{"verdict":"FAIL","visual":"verified","evidence":"screenshot shows overlap"}"#
        )
        .await,
        "visual=verified must not upgrade a structured FAIL"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_gate_rejects_truncated_workers_and_reviewers() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    // A worker that hit the model's output limit produced a truncated file;
    // the reviewer gate must not treat that generation as verifiable.
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    let mut worker = reviewer_tool_result("call-worker", "partial output");
    worker["details"]["results"][0]["agent"] = json!("worker");
    worker["details"]["results"][0]["stopReason"] = json!("length");
    let result = emit_settle(&host, completed_turn(vec![worker])).await;
    assert_eq!(result["continue"], true, "{result}");
    assert!(
        result["entries"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("failed"),
        "a truncated worker must be reported as failed work: {result}"
    );

    // A reviewer that itself hit the output limit cannot return a trustworthy
    // verdict, even when its visible text starts with a JSON PASS.
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;
    let mut review = reviewer_tool_result(
        "call-review",
        r#"{"verdict":"PASS","visual":"not_checked","evidence":"looks fine"}"#,
    );
    review["details"]["results"][0]["stopReason"] = json!("length");
    let result = emit_settle(&host, completed_turn(vec![review])).await;
    assert_eq!(
        result["continue"], true,
        "a truncated reviewer must not satisfy the gate: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_switches_prompt_and_status_and_normal_restores_them() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(Arc::clone(&session), Arc::clone(&status));

    let current = host
        .dispatch_command("mode", "")
        .await
        .expect("mode status");
    assert!(current.ok);
    assert!(current.error.as_deref().unwrap_or("").contains("normal"));

    let enabled = host
        .dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    assert!(enabled.ok, "{enabled:?}");
    assert_eq!(status.text("mode").as_deref(), Some("MODE: FOREMAN"));

    let payload = emit_prompt(&host, true).await;
    let section = payload["systemPromptOptions"]["sections"]["foreman_mode"]
        .as_str()
        .unwrap_or_default();
    assert!(section.contains("<foreman_mode>"), "{section}");
    assert!(section.contains("`subagent`"), "{section}");
    assert!(section.contains("reviewer"), "{section}");
    assert!(section.contains("restricted"), "{section}");
    assert!(section.contains("an authorized smoke command"), "{section}");
    assert!(section.contains("first execution cycle"), "{section}");
    assert!(section.contains("independent execution"), "{section}");
    let rendered = pir::extensions::system_prompt_options::SystemPromptOptions::from_json(
        &payload["systemPromptOptions"],
    )
    .render(None);
    assert!(
        rendered.contains("<foreman_mode>"),
        "the options renderer must carry the foreman section: {rendered}"
    );

    let disabled = host
        .dispatch_command("mode", "normal")
        .await
        .expect("disable foreman");
    assert!(disabled.ok, "{disabled:?}");
    assert_eq!(status.text("mode"), None);

    let normal_payload = emit_prompt(&host, true).await;
    assert!(
        normal_payload["systemPromptOptions"]["sections"]
            .get("foreman_mode")
            .is_none(),
        "normal mode must not inject the foreman section: {normal_payload}"
    );

    let plan = host
        .dispatch_command("mode", "plan")
        .await
        .expect("plan command");
    assert!(plan.ok, "{plan:?}");
    assert_eq!(status.text("mode").as_deref(), Some("MODE: PLAN"));
    assert_eq!(
        status.text("mode_phase").as_deref(),
        Some("PHASE: PLANNING")
    );
    let plan_payload = emit_prompt(&host, true).await;
    let plan_section = plan_payload["systemPromptOptions"]["sections"]["plan_mode"]
        .as_str()
        .unwrap_or_default();
    assert!(plan_section.contains("<plan_mode>"), "{plan_section}");
    let plan_flat = plan_section
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        plan_flat.contains("startup and first execution cycle"),
        "plan verification must require executing runnable deliverables: {plan_section}"
    );
    assert!(
        plan_payload["systemPromptOptions"]["sections"]
            .get("foreman_mode")
            .is_none(),
        "plan mode must not inject the foreman section: {plan_payload}"
    );
    host.dispatch_command("mode", "normal")
        .await
        .expect("leave plan mode");
    assert_eq!(status.text("mode"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn mode_switch_injects_a_one_shot_transition_message() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    let first = emit_prompt_result(&host).await;
    let text = first["messages"]
        .as_array()
        .and_then(|messages| messages.first())
        .and_then(|message| message.pointer("/content/0/text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(text.contains("MODE CHANGED to foreman"), "{first}");
    assert!(text.contains("supersedes"), "{first}");

    // The note belongs to the transition, not to every later turn.
    let second = emit_prompt_result(&host).await;
    assert!(
        second.get("messages").is_none(),
        "the transition note must be one-shot: {second}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_approval_queue_journals_and_resumes_the_next_worker() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-approval-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual approvals");
    host.dispatch_command("foreman", "router process")
        .await
        .expect("process router");

    let mut payload = json!({
        "type": "tool_call",
        "toolCallId": "call-worker",
        "toolName": "subagent",
        "input": worker_input(),
    });
    let outcome = host
        .emit("tool_call", &mut payload)
        .await
        .expect("tool_call");
    assert_eq!(outcome.result["block"], true, "{:?}", outcome.result);
    let reason = outcome.result["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("approval pending"), "{reason}");

    let approvals = host
        .dispatch_command("approvals", "")
        .await
        .expect("approvals command");
    let text = approvals.error.unwrap_or_default();
    assert!(text.contains("Pending approvals"), "{text}");
    let mut journal_path = path.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    let journal =
        std::fs::read_to_string(std::path::PathBuf::from(journal_path)).expect("foreman journal");
    assert!(journal.contains("\"requested\""), "{journal}");
    let id = journal
        .lines()
        .find_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            (value.get("t") == Some(&Value::String("approval".to_owned()))
                && value.get("state") == Some(&Value::String("requested".to_owned())))
            .then(|| value.get("id")?.as_str().map(str::to_owned))
            .flatten()
        })
        .expect("approval id in journal");

    pir::foreman::decide_approval(&path, Some(&id), true).expect("approve");
    let mut start = json!({ "type": "session_start", "reason": "resume" });
    host.emit("session_start", &mut start)
        .await
        .expect("restore journal");
    let mut retry = json!({
        "type": "tool_call",
        "toolCallId": "call-worker-2",
        "toolName": "subagent",
        "input": worker_input(),
    });
    let outcome = host.emit("tool_call", &mut retry).await.expect("retry");
    assert_ne!(outcome.result["block"], true, "{:?}", outcome.result);
    assert_eq!(retry["input"]["executor"], "process");
}

#[tokio::test(flavor = "current_thread")]
async fn worker_approval_id_covers_the_request_authority() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-approval-id-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let (host, _plugin) = host_with(session, Arc::new(FakeStatus::default()));
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual approvals");

    // Same task text, different authority: a read-only approval must not
    // unlock a request that also carries `write`.
    for (call, tools) in [
        ("call-narrow", json!(["read"])),
        ("call-wide", json!(["read", "write"])),
    ] {
        let mut payload = json!({
            "type": "tool_call",
            "toolCallId": call,
            "toolName": "subagent",
            "input": { "agent": "worker", "task": "do the thing", "tools": tools },
        });
        let outcome = host
            .emit("tool_call", &mut payload)
            .await
            .expect("tool_call");
        assert_eq!(outcome.result["block"], true, "{:?}", outcome.result);
    }

    let mut journal_path = path.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    let journal =
        std::fs::read_to_string(std::path::PathBuf::from(journal_path)).expect("foreman journal");
    let ids: Vec<String> = journal
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            (value.get("t") == Some(&Value::String("approval".to_owned()))
                && value.get("state") == Some(&Value::String("requested".to_owned())))
            .then(|| value.get("id")?.as_str().map(str::to_owned))
            .flatten()
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "two distinct requests need two approvals: {ids:?}"
    );
    assert_ne!(ids[0], ids[1], "the tool list must affect the id: {ids:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn denying_one_approval_clears_an_unused_approval_allowance() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-deny-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual approvals");

    let mut first = json!({
        "type": "tool_call",
        "toolCallId": "call-a",
        "toolName": "subagent",
        "input": worker_input(),
    });
    assert_eq!(
        host.emit("tool_call", &mut first)
            .await
            .expect("first")
            .result["block"],
        true
    );
    let mut second = json!({
        "type": "tool_call",
        "toolCallId": "call-b",
        "toolName": "subagent",
        "input": { "agent": "worker", "task": "write a different file" },
    });
    assert_eq!(
        host.emit("tool_call", &mut second)
            .await
            .expect("second")
            .result["block"],
        true
    );

    let mut journal_path = path.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(std::path::PathBuf::from(journal_path)).expect("journal");
    let ids: Vec<String> = journal
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            (value.get("t") == Some(&Value::String("approval".to_owned()))
                && value.get("state") == Some(&Value::String("requested".to_owned())))
            .then(|| value.get("id")?.as_str().map(str::to_owned))
            .flatten()
        })
        .collect();
    assert_eq!(ids.len(), 2, "{journal}");

    // Approve the first, then deny the second. The deny must retire the
    // approval allowance left by the first decision.
    pir::foreman::decide_approval(&path, Some(&ids[0]), true).expect("approve first");
    pir::foreman::decide_approval(&path, Some(&ids[1]), false).expect("deny second");

    let mut start = json!({ "type": "session_start", "reason": "resume" });
    host.emit("session_start", &mut start)
        .await
        .expect("restore journal");

    let mut retry_denied = json!({
        "type": "tool_call",
        "toolCallId": "call-b-2",
        "toolName": "subagent",
        "input": { "agent": "worker", "task": "write a different file" },
    });
    let outcome = host
        .emit("tool_call", &mut retry_denied)
        .await
        .expect("retry denied");
    assert_eq!(
        outcome.result["block"], true,
        "a denied delegation must not inherit an unused approval: {:?}",
        outcome.result
    );
}

/// A denied worker request must be re-requestable: the stable approval id is
/// reopened instead of being deduped forever against a terminal record.
#[tokio::test(flavor = "current_thread")]
async fn denied_worker_can_be_requested_again_and_approved() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-reopen-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual");

    let input = worker_input();
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-1", input.clone()).await["block"],
        true,
        "first request must block"
    );
    let journal_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(".foreman.jsonl");
        std::path::PathBuf::from(p)
    };
    let read_id = || -> String {
        let journal = std::fs::read_to_string(&journal_path).expect("journal");
        // Read the LAST pending approval from either an approval event or a
        // compacted snapshot (decide_approval snapshots the journal).
        journal
            .lines()
            .rev()
            .find_map(|line| {
                let v: Value = serde_json::from_str(line).ok()?;
                let t = v.get("t")?.as_str()?;
                if t == "approval" && v.get("state") == Some(&Value::String("requested".to_owned()))
                {
                    return v.get("id")?.as_str().map(str::to_owned);
                }
                if t == "snapshot" {
                    if let Some(approvals) = v.get("approvals").and_then(Value::as_array) {
                        for record in approvals.iter().rev() {
                            if record.get("state") == Some(&Value::String("requested".to_owned())) {
                                if let Some(id) = record.get("id")?.as_str() {
                                    return Some(id.to_owned());
                                }
                            }
                        }
                    }
                }
                None
            })
            .expect("pending approval id")
    };
    let id = read_id();
    pir::foreman::decide_approval(&path, Some(&id), false).expect("deny");
    let mut start = json!({"type": "session_start", "reason": "resume"});
    host.emit("session_start", &mut start)
        .await
        .expect("restore after deny");

    // Re-request the SAME task after the deny: it must become pending again.
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-2", input.clone()).await["block"],
        true,
        "reopened request must still block until approved"
    );
    let id2 = read_id();
    assert_eq!(id2, id, "reopen reuses the stable approval id");
    pir::foreman::decide_approval(&path, Some(&id2), true).expect("approve reopened");
    let mut start2 = json!({"type": "session_start", "reason": "resume"});
    host.emit("session_start", &mut start2)
        .await
        .expect("restore after approve");
    assert_ne!(
        emit_tool_input(&host, "subagent", "call-3", input).await["block"],
        true,
        "approved reopened request must admit the worker"
    );
}

/// Approving worker A must not authorize a different worker B.
#[tokio::test(flavor = "current_thread")]
async fn approving_worker_a_does_not_admit_worker_b() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-bound-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual");

    let a = worker_input();
    let b = json!({ "agent": "worker", "task": "write the other file" });
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-a", a.clone()).await["block"],
        true
    );
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-b", b.clone()).await["block"],
        true
    );

    let journal_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(".foreman.jsonl");
        std::path::PathBuf::from(p)
    };
    let journal = std::fs::read_to_string(&journal_path).expect("journal");
    let id_a = journal
        .lines()
        .find_map(|line| {
            let v: Value = serde_json::from_str(line).ok()?;
            let summary = v.get("summary")?.as_str()?;
            (v.get("t") == Some(&Value::String("approval".to_owned()))
                && v.get("state") == Some(&Value::String("requested".to_owned()))
                && summary.contains("write the file"))
            .then(|| v.get("id")?.as_str().map(str::to_owned))
            .flatten()
        })
        .expect("id A");
    pir::foreman::decide_approval(&path, Some(&id_a), true).expect("approve A");
    let mut start = json!({"type": "session_start", "reason": "resume"});
    host.emit("session_start", &mut start)
        .await
        .expect("restore");

    // A is approved -> allowed; B is a different task -> still blocked.
    assert_ne!(
        emit_tool_input(&host, "subagent", "call-a2", a).await["block"],
        true,
        "A must pass"
    );
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-b2", b).await["block"],
        true,
        "B must not pass on A's approval"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_consumed_worker_approval_stays_consumed_after_resume() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-consume-resume-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let (host, _plugin) = host_with(session, Arc::new(FakeStatus::default()));
    host.dispatch_command("mode", "foreman")
        .await
        .expect("foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual");

    let input = worker_input();
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-a", input.clone()).await["block"],
        true
    );
    let journal_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(".foreman.jsonl");
        std::path::PathBuf::from(p)
    };
    let journal = std::fs::read_to_string(&journal_path).expect("journal");
    let id = journal
        .lines()
        .find_map(|line| {
            let v: Value = serde_json::from_str(line).ok()?;
            (v.get("t") == Some(&Value::String("approval".to_owned()))
                && v.get("state") == Some(&Value::String("requested".to_owned())))
            .then(|| v.get("id")?.as_str().map(str::to_owned))
            .flatten()
        })
        .expect("approval id");
    pir::foreman::decide_approval(&path, Some(&id), true).expect("approve");
    let mut start = json!({"type": "session_start", "reason": "resume"});
    host.emit("session_start", &mut start)
        .await
        .expect("restore");

    // The one-shot approval admits the matching delegation once.
    assert_ne!(
        emit_tool_input(&host, "subagent", "call-a2", input.clone()).await["block"],
        true,
        "the approved delegation must run"
    );

    // A restart replays the journal: the persisted consumption must win over
    // the old Approved snapshot, so the same delegation is gated again.
    let mut restart = json!({"type": "session_start", "reason": "resume"});
    host.emit("session_start", &mut restart)
        .await
        .expect("restore after consumption");
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-a3", input).await["block"],
        true,
        "a consumed approval must not be re-armed by a replay"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// approve-all (a single CLI decision over all pending) must grant EVERY
/// pending worker, not just the last one. Previously `allow_next_worker_id`
/// was a single slot, so the second approval overwrote the first and the
/// first worker was needlessly re-prompted.
#[tokio::test(flavor = "current_thread")]
async fn approving_all_pending_workers_grants_each_independently() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-approveall-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("foreman");
    host.dispatch_command("foreman", "approvals manual")
        .await
        .expect("manual");

    let a = worker_input();
    let b = json!({ "agent": "worker", "task": "write the other file" });
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-a", a.clone()).await["block"],
        true
    );
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-b", b.clone()).await["block"],
        true
    );

    // One decision over ALL pending approvals (None = every pending id).
    pir::foreman::decide_approval(&path, None, true).expect("approve all");
    let mut start = json!({"type": "session_start", "reason": "resume"});
    host.emit("session_start", &mut start)
        .await
        .expect("restore");

    // Both distinct workers pass, each consuming its OWN one-shot allowance.
    assert_ne!(
        emit_tool_input(&host, "subagent", "call-a2", a.clone()).await["block"],
        true,
        "worker A must be admitted by approve-all"
    );
    assert_ne!(
        emit_tool_input(&host, "subagent", "call-b2", b).await["block"],
        true,
        "worker B must also be admitted by approve-all"
    );
    // The allowance is one-shot: a second delegation of A needs a new approval.
    assert_eq!(
        emit_tool_input(&host, "subagent", "call-a3", a).await["block"],
        true,
        "a consumed allowance must not admit the worker twice"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_blocks_direct_mutation_and_allows_read_only_and_subagent() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    for tool in [
        "bash",
        "powershell",
        "edit",
        "write",
        "codemode",
        "ctx_execute",
        "ctx_execute_file",
        "memory",
    ] {
        let result = emit_tool(&host, tool).await;
        assert_eq!(
            result["block"], true,
            "foreman must block direct tool `{tool}`: {result}"
        );
        assert!(
            result["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("Delegate"),
            "block reason should point at delegation: {result}"
        );
    }

    for tool in [
        "subagent",
        "read",
        "grep",
        "find",
        "ls",
        "tool_search",
        "todo",
        "bg_status",
        "bg_abort",
    ] {
        let result = emit_tool(&host, tool).await;
        assert_ne!(
            result["block"], true,
            "foreman must allow `{tool}`: {result}"
        );
    }

    host.dispatch_command("mode", "normal")
        .await
        .expect("disable foreman");
    let result = emit_tool(&host, "bash").await;
    assert_ne!(
        result["block"], true,
        "normal mode must not block bash: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn busy_or_missing_subagent_rejects_foreman() {
    let busy = FakeSession::new(false, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (busy_host, _plugin) = host_with(Arc::clone(&busy), Arc::clone(&status));
    let outcome = busy_host
        .dispatch_command("mode", "foreman")
        .await
        .expect("mode command");
    assert!(!outcome.ok);
    assert!(outcome
        .error
        .unwrap_or_default()
        .contains("run is in progress"));
    assert_eq!(status.text("mode"), None);

    busy.set_idle(true);
    let no_subagent = FakeSession::new(true, &["read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(no_subagent, Arc::clone(&status));
    let outcome = host
        .dispatch_command("mode", "foreman")
        .await
        .expect("mode command");
    assert!(!outcome.ok);
    assert!(outcome.error.unwrap_or_default().contains("subagent"));
    assert_eq!(status.text("mode"), None);
}

#[tokio::test(flavor = "current_thread")]
async fn session_start_resets_and_prompt_fallback_appends() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, Arc::clone(&status));

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    assert_eq!(status.text("mode").as_deref(), Some("MODE: FOREMAN"));

    let mut start = json!({ "type": "session_start", "reason": "new" });
    host.emit("session_start", &mut start)
        .await
        .expect("emit session_start");
    assert_eq!(status.text("mode"), None);
    let payload = emit_prompt(&host, true).await;
    assert!(
        payload["systemPromptOptions"]["sections"]
            .get("foreman_mode")
            .is_none(),
        "a new session must start in normal mode"
    );

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman again");
    let mut fallback = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "do the task",
        "systemPrompt": "base prompt"
    });
    let outcome = host
        .emit("before_agent_start", &mut fallback)
        .await
        .expect("emit fallback prompt");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let prompt = outcome.result["systemPrompt"].as_str().unwrap_or_default();
    assert!(prompt.contains("base prompt"), "{prompt}");
    assert!(prompt.contains("<foreman_mode>"), "{prompt}");
}

#[tokio::test(flavor = "current_thread")]
async fn reload_and_shutdown_clear_mode_and_state_is_instance_local() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, plugin) = host_with(session, Arc::clone(&status));

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    plugin.on_reload().await.expect("reload mode plugin");
    assert_eq!(status.text("mode"), None);
    let payload = emit_prompt(&host, true).await;
    assert!(payload["systemPromptOptions"]["sections"]
        .get("foreman_mode")
        .is_none());

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman again");
    plugin.on_shutdown().await.expect("shutdown mode plugin");
    assert_eq!(status.text("mode"), None);
    let payload = emit_prompt(&host, true).await;
    assert!(payload["systemPromptOptions"]["sections"]
        .get("foreman_mode")
        .is_none());

    // Two builtin instances model the parent and a worker child: enabling
    // foreman on one must not affect the other.
    let session_a = FakeSession::new(true, &["subagent", "read"]);
    let status_a = Arc::new(FakeStatus::default());
    let (host_a, _) = host_with(session_a, status_a);
    let session_b = FakeSession::new(true, &["subagent", "read"]);
    let status_b = Arc::new(FakeStatus::default());
    let (host_b, _) = host_with(session_b, status_b);

    host_a
        .dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman on A");
    let payload_a = emit_prompt(&host_a, true).await;
    assert!(payload_a["systemPromptOptions"]["sections"]
        .get("foreman_mode")
        .is_some());
    let payload_b = emit_prompt(&host_b, true).await;
    assert!(
        payload_b["systemPromptOptions"]["sections"]
            .get("foreman_mode")
            .is_none(),
        "a separate plugin instance (worker child) must start normal"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreman_gate_requires_a_reviewer_after_a_worker() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let ui = Arc::new(FakeUi::default());
    let (host, _plugin) = host_with_ui(session, Arc::clone(&status), Arc::clone(&ui));

    assert!(
        !host.has_handlers("agent_before_settle"),
        "normal mode must not announce a settle handler"
    );
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    assert!(
        host.has_handlers("agent_before_settle"),
        "foreman must announce the settle gate"
    );

    // A foreman turn that never delegated has nothing to verify.
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_ne!(result["continue"], true, "{result}");

    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_eq!(result["continue"], true, "{result}");
    let entry = &result["entries"][0];
    assert_eq!(entry["type"], "custom_message", "{result}");
    assert_eq!(entry["customType"], "foreman_gate", "{result}");
    assert_eq!(entry["display"], true, "{result}");
    assert!(
        entry["content"]
            .as_str()
            .unwrap_or_default()
            .contains("reviewer"),
        "{result}"
    );
    let nudge = entry["content"].as_str().unwrap_or_default();
    assert!(nudge.contains("an authorized smoke command"), "{result}");
    assert!(nudge.contains("first execution cycle"), "{result}");
    assert_eq!(
        ui.notices()
            .iter()
            .filter(|notice| notice.contains("FOREMAN GATE"))
            .count(),
        1,
        "the first gate nudge must be visible to the user: {:?}",
        ui.notices()
    );

    // A passing reviewer ends the gate without another notice.
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;
    let result = emit_settle(
        &host,
        completed_turn(vec![reviewer_tool_result("call-review", "PASS")]),
    )
    .await;
    assert_ne!(result["continue"], true, "{result}");
    assert_eq!(
        ui.notices()
            .iter()
            .filter(|notice| notice.contains("FOREMAN GATE"))
            .count(),
        1,
        "a passing reviewer must not add another gate notice: {:?}",
        ui.notices()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_pass_after_a_worker_allows_settle() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;

    let result = emit_settle(
        &host,
        completed_turn(vec![reviewer_tool_result(
            "call-review",
            "PASS\nEvidence: the worker output is correct.",
        )]),
    )
    .await;
    assert_ne!(result["continue"], true, "{result}");
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_pass_in_a_mixed_chain_cannot_verify_the_worker() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    emit_tool_input(
        &host,
        "subagent",
        "call-chain",
        json!({
            "chain": [
                { "agent": "worker", "task": "write the file" },
                { "agent": "reviewer", "task": "review the result" }
            ]
        }),
    )
    .await;

    let result = emit_settle(
        &host,
        completed_turn(vec![reviewer_tool_result("call-chain", "PASS")]),
    )
    .await;
    // Both agents share one call id, so the PASS could cover work that ran
    // concurrently (parallel) or afterwards (a reviewer-first chain). The
    // foreman contract requires a separate reviewer call; a mixed call must
    // not settle as verified.
    assert_eq!(
        result["continue"], true,
        "a mixed worker+reviewer call must stay gated: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_fail_error_and_stderr_keep_the_gate() {
    let mut failing = reviewer_tool_result("call-review", "FAIL\nEvidence: broken.");
    let mut errored = reviewer_tool_result("call-review", "PASS");
    errored["isError"] = json!(true);
    let mut stderr = reviewer_tool_result("call-review", "PASS");
    stderr["details"]["results"][0]["stderr"] = json!("child crashed");

    for (label, reviewer_result) in [
        ("fail", std::mem::take(&mut failing)),
        ("error", std::mem::take(&mut errored)),
        ("stderr", std::mem::take(&mut stderr)),
    ] {
        let session = FakeSession::new(true, &["subagent", "read"]);
        let status = Arc::new(FakeStatus::default());
        let (host, _plugin) = host_with(session, status);
        host.dispatch_command("mode", "foreman")
            .await
            .expect("enable foreman");
        emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
        emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;
        let result = emit_settle(&host, completed_turn(vec![reviewer_result])).await;
        assert_eq!(result["continue"], true, "{label}: {result}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_first_frame_failure_keeps_the_gate() {
    assert!(
        gate_after_reviewer_text(
            r#"{"verdict":"FAIL","visual":"not_checked","evidence":"node first_frame.cjs first-frame broken.html exited 1: the first menu frame read particles.length before initialization; syntax checks passed."}"#,
        )
        .await,
        "a failed runtime check must not settle even when syntax passes"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_result_without_details_falls_back_to_content() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;

    let result = emit_settle(
        &host,
        completed_turn(vec![json!({
            "role": "toolResult",
            "toolCallId": "call-review",
            "toolName": "subagent",
            "isError": false,
            "content": [{ "type": "text", "text": "PASS - looks good" }]
        })]),
    )
    .await;
    assert_ne!(result["continue"], true, "{result}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_worker_invalidates_an_earlier_reviewer_pass() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    emit_tool_input(&host, "subagent", "call-worker-1", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;
    let passed = completed_turn(vec![reviewer_tool_result("call-review", "PASS")]);
    let result = emit_settle(&host, passed.clone()).await;
    assert_ne!(result["continue"], true, "{result}");

    emit_tool_input(&host, "subagent", "call-worker-2", worker_input()).await;
    let result = emit_settle(&host, passed).await;
    assert_eq!(result["continue"], true, "{result}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_fresh_reviewer_in_the_same_generation_clears_an_earlier_pass() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review-1", reviewer_input()).await;
    let passed = completed_turn(vec![reviewer_tool_result("call-review-1", "PASS")]);
    let result = emit_settle(&host, passed.clone()).await;
    assert_ne!(result["continue"], true, "the first PASS settles: {result}");

    // A second reviewer in the SAME worker generation re-opens the verdict:
    // its FAIL must not be masked by the persisted PASS above.
    emit_tool_input(&host, "subagent", "call-review-2", reviewer_input()).await;
    let failed = completed_turn(vec![reviewer_tool_result("call-review-2", "FAIL")]);
    let result = emit_settle(&host, failed).await;
    assert_eq!(
        result["continue"], true,
        "a fresh reviewer's FAIL must re-open the gate: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_before_any_worker_cannot_satisfy_a_later_worker_gate() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");

    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    let result = emit_settle(
        &host,
        completed_turn(vec![reviewer_tool_result("call-review", "PASS")]),
    )
    .await;
    assert_eq!(result["continue"], true, "{result}");
}

#[tokio::test(flavor = "current_thread")]
async fn normal_mode_and_gate_resets_clear_the_settle_gate() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let (host, plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_eq!(result["continue"], true, "{result}");

    host.dispatch_command("mode", "normal")
        .await
        .expect("disable foreman");
    assert!(
        !host.has_handlers("agent_before_settle"),
        "normal mode must restore the settle fast path"
    );
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_ne!(result["continue"], true, "{result}");

    // Re-entering foreman starts a fresh gate budget.
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman again");
    emit_tool_input(&host, "subagent", "call-worker-2", worker_input()).await;
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_eq!(result["continue"], true, "{result}");

    // session_start also resets mode and gate state.
    let mut start = json!({ "type": "session_start", "reason": "new" });
    host.emit("session_start", &mut start)
        .await
        .expect("emit session_start");
    assert!(!host.has_handlers("agent_before_settle"));
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman after session_start");
    emit_tool_input(&host, "subagent", "call-worker-3", worker_input()).await;
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_eq!(result["continue"], true, "{result}");

    // Reload and shutdown reset the plugin as well.
    plugin.on_reload().await.expect("reload mode plugin");
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman after reload");
    emit_tool_input(&host, "subagent", "call-worker-4", worker_input()).await;
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_eq!(result["continue"], true, "{result}");
    plugin.on_shutdown().await.expect("shutdown mode plugin");
    assert!(!host.has_handlers("agent_before_settle"));
}

#[tokio::test(flavor = "current_thread")]
async fn settle_gate_stops_after_three_nudges_and_marks_unverified() {
    let session = FakeSession::new(true, &["subagent", "read"]);
    let status = Arc::new(FakeStatus::default());
    let ui = Arc::new(FakeUi::default());
    let (host, _plugin) = host_with_ui(session, Arc::clone(&status), Arc::clone(&ui));
    host.dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    assert_eq!(
        status.text("mode_phase").as_deref(),
        Some("PHASE: DELEGATING")
    );

    for nudge in 1..=3 {
        let result = emit_settle(&host, completed_turn(Vec::new())).await;
        assert_eq!(result["continue"], true, "nudge {nudge}: {result}");
        assert_eq!(
            status.text("mode_phase").as_deref(),
            Some("PHASE: REWORK"),
            "nudge {nudge}"
        );
    }
    let result = emit_settle(&host, completed_turn(Vec::new())).await;
    assert_ne!(
        result["continue"], true,
        "the gate must stop after 3 nudges: {result}"
    );
    assert_eq!(
        status.text("mode_phase").as_deref(),
        Some("PHASE: UNVERIFIED")
    );
    assert_eq!(
        ui.notices()
            .iter()
            .filter(|notice| notice.contains("UNVERIFIED"))
            .count(),
        1,
        "the cap warning must be visible exactly once: {:?}",
        ui.notices()
    );
}

fn mode_sidecar(path: &std::path::Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".mode.json");
    PathBuf::from(os)
}

#[tokio::test(flavor = "current_thread")]
async fn persisted_mode_restores_and_rebuilds_the_gate_on_resume() {
    let dir = std::env::temp_dir().join(format!(
        "pir-mode-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("session.jsonl");
    let sidecar = mode_sidecar(&path);

    let session1 = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status1 = Arc::new(FakeStatus::default());
    let (host1, _plugin1) = host_with(session1, status1);
    host1
        .dispatch_command("mode", "foreman")
        .await
        .expect("enable foreman");
    let persisted = std::fs::read_to_string(&sidecar).expect("read mode sidecar");
    assert!(persisted.contains("\"foreman\""), "{persisted}");
    emit_tool_input(&host1, "subagent", "call-worker", worker_input()).await;

    // Resume: a fresh plugin instance sees the same session path and the
    // journaled worker call, so it restores foreman and the pending gate.
    let session2 = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    session2.set_entries(json!([{
        "messages": [{
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call-worker",
                "name": "subagent",
                "arguments": { "agent": "worker", "task": "write" }
            }]
        }]
    }]));
    let status2 = Arc::new(FakeStatus::default());
    let (host2, plugin2) = host_with(session2, Arc::clone(&status2));
    assert_eq!(status2.text("mode").as_deref(), Some("MODE: FOREMAN"));
    assert_eq!(
        status2.text("mode_phase").as_deref(),
        Some("PHASE: DELEGATING")
    );
    assert!(plugin2
        .hidden_tool_declarations()
        .iter()
        .any(|name| name == "bash"));

    let mut start = json!({ "type": "session_start", "reason": "resume" });
    host2
        .emit("session_start", &mut start)
        .await
        .expect("emit session_start");
    let result = emit_settle(&host2, completed_turn(Vec::new())).await;
    assert_eq!(
        result["continue"], true,
        "a resumed worker must still require a reviewer: {result}"
    );

    // Plan and normal persist too.
    host1
        .dispatch_command("mode", "plan")
        .await
        .expect("enable plan");
    let session3 = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status3 = Arc::new(FakeStatus::default());
    let (_host3, _plugin3) = host_with(session3, Arc::clone(&status3));
    assert_eq!(status3.text("mode").as_deref(), Some("MODE: PLAN"));

    host1
        .dispatch_command("mode", "normal")
        .await
        .expect("leave plan");
    let session4 = FakeSession::with_path(true, &["subagent", "read"], path);
    let status4 = Arc::new(FakeStatus::default());
    let (_host4, _plugin4) = host_with(session4, Arc::clone(&status4));
    assert_eq!(status4.text("mode"), None);
    assert_eq!(status4.text("mode_phase"), None);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Plan mode: submit → approval → execution handoff
// ---------------------------------------------------------------------------

fn plan_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-plan-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn submit_plan_input(plan: &str) -> Value {
    json!({ "plan": plan })
}

fn submit_plan_input_with_verification(plan: &str, verification: &str) -> Value {
    json!({ "plan": plan, "verification": [verification] })
}

/// A successful shell command: assistant toolCall paired with a zero-exit
/// toolResult, matching the generic evidence rules used by the contract gate.
fn successful_verification_turn(call_id: &str) -> Vec<Value> {
    successful_verification_turn_with_command(call_id, "python -m unittest")
}

fn successful_verification_turn_with_command(call_id: &str, command: &str) -> Vec<Value> {
    vec![
        json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": call_id,
                "name": "bash",
                "arguments": { "command": command }
            }],
            "stopReason": "toolUse"
        }),
        json!({
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": "bash",
            "isError": false,
            "content": [{ "type": "text", "text": "Ran 3 tests OK\nCommand exited with code 0" }],
            "details": { "exitCode": 0 }
        }),
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "done" }],
            "stopReason": "stop"
        }),
    ]
}

fn plan_approval_id_from_journal(path: &std::path::Path) -> String {
    let mut os = path.as_os_str().to_owned();
    os.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(PathBuf::from(os)).expect("foreman journal");
    journal
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            (value.get("t") == Some(&Value::String("approval".to_owned()))
                && value.get("state") == Some(&Value::String("requested".to_owned())))
            .then(|| value.get("id")?.as_str().map(str::to_owned))
            .flatten()
        })
        .find(|id| id.starts_with('p'))
        .expect("plan approval id in journal")
}

#[tokio::test(flavor = "current_thread")]
async fn plan_submission_blocks_and_journals_a_plan_approval() {
    let dir = plan_dir("submit");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    let plan = "1. read src/lib.rs\n2. add a helper\n3. run tests";
    let outcome = emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification(plan, "cargo test"),
    )
    .await;
    assert_eq!(outcome["block"], true, "{outcome}");
    let reason = outcome["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("Plan approval pending"), "{reason}");

    let mut os = path.as_os_str().to_owned();
    os.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(PathBuf::from(os)).expect("journal");
    assert!(journal.contains("plan_submitted"), "{journal}");
    assert!(journal.contains("read src/lib.rs"), "{journal}");
    let id = plan_approval_id_from_journal(&path);
    assert!(id.starts_with('p'), "{id}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn approving_a_plan_switches_to_normal_and_sends_an_execution_follow_up() {
    let dir = plan_dir("approve");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let ui = FakeUi::selecting("approve");
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, Arc::clone(&status), ui, Arc::clone(&messages));

    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    let plan = "1. change foo\n2. verify bar";
    let outcome = emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification(plan, "cargo test"),
    )
    .await;
    assert_eq!(outcome["block"], true, "{outcome}");

    // The session is idle in tests, so /approvals shows the dialog and our
    // FakeUi picks the first "approve" option.
    let result = host
        .dispatch_command("approvals", "")
        .await
        .expect("approvals command");
    assert!(result.ok, "{result:?}");
    let text = result.error.unwrap_or_default();
    assert!(text.contains("Approved plan"), "{text}");

    // Approval transitions out of plan mode.
    assert_eq!(status.text("mode"), None, "plan badge cleared on approval");

    // A follow-up user message carrying the approved plan was queued.
    let sent = messages.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].0.contains("PLAN APPROVED"), "{}", sent[0].0);
    assert!(sent[0].0.contains("change foo"), "{}", sent[0].0);
    assert_eq!(sent[0].1, Some(DeliverAs::FollowUp));

    // The journal records the approved decision.
    let mut os = path.as_os_str().to_owned();
    os.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(PathBuf::from(os)).expect("journal");
    assert!(journal.contains("\"approved\""), "{journal}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn denying_a_plan_keeps_plan_mode_and_asks_for_revision() {
    let dir = plan_dir("deny");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let ui = FakeUi::selecting("deny");
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, Arc::clone(&status), ui, Arc::clone(&messages));

    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    let plan = "1. do the thing";
    emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification(plan, "cargo test"),
    )
    .await;

    host.dispatch_command("approvals", "")
        .await
        .expect("approvals command");
    assert_eq!(status.text("mode"), Some("MODE: PLAN".to_owned()));
    let sent = messages.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].0.contains("PLAN DENIED"), "{}", sent[0].0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn submit_plan_is_hidden_outside_plan_mode_and_rejects_empty_plans() {
    let dir = plan_dir("hidden");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path);
    let status = Arc::new(FakeStatus::default());
    let (host, plugin) = host_with(session, status);

    // Normal mode hides submit_plan from the model-facing declarations.
    let hidden = plugin.hidden_tool_declarations();
    assert!(
        hidden.iter().any(|name| name == "submit_plan"),
        "submit_plan hidden in normal mode: {hidden:?}"
    );

    // Calling it in normal mode is blocked as an unlisted tool.
    let outcome = emit_tool_input(&host, "submit_plan", "call-1", submit_plan_input("x")).await;
    assert_eq!(outcome["block"], true, "{outcome}");

    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    // In plan mode it is visible (not hidden).
    let hidden = plugin.hidden_tool_declarations();
    assert!(
        !hidden.iter().any(|name| name == "submit_plan"),
        "submit_plan visible in plan mode: {hidden:?}"
    );
    // Empty plans are rejected.
    let outcome = emit_tool_input(&host, "submit_plan", "call-2", submit_plan_input("   ")).await;
    assert_eq!(outcome["block"], true, "{outcome}");
    assert!(outcome["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("non-empty"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn submit_plan_with_verification_journals_it_and_changes_the_approval_id() {
    let dir = plan_dir("verify-submit");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    let plan = "1. implement\n2. check";
    let plain_id = {
        emit_tool_input(
            &host,
            "submit_plan",
            "call-plain",
            submit_plan_input_with_verification(plan, "cargo test"),
        )
        .await;
        plan_approval_id_from_journal(&path)
    };
    // A different verification clause is a materially different plan.
    host.dispatch_command("mode", "plan").await.ok();
    let outcome = emit_tool_input(
        &host,
        "submit_plan",
        "call-verify",
        submit_plan_input_with_verification(plan, "run the test suite"),
    )
    .await;
    assert_eq!(outcome["block"], true, "{outcome}");

    let mut os = path.as_os_str().to_owned();
    os.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(PathBuf::from(os)).expect("journal");
    assert!(journal.contains("run the test suite"), "{journal}");
    assert!(journal.contains("plan_submitted"), "{journal}");
    // Both approvals are journaled and distinct (different verification).
    let ids: Vec<String> = journal
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            if value.get("t") == Some(&Value::String("approval".to_owned()))
                && value.get("state") == Some(&Value::String("requested".to_owned()))
            {
                value.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .filter(|id| id.starts_with('p'))
        .collect();
    assert!(ids.contains(&plain_id), "{ids:?}");
    assert!(ids.iter().any(|id| id != &plain_id), "{ids:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Helper: drive an approved plan (with verification) through /approvals.
async fn approve_plan_with_contract(
    dir: &std::path::Path,
    plan: &str,
    verification: &str,
) -> (PluginHost, Arc<FakeStatus>) {
    approve_plan_with_verification_value(
        dir,
        plan,
        submit_plan_input_with_verification(plan, verification)["verification"].clone(),
    )
    .await
}

async fn approve_plan_with_verification_value(
    dir: &std::path::Path,
    plan: &str,
    verification: Value,
) -> (PluginHost, Arc<FakeStatus>) {
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read", "bash"], path);
    // Approval can enumerate an (empty) transcript, so the baseline is a
    // complete empty set rather than "unknown" (which fail-closes the gate).
    session.set_entries(json!([]));
    let status = Arc::new(FakeStatus::default());
    let ui = FakeUi::selecting("approve");
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, Arc::clone(&status), ui, messages);
    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    let outcome = emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        json!({ "plan": plan, "verification": verification }),
    )
    .await;
    assert_eq!(outcome["block"], true, "{outcome}");
    let result = host
        .dispatch_command("approvals", "")
        .await
        .expect("approvals command");
    assert!(result.ok, "{result:?}");
    (host, status)
}

#[tokio::test(flavor = "current_thread")]
async fn approvals_offer_a_read_only_plan_review() {
    let dir = plan_dir("review");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read", "bash"], path);
    session.set_entries(json!([]));
    let status = Arc::new(FakeStatus::default());
    // First menu call opens the review, the second approves the plan.
    let ui = FakeUi::selecting_script(&["review", "approve"]);
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, Arc::clone(&status), ui.clone(), messages);
    host.dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    let outcome = emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification("1. build the thing\n2. verify it", "cargo test"),
    )
    .await;
    assert_eq!(outcome["block"], true, "{outcome}");

    let result = host
        .dispatch_command("approvals", "")
        .await
        .expect("approvals command");
    assert!(result.ok, "{result:?}");
    let text = result.error.unwrap_or_default();
    assert!(text.contains("Approved plan"), "{text}");

    let requests = ui
        .editor_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].0.contains("Plan "), "{}", requests[0].0);
    assert!(
        requests[0].1.contains("build the thing"),
        "{}",
        requests[0].1
    );
    assert!(
        requests[0].1.contains("Verification commands:"),
        "{}",
        requests[0].1
    );
    assert!(requests[0].1.contains("cargo test"), "{}", requests[0].1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn approving_a_plan_keeps_a_contract_and_injects_it_into_normal_prompts() {
    let dir = plan_dir("contract-prompt");
    let (host, status) = approve_plan_with_contract(
        &dir,
        "1. build the thing\n2. verify it",
        "python -m unittest",
    )
    .await;
    assert_eq!(status.text("mode"), None, "normal mode restored");

    let payload = emit_prompt(&host, true).await;
    let sections = &payload["systemPromptOptions"]["sections"];
    let contract = sections["execution_contract"].as_str().unwrap_or_default();
    assert!(contract.contains("build the thing"), "{contract}");
    assert!(contract.contains("python -m unittest"), "{contract}");
    assert!(
        contract.contains("real tool result"),
        "evidence requirement is explicit: {contract}"
    );

    let mut os = dir.join("session.jsonl").as_os_str().to_owned();
    os.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(PathBuf::from(os)).expect("journal");
    assert!(journal.contains("active_contract"), "{journal}");
    assert!(journal.contains("python -m unittest"), "{journal}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_verified_contract_stops_being_injected_as_active_work() {
    let dir = plan_dir("contract-complete-prompt");
    let (host, _status) = approve_plan_with_contract(
        &dir,
        "1. build the thing\n2. verify it",
        "python -m unittest",
    )
    .await;

    // Before evidence, the contract is active and the prompt says so.
    let pending = emit_prompt(&host, true).await;
    assert!(
        pending["systemPromptOptions"]["sections"]["execution_contract"]
            .as_str()
            .unwrap_or_default()
            .contains("build the thing")
    );

    let passed = emit_settle(&host, successful_verification_turn("call-verify")).await;
    assert_ne!(passed["continue"], true, "verification passes: {passed}");

    // After the gate accepts evidence, the prompt must stop demanding it:
    // prompt and gate always describe the same contract state.
    let done = emit_prompt(&host, true).await;
    assert!(
        done["systemPromptOptions"]["sections"]
            .get("execution_contract")
            .is_none(),
        "a verified contract is no longer active work: {}",
        done["systemPromptOptions"]["sections"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn preapproved_shell_calls_cannot_satisfy_the_contract_without_the_marker() {
    let dir = plan_dir("contract-baseline");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read", "bash"], path);
    // The command was already in the transcript when the plan was approved;
    // its call id must be part of the persisted baseline.
    session.set_entries(json!([{
        "messages": [{
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call-before",
                "name": "bash",
                "arguments": { "command": "python -m unittest" }
            }]
        }]
    }]));
    let status = Arc::new(FakeStatus::default());
    let ui = FakeUi::selecting("approve");
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, Arc::clone(&status), ui, messages);

    host.dispatch_command("mode", "plan").await.ok();
    emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification("1. build", "python -m unittest"),
    )
    .await;
    host.dispatch_command("approvals", "").await.ok();

    // No PLAN APPROVED marker in the settle payload (as after compaction):
    // only the call-id baseline can tell that this result predates approval.
    let stale = successful_verification_turn_with_command("call-before", "python -m unittest");
    let gated = emit_settle(&host, stale).await;
    assert_eq!(
        gated["continue"], true,
        "a pre-approval call must not satisfy the contract: {gated}"
    );

    let fresh = successful_verification_turn_with_command("call-after", "python -m unittest");
    let passed = emit_settle(&host, fresh).await;
    assert_ne!(
        passed["continue"], true,
        "a call created after approval passes: {passed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_normal_run_without_a_contract_is_not_gated() {
    let dir = plan_dir("no-contract");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["bash"], path);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    let result = emit_settle(&host, completed_turn(vec![])).await;
    assert_ne!(result["continue"], true, "plain normal settles: {result}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn contract_gate_nudges_until_a_successful_verification_tool_result() {
    let dir = plan_dir("contract-gate");
    let (host, _status) =
        approve_plan_with_contract(&dir, "1. build the thing", "python -m unittest").await;

    // No verification tool result yet: the gate asks for one (custom message).
    let gated = emit_settle(&host, completed_turn(vec![])).await;
    assert_eq!(gated["continue"], true, "gate demands evidence: {gated}");
    assert_eq!(gated["entries"][0]["customType"], "verification_gate");
    let content = gated["entries"][0]["content"].as_str().unwrap_or_default();
    assert!(content.contains("verification"), "{content}");

    // A successful shell command satisfies the contract; settle proceeds.
    let passed = emit_settle(&host, successful_verification_turn("call-verify-1")).await;
    assert_ne!(
        passed["continue"], true,
        "successful evidence settles: {passed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_shell_command_does_not_satisfy_the_contract_gate() {
    let dir = plan_dir("contract-fail");
    let (host, _status) =
        approve_plan_with_contract(&dir, "1. build the thing", "python -m unittest").await;

    let failed_turn = vec![
        json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call-fail",
                "name": "bash",
                "arguments": { "command": "python -m unittest" }
            }],
            "stopReason": "toolUse"
        }),
        json!({
            "role": "toolResult",
            "toolCallId": "call-fail",
            "toolName": "bash",
            "isError": false,
            "content": [{ "type": "text", "text": "FAILED (failures=1)\nCommand exited with code 1" }],
            "details": { "exitCode": 1 }
        }),
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "done" }],
            "stopReason": "stop"
        }),
    ];
    let gated = emit_settle(&host, failed_turn).await;
    assert_eq!(
        gated["continue"], true,
        "failed evidence keeps gate: {gated}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn contract_gate_stops_after_three_nudges_and_marks_unverified() {
    let dir = plan_dir("contract-cap");
    let (host, status) =
        approve_plan_with_contract(&dir, "1. build the thing", "python -m unittest").await;

    for _ in 0..3 {
        let gated = emit_settle(&host, completed_turn(vec![])).await;
        assert_eq!(gated["continue"], true, "nudge within cap: {gated}");
    }
    // Fourth boundary: cap reached, no more nudge, run settles as unverified.
    let settled = emit_settle(&host, completed_turn(vec![])).await;
    assert_ne!(settled["continue"], true, "cap allows settle: {settled}");
    assert_eq!(status.text("mode"), None, "still in normal mode");
    let mut os = dir.join("session.jsonl").as_os_str().to_owned();
    os.push(".foreman.jsonl");
    let journal = std::fs::read_to_string(PathBuf::from(os)).expect("journal");
    assert!(
        journal.to_ascii_lowercase().contains("unverified"),
        "unverified phase persisted: {journal}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_completed_contract_is_not_reopened_by_a_later_worker() {
    let dir = plan_dir("contract-worker-after-complete");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "bash", "read"], path);
    session.set_entries(json!([]));
    let status = Arc::new(FakeStatus::default());
    let ui = FakeUi::selecting("approve");
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, Arc::clone(&status), ui, messages);

    host.dispatch_command("mode", "plan").await.ok();
    let outcome = emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification("1. build", "python -m unittest"),
    )
    .await;
    assert_eq!(outcome["block"], true, "{outcome}");
    host.dispatch_command("approvals", "").await.ok();

    let passed = emit_settle(&host, successful_verification_turn("call-verify")).await;
    assert_ne!(
        passed["continue"], true,
        "first verification passes: {passed}"
    );

    // The contract is one-shot: later work (even a worker) is ordinary Normal
    // work, not a re-opened plan.
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    let settled = emit_settle(&host, completed_turn(vec![])).await;
    assert_ne!(
        settled["continue"], true,
        "a completed contract stays retired: {settled}"
    );
    let prompt = emit_prompt(&host, true).await;
    assert!(
        prompt["systemPromptOptions"]["sections"]
            .get("execution_contract")
            .is_none(),
        "a retired contract is not injected: {prompt}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn strict_verification_commands_require_the_exact_command_to_run() {
    let dir = plan_dir("strict-verify-match");
    let (host, _status) = approve_plan_with_verification_value(
        &dir,
        "1. build the thing",
        json!(["python -m unittest"]),
    )
    .await;

    // The exact declared command succeeds -> gate passes.
    let passed = emit_settle(&host, successful_verification_turn("call-strict")).await;
    assert_ne!(passed["continue"], true, "exact command passes: {passed}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn write_call_after(text: &str) -> Vec<Value> {
    vec![
        json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call-write",
                "name": "write",
                "arguments": { "path": "late.txt", "content": text }
            }],
            "stopReason": "toolUse"
        }),
        json!({
            "role": "toolResult",
            "toolCallId": "call-write",
            "toolName": "write",
            "isError": false,
            "content": [{ "type": "text", "text": "Successfully wrote to late.txt" }]
        }),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn verification_followed_by_a_mutation_does_not_retire_the_contract() {
    let dir = plan_dir("verify-then-mutate");
    let (host, _status) = approve_plan_with_verification_value(
        &dir,
        "1. build the thing",
        json!(["python -m unittest"]),
    )
    .await;

    let mut turn = successful_verification_turn("call-verify");
    // Drop the closing text turn and mutate after the verification command.
    turn.pop();
    turn.extend(write_call_after("late change"));
    turn.push(json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "done" }],
        "stopReason": "stop"
    }));
    let gated = emit_settle(&host, turn).await;
    assert_eq!(
        gated["continue"], true,
        "evidence before a later write must not retire the contract: {gated}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_mutation_before_verification_still_retires_the_contract() {
    let dir = plan_dir("mutate-then-verify");
    let (host, _status) = approve_plan_with_verification_value(
        &dir,
        "1. build the thing",
        json!(["python -m unittest"]),
    )
    .await;

    let mut turn = write_call_after("initial change");
    turn.extend(successful_verification_turn("call-verify"));
    let passed = emit_settle(&host, turn).await;
    assert_ne!(
        passed["continue"], true,
        "the declared command after the mutation must pass: {passed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_different_successful_command_does_not_satisfy_strict_verification() {
    let dir = plan_dir("strict-verify-mismatch");
    let (host, _status) = approve_plan_with_verification_value(
        &dir,
        "1. build the thing",
        json!(["python -m unittest"]),
    )
    .await;

    // A different command succeeds but is not the declared one -> still gated.
    let gated = emit_settle(
        &host,
        successful_verification_turn_with_command("call-other", "echo ok"),
    )
    .await;
    assert_eq!(
        gated["continue"], true,
        "undeclared command must not pass: {gated}"
    );

    // Running the declared command afterwards passes.
    let passed = emit_settle(&host, successful_verification_turn("call-strict")).await;
    assert_ne!(
        passed["continue"], true,
        "declared command passes: {passed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_completed_contract_is_retired_after_resume() {
    let dir = plan_dir("completed-contract-resume");
    let path = dir.join("session.jsonl");

    let session = FakeSession::with_path(true, &["subagent", "read", "bash"], path.clone());
    session.set_entries(json!([]));
    let status = Arc::new(FakeStatus::default());
    let ui = FakeUi::selecting("approve");
    let messages = Arc::new(FakeMessages::default());
    let (host, _plugin) = host_with_plan(session, status, ui, messages);
    host.dispatch_command("mode", "plan").await.ok();
    emit_tool_input(
        &host,
        "submit_plan",
        "call-plan",
        submit_plan_input_with_verification("1. build", "python -m unittest"),
    )
    .await;
    host.dispatch_command("approvals", "").await.ok();

    let passed = emit_settle(
        &host,
        successful_verification_turn_with_command("call-verify", "python -m unittest"),
    )
    .await;
    assert_ne!(passed["continue"], true, "verified: {passed}");
    drop(host);

    // A completed one-shot contract is not restored as active work.
    let session2 = FakeSession::with_path(true, &["subagent", "read", "bash"], path);
    let status2 = Arc::new(FakeStatus::default());
    let (host2, _plugin2) = host_with(session2, status2);
    let mut start = json!({ "type": "session_start", "reason": "resume" });
    host2
        .emit("session_start", &mut start)
        .await
        .expect("emit session_start");

    let prompt = emit_prompt(&host2, true).await;
    assert!(
        prompt["systemPromptOptions"]["sections"]
            .get("execution_contract")
            .is_none(),
        "retired contract has no prompt: {prompt}"
    );
    let settled = emit_settle(&host2, completed_turn(vec![])).await;
    assert_ne!(
        settled["continue"], true,
        "retired contract does not gate after resume: {settled}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn worker_failure_result(call_id: &str, stop_reason: &str) -> Value {
    json!({
        "role": "toolResult",
        "toolCallId": call_id,
        "toolName": "subagent",
        "isError": true,
        "content": [{ "type": "text", "text": format!("Agent {stop_reason}: Subagent was aborted") }],
        "details": {
            "mode": "single",
            "agentScope": "user",
            "projectAgentsDir": null,
            "results": [{
                "agent": "worker",
                "agentSource": "user",
                "task": "write the file",
                "exitCode": 1,
                "stopReason": stop_reason,
                "stderr": "worker failed",
                "messages": []
            }]
        }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_worker_blocks_reviewer_pass() {
    let dir = plan_dir("worker-fail-gate");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman").await.ok();
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;

    let messages = vec![
        json!({"role":"user","content":[{"type":"text","text":"do the task"}]}),
        worker_failure_result("call-worker", "error"),
        reviewer_tool_result("call-review", "PASS"),
        json!({"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"}),
    ];
    let result = emit_settle(&host, messages).await;
    assert_eq!(
        result["continue"], true,
        "failed worker must block even a reviewer PASS: {result}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn an_aborted_worker_blocks_reviewer_pass() {
    let dir = plan_dir("worker-abort-gate");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);

    host.dispatch_command("mode", "foreman").await.ok();
    emit_tool_input(&host, "subagent", "call-worker", worker_input()).await;
    emit_tool_input(&host, "subagent", "call-review", reviewer_input()).await;

    let messages = vec![
        json!({"role":"user","content":[{"type":"text","text":"do the task"}]}),
        worker_failure_result("call-worker", "aborted"),
        reviewer_tool_result("call-review", "PASS"),
        json!({"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"}),
    ];
    let result = emit_settle(&host, messages).await;
    assert_eq!(
        result["continue"], true,
        "aborted worker must block even a reviewer PASS: {result}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn a_resumed_failed_worker_still_blocks_a_later_reviewer_pass() {
    let dir = plan_dir("worker-fail-resume");
    let path = dir.join("session.jsonl");

    // First run journals the worker delegation but exits before the gate can
    // snapshot anything. On resume the worker call id must be reconstructed
    // from the Delegation event, not lost.
    let session1 = FakeSession::with_path(true, &["subagent", "read"], path.clone());
    let status1 = Arc::new(FakeStatus::default());
    let (host1, _plugin1) = host_with(session1, status1);
    host1.dispatch_command("mode", "foreman").await.ok();
    emit_tool_input(&host1, "subagent", "call-worker", worker_input()).await;
    drop(host1);

    let session2 = FakeSession::with_path(true, &["subagent", "read"], path);
    session2.set_entries(json!([{
        "messages": [
            {
                "role": "assistant",
                "content": [{
                    "type": "toolCall",
                    "id": "call-worker",
                    "name": "subagent",
                    "arguments": { "agent": "worker", "task": "write the file" }
                }]
            },
            worker_failure_result("call-worker", "error")
        ]
    }]));
    let status2 = Arc::new(FakeStatus::default());
    let (host2, _plugin2) = host_with(session2, status2);

    // A second reviewer delegation supersedes the old reviewer state but must
    // never hide the failed worker.
    emit_tool_input(&host2, "subagent", "call-review-2", reviewer_input()).await;
    let result = emit_settle(
        &host2,
        vec![
            json!({"role":"user","content":[{"type":"text","text":"do the task"}]}),
            worker_failure_result("call-worker", "error"),
            reviewer_tool_result("call-review-2", "PASS"),
            json!({"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"}),
        ],
    )
    .await;
    assert_eq!(
        result["continue"], true,
        "resumed worker failure must survive the journal replay: {result}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn submit_plan_requires_a_non_empty_verification_field() {
    let dir = plan_dir("require-verify");
    let path = dir.join("session.jsonl");
    let session = FakeSession::with_path(true, &["subagent", "read"], path);
    let status = Arc::new(FakeStatus::default());
    let (host, _plugin) = host_with(session, status);
    host.dispatch_command("mode", "plan").await.ok();

    // Missing verification is rejected.
    let missing =
        emit_tool_input(&host, "submit_plan", "call-1", submit_plan_input("1. do x")).await;
    assert_eq!(missing["block"], true, "{missing}");
    assert!(
        missing["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("verification"),
        "{}",
        missing["reason"]
    );

    // Free-form prose is rejected even when non-empty: only executable
    // commands can be verified.
    let prose = emit_tool_input(
        &host,
        "submit_plan",
        "call-2",
        json!({"plan": "1. do x", "verification": "run the unit tests"}),
    )
    .await;
    assert_eq!(prose["block"], true, "{prose}");
    assert!(
        prose["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("verification"),
        "{}",
        prose["reason"]
    );

    let empty_array = emit_tool_input(
        &host,
        "submit_plan",
        "call-3",
        json!({"plan": "1. do x", "verification": []}),
    )
    .await;
    assert_eq!(empty_array["block"], true, "{empty_array}");
    assert!(empty_array["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("verification"));

    // A valid verification is accepted (blocked for approval, not rejected).
    let ok = emit_tool_input(
        &host,
        "submit_plan",
        "call-4",
        submit_plan_input_with_verification("1. do x", "python -m unittest"),
    )
    .await;
    let reason = ok["reason"].as_str().unwrap_or_default();
    assert_eq!(ok["block"], true, "{ok}");
    assert!(reason.contains("Plan approval pending"), "{reason}");
    let _ = std::fs::remove_dir_all(&dir);
}
