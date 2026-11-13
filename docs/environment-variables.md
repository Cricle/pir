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
| `PIR_CODEMODE_TIMEOUT_MS` / `PIR_CODEMODE_MAX_OUTPUT_CHARS` / `PIR_CODEMODE_MAX_OUTPUT_ITEMS` | Codemode sandbox limits |
| `PIR_CODEMODE_MEMORY_MB` | Codemode sandbox heap cap override (`0` = no cap; default 128) |
| `PIR_TOOL_MAX_SOURCE_BYTES` | Whole-file source ceiling for `edit`, image decode and grep context reads (default 67108864 = 64 MiB; `read` streams and is not capped). A positive byte count overrides; `0`/malformed falls back rather than disabling the guard |
| `PIR_READ_BEFORE_EDIT` | Gate for `edit`/`write` on an existing file: `off` (no check), `warn` (proceed with a warning), `edit-warn` (edit may proceed with a warning for partial/stale reads; write stays strict), or `enforce` (refuse, default). Case-insensitive; `0`/`false`/`disabled` = `off`, `warning` = `warn`; unset/unrecognized = `enforce` in balanced, `edit-warn` in efficient. Requires a full read that is unchanged (mtime/size); new-file writes bypass the check; successful writes/edits refresh the ledger; resolved once at session build |
| `PIR_NATIVE_FALLBACK=0` | Disable the `pir grep\|sed\|python` fallback to the system tool (`0`/`false`; unset or any other value keeps it enabled) |
| `PIR_CONTEXT_MODE_ROUTING` / `PIR_CONTEXT_MODE_INDEX_RESULTS` | `context-mode` `tool_call` routing / `tool_result` auto-indexing. Both default on; disabled by `0`, `false`, `off` or `no` (unset or any other value keeps it on) |
| `PI_EXT_MEMORY_MB` / `PI_EXT_STACK_MB` / `PI_EXT_GC_THRESHOLD_MB` / `PI_EXT_CALL_TIMEOUT_MS` / `PI_EXT_HOOK_TIMEOUT_MS` / `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS` | Extension runtime QuickJS limits and lifecycle budgets (override `extensionLimits`; see [extensions.md](extensions.md)) |
| `PIR_JS_MEMORY_MB` | Heap cap of a `pir js` run in MiB. `0`/unset = no cap. A malformed value (including negative) is reported on stderr and ignored. `pir js` reads neither `PI_EXT_MEMORY_MB` nor `extensionLimits` |

## Subagents

| Variable | Meaning |
|---|---|
| `PIR_SUBAGENT_MAX_TURNS` | Per-child turn cap (one turn = one model request; default 1000). A positive integer overrides; `0` removes the cap; malformed/negative values keep the default. Resolved once when the subagent tool is built and applied to every single/chain/parallel child |
| `PIR_SUBAGENT_WALL_CLOCK_MS` | Per-child wall-clock budget in milliseconds (default 1800000 = 30 minutes). A positive integer overrides; `0` disables the budget; malformed/negative values keep the default. Resolved once when the subagent tool is built and applied to every single/chain/parallel child |

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
| `PI_SHARE_VIEWER_URL` / `PI_RADIUS_GATEWAY` | `/share` gist viewer URL prefix (default `https://pi.dev/session/`) / Radius gateway override (default `https://radius.pi.dev`) |

`RADIUS_API_KEY` and other provider keys are listed in `pir auth`.
