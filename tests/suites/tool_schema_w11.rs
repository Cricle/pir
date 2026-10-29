//! W11-L: the tool-argument JSON Schema validator (`validate_schema_value`,
//! `src/agent/tool_scheduler/prepare.rs`) — decision lock-in.
//!
//! The validator stays hand-rolled; the measured decision is recorded on the
//! function itself (no compile-once seam for the per-call
//! `AgentTool::parameters()` value or the cache-less `tool_repair_schema_check`
//! seam, a model-visible `{path}: expected ...` error contract to preserve,
//! eight new lock lines plus a second TLS stack, and a two-keyword vocabulary
//! gap). This suite is the regression net for that decision: every supported
//! keyword, the schema-error vs argument-error split, fail-closed behavior,
//! bounded recursion on a pathological schema, and the three-verdict
//! `tool_repair_schema_check` contract.
//!
//! How verdicts are obtained: `validate_schema_value` and its
//! `SchemaValidationError` are module-private, so every case runs through a
//! probe tool on the public [`ToolScheduler::execute_batch`] path — the same
//! walk production preflight runs (`prepare_calls` -> `validate_arguments`).
//! The two error arms are distinguishable by the model-facing prefix:
//! `Validation failed for tool "<name>":` (argument error; upstream
//! `validateToolArguments`, validation.ts:344-352) vs
//! `Tool '<name>' has an invalid argument schema: ...` (schema error). No
//! test-side validator exists to drift from the production one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};
use tokio::runtime::Runtime;

use pir::agent::tool_batch::{AgentToolCall, ToolResult};
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent::tool_scheduler::ToolScheduler;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A tool whose only purpose is to carry a schema through preflight and record
/// whether it ran.
struct ProbeTool {
    runs: Arc<AtomicUsize>,
    schema: Value,
}

impl AgentTool for ProbeTool {
    fn name(&self) -> &'static str {
        "schema_probe"
    }

    fn description(&self) -> &'static str {
        "W11-L schema validator probe"
    }

    fn parameters(&self) -> Option<Value> {
        Some(self.schema.clone())
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { ToolResult::ok("ran") })
    }
}

/// One probe-visible outcome of a preflight validation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// The payload passed: the tool ran.
    Accepted,
    /// The payload was rejected: every violation (`  - ` stripped, newline
    /// joined) plus the payload the error reported as received.
    Rejected { violations: String, received: Value },
    /// The declared schema was rejected (`has an invalid argument schema`).
    SchemaError(String),
}

/// Shared runtime: `block_on` on one multi-thread runtime is safe from the
/// parallel libtest threads.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("test runtime"))
}

/// Run the production preflight walk of `arguments` against `schema`.
///
/// Takes ownership: the validation path clones the schema
/// (`AgentTool::parameters`) and moves the payload, and the deep-schema case
/// must not pay an extra deep clone.
///
/// Returns the verdict and whether the tool executed.
fn check(schema: Value, arguments: Value) -> (Verdict, bool) {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ProbeTool {
        runs: runs.clone(),
        schema,
    }));
    let outcome = runtime().block_on(async {
        ToolScheduler::default()
            .execute_batch(
                vec![AgentToolCall::new(0, "probe", "schema_probe", arguments)],
                &registry,
                &ToolContext::new("."),
                None,
            )
            .await
    });

    let finalized = outcome
        .results
        .first()
        .unwrap_or_else(|| panic!("preflight must finalize the call: {outcome:?}"));
    let content = &finalized.result.content;
    let verdict = if !finalized.is_error() {
        Verdict::Accepted
    } else if let Some(rest) =
        content.strip_prefix("Validation failed for tool \"schema_probe\":\n")
    {
        // Upstream shape (validation.ts:344-352): every violation as a
        // `  - path: message` line, a blank line, then the received payload.
        let (violations, received) = match rest.split_once("\n\nReceived arguments:\n") {
            Some((violations, received)) => (violations, received),
            None => panic!("the argument error must carry the received payload: {content}"),
        };
        let violations = violations
            .lines()
            .map(|line| {
                line.strip_prefix("  - ")
                    .unwrap_or_else(|| panic!("violation line shape: {line:?}"))
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        Verdict::Rejected {
            violations,
            received: serde_json::from_str(received)
                .unwrap_or_else(|_| panic!("received payload is not JSON: {received}")),
        }
    } else if let Some(message) =
        content.strip_prefix("Tool 'schema_probe' has an invalid argument schema: ")
    {
        Verdict::SchemaError(message.to_owned())
    } else {
        panic!("unexpected preflight error shape: {content}");
    };
    (verdict, runs.load(Ordering::SeqCst) == 1)
}

/// The argument-error message, panicking on any other verdict.
fn rejected(schema: &Value, arguments: &Value) -> String {
    match check(schema.clone(), arguments.clone()) {
        (
            Verdict::Rejected {
                violations,
                received,
            },
            ran,
        ) => {
            assert!(!ran, "a rejected call must never reach the tool");
            assert_eq!(
                received, *arguments,
                "the error must echo the arguments the model sent"
            );
            violations
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
}

/// The schema-error message, panicking on any other verdict.
fn schema_error(schema: &Value, arguments: &Value) -> String {
    match check(schema.clone(), arguments.clone()) {
        (Verdict::SchemaError(message), ran) => {
            assert!(!ran, "a broken schema must never run the tool");
            message
        }
        other => panic!("expected a schema error, got {other:?}"),
    }
}

/// The payload was accepted and the tool ran.
fn accepted(schema: &Value, arguments: &Value) {
    match check(schema.clone(), arguments.clone()) {
        (Verdict::Accepted, true) => {}
        other => panic!("expected acceptance and one run, got {other:?}"),
    }
}

fn object_schema(properties: Value, required: Value) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

// ---------------------------------------------------------------------------
// 1. Every supported keyword accepts its valid form and rejects its invalid form
// ---------------------------------------------------------------------------

#[test]
fn type_accepts_every_json_type_and_rejects_the_wrong_one() {
    for (ty, value) in [
        ("null", json!(null)),
        ("boolean", json!(true)),
        ("object", json!({})),
        ("array", json!([])),
        ("number", json!(1.5)),
        ("integer", json!(7)),
        ("string", json!("s")),
    ] {
        accepted(&json!({ "type": ty }), &value);
    }

    // Integer accepts integral floats (JSON Schema's mathematical definition)
    // and both integer widths.
    accepted(&json!({ "type": "integer" }), &json!(3.0));
    accepted(
        &json!({ "type": "integer" }),
        &json!(-9_007_199_254_740_991_i64),
    );

    assert_eq!(
        rejected(&json!({ "type": "integer" }), &json!(1.5)),
        "$: expected integer, got number"
    );
    assert_eq!(
        rejected(&json!({ "type": "object" }), &json!(null)),
        "$: expected object, got null"
    );
    assert_eq!(
        rejected(&json!({ "type": "string" }), &json!(["a"])),
        "$: expected string, got array"
    );

    // A `type` union matches any member; the message names every member.
    accepted(&json!({ "type": ["string", "null"] }), &json!(null));
    accepted(&json!({ "type": ["string", "null"] }), &json!("x"));
    assert_eq!(
        rejected(&json!({ "type": ["string", "null"] }), &json!(4)),
        "$: expected string or null, got number"
    );
}

#[test]
fn enum_requires_membership_and_validates_its_own_declaration() {
    let schema = json!({ "type": "string", "enum": ["create", "update"] });
    accepted(&schema, &json!("create"));
    accepted(&schema, &json!("update"));
    assert_eq!(
        rejected(&schema, &json!("delete")),
        "$: value is not one of the values in enum"
    );
    // Enum values compare structurally, so objects/arrays/null are legal members.
    let structural = json!({ "enum": [{ "a": 1 }, [1, 2], null] });
    accepted(&structural, &json!({ "a": 1 }));
    accepted(&structural, &json!([1, 2]));
    accepted(&structural, &json!(null));
}

#[test]
fn required_reports_every_missing_property_in_sorted_order() {
    let schema = object_schema(json!({ "a": {}, "b": {}, "c": {} }), json!(["c", "a", "b"]));
    accepted(&schema, &json!({ "a": 1, "b": 2, "c": 3 }));
    // Every missing name is reported, sorted, regardless of the schema's
    // required-array order (upstream `allErrors`).
    assert_eq!(
        rejected(&schema, &json!({ "c": 3 })),
        "$: missing required property 'a'\n$: missing required property 'b'"
    );
    assert_eq!(
        rejected(&schema, &json!({ "a": 1 })),
        "$: missing required property 'b'\n$: missing required property 'c'"
    );
    // A required property declared but absent from `properties` is still required.
    let undeclared = json!({ "type": "object", "required": ["ghost"] });
    assert_eq!(
        rejected(&undeclared, &json!({})),
        "$: missing required property 'ghost'"
    );
}

#[test]
fn properties_recurse_with_dotted_and_bracketed_paths() {
    let schema = json!({
        "type": "object",
        "properties": {
            "outer": {
                "type": "object",
                "properties": { "inner": { "type": "integer" } },
            },
            "weird name": { "type": "boolean" },
        },
    });
    accepted(
        &schema,
        &json!({ "outer": { "inner": 1 }, "weird name": true }),
    );
    assert_eq!(
        rejected(&schema, &json!({ "outer": { "inner": "no" } })),
        "$.outer.inner: expected integer, got string"
    );
    // A non-identifier property name uses the JSON-string bracket form.
    assert_eq!(
        rejected(&schema, &json!({ "weird name": 1 })),
        "$[\"weird name\"]: expected boolean, got number"
    );
    // An undeclared property is unconstrained (no `additionalProperties`).
    accepted(&schema, &json!({ "unknown": { "deeply": [1, 2] } }));
}

#[test]
fn additional_properties_false_schema_and_true() {
    let closed = json!({
        "type": "object",
        "properties": { "known": { "type": "string" } },
        "additionalProperties": false,
    });
    accepted(&closed, &json!({ "known": "x" }));
    accepted(&closed, &json!({}));
    assert_eq!(
        rejected(&closed, &json!({ "zed": 1, "alpha": 2 })),
        "$: additional property 'alpha' is not allowed\n$: additional property 'zed' is not allowed"
    );

    // Without `properties`, every key is additional.
    let bare_closed = json!({ "type": "object", "additionalProperties": false });
    accepted(&bare_closed, &json!({}));
    assert_eq!(
        rejected(&bare_closed, &json!({ "anything": true })),
        "$: additional property 'anything' is not allowed"
    );

    // A schema-valued `additionalProperties` constrains the extras.
    let typed_extras = json!({
        "type": "object",
        "properties": { "known": { "type": "string" } },
        "additionalProperties": { "type": "number" },
    });
    accepted(&typed_extras, &json!({ "known": "x", "extra": 1 }));
    assert_eq!(
        rejected(&typed_extras, &json!({ "extra": "no" })),
        "$.extra: expected number, got string"
    );

    // Without `properties`, a schema-valued `additionalProperties` applies to all.
    let all_typed = json!({ "type": "object", "additionalProperties": { "type": "boolean" } });
    accepted(&all_typed, &json!({ "a": true, "b": false }));
    assert_eq!(
        rejected(&all_typed, &json!({ "a": 1 })),
        "$.a: expected boolean, got number"
    );

    // `true` accepts anything, including nested containers.
    accepted(
        &json!({ "type": "object", "additionalProperties": true }),
        &json!({ "a": { "b": [1, null] } }),
    );
}

#[test]
fn items_recurses_and_min_max_items_bound_the_array() {
    let schema = json!({ "type": "array", "items": { "type": "string" } });
    accepted(&schema, &json!([]));
    accepted(&schema, &json!(["a", "b"]));
    assert_eq!(
        rejected(&schema, &json!(["a", 2])),
        "$[1]: expected string, got number"
    );

    let bounded = json!({
        "type": "object",
        "properties": {
            "scripts": { "type": "array", "minItems": 1, "maxItems": 2,
                         "items": { "type": "object" } },
        },
    });
    accepted(&bounded, &json!({ "scripts": [{}] }));
    accepted(&bounded, &json!({ "scripts": [{}, {}] }));
    assert_eq!(
        rejected(&bounded, &json!({ "scripts": [] })),
        "$.scripts: expected at least 1 items, got 0"
    );
    assert_eq!(
        rejected(&bounded, &json!({ "scripts": [{}, {}, {}] })),
        "$.scripts: expected at most 2 items, got 3"
    );
    // The bounds only apply to array instances: a non-array payload is a type
    // error when `type` is declared, and unconstrained otherwise.
    assert_eq!(
        rejected(&json!({ "type": "array", "minItems": 1 }), &json!(null)),
        "$: expected array, got null"
    );
    accepted(&json!({ "minItems": 5 }), &json!({ "not": "an array" }));
}

/// The production schemas this tree declares must be enforced, not merely
/// carried: `ctx_batch_execute` is the only in-tree tool whose schema uses
/// `minItems`/`maxItems`, so it is the real-corpus regression guard for that
/// keyword pair.
#[test]
fn real_ctx_batch_execute_bounds_are_enforced_at_preflight() {
    let schema = batch_schema();

    accepted(&schema, &json!({ "scripts": [{ "code": "print(1)" }] }));
    assert_eq!(
        rejected(&schema, &json!({ "scripts": [] })),
        "$.scripts: expected at least 1 items, got 0"
    );
    let too_many: Vec<Value> = (0..17)
        .map(|index| json!({ "code": format!("print({index})") }))
        .collect();
    assert_eq!(
        rejected(&schema, &json!({ "scripts": too_many })),
        "$.scripts: expected at most 16 items, got 17"
    );
    let too_many_queries: Vec<Value> = (0..33).map(|index| json!(format!("q{index}"))).collect();
    assert_eq!(
        rejected(
            &schema,
            &json!({ "scripts": [{ "code": "print(1)" }], "queries": too_many_queries })
        ),
        "$.queries: expected at most 32 items, got 33"
    );
}

/// The real `ctx_batch_execute` schema from the production plugin registration.
#[cfg(feature = "context-mode")]
fn batch_schema() -> Value {
    use pir::plugins::builtin::{ContextModePlugin, CTX_BATCH_EXECUTE_TOOL_NAME};
    use pir::plugins::plugin::{
        AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend,
    };

    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            let mut registrations = PluginRegistrations::default();
            ContextModePlugin::with_db_path(
                std::env::temp_dir().join(format!("pir-w11l-schema-{}.db", std::process::id())),
            )
            .register(&mut RegistrationContext::new(&mut registrations))
            .expect("register the context-mode plugin");
            registrations
                .tools
                .into_iter()
                .find(|(definition, _)| definition.name == CTX_BATCH_EXECUTE_TOOL_NAME)
                .and_then(|(_, backend)| match backend {
                    ToolBackend::Rust(tool) => tool.parameters(),
                    ToolBackend::Js { .. } => None,
                })
                .expect("ctx_batch_execute declares a parameters schema")
        })
        .clone()
}

/// Corpus guard without the `context-mode` builtin: the same declaration shape,
/// so the keyword coverage still runs in a slim build.
#[cfg(not(feature = "context-mode"))]
fn batch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "scripts": {
                "type": "array",
                "minItems": 1,
                "maxItems": 16,
                "items": { "type": "object",
                           "properties": { "code": { "type": "string" } },
                           "required": ["code"],
                           "additionalProperties": false },
            },
            "queries": { "type": "array", "items": { "type": "string" }, "maxItems": 32 },
            "source": { "type": "string" },
            "timeout": { "type": "number" },
        },
        "required": ["scripts"],
        "additionalProperties": false,
    })
}

// ---------------------------------------------------------------------------
// 2. Malformed schemas take the schema-error arm, never the argument-error arm
// ---------------------------------------------------------------------------

#[test]
fn malformed_schemas_are_schema_errors_not_argument_errors() {
    // Keywords are walked only for the instance types they apply to (JSON
    // Schema semantics), so each case carries a payload of the matching type;
    // otherwise the type error would be (correctly) reported first.
    let object_payload = json!({ "a": [1] });
    let array_payload: Value = json!([]);
    let cases: Vec<(Value, &Value, &str)> = vec![
        (
            json!("not an object"),
            &object_payload,
            "schema at $ must be an object",
        ),
        (json!([1]), &object_payload, "schema at $ must be an object"),
        (
            json!({ "enum": "nope" }),
            &object_payload,
            "enum at $ must be an array",
        ),
        (
            json!({ "enum": [] }),
            &object_payload,
            "enum at $ must not be empty",
        ),
        (
            json!({ "type": 1 }),
            &object_payload,
            "type must be a string or array of strings",
        ),
        (
            json!({ "type": ["string", 2] }),
            &object_payload,
            "type must contain only strings",
        ),
        (
            json!({ "type": [] }),
            &object_payload,
            "type array must not be empty",
        ),
        (
            json!({ "type": "int" }),
            &object_payload,
            "unsupported type 'int'",
        ),
        (
            json!({ "type": "object", "required": "a" }),
            &object_payload,
            "required at $ must be an array",
        ),
        (
            json!({ "type": "object", "required": ["a", 1] }),
            &object_payload,
            "required at $ must contain only strings",
        ),
        (
            json!({ "type": "object", "properties": [] }),
            &object_payload,
            "properties at $ must be an object",
        ),
        (
            json!({ "type": "object", "additionalProperties": 1 }),
            &object_payload,
            "additionalProperties at $ must be a boolean or object",
        ),
        (
            json!({ "type": "object", "additionalProperties": "no" }),
            &object_payload,
            "additionalProperties at $ must be a boolean or object",
        ),
        (
            json!({ "type": "array", "items": "no" }),
            &array_payload,
            "items at $ must be an object",
        ),
        (
            json!({ "type": "array", "minItems": "1" }),
            &array_payload,
            "minItems at $ must be a non-negative integer",
        ),
        (
            json!({ "type": "array", "minItems": -1 }),
            &array_payload,
            "minItems at $ must be a non-negative integer",
        ),
        (
            json!({ "type": "array", "maxItems": 1.5 }),
            &array_payload,
            "maxItems at $ must be a non-negative integer",
        ),
    ];
    for (schema, payload, expected) in cases {
        assert_eq!(schema_error(&schema, payload), expected, "schema: {schema}");
    }

    // A malformed nested schema is reported at its own path.
    let nested = json!({
        "type": "object",
        "properties": { "outer": { "type": "object",
                                    "properties": { "inner": { "type": [] } } } },
    });
    assert_eq!(
        schema_error(&nested, &json!({ "outer": { "inner": 1 } })),
        "type array must not be empty"
    );
    // The same nested malformation under `items` carries the index path.
    let nested_items = json!({
        "type": "array",
        "items": { "type": "object", "properties": { "x": { "enum": 3 } } },
    });
    assert_eq!(
        schema_error(&nested_items, &json!([{ "x": 1 }])),
        "enum at $[0].x must be an array"
    );
}

#[test]
fn a_malformed_schema_wins_over_an_also_invalid_payload() {
    // Fail closed on the schema first: a broken declaration is not allowed to
    // masquerade as a model mistake, and the tool never runs.
    let (verdict, ran) = check(
        json!({ "type": "object", "required": "oops" }),
        json!({ "definitely": "wrong" }),
    );
    assert!(matches!(verdict, Verdict::SchemaError(_)), "{verdict:?}");
    assert!(!ran);
}

// ---------------------------------------------------------------------------
// 3. Fail-closed behavior
// ---------------------------------------------------------------------------

#[test]
fn an_empty_schema_accepts_anything() {
    // `{}` declares no constraint: every payload type is admitted, including
    // null (a schema'd tool is not null-normalized; the schema itself does not
    // forbid null, so preflight must not invent a restriction).
    for payload in [
        json!(null),
        json!(1),
        json!("x"),
        json!([]),
        json!({ "a": 1 }),
    ] {
        accepted(&json!({}), &payload);
    }
}

#[test]
fn schema_keywords_on_the_wrong_instance_type_are_ignored_not_fatal() {
    // JSON Schema semantics: object keywords do not apply to non-objects and
    // array keywords do not apply to non-arrays. Enforcing them on the wrong
    // instance type would reject valid calls or misfire the schema-error arm.
    accepted(
        &json!({ "required": ["a"], "additionalProperties": false }),
        &json!("s"),
    );
    accepted(&json!({ "minItems": 4, "maxItems": 5 }), &json!({ "a": 1 }));
    accepted(
        &json!({ "required": ["a"], "properties": { "a": { "type": "string" } } }),
        &json!([1]),
    );
}

#[test]
fn error_selection_is_deterministic_regardless_of_payload_order() {
    // Object properties are walked in sorted name order: violations come back
    // in that order no matter how the model ordered the keys (upstream
    // `allErrors` reports them together).
    let schema = json!({
        "type": "object",
        "properties": { "zeta": { "type": "string" }, "alpha": { "type": "string" } },
    });
    assert_eq!(
        rejected(&schema, &json!({ "zeta": 1, "alpha": 2 })),
        "$.alpha: expected string, got number\n$.zeta: expected string, got number"
    );
    // Arrays report their failing indices lowest-first.
    assert_eq!(
        rejected(
            &json!({ "type": "array", "items": { "type": "integer" } }),
            &json!([1, "x", "y"])
        ),
        "$[1]: expected integer, got string\n$[2]: expected integer, got string"
    );
}

/// P4 (upstream validation.ts:344-352): one turn must surface EVERY mistake of
/// the call plus the payload that was received, not just the first one.
#[test]
fn every_violation_and_the_received_arguments_are_reported() {
    let schema = json!({
        "type": "object",
        "required": ["path"],
        "properties": {
            "path": { "type": "string" },
            "offset": { "type": "number" },
            "limit": { "type": "number" },
        },
        "additionalProperties": false,
    });
    let arguments = json!({ "offset": "five", "limit": true, "bogus": 1 });
    match check(schema, arguments.clone()) {
        (
            Verdict::Rejected {
                violations,
                received,
            },
            ran,
        ) => {
            assert!(!ran, "a rejected call must never reach the tool");
            assert_eq!(
                violations,
                "$: missing required property 'path'\n\
                 $.limit: expected number, got boolean\n\
                 $.offset: expected number, got string\n\
                 $: additional property 'bogus' is not allowed",
                "all four violations, in walk order"
            );
            assert_eq!(
                received, arguments,
                "the error must echo the exact arguments the model sent"
            );
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 4. Deep nesting must not blow the stack
// ---------------------------------------------------------------------------

/// The walk is recursive over the schema chain, so a pathological declaration
/// must fail closed (schema error) at `MAX_SCHEMA_DEPTH` instead of recursing
/// once per schema level — which is what would exhaust a 2 MiB libtest thread
/// stack on a large enough declaration. The guarantee is a stack cost bounded
/// by the constant, not by the input.
#[test]
fn a_pathologically_nested_schema_fails_closed_instead_of_overflowing() {
    // Twice the guard (128), so the walk must reject well before the payload
    // ends. The fixture is deliberately not deeper: the probe returns the
    // schema `Value` by value, and `serde_json`'s recursive clone/drop frames
    // (which the walk's depth guard does not bound) would exhaust a 2 MiB
    // libtest stack in the fixture itself, before the validator ran.
    const DEPTH: usize = 256;

    let mut schema = json!({ "type": "integer" });
    let mut payload = json!(1);
    for _ in 0..DEPTH {
        schema = json!({
            "type": "object",
            "properties": { "next": schema },
            "additionalProperties": false,
        });
        payload = json!({ "next": payload });
    }

    match check(schema, payload) {
        (Verdict::SchemaError(message), ran) => {
            assert!(!ran, "the tool must never run under a rejected schema");
            assert!(
                message.contains("exceeds the maximum supported depth"),
                "unexpected schema error: {message}"
            );
        }
        other => panic!("a pathological schema must fail closed, got {other:?}"),
    }

    // A chain under the limit still validates normally: the guard rejects past
    // `MAX_SCHEMA_DEPTH`, it does not clip ordinary nesting.
    let mut shallow = json!({ "type": "integer" });
    let mut shallow_payload = json!(1);
    for _ in 0..8 {
        shallow = json!({ "type": "object", "properties": { "next": shallow } });
        shallow_payload = json!({ "next": shallow_payload });
    }
    accepted(&shallow, &shallow_payload);
}

// ---------------------------------------------------------------------------
// 5. The three-verdict `tool_repair_schema_check` contract
// ---------------------------------------------------------------------------

/// The three-verdict contract, exercised as far as an integration test can:
/// `tool_repair_schema_check` itself is `pub(crate)` (re-exported crate-only in
/// `tool_scheduler::mod.rs`), so its *sole consumer*, the live-repair core, is
/// driven directly instead. The mapping pinned here is the documented one:
/// preflight `Ok` -> `Accepted`, argument error -> `Rejected`, schema error ->
/// `Unavailable`; a broken schema must short-circuit to `Unchanged` even when
/// a repair rule would otherwise fire.
#[cfg(feature = "tool-repair")]
#[test]
fn tool_repair_schema_check_three_verdict_contract() {
    use pir::plugins::tool_repair::repair::{
        repair_tool_call_arguments, LiveRepairStatus, SchemaCheck,
    };

    // The same three-arm mapping `tool_repair_schema_check` applies, fed from
    // the production preflight walk.
    fn production_check(arguments: &Value, schema: &Value) -> SchemaCheck {
        match check(schema.clone(), arguments.clone()).0 {
            Verdict::Accepted => SchemaCheck::Accepted,
            Verdict::Rejected { .. } => SchemaCheck::Rejected,
            Verdict::SchemaError(_) => SchemaCheck::Unavailable,
        }
    }

    let schema = json!({
        "type": "object",
        "properties": { "n": { "type": "integer" } },
        "required": ["n"],
    });

    // Accepted: valid input stays untouched.
    let accepted =
        repair_tool_call_arguments("probe", &json!({ "n": 1 }), &schema, &production_check);
    assert_eq!(accepted.status, LiveRepairStatus::Unchanged);
    assert!(accepted.rules_fired.is_empty(), "{accepted:?}");
    assert_eq!(
        production_check(&json!({ "n": 1 }), &schema),
        SchemaCheck::Accepted
    );

    // Rejected: invalid input is reported as unrepairable here (no rule can
    // fix a number where an object is required), never silently accepted.
    let rejected = repair_tool_call_arguments(
        "probe",
        &json!({ "n": "not-an-integer" }),
        &schema,
        &production_check,
    );
    assert_eq!(rejected.status, LiveRepairStatus::Unrepairable);
    assert!(rejected.repaired.is_none(), "{rejected:?}");
    assert_eq!(
        production_check(&json!({ "n": "not-an-integer" }), &schema),
        SchemaCheck::Rejected
    );

    // Unavailable: an unusable schema yields NO verdict, so the pipeline must
    // pass the call through untouched even though `dropNullOrUndefined` would
    // fire on this payload if the schema were merely Rejected.
    let broken_schema = json!({
        "type": "object",
        "properties": { "x": { "type": "string" } },
        "required": "not-an-array",
    });
    let unavailable = repair_tool_call_arguments(
        "probe",
        &json!({ "x": null }),
        &broken_schema,
        &production_check,
    );
    assert_eq!(unavailable.status, LiveRepairStatus::Unchanged);
    assert!(
        unavailable.rules_fired.is_empty(),
        "no repair may run without a verdict: {unavailable:?}"
    );
    assert_eq!(
        production_check(&json!({ "x": null }), &broken_schema),
        SchemaCheck::Unavailable
    );

    // Control: the very same repair DOES fire when the schema is usable, so
    // the assertions above distinguish a real Unavailable from a no-op.
    let repairable = repair_tool_call_arguments(
        "probe",
        &json!({ "x": null }),
        &json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
        &production_check,
    );
    assert_eq!(repairable.status, LiveRepairStatus::Recovered);
    assert_eq!(
        repairable.rules_fired,
        vec!["dropNullOrUndefined"],
        "{repairable:?}"
    );
}

/// The same split expressed purely through the public scheduler API, so the
/// probe verdicts are pinned independently of the feature gate.
#[test]
fn probe_verdicts_round_trip_the_three_way_split() {
    let schema = object_schema(json!({ "n": { "type": "integer" } }), json!(["n"]));
    assert!(matches!(
        check(schema.clone(), json!({ "n": 1 })),
        (Verdict::Accepted, true)
    ));
    assert!(matches!(
        check(schema, json!({ "n": "x" })),
        (Verdict::Rejected { .. }, false)
    ));
    assert!(matches!(
        check(json!({ "type": 5 }), json!({ "n": 1 })),
        (Verdict::SchemaError(_), false)
    ));
}
