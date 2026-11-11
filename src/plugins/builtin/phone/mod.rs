//! pi-phone: the agent-side client for the central `pir-phone-server`.
//!
//! The plugin no longer binds a local HTTP port. `/phone-start <ws(s)://server>
//! <device-token>` opens an outbound WebSocket to the central relay, registers
//! the live session, and forwards commands/events through the existing
//! [`SessionBridge`] mapping. Agent execution, files, tools, and model calls
//! stay in this process; the central server only stores user/device/session
//! metadata and relays messages.
//!
//! Module layout:
//! - `state`: remote transport state, status line, bridge bookkeeping.
//! - `remote`: outbound WebSocket client, reconnect/backoff, frame routing.
//! - `session`: [`SessionBridge`] (event mapping + bounded fan-out) and the
//!   parallel RPC session pool.
//! - `quota`: Codex quota lookup on the local agent (proxied by the relay).
//!
//! ## Registered surface
//!
//! | command | args |
//! |---|---|
//! | `phone-start` | `<ws(s)://server> [device-token] [--device-name name] [--device-id id]` |
//! | `phone-stop` | none |
//! | `phone-status` | none |
//! | `phone-token` | none (reports the device token) |
//!
//! Status key `pi-phone`: `📱 ● phone on` / `📱 ○ phone off`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use serde_json::{json, Value as JsonValue};

use crate::extensions::{CommandOutcome, ExtensionHandle};
use crate::plugins::plugin::{
    AgentPlugin, CommandBackend, CommandHandler, EventInterest, PluginEvent, PluginEventOutcome,
    PluginFuture,
};
use crate::plugins::{PluginInfo, RegistrationContext};

pub mod quota;
pub mod remote;
pub mod session;
pub mod state;

pub use quota::quota_auth_path;
pub use session::{
    BridgeConfig, BridgeStats, BridgeSubscriber, CloseReason, ForwardedCommand, HostCommands,
    HostFuture, HostReply, PhoneCommand, SessionBridge, SubscribeError,
};
pub use state::{status_line, BridgeRuntime, PhoneState, COMMAND_NAMES, STATUS_KEY};

/// Events whose payloads the plugin forwards to the bridge's host feed.
///
/// These are exactly the event names [`session::SessionBridge`] maps to phone
/// envelopes (plus RPC responses, which the host returns through
/// [`HostReply`]). Keep this list in sync with the mapping table in
/// `session.rs`; the bridge itself owns the shape normalization.
pub const PHONE_CUSTOM_EVENT_PREFIX: &str = "phone.custom.";
pub const PHONE_CUSTOM_EVENT_MAX_BYTES: usize = 64 * 1024;

pub const PHONE_HOST_EVENT_NAMES: [&str; 15] = [
    "agent_start",
    "agent_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "queue_update",
    "compaction_start",
    "compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "extension_ui_request",
    "status",
];

/// `/phone-start` description.
pub const PHONE_START_DESCRIPTION: &str = "Connect to the central phone server. Usage: /phone-start <ws(s)://server> [device-token] [--device-name name] [--device-id id]";
/// `/phone-stop` description.
pub const PHONE_STOP_DESCRIPTION: &str = "Disconnect from the central phone server";
/// `/phone-status` description.
pub const PHONE_STATUS_DESCRIPTION: &str = "Show phone server connection status";
/// `/phone-token` description.
pub const PHONE_TOKEN_DESCRIPTION: &str = "Show the current phone device token";

/// The binding slot shared by a [`PhonePlugin`] and its live-session adapter.
///
/// The host installs the adapter at session construction
/// ([`PhonePlugin::ensure_live_session_slot`]) and binds the session once it
/// is reachable as an `Arc` ([`PhonePlugin::bind_live_session`], normally
/// `AgentSession::install_phone_host`). The slot is the only bridge between
/// the two: the adapter holds a `Weak` so the session -> plugin host -> phone
/// plugin -> adapter chain cannot form a reference cycle.
#[doc(hidden)]
#[derive(Default)]
pub struct PhoneSessionSlot {
    session: StdMutex<Option<Weak<crate::agent_session::AgentSession>>>,
}

impl PhoneSessionSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Bind (or replace) the live session this slot mirrors.
    pub fn bind(&self, session: Weak<crate::agent_session::AgentSession>) {
        *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
    }

    /// The live session, when it is still alive.
    pub fn upgrade(&self) -> Option<Arc<crate::agent_session::AgentSession>> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub fn is_bound(&self) -> bool {
        self.upgrade().is_some()
    }
}

/// The production [`HostCommands`] adapter: every phone command runs against
/// the live [`crate::agent_session::AgentSession`] the host bound to the slot.
///
/// Installed by `AgentSession::new_with_plugins` (via
/// [`PhonePlugin::ensure_live_session_slot`]) and bound by
/// `AgentSession::install_phone_host`. Until a session is bound, dispatch
/// fails honestly instead of pretending the standalone placeholders are live
/// state.
pub struct LivePhoneHost {
    slot: Arc<PhoneSessionSlot>,
    pool: state::PhonePoolCell,
}

impl LivePhoneHost {
    pub fn new(slot: Arc<PhoneSessionSlot>, pool: state::PhonePoolCell) -> Self {
        Self { slot, pool }
    }
}

impl HostCommands for LivePhoneHost {
    fn dispatch(&self, forwarded: ForwardedCommand) -> HostFuture {
        let session = self.slot.upgrade();
        let pool = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            let Some(session) = session else {
                return Err(
                    "The live session is not bound to the phone host yet: this session \
                     was built without a plugin host or its phone plugin was created \
                     standalone."
                        .to_owned(),
                );
            };
            if let Some(pool) = pool {
                match &forwarded.command {
                    PhoneCommand::SessionSpawn { session_file } => {
                        let session_id = pool
                            .spawn(session_file.clone())
                            .map_err(|error| error.to_string())?;
                        session.publish_phone_catalog().await;
                        return Ok(HostReply::data(json!({
                            "sessionId": session_id,
                            "kind": "parallel",
                        }))
                        .with_broadcast(json!({
                            "type": "session-spawned",
                            "data": { "message": "Opened new parallel session." }
                        })));
                    }
                    PhoneCommand::SessionSelect { session_id } => {
                        if pool.session(session_id).is_none() {
                            return Err(format!("unknown parallel session: {session_id}"));
                        }
                        let event = pool.snapshot_event(session_id);
                        return Ok(event.map(HostReply::broadcast_event).unwrap_or_default());
                    }
                    PhoneCommand::LoadEarlierMessages { before, count }
                        if forwarded
                            .active_session_id
                            .as_deref()
                            .is_some_and(|id| id.starts_with("active-session-")) =>
                    {
                        let active = forwarded.active_session_id.as_deref().unwrap_or_default();
                        let response = pool
                            .request(
                                active,
                                json!({ "type": "get_messages" }),
                                std::time::Duration::from_secs(30),
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        let messages = response
                            .get("data")
                            .and_then(|data| data.get("messages"))
                            .and_then(JsonValue::as_array)
                            .cloned()
                            .unwrap_or_default();
                        let total = messages.len();
                        let before = before.unwrap_or(total).min(total);
                        let page_size = count.unwrap_or(200).min(200);
                        let start = before.saturating_sub(page_size);
                        let page = messages[start..before].to_vec();
                        return Ok(HostReply::broadcast_event(json!({
                            "channel": "snapshot",
                            "event": "earlier-messages",
                            "sessionWorkerId": active,
                            "messages": page,
                            "startIndex": start,
                            "hasMore": start > 0,
                            "totalMessageCount": total,
                        })));
                    }
                    PhoneCommand::Refresh
                        if forwarded
                            .active_session_id
                            .as_deref()
                            .is_some_and(|id| id.starts_with("active-session-")) =>
                    {
                        let active = forwarded.active_session_id.as_deref().unwrap_or_default();
                        pool.refresh(active, std::time::Duration::from_secs(30))
                            .await
                            .map_err(|error| error.to_string())?;
                        let event = pool.snapshot_event(active);
                        return Ok(event.map(HostReply::broadcast_event).unwrap_or_default());
                    }
                    _ => {}
                }
                if forwarded
                    .active_session_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("active-session-"))
                {
                    let active = forwarded.active_session_id.as_deref().unwrap_or_default();
                    if pool.session(active).is_none() {
                        return Err(format!("unknown parallel session: {active}"));
                    }
                    // The child speaks the raw pi RPC command shape, not the
                    // phone envelope wrapper (`{kind:"rpc", command:...}`).
                    // The vocabulary differs for the tree read: phone clients
                    // ask for `phone_get_tree`, while `pir --mode rpc` only
                    // implements `get_tree`. Translate the wire name and adapt
                    // the nested RPC tree into the phone tree payload below.
                    let phone_tree = matches!(forwarded.command, PhoneCommand::PhoneGetTree);
                    let mut child_command = forwarded
                        .raw
                        .get("command")
                        .cloned()
                        .unwrap_or_else(|| forwarded.raw.clone());
                    if phone_tree {
                        // A distinct child correlation id: the pool's reader
                        // also emits every child frame as an rpc event, so
                        // reusing the phone request id would surface the raw
                        // `get_tree` response next to the converted phone
                        // tree. The child only echoes ids it was given, so the
                        // pool's own `srv-N` assignment cannot be used here.
                        let child_id = format!(
                            "{}-get_tree",
                            forwarded.request_id.as_deref().unwrap_or("phone-tree")
                        );
                        child_command = json!({ "type": "get_tree", "id": child_id });
                    }
                    // A child dialog answer is fire-and-forget on the child
                    // side: `pir --mode rpc` resolves the `ctx.ui` promise and
                    // deliberately emits no response frame. Answer the phone
                    // synchronously instead of waiting for a reply that will
                    // never come (the parent path does the same).
                    if child_command.get("type").and_then(JsonValue::as_str)
                        == Some("extension_ui_response")
                    {
                        let answered = child_command
                            .get("id")
                            .and_then(JsonValue::as_str)
                            .is_some_and(|id| !id.is_empty());
                        pool.send_command(active, child_command)
                            .await
                            .map_err(|error| error.to_string())?;
                        return Ok(HostReply::data(json!({ "answered": answered })));
                    }
                    if forwarded.command.expects_response() {
                        let response = pool
                            .request(active, child_command, std::time::Duration::from_secs(30))
                            .await
                            .map_err(|error| error.to_string())?;
                        let data = response.get("data").cloned().unwrap_or(JsonValue::Null);
                        if phone_tree {
                            let session_file = pool
                                .session(active)
                                .and_then(|summary| summary.session_file);
                            return Ok(HostReply::data(child_tree_to_phone_tree(
                                &data,
                                session_file,
                            )));
                        }
                        return Ok(HostReply::data(data));
                    }
                    pool.send_command(active, child_command)
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(HostReply::default());
                }
            }
            session.phone_dispatch_command(forwarded).await
        })
    }
}

/// Adapt a child RPC `get_tree` payload (`{tree, leafId}`) into the phone
/// tree shape the parent host answers (`sessionFile`, `currentLeafId`,
/// `currentPathIds`, flattened `nodes`).
///
/// `pir --mode rpc` returns the nested `SessionTreeNode` forest, while phone
/// clients consume the flat rows [`crate::session_manager::SessionManager::for_each_tree_row`]
/// produces (the same rows `AgentSession::phone_dispatch_command` answers for
/// the parent). This walk is iterative because the tree nests one level per
/// session entry and a long linear session must not recurse on the stack.
///
/// `#[doc(hidden)] pub`: asserted directly by `tests/suites/phone_units.rs`.
#[doc(hidden)]
pub fn child_tree_to_phone_tree(data: &JsonValue, session_file: Option<String>) -> JsonValue {
    let leaf_id = data.get("leafId").cloned().unwrap_or(JsonValue::Null);
    let mut nodes: Vec<JsonValue> = Vec::new();
    // First-visit parent resolution, matching `for_each_tree_row`: a duplicate
    // entry id keeps the parent of its first (visited) occurrence.
    let mut parents: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    if let Some(roots) = data.get("tree").and_then(JsonValue::as_array) {
        let mut stack: Vec<(&JsonValue, usize, Option<String>)> = roots
            .iter()
            .rev()
            .map(|node| (node, 0_usize, None))
            .collect();
        while let Some((node, depth, parent)) = stack.pop() {
            let Some(id) = node
                .get("id")
                .and_then(JsonValue::as_str)
                .or_else(|| node.pointer("/entry/id").and_then(JsonValue::as_str))
            else {
                continue;
            };
            let id = id.to_owned();
            let entry = node.get("entry").unwrap_or(&JsonValue::Null);
            let kind = entry_kind_hint(entry);
            let mut summary = json!({
                "kind": kind,
                "preview": entry_preview(entry),
            });
            if let Some(role) = entry_summary_role(entry) {
                summary["role"] = JsonValue::String(role.to_owned());
            }
            let timestamp = entry
                .get("timestamp")
                .and_then(JsonValue::as_str)
                .and_then(crate::session_manager::ms_from_iso)
                .unwrap_or(0);
            let mut row = json!({
                "id": id,
                "parentId": parent,
                "type": kind,
                "depth": depth,
                "timestamp": timestamp,
                "label": node.get("label").cloned().unwrap_or(JsonValue::Null),
                "summary": summary,
            });
            if let Some(tool_name) = entry
                .pointer("/message/toolName")
                .and_then(JsonValue::as_str)
            {
                row["toolName"] = JsonValue::String(tool_name.to_owned());
            }
            nodes.push(row);
            parents.entry(id.clone()).or_insert_with(|| parent.clone());
            if let Some(children) = node.get("children").and_then(JsonValue::as_array) {
                for child in children.iter().rev() {
                    stack.push((child, depth + 1, Some(id.clone())));
                }
            }
        }
    }
    let mut current_path: Vec<JsonValue> = Vec::new();
    if let Some(leaf) = leaf_id.as_str() {
        let mut cursor = Some(leaf.to_owned());
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = cursor {
            if !seen.insert(id.clone()) {
                break; // Defensive: a malformed tree must not loop forever.
            }
            current_path.push(JsonValue::String(id.clone()));
            cursor = parents.get(&id).cloned().flatten();
        }
        current_path.reverse();
    }
    json!({
        "sessionFile": session_file,
        "currentLeafId": leaf_id,
        "currentPathIds": current_path,
        "nodes": nodes,
    })
}

/// Entry kind label matching `SessionManager::for_each_tree_row`'s
/// `TreeRowMeta::kind_hint` (messages refined by role).
fn entry_kind_hint(entry: &JsonValue) -> &'static str {
    match entry.get("type").and_then(JsonValue::as_str) {
        Some("message") => match entry.pointer("/message/role").and_then(JsonValue::as_str) {
            Some("user") => "user_message",
            Some("assistant") => "assistant_message",
            Some("toolResult") => "tool_result",
            Some("system") => "system_message",
            Some("developer") => "developer_message",
            Some("bashExecution") => "bash_execution",
            Some("custom") => "custom",
            _ => "message",
        },
        Some("session") => "session",
        Some("thinking_level_change") => "thinking_level_change",
        Some("model_change") => "model_change",
        Some("usage") => "usage",
        Some("compaction") => "compaction",
        Some("branch_summary") => "branch_summary",
        Some("custom") => "custom",
        Some("custom_message") => "custom_message",
        Some("context_edit") => "context_edit",
        Some("label") => "label",
        Some("session_info") => "session_info",
        _ => "unknown",
    }
}

/// `summary.role` the parent phone tree attaches for message rows.
fn entry_summary_role(entry: &JsonValue) -> Option<&'static str> {
    if entry.get("type").and_then(JsonValue::as_str) != Some("message") {
        return None;
    }
    match entry.pointer("/message/role").and_then(JsonValue::as_str) {
        Some("user") => Some("user"),
        Some("assistant") => Some("assistant"),
        Some("toolResult") => Some("toolResult"),
        Some("system") => Some("system"),
        Some("developer") => Some("developer"),
        _ => None,
    }
}

/// Flattened text preview matching `tree_row_preview` (message text blocks
/// concatenated, whitespace-normalized, capped at the `/tree` row length).
fn entry_preview(entry: &JsonValue) -> String {
    fn text_of_content(content: Option<&JsonValue>) -> String {
        match content {
            Some(JsonValue::String(text)) => text.clone(),
            Some(JsonValue::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
                .collect::<String>(),
            _ => String::new(),
        }
    }
    let raw = match entry.get("type").and_then(JsonValue::as_str) {
        Some("message") => match entry.pointer("/message/role").and_then(JsonValue::as_str) {
            Some("user") | Some("assistant") | Some("toolResult") => {
                text_of_content(entry.pointer("/message/content"))
            }
            Some("system") => entry
                .pointer("/message/content")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_owned(),
            Some("bashExecution") => entry
                .pointer("/message/output")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_owned(),
            Some("custom") => text_of_content(entry.pointer("/message/content")),
            _ => String::new(),
        },
        Some("compaction") => entry
            .pointer("/summary")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("branch_summary") => entry
            .pointer("/summary")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("custom_message") => match entry.pointer("/content") {
            Some(JsonValue::String(text)) => text.clone(),
            Some(JsonValue::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let normalized: String = raw
        .chars()
        .map(|character| match character {
            '\n' | '\t' => ' ',
            other => other,
        })
        .collect();
    normalized
        .trim()
        .chars()
        .take(crate::session_manager::TREE_ROW_PREVIEW_CHARS)
        .collect()
}

/// The most recent live state the host published into the plugin.
///
/// `/api/health` and the boot `server/status` frame are server-owned today, so
/// this is the host half the server lane needs to merge in (see
/// [`PhonePlugin::published_health_json`]).
#[doc(hidden)]
#[derive(Default)]
pub struct PublishedPhoneState {
    status: Option<JsonValue>,
    snapshot: Option<JsonValue>,
    catalog: Option<JsonValue>,
    session_list: Option<JsonValue>,
}

/// Shared bookkeeping captured by the plugin's command closures.
#[derive(Default)]
struct PhoneShared {
    /// The live-session binding slot (installed with the adapter).
    live_session: StdMutex<Option<Arc<PhoneSessionSlot>>>,
    /// Last host-published status/snapshot/catalog/session list.
    published: StdMutex<PublishedPhoneState>,
    /// `true` once a phone write took ownership while idle.
    phone_owner: AtomicBool,
    /// A phone write happened since the last run boundary. A write DURING a
    /// CLI run (abort, steer, model change) keeps ownership on the phone when
    /// that run settles.
    phone_dirty: AtomicBool,
    /// Phone-initiated runs currently in flight (a counter so a settle can
    /// tell whether the run it just finished belongs to the phone).
    phone_runs_active: AtomicUsize,
    /// Opt-in namespaced custom events (`PI_PHONE_CUSTOM_EVENTS`).
    custom_events: Vec<String>,
}

/// Parse `PI_PHONE_CUSTOM_EVENTS`: comma-separated namespaced event names.
///
/// Filtering, deduplication and the 32-entry cap happen in that order, and the
/// first occurrence wins, so the resulting list — and therefore the
/// `EventInterest::List` built by [`PhonePlugin::events`] — is stable for a
/// given environment value.
///
/// `#[doc(hidden)] pub`: asserted directly by `tests/suites/phone_units.rs`.
#[doc(hidden)]
pub fn parse_custom_events(value: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in value.split(',').map(str::trim) {
        if !name.starts_with(PHONE_CUSTOM_EVENT_PREFIX) || name.len() > 128 {
            continue;
        }
        if names.len() >= 32 {
            break;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    names
}

fn configured_custom_events() -> Vec<String> {
    std::env::var("PI_PHONE_CUSTOM_EVENTS")
        .ok()
        .map(|value| parse_custom_events(&value))
        .unwrap_or_default()
}

/// The builtin plugin instance.
pub struct PhonePlugin {
    info: PluginInfo,
    state: Arc<PhoneState>,
    shared: Arc<PhoneShared>,
}

impl PhonePlugin {
    /// Build the remote-only phone plugin.
    pub fn new() -> Self {
        let mut info = PluginInfo::builtin("phone");
        // Mirror the builtin table's `replaceable` flag: a user extension
        // registering a phone command must omit this builtin (upstream
        // `omitReplacedExtensions`) instead of producing `phone-start:2`.
        info.replaceable = crate::plugins::builtin::builtin_extension_def("phone")
            .is_some_and(|def| def.replaceable);
        Self {
            info,
            state: Arc::new(PhoneState::from_env()),
            shared: Arc::new(PhoneShared {
                custom_events: configured_custom_events(),
                ..PhoneShared::default()
            }),
        }
    }

    /// The status-line entry this plugin owns.
    pub fn status_entry(&self) -> (&'static str, String) {
        (STATUS_KEY, status_line(self.state.is_running()))
    }

    /// Whether the outbound central-server link is active.
    pub fn is_running(&self) -> bool {
        self.state.is_running()
    }

    /// Direct test seam: connect the outbound central-server link.
    #[doc(hidden)]
    pub async fn start_remote_for_test(&self, args: &str) -> Result<String, String> {
        self.state.start_remote_command(args).await
    }

    /// Direct test seam: disconnect the outbound central-server link.
    #[doc(hidden)]
    pub async fn stop_remote_for_test(&self) -> Result<String, String> {
        self.state.stop_remote_command().await
    }

    /// Direct test seam: whether the outbound relay handshake completed.
    #[doc(hidden)]
    pub fn remote_connected_for_test(&self) -> bool {
        self.state
            .remote_info()
            .map(|(_, connected, _)| connected)
            .unwrap_or(false)
    }

    /// Install a `HostCommands` adapter: the documented plugin/host seam.
    ///
    /// Production uses [`LivePhoneHost`], installed at session construction
    /// ([`PhonePlugin::ensure_live_session_slot`]) and bound by
    /// `AgentSession::install_phone_host`.
    #[doc(hidden)]
    pub fn set_host_commands(&self, host: Arc<dyn HostCommands>) {
        self.state.set_host_commands(host);
    }

    /// Whether a `HostCommands` adapter is installed (the production
    /// [`LivePhoneHost`] or a recording host from a test).
    #[doc(hidden)]
    pub fn has_host_commands(&self) -> bool {
        self.state.host_commands().is_some()
    }

    /// Whether the installed adapter is bound to a live session.
    #[doc(hidden)]
    pub fn has_live_session(&self) -> bool {
        self.live_session_slot().is_some_and(|slot| slot.is_bound())
    }

    /// The live-session slot, when the host installed one.
    #[doc(hidden)]
    pub fn live_session_slot(&self) -> Option<Arc<PhoneSessionSlot>> {
        self.shared
            .live_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Install the production adapter (`LivePhoneHost`) if no host adapter is
    /// installed yet, and return the binding slot.
    ///
    /// Called by `AgentSession::new_with_plugins` whenever the phone plugin is
    /// part of the plan. A host already installed through
    /// [`PhonePlugin::set_host_commands`] is never replaced.
    #[doc(hidden)]
    pub fn ensure_live_session_slot(&self) -> Arc<PhoneSessionSlot> {
        let mut slot_guard = self
            .shared
            .live_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slot_guard.as_ref() {
            return Arc::clone(slot);
        }
        let slot = PhoneSessionSlot::new();
        *slot_guard = Some(Arc::clone(&slot));
        if !self.has_host_commands() {
            self.state.set_host_commands(Arc::new(LivePhoneHost::new(
                Arc::clone(&slot),
                self.state.pool_cell(),
            )));
        }
        slot
    }

    /// Bind the live session the adapter dispatches to.
    ///
    /// Idempotent; re-binding replaces the previous session. Returns `false`
    /// when the session has no plugin host (the phone plugin cannot be part
    /// of such a session).
    #[doc(hidden)]
    pub fn bind_live_session(&self, session: Weak<crate::agent_session::AgentSession>) -> bool {
        let slot = self.ensure_live_session_slot();
        slot.bind(session);
        true
    }

    pub fn has_bridge(&self) -> bool {
        self.state.has_bridge()
    }

    /// Whether `session_id` currently has a dialog-class UI request waiting
    /// for an answer (the parent row's `hasPendingUiRequest` source).
    pub fn has_pending_ui_request(&self, session_id: &str) -> bool {
        self.state.pending_ui_request(session_id)
    }

    /// Record a state-changing phone write: while idle the phone owns control.
    pub fn note_phone_write(&self) {
        self.shared.phone_owner.store(true, Ordering::SeqCst);
        self.shared.phone_dirty.store(true, Ordering::SeqCst);
    }

    /// Wrap a phone-initiated prompt run. While the counter is non-zero, the
    /// run boundary does not hand ownership back to the CLI (the run is the
    /// phone's write).
    pub fn begin_phone_run(&self) {
        self.shared.phone_runs_active.fetch_add(1, Ordering::SeqCst);
    }

    pub fn end_phone_run(&self) {
        self.shared.phone_runs_active.fetch_sub(1, Ordering::SeqCst);
    }

    /// The run boundary: a run the phone did not start takes control back
    /// (`controlOwner` was already `cli` while it streamed; it stays `cli`
    /// after settling until the next phone write).
    pub fn note_run_started(&self) {
        if self.shared.phone_runs_active.load(Ordering::SeqCst) == 0 {
            self.shared.phone_owner.store(false, Ordering::SeqCst);
            self.shared.phone_dirty.store(false, Ordering::SeqCst);
        }
    }

    /// The run settled. With no phone-initiated run active and no phone write
    /// during the run, the CLI owns the idle session again. A phone write
    /// DURING the run (abort/steer/model change) keeps ownership on the phone.
    pub fn note_run_settled(&self) {
        if self.shared.phone_runs_active.load(Ordering::SeqCst) == 0 {
            if !self.shared.phone_dirty.swap(false, Ordering::SeqCst) {
                self.shared.phone_owner.store(false, Ordering::SeqCst);
            }
        } else {
            self.shared.phone_dirty.store(false, Ordering::SeqCst);
        }
    }

    /// The minimal ownership rule ported from the JS runtime:
    /// `cli` while a run streams, `phone` after a phone write while idle.
    pub fn control_owner(&self, streaming: bool) -> &'static str {
        if streaming {
            "cli"
        } else if self.shared.phone_owner.load(Ordering::SeqCst) {
            "phone"
        } else {
            "cli"
        }
    }

    /// Store + forward one host-owned status object.
    ///
    /// Server-owned fields (`isRunning`, `host`, `port`, `connectedClients`,
    /// `singleClientMode`) are merged here so a WebSocket `status` frame never
    /// replaces the client's status with a partial object (the JS boot status
    /// is replaced wholesale by the client).
    pub fn publish_status(&self, mut status: JsonValue) {
        if let Some(object) = status.as_object_mut() {
            let running = self.state.is_running();
            let streaming = object
                .get("isStreaming")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            object.insert("isRunning".to_owned(), json!(running));
            object.insert("serverRunning".to_owned(), json!(running));
            object.insert(
                "singleClientMode".to_owned(),
                json!(!self.state.is_remote()),
            );
            object.insert(
                "controlOwner".to_owned(),
                json!(self.control_owner(streaming)),
            );
            if let Some((server_url, connected, device_id)) = self.state.remote_info() {
                object.insert("serverUrl".to_owned(), json!(server_url));
                object.insert("isConnected".to_owned(), json!(connected));
                object.insert("deviceId".to_owned(), json!(device_id));
            }
        }
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status = Some(status.clone());
        // The server lane reads this cell for `/api/health` and the boot
        // `status` frame, so a live host replaces the static placeholders.
        self.state.set_host_status(status.clone());
        if self.state.has_bridge() {
            self.state
                .forward_host_event(json!({ "type": "status", "data": status }));
        }
    }

    /// Store + forward the catalog (live session rows).
    pub fn publish_catalog(&self, mut catalog: JsonValue) {
        if let Some(object) = catalog.as_object_mut() {
            object
                .entry("activeSessionId")
                .or_insert_with(|| json!("parent-session"));
        }
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog = Some(catalog.clone());
        self.state.set_host_catalog(catalog.clone());
        if self.state.has_bridge() {
            self.state
                .forward_host_event(json!({ "type": "catalog", "data": catalog }));
        }
    }

    /// Store + forward one snapshot envelope body (`state`/`messages`/...).
    ///
    /// Inline base64 image parts are replaced with `/api/image/<key>` URLs
    /// first (the reference `PhoneImageStore.extractImages`): a snapshot with
    /// screenshots would otherwise re-send hundreds of KB of base64 on every
    /// refresh. The snapshot is owned here, so the rewrite mutates it in place
    /// and costs one copy per image, never one per message.
    pub fn publish_snapshot(&self, snapshot: JsonValue) {
        // The central server owns the `/api/image` store; raw base64 reaches it
        // intact so it can extract and rewrite preview URLs.
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot = Some(snapshot.clone());
        self.state.set_host_snapshot(snapshot.clone());
        if self.state.has_bridge() {
            let mut event = snapshot;
            if let Some(object) = event.as_object_mut() {
                object.insert("type".to_owned(), json!("snapshot"));
            }
            self.state.forward_host_event(event);
        }
    }

    /// Store the saved-session listing (answered from `phone_list_sessions`).
    pub fn publish_session_list(&self, list: JsonValue) {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_list = Some(list);
    }

    /// Forward one agent-loop event to the bridge (no-op without a bridge).
    pub fn publish_agent_event(&self, event: &crate::agent::agent_loop::AgentEvent) {
        if !self.state.has_bridge() {
            return;
        }
        self.state
            .forward_host_event(crate::agent_session::agent_event_to_json(event));
    }

    /// Surface an out-of-band host error (a spawned run that failed after the
    /// command response was already acknowledged).
    pub fn publish_client_error(&self, message: &str) {
        if !self.state.has_bridge() {
            return;
        }
        self.state.forward_host_event(json!({
            "type": "client-error",
            "data": { "message": message },
        }));
    }

    /// Forward one session-level event (`queue_update`, `compaction_*`,
    /// `auto_retry_*`).
    pub fn publish_session_event(&self, event: &crate::agent_session::SessionEvent) {
        if !self.state.has_bridge() {
            return;
        }
        self.state
            .forward_host_event(crate::agent_session::session_event_to_json(event));
    }

    /// The last status the host published (`None` before the first publish).
    #[doc(hidden)]
    pub fn published_status(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone()
    }

    /// Whether a snapshot was ever published (the run boundary skips the
    /// expensive transcript build while no phone server is listening).
    #[doc(hidden)]
    pub fn has_published_snapshot(&self) -> bool {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .is_some()
    }

    /// The optional parallel-session pool bound to this phone plugin.
    #[doc(hidden)]
    pub fn pool(&self) -> Option<Arc<session::SessionPool>> {
        self.state.pool()
    }

    /// The last snapshot the host published.
    #[doc(hidden)]
    pub fn published_snapshot(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    /// The last catalog the host published.
    #[doc(hidden)]
    pub fn published_catalog(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog
            .clone()
    }

    /// The last saved-session listing the host published.
    #[doc(hidden)]
    pub fn published_session_list(&self) -> Option<JsonValue> {
        self.shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_list
            .clone()
    }

    /// The real status `/api/health` and the boot `server/status` frame must
    /// merge from (finding 3 of the adversarial review).
    ///
    /// When no host state was published yet, only the live server fields are
    /// reported; the server keeps its own `pid`/`host`/`port`/`hasToken`.
    #[doc(hidden)]
    pub fn published_health_json(&self) -> JsonValue {
        let status = self
            .shared
            .published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone();
        match status {
            Some(status) => status,
            None => {
                let running = self.state.is_running();
                json!({
                    "isRunning": running,
                    "serverRunning": running,
                    "singleClientMode": true,
                    "controlOwner": self.control_owner(false),
                })
            }
        }
    }
}

impl Default for PhonePlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn command_outcome(result: Result<String, String>) -> CommandOutcome {
    match result {
        Ok(text) => CommandOutcome {
            ok: true,
            error: Some(text),
        },
        Err(message) => CommandOutcome {
            ok: false,
            error: Some(message),
        },
    }
}

impl AgentPlugin for PhonePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Downcast support for the host's generic live-adapter install
    /// (`AgentSession::new_with_plugins` reaches the phone plugin through
    /// `PluginHost::plugins()` and `set_host_commands`).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    /// The live `pi-phone` entry: `📱 ● phone on` while the server runs,
    /// `📱 ○ phone off` otherwise (the reference status text — the JS face
    /// adds no Tailscale suffix to the status line; the Serve URL is reported
    /// by `/phone-status` notifications instead).
    ///
    /// The plugin has no external status state of its own, so this reflects
    /// the shared [`PhoneState`] on every poll and flips as start/stop does.
    fn statuses(&self) -> Vec<(String, String)> {
        let (key, text) = self.status_entry();
        vec![(key.to_owned(), text)]
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        let state = Arc::clone(&self.state);
        let shared = Arc::clone(&self.shared);
        let start: CommandHandler = Arc::new(move |args: &str| {
            let state = Arc::clone(&state);
            let shared = Arc::clone(&shared);
            let args = args.to_owned();
            Box::pin(async move {
                let result = state.start_remote_command(&args).await;
                if result.is_ok() {
                    // Seed the live session's status/catalog/snapshot right
                    // after the relay handshake starts.
                    let session = shared
                        .live_session
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .and_then(|slot| slot.upgrade());
                    if let Some(session) = session {
                        tokio::spawn(async move {
                            session.publish_phone_state_now().await;
                        });
                    }
                }
                command_outcome(result)
            })
        });
        ctx.register_command(
            COMMAND_NAMES[0],
            Some(PHONE_START_DESCRIPTION),
            Some(CommandBackend::Rust(start)),
        );

        let state = Arc::clone(&self.state);
        let shared = Arc::clone(&self.shared);
        let stop: CommandHandler = Arc::new(move |_args: &str| {
            let state = Arc::clone(&state);
            let shared = Arc::clone(&shared);
            Box::pin(async move {
                let stopped = state.stop_remote_command().await;
                shared.phone_owner.store(false, Ordering::SeqCst);
                command_outcome(stopped)
            })
        });
        ctx.register_command(
            COMMAND_NAMES[1],
            Some(PHONE_STOP_DESCRIPTION),
            Some(CommandBackend::Rust(stop)),
        );

        let state = Arc::clone(&self.state);
        let status: CommandHandler = Arc::new(move |_args: &str| {
            let state = Arc::clone(&state);
            Box::pin(async move { command_outcome(state.status_command().await) })
        });
        ctx.register_command(
            COMMAND_NAMES[2],
            Some(PHONE_STATUS_DESCRIPTION),
            Some(CommandBackend::Rust(status)),
        );

        let state = Arc::clone(&self.state);
        let token: CommandHandler = Arc::new(move |_args: &str| {
            let state = Arc::clone(&state);
            Box::pin(async move { command_outcome(state.token_command().await) })
        });
        ctx.register_command(
            COMMAND_NAMES[3],
            Some(PHONE_TOKEN_DESCRIPTION),
            Some(CommandBackend::Rust(token)),
        );

        // The `pi-phone` entry rides the native status channel
        // (`AgentPlugin::statuses` → `PluginHost::statuses`); the JS runtime
        // bridge in `on_runtime_attached` stays for JS-facing surfaces.
        Ok(())
    }

    fn events(&self) -> EventInterest {
        // The bridge's fixed mapping table plus the namespaced custom events
        // opted in at construction (`PI_PHONE_CUSTOM_EVENTS`). Keep this a
        // concrete list: `EventInterest::All` would make `has_handlers` true
        // for every event in the session and defeat the host's emission gates
        // for a plugin that only forwards the bridge's own table.
        let mut names: Vec<String> = PHONE_HOST_EVENT_NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        for name in &self.shared.custom_events {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
        EventInterest::List(names)
    }

    /// Forward one agent/session event into the running phone bridge.
    ///
    /// The payload is cloned and given its `type` when the emitter omitted it
    /// (`status` is the common case); shape mapping stays in `session.rs` so
    /// there is exactly one host-event -> phone-envelope table. The feed uses
    /// the bridge's bounded queue with `try_send`, so a slow phone client can
    /// never block the agent actor.
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let name = event.name.as_ref();
        let is_custom = name.starts_with(PHONE_CUSTOM_EVENT_PREFIX)
            && self
                .shared
                .custom_events
                .iter()
                .any(|allowed| allowed == name);
        let interested = PHONE_HOST_EVENT_NAMES.contains(&name) || is_custom;
        // Skip the payload clone when no bridge is listening: `message_update`
        // payloads carry the cumulative assistant message and this runs on the
        // streaming hot path.
        if interested && self.state.has_bridge() {
            if is_custom
                && serde_json::to_vec(event.payload)
                    .map(|bytes| bytes.len() > PHONE_CUSTOM_EVENT_MAX_BYTES)
                    .unwrap_or(true)
            {
                return Box::pin(async { PluginEventOutcome::None });
            }
            let mut value = event.payload.clone();
            let typed = value.get("type").and_then(JsonValue::as_str) == Some(name);
            if !typed {
                match value.as_object_mut() {
                    Some(object) => {
                        object.insert("type".to_owned(), JsonValue::String(name.to_owned()));
                    }
                    None => {
                        value = json!({ "type": name, "data": value });
                    }
                }
            }
            self.state.forward_host_event(value);
        }
        Box::pin(async { PluginEventOutcome::None })
    }

    fn on_runtime_attached(&self, js: Option<ExtensionHandle>) {
        self.state.attach_runtime(js);
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state.shutdown().await;
            Ok(())
        })
    }
}

/// The factory the builtin table names.
pub fn new_phone_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(PhonePlugin::new())
}
