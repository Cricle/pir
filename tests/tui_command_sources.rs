//! Slash-command source precedence (CMDPLUG).
//!
//! The popup lists built-ins, native plugin commands, JS extension commands
//! and skill/prompt-template commands. A submitted `/name` is resolved in that
//! same order, so a later source registering an already-taken name can never
//! execute. These tests pin the ONE merge rule: first source wins, later
//! duplicates are not offered.

use std::path::PathBuf;
use std::sync::Arc;

use pir::tui::autocomplete::{
    merge_command_sources, register_extension_command_provider, CombinedAutocompleteProvider,
    CommandProvider, SlashCommand,
};

fn command(name: &str, description: &str) -> SlashCommand {
    SlashCommand::new(name, description)
}

#[test]
fn merge_keeps_first_source_and_drops_later_duplicates() {
    let merged = merge_command_sources(
        vec![command("model", "builtin model"), command("help", "help")],
        vec![
            command("todos", "native todos"),
            command("model", "native model"),
        ],
        vec![
            command("model", "extension model"),
            command("ext:deploy", "extension deploy"),
            command("todos", "extension todos"),
        ],
        vec![
            command("review", "template"),
            command("ext:deploy", "template collision"),
        ],
    );

    let names: Vec<&str> = merged.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["model", "help", "todos", "ext:deploy", "review"],
        "first source wins; every later duplicate is dropped"
    );
    assert_eq!(
        merged[0].description.as_deref(),
        Some("builtin model"),
        "the built-in description wins for a shadowed name"
    );
    assert_eq!(
        merged[2].description.as_deref(),
        Some("native todos"),
        "native plugin commands beat JS extension commands"
    );
}

struct Fixed(Vec<SlashCommand>);

impl CommandProvider for Fixed {
    fn commands(&self) -> Vec<SlashCommand> {
        self.0.clone()
    }
}

/// End-to-end through the combined provider: a JS extension command named
/// like a built-in is still in `command_defs()` (registration is legal), but
/// the slash menu offers `/model` exactly once and with the built-in row the
/// dispatcher would actually run.
#[test]
fn popup_offers_one_row_per_name_with_builtin_precedence() {
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    register_extension_command_provider(Some(Arc::new(Fixed(vec![
        command("model", "Extension model that can never run"),
        command("cmdplug:probe", "A real extension command"),
    ]))));

    let suggestions = provider
        .suggestions(&["/model".to_string()], 0, 6, false)
        .expect("the built-in model command must match");
    let model_rows: Vec<_> = suggestions
        .items
        .iter()
        .filter(|item| item.value == "model")
        .collect();
    assert_eq!(
        model_rows.len(),
        1,
        "a shadowed extension duplicate must not render: {:?}",
        suggestions.items
    );
    assert_eq!(
        model_rows[0].description.as_deref(),
        // The built-in row renders the upstream hint + description
        // (`<provider/model> — Select model (opens selector UI)`, PIR-BIN-089).
        Some("<provider/model> — Select model (opens selector UI)"),
        "the row must be the built-in the dispatcher runs"
    );

    let all = provider
        .suggestions(&["/".to_string()], 0, 1, false)
        .expect("builtins");
    assert!(all.items.iter().any(|item| item.value == "cmdplug:probe"));

    // Disabled/unloaded extensions must not leave their commands in the menu.
    register_extension_command_provider(None);
    let cleared = provider
        .suggestions(&["/".to_string()], 0, 1, false)
        .expect("builtins");
    assert!(!cleared
        .items
        .iter()
        .any(|item| item.value == "cmdplug:probe"));
}
