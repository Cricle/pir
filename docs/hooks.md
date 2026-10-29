# Hook coverage: upstream pi → pir (native + JS faces)

This is the coverage matrix for pi's extension hook surface, derived from the
installed upstream `types.d.ts` (`ExtensionEvent`, `ExtensionAPI.on`,
`ExtensionUIContext`) and audited against pir's actual dispatch sites. Every
row names the emit site so a drift can be checked mechanically; the two marker
blocks at the bottom are compared against `src/plugins/coverage.rs` by
`tests/hook_coverage.rs`.

Two faces receive hooks in pir:

- **JS face** — the embedded QuickJS runtime (`pi.on(...)`), reached through
  `ExtensionHandle::emit` / `dispatch_hosted_event`.
- **Native face** — Rust builtins and inline plugins
  (`AgentPlugin::on_event`), reached through `PluginHost::emit` (ordered list,
  JS adapter included once) or `PluginHost::emit_native` (native plugins
  only, used where the JS face has its own lifecycle path).

"Exactly one delivery per face" is the invariant: a site that already
dispatches the JS runtime directly uses `emit_native` for the Rust half; a
site with no separate JS path uses `emit`, whose JS adapter is one plugin in
the ordered list.

## Event matrix

Capability abbreviations: **O** observe, **M** mutate payload in place,
**T** transform the result the host applies, **B** block/cancel,
**R** replace the provider request/response payload.

| Event | JS face | Native face | Emit site (file:line) | Capability | Status |
|---|---|---|---|---|---|
| `project_trust` | runtime loader, pre-host | `emit_native` | `agent_session/build.rs:61`, `agent_session/runtime_loader.rs:774` | O | wired (native is supplemental; host owns trust) |
| `resources_discover` | `plugins.emit` | `plugins.emit` | `agent_session/session_ctx.rs:589` | O (paths collected) | wired |
| `session_start` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:297`; print `coding_agent/print.rs:260` | O | wired (reasons: `startup`; see deviations) |
| `session_info_changed` | `plugins.emit` | `plugins.emit` | `agent_session/notifications.rs:443` | O | wired |
| `session_before_switch` | `plugins.emit` | `plugins.emit` | `agent_session/session_store.rs:69` | B | wired (reason `switch`; see deviations) |
| `session_before_fork` | `plugins.emit` | `plugins.emit` | `agent_session/session_store.rs:493` | B | wired |
| `session_before_compact` | `plugins.emit` | `plugins.emit` | `agent_session/compact.rs:273`, `:525` | B, T (compaction) | wired |
| `session_compact` | `plugins.emit` | `plugins.emit` | `agent_session/compact.rs:336`, `:607`, `:703`; `prompt_loop.rs:1086` | O | wired |
| `session_compact_failed` | `plugins.emit` | `plugins.emit` | `agent_session/notifications.rs:400`; `compact.rs:538`, `:644` | O | wired |
| `session_shutdown` | actor mailbox close / reload (`extensions/handle/actor.rs:302`, `:362`) | `emit_native` | `plugins/host.rs:1442` (`emit_native_session_shutdown`), called at `:1461` (reload) and `:1499` (shutdown) | O | wired (native; JS has its own lifecycle, never double-emitted) |
| `session_before_tree` | `plugins.emit` | `plugins.emit` | `agent_session/session_store.rs:361` (after the read-only collection, before summarize/leaf move) | B, M, T (cancel; instructions/summary/label overrides) | wired |
| `session_tree` | `emit_read_only_json` | `emit_read_only_json` | `agent_session/session_store.rs:172`, `:185`; navigation `:414` | O | wired |
| `context` | runtime emit | `emit_native` | `agent/agent_loop.rs:269` (native at `:272`) | T (replace messages) | wired (native mutation + JS return; JS return wins) |
| `context_with_system` | runtime emit | `emit_native` | `agent/agent_loop.rs:360` (native at `:363`) | T (messages + system prompt) | wired (native mutation + JS return; JS return wins) |
| `cache_warming_decision` | `plugins.emit` | `plugins.emit` | `agent_session/cache_warm.rs:62` | T | wired |
| `before_provider_request` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:164`; print `coding_agent/print.rs:169` | R | wired (returned payload replaces) |
| `before_provider_headers` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:428` | M | wired |
| `after_provider_response` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:194`; print `print.rs:196` | O | wired |
| `provider_stream_event` | `plugins.emit` | `plugins.emit` | `agent_session/stream.rs:361`; print `coding_agent/print.rs:1418` | O | wired |
| `before_agent_start` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:374` | T (system prompt, injected message) | wired |
| `agent_start` | `emit_extension` | `emit_native` | `agent/agent_loop.rs:941` | O | wired |
| `agent_end` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:1986` | O | wired |
| `agent_before_settle` | `plugins.emit` | `plugins.emit` | `agent_session/settle.rs:85` | T (continue/entries) | wired |
| `agent_settled` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_loop.rs:1219`; print `coding_agent/print.rs:1803` | O | wired |
| `turn_start` | `emit_extension` | `emit_native` | `agent/agent_loop.rs:1070` | O | wired |
| `turn_end` | `plugins.emit` | `plugins.emit` | `agent_session/settle.rs:275` | T (continue) | wired |
| `message_start` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:1095`, `:1180`, `:1364`, `:1732` | O | wired |
| `message_update` | runtime direct | `emit_native` | `agent/agent_loop.rs:2491` (native), `:2652` (JS) | O | wired |
| `message_end` | runtime direct | `emit_native` | `agent/agent_loop.rs:181`; loop `:1810` | T (replace message) | wired |
| `tool_execution_start` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:1314`, `:1620` | O | wired |
| `tool_execution_update` | runtime / `plugins.emit` | `emit_native` | `agent_session/tools.rs:254`, `:285`, `:308`; `notifications.rs:714` | O | wired |
| `tool_execution_end` | `emit_extension_lazy` | `emit_native` | `agent/agent_loop.rs:1344`, `:1670` | O | wired |
| `tool_call` | `dispatch_hosted_event` | `plugins.emit` | `agent/tool_execution.rs:1382` | B, M (input) | wired |
| `tool_result` | `dispatch_hosted_event` | `plugins.emit` | `agent/tool_execution.rs:1569` | T (content/details/error) | wired |
| `model_select` | `plugins.emit` | `plugins.emit` | `agent_session/notifications.rs:483` | O | wired |
| `thinking_level_select` | `plugins.emit` | `plugins.emit` | `agent_session/notifications.rs:518` | O | wired |
| `user_bash` | runtime emit | `emit_native` | `agent_session/runtime_loader.rs:827` | T (operations/result) | wired |
| `input` | `plugins.emit` | `plugins.emit` | `agent_session/prompt_input.rs:88` | T (text/images) | wired |
| `terminal_input` | `ExtensionHandle::emit_with_timeout` (prelude `ctx.ui.onTerminalInput` list) | `PluginHost::dispatch_terminal_input` (native event; `UiService::on_terminal_input` registry first) | `tui/terminal.rs:8835` (input gate) → `plugins/host.rs:969` (`dispatch_terminal_input_with_timeout`) | B, M (consume / replace `data`) | wired (bridge for upstream `ExtensionUIContext.onTerminalInput`; one chunk per physical input; 5 ms inline decision, deferred answer with a 5 s fail-open backstop) |
| `ui_prompt_start` | prelude `withUIPrompt` | `emit_native` | `agent_session/plugin_services.rs:1373` (native dialogs) | O | wired |
| `ui_prompt_end` | prelude `withUIPrompt` | `emit_native` | `agent_session/plugin_services.rs:1387`, `:1407` (drop path) | O | wired |
| `mcp_servers_change` | `js.emit` (direct handle) | `emit_native` | `plugins/builtin/mcp.rs:698` (native at `:702`) | O | wired (one copy per face; coalesced per registry rebuild) |

Session-event bridge (upstream `AgentSessionEvent` names outside the
`ExtensionEvent` union; both faces through the ordered session event queue,
`agent_session/notifications.rs:58` / `:62`):

| Event | JS face | Native face | Source | Status |
|---|---|---|---|---|
| `queue_update` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:129` | wired |
| `compaction_start` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:134` | wired |
| `compaction_end` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:145` | wired |
| `auto_retry_start` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:231` | wired |
| `auto_retry_end` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:243` | wired |
| `bash_execution_update` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:158` | wired |
| `background_tool_end` | `plugins.emit` | `plugins.emit` | `agent_session/events.rs:181` | wired |

Native-only presentation events (no upstream `on` counterpart; headless
projection of `ctx.ui.*` notifications): `status`, `extension_ui_request`
(`agent_session/notifications.rs:220`, `:347`).

## Marker blocks (machine-checked)

`tests/hook_coverage.rs` requires these two blocks to match
`src/plugins/coverage.rs` exactly (set equality, one event per line).

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

Upstream `ExtensionUIContext` → pir's native `UiService` / `PluginContext`.
The JS face is the prelude's `ctx.ui.*`; the native face is the bound
service (`PluginContext::ui()`), with `StatusService` for status cells.

| Capability | JS face | Native face | Parity / reason |
|---|---|---|---|
| `select` | prelude → `DialogRequest` | `UiService::select` | full; same request/reply queue, native id range |
| `confirm` | prelude | `UiService::confirm` | full |
| `input` | prelude | `UiService::input` | full |
| `editor` | prelude | `UiService::editor` | full |
| `notify` | `notify` host fn | `UiService::notify` | full |
| `setStatus` | `setStatus` host fn | `StatusService::set_status` (via `ctx.status()`) | full, different accessor (status is a live cell, not a UI notification) |
| `setWidget(key, string[])` | `setWidgetJson` | `UiService::set_widget_static` | full (static lines, above/below slots) |
| `setWidget(key, factory)` | `widgetRenderers` round-trip | `UiService::set_widget` / `clear_widget` | wired: native factories render through the same `FactoryWidget` cache (`FactoryRendererInvoker`, native wins at the slot; removal reveals the JS factory) |
| `setFooter(factory)` | `footerRenderers` round-trip | `UiService::set_footer` | wired: native bar factory replaces the footer; `None` restores the built-in |
| `setHeader(factory)` | `headerRenderers` round-trip | `UiService::set_header` | wired: native bar factory replaces the header; `None` restores the built-in |
| `setWorkingMessage` | notification | `UiService::set_working_message` | full |
| `setWorkingVisible` | notification | `UiService::set_working_visible` | full |
| `setWorkingIndicator` | notification | `UiService::set_working_indicator` | full |
| `setHiddenThinkingLabel` | notification | `UiService::set_hidden_thinking_label` | full |
| `setTitle` | notification | `UiService::set_title` | full |
| `pasteToEditor` | notification | `UiService::paste_to_editor` | full |
| `setEditorText` / `getEditorText` | host fn / shared cell | `UiService::set_editor_text` / `editor_text` | full |
| `setTheme` / `theme` / `getAllThemes` | theme registry | `UiService::set_theme` / `theme` / `themes` | full |
| `getToolsExpanded` / `setToolsExpanded` | shared runtime cell | `UiService::tools_expanded` / `set_tools_expanded` | full (same `RuntimeState::tools_expanded` cell the TUI's Ctrl+O writes) |
| `onTerminalInput` | prelude handler list, dispatched through the `terminal_input` bridge | `UiService::on_terminal_input` + `AgentPlugin::on_event("terminal_input")` | full: every key/mouse/paste chunk dispatches once per physical input before built-in handling; `{consume}` stops it, `{data}` replaces it; 5 ms inline wait, deferred fallback with a 5 s backstop (`PIR_TUI_TERMINAL_INPUT_BUDGET_MS`). See deviation 6 |
| `custom` (overlay component) | async stub: emits `ui_prompt_start`/`ui_prompt_end`, returns `undefined`, warns once | not supported | **intentional deviation**: non-TUI/RPC/print match upstream (`undefined`/no-op). pir renders arbitrary JS components as dock/footer/header widgets through the existing renderer bridge, but does not host modal/overlay components with input/focus. Workaround: use `select`/`confirm`/`input`/`editor`, or the `setWidget`/`setFooter`/`setHeader` widget factories |
| `setEditorComponent` | recorded for `getEditorComponent` bookkeeping, never rendered | not bound | **unsupported by design**: pir's editor is a native Rust component with no byte-level editor factory seam (documented in the prelude) |
| dialogs (`ui_prompt_start/end`) | prelude `withUIPrompt` | native `native_ui_prompt` guard | full for each native dialog (start on first poll, end exactly once on ok/cancel/timeout/error/drop) |

## Intentional deviations

1. **No npm module loading.** pir embeds QuickJS and shims the SDK packages
   (`@earendil-works/pi-coding-agent`, `pi-agent-core`, `pi-tui`, `pi-ai`,
   …) as virtual modules; Node built-ins and arbitrary npm dependencies do
   not resolve (`docs/packages.md`, `docs/extensions.md`). Hook *names and
   payloads* are unaffected.
2. **`session_start` reasons.** pir emits `startup` (first prompt / print
   start); `new` / `resume` / `fork` are not emitted on session replacement.
   `session_shutdown` reasons are `quit` (host teardown) and `reload`; the
   replacement reasons are not emitted.
3. **`session_before_switch` reason.** pir emits `switch` (storage swap);
   upstream's `new` / `resume` split is not represented because the swap
   API does not carry the distinction.
4. **`session_before_tree`'s `signal` is a state snapshot.** Upstream passes
   a live `AbortSignal`; pir's JSON wire carries `{aborted: false}` (the same
   shape `session_before_compact` uses). Host-side Esc during branch
   summarization remains the live cancellation seam
   (`abort_branch_summary`).
5. **Native dialogs' `ui_prompt_*` events go to native plugins only.** The
   JS prelude emits the pair for JS dialogs; a native dialog is not a JS
   prompt, so `emit_native` keeps exactly-one delivery per face. The pair is
   per-dialog, not outermost-only (upstream's `withUIPrompt` nesting counter
   applies to the JS face's recursive prompts).
6. **`terminal_input` dispatch is asynchronous with a deferred decision.**
   Upstream listeners run synchronously on the TUI input thread and can block
   typing/rendering indefinitely. pir runs them on a dedicated thread and waits
   **5 ms inline**; a miss holds the physical event deferred (later input
   queues behind it, rendering continues), and the late answer applies to the
   original chunk (`pass`/`consume`/`replace`; replacement events are
   bypass-marked). The deferral is bounded by **5 s**
   (`TERMINAL_INPUT_DEFER_BUDGET`, `PIR_TUI_TERMINAL_INPUT_BUDGET_MS`, positive
   values capped at 60000, invalid/zero/negative → 5000); expiry fails open and
   discards the late answer. The request queue is bounded (64; overflow fails
   open; over-cap replacement 4 KiB/256 events is consumed with a diagnostic).
   Face order: native registry → native plugin handlers → JS
   `ctx.ui.onTerminalInput`. pir re-encodes crossterm events into canonical
   escape spelling, so handlers matching `"!"`, `"\x1b[A"` or
   `"\x1b[200~…\x1b[201~"` behave identically; mouse/focus reports included.
7. **`setEditorComponent` is bookkeeping-only upstream in pir.** The JS
   face records the factory for `getEditorComponent()` but never renders a
   custom editor; a native equivalent is therefore not missing behavior.
