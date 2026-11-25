# RPC mode

`pir rpc` speaks JSON Lines over stdin/stdout: one JSON object per line in,
one per line out. Implementation:
`crates/pir-core/src/coding_agent/rpc.rs`; protocol version 8
(`crates/pir-protocol/src/schemas/mod.rs`, `PROTOCOL_VERSION` — the wire
protocol lives in the standalone `pir-protocol` crate, re-exported at
`crate::protocol`).

The framing, field names and missing/null semantics match upstream pi
(camelCase). `pir server` / `pir client` use the same command surface over
framed CBOR instead of JSON Lines — see [`cli.md`](cli.md).

## Request shape

Every request line is:

```json
{ "id": "<optional correlation id>", "type": "<command>", ... }
```

`id` is echoed on the response; a request without one produces a response with
no `id` field. An unknown `type` is answered with an error and does not end the
loop.

## Responses

```json
{ "type": "response", "command": "<command>", "id": "<id>", "success": true, "data": { ... } }
{ "type": "response", "command": "<command>", "id": "<id>", "success": false, "error": "..." }
```

`type` is always `"response"`, `command` repeats the request's `type`, and
`data` is omitted when the command returns nothing. `prompt` is special: its
acknowledgment is written **before** the run starts, so the response line
precedes the event stream.

## Commands

45 command arms. Grouped:

### Session lifecycle

| Command | Purpose |
|---|---|
| `hello` | version handshake; returns `{ version, protocol: "pir-rpc-jsonl" }` |
| `list` | list sessions |
| `create` | create a session |
| `attach` | attach to a session (subsequent commands act on it) |
| `new_session` | start a fresh session |
| `switch_session` | switch to another session |
| `fork` | fork at an entry |
| `clone` | clone the current session |
| `get_fork_messages` | messages available as fork points |
| `set_session_name` | rename the session |

### Running the agent

| Command | Purpose |
|---|---|
| `prompt` | run a turn (ack precedes the event stream) |
| `steer` | inject steering input |
| `follow_up` | queue a follow-up |
| `abort` | cancel the current run |
| `compact` | force compaction |
| `clear_queue` | drop the queued steering/follow-up input |

### Model and thinking

| Command | Purpose |
|---|---|
| `set_model` / `cycle_model` | select a model |
| `get_available_models` | list selectable models |
| `set_thinking_level` / `cycle_thinking_level` | select a thinking level |
| `get_available_thinking_levels` | list the model's levels |
| `set_steering_mode` / `set_follow_up_mode` | `one-at-a-time` vs `all` |
| `set_auto_compaction` / `set_auto_retry` / `abort_retry` | runtime toggles |

### Shell and background work

| Command | Purpose |
|---|---|
| `bash` / `abort_bash` | run/abort a shell command outside the loop |
| `background_calls` / `abort_background` / `abort_all_background` | inspect and cancel background tool calls |

### Inspection and export

| Command | Purpose |
|---|---|
| `get_state` | current session/agent state |
| `get_session_stats` | token/usage stats |
| `get_messages` | the transcript |
| `get_entries` / `get_tree` | raw entries / session tree |
| `get_last_assistant_text` | the last assistant message |
| `get_commands` | registered slash commands |
| `export_html` | render the session to HTML |

## Extension UI bridge

`extension_ui_response` is not a command: it carries a dialog reply and is
correlated by `id` back to the pending `ctx.ui` promise. Unknown ids, or no
attached extension runtime, are dropped silently (matching upstream).

The host emits `extension_ui_request` frames with `method` values
`select`, `confirm`, `input`, `editor`, `notify`, and `set_editor_text`.

## Events

After `prompt`, the host streams event frames on the same stdout, interleaved
with responses and distinguished by `type`:

- `message_update` — an assistant stream delta: `{ type, turn,
  assistantMessageEvent, usage? }`. The cumulative `partial` mirror is stripped
  from the wire.
- The remaining `agent_event_to_json` / `to_session_event_json` frame shapes
  (message-end, tool start/end, session events) follow upstream's camelCase
  names — including `contentIndex`/`toolCall` on stream events — see
  `crates/pir-core/src/coding_agent/json.rs`.

## Notes

- The facade keeps the historical `pir::coding_agent::rpc` path.
- `abort` cancels the run; the same command surface backs the TUI and the
  server/client transports.
- Extension dialogs need an attached session with an extension runtime; a
  reply with neither is dropped rather than erroring.
