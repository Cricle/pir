//! `grep` context lines come from the engine, not from re-reading matched
//! files (`8da141c2a`, #11705; upstream stopped reading matched files and
//! passes `--context N` to ripgrep instead).
//!
//! The in-process engine configures ripgrep's own `before_context` /
//! `after_context`, so overlapping context around nearby matches is merged:
//! shared lines appear once, the one intended output change of #11705.

#![cfg(feature = "grep-find")]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use serde_json::json;

fn fixture_file() -> (PathBuf, PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pir-grep-context-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create grep fixture dir");
    let file = dir.join("sample.txt");
    std::fs::write(&file, "zero\nmatch one\nbetween\nmatch two\ntail\n").expect("write fixture");
    (dir, file)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_output_merges_overlapping_windows() {
    let (dir, _file) = fixture_file();

    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let tool = registry.get("grep").expect("the grep tool is registered");
    let context = ToolContext::new(dir.to_string_lossy().into_owned());
    let result = tool
        .run(
            &json!({
                "pattern": "match",
                "path": dir.to_string_lossy(),
                "context": 1,
            }),
            &context,
        )
        .await;
    assert!(!result.is_error, "grep failed: {}", result.content);

    // Two matches in one file with one line of context each: the engine
    // merges the overlapping windows, so `between` appears once (#11705).
    let expected = concat!(
        "sample.txt-1- zero\n",
        "sample.txt:2: match one\n",
        "sample.txt-3- between\n",
        "sample.txt:4: match two\n",
        "sample.txt-5- tail",
    );
    assert_eq!(result.content, expected);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A lone CR is content, not a line break (upstream #11398/#11705): the
/// engine strips only the line terminator, so `before\rprogress` keeps its CR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_line_numbers_align_with_ripgrep_for_a_lone_cr() {
    let (dir, _file) = fixture_file();
    let file = dir.join("lone-cr.txt");
    std::fs::write(&file, "before\rprogress\nneedle\nafter").expect("write lone-CR fixture");

    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let tool = registry.get("grep").expect("the grep tool is registered");
    let context = ToolContext::new(dir.to_string_lossy().into_owned());
    let result = tool
        .run(
            &json!({
                "pattern": "needle",
                "path": file.to_string_lossy(),
                "context": 1,
            }),
            &context,
        )
        .await;
    assert!(!result.is_error, "grep failed: {}", result.content);

    assert_eq!(
        result.content,
        concat!(
            "lone-cr.txt-1- before\rprogress\n",
            "lone-cr.txt:2: needle\n",
            "lone-cr.txt-3- after",
        )
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #11705: exactly `limit` matches no longer claim the limit cut results off;
/// the notice needs one more match. The trailing context of the last allowed
/// match is still kept, and an extra match inside that window is rendered as
/// a context line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_limit_and_trailing_context_follow_upstream() {
    let (dir, _file) = fixture_file();
    let file = dir.join("limit.txt");

    let mut registry = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut registry);
    let tool = registry.get("grep").expect("the grep tool is registered");
    let context = ToolContext::new(dir.to_string_lossy().into_owned());

    // Exactly two matches for limit=2: no notice and no matchLimitReached.
    std::fs::write(&file, "match one\nmatch two\n").expect("write exact fixture");
    let result = tool
        .run(
            &json!({
                "pattern": "match",
                "path": file.to_string_lossy(),
                "limit": 2,
            }),
            &context,
        )
        .await;
    assert!(!result.is_error, "grep failed: {}", result.content);
    assert!(
        !result.content.contains("limit reached"),
        "exactly `limit` matches must not claim truncation: {}",
        result.content
    );
    assert_eq!(
        result
            .details
            .as_ref()
            .and_then(|details| details.get("matchLimitReached")),
        None
    );

    // Limit=2 with a third match inside the last match's two-line trailing
    // window: the third match is shown as context, the window end is kept,
    // and the line beyond it is dropped.
    std::fs::write(
        &file,
        "match one\nmatch two\nmatch three\nwindow end\nbeyond\n",
    )
    .expect("write window fixture");
    let result = tool
        .run(
            &json!({
                "pattern": "match",
                "path": file.to_string_lossy(),
                "limit": 2,
                "context": 2,
            }),
            &context,
        )
        .await;
    assert!(!result.is_error, "grep failed: {}", result.content);
    assert_eq!(
        result.content,
        concat!(
            "limit.txt:1: match one\n",
            "limit.txt:2: match two\n",
            "limit.txt-3- match three\n",
            "limit.txt-4- window end\n",
            "\n[2 matches limit reached. Use limit=4 for more, or refine pattern]",
        )
    );
    assert!(
        result.content.contains("2 matches limit reached"),
        "the third match proves the limit: {}",
        result.content
    );
    assert!(
        !result.content.contains("beyond"),
        "lines past the trailing window are not kept: {}",
        result.content
    );

    let _ = std::fs::remove_dir_all(&dir);
}
