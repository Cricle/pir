//! Shared helpers for the SDK examples.
//!
//! This is a plain module, not an example target. It contains:
//!
//! - [`model`] — a complete [`Model`] for a keyless mock provider;
//! - [`ScriptedProvider`] — the smallest useful custom [`Provider`]. Copy this
//!   file as the starting point for a provider that talks to your own backend;
//!   [`ScriptedProvider::registry_recording`] additionally records every
//!   request, so an example can show what the model was sent;
//! - [`AbortableProvider`] — a gated provider that waits for the run's abort
//!   signal, used by the control example (no sleeps, no races);
//! - [`text_message`] / [`tool_calls_message`] — scripted assistant replies;
//! - [`user_texts`] — the user-message texts of a recorded request;
//! - [`EchoTool`] — a typed tool that emits a `ToolExecutionUpdate` while it
//!   runs, so the streaming example can show that event too.
//!
//! Everything here is offline and deterministic: no network, no API key, no
//! `~/.pi` state.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorReason, Message, Model,
    ModelCost, ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, ToolCall,
    Usage,
};

// ---------------------------------------------------------------------------
// A complete model definition for the mock provider
// ---------------------------------------------------------------------------

/// A minimal chat model. All fields are explicit because [`Model`] has no
/// `Default`; the two limits must be non-zero for `with_registry` to accept it.
pub fn model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock model".into(),
        api: "mock-api".into(),
        provider: "mock".into(),
        base_url: "http://127.0.0.1:1".into(),
        model_type: None,
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        input_limits: None,
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 32_768,
        max_tokens: 4_096,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        headers: None,
        prompt_cache: None,
        thinking_token_budget_field: None,
        compat: None,
    }
}

// ---------------------------------------------------------------------------
// Scripted assistant messages
// ---------------------------------------------------------------------------

/// A terminal text reply.
pub fn text_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "mock-api".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        usage: Usage {
            input: 12,
            output: 8,
            total_tokens: 20,
            ..Usage::default()
        },
        ..AssistantMessage::default()
    }
}

/// A reply that asks the agent to execute one or more tools in a single batch.
pub fn tool_calls_message(calls: &[(&str, Value)]) -> AssistantMessage {
    let content = calls
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            let arguments: BTreeMap<String, Value> = arguments
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            ContentBlock::ToolCall(ToolCall {
                id: format!("call-{index}"),
                name: (*name).to_owned(),
                arguments,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })
        })
        .collect();

    AssistantMessage {
        api: "mock-api".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        content,
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input: 16,
            output: 4,
            total_tokens: 20,
            ..Usage::default()
        },
        ..AssistantMessage::default()
    }
}

/// Concatenated text blocks of a message (empty for tool-only messages).
fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The text of every user message in `context`, in order.
///
/// Steering and follow-up messages arrive as user messages, so this is the
/// simplest way for an example to show what a recorded request actually
/// carried. Image-only user messages are skipped.
pub fn user_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => {
                let text = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The scripted provider
// ---------------------------------------------------------------------------

/// A [`Provider`] that returns pre-scripted replies in order.
///
/// This is deliberately small: one `Mutex<VecDeque<_>>`, no network, no auth.
/// It shows the whole provider contract the SDK needs:
///
/// - `get_models` advertises the model;
/// - `api_key_auth` is empty for a keyless provider;
/// - `stream` returns an [`AssistantMessageEventStream`] and pushes deltas plus
///   a terminal `Done`/`Error` event. The agent loop consumes those events.
///
/// Every request's [`Context`] is also recorded, so
/// [`ScriptedProvider::registry_recording`] can show what the loop sent —
/// including steering and follow-up messages.
pub struct ScriptedProvider {
    replies: Mutex<VecDeque<AssistantMessage>>,
    requests: Arc<Mutex<Vec<Context>>>,
}

impl ScriptedProvider {
    pub fn new(replies: Vec<AssistantMessage>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build the `(registry, model)` pair that
    /// [`AgentBuilder::with_registry`](pir::sdk::AgentBuilder::with_registry)
    /// expects.
    pub fn registry(replies: Vec<AssistantMessage>) -> (Arc<ProviderRegistry>, Model) {
        let (registry, model, _requests) = Self::registry_recording(replies);
        (registry, model)
    }

    /// [`ScriptedProvider::registry`] plus the request log: one [`Context`] per
    /// model request, in order. The log is shared with the provider, so read it
    /// after a run (for example with [`user_texts`]).
    pub fn registry_recording(
        replies: Vec<AssistantMessage>,
    ) -> (Arc<ProviderRegistry>, Model, Arc<Mutex<Vec<Context>>>) {
        let provider = Self::new(replies);
        let requests = Arc::clone(&provider.requests);
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(provider));
        (Arc::new(registry), model(), requests)
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn name(&self) -> &str {
        "scripted mock provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        // Keyless. `with_registry` uses `stream_unchecked`, so no credential
        // is resolved and no environment variable is read.
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        let stream = AssistantMessageEventStream::new();
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();

        let Some(reply) = reply else {
            let mut error = text_message("scripted provider ran out of replies");
            error.stop_reason = StopReason::Error;
            error.error_message = Some("scripted provider ran out of replies".to_owned());
            stream.push(AssistantMessageEventStream::error_event(
                ErrorReason::Error,
                error,
            ));
            return stream;
        };

        let reason = match reply.stop_reason {
            StopReason::ToolUse => TerminalReason::ToolUse,
            StopReason::Length => TerminalReason::Length,
            _ => TerminalReason::Stop,
        };

        // Stream text replies in small chunks so `prompt_stream` shows
        // `MessageUpdate { delta, .. }` events. Tool-call replies have no text:
        // the agent loop reads the tool blocks from the terminal `Done` message.
        let text = text_of(&reply);
        if !text.is_empty() {
            let chars: Vec<char> = text.chars().collect();
            let mut accumulated = String::new();
            for chunk in chars.chunks(24) {
                accumulated.extend(chunk);
                let delta: String = chunk.iter().collect();
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta,
                    partial: Arc::new(text_message(&accumulated)),
                });
            }
        }

        stream.push(AssistantMessageEventStream::done_event(reason, reply));
        stream
    }
}

// ---------------------------------------------------------------------------
// A gated provider for the abort example
// ---------------------------------------------------------------------------

/// A [`Provider`] whose one request never answers until the run is aborted.
///
/// `stream` signals `started` and then waits on the request's
/// [`AbortSignal`](pir::ai::types::AbortSignal). [`Agent::abort`] cancels that
/// signal, the provider emits an aborted terminal event, and the run settles.
/// The handshake makes the abort example deterministic: the caller waits for
/// `started` before calling `abort`, so there is no sleep and no timing race.
///
/// [`Agent::abort`]: pir::sdk::Agent::abort
pub struct AbortableProvider {
    started: Arc<tokio::sync::Notify>,
}

impl AbortableProvider {
    pub fn new() -> Self {
        Self {
            started: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Build the `(registry, model, started)` triple. Await `started.notified()`
    /// once the run is in flight, then call `Agent::abort`.
    pub fn registry() -> (Arc<ProviderRegistry>, Model, Arc<tokio::sync::Notify>) {
        let provider = Self::new();
        let started = Arc::clone(&provider.started);
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(provider));
        (Arc::new(registry), model(), started)
    }
}

impl Default for AbortableProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for AbortableProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn name(&self) -> &str {
        "abortable mock provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        // A producer handle does not keep the stream open for a consumer.
        let producer = stream.producer();
        let started = Arc::clone(&self.started);
        tokio::spawn(async move {
            started.notify_one();
            let Some(signal) = options.signal else {
                producer.end(None);
                return;
            };
            signal.cancelled().await;
            let mut message = text_message("the run was aborted before a reply was produced");
            message.stop_reason = StopReason::Aborted;
            message.error_message = Some("aborted by the caller".to_owned());
            producer.push(AssistantMessageEventStream::error_event(
                ErrorReason::Aborted,
                message,
            ));
        });
        stream
    }
}

// ---------------------------------------------------------------------------
// A typed tool with a live update
// ---------------------------------------------------------------------------

/// Echo a `text` argument. Sends one partial result through
/// [`ToolContext::update_channel`] before returning, which the SDK surfaces as
/// `SdkEvent::ToolExecutionUpdate`.
pub struct EchoTool;

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the provided text back to the model"
    }

    fn parameters(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo" }
            },
            "required": ["text"]
        }))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let text = args["text"].as_str().unwrap_or_default().to_owned();
        let update = ctx.update_channel();

        Box::pin(async move {
            if let Some(update) = update {
                update(json!({
                    "content": [{ "type": "text", "text": format!("echoing: {text}") }],
                    "isError": false
                }));
            }
            // A real tool would await I/O here; the sleep keeps the update and
            // the end event visibly separate for the streaming example.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            ToolResult::ok(format!("echoed: {text}"))
        })
    }
}

// ---------------------------------------------------------------------------
// Small printing helpers
// ---------------------------------------------------------------------------

/// Run one prompt and return its [`AgentOutcome`](pir::sdk::AgentOutcome).
///
/// A thin convenience for the examples: `Agent::prompt` boxes its future
/// internally, so this is safe to call from a small start stack even in
/// unoptimized builds.
pub async fn prompt_outcome(
    agent: &pir::sdk::Agent,
    text: impl Into<String>,
) -> Result<pir::sdk::AgentOutcome, pir::sdk::SdkError> {
    agent.prompt(text).await
}

pub fn print_usage(agent: &pir::sdk::Agent) {
    let usage = agent.usage_totals();
    println!(
        "[usage] turns={} total_tokens={}",
        usage.turns, usage.total_tokens
    );
}
