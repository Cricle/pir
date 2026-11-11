# `tool-repair` builtin — assistant tool-call repair pipeline

`builtin:tool-repair` is the native Rust port of npm **`pi-tool-repair` 0.3.6**
(monotykamary/pi-tool-repair). It registers **no tool, command or flag**; its work is two host
seams:

1. **`message_end` repair** — a dedicated native hook (`LoopDeps.message_end_repair`, wired in
   `src/agent_session/tool_repair.rs` and `src/coding_agent/print.rs`) that mutates the finalized
   assistant message before it is persisted and resent.
2. **`before_provider_request` schema sanitizer** — Phase 0 anchor-bleed repair on the unified
   plugin event dispatch, gated by model id.

- Implementation: `src/plugins/tool_repair.rs`, `src/plugins/tool_repair/{repair,aliases,config,grammar}.rs`;
  feature `tool-repair`; entry `src/plugins/builtin.rs` (`name: "tool-repair"`, `replaceable: true`,
  default-enabled).
- Upstream truth: package `src/index.ts:111-134` (Phase 0), `:136-231` (strip passes), `:400-434`
  (phantom normalization), `:709-963` (live-schema repair), `src/grammar-repair.ts` (ten families),
  `tool-repair.ts:73-105` (request handler), `:109-173` (message_end order).

## 1. Repair pipeline (`message_end`)

Applied in order by `repair::repair_message_with_grammar`:

1. GLM token-leak strip.
2. Anchor-bleed strip.
3. Grammar-leak recovery/strip (ten families; recover or strip per family and model gate).
4. Live-schema repair against the real `AgentTool::parameters()` schemas (rename alias fields,
   coerce numeric strings, drop unknown properties).
5. Phantom `toolUse` normalization.
6. Finalized message written back; the corrected call is what the session JSONL stores and what
   is resent next turn. Unrepairable calls keep their original error.

Debug logging is one stderr line per repair when `PI_TOOL_REPAIR_DEBUG` is set (upstream prints a
multi-line block). Config is loaded lazily from `PI_TOOL_REPAIR_DEBUG` and
`<agent-dir>/extensions/pi-tool-repair.json`; package defaults apply when the file is absent.

## 2. `before_provider_request` (Phase 0)

- Consumes exactly one event: `EventInterest::List(["before_provider_request"])`.
- Gate: the model id cached from `AgentPlugin::on_session_context` (or the request's model) must be
  in `anchorBleedModels`; otherwise the handler returns `None` and the host keeps the original body.
  The host may skip installing the transport hook entirely for an inert model
  (`handler_active_for_event`).
- When active, `sanitize_request_payload` strips anchor-bleed text from the serialized request
  `payload`; the sanitized body is returned as the merged event outcome, so later plugins and
  observers see it too.
- Print and session/AgentSession hosts both route this event through the same unified dispatch;
  the native face runs in print mode as well.

No `tool_call` / `tool_result` / `session_start` handlers. `on_session_context` is not a plugin
event; it is the host snapshot push used only to cache the model id and lazily set config.

## 3. Default plan / feature gate

- Default plan: enabled. Disable with `disabledBuiltinExtensions: ["tool-repair"]`.
- Native-first precedence: when both the builtin and the JS `pi-tool-repair` package are on the
  effective extension plan, `plugins::loader::tool_repair_precedence` drops the JS entry so the
  pipeline never runs twice. With the builtin disabled the JS package stays as the fallback face.
- Without the `tool-repair` feature the builtin is absent; `builtin:tool-repair` reports the
  unknown/not-compiled builtin error and the JS fallback remains available.

## 4. Error paths / known boundaries

- Malformed or missing model in the session snapshot clears the request gate (safe: no rewrite).
- A request payload without `tools`/`response_format` is left untouched (`sanitize` returns
  `false`).
- Bare-string tool-call roots cannot occur via the message pipeline because Rust parses arguments
  into an object map; `wrapRootStringAsObject` is ported but only reachable through the per-call
  API.
- Live-schema validation uses the injected preflight validator; no parallel validator exists here.
- A repair keeps the original call id; recovered grammar calls get
  `tool_repair_<grammar>_<base36>_<i>` ids. A failed repair never turns a valid result into an
  error; it leaves the original message.
