//! Shared request-mapping helpers for the Google Generative AI adapter.
//!
//! Rust equivalent of the upstream `packages/ai/src/api/google-shared.ts`
//! (subset relevant to pir): message → `contents` conversion, thinking-level /
//! thinking-budget resolution, tool declarations, and finish-reason mapping.

use serde_json::{json, Map, Value};

use crate::ai::api::constrained_sampling::{
    get_json_schema_tool_parameters, resolve_json_schema_strict_sampling,
};
use crate::ai::api::transform::transform_messages_lazy_refs;
use crate::ai::error::{AiError, AiResult};
use crate::ai::types::{
    ContentBlock, Context, Message, Model, StopReason, ThinkingBudgets, ThinkingLevel, Tool,
    ToolChoice, ToolResultMessage, UserMessage,
};

// ---------------------------------------------------------------------------
// Model-id probes (upstream google-shared.ts)
// ---------------------------------------------------------------------------

/// Major Gemini version of a model id (`gemini-2.5-flash` → 2). Accepts the
/// `gemini-live-*` naming form; `None` for non-Gemini ids.
pub fn gemini_major_version(model_id: &str) -> Option<u32> {
    let id = model_id.to_lowercase();
    let rest = id.strip_prefix("gemini")?;
    let rest = rest.strip_prefix("-live").unwrap_or(rest);
    let rest = rest.strip_prefix('-')?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    rest[..digits].parse().ok()
}

/// Whether the id matches upstream's `/gemini-3(?:\.\d+)?-(?:pro|flash)/`
/// probe (Gemini 3 Pro/Flash with or without a minor version).
fn gemini3_pro_or_flash(id: &str) -> bool {
    let Some(index) = id.find("gemini-") else {
        return false;
    };
    let Some(rest) = id[index + "gemini-".len()..].strip_prefix('3') else {
        return false;
    };
    let rest = match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits = fraction.chars().take_while(char::is_ascii_digit).count();
            if digits == 0 {
                return false;
            }
            &fraction[digits..]
        }
        None => rest,
    };
    rest.starts_with("-pro") || rest.starts_with("-flash")
}

/// Whether the id matches upstream's `/gemini-(?:[4-9]|[1-9]\d)/` probe
/// (Gemini 4 and later, such as `gemini-4-argon`).
fn gemini4_or_later(id: &str) -> bool {
    let Some(index) = id.find("gemini-") else {
        return false;
    };
    let rest = &id[index + "gemini-".len()..];
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    match digits {
        0 => false,
        // `[4-9]`.
        1 => matches!(rest.as_bytes()[0], b'4'..=b'9'),
        // `[1-9]\d`.
        _ => rest.as_bytes()[0] != b'0',
    }
}

/// Whether this model uses Gemini's discrete `thinkingLevel` control instead
/// of the token-based `thinkingBudget` control (upstream
/// `usesGoogleThinkingLevel`).
pub fn uses_google_thinking_level(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    gemini3_pro_or_flash(&id)
        // Match Gemini 4 and later, such as gemini-4-argon (#10890).
        || gemini4_or_later(&id)
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || id.contains("gemma-4")
        || id.contains("gemma4")
}

/// Models via Google APIs that require explicit tool call IDs in function
/// calls/responses (upstream `requiresToolCallId`).
pub fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

/// Gemini 3+ supports images nested inside `functionResponse.parts`
/// (upstream `supportsMultimodalFunctionResponse`).
fn supports_multimodal_function_response(model_id: &str) -> bool {
    match gemini_major_version(model_id) {
        Some(version) => version >= 3,
        None => true,
    }
}

/// Models via Google APIs that validate required tool parameters
/// (upstream `supportsGoogleStrictToolSampling`).
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

// ---------------------------------------------------------------------------
// Thinking levels & budgets (upstream google-shared.ts + getGoogleBudget)
// ---------------------------------------------------------------------------

/// A standard Google thinking level after resolution
/// (upstream `ResolvedGoogleThinkingLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedGoogleLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ResolvedGoogleLevel {
    /// The uppercase wire value sent in `thinkingConfig.thinkingLevel`
    /// (upstream `toGoogleThinkingLevel`).
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::Minimal => "MINIMAL",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }
}

const LEVEL_ORDER: [ThinkingLevel; 7] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];

/// Whether the model supports a pi thinking level (upstream
/// `getSupportedThinkingLevels`): standard levels are supported unless the
/// `thinkingLevelMap` entry is an explicit `null`; `xhigh`/`max` require a
/// non-null explicit mapping.
fn level_supported(model: &Model, level: ThinkingLevel) -> bool {
    let extended = matches!(level, ThinkingLevel::XHigh | ThinkingLevel::Max);
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(level.wire_key()))
    {
        Some(None) => false,
        Some(Some(_)) => true,
        None => !extended,
    }
}

/// Clamp a requested level to the closest supported one, walking up then down
/// (upstream `clampThinkingLevel`).
pub fn clamp_google_thinking_level(model: &Model, level: ThinkingLevel) -> ThinkingLevel {
    if level_supported(model, level) {
        return level;
    }
    let index = LEVEL_ORDER
        .iter()
        .position(|candidate| *candidate == level)
        .unwrap_or(0);
    for candidate in &LEVEL_ORDER[index..] {
        if level_supported(model, *candidate) {
            return *candidate;
        }
    }
    for candidate in LEVEL_ORDER[..index].iter().rev() {
        if level_supported(model, *candidate) {
            return *candidate;
        }
    }
    ThinkingLevel::Off
}

/// Resolve a supported pi level (or a model-specific Google mapping) to a
/// standard Google level (upstream `resolveGoogleThinkingLevel`).
pub fn resolve_google_thinking_level(
    model: &Model,
    level: ThinkingLevel,
) -> AiResult<ResolvedGoogleLevel> {
    let key = level.wire_key();
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(key))
        .cloned()
        .flatten();
    let resolved = match &mapped {
        Some(value) => value.to_lowercase(),
        None => key.to_string(),
    };
    match resolved.as_str() {
        "minimal" => Ok(ResolvedGoogleLevel::Minimal),
        "low" => Ok(ResolvedGoogleLevel::Low),
        "medium" => Ok(ResolvedGoogleLevel::Medium),
        "high" => Ok(ResolvedGoogleLevel::High),
        _ => {
            // Upstream prints `String(mapped)`: "undefined" when absent, the
            // raw value otherwise.
            let display = mapped
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "undefined".into());
            Err(AiError::NonRetryable(format!(
                "Unsupported Google thinking level mapping for {}/{}: {} -> {}",
                model.provider, model.id, key, display
            )))
        }
    }
}

/// Thinking-token budget for a (non-`thinkingLevel`) model at a given level
/// (upstream `getGoogleBudget`); `-1` means dynamic thinking.
pub fn google_thinking_budget(model_id: &str, level: ResolvedGoogleLevel) -> i64 {
    google_thinking_budget_with(model_id, level, None, false)
}

/// [`google_thinking_budget`] with the per-request `thinkingBudgets` override
/// table and the Vertex table selection (upstream `getGoogleBudget`):
/// `thinkingBudgets[level]` wins; the Vertex table has no separate
/// `2.5-flash-lite` row (that id falls into its `2.5-flash` row), while the
/// Generative AI table does.
pub fn google_thinking_budget_with(
    model_id: &str,
    level: ResolvedGoogleLevel,
    budgets: Option<&ThinkingBudgets>,
    vertex: bool,
) -> i64 {
    let overridden = budgets.and_then(|budgets| match level {
        ResolvedGoogleLevel::Minimal => budgets.minimal,
        ResolvedGoogleLevel::Low => budgets.low,
        ResolvedGoogleLevel::Medium => budgets.medium,
        ResolvedGoogleLevel::High => budgets.high,
    });
    if let Some(overridden) = overridden {
        return overridden as i64;
    }
    let id = model_id.to_lowercase();
    // [minimal, low, medium, high]
    let table: [i64; 4] = if id.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if id.contains("2.5-flash-lite") && !vertex {
        [512, 2048, 8192, 24576]
    } else if id.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return -1;
    };
    match level {
        ResolvedGoogleLevel::Minimal => table[0],
        ResolvedGoogleLevel::Low => table[1],
        ResolvedGoogleLevel::Medium => table[2],
        ResolvedGoogleLevel::High => table[3],
    }
}

/// Thinking config for a disabled request (upstream
/// `getDisabledGoogleThinkingConfig`): budget-based models get
/// `{"thinkingBudget": 0}`; `thinkingLevel` models get the lowest mapped
/// supported level, or fall back to `{"thinkingBudget": 0}` when "off" itself
/// is supported.
pub fn disabled_google_thinking_config(model: &Model) -> AiResult<Value> {
    if !uses_google_thinking_level(&model.id) {
        return Ok(json!({"thinkingBudget": 0}));
    }
    let fallback = clamp_google_thinking_level(model, ThinkingLevel::Off);
    if fallback == ThinkingLevel::Off {
        return Ok(json!({"thinkingBudget": 0}));
    }
    let resolved = resolve_google_thinking_level(model, fallback)?;
    Ok(json!({"thinkingLevel": resolved.wire_name()}))
}

/// Build `generationConfig.thinkingConfig` for a request (upstream
/// `buildParams` thinking branch + `streamSimple` level/budget dispatch).
/// Reasoning omitted (or clamping to `off`) disables thinking via
/// `getDisabledGoogleThinkingConfig`; a requested level enables thinking —
/// Gemini 3-family models use the discrete `thinkingLevel` control,
/// everything else the token-budget control.
pub fn google_thinking_config(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
) -> AiResult<Option<Value>> {
    google_thinking_config_with(model, reasoning, None, false)
}

/// [`google_thinking_config`] with the per-request `thinkingBudgets` overrides
/// (upstream `getGoogleBudget(model, level, options.thinkingBudgets)`) and the
/// Vertex table selection.
pub fn google_thinking_config_with(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    budgets: Option<&ThinkingBudgets>,
    vertex: bool,
) -> AiResult<Option<Value>> {
    if !model.reasoning {
        return Ok(None);
    }
    // Upstream streamSimple: omitted reasoning always disables; a requested
    // level is clamped first and disables only when the clamp lands on "off".
    let requested = reasoning.and_then(|level| {
        let clamped = clamp_google_thinking_level(model, level);
        (clamped != ThinkingLevel::Off).then_some(clamped)
    });
    let Some(clamped) = requested else {
        return Ok(Some(disabled_google_thinking_config(model)?));
    };
    let resolved = resolve_google_thinking_level(model, clamped)?;
    if uses_google_thinking_level(&model.id) {
        Ok(Some(json!({
            "includeThoughts": true,
            "thinkingLevel": resolved.wire_name(),
        })))
    } else {
        Ok(Some(json!({
            "includeThoughts": true,
            "thinkingBudget": google_thinking_budget_with(&model.id, resolved, budgets, vertex),
        })))
    }
}

// ---------------------------------------------------------------------------
// Finish reasons (upstream google-shared.ts mapStopReason)
// ---------------------------------------------------------------------------

/// Map a raw Gemini `finishReason` to a pir stop reason. Upstream's exhaustive
/// enum maps every value other than `STOP`/`MAX_TOKENS` (SAFETY, RECITATION,
/// BLOCKLIST, PROHIBITED_CONTENT, SPII, LANGUAGE, MALFORMED_FUNCTION_CALL, …)
/// to `error`; the string form keeps the raw reason on the message.
pub fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

// ---------------------------------------------------------------------------
// Thought signatures (upstream google-shared.ts)
// ---------------------------------------------------------------------------

/// `thought: true` is the definitive marker for thinking content; a
/// `thoughtSignature` alone does not make a part thinking content
/// (upstream `isThinkingPart`).
pub fn is_thinking_part(part: &Value) -> bool {
    part.get("thought").and_then(Value::as_bool) == Some(true)
}

/// Retain the last non-empty signature for a streamed block; later deltas may
/// omit it (upstream `retainThoughtSignature`).
pub fn retain_thought_signature(existing: Option<&str>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(value) if !value.is_empty() => Some(value.to_string()),
        _ => existing.map(str::to_string),
    }
}

/// Thought signatures must be base64 for Google APIs (TYPE_BYTES).
fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() || !signature.len().is_multiple_of(4) {
        return false;
    }
    let body = signature.trim_end_matches('=');
    let padding = signature.len() - body.len();
    if padding > 2 || body.is_empty() {
        return false;
    }
    body.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
}

/// Only keep signatures from the same provider/model and with valid base64
/// (upstream `resolveThoughtSignature`).
pub fn resolve_thought_signature(
    same_provider_and_model: bool,
    signature: Option<&str>,
) -> Option<String> {
    match signature {
        Some(signature) if same_provider_and_model && is_valid_thought_signature(signature) => {
            Some(signature.to_string())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Message conversion (upstream google-shared.ts convertMessages)
// ---------------------------------------------------------------------------

/// Normalize a tool call id for models that require explicit ids
/// (upstream: replace non `[a-zA-Z0-9_-]` with `_`, cap at 64 chars).
fn normalize_tool_call_id(model_id: &str, id: &str) -> String {
    if !requires_tool_call_id(model_id) {
        return id.to_string();
    }
    crate::ai::api::transform::sanitize_id_part(id)
        .chars()
        .take(64)
        .collect()
}

fn is_same_provider_and_model(model: &Model, message: &crate::ai::types::AssistantMessage) -> bool {
    message.provider == model.provider && message.model == model.id
}

/// Convert pir messages into Gemini `Content[]` (upstream `convertMessages`).
/// [`transform_messages_lazy`](crate::ai::api::transform::transform_messages_lazy)
/// runs first (errored/aborted assistant skip, synthetic tool results,
/// cross-model thinking→text lowering, tool-call id normalization, image
/// downgrade); untouched messages stay borrowed.
/// Assistant thinking blocks are replayed as `thought: true` parts only for
/// same provider+model history; cross-provider thinking becomes plain text.
/// System/developer text (including mid-conversation updates) is folded out of
/// the conversation and returned separately — Gemini has no mid-conversation
/// system role; the caller joins it into `systemInstruction` (upstream
/// `collapseSystemMessages` + `withoutInitialSystemMessage`).
pub fn convert_google_contents(model: &Model, context: &Context) -> (Vec<Value>, Option<String>) {
    // Fold system/developer text out of the conversation. The kept messages
    // are REFERENCED, not cloned: `transform_messages_lazy_refs` only rebuilds
    // the messages it actually rewrites, so a transcript-sized request no
    // longer pays a second copy just to drop the system entries (Gemini has no
    // mid-conversation system role, so every request takes this branch).
    let mut system_parts: Vec<String> = Vec::new();
    let mut conversation: Vec<&Message> = Vec::with_capacity(context.messages.len());
    for message in &context.messages {
        match message {
            Message::System(system) | Message::Developer(system) => {
                let text = crate::ai::types::system_message_text(system);
                if !text.trim().is_empty() {
                    system_parts.push(text);
                }
            }
            Message::User(user) if user.role != crate::ai::types::UserRole::User => {
                let text = blocks_joined(&user.content);
                if !text.trim().is_empty() {
                    system_parts.push(text);
                }
            }
            other => conversation.push(other),
        }
    }
    let folded_system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    let transformed = transform_messages_lazy_refs(
        &conversation,
        model,
        Some(&|id, _source| normalize_tool_call_id(&model.id, id)),
    );
    let mut contents: Vec<Value> = Vec::new();
    for message in &transformed {
        match message.as_ref() {
            Message::User(user) => {
                let parts = user_parts(user);
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({"role": "user", "parts": parts}));
            }
            Message::Assistant(assistant) => {
                let parts = assistant_parts(model, assistant);
                if parts.is_empty() {
                    continue;
                }
                contents.push(json!({"role": "model", "parts": parts}));
            }
            Message::ToolResult(result) => {
                push_tool_result(model, &mut contents, result);
            }
            // System content was folded out above.
            Message::System(_) | Message::Developer(_) => {}
        }
    }
    (contents, folded_system)
}

fn blocks_joined(content: &[ContentBlock]) -> String {
    crate::ai::api::joined_text_blocks(content, "\n\n")
}

fn user_parts(user: &UserMessage) -> Vec<Value> {
    user.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(json!({ "text": text.text })),
            ContentBlock::Image(image) => Some(json!({
                "inlineData": { "mimeType": image.mime_type, "data": image.data }
            })),
            _ => None,
        })
        .collect()
}

fn assistant_parts(model: &Model, assistant: &crate::ai::types::AssistantMessage) -> Vec<Value> {
    let same = is_same_provider_and_model(model, assistant);
    let mut parts: Vec<Value> = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Text(text) => {
                let signature = resolve_thought_signature(same, text.text_signature.as_deref());
                // Signed empty text blocks must be echoed back (Gemini can
                // attach the signature to an empty-text part).
                if text.text.trim().is_empty() && signature.is_none() {
                    continue;
                }
                let mut part = json!({ "text": text.text });
                if let Some(signature) = &signature {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            ContentBlock::Thinking(thinking) => {
                if same {
                    let signature =
                        resolve_thought_signature(same, thinking.thinking_signature.as_deref());
                    if thinking.thinking.trim().is_empty() && signature.is_none() {
                        continue;
                    }
                    let mut part = json!({ "thought": true, "text": thinking.thinking });
                    if let Some(signature) = &signature {
                        part["thoughtSignature"] = json!(signature);
                    }
                    parts.push(part);
                } else {
                    // Cross-provider/model: signature unusable; empty stays dropped.
                    if thinking.thinking.trim().is_empty() {
                        continue;
                    }
                    parts.push(json!({ "text": thinking.thinking }));
                }
            }
            ContentBlock::ToolCall(call) => {
                let signature = resolve_thought_signature(same, call.thought_signature.as_deref());
                let mut function_call = json!({ "name": call.name, "args": call.arguments });
                if requires_tool_call_id(&model.id) {
                    function_call["id"] = json!(normalize_tool_call_id(&model.id, &call.id));
                }
                let mut part = json!({ "functionCall": function_call });
                if let Some(signature) = &signature {
                    part["thoughtSignature"] = json!(signature);
                }
                parts.push(part);
            }
            ContentBlock::Image(_) => {}
        }
    }
    parts
}

fn push_tool_result(model: &Model, contents: &mut Vec<Value>, result: &ToolResultMessage) {
    let text: String = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<Value> = if model.input.iter().any(|input| input == "image") {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image(image) => Some(json!({
                    "inlineData": { "mimeType": image.mime_type, "data": image.data }
                })),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    // Whitespace-only output (a bare `\r\n` from a shell tool) uses the empty
    // placeholder: some providers reject a whitespace-only function response
    // with a 400 that repeats on every later request (#8720, drift D35).
    let has_text = !text.trim().is_empty();
    let has_images = !images.is_empty();
    let multimodal = supports_multimodal_function_response(&model.id);

    // "output" key for success, "error" key for failures (SDK convention).
    let response_value = if has_text {
        json!(text)
    } else if has_images {
        json!("(see attached image)")
    } else {
        json!("")
    };
    let mut function_response = json!({
        "name": result.tool_name,
        "response": if result.is_error { json!({ "error": response_value }) } else { json!({ "output": response_value }) },
    });
    if has_images && multimodal {
        function_response["parts"] = json!(images);
    }
    if requires_tool_call_id(&model.id) {
        function_response["id"] = json!(normalize_tool_call_id(&model.id, &result.tool_call_id));
    }
    let part = json!({ "functionResponse": function_response });

    // All function responses merge into a single user turn.
    let last = contents.last_mut();
    let merged = match last {
        Some(last) if last.get("role").and_then(Value::as_str) == Some("user") => last
            .get("parts")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().any(|p| p.get("functionResponse").is_some()))
            .unwrap_or(false),
        _ => false,
    };
    if merged {
        if let Some(parts) = contents
            .last_mut()
            .and_then(|last| last.get_mut("parts"))
            .and_then(Value::as_array_mut)
        {
            parts.push(part);
        }
    } else {
        contents.push(json!({ "role": "user", "parts": [part] }));
    }

    // Gemini < 3: images ride in a separate user turn.
    if has_images && !multimodal {
        let mut parts = vec![json!({ "text": "Tool result image:" })];
        parts.extend(images);
        contents.push(json!({ "role": "user", "parts": parts }));
    }
}

// ---------------------------------------------------------------------------
// Tools (upstream google-shared.ts convertTools + function-calling mode)
// ---------------------------------------------------------------------------

/// Convert pir tools to Gemini `functionDeclarations` (upstream `convertTools`
/// with `parametersJsonSchema`). Returns `(tools, functionCallingMode)`; the
/// mode follows upstream `resolveGoogleFunctionCallingMode`
/// (google-shared.ts:423-436): an expressible `none` maps to `NONE` before the
/// strict check (upstream also maps `any`, which pir's [`ToolChoice`] cannot
/// express), strict tools select `VALIDATED`, and an explicit `auto` maps to
/// `AUTO` — an unset choice leaves the mode unset.
///
/// Strictness comes from `resolve_json_schema_strict_sampling` (upstream
/// `resolveJsonSchemaStrictSampling`): a `"require"` tool that cannot be made
/// strict is an error, a `"prefer"` tool silently falls back to non-strict, and
/// only a successfully transformed tool counts toward `VALIDATED`.
pub fn convert_google_tools(
    model: &Model,
    tools: &[Tool],
    tool_choice: Option<&ToolChoice>,
) -> AiResult<Option<(Value, Option<Value>)>> {
    if tools.is_empty() {
        return Ok(None);
    }
    let supports_strict = supports_google_strict_tool_sampling(&model.id);
    let mut declarations = Vec::new();
    let mut any_strict = false;
    for tool in tools {
        // Upstream only reports strict when the transformation succeeds; the
        // shared resolver owns the require/prefer policy and its errors.
        let strict =
            resolve_json_schema_strict_sampling(tool, supports_strict, None)?.unwrap_or(false);
        if strict {
            any_strict = true;
        }
        let parameters = get_json_schema_tool_parameters(tool, strict)?;
        let mut declaration = json!({ "name": tool.name, "description": tool.description });
        // Upstream sends the full JSON-Schema `parametersJsonSchema` and only
        // rewrites `const` (Gemini ignores it) to a single-value `enum`. Meta
        // declarations (`$schema`, `$defs`, ...) are forwarded verbatim on
        // both paths: the strict transform already rejects `$defs`/
        // `definitions` as unsupported, so those can only reach the wire on
        // the documented non-strict fallback, where upstream forwards them.
        declaration["parametersJsonSchema"] = rewrite_const_as_enum(&parameters, false);
        declarations.push(declaration);
    }
    let tools_value = json!([{ "functionDeclarations": declarations }]);
    // Upstream checks none/any before the strict-validated mode.
    let mode: Option<&'static str> = match tool_choice {
        Some(ToolChoice::None) => Some("NONE"),
        Some(ToolChoice::Any) | Some(ToolChoice::Required) => Some("ANY"),
        _ if any_strict => Some("VALIDATED"),
        // A named-tool choice has no Gemini equivalent; upstream maps the
        // fallback through `mapToolChoice`, whose default is AUTO.
        Some(ToolChoice::Auto) | Some(ToolChoice::Tool { .. }) => Some("AUTO"),
        None => None,
    };
    Ok(Some((
        tools_value,
        mode.map(|mode| json!({ "functionCallingConfig": { "mode": mode } })),
    )))
}

/// Schema keywords whose values map names to subschemas, so their keys are not
/// schema keywords (upstream `JSON_SCHEMA_NAME_MAPS`). The `is_name_map` flag
/// passed to [`rewrite_const_as_enum`] distinguishes these from ordinary
/// schema objects: a `const` key inside a property named "const" is data, not
/// a schema keyword.
const JSON_SCHEMA_NAME_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

/// Keywords containing a subschema or an array of subschemas, not literal data
/// (upstream `JSON_SCHEMA_SUBSCHEMAS`).
const JSON_SCHEMA_SUBSCHEMAS: [&str; 16] = [
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "items",
    "prefixItems",
    "additionalItems",
    "unevaluatedItems",
    "contains",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "contentSchema",
];

/// Rewrite string and number `const` values to single-value `enum`s
/// (upstream `rewriteConstAsEnum`): Gemini ignores `const` in
/// `parametersJsonSchema` but honors `enum`. Booleans and other constant
/// values are left untouched, as upstream does. `is_name_map` marks an object
/// whose keys are property names rather than schema keywords, so its values
/// are all schemas and the map itself is never rewritten.
fn rewrite_const_as_enum(schema: &Value, is_name_map: bool) -> Value {
    match schema {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| rewrite_const_as_enum(item, false))
                .collect(),
        ),
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                let rewritten = if is_name_map || JSON_SCHEMA_SUBSCHEMAS.contains(&key.as_str()) {
                    rewrite_const_as_enum(value, false)
                } else if JSON_SCHEMA_NAME_MAPS.contains(&key.as_str()) {
                    rewrite_const_as_enum(value, true)
                } else {
                    // Literal data (`examples`, `default`, `enum`, ...) and
                    // unknown keywords are not schemas and stay untouched.
                    value.clone()
                };
                result.insert(key.clone(), rewritten);
            }
            if !is_name_map && result.get("enum").is_none() {
                let const_value = result.get("const").cloned();
                if let Some(const_value) = const_value {
                    if const_value.is_string() || const_value.is_number() {
                        result.remove("const");
                        result.insert("enum".into(), Value::Array(vec![const_value]));
                    }
                }
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}
