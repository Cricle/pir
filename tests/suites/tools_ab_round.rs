//! Regression tests for the builtin-tool A/B round (lane-ab-tools,
//! 2026-11-11) — four model-facing divergences found by running pir and
//! upstream pi 1.4.0 side by side on identical fixtures with identical tool
//! arguments (evidence: `.play/evidence/ab-round1/lane-ab-tools/`).
//!
//! 1. **Spill recovery** (PIR-BIN-029): a truncated shell result advertises
//!    `Full output: <temp path>`; `read` on that exact path must deliver the
//!    file. Before the fix the workspace confinement refused it (`Path '…'
//!    escapes the workspace root`) while upstream pi read it — the footer was
//!    a dead end.
//! 2. **Footer spacing** (PIR-BIN-030): upstream renders
//!    `[Showing lines a-b of N. Full output: <path>]` (one space); pir emitted
//!    TWO (`.  Full output:`), so every truncated shell result differed by one
//!    byte.
//! 3. **Status-line whitespace** (PIR-BIN-031): upstream `appendStatus` does
//!    not trim the captured output, so `out\nerr\n` + exit 7 renders
//!    `out\nerr\n\n\nCommand exited with code 7`; pir trimmed and produced
//!    only one blank line.
//! 4. **Verbatim path display** (PIR-BIN-032): model-facing `Path not found:`
//!    / `Not a directory:` / read-gate messages printed the Windows `\\?\`
//!    verbatim prefix; upstream pi prints the native `C:\…` spelling.
//!
//! Every assertion is a general behavior of the tools (no fixture-specific
//! special casing): it is derived from the upstream implementation that the
//! A/B run pinned.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::agent::tool_execution::ToolContext;
use pir::agent::tool_execution::ToolRegistry;
use pir::coding_agent::tools::{register_core_tools, spill};

fn temp_workspace() -> PathBuf {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "pir-tools-ab-{nonce}-{sequence}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create workspace");
    root
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
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

fn bash_available() -> bool {
    std::process::Command::new("bash")
        .arg("-c")
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// PIR-BIN-030: the truncated-tail footer separates the retention sentence
/// from the full-output pointer with exactly ONE space (upstream pi's
/// `formatOutput`), and the byte-limit footer keeps the same rule.
#[test]
fn truncated_shell_footer_uses_one_space_before_full_output() {
    use pir::coding_agent::truncate::{format_shell_output, truncate_tail};

    let content = (1..=5000)
        .map(|line| format!("line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let truncation = truncate_tail(&content, 2000, 50 * 1024);
    let rendered = format_shell_output(&truncation, Some("/tmp/spill"), "(no output)");
    assert!(
        rendered.contains("[Showing lines 3001-5000 of 5000. Full output: /tmp/spill]"),
        "one space between the sentence and the pointer: {}",
        &rendered[rendered.len().saturating_sub(120)..]
    );
    assert!(
        !rendered.contains(".  Full output:"),
        "the double space upstream never emits must not come back: {}",
        &rendered[rendered.len().saturating_sub(120)..]
    );

    // No spill path (degraded capture): the footer ends at the period.
    let no_path = format_shell_output(&truncation, None, "(no output)");
    assert!(
        no_path.contains("[Showing lines 3001-5000 of 5000.]"),
        "footer without a path must not carry a stray space: {}",
        &no_path[no_path.len().saturating_sub(120)..]
    );

    // Byte-limited variant (short lines, huge count would need >50KB).
    let wide = "a".repeat(120);
    let byte_content = (1..=2000)
        .map(|_| wide.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let byte_truncation = truncate_tail(&byte_content, 2000, 50 * 1024);
    assert_eq!(
        byte_truncation.truncated_by,
        Some(pir::coding_agent::truncate::TruncatedBy::Bytes)
    );
    let byte_rendered = format_shell_output(&byte_truncation, Some("/tmp/spill2"), "(no output)");
    assert!(
        byte_rendered.contains("(50.0KB limit). Full output: /tmp/spill2]"),
        "byte-limit footer uses the same single space: {}",
        &byte_rendered[byte_rendered.len().saturating_sub(120)..]
    );
}

/// PIR-BIN-031: a non-zero exit appends the status line to the UNTRIMMED
/// capture, byte-identical to upstream `appendStatus(outputText, status)`.
#[tokio::test]
async fn bash_exit_status_keeps_the_capture_whitespace() {
    let _agent_dir = crate::common::AgentDirEnvGuard::scratch("ab-status", None).await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    let registry = registry();
    let context = ToolContext::new(root.clone());

    let result = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "echo out; echo err >&2; exit 7"}),
        &context,
    )
    .await;

    assert!(result.is_error, "exit 7 must be an error result");
    // Capture is `out\nerr\n`; appendStatus adds `\n\n` BEFORE the status, so
    // the rendered body carries the capture's own trailing newline plus the
    // separator's blank line.
    assert_eq!(result.content, "out\nerr\n\n\nCommand exited with code 7");

    // The empty-capture case keeps the upstream `(no output)` placeholder.
    let empty = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "exit 3"}),
        &context,
    )
    .await;
    assert_eq!(empty.content, "(no output)\n\nCommand exited with code 3");

    let _ = std::fs::remove_dir_all(&root);
}

/// PIR-BIN-032: model-facing path errors carry the native spelling, never the
/// Windows `\\?\` verbatim prefix (upstream pi's messages show `C:\…`).
#[cfg(windows)]
#[tokio::test]
async fn path_errors_do_not_leak_the_verbatim_prefix() {
    let root = temp_workspace();
    let registry = registry();
    let context = ToolContext::new(root.clone());

    let missing = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "nope.txt"}),
        &context,
    )
    .await;
    assert!(missing.is_error);
    assert!(
        missing.content.starts_with("Path not found: "),
        "missing read keeps its wording: {}",
        missing.content
    );
    assert!(
        !missing.content.contains(r"\\?\"),
        "no verbatim prefix leaks into the message: {}",
        missing.content
    );

    std::fs::write(root.join("a.txt"), "a\n").expect("fixture");
    let not_dir = run_tool(
        &registry,
        "ls",
        serde_json::json!({"path": "a.txt"}),
        &context,
    )
    .await;
    assert!(not_dir.is_error);
    assert!(
        not_dir.content.starts_with("Not a directory: "),
        "ls keeps its wording: {}",
        not_dir.content
    );
    assert!(
        !not_dir.content.contains(r"\\?\"),
        "no verbatim prefix leaks into the ls message: {}",
        not_dir.content
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// PIR-BIN-032 (read gate): the read-before-edit refusal names the path the
/// user typed, without the `\\?\` spelling the ledger keys on.
#[cfg(windows)]
#[tokio::test]
async fn read_gate_refusal_does_not_leak_the_verbatim_prefix() {
    let root = temp_workspace();
    std::fs::write(root.join("gate.txt"), "x\n").expect("fixture");
    let ledger = std::sync::Arc::new(pir::agent::read_ledger::ReadLedger::new(
        pir::agent::read_ledger::ReadBeforeEditMode::Enforce,
    ));
    let canonical = std::fs::canonicalize(root.join("gate.txt")).expect("canonical");
    let gate = ledger.gate_for_edit(&canonical, "edit");
    let pir::agent::read_ledger::EditGate::Blocked(message) = gate else {
        panic!("an unread file must be blocked in enforce mode");
    };
    assert!(
        message.contains("has not been read in this session"),
        "the refusal keeps its reason: {message}"
    );
    assert!(
        !message.contains(r"\\?\"),
        "the refusal must not leak the verbatim prefix: {message}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// PIR-BIN-029: the spill file a truncated shell result advertises must be
/// readable through `read`, exactly as the model would follow the footer.
#[tokio::test]
async fn advertised_spill_file_is_readable_through_read() {
    let _agent_dir = crate::common::AgentDirEnvGuard::scratch("ab-spill", None).await;
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    let registry = registry();
    let context = ToolContext::new(root.clone());

    // >1 MiB so the capture evicts a prefix and must spill.
    let run = run_tool(
        &registry,
        "bash",
        serde_json::json!({"command": "seq 1 200000"}),
        &context,
    )
    .await;
    assert!(!run.is_error, "unexpected error: {}", run.content);
    let details = run.details.clone().expect("truncation details");
    let path = details["fullOutputPath"]
        .as_str()
        .expect("advertised full output path")
        .to_owned();
    assert!(
        run.content.contains(&format!("Full output: {path}]")),
        "the footer advertises the same path as details.fullOutputPath"
    );
    assert!(
        spill::is_registered_spill(std::path::Path::new(&path)),
        "the produced path is registered"
    );
    assert!(
        !root.join(&path).exists() && !path.starts_with(root.to_string_lossy().as_ref()),
        "the spill file lives outside the workspace root: {path}"
    );

    // The contract: a follow-up `read` of exactly that path delivers the file.
    let read = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": path}),
        &context,
    )
    .await;
    assert!(
        !read.is_error,
        "the advertised spill path must be readable, got: {}",
        read.content
    );
    assert!(
        read.content.starts_with("1\n2\n3\n"),
        "the read delivers the spilled stream from its head: {:?}",
        &read.content[..read.content.len().min(40)]
    );

    // Confinement is NOT weakened: an outside path that is not a registered
    // spill file stays refused, even when its name looks like one.
    let planted = std::env::temp_dir().join(format!(
        "{}{}-h0-0123456789abcdef0123456789abcdef",
        spill::SPILL_PREFIX,
        std::process::id()
    ));
    std::fs::write(&planted, "secret\n").expect("planted file");
    let refused = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": planted.to_string_lossy()}),
        &context,
    )
    .await;
    assert!(
        refused.is_error && refused.content.contains("escapes the workspace root"),
        "a planted, unregistered outside file must stay refused: {}",
        refused.content
    );
    let _ = std::fs::remove_file(&planted);
    let _ = std::fs::remove_dir_all(&root);
}

/// PIR-BIN-033: reading a directory is a deterministic refusal on every
/// platform. Before the fix the Windows arm surfaced the localized
/// `拒绝访问。 (os error 5)` (an os error 5 from the read itself), the Unix arm
/// an OS `Is a directory` string, and upstream pi `EISDIR: illegal operation
/// on a directory, read` — three different texts for one user error.
#[tokio::test]
async fn read_of_a_directory_is_a_platform_independent_refusal() {
    let root = temp_workspace();
    std::fs::create_dir_all(root.join("adir")).expect("fixture dir");
    std::fs::write(
        root.join("adir").join("a.txt"),
        "x
",
    )
    .expect("fixture file");
    let registry = registry();
    let context = ToolContext::new(root.clone());

    let result = run_tool(
        &registry,
        "read",
        serde_json::json!({"path": "adir"}),
        &context,
    )
    .await;
    assert!(result.is_error, "a directory read must fail");
    assert_eq!(
        result.content,
        "Cannot read 'adir': is a directory (use ls to list its entries)"
    );
    // No OS code / locale text leaks into the message.
    assert!(!result.content.contains("os error"), "{}", result.content);
    assert!(!result.content.contains("EISDIR"), "{}", result.content);

    let _ = std::fs::remove_dir_all(&root);
}
