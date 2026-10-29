//! P0.3 — TUI load-more: scrolling up in a tail-window session streams the
//! skipped prefix off disk page by page.
//!
//! `SessionManager::open_paged` keeps only a byte/entry-capped tail window
//! resident; the TUI's windowed transcript materializes a band inside it.
//! Scrolling to the materialized top used to be a dead end. Now the worker's
//! `LoadOlder` command streams one older page (`read_older_page` →
//! `older_transcript_messages`), and the UI prepends it with every live index
//! shifted (`prepend_older_entries`), so:
//!
//! (i) the prepend order is oldest-chunk-first and the concatenated view is
//!     the SAME entry list a full load would replay;
//! (ii) the painted view does not jump (the materialized slice is shifted, not
//!      rebuilt; the offset-from-end anchor is invariant);
//! (iii) the file start is a terminal state (`read_older_page` → `None`), not
//!      a spin — the UI's trigger asks for a page only while a cursor exists;
//! (iv) a non-paged session has no cursor, so the trigger never fires.

use std::sync::{Arc, Mutex};

use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::session_manager::SessionManager;
use pir::tui::altscreen_search::max_scroll_offset;
use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::core::{Component, Rows, Terminal, Text, TuiCore};
use pir::tui::terminal::{
    prepend_older_entries, should_load_older, transcript_entries_from_messages, TranscriptWindow,
};
use pir::tui::theme::{with_theme, ThemeColor};

// ---------------------------------------------------------------------------
// Theme lock (the g_tui suites' per-binary analogue; rendering reads the
// global theme).
// ---------------------------------------------------------------------------

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn warm_theme() {
    with_theme(|theme| {
        let _ = theme.fg(ThemeColor::Text, "warm");
    });
}

// ---------------------------------------------------------------------------
// Fixture: a session file whose tail window is far below the whole session
// ---------------------------------------------------------------------------

const FIXTURE_TURNS: usize = 120;
const TURN_BYTES: usize = 2048;
const PAGE_BYTES: usize = 24 * 1024;
const PAGE_ENTRIES: usize = 24;

/// Marker unique to turn `index` (the reachability probe).
fn marker(index: usize) -> String {
    format!("LOADMARK{index:05}")
}

fn turn_text(index: usize) -> String {
    let head = format!("turn {index:05} {}\n", marker(index));
    let mut text = head;
    while text.len() + 40 < TURN_BYTES {
        text.push_str("paged load-more payload line\n");
    }
    text
}

struct Fixture {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "pir-tui-load-more-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let mut manager = SessionManager::create("C:\\repo", &dir).expect("create session");
    let path = manager.path().to_path_buf();
    for index in 0..FIXTURE_TURNS {
        manager
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: turn_text(index),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }))
            .expect("append turn");
    }
    Fixture { dir, path }
}

fn paged(fixture: &Fixture) -> SessionManager {
    let manager = SessionManager::open_paged(&fixture.path, PAGE_BYTES, PAGE_ENTRIES)
        .expect("open fixture paged");
    assert!(
        manager.is_paged(),
        "fixture must exceed the page caps ({} resident entries)",
        manager.resident_entries().len()
    );
    manager
}

/// The messages of one paged page, in file order, converted exactly like the
/// worker's `LoadOlder` handler.
fn page_entries(page: &pir::session_manager::SessionTailPage) -> Vec<ChatEntry> {
    let messages: Vec<Message> = page
        .entries()
        .iter()
        .flat_map(|entry| {
            pir::session_manager::projection::session_entry_to_context_messages(entry)
        })
        .collect();
    transcript_entries_from_messages(messages)
}

fn user_texts(entries: &[ChatEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::User(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// (i) prepend order + (iii) file-start terminal state + (iv) full equality
// ---------------------------------------------------------------------------

#[test]
fn paged_walk_reconstructs_the_full_entry_list_and_stops_at_the_file_start() {
    let fixture = fixture("walk");
    let full = SessionManager::open(&fixture.path).expect("full open");
    let expected =
        transcript_entries_from_messages(full.build_context_entries().expect("full projection"));
    let manager = paged(&fixture);

    // The resident replay (what the worker sends at attach).
    let mut entries = transcript_entries_from_messages(
        manager.build_context_entries().expect("window projection"),
    );
    assert!(
        entries.len() < expected.len(),
        "sanity: the resident replay is the tail window"
    );

    // Scroll-up paging: each step goes strictly older; the chunk is prepended
    // in front of the already-loaded view.
    let mut cursor = manager.prefix_cursor().expect("paged cursor");
    // ONE window across the whole walk: its start must accumulate every
    // inserted page, exercising the prepend on a live (already-shifted)
    // window rather than resetting to zero each iteration.
    let mut window = TranscriptWindow::new();
    let mut chunks = 0usize;
    let mut inserted_total = 0usize;
    while let Some(page) = manager
        .read_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read older page")
    {
        chunks += 1;
        cursor = page.page_start();
        let chunk = page_entries(&page);
        assert!(!chunk.is_empty());
        let mut streaming = None;
        let mut tools = std::collections::HashMap::new();
        let inserted =
            prepend_older_entries(&mut entries, chunk, &mut window, &mut streaming, &mut tools);
        inserted_total += inserted;
        // The prepend keeps the materialized slice bit-identical: the window
        // start moved by exactly the cumulative inserted count.
        assert_eq!(window.start(), inserted_total);
    }
    // (iii) `None` is the file start: the cursor walk terminates, and asking
    // again with cursor 0 keeps answering `None` (no spin).
    assert!(chunks > 1, "the fixture needs several pages: {chunks}");
    assert!(inserted_total > 0);
    assert_eq!(cursor, 0);
    assert!(
        manager
            .read_older_page(0, PAGE_BYTES, PAGE_ENTRIES)
            .expect("read at the start")
            .is_none(),
        "the file start is a terminal state"
    );

    // (iv-a) the concatenated view equals a full load's entry list.
    assert_eq!(user_texts(&entries), user_texts(&expected));
    assert_eq!(entries.len(), expected.len());
    // Oldest chunk first: the first entry is the session's first turn.
    assert!(user_texts(&entries)[0].starts_with("turn 00000 "));

    // The one-time "start of session" marker the UI prepends when a load
    // reports `None`: it lands at the OLDEST position (where the user is
    // looking) and shifts the window exactly like a loaded chunk, so the
    // painted view does not move.
    let mut marker_window = TranscriptWindow::new();
    marker_window.shift_start(3);
    let mut streaming = None;
    let mut tools = std::collections::HashMap::new();
    let marked = prepend_older_entries(
        &mut entries,
        vec![ChatEntry::Notice {
            kind: pir::tui::app::NoticeKind::Status,
            text: "Start of session (no older history on disk)".to_owned(),
        }],
        &mut marker_window,
        &mut streaming,
        &mut tools,
    );
    assert_eq!(marked, 1);
    assert_eq!(marker_window.start(), 4);
    assert!(matches!(entries.first(), Some(ChatEntry::Notice { .. })));
    // The user turns are the unchanged slice the marker was prepended to.
    assert_eq!(user_texts(&entries), user_texts(&expected));
}

/// A session that fits the page caps has no cursor: the trigger can never
/// fire, so the view behaves exactly as before load-more existed.
#[test]
fn non_paged_sessions_offer_no_cursor() {
    let dir = std::env::temp_dir().join(format!(
        "pir-tui-load-more-small-{}-{}",
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
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "one".to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        }))
        .expect("append");
    let opened = SessionManager::open_paged(&path, PAGE_BYTES, PAGE_ENTRIES).expect("open");
    assert!(!opened.is_paged());
    assert_eq!(opened.prefix_cursor(), None);
    assert!(opened
        .read_older_page(0, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read")
        .is_none());
    assert!(!should_load_older(
        true,
        true,
        0,
        0,
        20,
        opened.prefix_cursor().is_some(),
        false
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Pure seams
// ---------------------------------------------------------------------------

#[test]
fn prepend_older_entries_shifts_every_live_index() {
    let mut entries = vec![ChatEntry::User("newer".to_owned())];
    let mut window = TranscriptWindow::new();
    let mut streaming = Some(0usize);
    let mut tools = std::collections::HashMap::from([("call".to_owned(), 0usize)]);
    let inserted = prepend_older_entries(
        &mut entries,
        vec![
            ChatEntry::User("oldest".to_owned()),
            ChatEntry::User("older".to_owned()),
        ],
        &mut window,
        &mut streaming,
        &mut tools,
    );
    assert_eq!(inserted, 2);
    assert_eq!(
        user_texts(&entries),
        vec!["oldest".to_owned(), "older".to_owned(), "newer".to_owned()]
    );
    assert_eq!(window.start(), 2);
    assert_eq!(streaming, Some(2));
    assert_eq!(tools.get("call"), Some(&2));

    // An empty chunk is a no-op (no index churn).
    let inserted = prepend_older_entries(
        &mut entries,
        Vec::new(),
        &mut window,
        &mut streaming,
        &mut tools,
    );
    assert_eq!(inserted, 0);
    assert_eq!(window.start(), 2);
}

#[test]
fn should_load_older_fires_only_at_the_materialized_top() {
    // At the top of a scrolled, windowed view with a cursor: load.
    assert!(should_load_older(true, true, 0, 0, 20, true, false));
    // Everything else is a no-op:
    // not windowed / not scrolled / window not at entry 0 / no cursor /
    // request already in flight / the view is far below the band top.
    assert!(!should_load_older(false, true, 0, 0, 20, true, false));
    assert!(!should_load_older(true, false, 0, 0, 20, true, false));
    assert!(!should_load_older(true, true, 5, 0, 20, true, false));
    assert!(!should_load_older(true, true, 0, 0, 20, false, false));
    assert!(!should_load_older(true, true, 0, 0, 20, true, true));
    assert!(!should_load_older(true, true, 0, 1000, 20, true, false));
}

// ---------------------------------------------------------------------------
// (ii) the anchor: the painted view does not jump when a chunk is prepended
// ---------------------------------------------------------------------------

/// Test-local component wrapper around the live chat container (the production
/// wrapper is private to `terminal.rs`).
struct WindowedChat(Arc<Mutex<ChatContainer>>);

impl Component for WindowedChat {
    fn render(&self, width: usize) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render_into(width, out)
    }

    fn invalidate(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate();
    }
}

struct Harness {
    core: TuiCore,
    chat: Arc<Mutex<ChatContainer>>,
    window: TranscriptWindow,
}

impl Harness {
    fn new(width: usize, height: usize, dock_rows: usize) -> Self {
        let terminal = Terminal::new();
        terminal.set_size(width, height);
        let mut core = TuiCore::new(terminal);
        core.set_windowed_viewport(true);
        let chat = Arc::new(Mutex::new(ChatContainer::new()));
        core.push_content(Box::new(WindowedChat(Arc::clone(&chat))));
        let dock: Vec<String> = (0..dock_rows)
            .map(|row| {
                if row == 0 {
                    "EDITOR row".to_string()
                } else {
                    format!("FOOTER row {row}")
                }
            })
            .collect();
        core.push_content(Box::new(Text::with_padding(dock.join("\n"), 0, 0)));
        core.set_dock_height(dock_rows);
        Self {
            core,
            chat,
            window: TranscriptWindow::new(),
        }
    }

    /// One loop iteration: plan the window, materialize the slice, re-anchor,
    /// render (the production loop's windowed path).
    fn step(&mut self, entries: &[ChatEntry]) {
        let moved = self.window.sync(entries, &self.core);
        self.chat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_entries(self.window.slice(entries));
        if moved {
            self.core.resync_window();
        }
        self.core.render();
    }

    fn transcript_top(&self) -> usize {
        self.window.start()
    }

    fn max_offset(&self) -> usize {
        max_scroll_offset(
            self.core.frame_buffer_length(),
            self.core.dock_height(),
            self.core.rows(),
        )
    }
}

#[test]
fn scroll_up_load_more_keeps_the_anchor_and_reveals_older_entries() {
    let _guard = test_lock();
    warm_theme();
    let fixture = fixture("anchor");
    let manager = paged(&fixture);
    let mut entries = transcript_entries_from_messages(
        manager.build_context_entries().expect("window projection"),
    );
    assert!(entries.len() < FIXTURE_TURNS);

    let mut harness = Harness::new(60, 24, 3);
    harness.step(&entries);
    // Scroll all the way up: the oldest turn of the resident window is on
    // screen, and the window manager has already materialized the whole
    // resident list (`start() == 0`).
    let max = harness.max_offset();
    assert!(max > 0, "the fixture band must be scrollable");
    harness.core.set_scroll_offset(max);
    harness.step(&entries);
    assert_eq!(harness.transcript_top(), 0);
    let resident_oldest = entries
        .first()
        .and_then(|entry| match entry {
            ChatEntry::User(text) => Some(text.clone()),
            _ => None,
        })
        .expect("resident oldest turn")
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let painted_before = harness.core.last_screen_lines();
    assert!(
        painted_before
            .iter()
            .any(|line| line.contains(&resident_oldest)),
        "sanity: the resident top is visible"
    );

    // Load the next older page (the worker's LoadOlder reply) and prepend it.
    let cursor = manager.prefix_cursor().expect("paged cursor");
    let page = manager
        .read_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read page")
        .expect("older page");
    let chunk = page_entries(&page);
    assert!(!chunk.is_empty());
    // The OLDEST turn of the loaded chunk (its first rendered line): after
    // scrolling to the new top it must be on screen.
    let oldest_loaded = chunk
        .first()
        .and_then(|entry| match entry {
            ChatEntry::User(text) => Some(text.clone()),
            _ => None,
        })
        .expect("loaded chunk starts with a user turn")
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut streaming = None;
    let mut tools = std::collections::HashMap::new();
    let inserted = prepend_older_entries(
        &mut entries,
        chunk,
        &mut harness.window,
        &mut streaming,
        &mut tools,
    );
    assert!(inserted > 0);
    harness.step(&entries);

    // (ii) The painted screen is bit-identical: the view did not jump.
    assert_eq!(
        harness.core.last_screen_lines(),
        painted_before,
        "prepending older entries must not move the painted view"
    );

    // Scrolling further up now reaches the newly loaded page.
    let new_max = harness.max_offset();
    assert!(new_max > max, "the band grew by the loaded chunk");
    harness.core.set_scroll_offset(new_max);
    harness.step(&entries);
    assert!(
        harness
            .core
            .last_frame_lines()
            .iter()
            .any(|line| line.contains(&oldest_loaded)),
        "the oldest loaded turn must become visible after paging up"
    );
}
