# Extensions

pir loads JS/TS extensions through the embedded QuickJS runtime (rquickjs +
swc transpile) — no Node, no npm at runtime. Load with `-e <path>` (file or
directory), or discover from `<agentDir>/extensions`, `<cwd>/.pi/extensions`
(trust-gated), and installed packages (see [packages.md](packages.md)).

## Default-disabled packages

`@malinamnam/pi-phone` stays installed and listed in `packages` but is
**skipped by default**: loading it starts a local web server and costs about
14 MB of QuickJS heap on every startup. Two opt-ins exist:

```bash
# Option 1: turn the default-disabled set on (GLOBAL settings scope only —
# a project .pi/settings.json cannot opt in on the user's behalf).
#   ~/.pi/agent/settings.json:
#   { "defaultDisabledPackagesEnabled": true }
pir

# Option 2: name the package explicitly; an explicit -e always wins.
pir -e ~/.pi/agent/npm/node_modules/@malinamnam/pi-phone
```

The package is recognized by its canonicalized path and its `package.json`
`"name"`, so the npm install (`<agentDir>/npm/node_modules/@malinamnam/pi-phone`),
a local checkout, and a symlinked node_modules entry all match. One `[ext]`
diagnostic names the opt-in when the package is skipped; `--no-extensions`
already drops default discovery, and an explicit `-e` still loads the path.
Enabling it runs the unchanged JS package — there is no native
reimplementation.

## Builtin-only mode (`--only-builtin`)

`--only-builtin` is the fast path between the default plan and
`--no-extensions`:

```bash
pir --only-builtin                 # Rust builtins only
pir --only-builtin -e builtin:phone  # explicit default-off builtin still loads
pir --only-builtin -e ./my-ext.ts  # ignored: one [ext] diagnostic names it
pir --only-builtin --no-extensions # --no-extensions wins (no builtins, no JS)
```

Semantics:

- **Plan**: explicit `-e builtin:<name>` entries in CLI order (deduped), then
  every default-enabled builtin — the normal plan's builtin subset. `phone`
  stays default-off, exactly like the default plan.
- **Ignored**: settings `extensions` entries, project/user auto-discovery
  directories, installed `packages` extensions, and on-disk `-e` paths. The
  ignored `-e` paths are reported once:
  `[ext] --only-builtin ignores N on-disk extension path(s): …`.
- **No JS runtime**: the plan contains only `builtin:<name>` pseudo-paths, so
  the loader builds a plan-only `ExtensionHandle`
  (`has_js_runtime() == false`). No QuickJS actor, no prelude, no SWC
  transpile, no `node:*` shims, no npm/config directory scan.
- **Native surface intact**: every builtin's tools/commands/hooks register
  through the unified plugin host (`subagent`, `mcp`, `codemode`, `python`,
  `context-mode`, `tool_search`, `tool-repair`, `todo`, plus `phone` when
  explicitly named).
- **Precedence**: `--no-extensions` wins when both flags are set; its
  historical behavior is unchanged (only explicit `-e` paths, no builtins
  injected).
- **Every host**: the flag is global and the plan is resolved once for TUI,
  RPC, print and `--mode json`. The `server`/`client` hosts load no extension
  runtime, so the flag has no effect there (same as `--no-extensions`). The
  SDK exposes the equivalent filter as
  `AgentBuilder::only_builtin(true)` (see [sdk.md](sdk.md)).

This path reuses the feature-slim findings in
[FEATURES.md](FEATURES.md): the `extensions` feature's QuickJS + SWC + `node:*`
stack is never entered, while the Rust builtins (compiled into the binary
anyway) remain available.

## Timing the extension startup

`PI_TIMING=1` logs one `pir-timing: <phase>: <ms>ms` line per phase to
stderr; no prompts, payloads or credentials are logged. Phases:
`ext.runtime.start` (actor + QuickJS + prelude), `ext.runtime.plan_only`
(plan-only handle, what `--only-builtin` uses), `ext.runtime.load`
(discovery + SWC + module evaluation), `plugins.host.build`, first
`ext.event.<name>` dispatch, `ext.handler.<event>` (one handler invocation,
elapsed incl. awaits, sanitized identity, ok/error), `ext.host.<module>.<op>`
(blocking bridge call over 50 ms), `ext.background_job`, `loadout.first_build`,
`prompt.system_prompt_build`, `provider.first_request_send`,
`provider.first_stream_event`. Each phase label is emitted at most once per
process (per-handler/host/background lines are per occurrence). A
`--only-builtin` run logs `ext.runtime.plan_only` and never
`ext.runtime.start`/`ext.runtime.load`. Handler lines carry only the elapsed
milliseconds plus `status=ok|error`; both JS and host check the `PI_TIMING=1`
gate, so a normal run pays one boolean read per handler.

### Event-dispatch budgets and actor responsiveness

An extension shares one QuickJS actor thread with every other extension. Four
rules keep a stuck handler or runaway continuation from holding the host:

1. **Command budget.** `callTimeoutMs` is enforced on the caller's runtime and
   on the actor: the tokio wait and the QuickJS interrupt deadline expire
   together; the actor aborts the JS execution (`event <name> dispatch timed
   out`).
2. **Background preemption.** A promise continuation left by a finished command
   (no command task in flight) runs under the per-call budget and is interrupted
   ~100 ms after a host command is queued, so a prompt, tool call or UI action
   is never queued behind background work. The runtime is recovered before that
   command runs.
3. **`before_agent_start` budget.** Uses the shorter
   `beforeAgentStartTimeoutMs` (default 10 s); when it elapses the run PROCEEDS
   with a warning, the reply is discarded, and side effects queued after the
   timeout are dropped by dispatch epoch.
4. **Manifest discovery cache.** `npm_resolve::ManifestCache` keeps up to 128
   parsed `package.json` values, validated against the live file's
   length+mtime; a changed manifest is re-read and `/reload` clears the cache.

Remaining limit: one host bridge call that blocks longer than the budget (a
huge `readdir`/`readFile`, an uninterruptible OS call) cannot be interrupted by
QuickJS; the caller's deadline returns, but the actor stays inside that call
until it returns. Raise `extensionLimits.callTimeoutMs` /
`beforeAgentStartTimeoutMs` for legitimate handlers that need more.

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

`registerTool` (with `defaultActive`/`exposure`), `registerCommand`
(slash commands with flags), `registerMessageRenderer` /
`registerEntryRenderer` / `registerMarkdownTransformer`,
`registerProvider` / `registerVirtualModel` (see
[custom-provider.md](custom-provider.md)), `registerShortcut`,
`registerFlag`, `registerMCPServer`, `events.on/emit`, and the failure mode
is fail-closed: unknown event names or invalid schemas are load errors.

## The `ctx` surface

- `ctx.model`, `ctx.thinkingLevel`, `ctx.hasUI`, `ctx.isProjectTrusted()`
- `ctx.ui.*`: `select/confirm/input/editor` (dialog bridges with optional
  request-level `timeout`), `notify`, `setTitle`, `setWidget`,
  `setStatus`, `getEditorText`
- `ctx.newSession / fork / navigateTree / switchSession / reload /
  waitForIdle` (the command-context actions)
- `ctx.executeTool(name, params, { background: true })` (nested calls,
  depth-capped; with `background` the call detaches — the awaited envelope is
  a placeholder carrying the call's id, and the real result arrives later as a
  `tool_result` event with the same `toolCallId`)
- `ctx.compact(options)`, `ctx.abort()`, `ctx.exec` (host-backed exec with
  abort)
- `ctx.sessionManager`: the scalar getters (`getSessionId`, `getSessionPath`,
  `getCwd`, `getLeafId`, `getHeader`, `getLabel`) read dedicated host fields
  and cost one lookup; the entry and collection getters (`getEntries`,
  `getTree`, `getBranch`, `getChildren`, `getSessions`, `getEntry`,
  `getLeafEntry`, `buildContextEntries`, `buildSessionProjection`) still
  parse the full snapshot. The session-context snapshot is cached by a
  session fingerprint: an unchanged push rebuilds nothing and skips the
  plugin `on_session_context` fan-out.

## Resource limits

Extensions share one QuickJS runtime; pir caps its resources so a runaway or
memory-hungry extension fails its own call instead of taking the host process
down (upstream `pi` runs on Node and cannot enforce any of this).

| Setting (`extensionLimits`) | Env override | Default | Meaning |
|---|---|---|---|
| `memoryMb` | `PI_EXT_MEMORY_MB` | 128 MiB | QuickJS heap cap of the shared runtime; `0` disables the cap |
| `stackMb` | `PI_EXT_STACK_MB` | 16 MiB | JS stack; `0` keeps the default, values above 16 are clamped to 16. The worker runtime always requests `WORKER_JS_STACK_BYTES` (16 MiB): rquickjs's `set_max_stack_size` silently reads ANY request above 16 MiB as `0`, which disables the stack check instead of reporting an error (a worker asking for 32 MiB used to run with the guard off). A lowered value applies to the shared host runtime, not the worker |
| `gcThresholdMb` | `PI_EXT_GC_THRESHOLD_MB` | QuickJS default | GC threshold |
| `callTimeoutMs` | `PI_EXT_CALL_TIMEOUT_MS` | 30000 | Per-call timeout of event dispatch and tool execution |
| `hookTimeoutMs` | `PI_EXT_HOOK_TIMEOUT_MS` | 1800000 | Timeout of reserved-prefix hook calls (`__pir_stream:` / `__pir_route:`) |
| `beforeAgentStartTimeoutMs` | `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS` | 10000 | Budget of the `before_agent_start` lifecycle dispatch; a timed-out dispatch is abandoned and the run proceeds |

The shared runtime is capped at **128 MiB** of QuickJS heap by default, so a
runaway extension fails its own call instead of OOM-killing the host.
`memoryMb`/`PI_EXT_MEMORY_MB` change the cap; `0` disables it. `0` also means
"no limit" for `gcThresholdMb`; for `stackMb` and the timeouts `0` keeps the
default (the stack check must stay enabled and interrupts need a positive
deadline). Invalid env values are ignored with a diagnostic.

```json
// ~/.pi/agent/settings.json
{ "extensionLimits": { "memoryMb": 512, "callTimeoutMs": 60000 } }
```

**Global scope only.** The engine reads `<agentDir>/settings.json`
(`~/.pi/agent/settings.json`) plus the `PI_EXT_*` variables. A project
`.pi/settings.json` can never loosen these limits, trusted or not: the
repository an extension came from must not be able to opt out of the host's
own safety net. Resolution order: an explicitly injected
`ExtensionConfig::limits` (SDK/embedders) wins verbatim, then `PI_EXT_*`,
then the global settings object, then the defaults. The limits are re-applied
on `/reload` (a runtime `set_call_timeout` override is not sticky across a
reload).

`pir js` is the deliberate exception: it runs on the same QuickJS engine but
does **not** inherit the extension host's heap policy. A standalone script
gets no heap cap by default (large JSON/files are its normal workload), and
`PIR_JS_MEMORY_MB` opts into one (`0` or absent = no cap; a malformed value is
reported on stderr and ignored). It reads neither `PI_EXT_MEMORY_MB` nor
`extensionLimits`; the stack and timeout fields keep the extension defaults,
while the script thread drives its own OS stack.

### When the heap cap trips

The over-allocating call fails with a clear, actionable error; the session and
every other extension stay usable (the pending exception is cleared, the job
queue drained and a GC runs):

```
tool `memoryHog` failed: extension exceeded its JS memory limit (512 MB);
raise `extensionLimits.memoryMb` in ~/.pi/agent/settings.json or set
PI_EXT_MEMORY_MB (0 = no limit)
```

Approaching the cap (≥ 80%) logs one warning per crossing — and, with
`PI_TIMING=1`, one line per extension load reports the resolved limits:

```
pir-ext: JS heap at 430 MB of the 512 MB memory limit (83%); raise `extensionLimits.memoryMb` ...
pir-ext: resolved limits: memory=512 MB stack=16 MB gc_threshold=default call_timeout=30000 ms hook_timeout=1800000 ms
```

The codemode sandbox keeps its own per-script cap (`codemode.sandboxMemoryMb`,
default 128 MiB, `PIR_CODEMODE_MEMORY_MB` override, `0` = no cap) applied to
both sandbox engines — see [codemode.md](codemode.md).

### Queue bounds

Every host↔JS queue an extension can drive is bounded, and the bound is
visible. The rule: **if the source can pause, the producer waits** (real
backpressure — the OS pipe, socket or transport stops being read); only where
Node itself coalesces or truncates (a watcher's notifications, the blocking
`spawnSync` form) does the queue drop, and every drop is counted and reported
through `ext_diag!` (`[ext] …` / `[env-daemon] …` / `[env-client] …` /
`[session] …` lines; repeat-collapsed by the logging router). The provider
event stream (`src/ai/event_stream.rs`, capacity 32 + producer backpressure)
is the same pattern on the AI side.

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

- `fs.watch` uses the native `notify` backend (the `extensions` feature
  enables `dep:notify`): a native event triggers the mark-and-sweep
  immediately. The fallback poll is adaptive — 200 ms while changes are
  observed, backing off to 2 s after five quiet sweeps, and reset to the
  fast cadence by any change or native wake-up.
- Only relative imports and bundled modules resolve; Node built-ins
  (`node:fs` etc.) and npm dependencies are unsupported by design.
- Scripts run on a dedicated actor thread; Rust↔JS round-trips interleave
  without deadlock. A panic or `unload` invalidates captured `pi` handles.
- Reload with `/reload`: extensions re-execute, tools/renderers from the
  discarded load are removed first. Each runtime generation owns its
  process-level resources: shutdown (explicitly before the actor exits, and
  again from the runtime's `Drop`) releases that generation's child
  processes, worker threads, in-flight fetches, HTTP servers (and buffered
  request bodies), TCP/Unix servers and sockets, `fs.watch` threads, and fs
  descriptors. The release is generation-scoped, so a reload cannot leak the
  discarded generation's live processes, threads, listeners or ports into its
  successor.
