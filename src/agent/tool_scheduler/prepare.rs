//! Source-order preflight: tool lookup, argument validation, resource claims,
//! and before-hooks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::FutureExt;

use crate::agent::tool_batch::{
    AgentToolCall, FinalizedToolCall, PreparedToolCall, ToolExecutionEvent, ToolResult,
};
use crate::agent::tool_dependencies::ResourceKey;
use crate::agent::tool_execution::{
    panic_payload_message, AgentTool, BeforeToolCallResult, ToolContext, ToolEventFaces, ToolHooks,
    ToolRegistry,
};

use super::execute::end_event;
use super::ToolExecutionProgress;

/// One preflighted call plus the scheduler-level facts preflight extracted
/// beside it.
///
/// The `background` request cannot live in `validated_args` (it is stripped
/// before validation, so tools never see it and `additionalProperties: false`
/// schemas accept it) and is kept off the public [`PreparedToolCall`] (its
/// literals are exhaustive outside this crate), so it travels in this
/// scheduler-internal wrapper instead.
pub(super) struct PreparedCall {
    /// The validated call handed to hooks and execution.
    pub(super) call: PreparedToolCall,
    /// The call asked for background execution (`background: true`).
    pub(super) background: bool,
}

/// Preflight every call in source order (lookup, `background` extraction,
/// validation, background policy, before-hook), returning the prepared calls,
/// the error results produced during preflight, and the lifecycle events.
/// Upstream emits `tool_execution_start` for EVERY call before preparation
/// (agent-loop.ts:541-547,597-602); a failed preparation then emits an
/// immediate `tool_execution_end` for the error result
/// (agent-loop.ts:551-556,605-616).
///
/// `callable` is the batch's top-level executable set: the tools declared to
/// the model (`Context.tools`) plus the run's active `direct` tools (the
/// `direct` subset of `LoopDeps::nested_callable`, so hook-hidden declarations
/// stay executable). The full nested-callable set — every registered
/// `codemode`/`deferred` tool included — stays with `ctx.executeTool`/nested
/// calls and does NOT widen this gate: an undeclared `codemode`/`deferred` tool
/// is unknown to the top-level loop, exactly like an undeclared `direct` one.
/// A registered name outside that set is unknown
/// to the loop, exactly like an absent one (upstream resolves a call only in
/// `agent.state.tools` = the declared loadout, agent-session.ts:1623 /
/// agent-loop.ts:713-723), so it produces the same not-found result. `None`
/// keeps the registry-wide default (every registered tool executable), which
/// is what a host that declares no callable set gets.
///
/// The calls are consumed: preflight is their last reader, so each call's
/// argument payload moves into its prepared call instead of being copied.
// The scheduler's per-batch context is a fixed set of borrowed handles; folding
// them into a struct would only move the same fields around.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_calls(
    calls: Vec<AgentToolCall>,
    registry: &ToolRegistry,
    callable: Option<&std::collections::BTreeSet<String>>,
    context: &ToolContext,
    hooks: Option<&ToolHooks>,
    aborted_flag: &Arc<AtomicBool>,
    progress: Option<&ToolExecutionProgress>,
    faces: Option<&ToolEventFaces>,
) -> (
    Vec<PreparedCall>,
    Vec<FinalizedToolCall>,
    Vec<ToolExecutionEvent>,
) {
    let mut prepared: Vec<PreparedCall> = Vec::new();
    let mut results: Vec<FinalizedToolCall> = Vec::new();
    let mut start_events: Vec<ToolExecutionEvent> = Vec::new();
    // The requesting call id for `tool_call`/`tool_result` payloads and the
    // `tool_execution_*` events: the batch context's nested executor carries
    // it exactly when another tool made the call (upstream
    // `parentToolCallId`); a model-issued batch reports none.
    let parent_call_id = context.parent_call_id().map(str::to_owned);

    for mut call in calls {
        // `tool_execution_start` fires for every call, before preflight, and
        // reaches a progress observer at production time (before the batch
        // executes).
        let start_event = ToolExecutionEvent::Start {
            index: call.index,
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            parent_call_id: parent_call_id.clone(),
        };
        if let Some(progress) = progress {
            progress(start_event.clone(), None);
        }
        start_events.push(start_event);
        // Live abort check at this call's preflight start: the flag only
        // records the batch-start snapshot, so a mid-batch abort must be
        // read from the signal itself. The remaining calls become aborted
        // error results, per the module contract.
        if aborted_flag.load(Ordering::Acquire) || context.is_aborted() {
            push_failure(
                &mut results,
                progress,
                faces,
                parent_call_id.as_deref(),
                Some(&call.arguments),
                true,
                error_result(&call, "Operation aborted"),
            )
            .await;
            continue;
        }
        let Some(tool) = registry.get(&call.tool_name) else {
            push_failure(
                &mut results,
                progress,
                faces,
                parent_call_id.as_deref(),
                Some(&call.arguments),
                true,
                error_result(&call, unknown_tool_message(&call.tool_name)),
            )
            .await;
            continue;
        };
        // Executable-set gate (W16-D11): the registry is the full registered
        // set, but a model-issued call may only reach the loadout it was
        // declared. An undeclared `direct`/`hidden`/`model-only` tool — and an
        // undeclared `codemode`/`deferred` tool — stays registered for
        // `codemode`/`tool_search`/nested calls yet is unknown to the
        // top-level loop, so it fails with the SAME result an absent
        // name gets (upstream `agent-loop.ts:713-723`).
        if let Some(callable) = callable {
            if !callable.contains(&call.tool_name) {
                push_failure(
                    &mut results,
                    progress,
                    faces,
                    parent_call_id.as_deref(),
                    Some(&call.arguments),
                    true,
                    error_result(&call, unknown_tool_message(&call.tool_name)),
                )
                .await;
                continue;
            }
        }
        // The shared `background` argument is the scheduler's, not the tool's:
        // take it out BEFORE validation so a tool with
        // `additionalProperties: false` never rejects it, and the tool itself
        // never sees it.
        let background = take_background_argument(&mut call.arguments);
        // A final-parse failure reported by the provider is rejected before
        // schema validation (upstream `validateToolArguments`): the model sees
        // a normal error tool result and can retry, instead of the stream
        // failing or the empty object being validated as if it were real.
        if let Some(error) = call.arguments_error.as_deref() {
            push_failure(
                &mut results,
                progress,
                faces,
                parent_call_id.as_deref(),
                Some(&call.arguments),
                true,
                error_result(
                    &call,
                    format!("Invalid arguments for tool \"{}\": {error}", call.tool_name),
                ),
            )
            .await;
            continue;
        }
        // `prepareArguments` (upstream `AgentTool.prepareArguments`,
        // pkg-agent types.ts:469-473; `prepareToolCallArguments`,
        // agent-loop.ts:704-716): the tool's compatibility shim runs BEFORE
        // schema validation, so a legacy argument shape it repairs is
        // validated in its repaired form. Fail-closed: a failing shim rejects
        // the call here and its output never reaches validation or execution.
        // The capability probe keeps a tool without a shim on the allocation-
        // free preflight path.
        if tool.prepares_arguments() {
            match tool
                .prepare_arguments(std::mem::take(&mut call.arguments))
                .await
            {
                Ok(prepared) => call.arguments = prepared,
                Err(message) if message.trim().is_empty() => {
                    push_failure(
                        &mut results,
                        progress,
                        faces,
                        parent_call_id.as_deref(),
                        Some(&call.arguments),
                        true,
                        error_result(
                            &call,
                            format!("prepareArguments for tool \"{}\" failed", call.tool_name),
                        ),
                    )
                    .await;
                    continue;
                }
                Err(message) => {
                    push_failure(
                        &mut results,
                        progress,
                        faces,
                        parent_call_id.as_deref(),
                        Some(&call.arguments),
                        true,
                        error_result(&call, message),
                    )
                    .await;
                    continue;
                }
            }
        }
        // Validation reads the arguments and, only when the upstream
        // normalizations (optional-`null` drop, numeric-string coercion) are
        // what makes the payload legal, replaces them IN PLACE — so the call's
        // own payload still moves into the prepared call below with no copy on
        // the accepting path.
        let validation =
            match validate_arguments(&call.tool_name, tool.as_ref(), &mut call.arguments) {
                Ok(validation) => validation,
                Err(message) => {
                    push_failure(
                        &mut results,
                        progress,
                        faces,
                        parent_call_id.as_deref(),
                        Some(&call.arguments),
                        true,
                        error_result(&call, message),
                    )
                    .await;
                    continue;
                }
            };
        // The payload every reader below sees: exactly the call's payload
        // except for the schema-less `null` → `{}` normalization. Hooks,
        // mutation-path extraction and execution all take it by value, so no
        // copy is made on the accepting path.
        let mut validated_args = match validation {
            ArgumentValidation::NoSchema if call.arguments.is_null() => {
                serde_json::Value::Object(Default::default())
            }
            _ => std::mem::take(&mut call.arguments),
        };
        // Extension `tool_call` (upstream `beforeToolCall`, agent-loop.ts:738-
        // 766): dispatched AFTER `prepareArguments` and schema validation and
        // BEFORE execution. A handler mutates `input` in place; the rewritten
        // payload is what the tool executes and is deliberately NOT
        // re-validated (upstream types.ts:1250-1253: "No re-validation is
        // performed after mutation"). An invalid payload therefore never
        // reaches a handler, and a rewritten one never fails a second
        // validation.
        if let Some(faces) = faces.filter(|faces| faces.dispatches_tool_call()) {
            match crate::agent::tool_execution::gate_single_tool_call(
                faces.extensions(),
                faces.plugins(),
                &call.call_id,
                &call.tool_name,
                validated_args,
                parent_call_id.as_deref(),
            )
            .await
            {
                Ok(decision) if decision.blocked => {
                    let reason = decision.reason.unwrap_or_else(|| {
                        format!("Tool '{}' was blocked by an extension", call.tool_name)
                    });
                    // Upstream `ToolCallEventResult.terminate` (types.ts:1430-
                    // 1434): a hint that the agent should stop after the
                    // current tool batch when this call is blocked — honored
                    // only when EVERY finalized result in the batch carries
                    // it (ToolBatchOutcome::should_terminate's all() rule;
                    // a mixed batch keeps going).
                    let mut result = ToolResult::error(reason);
                    result.terminate = decision.terminate;
                    // A gate-blocked call never ran: `afterToolCall`/
                    // `tool_result` runs only for executed calls.
                    push_failure(
                        &mut results,
                        progress,
                        Some(faces),
                        parent_call_id.as_deref(),
                        None,
                        false,
                        FinalizedToolCall {
                            index: call.index,
                            call_id: call.call_id.clone(),
                            tool_name: call.tool_name.clone(),
                            duration_ms: None,
                            result,
                            usage: None,
                        },
                    )
                    .await;
                    continue;
                }
                Ok(decision) => validated_args = decision.input,
                Err(error) => {
                    // A failed dispatch blocks the call with the dispatch
                    // error (upstream's "extension failed, blocking
                    // execution" behavior); the tool must not run.
                    push_failure(
                        &mut results,
                        progress,
                        Some(faces),
                        parent_call_id.as_deref(),
                        None,
                        false,
                        FinalizedToolCall {
                            index: call.index,
                            call_id: call.call_id.clone(),
                            tool_name: call.tool_name.clone(),
                            duration_ms: None,
                            result: ToolResult::error(format!(
                                "Extension failed, blocking execution: {error}"
                            )),
                            usage: None,
                        },
                    )
                    .await;
                    continue;
                }
            }
        }
        // Background policy (step 2): mutation tools are admitted — the
        // per-file queue locks are acquired INSIDE the call's own execution
        // future (`execute.rs::run_call_body` → `MutationQueues::run_locked`),
        // so a detached mutation call holds them only across its own run and
        // never across a turn. A tool may still refuse backgrounding through
        // `supports_background` (and the sink may refuse at its limit); the
        // synthesized error lets the model retry the same call in the
        // foreground.
        if background && !tool.supports_background() {
            push_failure(
                &mut results,
                progress,
                faces,
                parent_call_id.as_deref(),
                Some(&validated_args),
                true,
                error_result(
                    &call,
                    format!(
                        "Tool '{}' does not support running in the background; retry the call without \"background\": true.",
                        call.tool_name
                    ),
                ),
            )
            .await;
            continue;
        }
        // Resource claims: canonical mutation paths when the tool writes. The
        // paths come from the executed (post-gate, post-validation) payload.
        let mut mutation_keys = Vec::new();
        if tool.is_mutation_tool() {
            let mutation_paths = tool.mutation_paths(&validated_args, &context.cwd);
            let mut invalid_path = false;
            for path in mutation_paths {
                // Canonicalization is a blocking filesystem syscall; await the
                // blocking-pool wrapper so preflight never stalls a tokio
                // worker. The key is identical to the sync API's.
                match ResourceKey::canonical_async(&path, &context.cwd).await {
                    Ok(key) => mutation_keys.push(key),
                    Err(_) => {
                        invalid_path = true;
                        break;
                    }
                }
            }
            if invalid_path {
                push_failure(
                    &mut results,
                    progress,
                    faces,
                    parent_call_id.as_deref(),
                    Some(&validated_args),
                    true,
                    error_result(&call, "Invalid mutation path"),
                )
                .await;
                continue;
            }
        }
        // The `background` flag rides BESIDE `PreparedToolCall` rather than on
        // it: the public struct is constructed exhaustively outside this
        // module (tests/suites/agent_tool_types.rs), so a new public field
        // would break those literals, and the flag must not leak into the
        // arguments the tool sees.
        let prepared_call = PreparedCall {
            call: PreparedToolCall {
                index: call.index,
                call_id: call.call_id.clone(),
                tool_name: call.tool_name.clone(),
                validated_args,
                mutation_paths: mutation_keys,
                execution_mode: tool.execution_mode(),
            },
            background,
        };
        // Before hook (blocking prevents execution). A panicking host hook is
        // isolated as a block (fail closed) instead of unwinding preflight.
        if let Some(before) = hooks.and_then(|h| h.before.as_ref()) {
            let check: BeforeToolCallResult =
                match std::panic::AssertUnwindSafe(before(&prepared_call.call, context))
                    .catch_unwind()
                    .await
                {
                    Ok(check) => check,
                    Err(payload) => BeforeToolCallResult {
                        block: true,
                        reason: Some(format!(
                            "beforeToolCall hook panicked: {}",
                            panic_payload_message(payload.as_ref())
                        )),
                        terminate: false,
                    },
                };
            if check.block {
                let mut result = ToolResult::error(
                    check
                        .reason
                        .clone()
                        .unwrap_or_else(|| format!("Tool '{}' was blocked", call.tool_name)),
                );
                result.terminate = check.terminate;
                push_failure(
                    &mut results,
                    progress,
                    faces,
                    parent_call_id.as_deref(),
                    Some(&prepared_call.call.validated_args),
                    true,
                    FinalizedToolCall {
                        index: call.index,
                        call_id: call.call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        duration_ms: None,
                        result,
                        usage: None,
                    },
                )
                .await;
                continue;
            }
        }
        prepared.push(prepared_call);
    }

    (prepared, results, start_events)
}

/// Record a preflight failure: the `tool_result` contract is dispatched FIRST
/// (for callers that observe it), then a progress observer receives the
/// failure's `end` — upstream emits the end right after the failed
/// preparation and only executed calls run `afterToolCall`. A gate-blocked
/// call passes `dispatch_result = false`: it never ran, so it gets no
/// `tool_result` notification.
#[allow(clippy::too_many_arguments)]
async fn push_failure(
    results: &mut Vec<FinalizedToolCall>,
    progress: Option<&ToolExecutionProgress>,
    faces: Option<&ToolEventFaces>,
    parent_call_id: Option<&str>,
    input: Option<&serde_json::Value>,
    dispatch_result: bool,
    mut finalized: FinalizedToolCall,
) {
    if dispatch_result {
        if let Some(faces) = faces {
            crate::agent::tool_execution::dispatch_tool_result_merge(
                faces,
                input,
                parent_call_id,
                &mut finalized,
            )
            .await;
        }
    }
    if let Some(progress) = progress {
        progress(end_event(&finalized, parent_call_id), Some(&finalized));
    }
    results.push(finalized);
}

/// Take the shared `background` argument out of a call's payload.
///
/// The key is injected into every declared tool schema
/// ([`crate::agent::tool_execution::declared_tool_definitions`]) but belongs to
/// the scheduler: only a JSON `true` requests background execution, and the key
/// is removed either way so tools with `additionalProperties: false` never
/// reject it and no tool ever sees it.
fn take_background_argument(args: &mut serde_json::Value) -> bool {
    args.as_object_mut()
        .and_then(|object| object.remove("background"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Validate arguments against the tool's declared JSON Schema.
///
/// This intentionally implements the schema vocabulary used by tools without
/// pulling in a schema crate: `type`, `required`, `properties`,
/// `additionalProperties`, `enum`, `items`, `minItems`/`maxItems`, and the
/// object/array constraints those keywords imply. The vocabulary is the
/// measured corpus of every `AgentTool::parameters()` declaration in this
/// tree (the only keyword outside the original set was `minItems`/`maxItems`
/// on `ctx_batch_execute`; see `validate_schema_value` for the library
/// trade-off). Validation is performed during preflight, before the
/// before-hook or any resource-path extraction can run the tool.
///
/// The payload is only read on the accepting path; a call whose raw payload
/// passes is left untouched and can be moved into the prepared call afterwards
/// instead of copied. When the raw payload fails, the upstream normalizations
/// below run on a CLONE (exactly like `structuredClone` +
/// `normalizeOptionalNulls` + `Value.Convert`, validation.ts:335-342); only a
/// payload that then validates replaces `args`, so a rejected call keeps the
/// bytes the model sent. The returned [`ArgumentValidation`] tells the caller
/// how to treat a `null` payload (`ArgumentValidation::NoSchema` normalizes it
/// to an empty object, the no-schema behavior).
fn validate_arguments(
    tool_name: &str,
    tool: &dyn AgentTool,
    args: &mut serde_json::Value,
) -> Result<ArgumentValidation, String> {
    let Some(schema) = tool.parameters() else {
        return Ok(ArgumentValidation::NoSchema);
    };

    // Fast path: the bytes the model sent are already legal. Nothing is
    // copied; the caller moves the payload straight into the prepared call.
    match validate_schema_value(args, &schema, "$") {
        Ok(()) => return Ok(ArgumentValidation::SchemaAccepted),
        Err(SchemaValidationError::Schema(message)) => {
            return Err(format!(
                "Tool '{tool_name}' has an invalid argument schema: {message}"
            ));
        }
        // Arguments were rejected — the normalization path below gets a turn.
        Err(SchemaValidationError::Arguments(_)) => {}
    }

    // The raw payload is illegal: it may be illegal only because upstream's
    // normalizations have not run yet. `validateToolArguments` runs
    // `normalizeOptionalNulls` and `Value.Convert` before it validates, on a
    // clone of the call's arguments (validation.ts:335-342) — so a numeric
    // `"5"` and an optional `null` are accepted without costing a turn. The
    // clone here is the same one upstream takes (`structuredClone`): a
    // rejected call must keep the exact bytes the model sent.
    let mut coerced = args.clone();
    normalize_optional_nulls(&mut coerced, &schema);
    coerce_schema_directed(&mut coerced, &schema);

    match validate_schema_value(&coerced, &schema, "$") {
        Ok(()) => {
            *args = coerced;
            Ok(ArgumentValidation::SchemaAccepted)
        }
        // Upstream `validateToolArguments`
        // (packages/ai/src/utils/validation.ts:344-352) lists EVERY
        // violation, one `  - path: message` line each, then the payload
        // that was RECEIVED (`toolCall.arguments`, the raw one). The
        // violations describe the NORMALIZED payload — exactly what was
        // validated — while the echo stays raw, so the model sees what it
        // actually sent. Reporting only the first mistake made it pay a turn
        // per error.
        Err(SchemaValidationError::Arguments(messages)) => {
            let violations = messages
                .iter()
                .map(|message| format!("  - {message}"))
                .collect::<Vec<_>>()
                .join("\n");
            let received =
                serde_json::to_string_pretty(&*args).unwrap_or_else(|_| args.to_string());
            Err(format!(
                "Validation failed for tool \"{tool_name}\":\n{violations}\n\nReceived arguments:\n{received}"
            ))
        }
        Err(SchemaValidationError::Schema(message)) => Err(format!(
            "Tool '{tool_name}' has an invalid argument schema: {message}"
        )),
    }
}

/// Upstream `normalizeOptionalNulls` (validation.ts:240-269), recursively: for
/// each declared property whose value is `null` and which the schema does NOT
/// require and whose own schema does NOT accept `null`, drop the key. A
/// required `null` is kept (and then rejected by the validator), and a
/// nullable declaration (`"type": ["string", "null"]`) keeps its `null` too.
///
/// `$ref` properties are skipped, like upstream: the walk cannot resolve a
/// reference and must not delete a value it cannot judge. A property whose
/// subschema is itself unusable is kept for the same reason (upstream's
/// `validator?.Check(null) === false` is false when the check throws).
///
/// JSON has no `undefined`, so the `null` arm is the whole rule on this side.
fn normalize_optional_nulls(value: &mut serde_json::Value, schema: &serde_json::Value) {
    normalize_optional_nulls_at(value, schema, 0);
}

fn normalize_optional_nulls_at(
    value: &mut serde_json::Value,
    schema: &serde_json::Value,
    depth: usize,
) {
    // Same ceiling as the validator's walk. Reaching it only stops the
    // normalization; validation still decides the call.
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(schema) = schema.as_object() else {
        return;
    };
    if let Some(array) = value.as_array_mut() {
        match schema.get("items") {
            Some(serde_json::Value::Array(items)) => {
                for (index, item) in array.iter_mut().enumerate() {
                    if let Some(item_schema) = items.get(index) {
                        normalize_optional_nulls_at(item, item_schema, depth + 1);
                    }
                }
            }
            Some(items @ serde_json::Value::Object(_)) => {
                for item in array.iter_mut() {
                    normalize_optional_nulls_at(item, items, depth + 1);
                }
            }
            _ => {}
        }
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect()
        })
        .unwrap_or_default();

    for (key, property_schema) in properties {
        let Some(property_value) = object.get(key.as_str()) else {
            continue;
        };
        let is_null = property_value.is_null();
        let referenced = property_schema
            .get("$ref")
            .is_some_and(serde_json::Value::is_string);
        if is_null
            && !required.contains(&key.as_str())
            && !referenced
            && property_accepts_null(property_schema) == Some(false)
        {
            object.remove(key.as_str());
        } else if let Some(property_value) = object.get_mut(key.as_str()) {
            normalize_optional_nulls_at(property_value, property_schema, depth + 1);
        }
    }
}

/// `Some(true)` when `schema` accepts `null`, `Some(false)` when it rejects it,
/// `None` when the declaration is unusable (the TS `schemaCheck` throw arm).
fn property_accepts_null(schema: &serde_json::Value) -> Option<bool> {
    match validate_schema_value(&serde_json::Value::Null, schema, "$") {
        Ok(()) => Some(true),
        Err(SchemaValidationError::Arguments(_)) => Some(false),
        Err(SchemaValidationError::Schema(_)) => None,
    }
}

/// The schema-directed half of upstream `coerceWithJsonSchema`
/// (validation.ts:194-238), narrowed to the rules this tree adopts: a numeric
/// string for a `"number"` or `"integer"` field becomes a JSON number.
///
/// Everything else upstream's converters accept is deliberately NOT adopted —
/// `null` → `0`/`false`/`""`, boolean ↔ number/string, number/boolean →
/// string, `[value]` → array wrapping, and `Value.Convert`'s
/// truncating integer arm — because each would rewrite a value whose type the
/// schema was supposed to reject (`{"path": null}` must stay an error; a
/// `"integer"` field must not silently take `5.5`). Numbers and booleans are
/// left alone here, and a string that is not a finite JS number is left
/// alone, so a genuinely wrong type still reaches the validator unchanged.
///
/// The walk mirrors upstream's ordering: `allOf` members first, then the
/// primitive/union coercion, then object/array recursion — each only where the
/// declared `type` says to descend. A union (`"type": [...]`) whose value
/// already matches a member is not coerced at all; otherwise the members are
/// tried in declaration order and the first coercion that fires wins.
fn coerce_schema_directed(value: &mut serde_json::Value, schema: &serde_json::Value) {
    coerce_schema_directed_at(value, schema, 0);
}

fn coerce_schema_directed_at(
    value: &mut serde_json::Value,
    schema: &serde_json::Value,
    depth: usize,
) {
    // Same ceiling as the validator's walk (and the `allOf` chain is the one
    // path a malformed external schema could otherwise recurse without bound:
    // the validator ignores `allOf`, so nothing else bounds it).
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(schema_object) = schema.as_object() else {
        return;
    };
    if let Some(all_of) = schema_object
        .get("allOf")
        .and_then(serde_json::Value::as_array)
    {
        for nested in all_of {
            coerce_schema_directed_at(value, nested, depth + 1);
        }
    }

    let types = declared_type_names(schema_object);
    let matches_union_member =
        types.len() > 1 && types.iter().any(|name| matches_declared_type(value, name));
    if !types.is_empty() && !matches_union_member {
        for name in &types {
            if let Some(coerced) = coerce_numeric_string(value, name) {
                *value = coerced;
                break;
            }
        }
    }

    if types.contains(&"object") {
        if let Some(object) = value.as_object_mut() {
            let properties = schema_object
                .get("properties")
                .and_then(serde_json::Value::as_object);
            if let Some(properties) = properties {
                for (key, property_schema) in properties {
                    if let Some(property_value) = object.get_mut(key.as_str()) {
                        coerce_schema_directed_at(property_value, property_schema, depth + 1);
                    }
                }
            }
            if let Some(additional) = schema_object
                .get("additionalProperties")
                .filter(|additional| additional.is_object())
            {
                for (key, property_value) in object.iter_mut() {
                    if properties.is_some_and(|properties| properties.contains_key(key.as_str())) {
                        continue;
                    }
                    coerce_schema_directed_at(property_value, additional, depth + 1);
                }
            }
        }
    }

    if types.contains(&"array") {
        if let Some(array) = value.as_array_mut() {
            match schema_object.get("items") {
                Some(serde_json::Value::Array(items)) => {
                    for (index, item) in array.iter_mut().enumerate() {
                        if let Some(item_schema) = items.get(index) {
                            coerce_schema_directed_at(item, item_schema, depth + 1);
                        }
                    }
                }
                Some(items @ serde_json::Value::Object(_)) => {
                    for item in array.iter_mut() {
                        coerce_schema_directed_at(item, items, depth + 1);
                    }
                }
                _ => {}
            }
        }
    }
}

/// `schema.type` as a list of names, ignoring non-string entries (upstream
/// `getSchemaTypes`).
fn declared_type_names(schema: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(serde_json::Value::String(name)) => vec![name.as_str()],
        Some(serde_json::Value::Array(names)) => {
            names.iter().filter_map(serde_json::Value::as_str).collect()
        }
        _ => Vec::new(),
    }
}

/// Upstream `matchesJsonType` (validation.ts:38-60). Unknown names match
/// nothing; they are the validator's schema-error arm, not this walk's.
fn matches_declared_type(value: &serde_json::Value, type_name: &str) -> bool {
    match type_name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(|number| {
            number.is_i64() || number.is_u64() || number.as_f64().is_some_and(|n| n.fract() == 0.0)
        }),
        "string" => value.is_string(),
        _ => false,
    }
}

/// One `coercePrimitiveByType` arm, restricted to the numeric-string coercions
/// this tree adopts (see [`coerce_schema_directed`]).
fn coerce_numeric_string(value: &serde_json::Value, type_name: &str) -> Option<serde_json::Value> {
    let serde_json::Value::String(text) = value else {
        return None;
    };
    if text.trim().is_empty() {
        return None;
    }
    let number = js_number_from_str(text)?;
    match type_name {
        // `"number"`: any finite `Number(value)`.
        "number" => Some(js_number_json(number)),
        // `"integer"`: only when `Number.isInteger(Number(value))` — no
        // truncation, so `"5.5"` stays a string and fails the schema.
        "integer" if number.fract() == 0.0 => Some(js_number_json(number)),
        _ => None,
    }
}

/// JS `Number(string)` for the finite cases (the `0x`/`0b`/`0o` radix
/// literals included). A non-finite parse (`"Infinity"`, `"NaN"`, overflow)
/// yields `None`, exactly `Number.isFinite` off.
fn js_number_from_str(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    for (prefix, radix) in [("0x", 16u32), ("0b", 2), ("0o", 8)] {
        let has_prefix = trimmed.len() >= 2
            && trimmed.is_char_boundary(2)
            && trimmed[..2].eq_ignore_ascii_case(prefix);
        if has_prefix {
            let digits = &trimmed[2..];
            if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
                return None;
            }
            return u64::from_str_radix(digits, radix)
                .ok()
                .map(|value| value as f64);
        }
    }
    let parsed = trimmed.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

/// A coerced number as JSON, keeping integral values integral so the payload
/// the tool sees serializes like `JSON.stringify(Number("20"))` (`20`, not
/// `20.0`).
fn js_number_json(number: f64) -> serde_json::Value {
    if number.fract() == 0.0 && number >= (i64::MIN as f64) && number <= (i64::MAX as f64) {
        return serde_json::Value::Number(serde_json::Number::from(number as i64));
    }
    match serde_json::Number::from_f64(number) {
        Some(number) => serde_json::Value::Number(number),
        // Non-finite values are filtered above; staying total beats panicking
        // on a preflight path.
        None => serde_json::Value::Null,
    }
}

/// How [`validate_arguments`] accepted a call's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgumentValidation {
    /// A declared schema accepted the payload as-is.
    SchemaAccepted,
    /// The tool declares no schema: a `null` payload normalizes to an empty
    /// object.
    NoSchema,
}

/// The `tool-repair` builtin's live-schema check (the `SchemaCheck` seam the
/// repair core in `plugins/tool_repair/repair.rs` takes as its injected
/// validator): the production preflight verdict of `arguments` against
/// `schema` — the exact `validate_schema_value` walk [`validate_arguments`]
/// runs during `prepare_calls` — mapped onto the three-verdict shape:
/// `Ok` → `Accepted`, a rejected-arguments error → `Rejected`, a
/// broken-schema error → `Unavailable` (the TS `Value.Check` throw arm).
/// Visibility-only exposure: no behavior, no re-validation, and no parallel
/// validator exists anywhere in the tree.
///
/// Compiled only with the `tool-repair` feature: the sole consumer is the
/// gated repair hook (`agent_session/tool_repair.rs`); the shared
/// `validate_schema_value` walk below stays unconditional (the preflight
/// itself uses it in every build).
#[cfg(feature = "tool-repair")]
pub(crate) fn tool_repair_schema_check(
    arguments: &serde_json::Value,
    schema: &serde_json::Value,
) -> crate::plugins::SchemaCheck {
    match validate_schema_value(arguments, schema, "$") {
        Ok(()) => crate::plugins::SchemaCheck::Accepted,
        Err(SchemaValidationError::Arguments(_)) => crate::plugins::SchemaCheck::Rejected,
        Err(SchemaValidationError::Schema(_)) => crate::plugins::SchemaCheck::Unavailable,
    }
}

#[derive(Debug)]
enum SchemaValidationError {
    /// One or more violations of the model's payload, in walk order. Every
    /// violation is collected (upstream ajv `allErrors`); a schema defect
    /// short-circuits as [`Self::Schema`] because the walk cannot trust the
    /// declaration any further.
    Arguments(Vec<String>),
    Schema(String),
}

/// Validate one value against the supported JSON Schema vocabulary.
///
/// Every argument violation is collected into one list (upstream ajv runs with
/// `allErrors`, validation.ts:344-352 prints them all); a defect in the
/// declared SCHEMA still short-circuits as [`SchemaValidationError::Schema`],
/// because the walk must not keep interpreting a declaration it cannot trust.
///
/// ## Why hand-rolled (the W11-L library decision)
///
/// `jsonschema` 0.47 (present in the local cargo cache) was evaluated and not
/// adopted. The measured reasons, in order of weight:
///
/// 1. **No compile-once seam.** [`AgentTool::parameters`] returns a fresh
///    `serde_json::Value` per call, so a compiled `jsonschema::Validator`
///    cannot be cached by tool identity without a schema hash plus a new
///    registry-level cache (outside this module). Compiling per call is
///    strictly more expensive than this O(payload) walk for the tree's small
///    schemas, and `tool_repair_schema_check` is handed an arbitrary schema
///    per call with no cache key at all.
/// 2. **Model-visible error contract.** Preflight errors are `{path}: expected
///    X, got Y` / `{path}: missing required property 'n'` (asserted, e.g.
///    `tests/suites/tool_repair.rs` expects `$.offset: expected number, got
///    null`). `jsonschema` reports `instance_path()` as a JSON pointer plus
///    kind-specific prose (`... is not of type "integer"`), so preserving the
///    contract needs a per-kind adapter and a pointer→`$.a[0]` rewriter.
/// 3. **Dependency cost.** `jsonschema` is not in `Cargo.lock`; adopting it
///    adds the crate plus eight new lock lines (`referencing`,
///    `jsonschema-regex`, `bytecount`, `email_address`, `fraction`,
///    `num-cmp`, `unicode-general-category`, `uuid-simd`) and, with default
///    features, reqwest/rustls/aws-lc-rs (a second TLS stack with a native
///    build).
/// 4. **Vocabulary gap is two keywords.** The corpus of `parameters()`
///    declarations in this tree uses `type`, `properties`, `required`,
///    `additionalProperties`, `enum`, `items`, and `minItems`/`maxItems`;
///    only the last pair was missing here, and it is ~30 lines.
///
/// Unknown keywords are deliberately ignored (JSON Schema annotation
/// semantics): third-party MCP/extension tools may carry `pattern`, `$ref`,
/// `oneOf`, ... which are not enforced here — the tool's own runtime checks
/// (or the MCP server) remain the backstop. Malformed values for a keyword
/// this walk *does* implement are a schema error (fail closed), never a
/// silent acceptance.
fn validate_schema_value(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
) -> Result<(), SchemaValidationError> {
    let mut violations = Vec::new();
    validate_schema_at(value, schema, path, 0, &mut violations)?;
    if violations.is_empty() {
        Ok(())
    } else {
        Err(SchemaValidationError::Arguments(violations))
    }
}

/// Deepest `properties`/`items`/`additionalProperties` chain the walk follows.
///
/// Tool schemas are small (the deepest in-tree declaration nests a handful of
/// levels), but extension and MCP tools carry externally supplied schemas, so
/// the walk is bounded: a pathological chain must fail closed as a schema
/// error instead of overflowing the thread stack. The model-supplied payload
/// is irrelevant here — recursion only descends where the schema descends —
/// and `serde_json` already caps parsed nesting at 128, so 128 admits every
/// schema an argument could match.
const MAX_SCHEMA_DEPTH: usize = 128;

/// Walk one value against one schema node, pushing every violation of the
/// model's payload onto `violations`. A defect in the declared schema returns
/// `Err(Schema(..))` immediately: the walk cannot trust the declaration past
/// that point, and a broken schema must stay a distinct (unrepairable) arm.
fn validate_schema_at(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    depth: usize,
    violations: &mut Vec<String>,
) -> Result<(), SchemaValidationError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(SchemaValidationError::Schema(format!(
            "schema nesting at {path} exceeds the maximum supported depth of {MAX_SCHEMA_DEPTH}"
        )));
    }
    let Some(schema) = schema.as_object() else {
        return Err(SchemaValidationError::Schema(format!(
            "schema at {path} must be an object"
        )));
    };

    // Upstream ajv runs with `allErrors`: the remaining keywords and sibling
    // properties are still checked after one violation, so the model sees
    // every mistake in one turn. Once the instance's TYPE is wrong, though,
    // the type-specific keywords below cannot say anything more about it
    // (ajv's `type` failure is likewise a single error per instance), so the
    // walk returns from this node after recording it.
    let mut type_mismatch = false;
    if let Some(enum_values) = schema.get("enum") {
        let Some(enum_values) = enum_values.as_array() else {
            return Err(SchemaValidationError::Schema(format!(
                "enum at {path} must be an array"
            )));
        };
        if enum_values.is_empty() {
            return Err(SchemaValidationError::Schema(format!(
                "enum at {path} must not be empty"
            )));
        }
        if !enum_values.iter().any(|candidate| candidate == value) {
            violations.push(format!("{path}: value is not one of the values in enum"));
        }
    }

    if let Some(type_value) = schema.get("type") {
        if !value_matches_type(value, type_value)? {
            violations.push(format!(
                "{path}: expected {}, got {}",
                expected_type_name(type_value),
                json_type_name(value)
            ));
            type_mismatch = true;
        }
    }

    // Object keywords are applicable to object instances. A declared object
    // type was checked above; without it, this follows JSON Schema's behavior
    // of ignoring object keywords for other instance types. A wrong declared
    // type makes the type-specific walk meaningless for this node.
    if !type_mismatch {
        if let Some(object) = value.as_object() {
            if let Some(required) = schema.get("required") {
                let Some(required) = required.as_array() else {
                    return Err(SchemaValidationError::Schema(format!(
                        "required at {path} must be an array"
                    )));
                };
                let mut required_names = Vec::with_capacity(required.len());
                for name in required {
                    let Some(name) = name.as_str() else {
                        return Err(SchemaValidationError::Schema(format!(
                            "required at {path} must contain only strings"
                        )));
                    };
                    required_names.push(name);
                }
                // Sorting makes malformed/invalid calls report the same
                // missing keys regardless of the schema's required-array order.
                required_names.sort_unstable();
                for name in required_names {
                    if !object.contains_key(name) {
                        violations.push(format!("{path}: missing required property '{name}'"));
                    }
                }
            }

            let properties = match schema.get("properties") {
                Some(properties) => {
                    let Some(properties) = properties.as_object() else {
                        return Err(SchemaValidationError::Schema(format!(
                            "properties at {path} must be an object"
                        )));
                    };
                    Some(properties)
                }
                None => None,
            };

            if let Some(properties) = properties {
                let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
                names.sort_unstable();
                for name in names {
                    if let Some(property_schema) = properties.get(name) {
                        validate_schema_at(
                            &object[name],
                            property_schema,
                            &property_path(path, name),
                            depth + 1,
                            violations,
                        )?;
                    }
                }

                if let Some(additional) = schema.get("additionalProperties") {
                    match additional {
                        serde_json::Value::Bool(false) => {
                            let mut additional_names: Vec<&str> = object
                                .keys()
                                .filter(|name| !properties.contains_key(*name))
                                .map(String::as_str)
                                .collect();
                            additional_names.sort_unstable();
                            for name in additional_names {
                                violations.push(format!(
                                    "{path}: additional property '{name}' is not allowed"
                                ));
                            }
                        }
                        serde_json::Value::Bool(true) => {}
                        serde_json::Value::Object(_) => {
                            let mut additional_names: Vec<&str> = object
                                .keys()
                                .filter(|name| !properties.contains_key(*name))
                                .map(String::as_str)
                                .collect();
                            additional_names.sort_unstable();
                            for name in additional_names {
                                validate_schema_at(
                                    &object[name],
                                    additional,
                                    &property_path(path, name),
                                    depth + 1,
                                    violations,
                                )?;
                            }
                        }
                        _ => {
                            return Err(SchemaValidationError::Schema(format!(
                                "additionalProperties at {path} must be a boolean or object"
                            )));
                        }
                    }
                }
            } else if let Some(additional) = schema.get("additionalProperties") {
                // With no properties declaration every key is additional.
                match additional {
                    serde_json::Value::Bool(false) => {
                        let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
                        names.sort_unstable();
                        for name in names {
                            violations.push(format!(
                                "{path}: additional property '{name}' is not allowed"
                            ));
                        }
                    }
                    serde_json::Value::Bool(true) => {}
                    serde_json::Value::Object(_) => {
                        let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
                        names.sort_unstable();
                        for name in names {
                            validate_schema_at(
                                &object[name],
                                additional,
                                &property_path(path, name),
                                depth + 1,
                                violations,
                            )?;
                        }
                    }
                    _ => {
                        return Err(SchemaValidationError::Schema(format!(
                            "additionalProperties at {path} must be a boolean or object"
                        )));
                    }
                }
            }
        }

        if let Some(array) = value.as_array() {
            // `minItems`/`maxItems` are the tree's only out-of-set keyword (the
            // `ctx_batch_execute` schema declares 1..16 scripts and <= 32
            // queries), so preflight must enforce them rather than let the call
            // reach a tool that only re-checks them at run time.
            if let Some(min_items) = schema.get("minItems") {
                let minimum = array_limit(min_items, "minItems", path)?;
                if array.len() < minimum {
                    violations.push(format!(
                        "{path}: expected at least {minimum} items, got {}",
                        array.len()
                    ));
                }
            }
            if let Some(max_items) = schema.get("maxItems") {
                let maximum = array_limit(max_items, "maxItems", path)?;
                if array.len() > maximum {
                    violations.push(format!(
                        "{path}: expected at most {maximum} items, got {}",
                        array.len()
                    ));
                }
            }
            if let Some(items) = schema.get("items") {
                if !items.is_object() {
                    return Err(SchemaValidationError::Schema(format!(
                        "items at {path} must be an object"
                    )));
                }
                for (index, item) in array.iter().enumerate() {
                    validate_schema_at(
                        item,
                        items,
                        &format!("{path}[{index}]"),
                        depth + 1,
                        violations,
                    )?;
                }
            }
        }
    }

    Ok(())
}

/// One `minItems`/`maxItems` value as a `usize`.
///
/// JSON Schema defines both as non-negative integers; anything else is a
/// defect in the declared schema (the schema-error arm), not in the model's
/// arguments.
fn array_limit(
    limit: &serde_json::Value,
    keyword: &str,
    path: &str,
) -> Result<usize, SchemaValidationError> {
    let Some(limit) = limit.as_u64() else {
        return Err(SchemaValidationError::Schema(format!(
            "{keyword} at {path} must be a non-negative integer"
        )));
    };
    usize::try_from(limit).map_err(|_| {
        SchemaValidationError::Schema(format!(
            "{keyword} at {path} is larger than this platform supports"
        ))
    })
}

fn value_matches_type(
    value: &serde_json::Value,
    type_value: &serde_json::Value,
) -> Result<bool, SchemaValidationError> {
    let names: Vec<&str> = match type_value {
        serde_json::Value::String(name) => vec![name.as_str()],
        serde_json::Value::Array(names) => names
            .iter()
            .map(|name| {
                name.as_str().ok_or_else(|| {
                    SchemaValidationError::Schema("type must contain only strings".to_string())
                })
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(SchemaValidationError::Schema(
                "type must be a string or array of strings".to_string(),
            ));
        }
    };
    if names.is_empty() {
        return Err(SchemaValidationError::Schema(
            "type array must not be empty".to_string(),
        ));
    }
    for name in &names {
        if !matches!(
            *name,
            "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
        ) {
            return Err(SchemaValidationError::Schema(format!(
                "unsupported type '{name}'"
            )));
        }
    }
    Ok(names.iter().any(|name| match *name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(|number| {
            number.is_i64() || number.is_u64() || number.as_f64().is_some_and(|n| n.fract() == 0.0)
        }),
        "string" => value.is_string(),
        _ => false,
    }))
}

fn expected_type_name(type_value: &serde_json::Value) -> String {
    match type_value {
        serde_json::Value::String(name) => name.clone(),
        serde_json::Value::Array(names) => names
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "a valid JSON type".to_string(),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn property_path(path: &str, name: &str) -> String {
    if name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        format!("{path}.{name}")
    } else {
        format!("{path}[{}]", serde_json::Value::String(name.to_string()))
    }
}

/// The model-facing text for a call that names a tool this batch cannot reach
/// (unknown, or registered but not declared): upstream
/// `createErrorToolResult(\`Tool ${toolCall.name} not found\`)`
/// (`agent-loop.ts:720`). Naming the tool is the actionable part — the model
/// can correct the name instead of guessing.
fn unknown_tool_message(tool_name: &str) -> String {
    format!("Tool {tool_name} not found")
}

/// Build an error `FinalizedToolCall` from an un-preflighted call.
fn error_result(call: &AgentToolCall, message: impl Into<String>) -> FinalizedToolCall {
    FinalizedToolCall {
        index: call.index,
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        duration_ms: None,
        result: ToolResult::error(message),
        usage: None,
    }
}
