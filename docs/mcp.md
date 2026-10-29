# MCP servers

pir runs MCP servers as a builtin plugin (rmcp 3.5 client). Configure them
in settings.json (`mcpServers`, upstream-shaped; `deny_unknown_fields` on
the server config):

```json
{
  "mcpServers": {
    "docs": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/docs"]
    }
  }
}
```

## CLI

- `pir mcp list` — alias of `pir mcp check`: configured servers, their
  transport/source, and a bounded test-connect per server. (Upstream
  `pi mcp list [--json]` prints a richer per-server report — scope, exposure,
  transport line, `{servers, errors}` JSON — and exits 1 when an enabled
  server is not connected; pir's alias keeps the check report and exit shape.)
- `pir mcp check [<server>] [--json]` — the same probe, optionally for one
  server (also the default when no subcommand is given)
- `pir mcp login <server>` / `pir mcp logout <server>` — OAuth for remote
  servers (dynamic client registration supported)

## Built-in `sd` server (search & replace)

pir is also an MCP *server*: `pir mcp-sd` (hidden subcommand, stdio) exposes
one tool, `sd`, with [sd](https://github.com/chmln/sd) semantics — a regex
`pattern` plus a `replacement` (`$1`, `${name}`, `$$` for a literal `$`),
`literal` / `case_insensitive` / `whole_word` / `multiline` flags,
gitignore-aware walks, and `dry_run` **defaulting to true** (pass
`dry_run: false` to write in place). `max_matches` / `max_files` bound a call
and are reported as `truncated`. Paths resolve inside `--root` (default: the
server's cwd); anything resolving outside it is refused, and symlinks are
never followed. Register it like any other server (`<agentDir>/mcp.json`,
project `.pi/mcp.json`, or settings.json `mcpServers`) — `command` is this
same executable:

```json
{
  "mcpServers": {
    "sd": {
      "type": "stdio",
      "command": "C:/path/to/pir.exe",
      "args": ["mcp-sd", "--root", "C:/path/to/project"]
    }
  }
}
```

The result carries a text block plus structured content: `dry_run`,
`files_scanned`, `files_changed`, `replacements`, `truncated`,
`binary_skipped`, `errors`, `files[{path, replacements, preview}]`.

## Tool exposure

MCP tools register as `mcp__<server>__<tool>` with annotations carried
through. `exposure` selects how the model reaches them:

- `direct` (the default, see `builtinTools` below) — declared like a
  regular builtin tool;
- `codemode` — callable from `codemode` scripts (`searchTools()`/the tool
  name), never declared to the model directly;
- `deferred` — declared to the model only after `tool_search` loads them;
- `hidden` — registered but unreachable.

`toolExposure` overrides the exposure of individual tools; it always wins
over the base `exposure` (and therefore over the `builtinTools` default).

### `builtinTools`

The top-level `mcp.json` flag `builtinTools` (default `true`) decides what a
server that does NOT set `exposure` gets:

- `true` (default) — `direct`: the server's tools are declared to the model
  like builtin tools, with no `codemode`/`tool_search` hop;
- `false` — upstream pi's default, `codemode`.

```json
{
  "builtinTools": false,
  "mcpServers": { "docs": { "command": "npx", "args": ["-y", "@mcp/docs"] } }
}
```

An explicit `"exposure": "codemode" | "deferred" | "hidden" | "direct"` on a
server always wins, and so does a per-tool `toolExposure` entry. The flag is
read only from the wrapper shape (`mcpServers` present, like
`autoEnableCodemode`); in the legacy flat shape a `builtinTools` key is
treated as a server entry, and the file's servers keep the `codemode`
default. The project `.pi/mcp.json` value overrides the global one; with
neither set the default is `true`. A non-boolean value is reported as a
config error and treated as absent.

`--no-mcp` / `disabledBuiltinExtensions: ["mcp"]` still disable the whole
plugin; `builtinTools` only changes the default exposure.

### Automatic discovery activation

A tool that is not active cannot be used to reach anything, so the builtin
activates the discovery tool the configured exposures ask for
(upstream `ensureDiscoveryActive`):

- at least one **enabled** server with `codemode` exposure (or a per-tool
  `toolExposure: codemode`) activates **`codemode`**;
- at least one enabled server with `deferred` exposure activates
  **`tool_search`**.

The top-level `mcp.json` flag `autoEnableCodemode` (default `true`)
suppresses the `codemode` half only:

```json
{
  "autoEnableCodemode": false,
  "mcpServers": { "docs": { "command": "npx", "args": ["-y", "@mcp/docs"] } }
}
```

With the flag off and no `direct`/`deferred` tools, the server's tools are
unreachable; pir logs the same "MCP tools are only reachable from the
codemode or tool_search tool" warning upstream emits.

Activation semantics:

- the name is **appended** to the session's active set, never a replacement:
  `--tools`/`--exclude-tools`, `settings.defaultTools`, a JS
  `setActiveTools` call, and `tool_search`'s loaded matches keep every tool
  they selected. An explicit `--tools codemode` is therefore respected and is
  never removed by the MCP plugin;
- the plugin tracks what it added. When the last enabled server that needed
  a discovery tool disappears (removed from `mcp.json`, disabled, or through
  `/mcp`), only that auto-added name is removed; user-selected names stay;
- the decision is recomputed on every registry rebuild (`/reload`, a
  mid-session `mcp.json` edit, `pi.registerMcpServer`/`unregisterMcpServer`),
  at `session_start`, and after a `/mcp` action. Repeated rebuilds are
  idempotent: the active set is written only when it actually changes, so no
  duplicate names and no loadout churn.

The per-tool selection `--tools mcp__docs__search` continues to work for
direct and deferred loading; it does not activate `codemode` by itself.

## `/mcp`

The native `/mcp` command lists every registered server with its connection
state, exposure, discovered tool count, and source (`global`, `project`,
`extension`):

```
MCP servers:
  docs (lazy · direct · 0 tools · global)
```

Arguments apply to the current session and update the discovery activation
immediately:

- `/mcp enable <server>` / `/mcp disable <server>` — toggle the entry (a
  disabled entry stays registered but is never connected);
- `/mcp remove <server>` — unregister it and drop its discovered tools and
  live connection; `/mcp enable` can restore it from `mcp.json`;
- `/mcp reconnect <server>` — reconnect and re-list its tools.

Unlike upstream, pir's `/mcp` does not write the changes back to `mcp.json`;
use `pir mcp add` / `pir mcp remove` for persistent edits.


## Auth

Remote (SSE/streamable-HTTP) servers support OAuth: Authorization Code with
PKCE via `pir mcp login`, token refresh handled per request, storage in
`<agentDir>/mcp-auth.json` (upstream `McpOAuthCredentialStore`; separate from
the provider tokens in `<agentDir>/auth.json`).
