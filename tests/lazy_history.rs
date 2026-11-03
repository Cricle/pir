//! Focused tests for tail-first lazy paged history.
//!
//! The paged session manager ([`SessionManager::open_paged`]) is the product
//! seam: an open maps the file, parses ONLY a byte/entry-capped tail window and
//! leaves the prefix on disk. These tests pin the contract the TUI/SDK rely on:
//!
//! - the resident window is bounded and the file header still identifies the
//!   session;
//! - walking pages back is a partition of the file's entries (no duplicate, no
//!   skip) even with a torn/partial trailing line or a line larger than the
//!   byte cap;
//! - the page cache dedupes a prefetch and a concurrent read, and repeated
//!   scroll-ups are cache hits;
//! - the near-top prefetch trigger is strictly earlier than the load trigger.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::session_manager::page_cache::PageCache;
use pir::session_manager::{PageCacheKey, SessionManager, SessionTailPage};
use pir::tui::app::ChatEntry;
use pir::tui::terminal::{
    prepend_older_entries, should_load_older, should_prefetch_older,
    transcript_entries_from_messages, OlderPrefetchGate, TranscriptWindow,
    OLDER_PREFETCH_VIEWPORTS,
};

const PAYLOAD: usize = 512;
const PAGE_BYTES: usize = 8 * 1024;
const PAGE_ENTRIES: usize = 12;

struct Fixture {
    dir: PathBuf,
    path: PathBuf,
    ids: Vec<String>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-lazy-history-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn write_raw_session(path: &Path, turns: usize, payload: usize) -> Vec<String> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create session"));
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "lazy-history-fixture",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": "C:\\\\repo",
        })
    )
    .expect("header");
    let mut ids = Vec::with_capacity(turns);
    let mut parent: Option<String> = None;
    for index in 0..turns {
        let id = format!("u{index:06}");
        let text = format!("MARKER{index:06} {}", "x".repeat(payload));
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message",
                "id": id,
                "parentId": parent,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": "user",
                    "timestamp": 0,
                    "content": [{"type": "text", "text": text}],
                },
            })
        )
        .expect("message line");
        parent = Some(id.clone());
        ids.push(id);
    }
    file.flush().expect("flush session");
    ids
}

fn fixture(label: &str, turns: usize) -> Fixture {
    let dir = temp_dir(label);
    let path = dir.join("session.jsonl");
    let ids = write_raw_session(&path, turns, PAYLOAD);
    Fixture { dir, path, ids }
}

fn entry_ids(entries: &[Arc<pir::session_manager::FileEntry>]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| !entry.is_header())
        .map(|entry| entry.entry_id().to_owned())
        .collect()
}

fn page_ids(page: &SessionTailPage) -> Vec<String> {
    entry_ids(page.entries())
}

fn chat_user_texts(entries: &[ChatEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::User(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Walk every page from the file end to the start, returning pages newest
/// first (the order `read_older_page` produces them).
fn walk_pages(path: &Path, max_bytes: usize, max_entries: usize) -> Vec<SessionTailPage> {
    let mut pages = Vec::new();
    let Some(mut page) = SessionTailPage::open(path, max_bytes, max_entries).expect("open page")
    else {
        return pages;
    };
    loop {
        let next = if page.page_start() == 0 {
            None
        } else {
            page.extend(max_bytes, max_entries).expect("extend page")
        };
        pages.push(page);
        let Some(next) = next else {
            break;
        };
        page = next;
    }
    pages
}

fn concat_oldest_first(pages: Vec<SessionTailPage>) -> Vec<String> {
    let mut ids = Vec::new();
    for page in pages.into_iter().rev() {
        ids.extend(page_ids(&page));
    }
    ids
}

// ---------------------------------------------------------------------------
// 1. Tail-only open
// ---------------------------------------------------------------------------

#[test]
fn open_paged_parses_only_the_bounded_tail_and_keeps_the_file_header() {
    let fixture = fixture("tail-only", 900);
    let file_bytes = std::fs::metadata(&fixture.path).expect("metadata").len();
    assert!(file_bytes > 256 * 1024, "fixture must be paged");

    let manager = SessionManager::open_paged(&fixture.path, 32 * 1024, 40).expect("paged open");
    assert!(manager.is_paged(), "fixture must exceed the caps");
    let window = manager.paged_window().expect("paged window");
    assert_eq!(window.file_bytes, file_bytes);
    assert!(window.page_start > 0, "the prefix must be skipped");
    assert!(window.parsed_entries <= 40, "{window:?}");

    // Header identity comes from the file head, not from the tail page's first
    // line (which is an ordinary message).
    assert_eq!(manager.id(), "lazy-history-fixture");
    assert_eq!(manager.cwd(), "C:\\repo");
    let resident = manager.resident_entries();
    assert!(resident[0].is_header(), "the manager seeds the real header");
    let resident_messages = manager.entries();
    assert!(
        resident_messages.len() <= 40,
        "resident entries are bounded: {}",
        resident_messages.len()
    );
    assert!(
        resident_messages
            .last()
            .is_some_and(|entry| entry.entry_id() == fixture.ids.last().unwrap()),
        "the newest entry is resident"
    );
    assert!(
        !resident_messages
            .iter()
            .any(|entry| entry.entry_id() == fixture.ids[0]),
        "the oldest entry stays on disk"
    );

    let page = SessionTailPage::open(&fixture.path, 32 * 1024, 40)
        .expect("direct page")
        .expect("mappable file");
    let stats = page.stats();
    assert!(!stats.whole_file, "{stats:?}");
    assert!(stats.page_bytes <= 32 * 1024 + 64 * 1024, "{stats:?}");
    assert!(
        stats.skipped_bytes >= file_bytes - stats.page_bytes - 1024,
        "the prefix stays untouched: {stats:?}"
    );
    assert!(stats.parsed_entries <= 40, "{stats:?}");
    assert_eq!(
        page.file_header().map(|header| header.id),
        Some("lazy-history-fixture".into())
    );
}

// ---------------------------------------------------------------------------
// 2. Page boundaries: no duplicate, no skip
// ---------------------------------------------------------------------------

#[test]
fn page_walk_is_a_partition_with_a_torn_valid_tail() {
    let fixture = fixture("torn-valid", 160);
    // Full-load oracle (this also repairs the missing final newline).
    let full = SessionManager::open(&fixture.path).expect("full open");
    let expected: Vec<String> = full
        .entries()
        .iter()
        .map(|entry| entry.entry_id().to_owned())
        .collect();
    drop(full);

    // Torn tail: the last complete record lost only its newline byte.
    let bytes = std::fs::read(&fixture.path).expect("read");
    assert_eq!(bytes.last(), Some(&b'\n'));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&fixture.path)
        .expect("open for truncate")
        .set_len(bytes.len() as u64 - 1)
        .expect("truncate newline");
    assert_ne!(std::fs::read(&fixture.path).unwrap().last(), Some(&b'\n'));

    let pages = walk_pages(&fixture.path, PAGE_BYTES, PAGE_ENTRIES);
    assert!(pages.len() > 3, "the fixture must need several pages");
    for page in &pages {
        assert!(
            page.stats().parsed_entries <= PAGE_ENTRIES,
            "the torn tail must not break the entry cap: {:?}",
            page.stats()
        );
    }
    let actual = concat_oldest_first(pages);
    assert_eq!(actual, expected, "pages must partition the file exactly");
    let unique: std::collections::HashSet<_> = actual.iter().collect();
    assert_eq!(unique.len(), actual.len(), "no duplicate entry ids");
}

#[test]
fn page_walk_skips_a_partial_torn_line_without_duplicating_entries() {
    let fixture = fixture("torn-partial", 120);
    let full = SessionManager::open(&fixture.path).expect("full open");
    let expected: Vec<String> = full
        .entries()
        .iter()
        .map(|entry| entry.entry_id().to_owned())
        .collect();
    drop(full);

    // A crash mid-write leaves a JSON fragment on the final line. It must be
    // preserved as an unparsed line and must not poison any page boundary.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.path)
        .expect("append");
    file.write_all(
        b"{\"type\":\"message\",\"id\":\"torn-partial\",\"parentId\":\"u000119\",\"message\":",
    )
    .expect("partial line");
    file.flush().expect("flush");

    let pages = walk_pages(&fixture.path, PAGE_BYTES, PAGE_ENTRIES);
    let newest = pages.first().expect("newest page");
    assert!(
        !newest.unparsed_lines().is_empty(),
        "the torn fragment is preserved verbatim"
    );
    let actual = concat_oldest_first(pages);
    assert_eq!(actual, expected, "a torn line must not hide older entries");
    assert!(
        !actual.iter().any(|id| id == "torn-partial"),
        "an unparseable line is never invented as an entry"
    );
}

#[test]
fn page_boundaries_never_split_a_line_larger_than_the_byte_cap() {
    let dir = temp_dir("long-line");
    let path = dir.join("session.jsonl");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "session", "version": 3, "id": "long-line", "cwd": "C:\\\\repo",
            "timestamp": "2026-01-01T00:00:00.000Z",
        })
    )
    .unwrap();
    let mut ids = Vec::new();
    for index in 0..30 {
        let id = format!("l{index:03}");
        let text = if index == 15 {
            format!("LONG{index:03} {}", "y".repeat(256 * 1024))
        } else {
            format!("SHORT{index:03} {}", "z".repeat(200))
        };
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "message", "id": id, "parentId": if index == 0 { None } else { Some(format!("l{:03}", index - 1)) },
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {"role":"user","timestamp":0,"content":[{"type":"text","text":text}]},
            })
        )
        .unwrap();
        ids.push(id);
    }
    file.flush().unwrap();

    let pages = walk_pages(&path, 8 * 1024, 8);
    let mut saw_long = false;
    for page in &pages {
        let stats = page.stats();
        assert!(
            stats.parsed_entries <= 8,
            "the entry cap still holds around a long line: {stats:?}"
        );
        for entry in page.entries() {
            if entry.entry_id() == "l015" {
                saw_long = true;
            }
        }
    }
    assert!(saw_long, "the oversized line must appear whole on one page");
    assert_eq!(concat_oldest_first(pages), ids);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. Prefetch, cache and concurrent coalescing
// ---------------------------------------------------------------------------

fn wait_for_cached(manager: &SessionManager, cursor: u64, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if manager.is_older_page_cached(cursor, PAGE_BYTES, PAGE_ENTRIES) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    manager.is_older_page_cached(cursor, PAGE_BYTES, PAGE_ENTRIES)
}

#[test]
fn prefetch_populates_the_cache_and_repeated_scroll_ups_are_hits() {
    let fixture = fixture("prefetch", 240);
    let manager = SessionManager::open_paged(&fixture.path, 24 * 1024, 24).expect("open");
    assert!(manager.is_paged());
    let cursor = manager.prefix_cursor().expect("paged cursor");

    assert!(
        manager.prefetch_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES),
        "first prefetch starts a worker"
    );
    assert!(
        !manager.prefetch_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES),
        "a duplicate prefetch for the same key is coalesced"
    );
    assert!(wait_for_cached(
        &manager,
        cursor,
        std::time::Duration::from_secs(10)
    ));
    let after_prefetch = manager.older_page_cache_stats().expect("stats");
    assert_eq!(after_prefetch.loads, 1, "{after_prefetch:?}");
    assert_eq!(after_prefetch.prefetches, 1, "{after_prefetch:?}");
    assert!(!manager.prefetch_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES));

    let first = manager
        .read_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read")
        .expect("page");
    let first_ids = page_ids(&first);
    assert_eq!(
        first.file_header().map(|header| header.id),
        Some("lazy-history-fixture".to_string()),
        "a cached page still reports the file header without the mmap"
    );
    let after_first = manager.older_page_cache_stats().expect("stats");
    assert_eq!(
        after_first.loads, after_prefetch.loads,
        "a cached read must not re-parse the page"
    );
    assert!(after_first.hits >= 1, "{after_first:?}");

    let second = manager
        .read_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read")
        .expect("page");
    assert_eq!(page_ids(&second), first_ids);
    let after_second = manager.older_page_cache_stats().expect("stats");
    assert_eq!(after_second.loads, after_first.loads);
    assert!(after_second.hits >= 2, "{after_second:?}");

    // The page is the newest chunk of the skipped prefix, not the resident
    // tail: its first marker is older than the resident window's first one.
    let resident: Vec<String> = manager
        .entries()
        .iter()
        .map(|entry| entry.entry_id().to_owned())
        .collect();
    assert!(
        first_ids
            .iter()
            .all(|id| !resident.iter().any(|resident_id| resident_id == id)),
        "older page and resident window must not overlap"
    );
    assert!(first_ids.last().unwrap() < resident.first().unwrap());

    // Walking to the next page and prefetching it remains ordered.
    let next_cursor = first.page_start();
    assert!(next_cursor > 0);
    assert!(manager.prefetch_older_page(next_cursor, PAGE_BYTES, PAGE_ENTRIES));
    assert!(wait_for_cached(
        &manager,
        next_cursor,
        std::time::Duration::from_secs(10)
    ));
    let next = manager
        .read_older_page(next_cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read")
        .expect("page");
    assert!(
        next.page_start() < first.page_start(),
        "the next page must start strictly older"
    );
    assert!(page_ids(&next).last().unwrap() < first_ids.first().unwrap());
}

#[test]
fn scroll_up_prepends_an_older_page_without_moving_the_resident_order() {
    let fixture = fixture("scroll-up", 140);
    let manager = SessionManager::open_paged(&fixture.path, 16 * 1024, 16).expect("open");
    let resident = transcript_entries_from_messages(
        manager
            .build_context_entries()
            .expect("resident projection"),
    );
    let resident_texts = chat_user_texts(&resident);
    assert!(!resident_texts.is_empty());

    let cursor = manager.prefix_cursor().expect("cursor");
    let page = manager
        .read_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read")
        .expect("page");
    let older_messages: Vec<pir::ai::types::Message> = page
        .entries()
        .iter()
        .flat_map(|entry| {
            pir::session_manager::projection::session_entry_to_context_messages(entry)
        })
        .collect();
    let older = transcript_entries_from_messages(older_messages);
    let older_texts = chat_user_texts(&older);
    assert!(!older_texts.is_empty());

    let mut entries = resident;
    let mut window = TranscriptWindow::new();
    let mut streaming_entry = None;
    let mut tool_entries = std::collections::HashMap::new();
    let inserted = prepend_older_entries(
        &mut entries,
        older,
        &mut window,
        &mut streaming_entry,
        &mut tool_entries,
    );
    assert!(inserted > 0);

    let texts = chat_user_texts(&entries);
    let resident_start = texts.len() - resident_texts.len();
    assert_eq!(&texts[resident_start..], resident_texts.as_slice());
    assert!(
        older_texts.last().unwrap() < &resident_texts[0],
        "the prepended page must be strictly older than the resident window"
    );
}

#[test]
fn concurrent_page_reads_coalesce_onto_one_parse() {
    let fixture = fixture("coalesce", 160);
    let file_len = std::fs::metadata(&fixture.path).expect("metadata").len();
    let cache = Arc::new(PageCache::new(4, 8 * 1024 * 1024));
    let key = PageCacheKey::new(fixture.path.clone(), file_len, PAGE_BYTES, PAGE_ENTRIES);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let cache = Arc::clone(&cache);
        let key = key.clone();
        handles.push(std::thread::spawn(move || {
            cache
                .get_or_load(key)
                .expect("page read")
                .expect("page")
                .entries()
                .len()
        }));
    }
    let lengths: Vec<usize> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join"))
        .collect();
    assert!(lengths.iter().all(|length| *length == lengths[0]));
    let stats = cache.stats();
    assert_eq!(stats.loads, 1, "one leader parsed the page: {stats:?}");
    assert_eq!(
        stats.hits + stats.coalesced,
        7,
        "every other reader was a hit or waited on the leader: {stats:?}"
    );
    assert!(stats.cached_pages >= 1);
}

#[test]
fn page_cache_is_bounded_and_evicts_old_pages() {
    let fixture = fixture("bounded", 260);
    let cache = PageCache::new(2, 64 * 1024 * 1024);
    let mut cursor = std::fs::metadata(&fixture.path).expect("metadata").len();
    let mut loaded = 0usize;
    while cursor > 0 && loaded < 8 {
        let key = PageCacheKey::new(fixture.path.clone(), cursor, PAGE_BYTES, PAGE_ENTRIES);
        let Some(page) = cache.get_or_load(key).expect("read") else {
            break;
        };
        cursor = page.page_start();
        loaded += 1;
    }
    assert!(loaded >= 5, "the fixture must span several pages");
    let stats = cache.stats();
    assert!(
        stats.cached_pages <= 2,
        "the LRU stays inside its page bound: {stats:?}"
    );
    assert!(stats.evictions >= 1, "{stats:?}");
}

#[test]
fn the_second_page_is_never_truncated_or_duplicated_by_the_cache() {
    let fixture = fixture("cache-walk", 120);
    let manager = SessionManager::open_paged(&fixture.path, 16 * 1024, 16).expect("open");
    let mut cursor = manager.prefix_cursor().expect("cursor");
    let mut chunks: Vec<Vec<String>> = Vec::new();
    while let Some(page) = manager
        .read_older_page(cursor, PAGE_BYTES, PAGE_ENTRIES)
        .expect("read")
    {
        let chunk = page_ids(&page);
        assert!(
            chunk
                .iter()
                .all(|id| !chunks.iter().flatten().any(|seen| seen == id)),
            "duplicate page entry"
        );
        chunks.push(chunk);
        cursor = page.page_start();
    }
    // Pages arrive newest-first; reversing the CHUNKS (not the entries inside
    // each page) must reproduce the skipped prefix's file order exactly. The
    // walk deliberately starts at the resident window's first entry, so the
    // resident entries are not part of it.
    let resident: std::collections::HashSet<String> = manager
        .entries()
        .iter()
        .map(|entry| entry.entry_id().to_owned())
        .collect();
    let expected_prefix: Vec<String> = fixture
        .ids
        .iter()
        .filter(|id| !resident.contains(*id))
        .cloned()
        .collect();
    let ids: Vec<String> = chunks.into_iter().rev().flatten().collect();
    assert_eq!(ids, expected_prefix);
}

// ---------------------------------------------------------------------------
// 4. TUI triggers: prefetch strictly before load
// ---------------------------------------------------------------------------

#[test]
fn prefetch_trigger_fires_before_the_load_trigger() {
    let viewport = 10usize;
    let prefetch_at = viewport * OLDER_PREFETCH_VIEWPORTS;
    assert!(
        prefetch_at > viewport * 2,
        "prefetch must lead the load trigger"
    );

    let gate = |windowed,
                scrolled,
                window_start,
                visible_top,
                cursor_available,
                request_in_flight,
                already_prefetched| {
        OlderPrefetchGate {
            windowed,
            scrolled,
            window_start,
            visible_top,
            viewport_rows: viewport,
            cursor_available,
            request_in_flight,
            already_prefetched,
        }
    };
    assert!(should_prefetch_older(gate(
        true,
        true,
        0,
        prefetch_at,
        true,
        false,
        false
    )));
    assert!(!should_load_older(
        true,
        true,
        0,
        prefetch_at,
        viewport,
        true,
        false
    ));
    assert!(should_load_older(
        true,
        true,
        0,
        viewport * 2,
        viewport,
        true,
        false
    ));
    assert!(!should_prefetch_older(gate(
        true,
        true,
        0,
        prefetch_at,
        true,
        false,
        true
    )));
    assert!(!should_prefetch_older(gate(
        true,
        false,
        0,
        prefetch_at,
        true,
        false,
        false
    )));
    assert!(!should_prefetch_older(gate(
        true,
        true,
        3,
        prefetch_at,
        true,
        false,
        false
    )));
    assert!(!should_prefetch_older(gate(
        true,
        true,
        0,
        prefetch_at,
        false,
        false,
        false
    )));
    assert!(!should_prefetch_older(gate(
        true,
        true,
        0,
        prefetch_at,
        true,
        true,
        false
    )));
}

#[test]
fn page_cache_key_is_cap_sensitive() {
    let a = PageCacheKey::new(PathBuf::from("a.jsonl"), 100, 1024, 8);
    let b = PageCacheKey::new(PathBuf::from("a.jsonl"), 100, 1024, 9);
    assert_ne!(a, b);
}
