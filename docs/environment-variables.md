# Environment variables

## Agent dir & sessions

| Variable | Meaning |
|---|---|
| `PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR` | Agent directory override, first non-empty wins (default `~/.pi/agent`) |
| `PI_CODING_AGENT_SESSION_DIR` / `PIR_SESSION_DIR` | Session storage location, first non-empty wins (`--session-dir` and settings `sessionDir` participate) |
| `PI_EXPERIMENTAL=1` | Upstream `areExperimentalFeaturesEnabled`. Exact `1` only. Together with no agent-dir override and no existing settings file, it is what makes the first-run setup dialog appear |
| `PI_EXT_RELOAD=1` | Exact `1` disables the persisted extension-failure negative cache, forcing every host through a real load. The cache only applies to the one-shot headless hosts (`print`, `--mode json`); interactive/TUI/RPC hosts never skip a path, so this switch matters when re-testing a fixed extension in a print run |
| `PI_SESSION_FILE` / `PI_SESSION_ID` | Exported by the host into `bash` child environments and the shared shell pool; not read as configuration |
| `PIR_BIN` | Exported into every `bash` child: the absolute path of the running `pir` binary, with that binary's directory prepended to the child `PATH` (following the shell dialect, so a POSIX shell never gets a `;`+`:` hybrid). That is what lets a command call `pir grep\|sed …` directly. Set by pir, not read as user configuration |
| `PIR_SESSION_FILE` | Best-effort session-file hint for the crash logger (hosts set it when a session is open; unset or empty = no hint) |
| `PIR_DISABLE_SESSION_MMAP` | Test/diagnostic escape hatch: any value forces the streaming session loader instead of the read-only `memmap2` map (the reference loader for before/after measurements) |
| `PI_PACKAGE_DIR` | Package asset directory anchoring the prompt's `<docs>` section (upstream `getPackageDir`; for Nix/Guix store paths). Set and non-empty wins; unset/empty falls back to the running `pir` executable's directory when it holds a `README.md` file or a `docs/` directory (the release archives ship `pir`, `pir-phone-server`, `README.md`), mirroring upstream's compiled-binary `dirname(process.execPath)` branch. Neither resolves = no `<docs>` section |

## Models & providers

| Variable | Meaning |
|---|---|
| `PI_PROVIDER` / `PI_MODEL` / `PI_REASONING_LEVEL` | Default provider/model/thinking overrides |
| `PI_CACHE_RETENTION=long` | pi-messages default cache retention |
| `PI_HTTP_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` | Request proxy (after settings `httpProxy`) |
| `PI_OAUTH_CALLBACK_HOST` | OAuth loopback callback host |
| `PI_CATALOG_BASE_URL` / `PI_OFFLINE` / `PI_SKIP_VERSION_CHECK` | Remote catalog endpoint override / disable every network call (catalog overlays, update check) / disable the startup update check |
| `ANTHROPIC_BASE_URL` | Claude Code gateway URL for the anthropic provider (models.json `baseUrl` wins) |
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_OAUTH_TOKEN` | Bearer credential for anthropic requests (`Authorization: Bearer`) |
| `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL` | Claude Code model ids; synthesized into the anthropic provider when not in models.json |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` / `CLAUDE_CODE_MAX_OUTPUT_TOKENS` | Context window / max output for the env-synthesized model (defaults 200000 / 32000) |
| `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST` | OAuth host for the Kimi coding provider, first non-blank wins (default `https://auth.kimi.com`); trailing slashes are stripped. Upstream `getOauthHost` |
| `RUST_LOG` | Standard `tracing` filter, honored when a subscriber is installed. When none is, a filter at `debug`/`trace` still routes log-only diagnostics to stderr (the same effect as `--verbose`). See also `PIR_VERBOSE` |

## Tools & execution

| Variable | Meaning |
|---|---|
| `PIR_TOOL_EXECUTION` / `PIR_MAX_PARALLEL_TOOLS` / `PIR_FAIL_FAST` | Tool execution mode override / parallel tool batch cap / fail-fast tool batches |
| `PIR_MAX_TURNS` | Per-prompt turn cap for print/RPC/server (default 1000; one turn = one model request, not one tool call). Only a positive integer is accepted; `0`/junk keeps the default. SDK `AgentBuilder` has its own fixed 1000 default and raw `CreateAgentSessionOptions` defaults to 100; neither reads this variable. TUI ignores it and keeps its 100-turn cap |
| `PIR_CODEMODE_WORKER=0` | Disable the codemode worker process (thread fallback) |
| `PI_CODEMODE_MAX_WORKERS` | Maximum codemode worker processes alive at once across all sessions in the process (default `2`). A positive integer overrides; `0`, negative or junk keeps the default. Read on every admission, so a host can raise it at runtime. Further scripts wait for a free worker; raise it when one process runs several sessions, such as in-process subagents |
| `PIR_CODEMODE_TIMEOUT_MS` / `PIR_CODEMODE_MAX_OUTPUT_CHARS` / `PIR_CODEMODE_MAX_OUTPUT_ITEMS` | Codemode sandbox limits |
| `PIR_CODEMODE_MEMORY_MB` | Codemode sandbox heap cap override (`0` = no cap; default 128) |
| `PIR_TOOL_MAX_SOURCE_BYTES` | Whole-file source ceiling for `edit`, image decode and grep context reads (default 67108864 = 64 MiB; `read` streams and is not capped). A positive byte count overrides; `0`/malformed falls back rather than disabling the guard |
| `PIR_READ_BEFORE_EDIT` | Gate for `edit`/`write` on an existing file: `off` (no check), `warn` (proceed with a warning), `edit-warn` (edit may proceed with a warning for partial/stale reads; write stays strict), `edit-partial` (edit may proceed with a warning after a partial read; stale/never-read targets and `write` stay strict; default), or `enforce` (refuse any incomplete read). Case-insensitive; `0`/`false`/`disabled` = `off`, `warning` = `warn`; unset/unrecognized = `edit-partial` in both balanced and efficient. A partial read must still be unchanged (mtime/size); new-file writes bypass the check; successful writes/edits refresh the ledger; resolved once at session build |
| `PIR_TOOL_OUTPUT_MAX_LINES` / `PIR_TOOL_OUTPUT_MAX_BYTES` | `read`/`bash` output truncation caps. Positive integers override; otherwise `PIR_PERF_PROFILE=efficient` uses 1,000 lines / 24 KiB, and every other profile uses the historical 2,000 lines / 50 KiB. The `read` continuation footer and the `bash` tail footer report the effective limit |
| `PIR_NATIVE_FALLBACK=0` | Disable the `pir grep\|sed` fallback to the system tool (`0`/`false`; unset or any other value keeps it enabled) |
| `PIR_CONTEXT_MODE_ROUTING` / `PIR_CONTEXT_MODE_INDEX_RESULTS` | `context-mode` `tool_call` routing / `tool_result` auto-indexing. Both default on; disabled by `0`, `false`, `off` or `no` (unset or any other value keeps it on) |
| `PI_EXT_MEMORY_MB` / `PI_EXT_STACK_MB` / `PI_EXT_GC_THRESHOLD_MB` / `PI_EXT_CALL_TIMEOUT_MS` / `PI_EXT_HOOK_TIMEOUT_MS` / `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS` | Extension runtime QuickJS limits and lifecycle budgets (override `extensionLimits`; see [extensions.md](extensions.md)) |
| `PIR_GC_PROFILE` | Named GC-pacing profile for the extension runtime: `low` / `balanced` (4 MiB GC threshold) / `throughput` (16 MiB). Sets only the BASE default — the settings file and `PI_EXT_GC_THRESHOLD_MB` override it. Empty or unknown value is ignored with a diagnostic and keeps the built-in default |
| `PIR_EXT_BYTECODE` | `0`/`false`/`off` forces every JS shim through the source path instead of the ahead-of-time QuickJS bytecode blob. Development aid: the blob's guard header records the payload, not the source, so editing a shim `.js.txt` without regenerating would otherwise load stale code |
| `PIR_EXT_BYTECODE_CACHE` | `0`/`false`/`off` disables the on-disk + in-memory transpiled-module cache. Default on: modules are keyed by a schema hash, capped at 512 entries / 32 MiB in memory and 1024 files / 64 MiB on disk (evicting down to 768 / 48 MiB) |
| `PIR_EXT_SHIM_SRC` | Non-empty (and not `0`/`false`/`off`) reads each JS shim from the checkout's `src/extensions/api/js/*.js.txt` instead of the embedded blob. Only active when the compile-time checkout path still exists, so an installed or moved binary silently keeps the blob |
| `PIR_QJS_ALLOC` / `PIR_QJS_ALLOC_STATS` | `PIR_QJS_ALLOC=pool` (case-insensitive) opts the extension runtime into the pool allocator; every other value including unset keeps the default allocator. `PIR_QJS_ALLOC_STATS=1` (exact) collects the pool allocator's statistics; ignored when the pool is not selected |
| `PIR_PACKAGE_COMMAND_TIMEOUT_MS` | Deadline for one package-manager / git child process (default 300000 = 5 minutes). A positive integer overrides so a slow network can finish; `0`/malformed keeps the default. A hung registry lookup or clone must not block startup or an interactive `pir install` forever |
| `PIR_JS_MEMORY_MB` | Heap cap of a `pir js` run in MiB. `0`/unset = no cap. A malformed value (including negative) is reported on stderr and ignored. `pir js` reads neither `PI_EXT_MEMORY_MB` nor `extensionLimits` |
| `PIR_MIN_BASE_URL` / `PIR_MIN_API_KEY` / `PIR_MIN_MODEL` / `PIR_MIN_SYSTEM_PROMPT` / `PIR_MIN_CWD` | Defaults for the zero-feature `pir-minimal` binary only, each used when the matching CLI flag is absent: endpoint, credential (falls back to `OPENAI_API_KEY`), model id, system prompt, and working directory (default = the process cwd). Blank values are skipped. The normal `pir` binary ignores all five |
| `PIR_NODE_SHIM` | Marker set on every child the node-executable shim routed to the JS runner. Set by pir, not read as user configuration |

## Subagents

| Variable | Meaning |
|---|---|
| `PIR_SUBAGENT_MAX_TURNS` | Per-child turn cap (one turn = one model request; default from `performance.subagentMaxTurns`: 1000 balanced, 100 efficient). A positive integer overrides the settings value; `0` removes the cap; malformed/negative values keep the resolved setting. Resolved at each `subagent` call and applied to every single/chain/parallel child |
| `PIR_SUBAGENT_MAX_TOTAL_TOKENS` | Per-child cumulative token budget (`input + output + cache_read + cache_write`). Default from `performance.subagentMaxTotalTokens`: unlimited on balanced, 100,000 on efficient. A positive integer overrides; `0` disables the budget; malformed/negative values keep the resolved setting |
| `PIR_SUBAGENT_WALL_CLOCK_MS` | Per-child wall-clock budget in milliseconds (default 1800000 = 30 minutes). A positive integer overrides; `0` disables the budget; malformed/negative values keep the default. Resolved at each `subagent` call and applied to every single/chain/parallel child |
| `PIR_SUBAGENT_DEPTH` / `PIR_SUBAGENT_API_KEY` | Internal markers the parent sets on a process-mode subagent child (depth is 1-based; the key travels in the environment rather than argv so it never lands in a process listing). Set by pir, not read as user configuration |

## Server / remote

| Variable | Meaning |
|---|---|
| `PIR_SERVER_ADDRESS`, `PIR_SERVER_AUTH_TOKEN` | Remote server bind / bearer token |
| `PIR_SERVER_ALLOWED_ROOTS` | Workspace roots the server may touch |
| `PIR_SERVER_MAX_CONNECTIONS`, `PIR_SERVER_MAX_CONNECTION_BODY_BYTES`, `PIR_SERVER_MAX_REQUEST_BODY_BYTES` | Server limits |
| `PIR_SERVER_MAX_SESSIONS` / `PIR_SERVER_MAX_SUBSCRIPTIONS` | Live session cap (default 256) / `pir.session` subscriptions per connection (default 64). Accepted `1..=1000000`; malformed/out-of-range keeps the default |
| `PIR_SERVER_IDLE_TIMEOUT_SECS` | Per-read idle timeout after handshake, in seconds, reset on activity (default 300). Accepted `1..=86400`; malformed/out-of-range keeps the default |
| `PIR_ALLOW_REMOTE_AGENT` / `PIR_ALLOW_REMOTE_BASH` / `PIR_ALLOW_PARALLEL_SIDE_EFFECTS` | Remote capability gates, each enabled by exactly `true` or `1`. `PIR_ALLOW_REMOTE_BASH` and `PIR_ALLOW_PARALLEL_SIDE_EFFECTS` default OFF; `PIR_ALLOW_REMOTE_AGENT` defaults **ON** (historical `Server::bind` behavior) — set it to `false` to refuse prompts from remote clients. See [security.md](security.md) |

`pir server` requires `PIR_SERVER_AUTH_TOKEN`; `--allow-unauthenticated` is the
explicit local-development opt-in (no environment-variable spelling). Loopback
is not user isolation: without a token any local process can use the server's
model credentials and working directory. Tokens are compared in constant time
and never logged.

## Runtime & threading

| Variable | Meaning |
|---|---|
| `PIR_TOKIO_WORKERS` / `PIR_TOKIO_MAX_BLOCKING` | Worker threads of the main tokio runtime (unset = tokio default, one per core; positive clamped to `1..=64`; `0`/malformed keeps the default) / blocking-pool ceiling (unset = two per core clamped to `4..=32`; positive clamped to `1..=512`; `0`/malformed keeps the adaptive value) |
| `PIR_ENV_DAEMON_WORKERS` / `PIR_ENV_DAEMON_MAX_LONG_REQUESTS` | `pir env-daemon` file-operation worker threads (unset = detected parallelism clamped to `2..=8`; positive clamped to `1..=64`; `0`/malformed keeps the adaptive value) / concurrent `exec`/`watch`/`pty` requests per daemon (default 32; positive clamped to `1..=256`; `0`/malformed keeps 32; past the cap gives a structured `EAGAIN`) |
| `PIR_EXECUTABLE` | Override the pir image for the local `env-daemon`, remote daemon upload, and child-session re-entry (unset/empty = running executable) |
| `PI_ENV_DAEMON` | Absolute path to a standalone `pi-env` daemon binary of any version (protocol 1 is the contract) used for a remote system instead of shipping pir's own image. Lets a cross-platform deploy work, which pir otherwise refuses rather than guessing a daemon it did not build |

## TUI & misc

| Variable | Meaning |
|---|---|
| `PIR_TRUST_PROJECT=1` / `PIR_NO_CONTEXT_FILES` | Pre-trust the project (skips the trust dialog) / skip AGENTS.md/CLAUDE.md discovery |
| `PIR_DISABLE_TERMINAL_QUERY` / `PIR_TERMINAL_COLOR_SCHEME_NOTIFY` | Disable OSC terminal queries (program status / bg color) / write mode-2031 light/dark notification sequences (unset records the state but writes nothing; any set value, not parsed, writes them) |
| `PIR_TUI_VT_INPUT=0` / `=false` | Windows only: opt OUT of `ENABLE_VIRTUAL_TERMINAL_INPUT`. The default is ON (upstream `enableWindowsVTInput`; the TUI owns the console's VT byte stream so bracketed-paste markers survive ConPTY). Unset, `1` and every other value keep the default; the install is best-effort and a console that refuses the mode falls back to crossterm's `INPUT_RECORD` parser |
| `PIR_TUI_TERMINAL_INPUT_BUDGET_MS` | Deferral backstop for raw `onTerminalInput` hooks, in ms (default 5000). Positive integer, values above 60000 capped; `0`, negative and non-numeric fall back to 5000. The inline 5 ms wait is clamped to this total |
| `PIR_TUI_INPUT_TRACE` | Emit one `tui-input-trace` line per second to `<agent dir>/pir.log` (drained events, paste coalescing, processed keys/pastes, editor/layout/render counters). Off by default; any non-empty value other than `0` enables it |
| `PIR_TUI_SEARCH_TRACE` / `PIR_TUI_SEARCH_TRACE_FILE` | Any set value appends one tab-separated latency line per measured segment of the alt-screen search/render pipeline to `PIR_TUI_SEARCH_TRACE_FILE` (default `pir-search-trace.log` in the process working directory). Off by default: the sink is resolved once, then every call site pays one `OnceLock` read and — when off — a branch. No behavior change when off |
| `PI_PROGRAM_STATUS=1` / `PI_HARDWARE_CURSOR` / `PI_CLEAR_ON_SHRINK` / `PI_TELEMETRY` / `PIR_VERBOSE` | Force-enable OSC 7501 program status / hardware cursor rendering / the `clearOnShrink` main-screen repaint / telemetry mode override / verbose logging (the three settings also have `/settings` rows; `showHardwareCursor`, `terminal.showTerminalProgress` and `terminal.clearOnShrink` apply live, PIR-TUI-017/018) |
| `PI_CLIPBOARD_OSC52=1` | After a local clipboard write, also send the text via OSC 52 (as over SSH). Use when a remote client reaches the TUI without SSH variables |
| `PI_HYPERLINKS` | OSC 8 hyperlink override: exactly `1`/`0` forces on/off; any other value (including `auto`) keeps the detected value. An explicit override also skips the tmux probe that otherwise decides hyperlinks under tmux |
| `PI_TRUE_COLOR` | Truecolor override: exactly `1`/`0` forces on/off; any other value (including `auto`) keeps the detected value |
| `PI_IMAGE_PROTOCOL` | Inline-image protocol override: `kitty`/`iterm2` force it, `none`/`0` disable it (case-insensitive); any other value keeps detection. Feeds the same capability resolution as `terminal.images`, which gates the settings menu's image rows and selects protocol vs half-block output for `/image` |
| `PI_TUI_ESC_TIMEOUT` | How long to wait after a lone ESC before treating it as Escape, in milliseconds: a positive number replaces the default (fractional values truncate); default `100` over SSH (`SSH_CONNECTION`/`SSH_TTY`), `10` otherwise. Increase if Alt-key input is misread as Escape |
| `VISUAL`, `EDITOR` | External editor (Ctrl+G) when the `externalEditor` setting is empty: `VISUAL` first, then `EDITOR`; blank values are skipped; default `notepad` on Windows / `nano` elsewhere |
| `PI_SHARE_VIEWER_URL` / `PI_RADIUS_GATEWAY` | `/share` gist viewer URL prefix (default `https://pi.dev/session/`) / Radius gateway override (default `https://radius.pi.dev`) |
| `PIR_NATIVE_SHELL_URL` / `PIR_NATIVE_SHELL_MODEL` | Endpoint the `pir-native-shell` headless desktop harness drives (`OpenAiCompletions`; default `http://localhost:11434/v1`) / model id sent to it (default `default`). Only the native-shell crate reads them; the normal TUI/CLI never does |

`RADIUS_API_KEY` and other provider keys are listed in `pir auth` and
[`providers.md`](providers.md).

## Loading & selection

| Variable | Meaning |
|---|---|
| `PIR_PREFER_NATIVE=0` | Opt OUT of native-first: keep an installed/selected JS package over its full native port (`subagent`, `mcp`, `codemode`, `tool-search`, `todo`, `context-mode`). On by default, because the point of a native port is that the unmodified plugin gets native throughput. The `tool-repair` and `phone` single-face rules apply regardless |
| `PIR_SYSTEM_PROMPT_FILE` | Read the system prompt from this file instead of the discovered `SYSTEM.md`. A failed read aborts a print run (see [`system-prompt-audit.md`](system-prompt-audit.md) B17) |
| `PIR_TOOL_MAX_SOURCE_BYTES` | Ceiling for a tool read that must materialize a whole file (`edit`, image decode, grep context); default 64 MiB. Malformed or `0` falls back to the default instead of disabling the guard |

## Builtins

| Variable | Meaning |
|---|---|
| `PIR_CONTEXT_MODE_MEMORY_MB` | `context-mode` QuickJS heap cap in MiB (default and maximum 512; `0` = uncapped) |
| `PIR_FOREMAN_PIR` | Override the `pir` binary a process-router subagent and a detached foreman run spawn (unset = the current executable) |
| `PIR_FOREMAN_STRICT_EXIT` | Detached-job form of "an unverified run is not success": a run that never wrote a journal or did not reach `VERIFIED` exits non-zero (see [`foreman.md`](foreman.md)) |
| `PI_PHONE_CUSTOM_EVENTS` | Comma-separated list of additional namespaced events the phone relay forwards to a device. Parsed by the engine-free `pir-builtin-phone` crate so the relay and the client cannot disagree about which names are accepted; empty/unset forwards none |
| `PI_PHONE_SESSION_POOL_EXECUTABLE` | Executable a phone session pool spawns its child sessions with (unset/blank = the running binary). Lets an owner pass its own image without touching the process-wide environment every other session in the process shares |
| `PI_PHONE_SERVER_URL` / `PI_PHONE_DEVICE_TOKEN` / `PI_PHONE_TOKEN` / `PI_PHONE_DEVICE_NAME` / `PI_PHONE_DEVICE_ID` | Defaults for `/phone-start` when the command line omits them: server URL, device token (`PI_PHONE_DEVICE_TOKEN` wins over `PI_PHONE_TOKEN`), device name and device id. An explicit positional argument, `--token`/`--server`/`--url` or the persisted `<agent dir>/phone-device.json` overrides them. Missing URL or token is an error, not a silent no-op |
| `PI_TOOL_REPAIR_DEBUG` | Any non-empty value turns on the `tool-repair` builtin's debug diagnostics (upstream `src/index.ts:25`). Empty or unset keeps them off |
| `PI_PHONE_CHILD` | Marker the phone session pool sets on every child it spawns, so the idle-message path can tell a pooled child from a parent session. Set by pir, not read as user configuration |
| `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT` | The env override the `pi-subagents` extension consults when it cannot find the host pi package from `process.argv[1]` or `import.meta.resolve`. pir exports its own materialized root (`<agent dir>/pir/host-pi-package`) so the detached runner can load the SDK entry; set it to point the extension at a different host package |

### Phone server (`pir-phone-server`)

| Variable | Meaning |
|---|---|
| `PIR_PHONE_SERVER_PEPPER` | Server secret used to hash device tokens. Must be at least 32 characters. Falls back to `<data-dir>/secret`, which `pir-phone-server init --data-dir …` writes; the server refuses to start when neither exists |
| `PIR_PHONE_SERVER_METRICS_TOKEN` | Bearer token for the metrics endpoint. Empty/unset disables authenticated metrics access |
| `PIR_PHONE_SERVER_MAX_RESIDENT_SESSIONS` | Cap on sessions resident in memory at once (default `100000`). A positive integer overrides; malformed keeps the default |

## Diagnostics (internal)

Developer-only switches: they change logging or test seams, never behavior a
user should depend on.

| Variable | Meaning |
|---|---|
| `PIR_MCP_DEBUG` | Print dispatch-deadline diagnostics for a blocked MCP dispatch |
| `PIR_PERF_TRACE` | Perf tracing around session build |
| `PIR_DEBUG_RPC` | `[rpc-debug]` stderr lines on run-claim |
| `PIR_PAGED_DEBUG` | `DBG paged threshold` lines for compaction window math |
| `PIR_ALLOC_TRACE` | TUI allocator tracing (heap-profile tests) |
| `PIR_EXT_SKIP_PRELUDE` / `PIR_EXT_SKIP_NODE_BUILTINS` | Skip the JS host prelude / node builtin bootstrap at extension boot (test seam; breaks extensions that need them) |
| `PIR_RESUME_ON_ERROR=1` | On resume, inject a synthetic continuation prompt when the leaf is a failed request (off by default) |
| `PIR_FAIL_FAST` | Fail fast on the first error in a test/batch context |
| `PIR_ALLOC_TRACE_FILE` | Target of the TUI allocator tracing sample lines (one JSON object per run). Default `pir-alloc-trace-<pid>.jsonl` in the system temp directory. Best effort: a missing or unwritable path must never disturb the TUI |
| `PI_TIMING` / `PI_STARTUP_BENCHMARK` | Startup phase timing. `PI_TIMING` (strict `1`) gates the instrumentation; `PI_STARTUP_BENCHMARK` (`1`/`true`/`yes`) additionally enables the benchmark and, off the interactive mode, errors with `PI_STARTUP_BENCHMARK only supports interactive mode`. pir instruments the shared startup path in `main_cli` and prints the same report shape to stderr |

## Test-runner escape hatches

The suites whose property IS the process environment (variable precedence, a
default-built session following the env, a child process inheriting it, a
`HOME`-anchored skill scan, the crash hook's live-dir read) pin that variable
instead of passing a directory. A process-wide pin is only race-free when one
test owns the process, so those tests call
`tests/common/mod.rs::skip_without_nextest` and print a `SKIP` line unless they
are running under `cargo nextest` (one process per test — the CI runner, see
[`test-harness-map.md`](test-harness-map.md)). Each variable family has its own
opt-in for a single-process run that knows what it is doing:

| Variable | Meaning |
|---|---|
| `NEXTEST_RUN_ID` | Set by nextest itself; its presence is what the guards check |
| `PIR_ALLOW_SHARED_PROCESS_AGENT_DIR=1` | Allow the `PIR_AGENT_DIR`-pinning tests in a shared process (run only that test, `--test-threads=1`) |
| `PIR_ALLOW_SHARED_PROCESS_HOME=1` | Same for the `HOME`-pinning tests |
| `PIR_ALLOW_SHARED_PROCESS_TEMP=1` | Same for the `TMP`/`TEMP`/`TMPDIR`-pinning test |
| `PIR_ALLOW_SHARED_PROCESS_PACKAGE_DIR=1` | Same for the `PI_PACKAGE_DIR`-pinning tests |

Setting any of them while other suites run in the same process reintroduces the
races documented as TEST-2 in `.audit/review-2026-11-23/findings.jsonl`.
