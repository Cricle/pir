//! The `tool-repair` builtin's pure repair core (Step 1a): the README rule
//! table, the two model-gated in-place strip passes, the phantom-toolUse
//! normalization, and the never-swallow fallback.
//!
//! ## How the schemas and the validator are sourced
//!
//! Two things make this suite exercise the real thing rather than a fixture:
//!
//! - **Schemas come from the real core tools**: [`register_core_tools`] builds
//!   the production registry, and each test reads `AgentTool::parameters()` off
//!   it — the exact schemas the Step 1b wiring will pass in. (The JS package
//!   saw `parameters: null` for core tools, so this is also the regression
//!   guard for the "repair actually engages" net gain.)
//! - **Validation goes through the real preflight validator**: the core takes
//!   its live-schema check as an injected callback (the `SchemaCheck` seam),
//!   and this suite injects a bridge that runs
//!   [`ToolScheduler::execute_batch`] on a probe tool carrying the schema. That
//!   preflight path calls `validate_schema_value` in
//!   `src/agent/tool_scheduler/prepare.rs` — the same function
//!   `validate_arguments` uses — so no test-side validator exists to drift.
//!   (Direct reuse is impossible today: that module, function, and its error
//!   type are module-private. Step 1b exposes them; see the `repair` module
//!   docs.)

#![cfg(feature = "tool-repair")]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value as JsonValue};

use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::ToolScheduler;
use pir::ai::types::{AssistantMessage, ContentBlock, StopReason, ToolCall};
use pir::coding_agent::tools::register_core_tools;
use pir::plugins::tool_repair::aliases::{
    has_anchor_bleed_bug, has_grammar_leak_bug, string_arg_tool, WrapShape, GRAMMAR_NAMES,
};
use pir::plugins::tool_repair::config::{GrammarRepairConfig, GrammarRepairMode, ToolRepairConfig};
use pir::plugins::tool_repair::repair::{
    normalize_phantom_tool_use, repair_message, repair_tool_call_arguments, strip_anchor_bleed,
    strip_grammar_token_leaks, wrap_root_string_as_object, LiveRepairStatus, LiveToolRepair,
    LiveToolSchema, MessageRepairOutcome, SchemaCheck, PHANTOM_TOOL_USE_ERROR,
};

// ---------------------------------------------------------------------------
// The real-validator bridge
// ---------------------------------------------------------------------------

/// A probe tool whose only purpose is to carry a schema through preflight.
struct ProbeTool {
    schema: JsonValue,
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe_tool"
    }

    fn description(&self) -> &'static str {
        "tool-repair validator bridge probe"
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(self.schema.clone())
    }

    fn run(&self, _args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ran") })
    }
}

fn bridge_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    // Multi-thread: the libtest harness runs `#[test]` functions on parallel
    // threads, and only a multi-thread runtime supports concurrent `block_on`
    // calls on one shared runtime.
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

/// The production preflight verdict for `arguments` against `schema`, obtained
/// through the public scheduler API so it runs the real
/// `validate_schema_value` (`src/agent/tool_scheduler/prepare.rs:234`).
fn real_schema_check(arguments: &JsonValue, schema: &JsonValue) -> SchemaCheck {
    if !schema.is_object() {
        // No checkable schema is the TS `Value.Check` throw arm
        // (`src/index.ts:674-680`).
        return SchemaCheck::Unavailable;
    }
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ProbeTool {
        schema: schema.clone(),
    }));
    let outcome = bridge_runtime().block_on(async {
        let context = ToolContext::new(".");
        let call = AgentToolCall::new(0, "probe-call", "probe_tool", arguments.clone());
        ToolScheduler::default()
            .execute_batch(vec![call], &registry, &context, None)
            .await
    });
    match outcome.results.first().map(|finalized| &finalized.result) {
        Some(result) if !result.is_error => SchemaCheck::Accepted,
        Some(result)
            if result
                .content
                .starts_with("Validation failed for tool \"probe_tool\":") =>
        {
            SchemaCheck::Rejected
        }
        // An invalid schema ("has an invalid argument schema") or any other
        // preflight failure: no verdict, like the TS `catch`.
        _ => SchemaCheck::Unavailable,
    }
}

/// [`repair_tool_call_arguments`] with the real validator injected.
fn repair_call(tool_name: &str, arguments: &JsonValue, schema: &JsonValue) -> LiveToolRepair {
    let check = |arguments: &JsonValue, schema: &JsonValue| real_schema_check(arguments, schema);
    repair_tool_call_arguments(tool_name, arguments, schema, &check)
}

/// [`repair_message`] with the real validator injected (debug off).
fn repair_with_real_validator(
    message: &mut AssistantMessage,
    model: Option<&str>,
    tools: &[LiveToolSchema],
) -> MessageRepairOutcome {
    let check = |arguments: &JsonValue, schema: &JsonValue| real_schema_check(arguments, schema);
    repair_message(message, model, tools, &check, false)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The real core-tool schemas, straight off the production registry.
fn core_tool_schemas() -> Vec<LiveToolSchema> {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let mut tools: Vec<LiveToolSchema> = registry
        .iter()
        .map(|tool| LiveToolSchema::new(tool.name(), tool.parameters().unwrap_or(JsonValue::Null)))
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// One real core tool's schema.
fn core_schema(name: &str) -> JsonValue {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
        .get(name)
        .and_then(|tool| tool.parameters())
        .unwrap_or(JsonValue::Null)
}

/// `fabric_exec` is Fabric-owned (a JS extension tool), so it has no Rust
/// schema; this mirrors the shape the package repairs against
/// (`src/index.ts:772-786` requires `properties.code.type === "string"`).
fn fabric_exec_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "code": { "type": "string", "description": "TypeScript code" }
        },
        "required": ["code"]
    })
}

/// A synthetic schema for the array rules whose item type is a string: no
/// pir core tool declares an array-of-strings field (`edit.edits` items are
/// objects), while the README row ("bare string where an array is expected")
/// needs one.
fn string_array_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {
            "include": { "type": "array", "items": { "type": "string" } }
        },
        "required": []
    })
}

fn tool_call_block(id: &str, name: &str, arguments: JsonValue) -> ContentBlock {
    let map: BTreeMap<String, JsonValue> = arguments
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: map,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn call_arguments(block: &ContentBlock) -> JsonValue {
    match block {
        ContentBlock::ToolCall(call) => {
            JsonValue::Object(call.arguments.clone().into_iter().collect())
        }
        other => panic!("expected a tool call block, got {other:?}"),
    }
}

fn assistant_message(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        stop_reason,
        model: "test-model".to_owned(),
        ..AssistantMessage::default()
    }
}

fn rules_of(repair: &LiveToolRepair) -> Vec<&'static str> {
    repair.rules_fired.clone()
}

/// A JSON object as the `BTreeMap` the in-place strip passes take.
fn arguments_map(arguments: &JsonValue) -> BTreeMap<String, JsonValue> {
    arguments
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// The strip passes' map back as JSON for assertions.
fn map_json(arguments: &BTreeMap<String, JsonValue>) -> JsonValue {
    JsonValue::Object(arguments.clone().into_iter().collect())
}

// ---------------------------------------------------------------------------
// README table, row by row
// ---------------------------------------------------------------------------

#[test]
fn optional_null_field_dropped() {
    let schema = core_schema("read");
    let call = repair_call("read", &json!({ "path": "/foo", "offset": null }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "path": "/foo" })));
    assert_eq!(rules_of(&call), vec!["dropNullOrUndefined"]);
}

#[test]
fn json_stringified_array_parsed() {
    let schema = core_schema("edit");
    let call = repair_call(
        "edit",
        &json!({ "path": "/x", "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]" }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "path": "/x", "edits": [{ "oldText": "a", "newText": "b" }] }))
    );
    assert_eq!(rules_of(&call), vec!["parseJsonStringifiedArray"]);
}

#[test]
fn empty_object_placeholder_dropped_from_array_field() {
    // A synthetic optional array field: dropping the placeholder leaves a
    // valid call.
    let optional_array_schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "items": { "type": "array", "items": { "type": "object" } }
        },
        "required": ["path"]
    });
    let call = repair_call(
        "demo_tool",
        &json!({ "path": "/x", "items": {} }),
        &optional_array_schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "path": "/x" })));
    assert_eq!(rules_of(&call), vec!["dropEmptyObjectPlaceholder"]);

    // The real edit schema now REQUIRES `edits` (upstream edit.ts:32-41), so
    // dropping its empty-object placeholder cannot produce a valid call.
    let schema = core_schema("edit");
    let call = repair_call("edit", &json!({ "path": "/x", "edits": {} }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Unrepairable);
}

#[test]
fn bare_string_wrapped_as_array() {
    let schema = string_array_schema();
    let call = repair_call("demo_tool", &json!({ "include": "foo" }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "include": ["foo"] })));
    assert_eq!(rules_of(&call), vec!["wrapBareStringAsArray"]);
}

#[test]
fn wrong_field_name_renamed() {
    let schema = core_schema("read");
    let call = repair_call("read", &json!({ "file_path": "/foo" }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "path": "/foo" })));
    assert_eq!(rules_of(&call), vec!["renameAliasedField"]);
}

#[test]
fn numeric_string_coerced_for_number_field() {
    // grep.limit is `type: "number"` in the real schema (as are read's
    // offset/limit upstream), and the TS-faithful rule coerces a numeric
    // string for it.
    let schema = core_schema("grep");
    let call = repair_call("grep", &json!({ "pattern": "x", "limit": "20" }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "pattern": "x", "limit": 20 })));
    assert_eq!(rules_of(&call), vec!["coerceNumericString"]);
}

#[test]
fn integer_typed_field_is_not_coerced_by_the_repair_rule() {
    // The faithful `type === "number"` gate (upstream `src/index.ts:804-818`
    // numeric coercion): a numeric string for an INTEGER-typed field is not
    // touched by the REPAIR package. The integer arm lives in the preflight
    // (`validate_arguments` applies upstream `Value.Convert`'s integer rule),
    // so the call still validates — but no repair rule fires and `repaired`
    // stays `None`: the hook contributes nothing here.
    let schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "count": { "type": "integer" }
        },
        "required": ["path"]
    });
    let call = repair_call(
        "demo_tool",
        &json!({ "path": "/x", "count": "20" }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Unchanged);
    assert!(call.rules_fired.is_empty(), "{call:?}");
    assert_eq!(call.repaired, None, "{call:?}");
    assert_eq!(call.input, json!({ "path": "/x", "count": "20" }));
}

#[test]
fn numeric_string_coerced_for_read_offset_and_limit() {
    // P2a: upstream declares `read.offset`/`read.limit` as `Type.Number`
    // (`core/tools/read.ts:16-17`), so the numeric-string repair rule must
    // reach them. While the schema said `"integer"` a provider that emitted
    // `"5"` was rejected at preflight and cost a turn.
    let schema = core_schema("read");
    let call = repair_call(
        "read",
        &json!({ "path": "/x", "offset": "5", "limit": "20" }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "path": "/x", "offset": 5, "limit": 20 }))
    );
    assert_eq!(rules_of(&call), vec!["coerceNumericString"]);
}

#[test]
fn bare_string_root_wrapped_as_object() {
    let schema = core_schema("read");
    let call = repair_call("read", &json!("/path/to/file"), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "path": "/path/to/file" })));
    assert_eq!(rules_of(&call), vec!["wrapRootStringAsObject"]);
}

#[test]
fn bare_string_root_for_unknown_tool_is_unrepairable() {
    let call = repair_call("demo_tool", &json!("hello"), &string_array_schema());
    assert_eq!(call.status, LiveRepairStatus::Unrepairable);
    assert_eq!(call.input, json!("hello"));
    assert!(call.repaired.is_none());
    assert!(call.rules_fired.is_empty());
}

#[test]
fn fabric_exec_code_array_joined() {
    let schema = fabric_exec_schema();
    let call = repair_call(
        "fabric_exec",
        &json!({ "code": ["const x=1;", "return x;"] }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "code": "const x=1;\nreturn x;" }))
    );
    assert_eq!(rules_of(&call), vec!["joinStringArray"]);
}

#[test]
fn edit_legacy_pair_becomes_edits_array() {
    // The real edit schema no longer requires `edits`, so the legacy pair is
    // *schema-valid* input: the repair pass still runs on accepted input (the
    // TS `repairObjectFromSchema` has no `initial` guard) and rewrites it.
    let schema = core_schema("edit");
    let call = repair_call(
        "edit",
        &json!({ "path": "/x", "oldText": "a", "newText": "b" }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "path": "/x", "edits": [{ "oldText": "a", "newText": "b" }] }))
    );
    assert_eq!(rules_of(&call), vec!["wrapLegacyEditFields"]);
}

#[test]
fn bash_timeout_ms_converted_to_seconds() {
    let schema = core_schema("bash");
    let call = repair_call(
        "bash",
        &json!({ "command": "ls", "timeoutMs": 5000 }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "command": "ls", "timeout": 5 }))
    );
    assert_eq!(rules_of(&call), vec!["convertTimeoutMilliseconds"]);
}

#[test]
fn bash_timeout_ms_fractional_stays_fractional() {
    let schema = core_schema("bash");
    let call = repair_call(
        "bash",
        &json!({ "command": "ls", "timeoutMs": 1500 }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "command": "ls", "timeout": 1.5 }))
    );
}

#[test]
fn anchor_bleed_stripped_for_gated_model() {
    let mut message = assistant_message(
        vec![
            tool_call_block("c1", "grep", json!({ "pattern": "^foo$" })),
            tool_call_block("c2", "write", json!({ "path": "/x", "content": "^a$" })),
        ],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(
        &mut message,
        Some("moonshotai/kimi-k2-instruct"),
        &core_tool_schemas(),
    );
    assert!(outcome.changed(), "anchor bleed must commit in place");
    assert!(outcome
        .report()
        .is_some_and(|report| report.calls.is_empty()));
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "pattern": "foo" })
    );
    assert_eq!(
        call_arguments(&message.content[1]),
        json!({ "path": "/x", "content": "a" })
    );
}

#[test]
fn anchor_bleed_untouched_for_ungated_model() {
    let mut message = assistant_message(
        vec![tool_call_block("c1", "grep", json!({ "pattern": "^foo$" }))],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, Some("gpt-5"), &core_tool_schemas());
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "pattern": "^foo$" })
    );
}

#[test]
fn glm_token_leak_stripped_for_glm_model() {
    let mut message = assistant_message(
        vec![tool_call_block(
            "c1",
            "read",
            json!({ "<arg_key>path</arg_key>": "<arg_value>/foo</arg_value>" }),
        )],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, Some("glm-4.6"), &core_tool_schemas());
    assert!(outcome.changed(), "token leak must commit in place");
    // The strip makes an otherwise unrecoverable call valid for the real read
    // schema (missing required `path`).
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "path": "/foo" })
    );
}

#[test]
fn glm_token_leak_untouched_for_non_glm_model() {
    let original = json!({ "<arg_key>path</arg_key>": "<arg_value>/foo</arg_value>" });
    let mut message = assistant_message(
        vec![tool_call_block("c1", "read", original.clone())],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, Some("gpt-5"), &core_tool_schemas());
    // The strip is model-gated, and the tagged keys repair nothing on the real
    // schema: an unrepairable call keeps its original arguments bit for bit.
    assert!(matches!(outcome, MessageRepairOutcome::Unrepairable(_)));
    assert_eq!(call_arguments(&message.content[0]), original);
}

#[test]
fn phantom_tool_use_normalized_to_error() {
    let mut message = assistant_message(
        vec![ContentBlock::Text(pir::ai::types::TextContent {
            text: "thinking out loud".to_owned(),
            text_signature: None,
        })],
        StopReason::ToolUse,
    );
    let outcome = repair_with_real_validator(&mut message, None, &core_tool_schemas());
    assert!(outcome.changed());
    assert!(outcome
        .report()
        .is_some_and(|report| report.phantom_normalized));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(PHANTOM_TOOL_USE_ERROR)
    );
    assert_eq!(
        PHANTOM_TOOL_USE_ERROR,
        "stream ended before tool_calls were received (vLLM phantom tool_use)"
    );
}

#[test]
fn phantom_normalization_skipped_when_a_tool_call_exists() {
    let mut message = assistant_message(
        vec![tool_call_block("c1", "read", json!({ "path": "/x" }))],
        StopReason::ToolUse,
    );
    let outcome = repair_with_real_validator(&mut message, None, &core_tool_schemas());
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert!(message.error_message.is_none());
}

#[test]
fn unrepairable_keeps_arguments_byte_identical() {
    let original = json!({ "file_path": "/foo", "offset": { "n": 5 } });
    let mut message = assistant_message(
        vec![tool_call_block("c1", "read", original.clone())],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, None, &core_tool_schemas());
    let report = outcome
        .report()
        .expect("an attempted repair is reported even when rejected");
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].status, LiveRepairStatus::Unrepairable);
    // The alias rename fired (`file_path` → `path`), but the candidate still
    // fails on read's number-typed `offset` (no rule turns an object into a
    // number); the committed message must keep the provider's original
    // arguments so the host surfaces the real error.
    assert_eq!(report.calls[0].rules_fired, vec!["renameAliasedField"]);
    assert_eq!(call_arguments(&message.content[0]), original);
    assert_eq!(
        serde_json::to_string(&call_arguments(&message.content[0])).unwrap(),
        serde_json::to_string(&original).unwrap()
    );
}

#[test]
fn alias_canonical_wins_over_alias_spelling() {
    let schema = core_schema("read");
    let call = repair_call(
        "read",
        &json!({ "file_path": "/alias", "path": "/canonical" }),
        &schema,
    );
    // Already schema-valid, and the canonical value is never overwritten by
    // the alias: the call stays untouched.
    assert_eq!(call.status, LiveRepairStatus::Unchanged);
    assert_eq!(
        call.input,
        json!({ "file_path": "/alias", "path": "/canonical" })
    );
    assert!(call.rules_fired.is_empty());
}

// ---------------------------------------------------------------------------
// Early exit, purity, and the validator seam
// ---------------------------------------------------------------------------

#[test]
fn message_without_tool_calls_does_no_work() {
    let mut message = assistant_message(
        vec![ContentBlock::Text(pir::ai::types::TextContent {
            text: "no tools here".to_owned(),
            text_signature: None,
        })],
        StopReason::Stop,
    );
    let before = message.clone();
    let tools = core_tool_schemas();
    let check = |_: &JsonValue, _: &JsonValue| -> SchemaCheck {
        panic!("the validator must not run without tool call blocks")
    };
    let outcome = repair_message(&mut message, Some("glm-4.6"), &tools, &check, false);
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(message, before);
}

#[test]
fn call_without_a_live_schema_does_no_work() {
    let mut message = assistant_message(
        vec![tool_call_block(
            "c1",
            "some_extension_tool",
            json!({ "x": 1 }),
        )],
        StopReason::Stop,
    );
    let check = |_: &JsonValue, _: &JsonValue| -> SchemaCheck {
        panic!("tools without a live schema must be skipped")
    };
    let outcome = repair_message(&mut message, None, &[], &check, false);
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
}

#[test]
fn valid_arguments_are_left_untouched() {
    let mut message = assistant_message(
        vec![tool_call_block(
            "c1",
            "read",
            json!({ "path": "/x", "limit": 10 }),
        )],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, None, &core_tool_schemas());
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "path": "/x", "limit": 10 })
    );
}

#[test]
fn unavailable_schema_check_passes_the_call_through() {
    let schema = core_schema("read");
    let check = |_: &JsonValue, _: &JsonValue| SchemaCheck::Unavailable;
    let call = repair_tool_call_arguments("read", &json!({ "file_path": "/foo" }), &schema, &check);
    // The TS `catch` arm: no verdict means no repair, not an unrepairable
    // report.
    assert_eq!(call.status, LiveRepairStatus::Unchanged);
    assert_eq!(call.input, json!({ "file_path": "/foo" }));
}

#[test]
fn phantom_normalization_is_directly_callable() {
    let mut message = assistant_message(vec![], StopReason::ToolUse);
    assert!(normalize_phantom_tool_use(&mut message));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(PHANTOM_TOOL_USE_ERROR)
    );
    // Idempotent: the second call sees stop_reason `error`.
    assert!(!normalize_phantom_tool_use(&mut message));
}

// ---------------------------------------------------------------------------
// Config (Step 2 knobs, parsed now)
// ---------------------------------------------------------------------------

#[test]
fn default_config_matches_the_package_defaults() {
    let config = ToolRepairConfig::default();
    assert!(!config.debug);
    assert!(!config.grammar.enabled);
    assert_eq!(config.grammar.mode, GrammarRepairMode::Recover);
    assert!(config.grammar.require_known_tool);
    assert!(!config.grammar.debug);
    assert_eq!(config.grammar.grammars, GRAMMAR_NAMES.to_vec());
    assert!(config.grammar.leak_models.is_empty());
}

#[test]
fn grammar_config_normalizes_the_package_file_shape() {
    let config = GrammarRepairConfig::from_json(&json!({
        "grammarRepair": {
            "enabled": true,
            "mode": "strip",
            "requireKnownTool": false,
            "debug": true,
            "grammars": ["dsml", "bogus", "olmo"],
            "leakModels": ["kimi", "("]
        }
    }));
    assert!(config.enabled);
    assert_eq!(config.mode, GrammarRepairMode::Strip);
    assert!(!config.require_known_tool);
    assert!(config.debug);
    // Unknown families are dropped; an uncompilable leak pattern is ignored.
    assert_eq!(config.grammars, vec!["dsml", "olmo"]);
    assert_eq!(config.leak_models, vec!["kimi".to_owned()]);
}

#[test]
fn grammar_config_accepts_the_bare_form_and_keeps_defaults() {
    let config = GrammarRepairConfig::from_json(&json!({ "mode": "strip" }));
    assert!(!config.enabled);
    assert_eq!(config.mode, GrammarRepairMode::Strip);
    assert!(config.require_known_tool);
    assert_eq!(config.grammars, GRAMMAR_NAMES.to_vec());
}

// ---------------------------------------------------------------------------
// Rule order, the remaining README rows, and rule gates
// ---------------------------------------------------------------------------

#[test]
fn multi_rule_calls_fire_in_the_ts_order() {
    let schema = core_schema("read");
    let call = repair_call(
        "read",
        &json!({ "file_path": "/foo", "offset": null }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "path": "/foo" })));
    // Alias renames run before the per-property rules (`src/index.ts:721-733`
    // then `:788-802`); the canonical `offset` being present stops the alias
    // pass from touching it, so the null drop is what removes it.
    assert_eq!(
        rules_of(&call),
        vec!["renameAliasedField", "dropNullOrUndefined"]
    );
    assert_eq!(call.hints.len(), 2);
}

#[test]
fn alias_rename_then_numeric_coercion_for_ls_limit() {
    // `max` → `limit` (an alias) leaves a numeric string behind, which the
    // per-property coercion then converts: two rules, alias first.
    let schema = core_schema("ls");
    let call = repair_call("ls", &json!({ "max": "20" }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "limit": 20 })));
    assert_eq!(
        rules_of(&call),
        vec!["renameAliasedField", "coerceNumericString"]
    );
}

#[test]
fn grep_query_and_bash_cmd_aliases_rename() {
    let grep = repair_call("grep", &json!({ "query": "fn main" }), &core_schema("grep"));
    assert_eq!(grep.status, LiveRepairStatus::Recovered);
    assert_eq!(grep.repaired, Some(json!({ "pattern": "fn main" })));
    assert_eq!(rules_of(&grep), vec!["renameAliasedField"]);

    let bash = repair_call("bash", &json!({ "cmd": "ls -la" }), &core_schema("bash"));
    assert_eq!(bash.status, LiveRepairStatus::Recovered);
    assert_eq!(bash.repaired, Some(json!({ "command": "ls -la" })));
    assert_eq!(rules_of(&bash), vec!["renameAliasedField"]);
}

#[test]
fn empty_or_null_alias_values_are_not_usable() {
    // `firstUsableField` skips `""` and `null` (`src/index.ts:692-703`), so
    // neither spelling rescues the missing required `path`.
    let schema = core_schema("read");
    let empty = repair_call("read", &json!({ "file_path": "", "offset": 5 }), &schema);
    assert_eq!(empty.status, LiveRepairStatus::Unrepairable);
    assert!(empty.rules_fired.is_empty());
    assert_eq!(empty.repaired, None);

    let null = repair_call("read", &json!({ "file_path": null, "offset": 5 }), &schema);
    assert_eq!(null.status, LiveRepairStatus::Unrepairable);
    assert!(null.rules_fired.is_empty());
    assert_eq!(null.repaired, None);
}

#[test]
fn unrepairable_report_keeps_the_rejected_candidate() {
    // TS keeps `repaired` on `unrepairable` outcomes (`src/index.ts:908-915`)
    // while `input` stays the raw payload; only the message commit is dropped.
    let schema = core_schema("read");
    let original = json!({ "file_path": "/foo", "limit": [1, 2] });
    let call = repair_call("read", &original, &schema);
    assert_eq!(call.status, LiveRepairStatus::Unrepairable);
    assert_eq!(
        call.repaired,
        Some(json!({ "path": "/foo", "limit": [1, 2] }))
    );
    assert_eq!(call.input, original);
    assert_eq!(rules_of(&call), vec!["renameAliasedField"]);
}

#[test]
fn duplicate_rules_are_deduplicated_but_hints_are_not() {
    let schema = json!({
        "type": "object",
        "properties": {
            "first": { "type": "array", "items": { "type": "string" } },
            "second": { "type": "array", "items": { "type": "string" } }
        },
        "required": []
    });
    let call = repair_call("demo_tool", &json!({ "first": {}, "second": {} }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({})));
    // `addRule` dedupes the name, always pushes the hint (`src/index.ts:682-690`).
    assert_eq!(rules_of(&call), vec!["dropEmptyObjectPlaceholder"]);
    assert_eq!(call.hints.len(), 2);
}

#[test]
fn non_empty_object_is_not_dropped_from_an_array_field() {
    let call = repair_call(
        "demo_tool",
        &json!({ "include": { "a": 1 } }),
        &string_array_schema(),
    );
    assert_eq!(call.status, LiveRepairStatus::Unrepairable);
    assert!(call.rules_fired.is_empty());
    assert_eq!(call.repaired, None);
}

#[test]
fn empty_object_placeholder_is_gated_to_array_fields() {
    // The same `{}` on an object-typed property is never dropped
    // (`src/index.ts:820-830` lives in the array branch).
    let schema = json!({
        "type": "object",
        "properties": {
            "config": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        "required": ["config"]
    });
    let call = repair_call("demo_tool", &json!({ "config": {} }), &schema);
    assert_eq!(call.status, LiveRepairStatus::Unrepairable);
    assert!(call.rules_fired.is_empty());
    assert_eq!(call.repaired, None);
}

#[test]
fn edit_item_fields_recurse_through_the_items_schema() {
    // Array items are repaired against the item schema with an indexed path
    // (`src/index.ts:843-859`).
    let schema = core_schema("edit");
    let call = repair_call(
        "edit",
        &json!({ "path": "/x", "edits": [{ "old_string": "a", "new_string": "b" }] }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "path": "/x", "edits": [{ "oldText": "a", "newText": "b" }] }))
    );
    assert_eq!(rules_of(&call), vec!["renameAliasedField"]);
    assert_eq!(call.hints.len(), 2);
    assert!(call.hints[0].contains("edits.0.old_string"));
    assert!(call.hints[0].contains("edits.0.oldText"));
    assert!(call.hints[1].contains("edits.0.new_string"));
    assert!(call.hints[1].contains("edits.0.newText"));
}

#[test]
fn nested_object_properties_recurse_with_paths() {
    let schema = json!({
        "type": "object",
        "properties": {
            "options": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer" }
                },
                "required": ["path"]
            }
        },
        "required": ["options"]
    });
    let call = repair_call(
        "read",
        &json!({ "options": { "file_path": "/foo" } }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(
        call.repaired,
        Some(json!({ "options": { "path": "/foo" } }))
    );
    assert_eq!(rules_of(&call), vec!["renameAliasedField"]);
    assert!(call.hints[0].contains("options.file_path"));
    assert!(call.hints[0].contains("options.path"));
}

#[test]
fn fabric_exec_code_join_requires_all_strings() {
    let call = repair_call(
        "fabric_exec",
        &json!({ "code": ["const x=1;", 2] }),
        &fabric_exec_schema(),
    );
    assert_eq!(call.status, LiveRepairStatus::Unrepairable);
    assert!(call.rules_fired.is_empty());
    assert_eq!(call.repaired, None);
}

#[test]
fn fabric_exec_empty_code_array_joins_to_an_empty_string() {
    // `[].every(...)` is true in JS, so the rule fires and `[].join("\n")`
    // produces `""` (`src/index.ts:772-786`).
    let call = repair_call("fabric_exec", &json!({ "code": [] }), &fabric_exec_schema());
    assert_eq!(call.status, LiveRepairStatus::Recovered);
    assert_eq!(call.repaired, Some(json!({ "code": "" })));
    assert_eq!(rules_of(&call), vec!["joinStringArray"]);
}

#[test]
fn bash_timeout_ms_is_skipped_when_timeout_is_present() {
    let schema = core_schema("bash");
    let call = repair_call(
        "bash",
        &json!({ "command": "ls", "timeoutMs": 5000, "timeout": 10 }),
        &schema,
    );
    assert_eq!(call.status, LiveRepairStatus::Unchanged);
    assert!(call.rules_fired.is_empty());
    assert_eq!(call.repaired, None);
}

#[test]
fn bash_timeout_ms_non_numeric_is_left_alone() {
    let call = repair_call(
        "bash",
        &json!({ "command": "ls", "timeoutMs": "soon" }),
        &core_schema("bash"),
    );
    assert_eq!(call.status, LiveRepairStatus::Unchanged);
    assert!(call.rules_fired.is_empty());
}

#[test]
fn bash_timeout_ms_follows_js_number_coercion() {
    // `Number([x])` is `Number(x)` and `Number([])` is `0`; the TS guard is
    // `Number.isFinite(Number(timeoutMs))`, not a typeof check.
    let schema = core_schema("bash");
    let single = repair_call(
        "bash",
        &json!({ "command": "ls", "timeoutMs": [2500] }),
        &schema,
    );
    assert_eq!(single.status, LiveRepairStatus::Recovered);
    assert_eq!(
        single.repaired,
        Some(json!({ "command": "ls", "timeout": 2.5 }))
    );

    let empty = repair_call(
        "bash",
        &json!({ "command": "ls", "timeoutMs": [] }),
        &schema,
    );
    assert_eq!(empty.status, LiveRepairStatus::Recovered);
    assert_eq!(
        empty.repaired,
        Some(json!({ "command": "ls", "timeout": 0 }))
    );
}

#[test]
fn numeric_string_radix_forms_are_coerced() {
    // JS `Number("0x10"/"0b101"/"0o17")` is 16/5/15 and all are finite.
    let schema = core_schema("grep");
    let hex = repair_call("grep", &json!({ "pattern": "x", "limit": "0x10" }), &schema);
    assert_eq!(hex.status, LiveRepairStatus::Recovered);
    assert_eq!(hex.repaired, Some(json!({ "pattern": "x", "limit": 16 })));

    let binary = repair_call(
        "grep",
        &json!({ "pattern": "x", "limit": "0b101" }),
        &schema,
    );
    assert_eq!(binary.repaired, Some(json!({ "pattern": "x", "limit": 5 })));

    let octal = repair_call("grep", &json!({ "pattern": "x", "limit": "0o17" }), &schema);
    assert_eq!(octal.repaired, Some(json!({ "pattern": "x", "limit": 15 })));
}

// ---------------------------------------------------------------------------
// The two in-place strip passes, directly
// ---------------------------------------------------------------------------

#[test]
fn anchor_strip_touches_values_only() {
    let mut arguments = arguments_map(&json!({
        "^key$": "^foo$",
        "nested": {
            "list": ["^^x$$", "keep"],
            "inner": { "value": "$$y$" }
        }
    }));
    assert!(strip_anchor_bleed(&mut arguments));
    assert_eq!(
        map_json(&arguments),
        json!({
            "^key$": "foo",
            "nested": {
                "list": ["x", "keep"],
                "inner": { "value": "$$y" }
            }
        })
    );

    let mut clean = arguments_map(&json!({ "path": "/x" }));
    assert!(!strip_anchor_bleed(&mut clean));
}

#[test]
fn grammar_token_strip_rewrites_tagged_keys_and_values() {
    let mut arguments = arguments_map(&json!({
        "<arg_key>path</arg_key>": "<arg_value>  /foo  </arg_value>",
        "nested": { "list": ["<arg_value>a</arg_value>", "plain"] }
    }));
    assert!(strip_grammar_token_leaks(&mut arguments));
    // The tagged value is trimmed only because a boundary tag was stripped.
    assert_eq!(
        map_json(&arguments),
        json!({ "path": "/foo", "nested": { "list": ["a", "plain"] } })
    );

    let mut clean = arguments_map(&json!({ "path": "/x" }));
    assert!(!strip_grammar_token_leaks(&mut clean));
}

#[test]
fn both_gated_strip_passes_run_grammar_then_anchor() {
    // GLM is in both model sets: the token strip has to run first so the
    // anchor strip sees `^foo$`, not `<arg_value>^foo$</arg_value>`
    // (`tool-repair.ts:116-119`).
    let mut message = assistant_message(
        vec![tool_call_block(
            "c1",
            "grep",
            json!({ "<arg_key>pattern</arg_key>": "<arg_value>^foo$</arg_value>" }),
        )],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, Some("glm-4.6"), &core_tool_schemas());
    assert!(outcome.changed());
    assert!(outcome
        .report()
        .is_some_and(|report| report.calls.is_empty()));
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "pattern": "foo" })
    );
}

#[test]
fn anchor_gate_without_the_grammar_gate_leaves_tagged_keys() {
    // MiniMax matches `anchorBleedModels` only (`src/index.ts:26-35`): the
    // value is anchor-stripped, the tagged key is not token-stripped.
    let mut message = assistant_message(
        vec![tool_call_block(
            "c1",
            "grep",
            json!({ "<arg_key>pattern</arg_key>": "^foo$" }),
        )],
        StopReason::Stop,
    );
    let outcome =
        repair_with_real_validator(&mut message, Some("minimax-m1"), &core_tool_schemas());
    let report = outcome
        .report()
        .expect("the anchor strip committed a change");
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].status, LiveRepairStatus::Unrepairable);
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "<arg_key>pattern</arg_key>": "foo" })
    );
}

#[test]
fn anchor_and_grammar_gates_cover_the_configured_models() {
    for model in [
        "kimi-k2-instruct",
        "kimi_k2",
        "moonshotai/kimi-k2.6",
        "@cf/moonshotai/kimi-k2.6",
        "minimax-m1",
        "glm-4.6",
        "GLM-4.6",
    ] {
        assert!(has_anchor_bleed_bug(Some(model)), "{model} anchors");
    }
    for model in ["gpt-5", "deepseek-v4-pro", "kimi-k1", "claude-sonnet-4"] {
        assert!(!has_anchor_bleed_bug(Some(model)), "{model} does not");
    }
    assert!(!has_anchor_bleed_bug(None));
    assert!(!has_anchor_bleed_bug(Some("")));

    // The token-leak gate is the narrower `/glm/i` set.
    assert!(has_grammar_leak_bug(Some("glm-4.6")));
    assert!(has_grammar_leak_bug(Some("GLM-4.6")));
    assert!(!has_grammar_leak_bug(Some("minimax-m1")));
    assert!(!has_grammar_leak_bug(Some("kimi-k2")));
    assert!(!has_grammar_leak_bug(None));
}

#[test]
fn wrap_root_string_table_covers_every_entry() {
    for (tool, field) in [
        ("grep", "pattern"),
        ("find", "pattern"),
        ("bash", "command"),
        ("read", "path"),
        ("ls", "path"),
        ("fabric_exec", "code"),
    ] {
        let mapping = string_arg_tool(tool).expect("tabulated string-arg tool");
        assert_eq!(mapping.field, field);
        assert_eq!(mapping.shape, WrapShape::Str);
        let wrapped = wrap_root_string_as_object(&json!("/x"), tool).expect("a string root wraps");
        let mut expected = serde_json::Map::new();
        expected.insert(field.to_owned(), json!("/x"));
        assert_eq!(wrapped.wrapped, JsonValue::Object(expected));
        assert!(wrapped.hint.contains(field));
    }

    for tool in ["write", "edit", "unknown_tool"] {
        assert!(string_arg_tool(tool).is_none());
        assert!(wrap_root_string_as_object(&json!("/x"), tool).is_none());
    }
    assert!(wrap_root_string_as_object(&json!({ "path": "/x" }), "read").is_none());
}

// ---------------------------------------------------------------------------
// The message pipeline: reporting, purity, and the Step 2 boundary
// ---------------------------------------------------------------------------

#[test]
fn report_lists_attempted_calls_in_content_order() {
    let mut message = assistant_message(
        vec![
            tool_call_block("c1", "read", json!({ "file_path": "/foo" })),
            tool_call_block("c2", "mystery_tool", json!({ "x": 1 })),
            tool_call_block(
                "c3",
                "read",
                json!({ "file_path": "/bar", "limit": [1, 2] }),
            ),
        ],
        StopReason::Stop,
    );
    let outcome = repair_with_real_validator(&mut message, None, &core_tool_schemas());
    assert!(outcome.changed());
    let report = outcome.report().expect("two calls were attempted");
    assert_eq!(report.calls.len(), 2);
    assert_eq!(report.calls[0].call_id, "c1");
    assert_eq!(report.calls[0].status, LiveRepairStatus::Recovered);
    assert_eq!(report.calls[1].call_id, "c3");
    assert_eq!(report.calls[1].status, LiveRepairStatus::Unrepairable);
    assert_eq!(
        call_arguments(&message.content[0]),
        json!({ "path": "/foo" })
    );
    assert_eq!(call_arguments(&message.content[1]), json!({ "x": 1 }));
    assert_eq!(
        call_arguments(&message.content[2]),
        json!({ "file_path": "/bar", "limit": [1, 2] })
    );
}

#[test]
fn unrepairable_outcome_leaves_the_rest_of_the_message_alone() {
    let mut message = assistant_message(
        vec![tool_call_block(
            "c1",
            "read",
            json!({ "file_path": "/foo", "limit": [1, 2] }),
        )],
        StopReason::Stop,
    );
    message.model = "glm-4.6".to_owned();
    let before = message.clone();
    let outcome = repair_with_real_validator(&mut message, Some("glm-4.6"), &core_tool_schemas());
    assert!(matches!(outcome, MessageRepairOutcome::Unrepairable(_)));
    assert_eq!(message, before);
}

#[test]
fn phantom_normalization_gates_on_the_tool_use_stop_reason() {
    for stop_reason in [StopReason::Stop, StopReason::Length, StopReason::Aborted] {
        let mut message = assistant_message(vec![], stop_reason);
        assert!(!normalize_phantom_tool_use(&mut message));
        assert_eq!(message.stop_reason, stop_reason);
        assert!(message.error_message.is_none());
    }
}

#[test]
fn leaked_grammar_text_is_not_recovered_in_step_1a() {
    // Grammar recovery is opt-in and lands in Step 2; Step 1a must leave
    // assistant text byte-identical (`tool-repair.ts:121-145`).
    let leaked = concat!(
        "<｜DSML｜tool_calls><｜DSML｜invoke name=\"bash\">",
        "<｜DSML｜parameter name=\"command\">ls</｜DSML｜parameter>",
        "</｜DSML｜invoke>"
    );
    let mut message = assistant_message(
        vec![ContentBlock::Text(pir::ai::types::TextContent {
            text: leaked.to_owned(),
            text_signature: None,
        })],
        StopReason::Stop,
    );
    let before = message.clone();
    let outcome = repair_with_real_validator(&mut message, Some("glm-4.6"), &core_tool_schemas());
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(message, before);
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

#[test]
fn config_file_loads_the_grammar_block_and_the_readme_debug_or() {
    let path = std::env::temp_dir().join(format!(
        "pi-tool-repair-test-{}-grammar.json",
        std::process::id()
    ));
    std::fs::write(
        &path,
        r#"{"grammarRepair":{"enabled":true,"mode":"strip","debug":true,"grammars":["dsml"]}}"#,
    )
    .expect("write the temp config");
    let config = ToolRepairConfig::load_from(&path);
    let _ = std::fs::remove_file(&path);

    assert!(config.grammar.enabled);
    assert_eq!(config.grammar.mode, GrammarRepairMode::Strip);
    assert_eq!(config.grammar.grammars, vec!["dsml"]);
    // The README documents the OR of the env var and the file flag.
    assert!(config.debug);
}

#[test]
fn config_missing_file_keeps_the_defaults() {
    let path = std::env::temp_dir().join(format!(
        "pi-tool-repair-test-{}-missing.json",
        std::process::id()
    ));
    let config = ToolRepairConfig::load_from(&path);
    assert_eq!(config.grammar, GrammarRepairConfig::default());
    assert_eq!(config.debug, ToolRepairConfig::debug_enabled());
}

// ---------------------------------------------------------------------------
// Step 1b loop-level wiring
// ---------------------------------------------------------------------------
//
// The sync tests above pin the pure repair core; the async tests below pin the
// `message_end` seam itself. A real `AgentSession` drives a scripted in-process
// provider through `prompt()`:
//
// - `src/agent_session/prompt_loop.rs:582` assembles the run's `LoopDeps` with
//   `message_end_repair: self.tool_repair_hook()`;
// - `src/agent/agent_loop.rs:847` applies the hook AFTER the assistant
//   `message_end` extension dispatch and BEFORE the terminal assignment,
//   context push, persistence, and tool-call extraction/validation.
//
// Scaffolding mirrors `tests/suites/agent_session.rs:88-145` (the scripted
// `Provider` plus `registry_stream_override`) and `:300-361` (a session stored
// in a temp dir and re-opened to read the persisted transcript).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use pir::agent::agent_loop::AgentEvent;
use pir::agent_session::{AgentSession, AgentSessionConfig, EventHandler, SharedEventStreamFn};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    Context, Message, Model, ModelCost, ModelCostRates, StreamOptions, TerminalReason, TextContent,
    ThinkingLevel,
};
use pir::plugins::builtin::instantiate_builtin;
use pir::plugins::{AgentPlugin, PluginInfo};
use pir::session_manager::SessionManager;

/// The scripted provider's responder: request index -> finalized message.
/// Mirrors `tests/suites/agent_session.rs:89`.
type LoopResponder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// An in-process provider answering every request with a scripted message (the
/// `tests/suites/agent_session.rs:92-125` fake). No HTTP; the loop path (hook
/// application, preflight validation, scheduler execution, persistence) is the
/// production one.
struct ScriptedLoopProvider {
    calls: Arc<AtomicUsize>,
    respond: LoopResponder,
}

impl Provider for ScriptedLoopProvider {
    fn id(&self) -> &str {
        "repair-loop"
    }

    fn name(&self) -> &str {
        "Tool-repair loop harness"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![loop_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new(
            "repair-loop key",
            vec!["PIR_TOOL_REPAIR_LOOP_KEY".into()],
        )]
    }

    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// The scripted model (mirror: `tests/suites/agent_session.rs:31-57`). The id
/// deliberately avoids the `/glm/i` strip-pass gate, so only the model-free
/// passes can fire.
fn loop_model() -> Model {
    Model {
        id: "repair-loop-model".into(),
        name: "Tool-repair loop model".into(),
        api: "fake-api".into(),
        provider: "repair-loop".into(),
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
        max_tokens: 256,
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

fn loop_text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

/// A scripted assistant message with the harness model identity (mirror:
/// `tests/suites/agent_session.rs:66-76`).
fn loop_assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: "repair-loop".into(),
        model: "repair-loop-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..AssistantMessage::default()
    }
}

fn loop_assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn noop_events() -> EventHandler {
    Arc::new(|_event: AgentEvent| {})
}

/// The registry-backed stream override (mirror:
/// `tests/suites/agent_session.rs:136-145`): `stream_unchecked` skips auth
/// resolution, so the scripted provider is always the one consulted.
fn loop_stream_override(registry: Arc<ProviderRegistry>, model: Model) -> SharedEventStreamFn {
    Arc::new(move |request| {
        let stream = registry.stream_unchecked(
            request.model.unwrap_or_else(|| model.clone()),
            // The provider trait takes the context by value; the request
            // carries it shared (`ProviderRequest.context`), so materialize
            // the owned copy through the loop's own helper.
            pir::agent::agent_loop::context_into_owned(request.context),
            StreamOptions::default(),
        );
        Box::pin(async move { stream })
    })
}

/// The `read`-shaped probe: it carries the REAL production `read` schema (the
/// bytes the repair core validates against; `src/coding_agent/tools/read.rs:76`)
/// and records every argument object it is handed, so a test can tell the
/// repaired arguments from the raw ones — and that a rejected call never
/// executes.
struct LoopProbeTool {
    received: Arc<Mutex<Vec<JsonValue>>>,
}

impl AgentTool for LoopProbeTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "tool-repair loop harness probe"
    }

    fn parameters(&self) -> Option<JsonValue> {
        Some(core_schema("read"))
    }

    fn run(&self, args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(args.clone());
        Box::pin(async { ToolResult::ok("probe ran") })
    }
}

/// Per-harness temp dir (mirror: `tests/suites/agent_session.rs:234-256`).
struct LoopTempDir(PathBuf);

impl LoopTempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-tool-repair-loop-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for LoopTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One scripted session plus the observables its tests assert on.
struct RepairHarness {
    session: Arc<AgentSession>,
    /// The stored session file, re-opened to read the persisted transcript.
    transcript: PathBuf,
    /// Every argument object the `read` probe executed with.
    received: Arc<Mutex<Vec<JsonValue>>>,
    /// Provider requests issued during the run.
    provider_calls: Arc<AtomicUsize>,
    /// Dropped last: the session must fall before its temp dir is removed.
    _dir: LoopTempDir,
}

/// Build a stored session over the scripted provider. `plugins` is the
/// session's inline plugin list: empty for the no-plugin baseline, the
/// `tool-repair` builtin (or an unrelated plugin) otherwise.
fn repair_harness(plugins: Vec<Arc<dyn AgentPlugin>>, respond: LoopResponder) -> RepairHarness {
    repair_harness_with_extensions(plugins, None, respond)
}

/// [`repair_harness`] with a live JS extension runtime attached (the
/// `AgentSessionConfig.extensions` slot production sessions carry): drives the
/// generic JS `message_end` handler surface together with the native builtin
/// hook, so a test can pin their coexistence through the real session wiring
/// (`src/agent_session/prompt_loop.rs` hands both to the loop).
fn repair_harness_with_extensions(
    plugins: Vec<Arc<dyn AgentPlugin>>,
    extensions: Option<pir::extensions::ExtensionHandle>,
    respond: LoopResponder,
) -> RepairHarness {
    let dir = LoopTempDir::new("loop");
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(LoopProbeTool {
        received: Arc::clone(&received),
    }));

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(ScriptedLoopProvider {
        calls: Arc::clone(&provider_calls),
        respond,
    }));
    let registry = Arc::new(registry);

    let model = loop_model();
    let session = SessionManager::create(".", &dir.0).expect("create session");
    let transcript = session.path().to_path_buf();
    let config = AgentSessionConfig {
        provider_registry: Arc::clone(&registry),
        model: model.clone(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: Some("tool-repair loop harness".to_owned()),
        cwd: Some(dir.0.clone()),
        event_stream: Some(loop_stream_override(registry, model)),
        extensions,
        tool_selection: None,
    };
    let session = if plugins.is_empty() {
        Arc::new(AgentSession::new(config))
    } else {
        Arc::new(AgentSession::new_with_plugins(config, plugins))
    };
    RepairHarness {
        session,
        transcript,
        received,
        provider_calls,
        _dir: dir,
    }
}

/// The `tool-repair` builtin exactly as the plugin host instantiates it for a
/// `builtin:tool-repair` path entry (`src/plugins/host.rs:270`), i.e. through
/// the factory table (`src/plugins/builtin.rs:114`). A revert that drops the
/// registration makes `instantiate_builtin` return `None` and every enabled
/// fixture fail here.
fn tool_repair_builtin() -> Arc<dyn AgentPlugin> {
    instantiate_builtin("tool-repair")
        .expect("builtin:tool-repair must be registered (src/plugins/builtin.rs:114)")
}

/// A loaded plugin that is NOT `tool-repair`: it keeps the session's plugin
/// host non-empty, so the disabled case proves the repair hook is keyed on the
/// builtin's presence in the dispatch list, not merely on a host existing.
struct UnrelatedPlugin {
    info: PluginInfo,
}

impl UnrelatedPlugin {
    fn new() -> Self {
        Self {
            info: PluginInfo::builtin("test-unrelated"),
        }
    }
}

impl AgentPlugin for UnrelatedPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

/// The persisted transcript of a stored session, re-opened after the run
/// (mirror: `tests/suites/agent_session.rs:334-340`).
fn persisted_messages(transcript: &Path) -> Vec<Message> {
    let reopened = SessionManager::open(transcript).expect("reopen session");
    reopened.build_context_entries().expect("context entries")
}

/// The persisted assistant tool call's argument object, i.e. the transcript
/// bytes the repair hook had to precede. Panics when the call is absent, so a
/// dropped call cannot silently satisfy an assertion.
fn persisted_tool_call_arguments(messages: &[Message], call_id: &str) -> JsonValue {
    for message in messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            if let ContentBlock::ToolCall(call) = block {
                if call.id == call_id {
                    return call_arguments(block);
                }
            }
        }
    }
    panic!("no persisted assistant tool call {call_id}: {messages:?}");
}

/// `(is_error, text)` of the persisted tool result for `call_id`.
fn persisted_tool_result(messages: &[Message], call_id: &str) -> (bool, String) {
    for message in messages {
        let Message::ToolResult(result) = message else {
            continue;
        };
        if result.tool_call_id == call_id {
            let text = result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return (result.is_error, text);
        }
    }
    panic!("no persisted tool result for {call_id}: {messages:?}");
}

/// Script the first request into a `read` call with the given arguments and
/// every later request into a plain stop.
fn scripted_read_call(arguments: JsonValue) -> LoopResponder {
    Arc::new(move |index, _context| {
        if index == 0 {
            loop_assistant(
                vec![tool_call_block("call-read", "read", arguments.clone())],
                StopReason::ToolUse,
            )
        } else {
            loop_assistant(vec![loop_text("done")], StopReason::Stop)
        }
    })
}

#[tokio::test]
async fn repair_reaches_the_tool_arguments_through_the_loop() {
    // The mistake: an optional field sent as `null`; the repair core's
    // `dropNullOrUndefined` rule removes it. (The PREFLIGHT would also drop it
    // — upstream `normalizeOptionalNulls` — so the transcript assertion below
    // is what distinguishes the hook's own repair from that fallback.)
    let raw = json!({ "path": "x", "offset": null });
    let harness = repair_harness(vec![tool_repair_builtin()], scripted_read_call(raw.clone()));
    assert!(
        harness.session.tool_repair_hook().is_some(),
        "a session carrying the tool-repair builtin builds the message_end hook"
    );

    let terminal = harness
        .session
        .prompt("call the read probe", noop_events())
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(loop_assistant_text(&terminal), "done");
    assert_eq!(
        harness.provider_calls.load(Ordering::SeqCst),
        2,
        "the tool result was fed back for a second request"
    );

    // The TOOL ran with the REPAIRED arguments — the raw `offset: null` would
    // have failed preflight, so an executed repair is the only explanation.
    let received = harness
        .received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(received.len(), 1, "the probe executed exactly once");
    assert_eq!(
        received[0],
        json!({ "path": "x" }),
        "the tool must receive the repaired arguments, not {raw}"
    );
    assert!(
        received[0].get("offset").is_none(),
        "the optional null must be dropped before validation"
    );

    // And the PERSISTED transcript records the repaired call and a successful
    // result: the hook ran before persistence and before execution.
    let messages = persisted_messages(&harness.transcript);
    assert_eq!(
        persisted_tool_call_arguments(&messages, "call-read"),
        json!({ "path": "x" }),
        "the persisted assistant message must carry the repaired call"
    );
    let (is_error, text) = persisted_tool_result(&messages, "call-read");
    assert!(!is_error, "the repaired call must pass preflight: {text}");
    assert_eq!(text, "probe ran");
}

#[tokio::test]
async fn disabled_builtin_extension_leaves_the_call_unrepaired() {
    // The settings surface `disabledBuiltinExtensions: ["tool-repair"]`
    // (src/plugins/loader.rs `filter_disabled_builtins`): the builtin path is dropped before the
    // host is built, so no ToolRepairPlugin reaches the dispatch list.
    let plan = vec![PathBuf::from("builtin:tool-repair")];
    let survivors =
        pir::plugins::filter_disabled_builtins(&plan, &BTreeSet::from(["tool-repair".to_owned()]));
    assert!(
        survivors.is_empty(),
        "the disabled builtin leaves no path for the host to instantiate: {survivors:?}"
    );

    // A genuinely wrong type, not an optional null: preflight's upstream
    // normalizations (`normalizeOptionalNulls` / numeric-string coercion)
    // would rescue a droppable `null`, but nothing rescues `"abc"` for a
    // `"number"` field. With no repair hook the call fails preflight and
    // never executes.
    let raw = json!({ "path": "x", "offset": "abc" });
    let harness = repair_harness(
        vec![Arc::new(UnrelatedPlugin::new())],
        scripted_read_call(raw.clone()),
    );
    assert!(
        harness.session.tool_repair_hook().is_none(),
        "a plugin host without the tool-repair builtin builds no repair hook"
    );

    let terminal = harness
        .session
        .prompt("call the read probe", noop_events())
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(loop_assistant_text(&terminal), "done");
    assert_eq!(harness.provider_calls.load(Ordering::SeqCst), 2);

    // Without the hook the raw mistake reaches validation: the call fails
    // preflight and never executes.
    assert!(
        harness
            .received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "the unrepaired call must be rejected before execution"
    );
    let messages = persisted_messages(&harness.transcript);
    assert_eq!(
        persisted_tool_call_arguments(&messages, "call-read"),
        raw,
        "the transcript must keep the RAW arguments"
    );
    let (is_error, text) = persisted_tool_result(&messages, "call-read");
    assert!(is_error, "the raw call must fail: {text}");
    assert!(
        text.contains("Validation failed for tool \"read\":"),
        "the host must surface the validation error: {text}"
    );
    assert!(
        text.contains("$.offset: expected number, got string"),
        "the error must name the raw mistake: {text}"
    );
}

/// ③ builtin + JS coexistence through the REAL session wiring: the builtin
/// builds its native `message_end_repair` hook (which `prompt_loop` hands to
/// the loop) and the JS runtime's `message_end` handler is dispatched by the
/// same loop. The JS replacement is applied first (its marker and its `offset`
/// repair are what the transcript records), the native hook runs over that
/// already-repaired message in the same ordered chain — exactly one repair per
/// message — and only ONE `tool-repair` face is loaded (native-first
/// precedence), so the two pipeline copies can never double-run.
#[tokio::test]
async fn session_js_message_end_and_native_hook_share_one_ordered_chain() {
    let dir = LoopTempDir::new("js-message-end");
    let fixture = dir.0.join("ext.ts");
    std::fs::write(
        &fixture,
        r#"
        export default function (pi) {
            globalThis.__jsRuns = 0;
            pi.on("message_end", (event) => {
                if (!event.message || event.message.role !== "assistant") return;
                const content = Array.isArray(event.message.content) ? event.message.content : [];
                let changed = false;
                const next = content.map((block) => {
                    if (block.type !== "toolCall" || !block.arguments) return block;
                    if (!Object.prototype.hasOwnProperty.call(block.arguments, "offset")) return block;
                    const { offset, ...rest } = block.arguments;
                    changed = true;
                    return { ...block, arguments: rest };
                });
                if (!changed) return;
                globalThis.__jsRuns += 1;
                return {
                    message: {
                        ...event.message,
                        content: [{ type: "text", text: "js-marker" }, ...next],
                    },
                };
            });
        }
        "#,
    )
    .expect("write fixture");
    let runtime = pir::extensions::ExtensionHandle::start(pir::extensions::ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime");
    let loaded = runtime
        .load(std::slice::from_ref(&fixture))
        .await
        .expect("load");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);

    // The mistake: an optional field sent as `null`, which the real `read`
    // schema rejects — the shape BOTH faces repair.
    let raw = json!({ "path": "x", "offset": null });
    let harness = repair_harness_with_extensions(
        vec![tool_repair_builtin()],
        Some(runtime.clone()),
        scripted_read_call(raw.clone()),
    );
    assert!(
        harness.session.tool_repair_hook().is_some(),
        "the builtin's native message_end hook is wired next to the JS runtime"
    );

    let terminal = harness
        .session
        .prompt("call the read probe", noop_events())
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::Stop);
    assert_eq!(harness.provider_calls.load(Ordering::SeqCst), 2);

    // The JS face ran exactly once (one message needed a repair), and its
    // replacement is what the transcript holds: the marker block first, the
    // null argument gone.
    let js_runs = runtime
        .eval_json("JSON.stringify(globalThis.__jsRuns)")
        .await
        .expect("probe");
    assert_eq!(js_runs, json!(1), "one repair, one JS handler pass");
    let messages = persisted_messages(&harness.transcript);
    let assistant = messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.clone()),
            _ => None,
        })
        .expect("the persisted assistant message");
    assert_eq!(
        assistant.content.first(),
        Some(&ContentBlock::Text(TextContent {
            text: "js-marker".into(),
            text_signature: None,
        })),
        "the JS replacement is what the transcript records: {assistant:?}"
    );
    assert_eq!(
        persisted_tool_call_arguments(&messages, "call-read"),
        json!({ "path": "x" }),
        "the JS face's repair reached the transcript (the native hook then \
         found nothing left to change)"
    );

    // The tool executed with the repaired arguments: the raw `offset: null`
    // would have failed the preflight, so a run is only possible after the
    // `message_end` chain (JS + native) was applied.
    let received = harness
        .received
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(received.len(), 1, "the probe executed exactly once");
    assert_eq!(received[0], json!({ "path": "x" }), "not {raw}");
    let (is_error, text) = persisted_tool_result(&messages, "call-read");
    assert!(!is_error, "the repaired call must pass preflight: {text}");
}

#[tokio::test]
async fn unrepairable_call_surfaces_the_original_error() {
    // `{"path": 5}`: no repair rule turns a number into a string, so the core
    // reports Unrepairable and the hook commits nothing (`repair.rs:1094-1102`
    // only commits a `Recovered` candidate).
    let respond = scripted_read_call(json!({ "path": 5 }));

    let repaired_run = repair_harness(vec![tool_repair_builtin()], respond.clone());
    let baseline_run = repair_harness(Vec::new(), respond);
    assert!(repaired_run.session.tool_repair_hook().is_some());
    assert!(baseline_run.session.tool_repair_hook().is_none());

    let repaired = repaired_run
        .session
        .prompt("call the read probe", noop_events())
        .await
        .expect("prompt");
    let baseline = baseline_run
        .session
        .prompt("call the read probe", noop_events())
        .await
        .expect("prompt");
    assert_eq!(repaired.stop_reason, StopReason::Stop);
    assert_eq!(baseline.stop_reason, StopReason::Stop);

    for run in [&repaired_run, &baseline_run] {
        assert!(
            run.received
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "an unrepairable call never reaches the tool"
        );
    }

    let repaired_messages = persisted_messages(&repaired_run.transcript);
    let baseline_messages = persisted_messages(&baseline_run.transcript);
    assert_eq!(
        persisted_tool_call_arguments(&repaired_messages, "call-read"),
        json!({ "path": 5 }),
        "an unrepairable call keeps its raw bytes in the transcript"
    );
    assert_eq!(
        persisted_tool_call_arguments(&baseline_messages, "call-read"),
        json!({ "path": 5 })
    );

    let (repaired_error, repaired_text) = persisted_tool_result(&repaired_messages, "call-read");
    let (baseline_error, baseline_text) = persisted_tool_result(&baseline_messages, "call-read");
    assert!(repaired_error && baseline_error);
    assert_eq!(
        repaired_text, baseline_text,
        "the repair pass must not swallow or reword the original validation error"
    );
    assert!(
        repaired_text.contains("$.path: expected string, got number"),
        "the surfaced error is the production preflight error: {repaired_text}"
    );
}

#[tokio::test]
async fn phantom_tool_use_becomes_a_retryable_error() {
    // `toolUse` with zero toolCall blocks: the phantom shape
    // (`repair.rs:943-957`); the scripted provider ignores the index so a
    // retry (if any) would re-answer identically.
    let respond: LoopResponder = Arc::new(|_index, _context| {
        loop_assistant(vec![loop_text("no tool calls here")], StopReason::ToolUse)
    });

    let repaired_run = repair_harness(vec![tool_repair_builtin()], respond.clone());
    let terminal = repaired_run
        .session
        .prompt("phantom", noop_events())
        .await
        .expect("prompt");
    assert_eq!(
        terminal.stop_reason,
        StopReason::Error,
        "the phantom toolUse must leave the loop as an error"
    );
    assert_eq!(
        terminal.error_message.as_deref(),
        Some(PHANTOM_TOOL_USE_ERROR)
    );
    assert_eq!(
        repaired_run.provider_calls.load(Ordering::SeqCst),
        1,
        "the loop must not retry the request on its own"
    );

    // The repaired message is what the loop assigned/persisted
    // (`agent_loop.rs:847-855`).
    let messages = persisted_messages(&repaired_run.transcript);
    let assistant = messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .expect("the persisted transcript holds the assistant message");
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert_eq!(
        assistant.error_message.as_deref(),
        Some(PHANTOM_TOOL_USE_ERROR)
    );

    // Baseline: WITHOUT the builtin the loop leaves the provider's message
    // exactly as finalized — the phantom error belongs to the repair wiring.
    let baseline_run = repair_harness(Vec::new(), respond);
    let terminal = baseline_run
        .session
        .prompt("phantom", noop_events())
        .await
        .expect("prompt");
    assert_eq!(terminal.stop_reason, StopReason::ToolUse);
    assert_eq!(terminal.error_message, None);
    assert_eq!(baseline_run.provider_calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Step 3: the `before_provider_request` Phase 0 face
// ---------------------------------------------------------------------------
//
// The pure schema sanitizer (`repair.rs::sanitize_request_payload`,
// `tool-repair.ts:73-105` + `src/index.ts:111-134`) plus one dispatcher-level
// test through the real `PluginHost`: the event-interest registration, the
// `on_session_context` model push, and the merged result the session host
// reads as the new request body (`agent_session/stream.rs:124-131`).

use pir::extensions::SessionContextSnapshot;
use pir::plugins::tool_repair::repair::{
    sanitize_pattern, sanitize_request_payload, sanitize_schema_anchors,
};
use pir::plugins::PluginHost;

/// The OpenAI-family tool slot pir's `convert_tool` emits
/// (`src/ai/api/openai/mod.rs:500-535`), with one anchored string schema.
fn anchored_body() -> JsonValue {
    json!({
        "model": "gated-model",
        "tools": [{
            "type": "function",
            "function": {
                "name": "read",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string", "pattern": "^foo$"}},
                    "required": ["path"],
                },
            },
        }],
    })
}

/// The `pattern` value of [`anchored_body`]'s one tool schema.
fn tool_pattern(body: &JsonValue) -> &JsonValue {
    &body["tools"][0]["function"]["parameters"]["properties"]["path"]["pattern"]
}

#[test]
fn area0_sanitize_pattern_follows_the_ts_reference_rules() {
    // The plain anchor form: strip the anchors, keep the body.
    assert_eq!(sanitize_pattern("^foo$"), Some("foo".to_owned()));
    assert_eq!(sanitize_pattern("foo"), Some("foo".to_owned()));
    // Alternation + anchors: the whole key goes (anchoring is unreliable).
    assert_eq!(sanitize_pattern("^a|b$"), None);
    assert_eq!(sanitize_pattern("a|b"), Some("a|b".to_owned()));
    // TS `replace(/\^|\$/g, "")`: ALL occurrences, not boundaries only.
    assert_eq!(
        sanitize_pattern("^foo^bar$baz$"),
        Some("foobarbaz".to_owned())
    );
    // Empty after the strip: the key goes too.
    assert_eq!(sanitize_pattern("^^"), None);
    assert_eq!(sanitize_pattern("$^$"), None);
    assert_eq!(sanitize_pattern(""), None);
}

#[test]
fn area0_gated_models_strip_schema_anchors_in_tool_parameters() {
    // The `anchorBleedModels` set (`src/index.ts:26-32`), one id per regex.
    for model in [
        "kimi-k2-instruct",
        "kimi_k2",
        "moonshotai/kimi-k2",
        "@cf/moonshotai/kimi-k2",
        "MiniMax-Text-01",
        "glm-4.6",
        "GLM-4.5-Air",
    ] {
        let mut body = anchored_body();
        assert!(
            sanitize_request_payload(&mut body, Some(model)),
            "gated model {model} must sanitize"
        );
        assert_eq!(tool_pattern(&body), &json!("foo"), "model {model}");
        // Everything but the pattern survives byte-for-byte.
        assert_eq!(body["tools"][0]["function"]["name"], json!("read"));
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["required"],
            json!(["path"])
        );
    }
}

#[test]
fn area0_non_gated_models_leave_the_payload_untouched() {
    for model in [
        None,
        Some(""),
        Some("gpt-4.1"),
        Some("claude-sonnet-4-20250514"),
        Some("qwen3-coder"),
    ] {
        let mut body = anchored_body();
        let pristine = body.clone();
        assert!(
            !sanitize_request_payload(&mut body, model),
            "model {model:?} must not sanitize"
        );
        assert_eq!(body, pristine, "model {model:?} must stay byte-identical");
    }
}

#[test]
fn area0_alternation_with_anchors_deletes_the_key_and_keeps_siblings() {
    let mut body = json!({
        "tools": [{"function": {"parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "pattern": "^a|b$", "description": "keep me"},
                "limit": {"type": "number"},
            },
        }}}],
    });
    assert!(sanitize_request_payload(&mut body, Some("glm-4.6")));
    let path = &body["tools"][0]["function"]["parameters"]["properties"]["path"];
    assert!(
        path.get("pattern").is_none(),
        "pattern must be deleted: {path}"
    );
    assert_eq!(path["description"], json!("keep me"));
    assert_eq!(path["type"], json!("string"));
    assert!(
        body["tools"][0]["function"]["parameters"]["properties"]["limit"]
            .get("pattern")
            .is_none()
    );
}

#[test]
fn area0_recursion_covers_nested_properties_items_and_arrays() {
    let mut body = json!({
        "model": "glm-4.6",
        "tools": [{"function": {"parameters": {
            "type": "object",
            "properties": {
                "nested": {"type": "object", "properties": {
                    "deep": {"type": "string", "pattern": "^deep$"},
                }},
                "list": {"type": "array", "items": {"type": "string", "pattern": "^item$"}},
                "choice": {"anyOf": [
                    {"type": "string", "pattern": "^one$"},
                    {"type": "string", "pattern": "^a|b$"},
                ]},
            },
            "required": ["nested"],
        }}}],
    });
    assert!(sanitize_request_payload(&mut body, Some("glm-4.6")));
    let parameters = &body["tools"][0]["function"]["parameters"];
    assert_eq!(
        parameters["properties"]["nested"]["properties"]["deep"]["pattern"],
        json!("deep")
    );
    assert_eq!(
        parameters["properties"]["list"]["items"]["pattern"],
        json!("item")
    );
    assert_eq!(
        parameters["properties"]["choice"]["anyOf"][0]["pattern"],
        json!("one")
    );
    assert!(parameters["properties"]["choice"]["anyOf"][1]
        .get("pattern")
        .is_none());
    assert_eq!(parameters["required"], json!(["nested"]));
    assert_eq!(body["model"], json!("glm-4.6"));
}

#[test]
fn area0_empty_after_strip_deletes_the_key_and_non_string_patterns_survive() {
    let mut body = json!({
        "tools": [{"function": {"parameters": {
            "properties": {
                "emptied": {"pattern": "^^"},
                "numeric": {"pattern": 42},
                "boolean": {"pattern": false},
                "null": {"pattern": null},
                "array": {"pattern": ["^keep$"]},
                "object": {"pattern": {"pattern": "^inner$"}},
                "anchored": {"pattern": "^keep$"},
            },
        }}}],
    });
    assert!(sanitize_request_payload(&mut body, Some("minimax-01")));
    let props = &body["tools"][0]["function"]["parameters"]["properties"];
    assert!(props["emptied"].get("pattern").is_none());
    assert_eq!(props["numeric"]["pattern"], json!(42));
    assert_eq!(props["boolean"]["pattern"], json!(false));
    assert_eq!(props["null"]["pattern"], JsonValue::Null);
    // A non-string `pattern` is not a schema pattern: only real object levels
    // recurse (TS key+type check), so the array value is untouched...
    assert_eq!(props["array"]["pattern"], json!(["^keep$"]));
    // ...while an object VALUE recurses and its inner `pattern` key is a real
    // schema keyword.
    assert_eq!(props["object"]["pattern"]["pattern"], json!("inner"));
    assert_eq!(props["anchored"]["pattern"], json!("keep"));
}

#[test]
fn area0_response_format_json_schema_is_sanitized() {
    let mut body = json!({
        "model": "glm-4.6",
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "report",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {"title": {"type": "string", "pattern": "^Title: "}},
                },
            },
        },
    });
    assert!(sanitize_request_payload(&mut body, Some("glm-4.6")));
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["properties"]["title"]["pattern"],
        json!("Title: ")
    );
    assert_eq!(
        body["response_format"]["json_schema"]["name"],
        json!("report")
    );
    assert_eq!(
        body["response_format"]["json_schema"]["strict"],
        json!(true)
    );
}

#[test]
fn area0_payloads_without_the_target_shapes_are_no_ops() {
    for payload in [
        json!({}),
        json!({"model": "glm-4.6"}),
        json!({"tools": "not-an-array"}),
        json!({"tools": []}),
        json!({"tools": [{}]}),
        json!({"tools": [{"function": {}}]}),
        json!({"tools": [{"function": {"parameters": "not-an-object"}}]}),
        json!({"tools": null, "response_format": {"json_schema": {}}}),
        json!({"response_format": null}),
        json!("a bare string payload"),
        json!(7),
        JsonValue::Null,
    ] {
        let pristine = payload.clone();
        let mut body = payload;
        assert!(
            !sanitize_request_payload(&mut body, Some("glm-4.6")),
            "no target shape must mean no change: {pristine}"
        );
        assert_eq!(body, pristine);
    }
}

#[test]
fn area0_sanitize_schema_anchors_returns_a_sanitized_copy() {
    let schema = json!({
        "type": "object",
        "properties": {"p": {"pattern": "^foo$"}},
        "items": {"pattern": "^a|b$"},
    });
    let sanitized = sanitize_schema_anchors(&schema);
    assert_eq!(sanitized["properties"]["p"]["pattern"], json!("foo"));
    assert!(sanitized["items"].get("pattern").is_none());
    // The input is untouched: the value function is a transform, not a wipe.
    assert_eq!(schema["properties"]["p"]["pattern"], json!("^foo$"));
    assert_eq!(schema["items"]["pattern"], json!("^a|b$"));
    // Non-object schemas pass through unchanged.
    assert_eq!(sanitize_schema_anchors(&json!("plain")), json!("plain"));
    assert_eq!(sanitize_schema_anchors(&JsonValue::Null), JsonValue::Null);
}

#[tokio::test]
async fn area0_host_dispatch_gates_and_replaces_the_request_payload() {
    let host = PluginHost::build(None, vec![tool_repair_builtin()]);
    // Interest registration: the session host installs its per-request
    // transport hook exactly when `has_handlers` is true
    // (`agent_session/stream.rs:115`), and the list stays narrow.
    assert!(host.has_handlers("before_provider_request"));
    assert!(!host.has_handlers("session_start"));

    // No pushed model: the gate must not fire (and the payload is untouched).
    let mut event = json!({"type": "before_provider_request", "payload": anchored_body()});
    let outcome = host
        .emit("before_provider_request", &mut event)
        .await
        .expect("emit");
    assert!(
        outcome.result.is_null(),
        "no pushed model must mean no repair"
    );
    assert_eq!(event["payload"], anchored_body());

    // The configured-model-id STRING shape (`runtime_loader.rs:80-84`): the
    // merged result is the sanitized body the transport substitutes.
    host.update_session_context(SessionContextSnapshot {
        model: JsonValue::String("kimi-k2-instruct".to_owned()),
        ..SessionContextSnapshot::default()
    });
    let mut event = json!({"type": "before_provider_request", "payload": anchored_body()});
    let outcome = host
        .emit("before_provider_request", &mut event)
        .await
        .expect("emit");
    assert_eq!(tool_pattern(&outcome.result), &json!("foo"));
    // The in-place rewrite is visible too, so later plugins chain on it.
    assert_eq!(tool_pattern(&event["payload"]), &json!("foo"));

    // The serialized-Model object shape (the per-prompt push).
    host.update_session_context(SessionContextSnapshot {
        model: json!({"id": "GLM-4.6", "provider": "zhipu"}),
        ..SessionContextSnapshot::default()
    });
    let mut event = json!({"type": "before_provider_request", "payload": anchored_body()});
    let outcome = host
        .emit("before_provider_request", &mut event)
        .await
        .expect("emit");
    assert_eq!(tool_pattern(&outcome.result), &json!("foo"));

    // A model switch to a non-gated id turns the face back off.
    host.update_session_context(SessionContextSnapshot {
        model: json!({"id": "gpt-4.1"}),
        ..SessionContextSnapshot::default()
    });
    let mut event = json!({"type": "before_provider_request", "payload": anchored_body()});
    let outcome = host
        .emit("before_provider_request", &mut event)
        .await
        .expect("emit");
    assert!(outcome.result.is_null());
    assert_eq!(event["payload"], anchored_body());
}
