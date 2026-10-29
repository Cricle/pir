//! Alt-screen interaction family: transcript search, text selection,
//! copy-on-select flash, wheel-scroll routing and scrollbar decoration.
//!
//! Ports the upstream `packages/tui/src` pieces pir's renderer can support:
//! - `alt-screen-search.ts` — the search corpus/matching model (spans map
//!   corpus text back to `(row, startCol, endCol)` of the *rendered* frame
//!   lines), plus match-key retention across re-searches;
//! - `tui-alt-screen.ts` — viewport scrolling (`scrollBy` with follow-end
//!   semantics mirrored by [`TuiCore::set_scroll_offset`]), mouse selection
//!   with `fullscreenCopyOnSelect`, wheel routing (`ALT_WHEEL_SCROLL_MULTIPLIER`),
//!   the scrollbar geometry (`layout.ts getScrollbarGeometry`), the scrollbar
//!   click/drag scrubbing (`handleScrollbarMouseEvent` /
//!   `scrollScrollbarToPointer` / `getScrollbarTargetAt`) with the
//!   hover-active thumb (`updateScrollbarHover` / `setScrollbarHover` →
//!   `ScrollView.setScrollbarActive`), the jump-to-end indicator
//!   (`compositeScrollToEndIndicator` / `handleScrollToEndIndicatorMouseEvent`),
//!   the Windows right-click paste route (`handleRightClickPaste`), and the
//!   `flash()`/`AltScreenFlashContainer` transient messages;
//! - `wheel-scroll.ts` — via [`super::wheel_scroll`].
//! - `utils.ts::getOsc8LinkAtColumn` — the OSC 8 hyperlink covering a pointer
//!   column ([`osc8_link_at_column`]), captured on a left press and opened by
//!   the release that lands back on the same cell (`openUrl`), per
//!   `tui-alt-screen.ts::handleSelectionMouseEvent`; the side effect leaves as
//!   [`AltMouseRoute::OpenUrl`].
//!
//! The mouse router returns an [`AltMouseRoute`]: the event loop owns the
//! clipboard side of the right-click paste (`AltMouseRoute::PasteClipboard`
//! → inject the clipboard as one bracketed paste into the focused component)
//! and the browser side of a hyperlink click (`AltMouseRoute::OpenUrl(url)` →
//! launch the URL, upstream's `TuiAltScreenOptions.openUrl` callback).
//! [`handle_mouse_event`] stays as the bool ("repaint") wrapper for callers
//! and tests that do not need those routes.
//!
//! This module is deliberately pure: no terminal I/O. The caller (the
//! `terminal.rs` event loop) supplies crossterm events, the [`TuiCore`]
//! renderer handle and a clipboard callback.
//!
//! Documented deviations from upstream (see the parity notes in each item):
//! - key ids: the `tui.altScreen.*` family is not in pir's keybinding
//!   registry (`keybindings.rs` is frozen for the parity batch), so the
//!   upstream-verbatim DEFAULT keys below are used directly; user overrides
//!   for these ids are not honored yet.
//! - search matching is Unicode case-insensitive with the same per-code-point
//!   canonicalization as the upstream `giu` regex (ASCII fast path; `ß`/`SS`
//!   full foldings are not matched, exactly like `iu`); the query is a plain
//!   substring, not a pattern.
//! - selection columns are measured over grapheme clusters (upstream
//!   `sliceByColumn` / `applySearchTextHighlight`): copying and painting a
//!   range never split a ZWJ emoji, a flag pair or a wide CJK cell.
//! - double/triple click select word/line granularity and a drag then extends
//!   by the same unit (upstream `getClickCount` / `getWordSelection` /
//!   `getLineSelection`, 500 ms window). Crossterm events carry no click
//!   count, so the count is reconstructed here from successive left presses on
//!   the same word within [`DOUBLE_CLICK_INTERVAL`] — the same rule upstream
//!   applies to raw SGR presses.
//! - semantic prompt navigation (`previousPrompt`/`nextPrompt`) anchors on
//!   the user-message block's `userMessageBg` paint instead of the OSC 133
//!   `A` zone upstream's user-message component prepends — see
//!   [`prompt_start_rows`].
//! - the OSC 8 lookup reads the *undecorated* frame line mapped through
//!   [`TuiCore::screen_row_to_buffer_row`] (upstream reads
//!   `previousScreen[y]`, the decorated screen): pir's decorations only splice
//!   styling over the same cells (an OSC 8 run is copied verbatim), and this
//!   keeps the link lookup on the same screen→line mapping the selection
//!   coordinates use. The upstream `pressedUrl = range ? undefined :
//!   getOsc8LinkAtColumn(...)` guard applies directly: a double/triple click
//!   selects a word/line and never captures a link.
//! - the Rust renderers in this checkout emit the plain `text (url)` link
//!   fallback, not OSC 8 (`markdown.rs` / `ansi.rs` track SGR only), so
//!   [`AltMouseRoute::OpenUrl`] fires for whatever OSC 8 runs do reach a
//!   frame line — e.g. extension UI text whose JS side already carries the
//!   shim's own `getOsc8LinkAtColumn` port (`extensions/api/js/
//!   pi_tui_shim.js.txt` and its `hyperlink()`), or upstream-authentic frames
//!   once the markdown renderer emits them.
//! - transcript search is not confined to the materialized band: a query
//!   with no match in the band runs a bounded backward walk
//!   ([`SearchWalk`]) — the resident prefix the window does not materialize
//!   first, then older pages off disk — and materializes the page holding the
//!   newest match for the ordinary reveal. Matches outside the loaded pages
//!   are only found as the walk loads them, so `n/m` counts exactly what the
//!   walk brought in (see [`SearchWalk`] and [`SearchState::result_text`]).
//! - a query edit that leaves zero matches returns the viewport to its
//!   pre-search position ([`SearchState::begin`] captures it): a live-typed
//!   query can match on an early prefix (the play test's `z` in `zeta`) and
//!   jump, and the matchless continuation must not leave the view at that
//!   stale reveal. Upstream leaves it there.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use super::ansi;
use super::core::{PaintedLayout, TuiCore};
use super::theme::{with_theme, ThemeColor};
use super::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};

// Upstream `PAGE_SCROLL_OVERLAP` / `ALT_WHEEL_SCROLL_MULTIPLIER` /
// `DOUBLE_CLICK_INTERVAL_MS` / flash durations (`alt-screen-flash.ts`,
// `tui-alt-screen.ts`).

/// Rows of overlap when page-scrolling (upstream `PAGE_SCROLL_OVERLAP`).
pub const PAGE_SCROLL_OVERLAP: usize = 4;
/// Lines per Alt+wheel notch (upstream `ALT_WHEEL_SCROLL_MULTIPLIER`).
pub const ALT_WHEEL_SCROLL_MULTIPLIER: u32 = 5;
/// Default transient-message duration (upstream flash default).
pub const DEFAULT_FLASH_DURATION: Duration = Duration::from_millis(1000);
/// Duration of the "Copy failed" flash (upstream error-flash duration).
pub const COPY_ERROR_FLASH_DURATION: Duration = Duration::from_millis(5000);
/// How long the scrollbar stays visible after a scroll gesture.
pub const SCROLLBAR_LINGER: Duration = Duration::from_millis(1000);
/// Successive left presses on the same word within this window count as a
/// double/triple click (upstream `DOUBLE_CLICK_INTERVAL_MS = 500`,
/// tui-alt-screen.ts:81).
pub const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Word-selection joiners (upstream `TERMINAL_WORD_SELECTION_JOINERS`,
/// tui-alt-screen.ts:86): a run of letters/digits joins across one of these
/// single-character segments, so double-clicking `src/main.rs` selects the
/// whole path.
pub const WORD_SELECTION_JOINERS: [&str; 7] = ["/", "-", "+", "\\", "~", "_", "."];

// Upstream `tui.altScreen.*` default chords
// (packages/tui/src/keybindings.ts:160-209). pir's frozen registry does not
// carry this family yet.

/// Search-match highlight (upstream `searchMatchStyle` default: underline).
const MATCH_OPEN: &str = "\x1b[4m";
const MATCH_CLOSE: &str = "\x1b[24m";
/// Current search match (upstream `searchCurrentMatchStyle` default:
/// bold + reverse).
const CURRENT_MATCH_OPEN: &str = "\x1b[1;7m";
const CURRENT_MATCH_CLOSE: &str = "\x1b[22;27m";
/// Selection paint (reverse) and the scrollbar palette (upstream
/// `scroll-view.ts` defaults).
const SELECTION_OPEN: &str = "\x1b[7m";
const SELECTION_CLOSE: &str = "\x1b[27m";
/// Idle thumb (`┃`, upstream `scrollbarThumbStyle("┃")`).
const SCROLLBAR_THUMB: &str = "\x1b[37m\u{2503}\x1b[39m";
/// Hover-active thumb (`█`): upstream paints it while
/// `ScrollView.isScrollbarActive` holds, which only `setScrollbarActive` (the
/// pointer over the track) sets — see [`AltScreenState::set_scrollbar_hover`].
const SCROLLBAR_THUMB_ACTIVE: &str = "\x1b[37m\u{2588}\x1b[39m";
const SCROLLBAR_TRACK: &str = "\x1b[90m\u{2502}\x1b[39m";
/// The dim SGR pair upstream's `Input` uses for its placeholder and the search
/// component for its result count (`\x1b[2m … \x1b[22m`).
const DIM_OPEN: &str = "\x1b[2m";
const DIM_CLOSE: &str = "\x1b[22m";
/// Box-drawing characters of the search box (upstream
/// `AltScreenSearchComponent.render`).
const BOX_TOP_LEFT: char = '\u{250c}';
const BOX_TOP_RIGHT: char = '\u{2510}';
const BOX_BOTTOM_LEFT: char = '\u{2514}';
const BOX_BOTTOM_RIGHT: char = '\u{2518}';
const BOX_HORIZONTAL: char = '\u{2500}';
const BOX_VERTICAL: char = '\u{2502}';
/// The transcript search overlay's placement (upstream
/// `showOverlay(component, { anchor: "top-right", width: "40%", minWidth: 32,
/// margin: 1 })`) and the label margin it needs to paint its own borders.
const SEARCH_BOX_MARGIN: usize = 1;
const SEARCH_BOX_WIDTH_PERCENT: usize = 40;
const SEARCH_BOX_MIN_WIDTH: usize = 32;
const SEARCH_BOX_MIN_PAINT_WIDTH: usize = 8;
/// The prev/next affordances' arrow glyphs.
const SEARCH_PREVIOUS_ARROW: char = '\u{2191}';
const SEARCH_NEXT_ARROW: char = '\u{2193}';
/// The jump-to-end indicator's arrow (upstream `tui-renderer.ts`:
/// `" ↓ Jump to latest message · <shortcut> "`).
const SCROLL_TO_END_ARROW: char = '\u{2193}';
const SCROLL_TO_END_LABEL: &str = "Jump to latest message";

// ---------------------------------------------------------------------------
// Search corpus + matching (upstream `alt-screen-search.ts`)
// ---------------------------------------------------------------------------

/// One match segment: `row` is an absolute buffer row (a full-frame line
/// index), columns are visible-cell columns of that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchSegment {
    /// Absolute buffer row (a full-frame line index).
    pub row: usize,
    /// First painted visible-cell column of the segment.
    pub start_col: usize,
    /// One past the last painted visible-cell column.
    pub end_col: usize,
}

/// One search match, possibly spanning several rows (upstream
/// `AltScreenSearchMatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// One segment per covered row, in row order.
    pub segments: Vec<SearchSegment>,
}

#[derive(Debug)]
struct CorpusSpan {
    text_start: usize,
    text_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
    linear_columns: bool,
}

#[derive(Debug, Default)]
struct SearchCorpus {
    text: String,
    /// `text` case-folded once at assembly time (upstream's `giu` regex
    /// canonicalizes both sides). ASCII text uses the cheap
    /// `to_ascii_lowercase` with identity offsets; non-ASCII text goes through
    /// the per-char Unicode canonicalization below.
    lower: String,
    /// `None` for ASCII-only corpora (folded byte offsets == original ones).
    /// `Some(offsets)` maps every folded byte to the byte offset of the
    /// ORIGINAL character that produced it, so a folded match translates back
    /// to the rendered line's columns even when a character's folded form has
    /// a different UTF-8 length (e.g. `ẞ` → `ß`).
    fold_offsets: Option<Vec<u32>>,
    spans: Vec<CorpusSpan>,
}

/// Per-char case canonicalization with ES `iu` semantics (upstream's
/// `new RegExp(…, "giu")` uses `Canonicalize`, ES2015 21.2.2.8.2): uppercase
/// the code point, then lowercase the single resulting code point; a
/// multi-code-point result leaves the character unchanged. The extra
/// `ch > 0x7F && upper < 0x80` rule keeps look-alikes such as KELVIN SIGN
/// (U+212A) and LONG S (U+017F) from matching ASCII `k`/`s`.
fn fold_case_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut upper_chars = c.to_uppercase();
    let Some(upper) = upper_chars.next() else {
        return c;
    };
    if upper_chars.next().is_some() {
        return c;
    }
    if (c as u32) > 0x7F && (upper as u32) < 0x80 {
        return c;
    }
    let mut lower_chars = upper.to_lowercase();
    let Some(lower) = lower_chars.next() else {
        return c;
    };
    if lower_chars.next().is_some() {
        return c;
    }
    lower
}

/// [`fold_case_char`] over a whole string (the query side).
fn fold_case(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    text.chars().map(fold_case_char).collect()
}

/// Case-fold `text` and, for non-ASCII input, build the folded-byte →
/// original-byte map used to translate matches back (see
/// [`SearchCorpus::fold_offsets`]).
fn fold_case_with_offsets(text: &str) -> (String, Option<Vec<u32>>) {
    if text.is_ascii() {
        return (text.to_ascii_lowercase(), None);
    }
    let mut folded = String::with_capacity(text.len());
    let mut offsets: Vec<u32> = Vec::with_capacity(text.len() + 1);
    for (index, c) in text.char_indices() {
        let mapped = fold_case_char(c);
        folded.push(mapped);
        for _ in 0..mapped.len_utf8() {
            offsets.push(index as u32);
        }
    }
    (folded, Some(offsets))
}

/// Stable identity of a match across re-searches (upstream
/// `getAltScreenSearchMatchKey`).
pub fn match_key(matched: &SearchMatch) -> String {
    match (matched.segments.first(), matched.segments.last()) {
        (Some(first), Some(last)) => format!(
            "{}:{}:{}:{}",
            first.row, first.start_col, last.row, last.end_col
        ),
        _ => String::new(),
    }
}

/// One non-space run of an ANSI-stripped frame line: the unit a
/// [`CorpusSpan`] indexes. Columns are visible cells (a tab counts three).
#[derive(Debug)]
struct CachedSearchRun {
    text: String,
    start_col: usize,
    width: usize,
    linear_columns: bool,
}

/// The parse of one frame line, tagged with a cheap fingerprint of the raw
/// (still ANSI-decorated) line so a refresh can reuse it while only the rows
/// whose content changed are stripped again.
#[derive(Debug)]
struct CachedSearchRow {
    /// `line.len()`; checked first, so a changed length misses without
    /// hashing.
    byte_len: usize,
    /// FNV-1a over the raw line bytes.
    fingerprint: u64,
    runs: Vec<CachedSearchRun>,
}

impl CachedSearchRow {
    fn parse(line: &str) -> Self {
        Self {
            byte_len: line.len(),
            fingerprint: line_fingerprint(line),
            runs: parse_search_runs(line),
        }
    }

    fn unchanged(&self, line: &str) -> bool {
        self.byte_len == line.len() && self.fingerprint == line_fingerprint(line)
    }
}

/// Content fingerprint of one raw frame line (FNV-1a 64): one
/// allocation-free pass over the line's bytes, far cheaper than
/// `strip_ansi`, a `Vec<char>` and the run strings. Unlike a
/// first/last-N-bytes sample it is sensitive to a change anywhere in the
/// line (a mid-row spinner update).
fn line_fingerprint(line: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in line.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Strip ANSI and split one line into its maximal non-space runs.
fn parse_search_runs(line: &str) -> Vec<CachedSearchRun> {
    let plain = ansi::strip_ansi(line);
    let mut runs = Vec::new();
    let mut column = 0usize;
    let mut chars = plain.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' || c == '\t' {
            column += if c == '\t' { 3 } else { 1 };
            continue;
        }
        // One non-space run per span (upstream fast path: rendered
        // transcripts are overwhelmingly ASCII, so runs are indexed whole
        // instead of allocating one mapping per cell).
        let mut run = String::new();
        run.push(c);
        let mut run_width = UnicodeWidthChar::width(c).unwrap_or(0).max(1);
        while let Some(&next) = chars.peek() {
            if next == ' ' || next == '\t' {
                break;
            }
            run.push(next);
            chars.next();
            run_width += UnicodeWidthChar::width(next).unwrap_or(0).max(1);
        }
        let linear_columns = run.is_ascii();
        runs.push(CachedSearchRun {
            text: run,
            start_col: column,
            width: run_width,
            linear_columns,
        });
        column += run_width;
    }
    runs
}

/// Assemble cached row parses into the flattened corpus. Runs are maximal, so
/// every gap (inside a row or across rows) collapses to exactly one separator
/// before each run except the first — the whitespace rule the previous
/// single-pass builder applied.
fn assemble_search_corpus(rows: &[CachedSearchRow]) -> SearchCorpus {
    let mut corpus = SearchCorpus::default();
    for (row, cached) in rows.iter().enumerate() {
        for run in &cached.runs {
            if !corpus.text.is_empty() {
                corpus.text.push(' ');
            }
            let text_start = corpus.text.len();
            corpus.text.push_str(&run.text);
            corpus.spans.push(CorpusSpan {
                text_start,
                text_end: corpus.text.len(),
                row,
                start_col: run.start_col,
                end_col: run.start_col + run.width,
                linear_columns: run.linear_columns,
            });
        }
    }
    let (folded, offsets) = fold_case_with_offsets(&corpus.text);
    corpus.lower = folded;
    corpus.fold_offsets = offsets;
    corpus
}

fn build_search_corpus(lines: &[String]) -> SearchCorpus {
    let rows: Vec<CachedSearchRow> = lines
        .iter()
        .map(|line| CachedSearchRow::parse(line))
        .collect();
    assemble_search_corpus(&rows)
}

/// Frame-content-keyed corpus cache owned by [`SearchState`].
///
/// The caller hands [`refresh_search`] the whole frame on every rendered
/// frame and `TuiCore` bumps its frame version on every render, so a
/// version-keyed cache never hits. This cache keys on the CONTENT instead:
/// each row is fingerprinted, and only rows whose raw line changed are
/// ANSI-stripped and re-split. A refresh of unchanged content does no corpus
/// work at all — the assembled corpus, its lowercased haystack and the match
/// set are all reused. The cache is cleared when the query changes and when
/// the search overlay closes (see [`SearchState::close`]).
#[derive(Debug, Default)]
pub struct SearchFrameCache {
    /// Normalized query the cached corpus/matches belong to (empty = none).
    query: String,
    /// One entry per transcript row of the last prepared frame.
    rows: Vec<CachedSearchRow>,
    /// Assembled corpus (spans + lowercased haystack) for `rows`.
    corpus: SearchCorpus,
    corpus_valid: bool,
    matches_valid: bool,
    row_rebuilds: u64,
    corpus_builds: u64,
    match_runs: u64,
}

impl SearchFrameCache {
    /// Drop every cached row, corpus and match set (a query edit or the
    /// overlay closing). The cumulative diagnostics counters survive.
    pub fn clear(&mut self) {
        self.query.clear();
        self.rows.clear();
        self.corpus = SearchCorpus::default();
        self.corpus_valid = false;
        self.matches_valid = false;
    }

    /// Rows re-parsed since construction (test seam: a one-row change must
    /// move this by exactly one).
    pub fn row_rebuilds(&self) -> u64 {
        self.row_rebuilds
    }

    /// Full corpus assemblies since construction.
    pub fn corpus_builds(&self) -> u64 {
        self.corpus_builds
    }

    /// Match scans since construction.
    pub fn match_runs(&self) -> u64 {
        self.match_runs
    }

    /// Rows currently cached (0 after [`SearchFrameCache::clear`]).
    pub fn cached_rows(&self) -> usize {
        self.rows.len()
    }

    /// Whether the cached match set already answers `query`.
    pub fn is_current(&self, query: &str) -> bool {
        self.matches_valid && self.query == normalize_query(query)
    }

    /// Bring the cache up to date with `lines` / `query` when the caller does
    /// not know which rows changed. `Some(matches)` when the query or some row
    /// changed and the caller must publish the new match set; `None` when the
    /// cached set still applies.
    fn prepare(&mut self, lines: &[String], query: &str) -> Option<Vec<SearchMatch>> {
        self.prepare_rows(lines, query, RowChange::Unknown)
    }

    /// [`Self::prepare`] with the renderer's changed-row hint: `Some((first,
    /// last))` re-fingerprints only that inclusive range, `None` trusts that
    /// no row changed (a query edit or a length change still forces a full
    /// pass). The hint is what keeps a frame that only moved a dock row from
    /// hashing the whole transcript.
    fn prepare_with_hint(
        &mut self,
        lines: &[String],
        query: &str,
        changed: Option<(usize, usize)>,
    ) -> Option<Vec<SearchMatch>> {
        let change = match changed {
            Some((first, last)) => RowChange::Range(first, last),
            None => RowChange::None,
        };
        self.prepare_rows(lines, query, change)
    }

    fn prepare_rows(
        &mut self,
        lines: &[String],
        query: &str,
        change: RowChange,
    ) -> Option<Vec<SearchMatch>> {
        let normalized = normalize_query(query);
        if normalized != self.query {
            // A query edit drops the corpus cache (upstream recomputes from
            // scratch). Rows are query-independent, but one clear rule 鈥?as
            // the overlay closing 鈥?keeps the cache's lifetime honest, and a
            // query edit only costs the same one-pass parse the old code paid
            // on every frame anyway.
            self.clear();
            self.query = normalized.clone();
        }
        if normalized.is_empty() {
            if self.matches_valid {
                return None;
            }
            self.matches_valid = true;
            return Some(Vec::new());
        }
        let rows_changed = match change {
            RowChange::Unknown => self.sync_rows_full(lines),
            RowChange::None if self.rows.len() == lines.len() => false,
            RowChange::None => self.sync_rows_full(lines),
            RowChange::Range(first, last)
                if self.rows.len() == lines.len() && !lines.is_empty() =>
            {
                self.sync_rows_range(lines, first, last)
            }
            RowChange::Range(..) => self.sync_rows_full(lines),
        };
        if rows_changed {
            self.corpus_valid = false;
            self.matches_valid = false;
        }
        if !self.corpus_valid {
            self.corpus = assemble_search_corpus(&self.rows);
            self.corpus_valid = true;
            self.corpus_builds += 1;
        }
        if self.matches_valid {
            return None;
        }
        self.matches_valid = true;
        self.match_runs += 1;
        Some(find_corpus_matches(&self.corpus, &normalized))
    }

    /// Reuse the cached parse of every row whose raw content is unchanged;
    /// returns whether any row was added, removed or re-parsed.
    fn sync_rows_full(&mut self, lines: &[String]) -> bool {
        let mut changed = false;
        let common = self.rows.len().min(lines.len());
        for (index, line) in lines.iter().take(common).enumerate() {
            if self.rows[index].unchanged(line) {
                continue;
            }
            self.rows[index] = CachedSearchRow::parse(line);
            self.row_rebuilds += 1;
            changed = true;
        }
        if self.rows.len() > lines.len() {
            self.rows.truncate(lines.len());
            changed = true;
        }
        for line in &lines[common..] {
            self.rows.push(CachedSearchRow::parse(line));
            self.row_rebuilds += 1;
            changed = true;
        }
        changed
    }

    /// Re-fingerprint only the inclusive `[first, last]` row range (clamped to
    /// the frame). Lengths are equal (checked by the caller), so unchanged
    /// rows outside the range keep their cached parse.
    fn sync_rows_range(&mut self, lines: &[String], first: usize, last: usize) -> bool {
        let last = last.min(lines.len() - 1);
        let first = first.min(last);
        let mut changed = false;
        for (index, line) in lines.iter().enumerate().take(last + 1).skip(first) {
            if self.rows[index].unchanged(line) {
                continue;
            }
            self.rows[index] = CachedSearchRow::parse(line);
            self.row_rebuilds += 1;
            changed = true;
        }
        changed
    }
}

/// Which rows of a frame may have changed since the last [`SearchFrameCache`]
/// prepare (see [`SearchFrameCache::prepare_with_hint`]).
#[derive(Debug, Clone, Copy)]
enum RowChange {
    /// No hint: re-fingerprint every row.
    Unknown,
    /// The caller proved no row changed (row counts still checked).
    None,
    /// Only this inclusive range may have changed.
    Range(usize, usize),
}

/// Collapse whitespace runs and trim (upstream `normalizeQuery`).
pub fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Hard cap on one query's match collection. A one-character query over a
/// large band could otherwise allocate a `SearchMatch` (with its segment
/// vector) per character; 5k matches is far beyond what the `n/m` UI or a
/// user can navigate, and keeps the per-query allocation bounded without a
/// second pass to count the rest.
pub const SEARCH_MATCH_LIMIT: usize = 5000;

/// Case-insensitive literal search over the corpus (upstream builds a
/// `giu` regex from the escaped query; pir implements the same
/// canonicalization — per-code-point Unicode case folding on both sides —
/// with a byte-substring scan, so non-ASCII letters match regardless of case
/// while `ß`/`SS` style full foldings do not, exactly like the `iu` regex).
fn find_corpus_matches(corpus: &SearchCorpus, normalized: &str) -> Vec<SearchMatch> {
    if normalized.is_empty() {
        return Vec::new();
    }
    let haystack = &corpus.lower;
    let needle = fold_case(normalized);
    let needle_bytes = needle.as_bytes();
    let mut matches = Vec::new();
    if needle_bytes.is_empty() {
        return matches;
    }
    let haystack_bytes = haystack.as_bytes();
    let mut span_index = 0usize;
    let mut search_from = 0usize;
    loop {
        if search_from >= haystack.len() {
            break;
        }
        // Byte slicing panics on a non-char-boundary index; advance to the
        // next boundary (UTF-8 is self-synchronizing, so a needle match can
        // only start at a boundary — this guards the +1 resume step).
        while search_from < haystack.len() && !haystack.is_char_boundary(search_from) {
            search_from += 1;
        }
        if search_from >= haystack.len() {
            break;
        }
        let Some(relative) = find_substring(&haystack_bytes[search_from..], needle_bytes) else {
            break;
        };
        let folded_start = search_from + relative;
        let folded_end = folded_start + needle.len();
        // Resume one char after the match start.
        search_from = (folded_start + 1).min(haystack.len()).max(search_from + 1);
        // Translate the folded byte range back to the ORIGINAL corpus text:
        // spans and segments index `corpus.text` bytes.
        let (start, end) = match &corpus.fold_offsets {
            None => (folded_start, folded_end),
            Some(offsets) => {
                let original_start = offsets.get(folded_start).copied().unwrap_or(0) as usize;
                let original_end = offsets
                    .get(folded_end)
                    .copied()
                    .map(|offset| offset as usize)
                    .unwrap_or(corpus.text.len());
                (original_start, original_end)
            }
        };
        // Advance to the first span that can overlap the match.
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= start {
            span_index += 1;
        }
        let mut segments: Vec<SearchSegment> = Vec::new();
        for span in &corpus.spans[span_index.min(corpus.spans.len())..] {
            if span.text_start >= end {
                break;
            }
            if span.text_end <= start {
                continue;
            }
            let (start_col, end_col) = if span.linear_columns {
                (
                    span.start_col + start.saturating_sub(span.text_start),
                    span.start_col + end.min(span.text_end) - span.text_start,
                )
            } else {
                (span.start_col, span.end_col)
            };
            match segments.last_mut() {
                Some(previous) if previous.row == span.row && start_col <= previous.end_col => {
                    previous.end_col = previous.end_col.max(end_col);
                }
                _ => segments.push(SearchSegment {
                    row: span.row,
                    start_col,
                    end_col,
                }),
            }
        }
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= end {
            span_index += 1;
        }
        if !segments.is_empty() {
            matches.push(SearchMatch { segments });
            if matches.len() >= SEARCH_MATCH_LIMIT {
                // The count text and the navigation both stop here; a capped
                // set is a bounded, truthful prefix of the full match list.
                break;
            }
        }
    }
    matches
}

/// Plain byte-substring search (no `memchr` dependency here).
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Search the rendered frame lines (upstream `findAltScreenSearchMatches`).
pub fn find_search_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    let normalized = normalize_query(query);
    if normalized.is_empty() {
        return Vec::new();
    }
    find_corpus_matches(&build_search_corpus(lines), &normalized)
}

// ---------------------------------------------------------------------------
// ANSI column splicing (match / selection paint)
// ---------------------------------------------------------------------------

/// Wrap visible-cell columns `[start, end)` of an ANSI line with `open` /
/// `close` sequences, preserving the surrounding SGR state (upstream
/// `applySearchTextHighlight` re-opens the active codes after a highlight
/// closes). Ranges must be sorted and non-overlapping. Column boundaries are
/// grapheme clusters — a ZWJ emoji, flag pair or wide CJK cell is painted
/// whole, never split mid-cluster.
pub fn splice_columns(line: &str, ranges: &[(usize, usize, &'static str, &'static str)]) -> String {
    if ranges.is_empty() {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + ranges.len() * 12);
    let mut col = 0usize;
    let mut next_range = 0usize;
    // The range currently painted: `(end_col, close_seq)`.
    let mut open: Option<(usize, &'static str)> = None;
    let mut active_codes: Vec<String> = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy the escape sequence verbatim; remember SGR codes so the
            // style can be restored after a highlight closes.
            let mut seq = String::from("\x1b");
            if chars.peek() == Some(&'[') {
                seq.push(chars.next().unwrap_or('\x1b'));
                for ch in chars.by_ref() {
                    seq.push(ch);
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
                if seq.ends_with('m') && seq.len() > 2 {
                    active_codes.push(seq.clone());
                }
            } else if matches!(chars.peek(), Some(']') | Some('_')) {
                seq.push(chars.next().unwrap_or('\x1b'));
                let mut prev = '\0';
                for ch in chars.by_ref() {
                    seq.push(ch);
                    if ch == '\x07' || (prev == '\x1b' && ch == '\\') {
                        break;
                    }
                    prev = ch;
                }
            }
            out.push_str(&seq);
            continue;
        }
        // The plain run up to the next escape sequence: escape sequences are
        // never part of a grapheme cluster, so each run segments on its own.
        let mut run = String::new();
        run.push(c);
        while let Some(&next) = chars.peek() {
            if next == '\x1b' {
                break;
            }
            run.push(next);
            chars.next();
        }
        // Iterate the clusters directly instead of materializing
        // `ansi::grapheme_clusters`' `Vec<&str>`: the run is consumed once,
        // left to right (the decoration path hits this for every painted row).
        for cluster in UnicodeSegmentation::graphemes(run.as_str(), true) {
            // Close the painted range when this cluster starts at or past
            // its end.
            if let Some((end, close)) = open {
                if col >= end {
                    out.push_str(close);
                    for code in &active_codes {
                        out.push_str(code);
                    }
                    open = None;
                }
            }
            // Open the next range when it starts at or before this cluster.
            // A range entirely left of the line (zero-width start) is
            // skipped.
            while next_range < ranges.len() && ranges[next_range].0 <= col {
                let (_, end, open_seq, close_seq) = ranges[next_range];
                next_range += 1;
                if end > col && open.is_none() {
                    out.push_str(open_seq);
                    open = Some((end, close_seq));
                }
            }
            out.push_str(cluster);
            let width = if cluster == "\t" {
                3
            } else {
                ansi::grapheme_width(cluster)
            };
            col += width.max(1);
        }
    }
    // A range extending past the line end stays painted to the end.
    if let Some((_, close)) = open {
        out.push_str(close);
        for code in &active_codes {
            out.push_str(code);
        }
    }
    out
}

/// Slice visible columns `[start, end)` of a PLAIN text line (already
/// ANSI-stripped) into a new string — port of upstream `sliceByColumn(line,
/// start, end - start, true)` (`utils.ts:1263-1265` over `sliceWithWidth`):
/// columns are measured over grapheme clusters, and a cluster is kept only
/// when it lies fully inside the range, so a ZWJ emoji, a flag pair or a
/// wide CJK cell is never cut in half.
pub fn slice_columns(plain: &str, start: usize, end: usize) -> String {
    if end <= start {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    // Same lazy iteration as `splice_columns`: clusters are consumed in order
    // with an early stop, so the intermediate vec bought nothing.
    for cluster in UnicodeSegmentation::graphemes(plain, true) {
        if col >= end {
            break;
        }
        let width = if cluster == "\t" {
            3
        } else {
            ansi::grapheme_width(cluster)
        };
        if col >= start && col + width <= end {
            out.push_str(cluster);
        }
        col += width;
    }
    out
}

// ---------------------------------------------------------------------------
// Interaction state
// ---------------------------------------------------------------------------

/// Frame-decoration context computed by the renderer (see
/// [`TuiCore::set_decorator`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDecorateContext {
    /// Terminal width in columns.
    pub width: usize,
    /// `true` when painting a scrolled window (transcript slice + dock)
    /// instead of the full frame.
    pub windowed: bool,
    /// Absolute buffer row painted at the top of the transcript area
    /// (0 when windowed; `previous_viewport_top` otherwise).
    pub window_start: usize,
    /// Index within the painted lines where the dock begins
    /// (`lines.len()` when the dock is absent).
    pub dock_start: usize,
    /// Index within the painted lines of the first visible row
    /// (0 when windowed; the viewport top otherwise).
    pub visible_top: usize,
    /// Absolute buffer row one past the last transcript row
    /// (`frame_len - dock_height`).
    pub transcript_end: usize,
    /// Current scrollback offset in rows (0 = follow the end).
    pub scroll_offset: usize,
}

/// Scrollbar visibility (upstream `fullscreenScrollbar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarMode {
    Auto,
    Always,
    Hidden,
}

/// Resolved alt-screen interaction settings (upstream `TuiAltScreenOptions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltScreenConfig {
    /// Scrollbar visibility (upstream `fullscreenScrollbar`).
    pub scrollbar: ScrollbarMode,
    /// Copy the selection to the clipboard on mouse-up (upstream
    /// `fullscreenCopyOnSelect`).
    pub copy_on_select: bool,
}

impl Default for AltScreenConfig {
    fn default() -> Self {
        // Upstream SETTINGS_DEFAULTS: fullscreenScrollbar "auto",
        // fullscreenCopyOnSelect true.
        Self {
            scrollbar: ScrollbarMode::Auto,
            copy_on_select: true,
        }
    }
}

/// Live cell for the resolved `fullscreen*` settings.
///
/// The TUI installs one at startup and reads it per frame / per event instead
/// of capturing the startup snapshot: `/settings` can change
/// `fullscreenScrollbar` (= `ScrollbarMode`) and `fullscreenCopyOnSelect`
/// after a runtime `switchTuiMode`, and the very next frame must honor the
/// new values (upstream `settingsManager.getFullscreen*()` getters).
#[derive(Debug)]
pub struct LiveAltScreenConfig {
    config: std::sync::Mutex<AltScreenConfig>,
}

impl LiveAltScreenConfig {
    pub fn new(config: AltScreenConfig) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
        }
    }

    /// The current resolved values (cheap lock + copy).
    pub fn snapshot(&self) -> AltScreenConfig {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Apply `fullscreenScrollbar` live (upstream `onFullscreenScrollbarChange`).
    pub fn set_scrollbar(&self, scrollbar: ScrollbarMode) {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .scrollbar = scrollbar;
    }

    /// Apply `fullscreenCopyOnSelect` live (upstream
    /// `onFullscreenCopyOnSelectChange`).
    pub fn set_copy_on_select(&self, enabled: bool) {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .copy_on_select = enabled;
    }
}

/// Whether the transcript must reserve its last column for the scrollbar
/// (`fullscreen && fullscreenScrollbar == "always"`). Re-evaluated by the
/// runtime mode switch and by a live `fullscreenScrollbar` change.
pub fn transcript_reserves_scrollbar(fullscreen: bool, config: &AltScreenConfig) -> bool {
    fullscreen && config.scrollbar == ScrollbarMode::Always
}

/// Transcript search state (upstream `ActiveSearch`, input-model reduced to
/// append/backspace — upstream hosts a full editor in the search overlay).
#[derive(Debug, Default)]
pub struct SearchState {
    /// Whether the search box is open.
    pub active: bool,
    /// The raw query as typed.
    pub query: String,
    /// Matches against the last-refreshed frame.
    pub matches: Vec<SearchMatch>,
    /// The current match index; `None` = nothing selected yet.
    pub selected: Option<usize>,
    /// The frame version the last refresh was accepted for. This is only the
    /// caller's change signal; the corpus/matches themselves are keyed by
    /// frame CONTENT (see [`SearchFrameCache`]) — `TuiCore` advances the
    /// version on every render, unchanged or not.
    pub frame_version: u64,
    /// Content-keyed corpus/match cache. Survives frames (only changed rows
    /// are re-stripped) and is cleared on a query edit or when the overlay
    /// closes.
    pub cache: SearchFrameCache,
    /// The navigation button the pointer is over (upstream
    /// `AltScreenSearchComponent.hoveredNavigationDirection`): drives the
    /// underline of `searchNavigationButtonStyle(text, hovered)`.
    pub hovered_nav: Option<SearchNav>,
    /// The live backward paging walk of the query (windowed huge sessions;
    /// see [`SearchWalk`]). Windowed mode only: a regular session's whole
    /// transcript is materialized, so there is nothing to walk.
    pub walk: SearchWalk,
    /// Viewport offset from before the box opened (`None` = the box was not
    /// opened through [`route_alt_screen_key`], e.g. a test seeding `active`).
    /// A query edit that leaves zero matches returns the view here instead of
    /// leaving it at a position a previous prefix's reveal scrolled it to.
    pub origin_offset: Option<usize>,
    /// Whether the pre-search view was held at the end without following
    /// ([`TuiCore::follow_suppressed`]); restored together with
    /// [`SearchState::origin_offset`].
    pub origin_pinned: bool,
    /// Whether the live search has moved the viewport (a reveal). Only then
    /// may a zero-match edit restore the pre-search viewport — a query typed
    /// entirely without matches never moves it in the first place.
    pub revealed: bool,
}

impl SearchState {
    pub fn selected_match(&self) -> Option<&SearchMatch> {
        self.selected
            .and_then(|selected| self.matches.get(selected))
    }

    /// Record the pre-search viewport (the box just opened). The origins are
    /// kept for the whole search session: a query that ends up matching
    /// nothing must not leave the view where an intermediate prefix's reveal
    /// put it.
    pub fn begin(&mut self, offset: usize, pinned: bool) {
        self.origin_offset = Some(offset);
        self.origin_pinned = pinned;
        self.revealed = false;
    }

    /// A query edit left zero matches: undo the viewport move the search's
    /// reveal made, so the view stays where it was before searching (the
    /// report's "typing `zzz` at the follow-end scrolled up ~11 rows" — the
    /// first `z` matched `zeta` and jumped, then `zz`/`zzz` had no match).
    /// No-op when the search never moved the view or the box was not opened
    /// through the router.
    fn restore_origin(&mut self, core: &mut TuiCore) {
        if !self.revealed {
            return;
        }
        self.revealed = false;
        if let Some(offset) = self.origin_offset {
            core.scroll_to(offset, self.origin_pinned);
        }
    }

    /// `n/m` display text (upstream `AltScreenSearchComponent.render`), with
    /// the backward walk's honest states: `…` while a page is on the wire or
    /// a materialized page awaits its band check; "No match so far" when the
    /// walk stopped at its bound ("No matches" would claim the whole session
    /// was searched); and the upstream "No matches" once the walk reached the
    /// session start (or the session has nothing older to read at all).
    pub fn result_text(&self) -> String {
        if self.query.trim().is_empty() {
            String::new()
        } else if !self.matches.is_empty() {
            if let Some(selected) = self.selected {
                format!("{}/{}", selected + 1, self.matches.len())
            } else {
                format!("/{}", self.matches.len())
            }
        } else if self.walk.is_walking() {
            "\u{2026}".to_string()
        } else if self.walk.outcome() == Some(SearchWalkOutcome::Capped) {
            "No match so far".to_string()
        } else {
            "No matches".to_string()
        }
    }

    /// Upstream `setHoveredNavigationDirection`: `true` when the hover changed
    /// (the caller repaints).
    pub fn set_hovered_navigation(&mut self, direction: Option<SearchNav>) -> bool {
        if direction == self.hovered_nav {
            return false;
        }
        self.hovered_nav = direction;
        true
    }

    /// Close the box, clearing the query and every transient affordance
    /// (upstream `closeSearch` drops the whole `ActiveSearch`). An in-flight
    /// backward walk dies with it: its page reply then lands as an ordinary
    /// load-more chunk (see [`SearchWalk::take_reply`]).
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.selected = None;
        self.hovered_nav = None;
        self.cache.clear();
        self.walk.cancel();
        self.origin_offset = None;
        self.origin_pinned = false;
        self.revealed = false;
    }
}

/// Search navigation direction (upstream `AltScreenSearchComponent.
/// getNavigationDirectionAt`'s `-1 | 1`): the previous/next match affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchNav {
    /// `↑ <tui.altScreen.searchPrevious>` — the previous match.
    Previous,
    /// `↓ <tui.altScreen.searchNext>` — the next match.
    Next,
}

impl SearchNav {
    /// The `navigate_search` direction sign (upstream `navigateSearch`'s
    /// `direction: -1 | 1`).
    pub fn delta(self) -> i32 {
        match self {
            SearchNav::Previous => -1,
            SearchNav::Next => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Backward paging walk (windowed sessions; see the module header)
// ---------------------------------------------------------------------------

/// Page bound of ONE backward search walk: how many older pages (resident
/// probe slices + disk pages) a single query may walk before the walk stops
/// at its bound — the empty state then stays partial
/// ([`SearchWalkOutcome::Capped`]).
///
/// The search box is live-typed, so the walk must be bounded: sixteen page
/// reads per query, with the byte bound below stopping a session of many tiny
/// pages earlier (the load-more page the loop reads is itself capped at 1 MiB,
/// so a huge session usually spends the byte budget first). A walk stopped
/// here reports a partial state — never the whole-session "No matches"
/// (see [`SearchWalkOutcome::Capped`]).
pub const SEARCH_WALK_MAX_PAGES: usize = 16;

/// Byte bound of ONE backward search walk, in payload bytes — the same
/// measure the transcript window budgets in (`entry_payload_bytes`). The walk
/// materializes the page it finds, so its bound is kept at the order of the
/// window's own byte budget: a query never grows the materialized band past
/// what scrolling the same distance would.
pub const SEARCH_WALK_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Why a backward search walk stopped (`None` = it has not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchWalkOutcome {
    /// The walk reached the start of the session (or the session offers no
    /// older pages at all): the whole history was searched, so the upstream
    /// "No matches" is the honest empty state.
    Exhausted,
    /// The walk materialized a page holding a match and the band search
    /// confirmed it (`finish`), or the materialized band holds matches
    /// regardless of the walk (`settle`). The box shows `n/m` while those
    /// matches stay materialized; if scrolling unmaterializes them the box
    /// falls back to the loaded-content empty state, exactly like upstream
    /// searching its scroll content.
    Found,
    /// The walk stopped at its bound ([`SEARCH_WALK_MAX_PAGES`] /
    /// [`SEARCH_WALK_MAX_BYTES`]) or a page read failed: older history was
    /// NOT searched, and the box must not claim it was.
    Capped,
}

/// What the loop must do next to advance the live search's backward walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchWalkPlan {
    /// Nothing to do.
    Idle,
    /// Probe the resident prefix the window does not materialize (the loop
    /// takes a page-sized slice immediately above the band top and answers
    /// through [`SearchWalk::note_probe`]). Those entries are already in
    /// memory: re-reading them from disk is impossible (the disk cursor sits
    /// BELOW them), so a match there would otherwise be unfindable.
    ProbeResident,
    /// Read the next older page at `cursor` (`WorkerCommand::LoadOlder`); the
    /// walk has already been tagged for the reply ([`SearchWalk::take_reply`]).
    ReadPage { cursor: u64 },
}

/// The live backward paging walk of an open search box (windowed sessions): a
/// query with no match in the materialized band probes the resident prefix the
/// window does not materialize, then pulls older pages off disk
/// (`WorkerCommand::LoadOlder`) until it finds the newest page holding a
/// match, reaches the session start, or hits its bound.
///
/// Nothing new is materialized while the walk searches — the pages are probed
/// in a scratch render (the loop's `probe_older_page`) — so a walk over a
/// huge session costs one render of each page it reads, not a growing band.
/// The page holding the match is then materialized in the band (the window's
/// top held, see [`search_holds_band`]) and the ordinary band search the
/// caller runs after the next render is the authority: it fills
/// `SearchState::matches` and the reveal scrolls to it, so `n/m` counts
/// exactly the matches of the content the walk loaded.
///
/// Reads are serialized with the transcript's own scroll-up load-more (the
/// loop shares one in-flight flag), so at most one page is ever on the wire
/// and [`SearchWalk::take_reply`] can attribute a reply to (only) this walk.
/// Every query edit re-arms the walk ([`SearchState::walk`] is bumped by
/// `start`), which is also the cancellation: a reply from the superseded walk
/// lands as a plain load-more chunk and can never complete the new walk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchWalk {
    /// Query the walk belongs to; empty = no walk.
    query: String,
    /// Bumped by every arm/cancel: a reply that lands after the bump belongs
    /// to an older walk (or to the scroll-up path) and is not this walk's page.
    generation: u64,
    /// The generation of the page read currently on the wire (`None` = none).
    in_flight: Option<u64>,
    /// Entries above the band top this walk has already probed, as a DISTANCE
    /// (a prepend shifts the band top and stays valid; a page the walk did not
    /// probe — the scroll-up path's, or a release that grew the region at its
    /// bottom — resets it, see [`SearchWalk::reset_probe`]).
    probed_entries: usize,
    /// Pages probed/read by this walk (resident slices count as pages).
    pages: usize,
    /// Payload bytes probed/read by this walk.
    bytes: usize,
    /// A found page is materialized and waits for the band check
    /// (`finish_search_walk_check`).
    completing: bool,
    /// How the walk stopped (`None` = still walking or never armed).
    outcome: Option<SearchWalkOutcome>,
    /// The walk materialized a page into the band: its top must be held while
    /// the reveal is on screen ([`search_holds_band`]).
    revealed: bool,
}

impl SearchWalk {
    /// Arm the walk for `query` — a query edit, so any earlier walk of the
    /// box (and its page reply) is superseded.
    pub fn start(&mut self, query: &str) {
        self.generation = self.generation.wrapping_add(1);
        self.query = query.to_owned();
        self.reset_progress();
    }

    /// Abandon the walk (box closed, dialog focused, conversation reset).
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.query.clear();
        self.reset_progress();
    }

    fn reset_progress(&mut self) {
        self.in_flight = None;
        self.probed_entries = 0;
        self.pages = 0;
        self.bytes = 0;
        self.completing = false;
        self.outcome = None;
        self.revealed = false;
    }

    /// The walk is live: armed and not stopped. The box shows `…` — a page is
    /// on the wire, a page is being checked, or the next probe/read is one
    /// frame away.
    pub fn is_walking(&self) -> bool {
        !self.query.is_empty() && self.outcome.is_none()
    }

    /// A found page is materialized and awaits the post-render band check.
    pub fn is_completing(&self) -> bool {
        self.completing
    }

    /// How the walk stopped (`None` while it is live or was never armed).
    pub fn outcome(&self) -> Option<SearchWalkOutcome> {
        self.outcome
    }

    /// Whether the walk materialized a page into the band (the loop holds the
    /// window's top — [`search_holds_band`]).
    pub fn holds_band(&self) -> bool {
        self.revealed
    }

    /// The query the walk belongs to (empty = none).
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Entries above the band top this walk has probed (see
    /// [`SearchWalk::note_probe`]).
    pub fn probed_entries(&self) -> usize {
        self.probed_entries
    }

    /// Pages probed/read by this walk.
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// Payload bytes probed/read by this walk.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Whether a page read is on the wire for THIS walk.
    pub fn request_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether the walk may take one more page (both bounds still hold).
    pub fn budget_left(&self) -> bool {
        self.pages < SEARCH_WALK_MAX_PAGES && self.bytes < SEARCH_WALK_MAX_BYTES
    }

    /// Tag the request the loop is about to send (`plan_search_walk` calls
    /// this before returning [`SearchWalkPlan::ReadPage`]).
    pub fn note_request(&mut self) {
        self.in_flight = Some(self.generation);
    }

    /// Consume one page reply: `true` when it belongs to this walk. At most
    /// one read is ever on the wire (the loop's shared in-flight flag), so the
    /// reply is this walk's exactly when the walk tagged a request and no
    /// edit/cancel has since bumped the generation.
    pub fn take_reply(&mut self) -> bool {
        matches!(
            self.in_flight.take(),
            Some(generation) if generation == self.generation
        )
    }

    /// Count one loaded disk page: `bytes` of payload the loop read, and
    /// `inserted` entries prepended at the band's front. The page was probed
    /// by the walk, and a disk read only happens once the whole resident
    /// prefix above the band is probed — so the inserted entries extend the
    /// probed region exactly (see [`SearchWalk::probed_entries`]); without
    /// this the next plan step would re-probe the page it just read.
    pub fn note_page(&mut self, bytes: usize, inserted: usize) {
        self.pages = self.pages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.probed_entries = self.probed_entries.saturating_add(inserted);
    }

    /// Count one probed resident slice (`entries` above the band top).
    pub fn note_probe(&mut self, entries: usize, bytes: usize) {
        self.probed_entries = self.probed_entries.saturating_add(entries);
        self.pages = self.pages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }

    /// The transcript window grew new entries immediately above the band top
    /// that this walk did NOT probe — a release / a return to the follow
    /// window moved the band top up, or the scroll-up load-more prepended a
    /// page of its own: the probed distance no longer describes "the newest N
    /// entries above the band", so restart it. Safe (the walk re-probes
    /// newest-first, never skipping), bounded by the walk's own budget.
    pub fn reset_probe(&mut self) {
        self.probed_entries = 0;
    }

    /// A found page is materialized into the band and awaits the band check.
    pub fn note_reveal(&mut self) {
        self.completing = true;
        self.revealed = true;
    }

    /// The band check did not confirm the probe (the render disagreed, or the
    /// window dropped the page): keep walking.
    pub fn resume(&mut self) {
        self.completing = false;
    }

    /// The band check found the matches: the walk is done.
    pub fn finish(&mut self) {
        self.completing = false;
        self.in_flight = None;
        self.outcome = Some(SearchWalkOutcome::Found);
    }

    /// The materialized band holds matches (this walk's page, the live stream,
    /// or a plain query edit) — the walk is moot and must not read further.
    pub fn settle(&mut self) {
        if self.query.is_empty() {
            return;
        }
        self.in_flight = None;
        self.completing = false;
        self.outcome = Some(SearchWalkOutcome::Found);
    }

    /// Stop without a match: `Exhausted` when there is nothing older to read,
    /// `Capped` when the bound (or a failed read) ended the walk.
    pub fn stop(&mut self, outcome: SearchWalkOutcome) {
        self.in_flight = None;
        self.completing = false;
        self.outcome = Some(outcome);
    }
}

/// Decide the next step of the live backward walk (see [`SearchWalk`]). Pure:
/// the loop passes the transcript's load-more state in and performs the
/// returned action. `band_start` is the window's first materialized entry
/// index (`TranscriptWindow::start`), so `band_start > walk.probed_entries()`
/// means the resident prefix above the band still holds unprobed entries.
///
/// The walk never advances while another read is on the wire — the shared
/// single-read invariant the reply attribution relies on — and stops rather
/// than spinning when the session offers no older page or the bound is spent.
pub fn plan_search_walk(
    search: &mut SearchState,
    windowed: bool,
    band_start: usize,
    cursor: Option<u64>,
    request_in_flight: bool,
) -> SearchWalkPlan {
    let active = search.active;
    let query_empty = normalize_query(&search.query).is_empty();
    let has_matches = !search.matches.is_empty();
    let walk = &mut search.walk;
    if !active || query_empty {
        // A closed box / an emptied query has no walk. `SearchState::close`
        // already cancels; this also covers backspace-to-empty.
        walk.cancel();
        return SearchWalkPlan::Idle;
    }
    if has_matches {
        // The band holds matches now (the walk's own page, live content, or
        // an earlier reveal): the walk is done.
        walk.settle();
        return SearchWalkPlan::Idle;
    }
    if !windowed || !walk.is_walking() || walk.is_completing() {
        // A regular session has nothing unmaterialized to search; a stopped
        // or completing walk has nothing to advance here (the post-render
        // band check resumes a completing one).
        return SearchWalkPlan::Idle;
    }
    if walk.request_in_flight() || request_in_flight {
        return SearchWalkPlan::Idle;
    }
    if band_start > walk.probed_entries() && walk.budget_left() {
        return SearchWalkPlan::ProbeResident;
    }
    match cursor {
        Some(cursor) if walk.budget_left() => {
            walk.note_request();
            SearchWalkPlan::ReadPage { cursor }
        }
        _ => {
            // `Exhausted` renders as "No matches" — a whole-session claim, so it
            // may only be made when the whole session really was searched: the
            // disk walk reached the file start (`cursor.is_none()`) AND every
            // resident entry above the band was probed (`band_start <=
            // probed_entries`, the complement of the probe condition above). A
            // fully resident prefix that outran the page budget would otherwise
            // claim the session held no match while entries above the band were
            // never looked at.
            let whole_session = cursor.is_none() && band_start <= walk.probed_entries();
            walk.stop(if whole_session {
                SearchWalkOutcome::Exhausted
            } else {
                SearchWalkOutcome::Capped
            });
            SearchWalkPlan::Idle
        }
    }
}

/// Whether the materialized band's top must be held for the live search (the
/// loop sets it on the window before planning it each frame). Two reasons, in
/// order:
///
/// - a found page is materialized and waits for its post-render band check
///   (`SearchWalk::is_completing`): it must stay in the band for the check to
///   see it, even while the viewport still follows the live end — the reveal
///   has not moved it yet, and the window's tail branch would trim the found
///   page straight back out;
/// - after the reveal (`SearchWalk::holds_band`), while the view is NOT
///   following: the user is looking at the revealed match, and the window's
///   release rule must not unmaterialize it. A return to the live end
///   (`following`) releases the hold — the tail window is what the user is
///   looking at then, and the search's matches are recomputed over it.
pub fn search_holds_band(windowed: bool, following: bool, search: &SearchState) -> bool {
    if !windowed || !search.active {
        return false;
    }
    search.walk.is_completing() || (!following && search.walk.holds_band())
}

/// The post-render half of a walk page (the loop calls this right after the
/// frame that materialized the page was painted): the BAND search is the
/// authority — the probe only decided to materialize. Non-empty matches are
/// selected and revealed with the same [`scroll_to_match`] a query edit uses;
/// the selection is the LAST match, because the walk walked backward: every
/// match it can bring in lies above the rows the query edit's anchor was on,
/// which is exactly upstream `retarget_search`'s "last match before the
/// anchor" fallback (`retarget_search`'s `matches.len() - 1`).
///
/// `true` when the reveal moved the viewport (the caller repaints).
pub fn finish_search_walk_check(
    core: &mut TuiCore,
    state: &mut AltScreenState,
    dock_height: usize,
) -> bool {
    if !state.search.walk.is_completing() {
        return false;
    }
    let matches = {
        let frame = core.last_frame_lines();
        let dock = dock_height.min(frame.len());
        find_search_matches(&frame[..frame.len() - dock], &state.search.query)
    };
    if matches.is_empty() {
        // The probe's page did not survive the band render (the probes are a
        // presence heuristic — see the module header — and the window's own
        // rules can drop a page): keep walking from where the walk was.
        state.search.walk.resume();
        return false;
    }
    state.search.matches = matches;
    state.search.selected = Some(state.search.matches.len() - 1);
    // The matches correspond to the frame just painted: pin the version so
    // `refresh_search` does not recompute (and drop the selection) before the
    // reveal has been painted.
    state.search.frame_version = core.frame_version();
    state.search.walk.finish();
    scroll_to_match(core, state, dock_height);
    true
}

/// The search box's painted layout (upstream `AltScreenSearchComponent.render`
/// plus its overlay's `top-right`, `40%`/min-32, margin-1 placement): the box's
/// screen rect, the bottom rule's rule widths, and the navigation buttons'
/// hit rects — all in screen coordinates, so the decorator and the mouse router
/// agree cell for cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchBoxLayout {
    /// Screen row of the box's top rule.
    pub top: usize,
    /// Screen column of the box's left border.
    pub left: usize,
    /// Outer box width (borders included).
    pub width: usize,
    /// Inner width (`width - 2`).
    pub inner: usize,
    /// The previous-match button's label (upstream falls back to the bare
    /// arrow, then hides the buttons, as the box narrows).
    pub previous_label: Option<String>,
    /// The next-match button's label; `None` = no buttons painted.
    pub next_label: Option<String>,
    /// The separator between the two buttons.
    pub separator: String,
    /// Horizontal rule cells left of the buttons on the bottom rule.
    pub left_rule: usize,
    /// Horizontal rule cells right of the buttons.
    pub right_rule: usize,
    /// The previous button's screen columns `[start, end)`; `(0, 0)` when the
    /// buttons are hidden.
    pub previous: (usize, usize),
    /// The next button's screen columns `[start, end)`.
    pub next: (usize, usize),
}

/// Lay out the search box for a terminal `width` (upstream's overlay options:
/// `anchor: "top-right"`, `width: "40%"`, `minWidth: 32`, `margin: 1`).
/// `None` when the terminal cannot hold the box at all.
pub fn search_box_layout(width: usize) -> Option<SearchBoxLayout> {
    if width == 0 {
        return None;
    }
    let available = width.saturating_sub(SEARCH_BOX_MARGIN * 2);
    let box_width = (width * SEARCH_BOX_WIDTH_PERCENT / 100)
        .max(SEARCH_BOX_MIN_WIDTH)
        .min(available);
    if box_width < SEARCH_BOX_MIN_PAINT_WIDTH {
        return None;
    }
    let inner = box_width - 2;
    let left = width - SEARCH_BOX_MARGIN - box_width;
    let mut previous_label = format!(
        "{SEARCH_PREVIOUS_ARROW} {}",
        button_key_text("tui.altScreen.searchPrevious")
    );
    let mut next_label = format!(
        "{SEARCH_NEXT_ARROW} {}",
        button_key_text("tui.altScreen.searchNext")
    );
    let mut separator = " \u{b7} ".to_string();
    let outer_gap = 1;
    let available_controls = inner.saturating_sub(outer_gap * 2 + 1);
    let mut controls = ansi::visible_width(&previous_label)
        + ansi::visible_width(&separator)
        + ansi::visible_width(&next_label);
    if controls > available_controls {
        // Too narrow for the key labels: the bare arrows and a single space.
        previous_label = SEARCH_PREVIOUS_ARROW.to_string();
        next_label = SEARCH_NEXT_ARROW.to_string();
        separator = " ".to_string();
        controls = 3;
    }
    let show_buttons = controls <= available_controls;
    if !show_buttons {
        return Some(SearchBoxLayout {
            top: SEARCH_BOX_MARGIN,
            left,
            width: box_width,
            inner,
            previous_label: None,
            next_label: None,
            separator,
            left_rule: inner,
            right_rule: 0,
            previous: (0, 0),
            next: (0, 0),
        });
    }
    let right_rule = usize::from(inner > controls + outer_gap * 2);
    let left_rule = inner.saturating_sub(controls + outer_gap * 2 + right_rule);
    let previous_start = 1 + left_rule + outer_gap;
    let previous_end = previous_start + ansi::visible_width(&previous_label);
    let next_start = previous_end + ansi::visible_width(&separator);
    let next_end = next_start + ansi::visible_width(&next_label);
    Some(SearchBoxLayout {
        top: SEARCH_BOX_MARGIN,
        left,
        width: box_width,
        inner,
        previous_label: Some(previous_label),
        next_label: Some(next_label),
        separator,
        left_rule,
        right_rule,
        previous: (left + previous_start, left + previous_end),
        next: (left + next_start, left + next_end),
    })
}

/// Upstream `getSearchNavigationDirectionAt` +
/// `AltScreenSearchComponent.getNavigationDirectionAt`: the buttons live on
/// the box's bottom rule (the component's third row), so only a pointer there
/// can hover or click them.
pub fn search_navigation_direction_at(
    layout: &SearchBoxLayout,
    row: usize,
    column: usize,
) -> Option<SearchNav> {
    if row != layout.top + 2 {
        return None;
    }
    if layout.previous_label.is_some() && column >= layout.previous.0 && column < layout.previous.1
    {
        return Some(SearchNav::Previous);
    }
    if layout.next_label.is_some() && column >= layout.next.0 && column < layout.next.1 {
        return Some(SearchNav::Next);
    }
    None
}

/// An ordered selection span in absolute buffer coordinates
/// `(row, visible_col)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSpan {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

impl SelectionSpan {
    fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Shared alt-screen interaction state (search + selection + flash), owned by
/// the event loop and read by the frame decorator.
#[derive(Debug, Default)]
pub struct AltScreenState {
    pub search: SearchState,
    pub selection: Option<SelectionSpan>,
    /// In-progress mouse selection anchor (absolute buffer coordinates).
    selection_anchor: Option<(usize, usize)>,
    /// In-progress mouse selection focus (absolute buffer coordinates); the
    /// painted [`AltScreenState::selection`] is the ordered anchor/focus span.
    selection_focus: Option<(usize, usize)>,
    /// A drag actually moved: a plain click clears the selection instead of
    /// copying (upstream `selectionDragged`).
    selection_dragged: bool,
    /// The unit a double/triple click selected; a drag extends by the same
    /// unit (upstream `selectionGranularity`).
    selection_granularity: SelectionGranularity,
    /// The word/line range the click selected, kept across a drag so the
    /// extension stays word/line-aligned (upstream `selectionInitialRange`).
    selection_initial_range: Option<SelectionSpan>,
    /// Last word click, for double/triple-click detection (upstream
    /// `lastClick`). Cleared by a drag and by a scrollbar grab.
    last_click: Option<LastClick>,
    /// Upstream `TuiAltScreen.pressedUrl`: the OSC 8 hyperlink under the
    /// press point ([`osc8_link_at_column`] over the frame line the pointer's
    /// screen row shows), remembered until the release decides — a click that
    /// lands on the same cell opens it ([`AltMouseRoute::OpenUrl`]), a drag
    /// drops it.
    pressed_url: Option<String>,
    pub flash: Option<(String, Instant, Duration)>,
    scrollbar_linger: Option<Instant>,
    /// Upstream `TuiAltScreen.scrollbarHover`: the pointer is over the
    /// transcript's track (`ScrollView.isScrollbarActive`).
    scrollbar_hover: bool,
    /// Upstream `TuiAltScreen.scrollbarDrag`: the grab offset in rows while a
    /// scrollbar drag is live (one scrollable, so no view reference is kept).
    scrollbar_drag: Option<usize>,
    /// Upstream `hasOverlay()`: `true` while a dialog/selector owns the focus.
    /// The event loop sets it (the mouse gate already blocks non-wheel events
    /// while an overlay is open); it additionally clears the scrollbar hover
    /// and refuses a bar grab so a gesture can never straddle an overlay.
    overlay_focus: bool,
    /// Upstream `TuiAltScreen.scrollToEndIndicatorRect`: the painted
    /// jump-to-end label, kept for observability — the mouse router
    /// recomputes it from the painted layout, so a frame that predates a
    /// resize can never wedge a stale rect.
    scroll_to_end_rect: Option<TextRect>,
    /// Mirror of `TuiCore::follow_suppressed` for the frame decorator, which
    /// only sees the painted frame: the follow path hands it
    /// `scroll_offset: 0` whether or not the view is following, so the
    /// search-reveal pin (`scrollTo(disableFollow)`) would otherwise keep the
    /// jump-to-end indicator hidden. Refreshed by every routed key/mouse event
    /// and by [`scroll_to_match`]; the router itself reads the core directly.
    follow_pinned_at_end: bool,
    /// The terminal height the interaction layer last saw: set by every routed
    /// key/mouse event (`TuiCore::rows`) and by every painted frame (the
    /// windowed frame IS the screen; a follow frame with a positive
    /// `visible_top` spans `frame_len - visible_top` screen rows). The follow
    /// decorator needs it to resolve the screen top after
    /// `TuiCore::invalidate_frame` zeroes the ctx's `visible_top` for one full
    /// repaint — see [`follow_screen_top`] (the function's docs carry the why).
    /// `0` = not known yet (the session's first frame).
    screen_rows: usize,
    /// W11-C: the live keybinding map the viewport router resolves its ids
    /// against. The loop installs it once per session (`set_keybindings`), so
    /// a settings override for `tui.altScreen.*` reaches the router; `None`
    /// (tests, a host without settings) keeps the upstream-verbatim defaults
    /// in [`ALT_SCREEN_KEYBINDINGS`].
    keybindings: Option<crate::tui::keybindings::Keybindings>,
    /// W11-C: the half-page scroll delta, set by the latest routed key/mouse
    /// event (the router is the only writer). `None` = the generic formula
    /// (viewport / 2) applies. Pure seam for tests that cannot stand up a
    /// rendered frame.
    half_page_step: Option<usize>,
    /// W11-C: lines moved by the last viewport scroll ([`AltScreenState::scroll_step`]),
    /// so a test can pin the exact delta each `tui.altScreen.*` action used.
    last_scroll_step: Option<isize>,
}

impl AltScreenState {
    /// Show a transient message (upstream `TuiAltScreen.flash` /
    /// `AltScreenFlashContainer.flash`).
    pub fn flash(&mut self, message: impl Into<String>, duration: Duration) {
        self.flash = Some((message.into(), Instant::now(), duration));
    }

    /// Install the live keybinding map the viewport router resolves against
    /// (W11-C). A `tui.altScreen.*` settings override then applies to the
    /// routed action instead of being ignored.
    pub fn set_keybindings(&mut self, keybindings: crate::tui::keybindings::Keybindings) {
        self.keybindings = Some(keybindings);
    }

    /// The installed keybinding map, if any (test/render seam).
    pub fn keybindings(&self) -> Option<&crate::tui::keybindings::Keybindings> {
        self.keybindings.as_ref()
    }

    /// Does `key` trigger the `tui.altScreen.*` id `id`?
    ///
    /// Resolution order (W11-C): the live map's SETTINGS override for the id
    /// (an explicit override, including the empty list, REPLACES the default
    /// — upstream `KeybindingsManager.rebuild`'s `userKeys === undefined ?
    /// defaultKeys : normalizeKeys(userKeys)`) → the platform-resolved
    /// coding-agent defaults (`config/keybindings.rs`, which carries the
    /// Windows variants of `search` / `previousPrompt` / `nextPrompt`) → the
    /// upstream-verbatim tui-package rows in [`ALT_SCREEN_KEYBINDINGS`]. The
    /// platform-branched prompt/search ids therefore need no hardcoded shape
    /// check in the router: their resolved default already runs through here.
    pub fn matches_alt_key(&self, id: &str, key: &KeyEvent) -> bool {
        self.alt_key_combos(id)
            .is_some_and(|combos| combos.iter().any(|combo| combo.matches(key)))
    }

    /// [`AltScreenState::matches_alt_key`] with SHIFT compared exactly, even
    /// for letter keys.
    ///
    /// Upstream `matchesKey` requires a chord's full modifier set: in a
    /// kitty/modifyOtherKeys sequence `ctrl+shift+g` carries SHIFT, so it
    /// matches `ctrl+shift+g` and NOT `ctrl+g` (`keys.ts`: the `modifier ===
    /// MODIFIERS.ctrl` branch). [`KeyCombo::matches`] is deliberately lenient
    /// for ASCII letters instead, because legacy terminals do not report
    /// SHIFT on ctrl/alt chords — under it `ctrl+g` and `ctrl+shift+g` are
    /// both "ctrl + g". The router tries this exact pass FIRST, so a terminal
    /// that does report SHIFT keeps the two chords distinct, then falls back
    /// to the lenient rule for the legacy case (where upstream too resolves
    /// ctrl+shift+g to the `ctrl+g` binding).
    fn matches_alt_key_exact(&self, id: &str, key: &KeyEvent) -> bool {
        self.alt_key_combos(id).is_some_and(|combos| {
            combos.iter().any(|combo| {
                combo.code == key.code
                    && combo.ctrl == key.modifiers.contains(KeyModifiers::CONTROL)
                    && combo.alt == key.modifiers.contains(KeyModifiers::ALT)
                    && combo.shift == key.modifiers.contains(KeyModifiers::SHIFT)
            })
        })
    }

    /// The resolved chords for one `tui.altScreen.*` id (the resolution order
    /// [`AltScreenState::matches_alt_key`] documents): the live map's
    /// SETTINGS override → the platform-resolved coding-agent defaults → the
    /// upstream-verbatim rows in [`ALT_SCREEN_KEYBINDINGS`]. `None` when no
    /// layer binds the id.
    fn alt_key_combos(&self, id: &str) -> Option<Vec<crate::tui::keybindings::KeyCombo>> {
        if let Some(overridden) = self
            .keybindings
            .as_ref()
            .and_then(|bindings| bindings.alt_id_combos(id))
        {
            return Some(overridden);
        }
        if let Some(combos) = crate::config::keybindings::default_keys_for(id) {
            return Some(
                combos
                    .iter()
                    .filter_map(|spec| crate::tui::keybindings::parse_key(spec))
                    .collect(),
            );
        }
        alt_screen_default_keys(id).map(|keys| {
            keys.iter()
                .filter_map(|spec| crate::tui::keybindings::parse_key(spec))
                .collect()
        })
    }

    /// The half-page delta the router last used (`None` before the first
    /// half-page key, or when the caller pins one through
    /// [`AltScreenState::set_half_page_step`]).
    pub fn half_page_step(&self) -> Option<usize> {
        self.half_page_step
    }

    /// Pin the half-page delta (test seam for hosts without a rendered
    /// viewport).
    pub fn set_half_page_step(&mut self, step: Option<usize>) {
        self.half_page_step = step;
    }

    /// Lines moved by the last routed viewport scroll, with its sign
    /// (test seam).
    pub fn last_scroll_step(&self) -> Option<isize> {
        self.last_scroll_step
    }

    pub fn flash_active(&self, now: Instant) -> bool {
        matches!(&self.flash, Some((_, at, duration)) if now.duration_since(*at) < *duration)
    }

    /// Upstream `setScrollbarHover` → `ScrollView.setScrollbarActive(active)`:
    /// flips the active state and marks scrollbar activity (upstream
    /// `markScrollbarActivity`, which while active installs NO hide timer — the
    /// bar stays up for as long as the pointer is on the track and gets a fresh
    /// [`SCROLLBAR_LINGER`] when it leaves). Returns whether the hover changed
    /// (the caller repaints).
    pub fn set_scrollbar_hover(&mut self, hovered: bool) -> bool {
        if hovered == self.scrollbar_hover {
            return false;
        }
        self.scrollbar_hover = hovered;
        self.scrollbar_linger = Some(Instant::now());
        true
    }

    /// `ScrollView.isScrollbarActive` (the `█` thumb).
    pub fn scrollbar_hover(&self) -> bool {
        self.scrollbar_hover
    }

    /// Whether a scrollbar drag gesture is live.
    pub fn scrollbar_dragging(&self) -> bool {
        self.scrollbar_drag.is_some()
    }

    /// Upstream `hasOverlay()`: while an overlay owns the focus the bar cannot
    /// be grabbed, the hover is cleared, and a live drag is ended (the event
    /// loop's mouse gate stops delivering the gesture's events, so the gesture
    /// must not survive the overlay).
    pub fn set_overlay_focus(&mut self, owns_focus: bool) {
        self.overlay_focus = owns_focus;
        if owns_focus {
            self.scrollbar_drag = None;
            self.scrollbar_hover = false;
            self.search.hovered_nav = None;
            self.pressed_url = None;
        }
    }

    /// The last painted jump-to-end label rect (upstream
    /// `scrollToEndIndicatorRect`); `None` when the view follows the end.
    pub fn scroll_to_end_rect(&self) -> Option<TextRect> {
        self.scroll_to_end_rect
    }

    /// Upstream `handleScrollbarMouseEvent`'s press branch: the bar consumes
    /// the press, so the selection GESTURE (anchor + dragged flag) is dropped.
    /// The painted selection itself is left alone — upstream clears only the
    /// anchor/focus there. The remembered hyperlink goes with the gesture
    /// (upstream `clearTextSelection` also drops `pressedUrl`), and so does
    /// the word-click chain (`lastClick = undefined`).
    pub fn clear_selection_gesture(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_dragged = false;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.last_click = None;
        self.pressed_url = None;
    }

    /// The selection unit of the last press (upstream `selectionGranularity`);
    /// it persists after the release until the next press or scrollbar grab.
    pub fn selection_granularity(&self) -> SelectionGranularity {
        self.selection_granularity
    }

    /// The word/line range the last double/triple click selected (upstream
    /// `selectionInitialRange`), kept so a following drag extends by the same
    /// unit.
    pub fn selection_initial_range(&self) -> Option<SelectionSpan> {
        self.selection_initial_range
    }

    /// The last word click's count (1/2/3), when one is remembered
    /// (upstream `lastClick?.count`).
    pub fn last_click_count(&self) -> Option<u8> {
        self.last_click.map(|click| click.count)
    }

    /// Record one left press on `row` and return its click count (upstream
    /// `getClickCount`): a press on the SAME word within
    /// [`DOUBLE_CLICK_INTERVAL`] advances 1 → 2 → 3 → 1; any other press (or
    /// a press without a word, e.g. outside the painted line) restarts at 1
    /// and clears the chain.
    fn next_click_count(&mut self, row: usize, word: Option<(usize, usize)>, now: Instant) -> u8 {
        let Some((start_col, end_col)) = word else {
            self.last_click = None;
            return 1;
        };
        let count = match self.last_click {
            Some(previous)
                if now.duration_since(previous.at) <= DOUBLE_CLICK_INTERVAL
                    && previous.row == row
                    && previous.start_col == start_col
                    && previous.end_col == end_col =>
            {
                (previous.count % 3) + 1
            }
            _ => 1,
        };
        self.last_click = Some(LastClick {
            at: now,
            count,
            row,
            start_col,
            end_col,
        });
        count
    }

    /// The selection span for one point at the current granularity (upstream
    /// `getWordSelection` / `getLineSelection`), or `None` in character mode.
    fn range_at(&self, point: (usize, usize), line: &str) -> Option<SelectionSpan> {
        let (start_col, end_col) = match self.selection_granularity {
            SelectionGranularity::Character => return None,
            SelectionGranularity::Word => word_selection_columns(line, point.1)?,
            SelectionGranularity::Line => line_selection_columns(line),
        };
        Some(SelectionSpan {
            start: (point.0, start_col),
            end: (point.0, end_col),
        })
    }

    /// Move the gesture focus to `point` (upstream `updateSelectionFocus`):
    /// word/line granularity snaps to that unit and extends the INITIAL range
    /// instead of the raw point, so the selection always covers whole words /
    /// lines.
    fn update_selection_focus(&mut self, point: (usize, usize), line: &str) {
        let Some(initial) = self.selection_initial_range else {
            self.selection_focus = Some(point);
            return;
        };
        let Some(range) = self.range_at(point, line) else {
            self.selection_focus = Some(point);
            return;
        };
        let target_before_initial =
            (range.start.0, range.start.1) < (initial.start.0, initial.start.1);
        if target_before_initial {
            self.selection_anchor = Some(initial.end);
            self.selection_focus = Some(range.start);
        } else {
            self.selection_anchor = Some(initial.start);
            self.selection_focus = Some(range.end);
        }
    }

    /// The ordered span of the live anchor/focus gesture (`None` when they
    /// coincide or no gesture is live).
    fn gesture_span(&self) -> Option<SelectionSpan> {
        let (anchor, focus) = (self.selection_anchor?, self.selection_focus?);
        if anchor == focus {
            return None;
        }
        Some(order_span(anchor, focus))
    }

    /// The OSC 8 hyperlink remembered by the last left press, until its
    /// release decides (upstream `pressedUrl`) — diagnostics/tests.
    pub fn pressed_url(&self) -> Option<&str> {
        self.pressed_url.as_deref()
    }

    /// Expire the flash when its duration passed (upstream timer expiry). The
    /// scrollbar linger is held while the pointer is on the track (upstream
    /// `markScrollbarActivity` skips the hide timer while `scrollbarActive`).
    pub fn tick(&mut self, now: Instant) -> bool {
        let expired = self
            .flash
            .as_ref()
            .is_some_and(|(_, at, duration)| now.duration_since(*at) >= *duration);
        if expired {
            self.flash = None;
        }
        if !self.scrollbar_hover {
            if let Some(at) = self.scrollbar_linger {
                if now.duration_since(at) >= SCROLLBAR_LINGER {
                    self.scrollbar_linger = None;
                }
            }
        }
        expired
    }

    /// Whether a wall-clock transient (the copy flash, the scrollbar linger)
    /// is on screen and will expire without another event.
    pub fn transient_pending(&self) -> bool {
        self.flash.is_some() || self.scrollbar_linger.is_some()
    }

    /// [`AltScreenState::tick`] for an IDLE-aware event loop, reporting
    /// whether anything visible actually disappeared: the flash and the
    /// scrollbar linger expire on wall-clock deadlines, and nothing else
    /// repaints a quiet screen, so the caller paints one frame exactly when
    /// this returns `true` (and never spins for a transient that is gone).
    pub fn expire_transients(&mut self, now: Instant) -> bool {
        if !self.transient_pending() {
            return false;
        }
        let before = (self.flash.is_some(), self.scrollbar_linger.is_some());
        let _ = self.tick(now);
        before != (self.flash.is_some(), self.scrollbar_linger.is_some())
    }
}

// ---------------------------------------------------------------------------
// Scroll geometry (upstream ScrollView + getScrollbarGeometry)
// ---------------------------------------------------------------------------

/// Transcript viewport geometry for one frame (upstream `ScrollView`'s
/// `contentHeight`/`viewportHeight` pair plus the largest in-bounds
/// `scrollTop`): the row one past the last transcript row, the transcript
/// viewport height, and `max_offset = contentHeight - viewportHeight`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportGeometry {
    /// Row one past the last transcript row (the dock starts here).
    pub transcript_end: usize,
    /// Transcript viewport height in rows.
    pub viewport_height: usize,
    /// Largest in-bounds scroll offset (`contentHeight - viewportHeight`).
    pub max_offset: usize,
}

/// Transcript/viewport geometry shared by every scroll action and by the
/// frame-level viewport update (`TuiCore::render`, upstream
/// `ScrollView.updateLayout`). The dock is clamped so at least one transcript
/// row stays visible.
pub fn viewport_geometry(frame_len: usize, dock_height: usize, rows: usize) -> ViewportGeometry {
    let dock = dock_height.min(frame_len).min(rows.saturating_sub(1));
    let transcript_end = frame_len.saturating_sub(dock);
    let viewport_height = rows.saturating_sub(dock).max(1);
    ViewportGeometry {
        transcript_end,
        viewport_height,
        max_offset: transcript_end.saturating_sub(viewport_height),
    }
}

/// Clamp the scroll offset so the viewport stays inside the transcript.
pub fn max_scroll_offset(frame_len: usize, dock_height: usize, rows: usize) -> usize {
    viewport_geometry(frame_len, dock_height, rows).max_offset
}

/// The transcript clip as the mouse family sees it, in SCREEN coordinates: the
/// clip starts at screen row 0 (the transcript is the alt-screen layout's top
/// region), `scroll_top` is the absolute buffer row painted at its top and
/// `content_height` the absolute row one past the last transcript row.
///
/// This is upstream `getScrollViewBox(layout, scrollView)` reduced to the one
/// scrollable pir paints (`clip.height` + `ScrollView.scrollTop` /
/// `contentHeight`), which is all [`scrollbar_geometry`] and
/// [`scroll_to_end_indicator_rect`] need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipGeometry {
    /// Rows of the transcript clip currently on screen.
    pub clip_height: usize,
    /// Absolute buffer row one past the last transcript row.
    pub content_height: usize,
    /// Absolute buffer row painted at the clip's top (upstream `scrollTop`).
    pub scroll_top: usize,
}

impl ClipGeometry {
    /// Upstream `maxScrollTop = max(0, contentHeight - viewportHeight)`.
    pub fn max_scroll_top(&self) -> usize {
        self.content_height.saturating_sub(self.clip_height)
    }
}

/// [`ClipGeometry`] of the last painted frame (upstream the layout pass's
/// `getScrollViewBox`): `TuiCore::painted_layout` already resolves the
/// follow/windowed split into the transcript window's rows and its top row.
pub fn clip_geometry(layout: &PaintedLayout) -> ClipGeometry {
    ClipGeometry {
        clip_height: layout.window_len,
        content_height: layout.transcript_end,
        scroll_top: layout.window_start,
    }
}

/// The follow path's SCREEN top row — the frame line drawn at screen row 0 —
/// from the decorator's inputs.
///
/// `TuiCore::render` hands the decorator the PREVIOUS frame's viewport top
/// (`FrameDecorateContext::visible_top`), which
/// `TuiCore::invalidate_frame` clears to 0 for one full repaint — and that is
/// exactly what `TuiCore::scroll_to(0, …)` runs on every return from a
/// scrolled view, i.e. after the search reveal's `disableFollow` pin, the
/// scrollbar/indicator jump to the bottom and the `/clear`-style resets. The
/// zero is right for the DIFF (a full repaint decorates every row) but wrong
/// for the screen geometry: a frame taller than the terminal stays
/// bottom-anchored, so its screen top is `frame_len - screen_rows`.
///
/// `screen_rows` is the terminal height the interaction layer last saw
/// ([`AltScreenState`]'s hint, refreshed by every routed key/mouse event and by
/// every painted frame); `0` means "not known yet" (a session's first frame)
/// and the ctx value is kept — the pre-existing first-frame behavior.
pub fn follow_screen_top(visible_top: usize, frame_len: usize, screen_rows: usize) -> usize {
    if visible_top > 0 || screen_rows == 0 || frame_len <= screen_rows {
        visible_top
    } else {
        frame_len - screen_rows
    }
}

/// Scrollbar hit geometry (upstream `layout.ts ScrollbarGeometry`) in SCREEN
/// coordinates: `track_top` is the clip's first screen row, `thumb_top` the
/// thumb's first row **relative to the track** (the track always starts at
/// `track_top`, so the two coincide only at offset 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    /// The bar's column (the terminal's last column).
    pub column: usize,
    /// First screen row of the track.
    pub track_top: usize,
    /// Track height in rows (the transcript clip's height).
    pub track_height: usize,
    /// First row of the thumb, relative to `track_top`.
    pub thumb_top: usize,
    /// Thumb height in rows.
    pub thumb_height: usize,
    /// Upstream `maxScrollTop` (`contentHeight - viewportHeight`).
    pub max_scroll_top: usize,
    /// `ScrollView.isScrollbarVisible` for this frame. Both the press and the
    /// hover hit-tests pass `includeHiddenAuto` (pir's press-hit deviation:
    /// a press on a faded `"auto"` bar's track proves the pointer is there
    /// and must grab it), so this flag is the painted/visible state, not a
    /// press precondition.
    pub visible: bool,
}

/// Upstream `layout.ts getScrollbarGeometry(box, includeHiddenAuto)` over the
/// transcript clip. `None` when the mode never paints a bar (`"hidden"`), when
/// the content fits the clip, or when an `"auto"` bar is currently hidden and
/// `include_hidden_auto` is false; the `"auto"` transience itself is the
/// caller's [`AltScreenState`] linger (`markScrollbarActivity`'s
/// `transientScrollbarVisible`). The press and hover hit-tests both pass
/// `true` (see [`handle_mouse_event_route`]); paint passes `false`.
pub fn scrollbar_geometry(
    clip: ClipGeometry,
    width: usize,
    mode: ScrollbarMode,
    include_hidden_auto: bool,
    transient_visible: bool,
) -> Option<ScrollbarGeometry> {
    if width == 0 || clip.clip_height == 0 {
        return None;
    }
    let track_height = clip.clip_height;
    let content_height = clip.content_height;
    let visible = match mode {
        ScrollbarMode::Hidden => false,
        // Upstream `isScrollbarVisible`: `"always"` paints whenever the
        // viewport has rows; `"auto"` needs overflow plus the transient flag.
        ScrollbarMode::Always => true,
        ScrollbarMode::Auto => content_height > track_height && transient_visible,
    };
    let can_reveal_hidden_auto =
        include_hidden_auto && mode == ScrollbarMode::Auto && content_height > track_height;
    if !visible && !can_reveal_hidden_auto {
        return None;
    }
    let min_thumb = track_height.min(2);
    let thumb_height = if content_height <= track_height {
        track_height
    } else {
        min_thumb
            .max(track_height.min(
                (track_height.saturating_mul(track_height) as f64 / content_height as f64).round()
                    as usize,
            ))
            .min(track_height)
    };
    let max_scroll_top = content_height.saturating_sub(track_height);
    let max_thumb_offset = track_height - thumb_height;
    let thumb_top = if max_scroll_top == 0 {
        0
    } else {
        ((clip.scroll_top.min(max_scroll_top) as f64 / max_scroll_top as f64)
            * max_thumb_offset as f64)
            .round() as usize
    }
    .min(max_thumb_offset);
    Some(ScrollbarGeometry {
        column: width - 1,
        track_top: 0,
        track_height,
        thumb_top,
        thumb_height,
        max_scroll_top,
        visible,
    })
}

/// One painted text rectangle in screen coordinates (upstream
/// `ScrollToEndIndicatorRect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRect {
    /// Screen row of the painted text.
    pub row: usize,
    /// First screen column of the painted text.
    pub column: usize,
    /// Visible width of the painted text.
    pub width: usize,
}

/// The jump-to-end indicator's painted rect (upstream
/// `compositeScrollToEndIndicator` + `scrollToEndIndicatorRect`): the
/// transcript clip's LAST screen row, centered over the clip width, truncated
/// at the scrollbar's column when the bar is painted so the label never covers
/// or captures the bar (upstream regression #9136).
///
/// `label_width` is the label's visible width BEFORE truncation — upstream
/// centers the untruncated label and only then clamps it to the free width.
pub fn scroll_to_end_indicator_rect(
    clip: ClipGeometry,
    width: usize,
    scrollbar_column: Option<usize>,
    label_width: usize,
) -> Option<TextRect> {
    if clip.clip_height == 0 || width == 0 {
        return None;
    }
    let label_width = label_width.min(width);
    let column = (width - label_width) / 2;
    let right_edge = scrollbar_column.unwrap_or(width);
    let text_width = label_width.min(right_edge.saturating_sub(column));
    if text_width == 0 {
        return None;
    }
    Some(TextRect {
        row: clip.clip_height - 1,
        column,
        width: text_width,
    })
}

/// The `tui.altScreen.*` default chords, upstream-verbatim from
/// `packages/tui/src/keybindings.ts` (`TUI_KEYBINDINGS`, the "intentionally
/// shadow the unmodified editor bindings in fullscreen mode" block).
///
/// pir's frozen registries deliberately do NOT carry this family:
/// `tui/keybindings.rs::TUI_KEYBINDING_DEFINITIONS`'s doc says the whole
/// `tui.altScreen.*` viewport family "is an app-layer concern in pir"
/// (routed from this module), and `config/keybindings.rs::
/// default_keybinding_definitions_for` carries only the three ids the
/// coding-agent layer rebinds (`search`, `previousPrompt`, `nextPrompt`, with
/// their Windows variants). The upstream-verbatim defaults for the rest
/// therefore live HERE, next to the router that implements them, so the
/// routing predicates and the labels (`keyDisplayText` for the jump-to-end
/// indicator, the search component's `formatKey` for its buttons) read one
/// table. Settings overrides still resolve through the config layer first (see
/// [`key_display_text`]); an override for an id this table owns is not honored
/// yet, matching the module header's keybinding note.
const ALT_SCREEN_KEYBINDINGS: &[(&str, &[&str])] = &[
    // W11-C: the six viewport ids that complete the upstream family. Upstream
    // ships line/half-page unbound by default (user-bindable only) and
    // pageUp/pageDown on the plain keys.
    ("tui.altScreen.pageUp", &["pageUp"]),
    ("tui.altScreen.pageDown", &["pageDown"]),
    ("tui.altScreen.halfPageUp", &[]),
    ("tui.altScreen.halfPageDown", &[]),
    ("tui.altScreen.lineUp", &[]),
    ("tui.altScreen.lineDown", &[]),
    (
        "tui.altScreen.previousPrompt",
        &["ctrl+shift+up", "ctrl+up"],
    ),
    (
        "tui.altScreen.nextPrompt",
        &["ctrl+shift+down", "ctrl+down"],
    ),
    ("tui.altScreen.search", &["ctrl+shift+f"]),
    ("tui.altScreen.searchNext", &["enter", "ctrl+g"]),
    (
        "tui.altScreen.searchPrevious",
        &["shift+enter", "ctrl+shift+g"],
    ),
    ("tui.altScreen.searchClose", &["escape"]),
    ("tui.altScreen.top", &["ctrl+home"]),
    ("tui.altScreen.bottom", &["ctrl+end"]),
];

/// The upstream-verbatim defaults for one `tui.altScreen.*` id
/// ([`ALT_SCREEN_KEYBINDINGS`]); `None` for ids outside the family.
pub fn alt_screen_default_keys(id: &str) -> Option<&'static [&'static str]> {
    ALT_SCREEN_KEYBINDINGS
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, keys)| *keys)
}

/// The `'static` registry spelling of an alt-screen id (W11-C: settings
/// overrides key on the canonical id string, so the registry lookup must hand
/// back the table's own `'static` slice).
pub fn alt_screen_id(name: &str) -> Option<&'static str> {
    ALT_SCREEN_KEYBINDINGS
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(id, _)| *id)
}

/// Whether `id` is inside the routed `tui.altScreen.*` viewport family.
pub fn is_alt_screen_id(id: &str) -> bool {
    alt_screen_id(id).is_some()
}

/// Upstream `formatKeys(keys, { capitalize: true })` / the search component's
/// local `formatKey`: the first resolved chord for a `tui.*` id, title-cased
/// per `+` part (`ctrl+end` → `Ctrl+End`; `alt` renders as `Option` on macOS).
/// Empty when the id is unbound.
///
/// Resolution order: the config layer (`config/keybindings.rs`, which carries
/// the three rebinding ids and the settings-registered ones) → the
/// upstream-verbatim rows this module routes ([`alt_screen_default_keys`]) →
/// the tui-package registry (`tui/keybindings.rs`, for any remaining
/// registered id). Settings overrides for the routed family are applied by
/// [`AltScreenState::matches_alt_key`]; the labels keep the default spelling
/// because the free-function label seam has no access to the live map.
fn key_display_text(id: &str) -> String {
    if let Some(first) = crate::config::keybindings::default_keys_for(id)
        .and_then(|keys| keys.into_iter().next().map(|key| format_key_spec(&key)))
    {
        return first;
    }
    if let Some(first) =
        alt_screen_default_keys(id).and_then(|keys| keys.first().map(|key| format_key_spec(key)))
    {
        return first;
    }
    crate::tui::keybindings::tui_effective_default_keys(id)
        .and_then(|keys| keys.first().map(|key| format_key_spec(key)))
        .unwrap_or_default()
}

fn format_key_spec(spec: &str) -> String {
    spec.split('+')
        .map(|part| {
            if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
                return "Option".to_string();
            }
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// The jump-to-end label's plain text (upstream `modes/interactive/tui-renderer.ts`
/// `scrollToEndIndicator`): `" ↓ Jump to latest message · <tui.altScreen.bottom
/// shortcut> "`. The shortcut segment is dropped when the id is unbound.
pub fn scroll_to_end_indicator_text() -> String {
    let shortcut = key_display_text("tui.altScreen.bottom");
    if shortcut.is_empty() {
        format!(" {SCROLL_TO_END_ARROW} {SCROLL_TO_END_LABEL} ")
    } else {
        format!(" {SCROLL_TO_END_ARROW} {SCROLL_TO_END_LABEL} \u{b7} {shortcut} ")
    }
}

/// The search box's button key text: `keyDisplayText` for the id, with the
/// search component's `formatKey` fallback when it is unbound.
fn button_key_text(id: &str) -> String {
    let text = key_display_text(id);
    if text.is_empty() {
        "Unbound".to_string()
    } else {
        text
    }
}

/// [`scroll_to_end_indicator_text`] painted with the theme's
/// `selectedBg`/`text` colors (upstream `theme.bg("selectedBg",
/// theme.fg("text", label))`).
pub fn scroll_to_end_indicator_label() -> String {
    paint_scroll_to_end_label(&scroll_to_end_indicator_text())
}

/// Paint one already-truncated label text (the caller clips the PLAIN string,
/// so a label clamped at the scrollbar column cannot leak the highlight
/// background past its closes).
fn paint_scroll_to_end_label(text: &str) -> String {
    with_theme(|theme| {
        let painted = theme.fg(ThemeColor::Text, text);
        theme.bg(ThemeColor::SelectedBg, &painted)
    })
}

// ---------------------------------------------------------------------------
// ScrollView state + wheel chain (upstream components/scroll-view.ts and
// TuiAltScreen.routeWheel)
// ---------------------------------------------------------------------------

/// Overscroll policy of one scrollable in the wheel chain (upstream
/// `ScrollViewOptions.overscroll`, default `"chain"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overscroll {
    /// A leftover delta keeps travelling outward (upstream default).
    #[default]
    Chain,
    /// The chain stops here: the leftover does not reach the enclosing scroll
    /// views (upstream `"contain"`; the primary-transcript fallback below is a
    /// separate rule and still applies).
    Contain,
}

/// One scrollable as upstream `ScrollView` models it, for the wheel chain and
/// the viewport state machine: top-anchored `scrollTop` over
/// `contentHeight - viewportHeight`, the `follow: "end"` flags, and the
/// overscroll policy. (pir's viewport stores the inverted offset-from-end and
/// converts at the boundary — see [`scroll_view_scroll_by`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelScrollView {
    /// Overscroll policy.
    pub overscroll: Overscroll,
    /// Whether the pointer is inside this view's rect (upstream
    /// `getScrollViewsAt`'s containment test). A view the pointer missed is
    /// skipped by the chain but still participates in the primary fallback.
    pub hit: bool,
    /// Upstream `ScrollView.scrollTop`.
    pub scroll_top: usize,
    /// Upstream `maxScrollTop = max(0, contentHeight - viewportHeight)`.
    pub max_scroll_top: usize,
    /// Upstream `ScrollViewOptions.follow === "end"` (the fullscreen
    /// transcript, `chat-viewport.ts:24`).
    pub follow_end: bool,
    /// Upstream `ScrollView.followingEnd`: the view tracks the content end.
    pub following: bool,
    /// Upstream `ScrollView.followSuppressedAtEnd`: at the end but held there
    /// (`scrollTo(end, { disableFollow: true })`).
    pub follow_suppressed_at_end: bool,
}

/// Upstream `ScrollView.scrollBy(lines)`: move `delta` rows (positive = toward
/// the end, upstream's sign), clamp to `[0, maxScrollTop]`, re-enable follow at
/// the end, and return the leftover delta the view could not consume — the
/// amount the wheel chain offers to the next view out.
///
/// `delta` is `i64`: the keyboard jumps pass `isize::MAX / 4` as a
/// "scroll all the way" sentinel (upstream's `Number.MAX_SAFE_INTEGER`
/// equivalent), which must survive the trip.
pub fn scroll_view_scroll_by(view: &mut WheelScrollView, delta: i64) -> i64 {
    if delta == 0 {
        return 0;
    }
    let start = view.scroll_top as i64;
    let max = view.max_scroll_top.min(i64::MAX as usize) as i64;
    let next = start.saturating_add(delta).clamp(0, max);
    let moved = next - start;
    view.scroll_top = next as usize;
    view.following = view.follow_end && next == max;
    view.follow_suppressed_at_end = false;
    delta - moved
}

/// Upstream `ScrollView.scrollTo(top, { disableFollow })`: clamp the target,
/// clear a search pin, and re-enable follow only when the target reaches the
/// end and `disable_follow` did not pin it there (`followSuppressedAtEnd`).
pub fn scroll_view_scroll_to(view: &mut WheelScrollView, scroll_top: usize, disable_follow: bool) {
    let max = view.max_scroll_top;
    let next = scroll_top.min(max);
    let suppressed = disable_follow && next == max;
    view.scroll_top = next;
    view.following = !suppressed && view.follow_end && next == max;
    view.follow_suppressed_at_end = suppressed;
}

/// Upstream `ScrollView.updateLayout(contentHeight, viewportHeight)`: a
/// following view re-anchors at the end, a pinned one keeps its rows (clamped
/// to the new bounds), landing at the end resumes follow unless pinned.
pub fn scroll_view_update_layout(
    view: &mut WheelScrollView,
    content_height: usize,
    viewport_height: usize,
) {
    let max = content_height.saturating_sub(viewport_height);
    view.max_scroll_top = max;
    view.scroll_top = if view.following {
        max
    } else {
        view.scroll_top.min(max)
    };
    if view.scroll_top < max {
        view.follow_suppressed_at_end = false;
    }
    if view.follow_end && view.scroll_top == max && !view.follow_suppressed_at_end {
        view.following = true;
    }
}

/// Upstream `TuiAltScreen.routeWheel`: hand one wheel gesture to the
/// scrollables under the pointer, innermost first (upstream
/// `getScrollViewsAt` order). Each view takes what it can and chains the
/// leftover outward, unless it is [`Overscroll::Contain`], which stops the
/// chain. A leftover the chain did not hand to the primary — including every
/// wheel outside it, e.g. over the dock — then goes to `views[primary]`
/// (upstream `if (remaining !== 0 && !seen.has(primary)) primary.scrollBy(...)`),
/// so the transcript keeps taking wheel input anywhere on the screen.
///
/// Returns the delta still unclaimed after the primary fallback.
pub fn route_wheel(views: &mut [WheelScrollView], primary: usize, delta: i64) -> i64 {
    let mut remaining = delta;
    let mut visited_primary = false;
    for (index, view) in views.iter_mut().enumerate() {
        if !view.hit {
            continue;
        }
        if index == primary {
            visited_primary = true;
        }
        remaining = scroll_view_scroll_by(view, remaining);
        if remaining == 0 || view.overscroll == Overscroll::Contain {
            break;
        }
    }
    if remaining != 0 && !visited_primary {
        if let Some(view) = views.get_mut(primary) {
            remaining = scroll_view_scroll_by(view, remaining);
        }
    }
    remaining
}

/// The fullscreen transcript as a [`WheelScrollView`], converted from the
/// viewport's inverted offset-from-end (`offset == 0` = at the end):
/// `scrollTop = maxScrollTop - offset`. `hit` is whether the pointer is inside
/// the transcript's rows (upstream `getScrollViewsAt`); the transcript is
/// `follow: "end"` (`chat-viewport.ts:24`).
fn transcript_scroll_view(
    core: &TuiCore,
    geometry: &ViewportGeometry,
    hit: bool,
) -> WheelScrollView {
    let offset = core.scroll_offset().min(geometry.max_offset);
    WheelScrollView {
        overscroll: Overscroll::Chain,
        hit,
        scroll_top: geometry.max_offset.saturating_sub(offset),
        max_scroll_top: geometry.max_offset,
        follow_end: true,
        following: core.is_following(),
        follow_suppressed_at_end: core.follow_suppressed(),
    }
}

/// Write a routed [`WheelScrollView`] back into the viewport (upstream
/// `ScrollView` state → inverted offset + search pin).
fn store_transcript_scroll_view(
    core: &mut TuiCore,
    geometry: &ViewportGeometry,
    view: &WheelScrollView,
) {
    let offset = geometry
        .max_offset
        .saturating_sub(view.scroll_top.min(geometry.max_offset));
    core.scroll_to(offset, view.follow_suppressed_at_end);
}

fn scroll_by(core: &mut TuiCore, state: &mut AltScreenState, dock_height: usize, delta: isize) {
    let rows = core.rows().max(1);
    let geometry = viewport_geometry(core.frame_buffer_length(), dock_height, rows);
    // The keyboard viewport keys always drive the primary transcript
    // (upstream `TuiAltScreen.scrollBy` → primary `scrollBy`), not the chain.
    // `delta` is in the viewport's offset-from-end units (positive = earlier
    // content, the sign every call site uses); upstream's `scrollBy` counts
    // toward the end.
    let mut view = transcript_scroll_view(core, &geometry, true);
    scroll_view_scroll_by(&mut view, -(delta as i64));
    store_transcript_scroll_view(core, &geometry, &view);
    state.scrollbar_linger = Some(Instant::now());
    // A keyboard scroll is `ScrollView.scrollBy`/`scrollToEnd`: the search pin
    // is cleared (upstream `scrollBy` resets `followSuppressedAtEnd` even with
    // no movement).
    state.follow_pinned_at_end = false;
}

/// Route one wheel gesture at `pointer_row` through the fullscreen scroll
/// chain (upstream `routeWheel`). The fullscreen layout has exactly one
/// scrollable — the transcript (`chat-viewport.ts` wraps the document in a
/// single `ScrollView` above the dock) — so the chain is `[transcript]` while
/// the pointer is inside the transcript's rows and empty otherwise; an empty
/// chain hands the whole delta to the transcript through upstream's primary
/// fallback, which is why a wheel over the dock (or an overlay) still scrolls
/// the transcript (D18). Returns the delta nothing claimed.
fn route_wheel_event(
    core: &mut TuiCore,
    state: &mut AltScreenState,
    dock_height: usize,
    rows: usize,
    pointer_row: usize,
    delta: i64,
) -> i64 {
    let geometry = viewport_geometry(core.frame_buffer_length(), dock_height, rows);
    // The transcript spans the full width, so row containment is the whole
    // `getScrollViewsAt` test here (the dock is the only other region).
    let dock = dock_height.min(rows.saturating_sub(1));
    let transcript_rows = rows.saturating_sub(dock);
    let mut views = [transcript_scroll_view(
        core,
        &geometry,
        pointer_row < transcript_rows,
    )];
    let remaining = route_wheel(&mut views, 0, delta);
    store_transcript_scroll_view(core, &geometry, &views[0]);
    state.scrollbar_linger = Some(Instant::now());
    // `ScrollView.scrollBy` clears `followSuppressedAtEnd` on every gesture.
    state.follow_pinned_at_end = false;
    remaining
}

fn scroll_to_match(core: &mut TuiCore, state: &mut AltScreenState, dock_height: usize) {
    let Some(segment) = state
        .search
        .selected_match()
        .and_then(|m| m.segments.first())
    else {
        return;
    };
    let rows = core.rows().max(1);
    let dock = dock_height.min(rows.saturating_sub(1));
    let viewport_h = rows.saturating_sub(dock).max(1);
    let frame_len = core.frame_buffer_length();
    let transcript_end = frame_len.saturating_sub(dock_height.min(frame_len));
    // Reveal the match one third of a viewport from the top (upstream
    // `refreshSearch` reveal: `firstSegment.row - viewportHeight / 3`).
    let top = segment
        .row
        .saturating_sub(viewport_h / 3)
        .min(transcript_end.saturating_sub(1));
    let offset = transcript_end
        .saturating_sub(top)
        .saturating_sub(viewport_h)
        .min(max_scroll_offset(frame_len, dock_height, rows));
    // Upstream `refreshSearch`: `scrollView.scrollTo(target, { disableFollow:
    // true })` — the reveal pins the view even when the target *is* the
    // content end (a match already visible at the bottom), so streaming rows
    // grow below the window until a scroll toward the end resumes follow.
    core.scroll_to(offset, true);
    // Remember that the search owns the viewport's position: a query edit
    // that later leaves zero matches returns to the pre-search origin
    // (`SearchState::restore_origin`).
    state.search.revealed = true;
    // The decorator cannot see the pin (the follow path hands it
    // `scroll_offset: 0`), so it is mirrored into the shared state for the
    // jump-to-end indicator.
    state.follow_pinned_at_end = core.follow_suppressed();
    // Note: `scrollbar_linger` is intentionally not set — a search jump is not
    // a scroll gesture (upstream sets `disableFollow` only).
}

// ---------------------------------------------------------------------------
// Semantic prompt navigation (upstream `tui.altScreen.previousPrompt` /
// `nextPrompt` → `tui-alt-screen.ts:506-517 scrollToPrompt`)
// ---------------------------------------------------------------------------

/// The prompt-start marker pir can see in a rendered frame: the `userMessageBg`
/// background code the user-message box paints on every one of its rows.
///
/// Upstream anchors prompts on the OSC 133 `A` zone the user-message component
/// prepends to its first line (`user-message.ts` `OSC133_ZONE_START`) and
/// `scrollToPrompt` tests each scroll-content line with
/// `OSC133_PROMPT_START = /^\x1b\]133;A(?:\x07|\x1b\\)/`
/// (`tui-alt-screen.ts:75`). pir's transcript never emits OSC 133 zones, so the
/// nearest *existing* anchor is the user-message block's background paint
/// (`messages.rs` `UserMessageComponent` → `BoxContainer`, which the render
/// path prefixes to the whole line): the first row of a maximal run of rows
/// opening with that code is the prompt start, matching the row upstream marks
/// `A` on. A theme that paints no `userMessageBg` yields no background anchor;
/// rows carrying an upstream OSC 133 zone still match, and without either the
/// actions no-op, like upstream lines whose zones were stripped for a terminal
/// without OSC 133 support.
fn user_message_bg_prefix() -> String {
    // `Theme::bg` wraps the text between the background code and its reset;
    // the sentinel isolates the opening code from the closing one.
    let painted = with_theme(|theme| theme.bg(ThemeColor::UserMessageBg, "\u{1}"));
    match painted.split('\u{1}').next() {
        // Only a real background code marks a block: a theme without the key
        // resolves to a bare reset, which would otherwise match every row.
        Some(prefix) if prefix.starts_with("\x1b[48") => prefix.to_string(),
        _ => String::new(),
    }
}

/// The marker upstream scans for: a line opening with the OSC 133 prompt-start
/// zone (`OSC133_PROMPT_START = /^\x1b\]133;A(?:\x07|\x1b\\)/`).
const OSC133_PROMPT_START: &str = "\x1b]133;A";

/// Whether a rendered line starts a user message (upstream `scrollToPrompt`'s
/// per-row test, on the anchors pir can see).
fn is_prompt_start(line: &str, bg_prefix: &str) -> bool {
    line.starts_with(OSC133_PROMPT_START) || (!bg_prefix.is_empty() && line.starts_with(bg_prefix))
}

/// Absolute rows of the frame that start a user message (upstream
/// `scrollToPrompt`'s line scan, on the marker pir has). One row per block:
/// the run's first row, not its continuation rows.
pub fn prompt_start_rows(lines: &[String]) -> Vec<usize> {
    let prefix = user_message_bg_prefix();
    let mut rows = Vec::new();
    let mut in_block = false;
    for (row, line) in lines.iter().enumerate() {
        if is_prompt_start(line, &prefix) {
            if !in_block {
                rows.push(row);
            }
            in_block = true;
        } else {
            in_block = false;
        }
    }
    rows
}

/// Jump to the previous (`direction < 0`) / next semantic prompt, upstream
/// `scrollToPrompt`: step row by row from the current viewport top and stop at
/// the first prompt boundary strictly above/below it, then scroll there
/// (upstream `ScrollView.scrollTo` clamps to `maxScrollTop`, which also
/// re-enables follow-end at the bottom).
///
/// pir's `scroll_offset` counts rows back from the transcript end, so a target
/// row maps to `max - row`; a target at or below the follow position clamps to
/// 0 (live follow), exactly upstream's "scrolled to `maxScrollTop` ⇒ following
/// output". No prompt in that direction leaves the viewport untouched
/// (upstream's scan falls through without scrolling), and the transient
/// scrollbar is marked only when the viewport actually moved (upstream
/// `ScrollView.scrollTo`: `if (moved) markScrollbarActivity()`).
fn scroll_to_prompt(
    core: &mut TuiCore,
    state: &mut AltScreenState,
    dock_height: usize,
    direction: isize,
) {
    let rows = core.rows().max(1);
    let frame_len = core.frame_buffer_length();
    let ViewportGeometry {
        transcript_end,
        max_offset: max,
        ..
    } = viewport_geometry(frame_len, dock_height, rows);
    if max == 0 {
        // The transcript fits the viewport: nothing to scroll to (upstream
        // clamps every `scrollTo` to 0).
        return;
    }
    let current = core.scroll_offset().min(max);
    let current_top = max - current;
    let frame = core.last_frame_lines();
    let transcript = &frame[..transcript_end.min(frame.len())];
    let target = if direction < 0 {
        prompt_start_rows(transcript)
            .into_iter()
            .rev()
            .find(|row| *row < current_top)
    } else {
        prompt_start_rows(transcript)
            .into_iter()
            .find(|row| *row > current_top)
    };
    let Some(target) = target else {
        return;
    };
    let offset = max.saturating_sub(target);
    if offset != current {
        core.set_scroll_offset(offset);
        state.scrollbar_linger = Some(Instant::now());
        // Any scroll clears the search pin (upstream `scrollBy` /
        // `scrollTo` without `disableFollow`).
        state.follow_pinned_at_end = false;
    }
}

// ---------------------------------------------------------------------------
// Frame decoration (search highlight, selection, scrollbar, box, flash)
// ---------------------------------------------------------------------------

/// Paint the alt-screen interaction layer onto the painted frame lines.
/// Called by [`TuiCore::render`] through the decorator hook.
pub fn decorate_frame(
    lines: &mut [String],
    ctx: FrameDecorateContext,
    state: &mut AltScreenState,
    config: &AltScreenConfig,
) {
    if lines.is_empty() || ctx.width == 0 {
        return;
    }
    let now = Instant::now();
    state.tick(now);
    let search_visible = state.search.active && !state.search.matches.is_empty();
    let selection = state.selection.filter(|selection| !selection.is_empty());

    // Transcript rows and the dock under them, and only rows that are
    // actually on screen: the diff renderer would treat changes above the
    // viewport as a full repaint. The dock rows are always painted (the dock
    // bottoms the frame), so the selection highlight reaches the
    // editor/footer/dialog rows exactly like the transcript's.
    let dock_start = ctx.dock_start.min(lines.len());
    let first = if ctx.windowed {
        0
    } else {
        ctx.visible_top.min(dock_start)
    };
    for index in first..lines.len() {
        // Windowed frames paint a transcript window with the dock directly
        // under it (a gap in buffer coordinates); the follow frame paints the
        // whole buffer verbatim. Both map a dock row to
        // `transcript_end + (index - dock_start)` (in the follow path
        // `dock_start == ctx.transcript_end`, so the formula keeps
        // `index == absolute_row`).
        let absolute_row = if index < dock_start {
            if ctx.windowed {
                ctx.window_start + index
            } else {
                index
            }
        } else {
            ctx.transcript_end + (index - dock_start)
        };
        let Some(line) = lines.get_mut(index) else {
            continue;
        };
        // Selection paint first (upstream paints the selection under the
        // search highlight).
        if let Some(selection) = selection {
            if absolute_row >= selection.start.0 && absolute_row <= selection.end.0 {
                let start_col = if absolute_row == selection.start.0 {
                    selection.start.1
                } else {
                    0
                };
                let end_col = if absolute_row == selection.end.0 {
                    selection.end.1
                } else {
                    usize::MAX
                };
                if start_col < end_col {
                    *line = splice_columns(
                        line,
                        &[(start_col, end_col, SELECTION_OPEN, SELECTION_CLOSE)],
                    );
                }
            }
        }
        // Search match paint.
        if search_visible {
            let mut ranges: Vec<(usize, usize, &'static str, &'static str)> = Vec::new();
            for (match_index, matched) in state.search.matches.iter().enumerate() {
                for segment in &matched.segments {
                    if segment.row != absolute_row {
                        continue;
                    }
                    let current = state.search.selected == Some(match_index);
                    ranges.push((
                        segment.start_col,
                        segment.end_col,
                        if current {
                            CURRENT_MATCH_OPEN
                        } else {
                            MATCH_OPEN
                        },
                        if current {
                            CURRENT_MATCH_CLOSE
                        } else {
                            MATCH_CLOSE
                        },
                    ));
                }
            }
            if !ranges.is_empty() {
                ranges.sort_by_key(|(start, _, _, _)| *start);
                *line = splice_columns(line, &ranges);
            }
        }
    }

    // The transcript clip the scrollbar math, the indicator and the mouse
    // router share (upstream `getScrollViewBox`): `screen_base` is the line
    // index of screen row 0, the track covers the clip's rows, and `scroll_top`
    // is the absolute buffer row painted at its top. The windowed frame IS the
    // screen; the follow frame is bottom-anchored, so its screen top is
    // resolved through [`follow_screen_top`] and remembered for the frames
    // whose ctx value core had to reset.
    let dock = ctx.dock_start.min(lines.len());
    let (screen_base, clip) = if ctx.windowed {
        state.screen_rows = lines.len();
        (
            0,
            ClipGeometry {
                clip_height: dock,
                content_height: ctx.transcript_end,
                scroll_top: ctx.window_start,
            },
        )
    } else {
        let screen_top = follow_screen_top(ctx.visible_top, lines.len(), state.screen_rows);
        if ctx.visible_top > 0 {
            state.screen_rows = lines.len().saturating_sub(ctx.visible_top);
        }
        (
            screen_top,
            ClipGeometry {
                clip_height: dock.saturating_sub(screen_top),
                content_height: ctx.transcript_end,
                scroll_top: screen_top,
            },
        )
    };

    // Scrollbar (upstream `paintScrollbar` over `getScrollbarGeometry`). With
    // `"always"` the column is reserved and painted whenever the viewport has
    // rows — even when the content fits, where the whole track is thumb
    // (`getContentWidth` reserves the column unconditionally too, see
    // `ScrollbarColumnReserve`). Upstream `isScrollbarVisible` for `"auto"` is
    // exactly `contentHeight > viewportHeight && transientScrollbarVisible`:
    // the transient flag is set by the scroll actions (`scrollBy` / `scrollTo`
    // / the wheel) and by the pointer entering the track (`markScrollbarActivity`)
    // and fades after `scrollbarHideDelayMs` (1000 ms, pir's
    // [`SCROLLBAR_LINGER`]) once the pointer leaves. It is NOT a function of
    // the current scroll position — a view left scrolled once the fade passed
    // shows no bar (upstream behavior).
    let transient = state.scrollbar_linger.is_some();
    let painted_bar = scrollbar_geometry(clip, ctx.width, config.scrollbar, false, transient);
    if let Some(geometry) = painted_bar {
        for offset in 0..geometry.track_height {
            let index = screen_base + geometry.track_top + offset;
            let Some(line) = lines.get_mut(index) else {
                continue;
            };
            let is_thumb =
                offset >= geometry.thumb_top && offset < geometry.thumb_top + geometry.thumb_height;
            let cell = if is_thumb {
                // Upstream `isScrollbarActive ? "█" : "┃"`: only the pointer
                // over the track sets the active state (`setScrollbarActive`,
                // see [`AltScreenState::set_scrollbar_hover`]).
                if state.scrollbar_hover {
                    SCROLLBAR_THUMB_ACTIVE
                } else {
                    SCROLLBAR_THUMB
                }
            } else {
                SCROLLBAR_TRACK
            };
            let base = ansi::pad_to_width(
                &ansi::truncate_to_width(line, geometry.column, ""),
                geometry.column,
            );
            *line = format!("{base}{cell}");
        }
    }

    // Jump-to-end indicator (upstream `compositeScrollToEndIndicator`): the
    // transcript clip's last row while the view is scrolled away from its end.
    // Painted under the search box (upstream composites the indicator before
    // the overlays) and clamped at the scrollbar's column so the bar stays
    // visible and clickable (upstream regression #9136).
    state.scroll_to_end_rect = None;
    if ctx.scroll_offset > 0 || state.follow_pinned_at_end {
        let text = scroll_to_end_indicator_text();
        if let Some(rect) = scroll_to_end_indicator_rect(
            clip,
            ctx.width,
            painted_bar.map(|geometry| geometry.column),
            ansi::visible_width(&text),
        ) {
            // The plain text is clipped first and the theme applied after it,
            // so the label's closes always land inside the painted span.
            let clipped = ansi::truncate_to_width(&text, rect.width, "");
            if let Some(line) = lines.get_mut(screen_base + rect.row) {
                *line = composite_columns(line, rect.column, &paint_scroll_to_end_label(&clipped));
            }
            state.scroll_to_end_rect = Some(rect);
        }
    }

    // Transcript search box (upstream the `AltScreenSearchComponent` overlay:
    // `top-right`, margin 1): the query + `n/m` on the middle row and the
    // hover-styled prev/next affordances on the bottom rule.
    if state.search.active {
        if let Some(layout) = search_box_layout(ctx.width) {
            paint_search_box(lines, screen_base, &layout, &state.search);
        }
    }

    // Flash message: a reversed line on the last painted row (upstream
    // composites flash entries as reversed lines at the bottom).
    if let Some((message, _, _)) = &state.flash {
        let label = ansi::truncate_to_width(&format!(" {message} "), ctx.width, "");
        let lead = ctx.width.saturating_sub(ansi::visible_width(&label)) / 2;
        let painted = format!("{}\x1b[7m{label}\x1b[27m", " ".repeat(lead));
        if let Some(line) = lines.last_mut() {
            *line = format!(
                "{}{}",
                ansi::truncate_to_width(
                    line,
                    ctx.width.saturating_sub(ansi::visible_width(&painted)),
                    ""
                ),
                painted
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Search box painting + column compositing
// ---------------------------------------------------------------------------

/// Paint one search box (see [`search_box_layout`]) into `lines`, whose screen
/// row `0` is `screen_base`: three rows — top rule, query + `n/m`, bottom rule
/// with the prev/next affordances — at the top-right, one cell inside the
/// screen edge. Rows the frame cannot reach are skipped.
fn paint_search_box(
    lines: &mut [String],
    screen_base: usize,
    layout: &SearchBoxLayout,
    search: &SearchState,
) {
    let result = search.result_text();
    let result_text = if result.is_empty() {
        String::new()
    } else {
        format!("{DIM_OPEN} {result} {DIM_CLOSE}")
    };
    let input_width = layout
        .inner
        .saturating_sub(ansi::visible_width(&result_text));
    let input_display = if search.query.trim().is_empty() {
        format!(
            " {DIM_OPEN}{}{DIM_CLOSE}",
            ansi::truncate_to_width("Find in transcript", input_width.saturating_sub(1), "")
        )
    } else {
        // The tail keeps what was just typed in view; `slice_columns` measures
        // grapheme clusters, so a wide cell is never cut in half.
        let width = input_width.saturating_sub(1);
        let query_width = ansi::visible_width(&search.query);
        let tail = slice_columns(
            &search.query,
            query_width.saturating_sub(width),
            query_width,
        );
        format!(" {tail}")
    };
    let input_padding = " ".repeat(input_width.saturating_sub(ansi::visible_width(&input_display)));
    let content = format!("{input_display}{input_padding}{result_text}");
    let previous = layout
        .previous_label
        .as_deref()
        .map(|text| nav_button_style(text, search.hovered_nav == Some(SearchNav::Previous)))
        .unwrap_or_default();
    let next = layout
        .next_label
        .as_deref()
        .map(|text| nav_button_style(text, search.hovered_nav == Some(SearchNav::Next)))
        .unwrap_or_default();
    let bottom = if layout.previous_label.is_some() {
        // `└` + left rule + gap + buttons + gap + right rule + `┘`
        // (upstream's `outerGapWidth` spaces around `renderedButtons`).
        format!(
            "{BOX_BOTTOM_LEFT}{left_rule} {previous}{separator}{next} {right_rule}{BOX_BOTTOM_RIGHT}",
            left_rule = BOX_HORIZONTAL.to_string().repeat(layout.left_rule),
            right_rule = BOX_HORIZONTAL.to_string().repeat(layout.right_rule),
            separator = layout.separator,
        )
    } else {
        format!(
            "{BOX_BOTTOM_LEFT}{}{BOX_BOTTOM_RIGHT}",
            BOX_HORIZONTAL.to_string().repeat(layout.inner)
        )
    };
    let rows = [
        format!(
            "{BOX_TOP_LEFT}{}{BOX_TOP_RIGHT}",
            BOX_HORIZONTAL.to_string().repeat(layout.inner)
        ),
        format!("{BOX_VERTICAL}{content}{BOX_VERTICAL}"),
        bottom,
    ];
    for (offset, text) in rows.iter().enumerate() {
        if let Some(line) = lines.get_mut(screen_base + layout.top + offset) {
            *line = composite_columns(line, layout.left, text);
        }
    }
}

/// Upstream `searchNavigationButtonStyle(text, hovered)`: the interactive
/// renderer styles a hovered button with `theme.underline(text)`.
fn nav_button_style(text: &str, hovered: bool) -> String {
    if hovered {
        with_theme(|theme| theme.underline(text))
    } else {
        text.to_string()
    }
}

/// Replace visible columns `[start, start + width(replacement))` of an ANSI
/// line, keeping the cells before and after it (upstream `compositeTuiLine`);
/// the line's visible width is preserved, so a short source row is padded.
/// Escape sequences left of the cut are not re-opened — the tail carries the
/// source's own codes from the cut onward, like the scrollbar painter's
/// truncation.
fn composite_columns(line: &str, start: usize, replacement: &str) -> String {
    let line_width = ansi::visible_width(line);
    let mut out = ansi::truncate_to_width(line, start, "");
    let head_width = ansi::visible_width(&out);
    if head_width < start {
        out.push_str(&" ".repeat(start - head_width));
    }
    out.push_str(replacement);
    let end = start + ansi::visible_width(replacement);
    if end < line_width {
        out.push_str(&slice_from_column(line, end));
    }
    let out_width = ansi::visible_width(&out);
    if out_width < line_width {
        out.push_str(&" ".repeat(line_width - out_width));
    }
    out
}

/// The ANSI line's cells at or after visible column `skip`; grapheme clusters
/// are kept whole and `\t` counts as the three cells the corpus model gives it.
fn slice_from_column(line: &str, skip: usize) -> String {
    if skip == 0 {
        return line.to_string();
    }
    let mut out = String::new();
    let mut col = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut seq = String::from("\x1b");
            if chars.peek() == Some(&'[') {
                seq.push(chars.next().unwrap_or('\x1b'));
                for ch in chars.by_ref() {
                    seq.push(ch);
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            } else if matches!(chars.peek(), Some(']') | Some('_')) {
                seq.push(chars.next().unwrap_or('\x1b'));
                let mut prev = '\0';
                for ch in chars.by_ref() {
                    seq.push(ch);
                    if ch == '\x07' || (prev == '\x1b' && ch == '\\') {
                        break;
                    }
                    prev = ch;
                }
            }
            if col >= skip {
                out.push_str(&seq);
            }
            continue;
        }
        let mut run = String::new();
        run.push(c);
        while let Some(&next_char) = chars.peek() {
            if next_char == '\x1b' {
                break;
            }
            run.push(next_char);
            chars.next();
        }
        for cluster in UnicodeSegmentation::graphemes(run.as_str(), true) {
            let width = if cluster == "\t" {
                3
            } else {
                ansi::grapheme_width(cluster)
            };
            if col >= skip {
                out.push_str(cluster);
            }
            col += width.max(1);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Selection text extraction
// ---------------------------------------------------------------------------

/// The selected text from the last rendered frame (upstream
/// `getActiveSelectionText`: column-slice each covered row, trim trailing
/// whitespace per row, join with `\n`).
///
/// A row OUTSIDE the frame contributes an empty line instead of failing the
/// whole extraction (upstream `sourceLines[row] ?? ""`,
/// `tui-alt-screen.js:955-961`): the anchor may have been recorded before the
/// frame shrank (a streaming reply being replaced, `/clear`, a session
/// switch) and the copy-on-select verdict must not turn a benign condition
/// into "Copy failed" — the rows that DO exist are still what the user
/// selected and are copied. Only an entirely empty selection is `None`.
pub fn selection_text(frame: &[String], selection: &SelectionSpan) -> Option<String> {
    let mut collected: Vec<String> = Vec::new();
    for (row, _span) in row_span(selection) {
        let line = match frame.get(row) {
            Some(line) => ansi::strip_ansi(line),
            None => String::new(),
        };
        let start = if row == selection.start.0 {
            selection.start.1
        } else {
            0
        };
        let end = if row == selection.end.0 {
            selection.end.1
        } else {
            usize::MAX
        };
        collected.push(slice_columns(&line, start, end).trim_end().to_string());
    }
    let text = collected.join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn row_span(selection: &SelectionSpan) -> impl Iterator<Item = (usize, &SelectionSpan)> {
    (selection.start.0..=selection.end.0).map(move |row| (row, selection))
}

// ---------------------------------------------------------------------------
// Double/triple-click word + line selection (upstream `getWordSelection` /
// `getLineSelection` / `getClickCount`, tui-alt-screen.ts:1188-1276)
// ---------------------------------------------------------------------------

/// Selection unit of the live mouse gesture (upstream
/// `selectionGranularity`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionGranularity {
    #[default]
    Character,
    Word,
    Line,
}

/// One successful click, kept for double/triple-click detection (upstream
/// `lastClick`).
#[derive(Debug, Clone, Copy)]
struct LastClick {
    at: Instant,
    /// 1, 2 or 3 (a fourth click cycles back to 1).
    count: u8,
    row: usize,
    /// The clicked word's column range; only same-word clicks chain.
    start_col: usize,
    end_col: usize,
}

/// The visible width of a segmentation unit, over grapheme clusters (tabs
/// count three cells, like every other column measure in this module).
fn segment_width(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true)
        .map(|cluster| {
            if cluster == "\t" {
                3
            } else {
                ansi::grapheme_width(cluster)
            }
        })
        .sum()
}

/// The column range of the word (or word-like run) under `column` on a PLAIN
/// (ANSI-stripped) line — upstream `getWordSelection`: UAX #29 word
/// segments, with letters/digits `selectable`, the
/// [`WORD_SELECTION_JOINERS`] single-character segments joining their
/// neighbours, and only `selectable`+joiner neighbours chaining.
pub fn word_selection_columns(line: &str, column: usize) -> Option<(usize, usize)> {
    struct Segment {
        start: usize,
        end: usize,
        selectable: bool,
        joiner: bool,
    }
    let mut segments: Vec<Segment> = Vec::new();
    let mut start = 0usize;
    for segment in UnicodeSegmentation::split_word_bounds(line) {
        let end = start + segment_width(segment);
        let joiner = WORD_SELECTION_JOINERS.contains(&segment);
        let selectable = joiner || segment.chars().any(|character| character.is_alphanumeric());
        segments.push(Segment {
            start,
            end,
            selectable,
            joiner,
        });
        start = end;
    }
    let clicked = segments
        .iter()
        .position(|segment| column >= segment.start && column < segment.end)?;
    let can_join = |left: &Segment, right: &Segment| {
        left.selectable && right.selectable && (left.joiner || right.joiner)
    };
    let mut selection_start = segments[clicked].start;
    let mut selection_end = segments[clicked].end;
    let mut index = clicked;
    while index > 0 && can_join(&segments[index - 1], &segments[index]) {
        selection_start = segments[index - 1].start;
        index -= 1;
    }
    let mut index = clicked;
    while index + 1 < segments.len() && can_join(&segments[index], &segments[index + 1]) {
        selection_end = segments[index + 1].end;
        index += 1;
    }
    Some((selection_start, selection_end))
}

/// The whole visible line's column range (upstream `getLineSelection`:
/// `col: 0` → `visibleWidth(line)`).
pub fn line_selection_columns(line: &str) -> (usize, usize) {
    (0, ansi::visible_width(line))
}

/// The ANSI-stripped frame line for an absolute buffer row (the same source
/// `selection_text` slices).
fn plain_frame_line(core: &TuiCore, row: usize) -> String {
    core.last_frame_lines()
        .get(row)
        .map(|line| ansi::strip_ansi(line))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Input routing
// ---------------------------------------------------------------------------

/// What the event loop should do with a key the alt-screen family wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltKeyRoute {
    /// The family consumed the key (do not dispatch to the editor).
    Consumed,
    /// No alt-screen behavior for this key; dispatch normally.
    Passthrough,
    /// `app.suspend` was routed (the caller resolves the chord through the
    /// keybinding table; default ctrl+z outside native win32, EMPTY there).
    Suspend,
    /// `app.thinking.toggle` was routed (default ctrl+t; same table-driven
    /// caller dispatch).
    ToggleThinking,
}

fn is_plain_char(key: &KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// The search box's navigation ids with their `navigate_search` direction,
/// in upstream's check order (`tui-alt-screen.ts:735-742`: next before
/// previous). Kept as one table so [`route_alt_screen_key`]'s exact-SHIFT and
/// lenient passes cannot drift apart.
const SEARCH_NAV_IDS: &[(&str, i32)] = &[
    ("tui.altScreen.searchNext", 1),
    ("tui.altScreen.searchPrevious", -1),
];

/// Route one key press through the alt-screen family (upstream
/// `handleViewportInput` + the interactive-mode `app.*` editor actions).
/// The caller must already have established that no dialog/selector overlay
/// owns the focus — this router never sees those.
pub fn route_alt_screen_key(
    key: &KeyEvent,
    core: &mut TuiCore,
    state: &mut AltScreenState,
    dock_height: usize,
) -> AltKeyRoute {
    // Re-sync the follow-pin mirror (see [`AltScreenState::follow_pinned_at_end`])
    // with the core before the key's action runs, and keep the screen-height
    // hint fresh (the follow decorator needs it after an `invalidate_frame`).
    state.follow_pinned_at_end = core.follow_suppressed();
    state.screen_rows = core.rows().max(1);
    // `tui.altScreen.search`: resolved through the live map (settings
    // override first) and the platform defaults (`ctrl+f` where
    // `useWindowsKeybindings()` holds — native Windows and WSL — `ctrl+shift+f`
    // elsewhere). In fullscreen mode this intentionally shadows the editor's
    // `ctrl+f` (cursorRight) on the Windows branch.
    if state.matches_alt_key("tui.altScreen.search", key) {
        state.search.active = !state.search.active;
        if state.search.active {
            state
                .search
                .begin(core.scroll_offset(), core.follow_suppressed());
        } else {
            state.search.close();
        }
        return AltKeyRoute::Consumed;
    }

    // `app.suspend` (ctrl+z) and `app.thinking.toggle` (ctrl+t) are NOT routed
    // here: they are editor actions upstream (`defaultEditor.onAction`,
    // interactive-mode.ts:3101/3110), so the caller dispatches them through
    // the keybinding table BEFORE this router — which keeps the native-win32
    // EMPTY `app.suspend` default (ctrl+z must not respond) and settings
    // overrides authoritative.

    // While the search box is open it owns the keyboard (upstream: the search
    // overlay holds the focus). The three binding-backed ids resolve through
    // the live map first (W11-C), with the upstream default shapes as the
    // fallback for a host without a keybinding map.
    //
    // Two passes, because `searchNext` (`ctrl+g`) and `searchPrevious`
    // (`ctrl+shift+g`) differ only in SHIFT and [`KeyCombo::matches`] ignores
    // SHIFT on letters for legacy terminals. Upstream `matchesKey` requires
    // the full modifier set, so a terminal that DOES report SHIFT (kitty /
    // modifyOtherKeys) must resolve `ctrl+shift+g` to `previous`; the lenient
    // pass then covers the legacy report where the two chords are one event.
    if state.search.active {
        let direction = SEARCH_NAV_IDS
            .iter()
            .find(|(id, _)| state.matches_alt_key_exact(id, key))
            .or_else(|| {
                SEARCH_NAV_IDS
                    .iter()
                    .find(|(id, _)| state.matches_alt_key(id, key))
            })
            .map(|(_, direction)| *direction);
        if let Some(direction) = direction {
            navigate_search(core, state, dock_height, direction);
            return AltKeyRoute::Consumed;
        }
        if state.matches_alt_key("tui.altScreen.searchClose", key) {
            state.search.close();
            return AltKeyRoute::Consumed;
        }
        if let KeyCode::Backspace = key.code {
            // Upstream `updateSearchQuery`: capture the anchor (the
            // current match's first row, else the viewport top) BEFORE
            // the edit, then re-target the closest match at/after it.
            let anchor = search_anchor(core, state);
            state.search.query.pop();
            retarget_search(core, state, dock_height, anchor);
            return AltKeyRoute::Consumed;
        }
        if let Some(c) = is_plain_char(key) {
            let anchor = search_anchor(core, state);
            state.search.query.push(c);
            retarget_search(core, state, dock_height, anchor);
            return AltKeyRoute::Consumed;
        }
        // Swallow everything else while the search box is focused.
        return AltKeyRoute::Consumed;
    }

    // Viewport scroll keys (upstream `tui.altScreen.*`, which intentionally
    // shadow the unmodified editor bindings in fullscreen mode). The key
    // ORDER is upstream's (`tui-alt-screen.ts:746-789`): page → half page →
    // line → prompt jumps → top/bottom. Every id resolves through
    // [`AltScreenState::matches_alt_key`], so a settings override (including an
    // empty list that unbinds a default) replaces the default and the
    // platform-variant prompt/search chords come from
    // `config/keybindings.rs`.
    let rows = core.rows().max(1);
    // The primary scroll view is the transcript above the dock (upstream
    // `getPrimaryScrollView().viewportHeight`).
    let viewport_height = rows.saturating_sub(dock_height.min(rows)).max(1);
    let viewport_step = viewport_height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1);
    // Upstream `tui.altScreen.halfPageUp/Down`:
    // `max(1, floor(viewportHeight / 2))` — the raw viewport height, NOT the
    // page step (no overlap subtraction, `tui-alt-screen.ts:762-770`).
    let half_step = state.half_page_step.unwrap_or(viewport_height / 2).max(1);
    // Scroll deltas are top-anchored upstream and end-anchored here, hence
    // the sign flip: `scroll_offset` counts lines back from the transcript
    // end (0 = live follow), so showing earlier content *increases* it — an
    // upstream `scrollBy(-n)` maps to `+n`.
    let mut step = |state: &mut AltScreenState, delta: isize| {
        state.last_scroll_step = Some(delta);
        scroll_by(core, state, dock_height, delta);
    };
    if state.matches_alt_key("tui.altScreen.pageUp", key) {
        step(state, viewport_step as isize);
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.pageDown", key) {
        step(state, -(viewport_step as isize));
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.halfPageUp", key) {
        step(state, half_step as isize);
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.halfPageDown", key) {
        step(state, -(half_step as isize));
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.lineUp", key) {
        step(state, 1);
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.lineDown", key) {
        step(state, -1);
        return AltKeyRoute::Consumed;
    }
    // Semantic prompt jumps (`tui.altScreen.previousPrompt` / `nextPrompt`).
    if state.matches_alt_key("tui.altScreen.previousPrompt", key) {
        scroll_to_prompt(core, state, dock_height, -1);
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.nextPrompt", key) {
        scroll_to_prompt(core, state, dock_height, 1);
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.top", key) {
        // Upstream `tui.altScreen.top` (ctrl+home): earliest content.
        scroll_by(core, state, dock_height, isize::MAX / 4);
        return AltKeyRoute::Consumed;
    }
    if state.matches_alt_key("tui.altScreen.bottom", key) {
        // Upstream `tui.altScreen.bottom` (ctrl+end): back to live follow.
        scroll_by(core, state, dock_height, isize::MIN / 4);
        return AltKeyRoute::Consumed;
    }
    AltKeyRoute::Passthrough
}

fn navigate_search(
    core: &mut TuiCore,
    state: &mut AltScreenState,
    dock_height: usize,
    direction: i32,
) {
    if state.search.matches.is_empty() {
        return;
    }
    let count = state.search.matches.len();
    state.search.selected = Some(match state.search.selected {
        // Nothing selected yet (or stale past the end): enter from the top
        // going forward, from the last match going backward.
        Some(selected) if selected >= count => {
            if direction < 0 {
                count - 1
            } else {
                0
            }
        }
        Some(selected) => ((selected as i64 + direction as i64).rem_euclid(count as i64)) as usize,
        None => {
            if direction < 0 {
                count - 1
            } else {
                0
            }
        }
    });
    scroll_to_match(core, state, dock_height);
}

/// The anchor row for a query edit (upstream `updateSearchQuery`):
/// `search.anchorRow = selected?.segments[0]?.row ?? scrollView.scrollTop`.
fn search_anchor(core: &TuiCore, state: &AltScreenState) -> usize {
    state
        .search
        .selected_match()
        .and_then(|matched| matched.segments.first())
        .map(|segment| segment.row)
        .unwrap_or_else(|| core.visible_top())
}

/// Apply a query edit: recompute the matches against the current frame,
/// select the match closest to the anchor (the first one at/after it, else
/// the last before it — upstream `refreshSearch` binary-searches for the
/// closest match), and reveal it with `scrollToMatch`
/// (upstream `tui-alt-screen.ts:616-661`). The search result text then
/// carries the current ordinal (`n/m`), not just the count.
///
/// A query edit also re-arms the backward walk ([`SearchWalk::start`]) in the
/// windowed viewport: with no match in the materialized band the loop then
/// pages older history in (`plan_search_walk`); with matches in the band the
/// walk settles on the first frame. In the regular (main-screen) viewport the
/// whole transcript is materialized, so no walk is armed — that path behaves
/// exactly as before.
fn retarget_search(
    core: &mut TuiCore,
    state: &mut AltScreenState,
    dock_height: usize,
    anchor: usize,
) {
    let matches = {
        let frame = core.last_frame_lines();
        let dock = dock_height.min(frame.len());
        // Route the query edit through the cache: it is now keyed on the new
        // query, so the `refresh_search` after the reveal's repaint reuses
        // this parse instead of stripping the whole frame again.
        match state
            .search
            .cache
            .prepare(&frame[..frame.len() - dock], &state.search.query)
        {
            Some(matches) => matches,
            // The normalized query did not change (e.g. a trailing space was
            // typed or deleted): the cached set is this query's set.
            None => state.search.matches.clone(),
        }
    };
    let has_matches = !matches.is_empty();
    state.search.selected = matches
        .iter()
        .position(|matched| {
            matched
                .segments
                .first()
                .is_some_and(|segment| segment.row >= anchor)
        })
        .or_else(|| has_matches.then(|| matches.len() - 1));
    state.search.matches = matches;
    // The matches here already correspond to the current rendered frame, so
    // `refresh_search` keeps them as-is on the next tick (no forced refresh)
    // and retains the selection by match key.
    state.search.frame_version = core.frame_version();
    if core.windowed_viewport() {
        state.search.walk.start(&state.search.query);
    } else {
        state.search.walk.cancel();
    }
    if has_matches {
        scroll_to_match(core, state, dock_height);
    } else if normalize_query(&state.search.query).is_empty() {
        // An emptied query keeps today's behavior (the view stays where the
        // last reveal left it); it also ends the search's claim on the
        // viewport, so the next query starts from the current position.
        state.search.revealed = false;
        if state.search.origin_offset.is_some() {
            state.search.origin_offset = Some(core.scroll_offset());
            state.search.origin_pinned = core.follow_suppressed();
        }
    } else {
        // A non-empty query with zero matches must not leave the viewport at
        // a position a previous prefix's reveal scrolled it to (the play
        // test's "zzz": its first `z` matched `zeta` and jumped). Return to
        // the pre-search view; a query typed without ever matching never
        // moved it, so the restore is a no-op then.
        state.search.restore_origin(core);
        state.follow_pinned_at_end = core.follow_suppressed();
    }
}
pub fn refresh_search(
    state: &mut AltScreenState,
    frame: &[String],
    dock_height: usize,
    frame_version: u64,
) -> bool {
    refresh_search_impl(state, frame, dock_height, frame_version, None, false)
}

/// [`refresh_search`] with the renderer's changed-row hint: `None` means no
/// raw frame row changed since the previous render, `Some((first, last))`
/// means only that inclusive range may have. The hint skips the whole-band
/// fingerprint pass on an unchanged frame and narrows it to the changed rows
/// otherwise; row-count changes and query edits still force the full pass.
pub fn refresh_search_with_hint(
    state: &mut AltScreenState,
    frame: &[String],
    dock_height: usize,
    frame_version: u64,
    changed_rows: Option<(usize, usize)>,
) -> bool {
    refresh_search_impl(state, frame, dock_height, frame_version, changed_rows, true)
}

fn refresh_search_impl(
    state: &mut AltScreenState,
    frame: &[String],
    dock_height: usize,
    frame_version: u64,
    changed_rows: Option<(usize, usize)>,
    trust_hint: bool,
) -> bool {
    if !state.search.active {
        return false;
    }
    // `u64::MAX` is the tests' "force a refresh" sentinel.
    let forced = state.search.frame_version == u64::MAX;
    if !forced && state.search.frame_version == frame_version {
        return false;
    }
    let transcript = &frame[..frame.len().saturating_sub(dock_height.min(frame.len()))];
    // The hint indexes the FULL frame; clip it to the transcript (the dock
    // changes cannot move a match) and keep `None` = no transcript row changed.
    let hint = if trust_hint {
        changed_rows.and_then(|(first, last)| {
            if transcript.is_empty() || first >= transcript.len() {
                None
            } else {
                Some((first, last.min(transcript.len() - 1)))
            }
        })
    } else {
        None
    };
    if trust_hint
        && hint.is_none()
        && state.search.cache.is_current(&state.search.query)
        && state.search.cache.cached_rows() == transcript.len()
    {
        // Nothing under the search changed: keep the match set and selection
        // without hashing a single row.
        state.search.frame_version = frame_version;
        return true;
    }
    let previous_key = state
        .search
        .selected_match()
        .map(match_key)
        .unwrap_or_default();
    // Content-keyed: a rendered frame whose text matches the cached one does
    // no corpus work even though its version advanced (`TuiCore` bumps the
    // version on every render, unchanged or not).
    let recomputed = if trust_hint {
        state
            .search
            .cache
            .prepare_with_hint(transcript, &state.search.query, hint)
    } else {
        state.search.cache.prepare(transcript, &state.search.query)
    };
    state.search.frame_version = frame_version;
    let Some(matches) = recomputed else {
        // Same query + same rendered text: the cached match set — and with
        // it the selection — is already current. The version still moved, so
        // report the accepted refresh.
        return true;
    };
    state.search.matches = matches;
    if previous_key.is_empty() {
        // First search on this query: keep nothing selected so the box shows
        // `n/m` without a current match until the user navigates (upstream
        // `selectedIndex = -1` on a fresh query).
        state.search.selected = None;
        return true;
    }
    state.search.selected = state
        .search
        .matches
        .iter()
        .position(|matched| match_key(matched) == previous_key);
    true
}

// ---------------------------------------------------------------------------
// OSC 8 hyperlinks (upstream `utils.ts::getOsc8LinkAtColumn` + `openUrl`)
// ---------------------------------------------------------------------------

/// The OSC 8 hyperlink covering a visible terminal column of one frame line —
/// the pure port of upstream `getOsc8LinkAtColumn` (`utils.ts:358-382`), the
/// function `TuiAltScreen.handleSelectionMouseEvent` calls on a left press to
/// remember the link under the pointer (`pressedUrl`).
///
/// The walk is upstream's: escape sequences are consumed without advancing a
/// column (upstream `extractAnsiCode` / `ansiCodeLength`), an OSC 8 sequence
/// (`ESC ] 8 ; <params> ; <url> <BEL | ESC \>`) makes its URL the active one,
/// a sequence with an empty URL closes the link (upstream's
/// `hyperlink[1] || undefined`), and every visible grapheme cluster covers
/// `column .. column + width` — a tab counts three cells. The FIRST cluster
/// whose span contains `column` answers with the URL active at that cell;
/// columns past the line's last cell answer `None` (upstream's loop simply
/// runs out).
///
/// Anything else in the line is not a link: an OSC sequence that is not OSC 8,
/// or an `8;` code with no second `;` (`ESC ] 8 ; x BEL`), leaves the active
/// URL untouched, and an escape sequence that never terminates is ordinary
/// text (upstream `extractAnsiCode` answers `null` for it).
pub fn osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut active: Option<String> = None;
    let mut current_column = 0usize;
    let mut position = 0usize;
    while position < chars.len() {
        if chars[position] == '\x1b' {
            let Some(length) = ansi_code_length_at(&chars, position) else {
                // An unterminated escape is text, not a sequence (upstream
                // `extractAnsiCode` returns null): it advances by one char and
                // measures as a zero-width cluster like any other control.
                position += 1;
                continue;
            };
            let code: String = chars[position..position + length].iter().collect();
            if let Some(url) = osc8_url_in_code(&code) {
                active = (!url.is_empty()).then_some(url);
            }
            position += length;
            continue;
        }
        // The text run up to the next POSITION where a valid escape sequence
        // starts (upstream's `textEnd` scan); clusters inside it advance the
        // column counter and the covering one answers.
        let run_start = position;
        while position < chars.len()
            && (chars[position] != '\x1b' || ansi_code_length_at(&chars, position).is_none())
        {
            position += 1;
        }
        let run: String = chars[run_start..position].iter().collect();
        for cluster in UnicodeSegmentation::graphemes(run.as_str(), true) {
            let width = if cluster == "\t" {
                3
            } else {
                ansi::grapheme_width(cluster)
            };
            if column >= current_column && column < current_column + width {
                return active;
            }
            current_column += width;
        }
    }
    None
}

/// Length in chars of the ANSI/OSC/APC escape sequence starting at `position`,
/// `None` when no sequence starts there (upstream `ansiCodeLength`,
/// `utils.ts:452-482`). Upstream counts a CSI sequence only when it ends in one
/// of the finals it recognizes (`m`/`G`/`K`/`H`/`J`), and an OSC/APC string
/// only when it is terminated (`BEL` or `ESC \`); anything else is text.
fn ansi_code_length_at(chars: &[char], position: usize) -> Option<usize> {
    if chars.get(position) != Some(&'\x1b') {
        return None;
    }
    match chars.get(position + 1) {
        Some('[') => {
            for (offset, c) in chars[position + 2..].iter().enumerate() {
                if matches!(c, 'm' | 'G' | 'K' | 'H' | 'J') {
                    return Some(offset + 3);
                }
            }
            None
        }
        Some(']') | Some('_') => {
            let mut index = position + 2;
            while index < chars.len() {
                if chars[index] == '\x07' {
                    return Some(index + 1 - position);
                }
                if chars[index] == '\x1b' && chars.get(index + 1) == Some(&'\\') {
                    return Some(index + 2 - position);
                }
                index += 1;
            }
            None
        }
        _ => None,
    }
}

/// The URL an extracted OSC 8 code activates — `Some(String::new())` when the
/// code CLOSES the link — or `None` when the code is not an OSC 8 hyperlink.
///
/// Upstream tests the extracted code against
/// `/^\x1b\]8;[^;]*;([^\x07\x1b]*)(?:\x07|\x1b\\)$/` (`utils.ts:365-367`),
/// which fixes the split exactly: the params group is `[^;]*`, so the FIRST
/// `;` after `8;` ends it, and the URL is everything after that `;` up to the
/// terminator — semicolons INCLUDED, since its group is `[^\x07\x1b]*` (only
/// the terminator bytes are excluded). Probed against the regex:
/// `ESC]8;a=b;c;real BEL` activates `c;real` and `ESC]8;a;b;c BEL` activates
/// `b;c`. An `8;` code with no second `;` at all matches nothing, so the
/// caller leaves the active URL untouched.
fn osc8_url_in_code(code: &str) -> Option<String> {
    let rest = code.strip_prefix("\x1b]8;")?;
    let body = rest
        .strip_suffix('\x07')
        .or_else(|| rest.strip_suffix("\x1b\\"))?;
    // Everything after the first `;` is the URL (upstream's `[^;]*;` prefix);
    // with no second `;` there is no URL at all.
    let url = body.split_once(';')?.1;
    if url.contains('\x07') || url.contains('\x1b') {
        return None;
    }
    Some(url.to_string())
}

// ---------------------------------------------------------------------------
// Mouse routing (upstream parseWheelEvent/routeWheel + selection handlers)
// ---------------------------------------------------------------------------

/// What the event loop should do with one mouse event (the side the router
/// cannot perform itself, mirroring [`AltKeyRoute`] for keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AltMouseRoute {
    /// Nothing changed: no repaint, nothing for the caller to do.
    Ignore,
    /// The alt-screen interaction layer changed the frame: repaint.
    Repaint,
    /// Upstream `handleRightClickPaste`'s `onRightClickPaste`: the caller
    /// reads the clipboard and injects it into the focused component as ONE
    /// bracketed paste (the gate's paste path, see the function docs).
    PasteClipboard,
    /// Upstream `TuiAltScreenOptions.openUrl`: the caller opens `url` in the
    /// browser. Returned by `handleSelectionMouseEvent`'s release branch when
    /// a left click lands on the same cell as its press and that cell carried
    /// an OSC 8 hyperlink — the click is consumed (no selection is kept, no
    /// copy-on-select runs) exactly like upstream's early `return`.
    OpenUrl(String),
}

/// Route one crossterm mouse event through the alt-screen family (upstream
/// `TuiAltScreen.handleMouseEvent`'s pipeline) and report the side effect the
/// caller owns.
///
/// Pipeline order (upstream `tui-alt-screen.ts:906-961`): wheel → scrollbar
/// drag continuation → search navigation buttons → jump-to-end indicator →
/// scrollbar press → scrollbar hover → right-click paste → text selection.
/// The event loop must already have established that no dialog/selector owns
/// the focus — except for the wheel, whose primary fallback deliberately
/// reaches the transcript under an overlay (D18). Callers that keep the
/// overlay state in [`AltScreenState::set_overlay_focus`] get the scrollbar
/// and the buttons to refuse gestures too.
pub fn handle_mouse_event_route(
    event: MouseEvent,
    core: &mut TuiCore,
    state: &mut AltScreenState,
    config: &AltScreenConfig,
    wheel: &mut WheelScrollAccelerator,
    dock_height: usize,
    copy_text: &dyn Fn(&str) -> bool,
) -> AltMouseRoute {
    let rows = core.rows().max(1);
    let width = core.columns().max(1);
    // The clip the last frame painted (see [`clip_geometry`]): the scrollbar
    // and indicator hit-tests resolve through it exactly like their paint.
    let clip = clip_geometry(&core.painted_layout());
    let transient = state.scrollbar_linger.is_some();
    // Keep the decorator's follow-pin mirror and screen-height hint in sync
    // with the core.
    state.follow_pinned_at_end = core.follow_suppressed();
    state.screen_rows = core.rows().max(1);

    // Wheel (upstream `handleViewportInput`'s wheel branch → `routeWheel`),
    // which also refreshes the scrollbar hover (`routeWheel`'s
    // `updateScrollbarHover`).
    if matches!(
        event.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        let direction: i32 = if event.kind == MouseEventKind::ScrollUp {
            -1
        } else {
            1
        };
        // Alt+wheel moves five times as far (upstream
        // `ALT_WHEEL_SCROLL_MULTIPLIER`, applied when the SGR button code
        // carries the Alt bit — crossterm reports it as a modifier).
        let multiplier = if event.modifiers.contains(KeyModifiers::ALT) {
            ALT_WHEEL_SCROLL_MULTIPLIER
        } else {
            1
        };
        let lines = wheel.next(direction, now_millis()) * multiplier;
        // Upstream `routeWheel`'s delta sign: a wheel notch moves
        // `lines` rows toward the end (positive = down). pir's
        // offset-from-end negates it (wheel-up → earlier content), so the
        // chain runs in upstream's top-anchored `scrollTop` units and the
        // transcript converts at the boundary.
        route_wheel_event(
            core,
            state,
            dock_height,
            rows,
            event.row as usize,
            direction as i64 * lines as i64,
        );
        update_scrollbar_hover(state, config, &clip, width, &event, transient);
        return AltMouseRoute::Repaint;
    }

    // A live scrollbar drag owns every pointer event (upstream
    // `handleScrollbarMouseEvent`'s drag branch): the release ends it, any
    // other pointer event keeps scrubbing — and no text selection can start
    // while the bar is held.
    if let Some(grab_offset) = state.scrollbar_drag {
        if matches!(event.kind, MouseEventKind::Up(_)) {
            state.scrollbar_drag = None;
            return AltMouseRoute::Repaint;
        }
        if let Some(geometry) = scrollbar_geometry(clip, width, config.scrollbar, true, transient) {
            scroll_scrollbar_to_pointer(core, state, &geometry, event.row as usize, grab_offset);
        }
        return AltMouseRoute::Repaint;
    }

    let mut repaint = false;

    // Search navigation buttons (upstream `handleSearchMouseEvent`): the
    // hover follows the pointer on every event, a left press navigates. The
    // control is checked BEFORE the indicator and the scrollbar, and a press
    // inside the search overlay never reaches the selection gesture.
    let nav_direction = if state.search.active && !state.overlay_focus {
        search_box_layout(width).and_then(|layout| {
            search_navigation_direction_at(&layout, event.row as usize, event.column as usize)
        })
    } else {
        None
    };
    if state.search.set_hovered_navigation(nav_direction) {
        repaint = true;
    }
    if let Some(direction) = nav_direction {
        if is_left_press(&event) {
            navigate_search(core, state, dock_height, direction.delta());
            return AltMouseRoute::Repaint;
        }
    }

    // Jump-to-end indicator (upstream `handleScrollToEndIndicatorMouseEvent`):
    // a left press anywhere inside the painted label scrolls to the bottom
    // (`scrollToBottom` → `ScrollView.scrollToEnd`, which resumes follow); a
    // press next to it falls through to the selection.
    if !state.overlay_focus && !core.is_following() {
        let label = scroll_to_end_indicator_text();
        let bar_column = scrollbar_geometry(clip, width, config.scrollbar, false, transient)
            .map(|geometry| geometry.column);
        if let Some(rect) =
            scroll_to_end_indicator_rect(clip, width, bar_column, ansi::visible_width(&label))
        {
            if is_left_press(&event)
                && event.row as usize == rect.row
                && event.column as usize >= rect.column
                && (event.column as usize) < rect.column + rect.width
            {
                // Upstream `scrollToEnd()` marks scrollbar activity when the
                // view actually changed (scrollTop or follow state).
                let changed = core.scroll_offset() != 0 || core.follow_suppressed();
                core.set_scroll_offset(0);
                state.follow_pinned_at_end = false;
                if changed {
                    state.scrollbar_linger = Some(Instant::now());
                }
                return AltMouseRoute::Repaint;
            }
        }
    }

    // Scrollbar press (upstream `handleScrollbarMouseEvent`'s press branch):
    // a left press on the bar's column over the track grabs it — on the thumb
    // with a relative grab, on the track with a center grab plus an immediate
    // jump to the pointer. The press clears the selection gesture (the bar
    // consumes it).
    //
    // The hit-test passes `include_hidden_auto = true`, a deliberate
    // deviation from upstream's visible-bar-only press: a hidden `"auto"`
    // bar has already faded by the time the user presses it again (e.g.
    // after dragging to the top, waiting out `SCROLLBAR_LINGER` and pressing
    // the same column), and the press itself proves the pointer is on the
    // track. Without this the press fell through to text selection, so the
    // second drag never moved the view. `scrollbar_geometry` still returns
    // `None` for `"hidden"` and for a transcript without overflow, so a
    // press on empty space stays a selection.
    if !state.overlay_focus && is_left_press(&event) {
        if let Some(geometry) = scrollbar_geometry(clip, width, config.scrollbar, true, transient) {
            let pointer_column = event.column as usize;
            let pointer_row = event.row as usize;
            let track_row = pointer_row.saturating_sub(geometry.track_top);
            let on_track = pointer_row >= geometry.track_top && track_row < geometry.track_height;
            if pointer_column == geometry.column && on_track {
                let on_thumb = track_row >= geometry.thumb_top
                    && track_row < geometry.thumb_top + geometry.thumb_height;
                let grab_offset = if on_thumb {
                    track_row - geometry.thumb_top
                } else {
                    geometry.thumb_height / 2
                };
                state.set_scrollbar_hover(true);
                state.clear_selection_gesture();
                if !on_thumb {
                    scroll_scrollbar_to_pointer(core, state, &geometry, pointer_row, grab_offset);
                }
                state.scrollbar_drag = Some(grab_offset);
                return AltMouseRoute::Repaint;
            }
        }
    }

    // Scrollbar hover (upstream `if (!this.scrollbarDrag)
    // this.updateScrollbarHover(...)`): with `includeHiddenAuto = true`, so
    // merely entering a hidden `"auto"` bar's track reveals it in the active
    // glyph and keeps it up while the pointer stays.
    if update_scrollbar_hover(state, config, &clip, width, &event, transient) {
        repaint = true;
    }

    // Windows right-click paste (upstream `handleRightClickPaste`): the caller
    // reads the clipboard and injects it as a bracketed paste into the focused
    // component — see [`AltMouseRoute::PasteClipboard`].
    if matches!(event.kind, MouseEventKind::Down(MouseButton::Right))
        && is_right_click_paste(
            &event,
            cfg!(windows),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
        )
    {
        return AltMouseRoute::PasteClipboard;
    }

    // The caller's gate (`transcript_mouse_route`) already stops every
    // non-wheel event while a dialog/selector owns the focus; when the state
    // has been told about the overlay anyway, no gesture may start here.
    if state.overlay_focus {
        return repaint_route(repaint);
    }

    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // A press clears any previous selection and starts a new gesture
            // (upstream `handleSelectionMouseEvent` press branch). A second
            // press on the same word within [`DOUBLE_CLICK_INTERVAL`] selects
            // that word; a third selects the whole line. The OSC 8 hyperlink
            // under the press point is only captured for a character-granularity
            // press (upstream `pressedUrl = range ? undefined : …`) — a
            // word/line click never opens a link.
            let point = mouse_point(&event, core);
            let line = plain_frame_line(core, point.0);
            let word = word_selection_columns(&line, point.1);
            let click_count = state.next_click_count(point.0, word, Instant::now());
            let range = match click_count {
                2 => word.map(|(start_col, end_col)| SelectionSpan {
                    start: (point.0, start_col),
                    end: (point.0, end_col),
                }),
                3 => {
                    let (start_col, end_col) = line_selection_columns(&line);
                    Some(SelectionSpan {
                        start: (point.0, start_col),
                        end: (point.0, end_col),
                    })
                }
                _ => None,
            };
            state.selection_granularity = match click_count {
                2 => SelectionGranularity::Word,
                3 => SelectionGranularity::Line,
                _ => SelectionGranularity::Character,
            };
            state.selection_initial_range = range;
            state.selection_anchor = Some(range.map_or(point, |span| span.start));
            state.selection_focus = Some(range.map_or(point, |span| span.end));
            state.selection_dragged = false;
            state.selection = range;
            state.pressed_url = if range.is_some() {
                None
            } else {
                osc8_link_at_point(core, &event)
            };
            AltMouseRoute::Repaint
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(anchor) = state.selection_anchor else {
                return repaint_route(repaint);
            };
            let focus = mouse_point(&event, core);
            state.selection_dragged = anchor != focus;
            // Upstream's motion branch drops the remembered link and the
            // click chain unconditionally: a drag that wanders back onto the
            // press cell is still not a click (`selectionDragged` is set
            // there), and the next press starts a fresh click count.
            state.pressed_url = None;
            state.last_click = None;
            let line = plain_frame_line(core, focus.0);
            state.update_selection_focus(focus, &line);
            state.selection = state.gesture_span();
            AltMouseRoute::Repaint
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(anchor) = state.selection_anchor else {
                return repaint_route(repaint);
            };
            let point = mouse_point(&event, core);
            let line = plain_frame_line(core, point.0);
            // Upstream `updateSelectionFocus` runs on release too, so a drag
            // ending inside a word snaps the range to that word.
            state.update_selection_focus(point, &line);
            // Upstream's `isClick`: nothing dragged and the release landed
            // back on the press point (the scroll-view identity is trivially
            // satisfied — pir paints one scrollable, and the pin/mirror stays
            // consistent). It decides whether the remembered link counts; the
            // release always drops it (`clickedUrl = isClick ? pressedUrl :
            // undefined`, then `pressedUrl = undefined`).
            let is_click = !state.selection_dragged && anchor == point;
            let clicked_url = if is_click {
                state.pressed_url.take()
            } else {
                state.pressed_url = None;
                None
            };
            if let Some(url) = clicked_url {
                // Upstream clears the selection it was about to make
                // (`selectionAnchor`/`selectionFocus = undefined`) and consumes
                // the click: no component dispatch, no copy-on-select, no
                // flash — `openUrl` is invoked instead (best-effort there; the
                // caller launches the browser here).
                state.selection = None;
                state.selection_dragged = false;
                state.selection_anchor = None;
                state.selection_focus = None;
                state.selection_granularity = SelectionGranularity::Character;
                state.selection_initial_range = None;
                return AltMouseRoute::OpenUrl(url);
            }
            let span = state.gesture_span();
            // The gesture is over: drop the anchor/focus (the painted
            // `selection` is kept). The granularity and initial range stay
            // until the next press/scrollbar grab, exactly like upstream's
            // `selectionGranularity` / `selectionInitialRange`.
            state.selection_anchor = None;
            state.selection_focus = None;
            let Some(span) = span else {
                // Plain click (or a release that came back to the press
                // point): no selection, no copy (upstream clears on click
                // without drag and does not copy).
                state.selection = None;
                return repaint_route(repaint);
            };
            state.selection = Some(span);
            if config.copy_on_select {
                let frame = core.last_frame_lines();
                let copied = selection_text(frame, &span)
                    .map(|text| copy_text(&text))
                    .unwrap_or(false);
                if copied {
                    state.flash("Copied!", DEFAULT_FLASH_DURATION);
                } else {
                    state.flash("Copy failed", COPY_ERROR_FLASH_DURATION);
                }
            }
            AltMouseRoute::Repaint
        }
        // Non-left presses and horizontal wheel events have no alt-screen
        // behavior (upstream `routeWheel` only handles the vertical wheel and
        // the left-button selection gesture); the hover above may still have
        // changed.
        MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::Down(_)
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => repaint_route(repaint),
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => AltMouseRoute::Ignore,
    }
}

/// "Repaint when the interaction layer changed" as a route.
fn repaint_route(repaint: bool) -> AltMouseRoute {
    if repaint {
        AltMouseRoute::Repaint
    } else {
        AltMouseRoute::Ignore
    }
}

/// [`handle_mouse_event_route`] reduced to "repaint?" for callers that do not
/// own the Windows right-click paste (upstream returns nothing to render for
/// it: the paste handler requests its own render after injecting the text).
pub fn handle_mouse_event(
    event: MouseEvent,
    core: &mut TuiCore,
    state: &mut AltScreenState,
    config: &AltScreenConfig,
    wheel: &mut WheelScrollAccelerator,
    dock_height: usize,
    copy_text: &dyn Fn(&str) -> bool,
) -> bool {
    matches!(
        handle_mouse_event_route(event, core, state, config, wheel, dock_height, copy_text),
        AltMouseRoute::Repaint
    )
}

/// Whether the event is a bare left-button press (upstream's
/// `!event.release && (button & 32) === 0 && (button & 3) === 0`).
fn is_left_press(event: &MouseEvent) -> bool {
    matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
}

/// The Windows right-click paste gate (upstream `handleRightClickPaste`):
/// `process.platform === "win32"`, `TERM_PROGRAM` is not VS Code, the event is
/// a PRESS of the bare secondary button (modifiers make it a different SGR
/// button code upstream, which fails the `button !== 2` test).
pub fn is_right_click_paste(event: &MouseEvent, windows: bool, term_program: Option<&str>) -> bool {
    if !right_click_paste_enabled(windows, term_program) {
        return false;
    }
    if event
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
    {
        return false;
    }
    matches!(event.kind, MouseEventKind::Down(MouseButton::Right))
}

/// The platform half of [`is_right_click_paste`] (upstream's
/// `process.platform !== "win32"` / `TERM_PROGRAM === "vscode"` checks).
pub fn right_click_paste_enabled(windows: bool, term_program: Option<&str>) -> bool {
    windows && !term_program.is_some_and(|program| program.eq_ignore_ascii_case("vscode"))
}

/// Upstream `updateScrollbarHover(x, y, includeHiddenAuto = true)`: the
/// pointer's track hit → [`AltScreenState::set_scrollbar_hover`]. Returns
/// whether the hover changed (repaint needed).
fn update_scrollbar_hover(
    state: &mut AltScreenState,
    config: &AltScreenConfig,
    clip: &ClipGeometry,
    width: usize,
    event: &MouseEvent,
    transient: bool,
) -> bool {
    if state.overlay_focus {
        return state.set_scrollbar_hover(false);
    }
    let hit = scrollbar_geometry(*clip, width, config.scrollbar, true, transient).is_some_and(
        |geometry| {
            let row = event.row as usize;
            event.column as usize == geometry.column
                && row >= geometry.track_top
                && row < geometry.track_top + geometry.track_height
        },
    );
    state.set_scrollbar_hover(hit)
}

/// Upstream `scrollScrollbarToPointer`: map the pointer row to a thumb offset
/// with the grab offset, then `ScrollView.scrollTo(scrollTop)` — without
/// `disableFollow`, so landing on the end resumes follow; the transient bar is
/// marked when the view actually moved.
fn scroll_scrollbar_to_pointer(
    core: &mut TuiCore,
    state: &mut AltScreenState,
    geometry: &ScrollbarGeometry,
    pointer_row: usize,
    grab_offset: usize,
) {
    let max_thumb_offset = geometry.track_height.saturating_sub(geometry.thumb_height);
    let relative = pointer_row as i64 - geometry.track_top as i64 - grab_offset as i64;
    let thumb_offset = relative.clamp(0, max_thumb_offset as i64) as usize;
    let scroll_top = if max_thumb_offset == 0 {
        0
    } else {
        ((thumb_offset as f64 / max_thumb_offset as f64) * geometry.max_scroll_top as f64).round()
            as usize
    };
    let offset = geometry
        .max_scroll_top
        .saturating_sub(scroll_top.min(geometry.max_scroll_top));
    let moved = offset != core.scroll_offset() || core.follow_suppressed();
    core.set_scroll_offset(offset);
    state.follow_pinned_at_end = false;
    if moved {
        state.scrollbar_linger = Some(Instant::now());
    }
}

/// The OSC 8 hyperlink under a pointer position (upstream
/// `getOsc8LinkAtColumn(this.previousScreen[clamp(y)] ?? "", clamp(x))`).
///
/// The screen row resolves through [`TuiCore::screen_row_to_buffer_row`] — the
/// same mapping the selection coordinates use — and a row the last frame did
/// not paint at all answers `None` (upstream's `previousScreen[y] ?? ""`).
/// The column is clamped into the terminal like [`mouse_point`]'s, and the
/// line is the undecorated frame line the row shows (see the module header's
/// deviation note: pir's decorations preserve OSC 8 runs, and this keeps the
/// lookup on the selection's line corpus).
fn osc8_link_at_point(core: &TuiCore, event: &MouseEvent) -> Option<String> {
    let row = core.screen_row_to_buffer_row(event.row as usize)?;
    let column = (event.column as usize).min(core.columns().max(1).saturating_sub(1));
    let line = core.last_frame_lines().get(row)?;
    osc8_link_at_column(line, column)
}

/// Screen point → absolute buffer point through the last frame's painted
/// layout ([`TuiCore::screen_row_to_buffer_row`]): transcript rows resolve
/// through the painted window, and the dock rows (editor, footer, open
/// dialog/selector) resolve to their own buffer rows — a drag over "the
/// content below the transcript" selects exactly the line on screen. A row
/// below the painted content clamps to the last buffer row (dragging out of
/// the frame selects to its end).
fn mouse_point(event: &MouseEvent, core: &TuiCore) -> (usize, usize) {
    let frame_len = core.frame_buffer_length();
    let row = core
        .screen_row_to_buffer_row(event.row as usize)
        .unwrap_or_else(|| frame_len.saturating_sub(1));
    let col = (event.column as usize).min(core.columns().max(1).saturating_sub(1));
    (row, col)
}

fn order_span(a: (usize, usize), b: (usize, usize)) -> SelectionSpan {
    if (a.0, a.1) <= (b.0, b.1) {
        SelectionSpan { start: a, end: b }
    } else {
        SelectionSpan { start: b, end: a }
    }
}

fn now_millis() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Build the wheel accelerator from the parsed `fullscreenWheelScrollLines`
/// setting (`None` = unset/"auto"; upstream default is `"auto"`).
pub fn wheel_from_setting(setting: Option<u32>) -> WheelScrollAccelerator {
    match setting {
        Some(lines) => WheelScrollAccelerator::new(WheelScrollLines::Lines(lines)),
        None => WheelScrollAccelerator::new(WheelScrollLines::Auto),
    }
}
