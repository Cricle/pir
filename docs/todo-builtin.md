# `todo` builtin — the `@juicesharp/rpiv-todo` port

`builtin:todo` is the native Rust port of the npm package **`@juicesharp/rpiv-todo` v2.12.0** (installed reference:
`~/.pi/agent/npm/node_modules/@juicesharp/rpiv-todo`). It ships the model-facing `todo` tool, the `/todos` slash
command, and transcript-backed task-list persistence.

- Implementation: `src/plugins/builtin/todo.rs` (feature `todo`); entry `src/plugins/builtin.rs` (`name: "todo"`,
  `replaceable: true`).
- Upstream contract truth: `docs/tool-schema.md`; this document records only pir's semantics and deviations. Tests:
  module unit tests; `tests/suites/todo_builtin.rs` (registration, `/todos`, persistence, CPU invariants);
  `tests/suites/sdk.rs` (`.builtin("todo")`, session-file round trip).

## 1. Tool contract

One tool, `todo`, with six actions — `create`, `update`, `list`, `get`, `delete`, `clear` — and the upstream
parameter set (`subject`, `description`, `activeForm`, `status`, `blockedBy`, `addBlockedBy`, `removeBlockedBy`,
`owner`, `metadata`, `id`, `includeDeleted`). `todo_parameters_schema()` generates the JSON Schema with the same
properties, enums and descriptions as TypeBox (LLM-facing copy verbatim; only key order differs — `serde_json`
sorts maps).

Status machine (a same→same transition is an accepted no-op):

| From | Allowed targets |
| --- | --- |
| `pending` | `in_progress`, `completed`, `deleted` |
| `in_progress` | `pending`, `completed`, `deleted` |
| `completed` | `deleted` |
| `deleted` | _(terminal)_ |

`delete` tombstones (the task stays so historic `blockedBy` edges resolve); `clear` drops everything and resets the
id counter to 1. Dependencies are validated before any mutation (dangling, tombstoned, self-block, cycle);
`addBlockedBy`/`removeBlockedBy` are additive; `metadata` merges key by key, `null` deletes a key, and an emptied
record drops the field.

The response envelope is upstream's:

```json
{
  "content": [{ "type": "text", "text": "..." }],
  "details": { "action": "...", "params": {...}, "tasks": [...], "nextId": 3, "error": "..." }
}
```

`details` is the persistence format: every call (including rejections) embeds the complete post-mutation snapshot.
Rejections are **in-band**: content is `Error: <message>`, `details.error` is the bare message, `is_error` stays
`false` (upstream parity), state untouched.

Error strings are upstream-verbatim for every case in `docs/tool-schema.md` (`subject required for create`,
`blockedBy: #N not found|is deleted`, `id required for <action>`, `#N not found`, the long `update requires at
least one mutable field: …`, `illegal transition <from> → <to>`, `cannot block #N on itself`, `addBlockedBy:
…`, `#N is already deleted`); the bounded-record guards add new ones (§5).

## 2. Persistence semantics

### Where the state comes from

1. **Live**: `TodoTool::run` applies the mutation to the active session slot and commits (upstream's `execute() →
   commitState` seam). `tool_call`/`tool_result` are deliberately **not** the live source: pir's agent loop
   dispatches them only to the JS runtime handle (`agent::tool_execution::{gate_tool_calls, emit_tool_results}`).
   `tool_result` stays a **defensive second source** (a matching `todo` envelope is an idempotent whole-state
   REPLACE for hosts that dispatch directly) but the plugin does not declare interest, avoiding the per-batch
   hosted result fan-out; `tool_call` is not consumed (it fires before execution, input only).
2. **Replay**: `AgentPlugin::on_session_context`. The host pushes a `SessionContextSnapshot` before every prompt
   and after every run; when it carries `entries` (serialized `Vec<Message>`) and a `session_id`, the plugin
   rebuilds the slot. The filter is upstream's: the **last** entry with `role == "toolResult"`, `toolName ==
   "todo"`, and a `details` object shaped like `{tasks: [...], nextId: <number>}` wins (last-write-wins,
   **replace**, never merge). `details.tasks[].status` etc. deserialize into the same `Task` shape the envelope
   wrote, so a snapshot round-trips losslessly; extra fields are ignored, so a session produced by the JS package
   replays into the builtin — switching from `pi install npm:@juicesharp/rpiv-todo` to `builtin:todo` keeps the
   list.

### Trigger points and cost

Replay runs only when it can change something:

| Situation | Work |
| --- | --- |
| `snapshot.entries` is not an array (model-only host, e.g. an SDK session with inline plugins and no JS runtime) | nothing; live state stands |
| entries fingerprint == the slot's cached fingerprint | one cheap filtering + structural-hash pass, **zero deserializations**, no state write |
| fingerprint moved (result appended, dropped, edited, truncated) | one cheap pass **plus exactly one** deserialization of the last matching snapshot and one state replacement |
| `session_start` / `on_reload` | replay cache invalidated; next push is authoritative |
| `on_shutdown` | every slot is dropped |

The fingerprint is FNV-1a over `(entries.len(), toolCallId, timestamp,` a structural hash of `details`) of every
matching `todo` result. It moves on append, truncation, branch switches and any edit to a result's snapshot
content, and stays put for rewrites that cannot affect the task list.

Only when the fingerprint moved is the transcript's **last** matching snapshot deserialized. If its `details` pass
the shape check but fail task deserialization, the scan walks back to the previous shape-matching result (still one
deserialization for a well-formed transcript).

### Idempotence and boundaries

- Replaying the same transcript twice is a no-op the second time; replaying one containing the newest snapshot
  reproduces live state exactly, including `nextId` (ids never restart or double up).
- **Compaction** truncates the transcript: the newest surviving snapshot wins (a cut dropping every result resets
  to empty, `nextId: 1`). Every successful call re-embeds the full list, so tasks survive any cut keeping its
  result.
- `/resume` / `/reload` / a session replacement build a fresh host; the first entries push restores the list. State
  is keyed by `session_id` (LRU, 8 slots).
- A push without `entries` (no JS runtime) cannot replay; live state and the persisted transcript remain. The SDK's
  inline `.builtin("todo")` takes this branch.

## 3. `/todos`

`/todos` prints the whole list grouped by status, independent of any row budget:

```
2/7 completed · 1 in progress · 4 pending
── Pending ──
  ○ #4 Register DI bindings
  ○ #5 Add integration tests    ⛓ #4
── In Progress ──
  ◐ #3 Create DemoTodoRepository (creating the repository)
── Completed ──
  ✓ #1 Create DemoTodo domain entity
```

Header counts omit zero values; sections appear only when non-empty; tombstones are never listed. Empty list: `No
todos yet. Ask the agent to add some!`. No UI: `/todos requires interactive mode` (`ok = false`).

**Text channel.** A Rust command's only host-visible text output is `CommandOutcome.error` (the `mcp` builtin's
precedent), so the grouped list rides that field with `ok = true`. Print mode prints it (`[ext] command:todos:
…`); the TUI renders extension-command text only for failed outcomes and does not list Rust builtin commands, so
`/todos` is not surfaced there yet (same gap `/mcp` has).

## 4. Feature control

- `todo = []` in `Cargo.toml`, part of `full`. Gated: the module, the factory entry, and the `BUILTIN_FEATURES`
  diagnostic row.
- Trimmed builds report `builtin:todo` honestly: *"Built-in extension `builtin:todo` is not compiled into this
  build (the `todo` feature is off)"*; a genuinely unknown name keeps upstream's *"Unknown built-in extension:
  builtin:todo"* wording.
- Replaceable: a user extension registering a `todo` tool or `todos` command omits the builtin. Disable explicitly
  with `disabledBuiltinExtensions: ["todo"]`.
- The builtin registers exactly: the `todo` tool, the `todos` command, and the `ctrl+shift+t` collapse definition.
  It claims no flags.

## 5. Memory and CPU

| Aspect | Bound |
| --- | --- |
| State per session | O(tasks); `TodoState { tasks, next_id }` and nothing derived |
| Records | `MAX_TASKS_PER_SESSION` = 1000 tasks; `MAX_METADATA_BYTES` = 16 KiB per task; `MAX_SUBJECT_CHARS` = 1024; `MAX_DESCRIPTION_CHARS` = 8192; `MAX_ACTIVE_FORM_CHARS` = 256; `MAX_OWNER_CHARS` = 256 |
| Sessions map | LRU, `MAX_SESSION_SLOTS` = 8; cleared on `on_shutdown` |
| Replay | one cheap pass per push: O(entries) filtering + allocation-free structural hash of matching `details`, **zero** deserializations when the fingerprint is unchanged; when moved, exactly one O(tasks) deserialization (the last matching one). Pushes are ≤ 2 per run, never per delta |
| Reducer | in place after validation; only the updated task is cloned for the no-op comparison; `detect_cycle` is an iterative O(V+E) DFS (no recursion, stack-safe at the task cap) |
| Sanitizer | single O(chars) pass, no regex |
| Events | `events()` = `["session_start"]`: `message_update` and the per-batch `tool_result` fan-out never reach the plugin through gated loop paths (host `has_handlers`/`has_native_handlers` gate). A direct `PluginHost::emit("tool_result", …)` still reaches the defensive branch, where only a matching `todo` envelope replaces state |

Pinned by `tests/suites/todo_builtin.rs` and module unit tests.

## 6. Deviations from upstream (each with a reason)

| # | Upstream (`rpiv-todo` 2.12.0) | pir | Reason |
| --- | --- | --- | --- |
| 1 | Replays on `session_start` / `session_compact` / `session_tree` by walking the branch; re-walks on every lifecycle event | Replays on the host's `on_session_context` push (entries + session id), fingerprint-gated; one pass per push, skipped when unchanged | pir pushes the transcript at those points (plus `/resume`/`/reload`); one memory seam, no branch API; one scan per run instead of two, no rollback by an identical push |
| 3 | Live state commits in `execute()`; a `tool_execution_end` handler repaints the overlay | Same commit seam; a `tool_result` envelope is accepted as an idempotent second source (replace) but **not declared**; `tool_call` is not consumed | pir dispatches `tool_call`/`tool_result` only to JS, so the live commit is the only source that always fires; declaring `tool_result` would clone every payload for published state |
| 4 | Module-level `sessions` Map + `activeRenderSession` pointer | Per-plugin-instance store keyed by session id, LRU-capped | One plugin instance per session host; the cap bounds a reused instance |
| 5 | Unbounded tasks/metadata/text | `MAX_TASKS_PER_SESSION`, `MAX_METADATA_BYTES`, per-field char limits with new in-band error strings | Bounded memory is required; the transcript is host-bounded, plugin state must be too |
| 6 | TypeBox rejects malformed shapes; `metadata: null` throws; `{"description": null}` stores `null` | The parser reports `invalid params: …`; a `null` optional field means "absent" (no mutation), never a stored `null` | Schema-impossible inputs get an in-band error; `null` as "unset" is Rust's `Option` |
| 7 | Immutable reducer (new arrays per mutation) | In-place reducer, validation first | Same observable contract (rejections never mutate), O(1) per mutation instead of O(tasks) |
| 8 | No execution-mode knobs | `execution_mode: sequential`, `supports_background: false` | Two `todo` calls in one batch must not interleave read-modify-write; id assignment follows call order; a backgrounded result would land after its turn |
| 9 | `details`/`params` key insertion order | `serde_json` map order (sorted) | Semantically identical JSON; consumers look up keys |
| 10 | `id` is a JS number; `-1` reaches the reducer and reports `#-1 not found` | Ids are `u64`; a negative id is `invalid params: id must be a non-negative integer` | Rust type honesty; the schema already requires a non-negative number |
| 11 | Table-driven `VALID_TRANSITIONS` map | `matches!` table with the same cells, pinned cell-by-cell | Idempotent, allocation-free, same semantics |
| 12 | Regex-based `sanitizeTerminalText` | Hand-written single-pass scanner | No regex on a hot path; pinned character cases (CSI, C1 CSI, OSC with/without ST, bare ESC, C0/C1 controls, U+2028/29, bidi) are covered |
| 13 | i18n through `@juicesharp/rpiv-i18n` (9 locales) with English fallbacks | English literals only | No i18n layer; scoped out |
| 14 | `~/.config/rpiv-todo/config.json` (`maxWidgetLines`, `collapseKey`, `guidance`) | Not read | These affect overlay rows, collapse key and prompt guidance only; overlay out of scope, built-in guidance shipped |
| 15 | Overlay widget, collapse state, "completed rows fade after the turn", per-row glyphs/colors, `selectOverlayLayout`; only the foreground session's overlay repaints | Not ported; no render surface and each session host owns its instance. `ctrl+shift+t` is a registered definition (name + description) with a no-op Rust handler for a UI lane | v1 scope: `src/tui/**` is another lane's surface; the definition is the seam |
| 16 | `/todos` through `ctx.ui.notify(..., "info")` | `CommandOutcome.error` text (`ok: true`), the mcp builtin's channel | No Rust notify route today (see §3); print shows it, TUI surfacing is a host fix |

Everything else — six actions, parameter names/semantics, status machine, tombstones, dependency validation and
`blocks` edges, metadata null-deletion, the envelope shape, all contract error strings, `list`/`get` content lines
and the `/todos` layout — follows upstream.
