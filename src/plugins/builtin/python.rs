//! The `python` builtin plugin —an embedded RustPython 3 interpreter
//! (pir-only differentiation from upstream pi, where Python is reached by
//! spawning the user's native `python3` through the bash tool).
//!
//! ## What this gives over the upstream bash+python3 path
//!
//! - **Pure Rust, no system Python**: the interpreter (and the pure-Python
//!   half of the stdlib, frozen into the binary via `rustpython-pylib`'s
//!   `freeze-stdlib`) ships inside pir itself. Nothing is spawned, nothing is
//!   looked up on PATH, and the tool works offline.
//! - **Persistent session**: one RustPython VM per plugin instance, shared by
//!   every tool object the instance registers (a registry rebuild replaces
//!   the tool, not the session). Globals (functions, variables, imports)
//!   defined in one call are visible in every later call —the agent can
//!   iterate (define, then use) without re-sending setup code. The VM lives on
//!   a dedicated worker thread (RustPython's `VirtualMachine` is `!Send`); the
//!   thread exits with the session (plugin shutdown or a reset).
//! - **Bounded execution**: a host-side watchdog turns the deadline (default
//!   30s, `timeout_ms` up to 120s) into a KeyboardInterrupt-style interrupt
//!   delivered through RustPython's user-signal channel —the eval loop polls
//!   it (`vm.check_signals()` per bytecode), so runaway loops stop
//!   cooperatively. Captured output is capped at 1 MiB.
//!
//! The worker/interpreter machinery itself lives in
//! [`super::python_vm`] and is shared with the `context-mode` builtin (whose
//! executor runs each job in a fresh namespace over the same interpreter).
//!
//! ## Execution semantics
//!
//! `code` is compiled in `eval` mode first: a single expression's repr is
//! appended to the captured stdout as the tool's result (REPL-style echo).
//! Anything that is not a single expression compiles in `exec` mode and only
//! captured stdout is returned (the minimal-correct contract: print/stdout +
//! an explicit result, no implicit echo of intermediate expression
//! statements). Uncaught exceptions return `isError: true` with the Python
//! traceback (captured `sys.stderr`, which `print_exception` writes through);
//! partially printed stdout is kept on the error result.
//!
//! ## Capability boundary (stated in the tool description)
//!
//! The embedded stdlib is RustPython's: pure-Python modules (json, re,
//! asyncio, csv, ...) plus its Rust-implemented modules (math, io, os, ...).
//! CPython C extensions (numpy/pandas/...) are not available, and the
//! `threading`/`ssl` feature flags are deliberately disabled (no native
//! thread module, no TLS backends). RustPython 0.4 has no VM memory-limit
//! knob —the output cap and the deadline are the containment surface.
//!
//! ## Timeout / abort behavior (session reset)
//!
//! A deadline that fires while the call is still QUEUED behind another job
//! only abandons that call: the worker skips it, so the job actually running
//! (and the session's globals) survive. A deadline that fires on the job the
//! VM is running delivers the interrupt and resets the session — an interrupt
//! goes through the session's shared user-signal channel, so it must never be
//! sent on behalf of a call that does not own the eval loop.
//!
//! On deadline or abort the host drops the worker handle after delivering the
//! interrupt: if the interrupt lands (the common case —nearly every Python
//! loop hits a check), the worker finishes the interrupted job, finds the job
//! channel closed, and exits cleanly; the next call spins a fresh worker with
//! an empty namespace. If the script wedged inside non-cooperative native
//! code (e.g. an unbounded `re` backtrack), that one worker thread stays
//! parked (a bounded leak), but no future call can be starved behind it.
//! Either way the timeout error says the session was reset.

use std::sync::Arc;
use std::time::Duration;

use super::python_vm::{ExecOutcome, PythonJob, PythonSession};

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};

use crate::plugins::plugin::{AgentPlugin, PluginFuture, RegistrationContext};
use crate::plugins::source::PluginInfo;

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// The `python` tool name (also the `builtin:python` extension name).
pub const PYTHON_TOOL_NAME: &str = "python";

/// Default per-call deadline (30s; `timeout_ms` may raise it up to
/// [`MAX_TIMEOUT_MS`]).
pub use super::python_vm::{DEFAULT_TIMEOUT_MS, MAX_OUTPUT_CHARS, MAX_TIMEOUT_MS};

// ---------------------------------------------------------------------------
// The `python` AgentTool
// ---------------------------------------------------------------------------

/// The tool object registered into the shared registry (upsert by name, so
/// `--tools python` / `defaultTools` selection and plugin-host resolution
/// all see it like any other tool).
pub struct PythonTool {
    session: Arc<PythonSession>,
}

impl PythonTool {
    /// A tool backed by a fresh persistent interpreter session (no worker is
    /// spawned until the first call).
    pub fn new() -> Self {
        Self::with_session(Arc::new(PythonSession::new()))
    }

    /// A tool sharing an existing session — the plugin keeps one
    /// [`Arc<PythonSession>`] and hands a clone to every tool object a
    /// registry rebuild registers, so the user's globals/imports survive a
    /// changed tool surface (`/reload`, MCP discovery, JS registrations).
    pub fn with_session(session: Arc<PythonSession>) -> Self {
        Self { session }
    }
}

impl Default for PythonTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool for PythonTool {
    fn name(&self) -> &'static str {
        PYTHON_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Embedded RustPython 3 —pure-Python stdlib subset; C extensions (numpy/pandas) not available; state persists across calls in this session. \
Executes Python `code` in a persistent interpreter: variables, functions and imports defined in one call remain available in later calls. \
A single expression's repr is returned after captured output; print/stdout and stderr are captured, and uncaught exceptions return the Python traceback as an error. \
The default 30s deadline can be raised with timeout_ms (max 120000); a timed-out or aborted call resets the session (globals are lost). \
Imports resolve against the frozen stdlib and the working directory; no network, no native threads, no SSL, and no memory cap (output is capped at 1 MiB per call)."
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute: one expression (its repr is returned) or any number of statements. Globals persist across calls."
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "Execution deadline in milliseconds. Default 30000; values above 120000 are clamped."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        // The stdlib reaches the filesystem (open/os) like the shell tool;
        // informational only.
        true
    }

    fn run(&self, args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let session = Arc::clone(&self.session);
        // Own everything the future needs before pinning: the borrowed `args`
        // and `ctx` do not outlive the call.
        let code = match args.get("code").and_then(serde_json::Value::as_str) {
            Some(code) if !code.trim().is_empty() => Some(code.to_owned()),
            _ => None,
        };
        let timeout_ms = args.get("timeout_ms").and_then(serde_json::Value::as_u64);
        let cwd = ctx.cwd.to_string_lossy().into_owned();
        let signal = ctx.signal.clone();

        Box::pin(async move {
            let code = match code {
                Some(code) => code,
                None => return ToolResult::error(
                    "python: the `code` parameter is required and must be non-empty Python source",
                ),
            };

            let timeout = match timeout_ms {
                Some(ms) if ms > 0 => Duration::from_millis(ms.min(MAX_TIMEOUT_MS)),
                _ => Duration::from_millis(DEFAULT_TIMEOUT_MS),
            };

            // Deadline and abort race the same reply channel; either path
            // interrupts the VM and resets the session.
            let exec = session.exec(PythonJob::for_python_tool(code, Some(cwd)), timeout);
            let aborted = async {
                match &signal {
                    Some(signal) => signal.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                outcome = exec => exec_outcome_to_result(outcome),
                _ = aborted => {
                    // The job is already on the worker, so dropping the `exec`
                    // future alone would leave the script running; deliver the
                    // interrupt and reset the session before reporting.
                    session.abort_worker();
                    ToolResult::error(
                        "python: execution aborted; the persistent session was reset (variables defined this session were lost)",
                    )
                }
            }
        })
    }
}

/// Shape the worker's outcome into the model-facing tool result.
fn exec_outcome_to_result(outcome: ExecOutcome) -> ToolResult {
    let mut sections: Vec<String> = Vec::new();
    if !outcome.stdout.is_empty() {
        sections.push(outcome.stdout);
    }
    if let Some(repr) = outcome.eval_repr {
        sections.push(repr);
    }

    if outcome.error {
        // Traceback (plus any user-written stderr) dominates the result;
        // partial stdout is kept above it.
        let mut content = sections.join("\n");
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&outcome.stderr);
        return ToolResult {
            content,
            images: Vec::new(),
            details: Some(serde_json::json!({ "durationMs": outcome.duration_ms })),
            structured_content: None,
            is_error: true,
            terminate: false,
        };
    }

    // Non-exception stderr (warnings, explicit user writes) rides along
    // after stdout on success too.
    if !outcome.stderr.is_empty() {
        sections.push(outcome.stderr);
    }

    let content = if sections.is_empty() {
        // A clean run with no output and no expression value: say so rather
        // than returning an empty block.
        "(no output)".to_owned()
    } else {
        sections.join("\n")
    };
    ToolResult {
        content,
        images: Vec::new(),
        details: Some(serde_json::json!({ "durationMs": outcome.duration_ms })),
        structured_content: None,
        is_error: false,
        terminate: false,
    }
}

// ---------------------------------------------------------------------------
// The builtin plugin
// ---------------------------------------------------------------------------

/// The `builtin:python` plugin (not an upstream builtin —this is pir's
/// embedded-interpreter differentiation).
pub struct PythonPlugin {
    info: PluginInfo,
    /// `pythonTool` settings switch (default on): `false` skips tool
    /// registration entirely (embedders build the plugin via
    /// [`PythonPlugin::with_config`]; the builtin factory path runs with the
    /// default, following the codemode convention).
    enabled: bool,
    /// The persistent interpreter session, owned by the PLUGIN (not the tool
    /// object). Registry rebuilds re-run [`AgentPlugin::register`] and replace
    /// the tool object; the session — and therefore the VM worker and its
    /// namespace — must survive them.
    session: Arc<PythonSession>,
}

impl PythonPlugin {
    fn new(enabled: bool) -> Self {
        let mut info = PluginInfo::builtin(PYTHON_TOOL_NAME);
        info.replaceable = crate::plugins::builtin::builtin_extension_def(PYTHON_TOOL_NAME)
            .is_some_and(|def| def.replaceable);
        Self {
            info,
            enabled,
            session: Arc::new(PythonSession::new()),
        }
    }

    /// Build the plugin with the resolved `pythonTool` setting.
    pub fn with_config(enabled: bool) -> Self {
        Self::new(enabled)
    }

    /// Factory for the builtin registration list.
    pub fn new_plugin() -> Arc<dyn AgentPlugin> {
        Arc::new(Self::new(true))
    }

    /// Whether the shared worker thread/VM currently exists. Hidden test
    /// seam: `false` before the first call and after shutdown.
    pub fn has_python_worker(&self) -> bool {
        self.session.has_worker()
    }

    /// How many interpreter workers this plugin instance has spawned. Hidden
    /// test seam for the rebuild contract (a rebuild must not increment it).
    pub fn python_spawn_count(&self) -> u64 {
        self.session.spawn_count()
    }
}

impl AgentPlugin for PythonPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        if self.enabled {
            // New tool OBJECT per registration, same session: the registry
            // sees fresh definitions, the user keeps their namespace.
            ctx.register_tool(Arc::new(PythonTool::with_session(Arc::clone(
                &self.session,
            ))));
        }
        Ok(())
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            // Interrupt a running job and stop the worker now rather than
            // leaving it to the call deadline; a no-op if Python never ran.
            self.session.shutdown().await;
            Ok(())
        })
    }
}
