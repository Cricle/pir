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

/// Build the extension context snapshot as a JSON value. The string form used
/// by the legacy `__pir_host.getContextJson` is produced by `to_string()` at
/// the call site; the native tier passes this tree straight into QuickJS.
fn context_json(state: &RuntimeState) -> JsonValue {
    let cfg = &state.config;
    let session = &state.session;
    serde_json::json!({
        "cwd": cfg.cwd.display().to_string(),
        // Normalized upstream mode: exactly "tui" | "rpc" | "json" | "print".
        "mode": normalize_mode(&cfg.mode),
        // A bootstrap fail-fast runtime has no usable presentation sink, so
        // expose the same "no UI" answer the headless hosts do.
        "hasUI": cfg.has_ui && !state.ui_fail_fast,
        // Full current Model object when the host pushed one; falls back to
        // the configured model id string.
        "model": if session.model.is_null() {
            cfg.model.clone().map(JsonValue::String).unwrap_or(JsonValue::Null)
        } else {
            session.model.clone()
        },
        "thinkingLevel": state.thinking_level,
        "scopedModels": state.scoped_models,
        "sessionId": cfg.session_id,
    })
}

/// State carried in `Ctx` userdata so named (HRTB) native functions can reach
/// the runtime state without a closure capturing it. A closure that returns
/// `Value<'js>` fails `Func::from`'s invariance check (see `buffer_ops.rs:92`);
/// a named generic fn item is higher-ranked over `'js` and works.
struct NativeFnState(Arc<StdMutex<RuntimeState>>);

// SAFETY: NativeFnState contains only an Arc to Rust-owned mutex state and no
// JavaScript lifetime-bearing values; it is safe to treat it as 'static.
#[allow(unsafe_code)]
unsafe impl<'js> rquickjs::JsLifetime<'js> for NativeFnState {
    type Changed<'to> = NativeFnState;
}

fn native_state_error() -> rquickjs::Error {
    rquickjs::Error::new_into_js("native state", "object")
}

fn get_context_native<'js>(ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
    let value = {
        let guard = ctx
            .userdata::<NativeFnState>()
            .ok_or_else(native_state_error)?;
        let json = context_json(&lock_state(&guard.0));
        json
    };
    super::native_bridge::json_to_js(&ctx, &value)
}

/// Snapshot getters that the legacy tier exposes as JSON strings. The native
/// tier returns the same tree as a live object, so the prelude stops paying a
/// `JSON.parse` for each of these reads (notably `getAllToolsJson`, whose parse
/// happens on every `ctx.tools` first access, and usage/model snapshots).
fn snapshot_native<'js>(
    ctx: rquickjs::Ctx<'js>,
    which: SnapshotKind,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    let value = {
        let guard = ctx
            .userdata::<NativeFnState>()
            .ok_or_else(native_state_error)?;
        let s = lock_state(&guard.0);
        match which {
            SnapshotKind::Models => s.model_registry.clone(),
            SnapshotKind::ScopedModels => s.scoped_models.clone(),
            SnapshotKind::Usage => {
                let session = &s.session;
                let usage = match (session.context_tokens, session.context_window) {
                    (Some(tokens), Some(window)) if window > 0 => serde_json::json!({
                        "tokens": tokens,
                        "contextWindow": window,
                        "percent": (tokens as f64 / window as f64) * 100.0,
                    }),
                    (None, Some(window)) => serde_json::json!({
                        "tokens": null,
                        "contextWindow": window,
                        "percent": null,
                    }),
                    _ => JsonValue::Null,
                };
                serde_json::json!({ "usage": usage })
            }
        }
    };
    super::native_bridge::json_to_js(&ctx, &value)
}

#[derive(Clone, Copy)]
pub(super) enum SnapshotKind {
    Models,
    ScopedModels,
    Usage,
}

macro_rules! snapshot_fn {
    ($name:ident, $kind:expr) => {
        fn $name<'js>(ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
            snapshot_native(ctx, $kind)
        }
    };
}
snapshot_fn!(get_models_native, SnapshotKind::Models);
snapshot_fn!(get_scoped_models_native, SnapshotKind::ScopedModels);
snapshot_fn!(get_context_usage_native, SnapshotKind::Usage);

fn get_all_tools_native<'js>(ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
    let value = {
        let guard = ctx
            .userdata::<NativeFnState>()
            .ok_or_else(native_state_error)?;
        let mut s = lock_state(&guard.0);
        // Same fingerprint/memo as the string tier, but cached as a JsonValue
        // so the native path never materializes a string or re-parses.
        let fingerprint = session_fns::all_tools_fingerprint(&s);
        if let Some(fp) = fingerprint {
            if let Some((cached, json)) = &s.all_tools_value {
                if *cached == fp {
                    return super::native_bridge::json_to_js(&ctx, json);
                }
            }
        }
        let value = session_fns::build_all_tools_value(&s);
        if let Some(fp) = fingerprint {
            s.all_tools_value = Some((fp, value.clone()));
        }
        value
    };
    super::native_bridge::json_to_js(&ctx, &value)
}

fn get_commands_native<'js>(ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
    let value = {
        let guard = ctx
            .userdata::<NativeFnState>()
            .ok_or_else(native_state_error)?;
        let s = lock_state(&guard.0);
        let defs: Vec<&CommandDef> = s
            .builtin_commands
            .iter()
            .chain(s.command_defs.iter())
            .collect();
        serde_json::to_value(&defs).unwrap_or(JsonValue::Null)
    };
    super::native_bridge::json_to_js(&ctx, &value)
}

fn get_active_tools_native<'js>(ctx: rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
    let value = {
        let guard = ctx
            .userdata::<NativeFnState>()
            .ok_or_else(native_state_error)?;
        let s = lock_state(&guard.0);
        session_fns::active_tools_value(&s)
    };
    super::native_bridge::json_to_js(&ctx, &value)
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

    // The JSON-free tier. Functions here take/return live rquickjs values via
    // `native_bridge`; the prelude prefers them and falls back to `__pir_host`
    // string calls when absent. Start with the hottest read (the per-dispatch
    // context snapshot); later phases add the dispatch and host-call surfaces.
    //
    // Native functions are named generic fn items (not capturing closures)
    // because a `Value<'js>` return makes the closure invariant over `'js`;
    // rquickjs's `Func::from` only accepts higher-ranked fn items for that
    // signature (see `buffer_ops.rs`'s install macros). Runtime state is
    // reached through `Ctx::userdata`.
    ctx.store_userdata(NativeFnState(state.clone()))
        .map_err(|_| native_state_error())?;
    {
        let native = Object::new(ctx.clone())?;
        globals.set(super::native_bridge::NATIVE_GLOBAL, native.clone())?;
        native.set(
            "getContext",
            rquickjs::prelude::Func::from(get_context_native),
        )?;
        native.set(
            "getModels",
            rquickjs::prelude::Func::from(get_models_native),
        )?;
        native.set(
            "getScopedModels",
            rquickjs::prelude::Func::from(get_scoped_models_native),
        )?;
        native.set(
            "getContextUsage",
            rquickjs::prelude::Func::from(get_context_usage_native),
        )?;
        native.set(
            "getAllTools",
            rquickjs::prelude::Func::from(get_all_tools_native),
        )?;
        native.set(
            "getCommands",
            rquickjs::prelude::Func::from(get_commands_native),
        )?;
        native.set(
            "getActiveTools",
            rquickjs::prelude::Func::from(get_active_tools_native),
        )?;
    }

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
    // Script-mode `process.exit` is the only legitimate caller of the hard
    // process exit bridge. Extension and worker runtimes intentionally expose
    // a throwing `process.exit` shim instead; publishing this raw host method
    // in those runtimes would let extension code bypass that safety boundary.
    let script_mode = {
        let guard = lock_state(state);
        guard.config.allow_process_exit
    };
    if script_mode {
        host.set(
            "exitProcess",
            Func::from(|code: i32| -> () {
                std::process::exit(code);
            }),
        )?;
    }

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
///
/// NOTE: this counts EVERY generation in the process, so a baseline-and-compare
/// also sees a sibling runtime's handles in a merged test binary. The
/// reload-release regression uses [`host_resource_counts_for_generation`].
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

/// [`host_resource_counts_for_tests`] restricted to ONE runtime generation.
///
/// Every host table entry carries its owning generation and
/// `release_host_generation` removes exactly those, so this is the honest form
/// of the reload/shutdown release assertion: a concurrent runtime elsewhere in
/// the same test binary holding its own fetch/server can no longer fail it
/// (observed as a stray `fetches: 1` in `zz_merged_3`).
#[doc(hidden)]
pub fn host_resource_counts_for_generation(generation: u64) -> HostResourceCounts {
    let (fetches, servers, request_bodies, net_servers, net_sockets) =
        http_fns::http_resource_counts_for_generation(generation);
    HostResourceCounts {
        children: child_fns::child_count_for_generation(generation),
        workers: worker_fns::worker_count_for_generation(generation),
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
pub mod child_fns;
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
pub mod worker_fns;
