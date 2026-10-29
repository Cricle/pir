//! Unit tests for the plugin layer migrated out of `src/plugins/*` inline
//! `#[cfg(test)]` modules: builtin factory resolution, loader path semantics,
//! event-outcome merging, wire-outcome mapping, registry conflict rules, and
//! plugin identity helpers.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// src/plugins/builtin.rs
// ---------------------------------------------------------------------------

use pir::plugins::builtin::{builtin_names, instantiate_builtin};
use pir::plugins::plugin::{AgentPlugin, PluginRegistrations, RegistrationContext, ToolBackend};
use pir::plugins::registry::{PluginRegistry, RegisteredPlugin, RESERVED_KEYBINDING_IDS};

fn registered_plugin_for_test(
    plugin: std::sync::Arc<dyn AgentPlugin>,
    registrations: PluginRegistrations,
) -> RegisteredPlugin {
    RegisteredPlugin {
        info: plugin.info().clone(),
        registrations,
    }
}

#[test]
fn subagent_registers_a_tool_through_the_shared_registry() {
    let plugin = instantiate_builtin("subagent").expect("subagent exists");
    let mut regs = PluginRegistrations::default();
    let mut ctx = RegistrationContext::new(&mut regs);
    plugin.register(&mut ctx).expect("register");
    assert!(regs.tools.iter().any(|(def, _)| def.name == "subagent"));

    let registry =
        PluginRegistry::build(vec![registered_plugin_for_test(plugin, regs)], Vec::new());
    let tools = registry.resolved_tools();
    assert!(tools.iter().any(|(def, _)| def.name == "subagent"));
}

/// `tool_backend` is the allocation-free lookup `PluginHost::call_tool_with_context`
/// uses; it must answer exactly what the full `resolved_tools()` list would
/// (first registration wins, same backend object), and `None` for an unknown
/// name.
#[test]
fn tool_backend_matches_the_resolved_tools_first_wins_rule() {
    use pir::extensions::ToolDef;

    let def = |label: &str| {
        let mut def: ToolDef =
            serde_json::from_value(serde_json::json!({ "name": "dup" })).expect("minimal tool def");
        def.label = Some(label.to_owned());
        def
    };
    let mut first = candidate(PluginInfo::cli_path(std::path::Path::new("/a.ts")));
    first.registrations.tools.push((
        def("first"),
        ToolBackend::Js {
            execution_mode: Default::default(),
        },
    ));
    let mut second = candidate(PluginInfo::cli_path(std::path::Path::new("/b.ts")));
    second.registrations.tools.push((
        def("second"),
        ToolBackend::Js {
            execution_mode: Default::default(),
        },
    ));

    let registry = PluginRegistry::build(vec![first, second], Vec::new());
    let resolved = registry.resolved_tools();
    assert_eq!(resolved.len(), 1, "duplicate tool name resolves once");
    assert_eq!(resolved[0].0.label.as_deref(), Some("first"));

    let backend = registry.tool_backend("dup").expect("registered tool");
    assert!(
        std::ptr::eq(&resolved[0].1, backend),
        "tool_backend must return the same first-wins backend"
    );
    assert!(registry.tool_backend("missing").is_none());
}

#[test]
fn unknown_builtins_are_absent() {
    // `builtin:mcp` is a real builtin since the MCP integration landed; the
    // absent-name probe uses a name no factory registers.
    assert!(instantiate_builtin("no-such-builtin").is_none());
    assert!(instantiate_builtin("").is_none());
    assert!(instantiate_builtin("mcp").is_some());
    assert!(instantiate_builtin("codemode").is_some());
    // pir-only builtin: the embedded RustPython `python` tool.
    assert!(instantiate_builtin("python").is_some());
    // The ported npm `@juicesharp/rpiv-todo` package: `todo` + `/todos`.
    assert!(instantiate_builtin("todo").is_some());
    assert_eq!(
        builtin_names(),
        vec![
            "subagent",
            "mcp",
            "codemode",
            "python",
            "context-mode",
            "tool-search",
            "tool-repair",
            "todo",
            "phone"
        ]
    );

    // The reserved-keybinding list ships with the framework for hosts.
    assert!(RESERVED_KEYBINDING_IDS.contains(&"tui.input.submit"));
}

// ---------------------------------------------------------------------------
// src/plugins/loader.rs
// ---------------------------------------------------------------------------

use pir::plugins::loader::{
    disabled_from_flags, filter_disabled_builtins, order_extension_paths, partition_paths,
    unknown_builtin_errors,
};
use pir::plugins::registry::DiagnosticKind;

fn builtin(name: &str) -> PathBuf {
    PathBuf::from(format!("builtin:{name}"))
}

#[test]
fn partitions_builtin_and_file_paths() {
    let (builtins, files) = partition_paths(&[builtin("mcp"), PathBuf::from("./ext.ts")]);
    assert_eq!(builtins, vec!["mcp".to_owned()]);
    assert_eq!(files, vec![PathBuf::from("./ext.ts")]);
}

#[test]
fn disabled_builtins_are_filtered_even_from_cli() {
    let mut disabled = std::collections::BTreeSet::new();
    disabled.insert("mcp".to_owned());
    let filtered = filter_disabled_builtins(
        &[builtin("mcp"), builtin("subagent"), PathBuf::from("x.ts")],
        &disabled,
    );
    assert_eq!(filtered, vec![builtin("subagent"), PathBuf::from("x.ts")]);
}

// ---------------------------------------------------------------------------
// Native-first `tool-repair` precedence (src/plugins/loader.rs)
// ---------------------------------------------------------------------------

/// The npm package's real install shape
/// (`~/.pi/agent/npm/node_modules/pi-tool-repair/tool-repair.ts`).
fn js_tool_repair_entry() -> PathBuf {
    PathBuf::from("C:/u/.pi/agent/npm/node_modules/pi-tool-repair/tool-repair.ts")
}

#[test]
fn tool_repair_precedence_is_native_first() {
    use pir::plugins::loader::{tool_repair_precedence, ToolRepairPrecedence};
    // A builtin on the effective plan wins whether or not the JS package is
    // also on it.
    assert_eq!(
        tool_repair_precedence(true, true),
        ToolRepairPrecedence::NativeBuiltin
    );
    assert_eq!(
        tool_repair_precedence(true, false),
        ToolRepairPrecedence::NativeBuiltin
    );
    // Builtin disabled/absent: the JS copy keeps the face (the fallback).
    assert_eq!(
        tool_repair_precedence(false, true),
        ToolRepairPrecedence::JsPackage
    );
    assert_eq!(
        tool_repair_precedence(false, false),
        ToolRepairPrecedence::Absent
    );
}

#[test]
fn native_builtin_wins_over_the_js_package_in_the_load_list() {
    // Both faces on the plan: the builtin survives the split and the JS
    // package's entry is dropped from the runtime load list — the JS Phase 0
    // must never run beside the native one (the native builtin is the only
    // face whose `message_end` repair lands).
    let (builtins, files) = partition_paths(&[
        builtin("mcp"),
        builtin("tool-repair"),
        js_tool_repair_entry(),
        PathBuf::from("./other-ext.ts"),
    ]);
    assert!(builtins.contains(&"tool-repair".to_owned()));
    assert_eq!(
        files,
        vec![PathBuf::from("./other-ext.ts")],
        "the JS pi-tool-repair entry must not reach the load list"
    );
}

#[test]
fn disabled_tool_repair_leaves_the_js_package_on_the_plan() {
    // `disabledBuiltinExtensions: ["tool-repair"]` drops the builtin path
    // before the precedence pass, so the JS copy stays as the only face —
    // the user must never get neither.
    let disabled = std::collections::BTreeSet::from(["tool-repair".to_owned()]);
    let filtered = filter_disabled_builtins(
        &[
            builtin("tool-repair"),
            js_tool_repair_entry(),
            PathBuf::from("./other-ext.ts"),
        ],
        &disabled,
    );
    assert_eq!(
        filtered,
        vec![js_tool_repair_entry(), PathBuf::from("./other-ext.ts")],
        "the disabled builtin must leave the JS entry on the effective plan"
    );
    // The runtime load list keeps the JS entry too (no builtin left to win).
    let (builtins, files) = partition_paths(&filtered);
    assert!(builtins.is_empty());
    assert_eq!(
        files,
        vec![js_tool_repair_entry(), PathBuf::from("./other-ext.ts")]
    );
}

#[test]
fn disabling_another_builtin_still_suppresses_the_js_package() {
    // Disabling some OTHER builtin keeps `builtin:tool-repair` on the plan,
    // so the JS copy is suppressed even though the filter ran.
    let disabled = std::collections::BTreeSet::from(["mcp".to_owned()]);
    let filtered = filter_disabled_builtins(
        &[
            builtin("mcp"),
            builtin("tool-repair"),
            js_tool_repair_entry(),
        ],
        &disabled,
    );
    assert_eq!(filtered, vec![builtin("tool-repair")]);
}

#[test]
fn no_mcp_flag_disables_the_mcp_builtin_name() {
    assert_eq!(disabled_from_flags(true), vec!["mcp".to_owned()]);
    assert!(disabled_from_flags(false).is_empty());
}

#[test]
fn cli_paths_come_first_then_project_then_global() {
    let cli = vec![PathBuf::from("cli.ts"), builtin("subagent")];
    // The caller passes default paths project-local BEFORE user-global
    // (the upstream discovery-order fix).
    let defaults = vec![
        PathBuf::from("D:/proj/.pi/extensions"),
        PathBuf::from("C:/u/.pi/agent/extensions"),
    ];
    let ordered = order_extension_paths(&cli, &defaults, false);
    // Default-enabled factory builtins are appended AFTER the discovered file
    // paths (upstream package-manager.ts:986-1001): `builtin:subagent` dedups
    // into its CLI position, the default paths follow, then the remaining
    // builtins in factory order; `phone` is compiled but default-disabled, so
    // it is not inserted.

    assert_eq!(ordered.len(), 11);
    assert_eq!(ordered[0], PathBuf::from("cli.ts"));
    assert_eq!(ordered[1], builtin("subagent"));
    assert_eq!(ordered[2], PathBuf::from("D:/proj/.pi/extensions"));
    assert_eq!(ordered[3], PathBuf::from("C:/u/.pi/agent/extensions"));

    assert_eq!(ordered[4], builtin("mcp"));
    assert_eq!(ordered[5], builtin("codemode"));
    assert_eq!(ordered[6], builtin("python"));
    assert_eq!(ordered[7], builtin("context-mode"));
    assert_eq!(ordered[8], builtin("tool-search"));
    assert_eq!(ordered[9], builtin("tool-repair"));
    assert_eq!(ordered[10], builtin("todo"));

    let cli_only = order_extension_paths(&cli, &defaults, true);
    assert_eq!(cli_only, cli);
}

#[test]
fn unknown_builtins_error_with_upstream_text() {
    let mut diagnostics = Vec::new();
    unknown_builtin_errors(&["nope".to_owned()], &mut diagnostics);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].path, "builtin:nope");
    assert_eq!(
        diagnostics[0].message,
        "Unknown built-in extension: builtin:nope"
    );
}

/// A `builtin:<name>` path that names a real builtin compiled out of this
/// build must say *why* instead of claiming the name is unknown; only a
/// genuinely unknown name keeps the upstream wording (the test above).
#[test]
fn compiled_out_builtins_name_their_cargo_feature() {
    // Every entry the diagnostic table knows is a registered builtin in a
    // full build (the default feature set this suite runs under).
    for (name, feature) in [
        ("subagent", "subagent"),
        ("tool-repair", "tool-repair"),
        ("tool-search", "tool-search"),
        ("mcp", "mcp"),
        ("python", "python"),
        ("codemode", "extensions"),
        ("todo", "todo"),
    ] {
        assert!(
            builtin_names().contains(&name),
            "{name} is not a registered builtin name in a full build"
        );
        assert_eq!(
            pir::plugins::builtin::missing_builtin_message(name),
            format!(
                "Built-in extension `builtin:{name}` is not compiled into this build (the `{feature}` feature is off)"
            ),
            "the diagnostic for a compiled-out builtin must name the feature"
        );
    }
    // Unknown names are untouched by the table.
    assert_eq!(
        pir::plugins::builtin::missing_builtin_message("definitely-not-a-builtin"),
        "Unknown built-in extension: builtin:definitely-not-a-builtin"
    );
}

// ---------------------------------------------------------------------------
// src/plugins/merge.rs
// ---------------------------------------------------------------------------

use pir::plugins::merge::MergedEmit;
use pir::plugins::plugin::{InputAction, PluginEventOutcome};

#[test]
fn tool_call_block_short_circuits() {
    let mut acc = MergedEmit::new();
    acc.merge(
        "tool_call",
        "a.ts",
        PluginEventOutcome::ToolCall {
            block: false,
            reason: None,
            terminate: false,
        },
    );
    acc.merge(
        "tool_call",
        "b.ts",
        PluginEventOutcome::ToolCall {
            block: true,
            reason: Some("no".into()),
            terminate: false,
        },
    );
    assert!(acc.stop);
    assert_eq!(acc.result["block"], serde_json::Value::Bool(true));
}

#[test]
fn failing_tool_call_handler_is_fail_closed() {
    let mut acc = MergedEmit::new();
    acc.merge(
        "tool_call",
        "builtin:x",
        PluginEventOutcome::Error {
            error: "boom".into(),
        },
    );
    assert!(acc.stop);
    assert_eq!(acc.result["block"], serde_json::Value::Bool(true));
    assert_eq!(acc.errors.len(), 1);
}

#[test]
fn input_handled_stops_dispatch() {
    let mut acc = MergedEmit::new();
    acc.merge(
        "input",
        "a.ts",
        PluginEventOutcome::Input {
            action: InputAction::Transform,
            text: Some("one".into()),
            images: None,
        },
    );
    acc.merge(
        "input",
        "b.ts",
        PluginEventOutcome::Input {
            action: InputAction::Handled,
            text: None,
            images: None,
        },
    );
    assert!(acc.stop);
    assert_eq!(
        acc.result["action"],
        serde_json::Value::String("handled".into())
    );
}

#[test]
fn before_agent_start_overlays_last_wins() {
    let mut acc = MergedEmit::new();
    acc.merge(
        "before_agent_start",
        "a.ts",
        PluginEventOutcome::BeforeAgentStart {
            system_prompt: Some("first".into()),
            message: Some(serde_json::json!({ "content": [] })),
            messages: Vec::new(),
        },
    );
    acc.merge(
        "before_agent_start",
        "b.ts",
        PluginEventOutcome::BeforeAgentStart {
            system_prompt: Some("second".into()),
            message: None,
            messages: Vec::new(),
        },
    );
    assert_eq!(
        acc.result["systemPrompt"],
        serde_json::Value::String("second".into())
    );
    assert!(acc.result.get("message").is_some());
}

// ---------------------------------------------------------------------------
// src/plugins/plugin.rs
// ---------------------------------------------------------------------------

#[test]
fn wire_outcomes_map_per_family() {
    let block = serde_json::json!({ "block": true, "reason": "nope" });
    match PluginEventOutcome::from_wire("tool_call", &block) {
        PluginEventOutcome::ToolCall { block, reason, .. } => {
            assert!(block);
            assert_eq!(reason.as_deref(), Some("nope"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        matches!(
            PluginEventOutcome::from_wire("input", &serde_json::json!({ "action": "handled" })),
            PluginEventOutcome::Input {
                action: InputAction::Handled,
                text: None,
                ..
            }
        ),
        "input handled outcome should map to InputAction::Handled"
    );
    assert!(
        matches!(
            PluginEventOutcome::from_wire("turn_start", &serde_json::Value::Null),
            PluginEventOutcome::None
        ),
        "null results map to None"
    );
}

// ---------------------------------------------------------------------------
// src/plugins/registry.rs
// ---------------------------------------------------------------------------

use pir::plugins::source::PluginInfo;

fn empty_registrations() -> PluginRegistrations {
    PluginRegistrations::default()
}

fn candidate(info: PluginInfo) -> RegisteredPlugin {
    RegisteredPlugin {
        info,
        registrations: empty_registrations(),
    }
}

fn register_command(plugin: &mut RegisteredPlugin, name: &str) {
    let mut regs = std::mem::take(&mut plugin.registrations);
    {
        let mut ctx = RegistrationContext::new(&mut regs);
        ctx.register_command(name, None, None);
    }
    plugin.registrations = regs;
}

#[test]
fn duplicate_commands_get_occurrence_suffixes() {
    let mut a = candidate(PluginInfo::cli_path(std::path::Path::new("/a.ts")));
    let mut b = candidate(PluginInfo::cli_path(std::path::Path::new("/b.ts")));
    register_command(&mut a, "greet");
    register_command(&mut b, "greet");
    let registry = PluginRegistry::build(vec![a, b], Vec::new());
    let names: Vec<String> = registry
        .resolved_commands()
        .into_iter()
        .map(|command| command.invocation_name)
        .collect();
    assert_eq!(names, vec!["greet:1", "greet:2"]);
}

#[test]
fn replaceable_builtin_is_omitted_with_upstream_warning() {
    let mut builtin = candidate(PluginInfo::builtin("codemode"));
    builtin.info.replaceable = true;
    register_command(&mut builtin, "codemode");
    let mut js = candidate(PluginInfo::cli_path(std::path::Path::new("/x/other.ts")));
    register_command(&mut js, "codemode");

    let registry = PluginRegistry::build(vec![builtin, js], Vec::new());
    assert_eq!(registry.plugins().len(), 1);
    assert!(registry.plugins()[0].info.path.ends_with("other.ts"));
    let warning = &registry.diagnostics()[0];
    assert_eq!(warning.kind, DiagnosticKind::Warning);
    assert!(
        warning
            .message
            .contains("built-in extension `codemode` was not loaded"),
        "{}",
        warning.message
    );
}

#[test]
fn non_victim_replaceable_plugins_survive() {
    let mut builtin = candidate(PluginInfo::builtin("codemode"));
    builtin.info.replaceable = true;
    register_command(&mut builtin, "codemode");
    let registry = PluginRegistry::build(vec![builtin], Vec::new());
    assert_eq!(registry.plugins().len(), 1);
    assert!(registry.diagnostics().is_empty());
}

// ---------------------------------------------------------------------------
// src/plugins/source.rs
// ---------------------------------------------------------------------------

use pir::plugins::source::{builtin_name, is_builtin_path};

#[test]
fn builtin_paths_resolve_names() {
    assert!(is_builtin_path("builtin:mcp"));
    assert_eq!(builtin_name("builtin:mcp"), Some("mcp"));
    assert_eq!(builtin_name("mcp"), None);
    assert_eq!(builtin_name("builtin:"), None);
}

#[test]
fn synthetic_sources() {
    assert_eq!(
        PluginInfo::synthetic_source("builtin:mcp"),
        Some("builtin".to_owned())
    );
    assert_eq!(
        PluginInfo::synthetic_source("<inline:1>"),
        Some("inline".to_owned())
    );
    assert_eq!(PluginInfo::synthetic_source("/tmp/x.ts"), None);
}
