# SDK

`pir::sdk` is the curated Rust embedding surface: **build** an agent (explicit endpoint, the environment, or your
own `ProviderRegistry` + `Provider` — the only path needing no `provider-*` feature), **extend** it
(closure/typed tools, inline `AgentPlugin`s, `on_event`, compiled-in builtins), and **run** it (`prompt` →
`AgentOutcome`, or `prompt_stream` → typed bounded `SdkEvent` stream).

## Quickstart

One complete offline program: a custom `Provider`, one prompt, `shutdown` — no network, API key, `~/.pi` state,
or `provider-*` feature, because `with_registry` dispatches straight to your provider. The provider is
[`examples/common/mod.rs`](../examples/common/mod.rs) (copy it next to `main.rs`, replace
`ScriptedProvider::stream` with your backend); this is [`examples/quickstart.rs`](../examples/quickstart.rs).

```toml
[dependencies]
# Package `pir-sdk`; the library is still imported as `pir`.
pir-sdk = { path = "../pir", default-features = false, features = ["sdk"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
```

```rust
mod common; // copy examples/common/mod.rs next to this file

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

#[tokio::main]
async fn main() -> Result<(), SdkError> {
    let (registry, model) = common::ScriptedProvider::registry(vec![common::text_message(
        "Hello from the pir SDK.",
    )]);

    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a tiny offline example agent.")
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    let outcome = agent.prompt("Say hello.").await?;
    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    println!(
        "run: turns={} aborted={} tokens={}",
        outcome.turns, outcome.aborted, outcome.usage.total_tokens
    );

    agent.shutdown().await?;
    Ok(())
}
```

`with_registry` skips `models.json`, the catalog, auth, and the environment. `no_session()` keeps the run in
memory; `no_compaction()` and `thinking_level(Off)` make one request and one answer; `prompt()` returns an
`AgentOutcome`; `shutdown()` aborts an in-flight run and rejects later prompts.

Next steps, each a runnable example in this repo:

| Next step | Example | Command |
|---|---|---|
| Tools | [`tools.rs`](../examples/tools.rs) | `cargo run --example tools --no-default-features --features "sdk,tools"` |
| Streaming | [`streaming.rs`](../examples/streaming.rs) | `cargo run --example streaming --no-default-features --features "sdk"` |
| Sessions + plugins | [`sessions.rs`](../examples/sessions.rs), [`plugin.rs`](../examples/plugin.rs) | `cargo run --example sessions --no-default-features --features "sdk,session"`<br>`cargo run --example plugin --no-default-features --features "sdk"` |
| Session lifecycle across processes | [`session_lifecycle.rs`](../examples/session_lifecycle.rs) | `cargo run --example session_lifecycle --no-default-features --features "sdk,session" -- start <root>` (then `continue` / `fork` / `inspect <jsonl>`) |
| Manual compaction | [`session_compaction.rs`](../examples/session_compaction.rs) | `cargo run --example session_compaction --no-default-features --features "sdk,session"` |
| A real repair with built-in tools | [`mini_coding.rs`](../examples/mini_coding.rs) | `PIR_PYTHON=<python> cargo run --example mini_coding --no-default-features --features "sdk,tools"` |

`scripts/run_examples.sh` runs the whole offline set; `examples/README.md` lists every example, including the two
network ones.

## Builder reference

`AgentBuilder` is a consuming builder; finish with `build().await`.

| Method | Effect |
|---|---|
| `new()` | Empty builder. Model resolves from the environment; default compaction; default session directory. |
| `endpoint(Endpoint)` | Explicit endpoint; never consults configuration. Requires the matching `provider-*` feature (`Endpoint::new` defaults to 128k context / 8192 max tokens). The synthesized model declares `reasoning: false`, so the effective thinking level is always `off`. No system-prompt discovery (see note). Mutually exclusive with `model*`/`with_registry`. |
| `model(pattern)` / `model_in_provider(provider, pattern)` | Resolve a bare id, `provider/id`, or fuzzy pattern (optional `:thinking` suffix) against `~/.pi/agent/models.json`, the built-in catalog, and configured auth. System-prompt discovery is on by default (CLI parity). Mutually exclusive with `endpoint`/`with_registry`. |
| `with_registry(Arc<ProviderRegistry>, Model)` | Caller-supplied registry and model; skips environment resolution. The model must have non-zero `context_window` and `max_tokens`. The only model path usable with no `provider-*` feature. No system-prompt discovery (see note). Mutually exclusive with `endpoint`/`model*`. |
| `api_key(key)` / `thinking_level(level)` | Request overrides: `api_key` is the keyless-endpoint fallback (`Endpoint::api_key` wins) or overrides configured auth / requested level defaults to the settings default, else `medium`, and is clamped to what the resolved model supports at `build()` (`off` for non-reasoning models). |
| `system_prompt(text)` / `append_system_prompt(text)` | Replace the system prompt (unset derives the context prompt, always built; discovered context files/skills add to it), or append with a blank line. An explicit `system_prompt` pins the whole prompt: no discovery and no `PIR_SYSTEM_PROMPT_FILE`. Alone, append targets the derived prompt; combined, that explicit prompt. |
| `discover_system_prompt(bool)` | Toggle ambient `SYSTEM.md`/`APPEND_SYSTEM.md` discovery and the `PIR_SYSTEM_PROMPT_FILE` read. Default on for `model*`; off for `endpoint`/`with_registry`/`proxy` and explicit `system_prompt_files`. |
| `system_prompt_files(SystemPromptFiles)` | Explicit `SystemPromptFiles { system, append }` for the derived prompt. `system` beats `PIR_SYSTEM_PROMPT_FILE`/discovered `SYSTEM.md`; discovered `APPEND_SYSTEM.md` precedes `append`; explicit `system_prompt` still wins. |
| `resource_loader(DefaultResourceLoader)` | Install skills and prompt templates discovered by the supplied loader before the first run (the builder form of `CreateAgentSessionOptions::resource_loader`). Unset = hermetic, no implicit discovery; the `skills` feature is required for actual skill discovery. |
| `cwd(path)` / `agent_dir(path)` | Working directory for tool execution and project discovery (default: process cwd) / the agent's own config directory — the per-agent form of `PIR_AGENT_DIR` (D2) and source of every agent-dir-scoped read; unset resolves the process environment once at `build()`; resolved once and build-bound (D9). See "Agent directory identity". |
| `max_turns(n)` | Maximum loop turns per prompt (default: 1000; one turn is one model request, not one tool call). |
| `tool(name, description, schema, closure)` / `with_tool(Arc<dyn AgentTool>)` | Register an async closure tool returning `ToolResult` (argument object only; `ToolContext` needs `with_tool`) or a typed `AgentTool` whose `run` receives `&ToolContext` (`cwd`, `is_aborted()`, `update_channel()`, nested-call and session environment handles). |
| `core_tools(bool)` | Register the built-in coding tools (read/write/edit/bash/…; requires the `tools` feature or an individual `tool-*` leaf). Disabled by default. A build with no built-in tool compiled fails `build()` with the missing-feature diagnostic instead of silently registering nothing. |
| `tools([...])` | Allowlist with CLI semantics: plain names and `*` globs replace the default set (`?` is a literal byte, matching is case-sensitive); a pure `+name`/`-name` modifier list adds to/removes from it. The two forms must not be mixed. |
| `exclude_tools([...])` / `no_tools()` | Denylist on top of the default set (and any `tools` allowlist; unmatched ignored) / declare and execute no tools. |
| `on_event(name, closure)` | One plugin per call: observe/transform a unified-dispatch event (`input`, `before_agent_start`, `tool_call`, `tool_result`, `session_start`, `model_select`, `thinking_level_select`, `turn_end`, `agent_before_settle`, `session_before_compact`/`fork`/`switch`, …). The closure receives `&mut serde_json::Value` and may return `()` or a `PluginEventOutcome`. Handlers run in registration order. |
| `with_plugin(Arc<dyn AgentPlugin>)` | Attach a full Rust plugin (tools, commands, event handlers). |
| `builtin(name)` / `builtins([...])` | Attach a compiled-in builtin (`"subagent"`, `"tool-repair"`, `"tool-search"`, `"todo"`, `"memory"`, `"mcp"`, `"python"`, `"codemode"`). `builtin:`-prefixed and bare names are equivalent. `tool-search` is model-only and its `tool_search` tool is default-inactive: also add `.tools(["+tool_search"])` (exposure is then the model's call surface); deferred tools load on demand. |
| `extension_paths(paths)` / `only_builtin(bool)` | Load JS extensions from files or `builtin:<name>` pseudo-paths (requires `extensions`) / filter to `builtin:<name>` only: on-disk paths ignored with one diagnostic, no QuickJS actor (the CLI `--only-builtin` shape), no default-injection — attach builtins with `builtin()`/`builtins()`. |
| `proxy(ProxyProvider, Model)` | Route requests through an application proxy server. Registers the provider in a private registry and pins the model exactly like `with_registry` (which it is mutually exclusive with, along with `endpoint`/`model*`). |
| `session(SessionMode)` / `no_session()` | `DefaultDir` (default), `Dir(path)`, `Existing(path)`, or `None` (`SessionMode::None`, in-memory); durable modes write the upstream JSONL format (pi and pir share session files). |
| `compaction(config)` / `no_compaction()` / `retry(max_retries, max_retry_delay_ms)` | Tune/disable threshold compaction / transport retry override (`None` fields keep transport defaults). This is the HTTP transport budget only: it does not drive the session's agent-level auto-retry (`AutoRetryStart`/`AutoRetryEnd`), which follows the `retry` settings (`enabled`, `maxRetries`, `baseDelayMs`, `maxAgentDelayMs`) exactly like upstream `AgentSession._prepareRetry`/`getRetrySettings`; custom/synthetic providers bypass the transport and therefore never see this budget. |
| `tool_hooks(ToolHooks)` / `shared_tool_hooks(Arc<ToolHooks>)` | Install the per-tool `beforeToolCall`/`afterToolCall` bundle for every run; the shared form is reusable across agents and replaceable at runtime with `Agent::set_tool_hooks`. |
| `build()` | Resolve the model and assemble the agent. All model/limit/builtin/extension validation failures surface here. |

Model setup is exclusive: more than one of `endpoint`, `model`/`model_in_provider`, and `with_registry` fails
`build()` with "conflicting model configuration"; `endpoint` and `with_registry` also reject zero
`context_window`/`max_tokens`.

**Extension loading errors.** `extension_paths` is strict — a failure never yields a silently
incomplete agent. `build()` maps each input to the same errors listed in the
`AgentBuilder::extension_paths` rustdoc:

| input | `build()` result |
|---|---|
| on-disk JS that fails to load or parse | `SdkError::Extension` (message names the path) |
| `builtin:<name>` that is not compiled into this build | `SdkError::Other` (`Unknown built-in extension: …`) |
| path that does not exist | `SdkError::Extension` (message names the missing path) |
| any on-disk path while `only_builtin(true)` is set | ignored before loading (one `[ext]` diagnostic); build succeeds |

The low-level `create_agent_session` factory has no `only_builtin` convenience switch:
`CreateAgentSessionOptions` takes an already-loaded `ExtensionHandle`, matching upstream. A host
that wants the same policy calls the public `plugins::filter_builtin_paths` +
`agent_session::load_extension_runtime_strict` before constructing the session (a recorded design
fact, not an API change).

**System-prompt discovery.** The derived prompt reads `PIR_SYSTEM_PROMPT_FILE` and discovers
`SYSTEM.md`/`APPEND_SYSTEM.md` by default only on the environment-resolved `model*` path; `endpoint`,
`with_registry` and `proxy` stay hermetic unless `discover_system_prompt(true)`/`system_prompt_files(..)`
asks otherwise. An explicit `system_prompt` always wins.

### Agent directory identity

`agent_dir(path)` is the per-agent equivalent of `PIR_AGENT_DIR` and the reason two agents with **different** agent
dirs can run in one process (D2):

```rust,no_run
# async fn example() -> Result<(), pir::sdk::SdkError> {
let a = pir::sdk::AgentBuilder::new().agent_dir("/tmp/agent-a").no_session().build().await?;
let b = pir::sdk::AgentBuilder::new().agent_dir("/tmp/agent-b").no_session().build().await?;
assert_ne!(a.agent_dir(), b.agent_dir());
# Ok(())
# }
```

The directory is resolved **once**, at `build()`: the builder's value, else `$PIR_AGENT_DIR` /
`$PIR_CODING_AGENT_DIR` / `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`; `Agent::agent_dir()` reports it. All
agent-dir-scoped reads use it — `settings.json`, `models.json`, `auth.json`, builtin gates, the `subagent`
`agents/` discovery dir, `mcp` log/auth/global config, `codemode` config, the remote-catalog `models-store.json`
cache, and the `SessionMode::DefaultDir` session root. `!command` results are keyed by `(agent dir, command)`;
every other cache stays process-global by design.

Two limits are deliberate: **build-bound** — a later `PIR_AGENT_DIR` change never re-points a live agent, and
`Agent::session_dir()` stays where built (a session belongs to the directory and cwd it was created under, D9;
build a new agent to change either, while `attach_session(path)` opens an explicit file anywhere); and
**process-global switches stay global** — `PI_OFFLINE`, `PI_CATALOG_BASE_URL`, the
`ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` Claude Code compatibility layer, `PIR_CODEMODE_MEMORY_MB` and provider
API-key variables describe the process, not the agent; the explicit-directory environment loader does not re-apply
the Claude Code compatibility layer.

## Events

`Agent::prompt_stream` yields `SdkEvent`s through a bounded channel: intermediate delivery is best-effort (a slow
consumer never blocks the run; a full channel drops the event), while terminal events (`AgentSettled`,
`AgentOutcome`, and a rejected prompt's `PromptRejected`/`AgentSettled` pair) are sent with backpressure after
settlement, so a live receiver always observes them. The enum is `#[non_exhaustive]`.

The table is the complete variant list for this tree (33 variants). "Turn" fields are one-based indexes of the
low-level agent-loop run; one prompt may run the loop more than once (`agent_before_settle` continuation, overflow
recovery), and `AgentSettled` closes the whole prompt run, not each loop run.

| Event | Payload |
|---|---|
| `TurnStart` / `TurnEnd` | `turn_index`; `TurnEnd` adds `message` (the assistant message that completed the turn) and `tool_results` (this turn's tool-result messages in source order; empty without tool calls). |
| `MessageStart` / `MessageEnd` | — (the assistant message stream started) / `message`: the completed `AssistantMessage`. |
| `MessageUpdate` | `delta`, `thinking` (`true` for reasoning deltas). |
| `TranscriptMessageStart` / `TranscriptMessageEnd` | `turn` (`0` = the run's starting prompt/preamble), `message`: a non-assistant (system/user/developer/toolResult/custom) `Message` entering the transcript; `End` is the finalized message after handler replacement, if any. |
| `ToolCallStart` / `ToolCallDelta` / `ToolCallEnd` | `turn`, `content_index` (a tool-call block started / one raw JSON argument fragment in `delta`), and the complete parsed `tool_call` (`ToolCall`). |
| `MessageDone` | `turn`, `reason` (`TerminalReason`), `message`: the provider's completed assistant message; emitted before `MessageEnd`. |
| `MessageError` | `turn`, `reason` (`ErrorReason`), `message`: the provider stream failed (`error`/`aborted`); emitted before `MessageEnd`. |
| `ToolExecutionStart` / `ToolExecutionUpdate` / `ToolExecutionEnd` | `call_id`, `tool_name`, `parent_call_id`, `args` (the model's argument object; `null` when unavailable); `Update` adds `tool_call_id` and tool-defined `partial` JSON (`{content, details, isError}`); `End` adds `is_error`, `duration_ms`, and `result` (upstream `AgentToolResult` wire shape). |
| `Compaction` | — (threshold compaction ran). |
| `QueueUpdate` | `steering: Vec<String>`, `follow_up: Vec<String>`: the full pending texts after a push, drain, or clear. |
| `CompactionStart` / `CompactionEnd` | `reason` (`"manual"`, `"threshold"`, `"overflow"`); `End` adds `result: Option<Value>` (upstream `CompactionResult` wire shape), `aborted`, `will_retry`, `error_message: Option<String>`. |
| `EntryAppended` | `entry`: the appended `SessionEntry` in its upstream wire shape. |
| `SessionInfoChanged` | `name: Option<String>` (`None` when cleared). |
| `ThinkingLevelChanged` | `level`: the new effective level in wire form (`"low"`, `"high"`, ...). |
| `BashExecutionUpdate` | `id: Option<String>`, `delta`: one streaming chunk of an out-of-loop bash execution. |
| `AutoRetryStart` / `AutoRetryEnd` | `attempt`, `max_attempts`, `delay_ms`, `error_message` / `success`, `attempt`, `final_error: Option<String>`. |
| `SummarizationRetryScheduled` / `SummarizationRetryAttemptStart` / `SummarizationRetryFinished` | `attempt`, `max_attempts`, `delay_ms`, `error_message`; `AttemptStart` adds `source` (`"branchSummary"`/`"compaction"`) and `reason: Option<String>` (the compaction trigger). |
| `BackgroundToolEnd` | `call_id`, `tool_name`, `parent_call_id`, `is_error`, `duration_ms`, `output`, `images: Vec<ImageContent>`, `details: Option<Value>`, `structured_content: Option<Value>`, `usage: Option<Usage>`. |
| `PromptRejected` | `message`: why the prompt never started. |
| `AgentEnd` | `messages`: every message that low-level loop run appended, in order; `will_retry`: whether the session will re-run the loop. Not terminal. |
| `AgentOutcome(Box<AgentOutcome>)` | The complete terminal `AgentOutcome`; only `prompt_stream_with_outcome`, immediately before `AgentSettled`. |
| `AgentSettled` | `aborted`: terminal event; no more events follow. |

A one-turn text run is:

```text
TurnStart → MessageStart → MessageUpdate* → MessageDone → MessageEnd → TurnEnd → AgentEnd → AgentSettled
```

A tool-calling turn inserts `ToolExecutionStart → ToolExecutionUpdate* → ToolExecutionEnd` between `MessageEnd`
and `TurnEnd`; incremental argument frames arrive as `ToolCallStart → ToolCallDelta* → ToolCallEnd` during the
message stream. The next round-trip starts with a new `TurnStart`; `Compaction` precedes the affected `TurnStart`.
A prompt that never starts yields `PromptRejected` then `AgentSettled`, and `prompt_stream` always terminates with
`AgentSettled` (terminal events bypass a `prompt_stream_with` filter).

The session-level variants (`QueueUpdate`, `CompactionStart`/`CompactionEnd`, `EntryAppended`,
`SessionInfoChanged`, `ThinkingLevelChanged`, `BashExecutionUpdate`, `BackgroundToolEnd`, the three summarizer
retry events) are projections of `SessionEvent` via `SdkEvent::from_session_event`; `AutoRetryStart`/`AutoRetryEnd`
project transport retries. `SessionEvent::AgentSettled` maps to `None`: the stream's one terminal `AgentSettled` is
emitted by the run driver.

`AgentSettled` is the **session** settle (upstream `agent_settled` vs `agent_end`), not the end of one loop run:
when an `agent_before_settle` handler or overflow recovery issues another provider request, those events stream
first and exactly one `AgentSettled` closes the stream; `prompt_stream_with_outcome`'s terminal outcome likewise
belongs to the final loop run.

The stream exposes events only; `ToolExecutionEnd.result` carries the finished `AgentToolResult` wire value, and a
reporting tool surfaces progress through `ToolExecutionUpdate.partial`. For a typed terminal outcome and
tool-result messages, use `prompt` or `prompt_stream_with_outcome`.

### Event subscription

A prompt stream observes events only while that prompt runs. For events between runs, or when you drive the agent
with `prompt()` (which has no stream), subscribe on the agent:

```rust
use pir::sdk::{SessionEvent, SdkEvent};

agent.on_session_event(|event| {
    // Raw session event (same values a raw `AgentSession` emits).
    if matches!(&event, SessionEvent::EntryAppended { .. }) { /* mirror */ }
    // Typed SDK projection; `None` only for `SessionEvent::AgentSettled`.
    let _ = SdkEvent::from_session_event(&event);
});
```

Handlers run synchronously on the emitting task; hand heavy work to a channel or thread. The subscription lives for
the session's lifetime. With a raw `AgentSession`, use `AgentSession::on_session_event` directly.

## Agent reference

`Agent` is cheap to clone; clones share one session. Prompt runs are serialized by the session — queue work with
`steer`/`follow_up` instead of starting a concurrent `prompt`.

A session FILE is single-writer: `attach_session` does not lock or merge across independent `Agent` instances
(same as upstream pi's `SessionManager`, which assumes one process owns a file). Two agents attached to the same
path both append complete JSONL rows — the log never becomes invalid — but each keeps its own leaf, so the rows
form sibling branches and a later reader projects only the active branch: messages from the other writer remain
on disk but are not in `transcript()` — a whole-history read widens a paged manager to full residency (see the
capacity contract below), but it still projects the active branch only and never merges sibling branches.
Coordinate writers at the application level (one agent per session file,
or hand the session over with `shutdown` + a fresh `attach_session`).

**Long-lived sessions (capacity contract).** A persisted SDK session can opt
into a bounded resident tail with `live_session_tail(bytes, entries)`: between
prompts it keeps only the newest budget of the JSONL resident, and the MODEL
CONTEXT of the next prompt is that resident tail plus the new turn (older
history stays on disk but is not sent until a whole-history read). The default
is upstream whole-transcript residency (`without_live_session_tail()`, also the
builder default); `SessionMode::None` is unaffected. The append-only file is
never rewritten; whole-history reads such as `transcript()`, `transcript_json`,
`session_stats()` and branch/label operations widen or stream the prefix, so
they stay complete. The trade-off is one transcript parse/build per prompt for
sessions above the budget instead of retaining every parsed entry.

R48 apples-to-apples (debug, 4 messages/round: user + assistant tool call +
tool result + assistant, 500 rounds, no provider request retention):
`live_session_tail(64 KiB, 128)` ends at 35-38 MiB with a post-warmup slope of
~0.001 MiB/round (R²=0.07, flat); the same payload with
`without_live_session_tail()` ends at 109-114 MiB, slope 0.165 MiB/round
(R²=0.98). The session file is the same size (~750 KiB) in both arms; the live
arm's last `agent.transcript()` still returns the complete 2016 messages /
2017 lines. Recommended for long-lived services: 64 KiB/128 entries (about
32 four-message turns of model context); tune upward if older-context recall
matters.

R50 1000-round recheck (same harness): live ends at 36.3 MiB (peak 37.8),
post-warmup slope -0.002 MiB/round (flat, R²=0.73); full ends at 188.6 MiB,
slope 0.160 MiB/round (R²=0.9935); both session files are ~1.5 MiB with a
complete transcript on disk (4002/4032 lines) and through `transcript()`
(4001/4031 messages). The live arm's extra 30 messages are `toolsAdded`
system entries emitted at each re-arm. R56 re-ran the same 1000-round soak
after the R55/R56 snapshot work (PIR-SDK-017 closed): live 34.05 MiB flat
(-0.00140 MiB/round), full 101.78 MiB at +0.06337 MiB/round (-60.5% slope —
the per-prompt re-serialization share of the default residency is gone; the
default path is still linear by design, so `live_session_tail` remains the
bounded solution).

R51 live-tail + manual compaction (300 rounds, `agent.compact()` every 100)
exposed a compaction-cadence sawtooth: r100 36.5 -> r200 48.7 MiB
(+0.131 MiB/round, R²=0.925), dropping to 37.1 MiB after compaction, while
the identical no-compaction control stayed 35-36 MiB. R52 fixed the cause
(PIR-SDK-024): `trim_resident_tail` now pins the newest compaction entry as an
extra ANCHOR next to the entry-capped tail instead of refusing the trim, so the
checkpoint summary stays in the prompt context while the summarized prefix is
dropped. Same 300-round arm after the fix: final 37.8 MiB / peak 37.9, segment
r100-r200 +0.002 MiB/round (R²=0.02) — a plateau next to the 36.3 MiB
no-compaction control; compaction history (3 rows), `transcript()`, resume and
fork stay correct. Manual-compaction hosts no longer need the old
full-residency (~0.13 MiB/round) budget between compactions; the checkpoint is
one entry over the configured entry window by design.

Release deployment budgets (R26, evidence
`.play/evidence/lane-sdk-dev/round26-release-1000/`): the same live-tail +
compaction shape on a release GNU binary (160-byte payload, real
tools/builtins, 0 errors / 0 guard triggers) measured 24.79 MiB steady /
29.72 MiB whole-history export peak after 1000 rounds for one agent
(+0.0028 MiB/round, 102 ms/prompt) and 31.32 MiB steady / 41.75 MiB export
peak for two tenants × 1000 rounds (+0.00205 MiB/prompt — the second tenant
adds ~6.5 MiB, not a second history). Recommended contractual process
budgets: **36 MiB** for one agent when a whole-history `transcript()` export
is part of the request path, **56 MiB** for two agents (export peak plus
~20–35% headroom). These budgets apply to the live-tail +
periodic-compaction deployment; the default whole-history residency is sized
from the debug-shaped budgets below instead.

**Whole-history reads widen the resident tail.** `transcript()`,
`transcript_json()` and the raw-session `full_messages_json()` materialize the
complete history even when `live_session_tail` is set: the resident window
grows to the whole session for that read and the process allocator retains a
high-water mark proportional to the history size afterwards, so RSS may not
fall back to the plateau immediately. Measured on the support-bot app: one
`transcript()` of a 2,824-message live-tail session raised RSS from 29.9 MiB
to 80.4 MiB; the next capacity-guard sample (100 rounds later, round 725 of
1000) read 80.4 MiB and tripped a 64 MiB guard although the steady-state
curve was flat. Treat these reads as on-demand export/debug operations, not
request-path or monitoring work: monitor session health from the session
file's byte/line count (or `session_stats()`, which streams the prefix without
widening), and keep transcript reads out of the sampling loop that feeds a
capacity guard. PIR-SDK-025 (fixed in R52): whole-history reads widen BOTH
`is_live_trimmed()` and generic `is_paged()` managers, so the first
`transcript()`/`transcript_json()`/`full_messages_json()` after resuming a
>1,024-entry session returns the complete history (verified 1210, not 1024).
R53: `transcript()` and the continuation/cache-warm rebuilds now use
`SessionManager::build_session_messages` — the same leaf-path/retention/
context-edit walk and the one required `Vec<Message>` materialization, without
the per-entry `ProjectedSessionEntry` provenance vec (`Arc` bump + `Range` per
retained entry). Probe on the 1,212-message R53 session: per-call allocations
7,364 -> 6,160 (-16.4%) and allocated bytes 1.284 MB -> 1.246 MB (-3.0%),
wall unchanged (2.5 -> 2.5 ms/call); the remaining bytes are the owned
messages the public `Vec<Message>` API must return. Callers that need
per-entry provenance keep `build_session_projection`.
R54 measured the owned-result cost on an 8,000-message session (4.27 MB):
`transcript()` 26.7 ms/call, 28.1k alloc calls and 7.60 MB allocated per call
(~950 B/message; ~3.3 us/message); `transcript_json()` 105 ms, 200k allocs,
17.9 MB/call; `full_messages_json()` 108 ms, 200k allocs, 18.0 MB/call. These
are export/debug reads, not polling APIs: use `message_count()` or
`session_stats()` for monitoring. An mmap/COW prototype (read-only `Mmap` +
`Vec<u32>` line offsets; `Arc<Mmap>`/`Arc<[u32]>` shared prefix) removed the
whole-file heap buffer (Private Bytes +0 vs +4.54 MB for `fs::read`+`String`,
though Working Set gains the reclaimable mapped pages) and 10,000 shared
prefix views cost 0 heap allocations; it is not integrated into the public
API yet.
mmap is already load-bearing inside the `session` feature, just not on this
public surface: the full-session loader
(`SessionManager::load_entries_from_file_mapped`) and the paged tail reader
(`session_manager::tail_page`) both map the JSONL read-only and parse borrowed
lines, so opening a large session does not copy the file; setting
`PIR_DISABLE_SESSION_MMAP` forces the streaming reference loader for
before/after measurements. What the prototype adds (and what PIR-SDK-028
tracks) is a reusable owned `Arc<Mmap>` snapshot for hosts that repeatedly
scan/export a big file outside the prompt path. The durable kernel is separate:
an mmap evaluation of its JSONL backend concluded mmap is NOT integrated for
recovery reads (see `docs/durable-spec.md` §11), while the TUI's spill preview
does map the spilled file (see `docs/tui.md`).
R55 (PIR-SDK-027 fixed): the per-prompt `ctx.sessionManager` snapshot push no
longer materializes three transcript-sized JSON trees. The `entries` aggregate
stays the one shared `Arc<Value>`; the `projection` (`buildSessionProjection()`)
and `branch` (`getBranch()`) snapshot fields are now PRE-RENDERED JSON text
(spliced borrowed per-entry views / leaf-path entries — byte-identical wire
shape, pinned by unit tests), and the entry-tree text is cached as a split
per-node structure in a flat arena: an append-only session splices ONLY the new
entries' fragments and re-renders by one concatenation (no `tree()` deep clone,
no re-serialization of existing entries); branch/fork/label/trim changes fall
back to a full render. Same 100×5 TUI load (500 tool calls, 802 entries):
with-session RSS growth +85.8 MB → +49.1 MB while `--no-session` grew +46.7 →
+42.1 MB, so the session-snapshot attribution drops 39.1 MB → 7.0 MB
(evidence `.play/evidence/lane-sdk/round55-027-fix/`). The snapshot still
rebuilds on every append (the fingerprint must move — the push reflects the
new tail); what changed is the per-rebuild cost: one aggregate `Value` plus
string splices instead of three `Value` trees plus a whole-forest
re-serialization.
R56 added a per-entry projection fragment cache
(`ProjectionEntryFragments`): the append-only push serializes only the new
entries and splices the cached texts, so per-push wall on the same 8,000-message
probe drops ~519 → ~333 ms (-36%; allocs -3.1% — the remaining ~200k are the
native `entries` aggregate contract, `Arc<Value>` per push, documented floor).
Same 100×5 TUI load after R56: with-session wall 65.3 s → 45.0 s (the R55 wall
regression is gone), attribution 15.7 MB (evidence
`.play/evidence/lane-sdk/round56-027-cache/`).

Before this bound (and with `without_live_session_tail()`), `SessionManager`
keeps the whole live transcript resident (the upstream `fileEntries` design):
RSS grows with the number of messages in ONE session, not with disk I/O —
measured on a debug build with real tools/builtins, 4 messages per round cost
~0.17 MiB/round on the pre-R55 snapshot path (0.036 MiB/round with
`--no-session`, 0.095 with `--no-tools`; a 1000-round session file was only
2.14 MB). After the R55/R56 snapshot work the same default-residency harness
measures **+0.063 MiB/round** (final 101.8 MiB at 1000 rounds), so budget
~0.06-0.17 MiB/round (debug; payload- and build-dependent — the R44
support-bot shape with real tools/builtins kept its own ~0.17 budget) and
recycle the session/agent every N rounds (for example 200), or re-open the
file with `attach_session`. The bounded live-session implementation is
verified in PIR-SDK-018 (R56 1000-round arms: live `live_session_tail(64
KiB, 128)` 34.05 MiB flat, -0.001 MiB/round; full default residency 101.78
MiB, +0.063 MiB/round).

| Method | Effect |
|---|---|
| `prompt(text)` / `prompt_with_images(text, images)` / `prompt_boxed(text)` | Run one prompt and return `AgentOutcome` (no `SdkEvent` stream); images append to the starting user message; `prompt_boxed` is `prompt` with an explicitly boxed `Pin<Box<dyn Future<...> + Send + '_>>`. |
| `prompt_stream(text)` / `prompt_stream_with_images(text, images)` | Return `impl Stream<Item = SdkEvent>` (boxed, `Unpin`), optionally with image attachments. Dropping the stream detaches the observer; the run continues. |
| `prompt_stream_with(text, filter)` / `prompt_stream_with_outcome(text)` | As `prompt_stream`, dropping intermediate events for which `filter(&SdkEvent)` is `false` (the filter runs on the loop thread; terminal and out-of-loop session-level events — `queue_update`, compaction lifecycle, entry/session/thinking/retry/background — bypass it), or adding `SdkEvent::AgentOutcome` immediately before `AgentSettled`. |
| `set_event_channel_capacity(n)` / `event_channel_capacity()` | Tune (clamped to at least 1) or read the bounded stream channel capacity used by the next run (default 1024). |
| `steer(text)` / `follow_up(text)` | Queue a steering message (delivered before the next model request) or a follow-up (delivered after the current turn sequence). |
| `abort()` / `compact()` | Abort the in-flight run (settles aborted; no-op between runs) / compact now, returning `CompactionResult`. |
| `is_streaming()` / `is_idle()` / `wait_for_idle()` | Run state; `wait_for_idle` polls until no prompt run is in flight. |
| `pending_message_count()` | Queued steering plus follow-up messages not yet delivered. |
| `tools()` / `take_notifications()` | The tools the next run declares (`ToolInfo { name, description, parameters }`, sorted; hidden/deferred excluded) / drain presentation-class extension notifications (headless hosts). |
| `on_session_event(handler)` | Subscribe to the out-of-loop `SessionEvent`s (`QueueUpdate`, `CompactionStart`/`CompactionEnd`, `EntryAppended`, `SessionInfoChanged`, `ThinkingLevelChanged`, `BashExecutionUpdate`, retries, `BackgroundToolEnd`, `AgentSettled`); project with `SdkEvent::from_session_event`. See "Event subscription". |
| `session_path()` / `session_id()` / `session_name()` / `set_session_name(Option<&str>)` / `session_dir()` / `agent_dir()` | Live session identity and display name; build-bound session root and agent config directory. |
| `new_session()` / `attach_session(path)` / `continue_session()` / `reset()` | Start fresh storage, resume an explicit file, resume the directory's most recent session, or replace the transcript with an empty in-memory one. All require session storage and an idle agent (except `reset`, which also requires storage); usage totals follow the switch. |
| `transcript()` / `transcript_json()` / `ephemeral_transcript()` | Current branch messages (or the in-memory history when there is no storage). All three are `async`; call `agent.transcript().await`, `agent.transcript_json().await`, etc. |
| `is_paged_session()` / `paged_window()` / `transcript_cursor()` / `older_transcript_messages(cursor, bytes, entries)` / `next_older_page(cursor)` / `older_transcript_json(...)` | Whether a large attached session holds only a resident tail window and that `PagedWindow`'s bounds; walk the skipped prefix one bounded page at a time. All paged reads are `async` (`paged_window().await`, `older_transcript_messages(..).await`, ...). |
| `session_stats()` / `context_usage()` | Whole-session token/cost/message totals (`SessionStats`; paged prefix included without widening) and current `ContextUsage`. Both are `async`: `session_stats().await`, `context_usage().await`. |
| `clear_queue()` / `queued_messages()` / `steering_messages()` / `follow_up_messages()` | Drain or peek the queued steering/follow-up messages; `clear_queue` returns `ClearedQueue`. |
| `steering_mode()` / `set_steering_mode(mode)` / `follow_up_mode()` / `set_follow_up_mode(mode)` | Queue delivery-order mode (`SteeringMode` / `FollowUpMode`) for steering and follow-ups. |
| `commands()` / `dispatch_command(name, args)` | List native plugin slash commands and dispatch one (falls back to the JS runtime); returns `CommandOutcome`. |
| `usage_totals()` / `model()` / `thinking_level()` | `UsageSnapshot { turns, total_tokens }`, `ModelRef { provider, id }`, and the effective (clamped) thinking level. |
| `set_model(model)` / `set_thinking_level(level)` | Switch for subsequent runs; the effective thinking level is clamped to what the model supports (also after `set_model`). |
| `set_tool_hooks(hooks)` / `tool_hooks()` | Read or replace the per-tool `ToolHooks` bundle installed for subsequent runs (`None` clears). |
| `shutdown()` | Abort, wait up to 2 s for the run to settle, mark the agent shut down, and run plugin `on_shutdown` hooks. Later prompts are rejected. |

`AgentOutcome` carries `message` (terminal `AssistantMessage`), `tool_results`, `messages` (every message the run
appended, upstream `agent_end.messages`), `usage` (terminal message only), `turns`, and `aborted`; helpers are
`message_text()` and `tool_result_texts()`.

`ToolInfo` is `{ name, description, parameters: Option<Value> }`. `UsageSnapshot::turns` counts loop turns —
model round-trips across every prompt run, so one tool-calling prompt that then gets a final answer reports `2`; it
is not a prompt counter.

## Errors

Every fallible SDK entry point returns the typed, `#[non_exhaustive]` `SdkError` surface:

| Variant | Meaning |
|---|---|
| `NoModel { message }` | Model resolution failed (no endpoint, nothing in `models.json`/catalog/auth). |
| `Provider(AiError)` | The AI layer rejected a request (unknown provider/model, auth, transport, non-terminated stream). |
| `Io(std::io::Error)` / `Session(String)` | A filesystem or transport I/O error (display text is the I/O error's own; its chain stays reachable via `source()`) / session storage failed to open, create, or persist entries (run-path precondition rejections "already streaming" / "compaction is in progress" stay `Other`). |
| `Extension(String)` | An on-disk extension failed to load or the JS runtime failed to start at build. |
| `Aborted` | The run aborted before producing assistant content; later aborts return the partial `AgentOutcome` with `aborted: true`. |
| `Unsupported(String)` | A requested capability is not compiled into this build (for example a `create_*_tool` factory whose `tool-*` feature is off); the message names the missing feature. |
| `Other(String)` | Anything the facade cannot classify (including rejected concurrent prompts and plugin registration failures). |

## Embedding without the full agent

The provider layer is public and usable on its own; `AgentBuilder` only wires it into the loop. You can drive a
provider directly:

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

- `ProviderRegistry::{stream, stream_unchecked, complete}` accept `impl Into<Arc<Context>>`; pass `Arc<Context>` to
  avoid copying the transcript. `complete` returns `AiResult<Arc<AssistantMessage>>`; `stream_unchecked` skips auth
  resolution.
- `Provider::stream_shared(model, Arc<Context>, options)` is the shared-transcript boundary: the default
  implementation materializes the owned context once (zero-copy when it holds the last handle), and every bundled
  wire adapter overrides it to read the shared context directly.
- `pir::agent::agent_loop::ProviderRequest.context` is `Arc<Context>`; `context_into_owned` is the identity
  function.
- The full session facade is public (`pir::agent_session::AgentSession::new(AgentSessionConfig)`), but the SDK is
  the supported embedding surface.

### Durable kernel

`pir::durable` is the core durable conversation/task/document runtime (no cargo feature; not re-exported through
`pir::sdk`). See the Rust quickstart in [`durable-spec.md`](durable-spec.md#rust-quickstart-developer-usage) for
`JsonlStorage` + `Session`, forks, reopen and torn-line recovery.

Two `define_task` functions exist and they are not interchangeable: the one in `pir::durable::harness::task`
builds an executable definition (`define_task(name, version, initial).phase(..).build()`) whose `Task` a
`TaskScheduler` runs, while `pir::durable::tasks` holds the metadata registry used for agent hooks
(`TaskDefinition { kind, version, phases }`). A task phase can commit a durable sleep through
`TaskRuntime::sleep_until`; `pir::durable::timers::Timers` wraps a host timer with the native fallback used by
the scheduler.

## Extending the agent

Every extension path lands on one plugin host: closure/typed tools, inline `AgentPlugin`s, compiled-in builtins,
and JS extensions share one registry and one ordered event dispatch.

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

Builtins resolve through the same registry as the CLI's `-e builtin:<name>` path. A name that cannot be attached
fails `build()` — never a silent no-op:

```text
Unknown built-in extension: builtin:<name>
Built-in extension `builtin:python` is not compiled into this build (the `python` feature is off)
```

The agent directory's settings gates apply too: `pythonTool: false` or `disabledBuiltinExtensions` fails `build()`
with a diagnostic naming the setting, so settings cannot be bypassed by attaching a builtin by name.
`.builtin("builtin:subagent")` and `.builtin("subagent")` are equivalent.

### Native providers

`ProviderService::register_provider_impl(ProviderBox)` installs a typed Rust provider owned by the calling plugin
binding; register it from `AgentPlugin::on_context` (the `register()` phase has no service context):

```text
ctx.providers().expect("providers bound").register_provider_impl(Arc::new(my_provider));
```

`ProviderBox` is `Arc<dyn Provider>` (`src/ai/provider.rs:316`); accessor `PluginContext::providers()`
(`src/plugins/context.rs:229`), trait method `src/plugins/services/mod.rs:1815`, live impl
`src/agent_session/plugin_services.rs:2985`. Lookup order is plugin-owned typed > session dynamic > shared dynamic
> static (`src/ai/provider.rs:586-640`); same-id registrations stack across bindings, last-wins within one, and
unregister/`Drop` removes only that binding's entries.

The JSON path `register_provider(id, config)` supports only `models` / `baseUrl` / `api` / `headers` / `authHeader`
(`apiKey` supplies the header value); `streamSimple` / `refreshModels` / `oauth` callbacks are rejected with a
diagnostic and need a typed provider (`src/agent_session/providers.rs:2575-2652`;
`src/plugins/services/mod.rs:1779-1800`). Native auth hooks stay process-wide and last-wins with no session
dimension (a live child's `register_auth_hooks` can shadow a parent's for the same provider id:
`src/agent_session/plugin_services.rs:2999-3030`; `src/plugins/services/mod.rs:887-935`).

`on_event` is the single-event shortcut: one closure plugin per call, stacked in registration order and receiving
the same `serde_json::Value` payloads as a full plugin. Rust handlers observe `tool_execution_update` and the
session-level events in the builder table; `tool_call` /`tool_result` dispatch through the same unified host
(`dispatch_hosted_event`, `src/agent/tool_execution.rs:1878`, `:2156`), so each face observes them once.

### Public value types

| Type | Fields / variants |
|---|---|
| `Endpoint` | `base_url`, `api_key`, `api` (`ApiKind`), `model_id`, `context_window`, `max_tokens`; `Endpoint::new` defaults to 128k / 8192. |
| `ApiKind` | `OpenAiCompletions`, `AnthropicMessages`, `GoogleGenerativeAi`; `wire_id()` returns the models.json `api` id. |
| `SystemPromptFiles` | `system`, `append: Option<PathBuf>`; explicit prompt files for the derived prompt (`.system_prompt_files(..)` / `CreateAgentSessionOptions::system_prompt_files`). Relative paths resolve against the process cwd. |
| `SessionMode` | `DefaultDir`, `Dir(path)`, `Existing(path)`, `None`. |
| `ToolInfo` / `UsageSnapshot` / `ModelRef` | `name`, `description`, `parameters`; `turns`, `total_tokens`; `provider`, `id` (`Display` prints `provider/id`). |
| `AgentOutcome` | `message`, `tool_results`, `messages`, `usage`, `turns`, `aborted`. |
| `ClearedQueue` | The messages drained by `Agent::clear_queue()`. |
| `CompactionResult` | Summary, `tokens_before`, `first_kept_index` / `first_kept_entry_id`, `retained_tail`, summarizer `usage`, and the summarized file lists. |
| `PagedWindow` | `page_start`, `file_bytes`, `parsed_entries`: the resident tail window of a paged session. |
| `CommandOutcome` | `ok` plus the command's display text in `error` (native commands use it for success text too). |
| `Notification` | A presentation or actionable extension notification drained by `Agent::take_notifications()`. |
| `SteeringMode` / `FollowUpMode` | Queue delivery-order modes. |
| `SessionStats` / `ContextUsage` | Whole-session aggregates and current context-window usage. |

## Session-level and runtime surface

`AgentBuilder` is the ergonomic single-agent path. The same SDK also exposes the raw upstream session objects and
factories for applications that own lifecycle (all in `pir::sdk`, no internal modules required):

| Item | Effect |
|---|---|
| `AgentSession` / `AgentSessionConfig` / `SessionEvent` / `SessionEventHandler` | The raw session object (the same one the CLI hosts and JS extensions use), its config, and its subscribe API. |
| `create_agent_session(CreateAgentSessionOptions)` | Resolve services (settings, model registry, storage, resource loader) and return `CreateAgentSessionResult { session, model, thinking_level, resources, model_fallback_message }`. CLI parity: when `resource_loader` is unset the factory resolves an environment `DefaultResourceLoader` for `cwd`/`agent_dir` and installs the skills/templates it discovers, so a bare low-level session is **not** hermetic; pass a loader (e.g. `DefaultResourceLoader::with_home(..).without_skills()`) to pin resources, or set `home` to pin only the default loader's `~`/user-global `.agents/skills` scope without touching process env. System-prompt discovery/`PIR_SYSTEM_PROMPT_FILE` is on by default (project files trust-gated); `discover_system_prompt = Some(false)` disables it, `system_prompt_files` supplies explicit files, `system_prompt` wins. |
| `create_agent_session_services` / `create_agent_session_from_services` | Split the resolution and build halves when several sessions share one environment. |
| `create_agent_session_runtime` / `AgentSessionRuntime` | Build a session plus the lifecycle wrapper: `new_session`, `switch_session`, `reload` (all gated on an idle session) and `dispose` (extension-runtime teardown). |
| `ModelRegistry` / `SettingsManager` / `SessionManager` | Model lookup + configured-auth filtering, settings snapshots, and session storage; `ModelRegistry::from_environment_in` / `SessionManager` scoped to an explicit agent directory. |
| `DefaultResourceLoader` / `ResourceLoader` / `DefaultPackageManager` | Skills and prompt-template discovery (including `resources_discover`) attachable through `CreateAgentSessionOptions::resource_loader` or `AgentBuilder::resource_loader`; installed-package resource paths. |
| `EventBus<T>` / `create_event_bus` | A native typed publish/subscribe bus, independent of the agent session event stream. |
| `create_coding_tools`, `create_read_only_tools`, `create_tool`, `create_read_tool`, `create_bash_tool`, `create_edit_tool`, `create_write_tool`, `create_ls_tool`, `create_grep_tool`, `create_find_tool`, `create_powershell_tool`, `has_core_tool` | The exact built-in tool objects the CLI registers; each factory returns `SdkError::Unsupported` naming the missing `tool-*` feature when compiled out. |

**Branching, fork and session switching are runtime-level.** An `Agent` (the
`AgentBuilder` result) has no `fork` method and no public session handle, so a
single linear conversation is all it can express. Build with
`create_agent_session_runtime(CreateAgentSessionOptions { .. })` when the app
needs to branch: the result carries both `created.session` (an
`Arc<AgentSession>`) and `runtime` (`AgentSessionRuntime`), whose
`fork(entry_id, ForkPosition)` branches at a stored entry
(`ForkPosition` is `pir::agent_session::ForkPosition`), and whose
`new_session` / `switch_session` / `reload` / `dispose` manage the session
lifecycle. `fork` is idle-gated like the other lifecycle calls. Use the
`AgentBuilder` path for one linear conversation; use the runtime factory for
branching (for example an alternate reply on an existing ticket), session
hand-over, or extension-runtime teardown. There is no public bridge from an
already-built `Agent` to a runtime — decide the construction path up front.

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

Known gaps: the raw factories do not repeat `Agent::attach_session`'s "restore the model recorded on an existing
session" step — pass storage and set the model explicitly, or use `Agent::attach_session`; JS `withSession`
callbacks are a host/JS bridge, so a Rust embedder calls `AgentSessionRuntime::new_session` / `switch_session` then
runs its own setup. `CreateAgentSessionOptions::max_turns` keeps the raw-factory default of 100 while
`AgentBuilder::max_turns` defaults to 1000; set it explicitly when the raw path needs the higher cap.
Upstream `CreateAgentSessionOptions.sessionStartEvent` is intentionally not exposed: the raw factory does not
classify session lifecycle reasons (its first prompt always dispatches `session_start {reason:"startup"}`); use
`Agent::attach_session` / `new_session` (or the runtime's switch/fork), which emit `resume` / `new` / `fork` with
`previousSessionFile`.

## Proxied requests

`ProxyProvider` routes every model request through an application server that owns the provider credentials — the
Rust equivalent of upstream `streamProxy` (`POST {proxy_url}/api/stream`, bearer token, SSE proxy events decoded
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

A minimal embed builds with `default-features = false, features = ["sdk"]`; add `"session"` (durable transcripts,
`Agent::attach_session`, paged reads) and `"skills"` when needed. `pir::sdk` is **not** part of the bare core (no
stub). The core — agent loop, provider framework, session types/config, prompt/skills rendering and plugin
framework — compiles without any UI/JS/phone stack (no tui/ratatui/crossterm/quickjs/phone in the minimal
`cargo tree`); built-in tools are the separate `tools` aggregate (a `cli` binary without it is toolless and
`core_tools(true)` fails `build()`), `sdk` itself adds no new packages, and `full` is the default. The one
deliberate heavyweight in the bare core is `reqwest` (with native-tls): it backs the provider HTTP transport
(`ai::transport`), every OAuth flow (`ai::auth::http`), the remote catalog refreshers and the core
OpenAI-compatible adapter — and the core adapter is intentionally compiled without a feature gate so the
`--no-default-features` minimal profile stays a runnable wire agent (see docs/FEATURES.md §2). An SDK embed
therefore inherits the HTTP stack even with no `provider-*` feature; recorded as PIR-SDK-029.
The `pir` binary requires only `cli`, which does **not** imply `tools`.

SDK-relevant features: `sdk` (facade), `session`, `skills`, `tools`, `provider-*`, `subagent`, `tool-repair`,
`tool-search`, `todo`, `memory`, `mcp`, `python`, `extensions`, `grep-find`, `image-tools`. `with_registry` works without any
`provider-*`; `endpoint()` fails `build()` naming the missing feature; an environment-resolved catalog model's
first request ends with the feature-naming error.

Recommended combinations:

| Shape | Features |
|---|---|
| Custom `Provider` + in-memory agent (hermetic, tests) | `sdk` |
| Durable sessions (`attach_session`, paging) | `sdk,session` |
| Skills / prompt templates through `resource_loader` | `sdk,session,skills` |
| Explicit HTTP `Endpoint` (OpenAI-compatible) | `sdk,provider-openai` |
| Built-in coding tools | `sdk,tools` (or individual `tool-*` leaves) |
| Compiled-in builtins | `sdk,subagent` / `sdk,tool-repair` / `sdk,tool-search` / `sdk,todo` / `sdk,memory` |
| JS extensions / `codemode` | `sdk,extensions` |
| Full CLI-equivalent surface | `full` (an embed rarely needs it) |

There is no standalone `sqlite` feature (the node:sqlite/better-sqlite3 host shims ride `extensions`). `tui`,
`env`, `server`, `cli`, and `mcp-sd` are host features an embed does not need; [`docs/FEATURES.md`](FEATURES.md)
has the full matrix, the gating map and the measured per-feature optionality/minimality matrix (§4.1).

Per-feature measured cost — release binary delta, packages added by `cargo tree`, and first-prompt / 100-round
heap and WorkingSet — is tabulated in [`docs/FEATURES.md`](FEATURES.md) §4.1. Its measurement vehicle is the
external consumer crate in the repo at
[`.play/lanes/lane-sdk-dev/apps/quickstart-copy/README.md`](../.play/lanes/lane-sdk-dev/apps/quickstart-copy/README.md)
(evaluation evidence, not part of the shipped package).

## Packaging and size

The crates.io package is `pir-sdk`; the library is imported as `pir` and the binary is `pir`. The package ships
`src/**`, `assets/**`, `examples/**`, `benches/**`, `schemas/*.json`, `README.md`, `CHANGELOG.md`, `docs/sdk.md`
and the license files; every other `docs/**` page is excluded (see `Cargo.toml`); `cargo package --list --no-verify`
inspects the exact `.crate` file list.

Measured Windows x64 executables, one consumer (Rust std + tokio + serde_json included), fat LTO, one CGU, symbols
stripped:

| Embed | opt-level 3 | opt-level `"s"` |
|---|---:|---:|
| `sdk` only, no custom tool | 12.11 MiB | **8.67 MiB** |
| `sdk,session,skills` + one tool | 12.63 MiB | **9.15 MiB** |
| above + `provider-openai` + `Endpoint` | 13.99 MiB | **10.11 MiB** |

A custom tool costs ~6.5 KiB. `opt-level = "s"` is the recommended embed default: ~28% smaller with no measurable
startup cost, and the loop is provider-I/O-bound. Numbers include the consumer's std/tokio. These rows are a
recorded measurement (the packaging round's `logs/lane-sdk-package/REPORT.md` is no longer in the tree); the
surviving external-consumer pins are PIR-SDK-029 and
`.play/evidence/lane-sdk-dev/round27-debug-budget/REPORT.md` (release quickstart 35,094,016 B; support-bot
45,366,784 B). Copy-paste profile:

```toml
[profile.release]
opt-level = "s"
lto = "fat"
codegen-units = 1
strip = "symbols"
```

Keep the feature set minimal (`full` is CLI parity, not embeds); when matching an existing profile, `lto`/`codegen-units`
matter more than `opt-level` for the `pir` code itself.

## Verification

```bash
cargo check --examples --no-default-features --features sdk   # the sdk-only examples
cargo check --no-default-features --features sdk --test sdk_consumer_embed
cargo check --no-default-features --features sdk --test sdk_consumer_surface
cargo check --all-targets --all-features
```

`tests/sdk_consumer_embed.rs` is the provider/registry/session contract test; `tests/sdk_consumer_surface.rs` pins
the documented builder/agent methods and event/error variants; `tests/sdk_extensibility.rs` covers tool selection,
`on_event`, stream filtering and model/thinking switches; `tests/suites/sdk.rs` covers builder resolution, event
ordering, tools, plugins, sessions and error mapping. `scripts/run_examples.sh` runs the offline example set.
