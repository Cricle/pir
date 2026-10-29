# MAPPING.md — TS → Rust module mapping

Per-module truth for the upstream `pi` → pir port. Current state and evidence:
[`docs/STATUS.md`](docs/STATUS.md); feature contract: [`docs/FEATURES.md`](docs/FEATURES.md).

**Last verified: 2026-10-22**; later rows carry their own dates.

## Legend

`planned` not started · `in_progress` partial · `done` migrated, gate passes,
tests present · `n/a` dropped by decision (a parenthetical says what part, if
any, is kept).

## Module mapping

| Rust module | TS source | Status | Notes |
|---|---|---|---|
| `src/error.rs` | `packages/protocol/src`, `packages/ai/src/types.ts` | done | Typed error codes + context-carrying errors. |
| `src/config/` (core) | `packages/coding-agent/src/config.ts` | done | Env, validated ToolExecutionMode, scheduler config. |
| `src/protocol/` | `packages/protocol/src` | done | Strict CBOR, framing, codec; PROTOCOL_VERSION 8. |
| `src/telemetry/` | `packages/telemetry/src` | done | Vendor-neutral core API: spans/links/status, counters/gauges/histograms, W3C `traceparent` propagation, `Telemetry` facade over `TelemetryRecorder`, `TestTelemetry`; OTel mapping in `src/telemetry/mod.rs`. AI path: `StreamOptions::telemetry` parents one `pi.ai.request` span per request, first-chunk/block events, duration/token metrics, trace headers at the provider boundary. No production exporter/backend ships in-tree (explicit N/A). Tests: `tests/suites/telemetry_core_w10.rs`, `ai_telemetry_w11.rs`. |
| `src/durable/chord/` | `packages/chord/src` | n/a (delta subset kept) | Only the delta closure: `diff.rs` (`diff_revisions`/`diff_objects`/`overlap`) and `apply.rs` (`apply_immutable_trusted`); `src/durable/delta.rs` carries `Op`/`Seg`/`Path`. Full chord is deferred/N-A by user decision (2026-10-21); chord-lite is unscheduled. |
| `src/ai/` | `packages/ai/src` | done | Core types, event stream, registry, bounded transport; OpenAI/Anthropic/Google SSE + responses/codex/azure/bedrock+SigV4/vertex/mistral adapters; thinking configs + retry classification; OAuth (9 flows); catalog **42 providers / 1,756 entries** (`ai_catalog_full_w10`). Shared `Arc<Context>` provider boundary; lazy message transforms; COW stream snapshots; bounded event stream (32, producer backpressure); structural `partial` mirror refresh. |
| `src/agent/` | `packages/agent/src` | done | Tool abstraction, loop, batch scheduler, per-file mutation queues, cancellation, steering/follow-up; chained `message_end` replacement before persistence/preflight (`tests/suites/tool_repair.rs`); finishTurn/prepareRequest; nested-call depth cap 16. Background/detach: `background: true` is stripped by preflight and the call runs detached with its own AbortSignal (`tool_batch.rs`); detached calls do not join batch-degrade decisions. Degenerate-loop guard: 20 consecutive identical single-tool executions (`REPEAT_STREAK_STOP = 20`, name+canonical args+final result+is_error after `message_end`) synthesize a `StopReason::Error` terminal. Print/RPC/server and `AgentBuilder` default to 1,000 turns (`PIR_MAX_TURNS` only for the CLI hosts; raw `CreateAgentSessionOptions` stays at 100); the TUI stays at 100. Loop handoffs are `Arc`-shared (`AgentStart`/`TurnStart.context`, `LoopResult`, `AgentEnd.result`); `run_with_options_shared` returns the `Arc`. |
| `src/agent_session/` | `packages/coding-agent/src/core` (AgentSession) | done | Headless facade over registry+tools+storage+compaction+loop; steeringMode/followUpMode (one-at-a-time); full session-control surface; `navigate_tree` summarization; AbortSignal backed by CancellationToken; copy-on-write history; single-consumer inbox; background tool-call registry (`background.rs`, limit 4 via `max_background_calls`, completion injects `backgroundToolResult` + `BackgroundToolEnd`; `abort_background`/`abort_all_background`/`background_calls`; Drop aborts in flight). `PI_TIMING` per-turn diagnostics + `last_run_diagnostics`. |
| `src/system_prompt.rs` | coding-agent core | done | Ordered section builder, XML wrapping, section diff. |
| `src/skills.rs` | `core/skills.ts` | done | SKILL.md validation/discovery + `<available_skills>` XML; structured warnings/collision diagnostics; symlink-following discovery. |
| `src/session_manager/` | `core/session-manager.ts` | done (rewritten for upstream interop) | On-disk version-3 JSONL (8-hex ids, 11 entry types, projection, `firstKeptEntryId`) shared with pi; old pir v4 files skipped, not migrated. Paged tail-first open (`open_paged` + `tail_page.rs`) keeps a bounded resident window, skipped prefix on disk. Stack-safe tree emission (`TreeChildren` iterative Drop, `to_value_iterative`/`forest_to_json_iterative`, `for_each_tree_row` + `TreeRowMeta`/`TreeScanStats`) works whole-session for both managers, streaming a paged prefix as row metadata; `json_depth.rs` has an iterative `Value`→text writer/teardown. Appends reuse one handle with per-line flush; fsync deferred to `sync_now()`/Drop; length re-check detects external deletion. Session listing is one pass per file with in-memory cwd filtering. |
| `src/client/` | `packages/client/src` | done | TCP client, versioned hello, framed CBOR, timeouts; `reconnect()` + ReconnectPolicy + typed connection-state listeners + attach/subscription restore. |
| `src/server/` | `packages/server/src` | done | TCP + Unix-socket presets, hello handshake, framed CBOR, snapshot; JSONL-persisted session hosting, multi-attach broadcast, draining with `server_draining`. |
| `src/coding_agent/` | `packages/coding-agent/src` | done | Core tools with a persistent shell pool and runtime-death guards; bash/powershell export `PIR_BIN` and prepend its dir to `PATH`. Print/RPC/JSON hosts, upstream CLI contract, skills/prompt-template wiring, `--export` HTML, crash log, package manager, in-process grep/find, OpenAI-compat detect. `tools.commandTimeoutMs` (default 120000; `0` disables) caps foreground shells; 30 s no-output heartbeat; a busy pool member >5 s falls back to a fresh shell; print/RPC signals abort with 130/143/129. Tool lifecycle frames carry upstream `args`/`result` shapes. Security: `read`/`write`/`edit` re-establish confinement against open handles; spill files are owner-only and swept; remote write/edit is atomic; out-of-loop bash arms `ShellTreeKill`. Tests: `tools_security_w9`, `env_client_e2e`, `bash_tree_kill_prompt`. |
| `src/tui/` | `packages/tui/src` | done | ratatui/crossterm loop, multiline editor, unicode-safe width, markdown/diff streaming renderers, fuzzy autocomplete, keybindings, OKLCH/OKHSL themes, clipboard, dialogs, OSC 7501. Alt-screen viewport: scrollbar scrub/hover/jump-to-end, search box, right-click paste; windowed transcript + paged history; whole-history search (bounded backward walk, `tests/tui_search_whole_history.rs`); `/tree` streams without widening. Extension renderer/widget/footer/header callbacks run with async `ctx.ui` suppressed. Syntax highlighting (`tests/suites/tui_syntax_highlight.rs`); clipboard paste (`tui_clipboard_paste.rs`); settings diagnostics; native Shift+Enter; settings-routed `tui.altScreen.*` (`tui_gaps_w11.rs`). Steady-frame call lines and write preview memoized (`tests/tui_frame_reuse.rs`). |
| `src/extensions/` | `core/extensions` | done | QuickJS (rquickjs + swc): full event surface, nested `ctx.executeTool`, dialogs, sessionManager, `registerProvider`/`registerVirtualModel`, `/reload`; npm resolution + CJS/ESM interop. Non-goals: libnode/node subprocess, native addons, stateful pi-tui components. Per-extension limits from GLOBAL `extensionLimits` + `PI_EXT_*`; `fs.watch` on notify with adaptive poll; scalar sessionManager getters read host fields; snapshot fingerprint-cached; builtin-only plans never start QuickJS (`tests/builtin_only_no_js.rs`). |
| `src/plugins/` | upstream extension architecture | done | Unified `AgentPlugin` layer: Rust builtins + JS adapter share one registry with upstream conflict semantics. Builtins: `subagent`, `mcp` (rmcp 3.5 + OAuth; `builtinTools`/`exposure` resolution), `codemode` (worker process, thread fallback), `python`, `context-mode`, `tool-search`, `tool-repair` (native wins over JS), `todo` (contract in `docs/todo-builtin.md`). MCP connects single-flight; loader default-disables `@malinamnam/pi-phone` (global opt-in or explicit `-e`), native `phone` builtin default-off; codemode live-worker cap 2 (third queues); deterministic tool_search ordering; subagent description from the resolved agent dir. Tests: `plugins_w11`. |
| `src/config/` (settings) | `core/settings-manager.ts` + schema/defaults | done | Full settings surface (all fields, deep merge, locked persistence, 4 migrations), trust.json + project trust, models.json `$ENV`/`!cmd` templates, keybindings/models-store/theme schemas in `schemas/`, model resolution order, `deny_unknown_fields` on MCP server config. `mcp.json` loader: upstream wrapper + legacy flat shape, `autoEnableCodemode`, pir-specific `builtinTools` (default `true`; `false` restores upstream codemode default; explicit exposure overrides win). |
| `src/sdk/` | `core/sdk.ts` + `packages/agent/src/agent.ts` | done | `AgentBuilder` → cheap-clone `Agent` → typed `SdkEvent` stream (`#[non_exhaustive]`, bounded backpressure); typed `SdkError`, no anyhow leakage; doctests on every public item; examples in `examples/`. Full reference: `docs/sdk.md`. |
| `src/compaction/` | `core/compaction` | done | Upstream algorithm at HEAD semantics (projected cut points, split-turn, checkpoint prompts; estimate prefix-timestamp guard) + branch summarization (prompts verbatim). |
| `src/env_daemon/` | `packages/env/daemon` (protocol v1) | done | Statement-by-statement port: 13-byte frame codec, `PI-ENV <token>` sync line, file family, listFiles/listDir (POSIX byte order, lstat), exec (timeout/spill/window/inheritEnv/shell probe), watch, pty (Unix), per-platform `sys/`. Exposed as `pir env-daemon`. Tests: `tests/env_daemon_protocol.rs` (26), `tests/env_daemon_js_shim.rs`. |
| `src/env_client/` | `packages/env` (client) + `js/pi_env_shim.js.txt` | done | `RemoteExecutionEnv` equivalent (file family/listFiles/exec with streaming+abort/watch with reconnect backoff), SSH probe + deploy (SHA-256 image name, host-key pinning, 15 `-o` no-prompt flags), remote POSIX/Win32 path lexing, bash/read/write/edit indirection (`PI_ENV_SSH_*` + `install_override`; zero change when unconfigured). Remote bash spill stays in the client's local temp (upstream behavior); daemon-side spill is out of scope. Remote `shellPath` via `PI_ENV_REMOTE_SHELL_PATH`. `BinaryReader`/`DirReader`/`TextLineReader` ported. Tests: `tests/upstream_env_protocol.rs`, `tests/env_client_e2e.rs`. |
| `src/durable/` (kernel core) | `packages/durable/src/{ids,errors,entries,types,documents,tasks,timers,truncate,search}.ts` + `@earendil-works/chord/{delta,context}` (subset) | done | Native record/storage kernel: `ids.rs`, `errors.rs`, `entries.rs`, `types.rs`, `documents.rs`, `tasks.rs`, `timers.rs`, `truncate.rs`, `search.rs`; chord `delta.rs` + `chord/{mod,diff,apply}.rs`, `context.rs`. Atomic JSONL/memory commits, branded ids/`Seq`, typed storage errors, document definitions, task metadata, host timers + `sleepUntil`, head truncation, BM25 search, trusted revision diff/apply. Caller supplies the storage root. Tests: `tests/durable_storage.rs`, `tests/suites/durable_{primitives,delta,chord_diff,definitions,context}.rs`, `tests/durable_shim.rs`. |
| `src/durable/session/` | `packages/durable/src/session/{session,transaction,observation,forks}.ts` | done | `session.ts` → `line.rs`/`guard.rs`/`documents.rs`/`publication.rs`/`attach.rs`/`facade.rs`; `transaction.ts` → `transaction.rs`+`assemble.rs`; `observation.ts` → `observation.rs`; `forks.ts` → `forks.rs`. Tests: `tests/suites/durable_{assemble,attach,document_cache,facade,forks,guard,observer,publication,transaction}.rs`. |
| `src/durable/harness/` | `packages/durable/src/harness/*.ts` | done | Every upstream module has a local counterpart (`settings`/`agent`, `registry`, `scheduler/*`, `generation`, `compaction`, `context`, `prompt`, `live`, `inbox`, `submissions`, `usage`, `view`, `task-graph`, `json`, `util`, `provider`, `tool`, `events`, `telemetry`, `harness` + built-in `documents`). All declarations land; admission/startRun/task creation commit atomically; telemetry sink forwards into `crate::telemetry`; `TaskRecord.links` become span links; `watch_doc` delivers off the commit line; `latest_head` gives read-your-writes; SQLite `submission_by_request` is deterministic. Deviations: `Waiters` unported, `tool.rs` reads-before-commit/two commits for nested calls, duplicate phase names rejected where upstream last-wins. Tests: one suite per module under `tests/suites/durable_*.rs`; `durable_residuals_w11.rs`. |
| `src/durable/storage/` | `packages/durable/src/storage/{jsonl,memory,scan}.ts` | done | `jsonl.rs`, `memory.rs`, `scan.rs` share one conformance suite; JSONL covers reopen, torn-line recovery, corruption detection, `fsync`. `storage/sqlite/**` is a third backend (WAL, schema v1/v2, atomic commits, scans/watermarks); `database.ts`/`node.ts`/`cloudflare.ts` and the `prefix` option are out of scope. Tests: `tests/durable_storage.rs`, `tests/suites/durable_storage_conformance.rs`, `tests/durable_shim.rs`. |
| `src/durable/pi_format/` | — (pir-native bridge) | done | pi version-3 JSONL ↔ durable records: `import.rs` maps every entry type (unknown → `OpaqueEntry`); `export.rs` renders committed entries/documents to a file `SessionManager::open` accepts. Lossy mappings in `pi_format/mod.rs`. Tests: `tests/suites/pi_format_{import,export,roundtrip}.rs`. |
| `src/durable/{codemode,tool-search,tools,images,env,testing}/` | `packages/durable/src/{...}/**` | n/a | Deliberately not ported; pir has native equivalents (`plugins::codemode`, `plugins::tool_search`, `coding_agent::tools`, `image` feature, `env_client`/`env_daemon`, test suites). `tests/durable_shim.rs` locks the published shim surface only. |
| `src/durable/storage/sqlite/` | `packages/durable/src/storage/sqlite/{index,node,cloudflare,database,migrations,schema,storage}.ts` | done | `migrations.rs` reproduces schema v1/v2 (sleeping-status rebuild), `schema.rs` mirrors names/indexes, `storage.rs` implements `Storage` with WAL, `BEGIN IMMEDIATE`, `PRAGMA synchronous`, scans/watermarks and document base/delta materialization. Out of scope: `database.ts`, `node.ts`, `cloudflare.ts`, `prefix`. Tests: `tests/suites/durable_storage_conformance.rs`. |
| `src/native_cli/` | — (pir-native; reuses grep engine + RustPython) | done | `pir grep`/`sed`/`python`: standalone surface (no agent/session/model/config load), argv forwarded verbatim; common cases in-process, unsupported cases exec the system tool. `--no-fallback`/`PIR_NATIVE_FALLBACK=0\|false` disables; GNU exit codes. Dispatch: `Command::{Grep,Sed,Python}` (`coding_agent/cli.rs`) → `main_cli/run_mode.rs` → `native_cli::run_*`. |
| `src/shell_embed/` | — (pir-native; third-party `brush`) | done | Embedded bash behind feature `brush`: hidden `pir __brush [-c <script>]` hosts the pool's persistent bash member; `shellPath` still wins; without the feature the pool spawns the external shell. `shell_embed::run_brush_main`, `embedded_shell_program()`. |
| `src/mcp/server/` | — (pir-native; third-party `chmln/sd`) | done | Built-in `sd` search-and-replace MCP server (`pir mcp-sd [--root <dir>]`, stdio, rmcp server half). Tool `sd`: regex/literal pattern + `$1`/`${name}`/`$$` replacement, case/word/multiline flags, gitignore-aware walk, binary skip, `dry_run` default true, match/file guards reported as `truncated`, paths outside `--root` refused. Feature `mcp-sd`. Tests: `tests/mcp_units.rs`, `tests/mcp_integration.rs`. |

## Feature trimming

Library-first: `default = ["full"]` keeps the CLI/CI/test build byte-identical;
`cargo check --lib --no-default-features` is the minimal-SDK gate. `durable` is
core. See [`docs/FEATURES.md`](docs/FEATURES.md); `Cargo.toml` `[features]` is
authoritative.

## Known deviations & deliberate absences

Current decisions only; superseded/landed items were removed in the 2026-10-29
cleanup (their landing points are frozen in code and tests).

### Deliberate absences

- **/llama router extension** — bundles an external llama.cpp server → not ported.
- **Easter eggs & first-run wizard** — decorative → not ported.
- **update check** — pir shows a prompt (silent background check + status hint); no npm self-update semantics; `--force` is a CLI-compat no-op.
- **classifier/image residuals** — system is ported; llama-cpp-classify is not; OpenRouter image prices are recorded as 0 (upstream fetches them dynamically).
- **withSession / setup callbacks** — unsupported; the JSON host-function bridge cannot express objects with methods.
- **stateful render components** — unsupported; renderers bridge as bounded ANSI line arrays.

### Deliberate behavior deviations

- **codemode** — parent/child share one settings snapshot and event re-dispatch; scripts receive `(prompt, {provider, model})`, not a resolved model object.
- **subagent trust gate** — safe-path rejection text instead of an interactive confirm.
- **grep** — `require_git(false)`: honors `.gitignore` outside git repos.
- **read/edit** — binary files are refused instead of lossy-decoded.
- **AWS credential chain** — short IMDS timeout; ECS FULL_URI restricted to loopback/link-local; `credential_process` trusted as user config.
- **print/JSON mode** — markdown renders after the message settles, not while streaming.
- **vertex** — GCE metadata fallback and non-service-account key types follow upstream code.
- **remote server transport** — TCP token authenticates but is not TLS; encryption is an operator boundary.
- **session service topology** — one `pir.session` service; upstream's experimental chord family is not aligned.

### Degraded paths (platform / ecosystem limits)

- **OSC 7501 / OSC 11** — crossterm swallows bare replies; both degrade to unsupported by default; `PI_PROGRAM_STATUS=1` and the pre-event-loop theme read force enable.
- **terminal image protocols** — halfblocks only (default-off feature); no kitty/iterm2 probing.
- **ctx.ui chrome** — `setFooter`/`setHeader`/`setWorkingIndicator`/`setHiddenThinkingLabel`/`ui.custom` are no-op shims; `setWidget` lacks placement/frames options.
- **CLI cosmetics** — `--version` shape and hand-written help sections differ cosmetically; `-p ---foo` parses differently (clap).
- **extensions/api/prelude.rs** — one byte-frozen JS asset, not split into modules.
- **syntax highlighting** — TUI half is ported; export HTML remains a static, script-free subset (`export_html.rs`).
- **keyboard protocols** — upstream `enableModifyOtherKeys` is not sent (crossterm drops it); native Shift+Enter and alt-screen bindings are implemented.
- **codemode script surface** — `ALL_TOOLS`/`searchTools()`/`describeTool`/`describeNamespace`/`store()`/`load()` and `models.getModelOfType` are not exposed; `models.ImagesContext` lacks upstream's strict unknown-field rejection.
- **Codex WebSocket** — ported (`codex_ws.rs`: pooled connection, `previous_response_id`, zstd, SSE fallback); setting `transport = "sse"` opts out.

## Wave history

Q1–Q10, G1–G11 and the 2026-10-10 alignment waves are closed; landing points
and tests are frozen in code. Historical ledgers were removed in the
2026-10-28/29 cleanup. Current evidence: [`docs/STATUS.md`](docs/STATUS.md) and
`logs/acc-final/` (the old `logs/phase2/` ledger was removed in the docs cleanup).
