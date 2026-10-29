# pir docs

pir ships these documentation pages (upstream pi's `docs/` set, adapted to
pir's Rust implementation). They are referenced from the system prompt and
code diagnostics.

| Page | Contents |
|---|---|
| [STATUS.md](STATUS.md) | Current truth: gates, evidence, residuals |
| [extensions.md](extensions.md) | JS/TS extensions, `pi`/`ctx` API, hooks, registration |
| [mcp.md](mcp.md) | MCP config, `pir mcp`, tool exposure |
| [models.md](models.md) | models.json, catalog, thinking-level maps |
| [custom-provider.md](custom-provider.md) | OpenAI-compatible providers, virtual models |
| [packages.md](packages.md) | `pir install`/`list`/`remove`, package layout |
| [keybindings.md](keybindings.md) | Keybinding ids, settings, defaults |
| [themes.md](themes.md) | Theme JSON schema, OKLCH/OKHSL colors |
| [skills.md](skills.md) | SKILL.md format, discovery, `/skill:name` |
| [prompt-templates.md](prompt-templates.md) | Prompt-template discovery and `/template` |
| [environment-variables.md](environment-variables.md) | Every `PI_*`/`PIR_*` variable |
| [codemode.md](codemode.md) | Codemode sandbox, script globals, non-LLM models |
| [tui.md](tui.md) | TUI components, alt-screen, renderer hooks |
| [sdk.md](sdk.md) | Rust SDK: quickstart, builder/event reference |
| [hooks.md](hooks.md) | Hook/event surface, `ctx.ui.custom` deviation |
| [durable-spec.md](durable-spec.md) | Durable kernel: records, storage, harness |
| [FEATURES.md](FEATURES.md) | Feature graph, profiles, off-error contract |
| [upstream-drift-status.md](upstream-drift-status.md) | Historical audit closure |
| [todo-builtin.md](todo-builtin.md) | `todo` builtin: contract, `/todos`, persistence |

Upstream-relative pages with no pir counterpart are noted in `MAPPING.md`'s
deviation list.
