//! Reasoning-detail merge/signature round-trip and thinking-budget helpers.

use std::collections::BTreeMap;

use serde_json::{json, Value};

// The per-level budget resolvers are shared with the bedrock adapter, so they
// live in the core (`crate::ai::api`) and keep compiling when `provider-openai`
// is off; `openai` re-exports them for external users.
use crate::ai::api::thinking_budget_for_level;
use crate::ai::types::*;

/// Whitelisted raw reasoning field names on assistant messages (upstream
/// `OPENAI_COMPLETIONS_REASONING_FIELDS`); e.g. llama.cpp + gpt-oss.
const OPENAI_COMPLETIONS_REASONING_FIELDS: [&str; 3] =
    ["reasoning", "reasoning_content", "reasoning_text"];

fn is_openai_completions_reasoning_field(field: &str) -> bool {
    OPENAI_COMPLETIONS_REASONING_FIELDS.contains(&field)
}

/// Replay reasoning metadata on assistant messages (upstream `convertMessages`,
/// assistant branch): when a thinking block's `thinking_signature` parses as a
/// valid `OpenAIReasoningDetail[]` it is sent verbatim as `reasoning_details`
/// and the raw reasoning text field is suppressed; otherwise legacy encrypted
/// reasoning recorded on tool calls is replayed as `reasoning_details`, and
/// failing both the thinking text is replayed under the whitelisted field named
/// by the signature.
pub(crate) fn apply_assistant_reasoning(item: &mut Value, message: &AssistantMessage) {
    let thinking_blocks: Vec<&ThinkingContent> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking(thinking) => Some(thinking),
            _ => None,
        })
        .collect();
    // Upstream `legacyReasoningDetails`: encrypted reasoning persisted on tool
    // calls before the dedicated thinking-signature slot existed. Only used
    // when no thinking block carries the signed details.
    let legacy_details: Vec<Value> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => {
                parse_legacy_encrypted_reasoning_detail(call.thought_signature.as_deref())
            }
            _ => None,
        })
        .collect();
    let preserved_reasoning_details = thinking_blocks
        .iter()
        .find_map(|block| parse_openai_reasoning_details(block.thinking_signature.as_deref()))
        .or_else(|| (!legacy_details.is_empty()).then_some(legacy_details));
    if let Some(details) = preserved_reasoning_details {
        item["reasoning_details"] = Value::Array(details);
        return;
    }
    let non_empty: Vec<&ThinkingContent> = thinking_blocks
        .into_iter()
        .filter(|block| !block.thinking.trim().is_empty())
        .collect();
    if let Some(first) = non_empty.first() {
        if let Some(field) = first.thinking_signature.as_deref() {
            // Upstream renames opencode-go's `reasoning` marker to
            // `reasoning_content` on replay so the gateway accepts it.
            let field = if message.provider == "opencode-go" && field == "reasoning" {
                "reasoning_content"
            } else {
                field
            };
            if is_openai_completions_reasoning_field(field) {
                let joined = non_empty
                    .iter()
                    .map(|block| block.thinking.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                item[field] = json!(joined);
            }
        }
    }
}

/// Parse a legacy encrypted reasoning detail stored on a tool call's
/// `thought_signature` (upstream `parseLegacyEncryptedReasoningDetail`): a
/// single `reasoning.encrypted` object whose `id` and `data` are non-empty.
fn parse_legacy_encrypted_reasoning_detail(signature: Option<&str>) -> Option<Value> {
    let parsed: Value = serde_json::from_str(signature?).ok()?;
    let object = parsed.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("reasoning.encrypted") {
        return None;
    }
    let id = object.get("id").and_then(Value::as_str)?;
    let data = object.get("data").and_then(Value::as_str)?;
    if id.is_empty() || data.is_empty() {
        return None;
    }
    Some(parsed)
}

/// Mirror upstream's thinking emission (openai-completions.ts
/// `applyThinkingLevel`): dispatch on `model.compat.thinking_format`, gated by
/// `compat.supports_reasoning_effort` where upstream gates it. The requested
/// level maps through `thinking_level_map[level] ?? level`; "off" is expressed
/// per format (openai: only via an explicit map entry; openrouter/deepseek/
/// string-thinking: a disabled/none marker unless the map's `off` entry is an
/// explicit null). Public for tests/.
pub fn apply_reasoning_effort(body: &mut Value, model: &Model, reasoning: Option<ThinkingLevel>) {
    if !model.reasoning {
        return;
    }
    let compat = super::compat_detect::resolve_compat(model);
    // pir models "thinking off" as `Some(Off)`; `None` means not requested.
    // Upstream receives the level as a string with off translated away, so
    // `requested` mirrors its `options?.reasoningEffort` truthiness.
    let requested = reasoning.filter(|level| !matches!(level, ThinkingLevel::Off));
    // `thinkingLevelMap[level] ?? level` (nullish: an explicit null map entry
    // falls back to the wire key).
    let effort = |model: &Model, level: ThinkingLevel| -> String {
        let key = level.wire_key();
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(key))
            .cloned()
            .flatten()
            .unwrap_or_else(|| key.to_string())
    };
    // `thinkingLevelMap?.off` three-state: absent (use fallback), explicit null
    // (suppress), explicit string (send verbatim).
    let off_value = || -> Option<Option<String>> {
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("off"))
            .cloned()
    };
    match compat.thinking_format {
        ThinkingFormat::Openai => {
            if !compat.supports_reasoning_effort {
                return;
            }
            match requested {
                Some(level) => {
                    body["reasoning_effort"] = json!(effort(model, level));
                }
                None => {
                    if let Some(Some(off)) = off_value() {
                        body["reasoning_effort"] = json!(off);
                    }
                }
            }
        }
        ThinkingFormat::Openrouter => {
            // OpenRouter normalizes reasoning across providers via a nested
            // reasoning object.
            match requested {
                Some(level) => {
                    body["reasoning"] = json!({ "effort": effort(model, level) });
                }
                None => match off_value() {
                    Some(None) => {}
                    Some(Some(off)) => {
                        body["reasoning"] = json!({ "effort": off });
                    }
                    None => {
                        body["reasoning"] = json!({ "effort": "none" });
                    }
                },
            }
        }
        ThinkingFormat::Deepseek => match requested {
            Some(level) => {
                body["thinking"] = json!({ "type": "enabled" });
                if compat.supports_reasoning_effort {
                    body["reasoning_effort"] = json!(effort(model, level));
                }
            }
            None => {
                if !matches!(off_value(), Some(None)) {
                    body["thinking"] = json!({ "type": "disabled" });
                }
            }
        },
        ThinkingFormat::Zai => match requested {
            Some(level) => {
                body["thinking"] = json!({ "type": "enabled", "clear_thinking": false });
                if compat.supports_reasoning_effort {
                    body["reasoning_effort"] = json!(effort(model, level));
                }
            }
            None => {
                body["thinking"] = json!({ "type": "disabled" });
            }
        },
        ThinkingFormat::Qwen => {
            body["enable_thinking"] = json!(requested.is_some());
            if let Some(level) = requested {
                if compat.supports_reasoning_effort {
                    body["reasoning_effort"] = json!(effort(model, level));
                }
            }
        }
        ThinkingFormat::QwenChatTemplate => {
            body["chat_template_kwargs"] = json!({
                "enable_thinking": requested.is_some(),
                "preserve_thinking": true,
            });
        }
        ThinkingFormat::Baseten => {
            // The reasoning_effort half; `chat_template_args` (with the
            // clamped thinking budget) is emitted by `apply_thinking_budget`.
            if compat.supports_reasoning_effort {
                match requested {
                    Some(level) => {
                        body["reasoning_effort"] = json!(effort(model, level));
                    }
                    None => {
                        if let Some(Some(off)) = off_value() {
                            body["reasoning_effort"] = json!(off);
                        }
                    }
                }
            }
        }
        ThinkingFormat::Together => {
            body["reasoning"] = json!({ "enabled": requested.is_some() });
            if let Some(level) = requested {
                if compat.supports_reasoning_effort {
                    body["reasoning_effort"] = json!(effort(model, level));
                }
            }
        }
        ThinkingFormat::StringThinking => match requested {
            Some(level) => {
                body["thinking"] = json!(effort(model, level));
            }
            None => match off_value() {
                Some(None) => {}
                Some(Some(off)) => {
                    body["thinking"] = json!(off);
                }
                None => {
                    body["thinking"] = json!("none");
                }
            },
        },
        ThinkingFormat::AntLing => {
            // Upstream requires an explicit string map entry (no raw fallback).
            if let Some(level) = requested {
                if let Some(Some(effort_value)) = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(level.wire_key()))
                    .cloned()
                {
                    body["reasoning"] = json!({ "effort": effort_value });
                }
            }
        }
        ThinkingFormat::ChatTemplate => {
            // `chat_template_kwargs` (with the clamped thinking budget) is
            // emitted by `apply_thinking_budget`.
        }
    }
}

/// Validate one `choice.delta.reasoning_details` entry (upstream
/// `isOpenAIReasoningDetail`): a non-array object with well-typed common
/// fields, then per-type required payload fields.
pub fn is_openai_reasoning_detail(detail: &Value) -> bool {
    let Some(obj) = detail.as_object() else {
        return false;
    };
    let Some(detail_type) = obj.get("type").and_then(Value::as_str) else {
        return false;
    };
    let common_ok = matches!(
        obj.get("id"),
        None | Some(Value::Null) | Some(Value::String(_))
    ) && matches!(obj.get("format"), None | Some(Value::String(_)))
        && matches!(obj.get("index"), None | Some(Value::Number(_)));
    if !common_ok {
        return false;
    }
    match detail_type {
        "reasoning.summary" => obj.get("summary").and_then(Value::as_str).is_some(),
        "reasoning.encrypted" => obj.get("data").and_then(Value::as_str).is_some(),
        "reasoning.text" => {
            obj.get("text").and_then(Value::as_str).is_some()
                && matches!(
                    obj.get("signature"),
                    None | Some(Value::Null) | Some(Value::String(_))
                )
        }
        _ => false,
    }
}

/// Merge one validated detail into the streamed list (upstream
/// `appendOpenAIReasoningDetail`): consecutive same-type text/summary deltas
/// concatenate, missing `id`/`format`/`index` are backfilled from the earlier
/// entry, everything else is pushed as a discrete entry.
pub fn append_openai_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let detail_type = detail.get("type").and_then(Value::as_str);
    if let Some(last) = details.last_mut() {
        let last_type = last.get("type").and_then(Value::as_str);
        if detail_type == Some("reasoning.text") && last_type == Some("reasoning.text") {
            concat_string_field(last, "text", detail, "text");
            // `signature ||=`: fill only when the earlier entry has none.
            let last_signature_empty =
                !matches!(last.get("signature"), Some(Value::String(s)) if !s.is_empty());
            if last_signature_empty {
                if let Some(signature) = detail.get("signature") {
                    if let Some(obj) = last.as_object_mut() {
                        obj.insert("signature".into(), signature.clone());
                    }
                }
            }
            fill_missing_common_reasoning_detail_fields(last, detail);
            return;
        }
        if detail_type == Some("reasoning.summary") && last_type == Some("reasoning.summary") {
            concat_string_field(last, "summary", detail, "summary");
            fill_missing_common_reasoning_detail_fields(last, detail);
            return;
        }
    }
    details.push(detail.clone());
}

/// Backfill missing common fields from the earlier entry (upstream
/// `fillMissingCommonReasoningDetailFields`: `id`/`index` use `??=`,
/// `format` uses `||=` which also fills empty strings).
fn fill_missing_common_reasoning_detail_fields(target: &mut Value, source: &Value) {
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    let fill_id = !matches!(obj.get("id"), Some(Value::String(_)));
    let fill_index = !matches!(obj.get("index"), Some(Value::Number(_)));
    let fill_format = !matches!(obj.get("format"), Some(Value::String(s)) if !s.is_empty());
    if fill_id {
        obj.insert(
            "id".into(),
            source.get("id").cloned().unwrap_or(Value::Null),
        );
    }
    if fill_index {
        obj.insert(
            "index".into(),
            source.get("index").cloned().unwrap_or(Value::Null),
        );
    }
    if fill_format {
        obj.insert(
            "format".into(),
            source.get("format").cloned().unwrap_or(Value::Null),
        );
    }
}

fn concat_string_field(target: &mut Value, target_key: &str, source: &Value, source_key: &str) {
    if let (Some(obj), Some(delta)) = (
        target.as_object_mut(),
        source.get(source_key).and_then(Value::as_str),
    ) {
        let current = obj.get(target_key).and_then(Value::as_str).unwrap_or("");
        obj.insert(target_key.into(), json!(format!("{current}{delta}")));
    }
}

/// Parse a thinking block signature into a validated `OpenAIReasoningDetail[]`
/// (upstream `parseOpenAIReasoningDetails`): non-empty array, all entries valid.
pub fn parse_openai_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed = serde_json::from_str::<Value>(signature?).ok()?;
    let array = parsed.as_array()?;
    if array.is_empty() || !array.iter().all(is_openai_reasoning_detail) {
        return None;
    }
    Some(array.clone())
}

/// Tokens always left for the answer when a thinking budget shares the
/// response ceiling (upstream `MIN_ANSWER_TOKENS`).
pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// Ordered pi thinking levels (upstream `MODEL_THINKING_LEVELS` minus `off`,
/// i.e. `THINKING_LEVELS`).
const EXTENDED_THINKING_LEVELS: [ThinkingLevel; 6] = [
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::XHigh,
    ThinkingLevel::Max,
];

/// Whether the model supports a pi thinking level (upstream
/// `getSupportedThinkingLevels`): non-reasoning models support only "off";
/// standard levels are supported unless the `thinkingLevelMap` entry is an
/// explicit `null`; `xhigh`/`max` require a non-null explicit mapping.
fn level_supported(model: &Model, level: ThinkingLevel) -> bool {
    if !model.reasoning {
        return matches!(level, ThinkingLevel::Off);
    }
    if matches!(level, ThinkingLevel::Off) {
        return true;
    }
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
fn clamp_thinking_level(model: &Model, level: ThinkingLevel) -> ThinkingLevel {
    if level_supported(model, level) {
        return level;
    }
    let index = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level);
    if let Some(index) = index {
        for candidate in &EXTENDED_THINKING_LEVELS[index..] {
            if level_supported(model, *candidate) {
                return *candidate;
            }
        }
        for candidate in EXTENDED_THINKING_LEVELS[..index].iter().rev() {
            if level_supported(model, *candidate) {
                return *candidate;
            }
        }
    }
    if level_supported(model, ThinkingLevel::Off) {
        ThinkingLevel::Off
    } else {
        ThinkingLevel::Medium
    }
}

/// Sampling-parameter merge chain (upstream `resolveSamplingParams` in
/// `simple-options.ts`): `model.samplingParams` ->
/// `samplingParamsByThinkingLevel[effectiveLevel]` -> request params, merged
/// per key with later sources winning. Returns `None` when no source sets
/// anything. Public for tests/.
pub fn resolve_sampling_params(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    request_params: Option<&BTreeMap<String, Value>>,
) -> Option<BTreeMap<String, Value>> {
    let effective_level = clamp_thinking_level(model, reasoning.unwrap_or(ThinkingLevel::Off));
    let thinking_level_params = model
        .compat
        .as_ref()
        .and_then(|compat| compat.sampling_params_by_thinking_level.as_ref())
        .or(model.sampling_params_by_thinking_level.as_ref())
        .and_then(|map| map.get(effective_level.wire_key()));
    if model.sampling_params.is_none()
        && thinking_level_params.is_none()
        && request_params.is_none()
    {
        return None;
    }
    let mut merged = BTreeMap::new();
    for source in [
        model.sampling_params.as_ref(),
        thinking_level_params,
        request_params,
    ]
    .into_iter()
    .flatten()
    {
        for (key, value) in source {
            merged.insert(key.clone(), value.clone());
        }
    }
    Some(merged)
}

/// Cap a thinking budget so at least [`MIN_ANSWER_TOKENS`] remain under a
/// shared response ceiling (upstream `clampThinkingBudgetToAnswerRoom`).
pub fn clamp_thinking_budget_to_answer_room(budget: u64, ceiling: u64) -> u64 {
    budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

/// The top-level field used to cap reasoning tokens (upstream
/// `resolveThinkingTokenBudgetField`): the model's field knob, then the compat
/// `thinkingTokenBudgetField`, then the `supportsThinkingTokenBudget` alias.
fn resolve_thinking_token_budget_field(
    model: &Model,
    compat: &OpenAiCompat,
) -> Option<ThinkingTokenBudgetField> {
    model.thinking_token_budget_field.or_else(|| {
        compat.thinking_token_budget_field_compat.or_else(|| {
            compat
                .supports_thinking_token_budget
                .filter(|supported| *supported)
                .map(|_| ThinkingTokenBudgetField::ThinkingTokenBudget)
        })
    })
}

/// Emit the per-model thinking token budget field and the chat-template
/// kwargs/args that consume the clamped budget (upstream
/// `resolveClampedThinkingBudget` + `resolveThinkingTokenBudgetField` +
/// `buildChatTemplateValues`). Requires the field knob, a reasoning model, a
/// non-`off` level, and a positive clamped budget. Public for tests/.
pub fn apply_thinking_budget(
    body: &mut Value,
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    budgets: Option<&ThinkingBudgets>,
) {
    let compat = super::compat_detect::resolve_compat(model);
    let Some(field) = resolve_thinking_token_budget_field(model, &compat) else {
        return;
    };
    let Some(level) = reasoning else {
        return;
    };
    if !model.reasoning || matches!(level, ThinkingLevel::Off) {
        return;
    }
    let ceiling = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| body.get("max_completion_tokens").and_then(Value::as_u64))
        .unwrap_or(model.max_tokens);
    let budget =
        clamp_thinking_budget_to_answer_room(thinking_budget_for_level(level, budgets), ceiling);
    if budget == 0 {
        return;
    }
    // Chat-template thinking formats consume the clamped budget through
    // `$var: "thinking.budget"` entries (upstream `buildChatTemplateValues`).
    match compat.thinking_format {
        ThinkingFormat::ChatTemplate => {
            if let Some(values) = build_chat_template_values(
                model,
                reasoning,
                compat.chat_template_kwargs.as_ref(),
                Some(budget),
            ) {
                body["chat_template_kwargs"] = values;
            }
        }
        ThinkingFormat::Baseten => {
            if let Some(values) = build_chat_template_values(
                model,
                reasoning,
                compat.chat_template_args.as_ref(),
                Some(budget),
            ) {
                body["chat_template_args"] = values;
            }
        }
        _ => {}
    }
    body[field.wire_name()] = json!(budget);
}

/// Resolve chat-template kwarg values (upstream `buildChatTemplateValues` +
/// `resolveChatTemplateKwargValue`): scalars pass through; `{ $var: ... }`
/// entries resolve to pi-controlled values (`thinking.enabled` → level
/// presence, `thinking.budget` → the clamped budget, otherwise the mapped
/// level value), and `omitWhenOff` entries drop out when no level is set.
fn build_chat_template_values(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    values: Option<&BTreeMap<String, Value>>,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let values = values?;
    let requested = reasoning.filter(|level| !matches!(level, ThinkingLevel::Off));
    let mut resolved = serde_json::Map::new();
    for (key, value) in values {
        let Some(resolved_value) =
            resolve_chat_template_kwarg_value(model, requested, value, thinking_budget)
        else {
            continue;
        };
        resolved.insert(key.clone(), resolved_value);
    }
    if resolved.is_empty() {
        None
    } else {
        Some(Value::Object(resolved))
    }
}

fn resolve_chat_template_kwarg_value(
    model: &Model,
    requested: Option<ThinkingLevel>,
    value: &Value,
    thinking_budget: Option<u64>,
) -> Option<Value> {
    let Some(object) = value.as_object() else {
        return Some(value.clone());
    };
    let var = object.get("$var").and_then(Value::as_str);
    let omit_when_off = object
        .get("omitWhenOff")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if requested.is_none() && omit_when_off {
        return None;
    }
    match var {
        Some("thinking.enabled") => {
            return Some(json!(requested.is_some()));
        }
        Some("thinking.budget") => {
            return thinking_budget.map(|budget| json!(budget));
        }
        _ => {}
    }
    // `thinkingLevelMap[level] ?? level` for a requested level; with no level
    // requested the map's `off` entry (if a string) stands in, else nothing.
    let mapped = match requested {
        Some(level) => model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(level.wire_key()))
            .cloned()
            .flatten(),
        None => model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get("off"))
            .cloned()
            .flatten(),
    };
    match mapped {
        Some(mapped) => Some(json!(mapped)),
        None => requested.map(|level| json!(level.wire_key())),
    }
}
