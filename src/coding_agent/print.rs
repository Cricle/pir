//! Print mode: non-interactive single execution (upstream `modes/print-mode.ts`).
//!
//! Two output modes share this path:
//! - `text` (default): the final assistant text goes to stdout, followed (when
//!   any background tool call completed) by one `backgroundToolResult` frame
//!   per completion — see [`text_with_background_reports`];
//! - `json` (`--mode json`): the session header (when persisted) plus one
//!   JSON line per agent event and per background completion (see
//!   [`super::json`]).
//!
//! Session storage follows the resolved [`SessionPlan`] (JSONL only):
//! `--continue` / `--session` / `--session-id` / `--fork` resume or fork an
//! existing transcript; `--no-session` keeps everything in memory.
//!
//! Background tool calls (`background: true`) are fully supported: the print
//! host installs the same scheduler detach sink / registry / `backgroundToolResult`
//! delivery the interactive session uses ([`PrintBackground`]), and settles
//! the calls still in flight at exit with a bounded wait + abort + explicit
//! diagnostics ([`DEFAULT_BACKGROUND_EXIT_WAIT`]). The exit rule has no
//! silent arm: aborted calls are reported (stderr + a json abort frame), and
//! a completion still mid-delivery when the abort grace expires is reported
//! through [`BackgroundExit::undelivered`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::agent::agent_loop::{
    self, CompactionFn, LoopDeps, LoopOptions, PersistMessageFn, ToolCallWaitFn,
};
use crate::agent::tool_execution::ToolRegistry;
use crate::agent::tool_scheduler::ToolScheduler;
use crate::agent_session::background::{
    background_sink, BackgroundDelivery, BackgroundFollowUpChannel, BackgroundPersistFn,
    BackgroundRegistry, BACKGROUND_TOOL_RESULT_TYPE, DEFAULT_BACKGROUND_CALL_LIMIT,
};
use crate::agent_session::{
    BackgroundCallStatus, SessionEvent, SessionEventHandler, SESSION_IDLE, SESSION_RUNNING,
};
use crate::ai::provider::ProviderRegistry;
use crate::ai::types::{ContentBlock, Context, Message, TextContent, UserMessage, UserRole};
use crate::coding_agent::session_resolve::{open_session_manager, SessionPlan};
use crate::compaction::{self, CompactionConfig, ProjectedEntry};
use crate::extensions::{ExtensionConfig, SessionContextSnapshot};
#[cfg(feature = "mcp")]
use crate::plugins::builtin::McpPlugin;
use crate::plugins::PluginHost;
use crate::system_prompt::build_system_prompt;

/// The print host's shared session storage: per-message persistence and the
/// compaction hook both append during the run, so a mid-run crash (or a
/// termination signal) loses nothing already streamed. Public for tests/.
pub type SharedSession = Arc<tokio::sync::Mutex<Option<crate::session_manager::SessionManager>>>;

/// The projection snapshot handed to the compaction hook: the prompt-start
/// per-entry METADATA (source entry + contributed message count) and the leaf
/// path metadata (agent_session's `ProjectionSnapshot`). It carries no
/// messages: the hook slices the live context it is handed.
type ProjectionSnapshot = (
    Arc<Vec<compaction::ProjectionEntryMeta>>,
    Arc<Vec<compaction::PathEntryMeta>>,
);

/// Per-request transport hooks for a bare extension runtime — the same seam
/// AgentSession's `stream_options` builds (agent_session/stream.rs:37-91): the
/// extension `before_provider_request` payload hook and the
/// `after_provider_response` observer, installed ONLY when a handler exists so
/// extension-free runs never touch the transport. print-mode previously built
/// bare `StreamOptions`, so print runs silently lost both extension events;
/// upstream fires them from the session streamFn for EVERY provider request —
/// conversational, compaction-summary, and navigate-summary calls alike
/// (sdk.ts:420-434).
///
/// This is the JS-handle-only seam: it dispatches straight off the
/// [`crate::extensions::ExtensionHandle`] and never consults a plugin host, so
/// Rust builtins get no say. The print run path routes through
/// [`print_transport_hooks`] instead (the host owns the same JS handle, so
/// this function stays the no-host fallback and the direct-JS test seam).
/// Public for tests/.
pub fn extension_transport_hooks(
    runtime: Option<&crate::extensions::ExtensionHandle>,
) -> crate::ai::types::ProviderStreamHooks {
    let mut hooks = crate::ai::types::ProviderStreamHooks::default();
    let Some(runtime) = runtime else {
        return hooks;
    };
    if runtime.has_handlers("before_provider_request") {
        let runtime = Arc::new(runtime.clone());
        hooks.before_provider_request = Some(Arc::new(move |payload: serde_json::Value| {
            let runtime = Arc::clone(&runtime);
            Box::pin(async move {
                let mut event = serde_json::json!({
                    "type": "before_provider_request",
                    "payload": payload,
                });
                match runtime.emit("before_provider_request", &mut event).await {
                    Ok(outcome) => {
                        if outcome.result.is_null() {
                            payload
                        } else {
                            outcome.result
                        }
                    }
                    Err(error) => {
                        eprintln!("[ext] before_provider_request dispatch failed: {error}");
                        payload
                    }
                }
            })
        }));
    }
    if runtime.has_handlers("after_provider_response") {
        let runtime = Arc::new(runtime.clone());
        hooks.after_provider_response = Some(Arc::new(
            move |status: u16, headers: std::collections::BTreeMap<String, String>| {
                let runtime = Arc::clone(&runtime);
                Box::pin(async move {
                    let mut event = serde_json::json!({
                        "type": "after_provider_response",
                        "status": status,
                        "headers": headers,
                    });
                    if let Err(error) = runtime.emit("after_provider_response", &mut event).await {
                        eprintln!("[ext] after_provider_response dispatch failed: {error}");
                    }
                })
            },
        ));
    }
    hooks
}

/// Per-request transport hooks for the print host, routed through the unified
/// plugin host — the print twin of the session's streamFn seam
/// (agent_session/stream.rs:112-164): the `before_provider_request` payload
/// hook and the `after_provider_response` observer, installed ONLY when the
/// host has a handler (`PluginHost::has_handlers` — the JS runtime's precise
/// per-event answer plus the Rust plugins' declared interest), so
/// extension-free runs never touch the transport.
///
/// The host's ONE ordered dispatch (`PluginHost::emit`) runs the Rust builtins
/// and the JS adapter over the runtime handle the host owns, sharing the
/// merged-result contract the session reads: a null result keeps the original
/// payload, anything else replaces it. The JS side is dispatched exactly once,
/// by the adapter inside `PluginHost::emit` — this path never calls
/// `ExtensionHandle::emit` itself, so a JS handler and a Rust handler
/// (`tool-repair`'s Phase 0 sanitizer) can never double-run. With no host the
/// JS-handle-only fallback ([`extension_transport_hooks`]) keeps the previous
/// behavior for host-less callers.
pub fn print_transport_hooks(
    plugins: Option<&Arc<PluginHost>>,
    runtime: Option<&crate::extensions::ExtensionHandle>,
) -> crate::ai::types::ProviderStreamHooks {
    print_transport_hooks_with_model(plugins, runtime, None)
}

/// [`print_transport_hooks`] with the run's model, which the
/// `provider_stream_event` payload names per parsed event.
///
/// The per-chunk hook is fire-and-forget and bounded
/// ([`crate::agent_session::provider_stream_event_hook`]); it is installed
/// only when the unified host has a handler. Without a model the hook is
/// omitted, matching the historical print path (before/after only).
pub fn print_transport_hooks_with_model(
    plugins: Option<&Arc<PluginHost>>,
    runtime: Option<&crate::extensions::ExtensionHandle>,
    model: Option<&crate::ai::types::Model>,
) -> crate::ai::types::ProviderStreamHooks {
    let Some(plugins) = plugins else {
        return extension_transport_hooks(runtime);
    };
    let mut hooks = crate::ai::types::ProviderStreamHooks::default();
    if plugins.has_handlers("before_provider_request") {
        let plugins = Arc::clone(plugins);
        hooks.before_provider_request = Some(Arc::new(move |payload: serde_json::Value| {
            let plugins = Arc::clone(&plugins);
            Box::pin(async move {
                let mut event = serde_json::json!({
                    "type": "before_provider_request",
                    "payload": payload,
                });
                match plugins.emit("before_provider_request", &mut event).await {
                    Ok(outcome) => {
                        if outcome.result.is_null() {
                            payload
                        } else {
                            outcome.result
                        }
                    }
                    Err(error) => {
                        eprintln!("[ext] before_provider_request dispatch failed: {error}");
                        payload
                    }
                }
            })
        }));
    }
    if plugins.has_handlers("after_provider_response") {
        let plugins = Arc::clone(plugins);
        hooks.after_provider_response = Some(Arc::new(
            move |status: u16, headers: std::collections::BTreeMap<String, String>| {
                let plugins = Arc::clone(&plugins);
                Box::pin(async move {
                    let mut event = serde_json::json!({
                        "type": "after_provider_response",
                        "status": status,
                        "headers": headers,
                    });
                    if let Err(error) = plugins.emit("after_provider_response", &mut event).await {
                        eprintln!("[ext] after_provider_response dispatch failed: {error}");
                    }
                })
            },
        ));
    }
    if let Some(model) = model {
        if plugins.has_handlers("provider_stream_event") {
            hooks.on_provider_stream_event = Some(
                crate::agent_session::provider_stream_event_hook(plugins, model),
            );
        }
    }
    hooks
}

/// The model snapshot the print host pushes to its plugin host — the print
/// twin of `agent_session::session_ctx`'s `model_snapshot` (the serialized
/// `Model` object, which `ToolRepairPlugin::on_session_context` resolves to
/// its `before_provider_request` gate; a malformed/absent value clears the
/// gate).
///
/// `project_trusted: true` mirrors the extension runtime's INITIAL snapshot
/// (`RuntimeState::new`) and the partial-push shape
/// `take_validated_extension_runtime` uses (runtime_loader.rs:75-85): the push
/// goes through `PluginHost::update_session_context`, which also forwards it
/// to the JS handle the host owns, and print never pushed a JS-facing
/// snapshot before. `..Default::default()` would flip `ctx.isProjectTrusted()`
/// to `false` for every print-mode extension, so the push stays additive — it
/// adds the model and nothing else observably changes.
fn print_model_snapshot(model: &crate::ai::types::Model) -> SessionContextSnapshot {
    SessionContextSnapshot {
        model: serde_json::to_value(model).unwrap_or(serde_json::Value::Null),
        project_trusted: true,
        ..SessionContextSnapshot::default()
    }
}

/// The unified plugin host for the print host, built the way
/// `AgentSession::new_with_plugins` builds its own: the same JS runtime
/// handle plus the enabled Rust builtins (`mcp`, `codemode`, `context-mode`,
/// ...), one ordered dispatch list. `None` without an extension runtime.
///
/// The print registry merges the host's tools through
/// [`crate::agent::tool_execution::merge_plugin_tools`] (the same call
/// `AgentSession::merged_tool_registry` makes), so a builtin's model-facing
/// tools are declared to the provider exactly as they are in the interactive
/// host. That merge used to be MISSING here: print drives `agent_loop`
/// directly, so the builtins registered into this host and never reached a
/// request body (`tools=9` in the captured first request of a full build,
/// `scripts/perf/body_probe.py`). The host also gives the run the MCP reach
/// wait hook below and owns the builtin plugin lifecycle for the run. The JS
/// extension tools the print registry carries hand their `reachesTools`
/// declaration to the Rust side (D23 (f): the registry-export snapshot plus
/// the plugin's live per-call probe), so the hook has real input for them.
/// Public for tests/.
///
/// The settings-driven builtin gates are applied here through the SAME helper
/// the session host uses (`AgentSession::host_settings` +
/// `AgentSession::restrict_plan_for_builtins`): `pythonTool: false` and
/// `disabledBuiltinExtensions` remove their `builtin:<name>` entries from the
/// recorded plan before the host instantiates them. Print used to build its
/// host directly and skipped that gate, so `pythonTool: false` worked in the
/// TUI/SDK/RPC hosts but not in `pir print`.
pub fn print_plugin_host(
    extensions: Option<&crate::extensions::ExtensionHandle>,
) -> Option<Arc<PluginHost>> {
    let runtime = extensions?;
    let cwd = runtime.config().cwd;
    let settings = crate::agent_session::AgentSession::host_settings(Some(&cwd));
    crate::agent_session::AgentSession::restrict_plan_for_builtins(runtime, &settings);
    Some(Arc::new(PluginHost::build(
        Some(runtime.clone()),
        Vec::new(),
    )))
}

/// Dispatch the print host's `session_start` event through the unified plugin
/// host.
///
/// The host's ordered dispatch reaches the JS face (at its adapter's list
/// position, through the same runtime handle print used before) and every
/// native Rust plugin exactly once, with the upstream wire payload
/// `{type: "session_start", reason}`. Public so the print host's lifecycle
/// event can be pinned by tests.
pub async fn dispatch_print_session_start(host: &PluginHost, reason: &str) {
    let mut payload = serde_json::json!({ "type": "session_start", "reason": reason });
    if let Err(error) = host.emit("session_start", &mut payload).await {
        eprintln!("[ext] session_start dispatch failed: {error}");
    }
}

/// Dispatch the print host's `agent_settled` event to both faces exactly
/// once.
///
/// The unified host's ordered dispatch is used when the print host built one
/// (it reaches the JS adapter and the native plugins); a host-less caller
/// falls back to the bare runtime handle, which is what the print path used
/// before the host existed. `aborted` is the run's abort flag
/// (agent-session.ts:1849 `_emitAgentSettled`).
pub async fn dispatch_print_agent_settled(
    plugins: Option<&Arc<PluginHost>>,
    runtime: Option<&crate::extensions::ExtensionHandle>,
    aborted: bool,
) {
    let mut payload = serde_json::json!({
        "type": "agent_settled",
        "aborted": aborted,
    });
    match plugins {
        Some(host) => {
            if host.has_handlers("agent_settled") {
                if let Err(error) = host.emit("agent_settled", &mut payload).await {
                    eprintln!("[ext] agent_settled dispatch failed: {error}");
                }
            }
        }
        None => {
            if let Some(runtime) = runtime {
                if runtime.has_handlers("agent_settled") {
                    if let Err(error) = runtime.emit("agent_settled", &mut payload).await {
                        eprintln!("[ext] agent_settled dispatch failed: {error}");
                    }
                }
            }
        }
    }
}

/// Apply the `input` event to the print host's prompt text through the
/// unified plugin host.
///
/// This is the print twin of `agent_session::transform_prompt_input`: the
/// same `{type, text, images: [], source: "rpc", streamingBehavior: null}`
/// payload, dispatched through the host's ordered list so native Rust
/// plugins can transform the prompt exactly like JS handlers. A
/// `handled`/absent outcome leaves the text untouched (the print facade
/// always runs the model turn). `host` `None` keeps the JS-only route.
pub async fn transform_print_input(
    host: Option<&PluginHost>,
    runtime: Option<&crate::extensions::ExtensionHandle>,
    text: &str,
) -> String {
    let Some(host) = host else {
        return match runtime {
            Some(runtime) => crate::agent_session::transform_prompt_input(runtime, text).await,
            None => text.to_owned(),
        };
    };
    let mut payload = serde_json::json!({
        "type": "input",
        "text": text,
        "images": [],
        "source": "rpc",
        "streamingBehavior": null,
    });
    match host.emit("input", &mut payload).await {
        Ok(outcome) => {
            let action = outcome
                .result
                .get("action")
                .and_then(serde_json::Value::as_str);
            if action == Some("transform") {
                if let Some(transformed) = outcome
                    .result
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    return transformed.to_owned();
                }
            }
            text.to_owned()
        }
        Err(_) => text.to_owned(),
    }
}

/// The `mcp` builtin in a plugin host's ordered dispatch list, if loaded
/// (the same lookup `agent_session::mcp_reach` makes for the session).
#[cfg(feature = "mcp")]
fn mcp_plugin_of(plugins: &PluginHost) -> Option<&McpPlugin> {
    plugins
        .plugins()
        .iter()
        .find_map(|plugin| plugin.as_any()?.downcast_ref::<McpPlugin>())
}

/// The MCP reach wait hook for the print host's loop deps — the same hook
/// (and the same source) the interactive session wires through
/// [`LoopDeps::tool_call_wait`] (`AgentSession::tool_call_wait_hook`,
/// `agent_session/mcp_reach.rs`): the `mcp` builtin answers each call's
/// `McpPlugin::wait_for_tool_call`, so a call that may reach MCP servers
/// waits for them before the batch executes (upstream the `mcp` extension's
/// `tool_call` handler). The hook then anchors MCP discovery publication
/// exactly like the session's (agent_session/mcp_reach.rs:162-163).
/// `None` when there is no plugin host or its ordered dispatch list carries
/// no `mcp` builtin. Public for tests/.
#[cfg(feature = "mcp")]
pub fn print_tool_call_wait(plugins: Option<&Arc<PluginHost>>) -> Option<ToolCallWaitFn> {
    let plugins = Arc::clone(plugins?);
    mcp_plugin_of(&plugins)?;
    Some(Arc::new(
        move |registry: Arc<ToolRegistry>, calls: Vec<crate::agent::tool_batch::AgentToolCall>| {
            let plugins = Arc::clone(&plugins);
            Box::pin(async move {
                let Some(mcp) = mcp_plugin_of(&plugins) else {
                    return;
                };
                for call in &calls {
                    mcp.wait_for_tool_call(&registry, &call.tool_name, &call.arguments)
                        .await;
                }
                // Discovery-publication anchor (the session hook's item 2):
                // the wait may have connected servers, and an earlier MCP call
                // in this run may have refreshed the discovered tool snapshot
                // (`tools/list_changed` -> `tools_dirty`). Print emits through
                // the extension runtime directly (`runtime.emit`), never
                // through `PluginHost::emit`, so the host's tail-of-emit
                // refresh never runs for this host and the batch seam is its
                // only flag consumer. The cache refresh runs first, mirroring
                // `McpPlugin::on_event` ahead of the host's rebuild (the
                // resource tools' exposure feeds that rebuild's registration
                // pass), then the host consumes the flags and rebuilds.
                mcp.refresh_resource_servers().await;
                plugins.refresh_registry_if_changed().await;
            })
        },
    ))
}

/// Without the `mcp` feature there is no `mcp` builtin to reach, so the
/// print host has no MCP wait hook.
#[cfg(not(feature = "mcp"))]
pub fn print_tool_call_wait(plugins: Option<&Arc<PluginHost>>) -> Option<ToolCallWaitFn> {
    let _ = plugins;
    None
}

/// The print host's loop dependencies for one prompt turn (public for
/// tests/): the run registry, scheduler, extension runtime, tool-update
/// channel, the MCP reach wait hook ([`print_tool_call_wait`]), the resolved
/// starting thinking level, and the `tool-repair` builtin's `message_end`
/// repair hook. Extracted so the hook wiring is directly testable — the loop
/// calls the wait hook at its tool seam (agent_loop.rs, post-gate /
/// pre-`execute_batch`) and the repair hook between the extension
/// `message_end` dispatch and the terminal assignment.
///
/// `thinking_level` is the run's resolved starting level (see `run_print`'s
/// resolution) and must equal the `reasoning` baked into every
/// `StreamOptions` the print host sends: `LoopDeps::thinking_level` seeds the
/// loop's `ProviderRequest.thinking_level`, and `registry_stream*` replaces
/// the baked `options.reasoning` with it whenever the two differ. The
/// `LoopDeps` default is `off`, so a print run that left the field unset
/// asked every provider for NO reasoning at all even though its stream
/// options carried a level.
///
/// The returned deps carry NO detach sink and NO follow-up channel: the print
/// host installs both per prompt run through [`PrintBackground::install`]
/// (`run_print`) — or leaves them uninstalled (extension-free unit deps,
/// tests that pin the sink-less loop shape). A `background: true` call in a
/// sink-less print run is stripped by preflight and executes inline in its
/// batch, exactly like every other call.
///
/// `retry` keeps the documented [`LoopDeps`] default (the upstream settings
/// default: enabled, 3 retries) so a bare embedder of this builder is never
/// silently un-retried. `run_print` replaces it with
/// [`print_retry_policy`] over the run's own settings snapshot.
///
/// `max_turns` starts at [`crate::config::defaults::DEFAULT_AGENT_MAX_TURNS`]
/// (the non-interactive default) so a bare embedder is bounded but not cut off
/// at the old 100-turn ceiling; `run_print` replaces it with the process
/// config's value (`PIR_MAX_TURNS` aware).
pub fn print_loop_deps(
    tools: Arc<ToolRegistry>,
    scheduler: ToolScheduler,
    cwd: PathBuf,
    extensions: Option<crate::extensions::ExtensionHandle>,
    tool_updates: Option<crate::agent::tool_execution::ToolUpdateFn>,
    tool_call_wait: Option<ToolCallWaitFn>,
    thinking_level: crate::ai::types::ThinkingLevel,
) -> LoopDeps {
    LoopDeps {
        tools,
        scheduler,
        cwd,
        extensions,
        tool_updates,
        tool_call_wait,
        thinking_level,
        // Same runaway guard as the interactive session and RPC hosts: a
        // model that loops (measured: one logq leg issued the SAME bash call
        // 489 times, 511 turns / 17.4M tokens) must be bounded. The
        // non-interactive default leaves room for the measured soak shape
        // (one tool call per assistant message: a 100-call prompt spends 100
        // turns); `run_print` applies `PIR_MAX_TURNS` on top.
        max_turns: crate::config::defaults::DEFAULT_AGENT_MAX_TURNS,
        ..LoopDeps::default()
    }
}

/// The print host's agent-level turn retry policy from its settings snapshot.
///
/// The interactive session resolves `settings.retry` per prompt through
/// `SettingsManager::agent_retry_policy()` (`agent_session/prompt_loop.rs`);
/// print drives `agent_loop` itself and therefore has to install the policy on
/// `LoopDeps::retry` explicitly. [`crate::config::settings::SettingsManager::retry_policy_from`]
/// is the shared conversion: the upstream defaults fill every absent field, so
/// this resolves the SAME policy the session host would for the same settings.
///
/// `retry.enabled: false` (or `maxRetries: 0`) yields `None`, which the loop
/// treats as a single attempt; a custom `maxRetries`/backoff is honored. The
/// provider block is ignored here on purpose (transport retries are a separate
/// budget, `StreamOptions::max_retries`). An absent `retry` block is the
/// documented enabled default, not a disabled one.
///
/// Public for tests/ and for embedders that build print-shaped deps themselves.
pub fn print_retry_policy(
    settings: &crate::config::settings::Settings,
) -> Option<crate::ai::util::AgentRetryPolicy> {
    let retry = settings.retry.clone().unwrap_or_default();
    crate::config::settings::SettingsManager::retry_policy_from(&retry)
}

/// Install the settings-derived agent-level retry policy on a print run's
/// deps (`run_print` calls this for every prompt run). The single assignment
/// seam, so the wiring is directly testable without duplicating it.
pub fn print_apply_retry(deps: &mut LoopDeps, settings: &crate::config::settings::Settings) {
    deps.retry = print_retry_policy(settings);
}

/// Install the non-interactive turn cap on a print run's deps (`run_print`
/// calls this for every prompt run): [`crate::config::Config::max_turns`],
/// default 1000, `PIR_MAX_TURNS` aware. The single assignment seam, testable
/// like [`print_apply_retry`].
pub fn print_apply_max_turns(deps: &mut LoopDeps, config: &crate::config::Config) {
    deps.max_turns = config.max_turns;
}

/// Map one agent-level retry transition onto the session-event shape the
/// interactive host publishes (the exact mapping `agent_session/prompt_loop.rs`
/// installs on `LoopDeps::retry_events`).
///
/// Upstream streams `auto_retry_start` / `auto_retry_end` as ordinary session
/// events in print JSON mode (print-mode.ts `session.subscribe` →
/// `toJsonEvent`). The loop is the only writer of
/// [`agent_loop::AgentRetryEvent`]; the transport's separate retry observers
/// (`StreamOptions::hooks`) never pass through this mapping, so the two retry
/// layers cannot double-report the same retry.
pub fn agent_retry_event_to_session_event(event: &agent_loop::AgentRetryEvent) -> SessionEvent {
    match event {
        agent_loop::AgentRetryEvent::Start {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => SessionEvent::AutoRetryStart {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_ms: *delay_ms,
            error_message: error_message.clone(),
        },
        agent_loop::AgentRetryEvent::End {
            success,
            attempt,
            final_error,
        } => SessionEvent::AutoRetryEnd {
            success: *success,
            attempt: *attempt,
            final_error: final_error.clone(),
        },
    }
}

/// Install the agent-level retry lifecycle sink on a print run's deps
/// (`run_print` calls this for every prompt run, right after
/// [`print_apply_retry`]).
///
/// `sink` is the host's out-of-loop session-event channel: JSON mode passes
/// [`json_session_event_sink`], so a re-issued turn appears in `--mode json`
/// as the upstream `auto_retry_start` / `auto_retry_end` lines. TEXT mode
/// passes `None`, leaving the field uninstalled — text print mode streams no
/// session events and its output stays byte-identical. The transport retry
/// observers installed on `transport_hooks` are a different budget and stay
/// untouched (a `RetryableTurn` failure is the only input here; such a
/// failure never reached a transport verdict).
pub fn install_print_retry_events(deps: &mut LoopDeps, sink: Option<PrintSessionEventSink>) {
    deps.retry_events = sink.map(|sink| {
        let sink: agent_loop::AgentRetryEventSink =
            Arc::new(move |event: agent_loop::AgentRetryEvent| {
                sink(&agent_retry_event_to_session_event(&event));
            });
        sink
    });
}

// ---------------------------------------------------------------------------
// Background tool calls in the print host (`TOOL-BACKGROUND-SCOPE.md`)
// ---------------------------------------------------------------------------
//
// Upstream `pi` has no background tool-call concept at all (no
// `background` argument, no `BackgroundTool`/`waitForBackground` symbol in
// the whole package tree), so print mode has no upstream delivery to mirror;
// the closest upstream behavior is print-mode.ts's signal path, which kills
// its tracked detached shell children on SIGTERM/SIGHUP instead of waiting
// for them. The print host therefore reuses pir's OWN established semantics
// (`agent_session/background.rs`): the same registry/delivery/limit, the same
// `backgroundToolResult` transcript frame, the same `background_tool_end`
// event, the same placeholder + follow-up injection for a live run.
//
// The one print-only decision is the EXIT SEMANTICS (upstream leaves it
// undefined; HANDOFF.md §9 item 1 recorded it as a user decision):
// - after the last prompt, the host waits up to
//   [`DEFAULT_BACKGROUND_EXIT_WAIT`] for the calls that outlived it;
// - what is still running when the wait expires is ABORTED (never left
//   dangling), reported on stderr (both modes) and as the same-shaped
//   `background_tool_end` event (json mode), and makes text mode exit 1 —
//   a bounded wait can never hang the process, and losing the result is
//   never silent.

/// Bounded wait the print host grants the background tool calls still in
/// flight when the last prompt finished, before aborting them at exit.
///
/// Conservative reading of the undefined-upstream exit semantics: long enough
/// for the short/medium calls a `background: true` batch typically starts,
/// finite so a hung call can never hold a one-shot process forever (the
/// harness contract: a print run always terminates). A completion that lands
/// inside the wait is delivered in full (event + transcript + text/json
/// surfaces); a call the wait aborts is reported, never silently dropped.
pub const DEFAULT_BACKGROUND_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Extra grace after [`DEFAULT_BACKGROUND_EXIT_WAIT`] expired: the aborted
/// calls' cooperative cancellation (the shell tools' process-tree kill, the
/// extension runtime's abort flag) gets one more bounded window to run before
/// the process returns, so an abort is an actual abort and not just a dropped
/// task. Nothing is delivered in this window (the aborted deliveries are
/// already suppressed by the registry).
const BACKGROUND_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The print host's background-call surface: ONE registry + ONE delivery
/// bundle for the whole invocation (shared across every prompt of this print
/// process), the run-state claim that gates the live follow-up injection, the
/// session-event list the delivery reports into, and the log of every
/// completion the host observed.
///
/// Built once per `run_print` and installed on each prompt run's deps
/// ([`Self::install`]) — the print twin of the session's construction-time
/// sink installation (`agent_session/build.rs`), over the print host's own
/// `SessionManager` handle and event surfaces.
pub struct PrintBackground {
    /// The in-flight-call registry (limit, per-call abort handles, delivery
    /// count). The same type the session installs.
    registry: Arc<BackgroundRegistry>,
    /// The completion task's owned handles (persist / events / follow-up /
    /// run state / extension merge). Cloned into the sink.
    delivery: BackgroundDelivery,
    /// The delivery-side follow-up channel; its poll is installed on
    /// `LoopDeps::follow_up` by [`Self::install`].
    follow_up: BackgroundFollowUpChannel,
    /// The host's run claim ([`SESSION_RUNNING`] while a prompt run is in
    /// flight): only then does a completion inject into the live loop.
    state: Arc<AtomicU8>,
    /// The host's session-event subscribers (the delivery reports here).
    events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
    /// Every completion the host observed, in settle order — the record the
    /// text surface renders and tests assert on.
    reports: Arc<std::sync::Mutex<Vec<SessionEvent>>>,
}

impl PrintBackground {
    /// Build the surface for one print invocation: the registry, the delivery
    /// over the print host's shared session (an ephemeral run's `None` storage
    /// simply persists nothing) and extension handle, the follow-up channel,
    /// and the completion recorder.
    pub fn new(
        session: SharedSession,
        extensions: Option<crate::extensions::ExtensionHandle>,
    ) -> Arc<Self> {
        let events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>> =
            Arc::new(std::sync::RwLock::new(Vec::new()));
        let reports: Arc<std::sync::Mutex<Vec<SessionEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let reports = Arc::clone(&reports);
            events
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::new(move |event: SessionEvent| {
                    // Only completions are recorded: the delivery emits
                    // nothing else on this list, and the print host has no
                    // other session-event source.
                    if matches!(event, SessionEvent::BackgroundToolEnd { .. }) {
                        reports
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(event);
                    }
                }));
        }
        // The host's persistence leg of the delivery: append the framed
        // `backgroundToolResult` custom entry to the shared session manager.
        // A `--no-session` run (`None` inside the lock) keeps no transcript —
        // then the recorded event is the only delivery surface, exactly like
        // the session's store-less path.
        let persist = Some(print_background_persist(Arc::clone(&session)));
        let follow_up = BackgroundFollowUpChannel::new();
        let registry = Arc::new(BackgroundRegistry::new(DEFAULT_BACKGROUND_CALL_LIMIT));
        let state = Arc::new(AtomicU8::new(SESSION_IDLE));
        let delivery = BackgroundDelivery::new(
            persist,
            Arc::clone(&events),
            follow_up.inbox(),
            Arc::clone(&state),
            extensions,
        );
        Arc::new(Self {
            registry,
            delivery,
            follow_up,
            state,
            events,
            reports,
        })
    }

    /// Install the detach sink and the completion follow-up channel on one
    /// prompt run's deps. Called by `run_print` for every prompt of the
    /// invocation (and by tests that drive the loop themselves), so a
    /// `background: true` call is adopted by this host's registry instead of
    /// running inline.
    pub fn install(&self, deps: &mut LoopDeps) {
        deps.scheduler = deps.scheduler.clone().with_detach(background_sink(
            Arc::clone(&self.registry),
            self.delivery.clone(),
        ));
        // This invocation's commit tracker: the placeholder gates (A1) and the
        // tool-batch barrier (B1) belong to THIS host's registry, never to the
        // process, so the completion can only land at a batch boundary of the
        // run that owns it.
        deps.commit_tracker = Some(self.registry.commit_tracker());
        // The print host has no interactive user follow-ups; its only
        // follow-up source is detached background completions.
        deps.follow_up = None;
        deps.background_follow_up = Some(self.follow_up.follow_up_fn());
    }

    /// Claim the run state for one prompt run: a completion settling from now
    /// on is injected into the live loop's next turn boundary (the session's
    /// §2.4.2 route). Without the claim (between prompts, after the last
    /// prompt) the completion still persists and reports, but never reaches a
    /// running turn — there is none.
    pub fn begin_run(&self) {
        self.state.store(SESSION_RUNNING, Ordering::SeqCst);
    }

    /// Release the run claim ([`Self::begin_run`]'s counterpart).
    pub fn end_run(&self) {
        self.state.store(SESSION_IDLE, Ordering::SeqCst);
    }

    /// The in-flight calls (registry snapshot), in registration order.
    pub fn calls(&self) -> Vec<BackgroundCallStatus> {
        self.registry.statuses()
    }

    /// How many calls are in flight right now.
    pub fn call_count(&self) -> usize {
        self.registry.len()
    }

    /// Every completion the host observed, in settle order (public for the
    /// text surface and tests/).
    pub fn reports(&self) -> Vec<SessionEvent> {
        self.reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Subscribe the `--mode json` frame stream: every completion is written
    /// to stdout immediately as its `background_tool_end` wire event (the
    /// exact shape the RPC host forwards — `SessionEvent` →
    /// `session_event_to_json`), in settle order, like every other print-mode
    /// event line.
    pub fn stream_events_as_json(&self) {
        self.events
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::new(|event: SessionEvent| {
                crate::coding_agent::json::print_session_event(&event);
            }));
    }

    /// Wait until every background call has finished delivering, or `budget`
    /// elapses. Returns whether the host drained inside the budget.
    pub async fn wait_for_idle(&self, budget: std::time::Duration) -> bool {
        self.registry.wait_for_idle(budget).await
    }

    /// The exit-time settle: wait up to `budget` for the calls that outlived
    /// the last prompt; what is still running is aborted (with
    /// [`BACKGROUND_ABORT_GRACE`] for the cancellation to take effect) and
    /// reported through the returned [`BackgroundExit`]. The caller owns the
    /// user-facing diagnostics (stderr + the json abort frames) — the decision
    /// stays here, the I/O stays with `run_print`.
    ///
    /// The wait covers a settled call's DELIVERY too (the registry's
    /// `in_flight` counts a reservation until the completion task finished
    /// persisting/injecting), so the returned
    /// [`BackgroundExit::undelivered`] is the last honesty check: a delivery
    /// that outlived both the budget and the abort grace is reported instead
    /// of exiting silently with a half-written result.
    pub async fn settle(&self, budget: std::time::Duration) -> BackgroundExit {
        if self.registry.wait_for_idle(budget).await {
            return BackgroundExit {
                timed_out: false,
                aborted: Vec::new(),
                undelivered: 0,
            };
        }
        let aborted = self.registry.statuses();
        self.registry.abort_all();
        let _ = self.registry.wait_for_idle(BACKGROUND_ABORT_GRACE).await;
        BackgroundExit {
            timed_out: !aborted.is_empty(),
            aborted,
            // Entries are already settle-free once their call completed (the
            // delivery takes the entry BEFORE persisting), so a nonzero count
            // here means a completion task is still mid-delivery — there is
            // no call left to abort, only a result to report as unfinished.
            undelivered: self.registry.in_flight(),
        }
    }

    /// Abort every in-flight call without waiting for completions (the
    /// terminal-error exit path: the run failed and its caller gets an error
    /// instead of a text payload, so waiting for results serves nobody). The
    /// aborted calls are returned for the caller's diagnostics; their
    /// deliveries are suppressed by the registry like every abort.
    pub async fn abort_in_flight(&self) -> Vec<BackgroundCallStatus> {
        let aborted = self.registry.statuses();
        self.registry.abort_all();
        let _ = self.registry.wait_for_idle(BACKGROUND_ABORT_GRACE).await;
        aborted
    }
}

impl crate::agent::tool_execution::BackgroundControl for PrintBackground {
    fn calls(&self) -> Vec<crate::agent::tool_execution::BackgroundCallInfo> {
        self.registry
            .statuses()
            .into_iter()
            .map(|status| crate::agent::tool_execution::BackgroundCallInfo {
                call_id: status.call_id,
                tool_name: status.tool_name,
                parent_call_id: status.parent_call_id,
            })
            .collect()
    }

    fn abort(&self, call_id: &str) -> bool {
        self.registry.abort(call_id)
    }

    fn abort_all(&self) -> usize {
        self.registry.abort_all()
    }
}

/// The print host's persistence leg of the background delivery: append the
/// framed `backgroundToolResult` custom entry to the shared session manager
/// (the same append the interactive session's `BackgroundDelivery` performs
/// over its manager handle, `agent_session/build.rs`). A `--no-session` run
/// (`None` inside the lock) keeps no transcript — the delivery's event is
/// then the only record.
fn print_background_persist(session: SharedSession) -> BackgroundPersistFn {
    Arc::new(
        move |framed: String,
              details: JsonValue|
              -> crate::agent::tool_execution::BoxFuture<'static, ()> {
            let session = Arc::clone(&session);
            Box::pin(async move {
                let mut guard = session.lock().await;
                if let Some(session) = guard.as_mut() {
                    if let Err(error) = session.append_custom_message_entry(
                        BACKGROUND_TOOL_RESULT_TYPE,
                        JsonValue::String(framed),
                        false,
                        Some(details),
                    ) {
                        eprintln!("pir: failed to persist a background tool result: {error}");
                    }
                }
            })
        },
    )
}

/// What the print host's exit-time background settle did.
#[derive(Debug, Clone, Default)]
pub struct BackgroundExit {
    /// At least one call was still running when the bounded wait expired and
    /// had to be aborted (its result was lost; the caller reported it).
    pub timed_out: bool,
    /// The calls aborted at exit, in registry order (empty when the host
    /// drained inside the wait).
    pub aborted: Vec<BackgroundCallStatus>,
    /// Completion tasks that were still DELIVERING (persisting / injecting)
    /// when both the budget and the abort grace expired: the call itself
    /// settled (there is nothing left to abort and nothing was lost from the
    /// event stream), but a surface (the transcript entry, the follow-up
    /// injection) may be incomplete at exit. `0` for every normal exit.
    pub undelivered: usize,
}

impl BackgroundExit {
    /// The text-mode exit-code contribution of this disposition: `1` when the
    /// host had to give up on a call (a requested result is missing from the
    /// output — the conservative signal) or a completion was still mid-delivery
    /// at exit, else `0`. Json mode keeps its in-band convention (exit 0, the
    /// abort frames / stderr diagnostics carry the failure).
    pub fn exit_code(&self) -> i32 {
        if self.timed_out || self.undelivered > 0 {
            1
        } else {
            0
        }
    }
}

/// The completion frame of one delivered `SessionEvent::BackgroundToolEnd`:
/// the exact bytes [`crate::agent_session::background`]'s
/// `background_result_frame` stored in the `backgroundToolResult` entry, so
/// the text surface and the transcript agree verbatim. `None` for any other
/// event.
pub fn background_frame_of(event: &SessionEvent) -> Option<String> {
    match event {
        SessionEvent::BackgroundToolEnd {
            call_id,
            tool_name,
            is_error,
            duration_ms,
            output,
            ..
        } => Some(crate::agent_session::background::background_result_frame(
            call_id,
            tool_name,
            *is_error,
            *duration_ms,
            output,
        )),
        _ => None,
    }
}

/// The text-mode stdout payload: the run's final assistant text followed by
/// one framed block per background completion the host observed (the
/// `backgroundToolResult` frames, in settle order), separated by one blank
/// line. The upstream text contract ("the final response only") stays the
/// FIRST block; the completion frames are appended so an out-of-run result is
/// visible on stdout instead of silently vanishing — upstream has no
/// counterpart to mirror here, and stderr would make the result invisible to
/// the piping consumer that asked for the run's output.
pub fn text_with_background_reports(text: &str, reports: &[SessionEvent]) -> String {
    let frames: Vec<String> = reports.iter().filter_map(background_frame_of).collect();
    if frames.is_empty() {
        return text.to_owned();
    }
    let joined = frames.join("\n\n");
    if text.trim().is_empty() {
        joined
    } else {
        format!("{text}\n\n{joined}")
    }
}

/// The exit-time abort diagnostic for one call, in the same
/// `BackgroundToolEnd` event shape every completion uses: `isError: true` (no
/// result was produced), the `reason` in both the output sentence and the
/// `details.aborted` marker, so a json consumer can tell an aborted call from
/// a failed one. There is no pir precedent for aborting a background call
/// mid-flight (the session's abort delivers nothing at all), but print is
/// EXITING — silence would be the only alternative to this frame.
pub fn background_abort_event(status: &BackgroundCallStatus, reason: &str) -> SessionEvent {
    SessionEvent::BackgroundToolEnd {
        call_id: status.call_id.clone(),
        tool_name: status.tool_name.clone(),
        // Keep the adoption-time attribution: an aborted nested detach still
        // belongs to its requesting call.
        parent_call_id: status.parent_call_id.clone(),
        is_error: true,
        duration_ms: None,
        output: format!(
            "Background tool call '{}' was aborted at print exit ({reason}); its result was not delivered.",
            status.tool_name
        ),
        // No result was produced, so there are no image/structured surfaces.
        images: Vec::new(),
        details: Some(serde_json::json!({
            "aborted": true,
            "reason": reason,
        })),
        structured_content: None,
        // No result was produced, so there is no handler-reported usage.
        usage: None,
    }
}

/// The stderr line for one call the print host aborted at exit (written in
/// both output modes: text mode has no frame stream, and json consumers
/// still deserve the human-readable trace).
pub fn background_abort_diagnostic(status: &BackgroundCallStatus, reason: &str) -> String {
    format!(
        "pir: background tool call '{}' ({}) aborted: {reason}",
        status.tool_name, status.call_id
    )
}

/// Provider tool definitions for a bare print declared set (no
/// `prepareLoadout` context): the shared session builder
/// ([`crate::agent::tool_execution::declared_tool_definitions`]), so every
/// declared schema carries the scheduler-owned `background` argument (the
/// provider-visible half of the feature) and the strict JSON-schema sampling
/// preference. `run_print` builds its request through
/// [`PrintToolLoadout::definitions`] — this builder plus the hook results —
/// because the bare [`crate::coding_agent::tools::tool_definitions`] declared
/// no `background` argument at all.
pub fn print_tool_definitions(registry: &ToolRegistry) -> Vec<crate::ai::types::Tool> {
    let declared: Vec<Arc<dyn crate::agent::tool_execution::AgentTool>> =
        registry.iter().cloned().collect();
    crate::agent::tool_execution::declared_tool_definitions(
        &declared,
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
    )
}

impl PrintToolLoadout {
    /// The provider tool definitions this loadout declares: the declared set
    /// with the [`crate::agent::tool_execution::AgentTool::prepare_loadout`]
    /// changes applied — description rewrites, and the hidden declarations
    /// left out of the request while their tools stay active and callable
    /// (upstream `_applyToolLoadout` + `_installHiddenDeclarationsProjection`,
    /// agent-session.ts:1580-1625).
    pub fn definitions(&self) -> Vec<crate::ai::types::Tool> {
        let declared_registry = filter_registry(&self.registry, &self.declared);
        let declared: Vec<Arc<dyn crate::agent::tool_execution::AgentTool>> =
            declared_registry.iter().cloned().collect();
        crate::agent::tool_execution::declared_tool_definitions(
            &declared,
            &self.descriptions,
            &self.hidden_declarations,
        )
    }
}

/// Re-derive the declared provider definitions for an active set captured
/// AFTER [`print_tool_loadout`]'s first build — the `tool_search` activation
/// path (`ToolSessionEnv::active_override`).
///
/// The print host builds its loadout once and replays it for every prompt of
/// the invocation; a search writes the loaded names into that cell, so the
/// next prompt's declarations must follow the cell exactly like a session's
/// per-iteration `build_tool_loadout`. The executable registry and callable
/// set stay the first build's (print never narrowed the registry by the
/// selection, so a load only widens the DECLARED set — the same names the
/// loadout would have derived had the selection named them up front).
/// Public for tests/.
pub fn print_active_definitions(
    registry: &ToolRegistry,
    active: &[String],
) -> Vec<crate::ai::types::Tool> {
    let declared: Vec<String> = crate::agent::tool_execution::declared_tools(registry, active)
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    let declared_registry = filter_registry(registry, &declared);
    let declared: Vec<Arc<dyn crate::agent::tool_execution::AgentTool>> =
        declared_registry.iter().cloned().collect();
    let (descriptions, hidden_declarations) = loadout_hook_changes(registry, active);
    crate::agent::tool_execution::declared_tool_definitions(
        &declared,
        &descriptions,
        &hidden_declarations,
    )
}

/// The print host's [`ToolSessionEnv`](crate::agent::tool_execution::ToolSessionEnv)
/// for one prompt run.
///
/// Print drives `agent_loop` without an `AgentSession`, so this env is the
/// only place the tools' session services (background control, the live
/// registry, the active-set cell) live. `resolved_active` is the run's
/// resolution of the tool selection — the same names
/// [`print_tool_loadout`] declares — and it is handed over as
/// [`ToolSessionEnv::active_override`]: `tool_search`'s baseline becomes the
/// run's active set (upstream `getActiveTools()`) instead of the exposure
/// defaults, so the search's additive union keeps every selected name,
/// `tool_search` itself included. The cell is shared by every prompt of the
/// invocation ([`run_print`] builds it once) and only written by
/// `tool_search` / the JS `setActiveTools` store, never re-seeded, so both
/// keep their precedence. No other tool reads the field (grep
/// `ToolSessionEnv.active_override`): the env change is invisible to every
/// other print behavior. Public for tests/.
pub fn print_tool_session_env(
    provider_registry: Arc<ProviderRegistry>,
    model: &crate::ai::types::Model,
    thinking: crate::ai::types::ThinkingLevel,
    registry: Arc<ToolRegistry>,
    resolved_active: Arc<std::sync::RwLock<Option<Vec<String>>>>,
    background: Option<Arc<dyn crate::agent::tool_execution::BackgroundControl>>,
) -> Arc<crate::agent::tool_execution::ToolSessionEnv> {
    print_tool_session_env_with_session(
        provider_registry,
        model,
        thinking,
        registry,
        resolved_active,
        background,
        None,
        None,
    )
}

/// [`print_tool_session_env`] with the run's persisted-session identity: the
/// print host exposes these through `PI_SESSION_ID` / `PI_SESSION_FILE` in
/// shell children exactly like an `AgentSession` run (upstream
/// `ctx.sessionManager.getSessionId()` / `getSessionFile()`). An ephemeral
/// `--no-session` run passes `None` for both.
// The parameters are the same session-env inputs the shorter
// `print_tool_session_env` forwards; grouping them would duplicate that
// struct's fields at the call site.
#[allow(clippy::too_many_arguments)]
pub fn print_tool_session_env_with_session(
    provider_registry: Arc<ProviderRegistry>,
    model: &crate::ai::types::Model,
    thinking: crate::ai::types::ThinkingLevel,
    registry: Arc<ToolRegistry>,
    resolved_active: Arc<std::sync::RwLock<Option<Vec<String>>>>,
    background: Option<Arc<dyn crate::agent::tool_execution::BackgroundControl>>,
    session_id: Option<String>,
    session_file: Option<std::path::PathBuf>,
) -> Arc<crate::agent::tool_execution::ToolSessionEnv> {
    Arc::new(crate::agent::tool_execution::ToolSessionEnv {
        provider_registry,
        model: model.clone(),
        thinking,
        session_id,
        session_file,
        base_tools: Arc::clone(&registry),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: None,
        // Print has no explicit agent-directory identity: it reads the same
        // process-resolved directory its own settings/model resolution used,
        // so a child run resolves the identical inputs.
        agent_dir: Some(crate::coding_agent::context::agent_dir()),
        // Print's request key is resolved per prompt by its stream options,
        // not stored on a session; child runs resolve their own from the
        // authority above (models.json/auth/env), exactly like this host.
        credential: None,
        // The loop's `tool_updates` channel takes precedence over this one
        // (`ToolContext::update_channel`), so no duplicate source.
        on_update: None,
        live_tools: Some(registry),
        active_override: Some(resolved_active),
        background,
    })
}

/// This prompt's request declarations ([`run_print`]'s `Context::tools`).
///
/// The loadout's first build (`first_build`) unless the env's active-set cell
/// has moved away from the selection it was seeded with: then the declared
/// set is re-derived from the cell, the print twin of a session's
/// per-iteration `build_tool_loadout` (the cell is the loadout chain's FIRST
/// precedence source, so it is followed verbatim rather than unioned).
/// `tool_search` only ever writes `[..., active, ...matches]`, and the seed is
/// the selection, so a search always ADDS declarations; a JS
/// `setActiveTools` narrowing is honoured the same way a session honours it.
/// `active` is the selection the loadout resolved (the cell's seed), so an
/// unchanged cell never re-derives. Public for tests/.
pub fn print_request_definitions(
    registry: &ToolRegistry,
    first_build: &[crate::ai::types::Tool],
    active: &[String],
    cell: Option<&[String]>,
) -> Vec<crate::ai::types::Tool> {
    match cell {
        Some(names) if names != active => print_active_definitions(registry, names),
        _ => first_build.to_vec(),
    }
}

/// Run the Rust `prepareLoadout` fan-out for `active` (upstream
/// `_applyToolLoadout`): the union of every declared tool's description
/// rewrites and hidden declarations.
///
/// Shared by [`print_tool_loadout`]'s one-time build and
/// [`print_active_definitions`]'s per-prompt rebuild, so the two agree
/// exactly. Only the Rust hooks run (see the build's comment); the in-tree
/// hook users are Rust.
fn loadout_hook_changes(
    registry: &ToolRegistry,
    active: &[String],
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeSet<String>,
) {
    let declared_tools = crate::agent::tool_execution::declared_tools(registry, active);
    let callable_tools = crate::agent::tool_execution::callable_tools(registry, active);
    let mut exposures = std::collections::BTreeMap::new();
    let mut namespaces = std::collections::BTreeMap::new();
    for tool in registry.iter() {
        exposures.insert(tool.name().to_owned(), tool.exposure());
        if let Some(namespace) = tool.namespace() {
            namespaces.insert(tool.name().to_owned(), namespace);
        }
    }
    let hook_loadout = crate::agent::tool_execution::ToolLoadout {
        declared: declared_tools,
        callable: callable_tools,
        registered: registry.iter().cloned().collect(),
        exposures,
        namespaces,
    };
    let mut descriptions = std::collections::BTreeMap::new();
    let mut hidden_declarations = std::collections::BTreeSet::new();
    for tool in &hook_loadout.declared {
        if let Some(changes) = tool.prepare_loadout(&hook_loadout) {
            descriptions.extend(changes.descriptions);
            hidden_declarations.extend(changes.hidden_declarations);
        }
    }
    (descriptions, hidden_declarations)
}

/// The declared names and the prompt's `hiddenTools` input for one active set,
/// re-derived exactly like [`print_tool_loadout`]'s one-time build (upstream
/// `_rebuildSystemPrompt`'s `selectedTools` + `hiddenTools`,
/// agent-session.ts:1705-1731).
///
/// The declared names keep the active order ([`crate::agent::tool_execution::
/// declared_tools`]); the hidden list is the loadout's: every registered name
/// the declared set leaves out, then the `prepareLoadout` hidden declarations
/// (already-active tools whose declarations requests omit). This is the same
/// pair `print_tool_loadout` builds, so a rebuild renders the same surface a
/// loadout resolved that way would have declared up front.
fn print_prompt_state(registry: &ToolRegistry, active: &[String]) -> (Vec<String>, Vec<String>) {
    let declared: Vec<String> = crate::agent::tool_execution::declared_tools(registry, active)
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    let declared_set: std::collections::BTreeSet<&str> =
        declared.iter().map(String::as_str).collect();
    let mut hidden: Vec<String> = registry
        .iter()
        .map(|tool| tool.name().to_string())
        .filter(|name| !declared_set.contains(name.as_str()))
        .collect();
    let (_, hidden_declarations) = loadout_hook_changes(registry, active);
    let not_already_hidden: Vec<String> = hidden_declarations
        .iter()
        .filter(|name| !hidden.iter().any(|hidden| hidden == *name))
        .cloned()
        .collect();
    hidden.extend(not_already_hidden);
    (declared, hidden)
}

/// What a [`PrintPromptCache`] last rendered, keyed by the declared surface.
struct PrintedPrompt {
    /// The active set the prompt was rendered from (the loadout's selection or
    /// the run's active-set cell) — the cheap steady-state key.
    active: Vec<String>,
    /// The declared names the render saw, in active order.
    declared: Vec<String>,
    /// The prompt's `hiddenTools` input: every registered name the declared
    /// set leaves out, plus the `prepareLoadout` hidden declarations.
    hidden: Vec<String>,
    /// The `mcp_servers` section the prompt was rendered with (`None` = no
    /// such section). Re-read per prompt; a change re-renders.
    mcp_servers: Option<String>,
    /// The rendered system prompt.
    prompt: String,
}

/// The print system prompt, re-rendered per prompt when the DECLARED set
/// moves — the session host's D6 behavior (`AgentSession::build_tool_loadout`
/// → `sync_context_prompt`) for the host that drives `agent_loop` itself.
///
/// Print resolves one loadout for the whole invocation, but `tool_search` (and
/// any other writer of `ToolSessionEnv`'s active-set cell: a JS
/// `setActiveTools`, MCP discovery activation) can widen the declared set
/// between prompts. The request declarations already follow the cell
/// ([`print_request_definitions`]); without this the prompt's `<tools>` /
/// `<rules>` / skills hint would keep describing the first prompt's surface,
/// so a loaded match would be declared in the request body but absent from the
/// system prompt. Rendering uses the SAME discovered inputs captured before
/// the first prompt — context files and skills are never re-read — and the
/// `--system-prompt` / `SYSTEM.md` override stays the first build's.
///
/// Steady state is one comparison: when the active set is the one the cached
/// prompt was rendered from, the prompt is returned byte-identically (no hook
/// fan-out, no render). A changed active set re-derives the declared set and
/// hidden declarations and re-renders only when that visible surface actually
/// differs (upstream `_rebuildSystemPrompt` on every active-set change,
/// agent-session.ts:1705-1731). Public for tests/.
pub struct PrintPromptCache {
    /// The fixed prompt override (`--system-prompt` / SYSTEM.md), if any.
    preamble_override: Option<String>,
    /// `--append-system-prompt` addenda (+ APPEND_SYSTEM.md).
    append: Vec<String>,
    /// Context files, read once before the first prompt.
    project_files: Vec<(PathBuf, String)>,
    /// Discovered skills, read once before the first prompt.
    skills: Vec<crate::skills::SkillInfo>,
    /// Working directory (the prompt's `cwd` section).
    cwd: PathBuf,
    /// Package asset dir, resolved once per invocation.
    package_dir: Option<PathBuf>,
    /// The `mcp_servers` section captured from the MCP manager's synchronous
    /// status snapshot (upstream re-renders it per `before_agent_start`,
    /// `extensions/mcp/index.ts:1176-1184`). `None` when MCP is off or has no
    /// enabled servers, so an MCP-less prompt keeps its previous bytes.
    mcp_servers: Option<String>,
    /// The last render, if any.
    rendered: Option<PrintedPrompt>,
}

impl PrintPromptCache {
    /// Capture the invocation's discovery inputs. They are the exact values
    /// `run_print` reads once before the loop, so a rebuild is a pure
    /// re-render (no disk IO).
    pub fn new(
        preamble_override: Option<String>,
        append: Vec<String>,
        project_files: Vec<(PathBuf, String)>,
        skills: Vec<crate::skills::SkillInfo>,
        cwd: PathBuf,
        package_dir: Option<PathBuf>,
    ) -> Self {
        Self::new_with_mcp_servers(
            preamble_override,
            append,
            project_files,
            skills,
            cwd,
            package_dir,
            None,
        )
    }

    /// [`Self::new`] with the invocation's `mcp_servers` section (see
    /// [`crate::coding_agent::context::mcp_servers_prompt_section`]).
    ///
    /// `run_print` renders it once after `session_start` from the MCP
    /// manager's synchronous status snapshot; [`Self::sync`] re-reads it per
    /// prompt through [`Self::set_mcp_servers`], so a `pending` -> `ready`
    /// transition lands on the next prompt.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_mcp_servers(
        preamble_override: Option<String>,
        append: Vec<String>,
        project_files: Vec<(PathBuf, String)>,
        skills: Vec<crate::skills::SkillInfo>,
        cwd: PathBuf,
        package_dir: Option<PathBuf>,
        mcp_servers: Option<String>,
    ) -> Self {
        Self {
            preamble_override,
            append,
            project_files,
            skills,
            cwd,
            package_dir,
            mcp_servers,
            rendered: None,
        }
    }

    /// Refresh the `mcp_servers` section from the manager's current
    /// synchronous snapshot (upstream re-renders it on every
    /// `before_agent_start`). Cheap and IO-free; a change re-renders on the
    /// next [`Self::sync`], an unchanged section keeps the cached bytes.
    pub fn set_mcp_servers(&mut self, mcp_servers: Option<String>) {
        self.mcp_servers = mcp_servers;
    }

    /// The system prompt for one prompt run.
    ///
    /// `active` is the loadout's resolved selection (the active-set cell's
    /// seed); `cell` is that cell, exactly as [`print_request_definitions`]
    /// receives it — `None` or the seed means "the first build". The first
    /// call is the invocation's first prompt, and its bytes are the pre-cache
    /// build's: same discovery inputs, and the same declared set the loadout
    /// declared (`declared == active` for a resolved selection, with the same
    /// order).
    pub fn sync(
        &mut self,
        registry: &ToolRegistry,
        active: &[String],
        cell: Option<&[String]>,
    ) -> String {
        let effective: &[String] = match cell {
            Some(names) if names != active => names,
            _ => active,
        };
        if let Some(rendered) = &self.rendered {
            if rendered.active == effective && rendered.mcp_servers == self.mcp_servers {
                // Steady state: the registry and the hooks are fixed for the
                // invocation, so the same active set and the same MCP section
                // cannot render differently.
                return rendered.prompt.clone();
            }
        }
        let (declared, hidden) = print_prompt_state(registry, effective);
        // The cache key is the DECLARED surface (declared names + hidden
        // declarations): an active-set change that only adds a name the
        // registry does not declare leaves the prompt identical, so reuse it.
        if self.rendered.as_ref().is_some_and(|rendered| {
            rendered.declared == declared
                && rendered.hidden == hidden
                && rendered.mcp_servers == self.mcp_servers
        }) {
            if let Some(rendered) = self.rendered.as_mut() {
                rendered.active = effective.to_vec();
                return rendered.prompt.clone();
            }
        }
        let tools: Vec<crate::system_prompt::ToolInfo> = declared
            .iter()
            .filter_map(|name| registry.get(name))
            .map(|tool| crate::system_prompt::ToolInfo::from_agent_tool(tool.as_ref()))
            .collect();
        let prompt = build_system_prompt(&crate::system_prompt::SystemPromptInput {
            preamble_override: self.preamble_override.clone(),
            append: self.append.clone(),
            tools: &tools,
            hidden_tools: &hidden,
            project_files: &self.project_files,
            skills: &self.skills,
            cwd: &self.cwd,
            package_dir: self.package_dir.as_deref(),
            custom_sections: crate::coding_agent::context::mcp_custom_sections(
                self.mcp_servers.as_deref(),
            ),
        });
        self.rendered = Some(PrintedPrompt {
            active: effective.to_vec(),
            declared,
            hidden,
            mcp_servers: self.mcp_servers.clone(),
            prompt: prompt.clone(),
        });
        prompt
    }
}

/// The print host's tool surface for one invocation: the executable registry,
/// the active (declared) names, and the registered names the model is not told
/// about.
///
/// The registry stays the FULL registered set — the selection is the
/// ACTIVE-set override, never a registry filter, exactly as the session host
/// applies it ([`crate::agent_session::AgentSession::active_tool_snapshot`] /
/// `resolve_active_override`): every registered tool remains reachable through
/// `ctx.executeTool`, `tool_search` and nested calls, and only the declared
/// set narrows to the selected names.
///
/// This is the parity seam between the two hosts. Print used to hard-filter
/// the registry to the selection, which dropped a tool the selection never
/// named — with `--exclude-tools bash` alone the shared resolver's base is the
/// CORE registry, so every plugin tool the print host merges (`ctx_*`,
/// `python`, `todo`, ...) fell out of the registry and became un-callable,
/// while the same selection left it callable in a session.
///
/// `hidden` exposure is the one exception, unchanged: those tools are
/// registered but unreachable (upstream `hidden`), so they never join the
/// executable registry.
///
/// Public for tests/.
pub struct PrintToolLoadout {
    /// The executable registry: every registered tool with `hidden` exposure
    /// removed. The selection never removes a tool from it.
    pub registry: ToolRegistry,
    /// The active names (the override); declared after exposure filtering.
    pub active: Vec<String>,
    /// Names declared to the model, in active order (the declared set BEFORE
    /// the `prepareLoadout` hooks). A hook-hidden name
    /// ([`Self::hidden_declarations`]) stays in this list — it is active and
    /// callable — but [`Self::definitions`] omits its declaration and the
    /// prompt's tool list leaves it out via [`Self::hidden`].
    pub declared: Vec<String>,
    /// Names tools may call through `ctx.executeTool()` (upstream
    /// `_getCallableTools`): active `direct` tools plus every registered
    /// `codemode`/`deferred` tool. `model-only` tools are never callable.
    pub callable: std::collections::BTreeSet<String>,
    /// Registered names the declared set leaves out — the system prompt's
    /// `hiddenTools` input.
    pub hidden: Vec<String>,
    /// Active tools whose declaration a `prepareLoadout` hook hides from
    /// requests (upstream `hiddenDeclarations`): they stay active and callable
    /// and the prompt's tool list leaves them out, but the registry keeps
    /// them.
    pub hidden_declarations: std::collections::BTreeSet<String>,
    /// Model-facing description rewrites a `prepareLoadout` hook returned, by
    /// tool name (upstream `descriptions`).
    pub descriptions: std::collections::BTreeMap<String, String>,
}

/// Resolve the tool selection for a print run and apply it to `registry`
/// (see [`PrintToolLoadout`]). The settings `defaultTools` list arrives
/// already resolved by the caller, exactly as the CLI flags do.
///
/// Errors are the shared resolver's validation failures (`--tools` mixing
/// plain names and `+`/`-` modifiers, patterns in a modifier list).
pub fn print_tool_loadout(
    mut registry: ToolRegistry,
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude_tools: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
) -> Result<PrintToolLoadout, String> {
    // Plain names and `*` glob patterns both match here (upstream
    // settings-manager validation; `?` is a literal character, matching is
    // case-sensitive — `toolPatternRegExp`, mcp-servers.ts:12-24). The
    // selection is the DECLARED set: an MCP tool no entry names or matches
    // stays registered but undeclared (upstream `_isActivatable`,
    // agent-session.ts:1548-1558).
    //
    // The registry is threaded in as registration FACTS, not just names: the
    // exclude-only and `--no-builtin-tools` bases are upstream
    // `_isActivatedOnRegistration` rules (`direct`/`model-only` exposure and
    // `defaultActive !== false`, agent-session.ts:3608-3613, 3632-3634), so a
    // denylist keeps the default-active plugin tools instead of
    // un-declaring them (D4) and `--no-builtin-tools` keeps only the
    // registration-active ones (D2).
    let available_tools =
        crate::coding_agent::tool_selection::AvailableTool::from_registry(&registry);
    let selection = crate::coding_agent::tool_selection::resolve_tool_selection_with_availability(
        settings_default_tools,
        cli_tools,
        cli_exclude_tools,
        no_tools,
        no_builtin_tools,
        &available_tools,
        // Base for the settings/modifier paths stays the upstream
        // `DEFAULT_TOOL_NAMES`; the exclude-only and `--no-builtin-tools`
        // paths derive their base from the registry's registration facts.
        &crate::config::settings::DEFAULT_TOOL_NAMES,
    )?;

    // Exposure model (upstream tool-exposure semantics): `hidden` tools are
    // registered but unreachable — dropped entirely; `codemode`/`deferred`
    // tools stay callable but are NOT declared to the model.
    registry.remove_exposure(crate::agent::tool_execution::ToolExposure::Hidden);
    // The selection names ARE the active override (upstream
    // `initialActiveToolNames` / `setActiveTools`); `active_names` keeps the
    // registered, non-hidden ones in selection order.
    let active = match selection.as_ref() {
        Some(resolved) => registry.active_names(Some(&resolved.enabled)),
        None => registry.active_names(None),
    };
    let declared: Vec<String> = crate::agent::tool_execution::declared_tools(&registry, &active)
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    // Callability is the registry-wide view, NOT the selected names: the
    // session host hands the loop exactly this set
    // (`build_tool_loadout` → `LoopDeps::nested_callable`), so print must too.
    let callable: std::collections::BTreeSet<String> =
        crate::agent::tool_execution::callable_tools(&registry, &active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();

    // Loadout hooks (upstream `_applyToolLoadout`, agent-session.ts:1580-1625):
    // every declared tool's `prepareLoadout` may rewrite descriptions and hide
    // declarations from requests while the tools stay active and callable.
    // The session host runs the same fan-out in
    // `AgentSession::build_tool_loadout`; without it here, codemode's
    // `mode: "only"` hidden declarations (the whole point of the mode) never
    // reached a print request and the direct tools stayed declared.
    //
    // Only the Rust hooks run: this seam is synchronous while JS hooks need
    // the async runtime round trip. The two in-tree hook users (`codemode`,
    // the shell warm-ups) are Rust, so the declaration set is complete; a JS
    // `prepareLoadout` remains unrouted in print (audit W16-D7).
    let (descriptions, hidden_declarations) = loadout_hook_changes(&registry, &active);
    // The hidden declarations stay active and callable, but the prompt's tool
    // list/rules and the request declarations leave them out (upstream
    // `hiddenTools: [..._hiddenDeclarations]`, agent-session.ts:1727).
    let declared_set: std::collections::BTreeSet<&str> =
        declared.iter().map(String::as_str).collect();
    let mut hidden: Vec<String> = registry
        .iter()
        .map(|tool| tool.name().to_string())
        .filter(|name| !declared_set.contains(name.as_str()))
        .collect();
    let not_already_hidden: Vec<String> = hidden_declarations
        .iter()
        .filter(|name| !hidden.iter().any(|hidden| hidden == *name))
        .cloned()
        .collect();
    hidden.extend(not_already_hidden);
    Ok(PrintToolLoadout {
        registry,
        active,
        declared,
        callable,
        hidden,
        hidden_declarations,
        descriptions,
    })
}

/// Output mode for one print run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Final assistant text only.
    Text,
    /// `--mode json`: header + JSON event lines on stdout.
    Json,
}

/// Finish a print run's diagnostics: when the collector was enabled
/// (`PI_TIMING=1`), print the summary block to stderr and — in `--mode json` —
/// emit the final `{"type":"run_diagnostics", ...}` frame on stdout after
/// every event line. A no-op (and no output) otherwise.
fn finish_run_diagnostics(
    collector: &crate::diagnostics::RunDiagnosticsCollector,
    mode: OutputMode,
) {
    let Some(summary) = collector.finish() else {
        return;
    };
    summary.print_to_stderr();
    if mode == OutputMode::Json {
        crate::coding_agent::json::print_run_diagnostics(&summary);
    }
}

/// The stderr warning for a final response cut off at the model's output token
/// limit (upstream print-mode.ts:161, verbatim).
pub const TRUNCATION_WARNING: &str =
    "Response was truncated: the model reached its output token limit.";

/// Text-mode terminal disposition of the final assistant message (upstream
/// print-mode.ts:155-166): a `length` stop reason still prints the partial
/// text, warns on stderr, and produces a nonzero exit code; every other
/// non-`error`/`aborted` reason exits 0.
pub fn truncation_notice(stop_reason: crate::ai::types::StopReason) -> Option<&'static str> {
    (stop_reason == crate::ai::types::StopReason::Length).then_some(TRUNCATION_WARNING)
}

/// What a print run produced: the text to write on stdout plus the process
/// exit code (upstream `runPrintMode` returns an exit code alongside the
/// output it already wrote).
#[derive(Debug, Clone)]
pub struct PrintOutcome {
    /// Final assistant text (text mode; empty in json mode), with the
    /// `backgroundToolResult` frames of every observed completion appended
    /// ([`text_with_background_reports`]).
    pub text: String,
    /// 1 when the final response was truncated at the model's output token
    /// limit, or when a background call the run started had to be aborted at
    /// exit — in text mode only, else 0.
    pub exit_code: i32,
}

/// Options for [`run_print`].
pub struct PrintOptions {
    /// The prompt to execute.
    pub prompt: String,
    /// Additional prompts sent in order after `prompt`, each in its own turn
    /// (upstream `runPrintMode`'s `messages`: the positional arguments left
    /// after the initial message).
    pub additional_messages: Vec<String>,
    /// Output mode.
    pub mode: OutputMode,
    /// Resolved session plan (see [`crate::coding_agent::session_resolve`]).
    pub session: SessionPlan,
    /// `--no-context-files`: skip AGENTS.md/CLAUDE.md discovery.
    pub no_context_files: bool,
    /// `--provider` + `--model` (upstream CLI selection takes priority).
    pub cli_provider: Option<String>,
    /// `--model <pattern[:thinking]>`.
    pub cli_model: Option<String>,
    /// `--thinking` (overrides the model-pattern suffix and settings).
    pub cli_thinking: Option<crate::ai::types::ThinkingLevel>,
    /// `--api-key` (overrides every other credential source for the run).
    pub api_key_override: Option<String>,
    /// `--models` patterns: `None` when the flag was absent (the settings
    /// `enabledModels` scope applies), `Some([])` for an explicit empty
    /// `--models ""` (upstream `parsed.models ?? getEnabledModels()`).
    pub model_patterns: Option<Vec<String>>,
    /// `--name`: the session display name.
    pub session_name: Option<String>,
    /// `--system-prompt`: replaces the default preamble.
    pub system_prompt_override: Option<String>,
    /// `--append-system-prompt` entries (text or `@file` contents).
    pub append_system_prompt: Vec<String>,
    /// The settings `defaultTools` selection (already resolved).
    pub settings_default_tools: Option<Vec<String>>,
    /// `--tools` entries (allowlist or pure modifiers).
    pub cli_tools: Vec<String>,
    /// `--exclude-tools` entries.
    pub cli_exclude_tools: Vec<String>,
    /// `--no-tools`.
    pub no_tools: bool,
    /// `--no-builtin-tools`.
    pub no_builtin_tools: bool,
    /// Whether this run continues/resumes an existing session (skips the
    /// "first scoped model" step of the resolution order).
    pub is_continuing: bool,
    /// `--skill` / `--prompt-template` CLI paths and the `--no-skills` /
    /// `--no-prompt-templates` disable flags.
    pub resources: crate::coding_agent::resources::ResourcePathOptions,
    /// Images attached to the FIRST prompt (upstream `PrintModeOptions
    /// .initialImages`, forwarded into `session.prompt(initialMessage,
    /// { images })` as image content blocks; text + images share the one user
    /// message). Later prompts are text-only, like upstream.
    pub images: Vec<crate::ai::types::ImageContent>,
}

/// Run a single prompt in print mode through the configured provider.
///
/// `extension_paths` carries the `-e/--extension` entries plus the default
/// discovery paths resolved by the CLI (see
/// [`crate::extensions::loader::default_extension_paths`]); an empty list
/// skips the extension runtime entirely.
pub async fn run_print(
    options: PrintOptions,
    config: &crate::config::Config,
    extension_paths: &[PathBuf],
) -> crate::Result<PrintOutcome> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd_text = cwd.to_string_lossy().into_owned();

    // Termination handlers (upstream print-mode.ts registerSignalHandlers):
    // the process exits with the conventional codes instead of dying mid-write
    // — SIGTERM→143, SIGHUP→129 and SIGINT→130 on Unix; on Windows the console
    // control events map onto the same codes (close/break→143 = SIGTERM
    // analog, Ctrl+C→130 = SIGINT abort analog). Unlike upstream, the handler
    // first aborts the in-flight run: the shell tools observe the cancel and
    // synchronously kill their process trees (so `pir -p` does not orphan a
    // foreground command the harness just stopped waiting for), and the
    // per-message persistence sink (see `persist_message` below) keeps
    // everything already streamed. `run_active` bounds the graceful window.
    let run_signal = crate::ai::types::AbortSignal::new();
    let run_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    install_signal_handlers(run_signal.clone(), Arc::clone(&run_active));

    // Run diagnostics (crate::diagnostics): the collector is fed from the
    // event handler built below (the layer that already consumes the loop's
    // stream), enabled by the existing `PI_TIMING=1` gate. It stays inert —
    // one bool check per event — otherwise.
    let run_diagnostics = Arc::new(crate::diagnostics::RunDiagnosticsCollector::new(
        crate::config::timings::timing_enabled(),
    ));

    // Model environment + settings (upstream `ModelRuntime` + SettingsManager).
    let environment = crate::ai::providers::load_pi_environment(Some(&cwd))?;
    let settings = &environment.settings;
    let has_auth =
        |provider: &str| crate::ai::providers::has_configured_auth(provider, &environment.api_keys);

    // Model scope (upstream main.ts:813): a present `--models` replaces the
    // settings `enabledModels` list — including an explicit empty value, which
    // yields an empty scope instead of falling back to the settings.
    // The initial model then follows upstream `findInitialModel`: CLI
    // --provider/--model first, then the first scoped model, then the
    // settings defaults (with auth), then the first available model with auth.
    let scoped_patterns = crate::coding_agent::overrides::effective_model_patterns(
        options.model_patterns.as_deref(),
        settings.enabled_models.as_deref(),
    );
    let (scoped_models, diagnostics) = crate::ai::providers::resolution::resolve_model_scope(
        &scoped_patterns,
        &environment.models,
    );
    for diagnostic in &diagnostics {
        eprintln!("Warning: {diagnostic}");
    }
    let empty_model_thinking_levels = std::collections::BTreeMap::new();
    use crate::ai::providers::resolution::{find_initial_model, InitialModelOptions};
    // Upstream main.ts:831-839: an inline key must be scoped to an explicitly
    // chosen model; silently applying it to the settings default could send
    // the credential to the wrong provider.
    if options.api_key_override.is_some()
        && options.cli_model.is_none()
        && options.cli_provider.is_none()
        && options.model_patterns.is_none()
    {
        return Err(anyhow::anyhow!(
            "--api-key requires a model to be specified via --model, --provider/--model, or --models"
        ));
    }
    let initial = find_initial_model(InitialModelOptions {
        cli_provider: options.cli_provider.as_deref(),
        cli_model: options.cli_model.as_deref(),
        cli_thinking: options.cli_thinking,
        scoped_models: &scoped_models,
        is_continuing: options.is_continuing,
        default_provider: settings.default_provider.as_deref(),
        default_model_id: settings.default_model.as_deref(),
        // `defaultThinkingLevel` never defaults to `off`.
        default_thinking_level: settings
            .default_thinking_level
            .filter(|level| *level != crate::ai::types::ThinkingLevel::Off),
        model_thinking_levels: settings
            .model_thinking_levels
            .as_ref()
            .unwrap_or(&empty_model_thinking_levels),
        models: &environment.models,
        has_auth: &has_auth,
        // An inline `--api-key` goes to whichever provider the model selects,
        // so it must not break ties by stored credentials.
        inline_api_key: options.api_key_override.is_some(),
    })
    .map_err(anyhow::Error::msg)?;
    // Chat-only, last-applied per-model overrides from models.json are
    // already merged into the environment model list.
    let Some(model) = initial.model else {
        let message = "no model available; configure a provider with `pir auth login` or ~/.pi/agent/models.json";
        if options.mode == OutputMode::Json {
            // Upstream json mode reports failures in-band and exits 0
            // (print-mode.ts maps only TEXT-mode terminal errors onto the
            // exit code): emit the error as a terminal message event.
            crate::coding_agent::json::print_event(
                &crate::agent::agent_loop::AgentEvent::TurnEnd {
                    turn: 0,
                    message: terminal_error_message(message),
                    // Synthetic terminal turn: no tools ran.
                    tool_results: Vec::new(),
                },
            );
            return Ok(PrintOutcome {
                text: String::new(),
                exit_code: 0,
            });
        }
        return Err(anyhow::anyhow!(message));
    };
    // Starting thinking level, upstream `buildSessionOptions` (main.ts:465-523)
    // → `createAgentSession` (sdk.ts:250-278): an explicit CLI level
    // (`--thinking`, else the `--model` pattern's `:level` suffix — parsed by
    // the same resolver `find_initial_model` uses, so the grammar stays
    // single-sourced), else the first scoped model's `:level` suffix, then the
    // per-model `modelThinkingLevels` override, then `defaultThinkingLevel`
    // (`off` treated as absent), then the shared `DEFAULT_THINKING_LEVEL`.
    // `find_initial_model` folds most of this chain into its
    // `initial.thinking_level`, but its CLI-args and first-available fallbacks
    // hard-code the shared default instead of consulting settings — resolving
    // through the shared helper keeps exactly one policy, the one the
    // interactive host applies.
    let thinking_level = {
        let explicit = match options.cli_model.as_deref() {
            Some(pattern) => {
                // Same resolver call as `find_initial_model`: `--model`
                // without `--provider` still contributes its `:level` suffix,
                // and an explicit `--thinking` keeps a custom model id's
                // suffix intact.
                let (_, pattern_thinking, _) =
                    crate::ai::providers::resolution::resolve_cli_model_with_auth(
                        options.cli_provider.as_deref(),
                        pattern,
                        &environment.models,
                        &has_auth,
                        options.api_key_override.is_some(),
                        options.cli_thinking,
                    )
                    .map_err(anyhow::Error::msg)?;
                options.cli_thinking.or(pattern_thinking)
            }
            // No CLI model: the first scoped model's suffix is the explicit
            // source (skipped when continuing/resuming, like the model choice).
            None if !scoped_models.is_empty() && !options.is_continuing => {
                scoped_models[0].thinking_level
            }
            None => None,
        };
        let per_model = settings.model_thinking_levels.as_ref().and_then(|levels| {
            levels
                .get(&format!("{}/{}", model.provider, model.id))
                .copied()
        });
        crate::agent_session::resolve_starting_thinking_level(
            explicit,
            per_model,
            settings.default_thinking_level,
        )
    };
    if let Some(message) = &initial.fallback_message {
        eprintln!("pir: {message}");
    }
    let api_key = options
        .api_key_override
        .clone()
        .or_else(|| environment.api_keys.get(&model.provider).cloned());

    // Session storage per the resolved plan (JSONL only). The manager lives
    // behind a shared handle: per-message persistence (LoopOptions
    // `persist_message`) and the compaction hook both append to it DURING the
    // run, so a mid-run crash or termination signal loses nothing already
    // streamed (upstream persists per `message_end` too,
    // agent-session.ts:1144-1163).
    let mut session = open_session_manager(
        &options.session,
        &cwd_text,
        std::path::Path::new(&config.session_dir),
    )?;
    // Upstream `assertSessionCwdExists` (session-cwd.ts:54-59): the text print
    // host is headless, so a stored session cwd that no longer exists is a hard
    // error (the interactive host may instead offer to continue in the current
    // cwd).
    if let Some(session) = &session {
        if let Some(issue) = session.missing_session_cwd() {
            return Err(anyhow::anyhow!(issue.error_message(&cwd_text)));
        }
    }
    if let (Some(session), Some(name)) = (session.as_mut(), options.session_name.as_deref()) {
        session.set_session_name(Some(name))?;
    }
    let session: SharedSession = Arc::new(tokio::sync::Mutex::new(session));

    // Extensions first so `session_start`-registered tools appear in the
    // prompt's tools section. The print host resolves the model environment
    // before loading, so push that catalog onto the runtime pre-load: a
    // factory that resolves a model synchronously during load must see it.
    // Only file-backed (JS) extensions can run such a factory; a builtin-only
    // plan never starts QuickJS, so building the 1,700-model catalog there is
    // pure startup cost (~50-90 ms at debug opt-level 0, measured in
    // logs/lane-cold-start/REPORT.md).
    let (_, file_extension_paths) = crate::plugins::partition_paths(extension_paths);
    let model_catalog = (!file_extension_paths.is_empty())
        .then(|| crate::agent_session::ModelCatalog::from_registry(&environment.registry));
    let extensions = crate::agent_session::load_extension_runtime_with_catalog(
        extension_paths,
        ExtensionConfig {
            cwd: cwd.clone(),
            // JSON mode must not let extension stdout inject raw bytes into
            // the JSONL frame stream; reuse the protocol-safe routing.
            mode: if options.mode == OutputMode::Json {
                "json".to_string()
            } else {
                "print".to_string()
            },
            has_ui: false,
            model: Some(model.id.clone()),
            session_id: None,
            system_prompt: String::new(),
            is_idle: true,
            limits: None,
        },
        model_catalog.as_ref(),
    )
    .await;

    let mut registry = ToolRegistry::new();
    crate::coding_agent::tools::register_core_tools(&mut registry);
    let mut prompt_text = options.prompt.clone();
    // The unified plugin host is built BEFORE `session_start`: the event is
    // dispatched through the host's ordered list, so its native plugins
    // observe the same startup event the JS face does (the JS adapter is
    // dispatched once, at its list position). It sources the MCP reach wait
    // hook below and owns the builtin plugin lifecycle for the run.
    let plugins = print_plugin_host(extensions.as_ref());
    if let Some(runtime) = &extensions {
        // Mirror the AgentSession extension lifecycle for this host: one
        // `session_start` before the first prompt, then the input transform,
        // then the tool merge (session_start handlers may register tools).
        if let Some(host) = plugins.as_deref() {
            dispatch_print_session_start(host, "startup").await;
        } else {
            let mut payload = serde_json::json!({ "type": "session_start", "reason": "startup" });
            if let Err(error) = runtime.emit("session_start", &mut payload).await {
                eprintln!("[ext] session_start dispatch failed: {error}");
            }
        }
        crate::agent_session::drain_extension_notifications(runtime);
        prompt_text = transform_print_input(plugins.as_deref(), Some(runtime), &prompt_text).await;
        crate::agent::tool_execution::merge_extension_tools(&mut registry, runtime);
    }
    // The unified plugin host's OWN tools join the same registry: the Rust
    // builtins (`context-mode`'s nine `ctx_*` tools, `todo`, `python`,
    // `subagent`, `tool_search`, `codemode`, the MCP bridges) and the JS
    // adapter's exports, through the one merge the session host applies in
    // `AgentSession::merged_tool_registry` (`agent_session/tools.rs`).
    //
    // Without this the builtins register into the host's registry and never
    // reach a request body: print drives `agent_loop` itself instead of an
    // `AgentSession`, so no later step merges them. Measured on the first
    // request of a default full build (`scripts/perf/body_probe.py`):
    // `tools=9` — exactly the core set minus `powershell` (registered but
    // `default_active: false`) — with no `ctx_execute`/`ctx_search`/... in the
    // body even though `builtin:context-mode` is on the plan. "Think in code"
    // cannot work if the model never sees the tools.
    //
    // Precedence is the session merge's (first name wins, CORE first, then the
    // plugin list in order); the JS tools merged just above are skipped by name
    // here, so nothing is registered twice. The `setActiveTools` override
    // prunes exactly like the session host's. Driven by the HOST, not by the
    // runtime: a builtin-only plan (no JS engine, plan-only handle) still
    // registers and must still merge.
    if let Some(host) = plugins.as_deref() {
        let active_override = host.active_tools_override();
        crate::agent::tool_execution::merge_plugin_tools(
            &mut registry,
            host,
            active_override.as_deref(),
        );
    }
    // Native half of `project_trust`: the runtime loader already consulted the
    // JS face at load time (before this host existed); dispatch the event to
    // the native plugin list once. `emit_native` skips the JS adapter, so the
    // JS face is never dispatched twice.
    if let Some(host) = plugins.as_deref() {
        let cwd_display = cwd.display().to_string();
        crate::agent_session::dispatch_project_trust(None, Some(host), &cwd_display).await;
    }

    // Skills + prompt templates for this run (CLI paths, settings paths,
    // default dirs; project dirs trust-gated). `/skill:name` and `/template`
    // commands expand into the prompt (upstream `prompt()`). The startup gate
    // already resolved and pinned this run's trust, so the print host sees the
    // same extension grant/denial and `--approve`/`--no-approve` decision as
    // the run gate (`resolve_project_trust` falls back to the store/policy
    // when there is no pinned/extension/override decision).
    let trusted = crate::extensions::project_trust::resolve_project_trust(
        &cwd,
        &crate::coding_agent::context::agent_dir(),
    );
    let resources = crate::coding_agent::resources::DiscoveredResources::discover(
        &crate::coding_agent::resources::ResourceDiscoveryOptions {
            cwd: &cwd,
            agent_dir: &crate::coding_agent::context::agent_dir(),
            project_trusted: trusted,
            cli_skill_paths: &options.resources.skill_paths,
            cli_prompt_paths: &options.resources.prompt_paths,
            no_skills: options.resources.no_skills,
            no_prompt_templates: options.resources.no_prompt_templates,
        },
    );
    // Resource-load diagnostics (collisions, malformed skills/templates):
    // upstream startup-diagnostic style on stderr, once per run; never fatal.
    for diagnostic in &resources.diagnostics {
        eprintln!("pir: {diagnostic}");
    }
    prompt_text = crate::coding_agent::resources::expand_prompt_commands(
        &prompt_text,
        &resources.skills,
        &resources.prompt_templates,
    );

    // Apply the resolved tool selection (`--tools` / `--exclude-tools` /
    // `--no-tools` / `--no-builtin-tools` / `settings.defaultTools`) through
    // the shared session rule ([`print_tool_loadout`]): the selection is the
    // ACTIVE override, never a registry cap. The declared set is therefore
    // exactly the selected registered names (the same set a session declares
    // for the same selection), while every other registered tool stays in the
    // registry for `ctx.executeTool`, `tool_search` and nested calls.
    let loadout_started = crate::config::timings::start_phase();
    let loadout = match print_tool_loadout(
        registry,
        options.settings_default_tools.as_deref(),
        &options.cli_tools,
        &options.cli_exclude_tools,
        options.no_tools,
        options.no_builtin_tools,
    ) {
        Ok(loadout) => loadout,
        Err(error) => return Err(anyhow::anyhow!(error)),
    };
    // The provider request declares only the active direct/model-only tools;
    // `codemode`/`deferred` tools stay in the (executable) registry. The
    // declared definitions carry the `prepareLoadout` results (upstream
    // `_applyToolLoadout` + `_installHiddenDeclarationsProjection`): a
    // hook-hidden name stays active/callable but its declaration is omitted —
    // codemode mode `"only"` hides the direct tools this way — and a
    // description rewrite replaces the tool's own text. The shared session
    // builder (`declared_tool_definitions`) is what makes the print model's
    // surface the interactive one, including the scheduler-owned `background`
    // argument ([`print_tool_definitions`]); the bare core builder it replaced
    // declared none.
    let tools = loadout.definitions();
    let PrintToolLoadout {
        registry,
        active,
        declared: _,
        callable,
        ..
    } = loadout;
    // The run's resolved active set, kept for the per-prompt rebuild below and
    // seeded into the tool env's override cell (see [`ToolSessionEnv`]): the
    // search builtin's baseline is the RUN's selection, not only the exposure
    // defaults, so a search never un-declares `tool_search` itself. Print
    // resolves the selection once, so the cell starts at the selection and
    // grows by `tool_search`'s additive union.
    let resolved_active: Arc<std::sync::RwLock<Option<Vec<String>>>> =
        Arc::new(std::sync::RwLock::new(Some(active.clone())));
    // One shared nested-callable handle for every prompt run of this
    // invocation (`LoopDeps::nested_callable`).
    let nested_callable = Arc::new(callable);
    // Print's loadout preparation: registry filters, active/declared
    // classification, and the Rust `prepareLoadout` hook fan-out (description
    // rewrites + hidden declarations, applied above via
    // [`PrintToolLoadout::definitions`]). JS hooks need the async runtime
    // round trip and are not routed here — logged for the same phase name the
    // session host uses.
    crate::config::timings::log_phase_once_opt(
        "loadout.first_build",
        "loadout.first_build",
        crate::config::timings::finish_phase(loadout_started),
    );

    // Context files (AGENTS.md / CLAUDE.md), upstream `loadProjectContextFiles`,
    // injected as the ordered builder's `project_context` section.
    let context_files = if options.no_context_files {
        Vec::new()
    } else {
        crate::coding_agent::context::resolve_context_files(&cwd)
    };
    let system_prompt_file = std::env::var_os("PIR_SYSTEM_PROMPT_FILE")
        .map(std::path::PathBuf::from)
        .map(std::fs::read_to_string)
        .transpose()?;
    let discovered = crate::config::settings::discover_system_md(
        &cwd,
        &crate::coding_agent::context::agent_dir(),
        // Same run-wide decision as the resource discovery above.
        trusted,
    );
    // `--system-prompt` wins, then the env/file discovery chain. The flag
    // value follows upstream `resolvePromptInput` (resource-loader.ts:167-182):
    // the CLI value becomes the resource loader's `systemPromptSource`
    // (main.ts:792 → resource-loader.ts:399), and resolvePromptInput reads it
    // as a FILE whenever `existsSync(input)` holds — so a value naming an
    // existing path is read (BOM stripped) instead of used literally. The
    // judgment basis: upstream's `--system-prompt <text>` help text promises
    // plain text, but the loader's path-exists branch is unconditional for
    // both `--system-prompt` and `--append-system-prompt` (which pir already
    // resolves text-or-file). A read failure warns and falls back to the
    // literal value, like upstream.
    let preamble_override = match options.system_prompt_override.as_deref() {
        Some(value) => Some(resolve_prompt_input(value)),
        None => system_prompt_file.or(discovered.0),
    };
    // `--append-system-prompt` entries (text or file contents) come after the
    // discovered APPEND_SYSTEM.md.
    let mut append = discovered.1.into_iter().collect::<Vec<_>>();
    for entry in &options.append_system_prompt {
        append.push(resolve_text_or_file(entry)?);
    }
    // The package asset dir is resolved once here (upstream `getPackageDir`);
    // the builder itself never reads the process environment.
    let package_dir = crate::config::env::package_dir();
    // The prompt's `selectedTools` are the ACTIVE names (upstream
    // `_rebuildSystemPrompt` → `selectedTools: validToolNames`,
    // agent-session.ts:1706-1728), not the whole registry: the skills section's
    // reader hint is resolved against this list, so a registry-wide one would
    // name a reader the selection left inactive. Before the selection stopped
    // filtering the registry, this list WAS the registry; the loadout's
    // `hiddenTools` input still omits every non-declared name, so the rendered
    // tools/rules sections stay exactly the declared set.
    //
    // The discovery inputs are captured ONCE (upstream's resource loader loads
    // them once too): [`PrintPromptCache`] renders the first build here and
    // later re-renders from these inputs only when the DECLARED set changes
    // between prompts (a `tool_search` load / MCP activation /
    // `setActiveTools`), never re-reading context files or skills. The first
    // prompt is therefore byte-identical to the previous one-shot build.
    // The `mcp_servers` section: upstream re-renders it on every
    // `before_agent_start` (`extensions/mcp/index.ts:1176-1184`) from the
    // manager's state; pir renders it here (after `session_start`, so the
    // startup discovery's outcome is visible) and refreshes it per prompt
    // through [`PrintPromptCache::set_mcp_servers`] — cheap, synchronous reads
    // of the status mirror, never a connect. `None` when MCP is off or has no
    // enabled server, which keeps the prompt byte-identical to before.
    let mcp_servers = crate::coding_agent::context::mcp_servers_prompt_section(plugins.as_deref());
    let mut prompt_cache = PrintPromptCache::new_with_mcp_servers(
        preamble_override,
        append,
        context_files,
        resources.skills,
        cwd.clone(),
        package_dir,
        mcp_servers,
    );
    let prompt_build_started = crate::config::timings::start_phase();
    prompt_cache.sync(&registry, &active, None);
    crate::config::timings::log_phase_once_opt(
        "prompt.system_prompt_build",
        "prompt.system_prompt_build",
        crate::config::timings::finish_phase(prompt_build_started),
    );

    // The print host's background-call surface (`TOOL-BACKGROUND-SCOPE.md`):
    // one registry + delivery for the whole invocation, installed on every
    // prompt run's deps below. Built here, after the shared session and the
    // extension handle exist (both are delivery inputs), and before the first
    // run can detach anything.
    let background = PrintBackground::new(Arc::clone(&session), extensions.clone());

    // JSON mode: session header first, then one line per event. An ephemeral
    // run (`--no-session`) still gets a header line: upstream's in-memory
    // session manager carries one (main.ts `createSessionManager` →
    // `SessionManager.inMemory`), so synthesize the same
    // `{type:"session",version,id,timestamp,cwd}` shape.
    if options.mode == OutputMode::Json {
        let guard = session.lock().await;
        if let Some(session) = guard.as_ref() {
            if let Some(header) = session.header() {
                crate::coding_agent::json::print_session_header(header);
            } else {
                print_ephemeral_header(&cwd_text);
            }
        } else {
            print_ephemeral_header(&cwd_text);
        }
        // The background completion stream rides the same stdout frame stream
        // (`background_tool_end`, the RPC host's wire shape), emitted as each
        // call settles.
        background.stream_events_as_json();
    }
    // The run's event handler: JSON mode streams every event to stdout, and
    // both modes feed the diagnostics collector on the way (a no-op while
    // disabled). One handler serves every prompt of this invocation, so the
    // summary covers the whole print run.
    let events: crate::agent_session::EventHandler = match options.mode {
        OutputMode::Json => {
            let diagnostics = Arc::clone(&run_diagnostics);
            Arc::new(move |event| {
                diagnostics.record(&event);
                crate::coding_agent::json::print_event(&event);
            })
        }
        OutputMode::Text => {
            let diagnostics = Arc::clone(&run_diagnostics);
            Arc::new(move |event| diagnostics.record(&event))
        }
    };

    // One prompt per entry (upstream `runPrintMode`): the initial message
    // first, then each remaining positional message in its own turn.
    let mut prompts: Vec<String> = vec![prompt_text];
    prompts.extend(options.additional_messages.iter().cloned());

    let registry = Arc::new(registry);
    let provider_registry = Arc::new(environment.registry);
    // Remote catalog overlays (upstream model-runtime wraps every built-in
    // provider with `withRemoteCatalog`): background ETag revalidation,
    // delayed first hop, PI_OFFLINE-gated.
    crate::ai::providers::install_remote_catalog_overlays(&provider_registry);

    // The unified plugin host (built above, before `session_start`) plus the
    // MCP reach wait hook it sources (`LoopDeps::tool_call_wait`, D23 (b)):
    // the print host drives `agent_loop` itself instead of AgentSession, so
    // it builds the same host the session builds and hands the loop the same
    // hook — a tool batch waits for the MCP servers its calls may reach
    // before executing, like the interactive path
    // (agent_session/mcp_reach.rs). Its tools were merged into `registry`
    // above (`merge_plugin_tools`, the session host's merge), so the run's
    // declared set carries them.
    // The live session-context push (the `tool-repair` builtin's
    // `before_provider_request` model gate reads the cached id — the print
    // twin of session `push_session_context`, agent_session/session_ctx.rs:52-62).
    // Print resolves ONE model for the whole run, so a single push before the
    // first request covers every prompt turn and every request kind
    // (conversational, compaction, navigate). Synchronous, so no request can
    // interleave before the gate has its id.
    if let Some(plugins) = &plugins {
        plugins.update_session_context(print_model_snapshot(&model));
    }

    // Per-request transport hooks shared by EVERY provider request this host
    // sends (loop stream, event stream, compaction/navigate summaries) —
    // routed through the plugin host so the Rust builtins participate the way
    // the session's streamFn seam installs them (agent_session/stream.rs:112-164);
    // see [`print_transport_hooks`].
    let mut transport_hooks =
        print_transport_hooks_with_model(plugins.as_ref(), extensions.as_ref(), Some(&model));
    // Transport retry lifecycle → the JSON event stream (upstream
    // print-mode.ts:108-112 `session.subscribe` → `toJsonEvent`:
    // `auto_retry_start` / `auto_retry_end` are ordinary session events
    // there). The print host drives the agent loop without an `AgentSession`,
    // so the retry observers are installed directly on the shared per-request
    // hooks; text mode stays silent like upstream's text print mode.
    // Compaction/navigate summaries share these hooks, so a TRANSPORT-level
    // retry of a summarizer request surfaces as `auto_retry_*` here too. The
    // summarizer's own single retry (`summarization_retry_*`) is a separate
    // budget, installed as the compaction module's task-local observer around
    // each summarizer run: the navigate summary emits it through
    // `generate_navigation_summary_with_retry_events` (source `branchSummary`,
    // see `try_extension_command_with_events`) and the threshold compaction
    // hook through `with_threshold_compaction_retry_events` (source
    // `compaction`, reason `threshold`).
    if options.mode == OutputMode::Json {
        transport_hooks.on_retry_start =
            Some(Arc::new(|start: &crate::ai::types::AutoRetryStart| {
                crate::coding_agent::json::print_session_event(
                    &crate::agent_session::SessionEvent::AutoRetryStart {
                        attempt: start.attempt,
                        max_attempts: start.max_attempts,
                        delay_ms: start.delay_ms,
                        error_message: start.error_message.clone(),
                    },
                );
            }));
        transport_hooks.on_retry_end = Some(Arc::new(|end: &crate::ai::types::AutoRetryEnd| {
            crate::coding_agent::json::print_session_event(
                &crate::agent_session::SessionEvent::AutoRetryEnd {
                    success: end.success,
                    attempt: end.attempt,
                    final_error: end.final_error.clone(),
                },
            );
        }));
    }

    // The print host's out-of-loop session-event channel (JSON mode only,
    // upstream text print mode is silent): shared by the agent-level retry
    // sink, the navigate-summary retry observer, and the threshold compaction
    // lifecycle below. Building it once keeps every frame on the same writer
    // ([`crate::coding_agent::json::print_session_event`]).
    let print_session_events: Option<PrintSessionEventSink> =
        (options.mode == OutputMode::Json).then(json_session_event_sink);

    // Live `tool_execution_update` channel (upstream `emitToolExecutionUpdate`,
    // agent-loop.ts:779-788): the print host runs its own loop and builds no
    // `ToolSessionEnv`, so without this seam the tool partial-result stream
    // (the shell tools' throttled rolling snapshots) has no transport at all
    // and an extension `tool_execution_update` handler never fires in print /
    // `--mode json`. The channel is the session's own implementation
    // (`agent_session::extension_tool_update_channel`: same payload
    // projection — `{toolCallId, toolName, args, partialResult,
    // parentToolCallId?}` — and the same bounded fire-and-forget dispatch),
    // handed to the loop through `LoopDeps::tool_updates`. JSON mode also
    // writes each frame to stdout: upstream's print sink subscribes to every
    // session event, so SDK consumers see the partials even without any
    // extension installed.
    let extension_updates =
        crate::agent_session::extension_tool_update_channel(extensions.as_ref());
    let tool_updates: Option<crate::agent::tool_execution::ToolUpdateFn> =
        if options.mode == OutputMode::Json {
            Some(std::sync::Arc::new(move |partial: serde_json::Value| {
                if let Some(dispatch) = extension_updates.as_ref() {
                    // Two consumers: the JSON line and the extension channel
                    // both need an owned value, so one copy is unavoidable.
                    crate::coding_agent::json::print_tool_update(
                        &crate::agent::tool_execution::tool_update_payload(&partial),
                    );
                    dispatch(partial);
                } else {
                    // JSON line is the only consumer: move the snapshot into
                    // the wire payload instead of deep-copying it.
                    crate::coding_agent::json::print_tool_update(
                        &crate::agent::tool_execution::tool_update_payload_owned(partial),
                    );
                }
            }))
        } else {
            extension_updates
        };

    let tool_call_wait = print_tool_call_wait(plugins.as_ref());
    // The `tool-repair` builtin's assistant `message_end` repair hook, sourced
    // from the same host (agent_session/tool_repair.rs `host_tool_repair_hook`,
    // the session's `AgentSession::tool_repair_hook` twin): print drives the
    // loop itself, so the repair must ride these deps the same way it rides
    // the session's.
    let message_end_repair = crate::agent_session::host_tool_repair_hook(plugins.as_ref());

    // Navigate summarizer for the extension command-context `navigateTree`
    // action with `summarize: true` (upstream print-mode.ts:85-93 binds
    // `navigateTree` to `session.navigateTree`, which summarizes the
    // abandoned branch with the session's model): the routed model and its
    // completion function, same shape as the compaction hook's summarizer.
    let navigate_summary = crate::agent_session::NavigateSummaryBinding {
        model: model.clone(),
        complete: {
            let complete_model = model.clone();
            let complete_registry = Arc::clone(&provider_registry);
            let complete_options = crate::ai::types::StreamOptions {
                api_key: api_key.clone(),
                reasoning: Some(thinking_level),
                hooks: transport_hooks.clone(),
                ..Default::default()
            };
            let complete: Arc<compaction::CompleteFn> = Arc::new(Box::new(move |context| {
                let registry = Arc::clone(&complete_registry);
                let model = complete_model.clone();
                let options = complete_options.clone();
                Box::pin(async move {
                    let stream = registry.stream_resolving_auth(model, context, options);
                    // Completion consumer: drain while waiting (bounded queue;
                    // see `drain_and_result`).
                    match stream.drain_and_result().await {
                        Some(message) => Ok((*message).clone()),
                        None => Err("summarization stream did not terminate".to_owned()),
                    }
                })
            }));
            complete
        },
        reserve_tokens: compaction::branch_summarization::DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS,
    };
    let session_dir_path = std::path::Path::new(&config.session_dir).to_path_buf();

    // Automatic compaction (upstream print runs through AgentSession's
    // threshold compaction; pir's raw loop previously left `compaction: None`,
    // so a long prompt could exceed the context window). The config resolves
    // the settings `compaction` block with the per-model override, and the
    // threshold is checked against the ROUTED model under a virtual selection
    // (agent_session `threshold_model_for`).
    let threshold_model = threshold_model_for(&provider_registry, &model);
    let compaction_config = print_compaction_config(&environment.settings, &threshold_model);

    // Agent-level turn retry (`settings.retry`, upstream `_prepareRetry`):
    // print drives `agent_loop` itself, so it installs on `LoopDeps::retry`
    // the policy the interactive session resolves per prompt through its
    // `SettingsManager` (`agent_session/prompt_loop.rs:882-885`).
    // [`print_apply_retry`] is that single conversion+assignment seam and runs
    // per prompt run below, over this invocation's merged settings snapshot
    // (the same snapshot the compaction config above reads):
    // `retry.enabled: false` yields no policy — one attempt — and
    // `maxRetries`/backoff are honored verbatim. This budget is the
    // AGENT-level one; transport retries stay on `StreamOptions` (`Q19`'s
    // structural verdicts are unchanged).

    // Per-message persistence sink (upstream persists each message at its
    // `message_end` during the run, agent-session.ts:1144-1163): every message
    // the run appends lands in session storage immediately, in append order —
    // tool-loadout declarations, assistant messages, tool results — so a
    // mid-run crash or termination signal keeps the transcript consistent
    // instead of dropping the whole turn.
    let persist_message: PersistMessageFn = Arc::new({
        let session = Arc::clone(&session);
        move |message| {
            let session = Arc::clone(&session);
            Box::pin(async move {
                let mut guard = session.lock().await;
                if let Some(session) = guard.as_mut() {
                    if let Err(error) = session.append_message(message) {
                        eprintln!("pir: failed to persist message: {error}");
                    }
                }
            })
        }
    });

    let mut final_text = String::new();
    // Text-mode exit code (upstream print-mode.ts:118-166 reads the LAST
    // assistant message of the session after every prompt ran): a final
    // response cut off at the model's output token limit warns and exits 1.
    // An extension-command turn (`continue` below) runs no model, so the
    // previous turn's disposition stands, like upstream reading session state.
    let mut exit_code = 0;
    for (turn_index, prompt) in prompts.iter().enumerate() {
        // Upstream `prompt()` runs a leading `/name args` that matches a
        // registered extension command through the extension runtime instead
        // of prompting the model (`_tryExecuteExtensionCommand`): the
        // command-context action surface the print host binds rides the
        // extension actor's RPC mailbox (`dispatch_command`, first
        // registration wins; a registered `reload` command dispatches the
        // runtime's reload action). The turn is consumed either way; dispatch
        // failures surface on stderr like upstream `emitError`.
        if let Some(runtime) = &extensions {
            if try_extension_command_with_events(
                runtime,
                prompt,
                &session,
                &session_dir_path,
                Some(&navigate_summary),
                print_session_events.clone(),
            )
            .await
            {
                continue;
            }
        }
        // Resume: persist the user message, then rebuild the transcript from
        // storage so a continued session carries its prior history. The
        // projection snapshot (cut entries + branch + snapshot length) feeds
        // the compaction hook so its cut point can carry persisted entry ids.
        let user = Message::User(UserMessage {
            role: UserRole::User,
            content: {
                let mut content = vec![ContentBlock::Text(TextContent {
                    text: prompt.clone(),
                    text_signature: None,
                })];
                // `initialImages` ride the FIRST prompt only (upstream
                // `session.prompt(initialMessage, { images: initialImages })`
                // attaches them to that one user message).
                if turn_index == 0 {
                    content.extend(
                        options
                            .images
                            .iter()
                            .map(|image| ContentBlock::Image(image.clone())),
                    );
                }
                content
            },
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: crate::ai::util::now_ms(),
        });
        let mut messages: Vec<Message> = Vec::new();
        // This prompt's starting user message, announced by the loop right
        // after `agent_start` (upstream runAgentLoop emits the initial
        // messages' `message_start`/`message_end` pair). The message is also
        // in `messages`/storage; the preamble only carries the announcement.
        let preamble_messages = vec![user.clone()];
        let projected: Option<ProjectionSnapshot> = {
            let mut guard = session.lock().await;
            match guard.as_mut() {
                Some(session) => {
                    // A PAGED session holds a tail window only. When the
                    // threshold will not fire (a window that swallows the whole
                    // session, or compaction switched off) the model context
                    // must be the complete transcript, so the session widens to
                    // the full resident view here — the documented cost of the
                    // fallback (`compaction::paged` module docs, `SDK-SCOPE.md`
                    // §8: measured +1353 MB / one 79.4 MB request on a 100 MB
                    // session with `--context-window 100000000`, i.e. upstream
                    // pi's compaction-off behavior; NOT silently truncated).
                    let widening = match compaction_config.as_ref() {
                        Some(config) => {
                            compaction::paged_requires_widening(session, &model, config)
                        }
                        None => session.is_paged(),
                    };
                    if widening {
                        session.widen_to_full()?;
                    }
                    session.append_message(user.clone())?;
                    let projection = session.build_session_projection()?;
                    let branch = session.branch_path_meta(None);
                    // The snapshot keeps per-entry METADATA only (shared
                    // source entries + message counts): the messages move into
                    // the loop context below and the hook slices THAT list, so
                    // the conversation is materialized once (the projection
                    // aggregate) instead of once per entry plus once for the
                    // aggregate plus once for the branch.
                    let metas =
                        compaction::ProjectionEntryMeta::from_projection(&projection.entries);
                    messages = projection.messages;
                    Some((Arc::new(metas), Arc::new(branch)))
                }
                None => None,
            }
        };
        if messages.is_empty() {
            messages.push(user);
        }

        let mut deps = print_loop_deps(
            Arc::clone(&registry),
            // The background host's `install` below replays the comment
            // `print_loop_deps` carries: this builder starts sink-less, the
            // host adds the detach sink + follow-up channel for every run of
            // this invocation.
            ToolScheduler::new(config.to_scheduler_config()),
            cwd.clone(),
            extensions.clone(),
            tool_updates.clone(),
            tool_call_wait.clone(),
            // The loop's request level must equal the `reasoning` baked into
            // the stream options below, or `registry_stream*` replaces the
            // baked level with the `LoopDeps` default (`off`) and the request
            // carries no reasoning at all.
            thinking_level,
        )
        .with_provider_unchecked(
            Arc::clone(&provider_registry),
            model.clone(),
            crate::ai::types::StreamOptions {
                api_key: api_key.clone(),
                reasoning: Some(thinking_level),
                hooks: transport_hooks.clone(),
                ..Default::default()
            },
        );
        // `settings.retry` for this prompt run, through the same seam tests
        // exercise: `None` means retries are disabled, so a retryable
        // mid-stream failure ends the run after a single request.
        print_apply_retry(&mut deps, &environment.settings);
        // The non-interactive turn cap: `Config` defaults to the 1000-turn
        // guard and carries the `PIR_MAX_TURNS` override. Applied per prompt
        // run (the loop's `turns` counter is per run), like the retry policy.
        print_apply_max_turns(&mut deps, config);
        // Agent-level retry lifecycle → the same out-of-loop session-event
        // channel the interactive host uses. The loop's sink only sees turns
        // it re-issues itself, so it never duplicates the transport observer
        // on `transport_hooks`; JSON mode writes `auto_retry_start` /
        // `auto_retry_end`, text mode stays silent (sink-less, like the
        // transport observers above).
        install_print_retry_events(&mut deps, print_session_events.clone());
        deps.message_end_repair = message_end_repair.clone();
        // `ctx.executeTool` gating, the session host's exact set
        // (`build_tool_loadout` → `LoopDeps::nested_callable`): active
        // `direct` tools plus every registered `codemode`/`deferred` tool.
        // Without it the loop's default (every registered tool callable)
        // would make `model-only` tools reachable from other tools in print,
        // which upstream never allows.
        deps.nested_callable = Some(Arc::clone(&nested_callable));
        // The loop's NATIVE event face: the print host built its unified
        // plugin host before `session_start`, so every loop-level event
        // (`agent_start`/`agent_end`, `turn_*`, `message_*`,
        // `tool_execution_*`, `context`, `context_with_system`) reaches the
        // non-JS plugins exactly once, alongside the direct JS handle in
        // `deps.extensions` (the host's JS adapter is skipped by
        // `emit_native`). Without a host the field stays `None` and dispatch
        // is unchanged.
        deps.plugin_events = plugins.clone();
        // The background sink + the follow-up channel of this invocation: a
        // `background: true` call is adopted by the host's registry from here
        // on (limit, per-call abort, cross-prompt lifespan), and a completion
        // settling while this run is live reaches the running model's next
        // turn boundary through `LoopDeps::follow_up`.
        background.install(&mut deps);
        background.begin_run();
        // The print host's session environment: print drives the loop without
        // an `AgentSession`, so without this env the model-facing
        // `background_status` / `background_abort` tools (registered by
        // `register_core_tools`) would report "no registry" even though this
        // host owns one. The env carries the same handles the session env
        // does, with the background control adapter over THIS invocation's
        // registry (one registry for the whole print process, so a status
        // call in a later prompt still sees earlier prompts' calls).
        // Session identity for the PI_SESSION_* child environment (upstream
        // `ctx.sessionManager`): a `--no-session` run has neither.
        let (session_id, session_file) = {
            let guard = session.lock().await;
            match guard.as_ref() {
                Some(manager) => (
                    Some(manager.id().to_owned()),
                    manager.session_file().map(std::path::Path::to_path_buf),
                ),
                None => (None, None),
            }
        };
        deps.session_env = Some(print_tool_session_env_with_session(
            Arc::clone(&provider_registry),
            &model,
            thinking_level,
            Arc::clone(&registry),
            Arc::clone(&resolved_active),
            Some(
                Arc::clone(&background) as Arc<dyn crate::agent::tool_execution::BackgroundControl>
            ),
            session_id,
            session_file,
        ));

        // This prompt's request declarations AND system prompt: the loadout's
        // first build, or — once a `tool_search` has loaded matches into the
        // active-set cell (or another writer moved it) — a re-derivation from
        // that cell, exactly like the session host's per-iteration
        // `build_tool_loadout` (a match must be declared AND described on the
        // NEXT prompt; the in-flight run's request list and prompt stay
        // fixed). [`PrintPromptCache::sync`] reuses the first prompt
        // byte-identically while the declared set is unchanged.
        let (prompt_tools, run_system_prompt) = {
            let current = resolved_active
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // Refresh the MCP section for THIS prompt (upstream re-renders it
            // per `before_agent_start`): a server that finished connecting
            // since the last prompt shows as ready here; a transition landing
            // mid-prompt shows on the next one. Cheap synchronous reads only.
            prompt_cache.set_mcp_servers(crate::coding_agent::context::mcp_servers_prompt_section(
                plugins.as_deref(),
            ));
            (
                print_request_definitions(&registry, &tools, &active, current.as_deref()),
                prompt_cache.sync(&registry, &active, current.as_deref()),
            )
        };

        // The signal handler's graceful window: aborting this signal cancels
        // the in-flight tool call (and its process tree) exactly like the TUI's
        // Esc/ctrl+c path, instead of exiting with the child still running.
        run_active.store(true, Ordering::SeqCst);
        let result = agent_loop::run_with_options(
            deps,
            Context {
                system_prompt: Some(run_system_prompt),
                messages,
                tools: Some(prompt_tools),
            },
            Some(run_signal.clone()),
            LoopOptions {
                events: Some(Arc::clone(&events)),
                preamble_messages,
                event_stream: Some(crate::agent_session::registry_event_stream(
                    Arc::clone(&provider_registry),
                    model.clone(),
                    crate::ai::types::StreamOptions {
                        api_key: api_key.clone(),
                        reasoning: Some(thinking_level),
                        hooks: transport_hooks.clone(),
                        ..Default::default()
                    },
                )),
                // Threshold compaction over the turn's projection snapshot; the
                // hook persists the compaction entry to session storage so the
                // next turn (and a later `--continue`) replays the summarized
                // context (upstream `_runAutoCompaction("threshold")`). JSON
                // mode also streams the `compaction_start` / `compaction_end`
                // lifecycle through the same stdout frame writer the retry
                // observers above install; text mode stays silent like
                // upstream's text print mode.
                compaction: compaction_config.clone().map(|config| {
                    print_compaction_hook(
                        config,
                        threshold_model.clone(),
                        Arc::clone(&provider_registry),
                        api_key.clone(),
                        thinking_level,
                        transport_hooks.clone(),
                        Arc::clone(&session),
                        projected.clone(),
                        extensions.clone(),
                        print_session_events.clone(),
                    )
                }),
                // Per-message persistence during the run (see the sink above).
                persist_message: Some(Arc::clone(&persist_message)),
                ..LoopOptions::default()
            },
        )
        .await;
        // The run settled (the aborted tool call has already killed its process
        // tree and persisted its result). Release the signal handler's grace.
        run_active.store(false, Ordering::SeqCst);
        // The run is over: release the claim so a completion from here on is
        // stored/reported for the next prompt (or the exit settle) instead of
        // being injected into a loop that is not running.
        background.end_run();
        if run_signal.is_aborted() {
            // A termination signal aborted this run. The signal handler owns
            // the process exit (conventional 130/143/129) once `run_active`
            // clears; park past its own bound instead of returning a normal
            // outcome that could race it and mask the signal's exit code. The
            // extra time is only a fallback for a handler that could not be
            // installed.
            tokio::time::sleep(SIGNAL_ABORT_GRACE + std::time::Duration::from_secs(3)).await;
            break;
        }

        // Persistence now happens per message DURING the run (the
        // `persist_message` sink above), matching upstream's per-message
        // `message_end` writes — the whole-turn batch write is gone. The
        // extension notifications still drain after the run settles.
        if let Some(runtime) = &extensions {
            crate::agent_session::drain_extension_notifications(runtime);
        }
        // Upstream `_runAgentPrompt` emits `agent_settled` from its `finally`
        // (agent-session.ts:1849 → `_emitAgentSettled`), i.e. once per prompt
        // run on EVERY exit — success, error stop reason, abort — after the
        // queued side effects are flushed, with `aborted` = the run's abort
        // flag. The print host drives the loop itself instead of
        // `AgentSession::prompt`, so the settle emission lives here. One event
        // per run: an extension-command turn (`continue` above) never reaches
        // this point and emits none, matching upstream's early return from
        // `prompt()` BEFORE `_runAgentPrompt`.
        dispatch_print_agent_settled(plugins.as_ref(), extensions.as_ref(), result.aborted).await;
        // Upstream print-mode.ts:139-156 treats an `error` OR `aborted`
        // stopReason as a process failure only in TEXT mode (exit 1 after
        // printing the error). In JSON mode the process exits 0 and the
        // terminal message arrives as an event line (the terminal message is
        // re-emitted through the standard event serializer), so JSON
        // consumers see the failure in-band.
        if matches!(
            result.terminal.stop_reason,
            crate::ai::types::StopReason::Error | crate::ai::types::StopReason::Aborted
        ) {
            if options.mode == OutputMode::Json {
                crate::coding_agent::json::print_event(
                    &crate::agent::agent_loop::AgentEvent::TurnEnd {
                        turn: turn_index,
                        message: result.terminal.clone(),
                        // The run's real TurnEnd already carried the tool
                        // results; this is the terminal re-emission.
                        tool_results: result.tool_results.clone(),
                    },
                );
            } else {
                // Upstream: `errorMessage || "Request " + stopReason`.
                let error_text = result
                    .terminal
                    .error_message
                    .clone()
                    .unwrap_or_else(|| format!("Request {}", result.terminal.stop_reason));
                // The text-mode error return leaves no stdout payload to
                // deliver into, so in-flight background calls are aborted
                // immediately (never waited for, never left dangling) and
                // reported on stderr. The session-teardown abort
                // (`AgentSession::drop`) is the interactive counterpart.
                for status in background.abort_in_flight().await {
                    eprintln!(
                        "{}",
                        background_abort_diagnostic(&status, "the run ended with a terminal error")
                    );
                }
                if let Some(plugins) = &plugins {
                    plugins.shutdown().await;
                }
                // The failed run's diagnostics still land before the error
                // propagates (stderr; text mode emits no JSON frames).
                finish_run_diagnostics(&run_diagnostics, options.mode);
                return Err(anyhow::anyhow!(error_text));
            }
        }
        // The reported text is the LAST assistant message of the run
        // (upstream reads `state.messages[messages.length - 1]`).
        final_text = result
            .terminal
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Text mode only (upstream `if (mode === "text")`): the partial text
        // was already captured above and is still written to stdout; the
        // truncation warning goes to stderr and the process exits 1.
        if options.mode == OutputMode::Text {
            exit_code = match truncation_notice(result.terminal.stop_reason) {
                Some(warning) => {
                    eprintln!("{warning}");
                    1
                }
                None => 0,
            };
        }
    }
    // Background calls that outlived the last prompt: the host waits (bounded,
    // `DEFAULT_BACKGROUND_EXIT_WAIT`) for their completions — a completion
    // that landed while a later prompt ran was already injected into that
    // live run, an out-of-run completion lands in the transcript and in the
    // report log — then ABORTS what is still running. Losing a result must
    // never be silent: every aborted call is reported on stderr in both
    // modes, and in `--mode json` as a same-shaped `background_tool_end`
    // frame (text mode turns the loss into exit 1 below). A completion whose
    // call settled but whose DELIVERY outlived the budget and the abort grace
    // has nothing left to abort and is reported through
    // `BackgroundExit::undelivered` below — the last disposition of the exit
    // rule, and the reason it has no silent arm.
    let background_exit = background.settle(DEFAULT_BACKGROUND_EXIT_WAIT).await;
    if background_exit.timed_out {
        let reason = format!(
            "still running {} ms after the last prompt finished",
            DEFAULT_BACKGROUND_EXIT_WAIT.as_millis()
        );
        for status in &background_exit.aborted {
            eprintln!("{}", background_abort_diagnostic(status, &reason));
            if options.mode == OutputMode::Json {
                crate::coding_agent::json::print_session_event(&background_abort_event(
                    status, &reason,
                ));
            }
        }
        // Text mode: the run started calls whose results never arrived, so
        // the process exits nonzero (the upstream text-mode convention for a
        // run that could not deliver what it promised). Json mode stays
        // in-band (the abort frames above are the report), like every other
        // json-mode failure.
        if options.mode == OutputMode::Text {
            exit_code = 1;
        }
    }
    // The one disposition with nothing to abort and no call left in flight:
    // a completion task that outlived both the budget and the abort grace is
    // still mid-delivery (the event was emitted before persistence, so the
    // frame is visible in `--mode json`; a transcript entry or a follow-up
    // injection may be incomplete). Neither a call nor a result may vanish
    // silently, so this is reported on stderr in both modes — and text mode
    // exits nonzero like every other missing-result exit.
    if background_exit.undelivered > 0 {
        eprintln!(
            "pir: {} background completion(s) were still being delivered at exit",
            background_exit.undelivered
        );
        if options.mode == OutputMode::Text {
            exit_code = 1;
        }
    }
    // Text mode: append one framed block per observed completion (the
    // `backgroundToolResult` frames, settle order), so a result that arrived
    // after the final answer is still visible on stdout.
    if options.mode == OutputMode::Text {
        final_text = text_with_background_reports(&final_text, &background.reports());
    }
    // The plugin host's lifecycle ends with the run (the session does the
    // same on teardown). A no-op while nothing connected.
    if let Some(plugins) = &plugins {
        plugins.shutdown().await;
    }
    // Run diagnostics (PI_TIMING=1): stderr block, plus the final JSON frame
    // in --mode json. Emitted after every event line, before the outcome.
    finish_run_diagnostics(&run_diagnostics, options.mode);
    Ok(PrintOutcome {
        text: final_text,
        exit_code,
    })
}

/// A terminal assistant message carrying `message` as the error stop reason
/// (mirrors `crate::ai::provider::error_message` for the no-model case, where
/// no `Model` exists yet).
fn terminal_error_message(message: &str) -> crate::ai::types::AssistantMessage {
    crate::ai::types::AssistantMessage {
        role: crate::ai::types::AssistantRole::Assistant,
        content: Vec::new(),
        api: String::new(),
        provider: String::new(),
        model: String::new(),
        response_model: None,
        response_id: None,
        diagnostics: Vec::new(),
        usage: Default::default(),
        stop_reason: crate::ai::types::StopReason::Error,
        deferred: None,
        error_message: Some(message.to_owned()),
        raw_stop_reason: None,
        end_turn: None,
        provider_thinking_level: None,
        thinking_level: None,
        duration_ms: None,
        timestamp: crate::ai::util::now_ms(),
    }
}

/// A no-prompt JSON print pass (upstream `print-mode.ts:122-127`): create or
/// open the planned session and emit its header line, then exit without a run.
/// Text mode keeps its explicit "no prompt given" diagnostic instead.
pub fn run_empty_json_print(
    plan: &SessionPlan,
    session_dir: &std::path::Path,
    cwd: &str,
) -> crate::Result<()> {
    match open_session_manager(plan, cwd, session_dir)? {
        Some(session) => match session.header() {
            Some(header) => crate::coding_agent::json::print_session_header(header),
            None => print_ephemeral_header(cwd),
        },
        None => print_ephemeral_header(cwd),
    }
    Ok(())
}

/// Print a synthesized session header for an ephemeral (`--no-session`) JSON
/// run (upstream print-mode.ts writes the in-memory session manager's header
/// first): the same `{type:"session",version,id,timestamp,cwd}` shape as a
/// persisted header.
fn print_ephemeral_header(cwd: &str) {
    let header = crate::session_manager::SessionHeader::new(
        crate::session_manager::new_session_id(),
        cwd,
        crate::session_manager::iso_from_ms(crate::ai::util::now_ms()),
    );
    crate::coding_agent::json::print_session_header(&header);
}

/// Rebuild a registry keeping only the enabled tools (the registry has no
/// remove API; tools are `Arc` clones, so re-registering the survivors is
/// equivalent).
fn filter_registry(registry: &ToolRegistry, enabled: &[String]) -> ToolRegistry {
    let mut filtered = ToolRegistry::new();
    for tool in registry.iter() {
        if enabled.iter().any(|name| name == tool.name()) {
            filtered.register(tool.clone());
        }
    }
    filtered
}

/// Resolve one `--append-system-prompt` entry: an existing file path's
/// contents (BOM-stripped like [`resolve_prompt_input`], upstream
/// `resolvePromptInput` applies to every append source,
/// resource-loader.ts:663-669), otherwise the literal text. A read failure on
/// an existing path is a hard error here (upstream downgrades it to a
/// warning; pir keeps the stricter surface).
pub fn resolve_text_or_file(entry: &str) -> crate::Result<String> {
    let path = PathBuf::from(entry);
    if path.is_file() {
        let text = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!("cannot read --append-system-prompt file {entry}: {error}")
        })?;
        return Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned());
    }
    Ok(entry.to_string())
}

/// Resolve the `--system-prompt` value with upstream `resolvePromptInput`
/// semantics (resource-loader.ts:167-182): a value naming an existing
/// filesystem path is read as a file and BOM-stripped; a read failure warns
/// (upstream `console.error(chalk.yellow(...))`) and the literal value still
/// stands; anything else is the literal text. Mirrors the text-or-file
/// resolution `--append-system-prompt` already applies.
pub fn resolve_prompt_input(input: &str) -> String {
    let path = PathBuf::from(input);
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(text) => return text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(),
            Err(error) => {
                eprintln!("Warning: Could not read system prompt file {input}: {error}");
            }
        }
    }
    input.to_owned()
}

// ---------------------------------------------------------------------------
// Termination handlers (upstream print-mode.ts:50-68)
// ---------------------------------------------------------------------------

/// Upper bound on the graceful termination window. The abort wakes the shell
/// tool's cancel branch, whose `ProcessTreeKill::terminate` is synchronous
/// (the Windows sweeps take ~150-300 ms, Unix is one `killpg`); the run then
/// settles and clears `run_active`. A wedged run still exits with the
/// conventional code.
const SIGNAL_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Abort the in-flight run, give it [`SIGNAL_ABORT_GRACE`] to settle (the tool
/// result, including the process-tree kill, is persisted during the run), then
/// exit with the signal's conventional code.
async fn terminate_after_abort(
    abort: crate::ai::types::AbortSignal,
    run_active: Arc<std::sync::atomic::AtomicBool>,
    name: &'static str,
    code: i32,
) -> ! {
    eprintln!("pir: terminated by {name}");
    abort.abort();
    let deadline = std::time::Instant::now() + SIGNAL_ABORT_GRACE;
    while run_active.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    std::process::exit(code);
}

/// Install the print host's termination handlers: each listener aborts the
/// in-flight run and then exits the process with the conventional code —
/// SIGTERM→143, SIGHUP→129 and SIGINT→130 (upstream print-mode.ts registers
/// SIGTERM/SIGHUP only and exits directly; pir adds the SIGINT abort and lets
/// the abort reach the foreground command's process tree). On Windows there is
/// no SIGTERM/SIGHUP delivery for `pir -p`; the console control events are
/// mapped instead: console close and Ctrl+Break → 143 (the SIGTERM analog),
/// Ctrl+C → 130 (the abort/SIGINT analog, 128+2). Session persistence is
/// per-message (`persist_message` sink), so the exit keeps every message
/// already streamed — the upstream `disposeRuntime` counterpart that pir needs
/// is the completed per-message writes themselves.
fn install_signal_handlers(
    abort: crate::ai::types::AbortSignal,
    run_active: Arc<std::sync::atomic::AtomicBool>,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for (kind, code, name) in [
            (SignalKind::terminate(), 143, "SIGTERM"),
            (SignalKind::hangup(), 129, "SIGHUP"),
            (SignalKind::interrupt(), 130, "SIGINT"),
        ] {
            let Ok(mut stream) = signal(kind) else {
                eprintln!("pir: cannot listen for {name}");
                continue;
            };
            let abort = abort.clone();
            let run_active = Arc::clone(&run_active);
            tokio::spawn(async move {
                if stream.recv().await.is_some() {
                    terminate_after_abort(abort, run_active, name, code).await;
                }
            });
        }
    }
    #[cfg(windows)]
    {
        if let Ok(mut stream) = tokio::signal::windows::ctrl_close() {
            let abort = abort.clone();
            let run_active = Arc::clone(&run_active);
            tokio::spawn(async move {
                if stream.recv().await.is_some() {
                    terminate_after_abort(abort, run_active, "console close", 143).await;
                }
            });
        }
        if let Ok(mut stream) = tokio::signal::windows::ctrl_break() {
            let abort = abort.clone();
            let run_active = Arc::clone(&run_active);
            tokio::spawn(async move {
                if stream.recv().await.is_some() {
                    terminate_after_abort(abort, run_active, "ctrl-break", 143).await;
                }
            });
        }
        // `tokio::signal::ctrl_c()` is an async registration on this tokio
        // version; the abort (SIGINT) analog exits 130 (128+2).
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                terminate_after_abort(abort, run_active, "ctrl-c", 130).await;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Extension commands (upstream print-mode.ts:78-100 commandContextActions)
// ---------------------------------------------------------------------------

/// Execute a leading `/name args` prompt as an extension command (upstream
/// `AgentSession.prompt` → `_tryExecuteExtensionCommand`, whose command
/// context carries the `commandContextActions` surface): the first registered
/// command wins and the handler runs over the extension actor's RPC mailbox
/// (`dispatch_command`). A registered `reload` command dispatches the
/// runtime's reload action. Session-control actions the handler requested
/// through the command ctx (`ctx.newSession` / `ctx.fork` /
/// `ctx.navigateTree` / `ctx.switchSession` / `ctx.reload` — upstream
/// print-mode.ts:78-100 `commandContextActions`) are drained from the
/// crate-level action queue and applied against this host's shared session
/// storage after the handler settles; application failures surface on stderr
/// like upstream `emitError`, and the turn is consumed either way, like
/// upstream's "handled". `false` means no extension command matched and the
/// prompt falls through to the model.
pub async fn try_extension_command(
    runtime: &crate::extensions::ExtensionHandle,
    prompt: &str,
    session: &SharedSession,
    session_dir: &std::path::Path,
    navigate_summary: Option<&crate::agent_session::NavigateSummaryBinding>,
) -> bool {
    try_extension_command_impl(
        runtime,
        prompt,
        session,
        session_dir,
        navigate_summary,
        None,
    )
    .await
}

/// [`try_extension_command`] with the host's out-of-loop session-event sink.
///
/// `run_print` installs it in JSON mode so a `navigateTree({summarize})`
/// branch summary's own retry lifecycle (upstream
/// `summarization_retry_scheduled` / `summarization_retry_attempt_start` /
/// `summarization_retry_finished`) reaches the `--mode json` stream. Text mode
/// passes `None` and emits nothing, exactly like every other print session
/// event.
async fn try_extension_command_with_events(
    runtime: &crate::extensions::ExtensionHandle,
    prompt: &str,
    session: &SharedSession,
    session_dir: &std::path::Path,
    navigate_summary: Option<&crate::agent_session::NavigateSummaryBinding>,
    session_events: Option<PrintSessionEventSink>,
) -> bool {
    try_extension_command_impl(
        runtime,
        prompt,
        session,
        session_dir,
        navigate_summary,
        session_events,
    )
    .await
}

async fn try_extension_command_impl(
    runtime: &crate::extensions::ExtensionHandle,
    prompt: &str,
    session: &SharedSession,
    session_dir: &std::path::Path,
    navigate_summary: Option<&crate::agent_session::NavigateSummaryBinding>,
    session_events: Option<PrintSessionEventSink>,
) -> bool {
    if !prompt.starts_with('/') {
        return false;
    }
    let (name, args) = match prompt.find(' ') {
        Some(index) => (&prompt[1..index], &prompt[index + 1..]),
        None => (&prompt[1..], ""),
    };
    if name.is_empty() {
        return false;
    }
    if !runtime
        .command_defs()
        .iter()
        .any(|command| command.name == name)
    {
        return false;
    }
    match runtime.dispatch_command(name, args).await {
        Ok(outcome) => {
            if let Some(error) = outcome.error {
                eprintln!("[ext] command:{name}: {error}");
            }
        }
        Err(error) => eprintln!("[ext] command:{name}: {error}"),
    }
    // Apply the command-context session actions (see the doc above). Only the
    // actions stamped with THIS host's identity are claimed — actions recorded
    // by other hosts stay queued for their owner. Each claimed action is
    // consumed even when its application fails; an ephemeral run
    // (`--no-session`, `None` storage) rejects every action like the upstream
    // runtimeHost does without session storage.
    for entry in crate::agent_session::take_session_command_actions_for(
        &crate::agent_session::SessionOrigin::Print,
    ) {
        // Three phases so the provider call for `navigateTree({summarize})`
        // runs with the shared session lock RELEASED (it used to be held for
        // the whole summarizer stream):
        //   1. prepare under the lock — synchronous actions are applied, the
        //      navigate plan (abandoned branch + leaf) is captured;
        //   2. summarize without the lock;
        //   3. re-lock and commit after the leaf re-check.
        let prepared = {
            let mut guard = session.lock().await;
            crate::agent_session::prepare_session_command_to_storage(
                &mut guard,
                session_dir,
                &entry.action,
                navigate_summary,
            )
        };
        let result = match prepared {
            Err(error) => Err(error),
            Ok(crate::agent_session::SessionCommandStep::Applied) => Ok(()),
            Ok(crate::agent_session::SessionCommandStep::Navigate(plan)) => {
                match navigate_summary {
                    Some(binding) => {
                        // The storage-level summarizer entry point takes no
                        // observer (it is shared with hosts that build their
                        // own channel), so the print host scopes the
                        // compaction module's public observer around the call
                        // itself: the summarizer's dropped-stream retry then
                        // reports `summarization_retry_*` (source
                        // `branchSummary`) through the same sink.
                        let generated = match session_events.as_ref() {
                            Some(sink) => {
                                generate_navigation_summary_with_retry_events(
                                    &plan,
                                    binding,
                                    Arc::clone(sink),
                                )
                                .await
                            }
                            None => {
                                crate::agent_session::generate_navigation_summary(&plan, binding)
                                    .await
                            }
                        };
                        match generated {
                            Err(error) => Err(error),
                            Ok(summary) => {
                                let mut guard = session.lock().await;
                                crate::agent_session::commit_navigation_to_storage(
                                    &mut guard, plan, summary,
                                )
                            }
                        }
                    }
                    None => Err(
                        "navigateTree(summarize) requires a provider-backed session host"
                            .to_owned(),
                    ),
                }
            }
        };
        if let Err(error) = result {
            eprintln!("[ext] command:{name}: {:?}: {error}", entry.action);
        }
    }
    true
}

/// The print host's summarizer-retry observer: maps the compaction module's
/// retry transitions onto the upstream `summarization_retry_*` session events
/// for `source` and writes them through `sink`.
///
/// One mapping serves both out-of-loop summarizer paths — the extension
/// command context's `navigateTree({summarize})` (source `branchSummary`) and
/// the threshold compaction summarizer (source `compaction`, reason
/// `threshold`) — so the wire shapes cannot drift apart. Public for tests/.
pub fn print_summarization_retry_observer(
    sink: PrintSessionEventSink,
    source: crate::agent_session::SummarizationRetrySource,
) -> compaction::SummarizationRetryObserver {
    Arc::new(move |event| {
        let session_event = match event {
            compaction::SummarizationRetryEvent::Scheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => SessionEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            },
            compaction::SummarizationRetryEvent::AttemptStart => {
                SessionEvent::SummarizationRetryAttemptStart { source }
            }
            compaction::SummarizationRetryEvent::Finished => {
                SessionEvent::SummarizationRetryFinished
            }
        };
        sink(&session_event);
    })
}

/// Run one threshold-compaction summarizer pipeline with that run's retry
/// observer installed (upstream `_runAutoCompaction("threshold")` wraps its
/// summarizer in `_summarizationRetryCallbacks({source: "compaction",
/// reason: "threshold"})`).
///
/// The observer is the compaction module's task-local seam
/// ([`compaction::with_summarization_retry`]), so every nested summarizer call
/// of the run (history + split-turn prefix + chunk folds) reports its single
/// dropped-stream retry as `summarization_retry_scheduled` /
/// `summarization_retry_attempt_start` (source `compaction`, reason
/// `threshold`) / `summarization_retry_finished` through the host's
/// out-of-loop sink. Text mode (`None`) runs the future untouched and emits
/// nothing, like every other print session event.
async fn with_threshold_compaction_retry_events<F, T>(
    session_events: &Option<PrintSessionEventSink>,
    future: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    match session_events {
        Some(sink) => {
            compaction::with_summarization_retry(
                print_summarization_retry_observer(
                    Arc::clone(sink),
                    crate::agent_session::SummarizationRetrySource::Compaction {
                        reason: "threshold",
                    },
                ),
                future,
            )
            .await
        }
        None => future.await,
    }
}

/// Run the branch summarizer with the summarizer-retry observer installed and
/// its lifecycle frames written to `sink`.
///
/// The storage-level [`crate::agent_session::generate_navigation_summary`]
/// accepts no observer (it is shared with hosts that build their own event
/// channel), so the print host installs the compaction module's public
/// task-local observer around the call itself — the same seam the session
/// wraps its navigate summary in (`compaction::with_summarization_retry`).
/// The summarizer's dropped-stream retry then reports upstream
/// `summarization_retry_scheduled` / `summarization_retry_attempt_start`
/// (source `branchSummary`) / `summarization_retry_finished` through the
/// host's out-of-loop sink. Public for tests/.
pub async fn generate_navigation_summary_with_retry_events(
    plan: &crate::agent_session::PreparedNavigateTree,
    binding: &crate::agent_session::NavigateSummaryBinding,
    sink: PrintSessionEventSink,
) -> Result<crate::agent_session::NavigateSummaryOutcome, String> {
    let observer = print_summarization_retry_observer(
        sink,
        crate::agent_session::SummarizationRetrySource::BranchSummary,
    );
    compaction::with_summarization_retry(
        observer,
        crate::agent_session::generate_navigation_summary(plan, binding),
    )
    .await
}

// ---------------------------------------------------------------------------
// Automatic compaction (upstream AgentSession threshold compaction)
// ---------------------------------------------------------------------------

/// The model compaction thresholds are checked against (agent_session
/// `threshold_model_for`): a virtual selection checks the ROUTED physical
/// model's context window (with its limit overrides applied); an unrouted
/// virtual model falls back to its catalog entry (context window 0 disables
/// threshold compaction, like upstream).
pub fn threshold_model_for(
    registry: &ProviderRegistry,
    model: &crate::ai::types::Model,
) -> crate::ai::types::Model {
    if crate::ai::virtual_::is_virtual_model(model) {
        if let Some(registration) = registry
            .virtual_model_registry()
            .get(&model.provider, &model.id)
        {
            let physical = registry
                .get_physical_model(&registration.physical.provider, &registration.physical.id);
            return registration.catalog_model(physical.as_ref());
        }
    }
    model.clone()
}

/// The print host's compaction config from the settings `compaction` block
/// (upstream `SETTINGS_DEFAULTS.compaction` + the per-model override, the same
/// resolution SettingsManager's `compactionToken` applies).
pub fn print_compaction_config(
    settings: &crate::config::settings::Settings,
    model: &crate::ai::types::Model,
) -> Option<CompactionConfig> {
    use crate::config::defaults::settings_defaults;
    let compaction = settings.compaction.as_ref();
    let enabled = compaction
        .and_then(|compaction| compaction.enabled)
        .unwrap_or(settings_defaults::COMPACTION_ENABLED);
    if !enabled {
        return None;
    }
    let key = format!("{}/{}", model.provider, model.id);
    let override_settings = compaction
        .and_then(|compaction| compaction.model_overrides.as_ref())
        .and_then(|overrides| overrides.get(&key));
    let reserve_tokens = override_settings
        .and_then(|over| over.reserve_tokens)
        .or_else(|| compaction.and_then(|compaction| compaction.reserve_tokens))
        .unwrap_or(settings_defaults::COMPACTION_RESERVE_TOKENS);
    let keep_recent_tokens = override_settings
        .and_then(|over| over.keep_recent_tokens)
        .or_else(|| compaction.and_then(|compaction| compaction.keep_recent_tokens))
        .unwrap_or(settings_defaults::COMPACTION_KEEP_RECENT_TOKENS);
    Some(CompactionConfig {
        enabled: true,
        reserve_tokens,
        keep_recent_tokens,
    })
}

/// The print host's out-of-loop session-event sink: every compaction
/// lifecycle frame (`compaction_start` / `compaction_end`) is written through
/// this closure. JSON mode installs [`json_session_event_sink`] — the stdout
/// frame writer [`crate::coding_agent::json::print_session_event`] the
/// transport retry observers above already use — and text mode installs
/// `None`, so a text print run emits no session events (upstream text print
/// mode is silent). Public for tests/, which record the frames instead of
/// writing stdout.
pub type PrintSessionEventSink = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// The JSON-mode sink: the print host's stdout session-event frame writer
/// (see [`crate::coding_agent::json::print_session_event`]).
pub fn json_session_event_sink() -> PrintSessionEventSink {
    Arc::new(crate::coding_agent::json::print_session_event)
}

/// Emit one compaction lifecycle frame to the installed sink. A `None` sink
/// (text mode) is silent — there is no other surface for these frames.
fn emit_compaction_event(sink: &Option<PrintSessionEventSink>, event: SessionEvent) {
    if let Some(sink) = sink {
        sink(&event);
    }
}

/// Report a failed threshold compaction: the matching `compaction_end` frame
/// (when the run had emitted its `compaction_start` — `started`) followed by
/// the extension `session_compact_failed` event (only when a handler exists,
/// upstream's `hasHandlers` gate). This is agent_session's catch order
/// (`compaction_end` first, then the extension event,
/// agent-session.ts:3272-3296; `agent_session/compact.rs`'s
/// `emit_threshold_compaction_persistence_failure`).
async fn report_threshold_compaction_failure(
    runtime: &Option<crate::extensions::ExtensionHandle>,
    session_events: &Option<PrintSessionEventSink>,
    error: &str,
    started: bool,
) {
    if started {
        emit_compaction_event(session_events, compaction_end_failure(error));
    }
    if let Some(runtime) = runtime {
        if runtime.has_handlers("session_compact_failed") {
            let mut payload = serde_json::json!({
                "type": "session_compact_failed",
                "reason": "threshold",
                "errorMessage": format!("Auto-compaction failed: {error}"),
                "aborted": false,
                "willRetry": false,
                "fromExtension": false,
            });
            if let Err(dispatch_error) = runtime.emit("session_compact_failed", &mut payload).await
            {
                eprintln!("[ext] session_compact_failed dispatch failed: {dispatch_error}");
            }
        }
    }
}

/// The session identity a paged compaction was captured from (see the paged
/// fast path in [`print_compaction_hook`]). The provider call runs with the
/// shared session lock RELEASED; this stamp is re-checked before the
/// compaction entry is appended.
struct PagedStamp {
    session_id: String,
    session_file: Option<PathBuf>,
    leaf_id: Option<String>,
}

/// Whether a paged compaction captured at `stamp` may still be committed to
/// `session`: same session identity/file, and the captured leaf still lies on
/// the current leaf path. Appends that landed during the summarizer keep the
/// captured leaf an ancestor of the new leaf (valid: their messages stay in
/// the retained tail); a session switch or a branch move off the captured
/// path invalidates the plan and the entry is NOT appended.
fn paged_compaction_target_valid(
    session: &crate::session_manager::SessionManager,
    stamp: &PagedStamp,
) -> bool {
    if session.id() != stamp.session_id.as_str() {
        return false;
    }
    if session.session_file() != stamp.session_file.as_deref() {
        return false;
    }
    match stamp.leaf_id.as_deref() {
        None => true,
        Some(leaf) => session
            .branch_path_meta(None)
            .iter()
            .any(|entry| entry.id.as_deref() == Some(leaf)),
    }
}

/// `compaction_start` for a threshold run (upstream `_runAutoCompaction`
/// `_emit({type: "compaction_start", reason: "threshold"})`).
fn compaction_start_frame() -> SessionEvent {
    SessionEvent::CompactionStart {
        reason: "threshold",
    }
}

/// The successful `compaction_end`: the upstream wire `CompactionResult`
/// ([`print_compaction_result_wire`]) with `estimatedTokensAfter` estimated
/// from the rebuilt context.
fn compaction_end_success(
    result: &compaction::CompactionResult,
    rebuilt_messages: &[Message],
) -> SessionEvent {
    SessionEvent::CompactionEnd {
        reason: "threshold",
        result: Some(print_compaction_result_wire(
            result,
            compaction::estimate_context_tokens(rebuilt_messages).tokens,
        )),
        aborted: false,
        will_retry: false,
        error_message: None,
    }
}

/// The failed `compaction_end`: upstream's `Auto-compaction failed: …`
/// message (agent_session's threshold hook uses the same text), `result`
/// absent.
fn compaction_end_failure(error: impl std::fmt::Display) -> SessionEvent {
    SessionEvent::CompactionEnd {
        reason: "threshold",
        result: None,
        aborted: false,
        will_retry: false,
        error_message: Some(format!("Auto-compaction failed: {error}")),
    }
}

/// The wire `CompactionResult` object for a `compaction_end` frame
/// (agent_session `compaction_result_wire`, upstream `CompactionResult`):
/// summary / firstKeptEntryId / tokensBefore / estimatedTokensAfter / usage /
/// details.
fn print_compaction_result_wire(
    result: &compaction::CompactionResult,
    estimated_tokens_after: u64,
) -> serde_json::Value {
    serde_json::json!({
        "summary": result.summary,
        "firstKeptEntryId": result.first_kept_entry_id,
        "tokensBefore": result.tokens_before,
        "estimatedTokensAfter": estimated_tokens_after,
        "usage": serde_json::to_value(result.usage).unwrap_or(serde_json::Value::Null),
        "details": print_compaction_details(result),
    })
}

/// The threshold gate over a borrowed cut basis — the resident pipeline's own
/// rule (upstream `_checkCompaction`), shared with the widened fallback:
/// projected tokens when a compaction boundary/context edit makes the plain
/// usage unreliable, the plain message-list estimate otherwise.
fn print_compaction_threshold_met(
    refs: &[compaction::CutEntryRef<'_>],
    branch: &[compaction::PathEntryMeta],
    messages: &[Message],
    model: &crate::ai::types::Model,
    config: &CompactionConfig,
) -> bool {
    let plain_estimate = branch.is_empty() && refs.iter().all(|entry| !entry.is_compaction());
    let context_tokens = if plain_estimate {
        compaction::estimate_context_tokens(messages).tokens
    } else {
        compaction::estimate_projected_context_tokens(refs, branch).tokens
    };
    compaction::should_compact_tokens(context_tokens, model.context_window, config)
}

/// Build the print host's loop compaction hook (agent_session
/// `compaction_hook`, threshold reason, minus the extension
/// `session_before_compact` gating the print host does not wire): when the
/// trigger fires, the cut point is computed over the projection snapshot plus
/// the run's newer messages, the summarizer call(s) run against the threshold
/// model, and on success the compaction entry is appended to session storage
/// with the cut point as `firstKeptEntryId`. Failure returns the original
/// context unchanged (the run continues un-compacted) and reports the failure
/// to extensions as `session_compact_failed` (reason `threshold`), the same
/// event agent_session's hook emits.
///
/// The snapshot is the cut basis only while no compaction succeeded in this
/// run. A successful compaction rebuilds the context (`[summary, retained
/// tail]`) and moves the session leaf onto the compaction entry, so the next
/// threshold trigger of the SAME run re-reads the live session's per-entry
/// metadata and leaf-path metadata (metadata only; no second materialization
/// of the conversation) — `agent_session/compact.rs`'s `snapshot_stale` rule.
/// Without it, a second compaction zips the pre-compaction entry counts/ids
/// against the rebuilt context and persists a `firstKeptEntryId` from before
/// the earlier boundary (or `None`, which self-retains and drops the newest
/// kept messages).
///
/// Session-event parity (upstream `_runAutoCompaction("threshold")`): every
/// run that starts emits `compaction_start {reason: "threshold"}` through
/// `session_events`, and every outcome emits its matching `compaction_end` —
/// the wire result on success, `aborted: false, willRetry: false,
/// errorMessage: Some(..)` on failure. A run that never starts (threshold
/// unmet, nothing to compact) emits NOTHING: in particular the paged fast
/// path's `ThresholdNotMet` no-op stays silent, which is why its trigger is
/// checked before the call ([`compaction::paged::threshold_met`]).
///
/// The summarizer's own single retry is a separate budget: each summarizer
/// pipeline runs inside [`with_threshold_compaction_retry_events`], so a
/// dropped summarizer stream emits the upstream `summarization_retry_*`
/// lifecycle (source `compaction`, reason `threshold`) between the
/// `compaction_start` / `compaction_end` pair. A run without a retry emits no
/// `summarization_retry_*` frame at all.
/// Rebuild a complete context after a paged compaction attempt failed.
/// The live context is normally the resident tail window; sending it as the
/// "full" history would silently drop the skipped prefix. Returns `None` when
/// the session is not paged (the caller then has a complete resident context).
async fn widen_paged_context(
    session: &SharedSession,
    context: &crate::ai::types::Context,
) -> Option<Arc<crate::ai::types::Context>> {
    let projection = {
        let mut guard = session.lock().await;
        let session = guard.as_mut()?;
        if !session.is_paged() {
            return None;
        }
        if let Err(error) = session.widen_to_full() {
            eprintln!("[session] paged widening failed: {error}");
            return None;
        }
        session.build_session_projection().ok()
    };
    let projection = projection?;
    Some(Arc::new(crate::ai::types::Context {
        system_prompt: context.system_prompt.clone(),
        messages: projection.messages,
        tools: context.tools.clone(),
    }))
}

/// Public for tests/.
#[allow(clippy::too_many_arguments)]
pub fn print_compaction_hook(
    config: CompactionConfig,
    model: crate::ai::types::Model,
    registry: Arc<ProviderRegistry>,
    api_key: Option<String>,
    thinking_level: crate::ai::types::ThinkingLevel,
    hooks: crate::ai::types::ProviderStreamHooks,
    session: SharedSession,
    projected: Option<ProjectionSnapshot>,
    runtime: Option<crate::extensions::ExtensionHandle>,
    session_events: Option<PrintSessionEventSink>,
) -> CompactionFn {
    // The summarizer completes against the same (routed) model the threshold
    // was checked against (agent_session `complete_fn_for`), with the same
    // per-request transport hooks (upstream compaction summaries go through
    // the session streamFn, so the extension transport hooks fire for them
    // too — sdk.ts:420-434).
    let complete_model = model.clone();
    let complete: Arc<compaction::CompleteFn> = Arc::new(Box::new(move |context| {
        let registry = Arc::clone(&registry);
        let model = complete_model.clone();
        let options = crate::ai::types::StreamOptions {
            api_key: api_key.clone(),
            reasoning: Some(thinking_level),
            hooks: hooks.clone(),
            ..Default::default()
        };
        Box::pin(async move {
            let stream = registry.stream_resolving_auth(model, context, options);
            // Completion consumer: drain while waiting (bounded queue; see
            // `drain_and_result`).
            match stream.drain_and_result().await {
                Some(message) => Ok((*message).clone()),
                None => Err("summarization stream did not terminate".to_owned()),
            }
        })
    }));
    // Flipped after a successful compaction (see the doc above): later turns
    // of the SAME print run rebuild the cut basis from the live session
    // instead of re-slicing the pre-compaction snapshot.
    let snapshot_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Box::new(move |context: Arc<crate::ai::types::Context>| {
        let config = config.clone();
        let model = model.clone();
        let complete = Arc::clone(&complete);
        let session = Arc::clone(&session);
        let projected = projected.clone();
        let snapshot_stale = Arc::clone(&snapshot_stale);
        let runtime = runtime.clone();
        let session_events = session_events.clone();
        Box::pin(async move {
            // PAGED fast path (a session opened tail-first, see
            // `SessionManager::open_paged`): the skipped prefix is summarized
            // straight from the file, chunk by chunk, so a 100 MB transcript
            // never becomes resident. Falls through to the resident pipeline
            // when the session is not paged, when the prefix needs the full
            // projection (`context_edit`), or when a widening already happened.
            enum PagedOutcome {
                Compacted(Box<compaction::CompactionResult>),
                Skip,
                Failed(String),
                Resident,
            }
            // Whether the paged run's `compaction_start` frame was emitted
            // (only then may a matching `compaction_end` follow).
            let mut paged_started = false;
            // The shared session is NOT held across the provider calls: the
            // bounded resident view is captured under the lock (the same
            // discipline agent_session/compact.rs uses for the resident
            // pipeline), the guard is dropped, and the summarizer streams
            // while every other `SharedSession` user (background-result
            // persistence, per-message persistence, header reads) stays live.
            // The capture-time identity/leaf is the commit-time staleness
            // check: if the session was replaced or the leaf left the captured
            // branch, the compaction entry is not appended over a moved target.
            enum PagedPreparation {
                Inputs(compaction::paged::PagedCompactionInputs),
                Failed(String),
                Skip,
                Resident,
            }
            let (paged_preparation, paged_stamp) = {
                let guard = session.lock().await;
                match guard.as_ref() {
                    Some(paged_session) if paged_session.is_paged() => {
                        let stamp = PagedStamp {
                            session_id: paged_session.id().to_owned(),
                            session_file: paged_session
                                .session_file()
                                .map(std::path::Path::to_path_buf),
                            leaf_id: paged_session.leaf_id().map(str::to_owned),
                        };
                        // The trigger is decided BEFORE the run so the
                        // lifecycle frames bracket real work: the pre-check is
                        // the same estimator+gate `compact_paged_inputs`
                        // applies over the SAME captured projection, so a
                        // threshold-not-met turn stays SILENT.
                        match compaction::paged::PagedCompactionInputs::capture(paged_session) {
                            Ok(inputs) if inputs.threshold_met(&model, &config) => {
                                (PagedPreparation::Inputs(inputs), Some(stamp))
                            }
                            Ok(_) => (PagedPreparation::Skip, Some(stamp)),
                            Err(compaction::PagedCompactionError::Unsupported(reason)) => {
                                (PagedPreparation::Failed(reason), Some(stamp))
                            }
                            Err(compaction::PagedCompactionError::ThresholdNotMet) => {
                                (PagedPreparation::Skip, Some(stamp))
                            }
                            Err(compaction::PagedCompactionError::Failed(error)) => {
                                (PagedPreparation::Failed(error), Some(stamp))
                            }
                        }
                    }
                    _ => (PagedPreparation::Resident, None),
                }
            };
            let paged_outcome = match paged_preparation {
                PagedPreparation::Inputs(inputs) => {
                    emit_compaction_event(&session_events, compaction_start_frame());
                    paged_started = true;
                    match with_threshold_compaction_retry_events(
                        &session_events,
                        compaction::paged::compact_paged_inputs(
                            inputs,
                            &model,
                            &config,
                            Arc::clone(&complete),
                            None,
                        ),
                    )
                    .await
                    {
                        Ok(paged) => PagedOutcome::Compacted(Box::new(paged.result)),
                        Err(compaction::PagedCompactionError::ThresholdNotMet) => {
                            // Unreachable after a met pre-check (same captured
                            // projection); carried so the Skip arm can close
                            // the pair defensively and a start never dangles.
                            PagedOutcome::Skip
                        }
                        Err(compaction::PagedCompactionError::Unsupported(reason)) => {
                            // The prefix holds a `context_edit`: this attempt
                            // cannot summarize anything. Close its pair
                            // truthfully, then fall over to the widening
                            // fallback below, which runs its own gate + pair.
                            emit_compaction_event(&session_events, compaction_end_failure(reason));
                            PagedOutcome::Resident
                        }
                        Err(compaction::PagedCompactionError::Failed(error)) => {
                            PagedOutcome::Failed(error)
                        }
                    }
                }
                PagedPreparation::Skip => PagedOutcome::Skip,
                PagedPreparation::Failed(error) => PagedOutcome::Failed(error),
                PagedPreparation::Resident => PagedOutcome::Resident,
            };
            match paged_outcome {
                PagedOutcome::Compacted(compacted) => {
                    // Re-lock ONLY for the append. The summarizer ran with the
                    // lock released, so the session may have advanced (new
                    // persisted messages keep the captured leaf an ancestor of
                    // the new leaf: still valid) or changed identity/branch
                    // (stale: append nothing and report the failure).
                    let mut failure: Option<String> = None;
                    {
                        let mut guard = session.lock().await;
                        match guard.as_mut() {
                            Some(current) => {
                                if let Some(stamp) = paged_stamp.as_ref() {
                                    if !paged_compaction_target_valid(current, stamp) {
                                        failure = Some(
                                            "session changed during paged compaction; the compaction entry was not persisted"
                                                .to_owned(),
                                        );
                                    }
                                }
                                if failure.is_none() {
                                    if let Err(error) = current.append_compaction_with_usage(
                                        compacted.summary.clone(),
                                        compacted.first_kept_entry_id.as_deref(),
                                        compacted.tokens_before,
                                        Some(print_compaction_details(&compacted)),
                                        Some(compacted.usage),
                                        false,
                                    ) {
                                        // Storage did not take the compaction:
                                        // the context must stay the
                                        // pre-compaction one, the snapshot
                                        // fresh, and the next turn retries
                                        // (agent_session/compact.rs's
                                        // threshold persistence-failure
                                        // contract).
                                        eprintln!(
                                            "[session] paged compaction persistence failed: {error}"
                                        );
                                        failure = Some(error.to_string());
                                    }
                                }
                            }
                            None => {
                                failure = Some(
                                    "session storage was detached during paged compaction; the compaction entry was not persisted"
                                        .to_owned(),
                                );
                            }
                        }
                    }
                    if let Some(error) = failure {
                        report_threshold_compaction_failure(
                            &runtime,
                            &session_events,
                            &error,
                            paged_started,
                        )
                        .await;
                        return context;
                    }
                    // The rebuilt context is shorter than the prompt-start
                    // snapshot and the session leaf path now carries this
                    // compaction boundary: later turns of the run rebuild the
                    // metadata from the live session (see `snapshot_stale`).
                    snapshot_stale.store(true, std::sync::atomic::Ordering::SeqCst);
                    // A real compaction returns a NEW shared context (the
                    // loop's `Arc::ptr_eq` check fires `Compaction`). The
                    // lifecycle frame closes AFTER the entry is persisted and
                    // the rebuilt context exists (`estimatedTokensAfter`).
                    let rebuilt = Arc::new(compaction::rebuilt_context(&context, &compacted));
                    emit_compaction_event(
                        &session_events,
                        compaction_end_success(&compacted, &rebuilt.messages),
                    );
                    return rebuilt;
                }
                PagedOutcome::Skip => {
                    // Nothing ran: the trigger never fired, so the no-op stays
                    // silent (upstream emits no lifecycle events for a
                    // threshold that is not met). The impossible
                    // `ThresholdNotMet` after a met pre-check is closed
                    // defensively, so an emitted start can never dangle.
                    if paged_started {
                        emit_compaction_event(
                            &session_events,
                            compaction_end_failure("the paged threshold no longer holds"),
                        );
                    }
                    if let Some(full) = widen_paged_context(&session, &context).await {
                        return full;
                    }
                    return context;
                }
                PagedOutcome::Failed(error) => {
                    // `compaction_end` first, then the extension failure event
                    // (agent_session's catch order); only a run that started (a
                    // met pre-check) has a session-event frame to close. A
                    // pre-check projection failure reports through the
                    // extension event alone.
                    report_threshold_compaction_failure(
                        &runtime,
                        &session_events,
                        &error,
                        paged_started,
                    )
                    .await;
                    // Never send the resident tail window as if it were the
                    // whole history after a failed paged compaction.
                    if let Some(full) = widen_paged_context(&session, &context).await {
                        return full;
                    }
                    return context;
                }
                PagedOutcome::Resident => {
                    // A PAGED session reaching the resident pipeline (a prefix
                    // `context_edit`): the live context is a tail window, so
                    // widening first and running the pipeline over the FULL
                    // projection is the only correct fallback — this costs the
                    // resident transcript, which is exactly why the paged path
                    // exists.
                    let widened = {
                        let mut guard = session.lock().await;
                        match guard.as_mut() {
                            Some(paged_session) if paged_session.is_paged() => {
                                if let Err(error) = paged_session.widen_to_full() {
                                    eprintln!("[session] paged widening failed: {error}");
                                    None
                                } else {
                                    paged_session.build_session_projection().ok()
                                }
                            }
                            _ => None,
                        }
                    };
                    if let Some(projection) = widened {
                        let branch = {
                            let guard = session.lock().await;
                            guard
                                .as_ref()
                                .map(|session| session.branch_path_meta(None))
                                .unwrap_or_default()
                        };
                        // The widened fallback's OWN trigger gate (the resident
                        // pipeline's estimator+gate pair over the full
                        // projection): a run that does not trigger stays
                        // SILENT, exactly like the resident path below.
                        let metas =
                            compaction::ProjectionEntryMeta::from_projection(&projection.entries);
                        let refs =
                            compaction::ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
                        if !print_compaction_threshold_met(
                            &refs,
                            &branch,
                            &projection.messages,
                            &model,
                            &config,
                        ) {
                            return context;
                        }
                        // Nothing to compact (the keep-recent window covers the
                        // whole widened transcript) is a no-op, not a run: the
                        // same silent return the resident path takes (upstream's
                        // threshold flow checks the preparation before it emits
                        // `compaction_start`).
                        let prepared =
                            match compaction::prepare_compaction_refs(&refs, &branch, &config) {
                                Some(prepared) => prepared,
                                None => return context,
                            };
                        emit_compaction_event(&session_events, compaction_start_frame());
                        return match with_threshold_compaction_retry_events(
                            &session_events,
                            compaction::compact_with_preparation_refs(
                                prepared,
                                &model,
                                Arc::clone(&complete),
                                None,
                            ),
                        )
                        .await
                        {
                            Ok(compacted) => {
                                let mut persistence_error: Option<String> = None;
                                if let Some(session) = session.lock().await.as_mut() {
                                    if let Err(error) = session.append_compaction_with_usage(
                                        compacted.summary.clone(),
                                        compacted.first_kept_entry_id.as_deref(),
                                        compacted.tokens_before,
                                        Some(print_compaction_details(&compacted)),
                                        Some(compacted.usage),
                                        false,
                                    ) {
                                        // Same contract as the resident path:
                                        // no rebuilt context, no stale
                                        // snapshot — the next turn retries.
                                        eprintln!(
                                            "[session] widening compaction persistence failed: {error}"
                                        );
                                        persistence_error = Some(error.to_string());
                                    }
                                }
                                match persistence_error {
                                    Some(error) => {
                                        report_threshold_compaction_failure(
                                            &runtime,
                                            &session_events,
                                            &error,
                                            true,
                                        )
                                        .await;
                                        context
                                    }
                                    None => {
                                        // The widened session was compacted in
                                        // place: a prompt-start snapshot (taken
                                        // over the paged window) no longer
                                        // describes the rebuilt context.
                                        snapshot_stale
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                        let rebuilt = Arc::new(compaction::rebuilt_context(
                                            &context, &compacted,
                                        ));
                                        emit_compaction_event(
                                            &session_events,
                                            compaction_end_success(&compacted, &rebuilt.messages),
                                        );
                                        rebuilt
                                    }
                                }
                            }
                            Err(error) => {
                                // `compaction_end` first, then the extension
                                // `session_compact_failed` (agent_session's
                                // catch order); the run continues un-compacted
                                // with the original context, like
                                // agent_session/compact.rs.
                                report_threshold_compaction_failure(
                                    &runtime,
                                    &session_events,
                                    &error,
                                    true,
                                )
                                .await;
                                context
                            }
                        };
                    }
                }
            }
            // The trigger and the whole pipeline run over a BORROWED basis:
            // per-entry metadata zipped with the live context's own message
            // slices. Nothing is copied out of the context (the former shape
            // deep-cloned the snapshot, the branch and every newer message on
            // every threshold-triggering request), and a sub-threshold request
            // returns the SAME `Arc` (the loop detects the no-op compaction by
            // pointer identity).
            //
            // A compaction rebuilt this run's context ([summary, retained
            // tail]) and moved the session leaf onto the new compaction
            // entry, so the prompt-start snapshot no longer describes the
            // entries the live context's messages came from. Rebuild the
            // metadata from the LIVE session (metadata only — no second
            // materialization of the conversation, the same basis
            // `agent_session/compact.rs` uses once its snapshot is stale);
            // only a fresh snapshot (or a session-less run) uses the
            // captured one.
            let snapshot_fresh =
                projected.is_some() && !snapshot_stale.load(std::sync::atomic::Ordering::SeqCst);
            let mut owned_metas: Vec<compaction::ProjectionEntryMeta> = Vec::new();
            let mut owned_branch: Vec<compaction::PathEntryMeta> = Vec::new();
            if !snapshot_fresh {
                let guard = session.lock().await;
                if let Some(current) = guard.as_ref() {
                    owned_metas = current.projection_entry_meta();
                    owned_branch = current.branch_path_meta(None);
                }
            }
            let (metas, branch) = match (&projected, snapshot_fresh) {
                (Some((metas, branch)), true) => (metas.as_slice(), branch.as_slice()),
                _ => (owned_metas.as_slice(), owned_branch.as_slice()),
            };
            let refs: Vec<compaction::CutEntryRef<'_>> = if metas.is_empty() {
                context
                    .messages
                    .iter()
                    .map(compaction::CutEntryRef::from_message)
                    .collect()
            } else {
                compaction::ProjectionEntryMeta::cut_refs(metas, &context.messages)
            };
            // Upstream reads the last assistant usage from the live context:
            // an O(1) check when the caller is already over the window
            // (`should_compact` on the raw context). The gate is the shared
            // estimator+gate pair (`print_compaction_threshold_met`), so
            // threshold-unmet turns (and everything that follows) stay silent.
            if !print_compaction_threshold_met(&refs, branch, &context.messages, &model, &config) {
                return context;
            }
            let prepared = match compaction::prepare_compaction_refs(&refs, branch, &config) {
                Some(prepared) => prepared,
                None => return context,
            };
            // The trigger is known to fire and the cut plan exists: the
            // lifecycle pair brackets the summarizer run (upstream
            // `_runAutoCompaction("threshold")` emits `compaction_start` once
            // the preparation succeeded and one `compaction_end` on every
            // exit).
            emit_compaction_event(&session_events, compaction_start_frame());
            match with_threshold_compaction_retry_events(
                &session_events,
                compaction::compact_with_preparation_refs(prepared, &model, complete, None),
            )
            .await
            {
                Ok(compacted) => {
                    let mut persistence_error: Option<String> = None;
                    if let Some(session) = session.lock().await.as_mut() {
                        if let Err(error) = session.append_compaction_with_usage(
                            compacted.summary.clone(),
                            compacted.first_kept_entry_id.as_deref(),
                            compacted.tokens_before,
                            Some(print_compaction_details(&compacted)),
                            Some(compacted.usage),
                            false,
                        ) {
                            // Storage did not take the compaction: keep the
                            // pre-compaction context and the fresh snapshot so
                            // the next turn retries instead of sending a
                            // context whose summary/leaf boundary is not
                            // persisted (agent_session/compact.rs's threshold
                            // persistence-failure contract).
                            eprintln!("[session] compaction persistence failed: {error}");
                            persistence_error = Some(error.to_string());
                        }
                    }
                    match persistence_error {
                        Some(error) => {
                            report_threshold_compaction_failure(
                                &runtime,
                                &session_events,
                                &error,
                                true,
                            )
                            .await;
                            context
                        }
                        None => {
                            // The rebuilt context is shorter than the
                            // prompt-start snapshot and the session leaf path
                            // now carries this compaction boundary: the NEXT
                            // threshold check of this run rebuilds its
                            // metadata from the live session, never from the
                            // pre-compaction snapshot (a second compaction over
                            // stale counts/ids would cut at a pre-boundary
                            // entry or a `None` id and lose the newest kept
                            // messages).
                            snapshot_stale.store(true, std::sync::atomic::Ordering::SeqCst);
                            // A real compaction returns a NEW shared context
                            // (the loop's `Arc::ptr_eq` check fires
                            // `Compaction`).
                            let rebuilt =
                                Arc::new(compaction::rebuilt_context(&context, &compacted));
                            emit_compaction_event(
                                &session_events,
                                compaction_end_success(&compacted, &rebuilt.messages),
                            );
                            rebuilt
                        }
                    }
                }
                Err(error) => {
                    // `compaction_end` first, then the extension
                    // `session_compact_failed` (agent_session's catch order).
                    // The run continues un-compacted with the original
                    // context, like agent_session/compact.rs.
                    report_threshold_compaction_failure(&runtime, &session_events, &error, true)
                        .await;
                    context
                }
            }
        })
    })
}

/// Compaction entry `details` object: the file-operation lists persisted
/// alongside the summary (agent_session `compaction_details`).
fn print_compaction_details(result: &compaction::CompactionResult) -> serde_json::Value {
    serde_json::json!({
        "readFiles": result.read_files,
        "modifiedFiles": result.modified_files,
    })
}

// ---------------------------------------------------------------------------
// `@file` image attachments (upstream cli/file-processor.ts + main.ts
// prepareInitialMessage)
// ---------------------------------------------------------------------------

/// Process `@file` arguments into the upstream-shaped file text plus the
/// image attachments (upstream cli/file-processor.ts + main.ts
/// prepareInitialMessage): text files render as
/// `<file name="…">\n{content}\n</file>\n`, image files become the
/// `initialImages` attachments AND still contribute their upstream marker
/// line to the prompt — `<file name="…"></file>\n` when there are no
/// processing hints, otherwise `<file name="…">{hints}</file>\n`.
/// Arguments are processed in order, so mixed text/image positions match
/// upstream. Images go through `processImage(..., autoResizeImages: false)`
/// like upstream print mode: unsupported formats (BMP) are normalized to
/// PNG, but nothing is resized; a conversion failure renders the upstream
/// failure text and attaches nothing.
///
/// Each file is read once: a 4 KB head-sized `File::read` decides the MIME
/// type, and the already-read bytes are reused for the text block or
/// extended in place for an image (the previous code read the whole file for
/// the sniff and `process_file_arguments` then read it again). Missing files
/// are hard errors (upstream exits 1); empty files are skipped on the text
/// path; unreadable files surface the read error.
pub fn partition_file_args(
    file_args: &[String],
    cwd: &std::path::Path,
) -> crate::Result<(String, Vec<crate::ai::types::ImageContent>)> {
    use std::io::Read as _;

    let mut text = String::new();
    let mut images = Vec::new();
    for file_arg in file_args {
        let path = resolve_file_arg_path(file_arg, cwd);
        if !path.is_file() {
            return Err(anyhow::anyhow!("File not found: {}", path.display()));
        }
        let mut file = std::fs::File::open(&path)
            .map_err(|error| anyhow::anyhow!("Could not read file {}: {error}", path.display()))?;
        let size = file
            .metadata()
            .map_err(|error| anyhow::anyhow!("Could not stat file {}: {error}", path.display()))?
            .len();
        // Upstream skips empty files before the MIME sniff.
        if size == 0 {
            continue;
        }
        // Sniff from a small read instead of the full file; these bytes are
        // then reused for the text block or extended for the image payload.
        let mut head = [0u8; crate::coding_agent::tools::image::IMAGE_TYPE_SNIFF_BYTES];
        let mut filled = 0;
        while filled < head.len() {
            match file.read(&mut head[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "Could not read file {}: {error}",
                        path.display()
                    ))
                }
            }
        }
        let mut bytes = Vec::with_capacity(filled);
        bytes.extend_from_slice(&head[..filled]);
        file.read_to_end(&mut bytes)
            .map_err(|error| anyhow::anyhow!("Could not read file {}: {error}", path.display()))?;
        let sniff_head = &bytes[..bytes
            .len()
            .min(crate::coding_agent::tools::image::IMAGE_TYPE_SNIFF_BYTES)];
        match crate::coding_agent::tools::image::detect_supported_image_mime_type(sniff_head) {
            Some(mime_type) => append_file_image(&path, bytes, mime_type, &mut text, &mut images),
            None => append_file_text(&path, &bytes, &mut text),
        }
    }
    Ok((text, images))
}

/// Text half of `processFileArguments`: lossy UTF-8 with a BOM strip.
fn append_file_text(path: &std::path::Path, bytes: &[u8], text: &mut String) {
    let raw = String::from_utf8_lossy(bytes);
    let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    text.push_str(&format!(
        "<file name=\"{}\">\n{content}\n</file>\n",
        path.display()
    ));
}

/// Image half of `processFileArguments`: `processImage` with
/// `autoResizeImages: false` (upstream print mode) — normalize to a supported
/// inline format, never resize — then render the conversion hints or the
/// processing failure text exactly like upstream.
#[cfg(any(feature = "image-tools", feature = "tui"))]
fn append_file_image(
    path: &std::path::Path,
    bytes: Vec<u8>,
    mime_type: &'static str,
    text: &mut String,
    images: &mut Vec<crate::ai::types::ImageContent>,
) {
    use crate::coding_agent::tools::image::{process_image, DEFAULT_JPEG_QUALITY};

    // The formats `processImage` normalizes to pass through untouched under
    // `autoResizeImages: false`: upstream returns the original bytes without
    // decoding them (only a non-inline format such as BMP is converted).
    if matches!(
        mime_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        use base64::Engine as _;
        images.push(crate::ai::types::ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            mime_type: mime_type.to_owned(),
        });
        text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
        return;
    }
    // BMP (and any other sniffed non-inline format): `processImage` decodes
    // and re-encodes it as PNG. Unlimited limits express `autoResizeImages:
    // false`: the dimension/byte checks always pass, so no resize ladder runs
    // and only the conversion hint is produced.
    let processed = process_image(
        &bytes,
        mime_type,
        u32::MAX,
        u32::MAX,
        usize::MAX,
        DEFAULT_JPEG_QUALITY,
    );
    match processed {
        Ok(processed) => {
            if processed.hints.is_empty() {
                text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
            } else {
                text.push_str(&format!(
                    "<file name=\"{}\">{}</file>\n",
                    path.display(),
                    processed.hints.join("\n")
                ));
            }
            images.push(crate::ai::types::ImageContent {
                data: processed.data_base64,
                mime_type: processed.mime_type,
            });
        }
        Err(error) => {
            text.push_str(&format!(
                "<file name=\"{}\">{}</file>\n",
                path.display(),
                error.message()
            ));
        }
    }
}

/// Image half without the `image` crate: no normalize/decode is possible, so
/// keep the previous pass-through attachment (raw bytes).
#[cfg(not(any(feature = "image-tools", feature = "tui")))]
fn append_file_image(
    path: &std::path::Path,
    bytes: Vec<u8>,
    mime_type: &'static str,
    text: &mut String,
    images: &mut Vec<crate::ai::types::ImageContent>,
) {
    use base64::Engine as _;

    images.push(crate::ai::types::ImageContent {
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        mime_type: mime_type.to_owned(),
    });
    text.push_str(&format!("<file name=\"{}\"></file>\n", path.display()));
}

/// Resolve an `@file` argument against `~`/`~/` and the cwd (same shape as
/// cli.rs's private `expand_tilde` + cwd join).
fn resolve_file_arg_path(file_arg: &str, cwd: &std::path::Path) -> PathBuf {
    let path = if file_arg == "~" || file_arg.starts_with("~/") || file_arg.starts_with("~\\") {
        match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            Some(home) => PathBuf::from(home).join(
                file_arg
                    .trim_start_matches('~')
                    .trim_start_matches(['/', '\\']),
            ),
            None => PathBuf::from(file_arg),
        }
    } else {
        PathBuf::from(file_arg)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
