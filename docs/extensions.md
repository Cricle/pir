# Extensions

pir loads JS/TS extensions through the embedded QuickJS runtime (rquickjs + swc transpile) — no Node, no npm at
runtime. Load with `-e <path>` (file or directory), or discover from `<agentDir>/extensions`,
`<cwd>/.pi/extensions` (trust-gated), and installed packages ([packages.md](packages.md)).

## Native value bridge (`__pir_native`)

The hot boundary between a JS extension and the Rust host is JSON-**shaped** but no longer JSON-**encoded**:
`globalThis.__pir_native` exposes functions that take and return live QuickJS values, converted to/from
`serde_json::Value` trees in-process (`src/extensions/runtime/native_bridge.rs`). The context snapshot, event
dispatch, tool execution, and the models/usage/tool/command snapshots cross as objects — no `JSON.stringify` /
`JSON.parse` at the seam. The legacy `__pir_host` string functions remain as a fallback: every prelude call site
guards with `typeof __pir_native?.x === "function"`, so a boot path without the native tier degrades to the
string bridge unchanged. Extension source files are not modified by any of this.

## Node builtins in Rust (`crates/pir-node`)

The Node/npm surface an extension sees (`node:fs`, `node:crypto`, `child_process`, `zlib`, `http`/`net`,
`worker_threads`, `node:sqlite`, `bcrypt`, `dns`, `path`, `os`, `process`, …) is implemented in Rust in the
standalone `crates/pir-node` workspace crate. It has no dependency on rquickjs: its public entry point is
`NodeRuntime::invoke(module, op, &serde_json::Value) -> Result<serde_json::Value, NodeError>`, and every module
keeps the exact Node error shape (`{ok:false, code, message, syscall, path}` / libuv `errno`). The crate owns the
whole implementation — there is no second op table in the host. `NodeRuntime::invoke` dispatches exactly these
host-op modules (`crates/pir-node/src/lib.rs`): `os`, `process`, `path`, `crypto`, `zlib`, `sqlite`, `bcrypt`,
`child`, `fs_watch`/`fsWatch`, `fs`, `dns`, `npm`, `http`, `worker`. The typed C-ABI fast paths (`__pirBufferCodec`,
`__pirPath`, the `__pirFs*`/`__pirCrypto*` kernels) are additional entry points into the same crate, not a second
implementation.

The JS side reaches it through one C-ABI funnel: `globalThis.__pirNode.invoke(module, op, payload)` receives live
QuickJS values (no `JSON.stringify`/`JSON.parse` at the seam) and returns a live value. The single shim helper
`__pirNodeCall` prefers that entry. The legacy `__pir_host.node` / `nodeExt` / `http` / `fsWatch` / `worker`
string entries still exist because the async shim pumps call them directly, but they are wire-only: they parse
the JSON payload once and dispatch into the same `NodeRuntime`, so each module has exactly one implementation and
the two transports cannot diverge. The host contributes only what must stay host-owned, through narrow
`NodeHooks` callbacks: npm specifier resolution + source transpilation and the per-worker QuickJS engine boot
(the worker transport itself is in `pir-node`). `crates/pir-node/tests/` covers the error envelope, path fencing
and dispatch, and `tests/differential_node.rs` compares behaviour against a real `node` binary when one is on
`PATH`.

Byte-heavy calls skip the generic funnel. `__pirBufferCodec` (the Buffer numeric/codec kernels), `__pirPath`
(`join`/`resolve`/`normalize`/`relative`), and `__pirFsRead` / `__pirFsWrite` / `__pirCryptoDigest` /
`__pirCryptoHmac` / `__pirCryptoRandom` take live `Uint8Array`/string arguments and return live values, so a
multi-MB read, write or digest never base64-encodes its payload on either leg. Every call site keeps the
`__pirNode.invoke` / pure-JS branch as its fallback; a host kernel failure after the shape was accepted is
rethrown with the same `code`/`syscall`/`path`/`errno` shape instead of falling back, so a partially written
append is never replayed.

Metadata-shaped calls skip the funnel too: `__pirFsStat(path, lstat)` returns the exact `Stats` payload
(`isFile`/`isDirectory`/`isSymbolicLink`/`mode`/`size`/`mtimeMs`/`birthtimeMs`), `__pirFsExists(path)` returns a
boolean, `__pirFsReaddir(path, dirents)` returns an array of names or raw dirents, and `__pirProcessCwd()`
returns the runtime cwd string. Directory walks and per-render `cwd` reads were paying a payload build, a
stringify, a parse, a `serde_json::Value` tree and a second stringify per call. `process.cwd()` additionally
memoizes the answer in the shim: `process.chdir` is unsupported in this host and the runtime cwd is fixed at
construction, so the steady-state cost is one property read.

The same shape rules apply to the newer typed families: `__pirUrlParse`, `__pirInspect`, and each workstream's
entries answer `undefined` when the argument shape is not one they own, and the shim then runs its own
implementation (which is also where the Node-exact error text lives).

Hot-path timing is a test, not a claim: `tests/perf_parity.rs` runs a 51-operation corpus (URL, querystring,
path, fs, crypto, Buffer, text codecs, JSON, util, events, zlib, process) through both `pir js` and a real
`node`, and prints ns/op plus the share of operations within 1x/2x/4x/10x of Node. It is `#[ignore]`d because
it needs Node on `PATH`; run it with `-- --ignored --nocapture`. The default is report-only; set
`PIR_PERF_THRESHOLD=0.8` to enforce the project target and fail below it. Dev/test builds compile the host
crate at `opt-level = 0`, so use a `release-fast` binary for production numbers.

Measured on the shipped path (`release-fast`, min of three runs, Node v24.22.1 on the same host): 9 of 51
operations are at least as fast as Node (fs read/exists, zlib deflate/inflate, pbkdf2, randomByte, Buffer.concat,
TextEncoder, sha256-1MB), 29% are within 2x, 47% within 4x, 75% within 10x. The remaining gap is structural:
`EventEmitter#emit`, `new URL`, `JSON.stringify`, `querystring.parse` and `path.join` are sub-microsecond
builtins that V8 inlines into the caller, while this runtime pays an interpreter dispatch plus a C-ABI crossing;
closing those needs a JIT, not more native code. Every hot path in the corpus does have a native/C-ABI or
precompiled-bytecode implementation, so none of them is left as a pure-JS reimplementation.

## Ahead-of-time shim bytecode

The embedded JS shims are compiled to QuickJS module bytecode offline, so a runtime instance no longer parses
~4 MB of shim source at boot:

- Generator: `cargo +stable-x86_64-pc-windows-gnu run --features extensions --bin pir-gen-bytecode` writes
  `assets/bytecode/`.
  - `assets/bytecode/prelude.js.txt.qc` — the script-boot shim evaluated per runtime (`eval_shim_precompiled`).
  - `assets/bytecode/node_globals.js.txt.qc` and `node_modules_bootstrap.js.txt.qc` — the two Node-compat boot
    bundles. Their text is assembled from many file-backed shims, so they are compiled under the synthetic
    names, not as one `Shim`.
  - `assets/bytecode/v/*.qc` — every virtual module the loader can resolve (`@node/*`, typebox, pi packages),
    compiled under its **canonical** module name. QuickJS keys its module cache by that resolved name, so the
    name inside the bytecode must match it; otherwise every import would create a fresh module instance
    (the `crypto.subtle` identity check catches this).
- Guard: 52 bytes of `magic("PIRQCJS\0", 8) | format_version=1 (u32 LE) | payload_len (u32 LE) | flags=0
  (u32 LE) | sha256(payload) (32)`, followed by engine bytecode. `api::bytecode::payload` checks the magic,
  format version and payload length (and rejects an empty/mismatched payload); the digest is pinned by the
  manifests and checked by the wiring tests. A stale or upgraded-engine blob falls back to the
  deflated-source path instead of loading.
- Manifests: `assets/bytecode/manifest.txt` and `virtual-manifest.txt` pin the payload digests, checked by
  `tests/suites/extensions_wiring.rs` along with a `Module::load` smoke test for every blob.
- Bytecode is bound to the build's QuickJS version and byte order. Regenerate after editing a shim, changing the
  `Shim` registry, or upgrading rquickjs.
- `PIR_EXT_BYTECODE=0` (`false`/`off`) makes every shim load from source instead of the embedded blobs. The
  guard header records the payload, not the source, so without this switch a shim edit that has not been
  re-AOT'd silently keeps running the old code — use it in the edit/test loop, and regenerate before shipping.
- `PIR_EXT_SHIM_SRC=1` reads a virtual shim's source from `src/extensions/api/js/<name>` at the checkout
  recorded by `CARGO_MANIFEST_DIR` (`api::node_shims::dev_shim_source`), instead of the embedded deflate blob.
  It is a development override only: an installed binary, a moved checkout, or a missing file silently keeps
  the embedded source. Only virtual modules go through this path; the prelude and the assembled
  `node_globals`/`node_modules_bootstrap` bundles always read their embedded `Shim` sources, so editing one of
  those still needs a rebuild. AOT blobs still win while `PIR_EXT_BYTECODE` is enabled, so the virtual-shim
  edit loop is `PIR_EXT_SHIM_SRC=1 PIR_EXT_BYTECODE=0`.

## Disk extension bytecode cache

On-disk extension modules (`.ts`/`.js` ESM entries and their relative imports) still have to be transpiled by SWC
and compiled by QuickJS the first time a source revision is seen. `bytecode_cache` persists that compiled module
and loads it with `Module::load` on later runs, so the work happens once per source revision instead of once per
`pir` start, worker runtime or extension reload:

- Key: `sha256("pir-ext-bytecode-3" NUL canonical module name NUL raw source bytes)` — the schema string is
  `CACHE_SCHEMA` (`src/extensions/bytecode_cache.rs`), and the hash is taken **before** SWC, so a hit skips both
  the transpile and the compile. An edited file always misses.
- Location: `<agent_dir>/cache/ext-bytecode/<key>.qc` (`PIR_AGENT_DIR` isolates test/alternate installs). Writes
  are temp-file + rename, so concurrent `pir` processes never read a partial blob; the directory is pruned once
  per process — past 1024 files / 64 MiB it deletes oldest-first down to 768 files / 48 MiB.
- Failure is always soft: a blob `Module::load` rejects is evicted, the loader falls back to
  `Module::declare`, and a read-only cache directory just means "no cache". QuickJS encodes its own BC version,
  so an rquickjs upgrade falls back rather than misreading an old blob; bump `CACHE_SCHEMA` when the transpiler's
  output for unchanged input changes.
- Measured on a release build, per module: 73 KiB TS = 5.1 ms transpile + 2.3 ms QuickJS compile → 0.4 ms bytecode
  load; 295 KiB TS = 26 ms + 12 ms → 1.6 ms. End to end through the loader a 95 KiB module saves ~8 ms of startup
  (the fixed runtime/prelude boot is unaffected).
- `PIR_EXT_BYTECODE_CACHE=0` (also `false`/`off`) disables reads and writes; deleting the directory is always safe.
- CJS/JSON interop wrappers keep the source path: they are tiny and only reference the real module, which the
  CommonJS runtime loads through its own cache.

## Module resolution and sandbox roots

On-disk modules are authority-checked before they are read (`src/extensions/loader.rs`,
`src/extensions/runtime/runtime.rs`):

- **Canonical containment.** Every resolved file is `std::fs::canonicalize`d and must `starts_with` a
  registered canonical root (`is_within_allowed_roots`, `allowed_module_file`). Symlinks and `..` are
  therefore judged by the real target, never by the spelling.
- **Entry gating.** `-e <dir>` registers that directory; `-e <file>` registers its package entry root.
  Each discovered entry is re-checked against the roots before its source is read or seeded; an entry
  outside them fails with `entry is outside the allowed extension roots or not a real file`
  (`ExtensionRuntime::load` / `allowed_entry_path`).
- **Relative, absolute and `file:` imports.** Relative imports resolve against the importer's real
  directory and must stay under a root. `file:` URLs and absolute paths are accepted
  (`path_specifier_to_local`); a non-local file URL (`file://host/x`) or a path outside every root is
  rejected (`outside the allowed extension roots`).
- **Bare-package scope.** Bare ESM imports and `require` resolve through `node_modules`/`package.json`
  (`npm_resolve`). The resolved file must be under its package root. The package root is auto-registered
  only for a real installed package; a `#`-alias or an externally linked tree cannot grant a new root.
  Unknown `node:` builtins stay rejected; bare npm specifiers that fail resolution fail at load time.
- **Identity.** Module identity is the canonical path encoded into the synthetic `pir-ext:` name
  (`module_name_for`). `%`, `?` and `#` are escaped in the path; a `?query`/`#fragment` suffix stays part
  of the id, so `./x.mjs` and `./x.mjs?v=1` are distinct modules as in Node. Loading an id re-canonicalizes
  the path and refuses it if the real file no longer matches that id.

## Native-first plugin precedence (`PIR_PREFER_NATIVE`)

Builtins that are full native ports of JS packages win automatically: when both the Rust builtin and the JS
package are on the load plan for `subagent`, `mcp`, `codemode`, `tool-search`, `todo` or `context-mode`, the JS
entry is dropped and only the Rust implementation runs. No plugin source change, no flag and no interpreted-JS
overhead — this is the default because the point of a native port is that the unmodified plugin gets native
throughput.

The two single-face rules are likewise always on because two copies would double-apply: `builtin:tool-repair`
suppresses the JS `pi-tool-repair`, and `builtin:phone` suppresses `@malinamnam/pi-phone`. `PIR_PREFER_NATIVE=0`
(also `false`/`off`) restores upstream first-wins behavior for the six full-port pairs, for a user who
deliberately wants the JS package to win. See `src/plugins/loader.rs`'s `NATIVE_PRECEDENCE`.

## Default-disabled packages

`@malinamnam/pi-phone` stays installed and listed in `packages` but is **skipped by default**: it starts a local
web server and costs ~14 MB of QuickJS heap per startup. Two opt-ins:

```bash
# Option 1: enable default-disabled packages (GLOBAL settings only —
# a project .pi/settings.json cannot opt in for the user).
#   ~/.pi/agent/settings.json:
#   { "defaultDisabledPackagesEnabled": true }
pir

# Option 2: name the package explicitly; an explicit -e always wins.
pir -e ~/.pi/agent/npm/node_modules/@malinamnam/pi-phone
```

The package matches by canonicalized path and `package.json` `"name"` — npm install
(`<agentDir>/npm/node_modules/@malinamnam/pi-phone`), local checkout, or symlinked node_modules entry. One `[ext]`
diagnostic names the opt-in when skipped; `--no-extensions` drops default discovery; an explicit `-e` still loads
the path. Enabling runs the unchanged JS package.

## Builtin-only mode (`--only-builtin`)

`--only-builtin` is the fast path between default discovery and `--no-extensions`:

```bash
pir --only-builtin                 # Rust builtins only
pir --only-builtin -e builtin:phone  # explicit default-off builtin still loads
pir --only-builtin -e ./my-ext.ts  # ignored: one [ext] diagnostic names it
pir --only-builtin --no-extensions # --no-extensions wins (no builtins, no JS)
```

- **Plan**: explicit `-e builtin:<name>` entries in CLI order (deduped), then every default-enabled builtin — the
  normal plan's builtin subset; `phone` stays default-off.
- **Ignored**: settings `extensions`, project/user auto-discovery, installed `packages` extensions, and on-disk
  `-e` paths (reported once: `[ext] --only-builtin ignores N on-disk extension path(s): …`).
- **No JS runtime**: only `builtin:<name>` pseudo-paths, so a plan-only `ExtensionHandle` (`has_js_runtime() ==
  false`) — no QuickJS actor, prelude, SWC transpile, `node:*` shims or npm/config scan.
- **Native surface intact**: every builtin's tools/commands/hooks register through the unified plugin host
  (`subagent`, `mcp`, `codemode`, `python`, `context-mode`, `tool_search`, `tool-repair`, `todo`, `memory`, plus
  `phone` when named).
- **Precedence and hosts**: `--no-extensions` wins (unchanged: only explicit `-e` paths, no builtins injected); the
  flag is global and resolved once for TUI, RPC, print and `--mode json`, while `server`/`client` load no extension
  runtime (same as `--no-extensions`). SDK equivalent: `AgentBuilder::only_builtin(true)` ([sdk.md](sdk.md)).

## Timing the extension startup

`PI_TIMING=1` logs one `pir-timing: <phase>: <ms>ms` line per phase to stderr; no prompts, payloads or credentials.
Phases: `ext.runtime.start` (actor + QuickJS + prelude), `ext.runtime.plan_only` (plan-only handle for
`--only-builtin`), `ext.runtime.load` (discovery + SWC + module evaluation), `plugins.host.build`, first
`ext.event.<name>` dispatch, `ext.handler.<event>` (one handler invocation, elapsed incl. awaits, sanitized
identity, ok/error), `ext.host.<module>.<op>` (blocking bridge call over 50 ms), `ext.background_job`,
`loadout.first_build`, `prompt.system_prompt_build`, `provider.first_request_send`, `provider.first_stream_event`.
Each phase label is emitted at most once per process (per-handler/host/background lines per occurrence); a
`--only-builtin` run logs `ext.runtime.plan_only`, never `.start`/`.load`; handler lines carry elapsed ms plus
`status=ok|error`.

### Event-dispatch budgets and actor responsiveness

An extension shares one QuickJS actor thread with every other extension. Four rules keep a stuck handler from
holding the host:

1. **Command budget.** `callTimeoutMs` is enforced on the caller's runtime and on the actor: the tokio wait and
   QuickJS interrupt deadline expire together; the actor aborts JS execution (`event <name> dispatch timed out`).
2. **Background preemption.** A promise continuation left by a finished command (no command task in flight) runs
   under the per-call budget and is interrupted ~100 ms after a host command is queued, so a prompt/tool/UI action
   never queues behind background work; the runtime is recovered before that command runs.
3. **`before_agent_start` budget.** Uses the shorter `beforeAgentStartTimeoutMs` (default 10 s); on expiry the run
   PROCEEDS with a warning, the reply is discarded, and side effects queued after the timeout are dropped by
   dispatch epoch.
4. **Manifest discovery cache.** `npm_resolve::ManifestCache` keeps up to 128 parsed `package.json` values,
   validated against the live file's length+mtime; a changed manifest is re-read and `/reload` clears the cache.

The same QuickJS interrupt handler also consults an external abort flag installed for worker runtimes, so a
parent `terminate`/`/reload` stops a pure synchronous loop (`while (true) {}`) at the next bytecode boundary.

Remaining limit: a host bridge call blocking longer than the budget (huge `readdir`/`readFile`, uninterruptible OS
call) cannot be interrupted by QuickJS — the caller's deadline returns but the actor stays in the call. Raise
`extensionLimits.callTimeoutMs` / `beforeAgentStartTimeoutMs` for legitimate handlers.

## Shape

```ts
export default function (pi) {
  // register a tool
  pi.registerTool({
    name: "roll",
    description: "Roll a die",
    parameters: {
      type: "object",
      properties: { sides: { type: "number", description: "sides" } },
      required: ["sides"],
    },
    async execute(_id, params, signal, onUpdate, ctx) {
      const value = 1 + Math.floor(Math.random() * params.sides);
      return { content: [{ type: "text", text: String(value) }] };
    },
  });

  // register a slash command
  pi.registerCommand("roll", { handler: async (args, ctx) => { /* ... */ } });

  // event hooks
  pi.on("before_provider_request", async (event, ctx) => { /* mutate payload */ });
  pi.on("session_start", async (_event, ctx) => {
    await ctx.ui.notify("hello", "info");
  });
}
```

## The `pi` API surface

`registerTool` (with `defaultActive`/`exposure`), `registerCommand` (slash commands with flags),
`registerMessageRenderer` / `registerEntryRenderer` / `registerMarkdownTransformer`, `registerProvider` /
`registerVirtualModel` ([custom-provider.md](custom-provider.md)), `registerShortcut`, `registerFlag`,
`registerMCPServer`, `events.on/emit`. Tool parameter schemas are validated at load; `pi.on(event, handler)` is
fail-closed against the known-event registry (an unknown name is a load-time `TypeError`; accepted set:
`src/plugins/coverage.rs::is_known_event`). CLI flags added with `registerFlag` are listed by
`pir --help-extensions` (the upstream `Extension CLI Flags:` help section); `--help` itself never executes
extension code.

`pi.events` is a **JS-runtime-only** bus: `emit` synchronously invokes the prelude's `busSubs` subscribers
(`prelude.js.txt:3075`). The native `EventService` (`PluginContext::events()`) is a separate Rust bus with no
cross-face bridge — JS emissions do not reach native subscribers and vice versa
(`src/agent_session/plugin_services.rs:178-182`, `:4161-4219`; `src/plugins/services/mod.rs:2112-2126`).

## The `ctx` surface

- `ctx.model`, `ctx.thinkingLevel`, `ctx.hasUI`, `ctx.isProjectTrusted()`
- `ctx.ui.*`: `select/confirm/input/editor` (dialog bridges with optional request-level `timeout`), `notify`,
  `setTitle`, `setWidget`, `setStatus`, `getEditorText`
- `ctx.newSession / fork / navigateTree / switchSession / reload / waitForIdle`
- `ctx.executeTool(name, params, { background: true })` (nested, depth-capped; with `background` the awaited
  envelope is a placeholder carrying the call id and the real result arrives later as a `tool_result` event with
  the same `toolCallId`)
- `ctx.compact(options)`, `ctx.abort()`, `ctx.exec` (host-backed exec with abort)
- `ctx.sessionManager`: scalar getters (`getSessionId`, `getSessionPath`, `getCwd`, `getLeafId`, `getHeader`,
  `getLabel`) read dedicated host fields and cost one lookup; entry/collection getters (`getEntries`, `getTree`,
  `getBranch`, `getChildren`, `getSessions`, `getEntry`, `getLeafEntry`, `buildContextEntries`,
  `buildSessionProjection`) parse the full snapshot, cached by session fingerprint — an unchanged push rebuilds
  nothing and skips the plugin `on_session_context` fan-out.

## V8 diagnostics (`node:v8`)

The engine is QuickJS, so `node:v8` is a load-compatible diagnostic surface, not V8
(`src/extensions/api/js/v8_shim.js.txt`, `src/extensions/runtime/host_functions/v8_diagnostics.rs`):

- `getHeapStatistics()` samples the live QuickJS heap. `malloc_size` maps to both `total_heap_size` and
  `malloced_memory`, `memory_used_size` to `used_heap_size`, and `malloc_limit` to `heap_size_limit`;
  `total_available_size` is `max(0, malloc_limit - malloc_size)` when a cap is configured, otherwise `0`.
  `total_heap_size_executable`, `total_physical_size`, `peak_malloced_memory`, `does_zap_garbage`,
  `number_of_native_contexts`, `number_of_detached_contexts`, `total_global_handles_size`,
  `used_global_handles_size` and `external_memory` are documented zero placeholders, not measurements.
  The object carries exactly the fourteen keys Node v24 returns; `external_memory_high_water_mark` is not
  among them.
- `setFlagsFromString("--expose-gc")` only flips a per-runtime flag. `gc` is seeded into vm contexts created
  afterwards; the main global is unchanged, existing contexts stay unchanged, and an explicit sandbox `gc`
  property wins. Other flags are inert.
- `GCProfiler.start()` / `.stop()` throw `ERR_NOT_SUPPORTED`. Heap snapshots (`getHeapSnapshot`,
  `writeHeapSnapshot`), `Serializer`/`Deserializer` (and their defaults), `serialize`/`deserialize`,
  `queryObjects`, `isStringOneByteRepresentation`, `startCpuProfile` and `startupSnapshot.*` throw the same
  code. `getHeapSpaceStatistics()` returns `[]`; `getHeapCodeStatistics()` and `getCppHeapStatistics()` return
  zero-filled objects; `setHeapSnapshotNearHeapLimit` is a no-op; `promiseHooks` is an inert shape;
  `stopCoverage`/`takeCoverage`/`stopCpuProfile` return `undefined`.
- `cachedDataVersionTag()` returns a stable non-zero FNV-1a tag seeded from the runtime identity
  (`pir/quickjs-ng`, platform, arch, Node baseline). Every `setFlagsFromString()` call folds its argument
  into the tag, so it changes when the flag set changes — the property callers use it for. It does not
  claim a V8 code-cache version.

## Resource limits

Extensions share one QuickJS runtime; pir caps resources so a runaway extension fails its own call instead of
taking down the host process.

| Setting (`extensionLimits`) | Env override | Default | Meaning |
|---|---|---|---|
| `memoryMb` | `PI_EXT_MEMORY_MB` | 128 MiB | QuickJS heap cap of the shared runtime; `0` disables the cap |
| `stackMb` | `PI_EXT_STACK_MB` | 16 MiB | JS stack; `0` keeps the default, values above 16 clamped to 16. The worker runtime always requests `WORKER_JS_STACK_BYTES` (16 MiB): rquickjs's `set_max_stack_size` silently reads ANY request above 16 MiB as `0`, which disables the stack check instead of reporting an error (a worker asking for 32 MiB used to run with the guard off). A lowered value applies to the shared host runtime, not the worker |
| `gcThresholdMb` | `PI_EXT_GC_THRESHOLD_MB` | QuickJS default | GC threshold |
| `callTimeoutMs` | `PI_EXT_CALL_TIMEOUT_MS` | 30000 | Per-call timeout of event dispatch and tool execution |
| `hookTimeoutMs` | `PI_EXT_HOOK_TIMEOUT_MS` | 1800000 | Timeout of reserved-prefix hook calls (`__pir_stream:` / `__pir_route:`) |
| `beforeAgentStartTimeoutMs` | `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS` | 10000 | Budget of the `before_agent_start` lifecycle dispatch; a timed-out dispatch is abandoned and the run proceeds |

`0` disables `memoryMb`/`gcThresholdMb`; for `stackMb` and timeouts it keeps the default (the stack check must stay
enabled; interrupts need a positive deadline). Invalid env values are ignored with a diagnostic.

```json
// ~/.pi/agent/settings.json
{ "extensionLimits": { "memoryMb": 512, "callTimeoutMs": 60000 } }
```

**Global scope only.** The engine reads `<agentDir>/settings.json` plus `PI_EXT_*`. A project `.pi/settings.json`
can never loosen these limits, trusted or not. Resolution order: an explicitly injected `ExtensionConfig::limits`
(SDK/embedders) wins verbatim, then `PI_EXT_*`, then global settings, then defaults. Limits are re-applied on
`/reload` (a runtime `set_call_timeout` override is not sticky).

`pir js` is the deliberate exception: it runs on the same QuickJS engine but does not inherit the extension heap
policy. `PIR_JS_MEMORY_MB` opts into a cap in MiB; `0`/unset means uncapped, and a malformed value is reported
on stderr and ignored. `pir js` reads neither `PI_EXT_MEMORY_MB` nor `extensionLimits` (see
[environment-variables.md](environment-variables.md)).

The QuickJS allocator is the system allocator by default. `PIR_QJS_ALLOC=pool` (case-insensitive) selects the
64 KiB-slab size-class pool; `system`, unset and every other value keep `AsyncRuntime::new()`. With the pool
selected, `PIR_QJS_ALLOC_STATS=1` enables process-global counters and prints one
`PIR_QJS_ALLOC stats allocs=… alloc_bytes=… frees=… large=… in_use_bytes=…` line when that runtime is torn
down; without the pool the variable does nothing. Each runtime, including every worker runtime, owns an
independent pool (`src/extensions/runtime/qjs_alloc.rs`).

### When the heap cap trips

The over-allocating call fails with a clear error; the session and every other extension stay usable (pending
exception cleared, job queue drained, GC run):

```
tool `memoryHog` failed: extension exceeded its JS memory limit (512 MB);
raise `extensionLimits.memoryMb` in ~/.pi/agent/settings.json or set
PI_EXT_MEMORY_MB (0 = no limit)
```

Approaching the cap (≥ 80%) logs one warning per crossing — and with `PI_TIMING=1`, one line per extension load
reports the resolved limits:

```
pir-ext: JS heap at 430 MB of the 512 MB memory limit (83%); raise `extensionLimits.memoryMb` ...
pir-ext: resolved limits: memory=512 MB stack=16 MB gc_threshold=default call_timeout=30000 ms hook_timeout=1800000 ms
```

The codemode sandbox keeps its own per-script cap (`codemode.sandboxMemoryMb`, default 128 MiB,
`PIR_CODEMODE_MEMORY_MB`, `0` = no cap); see [codemode.md](codemode.md).

### Queue bounds

Every host↔JS queue is bounded and the bound is visible. **If the source can pause, the producer waits** (real
backpressure: the OS pipe/socket/transport stops being read); only where Node coalesces or truncates (watcher
notifications, blocking `spawnSync`) does the queue drop, and every drop is counted through `ext_diag!` (`[ext]
…` / `[env-daemon] …` / `[env-client] …` / `[session] …`, repeat-collapsed by the logging router). The
provider event stream (`src/ai/event_stream.rs`, capacity 32 + producer backpressure) uses the same pattern.

| Queue (source) | Bound | When full | Observation |
|---|---|---|---|
| `fs.watch` events (`fs_watch_fns.rs`) | 1024 events per watcher | drop OLDEST, keep delivering | `dropped` (cumulative) in every `watchPoll` + `ext_diag!` on first drop and every 1024 |
| `child_process` stdout/stderr, async `spawn` (`child_fns.rs`) | 8 MiB per stream | reader PARKS: pipe fills, child blocks on `write`; no byte dropped | `ext_diag!` on first park and every 4096 waits; `truncated` false |
| `child_process.spawnSync` stdout/stderr | 8 MiB per stream | keep draining, drop OLDEST bytes (`truncated: true`) | `truncated` field + `ext_diag!` on first drop and every 4096 chunks |
| `worker_threads` mailboxes (`worker_fns.rs`) | 8 MiB message text per direction | drop NEWEST data message; `error`/`exit` control records exempt | `droppedCount`/`dropped` in every receive envelope + `ext_diag!` |
| `fetch` response body (`http_fns.rs`) | 8 MiB window | body reader PARKS (TCP window closes); `fetchPoll` drains and wakes it | `truncated` only for a defensive single-chunk overflow, turned into a loud stream error; cancellation releases parked readers |
| `http.Server` pending requests (`http_fns.rs`) | 256 parsed requests | connection thread PARKS before visibility; past the 120 s deadline the connection closes | `ext_diag!` on first park and every 256 waits |
| `http.Server` streaming request body (`http_fns.rs`) | 1 MiB window per request | connection thread PARKS while `serverBody` drains; expiry fails the read and closes | `ext_diag!` on expiry |
| codemode sandbox events (`plugins/codemode.rs`) | 256 events per run | producer PARKS until `codemodePoll` drains; `idle` coalesced, `started`/`done` bypass | `ext_diag!` if a full queue never drains within 30 s (one event dropped, counted) |
| env-daemon output (`env_daemon/output.rs`) | bulk: 4 MiB unsent; control: unbounded BY DESIGN | bulk parks the command reader; control is the priority lane (pings, cancel results) | `ext_diag!` past 4096 frames / 4 MiB control, and on first bulk park + every 1024 |
| env-client outbox (`env_client/connection.rs`) | unbounded, observed at 4096 frames | never parked or dropped: `write` runs in caller tasks and every frame awaits a reply | `ext_diag!` on water mark, first crossing and every 1024 |
| session client update buffer (`client/mod.rs`) | 1024 `service_update` frames | drop OLDEST (subscription updates are latest-state signals) | `pending_updates_dropped()` + `ext_diag!` on first drop and every 1024 |
| steering / follow-up inbox (`agent_session/inbox.rs`) | unbounded BY DESIGN, observed at 1024 messages | never dropped or blocked; drained every turn | `ext_diag!` past the water mark |

## Runtime notes

- `fs.watch` uses the native `notify` backend (`extensions` enables `dep:notify`); fallback polling is adaptive —
  200 ms while changes are observed, backing off to 2 s after five quiet sweeps, reset by any change.
- Relative, absolute and `file:` imports resolve on disk under the allowed roots; known `node:*` builtins are
  served from the shim table, and bare npm packages resolve through `node_modules`/`package.json`. Unknown
  `node:` builtins and native `.node` addons are rejected.
- Scripts run on a dedicated actor thread; Rust↔JS round-trips interleave without deadlock. A panic or `unload`
  invalidates captured `pi` handles.
- `/reload` re-executes extensions after removing old tools/renderers; each generation releases its child
  processes, worker threads, in-flight fetches, HTTP/TCP/Unix servers and sockets, `fs.watch` threads and fs
  descriptors (at shutdown and from `Drop`), scoped so a reload cannot leak the discarded generation's resources.
