//! RPC mode: headless operation of the agent over JSON lines on stdin/stdout
//! (upstream `packages/coding-agent` RPC mode, adapted to pir's session
//! facade).
//!
//! Protocol:
//! - Commands are JSON objects sent to stdin, one per line (LF-delimited,
//!   optional trailing `\r` is stripped).
//! - Responses are JSON objects with `type: "response"` carrying the command
//!   name, an optional correlation `id`, `success`, and `data`/`error`.
//!   Prompt and steer/follow-up responses carry `data.disposition`
//!   (upstream `PromptDisposition`/`QueuedInputDisposition`); a rejected
//!   prompt (empty text, no session) carries `data.disposition: "dropped"`
//!   in its error response for protocol symmetry.
//! - Agent events are streamed BARE at top level, one JSON object per line,
//!   exactly like upstream rpc-mode.ts (`output(toJsonEvent(event))`); the
//!   objects are the upstream session-event shapes (see
//!   [`crate::coding_agent::json::to_json_event`], the same serializer the
//!   JSON print mode uses — `message_update` lines carry the hoisted `usage`
//!   and the partial-stripped `assistantMessageEvent`, no cumulative
//!   `message` snapshot).
//!
//! Supported commands: `hello`, `list`, `create`, `attach`, `prompt` (async;
//! events stream back; a prompt arriving while the agent streams requires an
//! explicit `streamingBehavior` (`"steer"`/`"followUp"`) and is otherwise
//! rejected, upstream agent-session.ts:2014-2022), `steer`, `follow_up`,
//! `abort`, `get-state` (legacy alias, no payload), `get_state`, `compact`
//! (the full upstream `CompactionResult`, `usage`/`details` included),
//! `get_messages` (the complete `session.messages` view by default,
//! `view: "llm"` for the old context projection), `get_commands` (full
//! `sourceInfo`), and the upstream session-control surface, plus the pir-only
//! background tool-call members (upstream pi has no background-call concept,
//! `TOOL-BACKGROUND-SCOPE.md` §2.5): `background_calls` (in-flight snapshot
//! array), `abort_background` (`{inFlight}`, `callId` in the request) and
//! `abort_all_background` (cancelled count). Every input line is dispatched
//! on its own task (upstream `void handleInputLine(line)`, rpc-mode.ts:796):
//! a long-running command (bash, compaction, an in-flight prompt) never
//! blocks the read loop, so `abort`/`abort_bash`/`steer` stay reachable
//! while it runs; responses are correlated by their request id. Logs stay on
//! stderr; stdout carries only protocol lines.
//!
//! Extension UI: the attached session's extension runtime runs with
//! `has_ui: true`, so `ctx.ui.select/confirm/input/editor` produce
//! `Notification::DialogRequest`s. A poller drains presentation-class
//! notifications and forwards them as `extension_ui_request` protocol lines
//! (upstream rpc-mode.ts `createExtensionUIContext` + rpc-types.ts wire
//! shapes); the client answers with `extension_ui_response {id, ...}` frames
//! which resolve the pending dialog through
//! `ExtensionHandle::respond_dialog`. An unanswered dialog resolves `null`
//! after the runtime's dialog TTL (the pir headless fail-safe), so a client
//! without response support degrades instead of wedging the run.
//!
//! The stdout channel is bounded (backpressure: the run's event handler
//! blocks when the client cannot keep up, mirroring upstream's awaited
//! writes) and a dead writer ends the session instead of silently dropping
//! every further response.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use serde_json::{json, Map, Value};

use crate::agent_session::phone_child::{self, InlinePromptAction, InlinePromptImage};
use crate::agent_session::{AgentSession, CustomRunMessage};
use crate::ai::types::{ContentBlock, ImageContent, Message};
use crate::config::Config;
use crate::extensions::Notification;
use crate::session_manager::SessionManager;

/// Capacity of the bounded stdout channel. A healthy client never fills it;
/// a stalled one applies backpressure to the run instead of buffering
/// without limit.
const OUT_CHANNEL_CAPACITY: usize = 256;

/// Per-line input bound for the JSON-line protocol, mirroring the CBOR
/// framing limit style (`protocol::framing::DEFAULT_MAX_FRAME_LENGTH`,
/// also 16 MiB): a run of bytes without a newline can no longer grow the
/// reader's buffer without bound. An over-limit line is reported as a
/// `parse` error response and the session ends — an unterminated line
/// cannot be skipped safely.
const DEFAULT_MAX_LINE_LENGTH: usize = 16 * 1024 * 1024;

/// Largest number of input lines dispatched concurrently. Each line gets its
/// own task (a long `bash`/`compact`/prompt must not block `abort`), but a
/// client that pipelines thousands of lines without reading responses must
/// not create thousands of tasks: past this many in-flight commands the extra
/// line is answered with a structured error instead.
const MAX_CONCURRENT_COMMAND_DISPATCHES: usize = 16;

/// Whether one more command may be dispatched with `running` already in
/// flight. Used by the serve loop and pinned by
/// `rpc_command_dispatch_is_bounded`.
#[doc(hidden)]
pub fn command_dispatch_admits(running: usize) -> bool {
    running < MAX_CONCURRENT_COMMAND_DISPATCHES
}

/// The in-flight slot of one dispatched command: decrements the count on
/// drop, so a task that panics or is cancelled still frees its slot.
struct DispatchSlot(Arc<AtomicUsize>);

impl Drop for DispatchSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How often the extension-UI bridge polls the attached session's extension
/// runtime for new presentation-class notifications (dialogs, title,
/// editor, widgets). Upstream emits synchronously from the JS call; pir's
/// notification queue is pull-based, so the poll adds at most one interval
/// of latency.
const UI_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Idle ceiling for the extension-UI poll: an empty poll backs off by
/// doubling from [`UI_POLL_INTERVAL`] up to this, and any drained notification
/// snaps it back to the base. 500 ms keeps a dialog's latency invisible while
/// an idle session wakes twice a second instead of twenty times.
const UI_POLL_MAX_INTERVAL: Duration = Duration::from_millis(500);

/// The next extension-UI poll interval: `found` (the last poll drained at
/// least one notification) resets to the 50 ms base; an empty poll doubles the
/// current interval up to the 500 ms ceiling. Pure so the backoff policy can
/// be pinned without timing the poller.
#[doc(hidden)]
pub fn ui_poll_interval(current: Duration, found: bool) -> Duration {
    if found {
        UI_POLL_INTERVAL
    } else {
        current.saturating_mul(2).min(UI_POLL_MAX_INTERVAL)
    }
}

/// Sequence for fire-and-forget UI request ids (upstream uses fresh UUIDs;
/// the `ui-` prefix keeps them disjoint from the numeric dialog ids, so a
/// stray client reply to a fire-and-forget request can never be
/// misattributed to a pending dialog).
static UI_REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn fire_and_forget_ui_id() -> String {
    format!("ui-{}", UI_REQUEST_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// RAII guard for the RPC prompt streaming claim (`RpcServerState
/// .prompt_claim`): resets the claim when dropped — including when the
/// spawned prompt task panics or is cancelled mid-await, where a post-await
/// `store(false)` never runs and every later prompt would be rejected with
/// "agent is streaming" forever. The field is public so tests can exercise
/// the drop/unwind semantics against a standalone flag.
pub struct PromptClaimGuard(pub Arc<AtomicBool>);

impl Drop for PromptClaimGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// How one RPC `prompt` run starts. An `images` array (upstream
/// `RpcCommand.prompt.images`) starts an image-carrying normal turn; the
/// phone-child inline-image rewrite can also replace a plain text prompt with
/// an image-carrying turn or with a displayed custom message
/// (`register-phone-child-extension.ts`).
enum PromptRun {
    Text,
    WithImages(Vec<ContentBlock>),
    PhoneCustom(CustomRunMessage),
}

/// The `images` array of an RPC prompt (`[{data, mimeType}]`); malformed
/// entries are dropped (the reference `ImageContent` guard).
fn prompt_images(value: &Value) -> Vec<InlinePromptImage> {
    value
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .filter_map(|image| {
                    let data = image.get("data").and_then(Value::as_str)?;
                    let mime_type = image.get("mimeType").and_then(Value::as_str)?;
                    Some(InlinePromptImage::new(data, mime_type))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn inline_image_blocks(images: &[InlinePromptImage]) -> Vec<ContentBlock> {
    images
        .iter()
        .map(|image| {
            ContentBlock::Image(ImageContent {
                data: image.data.clone(),
                mime_type: image.mime_type.clone(),
            })
        })
        .collect()
}

/// Builds an [`AgentSession`] for a session file (or `None` for a fresh
/// session). The RPC host injects a factory so embedders/tests can supply
/// their own provider/model wiring.
pub type SessionFactory =
    Arc<dyn Fn(Option<SessionManager>) -> crate::Result<Arc<AgentSession>> + Send + Sync>;

// ---------------------------------------------------------------------------
// Extension UI bridge (upstream `RpcExtensionUIRequest` / `RpcExtensionUIResponse`)
// ---------------------------------------------------------------------------

/// Serialize one presentation-class extension notification as an upstream
/// `RpcExtensionUIRequest` wire object (rpc-types.ts:253-297). `None` marks
/// a notification RPC mode does not forward (working message, theme,
/// autocomplete — upstream no-ops these) or an unsupported dialog kind (the
/// caller cancels it fail-safe).
///
/// Divergence note: upstream declares `select` options as `string[]`, but
/// pir's `ctx.ui.select` accepts non-string options and the JS prelude
/// normalizes them to `{value, label}` entries — the wire carries that
/// normalized array so the client can echo the option `value` back.
fn extension_ui_request_value(notification: &Notification) -> Option<Value> {
    match notification {
        Notification::DialogRequest {
            id,
            kind,
            title,
            options,
        } => {
            let title = title
                .clone()
                .or_else(|| {
                    options
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            match kind.as_str() {
                "select" => Some(json!({
                    "type": "extension_ui_request",
                    "id": id.to_string(),
                    "method": "select",
                    "title": title,
                    "options": options.get("options").cloned().unwrap_or_else(|| json!([])),
                })),
                "confirm" => Some(json!({
                    "type": "extension_ui_request",
                    "id": id.to_string(),
                    "method": "confirm",
                    "title": title,
                    "message": options
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                })),
                "input" => {
                    let mut request = json!({
                        "type": "extension_ui_request",
                        "id": id.to_string(),
                        "method": "input",
                        "title": title,
                    });
                    if let Some(placeholder) = options.get("placeholder").and_then(Value::as_str) {
                        request["placeholder"] = json!(placeholder);
                    }
                    // Protocol parity: upstream echoes the extension's
                    // `opts.timeout` in the request (rpc-mode.ts select/input/
                    // confirm); pir enforces its runtime dialog TTL instead.
                    if let Some(timeout) = options.get("timeout") {
                        request["timeout"] = timeout.clone();
                    }
                    Some(request)
                }
                "editor" => {
                    let mut request = json!({
                        "type": "extension_ui_request",
                        "id": id.to_string(),
                        "method": "editor",
                        "title": title,
                    });
                    if let Some(prefill) = options.get("prefill").and_then(Value::as_str) {
                        request["prefill"] = json!(prefill);
                    }
                    Some(request)
                }
                _ => None,
            }
        }
        Notification::Notify { message, level } => Some(json!({
            "type": "extension_ui_request",
            "id": fire_and_forget_ui_id(),
            "method": "notify",
            "message": message,
            "notifyType": level,
        })),
        Notification::Status { key, text } => Some(json!({
            "type": "extension_ui_request",
            "id": fire_and_forget_ui_id(),
            "method": "setStatus",
            "statusKey": key,
            "statusText": text,
        })),
        Notification::Widget { id, content } => {
            // Upstream forwards only array content (widget lines) or a
            // removal (`undefined`); component factories are unsupported in
            // RPC mode.
            let lines = match content {
                Value::Array(lines) => json!(lines),
                Value::Null => Value::Null,
                _ => return None,
            };
            Some(json!({
                "type": "extension_ui_request",
                "id": fire_and_forget_ui_id(),
                "method": "setWidget",
                "widgetKey": id,
                "widgetLines": lines,
            }))
        }
        Notification::SetTitle(title) => Some(json!({
            "type": "extension_ui_request",
            "id": fire_and_forget_ui_id(),
            "method": "setTitle",
            "title": title,
        })),
        // `pasteToEditor` falls back to `setEditorText` upstream
        // (rpc-mode.ts pasteToEditor).
        Notification::EditorSetText(text) | Notification::EditorPaste(text) => Some(json!({
            "type": "extension_ui_request",
            "id": fire_and_forget_ui_id(),
            "method": "set_editor_text",
            "text": text,
        })),
        // Working message, theme, autocomplete and other TUI-only concerns
        // are no-ops upstream (rpc-mode.ts) and stay unforwarded here.
        _ => None,
    }
}

/// The dialog reply carried by an `extension_ui_response` frame (upstream
/// `{value|confirmed|cancelled}`): a `value` passes through, `confirmed`
/// becomes a boolean, everything else (including `cancelled: true` and a
/// bare ack) resolves `null`. The JS prelude maps `null` to `undefined`
/// (cancelled/absent) for select/input/editor and to a non-boolean (also
/// `undefined`) for confirm — the pir fail-safe answer.
fn extension_ui_response_value(response: &Value) -> Value {
    match response.get("value") {
        Some(value) if !value.is_null() => value.clone(),
        _ => match response.get("confirmed") {
            Some(Value::Bool(confirmed)) => Value::Bool(*confirmed),
            _ => Value::Null,
        },
    }
}

/// One extension-UI poll tick: take the presentation-class notifications
/// queued by the attached session's extension runtime and emit them as
/// `extension_ui_request` protocol lines. `Ok(true)` means at least one
/// notification was drained (the poller resets its backoff), `Ok(false)` an
/// empty queue; `Err(())` means the output writer died (the poller must
/// stop).
async fn drain_extension_ui_notifications(session: &AgentSession, out: &Out) -> Result<bool, ()> {
    let Some(runtime) = session.extensions_handle() else {
        return Ok(false);
    };
    if runtime.pending_notification_count() == 0 {
        return Ok(false);
    }
    let mut drained = false;
    for notification in runtime.take_notifications(Notification::is_presentation) {
        drained = true;
        match extension_ui_request_value(&notification) {
            Some(request) => {
                if out.send(request).await.is_err() {
                    return Err(());
                }
            }
            None => {
                // An unsupported dialog kind must not leave the JS
                // `ctx.ui` await wedged until the TTL: cancel it
                // immediately (the reply is `null`, the prelude's
                // cancelled/undefined answer). Everything else that is not
                // forwardable is dropped like upstream's RPC no-ops.
                if let Notification::DialogRequest { id, kind, .. } = notification {
                    eprintln!("pir rpc: cancelling unsupported dialog kind: {kind}");
                    runtime.respond_dialog(&id.to_string(), Value::Null);
                }
            }
        }
    }
    Ok(drained)
}

// ---------------------------------------------------------------------------
// Output plumbing
// ---------------------------------------------------------------------------

/// Handle to the stdout writer. All protocol lines are serialized through one
/// writer task behind a bounded channel (backpressure: senders await, so a
/// stalled client slows the run instead of growing memory without limit).
#[derive(Clone)]
struct Out {
    tx: tokio::sync::mpsc::Sender<Value>,
}

impl Out {
    /// Queue one protocol line, awaiting channel capacity (backpressure).
    /// Returns `Err` when the writer task is gone (the output side died or
    /// was closed): the caller must stop serving instead of silently
    /// dropping responses into a dead channel.
    async fn send(&self, value: Value) -> Result<(), String> {
        self.tx
            .send(value)
            .await
            .map_err(|_| "rpc output writer is closed".to_owned())
    }

    async fn response(
        &self,
        id: Option<&str>,
        command: &str,
        result: CommandOutcome,
    ) -> Result<(), String> {
        self.send(response_value(id, command, result)).await
    }

    /// [`Out::response`] without waiting for queue capacity: used by the
    /// serve loop's admission rejection, where blocking the read loop on a
    /// full output queue would be worse than dropping the rejection (the
    /// writer-alive watch arm still ends the session if the writer is gone).
    fn try_response(
        &self,
        id: Option<&str>,
        command: &str,
        result: CommandOutcome,
    ) -> Result<(), String> {
        self.tx
            .try_send(response_value(id, command, result))
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    "rpc output queue is full".to_owned()
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    "rpc output writer is closed".to_owned()
                }
            })
    }
}

/// The wire object of one response line (`type: "response"` + correlation id
/// + success/data or error).
fn response_value(id: Option<&str>, command: &str, result: CommandOutcome) -> Value {
    let mut object = Map::new();
    object.insert("type".into(), json!("response"));
    object.insert("command".into(), json!(command));
    if let Some(id) = id {
        object.insert("id".into(), json!(id));
    }
    match result {
        CommandOutcome::Ok(data) => {
            object.insert("success".into(), json!(true));
            if let Some(data) = data {
                object.insert("data".into(), data);
            }
        }
        CommandOutcome::Err(error) => {
            object.insert("success".into(), json!(false));
            object.insert("error".into(), json!(error));
        }
    }
    Value::Object(object)
}

/// Result of dispatching one command.
type CommandOutcome = Result<Option<Value>, String>;

/// Serialize a session tree WITHOUT recursing over its nesting depth.
///
/// `SessionManager::tree` returns a chain nested one `TreeNode` per entry, so
/// the derived `serde` serialization (and the chain's recursive `Drop`) grows
/// with the SESSION LENGTH: a long linear session overflows the calling
/// thread's stack (measured: a ~1400-entry fixture crashed a 2 MiB test
/// thread; a Windows main thread has less headroom still). This walks the tree
/// with an explicit worklist and reassembles the SAME JSON (`children` is the
/// only field touched; every node's other fields are still serialized by
/// serde). Consuming the tree by value also dismantles the chain as it goes,
/// so no nested chain is ever dropped recursively.
fn tree_to_value(tree: Vec<crate::session_manager::TreeNode>) -> Value {
    enum Work {
        // Boxed: `TreeNode` dwarfs the `Finish` payload (clippy's
        // `large_enum_variant`), and the worklist holds one per pending node.
        Node(Box<crate::session_manager::TreeNode>),
        Finish { node: Value, children: usize },
    }
    let mut work: Vec<Work> = tree
        .into_iter()
        .rev()
        .map(|node| Work::Node(Box::new(node)))
        .collect();
    let mut done: Vec<Value> = Vec::new();
    while let Some(item) = work.pop() {
        match item {
            Work::Node(node) => {
                let mut node = *node;
                let children = std::mem::take(&mut node.children);
                let value = serde_json::to_value(&node).unwrap_or(Value::Null);
                if children.is_empty() {
                    done.push(value);
                } else {
                    work.push(Work::Finish {
                        node: value,
                        children: children.len(),
                    });
                    // Reversed so the first child is processed (and pushed
                    // into `done`) first.
                    work.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|child| Work::Node(Box::new(child))),
                    );
                }
            }
            Work::Finish { mut node, children } => {
                let split = done.len().saturating_sub(children);
                let kids = done.split_off(split);
                if let Some(object) = node.as_object_mut() {
                    object.insert("children".to_owned(), Value::Array(kids));
                }
                done.push(node);
            }
        }
    }
    Value::Array(done)
}

/// Widen a tail-first session (only a byte/entry capped window resident,
/// `SessionManager::open_paged`) to its full resident transcript before an RPC
/// read that reports the WHOLE session: `get_entries`, `get_tree`,
/// `get_messages`, `get_fork_messages`, `get_session_stats`, `get_state` and
/// `compact`. Without this a paged session would answer a client with only its
/// newest window — a silently truncated history. A no-op (and free) for a
/// session that already holds everything, including every session `attach`
/// opened fully because it fit the page caps.
async fn ensure_full_session(session: &AgentSession) -> Result<(), String> {
    let Some(handle) = session.session_manager() else {
        return Ok(());
    };
    let mut manager = handle.lock().await;
    crate::coding_agent::session_resolve::ensure_full_transcript(&mut manager)
        .map(|_| ())
        .map_err(|error| format!("cannot read the full session: {error}"))
}

/// Whether an extension asked `session` to exit (`ctx.shutdown`): the
/// session-level latch (recorded when the session consumer drained
/// `Notification::ShutdownRequest`) OR a request still sitting in the
/// extension runtime's queue (not yet drained by the consumer — e.g. one
/// raised by an `agent_settled` handler right before the boundary check).
/// One-shot: a `true` consumes the request, so the host exits exactly once.
fn take_session_shutdown_request(session: &AgentSession) -> bool {
    if session.take_shutdown_request() {
        return true;
    }
    let Some(runtime) = session.extensions_handle() else {
        return false;
    };
    !runtime
        .take_notifications(|notification| matches!(notification, Notification::ShutdownRequest))
        .is_empty()
}

/// Drops a deep `Value` iteratively even when an early `?` in the writer path
/// returns (the normal drop glue would recurse once per session-tree level).
struct IterativeValue(Option<Value>);

impl Drop for IterativeValue {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            crate::session_manager::json_depth::drop_value_iteratively(value);
        }
    }
}

async fn writer_task<W>(
    mut rx: tokio::sync::mpsc::Receiver<Value>,
    mut output: W,
    writer_alive: tokio::sync::watch::Sender<bool>,
) -> crate::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let result = async {
        while let Some(value) = rx.recv().await {
            // Iterative text (byte-identical) instead of `serde_json::to_vec`:
            // a response can carry a session tree nested one level per entry
            // (`get_tree`), and serde_json's serializer recurses one frame per
            // level. The guard tears the value down iteratively on every path,
            // including a broken-pipe `?` return.
            let value = IterativeValue(Some(value));
            let Some(payload) = value.0.as_ref() else {
                continue;
            };
            let mut line = crate::session_manager::json_depth::write_value_iteratively(payload)
                .into_bytes();
            line.push(b'\n');
            output.write_all(&line).await?;
            output.flush().await?;
            drop(value);
        }
        Ok(())
    }
    .await;
    // Signal the serve loop that the writer is gone (output write failure):
    // a run still streaming must not continue into a closed pipe unnoticed.
    let _ = writer_alive.send(false);
    result
}

/// Start the stdout writer on its own OS thread with a dedicated
/// current-thread runtime and return the channel that reports its result.
///
/// The writer's `write_all` waits on the client: a client that stops reading
/// parks the future until the OS pipe drains. On the serving runtime that
/// would occupy one worker — on a 1-core device the ONLY worker — and the
/// stdin loop (and with it `abort`, teardown, and even serving the next
/// command) could never be polled again. The dedicated thread keeps the
/// write's backpressure on the writer alone. `tokio::spawn` is what this
/// replaces; `spawn_blocking` would be wrong too, because it would occupy a
/// blocking-pool slot for as long as a stalled client holds the pipe.
fn spawn_writer_thread<W>(
    rx: tokio::sync::mpsc::Receiver<Value>,
    output: W,
    writer_alive: tokio::sync::watch::Sender<bool>,
) -> tokio::sync::oneshot::Receiver<crate::Result<()>>
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("pir-rpc-writer".to_owned())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(writer_task(rx, output, writer_alive)),
                Err(error) => Err(anyhow::anyhow!("rpc writer runtime failed: {error}")),
            };
            let _ = done_tx.send(result);
        });
    if let Err(error) = spawned {
        // The closure (and with it the receiver) was dropped, so the channel
        // closes and the serve loop takes the dead-writer path; report the
        // spawn failure like a writer error.
        eprintln!("pir rpc: output writer thread failed: {error}");
    }
    done_rx
}

/// Run one prompt run on its own thread with a dedicated current-thread
/// runtime (never `spawn_blocking`: a run blocked on a stalled client would
/// hold a blocking-pool slot for its whole duration).
///
/// The run's event handler applies backpressure by blocking on the bounded
/// stdout bridge. If the run lived on a worker of the serving runtime, that
/// block would park the worker — on a 1-core device the only one — and the
/// stdin loop could no longer dispatch `abort` or notice teardown. A dedicated
/// thread confines the backpressure to the run.
fn spawn_prompt_run(future: impl std::future::Future<Output = ()> + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("pir-rpc-run".to_owned())
        // The prompt future nests the agent loop, tool scheduler and streaming
        // adapters; the 2 MiB default is not enough in debug builds (observed
        // as a stack overflow on the second prompt). Match the test harness's
        // RUST_MIN_STACK budget instead of leaving the production host with a
        // smaller stack than `cargo test`.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(crate::env_daemon::tokio_max_blocking_threads())
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(future),
                Err(error) => eprintln!("pir rpc: prompt runtime failed: {error}"),
            }
        });
    if let Err(error) = spawned {
        eprintln!("pir rpc: prompt run thread failed: {error}");
    }
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

/// The directory context a session was built with. `get_commands` enumerates
/// prompt templates and skills from THIS context (upstream
/// `session.resourceLoader.getSkills()` / `session.promptTemplates`), not from
/// whatever the process environment says at command time — a mid-process
/// agent-dir change must not make an attached session advertise another
/// directory's commands.
#[derive(Clone)]
struct SessionResourceEnv {
    cwd: PathBuf,
    agent_dir: PathBuf,
}

/// Live RPC server state: session directory plus the currently attached
/// session.
struct RpcServerState {
    session_dir: PathBuf,
    factory: SessionFactory,
    session: Arc<RwLock<Option<Arc<AgentSession>>>>,
    /// Wakes the serve loop when an extension's `ctx.shutdown()` ended the
    /// host (upstream `shutdownHandler` + `checkShutdownRequested`): the loop
    /// then exits through the normal teardown. `notify_one` stores a permit,
    /// so a request raised between read-loop iterations is never lost.
    shutdown_signal: Arc<tokio::sync::Notify>,
    /// The directory context of the attached session (see
    /// [`SessionResourceEnv`]), bound when the session is built or its storage
    /// is swapped. `None` = no session yet.
    resource_env: Arc<std::sync::RwLock<Option<SessionResourceEnv>>>,
    /// Streaming claim for the attached session, taken synchronously by the
    /// `prompt` handler before it responds: two rapid prompts can never both
    /// be acknowledged (`AgentSession::prompt` would only reject the second
    /// from inside its spawned task — too late for the response).
    prompt_claim: Arc<AtomicBool>,
}

impl RpcServerState {
    fn new(session_dir: PathBuf, factory: SessionFactory) -> Self {
        Self {
            session_dir,
            factory,
            session: Arc::new(RwLock::new(None)),
            shutdown_signal: Arc::new(tokio::sync::Notify::new()),
            resource_env: Arc::new(std::sync::RwLock::new(None)),
            prompt_claim: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Bind `get_commands`' discovery context to a session's cwd and the agent
    /// dir in effect when that session was built.
    fn bind_resource_env(&self, cwd: impl Into<PathBuf>) {
        let env = SessionResourceEnv {
            cwd: cwd.into(),
            agent_dir: crate::coding_agent::context::agent_dir(),
        };
        *self
            .resource_env
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(env);
    }

    /// Move an existing binding to a new cwd, KEEPING the agent dir it was
    /// bound with: a storage swap (`new_session`/`switch_session`/fork) is a
    /// session-identity change, not an agent-dir change, so it must not
    /// re-resolve the environment. Without a binding there is nothing to
    /// keep, so the live values are used.
    fn rebind_resource_cwd(&self, cwd: impl Into<PathBuf>) {
        let cwd = cwd.into();
        let mut slot = self
            .resource_env
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match slot.as_mut() {
            Some(env) => env.cwd = cwd,
            None => {
                *slot = Some(SessionResourceEnv {
                    cwd,
                    agent_dir: crate::coding_agent::context::agent_dir(),
                })
            }
        }
    }

    /// Dispatch one command. Returns `Err` when the response could not be
    /// delivered (the output writer died); the serve loop then ends instead of
    /// reading further commands into a closed channel.
    async fn dispatch(&self, value: Value, out: &Out) -> Result<(), String> {
        let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
        let command = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // Extension UI replies (upstream rpc-mode.ts `handleInputLine`):
        // correlate the id back to the pending dialog and resolve its
        // `ctx.ui` promise. Unknown ids / no attached runtime are silently
        // dropped (upstream ignores them too). Not a command: no response
        // frame.
        if command == "extension_ui_response" {
            let request_id = id.unwrap_or_default();
            let reply = extension_ui_response_value(&value);
            if let Ok(session) = self.attached() {
                if let Some(runtime) = session.extensions_handle() {
                    runtime.respond_dialog(&request_id, reply);
                } else {
                    eprintln!(
                        "pir rpc: extension_ui_response for {request_id} dropped: no extension runtime"
                    );
                }
            } else {
                eprintln!(
                    "pir rpc: extension_ui_response for {request_id} dropped: no attached session"
                );
            }
            return Ok(());
        }

        // `prompt` responds before the (async) run starts so its response line
        // precedes the event stream.
        if command == "prompt" {
            match self.handle_prompt(&value, out).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    // Upstream has no "dropped" disposition: a rejected prompt
                    // is a plain error response (rpc-mode.ts preflight).
                    return out.response(id.as_deref(), "prompt", Err(error)).await;
                }
            }
        }

        let outcome = match command.as_str() {
            "hello" => Ok(Some(json!({
                "version": crate::protocol::PROTOCOL_VERSION,
                "protocol": "pir-rpc-jsonl",
            }))),
            "list" => self.handle_list(),
            "create" => self.handle_create(&value, out),
            "attach" => self.handle_attach(&value, out),
            "steer" => self.handle_steer(&value),
            "follow_up" => self.handle_follow_up(&value),
            // Upstream `abort` answers with bare success (no data payload).
            "abort" => {
                self.attached()?.abort();
                Ok(None)
            }
            // Legacy kebab-case alias: bare success, no state snapshot.
            "get-state" => {
                self.attached()?;
                Ok(None)
            }
            "compact" => self.handle_compact(&value).await,
            // -----------------------------------------------------------------
            // Session-control surface (upstream rpc-mode.ts command list)
            // -----------------------------------------------------------------
            "clear_queue" => self.handle_clear_queue().await,
            "set_model" => self.handle_set_model(&value).await,
            "cycle_model" => self.handle_cycle_model().await,
            "get_available_models" => self.handle_available_models(),
            "set_thinking_level" => self.handle_set_thinking_level(&value).await,
            "cycle_thinking_level" => self.handle_cycle_thinking_level().await,
            "get_available_thinking_levels" => self.handle_available_thinking_levels(),
            "set_steering_mode" => self.handle_set_steering_mode(&value),
            "set_follow_up_mode" => self.handle_set_follow_up_mode(&value),
            "set_auto_compaction" => self.handle_set_auto_compaction(&value),
            "set_auto_retry" => self.handle_set_auto_retry(&value),
            "abort_retry" => self.handle_abort_retry(),
            "bash" => self.handle_bash(&value).await,
            "abort_bash" => self.handle_abort_bash(),
            "get_session_stats" => self.handle_session_stats().await,
            "get_state" => self.handle_get_state().await,
            "export_html" => self.handle_export_html(&value).await,
            "new_session" => self.handle_new_session(&value).await,
            "switch_session" => self.handle_switch_session(&value).await,
            "fork" => self.handle_fork(&value).await,
            "clone" => self.handle_clone().await,
            "get_fork_messages" => self.handle_get_fork_messages().await,
            "get_entries" => self.handle_get_entries(&value).await,
            "get_tree" => self.handle_get_tree().await,
            "get_last_assistant_text" => self.handle_last_assistant_text().await,
            "set_session_name" => self.handle_set_session_name(&value).await,
            "get_messages" => self.handle_get_messages(&value).await,
            "get_commands" => self.handle_get_commands(),
            // -----------------------------------------------------------------
            // Background tool-call surface (pir extension; NOT part of
            // upstream's rpc-mode.ts command list —
            // `TOOL-BACKGROUND-SCOPE.md` §2.5)
            // -----------------------------------------------------------------
            "background_calls" => self.handle_background_calls(),
            "abort_background" => self.handle_abort_background(&value),
            "abort_all_background" => self.handle_abort_all_background(),
            other => Err(format!("Unknown command: {other}")),
        };
        let response = out.response(id.as_deref(), &command, outcome).await;
        // `ctx.shutdown()` raised while idle ends the host after the command
        // has answered (upstream `checkShutdownRequested` after
        // `handleCommand`, rpc-mode.ts:746/789). While a run is still
        // streaming the request must NOT be consumed here: a command can
        // arrive mid-run (one task per input line), and taking the queued
        // request would let the serve loop tear the session down before the
        // run settles. The `agent_settled` observer above owns that case —
        // it is the only consumer while a run is in flight.
        if !self.run_in_flight() && self.take_extension_shutdown_request() {
            self.shutdown_signal.notify_one();
        }
        response
    }

    /// Whether a prompt run is in flight (or acknowledged and about to start).
    /// The command path uses it to leave a `ctx.shutdown()` request to the
    /// `agent_settled` observer while the run is streaming: consuming it here
    /// would end the host before the run settles. The synchronous
    /// `prompt_claim` covers the window between the `prompt` response and the
    /// spawned run task publishing `is_streaming`.
    fn run_in_flight(&self) -> bool {
        self.prompt_claim.load(Ordering::SeqCst)
            || self.attached().is_ok_and(|session| session.is_streaming())
    }

    /// Whether an extension asked `session` to exit: the session-level latch
    /// (recorded when the session consumer drained
    /// `Notification::ShutdownRequest`) or a still-queued request. One-shot: a
    /// `true` consumes the request.
    fn take_extension_shutdown_request(&self) -> bool {
        let Ok(session) = self.attached() else {
            return false;
        };
        take_session_shutdown_request(&session)
    }

    fn attached(&self) -> Result<Arc<AgentSession>, String> {
        self.session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "no attached session; send attach or create first".to_owned())
    }

    /// Steer a message into the attached session and answer with the REAL
    /// disposition the session reports (upstream `QueuedInputDisposition`:
    /// `"handled"` when delivered into a live run, `"queued"` when parked).
    fn handle_steer(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let disposition = session.steer(&message_text(value));
        Ok(Some(json!({ "disposition": disposition })))
    }

    /// Follow-up variant of [`Self::handle_steer`] (upstream `follow_up`).
    fn handle_follow_up(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let disposition = session.follow_up(&message_text(value));
        Ok(Some(json!({ "disposition": disposition })))
    }

    fn handle_list(&self) -> CommandOutcome {
        // Best-effort discovery like upstream `SessionManager.list`: only
        // files that parse as a pi session header are listed.
        let sessions =
            crate::session_manager::SessionManager::list_sessions(&self.session_dir, None)
                .into_iter()
                .map(|summary| {
                    json!({
                        "id": summary.id,
                        "path": summary.path.to_string_lossy(),
                        "cwd": summary.cwd,
                    })
                })
                .collect::<Vec<_>>();
        Ok(Some(json!({ "sessions": sessions })))
    }

    fn resolve_session_file(&self, name: &str) -> crate::Result<SessionManager> {
        // Tail-first like the TUI/print hosts (`open_session_tail_first`): a
        // huge session attaches with only a bounded tail window resident, and
        // every whole-session RPC read below widens on demand. A session that
        // fits the page caps — or a legacy file the paged open refuses — opens
        // fully, so behavior is unchanged for it.
        let as_path = Path::new(name);
        let path = if as_path.exists() {
            as_path.to_path_buf()
        } else {
            let with_extension = self.session_dir.join(format!("{name}.jsonl"));
            if !with_extension.exists() {
                anyhow::bail!("session not found: {name}");
            }
            with_extension
        };
        let manager = crate::coding_agent::session_resolve::open_session_tail_first(&path)?;
        // Upstream `assertSessionCwdExists` (session-cwd.ts:54-59): a stored
        // cwd that no longer exists is a hard error for the headless RPC host.
        if let Some(issue) = manager.missing_session_cwd() {
            return Err(anyhow::anyhow!(issue.error_message(&current_cwd_string())));
        }
        Ok(manager)
    }

    /// Construct the [`AgentSession`] via the factory, record it as the
    /// attached session, and return the attach/create payload.
    ///
    /// Subscribes the session-level event bridge (upstream rpc-mode.ts:351-
    /// 357 `session.subscribe((event) => output(toJsonEvent(event)))`): the
    /// events the agent-loop stream cannot carry (`queue_update`,
    /// `compaction_start`, `compaction_end`, `bash_execution_update`) are
    /// serialized by [`crate::coding_agent::json::to_session_event_json`] and
    /// streamed BARE on stdout like every other protocol line — including
    /// outside prompt runs (manual `compact`, idle steering, RPC `bash`).
    fn build_and_attach(
        &self,
        session: Option<SessionManager>,
        out: &Out,
    ) -> Result<Option<Value>, String> {
        // Replacing the session while a prompt run is in flight would let the
        // old run and the new session interleave frames on the same output.
        if self.run_in_flight() {
            return Err("agent is streaming; wait for agent_settled before attach/create".to_owned());
        }
        // The session's own cwd (the one its storage was opened with) is the
        // directory its prompt templates and skills come from; capture it
        // before the manager is moved into the factory.
        let session_cwd = session.as_ref().map(|manager| manager.cwd().to_owned());
        let agent_session = (self.factory)(session).map_err(|error| error.to_string())?;
        // Release the prompt claim on the session's idle edge, BEFORE the
        // `agent_settled` line is forwarded below. `agent_end` is emitted
        // while the run is still settling; a client that reacts to it may
        // send the next prompt immediately, and the claim must be free by the
        // time the terminal session event reaches that client. The run-task
        // guard remains the fallback for a run that fails before settling.
        {
            let prompt_claim = Arc::clone(&self.prompt_claim);
            agent_session.on_session_event(Arc::new(move |event| {
                if matches!(
                    event,
                    crate::agent_session::SessionEvent::AgentSettled { .. }
                ) {
                    prompt_claim.store(false, Ordering::SeqCst);
                }
            }));
        }
        // Session-event → stdout bridge: a bounded std channel (blocking send
        // = backpressure, mirroring the prompt event bridge) feeds a
        // forwarder thread that awaits the bounded stdout channel. The
        // bridge ends when the session is dropped or the writer dies.
        let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<Value>(OUT_CHANNEL_CAPACITY);
        let forwarder_out = out.clone();
        std::thread::spawn(move || {
            for line in line_rx {
                if forwarder_out.tx.blocking_send(line).is_err() {
                    break;
                }
            }
        });
        agent_session.on_session_event(Arc::new(move |event| {
            let line = crate::coding_agent::json::to_session_event_json(&event);
            if line_tx.send(line).is_err() {
                // The forwarder is gone (writer died): drop further events.
            }
        }));
        // Host-level shutdown (`ctx.shutdown`, upstream `shutdownHandler` +
        // `checkShutdownRequested` at `agent_settled`, rpc-mode.ts:345-358):
        // the session consumer recorded the request during the settling run,
        // so wake the serve loop once the run is over. The request never
        // aborts the run; the loop exits through the normal teardown.
        {
            // `Weak`: this handler lives in the session's own subscriber
            // list, so a strong handle would be a reference cycle and keep
            // the session (and its event-forwarder thread, which holds an
            // `Out` clone) alive past teardown. The prompt task that emits
            // `agent_settled` holds the strong reference, so the upgrade
            // succeeds at the only moment the check matters.
            let shutdown_session = Arc::downgrade(&agent_session);
            let shutdown_signal = Arc::clone(&self.shutdown_signal);
            agent_session.on_session_event(Arc::new(move |event| {
                if matches!(
                    event,
                    crate::agent_session::SessionEvent::AgentSettled { .. }
                ) {
                    if let Some(session) = shutdown_session.upgrade() {
                        if take_session_shutdown_request(&session) {
                            shutdown_signal.notify_one();
                        }
                    }
                }
            }));
        }
        let state = agent_session.state();
        let data = json!({
            "id": state.session_id,
            "path": state.session_path,
            "model": state.model,
        });
        *self
            .session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(agent_session);
        // Bind the command-discovery context to the session just built: the
        // cwd its storage carries, and the agent dir resolved NOW (the same
        // moment the factory resolved the session's configuration), so a
        // later agent-dir change cannot redirect `get_commands`.
        self.bind_resource_env(session_cwd.unwrap_or_else(current_cwd_string));
        Ok(Some(data))
    }

    fn handle_create(&self, value: &Value, out: &Out) -> CommandOutcome {
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(current_cwd_string);
        let session = SessionManager::create(&cwd, &self.session_dir)
            .map_err(|error| format!("cannot create session: {error}"))?;
        self.build_and_attach(Some(session), out).map_err(|error| {
            if std::env::var_os("PIR_DEBUG_RPC").is_some() {
                eprintln!("[rpc-debug] create attach failed: {error}");
            }
            error
        })
    }

    fn handle_attach(&self, value: &Value, out: &Out) -> CommandOutcome {
        let session = match value.get("session").and_then(Value::as_str) {
            Some(name) => self
                .resolve_session_file(name)
                .map_err(|error| error.to_string())?,
            None => SessionManager::continue_recent(&self.session_dir, &current_cwd_string())
                .map_err(|error| format!("cannot open recent session: {error}"))?,
        };
        self.build_and_attach(Some(session), out)
    }

    async fn handle_prompt(&self, value: &Value, out: &Out) -> Result<(), String> {
        if std::env::var_os("PIR_DEBUG_RPC").is_some() {
            eprintln!("[rpc-debug] prompt command");
        }
        let session = self.attached()?;
        let mut text = message_text(value);
        if text.is_empty() {
            return Err("prompt requires a non-empty 'text' field".to_owned());
        }
        // `images` is part of the upstream prompt contract (`RpcCommand.prompt`,
        // rpc-types.ts:22): they ride the turn's user message. A phone child
        // additionally uses them for the inline image-token rewrite below.
        let images = prompt_images(value);
        let mut run = PromptRun::Text;
        if session.is_phone_child() {
            if !images.is_empty() {
                match phone_child::plan_inline_prompt(&text, &images, session.is_streaming()) {
                    // No token, no image, or no token/image pair.
                    InlinePromptAction::Unchanged => {}
                    InlinePromptAction::Transform {
                        text: flattened,
                        images,
                    } => {
                        // Busy or a slash command: flatten the token text and
                        // run the normal turn. The ordered images ride it when
                        // this is an idle slash command; while busy the native
                        // steering/follow-up queues are text-only (images on a
                        // queued message are a pre-existing protocol limit).
                        text = flattened;
                        if !session.is_streaming() {
                            run = PromptRun::WithImages(inline_image_blocks(&images));
                        }
                    }
                    // Idle: the reference `pi.sendMessage(..., {triggerTurn:
                    // true})` — persist the displayed custom message and let
                    // it start the run.
                    InlinePromptAction::Custom(message) => run = PromptRun::PhoneCustom(message),
                }
            }
        } else if !images.is_empty() {
            // Plain RPC prompt: the images join the user message exactly like
            // the interactive/print image path.
            run = PromptRun::WithImages(inline_image_blocks(&images));
        }
        // The extension `input` event runs for every user prompt, before the
        // streaming queue decision and before model/auth validation (upstream
        // `prompt()`); its source is `"rpc"`. A `handled` outcome CONSUMES the
        // prompt: acknowledge `disposition: "handled"` and start no turn (and
        // queue nothing). The transform then rides into the run through
        // `prompt_with_prepared_input`, so the event is never emitted twice.
        // A phone-child custom message is not a user prompt (`sendMessage`
        // with `triggerTurn`), so it bypasses the event like upstream.
        let behavior = value
            .get("streamingBehavior")
            .filter(|behavior| !behavior.is_null());
        let (text, run) = match run {
            PromptRun::PhoneCustom(message) => (text, PromptRun::PhoneCustom(message)),
            run => {
                let input_images = match &run {
                    PromptRun::WithImages(images) => images.clone(),
                    _ => Vec::new(),
                };
                let streaming_behavior = if session.is_streaming() {
                    behavior.and_then(Value::as_str)
                } else {
                    None
                };
                match session
                    .prepare_prompt_input(&text, input_images, "rpc", streaming_behavior)
                    .await
                {
                    Some((text, images)) => {
                        let run = if images.is_empty() {
                            PromptRun::Text
                        } else {
                            PromptRun::WithImages(images)
                        };
                        (text, run)
                    }
                    None => {
                        let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
                        out.response(
                            id.as_deref(),
                            "prompt",
                            Ok(Some(json!({ "disposition": "handled" }))),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        };
        if std::env::var_os("PIR_DEBUG_RPC").is_some() {
            eprintln!(
                "[rpc-debug] prepared streaming={} claim={}",
                session.is_streaming(),
                self.prompt_claim.load(Ordering::SeqCst)
            );
        }
        if session.is_streaming() {
            // Upstream agent-session.ts:2012-2022: a prompt arriving while
            // the agent streams REQUIRES an explicit `streamingBehavior` —
            // without one the prompt is rejected ("Agent is already
            // processing. …"), never silently steered. `"followUp"` routes
            // into the follow-up queue, every other truthy value steers
            // (upstream's `if (=== "followUp") … else steer`); both answer
            // disposition "queued".
            let Some(behavior) = behavior else {
                return Err(
                    "Agent is already processing. Specify streamingBehavior ('steer' or \
                     'followUp') to queue the message."
                        .to_owned(),
                );
            };
            let disposition = if behavior.as_str() == Some("followUp") {
                session.follow_up(&text)
            } else {
                session.steer(&text)
            };
            let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
            return out
                .response(
                    id.as_deref(),
                    "prompt",
                    Ok(Some(json!({ "disposition": disposition }))),
                )
                .await;
        }
        // Reserve the streaming claim synchronously, before responding
        // (upstream checks `isStreaming` in its prompt preflight): two rapid
        // prompts can never both be acknowledged — the second gets an error
        // response instead of failing silently inside the spawned task.
        if self.prompt_claim.swap(true, Ordering::SeqCst) {
            return Err("agent is streaming; queue the message with steer instead".to_owned());
        }
        // The RAII claim guard is built HERE, before anything that can fail or
        // never be polled: moving it into the spawned run alone left the claim
        // taken forever when the response could not be queued (writer gone) or
        // the run's thread/runtime failed to start (the future is dropped
        // without ever being polled, so a guard built inside it never exists).
        let claim_guard = PromptClaimGuard(Arc::clone(&self.prompt_claim));
        // Acknowledge acceptance first so the response line precedes all
        // events emitted by the run; the disposition reports what happened to
        // the input (upstream `PromptDisposition`).
        let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
        let disposition = session.input_disposition();
        out.response(
            id.as_deref(),
            "prompt",
            Ok(Some(json!({ "disposition": disposition }))),
        )
        .await?;

        // Event bridge: the agent loop calls the handler synchronously, so a
        // bounded std channel (blocking send = backpressure on the loop,
        // mirroring upstream's awaited writes) feeds a forwarder thread that
        // awaits the bounded stdout channel. A dead writer ends the bridge;
        // the run then reports a terminal error line instead of streaming
        // into a closed pipe.
        let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<Value>(OUT_CHANNEL_CAPACITY);
        let degraded = Arc::new(AtomicBool::new(false));
        let forwarder_out = out.clone();
        let forwarder_degraded = Arc::clone(&degraded);
        std::thread::spawn(move || {
            for line in line_rx {
                if forwarder_out.tx.blocking_send(line).is_err() {
                    forwarder_degraded.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        let events: crate::agent_session::EventHandler = {
            let degraded = Arc::clone(&degraded);
            Arc::new(move |event| {
                // Upstream streams each session event BARE at top level, one
                // per line (rpc-mode.ts `output(toJsonEvent(event))`) through
                // the shared event serializer: `message_update` lines carry
                // the hoisted `usage` and the partial-stripped
                // `assistantMessageEvent` and NO cumulative `message`
                // snapshot (modes/json-event.ts), exactly like the JSON print
                // mode.
                let line = crate::coding_agent::json::to_json_event(&event);
                // Blocking send on the bounded bridge = backpressure. A
                // disconnected forwarder (dead writer) is recorded instead of
                // blocking the loop forever.
                if line_tx.send(line).is_err() {
                    degraded.store(true, Ordering::SeqCst);
                }
            })
        };
        let task_out = out.clone();
        let task_id = id.clone();
        if std::env::var_os("PIR_DEBUG_RPC").is_some() {
            eprintln!("[rpc-debug] spawning prompt run");
        }
        // A dedicated thread/runtime, not `tokio::spawn`: the run's event
        // bridge blocks when the client's stdout stalls, and that must never
        // park the serving runtime's only worker (see `spawn_prompt_run`).
        spawn_prompt_run(async move {
            // RAII claim guard: released exactly where the run settles — and
            // also on a panic or task cancellation, which the former
            // post-await `store(false)` missed, permanently wedging every
            // later prompt behind "agent is streaming". Constructed outside
            // this future (see above), so a future that is never polled still
            // releases the claim.
            let _claim_guard = claim_guard;
            if std::env::var_os("PIR_DEBUG_RPC").is_some() {
                eprintln!("[rpc-debug] prompt future started");
            }
            let result = {
                match run {
                    // The `input` event was already dispatched by
                    // `prepare_prompt_input` above (source "rpc"); the run
                    // consumes that transform instead of emitting again.
                    PromptRun::Text => {
                        session
                            .prompt_with_prepared_input(&text, Vec::new(), events)
                            .await
                    }
                    // Idle slash command with inline image tokens: the
                    // flattened text plus the ordered images.
                    PromptRun::WithImages(images) => {
                        session
                            .prompt_with_prepared_input(&text, images, events)
                            .await
                    }
                    // Idle inline image prompt: the displayed custom message
                    // starts the turn (`pi.sendMessage(..., {triggerTurn:
                    // true})`); the interleaved content is persisted as-is.
                    PromptRun::PhoneCustom(message) => {
                        session.run_custom_turn(message, events).await
                    }
                }
            };
            if degraded.load(Ordering::SeqCst) {
                eprintln!("pir rpc: prompt event stream ended: output writer closed");
                // Best-effort terminal error line for the interrupted run.
                let _ = task_out
                    .response(
                        task_id.as_deref(),
                        "prompt",
                        Err("rpc output writer closed mid-run; the event stream ended".to_owned()),
                    )
                    .await;
            }
            if let Err(error) = result {
                eprintln!("pir rpc: prompt failed: {error}");
                // The claim was taken but the run never started (an external
                // stream raced in): report the failure as a response frame
                // instead of swallowing it.
                let _ = task_out
                    .response(task_id.as_deref(), "prompt", Err(error.to_string()))
                    .await;
            }
        });
        Ok(())
    }

    async fn handle_compact(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        // A manual compaction summarizes the WHOLE transcript; a paged
        // session widens first instead of compacting its tail window.
        ensure_full_session(&session).await?;
        let instructions = value
            .get("customInstructions")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (result, estimated_tokens_after) = session
            .compact_with_estimated_tokens(instructions.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        // The full upstream `CompactionResult` (rpc-types.ts:185,
        // compaction.ts:105-114): `usage` is the summarizer response's usage
        // and `details` the persisted `CompactionDetails` file lists, exactly
        // as the `compaction_end` session event carries them. `firstKeptIndex`
        // stays as the pir-only addition (a client that resolves entry ids
        // through `get_entries` can also use the upstream `firstKeptEntryId`).
        //
        // Limitation: when a `session_before_compact` handler REPLACES the
        // compaction, its arbitrary `details` payload is reduced to the
        // `readFiles`/`modifiedFiles` lists at parse time
        // (`parse_extension_compaction`); pir's `CompactionResult` has no slot
        // for other extension data, so that part is not echoed back.
        let mut data =
            crate::agent_session::compaction_result_wire(&result, estimated_tokens_after);
        if let Some(object) = data.as_object_mut() {
            object.insert("firstKeptIndex".to_owned(), json!(result.first_kept_index));
        }
        Ok(Some(data))
    }

    // -------------------------------------------------------------------
    // Session-control commands (upstream rpc-mode.ts)
    // -------------------------------------------------------------------

    fn serialize_model(model: &crate::ai::types::Model) -> Value {
        serde_json::to_value(model).unwrap_or_else(|_| json!(model.id))
    }

    async fn handle_clear_queue(&self) -> CommandOutcome {
        let session = self.attached()?;
        let cleared = session.clear_queue().await;
        Ok(Some(
            serde_json::to_value(cleared).map_err(|error| error.to_string())?,
        ))
    }

    async fn handle_get_state(&self) -> CommandOutcome {
        let session = self.attached()?;
        // `messageCount` is a whole-session number; a paged session widens
        // (once — later calls are no-ops) so the client's count is complete.
        ensure_full_session(&session).await?;
        let snapshot = session.state();
        let state = json!({
            "model": Self::serialize_model(&session.model()),
            "thinkingLevel": session.thinking_level(),
            "isStreaming": snapshot.streaming,
            // Upstream reports `session.isCompacting` (rpc-mode.ts:453); pir's
            // facade flags every compaction run (manual/threshold/overflow).
            "isCompacting": session.is_compacting(),
            "steeringMode": session.steering_mode(),
            "followUpMode": session.follow_up_mode(),
            "sessionFile": snapshot.session_path,
            "sessionId": snapshot.session_id,
            "sessionName": session.session_name().await,
            "autoCompactionEnabled": session.auto_compaction_enabled(),
            "messageCount": session.message_count().await,
            "pendingMessageCount": session.pending_message_count(),
        });
        Ok(Some(state))
    }

    async fn handle_set_model(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let provider = value.get("provider").and_then(Value::as_str).unwrap_or("");
        let model_id = value.get("modelId").and_then(Value::as_str).unwrap_or("");
        let model = session
            .find_model(provider, model_id)
            .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
        session
            .set_model(model.clone())
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(Self::serialize_model(&model)))
    }

    async fn handle_cycle_model(&self) -> CommandOutcome {
        let session = self.attached()?;
        match session
            .cycle_model()
            .await
            .map_err(|error| error.to_string())?
        {
            Some(cycle) => Ok(Some(
                serde_json::to_value(cycle).map_err(|error| error.to_string())?,
            )),
            None => Ok(Some(Value::Null)),
        }
    }

    fn handle_available_models(&self) -> CommandOutcome {
        let session = self.attached()?;
        let models: Vec<Value> = session
            .available_models()
            .iter()
            .map(Self::serialize_model)
            .collect();
        Ok(Some(json!({ "models": models })))
    }

    async fn handle_set_thinking_level(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let level_text = value.get("level").and_then(Value::as_str).unwrap_or("");
        let level = crate::ai::providers::resolution::parse_thinking_level(level_text)
            .ok_or_else(|| format!("Invalid thinking level: {level_text}"))?;
        let applied = session.set_thinking_level(level).await;
        // Upstream answers with bare success (rpc-mode.ts:497-500); the
        // applied level is visible via get_state.
        let _ = applied;
        Ok(None)
    }

    async fn handle_cycle_thinking_level(&self) -> CommandOutcome {
        let session = self.attached()?;
        match session.cycle_thinking_level().await {
            Some(level) => Ok(Some(json!({ "level": level }))),
            None => Ok(Some(Value::Null)),
        }
    }

    fn handle_available_thinking_levels(&self) -> CommandOutcome {
        let session = self.attached()?;
        Ok(Some(
            json!({ "levels": session.available_thinking_levels() }),
        ))
    }

    fn handle_set_steering_mode(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        session.set_steering_mode(parse_queue_mode(value)?);
        Ok(None)
    }

    fn handle_set_follow_up_mode(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        session.set_follow_up_mode(parse_queue_mode(value)?);
        Ok(None)
    }

    fn handle_set_auto_compaction(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let enabled = value
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or("set_auto_compaction requires a boolean 'enabled' field")?;
        session.set_auto_compaction_enabled(enabled);
        Ok(None)
    }

    fn handle_set_auto_retry(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let enabled = value
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or("set_auto_retry requires a boolean 'enabled' field")?;
        session.set_auto_retry_enabled(enabled);
        Ok(None)
    }

    fn handle_abort_retry(&self) -> CommandOutcome {
        self.attached()?.abort_retry();
        Ok(None)
    }

    async fn handle_bash(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .ok_or("bash requires a 'command' field")?;
        let exclude_from_context = value
            .get("excludeFromContext")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Extension hook first (upstream `emitUserBash`): handlers on BOTH
        // plugin faces are consulted exactly once — the JS runtime at its
        // host-list slot and every native Rust plugin — and a handler that
        // executed the command short-circuits the local execution; its result
        // becomes the response and is recorded explicitly (upstream
        // rpc-mode.ts:569-573 `recordBashResult`). With neither face
        // configured the wrapper reports `Local` and the command runs below.
        let outcome = session
            .dispatch_user_bash_event(command, exclude_from_context)
            .await;
        match outcome {
            crate::agent_session::UserBashOutcome::Handled { output, is_error } => {
                session
                    .record_extension_bash_result(command, &output, is_error, exclude_from_context)
                    .await;
                return Ok(Some(json!({
                    "output": output,
                    "cancelled": false,
                    "truncated": false,
                    "isError": is_error,
                })));
            }
            crate::agent_session::UserBashOutcome::Operations { operations } => {
                // Upstream rpc-mode.ts:579-583 passes
                // `operations: eventResult?.operations` into `executeBash`,
                // whose local default is replaced by the extension's
                // `BashOperations.exec` (a JS closure over its own
                // transport). The session's executor runs those operations
                // through the extension runtime — streaming the same
                // `bash_execution_update` session events as a local command —
                // and RECORDS the result itself, so the response is its
                // `BashResult` and the result is not recorded twice here.
                let result = session
                    .execute_user_bash_operations(&operations, command, exclude_from_context, None)
                    .await;
                return Ok(Some(
                    serde_json::to_value(&result).map_err(|error| error.to_string())?,
                ));
            }
            crate::agent_session::UserBashOutcome::Aborted => {
                return Err("bash command aborted by an extension handler".to_owned());
            }
            crate::agent_session::UserBashOutcome::Local => {}
        }
        // Upstream rpc-mode.ts:577-586 passes the RPC request id as
        // `executeBash`'s `options.id`, so every streamed chunk emits
        // `{type: "bash_execution_update", id, delta}` on the event stream
        // (agent-session.ts:3862); pir forwards those through the
        // session-event bridge (see `build_and_attach`).
        let id = value.get("id").and_then(Value::as_str);
        let result = session
            .execute_bash_with_id(command, exclude_from_context, id, None)
            .await;
        Ok(Some(
            serde_json::to_value(result).map_err(|error| error.to_string())?,
        ))
    }

    fn handle_abort_bash(&self) -> CommandOutcome {
        self.attached()?.abort_bash();
        Ok(None)
    }

    /// In-flight background tool calls of the attached session
    /// (`TOOL-BACKGROUND-SCOPE.md` §2.5).
    ///
    /// Payload: one JSON array of `{callId, toolName, parentToolCallId?}`
    /// snapshots — an empty array when nothing is in flight. The key names
    /// are the ones the `background_tool_end` event uses, so a client can
    /// correlate a snapshot with the completion it later receives;
    /// `parentToolCallId` is omitted for model-issued calls (the
    /// `Option::is_none` omission style of the sibling event payloads).
    /// `BackgroundCallStatus` carries no `rename_all` of its own (and is
    /// owned by another module), so the wire keys are mapped explicitly here
    /// instead of serializing it wholesale.
    fn handle_background_calls(&self) -> CommandOutcome {
        let calls: Vec<Value> = self
            .attached()?
            .background_calls()
            .into_iter()
            .map(|call| {
                let mut entry = json!({ "callId": call.call_id, "toolName": call.tool_name });
                if let Some(parent) = call.parent_call_id {
                    entry["parentToolCallId"] = json!(parent);
                }
                entry
            })
            .collect();
        Ok(Some(Value::Array(calls)))
    }

    /// Abort one in-flight background tool call by its provider tool-call id
    /// (the `abort_bash` mirror, per call id).
    ///
    /// Payload: `{"inFlight": bool}` — `true` when the call was still in
    /// flight and its own abort signal has now been fired, `false` for an
    /// unknown or already settled id (a successful no-op). Unlike
    /// `abort_bash`'s bare success, the boolean tells the client whether the
    /// abort actually cancelled anything.
    fn handle_abort_background(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let call_id = value
            .get("callId")
            .and_then(Value::as_str)
            .ok_or("abort_background requires a 'callId' field")?;
        Ok(Some(
            json!({ "inFlight": session.abort_background(call_id) }),
        ))
    }

    /// Abort every in-flight background tool call of the attached session
    /// (the session-switch / teardown path of `TOOL-BACKGROUND-SCOPE.md`
    /// §2.5).
    ///
    /// Payload: the number of calls that were cancelled (`0` when none was in
    /// flight).
    fn handle_abort_all_background(&self) -> CommandOutcome {
        Ok(Some(json!(self.attached()?.abort_all_background())))
    }

    async fn handle_session_stats(&self) -> CommandOutcome {
        let session = self.attached()?;
        // Token/cost/message totals cover the whole session (including
        // history compaction removed); widening first keeps a paged session
        // from reporting only its tail window.
        ensure_full_session(&session).await?;
        let stats = session.session_stats().await;
        Ok(Some(
            serde_json::to_value(stats).map_err(|error| error.to_string())?,
        ))
    }

    #[cfg(feature = "export-html")]
    async fn handle_export_html(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let Some(path) = session.state().session_path else {
            return Err("cannot export an in-memory session (--no-session)".to_owned());
        };
        let output = value
            .get("outputPath")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from);
        let written = crate::coding_agent::export_html::export_session_to_html(
            std::path::Path::new(&path),
            output.as_deref(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Some(json!({ "path": written.display().to_string() })))
    }

    /// `export_html` without the `export-html` feature (an `rpc`-only library
    /// build): the command stays on the wire and answers the documented
    /// missing-feature error. The error is returned BEFORE `attached()`, so
    /// the text is pinnable without a session/provider and an unattached
    /// client gets the same answer as an attached one.
    #[cfg(not(feature = "export-html"))]
    async fn handle_export_html(&self, _value: &Value) -> CommandOutcome {
        Err(
            "`export_html` is unavailable: this build was built without the `export-html` feature"
                .to_owned(),
        )
    }

    async fn handle_new_session(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let cwd = session
            .with_session_manager(|sm| sm.cwd().to_owned())
            .await
            .unwrap_or_else(current_cwd_string);
        let mut new_session = SessionManager::create(&cwd, &self.session_dir)
            .map_err(|error| format!("new session failed: {error}"))?;
        // Optional `parentSession` (upstream `newSession({ parentSession })`):
        // re-issue the fresh header with the parent link recorded. The API is
        // always available on the attached session, so the field is honored.
        if let Some(parent) = value
            .get("parentSession")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            new_session
                .new_session(Some(crate::session_manager::NewSessionOptions {
                    id: None,
                    parent_session: Some(parent),
                }))
                .map_err(|error| format!("new session failed: {error}"))?;
        }
        if !session.replace_session_storage(new_session).await {
            return Err("no attached session storage; the run stays ephemeral".to_owned());
        }
        // The session now lives in a fresh file for `cwd`: commands are
        // discovered from that cwd from here on.
        self.rebind_resource_cwd(cwd);
        Ok(Some(json!({ "cancelled": false })))
    }

    async fn handle_switch_session(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let name = value
            .get("sessionPath")
            .and_then(Value::as_str)
            .ok_or("switch_session requires a 'sessionPath' field")?;
        let manager = self
            .resolve_session_file(name)
            .map_err(|error| error.to_string())?;
        let switched_cwd = manager.cwd().to_owned();
        if !session.replace_session_storage(manager).await {
            return Err("no attached session storage; the run stays ephemeral".to_owned());
        }
        // The swapped-in session carries its own cwd; `get_commands` follows
        // it.
        self.rebind_resource_cwd(switched_cwd);
        // Upstream `switchSession` answers with `{cancelled}` only.
        Ok(Some(json!({ "cancelled": false })))
    }

    async fn handle_fork(&self, value: &Value) -> CommandOutcome {
        // Upstream rpc-mode.ts:577-586: `runtimeHost.fork(command.entryId)`
        // → `fork(entryId, undefined)` → position defaults to "before"
        // (agent-session-runtime.ts:263-289): the entry must be a user
        // message, the fork branches at its PARENT (a fresh session with a
        // `parentSession` link when there is none), and the response carries
        // `text: result.selectedText` / `cancelled`. pir additionally accepts
        // `position` ("at" branches at the entry itself) for the
        // `runtimeHost.fork(entryId, {position})` surface. Upstream's
        // `session_before_fork` dispatch (a cancel aborts the fork) runs
        // inside `AgentSession::fork_entry`.
        let session = self.attached()?;
        match value.get("entryId").and_then(Value::as_str) {
            Some(entry_id) => {
                let position = crate::agent_session::ForkPosition::parse(
                    value.get("position").and_then(Value::as_str),
                );
                match session.fork_entry(entry_id, position).await {
                    Ok(crate::agent_session::ForkEntryOutcome::Branched { selected_text }) => {
                        let mut data = json!({ "cancelled": false });
                        if let Some(text) = selected_text {
                            data["text"] = json!(text);
                        }
                        Ok(Some(data))
                    }
                    Ok(crate::agent_session::ForkEntryOutcome::Cancelled) => {
                        Ok(Some(json!({ "cancelled": true })))
                    }
                    Err(error) => Err(error),
                }
            }
            None => {
                self.fork_current("fork").await?;
                Ok(Some(json!({ "text": "", "cancelled": false })))
            }
        }
    }

    async fn handle_clone(&self) -> CommandOutcome {
        self.fork_current("clone").await?;
        // Upstream `clone` answers with `{cancelled}` only.
        Ok(Some(json!({ "cancelled": false })))
    }

    /// Shared `fork`/`clone` body (upstream forks from a selected entry; pir
    /// duplicates the session at the current position). The response payload
    /// is chosen by the caller ({text,cancelled} vs {cancelled}).
    async fn fork_current(&self, label: &str) -> Result<(), String> {
        let session = self.attached()?;
        let Some(path) = session.state().session_path else {
            return Err(format!(
                "cannot {label} an in-memory session (--no-session)"
            ));
        };
        let cwd = session
            .with_session_manager(|sm| sm.cwd().to_owned())
            .await
            .unwrap_or_else(current_cwd_string);
        let forked = SessionManager::fork_from_source(
            std::path::Path::new(&path),
            &cwd,
            &self.session_dir,
            None,
        )
        .map_err(|error| format!("{label} failed: {error}"))?;
        if !session.replace_session_storage(forked).await {
            return Err("no attached session storage; the run stays ephemeral".to_owned());
        }
        // The forked storage keeps this cwd; command discovery follows it.
        self.rebind_resource_cwd(cwd);
        Ok(())
    }

    async fn handle_get_entries(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        // Whole-session read: a paged session widens first, so `since` can
        // name an entry in the skipped prefix and the client never receives a
        // truncated entry list.
        ensure_full_session(&session).await?;
        let Some(handle) = session.session_manager() else {
            return Err("no attached session storage".to_owned());
        };
        let manager = handle.lock().await;
        let mut entries: Vec<Value> = manager
            .entries()
            .iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect();
        if let Some(since) = value.get("since").and_then(Value::as_str) {
            let index = entries
                .iter()
                .position(|entry| entry.get("id").and_then(Value::as_str) == Some(since))
                .ok_or_else(|| format!("Entry not found: {since}"))?;
            entries.drain(..=index);
        }
        let leaf = manager.leaf_id().map(str::to_owned);
        Ok(Some(json!({ "entries": entries, "leafId": leaf })))
    }

    async fn handle_get_tree(&self) -> CommandOutcome {
        let session = self.attached()?;
        // Whole-session read (see `ensure_full_session`): the tree the client
        // walks must cover the prefix, and a later tree navigation/entry read
        // resolves ids through the resident index.
        ensure_full_session(&session).await?;
        let Some(handle) = session.session_manager() else {
            return Err("no attached session storage".to_owned());
        };
        let manager = handle.lock().await;
        // Iterative serialization: the tree nests one node per entry, and the
        // recursive `serde` path would overflow on a long session (see
        // `tree_to_value`). The response object is built with explicit inserts
        // because `json!` routes a `Value` expression through
        // `serde_json::to_value`, i.e. a recursive deep clone of the tree; the
        // writer task serializes and tears the value down iteratively.
        let tree = tree_to_value(manager.tree());
        let leaf = manager.leaf_id().map(str::to_owned);
        let mut data = serde_json::Map::new();
        data.insert(String::from("tree"), tree);
        data.insert(
            String::from("leafId"),
            leaf.map_or(Value::Null, Value::String),
        );
        Ok(Some(Value::Object(data)))
    }

    async fn handle_last_assistant_text(&self) -> CommandOutcome {
        let session = self.attached()?;
        // Reads the transcript: widen a paged session first (a tail window
        // whose newest entry is not an assistant message would answer `null`
        // for a session that HAS one in the prefix).
        ensure_full_session(&session).await?;
        let text = session.last_assistant_text().await;
        Ok(Some(json!({ "text": text })))
    }

    async fn handle_set_session_name(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if name.is_empty() {
            return Err("Session name cannot be empty".to_owned());
        }
        session
            .set_session_name(&name)
            .await
            .map_err(|error| error.to_string())?;
        Ok(None)
    }

    /// Upstream `get_messages` (`{ messages: session.messages }`).
    ///
    /// The default view is the COMPLETE message list
    /// ([`AgentSession::full_messages_json`], upstream `session.messages`):
    /// custom/`bashExecution` records, extension `custom_message` entries and
    /// compaction/branch summary records in their own roles. A client that
    /// wants the LLM-flattened projection (custom records rendered as user
    /// text, summaries framed, `!!` records omitted) passes `"view": "llm"`;
    /// that is exactly what `get_messages` answered before this change, so it
    /// stays reachable without a new command.
    async fn handle_get_messages(&self, value: &Value) -> CommandOutcome {
        let session = self.attached()?;
        // The read covers the resident entries: widen a paged session so the
        // client receives the whole conversation.
        ensure_full_session(&session).await?;
        match value.get("view").and_then(Value::as_str).unwrap_or("full") {
            "full" => Ok(Some(
                json!({ "messages": session.full_messages_json().await }),
            )),
            "llm" => Ok(Some(json!({ "messages": session.transcript_json().await }))),
            other => Err(format!(
                "invalid 'view' value: {other} (expected \"full\" or \"llm\")"
            )),
        }
    }

    /// Upstream `getUserMessagesForForking`: the user text messages on the
    /// current branch, each as `{ entryId, text }`.
    async fn handle_get_fork_messages(&self) -> CommandOutcome {
        let session = self.attached()?;
        // The branch covers the whole session, not just its tail window.
        ensure_full_session(&session).await?;
        let messages = session
            .with_session_manager(|sm| {
                let entries = sm.build_context_entries_with_ids().unwrap_or_default();
                entries
                    .into_iter()
                    .filter_map(|(entry_id, message)| {
                        let entry_id = entry_id?;
                        let Message::User(user) = message else {
                            return None;
                        };
                        let text = user
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Some(json!({ "entryId": entry_id, "text": text }))
                    })
                    .collect::<Vec<Value>>()
            })
            .await
            .unwrap_or_default();
        Ok(Some(json!({ "messages": messages })))
    }

    /// Upstream `get_commands`: extension commands from the attached session's
    /// runtime, then prompt templates and skills discovered for the cwd
    /// (invocable via prompt).
    ///
    /// Every entry carries the full upstream `RpcSlashCommand.sourceInfo`
    /// (`rpc-types.ts:81-90`): `path` plus the `source`/`scope`/`origin`/
    /// `baseDir` provenance pir can establish from the resource's location,
    /// inferred exactly like upstream `resource-loader.ts`
    /// `getDefaultSourceInfoForPath`. Fields a resource genuinely lacks (an
    /// extension whose JS registry exported only a path) are filled from that
    /// path; nothing is invented for a resource with no path at all.
    fn handle_get_commands(&self) -> CommandOutcome {
        let mut commands: Vec<Value> = Vec::new();
        // The discovery context is the one bound when this session was
        // built/attached — its cwd and the agent dir of that moment — not the
        // live process environment, so an agent-dir change cannot make an
        // attached session report another directory's commands. It is also
        // what sourceInfo provenance is resolved against.
        let resource_env = self
            .resource_env
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (cwd, agent_dir) = match resource_env {
            Some(env) => (env.cwd, env.agent_dir),
            // No attached session: the live values are all there is.
            None => (
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                crate::coding_agent::context::agent_dir(),
            ),
        };
        if let Ok(session) = self.attached() {
            if let Some(runtime) = session.extensions_handle() {
                for command in runtime.command_defs() {
                    let mut entry = json!({
                        "name": command.name,
                        "description": command.description,
                        "source": "extension",
                    });
                    if !command.source_info.path.is_empty() {
                        let info = command
                            .source_info
                            .clone()
                            .filled_from_path(&cwd, &agent_dir);
                        entry["sourceInfo"] = source_info_value(&info);
                    }
                    commands.push(entry);
                }
            }
        }
        let resources = crate::coding_agent::resources::DiscoveredResources::discover(
            &crate::coding_agent::resources::ResourceDiscoveryOptions {
                cwd: &cwd,
                agent_dir: &agent_dir,
                project_trusted: crate::config::trust::resolve_cwd_trust(&cwd, &agent_dir),
                cli_skill_paths: &[],
                cli_prompt_paths: &[],
                no_skills: false,
                no_prompt_templates: false,
            },
        );
        // Resource-load diagnostics (collisions, malformed skills/templates)
        // go to stderr: stdout carries only protocol lines (upstream
        // `rpc-mode.ts` keeps the JSON-line stream clean). Non-fatal.
        for diagnostic in &resources.diagnostics {
            eprintln!("pir: {diagnostic}");
        }
        for template in &resources.prompt_templates {
            let info = crate::extensions::SourceInfo::for_resource_path(
                &template.file_path,
                &cwd,
                &agent_dir,
            );
            commands.push(json!({
                "name": template.name,
                "description": template.description,
                "source": "prompt",
                "sourceInfo": source_info_value(&info),
            }));
        }
        for skill in &resources.skills {
            // Upstream skill source info uses the skill's own directory as
            // `baseDir` (`createSkillSourceInfo(..., baseDir: skillDir)`) while
            // the scope still comes from the containing root.
            let mut info =
                crate::extensions::SourceInfo::for_resource_path(&skill.path, &cwd, &agent_dir);
            if let Some(parent) = skill.path.parent() {
                info.base_dir = Some(parent.to_string_lossy().into_owned());
            }
            commands.push(json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
                "sourceInfo": source_info_value(&info),
            }));
        }
        Ok(Some(json!({ "commands": commands })))
    }
}

/// Prompt/steer message text: `text` first, `message` as an alias.
fn message_text(value: &Value) -> String {
    value
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn current_cwd_string() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_owned())
}

/// Materialize the CLI's startup session plan for the RPC host.
///
/// `Ok(None)` is the ephemeral (`--no-session`) run: the host still attaches
/// an in-memory session (upstream `SessionManager.inMemory`), it just has no
/// storage. Every other plan opens/creates/fork its storage and passes the
/// same headless cwd guard as `attach` (`assertSessionCwdExists`,
/// session-cwd.ts:54-59) before the host starts serving.
fn open_startup_session(
    plan: &crate::coding_agent::session_resolve::SessionPlan,
    session_dir: &Path,
) -> crate::Result<Option<SessionManager>> {
    if matches!(
        plan,
        crate::coding_agent::session_resolve::SessionPlan::Ephemeral { .. }
    ) {
        return Ok(None);
    }
    let cwd = current_cwd_string();
    let manager =
        crate::coding_agent::session_resolve::open_session_manager(plan, &cwd, session_dir)?;
    if let Some(manager) = &manager {
        if let Some(issue) = manager.missing_session_cwd() {
            return Err(anyhow::anyhow!(issue.error_message(&cwd)));
        }
    }
    Ok(manager)
}

// ---------------------------------------------------------------------------
// get_commands sourceInfo (upstream `SourceInfo` / `getDefaultSourceInfoForPath`)
// ---------------------------------------------------------------------------

/// `SourceInfo` wire object: `path` always, the provenance fields only when
/// known (see `SourceInfo`'s serde attributes).
fn source_info_value(info: &crate::extensions::SourceInfo) -> Value {
    serde_json::to_value(info).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Why reading one JSON line stopped.
enum LineReadError {
    /// Underlying I/O failure (including invalid UTF-8, matching the
    /// previous `lines()` behavior of failing the read).
    Io(std::io::Error),
    /// More than the configured limit of bytes arrived without a newline.
    TooLong,
}

/// Read one LF-delimited line, bounded: at most `limit` bytes may arrive
/// before the newline, so a peer streaming newline-free bytes cannot grow
/// the buffer without bound. Returns `Ok(None)` at a clean EOF; a final
/// unterminated line is returned like the previous `lines()` did. Buffered
/// reads (`fill_buf`) keep this O(total bytes), not byte-at-a-time.
async fn read_json_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<String>, LineReadError> {
    let decode = |bytes: Vec<u8>| {
        String::from_utf8(bytes).map(Some).map_err(|error| {
            LineReadError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })
    };
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(LineReadError::Io)?;
        if available.is_empty() {
            // EOF: a partial final line is still delivered, an empty buffer
            // means the stream ended cleanly.
            return if bytes.is_empty() {
                Ok(None)
            } else {
                decode(bytes)
            };
        }
        match available.iter().position(|&byte| byte == b'\n') {
            Some(newline) => {
                if bytes.len() + newline > limit {
                    return Err(LineReadError::TooLong);
                }
                bytes.extend_from_slice(&available[..newline]);
                reader.consume(newline + 1);
                return decode(bytes);
            }
            None => {
                if bytes.len() + available.len() > limit {
                    return Err(LineReadError::TooLong);
                }
                bytes.extend_from_slice(available);
                let buffered = available.len();
                reader.consume(buffered);
            }
        }
    }
}

/// JSON-line RPC server over any async reader/writer pair (testable).
pub struct RpcServer {
    session_dir: PathBuf,
    factory: SessionFactory,
    /// The session the host attaches at startup (upstream `main.ts:950-952`
    /// hands `runRpcMode` the session the CLI already created/opened). `None`
    /// keeps the library-embedder behavior: no session until `attach`/`create`.
    startup_session: Option<crate::coding_agent::session_resolve::SessionPlan>,
}

impl RpcServer {
    /// Build a server rooted at `session_dir`, using `factory` to construct
    /// attached [`AgentSession`]s. No startup session is attached.
    pub fn new(session_dir: PathBuf, factory: SessionFactory) -> Self {
        Self {
            session_dir,
            factory,
            startup_session: None,
        }
    }

    /// Attach `plan`'s session BEFORE the first protocol line is read
    /// (upstream `main.ts:950-952` -> `modes/rpc/rpc-mode.ts:54-57`): the
    /// session the CLI selected with `--session`/`--resume`/`--fork`/
    /// `--session-id`, or the fresh session the CLI planned, is the attached
    /// session from the start. Clients no longer need a preliminary
    /// `attach`/`create`; an explicit `attach`/`create` still replaces it
    /// exactly as before.
    pub fn with_startup_session(
        mut self,
        plan: crate::coding_agent::session_resolve::SessionPlan,
    ) -> Self {
        self.startup_session = Some(plan);
        self
    }

    /// Serve the JSON-line protocol until `input` closes or the output writer
    /// dies.
    pub async fn serve<R, W>(&self, input: R, output: W) -> crate::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::channel::<Value>(OUT_CHANNEL_CAPACITY);
        let (writer_alive_tx, mut writer_alive_rx) = tokio::sync::watch::channel(true);
        // Off the serving runtime's workers: a stalled client parks the
        // writer's write, not the stdin loop (see `spawn_writer_thread`).
        let writer = spawn_writer_thread(rx, output, writer_alive_tx);
        let out = Out { tx: tx.clone() };
        drop(tx);

        // `Arc` so every input line can be dispatched on its own task (see
        // the loop below) while the UI poller keeps reading the session slot.
        let state = Arc::new(RpcServerState::new(
            self.session_dir.clone(),
            self.factory.clone(),
        ));
        // In-flight dispatched commands (see `MAX_CONCURRENT_COMMAND_DISPATCHES`).
        let running_commands = Arc::new(AtomicUsize::new(0));

        // Extension UI bridge: forward presentation-class extension
        // notifications (dialogs, title, editor, widgets, notify, status) as
        // `extension_ui_request` protocol lines. Replies arrive as
        // `extension_ui_response` frames dispatched in
        // `RpcServerState::dispatch`; an unanswered dialog resolves `null`
        // after the runtime's dialog TTL, so an unresponsive client degrades
        // instead of wedging the run.
        let ui_poller = {
            let session_slot = Arc::clone(&state.session);
            let out = out.clone();
            tokio::spawn(async move {
                // Adaptive cadence: 50 ms while dialogs/status updates flow,
                // doubling up to 500 ms while the queue stays empty (an idle
                // session must not take the global state lock 20x/s). A
                // drained notification snaps it back to the base.
                let mut interval = UI_POLL_INTERVAL;
                loop {
                    tokio::time::sleep(interval).await;
                    let session = session_slot
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let Some(session) = session else {
                        // No attached session: keep checking, but slowly.
                        interval = ui_poll_interval(interval, false);
                        continue;
                    };
                    match drain_extension_ui_notifications(&session, &out).await {
                        Ok(found) => interval = ui_poll_interval(interval, found),
                        Err(()) => return,
                    }
                }
            })
        };

        // Startup attach (upstream `runRpcMode(runtime)`): materialize the
        // CLI's session plan and build the attached session before the read
        // loop, so the very first command already sees it. A failure (stored
        // cwd gone, unreadable session, no model) ends the host like the
        // upstream runtime creation does.
        if let Some(plan) = &self.startup_session {
            let session = match open_startup_session(plan, &self.session_dir) {
                Ok(session) => session,
                Err(error) => {
                    ui_poller.abort();
                    // The writer is a dedicated thread now: dropping the
                    // result receiver plus the `Out` senders closes its
                    // channel, so it drains and exits (no `JoinHandle`).
                    drop(writer);
                    return Err(error);
                }
            };
            if let Err(error) = state.build_and_attach(session, &out) {
                // A model-less build (for example the `cli`-only feature row,
                // or a fresh install before `auth login`) can still serve the
                // protocol: the client may `set_model`/`create` later. The RPC
                // host must not exit on closed stdin just because no startup
                // model exists. Every command that needs a session reports the
                // same typed error through the normal dispatch path.
                if error.contains("no model available") {
                    eprintln!("pir rpc: startup session deferred: {error}");
                } else {
                    ui_poller.abort();
                    // See above: dropping the result receiver and the senders
                    // ends the dedicated writer thread.
                    drop(writer);
                    return Err(anyhow::anyhow!(
                        "cannot attach the startup session: {error}"
                    ));
                }
            }
        }

        let mut input = BufReader::new(input);
        loop {
            let line = tokio::select! {
                // The writer died (output write failed): responses can no
                // longer reach the client. Stop serving instead of silently
                // dropping every future response and streaming events into a
                // closed pipe. (`changed` also resolves on sender drop —
                // treated the same, the writer is gone either way.)
                _ = writer_alive_rx.changed() => {
                    eprintln!("pir rpc: output writer closed; ending session");
                    break;
                }
                // `ctx.shutdown()`: the run in flight settled (or the idle
                // command answered), so leave through the normal teardown
                // below instead of waiting for another input line (upstream
                // `checkShutdownRequested`, rpc-mode.ts:746/789).
                _ = state.shutdown_signal.notified() => {
                    eprintln!("pir rpc: shutdown requested by an extension");
                    break;
                }
                read = read_json_line(&mut input, DEFAULT_MAX_LINE_LENGTH) => match read {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(LineReadError::Io(error)) => {
                        eprintln!("pir rpc: stdin read failed: {error}");
                        break;
                    }
                    Err(LineReadError::TooLong) => {
                        // An unterminated over-limit line cannot be skipped
                        // safely, so the connection is torn down: report it
                        // like a parse failure (upstream reports parse
                        // failures as responses with command "parse") and end
                        // the session, same as the dead-writer path.
                        eprintln!(
                            "pir rpc: stdin line exceeds the \
                             {DEFAULT_MAX_LINE_LENGTH}-byte limit; ending session"
                        );
                        if out
                            .response(
                                None,
                                "parse",
                                Err(format!(
                                    "Failed to parse command: line exceeds the \
                                     {DEFAULT_MAX_LINE_LENGTH}-byte limit"
                                )),
                            )
                            .await
                            .is_err()
                        {
                            eprintln!("pir rpc: output writer closed; ending session");
                        }
                        break;
                    }
                }
            };
            let line = line.strip_suffix('\r').unwrap_or(line.as_str());
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) => {
                    // Concurrent dispatch, one task per input line (upstream
                    // `void handleInputLine(line)`, rpc-mode.ts:796-798): a
                    // long-running command — `bash`, `compact`, or an
                    // in-flight prompt — never blocks the read loop, so
                    // `abort`/`abort_bash`/`steer` are processed while it
                    // runs. Every response is correlated by its request id,
                    // and all protocol lines stay serialized through the one
                    // bounded `Out` channel. A dead writer still ends the
                    // session: it is detected by the `writer_alive` watch arm
                    // above (the writer task signals as soon as a write
                    // fails), so dropping the inline error here loses
                    // nothing.
                    //
                    // The task count is capped: a client that pipelines lines
                    // without draining responses would otherwise grow one task
                    // per line without bound. Past the cap the line is
                    // answered with a structured error (and the session keeps
                    // serving), never silently dropped.
                    // `create`/`attach` establish the session every later
                    // command needs. They are short and must be ordered ahead
                    // of a pipelined `prompt`; dispatching them concurrently
                    // let the prompt task run first and fail with "no
                    // attached session" (the client saw a correct response
                    // ordering only by luck). Long-running commands stay on
                    // the concurrent path below so abort/steer still work.
                    let command_type = value.get("type").and_then(Value::as_str);
                    if matches!(command_type, Some("create") | Some("attach")) {
                        if let Err(error) = state.dispatch(value, &out).await {
                            eprintln!("pir rpc: {error}");
                        }
                        continue;
                    }
                    let in_flight = running_commands.fetch_add(1, Ordering::SeqCst);
                    if !command_dispatch_admits(in_flight) {
                        running_commands.fetch_sub(1, Ordering::SeqCst);
                        let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
                        let command = value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        eprintln!(
                            "pir rpc: rejecting {command}: {in_flight} commands already in flight \
                             (limit {MAX_CONCURRENT_COMMAND_DISPATCHES})"
                        );
                        if let Err(error) = out.try_response(
                            id.as_deref(),
                            command,
                            Err(format!(
                                "too many commands in flight (limit \
                                 {MAX_CONCURRENT_COMMAND_DISPATCHES}); retry after a \
                                 running command settles"
                            )),
                        ) {
                            // The queue is full or the writer is gone. Do not
                            // block the read loop here: a closed writer is
                            // caught by the writer-alive arm on the next
                            // iteration, and a full queue is already applying
                            // the backpressure this rejection exists for.
                            eprintln!("pir rpc: dropping admission rejection: {error}");
                        }
                        continue;
                    }
                    let slot = DispatchSlot(Arc::clone(&running_commands));
                    let state = Arc::clone(&state);
                    let out = out.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        if let Err(error) = state.dispatch(value, &out).await {
                            eprintln!("pir rpc: {error}");
                        }
                    });
                }
                // Upstream reports parse failures as responses with
                // command "parse".
                Err(error) => {
                    if out
                        .response(
                            None,
                            "parse",
                            Err(format!("Failed to parse command: {error}")),
                        )
                        .await
                        .is_err()
                    {
                        eprintln!("pir rpc: output writer closed; ending session");
                        break;
                    }
                }
            }
        }
        // The writer task drains the channel until every sender is gone;
        // `out` holds a clone (and so does the UI poller), so all of them
        // must be gone before awaiting it. Awaiting the aborted poller's
        // join handle guarantees its `Out` clone is dropped first, and
        // awaiting the writer also lets in-flight dispatch tasks flush their
        // responses: their `Out` clones keep the channel open until they
        // finish (stdin EOF does not cut a running command short).
        //
        // Detach and tear down the session FIRST: the session-event bridge's
        // forwarder thread (see `build_and_attach`) holds an `Out` clone that
        // is only released when the attached session — and its event
        // handlers — drop. Awaiting the writer before that would deadlock on
        // the forwarder. The teardown also runs the plugins' `on_shutdown`
        // hooks exactly once (upstream exit-time session teardown).
        let attached = state
            .session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(state);
        if let Some(session) = attached {
            // BOUNDED teardown, mirroring the TUI worker ("a stuck plugin
            // cannot hang the exit", terminal.rs): a plugin `on_shutdown`
            // (MCP stdio teardown, python, an extension actor parked on a
            // dialog) that never returns must not wedge the process after the
            // client closed stdin. Measured: the two process-level rpc_commands
            // tests that run a prompt timed out here (wait_or_kill: "pir rpc
            // did not exit after stdin closed") while the no-prompt tests
            // exited fine — i.e. a run initializes something the teardown can
            // wait on forever.
            if tokio::time::timeout(std::time::Duration::from_secs(2), session.shutdown())
                .await
                .is_err()
            {
                eprintln!("pir rpc: session shutdown did not finish; exiting anyway");
            }
        }
        ui_poller.abort();
        drop(out);
        let _ = ui_poller.await;
        // Bounded writer drain: queued responses/events get their chance to
        // flush, but a sender that outlived the teardown (a leaked `Arc`
        // clone held by a task or a session handler) can no longer hang the
        // exit. The grace window is far longer than a healthy drain.
        match tokio::time::timeout(std::time::Duration::from_secs(5), writer).await {
            Ok(Ok(writer_result)) => writer_result,
            Ok(Err(_)) => Err(anyhow::anyhow!(
                "rpc writer thread exited without reporting its result"
            )),
            Err(_elapsed) => {
                eprintln!("pir rpc: output writer still open after stdin closed; ending session");
                Ok(())
            }
        }
    }
}

/// Build the RPC host's resolved selection: core registry facts + the final
/// extension plan's builtin facts + the loaded runtime's on-disk tools.
/// `Ok(None)` when the request is empty (also skips the enumeration).
///
/// Resolution happens AFTER the extension runtime loaded, exactly like print
/// and the TUI resolve after their registries exist (D10): an on-disk
/// `--tools my_js_tool` must match, and the builtin facts come from the plan
/// the runtime carries into the session's plugin host. The shared resolver
/// emits the compiled-out-tool diagnostics to stderr here, before the
/// JSON-lines protocol starts, so stdout stays a clean protocol channel.
fn resolve_rpc_tool_selection(
    request: &crate::coding_agent::tool_selection::ToolSelectionRequest,
    extension_plan: &[std::path::PathBuf],
    runtime: Option<&crate::extensions::ExtensionHandle>,
) -> crate::Result<Option<crate::coding_agent::ToolSelection>> {
    if !request.requested() {
        return Ok(None);
    }
    let mut core = crate::agent::tool_execution::ToolRegistry::new();
    crate::coding_agent::tools::register_core_tools(&mut core);
    let available = crate::coding_agent::tool_selection::host_available_tools(
        &core,
        crate::coding_agent::tool_selection::extension_plan_available_tools(extension_plan),
        runtime,
    );
    request.resolve(&available).map_err(anyhow::Error::msg)
}

/// CLI overrides the RPC host applies to every session its factory builds.
#[derive(Debug, Clone, Default)]
pub struct RpcOverrides {
    /// `--provider/--model/--thinking/--api-key/--models`.
    pub model: crate::coding_agent::overrides::CliModelOverrides,
    /// `--system-prompt` (already resolved to text).
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` entries (already resolved to text).
    pub append_system_prompt: Vec<String>,
    /// `--name`: applied to each attached session storage.
    pub session_name: Option<String>,
    /// The session plan the CLI resolved (`-c/-r/--session/--session-id/
    /// --fork/--no-session`). The host attaches its session at startup
    /// (upstream `main.ts:950-952`); `None` keeps the library-embedder
    /// behavior of attaching only on `attach`/`create`.
    pub startup_session: Option<crate::coding_agent::session_resolve::SessionPlan>,
}

/// Default session factory: drives real providers from pi configuration.
///
/// The extension runtime (when configured) is shared by every session the
/// RPC host attaches; `session_start` is dispatched per session by the
/// [`AgentSession`] facade. `tool_selection` is the host-resolved global
/// selection (`--tools` / `--exclude-tools` / `--no-tools` /
/// `--no-builtin-tools` / settings `defaultTools`); it is cloned per factory
/// invocation because one factory serves every attached session.
fn default_session_factory(
    session: Option<SessionManager>,
    extensions: Option<crate::extensions::ExtensionHandle>,
    tool_selection: Option<crate::coding_agent::ToolSelection>,
    overrides: &RpcOverrides,
    max_turns: usize,
) -> crate::Result<Arc<AgentSession>> {
    // The full model environment so a CLI `--model/--provider` can select any
    // available model (built-in catalog included), like print and the TUI.
    let environment = crate::ai::providers::load_pi_environment(None)?;
    let selection = crate::coding_agent::overrides::resolve_model(
        &overrides.model,
        &environment.models,
        &environment.api_keys,
        &environment.settings,
    )?;
    for diagnostic in &selection.diagnostics {
        eprintln!("Warning: {diagnostic}");
    }
    let mut tools = crate::agent::tool_execution::ToolRegistry::new();
    crate::coding_agent::tools::register_core_tools(&mut tools);
    let provider_registry = Arc::new(environment.registry.clone());
    // Remote catalog overlays (upstream model-runtime `withRemoteCatalog`):
    // background ETag revalidation, delayed first hop, PI_OFFLINE-gated.
    // No-runtime spawns degrade to a 0-count inside the install helper.
    crate::ai::providers::install_remote_catalog_overlays(&provider_registry);
    let mut session = session;
    if let (Some(session), Some(name)) = (session.as_mut(), overrides.session_name.as_deref()) {
        session.set_session_name(Some(name))?;
    }
    let agent_session = AgentSession::new_arc(crate::agent_session::AgentSessionConfig {
        provider_registry,
        model: selection.model,
        api_key: selection.api_key,
        thinking: selection.thinking,
        tools,
        session,
        compaction: Some(crate::compaction::CompactionConfig::default()),
        // The non-interactive turn cap (`Config::max_turns`, default 1000,
        // `PIR_MAX_TURNS` override): one turn is one model request, not one
        // tool call. The old hard-coded 100 cut off a 100-tool prompt that
        // emits one call per assistant message before its final answer.
        max_turns,
        system_prompt: None,
        cwd: None,
        event_stream: None,
        extensions,
        tool_selection,
    });
    if !selection.scoped_models.is_empty() {
        agent_session.set_scoped_models(selection.scoped_models);
    }
    // `--system-prompt` / `--append-system-prompt` on the structured prompt.
    agent_session.apply_prompt_overrides(
        overrides.system_prompt.clone(),
        overrides.append_system_prompt.clone(),
    );
    Ok(agent_session)
}

/// Run the RPC loop over stdin/stdout with the host's global tool selection.
///
/// `extension_paths` carries the `-e/--extension` entries plus the default
/// discovery paths resolved by the CLI; extension notifications are printed
/// to stderr so the JSON-line protocol on stdout stays unchanged.
///
/// The request is resolved against the runtime this host actually loads (not
/// a clone the CLI parked earlier): the final extension plan's builtin facts
/// plus the runtime's on-disk tools are exactly what the session's plugin host
/// will merge, so the selection cannot name tools the run registry lacks (and
/// an on-disk tool named by `--tools` is selectable).
pub async fn run_rpc_with_tool_selection(
    extension_paths: &[std::path::PathBuf],
    tool_selection_request: crate::coding_agent::tool_selection::ToolSelectionRequest,
) -> crate::Result<()> {
    run_rpc_with_options(
        extension_paths,
        tool_selection_request,
        RpcOverrides::default(),
    )
    .await
}

/// [`run_rpc_with_tool_selection`] plus the CLI's model/credential/prompt
/// overrides, applied to every session this host builds.
pub async fn run_rpc_with_options(
    extension_paths: &[std::path::PathBuf],
    tool_selection_request: crate::coding_agent::tool_selection::ToolSelectionRequest,
    overrides: RpcOverrides,
) -> crate::Result<()> {
    let config = Config::from_env();
    // The session factory owns the live registry, but extensions load before
    // any session exists. Resolve the catalog once here (the same
    // `load_pi_config` the factory calls) so factories that resolve a model
    // synchronously during load see it. A configuration error is NOT fatal
    // here: the factory reports it per session exactly as before, and the
    // loader simply keeps the empty-catalog behavior.
    let model_catalog = crate::ai::providers::load_pi_config()
        .ok()
        .flatten()
        .map(|configured| crate::agent_session::ModelCatalog::from_registry(&configured.registry));
    let extensions = crate::agent_session::load_extension_runtime_with_catalog(
        extension_paths,
        crate::extensions::ExtensionConfig {
            mode: "rpc".to_string(),
            // Extension UI requests are bridged to the RPC client as
            // `extension_ui_request` protocol lines (upstream rpc-mode.ts).
            // A client that never answers does not wedge the run: the
            // pending dialog resolves `null` after the runtime's dialog TTL.
            has_ui: true,
            ..Default::default()
        },
        model_catalog.as_ref(),
    )
    .await;
    let tool_selection = resolve_rpc_tool_selection(
        &tool_selection_request,
        extension_paths,
        extensions.as_ref(),
    )?;
    let overrides = Arc::new(overrides);
    let startup_session = overrides.startup_session.clone();
    // Resolved once at host startup: every session this factory builds gets
    // the same non-interactive turn cap.
    let max_turns = config.max_turns;
    let factory: SessionFactory = Arc::new(move |session| {
        default_session_factory(
            session,
            extensions.clone(),
            tool_selection.clone(),
            overrides.as_ref(),
            max_turns,
        )
    });
    let mut server = RpcServer::new(PathBuf::from(&config.session_dir), factory);
    if let Some(plan) = startup_session {
        server = server.with_startup_session(plan);
    }
    server.serve(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Back-compat entry: no CLI selection (the pre-Q60 behavior).
///
/// Library embedders keep calling this; only the CLI passes a request through
/// [`run_rpc_with_tool_selection`].
pub async fn run_rpc(extension_paths: &[std::path::PathBuf]) -> crate::Result<()> {
    run_rpc_with_tool_selection(
        extension_paths,
        crate::coding_agent::tool_selection::ToolSelectionRequest::default(),
    )
    .await
}

/// Parse a queue-drain mode from the wire (`"all"` / `"one-at-a-time"`,
/// upstream kebab-case spelling). A missing or invalid `mode` is a command
/// error.
fn parse_queue_mode<M: serde::de::DeserializeOwned>(value: &Value) -> Result<M, String> {
    let mode = value
        .get("mode")
        .ok_or("the command requires a 'mode' field")?;
    serde_json::from_value(mode.clone()).map_err(|error| format!("invalid 'mode' value: {error}"))
}
