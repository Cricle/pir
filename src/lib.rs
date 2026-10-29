// Pi Rust (pir) — Single-crate implementation of the Pi coding agent
//
// Modules are organized by functional domain, following the upstream
// TypeScript package structure but adapted for Rust idioms.
//
// # Feature model (SDK)
//
// The crate is a Rust *library* first: the core (agent loop, providers,
// session management, config, prompt/skills) has no heavy dependencies and
// builds with `--no-default-features`. Everything above that is an opt-in
// cargo feature, so a consumer can embed the framework and leave the TUI,
// the embedded QuickJS/Python runtimes, MCP and the platform watchers out of
// the dependency graph entirely:
//
//   tools      — a meta-feature over the seven per-tool leaves `tool-read`,
//                `tool-bash`, `tool-powershell`, `tool-edit`, `tool-write`,
//                `tool-ls` and `tool-background` (the background-call
//                controls). `grep`/`find` stay under `grep-find` in this
//                tranche. Off (or with individual leaves off):
//                `register_core_tools` registers exactly the enabled leaves
//                and `CORE_TOOL_NAMES` lists exactly those names (empty with
//                none); the session's out-of-loop bash path, the shell pool,
//                the shell resolution and the `@file` image sniffer stay
//                core. Deliberately not implied by `cli`, so `--features cli`
//                is a real toolless binary and `--tools read` reports the
//                compiled-out `tool-read` diagnostic.
//   tui        — terminal UI, clipboard, theme watching
//   extensions — embedded QuickJS extension runtime (rquickjs + swc), including
//                the SQLite-backed shims (rusqlite rides this feature — there is
//                no separate `sqlite` feature)
//   python     — the `python` builtin tool (RustPython 3)
//   mcp        — MCP client + the `mcp` builtin (rmcp)
//   env        — the env daemon's watch/list backend (`notify` + the `ignore`
//                walker); `env-daemon` implies it. The daemon's wire data plane
//                (`frame`/`decode`) and the remote-env client stay core.
//   env-daemon — the built-in pi-env protocol-v1 daemon (`pir env-daemon`):
//                server, exec/pty/file/list/watch ops, platform `sys` layer.
//                Off: the command reports the missing feature; the wire data
//                plane and the remote-env client still compile.
//   server     — in-tree TCP/Unix server + client presets
//   session    — session *persistence*: the upstream-format JSONL reader/writer,
//                the tail-page memory-mapped reader (`memmap2`) and session
//                discovery. The session data plane (`types`, `projection`) and
//                the in-memory manager stay core, so an agent still runs with
//                no transcript persistence when this is off.
//   skills     — skill discovery/loading (`src/skills.rs`): filesystem walks,
//                frontmatter validation, the prompt XML. The `SkillInfo` type
//                and the pure frontmatter helpers stay core (the prompt
//                renderer needs them).
//   compaction — core today, *not* a cargo feature: the compaction engine
//                (cut-point selection, chunked summarization, paged prefix
//                streaming, branch summaries) compiles unconditionally. There
//                is no `compaction` feature to turn off; a future stub feature
//                would need the settings/result data types to stay core.
//   telemetry  — core today, *not* a cargo feature: the portable
//                span/metric/propagation module compiles unconditionally. The
//                no-op path is reached through `Telemetry::noop()`, not
//                through `cfg` (there is no `telemetry` feature).
//   sdk        — the curated embedding facade (`pir::sdk`). Off: the module is
//                not compiled at all (no stub); embedders enable `sdk`.
//   client     — the remote TCP protocol client (`pir::client`, `pir client`).
//                Off: the module is not compiled (no stub); the command reports
//                the missing feature at runtime.
//   full       — default; all of the above (what the `pir` binary needs)
//
// The full matrix (providers, the builtin plugins, `cli`, `tools`,
// `grep-find`, `image-tools`, `mcp-sd`, `client`) lives in `Cargo.toml`;
// `scripts/feature_matrix.sh` compiles every documented configuration.
//
// Design and acceptance criteria: SDK-SCOPE.md.

// Unsafe-code gate: no new `unsafe` may enter the crate. The only exceptions
// are platform/FFI boundaries, each of which opts in at its module with a
// documented `#[allow(unsafe_code)]` and a `// SAFETY:` note on every block:
// the TUI's native-key/console helpers (`tui::stdin_buffer`, `tui::theme::osc11`,
// `tui::terminal`'s SIGTSTP raise), the env daemon's `sys`/`pty`/`watch`
// backends, the confinement, shell-pool and spill Win32/libc layers, the
// QuickJS raw-buffer accessors (`extensions::runtime::host_functions::buffer_ops`),
// the extension kill calls, and the session tail map (`session` feature).
// Cargo's `[lints.rust] unsafe_code = "deny"` already enforces the same level
// for every target; this attribute keeps the rule visible in the crate root
// and independent of manifest configuration. Because a module- or
// function-level `#[allow(unsafe_code)]` switches the lint off for everything
// inside its scope, the block-level inventory of every `unsafe {`, `unsafe fn`
// and `unsafe impl` under `src/` is additionally pinned per file by
// `tests/unsafe_gate.rs` against `tests/unsafe_gate_allowlist.txt`: adding a
// block to an already-allowed file fails that gate, so an U1 allow cannot hide
// a new unsafe block.
#![deny(unsafe_code)]

pub mod agent;
pub mod agent_session;
pub mod ai;
pub mod cache_warmer;
// The remote protocol client (`pir client`). Feature-gated, not stubbed: when
// `client` is off the module does not exist and `src/main_cli/run_mode.rs`
// reports the missing feature for the command instead of failing to compile.
#[cfg(feature = "client")]
pub mod client;
pub mod coding_agent;
pub mod compaction;
pub mod config;
// Per-run timing breakdown (provider wait / stream / tools / tokens), fed from
// the existing agent-loop event stream by the hosts that already consume it.
pub mod diagnostics;
// Durable conversation/task/document runtime (the upstream
// `@earendil-works/pi-durable` entries + JSONL storage core): append-only
// commit log with atomic batches, cross-reopen id minting, record scans.
// Core, not optional: the session kernel and harness that build on it are
// what `agent_session` is being rebuilt onto (2026-10-20 migration decision).
pub mod durable;
// The built-in pi-env protocol-v1 daemon (`pir env-daemon serve --token`,
// upstream `packages/env/daemon`): the Rust end of the `@earendil-works/pi-env`
// wire that the `pi-env` JS shim and upstream clients deploy.
// The wire data plane (`frame`/`decode`, shared with the remote-env client)
// and `DAEMON_SUBCOMMAND` stay core; the daemon half (the server and its ops)
// compiles only with the `env-daemon` feature, which implies `env` (its
// `notify`/`ignore` watch/list backend).
pub mod env_daemon;
// The client half of the same wire (upstream `packages/env/src`): connection +
// handshake, `RemoteExecutionEnv`, `RemoteWatcher`, the SSH deployment surface,
// and the tool layer's local-vs-remote selection (`selection`).
// The remote-env *client* is dependency-free and is called from the
// core tool implementations; the daemon half of the wire is behind
// `env-daemon` (which implies `env` for its `notify`/`ignore` backend).
pub mod env_client;
// Extension *data* types ([`extensions::types`]) are always compiled; the JS
// engine inside is behind the `extensions` feature.
pub mod extensions;
// Diagnostic routing for host-facing extension/agent notices: raw `eprintln!`
// would corrupt the interactive TUI's frame, so they are routed through the
// host's logging destination (see the module docs).
pub mod logging;
#[cfg(feature = "mcp")]
pub mod mcp;
// `pir grep` / `pir sed` / `pir python`: the standalone native command surface
// (its clap subcommands live in `coding_agent::cli`, gated by `cli`).
#[cfg(feature = "cli")]
pub mod native_cli;
pub mod plugins;
pub mod protocol;
// The curated embedding facade (`pir::sdk`). Feature-gated, not stubbed: when
// `sdk` is off the module does not exist, so an embedder that uses it enables
// the feature (the consumer pins `tests/sdk_consumer_*.rs` compile with
// `--no-default-features --features sdk`).
#[cfg(feature = "sdk")]
pub mod sdk;
#[cfg(feature = "server")]
pub mod server;
pub mod session_manager;
// Embedded bash-compatible shell for the bash tool (`pir __brush`), behind
// the `brush` feature; without it the tool spawns the external shell.
#[cfg(feature = "brush")]
pub mod shell_embed;
pub mod skills;
pub mod system_prompt;

// Portable telemetry core. Core today, not a cargo feature: the module always
// compiles (there is no `telemetry` feature and no cfg-stubbed variant); the
// disabled path is `Telemetry::noop()`, so no consumer signature forks.
pub mod telemetry;

// Terminal-text helpers shared by the core and the TUI.
pub mod text;

#[cfg(feature = "tui")]
pub mod tui;

/// Convenience alias for application-level results.
pub type Result<T> = std::result::Result<T, anyhow::Error>;
