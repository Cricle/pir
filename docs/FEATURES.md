# FEATURES.md — feature contract (2026-10-27)

Contract, not a measurement log. Current baselines: [`RESULTS.md`](../RESULTS.md).

## 1. Contract

1. The agent loop + direct plumbing are the always-compiled core, plus the `pir::minimal` profile so
   `--no-default-features` is runnable; every other surface is a cargo feature. `default = ["full"]`.
2. A feature-off build must be usable: the command is absent or returns the exact documented off-error (``this
   build was built without the `X` feature`` for commands, ``this build was compiled without the `X` feature`` for
   the `--tools` diagnostics). No panic, no silent no-op. The `pir grep|sed|python` native fallback is the one
   deliberate silent swap (`--no-fallback`/`PIR_NATIVE_FALLBACK=0` disables it).
3. A `#[cfg]` must never change the feature-ON path.
4. `Cargo.toml` is single-writer: declare the feature/optional deps before gating code.
5. `scripts/feature_matrix.sh` asserts **85 cargo invocations** (57 lib + 18 bin + 2 SDK consumer + 8 smoke). Every selectable
   feature except `default`/`full` has its own `cargo check --lib --no-default-features --features <f>` row.

## 2. Feature graph

**Core (always compiled):** `agent`, `agent_session`, `ai` (plain `/chat/completions` adapter;
Responses/Codex/`pi-messages` are `provider-*`-gated), `minimal` (zero-feature profile), `config`, `compaction`,
`durable`, `diagnostics`, `env_client`, `logging`, `plugins` framework, `protocol`, `session_manager`,
`system_prompt`, `telemetry`, `text`, `coding_agent::json` and tool plumbing. Every core-tool leaf is off (the
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
| `tool-background` | `background_status` + `background_abort` |
| `tools` | meta-feature over the seven `tool-*` leaves above |
| `grep-find` | in-process ripgrep `grep`/`find`; an **independent leaf** — does NOT imply `tools` or any `tool-*`, so a search-only embed compiles grep/find without read/bash/edit/write/ls/background; `tool-grep`/`tool-find` deferred |
| `image-tools` | inline images in `read` (the `image` crate; no separate `image` tool) |
| `tui` | terminal UI: loop, editor, renderers, clipboard, themes, mermaid |
| `image-protocol` | half-block preview renderer in the TUI (`/image`); default-on in `full`, independently disableable; off: `terminal.showImages: true` renders the `[Image: …]` text fallback |
| `extensions` | QuickJS, JS/TS + SWC, `node:*`, `codemode`, hooks/widgets |
| `python` | embedded RustPython `python` |
| `context-mode` | `ctx_*` + FTS5/BM25 (implies `python`) |
| `mcp` / `mcp-sd` | MCP client + builtin (implies `extensions`) / `pir mcp-sd` stdio server (implies `mcp`) |
| `env` / `env-daemon` | env-daemon watch/list backend (`notify` + `ignore`; dependency-only alone, implied by `env-daemon`) / built-in pi-env protocol-v1 daemon (`pir env-daemon`; implies `env`, `frame`/`decode` stay core) |
| `server` | in-tree `pir server` |
| `client` | remote client (`pir::client`, `pir client`) |
| `session` | JSONL persistence, tail-page reader (pulls the optional `memmap2`), discovery |
| `skills` | skill discovery/loading |
| `sdk` | `pir::sdk` facade |
| `subagent` | native `subagent` builtin |
| `tool-repair` | tool-repair pipeline |
| `tool-search` | BM25 `tool_search` |
| `todo` | `todo` builtin + `/todos` |
| `memory` | embedded cm-memory engine: `memory` tool, `/memory`, input auto-recall |
| `phone` | phone UI; compiled, default **off** |
| `brush` | embedded bash behind `pir __brush` |
| `full` | all of the above (the default build) |

The central pi-phone relay is a separate workspace package, **not** a feature
of `pir-sdk`: build it with `cargo build -p pir-phone-server`. The `phone`
feature compiles only the remote agent client (`/phone-start wss://...`); the
legacy embedded HTTP server is gone. See
[`docs/phone-server.md`](phone-server.md).

**Not features:** `compaction`/`telemetry` are core (public types span 21 files; no-op stubs rejected); `durable`
is core; `bug_report` is gated by `tui`; `print`/`rpc`/`export-html` and the `env-daemon` split are features.
Remaining deferred split: `tool-grep`/`tool-find` (both stay under `grep-find`); `grep-find` owns only
`grep-searcher`/`grep-regex`/`ignore` and implies no `tools`/`tool-*`. No `sqlite` feature:
`node:sqlite`/better-sqlite3 ride `extensions`; `context-mode` pulls bundled SQLite via `rusqlite`; `python` pulls
bundled `libsqlite3-sys`, so SQLite cannot be removed while Python is kept. SDK scope: [`docs/sdk.md`](sdk.md).

## 3. Default-on policy

Default-on: `subagent`, `mcp`, `codemode`, `python`, `context-mode`, `tool-search`, `tool-repair`, `todo`,
`memory` (mcp drags extensions; codemode needs extensions; context-mode lazy; memory's store lives next to the
session transcript). `phone` is **off** even in `full` — it opens an outbound
connection to the central relay and needs a server URL/device token. `--only-builtin` loads exactly the Rust builtins (no settings/package scan, QuickJS, SWC or `node:*`);
`--no-extensions` wins if both are given.

### 3.1 Default tool loadout (documented superset)

**`balanced` (the default) declares a deliberate SUPERSET of upstream pi's
`DEFAULT_TOOL_NAMES`.** With no `--tools` / `settings.defaultTools`, a full
build declares **22 tools**: the registration-active core nine plus the 13
default-active builtin-extension tools.

Core nine: `background_abort`, `background_status`, `bash`, `edit`, `find`,
`grep`, `ls`, `read`, `write` (`powershell` stays registered-but-inactive,
exactly like upstream). Builtin thirteen: `ctx_batch_execute`, `ctx_doctor`,
`ctx_execute`, `ctx_execute_file`, `ctx_fetch_and_index`, `ctx_index`,
`ctx_purge`, `ctx_search`, `ctx_stats`, `python`, `todo`, `memory`,
`subagent` — all default-active under the builtin-extension policy (§3).
`--no-extensions` leaves the core nine; `codemode`, `tool_search`, `powershell`
and MCP bridges stay registered-but-not-declared unless selected. Upstream pi
declares four (`read`, `bash`, `edit`, `write`).

This is a product decision, not drift: the extra core five are pir's own
capabilities — the in-process ripgrep `grep`/`find` pair, `ls`, and the
scheduler-owned `background_status`/`background_abort` controls (see
`docs/tui.md` and `MAPPING.md` for the background/detach surface) — and the 13
builtins are the default-on extension layer. The measured cost of the full
declaration is ~+30.7 KB of `tools[]` JSON on the first request (~+7.7k tokens
at 4 chars/token; ~26 KB of it the builtins), against pi's 4-tool 2,969 B (see
the round-1 A/B report at
`.play/evidence/ab-round1/lane-ab-prompt/REPORT.md` § 7 and the 2026-11-14
decision brief `.play/live-assist/x10-tool-loadout.md`, which records this as
the decided deliberate deviation).

Two ways to shrink it, both first-class:

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
| `no-js` | `cli,tui,env,env-daemon,server,client,subagent,tool-repair,tool-search,todo,memory,python,context-mode,phone,provider-*,tools,grep-find,image-tools,session,skills` |
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
| `python` | +33.09 MiB | RustPython (+391) | R29 100 engine drives: 9.82 / 46.2 MiB (+7.64 live / +30.7 WS) |
| `context-mode` | +34.92 MiB | python + rusqlite (+399) | R29 100 engine drives: 11.77 / 32.4 MiB (+9.60 live / +16.9 WS) |
| `image-tools` | +0.00 MiB | image (+31, already linked) | — |
| `session+tools` | +2.32 MiB | memmap2 | R28 100 prompts: 11.08 / 26.4 MiB (+4.82 live over baseline) |

MiB = 1024² bytes (the R28/R29 REPORT tables quote the same samples in
decimal MB, ~5% larger: memory +10.59 MB, mcp +3.52 MB, python +8.01 MB,
context-mode +10.06 MB live deltas).

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
| `python` | `builtin:python` not compiled; `pir python` falls back to system python unless `--no-fallback` |
| `context-mode` | `builtin:context-mode` not compiled; no `ctx_*` |
| `mcp` / `mcp-sd` | `mcp check/list/login/logout`, `mcp-sd` missing-feature; `builtin:mcp` not compiled; `mcp add/remove` still write config / `pir mcp-sd` reports missing `mcp-sd` (the `cli,mcp` build compiles; tested) |
| `env` / `env-daemon` | dependency-only backend (no daemon): `pir env-daemon` needs `env-daemon` / `` `pir env-daemon` is unavailable: this build was built without the `env-daemon` feature `` (exit 1); `frame`/`decode` and the remote-env client still compile |
| `server` / `client` | `pir server` / `pir client` missing-feature; `pir::client` absent |
| `session` | open/persist/discovery return `SESSION_PERSISTENCE_DISABLED`; `--session/--fork` report it; `--session-id` warns in-memory |
| `skills` | discovery empty + one diagnostic; prompt renders |
| `sdk` | `pir::sdk` absent (no stub); enable `sdk` |
| `subagent`/`tool-repair`/`tool-search`/`todo`/`memory` | `builtin:<name>` reports “not compiled … feature off” |
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
| `tool-background` | `background_status`/`background_abort` absent; explicit names report `tool-background` |
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
bash scripts/feature_matrix.sh            # 85 cargo invocations (57 lib + 18 bin + 2 consumer + 8 smoke)
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

- `mcp→extensions`, `mcp-sd→mcp`, `context-mode→python`, `image-protocol→tui` are deliberate. The seven
  `tool-*` leaves are dependency-free; `tools` is exactly their union.
