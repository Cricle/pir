//! M7: `grep` with context materializes a file's line vector once per run.
//!
//! The context form re-reads each matching file through a per-run cache
//! (`read_file_lines`). The cache used to store `Vec<String>` and deep-copy
//! the whole line vector on every hit (and once more on insert); it now
//! stores `Arc<Vec<String>>`, so a file with several matches clones the `Arc`.
//! The output contract is unchanged: the two matches below must render the
//! same context blocks, byte for byte.

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
async fn context_output_is_unchanged_for_a_multi_match_file() {
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

    // Two matches in one file, each with one line of context: the second
    // match's context re-uses the cached line vector, and line 3 appears in
    // both blocks (exactly the pre-fix output).
    let expected = concat!(
        "sample.txt-1- zero\n",
        "sample.txt:2: match one\n",
        "sample.txt-3- between\n",
        "sample.txt-3- between\n",
        "sample.txt:4: match two\n",
        "sample.txt-5- tail",
    );
    assert_eq!(result.content, expected);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A lone CR is not a line break to ripgrep, which numbers lines at LF only
/// (upstream issue #11398). Context reads must split the same way: splitting
/// on a bare CR too put the following lines one number too high, so the
/// context block showed `progress` as the match and dropped `after`. The CR
/// is stripped for display, joining the two halves of the line.
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
            "lone-cr.txt-1- beforeprogress\n",
            "lone-cr.txt:2: needle\n",
            "lone-cr.txt-3- after",
        )
    );

    let _ = std::fs::remove_dir_all(&dir);
}
