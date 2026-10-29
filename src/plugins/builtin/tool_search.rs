//! The `tool_search` builtin (upstream `extensions/tool-search/{index,tool}.ts`):
//! a BM25-ranked search over the NOT-yet-active searchable tools
//! (codemode/deferred exposure), loading the matches into the active set.
//!
//! Upstream ships it as a JS extension with `defaultActive: false` and the
//! `getAllTools/getActiveTools/setActiveTools` API slice; pir implements it
//! as a Rust builtin whose tool reads the live merged registry through
//! [`ToolSessionEnv::live_tools`] and performs the additive activation by
//! writing [`ToolSessionEnv::active_override`] — the session-side override
//! cell, NOT the JS runtime's own `setActiveTools` copy (the two stores and
//! the merge rule that keeps both are documented below).
//!
//! Activation takes effect when the session next rebuilds its tool loadout
//! (`agent_session::tools::build_tool_loadout`, once per settle iteration and
//! at the start of every prompt): `declared_tools` (`agent::tool_execution`)
//! declares every active non-hidden tool, so activated `codemode`/`deferred`
//! matches join the model's tool list exactly like a `setActiveTools`
//! activation. The run's request tool list (`context.tools`) is seeded ONCE
//! per run (`agent::agent_loop`), so an activation landing while a run is in
//! flight is not visible to the next request of that same run — the next
//! prompt, not the next call (upstream rebuilds the prompt at
//! `setActiveTools` time; this remaining one-request window is the documented
//! deviation). A resume re-derives the session's active set from the
//! transcript (`agent_session::tools::restore_recorded_active_tools`,
//! upstream `_restoreToolsFromTranscript`): matches loaded before the resume
//! are re-activated as far as the transcript declared them, so a match that
//! only ever lived in this cell without being declared is the one that cannot
//! come back.
//!
//! The read-modify-write of the override cell runs under ONE write guard
//! (same-batch `tool_search` calls serialize instead of losing each other's
//! matches); the additive-union computation living in the tool (not the
//! override store) is the remaining deviation from upstream
//! `searchAndLoad`/`Bm25Ranker` beyond the language port itself.
//!
//! ## The two active-set stores (and the merge rule)
//!
//! Two stores hold "the active set" and the loadout consults them in ONE
//! fixed precedence chain (`agent_session::tools::build_tool_loadout`): this
//! file's cell (`ToolSessionEnv::active_override`) wins over the JS runtime's
//! `setActiveTools` override (written by `setActiveToolsJson` into the
//! runtime state; read through `PluginHost::active_tools_override`), which
//! wins over the host-resolved selection. The JS override ALSO shapes the run
//! registry itself: `merge_plugin_tools` prunes every merged name outside it,
//! so a JS override makes the pruned run registry exactly its own name set.
//! The cell can shadow the JS store (it wins the chain forever), so the merge
//! rule keeps both sources instead of replacing one with the other:
//!
//! - the baseline active set is the store the loadout currently resolves:
//!   the cell when one exists (intersected with the live registry — a cell
//!   name no longer in the run registry cannot be active), else the JS
//!   override when the run registry carries its prune signature (a session
//!   core tool missing from the live registry; core tools always join the
//!   merged registry unless the JS override's prune removed them, and the
//!   host-resolved `tool_selection` is only the active override — it never
//!   prunes the run registry (the F12 rule) — so it cannot produce the
//!   signature), else the session's resolved active set (the full
//!   precedence chain: JS store, host selection, then the exposure defaults);
//! - the additive union `baseline ∪ matches` is written back to the cell —
//!   the same store the baseline was read from — so neither the cell's
//!   earlier matches nor the JS store's names are dropped by a load;
//! - when the JS override owns the run AND the cell had been shadowing it,
//!   the cell is reconciled to the JS set before searching: the JS
//!   activation stops being masked, and with the run registry pruned to the
//!   JS names there is nothing left to search anyway. A shadow-free cell
//!   (None) is left untouched — writing it would only create shadowing for
//!   the JS store's own future updates.
//!
//! The stores themselves are not readable from a tool execution, but the
//! session's RESOLUTION of them is: the plugin caches the [`PluginContext`]
//! bound to its host ([`AgentPlugin::on_context`], the same seam the `mcp`
//! builtin uses) and asks the session service for `active_tools()` — the
//! live `getActiveTools()` value (cell, else JS store, else the host-resolved
//! selection, else the exposure defaults). The baseline is therefore the
//! currently resolved active set, so a `--tools` selection stays active
//! across a load and the search tool never un-declares itself (the W19-CL15
//! defect: `baseline ∪ matches` used to start from the exposure defaults).
//!
//! Residual boundaries: (a) a host with no bound session service behind the
//! tool (print mode drives `agent_loop` without an `AgentSession`, and
//! builds `ToolSessionEnv` directly) has no resolution to read, so the
//! exposure defaults remain its only observable baseline; (b) writing the
//! union to the cell still shadows a later JS `setActiveTools` (the store
//! trade-off above; upstream writes its ONE store instead). Loaded
//! `codemode`/`deferred` matches are declared with the rest of the active set
//! by `declared_tools`; the only remaining delay is the one-request window
//! above.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::BoxFuture;
use crate::agent::tool_execution::{
    AgentTool, ToolContext, ToolExposure, ToolNamespace, ToolReach,
};
use crate::plugins::plugin::{AgentPlugin, PluginContext, RegistrationContext};
use crate::plugins::source::PluginInfo;

// The registered tool name lives in the (ungated) `builtin` module: the MCP
// reach layer references it whether or not this builtin is compiled in.
use super::TOOL_SEARCH_TOOL_NAME;

/// How many matches load when the model passes no `limit`
/// (upstream `DEFAULT_TOOL_SEARCH_LIMIT`).
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";

// ---------------------------------------------------------------------------
// Text pipeline (upstream tool.ts:39-116)
// ---------------------------------------------------------------------------

/// Stop words dropped from queries and documents (upstream `STOP_WORDS`,
/// tool.ts:39-61: `without` is not a stop word there, `this` is).
const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Lowercase, split camelCase and non-alphanumeric runs, drop stop words,
/// naive-stem plurals (upstream `tokenize` / `stem`, tool.ts:64-80).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw in split_identifier_words(text) {
        let lowered = raw.to_lowercase();
        if lowered.is_empty() || STOP_WORDS.contains(&lowered.as_str()) {
            continue;
        }
        tokens.push(stem(&lowered));
    }
    tokens
}

/// camelCase / snake_case / kebab-case aware word split (upstream
/// `tokenize`'s pipeline: `([a-z0-9])([A-Z])` -> `$1 $2`,
/// `([A-Z]+)([A-Z][a-z])` -> `$1 $2`, lowercase, then `split(/[^a-z0-9]+/)`).
fn split_identifier_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    // Pass 1: a lowercase letter or digit immediately followed by an
    // uppercase letter starts a new word (`readFile` -> `read File`).
    let mut first = String::with_capacity(text.len() + 4);
    for (index, character) in chars.iter().enumerate() {
        if index > 0
            && character.is_ascii_uppercase()
            && (chars[index - 1].is_ascii_lowercase() || chars[index - 1].is_ascii_digit())
        {
            first.push(' ');
        }
        first.push(*character);
    }
    // Pass 2: an uppercase run followed by `[A-Z][a-z]` splits before its
    // last uppercase letter (`HTTPServer` -> `HTTP Server`, `ABCd` -> `AB Cd`).
    let chars: Vec<char> = first.chars().collect();
    let mut second = String::with_capacity(first.len() + 4);
    for (index, character) in chars.iter().enumerate() {
        if index > 0
            && chars[index - 1].is_ascii_uppercase()
            && character.is_ascii_uppercase()
            && chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase())
        {
            second.push(' ');
        }
        second.push(*character);
    }
    second
        .to_lowercase()
        .split(|character: char| !character.is_ascii_lowercase() && !character.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Naive plural stem (upstream `stem`, tool.ts:64-69).
fn stem(token: &str) -> String {
    if token.len() > 4 && token.ends_with("ies") {
        return format!("{}y", &token[..token.len() - 3]);
    }
    if token.len() > 4
        && (token.ends_with("ches")
            || token.ends_with("shes")
            || token.ends_with("sses")
            || token.ends_with("xes")
            || token.ends_with("zes"))
    {
        return token[..token.len() - 2].to_owned();
    }
    if token.len() > 3 && token.ends_with('s') && !token.ends_with("ss") {
        return token[..token.len() - 1].to_owned();
    }
    token.to_owned()
}

/// Okapi BM25 over the candidate documents (upstream `Bm25Ranker`,
/// tool.ts:119-157; k1=1.2, b=0.75). The rank order is a documented total
/// key: score descending, then tool name ascending ([`Self::rank`]).
pub struct Bm25Ranker {
    documents: Vec<(Vec<String>, String)>, // (tokens, id)
    document_frequency: BTreeMap<String, usize>,
    average_length: f64,
}

impl Bm25Ranker {
    pub fn new(documents: Vec<(String, String)>) -> Self {
        let documents: Vec<(Vec<String>, String)> = documents
            .into_iter()
            .map(|(id, text)| (tokenize(&text), id))
            .collect();
        let mut document_frequency: BTreeMap<String, usize> = BTreeMap::new();
        for (tokens, _) in &documents {
            for token in tokens.iter().collect::<std::collections::BTreeSet<_>>() {
                *document_frequency.entry(token.clone()).or_default() += 1;
            }
        }
        let average_length = if documents.is_empty() {
            0.0
        } else {
            documents
                .iter()
                .map(|(tokens, _)| tokens.len())
                .sum::<usize>() as f64
                / documents.len() as f64
        };
        Self {
            documents,
            document_frequency,
            average_length,
        }
    }

    /// Ranked ids with score > 0, truncated to `limit` (upstream `rank`).
    pub fn rank(&self, query: &str, limit: usize) -> Vec<(String, f64)> {
        let terms: Vec<String> = {
            let mut seen = std::collections::BTreeSet::new();
            tokenize(query)
                .into_iter()
                .filter(|token| seen.insert(token.clone()))
                .collect()
        };
        let count = self.documents.len() as f64;
        let k1 = 1.2_f64;
        let b = 0.75_f64;
        let mut scored: Vec<(String, f64)> = Vec::new();
        for (tokens, id) in &self.documents {
            let mut score = 0.0;
            for term in &terms {
                let frequency = tokens.iter().filter(|token| *token == term).count() as f64;
                if frequency == 0.0 {
                    continue;
                }
                let df = self.document_frequency.get(term).copied().unwrap_or(0) as f64;
                let idf = (1.0 + (count - df + 0.5) / (df + 0.5)).ln();
                let norm = k1 * (1.0 - b + b * tokens.len() as f64 / self.average_length.max(1.0));
                score += idf * (frequency * (k1 + 1.0)) / (frequency + norm);
            }
            if score > 0.0 {
                scored.push((id.clone(), score));
            }
        }
        // Deterministic ordering (upstream's sort is by score only and stable,
        // so equal scores keep its registration-ordered document list; pir's
        // candidate source is a HashMap-backed registry, whose iteration order
        // varies per run). The documented total key is:
        //
        //   primary:   score, descending
        //   tie-break: tool name, ascending (byte order)
        //
        // `sort_by` is a stable sort, and names are unique in the merged
        // registry, so the result is independent of document/input order —
        // never of hashing.
        scored.sort_by(|(id_a, score_a), (id_b, score_b)| {
            score_b
                .partial_cmp(score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| id_a.cmp(id_b))
        });
        scored.truncate(limit);
        scored
    }
}

/// The searchable document text: tool name (+ space-separated), description,
/// the recursive schema text, and the namespace's name/description/
/// instructions (upstream `createToolSearchDocument`, tool.ts:103-116).
fn tool_document(
    name: &str,
    description: &str,
    parameters: Option<&Value>,
    namespace: Option<&ToolNamespace>,
) -> String {
    let mut document = format!("{name} {} {description}", name.replace('_', " "));
    if let Some(schema) = parameters {
        append_schema_text(schema, &mut document);
    }
    if let Some(namespace) = namespace {
        for part in [
            Some(namespace.name.as_str()),
            namespace.description.as_deref(),
            namespace.instructions.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !part.trim().is_empty() {
                document.push(' ');
                document.push_str(part);
            }
        }
    }
    document
}

/// Recursive schema text (upstream `schemaText`, tool.ts:86-101): only object
/// schemas contribute — a description, property names plus their subtrees, an
/// object-valued `items`, and the `anyOf`/`oneOf`/`allOf` variant arrays.
/// Array-valued `items` and non-object variants contribute nothing, exactly
/// like upstream's `isObject` guard.
fn append_schema_text(schema: &Value, document: &mut String) {
    let Value::Object(map) = schema else {
        return;
    };
    if let Some(description) = map.get("description").and_then(Value::as_str) {
        document.push(' ');
        document.push_str(description);
    }
    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            document.push(' ');
            document.push_str(name);
            append_schema_text(property, document);
        }
    }
    if let Some(items) = map.get("items") {
        append_schema_text(items, document);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = map.get(key).and_then(Value::as_array) {
            for variant in variants {
                append_schema_text(variant, document);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// The `tool_search` tool: model-only exposure, never default-active.
struct ToolSearchTool {
    /// The plugin's bound context cell (see [`ToolSearchPlugin::context`]).
    context: Arc<std::sync::RwLock<Option<PluginContext>>>,
}

impl ToolSearchTool {
    /// The session's currently resolved active set (upstream
    /// `getActiveTools()`): the same precedence chain the loadout applies —
    /// override cell, JS store, host selection, exposure defaults. `None`
    /// when no session service is bound behind this tool (a host that drives
    /// `agent_loop` without an `AgentSession`); the caller then falls back to
    /// the exposure defaults.
    fn resolved_active(&self) -> Option<Vec<String>> {
        let context = self
            .context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let session = context.session()?;
        Some(
            session
                .active_tools()
                .iter()
                .map(|name| name.to_string())
                .collect(),
        )
    }

    /// Tool names the session's default-tools selection explicitly excluded
    /// (`--exclude-tools`, or a `-name` entry of a `--tools` modifier list).
    ///
    /// An exclusion outranks every activation source (upstream resolves
    /// `_excludedTools` at registration, so the tool is not registered at
    /// all). `tool_search` would otherwise load such a name back into the
    /// active set through its additive union, un-deleting a tool the user
    /// removed. Empty without a bound session (nothing to honor).
    fn default_exclusions(&self) -> Vec<String> {
        let context = self
            .context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(session) = context.as_ref().and_then(|context| context.session()) else {
            return Vec::new();
        };
        session
            .default_tool_exclusions()
            .iter()
            .map(|name| name.to_string())
            .collect()
    }
}

impl AgentTool for ToolSearchTool {
    fn name(&self) -> &'static str {
        TOOL_SEARCH_TOOL_NAME
    }
    fn description(&self) -> &'static str {
        TOOL_SEARCH_DESCRIPTION
    }
    fn parameters(&self) -> Option<Value> {
        // Upstream `toolSearchSchema` (`tool.ts:159-164`) verbatim: a plain
        // `number` for `limit` (the integer/positive check lives in
        // `execute`, not in the schema) and the same descriptions.
        Some(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query for deferred tools."
                },
                "limit": {
                    "type": "number",
                    "description": format!(
                        "Maximum number of tools to return. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}."
                    )
                }
            },
            "required": ["query"]
        }))
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some("Search for tools that are not loaded yet and load the matches".to_owned())
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::ModelOnly
    }
    fn default_active(&self) -> bool {
        false
    }
    fn reaches_tools(&self, _input: &Value) -> Option<ToolReach> {
        // Upstream `reachesTools: () => "all"`: a query can match any tool that
        // is not declared yet, so owners that register tools late (MCP servers
        // still connecting) wait for every server before the search runs, and
        // the MCP side auto-activates this tool for `deferred` servers.
        Some(ToolReach::All)
    }
    fn run(&self, args: &Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let raw_limit = args.get("limit").cloned();
        let env = ctx.session_env.clone();
        // The session's resolved active set (upstream `getActiveTools()`),
        // read HERE: the boxed future is `'static`, so it cannot borrow
        // `self`, and the answer is a snapshot either way (the write-back
        // below takes the cell under its own guard).
        let resolved_active = self.resolved_active();
        // A default-tools selection's `--exclude-tools` / `-name` entries
        // outrank activation, so an excluded tool must not even be a
        // candidate: a match would be unioned back into the active set and
        // un-delete a tool the user removed. Read here for the same `'static`
        // reason as the active set above.
        let exclusions = self.default_exclusions();
        Box::pin(async move {
            let Some(env) = env else {
                return tool_error("tool_search requires a session environment");
            };
            if query.trim().is_empty() {
                return tool_error("query must not be empty");
            }
            // Upstream `execute` (`tool.ts:230-233`): `limit ?? 8`, then
            // `!Number.isInteger(max) || max <= 0` -> throw. A JSON number is
            // an integer here exactly when it is finite and has no fraction;
            // the saturated `as usize` cast matches JS's unbounded slice limit
            // for absurdly large values.
            let limit = match raw_limit {
                None | Some(Value::Null) => DEFAULT_TOOL_SEARCH_LIMIT,
                Some(value) => match value.as_f64() {
                    Some(number) if number.is_finite() && number.fract() == 0.0 && number > 0.0 => {
                        number as usize
                    }
                    _ => return tool_error("limit must be a positive integer"),
                },
            };
            let Some(live_tools) = &env.live_tools else {
                return tool_error("tool_search found no live tool registry");
            };
            // ONE critical section for the read-modify-write: the override
            // cell's write guard is taken BEFORE the active set is read and
            // held through the union write-back below, so two `tool_search`
            // calls in the same batch (parallel tool execution) cannot read
            // the same active set and then overwrite each other's matches.
            let mut override_guard = env.active_override.as_ref().map(|cell| {
                cell.write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            // The cell value, if this session carries one: tool_search's own
            // store and the loadout chain's first-precedence source.
            let cell_names: Option<Vec<String>> =
                override_guard.as_deref().and_then(|names| names.clone());
            // Names present in the run's merged registry. When a JS
            // `setActiveTools` override is in effect, `merge_plugin_tools`
            // pruned the registry to exactly that override's names, so every
            // live name IS the JS active set.
            let live_names: std::collections::BTreeSet<String> =
                live_tools.names().into_iter().map(str::to_owned).collect();
            // Prune signature of the JS override: a session core tool that
            // left the run registry. Core tools always join the merged
            // registry (the host selection is only the active override and
            // never prunes the registry — the F12 rule — and core tools are
            // never `hidden`), hence only `merge_plugin_tools`' override
            // prune can remove them.
            let js_override_pruned = env
                .base_tools
                .names()
                .into_iter()
                .any(|name| !live_names.contains(name));
            // The baseline active set: the store the loadout currently
            // resolves (see the module doc's merge rule).
            let (baseline, reconcile_to_js): (std::collections::BTreeSet<String>, bool) =
                match (&cell_names, js_override_pruned) {
                    (Some(cell), false) => (
                        cell.iter()
                            .filter(|name| live_names.contains(*name))
                            .cloned()
                            .collect(),
                        false,
                    ),
                    // The JS override owns the run but the stale cell still
                    // shadows it: mirror the JS set into the cell (the JS
                    // activation must not keep being masked).
                    (Some(_), true) => (live_names.clone(), true),
                    // The JS override owns the run and the cell is absent:
                    // leave the cell alone (writing it would shadow the JS
                    // store's future updates).
                    (None, true) => (live_names.clone(), false),
                    (None, false) => {
                        // No cell, no JS prune: the baseline is the session's
                        // RESOLVED active set (upstream `getActiveTools()`:
                        // JS store, host selection, then the exposure
                        // defaults) — not the exposure defaults alone, which
                        // would drop a `--tools tool_search,...` selection
                        // from the write-back and un-declare this very tool.
                        // The session service answers with the names it
                        // resolves against ITS merged registry, so intersect
                        // with the run's live registry like the cell branch.
                        let baseline = match resolved_active {
                            Some(names) => names
                                .into_iter()
                                .filter(|name| live_names.contains(name))
                                .collect(),
                            None => live_tools.active_names(None).into_iter().collect(),
                        };
                        (baseline, false)
                    }
                };
            if reconcile_to_js {
                if let Some(guard) = override_guard.as_mut() {
                    **guard = Some(baseline.iter().cloned().collect());
                }
            }
            // Candidates: searchable exposure (codemode | deferred), not
            // already active (upstream `searchAndLoad`'s filter).
            //
            // DETERMINISTIC ORDER: `live_tools` is a `HashMap`-backed
            // registry, so iteration order varies between runs. Upstream's
            // document order is registration order and its BM25 ties are
            // stable, so the loaded set/order would otherwise be
            // nondeterministic for equal scores. Sort by tool name — a total,
            // stable order with no dependence on hashing or insertion
            // history — before ranking.
            let mut candidates: Vec<&Arc<dyn crate::agent::tool_execution::AgentTool>> = live_tools
                .iter()
                .filter(|tool| {
                    !crate::coding_agent::tool_selection::tool_excluded_by(&exclusions, tool.name())
                        && matches!(
                            tool.exposure(),
                            ToolExposure::Codemode | ToolExposure::Deferred
                        )
                        && !baseline.contains(tool.name())
                })
                .collect();
            candidates.sort_by(|left, right| left.name().cmp(right.name()));
            let documents: Vec<(String, String)> = candidates
                .into_iter()
                .map(|tool| {
                    (
                        tool.name().to_owned(),
                        tool_document(
                            tool.name(),
                            tool.description(),
                            tool.parameters().as_ref(),
                            tool.namespace().as_ref(),
                        ),
                    )
                })
                .collect();
            if documents.is_empty() {
                return loaded_result(&[]);
            }
            let ranker = Bm25Ranker::new(documents);
            let matches: Vec<(String, String)> = ranker
                .rank(&query, limit)
                .into_iter()
                .map(|(id, _)| {
                    // The model-facing result carries the first line of the
                    // candidate's description (upstream `searchAndLoad`'s
                    // `{ name, description }` matches).
                    let description = live_tools
                        .get(&id)
                        .map(|tool| tool.description().to_owned())
                        .unwrap_or_default();
                    (id, description)
                })
                .collect();
            if matches.is_empty() {
                return loaded_result(&[]);
            }
            // Additive union into the active set (upstream
            // `setActiveTools([...active, ...matches])`), still under the same
            // guard: the write-back cannot interleave with another call's
            // read. The baseline already carries every source the loadout
            // consulted, so the union loses neither the cell's earlier loads
            // nor the JS store's names.
            if let Some(mut guard) = override_guard {
                let mut names = baseline.iter().cloned().collect::<Vec<_>>();
                names.extend(matches.iter().map(|(name, _)| name.clone()));
                *guard = Some(names);
            }
            loaded_result(&matches)
        })
    }
}

fn tool_error(message: &str) -> ToolResult {
    ToolResult {
        content: message.to_owned(),
        images: Vec::new(),
        details: None,
        structured_content: None,
        is_error: true,
        terminate: false,
    }
}

fn loaded_result(loaded: &[(String, String)]) -> ToolResult {
    let text = if loaded.is_empty() {
        "No matching tools found.".to_owned()
    } else {
        // Upstream `createToolSearchToolDefinition`'s result text, including
        // the description's first line per match. The activated names are
        // declared on the session's next loadout rebuild; within the current
        // run the request tool list is already fixed (the module doc's
        // documented one-request window).
        format!(
            "Loaded {} tool{}. They are available from your next call:\n{}",
            loaded.len(),
            if loaded.len() == 1 { "" } else { "s" },
            loaded
                .iter()
                .map(|(name, description)| {
                    let first_line = description.trim().split(['\r', '\n']).next().unwrap_or("");
                    format!("- {name}: {first_line}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    ToolResult {
        content: text,
        images: Vec::new(),
        details: Some(json!({
            "loaded": loaded
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
        })),
        structured_content: None,
        is_error: false,
        terminate: false,
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// The `tool-search` builtin plugin: registers [`ToolSearchTool`].
pub struct ToolSearchPlugin {
    info: PluginInfo,
    /// The live plugin context, once the host binds it (upstream
    /// `getActiveTools`). Shared with the registered tool: `register` runs
    /// BEFORE `on_context`, so the tool cannot capture a snapshot — it reads
    /// the cell at call time.
    context: Arc<std::sync::RwLock<Option<PluginContext>>>,
}

impl Default for ToolSearchPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSearchPlugin {
    pub fn new() -> Self {
        let mut info = PluginInfo::builtin("tool-search");
        // Replaceable: a user extension registering a `tool_search` tool
        // omits this builtin (upstream `replaceable` factory flag).
        info.replaceable =
            super::builtin_extension_def("tool-search").is_some_and(|def| def.replaceable);
        Self {
            info,
            context: Arc::new(std::sync::RwLock::new(None)),
        }
    }
}

impl AgentPlugin for ToolSearchPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_tool(Arc::new(ToolSearchTool {
            context: Arc::clone(&self.context),
        }));
        Ok(())
    }

    fn on_context(&self, ctx: &PluginContext) {
        *self
            .context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ctx.clone());
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}
