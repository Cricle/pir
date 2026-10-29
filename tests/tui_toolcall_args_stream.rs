//! Regression tests for the TUI's streamed tool-call argument resolution
//! (`src/tui/terminal.rs`, the `toolcall_delta` arm).
//!
//! That arm used to repair and parse the WHOLE accumulated argument buffer on
//! every delta, even though every in-tree adapter already mirrors its parsed
//! in-flight arguments into the event's partial block. `streamed_tool_call_args`
//! now prefers the mirrored snapshot when the block carries arguments and only
//! falls back to the raw-buffer parse for adapters that leave the block empty.
//!
//! These tests pin the observable contract: the args delivered per delta still
//! equal the parsed arguments, the choice is made per call, and a large
//! buffer split into many deltas resolves at every step.

use std::collections::BTreeMap;

use pir::ai::api::openai_responses::shared::parse_partial_json;
use pir::ai::types::ToolCall;
use pir::tui::terminal::streamed_tool_call_args;
use serde_json::{json, Value};

fn call_with(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call-1".to_string(),
        name: "write".to_string(),
        arguments: serde_json::from_value::<BTreeMap<String, Value>>(arguments)
            .expect("tool arguments are an object"),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }
}

/// What the handler delivers for one delta given the caller's accumulated raw
/// buffer: `None` when no JSON object can be produced yet.
fn expected_delivery(raw: &str) -> Option<Value> {
    let parsed = parse_partial_json(raw);
    parsed.is_object().then_some(parsed)
}

#[test]
fn mirrored_arguments_win_over_the_raw_buffer() {
    // A mirroring adapter (anthropic/openai/bedrock/…) already parsed the
    // in-flight arguments into the partial block. The raw buffer is stale
    // here on purpose: if the handler re-parsed it, the delivered args would
    // differ (and be `None` for this garbage).
    let call = call_with(json!({ "path": "src/main.rs", "limit": 5 }));
    let mut raw = String::from("{\"stale\": true}");
    let delivered = streamed_tool_call_args(&call, "{\"path\":", &mut raw)
        .expect("mirrored arguments resolve without the buffer");
    assert_eq!(delivered, json!({ "path": "src/main.rs", "limit": 5 }));
    assert_ne!(
        Some(delivered),
        expected_delivery(&raw),
        "the raw-buffer fallback must not be what produced these args"
    );
}

#[test]
fn non_mirrored_adapter_still_gets_the_parsed_buffer() {
    // No adapter mirrors here: the block stays an empty object while the raw
    // fragments accumulate, so the fallback must parse every step exactly as
    // before.
    let call = call_with(json!({}));
    let mut raw = String::new();
    let mut rebuilt = String::new();
    let chunks = ["{\"path\":", "\"src/", "main.rs\",", "\"line\":", "12}"];
    for chunk in chunks {
        rebuilt.push_str(chunk);
        let delivered = streamed_tool_call_args(&call, chunk, &mut raw);
        assert_eq!(
            delivered,
            expected_delivery(&rebuilt),
            "fallback args drifted after {rebuilt:?}"
        );
    }
    assert_eq!(
        delivered_final(&raw),
        json!({ "path": "src/main.rs", "line": 12 })
    );
    assert_eq!(raw, rebuilt);
}

/// Re-run the final arrival state without keeping the per-step value around.
fn delivered_final(raw: &str) -> Value {
    expected_delivery(raw).expect("complete arguments are an object")
}

#[test]
fn mirroring_adapter_delivers_the_parsed_arguments_at_every_delta() {
    // Reproduce the provider side faithfully: each delta extends the raw
    // buffer and the provider mirrors the accumulated parse into the block.
    let mut call = call_with(json!({}));
    let mut raw = String::new();
    let mut rebuilt = String::new();
    let chunks = [
        "{\"path\":",
        "\"src/",
        "main.rs\",",
        "\"content\":",
        "\"hello ",
        "world\"}",
    ];
    for chunk in chunks {
        rebuilt.push_str(chunk);
        // Provider mirroring keeps the last usable parse: a fragment with no
        // object yet leaves the block unchanged, exactly like anthropic's
        // `parse_streaming_tool_arguments` returning `None`.
        if let Ok(arguments) =
            serde_json::from_value::<BTreeMap<String, Value>>(parse_partial_json(&rebuilt))
        {
            call.arguments = arguments;
        }
        let delivered = streamed_tool_call_args(&call, chunk, &mut raw);
        match expected_delivery(&rebuilt) {
            // Whenever the current buffer parses to an object, that parse is
            // what the provider mirrored and what we deliver.
            Some(parsed) => {
                assert_eq!(
                    delivered,
                    Some(parsed),
                    "delivered args must equal the parsed arguments after {rebuilt:?}"
                );
            }
            // A fragment that no longer parses keeps the provider's last
            // usable parse (it does not clear the pending block).
            None => assert_eq!(
                delivered,
                (!call.arguments.is_empty())
                    .then(|| serde_json::to_value(&call.arguments).unwrap()),
                "the last usable parse must survive an unparsable fragment ({rebuilt:?})"
            ),
        }
    }
    assert_eq!(
        delivered_final(&raw),
        json!({ "path": "src/main.rs", "content": "hello world" })
    );
    assert_eq!(
        call.arguments,
        serde_json::from_value::<BTreeMap<String, Value>>(
            json!({ "path": "src/main.rs", "content": "hello world" })
        )
        .unwrap()
    );
}

#[test]
fn the_choice_is_per_call_not_global() {
    // One mirroring call and one raw-only call stream at the same time.
    let mirroring = call_with(json!({ "path": "a.txt" }));
    let raw_only = call_with(json!({}));
    let mut mirroring_raw = String::new();
    let mut raw_only_buffer = String::new();

    let mirrored = streamed_tool_call_args(&mirroring, "{", &mut mirroring_raw)
        .expect("mirroring call resolves");
    assert_eq!(mirrored, json!({ "path": "a.txt" }));

    let raw_delivered = streamed_tool_call_args(&raw_only, "{\"path\":\"a", &mut raw_only_buffer)
        .expect("raw call resolves");
    assert_eq!(
        raw_delivered,
        expected_delivery(&raw_only_buffer).expect("the raw prefix parses to an object")
    );

    // The mirroring call's empty-ish delta did not disturb the other call.
    assert_ne!(mirrored, raw_delivered);
}

#[test]
fn non_object_fragment_keeps_the_previous_args() {
    let call = call_with(json!({}));
    let mut raw = String::new();
    assert!(
        streamed_tool_call_args(&call, "\"unterminated", &mut raw).is_none(),
        "a non-object fragment must not replace the pending block's args"
    );
}

#[test]
fn large_arguments_buffer_split_into_many_deltas() {
    let payload = "x".repeat(200_000);
    let full = serde_json::to_string(&json!({ "payload": payload })).expect("args serialize");
    let mut raw = String::new();
    let mut rebuilt = String::new();
    let mut call = call_with(json!({}));
    let mut deltas = 0usize;
    for chunk in full.as_bytes().chunks(997) {
        let chunk = std::str::from_utf8(chunk).expect("ASCII arguments");
        rebuilt.push_str(chunk);
        deltas += 1;
        // Provider mirroring: the block carries the parse of everything so
        // far, exactly like the anthropic/openai accumulators.
        call.arguments = serde_json::from_value(parse_partial_json(&rebuilt))
            .expect("every repaired prefix stays an object");
        let delivered = streamed_tool_call_args(&call, chunk, &mut raw)
            .expect("an object is available at every step");
        let expected = expected_delivery(&rebuilt).expect("an object is available at every step");
        assert_eq!(
            delivered,
            expected,
            "args diverged after {deltas} deltas ({} bytes accumulated)",
            rebuilt.len()
        );
    }
    assert!(deltas > 100, "the buffer arrives in many small deltas");
    assert_eq!(
        delivered_final(&raw)["payload"].as_str().unwrap().len(),
        200_000
    );
}
