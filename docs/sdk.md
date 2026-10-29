# SDK

`pir::sdk` is the curated Rust embedding surface for pir: build an agent,
extend it, and drive it from your own program.

- **Build** an agent from an explicit endpoint, the environment, or your own
  `ProviderRegistry` + `Provider` (the only path that needs no `provider-*`
  cargo feature).
- **Extend** it with closure/typed tools, inline `AgentPlugin`s, `on_event`
  handlers, or compiled-in builtins.
- **Run** it with `prompt` for an `AgentOutcome`, or `prompt_stream` for a
  typed, bounded `SdkEvent` stream.

## Quickstart

This is the canonical shape: a mock `Provider`, one tool, one `input`
transform, and `prompt_stream`. It needs no network, no API keys, no `~/.pi`
state, and no `provider-*` cargo feature — `with_registry` dispatches straight
to your provider. The canonical runnable examples are under `examples/`
(`quickstart`, `streaming`, `tools`, `sessions`, `plugin`, `control`, `builtin`,
`extension`, `proxy`, `real_endpoint`); each declares only the features its own
command uses (`required-features`): `sdk` for most, `sdk,session` for
`sessions`, plus `subagent` / `extensions` for those examples. `real_endpoint`
compiles in the common `sdk,session,skills` shape and needs a `provider-*`
adapter when run. `scripts/run_examples.sh` runs the offline set in one pass.
The old `logs/perf/sdk-mini/` fixture is historical and is not part of this
tree; the packaged consumer and its check evidence live in
`logs/lane-sdk-package/REPORT.md`.

```toml
[dependencies]
# Minimal core plus the optional SDK facade; no TUI, no JS engine, no
# provider wire adapters. Point `path` at your pir checkout. The package is
# `pir-sdk`; the library is still imported as `pir`.
pir-sdk = { path = "../pir", default-features = false, features = ["sdk"] }
futures-util = "0.3"
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::json;

use pir::agent::tool_batch::ToolResult;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkEvent};

// A scripted keyless provider lives in `examples/common/mod.rs`; this
// excerpt keeps the builder/stream shape of `examples/quickstart.rs`.
let mut registry = ProviderRegistry::default();
registry.set_provider(Arc::new(MockProvider::new()));

let agent = AgentBuilder::new()
    .with_registry(Arc::new(registry), mock_model())
    // An explicit system prompt suppresses AGENTS.md/CLAUDE.md discovery.
    .system_prompt("You are a tiny example agent.")
    .tool(
        "echo",
        "Echo the provided text back",
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
        |args| async move {
            let text = args["text"].as_str().unwrap_or_default();
            ToolResult::ok(format!("echoed: {text}"))
        },
    )
    .on_event("input", |payload| {
        let text = payload["text"].as_str().unwrap_or_default();
        pir::plugins::PluginEventOutcome::Input {
            action: pir::plugins::InputAction::Transform,
            text: Some(format!("{text} (transformed)")),
            images: None,
        }
    })
    .no_session()
    .no_compaction()
    .thinking_level(ThinkingLevel::Off)
    .build()
    .await?;

let mut events = agent.prompt_stream("echo hello");
while let Some(event) = events.next().await {
    match event {
        SdkEvent::MessageUpdate { delta, thinking: false } => print!("{delta}"),
        SdkEvent::ToolExecutionStart { tool_name, .. } => println!("tool started: {tool_name}"),
        SdkEvent::AgentSettled { aborted } => {
            println!("\nsettled (aborted: {aborted})");
            break;
        }
        _ => {}
    }
}

// One tool round-trip = two model requests = two loop turns.
println!("turns: {}", agent.usage_totals().turns);
agent.shutdown().await?;
```

What the example shows: `with_registry` injects the provider and model
directly (nothing consults `models.json`, the catalog, auth or the
environment); `tool` registers a closure tool (built-in coding tools exist
only after `core_tools(true)` and only with `tools` compiled in);
`on_event("input", ...)` rewrites the prompt; `prompt_stream` yields
`TurnStart`, `MessageStart`, `MessageUpdate*`, `MessageEnd`, `TurnEnd` per
turn, tool events between `MessageEnd` and `TurnEnd`, and ends with
`AgentSettled` (no `AgentOutcome`); `usage_totals().turns` counts model
round-trips (2 here), not prompts; `shutdown` aborts an in-flight run, waits
for settle and rejects later prompts.

## Builder reference

`AgentBuilder` is a consuming builder; finish with `build().await`.

| Method | Effect |
|---|---|
| `new()` | Empty builder. Model resolves from the environment; default compaction; default session directory. |
| `endpoint(Endpoint)` | Explicit endpoint; never consults configuration. Requires the matching `provider-*` feature (`Endpoint::new` defaults to 128k context / 8192 max tokens). The synthesized model declares `reasoning: false`, so the effective thinking level is always `off`. Mutually exclusive with `model*`/`with_registry`. |
| `model(pattern)` / `model_in_provider(provider, pattern)` | Resolve a bare id, `provider/id`, or fuzzy pattern (optional `:thinking` suffix) against `~/.pi/agent/models.json`, the built-in catalog, and configured auth. Mutually exclusive with `endpoint`/`with_registry`. |
| `with_registry(Arc<ProviderRegistry>, Model)` | Caller-supplied registry and model; skips environment resolution. The model must have non-zero `context_window` and `max_tokens`. The only model path usable with no `provider-*` feature. Mutually exclusive with `endpoint`/`model*`. |
| `api_key(key)` | Override. For an endpoint, `Endpoint::api_key` wins and this is the fallback for keyless endpoints; for environment-resolved models it overrides configured auth. |
| `thinking_level(level)` | Requested level; defaults to the settings default, else `medium`. The effective level is clamped to what the resolved model supports at `build()` (`off` for non-reasoning models). |
| `system_prompt(text)` | Replace the system prompt. Unset means derive the context prompt (always built; discovered context files/skills add to it). |
| `append_system_prompt(text)` | Append to the prompt with a blank line. Alone it appends to the derived context prompt (discovery stays on); combined with `system_prompt` it appends to that explicit prompt. |
| `resource_loader(DefaultResourceLoader)` | Install skills and prompt templates discovered by the supplied loader before the first run (the builder form of `CreateAgentSessionOptions::resource_loader`). Unset = hermetic, no implicit discovery; the `skills` feature is required for actual skill discovery. |
| `cwd(path)` | Working directory for tool execution and project discovery (default: process cwd). |
| `agent_dir(path)` | The agent's own config directory — the per-agent form of `PIR_AGENT_DIR` (D2). Every agent-dir-scoped read of this build (`settings.json`, `models.json`, `auth.json`, builtin gates, the `subagent` builtin's `agents/` discovery dir, the `mcp`/`codemode` builtin roots, the remote-catalog `models-store.json`, and the `SessionMode::DefaultDir` root) comes from it. Unset resolves the process environment once at `build()`, exactly like the CLI. Resolved once and build-bound (D9); see "Agent directory identity" below. |
| `max_turns(n)` | Maximum loop turns per prompt (default: 1000; one turn is one model request, not one tool call). |
| `tool(name, description, schema, closure)` | Register a tool from an async closure returning `ToolResult`. The closure receives the argument object only; use `with_tool` when the implementation needs `ToolContext` (`cwd`, abort checks, progress updates). |
| `with_tool(Arc<dyn AgentTool>)` | Register a typed `AgentTool` implementation; `run` receives `&ToolContext` (`cwd`, `is_aborted()`, `update_channel()`, nested-call and session environment handles). |
| `core_tools(bool)` | Register the built-in coding tools (read/write/edit/bash/…; requires the `tools` feature or an individual `tool-*` leaf). Disabled by default. A build with no built-in tool compiled fails `build()` with the missing-feature diagnostic instead of silently registering nothing. |
| `tools([...])` | Allowlist with CLI semantics: plain names and `*` globs replace the default set (`?` is a literal byte, matching is case-sensitive); a pure `+name`/`-name` modifier list adds to/removes from it. The two forms must not be mixed. |
| `exclude_tools([...])` | Denylist applied on top of the default set (and any `tools` allowlist); unmatched entries are ignored. |
| `no_tools()` | Declare and execute no tools. |
| `on_event(name, closure)` | One plugin per call: observe/transform a unified-dispatch event (`input`, `before_agent_start`, `session_start`, `model_select`, `thinking_level_select`, `turn_end`, `agent_before_settle`, `session_before_compact`/`fork`/`switch`, …). The closure receives `&mut serde_json::Value` and may return `()` or a `PluginEventOutcome`. Handlers run in registration order. |
| `with_plugin(Arc<dyn AgentPlugin>)` | Attach a full Rust plugin (tools, commands, event handlers). |
| `builtin(name)` / `builtins([...])` | Attach a compiled-in builtin (`"subagent"`, `"tool-repair"`, `"tool-search"`, `"todo"`, `"mcp"`, `"python"`, `"codemode"`). `builtin:`-prefixed and bare names are equivalent. |
| `extension_paths(paths)` | Load JS extensions from files or `builtin:<name>` pseudo-paths; requires the `extensions` feature. |
| `only_builtin(bool)` | Filter `extension_paths` down to `builtin:<name>` entries: on-disk paths are ignored with one diagnostic and no QuickJS actor starts (the CLI `--only-builtin` shape). It does not inject the default builtin set; attach those with `builtin()`/`builtins()`. |
| `proxy(ProxyProvider, Model)` | Route requests through an application proxy server. Registers the provider in a private registry and pins the model exactly like `with_registry` (which it is mutually exclusive with, along with `endpoint`/`model*`). |
| `session(SessionMode)` | `DefaultDir` (default), `Dir(path)`, `Existing(path)`, or `None`. Durable modes write the upstream JSONL format (pi and pir share session files). |
| `no_session()` | Shorthand for `SessionMode::None` (in-memory only). |
| `compaction(config)` / `no_compaction()` | Tune or disable threshold compaction. |
| `retry(max_retries, max_retry_delay_ms)` | Transport retry override; `None` fields keep the transport defaults. |
| `tool_hooks(ToolHooks)` / `shared_tool_hooks(Arc<ToolHooks>)` | Install the per-tool `beforeToolCall`/`afterToolCall` bundle for every run; the shared form is reusable across agents and replaceable at runtime with `Agent::set_tool_hooks`. |
| `build()` | Resolve the model and assemble the agent. All model/limit/builtin/extension validation failures surface here. |

Model setup is exclusive: a builder carrying more than one of `endpoint`,
`model`/`model_in_provider`, and `with_registry` fails `build()` with a
"conflicting model configuration" error. `endpoint` and `with_registry` also
reject zero `context_window`/`max_tokens`.

### Agent directory identity

`agent_dir(path)` is the per-agent equivalent of `PIR_AGENT_DIR` and the
reason two agents with **different** agent dirs can run in one process (D2):

```rust,no_run
# async fn example() -> Result<(), pir::sdk::SdkError> {
let a = pir::sdk::AgentBuilder::new().agent_dir("/tmp/agent-a").no_session().build().await?;
let b = pir::sdk::AgentBuilder::new().agent_dir("/tmp/agent-b").no_session().build().await?;
assert_ne!(a.agent_dir(), b.agent_dir());
# Ok(())
# }
```

The directory is resolved **once**, at `build()`: the builder's value, else
`$PIR_AGENT_DIR` / `$PIR_CODING_AGENT_DIR` / `$PI_CODING_AGENT_DIR`, else
`~/.pi/agent`. `Agent::agent_dir()` reports it. These reads use it instead of
the process environment: global `settings.json`, `models.json`, `auth.json`,
the settings-driven builtin gates (`pythonTool`, `disabledBuiltinExtensions`),
the `subagent` agents dir, `mcp`'s log/auth/global config, `codemode` config,
the remote-catalog `models-store.json` cache, and the `DefaultDir` session
root. `!command` results are keyed by `(agent dir, command)`; every other
cache stays process-global by design.

Two limits are deliberate and documented rather than fixed:

- **Build-bound.** A later `PIR_AGENT_DIR` change never re-points a live
  agent. `Agent::session_dir()` in particular stays where it was built: a
  session belongs to the directory (and cwd) it was created under (D9).
  Build a new agent to change either; `attach_session(path)` still opens an
  explicit file anywhere.
- **Process-global switches stay global.** Environment variables (`PI_OFFLINE`,
  `PI_CATALOG_BASE_URL`, the `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` Claude Code
  compatibility layer, `PIR_CODEMODE_MEMORY_MB`, provider API-key variables)
  describe the process, not the agent; the explicit-directory environment
  loader does not re-apply the Claude Code compatibility layer.

## Events

`Agent::prompt_stream` yields `SdkEvent`s through a bounded channel.
Intermediate-event delivery is best-effort: a slow consumer never blocks the
run, and when the channel is full the event is dropped. Terminal events
(`AgentSettled`, `AgentOutcome`, and a rejected prompt's
`PromptRejected`/`AgentSettled` pair) are sent with backpressure after the
run settles, so a live receiver always observes them. The enum is
`#[non_exhaustive]`.

The table below is the complete variant list for this tree (33 variants).
"Turn" fields are one-based indexes of the low-level agent-loop run; the
session may run the loop more than once inside one prompt (`agent_before_settle`
continuation, overflow recovery), and `AgentSettled` closes the whole prompt
run, not each loop run.

| Event | Payload |
|---|---|
| `TurnStart` | `turn_index`. |
| `TurnEnd` | `turn_index`, `message` (the assistant message that completed the turn), `tool_results` (this turn's tool-result messages in source order; empty for a turn without tool calls). |
| `MessageStart` | — (the assistant message stream started). |
| `MessageUpdate` | `delta`, `thinking` (`true` for reasoning deltas). |
| `MessageEnd` | `message`: the completed `AssistantMessage`. |
| `TranscriptMessageStart` | `turn` (`0` = the run's starting prompt/preamble), `message`: a non-assistant (system/user/developer/toolResult/custom) `Message` entering the transcript. |
| `TranscriptMessageEnd` | `turn`, `message`: the finalized non-assistant message after handler replacement, if any. |
| `ToolCallStart` | `turn`, `content_index`: a tool-call block started streaming. |
| `ToolCallDelta` | `turn`, `content_index`, `delta`: one raw JSON argument fragment. |
| `ToolCallEnd` | `turn`, `content_index`, `tool_call`: the complete parsed `ToolCall`. |
| `MessageDone` | `turn`, `reason` (`TerminalReason`), `message`: the provider's completed assistant message; emitted before `MessageEnd`. |
| `MessageError` | `turn`, `reason` (`ErrorReason`), `message`: the provider stream failed (`error`/`aborted`); emitted before `MessageEnd`. |
| `ToolExecutionStart` | `call_id`, `tool_name`, `parent_call_id`, `args` (the model's argument object; `null` when unavailable). |
| `ToolExecutionUpdate` | `tool_call_id`, `tool_name` (`Option`), `parent_call_id`, `args`, tool-defined `partial` JSON (`{content, details, isError}`). |
| `ToolExecutionEnd` | `call_id`, `tool_name`, `is_error`, `duration_ms`, `parent_call_id`, `result` (upstream `AgentToolResult` wire shape). |
| `Compaction` | — (threshold compaction ran). |
| `QueueUpdate` | `steering: Vec<String>`, `follow_up: Vec<String>`: the full pending texts after a push, drain, or clear. |
| `CompactionStart` | `reason`: `"manual"`, `"threshold"`, or `"overflow"`. |
| `CompactionEnd` | `reason`, `result: Option<Value>` (upstream `CompactionResult` wire shape), `aborted`, `will_retry`, `error_message: Option<String>`. |
| `EntryAppended` | `entry`: the appended `SessionEntry` in its upstream wire shape. |
| `SessionInfoChanged` | `name: Option<String>` (`None` when cleared). |
| `ThinkingLevelChanged` | `level`: the new effective level in wire form (`"low"`, `"high"`, ...). |
| `BashExecutionUpdate` | `id: Option<String>`, `delta`: one streaming chunk of an out-of-loop bash execution. |
| `AutoRetryStart` | `attempt`, `max_attempts`, `delay_ms`, `error_message`. |
| `AutoRetryEnd` | `success`, `attempt`, `final_error: Option<String>`. |
| `SummarizationRetryScheduled` | `attempt`, `max_attempts`, `delay_ms`, `error_message`. |
| `SummarizationRetryAttemptStart` | `source` (`"branchSummary"`/`"compaction"`), `reason: Option<String>` (the compaction trigger). |
| `SummarizationRetryFinished` | —. |
| `BackgroundToolEnd` | `call_id`, `tool_name`, `parent_call_id`, `is_error`, `duration_ms`, `output`, `images: Vec<ImageContent>`, `details: Option<Value>`, `structured_content: Option<Value>`, `usage: Option<Usage>`. |
| `PromptRejected` | `message`: why the prompt never started. |
| `AgentEnd` | `messages`: every message that low-level loop run appended, in order; `will_retry`: whether the session will re-run the loop. Not terminal. |
| `AgentOutcome(Box<AgentOutcome>)` | The complete terminal `AgentOutcome`; only `prompt_stream_with_outcome`, immediately before `AgentSettled`. |
| `AgentSettled` | `aborted`: terminal event; no more events follow. |

A one-turn text run is:

```text
TurnStart → MessageStart → MessageUpdate* → MessageDone → MessageEnd → TurnEnd → AgentEnd → AgentSettled
```

A tool-calling turn inserts `ToolExecutionStart → ToolExecutionUpdate* →
ToolExecutionEnd` between `MessageEnd` and `TurnEnd`; the provider's
incremental argument frames arrive as `ToolCallStart → ToolCallDelta* →
ToolCallEnd` during the message stream. The next round-trip then starts with a
new `TurnStart`. `Compaction` is emitted before the affected `TurnStart`. A
prompt that never starts yields `PromptRejected` followed by `AgentSettled`,
and `prompt_stream` always terminates with `AgentSettled` (terminal events
bypass a `prompt_stream_with` filter).

The session-level variants (`QueueUpdate`, `CompactionStart`/`CompactionEnd`,
`EntryAppended`, `SessionInfoChanged`, `ThinkingLevelChanged`,
`BashExecutionUpdate`, `BackgroundToolEnd`, and the three summarizer retry
events) are projections of the public `SessionEvent` type via
`SdkEvent::from_session_event`; `AutoRetryStart`/`AutoRetryEnd` project the
transport retry lifecycle. `SessionEvent::AgentSettled` maps to `None` here:
the SDK stream's one terminal `AgentSettled` is emitted by the run driver.

`AgentSettled` is the **session** settle, not the end of one low-level loop
run (upstream `agent_settled` vs `agent_end`): when a `agent_before_settle`
handler (or overflow recovery) makes the session issue another provider
request, those events are streamed first and exactly one `AgentSettled` closes
the stream afterwards. The terminal outcome of
`prompt_stream_with_outcome` likewise belongs to the final loop run.

The stream exposes events only. Tool result text is not a dedicated event;
`ToolExecutionEnd.result` carries the finished `AgentToolResult` wire value
and a tool that reports progress can surface it through
`ToolExecutionUpdate.partial`. For a typed terminal outcome and the
tool-result messages, use `prompt` or `prompt_stream_with_outcome`.

### Event subscription

A prompt stream observes events only while that prompt runs. For events
between runs, or when you drive the agent with `prompt()` (which has no
stream), subscribe on the agent:

```rust
use pir::sdk::{SessionEvent, SdkEvent};

agent.on_session_event(|event| {
    // Raw session event (same values a raw `AgentSession` emits).
    if matches!(&event, SessionEvent::EntryAppended { .. }) { /* mirror */ }
    // Typed SDK projection; `None` only for `SessionEvent::AgentSettled`.
    let _ = SdkEvent::from_session_event(&event);
});
```

Handlers run synchronously on the emitting task; hand heavy work to a channel
or thread. The subscription lives for the session's lifetime. With a raw
`AgentSession`, use `AgentSession::on_session_event` directly.

## Agent reference

`Agent` is cheap to clone; clones share one session. Prompt runs are
serialized by the session — queue work with `steer`/`follow_up` instead of
starting a concurrent `prompt`.

| Method | Effect |
|---|---|
| `prompt(text)` / `prompt_with_images(text, images)` | Run one prompt and return `AgentOutcome`; no `SdkEvent` stream. Images are appended to the starting user message. |
| `prompt_boxed(text)` | `prompt` with an explicitly boxed `Pin<Box<dyn Future<...> + Send + '_>>`; use it when you need the box in your own signature. |
| `prompt_stream(text)` | Return `impl Stream<Item = SdkEvent>` (boxed, `Unpin`). Dropping the stream detaches the observer; the run continues. |
| `prompt_stream_with(text, filter)` | Same, dropping intermediate events for which `filter(&SdkEvent)` is `false`. The filter runs on the loop thread; terminal events and the out-of-loop session-level events (`queue_update`, compaction lifecycle, entry/session/thinking/retry/background) bypass it. |
| `prompt_stream_with_outcome(text)` | Like `prompt_stream`, plus `SdkEvent::AgentOutcome` immediately before `AgentSettled`. |
| `prompt_stream_with_images(text, images)` | Like `prompt_stream`, with image attachments. |
| `set_event_channel_capacity(n)` / `event_channel_capacity()` | Tune (clamped to at least 1) or read the bounded stream channel capacity used by the next run (default 1024). |
| `steer(text)` / `follow_up(text)` | Queue a steering message (delivered before the next model request) or a follow-up (delivered after the current turn sequence). |
| `abort()` | Abort the in-flight run; it settles aborted. No-op between runs. |
| `compact()` | Compact now, returning `CompactionResult`. |
| `is_streaming()` / `is_idle()` / `wait_for_idle()` | Run state; `wait_for_idle` polls until no prompt run is in flight. |
| `pending_message_count()` | Queued steering plus follow-up messages not yet delivered. |
| `tools()` | The tools the next run declares (`ToolInfo { name, description, parameters }`), sorted; hidden/deferred tools are excluded. |
| `take_notifications()` | Drain presentation-class extension notifications (headless hosts). |
| `on_session_event(handler)` | Subscribe to the out-of-loop `SessionEvent`s (`QueueUpdate`, `CompactionStart`/`CompactionEnd`, `EntryAppended`, `SessionInfoChanged`, `ThinkingLevelChanged`, `BashExecutionUpdate`, retries, `BackgroundToolEnd`, `AgentSettled`); project with `SdkEvent::from_session_event`. See "Event subscription". |
| `session_path()` / `session_id()` / `session_name()` / `set_session_name(Option<&str>)` | Live session identity and display name. |
| `session_dir()` / `agent_dir()` | Build-bound session root and agent config directory. |
| `new_session()` / `attach_session(path)` / `continue_session()` / `reset()` | Start fresh storage, resume an explicit file, resume the directory's most recent session, or replace the transcript with an empty in-memory one. All require session storage and an idle agent (except `reset`, which also requires storage); usage totals follow the switch. |
| `transcript()` / `transcript_json()` / `ephemeral_transcript()` | Current branch messages (or the in-memory history when there is no storage). |
| `is_paged_session()` / `paged_window()` | Whether a large attached session holds only a resident tail window, and that window's `PagedWindow` bounds. |
| `transcript_cursor()` / `older_transcript_messages(cursor, bytes, entries)` / `next_older_page(cursor)` / `older_transcript_json(...)` | Walk the skipped prefix of a paged session one bounded page at a time. |
| `session_stats()` / `context_usage()` | Whole-session token/cost/message totals (`SessionStats`; paged prefix included without widening) and current `ContextUsage`. |
| `clear_queue()` / `queued_messages()` / `steering_messages()` / `follow_up_messages()` | Drain or peek the queued steering/follow-up messages; `clear_queue` returns `ClearedQueue`. |
| `steering_mode()` / `set_steering_mode(mode)` / `follow_up_mode()` / `set_follow_up_mode(mode)` | Queue delivery-order mode (`SteeringMode` / `FollowUpMode`) for steering and follow-ups. |
| `commands()` / `dispatch_command(name, args)` | List native plugin slash commands and dispatch one (falls back to the JS runtime); returns `CommandOutcome`. |
| `usage_totals()` | `UsageSnapshot { turns, total_tokens }` for the current session. |
| `model()` | `ModelRef { provider, id }`. |
| `thinking_level()` | Effective (clamped) thinking level. |
| `set_model(model)` / `set_thinking_level(level)` | Switch for subsequent runs; the effective thinking level is clamped to what the model supports (also after `set_model`). |
| `set_tool_hooks(hooks)` / `tool_hooks()` | Read or replace the per-tool `ToolHooks` bundle installed for subsequent runs (`None` clears). |
| `shutdown()` | Abort, wait up to 2 s for the run to settle, mark the agent shut down, and run plugin `on_shutdown` hooks. Later prompts are rejected. |

`AgentOutcome` carries `message` (terminal `AssistantMessage`), `tool_results`,
`messages` (every message the run appended, upstream `agent_end.messages`),
`usage` (terminal message only), `turns`, and `aborted`. Helpers:
`message_text()` and `tool_result_texts()`.

`ToolInfo` is `{ name, description, parameters: Option<Value> }`.
`UsageSnapshot::turns` counts loop turns, i.e. model round-trips across every
prompt run — one prompt that calls a tool and then gets a final answer reports
`2`. It is not a prompt counter.

## Errors

Every fallible SDK entry point returns the typed, `#[non_exhaustive]`
`SdkError` surface:

| Variant | Meaning |
|---|---|
| `NoModel { message }` | Model resolution failed (no endpoint, nothing in `models.json`/catalog/auth). |
| `Provider(AiError)` | The AI layer rejected a request (unknown provider/model, auth, transport, non-terminated stream). |
| `Io(std::io::Error)` | A filesystem or transport I/O error (the display text is the I/O error's own; the original chain stays reachable as `source()`). |
| `Session(String)` | Session storage failed to open, create, or persist entries (the run-path precondition rejections "already streaming" / "compaction is in progress" stay `Other`). |
| `Extension(String)` | An on-disk extension failed to load or the JS runtime failed to start at build. |
| `Aborted` | The run aborted before producing assistant content; later aborts return the partial `AgentOutcome` with `aborted: true`. |
| `Unsupported(String)` | A requested capability is not compiled into this build (for example a `create_*_tool` factory whose `tool-*` feature is off); the message names the missing feature. |
| `Other(String)` | Anything the facade cannot classify (including rejected concurrent prompts and plugin registration failures). |

## Embedding without the full agent

The provider layer is public and usable on its own; `AgentBuilder` only wires
it into the loop. You can drive a provider directly:

```rust
use std::sync::Arc;
use futures_util::StreamExt;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Context, Model, StreamOptions};

async fn run_once(registry: &ProviderRegistry, model: Model, context: Arc<Context>) {
    // `stream_unchecked` skips auth resolution; `stream` resolves auth and
    // `complete` also drains to the final message.
    let mut stream = registry.stream_unchecked(model, context, StreamOptions::default());
    while let Some(event) = stream.next().await { let _ = event; }
}
```

- `ProviderRegistry::{stream, stream_unchecked, complete}` accept
  `impl Into<Arc<Context>>`; pass an `Arc<Context>` to avoid copying the
  transcript.
- `Provider::stream_shared(model, Arc<Context>, options)` is the shared-
  transcript boundary. The default implementation materializes the owned
  context once (zero-copy when it holds the last handle); every bundled wire
  adapter overrides it and reads the shared context directly.
- `pir::agent::agent_loop::ProviderRequest.context` is `Arc<Context>`, and
  `context_into_owned` is the identity function.
- `complete(model, context, options)` returns `AiResult<Arc<AssistantMessage>>`.
- The full session facade is also public
  (`pir::agent_session::AgentSession::new(AgentSessionConfig)`), but the SDK
  is the supported embedding surface.

## Extending the agent

Every extension path lands on one plugin host: closure/typed tools, inline
`AgentPlugin`s, compiled-in builtins, and JS extensions share one registry and
one ordered event dispatch.

```rust
use std::sync::Arc;
use pir::plugins::{AgentPlugin, PluginInfo, RegistrationContext};
use pir::sdk::AgentBuilder;

struct MyPlugin { info: PluginInfo }

impl AgentPlugin for MyPlugin {
    fn info(&self) -> &PluginInfo { &self.info }
    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        // ctx.register_tool(...), ctx.register_command(...), ...
        Ok(())
    }
}

let builder = AgentBuilder::new()
    .with_plugin(Arc::new(MyPlugin { info: PluginInfo::inline("my-plugin") }));
```

Builtins resolve through the same registry as the CLI's `-e builtin:<name>`
path. A name that cannot be attached fails `build()` — never a silent no-op:

```text
Unknown built-in extension: builtin:<name>
Built-in extension `builtin:python` is not compiled into this build (the `python` feature is off)
```

The agent directory's settings gates apply here too: a requested name that
`pythonTool: false` or `disabledBuiltinExtensions` disables fails `build()`
with a diagnostic naming the setting (the same rule the extension plan
follows), so settings can never be bypassed by attaching a builtin by name.

`.builtin("builtin:subagent")` and `.builtin("subagent")` are equivalent.

`on_event` is the single-event shortcut: one closure plugin per call, stacked
in registration order. It receives the same `serde_json::Value` payloads as a
full plugin. Rust handlers observe `tool_execution_update` and the
session-level events listed in the builder table; the loop-level
`tool_call`/`tool_result` gate currently dispatches through the JS-extension
runtime only.

### Public value types

| Type | Fields / variants |
|---|---|
| `Endpoint` | `base_url`, `api_key`, `api` (`ApiKind`), `model_id`, `context_window`, `max_tokens`; `Endpoint::new` defaults to 128k / 8192. |
| `ApiKind` | `OpenAiCompletions`, `AnthropicMessages`, `GoogleGenerativeAi`; `wire_id()` returns the models.json `api` id. |
| `SessionMode` | `DefaultDir`, `Dir(path)`, `Existing(path)`, `None`. |
| `ToolInfo` | `name`, `description`, `parameters`. |
| `UsageSnapshot` | `turns`, `total_tokens`. |
| `ModelRef` | `provider`, `id` (`Display` prints `provider/id`). |
| `AgentOutcome` | `message`, `tool_results`, `messages`, `usage`, `turns`, `aborted`. |
| `ClearedQueue` | The messages drained by `Agent::clear_queue()`. |
| `CompactionResult` | Summary, `tokens_before`, `first_kept_index` / `first_kept_entry_id`, `retained_tail`, summarizer `usage`, and the summarized file lists. |
| `PagedWindow` | `page_start`, `file_bytes`, `parsed_entries`: the resident tail window of a paged session. |
| `CommandOutcome` | `ok` plus the command's display text in `error` (native commands use it for success text too). |
| `Notification` | A presentation or actionable extension notification drained by `Agent::take_notifications()`. |
| `SteeringMode` / `FollowUpMode` | Queue delivery-order modes. |
| `SessionStats` / `ContextUsage` | Whole-session aggregates and current context-window usage. |

## Session-level and runtime surface

`AgentBuilder` is the ergonomic single-agent path. The same SDK also exposes
the raw upstream session objects and factories an application that owns
session lifecycle needs (all of them live in `pir::sdk`, no internal modules
required):

| Item | Effect |
|---|---|
| `AgentSession` / `AgentSessionConfig` / `SessionEvent` / `SessionEventHandler` | The raw session object (the same one the CLI hosts and JS extensions use), its config, and its subscribe API. |
| `create_agent_session(CreateAgentSessionOptions)` | Resolve services (settings, model registry, storage, resource loader) and return `CreateAgentSessionResult { session, model, thinking_level, resources, model_fallback_message }`. |
| `create_agent_session_services` / `create_agent_session_from_services` | Split the resolution and build halves when several sessions share one environment. |
| `create_agent_session_runtime` / `AgentSessionRuntime` | Build a session plus the lifecycle wrapper: `new_session`, `switch_session`, `reload` (all gated on an idle session) and `dispose` (extension-runtime teardown). |
| `ModelRegistry` / `SettingsManager` / `SessionManager` | Model lookup + configured-auth filtering, settings snapshots, and session storage; `ModelRegistry::from_environment_in` / `SessionManager` scoped to an explicit agent directory. |
| `DefaultResourceLoader` / `ResourceLoader` | Skills and prompt-template discovery, including `resources_discover` contributions; attach through `CreateAgentSessionOptions::resource_loader` or `AgentBuilder::resource_loader`. |
| `DefaultPackageManager` | Installed-package resource paths. |
| `EventBus<T>` / `create_event_bus` | A native typed publish/subscribe bus, independent of the agent session event stream. |
| `create_coding_tools`, `create_read_only_tools`, `create_tool`, `create_read_tool`, `create_bash_tool`, `create_edit_tool`, `create_write_tool`, `create_ls_tool`, `create_grep_tool`, `create_find_tool`, `create_powershell_tool`, `has_core_tool` | The exact built-in tool objects the CLI registers; each factory returns `SdkError::Unsupported` naming the missing `tool-*` feature when compiled out. |

```rust,no_run
# async fn example() -> Result<(), pir::sdk::SdkError> {
use std::sync::Arc;

use pir::sdk::{create_agent_session, CreateAgentSessionOptions};

let created = create_agent_session(CreateAgentSessionOptions {
    no_session: true,
    ..Default::default()
})
.await?;

// The raw session is driven directly; `AgentEvent` is the loop event type.
let terminal = created
    .session
    .prompt("Summarize the repository.", Arc::new(|_event| {}))
    .await?;
let _ = terminal;
# Ok(())
# }
```

Known gaps, documented rather than faked: the raw factories do not repeat
`Agent::attach_session`'s "restore the model recorded on an existing session"
step — pass the storage and set the model explicitly, or use
`Agent::attach_session`; and JS `withSession` callbacks are a host/JS bridge
feature, while a Rust embedder calls `AgentSessionRuntime::new_session` /
`switch_session` and then runs its own setup code.
`CreateAgentSessionOptions::max_turns` keeps the raw-factory compatibility
default of 100; `AgentBuilder::max_turns` defaults to 1000. Set it explicitly
when the raw path must use the higher cap.

## Proxied requests

`ProxyProvider` routes every model request through an application server that
owns the provider credentials — the Rust equivalent of upstream `streamProxy`
(`POST {proxy_url}/api/stream` with a bearer token, SSE proxy events decoded
with client-side partial reconstruction):

```rust,no_run
use pir::sdk::{AgentBuilder, ProxyEndpoint, ProxyProvider};

# let model: pir::ai::types::Model = unimplemented!();
let provider = ProxyProvider::new(
    ProxyEndpoint::new("https://genai.example.com", "proxy-token"),
    vec![model.clone()],
)?;
let agent = AgentBuilder::new().proxy(provider, model).no_session().build().await?;
```

| Item | Effect |
|---|---|
| `ProxyEndpoint { proxy_url, auth_token }` / `stream_url()` | Proxy coordinates; requests go to `{proxy_url}/api/stream`. |
| `ProxyStreamOptions::new(url, token)` / `with_stream_options(..)` / `with_client(..)` | Direct `stream_proxy` call options: local `StreamOptions` (abort, retries) plus an optional transport client. |
| `stream_proxy(model, context, options)` | Stream one request through the proxy (upstream `streamProxy`). |
| `ProxyProvider::new(endpoint, models)` / `with_client(..)` / `endpoint()` | A keyless `Provider` registering the served models; `AgentBuilder::proxy` is the one-call path. |
| `PROXY_CONNECTION_CLOSED_MESSAGE` | The verbatim error text reported when the server closes without a terminal event. |

## Feature flags

A minimal embed builds with `default-features = false, features = ["sdk"]`;
add `"session"` (durable transcripts, `Agent::attach_session`, paged reads) and
`"skills"` when the embed needs them. `pir::sdk` is **not** part of the bare
core: without the `sdk` feature the module is not compiled at all (no stub).
The core — agent loop, provider framework (registry, resolution, catalog,
transport), session types/config, prompt/skills rendering and plugin
framework — has no heavy dependencies. Built-in tools are the separate
aggregate `tools` feature (without it a `cli` binary is toolless and
`core_tools(true)` fails `build()`); `sdk` adds no extra dependencies. `full`
is the default; the `pir` binary requires only `cli`, which does **not** imply
`tools`.

SDK-relevant features: `sdk` (facade; no stub), `session` (upstream-format
JSONL persistence and the tail-page reader), `skills` (resource-loader skill
discovery), `tools` (aggregate built-ins), `provider-*` (wire adapters;
`with_registry` works without any), `subagent`, `tool-repair`,
`tool-search`, `todo`, `mcp`, `python`, `extensions`, `grep-find`,
`image-tools`. Without a `provider-*` feature, `with_registry` still works;
`endpoint()` fails `build()` naming the missing feature; environment
resolution can select a catalog model but its first request ends with the
feature-naming error.

Recommended combinations:

| Shape | Features |
|---|---|
| Custom `Provider` + in-memory agent (hermetic, tests) | `sdk` |
| Durable sessions (`attach_session`, paging) | `sdk,session` |
| Skills / prompt templates through `resource_loader` | `sdk,session,skills` |
| Explicit HTTP `Endpoint` (OpenAI-compatible) | `sdk,provider-openai` |
| Built-in coding tools | `sdk,tools` (or individual `tool-*` leaves) |
| Compiled-in builtins | `sdk,subagent` / `sdk,tool-repair` / `sdk,tool-search` / `sdk,todo` |
| JS extensions / `codemode` | `sdk,extensions` |
| Full CLI-equivalent surface | `full` (an embed rarely needs it) |

There is no standalone `sqlite` feature: the node:sqlite/better-sqlite3 host
shims ride `extensions`. `tui`, `env`, `server`, `cli`, and `mcp-sd` are host
features an embed does not need. [`docs/FEATURES.md`](FEATURES.md) has the full
matrix, gating map and measured dependency weights.

## Packaging and size

The crates.io package is `pir-sdk`; the library is imported as `pir` and the
binary is `pir`. The package ships `src/**`, `assets/**`, `examples/**`,
`benches/**`, `schemas/*.json`, `README.md`, `CHANGELOG.md`, `docs/sdk.md` and
the license files; every other `docs/**` page is excluded (see `Cargo.toml`).
`cargo package --list --no-verify` inspects the exact `.crate` file list.

Measured Windows x64 executables, one consumer (Rust std + tokio + serde_json
included), fat LTO, one CGU, symbols stripped:

| Embed | opt-level 3 | opt-level `"s"` |
|---|---:|---:|
| `sdk` only, no custom tool | 12.11 MiB | **8.67 MiB** |
| `sdk,session,skills` + one tool | 12.63 MiB | **9.15 MiB** |
| above + `provider-openai` + `Endpoint` | 13.99 MiB | **10.11 MiB** |

A custom tool costs ~6.5 KiB. `opt-level = "s"` is the recommended default for
embeds: it saves ~28% with no measurable startup cost, and the agent loop is
provider-I/O-bound. The numbers include the consumer's std/tokio; packaging
facts and the external-consumer checks live in
`logs/lane-sdk-package/REPORT.md`. Copy-paste profile:

```toml
[profile.release]
opt-level = "s"
lto = "fat"
codegen-units = 1
strip = "symbols"
```

Keep the feature set minimal; `full` is for CLI parity, not embeds. If the
application already builds with a different profile, matching
`lto`/`codegen-units` matters more than `opt-level` for the `pir` code itself.

## Verification

```bash
cargo check --no-default-features --features sdk --test sdk_consumer_embed
cargo check --no-default-features --features sdk --test sdk_consumer_surface
cargo check --all-targets --all-features   # examples included
```

`tests/sdk_consumer_embed.rs` is the provider/registry/session contract test;
`tests/sdk_consumer_surface.rs` pins the documented builder/agent methods and
event/error variants; `tests/sdk_extensibility.rs` covers tool selection,
`on_event`, stream filtering and model/thinking switches; `tests/suites/sdk.rs`
covers builder resolution, event ordering, tools, plugins, sessions and error
mapping. `scripts/run_examples.sh` runs the offline example set.
