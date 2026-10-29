//! The `subagent` builtin plugin — the first Rust builtin, converted from
//! upstream's JS extension.
//!
//! Upstream tracking point:
//! `packages/coding-agent/examples/extensions/subagent/index.ts` (+ its
//! `agents.ts` discovery module, `README.md`, and the `agents/*.md` /
//! `prompts/*.md` fixtures). This builtin mirrors that extension's behavior
//! contract; drift reviews must re-diff this file against the upstream
//! example as it evolves.
//!
//! ## The JS-extension → Rust-builtin conversion path
//!
//! Upstream spawns a separate `pi --mode json -p --no-session` child process
//! per task for OS-level isolation. pir runs each task as an **in-process
//! ephemeral child [`crate::agent_session::AgentSession`]** (`session: None`,
//! in-memory transcript, no session persistence) — the performance win of the
//! conversion: no process spawn, no re-parsing of configuration, one provider
//! registry, and result extraction without JSON-line piping.
//!
//! Documented deviations from the child-process model:
//! - **Isolation**: upstream's OS process boundary becomes context isolation
//!   only — the child gets a fresh agent loop, fresh transcript, and a
//!   filtered tool registry, but shares the process (and the provider
//!   registry) with the parent. A malformed child cannot crash the parent
//!   (the loop is infallible), but there is no OS sandbox.
//! - **Child tool surface**: the child receives the parent's base (core)
//!   tool registry filtered by the agent's `tools` frontmatter allowlist,
//!   PLUS the parent's unified plugin registry: the parent's
//!   [`crate::extensions::ExtensionHandle`] is attached by the plugin host
//!   and passed straight into the child's
//!   [`crate::agent_session::AgentSessionConfig::extensions`], so the child
//!   merges the same JS-extension and Rust-builtin tools as the parent
//!   (upstream's child is a full `pi` with the complete tool surface — this
//!   matches it; a `tools` allowlist additionally becomes the child's
//!   [`crate::coding_agent::ToolSelection`] so plugin tools honor it too,
//!   mirroring upstream's `--tools` pass-through). The QuickJS actor is
//!   safely re-entrant here: every command is dispatched onto a spawned
//!   local task, the loop keeps draining the mailbox while a JS future is
//!   pending, and rquickjs releases the runtime lock at await points, so a
//!   child tool executing JS `ctx.executeTool` re-enters the runtime
//!   without deadlock.
//! - **Extension runtime isolation**: each child gets its OWN
//!   [`crate::extensions::ExtensionHandle`] — a fresh QuickJS actor loaded
//!   from the parent's recorded path plan but built with the child cwd's
//!   [`crate::extensions::ExtensionConfig`] — so the child's
//!   `session_start` dispatch, settings snapshot (re-read for the child cwd
//!   by the child session build) and event handlers land on its own
//!   runtime, matching upstream's per-child-process runtime. Before the
//!   fresh handle loads its extensions, the parent's serialized model
//!   catalog is pushed onto it ([`ModelCatalog`]): extension factories
//!   resolve models synchronously during load, and an empty catalog there
//!   fails the child with "No models available..." even though the parent
//!   has models configured. The handle is
//!   dropped when the child run ends: the actor fires
//!   `session_shutdown {reason: "quit"}` to the child's extensions and
//!   shuts down. Live child runtimes are globally capped
//!   ([`MAX_CHILD_RUNTIMES`]) so nested subagent dispatch cannot chain into
//!   unbounded actor growth; at the cap (or when the parent handle carries
//!   no recorded path plan, e.g. a directly started handle) the child falls
//!   back to sharing the parent's handle for that run, restoring the
//!   pre-isolation behavior instead of failing. The same-actor nested-call
//!   contract (`ctx.executeTool`) holds per child runtime: the child's
//!   extension tools wrap and re-enter the child's own handle.
//! - **Abort**: upstream SIGTERMs the child, then SIGKILLs after a 5s grace
//!   period. The in-process child aborts cooperatively: the parent abort
//!   signal is watched and the child session's abort signal is set; the child
//!   loop stops at its next cancellation checkpoint (turn boundary / provider
//!   request). No kill escalation is needed.
//! - **`stderr`**: children have no stderr stream; the `stderr` field of a
//!   result carries the child run error text (`AgentSession::prompt` error),
//!   keeping the upstream error-text precedence
//!   (`errorMessage` → stderr → final output → "(no output)").
//! - **Live updates**: per-task progress flows through the tool
//!   `onUpdate` seam ([`crate::agent::tool_execution::ToolUpdateFn`]) as
//!   `tool_execution_update` events instead of JSON-mode stdout lines.
//! - **Nesting depth**: an in-process child is itself a full agent loop, so
//!   a child's `subagent` call recursively enters another child. A per-session
//!   depth is carried in the child's own base tool registry (a hidden marker
//!   tool defined in `subagent/child.rs`) and enforced when a child is
//!   entered: entering depth [`MAX_SUBAGENT_DEPTH`] is refused with a normal
//!   failed result (`"Subagent nesting depth limit (3) reached"`), leaving
//!   the dispatcher running. The depth is session-local state, never a
//!   process-global counter, so parallel sibling children (which run as
//!   separate sessions/tasks) cannot inflate each other's level and a
//!   detached/background child carries its level with its session env; a
//!   refused or aborted run owns no counter slot to release.
//! - **Stack**: the recursive child run is heap-boxed at both recursion
//!   boundaries (`run_child` returns a `BoxFuture`, and it polls
//!   `Box::pin(child.prompt(..))`), so a nested chain does not grow the poll
//!   stack per level and runs on libtest's default test-thread stack.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value as JsonValue;

use crate::agent::agent_loop::{AgentEvent, EventHandler};
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{
    AgentTool, BoxFuture, ToolContext, ToolRegistry, ToolSessionEnv, ToolUpdateFn,
};
use crate::agent_session::{AgentSession, AgentSessionConfig, ModelCatalog};
use crate::ai::types::{AbortSignal, Message, Model, StopReason};
use crate::coding_agent::ToolSelection;
use crate::extensions::ExtensionHandle;

use super::plugin::{AgentPlugin, RegistrationContext};
use super::source::PluginInfo;

/// Upstream `MAX_PARALLEL_TASKS`.
const MAX_PARALLEL_TASKS: usize = 8;
/// Upstream `MAX_CONCURRENCY`.
const MAX_CONCURRENCY: usize = 4;
/// Upstream `PER_TASK_OUTPUT_CAP` (50 KB UTF-8 truncation of the
/// model-visible per-task output in parallel mode).
const PER_TASK_OUTPUT_CAP: usize = 50 * 1024;
/// Parent-abort watcher poll interval.
const ABORT_POLL_MS: u64 = 50;
/// Global cap on LIVE per-child extension runtimes, typed so the bound is a
/// named value with a constructor, not a bare magic integer at use sites.
/// Derived from the existing per-parent bounds: one parallel dispatch runs
/// at most `MAX_PARALLEL_TASKS` children, so the cap admits two full
/// parallel fan-outs (`MAX_PARALLEL_TASKS * MAX_CONCURRENCY`) plus
/// nested-dispatch headroom while still bounding the worst case. Beyond the
/// cap (and for a parent handle without a recorded path plan) the child
/// falls back to sharing the parent's handle — degraded isolation, never a
/// failure.
///
/// Each live runtime costs one actor OS thread + one QuickJS context and
/// re-runs extension discovery/transpile/evaluation (upstream pays the same
/// per child process, and more: a full process spawn).
#[derive(Debug, Clone, Copy)]
struct ChildRuntimeCap(usize);
const MAX_CHILD_RUNTIMES: ChildRuntimeCap = ChildRuntimeCap(MAX_PARALLEL_TASKS * MAX_CONCURRENCY);

/// The global live child-runtime cap, enforced as a tokio semaphore:
/// `try_acquire` (never waits — over cap means fall back, not block), and
/// the returned permit IS the slot guard (dropping it releases the slot —
/// no hand-written counter).
static CHILD_RUNTIME_SEM: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CHILD_RUNTIMES.0));

/// The `.pi` config directory name (upstream `CONFIG_DIR_NAME`).
const CONFIG_DIR_NAME: &str = ".pi";

// ---------------------------------------------------------------------------
// Agent discovery (upstream `agents.ts`)
// ---------------------------------------------------------------------------

mod child;
mod discovery;
mod dispatch;
mod results;
mod run;

pub use discovery::{
    discover_agents, find_nearest_project_agents_dir, load_agents_from_dir, parse_agent_markdown,
    parse_tool_list, split_frontmatter, AgentConfig, AgentFrontmatter, AgentScope, AgentSource,
    Discovery,
};
pub use results::{
    aggregate_parallel_text, apply_previous, get_final_output, is_failed, result_output,
    truncate_parallel_output, SingleResult, UsageTotals,
};
// Nesting-depth seam (builtin audit P1-6).
pub use child::{install_subagent_depth, subagent_depth, MAX_SUBAGENT_DEPTH};
// Model-catalog spawn seam (`tests/subagent_model_catalog.rs`): the fresh
// child runtime must receive the parent's catalog BEFORE its extensions load.
#[doc(hidden)]
pub use child::{spawn_child_runtime, ChildRuntime};
// Internal cross-submodule helpers (private to this module tree).
use child::run_child;
use discovery::{agent_list, agent_names_quoted};
use dispatch::{
    child_tool_registry, emit_partial, error_with_details, ok_with_details, resolve_model,
    EmitUpdate, OnTaskUpdate,
};
use results::DetailsBuilder;
use run::execute;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
struct TaskItem {
    agent: String,
    task: String,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SubagentParams {
    agent: Option<String>,
    task: Option<String>,
    tasks: Option<Vec<TaskItem>>,
    chain: Option<Vec<TaskItem>>,
    agent_scope: Option<String>,
    confirm_project_agents: Option<bool>,
    cwd: Option<String>,
}

fn task_item_json_schema(task_description: &str) -> JsonValue {
    // Upstream `TaskItem` / `ChainItem` (`index.ts:440-450`): the two differ
    // only in the `task` description; the item object itself carries no
    // `description` (that stays on the array schema).
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent": { "type": "string", "description": "Name of the agent to invoke" },
            "task": { "type": "string", "description": task_description },
            "cwd": { "type": "string", "description": "Working directory for the agent process" },
        },
        "required": ["agent", "task"],
    })
}

fn parameters_schema() -> JsonValue {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "description": "Name of the agent to invoke (for single mode)",
            },
            "task": {
                "type": "string",
                "description": "Task to delegate (for single mode)",
            },
            "tasks": {
                "type": "array",
                "description": "Array of {agent, task} for parallel execution",
                "items": task_item_json_schema("Task to delegate to the agent"),
            },
            "chain": {
                "type": "array",
                "description": "Array of {agent, task} for sequential execution",
                "items": task_item_json_schema(
                    "Task with optional {previous} placeholder for prior output",
                ),
            },
            "agentScope": {
                "type": "string",
                "enum": ["user", "project", "both"],
                "description": "Which agent directories to use. Default: \"user\". Use \"both\" to include project-local agents.",
                "default": "user",
            },
            "confirmProjectAgents": {
                "type": "boolean",
                "description": "Prompt before running project-local agents. Default: true.",
                "default": true,
            },
            "cwd": {
                "type": "string",
                "description": "Working directory for the agent process (single mode)",
            },
        },
    })
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// The `subagent` tool (upstream `pi.registerTool({ name: "subagent", ... })`).
pub struct SubagentTool {
    /// The parent session's extension runtime handle, attached by the plugin
    /// host (mirror of the `mcp` builtin's `attach_js` seam). Each child run
    /// spawns its OWN runtime from this handle's recorded plan (see
    /// [`spawn_child_runtime`]); the fresh handle goes into the child
    /// session's config so children own the extension tool surface
    /// (upstream's child is a full `pi`).
    extensions: Option<ExtensionHandle>,
    /// Tool description rendered from the RESOLVED user-agent directory (the
    /// same `agent_dir()/agents` resolution `run.rs` falls back to), not a
    /// hardcoded `~/.pi/agent/agents`: the agent dir honors
    /// `PIR_AGENT_DIR`/`PI_CODING_AGENT_DIR`. The per-run session override
    /// ([`ToolSessionEnv::user_agents_dir`]) still wins for discovery; this
    /// text names the default the tool itself resolves.
    description: String,
}

impl SubagentTool {
    /// Built by [`SubagentPlugin::register`] with the host-attached handle.
    pub fn with_extensions(extensions: Option<ExtensionHandle>) -> Self {
        let user_agents_dir = crate::coding_agent::context::agent_dir().join("agents");
        Self::with_extensions_and_dir(extensions, &user_agents_dir)
    }

    /// [`Self::with_extensions`] with an explicit resolved user-agent
    /// directory (the description is rendered from it).
    pub fn with_extensions_and_dir(
        extensions: Option<ExtensionHandle>,
        user_agents_dir: &std::path::Path,
    ) -> Self {
        Self {
            extensions,
            description: subagent_description(user_agents_dir),
        }
    }
}

/// Render the tool description against the resolved user-agent directory.
fn subagent_description(user_agents_dir: &std::path::Path) -> String {
    format!(
        "Delegate tasks to specialized subagents with isolated context. Modes: single (agent + task), \
         parallel (tasks array), chain (sequential with {{previous}} placeholder). Default agent scope \
         is \"user\" (from {}). To enable project-local agents in .pi/agents, set agentScope: \"both\" \
         (or \"project\").",
        user_agents_dir.display()
    )
}

impl AgentTool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(parameters_schema())
    }

    fn run(&self, args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let args = args.clone();
        let ctx = ctx.clone();
        let extensions = self.extensions.clone();
        Box::pin(async move { execute(&args, &ctx, extensions).await })
    }
}

/// The `subagent` tool as the parent session built it: without extensions
/// (tests, direct construction) the child runs with the base tool registry
/// only — the pre-`extensions` behavior.
impl Default for SubagentTool {
    fn default() -> Self {
        Self::with_extensions(None)
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// The `subagent` builtin plugin: registers the native Rust
/// [`SubagentTool`] on the unified plugin registry.
pub struct SubagentPlugin {
    info: PluginInfo,
    /// The JS runtime handle, attached by [`crate::plugins::PluginHost::build`]
    /// before registration (mirror of the `mcp` builtin's `attach_js` seam)
    /// so the registered [`SubagentTool`] hands it to every child session.
    js: RwLock<Option<ExtensionHandle>>,
    /// Explicit user-agent directory (the session's `agent_dir()/agents`,
    /// re-rooted by the SDK when the builder carries an explicit
    /// `agent_dir`). `None` keeps the tool's historical process-environment
    /// resolution ([`crate::coding_agent::context::agent_dir`]).
    user_agents_dir: Option<PathBuf>,
}

impl SubagentPlugin {
    fn new() -> Self {
        let mut info = PluginInfo::builtin("subagent");
        // Replaceable: a user JS extension registering a `subagent` tool
        // omits this builtin (upstream `replaceable` factory flag).
        info.replaceable =
            super::builtin_extension_def("subagent").is_some_and(|def| def.replaceable);
        Self {
            info,
            js: RwLock::new(None),
            user_agents_dir: None,
        }
    }

    /// The builtin rooted at an explicit user-agent directory for its TOOL
    /// DESCRIPTION (the text names `<dir>` as the default discovery root).
    /// Used by the SDK to keep `.agent_dir(..)` agents isolated (two agents
    /// in one process advertise their own `<agent_dir>/agents`).
    ///
    /// Per-run discovery is NOT bound by this value: `run.rs` reads the
    /// session env's [`ToolSessionEnv::user_agents_dir`] and only falls back
    /// to the process agent dir when the env carries none. The SDK/session
    /// paths populate that env from the same resolved directory, so the
    /// advertised text and the actual lookup agree.
    pub fn with_user_agents_dir(user_agents_dir: impl Into<PathBuf>) -> Self {
        let mut plugin = Self::new();
        plugin.user_agents_dir = Some(user_agents_dir.into());
        plugin
    }

    /// Attach the JS runtime handle (host-side, before the first registry
    /// build).
    pub fn attach_js(&self, js: Option<ExtensionHandle>) {
        *self
            .js
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = js;
    }
}

impl AgentPlugin for SubagentPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        let extensions = self
            .js
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let tool = match self.user_agents_dir.as_deref() {
            Some(dir) => SubagentTool::with_extensions_and_dir(extensions, dir),
            None => SubagentTool::with_extensions(extensions),
        };
        ctx.register_tool(Arc::new(tool));
        Ok(())
    }

    /// Downcast support: the plugin host reaches this concrete builtin to
    /// attach the JS runtime handle (the `mcp` builtin's seam).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Factory for [`super::builtin::BUILTIN_EXTENSION_FACTORIES`].
pub fn new_subagent_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(SubagentPlugin::new())
}
