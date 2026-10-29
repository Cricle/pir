//! Shared OpenAI Responses wire codec.
//!
//! Rust equivalent of upstream `packages/ai/src/api/openai-responses-shared.ts`:
//! `input` item conversion (message/function_call/function_call_output/
//! reasoning replay), tool conversion (`type: "function"` with strict JSON
//! schemas), request-body construction, and the streaming event mapping
//! (`response.output_text.delta`, `response.output_item.added/done`,
//! `response.function_call_arguments.*`, `response.reasoning_*`,
//! `response.completed` with usage) including the item-id prefix rules
//! (`fc_`-prefixed item ids, 64-char caps, foreign-call hashing) and the
//! unfinished-tool-call rejection at stream end.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{json, Value};

#[cfg(feature = "provider-openai")]
use super::responses_compat;
use crate::ai::api::base_message;
use crate::ai::api::constrained_sampling;
#[cfg(feature = "provider-openai")]
use crate::ai::api::openai::resolve_sampling_params;
use crate::ai::api::transform::{
    normalize_id_part, short_hash, transform_messages_lazy, transform_messages_lazy_refs,
};
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::*;

/// Providers whose tool-call ids may keep their OpenAI Responses shape
/// (upstream `OPENAI_TOOL_CALL_PROVIDERS`).
pub const OPENAI_TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode"];

/// Azure additionally accepts its own tool-call ids (upstream
/// `AZURE_TOOL_CALL_PROVIDERS`).
pub const AZURE_TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode", "azure"];

/// OpenAI Responses rejects `max_output_tokens` below 16 (upstream
/// `OPENAI_RESPONSES_MIN_OUTPUT_TOKENS`).
pub const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;

/// Fixed `reasoning.effort` while `configuration_update` items carry the
/// thinking level (upstream `CONFIGURATION_UPDATE_REQUEST_EFFORT`). Gated with
/// `provider-openai`: its only reader is the gated Responses request builder.
#[cfg(feature = "provider-openai")]
const CONFIGURATION_UPDATE_REQUEST_EFFORT: &str = "medium";

/// Maximum OpenAI prompt-cache key length (upstream
/// `OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH`).
const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// Clamp a prompt cache key to 64 characters (upstream
/// `clampOpenAIPromptCacheKey`).
pub fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    key.map(|key| {
        key.chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect()
    })
}

/// Encode the pir text signature (`{"v":1,"id":...[,"phase":...]}`) persisted
/// on replayed output text blocks (upstream `encodeTextSignatureV1`).
fn encode_text_signature_v1(id: &str, phase: Option<&str>) -> String {
    let mut payload = json!({"v": 1, "id": id});
    if let Some(phase) = phase {
        payload["phase"] = json!(phase);
    }
    payload.to_string()
}

/// Parse a pir text signature back to its item id and optional phase (upstream
/// `parseTextSignature`).
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature?;
    if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
        if parsed.get("v").and_then(Value::as_u64) == Some(1) {
            if let Some(id) = parsed.get("id").and_then(Value::as_str) {
                let phase = parsed
                    .get("phase")
                    .and_then(Value::as_str)
                    .filter(|phase| *phase == "commentary" || *phase == "final_answer")
                    .map(str::to_string);
                return Some((id.to_string(), phase));
            }
        }
    }
    Some((signature.to_string(), None))
}

// ---------------------------------------------------------------------------
// Tool support modes (upstream getResponsesToolSupport / resolveTranscriptTools)
// ---------------------------------------------------------------------------

/// How a Responses transport expresses tool changes (upstream
/// `TranscriptToolSupport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesToolSupport {
    /// Only the top-level tool list exists; it always holds the current tools.
    None,
    /// Tools can be added at a later message, but not removed or redefined;
    /// additions are anchored with synthetic `tool_search_call` /
    /// `tool_search_output` items.
    Additions,
    /// Every tool is declared in place with `additional_tools` items, and a
    /// later declaration under the same name replaces the earlier one.
    Changes,
}

/// Choose how tool changes ride the request (upstream
/// `getResponsesToolSupport`): `changes` needs both mid-conversation system
/// messages and `additional_tools`, `additions` only tool search, and
/// everything else rebuilds the top-level list.
pub fn get_responses_tool_support(
    supports_mid_convo_system_messages: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
) -> ResponsesToolSupport {
    if supports_mid_convo_system_messages && supports_additional_tools {
        ResponsesToolSupport::Changes
    } else if supports_tool_search {
        ResponsesToolSupport::Additions
    } else {
        ResponsesToolSupport::None
    }
}

/// The top-level request tools plus the number of leading system messages whose
/// tool changes they already hold (upstream `TranscriptTools`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResponsesTranscriptTools {
    /// Tools sent in the top-level `tools` request field.
    pub request_tools: Vec<Tool>,
    /// Number of leading system messages whose tool changes `request_tools`
    /// already holds. Every later system message carries its own tool changes
    /// in place.
    pub folded_system_messages: usize,
}

fn message_declares_tools(message: &Message) -> bool {
    let Message::System(system) = message else {
        return false;
    };
    system
        .tools_added
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        || system
            .tools_removed
            .as_ref()
            .is_some_and(|removed| !removed.is_empty())
}

fn system_text(system: &UserMessage) -> String {
    crate::ai::types::content_text(system)
}

/// Render a later system message for APIs that accept them mid-conversation:
/// content first, then section changes framed by name (upstream
/// `renderSystemMessageUpdate`).
fn render_system_message_update(system: &UserMessage) -> String {
    let mut parts: Vec<String> = Vec::new();
    let text = system_text(system);
    if !text.is_empty() {
        parts.push(text);
    }
    for (name, value) in system.sections.iter().flatten() {
        parts.push(match value {
            None => format!("Removed system prompt section \"{name}\"."),
            Some(value) => format!("Updated system prompt section \"{name}\":\n\n{value}"),
        });
    }
    parts.join("\n\n")
}

fn render_incremental_tools_notice(unavailable: &std::collections::BTreeSet<String>) -> String {
    let notice = "This is an incremental tools update. Previously declared tools remain available unless explicitly marked unavailable. A tool declared again here is available again, and its latest definition replaces the earlier one.";
    if unavailable.is_empty() {
        notice.to_string()
    } else {
        format!(
            "{notice}\nStill unavailable: {}.",
            unavailable.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

fn render_tool_removal_notice(names: &[String]) -> String {
    format!(
        "The following tools are no longer available. Do not call them:\n{}",
        names
            .iter()
            .map(|name| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The leading system message a Responses request starts from, plus how many
/// entries of `Context::messages` it consumed.
pub(crate) struct ResponsesHead {
    pub message: Option<Message>,
    /// `1` when the head was merged from `messages[0]`; `0` otherwise.
    pub skip_first: usize,
}

/// Build the leading system message for a Responses request.
///
/// Upstream normalizes a transcript whose first system message already holds
/// the prompt and the initial tools. pir carries the prompt out of band
/// (`Context::system_prompt`) and the agent transcript starts with a
/// declaration-only system message (`toolsAdded` with no content), so that
/// message is used as the head and the prompt is merged into it. Without a
/// declaration, the prompt and `Context::tools` become the head (the direct
/// API shape); when declarations exist, `Context::tools` only describes the
/// current executable set and must not rewrite the cached tool prefix.
fn build_responses_head(context: &Context, supports_mid_convo: bool) -> ResponsesHead {
    let declares_tools = context.messages.iter().any(message_declares_tools);
    if supports_mid_convo {
        if let Some(Message::System(first)) = context.messages.first() {
            let declares = first
                .tools_added
                .as_ref()
                .is_some_and(|tools| !tools.is_empty())
                || first
                    .tools_removed
                    .as_ref()
                    .is_some_and(|removed| !removed.is_empty());
            if declares {
                if let Some(prompt) = context
                    .system_prompt
                    .as_deref()
                    .filter(|prompt| !prompt.is_empty())
                {
                    let mut head = first.clone();
                    let existing = system_text(first);
                    let text = if existing.is_empty() {
                        prompt.to_string()
                    } else {
                        format!("{prompt}\n\n{existing}")
                    };
                    head.content = vec![ContentBlock::Text(TextContent {
                        text,
                        text_signature: None,
                    })];
                    return ResponsesHead {
                        message: Some(Message::System(head)),
                        skip_first: 1,
                    };
                }
            }
        }
    }
    let tools = if declares_tools {
        None
    } else {
        context.tools.as_deref()
    };
    ResponsesHead {
        message: create_initial_system_message(context.system_prompt.as_deref(), tools),
        skip_first: 0,
    }
}

/// Current tools after replaying the system-message deltas (upstream
/// `getCurrentTools`): within one message removals apply before additions, and
/// a later definition replaces an earlier one under the same name in place.
fn current_tools_from<'a>(messages: impl Iterator<Item = &'a Message>) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        for reference in system.tools_removed.iter().flatten() {
            tools.retain(|tool| tool.name != reference.name);
        }
        for tool in system.tools_added.iter().flatten() {
            match tools.iter_mut().find(|existing| existing.name == tool.name) {
                Some(existing) => *existing = tool.clone(),
                None => tools.push(tool.clone()),
            }
        }
    }
    tools
}

/// Index (among system messages) of the last system message with a removal or
/// same-name redeclaration, if any (upstream `findLastNonAdditiveToolChange`).
fn last_non_additive_tool_change<'a>(messages: impl Iterator<Item = &'a Message>) -> Option<usize> {
    let mut declared: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut last = None;
    let mut index = 0usize;
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        if system
            .tools_removed
            .as_ref()
            .is_some_and(|removed| !removed.is_empty())
        {
            last = Some(index);
        }
        for tool in system.tools_added.iter().flatten() {
            if !declared.insert(tool.name.as_str()) {
                last = Some(index);
            }
        }
        index += 1;
    }
    last
}

/// Split tool declarations between the top-level request field and in-place
/// changes (upstream `resolveTranscriptTools`). When the transport cannot take
/// mid-conversation system messages, the transcript is collapsed first, so the
/// top-level list always holds the current tools and nothing is anchored in
/// place.
fn resolve_transcript_tools_for<'a>(
    head: Option<&'a Message>,
    rest: &'a [Message],
    support: ResponsesToolSupport,
    supports_mid_convo: bool,
) -> ResponsesTranscriptTools {
    let system_messages: Vec<&Message> = head
        .into_iter()
        .chain(rest.iter())
        .filter(|message| matches!(message, Message::System(_)))
        .collect();
    if !supports_mid_convo {
        return ResponsesTranscriptTools {
            request_tools: current_tools_from(system_messages.iter().copied()),
            folded_system_messages: system_messages.len(),
        };
    }
    match support {
        ResponsesToolSupport::None => ResponsesTranscriptTools {
            request_tools: current_tools_from(system_messages.iter().copied()),
            folded_system_messages: system_messages.len(),
        },
        ResponsesToolSupport::Changes => ResponsesTranscriptTools {
            request_tools: Vec::new(),
            folded_system_messages: 0,
        },
        ResponsesToolSupport::Additions => {
            let initial_system = head.is_some()
                || rest
                    .first()
                    .is_some_and(|message| matches!(message, Message::System(_)));
            let last_non_additive = last_non_additive_tool_change(system_messages.iter().copied());
            let folded = std::cmp::max(
                usize::from(initial_system),
                last_non_additive.map(|index| index + 1).unwrap_or(0),
            )
            .min(system_messages.len());
            ResponsesTranscriptTools {
                request_tools: current_tools_from(system_messages.iter().copied().take(folded)),
                folded_system_messages: folded,
            }
        }
    }
}

pub(crate) struct ResponsesTranscriptPlan {
    pub support: ResponsesToolSupport,
    pub head: Option<Message>,
    pub rest_start: usize,
    pub request_tools: Vec<Tool>,
    pub folded_system_messages: usize,
}

pub(crate) fn plan_responses_transcript(
    context: &Context,
    supports_mid_convo_system_messages: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
) -> ResponsesTranscriptPlan {
    let support = get_responses_tool_support(
        supports_mid_convo_system_messages,
        supports_additional_tools,
        supports_tool_search,
    );
    let head = build_responses_head(context, supports_mid_convo_system_messages);
    let rest = &context.messages[head.skip_first.min(context.messages.len())..];
    let resolved = resolve_transcript_tools_for(
        head.message.as_ref(),
        rest,
        support,
        supports_mid_convo_system_messages,
    );
    ResponsesTranscriptPlan {
        support,
        head: head.message,
        rest_start: head.skip_first,
        request_tools: resolved.request_tools,
        folded_system_messages: resolved.folded_system_messages,
    }
}

/// Tools a Responses request declares in its top-level `tools` field
/// (upstream `resolveTranscriptTools(...).requestTools`). Public for tests/.
pub fn resolve_responses_request_tools(
    context: &Context,
    supports_mid_convo_system_messages: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
) -> Vec<Tool> {
    plan_responses_transcript(
        context,
        supports_mid_convo_system_messages,
        supports_additional_tools,
        supports_tool_search,
    )
    .request_tools
}

// ---------------------------------------------------------------------------
// Message conversion (upstream convertResponsesMessages)
// ---------------------------------------------------------------------------

pub struct ConvertResponsesMessagesOptions {
    /// Send the leading system prompt as the first input item (upstream
    /// `includeSystemPrompt`; codex sends it via `instructions` instead).
    pub include_system_prompt: bool,
    /// Send later system messages in place instead of folding them into the
    /// leading prompt (upstream `supportsMidConvoSystemMessages`).
    pub supports_mid_convo_system_messages: bool,
    /// Whether the provider accepts `additional_tools` input items
    /// (upstream `supportsAdditionalTools`).
    pub supports_additional_tools: bool,
    /// Whether synthetic tool search (`tool_search_call` /
    /// `tool_search_output`) can anchor later tool additions (upstream
    /// `supportsToolSearch`).
    pub supports_tool_search: bool,
    pub supports_strict_mode: bool,
    /// Whether the provider accepts the `developer` instruction role on
    /// reasoning models (upstream `supportsDeveloperRole`); `Some(false)`
    /// forces the `system` role.
    pub supports_developer_role: Option<bool>,
    /// Effort for the reply being generated. When set, the thinking level is
    /// sent as `configuration_update` items instead of a changing
    /// `reasoning.effort`, which would invalidate the prompt cache. One item
    /// goes before every earlier same-model reply with a recorded thinking
    /// level, and one with this effort goes at the end (upstream
    /// `configurationUpdateEffort`).
    pub configuration_update_effort: Option<String>,
}

impl Default for ConvertResponsesMessagesOptions {
    fn default() -> Self {
        Self {
            include_system_prompt: true,
            supports_mid_convo_system_messages: false,
            supports_additional_tools: false,
            supports_tool_search: false,
            supports_strict_mode: false,
            supports_developer_role: None,
            configuration_update_effort: None,
        }
    }
}

/// The effort string a thinking level is sent as (upstream
/// `model.thinkingLevelMap?.[level] ?? level`): a mapped string wins, `None`
/// marks the level unsupported (the caller never sends it), and an absent map
/// passes the level's wire key through. Gated with `provider-openai`: its only
/// callers are the gated Responses request builders and adapters.
#[cfg(feature = "provider-openai")]
pub(crate) fn responses_effort_for_level(model: &Model, level: ThinkingLevel) -> String {
    let key = level.wire_key();
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(key))
        .cloned()
        .flatten()
        .unwrap_or_else(|| key.to_string())
}

/// Append one `configuration_update` input item (upstream
/// `pushConfigurationUpdate`): it changes the reasoning effort from that point
/// on without changing the cached prompt prefix.
fn push_configuration_update(messages: &mut Vec<Value>, effort: &str) {
    messages.push(json!({
        "type": "configuration_update",
        "reasoning": {"effort": effort},
    }));
}

fn normalize_tool_call_id_for_responses(
    id: &str,
    model: &Model,
    source: &AssistantMessage,
    allowed: &[&str],
) -> String {
    if !allowed.contains(&model.provider.as_str()) {
        return normalize_id_part(id);
    }
    if !id.contains('|') {
        return normalize_id_part(id);
    }
    let mut parts = id.splitn(2, '|');
    let call_id = parts.next().unwrap_or("");
    let item_id = parts.next().unwrap_or("");
    let normalized_call_id = normalize_id_part(call_id);
    let is_foreign = source.provider != model.provider || source.api != model.api;
    let mut normalized_item_id = if is_foreign {
        let hash = short_hash(item_id);
        let mut normalized = format!("fc_{hash}");
        if normalized.chars().count() > 64 {
            normalized = normalized.chars().take(64).collect();
        }
        normalized
    } else {
        normalize_id_part(item_id)
    };
    // OpenAI Responses API requires item ids to start with "fc".
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

fn tool_result_output(model: &Model, content: &[ContentBlock]) -> Value {
    let text = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&ImageContent> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    let supports_images = model.input.iter().any(|input| input == "image");
    // Whitespace-only output (a bare `\r\n` from a shell tool) is the empty
    // placeholder in BOTH branches: some providers reject a whitespace-only
    // tool output with a 400 that repeats on every later request (#8720, D35).
    let has_text = !text.trim().is_empty();
    if images.is_empty() || !supports_images {
        let text = if has_text {
            text
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return json!(text);
    }
    let mut output = Vec::new();
    if has_text {
        output.push(json!({"type": "input_text", "text": text}));
    }
    for image in images {
        output.push(json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
        }));
    }
    Value::Array(output)
}

/// Tool definitions for an in-message declaration. A definition the normal
/// converter rejects falls back to the plain non-strict function shape: the
/// transcript already declared it, so dropping it would silently change the
/// request.
fn convert_tools_lenient(
    tools: &[Tool],
    supports_strict_mode: bool,
    tool_search_result: bool,
) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            convert_responses_tools_with(
                std::slice::from_ref(tool),
                supports_strict_mode,
                tool_search_result,
            )
            .ok()
            .and_then(|mut converted| converted.pop())
            .unwrap_or_else(|| {
                let mut function = json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                });
                if tool_search_result {
                    function["defer_loading"] = json!(true);
                }
                function
            })
        })
        .collect()
}

/// Append the in-place tool items for one anchored system message (upstream
/// `appendSystemToolAdditions`): `additional_tools` in `changes` mode, a
/// synthetic tool-search pair in `additions` mode, nothing in `none` mode.
#[allow(clippy::too_many_arguments)]
fn append_system_tool_additions(
    messages: &mut Vec<Value>,
    system: &UserMessage,
    support: ResponsesToolSupport,
    instruction_role: &str,
    declared_additional_tools: &mut bool,
    unavailable_tools: &mut std::collections::BTreeSet<String>,
    seed: &str,
    supports_strict_mode: bool,
) {
    let tools = system.tools_added.as_deref().unwrap_or(&[]);
    for tool in tools {
        unavailable_tools.remove(&tool.name);
    }
    if tools.is_empty() {
        return;
    }
    match support {
        ResponsesToolSupport::Changes => {
            if *declared_additional_tools {
                messages.push(json!({
                    "role": instruction_role,
                    "content": render_incremental_tools_notice(unavailable_tools),
                }));
            }
            *declared_additional_tools = true;
            messages.push(json!({
                "type": "additional_tools",
                "role": "developer",
                "tools": convert_tools_lenient(tools, supports_strict_mode, false),
            }));
        }
        ResponsesToolSupport::Additions => {
            let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
            let call_id = format!(
                "pi_tool_load_{}",
                short_hash(&format!("{seed}:{}", names.join(",")))
            );
            messages.push(json!({
                "type": "tool_search_call",
                "call_id": call_id,
                "execution": "client",
                "status": "completed",
                "arguments": {"query": names.join(" "), "limit": names.len()},
            }));
            messages.push(json!({
                "type": "tool_search_output",
                "call_id": call_id,
                "execution": "client",
                "status": "completed",
                "tools": convert_tools_lenient(tools, supports_strict_mode, true),
            }));
        }
        ResponsesToolSupport::None => {}
    }
}

/// Apply one anchored system message's tool changes and return the text parts
/// that follow its content: the incremental-tools notice is emitted by
/// `append_system_tool_additions`, and true removals are announced as text
/// (upstream renders them after this message's additions, before recording the
/// names as unavailable).
#[allow(clippy::too_many_arguments)]
fn apply_anchored_tool_changes(
    messages: &mut Vec<Value>,
    system: &UserMessage,
    support: ResponsesToolSupport,
    instruction_role: &str,
    declared_additional_tools: &mut bool,
    unavailable_tools: &mut std::collections::BTreeSet<String>,
    seed: &str,
    supports_strict_mode: bool,
) -> Vec<String> {
    append_system_tool_additions(
        messages,
        system,
        support,
        instruction_role,
        declared_additional_tools,
        unavailable_tools,
        seed,
        supports_strict_mode,
    );
    // A redefinition is a removal plus an addition of the same name; only true
    // removals are announced.
    let added: std::collections::BTreeSet<&str> = system
        .tools_added
        .iter()
        .flatten()
        .map(|tool| tool.name.as_str())
        .collect();
    let removed: Vec<String> = system
        .tools_removed
        .iter()
        .flatten()
        .map(|reference| reference.name.clone())
        .filter(|name| !added.contains(name.as_str()))
        .collect();
    if removed.is_empty() {
        return Vec::new();
    }
    for name in &removed {
        unavailable_tools.insert(name.clone());
    }
    vec![render_tool_removal_notice(&removed)]
}

/// Build the `input` items for a `/responses` request (upstream
/// `convertResponsesMessages`). Public for tests/.
pub fn convert_responses_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &[&str],
    options: &ConvertResponsesMessagesOptions,
) -> Vec<Value> {
    let allowed = allowed_tool_call_providers.to_vec();
    // Instruction-role selection (upstream openai-responses-shared.ts:214-215):
    // reasoning models use `developer` unless the compat knob explicitly
    // disables the role.
    let instruction_role = if model.reasoning && options.supports_developer_role != Some(false) {
        "developer"
    } else {
        "system"
    };

    // Upstream resolveTranscript: when the API cannot take mid-conversation
    // system messages they are folded into the leading prompt text. The
    // transcript enters the transform BORROWED when mid-conversation system
    // messages are supported; the collapse branch keeps a Vec<&Message> (one
    // pointer per message) instead of cloning every surviving message, and
    // `transform_messages_lazy_refs` only rebuilds the messages that actually
    // change. The folded prompt is Cow: borrowed when no system text was
    // appended.
    let plan = plan_responses_transcript(
        context,
        options.supports_mid_convo_system_messages,
        options.supports_additional_tools,
        options.supports_tool_search,
    );
    let support = plan.support;
    let mut collapsed_leading: Option<std::borrow::Cow<'_, str>> = None;
    let normalize = |id: &str, source: &AssistantMessage| {
        normalize_tool_call_id_for_responses(id, model, source, &allowed)
    };
    let replay = if options.supports_mid_convo_system_messages {
        transform_messages_lazy(
            &context.messages[plan.rest_start..],
            model,
            Some(&normalize),
        )
    } else {
        let mut leading: std::borrow::Cow<'_, str> =
            std::borrow::Cow::Borrowed(context.system_prompt.as_deref().unwrap_or(""));
        let mut kept: Vec<&Message> = Vec::with_capacity(context.messages.len());
        for message in &context.messages {
            match message {
                Message::System(system) => {
                    let text = system_text(system);
                    if !text.is_empty() {
                        let mut owned = leading.into_owned();
                        if !owned.is_empty() {
                            owned.push_str("\n\n");
                        }
                        owned.push_str(&text);
                        leading = std::borrow::Cow::Owned(owned);
                    }
                }
                other => kept.push(other),
            }
        }
        collapsed_leading = Some(leading);
        transform_messages_lazy_refs(&kept, model, Some(&normalize))
    };

    let mut messages: Vec<Value> = Vec::new();
    if let Some(leading) = &collapsed_leading {
        if options.include_system_prompt && !leading.is_empty() {
            messages.push(json!({"role": instruction_role, "content": leading}));
        }
    }

    let folded = if options.supports_mid_convo_system_messages {
        plan.folded_system_messages
    } else {
        // Nothing is anchored after the collapse: every system message was
        // folded into the leading prompt and the top-level list is current.
        usize::MAX
    };
    let mut declared_additional_tools = false;
    let mut unavailable_tools: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut system_index = 0usize;
    let mut source_index = 0usize;
    let mut msg_index = 0usize;

    if options.supports_mid_convo_system_messages {
        if let Some(head) = plan.head.as_ref() {
            let anchors = system_index >= folded;
            system_index += 1;
            let mut parts: Vec<String> = Vec::new();
            let base = match head {
                Message::System(system) => crate::ai::types::system_message_text(system),
                _ => String::new(),
            };
            if !base.is_empty() {
                parts.push(base);
            }
            if anchors {
                if let Message::System(system) = head {
                    parts.extend(apply_anchored_tool_changes(
                        &mut messages,
                        system,
                        support,
                        instruction_role,
                        &mut declared_additional_tools,
                        &mut unavailable_tools,
                        "system:0",
                        options.supports_strict_mode,
                    ));
                }
            }
            if options.include_system_prompt {
                let text = parts.join("\n\n");
                if !text.is_empty() {
                    messages.push(json!({"role": instruction_role, "content": text}));
                }
            }
            source_index += 1;
        }
    }

    for message in &replay {
        let is_leading = source_index == 0 && matches!(message.as_ref(), Message::System(_));
        source_index += 1;
        match message.as_ref() {
            Message::System(system) => {
                let anchors = system_index >= folded;
                system_index += 1;
                let mut parts: Vec<String> = Vec::new();
                let base = if is_leading {
                    crate::ai::types::system_message_text(system)
                } else {
                    render_system_message_update(system)
                };
                if !base.is_empty() {
                    parts.push(base);
                }
                if anchors {
                    parts.extend(apply_anchored_tool_changes(
                        &mut messages,
                        system,
                        support,
                        instruction_role,
                        &mut declared_additional_tools,
                        &mut unavailable_tools,
                        &format!("system:{msg_index}"),
                        options.supports_strict_mode,
                    ));
                }
                let text = parts.join("\n\n");
                if (!is_leading || options.include_system_prompt) && !text.is_empty() {
                    messages.push(json!({"role": instruction_role, "content": text}));
                }
                msg_index += 1;
            }
            Message::User(user) | Message::Developer(user) => {
                let mut content = Vec::new();
                for block in &user.content {
                    match block {
                        ContentBlock::Text(text) if !text.text.is_empty() => {
                            content.push(json!({"type": "input_text", "text": text.text}));
                        }
                        ContentBlock::Image(image)
                            if model.input.iter().any(|input| input == "image") =>
                        {
                            content.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    continue;
                }
                messages.push(json!({"role": "user", "content": content}));
                msg_index += 1;
            }
            Message::Assistant(assistant) => {
                let same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let same_model = same_provider_and_api && assistant.model == model.id;
                let different_model = same_provider_and_api && assistant.model != model.id;
                let mut output: Vec<Value> = Vec::new();
                let mut text_block_index = 0usize;
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking(thinking) => {
                            if let Some(signature) = &thinking.thinking_signature {
                                if let Ok(item) = serde_json::from_str::<Value>(signature) {
                                    output.push(item);
                                }
                            }
                        }
                        ContentBlock::Text(text) => {
                            let parsed = parse_text_signature(text.text_signature.as_deref());
                            let fallback_id = if text_block_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            let mut msg_id = parsed
                                .as_ref()
                                .map(|(id, _)| id.clone())
                                .unwrap_or(fallback_id);
                            // OpenAI requires ids to be at most 64 characters.
                            if msg_id.chars().count() > 64 {
                                msg_id = format!("msg_{}", short_hash(&msg_id));
                            }
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text.text, "annotations": []}],
                                "status": "completed",
                                "id": msg_id,
                            });
                            if let Some((_, Some(phase))) = parsed.as_ref() {
                                item["phase"] = json!(phase);
                            }
                            output.push(item);
                        }
                        ContentBlock::ToolCall(call) => {
                            let mut parts = call.id.splitn(2, '|');
                            let call_id = parts.next().unwrap_or("").to_string();
                            let mut item_id: Option<String> = parts.next().map(str::to_string);
                            // Drop ids that do not match the replayed item
                            // type (function_call ids must be fc_*) or belong
                            // to a different model (upstream pairing
                            // validation).
                            if different_model
                                || item_id.as_deref().is_none_or(|id| !id.starts_with("fc_"))
                            {
                                item_id = None;
                            }
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": call.name,
                                "arguments": serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            });
                            if let Some(item_id) = item_id {
                                item["id"] = json!(item_id);
                            }
                            if same_provider_and_api {
                                if let Some(namespace) = &call.namespace {
                                    item["namespace"] = json!(namespace);
                                }
                            }
                            output.push(item);
                        }
                        ContentBlock::Image(_) => {}
                    }
                }
                if output.is_empty() {
                    continue;
                }
                // Replay the effort this reply was generated with as an
                // effort-only item, so earlier input stays byte-identical
                // and the prompt cache survives level changes (#10853).
                if options.configuration_update_effort.is_some() && same_model {
                    if let Some(effort) = &assistant.provider_thinking_level {
                        push_configuration_update(&mut messages, effort);
                    }
                }
                messages.extend(output);
                msg_index += 1;
            }
            Message::ToolResult(result) => {
                let call_id = result
                    .tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or("")
                    .to_string();
                messages.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_result_output(model, &result.content),
                }));
            }
        }
    }
    if let Some(effort) = &options.configuration_update_effort {
        push_configuration_update(&mut messages, effort);
    }
    messages
}

// ---------------------------------------------------------------------------
// Tool conversion (upstream convertResponsesTools)
// ---------------------------------------------------------------------------

/// Convert tool definitions to the Responses `type: "function"` shape (upstream
/// `convertResponsesTools`). Public for tests/.
pub fn convert_responses_tools(tools: &[Tool], supports_strict_mode: bool) -> AiResult<Vec<Value>> {
    convert_responses_tools_with(tools, supports_strict_mode, false)
}

/// [`convert_responses_tools`] with the synthetic tool-search marker: tools
/// declared by a `tool_search_output` item carry `defer_loading: true`
/// (upstream `ConvertResponsesToolsOptions.toolSearchResult`).
pub fn convert_responses_tools_with(
    tools: &[Tool],
    supports_strict_mode: bool,
    tool_search_result: bool,
) -> AiResult<Vec<Value>> {
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = constrained_sampling::resolve_json_schema_strict_sampling(
            tool,
            supports_strict_mode,
            None,
        )?
        .unwrap_or(false);
        let parameters = constrained_sampling::get_json_schema_tool_parameters(tool, strict)?;
        let mut function = json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": parameters,
        });
        if supports_strict_mode {
            function["strict"] = json!(strict);
        }
        if tool_search_result {
            function["defer_loading"] = json!(true);
        }
        converted.push(function);
    }
    Ok(converted)
}

// ---------------------------------------------------------------------------
// Request body (upstream buildParams)
// ---------------------------------------------------------------------------

/// Whether the credential is a Sign in with ChatGPT access token (upstream
/// `isChatGPTSignIn`): a non-`sk-` token sent directly to OpenAI.
pub fn is_chatgpt_sign_in(model: &Model, api_key: Option<&str>) -> bool {
    model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && api_key.is_some_and(|key| !key.starts_with("sk-"))
}

/// Build the `/responses` request body shared by the OpenAI and Azure
/// variants (upstream `buildParams` in `openai-responses.ts` /
/// `azure-openai-responses.ts`). `model_field` is the wire `model` value
/// (model id for OpenAI, deployment name for Azure). Public for tests/.
///
/// Gated with `provider-openai`: it is the one piece of this module that
/// needs the OpenAI adapter's sampling resolvers and the Responses compat
/// knobs, and only the gated Responses providers call it. The rest of the
/// codec (message/tool conversion, the SSE parser, `parse_partial_json`)
/// stays core so other adapters and the TUI keep compiling without the
/// feature.
#[cfg(feature = "provider-openai")]
// Mirrors the upstream `buildParams` parameter list; public for tests/.
#[allow(clippy::too_many_arguments)]
pub fn build_responses_params(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &responses_compat::ResponsesCompat,
    model_field: &str,
    allowed_tool_call_providers: &[&str],
    omit_unsupported_fields: bool,
    configuration_update_effort: Option<&str>,
) -> AiResult<Value> {
    let message_options = ConvertResponsesMessagesOptions {
        include_system_prompt: true,
        supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
        supports_additional_tools: compat.supports_additional_tools,
        supports_tool_search: compat.supports_tool_search,
        supports_strict_mode: compat.supports_strict_mode,
        supports_developer_role: Some(compat.supports_developer_role),
        configuration_update_effort: configuration_update_effort.map(str::to_string),
    };
    let messages = convert_responses_messages(
        model,
        context,
        allowed_tool_call_providers,
        &message_options,
    );

    let mut params = json!({
        "model": model_field,
        "input": messages,
        "stream": true,
        "store": false,
    });
    // Cache retention: "long" maps to the 24h retention marker; "none" drops
    // the session cache key entirely. Resolved upstream-style
    // (`resolveCacheRetention`, openai-responses.ts:72-76): explicit request
    // value first, then `PI_CACHE_RETENTION=long`, then `short`.
    let cache_retention = crate::ai::api::resolve_cache_retention(options);
    if cache_retention != CacheRetention::None {
        if let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref()) {
            params["prompt_cache_key"] = json!(key);
        }
    }
    if !omit_unsupported_fields
        && compat.supports_prompt_cache_retention
        && cache_retention == CacheRetention::Long
        && compat.supports_long_cache_retention
        // Explicit prompt-cache-mode models take `prompt_cache_options.ttl`
        // instead of `prompt_cache_retention` (upstream
        // `getPromptCacheRetention`).
        && !compat.supports_explicit_prompt_cache_mode
    {
        params["prompt_cache_retention"] = json!("24h");
    }
    // Explicit prompt-cache mode (upstream `getPromptCacheOptions`): "none"
    // pins the cache off, "long" asks for a 30m ttl when supported.
    if !omit_unsupported_fields && compat.supports_explicit_prompt_cache_mode {
        match cache_retention {
            CacheRetention::None => {
                params["prompt_cache_options"] = json!({"mode": "explicit"});
            }
            CacheRetention::Long if compat.supports_long_cache_retention => {
                params["prompt_cache_options"] = json!({"ttl": "30m"});
            }
            _ => {}
        }
    }
    if let Some(max_tokens) = options.max_tokens {
        if compat.supports_max_output_tokens && !omit_unsupported_fields {
            params["max_output_tokens"] = json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS));
        }
    }
    if let Some(temperature) = options.temperature {
        if !omit_unsupported_fields {
            params["temperature"] = json!(temperature);
        }
    }
    // In-place tool changes (upstream `resolveTranscriptTools`): in `changes`
    // mode every tool is declared by an `additional_tools` item, in
    // `additions` mode the top-level list freezes at the last non-additive
    // change, and in `none` mode it holds the current tools. Rebuilding it
    // from `context.tools` every request would rewrite the cached tools
    // prefix whenever the loadout changes.
    let request_tools = resolve_responses_request_tools(
        context,
        compat.supports_mid_convo_system_messages,
        compat.supports_additional_tools,
        compat.supports_tool_search,
    );
    if !request_tools.is_empty() {
        params["tools"] = Value::Array(convert_responses_tools(
            &request_tools,
            compat.supports_strict_mode,
        )?);
    }
    // Requested tool choice (upstream `openai-responses.ts:148`): the typed
    // choice maps to the Responses API's string/function shape.
    if let Some(tool_choice) = options.tool_choice.as_ref() {
        if !omit_unsupported_fields {
            params["tool_choice"] = tool_choice.openai_responses_json();
        }
    }
    // Requested service tier (upstream buildParams:348-349); ChatGPT sign-in
    // tokens reject the field like the other omitted fields.
    if let Some(tier) = &options.service_tier {
        if !omit_unsupported_fields {
            params["service_tier"] = json!(tier);
        }
    }

    // Reasoning emission (upstream buildParams reasoning branch,
    // openai-responses.ts:363-380). pir models the requested level as
    // `options.reasoning`. xai always carries the encrypted-reasoning include,
    // even with no requested level; github-copilot rejects the reasoning field
    // entirely when the effort is off.
    if model.reasoning {
        if model.provider == "xai" {
            params["include"] = json!(["reasoning.encrypted_content"]);
        }
        let requested = options
            .reasoning
            .filter(|level| !matches!(level, ThinkingLevel::Off));
        if let Some(level) = requested {
            // With configuration_update items the request effort stays fixed
            // so the cached prefix does not change; the level rides in the
            // input items (upstream `CONFIGURATION_UPDATE_REQUEST_EFFORT`).
            let effort = if configuration_update_effort.is_some() {
                CONFIGURATION_UPDATE_REQUEST_EFFORT.to_string()
            } else {
                responses_effort_for_level(model, level)
            };
            params["reasoning"] = json!({
                "effort": effort,
                "summary": options
                    .reasoning_summary
                    .map(|summary| summary.as_str())
                    .unwrap_or("auto"),
            });
            if model.provider != "xai" {
                params["include"] = json!(["reasoning.encrypted_content"]);
            }
        } else if !super::copilot::is_github_copilot_model(model)
            && off_map_entry_is_not_null(model)
        {
            let effort = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get("off"))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "none".to_string());
            params["reasoning"] = json!({"effort": effort});
        }
    }

    // Last so model and request sampling parameters override named fields.
    let reasoning_level = options.reasoning.unwrap_or(ThinkingLevel::Off);
    if let Some(sampling_params) = resolve_sampling_params(
        model,
        Some(reasoning_level),
        options.sampling_params.as_ref(),
    ) {
        for (name, value) in sampling_params {
            params[name] = value;
        }
    }
    Ok(params)
}

/// Upstream `model.thinkingLevelMap?.off !== null`: true when the map is
/// absent or holds a non-null "off" entry. Gated with `provider-openai`: its
/// only callers are the gated Responses request builders.
#[cfg(feature = "provider-openai")]
pub(crate) fn off_map_entry_is_not_null(model: &Model) -> bool {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get("off"))
        .map(|entry| entry.is_some())
        .unwrap_or(true)
}

/// The cost multiplier for a service tier (upstream `getServiceTierCostMultiplier`):
/// `flex` bills half, `priority`/`fast` double (2.5x for `gpt-5.5`).
pub fn service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") | Some("fast") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// Reprice a usage record for its service tier (upstream
/// `applyServiceTierPricing`): the default multiplier is a no-op.
pub fn apply_service_tier_pricing(model_id: &str, usage: &mut Usage, service_tier: Option<&str>) {
    let multiplier = service_tier_cost_multiplier(model_id, service_tier);
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

// ---------------------------------------------------------------------------
// Stream processing (upstream processResponsesStream)
// ---------------------------------------------------------------------------

/// One open output slot keyed by the event `output_index` (JS `Map` keys can
/// be `undefined`, so the key is optional).
#[derive(Clone)]
struct Slot {
    kind: SlotKind,
    content_index: usize,
    /// Accumulated function-call arguments JSON (upstream `partialJson`).
    partial_json: Option<String>,
    /// Bounded-cadence refresh of the parsed arguments mirrored into the
    /// block (`partial_json` stays the accumulation source of truth).
    mirror: crate::ai::api::partial_mirror::MirrorCadence,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
}

/// Incremental Responses SSE parser. Both the batch path
/// ([`parse_responses_sse`]) and the streaming path drive the same state
/// machine. Public for tests/.
pub struct ResponsesSseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    /// Accumulated message behind an `Arc`, mutated copy-on-write
    /// (`Arc::make_mut(&mut self.message)`) so partial snapshots are O(1)
    /// hand-outs and a delta nobody snapshotted is applied in place — instead
    /// of the previous deep clone of the whole accumulated message per delta.
    message: Arc<AssistantMessage>,
    slots: HashMap<Option<usize>, Slot>,
    /// reasoning item id -> content index, for encrypted-content backfill.
    reasoning_blocks_by_id: HashMap<String, usize>,
    saw_terminal_response_event: bool,
    /// Codex wire deltas: `response.done`/`response.completed`/
    /// `response.incomplete` normalize to `response.completed`, `error` and
    /// `response.failed` carry Codex error payloads (upstream
    /// `mapCodexEvents`).
    pub codex_mode: bool,
    finish: StopReason,
    /// The model whose rates price the usage when the response completed
    /// under a non-default service tier (upstream `applyServiceTierPricing`
    /// runs inside the usage parse; pir's registry only prices messages
    /// whose cost is still zero).
    model: Model,
    /// The requested service tier (upstream `options.serviceTier`); the
    /// response's own tier reconciles over it at completion.
    requested_service_tier: Option<String>,
    /// Attached upstream `onProviderStreamEvent` observer (each parsed
    /// provider SSE event before pir normalization; read-only).
    provider_event_hook: Option<crate::ai::types::StreamHook>,
}

impl ResponsesSseParser {
    pub fn new(model: &Model) -> Self {
        Self {
            buffer: Vec::new(),
            data_lines: Vec::new(),
            message: Arc::new(base_message(model)),
            slots: HashMap::new(),
            reasoning_blocks_by_id: HashMap::new(),
            saw_terminal_response_event: false,
            codex_mode: false,
            finish: StopReason::Stop,
            model: model.clone(),
            requested_service_tier: None,
            provider_event_hook: None,
        }
    }

    /// Record the requested service tier (upstream `options.serviceTier`).
    pub fn with_service_tier(mut self, tier: Option<String>) -> Self {
        self.requested_service_tier = tier;
        self
    }

    /// Record the exact provider-native effort this reply was generated with,
    /// so later requests can replay it as a `configuration_update` item
    /// (upstream `output.providerThinkingLevel`). Only set when the compat path
    /// is active.
    pub fn with_provider_thinking_level(mut self, effort: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.message).provider_thinking_level = Some(effort.into());
        self
    }

    /// Attach the upstream `onProviderStreamEvent` observer (each parsed
    /// provider SSE event before pir normalization). Public for tests/.
    pub fn with_provider_event_hook(mut self, hook: Option<crate::ai::types::StreamHook>) -> Self {
        self.provider_event_hook = hook;
        self
    }

    /// Snapshot of the message accumulated so far (e.g. for the Start event).
    ///
    /// O(1): hands out a clone of the accumulator's `Arc`; every later
    /// mutation copies-on-write, so the handed-out value stays the snapshot
    /// taken here.
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::clone(&self.message)
    }

    pub fn feed(&mut self, chunk: &[u8], events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result =
            crate::ai::api::sse_feed_lines(chunk, &mut buffer, |line| self.feed_line(line, events));
        self.buffer = buffer;
        result
    }

    pub fn feed_line(
        &mut self,
        line: &str,
        events: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if line.is_empty() {
            return self.flush_event(events);
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines.push(rest.trim().to_string());
        }
        Ok(())
    }

    fn flush_event(&mut self, events: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        if self.data_lines.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data_lines).join("\n");
        self.handle_data(&data, events)
    }

    /// Map one SSE data payload (a `ResponseStreamEvent`) to events.
    pub fn handle_data(
        &mut self,
        data: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)?;
        // Upstream `options.onProviderStreamEvent` fires for every parsed
        // provider event before normalization (openai-responses-shared.ts:600,
        // openai-codex-responses.ts:669).
        if let Some(hook) = &self.provider_event_hook {
            hook(&value);
        }
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if self.codex_mode {
            match event_type {
                "error" => {
                    let code = codex_error_field(&value, "code");
                    let message = codex_error_field(&value, "message");
                    let text = format!(
                        "Codex error: {}",
                        message.unwrap_or_else(|| code.unwrap_or("unknown error"))
                    );
                    return Err(crate::ai::api::provider_error_frame(text, None, code, None));
                }
                "response.failed" => {
                    let response_error = value
                        .get("response")
                        .and_then(|response| response.get("error"));
                    let message = response_error
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str);
                    let code = response_error
                        .and_then(|error| error.get("code"))
                        .and_then(Value::as_str);
                    let text = message.unwrap_or("Codex response failed").to_string();
                    return Err(crate::ai::api::provider_error_frame(text, None, code, None));
                }
                "response.done" | "response.completed" | "response.incomplete" => {
                    let mut event = value;
                    if let Some(response) = event.get_mut("response") {
                        if let Some(end_turn) = response.get("end_turn").and_then(Value::as_bool) {
                            Arc::make_mut(&mut self.message).end_turn = Some(end_turn);
                        }
                        // Normalize unknown statuses to absent so the shared
                        // stop mapping treats them as "completed".
                        let status =
                            response
                                .get("status")
                                .and_then(Value::as_str)
                                .filter(|status| {
                                    matches!(
                                        *status,
                                        "completed"
                                            | "incomplete"
                                            | "failed"
                                            | "cancelled"
                                            | "queued"
                                            | "in_progress"
                                    )
                                });
                        if let Some(status) = status {
                            response["status"] = json!(status);
                        }
                    }
                    event["type"] = json!("response.completed");
                    return self.handle_response_event(&event, out);
                }
                _ => {}
            }
        }
        self.handle_response_event(&value, out)
    }

    fn handle_response_event(
        &mut self,
        value: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.created" => {
                if let Some(id) = value
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                {
                    Arc::make_mut(&mut self.message).response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let output_index = value.get("output_index").and_then(Value::as_u64);
                if let Some(item) = value.get("item") {
                    self.create_slot(output_index.map(|index| index as usize), item, out)?;
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize);
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    if let Some(slot) = self.slot_of(output_index, SlotKind::Thinking) {
                        self.push_thinking_delta(slot.content_index, delta, out)?;
                    }
                }
            }
            "response.reasoning_summary_part.done" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize);
                if let Some(slot) = self.slot_of(output_index, SlotKind::Thinking) {
                    self.push_thinking_delta(slot.content_index, "\n\n", out)?;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize);
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    if let Some(slot) = self.slot_of(output_index, SlotKind::Text) {
                        let index = slot.content_index;
                        if let Some(ContentBlock::Text(text)) =
                            Arc::make_mut(&mut self.message).content.get_mut(index)
                        {
                            text.text.push_str(delta);
                        }
                        out.push(AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta: delta.to_string(),
                            partial: Arc::clone(&self.message),
                        });
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize);
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    let key = self.slot_key_for(output_index);
                    let Some(content_index) = self
                        .slots
                        .get(&key)
                        .filter(|slot| slot.kind == SlotKind::ToolCall)
                        .map(|slot| slot.content_index)
                    else {
                        return Ok(());
                    };
                    // Accumulate in place: cloning the accumulated string per
                    // delta was an O(args) copy on top of the parse.
                    let refresh = {
                        let Some(slot) = self.slots.get_mut(&key) else {
                            return Ok(());
                        };
                        let Some(partial_json) = slot.partial_json.as_mut() else {
                            return Ok(());
                        };
                        partial_json.push_str(delta);
                        slot.mirror.record(delta)
                    };
                    // Upstream parses the accumulated partial JSON on every
                    // delta; the mirror is refreshed at a bounded cadence
                    // instead (first fragment, then every 8 KiB). The final
                    // arguments come from the complete raw buffer.
                    if refresh {
                        let partial_json = self
                            .slots
                            .get(&key)
                            .and_then(|slot| slot.partial_json.as_deref())
                            .unwrap_or_default();
                        let arguments = parse_partial_json(partial_json);
                        self.set_tool_arguments(content_index, arguments);
                    }
                    out.push(AssistantMessageEvent::ToolCallDelta {
                        content_index,
                        delta: delta.to_string(),
                        partial: Arc::clone(&self.message),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize);
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    let Some((content_index, Some(partial_json))) =
                        self.tool_call_slot_data(output_index)
                    else {
                        return Ok(());
                    };
                    let delta = arguments
                        .strip_prefix(partial_json.as_str())
                        .map(str::to_string);
                    if let Some(slot) = self.slots.get_mut(&output_index) {
                        slot.partial_json = Some(arguments.to_string());
                    }
                    let parsed = parse_partial_json(arguments);
                    self.set_tool_arguments(content_index, parsed);
                    if let Some(delta) = delta {
                        if !delta.is_empty() {
                            out.push(AssistantMessageEvent::ToolCallDelta {
                                content_index,
                                delta,
                                partial: Arc::clone(&self.message),
                            });
                        }
                    }
                }
            }
            "response.output_item.done" => {
                self.handle_item_done(value, out)?;
            }
            "response.completed" | "response.incomplete" => {
                self.finalize_response(value)?;
            }
            "error" => {
                let text = responses_error_text(value);
                return Err(crate::ai::api::provider_error_frame_from_value(value, text));
            }
            "response.failed" => {
                self.saw_terminal_response_event = true;
                let response_error = value
                    .get("response")
                    .and_then(|response| response.get("error"));
                let message = response_error
                    .map(|error| {
                        let code = error
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("no message");
                        format!("{code}: {message}")
                    })
                    .or_else(|| {
                        value
                            .get("response")
                            .and_then(|response| response.get("incomplete_details"))
                            .and_then(|details| details.get("reason"))
                            .and_then(Value::as_str)
                            .map(|reason| format!("incomplete: {reason}"))
                    })
                    .unwrap_or_else(|| "Unknown error (no error details in response)".to_string());
                return Err(match response_error {
                    Some(error) => crate::ai::api::provider_error_frame_from_value(error, message),
                    None => crate::ai::api::provider_error_frame(message, None, None, None),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_item_done(
        &mut self,
        value: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let output_index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .map(|i| i as usize);
        let Some(item) = value.get("item") else {
            return Ok(());
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        // A message item with phase final_answer ends the turn (upstream
        // `applyMessagePhaseStopReason`).
        if item_type == "message"
            && item.get("phase").and_then(Value::as_str) == Some("final_answer")
        {
            self.finish = StopReason::Stop;
        }
        // Ensure a slot exists (upstream `getOrCreateSlot`).
        let key = self.slot_key_for(output_index);
        if !self.slots.contains_key(&key) {
            self.create_slot(output_index, item, out)?;
        }
        let Some(slot) = self.slots.get(&key).cloned() else {
            return Ok(());
        };
        match item_type {
            "reasoning" if slot.kind == SlotKind::Thinking => {
                let summary_text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                let content_text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                let thinking_text = if !summary_text.is_empty() {
                    summary_text
                } else if !content_text.is_empty() {
                    content_text
                } else {
                    self.current_thinking(slot.content_index)
                };
                if let Some(ContentBlock::Thinking(thinking)) = Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(slot.content_index)
                {
                    thinking.thinking = thinking_text.clone();
                    thinking.thinking_signature = Some(item.to_string());
                }
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    self.reasoning_blocks_by_id
                        .insert(id.to_string(), slot.content_index);
                }
                out.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: slot.content_index,
                    content: thinking_text,
                    partial: Arc::clone(&self.message),
                });
                self.slots.remove(&key);
            }
            "message" if slot.kind == SlotKind::Text => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| {
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .or_else(|| part.get("refusal").and_then(Value::as_str))
                                    .unwrap_or("")
                            })
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                if let Some(ContentBlock::Text(block)) = Arc::make_mut(&mut self.message)
                    .content
                    .get_mut(slot.content_index)
                {
                    block.text = text.clone();
                    let phase = item.get("phase").and_then(Value::as_str);
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        block.text_signature = Some(encode_text_signature_v1(id, phase));
                    }
                }
                out.push(AssistantMessageEvent::TextEnd {
                    content_index: slot.content_index,
                    content: text,
                    partial: Arc::clone(&self.message),
                });
                self.slots.remove(&key);
            }
            "function_call" if slot.kind == SlotKind::ToolCall => {
                // Upstream `item.arguments || slot.block.partialJson`: an
                // empty final field falls back to the accumulated deltas.
                let arguments_json = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|arguments| !arguments.is_empty())
                    .map(str::to_string)
                    .or_else(|| slot.partial_json.clone())
                    .unwrap_or_else(|| "{}".to_string());
                // A finished call's arguments are parsed strictly (upstream
                // `parseToolCallArguments`, #10944): keeping the parsed prefix
                // of a malformed document would run the tool with wrong input.
                // A malformed document keeps the call with `arguments: {}`
                // plus `argumentsError`; the stream still completes.
                let parsed = crate::ai::api::tool_args::parse_final_tool_arguments(&arguments_json);
                let namespace = item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.finalize_tool_call(
                    slot.content_index,
                    parsed.arguments,
                    parsed.error,
                    namespace,
                    out,
                )?;
                self.slots.remove(&key);
            }
            "custom_tool_call" if slot.kind == SlotKind::ToolCall => {
                // pir does not model grammar (custom) tools; finalize the
                // in-flight call with the raw input string as its arguments.
                let input = item
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut arguments = BTreeMap::new();
                arguments.insert("input".to_string(), json!(input));
                let namespace = item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.finalize_tool_call(slot.content_index, arguments, None, namespace, out)?;
                self.slots.remove(&key);
            }
            _ => {}
        }
        Ok(())
    }

    fn finalize_tool_call(
        &mut self,
        content_index: usize,
        arguments: BTreeMap<String, Value>,
        arguments_error: Option<String>,
        namespace: Option<String>,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let Some(ContentBlock::ToolCall(call)) = Arc::make_mut(&mut self.message)
            .content
            .get_mut(content_index)
        else {
            return Ok(());
        };
        call.arguments = arguments;
        call.arguments_error = arguments_error;
        if let Some(namespace) = namespace {
            call.namespace = Some(namespace);
        }
        let call = call.clone();
        out.push(AssistantMessageEvent::ToolCallEnd {
            content_index,
            tool_call: call,
            partial: Arc::clone(&self.message),
        });
        Ok(())
    }

    fn finalize_response(&mut self, value: &Value) -> AiResult<()> {
        self.saw_terminal_response_event = true;
        let response = value.get("response");
        // Backfill persisted reasoning signatures from the terminal response
        // (Azure can omit reasoning.encrypted_content from output_item.done;
        // upstream issue 6409).
        if let Some(output) = response.and_then(|response| response.get("output")) {
            if let Some(items) = output.as_array() {
                for item in items {
                    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                        continue;
                    }
                    let encrypted = item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    if encrypted.is_empty() || id.is_empty() {
                        continue;
                    }
                    if let Some(content_index) = self.reasoning_blocks_by_id.get(id) {
                        if let Some(ContentBlock::Thinking(thinking)) =
                            Arc::make_mut(&mut self.message)
                                .content
                                .get_mut(*content_index)
                        {
                            if let Some(signature) = &thinking.thinking_signature {
                                if let Ok(mut stored) = serde_json::from_str::<Value>(signature) {
                                    if stored.get("encrypted_content").is_none() {
                                        stored["encrypted_content"] = json!(encrypted);
                                        thinking.thinking_signature = Some(stored.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(id) = response
            .and_then(|response| response.get("id"))
            .and_then(Value::as_str)
        {
            Arc::make_mut(&mut self.message).response_id = Some(id.to_string());
        }
        if let Some(usage) = response.and_then(|response| response.get("usage")) {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cached = usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_write = usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cache_write_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let reasoning = usage
                .get("output_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Arc::make_mut(&mut self.message).usage = Usage {
                // OpenAI includes cached and cache-write tokens in
                // input_tokens, so subtract both.
                input: input_tokens
                    .saturating_sub(cached)
                    .saturating_sub(cache_write),
                output: output_tokens,
                cache_read: cached,
                cache_write,
                reasoning: (reasoning > 0).then_some(reasoning),
                total_tokens: usage
                    .get("total_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(input_tokens + output_tokens),
                ..Usage::default()
            };
            // Service-tier reconciliation (upstream openai-responses.ts
            // resolveServiceTier + applyServiceTierPricing): the response's
            // own tier wins over the requested one, and flex/priority/fast
            // tiers reprice the usage at the tier multiplier. Computing the
            // cost here stops the registry's generic cost pass from running.
            let effective_tier = response
                .and_then(|response| response.get("service_tier"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| self.requested_service_tier.clone());
            if effective_tier.is_some() {
                let message = Arc::make_mut(&mut self.message);
                calculate_cost(&self.model, &mut message.usage);
                apply_service_tier_pricing(
                    &self.model.id,
                    &mut message.usage,
                    effective_tier.as_deref(),
                );
            }
        }
        let status = response
            .and_then(|response| response.get("status"))
            .and_then(Value::as_str);
        let incomplete_reason = response
            .and_then(|response| response.get("incomplete_details"))
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str);
        let message = Arc::make_mut(&mut self.message);
        message.raw_stop_reason = Some(match incomplete_reason {
            Some(reason) => format!("{}.{reason}", status.unwrap_or("")),
            None => status.unwrap_or("").to_string(),
        });
        let (stop, error_message) = map_stop_reason(status, incomplete_reason);
        self.finish = stop;
        message.error_message = error_message;
        let has_tool_call = self
            .message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        if has_tool_call && self.finish == StopReason::Stop {
            self.finish = StopReason::ToolUse;
        }
        Ok(())
    }

    /// Flush a trailing event block, run the end-of-stream checks (terminal
    /// event seen, no unfinished tool calls) and emit the `Done` event.
    pub fn finish(&mut self, out: &mut Vec<AssistantMessageEvent>) -> AiResult<()> {
        self.flush_event(out)
            .map_err(crate::ai::api::trailing_event_error)?;
        if !self.saw_terminal_response_event {
            // A clean EOF before `response.completed`/`response.failed` is a
            // truncated stream (the transport read a complete body, so no
            // HTTP verdict applies): retryable at the agent level.
            return Err(AiError::StreamTruncated(
                "OpenAI Responses stream ended before a terminal response event".to_string(),
            ));
        }
        // The agent runs every tool call in the final message; refuse calls
        // whose output_item.done never arrived.
        if self.finish == StopReason::ToolUse {
            let unfinished: Vec<String> = self
                .slots
                .values()
                .filter(|slot| slot.kind == SlotKind::ToolCall)
                .filter_map(|slot| match self.message.content.get(slot.content_index) {
                    Some(ContentBlock::ToolCall(call)) => {
                        Some(format!("{} ({})", call.name, call.id))
                    }
                    _ => None,
                })
                .collect();
            if !unfinished.is_empty() {
                return Err(AiError::NonRetryable(format!(
                    "OpenAI Responses stream completed with an unfinished tool call: {}",
                    unfinished.join(", ")
                )));
            }
        }
        if self.finish == StopReason::Error {
            // A failed/cancelled provider response is a terminal error, not a
            // successful Done event. Other adapters already surface this
            // branch; encoding it as `Done { reason: Stop }` would make the
            // agent retry/continue against a failed response.
            let detail = self
                .message
                .error_message
                .clone()
                .or_else(|| self.message.raw_stop_reason.clone())
                .unwrap_or_else(|| "Provider returned an error stop reason".to_owned());
            return Err(AiError::NonRetryable(detail));
        }
        // The end events above may still hold the previous snapshot, so this
        // finalization copies-on-write once; the terminal event then reclaims
        // the accumulator without another copy.
        Arc::make_mut(&mut self.message).stop_reason = self.finish;
        let terminal = match self.finish {
            StopReason::ToolUse => TerminalReason::ToolUse,
            StopReason::Length => TerminalReason::Length,
            _ => TerminalReason::Stop,
        };
        let message = std::mem::replace(&mut self.message, Arc::new(AssistantMessage::default()));
        let message = match Arc::try_unwrap(message) {
            Ok(message) => message,
            Err(shared) => (*shared).clone(),
        };
        out.push(AssistantMessageEvent::Done {
            reason: terminal,
            message,
        });
        Ok(())
    }

    fn slot_key_for(&self, output_index: Option<usize>) -> Option<usize> {
        output_index
    }

    fn slot_of(&self, output_index: Option<usize>, kind: SlotKind) -> Option<&Slot> {
        let key = self.slot_key_for(output_index);
        self.slots.get(&key).filter(|slot| slot.kind == kind)
    }

    /// Clone the (content index, partial JSON) of the tool-call slot for an
    /// output index, if one exists.
    fn tool_call_slot_data(&self, output_index: Option<usize>) -> Option<(usize, Option<String>)> {
        let slot = self.slot_of(output_index, SlotKind::ToolCall)?;
        Some((slot.content_index, slot.partial_json.clone()))
    }

    fn push_thinking_delta(
        &mut self,
        content_index: usize,
        delta: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        if let Some(ContentBlock::Thinking(thinking)) = Arc::make_mut(&mut self.message)
            .content
            .get_mut(content_index)
        {
            thinking.thinking.push_str(delta);
        }
        out.push(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta: delta.to_string(),
            partial: Arc::clone(&self.message),
        });
        Ok(())
    }

    fn current_thinking(&self, content_index: usize) -> String {
        match self.message.content.get(content_index) {
            Some(ContentBlock::Thinking(thinking)) => thinking.thinking.clone(),
            _ => String::new(),
        }
    }

    fn set_tool_arguments(&mut self, content_index: usize, arguments: Value) {
        if let Some(ContentBlock::ToolCall(call)) = Arc::make_mut(&mut self.message)
            .content
            .get_mut(content_index)
        {
            call.arguments = serde_json::from_value(arguments).unwrap_or_default();
        }
    }

    fn create_slot(
        &mut self,
        output_index: Option<usize>,
        item: &Value,
        out: &mut Vec<AssistantMessageEvent>,
    ) -> AiResult<()> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let key = self.slot_key_for(output_index);
        let content_index = self.message.content.len();
        match item_type {
            "reasoning" => {
                Arc::make_mut(&mut self.message)
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }));
                self.slots.insert(
                    key,
                    Slot {
                        kind: SlotKind::Thinking,
                        content_index,
                        partial_json: None,
                        mirror: crate::ai::api::partial_mirror::MirrorCadence::new(),
                    },
                );
                out.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: Arc::clone(&self.message),
                });
            }
            "message" => {
                if item.get("phase").and_then(Value::as_str) == Some("final_answer") {
                    self.finish = StopReason::Stop;
                }
                Arc::make_mut(&mut self.message)
                    .content
                    .push(ContentBlock::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                self.slots.insert(
                    key,
                    Slot {
                        kind: SlotKind::Text,
                        content_index,
                        partial_json: None,
                        mirror: crate::ai::api::partial_mirror::MirrorCadence::new(),
                    },
                );
                out.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: Arc::clone(&self.message),
                });
            }
            "function_call" | "custom_tool_call" => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = if item_type == "function_call" {
                    item.get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                } else {
                    item.get("input")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                };
                let mut call = ToolCall {
                    id: format!("{call_id}|{item_id}"),
                    name: name.to_string(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments_error: None,
                };
                if item_type == "custom_tool_call" {
                    call.arguments.insert("input".to_string(), json!(arguments));
                }
                Arc::make_mut(&mut self.message)
                    .content
                    .push(ContentBlock::ToolCall(call));
                self.slots.insert(
                    key,
                    Slot {
                        kind: SlotKind::ToolCall,
                        content_index,
                        partial_json: Some(arguments),
                        mirror: crate::ai::api::partial_mirror::MirrorCadence::new(),
                    },
                );
                out.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: Arc::clone(&self.message),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

/// Read a Codex error field from the top level or the nested `error` object
/// (upstream `extractCodexEventError`).
fn codex_error_field<'a>(event: &'a Value, field: &str) -> Option<&'a str> {
    event
        .get(field)
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("error")
                .and_then(|error| error.get(field))
                .and_then(Value::as_str)
        })
        .filter(|value| !value.is_empty())
}

/// Display text for an OpenAI Responses `error` event. Accepts both the
/// top-level `{code, message}` shape and the nested `{"error": {...}}` relay
/// shape; the pre-existing `Error Code {code}: {message}` wording is kept for
/// the top-level shape.
fn responses_error_text(value: &Value) -> String {
    let nested = value.get("error").filter(|error| !error.is_null());
    let code = value.get("code").and_then(Value::as_str).or_else(|| {
        nested
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
    });
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            nested
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .or_else(|| nested.and_then(Value::as_str));
    match (code, message) {
        (Some(code), Some(message)) => format!("Error Code {code}: {message}"),
        (None, Some(message)) => message.to_string(),
        (Some(code), None) => format!("Error Code {code}"),
        (None, None) => "Provider returned an error".to_string(),
    }
}

/// Map a terminal response status to a stop reason (upstream `mapStopReason`).
pub fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> (StopReason, Option<String>) {
    match status {
        None | Some("completed") => (StopReason::Stop, None),
        Some("incomplete") => match incomplete_reason {
            Some("max_output_tokens") => (StopReason::Length, None),
            Some(reason) => (
                StopReason::Error,
                Some(format!("Response incomplete: {reason}")),
            ),
            None => (
                StopReason::Error,
                Some("Response incomplete without a provider reason".to_string()),
            ),
        },
        Some("failed") | Some("cancelled") => (StopReason::Error, None),
        // These two are wonky ...
        Some("in_progress") | Some("queued") => (StopReason::Stop, None),
        // Unknown statuses surface as errors carrying the raw status (upstream
        // throws `Unhandled stop reason: ...` for non-exhaustive values).
        Some(other) => (
            StopReason::Error,
            Some(format!("Unhandled stop reason: {other}")),
        ),
    }
}

/// Parse a full SSE body into events. Public for tests/.
pub fn parse_responses_sse(
    body: &[u8],
    model: &Model,
) -> Result<Vec<AssistantMessageEvent>, AiError> {
    let mut parser = ResponsesSseParser::new(model);
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for data in crate::ai::api::sse_data(body) {
        parser.handle_data(&data, &mut out)?;
    }
    parser.finish(&mut out)?;
    Ok(out)
}

/// Parse a possibly-truncated JSON document by repairing open strings and
/// containers, normalizing invalid string escapes, and completing truncated
/// literals (upstream uses `parseStreamingJson` from the `partial-json`
/// package; the subset needed for tool-call arguments).
pub fn parse_partial_json(input: &str) -> Value {
    let repaired = repair_partial_json(input);
    if let Ok(parsed) = serde_json::from_str::<Value>(&repaired) {
        return parsed;
    }
    // A truncated bareword literal (`tru`, `fals`, `nul`, …) must not collapse
    // to `Null` arguments; complete it before giving up.
    let completed = complete_truncated_literal(&repaired);
    serde_json::from_str(&completed).unwrap_or(Value::Null)
}

/// Append the missing remainder of a trailing bareword that is a prefix of
/// `true` / `false` / `null` (upstream partial-json behavior for cut
/// literals).
fn complete_truncated_literal(input: &str) -> String {
    let trimmed = input.trim_end();
    for (prefix, complete) in [
        ("t", "true"),
        ("tr", "true"),
        ("tru", "true"),
        ("f", "false"),
        ("fa", "false"),
        ("fal", "false"),
        ("fals", "false"),
        ("n", "null"),
        ("nu", "null"),
        ("nul", "null"),
    ] {
        if trimmed.ends_with(prefix) && prefix.len() < trimmed.len() {
            // Only complete when the bareword is a standalone value: the
            // character before it must be structural (not alphanumeric).
            let before = &trimmed[..trimmed.len() - prefix.len()];
            if before
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
            {
                return format!("{before}{complete}{}", &input[trimmed.len()..]);
            }
        }
    }
    input.to_string()
}

/// Escape characters JSON accepts after a backslash (upstream
/// `VALID_JSON_ESCAPES` minus `"u"`, which is handled by the four-hex-digit
/// special case in the loop below).
fn is_valid_json_escape(ch: char) -> bool {
    matches!(ch, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')
}

/// Structural repair (open strings/containers closed, truncated literals
/// completed) plus escape normalization for string literals.
///
/// Upstream splits this between `repairJson` (escape normalization,
/// `packages/ai/src/utils/json-parse.ts`) and the `partial-json` fallback that
/// closes open structures; pir has no partial-json library, so both live here.
/// The escape decisions mirror upstream `repairJson` (D29, `4b994d5e5`):
/// - a valid `\uXXXX` (exactly four hex digits) is copied verbatim and still
///   decodes to its code point;
/// - every other escape character — including `u` not followed by four hex
///   digits (`\usepackage`) — is kept as literal text by doubling the
///   backslash. Only the backslash is consumed; the following character is
///   processed normally (upstream does not consume it either);
/// - a `\u` escape cut off by the end of the input is dropped, not kept
///   literal: upstream's partial-json fallback truncates the half-escape
///   (`parseStreamingJson(r'{"text":"ab\u00')` → `{"text":"ab"}`) and pir has
///   no such fallback, so the truncation happens here.
fn repair_partial_json(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    // Track the trailing bareword token so a truncated literal can be
    // completed below.
    let mut bareword_start: Option<usize> = None;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            if ch == '"' {
                in_string = false;
                bareword_start = None;
                output.push(ch);
                index += 1;
                continue;
            }
            if ch == '\\' {
                let Some(next) = chars.get(index + 1).copied() else {
                    // Trailing backslash at end of input: upstream keeps a
                    // literal backslash (appends `\\`); the unterminated
                    // string is closed below.
                    output.push_str("\\\\");
                    index += 1;
                    continue;
                };
                if next == 'u' {
                    let digits: String = chars[index + 2..].iter().take(4).collect();
                    if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
                        output.push_str("\\u");
                        output.push_str(&digits);
                        index += 6;
                        continue;
                    }
                    let tail = &chars[index + 2..];
                    if tail.len() < 4 && tail.iter().all(char::is_ascii_hexdigit) {
                        // Fewer than four hex digits left: the escape is cut
                        // off by the end of the stream (see doc comment).
                        break;
                    }
                }
                if is_valid_json_escape(next) {
                    output.push('\\');
                    output.push(next);
                    index += 2;
                    continue;
                }
                // Invalid escape (`\u` without four hex digits, `\p`, …):
                // double the backslash so the character survives as literal
                // text, and leave the following character to the next
                // iteration.
                output.push_str("\\\\");
                index += 1;
                continue;
            }
            output.push(ch);
            index += 1;
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                bareword_start = None;
                output.push(ch);
            }
            '{' | '[' => {
                stack.push(ch);
                bareword_start = None;
                output.push(ch);
            }
            '}' | ']' => {
                stack.pop();
                bareword_start = None;
                output.push(ch);
            }
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '+' | '.' => {
                if bareword_start.is_none() {
                    bareword_start = Some(output.len());
                }
                output.push(ch);
            }
            _ => {
                bareword_start = None;
                output.push(ch);
            }
        }
        index += 1;
    }
    // Truncated literal (`{"a": tru`): complete the bareword before closing
    // the open containers so the result parses as real JSON.
    if let (false, Some(start)) = (in_string, bareword_start) {
        let tail = output[start..].to_string();
        for (prefix, complete) in [
            ("t", "true"),
            ("tr", "true"),
            ("tru", "true"),
            ("f", "false"),
            ("fa", "false"),
            ("fal", "false"),
            ("fals", "false"),
            ("n", "null"),
            ("nu", "null"),
            ("nul", "null"),
        ] {
            if tail == prefix {
                output.truncate(start);
                output.push_str(complete);
                break;
            }
        }
    }
    if in_string {
        output.push('"');
    }
    while let Some(open) = stack.pop() {
        output.push(if open == '{' { '}' } else { ']' });
    }
    output
}
