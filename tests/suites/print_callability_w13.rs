//! W13-Z: the print host applies the resolved tool selection the way the
//! session host does — as the ACTIVE-set override, never as a registry cap.
//!
//! The defect: `pir print` filtered its merged registry down to
//! `ToolSelection.enabled`. For `--exclude-tools bash` alone the resolver's
//! base was core-only (`CORE_TOOL_NAMES` plus `DEFAULT_TOOL_NAMES`; it is now
//! every available tool that activates on registration,
//! `tool_selection.rs:318-339`), so every plugin tool the print host merges
//! (`ctx_*`, `python`, `todo`, ...) was removed from the registry and became
//! un-callable — absent from the scheduler's lookup, from `ctx.executeTool`
//! and from `tool_search`'s search space — while the exact same selection left
//! them registered (hence callable) in a session.
//!
//! The contract asserted here is the one the two hosts share: for one registry
//! and one selection, print and a session must declare the same names, and the
//! print registry must hold every registered non-`hidden` tool. The declared
//! set is compared against a REAL [`AgentSession`]
//! (`active_tool_snapshot`), not a copy of the formula.
//!
//! Upstream note (`../pi`): print mode there is the same `AgentSession` the
//! interactive mode builds (`main.ts` passes the same `tools`/`excludeTools`/
//! `noTools` to `createAgentSessionFromServices`; `print-mode.ts` only drives
//! `session.prompt`), so the two hosts cannot diverge. Registration is gated
//! by `_isAllowedTool` (`agent-session.ts:1542-1546`, applied at
//! 3541-3598): an explicit `--tools` allowlist / `--no-tools` prunes
//! registration, while `--exclude-tools` removes only the named tools —
//! everything else, plugin tools included, stays registered and reachable.
//! pir's session host keeps the full registry and applies the selection as the
//! active override (the documented F12 rule); print now matches that host.

use std::sync::Arc;

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::coding_agent::print::print_tool_loadout;
use pir::coding_agent::tool_selection::{
    resolve_tool_selection, resolve_tool_selection_with_availability, AvailableTool,
};
use pir::coding_agent::tools::{register_core_tools, CORE_TOOL_NAMES};
use pir::coding_agent::ToolSelection;
use pir::config::settings::DEFAULT_TOOL_NAMES;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A tool with the given exposure, mirroring a merged plugin/builtin tool
/// (`ctx_*`, `todo`, `python` are `direct`; MCP servers may be configured
/// `codemode`/`deferred`; `hidden` tools are registered-but-unreachable).
struct PluginTool {
    name: &'static str,
    exposure: ToolExposure,
    default_active: bool,
}

impl AgentTool for PluginTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "print-callability fixture tool"
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

fn direct(name: &'static str) -> Arc<dyn AgentTool> {
    Arc::new(PluginTool {
        name,
        exposure: ToolExposure::Direct,
        default_active: true,
    })
}

fn deferred(name: &'static str) -> Arc<dyn AgentTool> {
    Arc::new(PluginTool {
        name,
        exposure: ToolExposure::Deferred,
        default_active: false,
    })
}

/// The registry the print host runs with after its merges: the core tools plus
/// the plugin-host tools (`merge_plugin_tools`) and an extension tool.
fn merged_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry.register(direct("ctx_execute"));
    registry.register(direct("todo"));
    registry.register(deferred("ext_deferred"));
    registry.register(Arc::new(PluginTool {
        name: "secret_hidden",
        exposure: ToolExposure::Hidden,
        default_active: true,
    }));
    registry
}

fn names(registry: &ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry
        .names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    names.sort();
    names
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn strings(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| (*entry).to_owned()).collect()
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

/// The session host's view of one registry + selection: its executable
/// registry (hidden removed) and the names it declares. Read from the real
/// `AgentSession`, not from a re-implementation of the rule.
struct SessionView {
    registered: Vec<String>,
    declared: Vec<String>,
    callable: Vec<String>,
}

fn session_view(registry: &ToolRegistry, selection: Option<ToolSelection>) -> SessionView {
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: registry.clone(),
        session: None,
        compaction: None,
        max_turns: 1,
        // Avoid ambient discovery: the test only reads the loadout.
        system_prompt: Some("base system prompt".into()),
        cwd: Some(std::env::temp_dir()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    });
    let (session_registry, active) = session.active_tool_snapshot(None);
    let declared: Vec<String> =
        pir::agent::tool_execution::declared_tools(&session_registry, &active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
    let callable: Vec<String> =
        pir::agent::tool_execution::callable_tools(&session_registry, &active)
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
    SessionView {
        registered: sorted(
            session_registry
                .names()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        ),
        declared: sorted(declared),
        callable: sorted(callable),
    }
}

fn select(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    available: &[String],
) -> Option<ToolSelection> {
    resolve_tool_selection(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        false,
        false,
        available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("valid selection")
}

fn print_loadout(
    registry: &ToolRegistry,
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    no_tools: bool,
) -> pir::coding_agent::print::PrintToolLoadout {
    print_tool_loadout(
        registry.clone(),
        settings_default_tools,
        cli_tools,
        cli_exclude,
        no_tools,
        false,
    )
    .expect("valid selection")
}

// ---------------------------------------------------------------------------
// (a) --exclude-tools bash
// ---------------------------------------------------------------------------

/// The defect case: excluding one core tool must not un-register the merged
/// plugin tools. The declared set is unchanged (it matches the session host's
/// exactly); the registry keeps every non-hidden tool, so the plugin tools
/// stay reachable from `ctx.executeTool`, `tool_search` and model-issued
/// calls.
#[tokio::test(flavor = "current_thread")]
async fn exclude_tools_bash_keeps_plugin_tools_registered_and_declared_set_matches_the_session() {
    let registry = merged_registry();
    let exclude = strings(&["bash"]);

    // Upstream's exclude-only base is the default set plus every
    // registration-active extension tool (audit D4): `ctx_execute` stays
    // declared and only the excluded `bash` is removed.
    let selection = resolve_tool_selection_with_availability(
        None,
        &[],
        &exclude,
        false,
        false,
        &AvailableTool::from_registry(&registry),
        &DEFAULT_TOOL_NAMES,
    )
    .expect("selection")
    .expect("exclude-only changes the selection");
    assert!(
        selection.enabled.iter().any(|name| name == "ctx_execute"),
        "the exclude-only base declares the registration-active plugin tool: {:?}",
        selection.enabled
    );

    let print = print_loadout(&registry, None, &[], &exclude, false);

    // Registry: everything except the hidden tool, plugin tools included.
    assert!(
        print.registry.get("ctx_execute").is_some(),
        "the print registry must keep the merged plugin tools: {:?}",
        print.registry.names()
    );
    assert!(print.registry.get("todo").is_some());
    assert!(print.registry.get("ext_deferred").is_some());
    assert!(
        print.registry.get("bash").is_some(),
        "an excluded tool stays registered and merely inactive in the session \
         host's F12 rule (print now matches it)"
    );
    assert!(
        print.registry.get("secret_hidden").is_none(),
        "hidden-exposure tools are dropped entirely"
    );

    // Declared set: exactly the session host's, with the D4 registration-active
    // plugin tools active and only the excluded core tool removed.
    let session = session_view(&registry, Some(selection));
    assert_eq!(
        sorted(print.declared.clone()),
        session.declared,
        "print must declare exactly the session host's set"
    );
    assert!(!print.declared.iter().any(|name| name == "bash"));
    assert!(print.declared.iter().any(|name| name == "ctx_execute"));

    // Callability: the session's `ctx.executeTool` set and print's agree; the
    // deferred plugin tool stays callable while merely not declared.
    assert_eq!(
        sorted(print.callable.iter().cloned().collect()),
        session.callable,
        "print's nested-callable set must equal the session's"
    );
    assert!(
        print.callable.contains("ext_deferred"),
        "a registered codemode/deferred tool is callable without being active"
    );
    assert!(!print.callable.contains("secret_hidden"));
}

// ---------------------------------------------------------------------------
// (b) --tools ctx_execute
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn tools_allowlist_declares_exactly_the_named_tool() {
    let registry = merged_registry();
    let available = names(&registry);
    let cli_tools = strings(&["ctx_execute"]);
    let selection = select(None, &cli_tools, &[], &available).expect("selection");
    assert_eq!(selection.enabled, strings(&["ctx_execute"]));

    let print = print_loadout(&registry, None, &cli_tools, &[], false);
    assert_eq!(
        sorted(print.declared.clone()),
        strings(&["ctx_execute"]),
        "the allowlist declares exactly the named tool"
    );
    let session = session_view(&registry, Some(selection));
    assert_eq!(sorted(print.declared.clone()), session.declared);

    // Non-selected tools stay registered: the allowlist is not a registry cap
    // (pir's F12 rule; the session host behaves identically).
    assert!(print.registry.get("read").is_some());
    assert!(print.registry.get("bash").is_some());
    assert_eq!(print.registry.names().len(), session.registered.len());
}

// ---------------------------------------------------------------------------
// (c) --no-builtin-tools
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn no_builtin_tools_declares_plugin_tools() {
    let registry = merged_registry();
    let selection = resolve_tool_selection_with_availability(
        None,
        &[],
        &[],
        false,
        true,
        &AvailableTool::from_registry(&registry),
        &DEFAULT_TOOL_NAMES,
    )
    .expect("valid")
    .expect("selection");
    // The active set keeps only the non-core tools that activate on
    // registration (audit D2): a deferred/default-inactive tool is callable
    // but not declared.
    assert!(selection.enabled.iter().any(|name| name == "ctx_execute"));
    assert!(selection.enabled.iter().any(|name| name == "todo"));
    assert!(!selection.enabled.iter().any(|name| name == "ext_deferred"));
    for core in CORE_TOOL_NAMES {
        assert!(
            !selection.enabled.iter().any(|name| name == core),
            "{core} must leave the active set"
        );
    }

    let print =
        print_tool_loadout(registry.clone(), None, &[], &[], false, true).expect("valid selection");
    let declared = sorted(print.declared.clone());
    assert!(declared.contains(&"ctx_execute".to_owned()));
    assert!(declared.contains(&"todo".to_owned()));
    assert!(!declared.contains(&"ext_deferred".to_owned()));
    assert!(!declared.contains(&"read".to_owned()));
    assert_eq!(
        declared,
        session_view(&registry, Some(selection)).declared,
        "print and the session host declare the same plugin tools"
    );
    // Upstream #3592: `noTools: "builtin"` keeps the built-ins REGISTERED
    // (`getAllTools()` still lists them); only the active set changes.
    assert!(print.registry.get("read").is_some());
    assert!(print.registry.get("bash").is_some());
}

// ---------------------------------------------------------------------------
// (d) --no-tools
// ---------------------------------------------------------------------------

/// Verified upstream behaviour: `noTools: "all"` sets `allowedToolNames = []`,
/// which `_isAllowedTool` turns into an EMPTY registry upstream
/// (`getAllTools()` is `[]` — `3592-no-builtin-tools-keeps-extension-tools`
/// asserts it). pir's session host deliberately keeps the registry and clears
/// the active set (the documented F12 rule), and print now matches the session
/// host — the declared set is the contract, and it is empty either way.
#[tokio::test(flavor = "current_thread")]
async fn no_tools_declares_nothing_but_keeps_the_session_registry() {
    let registry = merged_registry();
    let print =
        print_tool_loadout(registry.clone(), None, &[], &[], true, false).expect("valid selection");
    assert!(
        print.declared.is_empty(),
        "nothing is declared: {:?}",
        print.declared
    );
    assert!(print.active.is_empty());
    // Session parity: the same registry (hidden removed) and an empty active
    // set. Direct tools are therefore not callable either (callability needs
    // activity); codemode/deferred ones remain registry-callable.
    let session = session_view(
        &registry,
        Some(ToolSelection {
            enabled: Vec::new(),
            modifiers: Vec::new(),
            uses_default_tools: false,
        }),
    );
    assert_eq!(session.declared, Vec::<String>::new());
    assert_eq!(sorted(names(&print.registry)), session.registered);
    assert_eq!(
        sorted(print.callable.iter().cloned().collect()),
        session.callable
    );
    assert!(print.registry.get("read").is_some());
    assert!(print.registry.get("ctx_execute").is_some());
}

// ---------------------------------------------------------------------------
// settings `defaultTools`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn settings_default_tools_declares_the_same_set_as_the_session() {
    let registry = merged_registry();
    let available = names(&registry);
    let settings = strings(&["read", "ctx_execute"]);
    let selection = select(Some(&settings), &[], &[], &available).expect("selection");
    let print = print_loadout(&registry, Some(&settings), &[], &[], false);
    assert_eq!(
        sorted(print.declared.clone()),
        strings(&["ctx_execute", "read"])
    );
    assert_eq!(
        sorted(print.declared.clone()),
        session_view(&registry, Some(selection)).declared
    );
    // Unselected plugin and core tools remain registered (callable through the
    // scheduler / a codemode activation), never declared.
    assert!(print.registry.get("bash").is_some());
    assert!(print.registry.get("todo").is_some());
}

// ---------------------------------------------------------------------------
// No selection: unchanged behaviour
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn no_selection_declares_the_registration_active_set() {
    let registry = merged_registry();
    let print = print_loadout(&registry, None, &[], &[], false);
    let declared = sorted(print.declared.clone());
    // Every default-active direct tool (core + plugin), not the deferred or
    // hidden ones.
    for expected in ["read", "bash", "edit", "write", "ctx_execute", "todo"] {
        assert!(
            declared.contains(&expected.to_owned()),
            "{expected} should be declared by default: {declared:?}"
        );
    }
    assert!(!declared.contains(&"ext_deferred".to_owned()));
    assert!(!declared.contains(&"secret_hidden".to_owned()));
    assert_eq!(
        declared,
        session_view(&registry, None).declared,
        "no-selection parity holds too"
    );
}
