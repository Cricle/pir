//! `read` call-header line-range labels, plus the render-thread lock discipline
//! that keeps them from freezing the transcript (lane W14-C).
//!
//! Upstream `formatReadLineRange` (render-utils.ts:38-49) runs INSIDE the
//! `with_theme` scope of `formatToolCallLine`, so it must never take the theme
//! lock again. pir's theme cell is a non-reentrant `Mutex<Theme>`
//! (`src/tui/theme/state.rs`): an inner `with_theme` there self-deadlocks the
//! UI render thread, which is exactly the `s43_read_range_label` failure — the
//! whole transcript froze (`UI thread stalled ... in phase render`) the moment
//! a `read` call carried `offset`/`limit`, before a single row painted.
//!
//! A deadlock cannot be asserted from inside the process that takes it (the
//! test binary would hang, not fail), so the two `*_without_deadlocking` cases
//! re-invoke THIS binary as a child (`mock` test below, same pattern as
//! `tests/mcp_frame_cap.rs`) and bound the wait. Before the fix the child
//! never exits; after it, it prints the formatted header.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pir::tui::ansi::strip_ansi;
use pir::tui::messages::format_read_line_range;
use pir::tui::theme::set_theme;

/// Upper bound for the child's whole run (process spawn + format). A
/// deadlocked child never exits, so this only has to sit far above the
/// microseconds the real work takes; it is generous so a loaded CI box cannot
/// flake.
const CHILD_BUDGET: Duration = Duration::from_secs(30);

/// Spawn this test binary running one `mock` child test, wait up to
/// [`CHILD_BUDGET`], and return its stdout.
///
/// On timeout the child is killed and the test FAILS: that is the deadlock
/// signal. The test name is passed as a libtest SUBSTRING filter (not
/// `--exact`), so it matches whatever module path `g_*` registers this suite
/// under; a filter that matches nothing produces no `RENDERED=` line, which
/// [`rendered_line`] turns into a failure rather than a silent pass.
fn run_child(test_name: &str, label: &str) -> String {
    let exe = std::env::current_exe().expect("current test exe");
    let mut child = Command::new(exe)
        .args([test_name, "--nocapture", "--test-threads=1"])
        .env("PIR_W14C_MOCK_CHILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the mock child");

    let deadline = Instant::now() + CHILD_BUDGET;
    loop {
        match child.try_wait().expect("poll the mock child") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "{label} did not finish within {CHILD_BUDGET:?} — a nested \
                     `with_theme` on the non-reentrant theme mutex deadlocks \
                     the UI render thread"
                );
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }

    let output = child
        .wait_with_output()
        .expect("collect the mock child output");
    assert!(
        output.status.success(),
        "{label}: the mock child failed ({:?}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Pull the `RENDERED=<text>` payload the mock children print.
///
/// FIND the marker inside a line, never require it at the start: libtest
/// writes the `test <name> ...` banner without a trailing newline, so the
/// child's `println!` is glued onto the same line —
/// `test tui_read_header_w14::mock_... ... RENDERED= | ...`. A prefix match
/// therefore never fired even though the payload was printed.
fn rendered_line(stdout: &str, label: &str) -> String {
    stdout
        .lines()
        .find_map(|line| {
            line.find("RENDERED=")
                .map(|at| &line[at + "RENDERED=".len()..])
        })
        .unwrap_or_else(|| panic!("{label}: no RENDERED= line in the child output:\n{stdout}"))
        .to_string()
}

// --- the label shapes (render-utils.ts:38-49) ---------------------------

/// `offset` + `limit` renders `:<offset>-<offset+limit-1>`; a lone `limit`
/// starts at line 1 (upstream's normalized behavior — the old inline matcher
/// emitted nothing at all when `offset` was absent).
#[test]
fn read_line_range_offsets_and_limits_render_the_normalized_shape() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");

    let both = format_read_line_range(&serde_json::json!({ "offset": 2, "limit": 1 }));
    assert_eq!(strip_ansi(&both), ":2-2");

    let limit_only = format_read_line_range(&serde_json::json!({ "limit": 1 }));
    assert_eq!(strip_ansi(&limit_only), ":1-1");

    let offset_only = format_read_line_range(&serde_json::json!({ "offset": 5 }));
    assert_eq!(strip_ansi(&offset_only), ":5");

    let wide = format_read_line_range(&serde_json::json!({ "offset": 10, "limit": 25 }));
    assert_eq!(strip_ansi(&wide), ":10-34");
}

/// Numeric strings count like numbers (`parseLineNumber`); an absent/null pair
/// renders no range; malformed inputs omit the range rather than leaking a
/// `NaN`/`inf` sentinel.
#[test]
fn read_line_range_accepts_numeric_strings_and_drops_malformed_values() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");

    let strings = format_read_line_range(&serde_json::json!({ "offset": "3", "limit": "4" }));
    assert_eq!(strip_ansi(&strings), ":3-6");

    for none in [
        serde_json::json!({}),
        serde_json::json!({ "path": "models.json" }),
        serde_json::json!({ "offset": null, "limit": null }),
    ] {
        assert_eq!(
            format_read_line_range(&none),
            "",
            "no offset/limit must render no range: {none}"
        );
    }

    for malformed in [
        serde_json::json!({ "offset": "abc", "limit": 2 }),
        serde_json::json!({ "offset": true }),
        serde_json::json!({ "offset": [1, 2] }),
        serde_json::json!({ "limit": "  " }),
    ] {
        let rendered = strip_ansi(&format_read_line_range(&malformed));
        assert_eq!(
            rendered, "",
            "a rejected line number renders no range: {malformed}"
        );
    }
}

// --- the render path that stalled (issue: s43_read_range_label) ----------
//
// Both cases drive the REAL render entry (`ToolExecutionComponent::render`)
// rather than the bare formatter, because the deadlock lives in the enclosing
// `with_theme` scope — the bare formatter takes the lock only once.

/// The `read` call block renders both range shapes. `models.json` is not a
/// compact-read resource, so this is the standard `formatReadCall` path
/// (`format_tool_call_line` → the range formatter while the theme lock is
/// held) — the exact header that never painted in s43.
#[test]
fn read_block_with_offset_and_limit_renders_without_deadlocking() {
    let stdout = run_child(
        "mock_read_block_offset_and_limit",
        "read block (offset+limit)",
    );
    let rendered = rendered_line(&stdout, "read block (offset+limit)");
    assert!(
        rendered.contains("read models.json:2-2"),
        "the offset+limit header must render: {rendered:?}"
    );

    let stdout = run_child("mock_read_block_limit_only", "read block (limit only)");
    let rendered = rendered_line(&stdout, "read block (limit only)");
    assert!(
        rendered.contains("read models.json:1-1"),
        "the limit-only header must render `:1-1`: {rendered:?}"
    );
}

/// The compact read header (`formatCompactReadCall`, read.ts:89-111) formats
/// the same range from inside its OWN `with_theme`; an agent-resource read
/// with `offset`/`limit` must render too (the second in-lock caller).
#[test]
fn compact_read_block_with_offset_renders_without_deadlocking() {
    let stdout = run_child("mock_compact_read_offset", "compact read block");
    let rendered = rendered_line(&stdout, "compact read block");
    assert!(
        rendered.contains(":1-3"),
        "the compact read header keeps the range: {rendered:?}"
    );
    assert!(
        rendered.contains("ctrl+o to expand"),
        "the compact read header keeps its expand hint: {rendered:?}"
    );
}

// --- mock children (re-invoked this binary; see `run_child`) ------------
//
// Each child takes the theme lock through the real render path. If the path
// is nested-deadlocking, the child hangs and the parent's budget fires.

/// Render a plain `read` block and print its rows. Args come from the test
/// name so one child serves both range shapes.
fn mock_read_block(args: serde_json::Value) {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use pir::tui::core::Component;
    use pir::tui::messages::ToolExecutionComponent;

    set_theme("dark");
    let block =
        ToolExecutionComponent::new("read", Arc::new(args), Arc::new(AtomicBool::new(false)));
    let rows = block.render(70).join("\n");
    println!("RENDERED={}", strip_ansi(&rows).replace('\n', " | "));
}

#[test]
fn mock_read_block_offset_and_limit() {
    // Gated so the normal `cargo test` run only exercises this via `run_child`.
    if std::env::var_os("PIR_W14C_MOCK_CHILD").is_none() {
        return;
    }
    mock_read_block(serde_json::json!({ "path": "models.json", "offset": 2, "limit": 1 }));
}

#[test]
fn mock_read_block_limit_only() {
    if std::env::var_os("PIR_W14C_MOCK_CHILD").is_none() {
        return;
    }
    mock_read_block(serde_json::json!({ "path": "models.json", "limit": 1 }));
}

#[test]
fn mock_compact_read_offset() {
    if std::env::var_os("PIR_W14C_MOCK_CHILD").is_none() {
        return;
    }
    // `AGENTS.md` is an agent-resource name (`COMPACT_RESOURCE_FILE_NAMES`), so
    // this header goes through `formatCompactReadCall`.
    mock_read_block(serde_json::json!({ "path": "AGENTS.md", "offset": 1, "limit": 3 }));
}
