//! G6 shell-tool parity tests: the "(no output)" placeholder, truncation
//! details, streaming partial-result updates, structuredContent, fractional
//! timeouts, and the timeout tree kill (upstream bash.ts/powershell.ts).
//!
//! Hermeticity: every test that runs the `bash`/`powershell` tools pins the
//! process-global `PIR_AGENT_DIR` at a fresh, empty directory for its duration
//! (`hermetic_agent_dir`), so `current_shell_prefs()` -> `agent_dir()` reads the
//! built-in defaults instead of an ambient `~/.pi/agent/settings.json`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pir::agent::tool_execution::{ToolContext, ToolRegistry, ToolUpdateFn};
use pir::coding_agent::register_core_tools;

fn temp_workspace() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pir-shell-{nonce}-{sequence}-{}",
        std::process::id()
    ))
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

fn context(root: &std::path::Path) -> ToolContext {
    ToolContext::new(root)
}

fn context_with_updates(
    root: &std::path::Path,
    updates: &Arc<Mutex<Vec<serde_json::Value>>>,
) -> ToolContext {
    let sink = Arc::clone(updates);
    let emit: ToolUpdateFn = Arc::new(move |payload: serde_json::Value| {
        if let Ok(mut guard) = sink.lock() {
            guard.push(payload);
        }
    });
    ToolContext {
        cwd: root.to_path_buf(),
        signal: None,
        nested: None,
        session_env: None,
        held_mutation_keys: None,
        on_update: Some(emit),
        background: false,
    }
}

async fn run_tool(
    registry: &ToolRegistry,
    name: &str,
    args: serde_json::Value,
    context: &ToolContext,
) -> pir::agent::tool_batch::ToolResult {
    registry
        .get(name)
        .expect("tool should be registered")
        .run(&args, context)
        .await
}

/// Skip guard for hosts without a resolvable bash (the suite targets the
/// full-fidelity environment; the placeholder logic itself is platform
/// independent).
fn bash_available() -> bool {
    pir::coding_agent::tools::resolve_shell_config(None).is_ok()
}

/// Hermetic agent-dir pin for tests that run the real shell tools.
///
/// `bash`/`powershell` resolve their shell through `current_shell_prefs()` ->
/// `agent_dir()`, so an ambient `~/.pi/agent/settings.json` (`shellPath`,
/// `shellCommandPrefix`, `shellPool: false`) would silently change what these
/// tests execute. The guard points the process-global `PIR_AGENT_DIR` at a
/// fresh, empty directory (no settings.json, no trust store) for the test's
/// duration, so the preferences fall back to the built-in defaults, and
/// restores the previous value (and removes the directory) on drop. It uses
/// the binary-wide [`crate::common::AgentDirEnvGuard`] lock, so no other suite
/// in this binary observes the temporary value.
async fn hermetic_agent_dir(label: &str) -> crate::common::AgentDirEnvGuard {
    crate::common::AgentDirEnvGuard::scratch(label, None).await
}

#[tokio::test]
async fn bash_empty_output_gets_no_output_placeholder() {
    let _agent_dir = hermetic_agent_dir("empty-output").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "true"}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert_eq!(result.content, "(no output)");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_exposes_common_coreutils() {
    let _agent_dir = hermetic_agent_dir("coreutils").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({
            "command": "printf 'a\\nb\\nc\\n' | head -1; printf 'x\\ny\\nz\\n' | tail -1; date +%Y"
        }),
        &context,
    )
    .await;

    assert!(!result.is_error, "coreutils failed: {}", result.content);
    let lines: Vec<&str> = result.content.lines().map(str::trim).collect();
    assert_eq!(lines.first().copied(), Some("a"), "head: {lines:?}");
    assert_eq!(lines.get(1).copied(), Some("z"), "tail: {lines:?}");
    let year = lines.get(2).copied().unwrap_or_default();
    assert!(
        year.len() == 4 && year.chars().all(|character| character.is_ascii_digit()),
        "date: {lines:?}"
    );
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_exit_code_error_keeps_placeholder_and_structured_content() {
    let _agent_dir = hermetic_agent_dir("exit-code").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "exit 3"}),
        &context,
    )
    .await;

    // Upstream: appendStatus(outputText, "Command exited with code 3") with
    // the formatOutput "(no output)" placeholder.
    assert_eq!(result.content, "(no output)\n\nCommand exited with code 3");
    assert!(result.is_error);
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["exit_code"], 3);
    assert!(structured["wall_time_seconds"].is_number());
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_success_carries_structured_content() {
    let _agent_dir = hermetic_agent_dir("success").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "echo hello-g6"}),
        &context,
    )
    .await;

    assert!(!result.is_error);
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["truncated"], false);
    assert!(structured["output"]
        .as_str()
        .expect("output string")
        .contains("hello-g6"));
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_truncation_sets_details_and_footer() {
    let _agent_dir = hermetic_agent_dir("truncation").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    // 12000 lines (~70KB) trips both the 2000-line and 50KB limits.
    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "seq 1 12000"}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("truncation details");
    let truncation = &details["truncation"];
    assert_eq!(truncation["truncated"], true);
    assert_eq!(truncation["truncatedBy"], "lines");
    assert_eq!(truncation["totalLines"], 12000);
    assert_eq!(truncation["outputLines"], 2000);
    assert_eq!(truncation["maxLines"], 2000);
    assert!(result.content.contains("[Showing lines"));
    // The full-output spill file is reported in the details too.
    assert!(details["fullOutputPath"].is_string());
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_streams_throttled_updates() {
    let _agent_dir = hermetic_agent_dir("streaming").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let context = context_with_updates(&root, &updates);

    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "echo start; sleep 1; echo middle; sleep 1; echo done"}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let updates = updates.lock().expect("updates lock").clone();
    // The initial attach snapshot plus at least one throttled rolling view.
    assert!(
        updates.len() >= 2,
        "expected streaming updates, got {}",
        updates.len()
    );
    assert_eq!(updates[0]["content"], serde_json::json!([]));
    let joined: String = updates
        .iter()
        .map(|payload| {
            payload["content"]
                .as_array()
                .and_then(|blocks| blocks.first())
                .and_then(|block| block["text"].as_str())
                .unwrap_or("")
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("start"), "streamed: {joined}");
    assert!(joined.contains("done"), "streamed: {joined}");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_timeout_kills_the_process_tree() {
    let _agent_dir = hermetic_agent_dir("timeout").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let started = Instant::now();
    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "sleep 30", "timeout": 1}),
        &context,
    )
    .await;
    let elapsed = started.elapsed();

    // Upstream: the timeout is an error result, and the process tree (shell +
    // descendants via Job Object / process group) is terminated so the wait
    // returns promptly instead of hanging on the 30s sleep.
    assert!(result.is_error);
    assert!(
        result
            .content
            .ends_with("Command timed out after 1 seconds"),
        "unexpected content: {}",
        result.content
    );
    assert!(elapsed < std::time::Duration::from_secs(20), "{elapsed:?}");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn powershell_placeholder_and_structured_content() {
    let _agent_dir = hermetic_agent_dir("powershell").await;
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let empty = run_tool(
        &registry,
        "powershell",
        serde_json::json!({"command": "exit 0"}),
        &context,
    )
    .await;
    if empty.is_error && empty.content.contains("only available on Windows") {
        return; // non-Windows host
    }
    if empty.is_error && empty.content.contains("No PowerShell executable found") {
        return; // host without pwsh/powershell on PATH
    }
    assert_eq!(empty.content, "(no output)");

    let failed = run_tool(
        &registry,
        "powershell",
        serde_json::json!({"command": "exit 7"}),
        &context,
    )
    .await;
    assert_eq!(failed.content, "(no output)\n\nCommand exited with code 7");
    assert!(failed.is_error);
    let structured = failed.structured_content.expect("structured content");
    assert_eq!(structured["exit_code"], 7);
    assert!(structured["wall_time_seconds"].is_number());
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn powershell_accepts_fractional_timeout_and_kills() {
    let _agent_dir = hermetic_agent_dir("powershell-timeout").await;
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    let started = Instant::now();
    let result = run_tool(
        &registry,
        "powershell",
        serde_json::json!({"command": "Start-Sleep -Seconds 30", "timeout": 0.5}),
        &context,
    )
    .await;
    let elapsed = started.elapsed();

    if result.is_error && result.content.contains("only available on Windows") {
        return;
    }
    if result.is_error && result.content.contains("No PowerShell executable found") {
        return;
    }
    assert!(result.is_error);
    assert!(
        result
            .content
            .ends_with("Command timed out after 0.5 seconds"),
        "unexpected content: {}",
        result.content
    );
    assert!(elapsed < std::time::Duration::from_secs(20), "{elapsed:?}");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn read_truncation_sets_details_and_byte_limit_footer() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    // Line-limit case: 3000 short lines.
    let mut content = String::new();
    for i in 0..3000 {
        content.push_str(&format!("line-{i}\n"));
    }
    tokio::fs::write(root.join("lines.txt"), &content)
        .await
        .expect("write fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "lines.txt"}),
        &context,
    )
    .await;
    let details = result.details.expect("read truncation details");
    assert_eq!(details["truncation"]["truncated"], true);
    assert_eq!(details["truncation"]["truncatedBy"], "lines");

    // Byte-limit case: the footer reports the 50KB limit (upstream
    // read.ts:192).
    let wide = "a".repeat(100);
    let mut content = String::new();
    for _ in 0..600 {
        content.push_str(&wide);
        content.push('\n');
    }
    tokio::fs::write(root.join("wide.txt"), &content)
        .await
        .expect("write fixture");
    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "wide.txt"}),
        &context,
    )
    .await;
    assert!(
        result.content.contains("50.0KB limit"),
        "missing byte-limit footer: {}",
        result.content
    );
    let details = result.details.expect("read truncation details");
    assert_eq!(details["truncation"]["truncatedBy"], "bytes");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn edit_details_carry_diff_patch_and_first_changed_line() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("file.txt"), "one\ntwo\nthree\n")
        .await
        .expect("write fixture");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "edit",
        serde_json::json!({"path": "file.txt", "edits": [{"oldText": "two", "newText": "TWO"}]}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("edit details");
    assert_eq!(details["firstChangedLine"], 2);
    let diff = details["diff"].as_str().expect("diff string");
    assert!(diff.contains("+2 TWO"), "diff: {diff}");
    assert!(diff.contains("-2 two"), "diff: {diff}");
    assert!(!details["patch"].as_str().expect("patch string").is_empty());
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn edit_pure_deletion_reports_new_file_line() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    tokio::fs::write(root.join("del.txt"), "one\ntwo\nthree\n")
        .await
        .expect("write fixture");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "edit",
        serde_json::json!({"path": "del.txt", "edits": [{"oldText": "two\n", "newText": ""}]}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("edit details");
    // A pure deletion anchors at the line it removed (newLineNum at that
    // point, upstream generateDiffString).
    assert_eq!(details["firstChangedLine"], 2);
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn grep_truncation_sets_details() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    // 1000 matches of ~60 bytes each: past the 50KB byte cap (a limit above
    // the match count is required — the default 100-match cap ends the
    // search before the byte truncation).
    let mut content = String::new();
    for _ in 0..1000 {
        content.push_str(&format!("needle {}\n", "x".repeat(60)));
    }
    tokio::fs::write(root.join("hay.txt"), &content)
        .await
        .expect("write fixture");
    let registry = registry();
    let context = context(&root);

    let result = run_tool(
        &registry,
        "grep",
        serde_json::json!({"pattern": "needle", "path": ".", "limit": 2000}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("grep details");
    assert_eq!(details["truncation"]["truncated"], true);
    assert_eq!(details["truncation"]["truncatedBy"], "bytes");
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

/// The local spill behind a truncated bash run: the advertised
/// `fullOutputPath` is this machine's own `pir-shell-*` temp file and holds
/// the COMPLETE stream (not just the retained tail or the dropped prefix).
/// This is the property the remote arm relies on when it keeps the file
/// local — `spill: None` on the exec request, exactly as upstream
/// coding-agent wraps remote `BashOperations` in a client-side
/// `OutputAccumulator` — so a future daemon-side `spill` request would break
/// this test, not just the docs. (Relocated from an inline module in
/// `src/coding_agent/tools/bash.rs`, which the source-hygiene gate forbids:
/// all tests live under `tests/`.)
#[tokio::test]
async fn bash_truncated_run_advertises_a_complete_local_spill_file() {
    let _agent_dir = hermetic_agent_dir("spill").await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = context(&root);

    // One long line past the rolling-buffer keep (`2 * DEFAULT_MAX_BYTES`
    // with a 50 KiB byte limit → the model-facing result keeps only the tail),
    // while the tool's structured copy (1 MiB cap) still fits the complete
    // `3 * limit` plus one chunk.
    let total = 3 * 50 * 1024 + 4096;
    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({
            "command": format!("head -c {total} /dev/zero | tr '\\0' x"),
        }),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("a truncated run carries details");
    assert_eq!(
        details["truncation"]["truncated"],
        serde_json::json!(true),
        "{details}"
    );
    let path = details["fullOutputPath"]
        .as_str()
        .expect("a truncated run advertises its full-output file")
        .to_string();
    let name = std::path::Path::new(&path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with("pir-shell-"),
        "the client's own spill name (a daemon spill is `pi-output-*.log`): {name}"
    );
    let file = std::fs::read(&path).expect("the advertised file exists on this machine");
    assert_eq!(
        file.len(),
        total,
        "the file is the complete stream, not the dropped prefix"
    );
    assert!(file.iter().all(|byte| *byte == b'x'));
    assert!(
        result.content.contains(&path),
        "the truncation footer names the file: {:?}",
        result.content
    );
    let structured = result
        .structured_content
        .expect("a completed run carries structured content");
    assert_eq!(
        structured["output"].as_str().map(str::len),
        Some(total),
        "structured content repeats the complete output"
    );
    let _ = std::fs::remove_file(&path);
    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}
