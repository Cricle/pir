//! Strict preflight validation for AgentTool JSON schemas.
//!
//! The last section pins the upstream preflight NORMALIZATIONS
//! (`validateToolArguments`, `../pi/packages/ai/src/utils/validation.ts:335-342`):
//! `normalizeOptionalNulls` drops an optional `null`, and the schema-directed
//! coercion turns a numeric string into a JSON number for `"number"`/
//! `"integer"` fields. Both are accepted WITHOUT a rejected-call turn; every
//! other wrong type still fails with the complete violation list.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolHooks, ToolRegistry};
use pir::agent::tool_scheduler::ToolScheduler;

struct SchemaTool {
    runs: Arc<AtomicUsize>,
    schema: serde_json::Value,
}

impl AgentTool for SchemaTool {
    fn name(&self) -> &'static str {
        "schema_tool"
    }

    fn description(&self) -> &'static str {
        "schema validation test tool"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(self.schema.clone())
    }

    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("ran") })
    }
}

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["mode", "items"],
        "properties": {
            "mode": { "type": "string", "enum": ["fast", "safe"] },
            "items": {
                "type": "array",
                "items": { "type": "integer" }
            },
            "metadata": {
                "type": "object",
                "properties": { "label": { "type": "string" } },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    })
}

fn call(arguments: serde_json::Value) -> AgentToolCall {
    AgentToolCall::new(0, "schema-call", "schema_tool", arguments)
}

#[tokio::test]
async fn schema_rejects_invalid_type_required_enum_items_and_additional_properties() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(SchemaTool {
        runs: runs.clone(),
        schema: schema(),
    }));
    let scheduler = ToolScheduler::default();
    let context = ToolContext::new(".");

    for arguments in [
        serde_json::json!([]),
        serde_json::json!({"items": [1]}),
        serde_json::json!({"mode": "unknown", "items": [1]}),
        serde_json::json!({"mode": "fast", "items": ["not an integer"]}),
        serde_json::json!({"mode": "fast", "items": [1], "unexpected": true}),
        serde_json::json!({"mode": "fast", "items": [1], "metadata": {"other": true}}),
    ] {
        let outcome = scheduler
            .execute_batch(vec![call(arguments)], &registry, &context, None)
            .await;
        assert_eq!(outcome.results.len(), 1);
        assert!(outcome.results[0].is_error());
        // Upstream emits `tool_execution_start` for every call before
        // preparation and an immediate `tool_execution_end` for a failed
        // preparation (agent-loop.ts:541-547,551-556).
        assert_eq!(outcome.execution_start.len(), 1);
        assert_eq!(outcome.execution_end.len(), 1);
        assert!(matches!(
            outcome.execution_end[0],
            pir::agent::tool_batch::ToolExecutionEvent::End { is_error: true, .. }
        ));
    }
    assert_eq!(runs.load(Ordering::SeqCst), 0, "invalid calls must not run");
}

#[tokio::test]
async fn valid_schema_arguments_run_and_validation_precedes_before_hook() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(SchemaTool {
        runs: runs.clone(),
        schema: schema(),
    }));
    let scheduler = ToolScheduler::default();
    let context = ToolContext::new(".");
    let before_calls = Arc::new(AtomicUsize::new(0));
    let before_calls_for_hook = before_calls.clone();
    let hooks = ToolHooks {
        before: Some(Box::new(move |_call, _context| {
            before_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Default::default() })
        })),
        ..ToolHooks::default()
    };

    let outcome = scheduler
        .execute_batch(
            vec![
                call(serde_json::json!({"mode": "fast", "items": [1, 2], "metadata": {"label": "ok"}})),
                AgentToolCall::new(1, "invalid-call", "schema_tool", serde_json::json!({"mode": "fast"})),
            ],
            &registry,
            &context,
            Some(&hooks),
        )
        .await;

    assert_eq!(outcome.results.len(), 2);
    assert!(
        !outcome.results[0].is_error(),
        "{}",
        outcome.results[0].result.content
    );
    assert!(outcome.results[1].is_error());
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(before_calls.load(Ordering::SeqCst), 1);
}

// --- Regression: schema-less tools + `null` arguments ---------------------
//
// Preflight consumed the call's payload (`mem::take`) after validation, so the
// schema-less `null` branch became load-bearing. An `AgentToolCall` can carry
// `Value::Null` on paths that construct calls directly (nested/JS tool calls
// pass a `serde_json::Value` through, `nested_tool_calls.rs::execute`), so the
// normalization is observable behavior, not a dead branch.

/// A tool with NO parameter schema (`parameters()` keeps the trait default
/// `None`) that records the exact payload handed to `run`.
struct NoSchemaTool {
    runs: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl AgentTool for NoSchemaTool {
    fn name(&self) -> &'static str {
        "no_schema_tool"
    }

    fn description(&self) -> &'static str {
        "tool without a parameter schema"
    }

    // `parameters()` is intentionally NOT overridden: the trait default is
    // `None`, which is what selects `ArgumentValidation::NoSchema`.
    fn run(&self, args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .expect("no_schema_tool capture mutex poisoned")
            .push(args.clone());
        Box::pin(async { ToolResult::ok("ran") })
    }
}

fn no_schema_registry(
    runs: &Arc<AtomicUsize>,
    seen: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NoSchemaTool {
        runs: runs.clone(),
        seen: seen.clone(),
    }));
    registry
}

/// Schema-less tool + `arguments: null` must pass preflight, run once, and be
/// handed an EMPTY OBJECT rather than the raw `null` payload
/// (`prepare.rs`: `ArgumentValidation::NoSchema if call.arguments.is_null()`
/// takes `Value::Object(Default::default())`; every other payload is moved
/// through untouched).
#[tokio::test]
async fn no_schema_tool_normalizes_null_arguments_to_empty_object() {
    let runs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let registry = no_schema_registry(&runs, &seen);
    let scheduler = ToolScheduler::default();
    let context = ToolContext::new(".");

    let outcome = scheduler
        .execute_batch(
            vec![AgentToolCall::new(
                0,
                "null-args-call",
                "no_schema_tool",
                serde_json::Value::Null,
            )],
            &registry,
            &context,
            None,
        )
        .await;

    assert_eq!(outcome.results.len(), 1);
    assert!(
        !outcome.results[0].is_error(),
        "a schema-less call with null arguments must not fail preflight: {}",
        outcome.results[0].result.content
    );
    assert_eq!(outcome.execution_start.len(), 1);
    assert_eq!(outcome.execution_end.len(), 1);
    assert!(matches!(
        outcome.execution_end[0],
        pir::agent::tool_batch::ToolExecutionEvent::End {
            is_error: false,
            ..
        }
    ));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the call must reach the tool, not be dropped by preflight"
    );

    let seen = seen.lock().expect("capture mutex poisoned");
    assert_eq!(seen.len(), 1, "the tool must run exactly once");
    assert!(
        !seen[0].is_null(),
        "the tool must not receive the raw null payload: {}",
        seen[0]
    );
    assert_eq!(
        seen[0],
        serde_json::json!({}),
        "prepare.rs normalizes a schema-less null payload to an empty object"
    );
    assert_eq!(
        seen[0].as_object().map(serde_json::Map::len),
        Some(0),
        "the normalized payload must be an empty object"
    );
}

/// Control for the same path: a schema-less tool called with a normal object
/// payload receives it byte-for-byte unchanged (the `null` branch must not be
/// taken, and the `mem::take` move must not perturb the value).
#[tokio::test]
async fn no_schema_tool_forwards_object_arguments_unchanged() {
    let runs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let registry = no_schema_registry(&runs, &seen);
    let scheduler = ToolScheduler::default();
    let context = ToolContext::new(".");

    let arguments = serde_json::json!({
        "path": "a.txt",
        "count": 3,
        "nested": { "list": [1, 2, 3], "flag": true },
        "maybe": null,
    });
    let expected = arguments.clone();

    let outcome = scheduler
        .execute_batch(
            vec![AgentToolCall::new(
                0,
                "object-args-call",
                "no_schema_tool",
                arguments,
            )],
            &registry,
            &context,
            None,
        )
        .await;

    assert_eq!(outcome.results.len(), 1);
    assert!(
        !outcome.results[0].is_error(),
        "{}",
        outcome.results[0].result.content
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let seen = seen.lock().expect("capture mutex poisoned");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0], expected,
        "a non-null payload must be passed through unchanged"
    );
}

/// Contrast: a tool WITH a schema is NOT normalized — the declared
/// `type: "object"` rejects a `null` payload during preflight
/// (`validate_schema_value` -> `value_matches_type`, path `$`) and the tool
/// never runs. Only the schema-less branch coerces `null` to `{}`.
#[tokio::test]
async fn schema_tool_rejects_null_arguments_instead_of_normalizing_them() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(SchemaTool {
        runs: runs.clone(),
        schema: schema(),
    }));
    let scheduler = ToolScheduler::default();
    let context = ToolContext::new(".");

    let outcome = scheduler
        .execute_batch(
            vec![call(serde_json::Value::Null)],
            &registry,
            &context,
            None,
        )
        .await;

    assert_eq!(outcome.results.len(), 1);
    assert!(
        outcome.results[0].is_error(),
        "a null payload must not be silently coerced for a schema'd tool: {}",
        outcome.results[0].result.content
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a rejected call must not run"
    );
    // Preflight failures still emit start + immediate end, matching the
    // existing error-path assertions above.
    assert_eq!(outcome.execution_start.len(), 1);
    assert_eq!(outcome.execution_end.len(), 1);
    assert!(matches!(
        outcome.execution_end[0],
        pir::agent::tool_batch::ToolExecutionEvent::End { is_error: true, .. }
    ));
    // `validate_arguments` wraps the argument error in upstream's shape
    // (`Validation failed for tool "<name>":` + violations + the received
    // payload, validation.ts:344-352) and `validate_schema_value` reports the
    // root type mismatch at `$`.
    assert!(
        outcome.results[0]
            .result
            .content
            .starts_with("Validation failed for tool \"schema_tool\":\n"),
        "unexpected error content: {}",
        outcome.results[0].result.content
    );
    assert!(
        outcome.results[0]
            .result
            .content
            .contains("expected object, got null"),
        "unexpected error content: {}",
        outcome.results[0].result.content
    );
}

// --- Upstream preflight normalizations (CL18 audit item 6, P2b) -----------
//
// `validateToolArguments` (validation.ts:335-342) does not validate the raw
// payload straight away: it clones it, runs `normalizeOptionalNulls`, runs the
// schema-directed `Value.Convert`, and only then validates. A payload that the
// normalization made legal is ACCEPTED — so a model that sends `"5"` for a
// `"number"` field, or an optional `null`, does not lose a turn. A payload that
// is still illegal is rejected exactly as before, with every violation and the
// RAW arguments.

/// A schema with an optional `number`, an optional `integer`, a required
/// `string`, a required `integer`, and a nullable optional field (no
/// production tool declares that last shape; it pins the rule's boundary
/// rather than a corpus member).
fn coercion_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["path", "retries"],
        "properties": {
            "path": { "type": "string" },
            "retries": { "type": "integer" },
            "offset": { "type": "number" },
            "limit": { "type": "integer" },
            "note": { "type": ["string", "null"] },
        },
        "additionalProperties": false
    })
}

/// One preflight run: `(the arguments the tool received, the error text)`.
/// Exactly one arm is `Some`; a rejected call never executes.
async fn run_coercion_tool(
    arguments: serde_json::Value,
) -> (Option<serde_json::Value>, Option<String>) {
    struct CaptureTool {
        seen: std::sync::Mutex<Option<serde_json::Value>>,
        schema: serde_json::Value,
    }

    impl AgentTool for CaptureTool {
        fn name(&self) -> &'static str {
            "coercion_tool"
        }

        fn description(&self) -> &'static str {
            "preflight normalization probe"
        }

        fn parameters(&self) -> Option<serde_json::Value> {
            Some(self.schema.clone())
        }

        fn run(
            &self,
            args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            *self.seen.lock().expect("capture mutex poisoned") = Some(args.clone());
            Box::pin(async { ToolResult::ok("ran") })
        }
    }

    let capture = Arc::new(CaptureTool {
        seen: std::sync::Mutex::new(None),
        schema: coercion_schema(),
    });
    let mut registry = ToolRegistry::new();
    registry.register(capture.clone());
    let outcome = ToolScheduler::default()
        .execute_batch(
            vec![AgentToolCall::new(
                0,
                "coercion-call",
                "coercion_tool",
                arguments,
            )],
            &registry,
            &ToolContext::new("."),
            None,
        )
        .await;
    assert_eq!(outcome.results.len(), 1);
    let seen = capture.seen.lock().expect("capture mutex poisoned").clone();
    if outcome.results[0].is_error() {
        assert!(
            seen.is_none(),
            "a rejected call must never execute the tool"
        );
        (None, Some(outcome.results[0].result.content.clone()))
    } else {
        (seen, None)
    }
}

/// Assert the call ran and the tool received exactly `expected`.
async fn assert_coerced_to(arguments: serde_json::Value, expected: serde_json::Value) {
    let (seen, error) = run_coercion_tool(arguments).await;
    assert_eq!(
        seen,
        Some(expected),
        "the call must run with the normalized payload, not be rejected: {error:?}"
    );
    assert_eq!(error, None);
}

#[tokio::test]
async fn numeric_string_is_coerced_for_number_and_integer_fields() {
    // `"number"` takes any finite `Number(value)`; `"integer"` takes it only
    // when the value is integral (upstream `Value.Convert`, narrowed by this
    // tree to the numeric-string arms). The required `retries` string is
    // coerced too — the rule is per declared field, not per optionality.
    assert_coerced_to(
        serde_json::json!({ "path": "a", "retries": "3", "offset": "5", "limit": "20" }),
        serde_json::json!({ "path": "a", "retries": 3, "offset": 5, "limit": 20 }),
    )
    .await;
    // JS `Number()` forms: surrounding whitespace and the radix literals.
    assert_coerced_to(
        serde_json::json!({ "path": "a", "retries": " 7 ", "offset": "1.5" }),
        serde_json::json!({ "path": "a", "retries": 7, "offset": 1.5 }),
    )
    .await;
    assert_coerced_to(
        serde_json::json!({ "path": "a", "retries": "0x10" }),
        serde_json::json!({ "path": "a", "retries": 16 }),
    )
    .await;
    // A float string is NOT truncated into an `"integer"` field (upstream
    // `FromInteger` would truncate; this tree deliberately does not, so `5.5`
    // stays illegal instead of silently becoming `5`).
    let (seen, error) =
        run_coercion_tool(serde_json::json!({ "path": "a", "retries": "5.5" })).await;
    assert_eq!(seen, None);
    assert!(
        error
            .as_deref()
            .unwrap_or_default()
            .contains("$.retries: expected integer, got string"),
        "{error:?}"
    );
}

#[tokio::test]
async fn numeric_string_is_coerced_for_the_real_read_schema() {
    // The audit's actual case: `read.offset`/`read.limit` are `"number"`
    // upstream (`core/tools/read.ts:16-17`) and here (`read.rs:161-162`), so a
    // provider that emits `"5"` must reach the tool as a number.
    use pir::coding_agent::tools::register_core_tools;

    let mut core = ToolRegistry::new();
    register_core_tools(&mut core);
    let schema = core
        .get("read")
        .and_then(|tool| tool.parameters())
        .expect("the read tool declares a schema");

    struct ReadProbe {
        seen: std::sync::Mutex<Option<serde_json::Value>>,
        schema: serde_json::Value,
    }
    impl AgentTool for ReadProbe {
        fn name(&self) -> &'static str {
            "read"
        }
        fn description(&self) -> &'static str {
            "read schema probe"
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(self.schema.clone())
        }
        fn run(
            &self,
            args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            *self.seen.lock().expect("capture mutex poisoned") = Some(args.clone());
            Box::pin(async { ToolResult::ok("ran") })
        }
    }

    let probe = Arc::new(ReadProbe {
        seen: std::sync::Mutex::new(None),
        schema,
    });
    let mut registry = ToolRegistry::new();
    registry.register(probe.clone());
    let outcome = ToolScheduler::default()
        .execute_batch(
            vec![AgentToolCall::new(
                0,
                "read-call",
                "read",
                serde_json::json!({ "path": "x", "offset": "2", "limit": "3" }),
            )],
            &registry,
            &ToolContext::new("."),
            None,
        )
        .await;
    assert!(
        !outcome.results[0].is_error(),
        "a numeric string for read.offset/limit must pass preflight: {}",
        outcome.results[0].result.content
    );
    assert_eq!(
        probe.seen.lock().expect("capture mutex poisoned").clone(),
        Some(serde_json::json!({ "path": "x", "offset": 2, "limit": 3 }))
    );
}

#[tokio::test]
async fn optional_null_is_dropped_before_validation() {
    // Upstream `normalizeOptionalNulls`: an OPTIONAL property whose own schema
    // rejects `null` is deleted, so the call validates and runs.
    assert_coerced_to(
        serde_json::json!({ "path": "a", "retries": 1, "offset": null, "limit": null }),
        serde_json::json!({ "path": "a", "retries": 1 }),
    )
    .await;

    // Nested: the drop recurses through declared object properties.
    struct NestedTool;
    impl AgentTool for NestedTool {
        fn name(&self) -> &'static str {
            "nested_tool"
        }
        fn description(&self) -> &'static str {
            "nested optional-null probe"
        }
        fn parameters(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({
                "type": "object",
                "required": ["outer"],
                "properties": {
                    "outer": {
                        "type": "object",
                        "required": ["keep"],
                        "properties": {
                            "keep": { "type": "string" },
                            "drop": { "type": "number" },
                        },
                        "additionalProperties": false,
                    },
                },
                "additionalProperties": false,
            }))
        }
        fn run(
            &self,
            args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'static, ToolResult> {
            let rendered = args.to_string();
            Box::pin(async move { ToolResult::ok(rendered) })
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NestedTool));
    let outcome = ToolScheduler::default()
        .execute_batch(
            vec![AgentToolCall::new(
                0,
                "nested-call",
                "nested_tool",
                serde_json::json!({ "outer": { "keep": "k", "drop": null } }),
            )],
            &registry,
            &ToolContext::new("."),
            None,
        )
        .await;
    assert!(
        !outcome.results[0].is_error(),
        "a nested optional null must be dropped: {}",
        outcome.results[0].result.content
    );
    assert_eq!(
        outcome.results[0].result.content,
        "{\"outer\":{\"keep\":\"k\"}}"
    );
}

#[tokio::test]
async fn required_null_and_nullable_optional_null_are_rejected_or_kept() {
    // `required` is not exempt: the null is kept, the validator reports the
    // type, and the error echoes the RAW payload.
    let (seen, error) =
        run_coercion_tool(serde_json::json!({ "path": "a", "retries": null })).await;
    assert_eq!(seen, None);
    let error = error.expect("a required null must be rejected");
    assert!(
        error.contains("$.retries: expected integer, got null"),
        "{error}"
    );
    assert!(
        error.contains("Received arguments:\n{\n  \"path\": \"a\",\n  \"retries\": null\n}"),
        "the error must echo the raw payload, not a normalized one: {error}"
    );

    // A nullable declaration keeps its null: `["string", "null"]` accepts it,
    // so nothing is dropped and the call runs unchanged.
    assert_coerced_to(
        serde_json::json!({ "path": "a", "retries": 1, "note": null }),
        serde_json::json!({ "path": "a", "retries": 1, "note": null }),
    )
    .await;
}

#[tokio::test]
async fn genuinely_wrong_types_still_fail_and_list_every_violation() {
    // Coercion must not paper over real mistakes: `"abc"` is not a number, a
    // boolean is not a string, and an unknown key is still additional. All
    // three are reported together, with the arguments the model sent.
    let raw = serde_json::json!({
        "path": true,
        "retries": "abc",
        "offset": "also-not-a-number",
        "bogus": 1,
    });
    let (seen, error) = run_coercion_tool(raw.clone()).await;
    assert_eq!(seen, None, "an invalid call must never reach the tool");
    let error = error.expect("the call must be rejected");
    assert!(
        error.starts_with("Validation failed for tool \"coercion_tool\":\n"),
        "{error}"
    );
    for violation in [
        "  - $.offset: expected number, got string",
        "  - $.retries: expected integer, got string",
        "  - $.path: expected string, got boolean",
        "  - $: additional property 'bogus' is not allowed",
    ] {
        assert!(error.contains(violation), "missing {violation:?}: {error}");
    }
    let received: serde_json::Value = serde_json::from_str(
        error
            .split_once("\n\nReceived arguments:\n")
            .expect("the payload echo")
            .1,
    )
    .expect("the echo is JSON");
    assert_eq!(
        received, raw,
        "the error must echo the exact arguments the model sent"
    );
}

#[tokio::test]
async fn a_top_level_null_payload_is_never_coerced_to_an_object() {
    // Upstream `normalizeOptionalNulls`/`Value.Convert` never turn a root
    // `null` into `{}`: only a declared object's optional properties are
    // normalized. A schema'd tool keeps rejecting a root null.
    let (seen, error) = run_coercion_tool(serde_json::Value::Null).await;
    assert_eq!(seen, None);
    let error = error.expect("a root null must be rejected");
    assert!(error.contains("$: expected object, got null"), "{error}");
}
