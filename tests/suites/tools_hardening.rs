//! Focused security and output-bound tests for the coding-agent tools.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pir::agent::tool_execution::{AgentTool, ToolContext, ToolRegistry};
use pir::coding_agent::register_core_tools;

fn temp_workspace() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    // The sequence counter guarantees uniqueness even when parallel tests
    // sample the (coarse-grained) Windows clock at the same instant.
    std::env::temp_dir().join(format!(
        "pir-tools-{nonce}-{sequence}-{}",
        std::process::id()
    ))
}

fn tool(registry: &ToolRegistry, name: &str) -> Arc<dyn AgentTool> {
    registry.get(name).expect("core tool should be registered")
}

async fn run_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    context: &ToolContext,
) -> String {
    run_tool_checked(registry, name, args, context).await.0
}

/// Like [`run_tool`], but keeps the tool's error flag for tests that assert
/// success explicitly.
async fn run_tool_checked(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    context: &ToolContext,
) -> (String, bool) {
    let result = tool(registry, name).run(&args, context).await;
    (result.content, result.is_error)
}

#[tokio::test]
async fn file_tools_reject_parent_traversal() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let outside = root
        .parent()
        .expect("temporary directory has a parent")
        .join("pir-tools-escaped.txt");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "write",
        serde_json::json!({"path": "../pir-tools-escaped.txt", "content": "blocked"}),
        &context,
    )
    .await;

    assert!(result.contains("escapes the workspace root"));
    assert!(!outside.exists());
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn edit_requires_exactly_one_match() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("file.txt"), "needle\nneedle\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "edit",
        serde_json::json!({"path": "file.txt", "oldText": "needle", "newText": "changed"}),
        &context,
    )
    .await;

    // Upstream edit-diff.ts uniqueness rule (occurrences counted in fuzzy space).
    assert!(
        result.contains("Found 2 occurrences of the text in"),
        "unexpected error: {result}"
    );
    assert!(result.contains("must be unique"));
    assert_eq!(
        tokio::fs::read_to_string(root.join("file.txt"))
            .await
            .expect("read fixture"),
        "needle\nneedle\n"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn edit_multi_block_edits_apply_all() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(
        root.join("multi.txt"),
        "alpha: one\nbeta: two\ngamma: three\n",
    )
    .await
    .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = tool(&registry, "edit")
        .run(
            &serde_json::json!({
                "path": "multi.txt",
                "edits": [
                    {"oldText": "alpha: one", "newText": "alpha: 1"},
                    {"oldText": "gamma: three", "newText": "gamma: 3"}
                ]
            }),
            &context,
        )
        .await;

    // Upstream: the model-visible text is ONLY the success line; the unified
    // patch is UI-only details.
    assert_eq!(
        result.content,
        "Successfully replaced 2 block(s) in multi.txt."
    );
    let details = result.details.expect("edit carries the patch in details");
    assert!(
        details["patch"].as_str().unwrap().contains("--- multi.txt"),
        "expected diff header in details: {details}"
    );
    assert!(
        details["patch"].as_str().unwrap().contains("+alpha: 1"),
        "expected diff body in details: {details}"
    );
    assert_eq!(
        tokio::fs::read_to_string(root.join("multi.txt"))
            .await
            .expect("read fixture"),
        "alpha: 1\nbeta: two\ngamma: 3\n"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn edit_rejects_overlapping_edits() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("file.txt"), "abcdef\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "edit",
        serde_json::json!({
            "path": "file.txt",
            "edits": [
                {"oldText": "abcd", "newText": "x"},
                {"oldText": "cdef", "newText": "y"}
            ]
        }),
        &context,
    )
    .await;

    assert!(result.contains("overlap"), "unexpected error: {result}");
    assert_eq!(
        tokio::fs::read_to_string(root.join("file.txt"))
            .await
            .expect("read fixture"),
        "abcdef\n"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn edit_preserves_bom_and_crlf() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let original = "\u{FEFF}hello world\r\nsecond line\r\n";
    tokio::fs::write(root.join("crlf.txt"), original)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    // oldText uses LF; the tool matches in normalized space and restores CRLF.
    let result = run_tool(
        &registry,
        "edit",
        serde_json::json!({"path": "crlf.txt", "oldText": "hello world\nsecond line", "newText": "hi\nlast line"}),
        &context,
    )
    .await;

    assert!(
        result.contains("Successfully replaced 1 block(s)"),
        "unexpected result: {result}"
    );
    let after = tokio::fs::read(root.join("crlf.txt"))
        .await
        .expect("read fixture");
    assert_eq!(
        String::from_utf8(after).expect("utf-8"),
        "\u{FEFF}hi\r\nlast line\r\n"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn read_binary_file_errors_cleanly() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let mut bytes = vec![0x89u8, b'P', b'N', b'G'];
    bytes.extend(std::iter::repeat_n(0u8, 64));
    tokio::fs::write(root.join("image.bin"), &bytes)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "image.bin"}),
        &context,
    )
    .await;

    assert_eq!(result, "Cannot read 'image.bin': binary file");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_limit_stops_at_100_matches() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let content: String = (0..150).map(|i| format!("match{i} filler\n")).collect();
    tokio::fs::write(root.join("many.txt"), content)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "match", "path": "."}),
        &context,
    )
    .await;

    let match_lines = result
        .lines()
        .filter(|line| line.starts_with("many.txt:"))
        .count();
    assert_eq!(
        match_lines, 100,
        "expected exactly 100 match lines: {result}"
    );
    assert!(
        result.contains("[100 matches limit reached"),
        "expected limit notice: {result}"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_match_text_collection_is_bounded() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    // 9 MiB of matching lines (1 KiB each): over the 8 MiB retained-text
    // budget and under the 100k match limit, so only the byte budget can
    // stop the searcher. The engine used to return every match line before
    // the tool could apply its own cap.
    let line = format!("needle {}\n", "x".repeat(1016));
    let mut body = String::with_capacity(line.len() * 9216);
    for _ in 0..9216 {
        body.push_str(&line);
    }
    tokio::fs::write(root.join("many.txt"), &body)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = tool(&registry, "grep")
        .run(
            &serde_json::json!({"pattern": "needle", "path": ".", "limit": 100_000}),
            &context,
        )
        .await;

    assert!(!result.is_error, "grep failed: {}", result.content);
    let details = result.details.expect("details");
    assert_eq!(
        details["matchByteLimitReached"],
        serde_json::json!(8 * 1024 * 1024),
        "{details}"
    );
    assert!(
        details.get("truncation").is_some(),
        "the display is still truncated to 50KB: {details}"
    );
    assert!(
        result.content.contains("8.0MB of match data collected"),
        "the byte cap must be reported: {}",
        result.content
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_reports_caps_that_actually_clamped_the_request() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("few.txt"), "match one\nmatch two\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = tool(&registry, "grep")
        .run(
            &serde_json::json!({
                "pattern": "match",
                "path": ".",
                "limit": 150_000,
                "context": 1_500,
            }),
            &context,
        )
        .await;

    assert!(!result.is_error, "grep failed: {}", result.content);
    assert!(
        result.content.contains(
            "[context clamped to 1000; requested 1500. limit clamped to 100000; requested 150000]"
        ),
        "both real caps must be stated explicitly, on one line: {}",
        result.content
    );
    // The notice is text-only: the structured details keep their pre-clamp
    // shape (no clamp keys, and with two matches neither limit fired).
    assert!(
        result.details.is_none(),
        "clamp notices must not enter structured details: {:?}",
        result.details
    );
    // The model-visible rows are unaffected by the caps.
    assert!(result.content.contains("few.txt:1: match one"));
    assert!(result.content.contains("few.txt:2: match two"));

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_within_caps_emits_no_clamp_notice() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("few.txt"), "match one\nmatch two\nmatch three\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({
            "pattern": "match",
            "path": ".",
            "limit": 2,
            "context": 0,
        }),
        &context,
    )
    .await;

    assert!(
        !result.contains("clamped"),
        "a request inside the caps must not mention clamping: {result}"
    );
    // The pre-existing limit notice is still exactly the old one.
    assert!(
        result.contains("[2 matches limit reached. Use limit=4 for more, or refine pattern]"),
        "the normal limit notice is unchanged: {result}"
    );

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_clamp_notice_also_rides_a_no_match_result() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("plain.txt"), "nothing here\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "absent", "path": ".", "limit": 1e300}),
        &context,
    )
    .await;
    assert_eq!(
        result, "No matches found\n[limit clamped to 100000; requested 1e+300]",
        "a clamped no-match result states the cap without changing the row contract"
    );

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_truncates_long_lines() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let long_line = format!("needle{}\n", "x".repeat(2000));
    tokio::fs::write(root.join("long.txt"), long_line)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "needle", "path": "."}),
        &context,
    )
    .await;

    assert!(
        result.contains("... [truncated]"),
        "expected truncation marker: {result}"
    );
    assert!(result.contains("Some lines truncated to 500 chars"));
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_no_matches_is_a_successful_result() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("plain.txt"), "nothing to see here\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = tool(&registry, "grep")
        .run(
            &serde_json::json!({"pattern": "zzz-not-present", "path": "."}),
            &context,
        )
        .await;

    assert!(
        !result.is_error,
        "no matches is success: {}",
        result.content
    );
    assert_eq!(result.content, "No matches found");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_glob_ignore_case_literal_and_context_params() {
    // Skipped when ripgrep is unavailable (mirrors the other rg-backed tests,
    // which simply require `rg` on PATH).
    if which("rg").is_none() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("code.rs"), "Needle HERE\nother\nneedle there\n")
        .await
        .expect("write fixture");
    tokio::fs::write(root.join("notes.md"), "needle in markdown\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    // glob restricts the search to *.md files.
    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "needle", "path": ".", "glob": "*.md"}),
        &context,
    )
    .await;
    assert!(
        result.contains("notes.md:1:") && !result.contains("code.rs"),
        "glob must filter files: {result}"
    );

    // ignoreCase matches both spellings.
    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "needle here", "path": ".", "ignoreCase": true}),
        &context,
    )
    .await;
    assert!(
        result.contains("Needle HERE"),
        "ignoreCase must match case-insensitively: {result}"
    );

    // literal treats the pattern as a plain string (regex metachars match
    // literally): "a.b" does not match the file's "axb" line, while the
    // regex reading does.
    tokio::fs::write(root.join("axb.txt"), "axb\n")
        .await
        .expect("write fixture");
    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "a.b", "path": "."}),
        &context,
    )
    .await;
    assert!(
        result.contains("axb.txt"),
        "regex mode matches the metachar: {result}"
    );
    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "a.b", "path": ".", "literal": true}),
        &context,
    )
    .await;
    assert_eq!(result, "No matches found", "literal mode: {result}");

    // context prints the surrounding block with the upstream separators.
    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "other", "path": ".", "context": 1}),
        &context,
    )
    .await;
    assert!(
        result.contains("code.rs-1- Needle HERE"),
        "leading context line: {result}"
    );
    assert!(result.contains("code.rs:2: other"), "match line: {result}");
    assert!(
        result.contains("code.rs-3- needle there"),
        "trailing context line: {result}"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

/// Whether `executable` resolves on PATH (best effort; mirrors how the shell
/// tests probe for bash availability).
fn which(executable: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let mut names = vec![executable.to_owned()];
    if cfg!(windows) {
        names.push(format!("{executable}.exe"));
    }
    for dir in std::env::split_paths(&path_var) {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[tokio::test]
async fn find_uses_fd_and_reports_relative_paths() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(root.join("sub"))
        .await
        .expect("create root");
    tokio::fs::write(root.join("sub").join("target_file.rs"), "x")
        .await
        .expect("write fixture");
    tokio::fs::write(root.join("target_file.md"), "x")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let (result, is_error) = run_tool_checked(
        &registry,
        "find",
        serde_json::json!({"pattern": "target_file.*", "path": "."}),
        &context,
    )
    .await;

    assert!(!is_error, "unexpected error: {result}");
    assert!(
        result.contains("sub/target_file.rs"),
        "paths are relative and posix: {result}"
    );
    assert!(
        !result.contains(&root.to_string_lossy().to_string()),
        "no absolute paths in output: {result}"
    );

    // Empty result set is a successful result.
    let (result, is_error) = run_tool_checked(
        &registry,
        "find",
        serde_json::json!({"pattern": "no-such-file-*", "path": "."}),
        &context,
    )
    .await;
    assert!(!is_error, "empty find is success: {result}");
    assert_eq!(result, "No files found matching pattern");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn ls_sorts_case_insensitive_and_suffixes_directories() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(root.join("Zeta"))
        .await
        .expect("dir");
    tokio::fs::create_dir_all(root.join("empty-child"))
        .await
        .expect("dir");
    tokio::fs::write(root.join("alpha.txt"), "x")
        .await
        .expect("file");
    tokio::fs::write(root.join("Beta.txt"), "x")
        .await
        .expect("file");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(&registry, "ls", serde_json::json!({"path": "."}), &context).await;

    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(
        lines,
        vec!["alpha.txt", "Beta.txt", "empty-child/", "Zeta/"],
        "case-insensitive sort with / suffixes: {result:?}"
    );

    // The limit parameter caps entries with the upstream notice.
    let result = run_tool(
        &registry,
        "ls",
        serde_json::json!({"path": ".", "limit": 2}),
        &context,
    )
    .await;
    assert!(
        result.contains("[2 entries limit reached. Use limit=4 for more]"),
        "entry limit notice: {result}"
    );

    // An empty directory is a successful "(empty directory)" result.
    let result = run_tool(
        &registry,
        "ls",
        serde_json::json!({"path": "Zeta"}),
        &context,
    )
    .await;
    assert_eq!(result, "(empty directory)");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn read_offset_beyond_end_errors_and_limit_reports_remaining() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("lines.txt"), "one\ntwo\nthree\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    // Out-of-range offset: upstream counts the trailing empty split piece, so
    // "one\ntwo\nthree\n" has 4 lines total (read.ts:157-164).
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt", "offset": 5}),
        &context,
    )
    .await;
    assert!(
        result.contains("Offset 5 is beyond end of file (4 lines total)"),
        "unexpected error: {result}"
    );

    // A user limit that stops early reports the remaining lines.
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt", "offset": 2, "limit": 1}),
        &context,
    )
    .await;
    assert!(
        result.contains("[2 more lines in file. Use offset=3 to continue.]"),
        "remaining-lines hint: {result}"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn read_invalid_utf8_is_lossy_and_binary_still_rejected() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    // Invalid UTF-8 without a NUL byte: decoded lossily, not an error.
    tokio::fs::write(root.join("lossy.txt"), b"ok\xff\xfe-end")
        .await
        .expect("write fixture");
    // A NUL byte marks binary regardless.
    let mut binary = b"text".to_vec();
    binary.push(0);
    tokio::fs::write(root.join("bin.txt"), &binary)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lossy.txt"}),
        &context,
    )
    .await;
    assert!(
        result.contains("ok") && result.contains("-end"),
        "lossy decode: {result}"
    );

    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "bin.txt"}),
        &context,
    )
    .await;
    assert_eq!(result, "Cannot read 'bin.txt': binary file");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn read_output_is_capped() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    // 100 lines of ~1KB each: byte truncation cuts mid-file (the first line
    // fits, so the byte-limit continuation footer applies).
    let content: String = (0..100)
        .map(|i| format!("{i:04}{}\n", "x".repeat(1000)))
        .collect();
    tokio::fs::write(root.join("large.txt"), content)
        .await
        .expect("write fixture");
    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "large.txt", "limit": 100_000}),
        &context,
    )
    .await;

    assert!(result.len() <= 50 * 1024 + 200);
    // Byte-limit footer: upstream `Showing lines a-b of N (50.0KB limit). Use
    // offset=… to continue.` — the `...` form belongs to the reason-less
    // footer, so assert the continuation part itself.
    assert!(
        result.contains("Use offset=") && result.contains("50.0KB limit"),
        "expected byte-limit continuation footer at the tail: {}",
        &result[result.len().saturating_sub(200)..]
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

/// A raw OS access-denied error for this platform. `libc` is not a
/// dev-dependency of the test target, so the codes are spelled out: `EACCES`
/// is 13 on the Unix platforms this repository targets; Windows'
/// `ERROR_ACCESS_DENIED` is 5.
fn os_access_denied() -> std::io::Error {
    #[cfg(unix)]
    let code = 13;
    #[cfg(windows)]
    let code = 5;
    #[cfg(not(any(unix, windows)))]
    let code = 13;
    std::io::Error::from_raw_os_error(code)
}

/// The split the tools rely on: the handle walk's own refusals use
/// `io::Error::new` and therefore carry no OS code; a real denial does.
#[test]
fn policy_refusal_and_os_denial_are_distinguished() {
    use pir::coding_agent::tools::is_confinement_refusal_for_tests;

    let policy = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "path is outside the workspace root",
    );
    assert!(is_confinement_refusal_for_tests(&policy));

    let reparse = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "path component is a reparse point",
    );
    assert!(is_confinement_refusal_for_tests(&reparse));

    let os_denied = os_access_denied();
    assert_eq!(os_denied.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!is_confinement_refusal_for_tests(&os_denied));

    assert!(!is_confinement_refusal_for_tests(&std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "missing"
    )));
}

/// A real OS denial must not claim the path escapes the workspace: the path is
/// inside it and the model needs to fix permissions.
#[test]
fn os_denial_does_not_claim_the_path_escapes() {
    use pir::coding_agent::tools::confinement_error_for_tests;

    let text = confinement_error_for_tests("sub/file.txt", &os_access_denied());
    assert!(text.contains("permission denied"), "{text}");
    assert!(text.contains("the OS denied access"), "{text}");
    assert!(text.contains("sub/file.txt"), "{text}");
    assert!(!text.contains("escapes the workspace root"), "{text}");
    assert!(!text.contains("symbolic link"), "{text}");
}

/// Policy refusals keep the exact confinement wording the name-based check
/// uses.
#[test]
fn policy_refusal_keeps_the_confinement_wording() {
    use pir::coding_agent::tools::confinement_error_for_tests;

    let outside = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "path is outside the workspace root",
    );
    assert_eq!(
        confinement_error_for_tests("sub/file.txt", &outside),
        "Path 'sub/file.txt' escapes the workspace root"
    );

    let reparse = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "path component is a reparse point",
    );
    assert_eq!(
        confinement_error_for_tests("sub/file.txt", &reparse),
        "Path 'sub/file.txt' must not traverse a symbolic link"
    );
}

/// Poll `live_watchers` until it returns to `baseline`: aborting a tokio task
/// drops its future (and the liveness guard) asynchronously.
async fn wait_for_watcher_count(live_watchers: fn() -> usize, baseline: usize) {
    for _ in 0..10_000 {
        if live_watchers() == baseline {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "abort watcher did not return to baseline {baseline}: {} still live",
        live_watchers()
    );
}

/// Drop the tool's future while its blocking scan is still pending, exactly
/// like a caller cancelling a batch: the signal is never aborted, so only the
/// drop path can release the watcher.
async fn cancel_tool_and_expect_watcher_released(
    tool: Arc<dyn AgentTool>,
    args: serde_json::Value,
    context: &ToolContext,
    live_watchers: fn() -> usize,
    baseline: usize,
) {
    let mut future = tool.run(&args, context);
    tokio::select! {
        result = &mut future => {
            panic!("tool finished before it could be cancelled: {}", result.content);
        }
        _ = async {
            while live_watchers() <= baseline {
                tokio::task::yield_now().await;
            }
        } => {}
    }
    assert!(
        live_watchers() > baseline,
        "the abort watcher must be alive while the blocking scan is pending"
    );
    drop(future);
    wait_for_watcher_count(live_watchers, baseline).await;
}

/// A cancelled call must not leave its abort watcher parked on
/// `signal.cancelled()`: dropping the future used to discard the watcher's
/// `JoinHandle` un-aborted (leaking one idle task and a signal clone per
/// cancellation). A normal non-cancelled call must also stop its watcher.
#[tokio::test]
async fn cancelled_grep_future_releases_abort_watcher() {
    use pir::coding_agent::tools::GrepTool;

    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    // Enough non-matching data that the blocking search cannot finish inside
    // the poll that spawns it, so the future is dropped with the
    // `spawn_blocking` join still pending.
    let body = format!("{}\n", "x".repeat(1024)).repeat(16);
    for index in 0..256 {
        tokio::fs::write(root.join(format!("file{index}.txt")), &body)
            .await
            .expect("write fixture");
    }
    let baseline = GrepTool::live_abort_watchers_for_tests();
    let mut context = ToolContext::new(&root);
    context.signal = Some(pir::ai::types::AbortSignal::new());
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    cancel_tool_and_expect_watcher_released(
        tool(&registry, "grep"),
        serde_json::json!({"pattern": "zzz-no-such-token", "path": ".", "limit": 100_000}),
        &context,
        GrepTool::live_abort_watchers_for_tests,
        baseline,
    )
    .await;

    // The normal completion path stops the watcher without the signal firing.
    let result = tool(&registry, "grep")
        .run(
            &serde_json::json!({"pattern": "xxxx", "path": ".", "limit": 1}),
            &context,
        )
        .await;
    assert!(!result.is_error, "grep failed: {}", result.content);
    wait_for_watcher_count(GrepTool::live_abort_watchers_for_tests, baseline).await;

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

/// The find tool owns an identical watcher shape; its walk must be cancelled
/// the same way.
#[tokio::test]
async fn cancelled_find_future_releases_abort_watcher() {
    use pir::coding_agent::tools::FindTool;

    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    // A wide tree with no matching names keeps the blocking walk busy for the
    // poll that spawns it (and long enough for the drop to land mid-scan).
    for index in 0..1200 {
        tokio::fs::write(root.join(format!("entry{index}.txt")), "x")
            .await
            .expect("write fixture");
    }
    let baseline = FindTool::live_abort_watchers_for_tests();
    let mut context = ToolContext::new(&root);
    context.signal = Some(pir::ai::types::AbortSignal::new());
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    cancel_tool_and_expect_watcher_released(
        tool(&registry, "find"),
        serde_json::json!({"pattern": "*.no-such-suffix", "path": "."}),
        &context,
        FindTool::live_abort_watchers_for_tests,
        baseline,
    )
    .await;

    let result = tool(&registry, "find")
        .run(
            &serde_json::json!({"pattern": "*.txt", "path": ".", "limit": 1}),
            &context,
        )
        .await;
    assert!(!result.is_error, "find failed: {}", result.content);
    wait_for_watcher_count(FindTool::live_abort_watchers_for_tests, baseline).await;

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

/// A real OS denial must not reuse the confinement text: the path is *inside*
/// the workspace and the operating system refused access, so the model should
/// fix permissions instead of hunting for another path (CL18 P9).
///
/// Unix only: the fixture is a directory with execute-but-not-read mode, which
/// the name-based walk can `lstat` but the handle walk cannot open (`EACCES`).
/// A root-run test process bypasses the mode bits, so the test skips itself
/// when the host cannot produce the denial.
#[cfg(unix)]
#[tokio::test]
async fn os_permission_denied_write_is_not_reported_as_confinement() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_workspace();
    let blocked = root.join("blocked");
    tokio::fs::create_dir_all(&blocked)
        .await
        .expect("create root");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o111))
        .expect("chmod fixture");
    // Probe before asserting: if this process can still read the directory
    // (running as root), the tool cannot hit EACCES either — skip gracefully.
    if std::fs::read_dir(&blocked).is_ok() {
        let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&root);
        return;
    }

    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    // A policy refusal keeps the confinement wording.
    let (escape, is_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({"path": "../pir-tools-os-denied-escape.txt", "content": "blocked"}),
        &context,
    )
    .await;
    assert!(is_error, "the escape must be refused: {escape}");
    assert!(
        escape.contains("escapes the workspace root"),
        "policy refusal wording changed: {escape}"
    );

    // The OS denial reports the denial, not a confinement escape.
    let (result, is_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({"path": "blocked/new.txt", "content": "denied"}),
        &context,
    )
    .await;
    // Restore before asserting so cleanup works even on failure.
    let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755));
    assert!(is_error, "the denied write must fail: {result}");
    assert!(
        result.contains("permission denied") && result.contains("the OS denied access"),
        "expected an accurate OS-denial message: {result}"
    );
    assert!(
        !result.contains("escapes the workspace root") && !result.contains("symbolic link"),
        "an OS denial must not be reported as confinement: {result}"
    );
    assert!(
        !blocked.join("new.txt").exists(),
        "the denied write must not create the file"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

// ---------------------------------------------------------------------------
// PIR-BIN-077 — `write`/`edit` failure text is stable English + the OS code
// ---------------------------------------------------------------------------

/// The formatter contract every `write`/`edit` failure goes through: an
/// English reason category plus the numeric OS code, and never the
/// host-localized `io::Error` `Display` string (`拒绝访问。 (os error 5)` on a
/// zh-CN Windows), which made the message unreproducible across machines.
///
/// Driven through the formatter seam with synthetic `io::Error`s: a real
/// permission-denied state is not portably producible (it depends on the
/// account's rights, and a root/Administrator test process bypasses the
/// denial), while the contract under test here is the rendering itself. The
/// Windows end-to-end arm below covers the real OS failure.
#[test]
fn mutation_failure_text_is_english_and_locale_independent() {
    use pir::coding_agent::tools::mutation_error_for_tests;

    let denied = os_access_denied();
    let code = denied.raw_os_error().expect("raw OS error");
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);

    let write = mutation_error_for_tests("write", "x.txt", &denied);
    assert!(
        write.starts_with("Cannot write 'x.txt': permission denied ("),
        "the category must be stable English: {write}"
    );
    assert!(
        write.ends_with(&format!("os error {code})")),
        "the numeric OS code must survive: {write}"
    );
    assert!(write.is_ascii(), "localized OS text leaked: {write}");
    #[cfg(windows)]
    assert_eq!(
        write,
        "Cannot write 'x.txt': permission denied (Access is denied; os error 5)"
    );

    // An edit's failed write must name the edit tool, not `write` (the pre-fix
    // arm said `Cannot write` for an edit — PIR-BIN-077 2h).
    let edit = mutation_error_for_tests("edit", "x.txt", &denied);
    assert!(
        edit.starts_with("Cannot edit 'x.txt': permission denied ("),
        "{edit}"
    );
    assert!(!edit.contains("Cannot write"), "{edit}");
    assert!(edit.is_ascii(), "{edit}");

    // Every mapped kind is English and keeps its numeric code.
    let missing =
        mutation_error_for_tests("write", "gone.txt", &std::io::Error::from_raw_os_error(2));
    assert!(
        missing.starts_with("Cannot write 'gone.txt': no such file or directory ("),
        "{missing}"
    );
    assert!(missing.ends_with("os error 2)"), "{missing}");
    assert!(missing.is_ascii(), "{missing}");

    // A component over the platform limit: `ERROR_INVALID_NAME` (os error 123)
    // — the exact case from PIR-BIN-077 (2f). Windows-only: 123 is not a
    // portable errno.
    #[cfg(windows)]
    {
        let invalid_name = std::io::Error::from_raw_os_error(123);
        assert_eq!(invalid_name.kind(), std::io::ErrorKind::InvalidFilename);
        let long_name =
            mutation_error_for_tests("write", &format!("{}.txt", "n".repeat(300)), &invalid_name);
        assert!(long_name.contains(": invalid file name ("), "{long_name}");
        assert!(long_name.ends_with("os error 123)"), "{long_name}");
        assert!(long_name.is_ascii(), "{long_name}");
    }

    // An OS-layer error whose own `Display` is localized must not contribute
    // text: with no raw code there is no detail tail, and never the Display
    // string (`io::Error::new` keeps it for `Display` only).
    let localized = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "拒绝访问。");
    assert_eq!(localized.raw_os_error(), None);
    let rendered = mutation_error_for_tests("write", "x.txt", &localized);
    assert_eq!(rendered, "Cannot write 'x.txt': permission denied");
    assert!(rendered.is_ascii(), "{rendered}");
}

/// The real `write`/`edit` tools on Windows: a directory target and a
/// read-only file both fail with `ERROR_ACCESS_DENIED`, and the model gets the
/// stable English text with the OS code — never the host-localized string, and
/// an `edit` failure says `edit` (PIR-BIN-077 arms 2c/2a/2h).
///
/// Both states are deterministic on Windows (the file API refuses a directory
/// without `FILE_FLAG_BACKUP_SEMANTICS`; the read-only attribute denies the
/// atomic replace), so this arm needs no flaky permission setup.
#[cfg(windows)]
#[tokio::test]
async fn write_and_edit_failures_use_stable_english_text() {
    fn set_read_only(path: &std::path::Path, read_only: bool) {
        let mut permissions = std::fs::metadata(path).expect("stat").permissions();
        permissions.set_readonly(read_only);
        std::fs::set_permissions(path, permissions).expect("set read-only attribute");
    }

    let root = temp_workspace();
    tokio::fs::create_dir_all(root.join("adir"))
        .await
        .expect("create dir target");
    tokio::fs::write(root.join("ro.txt"), "RO\n")
        .await
        .expect("write fixture");
    set_read_only(&root.join("ro.txt"), true);

    let context = ToolContext::new(&root);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let (onto_dir, dir_is_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({"path": "adir", "content": "x\n"}),
        &context,
    )
    .await;
    let (over_read_only, write_is_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({"path": "ro.txt", "content": "NEW\n"}),
        &context,
    )
    .await;
    let (edit_read_only, edit_is_error) = run_tool_checked(
        &registry,
        "edit",
        serde_json::json!({"path": "ro.txt", "edits": [{"oldText": "RO", "newText": "EDITED"}]}),
        &context,
    )
    .await;
    // Restore before asserting so cleanup works even on failure.
    set_read_only(&root.join("ro.txt"), false);

    assert!(
        dir_is_error,
        "writing onto a directory must fail: {onto_dir}"
    );
    assert_eq!(
        onto_dir,
        "Cannot write 'adir': permission denied (Access is denied; os error 5)"
    );
    assert!(
        write_is_error,
        "writing over a read-only file must fail: {over_read_only}"
    );
    assert_eq!(
        over_read_only,
        "Cannot write 'ro.txt': permission denied (Access is denied; os error 5)"
    );
    assert!(
        edit_is_error,
        "editing a read-only file must fail: {edit_read_only}"
    );
    assert_eq!(
        edit_read_only,
        "Cannot edit 'ro.txt': permission denied (Access is denied; os error 5)"
    );
    for text in [&onto_dir, &over_read_only, &edit_read_only] {
        assert!(text.is_ascii(), "localized OS text leaked: {text}");
    }
    assert!(
        !edit_read_only.contains("Cannot write"),
        "an edit failure must not be reported as a write: {edit_read_only}"
    );
    assert_eq!(
        tokio::fs::read_to_string(root.join("ro.txt"))
            .await
            .expect("read fixture"),
        "RO\n",
        "no failed mutation may change the file"
    );
    assert!(
        root.join("adir").is_dir(),
        "the directory target is untouched"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

// ---------------------------------------------------------------------------
// PIR-BIN-079 — workspace-internal `..` is accepted, escapes stay refused
// ---------------------------------------------------------------------------

/// A `..` that never leaves the workspace is accepted and resolves to the
/// intended file, while a path that genuinely escapes keeps the refusal.
/// Pinned at the confinement seam (`workspace_path_parts`) that every local
/// file tool shares; the spellings are the case table from PIR-BIN-079.
#[test]
fn in_workspace_parent_components_are_accepted_and_escapes_still_refused() {
    use pir::coding_agent::tools::workspace_path_parts;
    use std::path::Component;

    let base = temp_workspace();
    let ws = base.join("ws");
    std::fs::create_dir_all(ws.join("dir").join("sub")).expect("create fixture");
    std::fs::create_dir_all(ws.join("src")).expect("create fixture");
    std::fs::write(ws.join("src").join("main.rs"), "fn main(){}\n").expect("write fixture");
    std::fs::write(ws.join("dir").join("a.txt"), "A-DIR\n").expect("write fixture");
    std::fs::write(base.join("outside.txt"), "OUT\n").expect("write fixture");
    let canonical =
        |path: &std::path::Path| std::fs::canonicalize(path).expect("canonicalize fixture");

    let main_rs = canonical(&ws.join("src").join("main.rs"));
    let dir_a = canonical(&ws.join("dir").join("a.txt"));
    // Absolute spellings of the same targets: an absolute request is never
    // joined to the root, so the confinement walk itself has to resolve the
    // `..` (the PIR-BIN-079 residual).
    let abs_main = ws.join("src").join("..").join("src").join("main.rs");
    let abs_dir = ws.join("dir").join("sub").join("..").join("a.txt");
    let abs_dot = ws
        .join(".")
        .join("src")
        .join("..")
        .join("src")
        .join("main.rs");
    let abs_sub2 = ws.join("sub2").join("..").join("dir").join("a.txt");
    let cases: Vec<(String, PathBuf)> = vec![
        ("src/../src/main.rs".to_owned(), main_rs.clone()),
        // The native Windows spelling a model retypes from a listing.
        (r"src\..\src\main.rs".to_owned(), main_rs.clone()),
        ("./src/../src/main.rs".to_owned(), main_rs.clone()),
        ("dir/sub/../a.txt".to_owned(), dir_a.clone()),
        ("dir/sub/../sub/../a.txt".to_owned(), dir_a.clone()),
        // A not-yet-existing intermediate directory in front of the `..`
        // (pi 1.4.0 resolves the same spelling to the same file).
        ("sub2/../dir/a.txt".to_owned(), dir_a.clone()),
        (abs_main.to_string_lossy().into_owned(), main_rs.clone()),
        (abs_dir.to_string_lossy().into_owned(), dir_a.clone()),
        (abs_dot.to_string_lossy().into_owned(), main_rs.clone()),
        (abs_sub2.to_string_lossy().into_owned(), dir_a.clone()),
    ];
    for (requested, expected) in cases {
        let parts = workspace_path_parts(&ws, &requested)
            .unwrap_or_else(|error| panic!("'{requested}' must be accepted: {error}"));
        assert_eq!(canonical(&parts.candidate), expected, "'{requested}'");
        assert!(
            !parts
                .relative
                .components()
                .any(|component| matches!(component, Component::ParentDir)),
            "'{requested}' must resolve before the symlink walk sees it: {}",
            parts.relative.display()
        );
    }

    // `..` onto a directory is accepted at the seam; the tools then report the
    // ordinary directory result, not an escape. The absolute spelling must
    // behave the same.
    let onto_dir = workspace_path_parts(&ws, "dir/sub/..")
        .unwrap_or_else(|error| panic!("'dir/sub/..' must be accepted: {error}"));
    assert_eq!(canonical(&onto_dir.candidate), canonical(&ws.join("dir")));
    let abs_onto_dir = ws.join("dir").join("sub").join("..");
    let onto_dir_abs = workspace_path_parts(&ws, &abs_onto_dir.to_string_lossy())
        .unwrap_or_else(|error| panic!("'{}' must be accepted: {error}", abs_onto_dir.display()));
    assert_eq!(
        canonical(&onto_dir_abs.candidate),
        canonical(&ws.join("dir"))
    );

    // The escape half stays refused, and the fixture proves the target really
    // lies outside the root: the refusal is about the boundary, not a missing
    // file.
    assert!(base.join("outside.txt").is_file());
    let abs_escape = base.join("outside.txt");
    let abs_escape_dotdot = ws.join("..").join("outside.txt");
    let abs_escape_climb = ws.join("dir").join("..").join("..").join("outside.txt");
    let escapes: Vec<String> = vec![
        "..".to_owned(),
        "../outside.txt".to_owned(),
        "dir/../../outside.txt".to_owned(),
        "src/../../outside.txt".to_owned(),
        abs_escape.to_string_lossy().into_owned(),
        abs_escape_dotdot.to_string_lossy().into_owned(),
        abs_escape_climb.to_string_lossy().into_owned(),
    ];
    for requested in escapes {
        match workspace_path_parts(&ws, &requested) {
            Ok(parts) => panic!(
                "'{requested}' must be refused, got {}",
                parts.candidate.display()
            ),
            Err(error) => assert_eq!(
                error,
                format!("Path '{requested}' escapes the workspace root")
            ),
        }
    }

    std::fs::remove_dir_all(&base).expect("remove root");
}

/// The same two directions through the real tools (the model-facing surface):
/// the in-workspace spelling reads the intended file and lands a `write` at
/// the resolved path; the escaping spelling is refused with the confinement
/// wording and touches nothing outside the root.
#[tokio::test]
async fn file_tools_accept_dotdot_inside_the_workspace() {
    let base = temp_workspace();
    let ws = base.join("ws");
    tokio::fs::create_dir_all(ws.join("dir").join("sub"))
        .await
        .expect("create fixture");
    tokio::fs::write(ws.join("dir").join("a.txt"), "A-DIR\n")
        .await
        .expect("write fixture");
    tokio::fs::write(base.join("outside.txt"), "OUTSIDE\n")
        .await
        .expect("write fixture");
    let context = ToolContext::new(&ws);
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);

    let (content, is_error) = run_tool_checked(
        &registry,
        "read",
        serde_json::json!({"path": "dir/sub/../a.txt"}),
        &context,
    )
    .await;
    assert!(!is_error, "in-workspace `..` read failed: {content}");
    assert_eq!(content, "A-DIR\n");

    // The absolute spelling of the same file: it bypasses the root join, so
    // the confinement walk itself must resolve the `..`.
    let abs_read = ws.join("dir").join("sub").join("..").join("a.txt");
    let (abs_content, abs_is_error) = run_tool_checked(
        &registry,
        "read",
        serde_json::json!({"path": abs_read.to_string_lossy().into_owned()}),
        &context,
    )
    .await;
    assert!(
        !abs_is_error,
        "absolute in-workspace `..` read failed: {abs_content}"
    );
    assert_eq!(abs_content, "A-DIR\n");

    let (written, is_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({"path": "dir/sub/../written.txt", "content": "DD-WRITE\n"}),
        &context,
    )
    .await;
    assert!(!is_error, "in-workspace `..` write failed: {written}");
    assert_eq!(
        tokio::fs::read_to_string(ws.join("dir").join("written.txt"))
            .await
            .expect("read back"),
        "DD-WRITE\n",
        "the bytes must land at the resolved inside path"
    );

    let abs_written_target = ws
        .join("dir")
        .join("sub")
        .join("..")
        .join("abs-written.txt");
    let (abs_written, abs_write_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({
            "path": abs_written_target.to_string_lossy().into_owned(),
            "content": "ABS-WRITE\n"
        }),
        &context,
    )
    .await;
    assert!(
        !abs_write_error,
        "absolute in-workspace `..` write failed: {abs_written}"
    );
    assert_eq!(
        tokio::fs::read_to_string(ws.join("dir").join("abs-written.txt"))
            .await
            .expect("read back"),
        "ABS-WRITE\n",
        "the absolute spelling must land at the resolved inside path"
    );

    let (escape, is_error) = run_tool_checked(
        &registry,
        "read",
        serde_json::json!({"path": "../outside.txt"}),
        &context,
    )
    .await;
    assert!(is_error, "the escape must be refused: {escape}");
    assert!(escape.contains("escapes the workspace root"), "{escape}");
    assert!(
        !escape.contains("OUTSIDE"),
        "no outside bytes may leak: {escape}"
    );

    // The absolute spelling of the escape is refused the same way.
    let abs_escape = base.join("outside.txt");
    let (abs_escape_text, abs_escape_error) = run_tool_checked(
        &registry,
        "read",
        serde_json::json!({"path": abs_escape.to_string_lossy().into_owned()}),
        &context,
    )
    .await;
    assert!(
        abs_escape_error,
        "the absolute escape must be refused: {abs_escape_text}"
    );
    assert!(
        abs_escape_text.contains("escapes the workspace root"),
        "{abs_escape_text}"
    );
    assert!(
        !abs_escape_text.contains("OUTSIDE"),
        "no outside bytes may leak: {abs_escape_text}"
    );

    // ... and the absolute spelling that climbs out through `..`.
    let abs_climb = ws.join("..").join("outside.txt");
    let (abs_climb_text, abs_climb_error) = run_tool_checked(
        &registry,
        "read",
        serde_json::json!({"path": abs_climb.to_string_lossy().into_owned()}),
        &context,
    )
    .await;
    assert!(
        abs_climb_error,
        "the absolute `..` escape must be refused: {abs_climb_text}"
    );
    assert!(
        abs_climb_text.contains("escapes the workspace root"),
        "{abs_climb_text}"
    );

    let (escape_write, is_error) = run_tool_checked(
        &registry,
        "write",
        serde_json::json!({"path": "dir/../../escaped.txt", "content": "nope\n"}),
        &context,
    )
    .await;
    assert!(is_error, "the escape must be refused: {escape_write}");
    assert!(
        escape_write.contains("escapes the workspace root"),
        "{escape_write}"
    );
    assert!(
        !base.join("escaped.txt").exists(),
        "a refused write must not create anything outside the root"
    );
    tokio::fs::remove_dir_all(&base).await.expect("remove root");
}
