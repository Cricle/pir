//! P0.3 follow-up — transcript search reaches the WHOLE history in the
//! windowed (huge-session) TUI.
//!
//! The windowed transcript materializes a bounded band (the tail budget plus
//! whatever scrolling pulled in); older pages stay on disk and the band is
//! only a window over the resident entries. Upstream searches the
//! ScrollView's whole content, so this suite pins the walk that closes the
//! gap: a query with no match in the materialized band
//!
//! (i) probes the resident prefix the window has NOT materialized (those
//!     entries are already in memory, and the disk cursor sits BELOW them —
//!     a match there is unreachable by paging),
//! (ii) then pages older history off disk one page at a time
//!     (`WorkerCommand::LoadOlder`, the same read the scroll-up load-more
//!     uses), materializing nothing until a page holds the query,
//! (iii) materializes the found page (holding the window's top so the window
//!     rules cannot unmaterialize it) and lets the ordinary band search +
//!     `scroll_to_match` reveal it,
//! (iv) stays bounded (page/byte budget) and cancellable: a live-typed query
//!     supersedes the walk, Esc aborts it, and a reply from a superseded read
//!     lands as a plain load-more chunk — it can never complete the live walk
//!     or move the selection,
//! (v) and leaves a regular (non-windowed) session on exactly the
//!     pre-existing path: nothing unmaterialized, nothing paged.
//!
//! The harness replays the loop's windowed path through the PRODUCTION
//! functions (`route_alt_screen_key`, `search_walk_step`, `apply_older_page`,
//! `sync_windowed_band`, `should_load_older`, `finish_search_walk_check`) and
//! a real paged [`SessionManager`] standing in for the worker; only the event
//! plumbing differs (a read's reply is DELIVERED LATER — the loop's worker
//! channel is not a pollable input handle — and the harness has no streaming
//! transcript). That is what lets a keystroke land while a read is on the
//! wire, the supersession case.
//!
//! Query discipline (load-bearing, not decoration). A marker query is typed
//! one character at a time, and the router reveals the closest match per
//! keystroke (upstream `refreshSearch`) — so a fixture string containing even
//! the query's FIRST character scrolls the view before the walk starts. To
//! keep "the walk is the first thing that scrolls this view" a real
//! precondition:
//!
//! - the quietly-typed markers ([`MARKER_OLDEST`], [`MARKER_MISSING`]) start
//!   with `QQ`, and NO other string the band can materialize contains `q` —
//!   including [`MARKER_PAGE`] once a walk has loaded its page;
//! - [`MARKER_PAGE`] starts with `ZZ` and is typed only while the band holds
//!   no `z` (the fixture text, [`MARKER_NEWEST`] included, is otherwise
//!   `z`-free);
//! - the payload filler is free of both letters.
//!
//! [`Harness::type_quiet`] asserts the precondition after EVERY keystroke and
//! names the offending prefix, so a fixture change fails there, loudly,
//! instead of one screen later. The one consequence, documented rather than
//! hidden: a query that matches the materialized band *while* being typed can
//! only be exercised where the test wants exactly that
//! (`regular_sessions_keep_the_pre_existing_search_path` and
//! `a_match_in_the_band_settles_without_paging`, which assert the walk never
//! starts rather than where the viewport sits).

//! The resident-prefix step's second half is asserted here, not hand-traced.
//! `resident_prefix_is_probed_before_the_disk` drives that state — the match
//! resident but unmaterialized — through the planner contract
//! (`plan_search_walk` ⇒ `ProbeResident`), the probe slice, the production
//! probe's verdict on it, the materialization, the post-render band check and
//! the reveal (a selected match, the viewport off the live end, the match line
//! on screen, the band top held, and no disk read anywhere in that run).
//! `older_page_match_is_found_and_revealed` asserts the identical chain when
//! the match arrives off disk.
//!
//! The arming rule is single-sourced (`SearchWalkStep::arms_band_check`) and
//! consumed by both the production loop and this harness, so the two cannot
//! disagree about WHICH step needs the band check — but this harness runs its
//! OWN glue (`Harness::step`), so a production-only regression in the call site
//! (`terminal.rs`'s walk block, where the predicate is read) is caught by the
//! TUI acceptance battery, not here. That split is the honest residual.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
use pir::session_manager::SessionManager;
use pir::tui::altscreen_search::{
    self, AltScreenState, SearchState, SearchWalk, SearchWalkOutcome, SearchWalkPlan,
    SEARCH_WALK_MAX_PAGES,
};
use pir::tui::ansi::strip_ansi;
use pir::tui::app::{ChatContainer, ChatEntry};
use pir::tui::core::{Component, Rows, Terminal, Text, TuiCore};
use pir::tui::terminal::{
    apply_older_page, probe_older_page, resident_probe_slice, search_walk_step, should_load_older,
    sync_windowed_band, tail_window_start, OlderPageEffect, OlderPages, SearchWalkStep,
    TranscriptWindow, TRANSCRIPT_TAIL_BYTES, TRANSCRIPT_WINDOW_MAX_ENTRIES,
};
use pir::tui::theme::{with_theme, ThemeColor};

// ---------------------------------------------------------------------------
// Theme lock (the per-binary analogue of the g_tui suites' lock: rendering
// reads the global theme)
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
// Fixture: a paged session whose markers live in known turns
// ---------------------------------------------------------------------------
//
// The fixture is deliberately LARGE enough that the windowed transcript really
// is a window: the tail budget stops at 1 MiB (`tail_window_start`), even when
// that is fewer than the 64-entry soft minimum, so a resident history of >16
// KiB turns leaves the oldest turns UNMATERIALIZED — the resident prefix the
// walk must probe (and the state a huge session is in). Every turn renders to
// roughly `TURN_BYTES / 200` rows (payload lines are padded to one rendered
// row under the harness's 200-column terminal), so the suite's row cost tracks
// its byte cost.

const FIXTURE_TURNS: usize = 72;
/// 72 turns × 16 KiB ≈ 1.15 MB of payload: still over the 1 MiB tail budget,
/// so `start > 0` and the oldest turns stay unmaterialized once everything is
/// resident. The byte budget, not the 64-entry soft minimum, decides where.
/// (16 KiB is the smallest turn that keeps the fixture over the 1 MiB tail
/// budget; it renders ~2/3 of the rows the previous 24 KiB turn did.)
const TURN_BYTES: usize = 16 * 1024;
const PAGE_BYTES: usize = 256 * 1024;
/// Turns per page (both the paged open's resident window and each
/// `read_older_page`): 72 turns = 9 pages, the resident tail = the newest
/// page, so the oldest marker sits 8 pages down — inside the walk's 16-page
/// budget, and well beyond the materialized band.
const PAGE_TURNS: usize = 8;

/// Only in turn 0 (the oldest turn).
///
/// The `QQ` prefix is the "quiet" marker prefix: `q` appears in NO other
/// fixture string that the band can materialize (payload, turn heads, the
/// tail marker, or the page marker below), so typing this query never matches
/// anything until the walk loads turn 0 — see [`Harness::type_quiet`].
const MARKER_OLDEST: &str = "QQMARK-OLDEST-ONLY";
/// Only in turn 57 — inside the FIRST page a walk reads off disk (turns
/// 56..63), which makes it the page a walk has on the wire when a keystroke
/// supersedes the walk. `ZZ`-prefixed instead of `QQ`: this marker IS in the
/// band (materialized by an earlier walk) when the `QQ` queries are typed, so
/// it must stay free of `q` — and its own `z` is safe because the band it is
/// typed against contains none.
const MARKER_PAGE: &str = "ZZMARK-PAGE-57";
/// Only in the newest turn (the resident/materialized tail). Free of BOTH `q`
/// and `z`: this line sits in the band while either marker discipline's query
/// is typed.
const MARKER_NEWEST: &str = "TAIL-NEWEST-MARK";
/// In no turn at all.
const MARKER_MISSING: &str = "QQMARK-NOT-ANYWHERE";

/// The one rendered line carrying [`MARKER_OLDEST`] (turn 0's head), used for
/// the SCREEN assertions: the search box paints the typed query, so
/// `screen().contains(MARKER_OLDEST)` could be satisfied by the box instead of
/// by the transcript row the test means.
fn oldest_line() -> String {
    format!("turn 00000 {MARKER_OLDEST}")
}

/// The one rendered line carrying [`MARKER_PAGE`] (turn 57's head), the first
/// page a walk reads off disk.
fn stale_page_line() -> String {
    format!("turn 00057 {MARKER_PAGE}")
}

/// The one rendered line carrying [`MARKER_NEWEST`]: the resident tail's
/// marker, in the newest turn.
fn newest_line() -> String {
    format!("turn {:05} {MARKER_NEWEST}", FIXTURE_TURNS - 1)
}

/// The line carrying [`MARKER_OLDEST`] in the small fixture.
fn small_line() -> String {
    format!("small session turn 0 {MARKER_OLDEST}")
}

/// One payload line, padded to 199 columns: under the harness's 200-column
/// terminal it renders as one row, so a turn's row count is its byte count over
/// the width instead of over the line length.
///
/// The filler is deliberately free of `q` and `z` — the characters the
/// quietly-typed marker queries start with — since it is the bulk of what the
/// band shows (see the module note and [`Harness::type_quiet`]).
fn payload_line() -> String {
    let mut line = String::from("payload filler line without the forbidden letter");
    while line.len() < 199 {
        line.push('.');
    }
    line.push('\n');
    line
}

fn turn_text(index: usize) -> String {
    let mut head = format!("turn {index:05}");
    if index == 0 {
        head.push(' ');
        head.push_str(MARKER_OLDEST);
    }
    if index + 1 == FIXTURE_TURNS {
        head.push(' ');
        head.push_str(MARKER_NEWEST);
    }
    head.push('\n');
    let mut text = head;
    let line = payload_line();
    while text.len() + line.len() < TURN_BYTES {
        text.push_str(&line);
    }
    text
}

fn fixture_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-tui-search-walk-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// A fixture plus the page caps it was opened with: the harness reads pages
/// with the SAME caps, so a fixture's "how many pages down is the marker" is
/// true of the reads the walk actually issues. The directory is removed by the
/// [`Harness`] it is handed to.
struct Fixture {
    manager: SessionManager,
    dir: std::path::PathBuf,
    page_bytes: usize,
    page_entries: usize,
}

/// A paged fixture: `turns` user messages of [`TURN_BYTES`] each, whose page
/// caps keep only `page_turns` turns resident (and are used for every
/// `read_older_page`).
fn paged_fixture(label: &str, turns: usize, page_turns: usize) -> Fixture {
    let dir = fixture_dir(label);
    let mut manager = SessionManager::create("C:\\repo", &dir).expect("create session");
    let path = manager.path().to_path_buf();
    for index in 0..turns {
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
    let manager = SessionManager::open_paged(&path, PAGE_BYTES, page_turns).expect("open paged");
    assert!(
        manager.is_paged(),
        "fixture must exceed the page caps ({} resident entries)",
        manager.resident_entries().len()
    );
    Fixture {
        manager,
        dir,
        page_bytes: PAGE_BYTES,
        page_entries: page_turns,
    }
}

/// A small session that fits any page cap: no paged cursor, the pre-window
/// shape.
fn small_fixture(label: &str) -> Fixture {
    let dir = fixture_dir(label);
    let mut manager = SessionManager::create("C:\\repo", &dir).expect("create session");
    let path = manager.path().to_path_buf();
    for index in 0..3 {
        manager
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: format!("small session turn {index} {MARKER_OLDEST}"),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }))
            .expect("append turn");
    }
    Fixture {
        manager: SessionManager::open(&path).expect("open small"),
        dir,
        page_bytes: PAGE_BYTES,
        page_entries: PAGE_TURNS,
    }
}

// ---------------------------------------------------------------------------
// Harness: the event loop's windowed path over the production functions
// ---------------------------------------------------------------------------

/// Test-local component wrapper around the live chat container (the
/// production wrapper is private to `terminal.rs`).
struct HarnessChat(Arc<Mutex<ChatContainer>>);

impl Component for HarnessChat {
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
            .render_into(width, out);
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
    alt: Arc<Mutex<AltScreenState>>,
    window: TranscriptWindow,
    entries: Vec<ChatEntry>,
    /// Live streaming/tool slots (empty here: the harness has no streaming
    /// transcript) — the production reply path takes them.
    streaming_entry: Option<usize>,
    tool_entries: HashMap<String, usize>,
    manager: SessionManager,
    /// The production load-more bookkeeping the shared paths read and write.
    pages: OlderPages,
    /// Cursors of reads requested but not yet delivered (the worker's reply
    /// channel: the loop drains it, it is not a pollable input handle).
    pending: Vec<u64>,
    /// Every `LoadOlder` the harness issued (observability).
    load_older_calls: usize,
    page_bytes: usize,
    page_entries: usize,
    probe: ChatContainer,
    /// A found page was materialized and awaits the post-render band check.
    walk_check: bool,
    /// The fixture directory (removed on drop).
    dir: std::path::PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    fn new(fixture: Fixture, windowed: bool) -> Self {
        let Fixture {
            manager,
            dir,
            page_bytes,
            page_entries,
        } = fixture;
        let terminal = Terminal::new();
        // Wide enough that the fixture's padded payload lines stay one row
        // each (see the fixture note), tall enough for the search box.
        terminal.set_size(200, 30);
        let mut core = TuiCore::new(terminal);
        core.set_windowed_viewport(windowed);
        let chat = Arc::new(Mutex::new(ChatContainer::new()));
        core.push_content(Box::new(HarnessChat(Arc::clone(&chat))));
        let dock: Vec<String> = (0..3).map(|row| format!("DOCK row {row}")).collect();
        core.push_content(Box::new(Text::with_padding(dock.join("\n"), 0, 0)));
        core.set_dock_height(3);
        // The search box / highlight decorator, exactly like the app installs
        // it (the state is shared with the harness, so the tests can drive the
        // router and read the painted frame).
        let alt: Arc<Mutex<AltScreenState>> = Arc::new(Mutex::new(AltScreenState::default()));
        let decorator_alt = Arc::clone(&alt);
        core.set_decorator(Some(Box::new(
            move |lines: &mut Vec<String>, context: altscreen_search::FrameDecorateContext| {
                let mut state = decorator_alt
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                altscreen_search::decorate_frame(
                    lines,
                    context,
                    &mut state,
                    &altscreen_search::AltScreenConfig::default(),
                );
            },
        )
            as Box<
                dyn Fn(&mut Vec<String>, altscreen_search::FrameDecorateContext) + Send,
            >));
        let entries = manager
            .build_context_entries()
            .map(pir::tui::terminal::transcript_entries_from_messages)
            .expect("resident projection");
        let pages = OlderPages {
            cursor: manager.prefix_cursor(),
            ..OlderPages::default()
        };
        Self {
            core,
            chat,
            alt,
            window: TranscriptWindow::new(),
            entries,
            streaming_entry: None,
            tool_entries: HashMap::new(),
            manager,
            pages,
            pending: Vec::new(),
            load_older_calls: 0,
            page_bytes,
            page_entries,
            probe: ChatContainer::new(),
            walk_check: false,
            dir,
        }
    }

    fn dock_height(&self) -> usize {
        3
    }

    fn search(&self) -> std::sync::MutexGuard<'_, AltScreenState> {
        self.alt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn key(&mut self, key: KeyEvent) {
        let dock_h = self.dock_height();
        let state = Arc::clone(&self.alt);
        let mut alt = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        altscreen_search::route_alt_screen_key(&key, &mut self.core, &mut alt, dock_h);
    }

    /// Whether the platform's `tui.altScreen.search` chord is ctrl+f
    /// (Windows / WSL) or ctrl+shift+f (elsewhere).
    fn search_toggle_key() -> KeyEvent {
        if pir::config::keybindings::use_windows_keybindings() {
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
        } else {
            KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )
        }
    }

    fn open_search(&mut self) {
        self.key(Self::search_toggle_key());
        assert!(self.search().search.active, "ctrl+f opens the search box");
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// Type `text` one key at a time and assert that NO intermediate prefix
    /// revealed anything — i.e. the viewport is still following the live end
    /// after every keystroke. The tests that need "the walk is the first thing
    /// that scrolls this view" call this: a fixture string that accidentally
    /// contains the query or one of its prefixes would otherwise scroll the
    /// view mid-typing and quietly invalidate every later assertion. The
    /// failure names the prefix, so a fixture change fails here.
    fn type_quiet(&mut self, text: &str) {
        for (index, c) in text.char_indices() {
            self.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            let prefix = &text[..index + c.len_utf8()];
            assert_eq!(
                self.scroll_offset(),
                0,
                "typing {prefix:?} revealed something: the fixture text must not \
                 contain the query or any of its prefixes"
            );
            assert!(
                self.core.is_following(),
                "typing {prefix:?} left the view off the live end"
            );
        }
    }

    /// The precondition the marker-query tests rely on: nothing has scrolled
    /// the viewport yet, so the walk is the only thing that can.
    fn assert_at_live_end(&self) {
        assert_eq!(self.scroll_offset(), 0, "precondition: no scrollback");
        assert!(
            self.core.is_following(),
            "precondition: the view follows the live end"
        );
    }

    fn press_esc(&mut self) {
        self.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    /// One loop iteration: the worker-event drain (one reply), the walk step
    /// (the loop runs it outside the dirty-frame block), then the windowed
    /// dirty block — search refresh, band sync, scroll-up load-more, render,
    /// post-render walk band check. Every mutation goes through a production
    /// function; only the control flow is the harness's.
    fn step(&mut self) {
        self.deliver_one();
        let dock_h = self.dock_height();
        let windowed = self.core.windowed_viewport();
        let (walking, holding) = {
            let alt = self.search();
            (alt.search.walk.is_walking(), alt.search.walk.holds_band())
        };
        if walking || holding {
            let step = {
                let state = Arc::clone(&self.alt);
                let mut alt = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if alt.search.walk.is_walking() && windowed {
                    search_walk_step(
                        &mut alt.search,
                        windowed,
                        &mut self.pages,
                        &self.entries,
                        &mut self.window,
                        &mut self.probe,
                        self.core.columns().max(1),
                        false,
                    )
                } else {
                    SearchWalkStep::Idle
                }
            };
            match step {
                SearchWalkStep::Idle => {}
                SearchWalkStep::Read { cursor } => {
                    self.load_older_calls += 1;
                    self.pending.push(cursor);
                }
                SearchWalkStep::Probed | SearchWalkStep::Materialized => {
                    // The arming rule is the step's own (`arms_band_check`),
                    // the same predicate the production loop consumes, so the
                    // harness cannot disagree with it about which step needs
                    // the post-render band check.
                    self.walk_check |= step.arms_band_check();
                }
            }
        }
        // Windowed dirty block.
        {
            let frame = self.core.last_frame_lines();
            let version = self.core.frame_version();
            let mut alt = self.search();
            altscreen_search::refresh_search(&mut alt, frame, dock_h, version);
        }
        if windowed {
            let state = Arc::clone(&self.alt);
            let mut alt = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sync_windowed_band(
                &self.entries,
                &mut self.window,
                &mut self.core,
                &mut alt,
                &self.chat,
            );
            drop(alt);
            // Scroll-up load-more (the production trigger; a walk read is on
            // the wire exactly when `pages.loading`, so the two never
            // overlap).
            let viewport_rows = self
                .core
                .rows()
                .max(1)
                .saturating_sub(self.core.dock_height())
                .max(1);
            let offset = self.core.scroll_offset();
            if should_load_older(
                true,
                offset > 0,
                self.window.start(),
                self.core.visible_top(),
                viewport_rows,
                self.pages.cursor.is_some() && !self.pages.failed,
                self.pages.loading,
            ) {
                if let Some(cursor) = self.pages.cursor {
                    self.pages.loading = true;
                    self.load_older_calls += 1;
                    self.pending.push(cursor);
                }
            }
        } else {
            self.chat
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_entries(&self.entries);
        }
        self.core.render();
        if self.walk_check {
            self.walk_check = false;
            let state = Arc::clone(&self.alt);
            let mut alt = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            altscreen_search::finish_search_walk_check(&mut self.core, &mut alt, dock_h);
        }
    }

    /// Deliver ONE pending page read if there is one (the loop's worker-event
    /// drain) and report whether one landed. The read is served by the real
    /// paged manager with the fixture's page caps and applied through
    /// [`apply_older_page`], the loop's own reply path.
    fn deliver_one(&mut self) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let cursor = self.pending.remove(0);
        let page = self
            .manager
            .read_older_page(cursor, self.page_bytes, self.page_entries)
            .expect("read older page");
        let (chunk, next_cursor) = match page {
            Some(page) => {
                let messages: Vec<Message> = page
                    .entries()
                    .iter()
                    .flat_map(|entry| {
                        pir::session_manager::projection::session_entry_to_context_messages(entry)
                    })
                    .collect();
                (
                    pir::tui::terminal::transcript_entries_from_messages(messages),
                    Some(page.page_start()),
                )
            }
            None => (Vec::new(), None),
        };
        let state = Arc::clone(&self.alt);
        let mut alt = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let effect = apply_older_page(
            &mut self.pages,
            chunk,
            next_cursor,
            false,
            &mut self.entries,
            &mut self.window,
            &mut self.streaming_entry,
            &mut self.tool_entries,
            &mut alt.search,
            &mut self.probe,
            self.core.columns().max(1),
            false,
        );
        drop(alt);
        if effect == OlderPageEffect::Materialized {
            self.walk_check = true;
        }
        true
    }

    /// Drive iterations until the walk settled (or `max` steps), plus one
    /// extra frame so the reveal is painted.
    fn settle(&mut self, max: usize) {
        for _ in 0..max {
            self.step();
            if !self.search().search.walk.is_walking() {
                break;
            }
        }
        self.step();
    }

    /// The painted screen with ANSI stripped.
    fn screen(&self) -> Vec<String> {
        self.core
            .last_screen_lines()
            .iter()
            .map(|line| strip_ansi(line))
            .collect()
    }

    /// The undecorated band frame (the search corpus, ANSI stripped).
    fn band(&self) -> Vec<String> {
        self.core
            .last_frame_lines()
            .iter()
            .map(|line| strip_ansi(line))
            .collect()
    }

    fn screen_contains(&self, needle: &str) -> bool {
        self.screen().iter().any(|line| line.contains(needle))
    }

    fn band_contains(&self, needle: &str) -> bool {
        self.band().iter().any(|line| line.contains(needle))
    }

    fn current_match_highlight(&self) -> bool {
        self.core
            .last_screen_lines()
            .iter()
            .any(|line| line.contains("\x1b[1;7m"))
    }

    fn scroll_offset(&self) -> usize {
        self.core.scroll_offset()
    }
}

// ---------------------------------------------------------------------------
// (i) a match only in an older page: found, revealed, on screen
// ---------------------------------------------------------------------------

#[test]
fn older_page_match_is_found_and_revealed() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("found", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();
    assert!(
        !h.band_contains(MARKER_OLDEST),
        "sanity: the oldest marker is far outside the materialized band"
    );

    h.assert_at_live_end();
    h.open_search();
    h.type_quiet(MARKER_OLDEST);
    // The query has no match anywhere the band can see, so nothing revealed
    // and the viewport still FOLLOWS the live end — the state the window hold
    // has to protect the found page from (a following tail window would trim
    // it straight back out).
    assert!(!h.band_contains(MARKER_OLDEST));
    assert_eq!(h.scroll_offset(), 0, "typing revealed nothing");
    // The first plan step puts the read on the wire; the walk's progress
    // label must be the honest ellipsis, not "No matches".
    h.step();
    assert_eq!(
        h.search().search.result_text(),
        "\u{2026}",
        "walking shows progress, not an empty state"
    );

    h.settle(64);
    let alt = h.search();
    assert_eq!(
        alt.search.walk.outcome(),
        Some(SearchWalkOutcome::Found),
        "the walk must find the oldest page"
    );
    assert!(
        !alt.search.matches.is_empty(),
        "the band now holds the match"
    );
    assert_eq!(
        alt.search.selected,
        Some(alt.search.matches.len() - 1),
        "the reveal targets the newest match of the walked-in page"
    );
    assert!(
        h.load_older_calls >= 8,
        "the oldest turn is 8 pages down: {} reads",
        h.load_older_calls
    );
    assert!(
        h.core.scroll_offset() > 0,
        "the reveal moved the viewport off the live end"
    );
    assert!(
        h.screen_contains(&oldest_line()),
        "the revealed match line must be on screen:\n{}",
        h.screen().join("\n")
    );
    assert!(
        h.current_match_highlight(),
        "the current match is painted with the search's current-match style"
    );
    // The hold: the found page is still the band's first entry (the following
    // branch's tail trim must not have unmaterialized it) and its line is in
    // the band frame the band search runs over.
    assert_eq!(
        h.window.start(),
        0,
        "the revealed page must stay materialized (the window top is held)"
    );
    assert!(h.window.hold_top(), "the revealed band top is held");
    assert!(h.band_contains(MARKER_OLDEST));
    let result = alt.search.result_text();
    assert!(
        result.contains('/'),
        "the box reports n/m over the walked-in matches: {result}"
    );
    drop(alt);
    assert!(
        h.screen().iter().any(|line| line.contains(&result)),
        "the painted search box carries the result text: {result}"
    );
}

// ---------------------------------------------------------------------------
// (ii) no match anywhere: the walk terminates at the file start, honestly
// ---------------------------------------------------------------------------

#[test]
fn a_match_nowhere_walks_to_the_file_start_and_reports_it_honestly() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("none", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();

    h.assert_at_live_end();
    h.open_search();
    h.type_quiet(MARKER_MISSING);
    h.assert_at_live_end();
    h.step();
    assert_eq!(
        h.search().search.result_text(),
        "\u{2026}",
        "the walk is in flight"
    );

    h.settle(64);
    let alt = h.search();
    assert_eq!(
        alt.search.walk.outcome(),
        Some(SearchWalkOutcome::Exhausted),
        "no match means the walk must reach the file start"
    );
    assert!(alt.search.matches.is_empty());
    assert_eq!(alt.search.selected, None);
    assert!(h.pages.exhausted, "the file-start reply must have landed");
    assert_eq!(h.pages.cursor, None, "the cursor is spent");
    assert!(
        !alt.search.walk.is_walking(),
        "the walk stopped: no spinner may be left"
    );
    assert_eq!(
        alt.search.result_text(),
        "No matches",
        "the honest empty state (the whole session was searched)"
    );
    drop(alt);
    assert!(
        h.screen().iter().any(|line| line.contains("No matches")),
        "the painted box shows the empty state:\n{}",
        h.screen().join("\n")
    );
    assert!(!h.current_match_highlight());
    assert_eq!(h.scroll_offset(), 0, "no match means no reveal");

    // No spin: the settled walk issues no further reads.
    let reads = h.load_older_calls;
    for _ in 0..4 {
        h.step();
    }
    assert_eq!(h.load_older_calls, reads, "a settled walk must not read on");
    assert_eq!(h.search().search.result_text(), "No matches");
}

// ---------------------------------------------------------------------------
// (ii-b) a match already in the band: the walk never starts
// ---------------------------------------------------------------------------

#[test]
fn a_match_in_the_band_settles_without_paging() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("settle", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();
    assert!(
        h.band_contains(MARKER_NEWEST),
        "sanity: the tail marker is inside the materialized band"
    );

    // Deliberately NOT a quiet query: the marker is in the band, so the
    // ordinary keystroke-by-keystroke reveal runs (upstream `refreshSearch`).
    // What this test pins is the walk's premise: the band already holds the
    // match, so the backward walk must never read anything.
    h.open_search();
    h.type_text(MARKER_NEWEST);
    h.step();
    let alt = h.search();
    assert_eq!(
        alt.search.walk.outcome(),
        Some(SearchWalkOutcome::Found),
        "a band match settles the walk"
    );
    assert!(!alt.search.walk.is_walking(), "nothing left to walk");
    assert_eq!(
        alt.search.walk.pages(),
        0,
        "the walk never probed or read a page"
    );
    assert!(!alt.search.matches.is_empty(), "the band holds the match");
    assert_eq!(
        alt.search.selected,
        Some(0),
        "the single band match is the selected one"
    );
    drop(alt);
    assert_eq!(h.load_older_calls, 0, "nothing may be read off disk");
    assert!(
        h.screen_contains(&newest_line()),
        "the band match is revealed:\n{}",
        h.screen().join("\n")
    );
    assert!(h.current_match_highlight());
}

// ---------------------------------------------------------------------------
// (iii) a live-typed query supersedes a walk in flight
// ---------------------------------------------------------------------------

#[test]
fn a_superseded_walk_never_clobbers_the_new_query() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("supersede", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();

    h.assert_at_live_end();
    h.open_search();
    h.type_quiet(MARKER_PAGE);
    h.step();
    {
        let alt = h.search();
        assert!(alt.search.walk.is_walking());
        assert!(
            alt.search.walk.request_in_flight(),
            "the first page (which holds the marker) is on the wire"
        );
    }
    assert!(h.pages.loading);

    // Live edit: the query becomes one that matches nothing, which supersedes
    // the walk in flight.
    h.type_quiet("X");
    let superseded_query = format!("{MARKER_PAGE}X");
    {
        let alt = h.search();
        assert!(!alt.search.walk.request_in_flight());
        assert_eq!(
            alt.search.walk.query(),
            superseded_query.as_str(),
            "the live walk belongs to the new query"
        );
        assert_eq!(alt.search.walk.pages(), 0, "the new walk starts over");
    }

    // The superseded walk's page lands: it is real history (prepended), but it
    // must not be attributed to the live walk — no probe, no materialization,
    // no selection, no reveal, and no viewport move.
    let before = h.entries.len();
    let offset_before = h.scroll_offset();
    assert!(h.deliver_one(), "the superseded read is still on the wire");
    assert!(h.entries.len() > before, "the stale page is still loaded");
    {
        let alt = h.search();
        assert_eq!(
            alt.search.walk.pages(),
            0,
            "a superseded read is not the live walk's page"
        );
        assert!(alt.search.matches.is_empty());
        assert_eq!(alt.search.selected, None);
    }
    assert_eq!(
        h.scroll_offset(),
        offset_before,
        "the stale page must not move the view"
    );
    // The next iteration paints the stale page into the band without moving
    // the view: the visible tail is unchanged and the view has not jumped to
    // the newly loaded page.
    h.step();
    assert_eq!(h.scroll_offset(), offset_before);
    assert!(
        !h.screen_contains(&stale_page_line()),
        "the view must not jump to the stale page:\n{}",
        h.screen().join("\n")
    );

    h.settle(64);
    let alt = h.search();
    assert_eq!(alt.search.walk.query(), superseded_query.as_str());
    assert_eq!(
        alt.search.walk.outcome(),
        Some(SearchWalkOutcome::Exhausted)
    );
    assert!(
        alt.search.matches.is_empty(),
        "the new query has no match anywhere"
    );
    assert_eq!(alt.search.selected, None);
    assert_eq!(alt.search.result_text(), "No matches");
    drop(alt);
    assert_eq!(
        h.scroll_offset(),
        offset_before,
        "no reveal for the new query"
    );
    assert!(
        !h.current_match_highlight(),
        "no stale match may be highlighted for the new query"
    );
}

// ---------------------------------------------------------------------------
// (iv-a) Esc aborts a walk in flight; the reply lands as a plain chunk
// ---------------------------------------------------------------------------

#[test]
fn esc_cancels_a_walk_in_flight() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("esc", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();

    h.assert_at_live_end();
    h.open_search();
    // The marker lives in the FIRST page a walk reads, so the reply that is
    // on the wire when esc lands holds the match: cancelling must still make
    // it a plain chunk.
    h.type_quiet(MARKER_PAGE);
    h.step();
    {
        let alt = h.search();
        assert!(alt.search.walk.is_walking());
        assert!(alt.search.walk.request_in_flight());
    }
    h.press_esc();
    {
        let alt = h.search();
        assert!(!alt.search.active, "esc closes the box");
        assert!(
            !alt.search.walk.is_walking(),
            "esc aborts the in-flight walk"
        );
        assert!(!alt.search.walk.holds_band());
        assert!(alt.search.result_text().is_empty());
    }

    // The pending reply lands afterwards: prepended as history, nothing else.
    let before = h.entries.len();
    let reads = h.load_older_calls;
    let offset_before = h.scroll_offset();
    assert!(h.deliver_one(), "the aborted read still lands");
    assert!(h.entries.len() > before);
    assert_eq!(
        h.load_older_calls, reads,
        "the cancelled walk reads no more"
    );
    {
        let alt = h.search();
        assert!(alt.search.matches.is_empty());
        assert_eq!(alt.search.selected, None);
    }
    assert_eq!(
        h.scroll_offset(),
        offset_before,
        "no reveal after the cancel"
    );
    // The next iteration paints the aborted page into the band: the viewport
    // must stay where it was and must not jump to the page that held a match.
    h.step();
    assert_eq!(h.scroll_offset(), offset_before);
    assert!(
        !h.screen_contains(&stale_page_line()),
        "the view must not jump to the aborted page:\n{}",
        h.screen().join("\n")
    );
    for _ in 0..2 {
        h.step();
    }
    assert_eq!(h.load_older_calls, reads, "no walk restarts after esc");
    assert!(!h.current_match_highlight());
}

// ---------------------------------------------------------------------------
// (iv-b) the resident prefix is searchable without a disk read
// ---------------------------------------------------------------------------

#[test]
fn resident_prefix_is_probed_before_the_disk() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("resident", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();

    // Walk once for a query that is in no turn: every page is loaded
    // (prepended), and the window's tail budget keeps the oldest entries
    // UNMATERIALIZED — the resident prefix a huge session always has.
    h.assert_at_live_end();
    h.open_search();
    h.type_quiet(MARKER_MISSING);
    h.settle(64);
    assert!(h.pages.exhausted, "the first walk reached the file start");
    assert!(
        h.entries.len() == FIXTURE_TURNS,
        "the walk loaded the whole history into memory ({} entries)",
        h.entries.len()
    );
    // The band's top is the tail window's start, and the BYTE budget is the
    // binding one: 64 × ~24 KiB is over the 1 MiB soft minimum, so the window
    // holds fewer than `TRANSCRIPT_TAIL_MIN_ENTRIES` turns and the oldest
    // entries stay unmaterialized. The arithmetic is asserted through the
    // shipped planner, not assumed: a budget change must fail here loudly.
    let expected_start = tail_window_start(
        &h.entries,
        0,
        TRANSCRIPT_WINDOW_MAX_ENTRIES,
        TRANSCRIPT_TAIL_BYTES,
    );
    assert!(expected_start > 0, "the fixture must window at all");
    assert_eq!(
        h.window.start(),
        expected_start,
        "the tail window starts where the budget says ({expected_start}), not {}",
        h.window.start()
    );
    assert!(
        !h.band_contains(MARKER_OLDEST),
        "the unmaterialized prefix must not be in the band"
    );
    assert_eq!(
        h.scroll_offset(),
        0,
        "sanity: the first walk revealed nothing (the view still follows)"
    );

    // `type_quiet` types into the LIVE box, so the no-match query above is
    // still in it. Close and reopen: `SearchState::close` clears the query and
    // cancels the old walk, so the next query is the marker alone. Without
    // this the walk below searched `MARKER_MISSING + MARKER_OLDEST` and every
    // assertion about "the marker" was vacuous (nothing asserted the query).
    h.key(Harness::search_toggle_key());
    assert!(
        !h.search().search.active,
        "the search chord closes the box, which clears the query"
    );
    h.open_search();

    // A NEW query whose match lives in that unmaterialized prefix. The
    // contract, at the levels this harness can assert honestly:
    //
    // (1) the marker is IN the resident prefix (an entry-level fact — the
    //     prefix is what the walk must search, and the band provably has it
    //     not, see above);
    // (2) the PLANNER probes that prefix before it reads: with the marker
    //     resident and the band matchless, the next step is `ProbeResident`,
    //     never a `LoadOlder` — the disk cursor sits BELOW the prefix, so
    //     paging could not reach this marker at all;
    // (3) the probe slice is the whole prefix (one 1 MiB-capped page), and the
    //     PRODUCTION probe (`probe_older_page`, the scratch container) sees
    //     the marker in it;
    // (4) the step itself spends one probe page and issues no disk read.
    //
    // The step's second half — `materialize_from(from)`, the completing hold,
    // the band check and the reveal — is asserted at the END of this test for
    // this resident-prefix state, and end to end by
    // `older_page_match_is_found_and_revealed` when the same chain runs over a
    // page loaded off disk.
    let prefix = &h.entries[..h.window.start()];
    assert!(
        prefix.iter().any(|entry| matches!(
            entry,
            ChatEntry::User(text) if text.contains(MARKER_OLDEST)
        )),
        "the marker is in the resident prefix ({} entries above the band top)",
        h.window.start()
    );

    h.type_quiet(MARKER_OLDEST);
    assert!(
        !h.pages.loading,
        "no read may be on the wire when the walk plans its next step"
    );
    let plan = {
        let mut alt = h.search();
        altscreen_search::plan_search_walk(
            &mut alt.search,
            true,
            h.window.start(),
            h.pages.cursor,
            h.pages.loading,
        )
    };
    assert_eq!(
        plan,
        SearchWalkPlan::ProbeResident,
        "the walk probes the resident prefix before it reads anything"
    );
    let (from, bytes) = resident_probe_slice(
        &h.entries,
        h.window.start(),
        TRANSCRIPT_WINDOW_MAX_ENTRIES,
        TRANSCRIPT_TAIL_BYTES,
    );
    assert_eq!(from, 0, "the probe slice spans the whole resident prefix");
    assert!(bytes > 0);
    let mut scratch = ChatContainer::new();
    assert!(
        probe_older_page(
            &mut scratch,
            &h.entries[from..h.window.start()],
            MARKER_OLDEST,
            h.core.columns().max(1),
        ),
        "the production probe sees the marker in the resident prefix"
    );

    // The step: one probe page counted, no disk read issued.
    let reads_before = h.load_older_calls;
    let query_now = {
        let alt = h.search();
        alt.search.query.clone()
    };
    assert_eq!(
        query_now, MARKER_OLDEST,
        "the search box must hold the query verbatim before the walk steps"
    );
    assert!(
        probe_older_page(
            &mut scratch,
            &h.entries[from..h.window.start()],
            &query_now,
            h.core.columns().max(1),
        ),
        "the production probe sees the marker for the state's own query"
    );
    let (probed_before, window_before) = {
        let alt = h.search();
        (alt.search.walk.probed_entries(), h.window.start())
    };
    assert_eq!(
        probed_before, 0,
        "a freshly (re)opened query starts its prefix probe at the band top"
    );
    h.step();
    let (probe_pages, probe_probed) = {
        let alt = h.search();
        (alt.search.walk.pages(), alt.search.walk.probed_entries())
    };
    assert_eq!(probe_pages, 1, "one resident probe slice, no disk page");
    assert_eq!(
        probe_probed, window_before,
        "the single probe covered the whole resident prefix"
    );
    assert!(
        !h.search().search.walk.request_in_flight(),
        "a resident probe never puts a read on the wire"
    );
    assert_eq!(
        h.load_older_calls, reads_before,
        "probing the resident prefix must not read the disk"
    );

    // ---- the step's second half, for a RESIDENT hit -----------------------
    // The probe hit, so the step materialized the prefix, armed the post-render
    // band check via `arms_band_check` (the same predicate the production loop
    // consumes) and that check ran in this same frame — the reveal must have
    // landed. An unarmed check would leave `note_reveal`'s `completing` set
    // forever: nothing selected (`/n` with no ordinal), the band held, the box
    // stuck on `…`.
    assert!(
        SearchWalkStep::Materialized.arms_band_check(),
        "a materialization must be judged against the rendered band"
    );
    assert!(
        !SearchWalkStep::Probed.arms_band_check(),
        "a probe only repaints its progress"
    );
    assert_eq!(
        h.window.start(),
        0,
        "the hit materialized the whole resident prefix ({window_before} -> 0)"
    );
    let (outcome, matches_len, selected) = {
        let alt = h.search();
        (
            alt.search.walk.outcome(),
            alt.search.matches.len(),
            alt.search.selected,
        )
    };
    assert_eq!(
        outcome,
        Some(SearchWalkOutcome::Found),
        "the resident-prefix hit must be revealed by the post-render check"
    );
    assert!(matches_len > 0, "the band now holds the match");
    assert!(
        selected.is_some(),
        "the reveal must select the match — an unarmed band check leaves \
         `selected` None and the walk completing"
    );
    // `scroll_to_match` records the offset during the check; the next frame is
    // the one that paints it (the loop's normal one-frame lag).
    h.step();
    assert!(
        h.band_contains(MARKER_OLDEST),
        "the resident hit must be materialized into the band"
    );
    assert!(
        h.scroll_offset() > 0,
        "the reveal must move the viewport onto the resident hit (offset {})",
        h.scroll_offset()
    );
    assert!(
        h.screen_contains(&oldest_line()),
        "the revealed resident match line must be on screen:\n{}",
        h.screen().join("\n")
    );
    assert_eq!(
        h.load_older_calls, reads_before,
        "a resident hit needs no disk read, before or after the reveal"
    );
}

// ---------------------------------------------------------------------------
// (iv-c) the bound: an honest partial label, not a false "No matches"
// ---------------------------------------------------------------------------

#[test]
fn the_walk_bound_stops_with_a_partial_label() {
    let _guard = test_lock();
    warm_theme();
    // Two turns per page — for the manager's resident window AND for every
    // page the walk reads — so the oldest marker sits 35 pages down, beyond
    // the walk's 16-page budget.
    let fixture = paged_fixture("bound", FIXTURE_TURNS, 2);
    let mut h = Harness::new(fixture, true);
    h.step();
    h.assert_at_live_end();
    h.open_search();
    h.type_quiet(MARKER_OLDEST);
    h.settle(64);
    let alt = h.search();
    assert_eq!(
        alt.search.walk.outcome(),
        Some(SearchWalkOutcome::Capped),
        "a {SEARCH_WALK_MAX_PAGES}-page budget cannot reach a 35-page-deep marker"
    );
    assert!(alt.search.matches.is_empty());
    assert_eq!(
        alt.search.result_text(),
        "No match so far",
        "a capped walk must not claim the whole session was searched"
    );
    assert!(h.pages.cursor.is_some(), "history is still on disk");
    assert_eq!(
        h.load_older_calls, SEARCH_WALK_MAX_PAGES,
        "the page bound is the stop: no more, no fewer"
    );
    drop(alt);
    assert!(
        h.screen()
            .iter()
            .any(|line| line.contains("No match so far")),
        "the painted box carries the partial label:\n{}",
        h.screen().join("\n")
    );
    assert_eq!(h.scroll_offset(), 0, "a capped walk reveals nothing");
    let reads = h.load_older_calls;
    for _ in 0..3 {
        h.step();
    }
    assert_eq!(h.load_older_calls, reads, "a capped walk must not spin");
}

/// `Exhausted` renders as "No matches" — a whole-session claim. Reaching the
/// file start is not enough on its own: the resident entries above the band are
/// probed only while the page budget lasts, so a fully resident transcript
/// whose prefix outran the budget must stop with `Capped` ("No match so far")
/// rather than claim the session holds no match. This is the honesty rule
/// `plan_search_walk`'s terminal arm encodes.
#[test]
fn exhausted_requires_the_whole_session_to_have_been_searched() {
    /// A walk whose page bound is fully spent with nothing probed (each
    /// resident slice and each disk page is one page of the bound).
    fn spent_walk() -> SearchWalk {
        let mut walk = SearchWalk::default();
        walk.start("needle");
        while walk.budget_left() {
            walk.note_probe(0, 0);
        }
        assert_eq!(walk.pages(), SEARCH_WALK_MAX_PAGES);
        assert_eq!(walk.probed_entries(), 0);
        walk
    }

    let armed = || SearchState {
        active: true,
        query: "needle".to_owned(),
        walk: spent_walk(),
        ..SearchState::default()
    };

    // The disk walk is done (`cursor == None`), but 4096 resident entries above
    // the band were never probed.
    let mut search = armed();
    let plan = altscreen_search::plan_search_walk(&mut search, true, 4_096, None, false);
    assert_eq!(plan, SearchWalkPlan::Idle);
    assert_eq!(
        search.walk.outcome(),
        Some(SearchWalkOutcome::Capped),
        "an unprobed resident prefix forbids the whole-session claim"
    );
    assert_eq!(search.result_text(), "No match so far");

    // Nothing resident above the band: the whole session really was searched.
    let mut search = armed();
    let plan = altscreen_search::plan_search_walk(&mut search, true, 0, None, false);
    assert_eq!(plan, SearchWalkPlan::Idle);
    assert_eq!(
        search.walk.outcome(),
        Some(SearchWalkOutcome::Exhausted),
        "a spent budget with nothing unprobed above the band searched everything"
    );
    assert_eq!(search.result_text(), "No matches");
}

/// A page read issued before a session switch must be dropped, not prepended.
/// `reset_session_transcript` calls `OlderPages::mark_session_reset`; the reply
/// carries no session identity, and at most one read is ever in flight, so the
/// one reply that can land after the switch is the outgoing session's — putting
/// it at the top of the new transcript would be cross-session contamination.
#[test]
fn a_page_read_from_the_outgoing_session_is_dropped_after_a_switch() {
    let _guard = test_lock();
    warm_theme();
    let fixture = paged_fixture("switch", FIXTURE_TURNS, PAGE_TURNS);
    let mut h = Harness::new(fixture, true);
    h.step();
    // One read on the wire, as the scroll-up path would issue it.
    let cursor = h
        .manager
        .prefix_cursor()
        .expect("the fixture pages off disk");
    h.pages.cursor = Some(cursor);
    h.pages.loading = true;
    h.pending.push(cursor);
    let before = h.entries.len();

    // The user switches sessions while that read is in flight.
    h.pages.mark_session_reset();

    // Its reply lands: dropped, not prepended.
    assert!(h.deliver_one(), "the stale reply was still delivered");
    assert_eq!(
        h.entries.len(),
        before,
        "the outgoing session's page must not be prepended to the new transcript"
    );
    assert_eq!(
        h.pages.cursor, None,
        "the new session starts with no cursor"
    );
    assert!(!h.pages.loading, "the one in-flight read is accounted for");

    // The new session's own first page is applied normally: the discard
    // consumed exactly one stale reply, not the new session's page.
    let cursor = h.manager.prefix_cursor().expect("still paged");
    h.pages.cursor = Some(cursor);
    h.pages.loading = true;
    h.pending.push(cursor);
    assert!(h.deliver_one(), "the new session's page must land");
    assert!(
        h.entries.len() > before,
        "the new session's page is prepended normally"
    );
}

// ---------------------------------------------------------------------------
// (v) the non-windowed path is unchanged
// ---------------------------------------------------------------------------

#[test]
fn regular_sessions_keep_the_pre_existing_search_path() {
    let _guard = test_lock();
    warm_theme();
    let fixture = small_fixture("regular");
    assert_eq!(
        fixture.manager.prefix_cursor(),
        None,
        "sanity: nothing paged"
    );
    let mut h = Harness::new(fixture, false);
    h.step();
    assert!(
        h.screen_contains(&small_line()),
        "sanity: a regular session materializes the whole transcript"
    );

    h.open_search();
    h.type_text(MARKER_OLDEST);
    h.step();
    let alt = h.search();
    assert!(
        !alt.search.matches.is_empty(),
        "the ordinary in-frame search finds it without any walk"
    );
    assert!(
        !alt.search.walk.is_walking(),
        "a regular session never arms the backward walk"
    );
    assert_eq!(alt.search.walk.outcome(), None);
    drop(alt);
    assert_eq!(h.load_older_calls, 0, "nothing may be read off disk");
    h.step();
    assert!(
        h.screen_contains(&small_line()),
        "the reveal shows the match"
    );

    // A query with no match keeps the pre-existing empty state (no `…`, no
    // walk, no read).
    h.type_text("X");
    h.step();
    let alt = h.search();
    assert!(!alt.search.walk.is_walking());
    assert_eq!(alt.search.result_text(), "No matches");
    drop(alt);
    assert_eq!(h.load_older_calls, 0);
}
