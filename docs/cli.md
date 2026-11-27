# CLI reference

`pir` with no subcommand starts the interactive TUI. Anything else is a
subcommand or a global flag. Parsed by clap in
`crates/pir-core/src/coding_agent/cli.rs`; per-command bodies live in
`src/main_cli/`.

```bash
pir                          # interactive TUI agent
pir print "<prompt>"         # non-interactive single run
pir print --mode json "<p>"  # JSONL event stream
pir -p "<prompt>"            # --print shorthand; `-p a -p b` runs a then b
```

## Subcommands

| Command | Purpose |
|---|---|
| `interactive` | The interactive TUI agent, spelled explicitly. `pir` with no subcommand enters the same mode when stdin/stdout is a TTY; the subcommand exists for callers that cannot rely on that inference. Requires the `tui` feature |
| `print` | Non-interactive run. Positional prompts each run as their own turn; `@file` is read as a file reference; stdin is read when the prompt is omitted or `-`. `--mode json` switches to the JSONL event stream. |
| `rpc` | JSON Lines RPC over stdin/stdout ([rpc.md](rpc.md)) |
| `server` | Remote session server. Requires `PIR_SERVER_AUTH_TOKEN`; `--unix <path>` binds a Unix socket; `--allow-unauthenticated` is a local-dev escape hatch only ([security.md](security.md)) |
| `client` | Connect to a remote server and print its session count |
| `auth` | `status` (default), `login <provider>`, `logout <provider>`, `check`, `print-api-key` ([providers.md](providers.md)) |
| `models` | `list` (the registry) or print the configured model |
| `session` | Print session storage status |
| `sessions` | `prune` — delete sessions beyond the retention bounds (`--dry-run`, `--max-age-days`, `--max-sessions`, `--max-bytes`, `--all`, `--json`) |
| `jobs` | Detached background jobs: `start`, `list`, `status`, `wait`, `log`, `reap`, `kill`, `clean` |
| `checkpoint` | Per-session file checkpoints: `list`, `restore`, `run` |
| `foreman` | Detached foreman runs and approvals: `run`, `status`, `logs`, `abort`, `approve`, `deny`, `resume`, `clean` ([foreman.md](foreman.md)) |
| `mcp` | MCP server management: `add`, `remove`, `check`, `login`, `logout` ([mcp.md](mcp.md)) |
| `install` / `remove` / `uninstall` | Package sources (git URL, `npm:<name>`, local path); `-l`/`--local` writes project settings ([packages.md](packages.md)) |
| `update` | Refresh packages: `--models`, `--extensions`, `--all`, `--self`, `--force` |
| `list` | List installed packages from user and project settings |
| `config` | Package/resource listing (`--json`), `--enable`/`--disable KIND:PATTERN`, `--source` |
| `grep` / `sed` | Native grep/sed; unsupported syntax execs the system tool with the original argv (`--no-fallback`/`PIR_NATIVE_FALLBACK=0` disables) |
| `version` | Print version information |

`pir jobs` is pir-native (upstream has no `jobs` command). Each job lives at
`<agent-dir>/jobs/<job-id>/` with `meta.json` and an `output.log` capped at
256 MiB, written by a detached supervisor. `start` takes `--cwd <dir>` and
`--shell <path>` (a `-c` shell is required) and needs the command after `--`.

## Global flags

Accepted by every subcommand (`global = true`).

### Session selection

| Flag | Effect |
|---|---|
| `-c`, `--continue` | Continue the most recent session |
| `-r`, `--resume` | Resume picker |
| `--session <id\|path>` | Open a specific session |
| `--session-id <uuid>` | Open an exact id |
| `--fork <id\|path>` | Fork a session |
| `--no-session` | Ephemeral: nothing persisted |
| `--session-dir <dir>` | Override the session root |
| `--name <NAME>` | Session name |
| `--no-context-files` | Skip AGENTS.md / CLAUDE.md |

### Model and prompt

| Flag | Effect |
|---|---|
| `--provider <NAME>` / `--model <PATTERN>` | Select the provider / model |
| `--models <pattern>` | Model scope (settings `enabledModels`) |
| `--list-models [search]` | List the resolved registry, optionally fuzzy-filtered |
| `--api-key <key>` | Explicit credential |
| `--thinking <LEVEL>` | Thinking level |
| `--system-prompt <TEXT>` / `--append-system-prompt <TEXT>` | Override/append the prompt |
| `--perf-profile <name>` | `balanced` (default) or `efficient` ([performance-profiles.md](performance-profiles.md)) |

### Tools and extensions

| Flag | Effect |
|---|---|
| `--tools <TOOLS>` / `--exclude-tools <TOOLS>` | Explicit tool selection / exclusion |
| `--no-tools` / `--no-builtin-tools` | Drop all tools / only the builtin ones |
| `-e`, `--extension <PATH>` | Load a JS/TS extension (repeatable) |
| `--no-extensions` (alias `--ne`) | Skip discovery |
| `--only-builtin` | Load exactly the Rust builtins (no settings/package scan, no QuickJS/SWC) |
| `--no-mcp` | Skip MCP servers |
| `--help-extensions` | Load extensions and list the flags they registered |

### Resources

| Flag | Effect |
|---|---|
| `--skill <PATH>` / `--no-skills` | Skill paths / disable discovery ([skills.md](skills.md)) |
| `--prompt-template <PATH>` / `--no-prompt-templates` | Prompt templates ([prompt-templates.md](prompt-templates.md)) |
| `--theme <PATH>` / `--use-theme <NAME>` / `--no-themes` | Themes ([themes.md](themes.md)) |
| `--mode <MODE>` | Foreman/plan/normal mode |
| `--tui-mode <MODE>` | TUI layout mode |

### Trust, output and misc

| Flag | Effect |
|---|---|
| `-a`, `--approve` | Trust project-local files for this run |
| `-na`, `--no-approve` | Ignore project-local files for this run |
| `--export <...>` | Render the session to HTML |
| `--offline` | Disable startup network operations (same as `PI_OFFLINE=1`) |
| `--verbose` | Verbose logging |

`--help` never executes extension code; only `--help-extensions` loads them.

## See also

- [`settings.md`](settings.md) — the settings.json surface behind these flags
- [`environment-variables.md`](environment-variables.md) — every `PI_*`/`PIR_*` var
- [`security.md`](security.md) — server auth, trust, and remote-execution defaults
