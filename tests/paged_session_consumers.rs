//! P0.3 follow-up — every WHOLE-SESSION consumer of a paged (tail-window)
//! session must observe the FULL history, never a silent tail window.
//!
//! `SessionManager::open_paged` parses only a byte/entry-capped tail window;
//! the skipped prefix stays on disk. The host commands that operate on the
//! whole session therefore either widen first
//! (`session_resolve::ensure_full_transcript` — `/export`, `/share`, `/bug`,
//! `/session`, `/compact` and the RPC read commands) or STREAM the prefix
//! (`tui::terminal::fork_user_messages_streamed` for the `/fork` listing, and
//! `SessionManager::for_each_tree_row` — which the TUI maps to picker rows
//! through `tui::terminal::tree_entries_streamed` — for the `/tree` listing).
//! A streamed `/tree` pick may still load: navigation and label saves widen on
//! the ACTION when the chosen row is a prefix entry, never for the listing.
//! This suite pins each of those paths against a full-load baseline of the
//! same file, plus the legacy-session fallback of the tail-first open.
//!
//! The `/tree` pin exists at BOTH levels the consumer lane asked for:
//! `tree_listing_streams_the_prefix_without_widening` (TUI rows) and
//! `tree_scan_is_whole_session_for_a_paged_manager` (the API itself: rows AND
//! `TreeScanStats` equal a full load's, the prefix counter proves the stream,
//! and `whole_file == false`). `tree_scan_handles_duplicate_ids_and_cycles_across_the_page`
//! adds the invalid-data shapes whose prefix half a resident-only walk got
//! wrong (a duplicate id, and a parent cycle split across the page boundary).
//!
//! Classification of the remaining consumers (documented, tested elsewhere):
//! rendering/replay sees the window BY DESIGN and shows the "opened paged"
//! notice; `/sessions` + `/resume` listings stream the file (bounded peek);
//! the server snapshot path opens fully.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{usage_cost_breakdown, AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::bug_report::collect_bug_report_diagnostics;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::coding_agent::session_resolve::{ensure_full_transcript, open_session_tail_first};
use pir::session_manager::types::{FileEntry, MessageEntry, SessionHeader, StoredMessage};
use pir::session_manager::{SessionManager, TreeRowMeta, MAX_PREFIX_ENTRY_LINE_BYTES};
use pir::tui::selector::{TreeFilterMode, TreeSelector};
use pir::tui::terminal::{
    fork_user_messages_streamed, tree_entries_for_command, tree_entries_streamed,
};

// ---------------------------------------------------------------------------
// Fixture: a session file larger than a small page cap
// ---------------------------------------------------------------------------

/// How many ordinary turns the fixture records. The BYTE cap (not the entry
/// count) is what pages this fixture: the turns' payload keeps the file above
/// the host default `DEFAULT_TAIL_PAGE_BYTES` (1 MiB), so the attach test still
/// proves the tail-first open — while the DEPTH is now the crash-scale one.
///
/// DEPTH IS THE POINT (2000 entries): `SessionManager::tree` returns a chain
/// nested one `TreeNode` per entry, and the operations over that chain used to
/// recurse with the session length (the derived `TreeNode`/`serde_json::Value`
/// serialization, `Value` equality, the container drop glue) — at ~1400
/// entries the `get_tree` path overflowed a test thread (STATUS_STACK_OVERFLOW).
/// That lane landed the iterative emission (`TreeNode::to_value_iterative` /
/// `forest_to_json_iterative`, the `TreeChildren` teardown, the flat row scan,
/// and the stack-safe payload/response writers), so this suite now pins the
/// deep case end to end instead of staying under the old crash point.
const FIXTURE_TURNS: usize = 2_000;

/// Payload bytes per user turn. Deliberately smaller than the old 8 KiB: with
/// 2000 turns the fixture still clears the 1 MiB page cap (~2.6 MB file) while
/// keeping the per-test fixture build (one fsync'd append per turn) and the
/// five widening RPC reads cheap.
const TURN_PAYLOAD_BYTES: usize = 1024;

struct Fixture {
    dir: PathBuf,
    path: PathBuf,
    /// The FIRST appended entry's id: on a paged open it is always inside the
    /// skipped prefix (the window is the file's tail), so a payload that
    /// carries it proves the consumer read the prefix.
    prefix_entry_id: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn user_message(text: String) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

fn assistant_message(text: &str, cost: f64) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: 0,
        usage: Usage {
            input: 120,
            output: 30,
            cache_read: 10,
            cache_write: 5,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 165,
            cost: Cost {
                input: cost,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: cost,
                ..Cost::default()
            },
        },
        ..Default::default()
    })
}

/// A tool result whose `/tree` row must render `[tool_name]` — the display
/// fact `TreeRowMeta::preview` (the result TEXT) cannot express, so a streamed
/// prefix row needs `TreeRowMeta::tool_name` to stay byte-identical to the
/// full-load oracle.
fn tool_result_message(tool_name: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call-0001".into(),
        tool_name: tool_name.into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp: 0,
    })
}

/// Build a persisted session with `FIXTURE_TURNS` user turns, an assistant
/// turn carrying usage, a label, and a BRANCH (an earlier leaf move with a
/// second, shorter branch appended after it).
fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "pir-paged-consumers-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    // The session cwd must EXIST: the RPC `attach` path applies upstream
    // `assertSessionCwdExists` (session-cwd.ts), and these fixtures are
    // attached over RPC by the widening tests below. The fixture directory
    // itself is a real, existing cwd.
    let session_cwd = dir.to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&session_cwd, &dir).expect("create session");
    let path = manager.path().to_path_buf();
    let mut ids: Vec<String> = Vec::new();
    let mut fork_point: Option<String> = None;
    for index in 0..FIXTURE_TURNS {
        if index == FIXTURE_TURNS / 2 {
            // Assistant usage record (drives the /session cost breakdown).
            ids.push(
                manager
                    .append_message(assistant_message("assistant turn", 0.25))
                    .expect("append assistant"),
            );
            // The labeled entry + the branch point live in the PREFIX of a
            // paged open (they are old enough).
            let labeled = ids.last().cloned().unwrap_or_default();
            ids.push(
                manager
                    .set_label(&labeled, Some("checkpoint"))
                    .expect("append label"),
            );
            fork_point = Some(labeled);
            // A TOOL RESULT in the PREFIX: its `/tree` row renders
            // `[read]` from the row's `tool_name`, while the row's `preview`
            // carries the result text — so the streamed rows prove the name
            // traveled with the metadata.
            ids.push(
                manager
                    .append_message(tool_result_message("read", "tool result payload"))
                    .expect("append tool result"),
            );
        }
        let mut text = format!("message {index:05}\n");
        text.push_str(
            &"paged consumer payload line\n"
                .repeat(TURN_PAYLOAD_BYTES / "paged consumer payload line\n".len() + 1),
        );
        ids.push(
            manager
                .append_message(user_message(text))
                .expect("append user"),
        );
    }
    // Branch from the middle: move the leaf back, then append a short new
    // branch so `branch_entries(None)` is NOT the file's full entry list.
    if let Some(point) = &fork_point {
        manager.branch(point).expect("branch");
        for index in 0..3 {
            ids.push(
                manager
                    .append_message(user_message(format!("branched follow-up {index}")))
                    .expect("append branch"),
            );
        }
    }
    let prefix_entry_id = ids.first().cloned().unwrap_or_default();
    Fixture {
        dir,
        path,
        prefix_entry_id,
    }
}

fn paged(path: &Path) -> SessionManager {
    let manager = SessionManager::open_paged(path, 4 * 1024, 16).expect("open fixture paged");
    assert!(
        manager.is_paged(),
        "the fixture must exceed the page caps ({} entries)",
        manager.resident_entries().len()
    );
    manager
}

/// Write one hand-built session JSONL file: the header plus `entries`, in the
/// order given. Used by the invalid-data fixture, which the append API cannot
/// produce (a duplicate entry id, a parent cycle).
fn write_raw_session(path: &Path, entries: &[FileEntry]) {
    let header = FileEntry::Session(SessionHeader::new(
        "pathological-session",
        "C:\\repo",
        "2026-10-17T00:00:00.000Z".to_owned(),
    ));
    let mut text = String::new();
    text.push_str(&serde_json::to_string(&header).expect("header json"));
    text.push('\n');
    for entry in entries {
        text.push_str(&serde_json::to_string(entry).expect("entry json"));
        text.push('\n');
    }
    std::fs::write(path, text).expect("write session file");
}

/// A hand-built session whose page boundary splits the two invalid-data
/// shapes a resident-only tree walk got wrong:
///
/// - a DUPLICATE id: `d0000000` first appears as the chain's root (skipped
///   prefix) and again as the file's LAST entry (resident, under `d0000004`),
///   so `by_id` / `tree()` keep the last occurrence's facts;
/// - a parent CYCLE split across the boundary: `c0000001` (prefix) and
///   `c0000002` (resident) point at each other, so neither is reachable from a
///   root — and the resident half must NOT become a root.
///
/// The test computes the caps from the fixture's own byte lengths so the
/// window is exactly the file's last four entries (deterministic, no magic
/// numbers to drift).
fn pathological_fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "pir-paged-raw-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("pathological.jsonl");
    // Padding keeps each line comfortably larger than the byte share of the
    // 4-entry cap, so the ENTRY cap pins the window start.
    let pad = "cycle and duplicate payload ".repeat(6);
    let entry = |index: usize, id: &str, parent: Option<&str>, text: &str| -> FileEntry {
        FileEntry::Message(MessageEntry {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            timestamp: format!("2026-10-17T00:{:02}:{:02}.000Z", index / 60, index % 60),
            message: StoredMessage::from(user_message(format!("{text} {pad}"))),
        })
    };
    let entries = vec![
        entry(1, "d0000000", None, "first occurrence"),
        entry(2, "c0000001", Some("c0000002"), "cycle A"),
        entry(3, "d0000001", Some("d0000000"), "second"),
        entry(4, "d0000002", Some("d0000001"), "third"),
        entry(5, "d0000003", Some("d0000002"), "fourth"),
        entry(6, "d0000004", Some("d0000003"), "fifth"),
        entry(7, "c0000002", Some("c0000001"), "cycle B"),
        entry(8, "d0000000", Some("d0000004"), "duplicate occurrence"),
    ];
    write_raw_session(&path, &entries);
    Fixture {
        dir,
        path,
        prefix_entry_id: "d0000000".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// (a) widen-before-command: the seam every whole-session host command calls
// ---------------------------------------------------------------------------

#[test]
fn ensure_full_transcript_restores_entries_tree_and_session_scans() {
    let fixture = fixture("widen");
    let full = SessionManager::open(&fixture.path).expect("full open");
    let mut manager = paged(&fixture.path);
    assert!(
        manager.entries().len() < full.entries().len(),
        "sanity: the paged view starts truncated"
    );

    // The seam is idempotent and reports whether it widened.
    assert!(ensure_full_transcript(&mut manager).expect("widen"));
    assert!(!manager.is_paged());
    assert!(!ensure_full_transcript(&mut manager).expect("second widen"));
    assert_eq!(manager.entries(), full.entries());

    // `/tree`: the listing now STREAMS a paged session (the widen-based rows
    // are only the full-load oracle; see
    // `tree_listing_streams_the_prefix_without_widening` below), so this
    // widen seam no longer has a `/tree` caller to exercise here.

    // `/session`: the per-model cost breakdown / cache-waste scans the TUI
    // feeds from `sm.entries()` must see the whole session after the widen.
    let baseline_breakdown = usage_cost_breakdown(&full.entries());
    let manager_breakdown = usage_cost_breakdown(&manager.entries());
    assert_eq!(manager_breakdown, baseline_breakdown);
    assert!(
        !baseline_breakdown.is_empty(),
        "the fixture carries assistant usage"
    );
    let baseline_waste = pir::agent_session::compute_cache_waste(&full.entries(), &|_, _| None);
    let manager_waste = pir::agent_session::compute_cache_waste(&manager.entries(), &|_, _| None);
    assert_eq!(manager_waste, baseline_waste);

    // `/export` + `/share`: the serialized branch after the widen is
    // byte-identical to a full load's — except the header's `timestamp`, which
    // upstream stamps with the EXPORT time (`session-export.ts:13`
    // `new Date().toISOString()`), so two exports of the same session are
    // intentionally not byte-identical on that one field. Compare the branch
    // body verbatim and the header with that field normalized.
    let baseline_jsonl = pir::tui::slash::serialize_session_branch(&full);
    let manager_jsonl = pir::tui::slash::serialize_session_branch(&manager);
    let normalize_export_time = |jsonl: &str| -> Vec<String> {
        jsonl
            .lines()
            .enumerate()
            .map(|(index, line)| {
                if index != 0 {
                    return line.to_owned();
                }
                let mut value: serde_json::Value =
                    serde_json::from_str(line).expect("the header line is JSON");
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "timestamp".to_owned(),
                        serde_json::Value::String("<export-time>".to_owned()),
                    );
                }
                value.to_string()
            })
            .collect()
    };
    assert_eq!(
        normalize_export_time(&manager_jsonl),
        normalize_export_time(&baseline_jsonl)
    );
    assert!(
        manager_jsonl.lines().count() > 50,
        "the export carries the whole branch"
    );

    // `/bug`: the diagnostics summarize the whole session.
    let baseline_diagnostics =
        collect_bug_report_diagnostics(&full.entries(), Some(full.id()), &[]);
    let manager_diagnostics =
        collect_bug_report_diagnostics(&manager.entries(), Some(manager.id()), &[]);
    assert_eq!(manager_diagnostics, baseline_diagnostics);
    assert_eq!(
        baseline_diagnostics["entryCount"].as_u64(),
        Some(full.entries().len() as u64)
    );
}

/// The same seam on a session that fits the page caps: nothing to do, no
/// error, and the resident list was already complete.
#[test]
fn ensure_full_transcript_is_a_noop_for_small_sessions() {
    let dir = std::env::temp_dir().join(format!(
        "pir-paged-consumers-small-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let mut manager = SessionManager::create("C:\\repo", &dir).expect("create session");
    let path = manager.path().to_path_buf();
    manager
        .append_message(user_message("one".to_owned()))
        .expect("append");
    let mut opened = SessionManager::open_paged(&path, 4 * 1024, 16).expect("open");
    assert!(!opened.is_paged(), "a small session opens fully");
    assert!(!ensure_full_transcript(&mut opened).expect("noop"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// (b) stream-the-prefix: the `/fork` listing
// ---------------------------------------------------------------------------

#[test]
fn fork_listing_streams_the_prefix_without_widening() {
    let fixture = fixture("fork");
    let full = SessionManager::open(&fixture.path).expect("full open");
    let (baseline, baseline_prefix) =
        fork_user_messages_streamed(&full).expect("baseline fork listing");
    assert_eq!(baseline_prefix, 0, "a full session has no skipped prefix");

    // The reference shape: the user text messages on the branch, in order.
    let expected: Vec<(String, String)> = full
        .branch_entries(None)
        .expect("branch")
        .into_iter()
        .filter_map(|entry| match entry {
            pir::session_manager::FileEntry::Message(inner) => match &inner.message {
                pir::session_manager::types::StoredMessage::User(user)
                    if matches!(user.role, UserRole::User) =>
                {
                    let text = user
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    (!text.is_empty()).then(|| (inner.id.clone(), text))
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(baseline, expected);
    assert!(baseline.len() > 50, "the branch carries many user turns");

    // The paged listing must produce the SAME pairs while reading the prefix
    // from disk, and it must NOT widen the session (the transcript never
    // becomes resident just to list fork points).
    let manager = paged(&fixture.path);
    let (streamed, prefix_entries) =
        fork_user_messages_streamed(&manager).expect("streamed fork listing");
    assert_eq!(streamed, baseline);
    assert!(
        prefix_entries > 0,
        "the walk read the skipped prefix from disk"
    );
    assert!(manager.is_paged(), "the listing never widens the session");
}

// ---------------------------------------------------------------------------
// (b) stream-the-prefix: the `/tree` listing
// ---------------------------------------------------------------------------

/// `/tree` on a PAGED session lists the WHOLE tree while the prefix is
/// streamed, not materialized: the rows equal a full load's (order, labels,
/// displays), the session stays paged, and a picked prefix row resolves to
/// its entry once the action-time widen (the worker's navigate / label seam)
/// loads the file.
///
/// The API-level shape of the same invariant — `for_each_tree_row`'s rows AND
/// `TreeScanStats`, the prefix counter and `whole_file == false` — is pinned
/// separately by `tree_scan_is_whole_session_for_a_paged_manager`, so this
/// test stays the TUI-facing one (the walk itself now lives in the manager,
/// `tree_entries_streamed` only maps rows to picker shape).
#[test]
fn tree_listing_streams_the_prefix_without_widening() {
    let fixture = fixture("tree");

    // The full-load oracle: the widen-based rows `/tree` used to build, and
    // the tail page's own counters (`whole_file == false`).
    let mut full = SessionManager::open(&fixture.path).expect("full open");
    let baseline = tree_entries_for_command(&mut full).expect("baseline tree");
    assert!(
        baseline.len() > 100,
        "the listing is the whole session, not a window: {} rows",
        baseline.len()
    );
    assert!(
        baseline
            .iter()
            .any(|row| row.label.as_deref() == Some("checkpoint")),
        "the PREFIX entry's label must be in the oracle rows"
    );
    assert!(
        baseline.iter().any(|row| row.display == "[read]"),
        "a PREFIX tool result's oracle row renders `[tool_name]`"
    );
    let page = pir::session_manager::tail_page::SessionTailPage::open(&fixture.path, 4 * 1024, 16)
        .expect("page probe")
        .expect("the fixture must exceed the page caps");
    assert!(
        !page.stats().whole_file,
        "the fixture must NOT fit the page"
    );

    // A session that fits the page caps lists the same rows.
    let small = SessionManager::open(&fixture.path).expect("resident open");
    assert!(!small.is_paged());
    let small_rows = tree_entries_streamed(&small).expect("resident listing");
    assert_eq!(small_rows, baseline, "resident rows must equal today's");

    // The paged listing: every row, in the same order, read from disk one
    // entry at a time while the transcript stays unmaterialized.
    let mut manager = paged(&fixture.path);
    let entries_before = manager.entries().len();
    assert!(
        entries_before < full.entries().len(),
        "sanity: the paged view starts truncated"
    );
    let streamed_rows = tree_entries_streamed(&manager).expect("streamed tree listing");
    assert_eq!(
        streamed_rows, baseline,
        "streamed rows must equal a full load's (order, labels, displays)"
    );
    assert!(
        streamed_rows
            .iter()
            .any(|row| row.id == fixture.prefix_entry_id),
        "the oldest (prefix) entry must be listed"
    );
    assert!(
        streamed_rows.iter().any(|row| row.display == "[read]"),
        "a PREFIX tool result must render `[tool_name]` from the streamed metadata"
    );
    let labeled = streamed_rows
        .iter()
        .find(|row| row.label.as_deref() == Some("checkpoint"))
        .expect("the PREFIX entry's label must be listed");
    // The listing never widened: the manager is still the same tail window.
    assert!(manager.is_paged(), "the listing never widens the session");
    assert_eq!(
        manager.entries().len(),
        entries_before,
        "the listing materialized nothing"
    );
    assert!(manager
        .paged_window()
        .is_some_and(|window| window.page_start > 0));

    // The picker over the streamed rows behaves like the full-load one:
    // selection resolves to the row's entry id, and the labeled-only filter
    // finds the PREFIX entry's label.
    let selector = TreeSelector::new(&streamed_rows, TreeFilterMode::Default);
    assert_eq!(selector.selected(), Some(streamed_rows[0].id.clone()));
    assert!(selector.select_id(&labeled.id));
    assert_eq!(selector.selected().as_deref(), Some(labeled.id.as_str()));
    selector.set_filter_mode(TreeFilterMode::LabeledOnly);
    assert_eq!(selector.selected(), Some(labeled.id.clone()));

    // Acting on the selected prefix row needs it resident: the paged index
    // cannot resolve it, and the action-time widen (the worker's navigate /
    // label seam) can.
    assert!(
        manager.entry(&labeled.id).is_none(),
        "prefix rows stay unmaterialized until an action"
    );
    assert!(ensure_full_transcript(&mut manager).expect("action-time widen"));
    assert!(
        manager.entry(&labeled.id).is_some(),
        "the action-time widen makes the selection resolvable"
    );
}

// ---------------------------------------------------------------------------
// (b) the `/tree` API itself: `for_each_tree_row` is whole-session for a paged
// manager (the TUI test above only maps its rows)
// ---------------------------------------------------------------------------

/// The API-level pin: `SessionManager::for_each_tree_row` on a PAGED manager
/// covers the WHOLE session — the skipped prefix is streamed into row
/// metadata, merged with the resident window, and the shared DFS runs — so its
/// rows AND its `TreeScanStats` are identical to a full load's while the
/// manager stays paged.
///
/// This used to be a silent-truncation trap: the scan iterated `file_entries`
/// alone, so on the fixture's paged manager it listed a few thousand rows
/// instead of the whole session, and the window's oldest entry (its parent in
/// the skipped prefix) became a root.
#[test]
fn tree_scan_is_whole_session_for_a_paged_manager() {
    let fixture = fixture("tree-api");
    let full = SessionManager::open(&fixture.path).expect("full open");
    let mut full_rows: Vec<TreeRowMeta> = Vec::new();
    let full_stats = full
        .for_each_tree_row(|row| full_rows.push(row.clone()))
        .expect("full scan");
    assert_eq!(full_stats.rows, full_rows.len());
    assert!(full_stats.rows > 100, "the fixture is a long session");

    // The fixture does not fit the page caps (the tail page says so itself).
    let page = pir::session_manager::tail_page::SessionTailPage::open(&fixture.path, 4 * 1024, 16)
        .expect("page probe")
        .expect("the fixture must exceed the page caps");
    assert!(
        !page.stats().whole_file,
        "the fixture must NOT fit the page"
    );

    let manager = paged(&fixture.path);
    let entries_before = manager.entries().len();
    assert!(
        entries_before < full_stats.entries,
        "sanity: the paged window is a tail of the session"
    );

    let mut paged_rows: Vec<TreeRowMeta> = Vec::new();
    let paged_stats = manager
        .for_each_tree_row(|row| paged_rows.push(row.clone()))
        .expect("paged scan");

    // (1) The rows are a full load's, field for field and in DFS order.
    assert_eq!(
        paged_rows, full_rows,
        "a paged scan must equal a full load's rows (ids, parents, depths, timestamps, labels, kinds, previews, tool names)"
    );
    // (2) The stats describe the whole session too — never the window.
    assert_eq!(
        paged_stats, full_stats,
        "the stats must count the whole session, not the resident window"
    );
    // (3) The scan counted entries the window does not hold: the prefix was
    // STREAMED, and the prefix counter shows the read came from disk.
    assert!(
        paged_stats.entries > entries_before,
        "the scan counted only the window: {} entries vs {} resident",
        paged_stats.entries,
        entries_before
    );
    let prefix_visited = manager
        .for_each_prefix_entry(|_| {})
        .expect("prefix stream");
    assert!(
        prefix_visited > 0,
        "the prefix counter must show the prefix was streamed, not materialized"
    );
    assert_eq!(
        prefix_visited - 1 + manager.entries().len(),
        paged_stats.entries,
        "the streamed prefix's non-header entries plus the resident entries are exactly the scanned set"
    );
    // The header accounting `TreeScanStats::entries` documents: on a paged
    // manager `resident_entries()` is `[prepended file header] + window`, so
    // the whole-session counter is compared with the resident NON-HEADER
    // count (`resident_entries().len() - 1`), not the raw list length.
    assert_eq!(
        manager.resident_entries().len(),
        entries_before + 1,
        "the resident list carries the prepended file header"
    );
    assert!(
        paged_stats.entries > manager.resident_entries().len() - 1,
        "the whole-session counter must exceed the resident non-header count: {} vs {}",
        paged_stats.entries,
        manager.resident_entries().len() - 1
    );
    // (4) Nothing was materialized: the manager is still the same window.
    assert!(manager.is_paged(), "the scan never widens the session");
    assert_eq!(
        manager.entries().len(),
        entries_before,
        "the scan materialized nothing"
    );
    assert!(manager
        .paged_window()
        .is_some_and(|window| window.page_start > 0));

    // The labelled-prefix-entry case, explicitly: the streamed label change is
    // resolved (text and timestamp) by the paged scan, and the row's entry is
    // NOT resident — only the streamed metadata made it readable.
    let labeled = paged_rows
        .iter()
        .find(|row| row.label.as_deref() == Some("checkpoint"))
        .expect("the prefix entry's label must be resolved by the paged scan");
    assert!(
        labeled.label_timestamp.is_some(),
        "the label-change timestamp must ride the streamed metadata"
    );
    assert!(
        manager.entry(&labeled.id).is_none(),
        "the labeled row is a PREFIX entry, absent from the resident window"
    );

    // The window's oldest entry keeps its PREFIX parent — the trap turned it
    // into a root, which silently detached the resident window from the tree.
    let oldest = manager
        .resident_entries()
        .iter()
        .find(|entry| !entry.is_header() && entry.parent_id().is_some())
        .expect("the fixture's window starts mid-session");
    let oldest_row = paged_rows
        .iter()
        .find(|row| row.id == oldest.entry_id())
        .expect("the oldest resident entry is a row");
    assert_eq!(
        oldest_row.parent_id.as_deref(),
        oldest.parent_id(),
        "the window's oldest entry must keep its streamed prefix parent"
    );
    assert!(oldest_row.depth > 0, "it hangs off a prefix entry");
}

/// The invalid-data shapes, split across a paged manager's page boundary: a
/// duplicate id (the LAST occurrence's facts win, like `by_id` / `tree()`) and
/// a parent cycle (unreachable from any root, never listed — and the resident
/// half must not become a root). The full load and the paged scan must agree
/// exactly, rows and stats.
#[test]
fn tree_scan_handles_duplicate_ids_and_cycles_across_the_page() {
    let fixture = pathological_fixture();
    // The caps come from the fixture's own bytes: the window is exactly the
    // file's last four entries, so the page boundary splits both shapes.
    let text = std::fs::read_to_string(&fixture.path).expect("read fixture");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 9, "the header plus eight entries");
    let tail_four_bytes: usize = lines[lines.len() - 4..]
        .iter()
        .map(|line| line.len() + 1)
        .sum();
    let manager = SessionManager::open_paged(&fixture.path, tail_four_bytes + 1, 4)
        .expect("open pathological paged");
    assert!(manager.is_paged(), "the fixture must exceed the caps");
    assert_eq!(
        manager.resident_entries().len(),
        5,
        "the window is the header plus the last four entries"
    );
    assert!(
        manager.entry("c0000001").is_none(),
        "cycle A must be in the skipped prefix"
    );
    assert!(
        manager.entry("c0000002").is_some(),
        "cycle B must be resident"
    );
    assert!(
        manager.entry("d0000000").is_some(),
        "the duplicate id's LAST occurrence must be resident"
    );

    let full = SessionManager::open(&fixture.path).expect("full open");
    let mut full_rows: Vec<TreeRowMeta> = Vec::new();
    let full_stats = full
        .for_each_tree_row(|row| full_rows.push(row.clone()))
        .expect("full scan");
    let mut paged_rows: Vec<TreeRowMeta> = Vec::new();
    let paged_stats = manager
        .for_each_tree_row(|row| paged_rows.push(row.clone()))
        .expect("paged scan");

    assert_eq!(
        paged_rows, full_rows,
        "a paged scan must equal the full load on duplicate ids and cycles"
    );
    assert_eq!(paged_stats, full_stats, "stats included");
    assert_eq!(
        paged_rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["d0000000", "d0000001", "d0000002", "d0000003", "d0000004"],
        "the root chain only: the duplicate id is visited once, the cycle never"
    );
    assert_eq!(
        paged_stats.entries, 8,
        "all eight non-header entries are the scanned set"
    );
    assert_eq!(paged_stats.rows, 5);
    assert_eq!(paged_stats.roots, 1);
    assert_eq!(paged_stats.max_depth, 4);
    assert!(
        !paged_rows.iter().any(|row| row.id.starts_with("c000000")),
        "entries unreachable from a root (the cycle) are never listed"
    );
    let duplicate = paged_rows.first().expect("the root row");
    assert_eq!(
        duplicate.parent_id, None,
        "the duplicate's FIRST occurrence is the root position (the tree walk)"
    );
    assert!(
        duplicate.preview.starts_with("duplicate occurrence"),
        "the row facts come from the duplicate's LAST occurrence, like by_id: {}",
        duplicate.preview
    );
    assert!(
        paged_stats.entries > manager.entries().len(),
        "the scan counted the streamed prefix, not just the window"
    );
    assert!(manager.is_paged(), "the scan never widens the session");
}

// ---------------------------------------------------------------------------
// Legacy sessions: the tail-first open falls back to the full loader
// ---------------------------------------------------------------------------

#[test]
fn tail_first_open_falls_back_for_a_legacy_session() {
    let dir = std::env::temp_dir().join(format!(
        "pir-paged-consumers-legacy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let mut manager = SessionManager::create("C:\\repo", &dir).expect("create session");
    let path = manager.path().to_path_buf();
    for index in 0..40 {
        manager
            .append_message(user_message(format!("legacy {index}")))
            .expect("append");
    }
    // A v2 header makes the paged open REFUSE (the migration rewrite guard);
    // the host-level tail-first open must fall back to the full loader, which
    // migrates in place — the pre-paging behavior.
    let contents = std::fs::read_to_string(&path).expect("read fixture");
    let legacy = contents.replacen("\"version\":3", "\"version\":2", 1);
    assert_ne!(legacy, contents);
    std::fs::write(&path, &legacy).expect("write v2 fixture");

    let opened = open_session_tail_first(&path).expect("tail-first open falls back");
    assert!(!opened.is_paged(), "a legacy session opens fully");
    assert_eq!(opened.entries().len(), 40);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// RPC: attach pages a huge session; every whole-session read widens
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

struct NullProvider;

impl Provider for NullProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: Vec::new(),
                stop_reason: StopReason::Stop,
                timestamp: 0,
                ..Default::default()
            },
        ));
        stream
    }
}

/// The factory records whether the manager it was handed was PAGED, so the
/// test proves `attach` opened tail-first (not just that the reads answer).
fn recording_factory(observed: Arc<AtomicUsize>) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        let paged = session.as_ref().is_some_and(SessionManager::is_paged);
        observed.store(paged as usize, Ordering::SeqCst);
        let mut registry = ProviderRegistry::default();
        registry.set_provider(Arc::new(NullProvider));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: Arc::new(registry),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

struct RpcClient {
    writer: tokio::sync::mpsc::UnboundedSender<String>,
    reader: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl RpcClient {
    fn send(&self, value: serde_json::Value) {
        self.writer
            .send(serde_json::to_string(&value).expect("serialize"))
            .expect("send command");
    }

    async fn wait_for(
        &mut self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..600 {
            let line = tokio::time::timeout(Duration::from_secs(30), self.reader.recv())
                .await
                .expect("read must not time out")
                .expect("protocol line");
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("rpc line must be valid JSON");
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected protocol line never arrived");
    }

    /// [`Self::wait_for`] without parsing: for responses `serde_json` refuses
    /// to PARSE (its 128-level nesting limit rejects the nested `TreeNode`
    /// chain of a long session; see the fixture note).
    async fn wait_for_raw(&mut self, predicate: impl Fn(&str) -> bool) -> String {
        for _ in 0..600 {
            let line = tokio::time::timeout(Duration::from_secs(30), self.reader.recv())
                .await
                .expect("read must not time out")
                .expect("protocol line");
            if predicate(&line) {
                return line;
            }
        }
        panic!("expected protocol line never arrived");
    }
}

/// Attach the fixture session over a fresh in-process RPC server; returns the
/// client and whether the factory saw a PAGED manager.
async fn attach_paged_session(
    session_dir: &Path,
    session_path: &Path,
    observed: Arc<AtomicUsize>,
) -> RpcClient {
    let server = RpcServer::new(session_dir.to_path_buf(), recording_factory(observed));
    let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    tokio::spawn(async move {
        let _ = server.serve(server_read, server_write).await;
    });
    let (client_read, mut client_write) = tokio::io::split(client_stream);
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if client_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
        }
    });
    let mut client = RpcClient {
        writer: write_tx,
        reader: line_rx,
    };
    client.send(serde_json::json!({
        "id": "attach",
        "type": "attach",
        "session": session_path.display().to_string(),
    }));
    let attached = client
        .wait_for(|value| value["command"] == "attach" && value["id"] == "attach")
        .await;
    assert_eq!(attached["success"], serde_json::json!(true), "{attached}");
    client
}

#[tokio::test]
async fn rpc_whole_session_reads_answer_with_the_full_history() {
    let fixture = fixture("rpc");
    let full = SessionManager::open(&fixture.path).expect("full open");
    let full_entry_count = full.entries().len();
    // The host default byte cap (1 MiB) is what pages this fixture.
    assert!(
        std::fs::metadata(&fixture.path)
            .map(|meta| meta.len() as usize)
            .unwrap_or(0)
            > pir::session_manager::DEFAULT_TAIL_PAGE_BYTES,
        "the fixture must exceed the default page byte cap"
    );
    let full_messages = full.build_context_entries().expect("full projection");

    // One fresh server + attach per read command: each command must widen ON
    // ITS OWN (a shared session would let the first command's widening carry
    // the rest).
    let commands: [&str; 5] = [
        "get_entries",
        "get_tree",
        "get_fork_messages",
        "get_messages",
        "get_session_stats",
    ];
    for command in commands {
        let observed = Arc::new(AtomicUsize::new(0));
        let mut client =
            attach_paged_session(&fixture.dir, &fixture.path, Arc::clone(&observed)).await;
        assert_eq!(
            observed.load(Ordering::SeqCst),
            1,
            "attach must open the huge session tail-first"
        );
        client.send(serde_json::json!({ "id": "read", "type": command }));
        if command == "get_tree" {
            // `get_tree`'s payload is a `TreeNode` chain nested one level per
            // entry, and serde_json REFUSES to parse a value nested past its
            // 128-level recursion limit — so the response is checked on the
            // RAW line instead (a prefix-only entry id must be in it). The
            // nesting itself is a reported session_manager finding: a real
            // session reaches depths where serializing/dropping the chain
            // overflows a thread stack (see the fixture note).
            let line = client
                .wait_for_raw(|line| line.contains("\"command\":\"get_tree\""))
                .await;
            assert!(
                line.contains("\"success\":true"),
                "get_tree must succeed: {line}"
            );
            assert!(
                line.contains(&fixture.prefix_entry_id),
                "get_tree answered a tail window (no prefix entry in the payload)"
            );
            continue;
        }
        let response = client
            .wait_for(|value| value["command"] == command && value["id"] == "read")
            .await;
        assert_eq!(response["success"], serde_json::json!(true), "{response}");
        match command {
            "get_entries" => {
                let entries = response["data"]["entries"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{response}"));
                assert_eq!(
                    entries.len(),
                    full_entry_count,
                    "get_entries answered a tail window"
                );
                assert_eq!(
                    response["data"]["leafId"],
                    serde_json::json!(full.leaf_id())
                );
            }
            "get_tree" => unreachable!("handled on the raw line above"),
            "get_fork_messages" => {
                let expected = full
                    .build_context_entries_with_ids()
                    .expect("projection with ids")
                    .into_iter()
                    .filter_map(|(entry_id, message)| {
                        let entry_id = entry_id?;
                        let Message::User(user) = message else {
                            return None;
                        };
                        let text = user
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text(text) => Some(text.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Some(serde_json::json!({ "entryId": entry_id, "text": text }))
                    })
                    .collect::<Vec<serde_json::Value>>();
                assert_eq!(
                    response["data"]["messages"],
                    serde_json::Value::Array(expected),
                    "get_fork_messages answered a tail window"
                );
            }
            "get_messages" => {
                let expected = serde_json::to_value(&full_messages).expect("messages json");
                assert_eq!(
                    response["data"]["messages"], expected,
                    "get_messages answered a tail window"
                );
            }
            "get_session_stats" => {
                // `totalMessages` counts every message entry of the WHOLE
                // session (not the resident window, and not label entries).
                let expected_messages = full
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry, pir::session_manager::FileEntry::Message(_)))
                    .count() as u64;
                assert_eq!(
                    response["data"]["totalMessages"].as_u64(),
                    Some(expected_messages),
                    "get_session_stats counted only the window: {response}"
                );
            }
            other => panic!("unhandled command {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// P2: the prefix streamer has an over-long-line cap
// ---------------------------------------------------------------------------

/// `for_each_prefix_entry` tolerates unknown/unparseable lines by design, so
/// without a cap a single corrupt line grows `pending` without bound (the
/// paged-compaction sibling already capped the identical loop). The cap is a
/// public constant, so the fixture writes ONE sparse line just past it
/// instead of hundreds of MiB of real data.
#[test]
fn prefix_stream_refuses_an_over_long_entry_line_instead_of_allocating() {
    use std::io::{Seek as _, SeekFrom, Write as _};

    let dir = std::env::temp_dir().join(format!(
        "pir-paged-giant-line-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("giant-line.jsonl");
    let mut file = std::fs::File::create(&path).expect("create session");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "giant-line",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "C:\\repo",
        })
    )
    .expect("header");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "message", "id": "u0", "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {
                "role": "user", "timestamp": 0,
                "content": [{"type": "text", "text": "ordinary entry"}],
            },
        })
    )
    .expect("ordinary entry");

    // ONE unterminated line longer than the cap: a sparse run of NUL bytes
    // (unknown lines are tolerated by design) closed by a real newline.
    let giant_start = file.stream_position().expect("position");
    let giant_len = MAX_PREFIX_ENTRY_LINE_BYTES as u64 + 4096;
    assert!(
        giant_len > MAX_PREFIX_ENTRY_LINE_BYTES as u64,
        "the fixture line must exceed {MAX_PREFIX_ENTRY_LINE_BYTES}"
    );
    file.set_len(giant_start + giant_len + 1)
        .expect("extend file");
    file.seek(SeekFrom::Start(giant_start + giant_len))
        .expect("seek to the line end");
    file.write_all(b"\n").expect("terminate the giant line");

    // A small resident tail after the giant line, so the tail page (and thus
    // the skipped prefix) has a deterministic boundary.
    let mut parent = String::new();
    for index in 0..8 {
        let id = format!("tail-{index}");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": id,
                "parentId": if parent.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(parent.clone()) },
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "user", "timestamp": 0,
                    "content": [{"type": "text", "text": format!("tail {index}")}],
                },
            })
        )
        .expect("tail entry");
        parent = id;
    }
    drop(file);

    let manager = SessionManager::open_paged(&path, 2 * 1024, 8).expect("paged open");
    assert!(manager.is_paged(), "the fixture must exceed the page caps");
    let error = manager
        .for_each_prefix_entry(|_| {})
        .expect_err("an over-long prefix line must fail cleanly, not allocate");
    assert!(
        error.to_string().contains("over-long entry line"),
        "the error names the over-long line: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
