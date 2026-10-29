# Changelog

## Unreleased

<!-- Add user-visible changes for the next release here. The release workflow
     requires the `v*` tag to equal Cargo.toml's `[package].version` (v0.0.1 ->
     0.0.1), and the same value is recorded in .migration-info. -->

### 2026-10-29

**Core**

- Print/RPC/server and the SDK `AgentBuilder` default to a 1,000-turn
  per-prompt cap (`PIR_MAX_TURNS` overrides the CLI hosts; raw
  `CreateAgentSessionOptions` stays at 100); the TUI keeps its 100-turn cap.
- Durable JSONL storage flushes each append before `commit` returns, so an
  immediate read or reopen always sees the complete commit marker even with
  `fsync` off.
- Extension shim blobs are embedded from canonical LF source bytes, so the
  byte-exact shim gate is identical on Windows and Linux checkouts.

**Performance**

- Provider catalog parses once and is not cloned/serialized for builtin-only
  startup (debug print/TUI first frame roughly halved); large-session
  projection/compaction peaks cut by 22–34%.

### 2026-10-27

**Packaging**

- The crates.io package is `pir-sdk`: the library is imported as `pir`, the
  executable is `pir`; `cargo install pir-sdk` installs the `pir` binary.

**Core**

- Server session listing snapshots metadata under the registry lock and scans
  the directory off-lock; persisted-session attach opens/parses/prepares in one
  blocking task, with the session cap checked at insert.
- A service-update receiver that falls behind the broadcast ring (`Lagged`)
  gets a full snapshot for every subscribed live session instead of a gap.
- Client pending-request ids are retired on response, error, timeout or caller
  drop; transport teardown clears buffered updates/frames and the pending table.
- Compaction widen/projection and server attach run on the blocking pool.
- Server security caps: 128 connections, 64 MiB connection body, 256 sessions,
  64 subscriptions; `PIR_SERVER_MAX_SESSIONS`/`PIR_SERVER_MAX_SUBSCRIPTIONS`
  raise the limits. Delta path dictionary capped at 4096 ids; provider event
  streams stay bounded (32 events, producer backpressure).

**Runtime**

- `env_client`: 60 s daemon start + `hello` deadline, 30 s per frame write,
  5 s ping / 30 s silence teardown.
- `pir rpc`: dedicated threads for the stdout writer and each prompt run;
  16 MiB per input line; at most 16 concurrent commands.
- TUI: idle poll sleeps 750 ms (16 ms while work is in flight);
  `PI_TUI_WRITE_LOG` stops at 64 MiB with one truncation marker.
- Env daemon admits 32 concurrent long requests, overridable 1..=256 with
  `PIR_ENV_DAEMON_MAX_LONG_REQUESTS`.

**Extensions and MCP**

- A runtime generation releases child processes, worker threads, fetches, HTTP
  servers, bodies, net sockets, `fs.watch` threads and fds on `/reload`/shutdown.
- MCP image blocks and image-MIME resource blobs become real image content;
  binary/over-cap text goes to owner-restricted spill files. Spill names carry
  the owning pid; sweep reclaims only this process's files or a dead pid's;
  stdio servers escalate to SIGKILL after the grace period.
- `terminal_input`: 5 ms inline wait, deferred decision (later input queues
  behind it), late answer applies to the original chunk, 5 s fail-open backstop
  (`PIR_TUI_TERMINAL_INPUT_BUDGET_MS`, capped 60 s).
- `provider:refreshModels` runs at most once per provider definition per
  10 minutes; first sight or a changed definition refreshes immediately.
- Phone bridge caches bounded (4000 messages, 64 live tool entries per
  session); image store capped at 64 MiB with a 2 h TTL, pruned on read.

**CI**

- Two workflows (`ci.yml`, `release.yml`); release version derived from the
  `v*` tag; the CI gate is one Linux+Windows matrix job running fmt, clippy,
  doctests and eight sequential nextest groups per OS (group 1 also runs the
  lib/bin unit harnesses).

## Older releases

- **0.0.1 (2026-10-28)** — first release under pir's own semantic version;
  upstream mapping in `.migration-info` (pir 0.0.1 = upstream pi 1.4.0).
- **1.4.0 (2026-10-26)** — version parity with upstream pi 1.4.0
  (`38ff99177f`); package version reaches every metadata surface. Fixed
  truncated-assistant/tool-result persistence pairing (upstream #11227),
  `/compact <instructions>` survival through paged compaction, per-leaf
  settings recovery, and a 1 MiB `RpcClient` stdout-line bound.
- **Unreleased (2026-10-19)** — added `pir grep|sed|python` with transparent
  system-tool fallback, the `brush` embedded-shell feature, and the
  `mcp.json` `builtinTools` flag.
- **1.1.0 (2026-10-10)** — full Rust equivalent of pi v1.1.0-era upstream
  (`ce950d78f..f1b2e77f5`): 42-provider catalog, 9 OAuth flows, all protocol
  adapters, session-format interop, QuickJS extensions, builtins, shell pool,
  TUI parity, typed SDK. Deviations are archived in MAPPING.md.
