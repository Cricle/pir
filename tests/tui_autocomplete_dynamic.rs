//! Dynamic slash-command autocomplete sources.
//!
//! Reported bug: typing `/` showed only the hard-coded builtin list;
//! extension/package/prompt-provided commands never appeared, and a reload did
//! not update the candidates. The providers already existed behind
//! [`register_extension_command_provider`] / `register_resource_command_provider`
//! / `register_native_command_provider`; these tests pin that the combined
//! provider reads them **live on every query** (so a reload that replaces the
//! registered provider is visible without rebuilding the TUI).
//!
//! This is its own test binary: each provider is a process-global slot, and a
//! sibling suite registering a different provider would race.

use std::sync::Arc;

use pir::tui::autocomplete::{
    register_extension_command_provider, register_native_command_provider,
    register_resource_command_provider, CombinedAutocompleteProvider, CommandProvider,
    SlashCommand,
};

struct Fixed(Vec<SlashCommand>);

impl CommandProvider for Fixed {
    fn commands(&self) -> Vec<SlashCommand> {
        self.0.clone()
    }
}

fn provider() -> CombinedAutocompleteProvider {
    CombinedAutocompleteProvider::with_builtin_commands(std::env::temp_dir())
}

fn listed_names(provider: &CombinedAutocompleteProvider, line: &str) -> Vec<String> {
    let suggestions = provider
        .suggestions(&[line.to_string()], 0, line.chars().count(), false)
        .unwrap_or_else(|| panic!("`{line}` must offer suggestions"));
    suggestions
        .items
        .into_iter()
        .map(|item| item.value)
        .collect()
}

#[test]
fn extension_commands_appear_and_a_reload_replaces_them() {
    let provider = provider();
    let baseline = listed_names(&provider, "/");
    assert!(baseline.iter().any(|name| name == "model"));
    assert!(!baseline.iter().any(|name| name == "ext:first"));

    // First load.
    register_extension_command_provider(Some(Arc::new(Fixed(vec![SlashCommand::new(
        "ext:first",
        "First extension command",
    )]))));
    let first = listed_names(&provider, "/");
    assert!(
        first.iter().any(|name| name == "ext:first"),
        "an extension-provided command must join the popup: {first:?}"
    );

    // A reload REPLACES the provider; the old command must disappear and the
    // new one must be offered on the very next query (no TUI rebuild).
    register_extension_command_provider(Some(Arc::new(Fixed(vec![SlashCommand::new(
        "ext:second",
        "Reloaded extension command",
    )]))));
    let second = listed_names(&provider, "/");
    assert!(
        second.iter().any(|name| name == "ext:second"),
        "the reloaded command list must be visible: {second:?}"
    );
    assert!(
        !second.iter().any(|name| name == "ext:first"),
        "a command removed by reload must disappear: {second:?}"
    );

    register_extension_command_provider(None);
    let cleared = listed_names(&provider, "/");
    assert!(!cleared.iter().any(|name| name.starts_with("ext:")));
}

#[test]
fn resource_commands_from_skills_and_templates_are_visible() {
    let provider = provider();
    register_resource_command_provider(Some(Arc::new(Fixed(vec![
        SlashCommand::new("skill:deploy", "Deploy the service"),
        SlashCommand::new("review", "Review the current diff").with_argument_hint("<path>"),
    ]))));

    let names = listed_names(&provider, "/");
    assert!(
        names.iter().any(|name| name == "skill:deploy"),
        "a skill command must be offered: {names:?}"
    );
    assert!(
        names.iter().any(|name| name == "review"),
        "a prompt-template command must be offered: {names:?}"
    );

    // Clearing the resource snapshot (a reload that finds no resources) must
    // remove both.
    register_resource_command_provider(None);
    let cleared = listed_names(&provider, "/");
    assert!(!cleared.iter().any(|name| name == "skill:deploy"));
    assert!(!cleared.iter().any(|name| name == "review"));
}

#[test]
fn native_plugin_commands_are_visible() {
    let provider = provider();
    register_native_command_provider(Some(Arc::new(Fixed(vec![SlashCommand::new(
        "todos",
        "Manage the native todo list",
    )]))));

    let names = listed_names(&provider, "/");
    assert!(
        names.iter().any(|name| name == "todos"),
        "a native plugin command must be offered: {names:?}"
    );

    register_native_command_provider(None);
    let cleared = listed_names(&provider, "/");
    assert!(!cleared.iter().any(|name| name == "todos"));
}
