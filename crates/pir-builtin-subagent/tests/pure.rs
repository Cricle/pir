use pir_builtin_subagent::*;
use pir_plugin_api::BuiltinPlugin;
use serde_json::json;

#[test]
fn frontmatter_parses_name_description_tools_and_model() {
    let text =
        "---\nname: scout\ndescription: looks\ntools: read, bash\nmodel: p/m\n---\nbody here\n";
    let parsed = parse_agent_markdown(text).expect("frontmatter");
    assert_eq!(parsed.name.as_deref(), Some("scout"));
    assert_eq!(parsed.description.as_deref(), Some("looks"));
    assert_eq!(
        parsed.tools,
        Some(vec!["read".to_owned(), "bash".to_owned()])
    );
    assert_eq!(parsed.model.as_deref(), Some("p/m"));
    assert_eq!(parsed.body, "body here\n");
}

#[test]
fn frontmatter_rules_reject_incomplete_documents() {
    assert!(split_frontmatter("no frontmatter").is_none());
    assert!(parse_agent_markdown("---\nname: only\n---\nbody").is_none());
    // BOM and CRLF are both tolerated.
    let text = "\u{feff}---\r\nname: a\r\ndescription: d\r\n---\r\nb";
    assert!(parse_agent_markdown(text).is_some());
}

#[test]
fn tool_lists_accept_both_yaml_spellings() {
    let string = serde_yaml::from_str("read, bash").expect("yaml");
    let seq = serde_yaml::from_str("[read, bash]").expect("yaml");
    assert_eq!(
        parse_tool_list(&string),
        Some(vec!["read".into(), "bash".into()])
    );
    assert_eq!(
        parse_tool_list(&seq),
        Some(vec!["read".into(), "bash".into()])
    );
    let empty = serde_yaml::from_str("3").expect("yaml");
    assert_eq!(parse_tool_list(&empty), None);
}

#[test]
fn failure_classification_covers_truncation_and_stderr() {
    let mut result = SingleResult::failed("a", "t", "boom".to_owned(), None);
    assert!(is_failed(&result));
    assert_eq!(result_output(&result), "boom");

    result = SingleResult::placeholder("a", "t");
    assert!(result.pending);
    assert!(!is_failed(&result));

    let mut length = SingleResult::placeholder("a", "t");
    length.pending = false;
    length.stop_reason = Some("length".to_owned());
    assert!(is_failed(&length), "truncated output is not a success");
}

#[test]
fn final_output_is_the_last_assistant_text() {
    let messages = vec![
        json!({"role": "assistant", "content": [{"type": "text", "text": "first"}]}),
        json!({"role": "user", "content": [{"type": "text", "text": "ignored"}]}),
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "x"},
            {"type": "text", "text": "second"}
        ]}),
    ];
    assert_eq!(get_final_output(&messages).as_deref(), Some("second"));
    assert_eq!(get_final_output(&[]), None);
}

#[test]
fn bounded_transcript_drops_thinking_and_tool_result_internals() {
    let messages = vec![
        json!({"role": "system", "content": [{"type": "text", "text": "sys"}]}),
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "secret"},
            {"type": "toolCall", "arguments": {"path": "a"}},
            {"type": "text", "text": "kept"}
        ]}),
        json!({"role": "toolResult", "content": [],
               "details": {"x": 1}, "structuredContent": {}, "nestedCalls": []}),
    ];
    let (retained, dropped) = bounded_messages_json(&messages);
    let rows = retained.as_array().expect("array");
    assert_eq!(dropped, 0);
    assert_eq!(rows.len(), 3);
    let assistant_blocks = rows[1]["content"].as_array().expect("blocks");
    assert!(assistant_blocks.iter().all(|b| b["type"] != "thinking"));
    assert!(rows[2].get("details").is_none());
    assert!(rows[2].get("nestedCalls").is_none());
}

#[test]
fn transcript_budget_drops_oldest_non_system_messages() {
    let mut messages = vec![json!({"role": "system", "content": [{"type": "text", "text": "s"}]})];
    for index in 0..40 {
        messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": "x".repeat(8 * 1024)}],
            "index": index,
        }));
    }
    let (retained, dropped) = bounded_messages_json(&messages);
    let rows = retained.as_array().expect("array");
    assert!(dropped > 0, "the budget must actually trim");
    assert_eq!(rows[0]["role"], "system", "system message survives");
    assert!(rows.len() > 1, "the final message is never dropped");
}

#[test]
fn parallel_aggregate_counts_successes() {
    let mut ok = SingleResult::placeholder("a", "t");
    ok.pending = false;
    ok.messages = vec![json!({"role": "assistant",
        "content": [{"type": "text", "text": "done"}]})];
    let bad = SingleResult::failed("b", "t", "nope".to_owned(), None);
    let text = aggregate_parallel_text(&[ok, bad]);
    assert!(text.starts_with("Parallel: 1/2 succeeded"));
    assert!(text.contains("### [a] completed"));
    assert!(text.contains("### [b] failed (error)"));
    // The upstream summaries format separates blocks with a horizontal rule;
    // a plain blank line was an unnoticed deviation in this crate.
    assert!(text.contains("\n\n---\n\n"), "{text}");
}

#[test]
fn chain_helpers_substitute_the_previous_placeholder() {
    assert_eq!(apply_previous("do {previous}", "x"), "do x");
    assert_eq!(apply_previous("no placeholder", "x"), "no placeholder");
    assert_eq!(
        results_by_agent(&[
            SingleResult::placeholder("a", "t"),
            SingleResult::placeholder("a", "u")
        ])
        .get("a")
        .copied(),
        Some(2)
    );
}

#[test]
fn truncation_is_utf8_safe() {
    let long = "é".repeat(PER_TASK_OUTPUT_CAP);
    let cut = truncate_parallel_output(&long);
    assert!(cut.len() <= PER_TASK_OUTPUT_CAP + 200);
    assert!(cut.contains("Output truncated"));
    assert_eq!(truncate_parallel_output("short"), "short");
}

#[test]
fn plugin_identity() {
    assert_eq!(SubagentPlugin::new().info().path, "builtin:subagent");
}
