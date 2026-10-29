//! The `tool-repair` builtin's Step 2 grammar pass: the ten leaked-grammar
//! families, the recover/strip modes, and the safety gates, ported from
//! `grammar-repair.ts` (`:193-283` gates, `:296-322` dispatch, `:324-1225`
//! parsers).
//!
//! The per-family fixtures are the shapes enumerated in the package's
//! `docs/tool-call-grammar-leakage-survey.md` §A–J (the survey is the
//! behavior truth the package ships instead of tests); the gate tests pin the
//! scope doc's §1.3 list: gated-off no-op, recover vs strip, `requireKnownTool`
//! (including the empty-set rule), fenced-code immunity, dangling markers,
//! mangled names, existing-native-toolCall strip-only, and the recovered id
//! format/uniqueness.

#![cfg(feature = "tool-repair")]

use std::collections::BTreeSet;

use serde_json::{json, Value as JsonValue};

use pir::ai::types::{
    AssistantMessage, ContentBlock, StopReason, TextContent, ThinkingContent, ToolCall,
};
use pir::plugins::tool_repair::aliases::GRAMMAR_NAMES;
use pir::plugins::tool_repair::config::{GrammarRepairConfig, GrammarRepairMode};
use pir::plugins::tool_repair::grammar::{
    leak_model_matcher, parse_tool_grammar_leaks, repair_message_grammar_leaks,
    resolve_grammar_repair_for_model, GrammarRepairResult, RecoveredToolCall,
};
use pir::plugins::tool_repair::repair::{
    repair_message_with_grammar, LiveRepairStatus, LiveToolSchema, MessageRepairOutcome,
    SchemaCheck,
};

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

fn unknown_tools() -> BTreeSet<String> {
    BTreeSet::new()
}

fn known(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Every family on, recovery on, the tool gate off (family fixtures test the
/// parsers, not the registry).
fn open_config(grammars: &[&'static str]) -> GrammarRepairConfig {
    GrammarRepairConfig {
        enabled: true,
        grammars: grammars.to_vec(),
        mode: GrammarRepairMode::Recover,
        require_known_tool: false,
        debug: false,
        leak_models: Vec::new(),
    }
}

/// Recovery with `requireKnownTool` on.
fn gated_config(grammars: &[&'static str]) -> GrammarRepairConfig {
    GrammarRepairConfig {
        require_known_tool: true,
        ..open_config(grammars)
    }
}

/// Strip-only mode.
fn strip_config(grammars: &[&'static str]) -> GrammarRepairConfig {
    GrammarRepairConfig {
        mode: GrammarRepairMode::Strip,
        ..open_config(grammars)
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

fn thinking_block(text: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: text.to_owned(),
        thinking_signature: None,
        redacted: false,
    })
}

fn assistant_message(content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        content,
        stop_reason: StopReason::Stop,
        model: "test-model".to_owned(),
        ..AssistantMessage::default()
    }
}

fn text_message(text: &str) -> AssistantMessage {
    assistant_message(vec![text_block(text)])
}

fn run(
    text: &str,
    config: &GrammarRepairConfig,
    tools: &BTreeSet<String>,
) -> (AssistantMessage, GrammarRepairResult) {
    let mut message = text_message(text);
    let result = repair_message_grammar_leaks(&mut message, config, tools);
    (message, result)
}

fn part_text(message: &AssistantMessage, index: usize) -> String {
    match &message.content[index] {
        ContentBlock::Text(part) => part.text.clone(),
        ContentBlock::Thinking(part) => part.thinking.clone(),
        other => panic!("expected a text/thinking part, got {other:?}"),
    }
}

fn appended_calls(message: &AssistantMessage) -> Vec<&ToolCall> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn call_arguments(call: &ToolCall) -> JsonValue {
    JsonValue::Object(call.arguments.clone().into_iter().collect())
}

fn recovered(name: &str, arguments: JsonValue, grammar: &'static str) -> RecoveredToolCall {
    RecoveredToolCall {
        name: name.to_owned(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        grammar,
    }
}

fn names_of(result: &GrammarRepairResult) -> Vec<String> {
    result
        .recovered_calls
        .iter()
        .map(|call| call.name.clone())
        .collect()
}

/// A DSML block (`<｜DSML｜…>`) with one invoke and one typed parameter.
fn dsml_block(name: &str, parameter: &str, value: &str) -> String {
    format!(
        concat!(
            "<｜DSML｜tool_calls>\n",
            "<｜DSML｜invoke name=\"{name}\">\n",
            "<｜DSML｜parameter name=\"{parameter}\" string=\"true\">{value}</｜DSML｜parameter>\n",
            "</｜DSML｜invoke>\n",
            "</｜DSML｜tool_calls>"
        ),
        name = name,
        parameter = parameter,
        value = value
    )
}

/// A `<tool_call>` wrapper with a JSON body (the qwen/granite Hermes shape).
fn json_tool_call(name: &str, arguments: JsonValue) -> String {
    format!(
        "<tool_call>{}</tool_call>",
        json!({ "name": name, "arguments": arguments })
    )
}

// ---------------------------------------------------------------------------
// §A. DeepSeek DSML (`parseDsml` :324-344, `parseDsmlDanglingMarkers` :346-366)
// ---------------------------------------------------------------------------

#[test]
fn dsml_double_bar_block_recovers_and_strips() {
    let leaked = concat!(
        "<｜｜DSML｜｜tool_calls>\n",
        "<｜｜DSML｜｜invoke name=\"code_exec\">\n",
        "<｜｜DSML｜｜parameter name=\"language\" string=\"true\">python</｜｜DSML｜｜parameter>\n",
        "</｜｜DSML｜｜invoke>\n",
        "</｜｜DSML｜｜tool_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    assert!(result.changed);
    assert_eq!(result.stripped_ranges, 1);
    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "code_exec",
            json!({ "language": "python" }),
            "dsml"
        )]
    );
    // The whole block is the strip range, so nothing is left.
    assert_eq!(part_text(&message, 0), "");
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(appended_calls(&message).len(), 1);
}

#[test]
fn dsml_single_bar_string_false_value_is_json() {
    let leaked = concat!(
        "<｜DSML｜tool_calls>\n",
        "<｜DSML｜invoke name=\"fetch\">\n",
        "<｜DSML｜parameter name=\"url\" string=\"false\">[\"x\"]</｜DSML｜parameter>\n",
        "</｜DSML｜invoke>\n",
        "</｜DSML｜tool_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    // `string="false"` means the value is JSON (survey §A key detail).
    assert_eq!(
        result.recovered_calls,
        vec![recovered("fetch", json!({ "url": ["x"] }), "dsml")]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn dsml_ascii_pipe_variant_recovers() {
    // The proxy-transcoded variant: `< | DSML | …>` (survey §A).
    let leaked = concat!(
        "< | DSML | tool_calls>\n",
        "< | DSML | invoke name=\"builtin_web_search\">\n",
        "< | DSML | parameter name=\"additionalContext\" string=\"true\">ctx</ | DSML | parameter>\n",
        "</ | DSML | invoke>\n",
        "</ | DSML | tool_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "builtin_web_search",
            json!({ "additionalContext": "ctx" }),
            "dsml"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn dsml_multiple_invokes_share_one_strip_range() {
    let leaked = concat!(
        "<｜DSML｜tool_calls>\n",
        "<｜DSML｜invoke name=\"read\"><｜DSML｜parameter name=\"path\" string=\"true\">/a</｜DSML｜parameter></｜DSML｜invoke>\n",
        "<｜DSML｜invoke name=\"read\"><｜DSML｜parameter name=\"path\" string=\"true\">/b</｜DSML｜parameter></｜DSML｜invoke>\n",
        "</｜DSML｜tool_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    // One outer block is one strip range, but every closed invoke inside is a
    // candidate: identical ranges are not duplicates (`selectCandidates`).
    assert_eq!(result.stripped_ranges, 2);
    assert_eq!(names_of(&result), vec!["read", "read"]);
    assert_eq!(result.recovered_calls[0].arguments["path"], json!("/a"));
    assert_eq!(result.recovered_calls[1].arguments["path"], json!("/b"));
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn dsml_no_leading_bar_block_recovers() {
    let leaked = concat!(
        "<DSML｜tool_calls>\n",
        "<DSML｜invoke name=\"bash\">\n",
        "<DSML｜parameter name=\"command\" string=\"true\">ls</DSML｜parameter>\n",
        "</DSML｜invoke>\n",
        "</DSML｜tool_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered("bash", json!({ "command": "ls" }), "dsml")]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn dsml_direct_json_body_without_invoke_is_not_recovered() {
    // `parseDsmlArguments`' direct-JSON fallback is only reachable through an
    // `<invoke>`; an outer block with a bare JSON body produces no candidate
    // from `parseDsml`, so only the markers are stripped as dangling ones.
    let leaked = concat!(
        "<DSML｜function_calls>\n",
        "{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n",
        "</DSML｜function_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(
        part_text(&message, 0),
        "{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}"
    );
}

// ---------------------------------------------------------------------------
// §B. Anthropic-style `<invoke>` / `<parameter>` (`:668-740`)
// ---------------------------------------------------------------------------

#[test]
fn invoke_minimax_namespaced_wrapper_recovers() {
    let leaked = concat!(
        "<minimax:tool_call>\n",
        "<invoke name=\"search_web\">\n",
        "<parameter name=\"query_list\">[\"weather\"]</parameter>\n",
        "</invoke>\n",
        "</minimax:tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["invoke"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "search_web",
            json!({ "query_list": ["weather"] }),
            "invoke"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn invoke_standalone_block_recovers() {
    let leaked = concat!(
        "working on it\n",
        "<invoke name=\"read\">\n",
        "<parameter name=\"path\">/tmp/x</parameter>\n",
        "</invoke>\n",
        "done"
    );
    let (message, result) = run(leaked, &open_config(&["invoke"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered("read", json!({ "path": "/tmp/x" }), "invoke")]
    );
    assert_eq!(part_text(&message, 0), "working on it\n\ndone");
}

#[test]
fn invoke_malformed_minimax_brackets_recovers() {
    // MiniMax M2's bracket-dropped variant: `invoke name="x">` and
    // `parameter name="y">val parameter>` with no angle brackets (survey §B).
    let leaked = concat!(
        "here is the call:\n",
        "invoke name=\"search_web\">\n",
        "parameter name=\"query\">weather parameter>"
    );
    let (message, result) = run(leaked, &open_config(&["invoke"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "search_web",
            json!({ "query": "weather" }),
            "invoke"
        )]
    );
    assert_eq!(part_text(&message, 0), "here is the call:");
}

// ---------------------------------------------------------------------------
// §C. Qwen / Hermes (`parseToolCallXml` :742-785, `parseQwenFunctionBody` :811-829)
// ---------------------------------------------------------------------------

#[test]
fn qwen_xml_function_parameter_shape_recovers() {
    let leaked = concat!(
        "<tool_call>\n",
        "<function=get_weather>\n",
        "<parameter=location>Paris</parameter>\n",
        "</function>\n",
        "</tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["qwen"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_weather",
            json!({ "location": "Paris" }),
            "qwen"
        )]
    );
    assert_eq!(result.stripped_ranges, 1);
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn qwen_tools_wrapper_with_json_body_recovers() {
    // The Qwen2.5-Coder oddball: `<tools>` + JSON (survey §C).
    let leaked = concat!(
        "<tools>\n",
        "{\"name\": \"get_weather\", \"arguments\": {\"location\": \"San Francisco, CA\"}}\n",
        "</tools>"
    );
    let (message, result) = run(leaked, &open_config(&["qwen"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_weather",
            json!({ "location": "San Francisco, CA" }),
            "qwen"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn qwen_bare_function_without_wrapper_recovers() {
    let leaked = concat!(
        "<function=edit>\n",
        "<parameter=path>/x</parameter>\n",
        "<parameter=oldText>a</parameter>\n",
        "<parameter=newText>b</parameter>\n",
        "</function>"
    );
    let (message, result) = run(leaked, &open_config(&["qwen"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "edit",
            json!({ "path": "/x", "oldText": "a", "newText": "b" }),
            "qwen"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

// ---------------------------------------------------------------------------
// §D. Kimi sentinels (`parseKimi` :429-455, dangling :484-562)
// ---------------------------------------------------------------------------

#[test]
fn kimi_sentinel_section_recovers() {
    let leaked = concat!(
        "<|tool_calls_section_begin|>\n",
        "<|tool_call_begin|>functions.web_search:0<|tool_call_argument_begin|>{\"query\": \"kimi k2\"}<|tool_call_end|>\n",
        "<|tool_calls_section_end|>"
    );
    let (message, result) = run(leaked, &open_config(&["kimi"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "web_search",
            json!({ "query": "kimi k2" }),
            "kimi"
        )]
    );
    // The sentinel tokens nested in the section are overlapping strip-only
    // candidates and are dropped by the selection rule.
    assert_eq!(result.stripped_ranges, 1);
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn kimi_loose_tool_call_closed_by_thinking_recovers() {
    let leaked = concat!(
        "<tool_call>\n",
        "{\"name\": \"read\", \"arguments\": {\"path\": \"/x\"}}\n",
        "</thinking>"
    );
    let (message, result) = run(leaked, &open_config(&["kimi"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered("read", json!({ "path": "/x" }), "kimi")]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn kimi_dangling_section_opener_is_stripped_not_recovered() {
    // A stream that died right after the opener: the marker is removed but
    // nothing is promoted (README: dangling markers still get stripped).
    let leaked = "partial: <|tool_calls_section_begin|>";
    let (message, result) = run(leaked, &open_config(&["kimi"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(result.stripped_ranges, 1);
    assert_eq!(part_text(&message, 0), "partial:");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[test]
fn kimi_dangling_opener_extends_over_id_and_argument_marker() {
    // `findBestKimiUnclosedEnd`: the last sentinel after the opener extends the
    // range over a `functions.name:idx` id (the argument marker carries none).
    let leaked = "<|tool_calls_section_begin|><|tool_call_begin|>functions.bash:0";
    let (message, result) = run(leaked, &open_config(&["kimi"]), &unknown_tools());

    assert!(result.recovered_calls.is_empty());
    assert_eq!(result.stripped_ranges, 1);
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn orphan_tool_call_closer_is_stripped() {
    let leaked = "plain text </tool_call>\n";
    let (message, result) = run(leaked, &open_config(&["kimi"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(part_text(&message, 0), "plain text");
}

#[test]
fn unparseable_tool_call_pair_is_stripped_only() {
    // An empty `<tool_call>` pair: a candidate exists, parses to nothing, and
    // is strip-only (the empty-argument safety net).
    let leaked = "<tool_call>\n</tool_call>";
    let (message, result) = run(leaked, &open_config(&["kimi"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(part_text(&message, 0), "");
}

// ---------------------------------------------------------------------------
// §E. Mistral sentinels (`parseMistral` :595-646, bare JSON :882-906)
// ---------------------------------------------------------------------------

#[test]
fn mistral_bracketed_json_array_recovers() {
    let leaked = "[TOOL_CALLS] [{\"name\": \"calculator\", \"arguments\": {\"operation\": \"2+2\"}, \"id\": \"VvvODy9mT\"}]";
    let (message, result) = run(leaked, &open_config(&["mistral"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "calculator",
            json!({ "operation": "2+2" }),
            "mistral"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn mistral_v11_sentinel_recovers() {
    let leaked = "[TOOL_CALLS]get_weather[CALL_ID]abc123456[ARGS]{\"location\": \"Paris\"}";
    let (message, result) = run(leaked, &open_config(&["mistral"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_weather",
            json!({ "location": "Paris" }),
            "mistral"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn mistral_dropped_sentinel_bare_json_still_recovers() {
    // `skip_special_tokens` dropped the marker: the bare JSON object is the
    // recovery path.
    let leaked = "I will call {\"name\": \"read\", \"arguments\": {\"path\": \"/x\"}} now.";
    let (message, result) = run(leaked, &open_config(&["mistral"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered("read", json!({ "path": "/x" }), "mistral")]
    );
    assert_eq!(part_text(&message, 0), "I will call  now.");
}

// ---------------------------------------------------------------------------
// §F. Llama `<|python_tag|>` (`parseLlamaPythonTag` :847-880)
// ---------------------------------------------------------------------------

#[test]
fn llama_python_tag_json_recovers() {
    let leaked =
        "<|python_tag|>{\"name\": \"get_weather\", \"arguments\": {\"location\": \"Paris\"}}";
    let (message, result) = run(leaked, &open_config(&["llama"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_weather",
            json!({ "location": "Paris" }),
            "llama"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn llama_python_tag_pythonic_recovers() {
    let leaked = "<|python_tag|>get_weather(location=\"Paris\", days=3)";
    let (message, result) = run(leaked, &open_config(&["llama"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_weather",
            json!({ "location": "Paris", "days": 3 }),
            "llama"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn llama_bare_json_array_recovers() {
    // Llama 3.2 emits no start token (survey §F): the bare array is the leak.
    let leaked = "[{\"name\": \"read\", \"arguments\": {\"path\": \"/y\"}}]";
    let (message, result) = run(leaked, &open_config(&["llama"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered("read", json!({ "path": "/y" }), "llama")]
    );
    assert_eq!(part_text(&message, 0), "");
}

// ---------------------------------------------------------------------------
// §G. GLM `<arg_key>` / `<arg_value>` (`parseGlmToolCallBody` :831-845)
// ---------------------------------------------------------------------------

#[test]
fn glm_arg_key_value_block_recovers() {
    let leaked = concat!(
        "<tool_call>get_weather\n",
        "<arg_key>city</arg_key>\n",
        "<arg_value>Beijing</arg_value>\n",
        "</tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["glm"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_weather",
            json!({ "city": "Beijing" }),
            "glm"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn glm_same_line_name_and_json_values_recovers() {
    let leaked = concat!(
        "<tool_call>edit<arg_key>path</arg_key><arg_value>\"/x\"</arg_value>",
        "<arg_key>count</arg_key><arg_value>2</arg_value></tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["glm"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "edit",
            json!({ "path": "/x", "count": 2 }),
            "glm"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn glm_zero_arg_call_is_stripped_but_not_recovered() {
    // `<tool_call>get_current_date</tool_call>` is a valid GLM variant, but an
    // empty argument object is never promoted (`:230-238`).
    let leaked = "<tool_call>get_current_date</tool_call>";
    let (message, result) = run(leaked, &open_config(&["glm"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(result.stripped_ranges, 1);
    assert_eq!(part_text(&message, 0), "");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

// ---------------------------------------------------------------------------
// §H. IBM Granite (`parseToolCallXml` with the granite branch)
// ---------------------------------------------------------------------------

#[test]
fn granite_json_tool_call_recovers() {
    let leaked = json_tool_call("search", json!({ "query": "granite" }));
    let (message, result) = run(&leaked, &open_config(&["granite"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "search",
            json!({ "query": "granite" }),
            "granite"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn granite_escaped_string_arguments_unwrap() {
    // Granite's known quirk: `arguments` arrives as an escaped *string*.
    let leaked = concat!(
        "<tool_call>",
        "{\"name\": \"search\", \"arguments\": \"{\\\"query\\\": \\\"granite\\\"}\"}",
        "</tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["granite"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "search",
            json!({ "query": "granite" }),
            "granite"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn granite_pythonic_line_recovers() {
    // Granite 3.3/4.0 H-Small emits pythonic calls (survey §H).
    let leaked = "I'll check the weather.\nsearch(query=\"granite\", limit=3)\n";
    let (message, result) = run(leaked, &open_config(&["granite"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "search",
            json!({ "query": "granite", "limit": 3 }),
            "granite"
        )]
    );
    assert_eq!(part_text(&message, 0), "I'll check the weather.");
}

// ---------------------------------------------------------------------------
// §I. MiniMax-Text-01 (`parseMiniMaxText01` :648-666)
// ---------------------------------------------------------------------------

#[test]
fn minimax_text01_typescript_fence_recovers() {
    let leaked = concat!(
        "<function_call>```typescript\n",
        "functions.get_current_weather({\"location\": \"Shanghai\"})\n",
        "```"
    );
    let (message, result) = run(leaked, &open_config(&["minimax-text"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "get_current_weather",
            json!({ "location": "Shanghai" }),
            "minimax-text"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn minimax_text01_without_fence_recovers() {
    let leaked = "<function_call>functions.x({\"a\": 1})";
    let (message, result) = run(leaked, &open_config(&["minimax-text"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered("x", json!({ "a": 1 }), "minimax-text")]
    );
    assert_eq!(part_text(&message, 0), "");
}

// ---------------------------------------------------------------------------
// §J. OLMo 3 `<function_calls>` (`parseOlmo` :927-937)
// ---------------------------------------------------------------------------

#[test]
fn olmo_pythonic_calls_recover() {
    let leaked = concat!(
        "<function_calls>\n",
        "get_current_weather(location=\"Shanghai\")\n",
        "get_forecast(location=\"Shanghai\", days=3)\n",
        "</function_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["olmo"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![
            recovered(
                "get_current_weather",
                json!({ "location": "Shanghai" }),
                "olmo"
            ),
            recovered(
                "get_forecast",
                json!({ "location": "Shanghai", "days": 3 }),
                "olmo"
            ),
        ]
    );
    // Both candidates carry the wrapper's range; identical ranges coexist.
    assert_eq!(result.stripped_ranges, 2);
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn olmo_pythonic_json_literals_are_accepted() {
    let leaked = concat!(
        "<function_calls>\n",
        "set_flag(enabled=true, note=null, tags=[\"a\"])\n",
        "</function_calls>"
    );
    let (message, result) = run(leaked, &open_config(&["olmo"]), &unknown_tools());

    assert_eq!(
        result.recovered_calls,
        vec![recovered(
            "set_flag",
            json!({ "enabled": true, "note": JsonValue::Null, "tags": ["a"] }),
            "olmo"
        )]
    );
    assert_eq!(part_text(&message, 0), "");
}

// ---------------------------------------------------------------------------
// The extractor (`parseToolGrammarLeaks` :285-294)
// ---------------------------------------------------------------------------

#[test]
fn extractor_returns_calls_without_gates_or_strip_only() {
    let leaked = dsml_block("bash", "command", "ls");
    let calls = parse_tool_grammar_leaks(&leaked, GRAMMAR_NAMES);
    assert_eq!(
        calls,
        vec![recovered("bash", json!({ "command": "ls" }), "dsml")]
    );

    // Family selection is honoured.
    assert!(parse_tool_grammar_leaks(&leaked, &["glm"]).is_empty());

    // Strip-only candidates (dangling markers) are never returned.
    assert!(
        parse_tool_grammar_leaks("partial <|tool_calls_section_begin|>", GRAMMAR_NAMES).is_empty()
    );
}

#[test]
fn extractor_does_not_apply_the_mangled_name_recovery() {
    let leaked = dsml_block("bash_1_234", "command", "ls");
    let calls = parse_tool_grammar_leaks(&leaked, GRAMMAR_NAMES);
    assert_eq!(calls[0].name, "bash_1_234");
}

// ---------------------------------------------------------------------------
// Modes and the enable gate
// ---------------------------------------------------------------------------

#[test]
fn disabled_config_leaves_the_message_byte_identical() {
    let leaked = dsml_block("bash", "command", "ls");
    let mut message = text_message(&leaked);
    let before = message.clone();

    let result = repair_message_grammar_leaks(
        &mut message,
        &GrammarRepairConfig::default(),
        &known(&["bash"]),
    );

    assert_eq!(result, GrammarRepairResult::default());
    assert_eq!(message, before);
}

#[test]
fn disabled_config_is_a_no_op_through_the_full_pipeline() {
    // The Step 1a entry point keeps the grammar pass off (default config).
    let leaked = dsml_block("bash", "command", "ls");
    let mut message = text_message(&leaked);
    let before = message.clone();
    let check = |_: &JsonValue, _: &JsonValue| -> SchemaCheck {
        panic!("no tool schema is passed, so the validator must not run")
    };

    let outcome = repair_message_with_grammar(
        &mut message,
        Some("glm-4.6"),
        &[],
        &GrammarRepairConfig::default(),
        &known(&["bash"]),
        &check,
        false,
    );

    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(message, before);
}

#[test]
fn recover_mode_appends_and_flips_the_stop_reason() {
    let leaked = dsml_block("bash", "command", "ls");
    let (message, result) = run(&leaked, &open_config(&["dsml"]), &unknown_tools());

    let calls = appended_calls(&message);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "bash");
    assert_eq!(call_arguments(calls[0]), json!({ "command": "ls" }));
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(result.recovered_calls.len(), 1);
}

#[test]
fn recover_mode_preserves_terminal_stop_reasons() {
    // The repair hook runs before the loop's terminal-reason dispatch
    // (`agent_loop.rs:1358` vs the `Error|Aborted` exit at `:1461` and the
    // `Length` truncated-call path at `:1490`). Raising `ToolUse` here would
    // skip both, so the provider's terminal reason must survive recovery.
    let leaked = dsml_block("bash", "command", "ls");
    for stop_reason in [StopReason::Length, StopReason::Error, StopReason::Aborted] {
        let mut message = text_message(&leaked);
        message.stop_reason = stop_reason;
        let result =
            repair_message_grammar_leaks(&mut message, &open_config(&["dsml"]), &unknown_tools());

        // The call is still recovered (a `Length` turn fails it as truncated;
        // an errored/aborted turn is dropped by the request transform), but
        // the reason the provider reported is not overwritten.
        assert_eq!(result.recovered_calls.len(), 1, "{stop_reason:?}");
        assert_eq!(appended_calls(&message).len(), 1, "{stop_reason:?}");
        assert_eq!(message.stop_reason, stop_reason);
    }

    // A non-terminal reason keeps upstream's promotion.
    let (message, _) = run(&leaked, &open_config(&["dsml"]), &unknown_tools());
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[test]
fn strip_mode_removes_markup_without_recovering() {
    let leaked = dsml_block("bash", "command", "ls");
    let (message, result) = run(&leaked, &strip_config(&["dsml"]), &unknown_tools());

    // Strip-only candidates still count, but nothing is promoted and the stop
    // reason is untouched.
    assert_eq!(result.stripped_ranges, 1);
    assert!(result.recovered_calls.is_empty());
    assert!(result.changed);
    assert_eq!(part_text(&message, 0), "");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(appended_calls(&message).is_empty());
}

#[test]
fn thinking_content_is_scanned_too() {
    let leaked = dsml_block("bash", "command", "ls");
    let mut message = assistant_message(vec![thinking_block(&leaked)]);
    let result =
        repair_message_grammar_leaks(&mut message, &open_config(&["dsml"]), &unknown_tools());

    assert!(result.changed);
    assert_eq!(names_of(&result), vec!["bash"]);
    assert_eq!(part_text(&message, 0), "");
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[test]
fn text_and_thinking_leaks_both_recover_in_content_order() {
    let text = dsml_block("read", "path", "/txt");
    let thinking = dsml_block("bash", "command", "ls");
    let mut message = assistant_message(vec![thinking_block(&thinking), text_block(&text)]);
    let result =
        repair_message_grammar_leaks(&mut message, &open_config(&["dsml"]), &unknown_tools());

    assert_eq!(names_of(&result), vec!["bash", "read"]);
    assert_eq!(part_text(&message, 0), "");
    assert_eq!(part_text(&message, 1), "");
    let calls: Vec<String> = appended_calls(&message)
        .iter()
        .map(|call| call.name.clone())
        .collect();
    assert_eq!(calls, vec!["bash", "read"]);
}

// ---------------------------------------------------------------------------
// `resolveGrammarRepairForModel` (`:158-170`)
// ---------------------------------------------------------------------------

#[test]
fn leak_models_auto_enable_per_model() {
    let config = GrammarRepairConfig {
        enabled: false,
        leak_models: vec!["kimi".to_owned(), "qwen3".to_owned()],
        ..GrammarRepairConfig::default()
    };

    assert!(resolve_grammar_repair_for_model(&config, Some("moonshotai/kimi-k2.6")).enabled);
    assert!(resolve_grammar_repair_for_model(&config, Some("QWEN3-30B")).enabled);
    assert!(!resolve_grammar_repair_for_model(&config, Some("gpt-5")).enabled);
    assert!(!resolve_grammar_repair_for_model(&config, None).enabled);
    assert!(!resolve_grammar_repair_for_model(&config, Some("")).enabled);

    // `enabled: true` wins outright, and the fragment list is not consulted.
    let forced = GrammarRepairConfig {
        enabled: true,
        ..GrammarRepairConfig::default()
    };
    assert!(resolve_grammar_repair_for_model(&forced, Some("gpt-5")).enabled);
    // An uncompilable fragment never enables anything.
    let broken = GrammarRepairConfig {
        enabled: false,
        leak_models: vec!["(".to_owned()],
        ..GrammarRepairConfig::default()
    };
    assert!(!resolve_grammar_repair_for_model(&broken, Some("kimi")).enabled);
}

#[test]
fn leak_models_gate_drives_the_message_pipeline() {
    let leaked = dsml_block("bash", "command", "ls");
    let config = GrammarRepairConfig {
        enabled: false,
        leak_models: vec!["kimi".to_owned()],
        ..GrammarRepairConfig::default()
    };
    let check = |_: &JsonValue, _: &JsonValue| -> SchemaCheck {
        panic!("no tool schema is passed, so the validator must not run")
    };

    let mut leaking = text_message(&leaked);
    let outcome = repair_message_with_grammar(
        &mut leaking,
        Some("moonshotai/kimi-k2.6"),
        &[],
        &config,
        &known(&["bash"]),
        &check,
        false,
    );
    assert!(outcome.changed());
    assert_eq!(appended_calls(&leaking).len(), 1);

    let mut untouched = text_message(&leaked);
    let before = untouched.clone();
    let outcome = repair_message_with_grammar(
        &mut untouched,
        Some("gpt-5"),
        &[],
        &config,
        &known(&["bash"]),
        &check,
        false,
    );
    assert_eq!(outcome, MessageRepairOutcome::Unchanged);
    assert_eq!(untouched, before);
}

// ---------------------------------------------------------------------------
// The safety gates (`:178-191`, `:230-238`, `:251`, `:1065-1068`, `:1212-1216`)
// ---------------------------------------------------------------------------

#[test]
fn require_known_tool_with_an_empty_set_recovers_and_strips_nothing() {
    // A name-bearing candidate with an empty registry is dropped whole — not
    // even the markup goes away (`:1065-1068`: `knownTools.size > 0 && …`).
    // (`<tool_call>` JSON has no strip-only companion candidates; DSML's
    // dangling markers are covered separately below.)
    let leaked = json_tool_call("bash", json!({ "command": "ls" }));
    let mut message = text_message(&leaked);
    let before = message.clone();

    let result =
        repair_message_grammar_leaks(&mut message, &gated_config(&["qwen"]), &unknown_tools());

    assert_eq!(result, GrammarRepairResult::default());
    assert_eq!(message, before);
}

#[test]
fn dsml_dangling_markers_strip_even_when_the_tool_gate_rejects_the_block() {
    // The DSML marker sweep is strip-only and bypasses `requireKnownTool`, so
    // an unterminated block whose invoke parse yields nothing still loses its
    // raw markup: with no parseable candidate the markers themselves are the
    // strip-only candidates (a well-formed block dedupes them — same as TS).
    let leaked = concat!(
        "<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"mystery_tool\">\n",
        "<｜DSML｜parameter name=\"command\" string=\"true\">ls"
    );
    let (message, result) = run(leaked, &gated_config(&["dsml"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert!(!part_text(&message, 0).contains("DSML"));
    assert!(part_text(&message, 0).contains("ls"));
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[test]
fn require_known_tool_recovers_a_known_name() {
    let leaked = dsml_block("bash", "command", "ls");
    let (message, result) = run(&leaked, &gated_config(&["dsml"]), &known(&["bash"]));

    assert_eq!(names_of(&result), vec!["bash"]);
    assert_eq!(appended_calls(&message).len(), 1);
    assert_eq!(part_text(&message, 0), "");
}

#[test]
fn unknown_names_are_neither_recovered_nor_stripped() {
    let leaked = json_tool_call("mystery_tool", json!({ "x": 1 }));
    let (message, result) = run(&leaked, &gated_config(&["qwen"]), &known(&["bash"]));

    assert_eq!(result, GrammarRepairResult::default());
    assert_eq!(part_text(&message, 0), leaked);
}

#[test]
fn strip_only_markers_bypass_the_known_tool_gate() {
    // Dangling markers carry no name, so the gate cannot apply: they are
    // stripped even with an empty registry (README: the raw marker must not
    // persist).
    let leaked = "partial <|tool_calls_section_begin|>";
    let mut message = text_message(leaked);
    let result =
        repair_message_grammar_leaks(&mut message, &gated_config(&["kimi"]), &unknown_tools());

    assert!(result.changed);
    assert_eq!(part_text(&message, 0), "partial");
}

#[test]
fn mangled_name_recovers_the_longest_known_prefix() {
    let leaked = dsml_block("bash_1_234", "command", "ls");
    let (message, result) = run(&leaked, &gated_config(&["dsml"]), &known(&["bash"]));

    assert_eq!(names_of(&result), vec!["bash"]);
    assert_eq!(appended_calls(&message)[0].name, "bash");
}

#[test]
fn mangled_name_prefers_the_longest_known_tool() {
    // `read_file_1_2` must collapse to `read_file`, never to `read`
    // (`:181-188`).
    let leaked = dsml_block("read_file_1_2", "path", "/x");
    let (_, result) = run(
        &leaked,
        &gated_config(&["dsml"]),
        &known(&["read", "read_file"]),
    );

    assert_eq!(names_of(&result), vec!["read_file"]);
}

#[test]
fn mangled_suffix_with_real_characters_is_never_cut() {
    let leaked = json_tool_call("read_multiple_files", json!({ "path": "/x" }));
    let (message, result) = run(&leaked, &gated_config(&["qwen"]), &known(&["read"]));

    // The suffix `_multiple_files` carries letters: no longest-prefix cut, no
    // recovery, and — because the gate also guards the strip — no removal.
    assert_eq!(result, GrammarRepairResult::default());
    assert_eq!(part_text(&message, 0), leaked);
}

#[test]
fn existing_native_tool_call_means_strip_only() {
    let leaked = dsml_block("bash", "command", "ls");
    let mut message = assistant_message(vec![
        text_block(&leaked),
        ContentBlock::ToolCall(ToolCall {
            id: "native-1".to_owned(),
            name: "bash".to_owned(),
            arguments: [("command".to_owned(), json!("pwd"))].into_iter().collect(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }),
    ]);
    let result =
        repair_message_grammar_leaks(&mut message, &open_config(&["dsml"]), &unknown_tools());

    // The shadow text is stripped, the duplicate call is not appended, and the
    // stop reason is left to the provider's native `toolUse`.
    assert!(result.changed);
    assert_eq!(result.stripped_ranges, 1);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(part_text(&message, 0), "");
    assert_eq!(appended_calls(&message).len(), 1);
    assert_eq!(appended_calls(&message)[0].id, "native-1");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[test]
fn fenced_code_blocks_are_ignored() {
    let leaked = concat!(
        "Here is the shape:\n\n```text\n",
        "<tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"/x\"}}</tool_call>\n",
        "```\n\nThat is all."
    );
    let mut message = text_message(leaked);
    let before = message.clone();
    let result =
        repair_message_grammar_leaks(&mut message, &open_config(&["granite"]), &unknown_tools());

    assert_eq!(result, GrammarRepairResult::default());
    assert_eq!(message, before);
}

#[test]
fn only_unfenced_leaks_are_touched() {
    let leaked = concat!(
        "fenced:\n```\n<tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"/fenced\"}}</tool_call>\n```\n",
        "real:\n<tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"/real\"}}</tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["granite"]), &unknown_tools());

    assert_eq!(names_of(&result), vec!["read"]);
    assert_eq!(result.recovered_calls[0].arguments["path"], json!("/real"));
    assert_eq!(
        part_text(&message, 0),
        "fenced:\n```\n<tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"/fenced\"}}</tool_call>\n```\nreal:"
    );
}

#[test]
fn empty_argument_candidates_are_skipped_without_stopping_the_strip() {
    // `<tool_call>write</tool_call>` — malformed grammar parses to `{}` and
    // must not dispatch as a call with missing required fields (`:226-238`).
    let leaked = "before <tool_call>write</tool_call> after";
    let (message, result) = run(leaked, &open_config(&["glm"]), &unknown_tools());

    assert!(result.changed);
    assert!(result.recovered_calls.is_empty());
    assert_eq!(part_text(&message, 0), "before  after");
    assert_eq!(message.stop_reason, StopReason::Stop);
}

// ---------------------------------------------------------------------------
// Recovered ids (`makeRecoveredToolCallId` :1098-1100)
// ---------------------------------------------------------------------------

#[test]
fn recovered_ids_carry_the_grammar_stamp_and_index() {
    let leaked = concat!(
        "<tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"/a\"}}</tool_call>\n",
        "<tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"/b\"}}</tool_call>"
    );
    let (message, result) = run(leaked, &open_config(&["granite"]), &unknown_tools());
    assert_eq!(result.recovered_calls.len(), 2);

    let ids: Vec<String> = appended_calls(&message)
        .iter()
        .map(|call| call.id.clone())
        .collect();
    assert_eq!(ids.len(), 2);
    for (index, id) in ids.iter().enumerate() {
        let prefix = "tool_repair_granite_";
        assert!(id.starts_with(prefix), "{id}");
        let suffix = format!("_{index}");
        assert!(id.ends_with(&suffix), "{id}");
        let stamp = id
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(&suffix))
            .expect("grammar stamp");
        assert!(!stamp.is_empty(), "empty stamp in {id}");
        assert!(
            stamp
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch.is_ascii_lowercase()),
            "stamp is base36: {id}"
        );
    }
    assert_ne!(ids[0], ids[1], "ids must be unique");
}

#[test]
fn recovered_ids_sanitize_the_grammar_name() {
    let leaked = "<function_call>functions.x({\"a\": 1})";
    let (message, _) = run(leaked, &open_config(&["minimax-text"]), &unknown_tools());

    let id = &appended_calls(&message)[0].id;
    assert!(id.starts_with("tool_repair_minimax_text_"), "{id}");
}

#[test]
fn recovered_ids_stay_unique_across_messages() {
    let leaked = dsml_block("bash", "command", "ls");
    let mut first = text_message(&leaked);
    let mut second = text_message(&leaked);
    repair_message_grammar_leaks(&mut first, &open_config(&["dsml"]), &unknown_tools());
    repair_message_grammar_leaks(&mut second, &open_config(&["dsml"]), &unknown_tools());

    let first_id = &appended_calls(&first)[0].id;
    let second_id = &appended_calls(&second)[0].id;
    assert_ne!(first_id, second_id);
}

// ---------------------------------------------------------------------------
// Pipeline step order (grammar before live-schema repair)
// ---------------------------------------------------------------------------

#[test]
fn recovered_calls_go_through_the_live_schema_repair() {
    let leaked = json_tool_call("read", json!({ "file_path": "/x" }));
    let mut message = text_message(&leaked);
    let tools = vec![LiveToolSchema::new(
        "read",
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    )];
    let check = |arguments: &JsonValue, _schema: &JsonValue| -> SchemaCheck {
        if arguments.get("path").is_some() {
            SchemaCheck::Accepted
        } else {
            SchemaCheck::Rejected
        }
    };

    let outcome = repair_message_with_grammar(
        &mut message,
        None,
        &tools,
        &open_config(&["granite"]),
        &known(&["read"]),
        &check,
        false,
    );

    // The grammar pass appended the call, then the live-schema pass renamed
    // the alias and committed the candidate (`tool-repair.ts:121-147`).
    assert!(outcome.changed());
    let report = outcome.report().expect("a repaired call is reported");
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls[0].status, LiveRepairStatus::Recovered);
    assert_eq!(
        call_arguments(appended_calls(&message)[0]),
        json!({ "path": "/x" })
    );
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[test]
fn the_glm_token_strip_and_the_grammar_gate_stay_separate() {
    let leaked = dsml_block("bash", "command", "ls");
    let token_leak = json!({ "<arg_key>path</arg_key>": "<arg_value>/foo</arg_value>" });

    // GLM model, grammar pass disabled by default: the token strip fires, the
    // leaked grammar text does not.
    let mut message = assistant_message(vec![
        text_block(&leaked),
        ContentBlock::ToolCall(ToolCall {
            id: "c1".to_owned(),
            name: "read".to_owned(),
            arguments: token_leak
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }),
    ]);
    let check = |_: &JsonValue, _: &JsonValue| -> SchemaCheck {
        panic!("no tool schema is passed, so the validator must not run")
    };
    let outcome = repair_message_with_grammar(
        &mut message,
        Some("glm-4.6"),
        &[],
        &GrammarRepairConfig::default(),
        &known(&["bash"]),
        &check,
        false,
    );
    assert!(outcome.changed());
    assert_eq!(part_text(&message, 0), leaked);
    assert_eq!(
        call_arguments(appended_calls(&message)[0]),
        json!({ "path": "/foo" })
    );

    // Non-GLM model, grammar pass enabled: the grammar strips the leaked text
    // and the token strip does not run (the tags stay in the native call's
    // arguments). The native call also blocks recovery — strip-only.
    let mut other = assistant_message(vec![
        text_block(&leaked),
        ContentBlock::ToolCall(ToolCall {
            id: "c1".to_owned(),
            name: "read".to_owned(),
            arguments: token_leak
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }),
    ]);
    let outcome = repair_message_with_grammar(
        &mut other,
        Some("gpt-5"),
        &[],
        &open_config(&["dsml"]),
        &unknown_tools(),
        &check,
        false,
    );
    assert!(outcome.changed());
    let calls = appended_calls(&other);
    assert_eq!(calls.len(), 1, "existing native tool call is strip-only");
    assert_eq!(call_arguments(calls[0]), token_leak);
    assert_eq!(part_text(&other, 0), "");
}

// ---------------------------------------------------------------------------
// `removeRanges` cleanup (`:1070-1080`)
// ---------------------------------------------------------------------------

#[test]
fn range_removal_collapses_the_whitespace_it_leaves() {
    // A block on its own line leaves a blank line; `\n{3,}` collapses to two
    // and the result is trimmed.
    let leaked = concat!(
        "line one\n\n",
        "<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"bash\">\n",
        "<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n",
        "</｜DSML｜invoke>\n</｜DSML｜tool_calls>",
        "\n\n\nline two"
    );
    let (message, _) = run(leaked, &open_config(&["dsml"]), &unknown_tools());

    assert_eq!(part_text(&message, 0), "line one\n\nline two");
}

// ---------------------------------------------------------------------------
// `leakModels` matcher cache (`resolveGrammarRepairForModel`, `:158-170`)
// ---------------------------------------------------------------------------

/// The per-model gate runs on every assistant message; the pre-fix shape
/// compiled `(?i)<pattern>` for every configured fragment on every call. The
/// matcher accessor must return the one cached compiled regex (pointer
/// identity), and an uncompilable fragment is cached as a negative instead of
/// being retried.
#[test]
fn leak_model_matchers_are_compiled_once_and_reused() {
    let first =
        leak_model_matcher("glm-4(\\.[0-9])?").expect("the configured pattern must compile");
    let second = leak_model_matcher("glm-4(\\.[0-9])?").expect("the cached pattern must resolve");
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "two calls for one pattern must return the same compiled regex"
    );
    assert!(
        first.is_match("GLM-4.6"),
        "the matcher stays case-insensitive"
    );

    assert!(
        leak_model_matcher("(").is_none(),
        "an invalid fragment resolves to no matcher"
    );
    assert!(
        leak_model_matcher("(").is_none(),
        "the negative result is cached and stays stable"
    );

    // The gate itself still uses the cache: a disabled config whose
    // `leakModels` matches the model resolves to enabled.
    let config = GrammarRepairConfig {
        enabled: false,
        leak_models: vec!["glm-4(\\.[0-9])?".to_owned()],
        ..open_config(&["glm"])
    };
    assert!(resolve_grammar_repair_for_model(&config, Some("GLM-4.6")).enabled);
    assert!(!resolve_grammar_repair_for_model(&config, Some("gpt-5")).enabled);
}
