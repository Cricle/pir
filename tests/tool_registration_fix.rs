//! Regression tests for the tool registration/selection fixes:
//!
//! 1. `powershell` is REGISTERED like every other core tool (upstream
//!    `createAllToolDefinitions` registers it unconditionally; only its
//!    execution is Windows-gated), but it is not in the no-flag ACTIVE set:
//!    `default_active: false` matches upstream's `DEFAULT_TOOL_NAMES`, which
//!    has no powershell.
//! 2. `--exclude-tools` starts from the tools that activate on registration
//!    (plus the default names), so excluding one tool keeps the rest of the
//!    default run — and a denylist never ACTIVATES a default-off tool such as
//!    powershell.
//! 3. The `+name`/`-name` modifier base stays the upstream
//!    `DEFAULT_TOOL_NAMES` 4-name default (sdk.ts:282-286), and
//!    `--no-builtin-tools` drops every core tool (upstream
//!    `noTools: "builtin"`), keeping only extension/custom tools.

use pir::agent::tool_execution::ToolRegistry;
use pir::coding_agent::tool_selection::resolve_tool_selection;
use pir::coding_agent::tools::{register_core_tools, CORE_TOOL_NAMES};
use pir::config::settings::DEFAULT_TOOL_NAMES;

fn core_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

fn tools(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn registry_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry.names().iter().map(|s| (*s).to_string()).collect();
    names.sort();
    names
}

#[test]
fn no_flag_registry_declares_the_full_core_set() {
    let registry = core_registry();
    assert_eq!(registry_names(&registry), sorted(tools(CORE_TOOL_NAMES)));
    // The REGISTRY declares every core tool (the `--exclude-tools` base and
    // the modifier availability set above), but the no-flag ACTIVE set is
    // the upstream default: `DEFAULT_TOOL_NAMES` (read/bash/edit/write) plus
    // pir's registered-and-active core trio (grep/find/ls), with `powershell`
    // registered-but-inactive exactly like upstream (DEFAULT_TOOL_NAMES has
    // no powershell; tests/suites/powershell_tool.rs pins the same rule).
    let active = registry.active_names(None);
    assert!(
        !active.iter().any(|name| name == "powershell"),
        "powershell must stay out of the no-flag active set: {active:?}"
    );
    for expected in ["bash", "edit", "read", "write", "grep", "find", "ls"] {
        assert!(
            active.iter().any(|name| name == expected),
            "{expected} should be active by default: {active:?}"
        );
    }
}

#[test]
fn exclude_bash_keeps_the_rest_of_the_core_registry() {
    let registry = core_registry();
    let available = registry_names(&registry);
    let selection = resolve_tool_selection(
        None,
        &[],
        &["bash".to_string()],
        false,
        false,
        &available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("valid selection")
    .expect("exclude-tools requests a selection");

    assert!(!selection.enabled.iter().any(|name| name == "bash"));
    assert!(
        !selection.enabled.iter().any(|name| name == "powershell"),
        "a denylist must never activate default-off powershell: {:?}",
        selection.enabled
    );
    let expected: Vec<String> = sorted(tools(CORE_TOOL_NAMES))
        .into_iter()
        .filter(|name| name != "bash" && name != "powershell")
        .collect();
    assert_eq!(sorted(selection.enabled.clone()), expected);

    // Applying the selection like the print host does (drop non-enabled from
    // the registry, then take the active override) keeps the same rule:
    // powershell stays registered-but-inactive.
    let mut filtered = registry.clone();
    for name in registry_names(&registry) {
        if !selection.enabled.contains(&name) {
            filtered.remove(&name);
        }
    }
    let active = filtered.active_names(Some(&selection.enabled));
    assert!(
        !active.iter().any(|name| name == "powershell"),
        "powershell must stay out of the active set after --exclude-tools bash: {active:?}"
    );
    assert!(!active.iter().any(|name| name == "bash"));
}

#[test]
fn modifier_list_base_stays_the_four_defaults() {
    let registry = core_registry();
    let available = registry_names(&registry);
    let selection = resolve_tool_selection(
        None,
        &["-bash".to_string()],
        &[],
        false,
        false,
        &available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("valid selection")
    .expect("a modifier list requests a selection");

    // Modifiers apply to DEFAULT_TOOL_NAMES, not the full registry: the
    // upstream sdk.ts modifier base stays read/bash/edit/write, and `-bash`
    // removes bash from that base (read/edit/write remain).
    assert_eq!(
        sorted(selection.enabled.clone()),
        sorted(tools(&["read", "edit", "write"]))
    );
    assert!(selection.enabled.iter().any(|name| name == "read"));
    assert!(!selection.enabled.iter().any(|name| name == "bash"));
    assert!(
        !selection
            .enabled
            .iter()
            .any(|name| name == "grep" || name == "find" || name == "ls" || name == "powershell"),
        "modifier base must not pull in the non-default core tools: {:?}",
        selection.enabled
    );
}

#[test]
fn no_builtin_tools_keeps_only_extension_tools() {
    let registry = core_registry();
    let mut available = registry_names(&registry);
    available.push("ext_tool".to_string());
    let selection =
        resolve_tool_selection(None, &[], &[], false, true, &available, &DEFAULT_TOOL_NAMES)
            .expect("valid selection")
            .expect("no-builtin-tools requests a selection");

    // Upstream noTools: "builtin" leaves every built-in out of the active set
    // (not just the 4-name default), keeping extension/custom tools.
    assert_eq!(selection.enabled, tools(&["ext_tool"]));
}

#[test]
fn settings_default_tools_still_override_the_defaults() {
    let registry = core_registry();
    let available = registry_names(&registry);
    let selection = resolve_tool_selection(
        Some(&tools(&["grep", "find"])),
        &[],
        &[],
        false,
        false,
        &available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("valid selection")
    .expect("a settings defaultTools list requests a selection");

    assert_eq!(selection.enabled, tools(&["grep", "find"]));
}

/// The registry backs a `HashMap`, so raw iteration order is randomized per
/// process. The provider tools array is built from it; a moving prefix defeats
/// prompt caching and makes two identical runs send different requests. Both
/// `names()` and `iter()` must expose a stable name order (an explicit active
/// list still keeps its caller-provided order).
#[test]
fn registry_iteration_and_names_are_deterministic() {
    let registry = core_registry();
    let names = registry.names();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "names() must be sorted");

    let iterated: Vec<&str> = registry.iter().map(|tool| tool.name()).collect();
    assert_eq!(iterated, sorted, "iter() must follow the same stable order");
    assert_eq!(registry.names(), names, "repeated reads must not move");
    assert_eq!(
        registry.active_names(None),
        registry.active_names(None),
        "the default active list is stable too"
    );
}
