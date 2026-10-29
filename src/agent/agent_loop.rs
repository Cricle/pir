//! Agent loop: turn orchestration bridging the AI layer and the tool scheduler.
//!
//! The loop owns the conversation transcript, consumes provider streams, and
//! keeps tool execution deterministic even when the scheduler runs calls in
//! parallel.  The small callback surface is intentional: applications can
//! attach UI/session telemetry without making the agent depend on either.
//!
//! Upstream parity notes (`packages/agent/src/agent-loop.ts` at HEAD):
//! - `finish_turn` (upstream `finishTurn`) runs after a completed turn and its
//!   tool results but BEFORE `turn_end`; `{action: "continue"}` guarantees one
//!   more provider request (tool-result/steering/follow-up scheduling can
//!   satisfy it), `{action: "end"}` ends the run.
//! - `prepare_request` (upstream `prepareRequest`) runs before every
//!   conversational provider request, including the first, and may replace the
//!   context, model, and thinking level for this and later requests.
//! - `message_end` handlers may REPLACE the message they are handed (upstream
//!   `ExtensionRunner.emitMessageEnd` + `agent-session.ts:1318-1336`): the
//!   chained same-role replacement is applied to the message the loop goes on
//!   with — before persistence, before the tool calls are extracted, and
//!   before the native `tool-repair` `message_end_repair` hook runs over it
//!   (see [`emit_message_end`]). A handler that returns nothing, or the same
//!   message, leaves the stream's message untouched.
//! - Nested tool calls (upstream `core/nested-tool-calls.ts`) reuse the same
//!   scheduler path and carry `parentToolCallId` on their events.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use tracing::Instrument;

use crate::agent::nested_tool_calls::{
    NestedEventSink, NestedShared, NestedToolEvent, NestedToolExecutor,
};
use crate::agent::tool_batch::{AgentToolCall, ToolBatchOutcome, ToolResult};
use crate::agent::tool_execution::{
    error_results_for_length, tool_calls_from_message, ToolContext, ToolExposure, ToolHooks,
    ToolRegistry,
};
use crate::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use crate::ai::event_stream::AssistantMessageEventStream;
use crate::ai::provider::ProviderRegistry;
use crate::ai::types::AbortSignal;
use crate::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, StopReason,
    StreamOptions, TextContent, ThinkingLevel, Tool, ToolResultMessage, ToolResultRole,
    UserMessage, UserRole,
};

use serde_json::Value as JsonValue;

/// Deliver one loop event to the NATIVE Rust plugin fan-out.
///
/// The loop's JS face is dispatched through its own handle
/// ([`LoopDeps::extensions`], below); the unified host's non-JS plugins are
/// reached through this companion so a builtin/inline-only plan (a plan-only
/// extension handle, no JS runtime) still observes live agent events. The JS
/// adapter inside the host is deliberately NOT consulted here
/// ([`crate::plugins::PluginHost::emit_native`]), so each face receives the event exactly
/// once. `None` hosts (print mode, embedders without a plugin host) are a
/// no-op.
async fn emit_native_extension(deps: &LoopDeps, event: &str, payload: &mut JsonValue) {
    let Some(host) = deps.plugin_events.as_ref() else {
        return;
    };
    if !host.has_native_handlers(event) {
        return;
    }
    host.emit_native(event, payload).await;
}

/// Dispatch one extension event (upstream camelCase payload) to BOTH faces:
/// the native Rust plugins first (when the host declares interest), then the
/// JS runtime (when a handler is registered). Emissions are skipped entirely
/// when neither face wants the event (fast-path registry checks), so
/// extension-free runs pay nothing.
async fn emit_extension(deps: &LoopDeps, event: &str, payload: serde_json::Value) {
    let mut payload = payload;
    emit_native_extension(deps, event, &mut payload).await;
    let Some(runtime) = deps.extensions.as_ref() else {
        return;
    };
    if !runtime.has_handlers(event) {
        return;
    }
    if let Err(error) = runtime.emit(event, &mut payload).await {
        crate::ext_diag!("[ext] {event} dispatch failed: {error}");
    }
}

/// Which extension faces want `event`: the native plugin host and the JS
/// runtime, using the same fast-path registry checks every emit helper
/// performs. A hot path can call this before materializing a payload (or the
/// potentially large `args` it would carry) that no face would read.
fn extension_event_faces(deps: &LoopDeps, event: &str) -> (bool, bool) {
    (
        deps.plugin_events
            .as_ref()
            .is_some_and(|host| host.has_native_handlers(event)),
        deps.extensions
            .as_ref()
            .is_some_and(|runtime| runtime.has_handlers(event)),
    )
}

/// [`emit_extension`] with a lazily built payload: `build_payload` runs only
/// when at least one face wants `event` (a native handler is declared, or a JS
/// handler is registered) — the same fast-path checks the eager variant
/// performs. Payload construction here only clones and serializes (no side
/// effects), so not building a payload no handler can read changes nothing but
/// the work done.
async fn emit_extension_lazy<F>(deps: &LoopDeps, event: &str, build_payload: F)
where
    F: FnOnce() -> serde_json::Value,
{
    let (wants_native, wants_js) = extension_event_faces(deps, event);
    if !wants_native && !wants_js {
        return;
    }
    let mut payload = build_payload();
    if wants_native {
        if let Some(host) = deps.plugin_events.as_ref() {
            host.emit_native(event, &mut payload).await;
        }
    }
    if wants_js {
        if let Some(runtime) = deps.extensions.as_ref() {
            if let Err(error) = runtime.emit(event, &mut payload).await {
                crate::ext_diag!("[ext] {event} dispatch failed: {error}");
            }
        }
    }
}

/// `message_end` dispatch whose return value the loop APPLIES.
///
/// Upstream `ExtensionRunner.emitMessageEnd` (runner.ts:1151-1190) chains
/// every `message_end` handler (each sees the previous handler's replacement,
/// the last same-role replacement wins, a role change is dropped with an
/// isolated error) and returns the replacement; `agent-session.ts:1318-1336`
/// then normalizes and copies it back onto the live message object BEFORE the
/// session persists it (persistence happens after `_emitExtensionEvent`, and
/// the loop's `message` reference is the same object), so the transcript, the
/// next provider request, and the tool calls extracted from the message all
/// observe the replacement.
///
/// The JS chaining already lives in the prelude
/// (`extensions/api/js/prelude.js.txt` `dispatchEventToHandlers`
/// `messageEndTouched`): the runtime returns `{message}` only when at least
/// one handler produced one, so an empty/`undefined`/`{message: null}` return
/// — or no replacement at all — reaches this helper as no `message` key and
/// leaves the stream's message untouched (upstream `undefined`).
///
/// The replacement is normalized like upstream's agent-session
/// (agent-session.ts:1324-1332): a user/assistant/toolResult replacement with
/// null/missing `content` becomes `content: []`. A replacement whose role
/// does not match the dispatched message, or that does not deserialize into a
/// conversation message, is dropped with a diagnostic (the chaining layer
/// already dropped role-changing ones; this is defense in depth for a host
/// that dispatches without it). `custom` has no [`Message`] variant in pir
/// (custom messages are transcript entries, not loop messages), so a custom
/// replacement cannot land — reported as a rejected replacement.
async fn emit_message_end<F>(
    deps: &LoopDeps,
    expected_role: &str,
    build_payload: F,
) -> Option<Message>
where
    F: FnOnce() -> JsonValue,
{
    // Both faces observe `message_end` exactly once: the native fan-out first
    // (it cannot replace the message), then the JS chain whose replacement the
    // loop applies. When neither is interested the payload is never built.
    let wants_native = deps
        .plugin_events
        .as_ref()
        .is_some_and(|host| host.has_native_handlers("message_end"));
    let runtime = deps
        .extensions
        .as_ref()
        .filter(|runtime| runtime.has_handlers("message_end"));
    if !wants_native && runtime.is_none() {
        return None;
    }
    let mut payload = build_payload();
    if wants_native {
        if let Some(host) = deps.plugin_events.as_ref() {
            host.emit_native("message_end", &mut payload).await;
        }
    }
    let runtime = runtime?;
    let mut outcome = match runtime.emit("message_end", &mut payload).await {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::ext_diag!("[ext] message_end dispatch failed: {error}");
            return None;
        }
    };
    for error in &outcome.errors {
        // Upstream reports each failing handler through `emitError`, then
        // keeps the last valid replacement.
        crate::ext_diag!(
            "[ext] message_end handler (ext {}) failed: {}",
            error.ext_id,
            error.error
        );
    }
    // Take, do not clone: the outcome is dropped right after this.
    let replacement = outcome.result.get_mut("message").map(JsonValue::take)?;
    if replacement.is_null() {
        return None;
    }
    let replacement = normalize_message_end_content(replacement);
    match serde_json::from_value::<Message>(replacement) {
        Ok(message) if message.role() == expected_role => Some(message),
        Ok(message) => {
            crate::ext_diag!(
                "[ext] message_end replacement role `{}` does not match `{expected_role}`; kept the stream's message",
                message.role()
            );
            None
        }
        Err(error) => {
            crate::ext_diag!("[ext] message_end replacement rejected: {error}");
            None
        }
    }
}

/// Upstream `agent-session.ts:1324-1332` (the null-content normalization
/// applied to an `emitMessageEnd` replacement): an untyped handler may return
/// a message whose `content` is null/missing; normalize it to `[]` for the
/// roles that carry content so the null never enters the transcript or the
/// session history. `system`/`developer` keep upstream's non-normalized
/// treatment (their payloads require content here, so such a replacement is
/// rejected by the deserializer instead of being silently reshaped).
fn normalize_message_end_content(mut value: JsonValue) -> JsonValue {
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    if object.get("content").is_none_or(JsonValue::is_null)
        && matches!(
            object.get("role").and_then(JsonValue::as_str),
            Some("user" | "assistant" | "toolResult")
        )
    {
        object.insert("content".to_owned(), JsonValue::Array(Vec::new()));
    }
    value
}

/// `context` event: handlers may replace the message list before the model
/// request (upstream chaining semantics: each handler sees the previous
/// replacement, and the final replacement is applied). A replacement that
/// fails to deserialize is ignored (the loop keeps its own transcript).
///
/// Both faces observe the event exactly once: the native fan-out first (its
/// in-place `messages` rewrite is visible to the JS handlers), then the JS
/// runtime, whose return value wins when it produced one — the upstream
/// contract for a handler return.
async fn apply_extension_context(deps: &LoopDeps, context: &mut Arc<Context>) {
    let wants_native = deps
        .plugin_events
        .as_ref()
        .is_some_and(|host| host.has_native_handlers("context"));
    let runtime = deps
        .extensions
        .as_ref()
        .filter(|runtime| runtime.has_handlers("context"));
    if !wants_native && runtime.is_none() {
        return;
    }
    let Ok(messages) = serde_json::to_value(&context.messages) else {
        return;
    };
    let mut payload = serde_json::json!({ "type": "context", "messages": messages });
    if wants_native {
        if let Some(host) = deps.plugin_events.as_ref() {
            host.emit_native("context", &mut payload).await;
        }
    }
    let mut replacement = None;
    if let Some(runtime) = runtime {
        match runtime.emit("context", &mut payload).await {
            Ok(mut outcome) => {
                // Take the replacement out of the owned outcome (dropped
                // right after this match) instead of cloning the whole
                // messages array: the clone was pure per-turn cost on a
                // large transcript.
                replacement = outcome
                    .result
                    .get_mut("messages")
                    .map(serde_json::Value::take);
            }
            Err(error) => crate::ext_diag!("[ext] context dispatch failed: {error}"),
        }
    }
    // A JS replacement wins; otherwise a native in-place rewrite (the only
    // shape a native `on_event` has) is applied.
    let replacement = replacement.or_else(|| {
        wants_native
            .then(|| payload.get("messages").cloned())
            .flatten()
    });
    if let Some(messages) = replacement {
        if let Ok(replaced) = serde_json::from_value::<Vec<Message>>(messages) {
            // Copy-on-write only on an actual replacement: a handler that
            // returns no `messages` leaves the transcript allocation shared.
            Arc::make_mut(context).messages = replaced;
        }
    }
}

/// `context_with_system` event (upstream runner.ts two-phase emitContext):
/// fired after the `context` handlers, with the full transcript INCLUDING the
/// leading system message. A handler replacement updates both the message list
/// and the system prompt (extracted from a leading system message); losing the
/// leading system message is reported but honored.
async fn apply_extension_context_with_system(deps: &LoopDeps, context: &mut Arc<Context>) {
    let wants_native = deps
        .plugin_events
        .as_ref()
        .is_some_and(|host| host.has_native_handlers("context_with_system"));
    let runtime = deps
        .extensions
        .as_ref()
        .filter(|runtime| runtime.has_handlers("context_with_system"));
    if !wants_native && runtime.is_none() {
        return;
    }
    // Synthesize the leading system message from the loop's system prompt so
    // the handler sees the same transcript shape upstream does.
    //
    // INVARIANT (no transcript copy): the payload's message array is
    // serialized from the synthesized leading system message plus a BORROWED
    // walk of `context.messages`. The former shape first deep-cloned every
    // message into a temporary `Vec<Message>` (tool-result file contents
    // included) and serialized that — the clone was never read for anything
    // else: the leading-system split below derives the same two facts from
    // `leading_system_message` and `context.messages` directly, and the
    // handler's replacement replaces `context.messages` wholesale.
    let leading_system_message = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
        .map(|system_prompt| {
            Message::System(UserMessage {
                role: UserRole::System,
                content: vec![ContentBlock::Text(TextContent {
                    text: system_prompt.to_owned(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                sections: None,
                timestamp: crate::ai::util::now_ms(),
            })
        });
    let had_leading_system = leading_system_message.is_some()
        || matches!(context.messages.first(), Some(Message::System(_)));
    let mut messages_value: Vec<serde_json::Value> =
        Vec::with_capacity(context.messages.len() + usize::from(leading_system_message.is_some()));
    for message in leading_system_message.iter().chain(context.messages.iter()) {
        let Ok(value) = serde_json::to_value(message) else {
            return;
        };
        messages_value.push(value);
    }
    let mut payload =
        serde_json::json!({ "type": "context_with_system", "messages": messages_value });
    if wants_native {
        if let Some(host) = deps.plugin_events.as_ref() {
            host.emit_native("context_with_system", &mut payload).await;
        }
    }
    let mut replacement = None;
    if let Some(runtime) = runtime {
        match runtime.emit("context_with_system", &mut payload).await {
            Ok(mut outcome) => {
                // Take, do not clone: the outcome is dropped after this match.
                replacement = outcome
                    .result
                    .get_mut("messages")
                    .map(serde_json::Value::take);
            }
            Err(error) => {
                crate::ext_diag!("[ext] context_with_system dispatch failed: {error}")
            }
        }
    }
    let replacement = replacement.or_else(|| {
        wants_native
            .then(|| payload.get("messages").cloned())
            .flatten()
    });
    let Some(replaced) = replacement else {
        return;
    };
    let Ok(mut replaced) = serde_json::from_value::<Vec<Message>>(replaced) else {
        return;
    };
    // Split the leading system message back out.
    let leading_system = if had_leading_system {
        match replaced.first() {
            Some(Message::System(user)) => {
                let text = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                replaced.remove(0);
                Some(text)
            }
            _ => {
                // Fatal for the request (no system prompt left), so it
                // stays an error-level diagnostic: routed (never a raw
                // stderr write into a TUI frame), never collapsed.
                crate::ext_diag_fatal!(
                    "[ext] context_with_system: handler removed the leading system message; the request has no prompt",
                );
                None
            }
        }
    } else {
        None
    };
    // Copy-on-write only now that a replacement is actually installed; the
    // reads above (payload construction) leave the transcript allocation
    // shared.
    let context = Arc::make_mut(context);
    if let Some(system_prompt) = leading_system {
        if !system_prompt.is_empty() {
            context.system_prompt = Some(system_prompt);
        }
    }
    context.messages = replaced;
}

/// Serialize any wire-shaped value for an extension payload (`Null` on the
/// impossible serialization failure). Borrowed, so a payload that carries a
/// message does not clone its content first.
fn value_payload<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Value {
    match serde_json::to_value(value) {
        Ok(value) => value,
        Err(_) => serde_json::Value::Null,
    }
}

/// Serialized `message_end` payload for one conversation message.
fn message_payload(message: &Message) -> serde_json::Value {
    value_payload(message)
}

/// Boxed async future alias.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The loop's shared transcript handle for a provider request.
///
/// The provider boundary takes `Arc<Context>` directly
/// ([`crate::ai::provider::Provider::stream_shared`]), so there is nothing to
/// materialize: this is the identity. Retained under its historical name for
/// out-of-module call sites (the session event-stream callback and tests),
/// which hand the returned handle straight to
/// [`ProviderRegistry::stream_unchecked`](crate::ai::provider::ProviderRegistry::stream_unchecked).
pub fn context_into_owned(context: Arc<Context>) -> Arc<Context> {
    context
}

/// Everything the loop hands to the provider for one conversational request.
///
/// The model and thinking level are the loop's current runtime values — set
/// from [`LoopDeps`] and replaceable by the `prepare_request` hook (upstream
/// `config.model` / `config.reasoning`). `None` keeps the host-baked model of
/// the stream callback (model-less embedders).
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    /// Context streamed to the model (already prepare-request-replaced).
    ///
    /// SHARED, not owned (pir extension over upstream's live-object
    /// handoff): building a request is a refcount bump, so a
    /// transcript-sized context is never deep-cloned just to be handed to
    /// the provider. The provider boundary consumes the same handle
    /// ([`crate::ai::provider::Provider::stream_shared`]).
    pub context: Arc<Context>,
    /// Current runtime model, when the host supplied one.
    pub model: Option<Model>,
    /// Current runtime thinking level (recorded on the assistant message).
    pub thinking_level: ThinkingLevel,
}

/// Injectable model callback retained for lightweight callers and tests.
pub type StreamFn =
    Box<dyn Fn(ProviderRequest) -> BoxFuture<'static, AssistantMessage> + Send + Sync>;

/// A provider stream callback. Unlike [`StreamFn`], this preserves the stream
/// lifecycle and partial events for observers.
pub type EventStreamFn =
    Box<dyn Fn(ProviderRequest) -> BoxFuture<'static, AssistantMessageEventStream> + Send + Sync>;

/// Runtime state available immediately before a conversational provider
/// request (upstream `PrepareRequestContext`).
#[derive(Debug, Clone)]
pub struct PrepareRequestContext {
    /// Context about to be streamed (pending messages already appended).
    ///
    /// SHARED (pir extension over upstream's live-object handoff): the
    /// hook inspects the transcript the loop is about to send without
    /// paying a deep clone of it; a hook that wants to keep or mutate it
    /// can clone it itself, and the loop's own later mutations
    /// clone-on-write (`Arc::make_mut`), so a retained handle never
    /// observes them.
    pub context: Arc<Context>,
    /// Current runtime model; `None` for model-less embedders.
    pub model: Option<Model>,
    /// Current runtime thinking level.
    pub thinking_level: ThinkingLevel,
}

/// Replacement runtime state for the provider request being prepared
/// (upstream `AgentRequestUpdate`); unset fields keep the current values.
#[derive(Debug, Clone, Default)]
pub struct PrepareRequestUpdate {
    /// Replacement context for this and later requests (shared, like the
    /// context the hook was handed).
    pub context: Option<Arc<Context>>,
    /// Replacement model for this and later requests.
    pub model: Option<Model>,
    /// Replacement thinking level for this and later requests.
    pub thinking_level: Option<ThinkingLevel>,
}

/// `prepare_request` hook: invoked before every conversational provider
/// request, including the first (upstream `PrepareRequest`).
pub type PrepareRequestHook = Arc<
    dyn Fn(PrepareRequestContext) -> BoxFuture<'static, Option<PrepareRequestUpdate>> + Send + Sync,
>;

/// `transform_context` hook (upstream `AgentLoopConfig.transformContext`):
/// applied to the run's context immediately before EVERY provider request,
/// including the first, after `prepare_request`.
///
/// The returned context is what the provider receives for this request; it is
/// NOT written back to the run transcript (upstream's `transformContext`
/// returns a message list used only for the LLM call). Use it for
/// AgentMessage-level work such as pruning old messages or injecting external
/// context, without disturbing what the session persists and replays.
///
/// Contract (upstream): must not throw or reject; return the original context
/// (the same `Arc`, a refcount bump) or another safe fallback instead. The
/// hook may still return a NEW context when it wants to transform.
pub type TransformContextHook = Arc<
    dyn Fn(Arc<Context>, Option<AbortSignal>) -> BoxFuture<'static, Arc<Context>> + Send + Sync,
>;

/// Replacement runtime state for the next turn (upstream
/// `AgentLoopTurnUpdate`); unset fields keep the current values.
#[derive(Debug, Clone, Default)]
pub struct PrepareNextTurnUpdate {
    /// Replacement context for the next and later provider requests.
    pub context: Option<Arc<Context>>,
    /// Messages to append before the next provider request, with the normal
    /// message lifecycle (`message_start`/`message_end`) and persistence.
    pub messages: Vec<Message>,
    /// Replacement model for the next and later requests.
    pub model: Option<Model>,
    /// Replacement thinking level for the next and later requests.
    pub thinking_level: Option<ThinkingLevel>,
}

/// Snapshot handed to [`PrepareNextTurnHook`] (upstream
/// `PrepareNextTurnContext extends AgentTurnContext`): the turn that just
/// completed, with its tool results and the run state at `turn_end`.
pub type PrepareNextTurnContext = FinishTurnContext;

/// `prepare_next_turn` hook: called after a completed turn (after
/// `turn_end`, when the loop will continue) and immediately before the next
/// turn starts, BEFORE the compaction hook and the steering poll. Returning
/// `None` preserves the normal scheduling (upstream `prepareNextTurn`).
pub type PrepareNextTurnHook = Arc<
    dyn Fn(Arc<PrepareNextTurnContext>) -> BoxFuture<'static, Option<PrepareNextTurnUpdate>>
        + Send
        + Sync,
>;

/// Decision returned by [`FinishTurnHook`] (upstream `AgentTurnDecision`);
/// `None` preserves normal scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishTurnAction {
    /// Ensure exactly one more provider request.
    Continue,
    /// End the run after this turn.
    End,
}

/// Completed-turn snapshot handed to [`FinishTurnHook`] (upstream
/// `AgentTurnContext`).
#[derive(Debug, Clone)]
pub struct FinishTurnContext {
    /// Zero-based index of the turn this snapshot describes — the SAME value
    /// the loop emitted as `turn_start.turnIndex` (upstream keeps one
    /// `_turnIndex`, reset at agent start, read by both `turn_start` and the
    /// `turn_end` boundary). `turn_end.turnIndex` used to be a separate
    /// 1-based counter, so the two events disagreed on the first turn.
    pub turn_index: usize,
    /// The assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool result messages emitted for the completed turn.
    pub tool_results: Vec<ToolResultMessage>,
    /// Current agent context after the assistant message and tool results
    /// (shared: the hook inspects the live turn snapshot without a deep
    /// clone).
    pub context: Arc<Context>,
    /// Messages this run appended so far (assistant + tool results).
    /// Shared with the loop's own list: handing it to the hook is a refcount
    /// bump, not a deep clone of every produced message on every turn.
    pub new_messages: Arc<Vec<Message>>,
}

/// `finish_turn` hook: called after a completed assistant turn and all of its
/// tool-result messages, but BEFORE `turn_end` is emitted. Runs on error and
/// aborted turns too (their decision is ignored — those remain hard exits).
pub type FinishTurnHook =
    Arc<dyn Fn(FinishTurnContext) -> BoxFuture<'static, Option<FinishTurnAction>> + Send + Sync>;

/// One message a steering/follow-up provider injects into a run.
///
/// Upstream providers return `AgentMessage[]`, which covers text and image
/// user messages plus custom messages (`role: "custom"`). pir's [`Message`]
/// has no custom variant (custom messages are transcript entries, not loop
/// messages), so a custom message travels as the user-shaped projection the
/// model sees plus its `role: "custom"` wire shape and the storage sink that
/// writes the custom entry instead of a regular message entry.
///
/// [`Self::text`] is the compatibility layer for the historical
/// `Vec<String>` providers: a queued text becomes a regular user text message
/// with the full `message_start`/`message_end` lifecycle and persistence.
#[derive(Clone)]
pub struct PromptMessage {
    /// The model-facing message (the custom-message projection for a custom
    /// entry).
    pub message: Message,
    /// Extension `message_start`/`message_end` wire shape; `None` serializes
    /// [`Self::message`]. A custom message carries its own
    /// `customType`/`content`/`display`/`details` object here.
    pub wire: Option<JsonValue>,
    /// Storage override for a message whose entry shape differs from a normal
    /// message entry (a custom-message entry). `None` persists through
    /// `LoopOptions::persist_message` like every loop-produced message.
    ///
    /// The override only runs when the host has a per-message persistence
    /// sink (a store-backed session); a store-less host keeps the message in
    /// the run's produced list so its in-memory transcript receives it.
    pub persist: Option<PreamblePersistFn>,
}

impl std::fmt::Debug for PromptMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PromptMessage")
            .field("message", &self.message)
            .field("wire", &self.wire)
            .field("persist", &self.persist.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

impl PromptMessage {
    /// A plain user text message (the historical `Vec<String>` provider
    /// shape).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            message: Message::User(crate::ai::types::UserMessage {
                role: crate::ai::types::UserRole::User,
                content: vec![crate::ai::types::ContentBlock::Text(
                    crate::ai::types::TextContent {
                        text: text.into(),
                        text_signature: None,
                    },
                )],
                tools_added: None,
                tools_removed: None,
                sections: None,
                timestamp: crate::ai::util::now_ms(),
            }),
            wire: None,
            persist: None,
        }
    }

    /// A caller-built model-facing message (text and/or image content).
    pub fn message(message: Message) -> Self {
        Self {
            message,
            wire: None,
            persist: None,
        }
    }

    /// A custom (`role: "custom"`) message: the user-shaped projection the
    /// model receives, the custom wire shape for the message lifecycle, and
    /// the storage sink that appends the custom entry.
    pub fn custom(message: Message, wire: JsonValue, persist: PreamblePersistFn) -> Self {
        Self {
            message,
            wire: Some(wire),
            persist: Some(persist),
        }
    }

    /// The message's model-visible text (text blocks joined with newlines;
    /// empty for an image-only message). Used by the session's queue mirror
    /// and `queue_update` wire, which stay text.
    pub fn text_content(&self) -> String {
        let content = match &self.message {
            Message::User(user) | Message::System(user) | Message::Developer(user) => &user.content,
            _ => return String::new(),
        };
        content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl From<String> for PromptMessage {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for PromptMessage {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<Message> for PromptMessage {
    fn from(message: Message) -> Self {
        Self::message(message)
    }
}

/// Optional steering/follow-up provider. Steering is checked after a tool
/// batch; follow-up is checked after an otherwise terminal turn — and after
/// every completed tool batch, because a background tool result is delivered
/// through this route and a model that keeps calling tools would otherwise
/// never receive it.
///
/// The returned messages may be plain text ([`PromptMessage::text`]), any
/// model-facing [`Message`] (text plus image blocks), or a custom message
/// (see [`PromptMessage::custom`]).
pub type FollowUpFn = Box<dyn Fn() -> BoxFuture<'static, Vec<PromptMessage>> + Send + Sync>;

/// Optional context compaction hook. The hook may return the SAME shared
/// context unchanged when no compaction is needed.
///
/// SHARED context (pir extension, the memory lane's completion of the loop's
/// copy-on-write shape): the hook is handed the loop's own `Arc<Context>` and
/// returns it untouched when the threshold is not met — no deep clone of the
/// transcript per turn, and the loop's "did it compact?" check is an
/// [`Arc::ptr_eq`] instead of a whole-context structural comparison. A hook
/// that compacts returns a NEW `Arc` (the rebuilt context).
pub type CompactionFn = Box<dyn Fn(Arc<Context>) -> BoxFuture<'static, Arc<Context>> + Send + Sync>;

/// Agent and turn lifecycle events.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart {
        /// SHARED run transcript (refcount bump, never a deep copy): every
        /// in-tree consumer ignores it, and a consumer that wants the data
        /// can retain it without forcing the loop to materialize an owned
        /// context.
        context: Arc<Context>,
    },
    AgentEnd {
        /// SHARED run result: the same allocation [`run_with_options_shared`]
        /// returns (a refcount bump, never a deep copy of terminal/tool
        /// results/new messages). Consumers that only read fields borrow it;
        /// an owner uses [`Arc::unwrap_or_clone`] when it needs the value.
        result: Arc<LoopResult>,
    },
    TurnStart {
        turn: usize,
        /// SHARED run transcript (refcount bump, never a deep copy).
        context: Arc<Context>,
    },
    TurnEnd {
        turn: usize,
        message: AssistantMessage,
        /// This turn's tool-result messages in assistant source order
        /// (upstream `turn_end.toolResults`); empty for a turn that issued no
        /// tool calls and for synthetic terminal turns.
        tool_results: Vec<ToolResultMessage>,
    },
    /// A NON-assistant message entered the transcript (upstream
    /// `message_start` for user/tool-result/custom roles). Assistant messages
    /// use [`AgentEvent::StreamStart`] instead, which is also serialized as
    /// `message_start`.
    MessageStart {
        /// The turn the message is announced for; `0` for the run's starting
        /// prompt (upstream announces it before the first loop turn).
        turn: usize,
        message: Message,
    },
    /// The paired `message_end` for [`AgentEvent::MessageStart`].
    MessageEnd {
        turn: usize,
        message: Message,
    },
    StreamStart {
        turn: usize,
        /// The empty/partial assistant message the stream starts with
        /// (upstream emits `message_start` with the stream's first partial,
        /// agent-loop.ts:416-421; a stream with no start event emits it with
        /// the final message instead, agent-loop.ts:451-456).
        message: AssistantMessage,
    },
    StreamEvent {
        turn: usize,
        event: AssistantMessageEvent,
    },
    StreamEnd {
        turn: usize,
        message: AssistantMessage,
    },
    ToolExecutionStart {
        turn: usize,
        event: crate::agent::tool_batch::ToolExecutionEvent,
        /// The call's arguments (upstream `tool_execution_start.args`), so a
        /// JSON/SDK consumer can render the call from the start frame alone.
        args: serde_json::Value,
    },
    ToolExecutionEnd {
        turn: usize,
        event: crate::agent::tool_batch::ToolExecutionEvent,
        /// The finished result in upstream `AgentToolResult` wire shape
        /// (`content` block array, optional `details`/`structuredContent`).
        result: serde_json::Value,
    },
    /// A running tool reported a partial result (upstream
    /// `tool_execution_update`, agent-loop.ts:792). Carries the call identity
    /// so a host can route the update to the right tool block; root-level
    /// updates only (nested calls keep their own event sink).
    ToolExecutionUpdate {
        turn: usize,
        call_id: String,
        tool_name: String,
        /// Parent call id for nested (tool-issued) calls; `None` for
        /// model-issued calls.
        parent_call_id: Option<String>,
        /// The call's arguments (upstream `tool_execution_update.args`).
        args: serde_json::Value,
        /// The partial result in upstream `partialResult` shape.
        partial: serde_json::Value,
    },
    Compaction {
        turn: usize,
    },
}

/// Event listener for [`AgentEvent`] values.
pub type EventHandler = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Async sink called with every message the run appends (upstream persists
/// each `message_end` DURING the run, agent-session.ts:1144-1163).
pub type PersistMessageFn = Arc<dyn Fn(Message) -> BoxFuture<'static, ()> + Send + Sync>;

/// Awaits the MCP servers a tool batch may reach before the batch executes
/// (upstream the `mcp` extension's `pi.on("tool_call")` handler,
/// `extensions/mcp/index.ts:1191-1209`): a tool that declares `reachesTools`
/// waits for the servers its reach matches, the aggregate resource tools wait
/// for every enabled server, and a direct MCP call waits for its owner
/// server, so those servers' tools are registered before the call runs.
/// The hook owns everything it needs (the session's plugin host); the loop
/// hands it the run's registry and the batch's calls.
pub type ToolCallWaitFn =
    Arc<dyn Fn(Arc<ToolRegistry>, Vec<AgentToolCall>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Lazy, shareable view of the run registry's name → schema map handed to a
/// `message_end` repair hook ([`MessageEndRepairContext::active_tools`]).
///
/// Building the eager map means one name clone plus one FULL JSON-schema
/// clone per registered tool for EVERY assistant message, even when the
/// repair decides to change nothing — measurable on a tool-heavy run whose
/// assistant messages rarely need a repair.
///
/// The view instead keeps the registry handle only. [`Self::iter`]
/// (the seam the repair core reads) materializes the map once, on the first
/// read, and clones of the view share that one materialization: the loop
/// creates one view per run and clones it into every message's context, so
/// a hook that never reads the schemas pays no clone at all and one that does
/// pays a single materialization per run. A hook that needs the values by
/// value can `.cloned().collect()` the iterator, which is exactly the eager
/// cost the repair itself asked for.
#[derive(Clone)]
pub struct ActiveToolSchemas {
    tools: Arc<ToolRegistry>,
    materialized: Arc<std::sync::OnceLock<MaterializedToolSchemas>>,
}

/// One materialized `(tool name, input schema)` snapshot, shared by every clone
/// of an [`ActiveToolSchemas`] view.
type MaterializedToolSchemas = Arc<Vec<(String, JsonValue)>>;

impl ActiveToolSchemas {
    /// A lazy view over `tools`; nothing is cloned until the first
    /// [`Self::iter`].
    pub fn new(tools: Arc<ToolRegistry>) -> Self {
        Self {
            tools,
            materialized: Arc::new(std::sync::OnceLock::new()),
        }
    }

    fn materialize(&self) -> &MaterializedToolSchemas {
        self.materialized.get_or_init(|| {
            Arc::new(
                self.tools
                    .iter()
                    .map(|tool| {
                        (
                            tool.name().to_owned(),
                            tool.parameters().unwrap_or(JsonValue::Null),
                        )
                    })
                    .collect(),
            )
        })
    }

    /// Iterate the `(name, parameters)` pairs, materializing the map on the
    /// first call. The schema stays borrowed (`&JsonValue`), so a caller that
    /// only needs names never clones a schema.
    pub fn iter(&self) -> std::slice::Iter<'_, (String, JsonValue)> {
        self.materialize().iter()
    }
}

impl std::fmt::Debug for ActiveToolSchemas {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveToolSchemas")
            .field(
                "materialized",
                &self.materialized.get().map(|schemas| schemas.len()),
            )
            .finish()
    }
}

/// The finalized assistant message a `message_end` repair may rewrite (the
/// `tool-repair` builtin's native seam, scope doc §2): the message is handed
/// over by value together with the runtime state the repair core needs — the
/// current model id (the model-gated strip-pass gates read it) and the run
/// registry's name→parameters schemas (the live-schema repair and the grammar
/// recovery's known-tool gate read them).
#[derive(Debug, Clone)]
pub struct MessageEndRepairContext {
    /// The assistant message as the stream finalized it, before it is
    /// assigned to the terminal, pushed into the context, persisted, or its
    /// tool calls extracted.
    pub message: AssistantMessage,
    /// Current runtime model; `None` for model-less embedders (the strip
    /// passes stay off, like the TS `safeGetModel` miss).
    pub model: Option<Model>,
    /// Active tool schemas, name → `AgentTool::parameters()` (the run
    /// registry; `parameters()` is `None`-free via `JsonValue::Null`, which
    /// the repair core treats as "no schema, skip"). Lazily materialized and
    /// shared across a run's messages: see [`ActiveToolSchemas`].
    pub active_tools: ActiveToolSchemas,
}

/// `message_end` repair hook: invoked once per finalized assistant message,
/// between the extension `message_end` dispatch and the message's terminal /
/// context / persistence / tool-call extraction. `Some(message)` replaces the
/// finalized message (the repair core commits only a re-validated repair);
/// `None` keeps the stream's message untouched.
pub type MessageEndRepairFn = Arc<
    dyn Fn(MessageEndRepairContext) -> BoxFuture<'static, Option<AssistantMessage>> + Send + Sync,
>;

/// One agent-level retry lifecycle transition (upstream `auto_retry_start` /
/// `auto_retry_end`, agent-session.ts:220-222). The loop emits these through
/// [`LoopDeps::retry_events`] so the session can forward them to its
/// `SessionEvent` subscribers; the transport's own retry observers
/// (`StreamOptions::hooks.on_retry_start` / `on_retry_end`) are a separate
/// budget and stay unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRetryEvent {
    /// A re-issued turn was scheduled (before the backoff sleep).
    Start {
        /// 1-based retry ordinal.
        attempt: u32,
        /// The configured agent-level retry budget.
        max_attempts: u32,
        /// The backoff about to be slept.
        delay_ms: u64,
        /// The error that triggered the retry (upstream
        /// `message.errorMessage || "Unknown error"`).
        error_message: String,
    },
    /// The retry lifecycle closed: the retried attempt succeeded, exhausted
    /// the budget, failed non-retryably, or was cancelled.
    End {
        success: bool,
        attempt: u32,
        /// The final error (upstream `finalError`); `None` on success and when
        /// the provider reported no message.
        final_error: Option<String>,
    },
}

/// Observer for [`AgentRetryEvent`] values.
pub type AgentRetryEventSink = Arc<dyn Fn(AgentRetryEvent) + Send + Sync>;

/// Persistence sink for one [`PreambleMessage`] the loop appends. Unlike
/// [`PersistMessageFn`] the failure is returned: the loop has not issued the
/// first provider request yet, and a starting message that could not be stored
/// must fail the run before it burns a request (the pre-loop startup append
/// used to propagate this directly).
pub type PreamblePersistFn =
    Arc<dyn Fn(Message) -> BoxFuture<'static, crate::Result<()>> + Send + Sync>;

/// One starting message the loop announces at the top of a run (upstream
/// `runAgentLoop`'s `initialMessages`, agent-loop.ts:119-131).
///
/// The loop always drives the full message lifecycle for a preamble message —
/// host `message_start`/`message_end` plus the extension `message_start` /
/// `message_end` pair, with the extension replacement applied — in upstream
/// order (`agent_start` → `turn_start` → `message_start`/`message_end`).
///
/// Two ownership shapes exist:
/// - `append: true` (the session/print prompt path): the host has NOT added
///   the message anywhere. The loop pushes the FINAL message into the context
///   and persists it at its `message_end` (`persist` when set, otherwise
///   `LoopOptions::persist_message`), so a handler replacement is what the
///   first request and storage record.
/// - `append: false` (`LoopOptions::preamble_messages`): the host already
///   appended and persisted the message. The loop only announces it and, when
///   a handler replaces it, rewrites the matching transcript entry (storage is
///   the host's concern; print already wrote it).
pub struct PreambleMessage {
    /// The model-facing message.
    pub message: Message,
    /// Wire shape for the extension `message_start`/`message_end` payloads
    /// (`role: "custom"` for a run started by a custom message); `None`
    /// serializes [`Self::message`].
    pub wire: Option<JsonValue>,
    /// Whether the loop appends/persists the finalized message (see the type
    /// docs).
    pub append: bool,
    /// Persistence override for an appended message (a custom-message entry
    /// keeps its own storage shape); `None` uses `persist_message`. A returned
    /// error stops the run before the first provider request.
    pub persist: Option<PreamblePersistFn>,
}

/// Optional behavior for [`run_with_options`].
#[derive(Default)]
pub struct LoopOptions {
    pub events: Option<EventHandler>,
    pub steering: Option<FollowUpFn>,
    pub compaction: Option<CompactionFn>,
    /// `transform_context` (upstream `transformContext`): transforms the
    /// context handed to each provider request without mutating the run
    /// transcript. `None` hands the context through unchanged.
    pub transform_context: Option<TransformContextHook>,
    /// `prepare_next_turn` (upstream `prepareNextTurn`): replacement
    /// context/model/thinking state and messages to append after a completed
    /// turn, before the next turn starts. `None` keeps the current state.
    pub prepare_next_turn: Option<PrepareNextTurnHook>,
    /// When present, this is used instead of `LoopDeps::stream` and gives the
    /// caller full stream lifecycle events.
    pub event_stream: Option<EventStreamFn>,
    /// Per-message persistence sink: invoked with every message the run
    /// appends to the context (tool-loadout declarations, assistant messages,
    /// tool results) at its `message_end`, in append order. Hosts use it to
    /// persist during the run so a mid-run crash loses nothing.
    pub persist_message: Option<PersistMessageFn>,
    /// Messages the caller already appended to the starting context for THIS
    /// run (the starting prompt and any next-turn messages). The loop emits
    /// their host `message_start`/`message_end` pair and the extension
    /// `message_start`/`message_end` pair right after the first `turn_start`
    /// (upstream announces the initial messages after `agent_start` and
    /// `turn_start`), but does not re-add or re-persist them. A handler
    /// replacement rewrites the matching transcript entry. Empty for
    /// embedders that own their events.
    pub preamble_messages: Vec<Message>,
    /// Starting messages the loop itself appends and persists (the session
    /// prompt path): each entry drives the full message lifecycle and the
    /// handler replacement is applied BEFORE the message enters the context
    /// and storage. See [`PreambleMessage`].
    pub preamble: Vec<PreambleMessage>,
}

/// Dependencies for a single agent run.
pub struct LoopDeps {
    /// Streams assistant messages from the model.
    pub stream: StreamFn,
    /// Registered tools.
    pub tools: Arc<ToolRegistry>,
    /// Tool execution scheduler and its configuration.
    pub scheduler: ToolScheduler,
    /// Optional tool lifecycle hooks (`beforeToolCall` / `afterToolCall`).
    ///
    /// `Arc` so a session can install one hook handle for every run it drives:
    /// the run hands the same handle to the scheduler (which also gives it to
    /// detached background calls, so the after-hook still applies to their
    /// real results) and to the nested `ctx.executeTool` executor.
    ///
    /// Timing is the scheduler's (upstream `agent-loop.ts:738-766,880-912`):
    /// the extension/plugin `tool_call` face runs after validation and before
    /// the before-hook, the before-hook runs before execution, the after-hook
    /// runs after the tool and before the extension/plugin `tool_result` face,
    /// and both faces stay exactly-once — installing hooks never re-dispatches
    /// an extension event.
    pub hooks: Option<Arc<ToolHooks>>,
    /// Optional JS extension runtime. When set, the scheduler dispatches
    /// `tool_call` (after validation, before execution) and `tool_result`
    /// (after the tool/after-hook, before that call's `tool_execution_end`)
    /// through it.
    pub extensions: Option<crate::extensions::ExtensionHandle>,
    /// Unified plugin host for the NATIVE (Rust) event fan-out.
    ///
    /// `extensions` stays the JS dispatch face; this host delivers the same
    /// loop events to the non-JS plugins exactly once (the host's JS adapter
    /// is skipped, so a session with a JS runtime does not dispatch twice).
    /// The session installs its own host here; a builtin-only plan (plan-only
    /// handle, no JS runtime) is the case that motivates the seam: `phone`'s
    /// `on_event` and any inline Rust plugin observe `agent_start`,
    /// `message_*`, `tool_execution_*`, and `agent_end` without one JS
    /// handler. `None` (print mode, bare embedders) leaves dispatch unchanged.
    pub plugin_events: Option<Arc<crate::plugins::PluginHost>>,
    /// MCP reach readiness hook: when set, every tool batch awaits the MCP
    /// servers its calls may reach before the scheduler executes it (upstream
    /// the `mcp` extension's `tool_call` handler). The session builds it from
    /// the unified plugin host ([`crate::agent_session`]); a host without one
    /// (print mode) leaves it `None`.
    pub tool_call_wait: Option<ToolCallWaitFn>,
    /// Assistant `message_end` repair hook (the `tool-repair` builtin's seam,
    /// scope doc §2): when set, the finalized assistant message passes through
    /// the hook BEFORE the terminal assignment, context push, persistence, and
    /// tool-call extraction — so a committed repair is what the transcript
    /// records and what `validate_arguments` sees at batch preflight. `None`
    /// (every host that does not build the hook) changes nothing.
    pub message_end_repair: Option<MessageEndRepairFn>,
    /// Runtime model handed to the `prepare_request` hook (upstream
    /// `config.model`); `None` for model-less embedders.
    pub model: Option<Model>,
    /// Runtime thinking level handed to the `prepare_request` hook and
    /// recorded on every assistant message (upstream `config.reasoning`).
    pub thinking_level: ThinkingLevel,
    /// Called after each completed turn, before `turn_end`; `End` stops the
    /// loop, `Continue` guarantees one more provider request.
    pub finish_turn: Option<FinishTurnHook>,
    /// Called before every conversational provider request; may replace the
    /// context, model, and thinking level.
    pub prepare_request: Option<PrepareRequestHook>,
    /// Called when a turn has no more tool calls; returns follow-up prompts.
    /// Ordinary user follow-ups are drained only at the END of the inner tool
    /// loop (upstream `getFollowUpMessages` after `hasMoreToolCalls` turns
    /// false), never between tool batches.
    pub follow_up: Option<FollowUpFn>,
    /// Called at every completed tool batch; returns detached background tool
    /// results to inject into a model that keeps calling tools (A6). Kept
    /// separate from [`Self::follow_up`] so a queued user message cannot
    /// hijack the middle of a tool batch.
    pub background_follow_up: Option<FollowUpFn>,
    /// Working directory supplied to every tool invocation.
    pub cwd: std::path::PathBuf,
    /// Maximum number of turns before the loop gives up. The default is
    /// UNBOUNDED (upstream's loop has no turn cap); hosts that want a cap set
    /// it explicitly. Reaching the cap ends the run with a synthesized
    /// `StopReason::Error` terminal (see [`TURN_CAP_ERROR_PREFIX`]), never a
    /// silent break: a truncated run must not look like a finished one.
    pub max_turns: usize,
    /// Agent-level turn retry policy (upstream `AgentSession._prepareRetry`):
    /// a turn that ends with a retryable provider error is re-issued with
    /// exponential backoff instead of ending the run.
    ///
    /// The default is the upstream settings default (3 retries, 2 s base,
    /// 60 s cap — `SettingsManager::retry_settings`); `None` disables
    /// agent-level retries for this run. Hosts that resolve `settings.retry`
    /// themselves should install
    /// `SettingsManager::agent_retry_policy()` here. This budget is separate
    /// from the transport's own pre-first-byte retry
    /// (`StreamOptions::max_retries`): the loop re-issues the whole turn once
    /// per attempt, and never re-issues a status failure the transport
    /// already exhausted (see [`retryable_turn_error`]).
    pub retry: Option<crate::ai::util::AgentRetryPolicy>,
    /// Observer for the agent-level retry lifecycle (upstream
    /// `auto_retry_start` / `auto_retry_end`). The loop is the only writer: a
    /// scheduled retry reports `Start` before its backoff, and the lifecycle
    /// closes with exactly one `End` (success, exhausted/failed, or
    /// `"Retry cancelled"`). The transport's own retry budget does not use
    /// this sink, so the two retry layers never double-report.
    pub retry_events: Option<AgentRetryEventSink>,
    /// Host-provided session environment for tools that spawn agent runs (the
    /// `subagent` builtin; upstream `ctx.model`/`ctx.hasUI`).
    pub session_env: Option<Arc<crate::agent::tool_execution::ToolSessionEnv>>,
    /// Live tool partial-result channel for hosts that do NOT build a
    /// [`crate::agent::tool_execution::ToolSessionEnv`] (print mode): the loop
    /// hands it to every tool context as `on_update`, the transport of the
    /// extension `tool_execution_update` event (upstream
    /// `emitToolExecutionUpdate`, agent-loop.ts:779-788). Takes precedence
    /// over `session_env.on_update` when a host sets both.
    pub tool_updates: Option<crate::agent::tool_execution::ToolUpdateFn>,
    /// Names tools may call through `ctx.executeTool()` (upstream
    /// `_getCallableTools`): active `direct` tools plus registered
    /// `codemode`/`deferred` tools. `None` keeps the registry-wide default
    /// (every registered tool is callable), which is also what `model-only`
    /// exposure needs to forbid — a set without the name refuses nested calls.
    pub nested_callable: Option<Arc<std::collections::BTreeSet<String>>>,
    /// Per-runtime placeholder-commit and tool-batch boundary tracker for
    /// detached (`background: true`) tool calls
    /// ([`crate::agent::commit::CommitTracker`]).
    ///
    /// One tracker belongs to the host's background registry (`AgentSession`,
    /// the print host). The loop holds its batch barrier from the assistant
    /// message that carries a tool batch through the batch's last
    /// `tool_result` write, so a background completion can never persist
    /// between a tool call and its result. `None` (a host with no background
    /// calls) skips the barrier entirely.
    pub commit_tracker: Option<Arc<crate::agent::commit::CommitTracker>>,
}

impl Default for LoopDeps {
    fn default() -> Self {
        Self {
            stream: Box::new(|_| Box::pin(async { AssistantMessage::default() })),
            tools: Arc::new(ToolRegistry::new()),
            scheduler: ToolScheduler::new(ToolSchedulerConfig::default()),
            hooks: None,
            extensions: None,
            plugin_events: None,
            tool_call_wait: None,
            message_end_repair: None,
            model: None,
            thinking_level: ThinkingLevel::Off,
            finish_turn: None,
            prepare_request: None,
            follow_up: None,
            background_follow_up: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            max_turns: usize::MAX,
            retry: Some(crate::ai::util::AgentRetryPolicy {
                max_retries: crate::config::defaults::settings_defaults::RETRY_MAX_RETRIES,
                base_delay_ms: crate::config::defaults::settings_defaults::RETRY_BASE_DELAY_MS,
                max_agent_delay_ms:
                    crate::config::defaults::settings_defaults::RETRY_MAX_AGENT_DELAY_MS,
            }),
            retry_events: None,
            session_env: None,
            tool_updates: None,
            nested_callable: None,
            commit_tracker: None,
        }
    }
}

impl LoopDeps {
    /// Replace the injectable callback with a real [`ProviderRegistry`] request.
    pub fn with_provider(
        mut self,
        registry: Arc<ProviderRegistry>,
        model: Model,
        options: StreamOptions,
    ) -> Self {
        self.model = Some(model.clone());
        self.stream = registry_stream(registry, model, options);
        self
    }

    /// Use a configured provider directly, including keyless local providers.
    pub fn with_provider_unchecked(
        mut self,
        registry: Arc<ProviderRegistry>,
        model: Model,
        options: StreamOptions,
    ) -> Self {
        self.model = Some(model.clone());
        self.stream = registry_stream_unchecked(registry, model, options);
        self
    }
}

/// Result of a completed agent run.
#[derive(Debug, Clone)]
pub struct LoopResult {
    /// The final assistant message returned by the provider (including errors).
    pub terminal: AssistantMessage,
    /// Complete conversation context after assistant and tool turns.
    ///
    /// SHARED with the run's internal handle: returning it is a refcount
    /// bump, so the terminal result never deep-copies the transcript.
    pub context: Arc<Context>,
    /// Tool-result messages appended during the run, in source order.
    pub tool_results: Vec<ToolResultMessage>,
    /// Every message the run appended (assistant + tool results), in append
    /// order. Upstream persists each `message_end` during the run
    /// (agent-session.ts:1144-1163); hosts persist this list so multi-turn
    /// runs keep their intermediate assistant (toolCall) messages.
    pub new_messages: Vec<Message>,
    /// The finalized starting messages the loop appended for this run (the
    /// [`PreambleMessage`] entries with `append: true`), in order, after any
    /// `message_end` replacement. They are NOT part of [`Self::new_messages`]
    /// (upstream's `newMessages` includes the initial messages; pir keeps the
    /// long-standing produced-only contract for `agent_end.messages`), so an
    /// ephemeral host extends its in-memory transcript with this list before
    /// [`Self::new_messages`].
    pub preamble: Vec<Message>,
    /// Number of turns executed.
    pub turns: usize,
    /// Whether the run stopped because it was aborted.
    pub aborted: bool,
    /// Total token usage across EVERY assistant message of the run (the
    /// terminal message alone under-counts multi-turn runs; upstream
    /// aggregates per-assistant-message usage the same way).
    pub usage_total: u64,
    /// Whether a post-run retry would be scheduled for this result (upstream
    /// `agent_end.willRetry`, agent-session.ts:1144/1200-1214).
    ///
    /// pir re-issues retryable turns INSIDE this run, so a result that reaches
    /// `AgentEnd` has already exhausted every budget it was allowed: the value
    /// is `false` for a terminal error that the loop would not re-issue. It is
    /// computed from the same attempt counter and structural verdict the loop
    /// uses for [`AgentRetryEvent`] (never hardcoded), so a host that changes
    /// the loop's retry semantics cannot leave this field stale.
    pub will_retry: bool,
}

/// Run the agent loop with default lifecycle options.
pub async fn run(deps: LoopDeps, context: Context, signal: Option<AbortSignal>) -> LoopResult {
    run_with_options(deps, context, signal, LoopOptions::default()).await
}

/// Run the agent loop until no more work remains or a stop condition fires.
///
/// Assistant and tool-result messages are appended to the request context
/// before the next turn. This is the key distinction between an orchestration
/// loop and a sequence of independent model calls.
/// Run the agent loop with default lifecycle options, returning the run
/// result as the SAME allocation carried by [`AgentEvent::AgentEnd`].
///
/// An observer that retains the event result and the caller therefore share
/// one `LoopResult` (a refcount bump), instead of the run paying a deep clone
/// of terminal/tool results/new messages at emit time.
pub async fn run_with_options_shared(
    deps: LoopDeps,
    context: Context,
    signal: Option<AbortSignal>,
    options: LoopOptions,
) -> Arc<LoopResult> {
    // Run-level span (upstream `TelemetryContext.startSpan("agent.run")`): one
    // span per run, carrying the requested model id; per-turn spans nest under
    // it. Async-correct: the span wraps the run body via `Instrument` rather
    // than a guard held across await points.
    let model = deps
        .model
        .as_ref()
        .map(|model| model.id.clone())
        .unwrap_or_else(|| "-".to_string());
    let span = tracing::info_span!("agent.run", model = %model);
    run_with_options_in_span(deps, context, signal, options)
        .instrument(span)
        .await
}

/// Run the agent loop with default lifecycle options, returning the result by
/// value.
///
/// The shared allocation is handed back move-style when no observer retained
/// it (`Arc::unwrap_or_clone`); only an observer that keeps the `AgentEnd`
/// result forces the one clone the old eager implementation always paid.
pub async fn run_with_options(
    deps: LoopDeps,
    context: Context,
    signal: Option<AbortSignal>,
    options: LoopOptions,
) -> LoopResult {
    Arc::unwrap_or_clone(run_with_options_shared(deps, context, signal, options).await)
}

/// Body of [`run_with_options_shared`], executed inside the `agent.run` span.
async fn run_with_options_in_span(
    deps: LoopDeps,
    mut context: Context,
    signal: Option<AbortSignal>,
    mut options: LoopOptions,
) -> Arc<LoopResult> {
    // Origin for the first-request/first-event timing lines (`PI_TIMING=1`).
    let run_started_at = crate::config::timings::start_phase();
    if context.tools.is_none() {
        let mut definitions = deps
            .tools
            .iter()
            .map(|tool| Tool {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool
                    .parameters()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                // Upstream fcff255b0: each tool declares its own constraint
                // (`constrainedSampling`); only read/bash/edit/write prefer
                // strict JSON-schema sampling, every other tool is
                // unconstrained. Providers without support ignore the hint.
                constrained_sampling: tool.constrained_sampling(),
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        context.tools = Some(definitions);
    }
    // The run's transcript lives behind ONE shared handle for the whole run:
    // every handoff (hooks, the provider request, the event payloads, the
    // result) is a refcount bump, and every mutation clones only when another
    // handle is still alive (`Arc::make_mut`) - the former shape deep-cloned
    // the whole context at each handoff.
    let mut context: Arc<Context> = Arc::new(context);
    emit_lazy(&options.events, || AgentEvent::AgentStart {
        context: Arc::clone(&context),
    });
    emit_extension(
        &deps,
        "agent_start",
        serde_json::json!({ "type": "agent_start" }),
    )
    .await;

    // Initial-message lifecycle (upstream `runAgentLoop`, agent-loop.ts:126-131:
    // `agent_start` → `turn_start` → each initial message's `message_start` /
    // `message_end`). Both faces observe the pair, and the extension
    // `message_end` replacement is applied BEFORE the message enters the
    // context and storage — for `PreambleMessage::append` entries this is what
    // makes the replacement the first request's and the durable transcript's
    // message. The first `turn_start` is emitted here (rather than in the
    // loop's first iteration) so an abort requested while the run was starting
    // still records its prompt, exactly like upstream's initial messages.
    let mut first_turn_start_emitted = false;
    let mut preamble_failed = false;
    let mut finalized_preamble: Vec<Message> = Vec::new();
    let mut preamble = std::mem::take(&mut options.preamble);
    preamble.extend(
        std::mem::take(&mut options.preamble_messages)
            .into_iter()
            .map(|message| PreambleMessage {
                message,
                wire: None,
                append: false,
                persist: None,
            }),
    );
    if !preamble.is_empty() {
        emit_lazy(&options.events, || AgentEvent::TurnStart {
            turn: 1,
            context: Arc::clone(&context),
        });
        emit_extension(
            &deps,
            "turn_start",
            serde_json::json!({
                "type": "turn_start",
                "turnIndex": 0,
                "timestamp": crate::ai::util::now_ms(),
            }),
        )
        .await;
        first_turn_start_emitted = true;
        for entry in preamble {
            let appended = entry.append;
            match apply_preamble_message(&deps, &options, entry, &mut context).await {
                Ok(finalized) => {
                    if appended {
                        finalized_preamble.push(finalized);
                    }
                }
                Err(error) => {
                    // A starting message that could not be stored fails the
                    // run BEFORE the first provider request (the caller
                    // receives the same error through its persistence sink).
                    crate::ext_diag!("[session] starting-message persistence failed: {error}");
                    preamble_failed = true;
                    break;
                }
            }
        }
    }

    // Runtime request state: replaceable per request by `prepare_request`
    // (upstream replaces `config.model` / `config.reasoning` the same way).
    let mut current_model = deps.model.clone();
    let mut current_thinking = deps.thinking_level;

    // Nested tool-call state (upstream `NestedToolCallRunner`): one shared
    // scope per run; events flow to the extension runtime and the event
    // handler with `parentToolCallId` attached.
    let mut deps = deps;
    let hooks: Option<Arc<ToolHooks>> = deps.hooks.take();
    // The extension/plugin faces observe the per-call contracts at their
    // upstream timings inside the scheduler (`tool_call` after validation and
    // before execution, `tool_result` after the tool/after-hook and before
    // that call's `tool_execution_end`), so install them once on the run's
    // scheduler before the nested state clones it.
    if deps.extensions.is_some() || deps.plugin_events.is_some() {
        let faces = crate::agent::tool_execution::ToolEventFaces::new(
            deps.extensions.clone(),
            deps.plugin_events.clone(),
        );
        if faces.enabled() {
            deps.scheduler = deps.scheduler.clone().with_tool_event_faces(faces);
        }
    }
    // Lazy active-tool schemas for the `message_end_repair` seam: ONE lazy
    // view per run, cloned (refcount bump) into every assistant message's
    // context. The name/schema map materializes only if the hook reads
    // `active_tools`, and then only once per run — the former shape rebuilt
    // it (one name clone + one full JSON-schema clone per registered tool)
    // for every assistant message, including messages the repair leaves
    // untouched. `None` without a hook, so that path clones nothing.
    let repair_active_tools = deps
        .message_end_repair
        .as_ref()
        .map(|_| ActiveToolSchemas::new(Arc::clone(&deps.tools)));
    let turn_counter = Arc::new(AtomicUsize::new(0));
    let nested_sink = nested_event_sink(
        &options.events,
        &deps.extensions,
        &deps.plugin_events,
        &turn_counter,
    );
    let nested = NestedShared::new(
        Arc::clone(&deps.tools),
        deps.scheduler.clone(),
        hooks.clone(),
        deps.extensions.clone(),
        deps.cwd.clone(),
        signal.clone(),
        deps.scheduler.mode() == crate::agent::tool_batch::ToolExecutionMode::Sequential,
        nested_sink,
        deps.session_env.clone(),
        deps.nested_callable.clone(),
    );
    // The shared state starts inert (JS-only nested routing, no reach wait).
    // This is the run host's one install of the unified plugin host and the
    // MCP reach hook; `configure_plugins` is first-set-wins, and no tool has
    // run yet, so every nested `ctx.executeTool` below sees both seams.
    nested.configure_plugins(deps.plugin_events.clone(), deps.tool_call_wait.clone());
    // Nested tools announce their live partials on the same host event stream
    // as model-issued calls (upstream routes every tool `onUpdate` through
    // the agent event sink), attributed to the immediate caller.
    nested.configure_host_events(options.events.clone(), Arc::clone(&turn_counter));

    let mut tool_results = Vec::new();
    // Shared so a per-turn `finish_turn` snapshot is a refcount bump (the
    // former shape deep-cloned the run's whole produced list every turn).
    let mut new_messages: Arc<Vec<Message>> = Arc::new(Vec::new());
    let mut turns = 0usize;
    let mut terminal = AssistantMessage::default();
    // Run-level agent retry state (upstream `AgentSession._retryAttempt`):
    // the counter persists across turns and is reset by a successful
    // assistant response, so the budget is not silently refreshed per turn.
    // `auto_retry_*` and `agent_end.willRetry` read this SAME state.
    let mut retry_attempt: u32 = 0;
    // The structural verdict of the LAST finalized attempt: kept beside the
    // retry counter so the run's `will_retry` is computed from the same loop
    // state that decided (or refused) a re-issue.
    let mut last_turn_retry_class: Option<crate::ai::error::TurnRetryClass> = None;
    // Queue-poll discipline (upstream agent-loop.ts:176,201,295): the
    // steering queue is polled at most once per model request. A delivery
    // made after a completed turn is carried into the next request instead of
    // being polled a second time, so `one-at-a-time` mode delivers exactly
    // one queued message per turn (upstream guards the second poll with
    // `pendingMessages.length === 0`; follow-up deliveries are carried the
    // same way).
    let mut steering_carried = false;
    // Last completed turn offered to `prepare_next_turn` at the top of every
    // following iteration (upstream `lastCompletedTurn`). Kept across an
    // explicit continuation, exactly like upstream: the same turn is offered
    // again until a new assistant turn replaces it. It deliberately holds
    // ONLY the immutable per-turn payload; the live transcript and
    // produced-message handles are attached at the hook call (see
    // [`record_completed_turn`]), so the loop keeps no second reference into
    // its own `Arc<Context>`/`Arc<Vec<Message>>` between turns — such a
    // reference made the next turn's first `Arc::make_mut` deep-copy the
    // whole transcript, every tool output included, once per turn.
    // `None` (and never populated) when no `prepare_next_turn` hook is
    // installed, since nothing would read it.
    let mut last_completed_turn: Option<CompletedTurn> = None;
    // Total usage across every assistant message of the run (upstream folds
    // per-assistant-message usage into the run totals; the terminal message
    // alone under-counts multi-turn runs).
    let mut usage_total = 0u64;
    // Degenerate-loop guard state (see [`REPEAT_STREAK_STOP`]): the last
    // single-call execution digest, how many turns in a row it repeated
    // unchanged, the digest a nudge already covered (a second full streak of
    // the SAME call stops instead of nudging again), and how many nudges this
    // run has spent ([`REPEAT_NUDGE_LIMIT`]).
    let mut repeat_digest: Option<u64> = None;
    let mut repeat_streak: usize = 0;
    let mut repeat_warned_digest: Option<u64> = None;
    let mut repeat_nudges: usize = 0;
    // declareToolChanges state (upstream agent-loop.ts:333-375): the set of
    // tool names declared to the model so far. Seeded from the transcript's
    // system declaration messages so a resumed/replayed session does not
    // re-declare the tools it already declared.
    let mut last_declared: std::collections::BTreeSet<String> =
        declared_tool_names(&context.messages).unwrap_or_default();

    'agent: loop {
        if preamble_failed || signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            break;
        }
        // Turn cap (pir-native; upstream's loop has no cap). A capped run is
        // INCOMPLETE, so it must not look finished: synthesize an
        // identifiable `StopReason::Error` terminal (the repeat guard's
        // shape) and emit it as a real transcript message, so every host —
        // print's exit mapping included — can report a truncated run instead
        // of an empty success. The cap value itself is unchanged.
        if turns >= deps.max_turns {
            let mut stop = turn_cap_error(deps.max_turns, &terminal);
            tracing::warn!(
                target: "pir::agent",
                turns,
                max_turns = deps.max_turns,
                "agent loop: stopping at the turn cap (incomplete run)"
            );
            emit_lazy(&options.events, || AgentEvent::StreamStart {
                turn: turns,
                message: stop.clone(),
            });
            emit_lazy(&options.events, || AgentEvent::StreamEnd {
                turn: turns,
                message: stop.clone(),
            });
            // The synthetic message is a real transcript message, so it goes
            // through the SAME `message_end` chain a provider message gets:
            // native plugins observe it exactly once and a JS handler's
            // same-role replacement is applied to the terminal, the transcript,
            // the persisted bytes, and the `turn_end` payload (upstream's
            // single ordered handler chain). Event order is unchanged: the
            // `message_end` dispatch still runs between `stream_end` and
            // `turn_end`, exactly as on the provider path.
            if let Some(Message::Assistant(replaced)) = emit_message_end(&deps, "assistant", || {
                serde_json::json!({
                    "type": "message_end",
                    "message": value_payload(&stop),
                })
            })
            .await
            {
                stop = replaced;
            }
            Arc::make_mut(&mut context)
                .messages
                .push(Message::Assistant(stop.clone()));
            Arc::make_mut(&mut new_messages).push(Message::Assistant(stop.clone()));
            persist_message(&options, || Message::Assistant(stop.clone())).await;
            emit_lazy(&options.events, || AgentEvent::TurnEnd {
                turn: turns,
                message: stop.clone(),
                tool_results: Vec::new(),
            });
            terminal = stop;
            break;
        }
        // Upstream extension events index turns from 0 (`_turnIndex` starts at
        // 0 and increments after `turn_end`); the loop's `turns` is the 1-based
        // count the host events and the turn cap use. `turn_index` is the ONE
        // zero-based ordinal this iteration's `turn_start` and `turn_end` both
        // report, so the two events can never disagree.
        let turn_index = turns;
        turns += 1;
        turn_counter.store(turns, Ordering::SeqCst);
        // INVARIANT (turn slice): this turn's tool results are exactly the tail
        // `tool_results[turn_results_start..]` — both lists are appended in the
        // same order by the same code and `tool_results` is append-only for the
        // whole run. The former `turn_tool_results: Vec<ToolResultMessage>` was
        // a THIRD owner of every tool result (a deep clone of each result's
        // content per call, tool outputs included) built only to be re-cloned
        // for the `finish_turn` hook; the slice feeds that hook the identical
        // list.
        let turn_results_start = tool_results.len();
        // Per-turn span (upstream `startSpan("agent.turn")`), nesting under the
        // active `agent.run` span. The `Span` handle (not an `enter()` guard —
        // `EnteredSpan` is `!Send` and must not be held across await points) is
        // held for the whole loop iteration; the turn's heavyweight awaits
        // below — compaction, the provider stream, and the tool batch — wrap
        // their futures with `.instrument(turn_span.clone())` so child spans
        // and events record the turn.
        let turn_span = tracing::info_span!("agent.turn", turn = turns);
        // Compaction hook (upstream `prepareNextTurn`) runs BEFORE the
        // steering poll: preparation can be long-running, and messages queued
        // during compaction must be picked up before the next request, not
        // one turn late (#9548 timing).
        if let Some(compact) = options.compaction.as_ref() {
            // SHARED handoff (see [`CompactionFn`]): the hook is handed the
            // run's own handle — the former shape deep-cloned the whole
            // context into `before` on EVERY turn just to detect a compaction
            // and cloned again at the provider boundary. A hook that does not
            // compact returns the SAME `Arc` (identity preserved), so the
            // "did it compact?" check is one pointer comparison.
            let before = Arc::clone(&context);
            let after = compact(Arc::clone(&context))
                .instrument(turn_span.clone())
                .await;
            let compacted = !Arc::ptr_eq(&before, &after);
            context = after;
            // `compaction` fires only when the hook actually compacted
            // (threshold unmet, cancelled, and failed compactions all return
            // the original context unchanged).
            if compacted {
                emit(&options.events, AgentEvent::Compaction { turn: turns });
            }
        }
        // prepare_next_turn (upstream `prepareNextTurn`, run by the session
        // AFTER its compaction step): replacement context/model/thinking
        // state and messages to append before this turn's request. Only runs
        // when a previous turn completed; on the first iteration there is no
        // completed turn to hand over.
        if let (Some(prepare), Some(completed)) = (
            options.prepare_next_turn.as_ref(),
            last_completed_turn.as_ref(),
        ) {
            // Upstream runs compaction on the turn's context BEFORE the
            // generic hook and hands it the LIVE context plus the LIVE
            // produced-message list. Attach the loop's current handles here,
            // at the call: the snapshot is dropped when the hook returns
            // (unless the hook itself retains it), so the loop never holds a
            // second reference that would force the next turn's appends to
            // deep-copy the transcript. A retained snapshot still sees the
            // turn-boundary state it was handed: the loop's later appends
            // clone-on-write.
            let snapshot = prepare_next_turn_snapshot(completed, &context, &new_messages);
            if let Some(update) = prepare(snapshot).instrument(turn_span.clone()).await {
                if let Some(replaced) = update.context {
                    context = replaced;
                }
                if let Some(model) = update.model {
                    current_model = Some(model);
                }
                if let Some(level) = update.thinking_level {
                    current_thinking = level;
                }
                inject_messages(
                    &deps,
                    turns,
                    &mut context,
                    update.messages,
                    &mut new_messages,
                    &options,
                )
                .await;
            }
        }
        // One poll per request: a delivery made after the previous turn is
        // carried in `steering_carried` and is not polled again (upstream
        // guards the second poll with `pendingMessages.length === 0`).
        if !steering_carried {
            inject_prompts(
                &deps,
                turns,
                &mut context,
                options.steering.as_ref(),
                &mut new_messages,
                &options,
            )
            .await;
        }
        steering_carried = false;
        if std::mem::take(&mut first_turn_start_emitted) {
            // The first turn's boundary was emitted with the starting
            // messages (upstream `agent_start` → `turn_start` →
            // `message_start`/`message_end`). Later turns emit it here, after
            // `prepare_next_turn` and the steering poll.
        } else {
            emit_lazy(&options.events, || AgentEvent::TurnStart {
                turn: turns,
                context: Arc::clone(&context),
            });
            emit_extension(
                &deps,
                "turn_start",
                serde_json::json!({
                    "type": "turn_start",
                    "turnIndex": turn_index,
                    "timestamp": crate::ai::util::now_ms(),
                }),
            )
            .await;
        }

        // `context` handlers see and may replace the message list right
        // before the model request; `context_with_system` handlers then see
        // the full transcript including the system prompt. Both helpers
        // trigger copy-on-write only when they actually install a
        // replacement, so a handler-free (or handler-no-op) turn keeps the
        // transcript allocation in place.
        apply_extension_context(&deps, &mut context).await;
        apply_extension_context_with_system(&deps, &mut context).await;

        // declareToolChanges (upstream agent-loop.ts:333-375): when the
        // executable/declared tool set for this request differs from the last
        // declared set, a system message carrying `toolsAdded`/`toolsRemoved`
        // is injected before the request so the model sees the loadout
        // delta. The declaration participates in the message lifecycle and is
        // persisted with the transcript (replay-safe: resumed sessions seed
        // `last_declared` from the transcript's declaration messages).
        // The diff itself is read-only; the push below is the only write.
        if let Some(mut declaration) = declare_tool_changes(&context, &mut last_declared) {
            // Host lifecycle pair for the declaration message: announced before
            // the extension dispatch and finalized (with any `message_end`
            // replacement applied) before it enters the transcript.
            emit_lazy(&options.events, || AgentEvent::MessageStart {
                turn: turns,
                message: declaration.clone(),
            });
            emit_extension_lazy(&deps, "message_start", || {
                serde_json::json!({
                    "type": "message_start",
                    "message": message_payload(&declaration),
                })
            })
            .await;
            // The declaration participates in the `message_end` return
            // contract like every other message (upstream: the replaced
            // message is what enters the context and is persisted), so a
            // handler replacement is applied before the push/persist below.
            let declaration_replacement = emit_message_end(&deps, declaration.role(), || {
                serde_json::json!({
                    "type": "message_end",
                    "message": message_payload(&declaration),
                })
            })
            .await;
            if let Some(replaced) = declaration_replacement {
                declaration = replaced;
            }
            emit_lazy(&options.events, || AgentEvent::MessageEnd {
                turn: turns,
                message: declaration.clone(),
            });
            Arc::make_mut(&mut context)
                .messages
                .push(declaration.clone());
            Arc::make_mut(&mut new_messages).push(declaration.clone());
            persist_message(&options, || declaration.clone()).await;
        }

        // Agent-level turn retry (upstream `AgentSession._prepareRetry` /
        // `continue`, agent-session.ts:1954-1957,3792-3827): a turn whose
        // stream ends with a retryable provider error is re-issued with
        // exponential backoff instead of ending the run. The failed attempt
        // never enters the context, the run's produced list, or the
        // persistence sink, so the retried request sees exactly the history
        // the original request saw (upstream `_omitRecoveryAttempt`; pir has
        // one transcript rather than a raw history plus a projection, so
        // simply not appending is the equivalent). Transport-level retries
        // (`StreamOptions::max_retries`) run INSIDE one stream call and keep
        // their own budget; this loop counts only re-issued turns.
        let message = 'request: loop {
            // `prepare_request` runs per provider request (upstream
            // `prepareRequest`), including a re-issued attempt.
            if let Some(hook) = deps.prepare_request.as_ref() {
                let update = hook(PrepareRequestContext {
                    context: Arc::clone(&context),
                    model: current_model.clone(),
                    thinking_level: current_thinking,
                })
                .await;
                if let Some(update) = update {
                    if let Some(replaced) = update.context {
                        context = replaced;
                    }
                    if let Some(model) = update.model {
                        current_model = Some(model);
                    }
                    if let Some(level) = update.thinking_level {
                        current_thinking = level;
                    }
                }
            }

            // `transform_context` (upstream `transformContext`) runs on the
            // transcript immediately before every provider request, including
            // the first and each retried attempt. The result is REQUEST-LOCAL:
            // `context` (the run transcript the session persists and replays)
            // is never replaced, exactly like upstream, where the transformed
            // message list is only passed to `convertToLlm`.
            let request_context = if let Some(transform) = options.transform_context.as_ref() {
                transform(Arc::clone(&context), signal.clone()).await
            } else {
                Arc::clone(&context)
            };

            let request = ProviderRequest {
                context: request_context,
                model: current_model.clone(),
                thinking_level: current_thinking,
            };
            // `durationMs` uses a monotonic clock around the whole response
            // (upstream records it on the final message of a response).
            let stream_started_at = std::time::Instant::now();
            // First provider request of the process: elapsed since the run
            // started. This is the phase `before_agent_start` / loadout /
            // bootstrap work shows up in.
            if let Some(run_started_at) = run_started_at {
                crate::config::timings::log_phase_once(
                    "provider.first_request_send",
                    "provider.first_request_send",
                    run_started_at.elapsed(),
                );
            }
            let mut message = if let Some(event_stream) = options.event_stream.as_ref() {
                let stream = event_stream(request).await;
                consume_stream(&deps, stream, turns, &options.events, run_started_at)
                    .instrument(turn_span.clone())
                    .await
            } else {
                // No stream lifecycle events: the assistant `message_start` still
                // fires at stream start, with the empty message (upstream emits
                // `message_start` with the first partial, agent-loop.ts:416-421).
                emit_lazy(&options.events, || AgentEvent::StreamStart {
                    turn: turns,
                    message: AssistantMessage::default(),
                });
                emit_extension(
                    &deps,
                    "message_start",
                    serde_json::json!({
                        "type": "message_start",
                        "message": message_payload(&Message::Assistant(AssistantMessage::default())),
                    }),
                )
                .await;
                (deps.stream)(request).instrument(turn_span.clone()).await
            };
            // Record the requested thinking level and monotonic duration onto
            // the finalized message (upstream `Object.assign(await response.result(),
            // { thinkingLevel })` plus `durationMs`).
            //
            // `durationMs` is idempotent here: the provider's
            // `AssistantMessageEventStream` already stamps the terminal
            // message it saw start (and the registry forwarding wrappers keep
            // that value), so a stream-backed turn keeps the exact value a
            // direct `ProviderRegistry::stream`/`complete` caller sees. Only a
            // callback that returns a bare message (no stream lifecycle, e.g.
            // an embedder-injected `StreamFn`) falls back to the loop's own
            // clock measurement.
            if message.duration_ms.is_none() {
                message.duration_ms = Some(
                    u64::try_from(stream_started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
            }
            message.thinking_level = Some(current_thinking);
            // Structural retry verdict for THIS attempt, decided by the
            // provider/transport finalizer while the failure was still a
            // typed `AiError` (see `ai::error::TurnRetryClass`). Take it now,
            // BEFORE any event, replacement, history, or persistence: the
            // carrier is an internal diagnostic and the message that leaves
            // this point is byte-identical to what the provider emitted.
            let turn_retry_class = crate::ai::error::take_turn_retry_class(&mut message);
            last_turn_retry_class = turn_retry_class;
            // Single post-finalization `StreamEnd` (the stream's own tail event
            // carries the pre-finalization message; only this one is emitted).
            emit_lazy(&options.events, || AgentEvent::StreamEnd {
                turn: turns,
                message: message.clone(),
            });
            // Assistant `message_end` (mergeable: handlers may replace the
            // finalized message, keeping the role). The chained replacement is
            // applied BACK onto `message` — upstream copies it onto the live
            // message object before the session persists it
            // (agent-session.ts:1318-1336) — which makes it, in this order:
            // (1) the value the native repair hook below sees (upstream runs every
            // `message_end` handler over the previous handler's output, and the
            // builtin stands in for the JS `pi-tool-repair` handler that the
            // loader keeps off the load list while the builtin is enabled);
            // (2) the terminal message, the transcript entry, and the persisted
            // bytes; (3) the source of the tool calls the loop extracts, so the
            // scheduler's `validate_arguments` preflight sees the replacement's
            // arguments.
            // (Bind the dispatch result BEFORE the assignment: the payload
            // closure borrows `message`, and an `if let` scrutinee's temporaries
            // outlive its body.)
            let message_end_replacement = emit_message_end(&deps, "assistant", || {
                serde_json::json!({
                    "type": "message_end",
                    "message": value_payload(&message),
                })
            })
            .await;
            if let Some(Message::Assistant(replaced)) = message_end_replacement {
                message = replaced;
            }
            // Native `message_end` repair (the `tool-repair` builtin, scope doc
            // §2): applied between the extension dispatch and the message's
            // terminal / context / persistence, so a committed repair precedes
            // both the transcript bytes and the `validate_arguments` preflight of
            // the extracted tool calls. It runs over whatever the extension
            // dispatch left in `message` (the handler chain's replacement when
            // there was one), matching upstream's single ordered handler chain
            // (`emitMessageEnd`); double-running the same repair pipeline is
            // impossible because the loader drops the JS `pi-tool-repair` entry
            // from the load list while `builtin:tool-repair` is on the effective
            // plan (`plugins::loader::tool_repair_precedence`). The hook owns the
            // repair core; `None` (no hook) leaves the message bit-identical.
            if let (Some(repair), Some(active_tools)) = (
                deps.message_end_repair.as_ref(),
                repair_active_tools.as_ref(),
            ) {
                if let Some(repaired) = repair(MessageEndRepairContext {
                    message: message.clone(),
                    model: current_model.clone(),
                    // Refcount bump, not a map rebuild: the lazy view shares
                    // its one materialization across the run's messages.
                    active_tools: active_tools.clone(),
                })
                .await
                {
                    message = repaired;
                }
            }
            // Every attempt's usage was spent, retried or not (upstream counts
            // the failed response's usage in the run totals).
            usage_total += message.usage.total_tokens;
            // Retry decision (upstream `_isRetryableError` + `_prepareRetry`):
            // an abort is terminal and never retried; context overflow is left
            // to the session's compact-and-retry; only an explicit
            // `RetryableTurn` transport verdict may be re-issued, and a
            // non-retryable/exhausted verdict keeps the historical hard exit.
            let aborted_now = signal.as_ref().is_some_and(AbortSignal::is_aborted);
            let delay = match deps.retry {
                Some(policy)
                    if !aborted_now
                        && retry_attempt < policy.max_retries
                        && retryable_turn_error(
                            &message,
                            current_model.as_ref(),
                            turn_retry_class,
                        ) =>
                {
                    retry_attempt += 1;
                    Some(crate::ai::util::retry_delay_ms(
                        policy.base_delay_ms,
                        u64::from(retry_attempt),
                        Some(policy.max_agent_delay_ms),
                    ))
                }
                _ => None,
            };
            let Some(delay_ms) = delay else {
                // This attempt is terminal for the run: close an OPEN retry
                // lifecycle with exactly one `auto_retry_end` (upstream
                // `_handlePostAgentRun`: success for a recovered attempt, the
                // final error when the budget/verdict refused another retry,
                // and "Retry cancelled" for an aborted run).
                if retry_attempt > 0 {
                    let cancelled =
                        aborted_now || matches!(message.stop_reason, StopReason::Aborted);
                    let (success, final_error) = if cancelled {
                        (false, Some("Retry cancelled".to_owned()))
                    } else if matches!(message.stop_reason, StopReason::Error) {
                        // Upstream keeps `finalError` optional; an error with
                        // no message omits the field.
                        (false, message.error_message.clone())
                    } else {
                        (true, None)
                    };
                    if let Some(sink) = deps.retry_events.as_ref() {
                        sink(AgentRetryEvent::End {
                            success,
                            attempt: retry_attempt,
                            final_error,
                        });
                    }
                    // A recovered response closes this retry lifecycle AND
                    // restores the per-response budget for the rest of the
                    // run (upstream resets `_retryAttempt` on success). The
                    // counter used to stay at its last value, so every later
                    // successful turn re-reported `auto_retry_end` and no
                    // later turn could ever retry.
                    if success {
                        retry_attempt = 0;
                    }
                }
                break 'request message;
            };
            if let Some(sink) = deps.retry_events.as_ref() {
                sink(AgentRetryEvent::Start {
                    attempt: retry_attempt,
                    max_attempts: deps
                        .retry
                        .as_ref()
                        .map(|policy| policy.max_retries)
                        .unwrap_or(retry_attempt),
                    delay_ms,
                    error_message: message
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Unknown error".to_owned()),
                });
            }
            tracing::warn!(
                target: "pir::agent",
                turn = turns,
                attempt = retry_attempt,
                delay_ms,
                error = message.error_message.as_deref().unwrap_or("unknown error"),
                "agent loop: retrying the turn after a retryable provider error"
            );
            // Abortable backoff: a cancel during the wait ends the run like a
            // provider abort (upstream normalizes the in-flight response).
            if let Some(signal) = signal.as_ref() {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                    _ = signal.cancelled() => {
                        // Upstream `_prepareRetry` catch → `_finishCancelledRetry`:
                        // the UI is told the retry is over before the run ends.
                        if let Some(sink) = deps.retry_events.as_ref() {
                            sink(AgentRetryEvent::End {
                                success: false,
                                attempt: retry_attempt,
                                final_error: Some("Retry cancelled".to_owned()),
                            });
                        }
                        message.stop_reason = StopReason::Aborted;
                        message.error_message = None;
                        break 'request message;
                    }
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            // Re-issue the SAME turn: the failed attempt is not in `context`,
            // `new_messages`, or storage, so the retried request's history is
            // the history the failed request was given.
            continue 'request;
        };
        terminal = message.clone();
        // The turn's tool calls are final here (after the extension
        // `message_end` and the native repair hook); extracting them BEFORE
        // the assistant write lets the batch barrier below open first, so the
        // assistant message that carries a tool batch and its results are
        // never separated by a background completion's custom entry.
        let calls = tool_calls_from_message(&message);
        // B1 (batch boundary): hold the commit tracker's batch barrier from
        // this assistant-message write through the batch's last tool-result
        // write, so a detached (background) completion can never persist its
        // `backgroundToolResult` custom entry between a tool call and its
        // result. The guard is dropped at the end of this loop iteration
        // (before the next batch starts), which is exactly the boundary.
        let _batch_barrier = match deps.commit_tracker.as_ref() {
            Some(tracker) if !calls.is_empty() => Some(tracker.begin_batch().await),
            _ => None,
        };
        Arc::make_mut(&mut context)
            .messages
            .push(Message::Assistant(message.clone()));
        Arc::make_mut(&mut new_messages).push(Message::Assistant(message.clone()));
        persist_message(&options, || Message::Assistant(message.clone())).await;
        // (`usage_total` already counted this turn's attempts above, including
        // any retried one.)

        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            // Error/aborted turns run `finish_turn` too (upstream), but the
            // decision is ignored: they remain hard exits.
            let _ = call_finish_turn(
                deps.finish_turn.as_ref(),
                turn_index,
                &message,
                &[],
                &context,
                &new_messages,
            )
            .await;
            emit_lazy(&options.events, || AgentEvent::TurnEnd {
                turn: turns,
                message: message.clone(),
                tool_results: Vec::new(),
            });
            // No loop-level `turn_end` extension dispatch: the settle boundary
            // (settle.rs `turn_end_boundary_hook`) is the single dispatch site.
            break;
        }

        // A turn without tool calls breaks the "consecutive identical
        // executions" relation: the guard must not count calls across an
        // intervening text-only/steering/follow-up turn (a false "N in a row"
        // would end a legitimate run).
        if calls.is_empty() {
            repeat_digest = None;
            repeat_streak = 0;
            repeat_warned_digest = None;
        }
        if message.stop_reason == StopReason::Length {
            // A "length" stop means the output was cut off by the token
            // limit, so every tool call in the message may carry truncated
            // arguments. Fail them all instead of executing potentially
            // borked calls (upstream `failToolCallsFromTruncatedMessage`,
            // agent-loop.ts:478-503), with the same per-call lifecycle
            // events a real execution emits.
            for call in &calls {
                emit_lazy(&options.events, || AgentEvent::ToolExecutionStart {
                    turn: turns,
                    event: crate::agent::tool_batch::ToolExecutionEvent::Start {
                        index: call.index,
                        call_id: call.call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        parent_call_id: None,
                    },
                    args: call.arguments.clone(),
                });
                emit_extension_lazy(&deps, "tool_execution_start", || {
                    serde_json::json!({
                        "type": "tool_execution_start",
                        "toolCallId": call.call_id,
                        "toolName": call.tool_name,
                        "args": call.arguments,
                    })
                })
                .await;
                let finalized = error_results_for_length(std::slice::from_ref(call));
                for finalized in finalized {
                    let is_error = finalized.is_error();
                    let duration_ms = finalized.duration_ms;
                    let tool_name = finalized.tool_name.clone();
                    let call_id = finalized.call_id.clone();
                    let mut result = to_tool_result_message(&finalized, &message, None, None);
                    emit_lazy(&options.events, || AgentEvent::ToolExecutionEnd {
                        turn: turns,
                        event: crate::agent::tool_batch::ToolExecutionEvent::End {
                            index: finalized.index,
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            is_error,
                            duration_ms,
                            parent_call_id: None,
                        },
                        result: crate::agent::tool_execution::finalized_tool_result_event_json(
                            &finalized,
                        ),
                    });
                    emit_extension_lazy(&deps, "tool_execution_end", || {
                        serde_json::json!({
                            "type": "tool_execution_end",
                            "toolCallId": call_id,
                            "toolName": tool_name,
                            "result": crate::agent::tool_execution::finalized_tool_result_event_json(
                                &finalized,
                            ),
                            "isError": is_error,
                        })
                    })
                    .await;
                    // Tool-result messages participate in the message
                    // lifecycle in UPSTREAM order (agent-loop.ts:946-951
                    // `emitToolResultMessage`: `message_start`, then
                    // `message_end`, and only then does the result enter the
                    // context — the session persists it while the `message_end`
                    // listener is awaited), so a `message_end` replacement is
                    // what the context, the tool-result list, the run's
                    // new-message list and the persistence sink record.
                    emit_lazy(&options.events, || AgentEvent::MessageStart {
                        turn: turns,
                        message: Message::ToolResult(result.clone()),
                    });
                    emit_extension_lazy(&deps, "message_start", || {
                        serde_json::json!({
                            "type": "message_start",
                            "message": value_payload(&result),
                        })
                    })
                    .await;
                    let result_replacement = emit_message_end(&deps, "toolResult", || {
                        serde_json::json!({
                            "type": "message_end",
                            "message": value_payload(&result),
                        })
                    })
                    .await;
                    if let Some(Message::ToolResult(replaced)) = result_replacement {
                        result = replaced;
                    }
                    emit_lazy(&options.events, || AgentEvent::MessageEnd {
                        turn: turns,
                        message: Message::ToolResult(result.clone()),
                    });
                    Arc::make_mut(&mut context)
                        .messages
                        .push(Message::ToolResult(result.clone()));
                    tool_results.push(result.clone());
                    Arc::make_mut(&mut new_messages).push(Message::ToolResult(result.clone()));
                    persist_message(&options, || Message::ToolResult(result.clone())).await;
                }
            }
            let decision = call_finish_turn(
                deps.finish_turn.as_ref(),
                turn_index,
                &message,
                &tool_results[turn_results_start..],
                &context,
                &new_messages,
            )
            .await;
            emit_lazy(&options.events, || AgentEvent::TurnEnd {
                turn: turns,
                message: message.clone(),
                tool_results: tool_results[turn_results_start..].to_vec(),
            });
            // No loop-level `turn_end` extension dispatch: the settle boundary
            // (settle.rs `turn_end_boundary_hook`) is the single dispatch site.
            record_completed_turn(
                options.prepare_next_turn.as_ref(),
                &mut last_completed_turn,
                turn_index,
                &message,
                &tool_results[turn_results_start..],
            );
            if decision == Some(FinishTurnAction::End) {
                break;
            }
            // Upstream only reaches `hasMoreToolCalls = !executedToolBatch
            // .terminate` inside the `toolCalls.length > 0` branch
            // (agent-loop.ts:263-272). A length stop with tool calls therefore
            // continues (the failed results are in the transcript, so the
            // model can re-issue the calls); a length stop with NO tool calls
            // leaves `hasMoreToolCalls` false and ends the turn — re-issuing
            // the request would hit the same output cap forever and the
            // truncated message is the run's final response (print mode's
            // `length` exit-1 path depends on the loop returning).
            if calls.is_empty() {
                if inject_prompts(
                    &deps,
                    turns,
                    &mut context,
                    options.steering.as_ref(),
                    &mut new_messages,
                    &options,
                )
                .await
                {
                    steering_carried = true;
                    continue;
                }
                if inject_prompts(
                    &deps,
                    turns,
                    &mut context,
                    deps.follow_up.as_ref(),
                    &mut new_messages,
                    &options,
                )
                .await
                {
                    steering_carried = true;
                    continue;
                }
                if inject_prompts(
                    &deps,
                    turns,
                    &mut context,
                    deps.background_follow_up.as_ref(),
                    &mut new_messages,
                    &options,
                )
                .await
                {
                    steering_carried = true;
                    continue;
                }
                if decision == Some(FinishTurnAction::Continue) {
                    continue;
                }
                break 'agent;
            }
            // Steering queued during the turn is picked up by the top-of-turn
            // poll.
            continue;
        } else if !calls.is_empty() {
            let tool_context = ToolContext {
                cwd: deps.cwd.clone(),
                signal: signal.clone(),
                nested: Some(Arc::new(NestedToolExecutor::root(Arc::clone(&nested)))),
                session_env: deps.session_env.clone(),
                held_mutation_keys: None,
                // Live partial-result channel (upstream tool `onUpdate`): the
                // shell tools stream throttled snapshots through it. The
                // run-level channel (print and other session-less hosts) wins
                // over the session environment's.
                //
                // The host `tool_execution_update` event is emitted here, at
                // the loop's single update seam (upstream
                // `emitToolExecutionUpdate`, agent-loop.ts:779-788), and the
                // underlying channel still receives the payload unchanged.
                // The scheduler stamps `toolCallId`/`toolName`/`args` onto the
                // partial snapshot; a nested id (`<parent>/<n>`) yields
                // `parentToolCallId`. The wrapper is installed only when a
                // host event stream or an underlying sink observes updates.
                on_update: {
                    let base = deps.tool_updates.clone().or_else(|| {
                        deps.session_env
                            .as_ref()
                            .and_then(|env| env.on_update.clone())
                    });
                    let events = options.events.clone();
                    // No observer at all: keep the historical `None` so tools
                    // skip partial-result construction entirely (bare
                    // embedders).
                    if events.is_none() && base.is_none() {
                        None
                    } else {
                        let turn = turns;
                        Some(Arc::new(move |payload: serde_json::Value| {
                            // Only unpack the payload when a host event handler
                            // will read the structured event. With just the
                            // session/plugin update sink installed (`base`),
                            // `emit_lazy` is a no-op, and the unpack cloned
                            // the partial result and arguments on every
                            // throttled shell snapshot for nobody.
                            if events.is_some() {
                                // With a second consumer (`base`) the original
                                // payload is still needed; without one it can
                                // be consumed so the snapshot moves into the
                                // event instead of being deep-copied.
                                if let Some(base) = base.as_ref() {
                                    let parts =
                                        crate::agent::tool_execution::tool_update_parts(&payload);
                                    emit_lazy(&events, || AgentEvent::ToolExecutionUpdate {
                                        turn,
                                        call_id: parts.call_id,
                                        tool_name: parts.tool_name,
                                        parent_call_id: parts.parent_call_id,
                                        args: parts.args,
                                        partial: parts.partial,
                                    });
                                    base(payload);
                                } else {
                                    let parts =
                                        crate::agent::tool_execution::tool_update_parts_owned(
                                            payload,
                                        );
                                    emit_lazy(&events, || AgentEvent::ToolExecutionUpdate {
                                        turn,
                                        call_id: parts.call_id,
                                        tool_name: parts.tool_name,
                                        parent_call_id: parts.parent_call_id,
                                        args: parts.args,
                                        partial: parts.partial,
                                    });
                                }
                            } else if let Some(base) = base.as_ref() {
                                base(payload);
                            }
                        }))
                    }
                },
                background: false,
            };
            // Extension `tool_call` is dispatched by the scheduler itself,
            // after `prepareArguments`/schema validation and before execution
            // (`tool_scheduler/prepare.rs`), so a handler never sees an
            // invalid-argument call and a rewritten input is not re-validated.
            //
            // MCP reach readiness (upstream the `mcp` extension's `tool_call`
            // handler): a call that reaches MCP tools waits for the servers it
            // may reach, so their tools are registered before it runs. The
            // wait runs on the model's calls, before the scheduler's gate; an
            // extension rewriting the input cannot change which servers the
            // tool itself declares a reach for.
            if let Some(wait_for_reach) = deps.tool_call_wait.clone() {
                wait_for_reach(Arc::clone(&deps.tools), calls.clone()).await;
            }
            // Stream the batch's lifecycle events as they are produced: the
            // scheduler reports every preflight start (before any call runs)
            // and every end at that call's completion. A bounded channel feeds
            // a consumer driven concurrently with the batch below, so the TUI
            // renders each tool result as soon as its call finishes instead of
            // waiting for the slowest sibling. The capacity covers every start
            // and end this batch can produce (2 per call), so a `try_send`
            // can never drop an event even if the consumer is busy.
            let progress_capacity = calls.len().saturating_mul(2).saturating_add(16).max(64);
            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::channel::<ToolProgressItem>(progress_capacity);
            // The observer handles are captured so the sink can decide, per
            // event, whether an End frame's full result will be read at all.
            // `emit_tool_progress` returns before touching the frame when no
            // face observes `tool_execution_end`; building (and cloning) the
            // result JSON for that frame was pure per-call cost, and a tool
            // result can be large (spilled output, image blocks).
            let progress_events = options.events.clone();
            let progress_extensions = deps.extensions.clone();
            let progress_plugins = deps.plugin_events.clone();
            let progress_sink: crate::agent::tool_scheduler::ToolExecutionProgress = {
                let progress_tx = progress_tx.clone();
                Arc::new(move |event, finalized| {
                    let wants_result = progress_events.is_some()
                        || progress_extensions
                            .as_ref()
                            .is_some_and(|runtime| runtime.has_handlers("tool_execution_end"))
                        || progress_plugins
                            .as_ref()
                            .is_some_and(|host| host.has_native_handlers("tool_execution_end"));
                    let result = if wants_result {
                        finalized
                            .map(crate::agent::tool_execution::finalized_tool_result_event_json)
                    } else {
                        None
                    };
                    if progress_tx
                        .try_send(ToolProgressItem { event, result })
                        .is_err()
                    {
                        // Unreachable by construction (see the capacity above);
                        // never panic on a tool execution path.
                        tracing::error!(
                            target: "pir::agent",
                            "tool progress channel overflow: lifecycle event dropped"
                        );
                    }
                })
            };
            // The sink (inside the batch future) holds the only sender, so the
            // channel ends exactly when the batch does.
            drop(progress_tx);
            // W16-D11: a model-issued call resolves against the EXECUTABLE set
            // — upstream `agent.state.tools = declared` (agent-session.ts:1623)
            // is what the loop looks a call up in (agent-loop.ts:713-723). The
            // registry is the full registered set (the F12 rule keeps
            // `codemode`/`tool_search`/nested calls able to reach every tool), so
            // the batch needs its own boundary: this request's declared
            // definitions (`context.tools`) plus the run's active `direct` tools.
            // The second half is exactly the `direct` names inside
            // `LoopDeps::nested_callable`; a `codemode`/`deferred` tool is NOT
            // top-level executable unless it is declared (an explicit
            // activation puts it in `context.tools`) — the full callable set
            // stays with `ctx.executeTool`/nested calls (`NestedShared`), which
            // this batch gate must not widen. Hook-hidden declarations stay
            // active `direct` tools, so they remain executable (upstream
            // `agent.state.tools`). A registered tool in neither set fails
            // exactly like an absent one. `nested_callable: None` keeps the
            // historical registry-wide behavior for hosts that declare no
            // callable set.
            let batch_callable = deps.nested_callable.as_ref().map(|callable| {
                let mut names: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                if let Some(declared) = context.tools.as_ref() {
                    names.extend(declared.iter().map(|tool| tool.name.clone()));
                }
                names.extend(
                    callable
                        .iter()
                        .filter(|name| {
                            deps.tools
                                .get(name)
                                .is_some_and(|tool| tool.exposure() == ToolExposure::Direct)
                        })
                        .cloned(),
                );
                Arc::new(names)
            });
            let mut batch_scheduler = deps.scheduler.clone().with_progress(progress_sink);
            if let Some(callable) = batch_callable {
                batch_scheduler = batch_scheduler.with_callable(callable);
            }
            let batch_tools = Arc::clone(&deps.tools);
            let batch_hooks = hooks.clone();
            let batch_span = turn_span.clone();
            let run = async move {
                batch_scheduler
                    .execute_batch_with_owned_hooks(calls, &batch_tools, &tool_context, batch_hooks)
                    .instrument(batch_span)
                    .await
            };
            tokio::pin!(run);
            // `tool_execution_start.args` resolves from the assistant
            // message's own call payload (upstream's start frame carries the
            // model's arguments; a `tool_call` handler rewrite happens later,
            // inside the scheduler's preflight).
            let inputs = std::collections::HashMap::new();
            let outcome: ToolBatchOutcome = loop {
                tokio::select! {
                    // Progress first: the scheduler yields after preflight, so
                    // this drains every start (and any end already produced)
                    // before the batch future is polled again — observers see
                    // starts before the calls execute, and ends as they land.
                    biased;
                    item = progress_rx.recv() => {
                        match item {
                            Some(item) => {
                                emit_tool_progress(
                                    &options.events,
                                    &deps,
                                    turns,
                                    &inputs,
                                    &message,
                                    item,
                                )
                                .await;
                            }
                            None => {
                                // The batch future owns the only sender, so a
                                // closed channel means the batch is finished
                                // (or dropped) — never a reason to panic on a
                                // production tool path. Drive the pinned
                                // future to its output; when the sender closed
                                // early this is the exact outcome the batch
                                // produced, otherwise it waits for it like the
                                // other select arm would.
                                let outcome = (&mut run).await;
                                break outcome;
                            }
                        }
                    }
                    outcome = &mut run => break outcome,
                }
            };
            // Drain whatever was produced after the last consumer poll so no
            // event (and no end) is lost before the batch result is used.
            progress_rx.close();
            while let Some(item) = progress_rx.recv().await {
                emit_tool_progress(&options.events, &deps, turns, &inputs, &message, item).await;
            }
            // Per-call `tool_result` dispatch already happened inside the
            // scheduler (after each tool and its `after` hook, before that
            // call's `tool_execution_end`), and its merge is applied to
            // `outcome.results` there. Gate-blocked calls re-join the batch as
            // synthesized error results through the same path, so the
            // transcript stays 1:1 with the model's calls.
            // Bounded `nestedCalls` records: each executed call's nested
            // calls (including descendants) attach to its tool result,
            // together with their combined usage.
            let nested_records: std::collections::HashMap<
                String,
                crate::agent::nested_tool_calls::NestedCallSummary,
            > = outcome
                .results
                .iter()
                .filter_map(|finalized| {
                    nested
                        .take_record(&finalized.call_id)
                        .map(|record| (finalized.call_id.clone(), record))
                })
                .collect();
            // The final model-visible result of a single-call batch (after a
            // `message_end` replacement), for the degenerate-loop guard.
            let mut repeat_result: Option<(bool, String)> = None;
            for finalized in &outcome.results {
                let nested_summary = nested_records.get(&finalized.call_id).cloned();
                let nested_calls = nested_summary
                    .as_ref()
                    .and_then(|summary| summary.calls.clone());
                // Usage on the tool-result message: the `tool_result`
                // handler-reported usage combined with the nested calls'
                // recorded usage (upstream agent-session.ts:1110-1112; nested
                // results are not persisted, so their usage is only counted
                // through the recorder). The scheduler's per-call finalize
                // already replaced the `after` hook's usage with a
                // `tool_result` handler report when one was returned, so
                // `finalized.usage` is the correct base value here.
                let base_usage = finalized.usage;
                let usage = match nested_summary.and_then(|summary| summary.usage) {
                    Some(nested) => Some(match base_usage {
                        Some(base) => crate::agent::nested_tool_calls::combine_usage(base, nested),
                        None => nested,
                    }),
                    None => base_usage,
                };
                let mut result = to_tool_result_message(finalized, &message, nested_calls, usage);
                // Tool-result messages participate in the message lifecycle in
                // UPSTREAM order (agent-loop.ts:946-951 `emitToolResultMessage`:
                // `message_start`, then `message_end`, and only then does the
                // result enter the context — the session persists it while the
                // `message_end` listener is awaited), so a `message_end`
                // replacement is what the context, the tool-result list, the
                // run's new-message list and the persistence sink record.
                emit_lazy(&options.events, || AgentEvent::MessageStart {
                    turn: turns,
                    message: Message::ToolResult(result.clone()),
                });
                emit_extension_lazy(&deps, "message_start", || {
                    serde_json::json!({
                        "type": "message_start",
                        "message": value_payload(&result),
                    })
                })
                .await;
                let result_replacement = emit_message_end(&deps, "toolResult", || {
                    serde_json::json!({
                        "type": "message_end",
                        "message": value_payload(&result),
                    })
                })
                .await;
                if let Some(Message::ToolResult(replaced)) = result_replacement {
                    result = replaced;
                }
                emit_lazy(&options.events, || AgentEvent::MessageEnd {
                    turn: turns,
                    message: Message::ToolResult(result.clone()),
                });
                if outcome.results.len() == 1 {
                    repeat_result =
                        Some((result.is_error, tool_result_model_text(&result.content)));
                }
                Arc::make_mut(&mut context)
                    .messages
                    .push(Message::ToolResult(result.clone()));
                tool_results.push(result.clone());
                Arc::make_mut(&mut new_messages).push(Message::ToolResult(result.clone()));
                persist_message(&options, || Message::ToolResult(result.clone())).await;
                // A1: the tool result is now in storage. Release any
                // background completion waiting to append a
                // `backgroundToolResult` custom entry after it. Called for
                // EVERY result, not only detached ones: a nested
                // `ctx.executeTool(..., { background: true })` completion
                // waits on its enclosing model-issued call's id, which is
                // this result. No-op when no gate is registered. The batch
                // barrier above still holds the persist itself until the
                // whole batch is written (B1).
                if let Some(tracker) = deps.commit_tracker.as_ref() {
                    tracker.commit_key(&finalized.call_id);
                }
            }
            // Degenerate-loop guard (pir-native; upstream has none). A single
            // tool call repeated with the SAME arguments and the SAME
            // model-visible result is making no progress. A measured leg
            // repeated one bash call 489 times (511 turns, 17.4M tokens) until
            // its harness wall; intervening at twenty removes ~96% of that
            // cost while a changed result — the normal polling case — resets
            // the streak.
            //
            // Nudge-then-stop: a first trip may be legitimate polling whose
            // result happens to be stable, so it injects a steering message
            // telling the model to change approach and restarts the streak for
            // the SAME digest; a SECOND trip of a streak the model was already
            // warned about (and ignored) stops the run.
            match single_call_digest(&message, repeat_result.as_ref()) {
                Some(digest) if repeat_digest == Some(digest) => repeat_streak += 1,
                Some(digest) => {
                    repeat_digest = Some(digest);
                    repeat_streak = 1;
                    repeat_warned_digest = None;
                }
                None => {
                    repeat_digest = None;
                    repeat_streak = 0;
                    repeat_warned_digest = None;
                }
            }
            if repeat_streak >= REPEAT_STREAK_STOP
                && !signal.as_ref().is_some_and(AbortSignal::is_aborted)
            {
                let warned = repeat_digest.is_some() && repeat_warned_digest == repeat_digest;
                if !warned && repeat_nudges < REPEAT_NUDGE_LIMIT {
                    repeat_warned_digest = repeat_digest;
                    repeat_nudges += 1;
                    let nudge_text = repeat_nudge_text(repeat_streak);
                    tracing::warn!(
                        target: "pir::agent",
                        streak = repeat_streak,
                        "agent loop: nudging a degenerate run (identical tool calls with unchanged results)"
                    );
                    // Delivered exactly like a steering message: it is a real
                    // transcript/user message, persisted at its append, and the
                    // next request carries it.
                    let nudge = Message::User(UserMessage {
                        role: UserRole::User,
                        content: vec![ContentBlock::Text(TextContent {
                            text: nudge_text,
                            text_signature: None,
                        })],
                        tools_added: None,
                        tools_removed: None,
                        sections: None,
                        timestamp: crate::ai::util::now_ms(),
                    });
                    Arc::make_mut(&mut context).messages.push(nudge.clone());
                    Arc::make_mut(&mut new_messages).push(nudge.clone());
                    persist_message(&options, || nudge.clone()).await;
                    // The warning trip does not count toward the next one: the
                    // model gets a full streak to change course. Fall through
                    // to the normal `finish_turn`/`turn_end` sequence so the
                    // turn still settles like any other.
                    repeat_streak = 0;
                } else {
                    tracing::warn!(
                        target: "pir::agent",
                        streak = repeat_streak,
                        "agent loop: stopping a degenerate run (identical tool calls with unchanged results)"
                    );
                    let error = format!(
                        "Stopped after {repeat_streak} identical tool calls in a row (same name, arguments and result): the run is making no progress. Change the approach or finish the task."
                    );
                    let mut stop = AssistantMessage {
                        api: message.api.clone(),
                        provider: message.provider.clone(),
                        model: message.model.clone(),
                        stop_reason: StopReason::Error,
                        error_message: Some(error),
                        timestamp: crate::ai::util::now_ms(),
                        ..AssistantMessage::default()
                    };
                    emit_lazy(&options.events, || AgentEvent::StreamStart {
                        turn: turns,
                        message: stop.clone(),
                    });
                    emit_lazy(&options.events, || AgentEvent::StreamEnd {
                        turn: turns,
                        message: stop.clone(),
                    });
                    // The synthetic assistant message is a real transcript
                    // message, so it goes through the SAME `message_end` chain
                    // a provider message gets (native fan-out + the JS
                    // handler chain, whose same-role replacement is applied).
                    // Event order is unchanged: `message_end` still runs
                    // between `stream_end` and `turn_end`.
                    if let Some(Message::Assistant(replaced)) =
                        emit_message_end(&deps, "assistant", || {
                            serde_json::json!({
                                "type": "message_end",
                                "message": value_payload(&stop),
                            })
                        })
                        .await
                    {
                        stop = replaced;
                    }
                    Arc::make_mut(&mut context)
                        .messages
                        .push(Message::Assistant(stop.clone()));
                    Arc::make_mut(&mut new_messages).push(Message::Assistant(stop.clone()));
                    persist_message(&options, || Message::Assistant(stop.clone())).await;
                    emit_lazy(&options.events, || AgentEvent::TurnEnd {
                        turn: turns,
                        message: stop.clone(),
                        tool_results: Vec::new(),
                    });
                    terminal = stop;
                    break;
                }
            }
            // `finish_turn` runs BEFORE `turn_end` (upstream ordering).
            let decision = call_finish_turn(
                deps.finish_turn.as_ref(),
                turn_index,
                &message,
                &tool_results[turn_results_start..],
                &context,
                &new_messages,
            )
            .await;
            emit_lazy(&options.events, || AgentEvent::TurnEnd {
                turn: turns,
                message: message.clone(),
                tool_results: tool_results[turn_results_start..].to_vec(),
            });
            // NOTE: `turn_end` extension dispatch is NOT emitted here: the
            // session-level boundary hook (agent_session/settle.rs
            // `turn_end_boundary_hook`, wrapped into `deps.finish_turn`) is the
            // SINGLE dispatch site (upstream runner.ts emitBoundary), carrying
            // entries/continue/context/outcome. The loop-level emission was
            // removed because it fired alongside the boundary and double-
            // dispatched to the same JS handlers. The host `AgentEvent::TurnEnd`
            // event above is unaffected.
            record_completed_turn(
                options.prepare_next_turn.as_ref(),
                &mut last_completed_turn,
                turn_index,
                &message,
                &tool_results[turn_results_start..],
            );
            if decision == Some(FinishTurnAction::End) {
                break;
            }
            if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
                break;
            }
            // A terminating batch only ends the TOOL phase (upstream:
            // hasMoreToolCalls=false) ???steering and follow-up still get
            // their poll, so a queued redirect is not dropped by a
            // terminate (agent-loop.ts:241,263,267).
            if outcome.should_terminate() {
                if inject_prompts(
                    &deps,
                    turns,
                    &mut context,
                    options.steering.as_ref(),
                    &mut new_messages,
                    &options,
                )
                .await
                {
                    steering_carried = true;
                    continue;
                }
                // `hasMoreToolCalls` is false for a terminating batch, so the
                // ordinary user follow-ups get their poll here too (upstream
                // only drains them once the inner tool loop stops). Without
                // this poll a follow-up queued during the terminating batch
                // waited for the NEXT prompt even though the run had stopped
                // calling tools — the redirect was silently delayed.
                if inject_prompts(
                    &deps,
                    turns,
                    &mut context,
                    deps.follow_up.as_ref(),
                    &mut new_messages,
                    &options,
                )
                .await
                {
                    steering_carried = true;
                    continue;
                }
                if inject_prompts(
                    &deps,
                    turns,
                    &mut context,
                    deps.background_follow_up.as_ref(),
                    &mut new_messages,
                    &options,
                )
                .await
                {
                    // Follow-ups are carried into the next request like
                    // steering (upstream keeps both as `pendingMessages`), so
                    // the next turn must not poll a second message.
                    steering_carried = true;
                    continue;
                }
                if decision == Some(FinishTurnAction::Continue) {
                    // The continuation decision guarantees exactly one more
                    // provider request; with nothing scheduled, continue with
                    // the current context.
                    continue;
                }
                break;
            }
            // Steering is deliberately checked between tool turns, allowing a
            // user message to redirect the next model request.
            if inject_prompts(
                &deps,
                turns,
                &mut context,
                options.steering.as_ref(),
                &mut new_messages,
                &options,
            )
            .await
            {
                steering_carried = true;
            }
            // A6: detached background completions live on their OWN inbox.
            // The terminal polls below never run for a model that keeps
            // calling tools, so poll that inbox at every batch boundary too:
            // the frame then reaches the next request as a user message after
            // this batch's persisted tool results. User follow-ups are NOT
            // polled here (upstream only reads them once the inner tool loop
            // stops).
            if inject_prompts(
                &deps,
                turns,
                &mut context,
                deps.background_follow_up.as_ref(),
                &mut new_messages,
                &options,
            )
            .await
            {
                steering_carried = true;
            }
            continue;
        } else {
            let decision = call_finish_turn(
                deps.finish_turn.as_ref(),
                turn_index,
                &message,
                &[],
                &context,
                &new_messages,
            )
            .await;
            emit_lazy(&options.events, || AgentEvent::TurnEnd {
                turn: turns,
                message: message.clone(),
                tool_results: Vec::new(),
            });
            // No loop-level `turn_end` extension dispatch: the settle boundary
            // (settle.rs `turn_end_boundary_hook`) is the single dispatch site.
            record_completed_turn(
                options.prepare_next_turn.as_ref(),
                &mut last_completed_turn,
                turn_index,
                &message,
                &[],
            );
            if decision == Some(FinishTurnAction::End) {
                break;
            }
            if inject_prompts(
                &deps,
                turns,
                &mut context,
                options.steering.as_ref(),
                &mut new_messages,
                &options,
            )
            .await
            {
                steering_carried = true;
                continue;
            }
            if inject_prompts(
                &deps,
                turns,
                &mut context,
                deps.follow_up.as_ref(),
                &mut new_messages,
                &options,
            )
            .await
            {
                steering_carried = true;
                continue;
            }
            if inject_prompts(
                &deps,
                turns,
                &mut context,
                deps.background_follow_up.as_ref(),
                &mut new_messages,
                &options,
            )
            .await
            {
                steering_carried = true;
                continue;
            }
            if decision == Some(FinishTurnAction::Continue) {
                // No natural request was selected, so fulfill the continuation
                // decision with one context-only turn (upstream agent-loop.ts
                // ~311-314).
                continue;
            }
            break 'agent;
        }
    }

    let aborted = signal.as_ref().is_some_and(AbortSignal::is_aborted);
    // Run summary (upstream `TelemetryContext.setStatus` + terminal `addEvent`
    // on the `agent.run` span): one line per run with the turn count, abort
    // flag, and terminal stop reason. `debug!` by default: upstream routes
    // this to telemetry, not stderr, so surfacing it at INFO made every
    // print-mode run noisier than real pi (`--verbose`/RUST_LOG still show it).
    tracing::debug!(
        turns,
        aborted,
        stop_reason = %terminal.stop_reason,
        "agent run finished"
    );
    // Upstream `_willRetryAfterAgentEnd`: an abort never retries, the budget
    // must have room, and the last attempt must actually be retryable. The
    // loop re-issues retryable turns itself, so a value computed here is
    // `false` for a terminal that was already given its chance; the field can
    // become true only if a future loop variant hands a retryable terminal
    // back to the host mid-budget.
    let will_retry = !aborted
        && deps.retry.as_ref().is_some_and(|policy| {
            retry_attempt < policy.max_retries
                && retryable_turn_error(&terminal, current_model.as_ref(), last_turn_retry_class)
        });
    let result = Arc::new(LoopResult {
        terminal,
        // Refcount bump: the run's shared transcript handle moves into the
        // result (the former shape materialized an owned copy here).
        context,
        tool_results,
        // The shared handle becomes the result's owned list (a move when the
        // last per-turn snapshot is gone; a clone only if a hook retained it).
        new_messages: Arc::unwrap_or_clone(new_messages),
        // The finalized starting messages the loop appended (empty for the
        // announce-only `preamble_messages` form): ephemeral hosts extend
        // their in-memory transcript with these before `new_messages`.
        preamble: finalized_preamble,
        turns,
        aborted,
        usage_total,
        will_retry,
    });
    emit_lazy(&options.events, || AgentEvent::AgentEnd {
        // Share the ONE allocation with the event (and with a
        // `run_with_options_shared` caller); the old shape deep-cloned the
        // result here.
        result: Arc::clone(&result),
    });
    emit_extension_lazy(&deps, "agent_end", || {
        serde_json::json!({
            "type": "agent_end",
            // Upstream `agent_end` carries THIS run's produced messages (the
            // new assistant + tool-result messages), not the full transcript.
            "messages": result
                .new_messages
                .iter()
                .map(message_payload)
                .collect::<Vec<_>>(),
        })
    })
    .await;
    result
}

/// Invoke the per-message persistence sink (when installed). Upstream
/// persists each `message_end` during the run (agent-session.ts:1144-1163).
///
/// INVARIANT (lazy message): `build` runs ONLY when a sink is installed, so a
/// host without session storage (print mode, store-less embedders) never pays
/// the caller-side deep clone of the assistant/tool-result message it would
/// have handed to a sink that does not exist. The builder is a pure clone, so
/// skipping it changes nothing observable.
async fn persist_message(options: &LoopOptions, build: impl FnOnce() -> Message) {
    if let Some(sink) = options.persist_message.as_ref() {
        sink(build()).await;
    }
}

/// The completed turn offered to [`PrepareNextTurnHook`] at the top of the
/// next iteration (upstream `lastCompletedTurn`), limited to the immutable
/// per-turn payload (`message`, `tool_results`).
///
/// The live `context` / `new_messages` handles are attached ONLY at the hook
/// call ([`prepare_next_turn_snapshot`]): storing them here would leave a
/// second reference into the run's `Arc<Context>` / `Arc<Vec<Message>>`
/// alive between turns, so the next turn's first [`Arc::make_mut`] would
/// deep-copy the whole transcript — every tool output included — once per
/// turn (O(turns²) bytes copied over a run).
struct CompletedTurn {
    message: AssistantMessage,
    tool_results: Vec<ToolResultMessage>,
    /// Zero-based index of the completed turn (the same value its
    /// `turn_start`/`turn_end` carried).
    turn_index: usize,
}

/// Record the turn that just completed for `prepare_next_turn`.
///
/// A no-op without the hook: nothing reads the snapshot, so the clones of the
/// turn's message and tool results — and the `CompletedTurn` itself — are
/// pure cost on the hook-free (production default) path.
fn record_completed_turn(
    hook: Option<&PrepareNextTurnHook>,
    completed: &mut Option<CompletedTurn>,
    turn_index: usize,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
) {
    if hook.is_none() {
        return;
    }
    *completed = Some(CompletedTurn {
        message: message.clone(),
        tool_results: tool_results.to_vec(),
        turn_index,
    });
}

/// Build the snapshot [`PrepareNextTurnHook`] receives for a completed turn
/// (upstream `lastCompletedTurn = { message, toolResults, context,
/// newMessages }`). The context and produced-message handles are the loop's
/// LIVE handles, attached for the duration of the call only.
fn prepare_next_turn_snapshot(
    completed: &CompletedTurn,
    context: &Arc<Context>,
    new_messages: &Arc<Vec<Message>>,
) -> Arc<FinishTurnContext> {
    Arc::new(FinishTurnContext {
        turn_index: completed.turn_index,
        message: completed.message.clone(),
        tool_results: completed.tool_results.clone(),
        context: Arc::clone(context),
        new_messages: Arc::clone(new_messages),
    })
}

/// Invoke the `finish_turn` hook (when installed) with the completed-turn
/// snapshot.
async fn call_finish_turn(
    hook: Option<&FinishTurnHook>,
    turn_index: usize,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &Arc<Context>,
    new_messages: &Arc<Vec<Message>>,
) -> Option<FinishTurnAction> {
    let hook = hook?;
    hook(FinishTurnContext {
        turn_index,
        message: message.clone(),
        tool_results: tool_results.to_vec(),
        // Shared, not cloned (see `FinishTurnContext::context`).
        context: Arc::clone(context),
        // Refcount bump: the hook sees the run's live produced list (the
        // former `to_vec` deep-cloned every message produced so far on EVERY
        // turn — O(turns x run)).
        new_messages: Arc::clone(new_messages),
    })
    .await
}

/// Build the sink nested executions use to surface `tool_execution_*` events:
/// forwarded to the run's event handler (with `parentToolCallId`), to the
/// native plugin fan-out, and to the extension runtime.
/// One streamed lifecycle event from the tool scheduler's progress sink.
struct ToolProgressItem {
    event: crate::agent::tool_batch::ToolExecutionEvent,
    /// The finalized result's wire shape for an `End`; `None` for a `Start`.
    result: Option<JsonValue>,
}

/// Resolve the `args` a `tool_execution_start` frame carries: the tool gate's
/// post-rewrite input when one was recorded, otherwise the assistant message's
/// call arguments (upstream `tool_execution_start.args`). Callers must invoke
/// this ONLY when at least one observer exists: the whole point of
/// [`emit_tool_progress`]'s subscriber check is that an observer-free run never
/// pays for this clone.
fn resolve_tool_start_args(
    inputs: &std::collections::HashMap<String, JsonValue>,
    message: &AssistantMessage,
    call_id: &str,
) -> JsonValue {
    inputs.get(call_id).cloned().unwrap_or_else(|| {
        // Without an extension gate (the common no-extension run) fall back to
        // the assistant message's tool call, so the start frame still carries
        // upstream's `args`.
        message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) if call.id == call_id => Some(JsonValue::Object(
                    call.arguments
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                )),
                _ => None,
            })
            .unwrap_or(JsonValue::Null)
    })
}

/// Emit one streamed tool lifecycle event: the host `AgentEvent` first, then
/// the extension `tool_execution_*` fan-out, exactly as the former
/// batch-completion loops did — just when the scheduler produces the event.
///
/// Returns whether a `Start` frame materialized the call's `args` (always
/// `false` for `End`). Production callers ignore the result; the
/// `#[doc(hidden)]` test seam below uses it to lock the no-subscriber fast
/// path: when neither a host event handler nor an extension handler is
/// installed, a `Start` returns BEFORE touching `inputs`/`message`.
async fn emit_tool_progress(
    events: &Option<EventHandler>,
    deps: &LoopDeps,
    turn: usize,
    inputs: &std::collections::HashMap<String, JsonValue>,
    message: &AssistantMessage,
    item: ToolProgressItem,
) -> bool {
    use crate::agent::tool_batch::ToolExecutionEvent;
    match &item.event {
        ToolExecutionEvent::Start {
            call_id,
            tool_name,
            parent_call_id,
            ..
        } => {
            let wants_host = events.is_some();
            let (wants_native, wants_js) = extension_event_faces(deps, "tool_execution_start");
            let wants_extension = wants_native || wants_js;
            // No observer at all: do not construct or clone the args. A tool
            // call's argument object can be arbitrarily large (file contents,
            // patches), so this is the hot-path cost this check removes.
            if !wants_host && !wants_extension {
                return false;
            }
            let mut args = resolve_tool_start_args(inputs, message, call_id);
            if wants_host {
                // A8: the `tool_execution_start` event payload must carry the
                // SAME args the host event carries. Re-reading `inputs` here
                // was wrong: `inputs` is only populated by a `tool_call`
                // extension gate, so a host with no extension gate (the phone
                // path) sent `args: null` even though the fallback above
                // resolved the real arguments from the assistant message.
                let host_args = if wants_extension {
                    // Both faces observe the value: pay exactly one clone.
                    args.clone()
                } else {
                    // Only the host event observes it: move, never clone.
                    std::mem::take(&mut args)
                };
                emit_lazy(events, || AgentEvent::ToolExecutionStart {
                    turn,
                    event: item.event.clone(),
                    args: host_args,
                });
            }
            if wants_extension {
                let call_id = call_id.clone();
                let tool_name = tool_name.clone();
                let parent_call_id = parent_call_id.clone();
                emit_extension_lazy(deps, "tool_execution_start", move || {
                    let mut payload = serde_json::json!({
                        "type": "tool_execution_start",
                        "toolCallId": call_id,
                        "toolName": tool_name,
                        "args": args,
                    });
                    if let Some(parent) = parent_call_id {
                        payload["parentToolCallId"] = serde_json::Value::String(parent);
                    }
                    payload
                })
                .await;
            }
            true
        }
        ToolExecutionEvent::End {
            call_id,
            tool_name,
            is_error,
            duration_ms,
            parent_call_id,
            ..
        } => {
            let wants_host = events.is_some();
            let (wants_native, wants_js) = extension_event_faces(deps, "tool_execution_end");
            let wants_extension = wants_native || wants_js;
            // Same observer-free fast path as `Start`: the finalized result's
            // wire shape must not be cloned for a frame nobody reads.
            if !wants_host && !wants_extension {
                return false;
            }
            let mut result = item.result.unwrap_or(JsonValue::Null);
            if wants_host {
                let host_result = if wants_extension {
                    result.clone()
                } else {
                    std::mem::take(&mut result)
                };
                emit_lazy(events, || AgentEvent::ToolExecutionEnd {
                    turn,
                    event: item.event.clone(),
                    result: host_result,
                });
            }
            if wants_extension {
                let call_id = call_id.clone();
                let tool_name = tool_name.clone();
                let is_error = *is_error;
                let duration_ms = *duration_ms;
                let parent_call_id = parent_call_id.clone();
                emit_extension_lazy(deps, "tool_execution_end", move || {
                    let mut payload = serde_json::json!({
                        "type": "tool_execution_end",
                        "toolCallId": call_id,
                        "toolName": tool_name,
                        "result": result,
                        "isError": is_error,
                    });
                    if let Some(duration) = duration_ms {
                        payload["durationMs"] = serde_json::json!(duration);
                    }
                    if let Some(parent) = parent_call_id {
                        payload["parentToolCallId"] = serde_json::Value::String(parent);
                    }
                    payload
                })
                .await;
            }
            false
        }
    }
}

/// Regression seam for the observer-free fast path in [`emit_tool_progress`]:
/// emit one caller-supplied frame and report whether a `Start` materialized
/// the call's args. Exposed only so the integration suite can assert that no
/// host event handler and no extension handler means zero args construction.
#[doc(hidden)]
pub async fn emit_tool_progress_for_test(
    events: &Option<EventHandler>,
    deps: &LoopDeps,
    turn: usize,
    inputs: &std::collections::HashMap<String, JsonValue>,
    message: &AssistantMessage,
    event: crate::agent::tool_batch::ToolExecutionEvent,
    result: Option<JsonValue>,
) -> bool {
    emit_tool_progress(
        events,
        deps,
        turn,
        inputs,
        message,
        ToolProgressItem { event, result },
    )
    .await
}

fn nested_event_sink(
    events: &Option<EventHandler>,
    extensions: &Option<crate::extensions::ExtensionHandle>,
    plugin_events: &Option<Arc<crate::plugins::PluginHost>>,
    turn: &Arc<AtomicUsize>,
) -> NestedEventSink {
    let events = events.clone();
    let extensions = extensions.clone();
    let plugin_events = plugin_events.clone();
    let turn = Arc::clone(turn);
    Arc::new(move |event: NestedToolEvent| {
        let events = events.clone();
        let extensions = extensions.clone();
        let plugin_events = plugin_events.clone();
        let turn = Arc::clone(&turn);
        Box::pin(async move {
            let event_name = match &event {
                NestedToolEvent::Start { .. } => "tool_execution_start",
                NestedToolEvent::End { .. } => "tool_execution_end",
            };
            // Build the host event only when a handler will receive it: the
            // event carries the call's args/result, and cloning those for an
            // observer-free run (bare embedders, print mode without a handler)
            // was pure per-nested-call cost.
            if events.is_some() {
                let current_turn = turn.load(Ordering::SeqCst);
                let agent_event = match &event {
                    NestedToolEvent::Start {
                        call_id,
                        tool_name,
                        args,
                        parent_call_id,
                    } => AgentEvent::ToolExecutionStart {
                        turn: current_turn,
                        event: crate::agent::tool_batch::ToolExecutionEvent::Start {
                            index: 0,
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            parent_call_id: Some(parent_call_id.clone()),
                        },
                        args: args.clone(),
                    },
                    NestedToolEvent::End {
                        call_id,
                        tool_name,
                        is_error,
                        duration_ms,
                        parent_call_id,
                        result,
                    } => AgentEvent::ToolExecutionEnd {
                        turn: current_turn,
                        event: crate::agent::tool_batch::ToolExecutionEvent::End {
                            index: 0,
                            call_id: call_id.clone(),
                            tool_name: tool_name.clone(),
                            is_error: *is_error,
                            duration_ms: *duration_ms,
                            parent_call_id: Some(parent_call_id.clone()),
                        },
                        result: result.clone(),
                    },
                };
                emit(&events, agent_event);
            }
            // Native plugins observe nested starts/ends with the same
            // `parentToolCallId` payload the JS face gets.
            if let Some(host) = plugin_events.as_ref() {
                if host.has_native_handlers(event_name) {
                    let mut payload = nested_event_payload(&event);
                    host.emit_native(event_name, &mut payload).await;
                }
            }
            if let Some(runtime) = extensions.as_ref() {
                if runtime.has_handlers(event_name) {
                    let mut payload = nested_event_payload(&event);
                    if let Err(error) = runtime.emit(event_name, &mut payload).await {
                        crate::ext_diag!("[ext] {event_name} dispatch failed: {error}");
                    }
                }
            }
        })
    })
}

/// Wire payload of one nested `tool_execution_*` event (upstream adds
/// `parentToolCallId`). Built only when a handler is registered for the event
/// (see [`nested_event_sink`]).
fn nested_event_payload(event: &NestedToolEvent) -> serde_json::Value {
    match event {
        NestedToolEvent::Start {
            call_id,
            tool_name,
            args,
            parent_call_id,
        } => serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": call_id,
            "toolName": tool_name,
            "args": args,
            "parentToolCallId": parent_call_id,
        }),
        NestedToolEvent::End {
            call_id,
            tool_name,
            is_error,
            duration_ms,
            parent_call_id,
            result,
        } => {
            let mut payload = serde_json::json!({
                "type": "tool_execution_end",
                "toolCallId": call_id,
                "toolName": tool_name,
                "result": result,
                "isError": is_error,
                "parentToolCallId": parent_call_id,
            });
            if let Some(duration) = duration_ms {
                payload["durationMs"] = serde_json::json!(duration);
            }
            payload
        }
    }
}

/// Consecutive identical single-tool executions before the loop intervenes.
///
/// Twenty turns of a genuinely unchanged call means no progress; a lower
/// threshold would cut legitimate polling (a model waiting for a build or a
/// file whose identical result is unchanged for a few polls), while the
/// measured runaway (487 identical calls) is caught at ~4% of its cost.
///
/// The FIRST trip injects a nudge into the next request instead of ending the
/// run (the call may be a legitimate poll whose result happens to be stable);
/// a SECOND trip of the same streak, after the model ignored the nudge, stops
/// the run with the terminal error below.
const REPEAT_STREAK_STOP: usize = 20;

/// Upper bound on nudges per run: the guard's own infinite-nudge loop (a model
/// that ignores every nudge while the underlying result keeps changing enough
/// to reset the streak) must still be bounded. Each nudge is only injected
/// after a full [`REPEAT_STREAK_STOP`] streak, so this is unreachable in a
/// normal run.
const REPEAT_NUDGE_LIMIT: usize = 3;

/// The steering text injected when the repeat guard trips for the first time.
/// The next request carries it as a user message, so the model can change
/// approach without the run being cut.
fn repeat_nudge_text(streak: usize) -> String {
    format!(
        "You have issued the same tool call with the same arguments and received the same result \
         {streak} times in a row. You are not making progress. Change your approach: do not repeat \
         this call unchanged — inspect the situation differently, try a different tool or \
         arguments, or finish the task and report what you have."
    )
}

/// Terminal error text for a run stopped at [`LoopDeps::max_turns`].
///
/// Hosts and harnesses match on the [`TURN_CAP_ERROR_PREFIX`] prefix: print
/// mode and the round-trip surfaces report the run as incomplete instead of a
/// silent success (upstream has no turn cap, so there is no upstream text to
/// mirror — a truncated run must not look like a finished one).
///
/// The text names the effective cap, the counting rule (one turn is one model
/// request; a message's tool calls execute as one batch) and the operator
/// knob (`PIR_MAX_TURNS`, or the SDK builder's `max_turns`), so a truncated
/// soak run is diagnosable from the terminal message alone.
pub const TURN_CAP_ERROR_PREFIX: &str = "Stopped after the maximum of";

/// The terminal error for a run that exhausted [`LoopDeps::max_turns`].
fn turn_cap_error(max_turns: usize, last: &AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        api: last.api.clone(),
        provider: last.provider.clone(),
        model: last.model.clone(),
        stop_reason: StopReason::Error,
        error_message: Some(format!(
            "{TURN_CAP_ERROR_PREFIX} {max_turns} turns (turn limit reached; one turn is one \
             model request, not one tool call): the run is incomplete — the task was not \
             finished. Raise the limit with PIR_MAX_TURNS (print/RPC/server) or \
             AgentBuilder::max_turns (SDK) and re-run, or give the model a narrower task."
        )),
        timestamp: crate::ai::util::now_ms(),
        ..AssistantMessage::default()
    }
}

/// Whether an error turn is worth re-issuing at the AGENT level (upstream
/// `AgentSession._isRetryableError`, agent-session.ts:3739-3743).
///
/// - Only a [`StopReason::Error`] qualifies; an abort is terminal.
/// - The retry decision is STRUCTURAL, never textual: the provider/transport
///   finalizer classifies the failure while it is still a typed `AiError` and
///   carries [`TurnRetryClass`](crate::ai::error::TurnRetryClass) on the
///   terminal message. Adapter prefixes ("OpenAI API error: …") and the
///   flattened text of `AiError::RetryExhausted` cannot hide the verdict.
///   Only [`TurnRetryClass::RetryableTurn`] may be re-issued; a
///   `TransportExhausted` verdict (the transport already spent its
///   `StreamOptions::max_retries` budget) and a `NonRetryable` verdict
///   terminate here, so the two budgets never multiply.
/// - A message produced outside those finalizers (a synthetic host/embedder
///   stream) carries no verdict and is never re-issued.
/// - For an explicitly retryable turn the transient-error text is still
///   required: the class says the transport rendered no verdict, while the
///   text says the failure itself is a transient (mid-stream drop, timeout,
///   premature stream end) rather than a permanent I/O/parse problem.
/// - Context overflow is compaction's job, not retry's (upstream excludes it
///   explicitly); the session's own overflow recovery still sees the terminal.
fn retryable_turn_error(
    message: &AssistantMessage,
    model: Option<&Model>,
    retry_class: Option<crate::ai::error::TurnRetryClass>,
) -> bool {
    use crate::ai::error::TurnRetryClass;

    if message.stop_reason != StopReason::Error {
        return false;
    }
    if retry_class != Some(TurnRetryClass::RetryableTurn) {
        return false;
    }
    if crate::ai::util::is_context_overflow(message, model.map(|model| model.context_window)) {
        return false;
    }
    crate::ai::util::is_retryable_assistant_error(message)
}

/// The model-visible text of a tool-result message (text blocks only; images
/// carry no comparable progress signal).
fn tool_result_model_text(content: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text(part) = block {
            text.push_str(&part.text);
        }
    }
    text
}

/// Digest of one single-call turn: tool name + canonical arguments + the
/// model-visible result (text + error flag). `structuredContent`/`details`
/// are excluded on purpose — they can vary without any model-visible change
/// (e.g. a shell result's `wall_time_seconds`).
fn single_call_digest(message: &AssistantMessage, result: Option<&(bool, String)>) -> Option<u64> {
    let mut calls = message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolCall(call) => Some(call),
        _ => None,
    });
    let call = calls.next()?;
    if calls.next().is_some() {
        return None;
    }
    let (is_error, text) = result?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call.name.hash(&mut hasher);
    serde_json::to_string(&call.arguments)
        .unwrap_or_default()
        .hash(&mut hasher);
    text.hash(&mut hasher);
    is_error.hash(&mut hasher);
    Some(hasher.finish())
}

/// Dispatch one non-assistant message's full lifecycle, in the order every
/// loop-produced message already uses (declaration / tool-result sites):
/// host `message_start` → extension `message_start` → extension `message_end`
/// (whose same-role replacement is applied) → host `message_end`. The caller
/// appends and persists the returned (finalized) message, so a replacement is
/// what the context, the produced list, and storage record.
///
/// `wire` overrides the extension payload shape for a message pir has no
/// `Message` variant for (a custom-message run: `role: "custom"`); a
/// replacement of such a message cannot deserialize and is rejected, which
/// matches the existing contract.
async fn emit_message_lifecycle(
    deps: &LoopDeps,
    options: &LoopOptions,
    turn: usize,
    mut message: Message,
    wire: Option<&JsonValue>,
) -> Message {
    emit_lazy(&options.events, || AgentEvent::MessageStart {
        turn,
        message: message.clone(),
    });
    let start_wire = match wire {
        Some(wire) => wire.clone(),
        None => message_payload(&message),
    };
    emit_extension_lazy(
        deps,
        "message_start",
        || serde_json::json!({ "type": "message_start", "message": start_wire }),
    )
    .await;
    let expected_role = wire
        .and_then(|wire| wire.get("role"))
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| message.role().to_owned());
    let end_wire = match wire {
        Some(wire) => wire.clone(),
        None => message_payload(&message),
    };
    if let Some(replacement) = emit_message_end(
        deps,
        &expected_role,
        || serde_json::json!({ "type": "message_end", "message": end_wire }),
    )
    .await
    {
        message = replacement;
    }
    emit_lazy(&options.events, || AgentEvent::MessageEnd {
        turn,
        message: message.clone(),
    });
    message
}

/// Announce one starting message (upstream `runAgentLoop`'s
/// `initialMessages`) and apply its lifecycle. `append` entries become part of
/// the loop's context and are persisted at their `message_end`; announce-only
/// entries (`LoopOptions::preamble_messages`, print) only rewrite the matching
/// transcript entry when a handler replaced it.
async fn apply_preamble_message(
    deps: &LoopDeps,
    options: &LoopOptions,
    entry: PreambleMessage,
    context: &mut Arc<Context>,
) -> crate::Result<Message> {
    let PreambleMessage {
        message,
        wire,
        append,
        persist,
    } = entry;
    let original = message;
    let finalized = emit_message_lifecycle(deps, options, 0, original.clone(), wire.as_ref()).await;
    if append {
        // The replacement is what enters the context and the producer's
        // storage (upstream mutates the live message object before the
        // session persists it at `message_end`).
        Arc::make_mut(context).messages.push(finalized.clone());
        match persist {
            Some(sink) => sink(finalized.clone()).await?,
            None => persist_message(options, || finalized.clone()).await,
        }
    } else if finalized != original {
        // The host already appended/persisted this message: rewrite the
        // transcript entry so later requests observe the replacement. The
        // host's storage stays its own concern (print persists before the
        // run).
        let Some(index) = context
            .messages
            .iter()
            .rposition(|message| message == &original)
        else {
            crate::ext_diag!(
                "[ext] preamble message_end replacement has no matching transcript entry"
            );
            return Ok(finalized);
        };
        Arc::make_mut(context).messages[index] = finalized.clone();
    }
    Ok(finalized)
}

/// Push queued steering/follow-up prompts into the context. Each injected
/// message goes through the same per-message persistence sink as
/// loop-produced messages ([`persist_message`]) and is recorded in
/// `new_messages` in sink order, so the post-run batch in the session layer
/// (`skip(already_persisted)`) counts it as already persisted — no duplicate
/// append and no gap. Skipping the sink would leave injected messages out of
/// the durable transcript entirely (crash recovery via `--continue` / server
/// reopen would lose them, and a settle-retry's storage projection would
/// rebuild its context without this run's steering).
///
/// A provider may return a full model-facing message (text plus image blocks)
/// or a custom message. A custom message carries its own wire shape and
/// storage sink ([`PromptMessage::custom`]): its custom entry is written by
/// that sink (with the `entry_appended` dispatch) instead of a regular message
/// entry, and it stays out of the produced list so the post-run batch cannot
/// duplicate it. A store-less host has no per-message sink, so the custom
/// message falls back to the produced list and enters the in-memory transcript
/// like every other injected message.
///
/// Background-origin follow-ups (A5) are the one exception: a completion
/// frame already has its durable copy in the `backgroundToolResult` custom
/// entry (which projects back as a user message on resume), so the live
/// injection enters the run's context only. Persisting it — or recording it
/// in `new_messages`, which the session's post-run batch persists — would
/// duplicate the frame in the resumed transcript. A host with no per-message
/// sink keeps the historical path: with no custom entry, its in-memory
/// transcript is the only copy.
async fn inject_prompts(
    deps: &LoopDeps,
    turn: usize,
    context: &mut Arc<Context>,
    provider: Option<&FollowUpFn>,
    new_messages: &mut Arc<Vec<Message>>,
    options: &LoopOptions,
) -> bool {
    let Some(provider) = provider else {
        return false;
    };
    let prompts = provider().await;
    if prompts.is_empty() {
        return false;
    }
    // Copy-on-write is triggered only now, with at least one message to
    // append: an empty poll (the common case, every turn, for every follow-up
    // source) must not deep-copy a transcript that a hook or observer still
    // shares.
    let context = Arc::make_mut(context);
    for prompt in prompts {
        let PromptMessage {
            message,
            wire,
            persist,
        } = prompt;
        let background_origin = options.persist_message.is_some()
            && message_text_of(&message)
                .is_some_and(crate::agent_session::background::is_background_result_frame);
        // Full message lifecycle (upstream announces queued messages through
        // the SAME `message_start`/`message_end` pair as every other message,
        // and a `message_end` replacement is what enters the transcript).
        // Background-origin frames are announced by their custom entry event
        // instead, so they skip the lifecycle below.
        if background_origin {
            // Live delivery only: the custom entry is the durable copy.
            context.messages.push(message);
            continue;
        }
        let message = emit_message_lifecycle(deps, options, turn, message, wire.as_ref()).await;
        context.messages.push(message.clone());
        // A custom message persists through its own entry sink. Only a
        // store-backed host has a per-message sink; a store-less host keeps
        // the produced-list path (its in-memory transcript is the only copy).
        if let Some(persist) = persist
            .as_ref()
            .filter(|_| options.persist_message.is_some())
        {
            match persist(message.clone()).await {
                Ok(()) => continue,
                Err(error) => {
                    // Fall through to the regular message entry so the
                    // content is not lost when the custom append failed.
                    crate::ext_diag!("[session] queued custom message persistence failed: {error}");
                }
            }
        }
        Arc::make_mut(new_messages).push(message.clone());
        persist_message(options, || message.clone()).await;
    }
    true
}

/// The text of a message that carries exactly one text block (the shape a
/// background completion frame has).
fn message_text_of(message: &Message) -> Option<&str> {
    let Message::User(user) = message else {
        return None;
    };
    let mut blocks = user.content.iter();
    let Some(ContentBlock::Text(text)) = blocks.next() else {
        return None;
    };
    blocks.next().is_none().then_some(text.text.as_str())
}

/// Append messages supplied by `prepare_next_turn` (upstream
/// `AgentLoopTurnUpdate.messages`): each goes through the full message
/// lifecycle (host and extension `message_start`/`message_end`, with the
/// handler replacement applied), enters the context and the run's produced
/// list, and is persisted exactly once at its `message_end`, the same
/// lifecycle the loop applies to its own produced messages and to injected
/// steering prompts.
async fn inject_messages(
    deps: &LoopDeps,
    turn: usize,
    context: &mut Arc<Context>,
    messages: Vec<Message>,
    new_messages: &mut Arc<Vec<Message>>,
    options: &LoopOptions,
) {
    if messages.is_empty() {
        return;
    }
    // See `inject_prompts`: an update that carries no messages (a
    // model/thinking-only replacement) must not deep-copy a shared
    // transcript.
    let context = Arc::make_mut(context);
    for message in messages {
        let message = emit_message_lifecycle(deps, options, turn, message, None).await;
        context.messages.push(message.clone());
        Arc::make_mut(new_messages).push(message.clone());
        persist_message(options, || message.clone()).await;
    }
}

fn emit(handler: &Option<EventHandler>, event: AgentEvent) {
    if let Some(handler) = handler {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(event)))
        {
            tracing::error!(
                panic = %crate::agent::tool_execution::panic_payload_message(payload.as_ref()),
                "agent event handler panicked; continuing the run"
            );
        }
    }
}

/// [`emit`] with a lazily built event: the builder — which clones the small
/// payloads the event carries (`TurnEnd`/`StreamEnd` messages, one
/// tool-result message per call, the run result's produced messages) and
/// refcount-bumps the shared transcript (`AgentStart`/`TurnStart` contexts,
/// the `AgentEnd` result's context) — runs only when a handler is installed.
/// Without one the [`emit`] call dropped the event immediately, so the clone
/// was pure cost; the builder is a pure clone with no side effects, so
/// skipping it is not observable. Every remaining clone is paid only when a
/// handler exists, which is the contract the event's owned payload has.
fn emit_lazy(handler: &Option<EventHandler>, build: impl FnOnce() -> AgentEvent) {
    if let Some(handler) = handler {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler(build());
        })) {
            tracing::error!(
                panic = %crate::agent::tool_execution::panic_payload_message(payload.as_ref()),
                "agent event handler panicked; continuing the run"
            );
        }
    }
}

/// Bound on queued `message_update` events awaiting their QuickJS dispatch
/// (see [`MessageUpdateDispatcher`]). The stream loop keeps at most this many
/// events buffered ahead of the JS side; beyond it the loop waits for the
/// in-flight dispatch — bounded backpressure, never a dropped event.
///
/// Memory policy: the queue holds the per-delta WIRE STRINGS (O(delta) each,
/// built at [`MessageUpdateDispatcher::enqueue`] time), never the stream
/// events: an event's `partial` `Arc` is dropped before the enqueue returns, so
/// even a slow JS side can only hold `BOUND` delta-sized strings — and the
/// provider parser's accumulator stays unshared, which is what keeps its
/// copy-on-write mutation O(delta) instead of O(reply) per delta.
///
/// The same bound caps the native fan-out queue ([`NativeMessageUpdateDispatcher`]).
const MESSAGE_UPDATE_BACKLOG_BOUND: usize = 16;

/// Process-wide stream ids for [`MessageUpdateDispatcher`] (the internal
/// stream key of the `message_update` wire). One id per assistant-message
/// stream, monotonically increasing and never reused, so the JS-side per-stream
/// live message of a finished stream can never be re-anchored by a later
/// stream that happens to share its index space.
#[cfg(feature = "extensions")]
fn next_message_update_stream_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Per-stream record of the tool-call `arguments` object most recently sent to
/// the JS side, keyed by `content_index` (the baseline
/// [`crate::extensions::runtime::message_update_stream_json`] diffs a
/// `toolcall_delta` against to ship the O(delta) `patch`). The retained memory
/// is the arguments of the stream's ACTIVE tool calls — one value per in-flight
/// call, refreshed in place — never a per-delta table.
#[cfg(feature = "extensions")]
#[derive(Debug, Default)]
pub struct ToolArgumentBaselines {
    arguments:
        std::collections::HashMap<usize, std::collections::BTreeMap<String, serde_json::Value>>,
}

#[cfg(feature = "extensions")]
impl ToolArgumentBaselines {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every baseline (a stream's `start` re-anchors the JS accumulator).
    pub fn clear(&mut self) {
        self.arguments.clear();
    }

    pub(crate) fn take(
        &mut self,
        index: usize,
    ) -> Option<std::collections::BTreeMap<String, serde_json::Value>> {
        self.arguments.remove(&index)
    }

    pub(crate) fn put(
        &mut self,
        index: usize,
        arguments: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.arguments.insert(index, arguments);
    }

    pub(crate) fn forget(&mut self, index: usize) {
        self.arguments.remove(&index);
    }
}

/// Ordered, non-blocking dispatcher for `message_update` extension events.
///
/// JS-visible semantics are unchanged: every stream event is dispatched
/// exactly once, in stream order (one FIFO queue, one in-flight dispatch), and
/// the backlog is drained before any other extension event can be dispatched
/// (`message_start` inside the stream; `message_end` and everything after it
/// only once [`consume_stream`] returned). What the loop owns is only the
/// *queue*: [`Self::enqueue`] serializes the stream event to its O(delta) wire
/// on this thread, DROPS the event (and with it the `partial` `Arc`) and never
/// awaits the QuickJS round trip, so the provider stream and the Rust-side
/// event consumers (TUI/RPC/session) run at provider speed while the JS side
/// catches up.
///
/// The wire is deliberately built HERE, on the stream's thread, and not on the
/// extension actor: a `message_update` event carries the streamed message
/// behind an `Arc`, and holding even one clone of it across the provider's next
/// delta turns the parser's copy-on-write mutation into a deep copy of the
/// whole accumulated reply (the ~630 MB of churn this lane removed). Building
/// the wire before the event is queued is what makes the parse in-place again.
#[cfg(feature = "extensions")]
struct MessageUpdateDispatcher {
    /// This stream's wire id (see [`next_message_update_stream_id`]).
    stream: u64,
    /// Per-`content_index` argument baselines for the `toolcall_delta` patch.
    tool_arguments: ToolArgumentBaselines,
    /// Queued per-delta wires awaiting their JS dispatch, oldest first.
    pending: std::collections::VecDeque<String>,
    in_flight: Option<BoxFuture<'static, ()>>,
}

#[cfg(feature = "extensions")]
impl MessageUpdateDispatcher {
    fn new() -> Self {
        Self {
            stream: next_message_update_stream_id(),
            tool_arguments: ToolArgumentBaselines::new(),
            pending: std::collections::VecDeque::new(),
            in_flight: None,
        }
    }

    /// Start the next queued dispatch while the pipeline is idle.
    fn start_next(&mut self, deps: &LoopDeps) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(runtime) = deps.extensions.clone() else {
            return;
        };
        let Some(wire) = self.pending.pop_front() else {
            return;
        };
        self.in_flight = Some(Box::pin(async move {
            // Read-only dispatch: the loop ignores a `message_update`
            // handler's return value (upstream parity gap — `consume_stream`
            // never applies a `message_update` result to the streamed
            // message), so the JS side is not asked to serialize a reply
            // nobody reads.
            if let Err(error) = runtime.emit_stream_update(wire).await {
                crate::ext_diag!("[ext] message_update dispatch failed: {error}");
            }
        }));
    }

    /// Poll the in-flight dispatch once and start the next queued one when it
    /// completed. Never blocks: a still-running JS dispatch stays in flight.
    async fn advance(&mut self, deps: &LoopDeps) {
        if let Some(future) = self.in_flight.as_mut() {
            if futures_util::poll!(future.as_mut()).is_ready() {
                self.in_flight = None;
            }
        }
        self.start_next(deps);
    }

    /// Serialize one event to its per-delta wire, drop it, and queue the wire —
    /// keeping the pipeline moving without awaiting the JS round trip. When the
    /// JS side is more than the backlog bound behind, wait for the in-flight
    /// dispatch instead of growing memory without bound.
    ///
    /// The serialization (and the event's drop, which releases the `partial`
    /// `Arc` the provider parser is about to mutate) happens BEFORE the first
    /// await, so the loop's per-delta work is O(delta) and the parser's next
    /// mutation copies nothing (see [`MessageUpdateDispatcher`]).
    async fn enqueue(&mut self, deps: &LoopDeps, event: AssistantMessageEvent) {
        let wire = crate::extensions::runtime::message_update_stream_json(
            &event,
            self.stream,
            &mut self.tool_arguments,
        );
        drop(event);
        self.pending.push_back(wire);
        self.advance(deps).await;
        while self.pending.len() + usize::from(self.in_flight.is_some())
            > MESSAGE_UPDATE_BACKLOG_BOUND
        {
            let Some(future) = self.in_flight.take() else {
                break;
            };
            future.await;
            self.advance(deps).await;
        }
    }

    /// Dispatch everything still queued, in order, before any other extension
    /// event is emitted.
    async fn drain(&mut self, deps: &LoopDeps) {
        loop {
            self.advance(deps).await;
            let Some(future) = self.in_flight.take() else {
                break;
            };
            future.await;
        }
    }
}

/// Bounded, non-blocking dispatcher for `message_update` fan-out to the NATIVE
/// plugin list (the Rust half of the event, independent of the JS
/// [`MessageUpdateDispatcher`]).
///
/// The provider stream must never wait for a plugin: [`Self::enqueue`] builds
/// the reference payload, hands it to the queue, and only awaits when the
/// backlog reaches [`MESSAGE_UPDATE_BACKLOG_BOUND`] — the same bounded
/// backpressure the JS pipeline applies, so a slow Rust handler cannot grow
/// memory without bound and no delta is silently dropped. Deltas dispatch in
/// stream order; [`Self::drain`] runs before the loop's `message_end` and
/// `agent_end`, so a native consumer always observes the full sequence before
/// the terminal frames.
struct NativeMessageUpdateDispatcher {
    pending: std::collections::VecDeque<JsonValue>,
    in_flight: Option<BoxFuture<'static, ()>>,
}

impl NativeMessageUpdateDispatcher {
    fn new() -> Self {
        Self {
            pending: std::collections::VecDeque::new(),
            in_flight: None,
        }
    }

    /// Start the next queued dispatch while the pipeline is idle.
    fn start_next(&mut self, deps: &LoopDeps) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(host) = deps.plugin_events.clone() else {
            return;
        };
        let Some(mut payload) = self.pending.pop_front() else {
            return;
        };
        self.in_flight = Some(Box::pin(async move {
            host.emit_native("message_update", &mut payload).await;
        }));
    }

    /// Poll the in-flight dispatch once and start the next queued one when it
    /// completed. Never blocks: a still-running dispatch stays in flight.
    async fn advance(&mut self, deps: &LoopDeps) {
        if let Some(future) = self.in_flight.as_mut() {
            if futures_util::poll!(future.as_mut()).is_ready() {
                self.in_flight = None;
            }
        }
        self.start_next(deps);
    }

    /// Queue one stream event's compact payload, awaiting the in-flight
    /// dispatch only when the backlog bound is exceeded.
    async fn enqueue(&mut self, deps: &LoopDeps, event: &AssistantMessageEvent) {
        self.pending.push_back(native_message_update_payload(event));
        self.advance(deps).await;
        while self.pending.len() + usize::from(self.in_flight.is_some())
            > MESSAGE_UPDATE_BACKLOG_BOUND
        {
            let Some(future) = self.in_flight.take() else {
                break;
            };
            future.await;
            self.advance(deps).await;
        }
    }

    /// Dispatch everything still queued, in order.
    async fn drain(&mut self, deps: &LoopDeps) {
        loop {
            self.advance(deps).await;
            let Some(future) = self.in_flight.take() else {
                break;
            };
            future.await;
        }
    }
}

/// Reference construction of the `message_update` payload (upstream shape:
/// `type`, the message, and the raw `assistantMessageEvent`) — the exact
/// values a JS handler observes for one stream delta.
///
/// The event tree already carries the full cumulative `partial` snapshot, and
/// `message` is that same message (`Message::Assistant` serializes as the
/// payload itself — one object carrying its own `role`), so `message` is taken
/// from the event tree instead of deep-cloning the message and serializing it
/// again. Both slots keep exactly the values the previous construction
/// (`serde_json::to_value(event)` + `message_payload(&Message::Assistant(..))`)
/// produced, and the payload object is assembled by moves so the two trees are
/// not copied a third time by `json!`.
///
/// NOTE (streaming hot path): the per-delta path does not call this — it would
/// pay O(reply) per delta for the two snapshot copies (the O(n^2)-per-reply
/// net). The stream wire classifies every event first
/// ([`crate::extensions::runtime::message_update_stream_json`]): terminal
/// events (no `partial`) cost one copy either way and still go through here, so
/// this reference and the reference route of the stream wire cannot drift
/// silently, while the per-delta route builds the live-message view without it.
/// For per-delta events the wire carries the scalars plus the delta, and the JS
/// read-only entry builds the equal live message; the equivalence is pinned by
/// `message_update_stream_wire_carries_the_delta_not_the_snapshot` and
/// `message_update_live_object_matches_the_reference_at_every_delta`.
pub fn message_update_payload(event: &AssistantMessageEvent) -> JsonValue {
    assistant_event_payload(event, true)
}

/// The compact `message_update` payload for the NATIVE (Rust) plugin fan-out.
///
/// Native handlers receive a `serde_json::Value` they own, and the reference
/// construction materializes the cumulative reply twice per delta (the event
/// tree plus the hoisted `message`) — O(reply) per delta, O(reply²) per reply,
/// the same shape the JS face used to pay. This construction keeps the
/// reference KEYS (`type`, `message`, `assistantMessageEvent`) and the
/// reference values for the O(1) scalar part of the message, but drops the
/// message's `content`: the per-delta event already carries the delta, and a
/// native consumer that wants the full text reads `message_end`, exactly like
/// the documented JS picture (`prelude.js.txt`: the live message is rebuilt
/// from deltas there too). Delivered payloads stay O(delta).
pub fn native_message_update_payload(event: &AssistantMessageEvent) -> JsonValue {
    assistant_event_payload(event, false)
}

/// Shared construction behind [`message_update_payload`] and
/// [`native_message_update_payload`]: `with_content` selects whether the
/// hoisted `message` carries the cumulative `content` (reference) or omits it
/// (compact).
fn assistant_event_payload(event: &AssistantMessageEvent, with_content: bool) -> JsonValue {
    if !with_content {
        // Compact route: never materialize the cumulative snapshot. The event
        // view forwards only the event's own fields (O(delta)) and the message
        // view carries the O(1) scalars.
        let assistant_event = match serde_json::to_value(
            crate::coding_agent::json::AssistantMessageEventWire::new(event),
        ) {
            Ok(value) => value,
            Err(_) => JsonValue::Null,
        };
        let message_json = match assistant_event_partial(event) {
            Some(partial) => serde_json::to_value(NativePartialMessageWire::new(partial))
                .unwrap_or_else(|_| serde_json::json!({ "role": "assistant" })),
            None => serde_json::json!({ "role": "assistant" }),
        };
        let mut payload = serde_json::Map::new();
        payload.insert(
            "type".to_string(),
            JsonValue::String("message_update".to_string()),
        );
        payload.insert("message".to_string(), message_json);
        payload.insert("assistantMessageEvent".to_string(), assistant_event);
        return JsonValue::Object(payload);
    }
    let assistant_event = match serde_json::to_value(event) {
        Ok(value) => value,
        Err(_) => JsonValue::Null,
    };
    let message_json = assistant_event
        .get("partial")
        .filter(|partial| partial.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "role": "assistant" }));
    let mut payload = serde_json::Map::new();
    payload.insert(
        "type".to_string(),
        JsonValue::String("message_update".to_string()),
    );
    payload.insert("message".to_string(), message_json);
    payload.insert("assistantMessageEvent".to_string(), assistant_event);
    JsonValue::Object(payload)
}

/// The O(1) scalar view of a partial assistant message for the compact native
/// wire: every field EXCEPT the cumulative `content` (and the potentially
/// large `diagnostics`/`deferred` payloads), with the reference serde names.
/// Drift guard: `native_message_update_message_matches_reference_minus_content`
/// (tests/) compares it against the reference message on every stream shape.
#[derive(serde::Serialize)]
struct NativePartialMessageWire<'a> {
    role: &'a crate::ai::types::AssistantRole,
    api: &'a crate::ai::types::ApiId,
    provider: &'a crate::ai::types::ProviderId,
    model: &'a str,
    #[serde(rename = "responseModel", skip_serializing_if = "Option::is_none")]
    response_model: Option<&'a str>,
    #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
    response_id: Option<&'a str>,
    usage: &'a crate::ai::types::Usage,
    #[serde(rename = "stopReason")]
    stop_reason: crate::ai::types::StopReason,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    error_message: Option<&'a str>,
    #[serde(rename = "rawStopReason", skip_serializing_if = "Option::is_none")]
    raw_stop_reason: Option<&'a str>,
    #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
    end_turn: Option<bool>,
    #[serde(
        rename = "providerThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    provider_thinking_level: Option<&'a str>,
    #[serde(rename = "thinkingLevel", skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'a ThinkingLevel>,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    timestamp: u64,
}

impl<'a> NativePartialMessageWire<'a> {
    fn new(partial: &'a AssistantMessage) -> Self {
        Self {
            role: &partial.role,
            api: &partial.api,
            provider: &partial.provider,
            model: partial.model.as_str(),
            response_model: partial.response_model.as_deref(),
            response_id: partial.response_id.as_deref(),
            usage: &partial.usage,
            stop_reason: partial.stop_reason,
            error_message: partial.error_message.as_deref(),
            raw_stop_reason: partial.raw_stop_reason.as_deref(),
            end_turn: partial.end_turn,
            provider_thinking_level: partial.provider_thinking_level.as_deref(),
            thinking_level: partial.thinking_level.as_ref(),
            duration_ms: partial.duration_ms,
            timestamp: partial.timestamp,
        }
    }
}

async fn consume_stream(
    deps: &LoopDeps,
    mut stream: AssistantMessageEventStream,
    turn: usize,
    handler: &Option<EventHandler>,
    run_started_at: Option<std::time::Instant>,
) -> AssistantMessage {
    let mut final_message = AssistantMessage::default();
    // The assistant `message_start` fires once per stream, with the first
    // partial (upstream emits it on the stream's `start` event,
    // agent-loop.ts:416-421); a stream with no start event emits it with the
    // final message instead (agent-loop.ts:451-456).
    let mut message_start_emitted = false;
    // `message_update` dispatches are queued, not awaited per delta: the loop
    // stays at provider speed and the JS side catches up in order (see
    // [`MessageUpdateDispatcher`]). The backlog is drained before the stream
    // returns, so `message_end` and later extension events still observe every
    // update first.
    #[cfg(feature = "extensions")]
    let mut updates = MessageUpdateDispatcher::new();
    // Native consumers ride an independent bounded pipeline: their face is
    // never the JS wire, and a builtin-only plan has no JS handler at all.
    let mut native_updates = NativeMessageUpdateDispatcher::new();
    while let Some(event) = stream.next().await {
        // First provider stream event of the process, measured from the run
        // start (network/transport latency included).
        if let Some(run_started_at) = run_started_at {
            crate::config::timings::log_phase_once(
                "provider.first_stream_event",
                "provider.first_stream_event",
                run_started_at.elapsed(),
            );
        }
        if !message_start_emitted {
            if let Some(partial) = assistant_event_partial(&event) {
                message_start_emitted = true;
                emit(
                    handler,
                    AgentEvent::StreamStart {
                        turn,
                        message: partial.clone(),
                    },
                );
                emit_extension_lazy(deps, "message_start", || {
                    serde_json::json!({
                        "type": "message_start",
                        "message": value_payload(partial),
                    })
                })
                .await;
            }
        }
        if let AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { error: message, .. } = &event
        {
            final_message = message.clone();
        }
        // The terminal event is ABOUT to be dispatched to every `message_update`
        // consumer: `--mode json`/RPC stdout, the session-event wire, and the
        // JS/native handler fan-out all serialize it as-is. When it carries
        // this provider finalizer's internal retry-class carrier, take the
        // verdict HERE — the loop needs it, but no consumer frame may contain
        // it. `final_message` above already kept the verdict (the loop takes it
        // from there, `agent_loop.rs` post-finalization). Non-terminal events
        // carry no carrier; Done messages never carry one.
        let mut event = event;
        if let AssistantMessageEvent::Error { error: message, .. } = &mut event {
            let _ = crate::ai::error::take_turn_retry_class(message);
        }
        // `message_update` (upstream: token-by-token assistant streaming).
        // The event is queued after the loop's own consumers got it, and
        // [`MessageUpdateDispatcher::enqueue`] serializes it to its O(delta)
        // wire and drops it before its first await (the `partial` `Arc` is
        // released right there), so a slow QuickJS dispatch can no longer delay
        // the TUI/RPC stream and the provider parser's next mutation stays
        // unique: the loop's per-delta work is proportional to the delta, never
        // to the accumulated reply.
        // Each face gets the event once: the JS runtime through its compact
        // wire, the native plugins through the reference payload. With
        // neither face interested it goes straight to the host handler (no
        // clone, no payload).
        let wants_js_updates = deps
            .extensions
            .as_ref()
            .is_some_and(|runtime| runtime.has_handlers("message_update"));
        let wants_native_updates = deps
            .plugin_events
            .as_ref()
            .is_some_and(|host| host.has_native_handlers("message_update"));
        if wants_js_updates || wants_native_updates {
            emit(
                handler,
                AgentEvent::StreamEvent {
                    turn,
                    event: event.clone(),
                },
            );
            if wants_native_updates {
                native_updates.enqueue(deps, &event).await;
            }
            #[cfg(feature = "extensions")]
            if wants_js_updates {
                updates.enqueue(deps, event).await;
            }
        } else {
            emit(handler, AgentEvent::StreamEvent { turn, event });
        }
    }
    #[cfg(feature = "extensions")]
    updates.drain(deps).await;
    native_updates.drain(deps).await;
    if !message_start_emitted {
        // The fallback `message_start` carries the terminal message: emit a
        // carrier-free view (the caller still takes the verdict from the
        // returned `final_message`).
        let mut started = final_message.clone();
        let _ = crate::ai::error::take_turn_retry_class(&mut started);
        emit(
            handler,
            AgentEvent::StreamStart {
                turn,
                message: started.clone(),
            },
        );
        emit_extension_lazy(deps, "message_start", || {
            serde_json::json!({
                "type": "message_start",
                "message": value_payload(&started),
            })
        })
        .await;
    }
    final_message
}

/// The in-flight assistant message carried by a provider stream event
/// (upstream `event.partial`); `None` for terminal events whose message the
/// caller finalizes separately.
pub fn assistant_event_partial(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(&**partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

/// Reconstruct the set of tool names the transcript already declares to the
/// model (upstream `getCurrentTools(transcript)`): the accumulated
/// `toolsAdded` minus `toolsRemoved` across the transcript's system
/// declaration messages. `None` when the transcript carries no declarations.
fn declared_tool_names(messages: &[Message]) -> Option<std::collections::BTreeSet<String>> {
    let mut declared: Option<std::collections::BTreeSet<String>> = None;
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        if let Some(added) = &system.tools_added {
            let set = declared.get_or_insert_with(Default::default);
            for tool in added {
                set.insert(tool.name.clone());
            }
        }
        if let Some(removed) = &system.tools_removed {
            let set = declared.get_or_insert_with(Default::default);
            for tool in removed {
                set.remove(&tool.name);
            }
        }
    }
    declared
}

/// declareToolChanges (upstream agent-loop.ts:333-375): compute the delta
/// between the last declared tool set and the request's executable
/// definitions; when it differs, return the system message that declares
/// `toolsAdded`/`toolsRemoved` (pushed by the caller before the request) and
/// update the tracked set. `None` when the sets match.
fn declare_tool_changes(
    context: &Context,
    last_declared: &mut std::collections::BTreeSet<String>,
) -> Option<Message> {
    let definitions = context.tools.as_ref()?;
    let added: Vec<Tool> = definitions
        .iter()
        .filter(|tool| !last_declared.contains(&tool.name))
        .cloned()
        .collect();
    // Diff against a borrowed view of the request's tool names: the owned set
    // (one `String` per name) is only built when the declaration changes, so
    // the common unchanged-turn path allocates no per-name copies.
    let current: std::collections::BTreeSet<&str> =
        definitions.iter().map(|tool| tool.name.as_str()).collect();
    let removed: Vec<crate::ai::types::ToolReference> = last_declared
        .iter()
        .filter(|name| !current.contains(name.as_str()))
        .map(|name| crate::ai::types::ToolReference { name: name.clone() })
        .collect();
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    *last_declared = current.into_iter().map(str::to_owned).collect();
    Some(Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: if added.is_empty() { None } else { Some(added) },
        tools_removed: if removed.is_empty() {
            None
        } else {
            Some(removed)
        },
        sections: None,
        timestamp: crate::ai::util::now_ms(),
    }))
}

/// Build a callback backed by the real provider registry.
///
/// A `prepare_request`-replaced model takes precedence over the baked model;
/// a replaced thinking level (different from the options' baked level)
/// replaces `options.reasoning`.
pub fn registry_stream(
    registry: Arc<ProviderRegistry>,
    model: Model,
    options: StreamOptions,
) -> StreamFn {
    let base_thinking = options.reasoning.unwrap_or(ThinkingLevel::Off);
    Box::new(move |request: ProviderRequest| {
        let registry = registry.clone();
        let model = request.model.unwrap_or_else(|| model.clone());
        let mut options = options.clone();
        if request.thinking_level != base_thinking {
            options.reasoning = Some(request.thinking_level);
        }
        Box::pin(async move {
            let mut stream = registry.stream(model, request.context, options);
            let mut final_message = AssistantMessage::default();
            while let Some(event) = stream.next().await {
                if let AssistantMessageEvent::Done { message, .. }
                | AssistantMessageEvent::Error { error: message, .. } = event
                {
                    final_message = message;
                }
            }
            final_message
        })
    })
}

/// Build a callback backed by the real provider registry without auth lookup.
/// This is used for explicitly configured local or pi-compatible providers.
pub fn registry_stream_unchecked(
    registry: Arc<ProviderRegistry>,
    model: Model,
    options: StreamOptions,
) -> StreamFn {
    let base_thinking = options.reasoning.unwrap_or(ThinkingLevel::Off);
    Box::new(move |request: ProviderRequest| {
        let registry = registry.clone();
        let model = request.model.unwrap_or_else(|| model.clone());
        let mut options = options.clone();
        if request.thinking_level != base_thinking {
            options.reasoning = Some(request.thinking_level);
        }
        Box::pin(async move {
            let mut stream = registry.stream_unchecked(model, request.context, options);
            let mut final_message = AssistantMessage::default();
            while let Some(event) = stream.next().await {
                if let AssistantMessageEvent::Done { message, .. }
                | AssistantMessageEvent::Error { error: message, .. } = event
                {
                    final_message = message;
                }
            }
            final_message
        })
    })
}

/// Convert a finalized tool call into an upstream-compatible tool-result
/// message referencing the assistant message that requested it.
///
/// `nested_calls` carries the bounded record of calls the tool made through
/// its nested-call capability, `duration_ms` the monotonic execution time,
/// and `structured_content` the tool's machine-readable output. `usage` is the
/// tool-result message usage: a `tool_result` handler's report (upstream
/// merge), the `afterToolCall` hook's usage, or their combination with nested
/// call usage when present.
fn to_tool_result_message(
    finalized: &crate::agent::tool_batch::FinalizedToolCall,
    assistant: &AssistantMessage,
    nested_calls: Option<crate::ai::types::NestedToolCalls>,
    usage: Option<crate::ai::types::Usage>,
) -> ToolResultMessage {
    let tool_result: &ToolResult = &finalized.result;
    let mut content = vec![ContentBlock::Text(TextContent {
        text: tool_result.content.clone(),
        text_signature: None,
    })];
    // Image blocks (upstream tool results carry `{type: "image"}` blocks; the
    // adapter transform downgrades them for non-vision models).
    content.extend(tool_result.images.iter().map(|image| {
        ContentBlock::Image(crate::ai::types::ImageContent {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
        })
    }));
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: finalized.call_id.clone(),
        tool_name: finalized.tool_name.clone(),
        content,
        details: tool_result.details.clone(),
        usage,
        added_tool_names: Vec::new(),
        structured_content: tool_result.structured_content.clone(),
        nested_calls,
        is_error: tool_result.is_error,
        duration_ms: finalized.duration_ms,
        timestamp: assistant.timestamp,
    }
}
