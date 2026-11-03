//! Regression coverage for the in-flight tool-call argument mirror
//! (`src/ai/api/partial_mirror.rs`, R1-L2-008).
//!
//! The mirror cadence must treat only the ROOT container closing as a
//! structural completion. Before the fix every nested `}`/`]` triggered a
//! whole-document `parse_partial_json` of the accumulated arguments, which is
//! O(n²) for nested object/array payloads. These tests drive a real adapter
//! parser over an adversarial nested stream and pin the observable behavior:
//! the per-delta argument snapshots stay bounded by the byte cadence instead
//! of changing at every nested close, and `toolcall_end` still carries the
//! complete parsed arguments.

use std::collections::BTreeMap;

use pir::ai::api::anthropic::AnthropicSseParser;
use pir::ai::types::*;
use serde_json::{json, Value};

fn sample_model() -> Model {
    Model {
        id: "test".into(),
        name: "test".into(),
        api: "anthropic-messages".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 1024,
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

fn tool_arguments(partial: &AssistantMessage) -> Option<&BTreeMap<String, Value>> {
    partial.content.iter().find_map(|block| match block {
        ContentBlock::ToolCall(call) => Some(&call.arguments),
        _ => None,
    })
}

fn sse_data(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// Split immediately after every structural `}`/`]` outside strings: the
/// worst-case fragment boundaries for the old "any closer completes" rule.
fn split_after_structural_closers(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte == b'"' {
            in_string = true;
        } else if byte == b'}' || byte == b']' {
            parts.push(&text[start..=index]);
            start = index + 1;
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

fn anthropic_body(fragments: &[&str]) -> String {
    let mut body = String::new();
    body.push_str(&sse_data(&json!({
        "type": "message_start",
        "message": {
            "id": "msg_mirror",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        },
    })));
    body.push_str(&sse_data(&json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "tool_use", "id": "toolu_1", "name": "edit"},
    })));
    for fragment in fragments {
        body.push_str(&sse_data(&json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": fragment},
        })));
    }
    body.push_str(&sse_data(
        &json!({"type": "content_block_stop", "index": 0}),
    ));
    body.push_str(&sse_data(&json!({
        "type": "message_delta",
        "delta": {"stop_reason": "tool_use"},
    })));
    body.push_str(&sse_data(&json!({"type": "message_stop"})));
    body
}

#[test]
fn nested_tool_argument_stream_does_not_reparse_at_every_nested_close() {
    let items: Vec<Value> = (0..32)
        .map(|index| {
            json!({
                "index": index,
                "path": format!("src/module_{index}.rs"),
                "meta": {
                    "before": [1, 2, 3],
                    "after": {"lines": [format!("brace }} bracket ] quote \" {index}")]},
                },
                "nested": {"deeper": {"flag": true}},
            })
        })
        .collect();
    let full = serde_json::to_string(&json!({"items": items})).expect("fixture serializes");
    let fragments = split_after_structural_closers(&full);
    assert!(
        fragments.len() > 100,
        "fixture must expose many nested closes, got {} fragments",
        fragments.len()
    );
    assert!(
        full.len() < 8 * 1024,
        "keep the fixture inside one cadence window"
    );

    let model = sample_model();
    let mut parser = AnthropicSseParser::new(&model);
    let mut events = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    parser
        .feed(anthropic_body(&fragments).as_bytes(), &mut events)
        .expect("anthropic stream feeds");
    parser
        .finish(&mut events)
        .expect("anthropic stream finishes");

    // How many deltas carried a NEW argument snapshot? The mirror may refresh
    // on the first fragment and on the root close; nested closes must not.
    let mut snapshots: Vec<BTreeMap<String, Value>> = Vec::new();
    let mut deltas = 0usize;
    for event in &events {
        let AssistantMessageEvent::ToolCallDelta {
            content_index,
            partial,
            ..
        } = event
        else {
            continue;
        };
        assert_eq!(*content_index, 0);
        deltas += 1;
        let arguments = tool_arguments(partial).expect("tool block in the partial");
        if snapshots.last() != Some(arguments) {
            snapshots.push(arguments.clone());
        }
    }
    assert_eq!(deltas, fragments.len(), "every fragment is surfaced");
    assert!(
        snapshots.len() <= 3,
        "nested closes refreshed the mirror {} times across {} fragments (expected <= 3)",
        snapshots.len(),
        fragments.len()
    );

    let tool_end = events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
            _ => None,
        })
        .expect("toolcall_end at content_block_stop");
    let expected: BTreeMap<String, Value> =
        serde_json::from_str(&full).expect("fixture is valid JSON");
    assert_eq!(
        tool_end.arguments, expected,
        "toolcall_end must parse the complete raw arguments"
    );
}
