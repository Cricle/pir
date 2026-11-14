//! Unit tests for `src/coding_agent/session_pick.rs`, moved out of the
//! inline `#[cfg(test)] mod tests` block (source hygiene: no inline test
//! modules in `src/`). The seams they use are `#[doc(hidden)] pub`.

use pir::coding_agent::session_pick::{
    build_threaded_order, display_rows, filter_rows, PickerState, SessionRow, SortMode,
};
use std::path::PathBuf;

fn row(
    path: &str,
    name: Option<&str>,
    first_message: &str,
    modified_ms: u128,
    parent: Option<&str>,
) -> SessionRow {
    SessionRow {
        path: PathBuf::from(path),
        id: path.to_string(),
        cwd: "/tmp".to_string(),
        name: name.map(str::to_string),
        first_message: first_message.to_string(),
        all_messages_text: first_message.to_string(),
        message_count: 1,
        modified_ms,
        parent_session_path: parent.map(str::to_string),
    }
}

fn rows() -> Vec<SessionRow> {
    // b is a child of a; the threaded tree puts a's subtree first.
    vec![
        row("/tmp/a.jsonl", None, "alpha", 1_000, None),
        row(
            "/tmp/b.jsonl",
            Some("named"),
            "beta",
            3_000,
            Some("/tmp/a.jsonl"),
        ),
        row("/tmp/c.jsonl", None, "gamma", 2_000, None),
    ]
}

/// The default picker state (Threaded, empty query) must render the tree's
/// own pre-order with each row's node attached — the same sequence
/// `filter_rows(.., Threaded, ..)` + a node lookup produced before
/// `display_rows` was made single-pass.
#[test]
fn threaded_display_reuses_the_tree_order() {
    let rows = rows();
    let state = PickerState::with_all(rows.clone(), rows.clone());
    let display = display_rows(&state);
    let nodes = build_threaded_order(&rows);
    let expected: Vec<usize> = filter_rows(&rows, "", SortMode::Threaded, false);
    assert_eq!(
        display.iter().map(|entry| entry.row).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(display.len(), nodes.len());
    for (entry, node) in display.iter().zip(nodes.iter()) {
        assert_eq!(entry.row, node.row);
        assert_eq!(entry.node.as_ref(), Some(node));
    }
}

/// The named-only filter keeps the tree order and the per-row nodes.
#[test]
fn threaded_display_applies_the_named_filter_to_the_same_nodes() {
    let rows = rows();
    let mut state = PickerState::with_all(rows.clone(), rows.clone());
    state.named_only = true;
    let display = display_rows(&state);
    assert_eq!(
        display.iter().map(|entry| entry.row).collect::<Vec<_>>(),
        vec![1]
    );
    assert!(display[0].node.is_some());
    assert_eq!(
        display.iter().map(|entry| entry.row).collect::<Vec<_>>(),
        filter_rows(&rows, "", SortMode::Threaded, true)
    );
}

/// A query (or a non-threaded sort) drops the tree metadata: rows only.
#[test]
fn queried_display_has_no_thread_nodes() {
    let rows = rows();
    let mut state = PickerState::with_all(rows.clone(), rows.clone());
    state.query = "gamma".to_string();
    let display = display_rows(&state);
    assert_eq!(
        display.iter().map(|entry| entry.row).collect::<Vec<_>>(),
        vec![2]
    );
    assert!(display.iter().all(|entry| entry.node.is_none()));

    state.query.clear();
    state.sort_mode = SortMode::Recent;
    let display = display_rows(&state);
    assert_eq!(
        display.iter().map(|entry| entry.row).collect::<Vec<_>>(),
        vec![1, 2, 0]
    );
    assert!(display.iter().all(|entry| entry.node.is_none()));
}

/// The all-sessions scope (Tab) displays from its own list.
#[test]
fn all_scope_display_uses_the_all_rows() {
    let scoped = vec![rows()[0].clone()];
    let all = rows();
    let mut state = PickerState::with_all(scoped, all.clone());
    state.list_all = true;
    let display = display_rows(&state);
    assert_eq!(
        display.iter().map(|entry| entry.row).collect::<Vec<_>>(),
        filter_rows(&all, "", SortMode::Threaded, false)
    );
}
