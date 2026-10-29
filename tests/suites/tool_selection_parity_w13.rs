//! W13-P: the interactive host's tool selection must see the plugin tools.
//!
//! `resolve_interactive_tool_selection` (the TUI's `--tools` /
//! `--exclude-tools` / `--no-builtin-tools` / `defaultTools` resolution) runs
//! BEFORE any extension runtime exists: `run_mode` only has the CLI flags and
//! the settings there, while the TUI worker loads the runtime and builds the
//! `AgentSession` (whose plugin host merges the builtin/JS tools into the run
//! registry) later. It used to build its `available_tools` from a core-only
//! registry, and `resolve_tool_selection` matches allowlist/exclude entries
//! against exactly that list — so every plugin tool was silently
//! unselectable. `--tools ctx_execute` matched nothing and left an EMPTY
//! active override (the model got no tools at all), and `--no-builtin-tools`
//! (whose contract is "keep extension/custom tools") kept nothing, because
//! every name it could see was a core name.
//!
//! The fix feeds the resolver the same names the plugin host will register:
//! `tool_selection::builtin_plan_tool_names` runs the builtin factories' own
//! registration pass (the pass `PluginHost::build` runs) for the extension
//! plan the CLI flags select. These tests pin:
//!
//! 1. the enumeration is the host's own set — no hand-maintained name table
//!    that can drift when a builtin adds or renames a tool — AND carries each
//!    tool's registration facts (`exposure`, `defaultActive`) from that same
//!    `ToolDef`, compared name-by-name against the host registry;
//! 2. a plugin tool named in `--tools` resolves (plain name and glob), which
//!    is what the core-only list broke;
//! 3. `--no-builtin-tools` keeps only the registration-active plugin tools and
//!    drops the core ones — `codemode`, `tool_search`, the codemode model
//!    bridges and deferred MCP resource tools stay off;
//! 4. `--exclude-tools` alone keeps the default-active plugin tools
//!    (`ctx_execute`, `todo`, ...) instead of silently un-declaring them, and
//!    drops exactly the name it lists;
//! 5. flag semantics are unchanged: `--no-extensions`, `--no-mcp`,
//!    `pythonTool: false`, `--no-tools`, unknown names, the literal `?` /
//!    case-sensitive patterns and the `--tools` shape errors all behave
//!    exactly as before.

use std::path::PathBuf;
use std::sync::Arc;

use pir::agent::tool_execution::{ToolExposure, ToolRegistry};
use pir::coding_agent::tool_selection::{
    builtin_plan_available_tools, builtin_plan_tool_names, plugin_tool_names,
    resolve_tool_selection, resolve_tool_selection_with_availability, tool_selection_requested,
    AvailableTool,
};
use pir::coding_agent::tools::{register_core_tools, CORE_TOOL_NAMES};
use pir::config::settings::DEFAULT_TOOL_NAMES;
use pir::extensions::ExtensionConfig;
use pir::plugins::PluginHost;

#[cfg(feature = "extensions")]
const CODEMODE: &str = pir::plugins::builtin::CODEMODE_TOOL_NAME;
#[cfg(feature = "extensions")]
const CODEMODE_BRIDGES: [&str; 2] = [
    pir::plugins::codemode::models_bridge::GENERATE_IMAGES_TOOL,
    pir::plugins::codemode::models_bridge::CLASSIFY_TOOL,
];
#[cfg(feature = "context-mode")]
const CTX_EXECUTE: &str = pir::plugins::builtin::CTX_EXECUTE_TOOL_NAME;
#[cfg(feature = "context-mode")]
const CTX_SEARCH: &str = pir::plugins::builtin::CTX_SEARCH_TOOL_NAME;
#[cfg(feature = "python")]
const PYTHON: &str = pir::plugins::builtin::PYTHON_TOOL_NAME;
#[cfg(feature = "todo")]
const TODO: &str = pir::plugins::builtin::TODO_TOOL_NAME;
#[cfg(feature = "subagent")]
const SUBAGENT: &str = "subagent";
#[cfg(feature = "tool-search")]
const TOOL_SEARCH: &str = pir::plugins::builtin::TOOL_SEARCH_TOOL_NAME;

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|name| (*name).to_string()).collect()
}

/// The core registration as a [`ToolRegistry`] (the interactive host's base).
fn core_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

/// The core names a host always has (the same list the interactive host
/// starts from).
fn core_names() -> Vec<String> {
    core_registry()
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect()
}

/// The core tools WITH their registration facts (`AvailableTool::from_registry`,
/// what `resolve_interactive_tool_selection` starts from).
fn core_available() -> Vec<AvailableTool> {
    AvailableTool::from_registry(&core_registry())
}

/// The `available_tools` the interactive host resolves against: core tools
/// plus the builtin plugin tools of its extension plan, with each tool's
/// registration facts. This mirrors the two lines
/// `resolve_interactive_tool_selection` runs (that function lives in the
/// binary crate and is not reachable from an integration test).
fn interactive_available(cli_paths: &[PathBuf], no_extensions: bool) -> Vec<AvailableTool> {
    interactive_available_with(cli_paths, no_extensions, &Default::default())
}

/// [`interactive_available`] with the composed `disabledBuiltinExtensions` /
/// `pythonTool: false` / `--no-mcp` set (`disabled_from_flags(no_mcp)` unioned
/// through `SettingsManager::disabled_builtins_with_flags` in the real host).
fn interactive_available_with(
    cli_paths: &[PathBuf],
    no_extensions: bool,
    disabled_builtins: &std::collections::BTreeSet<String>,
) -> Vec<AvailableTool> {
    let mut available = core_available();
    available.extend(builtin_plan_available_tools(
        cli_paths,
        no_extensions,
        disabled_builtins,
    ));
    // Same sort/dedupe-by-name as the host, core entries first, so a plugin
    // tool shadowing a core name keeps the core facts (first-name-wins).
    available.sort_by(|left, right| left.name.cmp(&right.name));
    available.dedup_by(|left, right| left.name == right.name);
    available
}

fn available_names(tools: &[AvailableTool]) -> Vec<String> {
    tools.iter().map(|tool| tool.name.clone()).collect()
}

fn resolve(
    cli_tools: &[String],
    exclude: &[String],
    no_tools: bool,
    no_builtin_tools: bool,
    available: &[AvailableTool],
) -> Vec<String> {
    resolve_tool_selection_with_availability(
        None,
        cli_tools,
        exclude,
        no_tools,
        no_builtin_tools,
        available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("a well-formed selection resolves")
    .expect("a selection was requested")
    .enabled
}

// ---------------------------------------------------------------------------
// 1. The enumeration IS the plugin host's own set.
// ---------------------------------------------------------------------------

/// A self-cleaning temp directory (the host's cwd, so the `mcp` builtin's
/// config scan never reads this repository's `.pi/mcp.json`).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pir-tool-selection-w13-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_plan_tool_names_equals_the_plugin_host_registry() {
    let _env = crate::common::AgentDirEnvGuard::scratch("tool-selection-w13", None).await;
    let dir = TempDir::new("host");
    let plan = pir::plugins::order_extension_paths(&[], &[], false);
    if plan.is_empty() {
        // A minimal build compiles no builtin factories.
        return;
    }
    let handle = pir::extensions::ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "print".to_owned(),
        has_ui: false,
        ..Default::default()
    });
    handle.set_path_plan(plan);
    let host = Arc::new(PluginHost::build(Some(handle.clone()), Vec::new()));

    // The registry `merge_plugin_tools` reads: every builtin-registered tool,
    // hidden exposure included (the print host's `available_tools` includes
    // them too — it removes hidden tools only after resolving).
    let registry = host.registry();
    let mut host_names: Vec<String> = registry
        .resolved_tools()
        .into_iter()
        .map(|(definition, _)| definition.name.clone())
        .collect();
    host_names.sort();
    host_names.dedup();

    let mut enumerated = builtin_plan_tool_names(&[], false, &Default::default());
    enumerated.sort();
    enumerated.dedup();

    assert_eq!(
        enumerated, host_names,
        "the pre-runtime enumeration must be exactly the plugin host's tool set \
         (a hand-maintained name table would drift here)"
    );
    assert!(
        !enumerated
            .iter()
            .any(|name| CORE_TOOL_NAMES.contains(&name.as_str())),
        "plugin tools and core tools share no name: {enumerated:?}"
    );

    // The pre-runtime entries must also carry the SAME registration facts the
    // host's `ToolDef`s do — the exposure (with the `direct` fallback the
    // merged registry applies to a JS-backed tool) and `defaultActive`. These
    // facts, not the names, decide the exclude-only and `--no-builtin-tools`
    // bases.
    let host_facts: std::collections::BTreeMap<String, (ToolExposure, bool)> = registry
        .resolved_tools()
        .into_iter()
        .map(|(definition, _)| {
            (
                definition.name.clone(),
                (
                    ToolExposure::parse(&definition.exposure).unwrap_or(ToolExposure::Direct),
                    definition.default_active,
                ),
            )
        })
        .collect();
    let plan_tools = builtin_plan_available_tools(&[], false, &Default::default());
    assert_eq!(
        plan_tools.len(),
        enumerated.len(),
        "one available entry per enumerated name: {plan_tools:?}"
    );
    for tool in &plan_tools {
        let host = host_facts.get(&tool.name).unwrap_or_else(|| {
            panic!(
                "`{}` is enumerated but the host registry does not hold it",
                tool.name
            )
        });
        let registration = tool
            .registration
            .expect("builtin plan entries carry registration facts");
        assert_eq!(
            registration.exposure, host.0,
            "`{}` exposure must match the host ToolDef's",
            tool.name
        );
        assert_eq!(
            registration.default_active, host.1,
            "`{}` defaultActive must match the host ToolDef's",
            tool.name
        );
    }

    // Spot-checks on the facts Z5 is about, from both sides of the split.
    let core = core_available();
    assert_eq!(
        registration_of(&core, "powershell"),
        Some((ToolExposure::Direct, false)),
        "powershell registers direct but is NOT default-active"
    );
    #[cfg(feature = "context-mode")]
    assert_eq!(
        registration_of(&plan_tools, CTX_EXECUTE),
        Some((ToolExposure::Direct, true)),
        "ctx_execute registers direct and default-active"
    );
    #[cfg(feature = "extensions")]
    {
        assert_eq!(
            registration_of(&plan_tools, CODEMODE),
            Some((ToolExposure::ModelOnly, false)),
            "codemode is model-only and not default-active"
        );
        for bridge in CODEMODE_BRIDGES {
            // `defaultActive` is left at the trait default (true) — the
            // Codemode exposure alone keeps the bridge out of every
            // registration base (`active_on_registration` requires a
            // declarable exposure).
            assert_eq!(
                registration_of(&plan_tools, bridge),
                Some((ToolExposure::Codemode, true)),
                "the codemode models bridge `{bridge}` is codemode-exposure"
            );
        }
    }
    #[cfg(not(feature = "extensions"))]
    assert!(
        plan_tools.iter().all(|tool| tool.name != "codemode"),
        "a build without `extensions` has no codemode builtin"
    );
    #[cfg(feature = "tool-search")]
    assert_eq!(
        registration_of(&plan_tools, TOOL_SEARCH),
        Some((ToolExposure::ModelOnly, false)),
        "tool_search is model-only and not default-active"
    );
    #[cfg(not(feature = "tool-search"))]
    assert!(
        plan_tools.iter().all(|tool| tool.name != "tool_search"),
        "a build without `tool-search` has no tool_search builtin"
    );
}

/// One available entry's registration facts, or `None` when the set does not
/// hold that name.
fn registration_of(tools: &[AvailableTool], name: &str) -> Option<(ToolExposure, bool)> {
    tools.iter().find(|tool| tool.name == name).map(|tool| {
        let registration = tool
            .registration
            .expect("a host-built available entry carries facts");
        (registration.exposure, registration.default_active)
    })
}

#[test]
fn default_plan_offers_every_enabled_builtin_tool() {
    let enumerated = builtin_plan_tool_names(&[], false, &Default::default());
    // A minimal build compiles none of the gated builtins, so the `mut` and
    // the pushes below disappear.
    #[allow(unused_mut)]
    let mut expected: Vec<&str> = Vec::new();
    #[cfg(feature = "context-mode")]
    expected.extend([CTX_EXECUTE, CTX_SEARCH]);
    #[cfg(feature = "python")]
    expected.push(PYTHON);
    #[cfg(feature = "todo")]
    expected.push(TODO);
    #[cfg(feature = "subagent")]
    expected.push(SUBAGENT);
    #[cfg(feature = "tool-search")]
    expected.push(TOOL_SEARCH);
    let missing: Vec<&str> = expected
        .into_iter()
        .filter(|name| !enumerated.iter().any(|entry| entry == name))
        .collect();
    assert!(
        missing.is_empty(),
        "builtin tools missing from the selectable set: {missing:?} of {enumerated:?}"
    );
}

#[test]
fn unknown_builtin_names_contribute_nothing() {
    assert!(plugin_tool_names(["no-such-builtin"]).is_empty());
    // Compiled-out builtins resolve through the same `None` path.
    assert!(plugin_tool_names(["definitely-not-compiled"]).is_empty());
}

// ---------------------------------------------------------------------------
// 2. The regression: a plugin tool named in `--tools` must resolve.
// ---------------------------------------------------------------------------

#[cfg(feature = "context-mode")]
#[test]
fn allowlist_selects_a_builtin_plugin_tool() {
    let available = interactive_available(&[], false);
    assert_eq!(
        resolve(&names(&[CTX_EXECUTE]), &[], false, false, &available),
        names(&[CTX_EXECUTE]),
        "`--tools ctx_execute` must select the context-mode tool"
    );

    // The core-only list the interactive host used before this fix: the name
    // matched nothing, so the allowlist collapsed to an empty active set and
    // the model saw no tools at all.
    let core_only = core_available();
    assert!(
        resolve(&names(&[CTX_EXECUTE]), &[], false, false, &core_only).is_empty(),
        "the core-only available set is what made the tool unselectable"
    );
}

#[cfg(feature = "context-mode")]
#[test]
fn glob_allowlist_expands_over_builtin_plugin_tools() {
    let available = interactive_available(&[], false);
    let enabled = resolve(&names(&["ctx_*"]), &[], false, false, &available);
    assert!(
        enabled.iter().any(|name| name == CTX_EXECUTE),
        "`--tools 'ctx_*'` must expand over the plugin tools: {enabled:?}"
    );
    assert!(
        enabled.iter().any(|name| name == CTX_SEARCH),
        "`--tools 'ctx_*'` must expand over the plugin tools: {enabled:?}"
    );
    assert!(
        enabled.iter().all(|name| name.starts_with("ctx_")),
        "the glob must not pull in unrelated names: {enabled:?}"
    );
}

#[cfg(feature = "context-mode")]
#[test]
fn allowlist_and_exclude_compose_over_builtin_plugin_tools() {
    let available = interactive_available(&[], false);
    let enabled = resolve(
        &names(&["read", CTX_EXECUTE, CTX_SEARCH]),
        &names(&[CTX_SEARCH]),
        false,
        false,
        &available,
    );
    assert_eq!(
        enabled,
        names(&["read", CTX_EXECUTE]),
        "the exclude entry must match the plugin tool it names"
    );
}

/// Exclude-only (no allowlist) base: every tool that activates on
/// registration, so a denylist removes a name without ever ACTIVATING one.
#[cfg(feature = "context-mode")]
#[test]
fn exclude_only_drops_exactly_the_named_tool() {
    let available = interactive_available(&[], false);
    // A denylist entry matching nothing yields the exclude-only base itself.
    let base = resolve(&[], &names(&["ghost_tool"]), false, false, &available);
    let enabled = resolve(&[], &names(&[CTX_EXECUTE]), false, false, &available);

    assert!(
        base.iter().any(|name| name == CTX_EXECUTE),
        "the exclude-only base keeps the default-active plugin tools: {base:?}"
    );
    assert!(
        base.iter().any(|name| name == CTX_SEARCH),
        "the exclude-only base keeps the default-active plugin tools: {base:?}"
    );
    let mut expected = base.clone();
    expected.retain(|name| name != CTX_EXECUTE);
    assert_eq!(
        enabled, expected,
        "`--exclude-tools {CTX_EXECUTE}` must drop exactly that name and keep \
         every other base entry (there is no allowlist to narrow it)"
    );

    // The base is `_isActivatedOnRegistration`, so a denylist never activates
    // a tool the default run leaves off or a non-declarable one.
    assert!(
        !base.iter().any(|name| name == "powershell"),
        "`powershell` is registered with `defaultActive: false` and must not be \
         activated by an exclude-only selection: {base:?}"
    );
    #[cfg(feature = "extensions")]
    assert!(
        !base.iter().any(|name| name == CODEMODE),
        "`codemode` registers inactive and must not be activated by a denylist: \
         {base:?}"
    );
    #[cfg(feature = "tool-search")]
    assert!(
        !base.iter().any(|name| name == TOOL_SEARCH),
        "`tool_search` registers inactive and must not be activated by a \
         denylist: {base:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. `--no-builtin-tools` keeps extension/custom tools.
// ---------------------------------------------------------------------------

#[cfg(feature = "context-mode")]
#[test]
fn no_builtin_tools_keeps_plugin_tools_and_drops_core_tools() {
    let available = interactive_available(&[], false);
    let enabled = resolve(&[], &[], false, true, &available);

    assert!(
        enabled.iter().any(|name| name == CTX_EXECUTE),
        "`--no-builtin-tools` keeps extension tools: {enabled:?}"
    );
    assert!(
        enabled.iter().any(|name| name == CTX_SEARCH),
        "`--no-builtin-tools` keeps extension tools: {enabled:?}"
    );
    assert!(
        !enabled
            .iter()
            .any(|name| CORE_TOOL_NAMES.contains(&name.as_str())),
        "`--no-builtin-tools` drops the core tools: {enabled:?}"
    );

    // The base is `_isActivatedOnRegistration`: a tool that registers with a
    // non-declarable exposure or `defaultActive: false` stays registered and
    // callable but UNDECLARED. `codemode` and `tool_search` are both
    // model-only + inactive, and the codemode models bridges are
    // codemode-exposure, so none of them may be selected here.
    #[cfg(feature = "extensions")]
    {
        assert!(
            available.iter().any(|tool| tool.name == CODEMODE),
            "the default plan registers codemode (its absence would make the \
             assertion below vacuous): {available:?}"
        );
        assert!(
            !enabled.iter().any(|name| name == CODEMODE),
            "`--no-builtin-tools` must not declare codemode: {enabled:?}"
        );
        for bridge in CODEMODE_BRIDGES {
            assert!(
                available.iter().any(|tool| tool.name == bridge),
                "the codemode plan registers the models bridge `{bridge}`: \
                 {available:?}"
            );
            assert!(
                !enabled.iter().any(|name| name == bridge),
                "`--no-builtin-tools` must not declare the codemode models \
                 bridge `{bridge}`: {enabled:?}"
            );
        }
    }
    #[cfg(not(feature = "extensions"))]
    assert!(
        available.iter().all(|tool| tool.name != "codemode"),
        "a build without `extensions` registers no codemode tool"
    );

    #[cfg(feature = "tool-search")]
    {
        assert!(
            available.iter().any(|tool| tool.name == TOOL_SEARCH),
            "the default plan registers tool_search: {available:?}"
        );
        assert!(
            !enabled.iter().any(|name| name == TOOL_SEARCH),
            "`--no-builtin-tools` must not declare tool_search: {enabled:?}"
        );
    }
    #[cfg(not(feature = "tool-search"))]
    assert!(
        available.iter().all(|tool| tool.name != "tool_search"),
        "a build without `tool-search` registers no tool_search tool"
    );

    // The aggregate MCP resource tools register `hidden` before any server
    // connects, so the registration base leaves them undeclared too.
    #[cfg(feature = "mcp")]
    for name in pir::mcp::registry::MCP_RESOURCE_TOOLS {
        assert!(
            available.iter().any(|tool| tool.name == *name),
            "the default plan registers the MCP resource tool `{name}` (its \
             absence would make the assertion below vacuous): {available:?}"
        );
        assert!(
            !enabled.iter().any(|selected| selected == name),
            "`--no-builtin-tools` must not declare the deferred MCP resource \
             tool `{name}`: {enabled:?}"
        );
    }

    // Same flag, core-only available set: nothing survived at all, which is
    // the second symptom of the same core-only list.
    assert!(
        resolve(&[], &[], false, true, &core_available()).is_empty(),
        "the core-only available set kept nothing under --no-builtin-tools"
    );
}

// ---------------------------------------------------------------------------
// 4. Flag semantics stay exactly as they were.
// ---------------------------------------------------------------------------

/// Upstream `toolPatternRegExp` (mcp-servers.ts:12-24): only `*` is a
/// wildcard, `?` is literal, matching is case-sensitive.
#[test]
fn patterns_are_star_only_and_case_sensitive() {
    let available = interactive_available(&[], false);
    // Sanity: the same shape resolves when it is the plain name, so the empty
    // results below are the matcher's doing, not a missing tool.
    assert_eq!(
        resolve(&names(&["read"]), &[], false, false, &available),
        names(&["read"]),
        "the baseline allowlist resolves"
    );

    // `?` is a literal name character, not a one-character wildcard: `r?ad`
    // matches neither `read` (nor any spelling of it).
    assert!(
        resolve(&names(&["r?ad"]), &[], false, false, &available).is_empty(),
        "`?` is literal in tool patterns, so `r?ad` selects nothing"
    );

    // `*` matches any run, including none. (`rea*d` rather than `r*` because
    // the default plan also holds `read_mcp_resource`, which `r*` matches.)
    assert_eq!(
        resolve(&names(&["rea*d"]), &[], false, false, &available),
        names(&["read"]),
        "`*` matches any run, so `rea*d` selects `read` and only `read`"
    );
    assert!(
        resolve(&names(&["r*"]), &[], false, false, &available)
            .iter()
            .any(|name| name == "read"),
        "`*` matches any run, so `r*` selects `read`"
    );

    // Matching is case-sensitive.
    assert!(
        resolve(&names(&["READ"]), &[], false, false, &available).is_empty(),
        "tool patterns are case-sensitive, so `READ` selects nothing"
    );
    // ... and so is an exact allowlist entry: only `*` makes it a pattern, and
    // a non-pattern entry compares with `==`.
    assert!(
        resolve(&names(&["Read"]), &[], false, false, &available).is_empty(),
        "an exact allowlist entry is case-sensitive too"
    );
}

#[test]
fn unknown_names_are_still_dropped_silently() {
    let available = interactive_available(&[], false);
    let enabled = resolve(&names(&["ghost_tool"]), &[], false, false, &available);
    assert!(
        enabled.is_empty(),
        "a genuinely unknown name is not selectable: {enabled:?}"
    );
}

#[test]
fn tool_list_shape_errors_keep_their_text() {
    let available = interactive_available(&[], false);
    let error = resolve_tool_selection(
        None,
        &names(&["read", "-write"]),
        &[],
        false,
        false,
        &available_names(&available),
        &DEFAULT_TOOL_NAMES,
    )
    .expect_err("mixed forms stay invalid");
    assert_eq!(
        error, "--tools: tool names cannot be mixed with +name or -name entries",
        "the CLI-shaped validation error text is unchanged"
    );

    let error = resolve_tool_selection(
        None,
        &names(&["-write*"]),
        &[],
        false,
        false,
        &available_names(&available),
        &DEFAULT_TOOL_NAMES,
    )
    .expect_err("patterns in modifier lists stay invalid");
    assert!(
        error.starts_with("--tools: ") && error.contains("not patterns"),
        "the CLI-shaped pattern error text is unchanged: {error}"
    );
}

#[test]
fn no_tools_disables_every_name_known_or_plugin() {
    let available = interactive_available(&[], false);
    let enabled = resolve(&[], &[], true, false, &available);
    assert!(
        enabled.is_empty(),
        "--no-tools selects nothing: {enabled:?}"
    );
}

#[test]
fn no_flags_resolve_to_no_selection() {
    let available = interactive_available(&[], false);
    assert!(
        resolve_tool_selection(
            None,
            &[],
            &[],
            false,
            false,
            &available_names(&available),
            &DEFAULT_TOOL_NAMES
        )
        .expect("valid")
        .is_none(),
        "an untouched selection leaves the exposure defaults alone"
    );
}

#[test]
fn the_request_gate_matches_the_resolver_unchanged_return() {
    // The interactive host skips the (non-free) plugin enumeration behind
    // this gate, so it must agree exactly with the resolver's own
    // unchanged-selection return for every input shape.
    let available = interactive_available(&[], false);
    let read_selection = names(&["read"]);
    let mine: &[String] = &read_selection;
    let none: &[String] = &[];
    type Case<'a> = (&'a [String], &'a [String], bool, bool, Option<&'a [String]>);
    let cases: [Case<'_>; 6] = [
        (none, none, false, false, None),
        (mine, none, false, false, None),
        (none, mine, false, false, None),
        (none, none, true, false, None),
        (none, none, false, true, None),
        (none, none, false, false, Some(mine)),
    ];
    for (cli_tools, exclude, no_tools, no_builtin_tools, settings) in cases {
        let gate =
            tool_selection_requested(settings, cli_tools, exclude, no_tools, no_builtin_tools);
        let resolved = resolve_tool_selection(
            settings,
            cli_tools,
            exclude,
            no_tools,
            no_builtin_tools,
            &available_names(&available),
            &DEFAULT_TOOL_NAMES,
        )
        .expect("valid")
        .is_some();
        assert_eq!(
            gate, resolved,
            "gate and resolver disagree for {cli_tools:?}/{exclude:?}/\
             {no_tools}/{no_builtin_tools}/{settings:?}"
        );
    }
}

#[test]
fn no_extensions_drops_the_default_builtin_names() {
    // `--no-extensions` keeps only explicit `-e` paths, so no default builtin
    // is on the plan and none of its tools is selectable.
    let available = interactive_available(&[], true);
    assert_eq!(
        available_names(&available),
        {
            let mut core = core_names();
            core.sort();
            core.dedup();
            core
        },
        "`--no-extensions` without `-e` adds no plugin tool"
    );

    // An explicit `-e builtin:<name>` still loads, with or without
    // `--no-extensions`.
    #[cfg(feature = "python")]
    {
        let explicit = vec![PathBuf::from("builtin:python")];
        let available = interactive_available(&explicit, true);
        assert!(
            available.iter().any(|tool| tool.name == PYTHON),
            "an explicit builtin path stays selectable: {available:?}"
        );
        assert_eq!(
            resolve(&names(&[PYTHON]), &[], false, false, &available),
            names(&[PYTHON]),
            "`--no-extensions -e builtin:python --tools python` selects it"
        );
    }
}

#[cfg(feature = "mcp")]
#[test]
fn no_mcp_drops_the_aggregate_mcp_resource_tools() {
    // The exact flag composition `cli_preprocess` builds: `--no-mcp` is the
    // `disabledBuiltinExtensions: ["mcp"]` pattern.
    let disabled: std::collections::BTreeSet<String> = pir::plugins::disabled_from_flags(true)
        .into_iter()
        .collect();
    let with_mcp = builtin_plan_tool_names(&[], false, &Default::default());
    let without_mcp = builtin_plan_tool_names(&[], false, &disabled);
    let resource_tools = pir::mcp::registry::MCP_RESOURCE_TOOLS;
    // Server tools (`mcp__<server>__<tool>`) exist only after discovery, so
    // the aggregate resource tools are the mcp builtin's static tool names.
    assert!(
        resource_tools
            .iter()
            .any(|name| with_mcp.iter().any(|entry| entry == name)),
        "a default plan registers the MCP resource tools: {with_mcp:?}"
    );
    assert!(
        resource_tools
            .iter()
            .all(|name| !without_mcp.iter().any(|entry| entry == name)),
        "`--no-mcp` (`disabledBuiltinExtensions: [\"mcp\"]`) keeps them off the \
         plan: {without_mcp:?}"
    );
}

#[cfg(feature = "python")]
#[test]
fn python_tool_false_drops_python() {
    let enabled = builtin_plan_tool_names(&[], false, &Default::default());
    assert!(enabled.iter().any(|name| name == PYTHON));
    let disabled: std::collections::BTreeSet<String> = std::iter::once(PYTHON.to_owned()).collect();
    let without_python = builtin_plan_tool_names(&[], false, &disabled);
    assert!(
        without_python.iter().all(|name| name != PYTHON),
        "`pythonTool: false` (folded into the disabled set) drops python from \
         the selectable names: {without_python:?}"
    );
}

#[cfg(feature = "context-mode")]
#[test]
fn a_settings_disabled_builtin_is_not_selectable() {
    // `disabledBuiltinExtensions: ["context-mode"]` removes the builtin from
    // the plan, so its tools were never registered and naming one must not
    // conjure an active entry that the run registry cannot serve — exactly
    // the behavior `--exclude-tools`/settings already had for core names.
    let disabled: std::collections::BTreeSet<String> =
        std::iter::once("context-mode".to_owned()).collect();
    let available = interactive_available_with(&[], false, &disabled);
    assert!(
        !available.iter().any(|tool| tool.name == CTX_EXECUTE),
        "a disabled builtin contributes no selectable name: {available:?}"
    );
    assert!(
        resolve(&names(&[CTX_EXECUTE]), &[], false, false, &available).is_empty(),
        "naming a tool of a disabled builtin selects nothing"
    );
}

#[test]
fn builtin_tool_names_are_not_core_names() {
    // `--no-builtin-tools` classifies by the CORE set, so a builtin plugin
    // tool must never be listed there.
    let registry = {
        let mut registry = ToolRegistry::new();
        register_core_tools(&mut registry);
        registry.remove_exposure(ToolExposure::Hidden);
        registry
    };
    for name in builtin_plan_tool_names(&[], false, &Default::default()) {
        assert!(
            registry.get(&name).is_none(),
            "`{name}` is a plugin tool, not a core tool"
        );
    }
}
