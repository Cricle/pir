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
5. `scripts/feature_matrix.sh` asserts **81 rows** (57 lib + 14 bin + 2 SDK consumer + 8 smoke). Every selectable
   feature except `default`/`full` has its own `cargo check --lib --no-default-features --features <f>` row.

## 2. Feature graph

**Core (always compiled):** `agent`, `agent_session`, `ai` (plain `/chat/completions` adapter;
Responses/Codex/`pi-messages` are `provider-*`-gated), `minimal` (zero-feature profile), `config`, `compaction`,
`durable`, `diagnostics`, `env_client`, `logging`, `plugins` framework, `protocol`, `session_manager`,
`system_prompt`, `telemetry`, `text`, `coding_agent::json` and tool plumbing. Every core-tool leaf is off (the
seven `tool-*` leaves and `grep-find`), so `register_core_tools` is a no-op and `CORE_TOOL_NAMES` is empty.

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
| `phone` | phone UI; compiled, default **off** |
| `brush` | embedded bash behind `pir __brush` |
| `full` | all of the above (the default build) |

**Not features:** `compaction`/`telemetry` are core (public types span 21 files; no-op stubs rejected); `durable`
is core; `bug_report` is gated by `tui`; `print`/`rpc`/`export-html` and the `env-daemon` split are features.
Remaining deferred split: `tool-grep`/`tool-find` (both stay under `grep-find`); `grep-find` owns only
`grep-searcher`/`grep-regex`/`ignore` and implies no `tools`/`tool-*`. No `sqlite` feature:
`node:sqlite`/better-sqlite3 ride `extensions`; `context-mode` pulls bundled SQLite via `rusqlite`; `python` pulls
bundled `libsqlite3-sys`, so SQLite cannot be removed while Python is kept. SDK scope: [`docs/sdk.md`](sdk.md).

## 3. Default-on policy

Default-on: `subagent`, `mcp`, `codemode`, `python`, `context-mode`, `tool-search`, `tool-repair`, `todo` (mcp
drags extensions; codemode needs extensions; context-mode lazy). `phone` is **off** even in `full` — it binds a
port. `--only-builtin` loads exactly the Rust builtins (no settings/package scan, QuickJS, SWC or `node:*`);
`--no-extensions` wins if both are given.

## 4. Slim profiles

The binary profiles (`no-js`, `tui-slim`) require `cli`; `sdk-headless` is library-only; `minimal` is the
**zero-feature** executable profile. Sizes and opt-level trade-offs: [`RESULTS.md`](../RESULTS.md) and
`logs/w18-help/q55/SIZE-TUNE-REPORT.md`.

| Profile | Feature set |
|---|---|
| `full` | `default = ["full"]` |
| `minimal` | `--no-default-features` (the `minimal` marker adds nothing). Builds `pir-minimal`, not the full `pir` CLI: one OpenAI-compatible provider, one `read_file` tool, in-memory session |
| `no-js` | `cli,tui,env,env-daemon,server,client,subagent,tool-repair,tool-search,todo,python,context-mode,phone,provider-*,tools,grep-find,image-tools,session,skills` |
| `tui-slim` | `cli,tui,tools,provider-openai,provider-anthropic,session,skills` (no `env-daemon`) |
| `sdk-headless` | bare + needed `provider-*`/`sdk` |

`tui-slim` omits `env-daemon` (its `pir env-daemon` returns the off-error; `no-js` keeps it). Minimal is
`/chat/completions` + `read_file` only (confined to cwd), no persistence/compaction/skills/MCP/extensions;
`memmap2` only for `session`.

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
| `subagent`/`tool-repair`/`tool-search`/`todo` | `builtin:<name>` reports “not compiled … feature off” |
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

## 6. Verification

```bash
bash scripts/feature_matrix.sh            # 81 rows
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
