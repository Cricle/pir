use super::*;

use super::prompt_loop::PENDING_USER_RUN_TYPE;

/// The `input` event outcome: the (possibly rewritten) prompt text plus the
/// image content blocks to attach to the user message (upstream `emitInput`
/// keeps `text` + `images`).
#[derive(Clone)]
pub(super) struct PromptInputTransform {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

/// What the extension `input` event decided for one prompt (upstream
/// `_runInputHandlers`): continue with the (possibly transformed) prompt, or
/// `handled` — the prompt is consumed and NO turn starts.
pub(super) enum PromptInputAction {
    Continue(PromptInputTransform),
    /// A handler returned `{action: "handled"}` (upstream
    /// `agent-session.ts:2053-2056`): the host acknowledges the prompt and
    /// returns without starting a run.
    Handled,
}

/// Upstream `InputSource` default for a prompt submitted through
/// `AgentSession.prompt()` (agent-session.ts:2073, `options?.source ??
/// "interactive"`). TUI prompts pass `"interactive"`, RPC prompts `"rpc"` and
/// extension `pi.sendUserMessage()` runs `"extension"`.
const DEFAULT_INPUT_SOURCE: &str = "interactive";

/// Per-task pre-dispatched input: the source for prompts that reach the run
/// through the ordinary `AgentSession::prompt()` seam, plus the transform
/// already produced by a source-aware entry point.
///
/// The run's own `input` dispatch (`start_run`) reads this cell so a prompt
/// is never emitted twice: the entry point dispatches once (which is what lets
/// `handled` swallow the prompt before a turn starts) and the run consumes
/// the prepared transform instead of re-dispatching.
struct PromptInputContext {
    source: String,
    prepared: Option<PromptInputTransform>,
}

tokio::task_local! {
    static PROMPT_INPUT_CONTEXT: Arc<Mutex<PromptInputContext>>;
}

/// Convert a handler-provided `images` list (`[{data, mimeType}]`, upstream
/// `ImageContent`) into pir image content blocks; malformed entries are
/// dropped.
fn images_to_content_blocks(images: &JsonValue) -> Vec<ContentBlock> {
    let Some(items) = images.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let data = item.get("data").and_then(JsonValue::as_str)?;
            let mime_type = item
                .get("mimeType")
                .and_then(JsonValue::as_str)
                .unwrap_or("image/png");
            Some(ContentBlock::Image(crate::ai::types::ImageContent {
                data: data.to_owned(),
                mime_type: mime_type.to_owned(),
            }))
        })
        .collect()
}

/// Emit the extension `input` event for a user prompt (JS-only variant kept
/// for the print host, which drives the raw runtime handle).
///
/// A handler merging `{action: "transform", text, images?}` replaces the
/// prompt text (and images) — upstream `input` event contract. The source is
/// the upstream `prompt()` default (`"interactive"`): the print host submits
/// one-shot prompts through the same default. A `handled` outcome cannot be
/// applied here (this variant has no run to swallow and the print host always
/// runs the model turn); it leaves the prompt untouched. Dispatch failures
/// leave the prompt untouched.
#[cfg(feature = "print")]
pub(crate) async fn transform_prompt_input(runtime: &ExtensionHandle, text: &str) -> String {
    let mut payload = serde_json::json!({
        "type": "input",
        "text": text,
        "images": [],
        "source": DEFAULT_INPUT_SOURCE,
        "streamingBehavior": null,
    });
    match runtime.emit("input", &mut payload).await {
        Ok(outcome) => {
            let action = outcome
                .result
                .get("action")
                .and_then(serde_json::Value::as_str);
            if action == Some("transform") {
                if let Some(transformed) = outcome
                    .result
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    return transformed.to_owned();
                }
            }
            text.to_owned()
        }
        Err(_) => text.to_owned(),
    }
}

/// Dispatch one `input` event through the [`PluginHost`] (the single fan-out
/// list both faces observe) with the given source and streaming behavior.
///
/// `source` is upstream `InputSource` (`"interactive"` | `"rpc"` |
/// `"extension"`); `streamingBehavior` is `"steer"`/`"followUp"` only when the
/// prompt arrives during a run. Absent handlers, a dispatch error or an
/// unknown action all leave the prompt unchanged.
pub(super) async fn dispatch_prompt_input(
    plugins: &PluginHost,
    text: &str,
    images: Vec<ContentBlock>,
    source: &str,
    streaming_behavior: Option<&str>,
) -> PromptInputAction {
    // Nobody declared the event: skip the fan-out. `emit` hands every event
    // to every native plugin regardless of its declared `EventInterest`, so
    // without this gate a plugin that only observes a later event would still
    // be woken (and, if it blocks, would park the run) before the first
    // request.
    if !plugins.has_handlers("input") {
        return PromptInputAction::Continue(PromptInputTransform {
            text: text.to_owned(),
            images,
        });
    }
    let mut payload = serde_json::json!({
        "type": "input",
        "text": text,
        "images": images
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image(image) => serde_json::to_value(image).ok(),
                _ => None,
            })
            .collect::<Vec<_>>(),
        "source": source,
        "streamingBehavior": streaming_behavior,
    });
    match plugins.emit("input", &mut payload).await {
        Ok(outcome) => {
            let action = outcome
                .result
                .get("action")
                .and_then(serde_json::Value::as_str);
            match action {
                Some("transform") => {
                    let text = outcome
                        .result
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| text.to_owned());
                    let images = match outcome.result.get("images") {
                        Some(images) if images.is_array() => images_to_content_blocks(images),
                        _ => images,
                    };
                    PromptInputAction::Continue(PromptInputTransform { text, images })
                }
                // Upstream `handled`: the prompt is consumed by a handler and
                // no turn starts (agent-session.ts:2053-2056).
                Some("handled") => PromptInputAction::Handled,
                _ => PromptInputAction::Continue(PromptInputTransform {
                    text: text.to_owned(),
                    images,
                }),
            }
        }
        Err(_) => PromptInputAction::Continue(PromptInputTransform {
            text: text.to_owned(),
            images,
        }),
    }
}

/// Unified variant used by the run's in-loop dispatch: dispatches the `input`
/// event through the [`PluginHost`] so Rust builtin plugins observe the same
/// event on the same ordered list.
///
/// A source-aware entry point that already dispatched the event
/// ([`AgentSession::prompt_with_source`]) leaves its transform here; this
/// consumes it instead of emitting a second time. The fallback source is the
/// upstream prompt default (`"interactive"`).
pub(super) async fn transform_prompt_input_host(
    plugins: &PluginHost,
    text: &str,
    images: Vec<ContentBlock>,
    streaming_behavior: Option<&str>,
) -> PromptInputTransform {
    let context = PROMPT_INPUT_CONTEXT.try_with(Arc::clone).ok();
    if let Some(context) = &context {
        let prepared = context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prepared
            .take();
        if let Some(prepared) = prepared {
            return prepared;
        }
    }
    let source = context
        .map(|context| {
            context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .source
                .clone()
        })
        .unwrap_or_else(|| DEFAULT_INPUT_SOURCE.to_owned());
    match dispatch_prompt_input(plugins, text, images.clone(), &source, streaming_behavior).await {
        PromptInputAction::Continue(transform) => transform,
        // A direct `AgentSession::prompt()` caller has no entry point that
        // could swallow the prompt; keep the legacy behavior (the run
        // proceeds with the original prompt) and leave a diagnostic.
        PromptInputAction::Handled => {
            crate::ext_diag!(
                "[ext] input handler returned `handled`, but this prompt entry point \
                 cannot swallow the run; continuing with the original prompt"
            );
            PromptInputTransform {
                text: text.to_owned(),
                images,
            }
        }
    }
}

/// Decode an idle `pi.sendUserMessage()` record
/// ([`PENDING_USER_RUN_TYPE`]) back into its text + image blocks, so the
/// source-aware entry point can dispatch `input` for it exactly like a
/// prompt (upstream `sendUserMessage` → `prompt({source: "extension"})`).
fn pending_user_input(message: &CustomRunMessage) -> Option<(String, Vec<ContentBlock>, bool)> {
    if message.custom_type != PENDING_USER_RUN_TYPE {
        return None;
    }
    let details = message.details.as_ref()?;
    let text = details
        .get("text")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned();
    let images = details
        .get("images")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(JsonValue::as_str) == Some("image"))
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    // `pi.sendUserMessage()` dispatches `input` while the notification is
    // consumed (source "extension"); the record marks that so the pump does
    // not emit the event a second time.
    let dispatched = details
        .get("inputDispatched")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    Some((text, images, dispatched))
}

impl AgentSession {
    /// Dispatch the extension `input` event with an explicit
    /// [`InputSource`](DEFAULT_INPUT_SOURCE) and return the prompt a run should
    /// use, or `None` when a handler returned `{action: "handled"}`.
    ///
    /// This is the shared preflight behind [`Self::prompt_with_source`] and
    /// the RPC prompt path: it runs `session_start` (and the notifications its
    /// handlers queue) before the event, so the first `input` handler of a
    /// fresh session observes the same context a normal run would.
    pub async fn prepare_prompt_input(
        &self,
        text: &str,
        images: Vec<ContentBlock>,
        source: &str,
        streaming_behavior: Option<&str>,
    ) -> Option<(String, Vec<ContentBlock>)> {
        let Some(plugins) = self.plugins.as_ref() else {
            return Some((text.to_owned(), images));
        };
        if !plugins.has_handlers("input") {
            return Some((text.to_owned(), images));
        }
        // Mirror the pre-input part of `start_run`: the `ctx` facade carries
        // the live session/model snapshot and `session_start` fires before any
        // event handler sees this prompt.
        self.push_session_context(&self.current_model()).await;
        self.ensure_session_start().await;
        match dispatch_prompt_input(plugins, text, images, source, streaming_behavior).await {
            PromptInputAction::Continue(transform) => Some((transform.text, transform.images)),
            PromptInputAction::Handled => None,
        }
    }

    /// Run one user prompt with an explicit extension `input` source
    /// (`"interactive"` for TUI prompts, `"rpc"` for RPC prompts,
    /// `"extension"` for `pi.sendUserMessage()`).
    ///
    /// Returns `Ok(None)` when an `input` handler returned
    /// `{action: "handled"}`: the prompt is consumed and NO turn starts
    /// (upstream `prompt()` preflight `"handled"`), so no user message is
    /// persisted and no model request is issued.
    pub async fn prompt_with_source(
        &self,
        text: &str,
        source: &str,
        on_event: EventHandler,
    ) -> crate::Result<Option<AssistantMessage>> {
        self.prompt_with_images_and_source(text, Vec::new(), source, on_event)
            .await
    }

    /// [`Self::prompt_with_source`] with image attachments.
    pub async fn prompt_with_images_and_source(
        &self,
        text: &str,
        images: Vec<ContentBlock>,
        source: &str,
        on_event: EventHandler,
    ) -> crate::Result<Option<AssistantMessage>> {
        match self.prepare_prompt_input(text, images, source, None).await {
            Some((text, images)) => self
                .prompt_with_prepared_input(&text, images, on_event)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Run a prompt whose `input` event was ALREADY dispatched by
    /// [`Self::prepare_prompt_input`]: installs the prepared transform for the
    /// run's in-loop dispatch, which consumes it instead of emitting a second
    /// `input` event.
    pub async fn prompt_with_prepared_input(
        &self,
        text: &str,
        images: Vec<ContentBlock>,
        on_event: EventHandler,
    ) -> crate::Result<AssistantMessage> {
        let context = Arc::new(Mutex::new(PromptInputContext {
            source: DEFAULT_INPUT_SOURCE.to_owned(),
            prepared: Some(PromptInputTransform {
                text: text.to_owned(),
                images: images.clone(),
            }),
        }));
        PROMPT_INPUT_CONTEXT
            .scope(context, self.prompt_with_images(text, images, on_event))
            .await
    }

    /// [`Self::run_custom_turn`] with an explicit `input` source.
    ///
    /// An idle `pi.sendUserMessage()` rides this pump as a reserved user-run
    /// record: it dispatches `input` with `source` and may be swallowed by a
    /// `handled` outcome (`Ok(None)`). A genuine custom message
    /// (`pi.sendMessage(..., {triggerTurn: true})`) starts its run as-is — the
    /// `input` event only applies to user prompts upstream.
    pub async fn run_custom_turn_with_source(
        &self,
        message: CustomRunMessage,
        source: &str,
        on_event: EventHandler,
    ) -> crate::Result<Option<AssistantMessage>> {
        if let Some((text, images, dispatched)) = pending_user_input(&message) {
            if dispatched {
                // The notification drain already dispatched `input` (source
                // "extension") and applied `handled`; the run consumes the
                // prepared transform instead of emitting again.
                let context = Arc::new(Mutex::new(PromptInputContext {
                    source: source.to_owned(),
                    prepared: Some(PromptInputTransform {
                        text: text.clone(),
                        images: images.clone(),
                    }),
                }));
                return PROMPT_INPUT_CONTEXT
                    .scope(context, self.run_custom_turn(message, on_event))
                    .await
                    .map(Some);
            }
            return self
                .prompt_with_images_and_source(&text, images, source, on_event)
                .await;
        }
        let context = Arc::new(Mutex::new(PromptInputContext {
            source: source.to_owned(),
            prepared: None,
        }));
        PROMPT_INPUT_CONTEXT
            .scope(context, self.run_custom_turn(message, on_event))
            .await
            .map(Some)
    }
}
