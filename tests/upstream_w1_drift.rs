//! W1 upstream-alignment drift fixes (UPDATING.md ledger): one place to pin the
//! behavior of the low-risk fixes, each with the upstream commit it follows.
//!
//! - D1 `441f5767c` — print mode exits 1 with a stderr warning when the final
//!   response was cut off at the model's output token limit.
//! - D2 `7bd01fb92` — "The upstream provider returned an error" is a
//!   transient, retryable provider error.
//! - D9 `15e7bdc14` — `serializeConversation()` omits thinking blocks.
//! - D13 `b6d505cd1` — a `// @options:` line that is not the first line fails
//!   before the script runs.
//! - D16 `f11e50f72` — codemode output joining is linear.
//! - D17 `4130bd5b3` — Node `--watch` notifications on the worker channel are
//!   ignored instead of failing the run.
//! - D5 `f4cca2dc2` — connecting never lists MCP resources; the report says a
//!   server offers resources (`resources: true`).
//! - D6 `67996ab75` — MCP `structuredContent` accepts any JSON value.
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ThinkingContent, ToolCall,
    UserMessage, UserRole,
};
use tokio::io::AsyncBufReadExt as _;

// ---------------------------------------------------------------------------
// D1 — print mode: truncated response warns and exits 1 (upstream 441f5767c)
// ---------------------------------------------------------------------------

/// The upstream print-mode.ts:161 text, verbatim.
#[test]
fn d1_truncation_warning_text_is_verbatim_upstream() {
    assert_eq!(
        pir::coding_agent::print::TRUNCATION_WARNING,
        "Response was truncated: the model reached its output token limit."
    );
}

#[test]
fn d1_truncation_notice_only_for_length_stop_reason() {
    use pir::coding_agent::print::truncation_notice;

    // The `length` stop reason warns (and the caller exits 1 for it).
    assert_eq!(
        truncation_notice(StopReason::Length),
        Some(pir::coding_agent::print::TRUNCATION_WARNING)
    );
    // Every other reason that reaches the text branch stays silent, exit 0.
    for reason in [
        StopReason::Stop,
        StopReason::ToolUse,
        StopReason::Pending,
        StopReason::Deferred,
        // Error/aborted take the Err path before the notice is consulted
        // (they never print partial text, like upstream).
        StopReason::Error,
        StopReason::Aborted,
    ] {
        assert_eq!(truncation_notice(reason), None, "reason: {reason:?}");
    }
}

// ---------------------------------------------------------------------------
// D2 — "The upstream provider returned an error" is retryable (7bd01fb92)
// ---------------------------------------------------------------------------

fn error_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "openai-chat".into(),
        provider: "openrouter".into(),
        model: "test-model".into(),
        stop_reason: StopReason::Error,
        error_message: Some(text.to_owned()),
        timestamp: 1,
        ..Default::default()
    }
}

#[test]
fn d2_upstream_provider_error_is_retryable() {
    use pir::ai::util::is_retryable_assistant_error;

    // #10842: the gateway wording "The upstream provider returned an error".
    for text in [
        "The upstream provider returned an error",
        "the upstream provider returned an error: gateway timeout",
        "Upstream provider returned an error",
        // The pre-existing OpenRouter wording must keep matching (#2264).
        "Provider returned error",
        "provider returned an error",
    ] {
        assert!(
            is_retryable_assistant_error(&error_message(text)),
            "expected retryable: {text}"
        );
    }
}

#[test]
fn d2_non_retryable_limit_text_still_wins() {
    use pir::ai::util::is_retryable_assistant_error;

    // Quota/billing exhaustion stays fail-fast even with provider-error
    // wording next to it (the non-retryable pattern is checked first).
    for text in [
        "The upstream provider returned an error: quota exceeded",
        "provider returned an error: insufficient_quota",
        "The upstream provider returned an error: Monthly usage limit reached",
    ] {
        assert!(
            !is_retryable_assistant_error(&error_message(text)),
            "expected non-retryable: {text}"
        );
    }
    // Non-error stop reasons are never retried.
    let mut ok = error_message("The upstream provider returned an error");
    ok.stop_reason = StopReason::Stop;
    assert!(!is_retryable_assistant_error(&ok));
}

// ---------------------------------------------------------------------------
// D1 prerequisite — a truncated response with no tool calls ends the run
// (upstream agent-loop.ts:263-272: `hasMoreToolCalls` is only set inside the
// `toolCalls.length > 0` branch). Without this the print-mode `length` exit-1
// path is unreachable: the loop re-requests the same capped response forever.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn d1_length_stop_without_tool_calls_ends_the_run() {
    use pir::agent::agent_loop::{run_with_options, LoopDeps, LoopOptions};
    use pir::ai::types::Context;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_stream = Arc::clone(&requests);
    let deps = LoopDeps {
        stream: Box::new(move |_request| {
            requests_for_stream.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                AssistantMessage {
                    api: "test".into(),
                    provider: "test".into(),
                    model: "test-model".into(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "partial".into(),
                        text_signature: None,
                    })],
                    stop_reason: StopReason::Length,
                    timestamp: 1,
                    ..Default::default()
                }
            })
        }),
        max_turns: 5,
        ..LoopDeps::default()
    };

    let result = run_with_options(deps, Context::default(), None, LoopOptions::default()).await;

    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "a length stop with no tool calls must not be re-requested"
    );
    assert_eq!(result.terminal.stop_reason, StopReason::Length);
    assert_eq!(result.turns, 1);
}

// ---------------------------------------------------------------------------
// D9 — serializeConversation omits thinking (15e7bdc14)
// ---------------------------------------------------------------------------

#[test]
fn d9_serialized_conversation_omits_thinking() {
    let mut arguments = BTreeMap::new();
    arguments.insert("path".to_owned(), serde_json::json!("a.ts"));
    let assistant = Message::Assistant(AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "test".into(),
        content: vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "private reasoning".into(),
                thinking_signature: Some("sig".into()),
                redacted: false,
            }),
            ContentBlock::Text(TextContent {
                text: "visible reply".into(),
                text_signature: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "tc1".into(),
                name: "read".into(),
                arguments,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ],
        stop_reason: StopReason::ToolUse,
        timestamp: 1,
        ..Default::default()
    });

    let serialized = pir::compaction::serialize_conversation(&[assistant]);
    // Upstream's compaction-serialization expectation, verbatim.
    assert_eq!(
        serialized,
        "[Assistant]: visible reply\n\n[Assistant tool calls]: read(path=\"a.ts\")"
    );
    assert!(
        !serialized.contains("thinking") && !serialized.contains("private reasoning"),
        "thinking must not reach the summarization request: {serialized}"
    );
}

#[test]
fn d9_thinking_only_assistant_message_contributes_nothing() {
    let assistant = Message::Assistant(AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "test".into(),
        content: vec![ContentBlock::Thinking(ThinkingContent {
            thinking: "only reasoning".into(),
            thinking_signature: None,
            redacted: false,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    });
    let user = Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: "what happened".into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    });
    assert_eq!(
        pir::compaction::serialize_conversation(&[user, assistant]),
        "[User]: what happened"
    );
}

// ---------------------------------------------------------------------------
// D13 — a misplaced `// @options:` line fails before the script runs
// (b6d505cd1)
// ---------------------------------------------------------------------------

#[test]
fn d13_misplaced_options_line_is_rejected_with_upstream_text() {
    let parse = pir::plugins::codemode::parse_codemode_source;

    let cases: &[(&str, usize)] = &[
        (
            "// count things\n// @options: {\"timeout_ms\": 1000}\nwhile (true) {}",
            2,
        ),
        (
            "\n  // @options: {\"timeout_ms\": 1000}\nwhile (true) {}",
            2,
        ),
        ("text(1)\r\n// @options: {\"timeout_ms\": 1}", 2),
        (
            "// @options: {\"timeout_ms\": 1}\ntext(1)\n// @options: {\"timeout_ms\": 2}",
            3,
        ),
    ];
    for (input, line) in cases {
        let error = parse(input).unwrap_err();
        assert_eq!(
            error,
            format!(
                "The // @options: line must be the first line of the input; found one on line {line}"
            ),
            "input: {input:?}"
        );
    }
}

#[test]
fn d13_first_line_options_still_apply_and_lookalikes_are_ignored() {
    let parse = pir::plugins::codemode::parse_codemode_source;

    // The legitimate first-line preamble still parses and applies.
    let (code, options) = parse("// @options: {\"timeout_ms\": 1000}\nreturn 1;").unwrap();
    assert_eq!(options.timeout_ms, Some(1000));
    assert_eq!(code, "\nreturn 1;");

    // A non-matching prefix and a string literal containing the prefix on a
    // later line are ordinary code.
    assert!(parse("// @optionsx {}\ntext(1)")
        .unwrap()
        .1
        .timeout_ms
        .is_none());
    assert!(parse("text(\"// @options: {}\")")
        .unwrap()
        .1
        .timeout_ms
        .is_none());
}

// ---------------------------------------------------------------------------
// D16 — codemode output joining is linear (f11e50f72)
// ---------------------------------------------------------------------------

#[test]
fn d16_render_output_is_linear_at_the_item_limit() {
    use pir::plugins::codemode::{render_output, OutputItem, MAX_OUTPUT_ITEMS};

    // Upstream's regression shape (#10678): the item limit of empty text
    // items. A quadratic join took seconds to minutes at this size.
    let items: Vec<OutputItem> = (0..MAX_OUTPUT_ITEMS)
        .map(|_| OutputItem {
            kind: "text",
            text: String::new(),
        })
        .collect();
    let started = Instant::now();
    let rendered = render_output(&items);
    let elapsed = started.elapsed();

    assert!(
        rendered.starts_with("==> text 1/100000 <=="),
        "rendered head: {}",
        &rendered[..rendered.len().min(60)]
    );
    assert!(rendered.contains("==> text 100000/100000 <=="));
    assert!(
        elapsed < Duration::from_secs(5),
        "rendering {MAX_OUTPUT_ITEMS} items took {elapsed:?}"
    );
}

#[test]
fn d16_render_output_keeps_the_upstream_layout() {
    use pir::plugins::codemode::{render_output, OutputItem};

    let text_items = |texts: &[&str]| -> Vec<OutputItem> {
        texts
            .iter()
            .map(|text| OutputItem {
                kind: "text",
                text: (*text).to_owned(),
            })
            .collect()
    };
    // A single text item carries no index header.
    assert_eq!(render_output(&text_items(&["one"])), "one".to_owned(),);
    // Multiple items get one header each, and a console block is appended.
    let mut items = text_items(&["a", "b"]);
    items.push(OutputItem {
        kind: "console",
        text: "log line".to_owned(),
    });
    assert_eq!(
        render_output(&items),
        "==> text 1/2 <==\na\n==> text 2/2 <==\nb\n\n<console_output>\nlog line\n</console_output>"
    );
}

// ---------------------------------------------------------------------------
// D17 — Node `--watch` notifications are ignored (4130bd5b3)
// ---------------------------------------------------------------------------

#[test]
fn d17_node_watch_notifications_are_recognized() {
    use pir::plugins::codemode::worker::is_node_watch_notification;

    // The two message shapes Node 24/26 `--watch` posts (nodejs/node#65044).
    assert!(is_node_watch_notification(
        r#"{"watch:require":["/worker.js"]}"#
    ));
    assert!(is_node_watch_notification(
        r#"{"watch:import":["file:///worker.js"]}"#
    ));
    assert!(is_node_watch_notification(
        r#"  { "watch:import": [], "other": 1 }  "#
    ));
    // Protocol frames and unrelated payloads are not notifications.
    assert!(!is_node_watch_notification(r#"{"v":1,"type":"ready"}"#));
    assert!(!is_node_watch_notification(
        r#"{"v":1,"type":"output","kind":"text","text":"watch:import"}"#
    ));
    assert!(!is_node_watch_notification("[]"));
    assert!(!is_node_watch_notification("not json"));
}

#[tokio::test]
async fn d17_frame_reader_skips_watch_notifications() {
    use pir::plugins::codemode::worker::read_event_line;

    let input = concat!(
        "{\"watch:require\":[\"/worker.js\"]}\n",
        "{\"watch:import\":[\"file:///worker.js\"]}\n",
        "{\"v\":1,\"type\":\"ready\"}\n"
    );
    let mut lines = tokio::io::BufReader::new(input.as_bytes()).lines();
    let line = read_event_line(&mut lines)
        .await
        .expect("read")
        .expect("one line");
    assert_eq!(line, "{\"v\":1,\"type\":\"ready\"}");
    assert!(read_event_line(&mut lines).await.expect("eof").is_none());
}

// ---------------------------------------------------------------------------
// D5 — no resource listing at connect; the report says `resources: true`
// (f4cca2dc2)
// ---------------------------------------------------------------------------

#[test]
fn d5_report_entry_serializes_resources_only_when_offered() {
    use pir::coding_agent::cli::McpCheckEntry;

    let with_resources = McpCheckEntry {
        name: "docs".into(),
        transport: "stdio".into(),
        source: "/tmp/mcp.json".into(),
        status: "ok".into(),
        tools: vec!["search".into()],
        resources: Some(true),
        error: None,
    };
    let json = serde_json::to_value(&with_resources).expect("serialize");
    assert_eq!(json["resources"], serde_json::json!(true));

    let without = McpCheckEntry {
        resources: None,
        ..with_resources
    };
    let json = serde_json::to_value(&without).expect("serialize");
    assert!(
        json.get("resources").is_none(),
        "servers without resources omit the field: {json}"
    );
}

/// Mock child process for the connect-time test below: speaks MCP over
/// stdin/stdout, advertises `tools` + `resources`, and records every
/// `resources/list` request it sees in `PIR_MOCK_RESOURCE_MARKER` (the file
/// must NOT appear: pi never lists resources while connecting).
#[test]
fn mock_mcp_resource_server_child() {
    if std::env::var("PIR_MOCK_MCP_RESOURCE_SERVER").as_deref() != Ok("1") {
        return;
    }
    let marker = std::env::var("PIR_MOCK_RESOURCE_MARKER").unwrap_or_default();
    let stdin = std::io::stdin();
    for line in std::io::BufRead::lines(stdin.lock()) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").cloned() else {
            continue;
        };
        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let reply = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {}, "resources": {} },
                "serverInfo": { "name": "resource-mock", "version": "1.0.0" },
            }),
            "ping" => serde_json::json!({}),
            "tools/list" => serde_json::json!({
                "tools": [{
                    "name": "search",
                    "description": "Search the drive",
                    "inputSchema": { "type": "object", "properties": {} },
                }],
            }),
            "resources/list" => {
                if !marker.is_empty() {
                    let _ = std::fs::write(&marker, b"listed");
                }
                serde_json::json!({
                    "resources": [{ "uri": "drive://big/1", "name": "one" }],
                })
            }
            other => {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("method not found: {other}") },
                    })
                );
                let _ = out.flush();
                continue;
            }
        };
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": reply })
        );
        let _ = out.flush();
    }
}

#[tokio::test]
async fn d5_connect_reports_resources_without_listing_them() {
    let temp = std::env::temp_dir().join(format!(
        "pir-drift-d5-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp).expect("temp dir");
    let marker = temp.join("resources-listed");
    let exe = std::env::current_exe().expect("current exe");
    let env: BTreeMap<String, String> = BTreeMap::from([
        ("PIR_MOCK_MCP_RESOURCE_SERVER".to_owned(), "1".to_owned()),
        (
            "PIR_MOCK_RESOURCE_MARKER".to_owned(),
            marker.to_string_lossy().into_owned(),
        ),
    ]);

    let manager = pir::mcp::manager::McpManager::without_log(&temp);
    let config: pir::config::mcp::McpServerConfig = serde_json::from_value(serde_json::json!({
        "command": exe.to_string_lossy(),
        "args": [
            "mock_mcp_resource_server_child",
            "--exact",
            "--nocapture",
        ],
        "env": env,
        "timeout": 20,
    }))
    .expect("stdio config");
    manager
        .registry()
        .register(pir::mcp::registry::McpServerEntry {
            name: "resource-mock".into(),
            config,
            source: pir::mcp::registry::McpServerSource::Extension("test".into()),
        });

    let (tools, has_resources) = manager
        .check_server_report("resource-mock", Duration::from_secs(20))
        .await
        .expect("check_server_report");
    manager.shutdown().await;

    assert_eq!(tools, vec!["search".to_owned()]);
    assert!(
        has_resources,
        "the server advertises the resources capability"
    );
    assert!(
        !marker.exists(),
        "connecting must not list resources (upstream #10526)"
    );

    let _ = std::fs::remove_dir_all(&temp);
}

// ---------------------------------------------------------------------------
// D6 — `structuredContent` accepts any JSON value (67996ab75)
// ---------------------------------------------------------------------------

fn mcp_result(structured: Option<serde_json::Value>) -> pir::mcp::protocol::CallToolResult {
    pir::mcp::protocol::CallToolResult {
        content: Vec::new(),
        structured_content: structured,
        is_error: None,
        meta: None,
    }
}

#[test]
fn d6_structured_content_accepts_any_json_value() {
    use pir::plugins::builtin::convert_mcp_result;

    // Arrays, null, and primitives all reach the model as their JSON text and
    // ride `structuredContent` in the script-facing result, exactly as sent.
    for value in [
        serde_json::json!([{ "id": 1 }, { "id": 2 }]),
        serde_json::Value::Null,
        serde_json::json!("text"),
        serde_json::json!(3),
        serde_json::json!(false),
    ] {
        let result = convert_mcp_result("docs", "search", mcp_result(Some(value.clone())));
        assert!(!result.is_error, "value: {value}");
        assert_eq!(
            result.content,
            serde_json::to_string_pretty(&value).expect("pretty"),
            "value: {value}"
        );
        let script_result = result
            .structured_content
            .as_ref()
            .expect("script-facing CallToolResult");
        assert_eq!(script_result["structuredContent"], value);
        assert_eq!(script_result["isError"], serde_json::json!(false));
    }
}

#[test]
fn d6_object_structured_content_and_content_blocks_are_unchanged() {
    use pir::mcp::protocol::ContentBlock as McpContentBlock;
    use pir::plugins::builtin::convert_mcp_result;

    // Text content blocks keep winning over the structured fallback.
    let mut call_result = mcp_result(Some(serde_json::json!({ "n": 1 })));
    call_result.content = vec![McpContentBlock {
        kind: "text".into(),
        text: Some("nope".into()),
        ..Default::default()
    }];
    let result = convert_mcp_result("docs", "t", call_result);
    assert_eq!(result.content, "nope");
    assert_eq!(
        result.structured_content.as_ref().expect("structured")["structuredContent"],
        serde_json::json!({ "n": 1 })
    );

    // An absent `structuredContent` stays absent.
    let result = convert_mcp_result("docs", "t", mcp_result(None));
    assert!(result.content.is_empty());
    assert!(
        result
            .structured_content
            .as_ref()
            .expect("structured")
            .get("structuredContent")
            .is_none(),
        "the key is not set when the server sent none"
    );
}
