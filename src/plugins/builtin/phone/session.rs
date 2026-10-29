//! pi-phone native port: parent-session mirror plus the parallel RPC session pool.
//!
//! This file is deliberately self-contained: the HTTP/WS server lane holds a
//! [`SessionBridge`], feeds it host events, and forwards phone JSON lines into
//! it. Nothing here binds a port, spawns a thread besides the Tokio tasks the
//! caller already runs, or knows about `server.rs`/`assets.rs`.
//!
//! # Bridge interface (what the server lane uses)
//!
//! ```no_run
//! use std::sync::Arc;
//! use pir::plugins::builtin::phone::session::{
//!     BridgeConfig, HostCommands, SessionBridge,
//! };
//! # struct Host;
//! # impl HostCommands for Host {
//! #     fn dispatch(
//! #         &self,
//! #         _command: pir::plugins::builtin::phone::session::ForwardedCommand,
//! #     ) -> pir::plugins::builtin::phone::session::HostFuture {
//! #         Box::pin(async {
//! #             Ok(pir::plugins::builtin::phone::session::HostReply::default())
//! #         })
//! #     }
//! # }
//! # async fn example() {
//! let host: Arc<dyn HostCommands> = Arc::new(Host);
//! let (bridge, host_events) = SessionBridge::start(BridgeConfig::default(), host);
//!
//! let Ok(mut client) = bridge.subscribe() else { panic!("no client slot") };
//! // Server task: forward each JSON line to the WebSocket verbatim.
//! if let Some(line) = client.recv().await {
//!     let _ = line;
//! }
//! // Server task: hand every inbound WebSocket text frame to the bridge.
//! bridge.handle_client_line(client.id(), r#"{ "kind": "rpc", "command": { "type": "abort" } }"#).await;
//! // Host task: every mapped host event goes through this bounded channel.
//! host_events.send(serde_json::json!({ "type": "agent_start" })).await.unwrap();
//! # }
//! ```
//!
//! * [`SessionBridge::start`] creates the inbound channel and spawns the
//!   fan-out task. It must run inside a Tokio runtime. Use
//!   [`SessionBridge::new`] + `tokio::spawn(bridge.clone().run())` when the
//!   caller wants to own the task.
//! * [`SessionBridge::subscribe`] returns a bounded per-client queue. A client
//!   that cannot keep up is removed and marked
//!   [`CloseReason::SlowConsumer`] instead of growing memory without bound.
//! * [`SessionBridge::handle_client_line`] parses the phone envelope and awaits
//!   [`HostCommands::dispatch`]. The reply goes only to the originating client;
//!   envelopes returned in [`HostReply::broadcast`] go to every client.
//! * The host feeds [`serde_json::Value`] events into the sender returned by
//!   `start`. Already-shaped `{ "channel": ... }` envelopes pass through;
//!   typed events are mapped by the table below.
//!
//! # Host event -> phone wire mapping
//!
//! Derived from `pi-phone/src/session-pool/parent-session-worker.ts`,
//! `session-worker.ts` and the client's `public/app/handlers.js`. The host is
//! authoritative for session state; the bridge is transport plus shape
//! normalization only.
//!
//! | Host event (`type`) | Phone envelope |
//! |---|---|
//! | `snapshot` | `{"channel":"snapshot","sessionWorkerId":...,"state":...,"messages":[last 60],"commands":[...],"liveAssistantMessage":...,"liveTools":[...],"historyTruncated":true,"totalMessageCount":N}` (`commands` is per-client deduped: unchanged since the last snapshot for that client is omitted, which the client treats as "keep mine"; an explicit empty array clears it) |
//! | `earlier-messages` | `{"channel":"snapshot","event":"earlier-messages","messages":[<=200],...}` (`startIndex` adjusted if clamped) |
//! | `status` | `{"channel":"server","event":"status","data":{...}}` |
//! | `catalog` / `sessions` | `{"channel":"sessions","event":"catalog","data":{...}}` |
//! | `agent_start`, `agent_end` | `{"channel":"rpc","payload":{"type":"agent_start"|"agent_end"}}` |
//! | `message_start` / `message_end` | rpc payload, message passed through |
//! | `message_update` | rpc payload slimmed to `message:{role,timestamp}` + `assistantMessageEvent` (same as the parent worker; deltas carry the content) |
//! | `tool_execution_start` | rpc payload passed through |
//! | `tool_execution_update` / `tool_execution_end` | rpc payload with every text part in `result.content` / `partialResult.content` capped at 32 KiB characters (JS `trimToolContent`) |
//! | `extension_ui_request`, `queue_update`, `compaction_start`, `compaction_end`, `auto_retry_start`, `auto_retry_end`, `response` | rpc payload passed through |
//! | `stderr` | `{"channel":"server","event":"stderr","data":{"text":...}}` |
//! | `reloading` | `{"channel":"server","event":"reloading","data":{"message":...}}` |
//! | `agent-exit` / `agent_exit` | `{"channel":"server","event":"agent-exit","data":{"code","signal","message"}}` |
//! | `session-spawned` / `session_spawned` | `{"channel":"server","event":"session-spawned","data":{"message":...}}` |
//! | `client-error`, `snapshot-error`, `startup-error`, `parse-error` | `{"channel":"server","event":<same>,"data":{"message"|"line","error"}}` |
//! | an object that already has `channel` | passed through as-is (`snapshot` still windowed, `earlier-messages` still clamped) |
//! | anything else | ignored (counted in [`BridgeStats::ignored_events`]) |
//!
//! The pool's [`SessionEvent`] values are already `channel`-shaped; forward
//! `event.event` into the bridge's host sender (see [`SessionEvent::into_value`]).
//!
//! # Phone command -> host seam
//!
//! `parse_client_message` accepts the client's `{kind:"rpc"|"local-command"|
//! "refresh"|...}` envelopes and returns a [`PhoneCommand`]. The command enum
//! covers everything in scope; unmodelled pi RPC commands are preserved as
//! [`PhoneCommand::Raw`] so the host can decide (the JS child forwards
//! arbitrary RPC, the JS parent mirrors a fixed set).
//!
//! | Phone input | `PhoneCommand` | Host must supply |
//! |---|---|---|
//! | `{"kind":"rpc","command":{"type":"prompt",...}}` | `Prompt` | `sendUserMessage(content, {deliverAs})`; mirror the optimistic user bubble |
//! | `abort` | `Abort` | `ctx.abort()` |
//! | `set_model` | `SetModel` | model registry lookup + `setModel`; report missing keys/models |
//! | `set_thinking_level` | `SetThinkingLevel` | `setThinkingLevel` |
//! | `compact` | `Compact` | manual compaction, emit `compaction_start`/`compaction_end` |
//! | `{"kind":"local-command","command":"reload"}` or rpc `reload` | `Reload` | command-context reload |
//! | `get_session_stats` | `GetSessionStats` | compute the `computeStats` payload |
//! | `phone_get_tree` | `PhoneGetTree` | session tree (`currentLeafId`, `currentPathIds`, flattened nodes) |
//! | `fork` | `Fork` | `commandCtx.fork(entryId)` |
//! | `phone_list_sessions` | `PhoneListSessions` | fast first-line session scan under the project session dir |
//! | `new_session`, `switch_session`, `set_session_name`, `get_state`, `get_messages`, `get_commands`, `get_available_models`, `phone_cancel_pending_message`, `phone_open_branch_path`, `path_suggestions`, `cd`, `slash-command`, `extension_ui_response` | `Raw` | verbatim pi RPC / server-local handling |
//! | `{"kind":"refresh"}`, `load-earlier-messages`, `session-select`, `session-spawn`, `session-parent-new` | lifecycle variants | pool selection/spawn, snapshot refresh |
//!
//! # Parent mirror semantics
//!
//! The first phone client mirrors the live CLI session. The host owns that
//! session and must supply, through `HostCommands` and the event feed:
//!
//! * a `snapshot` after every state change, with `state` containing at least
//!   `model{id,name,provider,contextWindow}`, `thinkingLevel`, `isStreaming`,
//!   `isCompacting`, `sessionFile`, `sessionId`, `sessionName`, `messageCount`,
//!   `pendingMessageCount`, `contextUsage`, `usageTotals`, and the session
//!   messages in pi's wire shape;
//! * `agent_start` / `agent_end` / `message_*` / `tool_execution_*` /
//!   `extension_ui_request` / `queue_update` / `compaction_*` events as they
//!   happen (live content travels over the rpc channel, the snapshot is the
//!   resync point);
//! * prompt submission with the same `deliverAs` (`steer`/`followUp`)
//!   semantics and pending-queue mirroring the JS parent worker documents;
//! * status and catalog frames (`status`, `catalog`) whenever the pool or
//!   ownership state changes. The bridge does not guess them.
//!
//! The parent session id used by the JS pool is `parent-session`; parallel
//! sessions are `active-session-N`. The bridge passes `sessionWorkerId`
//! through, so the server lane can filter frames per client if it wants
//! (the first slice broadcasts to every subscriber).
//!
//! # Parallel session pool
//!
//! [`SessionPool::new`] returns the pool plus a bounded [`SessionEvent`]
//! receiver. [`SessionPool::spawn`] runs the current executable with
//! `--mode rpc` (never a shell), `PI_PHONE_CHILD=1`, and stdio pipes; a
//! `--session <file>` pair is appended when known. Each child speaks one JSON
//! object per line on stdout, matching `session-worker.ts`:
//!
//! * requests carry `id: "srv-N"` and are correlated through a
//!   `oneshot` map with a timeout (default 30 s);
//! * client-forwarded commands use `id: "cli-N"` when the client did not
//!   supply one;
//! * responses cache `get_state` / `get_messages` / `get_commands`;
//! * live events update the cached transcript/live tools before being
//!   forwarded as `{"channel":"rpc","sessionWorkerId":"<id>","payload":...}`;
//! * stderr becomes a `server/stderr` event; parse failures become
//!   `server/parse-error` with the line truncated to 200 characters;
//! * an unexpected exit emits `server/agent-exit` and restarts with
//!   exponential backoff while `auto_restart` is on, up to
//!   [`PoolConfig::max_restarts`] consecutive attempts (default 3). After that
//!   the session emits a final `agent-exit` and stays down. Disposing,
//!   stopping or a stop during the backoff cancels the restart.
//! * a supervisor that will not restart removes its own catalog row and
//!   releases the pool slot, so dead sessions never consume
//!   `max_sessions`; every error path (spawn, write, exit, stop, drop)
//!   releases exactly one slot.
//!
//! Limits kept from the JS reference: 60-message snapshot window, 200-message
//! earlier page, 32 KiB tool text cap, 1500 ms restart delay, 2000 ms kill
//! grace. The JS pool has **no child-count cap**; to keep the bridge bounded
//! this port adds `max_sessions` (default 16) and reports
//! [`PoolError::TooManySessions`] honestly. Line, cache, and queue sizes are
//! bounded here too (the JS accumulates `stdoutBuffer` without a limit). The
//! bridge additionally bounds queued *bytes* per client (4 MiB) and across the
//! bridge (16 MiB), coalescing streaming updates under pressure so terminal
//! events still arrive.
//!
//! # Not covered by this slice
//!
//! * WebSocket upgrade/auth/framing, HTTP routes, static assets, Tailscale and
//!   session ownership live in other lanes.
//! * Image extraction to `/api/image` and assistant-metadata slimming are the
//!   server runtime's `compactMessages`; this file only ports the 32 KiB tool
//!   trim (and snapshot windowing).
//! * `phone_cancel_pending_message` re-queueing through `clear_queue` is left
//!   to the host adapter.
//! * Process-tree cleanup uses `taskkill /T /F` on Windows (no shell, run via
//!   `spawn_blocking` so it never blocks a Tokio worker) and
//!   `kill(-pgid, SIGKILL)` on Unix (the child is a process-group leader), so
//!   tool descendants are swept too. `stop`/`shutdown` await and reap the
//!   supervisor; if a child somehow survives the kill grace, the supervisor
//!   is aborted, `kill_on_drop` fires, and an `agent-exit` reports the
//!   escalation. `Drop` kills the tree synchronously and signals the
//!   supervisor.
//! * The real `pir --mode rpc` child is not exercised by the tests; a mock
//!   child (the test binary re-entered in child mode) covers framing,
//!   request/response correlation, exit handling, limits and kill-on-stop.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Notify};

/// Trailing messages carried by a live snapshot (JS `SNAPSHOT_MESSAGE_WINDOW`).
pub const SNAPSHOT_MESSAGE_WINDOW: usize = 60;
/// Largest page `load-earlier-messages` may return (JS clamps at 200).
pub const EARLIER_MESSAGE_PAGE_MAX: usize = 200;
/// Per-text-part cap for live tool envelopes (JS `TOOL_ENVELOPE_CHAR_CAP`).
pub const TOOL_TEXT_CAP_CHARS: usize = 32 * 1024;
/// Delay before an unexpectedly exited child is restarted (JS: 1500 ms).
pub const CHILD_RESTART_DELAY_MS: u64 = 1_500;
/// How long a stop waits for the child to die (JS: SIGKILL after 2000 ms).
pub const CHILD_KILL_GRACE_MS: u64 = 2_000;
/// Default request timeout (JS `request` default: 30000 ms).
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;
/// Per-client queued-byte budget (lines are otherwise bounded only by count,
/// and one snapshot line can be multi-megabyte).
pub const DEFAULT_CLIENT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
/// Per-bridge queued-byte budget across every subscriber.
pub const DEFAULT_TOTAL_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// Auto-restart budget after unexpected child exits.
pub const DEFAULT_MAX_RESTARTS: u32 = 3;

// ---------------------------------------------------------------------------
// Host command seam
// ---------------------------------------------------------------------------

/// A command parsed from a phone client, ready for the host.
#[derive(Debug, Clone, PartialEq)]
pub enum PhoneCommand {
    /// Send a user prompt. `streaming_behavior` is `steer` or `followUp`.
    Prompt {
        message: String,
        images: Vec<Value>,
        streaming_behavior: Option<String>,
    },
    Abort,
    SetModel {
        provider: String,
        model_id: String,
    },
    SetThinkingLevel {
        level: String,
    },
    Compact {
        custom_instructions: Option<String>,
    },
    /// `local-command:"reload"` and rpc `reload` both land here.
    Reload,
    /// `get_session_stats` (`stats` is accepted as an alias).
    GetSessionStats,
    /// `phone_get_tree` (`tree` is accepted as an alias).
    PhoneGetTree,
    Fork {
        entry_id: Option<String>,
    },
    /// `phone_list_sessions` (`session_list` is accepted as an alias).
    PhoneListSessions {
        include_subagents: bool,
    },
    NewSession {
        parent_session: Option<String>,
    },
    SwitchSession {
        session_path: String,
    },
    SetSessionName {
        name: String,
    },
    /// Explicit resync request; the host answers with a snapshot.
    Refresh,
    /// Page older history; the host answers with an `earlier-messages` event.
    LoadEarlierMessages {
        before: Option<usize>,
        count: Option<usize>,
    },
    /// Spawn a parallel `pir --mode rpc` session.
    SessionSpawn {
        session_file: Option<String>,
    },
    /// Bind the client to an existing session.
    SessionSelect {
        session_id: String,
    },
    /// Create a new parent (mirrored CLI) session.
    SessionParentNew,
    /// Anything else the host may know (pi RPC passthrough).
    Raw {
        command: Value,
    },
}

impl PhoneCommand {
    /// Wire name used in a `response` envelope, matching the client's handler.
    pub fn wire_name(&self) -> &str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Abort => "abort",
            Self::SetModel { .. } => "set_model",
            Self::SetThinkingLevel { .. } => "set_thinking_level",
            Self::Compact { .. } => "compact",
            Self::Reload => "reload",
            Self::GetSessionStats => "get_session_stats",
            Self::PhoneGetTree => "phone_get_tree",
            Self::Fork { .. } => "fork",
            Self::PhoneListSessions { .. } => "phone_list_sessions",
            Self::NewSession { .. } => "new_session",
            Self::SwitchSession { .. } => "switch_session",
            Self::SetSessionName { .. } => "set_session_name",
            Self::Refresh => "refresh",
            Self::LoadEarlierMessages { .. } => "load_earlier_messages",
            Self::SessionSpawn { .. } => "session-spawn",
            Self::SessionSelect { .. } => "session-select",
            Self::SessionParentNew => "session-parent-new",
            Self::Raw { command } => match command.get("type").and_then(Value::as_str) {
                Some("slash-command") => "slash_command",
                Some(name) => name,
                None => "unknown",
            },
        }
    }

    /// Whether the phone client expects a `response` envelope for this command.
    /// Lifecycle commands are acknowledged with snapshots/catalog frames in the
    /// JS reference; the bridge only replies with data if the host supplies it.
    pub fn expects_response(&self) -> bool {
        !matches!(
            self,
            Self::Refresh
                | Self::LoadEarlierMessages { .. }
                | Self::SessionSpawn { .. }
                | Self::SessionSelect { .. }
                | Self::SessionParentNew
        )
    }
}

/// A command forwarded to [`HostCommands`], with the originating client and
/// the client's active session (for routing to the parent or a pool child).
#[derive(Debug, Clone)]
pub struct ForwardedCommand {
    pub command: PhoneCommand,
    /// Correlation id echoed in the response envelope (`cli-N` if the client
    /// did not send one).
    pub request_id: Option<String>,
    /// [`BridgeSubscriber::id`] of the originating WebSocket.
    pub client_id: u64,
    /// The client's selected session (`parent-session` or `active-session-N`),
    /// updated when the client sends `session-select`.
    pub active_session_id: Option<String>,
}

/// The host's reply to one command.
#[derive(Debug, Clone, Default)]
pub struct HostReply {
    /// Value placed under `payload.data` in the `response` envelope.
    pub data: Option<Value>,
    /// Envelopes to broadcast to every client before the response (for
    /// example `session-spawned` or a fresh `catalog`).
    pub broadcast: Vec<Value>,
}

impl HostReply {
    pub fn data(value: Value) -> Self {
        Self {
            data: Some(value),
            broadcast: Vec::new(),
        }
    }

    pub fn broadcast_event(value: Value) -> Self {
        Self {
            data: None,
            broadcast: vec![value],
        }
    }

    #[must_use]
    pub fn with_broadcast(mut self, value: Value) -> Self {
        self.broadcast.push(value);
        self
    }
}

/// Boxed future returned by [`HostCommands::dispatch`]. The host owns any
/// session/state borrows it needs; the future must be `'static` so the bridge
/// can await it from a WebSocket task.
pub type HostFuture = Pin<Box<dyn Future<Output = Result<HostReply, String>> + Send + 'static>>;

/// The seam the server lane implements: route phone commands to the live CLI
/// session or to a [`SessionPool`] child.
///
/// The host is expected to be the same object that feeds host events into the
/// bridge. For the parent mirror that means adapting the in-process
/// `ExtensionContext`/command context exactly as the JS `PhoneParentSessionWorker`
/// does (`sendUserMessage`, `abort`, `compact`, `setModel`, `setThinkingLevel`,
/// `commandCtx.reload`, stats/tree/list helpers). Parallel sessions are routed
/// through [`SessionPool::request`] / [`SessionPool::send_command`].
pub trait HostCommands: Send + Sync + 'static {
    fn dispatch(&self, command: ForwardedCommand) -> HostFuture;
}

/// Adapter for closures/async blocks, handy in tests and small hosts.
pub struct FnHostCommands<F>(pub F);

impl<F, Fut> HostCommands for FnHostCommands<F>
where
    F: Fn(ForwardedCommand) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<HostReply, String>> + Send + 'static,
{
    fn dispatch(&self, command: ForwardedCommand) -> HostFuture {
        Box::pin((self.0)(command))
    }
}

/// Parse a phone client line into a [`PhoneCommand`].
///
/// Accepts the client's `{kind:...}` envelopes. A bare `{type:...}` command
/// object is also accepted so the server lane and tests can drive the seam
/// without wrapping it.
pub fn parse_client_message(value: &Value) -> Result<PhoneCommand, String> {
    match value.get("kind").and_then(Value::as_str) {
        None => parse_rpc_command(value),
        Some("rpc") => {
            let command = value
                .get("command")
                .ok_or_else(|| "rpc message is missing its command object".to_owned())?;
            parse_rpc_command(command)
        }
        Some("local-command") => parse_local_command(value.get("command")),
        Some("refresh") => Ok(PhoneCommand::Refresh),
        Some("load-earlier-messages") => Ok(PhoneCommand::LoadEarlierMessages {
            before: value
                .get("before")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok()),
            count: value
                .get("count")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok()),
        }),
        Some("session-select") => Ok(PhoneCommand::SessionSelect {
            session_id: value
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| "session-select requires sessionId".to_owned())?
                .to_owned(),
        }),
        Some("session-spawn") => Ok(PhoneCommand::SessionSpawn {
            session_file: value
                .get("sessionFile")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        Some("session-parent-new") => Ok(PhoneCommand::SessionParentNew),
        Some(other) => Err(format!("Unsupported client message kind: {other}")),
    }
}

fn parse_local_command(command: Option<&Value>) -> Result<PhoneCommand, String> {
    match command {
        Some(Value::String(text)) if text == "reload" => Ok(PhoneCommand::Reload),
        Some(Value::Object(object)) => {
            if object.get("type").and_then(Value::as_str) == Some("reload") {
                Ok(PhoneCommand::Reload)
            } else {
                Ok(PhoneCommand::Raw {
                    command: Value::Object(object.clone()),
                })
            }
        }
        _ => Err("Unsupported local command.".to_owned()),
    }
}

fn parse_rpc_command(value: &Value) -> Result<PhoneCommand, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "rpc command must be a JSON object".to_owned())?;
    let command_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "rpc command is missing its type".to_owned())?;

    match command_type {
        "prompt" => Ok(PhoneCommand::Prompt {
            message: object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            images: object
                .get("images")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            streaming_behavior: match object.get("streamingBehavior").and_then(Value::as_str) {
                Some("steer") => Some("steer".to_owned()),
                Some("followUp") => Some("followUp".to_owned()),
                _ => None,
            },
        }),
        "abort" => Ok(PhoneCommand::Abort),
        "set_model" => Ok(PhoneCommand::SetModel {
            provider: object
                .get("provider")
                .and_then(Value::as_str)
                .ok_or_else(|| "set_model requires provider".to_owned())?
                .to_owned(),
            model_id: object
                .get("modelId")
                .and_then(Value::as_str)
                .ok_or_else(|| "set_model requires modelId".to_owned())?
                .to_owned(),
        }),
        "set_thinking_level" => Ok(PhoneCommand::SetThinkingLevel {
            level: object
                .get("level")
                .and_then(Value::as_str)
                .ok_or_else(|| "set_thinking_level requires level".to_owned())?
                .to_owned(),
        }),
        "compact" => Ok(PhoneCommand::Compact {
            custom_instructions: object
                .get("customInstructions")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "reload" => Ok(PhoneCommand::Reload),
        "get_session_stats" | "stats" => Ok(PhoneCommand::GetSessionStats),
        "phone_get_tree" | "tree" => Ok(PhoneCommand::PhoneGetTree),
        "fork" => Ok(PhoneCommand::Fork {
            entry_id: object
                .get("entryId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "phone_list_sessions" | "session_list" => Ok(PhoneCommand::PhoneListSessions {
            include_subagents: object
                .get("includeSubagents")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "new_session" => Ok(PhoneCommand::NewSession {
            parent_session: object
                .get("parentSession")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "switch_session" => Ok(PhoneCommand::SwitchSession {
            session_path: object
                .get("sessionPath")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "set_session_name" => Ok(PhoneCommand::SetSessionName {
            name: object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        _ => Ok(PhoneCommand::Raw {
            command: Value::Object(object.clone()),
        }),
    }
}

fn client_request_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("command")
                .and_then(|command| command.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// SessionBridge
// ---------------------------------------------------------------------------

/// Bridge limits. All queues are bounded; the defaults are chosen for one phone
/// (the JS server allows a single client, this port allows up to 8).
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Host event queue. Backpressure on [`SessionBridge::host_sender`] callers.
    pub inbound_capacity: usize,
    /// Per-subscriber JSON line queue.
    pub client_queue_capacity: usize,
    /// Per-subscriber queued-byte budget. A line that would push the client
    /// past this (or [`Self::total_queue_bytes`]) closes it as
    /// [`CloseReason::SlowConsumer`]; coalescible updates are evicted first.
    pub client_queue_bytes: usize,
    /// Queued-byte budget summed over every subscriber.
    pub total_queue_bytes: usize,
    /// Subscribers allowed at once.
    pub max_clients: usize,
    /// Trailing messages in a snapshot.
    pub snapshot_message_window: usize,
    /// Largest `earlier-messages` page.
    pub earlier_page_max: usize,
    /// Characters kept from one tool text part.
    pub tool_text_cap_chars: usize,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            inbound_capacity: 256,
            client_queue_capacity: 256,
            client_queue_bytes: DEFAULT_CLIENT_QUEUE_BYTES,
            total_queue_bytes: DEFAULT_TOTAL_QUEUE_BYTES,
            max_clients: 8,
            snapshot_message_window: SNAPSHOT_MESSAGE_WINDOW,
            earlier_page_max: EARLIER_MESSAGE_PAGE_MAX,
            tool_text_cap_chars: TOOL_TEXT_CAP_CHARS,
        }
    }
}

/// Why a subscriber stopped receiving lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The client's queue filled; it was dropped instead of growing memory.
    SlowConsumer,
    /// The bridge was closed (server stop / host feed closed).
    ServerClosed,
    /// The server lane dropped the [`BridgeSubscriber`].
    ClientDropped,
}

impl CloseReason {
    fn code(self) -> u8 {
        match self {
            Self::SlowConsumer => 1,
            Self::ServerClosed => 2,
            Self::ClientDropped => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::SlowConsumer),
            2 => Some(Self::ServerClosed),
            3 => Some(Self::ClientDropped),
            _ => None,
        }
    }
}

/// Bridge admission errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscribeError {
    /// The bridge has been closed.
    Closed,
    /// `max_clients` subscribers are already attached.
    TooManyClients { max: usize },
}

impl std::fmt::Display for SubscribeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(formatter, "phone bridge is closed"),
            Self::TooManyClients { max } => {
                write!(formatter, "phone bridge already has {max} clients")
            }
        }
    }
}

impl std::error::Error for SubscribeError {}

/// Snapshot of the bridge counters (monotonic except the queued-byte gauge).
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStats {
    pub host_events: u64,
    pub mapped_events: u64,
    pub ignored_events: u64,
    pub lines_sent: u64,
    pub slow_clients_dropped: u64,
    pub clients_rejected: u64,
    pub commands: u64,
    pub command_errors: u64,
    /// Updates folded into an already-queued update line.
    pub coalesced_updates: u64,
    /// Updates dropped because no queued update could absorb them.
    pub dropped_updates: u64,
    /// Bytes currently queued across every subscriber.
    pub queued_bytes: u64,
    /// High-water mark of [`Self::queued_bytes`].
    pub peak_queued_bytes: u64,
}

#[derive(Debug, Default)]
struct BridgeStatsAtomic {
    host_events: AtomicU64,
    mapped_events: AtomicU64,
    ignored_events: AtomicU64,
    lines_sent: AtomicU64,
    slow_clients_dropped: AtomicU64,
    clients_rejected: AtomicU64,
    commands: AtomicU64,
    command_errors: AtomicU64,
    coalesced_updates: AtomicU64,
    dropped_updates: AtomicU64,
    peak_queued_bytes: AtomicUsize,
}

impl BridgeStatsAtomic {
    fn snapshot(&self) -> BridgeStats {
        BridgeStats {
            host_events: self.host_events.load(Ordering::Relaxed),
            mapped_events: self.mapped_events.load(Ordering::Relaxed),
            ignored_events: self.ignored_events.load(Ordering::Relaxed),
            lines_sent: self.lines_sent.load(Ordering::Relaxed),
            slow_clients_dropped: self.slow_clients_dropped.load(Ordering::Relaxed),
            clients_rejected: self.clients_rejected.load(Ordering::Relaxed),
            commands: self.commands.load(Ordering::Relaxed),
            command_errors: self.command_errors.load(Ordering::Relaxed),
            coalesced_updates: self.coalesced_updates.load(Ordering::Relaxed),
            dropped_updates: self.dropped_updates.load(Ordering::Relaxed),
            queued_bytes: 0,
            peak_queued_bytes: self.peak_queued_bytes.load(Ordering::Relaxed) as u64,
        }
    }
}

#[derive(Debug, Default)]
struct ClientControl {
    reason: AtomicU8,
}

impl ClientControl {
    fn new() -> Self {
        Self::default()
    }

    fn reason(&self) -> Option<CloseReason> {
        CloseReason::from_code(self.reason.load(Ordering::Acquire))
    }

    fn set(&self, reason: CloseReason) {
        let _ = self
            .reason
            .compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire);
    }
}

/// How a queued line participates in overload handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineClass {
    /// High-frequency streaming update; can be coalesced or dropped.
    Update,
    /// Lifecycle/response line that must survive an update flood.
    Terminal,
    /// Everything else; dropped only when no update can make room.
    Other,
}

impl LineClass {
    fn is_update(self) -> bool {
        matches!(self, Self::Update)
    }
}

fn classify_line(value: &Value) -> LineClass {
    let payload_type = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str);
    match payload_type {
        Some("message_update" | "tool_execution_update") => LineClass::Update,
        Some("message_end" | "agent_end" | "response" | "tool_execution_end") => {
            LineClass::Terminal
        }
        _ => match value.get("event").and_then(Value::as_str) {
            Some("agent-exit") => LineClass::Terminal,
            _ => LineClass::Other,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueOutcome {
    Sent,
    Coalesced,
    Dropped,
    OverBudget,
}

#[derive(Default)]
struct ClientQueueState {
    lines: VecDeque<(String, LineClass)>,
    bytes: usize,
    closed: bool,
}

/// Per-client queue with both a line cap and a byte budget. The bridge is the
/// only producer and the WebSocket writer the only consumer; `Notify` replaces
/// the mpsc channel so queued updates can be coalesced under pressure.
struct ClientQueue {
    state: Mutex<ClientQueueState>,
    notify: Notify,
}

impl ClientQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(ClientQueueState::default()),
            notify: Notify::new(),
        }
    }

    /// Mark closed. Already-queued lines stay readable so the writer can
    /// flush them before the close frame; their bytes stay accounted.
    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.notify.notify_one();
    }

    /// Drop any undelivered lines and return how many bytes were released.
    fn discard(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = state.bytes;
        state.lines.clear();
        state.bytes = 0;
        bytes
    }

    fn try_pop(&self) -> Option<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (line, _) = state.lines.pop_front()?;
        state.bytes = state.bytes.saturating_sub(line.len());
        Some(line)
    }

    async fn recv(&self) -> Option<String> {
        loop {
            if let Some(line) = self.try_pop() {
                return Some(line);
            }
            if self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed
            {
                return None;
            }
            self.notify.notified().await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &self,
        line: String,
        class: LineClass,
        max_lines: usize,
        max_bytes: usize,
        total_bytes: &AtomicUsize,
        total_max: usize,
    ) -> EnqueueOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return EnqueueOutcome::Dropped;
        }
        let len = line.len();
        let max_lines = max_lines.max(1);
        let max_bytes = max_bytes.max(1);
        let total_max = total_max.max(1);

        if class.is_update() {
            if len > max_bytes {
                // This update can never fit; keep the queue intact.
                return EnqueueOutcome::Dropped;
            }
            if state.lines.len() < max_lines
                && state.bytes + len <= max_bytes
                && reserve_bytes(total_bytes, len, total_max)
            {
                state.bytes += len;
                state.lines.push_back((line, class));
                drop(state);
                self.notify.notify_one();
                return EnqueueOutcome::Sent;
            }
            // Full: evict the oldest queued update (it is superseded by this
            // one) and append the newest at the tail, preserving order.
            if let Some(index) = state
                .lines
                .iter()
                .position(|(_, existing)| existing.is_update())
            {
                let Some((removed, _)) = state.lines.remove(index) else {
                    return EnqueueOutcome::Dropped;
                };
                state.bytes = state.bytes.saturating_sub(removed.len());
                total_bytes.fetch_sub(removed.len(), Ordering::AcqRel);
                if state.bytes + len <= max_bytes && reserve_bytes(total_bytes, len, total_max) {
                    state.bytes += len;
                    state.lines.push_back((line, class));
                    drop(state);
                    self.notify.notify_one();
                    return EnqueueOutcome::Coalesced;
                }
            }
            return EnqueueOutcome::Dropped;
        }

        // A terminal/other line must survive: evict queued updates to fit.
        while state.lines.len() >= max_lines || state.bytes + len > max_bytes {
            let Some(index) = state
                .lines
                .iter()
                .position(|(_, existing)| existing.is_update())
            else {
                return EnqueueOutcome::OverBudget;
            };
            let Some((removed, _)) = state.lines.remove(index) else {
                return EnqueueOutcome::OverBudget;
            };
            state.bytes = state.bytes.saturating_sub(removed.len());
            total_bytes.fetch_sub(removed.len(), Ordering::AcqRel);
        }
        if !reserve_bytes(total_bytes, len, total_max) {
            return EnqueueOutcome::OverBudget;
        }
        state.bytes += len;
        state.lines.push_back((line, class));
        drop(state);
        self.notify.notify_one();
        EnqueueOutcome::Sent
    }
}

fn reserve_bytes(counter: &AtomicUsize, amount: usize, max: usize) -> bool {
    if amount == 0 {
        return true;
    }
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(amount).filter(|next| *next <= max)
        })
        .is_ok()
}

struct ClientEntry {
    queue: Arc<ClientQueue>,
    /// Per-connection line cap (`subscribe_with_capacity`).
    queue_capacity: usize,
    control: Arc<ClientControl>,
    /// `(sessionWorkerId, serialized commands)` of the last snapshot sent.
    last_commands: Mutex<Option<(String, String)>>,
    next_request_id: AtomicU64,
    active_session: Mutex<Option<String>>,
}

struct BridgeRunner {
    host_rx: mpsc::Receiver<Value>,
    wake_rx: mpsc::Receiver<()>,
}

struct BridgeInner {
    config: BridgeConfig,
    host: Arc<dyn HostCommands>,
    clients: Mutex<HashMap<u64, ClientEntry>>,
    next_client_id: AtomicU64,
    stats: BridgeStatsAtomic,
    /// Sum of every subscriber's queued bytes.
    queued_bytes: AtomicUsize,
    closed: AtomicBool,
    wake_tx: mpsc::Sender<()>,
}

impl BridgeInner {
    fn drop_client(&self, client_id: u64, reason: CloseReason) {
        let entry = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&client_id);
        if let Some(entry) = entry {
            entry.control.set(reason);
            entry.queue.close();
        }
    }

    fn send_to_client(&self, client_id: u64, line: String, class: LineClass) {
        let outcome = {
            let clients = self
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match clients.get(&client_id) {
                Some(entry) => entry.queue.push(
                    line,
                    class,
                    entry.queue_capacity,
                    self.config.client_queue_bytes,
                    &self.queued_bytes,
                    self.config.total_queue_bytes,
                ),
                None => return,
            }
        };
        self.account_outcome(outcome);
        if matches!(outcome, EnqueueOutcome::OverBudget) {
            self.drop_client(client_id, CloseReason::SlowConsumer);
        }
    }

    fn account_outcome(&self, outcome: EnqueueOutcome) {
        match outcome {
            EnqueueOutcome::Sent | EnqueueOutcome::Coalesced => {
                self.stats.lines_sent.fetch_add(1, Ordering::Relaxed);
                if matches!(outcome, EnqueueOutcome::Coalesced) {
                    self.stats.coalesced_updates.fetch_add(1, Ordering::Relaxed);
                }
                let queued = self.queued_bytes.load(Ordering::Relaxed);
                self.stats
                    .peak_queued_bytes
                    .fetch_max(queued, Ordering::Relaxed);
            }
            EnqueueOutcome::Dropped => {
                self.stats.dropped_updates.fetch_add(1, Ordering::Relaxed);
            }
            EnqueueOutcome::OverBudget => {
                self.stats
                    .slow_clients_dropped
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// A bounded phone-client subscription. Each item is one complete JSON line
/// (no trailing newline), ready for a WebSocket text frame.
pub struct BridgeSubscriber {
    id: u64,
    queue: Arc<ClientQueue>,
    control: Arc<ClientControl>,
    inner: Arc<BridgeInner>,
}

impl BridgeSubscriber {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Next JSON line. `None` means the client was dropped (check
    /// [`Self::close_reason`]) or the bridge closed.
    pub async fn recv(&mut self) -> Option<String> {
        let line = self.queue.recv().await;
        if let Some(line) = &line {
            self.inner
                .queued_bytes
                .fetch_sub(line.len(), Ordering::AcqRel);
        }
        line
    }

    /// Non-blocking receive.
    pub fn try_recv(&mut self) -> Option<String> {
        let line = self.queue.try_pop();
        if let Some(line) = &line {
            self.inner
                .queued_bytes
                .fetch_sub(line.len(), Ordering::AcqRel);
        }
        line
    }

    pub fn close_reason(&self) -> Option<CloseReason> {
        self.control.reason()
    }

    pub fn is_open(&self) -> bool {
        self.control.reason().is_none()
    }
}

impl Drop for BridgeSubscriber {
    fn drop(&mut self) {
        self.inner.drop_client(self.id, CloseReason::ClientDropped);
        // The entry may already be gone (slow-consumer drop), but the queue
        // can still hold lines that were never read. Release their bytes.
        let released = self.queue.discard();
        if released > 0 {
            self.inner
                .queued_bytes
                .fetch_sub(released, Ordering::AcqRel);
        }
    }
}

/// Fan-out hub between the host session and phone WebSocket clients.
///
/// Cheap to clone; all clones share the client registry and host feed.
#[derive(Clone)]
pub struct SessionBridge {
    inner: Arc<BridgeInner>,
    runner: Arc<Mutex<Option<BridgeRunner>>>,
}

impl SessionBridge {
    /// Create a bridge around an existing host event receiver. The caller must
    /// drive it with [`Self::run`] (or use [`Self::start`]).
    pub fn new(
        config: BridgeConfig,
        host: Arc<dyn HostCommands>,
        host_rx: mpsc::Receiver<Value>,
    ) -> Self {
        let (wake_tx, wake_rx) = mpsc::channel(1);
        Self {
            inner: Arc::new(BridgeInner {
                config,
                host,
                clients: Mutex::new(HashMap::new()),
                next_client_id: AtomicU64::new(0),
                stats: BridgeStatsAtomic::default(),
                queued_bytes: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                wake_tx,
            }),
            runner: Arc::new(Mutex::new(Some(BridgeRunner { host_rx, wake_rx }))),
        }
    }

    /// Create the inbound host channel and spawn the fan-out task.
    ///
    /// Requires a Tokio runtime. Returns the bridge and the sender the host
    /// uses to publish events (bounded by `config.inbound_capacity`).
    pub fn start(config: BridgeConfig, host: Arc<dyn HostCommands>) -> (Self, mpsc::Sender<Value>) {
        let (host_tx, host_rx) = mpsc::channel(config.inbound_capacity.max(1));
        let bridge = Self::new(config, host, host_rx);
        let runner = bridge.clone();
        tokio::spawn(async move {
            runner.run().await;
        });
        (bridge, host_tx)
    }

    /// Drive the inbound event loop. Safe to call once; later calls return
    /// immediately. The loop ends when every host sender is dropped or
    /// [`Self::close`] is called, and then closes all subscribers.
    pub async fn run(&self) {
        let taken = self
            .runner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(BridgeRunner {
            mut host_rx,
            mut wake_rx,
        }) = taken
        else {
            return;
        };

        loop {
            tokio::select! {
                event = host_rx.recv() => {
                    match event {
                        Some(event) => self.dispatch_host_event(event),
                        None => break,
                    }
                }
                _ = wake_rx.recv() => {
                    if self.inner.closed.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
        }

        self.inner.closed.store(true, Ordering::Release);
        let ids: Vec<u64> = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.inner.drop_client(id, CloseReason::ServerClosed);
        }
    }

    /// Attach a phone client with the configured queue size.
    pub fn subscribe(&self) -> Result<BridgeSubscriber, SubscribeError> {
        self.subscribe_with_capacity(self.inner.config.client_queue_capacity)
    }

    /// Attach a phone client with an explicit queue size (tests, or a server
    /// lane that wants a different bound per connection).
    pub fn subscribe_with_capacity(
        &self,
        capacity: usize,
    ) -> Result<BridgeSubscriber, SubscribeError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(SubscribeError::Closed);
        }

        let mut clients = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let max = self.inner.config.max_clients.max(1);
        if clients.len() >= max {
            drop(clients);
            self.inner
                .stats
                .clients_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(SubscribeError::TooManyClients { max });
        }

        let id = self.inner.next_client_id.fetch_add(1, Ordering::SeqCst) + 1;
        let queue = Arc::new(ClientQueue::new());
        let control = Arc::new(ClientControl::new());
        clients.insert(
            id,
            ClientEntry {
                queue: queue.clone(),
                queue_capacity: capacity.max(1),
                control: control.clone(),
                last_commands: Mutex::new(None),
                next_request_id: AtomicU64::new(0),
                active_session: Mutex::new(None),
            },
        );

        Ok(BridgeSubscriber {
            id,
            queue,
            control,
            inner: self.inner.clone(),
        })
    }

    /// Remove a client (normally done by dropping its [`BridgeSubscriber`]).
    pub fn disconnect_client(&self, client_id: u64) {
        self.inner
            .drop_client(client_id, CloseReason::ClientDropped);
    }

    /// Bind a client to a session without a client-side `session-select` line
    /// (used after `session-spawn` when the host has created the new session).
    pub fn set_client_session(&self, client_id: u64, session_id: Option<String>) {
        let clients = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = clients.get(&client_id) {
            *entry
                .active_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = session_id;
        }
    }

    pub fn client_count(&self) -> usize {
        self.inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn stats(&self) -> BridgeStats {
        let mut stats = self.inner.stats.snapshot();
        stats.queued_bytes = self.inner.queued_bytes.load(Ordering::Relaxed) as u64;
        stats
    }

    /// Map one host event and fan it out. Public so a host can publish without
    /// the channel; `start`'s sender is the normal path.
    pub fn dispatch_host_event(&self, event: Value) {
        if self.inner.closed.load(Ordering::Acquire) {
            return;
        }
        self.inner.stats.host_events.fetch_add(1, Ordering::Relaxed);

        match map_host_event(&event, &self.inner.config) {
            Some(MappedEvent::Line(envelope)) => {
                self.inner
                    .stats
                    .mapped_events
                    .fetch_add(1, Ordering::Relaxed);
                let class = classify_line(&envelope);
                self.broadcast_line(envelope.to_string(), class);
            }
            Some(MappedEvent::Snapshot {
                worker_id,
                envelope,
                commands,
            }) => {
                self.inner
                    .stats
                    .mapped_events
                    .fetch_add(1, Ordering::Relaxed);
                self.broadcast_snapshot(worker_id, envelope, commands);
            }
            None => {
                self.inner
                    .stats
                    .ignored_events
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn broadcast_line(&self, line: String, class: LineClass) {
        let mut dropped = Vec::new();
        {
            let clients = self
                .inner
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, entry) in clients.iter() {
                let outcome = entry.queue.push(
                    line.clone(),
                    class,
                    entry.queue_capacity,
                    self.inner.config.client_queue_bytes,
                    &self.inner.queued_bytes,
                    self.inner.config.total_queue_bytes,
                );
                self.inner.account_outcome(outcome);
                if matches!(outcome, EnqueueOutcome::OverBudget) {
                    dropped.push(*id);
                }
            }
        }
        let queued = self.inner.queued_bytes.load(Ordering::Relaxed);
        self.inner
            .stats
            .peak_queued_bytes
            .fetch_max(queued, Ordering::Relaxed);
        for id in dropped {
            self.inner.drop_client(id, CloseReason::SlowConsumer);
        }
    }

    fn broadcast_snapshot(&self, worker_id: String, envelope: Value, commands: Option<Value>) {
        let Some(commands) = commands else {
            self.broadcast_line(envelope.to_string(), LineClass::Terminal);
            return;
        };

        let signature = commands.to_string();
        let mut dropped = Vec::new();
        {
            let clients = self
                .inner
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, entry) in clients.iter() {
                let include_commands = {
                    let mut last = entry
                        .last_commands
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let unchanged = last.as_ref().is_some_and(|(worker, previous)| {
                        worker == &worker_id && previous == &signature
                    });
                    if !unchanged {
                        *last = Some((worker_id.clone(), signature.clone()));
                    }
                    !unchanged
                };

                let line = if include_commands {
                    let mut with_commands = envelope.clone();
                    if let Some(object) = with_commands.as_object_mut() {
                        object.insert("commands".to_owned(), commands.clone());
                    }
                    with_commands.to_string()
                } else {
                    envelope.to_string()
                };

                let outcome = entry.queue.push(
                    line,
                    LineClass::Terminal,
                    entry.queue_capacity,
                    self.inner.config.client_queue_bytes,
                    &self.inner.queued_bytes,
                    self.inner.config.total_queue_bytes,
                );
                self.inner.account_outcome(outcome);
                if matches!(outcome, EnqueueOutcome::OverBudget) {
                    dropped.push(*id);
                }
            }
        }
        let queued = self.inner.queued_bytes.load(Ordering::Relaxed);
        self.inner
            .stats
            .peak_queued_bytes
            .fetch_max(queued, Ordering::Relaxed);
        for id in dropped {
            self.inner.drop_client(id, CloseReason::SlowConsumer);
        }
    }

    /// Handle one inbound WebSocket text frame. Returns `false` when the client
    /// is unknown (for example after it was dropped as slow).
    ///
    /// Parse/validation errors are reported to the client as
    /// `server/client-error`, matching the JS server. Host errors become a
    /// failed `response` for commands that expect one.
    pub async fn handle_client_line(&self, client_id: u64, line: &str) -> bool {
        if self.inner.closed.load(Ordering::Acquire) {
            return false;
        }

        let next_request_id = {
            let clients = self
                .inner
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match clients.get(&client_id) {
                Some(entry) => entry.next_request_id.fetch_add(1, Ordering::Relaxed) + 1,
                None => return false,
            }
        };

        let parsed: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                self.send_client_error(client_id, &format!("Invalid JSON from client: {error}"));
                return true;
            }
        };

        let command = match parse_client_message(&parsed) {
            Ok(command) => command,
            Err(error) => {
                self.send_client_error(client_id, &error);
                return true;
            }
        };

        if let PhoneCommand::SessionSelect { session_id } = &command {
            let clients = self
                .inner
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = clients.get(&client_id) {
                *entry
                    .active_session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id.clone());
            }
        }

        let active_session_id = {
            let clients = self
                .inner
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            clients.get(&client_id).and_then(|entry| {
                entry
                    .active_session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
        };

        let request_id =
            client_request_id(&parsed).unwrap_or_else(|| format!("cli-{next_request_id}"));

        self.inner.stats.commands.fetch_add(1, Ordering::Relaxed);
        let forwarded = ForwardedCommand {
            command: command.clone(),
            request_id: Some(request_id.clone()),
            client_id,
            active_session_id,
        };

        match (self.inner.host.dispatch(forwarded)).await {
            Ok(reply) => {
                for event in reply.broadcast {
                    self.dispatch_host_event(event);
                }

                if command.expects_response() || reply.data.is_some() {
                    let mut payload = Map::new();
                    payload.insert("type".to_owned(), Value::String("response".to_owned()));
                    payload.insert("id".to_owned(), Value::String(request_id));
                    payload.insert(
                        "command".to_owned(),
                        Value::String(command.wire_name().to_owned()),
                    );
                    payload.insert("success".to_owned(), Value::Bool(true));
                    if let Some(data) = reply.data {
                        payload.insert("data".to_owned(), data);
                    }
                    let envelope = json!({
                        "channel": "rpc",
                        "payload": Value::Object(payload),
                    });
                    self.inner
                        .send_to_client(client_id, envelope.to_string(), LineClass::Terminal);
                }
            }
            Err(error) => {
                self.inner
                    .stats
                    .command_errors
                    .fetch_add(1, Ordering::Relaxed);
                if command.expects_response() {
                    let envelope = json!({
                        "channel": "rpc",
                        "payload": {
                            "type": "response",
                            "id": request_id,
                            "command": command.wire_name(),
                            "success": false,
                            "error": error,
                        },
                    });
                    self.inner
                        .send_to_client(client_id, envelope.to_string(), LineClass::Terminal);
                } else {
                    self.send_client_error(client_id, &error);
                }
            }
        }

        true
    }

    fn send_client_error(&self, client_id: u64, message: &str) {
        let envelope = server_envelope("client-error", json!({ "message": message }));
        self.inner
            .send_to_client(client_id, envelope.to_string(), LineClass::Terminal);
    }

    /// Stop accepting events and close every subscriber with
    /// [`CloseReason::ServerClosed`].
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let ids: Vec<u64> = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.inner.drop_client(id, CloseReason::ServerClosed);
        }
        let _ = self.inner.wake_tx.try_send(());
    }
}

// ---------------------------------------------------------------------------
// Host event mapping
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub enum MappedEvent {
    Line(Value),
    Snapshot {
        worker_id: String,
        /// Envelope without `commands`; re-added per client when needed.
        envelope: Value,
        commands: Option<Value>,
    },
}

fn server_envelope(event: &str, data: Value) -> Value {
    json!({ "channel": "server", "event": event, "data": data })
}

fn rpc_envelope(event: &Value, payload: Value) -> Value {
    let mut envelope = json!({ "channel": "rpc", "payload": payload });
    if let Some(worker_id) = event.get("sessionWorkerId") {
        envelope["sessionWorkerId"] = worker_id.clone();
    }
    envelope
}

fn data_or_rest(event: &Value) -> Value {
    event.get("data").cloned().unwrap_or_else(|| {
        let mut object = event.as_object().cloned().unwrap_or_default();
        object.remove("type");
        object.remove("sessionWorkerId");
        Value::Object(object)
    })
}

fn normalize_snapshot(event: &Value, config: &BridgeConfig) -> MappedEvent {
    let mut envelope = event.as_object().cloned().unwrap_or_default();
    envelope.remove("type");
    envelope.insert("channel".to_owned(), Value::String("snapshot".to_owned()));

    let worker_id = envelope
        .get("sessionWorkerId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "parent-session".to_owned());
    envelope.insert(
        "sessionWorkerId".to_owned(),
        Value::String(worker_id.clone()),
    );

    let commands = envelope.remove("commands").filter(|value| !value.is_null());
    let total = envelope
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let window = config.snapshot_message_window.max(1);

    if total > window {
        if let Some(messages) = envelope.get("messages").and_then(Value::as_array) {
            envelope.insert(
                "messages".to_owned(),
                Value::Array(messages[total - window..].to_vec()),
            );
        }
        envelope.insert("historyTruncated".to_owned(), Value::Bool(true));
        envelope.insert("totalMessageCount".to_owned(), json!(total));
    } else {
        envelope.remove("historyTruncated");
        envelope.remove("totalMessageCount");
    }

    MappedEvent::Snapshot {
        worker_id,
        envelope: Value::Object(envelope),
        commands,
    }
}

fn normalize_earlier(event: &Value, config: &BridgeConfig) -> Value {
    let mut envelope = event.as_object().cloned().unwrap_or_default();
    envelope.remove("type");
    envelope.insert("channel".to_owned(), Value::String("snapshot".to_owned()));
    envelope.insert(
        "event".to_owned(),
        Value::String("earlier-messages".to_owned()),
    );
    if !envelope.contains_key("sessionWorkerId") {
        envelope.insert(
            "sessionWorkerId".to_owned(),
            Value::String("parent-session".to_owned()),
        );
    }

    let max = config.earlier_page_max.max(1);
    if let Some(messages) = envelope.get("messages").and_then(Value::as_array) {
        if messages.len() > max {
            let skip = messages.len() - max;
            envelope.insert(
                "messages".to_owned(),
                Value::Array(messages[skip..].to_vec()),
            );
            if let Some(start) = envelope.get("startIndex").and_then(Value::as_u64) {
                envelope.insert("startIndex".to_owned(), json!(start + skip as u64));
            }
        }
    }

    Value::Object(envelope)
}

fn slim_message_update(event: &Value) -> Value {
    let mut payload = json!({ "type": "message_update" });
    if let Some(message) = event.get("message") {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            payload["message"] = json!({
                "role": "assistant",
                "timestamp": message.get("timestamp").cloned().unwrap_or(Value::Null),
            });
        } else {
            payload["message"] = message.clone();
        }
    }
    if let Some(update) = event.get("assistantMessageEvent") {
        payload["assistantMessageEvent"] = update.clone();
    }
    payload
}

fn trim_tool_text_parts(value: &mut Value, cap: usize) {
    let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for part in content {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        let count = text.chars().count();
        if count <= cap {
            continue;
        }
        let kept: String = text.chars().take(cap).collect();
        let dropped = count - cap;
        part["text"] = Value::String(format!(
            "{kept}\n\n… [truncated {dropped} characters for the phone view]"
        ));
    }
}

fn tool_payload(event: &Value, cap: usize) -> Value {
    let mut payload = event.clone();
    for key in ["partialResult", "result"] {
        if let Some(value) = payload.get_mut(key) {
            trim_tool_text_parts(value, cap);
        }
    }
    payload
}

#[doc(hidden)]
pub fn map_host_event(event: &Value, config: &BridgeConfig) -> Option<MappedEvent> {
    if let Some(channel) = event.get("channel").and_then(Value::as_str) {
        return match channel {
            "snapshot" => {
                if event.get("event").and_then(Value::as_str) == Some("earlier-messages") {
                    Some(MappedEvent::Line(normalize_earlier(event, config)))
                } else {
                    Some(normalize_snapshot(event, config))
                }
            }
            "sessions" | "server" | "rpc" => Some(MappedEvent::Line(event.clone())),
            _ => Some(MappedEvent::Line(event.clone())),
        };
    }

    let event_type = event.get("type").and_then(Value::as_str)?;
    let mapped = match event_type {
        "snapshot" => return Some(normalize_snapshot(event, config)),
        "earlier-messages" => {
            return Some(MappedEvent::Line(normalize_earlier(event, config)));
        }
        "status" => Some(server_envelope("status", data_or_rest(event))),
        "catalog" | "sessions" => Some(json!({
            "channel": "sessions",
            "event": "catalog",
            "data": data_or_rest(event),
        })),
        "stderr" => {
            let text = if let Some(text) = event.get("text") {
                text.clone()
            } else if let Some(data) = event.get("data") {
                data.get("text").cloned().unwrap_or_else(|| data.clone())
            } else {
                Value::String(String::new())
            };
            Some(server_envelope("stderr", json!({ "text": text })))
        }
        "reloading" => Some(server_envelope(
            "reloading",
            json!({
                "message": event
                    .get("message")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
            }),
        )),
        "agent-exit" | "agent_exit" => {
            let mut data = data_or_rest(event);
            if !data.is_object() {
                data = json!({});
            }
            Some(server_envelope("agent-exit", data))
        }
        "session-spawned" | "session_spawned" => {
            let mut data = data_or_rest(event);
            if let Some(object) = data.as_object_mut() {
                object
                    .entry("message")
                    .or_insert_with(|| Value::String("Opened new parallel session.".to_owned()));
            }
            Some(server_envelope("session-spawned", data))
        }
        "client-error" | "snapshot-error" | "startup-error" | "parse-error" => {
            let mut data = data_or_rest(event);
            if let Some(object) = data.as_object_mut() {
                object.entry("message").or_insert_with(|| {
                    Value::String("The phone bridge reported an error.".to_owned())
                });
            }
            Some(server_envelope(event_type, data))
        }
        "rpc" => Some(rpc_envelope(
            event,
            event.get("payload").cloned().unwrap_or(Value::Null),
        )),
        "message_update" => Some(rpc_envelope(event, slim_message_update(event))),
        "tool_execution_start" => Some(rpc_envelope(event, event.clone())),
        "tool_execution_update" | "tool_execution_end" => Some(rpc_envelope(
            event,
            tool_payload(event, config.tool_text_cap_chars.max(1)),
        )),
        "response"
        | "message_start"
        | "message_end"
        | "extension_ui_request"
        | "queue_update"
        | "agent_start"
        | "agent_end"
        | "compaction_start"
        | "compaction_end"
        | "auto_retry_start"
        | "auto_retry_end" => Some(rpc_envelope(event, event.clone())),
        _ => None,
    }?;
    Some(MappedEvent::Line(mapped))
}

// ---------------------------------------------------------------------------
// Parallel session pool
// ---------------------------------------------------------------------------

const CLOSE_GRACE_MARGIN_MS: u64 = 500;

/// One child event, already shaped as a phone envelope. `event` is a
/// `{"channel":...}` object (rpc/server), so it can be fed straight into
/// [`SessionBridge::dispatch_host_event`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    pub session_id: String,
    pub event: Value,
}

impl SessionEvent {
    /// Consume the wrapper and return the bridge-ready envelope.
    pub fn into_value(self) -> Value {
        self.event
    }
}

/// Pool tuning and the injectable child command. Every field is a hard bound
/// somewhere: queues, line length, caches, or lifetime.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Working directory for spawned children.
    pub cwd: PathBuf,
    /// Child binary. Defaults to the running `pir` image so a single-binary
    /// deployment works. `#[doc(hidden)]`: tests inject the test binary.
    #[doc(hidden)]
    pub executable: PathBuf,
    /// Child arguments before `--session`. Default `["--mode", "rpc"]`.
    /// `#[doc(hidden)]`: tests inject a mock child entry point.
    #[doc(hidden)]
    pub args: Vec<String>,
    /// Extra environment variables after `PI_PHONE_CHILD=1`.
    #[doc(hidden)]
    pub env: Vec<(String, String)>,
    /// Maximum live sessions. The JS pool is uncapped; this safety cap keeps
    /// the bridge/event queues bounded.
    pub max_sessions: usize,
    /// Maximum bytes of one stdout line (longer lines are reported and
    /// discarded, never buffered without bound).
    pub max_line_bytes: usize,
    /// Trailing messages retained per session for snapshots.
    pub max_cached_messages: usize,
    /// Live tool entries retained per session.
    pub max_live_tools: usize,
    /// Characters kept from one tool text part before it is forwarded.
    pub tool_text_cap_chars: usize,
    /// `SessionEvent` queue size (the server lane's receiver).
    pub event_queue_capacity: usize,
    /// Outbound JSON-line queue per child (stdin writer backpressure).
    pub write_queue_capacity: usize,
    /// Per-request response timeout.
    pub request_timeout: Duration,
    /// How long a restart waits for the new child to appear.
    pub start_timeout: Duration,
    /// How long `stop` waits after the kill signal.
    pub kill_grace: Duration,
    /// Auto-restart delay after an unexpected exit.
    pub restart_delay: Duration,
    /// Restart after an unexpected child exit while the session is alive.
    /// [`SessionPool::set_auto_restart`] can flip this at runtime (for example
    /// only while a phone client is attached).
    pub auto_restart: bool,
    /// Maximum consecutive auto-restarts before the session stays down and
    /// reports an `agent-exit`.
    pub max_restarts: u32,
    /// Upper bound for the exponential restart backoff.
    pub restart_backoff_max: Duration,
    /// A child that stayed alive at least this long resets the restart budget.
    pub restart_budget_reset: Duration,
}

impl PoolConfig {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            // An empty path makes `spawn_child` fail instead of falling back to
            // a bare `pir` name, which the Windows search path could resolve to
            // an attacker-controlled binary.
            executable: std::env::current_exe().unwrap_or_default(),
            args: vec!["--mode".to_owned(), "rpc".to_owned()],
            env: Vec::new(),
            max_sessions: 16,
            max_line_bytes: 1024 * 1024,
            max_cached_messages: 4000,
            max_live_tools: 64,
            tool_text_cap_chars: TOOL_TEXT_CAP_CHARS,
            event_queue_capacity: 1024,
            write_queue_capacity: 64,
            request_timeout: Duration::from_millis(REQUEST_TIMEOUT_MS),
            start_timeout: Duration::from_secs(5),
            kill_grace: Duration::from_millis(CHILD_KILL_GRACE_MS),
            restart_delay: Duration::from_millis(CHILD_RESTART_DELAY_MS),
            auto_restart: true,
            max_restarts: DEFAULT_MAX_RESTARTS,
            restart_backoff_max: Duration::from_secs(30),
            restart_budget_reset: Duration::from_secs(60),
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

/// Per-session (never global) pool failure. The JS pool throws; this port keeps
/// the failure attached to the session and reports it in
/// [`SessionSummary::last_error`] as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    Disposed,
    TooManySessions { max: usize },
    UnknownSession(String),
    NotRunning(String),
    Spawn(String),
    Io(String),
    Timeout { command: String, timeout_ms: u64 },
    ChildError(String),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disposed => write!(formatter, "phone session pool is disposed"),
            Self::TooManySessions { max } => {
                write!(formatter, "parallel session limit reached ({max})")
            }
            Self::UnknownSession(id) => write!(formatter, "unknown phone session: {id}"),
            Self::NotRunning(id) => write!(formatter, "pi rpc child is not running: {id}"),
            Self::Spawn(error) => write!(formatter, "failed to spawn pi rpc child: {error}"),
            Self::Io(error) => write!(formatter, "pi rpc child I/O error: {error}"),
            Self::Timeout {
                command,
                timeout_ms,
            } => write!(
                formatter,
                "timed out after {timeout_ms}ms waiting for child response to {command}"
            ),
            Self::ChildError(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PoolError {}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModel {
    pub id: String,
    pub name: Option<String>,
    pub provider: String,
}

/// Catalog row, matching the JS `SessionSummary` shape. `kind` is `"parallel"`
/// for pool children; the server lane adds the parent row.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub kind: &'static str,
    pub session_id: Option<String>,
    pub session_file: Option<String>,
    pub session_name: Option<String>,
    pub label: String,
    pub secondary_label: String,
    pub first_user_preview: Option<String>,
    pub last_user_preview: Option<String>,
    pub model: Option<SessionModel>,
    pub is_running: bool,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub message_count: u64,
    pub pending_message_count: u64,
    pub has_pending_ui_request: bool,
    pub last_error: String,
    pub last_activity_at: i64,
    pub child_pid: Option<u32>,
    pub cwd: String,
}

#[derive(Debug, Clone, Copy)]
struct StopSignal {
    restart: bool,
    quiet: bool,
    immediate: bool,
}

impl StopSignal {
    fn stop() -> Self {
        Self {
            restart: false,
            quiet: true,
            immediate: true,
        }
    }

    fn reload() -> Self {
        Self {
            restart: true,
            quiet: true,
            immediate: true,
        }
    }
}

#[derive(Default)]
struct CachedSession {
    last_state: Value,
    messages: Vec<Value>,
    commands: Vec<Value>,
    live_assistant: Option<Value>,
    live_tools: BTreeMap<String, Value>,
    pending_ui: Option<Value>,
    first_user_preview: Option<String>,
    last_user_preview: Option<String>,
    pending_queue_count: u64,
}

struct SessionHandle {
    id: String,
    cwd: PathBuf,
    config: Arc<PoolConfig>,
    events_tx: mpsc::Sender<SessionEvent>,
    pool_disposed: Arc<AtomicBool>,
    /// Back-reference so a supervisor that finally exits can remove its own
    /// row and release the session slot (`None` once the pool is gone).
    pool: Weak<PoolInner>,
    /// Runtime switch shared with [`SessionPool::set_auto_restart`].
    auto_restart: Arc<AtomicBool>,
    cache: Mutex<CachedSession>,
    last_error: Mutex<String>,
    running: AtomicBool,
    disposed: AtomicBool,
    child_pid: AtomicU32,
    generation: AtomicU64,
    last_activity_ms: AtomicI64,
    request_counter: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    out_tx: Mutex<Option<mpsc::Sender<String>>>,
    kill_tx: mpsc::Sender<StopSignal>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    dropped_events: AtomicU64,
}

impl SessionHandle {
    fn new(
        id: String,
        cwd: PathBuf,
        config: Arc<PoolConfig>,
        events_tx: mpsc::Sender<SessionEvent>,
        pool_disposed: Arc<AtomicBool>,
        pool: Weak<PoolInner>,
        auto_restart: Arc<AtomicBool>,
    ) -> (Arc<Self>, mpsc::Receiver<StopSignal>) {
        let (kill_tx, kill_rx) = mpsc::channel(4);
        let handle = Arc::new(Self {
            id,
            cwd,
            config,
            events_tx,
            pool_disposed,
            pool,
            auto_restart,
            cache: Mutex::new(CachedSession::default()),
            last_error: Mutex::new(String::new()),
            running: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            last_activity_ms: AtomicI64::new(now_ms()),
            request_counter: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            out_tx: Mutex::new(None),
            kill_tx,
            supervisor: Mutex::new(None),
            dropped_events: AtomicU64::new(0),
        });
        (handle, kill_rx)
    }

    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn set_error(&self, message: impl Into<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = message.into();
        self.touch();
    }

    fn error(&self) -> String {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn emit(&self, event: Value) {
        self.touch();
        let event = SessionEvent {
            session_id: self.id.clone(),
            event,
        };
        match self.events_tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn emit_rpc(&self, payload: Value) {
        self.emit(json!({
            "channel": "rpc",
            "sessionWorkerId": self.id,
            "payload": payload,
        }));
    }

    fn on_spawned(&self, pid: u32) {
        self.running.store(true, Ordering::Release);
        self.child_pid.store(pid, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = String::new();
        self.touch();
    }

    fn reject_pending(&self, message: &str) {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, sender) in pending {
            let _ = sender.send(Err(message.to_owned()));
        }
    }

    fn start_supervisor(
        self: &Arc<Self>,
        child: Child,
        kill_rx: mpsc::Receiver<StopSignal>,
        out_rx: mpsc::Receiver<String>,
    ) {
        let handle = self.clone();
        let join = tokio::spawn(async move {
            handle.supervise(child, kill_rx, out_rx).await;
        });
        *self
            .supervisor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(join);
    }

    async fn supervise(
        self: Arc<Self>,
        mut child: Child,
        mut kill_rx: mpsc::Receiver<StopSignal>,
        mut out_rx: mpsc::Receiver<String>,
    ) {
        let mut restart_attempts: u32 = 0;
        loop {
            let pid = child.id().unwrap_or(0);
            self.on_spawned(pid);
            let started_at = tokio::time::Instant::now();

            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            if let Some(stdout) = stdout {
                self.spawn_stdout_reader(stdout);
            }
            if let Some(stderr) = stderr {
                self.spawn_stderr_reader(stderr);
            }

            let mut stdin = stdin;
            let mut signal: Option<StopSignal> = None;
            let status = loop {
                tokio::select! {
                    line = out_rx.recv() => {
                        match line {
                            Some(line) => {
                                if let Some(pipe) = stdin.as_mut() {
                                    if pipe.write_all(line.as_bytes()).await.is_err()
                                        || pipe.write_all(b"\n").await.is_err()
                                        || pipe.flush().await.is_err()
                                    {
                                        // Broken pipe: fall through and wait for exit.
                                    }
                                }
                            }
                            None => {
                                signal = Some(StopSignal::stop());
                                kill_tree_and_child(&mut child, pid).await;
                                break child.wait().await;
                            }
                        }
                    }
                    status = child.wait() => break status,
                    incoming = kill_rx.recv() => {
                        signal = Some(incoming.unwrap_or_else(StopSignal::stop));
                        kill_tree_and_child(&mut child, pid).await;
                        break child.wait().await;
                    }
                }
            };

            *self
                .out_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            self.running.store(false, Ordering::Release);
            self.child_pid.store(0, Ordering::Relaxed);

            let signal_quiet = signal.map(|signal| signal.quiet).unwrap_or(false);
            let explicit_restart = signal.map(|signal| signal.restart).unwrap_or(false);
            let auto_restart_allowed = signal.is_none()
                && !signal_quiet
                && self.auto_restart.load(Ordering::Acquire)
                && !self.disposed.load(Ordering::Acquire)
                && !self.pool_disposed.load(Ordering::Acquire);
            // A child that stayed alive past `restart_budget_reset` is not part
            // of a crash loop: its old crash streak must be cleared BEFORE the
            // budget check, or a session that crashed once, recovered for
            // hours, and crashed again would stay down as if it had exhausted
            // its budget. (The reset used to run only when a restart had
            // already been decided, which is exactly the case the exhausted
            // budget skips.)
            if started_at.elapsed() >= self.config.restart_budget_reset {
                restart_attempts = 0;
            }
            // A crash loop must not run forever: after `max_restarts`
            // consecutive restarts the session stays down and reports it.
            let budget_exhausted =
                auto_restart_allowed && restart_attempts >= self.config.max_restarts;
            let auto_restart = auto_restart_allowed && !budget_exhausted;
            let restarting = explicit_restart || auto_restart;

            let code = status.as_ref().ok().and_then(|status| status.code());
            let mut message = match code {
                Some(code) => format!("pi rpc exited with code {code}"),
                None => "pi rpc exited".to_owned(),
            };
            if budget_exhausted {
                message = format!(
                    "{message}; restart budget exhausted after {} restart(s); staying down",
                    restart_attempts
                );
            }
            self.on_child_exit(&message, code, signal_quiet, restarting);

            if !restarting {
                break;
            }

            let immediate = signal.map(|signal| signal.immediate).unwrap_or(false);
            if auto_restart {
                restart_attempts = restart_attempts.saturating_add(1);
            }
            if !immediate {
                let delay = if auto_restart {
                    restart_delay(&self.config, restart_attempts)
                } else {
                    self.config.restart_delay
                };
                let mut cancelled = false;
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    incoming = kill_rx.recv() => {
                        // An explicit reload restarts immediately; a stop
                        // (including one during the backoff) cancels it.
                        match incoming {
                            Some(signal) if signal.restart => {}
                            _ => cancelled = true,
                        }
                    }
                }
                if cancelled {
                    break;
                }
            }
            if self.disposed.load(Ordering::Acquire) || self.pool_disposed.load(Ordering::Acquire) {
                break;
            }

            let session_file = self.session_file();
            match spawn_child(&self.config, &self.cwd, session_file.as_deref()) {
                Ok(next) => {
                    let (tx, rx) = mpsc::channel(self.config.write_queue_capacity.max(1));
                    *self
                        .out_tx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
                    out_rx = rx;
                    child = next;
                }
                Err(error) => {
                    let message = format!("pi rpc restart failed: {error}");
                    self.set_error(message.clone());
                    self.emit(server_envelope(
                        "agent-exit",
                        json!({
                            "code": Value::Null,
                            "signal": Value::Null,
                            "message": message,
                        }),
                    ));
                    break;
                }
            }
        }

        self.running.store(false, Ordering::Release);
        self.child_pid.store(0, Ordering::Relaxed);
        *self
            .out_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.clear_live_cache();
        // The supervisor is the last owner of a non-restarting session: drop
        // its catalog row and release the slot so a dead session can never
        // consume the pool cap.
        if let Some(pool) = self.pool.upgrade() {
            pool.finalize_session(&self.id);
        }
    }

    fn clear_live_cache(&self) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.live_assistant = None;
        cache.live_tools.clear();
        cache.pending_ui = None;
        if let Some(state) = cache.last_state.as_object_mut() {
            state.insert("isStreaming".to_owned(), Value::Bool(false));
        }
    }

    fn on_child_exit(&self, message: &str, code: Option<i32>, quiet: bool, restarting: bool) {
        self.clear_live_cache();

        let pending_error = if restarting {
            "Pi rpc is reloading.".to_owned()
        } else if quiet {
            "pi phone session stopped".to_owned()
        } else {
            message.to_owned()
        };
        self.reject_pending(&pending_error);

        if !quiet {
            self.set_error(message.to_owned());
            self.emit(server_envelope(
                "agent-exit",
                json!({
                    "code": code,
                    "signal": Value::Null,
                    "message": message,
                }),
            ));
        }
        self.touch();
    }

    fn spawn_stdout_reader(self: &Arc<Self>, stdout: tokio::process::ChildStdout) {
        let handle = self.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buffer = Vec::new();
            loop {
                match read_bounded_line(&mut reader, &mut buffer, handle.config.max_line_bytes)
                    .await
                {
                    Ok(Some(false)) => {
                        let line = String::from_utf8_lossy(&buffer).into_owned();
                        if !line.is_empty() {
                            handle.handle_line(&line);
                        }
                    }
                    Ok(Some(true)) => {
                        let preview = String::from_utf8_lossy(&buffer);
                        let preview: String = preview.chars().take(200).collect();
                        handle
                            .handle_parse_error(&preview, "child rpc line exceeded max_line_bytes");
                    }
                    Ok(None) => break,
                    Err(error) => {
                        handle.set_error(format!("child stdout read failed: {error}"));
                        break;
                    }
                }
            }
        });
    }

    fn spawn_stderr_reader(self: &Arc<Self>, stderr: tokio::process::ChildStderr) {
        let handle = self.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                        handle.on_stderr(&text);
                    }
                    Err(_) => break,
                }
            }
        });
    }

    fn on_stderr(&self, text: &str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.set_error(trimmed.chars().take(8 * 1024).collect::<String>());
        }
        let text: String = text.chars().take(8 * 1024).collect();
        self.emit(server_envelope("stderr", json!({ "text": text })));
    }

    fn handle_parse_error(&self, line: &str, error: &str) {
        let line: String = line.chars().take(200).collect();
        self.set_error(format!("Failed to parse child rpc output: {line}"));
        self.emit(server_envelope(
            "parse-error",
            json!({ "line": line, "error": error }),
        ));
    }

    fn handle_line(&self, line: &str) {
        self.touch();
        let mut payload: Value = match serde_json::from_str(line) {
            Ok(payload) => payload,
            Err(error) => {
                self.handle_parse_error(line, &error.to_string());
                return;
            }
        };

        if payload.get("type").and_then(Value::as_str) == Some("response") {
            let response_id = payload.get("id").and_then(Value::as_str).map(str::to_owned);

            if payload.get("success").and_then(Value::as_bool) == Some(true) {
                self.remember_response(&payload);
            }

            if let Some(response_id) = response_id {
                let sender = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&response_id);
                if let Some(sender) = sender {
                    let _ = sender.send(Ok(payload.clone()));
                }
            }

            self.emit_rpc(payload);
            return;
        }

        self.update_live(&payload);
        if matches!(
            payload.get("type").and_then(Value::as_str),
            Some("tool_execution_update" | "tool_execution_end")
        ) {
            for key in ["partialResult", "result"] {
                if let Some(value) = payload.get_mut(key) {
                    trim_tool_text_parts(value, self.config.tool_text_cap_chars.max(1));
                }
            }
        }
        self.emit_rpc(payload);
    }

    fn remember_response(&self, payload: &Value) {
        let command = payload
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = payload.get("data").cloned().unwrap_or(Value::Null);

        match command {
            "get_state" => {
                let pending = data.get("pendingMessageCount").and_then(Value::as_u64);
                let mut cache = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.last_state = data;
                if let Some(pending) = pending {
                    cache.pending_queue_count = pending;
                }
            }
            "get_messages" => {
                if let Some(messages) = data.get("messages").and_then(Value::as_array) {
                    let mut cache = self
                        .cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    cache.first_user_preview = messages
                        .iter()
                        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
                        .map(|message| content_to_preview_text(&message["content"]))
                        .filter(|preview| !preview.is_empty());
                    cache.last_user_preview = messages
                        .iter()
                        .rev()
                        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
                        .map(|message| content_to_preview_text(&message["content"]))
                        .filter(|preview| !preview.is_empty());
                    let max = self.config.max_cached_messages.max(1);
                    let start = messages.len().saturating_sub(max);
                    cache.messages = messages[start..].to_vec();
                }
            }
            "get_commands" => {
                if let Some(commands) = data.get("commands").and_then(Value::as_array) {
                    self.cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .commands = commands.clone();
                }
            }
            "new_session" | "switch_session" | "set_session_name" | "reload" => {
                let mut cache = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.live_assistant = None;
                cache.live_tools.clear();
                cache.pending_ui = None;
            }
            _ => {}
        }
    }

    fn update_live(&self, payload: &Value) {
        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match event_type {
            "agent_start" => {
                set_state_bool(&mut cache.last_state, "isStreaming", true);
            }
            "agent_end" => {
                set_state_bool(&mut cache.last_state, "isStreaming", false);
                cache.live_assistant = None;
                cache.live_tools.clear();
            }
            "message_start" | "message_update" => {
                let message = payload.get("message");
                if message
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
                {
                    cache.live_assistant = message.cloned();
                }
            }
            "message_end" => {
                if payload
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
                {
                    cache.live_assistant = None;
                }
            }
            "tool_execution_start" => {
                let tool_call_id = payload
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                cache.live_tools.insert(
                    tool_call_id.clone(),
                    json!({
                        "toolCallId": tool_call_id,
                        "toolName": payload
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("tool"),
                        "args": payload.get("args").cloned().unwrap_or_else(|| json!({})),
                        "partialResult": Value::Null,
                        "result": Value::Null,
                        "isError": false,
                        "startedAt": now_ms(),
                        "durationMs": Value::Null,
                    }),
                );
                trim_live_tools(&mut cache.live_tools, self.config.max_live_tools.max(1));
            }
            "tool_execution_update" => {
                update_live_tool(&mut cache.live_tools, payload, false);
                trim_live_tools(&mut cache.live_tools, self.config.max_live_tools.max(1));
            }
            "tool_execution_end" => {
                update_live_tool(&mut cache.live_tools, payload, true);
                trim_live_tools(&mut cache.live_tools, self.config.max_live_tools.max(1));
            }
            "extension_ui_request" => {
                let method = payload.get("method").and_then(Value::as_str);
                if matches!(method, Some("select" | "confirm" | "input" | "editor")) {
                    cache.pending_ui = Some(payload.clone());
                }
            }
            "compaction_start" => {
                set_state_bool(&mut cache.last_state, "isCompacting", true);
            }
            "compaction_end" => {
                set_state_bool(&mut cache.last_state, "isCompacting", false);
            }
            "queue_update" => {
                let steering = payload
                    .get("steering")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let follow_up = payload
                    .get("followUp")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let count = (steering + follow_up) as u64;
                cache.pending_queue_count = count;
                if let Some(state) = cache.last_state.as_object_mut() {
                    state.insert("pendingMessageCount".to_owned(), json!(count));
                }
            }
            _ => {}
        }
    }

    fn session_file(&self) -> Option<String> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_state
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
            .map(str::to_owned)
    }

    fn snapshot(&self) -> Value {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        json!({
            "state": cache.last_state.clone(),
            "messages": cache.messages.clone(),
            "commands": cache.commands.clone(),
            "liveAssistantMessage": cache.live_assistant.clone(),
            "liveTools": cache.live_tools.values().cloned().collect::<Vec<_>>(),
        })
    }

    fn snapshot_event(&self) -> Value {
        let mut snapshot = self.snapshot();
        if let Some(object) = snapshot.as_object_mut() {
            object.insert("type".to_owned(), Value::String("snapshot".to_owned()));
            object.insert("sessionWorkerId".to_owned(), Value::String(self.id.clone()));
        }
        snapshot
    }

    fn summary(&self) -> SessionSummary {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = cache.last_state.clone();
        let first_user_preview = cache.first_user_preview.clone();
        let last_user_preview = cache.last_user_preview.clone();
        let cached_message_count = cache.messages.len() as u64;
        let pending_queue_count = cache.pending_queue_count;
        let has_pending_ui_request = cache.pending_ui.is_some();
        drop(cache);

        let session_id = state
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let session_name = state
            .get("sessionName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let model = model_from_state(&state);
        let message_count = state
            .get("messageCount")
            .and_then(Value::as_u64)
            .unwrap_or(cached_message_count);
        let pending_message_count = state
            .get("pendingMessageCount")
            .and_then(Value::as_u64)
            .unwrap_or(pending_queue_count);
        let is_streaming = state
            .get("isStreaming")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_compacting = state
            .get("isCompacting")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let label = session_name
            .clone()
            .or_else(|| first_user_preview.clone())
            .unwrap_or_else(|| match &session_id {
                Some(session_id) => format!("Session {}", short_id(session_id)),
                None => format!("Session {}", short_id(&self.id)),
            });
        let secondary_label = if session_name.is_some() {
            first_user_preview
                .clone()
                .or_else(|| session_id.as_deref().map(short_id))
                .unwrap_or_default()
        } else {
            session_id.as_deref().map(short_id).unwrap_or_default()
        };

        SessionSummary {
            id: self.id.clone(),
            kind: "parallel",
            session_id,
            session_file: state
                .get("sessionFile")
                .and_then(Value::as_str)
                .filter(|file| !file.is_empty())
                .map(str::to_owned),
            session_name,
            label,
            secondary_label,
            first_user_preview,
            last_user_preview,
            model,
            is_running: self.running.load(Ordering::Acquire),
            is_streaming,
            is_compacting,
            message_count,
            pending_message_count,
            has_pending_ui_request,
            last_error: self.error(),
            last_activity_at: self.last_activity_ms.load(Ordering::Relaxed),
            child_pid: {
                let pid = self.child_pid.load(Ordering::Relaxed);
                (pid != 0).then_some(pid)
            },
            cwd: self.cwd.to_string_lossy().into_owned(),
        }
    }

    async fn write_line(&self, line: String) -> Result<(), PoolError> {
        let sender = self
            .out_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(sender) = sender else {
            return Err(PoolError::NotRunning(self.id.clone()));
        };
        sender
            .send(line)
            .await
            .map_err(|_| PoolError::NotRunning(self.id.clone()))
    }

    async fn request(&self, command: Value, timeout: Duration) -> Result<Value, PoolError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(PoolError::NotRunning(self.id.clone()));
        }

        let mut command = command;
        let object = command
            .as_object_mut()
            .ok_or_else(|| PoolError::Io("rpc command must be an object".to_owned()))?;
        if !object.contains_key("id") {
            let next = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
            object.insert("id".to_owned(), Value::String(format!("srv-{next}")));
        }
        let request_id = object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("srv-0")
            .to_owned();
        let command_name = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let line = serde_json::to_string(&command)
            .map_err(|error| PoolError::Io(format!("failed to encode rpc command: {error}")))?;

        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request_id.clone(), sender);

        if let Err(error) = self.write_line(line).await {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request_id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(payload))) => Ok(payload),
            Ok(Ok(Err(error))) => Err(PoolError::ChildError(error)),
            Ok(Err(_)) => Err(PoolError::ChildError(format!(
                "pi rpc exited while waiting for {command_name}"
            ))),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&request_id);
                Err(PoolError::Timeout {
                    command: command_name,
                    timeout_ms: timeout.as_millis() as u64,
                })
            }
        }
    }

    async fn send_command(&self, command: Value) -> Result<Option<String>, PoolError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(PoolError::NotRunning(self.id.clone()));
        }

        let mut command = command;
        let object = command
            .as_object_mut()
            .ok_or_else(|| PoolError::Io("rpc command must be an object".to_owned()))?;
        let command_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        if command_type == "extension_ui_response" {
            self.cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_ui = None;
        } else if !object.contains_key("id") {
            let next = self.request_counter.fetch_add(1, Ordering::Relaxed) + 1;
            object.insert("id".to_owned(), Value::String(format!("cli-{next}")));
        }

        let request_id = object.get("id").and_then(Value::as_str).map(str::to_owned);
        let line = serde_json::to_string(&command)
            .map_err(|error| PoolError::Io(format!("failed to encode rpc command: {error}")))?;
        self.write_line(line).await?;
        Ok(request_id)
    }

    async fn restart(&self) -> Result<(), PoolError> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(PoolError::Disposed);
        }
        let before = self.generation.load(Ordering::SeqCst);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = String::new();
        if self.kill_tx.send(StopSignal::reload()).await.is_err() {
            return Err(PoolError::NotRunning(self.id.clone()));
        }

        let deadline = tokio::time::Instant::now() + self.config.start_timeout;
        loop {
            if self.generation.load(Ordering::SeqCst) != before
                && self.running.load(Ordering::Acquire)
            {
                return Ok(());
            }
            if !self.running.load(Ordering::Acquire) {
                let error = self.error();
                if !error.is_empty() {
                    return Err(PoolError::ChildError(error));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(PoolError::Timeout {
                    command: "reload".to_owned(),
                    timeout_ms: self.config.start_timeout.as_millis() as u64,
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn shutdown(&self) {
        self.disposed.store(true, Ordering::Release);
        let _ = self.kill_tx.send(StopSignal::stop()).await;

        // Reap the supervisor: it is what owns the `Child`, runs the tree kill
        // and clears `running`. A stop during a restart backoff is caught by
        // the supervisor's `select!`, so this also cancels pending restarts.
        let grace = self.config.kill_grace + Duration::from_millis(CLOSE_GRACE_MARGIN_MS);
        let join = self
            .supervisor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut escalated = false;
        if let Some(mut join) = join {
            tokio::select! {
                _ = &mut join => {}
                _ = tokio::time::sleep(grace) => {
                    // The child ignored the hard kill (or the supervisor is
                    // wedged): drop the task, which drops the `Child` and
                    // trips `kill_on_drop`, then report the escalation.
                    join.abort();
                    let _ = join.await;
                    escalated = true;
                }
            }
        }

        self.running.store(false, Ordering::Release);
        self.child_pid.store(0, Ordering::Relaxed);
        *self
            .out_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.clear_live_cache();
        self.reject_pending("pi phone session stopped");

        if escalated {
            let message = format!(
                "pi rpc child did not exit within {}ms of the kill signal; supervisor aborted",
                grace.as_millis()
            );
            self.set_error(message.clone());
            self.emit(server_envelope(
                "agent-exit",
                json!({
                    "code": Value::Null,
                    "signal": Value::Null,
                    "message": message,
                }),
            ));
        }
    }

    fn begin_stop(&self) {
        self.disposed.store(true, Ordering::Release);
        // `Drop` cannot await the supervisor, so kill the tree synchronously
        // before waking it; `kill_on_drop` remains the last-resort backstop.
        let pid = self.child_pid.load(Ordering::Relaxed);
        if pid != 0 {
            kill_process_tree(pid);
        }
        let _ = self.kill_tx.try_send(StopSignal::stop());
    }
}

fn set_state_bool(state: &mut Value, key: &str, value: bool) {
    if let Some(object) = state.as_object_mut() {
        object.insert(key.to_owned(), Value::Bool(value));
    }
}

fn update_live_tool(tools: &mut BTreeMap<String, Value>, payload: &Value, finished: bool) {
    let tool_call_id = payload
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if tool_call_id.is_empty() {
        return;
    }

    let current = tools
        .get(&tool_call_id)
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut next = current.as_object().cloned().unwrap_or_default();
    next.insert("toolCallId".to_owned(), Value::String(tool_call_id.clone()));
    if let Some(tool_name) = payload.get("toolName") {
        next.insert("toolName".to_owned(), tool_name.clone());
    }
    if let Some(args) = payload.get("args") {
        next.insert("args".to_owned(), args.clone());
    }
    if let Some(partial) = payload.get("partialResult") {
        next.insert("partialResult".to_owned(), partial.clone());
    }
    if finished {
        if let Some(result) = payload.get("result") {
            next.insert("result".to_owned(), result.clone());
        }
        next.insert(
            "isError".to_owned(),
            payload
                .get("isError")
                .cloned()
                .unwrap_or(Value::Bool(false)),
        );
        let duration = next
            .get("startedAt")
            .and_then(Value::as_i64)
            .map(|started| now_ms().saturating_sub(started).max(0));
        next.insert(
            "durationMs".to_owned(),
            duration.map_or(Value::Null, |duration| json!(duration)),
        );
    } else {
        next.entry("result".to_owned()).or_insert(Value::Null);
        next.entry("isError".to_owned())
            .or_insert(Value::Bool(false));
        next.entry("durationMs".to_owned()).or_insert(Value::Null);
    }

    tools.insert(tool_call_id, Value::Object(next));
}

fn trim_live_tools(tools: &mut BTreeMap<String, Value>, max: usize) {
    while tools.len() > max {
        let Some(first) = tools.keys().next().cloned() else {
            break;
        };
        tools.remove(&first);
    }
}

fn model_from_state(state: &Value) -> Option<SessionModel> {
    let model = state.get("model")?.as_object()?;
    Some(SessionModel {
        id: model.get("id")?.as_str()?.to_owned(),
        name: model.get("name").and_then(Value::as_str).map(str::to_owned),
        provider: model
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn content_to_preview_text(content: &Value) -> String {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                Some("image") => "[image]".to_owned(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

struct PoolInner {
    config: Arc<PoolConfig>,
    sessions: Mutex<BTreeMap<String, Arc<SessionHandle>>>,
    events_tx: mpsc::Sender<SessionEvent>,
    next_id: AtomicU64,
    /// Reserved slots (live handles plus in-flight spawns). Released exactly
    /// once per reservation: on spawn failure, `stop`, or supervisor exit.
    session_count: AtomicUsize,
    disposed: Arc<AtomicBool>,
    /// Runtime auto-restart switch shared with every handle.
    auto_restart: Arc<AtomicBool>,
}

impl PoolInner {
    /// Called by a supervisor that will not restart again: drop the catalog
    /// row (if the pool still owns it) and release its slot.
    fn finalize_session(&self, session_id: &str) {
        let removed = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id)
            .is_some();
        if removed {
            self.release_slot();
        }
    }

    fn release_slot(&self) {
        let _ = self
            .session_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(count.saturating_sub(1))
            });
    }
}

/// Pool of parallel `pir --mode rpc` child sessions.
///
/// Drop kills every child (best effort; `kill_on_drop` is the backstop).
/// Explicit [`SessionPool::stop`] / [`SessionPool::shutdown`] await the kill.
pub struct SessionPool {
    inner: Arc<PoolInner>,
}

impl SessionPool {
    /// Create the pool and its bounded event receiver.
    pub fn new(config: PoolConfig) -> (Self, mpsc::Receiver<SessionEvent>) {
        let (events_tx, events_rx) = mpsc::channel(config.event_queue_capacity.max(1));
        let auto_restart = Arc::new(AtomicBool::new(config.auto_restart));
        let inner = Arc::new(PoolInner {
            config: Arc::new(config),
            sessions: Mutex::new(BTreeMap::new()),
            events_tx,
            next_id: AtomicU64::new(0),
            session_count: AtomicUsize::new(0),
            disposed: Arc::new(AtomicBool::new(false)),
            auto_restart,
        });
        (Self { inner }, events_rx)
    }

    /// Spawn a session in the pool's default cwd.
    pub fn spawn(&self, session_file: Option<String>) -> Result<String, PoolError> {
        let cwd = self.inner.config.cwd.clone();
        self.spawn_in(cwd, session_file)
    }

    /// Spawn a session in an explicit cwd. Must be called inside a Tokio
    /// runtime: the pool starts reader/supervisor tasks immediately.
    pub fn spawn_in(
        &self,
        cwd: PathBuf,
        session_file: Option<String>,
    ) -> Result<String, PoolError> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(PoolError::Disposed);
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(PoolError::Spawn(
                "SessionPool::spawn requires a Tokio runtime".to_owned(),
            ));
        }

        let max = self.inner.config.max_sessions.max(1);
        let count = self.inner.session_count.fetch_add(1, Ordering::SeqCst) + 1;
        if count > max {
            self.inner.session_count.fetch_sub(1, Ordering::SeqCst);
            return Err(PoolError::TooManySessions { max });
        }

        let id = format!(
            "active-session-{}",
            self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1
        );
        let (handle, kill_rx) = SessionHandle::new(
            id.clone(),
            cwd.clone(),
            self.inner.config.clone(),
            self.inner.events_tx.clone(),
            self.inner.disposed.clone(),
            Arc::downgrade(&self.inner),
            self.inner.auto_restart.clone(),
        );

        let child = match spawn_child(&self.inner.config, &cwd, session_file.as_deref()) {
            Ok(child) => child,
            Err(error) => {
                self.inner.release_slot();
                return Err(PoolError::Spawn(error.to_string()));
            }
        };

        handle.on_spawned(child.id().unwrap_or(0));
        let (out_tx, out_rx) = mpsc::channel(self.inner.config.write_queue_capacity.max(1));
        *handle
            .out_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(out_tx);
        // Register before spawning the supervisor so an immediate child exit
        // can always find (and remove) its row.
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), handle.clone());
        handle.start_supervisor(child, kill_rx, out_rx);

        // A concurrent `shutdown` may have flipped `disposed` after the check
        // above; do not leave an orphan handle/slot behind.
        if self.inner.disposed.load(Ordering::Acquire) {
            self.inner.finalize_session(&id);
            handle.begin_stop();
            return Err(PoolError::Disposed);
        }

        Ok(id)
    }

    /// Stop one session and remove it from the catalog.
    pub async fn stop(&self, session_id: &str) -> Result<(), PoolError> {
        let handle = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id)
            .ok_or_else(|| PoolError::UnknownSession(session_id.to_owned()))?;
        self.inner.release_slot();
        handle.shutdown().await;
        Ok(())
    }

    /// Stop every session.
    pub async fn shutdown(&self) {
        self.inner.disposed.store(true, Ordering::Release);
        let handles: Vec<Arc<SessionHandle>> = std::mem::take(
            &mut *self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_values()
        .collect();
        for handle in &handles {
            handle.shutdown().await;
        }
        self.inner.session_count.store(0, Ordering::SeqCst);
    }

    /// Reserved pool slots: live sessions plus in-flight spawns. A dead child
    /// no longer counts once its supervisor finalizes the row.
    pub fn session_count(&self) -> usize {
        self.inner.session_count.load(Ordering::SeqCst)
    }

    /// Enable or disable auto-restart for every session (and future ones).
    /// The server lane can call this with the number of attached phone
    /// clients so an empty phone UI never keeps a crash loop alive.
    pub fn set_auto_restart(&self, enabled: bool) {
        self.inner.auto_restart.store(enabled, Ordering::Release);
    }

    /// Catalog rows, most recently active first (JS `sortedWorkers`).
    pub fn list(&self) -> Vec<SessionSummary> {
        let handles: Vec<Arc<SessionHandle>> = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        let mut summaries: Vec<SessionSummary> =
            handles.iter().map(|handle| handle.summary()).collect();
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_activity_at));
        summaries
    }

    pub fn session(&self, session_id: &str) -> Option<SessionSummary> {
        self.handle(session_id).ok().map(|handle| handle.summary())
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.handle(session_id)
            .map(|handle| handle.running.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Cached snapshot in the JS `SessionSnapshot` shape.
    pub fn snapshot(&self, session_id: &str) -> Option<Value> {
        self.handle(session_id).ok().map(|handle| handle.snapshot())
    }

    /// Snapshot wrapped as a host event, ready for
    /// [`SessionBridge::dispatch_host_event`].
    pub fn snapshot_event(&self, session_id: &str) -> Option<Value> {
        self.handle(session_id)
            .ok()
            .map(|handle| handle.snapshot_event())
    }

    /// JSON-lines request with response correlation (`srv-N` ids, timeout).
    pub async fn request(
        &self,
        session_id: &str,
        command: Value,
        timeout: Duration,
    ) -> Result<Value, PoolError> {
        let handle = self.handle(session_id)?;
        handle.request(command, timeout).await
    }

    /// Fire-and-forget client command (`cli-N` ids, `extension_ui_response`
    /// handled locally).
    pub async fn send_command(
        &self,
        session_id: &str,
        command: Value,
    ) -> Result<Option<String>, PoolError> {
        let handle = self.handle(session_id)?;
        handle.send_command(command).await
    }

    /// Refresh the cached `get_state` / `get_messages` / `get_commands`
    /// concurrently, like the JS `refreshCachedSnapshot`.
    pub async fn refresh(&self, session_id: &str, timeout: Duration) -> Result<Value, PoolError> {
        let (state, messages, commands) = tokio::join!(
            self.request(session_id, json!({ "type": "get_state" }), timeout),
            self.request(session_id, json!({ "type": "get_messages" }), timeout),
            self.request(session_id, json!({ "type": "get_commands" }), timeout),
        );
        state?;
        messages?;
        commands?;
        self.snapshot(session_id)
            .ok_or_else(|| PoolError::UnknownSession(session_id.to_owned()))
    }

    /// Port of `PhoneSessionWorker.reload`: refuse while streaming/compacting,
    /// restart the child silently, then refresh the cache.
    pub async fn reload(&self, session_id: &str) -> Result<(), PoolError> {
        let handle = self.handle(session_id)?;
        let state = handle
            .request(
                json!({ "type": "get_state" }),
                self.inner.config.request_timeout,
            )
            .await?;
        let data = state.get("data").cloned().unwrap_or(Value::Null);
        if data.get("isStreaming").and_then(Value::as_bool) == Some(true) {
            return Err(PoolError::ChildError(
                "Wait for the current response to finish before reloading.".to_owned(),
            ));
        }
        if data.get("isCompacting").and_then(Value::as_bool) == Some(true) {
            return Err(PoolError::ChildError(
                "Wait for compaction to finish before reloading.".to_owned(),
            ));
        }

        if let Some(session_file) = data.get("sessionFile").and_then(Value::as_str) {
            if !session_file.is_empty() {
                let mut cache = handle
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(object) = cache.last_state.as_object_mut() {
                    object.insert(
                        "sessionFile".to_owned(),
                        Value::String(session_file.to_owned()),
                    );
                }
            }
        }

        handle.restart().await?;
        self.refresh(session_id, self.inner.config.request_timeout)
            .await?;
        Ok(())
    }

    /// Number of events dropped because the consumer's queue was full.
    pub fn dropped_events(&self, session_id: &str) -> u64 {
        self.handle(session_id)
            .map(|handle| handle.dropped_events.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn handle(&self, session_id: &str) -> Result<Arc<SessionHandle>, PoolError> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .cloned()
            .ok_or_else(|| PoolError::UnknownSession(session_id.to_owned()))
    }
}

impl Drop for SessionPool {
    fn drop(&mut self) {
        self.inner.disposed.store(true, Ordering::Release);
        let handles: Vec<Arc<SessionHandle>> = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for handle in handles {
            handle.begin_stop();
        }
    }
}

fn spawn_child(config: &PoolConfig, cwd: &Path, session_file: Option<&str>) -> io::Result<Child> {
    if config.executable.as_os_str().is_empty() {
        // Never fall back to a bare `pir` name: on Windows that resolves
        // through the search path (current directory included).
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "cannot resolve the pi-phone child executable (current_exe failed); \
             set PoolConfig::executable explicitly",
        ));
    }
    let mut command = Command::new(&config.executable);
    command.args(&config.args);
    if let Some(session_file) = session_file {
        command.arg("--session").arg(session_file);
    }
    command
        .current_dir(cwd)
        .env("PI_PHONE_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &config.env {
        command.env(key, value);
    }
    #[cfg(unix)]
    command.process_group(0);
    command.spawn()
}

async fn kill_tree_and_child(child: &mut Child, pid: u32) {
    if pid != 0 {
        #[cfg(windows)]
        {
            // `taskkill` blocks; keep it off the Tokio worker that drives the
            // session supervisor.
            let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
        }
        #[cfg(not(windows))]
        {
            kill_process_tree(pid);
        }
    }
    let _ = child.start_kill();
}

fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // Trusted absolute path (never a bare name from PATH / the current
        // directory) plus the no-default-current-directory search policy.
        let system_root = std::env::var_os("SystemRoot")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));
        let _ = std::process::Command::new(
            std::path::Path::new(&system_root)
                .join("System32")
                .join("taskkill.exe"),
        )
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .env("NoDefaultCurrentDirectoryInExePath", "1")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    }
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return;
        };
        // The child was spawned with `process_group(0)`, so its pgid is its
        // pid: SIGKILL the whole group instead of only the direct child.
        // SAFETY: `kill` takes no pointers and only acts on the pid/pgid it is
        // given. `pid` originated from `Child::id()` and both call sites skip a
        // zero pid (the supervisor's `pid != 0` guard and `begin_stop`), while
        // `pid_t::try_from` rejects values that do not fit a positive `pid_t`;
        // `pid` is therefore a live child pid and `-pid` its process group.
        // Both return values are checked, so a failed group kill falls back to
        // signalling the direct child.
        #[expect(
            unsafe_code,
            reason = "libc::kill SIGKILLs the child process group we spawned"
        )]
        unsafe {
            if libc::kill(-pid, libc::SIGKILL) != 0 {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// Exponential backoff for crash restarts: `restart_delay * 2^(attempt-1)`,
/// capped at [`PoolConfig::restart_backoff_max`].
fn restart_delay(config: &PoolConfig, attempt: u32) -> Duration {
    let factor = 1_u32 << attempt.saturating_sub(1).min(5);
    config
        .restart_delay
        .saturating_mul(factor)
        .min(config.restart_backoff_max)
}

/// Read one line with a hard byte cap. Returns `Ok(None)` at EOF.
///
/// `truncated` is true when the line was longer than `max_bytes`; the buffer
/// holds the prefix only and the rest of the line is discarded. This replaces
/// the JS unbounded `stdoutBuffer += chunk`.
#[doc(hidden)]
pub async fn read_bounded_line<R>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<Option<bool>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    buffer.clear();
    let mut truncated = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buffer.is_empty() {
                None
            } else {
                Some(truncated)
            });
        }

        let (chunk, found_newline, consumed) =
            match available.iter().position(|byte| *byte == b'\n') {
                Some(position) => (&available[..position], true, position + 1),
                None => (available, false, available.len()),
            };

        let remaining = max_bytes.saturating_sub(buffer.len());
        if remaining == 0 {
            truncated = true;
        } else {
            let take = chunk.len().min(remaining);
            buffer.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                truncated = true;
            }
        }

        reader.consume(consumed);

        if found_newline {
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
            return Ok(Some(truncated));
        }
    }
}
