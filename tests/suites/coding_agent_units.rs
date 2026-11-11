//! Coding-agent unit tests moved out of `src/coding_agent/**` inline
//! `#[cfg(test)]` modules:
//!
//! - `src/coding_agent/tool_selection.rs` → the `PerformanceProfile`-driven
//!   implicit tool selection (`performance_selection_tests`);
//! - `src/coding_agent/context.rs` → the total context-budget truncation
//!   (`context_budget_tests`);
//! - `src/coding_agent/tools/bash.rs` → the whole-filesystem search refusal
//!   (`broad_search_tests`) and the Windows PATH → MSYS spelling conversion
//!   (`msys_path_tests`);
//! - `src/coding_agent/tools/util.rs` → the shell-substitution refusal in
//!   `workspace_path_parts` (`path_substitution_tests`).
//!
//! Every private item those modules drove now has a `#[doc(hidden)] pub` seam
//! in `src/` (documented at its definition); the assertions are unchanged.

use pir::agent::tool_execution::ToolExposure;
use pir::coding_agent::tool_selection::{
    resolve_tool_selection_with_availability_and_profile, AvailableTool,
};
use pir::config::settings::{PerformanceProfile, DEFAULT_TOOL_NAMES};

// ---------------------------------------------------------------------------
// src/coding_agent/tool_selection.rs — performance_selection_tests
// ---------------------------------------------------------------------------

fn available() -> Vec<AvailableTool> {
    vec![
        AvailableTool::registered("read", ToolExposure::Direct, true),
        AvailableTool::registered("bash", ToolExposure::Direct, true),
        AvailableTool::registered("edit", ToolExposure::Direct, true),
        AvailableTool::registered("write", ToolExposure::Direct, true),
        AvailableTool::registered("todo", ToolExposure::Direct, true),
    ]
}

#[test]
fn balanced_implicit_selection_stays_none() {
    let resolved = resolve_tool_selection_with_availability_and_profile(
        None,
        &[],
        &[],
        false,
        false,
        &available(),
        &DEFAULT_TOOL_NAMES,
        PerformanceProfile::Balanced,
    )
    .expect("valid selection");
    assert!(resolved.is_none());
}

#[test]
fn efficient_implicit_selection_is_core_four() {
    let resolved = resolve_tool_selection_with_availability_and_profile(
        None,
        &[],
        &[],
        false,
        false,
        &available(),
        &DEFAULT_TOOL_NAMES,
        PerformanceProfile::Efficient,
    )
    .expect("valid selection")
    .expect("efficient profile selects core tools");
    assert_eq!(
        resolved.enabled,
        DEFAULT_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert!(resolved.uses_default_tools);
}

#[test]
fn explicit_tools_win_over_efficient_default() {
    let resolved = resolve_tool_selection_with_availability_and_profile(
        None,
        &["todo".to_owned()],
        &[],
        false,
        false,
        &available(),
        &DEFAULT_TOOL_NAMES,
        PerformanceProfile::Efficient,
    )
    .expect("valid selection")
    .expect("explicit selection");
    assert_eq!(resolved.enabled, vec!["todo".to_owned()]);
}

// ---------------------------------------------------------------------------
// src/coding_agent/context.rs — context_budget_tests
// ---------------------------------------------------------------------------

#[test]
fn total_budget_truncates_at_utf8_boundary_and_marks_it() {
    use std::path::PathBuf;

    let files = vec![(PathBuf::from("a.md"), "é".repeat(100))];
    let out = pir::coding_agent::context::truncate_context_files(files, 80);
    assert_eq!(out.len(), 1);
    assert!(out[0]
        .1
        .ends_with("[context truncated by performance profile]"));
    assert!(out[0].1.is_char_boundary(out[0].1.len()));
}

// ---------------------------------------------------------------------------
// src/coding_agent/tools/bash.rs — broad_search_tests
// ---------------------------------------------------------------------------

#[test]
fn root_searches_are_refused_but_scoped_ones_are_allowed() {
    use pir::coding_agent::tools::broad_search_violation;

    assert!(broad_search_violation("find / -name '*.log'").is_some());
    assert!(broad_search_violation("grep -R foo /tmp").is_some());
    assert!(broad_search_violation("rg pattern /").is_some());
    assert!(broad_search_violation("cd /tmp && node --test").is_some());
    assert!(broad_search_violation("cd /").is_some());
    assert!(broad_search_violation("cd /* && ls").is_some());
    assert!(broad_search_violation("find /* -name app.log").is_some());
    assert!(broad_search_violation("cd /tmp/pir-hard-logq-project && node --test").is_none());
    assert!(broad_search_violation("find . -name '*.log'").is_none());
    assert!(broad_search_violation("rg pattern src").is_none());
    assert!(broad_search_violation("grep -R foo test").is_none());
}

// ---------------------------------------------------------------------------
// src/coding_agent/tools/bash.rs — msys_path_tests (Windows only)
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[test]
fn windows_path_entries_become_msys_drive_paths() {
    use pir::coding_agent::tools::msys_path_from_windows;

    assert_eq!(
        msys_path_from_windows(r"C:\wp\bin;D:/tools/node;/c/already"),
        "/c/wp/bin:/d/tools/node:/c/already"
    );
}

// ---------------------------------------------------------------------------
// src/coding_agent/tools/util.rs — path_substitution_tests
// ---------------------------------------------------------------------------

#[test]
fn shell_substitutions_are_rejected_as_tool_paths() {
    use pir::coding_agent::tools::workspace_path_parts;

    let cwd = std::env::temp_dir();
    for path in ["$(pwd)/logq.js", "`pwd`/logq.js", "${PWD}/logq.js"] {
        match workspace_path_parts(&cwd, path) {
            Ok(_) => panic!("must reject shell path: {path}"),
            Err(error) => assert!(error.contains("literal filesystem path"), "{error}"),
        }
    }
}
