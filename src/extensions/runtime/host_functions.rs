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

// Holds no JS values (only an `Arc` to Rust state), so it is a 'static
// JsLifetime type — see rquickjs's `js_lifetime` docs, "static type" example.
#[allow(unsafe_code)]
unsafe impl<'js> rquickjs::JsLifetime<'js> for NativeFnState {
    type Changed<'to> = NativeFnState;
}

fn native_state_error() -> rquickjs::Error {
    rquickjs::Error::new_into_js("native state", "object")
}

/// The structured Node runtime, stored as a separate userdata type so it stays
/// independent of the session-state-backed native getters above. rquickjs keys
/// userdata by concrete type, so both can coexist on one context.
struct NodeState(Arc<crate::pir_node::NodeRuntime>);

#[allow(unsafe_code)]
unsafe impl<'js> rquickjs::JsLifetime<'js> for NodeState {
    type Changed<'to> = NodeState;
}

/// Universal C-ABI entry for Node ops: live JS `payload` object in, live result
/// object out. No `JSON.stringify`/`JSON.parse` at the boundary. The module
/// dispatch lives in `pir-node`; returned objects have the same `{ok,value}` /
/// `{ok:false,code,...}` shape the legacy JSON helper already understands.
fn node_invoke_native<'js>(
    ctx: rquickjs::Ctx<'js>,
    module: String,
    op: String,
    payload: rquickjs::Value<'js>,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    let json = super::native_bridge::js_to_json(&payload)?;
    let result = {
        let guard = ctx.userdata::<NodeState>().ok_or_else(native_state_error)?;
        match guard.0.invoke(&module, &op, &json) {
            Ok(value) => crate::pir_node::NodeError::ok(value),
            Err(error) => error.to_json_value(),
        }
    };
    super::native_bridge::json_to_js(&ctx, &result)
}

/// pir-side adapter supplying the small host facts `pir-node` ops consult.
struct PirNodeContext {
    state: Arc<StdMutex<RuntimeState>>,
    allowed_roots: Arc<StdMutex<Vec<PathBuf>>>,
    generation: u64,
}

impl crate::pir_node::NodeContext for PirNodeContext {
    fn cwd(&self) -> PathBuf {
        lock_state(&self.state).config.cwd.clone()
    }
    fn allowed_roots(&self) -> Vec<PathBuf> {
        self.allowed_roots.lock().map(|g| g.clone()).unwrap_or_default()
    }
    fn is_aborted(&self) -> bool {
        lock_state(&self.state).tool_call_aborted
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn record_host_op(&self, label: &'static str, elapsed: std::time::Duration) {
        super::host_op_stats::record(label, elapsed);
    }
}

/// Host hooks for the parts of the Node surface pir owns end to end: the
/// worker engine boot (a QuickJS runtime per worker), the npm resolver /
/// source transpiler, and the host's agent-root inheritance. Every Node
/// builtin implementation itself lives in `pir-node`.
struct PirNodeHooks {
    state: Arc<StdMutex<RuntimeState>>,
}

impl crate::pir_node::NodeHooks for PirNodeHooks {
    fn spawn_worker(
        &self,
        launch: crate::pir_node::modules::worker::WorkerLaunch,
        handle: crate::pir_node::modules::worker::WorkerHandle,
    ) -> Result<(), crate::pir_node::NodeError> {
        // The transport (registry/mailboxes/op table) lives in pir-node; the
        // host supplies the engine boot: host identity for stdio routing and
        // the parent's resolved limits (the worker is a second QuickJS
        // runtime in the same process).
        let (host_mode, has_ui, limits) = {
            let guard = lock_state(&self.state);
            (
                guard.config.mode.clone(),
                guard.config.has_ui,
                guard.config.limits.clone(),
            )
        };
        let spec = worker_fns::WorkerSpec {
            id: launch.id,
            code: launch.code,
            from_dir: launch.from_dir,
            worker_data: launch.worker_data,
            filename: launch.filename,
            cwd: launch.cwd,
            roots: launch.roots,
            host_mode,
            has_ui,
            limits,
        };
        std::thread::Builder::new()
            .name(format!("pir-worker-{}", spec.id))
            .stack_size(worker_fns::WORKER_THREAD_STACK)
            .spawn(move || worker_fns::worker_thread_main(spec, handle))
            .map(|_| ())
            .map_err(|error| {
                crate::pir_node::NodeError::new("EIO", format!("failed to spawn worker thread: {error}"))
            })
    }

    fn worker_roots(&self) -> Vec<std::path::PathBuf> {
        vec![crate::ai::providers::agent_dir().join("npm")]
    }

    fn npm_lookup(
        &self,
        spec: &str,
        dir: &std::path::Path,
        require: bool,
    ) -> crate::pir_node::NpmLookup {
        let payload = serde_json::json!({
            "dir": dir.to_string_lossy(),
            "spec": spec,
            "mode": if require { "require" } else { "esm" },
        });
        let value = match node_fns::npm_resolve_op(&payload) {
            Ok(value) => value,
            Err(error) => {
                let error = node_fns::into_pir_node_error(error);
                return crate::pir_node::NpmLookup::Error {
                    code: error.code,
                    message: error.message,
                };
            }
        };
        let text = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match value.get("kind").and_then(serde_json::Value::as_str) {
            Some("builtin") => crate::pir_node::NpmLookup::Builtin { name: text("name") },
            Some("file") => crate::pir_node::NpmLookup::File {
                path: std::path::PathBuf::from(text("path")),
                module_kind: text("moduleKind"),
            },
            Some("missing") => crate::pir_node::NpmLookup::NotFound {
                code: text("code"),
                message: text("message"),
            },
            _ => crate::pir_node::NpmLookup::Error {
                code: text("code"),
                message: text("message"),
            },
        }
    }

    fn npm_source(
        &self,
        path: &std::path::Path,
        as_cjs: bool,
    ) -> Result<crate::pir_node::NpmSource, crate::pir_node::NodeError> {
        let value = node_fns::npm_source_value(path, as_cjs)
            .map_err(node_fns::into_pir_node_error)?;
        Ok(crate::pir_node::NpmSource {
            kind: value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            source: value
                .get("source")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }
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
    // This runtime's own generation key (see `fd_generation`): every
    // generation-scoped host resource (workers, child processes, fetches,
    // watchers, sqlite handles) is tagged with it, so a `/reload`
    // successor's teardown can never release the predecessor's resources.
    generation: u64,
) -> rquickjs::Result<()> {
    use rquickjs::Object;

    // Transport helpers without a runtime handle report through the host's
    // `PIR_EXT_DIAG`-gated channel; install the bridge once per process.
    crate::pir_node::runtime::install_diag_sink(std::sync::Arc::new(|message: &str| {
        crate::ext_diag!("{}", message);
    }));

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

    // Structured Node surface. `pir-node` owns every implementation; this
    // C-ABI layer only converts live QuickJS values to/from JSON trees. The
    // same `NodeRuntime` also backs the legacy string entries below, so the
    // async shim pumps and the C-ABI funnel share one op table.
    let node_runtime = {
        // One guard for the whole snapshot: separate `lock_state` calls inside
        // a single struct expression would keep the earlier guard alive as a
        // temporary and deadlock on the second lock.
        let (cwd, mode, has_ui) = {
            let s = lock_state(state);
            (
                s.config.cwd.clone(),
                normalize_mode(&s.config.mode).to_owned(),
                s.config.has_ui,
            )
        };
        let cfg = crate::pir_node::SessionConfig {
            cwd,
            mode,
            has_ui,
            ..Default::default()
        };
        let context: Arc<dyn crate::pir_node::NodeContext> = Arc::new(PirNodeContext {
            state: state.clone(),
            allowed_roots: allowed_roots.clone(),
            generation,
        });
        let hooks: Arc<dyn crate::pir_node::NodeHooks> = Arc::new(PirNodeHooks {
            state: state.clone(),
        });
        // The sandbox fence must see the same root set the legacy string path
        // used: configured allowed roots plus the pi config/data root, the
        // agent dir, the OS temp scratch area and the session cwd.
        let effective_roots =
            Arc::new(StdMutex::new(node_fns::effective_roots(state, allowed_roots)));
        let node_runtime = Arc::new(crate::pir_node::NodeRuntime::new(
            effective_roots,
            cfg,
            context,
            hooks,
        ));
        ctx.store_userdata(NodeState(node_runtime.clone()))
            .map_err(|_| native_state_error())?;
        let node = Object::new(ctx.clone())?;
        globals.set("__pirNode", node.clone())?;
        node.set(
            "invoke",
            rquickjs::prelude::Func::from(node_invoke_native),
        )?;
        node_runtime
    };

    v8_diagnostics::register_v8_diagnostics(&host)?;
    register_session_host_fns(&host, state)?;
    register_context_host_fns(&host, state)?;
    // Host-backed SDK `SessionManager` ops (any session file; see
    // `agent_session::ext_session_ops`). Runtime-state-independent.
    crate::agent_session::ext_session_ops::register_ext_session_ops(&host)?;
    register_tool_exec_host_fns(&host, state)?;
    // Every legacy string entry below is wire-only: it parses the shim's JSON
    // payload once and dispatches into the shared `NodeRuntime`.
    register_node_host_fns(&host, &node_runtime)?;
    // child_process + zlib ride a second generic entry (`__pir_host.nodeExt`):
    // the JS bridge routes those two module names here.
    register_child_host_fns(&host, &node_runtime)?;
    // Web `fetch` and the loopback net backend ride a third generic entry
    // (`__pir_host.http`): same envelope, sandbox-independent (network needs
    // no fs roots; see the permission note in `pir-node`'s http module).
    register_http_host_fns(&host, &node_runtime)?;
    // worker_threads rides a fourth generic entry (`__pir_host.worker` +
    // async `workerRecvPoll`): one OS thread + one QuickJS runtime per
    // Worker, mailboxes + pumps for the message bridge.
    register_worker_host_fns(&host)?;
    // `fs.watch` rides a fifth generic entry (`__pir_host.fsWatch`): portable
    // snapshot-diff watcher threads (the TUI git-branch watcher's pattern),
    // JS drains events on a timer pump.
    register_fs_watch_host_fns(&host, &node_runtime)?;
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
    // `path`'s char-scanning kernels (`join`/`resolve`/`normalize`/
    // `relative`) get a typed entry too: the JS shim's calls pass strings
    // directly instead of paying the generic `invoke` payload conversion on
    // every `path.join`.
    register_path_ops(ctx)?;
    // Byte-moving fs/crypto kernels (`__pirFsRead`/`__pirFsWrite`/
    // `__pirCryptoDigest`/`__pirCryptoHmac`/`__pirCryptoRandom`): pass
    // `Uint8Array`s across the C-ABI directly, no base64 lap through the JSON
    // payload (see `bytes_ops`).
    register_bytes_ops(ctx)?;
    // Metadata-shaped calls (`__pirFsStat`/`__pirFsExists`/`__pirFsReaddir`/
    // `__pirProcessCwd`): one string in, an already-shaped live value out, no
    // envelope (see `system_ops`).
    register_system_ops(ctx)?;
    // SQLite row paths (`__pirSqliteQuery`): bind from a live array and build
    // row values directly, one C call per statement (see `sqlite_ops`).
    register_sqlite_ops(ctx)?;
    // WHATWG URL family (`__pirUrlParse`/`__pirUrlSearchParams`): the `url`
    // shim is pure JS today, so `new URL` pays a full re-parse per
    // construction. Typed entries hand back live component values (see
    // `url_ops`).
    register_url_ops(ctx)?;
    // `util.inspect`'s default formatting path (`__pirInspect`): renders
    // plain objects/arrays from live values instead of walking the shim's
    // general-purpose formatter (see `inspect_ops`).
    register_inspect_ops(ctx)?;
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

/// The legacy string wire (`__pir_host.node` / `nodeExt` / `http` /
/// `fsWatch`) that the async shim pumps still call directly. Parsing the JSON
/// payload and serializing the envelope is all this layer does: every op
/// executes in `pir-node`, so each Node module has exactly one implementation.
fn node_string_call(
    runtime: &crate::pir_node::NodeRuntime,
    module: &str,
    op: &str,
    payload: &str,
) -> String {
    let payload: serde_json::Value =
        serde_json::from_str(payload).unwrap_or(serde_json::Value::Null);
    match runtime.invoke(module, op, &payload) {
        Ok(value) => crate::pir_node::NodeError::ok(value).to_string(),
        Err(error) => error.to_json_value().to_string(),
    }
}

/// Install `__pir_host.nodeExt(module, op, payloadJson)`: the child_process /
/// zlib wire the shims call directly. Ops execute in `pir-node`.
fn register_child_host_fns(
    host: &rquickjs::Object<'_>,
    runtime: &Arc<crate::pir_node::NodeRuntime>,
) -> rquickjs::Result<()> {
    let runtime = runtime.clone();
    host.set(
        "nodeExt",
        Func::from(
            move |module: String, op: String, payload: String| -> String {
                node_string_call(&runtime, &module, &op, &payload)
            },
        ),
    )?;
    Ok(())
}

/// Install `__pir_host.http(op, payloadJson)`: the async fetch/server/net wire
/// the web shims poll from their own pumps. Ops execute in `pir-node`.
fn register_http_host_fns(
    host: &rquickjs::Object<'_>,
    runtime: &Arc<crate::pir_node::NodeRuntime>,
) -> rquickjs::Result<()> {
    let runtime = runtime.clone();
    host.set(
        "http",
        Func::from(move |op: String, payload: String| -> String {
            node_string_call(&runtime, "http", &op, &payload)
        }),
    )?;
    Ok(())
}

/// Install `__pir_host.fsWatch(op, payloadJson)`: the snapshot-diff watcher
/// wire the fs shim polls. Ops execute in `pir-node`.
fn register_fs_watch_host_fns(
    host: &rquickjs::Object<'_>,
    runtime: &Arc<crate::pir_node::NodeRuntime>,
) -> rquickjs::Result<()> {
    let runtime = runtime.clone();
    host.set(
        "fsWatch",
        Func::from(move |op: String, payload: String| -> String {
            node_string_call(&runtime, "fs_watch", &op, &payload)
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
    // All process-wide host tables are generation-scoped in `pir-node`; the
    // legacy string entries and the `__pirNode` C-ABI funnel share that op
    // table, so one fan-out covers every path.
    worker_fns::release_generation(generation);
    crate::pir_node::modules::child::release_generation(generation);
    crate::pir_node::modules::fs_watch::release_generation(generation);
    crate::pir_node::modules::http::release_generation(generation);
    crate::pir_node::modules::sqlite::release_generation(generation);
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
        crate::pir_node::modules::http::http_resource_counts_for_tests();
    HostResourceCounts {
        children: crate::pir_node::modules::child::child_count_for_tests(),
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
    crate::pir_node::modules::fs_watch::watch_count_for_tests()
}

use self::buffer_ops::register_buffer_ops;
use self::bytes_ops::register_bytes_ops;
use self::codemode_fns::register_codemode_host_fns;
use self::context_fns::register_context_host_fns;
use self::inspect_ops::register_inspect_ops;
use self::node_fns::register_node_host_fns;
use self::path_ops::register_path_ops;
use self::session_fns::register_session_host_fns;
use self::sqlite_ops::register_sqlite_ops;
use self::system_ops::register_system_ops;
use self::tool_exec_fns::register_tool_exec_host_fns;
use self::url_ops::register_url_ops;
use self::worker_fns::register_worker_host_fns;

mod buffer_ops;
mod bytes_ops;
mod codemode_fns;
mod context_fns;
mod inspect_ops;
mod node_fns;
mod path_ops;
mod session_fns;
mod sqlite_ops;
mod system_ops;
mod tool_exec_fns;
mod url_ops;
mod v8_diagnostics;
// `pub(crate)` so `runtime` can re-export the worker's JS stack request
// (`WORKER_JS_STACK_BYTES`) for the engine-bound test; nothing else escapes.
pub(crate) mod worker_fns;
