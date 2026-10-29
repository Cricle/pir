# pi-phone browser client protocol (native port wire spec)

This document is the contract between the embedded browser client in this
directory and the native `pir` phone server. It was produced by statically
reading the client; every claim cites `file:line` relative to
`src/plugins/builtin/phone/assets/` (the layout mirrors the upstream
`pi-phone/public/` tree).

## Provenance

- Source: `https://github.com/MaliNamNam/pi-phone`, npm package
  `@malinamnam/pi-phone`, version **0.0.13** (`pi-phone/package.json:2-3`).
- Copied verbatim from the local checkout
  `C:/Users/XPENG_USER/Desktop/github/pi-phone/public/` into
  `src/plugins/builtin/phone/assets/`, preserving relative paths.
- 27 client files, 308,354 bytes. No file was modified. `PROTOCOL.md` itself
  is the only file in this directory that did not come from the source tree.
- License: the upstream package contains **no LICENSE/COPYING file and no
  `license` field in `package.json`**. The only license text shipped is
  `vendor/marked.LICENSE` (MIT, for the vendored `marked` parser:
  `vendor/marked.LICENSE:1-20`). The pi-phone client code itself carries no
  license grant; this port exists because the user owns/requested it, and
  redistribution licensing of that code is the user's responsibility.

### Asset manifest (all copied)

| Path | Bytes | Referenced by |
|---|---:|---|
| `index.html` | 10204 | `/`, `sw.js:3` |
| `app.js` | 24 | `index.html:230`, `sw.js:5` |
| `styles.css` | 49122 | `index.html:23`, `sw.js:4` |
| `sw.js` | 2286 | `transport.js:277` |
| `manifest.webmanifest` | 393 | `index.html:20`, `sw.js:26` |
| `icon.svg` | 757 | `index.html:21-22`, `manifest.webmanifest:12`, `sw.js:27` |
| `vendor/marked.esm.js` | 42042 | `markdown.js:109`, `sw.js:6` |
| `vendor/marked.LICENSE` | 2942 | license text (not fetched at runtime) |
| `app/attachments.js` | 7344 | `index.html:62`, `sw.js:7` |
| `app/autocomplete-controller.js` | 2264 | `index.html:61`, `sw.js:8` |
| `app/autocomplete.js` | 4246 | `index.html:60`, `sw.js:9` |
| `app/bindings.js` | 9716 | `index.html:46`, `sw.js:10` |
| `app/command-catalog.js` | 2454 | `index.html:59`, `sw.js:11` |
| `app/commands.js` | 10909 | `index.html:58`, `sw.js:12` |
| `app/constants.js` | 1870 | `index.html:63`, `sw.js:13` |
| `app/formatters.js` | 4661 | `index.html:52`, `sw.js:14` |
| `app/handlers.js` | 22246 | `index.html:47`, `sw.js:15` |
| `app/main.js` | 263 | `index.html:45`, `sw.js:16` |
| `app/markdown.js` | 5794 | `index.html:53`, `sw.js:17` |
| `app/messages.js` | 48585 | `index.html:50`, `sw.js:18` |
| `app/sheet-actions.js` | 5018 | `index.html:56`, `sw.js:19` |
| `app/sheet-navigation.js` | 993 | `index.html:57`, `sw.js:20` |
| `app/sheets-view.js` | 16874 | `index.html:55`, `sw.js:21` |
| `app/state.js` | 5863 | `index.html:51`, `sw.js:22` |
| `app/tool-rendering.js` | 25162 | `index.html:54`, `sw.js:23` |
| `app/transport.js` | 8903 | `index.html:48`, `sw.js:24` |
| `app/ui.js` | 17419 | `index.html:49`, `sw.js:25` |

No client request points at a missing file. The service-worker precache list
(`sw.js:2-28`) names `/` plus 25 files; all resolve through `assets::lookup`.
`vendor/marked.LICENSE` is not fetched by the browser but lives under the
served tree, so it is embedded and served as `text/plain`.

## 1. Mount path and boot

- `index.html:18` contains the placeholder `window.PI_PHONE_BASE = "/";`.
  The server rewrites this line per request to the reverse-proxy prefix
  (`/phone-<port>`) before serving; direct access keeps `/`.
- Every client URL goes through `BASE_PATH`/`apiUrl()`
  (`app/transport.js:9-13`).
- Static subresources are relative (`styles.css`, `app/*.js`, etc.), so they
  inherit the document's mount path. Query strings are cache-busters
  (`?v=20260323-2` at `index.html:20,23`; `?v=20260323-20` at
  `index.html:230`), never part of the asset identity.
- Boot (`app/main.js:1-6`): initialize DOM bindings, then `boot()`:
  1. `GET /api/health` (`app/transport.js:233-239`, called at `:243`).
  2. If `health.hasToken && !state.token`, open the token modal instead of
     connecting (`app/transport.js:254`).
  3. Otherwise open the WebSocket (`:255`).
  4. Register the service worker at `<base>/sw.js` (`:276-278`).
- Service worker: network-first for navigations and the app shell, cache-first
  otherwise; it never intercepts `/api/*` or `/ws`
  (`sw.js:52-81`, skip rule at `:61`). The shell list is `sw.js:2-28`.

## 2. HTTP surface

### 2.1 Static files

| Request | Expected response |
|---|---|
| `GET /` | `index.html` (200, `text/html`) |
| `GET /index.html` | same |
| `GET /<any embedded path>` | that file, 200 with the MIME type from `assets.rs` |
| `GET /<missing>` | client tolerates a fallback `index.html` (server lane behavior) |

The client issues these exact static requests:

- `GET /` and `GET /sw.js` (`app/transport.js:277`; nav requests hit `/`).
- `GET /styles.css?v=...`, `/manifest.webmanifest?v=...`, `/icon.svg`
  (`index.html:20-23`).
- `GET /app.js?v=...` (`index.html:230`), which imports
  `./app/main.js` (`app.js:1`).
- `GET /app/{main,bindings,handlers,transport,ui,messages,state,formatters,markdown,tool-rendering,sheets-view,sheet-actions,sheet-navigation,commands,command-catalog,autocomplete,autocomplete-controller,attachments,constants}.js`
  via module preload (`index.html:45-63`) and transitive imports.
- `GET /vendor/marked.esm.js` lazily, on the first markdown render
  (`app/markdown.js:109-111`).
- `GET /icon.svg` also via manifest (`manifest.webmanifest:10-17`).

`assets::lookup` strips `?query`/`#fragment`, normalizes leading `/`, maps `/`
to `index.html`, and rejects any `..` segment or backslash/NUL. The server
must apply ETag/304 and caching headers itself; `assets::etag` returns a
quoted strong ETag (`"<16 hex FNV-1a>"`).

### 2.2 `GET /api/health`

Client: `app/transport.js:233-239` (`fetch(apiUrl("/api/health"), { cache: "no-store" })`).
No query, no body. Response JSON becomes both `state.health` and
`state.status` (`:236-237`). Fields read by the client:

| Field | Use |
|---|---|
| `hasToken: boolean` | decides token modal vs. connect (`transport.js:254`) |
| `isStreaming: boolean` | connection/composer state (`ui.js:319,337`; `handlers.js:241`) |
| `cwd: string` | directory display (`ui.js:248,359`) |
| `controlOwner: "cli"\|"phone"` | state line (`ui.js:363-364`) |
| `host: string`, `port: number` | server line (`ui.js:365`) |
| `theme: { name?: string, colors?: { mdCode?, mdCodeBlock?, mdCodeBlockBorder? } }` | CSS variables (`ui.js:25-37,347`; `constants.js:23-27`) |
| `pendingMessages: PendingMessage[]` | queued-chip display (`handlers.js:537-545`) |

The baseline server also returns `isRunning`, `pid`, `sessionCount`,
`idleTimeoutMs`; the client ignores extras. Method handling is server-side;
the client only sends GET.

### 2.3 `GET /api/quota`

Client: `app/transport.js:54-91`. Only called when the snapshot model is
`provider === "openai-codex"` and `id` matches `/^gpt-/i` (`:63`); otherwise
the client clears the quota and makes no request (`:63-67`).

Request:

```
GET <base>/api/quota?provider=openai-codex&modelId=<id>[&force=1]
cache: "no-store"
```

`force=1` is added on explicit refresh (`transport.js:75`). Response JSON:

```jsonc
{
  "visible": true,                       // ui.js:285
  "primaryWindow":   { "label": "...", "text": "...", "leftPercent": 42.0 },
  "secondaryWindow": { "label": "...", "text": "...", "leftPercent": 80.0 }
}
```

Windows are optional (`ui.js:283-309`). A failed request leaves `visible`
false/clears quota (`transport.js:83-88`).

### 2.4 `GET /api/image/<key>`

The client never constructs this path itself. User-message image parts arrive
from the server carrying either `previewUrl` or `url` (rendered as `<img src>`)
or inline `data` + `mimeType` (rendered as a data URL):

```js
// app/messages.js:34-41
if (typeof part.previewUrl === "string" && part.previewUrl) return part.previewUrl;
if (typeof part.url === "string" && part.url) return part.url;
if (typeof part.data === "string" && ...) return `data:${mimeType};base64,${data}`;
```

The server is therefore expected to rewrite snapshot images to
`/api/image/<content-key>` URLs (see the matrix report, `phone-images.ts`
references). Asset lookup plays no part in this route.

### 2.5 WebSocket upgrade

```
GET <base>/ws?token=<token>       (when a token is configured)
Upgrade: websocket
```

Constructed at `app/transport.js:182-186`:

```js
const protocol = location.protocol === "https:" ? "wss" : "ws";
const url = new URL(`${protocol}://${location.host}${apiUrl("/ws")}`);
if (state.token) url.searchParams.set("token", state.token);
```

No `Authorization` header, cookie, or subprotocol is used. Under Tailscale the
path is `/phone-<port>/ws`.

### 2.6 Other API routes (server-side, from the baseline report)

- `GET /api/health` is method-agnostic in the reference server; the client
  only uses GET.
- `POST /api/quota` returns 405 (client only GETs).
- `POST /__pi_phone__/control/stop` always 403 (not called by the client).
- Non-GET/HEAD unknown routes: 405 JSON.
- Unknown static paths fall back to `index.html` with `no-store`.

## 3. Authentication and token placement

- Token storage key: `pi-phone-token` (`app/constants.js:21`); read at startup
  (`app/state.js:48`), written/cleared by `storeToken`/`resetToken`
  (`app/ui.js:14-23`).
- The token is placed **only** in the WebSocket query string
  (`app/transport.js:184`). HTTP API calls carry no token.
- `health.hasToken === true` without a stored token opens the login modal
  (`app/transport.js:254`, modal at `index.html:191-200`).
- Server auth failure is signalled by closing the WS with code **1008**; the
  client clears the stored token, closes, reopens the modal, and shows an
  error banner (`app/transport.js:218-221`; `app/handlers.js:31-37`).
- Other close codes: **4009** = replaced by another client (no reconnect),
  **4010** = idle timeout/stop (no reconnect), **1006** = lost connection with
  reconnect banner (`app/transport.js:210-226`).
- Reconnect policy: foreground 1.2 s, background 10 s
  (`app/transport.js:131-135,165-174`); no reconnect after a manual close
  (`bindings.js:237-241`).

## 4. WebSocket envelopes, client -> server

Every client frame is a JSON object with a `kind` discriminator. There is no
request/response correlation at the transport layer except `requestId` inside
specific payloads.

### 4.1 `refresh`

```jsonc
{ "kind": "refresh" }
```

Sent on socket open (`transport.js:118-122`), on visibility return (`:264-266`),
and by the Refresh button (`bindings.js:90`). The server should answer with a
fresh `snapshot`, `sessions/catalog`, and `server/status`. On open the client
also immediately sends `rpc get_commands` and `rpc get_available_models`
(`transport.js:120-121`).

### 4.2 `load-earlier-messages`

```jsonc
{
  "kind": "load-earlier-messages",
  "before": 120,          // Math.max(0, totalMessageCount - loadedMessageCount)
  "count": 60,            // fixed page size
  "requestId": 1729000000000
}
```

`app/transport.js:94-113` (values at `:108-110`). `before` is an absolute
index into the full transcript. Response is a `snapshot` frame with
`event: "earlier-messages"` (see 5.2).

### 4.3 Session selection / lifecycle

```jsonc
{ "kind": "session-select", "sessionId": "<worker-id>" }   // commands.js:334-346
{ "kind": "session-parent-new" }                           // commands.js:348-361
{ "kind": "session-spawn" }                                // commands.js:363-376
```

`session-select` binds this client to a pool worker; `session-parent-new`
starts a fresh parent session (echoed as an rpc `new_parent_session` response);
`session-spawn` starts a parallel `pir --mode rpc` worker (server announces it
with `server/session-spawned`).

### 4.4 `local-command`

```jsonc
{ "kind": "local-command", "command": <string | object> }
```

Wrapped at `app/transport.js:30-38`. Server-side local commands:

| `command` | Fields | Source |
|---|---|---|
| `"reload"` | string, not an object | `transport.js:40-52` |
| `{"type":"path-suggestions","mode":"mention"\|"cd","query":"...","requestId":N}` | | `autocomplete-controller.js:28-41` |
| `{"type":"cd","args":"<path>"}` | | `commands.js:182-184` |
| `{"type":"slash-command","text":"/name ...","images":[...]?,"streamingBehavior":"steer"\|"followUp"?}` | | `commands.js:50-71` |

Slash-command notes:

- `images` entries are `{ "type": "image", "data": "<base64>", "mimeType": "image/png" }`
  (`attachments.js:220-232`).
- `streamingBehavior` is only added for non-extension commands that can queue:
  `"steer"` for the Steer button, otherwise `"followUp"` while streaming, and
  omitted when idle (`commands.js:56-68`).
- The client refuses images for extension-source commands (`commands.js:51-54`).
- The server answers through the rpc channel with
  `payload.command === "slash_command"` and optional
  `payload.data.source === "extension"` (`handlers.js:190-195`).

### 4.5 `rpc`

```jsonc
{ "kind": "rpc", "command": { "type": "<command>", ... } }
```

Wrapped at `app/transport.js:21-28`. Commands the client sends:

| `type` | Extra fields | Source |
|---|---|---|
| `get_commands` | — | `transport.js:120`, `sheet-navigation.js:12` |
| `get_available_models` | — | `transport.js:121`, `commands.js:201,206`, `sheet-navigation.js:11` |
| `get_state` | — | *not sent by this client; response handler exists* (`handlers.js:97-101`) |
| `get_messages` | — | *not sent by this client; response handler exists* (`handlers.js:104-108`) |
| `get_session_stats` | — | `commands.js:165-168`, `sheet-navigation.js:10`, `sheet-actions.js:33` |
| `abort` | — | `bindings.js:91-92` |
| `new_session` | — | `commands.js:150-152`, `sheet-actions.js:31` |
| `compact` | — | `commands.js:153-156`, `sheet-actions.js:32` |
| `set_thinking_level` | `level`: `"off"\|"minimal"\|"low"\|"medium"\|"high"\|"xhigh"` | `commands.js:185-192`, `sheet-actions.js:58`, `constants.js:1` |
| `set_model` | `provider: string`, `modelId: string` | `commands.js:193-209`, `sheet-actions.js:62` |
| `prompt` | `message: string`, `images?: ImagePart[]`, `streamingBehavior?: "steer"\|"followUp"` | `commands.js:274-300` |
| `extension_ui_response` | `id: string`, plus exactly one of `value`, `confirmed`, `cancelled` | `handlers.js:39-42`, `ui.js:410,419,427,431,441` |
| `phone_cancel_pending_message` | `id: string`, `text: string` | `bindings.js:122-130` |
| `phone_list_sessions` | `includeSubagents: boolean` | `sheet-actions.js:47-54`, `sheet-navigation.js:20` |
| `phone_get_tree` | — | `sheet-navigation.js:22` |
| `switch_session` | `sessionPath: string` | `sheet-actions.js:74` |
| `phone_open_branch_path` | `entryId: string` | `sheet-actions.js:77` |
| `fork` | `entryId: string` | `sheet-actions.js:80` |

Prompt notes: `message` is the raw composer text (may be empty for an
image-only prompt); image `data` is bare base64 (no `data:` prefix)
`attachments.js:166-177,220-232`. When not queueing, the client optimistically
adds the user message locally (`commands.js:302-311`), so the server must not
echo an extra user copy for that frame (it will arrive via the next snapshot
after pi commits it).

`extension_ui_response` answers the request `id`; `select` sends `value`,
`confirm` sends `confirmed`, `input`/`editor` send `value`, Cancel sends
`cancelled: true` (`ui.js:406-445`).

## 5. WebSocket envelopes, server -> client

All frames are JSON with either `{channel, event?, data?, ...}` for
channels/events or `{channel:"rpc", payload:{...}}` for rpc payloads.
The catalog agent (`window`) is not used.

### 5.1 `sessions` / `catalog`

```jsonc
{
  "channel": "sessions",
  "event": "catalog",
  "data": {
    "activeSessionId": "<worker-id>",
    "sessions": [
      {
        "id": "<worker-id>",
        "label": "Display name",
        "sessionId": "<optional session id>",
        "kind": "parent",                 // or a parallel worker kind
        "isStreaming": false,
        "hasPendingUiRequest": false,
        "lastActivityAt": "2026-10-18T...Z",
        "model": { "name": "..." }
      }
    ]
  }
}
```

Consumed at `handlers.js:439-454`; field uses at `sheets-view.js:230-248`
(live row: `label`, `lastActivityAt`, `isStreaming`,
`hasPendingUiRequest`, `model.name`, `kind`) and `:310-323` (sorting).

### 5.2 `snapshot`

Normal snapshot:

```jsonc
{
  "channel": "snapshot",
  "sessionWorkerId": "<worker-id>",       // optional; frames for non-active workers are dropped
  "state": {
    "sessionId": "...",
    "sessionName": "...",
    "model": { "provider": "...", "id": "...", "name": "...", "contextWindow": 200000 },
    "thinkingLevel": "medium",
    "isStreaming": false,
    "isCompacting": false,
    "usageTotals": { "input": 0, "output": 0, "reasoning": 0, "cacheRead": 0, "cacheWrite": 0 },
    "contextUsage": { "tokens": 0, "contextWindow": 200000, "percent": 0.0 }
  },
  "messages": [ /* pi transcript messages, roles below */ ],
  "historyTruncated": true,
  "totalMessageCount": 240,
  "commands": [ { "name": "...", "description": "...", "source": "extension|prompt|skill" } ],
  "liveAssistantMessage": { "role": "assistant", "content": [...], "timestamp": 1729000000000 },
  "liveTools": [
    {
      "toolCallId": "...",
      "toolName": "bash",
      "args": { },
      "partialResult": { "content": [...], "details": { } },
      "result": { "content": [...], "details": { } },
      "isError": false,
      "durationMs": 1234
    }
  ]
}
```

Consumed at `handlers.js:456-534`:

- Drop rule: if `sessionWorkerId` is present and differs from
  `state.activeSessionId`, the frame is ignored (`:457-459`).
- `commands` may be omitted when unchanged; an explicit array replaces the
  catalogue (`:488-490`).
- `historyTruncated`/`totalMessageCount` drive the "Load earlier" button
  (`:483-487`).
- `liveAssistantMessage` and `liveTools` restore in-flight state after a
  reconnect (`:493-526`).

Earlier-messages page:

```jsonc
{
  "channel": "snapshot",
  "sessionWorkerId": "<worker-id>",
  "event": "earlier-messages",
  "messages": [ /* older pi messages */ ],
  "startIndex": 60,
  "hasMore": true,
  "totalMessageCount": 240
}
```

Consumed at `handlers.js:461-474`; `hasMore !== false` means truncated, and
the page is prepended (`messages.js:1075-1090`).

### 5.3 `server` events

| `event` | Shape | Client action | Citation |
|---|---|---|---|
| `status` | `data`: full status object (same fields as health, incl. `isStreaming`, `pendingMessages`, `cwd`, `host`, `port`, `controlOwner`, `theme`) | header/composer/quota | `handlers.js:536-547`; `ui.js:334-368` |
| `queue-update` | `data: { steering: string[], followUp: string[], mode: "replace"\|"delivery" }` | merge pending chips | `handlers.js:548-584` |
| `stderr` | `data: { text: string }` | error banner | `handlers.js:585-588` |
| `reloading` | `data: { message?: string }` | banner | `handlers.js:589-592` |
| `session-spawned` | `data: { message?: string }` | toast | `handlers.js:593-596` |
| `single-client-replaced` | `data: { message?: string }` | banner | `handlers.js:597-600` |
| `idle-timeout` | `data: { message?: string }` | banner | `handlers.js:601-604` |
| `startup-error` | `data: { message?: string }` | error toast | `handlers.js:605-608` |
| `snapshot-error` | `data: { message?: string }` | error toast | `handlers.js:605-608` |
| `client-error` | `data: { message?: string }` | error toast | `handlers.js:605-608` |
| `agent-exit` | `data: { message?: string }` | error banner | `handlers.js:609-612` |

`PendingMessage` shape: `{ id, text, kind: "steer"|"followUp", imageCount?: number }`
(`messages.js:373-391`; ids are also used for cancellation at
`bindings.js:122-130`).

### 5.4 `rpc` payloads

Envelope: `{ "channel": "rpc", "payload": { "type": ... } }`
(`handlers.js:615-617`).

#### Response

```jsonc
{
  "channel": "rpc",
  "payload": {
    "type": "response",
    "command": "<request type>",
    "success": true,
    "data": { },             // command-specific, see table
    "error": "message"       // when success is false
  }
}
```

Handler: `handlers.js:84-238`. Command-specific `data` the client reads:

| `command` | `data` shape | Citation |
|---|---|---|
| `get_state` | state object (as in 5.2) | `handlers.js:97-101` |
| `get_messages` | `{ messages: [...] }` | `handlers.js:104-108` |
| `get_commands` | `{ commands: [{ name, description?, source? }] }`; sources `local`, `extension`, `prompt`, `skill` | `handlers.js:111-115`; `constants.js:19`; `sheets-view.js:160-163` |
| `path_suggestions` | `{ requestId, mode, query, suggestions: [{ value, isDirectory, description?, kind?: "previous" }] }` | `handlers.js:118-135` |
| `cd` | `{ cwd: string }` | `handlers.js:138-141` |
| `get_available_models` | `{ models: [{ provider, id, name? }] }` | `handlers.js:144-147`; `sheets-view.js:126-131` |
| `get_session_stats` | `{ tokens: { input, cacheRead, cacheWrite, output, reasoning }, toolCalls, totalMessages, cost }` | `handlers.js:150-154`; `sheets-view.js:6-32` |
| `phone_list_sessions` | `{ sessions: [{ id, name, firstMessage, path, modified, isSubagent, sizeBytes? }], subagentCount }` | `handlers.js:156-162`; `sheets-view.js:192-208,250-260` |
| `phone_get_tree` | `{ sessionFile, currentLeafId, currentPathIds: [], nodes: [{ id, depth, timestamp, label?, summary: { kind, role?, preview? } }] }` | `handlers.js:164-168`; `sheets-view.js:326-358` |
| `new_parent_session` | any (client only refreshes) | `handlers.js:170-175` |
| `new_session` | any | `handlers.js:177-182` |
| `compact` | any | `handlers.js:184-188` |
| `slash_command` | `{ source?: "extension" }` | `handlers.js:190-195` |
| `reload` | any | `handlers.js:197-202` |
| `set_model` | any | `handlers.js:204-208` |
| `set_thinking_level` | any | `handlers.js:210-214` |
| `switch_session` | any | `handlers.js:216-221` |
| `fork` | any | `handlers.js:223-228` |
| `phone_open_branch_path` | any | `handlers.js:230-235` |

A failed response shows `error` as a toast (or clears path autocomplete when
`command === "path_suggestions"`) (`handlers.js:88-95`).

#### Streaming/agent events

| `payload.type` | Fields | Citation |
|---|---|---|
| `agent_start` | — | `handlers.js:240-249` |
| `agent_end` | — | `handlers.js:251-260` |
| `message_update` | `assistantMessageEvent: { type: "text_delta"\|"thinking_delta", delta } \| { type: "toolcall_end", toolCall: { id, name, arguments } } \| { type: "error", message }`, `message: { role, timestamp }` | `handlers.js:262-267`; `messages.js:1016-1046` |
| `message_end` | `message`: assistant message (`role`, `timestamp`, `content`, `model`, `usage`, `stopReason`) | `handlers.js:270-282`; `messages.js:104-121` |
| `tool_execution_start` | `toolCallId`, `toolName`, `args` | `handlers.js:284-301` |
| `tool_execution_update` | `toolCallId`, `toolName`, `args`, `partialResult: { content, details }` | `handlers.js:303-322` |
| `tool_execution_end` | `toolCallId`, `toolName`, `args`, `result: { content, details }`, `isError`, `durationMs` | `handlers.js:324-345` |
| `extension_ui_request` | `id`, `method`, `title?`, `message?`, `options?: string[]`, `placeholder?`, `prefill?`, `statusText?`, `widgetKey?`, `widgetLines?: string[]`, `text?` | `handlers.js:44-82`; `ui.js:397-451` |
| `compaction_start` | — | `handlers.js:352-359` |
| `compaction_end` | `errorMessage?`, `aborted?` | `handlers.js:361-374` |
| `queue_update` | `steering: string[]`, `followUp: string[]`, `mode?: "delivery"` (any other value = replace) | `handlers.js:376-413` |
| `auto_retry_start` | `errorMessage?` | `handlers.js:415-418` |
| `auto_retry_end` | `success: boolean`, `finalError?` | `handlers.js:420-422` |

`extension_ui_request.method` handling: `notify` (toast), `setStatus`
(`statusText`), `setWidget` (`widgetKey`, `widgetLines`), `setTitle` (`title`),
`set_editor_text` (`text`), and the modal methods `select` (`options`),
`confirm`, `input`, `editor` (`placeholder`, `prefill`) — `handlers.js:44-82`,
`ui.js:397-451`. Any other method produces an "Unsupported UI request" toast
(`handlers.js:76-79`).

### 5.5 Transcript message shapes

Snapshots carry pi's own messages; the client renders these roles:
`user` (content parts `text`/`image`), `assistant` (parts `text`, `thinking`,
`toolCall { id, name, arguments }`), `toolResult` (`toolCallId`, `toolName`,
`content`, `details`, `isError`, `durationMs`), `bashExecution` (`command`,
`output`, `exitCode`, `truncated`, `fullOutputPath`, `cancelled`), `custom`
(`customType`, `display`, `content`, `details`; `phone-inline-user-message` is
rendered as a user message), `branchSummary` (`summary`, `fromId`),
`compactionSummary` (`summary`, `tokensBefore`) — `messages.js:90-208`.

## 6. Server-lane checklist derived from this spec

1. Serve `assets::lookup` for GET/HEAD; `/` -> `index.html`; ignore query
   strings; return 304 on matching ETag; gzip is optional but expected for
   text assets.
2. Rewrite `window.PI_PHONE_BASE = "/";` (`index.html:18`) to the mount prefix
   before sending HTML.
3. `GET /api/health` returns at least `hasToken`, `isStreaming`, `cwd`,
   `controlOwner`, `host`, `port`, `theme`, `pendingMessages`.
4. `GET /api/quota` parses `provider`, `modelId`, `force`; non-GPT/codex
   models should return `{visible:false}` without upstream calls.
5. Upgrade `/ws` (and `/phone-<port>/ws`); require `?token=` when configured;
   close with 1008 on mismatch, 4009 when replacing a client, 4010 on idle.
6. On connect/refresh send `server/status`, `sessions/catalog`, `snapshot`;
   answer `rpc` with `response` frames and stream the repo event payloads
   above.
7. Accept all four `local-command` forms, including the bare `"reload"`
   string.
8. Snapshot images must be rewritten to `previewUrl`/`url` values (normally
   `/api/image/<key>`); raw base64 is also acceptable but heavy.
