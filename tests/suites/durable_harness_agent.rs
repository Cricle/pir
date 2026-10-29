//! Agent state, changes, and resolution against a registry snapshot.
//!
//! Covers upstream `harness/agent.ts` (`resolveAgent`, `applyChange`,
//! `configure`) and the stored `pi.agent` shape from `harness/types.ts`:
//! extension selection arrays versus add/remove edits, the `tools` and
//! `modelTools` filters (including deferred loading), instructions appended
//! after extension sections, and the keep/clear/set change semantics for every
//! field.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use pir::durable::harness::agent::{
    configure, resolve_agent, resolve_agent_with, AgentChange, AgentState, Change, ExtensionEdit,
    ExtensionSelection, ModelRef, ModelToolSelection, ModelToolsEdit, ToolEdit, ToolSelection,
    INSTRUCTIONS_KEY,
};
use pir::durable::harness::registry::{
    Extension, HookRegistration, PromptInput, PromptSection, Registry, RegistryReader,
    RegistrySnapshot, ToolCaller, ToolRegistration, Wrap,
};
use pir::durable::harness::settings::{resolve_settings, HarnessSettings};
use pir::durable::{ConversationId, ROOT_CONVERSATION_ID};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// ── helpers ────────────────────────────────────────────────────────────────

fn tool(name: &str) -> ToolRegistration {
    ToolRegistration::new(name)
}

fn deferred(name: &str) -> ToolRegistration {
    ToolRegistration::new(name).deferred()
}

fn extension(name: &str, tools: Vec<ToolRegistration>) -> Extension {
    Extension::new(name).with_tools(tools)
}

fn snapshot(extensions: Vec<Extension>) -> RegistrySnapshot {
    RegistrySnapshot::build(extensions).expect("valid registry")
}

fn settings() -> pir::durable::harness::settings::Settings {
    resolve_settings(&HarnessSettings::default())
}

fn tool_names(tools: &[ToolRegistration]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn extension_names(agent: &pir::durable::harness::agent::Agent) -> Vec<&str> {
    agent
        .extensions
        .iter()
        .map(|extension| extension.name.as_str())
        .collect()
}

fn resolved(
    snapshot: &RegistrySnapshot,
    state: &AgentState,
) -> pir::durable::harness::agent::Agent {
    resolve_agent(snapshot, &settings(), state)
}

// ── extension selection ────────────────────────────────────────────────────

#[test]
fn unset_selection_follows_the_installed_registry() {
    let registry = snapshot(vec![
        extension("alpha", vec![tool("read")]),
        extension("beta", vec![tool("write")]),
    ]);
    let agent = resolved(&registry, &AgentState::default());
    assert_eq!(extension_names(&agent), ["alpha", "beta"]);

    // No model, thinking off, no instructions, no cwd.
    assert!(agent.model.is_none());
    assert_eq!(agent.thinking_level, pir::ai::types::ThinkingLevel::Off);
    assert!(agent.instructions.is_none());
    assert!(agent.cwd.is_none());
    assert!(agent.sections.is_empty());
}

#[test]
fn unset_selection_follows_the_host_default() {
    let registry = snapshot(vec![
        extension("alpha", vec![]),
        extension("beta", vec![]),
        extension("gamma", vec![]),
    ]);
    let settings = resolve_settings(&HarnessSettings {
        extensions: Some(vec![Extension::new("gamma"), Extension::new("alpha")]),
        ..Default::default()
    });
    let agent = resolve_agent(&registry, &settings, &AgentState::default());
    assert_eq!(extension_names(&agent), ["gamma", "alpha"]);
}

#[test]
fn selection_array_selects_exactly_those_extensions_in_order() {
    let registry = snapshot(vec![
        extension("alpha", vec![]),
        extension("beta", vec![]),
        extension("gamma", vec![]),
    ]);
    let state = AgentState {
        extensions: Some(ExtensionSelection::List(vec![
            "gamma".to_owned(),
            "alpha".to_owned(),
        ])),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(extension_names(&agent), ["gamma", "alpha"]);
}

#[test]
fn selection_array_dedupes_and_skips_uninstalled_names() {
    let registry = snapshot(vec![extension("alpha", vec![]), extension("beta", vec![])]);
    let state = AgentState {
        extensions: Some(ExtensionSelection::List(vec![
            "beta".to_owned(),
            "missing".to_owned(),
            "alpha".to_owned(),
            "beta".to_owned(),
        ])),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    // First position wins; the uninstalled name is skipped.
    assert_eq!(extension_names(&agent), ["beta", "alpha"]);
}

#[test]
fn selection_edit_adds_then_removes_from_the_host_default() {
    let registry = snapshot(vec![
        extension("alpha", vec![]),
        extension("beta", vec![]),
        extension("gamma", vec![]),
        extension("delta", vec![]),
    ]);
    let settings = resolve_settings(&HarnessSettings {
        extensions: Some(vec![
            Extension::new("alpha"),
            Extension::new("beta"),
            Extension::new("gamma"),
        ]),
        ..Default::default()
    });
    let state = AgentState {
        extensions: Some(ExtensionSelection::Edit {
            add: Some(vec!["delta".to_owned(), "alpha".to_owned()]),
            remove: Some(vec!["beta".to_owned()]),
        }),
        ..Default::default()
    };
    let agent = resolve_agent(&registry, &settings, &state);
    // base = [alpha, beta, gamma] + [delta, alpha] - [beta] -> alpha, gamma,
    // delta, alpha, then deduped keeping first positions.
    assert_eq!(extension_names(&agent), ["alpha", "gamma", "delta"]);
}

#[test]
fn selection_edit_without_members_inherits_the_registry() {
    let registry = snapshot(vec![extension("alpha", vec![]), extension("beta", vec![])]);
    let state = AgentState {
        extensions: Some(ExtensionSelection::inherit()),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(extension_names(&agent), ["alpha", "beta"]);
}

#[test]
fn selection_edit_remove_wins_over_add() {
    let registry = snapshot(vec![extension("alpha", vec![]), extension("beta", vec![])]);
    let state = AgentState {
        extensions: Some(ExtensionSelection::Edit {
            add: Some(vec!["beta".to_owned()]),
            remove: Some(vec!["beta".to_owned()]),
        }),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(extension_names(&agent), ["alpha"]);
}

// ── tools ──────────────────────────────────────────────────────────────────

#[test]
fn tools_are_composed_in_extension_order_with_in_place_replacement() {
    let registry = snapshot(vec![
        extension(
            "one",
            vec![tool("read").with_description("old read"), tool("write")],
        ),
        extension(
            "two",
            vec![tool("write").with_description("new write"), tool("bash")],
        ),
    ]);
    let agent = resolved(&registry, &AgentState::default());

    assert_eq!(tool_names(&agent.tools), ["read", "write", "bash"]);
    // The later same-name tool replaced the earlier one in place.
    assert_eq!(agent.tools[1].description, "new write");
    assert_eq!(tool_names(&agent.callable), ["read", "write", "bash"]);
}

#[test]
fn tools_array_enables_exactly_those_names_in_its_order() {
    let registry = snapshot(vec![extension(
        "one",
        vec![tool("read"), tool("write"), tool("bash")],
    )]);
    let state = AgentState {
        tools: Some(ToolSelection::List(vec![
            "bash".to_owned(),
            "read".to_owned(),
            "bash".to_owned(),
            "missing".to_owned(),
        ])),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["bash", "read"]);
}

#[test]
fn tools_remove_filter_drops_names_and_keeps_order() {
    let registry = snapshot(vec![extension(
        "one",
        vec![tool("read"), tool("write"), tool("bash")],
    )]);
    let state = AgentState {
        tools: Some(ToolSelection::Edit {
            remove: vec!["write".to_owned()],
        }),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["read", "bash"]);
}

#[test]
fn callers_split_offered_and_callable_tools() {
    let registry = snapshot(vec![extension(
        "one",
        vec![
            tool("both"),
            tool("model-only").with_callers(vec![ToolCaller::Model]),
            tool("tools-only").with_callers(vec![ToolCaller::Tools]),
        ],
    )]);
    let agent = resolved(&registry, &AgentState::default());
    assert_eq!(tool_names(&agent.tools), ["both", "model-only"]);
    assert_eq!(tool_names(&agent.callable), ["both", "tools-only"]);
}

// ── model tools and deferred loading ───────────────────────────────────────

fn deferred_registry() -> RegistrySnapshot {
    snapshot(vec![extension(
        "one",
        vec![
            tool("read"),
            tool("write"),
            deferred("search"),
            deferred("fetch"),
        ],
    )])
}

#[test]
fn unset_model_tools_offers_non_deferred_and_defers_the_rest() {
    let registry = deferred_registry();
    let agent = resolved(&registry, &AgentState::default());
    assert_eq!(tool_names(&agent.tools), ["read", "write"]);
    assert_eq!(tool_names(&agent.deferred), ["search", "fetch"]);
    // Deferred tools stay callable by other tools.
    assert_eq!(
        tool_names(&agent.callable),
        ["read", "write", "search", "fetch"]
    );
}

#[test]
fn model_tools_array_offers_exactly_those_names_including_deferred() {
    let registry = deferred_registry();
    let state = AgentState {
        model_tools: Some(ModelToolSelection::List(vec![
            "fetch".to_owned(),
            "read".to_owned(),
            "fetch".to_owned(),
        ])),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["fetch", "read"]);
    // `fetch` is offered now, so only the other deferred tool remains.
    assert_eq!(tool_names(&agent.deferred), ["search"]);
}

#[test]
fn model_tools_edit_loads_deferred_tools_in_add_order() {
    let registry = deferred_registry();
    let state = AgentState {
        model_tools: Some(ModelToolSelection::Edit {
            remove: None,
            add: Some(vec!["fetch".to_owned(), "search".to_owned()]),
        }),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    // Non-deferred tools in registry order, then loaded deferred tools in add
    // order.
    assert_eq!(
        tool_names(&agent.tools),
        ["read", "write", "fetch", "search"]
    );
    assert!(agent.deferred.is_empty());
}

#[test]
fn model_tools_edit_remove_drops_a_declared_tool() {
    let registry = deferred_registry();
    let state = AgentState {
        model_tools: Some(ModelToolSelection::Edit {
            remove: Some(vec!["write".to_owned()]),
            add: None,
        }),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["read"]);
    assert_eq!(tool_names(&agent.deferred), ["search", "fetch"]);
}

#[test]
fn model_tools_add_ignores_non_deferred_and_missing_names() {
    let registry = deferred_registry();
    let state = AgentState {
        model_tools: Some(ModelToolSelection::Edit {
            remove: None,
            add: Some(vec![
                "read".to_owned(),
                "missing".to_owned(),
                "search".to_owned(),
            ]),
        }),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["read", "write", "search"]);
}

#[test]
fn model_tools_remove_wins_over_add() {
    let registry = deferred_registry();
    let state = AgentState {
        model_tools: Some(ModelToolSelection::Edit {
            remove: Some(vec!["search".to_owned()]),
            add: Some(vec!["search".to_owned()]),
        }),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["read", "write"]);
    // Upstream leaves a removed deferred tool in `deferred` (it is enabled and
    // not offered); mirror that.
    assert_eq!(tool_names(&agent.deferred), ["search", "fetch"]);
}

#[test]
fn model_tools_cannot_widen_the_enabled_set() {
    // A model-tools array names a tool that the `tools` filter disabled; it
    // stays disabled, because `modelTools` filters the enabled tools only.
    let registry = deferred_registry();
    let state = AgentState {
        tools: Some(ToolSelection::List(vec!["read".to_owned()])),
        model_tools: Some(ModelToolSelection::List(vec![
            "write".to_owned(),
            "read".to_owned(),
        ])),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(tool_names(&agent.tools), ["read"]);
}

// ── sections and instructions ──────────────────────────────────────────────

#[test]
fn extension_sections_resolve_in_place_and_instructions_come_last() {
    let registry = snapshot(vec![
        Extension::new("roles").with_sections(vec![
            PromptSection::text("role", "You review diffs."),
            PromptSection::text("style", "Be terse.").untagged(),
        ]),
        Extension::new("overrides")
            .with_sections(vec![PromptSection::text("role", "You write code.")]),
    ]);
    let state = AgentState {
        instructions: Some("Follow the repository conventions.".to_owned()),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);

    let keys: Vec<&str> = agent
        .sections
        .iter()
        .map(|section| section.key.as_str())
        .collect();
    assert_eq!(keys, ["role", "style", INSTRUCTIONS_KEY]);
    // The later extension replaced `role` in place, keeping its position.
    assert!(agent.sections[0].tag);

    let input = PromptInput {
        conversation_id: ROOT_CONVERSATION_ID,
        shown: BTreeMap::new(),
    };
    assert_eq!(
        agent.sections[0].render(&input).as_deref(),
        Some("You write code.")
    );
    assert!(!agent.sections[1].tag);
    assert_eq!(
        agent.sections[2].render(&input).as_deref(),
        Some("Follow the repository conventions.")
    );
    assert_eq!(
        agent.instructions.as_deref(),
        Some("Follow the repository conventions.")
    );
}

#[test]
fn instructions_are_the_last_section_even_without_extension_sections() {
    let registry = snapshot(vec![extension("empty", vec![])]);
    let state = AgentState {
        instructions: Some("hello".to_owned()),
        ..Default::default()
    };
    let agent = resolved(&registry, &state);
    assert_eq!(agent.sections.len(), 1);
    assert_eq!(agent.sections[0].key, INSTRUCTIONS_KEY);
    assert!(agent.sections[0].tag);
}

#[test]
fn unreserved_instruction_key_cannot_be_provided_by_an_extension() {
    let registry =
        RegistrySnapshot::build(vec![Extension::new("bad")
            .with_sections(vec![PromptSection::text(INSTRUCTIONS_KEY, "nope")])]);
    assert!(registry.is_err());
}

// ── wrappers ───────────────────────────────────────────────────────────────

#[test]
fn tool_wrappers_apply_and_can_be_reported_when_they_fail() {
    let wrapper: pir::durable::harness::registry::ToolWrapper =
        Arc::new(|tool: &ToolRegistration| Ok(tool.clone().with_description("wrapped")));
    let failing: pir::durable::harness::registry::ToolWrapper =
        Arc::new(|_tool: &ToolRegistration| Err("boom".to_owned()));
    let renaming: pir::durable::harness::registry::ToolWrapper =
        Arc::new(|_tool: &ToolRegistration| Ok(ToolRegistration::new("renamed")));

    let registry = snapshot(vec![
        extension("one", vec![tool("read"), tool("write"), tool("bash")]),
        Extension::new("wrapping").with_wraps(vec![
            Wrap::tool("read", wrapper),
            Wrap::tool("write", failing),
            Wrap::tool("bash", renaming),
        ]),
    ]);

    let mut failures = Vec::new();
    let agent = resolve_agent_with(
        &registry,
        &settings(),
        &AgentState::default(),
        &mut |message: &str, extension: &str| {
            failures.push((extension.to_owned(), message.to_owned()));
        },
    );

    // The successful wrapper replaced the description; the failing and the
    // renaming wrappers dropped their targets.
    assert_eq!(tool_names(&agent.tools), ["read"]);
    assert_eq!(agent.tools[0].description, "wrapped");
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0], ("wrapping".to_owned(), "boom".to_owned()));
    assert!(failures[1].1.contains("renamed"));

    // The silent form applies the same drops.
    let silent = resolve_agent(&registry, &settings(), &AgentState::default());
    assert_eq!(tool_names(&silent.tools), ["read"]);
}

#[test]
fn section_wrappers_apply_in_place() {
    let wrapper: pir::durable::harness::registry::SectionWrapper =
        Arc::new(|section: &PromptSection| {
            let text = format!("{} wrapped", section.key);
            Ok(PromptSection::text(section.key.clone(), text))
        });
    let registry = snapshot(vec![
        Extension::new("one").with_sections(vec![PromptSection::text("role", "plain")]),
        Extension::new("wrapping").with_wraps(vec![Wrap::section("role", wrapper)]),
    ]);
    let agent = resolved(&registry, &AgentState::default());
    let input = PromptInput {
        conversation_id: ConversationId::from_raw(1),
        shown: BTreeMap::new(),
    };
    assert_eq!(agent.sections.len(), 1);
    assert_eq!(
        agent.sections[0].render(&input).as_deref(),
        Some("role wrapped")
    );
}

// ── configure: keep / clear / set for every field ──────────────────────────

fn full_state() -> AgentState {
    AgentState {
        model: Some(ModelRef::new("anthropic", "claude-sonnet")),
        thinking_level: Some(pir::ai::types::ThinkingLevel::High),
        extensions: Some(ExtensionSelection::List(vec!["alpha".to_owned()])),
        tools: Some(ToolSelection::List(vec!["read".to_owned()])),
        model_tools: Some(ModelToolSelection::Edit {
            remove: Some(vec!["write".to_owned()]),
            add: Some(vec!["search".to_owned()]),
        }),
        instructions: Some("be terse".to_owned()),
        cwd: Some("/repo".to_owned()),
    }
}

#[test]
fn configure_keep_leaves_every_field_alone() {
    let state = full_state();
    let next = configure(&state, &AgentChange::default());
    assert_eq!(next, state);
    assert!(AgentChange::default().is_empty());
}

#[test]
fn configure_clear_removes_every_field() {
    let state = full_state();
    let change = AgentChange {
        model: Change::Clear,
        thinking_level: Change::Clear,
        extensions: Change::Clear,
        tools: Change::Clear,
        model_tools: Change::Clear,
        instructions: Change::Clear,
        cwd: Change::Clear,
    };
    assert_eq!(configure(&state, &change), AgentState::default());
}

#[test]
fn configure_set_replaces_every_field() {
    let state = AgentState::default();
    let change = AgentChange {
        model: Change::Set(ModelRef::new("openai", "gpt-5")),
        thinking_level: Change::Set(pir::ai::types::ThinkingLevel::Low),
        extensions: Change::Set(ExtensionEdit::Select(vec![Extension::new("beta")])),
        tools: Change::Set(ToolEdit::Select(vec![ToolRegistration::new("bash")])),
        model_tools: Change::Set(ModelToolsEdit::Edit {
            remove: Some(vec![ToolRegistration::new("write")]),
            add: Some(vec![ToolRegistration::new("fetch")]),
        }),
        instructions: Change::Set("be verbose".to_owned()),
        cwd: Change::Set("/tmp".to_owned()),
    };
    let next = configure(&state, &change);

    assert_eq!(next.model, Some(ModelRef::new("openai", "gpt-5")));
    assert_eq!(
        next.thinking_level,
        Some(pir::ai::types::ThinkingLevel::Low)
    );
    assert_eq!(
        next.extensions,
        Some(ExtensionSelection::List(vec!["beta".to_owned()]))
    );
    assert_eq!(
        next.tools,
        Some(ToolSelection::List(vec!["bash".to_owned()]))
    );
    assert_eq!(
        next.model_tools,
        Some(ModelToolSelection::Edit {
            remove: Some(vec!["write".to_owned()]),
            add: Some(vec!["fetch".to_owned()]),
        })
    );
    assert_eq!(next.instructions.as_deref(), Some("be verbose"));
    assert_eq!(next.cwd.as_deref(), Some("/tmp"));
}

#[test]
fn configure_changes_only_the_named_field() {
    let state = full_state();
    let next = configure(&state, &AgentChange::cwd("/elsewhere"));
    assert_eq!(next.cwd.as_deref(), Some("/elsewhere"));
    assert_eq!(next.model, state.model);
    assert_eq!(next.thinking_level, state.thinking_level);
    assert_eq!(next.extensions, state.extensions);
    assert_eq!(next.tools, state.tools);
    assert_eq!(next.model_tools, state.model_tools);
    assert_eq!(next.instructions, state.instructions);
}

#[test]
fn configure_reduces_registrations_to_names_and_keeps_presence() {
    let next = configure(
        &AgentState::default(),
        &AgentChange {
            extensions: Change::Set(ExtensionEdit::Edit {
                add: Some(vec![Extension::new("alpha"), Extension::new("beta")]),
                remove: None,
            }),
            tools: Change::Set(ToolEdit::Select(vec![
                ToolRegistration::new("read"),
                ToolRegistration::new("write"),
            ])),
            model_tools: Change::Set(ModelToolsEdit::Edit {
                remove: Some(vec![]),
                add: None,
            }),
            ..Default::default()
        },
    );

    assert_eq!(
        next.extensions,
        Some(ExtensionSelection::Edit {
            add: Some(vec!["alpha".to_owned(), "beta".to_owned()]),
            remove: None,
        })
    );
    assert_eq!(
        next.tools,
        Some(ToolSelection::List(vec![
            "read".to_owned(),
            "write".to_owned()
        ]))
    );
    // Present-but-empty `remove` stays present; absent `add` stays absent.
    assert_eq!(
        next.model_tools,
        Some(ModelToolSelection::Edit {
            remove: Some(Vec::new()),
            add: None,
        })
    );
}

#[test]
fn configure_extension_edit_removal_reaches_the_stored_state() {
    let state = full_state();
    let next = configure(
        &state,
        &AgentChange {
            extensions: Change::Set(ExtensionEdit::Edit {
                add: Some(vec![Extension::new("beta")]),
                remove: Some(vec![Extension::new("alpha")]),
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        next.extensions,
        Some(ExtensionSelection::Edit {
            add: Some(vec!["beta".to_owned()]),
            remove: Some(vec!["alpha".to_owned()]),
        })
    );
}

// ── stored JSON shape ──────────────────────────────────────────────────────

#[test]
fn agent_state_serializes_the_upstream_wire_shape() {
    let state = AgentState {
        model: Some(ModelRef::new("anthropic", "claude-sonnet")),
        thinking_level: Some(pir::ai::types::ThinkingLevel::High),
        extensions: Some(ExtensionSelection::List(vec!["alpha".to_owned()])),
        tools: Some(ToolSelection::Edit {
            remove: vec!["bash".to_owned()],
        }),
        model_tools: Some(ModelToolSelection::Edit {
            remove: None,
            add: Some(vec!["search".to_owned()]),
        }),
        instructions: Some("be terse".to_owned()),
        cwd: None,
    };

    assert_eq!(
        serde_json::to_value(&state).expect("serializes"),
        serde_json::json!({
            "model": { "provider": "anthropic", "modelId": "claude-sonnet" },
            "thinkingLevel": "high",
            "extensions": ["alpha"],
            "tools": { "remove": ["bash"] },
            "modelTools": { "add": ["search"] },
            "instructions": "be terse"
        })
    );

    let decoded: AgentState =
        serde_json::from_value(serde_json::to_value(&state).expect("serializes"))
            .expect("round trips");
    assert_eq!(decoded, state);
}

#[test]
fn empty_extensions_edit_serializes_as_an_object() {
    let state = AgentState {
        extensions: Some(ExtensionSelection::inherit()),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&state).expect("serializes"),
        serde_json::json!({ "extensions": {} })
    );
}

// ── registry publication ───────────────────────────────────────────────────

#[test]
fn install_replaces_in_place_and_uninstall_removes() {
    let registry = Registry::new();
    registry
        .install(extension("alpha", vec![tool("read")]))
        .expect("valid");
    registry
        .install(extension("beta", vec![tool("write")]))
        .expect("valid");
    registry
        .install(extension("alpha", vec![tool("bash")]))
        .expect("valid");

    {
        let snapshot = registry.snapshot();
        let names: Vec<&str> = snapshot
            .installed()
            .iter()
            .map(|extension| extension.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(
            tool_names(
                &registry
                    .snapshot()
                    .installed()
                    .first()
                    .expect("alpha")
                    .tools
            ),
            ["bash"]
        );
    }

    registry.uninstall(&Extension::new("alpha"));
    let snapshot = registry.snapshot();
    let names: Vec<&str> = snapshot
        .installed()
        .iter()
        .map(|extension| extension.name.as_str())
        .collect();
    assert_eq!(names, ["beta"]);
}

#[test]
fn install_rejects_duplicate_tools_without_publishing() {
    let registry = Registry::new();
    let error = registry
        .install(extension("bad", vec![tool("read"), tool("read")]))
        .expect_err("duplicate tool");
    assert_eq!(
        error,
        pir::durable::harness::registry::RegistryError::DuplicateTool {
            extension: "bad".to_owned(),
            tool: "read".to_owned(),
        }
    );
    assert!(registry.snapshot().installed().is_empty());
}

#[test]
fn install_rejects_bad_and_reserved_section_keys() {
    let registry = Registry::new();
    let bad = registry
        .install(Extension::new("bad").with_sections(vec![PromptSection::text("Bad Key", "x")]))
        .expect_err("invalid key");
    assert_eq!(
        bad,
        pir::durable::harness::registry::RegistryError::InvalidSectionKey {
            key: "Bad Key".to_owned(),
        }
    );

    let reserved = registry
        .install(
            Extension::new("bad").with_sections(vec![PromptSection::text(INSTRUCTIONS_KEY, "x")]),
        )
        .expect_err("reserved key");
    assert_eq!(
        reserved,
        pir::durable::harness::registry::RegistryError::ReservedSectionKey {
            key: INSTRUCTIONS_KEY.to_owned(),
        }
    );
}

#[test]
fn install_rejects_a_task_that_collides_with_a_builtin() {
    let registry = Registry::new();
    let colliding = pir::durable::tasks::Task {
        definition: pir::durable::tasks::TaskDefinition {
            kind: "pi.generation".to_owned(),
            version: 1,
            phases: vec!["prepare".to_owned()],
        },
    };
    let error = registry
        .install(Extension::new("bad").with_tasks(vec![colliding]))
        .expect_err("task collision");
    assert_eq!(
        error,
        pir::durable::harness::registry::RegistryError::TaskAlreadyInstalled {
            task: "pi.generation".to_owned(),
            extension: "bad".to_owned(),
        }
    );
}

#[test]
fn builtin_tasks_are_always_present() {
    let registry = Registry::new();
    let snapshot = registry.snapshot();
    let names: Vec<&str> = snapshot
        .tasks()
        .iter()
        .map(|task| task.definition.kind.as_str())
        .collect();
    assert_eq!(names, ["pi.generation", "pi.tool", "pi.compaction"]);
    assert!(snapshot.task("pi.tool").is_some());
    assert!(snapshot.task("missing").is_none());
}

#[test]
fn subscribe_notifies_on_publication_and_unsubscribe_stops_it() {
    let registry = Registry::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = {
        let hits = Arc::clone(&hits);
        Arc::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
        })
    };
    let subscription = registry.subscribe(listener);

    registry.install(extension("alpha", vec![])).expect("valid");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    subscription.unsubscribe();
    registry.install(extension("beta", vec![])).expect("valid");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn subscription_unsubscribes_on_drop() {
    let registry = Registry::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = {
        let hits = Arc::clone(&hits);
        Arc::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
        })
    };
    drop(registry.subscribe(listener));

    registry.install(extension("alpha", vec![])).expect("valid");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn hooks_are_listed_per_task_in_extension_order() {
    let registry = snapshot(vec![
        Extension::new("one").with_hooks(vec![HookRegistration::new("pi.tool")]),
        Extension::new("two").with_hooks(vec![
            HookRegistration::new("pi.tool"),
            HookRegistration::new("pi.generation"),
        ]),
    ]);
    let agent = resolved(&registry, &AgentState::default());
    let hooks = pir::durable::harness::agent::hooks_for_task(&agent, "pi.tool");
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].0, "one");
    assert_eq!(hooks[1].0, "two");
    assert_eq!(
        pir::durable::harness::agent::hooks_for_task(&agent, "pi.generation").len(),
        1
    );
}

#[test]
fn section_and_tool_owners_are_the_last_selected_provider_or_wrapper() {
    let wrapper: pir::durable::harness::registry::ToolWrapper =
        Arc::new(|tool: &ToolRegistration| Ok(tool.clone()));
    let registry = snapshot(vec![
        Extension::new("tool-owner").with_tools(vec![tool("read")]),
        Extension::new("wrapper-owner")
            .with_sections(vec![PromptSection::text("role", "x")])
            .with_wraps(vec![Wrap::tool("read", wrapper)]),
    ]);
    let agent = resolved(&registry, &AgentState::default());
    assert_eq!(
        pir::durable::harness::agent::tool_extension(&agent, "read"),
        Some("wrapper-owner")
    );
    assert_eq!(
        pir::durable::harness::agent::section_extension(&agent, "role"),
        Some("wrapper-owner")
    );
}
