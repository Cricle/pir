# Changelog

## 0.3.1-rc.1 (2026-11-14)

Pre-release candidate carrying the post-0.3.1 batch: read-path performance work
(R1–R8 all landed; R5/R6/R7 in this batch), session-replacement semantics aligned
with upstream, and six real defects found by adversarial review. No breaking
changes.

### performance

- Run-end park validation is O(delta) instead of two transcript-sized walks: the
  prompt-start projection leaves a park token (leaf + message count + window
  flag) that the run-end park re-validates incrementally, with a full-validation
  fallback (R3, PIR-BIN-101 family).
- `context_usage()` walks the shared branch snapshot (1 622 → 21 allocations per
  call at N=800, R5) and the RPC `get_entries` poll no longer deep-clones the
  whole session (R6).
- The session-context snapshot's branch text is incremental: 16.7 ms / 5 283
  allocations → 1.06 ms / 35 per push at N=800, byte-identical (R7).
- The session and JSON event lines are pre-sized before serialization: 200 KB
  tool-result turn ~2.3 MB fewer allocations in the full host config, bytes
  unchanged (PIR-BIN-099, both faces); the session face alone −12.6 % (pse
  3 042 → 2 658 KB).
- R1/R2/R4 read-path wins from the earlier post-0.3.1 pass are included.

### correctness

- Windowed sessions (paged / live-trimmed) no longer graft abandoned-branch
  rows into the model context: the broken-chain bridge is whole-file only and
  both projection walkers agree on the same window (PIR-BIN-101, with the two
  adversarial-review residuals PIR-BIN-102/103).
- The R7 snapshot cache can no longer splice stale text across a same-path
  reload or across manager instances (process-global storage generation, cache
  drop on swap), and an empty cached aggregate falls back to a full render
  (PIR-BIN-105/106/108).
- A session replacement reports upstream's per-operation reason on every event
  — `new` for a fresh session, `resume` for any opened file, including one
  parent-linked to the outgoing session — and emits neither `session_tree` nor
  a drain-time `queue_update`; the fork flavor is the clone/fork-at-current
  caller's explicit seam, never a file-shape inference (PIR-BIN-049,
  PIR-BIN-110).
- The `--mode json` print host claims its session-control actions; ephemeral
  runs keep `ctx.newSession()` in memory and a storage-less fork rejects with
  upstream's message inside the call (PIR-BIN-052 residual, PIR-BIN-107).
- Extension `setStatus`/`setWidget` rows appear at interactive startup like pi,
  and a startup-queued `sendUserMessage` starts its turn at startup
  (PIR-TUI-042/043).

### tui

- Kitty image crop/re-emit for a scrolled placement is ported with byte parity
  (upstream `cropKittyImageLine`, `terminal-image.ts:471-483`), closing drift
  rows 15/16.

### tests and infrastructure

- The harness map was re-verified row-for-row, stale target counts corrected,
  and the generator's `-Check` now itemizes drift per artifact (PIR-QE-002).
- Timing flakes were root-caused with stronger contracts instead of widened
  timeouts (PIR-QE-001).
- Randomized adversarial wire corpora (776 events) pin byte identity across
  both event faces.
- `clipboard-win` 5.4.1 → 5.4.3 (PIR-BIN-025).

## 0.3.1 (2026-11-13)

A/B parity pass against upstream `pi` 1.4.0 (TUI, CLI, sessions, providers,
streaming, SDK) plus a Windows portability pass. No breaking changes;
`docs/FEATURES.md` §5b records the resource bounds this release makes visible.

### performance

- Compaction decides on the same request-projected context floor pi uses: the
  declared tool schema and prompt units are counted once instead of twice. A
  26-turn script now compacts 8 times instead of 24 (pi: 8); at 1000 turns
  both sides compact 332 times at the same trigger point.
- The TUI no longer self-drives a repaint loop while scrolled up: a dirty
  frame whose windowed band did not move schedules no next timer (measured
  97–99% CPU sustained → idle).

### tui

- Terminal scrollback is no longer cleared while the TUI is in use. Repaints
  for a shrunk or windowed viewport emit `ESC[2J` + home but never the
  destructive `ESC[3J`; a frame that starts above the window renders the full
  transcript; the bounded transcript window (512 entries / 4 MiB) announces
  truncation instead of silently dropping rows (see `docs/tui.md`,
  "Intentional deviations from upstream").
- The startup header's key hints collapse to the single compact row again and
  still expand on demand; an extension `setHeader` widget still replaces the
  built-in header.
- `/hotkeys` reports the live bindings (for example `ctrl+p / alt+p` for the
  model cycle and `ctrl+q` for follow-up) instead of stale literals, and the
  footer renders the thinking level actually resolved for the session model.
- Provider failures no longer show the internal `non-retryable provider
  error:` marker in front of `HTTP 500:`/`HTTP 401:` text; retry behavior is
  unchanged.

### cli and sessions

- `pir print` with an explicitly selected print surface treats a bare
  positional argument as the prompt: `pir print --mode json "version"` issues
  a model request instead of running the `version` subcommand.
- A resumed session sends exactly one system prompt; a stored system row is no
  longer concatenated with the live rendering (request 2219 → 1731 chars).
- `abort` answers only after the run settles, so a prompt sent immediately
  afterwards is queued in order instead of being rejected during the
  14–230 ms settling window.
- `fork {entryId}` swaps in a new session file whose header carries
  `parentSession` = the source file and whose rows are the root→target path,
  instead of moving the leaf inside the source JSONL. The fork now survives a
  restart, `get_state.sessionFile`/`sessionId` change, and the source file is
  never appended to again; `/fork`, the RPC `fork` command and
  `AgentSessionRuntime::fork` all go through the same seam.
- A malformed `settings.json` degrades to defaults with exit 0 and one
  warning (previously every non-interactive command exited 1); per-subsystem
  detail for the same problem is now log-only (`--verbose` /
  `RUST_LOG=debug`) instead of repeating the line.
- Provider `maxRetries`, the retry delay cap and `timeoutMs` are wired:
  `maxRetries: 0` sends 1 request instead of 3, `maxRetries: 3` sends 4, and a
  1 s `timeoutMs` times out at ~1.1 s.
- A close-delimited SSE stream that ends without a finish reason is reported
  as `Stream ended without finish_reason` instead of a silent success.
- `--mode json` exits 1 when no provider can serve the request (pi parity).
- `--thinking` is applied without `--model` too, and clamped to the model's
  supported rungs before the first request (`max` → `high` on models without
  a `max` rung).
- `--no-session` history trimming (1024 messages / 100k tokens) is announced:
  one `history_trimmed` event carrying `droppedMessages`, `retainedMessages`
  and `totalDroppedMessages`, one matching stderr line, and no event for
  persisted sessions.
- `@file` mentions keep pi's rules: a missing file is an error, and skipped
  context files are reported instead of silently ignored.
- A context file that is skipped (over the 1 MiB cap, or not valid UTF-8) is
  now reported as a warning instead of vanishing silently.
- File tools accept workspace-internal `..` segments (`src/../src/main.rs`);
  paths that actually escape the workspace root remain refused.
- The request's `tools[]` array follows the active-set order instead of being
  re-sorted by name, so it matches the order the `<tools>` prompt section
  lists.
- `bash` composes `PATH` in the dialect of the consumer (POSIX for MSYS
  shells, native form for Windows executables), so `node` and other native
  programs are found from MSYS bash again.
- `--continue`, `--session <id>` and `--resume` match the session cwd
  case-insensitively on Windows, so a differently-cased drive letter or
  directory spelling no longer starts an empty history.

### windows

- `write`/`edit` preserve the spelling stored in the directory entry, so
  editing `MixedCase.txt` through `mixedcase.txt` no longer renames the file
  on disk.
- Tool errors use stable English text plus the OS code instead of localized
  system strings (`Cannot write 'x': permission denied (Access is denied; os
  error 5)`).
- A project file whose name collides only by case (`AGENTS.md` /
  `AGENTS.MD`) is diagnosed once, not twice.

## 0.3.0 (2026-11-11)

### performance

- Added the opt-in `performance.profile = "efficient"` (or
  `PIR_PERF_PROFILE=efficient`) with a core-four default tool loadout, bounded
  tool-result text, soft/hard turn budgets, a cumulative token budget, and
  prompt-cache support. The default `balanced` path keeps the historical
  behavior unchanged.
- Removed a per-turn completed-turn clone on the session hook path, made
  multi-tool batches participate in the degenerate-loop guard, and added a
  high-water resident-session cap to `pir-phone-server` (offline sessions
  hydrate from redb; online/subscribed sessions are never evicted).

### builtin:memory

- New native `memory` builtin embedding `cm-memory` 0.3.0: one `memory` tool
  over the full `cm` CLI action surface (recall/brief, remember/append/absorb,
  frames, governance, audit, explore/wiki, checkpoint/reindex/verify/import/
  export), the `/memory` command, adaptive `input` auto-recall
  (`auto|always|off`, score floor), optional stable `before_agent_start` brief,
  and checkpoint/shutdown flush. No external `cm` process is spawned.
- Per-session store defaults to `<session-file-dir>/cm`, with
  `workspace`/`agentDir`/`path` modes; settings live under the `memory` key.
- Hardened writes: model `remember`/`append` cannot self-assign `verified`
  trust or create `frame`/`feedback`/reserved-scope records; embedded imports
  enforce the source policy, cap trust at `imported`, regenerate ids and strip
  supersede/merge metadata. Source anchors/snippets/audit reads are confined to
  the canonical project root.
- Added memory_builtin integration tests plus a 3-agent cross-review pass and
  a post-fix A/B report (`docs/memory-builtin-ab-2026-11-09.md`).

### pi-phone central relay

- New standalone `pir-phone-server` workspace member: a multi-user
  information proxy that stores users/devices/session metadata and relays
  phone commands/events to agent processes. It does **not** execute agents or
  tools; `pir` stays the local agent runtime.
- User personal access tokens and per-device tokens (`SHA-256(token+pepper)`,
  revocable), with user/device/session catalog API, paginated sessions, and
  per-user isolation.
- Agent protocol: outbound WSS `/agent` hello / `frame` / `command` / `resync`
  with request-id rewriting and targeted response routing; phone protocol:
  `/ws` with subprotocol auth, multi-client fan-out, and session selection.
- The native phone plugin now supports `/phone-start <ws(s)://server>
  [device-token] [--device-name]` as the remote-only agent client.
- New PWA login/setup flow stores the central server URL and token per server;
  chat/session frames are relayed through the central server.
- PWA device management page: list devices, create a device token (shown
  once), copy/revoke it; catalog pagination supports "Load more sessions".
- PWA v1 layout: restrained American-minimal surfaces, persistent session
  sidebar on desktop, drawer on mobile, sticky composer, no bottom navigation
  or header chip clutter. Session rows are single-line; settings, devices and
  session controls are center-column pages rather than modal sheets; login is
  a page. Light, dark and follow-system appearance plus a persisted
  Show/Hide Thinking preference are available in Settings.
- The phone `SessionPool` is wired end to end: parallel children register as
  `active-session-N` catalog rows, appear in the single-line session list, and
  prompt/model/thinking/abort/tree commands route to the selected child.
- Device revocation now closes the live agent WebSocket immediately; the new
  `device_revoke_closes_the_agent_socket` regression test covers it.
- Agent hello/status negotiate capabilities (`session-pool`,
  `extension-ui.v2`, `custom-events.v1`). Plugins can opt into namespaced
  `phone.custom.*` events via `PI_PHONE_CUSTOM_EVENTS`; frames are size-capped
  and re-emitted to the PWA as `pi-phone-custom` DOM events.
- `extension_ui_request` now covers status/title/widget/working-message/
  working-visibility/editor/theme/footer/header/working-indicator/
  hidden-thinking-label projections in addition to notify and dialogs.
- The `subagent` tool now renders a dedicated child-agent panel in both the
  TUI and PWA: agent/task/status plus the child transcript, tool calls, usage
  and errors stay inside the subagent block instead of being mistaken for the
  main assistant transcript.
- Server hot paths use shared `Arc<str>` broadcast lines, status-diff
  throttling, reverse B-tree catalog reads, `O(1)` health counts, compact
  bincode metadata rows and zero-copy static responses. `/api/metrics` exposes
  registry counters and RSS/VmHWM.
- Added `examples/load_test.rs` / `scripts/phone_server_load.sh`: 10k catalog,
  100-client fan-out, agent-resync reconnect, 1M-session RSS/peak RSS, cache
  and CPU measurements.
- HTTP proxy endpoints for session history and model lists through the agent's
  WebSocket (`/api/sessions/:id/messages`, `/api/models`); quota remains
  agent-side.
- Updated-at session index plus batch session upsert; 10k-session catalog
  page load is covered by `tests/load_catalog.rs`.
- Added repeatable server benchmarks (`examples/bench.rs`,
  `scripts/phone_server_bench.sh`) and stress/scenario tests: 50-client
  fan-out, 400 KiB and multi-MiB snapshots, slow-consumer isolation, client
  reconnect with agent resync, agent disconnect/reconnect.
- The relay stores only account/device/session metadata and the latest small
  status line; transcripts, snapshots, tool output and images are forwarded
  and dropped. Compact bincode rows keep a 1M-session catalog at ~617 MiB
  peak RSS. Agent connection ids prevent a stale socket from unregistering its
  replacement.
- Removed the legacy embedded HTTP/WS phone server, embedded browser assets,
  Tailscale Serve management, and runtime-state files. The native phone plugin
  is now remote-only; `phone` feature dependencies shrink accordingly.
- `--allowed-origin` now configures both WebSocket Origin checks and CORS for
  external PWA deployments; `/api/quota` proxies the agent-side quota RPC.
- Documentation: `docs/phone-server.md` and
  `scripts/phone_server_check.sh`.

### fixes

- Fixed the release-mode `python` crash: the tracing global allocator now
  returns a dangling pointer for zero-size layouts in `alloc`/`alloc_zeroed`
  and stops forwarding the matching `dealloc`/`realloc`, instead of tripping
  the non-zero-size `assert_unchecked` in core's blanket `GlobalAlloc` impl.
  `pir python -c "print(42)"` is now 10/10 EXIT 0 in release (previously
  EXIT 3221226505), with `PIR_ALLOC_TRACE` on or off.
- `LoopOptions::prepare_next_turn_needs_completed` now defaults to `true`,
  matching its own doc contract: `AgentTurnContext` always carries the
  completed `message`/`tool_results`, so a `prepare_next_turn` hook that reads
  them no longer gets an empty snapshot unless a caller opts out.
- The TUI search box now consumes burst input: a console burst of 8 or more
  characters, which the terminal coalescer rewrites into one paste event while
  a search is active, is inserted into the query character by character
  instead of being swallowed. Fast typing or pasting into the find bar no
  longer drops the input.
- Test contracts reconciled with the shipped behavior: inline `#[cfg(test)]`
  modules under `src/` are down to the original 8 grandfathered files (20
  files' modules moved to `tests/suites/*_units.rs`, 37 tests preserved
  verbatim); `tool_search` activation is asserted to be visible to the very
  next call of the same run, and paged `transcript()` is asserted to widen
  (2,288 total / 1,400 unique / 888 duplicate-by-id).

<!-- Add user-visible changes for the next release here. The release workflow
     requires the `v*` tag to equal the `[workspace.package].version` in
     Cargo.toml (v0.3.0 -> 0.3.0), and the same value is recorded in
     .migration-info. -->

## 0.0.2 (2026-11-08)

### 2026-11-02

**Memory**

- Session listing scans oversized single-line files through a 1 MiB line
  window (`memchr`, size-adaptive reader): peak RSS for 100 sessions with a
  256 MiB outlier drops from 799.4 MB to 2.12 MB (−99.7%), and median listing
  time from 636.77 ms to 407.60 ms (−36%).
- Printing a 100 MB session no longer clones the full projection or builds a
  request-sized JSON tree: paged prompt context is built without widening,
  OpenAI request bodies of 8 MiB or more stream from a temporary file, and
  messages replay lazily (16 B/element). Widen peak RSS falls from 973.6 MB to
  206.8–215.7 MB (−78%); compaction of a fully resident 100 MB session from
  563.2 MB to 426.3 MB (−24.3%).
- TUI startup peak for a 2 MB session falls from 81.9 MB to 72.1–72.6 MB
  (−11–12%) by deferring shell-pool prewarm and deduplicating the settings
  snapshot.
- A minimal JS extension adds about 25 MB (TUI peak 72.0–72.5 → 97.3–97.5 MB),
  but only 2.31 MB is live heap; the rest is QuickJS code pages and thread
  stack. Without JS plugins no QuickJS runtime is created, and the prelude
  inflate buffer is now exactly sized.
- Message slots shrink from 592 B to 408 B: `AssistantMessage.deferred` and
  `ToolResultMessage.nested_calls` are boxed optionals (400 B each), saving
  ~14 MB on a 100 MB transcript without changing the JSON wire shape.

**Performance**

- Eight SDK criterion benchmarks (`benches/sdk_benchmark.rs`) cover build,
  prompt roundtrip/streaming, transcript, attach and event paths.
- SDK fast paths: non-streaming prompts skip per-delta event projection, the
  session-event bridge subscribes on the first streamed prompt, and metadata
  reads use leaf-path accessors instead of building projections.
- Widen wall/CPU time is down 34%.

**Extensions and Hooks**

- All 49 native events are wired end to end, validated through one
  `is_known_event` that accepts pir's synthetic session-event names.
- Native fan-out honors `cancel`/`block`/`handled`; `before_provider_request`
  results chain into the request; `session_before_*` `Generic` results apply
  field overlays and respect `cancel: true`. A mutation-only handler that
  returns `None` no longer gets an earlier handler's replacement re-applied
  over its in-place payload edit.
- `/reload` runs exactly one lifecycle: `session_shutdown{reload}` →
  `on_reload` → JS reload → `session_start{reload}`.
- `terminal_input`, `on_init`/`on_reload`/`on_shutdown` and synchronous
  `on_context`/`on_session_context`/`status` callbacks are panic-isolated;
  `on_init` runs before the first terminal chunk and on read-only entry
  points.

**SDK and examples**

- Ten runnable SDK examples plus `scripts/run_examples.sh`: offline examples
  need no network or credentials, and each declares only its required
  features.
- Every example runs on a 64 MiB thread, so `cargo run --example …` works on
  Windows/GNU's 2 MiB main stack.
- `--no-default-features --features sdk` builds are clean again; the SDK
  quickstart is now a complete external-consumer program.

**CI**

- CI is split into lint (fmt, clippy, SDK feature matrix, doctests, examples),
  a Windows full test suite, and a Linux lib+bins subset; benchmarks run only
  manually via `scripts/run_benchmarks.sh`.
- Lint suppressions fall from 144 to 36 outer `#[allow]` in `src/`
  (`too_many_arguments` 70 → 0 across `src/` and `tests/`) via struct-based
  parameter refactors; new suppressions are not accepted.

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
