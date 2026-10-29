//! Unit tests for tool selection migrated out of
//! `src/coding_agent/tool_selection.rs` inline `#[cfg(test)]` tests: the
//! `--tools` / `--exclude-tools` / `--no-tools` / `--no-builtin-tools`
//! selection semantics over the default registry.

fn tools(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

const BUILTIN: &[&str] = &["read", "bash", "edit", "write"];

use pir::coding_agent::tool_selection::{
    compiled_out_selection_diagnostics, compiled_out_tool_message, resolve_tool_selection,
    unavailable_tool_message,
};
use pir::coding_agent::ToolSelection;

#[test]
fn no_flags_keeps_everything() {
    let available = tools(&["read", "bash", "edit", "write", "mcp__srv__t"]);
    let selection =
        resolve_tool_selection(None, &[], &[], false, false, &available, BUILTIN).expect("valid");
    assert!(selection.is_none());
}

#[test]
fn allowlist_replaces_and_keeps_mcp_registered_but_undeclared() {
    // Upstream `_isAllowedTool` / `_isActivatable` (agent-session.ts:1542-1558):
    // an `mcp__*` name the allowlist does not name or match stays REGISTERED
    // (pir's registry is complete regardless — the F12 rule) but never enters
    // the active/declared set. The old expectation kept it in `enabled`, which
    // is the declared set, so every explicit allowlist declared every MCP tool.
    let available = tools(&["read", "bash", "edit", "write", "mcp__srv__t"]);
    let selection = resolve_tool_selection(
        None,
        &tools(&["read", "grep"]),
        &[],
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect("valid")
    .expect("selection");
    assert_eq!(selection.enabled, tools(&["read"]));
}

#[test]
fn modifiers_add_and_remove() {
    let available = tools(&["read", "bash", "edit", "write", "codemode"]);
    let selection = resolve_tool_selection(
        None,
        &tools(&["-write", "+codemode"]),
        &[],
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect("valid")
    .expect("selection");
    assert_eq!(
        selection.enabled,
        tools(&["read", "bash", "edit", "codemode"])
    );
}

#[test]
fn mixed_modifiers_are_rejected() {
    let available = tools(&["read", "bash"]);
    let error = resolve_tool_selection(
        None,
        &tools(&["read", "-write"]),
        &[],
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect_err("mixed lists are invalid");
    assert!(error.contains("cannot be mixed"));
}

#[test]
fn modifier_patterns_are_rejected() {
    let available = tools(&["read", "bash"]);
    let error = resolve_tool_selection(
        None,
        &tools(&["-write*"]),
        &[],
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect_err("patterns in modifier lists are invalid");
    assert!(error.contains("not patterns"));
}

#[test]
fn exclude_removes_glob_matches() {
    let available = tools(&["read", "bash", "mcp__srv__t"]);
    let selection = resolve_tool_selection(
        None,
        &[],
        &tools(&["mcp__*"]),
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect("valid")
    .expect("selection");
    // The MCP tool is never in the selection to begin with (no carve-out), so
    // this pins that an `mcp__*` exclude glob stays valid and inert: it
    // removes a registered name the base never declared, and the built-ins
    // stay. Upstream's denylist filters the same way — it can only remove,
    // never add.
    assert_eq!(selection.enabled, tools(&["read", "bash", "edit", "write"]));
}

#[test]
fn no_builtin_tools_keeps_custom() {
    let available = tools(&["read", "bash", "ext_tool"]);
    let selection = resolve_tool_selection(None, &[], &[], false, true, &available, BUILTIN)
        .expect("valid")
        .expect("selection");
    assert_eq!(selection.enabled, tools(&["ext_tool"]));
}

// ---------------------------------------------------------------------------
// Resume restore inputs (upstream `usesDefaultTools` / `defaultToolModifiers`):
// a selection derived from the default tools carries the `+name`/`-name`
// entries and may be replaced on resume by the transcript's recorded loadout;
// an explicit selection may not.
// ---------------------------------------------------------------------------

#[test]
fn modifier_selection_carries_the_modifiers_and_the_default_gate() {
    let available = tools(&["read", "bash", "edit", "write", "codemode"]);
    let entries = tools(&["-write", "+codemode"]);
    let selection = resolve_tool_selection(None, &entries, &[], false, false, &available, BUILTIN)
        .expect("valid")
        .expect("selection");
    assert_eq!(selection.modifiers, entries);
    assert!(selection.uses_default_tools);
}

#[test]
fn allowlist_selection_is_explicit() {
    let available = tools(&["read", "bash", "edit", "write"]);
    let selection = resolve_tool_selection(
        None,
        &tools(&["read", "write"]),
        &[],
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect("valid")
    .expect("selection");
    assert!(selection.modifiers.is_empty());
    assert!(!selection.uses_default_tools);
}

#[test]
fn no_tools_and_no_builtin_tools_are_explicit() {
    let available = tools(&["read", "bash", "ext_tool"]);
    let no_tools = resolve_tool_selection(None, &[], &[], true, false, &available, BUILTIN)
        .expect("valid")
        .expect("selection");
    assert!(!no_tools.uses_default_tools);
    let no_builtin = resolve_tool_selection(None, &[], &[], false, true, &available, BUILTIN)
        .expect("valid")
        .expect("selection");
    assert!(!no_builtin.uses_default_tools);
}

#[test]
fn settings_and_exclude_only_selections_are_default_selections() {
    let available = tools(&["read", "bash", "edit", "write"]);
    let settings = tools(&["read", "bash"]);
    let from_settings =
        resolve_tool_selection(Some(&settings), &[], &[], false, false, &available, BUILTIN)
            .expect("valid")
            .expect("selection");
    assert!(from_settings.modifiers.is_empty());
    assert!(from_settings.uses_default_tools);
    // `--exclude-tools` alone does not select explicitly (upstream's gate).
    let exclude_only = resolve_tool_selection(
        None,
        &[],
        &tools(&["bash"]),
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect("valid")
    .expect("selection");
    assert!(exclude_only.uses_default_tools);
}

#[test]
fn a_literal_selection_defaults_to_explicit() {
    // Every struct literal that does not set the fields reads as an explicit
    // selection, so a host list never triggers the resume restore.
    let selection = ToolSelection {
        enabled: tools(&["read"]),
        ..Default::default()
    };
    assert!(!selection.uses_default_tools);
    assert!(selection.modifiers.is_empty());
}

// ---------------------------------------------------------------------------
// Explicit requests for a compiled-out surface: the `pir:` diagnostic (R5).
// The selection itself is unchanged — a genuinely unknown name is still
// silently dropped, exactly like upstream.
// ---------------------------------------------------------------------------

#[test]
fn unavailable_tool_message_names_the_missing_feature() {
    assert_eq!(
        unavailable_tool_message("grep", "grep-find"),
        "tool `grep` is unavailable: this build was compiled without the `grep-find` feature"
    );
}

#[test]
fn genuinely_unknown_names_stay_silent() {
    // The helper knows no feature for an unknown name ...
    assert!(compiled_out_tool_message("definitely-not-a-tool").is_none());
    // ... and the resolver still drops it without an error (upstream).
    let diagnostics = compiled_out_selection_diagnostics(
        None,
        &tools(&["definitely-not-a-tool"]),
        &[],
        false,
        false,
    );
    assert!(diagnostics.is_empty());
    let available = tools(&["read", "bash"]);
    let selection = resolve_tool_selection(
        None,
        &tools(&["read", "definitely-not-a-tool"]),
        &[],
        false,
        false,
        &available,
        BUILTIN,
    )
    .expect("valid")
    .expect("selection");
    assert_eq!(selection.enabled, tools(&["read"]));
}

#[test]
fn patterns_and_removal_modifiers_stay_silent() {
    // `*` entries are patterns, never names.
    assert!(
        compiled_out_selection_diagnostics(None, &tools(&["gr*"]), &[], false, false).is_empty()
    );
    // `-name` removes; it never requests a surface.
    assert!(
        compiled_out_selection_diagnostics(None, &tools(&["-grep"]), &[], false, false).is_empty()
    );
}

/// Every enabled surface stays silent: with `grep-find` on (independently of
/// `tools`) neither `grep` nor `find` is diagnosed, and every name the build
/// registers is compiled in.
#[cfg(feature = "grep-find")]
#[test]
fn compiled_in_surfaces_are_not_diagnosed() {
    assert!(compiled_out_tool_message("grep").is_none());
    assert!(compiled_out_tool_message("find").is_none());
    for name in pir::coding_agent::tools::CORE_TOOL_NAMES {
        assert!(
            compiled_out_tool_message(name).is_none(),
            "`{name}` is compiled in and must never be diagnosed"
        );
    }
}

#[cfg(not(feature = "grep-find"))]
#[test]
fn explicit_compiled_out_tool_names_are_diagnosed() {
    let expected = vec![unavailable_tool_message("grep", "grep-find")];
    // A plain `--tools` allowlist entry (only `grep` is requested: a core
    // name would also diagnose when its leaf is off in a partial build).
    assert_eq!(
        compiled_out_selection_diagnostics(None, &tools(&["grep"]), &[], false, false),
        expected
    );
    // The settings `defaultTools` base takes the same treatment.
    let defaults = tools(&["grep"]);
    assert_eq!(
        compiled_out_selection_diagnostics(Some(&defaults), &[], &[], false, false),
        expected
    );
    // A `+grep` modifier explicitly asks for the surface too.
    assert_eq!(
        compiled_out_selection_diagnostics(None, &tools(&["+grep"]), &[], false, false),
        expected
    );
    // A name a user extension registered is available even in this build, so
    // it is never diagnosed as compiled out.
    let available = [pir::coding_agent::tool_selection::AvailableTool::name_only(
        "grep",
    )];
    assert!(
        compiled_out_selection_diagnostics(None, &tools(&["grep"]), &available, false, false)
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// ToolSelectionRequest: the one request the CLI host hands to RPC and the
// server, resolved through the same shared resolver.
// ---------------------------------------------------------------------------

use pir::coding_agent::tool_selection::{
    builtin_plan_available_tools, extension_plan_available_tools, host_available_tools,
    AvailableTool, ToolSelectionRequest,
};

fn request(
    cli_tools: &[&str],
    cli_exclude: &[&str],
    no_tools: bool,
    no_builtin_tools: bool,
) -> ToolSelectionRequest {
    ToolSelectionRequest {
        settings_default_tools: None,
        cli_tools: tools(cli_tools),
        cli_exclude: tools(cli_exclude),
        no_tools,
        no_builtin_tools,
    }
}

#[test]
fn requested_is_false_for_an_empty_request_and_true_for_settings_defaults() {
    assert!(!ToolSelectionRequest::default().requested());
    assert!(request(&["read"], &[], false, false).requested());
    assert!(request(&[], &["bash"], false, false).requested());
    assert!(request(&[], &[], true, false).requested());
    assert!(request(&[], &[], false, true).requested());
    let defaults = ToolSelectionRequest {
        settings_default_tools: Some(tools(&["read"])),
        ..ToolSelectionRequest::default()
    };
    assert!(defaults.requested());
}

/// The request is a thin envelope: resolving it must produce exactly the
/// selection the shared resolver produces for the same inputs. Every flag
/// form is pinned here (allowlist, exclude-only, no-tools, no-builtin-tools,
/// no-tools + allowlist, modifiers, patterns, and an invalid mixed list).
#[test]
fn tool_selection_request_resolves_the_same_ways_as_the_shared_resolver() {
    let available_names = tools(&["read", "bash", "edit", "write", "codemode"]);
    let available: Vec<AvailableTool> = available_names
        .iter()
        .map(AvailableTool::name_only)
        .collect();
    let cases: [ToolSelectionRequest; 8] = [
        request(&["read"], &[], false, false),
        request(&[], &["bash"], false, false),
        request(&[], &[], true, false),
        request(&[], &[], false, true),
        request(&["read"], &[], true, false),
        request(&["-write", "+codemode"], &[], false, false),
        request(&["re*"], &[], false, false),
        request(&["read", "+bash"], &[], false, false),
    ];
    for case in cases {
        let from_request = case.resolve(&available).map_err(|error| error.to_string());
        let direct = resolve_tool_selection(
            case.settings_default_tools.as_deref(),
            &case.cli_tools,
            &case.cli_exclude,
            case.no_tools,
            case.no_builtin_tools,
            &available_names,
            BUILTIN,
        );
        assert_eq!(
            from_request, direct,
            "the request must be a thin envelope over the shared resolver: {case:?}"
        );
    }
    // The invalid mixed list surfaces the shared error, not a silent empty set.
    let error = request(&["read", "+bash"], &[], false, false)
        .resolve(&available)
        .expect_err("mixing plain names and modifiers is rejected");
    assert!(error.starts_with("--tools: "), "{error}");
}

/// The composition RPC resolves against: core facts first, then the builtin
/// plan, then the runtime's on-disk tools; first name wins.
#[test]
fn host_available_tools_prefers_core_then_builtin_then_runtime() {
    let mut core = pir::agent::tool_execution::ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut core);
    let core_available = AvailableTool::from_registry(&core);
    let core_read = core_available
        .iter()
        .find(|tool| tool.name == "read")
        .expect("the core registry registers read")
        .clone();
    let builtin = vec![
        // A plugin that shadows a core name keeps the core facts (first wins).
        AvailableTool::registered(
            "read",
            pir::agent::tool_execution::ToolExposure::Hidden,
            false,
        ),
        AvailableTool::registered(
            "ctx_execute",
            pir::agent::tool_execution::ToolExposure::Direct,
            true,
        ),
    ];
    let available = host_available_tools(&core, builtin, None);
    let read = available
        .iter()
        .find(|tool| tool.name == "read")
        .expect("read stays present");
    assert_eq!(read, &core_read, "core facts win over a shadowing plugin");
    assert!(
        available.iter().any(|tool| tool.name == "ctx_execute"),
        "the builtin plan contributes its tools"
    );
    // Sorted by name with duplicates removed.
    let names: Vec<&str> = available.iter().map(|tool| tool.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted);
}

/// The final extension plan the RPC host receives (after ordering and
/// `filter_disabled_builtins`) and the CLI-input helper must describe the same
/// builtin tools with the same registration facts.
#[test]
fn extension_plan_available_tools_matches_builtin_plan_available_tools() {
    let plan = pir::plugins::order_extension_paths(&[], &[], false);
    let mut disabled = std::collections::BTreeSet::new();
    // `--no-mcp` composes into the disabled set before the plan is built; the
    // plan helper filters it too, so add it on both sides.
    disabled.insert("mcp".to_string());
    let filtered_plan = pir::plugins::filter_disabled_builtins(&plan, &disabled);
    let from_plan = extension_plan_available_tools(&filtered_plan);
    let from_cli = builtin_plan_available_tools(&[], false, &disabled);
    assert_eq!(
        from_plan, from_cli,
        "the final-plan helper and the CLI-input helper must describe the same builtins"
    );
    assert!(
        from_plan.iter().any(|tool| tool.name == "subagent"),
        "the default extension plan carries its builtin tools"
    );
}
