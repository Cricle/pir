//! Interactive startup settings diagnostics (upstream interactive `showWarning`).
//!
//! The CLI collects and deduplicates settings load failures once, then hands
//! the list to `tui::run`. The TUI turns each diagnostic into a warning chat
//! notice before the crash notice; the notice renderer supplies the
//! `Warning: ` prefix. These tests pin both the entry ordering and the
//! rendered rows without starting a terminal.

use pir::config::settings::{SettingsDiagnostic, SettingsDiagnosticType};
use pir::tui::app::{ChatContainer, ChatEntry, NoticeKind};
use pir::tui::core::Component;
use pir::tui::terminal::{push_resource_diagnostic_notices, push_settings_diagnostic_notices};

fn warning(message: &str) -> SettingsDiagnostic {
    SettingsDiagnostic {
        type_: SettingsDiagnosticType::Warning,
        message: message.to_string(),
    }
}

#[test]
fn each_diagnostic_becomes_a_warning_notice_in_collection_order() {
    let mut entries = Vec::new();
    push_settings_diagnostic_notices(
        &mut entries,
        vec![
            warning("Invalid settings file /a/settings.json: bad"),
            warning("Invalid project settings: worse"),
        ],
    );

    assert_eq!(entries.len(), 2, "{entries:?}");
    for (entry, expected) in entries.iter().zip([
        "Invalid settings file /a/settings.json: bad",
        "Invalid project settings: worse",
    ]) {
        match entry {
            ChatEntry::Notice { kind, text } => {
                assert_eq!(*kind, NoticeKind::Warning);
                assert_eq!(text, expected);
            }
            other => panic!("expected a warning notice, got {other:?}"),
        }
    }
}

#[test]
fn empty_diagnostics_add_no_entries() {
    let mut entries = vec![ChatEntry::Notice {
        kind: NoticeKind::Warning,
        text: "crash notice".to_string(),
    }];
    push_settings_diagnostic_notices(&mut entries, Vec::new());
    assert_eq!(entries.len(), 1, "{entries:?}");
}

#[test]
fn rendered_rows_prefix_warnings_and_keep_order() {
    let mut entries = Vec::new();
    push_settings_diagnostic_notices(
        &mut entries,
        vec![
            warning("first settings failure"),
            warning("second settings failure"),
        ],
    );

    let mut chat = ChatContainer::new();
    chat.set_entries(&entries);
    let rows = chat.render(80);
    let first = rows
        .iter()
        .position(|row| row.contains("Warning: first settings failure"))
        .expect("the first diagnostic must render as a warning row");
    let second = rows
        .iter()
        .position(|row| row.contains("Warning: second settings failure"))
        .expect("the second diagnostic must render as a warning row");
    assert!(
        first < second,
        "diagnostics must render in collection order: {rows:#?}"
    );
}

// ---------------------------------------------------------------------------
// Resource-discovery diagnostics (skills / prompt-template collisions)
// ---------------------------------------------------------------------------

#[test]
fn each_resource_diagnostic_becomes_a_warning_notice_in_order() {
    let mut entries = Vec::new();
    push_resource_diagnostic_notices(
        &mut entries,
        &[
            "name \"alpha\" collision (kept /a, ignored /b)".to_string(),
            "warning: bad frontmatter (/c/SKILL.md)".to_string(),
        ],
    );

    assert_eq!(entries.len(), 2, "{entries:?}");
    for (entry, expected) in entries.iter().zip([
        "name \"alpha\" collision (kept /a, ignored /b)",
        "warning: bad frontmatter (/c/SKILL.md)",
    ]) {
        match entry {
            ChatEntry::Notice { kind, text } => {
                assert_eq!(*kind, NoticeKind::Warning);
                assert_eq!(text, expected);
            }
            other => panic!("expected a warning notice, got {other:?}"),
        }
    }
}

#[test]
fn resource_diagnostics_are_bounded_and_the_remainder_is_summarized() {
    let diagnostics: Vec<String> = (0..25).map(|index| format!("warning #{index}")).collect();
    let mut entries = Vec::new();
    push_resource_diagnostic_notices(&mut entries, &diagnostics);

    // 20 individual notices plus one summary for the remaining 5.
    assert_eq!(entries.len(), 21, "{entries:?}");
    let ChatEntry::Notice { kind, text } = entries.last().expect("summary notice") else {
        panic!("expected a notice");
    };
    assert_eq!(*kind, NoticeKind::Warning);
    assert!(
        text.contains('5'),
        "summary must count the remainder: {text}"
    );
}

#[test]
fn an_exact_bound_adds_no_summary_notice() {
    let diagnostics: Vec<String> = (0..20).map(|index| format!("warning #{index}")).collect();
    let mut entries = Vec::new();
    push_resource_diagnostic_notices(&mut entries, &diagnostics);
    assert_eq!(entries.len(), 20, "{entries:?}");
}

#[test]
fn no_resource_diagnostics_add_no_entries() {
    let mut entries = vec![ChatEntry::Notice {
        kind: NoticeKind::Warning,
        text: "settings notice".to_string(),
    }];
    push_resource_diagnostic_notices(&mut entries, &[]);
    assert_eq!(entries.len(), 1, "{entries:?}");
}
