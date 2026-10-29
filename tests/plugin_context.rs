//! Contract tests for the shared host-service spine: [`PluginContext`] +
//! [`PluginServices`] + [`PluginHost::bind_context`].
//!
//! These tests use fakes only; the concrete service implementations that need
//! `AgentSession`/TUI come in later lanes. The assertions pin:
//!
//! - `on_context` is called once per bind with a per-plugin identity,
//! - synchronous reads return the exact `Arc` the service holds (no
//!   serialization on the Rust path),
//! - async dialogs / tool execution / exec reach the fake with typed
//!   arguments,
//! - rebinding bumps the epoch and makes every earlier context detectable as
//!   stale,
//! - no context bound == no behavior change (the additive guarantee).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value as JsonValue};

use pir::agent::nested_tool_calls::NestedToolOutcome;
use pir::agent::tool_batch::ToolResult;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::extensions::SessionContextSnapshot;
use pir::plugins::plugin::context::PluginServiceError;
use pir::plugins::plugin::services::{
    DialogOptions, ExecOptions, ExecResult, ExecuteToolOptions, ExtensionMode, NotifyLevel,
    SelectOption, ServiceFuture, ServiceResult, SessionService, StatusEntry, StatusService,
    ToolCallRef, ToolService, UiService,
};
use pir::plugins::plugin::{PluginContext, PluginServices};
use pir::plugins::{AgentPlugin, PluginHost, PluginInfo};

// ---------------------------------------------------------------------------
// Test plugin
// ---------------------------------------------------------------------------

/// Minimal plugin that records every context the host hands it.
struct ProbePlugin {
    info: PluginInfo,
    contexts: Mutex<Vec<PluginContext>>,
}

impl ProbePlugin {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            contexts: Mutex::new(Vec::new()),
        })
    }

    fn contexts(&self) -> Vec<PluginContext> {
        self.contexts.lock().unwrap().clone()
    }

    fn last_context(&self) -> PluginContext {
        self.contexts().last().cloned().expect("context was bound")
    }
}

impl AgentPlugin for ProbePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_context(&self, ctx: &PluginContext) {
        self.contexts.lock().unwrap().push(ctx.clone());
    }
}

fn host_with(plugin: &Arc<ProbePlugin>) -> PluginHost {
    PluginHost::build(None, vec![plugin.clone() as Arc<dyn AgentPlugin>])
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeSession {
    snapshot: Arc<SessionContextSnapshot>,
    entries: Arc<JsonValue>,
    cwd: Arc<Path>,
    model: Arc<Model>,
}

impl FakeSession {
    fn new(cwd: &str) -> Self {
        let model = Arc::new(Model {
            id: "fake-model".to_owned(),
            name: "Fake Model".to_owned(),
            api: "fake-api".to_owned(),
            provider: "fake-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".to_owned()],
            cost: ModelCost {
                rates: ModelCostRates {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                tiers: Vec::new(),
            },
            context_window: 1_000,
            max_tokens: 100,
            sampling_params: None,
            headers: None,
            thinking_token_budget_field: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params_by_thinking_level: None,
            model_type: None,
            compat: None,
        });
        let snapshot = Arc::new(SessionContextSnapshot {
            model: json!({ "id": "fake-model", "provider": "fake-provider" }),
            entries: json!([{ "type": "message", "id": "entry-1" }]),
            session_id: Some("session-fake".to_owned()),
            cwd: Some(cwd.to_owned()),
            ..Default::default()
        });
        Self {
            snapshot,
            entries: Arc::new(json!([{ "type": "message", "id": "entry-1" }])),
            cwd: Arc::from(Path::new(cwd)),
            model,
        }
    }
}

impl SessionService for FakeSession {
    fn snapshot(&self) -> Option<Arc<SessionContextSnapshot>> {
        Some(Arc::clone(&self.snapshot))
    }

    fn entries(&self) -> Option<Arc<JsonValue>> {
        Some(Arc::clone(&self.entries))
    }

    fn cwd(&self) -> Option<Arc<Path>> {
        Some(Arc::clone(&self.cwd))
    }

    fn model(&self) -> Option<Arc<Model>> {
        Some(Arc::clone(&self.model))
    }

    fn mode(&self) -> ExtensionMode {
        ExtensionMode::Rpc
    }

    fn thinking_level(&self) -> ThinkingLevel {
        ThinkingLevel::Low
    }

    fn session_name(&self) -> Option<Arc<str>> {
        Some(Arc::from("fake session"))
    }
}

#[derive(Default)]
struct FakeUi {
    select_calls: Mutex<Vec<(String, Vec<SelectOption>, DialogOptions)>>,
    notices: Mutex<Vec<(NotifyLevel, String)>>,
}

impl UiService for FakeUi {
    fn notify(&self, level: NotifyLevel, message: &str) {
        self.notices
            .lock()
            .unwrap()
            .push((level, message.to_owned()));
    }

    fn select(
        &self,
        title: &str,
        options: &[SelectOption],
        dialog: DialogOptions,
    ) -> ServiceFuture<'static, ServiceResult<Option<String>>> {
        self.select_calls
            .lock()
            .unwrap()
            .push((title.to_owned(), options.to_vec(), dialog));
        Box::pin(async { Ok(Some("beta".to_owned())) })
    }
}

#[derive(Default)]
struct FakeTools {
    executed: Mutex<Vec<(String, String, JsonValue, ExecuteToolOptions)>>,
    execs: Mutex<Vec<(String, Vec<String>, ExecOptions)>>,
}

impl ToolService for FakeTools {
    fn execute_tool(
        &self,
        caller_id: &str,
        name: &str,
        args: JsonValue,
        options: ExecuteToolOptions,
    ) -> ServiceFuture<'static, ServiceResult<NestedToolOutcome>> {
        self.executed
            .lock()
            .unwrap()
            .push((caller_id.to_owned(), name.to_owned(), args, options));
        let call_id = format!("{caller_id}/1");
        let tool_name = name.to_owned();
        Box::pin(async move {
            Ok(NestedToolOutcome {
                tool_call_id: call_id,
                tool_name,
                result: ToolResult {
                    content: "nested-ok".to_owned(),
                    images: Vec::new(),
                    details: None,
                    structured_content: None,
                    is_error: false,
                    terminate: false,
                },
                is_error: false,
                duration_ms: Some(1),
                usage: None,
            })
        })
    }

    fn exec(
        &self,
        command: &str,
        args: Vec<String>,
        options: ExecOptions,
    ) -> ServiceFuture<'static, ServiceResult<ExecResult>> {
        self.execs
            .lock()
            .unwrap()
            .push((command.to_owned(), args, options));
        Box::pin(async {
            Ok(ExecResult {
                stdout: "ok".to_owned(),
                stderr: String::new(),
                code: 0,
                killed: false,
            })
        })
    }

    fn update(&self, _call: &ToolCallRef, _partial: JsonValue) {}
}

#[derive(Default)]
struct FakeStatus {
    cells: Mutex<Vec<StatusEntry>>,
}

impl StatusService for FakeStatus {
    fn set_status(&self, key: &str, text: Option<&str>) {
        let mut cells = self.cells.lock().unwrap();
        cells.retain(|(cell, _)| cell.as_ref() != key);
        if let Some(text) = text {
            cells.push((Arc::from(key), Arc::from(text)));
        }
    }

    fn statuses(&self) -> Arc<[StatusEntry]> {
        Arc::from(self.cells.lock().unwrap().clone())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn bind_context_hands_typed_services_and_sync_reads_are_zero_copy() {
    let plugin = ProbePlugin::new("test-parity");
    let host = host_with(&plugin);
    assert!(host.context().is_none());
    assert!(plugin.contexts().is_empty());

    let session = Arc::new(FakeSession::new("/tmp/pir-first"));
    let expected_entries = Arc::clone(&session.entries);
    let expected_snapshot = Arc::clone(&session.snapshot);
    host.bind_context(PluginServices::new().with_session(session).build());

    // `on_context` fired exactly once, with this plugin's identity.
    let contexts = plugin.contexts();
    assert_eq!(contexts.len(), 1);
    let ctx = &contexts[0];
    assert_eq!(ctx.identity().path(), "<inline:test-parity>");
    assert_eq!(ctx.identity().name(), "test-parity");
    assert_eq!(ctx.identity().order(), 0);
    assert_eq!(ctx.identity().builtin(), None);

    // The first bind stamps epoch 1 and the context is live.
    assert_eq!(ctx.epoch(), 1);
    assert_eq!(host.context_epoch(), 1);
    assert!(ctx.is_active());
    assert!(ctx.check_active().is_ok());
    assert_eq!(ctx.bound_services(), 1);

    // Typed, allocation-free accessors plus Arc-preserving sync reads.
    let service = ctx.session().expect("session service");
    assert_eq!(
        service.cwd().as_deref().map(Path::as_os_str),
        Some(Path::new("/tmp/pir-first").as_os_str())
    );
    assert!(Arc::ptr_eq(
        &service.entries().expect("entries"),
        &expected_entries
    ));
    assert!(Arc::ptr_eq(
        &service.snapshot().expect("snapshot"),
        &expected_snapshot
    ));
    assert_eq!(service.model().expect("model").id, "fake-model");
    assert_eq!(service.mode(), ExtensionMode::Rpc);
    assert_eq!(service.thinking_level(), ThinkingLevel::Low);
    assert_eq!(service.session_name().as_deref(), Some("fake session"));

    // The host exposes the same bound context.
    let bound = host.context().expect("bound context");
    assert_eq!(bound.epoch(), ctx.epoch());
    assert!(bound.is_active());
}

#[tokio::test]
async fn async_dialog_tool_and_exec_calls_reach_fakes_with_typed_args() {
    let plugin = ProbePlugin::new("async-probe");
    let host = host_with(&plugin);
    let ui = Arc::new(FakeUi::default());
    let tools = Arc::new(FakeTools::default());
    host.bind_context(
        PluginServices::new()
            .with_ui(ui.clone())
            .with_tools(tools.clone())
            .build(),
    );
    let ctx = plugin.last_context();

    // `select` receives typed options/options and returns the fake value.
    let answer = ctx
        .ui()
        .expect("ui service")
        .select(
            "Pick one",
            &[
                SelectOption::text("alpha"),
                SelectOption::new("Beta", "beta"),
            ],
            DialogOptions::with_timeout(Duration::from_secs(3)),
        )
        .await
        .expect("dialog succeeds");
    assert_eq!(answer.as_deref(), Some("beta"));
    {
        let calls = ui.select_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Pick one");
        assert_eq!(
            calls[0].1,
            vec![
                SelectOption::text("alpha"),
                SelectOption::new("Beta", "beta")
            ]
        );
        assert_eq!(calls[0].2.timeout, Some(Duration::from_secs(3)));
    }

    // `execute_tool` moves the typed args through unchanged.
    let args = json!({ "text": "hi" });
    let outcome = ctx
        .tools()
        .expect("tool service")
        .execute_tool(
            "call-7",
            "echo",
            args.clone(),
            ExecuteToolOptions { background: true },
        )
        .await
        .expect("nested tool succeeds");
    assert_eq!(outcome.tool_call_id, "call-7/1");
    assert_eq!(outcome.tool_name, "echo");
    assert_eq!(outcome.result.content, "nested-ok");
    {
        let executed = tools.executed.lock().unwrap();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].0, "call-7");
        assert_eq!(executed[0].1, "echo");
        assert_eq!(executed[0].2, args);
        assert!(executed[0].3.background);
    }

    // `exec` receives the moved argv and typed options.
    let result = ctx
        .tools()
        .expect("tool service")
        .exec(
            "printf",
            vec!["hi".to_owned()],
            ExecOptions {
                cwd: Some(PathBuf::from("/tmp/pir-first")),
                timeout_ms: Some(500.0),
                signal: None,
            },
        )
        .await
        .expect("exec succeeds");
    assert_eq!(
        result,
        ExecResult {
            stdout: "ok".to_owned(),
            stderr: String::new(),
            code: 0,
            killed: false,
        }
    );
    let execs = tools.execs.lock().unwrap();
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].0, "printf");
    assert_eq!(execs[0].1, vec!["hi".to_owned()]);
    assert_eq!(execs[0].2.cwd, Some(PathBuf::from("/tmp/pir-first")));
    assert_eq!(execs[0].2.timeout_ms, Some(500.0));
}

#[test]
fn rebinding_bumps_epoch_replaces_context_and_old_epoch_is_stale() {
    let plugin = ProbePlugin::new("rebind-probe");
    let host = host_with(&plugin);

    host.bind_context(
        PluginServices::new()
            .with_session(Arc::new(FakeSession::new("/tmp/pir-first")))
            .build(),
    );
    let first = plugin.last_context();
    let first_epoch = first.epoch();
    assert!(first.is_active());

    host.bind_context(
        PluginServices::new()
            .with_session(Arc::new(FakeSession::new("/tmp/pir-second")))
            .build(),
    );
    let second = plugin.last_context();

    // Two activations -> two handoffs; the new context is live at a newer
    // epoch and the host stores exactly it.
    assert_eq!(plugin.contexts().len(), 2);
    assert_ne!(first.epoch(), second.epoch());
    assert_eq!(second.epoch(), first_epoch + 1);
    assert_eq!(host.context_epoch(), second.epoch());
    assert_eq!(host.context().expect("bound").epoch(), second.epoch());
    assert!(second.is_active());

    // The old context (and clones cached by a plugin) is detectable.
    assert!(!first.is_active());
    match first.check_active() {
        Err(PluginServiceError::Stale { epoch, current }) => {
            assert_eq!(epoch, first_epoch);
            assert_eq!(current, second.epoch());
        }
        other => panic!("expected Stale, got {other:?}"),
    }

    // Reads still serve the old service's last snapshot.
    assert_eq!(
        first.session().expect("old session").cwd().as_deref(),
        Some(Path::new("/tmp/pir-first"))
    );
    assert_eq!(
        second.session().expect("new session").cwd().as_deref(),
        Some(Path::new("/tmp/pir-second"))
    );

    // `require_*` combines staleness and presence checks.
    assert!(matches!(
        first.require_session(),
        Err(PluginServiceError::Stale { .. })
    ));
    assert!(second.require_session().is_ok());
}

#[test]
fn no_context_bound_keeps_pre_service_behavior() {
    let plugin = ProbePlugin::new("no-context");
    let host = host_with(&plugin);

    assert!(host.context().is_none());
    assert_eq!(host.context_epoch(), 0);
    assert!(plugin.contexts().is_empty());
}

#[test]
fn status_cells_set_get_and_measure_seam_is_available() {
    let plugin = ProbePlugin::new("status-probe");
    let host = host_with(&plugin);
    let status = Arc::new(FakeStatus::default());
    let session = Arc::new(FakeSession::new("/tmp/pir-status"));
    host.bind_context(
        PluginServices::new()
            .with_session(session)
            .with_status(status)
            .build(),
    );
    let ctx = plugin.last_context();

    ctx.status()
        .expect("status service")
        .set_status("todo", Some("2 open"));
    let snapshot = ctx.status().expect("status service").statuses();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].0.as_ref(), "todo");
    assert_eq!(snapshot[0].1.as_ref(), "2 open");
    assert_eq!(
        ctx.status()
            .expect("status service")
            .status("todo")
            .as_deref(),
        Some("2 open")
    );

    // The perf lane can measure accessor closures without adding criterion
    // here; this is a sanity assertion, not a budget.
    let ns = ctx.measure_ns(1_000, |ctx| {
        let entries = ctx.session().and_then(SessionService::entries);
        let statuses = ctx.status().map(StatusService::statuses);
        (entries, statuses)
    });
    assert!(ns < 1_000_000, "accessor probe took {ns} ns/call");
}
