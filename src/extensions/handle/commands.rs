// The enum is constructed by the always-compiled mailbox and consumed by the
// extension actor. Without the `extensions` feature no actor exists, so the
// variants' payload fields are legitimately unread in that configuration; the
// conditional allow keeps the minimal build warning-free without hiding any
// dead code in the default (full) build.
#![cfg_attr(not(feature = "extensions"), allow(dead_code))]

use super::*;

/// One actor command. Each variant carries its own reply channel.
pub(crate) enum Command {
    /// Load extension files/directories (discovery + transpile + evaluate).
    Load {
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<anyhow::Result<LoadResult>>,
    },
    /// Dispatch an event to all registered handlers. The payload travels as
    /// an `Arc` with single ownership: the handle `std::mem::take`s the
    /// caller's value (no deep clone), the actor unwraps the `Arc` back, and
    /// the (possibly rewritten) payload is moved home in the reply.
    Emit {
        event: String,
        payload: Arc<JsonValue>,
        /// Dispatch epoch assigned by the handle. Side effects the JS
        /// handlers queue are tagged with it, so a caller-side timeout can
        /// discard the late completion instead of applying it to a later run.
        epoch: u64,
        /// Actor-side budget for this dispatch (the caller's budget): the
        /// tokio wait AND the QuickJS interrupt deadline, so an await on a
        /// slow host future is aborted when the caller stops waiting.
        timeout: Duration,
        reply: oneshot::Sender<anyhow::Result<(EmitOutcome, JsonValue)>>,
    },
    /// Dispatch one event through the reply-free read-only path. `payload` is
    /// the event payload as PRE-RENDERED JSON text, serialized by the caller
    /// on its own thread so a deep payload (the `session_tree` forest) never
    /// becomes a `serde_json::Value` on the reply side: the actor only moves
    /// the string and the JS side never builds a reply. The reply channel
    /// carries no value — it exists so the caller can await completion (and
    /// keep one dispatch in flight).
    EmitReadOnly {
        event: String,
        payload: String,
        /// Dispatch epoch assigned by the handle. It travels with the command
        /// so a CALLER-side timeout can mark the epoch abandoned (dropping the
        /// dispatch's late side effects) even while the actor is stuck in a
        /// long synchronous JS turn and never runs its own deadline.
        epoch: u64,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Execute an extension tool through its JS `execute`.
    CallTool {
        name: String,
        call_id: String,
        params: JsonValue,
        reply: oneshot::Sender<anyhow::Result<ToolResult>>,
    },
    /// Run a JS tool's `prepareLoadout` hook (upstream
    /// `ToolDefinition.prepareLoadout`); `None` when the tool has no hook.
    PrepareLoadout {
        name: String,
        loadout: JsonValue,
        reply: oneshot::Sender<Result<Option<JsonValue>, String>>,
    },
    /// Invoke one deferred `withSession` callback (`ctx.newSession/fork/
    /// switchSession` with `withSession`, upstream `ReplacedSessionContext`).
    /// `token` names the callback in the prelude's bounded table; the host
    /// pushes the replacement session's snapshot before sending this. The
    /// reply is the callback's outcome: `Ok(())`, or the error text.
    CallSessionCallback {
        token: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Run a JS `user_bash` handler's custom `BashOperations.exec`
    /// (upstream `UserBashEventResult.operations`, extensions/types.ts:
    /// 1466-1476). `token` names the exported closure in the prelude's state
    /// map; `request` is `{command, cwd, timeout?, env?}`. The reply is the
    /// `exec` promise's outcome: `{exitCode}` or `{error}`.
    CallBashOperations {
        token: String,
        request: JsonValue,
        reply: oneshot::Sender<Result<JsonValue, String>>,
    },
    /// Dispatch a slash command (first registration wins).
    RunSlash {
        name: String,
        args: String,
        reply: oneshot::Sender<anyhow::Result<CommandOutcome>>,
    },
    /// Invoke a registered keyboard shortcut by key combo.
    InvokeShortcut {
        key: String,
        reply: oneshot::Sender<anyhow::Result<CommandOutcome>>,
    },
    /// Re-run the extension loader in-process: clear registries, rebuild the
    /// QuickJS runtime, reload the last paths, then emit
    /// `session_shutdown {reason: "reload"}` (old runtime),
    /// `session_start {reason: "reload"}` and
    /// `resources_discover {reason: "reload"}` (new runtime).
    Reload {
        reply: oneshot::Sender<anyhow::Result<LoadResult>>,
    },
    /// Clear the abort flag and reset JS-side abort listeners (start of a
    /// fresh run).
    ClearAbort { reply: oneshot::Sender<()> },
    /// Evaluate a JS expression that must produce a JSON string (test probe).
    Eval {
        code: String,
        reply: oneshot::Sender<anyhow::Result<serde_json::Value>>,
    },
    /// Invoke one extension renderer callback (`__pir_callRenderer`) with a
    /// bounded timeout. The reply rides a std channel so the TUI loop can
    /// bound its wait with `recv_timeout` even when the actor is busy.
    InvokeRenderer {
        request: JsonValue,
        timeout: Duration,
        reply: std::sync::mpsc::Sender<anyhow::Result<RendererCallOutcome>>,
    },
    /// Fetch argument completions for a slash command from the JS
    /// `getArgumentCompletions` hook.
    GetArgCompletions {
        command: String,
        prefix: String,
        reply: oneshot::Sender<anyhow::Result<Vec<JsonValue>>>,
    },
    /// Consult the `ui.addAutocompleteProvider` suggesters with the
    /// serialized completion context.
    GetSuggestions {
        context_json: String,
        reply: oneshot::Sender<anyhow::Result<Vec<JsonValue>>>,
    },
    /// Stream one reserved-prefix JS hook's incremental events (the
    /// `__pir_stream:` `streamSimple` callback; upstream's live
    /// `AssistantMessageEventStream`). Unlike the one-shot commands there is
    /// no `reply` oneshot: each produced event travels through `events` as a
    /// [`crate::extensions::runtime::HookStreamItem`], and dropping the
    /// receiver is the cancel signal — the actor stops pulling and asks the
    /// JS side to close its session.
    CallHookStream {
        hook_key: String,
        payload: JsonValue,
        events: mpsc::Sender<crate::extensions::runtime::HookStreamItem>,
    },
    /// Tear the JS runtime down on the actor thread: emit
    /// `session_shutdown {reason: "quit"}`, invalidate and drop the runtime,
    /// then stop the actor loop so `pir-ext-actor` exits. Handled inline by
    /// the actor (not spawned), so it is not counted as a pending command.
    Shutdown {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
}

impl Command {
    /// Whether the handle counted this command in `RuntimeState::pending_commands`
    /// (and therefore whether the actor must release that slot when the command
    /// completes).
    ///
    /// The reply-free streaming event (`message_update` / `session_tree`) is
    /// deliberately NOT counted: it is informational and emitted once per
    /// provider delta, so letting each delta preempt extension background work
    /// would abort a long-running background continuation during normal
    /// streaming. `Shutdown` is not counted either: the actor consumes it
    /// inline, before any command task exists. Every other command whose
    /// caller waits on an outcome is counted (and joins the
    /// background-preemption rule).
    pub(crate) fn counts_pending(&self) -> bool {
        !matches!(
            self,
            Command::EmitReadOnly { .. } | Command::Shutdown { .. }
        )
    }
}
