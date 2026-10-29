# Codemode

The `codemode` builtin gives the model a sandboxed script whose only
capability is calling other tools (upstream `extensions/codemode`).

## Activation

Register a `codemode` tool (replaceable like every builtin), or
`--tools +codemode`. The sandbox executes a QuickJS script per call with a
single bridge: `await executeTool(name, params)` routes through the same
hooks/scheduler path as nested tool calls (depth-capped).

## Worker process

By default the script runs in a `pir __codemode-worker` child process
(crash isolation, `PIR_CODEMODE_WORKER=0` falls back to a thread). Limits:
`PIR_CODEMODE_TIMEOUT_MS`, `PIR_CODEMODE_MAX_OUTPUT_CHARS`,
`PIR_CODEMODE_MAX_OUTPUT_ITEMS`, and the QuickJS heap cap
`codemode.sandboxMemoryMb` (default 128 MiB, `PIR_CODEMODE_MEMORY_MB`
override, `0` = no cap), applied identically to both sandbox engines.

## Script surface

The nested-tool-call bridge is the tier-1 surface. The upstream helper
globals (`ALL_TOOLS`, `searchTools()`, `describeTool()`, `store()`,
`load()`) are not exposed yet — recorded in MAPPING.md's deviation list.

## Non-LLM models

`pi.registerProvider` also registers classifier and image models; their
request/response shapes follow upstream's `ImageApi`/`ClassifierApi` wire
names (`openrouter-images`, `typesafe-system-one`, `openai-decisions`,
`cloudflare-workers-ai-system-one`). `models.generateImages` /
`models.classify` are callable from extensions.
