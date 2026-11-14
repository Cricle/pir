# pi-phone central server

`pir-phone-server` is the central **information proxy** for pi-phone. It does
not execute the agent, run tools, or read user files. Agent sessions stay in the
user's own `pir` process; the server authenticates users/devices, stores session
metadata, relays commands/events, and fans out one session to many phones.

```text
user machine A ── pir + pi-phone plugin ── WSS /agent ─┐
user machine B ── pir + pi-phone plugin ── WSS /agent ─┤   pir-phone-server
                                                       ├── users/devices/sessions
phone PWA ─────────────── WSS /ws ─────────────────────┘   static PWA
```

The relay stores only account/device/session **metadata** (and a small latest
status line for reconnect). Transcripts, snapshots, tool output and images are
forwarded and then dropped; the server never persists session content.

## Quick start

```bash
# 1. Initialize the data directory (creates the database and a secret).
pir-phone-server init --data-dir /var/lib/pir-phone

# 2. Create a user and a login token. The token is printed once.
pir-phone-server user create alice --data-dir /var/lib/pir-phone
pir-phone-server token create --user alice --kind user --name phone \
  --data-dir /var/lib/pir-phone

# 3. Start the server.
pir-phone-server serve --data-dir /var/lib/pir-phone \
  --listen 0.0.0.0:8787 --public-url https://phone.example.com
```

Open the server URL in a phone browser, enter the server URL and the user
token, and connect. For a device, either create a device in the PWA/API or
create a device token:

```bash
pir-phone-server token create --user alice --kind device --name MacBook \
  --data-dir /var/lib/pir-phone
```

On the user's machine:

```text
/phone-start wss://phone.example.com <device-token> --device-name MacBook
```

The builtin also registers `/phone-stop` (disconnect and stop the agent-side
client), `/phone-status` (one-line connection state) and `/phone-token`
(show/persist the device token); all four are native Rust commands, so they
run in the TUI and print mode, not through the JS runtime.

If the token is bound to a device created via the API/PWA, pass
`--device-id <device-id>` as well. An unbound device token lets the agent
register its own stable device id on first hello.

The agent opens an outbound WebSocket to `/agent`; the server never dials the
user's machine.

## User and device model

- `users` — accounts; a user owns devices, tokens, and session metadata.
- `tokens` — `user` tokens for the PWA, `device` tokens for `pir` agents. Only
  `SHA-256(token || pepper)` is stored.
- `devices` — one per `pir` installation. The plugin generates a stable
  `device_id` in `<agent-dir>/phone-device.json`.
- `sessions` — metadata only: catalog id `deviceId:localSessionId`, label,
  kind, cwd, model, thinking level, streaming/compacting flags, message count,
  online state. Transcripts stay in the user's local session JSONL.

A user can have many devices; a device can register many sessions. The phone
plugin registers the live parent session and, when the phone session pool is
enabled, each `active-session-N` child as a parallel catalog row. Selecting a
child updates the same catalog subscription and routes commands to that child.
The pool is capped at 16 live children per device.

## PWA layout

The phone client is a responsive app shell: a persistent sidebar on desktop
and a drawer on mobile. The sidebar shows the signed-in user, single-line live
session rows, and registered devices. Settings, devices, session controls,
models, thinking, saved sessions, and stats open as center-column pages rather
than modal sheets. Light, dark and follow-system appearance are selectable;
thinking output can be hidden from Settings.

## Capabilities, custom events and extension UI

The agent hello and `server/status` advertise `sessions`, `tools`,
`extension-ui.v2`, `custom-events.v1`, and `session-pool`. The PWA keeps the
same protocol version and ignores unknown frames.

Custom plugin events are opt-in and namespaced. A plugin must be listed in
`PI_PHONE_CUSTOM_EVENTS`; only names beginning with `phone.custom.` are
forwarded, each frame is capped at 64 KiB, and the PWA re-emits accepted frames
as a DOM `pi-phone-custom` event (`state.customEvents` also keeps the last 100).

`extension_ui_request` covers `notify`, `select`, `confirm`, `input`, `editor`,
`setStatus`, `setTitle`, `setWidget`, `setWorkingMessage`, `setWorkingVisible`,
`setWorkingIndicator`, `setHiddenThinkingLabel`, `setEditorText`, `setTheme`,
`setFooter`, and `setHeader`. Methods without a visual equivalent are
acknowledged and ignored by the compact web shell.

## HTTP API

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/` `/assets/*` | none | PWA static files |
| GET | `/api/health` | optional | version, uptime, counts, cache stats, RSS/VmHWM |
| GET | `/api/metrics` | none | process memory, registry state and relay counters |
| GET | `/api/version` | none | protocol version |
| GET | `/api/me` | user | current user |
| GET | `/api/devices?cursor=&offset=&limit=` | user | user's devices; limit default 50 / max 200, `nextCursor`/`hasMore` |
| POST | `/api/devices` | user | `{name, platform}` → device + device token (shown once) |
| DELETE | `/api/devices/:id` | user | revoke a device/tokens and close its live agent socket immediately; idempotent for already-revoked owned ids |
| GET | `/api/sessions?cursor=&limit=&q=&deviceId=` | user | paginated session catalog |
| GET | `/api/sessions/:id` | user | session metadata |
| GET | `/api/sessions/:id/messages?before=&limit=` | user | earlier messages via the agent's `get_messages` RPC (limit default 60 / max 200) |
| GET | `/api/models?sessionId=` | user | the session's models via RPC; falls back to the first online session; `agent-offline` when none |
| GET | `/api/quota?sessionId=` | user | quota via the `get_quota` RPC; `{ "visible": false }` when the agent is offline |
| DELETE | `/api/sessions/:id` | user | soft-delete catalog metadata (local file untouched) |
| POST | `/api/auth/ws-ticket` | user | 30-second one-use WebSocket ticket |

Authenticated requests use `Authorization: Bearer <user-token>`.

## WebSocket protocols

### Phone PWA — `/ws`

Authenticate with the `Sec-WebSocket-Protocol` header:

```text
Sec-WebSocket-Protocol: pi-phone.v1, bearer.<base64url(user-token)>
```

Alternatively, obtain a one-use ticket from `POST /api/auth/ws-ticket` and
connect with `?ticket=...`.

The server sends boot `server/status` and `sessions/catalog` frames (the
relay does not cache transcripts, so `snapshot` follows a `session-select`
resync, not connect). Client messages are the existing phone envelopes:
`sessions/list`, `session-select`, `refresh`, `load-earlier-messages`, `rpc`,
`local-command`, `ping`. Session frames are the existing `snapshot`/`rpc`
shapes plus:

- `sessions/selected`
- `server/device-offline`
- `server/session-gone`
- `server/ping` / `server/pong`
- `server/error` (`{code,message}`; e.g. `no-session`, `session-not-found`,
  `unknown-message`, `invalid-json`, `agent-offline`)

`server/resync-required` is documented for the standalone server but the
central relay has no sender for it; treat it as not implemented.

Catalog rows always carry `sessionId`, `hasPendingUiRequest`, and
`lastActivityAt` (epoch ms). The last one falls back to the relay
`updatedAt`; `hasPendingUiRequest` defaults to `false` when the agent does
not report it (native parent rows currently report `false`, pool children
report their real value). Boot `server/status` includes `host`, `port`, and
an empty `pendingMessages`; `theme` is optional and not sent by the relay.
`controlOwner` is `"server"` in relay frames and `"cli"`/`"phone"` in agent
status frames.

Multiple phones may subscribe to the same session.

### Agent — `/agent`

Authenticate with `Authorization: Bearer <device-token>` and send a hello:

```json
{
  "type": "hello",
  "version": 1,
  "deviceId": "dev-uuid",
  "deviceName": "MacBook-Pro",
  "platform": "darwin-arm64",
  "sessions": [
    {
      "id": "parent-session",
      "label": "pir · ~/work",
      "kind": "parent",
      "cwd": "/Users/me/work",
      "model": { "provider": "anthropic", "id": "claude-sonnet-4" },
      "thinkingLevel": "medium",
      "isStreaming": false,
      "messageCount": 42
    }
  ]
}
```

Server → agent:

```json
{ "type": "command", "sessionId": "parent-session", "forwardId": "f-42", "line": "{...}" }
{ "type": "resync", "sessionId": "parent-session", "reason": "client-joined" }
{ "type": "ping" }
```

Agent → server frames wrap the existing phone envelope:

```json
{ "type": "frame", "sessionId": "parent-session", "payload": { "channel": "rpc", "payload": { ... } } }
```

The server rewrites client request ids to a unique `forwardId`, forwards the
command, and restores the original id when routing the agent's response back to
the originating phone. Exception: `extension_ui_response` keeps its `id` (it
addresses the pending dialog inside the session); the relay correlates that
command through the envelope-level `requestId` instead. Non-response frames are
fanned out to every subscriber of that session. Metadata changes are sent with
`session-upsert` / `session-remove`.

## Configuration

| Variable / flag | Default | Purpose |
|---|---|---|
| `--data-dir` | `./pir-phone-data` | database, secret, and future sidecars |
| `--listen` | `127.0.0.1:8787` | HTTP/WS bind address |
| `--public-url` | none | canonical URL shown in the PWA/health |
| `--allowed-origin` | same-origin | extra browser origins for CORS and WebSocket Origin checks (repeatable) |
| `PIR_PHONE_SERVER_PEPPER` | `<data-dir>/secret` | token-hash pepper (rotate by reissuing tokens) |
| `PIR_PHONE_SERVER_MAX_RESIDENT_SESSIONS` | `100000` | high-water resident session runtimes; `0` disables eviction. Only offline, unsubscribed sessions are evicted; online sessions stay resident and catalog pages hydrate from redb |
| `PIR_PHONE_SERVER_URL` | none | plugin default server URL |
| `PIR_PHONE_DEVICE_TOKEN` | none | plugin device token |
| `PIR_PHONE_DEVICE_NAME` | hostname | plugin device name |
| `PIR_PHONE_DEVICE_ID` | generated | plugin device id |
| `PI_PHONE_CUSTOM_EVENTS` | none | comma-separated opt-in `phone.custom.*` event names; max 64 KiB each |
| `PI_PHONE_SESSION_POOL_EXECUTABLE` | current `pir` binary | override the child binary used by the phone session pool |

Every session/device/agent route requires a user or device token (only the PWA shell and `/api/health`,
`/api/metrics`, `/api/version` are anonymous). Use TLS for any public
deployment; either put it behind Caddy/Nginx/Tailscale or terminate TLS in the
process in a future revision.

## Deployment

Caddy example:

```caddy
phone.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

systemd unit:

```ini
[Unit]
Description=pir-phone-server
After=network.target

[Service]
User=pir-phone
ExecStart=/usr/local/bin/pir-phone-server serve --data-dir /var/lib/pir-phone \
  --listen 127.0.0.1:8787 --public-url https://phone.example.com
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

redb backup: stop the service or use a filesystem snapshot; the data
directory contains `phone.redb` and `secret`. Keep the secret stable — changing
it invalidates every issued token.

## Security notes

- The server is a relay only: no agent, tool, shell, or filesystem execution.
- Tokens are high-entropy opaque strings; only hashes are stored.
- PWA WebSocket auth uses a subprotocol header, not a query parameter.
- Cross-origin browser requests are rejected unless explicitly allowed.
- Per-user device/session quotas and per-client bounded queues are enforced.
- Cross-user session ids return 404; no global session broadcast.
- PWA CSP should be set by the reverse proxy in production; the built-in shell
  already escapes markdown and raw HTML.

## Development

```bash
bash scripts/phone_server_check.sh

# Mock agent (no AgentSession)
cargo run -p pir-phone-server --example mock_agent -- \
  --server ws://127.0.0.1:8787 --token <device-token>

# Real AgentSession + phone plugin + relay + PWA, with a scripted local model
cargo run --example phone_relay_demo --features phone
```

`phone_relay_demo` prints the server URL and a user token; open the URL,
log in, and send a prompt through the same PWA used in production.

### Repeatable benchmark

```bash
bash scripts/phone_server_bench.sh --iterations 5
```

The harness uses fixed counts and deterministic IDs; it prints a Markdown
table plus one `BENCH_JSON` line per scenario. Current release-profile
reference results on the development machine:

| scenario | p50 | p95 |
|---|---:|---:|
| 10k sessions, first 50-row catalog page | **0.04 ms** | **2.61 ms** |
| 50 phones × 100 live events | **47.6 ms** | **48.0 ms** |
| 400 KiB snapshot delivery | **1.52 ms** | **2.15 ms** |
| client reconnect + agent resync | **3.45 ms** | **4.05 ms** |

The dedicated load/resource harness adds 100-client fan-out, 10k catalog
percentiles, RSS/VmHWM and CPU:

```bash
bash scripts/phone_server_load.sh \
  --sessions 10000 --clients 100 --events 500 --iterations 20
```

Latest reference: 10k catalog p50/p95/p99 **0.40/0.64/0.85 ms**,
100×500 fan-out **50,000 msgs in 56.2 ms (~889k msgs/s)**, RSS
**3.3 → 27.7 → 53.3 MiB** (start → seeded → after fan-out). A 1M-session
metadata catalog runs at **617 MiB peak RSS**. See
[`phone-server-performance.md`](phone-server-performance.md) for methodology
and caveats.

Use the release profile for comparable numbers across machines. The stress
scenarios live in `crates/pi-phone-server/tests/stress_scenarios.rs` and are
run by `scripts/phone_server_check.sh`.

The `mock_agent` example registers one session and answers prompts with a
canned reply, which is useful for exercising the PWA without a real agent.

## Current limitations

- The server stores no session content: only metadata plus the latest small
  status line. Reconnecting clients ask the agent for a fresh snapshot. Offline
  sessions show metadata only.
- One node is targeted first. The protocol reserves room for a future
  `nodeId`/shared-bus layer, but no cross-node routing is implemented.
- Team sharing, billing, and a web admin console are out of scope for this
  revision; users/tokens are managed with the CLI.
- `/api/quota` proxies the local agent's `get_quota` RPC; it returns
  `visible: false` when the agent is offline or the provider has no quota
  surface.
- A `pir` process registers its live parent session plus up to 16
  `SessionPool` children; arbitrary subagent processes are not catalog rows.
- The legacy local HTTP server, embedded assets, Tailscale Serve management,
  and runtime-state files were removed; the plugin is now remote-only.
