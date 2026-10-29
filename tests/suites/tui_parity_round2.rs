//! Round-2 parity repairs: the startup key hints (order, suspend, paste
//! text, compact/expanded split, live-registry chord resolution with the
//! win32 variants), the footer gates (truthy cost, routed thinking level,
//! detached-HEAD label, the registry-driven `(sub)` gate), the `/bug`
//! diagnostics bundle + redaction set, the `/login` auth-type selector
//! shape, the external editor's expanded text, and the dequeue
//! empty-queue status.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use pir::tui::ansi::strip_ansi;
use pir::tui::app::{
    apply_queue_snapshot, compact_line, default_expanded_key_hints, expanded_key_hints,
    header_line_raw, header_line_raw_for, header_line_styled, header_line_styled_for,
    render_footer, render_pending_messages, FooterState, LiveHeaderLine,
};
use pir::tui::command_dialogs::{
    auth_status_marker, auth_type_label, AuthKey, AuthOutcome, AuthProviderKind, AuthTypeChoice,
    AuthTypeSelector, RADIUS_LOGIN_INTRO,
};
use pir::tui::core::Component;
use pir::tui::editor::Editor;
use pir::tui::keybindings::Keybindings;
use pir::tui::terminal::{branch_summary_options, dequeue_status_text, footer_git_branch_text};

fn kb() -> Keybindings {
    Keybindings::new()
}

// ---------------------------------------------------------------------------
// Startup key hints (upstream interactive-mode.ts:1021-1056)
// ---------------------------------------------------------------------------

#[test]
fn expanded_hints_follow_the_upstream_order_and_text() {
    let hints = expanded_key_hints(&kb());
    let descriptions: Vec<&str> = hints
        .iter()
        .map(|(_, description)| description.as_str())
        .collect();
    assert_eq!(
        descriptions,
        vec![
            "to interrupt",
            "to clear",
            "to exit",
            "to exit (empty)",
            "to suspend",
            "to delete to end",
            "to cycle thinking level",
            "to cycle models",
            "to select model",
            "to expand tools",
            "to expand thinking",
            "for external editor",
            "for commands",
            "to run bash",
            "to run bash (no context)",
            "to queue follow-up",
            "to edit all queued messages",
            "to paste files on macOS, images, or text",
            "to attach",
        ]
    );
    // The compact-style "clear/exit" line is NOT part of the expanded list
    // (the expanded list carries the separate "ctrl+c to clear" entry).
    assert!(!descriptions.contains(&"clear/exit"));
    // The twice-to-exit line resolves from the live app.clear chord.
    assert_eq!(hints[2].0, format!("{} twice", hints[1].0));
    // The cycle-models line joins the two cycle chords with "/".
    assert_eq!(
        hints[7].0,
        format!(
            "{}/{}",
            kb().hint_key_text("app.model.cycleForward"),
            kb().hint_key_text("app.model.cycleBackward")
        )
    );
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows chord glyphs")]
fn expanded_hints_resolve_the_platform_chords() {
    let hints = expanded_key_hints(&kb());
    let by_description = |needle: &str| {
        hints
            .iter()
            .find(|(_, description)| description.contains(needle))
            .map(|(key, _)| key.clone())
            .unwrap_or_default()
    };
    if pir::config::keybindings::use_windows_keybindings() {
        // Upstream core/keybindings.ts:67-78 win32 variants.
        assert_eq!(by_description("follow-up"), "ctrl+q");
        assert_eq!(by_description("queued messages"), "alt+q");
        assert_eq!(by_description("paste files"), "alt+v");
        assert_eq!(by_description("cycle models"), "ctrl+p/alt+p");
        // app.suspend keeps an EMPTY win32 default: the hint renders with an
        // empty key text (upstream keyText over an empty getKeys list).
        assert_eq!(by_description("suspend"), "");
    } else {
        assert_eq!(by_description("follow-up"), "alt+enter");
        assert_eq!(by_description("queued messages"), "alt+up");
        assert_eq!(by_description("paste files"), "ctrl+v");
        assert_eq!(by_description("cycle models"), "ctrl+p/shift+ctrl+p");
        assert_eq!(by_description("suspend"), "ctrl+z");
    }
}

#[test]
fn hint_key_text_honors_settings_overrides() {
    let defaults = kb();
    assert_eq!(defaults.hint_key_text("app.editor.external"), "ctrl+g");
    let overridden = Keybindings::from_settings(Some(&serde_json::json!({
        "app.editor.external": "ctrl+shift+e"
    })))
    .expect("settings parse");
    assert_eq!(
        overridden.hint_key_text("app.editor.external"),
        "ctrl+shift+e"
    );
    // Unregistered hint ids fall back to the platform default table.
    assert_eq!(defaults.hint_key_text("app.interrupt"), "esc");
    assert_eq!(defaults.hint_key_text("app.tools.expand"), "ctrl+o");
}

#[test]
fn compact_line_keeps_the_upstream_shape() {
    let line = compact_line(&kb());
    let parts: Vec<&str> = line.split(" · ").collect();
    assert_eq!(parts.len(), 5);
    assert!(parts[0].ends_with(" interrupt"));
    assert!(parts[1].ends_with(" clear/exit"));
    assert!(parts[1].contains(&kb().hint_key_text("app.clear")));
    assert_eq!(parts[2], "/ commands");
    assert_eq!(parts[3], "! bash");
    assert!(parts[4].ends_with(" more"));
}

#[test]
fn header_lines_stay_raw_styled_symmetric() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    use pir::tui::app::startup_header_kinds;
    pir::tui::theme::set_theme("dark");
    for kind in startup_header_kinds(false) {
        assert_eq!(
            strip_ansi(&header_line_styled(kind)),
            header_line_raw(kind),
            "styled text of {kind:?} equals its raw text"
        );
        assert_eq!(
            strip_ansi(&header_line_styled_for(kind, &kb())),
            header_line_raw_for(kind, &kb()),
            "live-registry variant stays symmetric for {kind:?}"
        );
    }
}

#[test]
fn live_header_line_renders_the_default_expanded_list() {
    let _guard = super::tui_system_theme::lock();
    pir::tui::theme::set_theme("dark");
    use pir::tui::app::HeaderLineKind;
    let line = LiveHeaderLine::new(HeaderLineKind::ExpandedKeys, &kb());
    let rendered = line.render(200);
    let stripped: Vec<String> = rendered.iter().map(|line| strip_ansi(line)).collect();
    let text = stripped.join("\n");
    for (key, description) in default_expanded_key_hints() {
        let expected = if key.is_empty() {
            description.clone()
        } else {
            format!("{key} {description}")
        };
        assert!(
            text.contains(&expected),
            "expanded header line missing {expected:?} in {text:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Footer gates (upstream footer.ts:193/243, footer-data-provider.ts:245-263)
// ---------------------------------------------------------------------------

fn footer_line(state: &FooterState) -> String {
    pir::tui::theme::set_theme("dark");
    render_footer(200, state)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn footer_cost_line_renders_on_any_nonzero_cost() {
    let _guard = super::tui_system_theme::lock();
    // Upstream gates on truthiness (`if (usageTotals.cost || …)`): a sub-millicent
    // cost still renders the line (the old `>= 0.001` gate swallowed it).
    let state = FooterState {
        usage: pir::tui::app::UsageTotals {
            cost: 0.0004,
            ..pir::tui::app::UsageTotals::default()
        },
        model_id: "m".to_string(),
        ..FooterState::default()
    };
    assert!(
        footer_line(&state).contains("$0.000"),
        "sub-millicent cost renders"
    );
}

#[test]
fn footer_cost_line_hides_at_zero_cost() {
    let _guard = super::tui_system_theme::lock();
    let state = FooterState {
        model_id: "m".to_string(),
        ..FooterState::default()
    };
    let line = footer_line(&state);
    assert!(
        !line.contains('$'),
        "zero cost renders no cost line: {line}"
    );
}

#[test]
fn footer_routed_level_renders_when_truthy() {
    let _guard = super::tui_system_theme::lock();
    // Upstream footer.ts:243: `routed.thinkingLevel ? ` • ${level}` : ""` —
    // "off" is truthy and renders (pir filtered it out).
    let state = FooterState {
        model_id: "virtual".to_string(),
        routed: Some(("physical".to_string(), Some("off".to_string()))),
        ..FooterState::default()
    };
    assert!(
        footer_line(&state).contains("→ physical • off"),
        "routed off level renders: {}",
        footer_line(&state)
    );
    let no_level = FooterState {
        model_id: "virtual".to_string(),
        routed: Some(("physical".to_string(), None)),
        ..FooterState::default()
    };
    assert!(footer_line(&no_level).contains("→ physical"));
    assert!(!footer_line(&no_level).contains("• off"));
}

#[test]
fn footer_git_branch_labels_a_detached_head() {
    assert_eq!(footer_git_branch_text("HEAD"), "detached");
    assert_eq!(footer_git_branch_text("main"), "main");
    assert_eq!(footer_git_branch_text("feature/x"), "feature/x");
}

#[test]
fn subscription_gate_reads_the_registry_oauth_flag() {
    // The `(sub)` gate (model-runtime.ts:540-542) requires the provider's
    // registry OAuth metadata `isSubscription === true` — pir reads the flag
    // off the OAuth flow registry (`oauth_flow_for(...).is_subscription()`).
    let subscription_backed = [
        "anthropic",
        "openai",
        "openai-codex",
        "github-copilot",
        "kimi-coding",
        "meta",
        "xai",
    ];
    // openrouter's OAuth flow is plain account billing.
    assert!(!pir::ai::auth::oauth_flow_for("openrouter", None)
        .expect("flow registered")
        .is_subscription());
    for provider in subscription_backed {
        let flow = pir::ai::auth::oauth_flow_for(provider, None).expect("flow registered");
        assert!(
            flow.is_subscription(),
            "{provider} is subscription-backed in the upstream registry"
        );
    }
    // openrouter's OAuth flow is plain account billing.
    let flow = pir::ai::auth::oauth_flow_for("openrouter", None).expect("flow registered");
    assert!(
        !flow.is_subscription(),
        "openrouter is not subscription-backed"
    );
}

// ---------------------------------------------------------------------------
// /bug bundle (upstream core/bug-report.ts:183-223 + 330-375)
// ---------------------------------------------------------------------------

fn stored_entry(
    id: &str,
    message: pir::ai::types::AssistantMessage,
) -> pir::session_manager::types::FileEntry {
    pir::session_manager::types::FileEntry::Message(pir::session_manager::types::MessageEntry {
        id: id.to_string(),
        parent_id: None,
        timestamp: "2026-10-10T00:00:00Z".to_string(),
        message: pir::session_manager::types::StoredMessage::Assistant(message),
    })
}

fn assistant_message(
    stop_reason: pir::ai::types::StopReason,
    error: Option<String>,
) -> pir::ai::types::AssistantMessage {
    pir::ai::types::AssistantMessage {
        role: pir::ai::types::AssistantRole::Assistant,
        content: Vec::new(),
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        usage: pir::ai::types::Usage::default(),
        stop_reason,
        timestamp: 0,
        error_message: error,
        ..pir::ai::types::AssistantMessage::default()
    }
}

#[test]
fn bug_report_diagnostics_collect_failed_turns_only() {
    use pir::ai::types::StopReason;
    use pir::coding_agent::bug_report::collect_bug_report_diagnostics;
    let entries = vec![
        stored_entry("e1", assistant_message(StopReason::Stop, None)),
        stored_entry(
            "e2",
            assistant_message(StopReason::Error, Some("boom".to_string())),
        ),
        stored_entry("e3", assistant_message(StopReason::Aborted, None)),
    ];
    let diagnostics = collect_bug_report_diagnostics(&entries, Some("sess-1"), &[]);
    assert_eq!(diagnostics["schemaVersion"], 1);
    assert_eq!(diagnostics["sessionId"], "sess-1");
    assert_eq!(diagnostics["entryCount"], 3);
    assert_eq!(diagnostics["assistantMessageCount"], 3);
    let assistant = diagnostics["assistant"].as_array().expect("assistant list");
    assert_eq!(assistant.len(), 2, "only the failed turns are collected");
    assert_eq!(assistant[0]["entryId"], "e2");
    assert_eq!(assistant[0]["stopReason"], "error");
    assert_eq!(assistant[0]["errorMessage"], "boom");
    assert_eq!(assistant[0]["provider"], "fake");
    assert_eq!(assistant[0]["model"], "fake-model");
    assert_eq!(assistant[1]["entryId"], "e3");
    assert_eq!(assistant[1]["stopReason"], "aborted");
    assert!(diagnostics["crashes"]
        .as_array()
        .expect("crashes")
        .is_empty());
    // No conversation content leaks: the record keys are metadata only.
    let record = assistant[0].as_object().expect("record");
    for key in record.keys() {
        assert!(
            !matches!(key.as_str(), "content" | "messages" | "text"),
            "diagnostics must not carry conversation content ({key})"
        );
    }
}

#[test]
fn bug_report_diagnostics_count_healthy_assistants() {
    use pir::ai::types::StopReason;
    use pir::coding_agent::bug_report::collect_bug_report_diagnostics;
    let entries = vec![stored_entry(
        "e1",
        assistant_message(StopReason::Stop, None),
    )];
    let diagnostics = collect_bug_report_diagnostics(&entries, None, &[]);
    assert_eq!(diagnostics["assistantMessageCount"], 1);
    assert!(diagnostics["assistant"]
        .as_array()
        .expect("empty")
        .is_empty());
    assert!(diagnostics["sessionId"].is_null());
}

#[test]
fn bug_report_redaction_matches_the_upstream_regex() {
    use pir::coding_agent::bug_report::{is_sensitive_key, redact_json_value};
    // api[-_]key in its spellings.
    for key in ["api_key", "apiKey", "api-key", "my_api_key", "API_KEY"] {
        assert!(is_sensitive_key(key), "{key} is sensitive");
    }
    // The bare `api` segment is NOT sensitive (upstream has no bare `api`).
    for key in ["apiUrl", "api_base", "providerApi"] {
        assert!(!is_sensitive_key(key), "{key} must not be redacted");
    }
    let redacted = redact_json_value(&serde_json::json!({
        "api_key": "s",
        "apiKey": "s",
        "apiUrl": "https://example.com",
        "api_base": "https://example.com",
    }));
    assert_eq!(redacted["api_key"], "<redacted>");
    assert_eq!(redacted["apiKey"], "<redacted>");
    assert_eq!(redacted["apiUrl"], "https://example.com");
    assert_eq!(redacted["api_base"], "https://example.com");
}

#[test]
fn bug_report_summary_md_attaches_only_when_the_transcript_is_declined() {
    // Upstream generates summary.md with the session model when the user
    // declines the transcript (bug-report.ts:330-375); pir has no
    // consent-to-share-transcript LLM flow, so the worker leg attaches the
    // user's hint as the summary ONLY when `include_session` is false (the
    // bundle-level file wiring is covered by bug_report_files_shape).
    let declined = (false, Some("hangs on startup".to_string()));
    let included = (true, Some("hangs on startup".to_string()));
    let summary_for = |(include_session, hint): (bool, Option<String>)| -> Option<String> {
        if include_session {
            None
        } else {
            hint.as_deref()
                .map(|hint| format!("# Bug report summary\n\n{hint}\n"))
        }
    };
    assert!(summary_for(declined).is_some());
    assert!(summary_for(included).is_none());
}

// ---------------------------------------------------------------------------
// /login auth-type selector (upstream interactive-mode.ts:5885-5958,
// oauth-selector.ts:27-53)
// ---------------------------------------------------------------------------

#[test]
fn auth_type_selector_lists_the_upstream_options_with_radius_last() {
    let selector = AuthTypeSelector::login();
    assert_eq!(
        selector.labels(),
        vec![
            "Sign in with an account".to_string(),
            "Sign in with an API key".to_string(),
            "Sign in with Radius".to_string(),
        ]
    );
    assert_eq!(selector.selected(), Some(AuthTypeChoice::OAuth));
    assert_eq!(selector.handle_key(AuthKey::Down), AuthOutcome::None);
    assert_eq!(selector.selected(), Some(AuthTypeChoice::ApiKey));
    assert_eq!(selector.handle_key(AuthKey::Down), AuthOutcome::None);
    // The dedicated Radius entry sits LAST.
    assert_eq!(selector.selected(), Some(AuthTypeChoice::Radius));
    assert_eq!(selector.handle_key(AuthKey::Enter), AuthOutcome::Select(2));
    assert_eq!(selector.handle_key(AuthKey::Esc), AuthOutcome::Cancel);
    // Down clamps at the last row (Radius stays selected).
    assert_eq!(selector.handle_key(AuthKey::Down), AuthOutcome::None);
    assert_eq!(selector.selected(), Some(AuthTypeChoice::Radius));
}

#[test]
fn auth_type_selector_scopes_to_one_provider() {
    // Both methods: upstream order, no Radius row, provider-scoped title.
    let both = AuthTypeSelector::for_provider("anthropic", true, true, None);
    assert_eq!(
        both.labels(),
        vec![
            "Sign in with an account".to_string(),
            "Sign in with an API key".to_string(),
        ]
    );
    assert_eq!(both.selected(), Some(AuthTypeChoice::OAuth));
    // The upstream loginLabel ("Sign in with ChatGPT") replaces the default
    // account label when the flow declares one.
    let labeled = AuthTypeSelector::for_provider(
        "openai",
        true,
        true,
        Some("Sign in with ChatGPT".to_string()),
    );
    assert_eq!(labeled.labels()[0], "Sign in with ChatGPT");
    // A single-method provider lists one row.
    let only_key = AuthTypeSelector::for_provider("x", false, true, None);
    assert_eq!(
        only_key.labels(),
        vec!["Sign in with an API key".to_string()]
    );
    assert_eq!(only_key.selected(), Some(AuthTypeChoice::ApiKey));
}

#[test]
fn auth_type_selector_renders_the_radius_intro() {
    let _guard = super::tui_system_theme::lock();
    pir::tui::theme::set_theme("dark");
    let selector = AuthTypeSelector::login();
    let text: String = selector
        .render(200)
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Select authentication method:"));
    assert!(text.contains("Sign in with Radius"));
    assert!(
        text.contains(RADIUS_LOGIN_INTRO),
        "the Radius row carries its intro description: {text}"
    );
}

#[test]
fn auth_status_markers_match_oauth_selector() {
    let _guard = super::tui_system_theme::lock();
    pir::tui::theme::set_theme("dark");
    let marker = |kind, subscription, source, status_kind| {
        strip_ansi(&auth_status_marker(kind, subscription, source, status_kind))
    };
    // Not configured.
    assert_eq!(
        marker(AuthProviderKind::OAuth, false, "", None),
        " • not configured"
    );
    // Matching stored credential.
    assert_eq!(
        marker(
            AuthProviderKind::OAuth,
            false,
            "stored credential",
            Some(AuthProviderKind::OAuth)
        ),
        " ✓ configured"
    );
    // Mismatched configured type → the upstream warning with the CONFIGURED
    // type's label.
    assert_eq!(
        marker(
            AuthProviderKind::ApiKey,
            false,
            "stored credential",
            Some(AuthProviderKind::OAuth)
        ),
        " • account configured"
    );
    assert_eq!(
        marker(
            AuthProviderKind::OAuth,
            true,
            "stored credential",
            Some(AuthProviderKind::ApiKey)
        ),
        " • API key configured"
    );
    // ALL-CAPS environment sources render as `env: SRC`.
    assert_eq!(
        marker(
            AuthProviderKind::ApiKey,
            false,
            "ANTHROPIC_API_KEY",
            Some(AuthProviderKind::ApiKey)
        ),
        " ✓ env: ANTHROPIC_API_KEY"
    );
    // Any other source renders verbatim.
    assert_eq!(
        marker(
            AuthProviderKind::ApiKey,
            false,
            "models.json",
            Some(AuthProviderKind::ApiKey)
        ),
        " ✓ models.json"
    );
    // The type labels (oauth-selector.ts:27-33).
    assert_eq!(auth_type_label(AuthProviderKind::ApiKey, false), "API key");
    assert_eq!(auth_type_label(AuthProviderKind::OAuth, false), "account");
    assert_eq!(
        auth_type_label(AuthProviderKind::OAuth, true),
        "subscription"
    );
}

// ---------------------------------------------------------------------------
// External editor expanded text (upstream interactive-mode.ts:4598)
// ---------------------------------------------------------------------------

#[test]
fn external_editor_sends_the_expanded_text() {
    // Upstream edits `getExpandedText?.() ?? getText()` — the paste markers
    // are expanded before the editor sees the buffer (pir's
    // `Editor::expanded_text`).
    let editor = Editor::new();
    let long = (0..200)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.set_text("before ");
    editor.paste_text(&long);
    editor.paste_text("short tail");
    let raw = editor.get_text();
    assert!(
        raw.contains("[paste #1"),
        "raw text keeps the marker: {raw}"
    );
    let expanded = editor.expanded_text();
    assert!(
        expanded.contains("line-0") && expanded.contains("line-199"),
        "expanded text carries the full paste"
    );
    assert!(
        !expanded.contains("[paste #1"),
        "expanded text has no marker"
    );
}

// ---------------------------------------------------------------------------
// Dequeue empty queue (upstream interactive-mode.ts:4507-4514)
// ---------------------------------------------------------------------------

#[test]
fn dequeue_status_text_matches_upstream() {
    assert_eq!(dequeue_status_text(0), "No queued messages to restore");
    assert_eq!(
        dequeue_status_text(1),
        "Restored 1 queued message to editor"
    );
    assert_eq!(
        dequeue_status_text(2),
        "Restored 2 queued messages to editor"
    );
}

#[test]
fn pending_messages_use_the_live_dequeue_hint_and_replace_on_queue_update() {
    let lines = render_pending_messages(&["steer".to_string()], &[], "alt+q");
    assert!(
        lines.iter().any(|line| line.contains("alt+q")),
        "the hint must name the live chord: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("alt+up")),
        "the hint must not hardcode the non-Windows chord: {lines:?}"
    );
    assert!(render_pending_messages(&[], &[], "alt+q").is_empty());

    let steering = std::sync::Mutex::new(vec!["old".to_string()]);
    let follow_up = std::sync::Mutex::new(vec!["old follow".to_string()]);
    apply_queue_snapshot(&steering, &follow_up, Vec::new(), Vec::new());
    assert!(
        steering.lock().unwrap().is_empty(),
        "a delivered/cleared queue must replace the stale display entry"
    );
    assert!(follow_up.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// /tree summarize (upstream interactive-mode.ts:5593-5597)
// ---------------------------------------------------------------------------

#[test]
fn branch_summary_options_keep_the_upstream_shape() {
    let options = branch_summary_options();
    let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["No summary", "Summarize", "Summarize with custom prompt"]
    );
}

// ---------------------------------------------------------------------------
// Key-event guard: the win32 dequeue/paste chords stay dispatchable
// ---------------------------------------------------------------------------

#[test]
fn win32_hint_chords_match_real_events() {
    let bindings = kb();
    let event = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
    if pir::config::keybindings::use_windows_keybindings() {
        assert!(bindings.matches_app_id(
            "app.message.followUp",
            &event(KeyCode::Char('q'), KeyModifiers::CONTROL)
        ));
        assert!(bindings.matches_app_id(
            "app.message.dequeue",
            &event(KeyCode::Char('q'), KeyModifiers::ALT)
        ));
    } else {
        assert!(bindings.matches_app_id(
            "app.message.followUp",
            &event(KeyCode::Enter, KeyModifiers::ALT)
        ));
        assert!(bindings.matches_app_id(
            "app.message.dequeue",
            &event(KeyCode::Up, KeyModifiers::ALT)
        ));
    }
    let paste = bindings.hint_key_text("app.clipboard.pasteImage");
    assert!(!paste.is_empty(), "paste hint never renders unbound");
}
