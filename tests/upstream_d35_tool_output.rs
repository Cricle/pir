//! D35 — whitespace-only tool output is sent as the empty placeholder
//! (upstream #8720).
//!
//! A shell tool that prints nothing but a bare `\r\n` produced a
//! whitespace-only tool result. OpenAI Completions, OpenAI Responses and
//! Google forwarded it verbatim; some providers reject that with a 400 that
//! then repeats on every later request in the session. Upstream treats
//! whitespace-only as empty for the placeholder decision (and, for Responses
//! on multimodal models, for the `input_text` push too).

use std::collections::BTreeMap;

use serde_json::Value;

use pir::ai::api::google::shared::convert_google_contents;
use pir::ai::api::openai::convert_messages;
use pir::ai::api::openai_responses::shared::{
    convert_responses_messages, ConvertResponsesMessagesOptions,
};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates,
    OpenAiCompat, StopReason, TextContent, ToolCall, ToolResultMessage, ToolResultRole,
};

const WHITESPACE_OUTPUT: &str = " \r\n\t ";

fn model(api: &str, input: &[&str]) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: api.into(),
        provider: "test".into(),
        base_url: "https://example.invalid".into(),
        reasoning: false,
        thinking_level_map: None,
        input: input.iter().map(|value| (*value).to_string()).collect(),
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: vec![],
        },
        context_window: 16_000,
        max_tokens: 2_000,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
        text_signature: None,
    })
}

fn tool_exchange(output: &str) -> Context {
    let call_id = "call-1";
    Context {
        system_prompt: None,
        messages: vec![
            Message::Assistant(AssistantMessage {
                api: "openai-completions".into(),
                provider: "test".into(),
                model: "test-model".into(),
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: call_id.into(),
                    name: "bash".into(),
                    arguments: BTreeMap::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            }),
            Message::ToolResult(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: call_id.into(),
                tool_name: "bash".into(),
                content: vec![text(output)],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                structured_content: None,
                nested_calls: None,
                is_error: false,
                duration_ms: None,
                timestamp: 3,
            }),
        ],
        tools: None,
    }
}

/// Every string anywhere in a converted body, so the assertion does not depend
/// on each provider's JSON shape.
fn all_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| all_strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| all_strings(item, out)),
        _ => {}
    }
}

fn assert_placeholder_used(body: &Value, label: &str) {
    let mut strings = Vec::new();
    all_strings(body, &mut strings);
    assert!(
        strings.iter().any(|text| text == "(no tool output)"),
        "{label}: the empty placeholder must be sent for whitespace-only output, got {body}"
    );
    assert!(
        !strings.iter().any(|text| text == WHITESPACE_OUTPUT),
        "{label}: the whitespace-only output must not be forwarded verbatim, got {body}"
    );
}

#[test]
fn d35_openai_completions_uses_the_empty_placeholder() {
    let context = tool_exchange(WHITESPACE_OUTPUT);
    let converted = convert_messages(
        &model("openai-completions", &["text"]),
        &context,
        &OpenAiCompat::default(),
    );
    assert_placeholder_used(&Value::Array(converted), "openai-completions");
}

#[test]
fn d35_google_uses_the_empty_string() {
    // Google's empty placeholder is "" (not the "(no tool output)" text the
    // OpenAI adapters use); what matters is that the whitespace is gone.
    let context = tool_exchange(WHITESPACE_OUTPUT);
    let (converted, _system) =
        convert_google_contents(&model("google-generative-ai", &["text"]), &context);
    let body = Value::Array(converted);
    let mut strings = Vec::new();
    all_strings(&body, &mut strings);
    assert!(
        strings.iter().any(|text| text.is_empty()),
        "google: an empty function response must be sent, got {body}"
    );
    assert!(
        !strings.iter().any(|text| text == WHITESPACE_OUTPUT),
        "google: the whitespace-only output must not be forwarded verbatim, got {body}"
    );
}

#[test]
fn d35_openai_responses_uses_the_empty_placeholder() {
    let context = tool_exchange(WHITESPACE_OUTPUT);
    let converted = convert_responses_messages(
        &model("openai-responses", &["text"]),
        &context,
        &["test"],
        &ConvertResponsesMessagesOptions::default(),
    );
    assert_placeholder_used(&Value::Array(converted), "openai-responses");
}

#[test]
fn d35_real_output_is_still_sent_verbatim() {
    // Control: a non-blank result must not be replaced by the placeholder.
    let context = tool_exchange("  real output\n");
    let converted = convert_messages(
        &model("openai-completions", &["text"]),
        &context,
        &OpenAiCompat::default(),
    );
    let mut strings = Vec::new();
    all_strings(&Value::Array(converted), &mut strings);
    assert!(
        strings.iter().any(|text| text == "  real output\n"),
        "non-blank output must be forwarded verbatim"
    );
}
