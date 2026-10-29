//! LLRT-style node built-in shims: pure-JS implementations served as virtual
//! modules under `@node/<name>`, plus the runtime bootstrap that installs
//! `globalThis.__pirPlatform`, the shared `__pirNodeCall` bridge, the global
//! `Buffer` backfill and the `process` global.
//!
//! Phase 2 adds fs/os/crypto/buffer and host-backs `process.env/cwd/pid`; the
//! fs/os/crypto sources call `__pir_host.node(module, op, payloadJson)`
//! (registered by `runtime::host_functions::node_fns`), which enforces the
//! extension sandbox roots. Phase 3 adds child_process + zlib (host ops behind
//! the second entry `__pir_host.nodeExt`, see `child_fns`) and the minimal
//! `stream` shim. Phase 4a.1 fills the real-extension gaps: the fs fd family
//! plus chmod/cp/mkdtemp/rmdir/symlink, `fs/promises` open/opendir/mkdtemp
//! (`FileHandle`/`Dir`), `stream/promises` (`finished`/`pipeline`), the
//! `process` event emitter (with the `exit` dispatch at runtime teardown),
//! `os.devNull`, and `require("events")` returning the class. Phase 4b adds
//! the `worker_threads` shim (now real in-process workers; see
//! `worker_fns.rs`) and the
//! `Intl.Segmenter` code-point pad. Phase 4e adds the pi-web-access /
//! pi-phone load-parity surface: `net` (`isIP` real; loopback TCP real since
//! the minimal net wave, TLS/Unix sockets out of tier),
//! `dns/promises` (`lookup` over the host resolver; the `resolve*` family
//! throws), the `http`/`https`/`tls` load-compatible stubs (constructors and
//! entry points throw, skeletons and tables exist), `sqlite` (real since the
//! sqlite wave: DatabaseSync over the host SQLite bridge) and the
//! `async_hooks`
//! `AsyncLocalStorage` approximation. The P0 residual wave maps EVERY
//! remaining `module.builtinModules` entry (audited against real Node v24,
//! including the private `node:_*` internals and the `node:`-only
//! test/sea/reporters specifiers), so nothing fails at the dependency scan
//! anymore: real/near-real (`console`, `sys`, `diagnostics_channel`), minimal
//! (`stream/web`, `stream/consumers`, `readline/promises`,
//! `inspector(/promises)`), and throws-tier stubs (`http2`, `cluster`,
//! `dgram`, `repl`, `trace_events`, `domain`, `wasi`, `node:test(/reporters)`,
//! `node:sea`). All sources are ES2020: the engine has no global
//! URL/TextDecoder/atob/btoa, so codecs are handwritten here.

use super::embedded::Shim;

/// Canonical node module table: `(specifier, canonical @node/<name>)`.
const NODE_KNOWN: &[(&str, &str)] = &[
    ("path", "@node/path"),
    ("path/posix", "@node/path/posix"),
    ("path/win32", "@node/path/win32"),
    ("url", "@node/url"),
    ("util", "@node/util"),
    ("util/types", "@node/util/types"),
    ("events", "@node/events"),
    ("querystring", "@node/querystring"),
    ("string_decoder", "@node/string_decoder"),
    ("assert", "@node/assert"),
    ("assert/strict", "@node/assert/strict"),
    ("timers", "@node/timers"),
    ("timers/promises", "@node/timers/promises"),
    ("perf_hooks", "@node/perf_hooks"),
    ("process", "@node/process"),
    ("fs", "@node/fs"),
    ("fs/promises", "@node/fs/promises"),
    ("os", "@node/os"),
    ("crypto", "@node/crypto"),
    ("buffer", "@node/buffer"),
    ("child_process", "@node/child_process"),
    ("stream", "@node/stream"),
    ("stream/promises", "@node/stream/promises"),
    ("zlib", "@node/zlib"),
    ("module", "@node/module"),
    ("worker_threads", "@node/worker_threads"),
    ("net", "@node/net"),
    ("dns", "@node/dns"),
    ("dns/promises", "@node/dns/promises"),
    ("http", "@node/http"),
    ("https", "@node/https"),
    ("tls", "@node/tls"),
    ("sqlite", "@node/sqlite"),
    ("async_hooks", "@node/async_hooks"),
    ("tty", "@node/tty"),
    ("readline", "@node/readline"),
    ("readline/promises", "@node/readline/promises"),
    ("v8", "@node/v8"),
    ("vm", "@node/vm"),
    ("constants", "@node/constants"),
    ("punycode", "@node/punycode"),
    // P0 residual wave: the remaining public builtins (surface audited
    // against real Node v24 `module.builtinModules`; zero load-time gaps).
    ("console", "@node/console"),
    ("sys", "@node/sys"),
    ("diagnostics_channel", "@node/diagnostics_channel"),
    ("stream/web", "@node/stream/web"),
    ("stream/consumers", "@node/stream/consumers"),
    ("inspector", "@node/inspector"),
    ("inspector/promises", "@node/inspector/promises"),
    ("http2", "@node/http2"),
    ("cluster", "@node/cluster"),
    ("dgram", "@node/dgram"),
    ("repl", "@node/repl"),
    ("trace_events", "@node/trace_events"),
    ("domain", "@node/domain"),
    ("wasi", "@node/wasi"),
    // node:-prefixed-only builtins (bare `test`/`sea` do not exist in real
    // Node either; the keys here are what remains after the `node:` strip).
    ("test", "@node/test"),
    ("test/reporters", "@node/test/reporters"),
    ("sea", "@node/sea"),
    // Private `node:_*` internals (defensive requires in some http/stream
    // stacks): aliases onto the corresponding public shim surfaces.
    ("_http_agent", "@node/_http_agent"),
    ("_http_client", "@node/_http_client"),
    ("_http_common", "@node/_http_common"),
    ("_http_incoming", "@node/_http_incoming"),
    ("_http_outgoing", "@node/_http_outgoing"),
    ("_http_server", "@node/_http_server"),
    ("_stream_duplex", "@node/_stream_duplex"),
    ("_stream_passthrough", "@node/_stream_passthrough"),
    ("_stream_readable", "@node/_stream_readable"),
    ("_stream_transform", "@node/_stream_transform"),
    ("_stream_wrap", "@node/_stream_wrap"),
    ("_stream_writable", "@node/_stream_writable"),
    ("_tls_common", "@node/_tls_common"),
    ("_tls_wrap", "@node/_tls_wrap"),
];

/// Canonical short names of every shimmed builtin (`path`, `fs/promises`, …),
/// used verbatim as the `__pirNodeModules` registry keys the CJS `require`
/// returns modules from.
pub fn builtin_short_names() -> Vec<&'static str> {
    NODE_KNOWN
        .iter()
        .map(|(_, canonical)| canonical.trim_start_matches("@node/"))
        .collect()
}

/// Canonicalize a `node:X` / bare `X` specifier to `@node/X`.
/// Returns `None` when the module is not part of the shimmed surface.
pub fn canonical_node_name(name: &str) -> Option<&'static str> {
    // Idempotent: resolution passes canonical `@node/X` names back through.
    let bare = name.strip_prefix("node:").unwrap_or(name);
    let bare = bare.strip_prefix("@node/").unwrap_or(bare);
    NODE_KNOWN
        .iter()
        .find(|(from, _)| *from == bare)
        .map(|(_, canonical)| *canonical)
}

/// Source of the virtual module for a canonical node name.
pub fn source_for(canonical: &str) -> Option<&'static str> {
    match canonical {
        "@node/path" => Some(PATH_SHIM.source()),
        "@node/path/posix" => Some(PATH_POSIX_SHIM),
        "@node/path/win32" => Some(PATH_WIN32_SHIM),
        "@node/url" => Some(URL_SHIM.source()),
        "@node/util" => Some(UTIL_SHIM.source()),
        "@node/util/types" => Some(UTIL_TYPES_SHIM.source()),
        "@node/events" => Some(EVENTS_SHIM.source()),
        "@node/querystring" => Some(QUERYSTRING_SHIM.source()),
        "@node/string_decoder" => Some(STRING_DECODER_SHIM.source()),
        "@node/assert" => Some(ASSERT_SHIM.source()),
        "@node/assert/strict" => Some(ASSERT_STRICT_SHIM.source()),
        "@node/timers" => Some(TIMERS_SHIM),
        "@node/timers/promises" => Some(TIMERS_PROMISES_SHIM),
        "@node/perf_hooks" => Some(PERF_HOOKS_SHIM),
        "@node/process" => Some(PROCESS_SHIM.source()),
        "@node/fs" => Some(FS_SHIM.source()),
        "@node/fs/promises" => Some(FS_PROMISES_SHIM.source()),
        "@node/os" => Some(OS_SHIM.source()),
        "@node/crypto" => Some(CRYPTO_SHIM.source()),
        "@node/buffer" => Some(BUFFER_SHIM),
        "@node/child_process" => Some(CHILD_PROCESS_SHIM.source()),
        "@node/stream" => Some(STREAM_SHIM.source()),
        "@node/stream/promises" => Some(STREAM_PROMISES_SHIM.source()),
        "@node/zlib" => Some(ZLIB_SHIM.source()),
        "@node/module" => Some(MODULE_SHIM.source()),
        "@node/worker_threads" => Some(WORKER_THREADS_SHIM.source()),
        "@node/net" => Some(NET_SHIM.source()),
        "@node/dns" => Some(DNS_SHIM.source()),
        "@node/dns/promises" => Some(DNS_PROMISES_SHIM.source()),
        "@node/http" => Some(HTTP_SHIM.source()),
        "@node/https" => Some(HTTPS_SHIM.source()),
        "@node/tls" => Some(TLS_SHIM),
        "@node/sqlite" => Some(SQLITE_SHIM.source()),
        "@node/async_hooks" => Some(ASYNC_HOOKS_SHIM.source()),
        "@node/tty" => Some(TTY_SHIM.source()),
        "@node/readline" => Some(READLINE_SHIM.source()),
        "@node/readline/promises" => Some(READLINE_PROMISES_SHIM.source()),
        "@node/v8" => Some(V8_SHIM.source()),
        "@node/vm" => Some(VM_SHIM.source()),
        "@node/constants" => Some(CONSTANTS_SHIM.source()),
        "@node/punycode" => Some(PUNYCODE_SHIM.source()),
        "@node/console" => Some(CONSOLE_SHIM.source()),
        "@node/sys" => Some(SYS_SHIM.source()),
        "@node/diagnostics_channel" => Some(DIAGNOSTICS_CHANNEL_SHIM.source()),
        "@node/stream/web" => Some(STREAM_WEB_SHIM.source()),
        "@node/stream/consumers" => Some(STREAM_CONSUMERS_SHIM.source()),
        "@node/inspector" => Some(INSPECTOR_SHIM.source()),
        "@node/inspector/promises" => Some(INSPECTOR_PROMISES_SHIM.source()),
        "@node/http2" => Some(HTTP2_SHIM.source()),
        "@node/cluster" => Some(CLUSTER_SHIM.source()),
        "@node/dgram" => Some(DGRAM_SHIM.source()),
        "@node/repl" => Some(REPL_SHIM.source()),
        "@node/trace_events" => Some(TRACE_EVENTS_SHIM.source()),
        "@node/domain" => Some(DOMAIN_SHIM.source()),
        "@node/wasi" => Some(WASI_SHIM.source()),
        "@node/test" => Some(TEST_SHIM.source()),
        "@node/test/reporters" => Some(TEST_REPORTERS_SHIM.source()),
        "@node/sea" => Some(SEA_SHIM.source()),
        "@node/_http_agent" => Some(HTTP_INTERNAL_SHIM.source()),
        "@node/_http_client" => Some(HTTP_INTERNAL_SHIM.source()),
        "@node/_http_common" => Some(HTTP_COMMON_SHIM.source()),
        "@node/_http_incoming" => Some(HTTP_INCOMING_SHIM.source()),
        "@node/_http_outgoing" => Some(HTTP_OUTGOING_SHIM.source()),
        "@node/_http_server" => Some(HTTP_SERVER_SHIM.source()),
        "@node/_stream_duplex" => Some(STREAM_INTERNALS_SHIM.source()),
        "@node/_stream_passthrough" => Some(STREAM_EMPTY_INTERNALS_SHIM.source()),
        "@node/_stream_readable" => Some(STREAM_INTERNALS_SHIM.source()),
        "@node/_stream_transform" => Some(STREAM_EMPTY_INTERNALS_SHIM.source()),
        "@node/_stream_wrap" => Some(STREAM_EMPTY_INTERNALS_SHIM.source()),
        "@node/_stream_writable" => Some(STREAM_INTERNALS_SHIM.source()),
        "@node/_tls_common" => Some(TLS_INTERNALS_SHIM.source()),
        "@node/_tls_wrap" => Some(TLS_INTERNALS_SHIM.source()),
        _ => None,
    }
}

/// `process.platform`/`path` platform value, chosen at compile time.
pub const NODE_PLATFORM: &str = if cfg!(windows) {
    "win32"
} else if cfg!(target_os = "macos") {
    "darwin"
} else {
    "linux"
};

/// `process.arch` in Node's naming (cfg constant, not `std::env::consts::ARCH`).
const NODE_ARCH: &str = if cfg!(target_arch = "x86_64") {
    "x64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    std::env::consts::ARCH
};

/// One JS snippet injected once before `HOST_PRELUDE`: the platform constant,
/// the shared `__pir_host.node` bridge, the CJS require runtime, the global
/// `Buffer` backfill and the `process` global (LLRT-style compatibility
/// label: the values describe a Node-compatible surface, not the actual
/// host), plus the Web primitives (abort/events family, minimal streams,
/// TextEncoder/TextDecoder and the global `fetch` surface) — every one of
/// them guarded with `??` so an engine-provided global is never shadowed.
pub fn globals_source() -> String {
    let stub = PROCESS_GLOBAL_STUB.replace("__PIR_NODE_ARCH__", NODE_ARCH);
    let names: Vec<String> = builtin_short_names()
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect();
    format!(
        "globalThis.__pirPlatform = \"{NODE_PLATFORM}\";\n{NODE_CALL_HELPER}\n{NODE_CJS_RUNTIME}\n\
         globalThis.require ?? (globalThis.require = globalThis.__pirMakeRequire(\"\"));\n\
         globalThis.__pirBuiltinModuleNames = [{}];\n\
         globalThis.__pirNodeModules = globalThis.__pirNodeModules || {{}};\n{BUFFER_GLOBAL}\n{INTL_PAD}\n{stub}\n\
         {ABORT_GLOBAL}\n{WEB_STREAMS_GLOBAL}\n{TEXT_CODEC_GLOBAL}\n{FETCH_GLOBAL}\n{WEBCOMPAT_GLOBAL}",
        names.join(", "),
    )
}

/// One JS snippet (evaluated after `HOST_PRELUDE`) defining the async
/// builtin sweep; `runtime.rs` calls it once and awaits the returned promise
/// so `require("path")` and friends are synchronous afterwards.
pub fn node_modules_bootstrap_source() -> String {
    r#"
globalThis.__pirLoadNodeBuiltins = async function () {
  const mods = globalThis.__pirNodeModules;
  const failures = [];
  for (const name of globalThis.__pirBuiltinModuleNames) {
    try {
      mods[name] = await import("@node/" + name);
    } catch (error) {
      failures.push(name + ": " + (error && error.stack ? String(error.stack) : String(error)));
      mods[name] = {};
    }
  }
  // Bare-package virtual shims (see `shims::canonical_bare_package_alias`):
  // CJS `require("<bare name>")` resolves to a builtin-kind result keyed by
  // the bare name and reads the module here. All three are NAPI native addons
  // on disk (better-sqlite3, sqlite3, bcrypt) and can never load in-process.
  for (const name of ["better-sqlite3", "sqlite3", "bcrypt"]) {
    try {
      mods[name] = await import(name);
    } catch (error) {
      failures.push(
        name + ": " + (error && error.stack ? String(error.stack) : String(error))
      );
      mods[name] = {};
    }
  }
  globalThis.__pirBootErrors = failures;
  return failures.length > 0 ? "failures" : "ok";
};
"#
    .to_string()
}

/// Evaluated synchronously by `ExtensionRuntime::drop` (the runtime
/// teardown): Node's final event, with the exit code the process would
/// carry. A throwing exit handler is swallowed — Node prints and continues,
/// and teardown must not be aborted by extension code.
pub const PROCESS_EXIT_SOURCE: &str =
    "try { process.emit(\"exit\", process.exitCode || 0); } catch (e) {}";

/// Shared JSON bridge to `__pir_host.node`: unwraps the `{ok,value}` /
/// `{ok:false,code,message,syscall,path}` envelope and raises a Node-shaped
/// `Error`; the fs/os/crypto/process shims all route through this helper. The
/// `child`/`zlib` module names ride the second native entry
/// (`__pir_host.nodeExt`, registered by `child_fns`) — the primary dispatcher
/// is the phase-2 fs/os/crypto table's own — with the identical envelope.
static NODE_CALL_HELPER: Shim = Shim::new(
    "node_call_helper.js.txt",
    include_bytes!("../../../assets/shims/node_call_helper.js.txt.deflate"),
);

/// CommonJS runtime for npm dependencies: `__pirCjsLoad` executes a CJS file
/// synchronously through an indirect-eval wrapper (global scope, top-level
/// `return` tolerated, per-file `require`), and `__pirMakeRequire` builds the
/// dir-bound `require` with `.resolve` — the pieces `createRequire`
/// (`@node/module`) and the loader's CJS wrapper module call into.
static NODE_CJS_RUNTIME: Shim = Shim::new(
    "node_cjs_runtime.js.txt",
    include_bytes!("../../../assets/shims/node_cjs_runtime.js.txt.deflate"),
);

/// Global `Buffer` backfill: the single shared implementation. `@node/buffer`,
/// `@node/fs` and `@node/crypto` all alias `globalThis.Buffer`, so codec
/// behavior stays identical everywhere. ES2020 only (no atob/btoa/
/// TextDecoder in the engine); encodings: utf8/hex/base64/latin1/utf16le.
static BUFFER_GLOBAL: Shim = Shim::new(
    "buffer_global.js.txt",
    include_bytes!("../../../assets/shims/buffer_global.js.txt.deflate"),
);

/// Minimal `Intl` pad (phase 4b): installed only when the engine carries no
/// real `Intl` (QuickJS does not), so an actual implementation is never
/// shadowed. `Segmenter` is a code-point approximation, not grapheme
/// clustering: ZWJ emoji sequences and combining marks come back as separate
/// items — enough for the load-time `new Intl.Segmenter(...)` + per-character
/// iteration real extensions use, and documented here rather than faked.
static INTL_PAD: Shim = Shim::new(
    "intl_pad.js.txt",
    include_bytes!("../../../assets/shims/intl_pad.js.txt.deflate"),
);

/// `process` global: LLRT-style compatibility labels (`version`/`versions`)
/// plus host-backed env/cwd/pid. `env` is a snapshot taken once at bootstrap
/// (extensions treat it as read-mostly; a live view would need a proxy the
/// engine surface does not need); `cwd()` asks the host on every call, so it
/// stays live.
static PROCESS_GLOBAL_STUB: Shim = Shim::new(
    "process_global_stub.js.txt",
    include_bytes!("../../../assets/shims/process_global_stub.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// path
// ---------------------------------------------------------------------------

/// Node `path` shim: full posix + win32 implementations; `sep`/`delimiter`
/// and the default export follow `globalThis.__pirPlatform`. Phase 1
/// limitation: there is no cwd host, so `resolve()` with no args returns "."
/// and relative-only `resolve()` anchors at the root instead of `process.cwd()`.
static PATH_SHIM: Shim = Shim::new(
    "path_shim.js.txt",
    include_bytes!("../../../assets/shims/path_shim.js.txt.deflate"),
);

const PATH_POSIX_SHIM: &str = r#"
import path from "@node/path";

const posix = path.posix;
const _makeLong = posix.toNamespacedPath;
const matchesGlob = path.matchesGlob;

export default posix;
export const resolve = posix.resolve;
export const normalize = posix.normalize;
export const isAbsolute = posix.isAbsolute;
export const join = posix.join;
export const relative = posix.relative;
export const dirname = posix.dirname;
export const basename = posix.basename;
export const extname = posix.extname;
export const parse = posix.parse;
export const format = posix.format;
export const toNamespacedPath = posix.toNamespacedPath;
export const sep = posix.sep;
export const delimiter = posix.delimiter;
export { _makeLong, matchesGlob };
export { posix };
export const win32 = path.win32;
"#;

const PATH_WIN32_SHIM: &str = r#"
import path from "@node/path";

const win32 = path.win32;
const _makeLong = win32.toNamespacedPath;
const matchesGlob = path.matchesGlob;

export default win32;
export const resolve = win32.resolve;
export const normalize = win32.normalize;
export const isAbsolute = win32.isAbsolute;
export const join = win32.join;
export const relative = win32.relative;
export const dirname = win32.dirname;
export const basename = win32.basename;
export const extname = win32.extname;
export const parse = win32.parse;
export const format = win32.format;
export const toNamespacedPath = win32.toNamespacedPath;
export const sep = win32.sep;
export const delimiter = win32.delimiter;
export { _makeLong, matchesGlob };
export const posix = path.posix;
export { win32 };
"#;

// ---------------------------------------------------------------------------
// url
// ---------------------------------------------------------------------------

/// Node `url` shim. `URL`/`URLSearchParams` are re-exported from the engine
/// when globals exist; otherwise the minimal fallback below covers
/// file:/http(s): usage. Gaps of the fallback: no IDN/normalization beyond
/// lowercasing hosts. Relative references resolve against a `pir-ext:` base
/// (the extension module URL space, so `new URL(rel, import.meta.url)` works)
/// and otherwise keep the phase-1 "Invalid URL" error.
static URL_SHIM: Shim = Shim::new(
    "url_shim.js.txt",
    include_bytes!("../../../assets/shims/url_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// util / util/types
// ---------------------------------------------------------------------------

/// Node `util` shim (phase 1 subset): promisify/callbackify/inherits/deprecate,
/// format/formatWithOptions + a compact inspect, stripVTControlCharacters,
/// debuglog and util.types.
static UTIL_SHIM: Shim = Shim::new(
    "util_shim.js.txt",
    include_bytes!("../../../assets/shims/util_shim.js.txt.deflate"),
);

static UTIL_TYPES_SHIM: Shim = Shim::new(
    "util_types_shim.js.txt",
    include_bytes!("../../../assets/shims/util_types_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// High-fidelity `EventEmitter` for the surface extensions use:
/// snapshot-on-emit, `once()` wrappers exposing `.listener`, and a throw for
/// an unhandled `error` event.
static EVENTS_SHIM: Shim = Shim::new(
    "events_shim.js.txt",
    include_bytes!("../../../assets/shims/events_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// querystring
// ---------------------------------------------------------------------------

/// Node `querystring` shim (`escape`/`unescape` behave exactly like
/// `encodeURIComponent`/`decodeURIComponent` on the observable surface).
static QUERYSTRING_SHIM: Shim = Shim::new(
    "querystring_shim.js.txt",
    include_bytes!("../../../assets/shims/querystring_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// string_decoder
// ---------------------------------------------------------------------------

/// `StringDecoder` with a handwritten UTF-8 state machine (no TextDecoder):
/// incomplete trailing sequences are carried across `write()` calls and
/// flushed as U+FFFD from `end()`.
static STRING_DECODER_SHIM: Shim = Shim::new(
    "string_decoder_shim.js.txt",
    include_bytes!("../../../assets/shims/string_decoder_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// assert / assert/strict
// ---------------------------------------------------------------------------

/// Node `assert` shim: callable default export with the static surface; the
/// strict alias set is reachable as `assert.strict` (see the strict shim).
static ASSERT_SHIM: Shim = Shim::new(
    "assert_shim.js.txt",
    include_bytes!("../../../assets/shims/assert_shim.js.txt.deflate"),
);

static ASSERT_STRICT_SHIM: Shim = Shim::new(
    "assert_strict_shim.js.txt",
    include_bytes!("../../../assets/shims/assert_strict_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// timers / timers/promises / perf_hooks / process
// ---------------------------------------------------------------------------

/// Re-export of the host timer globals. The host prelude installs no
/// `setImmediate`, so the immediate pair rides `setTimeout(0)` — same
/// observable ordering guarantees the extensions rely on (macrotask-ish,
/// cancellable) without a dedicated host phase.
const TIMERS_SHIM: &str = r#"
import * as timersPromises from "@node/timers/promises";

const setTimeout = globalThis.setTimeout;
const clearTimeout = globalThis.clearTimeout;
const setInterval = globalThis.setInterval;
const clearInterval = globalThis.clearInterval;
// Node's `require("timers").setImmediate` IS the global (identity holds, and
// one implementation stays in sync): the global one bridges onto the host
// timer wheel so an immediate runs in the check phase — after the turn's
// microtasks — rather than as a promise job.
const setImmediate = globalThis.setImmediate;
const clearImmediate = globalThis.clearImmediate;

export { setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, clearImmediate };
export const promises = timersPromises;
export default {
  setTimeout: setTimeout,
  clearTimeout: clearTimeout,
  setInterval: setInterval,
  clearInterval: clearInterval,
  setImmediate: setImmediate,
  clearImmediate: clearImmediate,
};
"#;

/// Promise-based timers on top of the host timer globals, plus the
/// `setInterval` async generator and the `scheduler` skeleton.
const TIMERS_PROMISES_SHIM: &str = r#"
function setTimeout(ms, value, options) {
  if (options && typeof options.signal === "object" && options.signal !== null && options.signal.aborted) {
    return Promise.reject(options.signal.reason === undefined ? new Error("The operation was aborted") : options.signal.reason);
  }
  return new Promise((resolve, reject) => {
    const handle = globalThis.setTimeout(() => resolve(value), ms);
    if (options && typeof options.signal === "object" && options.signal !== null) {
      options.signal.addEventListener("abort", () => {
        globalThis.clearTimeout(handle);
        reject(options.signal.reason === undefined ? new Error("The operation was aborted") : options.signal.reason);
      }, { once: true });
    }
  });
}

function setImmediate(value, options) {
  return setTimeout(0, value, options);
}

function getAbortedError(signal) {
  return signal && signal.reason !== undefined ? signal.reason : new Error("The operation was aborted");
}

async function* setIntervalGenerator(ms, value, options) {
  const signal = options && options.signal;
  if (signal && signal.aborted) throw getAbortedError(signal);
  let count = options && typeof options.count === "number" ? options.count : Infinity;
  while (count > 0) {
    await new Promise((resolve, reject) => {
      const handle = globalThis.setTimeout(resolve, ms);
      if (signal) {
        signal.addEventListener("abort", () => {
          globalThis.clearTimeout(handle);
          reject(getAbortedError(signal));
        }, { once: true });
      }
    });
    yield value;
    count -= 1;
  }
}

function setInterval(ms, value, options) {
  return setIntervalGenerator(ms, value, options);
}

// Minimal `scheduler` (the timers/promises exports the promise-based one at
// `timersPromises.scheduler`): `yield` is a microtask, `wait` a timer.
const scheduler = {
  yield(value) {
    return Promise.resolve(value);
  },
  wait(delay, options) {
    return setTimeout(delay, undefined, options);
  },
  currentTask: undefined,
};

export { setTimeout, setImmediate, setInterval, scheduler };
export default {
  setTimeout: setTimeout,
  setImmediate: setImmediate,
  setInterval: setInterval,
  scheduler: scheduler,
};
"#;

/// `perf_hooks`: the performance timeline (marks, measures, observers,
/// `timerify`) over the runtime's millisecond clock, plus the observer /
/// histogram skeletons (values the host cannot measure stay zero and are
/// documented rather than faked per call).
///
/// `performance` here is the *global* pad object installed by
/// [`WEBCOMPAT_GLOBAL`], reparented onto `Performance.prototype`, so
/// `require("perf_hooks").performance === globalThis.performance` and
/// `performance.mark` cannot diverge between the two entry points.
const PERF_HOOKS_SHIM: &str = r#"
import { inspect as pirPerfInspect } from "@node/util";

const kInspect = Symbol.for("nodejs.util.inspect.custom");

// V8's wording for a symbol where a string is required; QuickJS words the
// same TypeError differently and the upstream tests match the V8 text.
function perfString(value) {
  if (typeof value === "symbol") {
    throw new TypeError("Cannot convert a Symbol value to a string");
  }
  return String(value);
}

function perfInvalidArg(argument, expected, value) {
  let seen;
  if (value === null) {
    seen = "null";
  } else if (typeof value === "object") {
    seen = "an instance of " + (value.constructor && value.constructor.name ? value.constructor.name : "Object");
  } else {
    // Node appends the value for primitives: "Received type string ('a')".
    seen = "type " + typeof value + " (" + String(value) + ")";
  }
  const error = new TypeError(
    "The \"" + argument + "\" argument must be of type " + expected + ". Received " + seen
  );
  error.code = "ERR_INVALID_ARG_TYPE";
  return error;
}

function perfMissingArg(argument) {
  const error = new TypeError("The \"" + argument + "\" argument must be specified");
  error.code = "ERR_MISSING_ARGS";
  return error;
}

function perfClone(value) {
  if (typeof globalThis.structuredClone === "function") return globalThis.structuredClone(value);
  return value;
}

// --- performance timeline --------------------------------------------------
// The engine already installs a `performance` global whose own (non-deletable)
// `now`/`timeOrigin` come from a microsecond, monotonic source — the ideal
// timeline clock, and the one `performance.now()` keeps using below. `now()`
// is therefore "elapsed since the engine's own origin", like Node's.
const perfOrigin = globalThis.performance && typeof globalThis.performance.timeOrigin === "number"
  ? globalThis.performance.timeOrigin
  : Date.now();

const perfNativeClock =
  globalThis.performance && typeof globalThis.performance.now === "function"
    ? globalThis.performance.now
    : null;

// Fallback only (engine without a performance global): `Date.now()` has 1 ms
// granularity while the usertiming/timerify tests assert `duration > 0`, so a
// repeat reading inside the same millisecond nudges forward by a fraction
// instead of repeating.
let perfLast = 0;
function perfNow() {
  if (perfNativeClock !== null) return perfNativeClock();
  let value = Date.now() - perfOrigin;
  if (value <= perfLast) value = perfLast + 0.001;
  perfLast = value;
  return value;
}
const perfMarks = new Map();
const perfMeasures = [];
const perfObservers = new Set();
let perfDeliveryPending = false;

// Node batches observer delivery per turn, so a burst of marks reaches the
// callback as one entry list (not one call per entry). Promise jobs are the
// one microtask source the engine is guaranteed to have.
function perfScheduleDelivery() {
  if (perfDeliveryPending) return;
  perfDeliveryPending = true;
  const flush = () => {
    perfDeliveryPending = false;
    for (const observer of [...perfObservers]) {
      if (!observer._active || observer._buffer.length === 0) continue;
      const entries = observer._buffer;
      observer._buffer = [];
      observer._callback(new PerformanceObserverEntryList(entries), observer);
    }
  };
  if (typeof globalThis.__pirMicrotask === "function") globalThis.__pirMicrotask(flush);
  else Promise.resolve().then(flush);
}

function perfEmit(entry) {
  if (perfObservers.size === 0) return;
  for (const observer of perfObservers) {
    if (observer._active && observer._types.indexOf(entry.entryType) !== -1) {
      observer._buffer.push(entry);
      perfScheduleDelivery();
    }
  }
}

// The plain `performance.mark(name)` path, allocation-for-allocation the same
// object the PerformanceMark constructor builds but without the two-level
// class-init cost — `performance.mark` is a hot API (the upstream many-marks
// test mints a million of them) and QuickJS class construction is the
// expensive half of the loop.
function perfCreateMark(name, startTime, detail) {
  const mark = Object.create(PerformanceMark.prototype);
  mark.name = name;
  mark.entryType = "mark";
  mark.startTime = startTime;
  mark.duration = 0;
  mark.detail = detail;
  return mark;
}

// A mark name resolves against the user-timing registry first, then against
// the node timing milestones (`nodeStart`, `bootstrapComplete`, ...), exactly
// the two sources Node's measure() accepts.
function perfLookupTime(name) {
  const mark = perfMarks.get(name);
  if (mark !== undefined) return mark.startTime;
  const timing = perfNodeTiming[name];
  if (typeof timing === "number" && name !== "startTime" && name !== "duration") return timing;
  const error = new Error("The \"" + name + "\" performance mark has not been set");
  error.name = "SyntaxError";
  error.code = 12;
  throw error;
}

const perfNodeTiming = {
  name: "node",
  entryType: "node",
  startTime: 0,
  duration: 0,
  nodeStart: perfNow(),
  v8Start: 0,
  environment: 0,
  bootstrapComplete: perfNow(),
  loopStart: -1,
  loopExit: -1,
  idleTime: 0,
};

class PerformanceEntry {
  constructor(name, entryType, startTime, duration) {
    this.name = String(name);
    this.entryType = String(entryType);
    this.startTime = startTime;
    this.duration = duration;
  }
  [kInspect](depth, options, inspect) {
    if (typeof depth === "number" && depth < 0) return this;
    const fields = {
      name: this.name,
      entryType: this.entryType,
      startTime: this.startTime,
      duration: this.duration,
    };
    if (this.detail !== undefined) fields.detail = this.detail;
    const body = typeof inspect === "function" ? inspect(fields, options) : JSON.stringify(fields);
    return this.constructor.name + " " + body;
  }
  toJSON() {
    return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration };
  }
}

class PerformanceMark extends PerformanceEntry {
  constructor(name, options) {
    if (options !== undefined && (options === null || typeof options !== "object")) {
      throw perfInvalidArg("options", "object", options);
    }
    const opts = options === undefined ? {} : options;
    let startTime = opts.startTime;
    if (startTime === undefined || startTime === null) {
      // Node treats a nullish startTime as "now", not as a type error.
      startTime = perfNow();
    } else if (typeof startTime !== "number") {
      throw perfInvalidArg("startTime", "number", startTime);
    }
    super(perfString(name), "mark", startTime, 0);
    // The detail is structured-cloned, so a mutating caller cannot rewrite a
    // recorded mark (Node's PerformanceMark semantics).
    this.detail = opts.detail === undefined ? null : perfClone(opts.detail);
  }
}

class PerformanceMeasure extends PerformanceEntry {
  constructor(name, startTime, duration, detail) {
    super(
      perfString(name),
      "measure",
      startTime === undefined ? 0 : startTime,
      duration === undefined ? 0 : duration
    );
    this.detail = detail === undefined ? null : detail;
  }
}

class PerformanceResourceTiming extends PerformanceEntry {
  constructor(name) {
    super(name, "resource", perfNow(), 0);
  }
}

// Node puts the tag on the mark/measure/performance prototypes only (not on
// PerformanceEntry), with this exact descriptor shape.
for (const pair of [
  [PerformanceMark, "PerformanceMark"],
  [PerformanceMeasure, "PerformanceMeasure"],
]) {
  Object.defineProperty(pair[0].prototype, Symbol.toStringTag, {
    configurable: true, enumerable: false, writable: false, value: pair[1],
  });
}

// ---------------------------------------------------------------------------
// Histogram / RecordableHistogram / ELDHistogram
// ---------------------------------------------------------------------------
// Node's histogram is an HDR histogram (deps/histogram): samples are counted in
// quantized buckets, `percentile(p)` is `hdr_value_at_percentile`, and the
// `percentiles` maps come from HDR's percentile iterator with
// `ticks_per_half_distance = 1`. Both formulas are reproduced here over exact
// per-value counts:
//   * percentile(p): rank = trunc(p / 100 * count + 0.5) clamped to >= 1, then
//     the value the rank lands on (HDR's highest-equivalent value).
//   * percentiles / percentilesBigInt: walk the distinct values as buckets and
//     emit every tick — `p += 100 / 2^(floor(log2(100 / (100 - p))) + 1)`, i.e.
//     0, 50, 75, 87.5, 93.75, … — that is <= the cumulative percentage reached
//     at that bucket, one entry per step, and finish with a single
//     (100, max) entry once every sample has been counted (HDR's "last value"
//     rule, which is why a one-sample histogram reports exactly {0, 100}).
// This is node's exact output inside HDR's single-unit-resolution region
// (values <= 2 * 10^figures, i.e. 2000 at the default figures = 3), which is
// where createHistogram's documented use lives.
// LIMIT: above that region node reports the containing bucket's equivalent
// range (record(65536) → min 65536, max 65599, percentile values rounded up to
// the bucket ceiling), while this shim keeps the exact value. Values are held
// as BigInt, so `minBigInt`/`countBigInt`/`percentileBigInt` are exact and
// `record(2n)` works like node's int64 handle.

const kPirHistogramState = Symbol("pirHistogramState");
const kPirHistogramMap = Symbol("pirHistogramMap");
const kPirHistogramRecordable = Symbol("pirHistogramRecordable");
const kPirHistogramEld = Symbol("pirHistogramEld");
const kPirHistogramSkipThrow = Symbol("pirHistogramSkipThrow");
// i64::MAX — HDR reports it as the min of an empty histogram (and as
// Number(i64::MAX) = 9223372036854776000, the double the upstream test pins).
const PIR_HISTOGRAM_I64_MAX = 9223372036854775807n;

function pirIllegalConstructor() {
  const error = new TypeError("Illegal constructor");
  error.code = "ERR_ILLEGAL_CONSTRUCTOR";
  return error;
}

function pirInvalidThis(expected) {
  const error = new TypeError("Value of \"this\" must be of type " + expected);
  error.code = "ERR_INVALID_THIS";
  return error;
}

/// node's ERR_INVALID_ARG_TYPE with a class-shaped `expected` ("must be an
/// instance of X" — the wording `histogram.add(other)` uses).
function pirInvalidInstance(argument, expected, value) {
  let seen;
  if (value === null) {
    seen = "null";
  } else if (typeof value === "object" || typeof value === "function") {
    seen = "an instance of " + (value.constructor && value.constructor.name ? value.constructor.name : "Object");
  } else {
    seen = "type " + typeof value + " (" + String(value) + ")";
  }
  const error = new TypeError(
    "The \"" + argument + "\" argument must be an instance of " + expected + ". Received " + seen
  );
  error.code = "ERR_INVALID_ARG_TYPE";
  return error;
}

function pirPerfShow(value) {
  return typeof value === "string" ? "'" + value + "'" : String(value);
}

function pirPerfRangeError(name, range, value) {
  const error = new RangeError(
    "The value of \"" + name + "\" is out of range. It must be " + range +
    ". Received " + pirPerfShow(value)
  );
  error.code = "ERR_OUT_OF_RANGE";
  return error;
}

/// node's internal/validators validateInteger: type, then integrality, then the
/// [min, max] window, each with its own message (`max` undefined keeps the
/// one-sided ">= min" wording monitorEventLoopDelay's resolution uses).
function pirPerfValidateInteger(value, name, min, max) {
  if (typeof value !== "number") throw perfInvalidArg(name, "number", value);
  if (!Number.isInteger(value)) throw pirPerfRangeError(name, "an integer", value);
  const over = max === undefined ? value < min : (value < min || value > max);
  if (over) {
    throw pirPerfRangeError(name, max === undefined ? ">= " + min : ">= " + min + " && <= " + max, value);
  }
  return value;
}

function pirPerfValidateObject(value, name) {
  if (value === null || (typeof value !== "object" && typeof value !== "function")) {
    throw perfInvalidArg(name, "object", value);
  }
  return value;
}

function pirPerfValidateBoolean(value, name) {
  if (typeof value !== "boolean") throw perfInvalidArg(name, "boolean", value);
  return value;
}

/// lib/internal/histogram.js createHistogram(): validateObject(options), then
/// lowest (default 1), highest (default MAX_SAFE_INTEGER) and figures
/// (default 3) — integer ranges, `highest >= 2 * lowest`.
function pirHistogramState(options) {
  const opts = options === undefined ? {} : pirPerfValidateObject(options, "options");
  const lowest = opts.lowest === undefined ? 1 : opts.lowest;
  const highest = opts.highest === undefined ? Number.MAX_SAFE_INTEGER : opts.highest;
  const figures = opts.figures === undefined ? 3 : opts.figures;
  if (typeof lowest !== "bigint") {
    pirPerfValidateInteger(lowest, "options.lowest", 1, Number.MAX_SAFE_INTEGER);
  }
  if (typeof highest !== "bigint") {
    pirPerfValidateInteger(highest, "options.highest", 2 * Number(lowest), Number.MAX_SAFE_INTEGER);
  } else if (highest < 2n * BigInt(lowest)) {
    throw pirPerfRangeError("options.highest", ">= " + (2n * BigInt(lowest)) + "n", highest);
  }
  pirPerfValidateInteger(figures, "options.figures", 1, 5);
  return {
    counts: new Map(),
    count: 0,
    min: null,
    max: null,
    exceeds: 0,
    lowest: BigInt(lowest),
    highest: BigInt(highest),
  };
}

/// HDR's record: a value outside [lowest, highest] is *not* stored but counted
/// as `exceeds` (node: `if (!hdr_record_value(...)) exceeds_++`).
function pirHistogramRecord(state, value) {
  if (value < state.lowest || value > state.highest) {
    state.exceeds += 1;
    return;
  }
  const seen = state.counts.get(value);
  state.counts.set(value, seen === undefined ? 1 : seen + 1);
  state.count += 1;
  if (state.min === null || value < state.min) state.min = value;
  if (state.max === null || value > state.max) state.max = value;
}

/// Ascending [value, count] pairs (the HDR counts array, exactly counted).
function pirHistogramBuckets(state) {
  const out = [];
  for (const entry of state.counts) out.push(entry);
  out.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return out;
}

function pirHistogramMean(state) {
  if (state.count === 0) return NaN;
  let sum = 0;
  for (const entry of state.counts) sum += Number(entry[0]) * entry[1];
  return sum / state.count;
}

/// Population standard deviation over the recorded values (hdr_stddev), with
/// NaN for an empty histogram — the two shapes the upstream test pins.
function pirHistogramStddev(state) {
  if (state.count === 0) return NaN;
  const mean = pirHistogramMean(state);
  let acc = 0;
  for (const entry of state.counts) {
    const delta = Number(entry[0]) - mean;
    acc += delta * delta * entry[1];
  }
  const variance = acc / state.count;
  return variance > 0 ? Math.sqrt(variance) : 0;
}

/// hdr_value_at_percentile over exact counts: round-half-up rank, clamped to
/// at least 1, then the value that rank falls on (0 for an empty histogram,
/// like hdr_value_at_percentile's get_value_from_idx_up_to_count failure path).
function pirHistogramValueAtPercentile(state, percentile) {
  if (state.count === 0) return 0;
  const requested = percentile < 100 ? percentile : 100;
  let rank = Math.trunc((requested / 100) * state.count + 0.5);
  if (rank < 1) rank = 1;
  let cumulative = 0;
  for (const entry of pirHistogramBuckets(state)) {
    cumulative += entry[1];
    if (cumulative >= rank) return entry[0];
  }
  return state.max === null ? 0n : state.max;
}

/// The next HDR percentile tick: `p += 100 / (ticks_per_half_distance *
/// 2^(floor(log2(100 / (100 - p))) + 1))` with ticks_per_half_distance = 1.
function pirPercentileStep(percentile) {
  const temp = Math.floor(Math.log(100 / (100 - percentile)) / Math.LN2) + 1;
  return 100 / Math.pow(2, temp);
}

/// HDR's percentile iterator over the exact counts (see the header comment).
/// `map` is filled in place — node hands back one reused Map per histogram.
function pirHistogramFillPercentiles(state, map, asBigInt) {
  const put = (key, value) => { map.set(key, asBigInt ? value : Number(value)); };
  if (state.count === 0) {
    put(100, 0n);
    return;
  }
  const buckets = pirHistogramBuckets(state);
  const total = state.count;
  const last = buckets[buckets.length - 1][0];
  let index = -1;
  let cumulative = 0;
  let tick = 0;
  let pending = true;
  for (;;) {
    // HDR checks the "all samples counted" case at the top of every step: the
    // iteration then emits exactly one (100, max) entry and stops.
    if (cumulative >= total) {
      put(100, last);
      return;
    }
    if (pending) {
      index += 1;
      if (index >= buckets.length) return;
      cumulative += buckets[index][1];
      pending = false;
    }
    const current = (100 * cumulative) / total;
    if (buckets[index][1] !== 0 && tick <= current) {
      put(tick, buckets[index][0]);
      tick += pirPercentileStep(tick);
      continue;
    }
    pending = true;
  }
}

class Histogram {
  constructor(pirSkip, pirState) {
    if (pirSkip !== kPirHistogramSkipThrow) throw pirIllegalConstructor();
    Object.defineProperty(this, kPirHistogramMap, {
      value: new Map(), writable: true, enumerable: false, configurable: true,
    });
    Object.defineProperty(this, kPirHistogramState, {
      value: pirState, writable: true, enumerable: false, configurable: true,
    });
  }

  [kInspect](depth, options) {
    // depth < 0: report the object itself so the caller renders
    // `[RecordableHistogram]` (util.inspect(h, { depth: -1 })).
    if (depth < 0) return this;
    const opts = Object.assign({}, options, {
      depth: options.depth === null || options.depth === undefined ? null : options.depth - 1,
    });
    return "Histogram " + pirPerfInspect({
      min: this.min,
      max: this.max,
      mean: this.mean,
      exceeds: this.exceeds,
      stddev: this.stddev,
      count: this.count,
      percentiles: this.percentiles,
    }, opts);
  }

  get count() { return pirHistogramStateOf(this, "Histogram").count; }
  get countBigInt() { return BigInt(pirHistogramStateOf(this, "Histogram").count); }

  get min() {
    const state = pirHistogramStateOf(this, "Histogram");
    return Number(state.min === null ? PIR_HISTOGRAM_I64_MAX : state.min);
  }
  get minBigInt() {
    const state = pirHistogramStateOf(this, "Histogram");
    return state.min === null ? PIR_HISTOGRAM_I64_MAX : state.min;
  }
  get max() {
    const state = pirHistogramStateOf(this, "Histogram");
    return Number(state.max === null ? 0n : state.max);
  }
  get maxBigInt() {
    const state = pirHistogramStateOf(this, "Histogram");
    return state.max === null ? 0n : state.max;
  }
  get exceeds() { return pirHistogramStateOf(this, "Histogram").exceeds; }
  get exceedsBigInt() { return BigInt(pirHistogramStateOf(this, "Histogram").exceeds); }
  get mean() { return pirHistogramMean(pirHistogramStateOf(this, "Histogram")); }
  get stddev() { return pirHistogramStddev(pirHistogramStateOf(this, "Histogram")); }

  percentile(percentile) {
    const state = pirHistogramStateOf(this, "Histogram");
    if (typeof percentile !== "number") throw perfInvalidArg("percentile", "number", percentile);
    if (Number.isNaN(percentile) || percentile <= 0 || percentile > 100) {
      throw pirPerfRangeError("percentile", "> 0 && <= 100", percentile);
    }
    return Number(pirHistogramValueAtPercentile(state, percentile));
  }

  percentileBigInt(percentile) {
    const state = pirHistogramStateOf(this, "Histogram");
    if (typeof percentile !== "number") throw perfInvalidArg("percentile", "number", percentile);
    if (Number.isNaN(percentile) || percentile <= 0 || percentile > 100) {
      throw pirPerfRangeError("percentile", "> 0 && <= 100", percentile);
    }
    return pirHistogramValueAtPercentile(state, percentile);
  }

  get percentiles() {
    const map = this[kPirHistogramMap];
    map.clear();
    pirHistogramFillPercentiles(pirHistogramStateOf(this, "Histogram"), map, false);
    return map;
  }

  get percentilesBigInt() {
    const map = this[kPirHistogramMap];
    map.clear();
    pirHistogramFillPercentiles(pirHistogramStateOf(this, "Histogram"), map, true);
    return map;
  }

  reset() {
    const state = pirHistogramStateOf(this, "Histogram");
    state.counts.clear();
    state.count = 0;
    state.min = null;
    state.max = null;
    state.exceeds = 0;
  }

  toJSON() {
    const percentiles = {};
    for (const entry of this.percentiles) percentiles[entry[0]] = entry[1];
    return {
      count: this.count,
      min: this.min,
      max: this.max,
      mean: this.mean,
      exceeds: this.exceeds,
      stddev: this.stddev,
      percentiles: percentiles,
    };
  }
}

class RecordableHistogram extends Histogram {
  constructor(pirSkip, pirState) {
    if (pirSkip !== kPirHistogramSkipThrow) throw pirIllegalConstructor();
    super(pirSkip, pirState);
    Object.defineProperty(this, kPirHistogramRecordable, {
      value: true, writable: false, enumerable: false, configurable: false,
    });
  }

  record(value) {
    if (this[kPirHistogramRecordable] !== true) throw pirInvalidThis("RecordableHistogram");
    const state = this[kPirHistogramState];
    if (typeof value === "bigint") {
      // node's bigint path skips validateInteger entirely (the int64 handle
      // takes any bigint).
      pirHistogramRecord(state, value);
      return;
    }
    pirPerfValidateInteger(value, "val", 1, Number.MAX_SAFE_INTEGER);
    pirHistogramRecord(state, BigInt(value));
  }

  recordDelta() {
    if (this[kPirHistogramRecordable] !== true) throw pirInvalidThis("RecordableHistogram");
    const state = this[kPirHistogramState];
    // Histogram::RecordDelta: `prev_` starts at 0, so the first call only
    // primes it; later calls record the elapsed nanoseconds.
    const now = BigInt(Math.round(perfNow() * 1e6));
    if (state.deltaPrev !== undefined && state.deltaPrev !== 0n) {
      pirHistogramRecord(state, now - state.deltaPrev);
    }
    state.deltaPrev = now;
  }

  add(other) {
    if (this[kPirHistogramRecordable] !== true) throw pirInvalidThis("RecordableHistogram");
    if (other === null || other === undefined || other[kPirHistogramRecordable] !== true) {
      throw pirInvalidInstance("other", "RecordableHistogram", other);
    }
    const state = this[kPirHistogramState];
    const source = other[kPirHistogramState];
    for (const entry of source.counts) {
      for (let i = 0; i < entry[1]; i += 1) pirHistogramRecord(state, entry[0]);
    }
    state.exceeds += source.exceeds;
  }
}

/// node's createHistogram(): a RecordableHistogram over a fresh HDR histogram.
function createHistogram(options) {
  pirInstallHistogramTransfer();
  return pirNewHistogram(pirHistogramState(options), true);
}

/// The internal construction path (node's ClonedHistogram/
/// ClonedRecordableHistogram): the public constructors stay illegal.
function pirNewHistogram(state, recordable) {
  if (!recordable) return new Histogram(kPirHistogramSkipThrow, state);
  const histogram = new RecordableHistogram(kPirHistogramSkipThrow, state);
  // node's ClonedRecordableHistogram assigns an *own enumerable* `constructor`
  // (that own key is also what keeps util.inspect(h, { depth: -1 }) out of the
  // empty-object shortcut, so it reports `[RecordableHistogram]` — the form
  // test-perf-hooks-histogram pins).
  histogram.constructor = RecordableHistogram;
  return histogram;
}

/// node's postMessage of a histogram transfers the *handle*: the receiver gets
/// a different object of the clone class backed by the same state, which is why
/// `data.record(2n)` on the receiving side moves the sender's `max` and why an
/// IntervalHistogram clone has no enable/disable
/// (test-perf-hooks-histogram pins both). pir's global MessagePort
/// (webcompat_globals) is a same-realm pass-through that delivers the object
/// itself, so histogram values are replaced by their clone view on the way in;
/// every non-histogram value still passes through untouched.
function pirHistogramTransferView(histogram) {
  const state = histogram[kPirHistogramState];
  if (state === undefined) return histogram;
  return pirNewHistogram(state, histogram[kPirHistogramRecordable] === true);
}

function pirInstallHistogramTransfer() {
  const Port = globalThis.MessagePort;
  if (typeof Port !== "function" || Port.prototype === undefined) return;
  const original = Port.prototype.postMessage;
  if (typeof original !== "function") return;
  // Idempotent: the builtin shim may be re-evaluated (PI_EXT_RELOAD) or the
  // port surface may appear after this module.
  if (original.__pirHistogramTransfer === true) return;
  const wrapper = function postMessage(value, transferList) {
    return original.call(this, value instanceof Histogram ? pirHistogramTransferView(value) : value, transferList);
  };
  Object.defineProperty(wrapper, "__pirHistogramTransfer", { value: true });
  Object.defineProperty(wrapper, "length", { value: original.length });
  Object.defineProperty(wrapper, "name", { value: "postMessage" });
  Port.prototype.postMessage = wrapper;
}

// ---------------------------------------------------------------------------
// Event-loop delay histogram
// ---------------------------------------------------------------------------
// node's ELDHistogram runs a libuv timer with the configured resolution and
// records how late each activation is. Two pir facts shape the port:
//   * there is no way to unref a timer, and a pending timer keeps `pir js`
//     alive until it fires (node's ELD timer is unref'ed, so the process still
//     exits), and
//   * the histogram has no callback the host could drive.
// So the sampler is observation-driven: while enabled, every histogram read
// measures the lateness of the scheduled activation and records it, then
// schedules the next one. The numbers are real wall-clock measurements (min >
// 0 for any observed period) and the process can still exit; a program that
// never reads the histogram sees no samples until it does.
class ELDHistogram extends Histogram {
  constructor(pirSkip, pirState, pirSampler) {
    super(pirSkip, pirState);
    Object.defineProperty(this, kPirHistogramEld, {
      value: pirSampler, writable: false, enumerable: false, configurable: false,
    });
  }

  enable() {
    const sampler = pirHistogramEldOf(this);
    if (sampler.enabled) return false;
    sampler.enabled = true;
    sampler.expected = perfNow() + sampler.resolution;
    return true;
  }

  disable() {
    const sampler = pirHistogramEldOf(this);
    if (!sampler.enabled) return false;
    sampler.enabled = false;
    return true;
  }

  [Symbol.dispose]() {
    this.disable();
  }
}

function pirHistogramEldOf(self) {
  const sampler = self === null || self === undefined ? undefined : self[kPirHistogramEld];
  if (sampler === undefined) throw pirInvalidThis("ELDHistogram");
  return sampler;
}

function pirELDSample(sampler) {
  if (sampler === undefined || !sampler.enabled) return;
  const now = perfNow();
  const delayMs = now - sampler.expected;
  if (delayMs < 0) return; // the scheduled activation has not come due yet
  // One sample per observation: the measured lateness, floored at 1ns (a timer
  // activation is never exactly on time, and Histogram values are >= 1).
  pirHistogramRecord(sampler.state, BigInt(Math.max(1, Math.round(delayMs * 1e6))));
  sampler.expected = now + sampler.resolution;
}

function pirHistogramStateOf(self, expected) {
  const state = self === null || self === undefined ? undefined : self[kPirHistogramState];
  if (state === undefined) throw pirInvalidThis(expected);
  pirELDSample(self[kPirHistogramEld]);
  return state;
}

/// lib/internal/perf/event_loop_delay.js monitorEventLoopDelay().
function monitorEventLoopDelay(options) {
  const opts = options === undefined ? {} : pirPerfValidateObject(options, "options");
  const resolution = opts.resolution === undefined ? 10 : opts.resolution;
  const samplePerIteration = opts.samplePerIteration === undefined ? false : opts.samplePerIteration;
  pirPerfValidateBoolean(samplePerIteration, "options.samplePerIteration");
  pirPerfValidateInteger(resolution, "options.resolution", 1);
  pirInstallHistogramTransfer();
  const state = pirHistogramState(undefined);
  const sampler = { enabled: false, resolution: resolution, expected: 0, state: state };
  // `samplePerIteration` selects node's per-loop-iteration sampling mode; the
  // observation-driven sampler below is iteration-agnostic, so the flag is
  // validated and otherwise unused.
  void samplePerIteration;
  return new ELDHistogram(kPirHistogramSkipThrow, state, sampler);
}

class Performance {
  constructor() {
    const error = new TypeError("Illegal constructor");
    error.code = "ERR_ILLEGAL_CONSTRUCTOR";
    throw error;
  }
  get timeOrigin() {
    return perfOrigin;
  }
  get nodeTiming() {
    return perfNodeTiming;
  }
  now() {
    return perfNow();
  }
  getEntries() {
    const all = [...perfMarks.values(), ...perfMeasures];
    return all.sort((a, b) => a.startTime - b.startTime);
  }
  getEntriesByType(type) {
    if (type === undefined) throw perfMissingArg("type");
    const wanted = perfString(type);
    if (wanted === "mark") return [...perfMarks.values()];
    if (wanted === "measure") return perfMeasures.slice();
    return [];
  }
  getEntriesByName(name, type) {
    if (name === undefined) throw perfMissingArg("name");
    // Filtered directly instead of via getEntries(): the many-marks upstream
    // test builds a million marks, and a merge+sort on every lookup would make
    // that quadratic.
    const wanted = perfString(name);
    const out = [];
    for (const mark of perfMarks.values()) {
      if (mark.name === wanted && (type === undefined || mark.entryType === type)) out.push(mark);
    }
    for (const measure of perfMeasures) {
      if (measure.name === wanted && (type === undefined || measure.entryType === type)) out.push(measure);
    }
    return out;
  }
  mark(name, options) {
    // Same-name re-marking replaces the record (Node keeps one per name).
    const mark = options === undefined
      ? perfCreateMark(perfString(name), perfNow(), null)
      : new PerformanceMark(name, options);
    perfMarks.set(mark.name, mark);
    perfEmit(mark);
    return mark;
  }
  measure(name, startOrOptions, endMark) {
    if (name === undefined) throw perfMissingArg("name");
    if (typeof name !== "string") throw perfInvalidArg("name", "string", name);
    let startRef;
    let endRef;
    let explicitDuration;
    let detail = null;
    if (startOrOptions !== undefined && startOrOptions !== null && typeof startOrOptions === "object") {
      if (startOrOptions.start !== undefined) startRef = perfString(startOrOptions.start);
      if (startOrOptions.end !== undefined) endRef = perfString(startOrOptions.end);
      // Node only honours a numeric duration; anything else falls back to
      // `end - start` rather than raising.
      if (typeof startOrOptions.duration === "number") explicitDuration = startOrOptions.duration;
      if (startOrOptions.detail !== undefined) detail = perfClone(startOrOptions.detail);
    } else {
      if (startOrOptions !== undefined) startRef = perfString(startOrOptions);
      if (endMark !== undefined) endRef = perfString(endMark);
    }
    // User Timing L3: an omitted start is time 0 on the timeline, an omitted
    // end is "now"; an explicit duration displaces whichever side is missing.
    let startTime = startRef === undefined ? 0 : perfLookupTime(startRef);
    let endTime = endRef === undefined ? perfNow() : perfLookupTime(endRef);
    let duration;
    if (explicitDuration !== undefined) {
      if (startRef === undefined && endRef !== undefined) {
        startTime = endTime - explicitDuration;
      } else {
        endTime = startTime + explicitDuration;
      }
      duration = explicitDuration;
    } else {
      duration = endTime - startTime;
    }
    const measure = new PerformanceMeasure(name, startTime, duration, detail);
    perfMeasures.push(measure);
    perfEmit(measure);
    return measure;
  }
  clearMarks(name) {
    if (name === undefined) perfMarks.clear();
    else perfMarks.delete(perfString(name));
  }
  clearMeasures(name) {
    if (name === undefined) {
      perfMeasures.length = 0;
    } else {
      const wanted = perfString(name);
      for (let index = perfMeasures.length - 1; index >= 0; index -= 1) {
        if (perfMeasures[index].name === wanted) perfMeasures.splice(index, 1);
      }
    }
  }
  clearResourceTimings() {}
  setResourceTimingBufferSize(_size) {}
  markResourceTiming(_name) {}
  onresourcetimingbufferfull() {
    return null;
  }
  eventLoopUtilization(utilization1, utilization2) {
    return eventLoopUtilization(utilization1, utilization2);
  }
  toJSON() {
    return {
      nodeTiming: perfNodeTiming,
      timeOrigin: perfOrigin,
      eventLoopUtilization: eventLoopUtilization(),
    };
  }
}
Object.defineProperty(Performance.prototype, Symbol.toStringTag, {
  configurable: true, enumerable: false, writable: false, value: "Performance",
});

class PerformanceObserverEntryList {
  constructor(entries) {
    this._entries = entries;
  }
  getEntries() {
    return this._entries.slice();
  }
  getEntriesByType(type) {
    return this._entries.filter((entry) => entry.entryType === type);
  }
  getEntriesByName(name, type) {
    return this._entries.filter((entry) => entry.name === name && (type === undefined || entry.entryType === type));
  }
}

class PerformanceObserver {
  constructor(callback) {
    if (typeof callback !== "function") throw perfInvalidArg("callback", "function", callback);
    this._callback = callback;
    this._buffer = [];
    this._types = [];
    this._active = false;
  }
  observe(options) {
    const types = [];
    if (options && Array.isArray(options.entryTypes)) {
      for (const type of options.entryTypes) types.push(perfString(type));
    } else if (options && options.type !== undefined) {
      types.push(perfString(options.type));
    }
    this._types = types;
    this._active = true;
    perfObservers.add(this);
  }
  disconnect() {
    this._active = false;
    this._buffer = [];
    perfObservers.delete(this);
  }
  takeRecords() {
    const records = this._buffer;
    this._buffer = [];
    return records;
  }
}
// Node's own list (v24): the entry types the runtime can report.
PerformanceObserver.supportedEntryTypes = [
  "dns", "function", "gc", "http", "http2", "mark", "measure", "net", "resource",
];

function eventLoopUtilization(_utilization1, _utilization2) {
  return { idle: 0, active: 0, utilization: 0 };
}

/// `timerify`: the real wrapper Node ships — the entry is emitted whether the
/// call returns or throws, and construction delegates to the wrapped function
/// so `new timerified() instanceof Original` still holds.
function timerify(fn, options) {
  if (typeof fn !== "function") throw perfInvalidArg("fn", "function", fn);
  const name = options && options.name !== undefined
    ? perfString(options.name)
    : (typeof fn.name === "string" ? fn.name : "");
  const timerified = function (...args) {
    const startTime = perfNow();
    try {
      if (new.target !== undefined) {
        return Reflect.construct(fn, args, new.target === timerified ? fn : new.target);
      }
      return Reflect.apply(fn, this, args);
    } finally {
      const entry = new PerformanceEntry(name, "function", startTime, perfNow() - startTime);
      entry.detail = args;
      perfEmit(entry);
    }
  };
  Object.defineProperty(timerified, "name", { value: "timerified " + name, configurable: true });
  return timerified;
}

// `require("perf_hooks").timerify === performance.timerify`, like Node.
Object.defineProperty(Performance.prototype, "timerify", {
  configurable: true, enumerable: false, writable: true, value: timerify,
});

// The shared global: the engine installs `globalThis.performance` before the
// shims run, so this is the very object extensions already hold. Reparenting it
// onto Performance.prototype installs the timeline API on that same object —
// never a replacement — so `require("perf_hooks").performance` and
// `globalThis.performance` cannot diverge. Its own `now`/`timeOrigin` (which
// the engine defines non-configurable) stay, and `perfNow()` reads the same
// clock, so the two surfaces agree.
const performance = globalThis.performance || (globalThis.performance = {});
Object.setPrototypeOf(performance, Performance.prototype);

const constants = Object.freeze({});

// Restore node's observable postMessage shape for histogram values (see
// pirInstallHistogramTransfer); a no-op when the port surface is absent.
pirInstallHistogramTransfer();

export { performance, Performance, PerformanceEntry, PerformanceMark, PerformanceMeasure, PerformanceObserver, PerformanceObserverEntryList, PerformanceResourceTiming, constants, createHistogram, eventLoopUtilization, monitorEventLoopDelay, timerify };
export default {
  performance: performance,
  Performance: Performance,
  PerformanceEntry: PerformanceEntry,
  PerformanceMark: PerformanceMark,
  PerformanceMeasure: PerformanceMeasure,
  PerformanceObserver: PerformanceObserver,
  PerformanceObserverEntryList: PerformanceObserverEntryList,
  PerformanceResourceTiming: PerformanceResourceTiming,
  constants: constants,
  createHistogram: createHistogram,
  eventLoopUtilization: eventLoopUtilization,
  monitorEventLoopDelay: monitorEventLoopDelay,
  timerify: timerify,
};
"#;

/// `node:process` module: the bootstrapped global plus named fields. `env` is
/// the bootstrap snapshot; `cwd` is host-backed and stays live. There is
/// deliberately no `chdir()` (the host never moves the runtime cwd).
static PROCESS_SHIM: Shim = Shim::new(
    "process_shim.js.txt",
    include_bytes!("../../../assets/shims/process_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// buffer
// ---------------------------------------------------------------------------

/// `node:buffer` module: a thin re-export of the global `Buffer` backfill
/// (the implementation lives in [`BUFFER_GLOBAL`] so fs/crypto share it),
/// plus the completion surface: `atob`/`btoa` over the shared base64 codec,
/// the `Blob`/`File` value holders and the misc constants/checks.
const BUFFER_SHIM: &str = r#"
const Buffer = globalThis.Buffer;
const kMaxLength = 9007199254740991;
const kStringMaxLength = 536870888;
const INSPECT_MAX_BYTES = 50;
const constants = { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: kStringMaxLength };

function atob(input) {
  const text = String(input).replace(/[\t\n\f\r ]/g, "");
  if (text.length % 4 === 1 || /[^A-Za-z0-9+/=]/.test(text)) {
    const error = new Error("Invalid base64 input");
    error.code = "ERR_INVALID_ARG_VALUE";
    throw error;
  }
  return Buffer.from(text, "base64").toString("latin1");
}

function btoa(input) {
  return Buffer.from(String(input), "latin1").toString("base64");
}

function isAscii(value) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(String(value), "utf8");
  for (let i = 0; i < bytes.length; i++) {
    if (bytes[i] > 0x7f) return false;
  }
  return true;
}

function isUtf8(value) {
  if (!Buffer.isBuffer(value)) return false;
  let i = 0;
  while (i < value.length) {
    const byte = value[i];
    if (byte <= 0x7f) {
      i += 1;
    } else if (byte >= 0xc2 && byte <= 0xdf) {
      if (i + 1 >= value.length || (value[i + 1] & 0xc0) !== 0x80) return false;
      i += 2;
    } else if (byte >= 0xe0 && byte <= 0xef) {
      if (i + 2 >= value.length || (value[i + 1] & 0xc0) !== 0x80 || (value[i + 2] & 0xc0) !== 0x80) return false;
      if (byte === 0xe0 && value[i + 1] < 0xa0) return false;
      if (byte === 0xed && value[i + 1] > 0x9f) return false;
      i += 3;
    } else if (byte >= 0xf0 && byte <= 0xf4) {
      if (i + 3 >= value.length || (value[i + 1] & 0xc0) !== 0x80 || (value[i + 2] & 0xc0) !== 0x80 || (value[i + 3] & 0xc0) !== 0x80) return false;
      if (byte === 0xf0 && value[i + 1] < 0x90) return false;
      if (byte === 0xf4 && value[i + 1] > 0x8f) return false;
      i += 4;
    } else {
      return false;
    }
  }
  return true;
}

// Minimal Blob value holder: stores the concatenated bytes; text()/
// arrayBuffer()/slice() work, streaming does not (no host Blob store).
class Blob {
  constructor(parts, options) {
    const chunks = [];
    let byteLength = 0;
    if (parts !== undefined && parts !== null) {
      for (const part of parts) {
        if (part === undefined || part === null) continue;
        const bytes = Buffer.isBuffer(part)
          ? part
          : typeof part === "string"
            ? Buffer.from(part, "utf8")
            : part instanceof ArrayBuffer
              ? Buffer.from(new Uint8Array(part))
              : ArrayBuffer.isView(part)
                ? Buffer.from(part.buffer, part.byteOffset, part.byteLength)
                : Buffer.from(String(part), "utf8");
        chunks.push(bytes);
        byteLength += bytes.length;
      }
    }
    this._bytes = Buffer.concat(chunks, byteLength);
    this.type = options && typeof options.type === "string" ? options.type.toLowerCase() : "";
    this.size = byteLength;
  }
  arrayBuffer() {
    const copy = new ArrayBuffer(this._bytes.length);
    new Uint8Array(copy).set(this._bytes);
    return Promise.resolve(copy);
  }
  text() {
    return Promise.resolve(this._bytes.toString("utf8"));
  }
  slice(start, end, contentType) {
    const from = start === undefined ? 0 : start < 0 ? Math.max(0, this._bytes.length + start) : Math.min(start, this._bytes.length);
    const to = end === undefined ? this._bytes.length : end < 0 ? Math.max(0, this._bytes.length + end) : Math.min(end, this._bytes.length);
    const blob = new Blob([], { type: contentType });
    blob._bytes = this._bytes.subarray(from, Math.max(from, to));
    blob.size = blob._bytes.length;
    return blob;
  }
  stream() {
    const error = new Error("blob.stream() is not supported by the pir extension runtime");
    error.code = "ERR_NOT_SUPPORTED";
    throw error;
  }
}

// Node's `File` is a Blob with a name and a timestamp.
class File extends Blob {
  constructor(parts, name, options) {
    super(parts, options);
    this.name = String(name);
    this.lastModified = options && typeof options.lastModified === "number" ? options.lastModified : Date.now();
  }
}

// Deprecated size constructor; kept for load compatibility.
function SlowBuffer(size) {
  return Buffer.alloc(size);
}

function resolveObjectURL(_url) {
  // No object-URL registry exists in this runtime.
  return undefined;
}

function transcode(_source, _from, _to) {
  const error = new Error("buffer.transcode is not supported by the pir extension runtime (only utf8/hex/base64/latin1/utf16le codecs exist)");
  error.code = "ERR_NOT_SUPPORTED";
  throw error;
}

const buffer = {
  Buffer: Buffer,
  kMaxLength: kMaxLength,
  kStringMaxLength: kStringMaxLength,
  INSPECT_MAX_BYTES: INSPECT_MAX_BYTES,
  constants: constants,
  atob: atob,
  btoa: btoa,
  isAscii: isAscii,
  isUtf8: isUtf8,
  Blob: Blob,
  File: File,
  SlowBuffer: SlowBuffer,
  resolveObjectURL: resolveObjectURL,
  transcode: transcode,
};

export { Buffer, kMaxLength, kStringMaxLength, INSPECT_MAX_BYTES, constants, atob, btoa, isAscii, isUtf8, Blob, File, SlowBuffer, resolveObjectURL, transcode };
export default buffer;
"#;

// ---------------------------------------------------------------------------
// fs
// ---------------------------------------------------------------------------

/// Node `fs` shim: the sync surface (including the fd family —
/// `openSync`/`readSync`/`writeSync`/`closeSync`/`fstatSync`/`fsyncSync`/
/// `fchmodSync` — `chmod`/`cp`/`mkdtemp`/`rmdir`/`symlink` and the
/// `FileHandle`/`Dir` promise types), `(err, res)` callback forms
/// (dispatched on a microtask) and `fs.promises`, all routed through the
/// sandboxed `__pir_host.node("fs", ...)` ops. Byte data crosses the
/// boundary as base64 and decodes into the shared global Buffer.
static FS_SHIM: Shim = Shim::new(
    "fs_shim.js.txt",
    include_bytes!("../../../assets/shims/fs_shim.js.txt.deflate"),
);

/// `node:fs/promises` module: the promise surface only, sourced from the fs
/// shim so both entry points share one implementation.
static FS_PROMISES_SHIM: Shim = Shim::new(
    "fs_promises_shim.js.txt",
    include_bytes!("../../../assets/shims/fs_promises_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// os
// ---------------------------------------------------------------------------

/// Node `os` shim: platform constants from the compile-time cfg values plus
/// host ops for the identity fields. `release()` is intentionally empty (the
/// host exposes no uname) and the memory numbers are stable pseudo values
/// (the sandbox exposes no real meminfo); both are documented here rather
/// than fabricated per call.
static OS_SHIM: Shim = Shim::new(
    "os_shim.js.txt",
    include_bytes!("../../../assets/shims/os_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// crypto
// ---------------------------------------------------------------------------

/// Node `crypto` shim: createHash/createHmac accumulate raw chunks and hit
/// the host digest ops once at `digest()`; randomBytes/randomUUID come from
/// the host entropy source. `webcrypto.subtle.digest` is the minimal
/// Promise<ArrayBuffer> surface.
static CRYPTO_SHIM: Shim = Shim::new(
    "crypto_shim.js.txt",
    include_bytes!("../../../assets/shims/crypto_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// child_process
// ---------------------------------------------------------------------------

/// Node `child_process` shim over the `child` host ops: `spawn` (a polling
/// ChildProcess backed by the host process registry), `exec`/`execFile`
/// (buffered callback form) and the blocking `spawnSync`/`execSync`/
/// `execFileSync`. Documented limits: `stdio`/`detached`/IPC are not modeled
/// (a child always gets piped stdin/stdout/stderr), `fork` throws, and
/// `maxBuffer` is not enforced (the host caps each stream at 8 MiB and sets
/// the `truncated` marker instead).
static CHILD_PROCESS_SHIM: Shim = Shim::new(
    "child_process_shim.js.txt",
    include_bytes!("../../../assets/shims/child_process_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------

/// Minimal `stream` shim: enough for the common `Readable`/`Writable`/
/// `PassThrough`/`Duplex` import shapes to construct and move data. LIMITS
/// (documented, not accidental): no backpressure or `highWaterMark`
/// accounting, no object mode, `read()` always returns null, and `_read` is
/// never called — this is minimal padding so `import ... from "node:stream"`
/// resolves and simple data plumbing works, not a faithful stream
/// implementation. The promise helpers live in `stream/promises`.
static STREAM_SHIM: Shim = Shim::new(
    "stream_shim.js.txt",
    include_bytes!("../../../assets/shims/stream_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// stream/promises
// ---------------------------------------------------------------------------

/// `node:stream/promises` over the minimal stream pad. LIMIT (documented):
/// `finished` resolves on the FIRST terminal signal — the pad's `readableEnded`/
/// `writableEnded`/`finished` flags or the `end`/`finish`/`close` events — and
/// rejects on `error`; real Node waits for both directions unless options say
/// otherwise. `pipeline` chains `pipe()` and settles on the last stream (or
/// any stream's `error`).
static STREAM_PROMISES_SHIM: Shim = Shim::new(
    "stream_promises_shim.js.txt",
    include_bytes!("../../../assets/shims/stream_promises_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// zlib
// ---------------------------------------------------------------------------

/// Node `zlib` shim: `gzip`/`gunzip`/`deflate`/`inflate` (+ the `unzip` alias)
/// over the host's flate2 codec, base64 across the boundary. `*Sync` returns a
/// Buffer and throws a `Z_DATA_ERROR`-coded error on bad input; the callback
/// forms defer on a microtask.
static ZLIB_SHIM: Shim = Shim::new(
    "zlib_shim.js.txt",
    include_bytes!("../../../assets/shims/zlib_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// module
// ---------------------------------------------------------------------------

/// `node:module` shim (phase 4a subset): `createRequire` returns the dir-bound
/// CommonJS `require` built by the global CJS runtime (accepts a file URL
/// string, a `URL` instance or a plain path — Node's input set), plus the
/// supported-builtin list the bootstrap populated.
static MODULE_SHIM: Shim = Shim::new(
    "module_shim.js.txt",
    include_bytes!("../../../assets/shims/module_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// worker_threads
// ---------------------------------------------------------------------------

/// `node:worker_threads` real in-process workers (one OS thread + one
/// QuickJS runtime per Worker, host-backed — see
/// `runtime/host_functions/worker_fns.rs`). The main thread keeps
/// `isMainThread: true` with a null `parentPort`; inside a worker the boot
/// overlay (`js/worker_boot.js.txt`) flips the module bindings through the
/// shim's apply hook. Messages cross the boundary as JSON
/// (structured-clone-minimal): plain data passes, functions/symbols/class
/// instances/cycles throw `DataCloneError`.
static WORKER_THREADS_SHIM: Shim = Shim::new(
    "worker_threads_shim.js.txt",
    include_bytes!("../../../assets/shims/worker_threads_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// net
// ---------------------------------------------------------------------------

/// `node:net` minimal-real surface: `isIP`/`isIPv4`/`isIPv6` are the real
/// RFC-shaped checks (0/4/6, same verdicts as Node), and the socket/server
/// entry points are a REAL loopback TCP implementation over 127.0.0.1 —
/// `createServer().listen()` plus `createConnection()`/`connect()` own a real
/// socket pair, with connect/data/end/close/error/timeout events, setEncoding,
/// setNoDelay, ref/unref and the `stream.Duplex` read/write machinery. TLS and
/// Unix-domain sockets are out of tier (clear errors); non-loopback hosts are
/// rejected with `ERR_NOT_SUPPORTED`. See the documented deviations in
/// `js/net_shim.js.txt`.
static NET_SHIM: Shim = Shim::new(
    "net_shim.js.txt",
    include_bytes!("../../../assets/shims/net_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// dns/promises
// ---------------------------------------------------------------------------

/// `node:dns/promises` load-compatible surface (phase 4e): `lookup` is real
/// (host OS resolver through the `dns` host op; `all`/`family` honored,
/// ENOTFOUND on an empty resolution); the `resolve*` family and `reverse`
/// need the c-ares-style query surface the runtime does not host and throw
/// one clear error each.
static DNS_PROMISES_SHIM: Shim = Shim::new(
    "dns_promises_shim.js.txt",
    include_bytes!("../../../assets/shims/dns_promises_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// http / https
// ---------------------------------------------------------------------------

/// `node:http` real client + minimal real server (LLRT tier): request/get
/// (ClientRequest → IncomingMessage with data/end events, redirects NOT
/// followed; connections persist across requests in the host reqwest pool) and
/// a loopback-only `http.Server` (HTTP/1.1 keep-alive, ~5 s idle close) both
/// ride the `__pir_host.http` bridge. See the documented limits in the source.
static HTTP_SHIM: Shim = Shim::new(
    "http_shim.js.txt",
    include_bytes!("../../../assets/shims/http_shim.js.txt.deflate"),
);

/// `node:https`: the client face shares `@node/http` with the https: default
/// protocol (the fetch bridge speaks https natively); the TLS server face
/// (Server/createServer) stays throws-tier.
static HTTPS_SHIM: Shim = Shim::new(
    "https_shim.js.txt",
    include_bytes!("../../../assets/shims/https_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// tls
// ---------------------------------------------------------------------------

/// `node:tls` load-compatible stub (phase 4e): `connect`/`createServer` throw
/// on call (no TLS stack is hosted); `TLSSocket` is an inert skeleton so
/// `instanceof`-style load paths link. ws references `tls.connect` only at
/// call time.
const TLS_SHIM: &str = r#"
const DEFAULT_CIPHERS = "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA";
const getCiphers = () => ["TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256", "TLS_AES_128_GCM_SHA256", "ECDHE-RSA-AES128-GCM-SHA256", "ECDHE-ECDSA-AES128-GCM-SHA256", "ECDHE-RSA-AES256-GCM-SHA384", "ECDHE-ECDSA-AES256-GCM-SHA384", "DHE-RSA-AES128-GCM-SHA256", "ECDHE-RSA-AES128-SHA256", "DHE-RSA-AES128-SHA256", "ECDHE-RSA-AES256-SHA384", "DHE-RSA-AES256-SHA384", "ECDHE-RSA-AES256-SHA256", "DHE-RSA-AES256-SHA256", "HIGH", "!aNULL", "!eNULL", "!EXPORT", "!DES", "!RC4", "!MD5", "!PSK", "!SRP", "!CAMELLIA"];

// The runtime hosts no TLS stack: connection entry points throw one clear
// error each; tables/skeletons exist so instanceof/link paths succeed.
function pirTlsUnsupported(api) {
  const error = new Error("node:tls." + api + " is not supported by the pir extension runtime");
  error.code = "ERR_NOT_SUPPORTED";
  return error;
}

const DEFAULT_MIN_VERSION = "TLSv1.2";
const DEFAULT_MAX_VERSION = "TLSv1.3";
const DEFAULT_ECDH_CURVE = "auto";
const CLIENT_RENEG_LIMIT = 3;
const CLIENT_RENEG_WINDOW = 600;

class TLSSocket {
  constructor(_socket, options) {
    this.options = options;
    this.authorized = false;
    this.encrypted = true;
  }
}

class SecureContext {
  constructor(context) {
    this.context = context === undefined || context === null ? {} : context;
  }
}

class Server {
  constructor(_options, _secureConnectionListener) {}
}

function connect(..._args) {
  throw pirTlsUnsupported("connect");
}

function createServer(..._args) {
  throw pirTlsUnsupported("createServer");
}

function createSecureContext(_details) {
  // The runtime links the value but hosts no certificate stack.
  return new SecureContext({});
}

function checkServerIdentity(hostname, cert) {
  // Minimal CN match: Node's full identity verification needs the
  // certificate parser the runtime does not host.
  const cn = cert && cert.subject && cert.subject.CN;
  if (cn === undefined || cn === hostname) return undefined;
  const error = new Error("Hostname/IP does not match certificate's altnames");
  error.code = "ERR_TLS_CERT_ALTNAME_INVALID";
  error.reason = "Host: " + hostname + ". is not in the cert's altnames";
  error.host = hostname;
  error.cert = cert;
  return error;
}

function convertALPNProtocols(protocols, out) {
  // NPN/ALPN wire format: [len][bytes] per entry.
  if (protocols === undefined || protocols === null) return undefined;
  const list = Array.isArray(protocols) || typeof protocols === "string" ? [].concat(protocols) : [];
  const Buffer = globalThis.Buffer;
  let length = 0;
  const encoded = list.map((entry) => {
    const bytes = Buffer.from(String(entry), "latin1");
    length += 1 + bytes.length;
    return bytes;
  });
  const wire = Buffer.alloc(length);
  let offset = 0;
  for (const bytes of encoded) {
    wire[offset++] = bytes.length;
    bytes.copy(wire, offset);
    offset += bytes.length;
  }
  if (out && typeof out === "object") out.ALPNProtocols = wire;
  return wire;
}

function getCACertificates() {
  return [];
}

function getCertificateCompressionAlgorithms() {
  return [];
}

const rootCertificates = Object.freeze([]);

function setDefaultCACertificates(_certificates) {
  throw pirTlsUnsupported("setDefaultCACertificates");
}

const tls = {
  connect: connect,
  createServer: createServer,
  createSecureContext: createSecureContext,
  checkServerIdentity: checkServerIdentity,
  convertALPNProtocols: convertALPNProtocols,
  getCACertificates: getCACertificates,
  getCertificateCompressionAlgorithms: getCertificateCompressionAlgorithms,
  getCiphers: getCiphers,
  rootCertificates: rootCertificates,
  setDefaultCACertificates: setDefaultCACertificates,
  DEFAULT_CIPHERS: DEFAULT_CIPHERS,
  DEFAULT_MIN_VERSION: DEFAULT_MIN_VERSION,
  DEFAULT_MAX_VERSION: DEFAULT_MAX_VERSION,
  DEFAULT_ECDH_CURVE: DEFAULT_ECDH_CURVE,
  CLIENT_RENEG_LIMIT: CLIENT_RENEG_LIMIT,
  CLIENT_RENEG_WINDOW: CLIENT_RENEG_WINDOW,
  TLSSocket: TLSSocket,
  SecureContext: SecureContext,
  Server: Server,
};

export default tls;
export {
  connect, createServer, createSecureContext, checkServerIdentity, convertALPNProtocols,
  getCACertificates, getCertificateCompressionAlgorithms, getCiphers, rootCertificates,
  setDefaultCACertificates, DEFAULT_CIPHERS, DEFAULT_MIN_VERSION, DEFAULT_MAX_VERSION,
  DEFAULT_ECDH_CURVE, CLIENT_RENEG_LIMIT, CLIENT_RENEG_WINDOW, TLSSocket, SecureContext, Server,
};
"#;

// ---------------------------------------------------------------------------
// sqlite
// ---------------------------------------------------------------------------

/// `node:sqlite` real implementation (sqlite wave): DatabaseSync/StatementSync
/// backed by the host SQLite bridge (rusqlite bundled, FTS5 included). The
/// former throws-tier verdict is retired; the JS body now lives in
/// `js/sqlite_shim.js.txt` (ES2020, host-bridge discipline).
static SQLITE_SHIM: Shim = Shim::new(
    "sqlite_shim.js.txt",
    include_bytes!("../../../assets/shims/sqlite_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// async_hooks
// ---------------------------------------------------------------------------

/// `node:async_hooks` approximation (phase 4e): `AsyncLocalStorage.run` /
/// `getStore` / `enterWith` / `exit` / `disable` share one module-level store
/// slot — the honest single-context model for this runtime (concurrent runs
/// with different stores are last-writer-wins, unlike Node's per-resource
/// tree; documented, not hidden). A thenable return of `run` keeps the store
/// visible until it settles so awaited bodies observe it. `AsyncResource` is
/// a minimal synchronous skeleton; the async-id/hook surface reports 0 /
/// stays inert because no resource tracking exists here.
static ASYNC_HOOKS_SHIM: Shim = Shim::new(
    "async_hooks_shim.js.txt",
    include_bytes!("../../../assets/shims/async_hooks_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// P0b surface wave: the remaining builtins
// ---------------------------------------------------------------------------

/// `node:dns`: the callback form over the promise surface (real `lookup`
/// through the host resolver; the resolve* family throws one clear error
/// each) plus the real c-ares/flag constant tables.
static DNS_SHIM: Shim = Shim::new(
    "dns_shim.js.txt",
    include_bytes!("../../../assets/shims/dns_shim.js.txt.deflate"),
);

/// `node:tty`: load-compatible surface; `isatty` reports the honest constant
/// false (the extension runtime owns no controlling terminal) and the stream
/// classes keep the instance shapes.
static TTY_SHIM: Shim = Shim::new(
    "tty_shim.js.txt",
    include_bytes!("../../../assets/shims/tty_shim.js.txt.deflate"),
);

/// `node:readline`: the `Interface` skeleton (line/whole-line semantics), the
/// real ANSI cursor helpers, and the promise-API module.
static READLINE_SHIM: Shim = Shim::new(
    "readline_shim.js.txt",
    include_bytes!("../../../assets/shims/readline_shim.js.txt.deflate"),
);

/// `node:v8`: heap/snapshot/serialization entries throw (no V8 introspection
/// host); the small value surfaces report stable zeros.
static V8_SHIM: Shim = Shim::new(
    "v8_shim.js.txt",
    include_bytes!("../../../assets/shims/v8_shim.js.txt.deflate"),
);

/// `node:vm`: minimal real — a JS-layer same-realm sandbox approximation
/// (`with(proxy)` + `eval` in a sloppy runner; true realm isolation stays a
/// permanent boundary per HANDOFF.md §2.3, see the shim header for the
/// documented divergences).
static VM_SHIM: Shim = Shim::new(
    "vm_shim.js.txt",
    include_bytes!("../../../assets/shims/vm_shim.js.txt.deflate"),
);

/// `node:constants`: the flat legacy constant table, values captured from the
/// real Node v24 win32 build at generation time.
static CONSTANTS_SHIM: Shim = Shim::new(
    "constants_shim.js.txt",
    include_bytes!("../../../assets/shims/constants_shim.js.txt.deflate"),
);

/// `node:punycode`: the full RFC 3492 codec (pure logic port).
static PUNYCODE_SHIM: Shim = Shim::new(
    "punycode_shim.js.txt",
    include_bytes!("../../../assets/shims/punycode_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// Web primitives (engine globals, not node: modules)
// ---------------------------------------------------------------------------

/// Web events/abort family (engine global): `Event`/`EventTarget` minimal
/// synchronous dispatch, `AbortSignal extends EventTarget` with
/// `abort`/`timeout`/`any`/`reason`/`throwIfAborted` and `AbortController`.
/// Every install is `??`-guarded; the pi-coding-agent shim's own
/// typeof-guarded fallback becomes a no-op when these land first.
static ABORT_GLOBAL: Shim = Shim::new(
    "abort_global.js.txt",
    include_bytes!("../../../assets/shims/abort_global.js.txt.deflate"),
);

/// Minimal Web streams (engine global): `ReadableStream`/`WritableStream`/
/// `TransformStream` with queue-based readers. Minimal tier — no backpressure
/// or highWaterMark fidelity (same tier as the `node:stream` pad), documented
/// in the source.
static WEB_STREAMS_GLOBAL: Shim = Shim::new(
    "web_streams.js.txt",
    include_bytes!("../../../assets/shims/web_streams.js.txt.deflate"),
);

/// `TextEncoder`/`TextDecoder` (engine global): pure-JS codecs (the QuickJS
/// engine has none) over the utf-8/latin1/utf-16le label set — the utf-8
/// state machine carries incomplete trailing sequences across streaming
/// `decode()` calls; `fatal` and BOM handling included. Labels outside the
/// set throw `RangeError` like the Web platform.
static TEXT_CODEC_GLOBAL: Shim = Shim::new(
    "textcodec_global.js.txt",
    include_bytes!("../../../assets/shims/textcodec_global.js.txt.deflate"),
);

/// Global `fetch` + `Request`/`Response`/`Headers` (engine global) over the
/// `__pir_host.http` bridge (`http_fns.rs`): headers return first, the body
/// streams through poll windows into `Response.body`'s ReadableStream,
/// `text()`/`json()`/`arrayBuffer()` aggregate through the same reader, and
/// an aborted `AbortSignal` cancels the host request. Real tier for the
/// common shape; limits (no CORS/cache/formData, request bodies are
/// aggregated) are documented in the source.
static FETCH_GLOBAL: Shim = Shim::new(
    "fetch_global.js.txt",
    include_bytes!("../../../assets/shims/fetch_global.js.txt.deflate"),
);

/// Web-compat residual globals (P0 wave): `queueMicrotask`/`structuredClone`
/// (minimal tier, documented in the source), the global `atob`/`btoa` base64
/// pair, the `performance` wall-clock pair, and the global `Blob`/`File`
/// value holders — every install `??`-guarded so an engine-provided global is
/// never shadowed. Runs last in the bootstrap eval (after the Buffer backfill
/// it builds on). `URL`/`URLSearchParams` are deliberately NOT defined here:
/// the url shim installs them at the builtin bootstrap sweep, and a second
/// parser definition would fork behavior.
static WEBCOMPAT_GLOBAL: Shim = Shim::new(
    "webcompat_globals.js.txt",
    include_bytes!("../../../assets/shims/webcompat_globals.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// P0 residual wave: the remaining public builtins
// ---------------------------------------------------------------------------

/// `node:console` (near-real): a real `Console` class (counters/timers/group
/// state over util.format); the five prelude-backed methods delegate to the
/// host console, the rest of the Node surface rides the shared instance.
static CONSOLE_SHIM: Shim = Shim::new(
    "console_shim.js.txt",
    include_bytes!("../../../assets/shims/console_shim.js.txt.deflate"),
);

/// `node:sys`: the deprecated `util` alias — pure re-export of the util shim.
static SYS_SHIM: Shim = Shim::new(
    "sys_shim.js.txt",
    include_bytes!("../../../assets/shims/sys_shim.js.txt.deflate"),
);

/// `node:diagnostics_channel` (real): named channel registry, publish
/// fan-out with isolated subscriber errors, subscribe/unsubscribe and the
/// tracingChannel start/end/error fan-out (pure JS, no host surface).
static DIAGNOSTICS_CHANNEL_SHIM: Shim = Shim::new(
    "diagnostics_channel_shim.js.txt",
    include_bytes!("../../../assets/shims/diagnostics_channel_shim.js.txt.deflate"),
);

/// `node:stream/web` (minimal): re-exports the engine's Web streams globals
/// plus the real queue-length strategies, reader/writer delegation, a
/// functional tee, and documented skeleton classes for the byte-level
/// controller internals the minimal engine pad cannot model.
static STREAM_WEB_SHIM: Shim = Shim::new(
    "stream_web_shim.js.txt",
    include_bytes!("../../../assets/shims/stream_web_shim.js.txt.deflate"),
);

/// `node:stream/consumers` (real): drain a Web/node-style readable into
/// ArrayBuffer/Blob/Buffer/bytes/text/json.
static STREAM_CONSUMERS_SHIM: Shim = Shim::new(
    "stream_consumers_shim.js.txt",
    include_bytes!("../../../assets/shims/stream_consumers_shim.js.txt.deflate"),
);

/// `node:readline/promises` (minimal): the Promise API over the readline
/// shim — PromiseInterface re-export plus the promise-returning `Readline`
/// cursor helper class.
static READLINE_PROMISES_SHIM: Shim = Shim::new(
    "readline_promises_shim.js.txt",
    include_bytes!("../../../assets/shims/readline_promises_shim.js.txt.deflate"),
);

/// `node:inspector` / `node:inspector/promises` (minimal): the Session
/// skeleton (post throws/rejects — no V8 inspector protocol host), the
/// module `console` passing through to the runtime console, and the honest
/// inactive values for `url()`/`waitForDebugger()`.
static INSPECTOR_SHIM: Shim = Shim::new(
    "inspector_shim.js.txt",
    include_bytes!("../../../assets/shims/inspector_shim.js.txt.deflate"),
);
static INSPECTOR_PROMISES_SHIM: Shim = Shim::new(
    "inspector_promises_shim.js.txt",
    include_bytes!("../../../assets/shims/inspector_promises_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// P0 residual wave: throws-tier builtins
// ---------------------------------------------------------------------------

/// `node:http2` (throws): no HTTP/2 framing stack. The real value surfaces
/// (the full nghttp2 `constants` tables and RFC 7540 `getDefaultSettings`)
/// are captured from real Node v24; every session/server entry point throws
/// the unified error.
static HTTP2_SHIM: Shim = Shim::new(
    "http2_shim.js.txt",
    include_bytes!("../../../assets/shims/http2_shim.js.txt.deflate"),
);

/// `node:cluster` (throws): single-process runtime. Real primary-state values
/// (isWorker=false, empty workers); fork/setup entries throw.
static CLUSTER_SHIM: Shim = Shim::new(
    "cluster_shim.js.txt",
    include_bytes!("../../../assets/shims/cluster_shim.js.txt.deflate"),
);

/// `node:dgram` (throws): no UDP stack.
static DGRAM_SHIM: Shim = Shim::new(
    "dgram_shim.js.txt",
    include_bytes!("../../../assets/shims/dgram_shim.js.txt.deflate"),
);

/// `node:repl` (throws): no interactive loop host; `isValidSyntax` reports
/// the conservative false.
static REPL_SHIM: Shim = Shim::new(
    "repl_shim.js.txt",
    include_bytes!("../../../assets/shims/repl_shim.js.txt.deflate"),
);

/// `node:trace_events` (throws): no trace host; `getEnabledCategories`
/// returns "".
static TRACE_EVENTS_SHIM: Shim = Shim::new(
    "trace_events_shim.js.txt",
    include_bytes!("../../../assets/shims/trace_events_shim.js.txt.deflate"),
);

/// `node:domain` (throws): deprecated module, no domain context tracking;
/// real no-domain state (`active: null`, `_stack: []`).
static DOMAIN_SHIM: Shim = Shim::new(
    "domain_shim.js.txt",
    include_bytes!("../../../assets/shims/domain_shim.js.txt.deflate"),
);

/// `node:wasi` (throws): no WASI host.
static WASI_SHIM: Shim = Shim::new(
    "wasi_shim.js.txt",
    include_bytes!("../../../assets/shims/wasi_shim.js.txt.deflate"),
);

/// `node:test` (throws): no test-runner host; `assert` is the node:assert
/// surface (the same wiring real Node uses), `mock`/`snapshot` keep their
/// real key shapes.
static TEST_SHIM: Shim = Shim::new(
    "test_shim.js.txt",
    include_bytes!("../../../assets/shims/test_shim.js.txt.deflate"),
);

/// `node:test/reporters` (throws): reporter classes throw on construction.
static TEST_REPORTERS_SHIM: Shim = Shim::new(
    "test_reporters_shim.js.txt",
    include_bytes!("../../../assets/shims/test_reporters_shim.js.txt.deflate"),
);

/// `node:sea` (throws with one real value): `isSea()` is honestly false; asset
/// access throws the unified error.
static SEA_SHIM: Shim = Shim::new(
    "sea_shim.js.txt",
    include_bytes!("../../../assets/shims/sea_shim.js.txt.deflate"),
);

// ---------------------------------------------------------------------------
// P0 residual wave: private `node:_*` internals (alias surfaces)
// ---------------------------------------------------------------------------

/// `node:_http_agent` / `node:_http_client`: pure re-exports of the node:http
/// shim surface (defensive internal requires in http stacks link).
static HTTP_INTERNAL_SHIM: Shim = Shim::new(
    "http_internal_shim.js.txt",
    include_bytes!("../../../assets/shims/http_internal_shim.js.txt.deflate"),
);

/// `node:_http_common`: real wire constants and RFC 7230 token/header checks;
/// the parser state machine throws the unified error.
static HTTP_COMMON_SHIM: Shim = Shim::new(
    "http_common_shim.js.txt",
    include_bytes!("../../../assets/shims/http_common_shim.js.txt.deflate"),
);

/// `node:_http_incoming`: IncomingMessage alias plus real no-op backpressure
/// pair (the shimmed message surface never pauses).
static HTTP_INCOMING_SHIM: Shim = Shim::new(
    "http_incoming_shim.js.txt",
    include_bytes!("../../../assets/shims/http_incoming_shim.js.txt.deflate"),
);

/// `node:_http_outgoing`: OutgoingMessage/validator aliases, the real
/// `parseUniqueHeadersOption` normalization, and the k-symbols.
static HTTP_OUTGOING_SHIM: Shim = Shim::new(
    "http_outgoing_shim.js.txt",
    include_bytes!("../../../assets/shims/http_outgoing_shim.js.txt.deflate"),
);

/// `node:_http_server`: server class/status-table aliases plus real no-op
/// lifecycle helpers and the k-symbols.
static HTTP_SERVER_SHIM: Shim = Shim::new(
    "http_server_shim.js.txt",
    include_bytes!("../../../assets/shims/http_server_shim.js.txt.deflate"),
);

/// `node:_stream_duplex` / `_stream_readable` / `_stream_writable`: `from`
/// aliases the stream shim factory; web interop (`fromWeb`/`toWeb`) throws;
/// the internal state classes are inert skeletons.
static STREAM_INTERNALS_SHIM: Shim = Shim::new(
    "stream_internals_shim.js.txt",
    include_bytes!("../../../assets/shims/stream_internals_shim.js.txt.deflate"),
);

/// `node:_stream_passthrough` / `_stream_transform` / `_stream_wrap`: zero
/// named exports in real Node; the empty default keeps them resolvable.
static STREAM_EMPTY_INTERNALS_SHIM: Shim = Shim::new(
    "stream_empty_internals_shim.js.txt",
    include_bytes!("../../../assets/shims/stream_empty_internals_shim.js.txt.deflate"),
);

/// `node:_tls_common` / `node:_tls_wrap`: aliases onto the node:tls shim
/// surface; `translatePeerCertificate` is a documented pass-through.
static TLS_INTERNALS_SHIM: Shim = Shim::new(
    "tls_internals_shim.js.txt",
    include_bytes!("../../../assets/shims/tls_internals_shim.js.txt.deflate"),
);

/// Every file-backed shim in this module, for the byte-identity test.
pub(crate) const SHIMS: &[(&str, &Shim)] = &[
    ("node_call_helper.js.txt", &NODE_CALL_HELPER),
    ("node_cjs_runtime.js.txt", &NODE_CJS_RUNTIME),
    ("buffer_global.js.txt", &BUFFER_GLOBAL),
    ("intl_pad.js.txt", &INTL_PAD),
    ("process_global_stub.js.txt", &PROCESS_GLOBAL_STUB),
    ("path_shim.js.txt", &PATH_SHIM),
    ("url_shim.js.txt", &URL_SHIM),
    ("util_shim.js.txt", &UTIL_SHIM),
    ("util_types_shim.js.txt", &UTIL_TYPES_SHIM),
    ("events_shim.js.txt", &EVENTS_SHIM),
    ("querystring_shim.js.txt", &QUERYSTRING_SHIM),
    ("string_decoder_shim.js.txt", &STRING_DECODER_SHIM),
    ("assert_shim.js.txt", &ASSERT_SHIM),
    ("assert_strict_shim.js.txt", &ASSERT_STRICT_SHIM),
    ("process_shim.js.txt", &PROCESS_SHIM),
    ("fs_shim.js.txt", &FS_SHIM),
    ("fs_promises_shim.js.txt", &FS_PROMISES_SHIM),
    ("os_shim.js.txt", &OS_SHIM),
    ("crypto_shim.js.txt", &CRYPTO_SHIM),
    ("child_process_shim.js.txt", &CHILD_PROCESS_SHIM),
    ("stream_shim.js.txt", &STREAM_SHIM),
    ("stream_promises_shim.js.txt", &STREAM_PROMISES_SHIM),
    ("zlib_shim.js.txt", &ZLIB_SHIM),
    ("module_shim.js.txt", &MODULE_SHIM),
    ("worker_threads_shim.js.txt", &WORKER_THREADS_SHIM),
    ("net_shim.js.txt", &NET_SHIM),
    ("dns_promises_shim.js.txt", &DNS_PROMISES_SHIM),
    ("http_shim.js.txt", &HTTP_SHIM),
    ("https_shim.js.txt", &HTTPS_SHIM),
    ("sqlite_shim.js.txt", &SQLITE_SHIM),
    ("async_hooks_shim.js.txt", &ASYNC_HOOKS_SHIM),
    ("dns_shim.js.txt", &DNS_SHIM),
    ("tty_shim.js.txt", &TTY_SHIM),
    ("readline_shim.js.txt", &READLINE_SHIM),
    ("v8_shim.js.txt", &V8_SHIM),
    ("vm_shim.js.txt", &VM_SHIM),
    ("constants_shim.js.txt", &CONSTANTS_SHIM),
    ("punycode_shim.js.txt", &PUNYCODE_SHIM),
    ("abort_global.js.txt", &ABORT_GLOBAL),
    ("web_streams.js.txt", &WEB_STREAMS_GLOBAL),
    ("textcodec_global.js.txt", &TEXT_CODEC_GLOBAL),
    ("fetch_global.js.txt", &FETCH_GLOBAL),
    ("webcompat_globals.js.txt", &WEBCOMPAT_GLOBAL),
    ("console_shim.js.txt", &CONSOLE_SHIM),
    ("sys_shim.js.txt", &SYS_SHIM),
    ("diagnostics_channel_shim.js.txt", &DIAGNOSTICS_CHANNEL_SHIM),
    ("stream_web_shim.js.txt", &STREAM_WEB_SHIM),
    ("stream_consumers_shim.js.txt", &STREAM_CONSUMERS_SHIM),
    ("readline_promises_shim.js.txt", &READLINE_PROMISES_SHIM),
    ("inspector_shim.js.txt", &INSPECTOR_SHIM),
    ("inspector_promises_shim.js.txt", &INSPECTOR_PROMISES_SHIM),
    ("http2_shim.js.txt", &HTTP2_SHIM),
    ("cluster_shim.js.txt", &CLUSTER_SHIM),
    ("dgram_shim.js.txt", &DGRAM_SHIM),
    ("repl_shim.js.txt", &REPL_SHIM),
    ("trace_events_shim.js.txt", &TRACE_EVENTS_SHIM),
    ("domain_shim.js.txt", &DOMAIN_SHIM),
    ("wasi_shim.js.txt", &WASI_SHIM),
    ("test_shim.js.txt", &TEST_SHIM),
    ("test_reporters_shim.js.txt", &TEST_REPORTERS_SHIM),
    ("sea_shim.js.txt", &SEA_SHIM),
    ("http_internal_shim.js.txt", &HTTP_INTERNAL_SHIM),
    ("http_common_shim.js.txt", &HTTP_COMMON_SHIM),
    ("http_incoming_shim.js.txt", &HTTP_INCOMING_SHIM),
    ("http_outgoing_shim.js.txt", &HTTP_OUTGOING_SHIM),
    ("http_server_shim.js.txt", &HTTP_SERVER_SHIM),
    ("stream_internals_shim.js.txt", &STREAM_INTERNALS_SHIM),
    (
        "stream_empty_internals_shim.js.txt",
        &STREAM_EMPTY_INTERNALS_SHIM,
    ),
    ("tls_internals_shim.js.txt", &TLS_INTERNALS_SHIM),
];
