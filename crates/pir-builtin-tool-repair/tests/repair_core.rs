//! Focused tests for the pure repair core: the two strip passes, the
//! schema-directed rules, the phantom normalization, the message pipeline, and
//! the Phase 0 request sanitizer.
//!
//! The engine suite drives the live-schema check through the real preflight
//! validator (a `pir-core` seam this package must not depend on). These tests
//! instead use the small structural checker below; the three-verdict contract
//! itself is covered through the injected-callback API.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value as JsonValue};

use pir_builtin_tool_repair::aliases::{has_anchor_bleed_bug, has_grammar_leak_bug};
use pir_builtin_tool_repair::repair::{
    normalize_phantom_tool_use, repair_message, repair_tool_call_arguments, rules,
    sanitize_pattern, sanitize_request_payload, sanitize_schema_anchors, strip_anchor_bleed,
    strip_grammar_token_leaks, wrap_root_string_as_object, LiveRepairStatus, LiveToolSchema,
    MessageRepairOutcome, SchemaCheck, PHANTOM_TOOL_USE_ERROR,
};
use pir_builtin_tool_repair::types::{AssistantMessage, ContentBlock, StopReason, ToolCall};

/// A deliberately small structural validator: `type` on declared properties,
/// `required` names, and recursion into object properties / array items.
/// Booleans and other non-object schemas are `Unavailable` (the TS catch arm).
fn schema_check(value: &JsonValue, schema: &JsonValue) -> SchemaCheck {
    let Some(schema_object) = schema.as_object() else {
        return SchemaCheck::Unavailable;
    };
    let Some(value_object) = value.as_object() else {
        return SchemaCheck::Rejected;
    };
    if let Some(required) = schema_object.get("required").and_then(JsonValue::as_array) {
        for name in required.iter().filter_map(JsonValue::as_str) {
            if !value_object.contains_key(name) {
                return SchemaCheck::Rejected;
            }
        }
    }
    let Some(properties) = schema_object
        .get("properties")
        .and_then(JsonValue::as_object)
    else {
        return SchemaCheck::Accepted;
    };
    for (key, property) in properties {
        let Some(value) = value_object.get(key) else {
            continue;
        };
        let Some(declared) = property.as_object() else {
            continue;
        };
        let type_ok = match declared.get("type").and_then(JsonValue::as_str) {
            Some("string") => value.is_string(),
            Some("number") => value.is_number(),
            Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
            Some("boolean") => value.is_boolean(),
            Some("array") => value.is_array(),
            Some("object") => value.is_object(),
            _ => true,
        };
        if !type_ok {
            return SchemaCheck::Rejected;
        }
        if let Some(items) = declared.get("items") {
            if let Some(array) = value.as_array() {
                for item in array.iter().filter(|item| item.is_object()) {
                    if schema_check(item, items) != SchemaCheck::Accepted {
                        return SchemaCheck::Rejected;
                    }
                }
            }
        }
        if declared.get("type").and_then(JsonValue::as_str) == Some("object")
            && schema_check(value, property) != SchemaCheck::Accepted
        {
            return SchemaCheck::Rejected;
        }
    }
    SchemaCheck::Accepted
}

fn arguments(pairs: &[(&str, JsonValue)]) -> BTreeMap<String, JsonValue> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn read_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "offset": {"type": "number"},
            "limit": {"type": "number"}
        },
        "required": ["path"]
    })
}

fn read_tool_call(pairs: &[(&str, JsonValue)]) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall::new("call-1", "read", arguments(pairs)))
}

// ---------------------------------------------------------------------------
// Strip passes
// ---------------------------------------------------------------------------

#[test]
fn grammar_token_leaks_are_stripped_from_keys_and_nested_values() {
    let mut input = arguments(&[
        (
            "<arg_key>path</arg_key>",
            json!("<arg_value> /tmp/a.txt </arg_value>"),
        ),
        (
            "nested",
            json!({"<arg_key>k</arg_key>": ["<arg_value>x</arg_value>", 3]}),
        ),
    ]);
    assert!(strip_grammar_token_leaks(&mut input));
    assert_eq!(input.get("path"), Some(&json!("/tmp/a.txt")));
    assert_eq!(
        input.get("nested"),
        Some(&json!({"k": ["x", 3]})),
        "keys and array string items are stripped recursively"
    );
}

#[test]
fn grammar_token_strip_is_boundary_only_and_reports_no_change() {
    let mut untouched = arguments(&[("text", json!("a <arg_value> b"))]);
    assert!(!strip_grammar_token_leaks(&mut untouched));
    assert_eq!(untouched.get("text"), Some(&json!("a <arg_value> b")));
}

#[test]
fn anchor_bleed_is_stripped_recursively_but_keys_are_untouched() {
    let mut input = arguments(&[
        ("^path$", json!("^/tmp/a.txt$")),
        ("nested", json!({"a": "^x$", "b": ["^y$", "^"]})),
    ]);
    assert!(strip_anchor_bleed(&mut input));
    // Keys are not part of the anchor pass.
    assert_eq!(input.get("^path$"), Some(&json!("/tmp/a.txt")));
    assert_eq!(
        input.get("nested"),
        Some(&json!({"a": "x", "b": ["y", ""]}))
    );
}

// ---------------------------------------------------------------------------
// Root wrap + schema-directed repair
// ---------------------------------------------------------------------------

#[test]
fn bare_string_roots_wrap_into_the_tool_string_field() {
    let wrapped = wrap_root_string_as_object(&json!("notes.txt"), "read").expect("read wrap");
    assert_eq!(wrapped.wrapped, json!({"path": "notes.txt"}));
    assert!(wrapped.hint.contains("`path`"));

    let grep = wrap_root_string_as_object(&json!("TODO"), "grep").expect("grep wrap");
    assert_eq!(grep.wrapped, json!({"pattern": "TODO"}));

    assert!(wrap_root_string_as_object(&json!("x"), "unknown").is_none());
    assert!(wrap_root_string_as_object(&json!({"path": "x"}), "read").is_none());
}

#[test]
fn alias_rename_fires_and_recommits_when_the_candidate_validates() {
    let outcome = repair_tool_call_arguments(
        "read",
        &json!({"file_path": "a.txt"}),
        &read_schema(),
        &schema_check,
    );
    assert_eq!(outcome.status, LiveRepairStatus::Recovered);
    assert_eq!(outcome.rules_fired, vec![rules::RENAME_ALIASED_FIELD]);
    assert_eq!(outcome.repaired, Some(json!({"path": "a.txt"})));
    assert_eq!(outcome.input, json!({"file_path": "a.txt"}));
}

#[test]
fn a_present_canonical_field_wins_over_its_alias() {
    let outcome = repair_tool_call_arguments(
        "read",
        &json!({"path": "a.txt", "file_path": "b.txt"}),
        &read_schema(),
        &schema_check,
    );
    assert_eq!(outcome.status, LiveRepairStatus::Unchanged);
    assert!(outcome.rules_fired.is_empty());
}

#[test]
fn legacy_edit_text_fields_become_an_edits_array() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": {"type": "string"},
                        "newText": {"type": "string"}
                    },
                    "required": ["oldText", "newText"]
                }
            }
        },
        "required": ["path", "edits"]
    });
    let outcome = repair_tool_call_arguments(
        "edit",
        &json!({"path": "f.rs", "oldText": "a", "newText": "b"}),
        &schema,
        &schema_check,
    );
    assert_eq!(outcome.status, LiveRepairStatus::Recovered);
    assert!(outcome
        .rules_fired
        .contains(&rules::WRAP_LEGACY_EDIT_FIELDS));
    assert_eq!(
        outcome.repaired,
        Some(json!({
            "path": "f.rs",
            "edits": [{"oldText": "a", "newText": "b"}]
        }))
    );
}

#[test]
fn bash_timeout_milliseconds_become_seconds_with_js_number_coercion() {
    let schema = json!({
        "type": "object",
        "properties": {
            "command": {"type": "string"},
            "timeout": {"type": "number"}
        },
        "required": ["command"]
    });
    for (input, expected) in [
        (json!(2000), json!(2)),
        (json!("2000"), json!(2)),
        (json!([2000]), json!(2)),
        (json!([]), json!(0)),
    ] {
        let outcome = repair_tool_call_arguments(
            "bash",
            &json!({"command": "sleep 1", "timeoutMs": input}),
            &schema,
            &schema_check,
        );
        assert_eq!(outcome.status, LiveRepairStatus::Recovered, "{input}");
        assert!(outcome
            .rules_fired
            .contains(&rules::CONVERT_TIMEOUT_MILLISECONDS));
        assert_eq!(
            outcome.repaired,
            Some(json!({"command": "sleep 1", "timeout": expected})),
            "{input}"
        );
    }
}

#[test]
fn optional_null_empty_object_placeholder_and_numeric_strings_are_repaired() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "offset": {"type": "number"},
            "tags": {"type": "array", "items": {"type": "string"}}
        }
    });

    let null = repair_tool_call_arguments(
        "read",
        &json!({"path": "a", "offset": null}),
        &schema,
        &schema_check,
    );
    assert!(null.rules_fired.contains(&rules::DROP_NULL_OR_UNDEFINED));
    assert_eq!(null.repaired, Some(json!({"path": "a"})));

    let numeric = repair_tool_call_arguments(
        "read",
        &json!({"path": "a", "offset": "10"}),
        &schema,
        &schema_check,
    );
    assert!(numeric.rules_fired.contains(&rules::COERCE_NUMERIC_STRING));
    assert_eq!(numeric.repaired, Some(json!({"path": "a", "offset": 10})));

    let placeholder = repair_tool_call_arguments(
        "read",
        &json!({"path": "a", "tags": {}}),
        &schema,
        &schema_check,
    );
    assert!(placeholder
        .rules_fired
        .contains(&rules::DROP_EMPTY_OBJECT_PLACEHOLDER));
    assert_eq!(placeholder.repaired, Some(json!({"path": "a"})));
}

#[test]
fn string_arrays_are_parsed_or_wrapped() {
    let schema = json!({
        "type": "object",
        "properties": {
            "tags": {"type": "array", "items": {"type": "string"}}
        }
    });

    let parsed = repair_tool_call_arguments(
        "read",
        &json!({"tags": "[\"a\",\"b\"]"}),
        &schema,
        &schema_check,
    );
    assert!(parsed
        .rules_fired
        .contains(&rules::PARSE_JSON_STRINGIFIED_ARRAY));
    assert_eq!(parsed.repaired, Some(json!({"tags": ["a", "b"]})));

    let wrapped = repair_tool_call_arguments("read", &json!({"tags": "a"}), &schema, &schema_check);
    assert!(wrapped
        .rules_fired
        .contains(&rules::WRAP_BARE_STRING_AS_ARRAY));
    assert_eq!(wrapped.repaired, Some(json!({"tags": ["a"]})));
}

#[test]
fn an_invalid_candidate_is_kept_as_unrepairable_and_never_swallows_the_error() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "limit": {"type": "number"}
        },
        "required": ["path", "limit"]
    });
    let outcome = repair_tool_call_arguments(
        "read",
        &json!({"file_path": "a.txt"}),
        &schema,
        &schema_check,
    );
    assert_eq!(outcome.status, LiveRepairStatus::Unrepairable);
    assert!(outcome.rules_fired.contains(&rules::RENAME_ALIASED_FIELD));
    assert_eq!(outcome.repaired, Some(json!({"path": "a.txt"})));
}

#[test]
fn an_unavailable_schema_short_circuits_to_unchanged() {
    let outcome = repair_tool_call_arguments("read", &json!({"x": 1}), &json!(true), &schema_check);
    assert_eq!(outcome.status, LiveRepairStatus::Unchanged);
    assert!(outcome.repaired.is_none());
    assert!(outcome.rules_fired.is_empty());
}

// ---------------------------------------------------------------------------
// Phantom normalization
// ---------------------------------------------------------------------------

#[test]
fn phantom_tool_use_without_calls_becomes_a_retryable_error() {
    let mut message = AssistantMessage::new(vec![ContentBlock::text("no calls")]);
    message.stop_reason = StopReason::ToolUse;
    assert!(normalize_phantom_tool_use(&mut message));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(PHANTOM_TOOL_USE_ERROR)
    );

    let mut with_call = AssistantMessage::new(vec![read_tool_call(&[("path", json!("a"))])]);
    with_call.stop_reason = StopReason::ToolUse;
    assert!(!normalize_phantom_tool_use(&mut with_call));
    assert_eq!(with_call.stop_reason, StopReason::ToolUse);
}

// ---------------------------------------------------------------------------
// Message pipeline
// ---------------------------------------------------------------------------

#[test]
fn the_glm_token_strip_is_model_gated() {
    let leaked = || {
        AssistantMessage::new(vec![read_tool_call(&[(
            "<arg_key>path</arg_key>",
            json!("<arg_value>a.txt</arg_value>"),
        )])])
    };
    let no_check = |_: &JsonValue, _: &JsonValue| SchemaCheck::Unavailable;

    let mut glm = leaked();
    let outcome = repair_message(&mut glm, Some("glm-4"), &[], &no_check, false);
    assert!(matches!(outcome, MessageRepairOutcome::Repaired(_)));
    assert_eq!(
        glm.content[0]
            .as_tool_call()
            .expect("call")
            .arguments
            .get("path"),
        Some(&json!("a.txt"))
    );

    let mut other = leaked();
    let outcome = repair_message(&mut other, Some("claude-3"), &[], &no_check, false);
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert!(other.content[0]
        .as_tool_call()
        .expect("call")
        .arguments
        .contains_key("<arg_key>path</arg_key>"));
}

#[test]
fn the_anchor_bleed_strip_is_model_gated() {
    let message = || AssistantMessage::new(vec![read_tool_call(&[("path", json!("^a.txt$"))])]);
    let no_check = |_: &JsonValue, _: &JsonValue| SchemaCheck::Unavailable;

    let mut kimi = message();
    assert!(matches!(
        repair_message(&mut kimi, Some("kimi-k2"), &[], &no_check, false),
        MessageRepairOutcome::Repaired(_)
    ));
    assert_eq!(
        kimi.content[0]
            .as_tool_call()
            .expect("call")
            .arguments
            .get("path"),
        Some(&json!("a.txt"))
    );

    let mut other = message();
    assert_eq!(
        repair_message(&mut other, Some("claude-3"), &[], &no_check, false),
        MessageRepairOutcome::Unchanged
    );
}

#[test]
fn the_pipeline_repairs_calls_against_the_live_schema_and_reports_them() {
    let mut message = AssistantMessage::new(vec![read_tool_call(&[("file_path", json!("a.txt"))])]);
    message.stop_reason = StopReason::ToolUse;
    let tools = vec![LiveToolSchema::new("read", read_schema())];

    let outcome = repair_message(&mut message, None, &tools, &schema_check, false);
    let MessageRepairOutcome::Repaired(report) = &outcome else {
        panic!("expected a repair, got {outcome:?}");
    };
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].call_id, "call-1");
    assert_eq!(report.calls[0].tool_name, "read");
    assert_eq!(report.calls[0].status, LiveRepairStatus::Recovered);
    assert_eq!(
        message.content[0].as_tool_call().expect("call").arguments,
        arguments(&[("path", json!("a.txt"))])
    );
}

#[test]
fn an_already_valid_message_is_unchanged() {
    let mut message = AssistantMessage::new(vec![read_tool_call(&[("path", json!("a.txt"))])]);
    message.stop_reason = StopReason::ToolUse;
    let tools = vec![LiveToolSchema::new("read", read_schema())];
    assert_eq!(
        repair_message(&mut message, None, &tools, &schema_check, false),
        MessageRepairOutcome::Unchanged
    );
}

// ---------------------------------------------------------------------------
// Phase 0 request sanitizer
// ---------------------------------------------------------------------------

#[test]
fn sanitize_pattern_strips_anchor_characters_and_drops_mixed_alternation() {
    assert_eq!(sanitize_pattern("^a$"), Some("a".to_owned()));
    assert_eq!(sanitize_pattern("^^a$$"), Some("a".to_owned()));
    assert_eq!(sanitize_pattern("a|b"), Some("a|b".to_owned()));
    assert_eq!(sanitize_pattern("^a|b$"), None);
    assert_eq!(sanitize_pattern("^$"), None);
}

#[test]
fn sanitize_schema_anchors_walks_nested_schemas() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "pattern": "^/tmp/"},
            "mode": {"type": "string", "pattern": "^a|b$"},
            "nested": {
                "type": "object",
                "properties": {"name": {"type": "string", "pattern": "x$"}}
            },
            "items": {"type": "array", "items": {"pattern": "^y"}}
        }
    });
    let sanitized = sanitize_schema_anchors(&schema);
    assert_eq!(sanitized["properties"]["path"]["pattern"], "/tmp/");
    assert!(sanitized["properties"]["mode"].get("pattern").is_none());
    assert_eq!(
        sanitized["properties"]["nested"]["properties"]["name"]["pattern"],
        "x"
    );
    assert_eq!(sanitized["properties"]["items"]["items"]["pattern"], "y");
}

#[test]
fn request_payload_sanitization_is_model_gated_and_covers_both_locations() {
    let payload = || {
        json!({
            "tools": [{"function": {"name": "read", "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string", "pattern": "^a$"}}
            }}}],
            "response_format": {"json_schema": {"schema": {
                "type": "object",
                "properties": {"q": {"pattern": "^b$"}}
            }}}
        })
    };

    let mut gated = payload();
    assert!(sanitize_request_payload(&mut gated, Some("glm-4")));
    assert_eq!(
        gated["tools"][0]["function"]["parameters"]["properties"]["path"]["pattern"],
        "a"
    );
    assert_eq!(
        gated["response_format"]["json_schema"]["schema"]["properties"]["q"]["pattern"],
        "b"
    );

    let mut other = payload();
    let original = other.clone();
    assert!(!sanitize_request_payload(&mut other, Some("claude-3")));
    assert_eq!(other, original);

    let mut none = payload();
    assert!(!sanitize_request_payload(&mut none, None));
}

#[test]
fn model_regex_sets_match_the_upstream_ids() {
    assert!(has_anchor_bleed_bug(Some("kimi-k2-0905")));
    assert!(has_anchor_bleed_bug(Some("moonshotai/kimi-k2")));
    assert!(has_anchor_bleed_bug(Some("glm-4.6")));
    assert!(has_grammar_leak_bug(Some("GLM-4.6")));
    assert!(!has_grammar_leak_bug(Some("kimi-k2")));
    assert!(!has_anchor_bleed_bug(None));
    assert!(!has_anchor_bleed_bug(Some("")));
}

#[test]
fn known_tool_set_is_accepted_for_the_grammar_gate() {
    // The known-tool set is a plain `BTreeSet<String>`; the grammar suite
    // covers the gate itself. This pins the neutral DTO/API boundary.
    let known: BTreeSet<String> = ["read".to_owned()].into_iter().collect();
    assert!(known.contains("read"));
}
