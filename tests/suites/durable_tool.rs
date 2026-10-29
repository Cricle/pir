//! Conformance tests for the built-in `pi.tool` task (spec §7, §8.6).
//!
//! The task's harness services are a [`ToolHost`]: the conversation's resolved
//! agent, entry reads, id minting and the tool implementation itself. The tests
//! implement that seam directly, then drive the ported `harness/tool.ts` through
//! a real [`TaskScheduler`] over [`MemoryStorage`], so every assertion below is
//! about committed state: the task record, the appended `pi.tool-result` entry,
//! the caller's nested-call document, `pi.live`, and `pi.usage`.
//!
//! | test | invariant |
//! |---|---|
//! | `checkpoint_and_result_shapes_round_trip` | version-1 round-trips; unknown checkpoints re-resolve |
//! | `check_key_rejects_ambiguous_keys` | keys that would break call IDs are refused |
//! | `bound_output_keeps_whole_lines` | head/tail windows are exact line slices |
//! | `model_call_appends_its_result_entry` | intent, execution and `pi.tool-result` in one task |
//! | `blocked_before_hook_settles_with_an_error_result` | a block is a result, not a fault |
//! | `invalid_arguments_settle_with_an_error_result` | the validator's message reaches the model |
//! | `unavailable_tool_settles_with_an_error_result` | the agent's offered tools decide availability |
//! | `explicit_output_is_bounded_to_its_head` | result content obeys the limits, with a diagnostic |
//! | `streamed_output_keeps_the_tail` | `api.output` windows, `output_window`, truncation |
//! | `nested_calls_deduplicate_by_key` | one child per key; repeated keys reuse it |
//! | `nested_key_conflict_is_rejected` | the same key with other arguments is an error |
//! | `terminate_and_add_tools_controls` | completed calls record their post-tool control |
//! | `usage_is_recorded_per_tool` | `pi.usage.tools` accumulates the call's spend |
//! | `replay_safe_reruns_after_restart` | safe intent and safe tool rerun `execute` |
//! | `unsafe_replay_settles_interrupted_with_partial_output` | unsafe intent settles the slot's partial |

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::ai::types::{
    AssistantMessage, AssistantRole, ContentBlock, Message, StopReason, TextContent, ThinkingLevel,
    ToolCall, ToolResultMessage, Usage,
};
use pir::durable::harness::agent::Agent;
use pir::durable::harness::live::{
    live_from_object, live_to_object, SlotProgress, SlotStatus, ToolSlot, LIVE_DEFINITION,
};
use pir::durable::harness::registry::{ReplayPolicy, ToolRegistration};
use pir::durable::harness::scheduler::runtime::TaskRuntime;
use pir::durable::harness::scheduler::{TaskScheduler, TaskSchedulerOptions};
use pir::durable::harness::settings::{ProgressPolicy, Settings};
use pir::durable::harness::task::BoxFuture;
use pir::durable::harness::tool::{
    bound_output, check_key, live_address, nested_call_address, tool_task, usage_address,
    BeforeToolDecision, ExecuteToolOptions, OutputLimits, ToolControl, ToolExecutionApi,
    ToolExecutionResult, ToolHookApi, ToolHookCall, ToolHooks, ToolHost, ToolTaskCheckpoint,
    ToolTaskInput, ToolTaskResult, TOOL_TASK_NAME, TOOL_TASK_VERSION,
};
use pir::durable::ids::{ConversationId, EntryId, TaskId};
use pir::durable::session::facade::{ConversationOwnership, Session};
use pir::durable::session::transaction::{EntryDraft, TaskOptions, TaskOwnership};
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::types::{
    EntryRecord, JsonObject, Storage, StorageWrite, TaskOutcome, TaskRecord, TaskState,
};
use pir::durable::Context;

// ── the tool host ──────────────────────────────────────────────────────────

/// A scripted tool implementation: the same shape as [`ToolHost::execute`].
type ExecFn = Arc<
    dyn for<'a> Fn(
            &'a ToolRegistration,
            JsonObject,
            &'a ToolExecutionApi<'a>,
            Context,
        ) -> BoxFuture<'a, Result<ToolExecutionResult, String>>
        + Send
        + Sync,
>;

/// The harness seam the task runs against.
///
/// The session is attached after the scheduler opens (the host is needed to
/// build the task definition, and the scheduler owns the session), mirroring
/// how a real host binds the two.
struct Host {
    storage: Arc<MemoryStorage>,
    session: Mutex<Option<Session>>,
    agent: Agent,
    exec: ExecFn,
    limits: OutputLimits,
    settings: Settings,
    hooks: ToolHooks,
    validation_error: Option<String>,
    /// Every tool name `execute` was invoked for, in order.
    executed: Arc<Mutex<Vec<String>>>,
}

impl Host {
    fn new(agent: Agent, exec: ExecFn) -> Self {
        Self {
            storage: Arc::new(MemoryStorage::new()),
            session: Mutex::new(None),
            agent,
            exec,
            limits: OutputLimits::default(),
            settings: Settings::default(),
            hooks: ToolHooks::default(),
            validation_error: None,
            executed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_limits(mut self, limits: OutputLimits) -> Self {
        self.limits = limits;
        self
    }

    fn with_hooks(mut self, hooks: ToolHooks) -> Self {
        self.hooks = hooks;
        self
    }

    fn with_validation_error(mut self, message: &str) -> Self {
        self.validation_error = Some(message.to_owned());
        self
    }

    fn attach(&self, session: Session) {
        *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
    }

    fn session(&self) -> Session {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("the host is attached to a session")
    }

    fn executed(&self) -> Vec<String> {
        self.executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ToolHost for Host {
    fn agent<'a>(
        &'a self,
        _runtime: &'a TaskRuntime,
        _context: Context,
    ) -> BoxFuture<'a, Result<Agent, String>> {
        let agent = self.agent.clone();
        Box::pin(async move { Ok(agent) })
    }

    fn entry<'a>(
        &'a self,
        _runtime: &'a TaskRuntime,
        id: EntryId,
        _context: Context,
    ) -> BoxFuture<'a, Result<Option<EntryRecord>, String>> {
        Box::pin(async move {
            self.session()
                .entry(id, &Context::background())
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn validate_arguments(
        &self,
        _tool: &ToolRegistration,
        _call: &ToolCall,
        arguments: &JsonObject,
    ) -> Result<JsonObject, String> {
        match &self.validation_error {
            Some(message) => Err(message.clone()),
            None => Ok(arguments.clone()),
        }
    }

    fn output_limits(&self, _tool: &ToolRegistration) -> OutputLimits {
        self.limits
    }

    fn execute<'a>(
        &'a self,
        tool: &'a ToolRegistration,
        arguments: JsonObject,
        api: &'a ToolExecutionApi<'a>,
        context: Context,
    ) -> BoxFuture<'a, Result<ToolExecutionResult, String>> {
        self.executed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tool.name.clone());
        (self.exec)(tool, arguments, api, context)
    }

    fn mint_id<'a>(&'a self, _runtime: &'a TaskRuntime) -> BoxFuture<'a, Result<i64, String>> {
        Box::pin(async move {
            self.storage
                .mint_id()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn settings(&self, _runtime: &TaskRuntime) -> Settings {
        self.settings.clone()
    }

    fn hooks(&self, _agent: &Agent) -> ToolHooks {
        self.hooks.clone()
    }
}

// ── agents, tools and messages ─────────────────────────────────────────────

fn agent_with(offered: Vec<ToolRegistration>, callable: Vec<ToolRegistration>) -> Agent {
    Agent {
        model: None,
        thinking_level: ThinkingLevel::Off,
        extensions: Vec::new(),
        tools: offered,
        callable,
        deferred: Vec::new(),
        sections: Vec::new(),
        instructions: None,
        cwd: None,
    }
}

/// One offered tool; also callable by tools unless `callable` is false.
fn registration(name: &str, replay: ReplayPolicy) -> ToolRegistration {
    let mut registration = ToolRegistration::new(name).with_parameters(json!({ "type": "object" }));
    registration.replay = replay;
    registration
}

fn assistant_with_call(call_id: &str, name: &str, arguments: Value) -> Message {
    let arguments: BTreeMap<String, Value> = arguments
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    Message::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::ToolCall(ToolCall {
            id: call_id.to_owned(),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        api: "test".to_owned(),
        provider: "test".to_owned(),
        model: "test".to_owned(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..AssistantMessage::default()
    })
}

fn text_exec(text: &'static str) -> ExecFn {
    Arc::new(move |_tool, _arguments, _api, _context| {
        Box::pin(async move { Ok(ToolExecutionResult::text(text)) })
    })
}

fn usage() -> Usage {
    Usage {
        input: 10,
        output: 4,
        cache_read: 2,
        cache_write: 0,
        total_tokens: 16,
        cost: Default::default(),
        ..Usage::default()
    }
}

fn spend_exec() -> ExecFn {
    Arc::new(move |_tool, _arguments, _api, _context| {
        Box::pin(async move {
            Ok(ToolExecutionResult {
                usage: Some(usage()),
                ..ToolExecutionResult::default()
            })
        })
    })
}

fn control_exec() -> ExecFn {
    Arc::new(move |_tool, _arguments, _api, _context| {
        Box::pin(async move {
            Ok(ToolExecutionResult {
                control: Some(ToolControl {
                    add_tools: Some(vec!["more".to_owned()]),
                    terminate: Some(true),
                    handoff: None,
                }),
                ..ToolExecutionResult::default()
            })
        })
    })
}

fn tail_window_exec(chunks: Vec<&'static str>) -> ExecFn {
    Arc::new(move |_tool, _arguments, api, _context| {
        let chunks = chunks.clone();
        Box::pin(async move {
            let window = api.output_window().map(|window| {
                json!({
                    "maxBytes": window.max_bytes,
                    "maxLines": window.max_lines,
                    "bytesPerSecond": window.bytes_per_second,
                    "minIntervalMs": window.min_interval_ms,
                })
            });
            for chunk in &chunks {
                api.output(chunk, None);
            }
            Ok(ToolExecutionResult {
                details: Some(json!({ "window": window })),
                ..ToolExecutionResult::default()
            })
        })
    })
}

/// A tool that makes nested calls, one per `(key, arguments)` in `calls`.
///
/// `inner` answers; the outer tool returns what each nested call produced in
/// its details, so the caller can compare task ids and results.
fn nested_exec(child: &'static str, calls: Vec<(&'static str, Value)>) -> ExecFn {
    Arc::new(move |_tool, _arguments, api, context| {
        let calls = calls.clone();
        Box::pin(async move {
            let mut outcomes = Vec::new();
            for (key, arguments) in calls {
                let arguments = arguments.as_object().cloned().unwrap_or_default();
                let result = api
                    .execute_tool(
                        child,
                        arguments,
                        ExecuteToolOptions {
                            key: Some(key.to_owned()),
                            progress: Some(false),
                        },
                        context.clone(),
                    )
                    .await;
                outcomes.push(match result {
                    Ok(nested) => {
                        // The caller's family member is written before the call
                        // returns; read it through the same api the caller has.
                        let member = match nested_call_address(api.task_id(), key) {
                            Ok(address) => api.snapshot(&address).await.ok().flatten(),
                            Err(_) => None,
                        };
                        json!({
                            "key": key,
                            "taskId": nested.task_id.as_i64(),
                            "structured": nested.structured_output,
                            "isError": nested.is_error,
                            "member": member,
                        })
                    }
                    Err(error) => json!({ "key": key, "error": error.to_string() }),
                });
            }
            Ok(ToolExecutionResult {
                details: Some(json!({ "outcomes": outcomes })),
                ..ToolExecutionResult::default()
            })
        })
    })
}

fn blocking_hook(reason: &'static str) -> ToolHooks {
    ToolHooks {
        before: vec![Arc::new(
            move |_call: ToolHookCall, _api: &ToolHookApi, _context: Context| {
                Box::pin(async move { Ok(Some(BeforeToolDecision::block(reason))) })
            },
        )],
        after: Vec::new(),
    }
}

// ── world ──────────────────────────────────────────────────────────────────

struct World {
    scheduler: TaskScheduler,
    storage: Arc<MemoryStorage>,
    host: Arc<Host>,
    conversation: ConversationId,
}

impl World {
    async fn open(host: Host) -> Self {
        let storage = host.storage.clone();
        let host = Arc::new(host);
        let definition =
            tool_task(host.clone() as Arc<dyn ToolHost>).expect("a valid pi.tool definition");
        let settings = Settings::default();
        let options = TaskSchedulerOptions {
            settings: Some(Arc::new(move || settings.clone())),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, storage.clone(), vec![definition])
            .await
            .expect("open scheduler");
        host.attach(scheduler.session().clone());
        let conversation = scheduler
            .session()
            .create_conversation(
                ConversationOwnership::Ownerless,
                None,
                Context::background(),
            )
            .await
            .expect("create conversation")
            .id;
        Self {
            scheduler,
            storage,
            host,
            conversation,
        }
    }

    /// Append a `pi.assistant` entry holding one tool call.
    async fn append_assistant(&self, call_id: &str, name: &str, arguments: Value) -> EntryId {
        let message = assistant_with_call(call_id, name, arguments);
        let record = self
            .scheduler
            .session()
            .append_entry(
                self.conversation,
                EntryDraft {
                    kind: "pi.assistant".to_owned(),
                    model: Some(vec![
                        serde_json::to_value(message).expect("assistant message serializes")
                    ]),
                    data: None,
                    head: None,
                    edits: None,
                },
                Context::background(),
            )
            .await
            .expect("append assistant entry");
        record.id
    }

    async fn start_tool(&self, input: Value) -> TaskId {
        let definition = self
            .scheduler
            .catalogue()
            .snapshot()
            .task(TOOL_TASK_NAME)
            .expect("pi.tool is installed");
        self.scheduler
            .create_task(
                &definition,
                input,
                TaskOptions {
                    trace: None,
                    conversation_id: Some(self.conversation),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
            )
            .await
            .expect("create tool task")
    }

    /// Create and run a model-issued call; returns its task.
    async fn run_model_call(&self, name: &str, arguments: Value) -> TaskId {
        let entry = self.append_assistant("c1", name, arguments).await;
        let task = self
            .start_tool(
                ToolTaskInput::Model {
                    assistant: entry,
                    call_id: "c1".to_owned(),
                }
                .to_value(),
            )
            .await;
        self.scheduler.resume();
        task
    }

    async fn terminal(&self, id: TaskId) -> TaskRecord {
        tokio::time::timeout(Duration::from_secs(10), self.scheduler.wait_for_task(id))
            .await
            .unwrap_or_else(|_| panic!("task {id} did not settle in time"))
            .expect("wait for task")
    }

    async fn entry(&self, id: EntryId) -> EntryRecord {
        self.scheduler
            .session()
            .entry(id, &Context::background())
            .await
            .expect("read entry")
            .expect("entry exists")
    }

    async fn live(&self) -> JsonObject {
        self.scheduler
            .session()
            .snapshot(
                &live_address(self.conversation).expect("pi.live address"),
                &Context::background(),
            )
            .await
            .expect("read pi.live")
            .unwrap_or_default()
    }

    async fn usage(&self) -> JsonObject {
        self.scheduler
            .session()
            .snapshot(
                &usage_address(self.conversation).expect("pi.usage address"),
                &Context::background(),
            )
            .await
            .expect("read pi.usage")
            .unwrap_or_default()
    }

    /// Seed a `pi.live` tool slot for `task`, holding `output` as partial
    /// progress, as a generation's tools phase would.
    async fn seed_tool_slot(&self, task: TaskId, name: &str, output: &str) {
        let address = live_address(self.conversation).expect("pi.live address");
        let definition = (*LIVE_DEFINITION).clone();
        let name = name.to_owned();
        let output = output.to_owned();
        self.scheduler
            .session()
            .doc(address, definition, Context::background(), move |value| {
                let mut state = live_from_object(value);
                let mut progress = SlotProgress::new("c1", name);
                progress.status = SlotStatus::Running;
                progress.output = Some(output);
                state.tools = Some(vec![ToolSlot {
                    progress,
                    task_id: Some(task),
                    entry: None,
                }]);
                *value = live_to_object(&state);
            })
            .await
            .expect("seed pi.live slot");
    }

    /// Write a `running` tool task record, as a crashed process leaves it.
    async fn seed_running(&self, task: TaskId, entry: EntryId, checkpoint: ToolTaskCheckpoint) {
        self.storage
            .commit(vec![StorageWrite::Task {
                value: TaskRecord {
                    trace: None,
                    links: Vec::new(),
                    id: task,
                    conversation_id: self.conversation,
                    kind: TOOL_TASK_NAME.to_owned(),
                    version: TOOL_TASK_VERSION,
                    input: ToolTaskInput::Model {
                        assistant: entry,
                        call_id: "c1".to_owned(),
                    }
                    .to_value(),
                    owner: None,
                    background: false,
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: None,
                    started_at: Some(1_000),
                    ended_at: None,
                    restarts: Some(0),
                    state: TaskState::Running {
                        checkpoint: checkpoint.to_value(),
                    },
                    memos: None,
                },
            }])
            .await
            .expect("seed the running tool task");
    }
}

fn outcome(record: &TaskRecord) -> TaskOutcome {
    match &record.state {
        TaskState::Terminal { outcome } => outcome.clone(),
        other => panic!("expected a terminal task, got {other:?}"),
    }
}

fn model_result(record: &TaskRecord) -> ToolTaskResult {
    let result = match outcome(record) {
        TaskOutcome::Completed { result } => result,
        TaskOutcome::Failed {
            result: Some(result),
            ..
        } => result,
        TaskOutcome::Aborted {
            result: Some(result),
            ..
        } => result,
        other => panic!("the task carries no result: {other:?}"),
    };
    ToolTaskResult::from_value(&result).expect("a tool task result")
}

fn result_entry(record: &TaskRecord) -> EntryId {
    model_result(record)
        .entry_id()
        .expect("a model-issued call settles with an entry")
}

fn tool_result(entry: &EntryRecord) -> ToolResultMessage {
    let value = entry
        .model
        .as_ref()
        .expect("the result entry carries a message")[0]
        .clone();
    match serde_json::from_value::<Message>(value).expect("the message parses") {
        Message::ToolResult(message) => message,
        other => panic!("expected a tool result, got {other:?}"),
    }
}

fn entry_text(entry: &EntryRecord) -> String {
    tool_result(entry)
        .content
        .iter()
        .filter_map(|item| match item {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The diagnostics the result entry's `data` carries.
fn entry_diagnostics(entry: &EntryRecord) -> Vec<Value> {
    entry
        .data
        .as_ref()
        .and_then(|data| data.get("diagnostics"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn diagnostic_codes(entry: &EntryRecord) -> Vec<String> {
    entry_diagnostics(entry)
        .iter()
        .filter_map(|diagnostic| diagnostic.get("code").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

// ── pure shapes ────────────────────────────────────────────────────────────

#[test]
fn checkpoint_and_result_shapes_round_trip() {
    // A version-1 input has no `kind`: it is a model-issued call.
    let legacy = json!({ "assistant": 7, "callId": "c1" });
    assert_eq!(
        ToolTaskInput::from_value(&legacy).expect("a version-1 input"),
        ToolTaskInput::Model {
            assistant: EntryId::from_raw(7),
            call_id: "c1".to_owned(),
        }
    );

    let nested = ToolTaskInput::Nested {
        parent: TaskId::from_raw(3),
        parent_call_id: "c1".to_owned(),
        key: "fetch".to_owned(),
        call: ToolCall {
            id: "c1/fetch".to_owned(),
            name: "read".to_owned(),
            arguments: BTreeMap::new(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        },
        progress: false,
    };
    assert_eq!(
        ToolTaskInput::from_value(&nested.to_value()).expect("a nested input"),
        nested,
        "`progress: false` survives the durable form"
    );
    assert!(nested.is_nested());
    assert!(!nested.streams_progress());

    let intent = ToolTaskCheckpoint::Execute {
        arguments: json!({ "path": "a" }).as_object().cloned().unwrap(),
        replay: ReplayPolicy::Safe,
    };
    assert_eq!(
        ToolTaskCheckpoint::from_value(&intent.to_value()),
        intent,
        "the execute intent round-trips"
    );
    assert_eq!(intent.phase(), "execute");
    assert_eq!(
        ToolTaskCheckpoint::from_value(&json!({ "phase": "surprise" })),
        ToolTaskCheckpoint::Call,
        "an unknown checkpoint re-resolves the call rather than faulting"
    );

    // A version-1 result has no `kind`.
    let legacy = ToolTaskResult::from_value(&json!({ "entryId": 11 })).expect("a v1 result");
    assert_eq!(legacy.entry_id(), Some(EntryId::from_raw(11)));
    assert!(legacy.control().is_none());
    assert_eq!(
        ToolTaskResult::from_value(&json!({ "kind": "nested" })),
        Some(ToolTaskResult::Nested)
    );
    assert_eq!(
        ToolTaskResult::from_value(&json!({ "kind": "other" })),
        None
    );
}

#[test]
fn check_key_rejects_ambiguous_keys() {
    // A key is a path segment of `<parentCallId>/<key>`.
    assert!(check_key("fetch").is_ok());
    assert!(check_key("fetch-2").is_ok());
    // "0" is not a positive integer: default keys start at "1".
    assert!(check_key("0").is_ok());
    // Default keys are positive integers, so an explicit key must not collide.
    assert!(check_key("1").is_err());
    assert!(check_key("42").is_err());
    assert!(check_key("").is_err());
    assert!(check_key("a/b").is_err());
    assert!(check_key("__proto__").is_err());
}

#[test]
fn bound_output_keeps_whole_lines() {
    let text = "one\ntwo\nthree\nfour\n";
    let head = bound_output(text, OutputLimits::head(usize::MAX, 2));
    assert_eq!(head.text, "one\ntwo\n");
    assert_eq!(head.dropped_lines, 2);
    assert_eq!(head.dropped_bytes, "three\nfour\n".len() as u64);

    let tail = bound_output(text, OutputLimits::tail(usize::MAX, 2));
    assert_eq!(tail.text, "three\nfour\n");
    assert_eq!(tail.dropped_lines, 2);
    assert_eq!(tail.dropped_bytes, "one\ntwo\n".len() as u64);

    // A single line longer than the byte limit is cut on a character boundary.
    let long = "ééééé";
    let cut = bound_output(long, OutputLimits::head(3, 10));
    assert_eq!(cut.text, "é");
    assert_eq!(cut.dropped_bytes, 8);
}

// ── a model-issued call ────────────────────────────────────────────────────

#[tokio::test]
async fn model_call_appends_its_result_entry() {
    let agent = agent_with(
        vec![registration("echo", ReplayPolicy::Unsafe)],
        vec![registration("echo", ReplayPolicy::Unsafe)],
    );
    let world = World::open(Host::new(agent, text_exec("hello from echo"))).await;
    let task = world.run_model_call("echo", json!({ "text": "hi" })).await;

    let record = world.terminal(task).await;
    assert!(
        matches!(outcome(&record), TaskOutcome::Completed { .. }),
        "the call completed: {:?}",
        record.state
    );
    assert_eq!(
        model_result(&record),
        ToolTaskResult::Model {
            entry_id: result_entry(&record),
            control: None,
        }
    );

    let entry = world.entry(result_entry(&record)).await;
    assert_eq!(entry.kind, "pi.tool-result");
    assert_eq!(entry.by_task_id, Some(task));
    assert_eq!(entry.conversation_id, world.conversation);
    let result = tool_result(&entry);
    assert_eq!(result.tool_call_id, "c1");
    assert_eq!(result.tool_name, "echo");
    assert_eq!(
        result.content,
        ToolExecutionResult::text("hello from echo").output
    );
    assert!(!result.is_error);
    assert!(entry_diagnostics(&entry).is_empty());
    assert_eq!(world.host.executed(), vec!["echo".to_owned()]);
}

#[tokio::test]
async fn blocked_before_hook_settles_with_an_error_result() {
    let agent = agent_with(
        vec![registration("echo", ReplayPolicy::Unsafe)],
        vec![registration("echo", ReplayPolicy::Unsafe)],
    );
    let host = Host::new(agent, text_exec("never runs")).with_hooks(blocking_hook("denied"));
    let world = World::open(host).await;
    let task = world.run_model_call("echo", json!({})).await;

    let record = world.terminal(task).await;
    assert!(
        matches!(outcome(&record), TaskOutcome::Completed { .. }),
        "a blocked call is a result, not a fault"
    );
    let entry = world.entry(result_entry(&record)).await;
    let result = tool_result(&entry);
    assert!(result.is_error, "the model sees an error result");
    let diagnostics = entry_diagnostics(&entry);
    assert_eq!(
        diagnostics[0]["message"],
        json!("Tool call blocked: denied")
    );
    assert_eq!(diagnostic_codes(&entry), vec!["blocked".to_owned()]);
    assert!(
        world.host.executed().is_empty(),
        "a blocked call never reaches the tool"
    );
}

#[tokio::test]
async fn invalid_arguments_settle_with_an_error_result() {
    let agent = agent_with(
        vec![registration("echo", ReplayPolicy::Unsafe)],
        vec![registration("echo", ReplayPolicy::Unsafe)],
    );
    let host =
        Host::new(agent, text_exec("never runs")).with_validation_error("`text` is required");
    let world = World::open(host).await;
    let task = world.run_model_call("echo", json!({})).await;

    let record = world.terminal(task).await;
    assert!(matches!(outcome(&record), TaskOutcome::Completed { .. }));
    let entry = world.entry(result_entry(&record)).await;
    assert!(tool_result(&entry).is_error);
    let diagnostics = entry_diagnostics(&entry);
    assert_eq!(diagnostics[0]["message"], json!("`text` is required"));
    assert_eq!(
        diagnostic_codes(&entry),
        vec!["invalid_arguments".to_owned()]
    );
    assert!(world.host.executed().is_empty());
}

#[tokio::test]
async fn unavailable_tool_settles_with_an_error_result() {
    // The agent offers nothing, so the model's call cannot resolve.
    let world = World::open(Host::new(
        agent_with(Vec::new(), Vec::new()),
        text_exec("x"),
    ))
    .await;
    let task = world.run_model_call("ghost", json!({})).await;

    let record = world.terminal(task).await;
    assert!(matches!(outcome(&record), TaskOutcome::Completed { .. }));
    let entry = world.entry(result_entry(&record)).await;
    assert!(tool_result(&entry).is_error);
    assert_eq!(
        diagnostic_codes(&entry),
        vec!["tool_unavailable".to_owned()]
    );
    assert!(world.host.executed().is_empty());
}

#[tokio::test]
async fn explicit_output_is_bounded_to_its_head() {
    let long = format!("{}\n{}", "a".repeat(30), "b".repeat(30));
    let exec: ExecFn = Arc::new(move |_tool, _arguments, _api, _context| {
        let long = long.clone();
        Box::pin(async move { Ok(ToolExecutionResult::text(long)) })
    });
    let agent = agent_with(
        vec![registration("echo", ReplayPolicy::Unsafe)],
        vec![registration("echo", ReplayPolicy::Unsafe)],
    );
    let world = World::open(Host::new(agent, exec).with_limits(OutputLimits::head(40, 4))).await;
    let task = world.run_model_call("echo", json!({})).await;

    let record = world.terminal(task).await;
    let entry = world.entry(result_entry(&record)).await;
    assert_eq!(
        entry_text(&entry),
        format!(
            "{}\n\n<harness>\n[warn] Output truncated to its beginning: 1 lines, 30 bytes dropped\n</harness>",
            "a".repeat(30)
        )
    );
    let truncated = entry_diagnostics(&entry)
        .into_iter()
        .find(|diagnostic| diagnostic.get("code").and_then(Value::as_str) == Some("truncated"))
        .expect("a truncation diagnostic");
    assert_eq!(truncated["severity"], json!("warn"));
    assert_eq!(
        truncated["message"],
        json!("Output truncated to its beginning: 1 lines, 30 bytes dropped")
    );
}

#[tokio::test]
async fn streamed_output_keeps_the_tail() {
    let agent = agent_with(
        vec![registration("stream", ReplayPolicy::Unsafe)],
        vec![registration("stream", ReplayPolicy::Unsafe)],
    );
    let world = World::open(
        Host::new(
            agent,
            tail_window_exec(vec!["one\n", "two\n", "three\n", "four\n", "five\n"]),
        )
        .with_limits(OutputLimits::tail(64, 3)),
    )
    .await;
    let task = world.run_model_call("stream", json!({})).await;

    let record = world.terminal(task).await;
    let entry = world.entry(result_entry(&record)).await;
    let result = tool_result(&entry);
    // The result omitted `output`, so the retained window is the model's output.
    assert_eq!(
        result.content.first(),
        Some(&ContentBlock::Text(TextContent {
            text: "three\nfour\nfive\n".to_owned(),
            text_signature: None,
        }))
    );
    assert!(
        entry_text(&entry).starts_with("three\nfour\nfive\n"),
        "the model reads the retained tail first: {:?}",
        entry_text(&entry)
    );
    let truncated = entry_diagnostics(&entry)
        .into_iter()
        .find(|diagnostic| diagnostic.get("code").and_then(Value::as_str) == Some("truncated"))
        .expect("a truncation diagnostic");
    assert_eq!(truncated["severity"], json!("warn"));
    assert_eq!(
        truncated["message"],
        json!("Output truncated to its end: 2 lines, 8 bytes dropped")
    );

    // `output_window` reports the tail limits and the commit pace.
    let window = result.details.expect("details")["window"].clone();
    assert_eq!(
        window,
        json!({
            "maxBytes": 64,
            "maxLines": 3,
            "bytesPerSecond": 100 * 1024,
            "minIntervalMs": 100,
        })
    );
}

// ── nested calls ───────────────────────────────────────────────────────────

fn nested_agent() -> Agent {
    let outer = registration("outer", ReplayPolicy::Unsafe);
    let inner = registration("inner", ReplayPolicy::Unsafe);
    agent_with(vec![outer.clone()], vec![outer, inner])
}

fn inner_or_nested_exec(label: &'static str, calls: Vec<(&'static str, Value)>) -> ExecFn {
    let nested = nested_exec("inner", calls);
    Arc::new(move |tool, arguments, api, context| {
        if tool.name == "inner" {
            Box::pin(async move { Ok(ToolExecutionResult::text(label)) })
        } else {
            let nested = nested.clone();
            Box::pin(async move { nested(tool, arguments, api, context).await })
        }
    })
}

#[tokio::test]
async fn nested_calls_deduplicate_by_key() {
    let exec = inner_or_nested_exec(
        "inner output",
        vec![
            ("fetch", json!({ "n": 1 })),
            ("fetch", json!({ "n": 1 })),
            ("other", json!({ "n": 2 })),
        ],
    );
    let world = World::open(Host::new(nested_agent(), exec)).await;
    let task = world.run_model_call("outer", json!({})).await;

    let record = world.terminal(task).await;
    assert!(
        matches!(outcome(&record), TaskOutcome::Completed { .. }),
        "the outer call completed: {:?}",
        record.state
    );
    let entry = world.entry(result_entry(&record)).await;
    let outcomes = tool_result(&entry).details.expect("details")["outcomes"]
        .as_array()
        .cloned()
        .expect("outcomes");

    let first = outcomes[0]["taskId"].as_i64().expect("a child task");
    let second = outcomes[1]["taskId"].as_i64().expect("a child task");
    let third = outcomes[2]["taskId"].as_i64().expect("a child task");
    assert_eq!(first, second, "the same key and arguments reuse the call");
    assert_ne!(first, third, "another key is another call");
    assert_eq!(outcomes[0]["structured"], json!("inner output"));
    assert_eq!(outcomes[1]["structured"], json!("inner output"));
    assert_eq!(outcomes[0]["isError"], json!(false));

    let executed = world.host.executed();
    assert_eq!(
        executed.iter().filter(|name| *name == "inner").count(),
        2,
        "deduplicated keys execute once, distinct keys execute once each: {executed:?}"
    );

    // Each child is a `pi.tool` task whose call id is `<parentCallId>/<key>`.
    let child = world
        .scheduler
        .get_task(TaskId::from_raw(first))
        .await
        .expect("read child")
        .expect("child exists");
    assert_eq!(child.owner, Some(task));
    assert_eq!(
        ToolTaskInput::from_value(&child.input).expect("nested input"),
        ToolTaskInput::Nested {
            parent: task,
            parent_call_id: "c1".to_owned(),
            key: "fetch".to_owned(),
            call: ToolCall {
                id: "c1/fetch".to_owned(),
                name: "inner".to_owned(),
                arguments: [("n".to_owned(), json!(1))].into_iter().collect(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            },
            progress: false,
        }
    );

    // The result lives in the caller's family document, keyed by the call key.
    let member = &outcomes[0]["member"];
    assert_eq!(member["taskId"], json!(first));
    assert_eq!(member["result"]["taskId"], json!(first));
    assert_eq!(member["result"]["structuredOutput"], json!("inner output"));
    assert_eq!(member["result"]["isError"], json!(false));

    // A nested call's document belongs to the caller's task, so the caller's
    // terminal commit retires it: only the result entry outlives the call.
    assert!(
        world
            .scheduler
            .session()
            .snapshot(
                &nested_call_address(task, "fetch").expect("nested-call address"),
                &Context::background(),
            )
            .await
            .expect("read the nested-call member")
            .is_none(),
        "the caller's nested-call documents retire with it"
    );
}

#[tokio::test]
async fn nested_key_conflict_is_rejected() {
    let exec = inner_or_nested_exec(
        "inner output",
        vec![("fetch", json!({ "n": 1 })), ("fetch", json!({ "n": 2 }))],
    );
    let world = World::open(Host::new(nested_agent(), exec)).await;
    let task = world.run_model_call("outer", json!({})).await;

    let record = world.terminal(task).await;
    assert!(
        matches!(outcome(&record), TaskOutcome::Completed { .. }),
        "the outer call completed even though the second nested call failed"
    );
    let entry = world.entry(result_entry(&record)).await;
    let outcomes = tool_result(&entry).details.expect("details")["outcomes"]
        .as_array()
        .cloned()
        .expect("outcomes");
    assert!(outcomes[0]["taskId"].as_i64().is_some());
    let error = outcomes[1]["error"]
        .as_str()
        .expect("the conflicting call is an error");
    assert!(
        error.contains("already made with another tool or other arguments"),
        "unexpected error: {error}"
    );
    assert_eq!(
        world
            .host
            .executed()
            .iter()
            .filter(|name| *name == "inner")
            .count(),
        1
    );
}

// ── controls and usage ─────────────────────────────────────────────────────

#[tokio::test]
async fn terminate_and_add_tools_controls() {
    let agent = agent_with(
        vec![registration("finish", ReplayPolicy::Unsafe)],
        vec![registration("finish", ReplayPolicy::Unsafe)],
    );
    let world = World::open(Host::new(agent, control_exec())).await;
    let task = world.run_model_call("finish", json!({})).await;

    let record = world.terminal(task).await;
    let result = model_result(&record);
    assert_eq!(
        result.control(),
        Some(&ToolControl {
            add_tools: Some(vec!["more".to_owned()]),
            terminate: Some(true),
            handoff: None,
        })
    );
    // The durable form keeps upstream's camelCase keys.
    let value = match outcome(&record) {
        TaskOutcome::Completed { result } => result,
        other => panic!("expected completion, got {other:?}"),
    };
    assert_eq!(
        value["control"],
        json!({ "addTools": ["more"], "terminate": true })
    );
}

#[tokio::test]
async fn usage_is_recorded_per_tool() {
    let agent = agent_with(
        vec![registration("spend", ReplayPolicy::Unsafe)],
        vec![registration("spend", ReplayPolicy::Unsafe)],
    );
    let world = World::open(Host::new(agent, spend_exec())).await;
    let task = world.run_model_call("spend", json!({})).await;

    let record = world.terminal(task).await;
    let ledger = world.usage().await;
    let bucket = &ledger["tools"]["spend"];
    assert_eq!(bucket["input"], json!(10));
    assert_eq!(bucket["output"], json!(4));
    assert_eq!(bucket["cacheRead"], json!(2));
    assert_eq!(bucket["totalTokens"], json!(16));

    // The result entry carries the spend too, so a context rebuild sees it.
    let entry = world.entry(result_entry(&record)).await;
    assert_eq!(tool_result(&entry).usage, Some(usage()));
}

// ── replay after a restart ─────────────────────────────────────────────────

#[tokio::test]
async fn replay_safe_reruns_after_restart() {
    let agent = agent_with(
        vec![registration("echo", ReplayPolicy::Safe)],
        vec![registration("echo", ReplayPolicy::Safe)],
    );
    let host = Host {
        settings: Settings {
            progress: ProgressPolicy {
                output_interval_ms: 0,
                ..ProgressPolicy::default()
            },
            ..Settings::default()
        },
        ..Host::new(agent, text_exec("reran"))
    };
    let world = World::open(host).await;
    let entry = world.append_assistant("c1", "echo", json!({})).await;
    let task = TaskId::from_raw(9_001);
    world.seed_tool_slot(task, "echo", "partial output\n").await;
    world
        .seed_running(
            task,
            entry,
            ToolTaskCheckpoint::Execute {
                arguments: JsonObject::new(),
                replay: ReplayPolicy::Safe,
            },
        )
        .await;

    // Reopen: the recovery pass writes the running task back `pending`, and the
    // execute phase reruns a safe intent under a safe tool.
    world.scheduler.join().await;
    let reopened = World::open_existing(&world).await;
    reopened.scheduler.resume();
    let record = reopened.terminal(task).await;

    assert!(
        matches!(outcome(&record), TaskOutcome::Completed { .. }),
        "the safe rerun completed: {:?}",
        record.state
    );
    assert_eq!(
        reopened.host.executed(),
        vec!["echo".to_owned()],
        "the tool ran again"
    );
    let result = reopened.entry(result_entry(&record)).await;
    assert_eq!(entry_text(&result), "reran");
    assert!(!tool_result(&result).is_error);

    // The rerun cleared the interrupted attempt's progress; the settled slot is
    // `done` and carries the result entry.
    let live = reopened.live().await;
    let slot = &live["tools"][0];
    assert_eq!(slot["status"], json!("done"));
    assert_eq!(slot["entry"], json!(result_entry(&record).as_i64()));
    assert!(slot.get("output").is_none());
}

#[tokio::test]
async fn unsafe_replay_settles_interrupted_with_partial_output() {
    let agent = agent_with(
        vec![registration("echo", ReplayPolicy::Unsafe)],
        vec![registration("echo", ReplayPolicy::Unsafe)],
    );
    let world = World::open(Host::new(agent, text_exec("never runs"))).await;
    let entry = world.append_assistant("c1", "echo", json!({})).await;
    let task = TaskId::from_raw(9_002);
    world.seed_tool_slot(task, "echo", "partial output\n").await;
    world
        .seed_running(
            task,
            entry,
            ToolTaskCheckpoint::Execute {
                arguments: JsonObject::new(),
                replay: ReplayPolicy::Unsafe,
            },
        )
        .await;

    world.scheduler.join().await;
    let reopened = World::open_existing(&world).await;
    reopened.scheduler.resume();
    let record = reopened.terminal(task).await;

    match outcome(&record) {
        TaskOutcome::Failed { error, .. } => assert!(
            error
                .message
                .contains("interrupted and may have partially run"),
            "unexpected message: {}",
            error.message
        ),
        other => panic!("expected a failed task, got {other:?}"),
    }
    assert!(
        reopened.host.executed().is_empty(),
        "an unsafe intent is never rerun"
    );
    let entry = reopened.entry(result_entry(&record)).await;
    let result = tool_result(&entry);
    assert!(result.is_error);
    // The output committed so far is what the model gets, framed with the
    // harness diagnostics the model also sees.
    assert_eq!(
        entry_text(&entry),
        "partial output\n\n<harness>\n[error] Tool echo was interrupted and may have partially run\n</harness>"
    );
    assert_eq!(diagnostic_codes(&entry), vec!["interrupted".to_owned()]);
    let diagnostics = entry_diagnostics(&entry);
    assert_eq!(
        diagnostics[0]["message"],
        json!("Tool echo was interrupted and may have partially run")
    );

    // The slot settles `done` as well, so the UI stops showing it running.
    let live = reopened.live().await;
    assert_eq!(live["tools"][0]["status"], json!("done"));
    assert_eq!(
        live["tools"][0]["entry"],
        json!(result_entry(&record).as_i64())
    );
}

impl World {
    /// Reopen a scheduler over an existing world's storage, as after a crash.
    async fn open_existing(world: &World) -> Self {
        let host = Arc::new(Host {
            storage: world.host.storage.clone(),
            session: Mutex::new(None),
            agent: world.host.agent.clone(),
            exec: world.host.exec.clone(),
            limits: world.host.limits,
            settings: world.host.settings.clone(),
            hooks: world.host.hooks.clone(),
            validation_error: world.host.validation_error.clone(),
            executed: world.host.executed.clone(),
        });
        let definition =
            tool_task(host.clone() as Arc<dyn ToolHost>).expect("a valid pi.tool definition");
        let settings = Settings::default();
        let options = TaskSchedulerOptions {
            settings: Some(Arc::new(move || settings.clone())),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, world.storage.clone(), vec![definition])
            .await
            .expect("reopen the scheduler");
        host.attach(scheduler.session().clone());
        Self {
            scheduler,
            storage: world.storage.clone(),
            host,
            conversation: world.conversation,
        }
    }
}
