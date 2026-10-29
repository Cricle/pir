//! Regression tests for the shell tools' full-output totals and the
//! PowerShell spill parity (upstream `core/tools/output-accumulator.ts`
//! `snapshot` + the shared bash/powershell shell-tool body):
//!
//! 1. `details.truncation.totalLines` / `totalBytes` (and the streamed
//!    snapshot payloads) must describe the WHOLE stream even after the
//!    rolling window evicted its prefix — previously they described only
//!    the 2×50KB window.
//! 2. The PowerShell tool must use the bash tool's bounded capture
//!    (spill file, `Full output: <path>` footer, `details.fullOutputPath`,
//!    `structuredContent.truncated`/`full_output_path`) instead of the old
//!    unbounded in-memory capture that always reported `truncated: false`.
//! 3. The advertised spill file must be the COMPLETE output (upstream temp
//!    file = the full byte stream) — previously it held only the evicted
//!    prefix, silently missing the retained window, and the structured
//!    output kept its head instead of the upstream head+marker+tail view.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::{ToolContext, ToolRegistry, ToolUpdateFn};
use pir::coding_agent::register_core_tools;
use pir::coding_agent::truncate::DEFAULT_MAX_BYTES;

/// The shell tools keep `2 * DEFAULT_MAX_BYTES` of rolling tail in memory;
/// everything evicted before it lands in the spill file.
const WINDOW_BYTES: usize = 2 * DEFAULT_MAX_BYTES;

fn temp_workspace() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pir-shell-totals-{nonce}-{sequence}-{}",
        std::process::id()
    ))
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
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
        on_update: Some(emit),
        ..ToolContext::new(root.to_path_buf())
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

/// Skip guard for hosts without a resolvable bash.
fn bash_available() -> bool {
    pir::coding_agent::tools::resolve_shell_config(None).is_ok()
}

#[tokio::test]
async fn bash_totals_stay_full_beyond_the_rolling_window() {
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let updates: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let context = context_with_updates(&root, &updates);

    // ~165KB (> the 2 * 50KB window): the window alone can only see its own
    // ~100KB tail, so both the final details and the streamed snapshots must
    // carry the full-stream totals.
    let expected: String = (1..=30_000).map(|line| format!("{line}\n")).collect();
    assert!(
        expected.len() > WINDOW_BYTES,
        "fixture must exceed the window"
    );
    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "seq 1 30000"}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("truncation details");
    let truncation = &details["truncation"];
    assert_eq!(truncation["truncated"], true);
    assert_eq!(truncation["truncatedBy"], "lines");
    assert_eq!(truncation["totalLines"], 30_000);
    assert_eq!(
        truncation["totalBytes"].as_u64(),
        Some(expected.len() as u64)
    );
    assert_eq!(truncation["outputLines"], 2_000);
    // The footer points at the spill file, which must be the COMPLETE stream
    // (evicted prefix + retained window). Before the fix the file held only
    // the evicted prefix — missing the final <= 100KB of output — while the
    // footer still called it "Full output".
    assert!(
        result.content.contains("of 30000"),
        "footer must report the full line total: {}",
        result.content
    );
    let path = details["fullOutputPath"]
        .as_str()
        .expect("full output path")
        .to_owned();
    let spilled = std::fs::read(&path).expect("read the spill file");
    assert_eq!(spilled, expected.as_bytes());

    // The final streamed snapshot must carry the same full totals.
    let updates = updates.lock().expect("updates lock").clone();
    let last = updates.last().expect("at least one streaming payload");
    let streamed = &last["details"]["truncation"];
    assert_eq!(streamed["totalLines"], 30_000);
    assert_eq!(streamed["totalBytes"].as_u64(), Some(expected.len() as u64));

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn powershell_truncation_spills_with_path_and_full_totals() {
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = ToolContext::new(root.to_path_buf());

    // ~1.3MB of output: past the 2 * 50KB rolling window (the old unbounded
    // capture spilled nothing and kept everything in memory) and past the
    // 1MB structured-output cap.
    let result = run_tool(
        &registry,
        "powershell",
        serde_json::json!({"command": "1..200000"}),
        &context,
    )
    .await;
    if result.is_error
        && (result.content.contains("only available on Windows")
            || result.content.contains("No PowerShell executable found"))
    {
        return; // non-Windows host or no PowerShell on PATH
    }

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let details = result.details.expect("truncation details");
    let truncation = &details["truncation"];
    assert_eq!(truncation["truncated"], true);
    assert_eq!(truncation["totalLines"], 200_000);
    let total_bytes = truncation["totalBytes"].as_u64().expect("totalBytes");
    assert!(
        total_bytes > WINDOW_BYTES as u64,
        "totals must describe the whole stream, got {total_bytes}"
    );
    // The footer now carries the full-output path like the bash tool.
    assert!(result.content.contains("[Showing lines"));
    assert!(result.content.contains("Full output:"));
    let path = details["fullOutputPath"]
        .as_str()
        .expect("full output path")
        .to_owned();
    let spilled = std::fs::read(&path).expect("read the spill file");
    // Complete stream: the file size equals the reported total and it ends
    // with the actual final output line (before the fix the last
    // <= 100KB were missing from the file).
    assert_eq!(spilled.len() as u64, total_bytes);
    assert!(
        String::from_utf8_lossy(&spilled)
            .trim_end()
            .ends_with("200000"),
        "spill file must end with the last output line"
    );

    // structuredContent picks up the 1MB `readFullOutput` view: the full
    // stream (not the 100KB window), capped with truncated/full_output_path.
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["truncated"], true);
    assert_eq!(structured["full_output_path"].as_str(), Some(path.as_str()));
    let structured_output = structured["output"].as_str().expect("output string");
    assert!(
        structured_output.len() >= 1024 * 1024 - 3,
        "structured output must be the 1MB full view, got {}",
        structured_output.len()
    );

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}

#[tokio::test]
async fn bash_structured_output_keeps_head_and_tail_around_marker() {
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let registry = registry();
    let context = ToolContext::new(root.to_path_buf());

    // ~1.23MB of output: past the 1MB structured-output cap, so
    // `structuredContent.output` must be the upstream `readFullOutput` view
    // (head + `[... N bytes omitted ...]` marker + tail). ASCII digits keep
    // the UTF-8 boundary arithmetic exact.
    const CAP: usize = 1024 * 1024;
    let expected: String = (1..=200_000).map(|line| format!("{line}\n")).collect();
    assert!(expected.len() > CAP, "fixture must exceed the cap");
    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "seq 1 200000"}),
        &context,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["truncated"], true);
    let shown = structured["output"].as_str().expect("output string");
    let head = &expected[..CAP / 2];
    let tail = &expected[expected.len() - CAP / 2..];
    let omitted = expected.len() - CAP;
    assert_eq!(
        shown,
        format!("{head}\n\n[... {omitted} bytes omitted ...]\n\n{tail}")
    );

    tokio::fs::remove_dir_all(&root).await.expect("remove root");
}
