// Integration tests (kept out of src/).

use pir_builtin_context_mode::*;
use serde_json::json;
use std::time::Duration;

fn request(value: serde_json::Value) -> Result<BatchRequest, String> {
    BatchRequest::parse(&value)
}

#[test]
fn parses_commands_and_queries() {
    let parsed = request(json!({
        "commands": ["git status", "  ", "cargo test"],
        "queries": ["failures", "warnings"]
    }))
    .unwrap();
    assert_eq!(parsed.commands, vec!["git status", "cargo test"]);
    assert_eq!(parsed.queries, vec!["failures", "warnings"]);
    assert_eq!(
        parsed.timeout,
        Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS)
    );
}

#[test]
fn rejects_missing_and_empty_commands_queries() {
    assert!(request(json!({ "queries": ["x"] }))
        .unwrap_err()
        .contains("`commands` is required"));
    assert!(request(json!({ "commands": ["  "], "queries": ["x"] }))
        .unwrap_err()
        .contains("`commands` is required"));
    assert!(request(json!({ "commands": ["ls"] }))
        .unwrap_err()
        .contains("`queries` is required"));
    assert!(request(json!({ "commands": ["ls"], "queries": [] }))
        .unwrap_err()
        .contains("`queries` is required"));
}

#[test]
fn enforces_command_and_query_bounds() {
    let too_many_commands: Vec<String> = (0..=MAX_COMMANDS)
        .map(|index| format!("cmd {index}"))
        .collect();
    assert!(
        request(json!({ "commands": too_many_commands, "queries": ["x"] }))
            .unwrap_err()
            .contains("at most 16 commands")
    );

    let too_many_queries: Vec<String> = (0..=MAX_SEARCH_QUERIES)
        .map(|index| format!("q{index}"))
        .collect();
    assert!(
        request(json!({ "commands": ["ls"], "queries": too_many_queries }))
            .unwrap_err()
            .contains("at most 32 queries")
    );

    let long_query = "x".repeat(MAX_QUERY_BYTES + 1);
    assert!(
        request(json!({ "commands": ["ls"], "queries": [long_query] }))
            .unwrap_err()
            .contains("at most 512 bytes")
    );
}

#[test]
fn accepts_exact_bounds() {
    let commands: Vec<String> = (0..MAX_COMMANDS).map(|i| format!("c{i}")).collect();
    let queries: Vec<String> = (0..MAX_SEARCH_QUERIES).map(|i| format!("q{i}")).collect();
    assert!(request(json!({ "commands": commands, "queries": queries })).is_ok());
    assert!(request(json!({
        "commands": ["ls"],
        "queries": ["x".repeat(MAX_QUERY_BYTES)]
    }))
    .is_ok());
}

#[test]
fn timeout_ms_wins_and_clamps() {
    assert_eq!(
        request(json!({
            "commands": ["ls"],
            "queries": ["x"],
            "timeout": 10,
            "timeoutMs": 5_000
        }))
        .unwrap()
        .timeout,
        Duration::from_secs(5)
    );
    assert_eq!(
        request(json!({
            "commands": ["ls"],
            "queries": ["x"],
            "timeout": 5,
            "timeoutMs": 999_000
        }))
        .unwrap()
        .timeout,
        Duration::from_secs_f64(MAX_TIMEOUT_SECS)
    );
    assert_eq!(
        request(json!({ "commands": ["ls"], "queries": ["x"], "timeout": 999 }))
            .unwrap()
            .timeout,
        Duration::from_secs_f64(MAX_TIMEOUT_SECS)
    );
    // Non-positive values fall through to the next source/default.
    assert_eq!(
        request(json!({
            "commands": ["ls"],
            "queries": ["x"],
            "timeoutMs": 0,
            "timeout": 7
        }))
        .unwrap()
        .timeout,
        Duration::from_secs(7)
    );
    assert_eq!(
        request(json!({ "commands": ["ls"], "queries": ["x"], "timeout": -1 }))
            .unwrap()
            .timeout,
        Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS)
    );
}

#[test]
fn source_is_stable_and_command_sensitive() {
    let commands = vec!["git status".to_owned(), "git log -1".to_owned()];
    let source = batch_source("C:/Work/Repo", &commands);
    assert!(source.starts_with("ctx_batch:"));
    assert_eq!(source.len(), "ctx_batch:".len() + 16);
    assert_eq!(source, batch_source("C:/Work/Repo", &commands));
    assert_ne!(source, batch_source("C:/Work/Repo", &[commands[0].clone()]));
    assert_ne!(
        source,
        batch_source("C:/Work/Other", &commands),
        "cwd participates in the label"
    );
}

#[test]
fn cap_reply_truncates_on_boundary() {
    let short = "half".to_owned();
    assert_eq!(cap_reply(short.clone()), short);

    let long = "é".repeat(OUTPUT_CAP);
    let capped = cap_reply(long);
    assert!(capped.ends_with("... [reply truncated at 16 KiB] ..."));
    assert!(capped.len() <= OUTPUT_CAP + 40);
    // The truncation marker is always appended and the prefix is valid UTF-8.
    assert!(capped.starts_with('é'));
}
