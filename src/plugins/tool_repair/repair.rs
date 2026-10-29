//! The pure repair core: the `message_end` pipeline of pi-tool-repair
//! (`tool-repair.ts:109-173`) ported over pir's typed
//! [`AssistantMessage`](crate::ai::types::AssistantMessage).
//!
//! Pipeline order is the contract (the scope doc's §1 lists the same order):
//!
//! 1. **GLM token-leak strip** (`src/index.ts:170-231`), in place, gated by
//!    `hasGrammarLeakBug` (`/glm/i`): recursively strip leading `<arg_key>` /
//!    `<arg_value>` and trailing `</arg_key>` / `</arg_value>` boundaries from
//!    argument keys and string values (boundaries only, then `trim`).
//! 2. **Anchor-bleed value strip** (`src/index.ts:136-168`), in place, gated by
//!    `hasAnchorBleedBug`: recursively drop every leading `^` and trailing `$`
//!    from strings.
//! 3. **Grammar-leak repair** (`grammar-repair.ts`, Step 2): parse the ten
//!    leaked-grammar families out of text/thinking content, strip the leaked
//!    ranges, and — in `recover` mode, with no native tool call present, and
//!    with at least one call surviving the known-tool/empty-argument gates —
//!    append the recovered calls as `ToolCall` blocks and set
//!    `stop_reason = toolUse`. Runs when the resolved
//!    [`GrammarRepairConfig`] is enabled (`enabled: true` or a `leakModels`
//!    match, [`grammar::resolve_grammar_repair_for_model`]); it is the one pass
//!    that also runs on messages with **no** tool-call blocks, because a
//!    leaked-grammar message is exactly the one with none. See
//!    [`grammar`] for the safety gates and the family coverage.
//! 4. **Live-schema repair** (`src/index.ts:869-943`, core
//!    `repairObjectFromSchema` `:709-867`): per `ToolCall`, validate
//!    `arguments` against the tool's live schema; valid input is left alone,
//!    invalid input goes `wrapRootStringAsObject` (`:381-398`) →
//!    `repairObjectFromSchema` and is committed only when the candidate
//!    re-validates. An unrepairable candidate is discarded so the host reports
//!    the original error — repairs never swallow the error (`:889-915`).
//!    Recovered calls from step 3 go through this pass too, exactly like the
//!    TS hook re-runs `repairAssistantToolCallInputs` on the replaced message.
//! 5. **Phantom toolUse normalization** (`src/index.ts:400-434`): `toolUse`
//!    with zero `toolCall` blocks becomes a retryable error.
//!
//! Both in-place passes (1, 2) run before the schema pass and commit
//! unconditionally (they are not re-validated), exactly like the TS hook.
//!
//! Step 3 adds the other half of the TS extension to this module: the
//! **Phase 0** `before_provider_request` schema-anchor sanitization
//! (`tool-repair.ts:73-105`, `src/index.ts:111-134`) — a pure payload pass
//! ([`sanitize_request_payload`]) that [`super::ToolRepairPlugin`] dispatches on
//! the unified plugin event path. It is model-gated like the message passes; a
//! non-gated (or unknown) model leaves the request payload byte-identical.
//!
//! ## The two entry points
//!
//! [`repair_message_with_grammar`] is the full pipeline above.
//! [`repair_message`] is the Step 1a-compatible wrapper: it runs the same
//! pipeline with the default (disabled) grammar config and an empty known-tool
//! set, which is observably identical to the Step 1a behavior it must keep for
//! the already-landed suite. Step 1b's loop hook and any caller that owns the
//! active-tool registry and the package config call
//! [`repair_message_with_grammar`].
//!
//! ## Schema validation seam
//!
//! TS validates with TypeBox's `Value.Check(schema, input)` inside a
//! `try/catch` (`src/index.ts:674-680`). pir's equivalent is the preflight
//! validator `validate_schema_value` in `src/agent/tool_scheduler/prepare.rs`
//! — the exact function `validate_arguments` (`prepare.rs:197`, called from
//! `prepare_calls` at `:78`; the walker at `:234`) runs — but that module,
//! function, and its `SchemaValidationError` are **module-private**, and this
//! lane may not edit `prepare.rs`. The core therefore takes validation as an
//! injected
//! [`SchemaCheck`] callback (`check: &dyn Fn(&JsonValue, &JsonValue) ->
//! SchemaCheck`) instead of reimplementing a validator:
//!
//! - Step 1b must expose the real validator (a `pub(crate)` visibility bump or
//!   a thin public wrapper in `prepare.rs`) and adapt it here; the adapter is
//!   `Ok(()) => Accepted`, `Err(Arguments) => Rejected`, `Err(Schema) =>
//!   Unavailable` (the TS `catch` arm).
//! - Until then tests drive the callback through the public scheduler API
//!   (`ToolScheduler::execute_batch`), which runs the very same
//!   `validate_schema_value` during preflight — no parallel validator exists
//!   anywhere in the tree.
//!
//! ## Deviations (Step 1a, recording only)
//!
//! - **Bare-string roots cannot reach the message pipeline**: pir's
//!   `ToolCall.arguments` is a `BTreeMap<String, JsonValue>` and the provider
//!   adapters parse a non-object `arguments` JSON into an empty map
//!   (`src/ai/api/anthropic.rs:1981-2003`; the other adapters do the same), so
//!   the TS `"/path/to/file"` root arrives as `{}`. `wrapRootStringAsObject`
//!   is retained for parity and is exercised through
//!   [`repair_tool_call_arguments`]'s untyped root; it currently cannot fire
//!   from [`repair_message`].
//! - **Real core-tool schemas**: the JS path received `parameters: null` for
//!   core tools (`src/extensions/runtime/host_functions/session_fns.rs:337-380`),
//!   so its step 3 was a no-op there. The Rust builtin is fed the real
//!   `AgentTool::parameters()` (e.g. `read`, `grep`), so the repair actually
//!   engages — the net gain recorded in the scope doc §0.5.
//! - **Numeric-string coercion stays TS-faithful (`type === "number"` only)**.
//!   `read` now declares `offset`/`limit` as `"number"`
//!   (`src/coding_agent/tools/read.rs:161-162`, matching upstream
//!   `core/tools/read.ts:16-17`), so the rule reaches them exactly as it
//!   reaches `grep`/`find`/`ls`/`bash`. The PREFLIGHT additionally applies
//!   upstream `Value.Convert`'s `"integer"` arm
//!   (`src/agent/tool_scheduler/prepare.rs`), which this hook deliberately
//!   does not: the injected [`SchemaCheck`] is the strict walker, so a repair
//!   candidate must be legal without preflight's coercion — the hook proposes
//!   the same edits the TS package does and lets the host validate.
//!   JS `Number()` itself is followed exactly, including `""` → `0`,
//!   whitespace, the `0x`/`0b`/`0o` literals and the `[]` → `0` / `[x]` →
//!   `Number(x)` coercions the `bash.timeoutMs` guard can observe.
//! - **Key order after a repair is sorted, not model order**: pir's
//!   `ToolCall.arguments` is a `BTreeMap`, so a committed repair rewrites the
//!   call's serialization order (scope doc §6: transcript byte diffs). Only
//!   repaired calls are rewritten; untouched calls keep their prior bytes.
//! - **Grammar recovery is a separate opt-in pass with its own entry point**:
//!   the TS `message_end` hook gates it on `resolveGrammarRepairForModel`
//!   (`tool-repair.ts:121-145`); here that gate lives in [`grammar`] and is
//!   applied by [`repair_message_with_grammar`]. The GLM token strip's `/glm/i`
//!   gate is a different gate and the two never merge (scope doc §6).
//! - Wired into the agent loop in Step 1b; the loop hook calls
//!   [`repair_message_with_grammar`] (this module is a pure library for now).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as JsonValue;

use crate::ai::types::{AssistantMessage, ContentBlock, StopReason};

use super::aliases::{
    field_aliases, has_anchor_bleed_bug, has_grammar_leak_bug, legacy_new_text_names,
    legacy_old_text_names, string_arg_tool, WrapShape,
};
use super::config::GrammarRepairConfig;
use super::grammar;

/// The exact phantom-toolUse error text (`src/index.ts:431`).
pub const PHANTOM_TOOL_USE_ERROR: &str =
    "stream ended before tool_calls were received (vLLM phantom tool_use)";

/// Rule names, exactly the TS `ruleName` strings (README rule table).
pub mod rules {
    /// TS `wrapRootStringAsObject` (`src/index.ts:381-398`).
    pub const WRAP_ROOT_STRING_AS_OBJECT: &str = "wrapRootStringAsObject";
    /// TS `renameAliasedField` (live-schema form, `src/index.ts:721-733`).
    pub const RENAME_ALIASED_FIELD: &str = "renameAliasedField";
    /// TS `wrapLegacyEditFields` (`src/index.ts:735-751`).
    pub const WRAP_LEGACY_EDIT_FIELDS: &str = "wrapLegacyEditFields";
    /// TS `convertTimeoutMilliseconds` (`src/index.ts:753-770`).
    pub const CONVERT_TIMEOUT_MILLISECONDS: &str = "convertTimeoutMilliseconds";
    /// TS `joinStringArray` (`src/index.ts:772-786`).
    pub const JOIN_STRING_ARRAY: &str = "joinStringArray";
    /// TS `dropNullOrUndefined` (`src/index.ts:793-802`).
    pub const DROP_NULL_OR_UNDEFINED: &str = "dropNullOrUndefined";
    /// TS `coerceNumericString` (`src/index.ts:804-818`).
    pub const COERCE_NUMERIC_STRING: &str = "coerceNumericString";
    /// TS `dropEmptyObjectPlaceholder` (`src/index.ts:820-830`).
    pub const DROP_EMPTY_OBJECT_PLACEHOLDER: &str = "dropEmptyObjectPlaceholder";
    /// TS `parseJsonStringifiedArray` (`src/index.ts:831-842`).
    pub const PARSE_JSON_STRINGIFIED_ARRAY: &str = "parseJsonStringifiedArray";
    /// TS `wrapBareStringAsArray` (`src/index.ts:831-842`).
    pub const WRAP_BARE_STRING_AS_ARRAY: &str = "wrapBareStringAsArray";
}

// ---------------------------------------------------------------------------
// Schema validation seam
// ---------------------------------------------------------------------------

/// One verbatim live-schema verdict (TS `schemaCheck`, `src/index.ts:674-680`:
/// `Value.Check` → `true`/`false`, a throw → `undefined`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaCheck {
    /// The schema accepts the value.
    Accepted,
    /// The schema rejects the value.
    Rejected,
    /// No verdict: the schema itself is unusable (the TS `catch` arm). The
    /// call passes through untouched, like TS `status: "unchanged"`.
    Unavailable,
}

/// The live schema of one active tool (TS `LiveToolSchema`,
/// `src/index.ts:635-638`).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveToolSchema {
    /// The tool name.
    pub name: String,
    /// The tool's `AgentTool::parameters()` JSON schema.
    pub parameters: JsonValue,
}

impl LiveToolSchema {
    /// Build one entry of the active-tool schema list.
    pub fn new(name: impl Into<String>, parameters: JsonValue) -> Self {
        Self {
            name: name.into(),
            parameters,
        }
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// TS `LiveRepairStatus` (`src/index.ts:640`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRepairStatus {
    /// The raw input already validates (or no verdict was available).
    Unchanged,
    /// Repairs fired and the candidate re-validates.
    Recovered,
    /// Repairs fired but the candidate does not validate; the raw input is
    /// kept, so the host still surfaces the original error.
    Unrepairable,
}

/// TS `LiveToolRepairOutcome` (`src/index.ts:642-649`) for one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveToolRepair {
    /// Tool name as it appeared in the call.
    pub tool_name: String,
    /// The verdict.
    pub status: LiveRepairStatus,
    /// The raw input, exactly as the model sent it (never mutated).
    pub input: JsonValue,
    /// The repaired candidate when any rule fired (present for
    /// `Unrepairable` too, mirroring the TS `repaired` field).
    pub repaired: Option<JsonValue>,
    /// Fired rule names, in first-fire order, deduplicated.
    pub rules_fired: Vec<&'static str>,
    /// One hint per fired rule (duplicates preserved, like TS `hints`).
    pub hints: Vec<String>,
}

/// One non-`Unchanged` tool-call outcome, kept on the message report
/// (TS `repairAssistantToolCallInputs`'s `repairs` array, `src/index.ts:918-943`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRepair {
    /// Provider tool call id.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The verdict.
    pub status: LiveRepairStatus,
    /// Fired rule names.
    pub rules_fired: Vec<&'static str>,
    /// Hints.
    pub hints: Vec<String>,
}

/// What the pipeline did to one assistant message.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepairReport {
    /// One entry per tool call that was not `Unchanged`, in content order.
    pub calls: Vec<ToolCallRepair>,
    /// Whether the phantom-toolUse normalization fired.
    pub phantom_normalized: bool,
}

/// Message-level outcome of [`repair_message`].
///
/// [`Self::Unrepairable`] is the "no change was committed, but repairs were
/// attempted and rejected" case: the message keeps its original arguments and
/// the provider error stays visible for the host to report.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageRepairOutcome {
    /// Nothing fired: the message was not touched.
    Unchanged,
    /// At least one pass committed a change.
    Repaired(RepairReport),
    /// Repairs were attempted, every candidate was rejected on re-validation,
    /// and nothing else changed.
    Unrepairable(RepairReport),
}

impl MessageRepairOutcome {
    /// Whether the pipeline modified the message.
    pub fn changed(&self) -> bool {
        matches!(self, Self::Repaired(_))
    }

    /// The per-call report, for both described outcomes.
    pub fn report(&self) -> Option<&RepairReport> {
        match self {
            Self::Unchanged => None,
            Self::Repaired(report) | Self::Unrepairable(report) => Some(report),
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 1 + 2: in-place strip passes
// ---------------------------------------------------------------------------

/// GLM grammar tokens that leak into keys and string values
/// (TS `GRAMMAR_TOKEN_LEAKS`, `src/index.ts:173-178`).
const GRAMMAR_TOKEN_LEAKS: &[(&str, Boundary)] = &[
    ("<arg_key>", Boundary::Start),
    ("</arg_key>", Boundary::End),
    ("<arg_value>", Boundary::Start),
    ("</arg_value>", Boundary::End),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Start,
    End,
}

/// TS `stripBoundaryGrammarTokens` (`src/index.ts:180-193`): strip the start
/// tags at the front and the end tags at the back, then trim. `None` when
/// nothing changed.
fn strip_boundary_grammar_tokens(value: &str) -> Option<String> {
    let mut stripped = value;
    let mut changed = false;
    for &(tag, at) in GRAMMAR_TOKEN_LEAKS {
        if at == Boundary::Start && stripped.starts_with(tag) {
            stripped = &stripped[tag.len()..];
            changed = true;
        } else if at == Boundary::End && stripped.ends_with(tag) {
            stripped = &stripped[..stripped.len() - tag.len()];
            changed = true;
        }
    }
    if changed {
        Some(stripped.trim().to_owned())
    } else {
        None
    }
}

/// Pass 1 (TS `stripGrammarTokenLeaksInPlace`, `src/index.ts:195-231`):
/// boundary grammar tokens stripped from argument keys and recursively from
/// string values (objects and arrays included).
///
/// The map is rebuilt rather than mutated in place: a renamed key can collide
/// with an existing one, and the map's own key order makes that collision
/// resolution deterministic (last insert wins) where the JS object keeps
/// insertion order. Only reachable for pathological inputs.
pub fn strip_grammar_token_leaks(arguments: &mut BTreeMap<String, JsonValue>) -> bool {
    strip_grammar_token_leaks_in_map(arguments)
}

/// The shared rebuild, generic over the ordered map type: `ToolCall.arguments`
/// is a `BTreeMap`, while objects nested inside a `JsonValue` are
/// `serde_json::Map`s.
fn strip_grammar_token_leaks_in_map<M>(map: &mut M) -> bool
where
    M: Default + Extend<(String, JsonValue)> + IntoIterator<Item = (String, JsonValue)>,
{
    let mut changed = false;
    let mut rebuilt = M::default();
    for (key, mut value) in std::mem::take(map) {
        let new_key = match strip_boundary_grammar_tokens(&key) {
            Some(stripped) => {
                changed = true;
                stripped
            }
            None => key,
        };
        if strip_grammar_tokens_value(&mut value) {
            changed = true;
        }
        rebuilt.extend(std::iter::once((new_key, value)));
    }
    *map = rebuilt;
    changed
}

fn strip_grammar_tokens_value(value: &mut JsonValue) -> bool {
    match value {
        JsonValue::String(text) => match strip_boundary_grammar_tokens(text) {
            Some(stripped) => {
                *text = stripped;
                true
            }
            None => false,
        },
        JsonValue::Array(items) => {
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= strip_grammar_tokens_value(item);
            }
            changed
        }
        JsonValue::Object(map) => strip_grammar_token_leaks_in_map(map),
        _ => false,
    }
}

/// Pass 2 (TS `stripAnchorBleedInPlace`, `src/index.ts:136-168`): strip every
/// leading `^` and trailing `$` from strings, recursively (string values in
/// objects and array items; keys are untouched).
pub fn strip_anchor_bleed(arguments: &mut BTreeMap<String, JsonValue>) -> bool {
    let mut changed = false;
    for value in arguments.values_mut() {
        changed |= strip_anchor_bleed_value(value);
    }
    changed
}

fn strip_anchor_bleed_in_map(map: &mut serde_json::Map<String, JsonValue>) -> bool {
    let mut changed = false;
    for value in map.values_mut() {
        changed |= strip_anchor_bleed_value(value);
    }
    changed
}

fn strip_anchor_bleed_value(value: &mut JsonValue) -> bool {
    match value {
        JsonValue::String(text) => {
            let stripped = text.trim_start_matches('^').trim_end_matches('$');
            if stripped == text.as_str() {
                false
            } else {
                *text = stripped.to_owned();
                true
            }
        }
        JsonValue::Array(items) => {
            let mut changed = false;
            for item in items.iter_mut() {
                changed |= strip_anchor_bleed_value(item);
            }
            changed
        }
        JsonValue::Object(map) => strip_anchor_bleed_in_map(map),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Pass 3: root wrap
// ---------------------------------------------------------------------------

/// A wrapped bare-string root (TS `wrapRootStringAsObject`, `src/index.ts:381-398`).
#[derive(Debug, Clone, PartialEq)]
pub struct WrappedRootString {
    /// The wrapped object.
    pub wrapped: JsonValue,
    /// The user-facing hint.
    pub hint: String,
}

/// TS `wrapRootStringAsObject`: interpret a bare string root as the tool's
/// primary string field (`stringArgTools`), for `read`/`grep`/`find`/`bash`/
/// `ls`/`fabric_exec` only. `None` for non-strings and unknown tools.
pub fn wrap_root_string_as_object(input: &JsonValue, tool_name: &str) -> Option<WrappedRootString> {
    let text = input.as_str()?;
    let mapping = string_arg_tool(tool_name)?;
    let field = match mapping.shape {
        WrapShape::Str => JsonValue::String(text.to_owned()),
        WrapShape::Array => JsonValue::Array(vec![JsonValue::String(text.to_owned())]),
    };
    let mut wrapped = serde_json::Map::new();
    wrapped.insert(mapping.field.to_owned(), field);
    Some(WrappedRootString {
        wrapped: JsonValue::Object(wrapped),
        hint: format!(
            "Interpreted your bare string as the `{}` argument for tool \"{tool_name}\". \
             Call this tool with an object, not a bare string, next time.",
            mapping.field
        ),
    })
}

// ---------------------------------------------------------------------------
// Pass 3: schema-directed repair
// ---------------------------------------------------------------------------

/// TS `repairInputAgainstLiveSchema` (`src/index.ts:869-916`): validate the raw
/// arguments, repair only when needed, and commit only a candidate that
/// re-validates. `arguments` is never mutated.
pub fn repair_tool_call_arguments(
    tool_name: &str,
    arguments: &JsonValue,
    schema: &JsonValue,
    check: &dyn Fn(&JsonValue, &JsonValue) -> SchemaCheck,
) -> LiveToolRepair {
    let initial = check(arguments, schema);
    if initial == SchemaCheck::Unavailable {
        return LiveToolRepair {
            tool_name: tool_name.to_owned(),
            status: LiveRepairStatus::Unchanged,
            input: arguments.clone(),
            repaired: None,
            rules_fired: Vec::new(),
            hints: Vec::new(),
        };
    }
    let accepted_initially = initial == SchemaCheck::Accepted;

    let mut rules_fired: Vec<&'static str> = Vec::new();
    let mut hints: Vec<String> = Vec::new();
    let mut candidate = if accepted_initially {
        arguments.clone()
    } else if let Some(wrapped) = wrap_root_string_as_object(arguments, tool_name) {
        rules_fired.push(rules::WRAP_ROOT_STRING_AS_OBJECT);
        hints.push(wrapped.hint);
        wrapped.wrapped
    } else {
        arguments.clone()
    };

    // The object repair pass also runs on already-valid input: an accepted
    // value can still carry a droppable `null` or a legacy alias pair (the TS
    // `if (isObject(candidate) && objectSchema)` has no `initial` guard).
    if candidate.is_object() && schema.is_object() {
        repair_object_from_schema(
            tool_name,
            schema,
            &mut candidate,
            &mut rules_fired,
            &mut hints,
            &[],
        );
    }

    if rules_fired.is_empty() {
        return LiveToolRepair {
            tool_name: tool_name.to_owned(),
            status: if accepted_initially {
                LiveRepairStatus::Unchanged
            } else {
                LiveRepairStatus::Unrepairable
            },
            input: arguments.clone(),
            repaired: None,
            rules_fired,
            hints,
        };
    }

    let recovered = check(&candidate, schema) == SchemaCheck::Accepted;
    LiveToolRepair {
        tool_name: tool_name.to_owned(),
        status: if recovered {
            LiveRepairStatus::Recovered
        } else {
            LiveRepairStatus::Unrepairable
        },
        input: arguments.clone(),
        repaired: Some(candidate),
        rules_fired,
        hints,
    }
}

/// TS `repairObjectFromSchema` (`src/index.ts:709-867`). `input` must be an
/// object; `path` is the JSON path prefix used in hints and in the root-only
/// special cases.
fn repair_object_from_schema(
    tool_name: &str,
    schema: &JsonValue,
    input: &mut JsonValue,
    rules_fired: &mut Vec<&'static str>,
    hints: &mut Vec<String>,
    path: &[String],
) {
    let properties = schema.get("properties").and_then(JsonValue::as_object);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(JsonValue::as_array)
        .map(|names| names.iter().filter_map(JsonValue::as_str).collect())
        .unwrap_or_default();

    // ①a. Alias renames. Canonical wins: a canonical that is already present
    // (or a canonical the schema does not declare) is skipped.
    if let (Some(properties), Some(input_object)) = (properties, input.as_object_mut()) {
        for alias in field_aliases(tool_name) {
            // TS `schemaProperties` keeps only object-valued entries, and the
            // canonical has to be declared for the rename to be schema-directed.
            if !properties
                .get(alias.canonical)
                .is_some_and(JsonValue::is_object)
                || input_object.contains_key(alias.canonical)
            {
                continue;
            }
            // TS filters the canonical out of its own alias list
            // (`aliasNames.filter((name) => name !== canonical)`).
            let names: Vec<&str> = alias
                .aliases
                .iter()
                .copied()
                .filter(|name| *name != alias.canonical)
                .collect();
            let Some((alias_key, value)) = first_usable_field(input_object, &names) else {
                continue;
            };
            input_object.insert(alias.canonical.to_owned(), value);
            input_object.remove(&alias_key);
            add_rule(
                rules_fired,
                hints,
                rules::RENAME_ALIASED_FIELD,
                format!(
                    "Renamed `{}` to `{}` for tool \"{tool_name}\".",
                    join_path(path, &alias_key),
                    join_path(path, alias.canonical)
                ),
            );
        }
    }

    // ①b. `edit`: a legacy top-level oldText/newText pair becomes `edits[]`.
    let edits_declared = properties.is_some_and(|properties| properties.contains_key("edits"));
    let edits_present = input
        .as_object()
        .is_some_and(|input| input.contains_key("edits"));
    if tool_name == "edit" && path.is_empty() && edits_declared && !edits_present {
        let old_names = legacy_old_text_names();
        let new_names = legacy_new_text_names();
        if let Some(input_object) = input.as_object_mut() {
            let old_field = first_usable_field(input_object, &old_names);
            let new_field = first_usable_field(input_object, &new_names);
            if let (Some((_, old_value)), Some((_, new_value))) = (&old_field, &new_field) {
                if let (Some(old_text), Some(new_text)) = (old_value.as_str(), new_value.as_str()) {
                    for name in old_names.iter().chain(new_names.iter()) {
                        input_object.remove(*name);
                    }
                    let mut edit = serde_json::Map::new();
                    edit.insert("oldText".to_owned(), JsonValue::String(old_text.to_owned()));
                    edit.insert("newText".to_owned(), JsonValue::String(new_text.to_owned()));
                    input_object.insert(
                        "edits".to_owned(),
                        JsonValue::Array(vec![JsonValue::Object(edit)]),
                    );
                    add_rule(
                        rules_fired,
                        hints,
                        rules::WRAP_LEGACY_EDIT_FIELDS,
                        "Moved legacy edit text fields into the current `edits` array.".to_owned(),
                    );
                }
            }
        }
    }

    // ①c. `bash`: `timeoutMs` milliseconds become `timeout` seconds.
    if tool_name == "bash" && path.is_empty() {
        if let Some(input_object) = input.as_object_mut() {
            let timeout_ms = input_object.get("timeoutMs").cloned();
            if !input_object.contains_key("timeout") {
                if let Some(timeout_ms) = timeout_ms {
                    if let Some(milliseconds) = js_number_coercion(&timeout_ms) {
                        input_object
                            .insert("timeout".to_owned(), number_json(milliseconds / 1000.0));
                        input_object.remove("timeoutMs");
                        add_rule(
                            rules_fired,
                            hints,
                            rules::CONVERT_TIMEOUT_MILLISECONDS,
                            "Converted `bash.timeoutMs` from milliseconds to `timeout` seconds."
                                .to_owned(),
                        );
                    }
                }
            }
        }
    }

    // ①d. `fabric_exec`: an all-string `code` array joins with newlines.
    let code_is_string = properties
        .and_then(|properties| properties.get("code"))
        .and_then(|code| code.get("type"))
        .and_then(JsonValue::as_str)
        == Some("string");
    if tool_name == "fabric_exec" && path.is_empty() && code_is_string {
        if let Some(input_object) = input.as_object_mut() {
            let joined = input_object
                .get("code")
                .and_then(JsonValue::as_array)
                .and_then(|lines| {
                    let mut joined = String::new();
                    for line in lines {
                        let text = line.as_str()?;
                        if !joined.is_empty() {
                            joined.push('\n');
                        }
                        joined.push_str(text);
                    }
                    Some(joined)
                });
            if let Some(joined) = joined {
                input_object.insert("code".to_owned(), JsonValue::String(joined));
                add_rule(
                    rules_fired,
                    hints,
                    rules::JOIN_STRING_ARRAY,
                    "Joined the `fabric_exec.code` string array with newlines.".to_owned(),
                );
            }
        }
    }

    // ②. Per declared property, in schema order.
    let Some(properties) = properties else {
        return;
    };
    let Some(input_object) = input.as_object_mut() else {
        return;
    };
    for (key, property_schema) in properties {
        // TS `schemaProperties` drops non-object entries (a boolean schema is
        // legal JSON Schema but never repaired here).
        let Some(declared) = property_schema.as_object() else {
            continue;
        };
        if !input_object.contains_key(key) {
            continue;
        }
        let Some(value) = input_object.get(key).cloned() else {
            continue;
        };
        let type_name = declared.get("type").and_then(JsonValue::as_str);

        if value.is_null() && !required.contains(&key.as_str()) {
            input_object.remove(key);
            add_rule(
                rules_fired,
                hints,
                rules::DROP_NULL_OR_UNDEFINED,
                format!(
                    "Dropped optional null `{}` from tool \"{tool_name}\".",
                    join_path(path, key)
                ),
            );
            continue;
        }

        if type_name == Some("number") {
            if let Some(text) = value.as_str() {
                if !text.trim().is_empty() {
                    if let Some(number) = js_number_from_str(text) {
                        input_object.insert(key.clone(), number_json(number));
                        add_rule(
                            rules_fired,
                            hints,
                            rules::COERCE_NUMERIC_STRING,
                            format!(
                                "Converted numeric string in {} for tool \"{tool_name}\".",
                                join_path(path, key)
                            ),
                        );
                        continue;
                    }
                }
            }
        }

        if type_name == Some("array") {
            if value.is_object() && value.as_object().is_some_and(serde_json::Map::is_empty) {
                input_object.remove(key);
                add_rule(
                    rules_fired,
                    hints,
                    rules::DROP_EMPTY_OBJECT_PLACEHOLDER,
                    format!(
                        "Dropped empty object placeholder from array field `{}` for tool \"{tool_name}\".",
                        join_path(path, key)
                    ),
                );
                continue;
            }
            if let Some(text) = value.as_str() {
                match try_parse_json_array(text) {
                    Some(parsed) => {
                        input_object.insert(key.clone(), parsed);
                        add_rule(
                            rules_fired,
                            hints,
                            rules::PARSE_JSON_STRINGIFIED_ARRAY,
                            format!(
                                "Parsed JSON-stringified array in `{}` for tool \"{tool_name}\".",
                                join_path(path, key)
                            ),
                        );
                    }
                    None => {
                        input_object.insert(
                            key.clone(),
                            JsonValue::Array(vec![JsonValue::String(text.to_owned())]),
                        );
                        add_rule(
                            rules_fired,
                            hints,
                            rules::WRAP_BARE_STRING_AS_ARRAY,
                            format!(
                                "Wrapped bare string in an array for `{}` in tool \"{tool_name}\".",
                                join_path(path, key)
                            ),
                        );
                    }
                }
            }
            // The TS string branch has no `continue`: freshly parsed/wrapped
            // items are still recursed into.
            if let Some(item_schema) = declared.get("items").filter(|items| items.is_object()) {
                if let Some(array) = input_object.get_mut(key).and_then(JsonValue::as_array_mut) {
                    for (index, item) in array.iter_mut().enumerate() {
                        if item.is_object() {
                            let mut child_path = path.to_vec();
                            child_path.push(key.clone());
                            child_path.push(index.to_string());
                            repair_object_from_schema(
                                tool_name,
                                item_schema,
                                item,
                                rules_fired,
                                hints,
                                &child_path,
                            );
                        }
                    }
                }
            }
            continue;
        }

        if type_name == Some("object") && value.is_object() {
            if let Some(child) = input_object.get_mut(key) {
                let mut child_path = path.to_vec();
                child_path.push(key.clone());
                repair_object_from_schema(
                    tool_name,
                    property_schema,
                    child,
                    rules_fired,
                    hints,
                    &child_path,
                );
            }
        }
    }
}

/// TS `firstUsableField` (`src/index.ts:692-703`): the first name the input
/// owns whose value is neither null nor the empty string.
fn first_usable_field(
    input: &serde_json::Map<String, JsonValue>,
    names: &[&str],
) -> Option<(String, JsonValue)> {
    for name in names {
        let Some(value) = input.get(*name) else {
            continue;
        };
        if value.is_null() || value.as_str() == Some("") {
            continue;
        }
        return Some(((*name).to_owned(), value.clone()));
    }
    None
}

/// TS `addRule` (`src/index.ts:682-690`): dedupe the rule name, always push
/// the hint.
fn add_rule(
    rules_fired: &mut Vec<&'static str>,
    hints: &mut Vec<String>,
    rule: &'static str,
    hint: String,
) {
    if !rules_fired.contains(&rule) {
        rules_fired.push(rule);
    }
    hints.push(hint);
}

/// TS `[...path, key].join(".")`.
fn join_path(path: &[String], key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{}.{}", path.join("."), key)
    }
}

/// TS `tryParseJsonArray` (`src/index.ts:334-341`): the parsed value must be
/// an array.
fn try_parse_json_array(text: &str) -> Option<JsonValue> {
    let parsed: JsonValue = serde_json::from_str(text).ok()?;
    parsed.is_array().then_some(parsed)
}

/// JS `Number(string)` for the finite cases: the `""` → `0` and whitespace
/// coercions, plus the `0x`/`0b`/`0o` radix literals
/// (`Number.isFinite(Number(value))`).
fn js_number_from_str(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16u32), ("0b", 2), ("0o", 8)] {
        let has_prefix = trimmed.len() >= 2
            && trimmed.is_char_boundary(2)
            && trimmed[..2].eq_ignore_ascii_case(prefix);
        if has_prefix {
            return radix_number(&trimmed[2..], radix);
        }
    }
    let parsed = trimmed.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

/// JS `Number("0x10")`-style literals: ASCII digits only (JS rejects a sign
/// there), value `None` when it is not finite/parseable.
fn radix_number(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix)
        .ok()
        .map(|value| value as f64)
}

/// JS `Number(value)` for the value shapes the repair rules can see: numbers,
/// numeric strings, booleans, `[]` → `0` and `[x]` → `Number(x)`; anything
/// else (multi-element arrays, objects) is `NaN`, i.e. no finite coercion.
///
/// `null` is excluded by the caller (`timeoutMs !== null`), matching the TS
/// guard; `undefined` has no JSON representation.
fn js_number_coercion(value: &JsonValue) -> Option<f64> {
    match value {
        JsonValue::Number(number) => number.as_f64().filter(|number| number.is_finite()),
        JsonValue::String(text) => js_number_from_str(text),
        JsonValue::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        JsonValue::Array(items) => match items.as_slice() {
            [] => Some(0.0),
            [single] => js_number_coercion(single),
            _ => None,
        },
        JsonValue::Object(_) | JsonValue::Null => None,
    }
}

/// A JS number as a JSON number, keeping integral values integral so the
/// serialized form matches `JSON.stringify(Number("20"))` (`20`, not `20.0`).
fn number_json(number: f64) -> JsonValue {
    if number.fract() == 0.0 && number >= (i64::MIN as f64) && number <= (i64::MAX as f64) {
        return JsonValue::Number(serde_json::Number::from(number as i64));
    }
    match serde_json::Number::from_f64(number) {
        Some(number) => JsonValue::Number(number),
        // Non-finite values are filtered by every caller; staying total beats
        // panicking on a debug-only repair path.
        None => JsonValue::Null,
    }
}

// ---------------------------------------------------------------------------
// Pass 4: phantom toolUse normalization
// ---------------------------------------------------------------------------

/// TS `normalizePhantomToolUse` (`src/index.ts:414-434`): a `toolUse` stop
/// reason with no `toolCall` block becomes a retryable error. Returns whether
/// it fired.
pub fn normalize_phantom_tool_use(message: &mut AssistantMessage) -> bool {
    if message.stop_reason != StopReason::ToolUse {
        return false;
    }
    let has_tool_calls = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)));
    if has_tool_calls {
        return false;
    }
    message.stop_reason = StopReason::Error;
    message.error_message = Some(PHANTOM_TOOL_USE_ERROR.to_owned());
    true
}

// ---------------------------------------------------------------------------
// The message pipeline
// ---------------------------------------------------------------------------

/// The full `message_end` pipeline (`tool-repair.ts:109-173`) over one
/// assistant message, in place — Step 1a-compatible entry point.
///
/// Identical to [`repair_message_with_grammar`] with the default (disabled)
/// grammar config and an empty known-tool set, i.e. the Step 2 grammar pass is
/// gated off and the behavior is exactly the Step 1a pipeline the existing
/// suite pins. Step 1b's loop hook calls [`repair_message_with_grammar`].
pub fn repair_message(
    message: &mut AssistantMessage,
    model: Option<&str>,
    tools: &[LiveToolSchema],
    check: &dyn Fn(&JsonValue, &JsonValue) -> SchemaCheck,
    debug: bool,
) -> MessageRepairOutcome {
    repair_message_with_grammar(
        message,
        model,
        tools,
        &GrammarRepairConfig::default(),
        &BTreeSet::new(),
        check,
        debug,
    )
}

/// The full `message_end` pipeline (`tool-repair.ts:109-173`) over one
/// assistant message, in place: the two model-gated strip passes, the
/// grammar-leak pass (Step 2), the live-schema repair, and the phantom-toolUse
/// normalization — in that order (module docs).
///
/// `model` is the session model id (`ctx.model.id` in the TS hook); `None`
/// disables both model-gated strip passes, like the TS `safeGetModel` miss.
/// `tools` are the active tools' live schemas (name → `parameters()`); calls
/// whose tool is absent (or whose schema is not an object) are left alone, the
/// TS `if (!schema) return part` fast path.
///
/// `grammar_config` is the package's grammar block; it is resolved per model
/// through [`grammar::resolve_grammar_repair_for_model`] (the `enabled: true`
/// OR `leakModels` gate) and the pass runs only when the resolution enables
/// it. `known_tools` is the active tool-name registry the recovery gate needs
/// (`requireKnownTool`; an **empty** set recovers nothing).
///
/// `check` is the live-schema validator (see the module docs): arguments are
/// validated with it and a repaired candidate is committed only when it
/// re-validates. `debug` enables the package's `PI_TOOL_REPAIR_DEBUG` logging
/// ([`super::config::debug_enabled`] is the env-backed source of that flag).
pub fn repair_message_with_grammar(
    message: &mut AssistantMessage,
    model: Option<&str>,
    tools: &[LiveToolSchema],
    grammar_config: &GrammarRepairConfig,
    known_tools: &BTreeSet<String>,
    check: &dyn Fn(&JsonValue, &JsonValue) -> SchemaCheck,
    debug: bool,
) -> MessageRepairOutcome {
    let mut report = RepairReport::default();
    let mut changed = false;

    // The strip passes only have argument maps to work on, so they need a
    // toolCall block; the grammar pass below must run even without one (a
    // leaked-grammar message is precisely the one with no native tool call).
    let has_tool_calls = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)));
    if has_tool_calls {
        let grammar_leak = has_grammar_leak_bug(model);
        let anchor_bleed = has_anchor_bleed_bug(model);

        if grammar_leak || anchor_bleed {
            for block in message.content.iter_mut() {
                let ContentBlock::ToolCall(call) = block else {
                    continue;
                };
                let mut touched = false;
                if grammar_leak && strip_grammar_token_leaks(&mut call.arguments) {
                    touched = true;
                }
                if anchor_bleed && strip_anchor_bleed(&mut call.arguments) {
                    touched = true;
                }
                if touched {
                    changed = true;
                }
            }
        }
    }

    // Pass 3 (Step 2): grammar-leak repair. The pass resolves its own
    // `enabled || leakModels` gate and reports whether it stripped/recovered.
    let effective_grammar = grammar::resolve_grammar_repair_for_model(grammar_config, model);
    if effective_grammar.enabled
        && grammar::repair_message_grammar_leaks(message, &effective_grammar, known_tools).changed
    {
        changed = true;
    }

    // Pass 4: live-schema repair, over the message as the grammar pass left it
    // (recovered calls included, like the TS hook's second call).
    let has_tool_calls_now = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)));
    if has_tool_calls_now {
        let schemas: BTreeMap<&str, &JsonValue> = tools
            .iter()
            .filter(|tool| tool.parameters.is_object())
            .map(|tool| (tool.name.as_str(), &tool.parameters))
            .collect();

        for block in message.content.iter_mut() {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            let Some(schema) = schemas.get(call.name.as_str()).copied() else {
                continue;
            };
            let arguments = JsonValue::Object(call.arguments.clone().into_iter().collect());
            let outcome = repair_tool_call_arguments(&call.name, &arguments, schema, check);
            if outcome.status != LiveRepairStatus::Unchanged {
                if debug {
                    log_repair(&outcome);
                }
                report.calls.push(ToolCallRepair {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    status: outcome.status,
                    rules_fired: outcome.rules_fired.clone(),
                    hints: outcome.hints.clone(),
                });
            }
            if outcome.status == LiveRepairStatus::Recovered {
                if let Some(repaired) = outcome
                    .repaired
                    .and_then(|value| value.as_object().cloned())
                {
                    call.arguments = repaired.into_iter().collect();
                    changed = true;
                }
            }
        }
    }

    if normalize_phantom_tool_use(message) {
        if debug {
            crate::ext_diag_info!(
                "[pi-tool-repair] phantom-tooluse: converted stopReason from \"toolUse\" \
                 to retryable error (no toolCall blocks)"
            );
        }
        report.phantom_normalized = true;
        changed = true;
    }

    if changed {
        MessageRepairOutcome::Repaired(report)
    } else if report.calls.is_empty() {
        MessageRepairOutcome::Unchanged
    } else {
        MessageRepairOutcome::Unrepairable(report)
    }
}

/// The package's `logRepair` (`src/index.ts:949-963`), reduced to one stderr
/// line per repair while keeping the same information level (tool, outcome,
/// rules, input, repaired candidate, hints).
fn log_repair(outcome: &LiveToolRepair) {
    let outcome_name = match outcome.status {
        LiveRepairStatus::Recovered => "recovered",
        LiveRepairStatus::Unrepairable => "unrepairable",
        LiveRepairStatus::Unchanged => "unchanged",
    };
    let rules = if outcome.rules_fired.is_empty() {
        "none".to_owned()
    } else {
        outcome.rules_fired.join(",")
    };
    let mut line = format!(
        "[pi-tool-repair] tool={} outcome={} rules={} hints={} input={}",
        outcome.tool_name,
        outcome_name,
        rules,
        outcome.hints.len(),
        outcome.input
    );
    if let Some(repaired) = &outcome.repaired {
        line.push_str(" repaired=");
        line.push_str(&repaired.to_string());
    }
    for (index, hint) in outcome.hints.iter().enumerate() {
        line.push_str(&format!(" hint[{index}]={hint}"));
    }
    crate::ext_diag_info!("{line}");
}

// ---------------------------------------------------------------------------
// Phase 0: `before_provider_request` schema-anchor sanitization (Step 3)
// ---------------------------------------------------------------------------

/// TS `sanitizePattern` (`src/index.ts:111-117`): a schema `pattern` that mixes
/// alternation with string anchors is dropped entirely — after anchor bleed the
/// anchors are unreliable and a surviving `|` would match far more than
/// intended. Otherwise the `^`/`$` characters are stripped; an empty result
/// drops the key too.
///
/// `None` = drop the `pattern` key; `Some(text)` = the rewritten value.
///
/// Invariant (TS-faithful): the strip removes **every** `^`/`$` character (TS
/// `pattern.replace(/\^|\$/g, "")`), not just the leading/trailing ones. The
/// message-end anchor strip ([`strip_anchor_bleed`]) is the boundary-only pass;
/// the two deliberately differ.
pub fn sanitize_pattern(pattern: &str) -> Option<String> {
    if pattern.contains('|') && (pattern.contains('^') || pattern.contains('$')) {
        return None;
    }
    let stripped: String = pattern
        .chars()
        .filter(|character| *character != '^' && *character != '$')
        .collect();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

/// TS `sanitizeSchemaAnchors` (`src/index.ts:119-134`) as an in-place walk:
/// at every object level a string `pattern` goes through [`sanitize_pattern`]
/// (delete on `None`); arrays and object values recurse. A non-object,
/// non-array value is never rewritten.
///
/// In-place is the host contract for event payloads (later plugins observe the
/// rewrite) and keeps untouched bytes and the `preserve_order` key order; the
/// TS rebuild returns a new object with the same observable content. Returns
/// whether anything changed — an unchanged subtree is not rewritten, so a
/// gated model whose schemas carry no anchors yields `false` and the caller
/// can leave the payload alone.
fn sanitize_schema_anchors_in_place(schema: &mut JsonValue) -> bool {
    match schema {
        JsonValue::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= sanitize_schema_anchors_in_place(item);
            }
            changed
        }
        JsonValue::Object(map) => {
            let mut changed = false;
            // The key check sits BEFORE the value recursion, like the TS branch
            // order: a string `pattern` is consumed by [`sanitize_pattern`] and
            // never recursed into; a non-string `pattern` (object/array) falls
            // through to the recursion below exactly like TS.
            let pattern = map
                .get("pattern")
                .and_then(JsonValue::as_str)
                .map(str::to_owned);
            if let Some(pattern) = pattern {
                match sanitize_pattern(&pattern) {
                    Some(sanitized) if sanitized == pattern => {}
                    Some(sanitized) => {
                        map.insert("pattern".to_owned(), JsonValue::String(sanitized));
                        changed = true;
                    }
                    None => {
                        map.remove("pattern");
                        changed = true;
                    }
                }
            }
            for (key, value) in map.iter_mut() {
                // The string `pattern` was handled above; recursing into the
                // string would be a no-op either way.
                if key == "pattern" && value.is_string() {
                    continue;
                }
                changed |= sanitize_schema_anchors_in_place(value);
            }
            changed
        }
        _ => false,
    }
}

/// [`sanitize_schema_anchors_in_place`] as the TS value function: a sanitized
/// copy of `schema` (primitives and arrays pass through the same rules).
pub fn sanitize_schema_anchors(schema: &JsonValue) -> JsonValue {
    let mut sanitized = schema.clone();
    sanitize_schema_anchors_in_place(&mut sanitized);
    sanitized
}

/// The `before_provider_request` Phase 0 pass (`tool-repair.ts:73-105`):
/// sanitize JSON-schema `pattern` anchors in a serialized provider request
/// body, gated on [`has_anchor_bleed_bug`] (the `anchorBleedModels` set).
///
/// Two payload locations, exactly the TS handler's:
/// - `tools[].function.parameters` — the OpenAI-family tool shape pir's
///   `convert_tool` emits (`src/ai/api/openai/mod.rs:500-535`). A non-array
///   `tools` or a tool without `function.parameters` is skipped;
/// - `response_format.json_schema.schema` — no pir adapter emits it today; the
///   path is ported for upstream parity and no-ops when absent.
///
/// Returns whether `payload` changed. A non-gated (or not yet pushed) model and
/// a payload with neither location are no-ops, so `false` is the common case
/// and the host falls back to the original body.
pub fn sanitize_request_payload(payload: &mut JsonValue, model: Option<&str>) -> bool {
    if !has_anchor_bleed_bug(model) {
        return false;
    }
    let mut changed = false;
    if let Some(tools) = payload.get_mut("tools").and_then(JsonValue::as_array_mut) {
        for tool in tools {
            if let Some(parameters) = tool
                .get_mut("function")
                .and_then(|function| function.get_mut("parameters"))
            {
                changed |= sanitize_schema_anchors_in_place(parameters);
            }
        }
    }
    if let Some(schema) = payload
        .get_mut("response_format")
        .and_then(|response_format| response_format.get_mut("json_schema"))
        .and_then(|json_schema| json_schema.get_mut("schema"))
    {
        changed |= sanitize_schema_anchors_in_place(schema);
    }
    changed
}
