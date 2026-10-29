use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use rquickjs::prelude::{Async as JsAsync, Func};
use serde_json::Value as JsonValue;

use super::exec::{run_exec, ExecAbort};
use super::{lock_state, CommandDef, FlagDef, Notification, RuntimeState};

// ---------------------------------------------------------------------------
// Host function table
// ---------------------------------------------------------------------------

/// Resolve the agent directory: the single source of truth
/// `crate::ai::providers::agent_dir()` (`$PIR_AGENT_DIR` →
/// `$PIR_CODING_AGENT_DIR` → `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`), so
/// `getAgentDir()` from extension context matches session/theme/config paths.
fn pir_agent_dir() -> PathBuf {
    crate::ai::providers::agent_dir()
}

/// Normalize a host mode label to the upstream extension mode set: exactly
/// `"tui" | "rpc" | "json" | "print"` (pir hosts say `interactive` for the
/// TUI; everything unknown reports as `print`).
fn normalize_mode(mode: &str) -> &'static str {
    match mode {
        "interactive" | "tui" => "tui",
        "rpc" => "rpc",
        "json" => "json",
        _ => "print",
    }
}

fn context_json(state: &RuntimeState) -> String {
    let cfg = &state.config;
    let session = &state.session;
    serde_json::json!({
        "cwd": cfg.cwd.display().to_string(),
        // Normalized upstream mode: exactly "tui" | "rpc" | "json" | "print".
        "mode": normalize_mode(&cfg.mode),
        "hasUI": cfg.has_ui,
        // Full current Model object when the host pushed one; falls back to
        // the configured model id string.
        "model": if session.model.is_null() {
            cfg.model.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null)
        } else {
            session.model.clone()
        },
        "thinkingLevel": state.thinking_level,
        "scopedModels": state.scoped_models,
        "sessionId": cfg.session_id,
    })
    .to_string()
}

pub(super) fn register_host_functions(
    ctx: &rquickjs::Ctx<'_>,
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    // This runtime's own generation key (see `state::FdTable`): stamped onto
    // every descriptor this runtime opens, so a `/reload` successor's
    // teardown can never close them.
    generation: u64,
) -> rquickjs::Result<()> {
    use rquickjs::Object;

    let globals = ctx.globals();
    let host = Object::new(ctx.clone())?;
    globals.set("__pir_host", host.clone())?;

    register_session_host_fns(&host, state)?;
    register_context_host_fns(&host, state)?;
    // Host-backed SDK `SessionManager` ops (any session file; see
    // `agent_session::ext_session_ops`). Runtime-state-independent.
    crate::agent_session::ext_session_ops::register_ext_session_ops(&host)?;
    register_tool_exec_host_fns(&host, state)?;
    // Node-surface host ops (fs/os/crypto/process) share the loader's
    // allow-list so extension file access stays inside its own roots.
    register_node_host_fns(&host, state, allowed_roots, generation)?;
    // child_process + zlib ride a second generic entry (`__pir_host.nodeExt`):
    // the primary `node` dispatcher is the fs/os/crypto table's own, and the
    // JS bridge routes those two module names here. Same envelope, same
    // sandbox roots.
    register_child_host_fns(&host, state, allowed_roots, generation)?;
    // Web `fetch` rides a third generic entry (`__pir_host.http`): same JSON
    // envelope, sandbox-independent (network needs no fs roots; see the
    // permission note in `http_fns`).
    register_http_host_fns(&host, generation)?;
    // worker_threads rides a fourth generic entry (`__pir_host.worker` +
    // async `workerRecvPoll`): one OS thread + one QuickJS runtime per
    // Worker, mailboxes + pumps for the message bridge (see `worker_fns`).
    register_worker_host_fns(&host, state, allowed_roots, generation)?;
    // `fs.watch` rides a fifth generic entry (`__pir_host.fsWatch`): portable
    // snapshot-diff watcher threads (the TUI git-branch watcher's pattern),
    // JS drains events on a timer pump (see `fs_watch_fns`).
    register_fs_watch_host_fns(&host, state, generation)?;
    // The pi-codemode shim's sandbox bridge (`__pir_host.codemode*`): the
    // extension `CodemodeSandbox`/`CodemodeSandboxBase` route into the host
    // codemode executor (DRIFT §2.3 item 2 / D28); see `codemode_fns`.
    register_codemode_host_fns(&host, state)?;
    // Buffer/TextCodec/Intl hot paths ride plain globals (`__pirInstallBufferOps`
    // + `__pirBufferCodec` + `__pirInstallTextCodec` + `__pirIntlFormat`) rather
    // than the `__pir_host` table: they are installed onto JS-created prototypes
    // (`Buffer.prototype`, `TextEncoder.prototype`) which cannot cross the JSON
    // envelope — the §6.5 host-first policy's direct C-ABI tier (see `buffer_ops`).
    register_buffer_ops(ctx)?;

    // Script-mode bridges (`pir js`, see `main_cli/js_mode.rs`) as well as the
    // backing store of the JS `process.stdout`/`process.stderr` streams the
    // process stub installs in every host
    // (`api/js/process_global_stub.js.txt`): real process stdio writes and a
    // hard process exit. Where a stdio write goes depends on the host — an
    // interactive TUI session routes both streams into the tracing log instead
    // of the frame it owns (see `super::ext_stdio`). The decision is fixed at
    // construction; a runtime's `mode`/`has_ui` never change.
    let stdio_route = {
        let guard = lock_state(state);
        super::ext_stdio::ext_stdio_route(&guard.config.mode, guard.config.has_ui)
    };
    host.set(
        "writeStdout",
        Func::from(move |text: String| -> () {
            super::ext_stdio::write_extension_stdio(
                stdio_route,
                super::ext_stdio::ExtStream::Stdout,
                &text,
            );
        }),
    )?;
    host.set(
        "writeStderr",
        Func::from(move |text: String| -> () {
            super::ext_stdio::write_extension_stdio(
                stdio_route,
                super::ext_stdio::ExtStream::Stderr,
                &text,
            );
        }),
    )?;
    // Node `process.exit` parity: the JS boot snippet has already emitted the
    // `exit` event; this call never returns. Truncation to `i32` happens JS
    // side (`Math.trunc`), so the number conversion cannot fail.
    host.set(
        "exitProcess",
        Func::from(|code: i32| -> () {
            std::process::exit(code);
        }),
    )?;

    Ok(())
}

/// Release every process-wide host resource owned by one runtime generation
/// (children, workers, fetches, HTTP servers, net handles, fs watchers).
/// Called from [`crate::extensions::runtime::ExtensionRuntime`]'s teardown and
/// before the actor handles `Shutdown`, so a `/reload` cannot leak the previous
/// generation's live processes/threads/listeners/ports into the successor's
/// lifetime.
pub(super) fn release_host_generation(generation: u64) {
    worker_fns::release_generation(generation);
    child_fns::release_generation(generation);
    http_fns::release_generation(generation);
    // `fs.watch` threads only self-reap when their weak `RuntimeState` owner is
    // gone, and a reload REUSES that state: without this call the previous
    // generation's OS watcher + thread would outlive its runtime (a reload is
    // not a process exit). Generation-scoped like the tables above.
    fs_watch_fns::release_generation(generation);
    sqlite_fns::release_generation(generation);
}

/// Live host-resource counts, for the reload/shutdown release regression.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostResourceCounts {
    pub children: usize,
    pub workers: usize,
    pub fetches: usize,
    pub servers: usize,
    pub request_bodies: usize,
    pub net_servers: usize,
    pub net_sockets: usize,
}

impl HostResourceCounts {
    /// Every table empty (the state a completed release leaves behind).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Snapshot of the process-wide host tables. Tests capture a baseline before
/// starting their own runtime and compare after the release, so an unrelated
/// live handle can never make the assertion flaky.
#[doc(hidden)]
pub fn host_resource_counts_for_tests() -> HostResourceCounts {
    let (fetches, servers, request_bodies, net_servers, net_sockets) =
        http_fns::http_resource_counts_for_tests();
    HostResourceCounts {
        children: child_fns::child_count_for_tests(),
        workers: worker_fns::worker_count_for_tests(),
        fetches,
        servers,
        request_bodies,
        net_servers,
        net_sockets,
    }
}

/// Live `fs.watch` handle count (the reload-release regression; a watcher's
/// thread is not part of [`HostResourceCounts`] because its entry is removed
/// synchronously by the release while the detached thread exits at its next
/// poll slice).
#[doc(hidden)]
pub fn fs_watch_count_for_tests() -> usize {
    fs_watch_fns::watch_count_for_tests()
}

use self::buffer_ops::register_buffer_ops;
use self::child_fns::register_child_host_fns;
use self::codemode_fns::register_codemode_host_fns;
use self::context_fns::register_context_host_fns;
use self::fs_watch_fns::register_fs_watch_host_fns;
use self::http_fns::register_http_host_fns;
use self::node_fns::register_node_host_fns;
use self::session_fns::register_session_host_fns;
use self::tool_exec_fns::register_tool_exec_host_fns;
use self::worker_fns::register_worker_host_fns;

mod buffer_ops;
pub(crate) mod child_fns;
mod codemode_fns;
mod context_fns;
mod fs_watch_fns;
mod http_fns;
mod node_fns;
mod session_fns;
// Real SQLite backend (`node:sqlite` DatabaseSync + better-sqlite3/sqlite3
// virtual package shims).
mod sqlite_fns;
// EksBlowfish backend behind the `bcrypt` virtual package shim.
mod bcrypt_fns;
mod tool_exec_fns;
// `pub(crate)` so `runtime` can re-export the worker's JS stack request
// (`WORKER_JS_STACK_BYTES`) for the engine-bound test; nothing else escapes.
pub(crate) mod worker_fns;
