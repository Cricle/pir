# pir documentation

Reference for pir, the Rust reimplementation of the `pi` coding agent. Start
where your task is; every page is self-contained and links onward.

The upstream-relative pages that have no counterpart here are listed in
[`../MAPPING.md`](../MAPPING.md). Documents are English except `RESULTS.md` and
the archived baseline audit, which stay Chinese.

## Start here

| Page | Contents |
|---|---|
| [`../README.md`](../README.md) | Install, status, the release gate, security summary |
| [`../RESULTS.md`](../RESULTS.md) | Current status, gates, performance, residuals (Chinese) |
| [`../MAPPING.md`](../MAPPING.md) | Per-module TS → Rust port map |
| [`../CHANGELOG.md`](../CHANGELOG.md) | Release-by-release change list |

## Reference

| Page | Contents |
|---|---|
| [cli.md](cli.md) | Every subcommand and global flag |
| [settings.md](settings.md) | All 67 settings.json keys, grouped |
| [environment-variables.md](environment-variables.md) | Every `PI_*`/`PIR_*` variable |
| [models.md](models.md) | models.json, catalog, thinking levels, Claude Code env |
| [providers.md](providers.md) | Credential precedence, OAuth and API-key providers |
| [custom-provider.md](custom-provider.md) | OpenAI-compatible auto-detect, virtual models |
| [mcp.md](mcp.md) | MCP config, `pir mcp`, the built-in `sd` server, tool exposure |
| [packages.md](packages.md) | `pir install`/`list`/`remove`, package layout |
| [rpc.md](rpc.md) | The JSON Lines RPC surface (protocol version 8) |
| [security.md](security.md) | Server auth, trust gating, confinement, remote gates |
| [keybindings.md](keybindings.md) | Keybinding ids, settings, defaults |
| [themes.md](themes.md) | Theme JSON, OKLCH/OKHSL colors |

## Extend

| Page | Contents |
|---|---|
| [sdk.md](sdk.md) | Rust SDK: quickstart, builder/event reference |
| [extensions.md](extensions.md) | JS/TS extensions, the `pi`/`ctx` API |
| [hooks.md](hooks.md) | Hook/event coverage matrix and delivery limits |
| [skills.md](skills.md) | SKILL.md format, discovery, `/skill:name` |
| [prompt-templates.md](prompt-templates.md) | Template discovery, `/name` commands |
| [tui.md](tui.md) | TUI components, alt-screen, renderer hooks |
| [codemode.md](codemode.md) | Codemode sandbox, script globals |
| [foreman.md](foreman.md) | Foreman mode: delegation, approvals, worktrees, detached runs |

## Builtins

| Page | Contents |
|---|---|
| [todo-builtin.md](todo-builtin.md) | `todo` builtin: contract, `/todos`, persistence |
| [memory-builtin.md](memory-builtin.md) | `memory` builtin: embedded cm-memory, actions, auto-recall |
| [context-mode-builtin.md](context-mode-builtin.md) | `context-mode`: `ctx_*` Code Mode + FTS5 knowledge base |
| [tool-search-builtin.md](tool-search-builtin.md) | `tool-search`: BM25 deferred-tool discovery |
| [tool-repair-builtin.md](tool-repair-builtin.md) | `tool-repair`: tool-call repair pipeline |

## Internals

| Page | Contents |
|---|---|
| [architecture.md](architecture.md) | Workspace crates, UI plug-in seam, feature ownership |
| [FEATURES.md](FEATURES.md) | Feature graph, profiles, off-error contract, tool loadout |
| [durable-spec.md](durable-spec.md) | Durable kernel: records, storage, harness (pinned to upstream 1.4.0) |
| [performance-profiles.md](performance-profiles.md) | `performance.profile` presets and measurement method |
| [test-harness-map.md](test-harness-map.md) | How `tests/*.rs` maps to merged harnesses; how to run one suite |
| [phone-server.md](phone-server.md) | Multi-user pi-phone relay: accounts, `/agent` + `/ws`, deployment |
| [phone-server-performance.md](phone-server-performance.md) | Repeatable release-profile load test |

## Project history

| Page | Contents |
|---|---|
| [upstream-drift-status.md](upstream-drift-status.md) | Upstream drift audited through v1.5.2, plus remaining Waves B/C |
| [system-prompt-audit.md](system-prompt-audit.md) | System prompt vs upstream: aligned, deviations, host matrix, priorities |
| [archive/](archive/) | Per-release gate evidence, superseded A/B runs, the baseline audit |
