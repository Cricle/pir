# MCP servers

pir runs MCP servers as a builtin plugin (rmcp 3.5 client). Configure them in settings.json (`mcpServers`,
upstream-shaped; `deny_unknown_fields` on the server config):

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

- `pir mcp list [--json]` / `pir mcp check [<server>] [--json]` — configured servers with scope, exposure,
  connection state and errors (list exits non-zero on a failed connect or invalid config), plus a bounded
  test-connect, optionally for one server (check is also the default with no subcommand).
- `pir mcp login <server>` / `pir mcp logout <server>` — OAuth for remote servers (dynamic client registration
  supported).

## Built-in `sd` server (search & replace)

pir is also an MCP *server*: `pir mcp-sd` (hidden subcommand, stdio) exposes one tool, `sd`, with
[sd](https://github.com/chmln/sd) semantics — a regex `pattern` plus `replacement` (`$1`, `${name}`, `$$` for a
literal `$`), `literal` / `case_insensitive` / `whole_word` / `multiline` flags, gitignore-aware walks, and
`dry_run` **defaulting to true** (pass `dry_run: false` to write in place). `max_matches` / `max_files` bound a
call and are reported as `truncated`. Paths resolve inside `--root` (default: the server's cwd); anything outside
is refused, and symlinks are never followed. Register it like any other server (`<agentDir>/mcp.json`, project
`.pi/mcp.json`, or settings `mcpServers`); `command` is this executable:

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

The result carries a text block plus structured content: `dry_run`, `files_scanned`, `files_changed`,
`replacements`, `truncated`, `binary_skipped`, `errors`, `files[{path, replacements, preview}]`.

## Tool exposure

MCP tools register as `mcp__<server>__<tool>` with annotations carried through. `exposure` selects how the model
reaches them:

- `direct` (the default, see `builtinTools` below) — declared like a regular builtin tool;
- `codemode` — callable from `codemode` scripts (`searchTools()`/the tool name), never declared to the model
  directly;
- `deferred` — declared only after `tool_search` loads them;
- `hidden` — registered but unreachable.

`toolExposure` overrides individual tools; it always wins over the base `exposure` (and the `builtinTools`
default).

### `builtinTools`

The top-level `mcp.json` flag `builtinTools` (default `true`) decides what a server that does NOT set `exposure`
gets:

- `true` (default) — `direct`: tools are declared like builtin tools, with no `codemode`/`tool_search` hop;
  `false` — upstream pi's default, `codemode`.

```json
{
  "builtinTools": false,
  "mcpServers": { "docs": { "command": "npx", "args": ["-y", "@mcp/docs"] } }
}
```

An explicit `"exposure": "codemode" | "deferred" | "hidden" | "direct"` on a server always wins, as does a per-tool
`toolExposure` entry. Only the wrapper shape (`mcpServers` present, like `autoEnableCodemode`) reads the flag; in
the legacy flat shape `builtinTools` is treated as a server entry and servers keep the `codemode` default. Project
`.pi/mcp.json` overrides global; default `true` when neither sets it. A non-boolean value is a config error and
treated as absent. `--no-mcp` / `disabledBuiltinExtensions: ["mcp"]` still disable the whole plugin; `builtinTools`
only changes the default exposure.

### Automatic discovery activation

The builtin activates the discovery tool the configured exposures ask for (`ensureDiscoveryActive`): at least one
**enabled** server with `codemode` exposure (or per-tool `toolExposure: codemode`) activates `codemode`; one with
`deferred` exposure activates `tool_search`.

The top-level `mcp.json` flag `autoEnableCodemode` (default `true`) suppresses the `codemode` half only. With it
off and no `direct`/`deferred` tools, the server's tools are unreachable; pir logs the same "MCP tools are only
reachable from the codemode or tool_search tool" warning upstream emits.

Activation semantics:

- the name is **appended** to the session's active set, never a replacement: `--tools`/`--exclude-tools`,
  `settings.defaultTools`, a JS `setActiveTools` call and `tool_search` matches keep every selected tool; an
  explicit `--tools codemode` is respected and never removed by the plugin;
- the plugin tracks what it added: when the last enabled server needing a discovery tool disappears (removed,
  disabled, or through `/mcp`), only that auto-added name is removed; user-selected names stay;
- the decision is recomputed on every registry rebuild (`/reload`, a mid-session `mcp.json` edit,
  `pi.registerMcpServer`/`unregisterMcpServer`), at `session_start`, and after a `/mcp` action; rebuilds are
  idempotent (the active set is written only when it changes).

The per-tool selection `--tools mcp__docs__search` works for direct and deferred loading; it does not activate
`codemode`.

## `/mcp`

The native `/mcp` command lists every registered server with connection state, exposure, discovered tool count, and
source (`global`, `project`, `extension`):

```
MCP servers:
  docs (lazy · direct · 0 tools · global)
```

Arguments apply to the current session and update discovery activation immediately: `/mcp enable <server>` / `/mcp
disable <server>` toggle the entry (disabled stays registered but never connects); `/mcp remove <server>`
unregisters it and drops its tools/connection (`/mcp enable` restores it from `mcp.json`); `/mcp reconnect
<server>` reconnects and re-lists its tools.

Unlike upstream, pir's `/mcp` does not write changes back to `mcp.json`; use `pir mcp add` / `pir mcp remove` for
persistent edits.

## Auth

Remote (SSE/streamable-HTTP) servers support OAuth: Authorization Code with PKCE (`pir mcp login`), per-request
token refresh, storage in `<agentDir>/mcp-auth.json` (upstream `McpOAuthCredentialStore`; separate from provider
tokens in `<agentDir>/auth.json`).
