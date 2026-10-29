//! Session → HTML export (upstream `core/export-html/**` + the `--export`
//! flag and the `/export` slash command).
//!
//! Covers the renderers directly (message text, tool blocks with their
//! outputs, ANSI stripping, HTML escaping, markdown subset) and the flag
//! wiring end-to-end through the built binary.

use std::path::{Path, PathBuf};

use pir::ai::types::{
    AssistantMessage, ContentBlock, ImageContent, Message, StopReason, TextContent,
    ThinkingContent, ToolCall, ToolResultMessage, ToolResultRole, UserMessage, UserRole,
};
use pir::coding_agent::export_html::{escape_html, export_session_to_html, markdown_to_html};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-export-{name}-{stamp}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: 1_700_000_000_000,
        sections: None,
    })
}

fn assistant_message(blocks: Vec<ContentBlock>) -> Message {
    Message::Assistant(AssistantMessage {
        content: blocks,
        provider: "anthropic".into(),
        model: "claude-sonnet-4".into(),
        stop_reason: StopReason::Stop,
        timestamp: 1_700_000_000_001,
        ..Default::default()
    })
}

fn tool_result(call_id: &str, name: &str, text: &str, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: name.into(),
        content: vec![text_block(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error,
        duration_ms: None,
        timestamp: 1_700_000_000_002,
    })
}

/// A session file with a user message, an assistant tool call, its result,
/// and a final assistant answer. Returns the file path.
fn fixture_session(dir: &Path) -> PathBuf {
    let mut manager = SessionManager::create(&dir.to_string_lossy(), dir).expect("create session");
    manager
        .append_message(user_message("Please read <notes.txt> & summarize"))
        .expect("append user");
    manager
        .append_message(assistant_message(vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "I should call the read tool.".into(),
                thinking_signature: None,
                redacted: false,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: [("path".to_owned(), serde_json::json!("notes.txt"))]
                    .into_iter()
                    .collect(),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ]))
        .expect("append assistant tool call");
    manager
        .append_message(tool_result(
            "call-1",
            "read",
            "\u{1b}[31mred\u{1b}[0m line\nplain line",
            false,
        ))
        .expect("append tool result");
    manager
        .append_message(assistant_message(vec![text_block(
            "# Summary\n\n- **bold** point\n- `code` point\n\nSee [docs](https://example.com).",
        )]))
        .expect("append assistant answer");
    let path = manager.path().to_path_buf();
    assert!(
        path.exists(),
        "session file must exist at {}",
        path.display()
    );
    path
}

// ---------------------------------------------------------------------------
// Renderer tests
// ---------------------------------------------------------------------------

#[test]
fn export_contains_messages_tool_blocks_and_escaped_content() {
    let dir = temp_dir("render");
    let session = fixture_session(&dir);
    let output = dir.join("out.html");
    let written = export_session_to_html(&session, Some(&output)).expect("export succeeds");
    assert_eq!(written, output);

    let html = std::fs::read_to_string(&output).expect("read html");

    // Self-contained document: inline CSS, no external assets or scripts.
    assert!(html.starts_with("<!DOCTYPE html>"), "missing doctype");
    assert!(html.contains("<style>"), "CSS must be inlined");
    assert!(html.contains("Session:"), "missing session metadata header");
    assert!(!html.contains("<script"), "exports must not carry scripts");
    assert!(
        !html.contains("<link "),
        "exports must not reference external stylesheets"
    );

    // Session metadata header fields (upstream `renderHeader`).
    assert!(html.contains("Date:"), "missing date info item");
    assert!(html.contains("Models:"), "missing models info item");
    assert!(html.contains("Tool Calls:"), "missing tool-call stat");
    assert!(
        html.contains("anthropic/claude-sonnet-4"),
        "missing model name"
    );

    // User message, markdown-converted assistant text.
    assert!(
        html.contains("Please read &lt;notes.txt&gt; &amp; summarize"),
        "user text must be present and escaped"
    );
    assert!(html.contains("<h1>Summary</h1>"), "heading not rendered");
    assert!(html.contains("<strong>bold</strong>"), "bold not rendered");
    assert!(
        html.contains("<code>code</code>"),
        "inline code not rendered"
    );
    assert!(html.contains("<li>"), "list item not rendered");
    assert!(
        html.contains("<a href=\"https://example.com\">docs</a>"),
        "link not rendered"
    );

    // Tool execution block with its ANSI-stripped output.
    assert!(
        html.contains("class=\"tool-execution"),
        "missing tool block"
    );
    assert!(html.contains("tool-name"), "missing tool name span");
    assert!(html.contains("read"), "tool name missing");
    assert!(
        html.contains("red line") && html.contains("plain line"),
        "tool output missing"
    );
    assert!(
        !html.contains("\u{1b}["),
        "ANSI escape sequences must be stripped from the export"
    );

    // Thinking block present.
    assert!(
        html.contains("I should call the read tool."),
        "thinking missing"
    );
}

#[test]
fn export_escapes_dangerous_markup() {
    let dir = temp_dir("escape");
    let mut manager =
        SessionManager::create(&dir.to_string_lossy(), dir.as_path()).expect("create session");
    manager
        .append_message(user_message("</div><script>alert('x')</script>"))
        .expect("append");
    let output = dir.join("out.html");
    export_session_to_html(manager.path(), Some(&output)).expect("export");

    let html = std::fs::read_to_string(&output).expect("read html");
    assert!(
        html.contains("&lt;/div&gt;&lt;script&gt;"),
        "raw markup must be escaped"
    );
    assert!(
        !html.contains("<script>alert"),
        "unescaped script tag leaked"
    );
}

#[test]
fn export_embeds_tool_result_images_as_data_urls() {
    let dir = temp_dir("images");
    let mut manager =
        SessionManager::create(&dir.to_string_lossy(), dir.as_path()).expect("create session");
    manager
        .append_message(assistant_message(vec![ContentBlock::ToolCall(ToolCall {
            id: "call-img".into(),
            name: "read".into(),
            arguments: Default::default(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })]))
        .expect("append call");
    manager
        .append_message(Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-img".into(),
            tool_name: "read".into(),
            content: vec![
                text_block("Read image file [image/png]"),
                ContentBlock::Image(ImageContent {
                    data: "QUJD".into(),
                    mime_type: "image/png".into(),
                }),
            ],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: None,
            nested_calls: None,
            is_error: false,
            duration_ms: None,
            timestamp: 1_700_000_000_003,
        }))
        .expect("append result");
    let output = dir.join("out.html");
    export_session_to_html(manager.path(), Some(&output)).expect("export");

    let html = std::fs::read_to_string(&output).expect("read html");
    assert!(
        html.contains("src=\"data:image/png;base64,QUJD\""),
        "tool-result image must be embedded inline"
    );
}

#[test]
fn export_default_output_path_is_derived_from_the_session_file() {
    let dir = temp_dir("default-path");
    let session = fixture_session(&dir);
    // No explicit output path: upstream derives `<app>-session-<stem>.html`.
    let written = export_session_to_html(&session, None).expect("export");
    assert!(
        written.to_string_lossy().contains("pir-session-"),
        "unexpected derived name: {}",
        written.display()
    );
    assert!(written.exists(), "derived output must be written");
    let _ = std::fs::remove_file(&written);
}

#[test]
fn export_reports_a_missing_session_file() {
    let dir = temp_dir("missing");
    let missing = dir.join("nope.jsonl");
    let error = export_session_to_html(&missing, None).expect_err("must fail");
    assert!(
        error.to_string().contains("File not found"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// Markdown subset + escaping units
// ---------------------------------------------------------------------------

#[test]
fn markdown_subset_renders_the_ported_constructs() {
    let html = markdown_to_html(
        "# Title\n\nsome **bold** and *italic* and `code`\n\n- one\n- two\n\n1. first\n2. second\n\n```\nlet x = 1;\n```\n",
    );
    assert!(html.contains("<h1>Title</h1>"));
    assert!(html.contains("<strong>bold</strong>"));
    assert!(html.contains("<em>italic</em>"));
    assert!(html.contains("<code>code</code>"));
    assert!(html.contains("<ul>") && html.contains("</ul>"));
    assert!(html.contains("<ol>") && html.contains("</ol>"));
    assert!(html.contains("<pre><code>let x = 1;\n</code></pre>"));
}

#[test]
fn markdown_escapes_html_before_formatting() {
    let html = markdown_to_html("A <b>tag</b> & `a < b`");
    assert!(html.contains("&lt;b&gt;tag&lt;/b&gt; &amp;"));
    assert!(html.contains("<code>a &lt; b</code>"));
}

#[test]
fn markdown_rejects_unsafe_link_schemes() {
    let html = markdown_to_html("[click](javascript:alert(1))");
    assert!(
        !html.contains("href=\"javascript:"),
        "unsafe scheme must not become an href: {html}"
    );
    assert!(html.contains("click"), "the label must still render");
}

#[test]
fn escape_html_covers_the_attribute_and_text_contexts() {
    assert_eq!(
        escape_html("<a href=\"x\">&'</a>"),
        "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
    );
}

// ---------------------------------------------------------------------------
// Flag wiring: `--export <session> [output]`
// ---------------------------------------------------------------------------

fn run_pir(args: &[&str], cwd: &Path) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    command.args(args).current_dir(cwd);
    command.output().expect("pir binary should start")
}

#[test]
fn export_flag_writes_html_and_exits_zero() {
    let dir = temp_dir("flag");
    let session = fixture_session(&dir);
    let output = dir.join("flag-out.html");

    let result = run_pir(
        &[
            "--export",
            &session.to_string_lossy(),
            &output.to_string_lossy(),
        ],
        &dir,
    );
    assert!(
        result.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("Exported to:"),
        "missing export notice: {stdout}"
    );
    assert!(output.exists(), "output file must be written");
    let html = std::fs::read_to_string(&output).expect("read html");
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("Please read"), "message missing from export");
}

#[test]
fn export_flag_without_output_derives_the_name_in_the_cwd() {
    let dir = temp_dir("flag-default");
    let session = fixture_session(&dir);
    let result = run_pir(&["--export", &session.to_string_lossy()], &dir);
    assert!(
        result.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let derived = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("pir-session-") && name.ends_with(".html"));
    assert!(
        derived.is_some(),
        "no derived export file in {}",
        dir.display()
    );
}

#[test]
fn export_flag_on_a_missing_session_fails() {
    let dir = temp_dir("flag-missing");
    let missing = dir.join("does-not-exist.jsonl");
    let result = run_pir(&["--export", &missing.to_string_lossy()], &dir);
    assert!(!result.status.success(), "missing session must fail");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("No session found matching") || stderr.contains("File not found"),
        "unexpected error: {stderr}"
    );
}

#[test]
fn export_flag_is_accepted_by_the_parser_with_two_values() {
    use clap::Parser as _;
    let cli = pir::coding_agent::cli::Cli::try_parse_from([
        "pir",
        "--export",
        "session.jsonl",
        "out.html",
    ])
    .expect("two-value --export must parse");
    let (session, output) = cli.export_request().expect("export request present");
    assert_eq!(session, "session.jsonl");
    assert_eq!(output.as_deref(), Some("out.html"));

    let cli = pir::coding_agent::cli::Cli::try_parse_from(["pir", "--export", "session.jsonl"])
        .expect("single-value --export must parse");
    let (session, output) = cli.export_request().expect("export request present");
    assert_eq!(session, "session.jsonl");
    assert!(output.is_none());
}

#[test]
fn export_flag_help_lists_the_export_option() {
    let dir = temp_dir("flag-help");
    let result = run_pir(&["--help"], &dir);
    assert!(result.status.success());
    let help = String::from_utf8_lossy(&result.stdout);
    assert!(help.contains("--export"), "help omitted --export: {help}");
}
