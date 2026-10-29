//! Tail-first paged session reads over a memory-mapped file.
//!
//! MEMORY SHAPE (pir extension): a 100 MB session must not cost 100 MB of
//! resident `String`s just to answer "what are the newest entries / when was
//! this session last touched". [`SessionTailPage`] maps the file read-only and
//! parses **only a byte- and entry-capped tail window**; the skipped prefix is
//! never parsed, never converted to `Message`s, and — for the counters below —
//! never even scanned.
//!
//! Why mmap is used here and NOT on the full-open path: resident mapped pages
//! count toward the process working set, so mapping the whole file to parse it
//! would trade heap RSS for the same RSS in the mapped-file bucket. A tail page
//! touches only its own bytes, which is exactly the bounded read the budget
//! asks for; the FULL load (`SessionManager::open`) keeps the bounded streaming
//! reader with a 1 MiB buffer.
//!
//! Invariants:
//!
//! - **Append-only input.** Like upstream (and every pir writer), session files
//!   are only ever appended to. A mapped length is fixed at map time, so a page
//!   reflects the file as of `open`; a later append is picked up by re-opening
//!   (or by the manager's own file handle), never by the page's map.
//! - **Fallback, never a scan.** When the file cannot be mapped (not a regular
//!   file, no mapping permission) or is empty, the page reports itself
//!   unavailable and callers fall back to the streaming reader. There is no
//!   "map the whole file anyway" path.
//! - **No whole-file scan for the counters.** The page records the bytes it
//!   parsed (`page_bytes`) and the file's total size from metadata
//!   (`file_bytes`); it does NOT count the prefix's lines — that would touch
//!   every page of a 100 MB file. `skipped_bytes = file_bytes - page_bytes`
//!   is the scan-free equivalent of "how much was NOT parsed".
//! - **Read-only.** The page never writes; the repair of a torn trailing line
//!   belongs to the full loader ([`super::SessionManager::open`]).
//!
//! [`super::SessionManager::open_paged`] builds the paged manager on top of
//! this type.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::types::{FileEntry, SessionHeader};
use super::{parse_entry_line, parse_entry_line_with_version};

/// Default byte cap for a tail page (the page is also entry-capped; whichever
/// binds first wins).
///
/// Sizing (measured): the page is a FIXED resident cost on top of the process
/// floor — the parsed entries dominate, so 1 MiB / 1024 entries keeps an
/// ordinary session's window (and every keep-recent cut window: the default
/// `keepRecentTokens` of 20000 is ~80 KB of text) fully covered while leaving
/// ~4x headroom over the previous 4 MiB default.
pub const DEFAULT_TAIL_PAGE_BYTES: usize = 1024 * 1024;

/// Default entry cap for a tail page.
pub const DEFAULT_TAIL_PAGE_ENTRIES: usize = 1024;

/// Largest first-line read used to resolve the session header's version (the
/// header is a single small JSON object; a pathological first line is capped
/// instead of being read whole).
const HEADER_PROBE_BYTES: usize = 64 * 1024;

/// Counters of one tail page read — the evidence that an open parsed only the
/// tail (see the module docs for why the prefix's line count is deliberately
/// not computed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TailPageStats {
    /// Bytes of the file, from `metadata` (no scan).
    pub file_bytes: u64,
    /// Bytes parsed (the tail window).
    pub page_bytes: u64,
    /// Non-blank lines parsed.
    pub parsed_lines: usize,
    /// Parseable entries in the page.
    pub parsed_entries: usize,
    /// Non-blank lines in the page that failed to parse (preserved verbatim).
    pub unparsed_lines: usize,
    /// `file_bytes - page_bytes`: the prefix that was NOT touched.
    pub skipped_bytes: u64,
    /// Whether the page covers the whole file (nothing was skipped).
    pub whole_file: bool,
}

/// A read-only, tail-first page of a session file (see the module docs).
pub struct SessionTailPage {
    path: PathBuf,
    map: memmap2::Mmap,
    /// First byte of the parsed window.
    page_start: u64,
    /// First byte of the region this page was allowed to cover (the file end,
    /// or the previous page's start for a "load more" step).
    window_end: u64,
    entries: Vec<Arc<FileEntry>>,
    unparsed_lines: Vec<String>,
    header: Option<SessionHeader>,
    stats: TailPageStats,
}

impl std::fmt::Debug for SessionTailPage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionTailPage")
            .field("path", &self.path)
            .field("stats", &self.stats)
            .finish()
    }
}

impl SessionTailPage {
    /// Map `path` and parse its newest entries, bounded to `max_bytes` and
    /// `max_entries` (whichever cap binds first). `Ok(None)` means "no usable
    /// page" (missing/empty file, or the file cannot be mapped) — the caller
    /// falls back to the streaming reader.
    pub fn open(
        path: &Path,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Option<SessionTailPage>> {
        Self::open_window(path, None, max_bytes, max_entries)
    }

    /// [`Self::open`] over the region `[..end)` (a "load more" step continues
    /// from the previous page's start).
    pub fn open_window(
        path: &Path,
        end: Option<u64>,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Option<SessionTailPage>> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(None),
        };
        if !metadata.is_file() || metadata.len() == 0 {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        // Safety: a read-only map of an append-only file. The map is never
        // written through, its length is fixed at map time, and every writer
        // in this process appends through its own handle (see the module
        // docs). A file truncated by another process would be a violation of
        // the append-only invariant, not of this call.
        let map = match map_read_only(&file) {
            Ok(map) => map,
            // Mapping is unavailable (not a regular file on some platforms,
            // permission, ...): the caller uses the streaming reader.
            Err(_) => return Ok(None),
        };
        let file_len = map.len() as u64;
        let window_end = end.map(|end| end.min(file_len)).unwrap_or(file_len);
        let header_version = read_header_version(&map);
        let page_start = find_page_start(&map[..window_end as usize], max_bytes, max_entries);
        let (entries, unparsed_lines, parsed_lines) = parse_window(
            &map[page_start as usize..window_end as usize],
            header_version,
        );
        let header = entries
            .iter()
            .find(|entry| entry.is_header())
            .and_then(|entry| entry.as_header())
            .cloned();
        let stats = TailPageStats {
            file_bytes: metadata.len().max(file_len),
            page_bytes: window_end - page_start as u64,
            parsed_lines,
            parsed_entries: entries.len(),
            unparsed_lines: unparsed_lines.len(),
            skipped_bytes: page_start as u64,
            whole_file: page_start == 0 && window_end == file_len,
        };
        Ok(Some(SessionTailPage {
            path: path.to_path_buf(),
            map,
            page_start: page_start as u64,
            window_end,
            entries,
            unparsed_lines,
            header,
            stats,
        }))
    }

    /// The file this page came from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The counters of this read (see [`TailPageStats`]).
    pub fn stats(&self) -> TailPageStats {
        self.stats
    }

    /// The parsed tail entries, oldest first. The first entry is NOT
    /// necessarily the session header (only when [`TailPageStats::whole_file`]
    /// is set).
    pub fn entries(&self) -> &[Arc<FileEntry>] {
        &self.entries
    }

    /// Non-blank lines of the page that failed to parse (preserved verbatim so
    /// a caller that materializes the file never drops them).
    pub fn unparsed_lines(&self) -> &[String] {
        &self.unparsed_lines
    }

    /// The session header, when the page contains it (a whole-file page).
    pub fn header(&self) -> Option<&SessionHeader> {
        self.header.as_ref()
    }

    /// The session header read from the FILE'S FIRST LINE, independent of the
    /// page (a bounded probe, see [`read_header_version`]): a tail page of a
    /// huge session does not contain the header, but the session id and cwd it
    /// carries are part of the identity a caller must not invent. `None` when
    /// the file does not start with a session header.
    ///
    /// This is what [`super::SessionManager::open_paged`] seeds the manager
    /// with, so a paged open reports the file's real session id — reading the
    /// page's first line would mis-identify a huge session.
    pub fn file_header(&self) -> Option<SessionHeader> {
        if let Some(header) = self.header.as_ref() {
            return Some(header.clone());
        }
        let probe = &self.map[..HEADER_PROBE_BYTES.min(self.map.len())];
        let end = probe
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(probe.len());
        let text = String::from_utf8_lossy(&probe[..end]);
        parse_entry_line(&text).and_then(|entry| entry.as_header().cloned())
    }

    /// The session id, when the header is in the page.
    pub fn session_id(&self) -> Option<&str> {
        self.header.as_ref().map(|header| header.id.as_str())
    }

    /// The newest `session_info` name in the page (upstream: latest wins,
    /// empty names clear).
    pub fn session_name(&self) -> Option<String> {
        let mut name: Option<String> = None;
        for entry in &self.entries {
            if let FileEntry::SessionInfo(info) = entry.as_ref() {
                name = info
                    .name
                    .as_ref()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty());
            }
        }
        name
    }

    /// Byte offset of the page inside the file (the prefix length).
    pub fn page_start(&self) -> u64 {
        self.page_start
    }

    /// Total file length at map time.
    pub fn file_len(&self) -> u64 {
        self.map.len() as u64
    }

    /// First byte of the region this page covered (the file end for a normal
    /// open, the previous page's start for a "load more" step).
    pub fn window_end(&self) -> u64 {
        self.window_end
    }

    /// The oldest entry of the page (the point a "load more" step continues
    /// from: the next page back ends here).
    pub fn oldest_entry(&self) -> Option<&Arc<FileEntry>> {
        self.entries.first()
    }

    /// The id of the last parseable entry (the session leaf when the page is
    /// current).
    pub fn leaf_id(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|entry| entry.id())
    }

    /// Read one page further back: re-map `path` and cover the window that ends
    /// where this page starts, still bounded by the byte/entry caps. `Ok(None)`
    /// when this page already started at the file start.
    pub fn extend(
        &self,
        max_bytes: usize,
        max_entries: usize,
    ) -> crate::Result<Option<SessionTailPage>> {
        if self.page_start == 0 {
            return Ok(None);
        }
        Self::open_window(&self.path, Some(self.page_start), max_bytes, max_entries)
    }

    /// Whether the page carries a line that parses as a `compaction` entry —
    /// the "is the newest entry already a compaction" question the paged turn
    /// path asks before summarizing anything.
    pub fn contains_compaction_entry(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.as_ref(), FileEntry::Compaction(_)))
    }
}

/// Map a session file read-only (the one `unsafe` in this module; see the
/// module docs for the append-only invariant that makes the mapping sound).
#[expect(
    unsafe_code,
    reason = "memmap2::Mmap::map is unsafe; a read-only map of an append-only file is sound and never written through"
)]
fn map_read_only(file: &std::fs::File) -> std::io::Result<memmap2::Mmap> {
    // SAFETY: the file is opened read-only, the map is never written through,
    // and the session writer only appends (never truncates or rewrites) bytes
    // already covered by the map, so the mapping stays valid for its lifetime.
    unsafe { memmap2::Mmap::map(file) }
}

/// Parse a byte window into entries, preserving undeserializable non-blank
/// lines verbatim (the same rule the streaming loader applies). Returns
/// `(entries, unparsed_lines, parsed_non_blank_lines)`.
fn parse_window(
    bytes: &[u8],
    header_version: Option<u32>,
) -> (Vec<Arc<FileEntry>>, Vec<String>, usize) {
    let mut entries: Vec<Arc<FileEntry>> = Vec::new();
    let mut unparsed_lines: Vec<String> = Vec::new();
    let mut parsed_lines = 0usize;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(line);
        parsed_lines += 1;
        match parse_entry_line_with_version(&text, header_version) {
            Some(entry) => entries.push(Arc::new(entry)),
            None => {
                if !text.trim().is_empty() {
                    unparsed_lines.push(text.into_owned());
                }
            }
        }
    }
    (entries, unparsed_lines, parsed_lines)
}

/// Read the version off the session header WITHOUT parsing the whole file: the
/// first line is probed within [`HEADER_PROBE_BYTES`] from the map start.
fn read_header_version(map: &[u8]) -> Option<u32> {
    let probe = &map[..HEADER_PROBE_BYTES.min(map.len())];
    let end = probe
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(probe.len());
    let text = String::from_utf8_lossy(&probe[..end]);
    parse_entry_line(&text).and_then(|entry| entry.as_header().and_then(|header| header.version))
}

/// First byte of the tail window: scan backwards from the end and stop at the
/// first boundary that fills the entry cap or the byte cap — only the scanned
/// tail bytes are touched, never the prefix.
///
/// The returned offset is always a line start (0, or the byte after a `\n`),
/// so the window parses as whole lines.
fn find_page_start(bytes: &[u8], max_bytes: usize, max_entries: usize) -> usize {
    let len = bytes.len();
    let max_bytes = max_bytes.max(1);
    let max_entries = max_entries.max(1);
    let mut newlines = 0usize;
    for index in (0..len).rev() {
        if bytes[index] != b'\n' {
            continue;
        }
        newlines += 1;
        if newlines > max_entries || len - index >= max_bytes {
            return index + 1;
        }
    }
    0
}
