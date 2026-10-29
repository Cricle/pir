//! Display-layer parity tests for the final-audit batch: the mermaid
//! renderer adapter (the `mermaid-text` engine that replaced the hand-written
//! `grok-mermaid` port — assertions are structural, not glyph-pinned), the
//! markdown `mermaid` fence branch, the
//! `/session` cost-breakdown renderer, `/export` JSONL serialization,
//! `collapseChangelog`, `PI_SHARE_VIEWER_URL`, the update-check version
//! comparison, the `/hotkeys` extension rows, and the `/bug` crash section.

use pir::config::settings::MermaidRenderingMode;
use pir::tui::core::Component;
use pir::tui::markdown::{Markdown, MarkdownOptions, MarkdownTheme};
use pir::tui::mermaid::{self, MermaidOutcome};
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// mermaid renderer (mermaid-text engine adapter)
// ---------------------------------------------------------------------------

/// Unwrap a render result, failing with the advisory when there is no art.
fn diagram(src: &str) -> pir::tui::mermaid::MermaidArt {
    match mermaid::render(src) {
        MermaidOutcome::Diagram(art) => art,
        MermaidOutcome::NotRendered(why) => panic!("{src:?} should render, got {why:?}"),
    }
}

#[test]
fn mermaid_replaces_flowchart_with_unicode_diagram() {
    let art = diagram("flowchart LR\n  A[Start] --> B[Done]");
    assert!(
        art.plain.iter().any(|row| row.contains("Start")),
        "start node text: {:?}",
        art.plain
    );
    assert!(
        art.plain.iter().any(|row| row.contains("Done")),
        "done node text: {:?}",
        art.plain
    );
    // The nodes are joined by box-drawing edge glyphs.
    assert!(
        art.plain.iter().any(|row| row.contains('─')),
        "edge rows: {:?}",
        art.plain
    );
    // LR places Start left of Done wherever they share a row.
    if let Some(row) = art
        .plain
        .iter()
        .find(|row| row.contains("Start") && row.contains("Done"))
    {
        assert!(
            row.find("Start").unwrap_or(0) < row.find("Done").unwrap_or(usize::MAX),
            "LR order: {row:?}"
        );
    }
    // Width is the widest row in display columns.
    assert_eq!(
        art.width,
        art.plain
            .iter()
            .map(|row| UnicodeWidthStr::width(row.as_str()))
            .max()
            .unwrap_or(0)
    );
}

#[test]
fn mermaid_renders_bare_ids_shapes_and_directions() {
    let bare = diagram("flowchart LR\n  A --> B");
    assert!(
        bare.plain.iter().any(|row| row.contains('─')),
        "{:?}",
        bare.plain
    );
    for src in [
        "flowchart BT\n  A --> B",
        "flowchart RL\n  A --> B",
        "flowchart TD\n  A{x} --> B",
    ] {
        assert!(
            matches!(mermaid::render(src), MermaidOutcome::Diagram(_)),
            "{src}"
        );
    }
}

#[test]
fn mermaid_renders_edge_labels_in_both_grammars() {
    for src in [
        "flowchart LR\n  A[Ask] -->|yes| B[Do]",
        "flowchart LR\n  A[Ask] -- yes --> B[Do]",
    ] {
        let art = diagram(src);
        assert!(
            art.plain.iter().any(|row| row.contains("yes")),
            "{src}: {:?}",
            art.plain
        );
    }
}

#[test]
fn mermaid_not_rendered_for_unsupported_and_blank_input() {
    // Unsupported diagram kinds and blank input keep the raw block, with no
    // advisory (upstream `render` → null).
    assert_eq!(
        mermaid::render("   \n  "),
        MermaidOutcome::NotRendered(None)
    );
    assert_eq!(
        mermaid::render("unknownDiagram title X\n  a --> b"),
        MermaidOutcome::NotRendered(None)
    );
}

#[test]
fn mermaid_header_failure_carries_an_advisory() {
    // A flowchart header the engine cannot read fails whole, with the
    // diagnostic riding along for the settled-render advisory.
    match mermaid::render("graph ZZ\n  A --> B") {
        MermaidOutcome::NotRendered(Some(message)) => {
            assert!(message.contains("direction"), "{message}");
        }
        other => panic!("expected an advisory, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// markdown mermaid fence branch (upstream createMermaidMarkdownTransformer)
// ---------------------------------------------------------------------------

fn render_markdown(text: &str, mode: MermaidRenderingMode, width: usize) -> Vec<String> {
    let options = MarkdownOptions {
        mermaid: mode,
        ..MarkdownOptions::default()
    };
    Markdown::with_options(text, 0, 0, MarkdownTheme::default(), options).render(width)
}

#[test]
fn markdown_mermaid_off_keeps_the_fence() {
    let text = "Before\n\n```mermaid\nflowchart LR\n  A --> B\n```\nAfter";
    let lines = render_markdown(text, MermaidRenderingMode::Off, 100);
    assert!(lines.iter().any(|line| line.contains("```mermaid")));
    assert!(!lines.iter().any(|line| line.contains('─')));
}

#[test]
fn markdown_mermaid_streaming_replaces_the_fence() {
    let text = "Before\n\n```mermaid\nflowchart LR\n  A --> B\n```\nAfter";
    let lines = render_markdown(text, MermaidRenderingMode::Streaming, 100);
    assert!(!lines.iter().any(|line| line.contains("```mermaid")));
    assert!(lines.iter().any(|line| line.contains('─')));
    assert!(lines.iter().any(|line| line.contains("After")));
}

#[test]
fn markdown_mermaid_oversized_or_unsupported_keeps_raw() {
    let supported = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
    // The diagram is wider than 10 columns; the viewport cut keeps the fence.
    let lines = render_markdown(supported, MermaidRenderingMode::Streaming, 10);
    assert!(lines.iter().any(|line| line.contains("```mermaid")));

    // An unsupported diagram kind keeps the raw fence, silently.
    let unsupported = "```mermaid\nunknownDiagram title X\n```";
    let lines = render_markdown(unsupported, MermaidRenderingMode::Streaming, 100);
    assert!(lines.iter().any(|line| line.contains("```mermaid")));
}

#[test]
fn markdown_mermaid_final_reports_engine_advisory() {
    // A header the engine cannot read fails whole: the settled (final) render
    // keeps the raw fence and appends the advisory; streaming keeps the raw
    // fence without one.
    let text = "```mermaid\ngraph ZZ\n  A --> B\n```";
    let final_lines = render_markdown(text, MermaidRenderingMode::Final, 100);
    assert!(final_lines.iter().any(|line| line.contains("```mermaid")));
    assert!(
        final_lines
            .iter()
            .any(|line| line.contains("Mermaid diagram not rendered: ")),
        "{final_lines:?}"
    );
    let streaming = render_markdown(text, MermaidRenderingMode::Streaming, 100);
    assert!(streaming.iter().any(|line| line.contains("```mermaid")));
    assert!(!streaming
        .iter()
        .any(|line| line.contains("Mermaid diagram not rendered")));
}

// ---------------------------------------------------------------------------
// /session breakdown renderer (upstream handleSessionCommand)
// ---------------------------------------------------------------------------

fn usage_cost(key: &str, cost: f64, tokens: u64) -> pir::tui::slash::UsageCostEntry {
    pir::tui::slash::UsageCostEntry {
        key: key.to_owned(),
        cost,
        tokens,
    }
}

#[test]
fn session_info_renders_breakdown_and_cache_rebilling() {
    let data = pir::tui::slash::SessionInfoData {
        session_name: Some("demo".to_owned()),
        session_file: Some("/tmp/session.jsonl".to_owned()),
        session_id: "s-1".to_owned(),
        total_messages: 4,
        user_messages: 2,
        assistant_messages: 2,
        tool_calls: 1,
        tool_results: 1,
        tokens_input: 1500,
        tokens_output: 300,
        tokens_cache_read: 12_000,
        tokens_cache_write: 500,
        tokens_total: 14_300,
        cost: 0.0134,
        usage_breakdown: vec![
            usage_cost("anthropic/claude-x", 0.0100, 12_345),
            usage_cost("Tools/summaries", 0.0034, 2_000),
        ],
        selected_model_key: "openai/gpt".to_owned(),
        cache_warming_mode: "streaming".to_owned(),
        cache_warming_status: None,
        cache_missed_tokens: 20_000,
        cache_missed_cost: 0.0123,
        cache_miss_count: 2,
    };
    let text = pir::tui::slash::session_info_text(&data);
    assert!(text.contains("Session Info"), "{text}");
    assert!(text.contains("Name: demo"));
    assert!(text.contains("File: /tmp/session.jsonl"));
    // Prompt volume = input + cacheRead + cacheWrite.
    assert!(text.contains("Input: 14,000"));
    assert!(text.contains("Cached: 12,000 (85.7%)"));
    assert!(text.contains("Uncached: 2,000 (500 written to cache)"));
    assert!(text.contains("Output: 300"));
    assert!(text.contains("Total: 14,300"));
    assert!(text.contains("Cache Warming"));
    assert!(text.contains("Mode: streaming"));
    assert!(text.contains("Status: Inactive (cache warming unavailable)"));
    assert!(text.contains("Total: $0.013"));
    // The selected model differs from the single bucket's key, so the
    // breakdown prints. Token counts round at the k level (upstream
    // `formatTokens`, footer.ts:25-32): 12,345 → "12k".
    assert!(text.contains("anthropic/claude-x: $0.010 (12k tokens)"));
    assert!(text.contains("Tools/summaries: $0.003 (2.0k tokens)"));
    assert!(text.contains("Cache Re-billed: $0.012 (20,000 tokens, 2 misses)"));
}

#[test]
fn session_info_single_entry_without_cost_stays_condensed() {
    let data = pir::tui::slash::SessionInfoData {
        selected_model_key: "anthropic/claude-x".to_owned(),
        usage_breakdown: vec![usage_cost("anthropic/claude-x", 0.5, 1_000)],
        cost: 0.5,
        ..Default::default()
    };
    let text = pir::tui::slash::session_info_text(&data);
    // A single entry matching the selected model repeats the total only.
    assert!(text.contains("Total: $0.500"));
    assert!(!text.contains("anthropic/claude-x: $0.500"));
    assert!(!text.contains("Cache Re-billed"));
}

// ---------------------------------------------------------------------------
// /export JSONL (upstream serializeSessionBranch)
// ---------------------------------------------------------------------------

#[test]
fn export_format_detection_and_default_name() {
    assert_eq!(
        pir::tui::slash::export_format(Some("out.jsonl")),
        pir::tui::slash::ExportFormat::Jsonl
    );
    assert_eq!(
        pir::tui::slash::export_format(Some("out.html")),
        pir::tui::slash::ExportFormat::Html
    );
    assert_eq!(
        pir::tui::slash::export_format(None),
        pir::tui::slash::ExportFormat::Html
    );
    assert_eq!(
        pir::tui::slash::default_jsonl_export_name("2026-10-09T12:00:00.000Z"),
        "session-2026-10-09T12-00-00-000Z.jsonl"
    );
}

#[test]
fn jsonl_export_chains_parent_ids() {
    // The export header carries the session's RESOLVED cwd, and
    // `resolve_path_str` only treats a path as absolute when the host does.
    // `C:\tmp` is absolute on Windows but a relative name on Unix, where it
    // would be resolved against the test process's cwd; use a host-absolute
    // path instead (Windows keeps the original drive-letter shape).
    #[cfg(windows)]
    let cwd = "C:\\tmp".to_owned();
    #[cfg(not(windows))]
    let cwd = std::env::temp_dir()
        .to_string_lossy()
        .trim_end_matches('/')
        .to_owned();
    let mut manager =
        pir::session_manager::SessionManager::in_memory(&cwd, None).expect("in-memory session");
    let first = manager
        .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
            role: pir::ai::types::UserRole::User,
            content: vec![pir::ai::types::ContentBlock::Text(
                pir::ai::types::TextContent {
                    text: "hello".to_owned(),
                    text_signature: None,
                },
            )],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        }))
        .expect("append");
    let _second = manager
        .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
            role: pir::ai::types::UserRole::User,
            content: vec![],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append");

    let jsonl = pir::tui::slash::serialize_session_branch(&manager);
    let lines: Vec<&str> = jsonl.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 3, "{jsonl}");
    // Header first, with the `type` tag the enum injects on persisted files.
    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["type"], "session");
    // Windows keeps the drive-letter path verbatim; on Unix the header
    // records the host-absolute temp path unchanged.
    assert_eq!(header["cwd"], cwd.as_str());
    let first_entry: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first_entry["type"], "message");
    assert_eq!(first_entry["id"], first.as_str());
    // First entry's parent is null; the second chains to the first.
    assert!(first_entry["parentId"].is_null());
    let second_entry: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(second_entry["parentId"], first.as_str());
}

// ---------------------------------------------------------------------------
// collapseChangelog (upstream interactive-mode.ts:857-863)
// ---------------------------------------------------------------------------

#[test]
fn changelog_collapse_notice_names_the_newest_version() {
    let changelog = "## [1.2.3] First\n\n- entry\n\n## [1.3.0] Second\n\n- newer entry\n";
    assert_eq!(
        pir::tui::slash::changelog_collapse_notice(changelog, "0.9.0"),
        "Updated to v1.3.0. Use /changelog to view full changelog."
    );
    // No parseable entries: upstream's regex falls back to the running version.
    assert_eq!(
        pir::tui::slash::changelog_collapse_notice("no versions here", "9.9.9"),
        "Updated to v9.9.9. Use /changelog to view full changelog."
    );
}

// ---------------------------------------------------------------------------
// PI_SHARE_VIEWER_URL (upstream getShareViewerUrl, config.ts:595-598)
// ---------------------------------------------------------------------------

#[test]
fn share_viewer_url_honors_env_override() {
    // Default base with the `#` fragment shape.
    std::env::remove_var("PI_SHARE_VIEWER_URL");
    assert_eq!(
        pir::tui::slash::share_viewer_url("abc123"),
        "https://pi.dev/session/#abc123"
    );
    std::env::set_var("PI_SHARE_VIEWER_URL", "https://viewer.example/s/");
    assert_eq!(
        pir::tui::slash::share_viewer_url("abc123"),
        "https://viewer.example/s/#abc123"
    );
    std::env::remove_var("PI_SHARE_VIEWER_URL");
}

// ---------------------------------------------------------------------------
// update check (upstream version-check.ts)
// ---------------------------------------------------------------------------

#[test]
fn version_comparison_matches_semver_order() {
    use pir::tui::update_check::{compare_versions, is_newer_version};
    use std::cmp::Ordering;
    assert_eq!(compare_versions("1.2.3", "1.2.3"), Some(Ordering::Equal));
    assert_eq!(compare_versions("1.10.0", "1.9.9"), Some(Ordering::Greater));
    assert_eq!(
        compare_versions("2.0.0", "1.99.99"),
        Some(Ordering::Greater)
    );
    assert_eq!(compare_versions("1.2.3", "1.2.4"), Some(Ordering::Less));
    // A release outranks its own pre-release.
    assert_eq!(
        compare_versions("1.2.3-beta", "1.2.3"),
        Some(Ordering::Less)
    );
    assert_eq!(
        compare_versions("1.2.3-beta", "1.2.3-beta"),
        Some(Ordering::Equal)
    );
    // Non-semver falls back to string inequality (upstream fallback).
    assert!(is_newer_version("nightly-42", "1.2.3"));
    assert!(!is_newer_version(" 1.2.3 ", "1.2.3"));
    assert!(is_newer_version("1.3.0", "1.2.9"));
}

// ---------------------------------------------------------------------------
// /hotkeys extension rows (upstream handleHotkeysCommand extensions block)
// ---------------------------------------------------------------------------

#[test]
fn extension_hotkey_rows_fall_back_to_the_source_path() {
    let rows = pir::tui::slash::extension_hotkey_rows(&[
        pir::extensions::runtime::ShortcutDef {
            key: "ctrl+shift+k".to_owned(),
            description: Some("Toggle panel".to_owned()),
            source_info: Default::default(),
        },
        pir::extensions::runtime::ShortcutDef {
            key: "ctrl+alt+l".to_owned(),
            description: None,
            source_info: Default::default(),
        },
    ]);
    assert_eq!(
        rows,
        vec![
            ("ctrl+shift+k".to_owned(), "Toggle panel".to_owned()),
            ("ctrl+alt+l".to_owned(), String::new()),
        ]
    );
}

// ---------------------------------------------------------------------------
// /bug crash section (upstream collectBugReportDiagnostics)
// ---------------------------------------------------------------------------

#[test]
fn bug_report_with_crash_includes_the_crash_summary() {
    let inputs = pir::tui::slash::BugReportInputs {
        pir_version: "0.1.0".to_owned(),
        os: "windows".to_owned(),
        cwd: "C:\\repo".to_owned(),
        model_provider: "anthropic".to_owned(),
        model_id: "claude-x".to_owned(),
        thinking_level: "off".to_owned(),
        session_path: None,
        entry_count: 3,
        generated_at: "123".to_owned(),
        hint: Some("it crashed".to_owned()),
    };
    let plain = pir::tui::slash::bug_report_text(&inputs);
    assert!(!plain.contains("last crash"));

    let crash = pir::coding_agent::crash_log::CrashRecord {
        timestamp: "2026-10-09T00:00:00Z".to_owned(),
        version: "0.1.0".to_owned(),
        kind: pir::coding_agent::crash_log::CrashKind::UncaughtException,
        message: "index out of bounds".to_owned(),
        stack: Some("at src/main.rs:1:1\nframe two".to_owned()),
        session_file: None,
        cwd: String::new(),
        notified: None,
    };
    let with_crash = pir::tui::slash::bug_report_text_with_crash(&inputs, Some(&crash));
    assert!(
        with_crash
            .contains("last crash: 2026-10-09T00:00:00Z (uncaught_exception): index out of bounds"),
        "{with_crash}"
    );
    assert!(with_crash.contains("    at src/main.rs:1:1"));
    // Upstream embeds the complete crash records (`bug-report.ts:183-224`);
    // pir bounds the excerpt at 20 lines, and this 2-frame stack fits whole.
    assert!(
        with_crash.contains("frame two"),
        "the full (bounded) stack excerpt is carried"
    );
}

// ---------------------------------------------------------------------------
// showCacheMissNotices (upstream addCompactionCostNotice)
// ---------------------------------------------------------------------------

#[test]
fn billed_usage_notice_formats_tokens_and_cost() {
    let usage = pir::ai::types::Usage {
        input: 12_000,
        output: 500,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 12_500,
        cost: pir::ai::types::Cost {
            total: 0.0512,
            ..Default::default()
        },
        ..Default::default()
    };
    // Token counts round at the k level (upstream `formatTokens`):
    // 12,500 → 12.5.round() = 13 → "13k".
    assert_eq!(
        pir::tui::slash::billed_usage_notice("Compaction", &usage),
        "Compaction: 13k tokens billed (~$0.05)"
    );
    let cheap = pir::ai::types::Usage {
        cost: pir::ai::types::Cost {
            total: 0.001,
            ..Default::default()
        },
        ..usage
    };
    assert_eq!(
        pir::tui::slash::billed_usage_notice("Compaction", &cheap),
        "Compaction: 13k tokens billed"
    );
}

// ---------------------------------------------------------------------------
// token formatters (upstream toLocaleString / formatTokens)
// ---------------------------------------------------------------------------

#[test]
fn number_formatters() {
    assert_eq!(pir::tui::slash::format_thousands(0), "0");
    assert_eq!(pir::tui::slash::format_thousands(999), "999");
    assert_eq!(pir::tui::slash::format_thousands(1_000), "1,000");
    assert_eq!(pir::tui::slash::format_thousands(1_234_567), "1,234,567");
    assert_eq!(pir::tui::slash::format_tokens(999), "999");
    assert_eq!(pir::tui::slash::format_tokens(1_234), "1.2k");
    assert_eq!(pir::tui::slash::format_tokens(12_345), "12k");
    assert_eq!(pir::tui::slash::format_tokens(1_234_567), "1.2M");
    assert_eq!(pir::tui::slash::format_tokens(12_345_678), "12M");
}
