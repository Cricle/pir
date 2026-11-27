# FEATURES.md — feature contract

Contract, not a measurement log. Current baselines: [`RESULTS.md`](../RESULTS.md).

> **Crate ownership (2026-11-21):** the engine features in this document are
> declared in `crates/pir-core/Cargo.toml`; `tui`/`image-protocol` are
> front-end features owned by `crates/pir-tui` and `pir-sdk`. The facade
> forwards every user-facing name unchanged (`session`, `extensions`, …), so
> the matrix and all documented commands behave as described. See
> [`architecture.md`](architecture.md).
>
> **Builtin features forward to a published crate (2026-11-25):** each builtin
> feature (`subagent`, `mode`, `mcp`, `codemode` under `extensions`,
> `context-mode`, `tool-search`, `tool-repair`, `todo`, `memory`, `phone`) also
> enables the matching optional `pir-builtin-*` dependency, which carries that
> builtin's pure logic. Turning a feature off drops BOTH the crate and the
> in-engine runtime, so the "feature off" behavior below is unchanged.
>
> `mode` is an alias of `subagent`: the foreman/plan session modes are only
> meaningful on top of the `subagent` tool they delegate through, so enabling
> `mode` enables `subagent`. The alias exists so every builtin has a feature of
> its own name instead of being reachable only as a side effect.

## 1. Contract

1. The agent loop + direct plumbing are the always-compiled core, plus the `pir::minimal` profile so
   `--no-default-features` is runnable; every other surface is a cargo feature. `default = ["full"]`.
2. A feature-off build must be usable: the command is absent or returns the exact documented off-error (``this
   build was built without the `X` feature`` for commands, ``this build was compiled without the `X` feature`` for
   the `--tools` diagnostics). No panic, no silent no-op. The `pir grep|sed` native fallback is the one
   deliberate silent swap (`--no-fallback`/`PIR_NATIVE_FALLBACK=0` disables it).
3. A `#[cfg]` must never change the feature-ON path.
4. `Cargo.toml` is single-writer: declare the feature/optional deps before gating code.
5. `scripts/feature_matrix.sh` asserts **125 cargo invocations**: 89 root-package rows (61 lib + 18 bin + 2 SDK consumer + 8 smoke) plus 36 support-crate rows (18 extracted crates × no-default + all-features). Every selectable
feature except `default`/`full` has its own `cargo check --lib --no-default-features --features <f>` row.


## 2. Feature graph

**Core (always compiled):** `agent`, `agent_session`, `ai` (plain `/chat/completions` adapter;
Responses/Codex/`pi-messages` are `provider-*`-gated), `minimal` (zero-feature profile), `config`, `compaction`,
`diagnostics`, `env_client`, `logging`, `plugins` framework, `session_manager`,
`system_prompt`, `coding_agent::json` and tool plumbing. `durable`, `telemetry`, `protocol`, the AI value types
(`ai::types`/`ai::util`/`ai::event_stream`/`ai::error`/the transport seam), the terminal-text helpers (`text`) and
the on-disk session record types (`session_manager::types`) are separate crates (`pir-durable`, `pir-telemetry`,
`pir-protocol`, `pir-ai-types`) re-exported at their historical paths; `durable` is the one that is also a cargo
feature. Every core-tool leaf is off (the
seven `tool-*` leaves and `grep-find`), so `register_core_tools` is a no-op and `CORE_TOOL_NAMES` is empty.
The core's HTTP stack (`reqwest` + native-tls behind `ai::transport`/`ai::auth::http`) is deliberately
unconditional: the plain OpenAI-compatible adapter is core (the `minimal` profile needs one runnable wire
provider) and every OAuth flow drives the shared auth HTTP client, so every feature set — including
`default-features = false` SDK embeds — inherits it (PIR-SDK-029; docs/sdk.md §Feature flags documents it).

| Feature | Gates |
|---|---|
| `default` | `full` (shipped default); its closure reaches every selectable feature, so default and `--all-features` cover all |
| `provider-openai` | OpenAI Responses/Codex/`pi-messages` adapters + catalog auto-construction |
| `minimal` | Profile marker; enables nothing (`pir::minimal`/`pir-minimal` are core); part of `full` (no-op) |
| `provider-anthropic` | Anthropic messages |
| `provider-google` | Google/Vertex + service-account JWT |
| `provider-bedrock` | Bedrock + SigV4 |
| `provider-mistral` | Mistral conversations |
| `cli` | clap argv; **bin minimum**; implies `print`/`rpc`/`export-html`; does not imply `tools` |
| `print` | `pir print` host (module absent when off; `cli` always implies it) |
| `rpc` | JSON-lines RPC host (module absent when off; `cli` always implies it) |
| `export-html` | HTML session export; `cli` and `tui` imply it; an `rpc`-without-export-html build returns the off-error |
| `tool-read` | `read` (shared confinement/path helpers compile with it) |
| `tool-bash` | the `bash` tool (`BashTool`); the bash module/shell pool stays core |
| `tool-powershell` | `powershell` (reuses the core bash spawn/capture machinery) |
| `tool-edit` | `edit`; the TUI edit preview also compiles the module under `tui` |
| `tool-write` | `write` |
| `tool-ls` | `ls` |
| `tool-background` | `bg_status` + `bg_abort` |
| `tools` | meta-feature over the seven `tool-*` leaves above |
| `grep-find` | in-process ripgrep `grep`/`find`; an **independent leaf** — does NOT imply `tools` or any `tool-*`, so a search-only embed compiles grep/find without read/bash/edit/write/ls/background; `tool-grep`/`tool-find` deferred |
| `image-tools` | inline images in `read` (the `image` crate; no separate `image` tool) |
| `image-resize` | engine-side image codec/resize helpers without enabling the full `image-tools` tool surface |
| `tui` | terminal UI: loop, editor, renderers, clipboard, themes, mermaid |
| `pir-native-shell/desktop` | workspace-only experimental Native Shell/NSH + GPUI desktop front end; build with `cargo build -p pir-native-shell --features desktop`, not through `pir-sdk` |
| `pir-native-shell/mobile` | workspace-only target-gated iOS UIKit / Android JNI adapter boundary; not a published `pir-sdk` facade feature |
| `image-protocol` | half-block preview renderer in the TUI (`/image`); default-on in `full`, independently disableable; off: `terminal.showImages: true` renders the `[Image: …]` text fallback |
| `extensions` | QuickJS, JS/TS + SWC, `node:*`, `codemode`, hooks/widgets |
| `context-mode` | `ctx_*` Code Mode (JavaScript/TypeScript on the embedded QuickJS suite) + FTS5/BM25 (implies `extensions`) |
| `mcp` / `mcp-sd` | MCP client + builtin (implies `extensions`) / `pir mcp-sd` stdio server (implies `mcp`) |
| `env` / `env-daemon` | env-daemon watch/list backend (`notify` + `ignore`; dependency-only alone, implied by `env-daemon`) / built-in pi-env protocol-v1 daemon (`pir env-daemon`; implies `env`, `frame`/`decode` stay core) |
| `server` | in-tree `pir server` |
| `client` | remote client (`pir::client`, `pir client`) |
| `session` | JSONL persistence, tail-page reader (pulls the optional `memmap2`), discovery |
| `skills` | skill discovery/loading |
| `jobs` | `pir jobs` (start/list/status/wait/log/reap/kill/clean) + the detached `__jobs-supervise` supervisor; implied by `cli` and `subagent`. The shared start-token probe the session lock needs is NOT gated (it lives in `pir_core::process_probe`) |
| `checkpoint-cli` | the `pir checkpoint` argv surface only (list/restore/run); implied by `cli`. The checkpoint engine that `edit`/`write` snapshot through stays always-compiled, so undo behavior never forks |
| `sdk` | `pir::sdk` facade |
| `subagent` | native `subagent` builtin + `/subagents` activity overlay (`ctrl+shift+a`); the companion `mode` builtin adds user-triggered `/mode foreman` / `/mode plan` / `/mode normal`, durable foreman journal, `/approvals` (worker + plan approvals), plan submission/approval with execution handoff, optional git-worktree isolation, process-per-task router, and `pir foreman run/status/logs/abort/approve/deny/resume/clean` detached runs |
| `tool-repair` | tool-repair pipeline |
| `tool-search` | BM25 `tool_search` |
| `todo` | `todo` builtin + `/todos` (interactive overlay in UI sessions) |
| `memory` | embedded cm-memory engine: `memory` tool, `/memory`, input auto-recall |
| `phone` | phone UI; compiled, default **off** |
| `brush` | embedded bash behind `pir __brush` |
| `full` | all of the above (the default build) |

The central pi-phone relay is a separate workspace package, **not** a feature
of `pir-sdk`: build it with `cargo build -p pir-phone-server`. The `phone`
feature compiles only the remote agent client (`/phone-start wss://...`); the
legacy embedded HTTP server is gone. See
[`docs/phone-server.md`](phone-server.md).

**Not features:** `compaction` is core (public types span 21 files; no-op stubs rejected); `durable` is the
default-on `durable` feature that pulls the separate `pir-durable` crate (re-exported at `crate::durable`; a slim
embed drops it with `--no-default-features`); `telemetry` is likewise the separate `pir-telemetry` crate re-exported
unconditionally (`Telemetry::noop()` is the off path, not a feature); `bug_report` is gated by `tui`; `print`/`rpc`/`export-html` and the `env-daemon` split are features.
Remaining deferred split: `tool-grep`/`tool-find` (both stay under `grep-find`); `grep-find` owns only
`grep-searcher`/`grep-regex`/`ignore` and implies no `tools`/`tool-*`. No `sqlite` feature:
`node:sqlite`/better-sqlite3 ride `extensions`; `context-mode` pulls bundled SQLite via `rusqlite` through its
`extensions` dependency. SDK scope: [`docs/sdk.md`](sdk.md).

## 3. Default-on policy

Default-on: `subagent`, `mode`, `todo`. Every other compiled builtin — `mcp`,
`codemode`, `context-mode`, `tool-search`, `tool-repair`, `memory`, `phone` —
is **off** by default and loads only when something names it: `-e
builtin:<name>`, `settings.extensions` (`+builtin:<name>` re-enables one a lower
scope disabled), or a `--tools` / `defaultTools` entry naming a tool that
builtin registers.

The reason is measured, not stylistic. The previous default-on set declared 25
tools at **39,530 bytes** of `tools[]` schema per request against upstream's 4
tools / 2,527 bytes, and 48% of that payload was the twelve `ctx_*` tools.
Across six measured runs of a read-and-edit bug hunt, `ctx_*` was called
**zero** times — the model used `read`/`bash`/`grep` instead. Paying ~15x the
schema on every request to expose tools the model declines is what this default
avoids. `phone` was already off for its own reason (it opens an outbound
connection to the central relay and needs a server URL/device token).

A `--tools codemode` selection brings `builtin:codemode` onto the plan
automatically (`tool_selection::builtin_paths_for_selected_tools`), so naming a
default-off tool still works without an extra `-e`. `--only-builtin` loads
exactly the Rust builtins (no settings/package scan, QuickJS, SWC or `node:*`);
`--no-extensions` wins if both are given.

### 3.1 Default tool loadout

**`balanced` (the default) declares a small superset of upstream pi's
`DEFAULT_TOOL_NAMES`.** With no `--tools` / `settings.defaultTools`, a full
build declares **12 tools**: the registration-active core nine plus three
default-active builtin-extension tools.

Core nine: `bg_abort`, `bg_status`, `bash`, `edit`, `find`,
`grep`, `ls`, `read`, `write` (`powershell` stays registered-but-inactive,
exactly like upstream). Builtin three: `todo`, `subagent`, `submit_plan` —
the default-on set (§3). `--no-extensions` leaves the core nine; `codemode`,
`tool_search`, `powershell`, MCP bridges and the twelve `ctx_*` tools stay
registered-but-not-declared unless selected or their builtin is enabled.
Upstream pi declares four (`read`, `bash`, `edit`, `write`), and this build
lists those same four FIRST (`DEFAULT_TOOL_NAMES` order), then the rest
alphabetically.

The extra core five are pir's own capabilities — the in-process ripgrep
`grep`/`find` pair, `ls`, and the scheduler-owned `bg_status`/`bg_abort`
controls (see `docs/tui.md` and `MAPPING.md` for the background/detach
surface). The 24-tool default that preceded this change measured ~33 KB of
`tools[]` JSON per request against upstream's 4-tool 2,527 B; this 12-tool set
is smaller by the whole default-off builtin layer (~19 KB of `ctx_*` schema
alone), and the exact byte count is not re-recorded here — measure it with a
request capture rather than trusting a stale figure.

> The `python` builtin was removed after that measurement (see §5). The
> `~33 KB` figure is kept as the recorded historical baseline, not re-measured.

Two ways to reach the upstream four, both first-class:

- `--tools read,bash,edit,write` (or `settings.defaultTools`) — an explicit
  selection, identical to upstream's set;
- `--perf-profile efficient` / `PIR_PERF_PROFILE=efficient` — the opt-in
  profile whose IMPLICIT selection is the upstream core four
  ([`docs/performance-profiles.md`](performance-profiles.md)).

Both leave every other tool reachable: the registry is never filtered by the
selection (F12), so an excluded-but-registered tool stays callable through
`--tools`, `tool_search`, MCP codemode and nested calls.

## 4. Slim profiles

The binary profiles (`no-js`, `tui-slim`) require `cli`; `sdk-headless` is library-only; `minimal` is the
**zero-feature** executable profile. Sizes and opt-level trade-offs: [`RESULTS.md`](../RESULTS.md) and
[`docs/sdk.md`](sdk.md) §Packaging and size (the older `logs/w18-help/q55/SIZE-TUNE-REPORT.md` is no longer in
the tree).

| Profile | Feature set |
|---|---|
| `full` | `default = ["full"]` |
| `minimal` | `--no-default-features` (the `minimal` marker adds nothing). Builds `pir-minimal`, not the full `pir` CLI: one OpenAI-compatible provider, one `read_file` tool, in-memory session |
| `no-js` | `cli,tui,env,env-daemon,server,client,subagent,tool-repair,tool-search,todo,memory,context-mode,phone,provider-*,tools,grep-find,image-tools,session,skills` |
| `tui-slim` | `cli,tui,tools,provider-openai,provider-anthropic,session,skills` (no `env-daemon`) |
| `sdk-headless` | bare + needed `provider-*`/`sdk` |

`tui-slim` omits `env-daemon` (its `pir env-daemon` returns the off-error; `no-js` keeps it). Minimal is
`/chat/completions` + `read_file` only (confined to cwd), no persistence/compaction/skills/MCP/extensions;
`memmap2` only for `session`.

### 4.1 SDK optionality / minimality matrix (measured, v7)

Authority for embedding size/runtime cost per SDK feature. Method and full
data: external consumer `.play/lanes/lane-sdk-dev/apps/quickstart-copy/README.md`
(evidence `.play/evidence/lane-sdk-dev/round28-sdk-matrix/` +
`round29-engine-driven/`, 2026-11-11). One consumer binary
(`default-features = false, features = ["sdk"]`, one feature per row), release
GNU, fat tables pinned per combo.

| Feature over `sdk` | binary Δ | packages added (cargo tree) | run cost (heap live / WorkingSet) |
|---|---:|---|---|
| (sdk-only baseline) | 38,072,320 B / 339 pkgs | reqwest+hyper+native-tls (documented core stack, PIR-SDK-029) | R28 100 scripted prompts: 6.25 / 16.8 MiB; R29 engine-probe baseline: 2.18 / 15.5 MiB |
| `session` | +0.22 MiB | memmap2 (+1) | resident tail + projection |
| `tools` | +1.82 MiB | +0 | +178 KiB heap at first prompt (182,423 B over baseline) |
| `todo` / `subagent` / `tool-repair` / `tool-search` / `skills` | +0.07–0.70 MiB | +0 each | sub-MiB first-prompt heap each |
| `memory` | +4.57 MiB | cm-memory (+12) | R28 100 prompts: 16.35 / 30.0 MiB (+10.10 live over baseline; store resident) |
| `extensions` | +21.32 MiB | rquickjs/swc/crypto/rusqlite (+333) | — |
| `mcp` | +39.85 MiB | rmcp + extensions tree (+426) | R29 100 engine drives: 5.54 / 25.0 MiB (+3.36 live / +9.49 WS) |
| `context-mode` | +0.0 MiB over `extensions` | rusqlite (already linked by `extensions`) | reuses the QuickJS suite; no separate interpreter |
| `image-tools` | +0.00 MiB | image (+31, already linked) | — |
| `session+tools` | +2.32 MiB | memmap2 | R28 100 prompts: 11.08 / 26.4 MiB (+4.82 live over baseline) |

MiB = 1024² bytes (the R28/R29 REPORT tables quote the same samples in
decimal MB, ~5% larger: memory +10.59 MB, mcp +3.52 MB, context-mode
+10.06 MB live deltas). `context-mode` now runs JavaScript on the same
`extensions` QuickJS suite and links no separate interpreter.

Zero tui/ratatui/crossterm/phone packages in every combo (the optionality
leak check; `rquickjs` appears only under the documented
`extensions`/`mcp`/`context-mode` carriers). Optionality contract, probed for
real: a `.builtin(<name>)` this build did not compile fails `build()` with the
feature-naming diagnostic; sdk-only registers 0 builtins/tools/commands;
enabled features really register (todo → tool+command; memory → tool+command +
`<memory-context>` auto-recall transform). Undriven interpreter/VM combos sit
near heap baseline — engine crates charge at link time and first use, not
import. Idle WS 6,208–8,356 KiB (6.1–8.2 MiB), warm start 30–43 ms across all
15 combos.

## 5. Off-error policy

| Feature off | Behavior |
|---|---|
| `tui` | `` `interactive mode` is unavailable: this build was built without the `tui` feature ``; `--mode rpc` works; `--list-models` substring filter |
| `extensions` | `pir js`/`__codemode-worker` missing-feature; on-disk `-e` dropped with a `pir:` diagnostic; `builtin:codemode` not compiled; handles report no JS runtime |
| `context-mode` | `builtin:context-mode` not compiled; no `ctx_*` |
| `mcp` / `mcp-sd` | `mcp check/list/login/logout`, `mcp-sd` missing-feature; `builtin:mcp` not compiled; `mcp add/remove` still write config / `pir mcp-sd` reports missing `mcp-sd` (the `cli,mcp` build compiles; tested) |
| `env` / `env-daemon` | dependency-only backend (no daemon): `pir env-daemon` needs `env-daemon` / `` `pir env-daemon` is unavailable: this build was built without the `env-daemon` feature `` (exit 1); `frame`/`decode` and the remote-env client still compile |
| `server` / `client` | `pir server` / `pir client` missing-feature; `pir::client` absent |
| `session` | open/persist/discovery return `SESSION_PERSISTENCE_DISABLED`; `--session/--fork` report it; `--session-id` warns in-memory |
| `skills` | discovery empty + one diagnostic; prompt renders |
| `sdk` | `pir::sdk` absent (no stub); enable `sdk` |
| `subagent`/`tool-repair`/`tool-search`/`todo`/`memory` | `builtin:<name>` reports “not compiled … feature off” |
| `python` | **removed entirely** (no feature, no builtin, no `pir python` subcommand). `builtin:python` reports unknown-builtin; `pir python …` is not a known subcommand. The old name only ever ran JavaScript on QuickJS, so nothing executable was lost |
| `phone` | builtin absent; default-off even in `full` |
| `provider-*` | model listed; streaming returns the adapter-missing error. `openai-completions` is the exception: the plain chat adapter is core and always usable |
| `cli` | no `pir` CLI binary (the `[[bin]]` target requires it); the library keeps the shared helpers, and the separate `pir-minimal` binary still builds with zero features |
| `print` / `rpc` | host module absent when off; `cli` always implies both |
| `export-html` | an `rpc`-only build returns `` `export_html` is unavailable … without the `export-html` feature `` before the session is attached |
| `tool-read` | `read` absent; explicit `--tools read` → `pir: tool 'read' is unavailable: … 'tool-read' feature`; shared confinement/path helpers still compile |
| `tool-bash` | `BashTool` absent; bash module, shell resolution/pool and out-of-loop `execute_bash` stay core; explicit `--tools bash` names `tool-bash` |
| `tool-powershell` | `powershell` absent; explicit `--tools powershell` names `tool-powershell` |
| `tool-edit` | edit tool absent; with `tui` on the preview engine still compiles; explicit `--tools edit` names `tool-edit` |
| `tool-write` | `write` absent; explicit `--tools write` names `tool-write` |
| `tool-ls` | `ls` absent; explicit `--tools ls` names `tool-ls` |
| `tool-background` | `bg_status`/`bg_abort` absent; explicit names report `tool-background` |
| `tools` | all seven leaves and `grep-find` absent ⇒ `CORE_TOOL_NAMES` empty; explicit core names report their leaf feature (`grep`/`find` → `grep-find`); unknown names silent |
| `grep-find` | `grep`/`find` absent; system-grep fallback unless `--no-fallback`; explicit `--tools grep` names `grep-find` |
| `image-tools` / `image-protocol` | `read` reports the image type/size but does not attach it / no halfblocks renderer; `/image` and `terminal.showImages: true` render the `[Image: …]` text fallback |
| `brush` | `pir __brush` missing-feature; bash uses external shell |

Known policy gaps: no-TUI hosts accept/ignore `--tui-mode`; `pir session` still prints a dir/count with `session`
off; `mcp add/remove` writes config in a no-`mcp` build — documented, not claimed correct.

## 5b. Resource bounds

Things pir caps that upstream leaves unbounded. Every cap announces itself —
a user must never lose visible information silently (PROTOCOL v8 §5).

| bound | value | who it applies to | how it is announced |
|---|---|---|---|
| ephemeral history | 1024 messages / 100 000 estimated tokens | sessions with NO storage (`--no-session`, an SDK agent built without a session store) | `history_trimmed` event (exactly once per trim, naming the dropped/retained counts and which cap forced it), one `pir: warning: …` line, and `historyDroppedMessages` / `historyTrimmed` in `get_state` + `get_session_stats`. Persisted sessions are never bounded by this window (their history is the store + compaction) |
| transcript window (TUI) | 512 entries / 4 MiB (regular mode), bounded band (fullscreen) | the interactive transcript of a huge session | the `... (N earlier entries, … to load)` lead row, plus `LoadOlder` paging; full repaints never emit `3J` so the terminal scrollback is not erased (see `docs/tui.md`) |
| non-interactive turns | `PIR_MAX_TURNS`, default 1000 | `print` / `rpc` / `server` runs | a synthesized `StopReason::Error` terminal naming the cap and how to raise it |
| compaction trigger | `contextWindow − compaction.reserveTokens` | every host that compacts | the compaction lifecycle events |

## 6. Verification

```bash
bash scripts/feature_matrix.sh            # 125 invocations (89 root + 36 support-crate)
cargo check --lib --no-default-features --features tool-read   # each feature alone
cargo check --lib --no-default-features --features grep-find   # independent search leaf
cargo check --bin pir-minimal --no-default-features            # zero-feature agent
cargo test --no-default-features --test minimal_profile        # end-to-end mock provider
cargo check --bin pir --no-default-features --features cli
cargo check --bin pir --no-default-features --features cli,tool-read
cargo check --bin pir --no-default-features --features cli,grep-find
cargo check --bin pir --no-default-features --features cli,provider-openai
cargo check --bin pir --no-default-features --features cli,tui,provider-openai
cargo test --no-default-features --features cli --test feature_off_surface
```

Smoke rows build real `pir` and pin `--version`, off-errors, compiled-out `--tools` diagnostics in print and RPC,
the `export-html` off-arm and a mock-provider print. `cli,tool-read` pins the partial build: `read` available,
every other leaf reports its own missing-feature diagnostic.

## 7. Boundaries

- `mcp→extensions`, `mcp-sd→mcp`, `context-mode→extensions`, `image-protocol→tui` are deliberate. The seven
  `tool-*` leaves are dependency-free; `tools` is exactly their union.
