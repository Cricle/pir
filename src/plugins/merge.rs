//! The shared event-outcome merge layer.
//!
//! ONE ordered dispatch walks every plugin — Rust builtins and the QuickJS
//! JS runtime alike — and merges their typed [`PluginEventOutcome`]s with
//! the upstream `runner.ts` semantics:
//!
//! - `tool_call`: the first `{block: true}` short-circuits the dispatch; a
//!   throwing handler is fail-closed (the call is blocked); non-block
//!   results chain (last result wins, matching `emitToolCall`'s
//!   `result = handlerResult` overwrite); `input` mutations happen in place
//!   so later handlers observe earlier rewrites.
//! - `before_agent_start`: `systemPrompt`/`message` overlays, last wins;
//!   messages accumulate in handler order.
//! - `input`: `handled` short-circuits; `transform` overlays `text`/`images`
//!   field-wise (an omitted field keeps the value an earlier handler set).
//! - `session_before_*`: a `cancel: true` short-circuits. The tree variant
//!   keeps its full result object (preparation overrides) while cancelled
//!   handlers still stop the list.
//! - `resources_discover`: path lists accumulate across handlers.
//! - `tool_result`: field-wise overlay (`content`/`details`/`isError`/
//!   `usage`).
//! - everything else: results are informational (upstream's generic `emit`
//!   ignores them); the last non-null result is surfaced for hosts that
//!   read one (e.g. `project_trust`).
//!
//! Handler errors are isolated into `MergedEmit::errors` and never abort
//! the dispatch (except the fail-closed `tool_call` case).

use serde_json::Value as JsonValue;

use crate::extensions::EventError;

use super::plugin::{InputAction, PluginEventOutcome};

/// Accumulator for one event dispatch across the ordered plugin list.
#[derive(Debug, Default)]
pub struct MergedEmit {
    /// Merged event result (event-specific shape; `Null` when no handler
    /// produced one).
    pub result: JsonValue,
    /// Handler errors (isolated, attributed to the plugin path).
    pub errors: Vec<EventError>,
    /// Whether a short-circuiting outcome ended the dispatch.
    pub stop: bool,
}

impl MergedEmit {
    /// Start a dispatch accumulator.
    pub fn new() -> Self {
        Self {
            result: JsonValue::Null,
            errors: Vec::new(),
            stop: false,
        }
    }

    /// Merge one plugin's outcome. `plugin_path` attributes errors. A
    /// handler-level transport failure surfaces as
    /// [`PluginEventOutcome::Error`] (for `tool_call` that is fail-closed,
    /// matching upstream's "extension failed, blocking execution").
    pub fn merge(&mut self, event: &str, plugin_path: &str, outcome: PluginEventOutcome) {
        if let PluginEventOutcome::Error { error } = &outcome {
            self.errors.push(EventError {
                ext_id: 0,
                event: event.to_owned(),
                error: format!("{plugin_path}: {error}"),
            });
            if event == "tool_call" {
                // Upstream: a throwing tool_call handler blocks the call.
                self.result = serde_json::json!({
                    "block": true,
                    "reason": format!("extension handler failed: {error}"),
                    "terminate": false,
                });
                self.stop = true;
            }
            return;
        }

        match outcome {
            PluginEventOutcome::None => {}
            // Already converted into an isolated error above.
            PluginEventOutcome::Error { .. } => {}
            PluginEventOutcome::Js { result, errors } => {
                self.errors.extend(errors);
                self.merge(event, plugin_path, *result);
            }
            PluginEventOutcome::ToolCall {
                block,
                reason,
                terminate,
            } => {
                self.result = serde_json::json!({
                    "block": block,
                    "reason": reason,
                    "terminate": terminate,
                });
                if block {
                    self.stop = true;
                }
            }
            PluginEventOutcome::BeforeAgentStart {
                system_prompt,
                message,
                messages,
            } => {
                let mut next = if self.result.is_object() {
                    self.result.clone()
                } else {
                    serde_json::Map::new().into()
                };
                if let Some(system_prompt) = system_prompt {
                    next["systemPrompt"] = JsonValue::String(system_prompt);
                }
                // Upstream runner.ts accumulates every handler's message in
                // `messages` (in handler order) and keeps the LAST one in the
                // legacy single `message` field. The loop injects the
                // accumulated list, falling back to `message`; merging only
                // `message` used to drop all but the last handler's
                // injection.
                //
                // `messages`, when present, is the COMPLETE list (the JS
                // runtime's merged outcome carries both fields, and its
                // `messages` already contains `message`). Appending `message`
                // on top of a non-empty list would duplicate that injection;
                // the single field is the fallback for handlers that only
                // return `message` (the Rust `BeforeAgentStart` shape).
                let mut accumulated = next
                    .get("messages")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !messages.is_empty() {
                    accumulated.extend(messages);
                } else if let Some(message) = &message {
                    accumulated.push(message.clone());
                }
                if let Some(message) = message {
                    next["message"] = message;
                }
                if !accumulated.is_empty() {
                    next["messages"] = JsonValue::Array(accumulated);
                }
                self.result = next;
            }
            PluginEventOutcome::Input {
                action,
                text,
                images,
            } => match action {
                InputAction::Handled => {
                    self.result = serde_json::json!({ "action": "handled" });
                    self.stop = true;
                }
                InputAction::Transform => {
                    // Field-wise overlay, not a fresh object: upstream's
                    // prelude keeps `payload` (which already carries the
                    // prompt's current text/images) and only writes the
                    // fields a handler returned. A later handler that
                    // transforms the text without touching `images` must not
                    // drop an earlier handler's replacement images — the
                    // former `json!({action,text})` rebuild here did exactly
                    // that.
                    let mut next = if self.result.is_object() {
                        self.result.clone()
                    } else {
                        serde_json::Map::new().into()
                    };
                    next["action"] = JsonValue::String("transform".to_owned());
                    if let Some(text) = text {
                        next["text"] = JsonValue::String(text);
                    }
                    // `images` is optional: an absent value leaves the
                    // prompt's current images untouched (upstream
                    // `result.images ?? currentImages`), so it is only
                    // written when a handler supplied one.
                    if let Some(images) = images {
                        next["images"] = images;
                    }
                    self.result = next;
                }
                InputAction::Continue => {}
            },
            PluginEventOutcome::SessionBefore { cancel } => {
                let mut next = if self.result.is_object() {
                    self.result.clone()
                } else {
                    serde_json::Map::new().into()
                };
                next["cancel"] = JsonValue::Bool(cancel);
                self.result = next;
                if cancel {
                    self.stop = true;
                }
            }
            PluginEventOutcome::Discover {
                skill_paths,
                prompt_paths,
                theme_paths,
            } => {
                let mut next = if self.result.is_object() {
                    self.result.clone()
                } else {
                    serde_json::Map::new().into()
                };
                let accumulate = |next: &mut JsonValue, key: &str, paths: Vec<String>| {
                    if paths.is_empty() {
                        return;
                    }
                    let mut list = next
                        .get(key)
                        .and_then(JsonValue::as_array)
                        .cloned()
                        .unwrap_or_default();
                    list.extend(paths.into_iter().map(JsonValue::String));
                    next[key] = JsonValue::Array(list);
                };
                accumulate(&mut next, "skillPaths", skill_paths);
                accumulate(&mut next, "promptPaths", prompt_paths);
                accumulate(&mut next, "themePaths", theme_paths);
                self.result = next;
            }
            PluginEventOutcome::ToolResult {
                content,
                details,
                structured_content,
                is_error,
                usage,
            } => {
                let mut next = if self.result.is_object() {
                    self.result.clone()
                } else {
                    serde_json::Map::new().into()
                };
                if let Some(content) = content {
                    next["content"] = content;
                    // Upstream rule: replacing `content` without replacing
                    // `structuredContent` drops the old structured value (it
                    // may no longer match). `Null` is an explicit drop, which
                    // the host reads as "direction absent".
                    if structured_content.is_none() {
                        next["structuredContent"] = JsonValue::Null;
                    }
                }
                if let Some(details) = details {
                    next["details"] = details;
                }
                if let Some(structured_content) = structured_content {
                    next["structuredContent"] = structured_content;
                }
                if let Some(is_error) = is_error {
                    next["isError"] = JsonValue::Bool(is_error);
                }
                if let Some(usage) = usage {
                    next["usage"] = usage;
                }
                self.result = next;
            }
            PluginEventOutcome::TerminalInput { consume, data } => {
                // Upstream `TerminalInputHandler` semantics: a later handler
                // never runs once one consumes; a `data` replacement chains
                // into the next handler (the host writes it back into the
                // payload). The merged result mirrors the wire shape the JS
                // bridge returns.
                let mut next = if self.result.is_object() {
                    self.result.clone()
                } else {
                    serde_json::Map::new().into()
                };
                next["consume"] = JsonValue::Bool(consume);
                if let Some(data) = data {
                    next["data"] = JsonValue::String(data);
                }
                self.result = next;
                if consume {
                    self.stop = true;
                }
            }
            PluginEventOutcome::Generic(value) => {
                // Informational events ignore results; hosts that read one
                // (project_trust) get last-wins. `session_before_tree` rides
                // this arm so its mutating preparation fields survive, but a
                // `cancel: true` still short-circuits the ordered list like
                // the other `session_before_*` events.
                if event == "session_before_tree"
                    && value.get("cancel").and_then(JsonValue::as_bool) == Some(true)
                {
                    self.stop = true;
                }
                self.result = value;
            }
        }
    }
}
