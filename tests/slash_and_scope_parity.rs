//! Slash-command long-tail + scoped-models runtime parity tests.
//!
//! Mirrors upstream `core/slash-commands.ts` (`BUILTIN_SLASH_COMMANDS`, 22
//! built-ins), `modes/interactive/session-share.ts` (the secret-gist request
//! shape), and `modes/interactive/components/scoped-models-selector.ts`
//! (the session-only enabled-model set math), all exercised through pir's
//! pure seams in `pir::tui::slash`.

use pir::tui::slash::{
    bug_hint, bug_report_file_name, bug_report_text, builtin_action, BugReportInputs,
    ChangelogEntry, EnabledModels, GistRequest, SlashAction,
};

/// The upstream built-in registry verbatim (`core/slash-commands.ts`).
const UPSTREAM_BUILTINS: [(&str, &str); 22] = [
    ("settings", "Open settings menu"),
    ("model", "Select model (opens selector UI)"),
    ("tree", "Navigate session tree (switch branches)"),
    ("thinking", "Set thinking level"),
    ("scoped-models", "Enable/disable models for Ctrl+P cycling"),
    (
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
    ),
    ("import", "Import and resume a session from a JSONL file"),
    // `cea257439` (D31) rewrote the /share description once the destination
    // became configurable.
    (
        "share",
        "Share session via a Radius artifact or a GitHub gist",
    ),
    ("bug", "Report a bug to the Pi developers"),
    ("copy", "Copy last agent message to clipboard"),
    ("name", "Set session display name"),
    ("session", "Show session info and stats"),
    ("changelog", "Show changelog entries"),
    ("hotkeys", "Show all keyboard shortcuts"),
    ("fork", "Create a new fork from a previous user message"),
    (
        "clone",
        "Duplicate the current session at the current position",
    ),
    ("trust", "Save project trust decision for future sessions"),
    ("login", "Configure provider authentication"),
    ("logout", "Remove provider authentication"),
    ("new", "Start a new session"),
    ("compact", "Manually compact the session context"),
    ("resume", "Resume a different session"),
];

#[test]
fn builtin_slash_commands_cover_the_upstream_registry() {
    for (name, _description) in UPSTREAM_BUILTINS {
        assert!(
            builtin_action(name).is_some(),
            "missing built-in slash command: /{name}"
        );
    }
    // The long-tail commands this batch added.
    assert_eq!(builtin_action("share"), Some(SlashAction::Share));
    assert_eq!(builtin_action("import"), Some(SlashAction::Import));
    assert_eq!(builtin_action("bug"), Some(SlashAction::Bug));
    // Upstream has no /reload / /debug / /clear / /help; pir keeps them as
    // extensions of the surface.
    assert!(builtin_action("no-such-command").is_none());
}

/// `cea257439` (D31): the /share description names both destinations, in the
/// autocomplete registry the TUI renders.
#[test]
fn share_description_matches_upstream() {
    let commands = pir::tui::autocomplete::builtin_commands();
    let share = commands
        .iter()
        .find(|command| command.name == "share")
        .expect("the share row is missing");
    let (_, upstream_description) = UPSTREAM_BUILTINS
        .iter()
        .find(|(name, _)| *name == "share")
        .expect("share is in the upstream registry");
    assert_eq!(share.description.as_deref(), Some(*upstream_description));
}

#[test]
fn slash_actions_quit_only_on_quit() {
    assert!(pir::tui::slash::action_quits(SlashAction::Quit));
    assert!(!pir::tui::slash::action_quits(SlashAction::Share));
    assert!(!pir::tui::slash::action_quits(SlashAction::Import));
}

#[test]
fn gist_request_assembles_a_secret_gist_body() {
    let request: GistRequest =
        pir::tui::slash::gist_request("{\"type\":\"session\"}", "pir session", "tok-1");
    assert_eq!(request.url, "https://api.github.com/gists");
    assert_eq!(request.token, "tok-1");
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["description"], "pir session");
    // Upstream `gh gist create --public=false`.
    assert_eq!(body["public"], serde_json::Value::Bool(false));
    assert_eq!(
        body["files"]["session.jsonl"]["content"],
        "{\"type\":\"session\"}"
    );
    // Exactly one file, named after the upstream temp export.
    let files = body["files"].as_object().unwrap();
    assert_eq!(files.len(), 1);
    assert!(files.contains_key("session.jsonl"));
}

#[test]
fn gist_token_resolution_prefers_gh_token_then_github_token() {
    let resolve = |values: &[(&str, &str)]| {
        pir::tui::slash::gist_token_from(|name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    };
    // GH_TOKEN wins when both are set.
    assert_eq!(
        resolve(&[("GH_TOKEN", "a"), ("GITHUB_TOKEN", "b")]).as_deref(),
        Some("a")
    );
    // GITHUB_TOKEN is the fallback.
    assert_eq!(resolve(&[("GITHUB_TOKEN", "b")]).as_deref(), Some("b"));
    // Blank values do not count (an unset env var reads as empty).
    assert_eq!(resolve(&[("GH_TOKEN", "  ")]), None);
    assert_eq!(resolve(&[]), None);
}

#[test]
fn gist_response_and_viewer_urls() {
    let response = r#"{"html_url":"https://gist.github.com/user/abc123","id":"abc123"}"#;
    let url = pir::tui::slash::gist_url_from_response(response).unwrap();
    assert_eq!(url, "https://gist.github.com/user/abc123");
    assert_eq!(pir::tui::slash::gist_id_from_url(&url), Some("abc123"));
    assert_eq!(
        pir::tui::slash::share_viewer_url("abc123"),
        // Upstream getShareViewerUrl: DEFAULT_SHARE_VIEWER_URL + "#" + id.
        "https://pi.dev/session/#abc123"
    );
    // Malformed / missing payloads yield None instead of panicking.
    assert_eq!(pir::tui::slash::gist_url_from_response("not json"), None);
    assert_eq!(pir::tui::slash::gist_url_from_response("{}"), None);
    assert_eq!(
        pir::tui::slash::gist_id_from_url("https://gist.github.com/"),
        None
    );
}

#[test]
fn import_and_bug_arguments() {
    assert_eq!(
        pir::tui::slash::import_argument("  /tmp/s.jsonl  "),
        Ok("/tmp/s.jsonl".to_owned())
    );
    assert_eq!(
        pir::tui::slash::import_argument(""),
        Err("Usage: /import <path.jsonl>".to_owned())
    );
    assert_eq!(
        bug_hint("  crashes on export "),
        Some("crashes on export".to_owned())
    );
    assert_eq!(bug_hint("   "), None);
}

#[test]
fn bug_report_bundle_lists_metadata_and_disclaimer() {
    let inputs = BugReportInputs {
        pir_version: "0.1.0".to_owned(),
        os: "windows".to_owned(),
        cwd: "C:/repo".to_owned(),
        model_provider: "anthropic".to_owned(),
        model_id: "claude-x".to_owned(),
        thinking_level: "medium".to_owned(),
        session_path: Some("C:/sessions/s.jsonl".to_owned()),
        entry_count: 12,
        generated_at: "1760000000".to_owned(),
        hint: Some("export fails".to_owned()),
    };
    let text = bug_report_text(&inputs);
    assert!(text.contains("pir version: 0.1.0"));
    assert!(text.contains("OS: windows"));
    assert!(text.contains("model: anthropic/claude-x"));
    assert!(text.contains("session storage: C:/sessions/s.jsonl"));
    assert!(text.contains("description: export fails"));
    // No API keys are involved, and nothing uploads automatically.
    assert!(!text.to_lowercase().contains("api key"));
    assert!(text.contains("NOT uploaded automatically"));
    // The file name is filesystem-safe.
    assert_eq!(
        bug_report_file_name("2026-10-09 12:00:00"),
        "bug-report-2026-10-09-12-00-00.md"
    );
}

#[test]
fn changelog_parses_versions_newest_first() {
    let changelog = "\
# Changelog

## [1.2.0] - 2026-10-01

- Added /share

## [1.1.0] - 2026-09-01

- Added /import

## Unreleased

- WIP notes are skipped (no version header)
";
    let entries = pir::tui::slash::parse_changelog(changelog);
    assert_eq!(
        entries,
        vec![
            ChangelogEntry {
                major: 1,
                minor: 2,
                patch: 0,
                content: "## [1.2.0] - 2026-10-01\n\n- Added /share".to_owned(),
            },
            ChangelogEntry {
                major: 1,
                minor: 1,
                patch: 0,
                content: "## [1.1.0] - 2026-09-01\n\n- Added /import".to_owned(),
            },
        ]
    );
    let text = pir::tui::slash::changelog_text(changelog);
    // Upstream reverses the file-order parse (the fixture lists 1.2.0
    // first), so the display shows 1.1.0 before 1.2.0.
    let v12 = text.find("v1.2.0").unwrap();
    let v11 = text.find("v1.1.0").unwrap();
    assert!(v11 < v12);
    // Missing/empty changelog content.
    assert_eq!(
        pir::tui::slash::changelog_text(""),
        "No changelog entries found."
    );
}

fn all_refs() -> Vec<String> {
    vec![
        "anthropic/claude-a".to_owned(),
        "anthropic/claude-b".to_owned(),
        "openai/gpt-a".to_owned(),
    ]
}

#[test]
fn enabled_models_toggle_matches_upstream_selector_semantics() {
    let refs = all_refs();
    // Start from "all enabled": toggling one model leaves its complement.
    let mut enabled = EnabledModels::all();
    assert!(enabled.is_all_enabled());
    assert!(enabled.is_enabled("openai/gpt-a"));
    enabled.toggle("openai/gpt-a", &refs);
    assert!(!enabled.is_all_enabled());
    assert!(!enabled.is_enabled("openai/gpt-a"));
    assert!(enabled.is_enabled("anthropic/claude-a"));
    // Removing every model empties the explicit list but does NOT collapse
    // (upstream toggle removal never calls normalizeEnabled).
    enabled.toggle("anthropic/claude-a", &refs);
    enabled.toggle("anthropic/claude-b", &refs);
    assert!(!enabled.is_all_enabled());
    assert!(enabled.enabled_ids().unwrap().is_empty());
    // Toggling every model back in re-covers the full set and collapses to
    // "all enabled" (upstream normalizeEnabled → null when the list equals
    // every available model).
    enabled.toggle("openai/gpt-a", &refs);
    enabled.toggle("anthropic/claude-a", &refs);
    enabled.toggle("anthropic/claude-b", &refs);
    assert!(enabled.is_all_enabled());
    // And toggling from an explicit list removes without collapsing.
    enabled.toggle("anthropic/claude-a", &refs);
    assert!(!enabled.is_all_enabled());
    enabled.toggle("anthropic/claude-a", &refs);
    assert!(enabled.is_all_enabled());
}

#[test]
fn enabled_models_enable_all_and_clear_all() {
    let refs = all_refs();
    // enable_all on "all enabled" is a no-op.
    let mut enabled = EnabledModels::all();
    enabled.enable_all(None, &refs);
    assert!(enabled.is_all_enabled());
    // Explicit list: enable_all appends and collapses on full coverage.
    let mut enabled = EnabledModels::explicit(vec!["anthropic/claude-a".to_owned()]);
    enabled.enable_all(Some(&refs[1..]), &refs);
    assert!(enabled.is_all_enabled());
    // clear_all from "all enabled" builds the complement.
    let mut enabled = EnabledModels::all();
    enabled.clear_all(None, &refs);
    assert_eq!(enabled.enabled_ids(), Some(&[][..]));
    let mut enabled = EnabledModels::all();
    let anthropic: Vec<String> = refs[..2].to_vec();
    enabled.clear_all(Some(&anthropic), &refs);
    assert_eq!(
        enabled.enabled_ids(),
        Some(&["openai/gpt-a".to_owned()][..])
    );
    // clear_all without targets on an explicit list disables everything.
    let mut enabled = EnabledModels::explicit(vec!["openai/gpt-a".to_owned()]);
    enabled.clear_all(None, &refs);
    assert_eq!(enabled.enabled_ids(), Some(&[][..]));
}

#[test]
fn enabled_models_sorted_ids_keep_the_explicit_order_first() {
    let refs = all_refs();
    let enabled = EnabledModels::explicit(vec![
        "openai/gpt-a".to_owned(),
        "anthropic/claude-b".to_owned(),
    ]);
    assert_eq!(
        enabled.sorted_ids(&refs),
        vec![
            "openai/gpt-a".to_owned(),
            "anthropic/claude-b".to_owned(),
            "anthropic/claude-a".to_owned(),
        ]
    );
    // "All enabled" keeps the registry order.
    assert_eq!(EnabledModels::all().sorted_ids(&refs), refs);
}

#[test]
fn scoped_cycle_target_walks_the_available_scope_with_wraparound() {
    let scope = vec![
        ("anthropic".to_owned(), "a".to_owned()),
        ("openai".to_owned(), "b".to_owned()),
        ("google".to_owned(), "c".to_owned()),
    ];
    let available = scope.clone();
    // Forward + wraparound.
    assert_eq!(
        pir::tui::slash::scoped_cycle_target(&scope, &available, ("anthropic", "a")),
        Some(("openai".to_owned(), "b".to_owned()))
    );
    assert_eq!(
        pir::tui::slash::scoped_cycle_target(&scope, &available, ("google", "c")),
        Some(("anthropic".to_owned(), "a".to_owned()))
    );
    // A current model outside the scope starts from the scope head.
    assert_eq!(
        pir::tui::slash::scoped_cycle_target(&scope, &available, ("mistral", "z")),
        Some(("openai".to_owned(), "b".to_owned()))
    );
}

#[test]
fn scoped_cycle_target_filters_unavailable_and_bails_on_tiny_scopes() {
    let scope = vec![
        ("anthropic".to_owned(), "a".to_owned()),
        ("openai".to_owned(), "b".to_owned()),
        ("google".to_owned(), "c".to_owned()),
    ];
    // "b" lost auth: the cycle skips it (upstream filters by
    // `getAvailableSnapshot`).
    let available = vec![
        ("anthropic".to_owned(), "a".to_owned()),
        ("google".to_owned(), "c".to_owned()),
    ];
    assert_eq!(
        pir::tui::slash::scoped_cycle_target(&scope, &available, ("anthropic", "a")),
        Some(("google".to_owned(), "c".to_owned()))
    );
    // A single remaining model never cycles (upstream `<= 1 → undefined`).
    let available = vec![("anthropic".to_owned(), "a".to_owned())];
    assert_eq!(
        pir::tui::slash::scoped_cycle_target(&scope, &available, ("anthropic", "a")),
        None
    );
    // An empty scope never cycles.
    assert_eq!(
        pir::tui::slash::scoped_cycle_target(&[], &scope, ("anthropic", "a")),
        None
    );
}

#[test]
fn hotkeys_renders_sectioned_rows() {
    let text = pir::tui::slash::hotkeys_text(&[
        (
            "Navigation",
            vec![("up / down".to_owned(), "Move cursor".to_owned())],
        ),
        ("Editing", vec![]),
        (
            "Other",
            vec![("!".to_owned(), "Run bash command".to_owned())],
        ),
    ]);
    assert!(text.starts_with("Keyboard Shortcuts"));
    assert!(text.contains("Navigation\nup / down — Move cursor"));
    // Empty sections are dropped.
    assert!(!text.contains("Editing"));
    assert!(text.contains("Other\n! — Run bash command"));
}
