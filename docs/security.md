# Security boundaries

The trust and authentication model, collected in one place. The defaults are
conservative; every escape hatch is named and off unless you turn it on.

## Remote server (`pir server`)

`pir server` requires `PIR_SERVER_AUTH_TOKEN`. The token is:

- compared in **constant time**,
- **never logged**,
- checked **before the handshake** completes.

It provides authentication, **not transport encryption**. Keep
`PIR_SERVER_ADDRESS` on loopback or a private network, or put it behind an
authenticated TLS proxy or VPN.

### `--allow-unauthenticated` is local-development only

Without a token, **any local process** can create sessions, use the server's
model credentials, and read/write files under its working directory. Loopback
binding does **not** isolate other local users. Never use the flag on a shared
host: it requires `PIR_SERVER_AUTH_TOKEN` to be unset, so setting the token is
the way to close it again.

### Limits

| Variable | Purpose |
|---|---|
| `PIR_SERVER_ALLOWED_ROOTS` | Workspace roots the server may touch |
| `PIR_SERVER_MAX_CONNECTIONS` | Concurrent connections |
| `PIR_SERVER_MAX_CONNECTION_BODY_BYTES` / `PIR_SERVER_MAX_REQUEST_BODY_BYTES` | Body caps |
| `PIR_SERVER_MAX_SESSIONS` | Live session cap (default 256) |
| `PIR_SERVER_MAX_SUBSCRIPTIONS` | `pir.session` subscriptions per connection (default 64) |
| `PIR_SERVER_IDLE_TIMEOUT_SECS` | Per-read idle timeout after handshake (default 300) |

Out-of-range or malformed values keep the default rather than failing open.

### Remote execution gates

| Variable | Default | Effect |
|---|---|---|
| `PIR_ALLOW_REMOTE_AGENT` | off | `false`/unset disables remote agent execution |
| `PIR_ALLOW_REMOTE_BASH` | off | Remote bash is off by default |
| `PIR_ALLOW_PARALLEL_SIDE_EFFECTS` | off | Gates parallel side-effecting tool calls |

An authenticated client that cannot execute the agent can still read and write
session state; the gates control *execution*, not authentication.

## Project trust

Project-scoped resources — `.pi/extensions`, `.pi/skills`, project settings —
are **trust-gated**. A project is trusted when any of these holds:

1. `--approve` / `-a` on the command line,
2. `PIR_TRUST_PROJECT=1` in the environment,
3. a recorded decision in `<agentDir>/trust.json`.

`--no-approve` (`-na`) forces the untrusted path for a run. `defaultProjectTrust`
in settings sets the default for projects with no recorded decision. Untrusted
projects skip project-scope discovery; user-scope resources are never gated.

`AGENTS.md` / `CLAUDE.md` loading is separate: it is controlled by
`--no-context-files`, not by trust.

## Filesystem confinement

- `read`, `write`, and `edit` re-establish confinement against open handles so
  a rename between check and use cannot escape the workspace.
- Spill files are owner-only and swept. The outside-workspace exemption for an
  advertised spill path (`Full output: <path>`) is bound to the file's OS
  **identity at creation**, not to its name: re-creating a file at a registered
  spill path does not inherit the exemption. Identity is `(dev, ino)` on Unix
  and the 128-bit `FILE_ID_INFO` id on Windows — the latter because both
  `Metadata::created()` and the older 64-bit file index were measured identical
  after a delete-then-recreate on NTFS. When no identity can be read, the path
  is refused.
- Remote `write`/`edit` are atomic.
- An out-of-loop bash command arms `ShellTreeKill` so a killed command cannot
  leave orphaned children.
- `tools.commandTimeoutMs` (default 120 s) kills the process tree on timeout.
  Set `0` to restore upstream's no-default-timeout behavior — an explicit
  per-call timeout still wins.

## Credentials

Stored at `~/.pi/agent/auth.json` (owner-only). Precedence: stored OAuth > stored
API key > environment. Tokens are never written to session transcripts and never
logged; `pir auth print-api-key` is the only command that prints one.

## Subagents and the process executor

`router=process` runs each task as a child `pir` process. The parent's
provider-scoped credential is passed through `PIR_SUBAGENT_API_KEY` — an
environment variable, never an argv secret, so it does not appear in a process
list. `PIR_SUBAGENT_DEPTH` carries the nesting depth so the global cap stays
effective across the process boundary.

## Extension limits

JS extension resource limits come from the **global** `extensionLimits` plus
`PI_EXT_*`; project settings cannot loosen them. Extension code never runs
during plain `--help` — only `--help-extensions` loads it.

## Telemetry

`enableAnalytics` is **opt-in** (default `false`).
`enableInstallTelemetry` is **on by default** and sends an anonymous version and
update ping after a changelog-detected update; set it to `false` to disable.
Neither sends prompt, transcript, or file content.
