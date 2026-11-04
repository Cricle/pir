# pir docs
Documentation for pir (upstream pi's `docs/`, adapted to Rust), referenced by the system prompt and code
diagnostics; upstream-relative pages with no counterpart are in [MAPPING.md](../MAPPING.md).

| Page | Contents |
|---|---|
| [RESULTS.md](../RESULTS.md) | Final result: status, gates, performance, residuals |
| [extensions.md](extensions.md) | JS/TS extensions, `pi`/`ctx` API, hooks |
| [mcp.md](mcp.md) | MCP config, `pir mcp`, tool exposure |
| [models.md](models.md) | models.json, catalog, thinking-level maps |
| [custom-provider.md](custom-provider.md) | OpenAI-compatible providers, virtual models |
| [packages.md](packages.md) | `pir install`/`list`/`remove`, package layout |
| [keybindings.md](keybindings.md) | Keybinding ids, settings, defaults |
| [themes.md](themes.md) | Theme JSON, OKLCH/OKHSL colors |
| [skills.md](skills.md) | SKILL.md format, discovery, `/skill:name` |
| [prompt-templates.md](prompt-templates.md) | Template discovery and `/template` |
| [environment-variables.md](environment-variables.md) | Every `PI_*`/`PIR_*` variable |
| [codemode.md](codemode.md) | Codemode sandbox, script globals |
| [tui.md](tui.md) | TUI components, alt-screen, renderer hooks |
| [sdk.md](sdk.md) | Rust SDK: quickstart, builder/event reference |
| [hooks.md](hooks.md) | Hook/event surface, `ctx.ui.custom` deviation |
| [durable-spec.md](durable-spec.md) | Durable kernel: records, storage, harness |
| [FEATURES.md](FEATURES.md) | Feature graph, profiles, off-error contract |
| [upstream-drift-status.md](upstream-drift-status.md) | Verified upstream drift: 184 commits since the recorded baseline, 62 after v1.4.0, with per-commit Landed/Partial/Open/N-A evidence |
| [todo-builtin.md](todo-builtin.md) | `todo` builtin: contract, `/todos`, persistence |
| [cm-rs-integration-plan.md](cm-rs-integration-plan.md) | cm-rs memory engine integration research: blockers, API refactor, plugin design, phased plan (not implemented) |
| [system-prompt-audit.md](system-prompt-audit.md) | System prompt audit vs upstream: aligned sections, deviations, missing host wiring, fix priorities (not implemented) |
