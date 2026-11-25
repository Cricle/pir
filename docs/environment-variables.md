# Environment variables

## Agent dir & sessions

| Variable | Meaning |
|---|---|
| `PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR` | Agent directory override, first non-empty wins (default `~/.pi/agent`) |
| `PI_CODING_AGENT_SESSION_DIR` / `PIR_SESSION_DIR` | Session storage location, first non-empty wins (`--session-dir` and settings `sessionDir` participate) |
| `PI_SESSION_FILE` / `PI_SESSION_ID` | Exported by the host into `bash` child environments and the shared shell pool; not read as configuration |
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
| `PIR_JS_MEMORY_MB` | Heap cap of a `pir js` run in MiB. `0`/unset = no cap. A malformed value (including negative) is reported on stderr and ignored. `pir js` reads neither `PI_EXT_MEMORY_MB` nor `extensionLimits` |

## Subagents

| Variable | Meaning |
|---|---|
| `PIR_SUBAGENT_MAX_TURNS` | Per-child turn cap (one turn = one model request; default from `performance.subagentMaxTurns`: 1000 balanced, 100 efficient). A positive integer overrides the settings value; `0` removes the cap; malformed/negative values keep the resolved setting. Resolved at each `subagent` call and applied to every single/chain/parallel child |
| `PIR_SUBAGENT_MAX_TOTAL_TOKENS` | Per-child cumulative token budget (`input + output + cache_read + cache_write`). Default from `performance.subagentMaxTotalTokens`: unlimited on balanced, 100,000 on efficient. A positive integer overrides; `0` disables the budget; malformed/negative values keep the resolved setting |
| `PIR_SUBAGENT_WALL_CLOCK_MS` | Per-child wall-clock budget in milliseconds (default 1800000 = 30 minutes). A positive integer overrides; `0` disables the budget; malformed/negative values keep the default. Resolved at each `subagent` call and applied to every single/chain/parallel child |

## Server / remote

| Variable | Meaning |
|---|---|
| `PIR_SERVER_ADDRESS`, `PIR_SERVER_AUTH_TOKEN` | Remote server bind / bearer token |
| `PIR_SERVER_ALLOWED_ROOTS` | Workspace roots the server may touch |
| `PIR_SERVER_MAX_CONNECTIONS`, `PIR_SERVER_MAX_CONNECTION_BODY_BYTES`, `PIR_SERVER_MAX_REQUEST_BODY_BYTES` | Server limits |
| `PIR_SERVER_MAX_SESSIONS` / `PIR_SERVER_MAX_SUBSCRIPTIONS` | Live session cap (default 256) / `pir.session` subscriptions per connection (default 64). Accepted `1..=1000000`; malformed/out-of-range keeps the default |
| `PIR_SERVER_IDLE_TIMEOUT_SECS` | Per-read idle timeout after handshake, in seconds, reset on activity (default 300). Accepted `1..=86400`; malformed/out-of-range keeps the default |
| `PIR_ALLOW_REMOTE_AGENT` / `PIR_ALLOW_REMOTE_BASH` / `PIR_ALLOW_PARALLEL_SIDE_EFFECTS` | Remote capability gates |

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

## TUI & misc

| Variable | Meaning |
|---|---|
| `PIR_TRUST_PROJECT=1` / `PIR_NO_CONTEXT_FILES` | Pre-trust the project (skips the trust dialog) / skip AGENTS.md/CLAUDE.md discovery |
| `PIR_DISABLE_TERMINAL_QUERY` / `PIR_TERMINAL_COLOR_SCHEME_NOTIFY` | Disable OSC terminal queries (program status / bg color) / write mode-2031 light/dark notification sequences (unset records the state but writes nothing; any set value, not parsed, writes them) |
| `PIR_TUI_VT_INPUT=0` / `=false` | Windows only: opt OUT of `ENABLE_VIRTUAL_TERMINAL_INPUT`. The default is ON (upstream `enableWindowsVTInput`; the TUI owns the console's VT byte stream so bracketed-paste markers survive ConPTY). Unset, `1` and every other value keep the default; the install is best-effort and a console that refuses the mode falls back to crossterm's `INPUT_RECORD` parser |
| `PIR_TUI_TERMINAL_INPUT_BUDGET_MS` | Deferral backstop for raw `onTerminalInput` hooks, in ms (default 5000). Positive integer, values above 60000 capped; `0`, negative and non-numeric fall back to 5000. The inline 5 ms wait is clamped to this total |
| `PIR_TUI_INPUT_TRACE` | Emit one `tui-input-trace` line per second to `<agent dir>/pir.log` (drained events, paste coalescing, processed keys/pastes, editor/layout/render counters). Off by default; any non-empty value other than `0` enables it |
| `PI_PROGRAM_STATUS=1` / `PI_HARDWARE_CURSOR` / `PI_CLEAR_ON_SHRINK` / `PI_TELEMETRY` / `PIR_VERBOSE` | Force-enable OSC 7501 program status / hardware cursor rendering / the `clearOnShrink` main-screen repaint / telemetry mode override / verbose logging (the three settings also have `/settings` rows; `showHardwareCursor`, `terminal.showTerminalProgress` and `terminal.clearOnShrink` apply live, PIR-TUI-017/018) |
| `PI_CLIPBOARD_OSC52=1` | After a local clipboard write, also send the text via OSC 52 (as over SSH). Use when a remote client reaches the TUI without SSH variables |
| `PI_HYPERLINKS` | OSC 8 hyperlink override: exactly `1`/`0` forces on/off; any other value (including `auto`) keeps the detected value. An explicit override also skips the tmux probe that otherwise decides hyperlinks under tmux |
| `PI_TRUE_COLOR` | Truecolor override: exactly `1`/`0` forces on/off; any other value (including `auto`) keeps the detected value |
| `PI_IMAGE_PROTOCOL` | Inline-image protocol override: `kitty`/`iterm2` force it, `none`/`0` disable it (case-insensitive); any other value keeps detection. Feeds the same capability resolution as `terminal.images`, which gates the settings menu's image rows and selects protocol vs half-block output for `/image` |
| `PI_TUI_ESC_TIMEOUT` | How long to wait after a lone ESC before treating it as Escape, in milliseconds: a positive number replaces the default (fractional values truncate); default `100` over SSH (`SSH_CONNECTION`/`SSH_TTY`), `10` otherwise. Increase if Alt-key input is misread as Escape |
| `VISUAL`, `EDITOR` | External editor (Ctrl+G) when the `externalEditor` setting is empty: `VISUAL` first, then `EDITOR`; blank values are skipped; default `notepad` on Windows / `nano` elsewhere |
| `PI_SHARE_VIEWER_URL` / `PI_RADIUS_GATEWAY` | `/share` gist viewer URL prefix (default `https://pi.dev/session/`) / Radius gateway override (default `https://radius.pi.dev`) |

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
