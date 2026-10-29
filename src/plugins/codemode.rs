//! The `codemode` builtin plugin — a JS tool-composition sandbox (upstream
//! `packages/coding-agent/src/extensions/codemode` + `packages/codemode`).
//!
//! The `codemode` tool takes raw JavaScript and runs it in a **dedicated,
//! minimal QuickJS runtime** created per execution. The only capability the
//! script has is calling injected tools: there is no module loader, no file
//! system, no network, no timers, and no extension `pi` API — the sandbox
//! registers exactly one host function (the tool-call bridge) plus a console
//! shim. Tool calls from the script go through the agent loop's nested-call
//! executor ([`crate::agent::nested_tool_calls::NestedToolExecutor`], the same
//! path JS extensions use for `ctx.executeTool`), so they surface as nested
//! calls (`<parentToolCallId>/<n>` ids) and leave the bounded `nestedCalls`
//! record on the codemode tool result.
//!
//! ## Upstream contract and pir deviations
//!
//! Mirrored: the `{code}` parameter schema (plus an optional `tools` array
//! restricting the callable set), the `// @options:` first-line preamble
//! (`max_output_tokens` token budget + `timeout_ms` deadline; unknown or
//! invalid options fail the tool with the upstream `CodemodeSourceError`
//! text), raw JS run as an async function body
//! (top-level `await`/`return` work), the `tools.<name>({...args})` bindings,
//! `text()`/`console.*` output items, `exit()`, the "Script completed" /
//! "Script failed" result headers (with the upstream wall-time line), error
//! results keeping partial output (`Script error:\n{Name}: {message}`), and
//! a per-execution timeout that actually aborts a runaway JS loop (the
//! QuickJS interrupt-handler mechanics from `src/extensions/runtime`).
//!
//! Documented deviations from upstream:
//! - **Failure call summary**: upstream's failed-script text ends with
//!   "Tool calls made before the failure (they are not undone): ..." (or
//!   "No tool calls were made."). pir's nested-call record lives in the
//!   agent-loop session (attached to the tool result as `nestedCalls`) and
//!   is not visible to the tool itself, so the summary line is omitted;
//!   readers of the model-visible text see the call record in the tool
//!   result's structured `nestedCalls` field instead.
//! - **Exposure model**: upstream registers codemode `exposure: "model-only"`
//!   with `defaultActive: false` (`extensions/codemode/index.js:27`,
//!   `tool.js:253`): a direct model tool once explicitly activated, never
//!   callable from other tools, and absent from the default active set.
//!   Activation is `--tools codemode`, the `defaultTools` setting,
//!   `setActiveTools`, or the MCP discovery activation. pir mirrors that:
//!   [`CodemodeTool::exposure`] is [`ToolExposure::ModelOnly`] and
//!   [`CodemodeTool::default_active`] is `false`. The loadout declares every
//!   active non-hidden tool, so an activated codemode is declared; a call to
//!   an inactive codemode is refused ("Tool codemode not found"), the
//!   outcome upstream's declared-tools execution model produces.
//! - **`mode` setting**: upstream `codemode.mode: "only"` leaves out the
//!   declarations of active direct tools via the `prepareLoadout` hook
//!   (`hiddenDeclarations`); pir wires the same semantics through
//!   [`CodemodeTool::prepare_loadout`] (the tool-exposure loadout model). The
//!   mode-`"on"` description rewrite (upstream rewrites declared descriptions
//!   to show how scripts call them) remains a documented deviation — pir
//!   keeps the static description.
//! - **Settings lifecycle**: the builtin factory has no session cwd, so
//!   [`CodemodePlugin::new`] resolves the merged settings from the PROCESS
//!   working directory ONCE at plugin construction; [`AgentPlugin::on_reload`]
//!   re-reads them on `/reload`. [`CodemodeTool`] carries the resolved
//!   [`CodemodeConfig`], so a script execution performs no settings IO.
//!   Embedders that already resolved settings use
//!   [`CodemodePlugin::with_config`], whose explicit config `/reload` keeps.
//!   The extension sandbox route has no plugin instance and uses the
//!   process-wide [`cached_codemode_config`], initialized on first use and
//!   refreshed by the codemode plugin's reload.
//! - **Tool catalog**: upstream's description lists nested tool declarations
//!   under `codemode.inlineBudget` and scripts get `ALL_TOOLS` /
//!   `searchTools()`. pir's `ToolContext` does not expose the registry to
//!   tools, so the description stays static and the `tools` proxy resolves
//!   whatever tool names the registry carries at execution time (unknown
//!   names return an error result). The `inlineBudget` setting is applied to
//!   **script output** instead: model-visible output beyond
//!   `budget * 4` chars (upstream's 4 chars/token estimate) is truncated
//!   with a notice.
//! - **`models` namespace**: upstream scripts pass a resolved catalog model
//!   (`models.getModelOfType(type, provider, id)` → `models.generateImages(
//!   model, context)`), which reaches upstream's session model registry.
//!   pir's `ToolContext` has no model-registry seam, so pir's scripts pass
//!   the provider/id as request options instead — `models.generateImages(
//!   promptOrBlocks, { provider, model, apiKey? })` and `models.classify(
//!   { state, questions, images? }, { provider, model, apiKey? })`. The
//!   namespace resolves through two hidden codemode-exposure bridge tools
//!   ([`models_bridge`]) against the **embedded catalog** (image/classifier
//!   entries), auth comes from the explicit `apiKey` option or the provider's
//!   environment key, and results return as the full upstream JSON objects
//!   (`AssistantImages` / `ClassifierResult` — image blocks carry base64
//!   `data` + `mimeType`; pir has no `image()` output helper, so scripts
//!   serialize blocks themselves). `models.getModelsOfType` and friends are
//!   not ported (no registry seam).
//! - **`store()`/`load()`**: no persistent store seam reaches a tool
//!   execution; these globals are absent (calling them is a script error).
//! - **Worker process**: upstream runs the sandbox in a worker with a QuickJS
//!   wasm build; pir mirrors the isolation model with a worker **process**
//!   (`<current_exe> __codemode-worker`, the versioned JSON-lines protocol in
//!   [`worker`], tool calls bridged back to the parent). When the worker
//!   process is unavailable (spawn/handshake failure, or
//!   `PIR_CODEMODE_WORKER=off`) the run falls back to the in-process thread
//!   sandbox below: an in-process minimal runtime per call where a malformed
//!   script cannot crash the parent (JS errors are captured), and the runtime
//!   is torn down after every call.
//! - **Global live-worker admission**: at most
//!   [`worker::MAX_CONCURRENT_LIVE_WORKERS`] (upstream
//!   `MAX_CONCURRENT_CODEMODE_WORKERS = 2`) live worker-process executions run
//!   at once in the whole process. [`worker::run_in_worker_process`] takes a
//!   process-global [`tokio::sync::Semaphore`] slot right before spawning the
//!   child and holds the owned permit until the worker is reaped and its
//!   tool tasks settle, so success, error, abort, panic and future
//!   cancellation all release it. Upstream throws at the cap; pir queues the
//!   third worker (deliberate deviation) and the caller's abort signal
//!   cancels the wait. The in-process thread sandbox and its fallback
//!   decision are untouched: a run that cannot use the worker process keeps
//!   its pre-admission semantics (no slot, no queueing).
//! - **Extension sandbox route** (DRIFT §2.3 item 2 / D28, the
//!   `@earendil-works/pi-codemode` shim's engine-backed faces): the extension
//!   runtime's `CodemodeSandbox`/`CodemodeSandboxBase` route their
//!   `execute()`/`start()` into the *same* sandbox described above through
//!   [`start_extension_run`] and the session protocol in
//!   [`next_extension_event`] / [`deliver_extension_result`] /
//!   [`abort_extension_run`]. The host owns the VM, the deadline, the output
//!   and the call records; the JS side answers the script's calls (its own
//!   declared tools) and receives the upstream `CodemodeResult` object. Tool
//!   names the sandbox did not declare resolve against the host session's
//!   in-flight tool set (`NestedToolExecutor`) when one is bound — the
//!   builtin `codemode` tool's "the `tools` proxy resolves whatever the
//!   registry carries at execution time" model, reachable from an extension.
//!   The route is thread-sandbox only by construction: its tool calls must
//!   come back to the extension actor, so the worker-process hop (whose
//!   parent-side calls run in the *host* process) would add a hop without
//!   isolation. See [`ExtensionRunRequest`] for the per-face residuals.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use crate::agent::nested_tool_calls::NestedToolExecutor;
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{
    AgentTool, BoxFuture, ToolContext, ToolExposure, ToolLoadout, ToolLoadoutChanges,
    ToolNamespace, ToolReach,
};
use crate::ai::types::AbortSignal;
// Post-timeout / post-interrupt recovery: the shared
// `extensions::runtime::recover_interrupted` helper (clear the pending
// uncatchable interrupt exception and drain the pending-job queue), the same
// machinery `ExtensionRuntime::recover_after_interrupt` uses.
use crate::extensions::runtime::recover_interrupted as recover;

use super::plugin::{AgentPlugin, RegistrationContext};
use super::source::PluginInfo;

/// The worker-process isolation module: the versioned JSON-lines wire
/// protocol, the `pir __codemode-worker` child entry, and the parent-side
/// runner with thread-mode fallback (upstream
/// `packages/codemode/src/runtime/{host,worker,protocol}.ts`). Declared via
/// `#[path]` so this registered builtin keeps its single-file entry layout.
#[path = "codemode_worker.rs"]
pub mod worker;

/// The `models` namespace bridge: the hidden codemode-exposure tools behind
/// the sandbox's `models.generateImages()` / `models.classify()` (upstream
/// `execute.ts createModelGlobals`). Declared via `#[path]` like `worker`.
#[path = "codemode_models.rs"]
pub mod models_bridge;

/// Per-execution sandbox budget (upstream worker timeout; the extension
/// runtime's [`crate::extensions::DEFAULT_CALL_TIMEOUT`] value).
pub const SANDBOX_TIMEOUT: Duration = Duration::from_secs(30);

/// JS stack budget of one sandbox runtime.
///
/// 16 MiB is rquickjs's clamped maximum: a larger request silently turns the
/// stack check off (see [`crate::extensions::limits::MAX_STACK_BYTES`]), and
/// the 1 MiB engine default RangeErrors far too early for real recursive
/// scripts.
pub const SANDBOX_JS_STACK_BYTES: usize = 16 * 1024 * 1024;

/// OS thread stack for one sandbox thread.
///
/// QuickJS's JS stack budget and the host's native frames (QuickJS C frames,
/// the Rust dispatch machinery, the prelude/eval code) share the thread's OS
/// stack, so reserve twice the JS stack plus the native headroom — the same
/// derivation as [`crate::extensions::limits::ExtensionLimits::actor_stack_bytes`]
/// (2x + [`crate::extensions::limits::ACTOR_STACK_HEADROOM_BYTES`]). The
/// previous flat 64 MiB reservation is gone: at the default 16 MiB JS stack the
/// sandbox thread now reserves 40 MiB, and the OS stack is never smaller than
/// the JS budget, so the QuickJS recursion guard always trips first.
pub fn sandbox_thread_stack_bytes() -> usize {
    SANDBOX_JS_STACK_BYTES
        .saturating_mul(2)
        .saturating_add(crate::extensions::limits::ACTOR_STACK_HEADROOM_BYTES)
        .max(SANDBOX_JS_STACK_BYTES)
}

/// Number of sandbox threads alive right now.
///
/// Test seam (E-2/E11): a cancelled run must abort its JS job and release the
/// thread instead of leaving it to run out its deadline, so repeated
/// cancellations cannot stack OS stacks + QuickJS heaps.
#[doc(hidden)]
pub fn sandbox_threads_alive() -> usize {
    SANDBOX_THREADS_ALIVE.load(std::sync::atomic::Ordering::SeqCst)
}

static SANDBOX_THREADS_ALIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// RAII marker for [`sandbox_threads_alive`]; lives inside the sandbox
/// thread's closure.
struct SandboxThreadAlive;

impl SandboxThreadAlive {
    fn enter() -> Self {
        SANDBOX_THREADS_ALIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for SandboxThreadAlive {
    fn drop(&mut self) {
        SANDBOX_THREADS_ALIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The registered tool name (upstream `CODEMODE_TOOL_NAME`). The MCP side
/// looks the tool up by this name to decide whether to auto-activate it.
pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Script globals that list or look up tools and namespaces, which the script
/// may then call by any name (upstream `DISCOVERY_GLOBALS`, execute.ts:360).
const DISCOVERY_GLOBALS: [&str; 4] = [
    "searchTools",
    "describeNamespace",
    "describeTool",
    "ALL_TOOLS",
];

/// The tools a script may call (upstream `ToolDefinition.reachesTools` →
/// `getScriptReach`, execute.ts:362-369): every tool when the script uses a
/// discovery global, else the words of its source, so
/// `tools.mcp__docs__search` and `tools["mcp__docs__" + name]` both name the
/// `mcp__docs` namespace. Non-string input (a missing `code`) reaches nothing,
/// as upstream's `[]`.
pub fn script_reach(code: &JsonValue) -> ToolReach {
    let Some(code) = code.as_str() else {
        return ToolReach::Names(Vec::new());
    };
    if DISCOVERY_GLOBALS
        .iter()
        .any(|global| contains_word(code, global))
    {
        return ToolReach::All;
    }
    source_words(code)
}

/// Whether `text` contains `word` with word boundaries on both sides (the
/// upstream `\b` test: `\w` is `[A-Za-z0-9_]`, so `tools.searchTools` and
/// `searchTools(` match while `mySearchTools` does not).
fn contains_word(text: &str, word: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    let needle: Vec<char> = word.chars().collect();
    if characters.len() < needle.len() {
        return false;
    }
    for start in 0..=characters.len() - needle.len() {
        if characters[start..start + needle.len()] != needle[..] {
            continue;
        }
        let before_ok = start == 0 || !is_word_char(characters[start - 1]);
        let after_ok = start + needle.len() == characters.len()
            || !is_word_char(characters[start + needle.len()]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// JavaScript `\w` (`[A-Za-z0-9_]`).
fn is_word_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// The `\w+` words of a script source (upstream `code.match(/\w+/g) ?? []`).
fn source_words(code: &str) -> ToolReach {
    let mut words = Vec::new();
    let mut current = String::new();
    for character in code.chars() {
        if is_word_char(character) {
            current.push(character);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    ToolReach::Names(words)
}

/// Per-execution sandbox budget: `PIR_CODEMODE_TIMEOUT_MS` override
/// (embedder/test seam), else [`SANDBOX_TIMEOUT`].
fn sandbox_timeout() -> Duration {
    std::env::var("PIR_CODEMODE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(SANDBOX_TIMEOUT)
}

/// Default QuickJS heap cap for one script execution (bounded memory), in
/// MiB. Configurable per `codemode.sandboxMemoryMb` (settings) or the
/// `PIR_CODEMODE_MEMORY_MB` embedder/test override; `0` = no cap.
pub const DEFAULT_SANDBOX_MEMORY_MB: u64 = 128;

/// The default heap cap in bytes (the value both sandbox engines used to
/// hardcode).
pub fn default_sandbox_memory_bytes() -> usize {
    (DEFAULT_SANDBOX_MEMORY_MB as usize) * 1024 * 1024
}

/// The `PIR_CODEMODE_MEMORY_MB` override (embedder/test seam, `0` = no cap):
/// `Some` when the variable is set to a valid non-negative integer, `None`
/// when absent or invalid (an invalid value is ignored with a diagnostic).
fn sandbox_memory_env_override() -> Option<Option<usize>> {
    let raw = std::env::var("PIR_CODEMODE_MEMORY_MB").ok()?;
    match raw.trim().parse::<u64>() {
        Ok(0) => Some(None),
        Ok(mb) => Some(Some(
            usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX),
        )),
        Err(_) => {
            crate::ext_diag!(
                "pir-codemode: PIR_CODEMODE_MEMORY_MB=\"{raw}\" ignored: expected MiB as a non-negative integer"
            );
            None
        }
    }
}

/// Map the `codemode.sandboxMemoryMb` setting (MiB) to an engine cap:
/// unset = the 128 MiB default, `0` = no cap.
fn sandbox_memory_bytes_from_setting(mb: Option<u32>) -> Option<usize> {
    match mb {
        None => Some(default_sandbox_memory_bytes()),
        Some(0) => None,
        Some(mb) => {
            Some(usize::try_from(u64::from(mb).saturating_mul(1024 * 1024)).unwrap_or(usize::MAX))
        }
    }
}

/// Characters per estimated token (upstream `CHARS_PER_TOKEN`).
const CHARS_PER_TOKEN: usize = 4;

/// Default `codemode.inlineBudget` (upstream `SETTINGS_DEFAULTS`).
pub const DEFAULT_INLINE_BUDGET: u32 = 3000;

/// How much output one script may produce across BOTH kinds — `text()` and
/// `console.*` — before the script FAILS (upstream prelude-source.ts
/// `MAX_OUTPUT_CHARS = 16 * 1024 * 1024`). The host keeps all output until
/// the script ends, so without a limit a script that prints in a loop grows
/// the host's memory until it crashes.
///
/// The measure is the ONE the worker protocol frames: the JSON-escaped UTF-8
/// byte length the item contributes to its frame (the prelude counts
/// `JSON.stringify(item)`'s bytes minus the two quote bytes, mirroring
/// serde_json's escaping; the thread sandbox's host function receives the
/// decoded string, which the frame accounts as a superset). For plain ASCII
/// text this equals the plain byte count, and for escaped/multibyte text it
/// is an upper bound of the decoded size — so a payload admitted by this
/// budget always fits one worker frame ([`FRAME_ENVELOPE_MARGIN`]).
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;

/// The envelope margin a protocol frame reserves beyond the output budget:
/// the JSON keys, frame kind, tool name, quote bytes and the trailing
/// newline of one frame line. Together with [`MAX_OUTPUT_CHARS`] it forms
/// the single frame-size definition ([`frame_line_cap`]).
pub const FRAME_ENVELOPE_MARGIN: usize = 4096;

/// The single frame-size definition: a worker protocol frame line may be at
/// most this many bytes (serialized JSON, escaping included, trailing
/// newline included). It is derived from the effective output budget
/// (`output_caps` — including the `PIR_CODEMODE_MAX_OUTPUT_CHARS` override),
/// so "a payload the output accounting admitted" and "a frame that fits" are
/// the same statement:
///
/// - the prelude admits an item only while its escaped size stays within the
///   budget, so the item's frame is `budget + envelope <= cap`;
/// - a 16 MiB payload (the default budget, the documented contract) is
///   therefore always transmittable as one frame, in both directions, for
///   both the worker process and (vacuously — no frames) the thread sandbox.
///
/// Both sides of the protocol evaluate this function against the same
/// inherited environment, so parent and child agree on the cap (the worker
/// module's `MAX_FRAME_LINE_BYTES` const is its default/no-override value).
pub fn frame_line_cap() -> usize {
    output_caps().0.saturating_add(FRAME_ENVELOPE_MARGIN)
}

/// How many output items (text + console calls) one script may produce
/// before the script FAILS (upstream prelude-source.ts
/// `MAX_OUTPUT_ITEMS = 100_000`); covers loops that print empty strings.
pub const MAX_OUTPUT_ITEMS: usize = 100_000;

/// `PIR_CODEMODE_MAX_OUTPUT_CHARS` / `PIR_CODEMODE_MAX_OUTPUT_ITEMS`
/// overrides (embedder/test seam), else the upstream defaults. Computed once
/// per script execution; a non-positive or unparseable value falls back.
fn output_caps() -> (usize, usize) {
    let chars = std::env::var("PIR_CODEMODE_MAX_OUTPUT_CHARS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MAX_OUTPUT_CHARS);
    let items = std::env::var("PIR_CODEMODE_MAX_OUTPUT_ITEMS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MAX_OUTPUT_ITEMS);
    (chars, items)
}

/// The failure a script that crossed the output caps gets (upstream's
/// `done()` error path: past the limits the script fails, so catching the
/// throw does not resume output).
fn output_limit_error(max_chars: usize, max_items: usize) -> String {
    format!(
        "script output exceeded the limit of {max_chars} characters or {max_items} text(), image(), and console calls. Print a summary instead, or write large data to a file with a tool."
    )
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Codemode settings resolved from the merged settings snapshot (upstream
/// `getSettings().codemode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodemodeConfig {
    /// Upstream `codemode.mode`: `"only"` leaves out the declarations of
    /// active direct tools (upstream `prepareCodemodeLoadout`
    /// `hiddenDeclarations`), wired through [`CodemodeTool::prepare_loadout`]
    /// while codemode is active.
    pub mode: crate::config::settings::CodemodeMode,
    pub inline_budget: u32,
    /// QuickJS heap cap of one script execution, in bytes (`None` = no cap).
    /// From `codemode.sandboxMemoryMb` (default 128 MiB), overridable by
    /// `PIR_CODEMODE_MEMORY_MB`.
    pub sandbox_memory_bytes: Option<usize>,
}

impl Default for CodemodeConfig {
    fn default() -> Self {
        Self {
            mode: crate::config::settings::CodemodeMode::On,
            inline_budget: DEFAULT_INLINE_BUDGET,
            sandbox_memory_bytes: Some(default_sandbox_memory_bytes()),
        }
    }
}

/// Extract the codemode settings from a loaded settings snapshot (upstream
/// `readMode`/`readInlineBudget`). Pure — the settings-wiring seam (the
/// environment override is applied by [`load_codemode_config`]).
pub fn codemode_config_from(manager: &crate::config::settings::SettingsManager) -> CodemodeConfig {
    let codemode = manager.settings.codemode.clone().unwrap_or_default();
    CodemodeConfig {
        mode: codemode
            .mode
            .unwrap_or(crate::config::settings::CodemodeMode::On),
        inline_budget: codemode.inline_budget.unwrap_or(DEFAULT_INLINE_BUDGET),
        sandbox_memory_bytes: sandbox_memory_bytes_from_setting(codemode.sandbox_memory_mb),
    }
}

/// Read the `codemode` settings from disk (merged global + trusted project
/// scope), the same public [`crate::config::settings::SettingsManager`] the
/// hosts use. Disk IO is two bounded settings.json reads, matching how the
/// session facade pushes settings to the extension runtime.
///
/// Callers that execute more than once should read the config through the
/// plugin (`CodemodePlugin::new` resolves it once, `/reload` refreshes it) or
/// through [`cached_codemode_config`]; this raw loader is the uncached face.
pub fn load_codemode_config(cwd: &std::path::Path) -> CodemodeConfig {
    let trusted = std::env::var("PIR_TRUST_PROJECT").is_ok_and(|value| value == "1");
    let mut manager = crate::config::settings::SettingsManager::load(
        &crate::coding_agent::context::agent_dir(),
        &cwd.join(".pi"),
        cwd,
        trusted,
    );
    for error in manager.drain_errors() {
        crate::ext_diag!(
            "[codemode] settings {}: {}",
            error.scope_name(),
            error.message
        );
    }
    let mut config = codemode_config_from(&manager);
    if let Some(override_bytes) = sandbox_memory_env_override() {
        config.sandbox_memory_bytes = override_bytes;
    }
    config
}

/// Number of settings loads the codemode caching paths performed
/// (plugin construction/reload plus the extension-route cache). Test seam:
/// proving a session's script executions do no settings IO is a count
/// comparison, not a timing assertion.
pub fn settings_load_count() -> u64 {
    SETTINGS_LOAD_COUNT.load(std::sync::atomic::Ordering::SeqCst)
}

static SETTINGS_LOAD_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`load_codemode_config`] with the test-visible load counter bumped. Every
/// caching path funnels through here so the count is exactly the number of
/// disk reads product code performed.
fn counted_load_codemode_config(cwd: &std::path::Path) -> CodemodeConfig {
    SETTINGS_LOAD_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    load_codemode_config(cwd)
}

/// The process working directory the builtin factory resolves settings
/// against (it has no session cwd at construction; the CLI's runtime assembly
/// runs in the session's cwd).
fn process_cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Process-wide config cache for the extension sandbox route
/// ([`start_extension_run`]): that route has no [`CodemodePlugin`] instance to
/// carry a config, and it must not re-read settings once per run.
fn extension_config_cache() -> &'static StdMutex<Option<CodemodeConfig>> {
    static CONFIG: std::sync::OnceLock<StdMutex<Option<CodemodeConfig>>> =
        std::sync::OnceLock::new();
    CONFIG.get_or_init(|| StdMutex::new(None))
}

/// The extension sandbox route's codemode config, resolved from the process
/// working directory on first use and cached thereafter (env override
/// included). [`reload_cached_codemode_config`] replaces it, which the
/// codemode plugin's `/reload` hook does.
pub fn cached_codemode_config() -> CodemodeConfig {
    let mut cached = extension_config_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(config) = *cached {
        return config;
    }
    let config = counted_load_codemode_config(&process_cwd());
    *cached = Some(config);
    config
}

/// Replace the process-wide extension-route config with a fresh settings read
/// (the `/reload` invalidation). Returns the new config.
pub fn reload_cached_codemode_config() -> CodemodeConfig {
    let config = counted_load_codemode_config(&process_cwd());
    store_cached_codemode_config(config);
    config
}

/// Publish an already-resolved config into the process-wide cache (the
/// plugin's reload reuses its single read instead of loading twice).
fn store_cached_codemode_config(config: CodemodeConfig) {
    *extension_config_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(config);
}

// ---------------------------------------------------------------------------
// Output accounting (upstream's output items, Rust side)
// ---------------------------------------------------------------------------

/// One script output item (upstream output protocol, text + console kinds).
#[derive(Debug, Clone)]
/// One buffered output item. Public for tests/ (the output-rendering
/// property test renders items directly, like upstream's codemode suite).
pub struct OutputItem {
    /// `text` or `console` (upstream `CodemodeOutputItem.type`).
    pub kind: &'static str,
    /// The item's text as the script produced it.
    pub text: String,
}

/// Shared output buffer: `text()`/`console.*` feed it through the host fn.
/// Total chars + items are tracked across BOTH kinds and capped (upstream
/// `MAX_OUTPUT_CHARS` / `MAX_OUTPUT_ITEMS`): past a cap further items are
/// refused (bounded memory) and the script is failed with the cap error
/// (upstream's `done()` error path). The prelude counts the items' ESCAPED
/// frame size (see [`MAX_OUTPUT_CHARS`]); this buffer counts the decoded
/// bytes it receives, which is a lower bound of that total — so for a
/// well-behaved prelude this defense layer can never trip before the
/// prelude's accounting, and it bounds the host memory even when a tampered
/// prelude bypasses the accounting.
struct OutputBuffer {
    items: Vec<OutputItem>,
    max_chars: usize,
    max_items: usize,
    total_chars: usize,
    total_items: usize,
    over_limit: bool,
}

impl OutputBuffer {
    fn new(max_chars: usize, max_items: usize) -> Self {
        Self {
            items: Vec::new(),
            max_chars,
            max_items,
            total_chars: 0,
            total_items: 0,
            over_limit: false,
        }
    }

    fn push(&mut self, kind: &'static str, text: String) {
        self.total_items = self.total_items.saturating_add(1);
        self.total_chars = self.total_chars.saturating_add(text.len());
        if self.total_chars > self.max_chars || self.total_items > self.max_items {
            // Refuse the item so the buffer stays bounded; the sandbox outcome
            // is failed with the cap error.
            self.over_limit = true;
            return;
        }
        self.items.push(OutputItem { kind, text });
    }

    /// Drain the buffered items plus whether a cap was exceeded.
    fn take(&mut self) -> (Vec<OutputItem>, bool) {
        (
            std::mem::take(&mut self.items),
            std::mem::take(&mut self.over_limit),
        )
    }
}

/// Render the buffered items into the model-visible output (upstream's
/// `==> text N/M <==` blocks and one `<console_output>` block).
///
/// The join is linear (upstream #10678 — `f11e50f72`): every part is appended
/// in place into one pre-sized buffer (no per-item re-concatenation of a
/// growing string, and no per-item temporary allocation), so a script with
/// tens of thousands of `text()` calls renders in one pass. Public for tests/.
pub fn render_output(items: &[OutputItem]) -> String {
    use std::fmt::Write as _;

    let text_items: Vec<&OutputItem> = items.iter().filter(|item| item.kind == "text").collect();
    let console_items: Vec<&OutputItem> =
        items.iter().filter(|item| item.kind == "console").collect();
    let total = text_items.len();
    let numbered = total > 1;
    // One allocation for the whole rendered output: the payload plus the
    // per-item block header / newline overhead.
    let mut out =
        String::with_capacity(items.iter().map(|item| item.text.len() + 32).sum::<usize>() + 32);
    for (index, item) in text_items.iter().enumerate() {
        if numbered {
            let _ = writeln!(out, "==> text {}/{} <==", index + 1, total);
        }
        out.push_str(&item.text);
        out.push('\n');
    }
    if !console_items.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("<console_output>\n");
        for item in console_items {
            out.push_str(&item.text);
            out.push('\n');
        }
        out.push_str("</console_output>");
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Apply the token budget with the upstream `truncateOutput` layout: the
/// output becomes one item keeping the START and END of the text (head
/// budget/2 + tail budget/2), so a trailing return value or console block
/// survives truncation. Upstream spills the full text to a temp file; pir
/// documents the spill as out of scope, and the upstream notice text is kept
/// verbatim.
fn truncate_to_budget(output: &str, budget_tokens: u32) -> String {
    let max_chars = (budget_tokens as usize)
        .saturating_mul(CHARS_PER_TOKEN)
        .max(1);
    if output.len() <= max_chars {
        return output.to_owned();
    }
    let head_chars = max_chars / 2;
    let tail_chars = max_chars - head_chars;
    let mut head_end = head_chars;
    while head_end > 0 && !output.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = output.len().saturating_sub(tail_chars);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start < head_end {
        tail_start = head_end;
    }
    let removed = output.len() - head_end - (output.len() - tail_start);
    format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}…{} tokens truncated…{}",
        output.len().div_ceil(CHARS_PER_TOKEN),
        output.lines().count(),
        &output[..head_end],
        removed.div_ceil(CHARS_PER_TOKEN),
        &output[tail_start..]
    )
}

// ---------------------------------------------------------------------------
// Source format: optional `// @options:` preamble (upstream
// `packages/codemode/src/source.ts`)
// ---------------------------------------------------------------------------

/// The options prefix of a script's optional first line (upstream
/// `CODEMODE_OPTIONS_PREFIX`).
pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

/// Fields a `// @options:` object may carry (upstream `SUPPORTED_FIELDS`).
const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";

/// Largest delay `setTimeout` supports, which bounds `timeout_ms` (upstream
/// `MAX_TIMEOUT_MS`).
const MAX_OPTIONS_TIMEOUT_MS: u64 = 2_147_483_647;

/// Largest JSON safe integer (upstream `Number.isSafeInteger`), which bounds
/// `max_output_tokens` and `timeout_ms`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Parsed `// @options:` values (upstream `CodemodeSourceOptions`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CodemodeSourceOptions {
    /// Token budget for the script's output.
    pub max_output_tokens: Option<u64>,
    /// Hard deadline for the whole script in milliseconds, including tool
    /// calls.
    pub timeout_ms: Option<u64>,
}

fn options_object_error() -> String {
    format!("@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}")
}

/// Validate the directive JSON after `// @options:` (upstream `parseOptions`).
fn parse_options(directive: &str) -> Result<CodemodeSourceOptions, String> {
    if directive.is_empty() {
        return Err(options_object_error());
    }
    let value: JsonValue = match serde_json::from_str(directive) {
        Ok(value) => value,
        Err(error) => {
            return Err(format!(
                "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {error}"
            ))
        }
    };
    let Some(fields) = value.as_object() else {
        return Err(options_object_error());
    };
    for key in fields.keys() {
        if key != "max_output_tokens" && key != "timeout_ms" {
            return Err(format!(
                "@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"
            ));
        }
    }
    let mut options = CodemodeSourceOptions::default();
    if let Some(max_output_tokens) = fields.get("max_output_tokens") {
        let Some(max_output_tokens) = max_output_tokens
            .as_u64()
            .filter(|value| *value <= MAX_SAFE_INTEGER)
        else {
            return Err(
                "@options field `max_output_tokens` must be a non-negative safe integer".to_owned(),
            );
        };
        options.max_output_tokens = Some(max_output_tokens);
    }
    if let Some(timeout_ms) = fields.get("timeout_ms") {
        let Some(timeout_ms) = timeout_ms
            .as_u64()
            .filter(|value| *value > 0 && *value <= MAX_OPTIONS_TIMEOUT_MS)
        else {
            return Err(format!(
                "@options field `timeout_ms` must be a positive integer up to {MAX_OPTIONS_TIMEOUT_MS}"
            ));
        };
        options.timeout_ms = Some(timeout_ms);
    }
    Ok(options)
}

/// Split an optional first-line `// @options: {...}` from the script
/// (upstream `parseCodemodeSource`). Returns the code with the options line
/// replaced by an empty line (so line numbers are unchanged) plus the parsed
/// options; `Err` carries the upstream `CodemodeSourceError` text for empty
/// input, invalid options, and an options line that is not the first line.
pub fn parse_codemode_source(input: &str) -> Result<(String, CodemodeSourceOptions), String> {
    if input.trim().is_empty() {
        return Err(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`."
                .to_owned(),
        );
    }
    // An options line on any line but the first was ignored silently, so its
    // `timeout_ms` never applied and the script ran without a deadline
    // (upstream #10631): reject it before the script runs.
    for (index, line) in input.split('\n').enumerate().skip(1) {
        if line.trim_start().starts_with(CODEMODE_OPTIONS_PREFIX) {
            return Err(format!(
                "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
                index + 1
            ));
        }
    }
    let newline = input.find('\n');
    let first_line = match newline {
        Some(index) => &input[..index],
        None => input,
    };
    let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
    let trimmed = first_line.trim_start();
    if !trimmed.starts_with(CODEMODE_OPTIONS_PREFIX) {
        return Ok((input.to_owned(), CodemodeSourceOptions::default()));
    }
    let code = match newline {
        Some(index) => &input[index..],
        None => "",
    };
    if code.trim().is_empty() {
        return Err(
            "The @options line must be followed by JavaScript source on subsequent lines"
                .to_owned(),
        );
    }
    let directive = trimmed[CODEMODE_OPTIONS_PREFIX.len()..].trim();
    let options = parse_options(directive)?;
    Ok((code.to_owned(), options))
}

// ---------------------------------------------------------------------------
// The sandbox: one minimal QuickJS runtime per execution
// ---------------------------------------------------------------------------

/// The identifier a script uses for a tool (upstream `identifier.ts`
/// `toCodemodeIdentifier`): characters that are not valid in an identifier
/// become `_`, and an empty result is `_`.
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character.is_ascii_alphabetic() || character == '_' || character == '$'
        } else {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        };
        identifier.push(if valid { character } else { '_' });
    }
    if identifier.is_empty() {
        identifier.push('_');
    }
    identifier
}

/// Whether `query` names `namespace`: its name or one of its `aliases`, as
/// written or as the same script identifier (upstream `isNamespaceName`,
/// execute.ts:559-568 — `dev-radius` and `dev_radius` are both `dev_radius`).
/// This is the lookup `describeNamespace()` and the `namespace` option of
/// `searchTools()` use, so an MCP namespace registered with
/// `aliases: [server]` is found by the server name.
///
/// pir note: the Rust codemode sandbox has no discovery globals (the
/// documented deviation in this module's header), so nothing in Rust calls
/// this yet; the JS-side codemode/durable discovery must apply the same rule
/// to the namespace's `aliases` (see the `UPDATING.md` ledger, D23).
pub fn namespace_matches(namespace: &ToolNamespace, query: &str) -> bool {
    let query_identifier = to_codemode_identifier(query);
    std::iter::once(&namespace.name)
        .chain(namespace.aliases.iter())
        .any(|name| name == query || to_codemode_identifier(name) == query_identifier)
}

/// The hidden `models` bridge tool names: they cross the same host bridge as
/// `tools.<name>()` but are NOT part of the callable-tools set (upstream: the
/// models namespace is independent of the `tools` allowlist), so the
/// allowlist never rejects them. Public for the whitelist tests
/// (`tests/codemode_whitelist.rs`).
pub fn is_models_bridge_name(name: &str) -> bool {
    name == "models.generateImages" || name == "models.classify"
}

/// Whether the tool-call allowlist rejects a bridge call to `name`:
/// `None` = the run is unconstrained (every registered tool except
/// `codemode` itself, which the bridge hard-rejects separately); a
/// constrained run refuses every name outside the list. The upstream
/// equivalent never hands the sandbox a tool outside the list (host.ts
/// `start()` passes the already-filtered tool table to the worker); pir
/// keeps the full table and enforces the list at BOTH enforcement points —
/// the thread sandbox's `__pir_callTool` bridge and the worker-process
/// parent's `call`-frame handler — so the prelude's `tools` Proxy is never
/// the only gate. Public for the whitelist tests.
pub fn allowlist_rejects(allowed: Option<&[String]>, name: &str) -> bool {
    if is_models_bridge_name(name) {
        return false;
    }
    matches!(allowed, Some(list) if !list.iter().any(|tool| tool == name))
}

/// The bridge error text for an allowlist-rejected call. The reply carries
/// the standard "not ok" tool-failure shape, which the prelude turns into a
/// rejected `tools.<name>()` promise — the same observable effect as the
/// Proxy denial, where a tool outside the list is simply not exposed.
pub fn allowlist_error_text(name: &str) -> String {
    format!("tool '{name}' is not in the allowed tools for this script")
}

/// The full bridge reply for an allowlist-rejected call (the prelude turns it
/// into a rejected `tools.<name>()` promise). Public for tests/
/// (`tests/codemode_whitelist.rs` locks the bridge reply shape).
pub fn allowlist_error_json(name: &str) -> String {
    ToolCallReply::failed(allowlist_error_text(name)).to_bridge_json()
}

// ---------------------------------------------------------------------------
// MCP result schemas (DRIFT D25 / upstream mcp `tools.ts`)
//
// A tool that reaches other tools describes what a call resolves to through
// its plain `outputSchema`; pi-codemode owns no MCP shape any more
// (`24382bedd` dropped `MCP_TYPESCRIPT_PREAMBLE`/`mcpStructuredContentSchema`,
// `c661bc574` the durable copy). The MCP bridge therefore declares the
// `CallToolResult` scripts receive *itself*, as an output schema, and codemode
// renders it like any other schema (upstream `createMcpResultSchema`).
// ---------------------------------------------------------------------------

/// MCP content blocks as one compact type with the fields scripts read most
/// (upstream `CONTENT_BLOCK_SCHEMA`): `type` is required, the text/image
/// payload fields are optional, and additional server fields ride the open
/// index.
fn mcp_content_block_schema() -> JsonValue {
    serde_json::json!({
        "type": "object",
        "properties": {
            "type": {
                "enum": ["text", "image", "audio", "resource_link", "resource"],
            },
            "text": { "type": "string" },
            "data": { "type": "string" },
            "mimeType": { "type": "string" },
        },
        "required": ["type"],
        "additionalProperties": true,
    })
}

/// Keywords whose values are data, so a `$ref` key inside them is not a
/// reference (upstream `SCHEMA_DATA_KEYWORDS`).
fn is_schema_data_keyword(key: &str) -> bool {
    matches!(key, "const" | "enum" | "default" | "examples")
}

/// Keywords whose values map names to schemas, so their keys are names, not
/// keywords (upstream `SCHEMA_MAP_KEYWORDS`).
fn is_schema_map_keyword(key: &str) -> bool {
    matches!(
        key,
        "properties"
            | "patternProperties"
            | "dependentSchemas"
            | "dependencies"
            | "$defs"
            | "definitions"
    )
}

/// `schema` as a subschema at the JSON pointer `pointer`
/// (`#/properties/structuredContent`) of another schema: its local references
/// (`#`, `#/$defs/Item`) are rewritten to point below `pointer`, so they
/// resolve to the same subschemas as before (upstream `relocateSchema`).
pub fn relocate_schema(schema: &JsonValue, pointer: &str) -> JsonValue {
    match schema {
        JsonValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .map(|item| relocate_schema(item, pointer))
                .collect(),
        ),
        JsonValue::Object(map) => {
            let mut relocated = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if key == "$ref" {
                    // A local reference (upstream: `value === "#" ||
                    // value.startsWith("#/")`); a remote reference stays as
                    // sent and renders `unknown`.
                    if let Some(reference) = value.as_str() {
                        if reference == "#" || reference.starts_with("#/") {
                            relocated.insert(
                                key.clone(),
                                JsonValue::String(format!("{pointer}{}", &reference[1..])),
                            );
                            continue;
                        }
                    }
                    // Anything else (remote string, or a non-string value —
                    // upstream's object check gates only the rewrite) falls
                    // through to the recursive branch, so a nested local
                    // `$ref` inside a malformed `$ref` is relocated too.
                    relocated.insert(key.clone(), relocate_schema(value, pointer));
                } else if is_schema_data_keyword(key) {
                    // Data that looks like a reference is kept as is.
                    relocated.insert(key.clone(), value.clone());
                } else if is_schema_map_keyword(key) {
                    // A name→schema map: every entry's schema is relocated.
                    // A non-object value is not a map, but upstream still
                    // walks it (the object check gates only the entry loop;
                    // the value falls through to the recursive branch), so
                    // the key is kept and recursed into rather than dropped.
                    if let JsonValue::Object(entries) = value {
                        let mut renamed = serde_json::Map::with_capacity(entries.len());
                        for (name, subschema) in entries {
                            renamed.insert(name.clone(), relocate_schema(subschema, pointer));
                        }
                        relocated.insert(key.clone(), JsonValue::Object(renamed));
                    } else {
                        relocated.insert(key.clone(), relocate_schema(value, pointer));
                    }
                } else {
                    relocated.insert(key.clone(), relocate_schema(value, pointer));
                }
            }
            JsonValue::Object(relocated)
        }
        other => other.clone(),
    }
}

/// Output schema of an MCP tool: the `CallToolResult` codemode scripts receive
/// (upstream `createMcpResultSchema`; `_meta` is dropped by
/// [`crate::plugins::builtin::convert_mcp_result`]), with the tool's own output
/// schema as `structuredContent`.
///
/// `structured_content_schema` is the server tool's `outputSchema`. Without
/// one, `structuredContent` declares the empty schema: servers may send
/// structured content without declaring a schema for it, and it may be any
/// JSON value (upstream `67996ab75` — the D6 leftover), which renders as
/// `unknown`.
///
/// Public for the MCP declaration tests; the MCP bridge is the caller
/// (`src/plugins/builtin/mcp.rs::McpToolBridge::output_schema`).
pub fn mcp_result_schema(structured_content_schema: Option<&JsonValue>) -> JsonValue {
    let structured_content = structured_content_schema
        .filter(|schema| !schema.is_null())
        .map(|schema| relocate_schema(schema, "#/properties/structuredContent"))
        .unwrap_or_else(|| serde_json::json!({}));
    serde_json::json!({
        "type": "object",
        "properties": {
            "content": {
                "type": "array",
                "items": mcp_content_block_schema(),
            },
            "structuredContent": structured_content,
            "isError": { "type": "boolean" },
        },
        "required": ["content"],
        // Fields newer protocol versions add.
        "additionalProperties": true,
    })
}

/// Sandbox JS prelude: output helpers, console shim, `exit()`, and the
/// `tools` proxy backed by the single host function. No loader, no modules,
/// no other host capability.
///
/// The whole prelude is one IIFE (upstream keeps the host bridge inside the
/// prelude's closure, prelude-source.ts header): the raw host capabilities —
/// the tool bridge `__pir_callTool`, the output sink `__pir_hostOutput`, the
/// `models`-namespace helper `__pir_modelCall`, and the allowlist
/// `__pir_allowedTools` — are captured into closure locals and their
/// `globalThis` handles are cleared before the script runs, so the script can
/// only call tools through `tools.<name>()` / the `models` namespace and the
/// allowlist is a real boundary, not a mutable global the script could widen.
const SANDBOX_PRELUDE: &str = r#"
(function () {
var bridge = globalThis.__pir_callTool;
var bridgeGlobal = globalThis.__pir_callGlobal;
var hostOut = globalThis.__pir_hostOutput;
var allowedTools = globalThis.__pir_allowedTools;
// Extension-sandbox route faces (upstream prelude): unset for the builtin
// `codemode` tool, whose reduced prelude is a documented deviation. When the
// host sets them the declared tool table, the globals, the script-visible
// store, the deterministic clock/randomness and the per-result acknowledge
// below are installed; captured (and cleared) exactly like the raw bridge so
// the script cannot reach them.
var declaredToolsJson = globalThis.__pir_declaredTools;
var declaredGlobalsJson = globalThis.__pir_globals;
var storeSnapshotJson = globalThis.__pir_store;
var toolsClosed = globalThis.__pir_toolsClosed === true;
var ackHost = globalThis.__pir_ack;
var startClock = globalThis.__pir_clock;
var startSeed = globalThis.__pir_seed;
globalThis.__pir_callTool = undefined;
globalThis.__pir_callGlobal = undefined;
globalThis.__pir_hostOutput = undefined;
globalThis.__pir_allowedTools = undefined;
globalThis.__pir_declaredTools = undefined;
globalThis.__pir_globals = undefined;
globalThis.__pir_store = undefined;
globalThis.__pir_toolsClosed = undefined;
globalThis.__pir_ack = undefined;
globalThis.__pir_clock = undefined;
globalThis.__pir_seed = undefined;
var __pir_exitSentinel = Symbol("codemode-exit");
// QuickJS stacks list frames only; pir carries no frames across the bridge,
// so an Error renders as "Name: message" (upstream prelude errorText).
var __pir_errorText = function (error) {
    return error.message ? error.name + ": " + error.message : String(error.name);
};
// Console formatting (upstream prelude format()): strings as is, Error
// instances as "Name: message", everything else JSON with a String fallback.
var __pir_format = function (value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return __pir_errorText(value);
    try {
        var json = JSON.stringify(value);
        return json === undefined ? String(value) : json;
    } catch (e) { return String(value); }
};
// text() formatting (upstream prelude outputText()): primitives become their
// string form (undefined -> "undefined", NaN -> "NaN"), objects and
// functions JSON, with a String fallback when JSON cannot represent them.
var __pir_text = function (value) {
    if (value === undefined || value === null || (typeof value !== "object" && typeof value !== "function")) {
        return String(value);
    }
    try {
        var json = JSON.stringify(value);
        return json === undefined ? String(value) : json;
    } catch (e) { return String(value); }
};
// JSON serialization for tool arguments and the return value (upstream
// serializes both with JSON.stringify across the bridge).
var __pir_fmt = function (value) {
    if (typeof value === "string") return value;
    try { return JSON.stringify(value); } catch (e) { return String(value); }
};
// Output accounting (upstream prelude-source.ts output()): every text() and
// console.* item is counted — in the ONE measure the worker frames use, the
// JSON-escaped UTF-8 byte length the item contributes to its frame — against
// the host-configured caps; past a cap the script FAILS with a RangeError —
// the host ends the script with partial output and the cap error, so catching
// it does not resume output. Counting the escaped frame size (not the plain
// string length) is what makes the budget and the frame cap one definition:
// an admitted item always fits a single frame, including the escaping.
var __pir_utf8len = function (s) {
    var bytes = 0;
    for (var i = 0; i < s.length; i++) {
        var c = s.charCodeAt(i);
        if (c < 0x80) { bytes += 1; }
        else if (c < 0x800) { bytes += 2; }
        else if (c >= 0xD800 && c <= 0xDBFF && i + 1 < s.length) { bytes += 4; i++; }
        else { bytes += 3; }
    }
    return bytes;
};
// The escaped size of one item as it rides a frame's `text`/`value` field:
// JSON.stringify mirrors serde_json's escaping (short control escapes,
// quotes/backslashes doubled, other text verbatim); the two quote bytes are
// subtracted so plain text counts exactly its byte length. Printable ASCII
// takes the O(1)-per-char fast path (upstream counts `data.length` directly):
// such strings need no JSON escaping and are 1 UTF-8 byte per unit, so the
// exact size equals the string length and the C-speed regex avoids scanning
// megabyte items inside the JS interpreter.
var __pir_printableAscii = /^[\x20-\x7e]+$/;
var __pir_escapedLen = function (s) {
    if (__pir_printableAscii.test(s)) { return s.length; }
    var json = JSON.stringify(s);
    return __pir_utf8len(json === undefined ? "\"\"" : json) - 2;
};
var __pir_outputChars = 0;
var __pir_outputItems = 0;
var __pir_output = function (kind, data) {
    if (typeof data !== "string") return;
    __pir_outputChars += __pir_escapedLen(data);
    __pir_outputItems += 1;
    var maxChars = typeof globalThis.__pir_maxOutputChars === "number"
        ? globalThis.__pir_maxOutputChars : 16777216;
    var maxItems = typeof globalThis.__pir_maxOutputItems === "number"
        ? globalThis.__pir_maxOutputItems : 100000;
    if (__pir_outputChars > maxChars || __pir_outputItems > maxItems) {
        throw new RangeError("script output exceeded the limit of " + maxChars +
            " characters or " + maxItems + " text(), image(), and console calls. " +
            "Print a summary instead, or write large data to a file with a tool.");
    }
    hostOut(kind, data);
};
globalThis.text = function () {
    for (var i = 0; i < arguments.length; i++) {
        __pir_output("text", __pir_text(arguments[i]));
    }
};
globalThis.exit = function () { throw __pir_exitSentinel; };
globalThis.console = Object.freeze({
    log: function () { __pir_output("console", Array.prototype.map.call(arguments, __pir_format).join(" ")); },
    info: function () { __pir_output("console", Array.prototype.map.call(arguments, __pir_format).join(" ")); },
    warn: function () { __pir_output("console", Array.prototype.map.call(arguments, __pir_format).join(" ")); },
    error: function () { __pir_output("console", Array.prototype.map.call(arguments, __pir_format).join(" ")); },
    debug: function () { __pir_output("console", Array.prototype.map.call(arguments, __pir_format).join(" ")); },
});
// One script call, host-reply handling included (upstream prelude `caller()`
// + `settle()`): arguments are serialized, the reply is parsed, the
// (deterministic) clock of the result is applied, the host is acknowledged,
// and the promise resolves with the value or rejects with the failure
// message carrying the caller's extra `properties`.
var __pir_callThrough = async function (rawBridge, target, name, args, spread) {
    var payload;
    if (spread) { payload = __pir_fmt(Array.prototype.slice.call(args)); }
    else { payload = __pir_fmt(args === undefined ? {} : args); }
    var replyJson = await rawBridge(target, name, payload);
    var reply;
    try { reply = JSON.parse(replyJson); }
    catch (e) { throw new Error("tool call bridge returned an unreadable reply"); }
    if (typeof __pir_setClock === "function" && typeof reply.clock === "number") {
        __pir_setClock(reply.clock);
    }
    if (typeof ackHost === "function") { ackHost(); }
    if (reply && reply.ok) {
        if (reply.json) {
            try { return JSON.parse(reply.value); }
            catch (e) { throw new Error("tool result is not valid structured output"); }
        }
        return reply.value === undefined ? undefined : reply.value;
    }
    var failure = new Error((reply && reply.error) || ("tool '" + name + "' failed"));
    // Upstream `settle()`: a rejected call's declared properties become own
    // properties of the Error (`name`/`message`/`stack` are never replaced).
    if (reply && typeof reply.properties === "object" && reply.properties !== null) {
        for (var key of Object.keys(reply.properties)) {
            if (key === "name" || key === "message" || key === "stack") continue;
            try { failure[key] = reply.properties[key]; } catch (e) { /* read-only: skip */ }
        }
    }
    throw failure;
};
// The builtin route's live-registry face: every name resolves (the host
// rejects what it does not have), and a requested allowlist hides the rest.
var __pir_liveTool = function (prop) {
    if (typeof prop !== "string") return undefined;
    if (allowedTools
        && allowedTools.indexOf(prop) === -1) {
        return undefined;
    }
    return function (args) {
        return __pir_callThrough(bridge, "tool", prop, args, false);
    };
};
globalThis.tools = new Proxy({}, {
    get: function (target, prop) {
        return __pir_liveTool(prop);
    },
});
// Upstream prelude's declared table (the `CodemodeSandboxBase` route): the
// script sees exactly the tools its caller declared, with `ALL_TOOLS` listing
// them. A deterministic run keeps that table closed (upstream); an `execute()`
// run falls back to the live-registry face for names it did not declare, so
// the host can resolve them against the running session's tool set (pir's
// documented extension of the builtin sandbox model).
if (typeof declaredToolsJson === "string") {
    var declaredTools = JSON.parse(declaredToolsJson);
    var declaredTable = Object.create(null);
    var allTools = [];
    var declaredCaller = function (canonical) {
        return function (args) {
            return __pir_callThrough(bridge, "tool", canonical, args, false);
        };
    };
    for (var declaredIndex = 0; declaredIndex < declaredTools.length; declaredIndex++) {
        var declared = declaredTools[declaredIndex];
        var declaredJsName = declared.jsName || declared.name;
        var declaredFn = declaredCaller(declared.name);
        // The first tool wins when two names normalize to the same
        // identifier (upstream prelude), and a name kept raw stays callable.
        if (!(declaredJsName in declaredTable)) {
            declaredTable[declaredJsName] = declaredFn;
            allTools.push(Object.freeze({ name: declaredJsName, description: declared.description || "" }));
        }
        if (!(declared.name in declaredTable)) { declaredTable[declared.name] = declaredFn; }
    }
    if (toolsClosed) {
        globalThis.tools = Object.freeze(declaredTable);
    } else {
        globalThis.tools = new Proxy(declaredTable, {
            get: function (target, prop) {
                if (typeof prop === "string" && prop in target) return target[prop];
                return __pir_liveTool(prop);
            },
        });
    }
    globalThis.ALL_TOOLS = Object.freeze(allTools);
}
if (typeof declaredGlobalsJson === "string") {
    var declaredGlobals = JSON.parse(declaredGlobalsJson);
    var globalNamespaces = {};
    var globalCaller = function (canonical, spread) {
        return function () {
            // A global takes one argument; a spread global receives all call
            // arguments as an array (upstream `caller("global", name, spread)`).
            if (spread) {
                return __pir_callThrough(bridgeGlobal, "global", canonical, Array.prototype.slice.call(arguments), true);
            }
            return __pir_callThrough(
                bridgeGlobal, "global", canonical,
                arguments.length === 0 ? undefined : arguments[0], false,
            );
        };
    };
    for (var globalIndex = 0; globalIndex < declaredGlobals.length; globalIndex++) {
        var declaredGlobal = declaredGlobals[globalIndex];
        var globalFn = globalCaller(declaredGlobal.name, declaredGlobal.spread === true);
        var dotIndex = declaredGlobal.name.indexOf(".");
        if (dotIndex === -1) {
            Object.defineProperty(globalThis, declaredGlobal.name, {
                value: globalFn, enumerable: true, configurable: true, writable: true,
            });
            continue;
        }
        var namespaceName = declaredGlobal.name.slice(0, dotIndex);
        if (!globalNamespaces[namespaceName]) { globalNamespaces[namespaceName] = Object.create(null); }
        globalNamespaces[namespaceName][declaredGlobal.name.slice(dotIndex + 1)] = globalFn;
    }
    for (var namespaceName of Object.keys(globalNamespaces)) {
        var members = globalNamespaces[namespaceName];
        Object.freeze(members);
        Object.defineProperty(globalThis, namespaceName, {
            value: members, enumerable: true, configurable: true, writable: true,
        });
    }
}
// Script-visible store (upstream prelude `store()`/`load()`): a synchronous
// JSON-text Map snapshot; the keys the script wrote are reported with the
// result's `storeWrites` (the host reads `__pir_storeWritesJson()` after the
// script). Size caps are upstream's prelude values.
if (typeof storeSnapshotJson === "string") {
    var __pir_stored = new Map(Object.entries(JSON.parse(storeSnapshotJson)));
    var __pir_storeWrites = new Map();
    var __pir_storedChars = 0;
    __pir_stored.forEach(function (json, key) { __pir_storedChars += key.length + json.length; });
    var __pir_storeHint = "store() is for small state such as IDs or summaries. " +
        "Keep large data in variables, or write it to a file with a tool.";
    globalThis.store = function (key, value) {
        if (typeof key !== "string") throw new TypeError("store() key must be a string");
        var previous = __pir_stored.has(key) ? key.length + __pir_stored.get(key).length : 0;
        if (value === undefined) {
            __pir_stored.delete(key);
            __pir_storedChars -= previous;
            __pir_storeWrites.set(key, undefined);
            return;
        }
        var json;
        try { json = JSON.stringify(value); }
        catch (error) {
            throw new TypeError("store(" + JSON.stringify(key) + ") value is not JSON-serializable: " +
                __pir_format(error));
        }
        if (json === undefined) {
            throw new TypeError("store(" + JSON.stringify(key) + ") value is not JSON-serializable");
        }
        if (json.length > 262144) {
            throw new RangeError("store(" + JSON.stringify(key) + ") value has " + json.length +
                " characters of JSON, more than the limit of 262144. " + __pir_storeHint);
        }
        var next = __pir_storedChars - previous + key.length + json.length;
        if (next > 1048576) {
            throw new RangeError("store is full: stored values would exceed 1048576 characters of JSON. " +
                "Delete keys with store(key, undefined). " + __pir_storeHint);
        }
        __pir_stored.set(key, json);
        __pir_storedChars = next;
        __pir_storeWrites.set(key, json);
    };
    globalThis.load = function (key) {
        if (typeof key !== "string") throw new TypeError("load() key must be a string");
        var json = __pir_stored.get(key);
        return json === undefined ? undefined : JSON.parse(json);
    };
    globalThis.__pir_storeWritesJson = function () {
        var entries = [];
        __pir_storeWrites.forEach(function (json, key) {
            entries.push(json === undefined ? [key] : [key, json]);
        });
        return JSON.stringify(entries);
    };
}
// Deterministic run (`CodemodeSandboxBase.start()`): the VM clock reads
// `startedAt` and then the `now` of each delivered result (monotonic), and
// `Math.random()` comes from the run's seed. Residuals (documented): `new
// Date()` and every other clock face still read the host clock, and the
// fingerprint is the engine + protocol version rather than a wasm hash.
if (typeof startClock === "number") {
    var __pir_clockValue = startClock;
    globalThis.__pir_setClock = function (value) {
        if (typeof value !== "number" || !isFinite(value)) return;
        if (value > __pir_clockValue) { __pir_clockValue = value; }
    };
    Date.now = function () { return __pir_clockValue; };
}
if (typeof startSeed === "number") {
    var __pir_seedState = (startSeed >>> 0) || 1;
    Math.random = function () {
        __pir_seedState = (__pir_seedState + 0x6d2b79f5) >>> 0;
        var t = __pir_seedState;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}
// The models namespace (upstream createModelGlobals): calls route through
// the hidden bridge tools via the same host bridge as tools.<name>, so they
// work identically in the worker-process and thread sandboxes and surface as
// nested call rows. Independent of the `tools` allowlist (upstream: the
// models namespace is not part of the callable-tools set) — enforced at the
// bridge, which always passes the two `models.` bridge names through. The
// helper stays inside the prelude's closure: exposing it would let a script
// route an arbitrary name through the bridge.
var __pir_modelCall = async function (bridgeName, payload, options) {
    var opts = (options === undefined || options === null) ? {} : options;
    var request = { payload: payload, options: opts };
    var replyJson = await bridge("tool", bridgeName, __pir_fmt(request));
    var reply;
    try { reply = JSON.parse(replyJson); }
    catch (e) { throw new Error("model call bridge returned an unreadable reply"); }
    if (reply && reply.ok) {
        var value = reply.value;
        try { return JSON.parse(value); }
        catch (e) { throw new Error("model call result is not valid JSON"); }
    }
    throw new Error((reply && reply.error) || ("model call '" + bridgeName + "' failed"));
};
globalThis.models = Object.freeze({
    // models.generateImages(promptOrBlocks, { provider, model, apiKey? }):
    // returns the full result object — { api, provider, model,
    // output: [{ type: "text" } | { type: "image", data (base64), mimeType }],
    // stopReason, errorMessage?, usage? } (upstream AssistantImages).
    generateImages: function (prompt, options) {
        return __pir_modelCall("models.generateImages", prompt === undefined ? null : prompt, options);
    },
    // models.classify({ state, questions, images? }, { provider, model, apiKey? }):
    // returns { api, provider, model, answers: { <id>: { type: "choice" |
    // "score" | "bool", ... } }, stopReason, errorMessage?, usage? }
    // (upstream ClassifierResult).
    classify: function (context, options) {
        return __pir_modelCall("models.classify", context === undefined ? null : context, options);
    },
});
globalThis.__pir_run = async function (code) {
    try {
        var body = "(async () => {\n" + code + "\n})()";
        var factory = new Function("return " + body + ";");
        var value = await factory();
        // `json` is the strict JSON form of the return value (the extension
        // sandbox route resolves `CodemodeResult.value` from it); `value` is
        // the builtin path's legacy text form, and `undefined` marks `exit()`
        // / a bare `return`, which JSON cannot tell apart from `null`.
        var json;
        try { json = JSON.stringify(value); } catch (e) { json = undefined; }
        return JSON.stringify({
            ok: true,
            value: __pir_fmt(value === undefined ? null : value),
            json: json === undefined ? null : json,
            undefined: json === undefined,
        });
    } catch (error) {
        if (error === __pir_exitSentinel) {
            return JSON.stringify({ ok: true, value: null });
        }
        // Upstream describeError(): Error instances carry their name and
        // message; a thrown non-Error becomes { message: format(value) } —
        // strings as is, everything else formatted like console output — so
        // `throw 42` reports "42" and `throw { a: 1 }` reports '{"a":1}'
        // instead of "[object Object]".
        var name;
        var message;
        if (error instanceof Error) {
            name = String(error.name);
            message = String(error.message);
        } else {
            name = null;
            message = __pir_format(error);
        }
        return JSON.stringify({ ok: false, name: name, error: message });
    }
};
})();
"#;

/// Result of one script execution.
#[derive(Debug)]
struct ScriptOutcome {
    /// "Script completed" / "Script failed" classification.
    ok: bool,
    /// Model-visible output: the rendered items (plus partial output on
    /// failure), already inline-budget truncated. Error text is carried
    /// separately in [`ScriptOutcome::error_head`].
    output: String,
    /// The upstream error head for a failed script: `{Name}: {message}` for
    /// script errors, `Script timed out: ...` for the deadline, and
    /// `Script sandbox failed: ...` for worker failures (upstream
    /// `formatError`; pir carries no stack frames across the bridge).
    error_head: Option<String>,
}

/// Per-execution sandbox limits (upstream worker options: `timeoutMs`, the
/// output-token budget, and the output caps).
#[derive(Debug, Clone, Copy)]
struct SandboxLimits {
    /// Model-visible output budget in estimated tokens (truncation); the
    /// `@options` `max_output_tokens` overrides the configured inline budget.
    inline_budget: u32,
    /// Hard deadline for the whole script, including tool calls (`None` = no
    /// deadline — upstream's `Infinity` default in a `start()` run); the
    /// `@options` `timeout_ms` overrides the default timeout.
    timeout: Option<Duration>,
    /// Output caps across text + console (see [`output_caps`]).
    max_output_chars: usize,
    max_output_items: usize,
    /// QuickJS heap cap of the sandbox run in bytes (`None` = no cap;
    /// `codemode.sandboxMemoryMb` / `PIR_CODEMODE_MEMORY_MB`).
    memory_bytes: Option<usize>,
}

impl SandboxLimits {
    /// Resolve the limits for one execution: the configured inline budget
    /// (possibly overridden by `@options.max_output_tokens`), the `@options`
    /// timeout (else the default), and the configured heap cap.
    fn resolve(
        inline_budget: u32,
        timeout_override: Option<Duration>,
        memory_bytes: Option<usize>,
    ) -> Self {
        let (max_output_chars, max_output_items) = output_caps();
        Self {
            inline_budget,
            timeout: Some(timeout_override.unwrap_or_else(sandbox_timeout)),
            max_output_chars,
            max_output_items,
            memory_bytes,
        }
    }

    /// Resolve the limits of one extension-sandbox run (upstream
    /// `CodemodeSandboxBase`): the caller's `timeoutMs`, where `None` means
    /// "no deadline" for a deterministic run and the sandbox default
    /// ([`CODEMODE_DEFAULT_TIMEOUT_MS`]) for an `execute()` run.
    fn for_extension_run(
        inline_budget: u32,
        timeout: Option<Duration>,
        memory_bytes: Option<usize>,
    ) -> Self {
        let (max_output_chars, max_output_items) = output_caps();
        Self {
            inline_budget,
            timeout,
            max_output_chars,
            max_output_items,
            memory_bytes,
        }
    }
}

/// Default per-execution deadline of the extension sandbox route (upstream
/// `CodemodeSandboxBase` `DEFAULT_TIMEOUT_MS`).
pub const CODEMODE_DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// The call kind of one script call (upstream's worker protocol `target`):
/// `tools.<name>()` is a tool call (recorded in `result.calls`), a declared
/// global is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptTarget {
    Tool,
    Global,
}

impl ScriptTarget {
    fn as_str(self) -> &'static str {
        match self {
            ScriptTarget::Tool => "tool",
            ScriptTarget::Global => "global",
        }
    }

    fn from_str(value: &str) -> Self {
        if value == "global" {
            ScriptTarget::Global
        } else {
            ScriptTarget::Tool
        }
    }
}

/// One call reply in the sandbox bridge's shape (upstream `settle(id, ok,
/// payload, propertiesJson)` plus the deterministic clock of the result).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallReply {
    pub ok: bool,
    /// The tool's text output, or its structured output serialized to JSON
    /// when [`ToolCallReply::json`]; `None` = the tool resolved `undefined`
    /// (upstream's `payload: undefined`).
    pub value: Option<String>,
    /// Whether `value` is structured output (upstream resolves outputSchema
    /// tools to structuredContent).
    pub json: bool,
    pub error: Option<String>,
    /// Own properties of a rejection's Error (upstream `settle`), applied JS
    /// side except `name`/`message`/`stack`.
    pub properties: Option<JsonValue>,
    /// The VM clock value of the result (deterministic runs only).
    pub clock: Option<f64>,
    /// How long the call took, for the call record (host-measured for the
    /// session tier, JS-reported for the extension tier).
    pub duration_ms: u64,
}

impl ToolCallReply {
    /// A successful call carrying a value (`json` marks structured output).
    pub fn ok(value: String, json: bool) -> Self {
        Self {
            ok: true,
            value: Some(value),
            json,
            ..Self::default()
        }
    }

    /// A successful call that resolved `undefined`.
    pub fn ok_undefined() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    /// A failed call.
    pub fn failed(error: String) -> Self {
        Self {
            ok: false,
            error: Some(error),
            ..Self::default()
        }
    }

    /// The bridge-reply JSON the prelude parses.
    ///
    /// Consumes the reply: every call site owns it, so the value / error /
    /// properties payloads are MOVED into the JSON instead of deep-cloned.
    /// `value` can be a script's entire return string, so the old `&self`
    /// form duplicated the largest object on the codemode tool-call path.
    pub fn to_bridge_json(self) -> String {
        let mut reply = serde_json::Map::new();
        reply.insert("ok".to_owned(), JsonValue::Bool(self.ok));
        if let Some(value) = self.value {
            reply.insert("value".to_owned(), JsonValue::String(value));
        }
        if self.json {
            reply.insert("json".to_owned(), JsonValue::Bool(true));
        }
        if let Some(error) = self.error {
            reply.insert("error".to_owned(), JsonValue::String(error));
        }
        if let Some(properties) = self.properties {
            reply.insert("properties".to_owned(), properties);
        }
        if let Some(clock) = self.clock {
            reply.insert("clock".to_owned(), serde_json::json!(clock));
        }
        JsonValue::Object(reply).to_string()
    }
}

/// Answers one script call. The builtin `codemode` tool routes to the host
/// session's nested-call executor ([`ScriptRouting::Session`]); the extension
/// sandbox route answers through its JS side ([`ScriptRouting::Custom`]).
pub trait ScriptToolResponder: Send + Sync + 'static {
    fn call(
        &self,
        target: ScriptTarget,
        name: String,
        args_json: String,
    ) -> BoxFuture<'static, ToolCallReply>;
}

/// The tool-call path of one sandbox run.
#[derive(Clone)]
enum ScriptRouting {
    /// Host session tools, through the nested-call executor. Held behind an
    /// `Arc` because the routing is cloned once per script tool call (the
    /// bridge responder's per-call clone), so a plain executor value would
    /// re-allocate its caller-id `String` on every call the script makes.
    Session(Arc<NestedToolExecutor>),
    /// A caller-supplied responder.
    Custom(Arc<dyn ScriptToolResponder>),
}

/// One script call record (upstream `CodemodeCall`): tools only, in issue
/// order, with the status the call reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptCallRecord {
    pub name: String,
    /// `"ok"` | `"error"` | `"cancelled"`.
    pub status: String,
    pub duration_ms: u64,
}

impl ScriptCallRecord {
    fn to_json(&self) -> JsonValue {
        serde_json::json!({
            "name": self.name,
            "status": self.status,
            "durationMs": self.duration_ms,
        })
    }
}

/// The classified end of one script run, for the upstream
/// `CodemodeResult.error` face (the builtin path keeps its own text head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptErrorInfo {
    /// `"script"` | `"timeout"` | `"aborted"` | `"sandbox"`.
    pub kind: &'static str,
    /// The script error's `name` (script errors only).
    pub name: Option<String>,
    pub message: String,
}

/// The structured end of one script run: the fields both the builtin text
/// outcome and the upstream `CodemodeResult` object are assembled from.
#[derive(Debug, Default, Clone)]
struct RunOutcome {
    ok: bool,
    /// The builtin path's return-value text (the prelude's `__pir_fmt` form).
    value: Option<String>,
    /// The strict JSON text of the return value; `None` = the script
    /// produced `undefined` (`exit()`, a bare `return`, or a value JSON
    /// cannot represent).
    value_json: Option<String>,
    /// The builtin-path error head (upstream `formatError`).
    error_head: Option<String>,
    /// The structured error (extension sandbox route).
    error: Option<ScriptErrorInfo>,
    /// Script `store()` writes (upstream `CodemodeStoreWrites`).
    store_writes: Option<JsonValue>,
}

/// Run `code` in a fresh minimal QuickJS runtime with only the tool-call
/// bridge available. The runtime is torn down when this returns.
///
/// The QuickJS future is not `Send` (QuickJS values must stay on one
/// thread), so — like upstream's worker process — the sandbox runs on a
/// **dedicated worker thread** with a current-thread tokio runtime. Nested
/// tool calls are spawned back onto the agent-loop runtime through the
/// captured [`tokio::runtime::Handle`], so the shared scheduler, hooks, and
/// queue stay on the loop's executor.
///
/// The mechanics mirror `src/extensions/runtime`: a QuickJS interrupt-handler
/// deadline that actually stops a runaway loop, a tokio deadline as the outer
/// bound, and post-interrupt recovery (clear the pending exception, drain
/// pending jobs) so the runtime is droppable.
async fn run_sandbox(
    code: &str,
    allowed_tools: Option<&[String]>,
    routing: ScriptRouting,
    signal: Option<AbortSignal>,
    limits: SandboxLimits,
) -> ScriptOutcome {
    run_sandbox_report(code, allowed_tools, routing, signal, limits, None, None)
        .await
        .0
}

/// Lifecycle handle of one in-process sandbox thread.
///
/// The thread's join handle plus the cancellation signal its QuickJS interrupt
/// handler reads. When the owning future completes, [`Self::join`] waits for
/// the thread in place (it has already sent its report, so this is immediate).
/// When the owning future is DROPPED instead — a task abort or a cancelled
/// tool call — `Drop` aborts the signal and hands the join to a reaper thread:
/// the JS job unwinds at its next instruction and the OS stack + QuickJS heap
/// are reclaimed well before the run's deadline, without ever blocking an
/// async executor thread on a stuck host call.
struct SandboxThread {
    cancel: AbortSignal,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Whether `Drop` should abort the signal. Cleared by [`Self::join`]: a
    /// completed run must not abort a caller-owned signal.
    cancel_on_drop: bool,
}

impl SandboxThread {
    fn new(cancel: AbortSignal, handle: Option<std::thread::JoinHandle<()>>) -> Self {
        Self {
            cancel,
            handle,
            cancel_on_drop: true,
        }
    }

    /// The run's outcome has been observed: wait for the thread in place.
    fn join(mut self) {
        self.cancel_on_drop = false;
        if let Some(handle) = self.handle.take() {
            // The thread sends its report before returning; it cannot block
            // here on anything the caller is still waiting for.
            let _ = handle.join();
        }
    }
}

impl Drop for SandboxThread {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            // Same signal the sandbox's interrupt handler polls: the script
            // stops uncatchably at its next instruction.
            self.cancel.abort();
        }
        let Some(handle) = self.handle.take() else {
            return;
        };
        // Join off the async executor thread: the interrupt lands promptly,
        // but the drop itself must not stall a tokio worker on a host call
        // that is still unwinding.
        let _ = std::thread::Builder::new()
            .name("pir-codemode-reaper".to_owned())
            .spawn(move || {
                let _ = handle.join();
            });
    }
}

/// In-process sandbox run used by the cancellation test (E-2/E11).
///
/// Runs `code` through the exact spawn / `SandboxThread` guard path the public
/// routes use, but with no tools and no worker process, and returns the run's
/// error head (or its output). Dropping the returned future is the
/// cancellation signal: the script must stop and [`sandbox_threads_alive`]
/// must fall back to its baseline well before `timeout_ms`.
#[doc(hidden)]
pub async fn run_sandbox_for_tests(code: &str, timeout_ms: u64) -> String {
    struct NoTools;
    impl ScriptToolResponder for NoTools {
        fn call(
            &self,
            _target: ScriptTarget,
            name: String,
            _args_json: String,
        ) -> BoxFuture<'static, ToolCallReply> {
            Box::pin(async move { ToolCallReply::failed(format!("no tool: {name}")) })
        }
    }
    let limits = SandboxLimits::resolve(
        DEFAULT_INLINE_BUDGET,
        Some(Duration::from_millis(timeout_ms.max(1))),
        None,
    );
    let (outcome, _report) = run_sandbox_report(
        code,
        None,
        ScriptRouting::Custom(Arc::new(NoTools)),
        None,
        limits,
        None,
        None,
    )
    .await;
    outcome.error_head.unwrap_or(outcome.output)
}

/// [`run_sandbox`] plus the structured end of the run, the per-call records
/// and the raw output items (the extension sandbox route reads all three).
///
/// `progress` is the extension route's run progress (delivered/consumed
/// result ordinals + issued call count); `extras` carries the prelude inputs
/// only that route sets (declared tools/globals, store, deterministic
/// clock/randomness). Both are `None` for the builtin path.
async fn run_sandbox_report(
    code: &str,
    allowed_tools: Option<&[String]>,
    routing: ScriptRouting,
    signal: Option<AbortSignal>,
    limits: SandboxLimits,
    progress: Option<Arc<RunProgress>>,
    extras: Option<ExtensionRunExtras>,
) -> (ScriptOutcome, RunReport) {
    // Worker-process isolation first (upstream host.ts/worker.ts): the child
    // only runs JS; tool calls bridge back here. `None` means the worker
    // process is unavailable (spawn/handshake failure, foreign current_exe,
    // or PIR_CODEMODE_WORKER=off) — fall through to the in-process thread
    // sandbox below. The fallback only happens before any script side
    // effect, so re-running the script is safe.
    //
    // The extension sandbox route skips the worker process by construction:
    // its calls must come back to the extension actor (the JS side answers
    // them), so the hop would only add a process boundary without isolation.
    if let ScriptRouting::Session(executor) = &routing {
        if let Some(outcome) = worker::run_in_worker_process(
            code,
            allowed_tools,
            // The worker fn still takes an owned executor (out of this
            // module); one value clone per run — the Arc's benefit is per
            // CALL, where `routing.clone()` is now a refcount bump.
            (**executor).clone(),
            signal.clone(),
            limits,
        )
        .await
        {
            let report = RunReport {
                outcome: RunOutcome {
                    ok: outcome.ok,
                    error_head: outcome.error_head.clone(),
                    ..RunOutcome::default()
                },
                ..RunReport::default()
            };
            return (outcome, report);
        }
    }

    let output = Arc::new(StdMutex::new(OutputBuffer::new(
        limits.max_output_chars,
        limits.max_output_items,
    )));
    // The nested executor runs on the agent-loop runtime; capture its handle
    // before moving to the worker thread.
    let Ok(loop_handle) = tokio::runtime::Handle::try_current() else {
        let head = "Script sandbox failed: codemode must run inside a tokio runtime".to_owned();
        let info = ScriptErrorInfo {
            kind: "sandbox",
            name: None,
            message: head.clone(),
        };
        return (
            ScriptOutcome {
                ok: false,
                output: String::new(),
                error_head: Some(head.clone()),
            },
            RunReport {
                outcome: RunOutcome {
                    ok: false,
                    error_head: Some(head),
                    error: Some(info),
                    ..RunOutcome::default()
                },
                ..RunReport::default()
            },
        );
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<RunReport>();
    let code = code.to_owned();
    let allowed_tools = allowed_tools.map(|names| serde_json::to_string(names).unwrap_or_default());
    let worker_output = Arc::clone(&output);
    // The sandbox thread must not outlive its caller (E11). `signal` is the
    // caller's cancellation signal; when there is none a private one is
    // installed so cancellation has a hook either way. The guard aborts that
    // signal (the sandbox's interrupt handler reads it) and joins the thread
    // when this future is dropped, so a cancelled run cannot keep its OS
    // stack + QuickJS heap alive until its own deadline or stack up behind
    // repeated cancellations.
    let cancel = signal.clone().unwrap_or_default();
    let signal = signal.or_else(|| Some(cancel.clone()));
    // The sandbox grants JS `SANDBOX_JS_STACK_BYTES` of stack
    // (execute_in_runtime); the OS thread reserves the derived budget
    // (`sandbox_thread_stack_bytes`: 2x the JS stack + native headroom), so
    // deep recursion raises QuickJS's RangeError before the OS stack can
    // overflow. A failed spawn drops `tx`, which lands on the "sandbox worker
    // terminated" arm below (defense in depth, same as a panicked worker).
    let spawned = std::thread::Builder::new()
        .name("pir-codemode-sandbox".to_owned())
        .stack_size(sandbox_thread_stack_bytes())
        .spawn(move || {
            let _alive = SandboxThreadAlive::enter();
            let report = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(worker) => worker.block_on(execute_sandbox(
                    code,
                    allowed_tools,
                    routing,
                    signal,
                    worker_output,
                    loop_handle,
                    limits,
                    progress,
                    extras,
                )),
                Err(error) => RunReport {
                    outcome: RunOutcome {
                        ok: false,
                        error_head: Some(format!(
                            "Script sandbox failed: failed to start the sandbox worker: {error}"
                        )),
                        ..RunOutcome::default()
                    },
                    ..RunReport::default()
                },
            };
            let _ = tx.send(report);
        });
    let thread = SandboxThread::new(cancel, spawned.ok());
    match rx.await {
        Ok(report) => {
            // The report arrived: the script is over and the thread is only
            // unwinding, so joining here is immediate and keeps the lifecycle
            // deterministic.
            thread.join();
            let outcome = assemble_run_outcome(&report, limits);
            let mut report = report;
            report.outcome.error_head = outcome.error_head.clone();
            (outcome, report)
        }
        // A dropped sender means the worker panicked before sending (defense
        // in depth); the guard still joins the (dead) thread.
        Err(_) => {
            thread.join();
            let (items, over_limit) = output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let partial = render_output(&items);
            let head = if over_limit {
                format!(
                    "RangeError: {}",
                    output_limit_error(limits.max_output_chars, limits.max_output_items)
                )
            } else {
                "Script sandbox failed: sandbox worker terminated".to_owned()
            };
            let info = ScriptErrorInfo {
                kind: "sandbox",
                name: None,
                message: head.clone(),
            };
            (
                ScriptOutcome {
                    ok: false,
                    output: truncate_to_budget(&partial, limits.inline_budget),
                    error_head: Some(head.clone()),
                },
                RunReport {
                    outcome: RunOutcome {
                        ok: false,
                        error_head: Some(head),
                        error: Some(info),
                        ..RunOutcome::default()
                    },
                    items,
                },
            )
        }
    }
}

/// What the sandbox thread reports back: the classified end of the run plus
/// the output items as the script produced them (before rendering).
#[derive(Debug, Default)]
struct RunReport {
    outcome: RunOutcome,
    items: Vec<OutputItem>,
}

/// Assemble the builtin path's text outcome from the structured report
/// (upstream's `done()` rendering: output items + the return value, then the
/// error head).
fn assemble_run_outcome(report: &RunReport, limits: SandboxLimits) -> ScriptOutcome {
    worker::assemble_script_outcome(
        report.outcome.ok,
        report.outcome.error_head.clone(),
        report.outcome.value.clone(),
        report.items.clone(),
        limits,
    )
}

/// The prelude inputs only the extension sandbox route sets.
struct ExtensionRunExtras {
    /// The declared tools as JSON (`[{name, jsName, description}]`).
    declared_tools: String,
    /// The declared globals as JSON (`[{name, spread}]`).
    globals: String,
    /// The script-visible store snapshot as JSON.
    store: String,
    /// `(startedAt, seed)` for a deterministic run.
    deterministic: Option<(f64, f64)>,
    /// The session the VM acknowledges each consumed result to (`__pir_ack`).
    session: Arc<SandboxSession>,
}

/// Execute one script end-to-end on the worker thread and assemble the
/// `RunReport` (output items + error/timeout classification).
#[allow(clippy::too_many_arguments)]
async fn execute_sandbox(
    code: String,
    allowed_tools_json: Option<String>,
    routing: ScriptRouting,
    signal: Option<AbortSignal>,
    output: Arc<StdMutex<OutputBuffer>>,
    loop_handle: tokio::runtime::Handle,
    limits: SandboxLimits,
    progress: Option<Arc<RunProgress>>,
    extras: Option<ExtensionRunExtras>,
) -> RunReport {
    let eval_result = execute_in_runtime(
        code,
        allowed_tools_json,
        routing,
        signal,
        Arc::clone(&output),
        loop_handle,
        limits,
        progress,
        extras,
    )
    .await;

    let outcome = match eval_result {
        Ok(Ok(EvalSuccess {
            value,
            store_writes,
        })) => RunOutcome {
            ok: true,
            // `value` is owned by this match arm and never read again: move
            // the script's return text/JSON into the report instead of
            // cloning it (a script may return a large string or object).
            value: value.text,
            value_json: value.json,
            store_writes: Some(store_writes),
            ..RunOutcome::default()
        },
        Ok(Err(failure)) => RunOutcome {
            ok: false,
            error_head: Some(failure.head),
            error: Some(failure.info),
            ..RunOutcome::default()
        },
        Err(elapsed) => {
            let message = format!("Execution timed out after {} ms", elapsed.as_millis());
            RunOutcome {
                ok: false,
                error_head: Some(format!("Script timed out: {message}")),
                error: Some(ScriptErrorInfo {
                    kind: "timeout",
                    name: None,
                    message,
                }),
                ..RunOutcome::default()
            }
        }
    };
    let (items, over_limit) = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let mut outcome = outcome;
    // Host-side output-cap enforcement (defense in depth against a tampered
    // prelude wrapper): past the caps the script fails like upstream's
    // `done()` error path. The buffer refused items past the cap, so the
    // buffered output stays bounded.
    if over_limit {
        let text = output_limit_error(limits.max_output_chars, limits.max_output_items);
        outcome.ok = false;
        outcome.error_head = Some(format!("RangeError: {text}"));
        outcome.error = Some(ScriptErrorInfo {
            kind: "script",
            name: Some("RangeError".to_owned()),
            message: text,
        });
    }
    RunReport { outcome, items }
}

/// The script's top-level return value: the builtin path's text form (the
/// prelude's `__pir_fmt`) plus the strict JSON text the extension sandbox
/// route resolves `CodemodeResult.value` from (`None` = `undefined`, which is
/// what `exit()` / a bare `return` produce; an explicit `return null` is the
/// JSON text `null`).
#[derive(Debug, Clone, Default)]
struct ReturnValue {
    text: Option<String>,
    json: Option<String>,
}

/// A successful eval: the return value plus the script's `store()` writes.
#[derive(Debug, Clone, Default)]
struct EvalSuccess {
    value: ReturnValue,
    store_writes: JsonValue,
}

/// A failed eval, classified for both outcome faces.
#[derive(Debug, Clone)]
struct EvalFailure {
    /// The builtin path's error head (upstream `formatError`).
    head: String,
    /// The structured error (extension sandbox route).
    info: ScriptErrorInfo,
}

/// The inner runtime execution: build the runtime, register the bridge, eval
/// the prelude + script, and produce `Ok(Ok(success))` /
/// `Ok(Err(script error))` / `Err(elapsed)`.
#[allow(clippy::too_many_arguments)]
async fn execute_in_runtime(
    code: String,
    allowed_tools_json: Option<String>,
    routing: ScriptRouting,
    signal: Option<AbortSignal>,
    output: Arc<StdMutex<OutputBuffer>>,
    loop_handle: tokio::runtime::Handle,
    limits: SandboxLimits,
    progress: Option<Arc<RunProgress>>,
    extras: Option<ExtensionRunExtras>,
) -> Result<Result<EvalSuccess, EvalFailure>, Duration> {
    use rquickjs::prelude::{Async as JsAsync, Func};
    use rquickjs::promise::MaybePromise;
    use rquickjs::{AsyncContext, AsyncRuntime};

    let deadline: Arc<StdMutex<Option<Instant>>> = Arc::default();
    let runtime = match AsyncRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            return Ok(Err(EvalFailure {
                head: format!("failed to start the codemode sandbox: {error}"),
                info: ScriptErrorInfo {
                    kind: "sandbox",
                    name: None,
                    message: format!("failed to start the codemode sandbox: {error}"),
                },
            }))
        }
    };
    // The JS stack budget the sandbox thread's OS stack is derived from
    // (`sandbox_thread_stack_bytes`): the 1 MiB engine default RangeErrors far
    // too early for real recursive scripts.
    runtime.set_max_stack_size(SANDBOX_JS_STACK_BYTES).await;
    // Heap cap from `codemode.sandboxMemoryMb` (default 128 MiB);
    // `PIR_CODEMODE_MEMORY_MB=0` / `sandboxMemoryMb: 0` = no cap.
    if let Some(memory_bytes) = limits.memory_bytes {
        runtime.set_memory_limit(memory_bytes).await;
    }
    // Interrupt handler: aborts the JS job once the deadline passes OR the
    // run's abort signal fires, so a runaway loop is actually stopped (not
    // just timed out from outside) and cancellation is honored mid-script —
    // a compute-bound loop cannot burn CPU until the deadline.
    {
        let deadline = Arc::clone(&deadline);
        let interrupt_signal = signal.clone();
        runtime
            .set_interrupt_handler(Some(Box::new(move || {
                if interrupt_signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                {
                    return true;
                }
                let guard = deadline
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match *guard {
                    Some(deadline) => Instant::now() >= deadline,
                    None => false,
                }
            })))
            .await;
    }
    let ctx = match AsyncContext::full(&runtime).await {
        Ok(ctx) => ctx,
        Err(error) => {
            return Ok(Err(EvalFailure {
                head: format!("failed to start the codemode sandbox: {error}"),
                info: ScriptErrorInfo {
                    kind: "sandbox",
                    name: None,
                    message: format!("failed to start the codemode sandbox: {error}"),
                },
            }))
        }
    };

    // The single host capability: one call bridge for `tools.<name>()`
    // (target `tool`) and one for declared globals (target `global`). Reply
    // shape: {"ok":bool,"value"?:string,"json"?:bool,"error"?:string,
    // "properties"?:{...},"clock"?:number} — `json` marks `value` as
    // structured output (upstream resolves outputSchema tools to
    // structuredContent), `properties` ride a rejection's Error, and `clock`
    // is the deterministic run's VM clock for the result being processed.
    //
    // The bridge itself enforces the allowlist (the prelude's Proxy is only
    // the first gate — the raw bridge must not be a script-reachable bypass
    // around it). Parsed once from the same JSON the Proxy's captured list
    // is built from.
    let allowed_tools: Option<Arc<[String]>> = allowed_tools_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .map(Arc::from);
    {
        let output = Arc::clone(&output);
        let bridge_routing = routing.clone();
        // Call records of the builtin/session path: the builtin tool's result
        // text does not carry them (its nested-call record lives on the agent
        // loop's tool result), so this list is only read by the extension
        // route — which records into its own session instead.
        let bridge_calls: Arc<StdMutex<Vec<ScriptCallRecord>>> = Arc::default();
        let bridge_signal = signal.clone();
        let bridge_progress = progress.clone();
        let registered = ctx
            .async_with(async |js_ctx| {
                let globals = js_ctx.globals();
                let output = Arc::clone(&output);
                // Registered as `__pir_hostOutput`: the prelude wraps it with
                // the output-cap accounting (see SANDBOX_PRELUDE).
                globals.set(
                    "__pir_hostOutput",
                    Func::from(move |kind: String, text: String| {
                        output
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(if kind == "console" { "console" } else { "text" }, text);
                    }),
                )?;
                // Host-configured output caps for the prelude's accounting.
                js_ctx.eval::<(), _>(format!(
                    "globalThis.__pir_maxOutputChars = {}; globalThis.__pir_maxOutputItems = {};",
                    limits.max_output_chars, limits.max_output_items
                ))?;

                // One responder shared by both bridges: the allowlist /
                // aborted / `codemode` guards are identical, and only the
                // call routing differs.
                let respond = {
                    let routing = bridge_routing.clone();
                    let calls = Arc::clone(&bridge_calls);
                    let signal = bridge_signal.clone();
                    let progress = bridge_progress.clone();
                    let allowed = allowed_tools.clone();
                    let loop_handle = loop_handle.clone();
                    move |target: ScriptTarget, name: String, args_json: String| {
                        let routing = routing.clone();
                        let calls = Arc::clone(&calls);
                        let signal = signal.clone();
                        let progress = progress.clone();
                        let allowed = allowed.clone();
                        let loop_handle = loop_handle.clone();
                        async move {
                            call_script_tool(
                                target,
                                name,
                                args_json,
                                routing,
                                calls,
                                signal,
                                progress,
                                allowed,
                                loop_handle,
                            )
                            .await
                        }
                    }
                };
                let tool_respond = respond.clone();
                globals.set(
                    "__pir_callTool",
                    Func::from(JsAsync(
                        move |target: String, name: String, args_json: String| {
                            let respond = tool_respond.clone();
                            async move {
                                respond(ScriptTarget::from_str(&target), name, args_json)
                                    .await
                                    .to_bridge_json()
                            }
                        },
                    )),
                )?;
                let global_respond = respond;
                globals.set(
                    "__pir_callGlobal",
                    Func::from(JsAsync(
                        move |target: String, name: String, args_json: String| {
                            let respond = global_respond.clone();
                            async move {
                                respond(ScriptTarget::from_str(&target), name, args_json)
                                    .await
                                    .to_bridge_json()
                            }
                        },
                    )),
                )?;
                // Allowlist default: everything (replaced before the prelude
                // runs when a `tools` list was requested — the prelude
                // captures the list into its closure and clears the global,
                // so the script can neither widen it nor reach the raw
                // bridge). A JSON array literal is a valid JS expression.
                js_ctx.eval::<(), _>("globalThis.__pir_allowedTools = null;")?;
                if let Some(allowed) = &allowed_tools_json {
                    js_ctx.eval::<(), _>(format!("globalThis.__pir_allowedTools = {allowed};"))?;
                }
                // Extension-route prelude inputs: the declared table, the
                // globals, the store snapshot, the deterministic clock/seed
                // and the per-result acknowledge. All absent for the builtin
                // `codemode` tool (its reduced prelude is a documented
                // deviation).
                if let Some(extras) = &extras {
                    let literal = |json: &str| {
                        serde_json::to_string(json).unwrap_or_else(|_| "\"\"".to_owned())
                    };
                    js_ctx.eval::<(), _>(format!(
                        "globalThis.__pir_declaredTools = {};",
                        literal(&extras.declared_tools)
                    ))?;
                    js_ctx.eval::<(), _>(format!(
                        "globalThis.__pir_globals = {};",
                        literal(&extras.globals)
                    ))?;
                    js_ctx.eval::<(), _>(format!(
                        "globalThis.__pir_store = {};",
                        literal(&extras.store)
                    ))?;
                    // A deterministic run's script sees exactly its declared
                    // tools (upstream, a closed table); an `execute()` run
                    // additionally keeps the live-registry fallback for names
                    // it did not declare (see SANDBOX_PRELUDE).
                    js_ctx.eval::<(), _>(format!(
                        "globalThis.__pir_toolsClosed = {};",
                        if extras.deterministic.is_some() {
                            "true"
                        } else {
                            "false"
                        }
                    ))?;
                    if let Some((started_at, seed)) = extras.deterministic {
                        js_ctx.eval::<(), _>(format!(
                            "globalThis.__pir_clock = {}; globalThis.__pir_seed = {};",
                            started_at, seed
                        ))?;
                    }
                    let ack_session = Arc::clone(&extras.session);
                    globals.set("__pir_ack", Func::from(move || ack_session.acknowledge()))?;
                }
                js_ctx.eval::<(), _>(SANDBOX_PRELUDE)?;
                Ok::<_, rquickjs::Error>(())
            })
            .await;
        if let Err(error) = registered {
            return Ok(Err(EvalFailure {
                head: format!("failed to prepare the codemode sandbox: {error}"),
                info: ScriptErrorInfo {
                    kind: "sandbox",
                    name: None,
                    message: format!("failed to prepare the codemode sandbox: {error}"),
                },
            }));
        }
    }

    // Arm the interrupt deadline and eval the script under the timeout (the
    // `@options` `timeout_ms` when set, else the default sandbox timeout).
    // `None` = no deadline (a deterministic `start()` run): only the abort
    // signal can stop it.
    let timeout = limits.timeout;
    let started = Instant::now();
    *deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout.map(|t| started + t);
    let call = ctx.async_with(async |js_ctx| {
        let run: rquickjs::Function = js_ctx.globals().get("__pir_run")?;
        let result: MaybePromise = run.call((code,))?;
        result.into_future::<String>().await
    });
    let outcome = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, call).await,
        None => Ok(call.await),
    };
    // Disarm regardless of outcome.
    *deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

    match outcome {
        Ok(Ok(reply_json)) => {
            // The script's `store()` writes are read while the runtime is
            // still alive (upstream reports them with a successful `done`).
            let store_writes = read_store_writes(&ctx).await;
            recover(&ctx, &runtime).await;
            let parsed: JsonValue = serde_json::from_str(&reply_json).unwrap_or(JsonValue::Null);
            if parsed.get("ok").and_then(JsonValue::as_bool) == Some(true) {
                // `json` is the strict JSON text of the return value; its
                // absence means the script produced `undefined` (`exit()`, a
                // bare `return`, or a value JSON cannot represent).
                Ok(Ok(EvalSuccess {
                    value: ReturnValue {
                        text: parsed
                            .get("value")
                            .and_then(JsonValue::as_str)
                            .map(str::to_owned),
                        json: parsed
                            .get("json")
                            .and_then(JsonValue::as_str)
                            .map(str::to_owned),
                    },
                    store_writes,
                }))
            } else {
                // Upstream error head (formatError): `${name}: ${message}`,
                // with the name defaulting to "Error" for thrown non-Error
                // values (the prelude serializes {name, error}).
                let message = parsed
                    .get("error")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown script error");
                let name = match parsed.get("name").and_then(JsonValue::as_str) {
                    Some(name) if !name.is_empty() => Some(name.to_owned()),
                    _ => None,
                };
                let head = match &name {
                    Some(name) => format!("{name}: {message}"),
                    None => format!("Error: {message}"),
                };
                Ok(Err(EvalFailure {
                    head,
                    info: ScriptErrorInfo {
                        kind: "script",
                        name,
                        message: message.to_owned(),
                    },
                }))
            }
        }
        Ok(Err(js_error)) => {
            let aborted = signal.as_ref().is_some_and(|signal| signal.is_aborted());
            let head = format!("script execution failed: {js_error}");
            // `__pir_run` catches script errors itself, so this path means the
            // JS job was interrupted (the uncatchable interrupt exception) or
            // a host bridge failed. Classify the two intended interrupt
            // triggers — the caller's abort and the deadline — as upstream's
            // `aborted`/`timeout` rather than a sandbox failure; whichever of
            // the interrupt handler and the tokio deadline noticed first, the
            // face is the same.
            let deadline_passed = timeout.is_some_and(|timeout| started.elapsed() >= timeout);
            let info = if aborted {
                ScriptErrorInfo {
                    kind: "aborted",
                    name: None,
                    message: "Execution aborted".to_owned(),
                }
            } else if deadline_passed {
                let message = format!(
                    "Execution timed out after {} ms",
                    timeout.unwrap_or_default().as_millis()
                );
                ScriptErrorInfo {
                    kind: "timeout",
                    name: None,
                    message,
                }
            } else {
                ScriptErrorInfo {
                    kind: "sandbox",
                    name: None,
                    message: head.clone(),
                }
            };
            recover(&ctx, &runtime).await;
            Ok(Err(EvalFailure { head, info }))
        }
        Err(_elapsed) => Err(started.elapsed()),
    }
}

/// One script call: the shared guard rails, then the routing's responder.
#[allow(clippy::too_many_arguments)]
async fn call_script_tool(
    target: ScriptTarget,
    name: String,
    args_json: String,
    routing: ScriptRouting,
    calls: Arc<StdMutex<Vec<ScriptCallRecord>>>,
    signal: Option<AbortSignal>,
    progress: Option<Arc<RunProgress>>,
    allowed: Option<Arc<[String]>>,
    loop_handle: tokio::runtime::Handle,
) -> ToolCallReply {
    if name == "codemode" {
        return ToolCallReply::failed("scripts must not start other scripts".to_owned());
    }
    // Whitelist enforcement at the bridge (the gate behind the prelude's
    // Proxy): a name outside the list gets the standard tool-failure reply,
    // the same observable effect as the Proxy denial.
    if allowlist_rejects(allowed.as_deref(), &name) {
        return ToolCallReply::failed(allowlist_error_text(&name));
    }
    if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
        return ToolCallReply::failed("aborted".to_owned());
    }
    if let Some(progress) = &progress {
        progress
            .issued
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    match routing {
        ScriptRouting::Custom(responder) => responder.call(target, name, args_json).await,
        ScriptRouting::Session(executor) => {
            execute_via_session(executor, loop_handle, target, name, args_json, calls).await
        }
    }
}

/// Read the script's `store()` writes (`__pir_storeWritesJson`, installed by
/// the prelude only when the run was given a store snapshot).
async fn read_store_writes(ctx: &rquickjs::AsyncContext) -> JsonValue {
    let collected = ctx
        .async_with(async |js_ctx| {
            let reader: Option<rquickjs::Function> =
                js_ctx.globals().get("__pir_storeWritesJson").ok();
            match reader {
                Some(reader) => reader.call::<_, String>(()).ok(),
                None => None,
            }
        })
        .await;
    match collected {
        Some(json) => decode_store_writes(&json),
        None => serde_json::json!({ "set": {}, "delete": [] }),
    }
}

/// Decode the prelude's `[[key, json] | [key]]` write list into upstream's
/// `CodemodeStoreWrites` (`{set: {...}, delete: [...]}`).
fn decode_store_writes(json: &str) -> JsonValue {
    let mut set = serde_json::Map::new();
    let mut delete = Vec::new();
    if let Ok(entries) = serde_json::from_str::<Vec<JsonValue>>(json) {
        for entry in entries {
            let Some(entry) = entry.as_array() else {
                continue;
            };
            let Some(key) = entry.first().and_then(JsonValue::as_str) else {
                continue;
            };
            match entry.get(1) {
                Some(value) => {
                    if let Ok(parsed) =
                        serde_json::from_str::<JsonValue>(value.as_str().unwrap_or_default())
                    {
                        set.insert(key.to_owned(), parsed);
                    }
                }
                None => delete.push(JsonValue::String(key.to_owned())),
            }
        }
    }
    serde_json::json!({ "set": JsonValue::Object(set), "delete": delete })
}

// ---------------------------------------------------------------------------
// The extension-runtime sandbox route (`__pir_host.codemode*`)
//
// The `@earendil-works/pi-codemode` shim's engine-backed faces
// (`CodemodeSandbox`/`CodemodeSandboxBase.execute()`/`start()`) route here:
// the host owns the VM (the same thread sandbox the builtin tool uses), and
// the extension's JS side answers the script's calls through a pull protocol
// (upstream's `host ↔ VM` message exchange, with the extension actor in the
// VM's place):
//
//   JS                                    host
//   ── codemodeStart(payload) ──────────▶  spawn the sandbox run, return its id
//   ── codemodePoll(id) ────────────────▶  next event:
//        ◀── {type:"started", fingerprint}      (deterministic runs)
//        ◀── {type:"call", id, target, name, args}
//        ◀── {type:"idle", seq, issued}
//        ◀── {type:"done", result: {...CodemodeResult...}}
//   ── codemodeDeliver(id, callId, {...}) ─▶  resolve the await of that call
//   ── codemodeAbort(id) ────────────────▶  interrupt the VM, resolve pending calls
//
// Session ids are per-process and never reused; a session lives until its
// `done` event is polled (or it is aborted).
// ---------------------------------------------------------------------------

/// Which upstream face started a run: `execute()` (the sandbox's own tools,
/// with its default deadline) or `start()` (declared-only, caller-answered,
/// no deadline, deterministic clock/randomness).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRunMode {
    Execute,
    Deterministic,
}

impl SandboxRunMode {
    fn parse(mode: &str) -> Option<Self> {
        match mode {
            "execute" => Some(SandboxRunMode::Execute),
            "start" => Some(SandboxRunMode::Deterministic),
            _ => None,
        }
    }
}

/// Progress of one extension-run sandbox: `start()`'s `during` face and the
/// `onIdle` ordinals.
#[derive(Debug, Default)]
pub struct RunProgress {
    /// Results the JS side delivered, in delivery order (1-based).
    delivered: std::sync::atomic::AtomicU64,
    /// The ordinal of the last delivered result the VM was seen to consume
    /// (`__pir_ack` runs right after a call's result is applied); 0 before.
    consumed: std::sync::atomic::AtomicU64,
    /// Calls the VM issued (tools + globals).
    issued: std::sync::atomic::AtomicU64,
}

impl RunProgress {
    fn mark_delivered(&self) -> u64 {
        self.delivered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    fn mark_consumed(&self) {
        let delivered = self.delivered.load(std::sync::atomic::Ordering::SeqCst);
        self.consumed
            .store(delivered, std::sync::atomic::Ordering::SeqCst);
    }

    fn issued(&self) -> u64 {
        self.issued.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn consumed(&self) -> u64 {
        self.consumed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// One call awaiting the JS side's answer.
struct PendingCall {
    reply: tokio::sync::oneshot::Sender<ToolCallReply>,
    /// Index into the run's call records (tools only).
    record: Option<usize>,
}

/// Bound of the host→JS event queue of one sandbox run, in EVENTS.
///
/// Every queued event is one JSON line; a `call` line carries the call's
/// arguments, so 256 queued events is "a few hundred in-flight tool calls" —
/// more than a script's `Promise.all` wave normally produces, and the point
/// where a consumer that stopped polling must stop the producer instead of the
/// producer retaining the run's whole log. The call producer is the sandbox
/// thread and the consumer is the extension runtime thread (the JS
/// `codemodePoll` loop), so a full queue PARKS the producer: the sandbox's
/// tool call waits, exactly like a synchronous host call in Node, and the
/// queue never grows past the bound. `idle` notifications are coalesced first
/// (they supersede each other — the JS `onIdle(seq, issued)` only needs the
/// newest ordinals — so replacing an older one costs a `continue` and no
/// wait). Lifecycle events (`started`, `done`) bypass the bound: the JS loop
/// cannot settle a run without seeing `done`, and there is at most one of
/// each, so the bypass cannot grow the queue.
const MAX_SANDBOX_EVENTS: usize = 256;

/// How long a producer parks on a full event queue before giving up and
/// dropping the event (with a diagnostic). Only a consumer that stopped
/// draining can hit this; the run's own abort/timeout then settles the run.
const EVENT_ROOM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The host→JS event queue with its bound, coalescing and drop accounting.
#[derive(Default)]
struct EventQueue {
    queue: std::collections::VecDeque<String>,
    /// `idle` notifications replaced by a newer one (not a loss: they are
    /// superseding progress notices).
    coalesced: u64,
    /// Events dropped because a full queue never drained in time.
    dropped: u64,
}

impl EventQueue {
    /// Drop the oldest queued `idle` event, if any (see [`MAX_SANDBOX_EVENTS`]).
    fn coalesce_idle(&mut self) -> bool {
        if let Some(position) = self.queue.iter().position(|line| is_idle_event(line)) {
            self.queue.remove(position);
            self.coalesced = self.coalesced.saturating_add(1);
            return true;
        }
        false
    }

    fn push(&mut self, line: String) {
        self.queue.push_back(line);
    }
}

/// Wire test for an `idle` event without parsing the line.
fn is_idle_event(line: &str) -> bool {
    line.starts_with(r#"{"type":"idle""#)
}

/// Wire test for a lifecycle event (`started` / `done`), which bypasses the
/// queue bound (see [`MAX_SANDBOX_EVENTS`]).
fn is_lifecycle_event(line: &str) -> bool {
    line.starts_with(r#"{"type":"started""#) || line.starts_with(r#"{"type":"done""#)
}

/// Wire test for the terminal `done` event. Popping it is what retires a run
/// (see [`next_extension_event`]).
fn is_done_event(line: &str) -> bool {
    line.starts_with(r#"{"type":"done""#)
}

/// One extension sandbox run: the host↔JS message queue, the pending calls,
/// the run's abort flag and its progress counters.
struct SandboxSession {
    mode: SandboxRunMode,
    events: StdMutex<EventQueue>,
    /// Parks a producer whose event did not fit ([`MAX_SANDBOX_EVENTS`]);
    /// signalled by every consumer pop.
    events_space: std::sync::Condvar,
    notify: tokio::sync::Notify,
    pending: StdMutex<std::collections::HashMap<u64, PendingCall>>,
    next_call_id: std::sync::atomic::AtomicU64,
    calls: Arc<StdMutex<Vec<ScriptCallRecord>>>,
    progress: Arc<RunProgress>,
    abort: AbortSignal,
    finished: std::sync::atomic::AtomicBool,
    /// The JS side stopped answering (its promise chain died): further calls
    /// fail instead of hanging.
    detached: std::sync::atomic::AtomicBool,
}

impl SandboxSession {
    fn lock_events(&self) -> std::sync::MutexGuard<'_, EventQueue> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push_event(&self, event: JsonValue) {
        let line = event.to_string();
        // Lifecycle events (`started` / `done`) bypass the bound: at most one
        // of each exists, and the JS loop cannot settle without `done`.
        if is_lifecycle_event(&line) {
            self.lock_events().push(line);
            // `notify_one` (not `notify_waiters`): a permit is stored when the
            // single consumer is not parked yet, so a poll that starts after
            // the push drains immediately instead of waiting for the next
            // event (the consumer is the one JS `codemodePoll` loop).
            self.notify.notify_one();
            return;
        }
        let mut queue = self.lock_events();
        let deadline = std::time::Instant::now() + EVENT_ROOM_TIMEOUT;
        loop {
            if queue.queue.len() < MAX_SANDBOX_EVENTS || queue.coalesce_idle() {
                queue.push(line);
                drop(queue);
                self.notify.notify_one();
                return;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                let dropped = {
                    queue.dropped = queue.dropped.saturating_add(1);
                    queue.dropped
                };
                drop(queue);
                crate::ext_diag!(
                    "[ext] codemode run event queue full ({} events) and not drained for {:?}: dropping the event ({} dropped so far)",
                    MAX_SANDBOX_EVENTS,
                    EVENT_ROOM_TIMEOUT,
                    dropped
                );
                return;
            }
            let (next, _) = self
                .events_space
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue = next;
        }
    }

    /// Pop the next event, awaiting one that has not arrived yet. A pop
    /// releases queue space, waking a parked producer.
    async fn next_event(&self) -> String {
        loop {
            // Register the waiter BEFORE the queue check so a push between
            // the check and the await cannot be lost.
            let notified = self.notify.notified();
            if let Some(line) = self.lock_events().queue.pop_front() {
                // Release a producer parked on the full queue.
                self.events_space.notify_all();
                return line;
            }
            notified.await;
        }
    }

    /// Answer one pending call (`deliver`). `Ok` when the call was waiting.
    fn deliver(&self, call_id: u64, reply: ToolCallReply) -> Result<(), String> {
        if self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the run has already finished".to_owned());
        }
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&call_id);
        let Some(pending) = pending else {
            return Err(format!("Call {call_id} is not waiting for a result"));
        };
        if let Some(index) = pending.record {
            let mut records = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(record) = records.get_mut(index) {
                record.status = if reply.ok { "ok" } else { "error" }.to_owned();
                record.duration_ms = reply.duration_ms;
            }
        }
        let ordinal = self.progress.mark_delivered();
        let _ = ordinal;
        let _ = pending.reply.send(reply);
        Ok(())
    }

    /// The run reached its outcome: mark it finished and fail every call that
    /// is still waiting (upstream `finish()` aborts pending calls).
    fn finish(&self) {
        if self
            .finished
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let drained: Vec<PendingCall> = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.drain().map(|(_, call)| call).collect()
        };
        for call in drained {
            let _ = call.reply.send(ToolCallReply::failed("aborted".to_owned()));
        }
    }

    /// The VM consumed a delivered result (`__pir_ack`).
    fn acknowledge(&self) {
        self.progress.mark_consumed();
        if self.mode == SandboxRunMode::Deterministic {
            self.push_event(serde_json::json!({
                "type": "idle",
                "seq": self.progress.consumed(),
                "issued": self.progress.issued(),
            }));
        }
    }
}

/// The responder of one extension sandbox run: every call goes to the JS
/// side — declared tools/globals through the sandbox's own table, everything
/// else flagged as a session call, which the JS side answers by running the
/// host session's in-flight tool set through `__pir_host.codemodeSessionCall`
/// (the builtin codemode tool's model, reachable from an extension). The
/// session hop deliberately goes through the JS side: the nested-call
/// pipeline must run in the extension runtime's own task context (the
/// `ctx.executeTool` path), not on the sandbox thread, or its `tool_call`
/// event dispatch starves.
struct ExtensionResponder {
    session: Arc<SandboxSession>,
    declared_tools: Arc<std::collections::HashSet<String>>,
    declared_globals: Arc<std::collections::HashSet<String>>,
    /// Whether the host session's tool set was available when the run
    /// started: undeclared names then go to the JS side (which runs them
    /// through `__pir_host.codemodeSessionCall`, re-resolving the executor at
    /// call time) instead of failing with upstream's `Unknown tool`.
    session_tools: bool,
    deterministic: bool,
}

impl ScriptToolResponder for ExtensionResponder {
    fn call(
        &self,
        target: ScriptTarget,
        name: String,
        args_json: String,
    ) -> BoxFuture<'static, ToolCallReply> {
        let session = Arc::clone(&self.session);
        let declared = match target {
            ScriptTarget::Tool => Arc::clone(&self.declared_tools),
            ScriptTarget::Global => Arc::clone(&self.declared_globals),
        };
        let has_session_tools = self.session_tools;
        let deterministic = self.deterministic;
        Box::pin(async move {
            // A deterministic run has no executors at all: every call goes to
            // the caller (upstream `start()`), declared or not.
            if deterministic || declared.contains(&name) {
                return session.dispatch_call(target, name, args_json, false).await;
            }
            if !has_session_tools {
                // Upstream's rejection for a name the sandbox does not know.
                return ToolCallReply::failed(format!(
                    "Unknown {} \"{name}\"",
                    if target == ScriptTarget::Tool {
                        "tool"
                    } else {
                        "global"
                    }
                ));
            }
            session.dispatch_call(target, name, args_json, true).await
        })
    }
}

impl SandboxSession {
    /// Hand one call to the JS side and await its answer. `session` marks a
    /// call the sandbox did not declare: the JS side answers it by running the
    /// host session's tool set (`__pir_host.codemodeSessionCall`).
    async fn dispatch_call(
        &self,
        target: ScriptTarget,
        name: String,
        args_json: String,
        session: bool,
    ) -> ToolCallReply {
        if self.detached.load(std::sync::atomic::Ordering::SeqCst) {
            return ToolCallReply::failed("aborted".to_owned());
        }
        let id = self
            .next_call_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let record = if target == ScriptTarget::Tool {
            let mut records = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            records.push(ScriptCallRecord {
                name: name.clone(),
                status: "cancelled".to_owned(),
                duration_ms: 0,
            });
            Some(records.len() - 1)
        } else {
            None
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<ToolCallReply>();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, PendingCall { reply: tx, record });
        let args: JsonValue = serde_json::from_str(&args_json).unwrap_or(JsonValue::Null);
        self.push_event(serde_json::json!({
            "type": "call",
            "id": id,
            "target": target.as_str(),
            "name": name,
            "args": args,
            "session": session,
        }));
        // A dropped sender means the run finished (or was aborted) between
        // the call and its answer; the caller gets the standard failure.
        rx.await
            .unwrap_or_else(|_| ToolCallReply::failed("aborted".to_owned()))
    }
}

/// Run one call through the host session's nested-call executor on the
/// agent-loop runtime — the BUILTIN `codemode` tool's path, whose tool calls
/// execute on the agent loop (a multi-task runtime the sandbox thread can
/// spawn onto). The extension sandbox route does not use this: it answers
/// through the JS side ([`session_call_payload`]).
async fn execute_via_session(
    executor: Arc<NestedToolExecutor>,
    loop_handle: tokio::runtime::Handle,
    target: ScriptTarget,
    name: String,
    args_json: String,
    calls: Arc<StdMutex<Vec<ScriptCallRecord>>>,
) -> ToolCallReply {
    let started = Instant::now();
    let record_index = if target == ScriptTarget::Tool {
        let mut records = calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        records.push(ScriptCallRecord {
            name: name.clone(),
            status: "cancelled".to_owned(),
            duration_ms: 0,
        });
        Some(records.len() - 1)
    } else {
        None
    };
    let args: JsonValue = serde_json::from_str(&args_json).unwrap_or(JsonValue::Null);
    // Nested execution belongs to the agent-loop runtime (the shared
    // scheduler/queue live there); spawn onto it and await from here.
    let executed = loop_handle.spawn(async move { executor.execute(&name, args).await });
    match executed.await {
        Ok(outcome) => {
            if let Some(index) = record_index {
                let mut records = calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(record) = records.get_mut(index) {
                    record.status = if outcome.is_error { "error" } else { "ok" }.to_owned();
                    record.duration_ms = started.elapsed().as_millis() as u64;
                }
            }
            if outcome.is_error {
                ToolCallReply::failed(outcome.result.content)
            } else if outcome.result.structured_content.is_some() {
                let value =
                    serde_json::to_string(&outcome.result.structured_content.unwrap_or_default())
                        .unwrap_or_default();
                ToolCallReply::ok(value, true)
            } else {
                ToolCallReply::ok(outcome.result.content, false)
            }
        }
        Err(join_error) => ToolCallReply::failed(format!("tool execution failed: {join_error}")),
    }
}

/// Run one session-tier call and build the `codemodeDeliver` payload the JS
/// side answers with (`__pir_host.codemodeSessionCall`).
///
/// This is the builtin `codemode` tool's call path — the host session's
/// nested-call executor — awaited INSIDE the host function's own future: the
/// nested pipeline must run in the extension runtime's task context (the
/// `ctx.executeTool` path), so its `tool_call`/`tool_result` event dispatch can
/// interleave with the script's suspended await. Spawning it onto the runtime
/// from the sandbox thread starved that dispatch (a 30 s event timeout before
/// the call proceeded).
///
/// `Ok` values ride as their strict JSON text (the delivery contract): a text
/// result becomes a JSON string, a structured result its JSON object.
pub async fn session_call_payload(
    executor: Arc<NestedToolExecutor>,
    name: String,
    args_json: String,
) -> JsonValue {
    let started = Instant::now();
    let args: JsonValue = serde_json::from_str(&args_json).unwrap_or(JsonValue::Null);
    let outcome = executor.execute(&name, args).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    if outcome.is_error {
        return serde_json::json!({
            "ok": false,
            "message": outcome.result.content,
            "durationMs": duration_ms,
        });
    }
    let value = match outcome.result.structured_content {
        Some(structured) => serde_json::to_string(&structured).unwrap_or_default(),
        None => serde_json::to_string(&outcome.result.content).unwrap_or_default(),
    };
    serde_json::json!({
        "ok": true,
        "value": value,
        "durationMs": duration_ms,
    })
}

/// One declared tool of an extension sandbox run (upstream
/// `CodemodeTool`/`CodemodeDeclaredTool` reduced to what crosses the bridge:
/// execution happens JS side, so only the names and the description do).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredScriptTool {
    pub name: String,
    #[serde(default)]
    pub js_name: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// One declared global (upstream `CodemodeDeclaredGlobal`).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredScriptGlobal {
    pub name: String,
    #[serde(default)]
    pub spread: bool,
}

/// The payload of one `codemodeStart` host call (the JS shim's request).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRunRequest {
    pub code: String,
    /// `"execute"` or `"start"`.
    #[serde(default = "default_run_mode")]
    pub mode: String,
    /// `CodemodeExecuteOptions.timeoutMs` / `CodemodeRunOptions.timeoutMs`;
    /// `None` = the face's own default.
    #[serde(default)]
    pub timeout_ms: Option<f64>,
    #[serde(default)]
    pub tools: Vec<DeclaredScriptTool>,
    #[serde(default)]
    pub globals: Vec<DeclaredScriptGlobal>,
    /// `CodemodeExecuteOptions.store` (an object; JSON values).
    #[serde(default)]
    pub store: JsonValue,
    /// `CodemodeRunOptions.startedAt` (deterministic runs; required).
    #[serde(default)]
    pub started_at: Option<f64>,
    /// `CodemodeRunOptions.seed` (deterministic runs; required).
    #[serde(default)]
    pub seed: Option<f64>,
}

fn default_run_mode() -> String {
    "execute".to_owned()
}

/// The host↔JS session registry. Process-global (the host fns are `'static`
/// closures) and keyed by a monotonic id that is never reused.
fn extension_runs() -> &'static StdMutex<std::collections::HashMap<u64, Arc<SandboxSession>>> {
    static RUNS: std::sync::OnceLock<
        StdMutex<std::collections::HashMap<u64, Arc<SandboxSession>>>,
    > = std::sync::OnceLock::new();
    RUNS.get_or_init(|| StdMutex::new(std::collections::HashMap::new()))
}

fn next_extension_run_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

/// The VM fingerprint a deterministic run reports (`onStarted`): stable per
/// engine + protocol version. Upstream hashes the compiled wasm module; pir's
/// VM is the native QuickJS compiled into the host binary, so the fingerprint
/// changes with the sandbox protocol instead — a replay that stored the
/// fingerprint still detects a different VM.
pub fn extension_engine_fingerprint() -> String {
    format!(
        "pir-quickjs-native/{}",
        worker::CODEMODE_WORKER_PROTOCOL_VERSION
    )
}

/// Start one extension sandbox run (`CodemodeSandboxBase.execute()` /
/// `.start()`; DRIFT §2.3 item 2). `session_executor` is the host session's
/// in-flight nested executor (the fallback for tool names the sandbox did not
/// declare), or `None` when the runtime has none. It is only probed for
/// presence here (the run advertises the session-tools proxy; the calls
/// themselves resolve the executor per `codemodeSessionCall`), so an `Arc`
/// from the runtime registry is enough and no executor value is cloned.
///
/// Returns `{"id": N}` — the session id to poll — or `{"error": "..."}` for a
/// request the host refuses outright (a malformed payload, an invalid
/// `startedAt`/`seed`, duplicate tool/script names, a `RESERVED_GLOBALS`
/// shadow — upstream throws for all of these before a VM starts).
pub fn start_extension_run(
    request_json: &str,
    session_executor: Option<Arc<NestedToolExecutor>>,
) -> String {
    let request: ExtensionRunRequest = match serde_json::from_str(request_json) {
        Ok(request) => request,
        Err(error) => {
            return serde_json::json!({ "error": format!("invalid codemode run request: {error}") })
                .to_string()
        }
    };
    let Some(mode) = SandboxRunMode::parse(&request.mode) else {
        return serde_json::json!({
            "error": format!("unknown codemode sandbox mode \"{}\"", request.mode),
        })
        .to_string();
    };
    let deterministic = mode == SandboxRunMode::Deterministic;
    // The JS base class validates too (upstream throws synchronously there);
    // the host repeats the checks because the payload crosses a boundary a
    // script could not, and a bad run must fail closed.
    if deterministic {
        let started_at = request.started_at.unwrap_or(f64::NAN);
        if !started_at.is_finite() || started_at < 0.0 {
            return serde_json::json!({
                "error": "startedAt must be a non-negative number of milliseconds since the epoch",
            })
            .to_string();
        }
        let seed = request.seed.unwrap_or(f64::NAN);
        if !(seed.is_finite() && seed >= 0.0 && seed <= u32::MAX as f64 && seed.fract() == 0.0) {
            return serde_json::json!({ "error": "seed must be a 32-bit unsigned integer" })
                .to_string();
        }
    }
    let mut tool_names = std::collections::HashSet::new();
    let mut script_names = std::collections::HashSet::new();
    let mut declared_tools = std::collections::HashSet::new();
    for tool in &request.tools {
        if !tool_names.insert(tool.name.clone()) {
            return serde_json::json!({ "error": "Tool names must be unique" }).to_string();
        }
        let js_name = tool
            .js_name
            .clone()
            .unwrap_or_else(|| to_codemode_identifier(&tool.name));
        if !script_names.insert(js_name.clone()) {
            return serde_json::json!({ "error": "Tool script names must be unique" }).to_string();
        }
        declared_tools.insert(tool.name.clone());
        declared_tools.insert(js_name);
    }
    let mut declared_globals = std::collections::HashSet::new();
    for global in &request.globals {
        if !declared_globals.insert(global.name.clone()) {
            return serde_json::json!({
                "error": format!("Global \"{}\" is already registered", global.name),
            })
            .to_string();
        }
    }
    let store = match &request.store {
        JsonValue::Null => "{}".to_owned(),
        JsonValue::Object(_) => request.store.to_string(),
        other => {
            return serde_json::json!({
                "error": format!("store must be an object, got {}", json_kind(other)),
            })
            .to_string()
        }
    };
    let timeout = match request.timeout_ms {
        Some(ms) if ms.is_finite() && ms >= 0.0 => {
            Some(Duration::from_millis(ms.min(u32::MAX as f64) as u64))
        }
        Some(_) => {
            return serde_json::json!({ "error": "timeoutMs must be a non-negative number" })
                .to_string()
        }
        // Upstream: `execute()` uses the sandbox's `timeoutMs ?? 300000`;
        // `start()` defaults to no deadline at all.
        None if deterministic => None,
        None => Some(Duration::from_millis(CODEMODE_DEFAULT_TIMEOUT_MS)),
    };
    // The run task and the JS-side polls need a runtime to live on; the VMs
    // themselves run on the sandbox thread.
    if tokio::runtime::Handle::try_current().is_err() {
        return serde_json::json!({
            "error": "codemode must run inside a tokio runtime",
        })
        .to_string();
    }

    let session = Arc::new(SandboxSession {
        mode,
        events: StdMutex::new(EventQueue::default()),
        events_space: std::sync::Condvar::new(),
        notify: tokio::sync::Notify::new(),
        pending: StdMutex::new(std::collections::HashMap::new()),
        next_call_id: std::sync::atomic::AtomicU64::new(0),
        calls: Arc::default(),
        progress: Arc::default(),
        abort: AbortSignal::new(),
        finished: std::sync::atomic::AtomicBool::new(false),
        detached: std::sync::atomic::AtomicBool::new(false),
    });
    let id = next_extension_run_id();
    extension_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, Arc::clone(&session));

    let responder = Arc::new(ExtensionResponder {
        session: Arc::clone(&session),
        declared_tools: Arc::new(declared_tools),
        declared_globals: Arc::new(declared_globals),
        session_tools: session_executor.is_some(),
        deterministic,
    });
    // The extension sandbox route runs host-side without a tool-context cwd;
    // resolve its heap cap from the process-wide codemode config (settings of
    // the process working directory, env override included). The config is
    // read once and refreshed by the codemode plugin's `/reload` — not once
    // per run.
    let sandbox_memory_bytes = cached_codemode_config().sandbox_memory_bytes;
    let limits =
        SandboxLimits::for_extension_run(DEFAULT_INLINE_BUDGET, timeout, sandbox_memory_bytes);
    let code = request.code.clone();
    let extras = ExtensionRunExtras {
        declared_tools: serde_json::to_string(&request.tools).unwrap_or_else(|_| "[]".to_owned()),
        globals: serde_json::to_string(&request.globals).unwrap_or_else(|_| "[]".to_owned()),
        store,
        deterministic: if deterministic {
            Some((
                request.started_at.unwrap_or(0.0),
                request.seed.unwrap_or(0.0),
            ))
        } else {
            None
        },
        session: Arc::clone(&session),
    };
    let run_session = Arc::clone(&session);
    tokio::spawn(async move {
        if deterministic {
            run_session.push_event(serde_json::json!({
                "type": "started",
                "fingerprint": extension_engine_fingerprint(),
            }));
        }
        let (_builtin_outcome, report) = run_sandbox_report(
            &code,
            None,
            ScriptRouting::Custom(responder),
            Some(run_session.abort.clone()),
            limits,
            Some(Arc::clone(&run_session.progress)),
            Some(extras),
        )
        .await;
        let result = extension_result_json(&run_session, &report);
        // Answer the outcome before failing the still-pending calls: a script
        // that ended with calls in flight keeps their "cancelled" records
        // (upstream `finish()`).
        run_session.push_event(serde_json::json!({ "type": "done", "result": result }));
        run_session.finish();
        // Do NOT remove the session here: `done` was only queued, and the
        // registry removal must not race the single JS poll that still has to
        // read it. `next_extension_event` retires the run when that poll pops
        // the `done` event (the contract documented on the host fn).
    });

    serde_json::json!({ "id": id }).to_string()
}

/// Build the upstream `CodemodeResult` JSON from a finished run.
fn extension_result_json(session: &SandboxSession, run: &RunReport) -> JsonValue {
    let report = &run.outcome;
    let output: Vec<JsonValue> = run
        .items
        .iter()
        .map(|item| {
            let mut entry = serde_json::Map::new();
            entry.insert("type".to_owned(), JsonValue::String("text".to_owned()));
            entry.insert("text".to_owned(), JsonValue::String(item.text.clone()));
            if item.kind == "console" {
                entry.insert("console".to_owned(), JsonValue::Bool(true));
            }
            JsonValue::Object(entry)
        })
        .collect();
    let calls: Vec<JsonValue> = session
        .calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(ScriptCallRecord::to_json)
        .collect();
    let during = if session.mode == SandboxRunMode::Deterministic {
        Some(serde_json::json!({
            "seq": session.progress.consumed(),
            "issued": session.progress.issued(),
        }))
    } else {
        None
    };
    match &report.error {
        None => {
            let mut result = serde_json::json!({
                "ok": true,
                "output": output,
                "calls": calls,
                "storeWrites": report
                    .store_writes
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({ "set": {}, "delete": [] })),
            });
            if let Some(json) = &report.value_json {
                let value = serde_json::from_str::<JsonValue>(json).unwrap_or(JsonValue::Null);
                result["value"] = value;
            } else {
                // Upstream: `exit()` / a bare `return` completes with
                // `value: undefined`, which JSON cannot carry — the JS shim
                // reads the missing key as `undefined`.
                result["undefined"] = JsonValue::Bool(true);
            }
            if let Some(during) = during {
                result["during"] = during;
            }
            result
        }
        Some(error) => {
            let mut error_json = serde_json::json!({
                "kind": error.kind,
                "message": error.message,
            });
            if let Some(name) = &error.name {
                error_json["name"] = JsonValue::String(name.clone());
            }
            let mut result = serde_json::json!({
                "ok": false,
                "error": error_json,
                "output": output,
                "calls": calls,
            });
            if let Some(during) = during {
                result["during"] = during;
            }
            result
        }
    }
}

/// The next event of one extension sandbox run, as JSON. An unknown or
/// already-retired session answers `{"type":"error",...}` (the JS side settles
/// the promise with a `sandbox` failure).
///
/// A run is retired from the registry by the poll that drains its terminal
/// `done` event, not by the task that produced it. Retiring eagerly after the
/// push raced the consumer: the producer could push `done`, mark the run
/// finished and remove it in the same scheduling slice, so the consumer's
/// next poll found no session and reported "run N is not active" instead of
/// the outcome (the 300-call queue-bound fixture hit this on ~2/3 of runs).
pub async fn next_extension_event(run_id: u64) -> String {
    let session = extension_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&run_id)
        .cloned();
    let Some(session) = session else {
        return serde_json::json!({
            "type": "error",
            "message": format!("codemode run {run_id} is not active"),
        })
        .to_string();
    };
    let line = session.next_event().await;
    // `done` is the last event the producer ever queues (the run task has
    // finished before pushing it), so the pop is the last read and the
    // session can be dropped.
    if is_done_event(&line) {
        extension_runs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run_id);
    }
    line
}

/// Answer call `call_id` of run `run_id` (`CodemodeRun.deliver` /
/// `CodemodeTool.execute`'s resolution). The payload is
/// `{ok, value?, undefined?, message?, properties?, durationMs?, clock?}`.
/// Returns `{"ok":true}` or `{"error":"..."}`.
///
/// This is the text entry point (the JS prelude's `codemodeDeliver` passes the
/// payload as JSON text); a host-side caller already holding the parsed value
/// uses [`deliver_extension_result_value`] and skips the text round-trip.
pub fn deliver_extension_result(run_id: u64, call_id: u64, payload_json: &str) -> String {
    let session = extension_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&run_id)
        .cloned();
    let Some(session) = session else {
        return serde_json::json!({
            "error": format!("codemode run {run_id} is not active"),
        })
        .to_string();
    };
    let payload: JsonValue = match serde_json::from_str(payload_json) {
        Ok(payload) => payload,
        Err(error) => {
            return serde_json::json!({ "error": format!("invalid delivery payload: {error}") })
                .to_string()
        }
    };
    deliver_payload_to_session(&session, call_id, payload)
}

/// [`deliver_extension_result`] with the payload already parsed and owned.
/// The extension sandbox's session-call tier builds its payload as a
/// [`JsonValue`], so handing the value straight over avoids serializing it to
/// text only to parse it back (`to_string` + `from_str`), and the field
/// extraction below moves the strings out instead of re-copying them
/// (`to_owned` / `cloned`). The delivery semantics are identical: the reply is
/// the same `ToolCallReply` built from the same fields, with the same
/// defaults for a missing or mistyped one, and a non-object payload reads as
/// empty (every read misses, exactly as `Value::get` behaves on it).
pub fn deliver_extension_result_value(run_id: u64, call_id: u64, payload: JsonValue) -> String {
    let session = extension_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&run_id)
        .cloned();
    let Some(session) = session else {
        return serde_json::json!({
            "error": format!("codemode run {run_id} is not active"),
        })
        .to_string();
    };
    deliver_payload_to_session(&session, call_id, payload)
}

/// Build the [`ToolCallReply`] for one delivery and hand it to the waiting
/// call. `value` / `message` / `properties` are removed from the payload map
/// (moved, not borrowed); the two numeric reads are `Copy` and borrow.
fn deliver_payload_to_session(
    session: &SandboxSession,
    call_id: u64,
    payload: JsonValue,
) -> String {
    let mut payload = match payload {
        JsonValue::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    let ok = matches!(payload.remove("ok"), Some(JsonValue::Bool(true)));
    let mut reply = if ok {
        match payload.remove("value") {
            // JS-side tools round-trip through JSON (upstream's payload is
            // `JSON.stringify(value)`), so the value is structured text.
            Some(JsonValue::String(value)) => ToolCallReply::ok(value, true),
            _ => ToolCallReply::ok_undefined(),
        }
    } else {
        let message = match payload.remove("message") {
            Some(JsonValue::String(message)) => message,
            _ => "tool call failed".to_owned(),
        };
        let mut reply = ToolCallReply::failed(message);
        reply.properties = payload.remove("properties").filter(|p| !p.is_null());
        reply
    };
    reply.duration_ms = payload
        .get("durationMs")
        .and_then(JsonValue::as_f64)
        .unwrap_or(0.0)
        .max(0.0) as u64;
    reply.clock = payload.get("clock").and_then(JsonValue::as_f64);
    match session.deliver(call_id, reply) {
        Ok(()) => r#"{"ok":true}"#.to_owned(),
        Err(error) => serde_json::json!({ "error": error }).to_string(),
    }
}

/// Abort one extension sandbox run: the VM's interrupt handler stops the
/// script (upstream `close()` / an aborted signal), pending calls are failed
/// with the standard `aborted` error, and the session is dropped.
pub fn abort_extension_run(run_id: u64) -> String {
    let session = extension_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&run_id);
    let Some(session) = session else {
        return serde_json::json!({
            "error": format!("codemode run {run_id} is not active"),
        })
        .to_string();
    };
    session.abort.abort();
    session.finish();
    r#"{"ok":true}"#.to_owned()
}

/// The JS side is gone (its promise chain died): fail the run's pending calls
/// instead of leaving the sandbox thread waiting forever.
pub fn detach_extension_run(run_id: u64) -> String {
    let session = extension_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&run_id)
        .cloned();
    let Some(session) = session else {
        return serde_json::json!({
            "error": format!("codemode run {run_id} is not active"),
        })
        .to_string();
    };
    session
        .detached
        .store(true, std::sync::atomic::Ordering::SeqCst);
    session.abort.abort();
    session.finish();
    r#"{"ok":true}"#.to_owned()
}

/// Render a JSON value's kind for a validation message.
fn json_kind(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "a boolean",
        JsonValue::Number(_) => "a number",
        JsonValue::String(_) => "a string",
        JsonValue::Array(_) => "an array",
        JsonValue::Object(_) => "an object",
    }
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// The `codemode` tool (upstream `codemodeSchema` + the `tools` binding list).
pub struct CodemodeTool {
    /// The settings snapshot resolved ONCE by [`CodemodePlugin`] (construction
    /// or `/reload`) and handed over at registration. [`Self::run`] reads it
    /// directly, so a script execution performs no settings IO.
    config: CodemodeConfig,
    /// Whether the current loadout declared this tool. Upstream executes a
    /// model call only against the declared (active) tool set; pir's merged
    /// registry keeps inactive tools executable so `codemode`/`deferred`
    /// tools stay reachable from scripts, which means an inactive
    /// `model-only` tool has to refuse its own direct call. The flag is set by
    /// [`Self::prepare_loadout`], which the loadout builder calls exactly for
    /// the tools it declares (active set, not hidden), and is shared by the
    /// tool object registered for one plugin/session.
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for CodemodeTool {
    fn default() -> Self {
        // Directly constructed tools (unit tests, embedders that run the tool
        // outside a session loadout) are not gated; the plugin-created tool
        // starts inactive and the loadout activates it.
        Self {
            config: CodemodeConfig::default(),
            active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

impl CodemodeTool {
    /// Build the plugin's tool: inactive until a loadout declares it, with the
    /// config snapshot [`CodemodePlugin::register`] resolved.
    fn registered(config: CodemodeConfig, active: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { config, active }
    }

    /// Whether the most recent loadout declared this tool.
    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl AgentTool for CodemodeTool {
    fn name(&self) -> &'static str {
        "codemode"
    }

    fn description(&self) -> &'static str {
        "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.\n- `await tools.<name>({ ...args })` resolves to a tool's text output (or its structured output when it declares a schema) and rejects with an Error on failure. Nested calls run through the agent's tool pipeline (validation, hooks, permission checks).\n- `text(value)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script.\n- Optional `tools` argument: an array of tool names the script may call. Calls still running when the script ends are cancelled. Only the script's output reaches the model; nested results do not."
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Raw JavaScript source.",
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional allowlist of tool names the script may call. Omit to allow every registered tool except codemode itself.",
                },
            },
            "required": ["code"],
        }))
    }

    fn execution_mode(&self) -> crate::agent::tool_batch::ToolExecutionMode {
        // Scripts compose arbitrary tools; run exclusively like upstream's
        // exclusive nested-call queue.
        crate::agent::tool_batch::ToolExecutionMode::Sequential
    }

    fn exposure(&self) -> ToolExposure {
        // Upstream `tool.js:253`: `exposure: "model-only"`.
        ToolExposure::ModelOnly
    }

    fn default_active(&self) -> bool {
        // Upstream `index.js:27`: registered inactive; `--tools codemode` /
        // `defaultTools` / `setActiveTools` activate it explicitly.
        false
    }

    fn reaches_tools(&self, input: &JsonValue) -> Option<ToolReach> {
        // Upstream `reachesTools: (input) => getScriptReach(input.code)`:
        // owners that register tools late (MCP servers still connecting) wait
        // for the ones a script may reach before it runs.
        Some(script_reach(input.get("code").unwrap_or(&JsonValue::Null)))
    }

    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        // Called only for declared (active, non-hidden) tools: this is the
        // activation signal `run` gates on (see the struct field docs).
        self.active.store(true, std::sync::atomic::Ordering::SeqCst);
        if self.config.mode != crate::config::settings::CodemodeMode::Only {
            return None;
        }
        // Upstream `prepareCodemodeLoadout` (mode `"only"`): requests leave
        // out the declarations of active direct tools — they stay active and
        // callable from scripts. (The mode-`"on"` description rewrite is a
        // documented deviation; see the module doc.)
        let hidden = loadout
            .declared
            .iter()
            .filter(|tool| tool.name() != self.name())
            .filter(|tool| loadout.get_exposure(tool.name()) == ToolExposure::Direct)
            .filter(|tool| {
                loadout
                    .callable
                    .iter()
                    .any(|callable| callable.name() == tool.name())
            })
            .map(|tool| tool.name().to_owned())
            .collect::<Vec<_>>();
        if hidden.is_empty() {
            None
        } else {
            Some(ToolLoadoutChanges {
                descriptions: Default::default(),
                hidden_declarations: hidden,
            })
        }
    }

    fn run(&self, args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let args = args.clone();
        let mut ctx = ctx.clone();
        // Move the executor out of the cloned context: it is already shared
        // behind an `Arc` (the sandbox routing clones that `Arc` per script
        // call — a refcount bump), so the `take` is a move and the second
        // full executor clone `ctx.nested.clone()` would cost never happens.
        // `execute` reads the executor from its own parameter, never the (now
        // empty) context field.
        let nested = ctx.nested.take();
        let config = self.config;
        let active = self.is_active();
        Box::pin(async move { execute(&args, &ctx, nested, config, active).await })
    }
}

/// Tool execution entry point (upstream `executeCodemode`, reduced).
///
/// `nested` is the calling context's nested-call executor, already owned by
/// the caller (`CodemodeTool::run` takes it out of its cloned context) so the
/// script's session routing can hold the shared `Arc`. `config` is the
/// plugin's cached settings snapshot (never a disk read here); `active` is
/// whether the current loadout declared the tool.
async fn execute(
    args: &JsonValue,
    ctx: &ToolContext,
    nested: Option<Arc<NestedToolExecutor>>,
    config: CodemodeConfig,
    active: bool,
) -> ToolResult {
    // Upstream executes a model tool call against the declared tool set only;
    // an inactive `model-only` tool is unknown to it. The merged pir registry
    // keeps inactive tools executable, so the tool refuses its own direct
    // call here (the same error the scheduler returns for an unknown name).
    if !active {
        return ToolResult::error(format!("Tool {CODEMODE_TOOL_NAME} not found"));
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        code: String,
        #[serde(default)]
        tools: Option<Vec<String>>,
    }
    // Deserialize straight from the borrowed `Value` (`&Value` implements
    // `serde::Deserializer`): the old `from_value(args.clone())` copied the
    // whole arguments object — including the `code` script, the largest
    // string a codemode call carries — only to parse it.
    let params: Params = match <Params as serde::Deserialize>::deserialize(args) {
        Ok(params) => params,
        Err(error) => return ToolResult::error(format!("Invalid codemode parameters: {error}")),
    };

    let started = Instant::now();
    // Split the optional `// @options:` preamble (upstream
    // `parseCodemodeSource`; a failure fails the tool with the upstream
    // `CodemodeSourceError` text).
    let (code, source_options) = match parse_codemode_source(&params.code) {
        Ok(parsed) => parsed,
        Err(error) => return ToolResult::error(error),
    };

    let Some(executor) = nested else {
        return ToolResult::error(
            "codemode requires a running tool-call context with nested tool execution",
        );
    };
    if ctx.is_aborted() {
        return ToolResult::error("Request was aborted before the script could run");
    }

    let inline_budget = source_options
        .max_output_tokens
        .map(|tokens| tokens.min(u32::MAX as u64) as u32)
        .unwrap_or(config.inline_budget);
    let timeout_override = source_options.timeout_ms.map(Duration::from_millis);
    let limits =
        SandboxLimits::resolve(inline_budget, timeout_override, config.sandbox_memory_bytes);
    let outcome = run_sandbox(
        &code,
        params.tools.as_deref(),
        ScriptRouting::Session(executor),
        ctx.signal.clone(),
        limits,
    )
    .await;

    // Upstream result header (execute.ts): the completed/failed line, the
    // wall time, then the output items.
    let header = format!(
        "Script {}\nWall time {:.1} seconds\nOutput:",
        if outcome.ok { "completed" } else { "failed" },
        started.elapsed().as_secs_f32()
    );
    let mut content = if outcome.output.is_empty() {
        header
    } else {
        format!("{header}\n{}", outcome.output)
    };
    if !outcome.ok {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("Script error:\n");
        content.push_str(
            &outcome
                .error_head
                .unwrap_or_else(|| "Error: unknown script error".to_owned()),
        );
    }
    if outcome.ok {
        ToolResult::ok(content)
    } else {
        ToolResult::error(content)
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// The `codemode` builtin plugin: registers the [`CodemodeTool`] on the
/// unified plugin registry (upstream `createCodemodeExtension()`).
pub struct CodemodePlugin {
    info: PluginInfo,
    /// The settings snapshot shared with the registered tools. `register`
    /// reads it and hands a copy to [`CodemodeTool`], whose executions never
    /// touch disk; the reload hook replaces the value.
    config: std::sync::RwLock<CodemodeConfig>,
    /// `with_config` pins the config: `/reload` keeps it instead of re-reading
    /// settings (the embedder owns the settings lifecycle). The extension
    /// route's process-wide cache is still refreshed.
    pinned: bool,
    /// Activation signal shared with every [`CodemodeTool`] this plugin
    /// registers: set when a loadout declares the tool (see the tool's field
    /// docs), so an inactive codemode refuses a direct call.
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl CodemodePlugin {
    fn builtin_info() -> PluginInfo {
        let mut info = PluginInfo::builtin("codemode");
        info.replaceable =
            super::builtin_extension_def("codemode").is_some_and(|def| def.replaceable);
        info
    }

    /// The builtin factory path. The factory has no session cwd (sessions are
    /// built after the plugin plan is assembled), so the settings are resolved
    /// ONCE from the PROCESS working directory here; the CLI assembles
    /// sessions from that cwd. `/reload` re-reads them.
    fn new() -> Self {
        Self {
            info: Self::builtin_info(),
            config: std::sync::RwLock::new(counted_load_codemode_config(&process_cwd())),
            pinned: false,
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Build the plugin from already-resolved settings (upstream `readMode`):
    /// `codemode.mode` then controls the loadout presentation through
    /// [`CodemodeTool::prepare_loadout`]. No settings IO happens at
    /// construction, and `/reload` keeps this explicit config (only the
    /// extension route's process-wide cache is refreshed).
    pub fn with_config(config: CodemodeConfig) -> Self {
        Self {
            info: Self::builtin_info(),
            config: std::sync::RwLock::new(config),
            pinned: true,
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn current_config(&self) -> CodemodeConfig {
        *self
            .config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl AgentPlugin for CodemodePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(CodemodeTool::registered(
            self.current_config(),
            Arc::clone(&self.active),
        )));
        // The `models` namespace bridges: codemode-exposure tools (callable
        // from scripts, never declared to the model) resolving the sandbox's
        // models.generateImages()/models.classify() calls against the
        // embedded catalog (upstream createModelGlobals).
        for bridge in models_bridge::ModelBridgeTool::pair() {
            ctx.register_tool(Arc::new(bridge));
        }
        Ok(())
    }

    /// `/reload` parity: re-read the codemode settings so a changed
    /// `codemode.mode`/`inlineBudget`/`sandboxMemoryMb` applies without a
    /// restart. An explicit [`CodemodePlugin::with_config`] snapshot is kept;
    /// the extension sandbox route's process-wide cache is refreshed either
    /// way.
    fn on_reload(&self) -> super::plugin::PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if self.pinned {
                let _ = reload_cached_codemode_config();
            } else {
                let config = counted_load_codemode_config(&process_cwd());
                *self
                    .config
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
                // One read serves both the plugin and the extension route.
                store_cached_codemode_config(config);
            }
            Ok(())
        })
    }
}

/// Factory for [`super::builtin::BUILTIN_EXTENSION_FACTORIES`].
pub fn new_codemode_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(CodemodePlugin::new())
}
