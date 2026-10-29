# Environment variables

## Agent dir & sessions

| Variable | Meaning |
|---|---|
| `PIR_AGENT_DIR` / `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR` | Agent directory override, first non-empty wins (default `~/.pi/agent`) |
| `PI_CODING_AGENT_SESSION_DIR` / `PIR_SESSION_DIR` | Session storage location, first non-empty wins (`--session-dir` and settings `sessionDir` participate) |
| `PI_SESSION_FILE` / `PI_SESSION_ID` | Exported by the host into `bash` child environments and the shared shell pool; not read as configuration |
| `PIR_SESSION_FILE` | Best-effort session-file hint the crash logger records (hosts set it when a session is open; unset or empty = no hint) |

## Models & providers

| Variable | Meaning |
|---|---|
| `PI_PROVIDER` / `PI_MODEL` / `PI_REASONING_LEVEL` | Default provider/model/thinking overrides |
| `PI_CACHE_RETENTION=long` | pi-messages default cache retention |
| `PI_HTTP_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` | Request proxy (after the settings `httpProxy`) |
| `PI_OAUTH_CALLBACK_HOST` | OAuth loopback callback host |
| `PI_CATALOG_BASE_URL` | Override the remote catalog endpoint |
| `PI_OFFLINE` | Disable every network call (catalog overlays, update check) |
| `PI_SKIP_VERSION_CHECK` | Disable the startup update check |
| `ANTHROPIC_BASE_URL` | Claude Code gateway URL for the anthropic provider (models.json `baseUrl` wins) |
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_OAUTH_TOKEN` | Bearer credential for anthropic requests (sent as `Authorization: Bearer`) |
| `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL` | Claude Code model ids; synthesized into the anthropic provider when not defined in models.json |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` / `CLAUDE_CODE_MAX_OUTPUT_TOKENS` | Context window / max output for the env-synthesized model (defaults 200000 / 32000) |

Provider API keys follow the upstream names (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `RADIUS_API_KEY`, ...); see
`pir auth` for the stored-credential view.

## Tools & execution

| Variable | Meaning |
|---|---|
| `PIR_TOOL_EXECUTION` | Tool execution mode override |
| `PIR_MAX_PARALLEL_TOOLS` | Parallel tool batch cap |
| `PIR_FAIL_FAST` | Fail-fast tool batches |
| `PIR_MAX_TURNS` | Per-prompt turn cap for print/RPC/server (default 1000; one turn = one model request, not one tool call). Only a positive integer is accepted; `0`/junk keeps the default. The SDK `AgentBuilder` uses its own fixed 1000 default and the raw `CreateAgentSessionOptions` defaults to 100; neither reads this variable. The TUI ignores it and keeps its 100-turn cap |
| `PIR_CODEMODE_WORKER=0` | Disable the codemode worker process (thread fallback) |
| `PIR_CODEMODE_TIMEOUT_MS` / `PIR_CODEMODE_MAX_OUTPUT_CHARS` / `PIR_CODEMODE_MAX_OUTPUT_ITEMS` | Codemode sandbox limits |
| `PIR_CODEMODE_MEMORY_MB` | Codemode sandbox heap cap override (`0` = no cap; default 128) |
| `PIR_TOOL_MAX_SOURCE_BYTES` | Whole-file source ceiling for `edit`, image decode and grep context reads (default 67108864 = 64 MiB; the `read` tool streams and is not capped). A positive byte count overrides it; `0` or a malformed value falls back to the default rather than disabling the guard |
| `PIR_NATIVE_FALLBACK=0` | Disable the `pir grep\|sed\|python` fallback to the system tool (`0`/`false`; unset or any other value keeps the fallback enabled) |
| `PIR_CONTEXT_MODE_ROUTING` / `PIR_CONTEXT_MODE_INDEX_RESULTS` | `context-mode` `tool_call` routing / `tool_result` auto-indexing. Both default on; each is disabled by `0`, `false`, `off` or `no` (unset or any other value keeps it on) |
| `PI_EXT_MEMORY_MB` / `PI_EXT_STACK_MB` / `PI_EXT_GC_THRESHOLD_MB` / `PI_EXT_CALL_TIMEOUT_MS` / `PI_EXT_HOOK_TIMEOUT_MS` / `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS` | Extension runtime QuickJS limits and lifecycle budgets (override `extensionLimits`; see [extensions.md](extensions.md)) |
| `PIR_JS_MEMORY_MB` | Heap cap of a `pir js` script run in MiB. `0` or unset = no cap. A malformed value (including a negative one) is reported on stderr and ignored. `pir js` deliberately reads neither `PI_EXT_MEMORY_MB` nor `extensionLimits` |

## Server / remote

| Variable | Meaning |
|---|---|
| `PIR_SERVER_ADDRESS`, `PIR_SERVER_AUTH_TOKEN` | Remote server bind / bearer token |
| `PIR_SERVER_ALLOWED_ROOTS` | Workspace roots the server may touch |
| `PIR_SERVER_MAX_CONNECTIONS`, `PIR_SERVER_MAX_CONNECTION_BODY_BYTES`, `PIR_SERVER_MAX_REQUEST_BODY_BYTES` | Server limits |
| `PIR_SERVER_MAX_SESSIONS` / `PIR_SERVER_MAX_SUBSCRIPTIONS` | Live session cap (default 256) / `pir.session` subscriptions per connection (default 64). Accepted range `1..=1000000`; a malformed or out-of-range value keeps the default |
| `PIR_SERVER_IDLE_TIMEOUT_SECS` | Per-read idle timeout after the handshake, in seconds and reset on activity (default 300). Accepted range `1..=86400`; a malformed or out-of-range value keeps the default |
| `PIR_ALLOW_REMOTE_AGENT` / `PIR_ALLOW_REMOTE_BASH` / `PIR_ALLOW_PARALLEL_SIDE_EFFECTS` | Remote capability gates |

## Runtime & threading

| Variable | Meaning |
|---|---|
| `PIR_TOKIO_WORKERS` | Worker threads of the main tokio runtime. Unset keeps tokio's default (one per core); a positive integer is clamped to `1..=64`; `0` or a malformed value keeps the tokio default |
| `PIR_TOKIO_MAX_BLOCKING` | Blocking-pool ceiling of the main runtime. Unset = two per core clamped to `4..=32`; a positive integer is clamped to `1..=512`; `0` or a malformed value keeps the adaptive value |
| `PIR_ENV_DAEMON_WORKERS` | `pir env-daemon` file-operation worker threads. Unset = detected parallelism clamped to `2..=8`; a positive integer is clamped to `1..=64`; `0` or a malformed value keeps the adaptive value |
| `PIR_ENV_DAEMON_MAX_LONG_REQUESTS` | Concurrent `exec`/`watch`/`pty` requests admitted by one daemon (default 32). A positive integer is clamped to `1..=256`; `0` or a malformed value keeps 32. Requests past the cap get a structured `EAGAIN` |
| `PIR_EXECUTABLE` | Override the pir image used as the local `env-daemon` executable, for the remote daemon upload, and for child-session re-entry (unset or empty = the running executable) |

## TUI & misc

| Variable | Meaning |
|---|---|
| `PIR_TRUST_PROJECT=1` | Pre-trust the project (skips the trust dialog) |
| `PIR_NO_CONTEXT_FILES` | Skip AGENTS.md/CLAUDE.md discovery |
| `PIR_DISABLE_TERMINAL_QUERY` | Disable OSC terminal queries (program status / bg color) |
| `PIR_TERMINAL_COLOR_SCHEME_NOTIFY` | Write the mode-2031 light/dark notification sequences to the terminal. Unset records the state but writes nothing; any set value (not parsed) writes them |
| `PIR_TUI_VT_INPUT=1` | Windows only: opt into `ENABLE_VIRTUAL_TERMINAL_INPUT` for the console; unset or any other value leaves the console mode untouched |
| `PIR_TUI_TERMINAL_INPUT_BUDGET_MS` | Deferral backstop for raw `onTerminalInput` hooks, in milliseconds (default 5000). A positive integer is used, values above 60000 are capped at 60000; `0`, negative and non-numeric values fall back to 5000. The inline 5 ms wait is clamped to this total |
| `PI_PROGRAM_STATUS=1` | Force-enable OSC 7501 program status |
| `PI_CLIPBOARD_OSC52=1` | After a local clipboard write, also send the copied text to the terminal with OSC 52 — as is done over SSH. Use it when a remote client reaches the TUI without SSH variables, such as a multiplexer attached from another machine or a remote IDE daemon |
| `PI_HARDWARE_CURSOR` | Hardware cursor rendering |
| `PI_CLEAR_ON_SHRINK` | Clear the alt-screen on shrink |
| `PI_TELEMETRY` | Telemetry mode override |
| `PI_SHARE_VIEWER_URL` | `/share` gist viewer URL prefix (default `https://pi.dev/session/`) |
| `RADIUS_GATEWAY` | Radius gateway override (default `https://radius.pi.dev`) |
| `PIR_VERBOSE` | Verbose logging |

`RADIUS_API_KEY` and the other provider keys are listed in `pir auth`.
