# Settings reference

`pir` reads settings from three scopes, later winning:

1. user — `~/.pi/agent/settings.json`
2. project — `<cwd>/.pi/settings.json` (trust-gated; see [security.md](security.md))
3. local project — the same file with `-l`/`--local` writes

The authoritative shape is [`schemas/settings.schema.json`](../schemas/settings.schema.json)
(67 top-level keys, validated with `deny_unknown_fields` where noted). This page
groups them by purpose; the schema's `description` fields carry the exact
semantics. Unknown keys are preserved, not rejected, except inside MCP server
config.

## Models and thinking

| Key | Purpose |
|---|---|
| `defaultProvider`, `defaultModel` | Default provider/model id |
| `enabledModels` | Model scope (same as `--models`) |
| `defaultThinkingLevel` | `off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max` |
| `modelThinkingLevels` | Per-model thinking-level overrides |
| `thinkingBudgets` | Token budgets per thinking level |
| `hideThinkingBlock` | Collapse reasoning in the transcript |
| `showCacheMissNotices` | Surface cache-miss notices |

## Tools

| Key | Purpose |
|---|---|
| `tools` | Per-tool execution settings. `commandTimeoutMs` (default 120000; `0` restores upstream's no-default timeout), `broadSearchGuard` (default true) |
| `defaultTools` | Default tool selection (same as `--tools`) |
| `performance` | `profile: "balanced" \| "efficient"` and its overrides ([performance-profiles.md](performance-profiles.md)) |
| `npmCommand` | argv form used for npm lookup/install |
| `doubleEscapeAction` | What a double Escape does |

## Session and storage

| Key | Purpose |
|---|---|
| `sessionDir` | Session root override |
| `sessionRetention` | `maxAgeDays` / `maxSessions` / `maxBytes` (drives `pir sessions prune`) |
| `compaction` | Auto-compaction thresholds |
| `branchSummary` | Branch-summarization behavior |
| `queueMode`, `steeringMode`, `followUpMode` | Input queueing (`one-at-a-time` vs `all`) |
| `checkpoint` (via `checkpoint` subcommand) | Per-session file checkpoint policy |

## Resources

| Key | Purpose |
|---|---|
| `packages` | Installed package sources |
| `extensions` | Extension enable/disable entries |
| `disabledBuiltinExtensions` | Builtins to skip |
| `defaultDisabledPackagesEnabled` | Opt in to packages pir keeps off (`@malinamnam/pi-phone`); **global scope only** |
| `skills`, `prompts`, `themes` | Resource path lists |
| `enableSkillCommands` | Register `/skill:<name>` |
| `extensionLimits` | Per-extension resource limits (project settings cannot loosen them) |

## Transport and providers

| Key | Purpose |
|---|---|
| `transport` | Provider transport tuning |
| `retry` | Retry policy (`maxRetries`, delay caps, `timeoutMs`) |
| `httpProxy`, `httpIdleTimeoutMs` | HTTP proxy and idle timeout |
| `websocketConnectTimeoutMs`, `websockets`, `cacheWarming` | WebSocket and cache-warming controls |
| `sessionShareDestination` | Where shared sessions go |

## Terminal and TUI

| Key | Purpose |
|---|---|
| `theme` | Theme name or path |
| `tuiMode` | Fullscreen vs inline |
| `fullscreenExitOutput`, `fullscreenScrollbar`, `fullscreenCopyOnSelect`, `fullscreenWheelScrollLines` | Fullscreen behavior |
| `keybindings` | Keybinding overrides ([keybindings.md](keybindings.md)) |
| `terminal` | Terminal capability overrides |
| `images` | Inline-image limits and resize |
| `markdown` | Markdown rendering options |
| `editorPaddingX`, `outputPad`, `autocompleteMaxVisible`, `showHardwareCursor` | Editor/layout |
| `externalEditor`, `shellPath`, `shellCommandPrefix`, `shellPool` | Shell and editor integration |
| `quietStartup`, `collapseChangelog` | Startup noise |

## Trust and telemetry

| Key | Purpose |
|---|---|
| `defaultProjectTrust` | Default for untrusted projects |
| `deviceId`, `trackingId` | Identity for telemetry |
| `enableInstallTelemetry`, `enableAnalytics` | Anonymous telemetry opt-ins |
| `warnings` | Warning suppression |
| `lastChangelogVersion` | Changelog bookkeeping |

## Builtins

| Key | Purpose |
|---|---|
| `memory` | `memory` builtin store policy ([memory-builtin.md](memory-builtin.md)) |
| `codemode` | `codemode` sandbox limits ([codemode.md](codemode.md)) |
| `treeFilterMode` | `/tree` filtering |

## Environment overrides

Many keys have a `PIR_*` equivalent that wins over the file — see
[`environment-variables.md`](environment-variables.md).
