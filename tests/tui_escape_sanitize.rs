//! Direct regression tests for terminal escape injection from model/tool
//! output into the TUI and the HTML session export (QA4/QA4b/QA4d).
//!
//! Runs as its own integration target so the security fix can be verified
//! without the whole TUI suite.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, TextContent, Tool, ToolCall, ToolReference,
    ToolResultMessage, ToolResultRole, UserMessage, UserRole,
};
use pir::coding_agent::export_html::export_session_to_html;
use pir::session_manager::SessionManager;
use pir::text::{sanitize_plain_text, sanitize_terminal_text};
use pir::tui::ansi::strip_ansi;
use pir::tui::app::{session_tab_title, ChatContainer, ChatEntry, NoticeKind};
use pir::tui::core::Component;
use pir::tui::messages::{AssistantMessageComponent, ToolExecutionComponent, UserMessageComponent};
use pir::tui::terminal::transcript_entries_from_messages;
use pir::tui::terminal_image::{
    encode_iterm2, encode_kitty, image_content_lines_with_protocol, is_image_protocol_lines,
    ImageProtocol, Iterm2EncodeOptions, KittyEncodeOptions,
};

/// Every control family the sanitizer must drop, interleaved with visible
/// text so a render-path regression keeps failing even if marker strings are
/// truncated by wrapping.
const PAYLOAD: &str = "VISIBLE_BEFORE\
\x1b]0;PIWNED\x07\
MID\
\x1b]52;c;Zm9v\x07\
\x1b[2J\x1b[H\
\x1bPq#0;2;0;0;0DCS_PWNED\x1b\\\
\x1b^PM_PWNED\x1b\\\
\x1bXSOS_PWNED\x1b\\\
\x1b_Ga=T;APC_PWNED\x1b\\\
\u{9b}2J\
\u{9d}0;C1_OSC_PWNED\u{9c}\
\u{90}C1_DCS_PWNED\u{9c}\
\u{98}C1_SOS_PWNED\u{9c}\
\u{9e}C1_PM_PWNED\u{9c}\
\u{9f}C1_APC_PWNED\u{9c}\
\x1b[?1049h\
\x00\x08\x0b\x0c\x7f\
VISIBLE_AFTER";

fn assistant_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1_700_000_000_000,
        sections: None,
    })
}

/// Assert every ESC in `rendered` starts a syntactically legal SGR run
/// (`ESC [ <digits/;/:> m`) and every other control character is at most
/// `\n`/`\r`/`\t`. This is the strong form of "only trusted SGR survives":
/// a leak of any other ESC / CSI / OSC / DCS / APC / BEL / C0 / C1 fails.
fn assert_only_legal_sgr(rendered: &str) {
    let bytes = rendered.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        assert_eq!(
            bytes.get(index + 1),
            Some(&b'['),
            "ESC outside SGR at byte {index}: {:?}",
            &rendered[index..]
        );
        let mut cursor = index + 2;
        loop {
            assert!(
                cursor < bytes.len(),
                "unterminated escape sequence at byte {index}: {:?}",
                &rendered[index..]
            );
            let byte = bytes[cursor];
            if byte == b'm' {
                break;
            }
            assert!(
                byte.is_ascii_digit() || byte == b';' || byte == b':',
                "non-SGR CSI byte 0x{byte:02x} at byte {cursor}: {:?}",
                &rendered[index..]
            );
            cursor += 1;
        }
        index = cursor + 1;
    }
    for ch in rendered.chars() {
        assert!(
            !ch.is_control() || matches!(ch, '\n' | '\r' | '\t' | '\x1b'),
            "control character {ch:?} survived render: {rendered:?}"
        );
    }
}

/// Every rendered row must be free of the injected controls while the
/// visible text around them survives.
fn assert_neutralized(rendered: &str) {
    assert_only_legal_sgr(rendered);
    for marker in [
        "PIWNED",
        "]52;c;",
        "DCS_PWNED",
        "PM_PWNED",
        "SOS_PWNED",
        "APC_PWNED",
        "C1_OSC_PWNED",
        "C1_DCS_PWNED",
        "C1_SOS_PWNED",
        "C1_PM_PWNED",
        "C1_APC_PWNED",
    ] {
        assert!(
            !rendered.contains(marker),
            "injected payload {marker:?} survived render: {rendered:?}"
        );
    }
    let plain = strip_ansi(rendered);
    assert!(
        plain.contains("VISIBLE_BEFORE"),
        "visible prefix lost: {plain:?}"
    );
    assert!(
        plain.contains("VISIBLE_AFTER"),
        "visible suffix lost: {plain:?}"
    );
}

/// Every escape/control family is dropped, including 8-bit C1 introducers
/// with their payload and every unterminated sequence shape.
#[test]
fn sanitizer_covers_c1_and_unterminated_sequences() {
    let cases: &[(&str, &str)] = &[
        ("A\x1b]0;TITLE\x1b\\B", "AB"),
        ("A\x1b]52;c;Zm9v\x07B", "AB"),
        ("A\x1bPq#0;2;0;0;0\x1b\\B", "AB"),
        ("A\x1b^PM-SECRET\x1b\\B", "AB"),
        ("A\x1b_Ga=T;AAAA\x1b\\B", "AB"),
        ("A\x1bXSOS-SECRET\x1b\\B", "AB"),
        ("A\x1b[2JB", "AB"),
        ("A\x1b[?1049hB", "AB"),
        ("A\x1b[1 qB", "AB"),
        ("A\u{9b}2JB", "AB"),
        ("A\u{9d}0;C1-TITLE\u{9c}B", "AB"),
        ("A\u{90}DCS-SECRET\u{9c}B", "AB"),
        ("A\u{98}SOS-SECRET\u{9c}B", "AB"),
        ("A\u{9e}PM-SECRET\u{9c}B", "AB"),
        ("A\u{9f}APC-SECRET\u{9c}B", "AB"),
        // Unterminated: whole tail (never a partial prefix) is dropped.
        ("A\x1b]0;never-terminated", "A"),
        ("A\x1bPdcs-never-terminated", "A"),
        ("A\x1b[31", "A"),
        ("A\x1b", "A"),
        // C0/C1 controls outside escape strings.
        ("A\x00B\x08C\x0bD\x0cE\x7fF\u{0085}G", "ABCDEFG"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            sanitize_terminal_text(input).as_ref(),
            *expected,
            "sanitizer mismatch for {input:?}"
        );
    }
    // Legal SGR is the only escape form preserved by the terminal policy;
    // the plain-text (HTML) policy drops it too.
    assert_eq!(
        sanitize_terminal_text("a\x1b[31mred\x1b[0m\x1b[2Jb"),
        "a\x1b[31mred\x1b[0mb"
    );
    assert_eq!(sanitize_plain_text("a\x1b[31mred\x1b[0mb"), "aredb");
}

#[test]
fn tui_render_strips_osc0_osc52_and_ed2_but_keeps_sgr() {
    // Settled assistant text (also the `-c`/resume replay shape).
    let assistant =
        AssistantMessageComponent::new_from(Arc::new(assistant_message(PAYLOAD)), false, 1);
    let assistant_rows = assistant.render(200).join("\n");
    assert_neutralized(&assistant_rows);

    // Tool result output (the F30/F31 path).
    let tool = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({"command": "echo hi"})),
        Arc::new(AtomicBool::new(false)),
    );
    tool.update_result(Arc::from(PAYLOAD), false, None);
    let tool_rows = tool.render(200).join("\n");
    assert_neutralized(&tool_rows);

    // Tool call arguments are model-controlled too.
    let args_tool = ToolExecutionComponent::new(
        "bash",
        Arc::new(serde_json::json!({"command": PAYLOAD})),
        Arc::new(AtomicBool::new(false)),
    );
    assert_neutralized(&args_tool.render(200).join("\n"));

    // User messages (live and replayed) go through their own sanitizer.
    let user = UserMessageComponent::new(PAYLOAD, 0);
    assert_neutralized(&user.render(200).join("\n"));

    // Streams and notices are appended directly to the chat container.
    let mut chat = ChatContainer::new();
    chat.set_entries(&[
        ChatEntry::Notice {
            kind: NoticeKind::Error,
            text: PAYLOAD.to_string(),
        },
        ChatEntry::Streaming {
            live: true,
            thinking: PAYLOAD.to_string(),
            text: PAYLOAD.to_string(),
        },
    ]);
    let rendered = chat.render(200).join("\n");
    assert_neutralized(&rendered);

    // Recorded policy: a plain SGR color run is preserved (the TUI itself and
    // trusted renderers use SGR); every non-SGR CSI/OSC/DCS is dropped.
    assert_eq!(
        sanitize_terminal_text("a\x1b[31mred\x1b[0m\x1b[2Jb"),
        "a\x1b[31mred\x1b[0mb"
    );
}

/// `-c`/resume rebuilds the transcript from the stored session messages via
/// `transcript_entries_from_messages`; the `ChatEntry::Assistant` it emits is
/// expanded by `ChatContainer` through the SAME `AssistantMessageComponent::
/// new_from` as a live message. Replay must therefore neutralize the payload
/// even though the stored bytes stay raw (storage is model context, not a
/// render sink).
#[test]
fn resume_replay_path_reuses_the_sanitizing_constructor() {
    let messages = vec![
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: PAYLOAD.to_string(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 1_700_000_000_000,
            sections: None,
        }),
        Message::Assistant(assistant_message(PAYLOAD)),
    ];
    let entries = transcript_entries_from_messages(messages);
    assert!(
        matches!(
            entries.as_slice(),
            [_, ChatEntry::Assistant(message)]
                if message.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text(text) if text.text.contains('\x1b')
                ))
        ),
        "replay must keep the stored bytes and sanitize at render time"
    );

    let mut chat = ChatContainer::new();
    chat.set_entries(&entries);
    let rendered = chat.render(200).join("\n");
    assert_neutralized(&rendered);
}

#[test]
fn session_tab_titles_sanitize_first_user_message_and_name() {
    let preview = session_tab_title(None, Some(PAYLOAD), "/tmp/session.jsonl");
    assert_only_legal_sgr(&preview);
    assert!(
        !preview.contains("PIWNED"),
        "tab preview leaked OSC: {preview:?}"
    );
    assert!(
        preview.contains("VISIBLE_BEFORE") && preview.contains("VISIBLE_AFTER"),
        "tab preview lost visible text: {preview:?}"
    );

    let named = session_tab_title(Some(PAYLOAD), Some("ignored"), "/tmp/session.jsonl");
    assert_only_legal_sgr(&named);
    assert!(!named.contains("PIWNED"), "tab name leaked OSC: {named:?}");
}

#[test]
fn image_protocol_lines_survive_trusted_raw_render() {
    let kitty = encode_kitty("iVBORw0KGgo=", KittyEncodeOptions::default());
    let iterm2 = encode_iterm2("iVBORw0KGgo=", &Iterm2EncodeOptions::default());

    // The structural trust check accepts exactly the `image_block_lines`
    // shape: one complete sequence plus blank reservation rows.
    assert!(is_image_protocol_lines(&[
        kitty.clone(),
        String::new(),
        String::new()
    ]));
    assert!(is_image_protocol_lines(&[String::new(), iterm2.clone()]));
    // Cursor-up prefix on the iTerm2 placement is part of the shape.
    assert!(is_image_protocol_lines(&[format!("\x1b[2A{iterm2}")]));
    // Truncated, embedded, or multi-line text never qualifies.
    assert!(!is_image_protocol_lines(&[kitty
        .trim_end_matches("\x1b\\")
        .to_string()]));
    assert!(!is_image_protocol_lines(&[format!("prefix {kitty}")]));
    assert!(!is_image_protocol_lines(&[
        kitty.clone(),
        "SECOND_LINE".to_string()
    ]));

    for (lines, expected) in [
        (
            vec![kitty.clone(), String::new(), String::new()],
            kitty.clone(),
        ),
        (vec![String::new(), iterm2.clone()], iterm2.clone()),
    ] {
        let mut chat = ChatContainer::new();
        chat.set_entries(&[ChatEntry::Raw {
            call_id: None,
            lines,
            render: None,
        }]);
        let rendered = chat.render(200).join("\n");
        assert!(
            rendered.contains(&expected),
            "trusted image protocol line was sanitized away: {rendered:?}"
        );
    }

    // Any other raw text that merely embeds the prefix is still untrusted:
    // the APC/OSC sequence is dropped and only the visible text survives.
    let mut chat = ChatContainer::new();
    chat.set_entries(&[ChatEntry::Raw {
        call_id: None,
        lines: vec![format!("EVIL {kitty}")],
        render: None,
    }]);
    let rendered = chat.render(200).join("\n");
    assert!(
        rendered.contains("EVIL"),
        "visible text around an embedded sequence was lost: {rendered:?}"
    );
    assert!(
        !rendered.contains("\x1b_G"),
        "embedded APC escaped the sanitizer: {rendered:?}"
    );
    assert_only_legal_sgr(&rendered);
}

#[test]
fn image_mime_fallback_is_sanitized() {
    for protocol in [
        None,
        Some(ImageProtocol::Kitty),
        Some(ImageProtocol::Iterm2),
    ] {
        let lines = image_content_lines_with_protocol(
            protocol,
            "AAAA",
            "image/png\x1b]0;MIME_PWNED\x07\u{9b}2J",
            40,
            None,
        );
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(
            line.starts_with("[Image: image/png"),
            "unexpected fallback shape: {line:?}"
        );
        assert!(
            !line.contains("MIME_PWNED"),
            "mime payload leaked into fallback: {line:?}"
        );
        assert_only_legal_sgr(line);
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-qa4d-{name}-{stamp}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Export sinks that used to take `escape_html` alone: the tool header/args
/// and the system prompt/tool list (user-supplied AGENTS.md/repo content,
/// model-controlled tool metadata). The whole generated document must contain
/// no ESC / BEL / C0 / C1 at all.
#[test]
fn export_sanitizes_tool_header_args_system_prompt_and_tools() {
    let dir = temp_dir("export");
    let cwd = format!("{}projdir\x1b]0;CWD_PWNED\x07", dir.display());
    let mut manager = SessionManager::create(&cwd, &dir).expect("create session");

    manager
        .append_message(Message::System(UserMessage {
            role: UserRole::System,
            content: vec![ContentBlock::Text(TextContent {
                text: "SYS_VISIBLE\x1b]0;SYS_PWNED\x07".to_string(),
                text_signature: None,
            })],
            tools_added: Some(vec![Tool {
                name: "evil\x1b]0;TOOL_PWNED\x07".to_string(),
                description: "TOOL_VISIBLE\x1b]52;c;Zm9v\x07".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "arg\x1b]0;PARAM_PWNED\x07": {"type": "string"}
                    }
                }),
                constrained_sampling: None,
            }]),
            tools_removed: None,
            sections: Some(BTreeMap::from([(
                "rules".to_string(),
                Some("SECTION_VISIBLE\u{9b}2J\u{9d}0;SEC_PWNED\u{9c}".to_string()),
            )])),
            timestamp: 1_700_000_000_000,
        }))
        .expect("append leading system message");
    manager
        .append_message(user_message("hello"))
        .expect("append user");
    manager
        .append_message(Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: BTreeMap::from([(
                    "command".to_string(),
                    serde_json::json!("EXPAND\x1b]0;EXPORT_PWNED\x07"),
                )]),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })],
            provider: "prov\x1b]0;MODEL_PWNED\x07".to_string(),
            model: "model".to_string(),
            ..AssistantMessage::default()
        }))
        .expect("append assistant tool call");
    manager
        .append_message(Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ContentBlock::Text(TextContent {
                text: "OUT_VISIBLE\x1b]0;OUT_PWNED\x07".to_string(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            structured_content: None,
            nested_calls: None,
            is_error: false,
            duration_ms: None,
            timestamp: 1_700_000_000_001,
        }))
        .expect("append tool result");
    // A later system update uses `describe_system_update` for its title and
    // `system_message_update_text` for its body/section names.
    manager
        .append_message(Message::System(UserMessage {
            role: UserRole::System,
            content: vec![],
            tools_added: Some(vec![Tool {
                name: "upd\x1b]0;UPD_TOOL_PWNED\x07".to_string(),
                description: "UPD_TOOL_VISIBLE".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                constrained_sampling: None,
            }]),
            tools_removed: Some(vec![ToolReference {
                name: "removed\x1b]0;REMOVED_PWNED\x07".to_string(),
            }]),
            sections: Some(BTreeMap::from([(
                "upd\x1b]0;UPD_SEC_PWNED\x07".to_string(),
                Some("UPD_TEXT_VISIBLE\u{9e}UPD_TEXT_PWNED\u{9c}".to_string()),
            )])),
            timestamp: 1_700_000_000_002,
        }))
        .expect("append system update");

    let output = dir.join("out.html");
    export_session_to_html(manager.path(), Some(&output)).expect("export");
    let html = std::fs::read_to_string(&output).expect("read html");

    // No terminal control byte of any kind may survive anywhere in the
    // document (export is plain text: not even SGR is allowed).
    assert!(
        !html.contains('\x1b'),
        "ESC leaked into exported HTML: {:?}",
        &html[html.find('\x1b').unwrap_or(0)..]
    );
    assert!(!html.contains('\x07'), "BEL leaked into exported HTML");
    for ch in html.chars() {
        assert!(
            !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'),
            "control {ch:?} leaked into exported HTML"
        );
    }

    for marker in [
        "SYS_PWNED",
        "TOOL_PWNED",
        "PARAM_PWNED",
        "SEC_PWNED",
        "OUT_PWNED",
        "MODEL_PWNED",
        "CWD_PWNED",
        "UPD_TOOL_PWNED",
        "REMOVED_PWNED",
        "UPD_SEC_PWNED",
        "UPD_TEXT_PWNED",
    ] {
        assert!(
            !html.contains(marker),
            "injected payload {marker:?} leaked into exported HTML"
        );
    }
    for visible in [
        "SYS_VISIBLE",
        "SECTION_VISIBLE",
        "TOOL_VISIBLE",
        "[bash: EXPAND]",
        "OUT_VISIBLE",
        "UPD_TOOL_VISIBLE",
        "UPD_TEXT_VISIBLE",
        "projdir",
    ] {
        assert!(
            html.contains(visible),
            "visible text {visible:?} missing from exported HTML"
        );
    }
}
