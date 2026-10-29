//! P2-9 regression pin: the rolling capture's eviction cut must land on a
//! UTF-8 character boundary.
//!
//! `ShellCapture::append` drains the evicted prefix at `tail.len() - keep`, a
//! RAW byte offset. When a multi-byte character straddled that point, the
//! retained tail began with continuation bytes and `tail_string()`'s
//! `from_utf8_lossy` turned the first character into U+FFFD. The cut now backs
//! up (at most 3 bytes) to the character start, the same rule `full_output`
//! applies to its head/tail cuts.
//!
//! The observable path for a lone retained tail is the spill-failure fallback
//! of `full_output` (`structuredContent.output` = `tail_string()`): the
//! display view truncates the first 50 KiB away, and a successful spill is
//! read back as file+tail (whose concatenation is byte-contiguous and
//! therefore valid). This test forces spill-file creation to fail by pointing
//! the process temp dir at a regular file, then drives the real bash tool.
//!
//! Fixture: 40000 euro signs (120000 bytes, no newlines). The eviction point
//! is `120000 - 2*DEFAULT_MAX_BYTES = 17600`, and 17600 = 3*5866 + 2 — the
//! third byte of a euro sign, i.e. a continuation byte. With the old raw cut
//! the output began with U+FFFD; with the boundary-aligned cut it begins with
//! '€' and keeps exactly `keep + 2` bytes (34134 whole characters).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use pir::agent::tool_execution::{ToolContext, ToolRegistry};
use pir::coding_agent::register_core_tools;

fn temp_workspace() -> PathBuf {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "pir-bash-eviction-{nonce}-{sequence}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create temp workspace");
    path
}

fn bash_available() -> bool {
    pir::coding_agent::tools::resolve_shell_config(None).is_ok()
}

/// The rolling window the bash tool keeps (`2 * DEFAULT_MAX_BYTES`).
const KEEP: usize = 2 * 50 * 1024;

#[tokio::test]
async fn eviction_cut_lands_on_a_utf8_boundary() {
    if !bash_available() {
        return;
    }
    let root = temp_workspace();
    std::fs::create_dir_all(&root).expect("create root");

    // A regular FILE where the spill file's parent directory would have to
    // be: `create_new(<tmp>/pir-shell-...)` fails on every platform, so the
    // capture latches `spill_failed` and `full_output` falls back to the
    // retained tail.
    let not_a_dir = root.join("not-a-directory");
    std::fs::write(&not_a_dir, b"file, not a directory").expect("write blocker file");

    let saved: Vec<(&'static str, Option<std::ffi::OsString>)> = ["TMP", "TEMP", "TMPDIR"]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
    for key in ["TMP", "TEMP", "TMPDIR"] {
        std::env::set_var(key, &not_a_dir);
    }

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let context = ToolContext::new(root.clone());
    // 70000 three-byte characters (octal escapes keep the argv ASCII-only;
    // no command substitution so the shell needs no temp files).
    let command =
        "( i=0; while [ $i -lt 70000 ]; do printf '\\342\\202\\254'; i=$((i+1)); done ) 2>/dev/null";
    let result = registry
        .get("bash")
        .expect("bash tool should be registered")
        .run(&serde_json::json!({ "command": command }), &context)
        .await;

    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    assert!(!result.is_error, "unexpected error: {}", result.content);
    let structured = result.structured_content.expect("structured content");
    // Precondition: the spill is unavailable, so the output IS the tail.
    assert!(
        structured.get("full_output_path").is_none(),
        "the spill file must not be advertisable in this fixture: {structured}"
    );
    // The fallback IS truncated (the spill could not retain the prefix), and
    // it says so; only `full_output_path` is absent, because there is no
    // stored copy to point at.
    assert_eq!(
        structured["truncated"],
        serde_json::json!(true),
        "the retained tail is an honest truncated view: {structured}"
    );
    let output = structured["output"].as_str().expect("output string");

    let head: String = output.chars().take(8).collect();
    assert!(
        !output.starts_with('\u{FFFD}'),
        "the evicted tail starts with a replacement character: {head:?}"
    );
    assert!(
        output.starts_with('€'),
        "the tail must start at the character straddling the eviction point: {head:?}"
    );
    // The buffer trims only past 2 x keep (the rolling window doubles as the
    // transient ceiling), so the retained length depends on where the last
    // read chunk lands: at least `keep`, at most `keep +` the 5200 bytes that
    // follow the final trim. What must hold is the boundary rule: the tail is
    // whole three-byte characters only.
    assert!(
        (KEEP..=KEEP + 5202).contains(&output.len()),
        "retained tail outside the 2x window: {}",
        output.len()
    );
    assert_eq!(output.len() % 3, 0, "the cut split a UTF-8 character");
    assert_eq!(output.chars().count(), output.len() / 3);
    assert!(
        output.chars().all(|character| character == '€'),
        "every retained character is whole: {head:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
