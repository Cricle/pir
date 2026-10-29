//! The `mcp` builtin plugin (upstream
//! `packages/coding-agent/src/extensions/mcp/**`, ~3.5k lines):
//!
//! - server sources: `~/.pi/agent/mcp.json` + the trust-gated project
//!   `.pi/mcp.json` (via [`crate::config::mcp`]) + `pi.registerMcpServer()`
//!   calls from JS extensions (via the runtime's `mcp_server_defs` surface,
//!   mirroring the `registerProvider` pattern);
//! - lazy connections: registering the plugin never starts a server; servers
//!   connect on first use (session-start discovery, direct tool calls, or
//!   `pir mcp check`), with per-server timeouts and HTTP retry backoff;
//! - each server tool becomes a registry tool named `mcp__<server>__<tool>`
//!   (sanitized, hash-suffixed on collision) with the JSON-schema parameters
//!   from `tools/list`, sequential execution, and results mapped to pir
//!   `ToolResult` (content blocks: text/resource/embedded -> text/details);
//! - `mcp_servers_change` events are dispatched to JS extensions whenever
//!   servers register/unregister (queued at registration time and flushed by
//!   the host right after the registry rebuild that queues them; the
//!   `session_start` handler keeps a fallback flush for entries queued before
//!   the first rebuild);
//! - stderr + lifecycle events land in `<agentDir>/mcp.log`;
//! - **discovery auto-activation** (upstream `ensureDiscoveryActive`): a
//!   server configured with `codemode` exposure (the default) makes the
//!   session activate the `codemode` tool, and a `deferred` exposure makes it
//!   activate `tool_search` — otherwise the MCP tools would be callable only
//!   from tools that are themselves inactive. The activation *unions* the
//!   names into the session's active set (never replacing the user's
//!   `--tools`/`setActiveTools` choice), tracks which names this plugin added
//!   (`codemode` is removable again when no enabled server needs it; names the
//!   user chose are never removed), and is recomputed on every registry
//!   rebuild, `/reload`, `session_start`, `/mcp` changes, and the binding of a
//!   fresh session context. `autoEnableCodemode` (top-level in `mcp.json`,
//!   default `true`) suppresses the `codemode` half only.
//! - **direct-bridge activation is prunable** (upstream activates `direct`
//!   tools on registration and `hideTools`/`setExposure` remove them again):
//!   a default-tools session's direct bridges are unioned into the active set
//!   and their names are tracked as plugin-owned, so disabling a server,
//!   changing its exposure away from `direct`, or excluding a name (exact or
//!   `*` pattern) removes the bridge from the set. A disabled server's
//!   bridges are registered as `hidden` (upstream `hideTools`), so a stale
//!   set entry can neither declare nor call them. An explicit
//!   `--tools`/`--no-tools` selection is never widened or pruned.
//!
//! Documented gaps vs upstream: no aggregate resources/prompts *tools* (the
//! client protocol methods exist and `is_mcp_tool_name` already covers their
//! names; the `list_mcp_resources`-style tool set lands with the resource-tools
//! stream), and truncated output is not mirrored to a temp file. MCP OAuth is
//! implemented (`src/mcp/oauth.rs`, driven by `pir mcp login/logout` and the
//! in-session `/mcp login|logout` subcommands).
//!
//! ## `/mcp`
//!
//! Upstream's `pi.registerCommand("mcp", ...)` (index.ts:1258-1381) is ported
//! here: `login`/`logout`/`reconnect` with upstream's server-picking rules and
//! its usage string, the interactive manager (server list -> per-server action
//! menu -> exposure picker) in TUI mode, the plain `formatStatus` text in
//! print/RPC mode, upstream `getArgumentCompletions`, and the
//! `Run /mcp to fix.` startup report. Enable/disable and exposure changes to a
//! file-defined server are written through
//! [`crate::coding_agent::cli::mcp_config_upsert`] (the `pir mcp add/remove`
//! writer); extension-registered servers keep them session-only, like
//! upstream. `list`/`enable`/`disable`/`remove` are pir extras kept from the
//! earlier handler (`remove` is session-scoped; upstream has no removal).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use base64::Engine as _;
use serde_json::Value as JsonValue;

use crate::agent::tool_batch::{ToolExecutionMode, ToolResult};
use crate::agent::tool_execution::{
    AgentTool, BoxFuture, ToolContext, ToolExposure, ToolNamespace, ToolReach, ToolRegistry,
};
use crate::config::mcp::{McpExposure, McpServerConfig};
use crate::mcp::manager::{DiscoveredTool, McpManager, McpStatusSnapshot, MCP_OUTPUT_MAX_BYTES};
use crate::mcp::protocol::CallToolResult;
use crate::mcp::registry::{McpServerEntry, McpServerSource};
use crate::mcp::transport::frame_cap_label;

use crate::plugins::plugin::{
    AgentPlugin, EventInterest, PluginContext, PluginEvent, PluginEventOutcome, PluginFuture,
    RegistrationContext,
};
use crate::plugins::source::PluginInfo;

/// Maximum pages walked for one server's aggregate resource/resource-template
/// listing. A malicious or broken server that keeps returning cursors must
/// not keep the tool call running forever (the tool list has the same bound,
/// see `mcp::client::MAX_LIST_PAGES`).
const MAX_RESOURCE_LIST_PAGES: usize = 1_000;

/// Maximum bytes one aggregate resource listing may accumulate (per server
/// and, for the all-server form, per tool result). Hitting it stops the walk
/// and reports an explicit truncation error instead of growing without
/// bound.
const MAX_RESOURCE_LIST_BYTES: usize = 8 * 1024 * 1024;

/// Maximum serialized size of a structured tool result retained in
/// `structuredContent` (per result). Larger payloads are replaced by an
/// explicit truncation marker; the model-facing text keeps its own 20 KiB
/// cap (`MCP_OUTPUT_MAX_BYTES`).
const MAX_STRUCTURED_RESULT_BYTES: usize = 8 * 1024 * 1024;

/// Counts the bytes `serde_json` writes without materializing the JSON.
/// `serde_json` streams tokens into the writer, so measuring a payload costs
/// no allocation proportional to its size.
#[derive(Default)]
struct JsonByteCounter(usize);

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialized JSON size of `value`, measured without allocating the JSON.
fn serialized_len<T: serde::Serialize>(value: &T) -> Option<usize> {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.0)
}

/// The explicit marker left in `structuredContent` (and structured list
/// results) when a payload exceeds its cap. Never silently empty: the marker
/// names the cap and the original size.
fn structured_truncation_marker(original_bytes: Option<usize>, cap: usize) -> JsonValue {
    let mut marker = serde_json::json!({
        "truncated": true,
        "capBytes": cap,
        "message": format!(
            "MCP structured content exceeded the {cap}-byte ({}) cap and was omitted",
            frame_cap_label(cap)
        ),
    });
    if let Some(bytes) = original_bytes {
        marker["originalBytes"] = serde_json::json!(bytes);
    }
    marker
}

/// Keep a structured payload only when it fits the cap; otherwise substitute
/// the explicit truncation marker.
fn bounded_structured_payload(payload: JsonValue, cap: usize) -> JsonValue {
    match serialized_len(&payload) {
        Some(bytes) if bytes > cap => structured_truncation_marker(Some(bytes), cap),
        _ => payload,
    }
}

/// Append `chunk` to `text` without exceeding `cap` bytes. Returns whether
/// the whole chunk fit; a partial append stops on a UTF-8 boundary.
fn append_bounded(text: &mut String, chunk: &str, cap: usize) -> bool {
    let room = cap.saturating_sub(text.len());
    if chunk.len() <= room {
        text.push_str(chunk);
        return true;
    }
    let mut end = room;
    while end > 0 && !chunk.is_char_boundary(end) {
        end -= 1;
    }
    text.push_str(&chunk[..end]);
    false
}

/// The manager-state follow-up of one registry rebuild's server passes
/// ([`McpPlugin::sync_config_files`] / [`McpPlugin::apply_js_server_defs`]):
/// which servers the manager must act on at the rebuild tail, because their
/// entry was added, changed, or removed since the previous rebuild.
///
/// Drained by [`McpPlugin::reconcile_config_servers`], which the host runs at
/// the rebuild's enqueue point ([`McpPlugin::flush_server_changes`]).
#[derive(Default)]
struct ConfigServerFollowUp {
    /// Entries added or changed: their live connection was created from the
    /// OLD entry, so it is released and the server re-discovered, publishing
    /// the tools of the current entry. Only an entry that REPLACES one the
    /// manager may already be serving queues an add (the config pass's
    /// mid-session file entry, the JS pass's re-registration of a name this
    /// rebuild removed or that a live connection was created from); a
    /// brand-new name is the session's initial configuration for its source
    /// and rides the session-start/lazy-wait discovery instead, so
    /// registration never starts a server.
    reconnected: BTreeSet<String>,
    /// Entries removed: their live connection is released (the removed
    /// entry's process must not outlive it, and a re-added entry must connect
    /// from its own config, never the removed one's).
    removed: BTreeSet<String>,
}

impl ConfigServerFollowUp {
    /// Queue one added/changed entry (the pass replaced its registry entry).
    fn mark_reconnected(&mut self, server: &str) {
        self.removed.remove(server);
        self.reconnected.insert(server.to_owned());
    }

    /// Queue one removed entry. A name a LATER pass re-added moves back to
    /// `reconnected` (release + re-discovery covers the re-add).
    fn mark_removed(&mut self, server: &str) {
        self.reconnected.remove(server);
        self.removed.insert(server.to_owned());
    }

    /// Absorb the follow-up of a later pass in the same rebuild (a rebuild
    /// runs the config pass and the JS-def pass); the last action per name
    /// wins.
    fn merge(&mut self, other: Self) {
        for server in other.reconnected {
            self.mark_reconnected(&server);
        }
        for server in other.removed {
            self.mark_removed(&server);
        }
    }
}

/// Who put one discovery tool (`codemode` / `tool_search`) into the session's
/// active set.
///
/// Upstream writes `pi.setActiveTools([...active, ...activate])` and never
/// removes anything; pir can outlive the reason for an activation (a server
/// removed or disabled mid-session), so it tracks ownership to remove **only**
/// the names it added. A name that was already active when this plugin first
/// looked — an explicit `--tools codemode`, a JS `setActiveTools`, a user's
/// `tool_search` load — is `External` and is never removed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DiscoveryToolOwnership {
    /// Not active, or not yet observed.
    #[default]
    Unowned,
    /// Added by this plugin's auto-activation.
    Owned,
    /// Already active before auto-activation; belongs to the user/another
    /// extension.
    External,
}

impl DiscoveryToolOwnership {
    /// The decision for one name: record ownership and update `next` in place.
    ///
    /// `desired` is the upstream gate (an enabled server needs the tool, the
    /// tool is registered with a reach, and — for codemode — the config flag
    /// allows it). Present-but-not-owned becomes `External` on first sight and
    /// stays there; desired-and-absent is appended and owned; no-longer-desired
    /// owned names are removed.
    fn reconcile(&mut self, next: &mut Vec<String>, name: &str, desired: bool) {
        let present = next.iter().any(|active| active == name);
        if present && *self != Self::Owned {
            *self = Self::External;
        }
        if desired && !present {
            next.push(name.to_owned());
            *self = Self::Owned;
        } else if !desired && *self == Self::Owned {
            if present {
                next.retain(|active| active != name);
            }
            *self = Self::Unowned;
        }
    }
}

/// Per-name ownership of the two discovery tools.
#[derive(Debug, Default)]
struct DiscoveryOwnership {
    codemode: DiscoveryToolOwnership,
    tool_search: DiscoveryToolOwnership,
}

/// An in-session `/mcp` override. Applied by
/// [`McpPlugin::sync_config_files`] and [`McpPlugin::apply_js_server_defs`] on
/// every registry rebuild, so a change is not undone by the next pass.
///
/// Config-file entries persist their enable/disable and exposure changes to
/// the defining `mcp.json` (through the same writer `pir mcp add/remove`
/// uses), so they need no override; the override carries the *session-only*
/// changes: extension-registered servers (which have no file) and the
/// `/mcp remove` action (pir's session-scoped removal; upstream has no
/// remove action).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ServerOverride {
    /// Hide the entry for the rest of the session.
    removed: bool,
    /// Force the entry's `enabled` flag.
    enabled: Option<bool>,
    /// Force the entry's base exposure.
    exposure: Option<McpExposure>,
}

impl ServerOverride {
    /// Whether the entry is hidden for the session.
    fn is_removed(self) -> bool {
        self.removed
    }
}

/// The session-facing half of the plugin, shared with the `/mcp` command
/// handler.
///
/// The handler is a `'static` closure and cannot capture `&self`; this cell
/// carries everything it needs. [`PluginContext`] clones are cheap and the
/// services inside hold weak session/host handles, so storing one does not
/// keep the session alive (no cycle).
struct McpSessionState {
    /// The context bound by [`AgentPlugin::on_context`], if any.
    context: RwLock<Option<PluginContext>>,
    /// The `mcp.json` top-level `autoEnableCodemode` (default `true`).
    auto_enable_codemode: AtomicBool,
    /// The `mcp.json` top-level `builtinTools` (default `true`). When true,
    /// a server without an explicit `exposure` defaults to `direct` instead
    /// of upstream's `codemode`. This is the file-level default; entries from
    /// a legacy flat-shaped file are pinned to `codemode` through
    /// [`Self::server_builtin_tools`].
    builtin_tools: AtomicBool,
    /// Per config-file server: whether the `builtinTools` default applies.
    /// Rebuilt by every config pass. A server from a legacy flat-shaped
    /// `mcp.json` maps to `false` (that shape has no flag slot, so it keeps
    /// upstream's `codemode`); a wrapper-shaped file's servers map to the
    /// merged file-level flag. Names absent here (extension registrations)
    /// fall back to [`Self::builtin_tools`].
    server_builtin_tools: RwLock<BTreeMap<String, bool>>,
    /// Which discovery tools this plugin added to the active set.
    discovery: Mutex<DiscoveryOwnership>,
    /// Direct-exposure MCP bridges this plugin unioned into a default-tools
    /// session's active set (upstream activation on registration).
    ///
    /// The active-set cell only holds names, so after this plugin MATERIALIZES
    /// the direct bridges into it (a recompute write for the discovery tools
    /// materializes the whole set) it must remember which of those names it is
    /// responsible for: the names are pruned when the server is disabled, its
    /// exposure stops resolving to `direct`, or a default-tools exclusion
    /// matches them. Names an explicit `--tools` selection or a JS
    /// `setActiveTools` put there are never pruned (see
    /// [`McpSessionState::recompute`]).
    bridge_owned: Mutex<BTreeSet<String>>,
    /// `/mcp` session overrides, applied on every registration pass.
    overrides: RwLock<BTreeMap<String, ServerOverride>>,
    /// Extension registrations an `mcp.json` entry of the same name shadows,
    /// as upstream-shaped lines (upstream `overridden`, index.ts:288/351-354),
    /// rebuilt by every JS-def pass and surfaced through
    /// [`McpPlugin::status`] / the `/mcp` report.
    overridden: RwLock<Vec<String>>,
    /// The outcome of the latest [`AgentPlugin::on_event`] session-start
    /// discovery (upstream `waitForDirectServers` + `reportProblems`): which
    /// servers failed and which were still connecting when the aggregate
    /// budget elapsed. Retained so a status consumer or a test can see them
    /// after startup; the per-server errors themselves live on the manager's
    /// connection states and in `mcp.log`.
    startup_report: RwLock<Option<crate::mcp::manager::StartupDiscoveryReport>>,
}

impl McpSessionState {
    fn new() -> Self {
        Self {
            context: RwLock::new(None),
            auto_enable_codemode: AtomicBool::new(true),
            builtin_tools: AtomicBool::new(true),
            server_builtin_tools: RwLock::new(BTreeMap::new()),
            discovery: Mutex::new(DiscoveryOwnership::default()),
            bridge_owned: Mutex::new(BTreeSet::new()),
            overrides: RwLock::new(BTreeMap::new()),
            overridden: RwLock::new(Vec::new()),
            startup_report: RwLock::new(None),
        }
    }

    fn auto_enable_codemode(&self) -> bool {
        self.auto_enable_codemode.load(Ordering::SeqCst)
    }

    /// The effective `builtinTools` flag (default `true`): a server without
    /// an explicit `exposure` defaults to `direct`.
    fn builtin_tools(&self) -> bool {
        self.builtin_tools.load(Ordering::SeqCst)
    }

    /// Whether the `builtinTools` default applies to `server`: the per-file
    /// applicability recorded by the config pass, else the file-level flag
    /// (extension registrations have no file shape of their own).
    fn builtin_tools_for(&self, server: &str) -> bool {
        self.server_builtin_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(server)
            .copied()
            .unwrap_or_else(|| self.builtin_tools())
    }

    /// The context bound by [`AgentPlugin::on_context`], if any.
    fn context(&self) -> Option<PluginContext> {
        self.context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn override_for(&self, name: &str) -> Option<ServerOverride> {
        self.overrides
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .copied()
    }

    /// Merge one session-only change into the server's override (creating it
    /// when absent). Used for the extension-registered servers, which have no
    /// file to persist to.
    fn update_override(&self, name: &str, update: impl FnOnce(&mut ServerOverride)) {
        let mut overrides = self
            .overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let patch = overrides.entry(name.to_owned()).or_default();
        update(patch);
        let empty = !patch.removed && patch.enabled.is_none() && patch.exposure.is_none();
        if empty {
            overrides.remove(name);
        }
    }

    fn clear_override(&self, name: &str) {
        self.overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }

    /// Replace the shadowed-extension lines (upstream `overridden`),
    /// rebuilt by every JS-def pass.
    fn set_overridden(&self, lines: Vec<String>) {
        *self
            .overridden
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = lines;
    }

    /// The shadowed-extension lines of the latest JS-def pass.
    fn overridden(&self) -> Vec<String> {
        self.overridden
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Resolve a bare manager snapshot into this plugin's status surface: the
    /// per-server `builtinTools` applicability (only this cell knows that a
    /// legacy flat file's servers keep upstream's `codemode` while a wrapper
    /// file's follow the flag) and the `overridden` lines.
    fn enrich(&self, snapshot: &mut McpStatusSnapshot) {
        let applicability = self
            .server_builtin_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let fallback = self.builtin_tools();
        for server in &mut snapshot.servers {
            server.builtin_tools = applicability.get(&server.name).copied().unwrap_or(fallback);
            server.exposure = server.config.effective_exposure(server.builtin_tools);
        }
        snapshot.overridden = self.overridden();
    }

    /// Recompute and (when needed) apply the session's discovery activation.
    ///
    /// Returns the resulting active set, or `None` before a context is bound
    /// (a standalone [`crate::plugins::PluginHost`] has no session to write).
    ///
    /// Reads the **effective** active set from the bound session service, so
    /// the write is a union: `--tools`, the JS `setActiveTools` store and the
    /// session cell are all preserved. The write only happens when the set
    /// actually changes, so repeated rebuilds neither churn the loadout cache
    /// nor duplicate entries.
    ///
    /// For a **default-tools** session ([`SessionService::uses_default_tools`])
    /// the server's DIRECT-exposure bridges are unioned in as well: upstream
    /// activates `direct` tools on registration, and a JS `setActiveTools`
    /// snapshot written by an unrelated extension (an extension reload's own
    /// tool list) names only that extension's tools. Without the union the
    /// connected server's registered `mcp__*` bridges stayed inactive, so every
    /// MCP call failed with "Tool not found" until something rewrote the store.
    /// An explicit `--tools`/`--no-tools` selection is authoritative and gets
    /// nothing added.
    ///
    /// The union is PRUNABLE: the names it materializes into the cell are
    /// owned by this plugin ([`McpSessionState::bridge_owned`]) and removed
    /// again when the bridge stops qualifying — its server is disabled or
    /// removed, its configured exposure no longer resolves to `direct`
    /// (upstream `setExposure` filters exactly those out of the active set), or
    /// a default-tools exclusion matches it (upstream consults
    /// `_excludedTools` at registration time). Without the prune a stale
    /// `mcp__docs__*` name stayed DECLARED after `/mcp disable docs` and every
    /// call failed with [`crate::mcp::error::McpError::ServerDisabled`].
    fn recompute(&self, manager: &McpManager) -> Option<Vec<String>> {
        let context = self.context()?;
        let session = context.session()?;
        let tools = session.all_tools();
        let active: Vec<String> = session
            .active_tools()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let exposures: Vec<McpExposure> = manager
            .registry()
            .list()
            .into_iter()
            .filter(|entry| entry.config.is_enabled())
            .flat_map(|entry| {
                configured_exposures_with_builtin_tools(
                    &entry.config,
                    self.builtin_tools_for(&entry.name),
                )
            })
            .collect();
        let needs_codemode = exposures.contains(&McpExposure::Codemode);
        let needs_tool_search = exposures.contains(&McpExposure::Deferred);
        // Upstream never activates another extension's same-named tool: the
        // registered tool must declare a reach (`reachesTools`).
        let declares_reach = |name: &str| {
            tools
                .iter()
                .any(|tool| tool.name == name && tool.reaches_tools.is_some())
        };
        let has_codemode = declares_reach(crate::plugins::builtin::CODEMODE_TOOL_NAME);
        let has_tool_search = declares_reach(crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME);
        let mut next = active.clone();
        let mut bridge_owned = self
            .bridge_owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Direct-exposure bridges activate on registration (upstream), so a
        // default-tools session keeps them active even when an extension's
        // `setActiveTools` snapshot enumerates only its own tools. This is the
        // source-distinguishing half of the rule: an explicit `--tools` /
        // `--no-tools` selection stays exactly as chosen. The session applies
        // the same union to its resolution and run registry
        // ([`AgentSession::resolve_active_override`]), so the names written
        // here are always resolvable; writing them keeps the cell (the winning
        // store) in step with what the loadout already sees.
        if session.uses_default_tools() {
            let exclusions: Vec<String> = session
                .default_tool_exclusions()
                .iter()
                .map(|name| name.to_string())
                .collect();
            let registered: BTreeSet<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
            let registered_direct: BTreeSet<&str> = tools
                .iter()
                .filter(|tool| tool.exposure == ToolExposure::Direct.as_str())
                .map(|tool| tool.name.as_str())
                .collect();
            let eligible =
                self.direct_bridge_names(manager, &registered, &registered_direct, &exclusions);
            // Remove the bridges this plugin previously unioned when they no
            // longer qualify. A name the user/another extension placed in the
            // set is NOT owned (it was never added here), so it survives; the
            // solver's direct-bridge union
            // ([`crate::agent_session::tools::mcp_direct_tool_names_for`])
            // applies the same eligibility, so a re-read cannot resurrect the
            // pruned name either.
            bridge_owned.retain(|name| {
                if eligible.contains(name) {
                    return true;
                }
                next.retain(|active| active != name);
                false
            });
            // Keep every eligible bridge active and record ownership: the cell
            // only holds names, so this set is the plugin's memory of which
            // ones it may prune later.
            for name in eligible {
                if !next.iter().any(|active| active == &name) {
                    next.push(name.clone());
                }
                bridge_owned.insert(name);
            }
        } else {
            // An explicit selection is authoritative: nothing is added and
            // nothing is removed. Ownership from a previous default-tools
            // binding (a rebind) is released without touching the names.
            bridge_owned.clear();
        }
        drop(bridge_owned);
        {
            let mut ownership = self
                .discovery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ownership.codemode.reconcile(
                &mut next,
                crate::plugins::builtin::CODEMODE_TOOL_NAME,
                needs_codemode && self.auto_enable_codemode() && has_codemode,
            );
            ownership.tool_search.reconcile(
                &mut next,
                crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME,
                needs_tool_search && has_tool_search,
            );
        }
        // Stable set: first occurrence wins, so a duplicate already in the
        // session's set is not carried forward.
        let mut seen = BTreeSet::new();
        next.retain(|name| seen.insert(name.clone()));
        if next != active {
            let names: Vec<Arc<str>> = next.iter().map(|name| Arc::from(name.as_str())).collect();
            session.set_active_tools(&names);
        }
        Some(next)
    }

    /// The direct-exposure bridge names the manager's ENABLED servers resolve
    /// right now, restricted to the registered tool surface and the
    /// default-tools exclusions.
    ///
    /// The exposure is resolved from each live ENTRY exactly like the
    /// registration pass ([`McpServerConfig::effective_tool_exposure`] with the
    /// server's `builtinTools` default), not from the registered tool's
    /// exposure: `/mcp disable` and `/mcp setExposure` update the entry and
    /// recompute BEFORE the next registry rebuild re-registers the bridges, and
    /// pruning must happen in that same step (upstream `hideTools` /
    /// `setExposure`). The registered-name gate preserves "activation happens
    /// on registration": a discovered tool the current registry does not carry
    /// yet is not activated early.
    ///
    /// A disabled server contributes nothing even though its discovered
    /// snapshot entry survives the disconnect (disabling does not forget), and
    /// a default-tools exclusion (exact name or `*` pattern, upstream
    /// `_excludedTools`) removes a bridge discovered after the selection was
    /// resolved.
    fn direct_bridge_names(
        &self,
        manager: &McpManager,
        registered: &BTreeSet<&str>,
        registered_direct: &BTreeSet<&str>,
        exclusions: &[String],
    ) -> BTreeSet<String> {
        let mut eligible = BTreeSet::new();
        let discovered = manager.discovered_tools();
        for entry in manager.registry().list() {
            if !entry.config.is_enabled() {
                continue;
            }
            let Some(tools) = discovered.get(&entry.name) else {
                // No discovery snapshot: a bridge registered for this server
                // by ANOTHER registration path (an extension's `registerTool`,
                // the unit fixtures). Its registered `direct` exposure is the
                // only exposure fact, exactly like the namespace-based union
                // this replaces; the config cannot describe a tool it never
                // listed.
                let namespace = crate::mcp::registry::mcp_namespace(&entry.name);
                for name in registered_direct {
                    if name
                        .strip_prefix(namespace.as_str())
                        .is_some_and(|rest| rest.starts_with("__"))
                        && !crate::coding_agent::tool_selection::tool_excluded_by(exclusions, name)
                    {
                        eligible.insert((*name).to_owned());
                    }
                }
                continue;
            };
            let builtin_tools = self.builtin_tools_for(&entry.name);
            let tool_names: Vec<String> = tools.iter().map(|tool| tool.tool.name.clone()).collect();
            let assigned = crate::mcp::registry::assign_mcp_tool_names(&entry.name, &tool_names);
            for (discovered, assigned) in tools.iter().zip(assigned) {
                if entry
                    .config
                    .effective_tool_exposure(&discovered.tool.name, builtin_tools)
                    != McpExposure::Direct
                {
                    continue;
                }
                if !registered.contains(assigned.as_str()) {
                    continue;
                }
                if crate::coding_agent::tool_selection::tool_excluded_by(exclusions, &assigned) {
                    continue;
                }
                eligible.insert(assigned);
            }
        }
        eligible
    }
}

/// The `mcp` builtin plugin.
pub struct McpPlugin {
    info: PluginInfo,
    manager: Arc<McpManager>,
    /// The agent directory this plugin resolved when it was built (the same
    /// one its manager's log, OAuth store and config root come from).
    ///
    /// Every config pass reads THIS value — [`Self::register`] and the
    /// `/mcp enable` restore path never re-resolve `PIR_AGENT_DIR` for a path,
    /// so the env cannot move discovery while the manager keeps its log and
    /// credentials on the construction-time directory (env audit §D5). A live
    /// env value that differs is still read, but only to surface the
    /// divergence as a diagnostic. [`AgentPlugin::on_reload`] is the one place
    /// a changed directory takes effect: it re-roots the manager and stores
    /// the new resolution here, both from the same read.
    agent_dir: RwLock<std::path::PathBuf>,
    /// The JS runtime handle, attached by [`crate::plugins::PluginHost::build`]
    /// so registration changes flush as `mcp_servers_change` events.
    js: RwLock<Option<crate::extensions::ExtensionHandle>>,
    /// Names registered from JS extensions during the previous registry
    /// rebuild; names that disappear by the next rebuild are unregistered
    /// (upstream `unregisterMcpServer` semantics), and a name whose deleted
    /// `mcp.json` entry used to shadow it is restored by the SAME rebuild:
    /// [`Self::sync_config_files`] unregisters stale config entries before
    /// the JS pass runs, so the JS registration lands again.
    extension_registered: RwLock<BTreeSet<String>>,
    /// Names whose `mcp.json` entry the latest [`Self::sync_config_files`]
    /// pass actually (re)registered. A rebuild re-runs `register()`, and the
    /// registry queues a `mcp_servers_change` per register call, so unchanged
    /// entries are kept out of the registry entirely; the JS pass below
    /// consults this set to emit its `overridden` change only when the
    /// shadowing config entry truly changed.
    config_changed: RwLock<BTreeSet<String>>,
    /// The manager-state follow-up of the latest registry rebuild's passes
    /// (see [`ConfigServerFollowUp`]): the servers whose config/JS entry was
    /// added, changed, or removed. Written by [`Self::sync_config_files`] and
    /// [`Self::apply_js_server_defs`] (merged, one pass each, in that order),
    /// drained by [`Self::reconcile_config_servers`] at the rebuild's
    /// enqueue point — the host calls [`Self::flush_server_changes`] right
    /// after the rebuild.
    config_follow_up: RwLock<ConfigServerFollowUp>,
    /// Whether [`Self::sync_config_files`] has run before. The FIRST pass
    /// registers the session's initial configuration, whose discovery rides
    /// the normal paths (session-start `McpManager::discover_all` and the
    /// lazy wait hooks), so it queues no re-discovery of its own entries; a
    /// name that is (re)registered by a LATER pass was added or changed
    /// mid-session (an `mcp.json` edit) and must be re-discovered.
    config_synced: AtomicBool,
    /// The session-facing state: the bound [`PluginContext`], the
    /// `autoEnableCodemode` flag, discovery-tool ownership, and `/mcp` session
    /// overrides. Shared with the `/mcp` handler (an `Arc`, no cycle).
    state: Arc<McpSessionState>,
    /// Servers whose live connection offers resources (upstream
    /// `serversWithResources()`): the exposure gate of the aggregate resource
    /// tools ([`crate::mcp::registry::MCP_RESOURCE_TOOLS`]) and the server set
    /// they list. Refreshed by [`Self::refresh_resource_servers`] on every
    /// event dispatch (a connection gained/lost between rebuilds changes the
    /// next `register()` pass).
    resource_servers: RwLock<BTreeSet<String>>,
    /// The set of CONNECTED servers the last [`Self::refresh_resource_servers`]
    /// probe ran against: the probe's `has_resources` check needs the live
    /// client (which can reconnect a dropped transport), so it only runs when
    /// this set changes.
    resource_probe_state: RwLock<Vec<String>>,
    /// The aggregate budget of the session-start discovery (upstream
    /// `startupWaitMs`, `DEFAULT_STARTUP_WAIT_MS = 10_000`,
    /// `extensions/mcp/index.ts:93`): the first prompt waits at most this
    /// long for the configured servers, then continues. A server still
    /// connecting is reported as pending and discovered in the background, so
    /// the cap never disables it. Set once at construction; tests shrink it
    /// through [`Self::with_startup_budget`].
    startup_budget: std::time::Duration,
}

/// A source's display text (the config file path, or the extension path).
fn source_text_of(source: &McpServerSource) -> String {
    match source {
        McpServerSource::GlobalConfig(path) | McpServerSource::ProjectConfig(path) => {
            path.display().to_string()
        }
        McpServerSource::Extension(path) => path.clone(),
    }
}

/// The `overridden` line upstream builds for a registration that lost to an
/// `mcp.json` entry (index.ts:351-354), with pir's source spelling for the
/// config file path.
fn overridden_line(name: &str, extension_path: Option<&str>, source: &McpServerSource) -> String {
    let extension = extension_path
        .filter(|path| !path.is_empty())
        .unwrap_or("<js-extension>");
    format!(
        "\"{name}\" registered by {extension} is overridden by \"{name}\" in {}",
        source_text_of(source)
    )
}

/// The diagnostic for a live `PIR_AGENT_DIR` that moved away from the
/// directory this plugin resolved at construction (or at the last reload).
///
/// The plugin NEVER follows the live value: every path it derives (config
/// discovery through the manager's root, `mcp.log`, `mcp-auth.json`) comes
/// from its one resolved directory, and the move takes effect only through
/// [`AgentPlugin::on_reload`]. Reporting it keeps the env audit's diagnostics
/// contract — a split is surfaced, not silently honoured — with the same
/// "rooted at" wording the manager's own divergence line uses.
fn divergent_live_dir_line(resolved_root: &std::path::Path) -> Option<String> {
    let live = crate::coding_agent::context::agent_dir();
    if crate::mcp::manager::same_dir(&live, resolved_root) {
        return None;
    }
    Some(format!(
        "{}: ignored; this MCP plugin is rooted at {} (its config, log and credentials stay together; a changed agent dir takes effect on /reload)",
        live.display(),
        resolved_root.display()
    ))
}

impl McpPlugin {
    /// Instantiate the plugin with its shared manager. The manager outlives
    /// registry rebuilds (one per plugin instance). The manager root is the
    /// process cwd; the plugin host re-roots via [`Self::with_cwd`] with the
    /// session cwd (a child session its task cwd) so `.pi/mcp.json`
    /// discovery and server spawns follow the session's working directory
    /// (upstream per-session cwd).
    pub fn new() -> Self {
        let agent_dir = crate::coding_agent::context::agent_dir();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Self::with_cwd(&agent_dir, &cwd)
    }

    /// Instantiate the plugin around a manager rooted at an explicit cwd
    /// (the plugin host passes the session cwd — a child session its task
    /// cwd — so `.pi/mcp.json` discovery and stdio spawns follow the
    /// session's working directory, upstream per-session cwd).
    ///
    /// `agent_dir` is resolved ONCE here and stored: it is the manager's log
    /// and OAuth root AND the directory every config pass reads. Nothing in
    /// this plugin re-resolves it from the environment except
    /// [`AgentPlugin::on_reload`], which moves both together.
    pub fn with_cwd(agent_dir: &std::path::Path, cwd: &std::path::Path) -> Self {
        let manager = McpManager::new(agent_dir, cwd);
        let mut info = PluginInfo::builtin("mcp");
        info.replaceable = super::builtin_extension_def("mcp").is_some_and(|def| def.replaceable);
        Self {
            info,
            manager,
            agent_dir: RwLock::new(agent_dir.to_path_buf()),
            js: RwLock::new(None),
            extension_registered: RwLock::new(BTreeSet::new()),
            config_changed: RwLock::new(BTreeSet::new()),
            config_follow_up: RwLock::new(ConfigServerFollowUp::default()),
            config_synced: AtomicBool::new(false),
            state: Arc::new(McpSessionState::new()),
            resource_servers: RwLock::new(BTreeSet::new()),
            resource_probe_state: RwLock::new(Vec::new()),
            startup_budget: crate::mcp::manager::DEFAULT_STARTUP_DISCOVERY_BUDGET,
        }
    }

    /// Override the session-start discovery budget (upstream `startupWaitMs`;
    /// tests use a short one so the cap is observable without a 10 s wait).
    pub fn with_startup_budget(mut self, budget: std::time::Duration) -> Self {
        self.startup_budget = budget;
        self
    }

    /// The session-start discovery budget this plugin applies.
    pub fn startup_budget(&self) -> std::time::Duration {
        self.startup_budget
    }

    /// The outcome of the latest session-start discovery: which servers
    /// failed and which were still connecting when the budget elapsed
    /// (`None` before the first `session_start`).
    pub fn startup_report(&self) -> Option<crate::mcp::manager::StartupDiscoveryReport> {
        self.state
            .startup_report
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The shared manager (the host seam and the CLI reuse it).
    pub fn manager(&self) -> &Arc<McpManager> {
        &self.manager
    }

    /// The `mcp_servers` system-prompt section for one prompt build, or `None`
    /// when MCP is not configured (no enabled servers) — the section is
    /// omitted entirely then, so a prompt without MCP is byte-identical to
    /// one built before this section existed.
    ///
    /// Synchronous and IO-free: it reads the manager's registry, discovered
    /// snapshot and state mirror only (see
    /// [`McpManager::prompt_server_listings`]), so a session build/reload and
    /// print's per-prompt cache can reuse it on every prompt. A status
    /// transition (`pending` -> `ready`) that lands while a prompt is being
    /// assembled shows on the NEXT prompt.
    ///
    /// The per-server `builtinTools` default comes from THIS plugin's config
    /// pass, so the reach label is the exposure the registration pass applied.
    pub fn prompt_section(&self) -> Option<String> {
        let state = Arc::clone(&self.state);
        let listings = self
            .manager
            .prompt_server_listings(|server| state.builtin_tools_for(server));
        crate::system_prompt::render_mcp_servers_section(&listings)
    }

    /// The agent directory this plugin resolved at construction (or at the
    /// last [`AgentPlugin::on_reload`]): the ONE directory behind its config
    /// discovery, its manager's log and its OAuth credential store.
    fn resolved_agent_dir(&self) -> std::path::PathBuf {
        self.agent_dir
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the JS runtime handle (host-side, before the first registry
    /// build).
    pub fn attach_js(&self, js: Option<crate::extensions::ExtensionHandle>) {
        *self
            .js
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = js;
    }
}

impl Default for McpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl McpPlugin {
    /// Register the servers of the global + (trust-gated) project `mcp.json`
    /// files, re-registering (and thereby queueing a `mcp_servers_change`)
    /// ONLY entries whose content actually differs from the registered one,
    /// and UNREGISTERING entries whose file definition disappeared since the
    /// previous pass.
    ///
    /// pir re-runs every plugin's `register()` on each registry rebuild, and
    /// `McpManager::load_config_files` registers every file entry
    /// unconditionally — so every rebuild queued a fabricated `replaced`
    /// change per configured server. Later files win per server name
    /// (project over global), so the desired entry is resolved per name
    /// first and an unchanged final entry never touches the registry.
    ///
    /// The removal pass is what keeps a DELETED `mcp.json` entry from
    /// shadowing a same-name JS registration forever: a stale config entry
    /// stays `GlobalConfig`/`ProjectConfig`-sourced in the registry, so the
    /// JS pass ([`Self::apply_js_server_defs`]) would treat it as the
    /// winning config source and skip the JS registration on every rebuild
    /// (upstream re-reading `mcp.json` simply loses the server). Only
    /// config-sourced entries are removed here — extension-sourced names are
    /// the JS pass's own bookkeeping — and the removed name is re-registered
    /// by the JS pass in the SAME rebuild when a JS def still claims it
    /// (upstream `unregisterMcpServer` + re-register).
    ///
    /// Both passes also release the manager state derived from the entry they
    /// dropped or replaced, because a rebuild happens mid-session:
    ///
    /// - a REMOVED entry's discovered snapshot entry is dropped here
    ///   ([`McpManager::forget_discovered`]) — the registration pass at the
    ///   end of [`Self::register`] reads the snapshot, so the removed
    ///   server's `mcp__*` bridges are gone with this rebuild instead of
    ///   being re-registered from a stale snapshot — and its live connection
    ///   is queued for release ([`ConfigServerFollowUp`], applied at the
    ///   rebuild tail);
    /// - an ADDED or CHANGED entry is queued for release + re-discovery: its
    ///   live connection was created from the OLD entry (so the new command /
    ///   env / headers would never be used, and a re-added entry would run
    ///   the removed one's command), and the re-discovery republishes the
    ///   tools of the current entry — marking the registry dirty when they
    ///   differ, so the next rebuild point registers bridges that match the
    ///   file. The FIRST pass queues no such re-discovery: its entries are
    ///   the session's initial configuration, already covered by the
    ///   session-start discovery and the lazy wait hooks.
    ///
    /// Returns the names actually (re)registered by this pass; the caller
    /// stores them for the JS pass ([`Self::config_changed`]).
    ///
    /// The directory read here is the plugin's OWN resolution
    /// ([`Self::resolved_agent_dir`]), never a fresh environment read: the
    /// documents come from [`McpManager::load_config_documents`] rooted at the
    /// same directory the manager's log and OAuth credential store were
    /// derived from. A live `PIR_AGENT_DIR` that differs is reported in the
    /// diagnostics, retained by [`McpManager::set_config_errors`] and written
    /// to the manager log, instead of silently discovering servers from a
    /// second directory; it takes effect through [`AgentPlugin::on_reload`],
    /// which re-roots the manager and updates this resolution together.
    fn sync_config_files(&self, project_trusted: bool) -> BTreeSet<String> {
        // The global-vs-project classification must name the file the manager
        // actually loaded, so the root it resolved decides both.
        let resolved_agent_dir = self.resolved_agent_dir();
        let root = self.manager.resolve_config_root(&resolved_agent_dir);
        let global_path = crate::config::mcp::global_mcp_path(&root);
        let (loaded, diagnostics) = self
            .manager
            .load_config_documents(&resolved_agent_dir, project_trusted);
        // The top-level `autoEnableCodemode` (upstream `loadMcpConfig` reads
        // both files into one state, so the project value overrides the
        // global one; a file without the flag keeps what came before, and no
        // file at all keeps the default `true`).
        let mut auto_enable_codemode = true;
        for file in &loaded {
            if let Some(value) = file.auto_enable_codemode {
                auto_enable_codemode = value;
            }
        }
        self.state
            .auto_enable_codemode
            .store(auto_enable_codemode, Ordering::SeqCst);
        // The top-level `builtinTools` (pir-specific, default true): same
        // merge as `autoEnableCodemode` — the project file overrides the
        // global one, a file without the flag keeps what came before, and no
        // file at all keeps the default `true`.
        let mut builtin_tools = true;
        for file in &loaded {
            if let Some(value) = file.builtin_tools {
                builtin_tools = value;
            }
        }
        self.state
            .builtin_tools
            .store(builtin_tools, Ordering::SeqCst);
        let mut desired: std::collections::BTreeMap<String, McpServerEntry> =
            std::collections::BTreeMap::new();
        // The load problems of every file, formatted like
        // [`McpManager::load_config_files`] (upstream `configErrors`), retained
        // by the manager so a status snapshot can report them. The manager's
        // own diagnostics (a divergent request) come first, then this plugin's
        // live-env divergence line, then the per-file problems in file order.
        let mut config_errors: Vec<String> = diagnostics;
        if let Some(line) = divergent_live_dir_line(&root) {
            self.manager.log().write_event("(config)", &line);
            config_errors.push(line);
        }
        // Per-server `builtinTools` applicability (see
        // [`McpSessionState::server_builtin_tools`]): wrapper-shaped files
        // contribute the merged flag, legacy flat files `false`.
        let mut server_builtin_tools: BTreeMap<String, bool> = BTreeMap::new();
        for file in loaded {
            let is_global = file.path == global_path;
            for error in &file.errors {
                let text = format!("{}: {error}", file.path.display());
                self.manager.log().write_event("(config)", &text);
                config_errors.push(text);
            }
            let source = if is_global {
                McpServerSource::GlobalConfig(file.path.clone())
            } else {
                McpServerSource::ProjectConfig(file.path.clone())
            };
            let applies = file.wrapper_shape && builtin_tools;
            for (name, config) in file.servers {
                server_builtin_tools.insert(name.clone(), applies);
                desired.insert(
                    name.clone(),
                    McpServerEntry {
                        name,
                        config,
                        source: source.clone(),
                    },
                );
            }
        }
        self.manager.set_config_errors(config_errors);
        *self
            .state
            .server_builtin_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = server_builtin_tools;
        // `/mcp` session overrides ride on top of the files (a toggle must not
        // be undone by this pass on the next rebuild). A removal leaves the
        // name out of `desired`, so the removal pass below unregisters it.
        let overrides = self
            .state
            .overrides
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for (name, patch) in &overrides {
            if patch.removed {
                desired.remove(name);
                continue;
            }
            let Some(entry) = desired.get_mut(name) else {
                continue;
            };
            if let Some(enabled) = patch.enabled {
                entry.config = config_with_enabled(&entry.config, enabled);
            }
            if let Some(exposure) = patch.exposure {
                entry.config = config_with_exposure(&entry.config, exposure);
            }
        }
        // Entries whose file definition disappeared since the previous pass:
        // unregister them before anything else, so a JS registration of the
        // same name (the JS pass below, same rebuild) is not blocked by the
        // stale config source. The unregister queues the `unregistered`
        // change event; a name that is both config-sourced and still desired
        // is untouched (the change loop replaces it when its content
        // changed). The manager state derived from the deleted entry is
        // released with it (see the doc comment): the snapshot entry now, the
        // connection at the rebuild tail.
        let mut follow_up = ConfigServerFollowUp::default();
        for entry in self.manager.registry().list() {
            let config_sourced = matches!(
                &entry.source,
                McpServerSource::GlobalConfig(_) | McpServerSource::ProjectConfig(_)
            );
            if !config_sourced || desired.contains_key(&entry.name) {
                continue;
            }
            self.manager
                .log()
                .write_event(&entry.name, "unregistered: removed from mcp.json");
            self.manager.registry().unregister(&entry.name);
            self.manager.forget_discovered(&entry.name);
            follow_up.mark_removed(&entry.name);
        }
        let mut changed = BTreeSet::new();
        for (name, entry) in desired {
            let unchanged = self.manager.registry().get(&name).is_some_and(|existing| {
                existing.config == entry.config && existing.source == entry.source
            });
            // Identical to the registered entry: no register call, so no
            // change event — the rebuild stays silent.
            if unchanged {
                continue;
            }
            let path = match &entry.source {
                McpServerSource::GlobalConfig(path) | McpServerSource::ProjectConfig(path) => {
                    path.display().to_string()
                }
                McpServerSource::Extension(path) => path.clone(),
            };
            self.manager
                .log()
                .write_event(&name, &format!("registered from {path}"));
            self.manager.registry().register(entry);
            // A mid-session addition or change: the connection and the
            // discovered tools still belong to the previous entry, so the
            // rebuild-tail flush releases the connection and re-discovers the
            // server (the FIRST pass is the session's initial configuration
            // and stays with the session-start/wait discovery).
            if self.config_synced.load(Ordering::SeqCst) {
                follow_up.mark_reconnected(&name);
            }
            changed.insert(name);
        }
        self.config_synced.store(true, Ordering::SeqCst);
        self.config_follow_up
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merge(follow_up);
        changed
    }

    /// Merge the JS-registered MCP servers (`pi.registerMcpServer`) into the
    /// registry. Config-file entries of the same name win (upstream
    /// precedence); extension-registered names that were removed since the
    /// previous rebuild are unregistered (upstream `unregisterMcpServer`),
    /// emitting the `unregistered` change. A name whose `mcp.json` entry was
    /// DELETED was already unregistered by [`Self::sync_config_files`]
    /// (which runs first in [`Self::register`]), so a JS def still claiming
    /// it re-registers here in the same rebuild instead of staying shadowed
    /// by the stale config entry.
    ///
    /// Only entries whose config actually differs touch the registry (a
    /// re-registration would queue a fabricated `replaced` change on every
    /// rebuild); a config-shadowed name reports `overridden` only when the
    /// shadowing `mcp.json` entry changed in this rebuild (see
    /// [`Self::config_changed`]).
    ///
    /// A registration that REPLACES a live extension entry (same name, new
    /// config) or re-adds a name whose removal is still queued in the
    /// manager-state follow-up ([`ConfigServerFollowUp`]: the config pass
    /// dropped the `mcp.json` entry — or an earlier JS pass unregistered it —
    /// and no reconcile has drained that yet) also queues that follow-up: the
    /// live connection and the discovered snapshot still belong to the
    /// previous entry, so without the release the session keeps talking to the
    /// old child and lists its stale tools. A brand-new extension server
    /// queues nothing: like the first config pass's entries it is reached by
    /// the session-start discovery and the lazy wait hooks, and queueing it
    /// would start its server at the rebuild tail (registration never starts
    /// a server).
    pub fn apply_js_server_defs(&self, defs: &[JsonValue]) {
        let previous = std::mem::take(
            &mut *self
                .extension_registered
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let config_changed = self
            .config_changed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut current = BTreeSet::new();
        let mut follow_up = ConfigServerFollowUp::default();
        let mut overridden: Vec<String> = Vec::new();
        for def in defs {
            let Some(name) = def.get("name").and_then(JsonValue::as_str) else {
                continue;
            };
            let config = def.get("config").cloned().unwrap_or(JsonValue::Null);
            // The registering extension's path when the prelude supplied it
            // (`RegisteredMcpServer.extensionPath`, mcp-servers.ts:307-313).
            let extension_path = def.get("extensionPath").and_then(JsonValue::as_str);
            let parsed: Result<McpServerConfig, _> = serde_json::from_value(config);
            match parsed {
                Ok(mut config) => {
                    // A `/mcp` override wins over the extension definition, so
                    // a disabled/removed server stays that way for the rest of
                    // the session (see [`ServerOverride`]).
                    let patch = self.state.override_for(name);
                    if let Some(patch) = patch {
                        if patch.is_removed() {
                            continue;
                        }
                        if let Some(enabled) = patch.enabled {
                            config = config_with_enabled(&config, enabled);
                        }
                        if let Some(exposure) = patch.exposure {
                            config = config_with_exposure(&config, exposure);
                        }
                    }
                    if let Some(problem) = crate::config::mcp::validate_server(name, &config) {
                        self.manager
                            .log()
                            .write_event(name, &format!("invalid registered config: {problem}"));
                        continue;
                    }
                    match self.manager.registry().get(name) {
                        // A config-file entry of the same name takes
                        // precedence (upstream); this rebuild changes nothing
                        // unless the shadowing entry itself just changed —
                        // then the override is reported once.
                        Some(existing)
                            if matches!(
                                existing.source,
                                McpServerSource::GlobalConfig(_)
                                    | McpServerSource::ProjectConfig(_)
                            ) =>
                        {
                            // Upstream `registeredServers` records the losers
                            // (index.ts:349-355); the list is rebuilt by every
                            // pass, so a deleted config entry drops its line.
                            overridden.push(overridden_line(
                                name,
                                extension_path,
                                &existing.source,
                            ));
                            if config_changed.contains(name) {
                                self.register_extension_server(name, config, extension_path);
                            }
                        }
                        // Unchanged extension registration: keep the live
                        // entry without a fabricated `replaced` change.
                        Some(existing) if existing.config == config => {
                            current.insert(name.to_owned());
                        }
                        // An extension entry whose config differs (the arms
                        // above split out the config-sourced and the unchanged
                        // cases): the entry is replaced, so the live
                        // connection, created from the OLD entry, must be
                        // released and the server re-discovered from the new
                        // one — exactly the config pass's change follow-up.
                        Some(_) => {
                            if self.register_extension_server(name, config, extension_path) {
                                current.insert(name.to_owned());
                                follow_up.mark_reconnected(name);
                            }
                        }
                        // No entry yet: a name whose removal is still queued
                        // (the config pass dropped its `mcp.json` entry, or an
                        // earlier JS pass unregistered it, and no reconcile has
                        // drained that follow-up) and the JS def re-adds it
                        // here, or a brand-new extension server. Only the
                        // re-add queues the re-discovery — it cancels the
                        // queued removal, so the re-added server's bridges come
                        // back with this rebuild instead of waiting for the
                        // lazy path; a brand-new name stays lazy.
                        None => {
                            if self.register_extension_server(name, config, extension_path) {
                                current.insert(name.to_owned());
                                let re_added = self
                                    .config_follow_up
                                    .read()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .removed
                                    .contains(name);
                                if re_added {
                                    follow_up.mark_reconnected(name);
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    self.manager
                        .log()
                        .write_event(name, &format!("invalid registered config: {error}"));
                }
            }
        }
        for name in previous.difference(&current) {
            // Only an extension-sourced live entry is unregistered: a name
            // that is config-shadowed in this rebuild keeps its `mcp.json`
            // entry (removing it would be a fabricated unregister); a
            // DELETED config entry was already removed by
            // `sync_config_files`, so a JS def re-registered here in the
            // same rebuild is not a candidate for this loop (it is in
            // `current`).
            let extension_sourced = self
                .manager
                .registry()
                .get(name)
                .is_some_and(|entry| matches!(entry.source, McpServerSource::Extension(_)));
            if !extension_sourced {
                continue;
            }
            self.manager.registry().unregister(name);
            self.manager
                .log()
                .write_event(name, "unregistered extension server");
            // The same release as the removed `mcp.json` entries
            // ([`Self::sync_config_files`]): without it the snapshot entry
            // would keep re-registering the removed server's bridges at this
            // rebuild, and its live connection would outlive the
            // registration (a later re-registration under the same name would
            // then talk to the stale process).
            self.manager.forget_discovered(name);
            follow_up.mark_removed(name);
        }
        *self
            .extension_registered
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = current;
        self.state.set_overridden(overridden);
        self.config_follow_up
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merge(follow_up);
    }

    /// Register one JS-extension server, translating the outcome to the
    /// `overridden` log line (the change event is queued inside the
    /// registry). `extension_path` is the registering extension's path when
    /// the prelude supplied it (`registerMcpServerJson` carries
    /// `extensionPath`); the historic placeholder is kept otherwise (older
    /// registration shapes / direct callers). Returns whether the
    /// registration landed.
    fn register_extension_server(
        &self,
        name: &str,
        config: McpServerConfig,
        extension_path: Option<&str>,
    ) -> bool {
        let landed = self.manager.registry().register_extension(McpServerEntry {
            name: name.to_owned(),
            config,
            source: McpServerSource::Extension(
                extension_path
                    .filter(|path| !path.is_empty())
                    .unwrap_or("<js-extension>")
                    .to_owned(),
            ),
        });
        if !landed {
            self.manager.log().write_event(
                name,
                "overridden: an mcp.json server of the same name takes precedence",
            );
        }
        landed
    }

    /// Flush the queued `mcp_servers_change` events through the unified
    /// fan-out, after applying the rebuild's manager-state follow-up.
    ///
    /// Upstream payload shape (`McpServersChangeEvent`, types.ts:744-748):
    /// `{type: "mcp_servers_change", servers: RegisteredMcpServer[]}` — the
    /// full post-change list (upstream emits `mcpServers.list()` on every
    /// registry change), each entry `{name, config, extensionPath}`
    /// (mcp-servers.ts:307-313). Config-file (`mcp.json`) entries stay out of
    /// the payload (upstream's registry covers `pi.registerMcpServer()`
    /// registrations only).
    ///
    /// One event per queued registry change (upstream's change listener fires
    /// on every `register`/`unregister`): the queue is drained in order and
    /// each entry produces its own dispatch with the current server list.
    /// Coalescing the queue into one event lost the change cadence upstream
    /// handlers observe (an extension connecting servers on each change saw
    /// only the first batch).
    ///
    /// Delivery is exactly one copy per face: the JS runtime receives the
    /// event through the direct handle (its historic path, unchanged wire
    /// semantics), and native plugins receive the SAME payload through
    /// [`crate::plugins::PluginHost::emit_native`], which skips the host's JS
    /// adapter — [`crate::plugins::PluginHost::emit`] would hand the JS face a
    /// second copy.
    ///
    /// Queue entries are produced by [`Self::register`] (config sync + JS-def
    /// pass) and by [`Self::register_extension_server`], so the host calls
    /// this at every rebuild enqueue point
    /// (`PluginHost::refresh_registry_if_changed`, `PluginHost::reload`) and,
    /// through the unconditional refresh tail, after a `session_start`
    /// dispatch that flushed entries queued at host build.
    ///
    /// That same post-rebuild position is where
    /// [`Self::reconcile_config_servers`] runs: it is the rebuild's first
    /// async step, so a server whose entry the passes just added, changed or
    /// removed has its stale connection released and its tools re-discovered
    /// before anything else observes the new registry.
    pub(crate) async fn flush_server_changes(&self, host: &crate::plugins::PluginHost) {
        self.reconcile_config_servers().await;
        // Rebuild-tail activation point: the host registry is the freshly
        // rebuilt one here, so a server that appeared/disappeared (or a
        // discovery that changed its exposure) is reflected before anything
        // else observes the new registry — including a resumed session whose
        // first loadout is built right after `session_start`.
        self.recompute_discovery_activation();
        let changes = self.manager.registry().drain_changes();
        if changes.is_empty() {
            return;
        }
        let servers = self.manager.registry().registered_servers();
        let js = self
            .js
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for _change in changes {
            // One payload per change: handlers may mutate it, and every event
            // carries the current full list like upstream's `list()` call.
            let mut payload = serde_json::json!({
                "type": "mcp_servers_change",
                "servers": servers.clone(),
            });
            if let Some(js) = &js {
                let _ = js.emit("mcp_servers_change", &mut payload).await;
            }
            // Native face: same payload, one dispatch, and only when a native
            // plugin declared interest (the JS copy above already happened).
            if host.has_native_handlers("mcp_servers_change") {
                host.emit_native("mcp_servers_change", &mut payload).await;
            }
        }
    }

    /// Apply the manager-state follow-up of the latest registry rebuild
    /// ([`Self::config_follow_up`], queued by [`Self::sync_config_files`] and
    /// [`Self::apply_js_server_defs`]): release the live connections of the
    /// entries that were changed or removed, then re-discover the enabled
    /// entries that were added or changed.
    ///
    /// A live connection holds the [`McpServerEntry`] it was created with, so
    /// without the release a config change would never take effect (the new
    /// command/env/headers stay unused) and a re-added entry would run the
    /// removed one's command; dropping the connection makes the next use —
    /// the re-discovery below, or a later call — connect from the registry's
    /// CURRENT entry. The re-discovery publishes like the lazy wait path
    /// ([`Self::wait_for_tool_call`]): the entry's snapshot is replaced and
    /// the registry is marked dirty when the tools differ, so the bridges
    /// that match the new entry register at the next rebuild point. A server
    /// whose discovery fails (unreachable, disabled since the pass, …) is
    /// logged by the manager and never blocks the rebuild, exactly like the
    /// wait path's swallowed failures.
    async fn reconcile_config_servers(&self) {
        let follow_up = std::mem::take(
            &mut *self
                .config_follow_up
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for server in &follow_up.reconnected {
            self.manager.disconnect(server).await;
        }
        for server in &follow_up.removed {
            self.manager.disconnect(server).await;
        }
        for server in &follow_up.reconnected {
            let enabled = self
                .manager
                .registry()
                .get(server)
                .is_some_and(|entry| entry.config.is_enabled());
            if enabled {
                let _ = self.manager.discover_server_and_publish(server).await;
            }
        }
    }
}

impl AgentPlugin for McpPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        // Server sources: global + trust-gated project mcp.json, then JS
        // registrations. The config pass re-registers ONLY content changes:
        // this method runs on every registry rebuild, and re-registering an
        // unchanged entry would queue a fabricated `replaced`
        // `mcp_servers_change` per server per rebuild (see
        // [`Self::sync_config_files`]).
        //
        // The plugin's ONE resolved agent directory: not a live environment
        // read, so config discovery and the manager's log/credential paths can
        // never come from different directories (env audit §D5). A moved
        // `PIR_AGENT_DIR` is reported by `sync_config_files` and adopted by
        // `on_reload`, never followed in place.
        let requested_agent_dir = self.resolved_agent_dir();
        let cwd = self.manager.cwd().to_path_buf();
        // The trust anchor is the SAME root the config pass will read from:
        // asking the manager keeps it from following a live `PIR_AGENT_DIR`
        // read the manager rejected (its log and credential store stay on the
        // construction-time root). Identical to the live value when nothing
        // moved; a rootless manager still follows the request.
        let root = self.manager.resolve_config_root(&requested_agent_dir);
        // `resolve_cwd_trust` applies the run's PINNED decision first (the
        // startup gate's `--approve`/`--no-approve`), so a resolved denial
        // cannot be reopened by `PIR_TRUST_PROJECT=1` or an old trust entry
        // when the project `mcp.json` is loaded.
        let trusted = crate::config::trust::resolve_cwd_trust(&cwd, &root);
        let config_changed = self.sync_config_files(trusted);
        *self
            .config_changed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config_changed;
        if let Some(js) = self
            .js
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            self.apply_js_server_defs(&js.mcp_server_defs());
        }

        // The `/mcp` command claim (upstream `pi.registerCommand("mcp", ...)`,
        // index.ts:1258): it makes the builtin replaceable by a JS extension
        // registering the same command. The handler carries upstream's
        // subcommands (`login`/`logout`/`reconnect`) and opens the interactive
        // manager in the TUI; `on_context` attaches the argument completions
        // (upstream `getArgumentCompletions`), which need the host-scoped
        // completion service and therefore cannot be registered here.
        let manager = Arc::clone(&self.manager);
        let state = Arc::clone(&self.state);
        // The handler carries the same resolution the pass above used, so the
        // `/mcp enable` restore path reads the manager's root rather than a
        // fresh environment value. A reload re-registers this command with the
        // re-rooted directory.
        let agent_dir = self.resolved_agent_dir();
        let handler: crate::plugins::CommandHandler = Arc::new(move |args: &str| {
            let manager = Arc::clone(&manager);
            let state = Arc::clone(&state);
            let agent_dir = agent_dir.clone();
            let args = args.to_owned();
            Box::pin(async move { run_mcp_command(&manager, &state, &agent_dir, &args).await })
        });
        ctx.register_command(
            "mcp",
            Some("Manage MCP servers: sign in, reconnect, enable or disable, and change exposure"),
            Some(crate::plugins::CommandBackend::Rust(handler)),
        );

        // Discovered tools (empty until a server connected) become registry
        // tools; a later rebuild after discovery picks them up. Names are
        // assigned per server with order-independent collision handling.
        // A server the passes above REMOVED has no snapshot entry left (the
        // removal released it), so its bridges are gone with this rebuild; a
        // server they ADDED has none yet, and one they CHANGED still carries
        // the previous discovery until the rebuild-tail flush
        // ([`Self::reconcile_config_servers`]) re-discovers it. The
        // registry-membership check below covers the removal whose release a
        // concurrent publication raced.
        for (server, tools) in self.manager.discovered_tools() {
            // A snapshot entry whose server is not in the registry is a
            // leftover of a removal (a publication that landed after
            // `McpManager::forget_discovered`): registering its bridges would
            // resurrect a removed server for the session. Registry entries
            // persist for disabled servers, so only a genuinely removed name
            // is skipped here; the concurrent-publish race itself is not
            // addressed at this point.
            let Some(entry) = self.manager.registry().get(&server) else {
                continue;
            };
            // Resolve each tool's exposure against the entry's CURRENT config
            // and the file-level `builtinTools` default: the manager's
            // snapshot carries upstream's `codemode` default, which the
            // `builtinTools` flag may have replaced with `direct`.
            let builtin_tools = self.state.builtin_tools_for(&server);
            let server_names: Vec<String> =
                tools.iter().map(|tool| tool.tool.name.clone()).collect();
            let assigned = crate::mcp::registry::assign_mcp_tool_names(&server, &server_names);
            for (discovered, registered) in tools.into_iter().zip(assigned) {
                // A disabled server's tools stay REGISTERED but hidden
                // (upstream `hideTools`): the snapshot entry survives the
                // disconnect, so without this the bridges would keep their
                // exposure — declaring (and calling) tools of a server the
                // user turned off. A re-enable restores the configured
                // exposure on the next rebuild (the discovery snapshot is
                // retained).
                let exposure = if entry.config.is_enabled() {
                    entry
                        .config
                        .effective_tool_exposure(&discovered.tool.name, builtin_tools)
                } else {
                    McpExposure::Hidden
                };
                ctx.register_tool(Arc::new(McpToolBridge::with_effective_exposure(
                    self.manager.clone(),
                    discovered,
                    registered,
                    exposure,
                )));
            }
        }

        // The aggregate resource tools (upstream `syncResourceTools`): always
        // registered, with the widest base exposure among the servers that
        // currently offer resources and `hidden` (registered but unreachable)
        // while none does.
        let exposure = self.resource_tool_exposure();
        for kind in [
            McpResourceToolKind::ListResources,
            McpResourceToolKind::ListResourceTemplates,
            McpResourceToolKind::ReadResource,
        ] {
            ctx.register_tool(Arc::new(McpResourceTool::new(
                self.manager.clone(),
                kind,
                exposure,
            )));
        }
        // Re-run the discovery activation against the pass that just ran:
        // `register()` is a rebuild point, and a server enabled/disabled/
        // removed by an `mcp.json` edit (or by a `/reload`) must move the
        // active set with it. The bound context is only present after host
        // construction — `on_context` performs the initial activation.
        self.recompute_discovery_activation();
        Ok(())
    }

    /// Capture the bound session context so the plugin can write the active
    /// set through [`SessionService::set_active_tools`]
    /// ([`crate::plugins::services::SessionService`]) and recompute the
    /// discovery activation before the first loadout.
    ///
    /// The clone is cheap and the session service holds a `Weak` handle to the
    /// session, so this adds no cycle. Binding is the documented invalidation
    /// point (`/reload`, `newSession`, `fork`, ...): the fresh context replaces
    /// the stale one and the activation is recomputed against the new session.
    fn on_context(&self, ctx: &PluginContext) {
        *self
            .state
            .context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
        // Upstream attaches `getArgumentCompletions` to `registerCommand`; the
        // native face keeps one provider per command name in the host-scoped
        // completion registry (first registration wins, so a rebind is
        // idempotent). Registered here rather than in `register()` because the
        // service only exists once a context is bound.
        if let Some(completions) = ctx.completions() {
            completions.register_argument(
                "mcp",
                Arc::new(McpArgumentCompletions {
                    manager: Arc::clone(&self.manager),
                    state: Arc::clone(&self.state),
                }),
            );
        }
        self.recompute_discovery_activation();
    }

    fn events(&self) -> EventInterest {
        // The only event with plugin work is `session_start` (the discovery
        // connect that publishes the servers' tools). The `mcp_servers_change`
        // flush is anchored to the registry rebuild that queues the change
        // (`PluginHost::refresh_registry_if_changed` / `PluginHost::reload`
        // call [`Self::flush_server_changes`]; the refresh tail also flushes
        // the entries queued at host build after the first `session_start`).
        // A wildcard interest here would make `PluginHost::has_handlers` true
        // for every event in the session — turning every caller's
        // "no consumer, skip the emission" gate into a no-op — for work that
        // no longer rides the event dispatch.
        EventInterest::List(vec!["session_start".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            // Cheap gate first: `session_start` is the only inbound event the
            // plugin handles (its declared interest). The host fans every
            // event out to every native plugin, so without this check
            // `input`, `tool_result`, `message_update`, ... each paid
            // `refresh_resource_servers` (connection + registry snapshots).
            // Returning before any lock keeps unrelated events at one string
            // comparison.
            if event.name.as_ref() != "session_start" {
                return PluginEventOutcome::None;
            }
            // First use: connect the configured servers and refresh the
            // discovered tool snapshot (the host rebuilds the registry
            // after this dispatch when the tool set changed). The refresh
            // tail flushes the `mcp_servers_change` entries queued at host
            // build, so no flush happens here — a flush inside the dispatch
            // could not reach the native fan-out (the plugin holds no host
            // handle) and would split the two faces.
            //
            // BOUNDED (upstream `waitForDirectServers`' 10 s
            // `DEFAULT_STARTUP_WAIT_MS`, index.ts:93,1157-1174): the first
            // prompt cannot block on a server that never answers. The
            // per-server timeout still applies; a server still connecting
            // when the aggregate budget elapses is cancelled, reported as
            // pending, and retried by the manager's background continuation.
            let report = self.manager.discover_all_bounded(self.startup_budget).await;
            self.report_startup_problems(&report);
            *self
                .state
                .startup_report
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
            // Upstream `reportProblems` (index.ts:646-659): one warning after
            // startup listing the servers that need the user, ending in the
            // exact remedy pointer `Run /mcp to fix.`.
            self.report_problems().await;
            // The resource tools' exposure is derived from the servers that
            // have resources (upstream `syncResourceTools` on connection
            // changes); refresh before the host's post-dispatch rebuild so
            // this dispatch's connections are reflected in the registration
            // pass that follows.
            self.refresh_resource_servers().await;
            PluginEventOutcome::None
        })
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.manager.shutdown().await;
            Ok(())
        })
    }

    fn on_reload(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            // Reload is the ONE place a changed agent directory takes effect.
            // Resolve it once, then move BOTH faces from the same value: the
            // manager keeps its shutdown-first teardown and swaps its config
            // root, log and OAuth store as one unit, and this plugin's stored
            // directory (read by the registry rebuild that follows this hook)
            // gets the same resolution. Neither face can be left on the old
            // directory while the other serves the new one.
            let agent_dir = crate::coding_agent::context::agent_dir();
            self.manager.re_root(&agent_dir).await;
            *self
                .agent_dir
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = agent_dir;
            Ok(())
        })
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Factory for [`super::builtin::BUILTIN_EXTENSION_FACTORIES`].
pub fn new_mcp_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(McpPlugin::new())
}

// ---------------------------------------------------------------------------
// `/mcp` (upstream `serversMenu`, index.ts:640-700)
// ---------------------------------------------------------------------------

/// A copy of `config` with its `enabled` flag forced (the `/mcp` toggle; the
/// shape is the upstream `McpServerConfigBase.enabled` field).
fn config_with_enabled(config: &McpServerConfig, enabled: bool) -> McpServerConfig {
    let mut next = config.clone();
    match &mut next {
        McpServerConfig::Stdio(stdio) => stdio.base.enabled = Some(enabled),
        McpServerConfig::Http(http) => http.base.enabled = Some(enabled),
    }
    next
}

/// A copy of `config` with its base `exposure` forced (the manager's exposure
/// action; the shape is the upstream `McpServerConfigBase.exposure` field).
fn config_with_exposure(config: &McpServerConfig, exposure: McpExposure) -> McpServerConfig {
    let mut next = config.clone();
    match &mut next {
        McpServerConfig::Stdio(stdio) => stdio.base.exposure = Some(exposure),
        McpServerConfig::Http(http) => http.base.exposure = Some(exposure),
    }
    next
}

/// The source shown per server (`global` / `project` / `extension`).
fn source_label(source: &McpServerSource) -> &'static str {
    match source {
        McpServerSource::GlobalConfig(_) => "global",
        McpServerSource::ProjectConfig(_) => "project",
        McpServerSource::Extension(_) => "extension",
    }
}

/// The `/mcp` report: every registered server with its connection state,
/// configured exposure, discovered tool count, and source (`serversMenu`,
/// index.ts:670-683).
///
/// The rows come from [`McpManager::status_snapshot`] — the same snapshot a
/// manager UI reads — so the command, the plugin's [`McpPlugin::status`]
/// accessor and a TUI panel cannot drift. The upstream-shaped plain text is
/// [`McpStatusSnapshot::format_status`]; this list form adds the source and
/// the always-visible tool count the `/mcp` menu shows.
fn servers_report(status: &McpStatusSnapshot) -> String {
    if status.servers.is_empty() {
        return status.format_status();
    }
    let mut lines = vec!["MCP servers:".to_owned()];
    for server in &status.servers {
        let count = server.tool_count();
        // Bare state word on the row; the error goes on its own indented
        // lines (so a multi-line transport message is not folded into the
        // label and then repeated by the error block).
        lines.push(format!(
            "  {} ({} · {} · {count} {} · {})",
            server.name,
            server.state_word(),
            crate::mcp::manager::exposure_label(server.exposure),
            if count == 1 { "tool" } else { "tools" },
            source_label(&server.source),
        ));
        if let Some(error) = server.error().filter(|_| !server.state.is_connected()) {
            for line in error.lines() {
                lines.push(format!("    {line}"));
            }
        }
    }
    for error in &status.config_errors {
        lines.push(format!("config error: {error}"));
    }
    for line in &status.overridden {
        lines.push(format!("overridden: {line}"));
    }
    lines.join("\n")
}

/// The plugin's status surface: the manager snapshot with this plugin's
/// per-file `builtinTools` applicability and the extension registrations an
/// `mcp.json` entry shadows.
///
/// Cheap and non-blocking (no connect, no `tools/list`): a server that was
/// never used reports `Idle` (rendered `configured`). This is what a manager
/// UI (the `/mcp` command, a TUI panel, an RPC consumer) should read instead
/// of re-deriving state from the registry and `mcp.json`.
impl McpPlugin {
    pub async fn status(&self) -> McpStatusSnapshot {
        let mut snapshot = self
            .manager
            .status_snapshot(|server| self.state.builtin_tools_for(server))
            .await;
        self.state.enrich(&mut snapshot);
        snapshot
    }

    /// The upstream-shaped plain text of [`Self::status`] (upstream
    /// `formatStatus`): a non-TUI caller (the `/mcp` command, an RPC client)
    /// prints this without touching the TUI.
    pub async fn format_status(&self) -> String {
        self.status().await.format_status()
    }

    /// Surface the outcome of the bounded session-start discovery
    /// ([`crate::mcp::manager::McpManager::discover_all_bounded`]).
    ///
    /// Two distinct notices, both bounded (server name + one-line reason, see
    /// [`crate::mcp::manager::bounded_reason`]; never a config dump):
    ///
    /// - **failed**: one warning per failed server, written as an `[ext]`
    ///   diagnostic at the existing warn verbosity. It reaches `pir.log` in
    ///   the TUI and stderr in the headless modes, so a server that could not
    ///   connect is visible instead of surfacing only as a later
    ///   `Tool not found in registry`. The manager log already holds the full
    ///   error; the connection state carries it for `/mcp`.
    /// - **pending**: one upstream-shaped info notice
    ///   (`MCP servers are still connecting; ...`, index.ts:1172) naming
    ///   every server the budget skipped. Their tools appear once the
    ///   background continuation connects them.
    ///
    /// Returns the diagnostic lines (observability/tests). A UI-bound host
    /// also receives the pending notice through [`crate::plugins::services::UiService`]
    /// like upstream; the failed lines are diagnostics, not chat messages, so
    /// they never enter the conversation.
    pub fn report_startup_problems(
        &self,
        report: &crate::mcp::manager::StartupDiscoveryReport,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        for problem in &report.failed {
            let line = format!(
                "[ext] MCP server \"{}\" failed to start: {}",
                problem.server, problem.message
            );
            crate::ext_diag!("{line}");
            lines.push(line);
        }
        if !report.pending.is_empty() {
            let notice = format!(
                "MCP servers are still connecting ({}); their tools become available once connected.",
                report.pending.join(", ")
            );
            crate::ext_diag_info!("[ext] {notice}");
            if let Some(ui) = self.state.context().and_then(|ctx| ctx.ui_arc()) {
                ui.notify(crate::plugins::services::NotifyLevel::Info, &notice);
            }
            lines.push(format!("[ext] {notice}"));
        }
        lines
    }

    /// Upstream `reportProblems` (index.ts:646-659): one warning after the
    /// startup discovery listing every enabled server that needs the user
    /// (failed or sign-in-required), ending in `Run /mcp to fix.`.
    ///
    /// Delivered through the bound [`PluginContext`]'s UI service, i.e. the
    /// same notification queue the TUI drains; a host without a UI service
    /// (print mode) reports nothing here and the command's status text
    /// carries the same information when the user asks for it.
    pub async fn report_problems(&self) -> Vec<String> {
        let Some(context) = self.state.context() else {
            return Vec::new();
        };
        let Some(ui) = context.ui() else {
            return Vec::new();
        };
        let snapshot = self.status().await;
        let mut lines: Vec<String> = snapshot
            .config_errors
            .iter()
            .map(|error| format!("config: {error}"))
            .collect();
        for server in &snapshot.servers {
            // Upstream reports only `needs-auth` and `failed` here; a disabled
            // server is the user's own choice, not a problem.
            let needs_user = matches!(
                server.state,
                crate::mcp::manager::ConnectionState::NeedsAuth { .. }
                    | crate::mcp::manager::ConnectionState::Failed { .. }
            );
            if needs_user {
                lines.push(format!("{}: {}", server.name, server.state_label()));
            }
        }
        if lines.is_empty() {
            return Vec::new();
        }
        let message = format!(
            "MCP servers need attention:\n{}\nRun /mcp to fix.",
            lines
                .iter()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        ui.notify(crate::plugins::services::NotifyLevel::Warning, &message);
        lines
    }

    /// The plugin's argument-completion candidates for the `/mcp` command
    /// (upstream `getArgumentCompletions`, index.ts:1259-1283): actions first,
    /// then the servers eligible for the typed action. Test/diagnostic seam —
    /// the TUI reaches the same function through
    /// [`McpArgumentCompletions`].
    #[doc(hidden)]
    pub async fn command_argument_completions(
        &self,
        prefix: &str,
    ) -> Vec<crate::plugins::services::AutocompleteItem> {
        let servers = command_servers(&self.manager, &self.state).await;
        argument_completions(prefix, &servers)
    }
}

/// Re-register one server from the `mcp.json` files (used by `/mcp enable`
/// after `/mcp remove` cleared the session override). Also records the
/// server's `builtinTools` applicability for the restored entry (a legacy
/// flat file keeps upstream's `codemode`).
///
/// The files come from [`McpManager::load_config_documents`], rooted at
/// `agent_dir` — the plugin's ONE resolved directory (the same one the
/// manager's log and OAuth credential store were derived from). Reading a live
/// `PIR_AGENT_DIR` here would re-open the split-brain for the `/mcp enable`
/// path; a live value that differs is diagnosed instead of followed, exactly
/// like the config pass.
fn restore_config_server(
    manager: &McpManager,
    state: &McpSessionState,
    agent_dir: &std::path::Path,
    name: &str,
) -> bool {
    let cwd = manager.cwd();
    // Same root the documents come from (see `register`'s trust anchor): a
    // rootless manager falls back to the request, a rooted one keeps its
    // current directory. The pinned run decision (see `register`) stays the
    // single source for the project scope here too.
    let root = manager.resolve_config_root(agent_dir);
    let trusted = crate::config::trust::resolve_cwd_trust(cwd, &root);
    let global_path = crate::config::mcp::global_mcp_path(&root);
    let mut found: Option<McpServerEntry> = None;
    let mut applies = false;
    let (loaded, mut diagnostics) = manager.load_config_documents(agent_dir, trusted);
    // A live env move is reported here too (the config pass's line would not
    // run again before this restore): the plugin never follows it, and the
    // line names the directory the restored entry actually came from.
    if let Some(line) = divergent_live_dir_line(&root) {
        manager.log().write_event("(config)", &line);
        diagnostics.push(line);
    }
    // No config pass runs after this point, so the loader's diagnostics are
    // also retained as `configErrors` instead of being dropped. De-duplicated
    // because a session can restore several servers under the same divergence.
    if !diagnostics.is_empty() {
        let mut errors = manager.config_errors();
        for line in diagnostics {
            if !errors.contains(&line) {
                errors.push(line);
            }
        }
        manager.set_config_errors(errors);
    }
    for file in loaded {
        let Some(config) = file.servers.get(name) else {
            continue;
        };
        let source = if file.path == global_path {
            McpServerSource::GlobalConfig(file.path.clone())
        } else {
            McpServerSource::ProjectConfig(file.path.clone())
        };
        applies = file.wrapper_shape && state.builtin_tools();
        found = Some(McpServerEntry {
            name: name.to_owned(),
            config: config.clone(),
            source,
        });
    }
    match found {
        Some(entry) => {
            state
                .server_builtin_tools
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(name.to_owned(), applies);
            manager.registry().register(entry);
            true
        }
        None => false,
    }
}

/// The upstream `/mcp` usage string (index.ts:277).
const MCP_USAGE: &str =
    "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";

/// Sign-in deadline (`pir mcp login --timeout`'s default, cli.ts:73).
const MCP_SIGN_IN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The connect deadline before a sign-in decides whether one is needed
/// (`pir mcp login`'s check, `MCP_CHECK_TIMEOUT` in `coding_agent::cli`).
const MCP_LOGIN_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One `/mcp` action. `Login`/`Logout`/`Reconnect` are upstream's subcommands;
/// `Enable`/`Disable`/`Remove`/`List` are pir's session-scoped extras (upstream
/// reaches enable/disable through the manager and has no removal at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpAction {
    Login,
    Logout,
    Reconnect,
    Enable,
    Disable,
    Remove,
    List,
}

impl McpAction {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "login" => Some(Self::Login),
            "logout" => Some(Self::Logout),
            "reconnect" => Some(Self::Reconnect),
            "enable" => Some(Self::Enable),
            "disable" => Some(Self::Disable),
            "remove" => Some(Self::Remove),
            "list" => Some(Self::List),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Logout => "logout",
            Self::Reconnect => "reconnect",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Remove => "remove",
            Self::List => "list",
        }
    }
}

/// One parsed `/mcp` line (upstream
/// `const [action, name, ...extra] = args.trim().split(/\s+/).filter(Boolean)`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum McpInvocation {
    /// No argument: the manager (TUI) or the plain status (print/RPC).
    Status,
    /// An action with its optional server name.
    Action(McpAction, Option<String>),
    /// More than two fields (or an unknown action): show the usage string.
    Usage,
}

impl McpInvocation {
    /// Parse `args` exactly like upstream: at most two whitespace-separated
    /// fields, the first one a known action.
    fn parse(args: &str) -> Self {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        match tokens.as_slice() {
            [] => Self::Status,
            [action] => match McpAction::parse(action) {
                Some(action) => Self::Action(action, None),
                None => Self::Usage,
            },
            [action, server] => match McpAction::parse(action) {
                Some(action) => Self::Action(action, Some((*server).to_owned())),
                None => Self::Usage,
            },
            _ => Self::Usage,
        }
    }
}

/// One configured server as the `/mcp` command and its completions see it:
/// the plugin's own status row (upstream `McpServer` minus the live connection
/// object) plus the two things a live connection answers — whether one exists
/// (upstream `connection !== undefined`, the `/mcp reconnect` eligibility
/// rule) and whether the server authenticates with OAuth (upstream `usesOAuth`,
/// runtime.ts:83).
///
/// The row comes from [`McpManager::status_snapshot`] through
/// [`McpStatusSnapshot::servers`], so the command, the manager menu and the
/// completions show exactly what [`McpPlugin::status`] reports — one state
/// formatter ([`crate::mcp::manager::McpServerStatus::state_label`]), not two.
#[derive(Debug, Clone)]
struct CommandServer {
    status: crate::mcp::manager::McpServerStatus,
    /// A connection object exists: the `/mcp reconnect` eligibility rule.
    has_connection: bool,
}

impl CommandServer {
    fn name(&self) -> &str {
        &self.status.name
    }

    /// Upstream `usesOAuth(server)`: an OAuth-capable server that is enabled (a
    /// disabled server has no connection upstream, so its connection-based
    /// predicate excludes it too).
    fn uses_oauth(&self) -> bool {
        self.status.enabled && crate::mcp::oauth::oauth_server_url(&self.status.config).is_some()
    }

    /// Upstream `describeState` (index.ts:143-162) through the shared status
    /// row. A connected server shows its tool count there, which is upstream's
    /// `connected · N tools` shape.
    fn state_label(&self) -> String {
        self.status.state_label()
    }
}

/// The servers `/mcp` acts on, built from ONE status snapshot (the same call
/// [`McpPlugin::status`] makes, so the command and the plugin's status surface
/// cannot drift). `states` is the manager's connection-state map; its keys are
/// exactly the live connection objects.
async fn command_servers(manager: &McpManager, state: &McpSessionState) -> Vec<CommandServer> {
    let mut snapshot = manager
        .status_snapshot(|server| state.builtin_tools_for(server))
        .await;
    state.enrich(&mut snapshot);
    let states = manager.connection_states().await;
    snapshot
        .servers
        .into_iter()
        .map(|status| CommandServer {
            has_connection: states.contains_key(&status.name),
            status,
        })
        .collect()
}

/// Upstream `attentionRank` (index.ts:164-179): the states that need the user
/// sort first in the manager list.
fn attention_rank(state: &crate::mcp::manager::ConnectionState, enabled: bool) -> u8 {
    use crate::mcp::manager::ConnectionState;
    if !enabled {
        return 5;
    }
    match state {
        ConnectionState::NeedsAuth { .. } => 0,
        ConnectionState::Failed { .. } => 1,
        ConnectionState::Disconnected { .. } => 2,
        ConnectionState::Connected => 4,
        _ => 3,
    }
}

/// `/mcp`'s argument completions (upstream `getArgumentCompletions`,
/// index.ts:1259-1283).
///
/// The prefix is the text after `/mcp` (the TUI passes exactly that): the
/// first field completes the action, the second the server name filtered by
/// that action's eligibility. A third field offers nothing (upstream returns
/// `null`), as does an unknown action.
fn argument_completions(
    prefix: &str,
    servers: &[CommandServer],
) -> Vec<crate::plugins::services::AutocompleteItem> {
    use crate::plugins::services::AutocompleteItem;
    let fields = split_completion_fields(prefix);
    if fields.len() > 2 {
        return Vec::new();
    }
    let action = fields.first().copied().unwrap_or("");
    let Some(server_prefix) = fields.get(1).copied() else {
        return ["login", "logout", "reconnect"]
            .into_iter()
            .filter(|item| item.starts_with(action))
            .map(|item| AutocompleteItem::new(format!("{item} "), item))
            .collect();
    };
    let Some(action) = McpAction::parse(action) else {
        return Vec::new();
    };
    if !matches!(
        action,
        McpAction::Login | McpAction::Logout | McpAction::Reconnect
    ) {
        return Vec::new();
    }
    servers
        .iter()
        .filter(|server| match action {
            McpAction::Reconnect => server.has_connection,
            _ => server.uses_oauth(),
        })
        .filter(|server| server.name().starts_with(server_prefix))
        .map(|server| {
            AutocompleteItem::new(
                format!("{} {}", action.as_str(), server.name()),
                server.name().to_owned(),
            )
            .with_description(server.state_label())
        })
        .collect()
}

/// Split a completion prefix on whitespace like upstream
/// `prefix.trimStart().split(/\s+/)`: leading whitespace is ignored and a
/// trailing empty field is preserved (`"login "` is `["login", ""]`, which is
/// what makes `/mcp login <tab>` list the OAuth servers).
fn split_completion_fields(prefix: &str) -> Vec<&str> {
    let trimmed = prefix.trim_start();
    if trimmed.is_empty() {
        return vec![""];
    }
    let mut fields = Vec::new();
    let mut start = None;
    for (index, ch) in trimmed.char_indices() {
        if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                fields.push(&trimmed[begin..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    match start {
        Some(begin) => fields.push(&trimmed[begin..]),
        None => fields.push(""),
    }
    fields
}

/// Completion provider registered on the session's
/// [`crate::plugins::services::CompletionService`] (upstream attaches
/// `getArgumentCompletions` to the registered command; the native registry is
/// per command name, so this is the equivalent seam).
struct McpArgumentCompletions {
    manager: Arc<McpManager>,
    state: Arc<McpSessionState>,
}

impl crate::plugins::services::ArgumentCompletions for McpArgumentCompletions {
    fn complete(
        &self,
        prefix: &str,
    ) -> crate::plugins::services::ServiceFuture<
        'static,
        Vec<crate::plugins::services::AutocompleteItem>,
    > {
        let manager = Arc::clone(&self.manager);
        let state = Arc::clone(&self.state);
        let prefix = prefix.to_owned();
        Box::pin(async move {
            let servers = command_servers(&manager, &state).await;
            argument_completions(&prefix, &servers)
        })
    }
}

/// The plain status text (upstream `formatStatus`, index.ts:1017-1042) with
/// this plugin's per-file `builtinTools` resolution and `overridden` lines.
async fn status_text(manager: &McpManager, state: &McpSessionState) -> String {
    let mut snapshot = manager
        .status_snapshot(|server| state.builtin_tools_for(server))
        .await;
    state.enrich(&mut snapshot);
    servers_report(&snapshot)
}

/// Pick the server for an action (upstream `pickServer`, index.ts:1287-1314).
///
/// `eligible` is the set the omitted-name form picks from; `named_eligible` is
/// the explicit-name rule (reconnect stays lenient there because pir connects
/// lazily, so a configured-but-unused server is still reconnectable). A single
/// candidate wins, then a single preferred candidate, then the UI's picker.
async fn pick_server(
    servers: &[CommandServer],
    requested: Option<&str>,
    eligible: impl Fn(&CommandServer) -> bool,
    named_eligible: impl Fn(&CommandServer) -> bool,
    preferred: impl Fn(&CommandServer) -> bool,
    none: &str,
    ui: Option<&dyn crate::plugins::services::UiService>,
) -> Result<Option<String>, String> {
    if let Some(name) = requested {
        let Some(server) = servers.iter().find(|server| server.name() == name) else {
            return Err(format!("No MCP server named \"{name}\"."));
        };
        if !named_eligible(server) {
            return Err(none.to_owned());
        }
        return Ok(Some(server.name().to_owned()));
    }
    let candidates: Vec<&CommandServer> =
        servers.iter().filter(|server| eligible(server)).collect();
    if candidates.is_empty() {
        return Err(none.to_owned());
    }
    if candidates.len() == 1 {
        return Ok(Some(candidates[0].name().to_owned()));
    }
    let preferred_candidates: Vec<&&CommandServer> = candidates
        .iter()
        .filter(|server| preferred(server))
        .collect();
    if preferred_candidates.len() == 1 {
        return Ok(Some(preferred_candidates[0].name().to_owned()));
    }
    let Some(ui) = ui else {
        let names = candidates
            .iter()
            .map(|server| server.name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Several MCP servers match: {names}. Name one explicitly: /mcp <action> <server>."
        ));
    };
    let options: Vec<crate::plugins::services::SelectOption> = candidates
        .iter()
        .map(|server| {
            crate::plugins::services::SelectOption::new(
                format!("{} — {}", server.name(), server.state_label()),
                server.name().to_owned(),
            )
        })
        .collect();
    match ui
        .select(
            "MCP server",
            &options,
            crate::plugins::services::DialogOptions::default(),
        )
        .await
    {
        Ok(choice) => Ok(choice),
        // A dialog that could not be shown (no UI sink, a timeout) leaves the
        // omitted-name form with no answer to act on; the caller reports it.
        Err(error) => Err(format!("MCP server selection is unavailable: {error}")),
    }
}

/// The prompt `/mcp login` runs the OAuth flow with (upstream `signInWithUi` /
/// `ctx.ui.input`): the authorization URL through the UI notification queue
/// and the browser, the pasted redirect URL through an input dialog.
struct UiSignInPrompt<'a> {
    ui: &'a dyn crate::plugins::services::UiService,
    server: String,
}

impl crate::mcp::oauth::McpSignInPrompt for UiSignInPrompt<'_> {
    fn show_authorization_url(&self, url: &str) {
        self.ui.notify(
            crate::plugins::services::NotifyLevel::Info,
            &format!(
                "Sign in to MCP server \"{}\" in your browser:\n{url}",
                self.server
            ),
        );
        crate::ai::auth::oauth::open_browser(url);
    }

    fn prompt_for_redirect_url(&self) -> crate::mcp::oauth::PromptFuture<'_> {
        let title = format!(
            "Waiting for sign-in to \"{}\". If the browser cannot reach this machine, paste the URL it was redirected to.",
            self.server
        );
        Box::pin(async move {
            self.ui
                .input(
                    &title,
                    Some("http://127.0.0.1:.../callback?code=..."),
                    crate::plugins::services::DialogOptions::default(),
                )
                .await
                .unwrap_or_default()
        })
    }
}

/// Sign in to one server, then reconnect (upstream `loginCommand`,
/// index.ts:1319-1349).
async fn sign_in_server(
    manager: &McpManager,
    state: &McpSessionState,
    name: &str,
) -> Result<String, String> {
    let Some(entry) = manager.registry().get(name) else {
        return Err(format!("No MCP server named \"{name}\"."));
    };
    let Some(url) = crate::mcp::oauth::oauth_server_url(&entry.config).map(str::to_owned) else {
        return Err(format!(
            "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
    };
    let ui = state.context().and_then(|context| context.ui_arc());
    let Some(ui) = ui else {
        return Err(format!(
            "Signing in to MCP server \"{name}\" requires interactive mode."
        ));
    };
    // Connecting first answers whether a sign-in is needed (upstream
    // loginCommand / cli.ts:532).
    match manager.check_server(name, MCP_LOGIN_CHECK_TIMEOUT).await {
        Ok(tools) => {
            return Ok(format!(
                "Already signed in to MCP server \"{name}\" ({} tools).",
                tools.len()
            ))
        }
        Err(crate::mcp::error::McpError::LoginRequired { .. }) => {}
        Err(error) if crate::mcp::oauth::is_authorization_required(&error) => {}
        Err(error) => return Err(format!("MCP server \"{name}\" failed to connect: {error}")),
    }
    let crate::config::mcp::McpServerConfig::Http(http) = &entry.config else {
        return Err(format!(
            "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
    };
    let settings = crate::mcp::oauth::McpOAuthSettings::from_config(http, name)
        .map_err(|error| format!("MCP server \"{name}\": {error}"))?;
    let store = manager.oauth_store().for_server(name, &url);
    let prompt = UiSignInPrompt {
        ui: ui.as_ref(),
        server: name.to_owned(),
    };
    match crate::mcp::oauth::sign_in(&store, &settings, &url, None, &prompt, MCP_SIGN_IN_TIMEOUT)
        .await
    {
        Ok(()) => {}
        Err(crate::mcp::oauth::McpOAuthError::Cancelled) => {
            return Err("Sign-in cancelled.".to_owned())
        }
        Err(error) => return Err(format!("Sign-in failed: {error}")),
    }
    manager.disconnect(name).await;
    match manager.discover_server_and_publish(name).await {
        Ok(tools) => {
            state.recompute(manager);
            Ok(format!(
                "Signed in to MCP server \"{name}\" ({} tools).",
                tools.len()
            ))
        }
        Err(error) => Ok(format!("Signed in, but {error}")),
    }
}

/// Delete one server's stored OAuth credentials (upstream `signOut`,
/// index.ts:704-712). The next connection attempt re-runs the challenge.
async fn sign_out_server(manager: &McpManager, name: &str) -> Result<String, String> {
    let Some(entry) = manager.registry().get(name) else {
        return Err(format!("No MCP server named \"{name}\"."));
    };
    let Some(url) = crate::mcp::oauth::oauth_server_url(&entry.config) else {
        return Err(format!(
            "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
    };
    let removed = manager
        .oauth_store()
        .for_server(name, url)
        .remove()
        .await
        .map_err(|error| format!("Could not update the MCP OAuth credentials: {error}"))?;
    manager.disconnect(name).await;
    Ok(if removed {
        format!("Signed out of MCP server \"{name}\".")
    } else {
        format!("No stored credentials for MCP server \"{name}\".")
    })
}

/// Reconnect one server: drop the live connection and re-discover it
/// (upstream `reconnect`, index.ts:714-733).
async fn reconnect_server(
    manager: &McpManager,
    state: &McpSessionState,
    name: &str,
) -> Result<String, String> {
    let Some(entry) = manager.registry().get(name) else {
        return Err(format!("No MCP server named \"{name}\"."));
    };
    if !entry.config.is_enabled() {
        return Err(format!("MCP server \"{name}\" is disabled."));
    }
    manager.disconnect(name).await;
    match manager.discover_server_and_publish(name).await {
        Ok(tools) => {
            state.recompute(manager);
            Ok(format!(
                "Reconnected to MCP server \"{name}\" ({} tools).",
                tools.len()
            ))
        }
        Err(error) => Err(format!(
            "MCP server \"{name}\" failed to reconnect: {error}"
        )),
    }
}

/// Write one enable/exposure patch into the server's defining `mcp.json`
/// (upstream `updateMcpServerConfig`, config.ts:169-193) through the same
/// writer `pir mcp add/remove` use ([`crate::coding_agent::cli::mcp_config_upsert`]).
///
/// Defaults are kept minimal like upstream: enabling removes the `enabled` key
/// (the default is on), and setting the exposure the entry already resolves to
/// removes the `exposure` key. `Ok(None)` means the entry is
/// extension-registered, so the change is session-only. The entry's other
/// fields are preserved verbatim.
fn persist_server_patch(
    entry: &McpServerEntry,
    enabled: Option<bool>,
    exposure: Option<(McpExposure, bool)>,
) -> Result<Option<std::path::PathBuf>, String> {
    let path = match &entry.source {
        McpServerSource::GlobalConfig(path) | McpServerSource::ProjectConfig(path) => path.clone(),
        McpServerSource::Extension(_) => return Ok(None),
    };
    // A source path that does not exist cannot define the entry (a test or a
    // host registered the entry in memory): the change stays session-only.
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not update {}: {error}", path.display())),
    };
    let root: JsonValue = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("Could not update {}: {error}", path.display()))?;
    // The upstream wrapper shape; a legacy flat file is migrated by the writer
    // below, so read the entry from either shape.
    let servers = root
        .get("mcpServers")
        .and_then(JsonValue::as_object)
        .or_else(|| root.as_object());
    let Some(object) = servers
        .and_then(|servers| servers.get(&entry.name))
        .and_then(JsonValue::as_object)
        .cloned()
    else {
        // The file does not define the name (the entry came from an extension
        // or an in-memory registration): keep the change session-only rather
        // than inventing an entry the file never had.
        return Ok(None);
    };
    let mut object = object;
    if let Some(enabled) = enabled {
        if enabled {
            object.remove("enabled");
        } else {
            object.insert("enabled".to_owned(), JsonValue::Bool(false));
        }
    }
    if let Some((exposure, keep_default)) = exposure {
        if keep_default {
            object.remove("exposure");
        } else {
            object.insert(
                "exposure".to_owned(),
                serde_json::to_value(exposure).map_err(|error| error.to_string())?,
            );
        }
    }
    crate::coding_agent::cli::mcp_config_upsert(&path, &entry.name, &JsonValue::Object(object))
        .map_err(|error| format!("Could not update {}: {error}", path.display()))?;
    Ok(Some(path))
}

/// Apply an enable/disable change (upstream `setEnabled`, index.ts:735-767):
/// persist it for a config-file server (extension registrations are
/// session-only), update the live entry, drop the connection when disabling,
/// and recompute the discovery activation.
async fn set_server_enabled(
    manager: &McpManager,
    state: &McpSessionState,
    agent_dir: &std::path::Path,
    name: &str,
    enabled: bool,
) -> Result<String, String> {
    let Some(entry) = manager.registry().get(name) else {
        if enabled {
            state.clear_override(name);
            if restore_config_server(manager, state, agent_dir, name) {
                // The restored entry has no discovery snapshot (`/mcp remove`
                // forgot it), so re-discover before the command's refresh
                // rebuild registers the bridges. A failed discovery keeps the
                // enable successful; the next use connects lazily.
                manager.mark_tools_dirty();
                let _ = manager.discover_server_and_publish(name).await;
                state.recompute(manager);
                return Ok(format!("Enabled MCP server \"{name}\" for this session."));
            }
        }
        return Err(format!("No MCP server named \"{name}\"."));
    };
    let location = match persist_server_patch(&entry, Some(enabled), None)? {
        Some(path) => {
            // The file is now the truth; a session override would mask later
            // edits to it.
            state.clear_override(name);
            format!(" (saved to {})", path.display())
        }
        None => {
            state.update_override(name, |patch| patch.enabled = Some(enabled));
            " for this session".to_owned()
        }
    };
    manager.registry().register(McpServerEntry {
        config: config_with_enabled(&entry.config, enabled),
        ..entry
    });
    if !enabled {
        manager.disconnect(name).await;
    }
    // The rebuild input changed without a new tool list (a disable keeps the
    // snapshot so the bridges re-register `hidden`, an enable restores their
    // configured exposure), so the dirty flag is explicit. Enabling also
    // re-discovers: the connection was dropped on disable (or the snapshot
    // was forgotten by `/mcp remove`), and the rebuild that follows this
    // command publishes the fresh bridges.
    manager.mark_tools_dirty();
    if enabled {
        let _ = manager.discover_server_and_publish(name).await;
    }
    state.recompute(manager);
    Ok(format!(
        "{} MCP server \"{name}\"{location}.",
        if enabled { "Enabled" } else { "Disabled" }
    ))
}

/// Apply an exposure change (upstream `setExposure`, index.ts:769-786):
/// persist it for a config-file server, update the live entry and recompute
/// the discovery activation. The tools' declared exposure moves at the next
/// registry rebuild.
fn set_server_exposure(
    manager: &McpManager,
    state: &McpSessionState,
    name: &str,
    exposure: McpExposure,
) -> Result<String, String> {
    let Some(entry) = manager.registry().get(name) else {
        return Err(format!("No MCP server named \"{name}\"."));
    };
    let builtin_tools = state.builtin_tools_for(name);
    let base = entry.config.base();
    // Writing the value the entry already resolves to removes the key
    // (upstream keeps default values out of the file).
    let keep_default =
        base.exposure.is_none() && base.effective_exposure(builtin_tools) == exposure;
    let location = match persist_server_patch(&entry, None, Some((exposure, keep_default)))? {
        Some(path) => {
            state.clear_override(name);
            format!(" (saved to {})", path.display())
        }
        None => {
            state.update_override(name, |patch| patch.exposure = Some(exposure));
            " for this session".to_owned()
        }
    };
    manager.registry().register(McpServerEntry {
        config: config_with_exposure(&entry.config, exposure),
        ..entry
    });
    // The exposure lives on the entry, not in the discovery snapshot: a
    // rebuild is what re-registers each bridge with the new reach, so mark
    // the manager dirty for the refresh that follows the command.
    manager.mark_tools_dirty();
    state.recompute(manager);
    Ok(format!(
        "Set the exposure of MCP server \"{name}\" to {}{location}.",
        crate::mcp::manager::exposure_label(exposure)
    ))
}

/// Remove one server for the rest of the session (pir's extension; upstream
/// has no remove action). The file entry is not touched, so `/mcp enable`
/// restores it.
async fn remove_server(
    manager: &McpManager,
    state: &McpSessionState,
    name: &str,
) -> Result<String, String> {
    if manager.registry().get(name).is_none() {
        return Err(format!("No MCP server named \"{name}\"."));
    }
    state.update_override(name, |patch| patch.removed = true);
    manager.registry().unregister(name);
    manager.forget_discovered(name);
    manager.disconnect(name).await;
    // `forget_discovered` deliberately leaves the dirty flag alone (its config
    // sync callers run inside a rebuild). This caller is a command: the
    // refresh that follows it must drop the removed server's bridges now.
    manager.mark_tools_dirty();
    state.recompute(manager);
    Ok(format!("Removed MCP server \"{name}\" for this session."))
}

/// Run one parsed action (shared by the subcommands and the manager menu).
async fn run_action(
    manager: &McpManager,
    state: &McpSessionState,
    agent_dir: &std::path::Path,
    action: McpAction,
    server: Option<&str>,
) -> Result<String, String> {
    match action {
        McpAction::List => {
            if server.is_some() {
                return Err(MCP_USAGE.to_owned());
            }
            Ok(status_text(manager, state).await)
        }
        McpAction::Enable | McpAction::Disable | McpAction::Remove => {
            let Some(name) = server else {
                return Err(format!("Usage: /mcp {} <server>", action.as_str()));
            };
            match action {
                McpAction::Enable => {
                    set_server_enabled(manager, state, agent_dir, name, true).await
                }
                McpAction::Disable => {
                    set_server_enabled(manager, state, agent_dir, name, false).await
                }
                _ => remove_server(manager, state, name).await,
            }
        }
        McpAction::Login | McpAction::Logout | McpAction::Reconnect => {
            let servers = command_servers(manager, state).await;
            let ui = state.context().and_then(|context| context.ui_arc());
            let picked = match action {
                McpAction::Reconnect => {
                    pick_server(
                        &servers,
                        server,
                        |server| server.has_connection,
                        |server| server.status.enabled,
                        |server| {
                            matches!(
                                server.status.state,
                                crate::mcp::manager::ConnectionState::Failed { .. }
                                    | crate::mcp::manager::ConnectionState::Disconnected { .. }
                            )
                        },
                        "No enabled MCP server to reconnect.",
                        ui.as_deref(),
                    )
                    .await
                }
                _ => {
                    pick_server(
                        &servers,
                        server,
                        CommandServer::uses_oauth,
                        CommandServer::uses_oauth,
                        |server| {
                            matches!(
                                server.status.state,
                                crate::mcp::manager::ConnectionState::NeedsAuth { .. }
                            )
                        },
                        "No enabled MCP server uses OAuth. Only HTTP servers without an Authorization header do.",
                        ui.as_deref(),
                    )
                    .await
                }
            }?;
            let Some(name) = picked else {
                return Ok(String::new());
            };
            match action {
                McpAction::Login => sign_in_server(manager, state, &name).await,
                McpAction::Logout => sign_out_server(manager, &name).await,
                _ => reconnect_server(manager, state, &name).await,
            }
        }
    }
}

/// The UI service when the bound session is the interactive TUI (upstream's
/// `ctx.mode === "tui"` manager path).
fn manager_ui(state: &McpSessionState) -> Option<Arc<dyn crate::plugins::services::UiService>> {
    let context = state.context()?;
    let session = context.session()?;
    if session.mode() != crate::plugins::services::ExtensionMode::Interactive || !session.has_ui() {
        return None;
    }
    context.ui_arc()
}

/// The per-server action list (upstream `serverMenu`, index.ts:860-926).
fn server_actions(server: &CommandServer) -> Vec<McpAction> {
    use crate::mcp::manager::ConnectionState;
    if !server.status.enabled {
        return vec![McpAction::Enable];
    }
    let mut actions = Vec::new();
    if matches!(server.status.state, ConnectionState::NeedsAuth { .. }) {
        actions.push(McpAction::Login);
    }
    if matches!(
        server.status.state,
        ConnectionState::NeedsAuth { .. }
            | ConnectionState::Failed { .. }
            | ConnectionState::Disconnected { .. }
            | ConnectionState::Connected
    ) {
        actions.push(McpAction::Reconnect);
    }
    if matches!(server.status.state, ConnectionState::Connected) && server.uses_oauth() {
        actions.push(McpAction::Logout);
    }
    actions.push(McpAction::Disable);
    actions.push(McpAction::Remove);
    actions
}

/// The interactive manager (upstream `manage`, index.ts:1000-1009): pick a
/// server, run an action, come back.
///
/// Every wait is a dialog request settled by the TUI's own loop (the manager
/// runs on the session worker, so the input thread keeps running). Returns the
/// last action's message.
async fn run_manager(
    manager: &McpManager,
    state: &McpSessionState,
    agent_dir: &std::path::Path,
    ui: &dyn crate::plugins::services::UiService,
) -> Result<Option<String>, String> {
    use crate::plugins::services::{DialogOptions, SelectOption};
    let unavailable = || "MCP manager is unavailable.".to_owned();
    let mut last: Option<String> = None;
    loop {
        let mut servers = command_servers(manager, state).await;
        if servers.is_empty() {
            return Ok(Some(status_text(manager, state).await));
        }
        servers.sort_by(|left, right| {
            attention_rank(&left.status.state, left.status.enabled)
                .cmp(&attention_rank(&right.status.state, right.status.enabled))
                .then_with(|| left.name().cmp(right.name()))
        });
        let options: Vec<SelectOption> = servers
            .iter()
            .map(|server| {
                SelectOption::new(
                    format!(
                        "{} — {} · {} · {}",
                        server.name(),
                        server.state_label(),
                        crate::mcp::manager::exposure_label(server.status.exposure),
                        source_label(&server.status.source)
                    ),
                    server.name().to_owned(),
                )
            })
            .collect();
        let Some(name) = ui
            .select("MCP servers", &options, DialogOptions::default())
            .await
            .map_err(|_| unavailable())?
        else {
            return Ok(last);
        };
        // Inner loop: stay on this server until the user goes back.
        loop {
            let Some(server) = command_servers(manager, state)
                .await
                .into_iter()
                .find(|server| server.name() == name)
            else {
                break;
            };
            let mut options: Vec<SelectOption> = server_actions(&server)
                .into_iter()
                .map(|action| {
                    let label = match action {
                        McpAction::Login => "Sign in",
                        McpAction::Logout => "Sign out",
                        McpAction::Reconnect => "Reconnect",
                        McpAction::Enable => "Enable",
                        McpAction::Disable => "Disable",
                        McpAction::Remove => "Remove for this session",
                        McpAction::List => "Status",
                    };
                    SelectOption::new(label, action.as_str())
                })
                .collect();
            options.push(SelectOption::new(
                format!(
                    "Exposure: {}",
                    crate::mcp::manager::exposure_label(server.status.exposure)
                ),
                "exposure",
            ));
            options.push(SelectOption::new("Back", "back"));
            let Some(choice) = ui
                .select(
                    &format!("MCP server {name} — {}", server.state_label()),
                    &options,
                    DialogOptions::default(),
                )
                .await
                .map_err(|_| unavailable())?
            else {
                return Ok(last);
            };
            if choice == "back" {
                break;
            }
            if choice == "exposure" {
                let exposures = [
                    McpExposure::Codemode,
                    McpExposure::Deferred,
                    McpExposure::Direct,
                    McpExposure::Hidden,
                ];
                let exposure_options: Vec<SelectOption> = exposures
                    .iter()
                    .map(|exposure| {
                        let label = crate::mcp::manager::exposure_label(*exposure);
                        SelectOption::new(
                            format!(
                                "{}{}",
                                if *exposure == server.status.exposure {
                                    "✓ "
                                } else {
                                    "  "
                                },
                                label
                            ),
                            label,
                        )
                    })
                    .collect();
                let Some(picked) = ui
                    .select(
                        &format!("Exposure of {name}"),
                        &exposure_options,
                        DialogOptions::default(),
                    )
                    .await
                    .map_err(|_| unavailable())?
                else {
                    continue;
                };
                let Some(exposure) = exposures
                    .iter()
                    .copied()
                    .find(|exposure| crate::mcp::manager::exposure_label(*exposure) == picked)
                else {
                    continue;
                };
                if exposure != server.status.exposure {
                    let message = set_server_exposure(manager, state, &name, exposure)?;
                    ui.notify(crate::plugins::services::NotifyLevel::Info, &message);
                    last = Some(message);
                }
                continue;
            }
            let Some(action) = McpAction::parse(&choice) else {
                continue;
            };
            match run_action(manager, state, agent_dir, action, Some(&name)).await {
                Ok(message) if message.is_empty() => {}
                Ok(message) => {
                    ui.notify(crate::plugins::services::NotifyLevel::Info, &message);
                    last = Some(message);
                }
                Err(message) => {
                    ui.notify(crate::plugins::services::NotifyLevel::Error, &message);
                    last = Some(message);
                }
            }
        }
    }
}

/// The `/mcp` command handler.
///
/// No arguments: the interactive manager in the TUI, or the plain status text
/// in print/RPC mode (upstream `ctx.mode === "tui"` vs `formatStatus()`).
/// `login`/`logout`/`reconnect` mirror upstream's subcommands, including the
/// server-picking rules; `list`/`enable`/`disable`/`remove` are pir's
/// session-scoped extras. An unknown action or a third argument reports the
/// upstream usage string.
///
/// Every mutation ends with [`McpSessionState::recompute`], so a disabled or
/// removed server drops its auto-activated discovery tool immediately and an
/// enabled one restores it.
async fn run_mcp_command(
    manager: &McpManager,
    state: &McpSessionState,
    agent_dir: &std::path::Path,
    args: &str,
) -> crate::extensions::CommandOutcome {
    let outcome = |ok: bool, text: String| crate::extensions::CommandOutcome {
        ok,
        error: Some(text),
    };
    let result = match McpInvocation::parse(args) {
        // Upstream reports the usage via `ctx.ui.notify(MCP_USAGE, "warning")`;
        // the native command channel has one text field plus an ok flag, and a
        // warning is closer to the failure notice than to a success line.
        McpInvocation::Usage => Err(MCP_USAGE.to_owned()),
        McpInvocation::Status => match manager_ui(state) {
            Some(ui) => match run_manager(manager, state, agent_dir, ui.as_ref()).await {
                Ok(Some(text)) => Ok(text),
                // Closed without a message: print the status so the terminal
                // keeps a record.
                Ok(None) => Ok(status_text(manager, state).await),
                Err(text) => Err(text),
            },
            None => Ok(status_text(manager, state).await),
        },
        McpInvocation::Action(action, server) => {
            run_action(manager, state, agent_dir, action, server.as_deref()).await
        }
    };
    match result {
        // A cancelled picker has nothing to report (upstream's notify is never
        // reached either).
        Ok(text) if text.is_empty() => crate::extensions::CommandOutcome {
            ok: true,
            error: None,
        },
        Ok(text) => outcome(true, text),
        Err(text) => outcome(false, text),
    }
}

// ---------------------------------------------------------------------------
// Reach-driven server readiness (upstream `extensions/mcp/index.ts`)
// ---------------------------------------------------------------------------
//
// A tool that reaches other tools declares it on its definition
// (`reachesTools`, the Rust mirror is [`AgentTool::reaches_tools`]): `codemode`
// names the tools a script may call, `tool_search` reaches every tool. The MCP
// side reads that declaration to
//
//   - wait for the servers a call may reach, so their tools are registered
//     before the call runs (upstream `tool_call` handler, index.ts:1191-1209),
//     and
//   - activate the discovery tool a server's configured exposure asks for
//     (upstream `ensureDiscoveryActive`, index.ts:498-537): `codemode` for
//     `codemode` exposure, `tool_search` for `deferred`.
//
// Both decisions live here (the MCP side, as upstream) and are pure functions
// of the tool registry plus the server registry, so a host can call them
// without the plugin owning the session's activation store. The consumers in
// pir are the session host / tool-call gate; see the `UPDATING.md` ledger (D23) for the
// wiring status.

/// Whether a call that reaches `reach` (upstream `ToolDefinition.reachesTools`,
/// parsed into [`ToolReach`]) needs `server`: it reaches every tool, or one of
/// its words contains the server's namespace (upstream `reachesServer`,
/// index.ts:224-228 — `word.includes(namespace)`, so words may over-match).
pub fn reaches_server(reach: &ToolReach, server: &str) -> bool {
    reach.reaches_namespace(&crate::mcp::registry::mcp_namespace(server))
}

/// The namespace a server's tools register under (`mcp__<server>`, with the
/// server name as its alias — the same shape [`McpToolBridge::new`] declares
/// for the bridged tools, and what `describeNamespace("docs")` resolves).
pub fn mcp_server_namespace(server: &str) -> ToolNamespace {
    ToolNamespace {
        name: crate::mcp::registry::mcp_namespace(server),
        aliases: vec![server.to_owned()],
        description: None,
        instructions: None,
    }
}

/// Whether a call that reaches `reach` needs `server` under the D23 namespace
/// ALIAS rule, on top of upstream's [`reaches_server`]: a word that NAMES the
/// server's namespace — its name or one of its aliases (`aliases: [server]`),
/// as written or as the same script identifier (upstream `isNamespaceName`,
/// [`super::namespace_matches`]) — reaches the server too. A JS
/// extension tool that declares `reachesTools: () => ["docs"]` (the server
/// name it knows) therefore waits for the `docs` server exactly as one
/// declaring `["mcp__docs"]` does; `dev_radius` and `dev-radius` name the same
/// server.
///
/// Upstream's rule stays authoritative (words may over-match); this is the
/// narrowing gap the JS-side reach declaration needs to be able to use the
/// server name at all.
pub fn reaches_server_by_alias(reach: &ToolReach, server: &str) -> bool {
    let words = match reach {
        ToolReach::All => return true,
        ToolReach::Names(words) => words,
    };
    let namespace = mcp_server_namespace(server);
    words
        .iter()
        .any(|word| super::namespace_matches(&namespace, word))
}

/// The surfaces of a server config under upstream's defaults: its base
/// exposure (default `codemode`) plus every per-tool override (upstream
/// `configuredExposures`, index.ts:140-142; a set there, a de-duplicated list
/// here).
///
/// Kept for callers that do not carry the file-level `builtinTools` flag;
/// [`configured_exposures_with_builtin_tools`] is the flag-aware form.
pub fn configured_exposures(config: &McpServerConfig) -> Vec<McpExposure> {
    configured_exposures_with_builtin_tools(config, false)
}

/// [`configured_exposures`] with the `mcp.json` `builtinTools` flag: when
/// true (pir's default), a server without an explicit `exposure` resolves to
/// `direct`. Per-tool `toolExposure` overrides are unaffected.
pub fn configured_exposures_with_builtin_tools(
    config: &McpServerConfig,
    builtin_tools: bool,
) -> Vec<McpExposure> {
    config.configured_exposures(builtin_tools)
}

/// The widest config exposure among `configs` under upstream's defaults, in
/// upstream `syncResourceTools`' precedence (`direct` > `codemode` >
/// `deferred`; the base exposure, `exposureOf`, decides); `None` for an
/// empty list or a list of `hidden`-base configs (the resource tools then
/// register `hidden`).
///
/// Kept for callers that do not carry the file-level `builtinTools` flag;
/// the plugin's own path resolves each server's flag and calls
/// [`crate::config::mcp::effective_resource_exposure_for`].
pub fn resource_exposure(configs: &[McpServerConfig]) -> Option<McpExposure> {
    crate::config::mcp::effective_resource_exposure(configs, false)
}

/// The server that owns a registry tool name, resolved by re-running the
/// per-server name assignment over the discovered snapshot (upstream
/// `toolOwners`, index.ts:377/1205). `None` for a name no discovered tool maps
/// to; the assignment is deterministic, so the owner matches the name the
/// plugin registered (hash-suffixed collisions included).
pub fn mcp_tool_owner(
    discovered: &std::collections::BTreeMap<String, Vec<DiscoveredTool>>,
    tool_name: &str,
) -> Option<String> {
    for (server, tools) in discovered {
        let names: Vec<String> = tools.iter().map(|tool| tool.tool.name.clone()).collect();
        if crate::mcp::registry::assign_mcp_tool_names(server, &names)
            .iter()
            .any(|name| name == tool_name)
        {
            return Some(server.clone());
        }
    }
    None
}

/// The `ensureDiscoveryActive` decision (upstream index.ts:498-537): which
/// discovery tools to activate, plus the flags its one-shot "unreachable"
/// warning reads. [`McpPlugin::discovery_activation`] produces it; the
/// session applies `activate` and warns when neither tool ends up reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryActivation {
    /// Tool names to append to the active set (never a replacement).
    pub activate: Vec<String>,
    /// Some enabled server exposes tools through `codemode`.
    pub needs_codemode: bool,
    /// Some enabled server exposes tools through `deferred`.
    pub needs_tool_search: bool,
    /// A registered `codemode` tool declares a reach (`reachesTools`).
    pub has_codemode: bool,
    /// A registered `tool_search` tool declares a reach.
    pub has_tool_search: bool,
    /// The `mcp.json` `autoEnableCodemode` flag (default true).
    pub auto_enable_codemode: bool,
}

impl McpPlugin {
    /// Enabled servers, in registry order.
    fn enabled_servers(&self) -> Vec<McpServerEntry> {
        self.manager
            .registry()
            .list()
            .into_iter()
            .filter(|entry| entry.config.is_enabled())
            .collect()
    }

    /// Refresh the cached resource-server snapshot (upstream
    /// `syncResourceTools` runs on every connection change). Returns whether
    /// the set changed, so a caller can request a registry rebuild.
    ///
    /// The `has_resources` probe needs the live client, and asking for it can
    /// reconnect a dropped transport — so the probe only runs when the set of
    /// CONNECTED servers changed since the last refresh (a fresh connection's
    /// transport is live, so this never reconnects). Between transitions the
    /// cached exposure stands; a server that gained resources surfaces at the
    /// next plugin-registry rebuild.
    pub async fn refresh_resource_servers(&self) -> bool {
        use crate::mcp::manager::ConnectionState;
        let states = self.manager.connection_states().await;
        let connected: Vec<String> = self
            .enabled_servers()
            .into_iter()
            .filter(|entry| matches!(states.get(&entry.name), Some(ConnectionState::Connected)))
            .map(|entry| entry.name)
            .collect();
        {
            let probe = self
                .resource_probe_state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *probe == connected {
                return false;
            }
        }
        *self
            .resource_probe_state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = connected.clone();
        let mut servers = Vec::new();
        for name in &connected {
            if let Ok(client) = self.manager.get_client(name).await {
                if client.has_resources() {
                    servers.push(name.clone());
                }
            }
        }
        servers.sort();
        let next: BTreeSet<String> = servers.into_iter().collect();
        let mut cached = self
            .resource_servers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = *cached != next;
        if changed {
            *cached = next;
        }
        changed
    }

    /// The exposure the aggregate resource tools register with (upstream
    /// `syncResourceTools`): the widest BASE exposure (`exposureOf`, the
    /// per-tool overrides do not count there) among the servers that have
    /// resources — `direct` beats `codemode` beats `deferred` — and `hidden`
    /// while no server has any.
    pub fn resource_tool_exposure(&self) -> ToolExposure {
        let servers = self
            .resource_servers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let configs: Vec<(McpServerConfig, bool)> = servers
            .iter()
            .filter_map(|name| {
                self.manager.registry().get(name).map(|entry| {
                    let builtin_tools = self.state.builtin_tools_for(name);
                    (entry.config, builtin_tools)
                })
            })
            .collect();
        crate::config::mcp::effective_resource_exposure_for(
            configs
                .iter()
                .map(|(config, builtin_tools)| (config, *builtin_tools)),
        )
        .map(tool_exposure)
        .unwrap_or(ToolExposure::Hidden)
    }

    /// The enabled servers a call that reaches `reach` may need (upstream
    /// `readyServers.filter(server => reachesServer(reach, server.entry.name))`,
    /// plus the D23 namespace-alias rule [`reaches_server_by_alias`] so a
    /// reach that names the server finds it too).
    pub fn servers_for_reach(&self, reach: &ToolReach) -> Vec<String> {
        self.enabled_servers()
            .into_iter()
            .filter(|entry| {
                reaches_server(reach, &entry.name) || reaches_server_by_alias(reach, &entry.name)
            })
            .map(|entry| entry.name)
            .collect()
    }

    /// Recompute and apply this session's discovery activation (the
    /// **stateful** counterpart of [`Self::discovery_activation`]).
    ///
    /// Unlike the pure decision, which only reports names to append, this
    /// writes the session's active set through the bound
    /// [`PluginContext`] and tracks ownership:
    ///
    /// - an enabled server with `codemode` exposure (and registrations that
    ///   declare a reach) adds `codemode` unless `autoEnableCodemode` is
    ///   false; a `deferred` exposure adds `tool_search`;
    /// - the names are unioned into the **effective** active set, so an
    ///   explicit `--tools`/`setActiveTools` selection keeps every tool it
    ///   chose (activation never replaces the user's set);
    /// - a name this plugin added is removed when no enabled server needs it
    ///   anymore; a name the user chose (or another extension activated) is
    ///   left alone;
    /// - the write is skipped when the set is unchanged, so repeated rebuilds
    ///   are stable and never duplicate entries.
    ///
    /// Returns the resulting active set (or `None` without a bound context).
    /// Called from `register()` (every rebuild), `on_context` (first bind and
    /// every rebind), the rebuild-tail [`Self::flush_server_changes`], and the
    /// `/mcp` handler after a toggle.
    pub fn recompute_discovery_activation(&self) -> Option<Vec<String>> {
        self.state.recompute(&self.manager)
    }

    /// Test/diagnostic seam: the effective `builtinTools` flag (default
    /// `true`), i.e. whether an entry without an explicit `exposure` resolves
    /// to `direct`.
    #[doc(hidden)]
    pub fn builtin_tools(&self) -> bool {
        self.state.builtin_tools()
    }

    /// Test seam: apply an exposure change through the same path the
    /// interactive manager's exposure picker uses (persist or session
    /// override, live entry update, dirty flag). The `/mcp` text syntax has no
    /// exposure subcommand (upstream exposes it only through the menu), so a
    /// command-refresh test reaches the action through this seam.
    #[doc(hidden)]
    pub fn set_exposure_for_test(
        &self,
        name: &str,
        exposure: crate::config::mcp::McpExposure,
    ) -> Result<String, String> {
        set_server_exposure(&self.manager, &self.state, name, exposure)
    }

    /// Test/diagnostic seam: the discovery tools this plugin currently owns
    /// (`Owned`), i.e. the names it would remove when their last enabled
    /// server disappears.
    #[doc(hidden)]
    pub fn auto_activated_tools(&self) -> Vec<String> {
        let ownership = self
            .state
            .discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut names = Vec::new();
        if ownership.codemode == DiscoveryToolOwnership::Owned {
            names.push(crate::plugins::builtin::CODEMODE_TOOL_NAME.to_owned());
        }
        if ownership.tool_search == DiscoveryToolOwnership::Owned {
            names.push(crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME.to_owned());
        }
        names
    }

    /// Test/diagnostic seam: the direct MCP bridges this plugin currently owns
    /// in the session's active set, i.e. the names it would prune when their
    /// server is disabled or no longer exposes them directly.
    #[doc(hidden)]
    pub fn auto_activated_bridges(&self) -> Vec<String> {
        self.state
            .bridge_owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Test/diagnostic seam: the session's effective active-tool names, when a
    /// context is bound.
    #[doc(hidden)]
    pub fn active_tools_snapshot(&self) -> Option<Vec<String>> {
        let context = self
            .state
            .context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        Some(
            context
                .session()?
                .active_tools()
                .iter()
                .map(|name| name.to_string())
                .collect(),
        )
    }

    /// The discovery tools the configured exposures ask for and the registry
    /// does not have active (upstream `ensureDiscoveryActive`,
    /// index.ts:498-537): `codemode` when some enabled server exposes
    /// `codemode` (unless the `mcp.json` `autoEnableCodemode` flag is false,
    /// default true), `tool_search` for `deferred`. Each is activated only
    /// when a REGISTERED tool of that name declares a reach (`reachesTools`),
    /// so another extension's tool of the same name is never activated; the
    /// names are appended to `active`, never replacing it.
    ///
    /// The presence probe calls the tool's hook with an empty input object
    /// (upstream tests that the hook exists, not what it returns; for both
    /// reaching builtins every input yields `Some`).
    pub fn discovery_tools_to_activate(
        &self,
        registry: &ToolRegistry,
        active: &[String],
    ) -> Vec<String> {
        self.discovery_activation(registry, active).activate
    }

    /// The full [`Self::discovery_tools_to_activate`] decision (upstream's
    /// `ensureDiscoveryActive` locals): the names to activate plus the flags
    /// its one-shot unreachable warning reads. `active` is the EFFECTIVE
    /// active set (upstream `pi.getActiveTools()`).
    pub fn discovery_activation(
        &self,
        registry: &ToolRegistry,
        active: &[String],
    ) -> DiscoveryActivation {
        let auto_enable_codemode = self.state.auto_enable_codemode();
        let exposures: Vec<McpExposure> = self
            .enabled_servers()
            .iter()
            .flat_map(|entry| {
                configured_exposures_with_builtin_tools(
                    &entry.config,
                    self.state.builtin_tools_for(&entry.name),
                )
            })
            .collect();
        let needs_codemode = exposures.contains(&McpExposure::Codemode);
        let needs_tool_search = exposures.contains(&McpExposure::Deferred);
        let declares_reach = |name: &str| {
            registry.get(name).is_some_and(|tool| {
                tool.reaches_tools(&JsonValue::Object(Default::default()))
                    .is_some()
            })
        };
        let has_codemode = declares_reach(crate::plugins::builtin::CODEMODE_TOOL_NAME);
        let has_tool_search = declares_reach(crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME);
        let mut activate = Vec::new();
        if !needs_codemode && !needs_tool_search {
            return DiscoveryActivation {
                activate,
                needs_codemode,
                needs_tool_search,
                has_codemode,
                has_tool_search,
                auto_enable_codemode,
            };
        }
        if needs_codemode
            && auto_enable_codemode
            && has_codemode
            && !active
                .iter()
                .any(|name| name == crate::plugins::builtin::CODEMODE_TOOL_NAME)
        {
            activate.push(crate::plugins::builtin::CODEMODE_TOOL_NAME.to_owned());
        }
        if needs_tool_search
            && has_tool_search
            && !active
                .iter()
                .any(|name| name == crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME)
        {
            activate.push(crate::plugins::builtin::TOOL_SEARCH_TOOL_NAME.to_owned());
        }
        DiscoveryActivation {
            activate,
            needs_codemode,
            needs_tool_search,
            has_codemode,
            has_tool_search,
            auto_enable_codemode,
        }
    }

    /// The servers a tool call waits for before it runs (upstream the mcp
    /// `tool_call` handler, index.ts:1191-1209): the servers the called tool's
    /// reach matches, every enabled server for the aggregate resource tools
    /// ([`crate::mcp::registry::MCP_RESOURCE_TOOLS`], upstream
    /// `RESOURCE_TOOL_NAMES`), else only the server that owns the called MCP
    /// tool (upstream `toolOwners`). Empty for a tool that reaches no MCP tool.
    ///
    /// The reach comes from the registry's synchronous mirror
    /// ([`AgentTool::reaches_tools`]): for a JS extension tool that is the
    /// registry-export snapshot, so a host with the runtime at hand resolves
    /// the LIVE per-call value instead and calls
    /// [`Self::waiting_servers_for_reach`]
    /// (see [`Self::wait_for_tool_call`]).
    pub fn waiting_servers(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        input: &JsonValue,
    ) -> Vec<String> {
        let Some(tool) = registry.get(tool_name) else {
            return Vec::new();
        };
        let reach = tool.reaches_tools(input);
        self.waiting_servers_for_reach(tool_name, reach.as_ref())
    }

    /// [`Self::waiting_servers`] with the call's reach already resolved
    /// (upstream `const reach = tool.reachesTools?.(event.input)`): the servers
    /// the reach matches, else every enabled server for the aggregate resource
    /// tools, else only the server that owns the called MCP tool. `None` is
    /// upstream's `undefined` — the tool declares no reach for this call — and
    /// falls through to the resource/owner rules.
    pub fn waiting_servers_for_reach(
        &self,
        tool_name: &str,
        reach: Option<&ToolReach>,
    ) -> Vec<String> {
        if let Some(reach) = reach {
            return self.servers_for_reach(reach);
        }
        if crate::mcp::registry::MCP_RESOURCE_TOOLS.contains(&tool_name) {
            return self
                .enabled_servers()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
        }
        if crate::mcp::registry::is_mcp_tool_name(tool_name) {
            let discovered = self.manager.discovered_tools();
            if let Some(owner) = mcp_tool_owner(&discovered, tool_name) {
                return vec![owner];
            }
        }
        Vec::new()
    }

    /// Await the servers of [`Self::waiting_servers`], so their tools are
    /// registered before the call runs (upstream `waitForServers`).
    ///
    /// The wait is READINESS-CACHED ([`McpManager::ensure_server_ready`]): a
    /// server that is already connected and whose tool list is current — no
    /// `notifications/tools/list_changed` and no reconnect since the last
    /// successful discovery — is skipped without a `tools/list` round trip.
    /// The per-server cache key and invalidation points live in
    /// [`crate::mcp::manager`]; concurrent calls serialize on the per-server
    /// single-flight discovery, and a failed server is returned from a
    /// bounded error backoff instead of paying its connect timeout on every
    /// call. The waits run CONCURRENTLY, so one unreachable server does not
    /// serialize the healthy ones behind it.
    ///
    /// A discovery that actually runs is PUBLISHED
    /// (`McpManager::ensure_server_ready` replaces the server's discovered
    /// snapshot entry and marks the plugin registry dirty when the tool set
    /// changed), so the rebuild the wait hooks anchor right after this loop
    /// (`crate::agent_session::AgentSession::tool_call_wait_hook`, print's
    /// `crate::coding_agent::print::print_tool_call_wait`) registers the
    /// fresh `mcp__*` bridges for the next loadout. A ready server publishes
    /// nothing: its bridges are already registered.
    ///
    /// A JS extension tool's reach hook is a function of the call's input and
    /// lives in QuickJS, so it is evaluated here for THIS call through the
    /// attached runtime (upstream evaluates it in the same `tool_call` handler,
    /// which is in JS); the registry-export snapshot answers when no runtime is
    /// attached or the probe fails.
    ///
    /// Returns the servers that were covered by the wait (observability/tests):
    /// ready servers included, so the set stays the same as
    /// [`Self::waiting_servers`]. pir note: the run's tool list is seeded once
    /// per run (`crate::agent::agent_loop`), so a server discovered here
    /// contributes its tools to the NEXT loadout rebuild, not to the call
    /// already in flight; the awaited connection is what makes that call's
    /// nested MCP dispatches succeed (`McpManager::call_tool` is lazy anyway).
    pub async fn wait_for_tool_call(
        &self,
        registry: &ToolRegistry,
        tool_name: &str,
        input: &JsonValue,
    ) -> Vec<String> {
        // Upstream's `const tool = pi.getAllTools().find(...); if (!tool) return;`
        let Some(tool) = registry.get(tool_name) else {
            return Vec::new();
        };
        let live = self.live_js_reach(tool.as_ref(), tool_name, input).await;
        let reach = live.unwrap_or_else(|| tool.reaches_tools(input));
        let waiting = self.waiting_servers_for_reach(tool_name, reach.as_ref());
        // Publish, not just probe: the wait hook's tail rebuild
        // (`PluginHost::refresh_registry_if_changed`) only re-runs the
        // registration pass when the discovery marked the registry dirty, so
        // a server first reached here (failed session-start discovery, or
        // registered mid-session) needs the snapshot write to have its tools
        // registered. `ensure_server_ready` does that only when it actually
        // discovers; a cached-ready server is a no-op.
        let waits = waiting
            .iter()
            .map(|server| self.manager.ensure_server_ready(server));
        // The waits must not block the call (a dead server never stalls the
        // batch), but a failure must not vanish either: it is recorded on the
        // server's connection state and in `mcp.log` by the manager. The
        // backoff-cached error returns here too, so this is the one place a
        // per-call failure is visible — report it once per distinct message
        // (the diagnostic router collapses repeats).
        let outcomes = futures_util::future::join_all(waits).await;
        for (server, outcome) in waiting.iter().zip(outcomes) {
            if let Err(error) = outcome {
                crate::ext_diag!(
                    "[ext] MCP server \"{}\" is unavailable: {}",
                    server,
                    crate::mcp::manager::bounded_reason(&error.to_string())
                );
            }
        }
        waiting
    }

    /// The live reach of a JS extension tool call (`None` when the tool is not
    /// a JS tool declaring a `reachesTools` hook — then the registry's
    /// synchronous mirror answers — or when the probe could not run).
    async fn live_js_reach(
        &self,
        tool: &dyn AgentTool,
        tool_name: &str,
        input: &JsonValue,
    ) -> Option<Option<ToolReach>> {
        let extension = tool
            .as_any()?
            .downcast_ref::<crate::agent::tool_execution::ExtensionTool>()?;
        if !extension.declares_js_reach() {
            return None;
        }
        let runtime = self.js_handle()?;
        js_call_reach(&runtime, tool_name, input).await.ok()
    }

    /// The attached JS runtime handle, when extensions are loaded.
    fn js_handle(&self) -> Option<crate::extensions::ExtensionHandle> {
        self.js
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Evaluate a JS extension tool's live `reachesTools` hook for one call
/// (upstream `tool.reachesTools?.(event.input)`) through the runtime's
/// synchronous eval channel. `Ok(None)` is upstream's `undefined` — the hook
/// declares no reach for this call; `Err` means the probe could not answer
/// (thrown hook, Promise answer, dead runtime) and the caller falls back to the
/// registry-export snapshot.
async fn js_call_reach(
    runtime: &crate::extensions::ExtensionHandle,
    tool_name: &str,
    input: &JsonValue,
) -> Result<Option<ToolReach>, String> {
    let literal = |value: &JsonValue| -> Result<String, String> {
        serde_json::to_string(value).map_err(|error| error.to_string())
    };
    // Both arguments travel as JS string literals; the input is the call's JSON
    // text, parsed back inside the prelude.
    let name = literal(&JsonValue::String(tool_name.to_owned()))?;
    let input_json = literal(&JsonValue::String(
        serde_json::to_string(input).map_err(|error| error.to_string())?,
    ))?;
    let code = format!("globalThis.__pir_callToolReach({name}, {input_json})");
    let value = runtime
        .eval_json(&code)
        .await
        .map_err(|error| format!("reachesTools probe failed: {error}"))?;
    if value.get("error").and_then(JsonValue::as_str).is_some() {
        return Err(format!("reachesTools hook failed: {value}"));
    }
    Ok(match value {
        JsonValue::String(text) if text == "all" => Some(ToolReach::All),
        JsonValue::String(_) => Some(ToolReach::Names(Vec::new())),
        JsonValue::Array(items) => Some(ToolReach::Names(
            items
                .iter()
                .filter_map(|item| match item {
                    JsonValue::String(text) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        )),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// The tool bridge
// ---------------------------------------------------------------------------

/// Intern dynamic tool names/descriptions to `'static` (the registry tool
/// surface requires `&'static str`; mirrors
/// `tool_execution::intern_tool_text`).
fn intern_tool_text(text: String) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashSet::new()));
    let mut pool = pool
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = pool.get(text.as_str()) {
        return existing;
    }
    let interned: &'static str = Box::leak(text.into_boxed_str());
    pool.insert(interned);
    interned
}

/// One MCP server tool, exposed as a native [`AgentTool`].
pub struct McpToolBridge {
    manager: Arc<McpManager>,
    server: String,
    tool: String,
    name: &'static str,
    description: &'static str,
    parameters: Option<JsonValue>,
    output_schema: Option<JsonValue>,
    exposure: ToolExposure,
    namespace: ToolNamespace,
}

/// Agent-level exposure of an MCP exposure (upstream `toToolExposure`):
/// `codemode` and `deferred` both leave tools out of the codemode
/// description — they are not declared to the model — and differ only in
/// which activation surface reaches them.
fn tool_exposure(exposure: crate::config::mcp::McpExposure) -> ToolExposure {
    use crate::config::mcp::McpExposure;
    match exposure {
        McpExposure::Codemode | McpExposure::Deferred => ToolExposure::Deferred,
        McpExposure::Direct => ToolExposure::Direct,
        McpExposure::Hidden => ToolExposure::Hidden,
    }
}

impl McpToolBridge {
    /// Build the bridge for one discovered tool with its pre-assigned
    /// registry name, using the exposure the manager resolved (upstream's
    /// `codemode` default).
    pub fn new(manager: Arc<McpManager>, discovered: DiscoveredTool, registered: String) -> Self {
        let exposure = discovered.exposure;
        Self::with_effective_exposure(manager, discovered, registered, exposure)
    }

    /// [`Self::new`] with the tool's effective exposure resolved against the
    /// file-level `builtinTools` default (the plugin's registration path):
    /// the manager's resolved exposure keeps upstream's `codemode` default,
    /// while `builtinTools: true` (pir's default) makes an unset `exposure`
    /// resolve to `direct`.
    pub fn with_effective_exposure(
        manager: Arc<McpManager>,
        discovered: DiscoveredTool,
        registered: String,
        exposure: McpExposure,
    ) -> Self {
        let server = discovered.server.clone();
        let tool = discovered.tool.name.clone();
        let description = discovered
            .tool
            .description
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| {
                discovered
                    .tool
                    .title
                    .clone()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| format!("MCP tool {tool} from server {server}"))
            });
        let namespace_description = manager
            .registry()
            .get(&server)
            .and_then(|entry| entry.config.base().description.clone());
        let mut namespace = mcp_server_namespace(&server);
        // `describeNamespace("docs")` finds `mcp__docs` (upstream
        // `registerTools`'s `aliases: [server]`).
        namespace.description = namespace_description;
        Self {
            manager,
            server,
            tool,
            name: intern_tool_text(registered),
            description: intern_tool_text(description),
            parameters: Some(normalize_parameters(discovered.tool.input_schema.clone())),
            output_schema: discovered.tool.output_schema.clone(),
            exposure: tool_exposure(exposure),
            namespace,
        }
    }
}

/// Tool input schemas must be objects; servers may omit `type` and providers
/// reject object schemas without `properties` (upstream `toParameters`).
fn normalize_parameters(schema: JsonValue) -> JsonValue {
    let mut schema = match schema {
        JsonValue::Object(map) => JsonValue::Object(map),
        _ => serde_json::json!({}),
    };
    if schema.get("type").is_none() {
        schema["type"] = JsonValue::String("object".to_owned());
    }
    if schema.get("properties").is_none() {
        schema["properties"] = serde_json::json!({});
    }
    schema
}

impl AgentTool for McpToolBridge {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> Option<JsonValue> {
        self.parameters.clone()
    }

    fn output_schema(&self) -> Option<JsonValue> {
        // DRIFT D25 (upstream `24382bedd`): MCP tools declare the
        // `CallToolResult` codemode scripts receive (upstream
        // `createMcpResultSchema`) instead of leaning on a codemode-side MCP
        // detection (`MCP_TYPESCRIPT_PREAMBLE`/`mcpStructuredContentSchema`,
        // both removed upstream). The server tool's own `outputSchema` rides
        // `structuredContent` with its local `$ref`s relocated below it, so
        // they still resolve. Without a server schema, `structuredContent` is
        // the EMPTY schema — not `{ "type": "object" }` — because structured
        // content may be any JSON value (upstream `67996ab75`, the DRIFT D6
        // leftover); the declaration renderer turns `{}` into `unknown`.
        Some(crate::plugins::codemode::mcp_result_schema(
            self.output_schema.as_ref(),
        ))
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // MCP tools execute sequentially by default (upstream default
        // execution mode for exposed MCP tools).
        ToolExecutionMode::Sequential
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn namespace(&self) -> Option<ToolNamespace> {
        Some(self.namespace.clone())
    }

    fn has_external_side_effects(&self) -> bool {
        true
    }

    fn run(&self, args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let manager = self.manager.clone();
        let server = self.server.clone();
        let tool = self.tool.clone();
        let arguments = args.clone();
        let aborted = ctx.is_aborted();
        // Live signal clone: the manager re-checks it before every dispatch,
        // so a call abandoned while an OAuth refresh is in flight is not sent
        // to the server (#10598; the pre-check above is only a snapshot).
        let signal = ctx.signal.clone();
        Box::pin(async move {
            if aborted {
                return ToolResult::error(
                    "Request was aborted before this tool call could execute",
                );
            }
            match manager
                .call_tool(&server, &tool, &arguments, signal.as_ref())
                .await
            {
                Ok(result) => convert_mcp_result(&server, &tool, result),
                Err(crate::mcp::error::McpError::Aborted) => {
                    ToolResult::error("Request was aborted before this tool call could execute")
                }
                Err(error) => ToolResult::error(error.to_string()),
            }
        })
    }
}

/// Whether a MIME type renders as text (upstream `isTextMimeType`).
fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else {
        return false;
    };
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    base.starts_with("text/")
        || base == "application/json"
        || base.ends_with("+json")
        || base.ends_with("+xml")
}

/// Whether a MIME type is an image the model can receive as an image block.
fn is_image_mime_type(mime_type: Option<&str>) -> bool {
    mime_type.is_some_and(|mime_type| {
        mime_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .starts_with("image/")
    })
}

/// The model-facing image payload of one block, if it is an image block or an
/// embedded resource with an image MIME type: `(base64 data, mime)`.
///
/// MCP carries images as `{type:"image", data, mimeType}` blocks and also as
/// `embedded resource` blobs; both reach the model as real
/// [`crate::ai::types::ImageContent`] (non-vision adapters downgrade them to a
/// placeholder themselves — that mechanism already exists).
fn image_payload(block: &crate::mcp::protocol::ContentBlock) -> Option<(String, String)> {
    match block.kind.as_str() {
        "image" => {
            let data = block.data.as_ref().filter(|data| !data.is_empty())?;
            let mime_type = block
                .mime_type
                .clone()
                .filter(|mime| !mime.trim().is_empty())
                .unwrap_or_else(|| "image/png".to_owned());
            Some((data.clone(), mime_type))
        }
        "resource" => {
            let resource = block.resource.as_ref()?;
            // `text` wins over a blob in the text path; keep that precedence
            // here too.
            if resource.text.is_some() || !is_image_mime_type(resource.mime_type.as_deref()) {
                return None;
            }
            let blob = resource.blob.as_ref().filter(|blob| !blob.is_empty())?;
            Some((
                blob.clone(),
                resource
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "image/png".to_owned()),
            ))
        }
        _ => None,
    }
}

/// Write one block's bytes to a restricted spill file, returning the path the
/// model can read (`fullOutputPath`). Best effort: `None` leaves the text-only
/// description in place.
fn spill_bytes(bytes: &[u8]) -> Option<String> {
    let mut spill = crate::coding_agent::tools::spill::create_spill_file()?;
    let path = spill.path().to_path_buf();
    // The writer enforces the per-file cap; an oversized block is not
    // advertised (the partial file is removed below).
    if spill.write_all(bytes).is_err() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    if spill.flush().is_err() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    drop(spill);
    Some(path.display().to_string())
}

fn spill_text(text: &str) -> Option<String> {
    spill_bytes(text.as_bytes())
}

/// Model-facing text of one content block (upstream `blockToContent`; binary
/// resources are described rather than saved to temp files in this stream —
/// [`convert_mcp_result`] spills non-image blobs before falling back here).
fn block_to_text(block: &crate::mcp::protocol::ContentBlock) -> String {
    match block.kind.as_str() {
        "text" => block.text.clone().unwrap_or_default(),
        "image" => format!(
            "[image: {}]",
            block.mime_type.as_deref().unwrap_or("unknown type")
        ),
        "audio" => format!(
            "[audio: {}]",
            block.mime_type.as_deref().unwrap_or("unknown type")
        ),
        "resource_link" => {
            let uri = block.uri.as_deref().unwrap_or("");
            let name = block
                .title
                .as_deref()
                .or(block.name.as_deref())
                .unwrap_or("");
            let details: Vec<String> = [
                block.mime_type.clone(),
                block.size.map(|size| format!("{size} bytes")),
            ]
            .into_iter()
            .flatten()
            .collect();
            let description = block
                .description
                .as_deref()
                .map(|text| format!(": {text}"))
                .unwrap_or_default();
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            format!("[Resource {uri} \"{name}\"{details}{description}]")
        }
        "resource" => {
            let Some(resource) = &block.resource else {
                return String::new();
            };
            match (&resource.text, &resource.blob) {
                (Some(text), _) => text.clone(),
                (None, Some(blob)) => {
                    use base64::Engine as _;
                    let decoded = base64::engine::general_purpose::STANDARD.decode(blob);
                    match decoded {
                        Ok(bytes) if is_text_mime_type(resource.mime_type.as_deref()) => {
                            String::from_utf8_lossy(&bytes).into_owned()
                        }
                        Ok(bytes) => format!(
                            "[Binary resource {} ({}, {} bytes)]",
                            resource.uri,
                            resource.mime_type.as_deref().unwrap_or("unknown type"),
                            bytes.len()
                        ),
                        Err(_) => {
                            format!("[Binary resource {} could not be decoded]", resource.uri)
                        }
                    }
                }
                (None, None) => String::new(),
            }
        }
        _ => String::new(),
    }
}

/// Keep model-facing text within the byte cap with a middle cut (upstream
/// `truncateMiddle`): the kept head/tail ranges are measured in bytes and
/// backed off to UTF-8 character boundaries, so multi-byte text does not
/// overshoot the cap (upstream truncate.ts).
fn truncate_middle(text: &str) -> String {
    let original_bytes = text.len();
    if original_bytes <= MCP_OUTPUT_MAX_BYTES {
        return text.to_owned();
    }
    let tokens = original_bytes / 4;
    let keep = MCP_OUTPUT_MAX_BYTES / 2;
    let mut start_end = keep;
    while start_end > 0 && !text.is_char_boundary(start_end) {
        start_end -= 1;
    }
    let mut end_start = original_bytes.saturating_sub(keep);
    while end_start < original_bytes && !text.is_char_boundary(end_start) {
        end_start += 1;
    }
    let start = &text[..start_end];
    let end = &text[end_start..];
    format!(
        "Warning: truncated output (original token count: {tokens})\n\n{start}\n\n[... output truncated ({original_bytes} bytes total) ...]\n\n{end}"
    )
}

/// Map an MCP `CallToolResult` onto pir's [`ToolResult`] (upstream
/// `convertMcpResult`): content blocks are flattened to text, `details`
/// carries `{server, tool}`, the structured result rides `structuredContent`,
/// and `isError` marks the result as an error for the model.
///
/// `structuredContent` is any JSON value (MCP 2026-07-28; upstream #10416):
/// arrays, `null`, and primitives are kept as sent, and without content blocks
/// the JSON becomes the model-facing text — an explicit `null` renders `null`
/// exactly like upstream `toLlmContent` (rmcp keeps a present
/// `"structuredContent": null` distinct from an absent field).
pub fn convert_mcp_result(server: &str, tool: &str, result: CallToolResult) -> ToolResult {
    let mut text = String::new();
    let mut text_truncated = false;
    let mut images: Vec<crate::ai::types::ImageContent> = Vec::new();
    // Path of the spill file backing `details.fullOutputPath`: either a
    // truncated model-facing text (the whole text is written) or a non-image
    // embedded resource blob.
    let mut full_output_path: Option<String> = None;
    for block in &result.content {
        // Image blocks / image-MIME resource blobs become real image content
        // (the adapter downgrades them for non-vision models).
        if let Some((data, mime_type)) = image_payload(block) {
            let block_text = format!("[image: {mime_type}]");
            images.push(crate::ai::types::ImageContent {
                data,
                mime_type: mime_type.clone(),
            });
            if !text.is_empty() && !append_bounded(&mut text, "\n", MAX_STRUCTURED_RESULT_BYTES) {
                text_truncated = true;
                break;
            }
            if !append_bounded(&mut text, &block_text, MAX_STRUCTURED_RESULT_BYTES) {
                text_truncated = true;
                break;
            }
            continue;
        }
        // A non-image embedded resource blob is binary the model cannot read
        // as text: write the decoded bytes to a restricted spill file and
        // point at it, instead of dropping the payload after measuring it.
        if block.kind == "resource" {
            if let Some(resource) = &block.resource {
                // Text resources keep the `block_to_text` path (the text wins
                // over any blob, as before).
                if resource.text.is_none() {
                    // An EMPTY blob carries no bytes: spilling it would create
                    // (and advertise) an empty file under `fullOutputPath`.
                    if let Some(blob) = resource.blob.as_ref().filter(|blob| !blob.is_empty()) {
                        let block_text = match base64::engine::general_purpose::STANDARD
                            .decode(blob)
                        {
                            Ok(bytes) if is_text_mime_type(resource.mime_type.as_deref()) => {
                                String::from_utf8_lossy(&bytes).into_owned()
                            }
                            Ok(bytes) => {
                                let mut label = format!(
                                    "[Binary resource {} ({}, {} bytes)]",
                                    resource.uri,
                                    resource.mime_type.as_deref().unwrap_or("unknown type"),
                                    bytes.len()
                                );
                                if let Some(path) = spill_bytes(&bytes) {
                                    label.push_str(&format!(" (saved to {path})"));
                                    // `fullOutputPath` is singular: advertise
                                    // the FIRST spilled blob (the earlier file
                                    // stays reachable through its label text;
                                    // overwriting left the earlier spill
                                    // orphaned and unreachable from details).
                                    if full_output_path.is_none() {
                                        full_output_path = Some(path);
                                    }
                                }
                                label
                            }
                            Err(_) => {
                                format!("[Binary resource {} could not be decoded]", resource.uri)
                            }
                        };
                        if !block_text.is_empty() {
                            if !text.is_empty()
                                && !append_bounded(&mut text, "\n", MAX_STRUCTURED_RESULT_BYTES)
                            {
                                text_truncated = true;
                                break;
                            }
                            if !append_bounded(&mut text, &block_text, MAX_STRUCTURED_RESULT_BYTES)
                            {
                                text_truncated = true;
                                break;
                            }
                        }
                        continue;
                    }
                }
            }
        }
        let block_text = block_to_text(block);
        if block_text.is_empty() {
            continue;
        }
        if !text.is_empty() && !append_bounded(&mut text, "\n", MAX_STRUCTURED_RESULT_BYTES) {
            text_truncated = true;
            break;
        }
        if !append_bounded(&mut text, &block_text, MAX_STRUCTURED_RESULT_BYTES) {
            text_truncated = true;
            break;
        }
    }
    if text_truncated {
        text.push_str(&format!(
            "\n\n[... output truncated at the {}-byte ({}) cap ...]",
            MAX_STRUCTURED_RESULT_BYTES,
            frame_cap_label(MAX_STRUCTURED_RESULT_BYTES)
        ));
    }
    if text.is_empty() {
        if let Some(structured) = &result.structured_content {
            match serialized_len(structured) {
                Some(bytes) if bytes > MAX_STRUCTURED_RESULT_BYTES => {
                    text = format!(
                        "[... structured content omitted: {bytes} bytes exceeds the {}-byte ({}) cap ...]",
                        MAX_STRUCTURED_RESULT_BYTES,
                        frame_cap_label(MAX_STRUCTURED_RESULT_BYTES)
                    );
                }
                _ => {
                    text = serde_json::to_string_pretty(structured)
                        .unwrap_or_else(|_| structured.to_string());
                }
            }
        }
    }
    if result.is_error.unwrap_or(false) && text.trim().is_empty() {
        text = format!("MCP tool {server}/{tool} returned an error");
    }
    // A model-facing text above the cap is cut in the middle; the ORIGINAL
    // text is written to a restricted spill file and advertised through
    // `details.fullOutputPath`, so the model can read what the cut dropped.
    let text_over_cap = text.len() > MCP_OUTPUT_MAX_BYTES;
    let text_spill_path = if text_over_cap {
        spill_text(&text)
    } else {
        None
    };
    let mut model_text = if text_over_cap {
        truncate_middle(&text)
    } else {
        text
    };
    if let Some(path) = &text_spill_path {
        model_text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        // The text spill is the primary advertised output; a binary resource
        // spill only stands in when no text was cut.
        full_output_path = Some(path.clone());
    }
    let mut tool_result = ToolResult::ok(model_text);
    let mut details = serde_json::json!({ "server": server, "tool": tool });
    if let Some(path) = &full_output_path {
        details["fullOutputPath"] = JsonValue::String(path.clone());
    }
    tool_result.details = Some(details);
    tool_result.images = images;
    let mut script_result = serde_json::json!({
        "content": result.content,
        "isError": result.is_error.unwrap_or(false),
    });
    if let Some(structured) = &result.structured_content {
        script_result["structuredContent"] = structured.clone();
    }
    tool_result.structured_content = Some(bounded_structured_payload(
        script_result,
        MAX_STRUCTURED_RESULT_BYTES,
    ));
    tool_result.is_error = result.is_error.unwrap_or(false);
    tool_result
}

// ---------------------------------------------------------------------------
// Aggregate resource tools (upstream `extensions/mcp/resources.ts`)
// ---------------------------------------------------------------------------
//
// `list_mcp_resources`, `list_mcp_resource_templates`, and
// `read_mcp_resource` (upstream the tool names Codex and opencode use) cover
// every connected server with resources through a `server` argument. They are
// registered by [`McpPlugin::register`] like upstream `syncResourceTools`:
// with the WIDEST base exposure among the servers that have resources, and
// `hidden` while no server has any. Their names are what
// [`McpPlugin::waiting_servers`]' resource branch matches, so a call to one of
// them waits for every enabled server.
//
// Two pir notes against upstream:
// - the exposure is read from the cached resource-server snapshot refreshed
//   on event dispatch ([`McpPlugin::refresh_resource_servers`]) instead of at
//   the exact connection-change moment, so a server that gains resources
//   without changing its tool set shows the wider exposure from the next
//   rebuild/dispatch on;
// - `list_mcp_resource_templates` reads `resources/templates/list` through
//   the client ([`crate::mcp::client::McpClient::list_resource_templates`]),
//   which tolerance-maps a server's method-not-found answer to an empty
//   listing (upstream `withoutTemplates`).

/// Which aggregate resource tool an [`McpResourceTool`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpResourceToolKind {
    ListResources,
    ListResourceTemplates,
    ReadResource,
}

impl McpResourceToolKind {
    fn name(self) -> &'static str {
        match self {
            Self::ListResources => crate::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            Self::ListResourceTemplates => crate::mcp::registry::LIST_MCP_RESOURCE_TEMPLATES_TOOL,
            Self::ReadResource => crate::mcp::registry::READ_MCP_RESOURCE_TOOL,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ListResources => {
                "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible."
            }
            Self::ListResourceTemplates => {
                "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible."
            }
            Self::ReadResource => {
                "Read a specific resource from an MCP server given the server name and resource URI."
            }
        }
    }

    fn parameters(self) -> JsonValue {
        match self {
            Self::ReadResource => serde_json::json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources.",
                    },
                    "uri": {
                        "type": "string",
                        "description": "Resource URI to read. Must be one of the URIs returned by list_mcp_resources.",
                    },
                },
                "required": ["server", "uri"],
                "additionalProperties": false,
            }),
            _ => serde_json::json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "MCP server name. Omit to list every server with resources.",
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Opaque cursor from a previous call with the same server; omit for the first page.",
                    },
                },
                "additionalProperties": false,
            }),
        }
    }

    fn output_schema(self) -> JsonValue {
        let optional_string = serde_json::json!({ "type": "string" });
        let listing_errors = serde_json::json!({
            "type": "array",
            "description": "Servers that could not be listed",
            "items": {
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "error": { "type": "string" },
                },
                "required": ["server", "error"],
            },
        });
        match self {
            Self::ListResources => serde_json::json!({
                "type": "object",
                "properties": {
                    "server": optional_string,
                    "resources": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "server": { "type": "string" },
                                "uri": { "type": "string" },
                                "name": { "type": "string" },
                                "title": optional_string,
                                "description": optional_string,
                                "mimeType": optional_string,
                                "size": { "type": "number" },
                            },
                            "required": ["server", "uri", "name"],
                        },
                    },
                    "nextCursor": optional_string,
                    "errors": listing_errors,
                },
                "required": ["resources"],
            }),
            Self::ListResourceTemplates => serde_json::json!({
                "type": "object",
                "properties": {
                    "server": optional_string,
                    "resourceTemplates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "server": { "type": "string" },
                                "uriTemplate": {
                                    "type": "string",
                                    "description": "RFC 6570 URI template",
                                },
                                "name": { "type": "string" },
                                "title": optional_string,
                                "description": optional_string,
                                "mimeType": optional_string,
                            },
                            "required": ["server", "uriTemplate", "name"],
                        },
                    },
                    "nextCursor": optional_string,
                    "errors": listing_errors,
                },
                "required": ["resourceTemplates"],
            }),
            Self::ReadResource => serde_json::json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" },
                    "contents": {
                        "type": "array",
                        "items": {
                            "anyOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "uri": { "type": "string" },
                                        "mimeType": optional_string,
                                        "text": { "type": "string" },
                                    },
                                    "required": ["uri", "text"],
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "uri": { "type": "string" },
                                        "mimeType": optional_string,
                                        "blob": {
                                            "type": "string",
                                            "description": "base64",
                                        },
                                    },
                                    "required": ["uri", "blob"],
                                },
                            ],
                        },
                    },
                },
                "required": ["server", "uri", "contents"],
            }),
        }
    }
}

/// One aggregate resource tool (upstream `createMcpResourceToolDefinitions`).
/// `exposure` is decided at registration from the servers that have resources;
/// the servers themselves are resolved per call from the live connections.
pub struct McpResourceTool {
    manager: Arc<McpManager>,
    kind: McpResourceToolKind,
    exposure: ToolExposure,
}

impl McpResourceTool {
    pub fn new(
        manager: Arc<McpManager>,
        kind: McpResourceToolKind,
        exposure: ToolExposure,
    ) -> Self {
        Self {
            manager,
            kind,
            exposure,
        }
    }
}

impl AgentTool for McpResourceTool {
    fn name(&self) -> &'static str {
        self.kind.name()
    }

    fn description(&self) -> &'static str {
        self.kind.description()
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(self.kind.parameters())
    }

    fn output_schema(&self) -> Option<JsonValue> {
        Some(self.kind.output_schema())
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn annotations(&self) -> Option<crate::agent::tool_execution::ToolAnnotations> {
        // Upstream `annotations: { readOnlyHint: true }` for all three.
        Some(crate::agent::tool_execution::ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Like the per-server bridges: MCP calls execute sequentially.
        ToolExecutionMode::Sequential
    }

    fn run(&self, args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let manager = self.manager.clone();
        let kind = self.kind;
        let arguments = args.clone();
        let aborted = ctx.is_aborted();
        Box::pin(async move {
            if aborted {
                return ToolResult::error(
                    "Request was aborted before this tool call could execute",
                );
            }
            match kind {
                McpResourceToolKind::ListResources => {
                    list_mcp_resources(&manager, &arguments).await
                }
                McpResourceToolKind::ListResourceTemplates => {
                    list_mcp_resource_templates(&manager, &arguments).await
                }
                McpResourceToolKind::ReadResource => read_mcp_resource(&manager, &arguments).await,
            }
        })
    }
}

/// `stringArgument` (resources.ts:158-164): non-strings are an error, absent /
/// empty / whitespace-only values are `None`.
fn resource_string_argument(args: &JsonValue, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(text)) => {
            let trimmed = text.trim();
            Ok(if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            })
        }
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// The result shape of upstream `jsonResult` / the resource tools' read path:
/// the model-facing `content` plus the `{server, tool}` details and the
/// structured payload.
fn resource_tool_result(
    tool: &str,
    server: Option<&str>,
    content: String,
    payload: JsonValue,
) -> ToolResult {
    let mut result = ToolResult::ok(truncate_middle(&content));
    result.details = Some(serde_json::json!({
        "server": server.unwrap_or(""),
        "tool": tool,
    }));
    result.structured_content = Some(bounded_structured_payload(
        payload,
        MAX_STRUCTURED_RESULT_BYTES,
    ));
    result
}

/// Upstream `jsonResult`: the JSON payload is the model-facing text.
fn resource_json_result(tool: &str, server: Option<&str>, payload: JsonValue) -> ToolResult {
    let text = serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string());
    resource_tool_result(tool, server, text, payload)
}

/// MCP App user interfaces (`ui://` URIs and `profile=mcp-app` MIME types) are
/// left out of listings — only hosts that render them can use them (upstream
/// `isMcpAppResource`).
fn is_mcp_app_resource(uri: &str, mime_type: Option<&str>) -> bool {
    if uri.starts_with("ui://") {
        return true;
    }
    let Some(mime_type) = mime_type else {
        return false;
    };
    // `/;\s*profile\s*=\s*"?mcp-app"?/i`: the parameter must follow a `;`.
    mime_type.split(';').skip(1).any(|parameter| {
        let Some((key, value)) = parameter.split_once('=') else {
            return false;
        };
        key.trim().eq_ignore_ascii_case("profile")
            && value
                .trim()
                .trim_matches('"')
                .eq_ignore_ascii_case("mcp-app")
    })
}

/// One listed resource without `_meta`/icons, tagged with its server
/// (upstream `listed`).
fn listed_resource(server: &str, resource: &crate::mcp::protocol::McpResource) -> JsonValue {
    let mut value = serde_json::json!({
        "server": server,
        "uri": resource.uri,
        "name": resource.name,
    });
    if let Some(title) = &resource.title {
        value["title"] = JsonValue::String(title.clone());
    }
    if let Some(description) = &resource.description {
        value["description"] = JsonValue::String(description.clone());
    }
    if let Some(mime_type) = &resource.mime_type {
        value["mimeType"] = JsonValue::String(mime_type.clone());
    }
    if let Some(size) = resource.size {
        value["size"] = serde_json::json!(size);
    }
    value
}

/// Servers whose live connection offers resources, sorted by name: enabled,
/// connected, and reporting the MCP `resources` capability (upstream
/// `serversWithResources()`). Never connects a server.
async fn live_resource_servers(manager: &McpManager) -> Vec<String> {
    use crate::mcp::manager::ConnectionState;
    let enabled: Vec<String> = manager
        .registry()
        .list()
        .into_iter()
        .filter(|entry| entry.config.is_enabled())
        .map(|entry| entry.name)
        .collect();
    let states = manager.connection_states().await;
    let mut servers = Vec::new();
    for name in enabled {
        if !matches!(states.get(&name), Some(ConnectionState::Connected)) {
            continue;
        }
        if let Ok(client) = manager.get_client(&name).await {
            if client.has_resources() {
                servers.push(name);
            }
        }
    }
    servers.sort();
    servers
}

/// Upstream `findServer`: the named server must be one with resources.
async fn find_resource_server(manager: &McpManager, name: &str) -> Result<String, String> {
    let servers = live_resource_servers(manager).await;
    if servers.iter().any(|server| server == name) {
        return Ok(name.to_owned());
    }
    let available = servers.join(", ");
    Err(format!(
        "MCP server \"{name}\" has no resources{}",
        if available.is_empty() {
            String::new()
        } else {
            format!(". Servers with resources: {available}")
        }
    ))
}

/// One page of one server's resources (upstream `resourcesPage`), MCP App
/// resources filtered out.
async fn list_resource_page(
    manager: &McpManager,
    server: &str,
    cursor: Option<&str>,
) -> Result<(Vec<JsonValue>, Option<String>), crate::mcp::error::McpError> {
    let client = manager.get_client(server).await?;
    let page = client.list_resources(cursor).await?;
    let resources = page
        .resources
        .iter()
        .filter(|resource| !is_mcp_app_resource(&resource.uri, resource.mime_type.as_deref()))
        .map(|resource| listed_resource(server, resource))
        .collect();
    Ok((resources, page.next_cursor))
}

/// Every page of one server's resources (upstream `allResources`), bounded
/// by a page count, a repeated-cursor guard, and a total byte budget. A
/// listing that exceeds a bound fails with an explicit truncation error
/// (surfaced per server by the aggregate tool) instead of accumulating
/// without limit.
async fn all_resources(
    manager: &McpManager,
    server: &str,
) -> Result<Vec<JsonValue>, crate::mcp::error::McpError> {
    let mut resources = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut total_bytes = 0usize;
    for _ in 0..MAX_RESOURCE_LIST_PAGES {
        let (page, next) = list_resource_page(manager, server, cursor.as_deref()).await?;
        for item in page {
            total_bytes = total_bytes.saturating_add(serialized_len(&item).unwrap_or(0) + 1);
            if total_bytes > MAX_RESOURCE_LIST_BYTES {
                return Err(crate::mcp::error::McpError::Protocol(format!(
                    "resources/list for server \"{server}\" exceeded the {}-byte ({}) result cap; listing truncated after {} resources",
                    MAX_RESOURCE_LIST_BYTES,
                    frame_cap_label(MAX_RESOURCE_LIST_BYTES),
                    resources.len()
                )));
            }
            resources.push(item);
        }
        match next {
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err(crate::mcp::error::McpError::Protocol(format!(
                        "resources/list for server \"{server}\" returned duplicate cursor: {next}"
                    )));
                }
                cursor = Some(next);
            }
            None => return Ok(resources),
        }
    }
    Err(crate::mcp::error::McpError::Protocol(format!(
        "resources/list for server \"{server}\" exceeded {MAX_RESOURCE_LIST_PAGES} pages"
    )))
}

/// One listed resource template without `_meta`/icons, tagged with its server
/// (upstream `listedTemplate`).
fn listed_template(
    server: &str,
    template: &crate::mcp::protocol::McpResourceTemplate,
) -> JsonValue {
    let mut value = serde_json::json!({
        "server": server,
        "uriTemplate": template.uri_template,
        "name": template.name,
    });
    if let Some(title) = &template.title {
        value["title"] = JsonValue::String(title.clone());
    }
    if let Some(description) = &template.description {
        value["description"] = JsonValue::String(description.clone());
    }
    if let Some(mime_type) = &template.mime_type {
        value["mimeType"] = JsonValue::String(mime_type.clone());
    }
    value
}

/// One page of one server's resource templates (upstream
/// `resourceTemplatesPage`), MCP App templates filtered out. A server without
/// `resources/templates/list` yields an empty page (the client's
/// `withoutTemplates` tolerance).
async fn list_template_page(
    manager: &McpManager,
    server: &str,
    cursor: Option<&str>,
) -> Result<(Vec<JsonValue>, Option<String>), crate::mcp::error::McpError> {
    let client = manager.get_client(server).await?;
    let page = client.list_resource_templates(cursor).await?;
    let templates = page
        .resource_templates
        .iter()
        .filter(|template| {
            !is_mcp_app_resource(&template.uri_template, template.mime_type.as_deref())
        })
        .map(|template| listed_template(server, template))
        .collect();
    Ok((templates, page.next_cursor))
}

/// Every page of one server's resource templates (upstream
/// `allResourceTemplates`), with the same page/byte/repeated-cursor bounds as
/// [`all_resources`].
async fn all_resource_templates(
    manager: &McpManager,
    server: &str,
) -> Result<Vec<JsonValue>, crate::mcp::error::McpError> {
    let mut templates = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut total_bytes = 0usize;
    for _ in 0..MAX_RESOURCE_LIST_PAGES {
        let (page, next) = list_template_page(manager, server, cursor.as_deref()).await?;
        for item in page {
            total_bytes = total_bytes.saturating_add(serialized_len(&item).unwrap_or(0) + 1);
            if total_bytes > MAX_RESOURCE_LIST_BYTES {
                return Err(crate::mcp::error::McpError::Protocol(format!(
                    "resources/templates/list for server \"{server}\" exceeded the {}-byte ({}) result cap; listing truncated after {} templates",
                    MAX_RESOURCE_LIST_BYTES,
                    frame_cap_label(MAX_RESOURCE_LIST_BYTES),
                    templates.len()
                )));
            }
            templates.push(item);
        }
        match next {
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err(crate::mcp::error::McpError::Protocol(format!(
                        "resources/templates/list for server \"{server}\" returned duplicate cursor: {next}"
                    )));
                }
                cursor = Some(next);
            }
            None => return Ok(templates),
        }
    }
    Err(crate::mcp::error::McpError::Protocol(format!(
        "resources/templates/list for server \"{server}\" exceeded {MAX_RESOURCE_LIST_PAGES} pages"
    )))
}

/// `list_mcp_resources` (resources.ts:253-277).
async fn list_mcp_resources(manager: &McpManager, args: &JsonValue) -> ToolResult {
    let tool = crate::mcp::registry::LIST_MCP_RESOURCES_TOOL;
    let server = match resource_string_argument(args, "server") {
        Ok(server) => server,
        Err(error) => return ToolResult::error(error),
    };
    let cursor = match resource_string_argument(args, "cursor") {
        Ok(cursor) => cursor,
        Err(error) => return ToolResult::error(error),
    };
    if let Some(server) = server {
        let name = match find_resource_server(manager, &server).await {
            Ok(name) => name,
            Err(error) => return ToolResult::error(error),
        };
        let (resources, next) = match list_resource_page(manager, &name, cursor.as_deref()).await {
            Ok(page) => page,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let mut payload = serde_json::json!({ "server": name, "resources": resources });
        if let Some(next) = next {
            payload["nextCursor"] = JsonValue::String(next);
        }
        return resource_json_result(tool, Some(&name), payload);
    }
    if cursor.is_some() {
        return ToolResult::error("cursor can only be used when a server is specified");
    }
    let servers = live_resource_servers(manager).await;
    let mut resources = Vec::new();
    let mut errors = Vec::new();
    let mut total_bytes = 0usize;
    for server in servers {
        match all_resources(manager, &server).await {
            Ok(items) => {
                let items_bytes = items
                    .iter()
                    .map(|item| serialized_len(item).unwrap_or(0) + 1)
                    .sum::<usize>();
                if total_bytes.saturating_add(items_bytes) > MAX_RESOURCE_LIST_BYTES {
                    errors.push(serde_json::json!({
                        "server": server,
                        "error": format!(
                            "aggregate resource listing exceeded the {}-byte ({}) cap; this and remaining servers were not listed",
                            MAX_RESOURCE_LIST_BYTES,
                            frame_cap_label(MAX_RESOURCE_LIST_BYTES)
                        ),
                    }));
                    break;
                }
                total_bytes = total_bytes.saturating_add(items_bytes);
                resources.extend(items);
            }
            Err(error) => errors.push(serde_json::json!({
                "server": server,
                "error": error.to_string(),
            })),
        }
    }
    let mut payload = serde_json::json!({ "resources": resources });
    if !errors.is_empty() {
        payload["errors"] = JsonValue::Array(errors);
    }
    resource_json_result(tool, None, payload)
}

/// `list_mcp_resource_templates` (resources.ts:279-300). Mirrors
/// [`list_mcp_resources`] over `resources/templates/list`.
async fn list_mcp_resource_templates(manager: &McpManager, args: &JsonValue) -> ToolResult {
    let tool = crate::mcp::registry::LIST_MCP_RESOURCE_TEMPLATES_TOOL;
    let server = match resource_string_argument(args, "server") {
        Ok(server) => server,
        Err(error) => return ToolResult::error(error),
    };
    let cursor = match resource_string_argument(args, "cursor") {
        Ok(cursor) => cursor,
        Err(error) => return ToolResult::error(error),
    };
    if let Some(server) = server {
        let name = match find_resource_server(manager, &server).await {
            Ok(name) => name,
            Err(error) => return ToolResult::error(error),
        };
        let (templates, next) = match list_template_page(manager, &name, cursor.as_deref()).await {
            Ok(page) => page,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        let mut payload = serde_json::json!({ "server": name, "resourceTemplates": templates });
        if let Some(next) = next {
            payload["nextCursor"] = JsonValue::String(next);
        }
        return resource_json_result(tool, Some(&name), payload);
    }
    if cursor.is_some() {
        return ToolResult::error("cursor can only be used when a server is specified");
    }
    let servers = live_resource_servers(manager).await;
    let mut templates = Vec::new();
    let mut errors = Vec::new();
    let mut total_bytes = 0usize;
    for server in servers {
        match all_resource_templates(manager, &server).await {
            Ok(items) => {
                let items_bytes = items
                    .iter()
                    .map(|item| serialized_len(item).unwrap_or(0) + 1)
                    .sum::<usize>();
                if total_bytes.saturating_add(items_bytes) > MAX_RESOURCE_LIST_BYTES {
                    errors.push(serde_json::json!({
                        "server": server,
                        "error": format!(
                            "aggregate resource-template listing exceeded the {}-byte ({}) cap; this and remaining servers were not listed",
                            MAX_RESOURCE_LIST_BYTES,
                            frame_cap_label(MAX_RESOURCE_LIST_BYTES)
                        ),
                    }));
                    break;
                }
                total_bytes = total_bytes.saturating_add(items_bytes);
                templates.extend(items);
            }
            Err(error) => errors.push(serde_json::json!({
                "server": server,
                "error": error.to_string(),
            })),
        }
    }
    let mut payload = serde_json::json!({ "resourceTemplates": templates });
    if !errors.is_empty() {
        payload["errors"] = JsonValue::Array(errors);
    }
    resource_json_result(tool, None, payload)
}

/// `read_mcp_resource` (resources.ts:302-350).
async fn read_mcp_resource(manager: &McpManager, args: &JsonValue) -> ToolResult {
    let tool = crate::mcp::registry::READ_MCP_RESOURCE_TOOL;
    let server = match resource_string_argument(args, "server") {
        Ok(server) => server,
        Err(error) => return ToolResult::error(error),
    };
    let Some(server) = server else {
        return ToolResult::error("server must be provided");
    };
    let uri = match resource_string_argument(args, "uri") {
        Ok(uri) => uri,
        Err(error) => return ToolResult::error(error),
    };
    let Some(uri) = uri else {
        return ToolResult::error("uri must be provided");
    };
    let name = match find_resource_server(manager, &server).await {
        Ok(name) => name,
        Err(error) => return ToolResult::error(error),
    };
    let client = match manager.get_client(&name).await {
        Ok(client) => client,
        Err(error) => return ToolResult::error(error.to_string()),
    };
    let result = match client.read_resource(&uri).await {
        Ok(result) => result,
        Err(error) => return ToolResult::error(error.to_string()),
    };
    let mut text = String::new();
    let mut text_truncated = false;
    for contents in &result.contents {
        // Several contents (for example a directory) are labeled with their
        // URIs (upstream `readResource`).
        if result.contents.len() > 1 {
            let label = format!("{}:\n", contents.uri);
            if !append_bounded(&mut text, &label, MAX_STRUCTURED_RESULT_BYTES) {
                text_truncated = true;
                break;
            }
        }
        let block = crate::mcp::protocol::ContentBlock {
            kind: "resource".to_owned(),
            resource: Some(contents.clone()),
            ..Default::default()
        };
        if !append_bounded(
            &mut text,
            &block_to_text(&block),
            MAX_STRUCTURED_RESULT_BYTES,
        ) {
            text_truncated = true;
            break;
        }
        if !text.ends_with('\n') && !append_bounded(&mut text, "\n", MAX_STRUCTURED_RESULT_BYTES) {
            text_truncated = true;
            break;
        }
    }
    if text_truncated {
        text.push_str(&format!(
            "\n[... resource content truncated at the {}-byte ({}) cap ...]",
            MAX_STRUCTURED_RESULT_BYTES,
            frame_cap_label(MAX_STRUCTURED_RESULT_BYTES)
        ));
    }
    let text = if text.trim().is_empty() {
        format!("Resource {uri} is empty.")
    } else {
        text.trim_end().to_owned()
    };
    let payload = serde_json::json!({
        "server": name,
        "uri": uri,
        "contents": result.contents,
    });
    resource_tool_result(tool, Some(&name), text, payload)
}
