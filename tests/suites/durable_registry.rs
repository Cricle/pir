//! `durable::harness::registry` — declarations, validation and publication.
//!
//! Ports upstream `harness/registry.ts` and the registry half of
//! `harness/types.ts`/`harness/define.ts`. This suite covers the stable
//! declarative surface: [`ToolRegistration`] defaults/builders, prompt
//! sections, hook registrations, the serde shapes of [`ToolCaller`] and
//! [`ReplayPolicy`], snapshot validation (duplicate names, section keys, task
//! collisions), install/uninstall order and subscription notification.
//!
//! `HookRegistration` carries opaque `Arc<dyn Any>` handlers and has no
//! `Serialize`/`Deserialize` impl, so only its builder/equality surface is
//! testable here.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::json;

use pir::durable::harness::registry::{
    builtin_tasks, create_registry, Extension, HookRegistration, PromptInput, PromptSection,
    Registry, RegistryError, RegistryReader, RegistrySnapshot, ReplayPolicy, ToolCaller,
    ToolRegistration, Wrap,
};
use pir::durable::harness::settings::ToolExecutionMode;
use pir::durable::ids::ConversationId;
use pir::durable::tasks::{
    define_task as define_meta_task, Task as MetaTask, TaskDefinition as MetaTaskDefinition,
};

// ── helpers ────────────────────────────────────────────────────────────────

/// A registry task definition (the `durable::tasks` metadata shape the
/// registry installs).
fn meta_task(kind: &str, version: i64, phases: &[&str]) -> MetaTask {
    let definition = MetaTaskDefinition::define(
        kind,
        version,
        phases.iter().map(|phase| (*phase).to_owned()).collect(),
    )
    .expect("valid meta task definition");
    define_meta_task(definition)
}

/// Install one extension holding sections named `keys`.
fn install_sections(keys: &[&str]) -> Result<(), RegistryError> {
    let registry = Registry::new();
    let sections: Vec<PromptSection> = keys
        .iter()
        .map(|key| PromptSection::text(*key, "text"))
        .collect();
    registry.install(Extension::new("sections").with_sections(sections))
}

fn extension_names(snapshot: &RegistrySnapshot) -> Vec<&str> {
    snapshot
        .installed()
        .iter()
        .map(|extension| extension.name.as_str())
        .collect()
}

fn tool_names(snapshot: &RegistrySnapshot) -> Vec<(&str, &str)> {
    snapshot
        .tools()
        .iter()
        .map(|(extension, tool)| (extension.name.as_str(), tool.name.as_str()))
        .collect()
}

fn task_names(snapshot: &RegistrySnapshot) -> Vec<&str> {
    snapshot
        .tasks()
        .iter()
        .map(|task| task.definition.kind.as_str())
        .collect()
}

// ── ToolRegistration ───────────────────────────────────────────────────────

#[test]
fn tool_registration_defaults_allow_every_caller() {
    let tool = ToolRegistration::new("read");

    assert_eq!(tool.name, "read");
    assert_eq!(tool.description, "");
    assert_eq!(tool.parameters, json!({}));
    assert_eq!(tool.callers, None);
    assert!(!tool.deferred);
    assert_eq!(tool.replay, ReplayPolicy::Unsafe);
    assert_eq!(tool.execution_mode, None);
    assert!(tool.implementation.is_none());

    // An unset `callers` means both, matching upstream's default.
    assert!(tool.is_callable_by(ToolCaller::Model));
    assert!(tool.is_callable_by(ToolCaller::Tools));
}

#[test]
fn tool_registration_builders_set_each_declaration() {
    let tool = ToolRegistration::new("read")
        .with_description("Read a file")
        .with_parameters(json!({ "type": "object" }))
        .with_callers(vec![ToolCaller::Model])
        .deferred()
        .replay_safe()
        .with_execution_mode(ToolExecutionMode::Sequential);

    assert_eq!(tool.name, "read");
    assert_eq!(tool.description, "Read a file");
    assert_eq!(tool.parameters, json!({ "type": "object" }));
    assert_eq!(tool.callers, Some(vec![ToolCaller::Model]));
    assert!(tool.deferred);
    assert_eq!(tool.replay, ReplayPolicy::Safe);
    assert_eq!(tool.execution_mode, Some(ToolExecutionMode::Sequential));
    assert!(tool.is_callable_by(ToolCaller::Model));
    assert!(!tool.is_callable_by(ToolCaller::Tools));

    let nested_only = ToolRegistration::new("spawn").with_callers(vec![ToolCaller::Tools]);
    assert!(!nested_only.is_callable_by(ToolCaller::Model));
    assert!(nested_only.is_callable_by(ToolCaller::Tools));

    let nobody = ToolRegistration::new("nobody").with_callers(Vec::new());
    assert!(!nobody.is_callable_by(ToolCaller::Model));
    assert!(!nobody.is_callable_by(ToolCaller::Tools));
}

#[test]
fn tool_registration_equality_ignores_the_opaque_implementation() {
    let bare = ToolRegistration::new("read");
    let implementation: pir::durable::harness::registry::BoxedTool = Arc::new(7u32);
    let with_impl = bare.clone().with_implementation(implementation);

    assert!(with_impl.implementation.is_some());
    assert_eq!(
        bare, with_impl,
        "PartialEq compares the declaration, not the opaque handle"
    );

    assert_ne!(bare, bare.clone().with_description("different"));
    assert_ne!(bare, bare.clone().deferred());
    assert_ne!(bare, bare.clone().replay_safe());
}

// ── PromptSection ──────────────────────────────────────────────────────────

#[test]
fn prompt_section_text_renders_constant_text_and_is_tagged_by_default() {
    let section = PromptSection::text("style", "Be terse.");

    assert_eq!(section.key, "style");
    assert!(section.tag);
    assert_eq!(
        section.render(&PromptInput::default()),
        Some("Be terse.".to_owned())
    );

    let untagged = PromptSection::text("style", "Be terse.").untagged();
    assert!(!untagged.tag);
    assert_eq!(
        untagged.render(&PromptInput::default()),
        Some("Be terse.".to_owned())
    );
}

#[test]
fn prompt_section_empty_text_renders_some_empty_string() {
    let section = PromptSection::text("empty", "");

    assert_eq!(section.render(&PromptInput::default()), Some(String::new()));
}

#[test]
fn prompt_section_renderer_receives_input_and_may_omit_the_section() {
    let section = PromptSection::new(
        "shown",
        Arc::new(|input: &PromptInput| {
            input
                .shown
                .get("style")
                .map(|text| format!("shown: {text}"))
        }),
    );

    // Empty input: the renderer omits the section.
    assert_eq!(section.render(&PromptInput::default()), None);

    let mut shown = BTreeMap::new();
    shown.insert("style".to_owned(), "terse".to_owned());
    let input = PromptInput {
        conversation_id: ConversationId::from_raw(4),
        shown,
    };
    assert_eq!(section.render(&input), Some("shown: terse".to_owned()));
}

#[test]
fn prompt_section_equality_compares_key_and_tag_not_the_renderer() {
    assert_eq!(
        PromptSection::text("style", "first"),
        PromptSection::text("style", "second")
    );
    assert_ne!(
        PromptSection::text("style", "text"),
        PromptSection::text("style", "text").untagged()
    );
    assert_ne!(
        PromptSection::text("style", "text"),
        PromptSection::text("other", "text")
    );
}

// ── hooks, callers, replay, wraps ──────────────────────────────────────────

#[test]
fn hook_registration_builds_in_order_and_compares_names() {
    let hooks = HookRegistration::new("pi.tool")
        .handler("before", Arc::new(1u8))
        .handler("after", Arc::new(2u8));

    assert_eq!(hooks.task, "pi.tool");
    assert_eq!(
        hooks.handlers.keys().cloned().collect::<Vec<_>>(),
        vec!["after".to_owned(), "before".to_owned()]
    );

    // Equality compares the task and the handler names; the opaque handler
    // values are not compared.
    let same_names = HookRegistration::new("pi.tool")
        .handler("before", Arc::new(9u8))
        .handler("after", Arc::new(8u8));
    assert_eq!(hooks, same_names);

    assert_ne!(
        hooks,
        HookRegistration::new("pi.tool").handler("before", Arc::new(1u8))
    );
    assert_ne!(hooks, HookRegistration::new("pi.generation"));

    assert_eq!(HookRegistration::default(), HookRegistration::new(""));
}

#[test]
fn tool_caller_serde_is_the_lowercase_tag_array() {
    assert_eq!(
        serde_json::to_value(ToolCaller::Model).expect("serialize"),
        json!("model")
    );
    assert_eq!(
        serde_json::to_value(ToolCaller::Tools).expect("serialize"),
        json!("tools")
    );
    assert_eq!(
        serde_json::from_value::<ToolCaller>(json!("model")).expect("deserialize"),
        ToolCaller::Model
    );
    assert_eq!(
        serde_json::from_value::<ToolCaller>(json!("tools")).expect("deserialize"),
        ToolCaller::Tools
    );
    assert!(serde_json::from_value::<ToolCaller>(json!("both")).is_err());
}

#[test]
fn replay_policy_serde_and_default_are_unsafe() {
    assert_eq!(ReplayPolicy::default(), ReplayPolicy::Unsafe);
    assert_eq!(
        serde_json::to_value(ReplayPolicy::Safe).expect("serialize"),
        json!("safe")
    );
    assert_eq!(
        serde_json::to_value(ReplayPolicy::Unsafe).expect("serialize"),
        json!("unsafe")
    );
    assert_eq!(
        serde_json::from_value::<ReplayPolicy>(json!("safe")).expect("deserialize"),
        ReplayPolicy::Safe
    );
    assert!(serde_json::from_value::<ReplayPolicy>(json!("never")).is_err());
}

#[test]
fn wrap_equality_compares_the_target_not_the_callback() {
    let tool_left = Wrap::tool("read", Arc::new(|tool: &ToolRegistration| Ok(tool.clone())));
    let tool_right = Wrap::tool("read", Arc::new(|tool: &ToolRegistration| Ok(tool.clone())));
    assert_eq!(tool_left, tool_right);
    assert_ne!(
        tool_left,
        Wrap::tool(
            "write",
            Arc::new(|tool: &ToolRegistration| Ok(tool.clone()))
        )
    );

    let section_left = Wrap::section(
        "style",
        Arc::new(|section: &PromptSection| Ok(section.clone())),
    );
    let section_right = Wrap::section(
        "style",
        Arc::new(|section: &PromptSection| Ok(section.clone())),
    );
    assert_eq!(section_left, section_right);
    assert_ne!(tool_left, section_left);
}

#[test]
fn extension_builders_hold_every_declaration() {
    let extension = Extension::new("demo")
        .with_tools(vec![ToolRegistration::new("read")])
        .with_sections(vec![PromptSection::text("style", "terse")])
        .with_hooks(vec![HookRegistration::new("pi.tool")])
        .with_wraps(vec![Wrap::tool(
            "read",
            Arc::new(|tool: &ToolRegistration| Ok(tool.clone())),
        )])
        .with_tasks(vec![meta_task("custom.task", 1, &["run"])]);

    assert_eq!(extension.name, "demo");
    assert_eq!(extension.tools.len(), 1);
    assert_eq!(extension.sections.len(), 1);
    assert_eq!(extension.hooks.len(), 1);
    assert_eq!(extension.wraps.len(), 1);
    assert_eq!(extension.tasks.len(), 1);
    assert_eq!(extension.tasks[0].definition.kind, "custom.task");

    assert_eq!(
        Extension::new("demo"),
        Extension {
            name: "demo".to_owned(),
            ..Extension::default()
        }
    );
    assert_eq!(Extension::default().name, "");
    assert!(Extension::default().tools.is_empty());
}

// ── built-ins and snapshots ────────────────────────────────────────────────

#[test]
fn an_empty_registry_holds_the_three_builtin_tasks() {
    let registry = create_registry();
    let snapshot = registry.snapshot();

    assert!(snapshot.installed().is_empty());
    assert_eq!(
        task_names(&snapshot),
        vec!["pi.generation", "pi.tool", "pi.compaction"]
    );

    let tasks = snapshot.tasks();
    assert_eq!(tasks[0].definition.version, 1);
    assert_eq!(
        tasks[0].definition.phases,
        vec![
            "prepare".to_owned(),
            "request".to_owned(),
            "retry".to_owned(),
            "poll".to_owned(),
            "tools".to_owned()
        ]
    );
    assert_eq!(tasks[1].definition.version, 2);
    assert_eq!(
        tasks[1].definition.phases,
        vec!["call".to_owned(), "execute".to_owned()]
    );
    assert_eq!(tasks[2].definition.version, 1);
    assert_eq!(
        tasks[2].definition.phases,
        vec![
            "select".to_owned(),
            "summarize".to_owned(),
            "retry".to_owned()
        ]
    );

    assert!(snapshot.task("pi.generation").is_some());
    assert!(snapshot.task("pi.tool").is_some());
    assert!(snapshot.task("pi.compaction").is_some());
    assert!(snapshot.task("missing").is_none());

    assert_eq!(builtin_tasks().len(), 3);
    assert!(RegistrySnapshot::empty().installed().is_empty());
}

#[test]
fn install_preserves_extension_and_declaration_order() {
    let registry = Registry::new();
    registry
        .install(
            Extension::new("alpha")
                .with_tools(vec![
                    ToolRegistration::new("a1"),
                    ToolRegistration::new("a2"),
                ])
                .with_sections(vec![PromptSection::text("a-style", "alpha")]),
        )
        .expect("installs alpha");
    registry
        .install(
            Extension::new("beta")
                .with_tools(vec![ToolRegistration::new("b1")])
                .with_sections(vec![PromptSection::text("b-style", "beta")]),
        )
        .expect("installs beta");

    let snapshot = registry.snapshot();

    assert_eq!(extension_names(&snapshot), vec!["alpha", "beta"]);
    assert_eq!(snapshot.extension("alpha").expect("alpha").tools.len(), 2);
    assert!(snapshot.extension("missing").is_none());

    assert_eq!(
        tool_names(&snapshot),
        vec![("alpha", "a1"), ("alpha", "a2"), ("beta", "b1")]
    );

    let sections: Vec<(&str, &str)> = snapshot
        .sections()
        .iter()
        .map(|(extension, section)| (extension.name.as_str(), section.key.as_str()))
        .collect();
    assert_eq!(sections, vec![("alpha", "a-style"), ("beta", "b-style")]);
}

#[test]
fn install_replaces_a_same_name_extension_in_place() {
    let registry = Registry::new();
    registry
        .install(Extension::new("alpha").with_tools(vec![ToolRegistration::new("a1")]))
        .expect("installs alpha");
    registry
        .install(Extension::new("beta").with_tools(vec![ToolRegistration::new("b1")]))
        .expect("installs beta");
    registry
        .install(Extension::new("alpha").with_tools(vec![ToolRegistration::new("a2")]))
        .expect("replaces alpha");

    let snapshot = registry.snapshot();

    assert_eq!(extension_names(&snapshot), vec!["alpha", "beta"]);
    assert_eq!(tool_names(&snapshot), vec![("alpha", "a2"), ("beta", "b1")]);
    assert_eq!(snapshot.extension("alpha").expect("alpha").tools.len(), 1);
}

#[test]
fn uninstall_removes_by_name_and_a_reinstall_appends() {
    let registry = Registry::new();
    registry
        .install(Extension::new("alpha"))
        .expect("installs alpha");
    registry
        .install(Extension::new("beta"))
        .expect("installs beta");

    registry.uninstall_by_name("alpha");
    assert_eq!(extension_names(&registry.snapshot()), vec!["beta"]);

    // A reinstall appends rather than restoring the old position.
    registry
        .install(Extension::new("alpha"))
        .expect("reinstalls alpha");
    assert_eq!(extension_names(&registry.snapshot()), vec!["beta", "alpha"]);

    // `uninstall` removes whichever object carries the name.
    registry.uninstall(&Extension::new("beta"));
    assert_eq!(extension_names(&registry.snapshot()), vec!["alpha"]);

    registry.uninstall_by_name("ghost");
    assert_eq!(extension_names(&registry.snapshot()), vec!["alpha"]);
}

#[test]
fn duplicate_tools_are_rejected_and_nothing_is_published() {
    let registry = Registry::new();
    let duplicate = Extension::new("dup")
        .with_tools(vec![ToolRegistration::new("x"), ToolRegistration::new("x")]);

    assert_eq!(
        registry.install(duplicate).unwrap_err(),
        RegistryError::DuplicateTool {
            extension: "dup".to_owned(),
            tool: "x".to_owned(),
        }
    );
    assert!(registry.snapshot().installed().is_empty());

    // A later failed install leaves the published snapshot untouched.
    registry
        .install(Extension::new("ok").with_tools(vec![ToolRegistration::new("x")]))
        .expect("installs ok");
    assert!(registry
        .install(
            Extension::new("dup")
                .with_tools(vec![ToolRegistration::new("y"), ToolRegistration::new("y"),])
        )
        .is_err());
    assert_eq!(extension_names(&registry.snapshot()), vec!["ok"]);
}

#[test]
fn section_keys_follow_the_upstream_pattern() {
    for key in [
        "Style", "1style", "_style", "style!", "style up", "styleA", "ä",
    ] {
        assert_eq!(
            install_sections(&[key]).unwrap_err(),
            RegistryError::InvalidSectionKey {
                key: key.to_owned()
            },
            "key {key:?} must be invalid"
        );
    }
    assert_eq!(
        install_sections(&[""]).unwrap_err(),
        RegistryError::InvalidSectionKey { key: String::new() }
    );

    assert_eq!(
        install_sections(&["instructions"]).unwrap_err(),
        RegistryError::ReservedSectionKey {
            key: "instructions".to_owned()
        }
    );

    assert_eq!(
        install_sections(&["style", "style"]).unwrap_err(),
        RegistryError::DuplicateSection {
            extension: "sections".to_owned(),
            key: "style".to_owned(),
        }
    );

    assert!(install_sections(&["a", "a1", "a_b-c"]).is_ok());

    // Upstream's `SECTION_KEY` is `^[a-z][a-z0-9_-]*$` with no length cap; the
    // Rust doc comment saying "nine characters or fewer" is stale.
    assert!(install_sections(&["a-very-long-section-key"]).is_ok());
}

#[test]
fn tasks_are_installed_after_the_builtins_in_install_order() {
    let registry = Registry::new();
    registry
        .install(Extension::new("custom").with_tasks(vec![
            meta_task("custom.one", 1, &["run"]),
            meta_task("custom.two", 2, &["run", "stop"]),
        ]))
        .expect("installs custom tasks");

    let snapshot = registry.snapshot();

    assert_eq!(
        task_names(&snapshot),
        vec![
            "pi.generation",
            "pi.tool",
            "pi.compaction",
            "custom.one",
            "custom.two"
        ]
    );
    let custom_two = snapshot.task("custom.two").expect("custom.two");
    assert_eq!(custom_two.definition.version, 2);
    assert_eq!(
        custom_two.definition.phases,
        vec!["run".to_owned(), "stop".to_owned()]
    );
}

#[test]
fn task_name_collisions_are_rejected_and_publish_nothing() {
    let registry = Registry::new();
    registry
        .install(Extension::new("custom").with_tasks(vec![meta_task("custom.one", 1, &["run"])]))
        .expect("installs custom");

    // Collision with an installed extension's task.
    assert_eq!(
        registry
            .install(Extension::new("other").with_tasks(vec![meta_task("custom.one", 3, &["run"])]))
            .unwrap_err(),
        RegistryError::TaskAlreadyInstalled {
            task: "custom.one".to_owned(),
            extension: "other".to_owned(),
        }
    );

    // Collision with a built-in task.
    assert_eq!(
        registry
            .install(Extension::new("clash").with_tasks(vec![meta_task("pi.tool", 9, &["call"])]))
            .unwrap_err(),
        RegistryError::TaskAlreadyInstalled {
            task: "pi.tool".to_owned(),
            extension: "clash".to_owned(),
        }
    );

    assert_eq!(extension_names(&registry.snapshot()), vec!["custom"]);
    assert_eq!(registry.snapshot().tasks().len(), 4);

    // Uninstalling the extension removes its task again.
    registry.uninstall_by_name("custom");
    assert!(registry.snapshot().task("custom.one").is_none());
    assert_eq!(registry.snapshot().tasks().len(), 3);
}

#[test]
fn two_tasks_with_the_same_name_inside_one_extension_are_rejected() {
    let registry = Registry::new();

    assert_eq!(
        registry
            .install(Extension::new("twice").with_tasks(vec![
                meta_task("dup.task", 1, &["a"]),
                meta_task("dup.task", 2, &["b"]),
            ]))
            .unwrap_err(),
        RegistryError::TaskAlreadyInstalled {
            task: "dup.task".to_owned(),
            extension: "twice".to_owned(),
        }
    );
    assert!(registry.snapshot().installed().is_empty());
}

#[test]
fn snapshot_build_validates_directly() {
    let duplicate = Extension::new("dup")
        .with_tools(vec![ToolRegistration::new("x"), ToolRegistration::new("x")]);
    assert_eq!(
        RegistrySnapshot::build(vec![duplicate]).unwrap_err(),
        RegistryError::DuplicateTool {
            extension: "dup".to_owned(),
            tool: "x".to_owned(),
        }
    );

    let builtin_collision =
        Extension::new("clash").with_tasks(vec![meta_task("pi.generation", 1, &["prepare"])]);
    assert_eq!(
        RegistrySnapshot::build(vec![builtin_collision]).unwrap_err(),
        RegistryError::TaskAlreadyInstalled {
            task: "pi.generation".to_owned(),
            extension: "clash".to_owned(),
        }
    );

    let valid = RegistrySnapshot::build(vec![Extension::new("ok")]).expect("valid");
    assert_eq!(extension_names(&valid), vec!["ok"]);
}

// ── subscriptions ──────────────────────────────────────────────────────────

#[test]
fn subscribers_are_notified_after_publication_and_dropping_unsubscribes() {
    let registry = Registry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let listener = {
        let count = Arc::clone(&count);
        Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
    };

    let subscription = registry.subscribe(listener);
    assert!(subscription.is_active());

    registry
        .install(Extension::new("a"))
        .expect("installs alpha");
    assert_eq!(count.load(Ordering::SeqCst), 1);

    registry.uninstall_by_name("a");
    assert_eq!(count.load(Ordering::SeqCst), 2);

    let invalid = Extension::new("bad")
        .with_tools(vec![ToolRegistration::new("x"), ToolRegistration::new("x")]);
    assert!(registry.install(invalid).is_err());
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "a failed install publishes nothing"
    );

    drop(subscription);
    registry
        .install(Extension::new("b"))
        .expect("installs beta");
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "a dropped subscription stops listening"
    );
}

#[test]
fn explicit_unsubscribe_stops_notifications() {
    let registry = Registry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let listener = {
        let count = Arc::clone(&count);
        Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
    };

    let subscription = registry.subscribe(listener);
    assert!(subscription.is_active());
    subscription.unsubscribe();

    registry
        .install(Extension::new("a"))
        .expect("installs alpha");
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
