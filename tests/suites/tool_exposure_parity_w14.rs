//! W14-D: tool-exposure parity across the hosts that decide what the model can
//! see or call.
//!
//! The audit (`docs/tool-exposure-audit-w14.md`) traced every host path
//! (print, interactive, RPC, server, SDK, durable) through
//! `registration → merge → selection → exposure → declared → callable`, and
//! found two proven defects in the shared resolver, both of which make
//! `--no-tools` / `--exclude-tools` behave differently from upstream:
//!
//! 1. **`--no-tools` swallowed an explicit `--tools` list.** Upstream treats
//!    `noTools` as a suppression of the DEFAULT base only (`sdk.ts:282`:
//!    `defaultToolNames = options.noTools ? [] : ...`) while an explicit
//!    `--tools` list still selects (`sdk.ts:283-291`: `selectedToolNames =
//!    options.tools`, `allowedToolNames = options.tools`) — which is why the
//!    SDK builder's `.no_tools().tools([...])` path declares the named tools
//!    (`sdk/builder.rs:1469-1509`). The shared resolver guarded the allowlist
//!    branch with `if !no_tools`, so `pir print --no-tools --tools read`
//!    declared NOTHING while the SDK declared `read`: two hosts, one input,
//!    different request bodies.
//!
//! 2. **A default-derived selection dropped the denylist on resume.**
//!    `--exclude-tools` alone is a *default* selection (`uses_default_tools`,
//!    upstream `usesDefaultTools`), and a resumed session replaces a default
//!    selection with the loadout its transcript records
//!    (`agent_session/tools.rs::restore_recorded_active_tools`, upstream
//!    `_restoreToolsFromTranscript`). The exclusion was applied to `enabled`
//!    but not carried as a `-name` modifier, so a resumed
//!    `--exclude-tools grep` session re-declared the `grep` its transcript had
//!    recorded — the exact case `tests/d32_resume_active_tools.rs`
//!    `excluded_tools_are_not_restored` asserts, which only passes today when
//!    the recorded loadout happens not to name the excluded tool. Upstream
//!    cannot hit this: its `_isAllowedTool` (agent-session.ts:1542-1546) keeps
//!    the excluded tool out of the registry, so the recorded name has nothing
//!    to resolve against. pir keeps the full registry by design (the F12
//!    rule), so the exclusion must ride on `modifiers` instead — exactly what
//!    the SDK builder does (`sdk/builder.rs:1569-1587`).
//!
//! The tests below are expressed against the public helpers
//! (`resolve_tool_selection`, `ToolSelection`, `declared_tools`,
//! `callable_tools`, a real `AgentSession`), not a spawned binary.

use std::sync::Arc;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    callable_tools, declared_tools, AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry,
};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{
    Message, Model, ModelCost, ModelCostRates, TextContent, ThinkingLevel, Tool, UserMessage,
    UserRole,
};
use pir::coding_agent::tool_selection::resolve_tool_selection;
use pir::coding_agent::tools::{register_core_tools, CORE_TOOL_NAMES};
use pir::config::settings::DEFAULT_TOOL_NAMES;
use pir::session_manager::SessionManager;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|name| (*name).to_string()).collect()
}

/// The core registry's names (the base every CLI host starts from).
fn core_names() -> Vec<String> {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let mut names: Vec<String> = registry.iter().map(|tool| tool.name().to_owned()).collect();
    names.sort();
    names
}

/// Core names plus the plugin tools the audited hosts merge (the shape a print
/// or session registry has after `merge_plugin_tools`).
fn available_names() -> Vec<String> {
    let mut available = core_names();
    available.extend(names(&[
        "ctx_execute",
        "ctx_search",
        "python",
        "todo",
        "subagent",
        "tool_search",
        "codemode",
        "ext_deferred",
        "mcp__docs__search",
    ]));
    available.sort();
    available.dedup();
    available
}

fn resolve(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
) -> Result<Option<pir::coding_agent::ToolSelection>, String> {
    resolve_tool_selection(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        no_builtin_tools,
        &available_names(),
        &DEFAULT_TOOL_NAMES,
    )
}

/// [`resolve`] for the cases a test expects to succeed.
fn resolve_selection(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
) -> pir::coding_agent::ToolSelection {
    resolve(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        no_builtin_tools,
    )
    .expect("a well-formed selection resolves")
    .expect("a selection was requested")
}

/// A direct-exposure, default-active tool (the shape a core or `direct` plugin
/// tool has in the run registry).
struct NamedTool {
    name: String,
    exposure: ToolExposure,
    default_active: bool,
}

impl AgentTool for NamedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &'static str {
        "w14 fixture tool"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn default_active(&self) -> bool {
        self.default_active
    }
}

fn tool(name: &str) -> Arc<dyn AgentTool> {
    Arc::new(NamedTool {
        name: name.to_owned(),
        exposure: ToolExposure::Direct,
        default_active: true,
    })
}

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
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
        context_window: 8192,
        max_tokens: 512,
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

/// The registry an audited host runs with: the core tools plus the merged
/// plugin tools this suite's `available_names` describes.
fn run_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    for name in [
        "ctx_execute",
        "ctx_search",
        "python",
        "todo",
        "subagent",
        "tool_search",
        "codemode",
        "mcp__docs__search",
    ] {
        registry.register(tool(name));
    }
    registry.register(Arc::new(NamedTool {
        name: "ext_deferred".to_owned(),
        exposure: ToolExposure::Deferred,
        default_active: false,
    }));
    registry
}

/// A system message declaring the loadout a run recorded (upstream
/// `toolsAdded`, what `_restoreToolsFromTranscript` reads).
fn declaration(names: &[&str]) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: Some(
            names
                .iter()
                .map(|name| Tool {
                    name: (*name).to_owned(),
                    description: format!("{name} tool"),
                    parameters: json!({ "type": "object" }),
                    constrained_sampling: None,
                })
                .collect(),
        ),
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

/// A resumed session's transcript: one recorded loadout plus a user turn.
fn recorded_storage(names: &[&str]) -> SessionManager {
    let cwd = std::env::temp_dir().display().to_string();
    let mut storage = SessionManager::in_memory(&cwd, None).expect("in-memory session");
    storage.append_message(declaration(names)).expect("append");
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 2,
            sections: None,
        }))
        .expect("append");
    storage
}

/// The declared names a session resolves with `selection`, read from the real
/// `AgentSession` (the audit's "declared to model" column).
fn session_declared(
    registry: &ToolRegistry,
    selection: Option<pir::coding_agent::ToolSelection>,
    storage: Option<SessionManager>,
) -> Vec<String> {
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: registry.clone(),
        session: storage,
        compaction: None,
        max_turns: 1,
        // Avoid ambient discovery: this suite only reads the loadout.
        system_prompt: Some("base system prompt".into()),
        cwd: Some(std::env::temp_dir()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    });
    let (session_registry, active) = session.active_tool_snapshot(None);
    let mut declared: Vec<String> = declared_tools(&session_registry, &active)
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    declared.sort();
    declared
}

/// The names a session keeps callable through `ctx.executeTool()`.
fn session_callable(
    registry: &ToolRegistry,
    selection: Option<pir::coding_agent::ToolSelection>,
) -> Vec<String> {
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: registry.clone(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: Some("base system prompt".into()),
        cwd: Some(std::env::temp_dir()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    });
    let (session_registry, active) = session.active_tool_snapshot(None);
    let mut callable: Vec<String> = callable_tools(&session_registry, &active)
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    callable.sort();
    callable
}

// ---------------------------------------------------------------------------
// Defect 1: `--no-tools` must not swallow an explicit `--tools` list.
// ---------------------------------------------------------------------------

/// Upstream's shape: `--no-tools` empties the DEFAULT base, and the explicit
/// allowlist then selects on top of it. Before the fix the resolver returned
/// an EMPTY selection here, so `pir print --no-tools --tools read` sent no
/// tool definitions at all while the SDK builder's
/// `.no_tools().tools(["read"])` declared `read` — the same input, two
/// different request bodies.
#[test]
fn no_tools_with_an_explicit_allowlist_still_selects_the_named_tools() {
    let selection = resolve_selection(None, &names(&["read"]), &[], true, false);
    assert_eq!(
        selection.enabled,
        names(&["read"]),
        "--no-tools suppresses the default base, not an explicit --tools list"
    );
    // The explicit list is an explicit selection: a resumed session keeps it
    // instead of restoring the transcript's loadout.
    assert!(!selection.uses_default_tools);
    assert!(selection.modifiers.is_empty());
}

/// `--no-tools --tools <glob>` selects the pattern's matches, and the MCP
/// carve-out does NOT re-add `mcp__*` tools, and neither does an allowlist
/// that names other tools: upstream's `--no-tools` is a zero-length allowlist
/// and any allowlist declares only the names it names or matches
/// (`_isAllowedTool` registers, `_isActivatable`/the `_buildRuntime` allowlist
/// branch declares, agent-session.ts:1542-1558, 3600-3607), so MCP tools stay
/// out of the active set.
#[test]
fn no_tools_with_an_allowlist_pattern_keeps_the_mcp_tools_out() {
    let selection = resolve_selection(None, &names(&["ctx_*"]), &[], true, false);
    assert_eq!(selection.enabled, names(&["ctx_execute", "ctx_search"]));
    assert!(
        !selection
            .enabled
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "no MCP carve-out under --no-tools: {:?}",
        selection.enabled
    );
}

/// `--no-tools` alone still selects nothing (the plain-suppression case is
/// unchanged), and `--no-builtin-tools --tools <plugin tool>` selects it —
/// the flag suppresses the built-in base, not the explicit list.
#[test]
fn no_builtin_tools_with_an_explicit_allowlist_selects_the_named_tool() {
    assert_eq!(
        resolve_selection(None, &[], &[], true, false).enabled,
        Vec::<String>::new(),
        "--no-tools alone selects nothing"
    );
    let selection = resolve_selection(None, &names(&["ctx_execute"]), &[], false, true);
    assert_eq!(selection.enabled, names(&["ctx_execute"]));
}

/// The provider-side gates must agree with what the hosts declare: the SDK's
/// `.no_tools().tools([...])` path (`sdk/builder.rs:1469-1509`) and the
/// resolver must produce the same active set for the same input, and a
/// session built with that selection must declare exactly it.
#[test]
fn no_tools_plus_allowlist_matches_the_sdk_builder_and_the_session_loadout() {
    let registry = run_registry();
    let selection = resolve_selection(None, &names(&["read", "write"]), &[], true, false);
    let declared = session_declared(&registry, Some(selection.clone()), None);
    assert_eq!(
        declared,
        names(&["read", "write"]),
        "the session declares exactly the explicitly selected names"
    );
    // Callability is registry-wide, not selection-wide (the F12 rule): the
    // deferred plugin tool stays reachable through `ctx.executeTool` even
    // though the selection never names it. Do NOT "fix" the `read` assertion
    // below back to `!callable.contains("read")`: the declaration assertion
    // above makes `read` ACTIVE, and upstream `_getCallableTools`
    // (agent-session.ts:1567-1571) returns every `codemode`/`deferred` tool
    // PLUS every ACTIVE `direct` tool — so an active `read` is callable by
    // construction. Requiring it not to be callable would contradict the
    // declaration half of this same test (and pir's `ToolExposure::callable`,
    // `tool_execution.rs`: `Direct => active`).
    let callable = session_callable(&registry, Some(selection));
    assert!(
        callable.iter().any(|name| name == "ext_deferred"),
        "a registered deferred tool stays callable: {callable:?}"
    );
    assert!(
        callable.iter().any(|name| name == "read"),
        "a selected direct tool is active and therefore callable: {callable:?}"
    );
    assert!(
        !callable.iter().any(|name| name == "bash"),
        "an inactive direct tool is not callable: {callable:?}"
    );
}

// ---------------------------------------------------------------------------
// Defect 2: a default-derived selection carries its denylist into the resume
// restore.
// ---------------------------------------------------------------------------

/// `--exclude-tools grep` alone is a DEFAULT selection (`uses_default_tools`),
/// so a resumed session replaces it with the transcript's recorded loadout —
/// with the exclusion re-applied as a `-name` modifier. A transcript that
/// recorded the excluded tool must NOT get it back. Before the fix the
/// selection carried no `modifiers`, so `grep` came back.
#[test]
fn an_exclude_only_selection_carries_the_denylist_as_modifiers() {
    let selection = resolve_selection(None, &[], &names(&["grep"]), false, false);
    assert!(selection.uses_default_tools);
    assert_eq!(
        selection.modifiers,
        names(&["-grep"]),
        "a default-derived exclude selection must re-apply the exclusion on resume"
    );
    // A glob denylist keeps BOTH its raw pattern (it must also cover tools
    // that register later, e.g. a lazily connected MCP server) and the names
    // it expands to against the tools present at resolve time, so the restore
    // removes exactly what the user asked for.
    let selection = resolve_selection(None, &[], &names(&["ctx_*"]), false, false);
    let mut expected = names(&["-ctx_*", "-ctx_execute", "-ctx_search"]);
    expected.sort();
    let mut actual = selection.modifiers.clone();
    actual.sort();
    assert_eq!(actual, expected);
}

/// The end-to-end consequence: a resumed session whose transcript recorded the
/// excluded tool must declare the rest of the recorded loadout without it.
#[test]
fn a_resumed_exclude_only_selection_does_not_restore_the_excluded_tool() {
    let registry = run_registry();
    let available = available_names();
    let selection = resolve_selection(None, &[], &names(&["grep"]), false, false);
    // The recorded loadout names the excluded tool.
    let storage = recorded_storage(&["read", "grep", "bash"]);
    let declared = session_declared(&registry, Some(selection), Some(storage));
    assert_eq!(
        declared,
        names(&["bash", "read"]),
        "the recorded loadout minus the excluded tool"
    );
    // `grep` is in the registry (F12: the selection never caps the registry),
    // it is merely not declared.
    assert!(available.iter().any(|name| name == "grep"));
}

/// A modifier-only `--tools` list already keeps its entries; adding an
/// `--exclude-tools` list appends the exclusions after them, so the restore
/// applies both in CLI order.
#[test]
fn modifiers_and_exclusions_combine_in_order() {
    let selection = resolve_selection(
        None,
        &names(&["+codemode"]),
        &names(&["bash"]),
        false,
        false,
    );
    assert!(selection.uses_default_tools);
    assert_eq!(selection.modifiers, names(&["+codemode", "-bash"]));
}

/// An EXPLICIT selection keeps its base on resume, so no `-name` entries are
/// needed (and upstream passes none). Adding them would be inert but
/// misleading; the resolver must leave them out.
#[test]
fn an_explicit_selection_carries_no_exclusion_modifiers() {
    let selection = resolve_selection(
        None,
        &names(&["read", "bash"]),
        &names(&["bash"]),
        false,
        false,
    );
    assert!(!selection.uses_default_tools);
    assert!(
        selection.modifiers.is_empty(),
        "an explicit selection keeps its base on resume: {:?}",
        selection.modifiers
    );
    assert_eq!(selection.enabled, names(&["read"]));
}

/// `--no-tools` never restores (upstream's `usesDefaultTools` gate is false),
/// so no exclusion modifiers are carried there either.
#[test]
fn a_no_tools_selection_carries_no_exclusion_modifiers() {
    let selection = resolve_selection(None, &[], &names(&["grep"]), true, false);
    assert!(!selection.uses_default_tools);
    assert!(selection.modifiers.is_empty());
    assert!(selection.enabled.is_empty());
}

/// A settings `defaultTools` list is a default selection too: excluding one of
/// its tools must survive a resume.
#[test]
fn a_settings_selection_carries_the_denylist_as_modifiers() {
    let settings = names(&["read", "bash", "edit", "write"]);
    let selection = resolve_selection(Some(&settings), &[], &names(&["write"]), false, false);
    assert!(selection.uses_default_tools);
    assert_eq!(selection.enabled, names(&["read", "bash", "edit"]));
    assert_eq!(selection.modifiers, names(&["-write"]));
}

// ---------------------------------------------------------------------------
// Checked and correct (kept here so a later reader does not re-audit)
// ---------------------------------------------------------------------------

/// The registries the hosts build all start from the same core set, so the
/// `--no-builtin-tools` base (`CORE_TOOL_NAMES`) really is "the built-ins that
/// exist" in every build.
#[test]
fn the_core_registry_and_the_no_builtin_base_agree() {
    let mut core = core_names();
    core.sort();
    let mut constant: Vec<String> = CORE_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    constant.sort();
    assert_eq!(
        core, constant,
        "CORE_TOOL_NAMES must name exactly what register_core_tools registers"
    );
}

/// An unchanged selection still resolves to `None`, so every host keeps the
/// exposure defaults (no override, `codemode`/`deferred` callable-only).
#[test]
fn an_untouched_selection_leaves_the_exposure_defaults() {
    assert!(resolve(None, &[], &[], false, false)
        .expect("valid")
        .is_none());
}

/// The `--tools` shape errors are unchanged by the no-tools fix.
#[test]
fn the_tools_shape_errors_are_unchanged() {
    let error = resolve(None, &names(&["read", "-write"]), &[], true, false)
        .expect_err("mixed lists stay invalid");
    assert!(error.starts_with("--tools: "), "{error}");
    let error = resolve(None, &names(&["-write*"]), &[], true, false)
        .expect_err("patterns in modifier lists stay invalid");
    assert!(error.contains("not patterns"), "{error}");
}
