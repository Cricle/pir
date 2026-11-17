# pir docs

Documentation for pir (upstream pi's `docs/`, adapted to Rust), referenced by the system prompt and code
diagnostics; upstream-relative pages with no counterpart are in [MAPPING.md](../MAPPING.md).

## Core reference

| Page | Contents |
|---|---|
| [RESULTS.md](../RESULTS.md) | Final result: status, gates, performance, residuals |
| [FEATURES.md](FEATURES.md) | Feature graph, profiles, off-error contract, default tool loadout |
| [sdk.md](sdk.md) | Rust SDK: quickstart, builder/event reference |
| [extensions.md](extensions.md) | JS/TS extensions, `pi`/`ctx` API, hooks |
| [hooks.md](hooks.md) | Hook/event surface, `ctx.ui.custom` deviation |
| [mcp.md](mcp.md) | MCP config, `pir mcp`, tool exposure |
| [models.md](models.md) | models.json, catalog, thinking-level maps |
| [custom-provider.md](custom-provider.md) | OpenAI-compatible providers, virtual models |
| [packages.md](packages.md) | `pir install`/`list`/`remove`, package layout |
| [keybindings.md](keybindings.md) | Keybinding ids, settings, defaults |
| [themes.md](themes.md) | Theme JSON, OKLCH/OKHSL colors |
| [skills.md](skills.md) | SKILL.md format, discovery, `/skill:name` |
| [prompt-templates.md](prompt-templates.md) | Template discovery, `/name` commands, argument substitution |
| [environment-variables.md](environment-variables.md) | Every `PI_*`/`PIR_*` variable |
| [codemode.md](codemode.md) | Codemode sandbox, script globals |
| [tui.md](tui.md) | TUI components, alt-screen, renderer hooks |
| [durable-spec.md](durable-spec.md) | Durable kernel: records, storage, harness |
| [phone-server.md](phone-server.md) | Central multi-user pi-phone relay: accounts/devices/session metadata, `/agent` + `/ws`, CLI and deployment |

## Builtins

| Page | Contents |
|---|---|
| [todo-builtin.md](todo-builtin.md) | `todo` builtin: contract, `/todos`, persistence |
| [memory-builtin.md](memory-builtin.md) | `memory` builtin: embedded cm-memory, `memory` tool actions, auto-recall, store/settings |
| [context-mode-builtin.md](context-mode-builtin.md) | `context-mode` builtin: `ctx_*` RustPython execution + FTS5 knowledge base |
| [tool-search-builtin.md](tool-search-builtin.md) | `tool-search` builtin: BM25 deferred-tool discovery |
| [tool-repair-builtin.md](tool-repair-builtin.md) | `tool-repair` builtin: tool-call repair pipeline |

## Audits, performance, development

| Page | Contents |
|---|---|
| [upstream-drift-status.md](upstream-drift-status.md) | Verified upstream drift through v1.5.2: 641 commits, 130 touching ported source, per-commit Landed/Partial/Open/N-A evidence |
| [upstream-next-plan.md](upstream-next-plan.md) | Remaining v1.5.2 follow-ups: A/B status and Wave B/C plan with upstream citations |
| [system-prompt-audit.md](system-prompt-audit.md) | System prompt audit vs upstream: aligned sections, open deviations, host matrix, fix priorities |
| [performance-profiles.md](performance-profiles.md) | `performance.profile` presets, loadout deltas, measurement method |
| [phone-server-performance.md](phone-server-performance.md) | Repeatable release-profile load test: 10k catalog, fan-out, reconnect, RSS and CPU |
| [test-harness-map.md](test-harness-map.md) | Generated map of every test file → merged harness/standalone target |
| [archive/](archive/) | Historical release chronology and superseded A/B reports |
