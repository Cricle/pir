# Hook coverage: upstream pi → pir (native + JS faces)

Coverage matrix for pi's extension hook surface (upstream `types.d.ts`: `ExtensionEvent`,
`ExtensionAPI.on`, `ExtensionUIContext`), audited against pir's dispatch sites; emit-site
`file:line` references are indicative and drift. `tests/hook_coverage.rs` compares the marker blocks
with `src/plugins/coverage.rs` and statically checks every wired name's emit site.

Two faces receive hooks: **JS** (`pi.on` via `ExtensionHandle::emit` / `dispatch_hosted_event`) and
**native** (Rust/inline plugins via `AgentPlugin::on_event`, through `PluginHost::emit` — ordered,
JS adapter once — or `emit_native` where JS has its own lifecycle). Delivery is ordered per face: a
site dispatching JS directly uses `emit_native` for the Rust half; otherwise `emit` includes the JS
adapter as one ordered plugin. Exactly-once holds only for direct lifecycle paths owning both faces
(`agent_settled` / `entry_appended` / `session_info_changed`, per-dialog `ui_prompt_*`);
`SessionEvent` and `message_update` use bounded per-face queues (best-effort/at-most-once under
overflow or dispatch failure).

### Delivery limits

- `SessionEvent` bridge: one ordered queue per session (64 entries; overflow drops the newest,
  dispatch capped at 10 s). Bridge events (`queue_update`, `compaction_start`/`compaction_end`,
  `auto_retry_*`, `bash_execution_update`, `background_tool_end`) are at-most-once per face.
- `message_update`: separate bounded queue per face (16 deltas); both drain before `message_end` in
  stream order, but a failed dispatch is logged and skipped (no cross-face exactly-once claim).
- `ui_prompt_start` / `ui_prompt_end` are per-face: a native dialog emits only to native plugins, a
  JS dialog only to the JS handler; each pair is once per dialog on its owning face.
- Other multi-face events are delivered once per face per emit call unless a handler fails or the
  host tears down mid-dispatch.

## Event matrix

Capability abbreviations: **O** observe, **M** mutate payload in place, **T** transform the result
the host applies, **B** block/cancel, **R** replace the provider request/response payload.

| Event | JS face | Native face | Emit site (file:line) | Capability | Status |
|---|---|---|---|---|---|
| `project_trust` | runtime loader, pre-host | `emit_native` | `agent_session/runtime_loader.rs:953` (JS), `:973` (native); `agent_session/build.rs:62` | O | wired (native supplemental; host owns trust; final loads do not dispatch) |
| `resources_discover` | `plugins.emit` | `plugins.emit` | `agent_session/session_ctx.rs:332`; print `coding_agent/print.rs:3026` | O (paths collected) | wired |
| `session_start` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:978`, `session_store.rs:949`; native reload `session_commands.rs:1092`; print `coding_agent/print.rs:553`, `:2851` | O | wired (reasons `startup`/`new`/`resume`/`fork`/`reload`; see deviations) |
| `session_info_changed` | `plugins.emit` | `plugins.emit` | `agent_session/transcript.rs:143` | O | wired |
| `session_before_switch` | `plugins.emit` | `plugins.emit` | `agent_session/session_store.rs:100` | B | wired (reason `switch`; see deviations) |
| `session_before_fork` | `plugins.emit` | `plugins.emit` | `agent_session/session_store.rs:786` | B | wired |
| `session_before_compact` | `plugins.emit` | `plugins.emit` | `agent_session/compact.rs:398`, `:851`, `:1752`; print `coding_agent/print.rs:5581`, `:5732` | B, T (compaction) | wired (both faces cancel/replace; print: threshold only) |
| `session_compact` | `plugins.emit` | `plugins.emit` | `agent_session/compact.rs:545`, `:1072`, `:1272`; `prompt_loop.rs:2474`; print `coding_agent/print.rs:5093`, `:5462` | O | wired |
| `session_compact_failed` | `plugins.emit` | `plugins.emit` | `agent_session/compact.rs:876`, `:919`, `:1138`, `:1322`; `notifications.rs:646`; print `coding_agent/print.rs:4827`, `:4857` | O | wired |
| `session_shutdown` | actor mailbox close / reload (`extensions/handle/actor.rs:450`, `:672`) | `emit_native` for quit/reload; both faces for switch | `plugins/host.rs:3138` (`emit_session_shutdown` → `self.emit`, both faces; switch caller `agent_session/session_store.rs:117`); `plugins/host.rs:3065` (`emit_native_session_shutdown`, reload `:3203`, quit `:3247`) | O | wired (switch sees both faces; quit/reload native-only; JS actor closes its own lifecycle, never double-emitted) |
| `session_before_tree` | `plugins.emit` | `plugins.emit` | `agent_session/session_store.rs:509` (after the read-only collection, before summarize/leaf move) | B, M, T (cancel; instructions/summary/label overrides) | wired |
| `session_tree` | `emit_read_only_json` | `emit_read_only_json` | `agent_session/session_store.rs:297`, `:332`, `:1042` | O | wired |
| `context` | runtime emit | `emit_native` | `agent/agent_loop.rs:286` (native at `:281`) | T (replace messages) | wired (native mutation + JS return; JS return wins) |
| `context_with_system` | runtime emit | `emit_native` | `agent/agent_loop.rs:380` (native at `:375`) | T (messages + system prompt) | wired (native mutation + JS return; JS return wins) |
| `cache_warming_decision` | `plugins.emit` | `plugins.emit` | `agent_session/cache_warm.rs:70` | T | wired |
| `before_provider_request` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:208`; print `coding_agent/print.rs:185` | R | wired (returned payload replaces) |
| `before_provider_headers` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:276`; print `coding_agent/print.rs:219` | M | wired per provider request (prompt, compaction/branch summary, print) before auth headers are added |
| `after_provider_response` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:249`; print `print.rs:212` | O | wired |
| `provider_stream_event` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:847`; print `coding_agent/print.rs:220` | O | wired |
| `before_agent_start` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:81`; print `coding_agent/print.rs:3792` | T (system prompt, injected message) | wired (print injections ride the loop preamble) |
| `agent_start` | `emit_extension` | `emit_native` | `agent/agent_loop.rs:1432` | O | wired |
| `agent_end` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:3105` | O | wired |
| `agent_before_settle` | `plugins.emit` | `plugins.emit` | `agent_session/settle.rs:1067`; print `coding_agent/print.rs:3963` | T (continue/entries) | wired (print re-projects the session before continuing) |
| `agent_settled` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:724`, `:2629`; print `coding_agent/print.rs:4057` | O | wired |
| `turn_start` | `emit_extension` | `emit_native` | `agent/agent_loop.rs:1467`, `:1789` | O | wired |
| `turn_end` | `plugins.emit` | `plugins.emit` | `agent_session/settle.rs:1016`; print `coding_agent/print.rs:3713` (hook builder `:1037`) | T (continue) | wired |
| `message_start` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:1826`, `:2276`, `:2689`, `:4415`, `:4496` | O | wired |
| `message_update` | runtime direct | `emit_native` | `agent/agent_loop.rs:4204` (native), `extensions/handle/mailbox.rs:569` (JS) | O | wired (native compact by default; `wants_message_update_content` opts into `message.content`) |
| `message_end` | runtime direct | `emit_native` | `agent/agent_loop.rs:190` (native), `:194` (JS) | T (replace message) | wired |
| `tool_execution_start` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:2222`, `:3318` | O | wired |
| `tool_execution_update` | runtime / `plugins.emit` | `emit_native` | `agent_session/tools.rs:390`, `:422`, `:445`; `notifications.rs:981` | O | wired |
| `tool_execution_end` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:2252`, `:3369` | O | wired |
| `tool_call` | `dispatch_hosted_event` | `plugins.emit` | `agent/tool_execution.rs:1918` | B, M (input) | wired |
| `tool_result` | `dispatch_hosted_event` | `plugins.emit` | `agent/tool_execution.rs:2197` | T (content/details/error) | wired |
| `model_select` | `plugins.emit` | `plugins.emit` | `agent_session/models.rs:59` | O | wired |
| `thinking_level_select` | `plugins.emit` | `plugins.emit` | `agent_session/models.rs:211` | O | wired |
| `user_bash` | runtime emit | `emit_native` | `agent_session/runtime_loader.rs:1057` | T (operations/result) | wired |
| `input` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_input.rs:90` (JS), `:149` (both); print `coding_agent/print.rs:3491` | T (text/images) | wired (every positional prompt) |
| `terminal_input` | `ExtensionHandle::emit_with_timeout` (prelude `ctx.ui.onTerminalInput` list) | `PluginHost::dispatch_terminal_input` (native event; `UiService::on_terminal_input` registry first) | `tui/terminal.rs:11224` (input gate) → `tui/terminal_input.rs:479` → `plugins/host.rs:1481` (`dispatch_terminal_input_with_timeout`) | B, M (consume / replace `data`) | wired (bridge for upstream `onTerminalInput`; one chunk per physical input; 5 ms inline, 5 s deferred fail-open) |
| `ui_prompt_start` | prelude `withUIPrompt` | `emit_native` | `agent_session/plugin_services.rs:1762` (native dialogs) | O | wired |
| `ui_prompt_end` | prelude `withUIPrompt` | `emit_native` | `agent_session/plugin_services.rs:1779`, `:1803` (drop path) | O | wired |
| `mcp_servers_change` | `js.emit` (direct handle) | `emit_native` | `plugins/builtin/mcp.rs:1437` (native at `:1442`) | O | wired (one copy per face; coalesced per registry rebuild) |

`session_before_compact` is a typed outcome: `PluginEventOutcome::SessionBeforeCompact` carries
`{cancel, compaction}` through the shared merge layer (`plugins/plugin.rs:130-145`,
`plugins/merge.rs:197-214`), so JS and native handlers both cancel and replace; `print` consumes the
same `dispatch_before_compact` decision (`agent_session/compact.rs:1725`).

**`pir print` host.** The one-shot host drives the agent loop without an `AgentSession` but emits
the same lifecycle through shared adapters: `session_start` (`coding_agent/print.rs`),
`resources_discover` at startup, `input` for **every** positional prompt (`source` is print's
historical `"rpc"`), `before_agent_start` per prompt (injected messages ride the loop preamble, so
`message_start`/`message_end` and replacement apply), `turn_end` (`print_turn_end_hook`),
`agent_before_settle` (`continue` re-projects the transcript so `custom_message` / `context_edit` /
`compaction` drafts reach the next request), `session_before_compact`, `session_compact`,
`session_compact_failed`, `agent_settled`; loop-level events reach native plugins through the host.

Print limitations: compaction is **threshold-only** (`reason` always `"threshold"`), and the
notification drain applies a safe **actionable subset** (`print.rs:1054-1185`): `sendMessage` steers
into the preparing run or persists a custom message entry, `sendUserMessage` steers only while
preparing, `appendEntry` / `setSessionName` / `setLabel` persist, and `abort` cancels the current
run; `setModel` / `setThinkingLevel` / `compact` / `shutdown` are logged and ignored (print resolves
model and compaction config once per invocation). Paged sessions use a **widening fallback**: print
widens to the full resident view when threshold compaction cannot fire (`:4379-4412`) and after a
paged compaction failure (`:6630-6652`), so the request sees the complete transcript instead of a
truncated tail. An extension `abort` settles only the current run; later prompts continue with a
fresh signal (`:3455-3470`, `:3691-3700`). `notify` / `status` keep their stderr diagnostic routing.
**Print SessionEvents** feed an ordered, non-blocking plugin bridge, so
`compaction_start`/`end`, `auto_retry_*`, `bash_execution_update`, `background_tool_end`,
`summarization_retry_*` and `entry_appended` reach plugin handlers and the JSON stdout sink.

Session-event bridge (upstream `AgentSessionEvent` names outside the `ExtensionEvent` union;
installed for native-only, JS-only and mixed hosts — `agent_session/notifications.rs:108-115` —
one ordered queue). Direct `agent_settled` / `entry_appended` / `session_info_changed` sites own
both faces; the bridge skips them to avoid double delivery (`notifications.rs:147-158`). Compaction
hooks flush the queued bridge first (`plugins/host.rs:2114`, `flush_session_event_backlog`), so both
faces observe `compaction_start → session_before_compact → session_compact → compaction_end` (and
`compaction_end → session_compact_failed` on failure):

| Event | JS face | Native face | Source | Status |
|---|---|---|---|---|
| `queue_update` | `plugins.emit` | `plugins.emit` | `agent_session/queue.rs:81`; `build.rs:1117` | wired |
| `compaction_start` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:518` | wired |
| `compaction_end` | `plugins.emit` | `plugins.emit` | `agent_session/compact.rs:163`; `prompt_loop.rs:1875` | wired |
| `auto_retry_start` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:100` | wired |
| `auto_retry_end` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:112` | wired |
| `bash_execution_update` | `plugins.emit` | `plugins.emit` | `agent_session/bash.rs:145` | wired |
| `background_tool_end` | `plugins.emit` | `plugins.emit` | `agent_session/background.rs:784` | wired |

Native-only presentation events (no upstream `on` counterpart; headless projection of `ctx.ui.*`
notifications): `status`, `extension_ui_request` (`agent_session/notifications.rs:357`, `:365`).

`pi.on` is fail-closed against the central registry (`src/plugins/coverage.rs::is_known_event`; the
prelude calls `isKnownEvent`). The accepted set is exactly `WIRED_NATIVE_EVENTS` +
`NATIVE_ONLY_EVENTS` (`status`, `extension_ui_request`) + `SYNTHETIC_EVENTS` (`entry_appended`,
`thinking_level_changed`, `history_trimmed` (PIR-BIN-075: the store-less ephemeral-history trim
notice), the three `summarization_retry_*` names, `run_failed`,
`provider:refreshModels`) + hidden `TEST_ONLY_EVENTS` used by in-tree harnesses. An unknown name is
a load-time `TypeError` (`pi.on: unknown event name ...`); custom bus events use `pi.events.on`.

## Marker blocks (machine-checked)

`tests/hook_coverage.rs` requires both blocks to match `src/plugins/coverage.rs` exactly (set
equality, one event per line). `WIRED_NATIVE_EVENTS` has 49 entries; `UNSUPPORTED_NATIVE_EVENTS` is
empty.

<!-- wired-native-events -->
project_trust
resources_discover
mcp_servers_change
session_start
session_info_changed
session_before_switch
session_before_fork
session_before_compact
session_compact
session_compact_failed
session_shutdown
session_before_tree
session_tree
context
context_with_system
cache_warming_decision
before_provider_request
before_provider_headers
after_provider_response
provider_stream_event
before_agent_start
agent_start
agent_end
agent_before_settle
agent_settled
turn_start
turn_end
message_start
message_update
message_end
tool_execution_start
tool_execution_update
tool_execution_end
tool_call
tool_result
model_select
thinking_level_select
user_bash
input
terminal_input
ui_prompt_start
ui_prompt_end
queue_update
compaction_start
compaction_end
auto_retry_start
auto_retry_end
bash_execution_update
background_tool_end
<!-- /wired-native-events -->

<!-- unsupported-native-events -->
<!-- /unsupported-native-events -->

## TUI capability matrix

Upstream `ExtensionUIContext` → pir's native `UiService` / `PluginContext`. JS face: the prelude's
`ctx.ui.*`; native face: the bound service (`PluginContext::ui()`), with `StatusService` for status
cells.

| Capability | JS face | Native face | Parity / reason |
|---|---|---|---|
| `select` / `confirm` / `input` / `editor` | prelude (`select` → `DialogRequest`) | `UiService::{select, confirm, input, editor}` | full; shared request/reply queue, native id range |
| `notify` | `notify` host fn | `UiService::notify` | full |
| `setStatus` | `setStatus` host fn | `StatusService::set_status` (via `ctx.status()`) | full; different accessor (live status cell, not a notification) |
| `setWidget(key, string[] or factory)` | `setWidgetJson` / `widgetRenderers` round-trip | `UiService::set_widget_static` / `set_widget` / `clear_widget` | full for static lines; native factories render through the shared `FactoryWidget` cache (`FactoryRendererInvoker`, native wins; removal reveals the JS factory) |
| `setFooter(factory)` / `setHeader(factory)` | `footerRenderers` / `headerRenderers` round-trip | `UiService::set_footer` / `set_header` | wired: native bar factory replaces the footer/header; `None` restores built-in |
| `setWorkingMessage` / `setWorkingVisible` / `setWorkingIndicator` / `setHiddenThinkingLabel` | notification | `UiService::set_working_message` / `set_working_visible` / `set_working_indicator` / `set_hidden_thinking_label` | full |
| `setTitle` / `pasteToEditor` | notification | `UiService::set_title` / `paste_to_editor` | full |
| `setEditorText` / `getEditorText` | host fn / shared cell | `UiService::set_editor_text` / `editor_text` | full |
| `setTheme` / `theme` / `getAllThemes` | theme registry | `UiService::set_theme` / `theme` / `themes` | full |
| `getToolsExpanded` / `setToolsExpanded` | shared runtime cell | `UiService::tools_expanded` / `set_tools_expanded` | full (same `RuntimeState::tools_expanded` cell the TUI's Ctrl+O writes) |
| `onTerminalInput` | prelude handler list, dispatched through the `terminal_input` bridge | `UiService::on_terminal_input` + `AgentPlugin::on_event("terminal_input")` | full: each chunk dispatches once before built-in handling; `{consume}` stops, `{data}` replaces; see deviation 6 |
| `custom` (overlay component) | prelude `custom`: builds the component and routes `uiDialog("custom")` through the shared overlay funnel (`prelude.js.txt:695`) | `UiService::custom` (`plugin_services.rs:2333`, `start_custom_overlay` `:2011`) | wired in TUI: same dialog slot/queue/deadline; components render and take input/focus, `done(value)` settles on the shared funnel. Non-UI hosts: JS `undefined`, native `capability("ui","custom")` |
| `setEditorComponent` | recorded for `getEditorComponent` bookkeeping, never rendered | not bound | **unsupported by design**: pir's editor is a native Rust component with no byte-level editor factory seam (documented in the prelude) |
| dialogs (`ui_prompt_start/end`) | prelude `withUIPrompt` | native `native_ui_prompt` guard | full for each native dialog (start on first poll, end exactly once on ok/cancel/timeout/error/drop) |

## Intentional deviations

1. **No npm module loading.** QuickJS shims the SDK packages (`@earendil-works/pi-coding-agent`,
   `pi-agent-core`, `pi-tui`, `pi-ai`, …) as virtual modules; Node built-ins and npm deps do not
   resolve ([packages.md](packages.md), [extensions.md](extensions.md)). Hook names/payloads are unaffected.
2. **`session_start` reasons.** `startup` (first prompt / print start), `reload` (native reload
   face; the JS actor reloads itself), and `new`/`resume`/`fork` when storage is replaced
   (`session_store.rs:949`); `session_shutdown` reasons: `quit`, `reload`, `switch` (storage
   replacement, `session_store.rs:117`).
3. **`session_before_switch` reason.** `switch` only; upstream's `new`/`resume` split rides the
   post-swap `session_start` reason.
4. **`session_before_tree`'s `signal` is a state snapshot.** Upstream passes a live `AbortSignal`;
   pir's JSON wire carries `{aborted: false}` (same shape as `session_before_compact`); host-side
   Esc during branch summarization remains the live seam (`abort_branch_summary`).
5. **Native dialogs' `ui_prompt_*` go to native plugins only.** The JS prelude emits the pair for JS
   dialogs; per-dialog, not outermost-only (upstream's nesting counter applies to JS recursive prompts).
6. **`terminal_input` dispatch is asynchronous with a deferred decision.** Upstream listeners run
   synchronously on the TUI thread and can block typing/rendering; pir runs them on a dedicated
   thread and waits **5 ms inline**, deferring a miss (later input queues behind it) until the late
   answer applies to the original chunk (`pass`/`consume`/`replace`; replacements bypass-marked).
   Deferral is bounded by **5 s** (`TERMINAL_INPUT_DEFER_BUDGET`, `PIR_TUI_TERMINAL_INPUT_BUDGET_MS`;
   positive capped at 60000, invalid/zero/negative → 5000); expiry fails open and discards the answer.
   Request queue bounded at 64; overflow fails open; over-cap replacement (4 KiB/256 events) is
   consumed with a diagnostic. Face order: native registry → native plugin handlers → JS
   `ctx.ui.onTerminalInput`. Crossterm events are re-encoded into canonical escape spelling, so `"!"`,
   `"\x1b[A"` and `"\x1b[200~…\x1b[201~"` match identically; mouse/focus included.
7. **`setEditorComponent` is bookkeeping-only.** The JS face records the factory for
   `getEditorComponent()` but never renders a custom editor; no native equivalent is missing.
8. **Native `message_update` payload is compact by default, with per-plugin content opt-in.** The
   hoisted `message` keeps scalar fields but omits cumulative `message.content` (O(delta) hot path).
   A native plugin overrides `AgentPlugin::wants_message_update_content()` to `true`; the host
   snapshots that at build and the loop builds the full payload per delta (O(reply)); `message_end`
   carries the final message, and the JS face keeps full payloads.
9. **`message_update` also carries the stream's terminal frames.** Upstream `pi.on("message_update")`
   runs only for content-block deltas (`agent-loop.ts:426-473` never emits the event for `start` /
   `done` / `error`); pir also delivers `assistantMessageEvent.type == "start"` (the initial partial)
   and `"done"`/`"error"` (the terminal message). Counts therefore differ (`tool-ok`: pir 11 vs pi 7
   for the same conversation). A handler that only wants deltas should switch on the event type.
10. **Session-event bridge names are `pi.on` events on pir only.** Upstream registers
    `compaction_start`/`compaction_end`/`queue_update`/`entry_appended` with `session.subscribe`, not
    `pi.on`; a pi extension that calls `pi.on("compaction_start")` never fires (verified on pi
    1.4.0). pir routes the same names to both hook faces (the table above), so a handler written
    against pir sees them and the same handler on pi does not — the bridge is a pir extension, not
    an upstream surface.
11. **`entry_appended` fan-out.** pir delivers `entry_appended` for every persisted transcript
    message (user/system/assistant/toolResult); upstream `pi.on`/`session.subscribe` emits it for
    `pi.appendEntry()` custom entries and boundary/context-edit drafts only. Counting appends in a
    handler therefore differs by design; use `message_start`/`message_end` for per-message work.
12. **Payload extensions and omissions (handler-visible).**
    - `session_compact` additionally carries `compaction` (summary/firstKeptEntryId/tokensBefore/
      estimatedTokensAfter/usage/details) next to upstream's `compactionEntry`; it backs
      `ctx.compact({ onComplete })`.
    - `entry_appended`, `bash_execution_update`, `queue_update`, `session_tree` payload shapes are
      pir's session-event shapes (`entry`/`delta`/`steering`+`followUp`/`newLeafId`+`oldLeafId`).
    - `tool_result` omits `details` and `usage` when the tool reported none (upstream's event keys
      exist with `undefined` values).
    - `input` always carries `images: []` and `streamingBehavior: null`; upstream omits both keys
      when absent.
    - Compaction summarization requests run the provider hooks (`before_provider_request`,
      `after_provider_response`, `provider_stream_event`) on pir; upstream's `completeSummarization`
      passes no `onPayload`/`onResponse`, so only `before_provider_headers` fires there (measured:
      5 vs 4 `before_provider_request` in one manual-compaction conversation).
    - `agent_end.messages` is the run's produced messages MINUS the caller-supplied starting prompt
      (upstream includes it); see PIR-BIN-047.
13. **The abort path dispatches no `agent_before_settle`.** Upstream's settle loop tests the run's
    abort request BEFORE the pre-settle boundary (`agent-session.ts:1900-1907`), so a cancelled turn
    emits `turn_end -> agent_end -> agent_settled{aborted:true}` with zero boundary calls. pir's
    AgentSession settle loop and the print settle loop take the same early exit (`abort_requested`
    from the run's terminal/abort flag/session flag in `agent_session/prompt_loop.rs`; the run
    signal in `coding_agent/print.rs`): no boundary dispatch, no honored `continue`, no extra
    `agent_start`/`agent_end` pair after the abort. A cancelled tool call still goes through
    `tool_execution_start -> tool_call -> tool_result -> tool_execution_end` (upstream prepares and
    awaits `beforeToolCall` before it reads the abort signal), with the aborted result as its
    `tool_result`/`tool_execution_end`, so a handler observes the cancellation exactly once.
14. **Tool hook order is `tool_execution_start` THEN `tool_call`.** Upstream awaits the start emit
    before `prepareToolCall(..., beforeToolCall)` (agent-loop.ts:553-566 sequential, 608-620
    parallel); pir's scheduler queues the start frame into its progress channel and yields once
    before dispatching the gate (only when both a progress consumer and a `tool_call` observer
    exist), so a handler sees the start first. `tool_execution_start.args` is the assistant
    message's raw call arguments, never the gate's rewrite; the mutated input reaches the tool
    through `tool_call` and `tool_result` sees the executed payload.
