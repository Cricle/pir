//! W16-F: tool exposure across every host that decides what the model can
//! SEE or CALL — the W16 additions to the W14 parity suite.
//!
//! `docs/archive/tool-exposure-audit-w16.md` traces `registration → merge → selection
//! → exposure → declared → callable` for `pir print`, the interactive TUI,
//! RPC, the server, the SDK and the durable harness. This file holds ONLY the
//! tests that audit produced; `tests/suites/tool_exposure_parity_w14.rs`
//! (written by the killed W14 attempt, and kept: its `--no-tools --tools`
//! defect, resume-modifier defect and "checked and correct" cases all still
//! hold) keeps its own cases, so no test is duplicated across the two suites.
//!
//! Hosts share `coding_agent::tool_selection::resolve_tool_selection` (and its
//! registration-aware sibling, `resolve_tool_selection_with_availability`) and
//! the loadout helpers in `agent::tool_execution`, so the tests compare the two
//! CLI applications of them — `coding_agent::print::print_tool_loadout` and a
//! real `AgentSession` — against each other for the same registry and flags.
//! The D2–D5 divergence targets assert the UPSTREAM contract and are active:
//! the resolver now derives the `--exclude-tools` and `--no-builtin-tools`
//! bases from each tool's registration facts (exposure + `default_active`),
//! so no host has to guess and both hosts declare the same set.
//!
//! ## Fixed in W16-F
//!
//! 1. `--exclude-tools` alone activated `powershell`: the exclude-only base
//!    took every `CORE_TOOL_NAMES` entry instead of the core tools that
//!    activate on registration, so a denylist turned ON a tool the no-flag run
//!    leaves off (`resolve_tool_selection` →
//!    `tool_selection::core_default_active_names`).
//! 2. An active-set override repeated a name: upstream `_applyToolLoadout`
//!    dedupes with `new Set(toolNames)` (agent-session.ts:1580-1584);
//!    `ToolRegistry::active_names` did not, so a JS
//!    `setActiveTools(["read", "read"])` produced a duplicate declared entry
//!    (and a duplicate line in print's system-prompt tool list, which is built
//!    from the active names).

use std::sync::Arc;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    callable_tools, declared_tools, AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry,
};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::coding_agent::print::print_tool_loadout;
use pir::coding_agent::tool_selection::{
    builtin_plan_available_tools, extension_plan_available_tools, host_available_tools,
    resolve_tool_selection_with_availability, runtime_available_tools, AvailableTool,
    ToolRegistration, ToolSelectionRequest,
};
use pir::coding_agent::tools::{register_core_tools, CORE_TOOL_NAMES};
use pir::coding_agent::ToolSelection;
use pir::config::settings::DEFAULT_TOOL_NAMES;
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|name| (*name).to_string()).collect()
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// The core tools a no-selection run declares (registration-active only):
/// `powershell` is registered with `default_active: false`
/// (`coding_agent/tools/powershell.rs`) and is absent here even though it is in
/// [`CORE_TOOL_NAMES`].
fn core_default_active_names() -> Vec<String> {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    sorted(
        registry
            .iter()
            .filter(|tool| {
                tool.exposure()
                    .active_on_registration(tool.default_active())
            })
            .map(|tool| tool.name().to_owned())
            .collect(),
    )
}

fn resolve_with(
    available: &[AvailableTool],
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
) -> Result<Option<ToolSelection>, String> {
    resolve_tool_selection_with_availability(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        no_builtin_tools,
        available,
        &DEFAULT_TOOL_NAMES,
    )
}

fn resolve_selection(
    available: &[AvailableTool],
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
) -> ToolSelection {
    resolve_with(
        available,
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        no_builtin_tools,
    )
    .expect("a well-formed selection resolves")
    .expect("a selection was requested")
}

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
        "w16 fixture tool"
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

fn direct(name: &str) -> Arc<dyn AgentTool> {
    Arc::new(NamedTool {
        name: name.to_owned(),
        exposure: ToolExposure::Direct,
        default_active: true,
    })
}

fn inactive(name: &str, exposure: ToolExposure) -> Arc<dyn AgentTool> {
    Arc::new(NamedTool {
        name: name.to_owned(),
        exposure,
        default_active: false,
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
/// plugin tools, one per exposure class — `ctx_*`/`python`/`todo`/`subagent`
/// are `direct` and default-active, `tool_search` is `model-only` with
/// `defaultActive: false` (`plugins/builtin/tool_search.rs`), the
/// `codemode`-exposure entry is callable-but-undeclared until explicitly
/// activated (upstream `_getCallableTools`, agent-session.ts:1567-1571), and a
/// deferred MCP-style tool is callable-only.
fn run_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    for name in ["ctx_execute", "ctx_search", "python", "todo", "subagent"] {
        registry.register(direct(name));
    }
    registry.register(inactive("tool_search", ToolExposure::ModelOnly));
    registry.register(inactive("codemode", ToolExposure::Codemode));
    registry.register(inactive("ext_deferred", ToolExposure::Deferred));
    registry.register(direct("mcp__docs__search"));
    registry
}

/// The `available_tools` a host resolves against for [`run_registry`] — what
/// `print_tool_loadout` derives from the registry itself (and what the SDK
/// builder derives from its merged host registry), registration facts
/// included, so print and the session view below see the same input.
fn registry_available(registry: &ToolRegistry) -> Vec<AvailableTool> {
    let mut available = AvailableTool::from_registry(registry);
    available.sort_by(|left, right| left.name.cmp(&right.name));
    available
}

fn session(registry: &ToolRegistry, selection: Option<ToolSelection>) -> AgentSession {
    AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: registry.clone(),
        session: None,
        compaction: None,
        max_turns: 1,
        // Avoid ambient discovery: these tests only read the loadout.
        system_prompt: Some("base system prompt".into()),
        cwd: Some(std::env::temp_dir()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    })
}

fn session_declared(registry: &ToolRegistry, selection: Option<ToolSelection>) -> Vec<String> {
    let session = session(registry, selection);
    let (session_registry, active) = session.active_tool_snapshot(None);
    sorted(
        declared_tools(&session_registry, &active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect(),
    )
}

fn session_callable(registry: &ToolRegistry, selection: Option<ToolSelection>) -> Vec<String> {
    let session = session(registry, selection);
    let (session_registry, active) = session.active_tool_snapshot(None);
    sorted(
        callable_tools(&session_registry, &active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// W16-F fix 1: an exclude-only selection never activates a default-inactive
// core tool.
// ---------------------------------------------------------------------------

/// `--exclude-tools bash` must not declare `powershell`. The exclude-only base
/// used to be every `CORE_TOOL_NAMES` entry, so `powershell` (registered with
/// `default_active: false`) was turned ON by a denylist — a tool the
/// no-selection run never declares. Both hosts share the resolver, so the
/// invariant is checked on the print loadout and on a real session.
///
/// The base is the real registration pass's active set (core plus the
/// default-active plugin tools, D4), minus the excluded names: this test pins
/// the core half and the host parity for it.
#[test]
fn exclude_only_declares_the_default_core_set_minus_the_excluded_tool() {
    let registry = run_registry();
    let available = registry_available(&registry);
    let registration_active = core_default_active_names();
    assert!(
        registration_active.contains(&"read".to_owned()),
        "fixture sanity: read activates on registration"
    );

    // The default (no flags) declaration over the same registry.
    let default = print_tool_loadout(registry.clone(), None, &[], &[], false, false)
        .expect("valid selection");
    let default_declared = sorted(default.declared.clone());
    assert!(
        !default_declared.iter().any(|name| name == "powershell"),
        "the no-flag declaration leaves powershell off: {default_declared:?}"
    );

    // Exclude-only: the registration-active core set minus the excluded tool.
    let excluded = print_tool_loadout(registry.clone(), None, &[], &names(&["bash"]), false, false)
        .expect("valid selection");
    let mut expected = registration_active.clone();
    expected.retain(|name| name != "bash");
    let excluded_declared = sorted(excluded.declared.clone());
    for name in &expected {
        assert!(
            excluded_declared.iter().any(|declared| declared == name),
            "{name} is registration-active and must stay declared: {excluded_declared:?}"
        );
    }
    assert!(
        !excluded_declared.iter().any(|name| name == "bash"),
        "the excluded tool is undeclared: {excluded_declared:?}"
    );
    assert!(
        !excluded_declared.iter().any(|name| name == "powershell"),
        "powershell is default-inactive and must stay undeclared: {:?}",
        excluded_declared
    );

    // Same input, session host: the shared resolver yields the same declared
    // set (the host-parity contract the W13/W14 suites started).
    let selection = resolve_selection(&available, None, &[], &names(&["bash"]), false, false);
    assert!(selection.uses_default_tools);
    assert_eq!(
        session_declared(&registry, Some(selection)),
        excluded_declared,
        "print and the session host declare the same set for --exclude-tools"
    );
}

// ---------------------------------------------------------------------------
// W16-F fix 2: override names are deduplicated.
// ---------------------------------------------------------------------------

/// Upstream `_applyToolLoadout` dedupes the requested names with
/// `new Set(toolNames)` (agent-session.ts:1580-1584). A duplicated override
/// (a JS `setActiveTools(["read", "read"])`) used to repeat the tool in
/// `active_names`, which print feeds straight into its system-prompt tool list
/// (`print.rs`, `prompt_tools`).
#[test]
fn an_active_override_deduplicates_names_and_drops_unregistered_ones() {
    let mut registry = run_registry();
    registry.register(inactive("secret_hidden", ToolExposure::Hidden));
    let override_names = names(&["read", "read", "ghost_tool", "write", "read"]);
    assert_eq!(
        registry.active_names(Some(override_names.as_slice())),
        names(&["read", "write"]),
        "the override keeps registered names once each, in request order"
    );
    assert_eq!(
        registry.active_names(Some(names(&["secret_hidden", "read"]).as_slice())),
        names(&["read"]),
        "hidden tools can never be activated, even when named"
    );
}

// ---------------------------------------------------------------------------
// Host parity matrix: print vs a real session, same registry and same flags.
// ---------------------------------------------------------------------------

/// The contract the three fixed defects converged on: for one registry and one
/// flag set, `print_tool_loadout` (print host) and `AgentSession` (TUI, RPC,
/// SDK) must declare the SAME names and keep the SAME names callable. Covers
/// every flag form the resolver produces, including the ones the W14 fixes
/// touched (`--no-tools --tools`, modifier lists).
#[test]
fn print_and_the_session_host_agree_for_every_flag_form() {
    let registry = run_registry();
    let available = registry_available(&registry);
    let read: &[String] = &names(&["read"]);
    let plugin: &[String] = &names(&["ctx_execute"]);
    let modifier: &[String] = &names(&["+codemode"]);
    let excluded: &[String] = &names(&["bash"]);
    let settings: &[String] = &names(&["read", "ctx_execute"]);

    struct Case<'a> {
        label: &'a str,
        settings: Option<&'a [String]>,
        tools: &'a [String],
        exclude: &'a [String],
        no_tools: bool,
        no_builtin: bool,
    }

    let cases = [
        Case {
            label: "no flags",
            settings: None,
            tools: &[],
            exclude: &[],
            no_tools: false,
            no_builtin: false,
        },
        Case {
            label: "--tools read",
            settings: None,
            tools: read,
            exclude: &[],
            no_tools: false,
            no_builtin: false,
        },
        Case {
            label: "--tools ctx_execute",
            settings: None,
            tools: plugin,
            exclude: &[],
            no_tools: false,
            no_builtin: false,
        },
        Case {
            label: "--tools +codemode",
            settings: None,
            tools: modifier,
            exclude: &[],
            no_tools: false,
            no_builtin: false,
        },
        Case {
            label: "--exclude-tools bash",
            settings: None,
            tools: &[],
            exclude: excluded,
            no_tools: false,
            no_builtin: false,
        },
        Case {
            label: "--no-tools",
            settings: None,
            tools: &[],
            exclude: &[],
            no_tools: true,
            no_builtin: false,
        },
        Case {
            label: "--no-builtin-tools",
            settings: None,
            tools: &[],
            exclude: &[],
            no_tools: false,
            no_builtin: true,
        },
        Case {
            label: "--no-tools --tools read",
            settings: None,
            tools: read,
            exclude: &[],
            no_tools: true,
            no_builtin: false,
        },
        Case {
            label: "settings defaultTools",
            settings: Some(settings),
            tools: &[],
            exclude: &[],
            no_tools: false,
            no_builtin: false,
        },
    ];

    for case in &cases {
        let selection = resolve_with(
            &available,
            case.settings,
            case.tools,
            case.exclude,
            case.no_tools,
            case.no_builtin,
        )
        .unwrap_or_else(|error| panic!("{}: invalid selection: {error}", case.label));

        let print = print_tool_loadout(
            registry.clone(),
            case.settings,
            case.tools,
            case.exclude,
            case.no_tools,
            case.no_builtin,
        )
        .unwrap_or_else(|error| {
            panic!("{}: print rejected a valid selection: {error}", case.label)
        });

        assert_eq!(
            sorted(print.declared.clone()),
            session_declared(&registry, selection.clone()),
            "{}: print and the session must declare the same names",
            case.label
        );
        assert_eq!(
            sorted(print.callable.iter().cloned().collect()),
            session_callable(&registry, selection),
            "{}: print and the session must keep the same names callable",
            case.label
        );
    }
}

// ---------------------------------------------------------------------------
// The W16 divergences, now fixed (see `docs/archive/tool-exposure-audit-w16.md`). Each
// test asserts the UPSTREAM behaviour; the shared resolver receives the run
// registry's registration facts from every host that has one, so print and the
// session host resolve the same bases.
// ---------------------------------------------------------------------------

/// D2 (no-builtin over-activation). Upstream `noTools: "builtin"` activates
/// only extension tools that activate on registration
/// (`_isActivatedOnRegistration`, agent-session.ts:3608-3613 + 3632-3634);
/// `codemode`/`tool_search` (`defaultActive: false`) and deferred tools stay
/// callable-but-undeclared. `resolve_tool_selection_with_availability` bases
/// `--no-builtin-tools` on the non-core tools whose exposure is declarable and
/// whose `default_active` is set, so those tools are never declared.
#[test]
fn no_builtin_tools_declares_only_registration_active_plugin_tools() {
    let registry = run_registry();
    let available = registry_available(&registry);
    let selection = resolve_selection(&available, None, &[], &[], false, true);
    let declared = session_declared(&registry, Some(selection));
    for inactive in ["codemode", "tool_search", "ext_deferred"] {
        assert!(
            !declared.iter().any(|name| name == inactive),
            "{inactive} does not activate on registration: {declared:?}"
        );
    }
    assert!(declared.iter().any(|name| name == "ctx_execute"));
}

/// D3 (unnamed MCP tools are declared). Upstream keeps MCP tools REGISTERED
/// but never declares one that no allowlist entry names or matches
/// (`_isActivatable`, agent-session.ts:1548-1557; docs/cli.md:132: "An MCP tool
/// that no entry names or matches is never declared directly, whatever its
/// exposure"). The old MCP carve-out pushed every `mcp__*` name into
/// `enabled`, which IS the active/declared set; the carve-out is gone (the
/// F12 registry keeps those tools reachable for codemode/tool_search), so an
/// allowlist declares exactly the names it names or matches.
#[test]
fn an_allowlist_never_declares_unmatched_mcp_tools() {
    let registry = run_registry();
    let available = registry_available(&registry);
    let selection = resolve_selection(&available, None, &names(&["read"]), &[], false, false);
    assert_eq!(
        session_declared(&registry, Some(selection)),
        names(&["read"])
    );
}

/// D4 (exclude-only drops default-active plugin tools). Upstream's exclude-only
/// selection keeps every extension tool that activates on registration and
/// drops only the named ones (the `includeAllExtensionTools` branch,
/// agent-session.ts:3608-3613); the registration-aware resolver derives the
/// exclude-only base from every available tool that activates on registration,
/// so a denylist can only remove names, never un-declare an unrelated
/// default-active plugin tool.
#[test]
fn exclude_only_keeps_the_default_active_plugin_tools() {
    let registry = run_registry();
    let available = registry_available(&registry);
    let selection = resolve_selection(&available, None, &[], &names(&["bash"]), false, false);
    let declared = session_declared(&registry, Some(selection));
    assert!(
        declared.iter().any(|name| name == "ctx_execute"),
        "the default-active plugin tool survives a denylist: {declared:?}"
    );
}

/// D5 (`?` in a tool pattern). Upstream `toolPatternRegExp`
/// (mcp-servers.ts:12-24) escapes everything except `*`, so `?` is a LITERAL
/// character and matching is case-sensitive. Tool-selection pattern matching
/// now follows it (`tool_pattern_matches`): `--tools 're?d'` selects nothing
/// and a case-sensitive upstream pattern (`READ`) matches nothing.
#[test]
fn tool_patterns_match_upstream_literal_question_mark_and_case() {
    let available = registry_available(&run_registry());
    let selection = resolve_selection(&available, None, &names(&["re?d"]), &[], false, false);
    assert!(
        selection.enabled.is_empty(),
        "`?` is literal upstream: {:?}",
        selection.enabled
    );
    let selection = resolve_selection(&available, None, &names(&["READ"]), &[], false, false);
    assert!(
        selection.enabled.is_empty(),
        "tool names are case-sensitive upstream: {:?}",
        selection.enabled
    );
}

// ---------------------------------------------------------------------------
// Checked and correct (the W16 slice; the W14 suite holds the rest)
// ---------------------------------------------------------------------------

/// The exclude-only base really is the registration-active core set — derived
/// from the real registration pass, so it cannot drift when a core tool changes
/// its `default_active` or a feature-gated core tool appears/disappears. This
/// is the invariant the fix relies on, asserted without going through the
/// resolver.
#[test]
fn the_registration_active_core_set_excludes_default_inactive_tools() {
    let registration_active = core_default_active_names();
    assert!(
        !registration_active.iter().any(|name| name == "powershell"),
        "powershell registers with default_active: false: {registration_active:?}"
    );
    for expected in ["read", "bash", "edit", "write", "ls"] {
        assert!(
            registration_active.iter().any(|name| name == expected),
            "{expected} activates on registration: {registration_active:?}"
        );
    }
    for expected in ["background_status", "background_abort"] {
        assert!(
            registration_active.iter().any(|name| name == expected),
            "{expected} is a default-active core tool: {registration_active:?}"
        );
    }
    // A subset of the registry constant, never a superset.
    for name in &registration_active {
        assert!(
            CORE_TOOL_NAMES.contains(&name.as_str()),
            "{name} is not a core tool"
        );
    }
}

/// `--tools read` keeps every other tool REGISTERED (the F12 rule) and
/// callable exactly as the exposure model says: `codemode`/`deferred` always,
/// `direct` only while active. Both hosts agree.
#[test]
fn an_allowlist_keeps_the_registry_reachable_and_the_exposure_model_exact() {
    let registry = run_registry();
    let available = registry_available(&registry);
    let selection = resolve_selection(&available, None, &names(&["read"]), &[], false, false);
    let callable = session_callable(&registry, Some(selection));
    assert!(callable.iter().any(|name| name == "ext_deferred"));
    assert!(callable.iter().any(|name| name == "codemode"));
    assert!(callable.iter().any(|name| name == "read"));
    assert!(
        !callable.iter().any(|name| name == "bash"),
        "an inactive direct tool is not callable: {callable:?}"
    );
    assert!(
        !callable.iter().any(|name| name == "tool_search"),
        "model-only tools are never callable from other tools: {callable:?}"
    );
}

// ---------------------------------------------------------------------------
// D7: print applies the codemode `mode: "only"` hidden declarations.
// ---------------------------------------------------------------------------

/// A direct, default-active probe tool the codemode hook declares over.
struct HiddenProbe(&'static str);

impl AgentTool for HiddenProbe {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "d7 fixture tool"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// The real `codemode` tool registered in the given mode
/// (`plugins/codemode.rs`: `default_active: false`, exposure `model-only`).
fn codemode_tool(mode: pir::config::settings::CodemodeMode) -> Arc<dyn AgentTool> {
    use pir::plugins::plugin::{PluginRegistrations, RegistrationContext};
    use pir::plugins::{AgentPlugin, CodemodeConfig, CodemodePlugin};

    let mut registrations = PluginRegistrations::default();
    CodemodePlugin::with_config(CodemodeConfig {
        mode,
        inline_budget: pir::plugins::codemode::DEFAULT_INLINE_BUDGET,
        sandbox_memory_bytes: Some(pir::plugins::codemode::default_sandbox_memory_bytes()),
    })
    .register(&mut RegistrationContext::new(&mut registrations))
    .expect("register the codemode plugin");
    match registrations.tools[0].1.clone() {
        pir::plugins::ToolBackend::Rust(tool) => tool,
        pir::plugins::ToolBackend::Js { .. } => panic!("codemode must register a Rust tool"),
    }
}

/// D7: `pir print` with `--tools codemode` under `codemode.mode: "only"` must
/// declare the codemode surface only. The plugin's `prepareLoadout` hides the
/// declarations of active direct tools (upstream `prepareCodemodeLoadout`
/// `hiddenDeclarations`, agent-session.ts:1585-1625), and print must apply
/// that set to its provider request, not merely carry it in the registry.
/// The hidden direct tools stay active and callable — scripts reach them.
#[test]
fn print_applies_codemode_only_hidden_declarations_to_the_request() {
    use pir::config::settings::CodemodeMode;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(HiddenProbe("direct_probe")));
    registry.register(codemode_tool(CodemodeMode::Only));

    let loadout = print_tool_loadout(
        registry,
        None,
        &names(&["direct_probe", "codemode"]),
        &[],
        false,
        false,
    )
    .expect("valid selection");

    // The direct tool is active and callable — mode `"only"` hides only its
    // DECLARATION.
    assert!(
        loadout.active.iter().any(|name| name == "direct_probe"),
        "the direct tool stays active: {:?}",
        loadout.active
    );
    assert!(loadout.callable.contains("direct_probe"));
    assert!(
        loadout.hidden_declarations.contains("direct_probe"),
        "codemode mode only hides the direct declaration: {:?}",
        loadout.hidden_declarations
    );

    // The request declaration set: codemode only.
    let declared: Vec<String> = loadout
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        declared.iter().any(|name| name == "codemode"),
        "the codemode surface is declared: {declared:?}"
    );
    assert!(
        !declared.iter().any(|name| name == "direct_probe"),
        "mode only must leave the direct declaration out of the request: {declared:?}"
    );

    // The system prompt's `hiddenTools` agrees with the request.
    assert!(loadout.hidden.iter().any(|name| name == "direct_probe"));

    // Contrast: the default mode declares the direct tool too.
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(HiddenProbe("direct_probe")));
    registry.register(codemode_tool(CodemodeMode::On));
    let loadout = print_tool_loadout(
        registry,
        None,
        &names(&["direct_probe", "codemode"]),
        &[],
        false,
        false,
    )
    .expect("valid selection");
    let declared: Vec<String> = loadout
        .definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        declared.iter().any(|name| name == "direct_probe"),
        "the default mode leaves the direct declaration in place: {declared:?}"
    );
}

// ---------------------------------------------------------------------------
// D6: the context-derived system prompt lists the DECLARED set.
// ---------------------------------------------------------------------------

/// A fixture tool that carries a prompt snippet, so it joins the system
/// prompt's `<tools>` section exactly like a snippet-carrying plugin tool
/// (`ctx_execute`, `todo`, ...) does.
struct SnippetTool {
    name: String,
    snippet: String,
}

impl AgentTool for SnippetTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &'static str {
        "w16 snippet fixture tool"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
    fn prompt_snippet(&self) -> Option<String> {
        Some(self.snippet.clone())
    }
}

fn unique_temp_cwd(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-w16-d6-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp cwd");
    dir
}

/// A session that lets the context builder own the prompt (`system_prompt:
/// None`) and returns the resulting system prompt. `cwd` must contain an
/// `AGENTS.md` so the builder has a `project_context` section to emit.
fn context_prompt(
    registry: &ToolRegistry,
    selection: Option<ToolSelection>,
    cwd: &std::path::Path,
) -> String {
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: registry.clone(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    });
    session
        .system_prompt_snapshot()
        .expect("a context file must produce the context-derived prompt")
}

/// D6: the session host's system prompt is built from the DECLARED (active)
/// tool set, not the base registry. Upstream `_rebuildSystemPrompt` renders
/// `selectedTools: validToolNames` (the active ∩ registered names) plus its
/// hidden declarations (agent-session.ts:1705-1731), and the same rebuild runs
/// whenever the active set changes. Before the fix the prompt came from
/// `tool_infos(&config.tools)`, so `--tools ctx_execute` still advertised
/// every core tool and never the selected plugin tool.
#[tokio::test]
async fn the_session_system_prompt_lists_the_declared_tools_not_the_base_registry() {
    // A clean agent dir keeps this hermetic: no global context file/skills
    // beyond the cwd fixture.
    let _env = crate::common::AgentDirEnvGuard::scratch("w16-d6", None).await;
    let cwd = unique_temp_cwd("declared");
    std::fs::write(cwd.join("AGENTS.md"), "project instructions").expect("write AGENTS.md");

    let mut registry = run_registry();
    // A plugin-shaped direct tool with a prompt snippet: `merge_plugin_tools`
    // registers exactly this after the base registry, so a prompt built from
    // the base registry could never list it.
    registry.register(Arc::new(SnippetTool {
        name: "ctx_execute".to_owned(),
        snippet: "Run code in a context sandbox".to_owned(),
    }));

    // `--tools ctx_execute`: the prompt lists exactly the declared tool.
    let available = registry_available(&registry);
    let selection = resolve_selection(
        &available,
        None,
        &names(&["ctx_execute"]),
        &[],
        false,
        false,
    );
    assert_eq!(selection.enabled, names(&["ctx_execute"]));
    let prompt = context_prompt(&registry, Some(selection), &cwd);
    assert!(
        prompt.contains("- ctx_execute: Run code in a context sandbox"),
        "the selected plugin tool must be advertised: {prompt}"
    );
    for absent in ["- read:", "- bash:", "- edit:", "- write:", "- ls:"] {
        assert!(
            !prompt.contains(absent),
            "the selection leaves {absent} inactive, so it must not be advertised: {prompt}"
        );
    }
    assert!(
        !prompt.contains("Use read to examine files"),
        "the rules section follows the same declared set: {prompt}"
    );

    // The no-selection startup path still lists the registration-active
    // defaults (the same names the request declares); `powershell` is
    // registered with `default_active: false` and is never declared, so the
    // prompt never advertises it.
    let prompt = context_prompt(&registry, None, &cwd);
    assert!(
        prompt.contains("- read: Read file contents"),
        "the default startup prompt keeps its core tools: {prompt}"
    );
    assert!(
        prompt.contains("- bash: Execute bash commands"),
        "the default startup prompt keeps its core tools: {prompt}"
    );
    assert!(
        !prompt.contains("- powershell:"),
        "a default-inactive tool is never declared, so it is not advertised: {prompt}"
    );

    // `--no-tools`: nothing is declared, so the tools section is empty.
    let prompt = context_prompt(
        &registry,
        Some(ToolSelection {
            enabled: Vec::new(),
            modifiers: Vec::new(),
            uses_default_tools: false,
        }),
        &cwd,
    );
    assert!(
        prompt.contains("<tools>\n(none)\n"),
        "an empty declaration renders an empty tools section: {prompt}"
    );
    assert!(
        !prompt.contains("- read:"),
        "no tool is advertised under --no-tools: {prompt}"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

// ---------------------------------------------------------------------------
// D10: the interactive selection resolves AFTER the on-disk extensions load.
// ---------------------------------------------------------------------------

/// An on-disk extension tool. `pi.registerTool` only updates the runtime's
/// `tool_defs()` when the file is evaluated, so its name cannot appear in a
/// pre-runtime enumeration.
const D10_EXTENSION_SOURCE: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "my_js_tool",
    description: "an on-disk extension tool",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
  });
}
"#;

fn extension_runtime(cwd: &std::path::Path) -> ExtensionHandle {
    ExtensionHandle::start(ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_owned(),
        has_ui: false,
        model: Some("fake-model".to_owned()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("extension runtime")
}

fn session_with_extensions(
    registry: &ToolRegistry,
    selection: Option<ToolSelection>,
    extensions: Option<ExtensionHandle>,
    cwd: &std::path::Path,
) -> AgentSession {
    AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: registry.clone(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: Some("base system prompt".into()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions,
        tool_selection: selection,
    })
}

fn session_declared_with_extensions(
    registry: &ToolRegistry,
    selection: Option<ToolSelection>,
    extensions: Option<ExtensionHandle>,
    cwd: &std::path::Path,
) -> Vec<String> {
    let session = session_with_extensions(registry, selection, extensions, cwd);
    let (merged, active) = session.active_tool_snapshot(session.plugin_host());
    sorted(
        declared_tools(&merged, &active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect(),
    )
}

/// D10 regression: `pir -e ./my-extension.js --tools my_js_tool`.
///
/// The interactive host resolves the selection after the CLI's strict
/// extension validation loaded the on-disk extensions (the same runtime the
/// TUI host consumes) but before the session exists. Resolving against a
/// core+builtin-only `available_tools` dropped the on-disk name: a plain
/// allowlist REPLACES the base, so the empty match became an empty active
/// override and NOTHING was declared. Upstream filters the whole registry
/// after extensions load (`_refreshToolRegistry`, agent-session.ts:3525-3606,
/// reached by the constructor before session_start).
///
/// The test pins both halves: the pre-fix enumeration cannot see the name, and
/// the post-load enumeration (core + builtin plan + `runtime_available_tools`)
/// selects it and the real session declares it. `--exclude-tools` and a `*`
/// pattern are checked through the same shared resolver.
#[tokio::test]
async fn on_disk_extension_tool_is_selectable_after_the_runtime_loads() {
    let _env = crate::common::AgentDirEnvGuard::scratch("w16-d10", None).await;
    let cwd = unique_temp_cwd("d10");
    let runtime = extension_runtime(&cwd);
    let path = cwd.join("my-extension.js");
    std::fs::write(&path, D10_EXTENSION_SOURCE).expect("write extension");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("extension load");
    assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);
    assert!(
        runtime
            .tool_defs()
            .iter()
            .any(|def| def.name == "my_js_tool"),
        "fixture sanity: the on-disk tool registered"
    );

    // The pre-runtime `available_tools` the interactive host used: the base
    // registry (core + the session's other registered tools) plus the builtin
    // plan's facts. The on-disk tool is absent, so the allowlist replaces the
    // base with nothing (the defect).
    let mut pre_load = registry_available(&run_registry());
    pre_load.extend(builtin_plan_available_tools(
        &[],
        false,
        &Default::default(),
    ));
    pre_load.sort_by(|left, right| left.name.cmp(&right.name));
    pre_load.dedup_by(|left, right| left.name == right.name);
    let defect = resolve_with(&pre_load, None, &names(&["my_js_tool"]), &[], false, false)
        .expect("valid selection")
        .expect("selection requested");
    assert!(
        defect.enabled.is_empty(),
        "pre-runtime resolution cannot see an on-disk tool: {:?}",
        defect.enabled
    );

    // After the runtime load the on-disk tools' facts join the available set.
    let mut after_load = pre_load;
    after_load.extend(runtime_available_tools(&runtime));
    after_load.sort_by(|left, right| left.name.cmp(&right.name));
    after_load.dedup_by(|left, right| left.name == right.name);
    let my_js_tool = after_load
        .iter()
        .find(|tool| tool.name == "my_js_tool")
        .expect("the loaded tool is part of the available set");
    assert_eq!(
        my_js_tool.registration,
        Some(ToolRegistration {
            exposure: ToolExposure::Direct,
            default_active: true,
        }),
        "the runtime facts are the merged registry's (direct, registration-active)"
    );

    // Plain allowlist: exactly the named on-disk tool is declared.
    let selection = resolve_with(
        &after_load,
        None,
        &names(&["my_js_tool"]),
        &[],
        false,
        false,
    )
    .expect("valid selection")
    .expect("selection requested");
    assert_eq!(selection.enabled, names(&["my_js_tool"]));
    let declared = session_declared_with_extensions(
        &run_registry(),
        Some(selection),
        Some(runtime.clone()),
        &cwd,
    );
    assert_eq!(
        declared,
        names(&["my_js_tool"]),
        "the session must declare the selected on-disk tool"
    );

    // A `*` pattern selects the on-disk tool too.
    let patterned = resolve_with(&after_load, None, &names(&["my_js_*"]), &[], false, false)
        .expect("valid selection")
        .expect("selection requested");
    assert_eq!(patterned.enabled, names(&["my_js_tool"]));

    // `--exclude-tools my_js_tool` alone: the registration-active base includes
    // the loaded tool, and the denylist drops it while the default set stays.
    let excluded = resolve_with(
        &after_load,
        None,
        &[],
        &names(&["my_js_tool"]),
        false,
        false,
    )
    .expect("valid selection")
    .expect("selection requested");
    assert!(
        !excluded.enabled.iter().any(|name| name == "my_js_tool"),
        "the denylist removes the on-disk tool: {:?}",
        excluded.enabled
    );
    let declared = session_declared_with_extensions(
        &run_registry(),
        Some(excluded),
        Some(runtime.clone()),
        &cwd,
    );
    assert!(
        !declared.iter().any(|name| name == "my_js_tool"),
        "the denylist keeps the on-disk tool undeclared: {declared:?}"
    );
    assert!(
        declared.iter().any(|name| name == "read"),
        "the rest of the default base stays declared: {declared:?}"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

/// Q66: the RPC host resolves the SAME request against core facts + the final
/// extension plan's builtin facts + the loaded runtime's on-disk tools
/// (`host_available_tools`). This is the in-process proof that RPC does not
/// repeat the D10 defect: an on-disk tool named by `--tools` must be
/// selectable after the runtime loads, while an empty request stays a no-op.
#[tokio::test]
async fn rpc_host_selection_sees_an_on_disk_extension_tool() {
    let _env = crate::common::AgentDirEnvGuard::scratch("q66-rpc", None).await;
    let cwd = unique_temp_cwd("q66-rpc");
    let runtime = extension_runtime(&cwd);
    let path = cwd.join("rpc-extension.js");
    std::fs::write(&path, D10_EXTENSION_SOURCE).expect("write extension");
    let loaded = runtime
        .load(std::slice::from_ref(&path))
        .await
        .expect("extension load");
    assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);

    let plan = pir::plugins::order_extension_paths(&[], &[], false);
    let available = host_available_tools(
        &run_registry(),
        extension_plan_available_tools(&plan),
        Some(&runtime),
    );
    assert!(
        available.iter().any(|tool| tool.name == "my_js_tool"),
        "the loaded on-disk tool is part of the RPC available set"
    );
    assert!(
        available.iter().any(|tool| tool.name == "ctx_execute"),
        "the final plan's builtin facts are part of the RPC available set"
    );

    // An empty request is a no-op (the host keeps exposure defaults).
    let empty = ToolSelectionRequest::default();
    assert_eq!(empty.resolve(&available), Ok(None));

    // A plain allowlist naming the on-disk tool selects exactly it.
    let request = ToolSelectionRequest {
        cli_tools: names(&["my_js_tool"]),
        ..ToolSelectionRequest::default()
    };
    let selection = request
        .resolve(&available)
        .expect("valid selection")
        .expect("selection requested");
    assert_eq!(selection.enabled, names(&["my_js_tool"]));

    let _ = std::fs::remove_dir_all(&cwd);
}
