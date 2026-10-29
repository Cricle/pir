//! Paged (disk-backed) compaction for a tail-window session.
//!
//! When a session is opened with [`SessionManager::open_paged`] the resident
//! state is a byte-capped tail window; the skipped prefix stays on disk. This
//! module summarizes that prefix **chunk by chunk straight from the file**: the
//! summarizer holds one chunk's messages (bounded by the chunk budget) plus the
//! running summary, never the span.
//!
//! Why: on a 100 MB session the resident parse (every entry as a `String`, then
//! every message as a `Message`) is what makes the load cost megabytes of RSS
//! per megabyte of session — the summarizer REQUESTS are already bounded by the
//! chunker. The span is the remaining cost, so it is streamed.
//!
//! Degradation, named: a prefix that contains a `context_edit` entry cannot be
//! summarized entry-by-entry (an edit rewrites or omits a target entry that may
//! lie earlier in the prefix), so the caller is told
//! ([`PagedCompactionError::Unsupported`]) and widens the session to the full
//! resident view ([`SessionManager::widen_to_full`]) before using the resident
//! path. A paged session whose threshold does NOT trigger must widen too (the
//! model context would otherwise be a tail window) —
//! [`PagedCompactionError::ThresholdNotMet`] reports that case.
//!
//! MEASURED CONSEQUENCE OF THE WIDENING FALLBACK (2026-10-17, `big_session_probe`
//! on a fresh 100 MB session, 2 MB baseline):
//!
//! - 128k window, compaction ON (the paged fast path): **+21.8 MB** peak
//!   process-tree increment, every request ~0.3 MB;
//! - `--context-window 100000000`, i.e. the threshold never fires: **+1353 MB**
//!   and ONE 79.4 MB request — the widened full transcript goes to the provider,
//!   exactly what upstream pi does with compaction off (a real gateway rejects
//!   that by context length). This is a named trade-off, not a defect: pir does
//!   not silently drop history to satisfy a memory budget, and refusing the
//!   request instead would be a semantic decision that needs its own approval.
//!   Documented in `SDK-SCOPE.md` §8. Keep the threshold compaction on (the
//!   default) to stay on the bounded path.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::ai::types::{Message, Model};
use crate::session_manager::{
    parse_entry_line_with_version, FileEntry, PagedWindow, SessionManager, SessionProjection,
    MAX_PREFIX_ENTRY_LINE_BYTES, SESSION_READ_BUFFER_SIZE,
};

use super::algorithm::estimate_context_tokens;
use super::chunking::StreamingChunker;
use super::{
    compute_file_lists, extract_file_ops_from_message_refs, format_file_operations,
    prepare_compaction_refs, should_compact_tokens, BorrowedCompactionPreparation,
    CompactionConfig, CompactionResult, CompleteFn, FileOperations, PathEntryMeta,
    ProjectionEntryMeta, SpanFraming,
};
use crate::session_manager::projection::session_entry_to_context_messages;

/// Why a paged compaction could not run (or failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PagedCompactionError {
    /// The streamed prefix needs the full projection (a `context_edit` entry
    /// lives before the resident window). The caller widens the session and
    /// compacts with the resident path.
    Unsupported(String),
    /// The threshold is not met: the caller decides (widen or skip).
    ThresholdNotMet,
    /// The summarizer or the file read failed (the message is the honest text).
    Failed(String),
}

/// The measured shape of the scan a paged compaction performed — the evidence
/// that nothing beyond the tail window became resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PagedCompactionStats {
    /// Bytes streamed from the skipped prefix.
    pub prefix_bytes: u64,
    /// Prefix entries that contributed to the span.
    pub prefix_entries: usize,
    /// Summarizer calls issued for the prefix.
    pub summarizer_calls: usize,
    /// The per-chunk serialized cap the streamer closed chunks at.
    pub chunk_cap_chars: usize,
    /// Whether a compaction entry inside the prefix supplied the running
    /// summary (the span started after it).
    pub resumed_from_prefix_compaction: bool,
}

/// One paged compaction attempt.
pub struct PagedCompaction {
    pub result: CompactionResult,
    pub stats: PagedCompactionStats,
}

/// The session state a paged compaction needs, captured while the caller holds
/// the session lock. [`compact_paged`] is [`capture`](Self::capture) plus
/// [`compact_paged_inputs`]; a host whose session storage is shared (the print
/// host's `Arc<Mutex<Option<SessionManager>>>`) captures the view under the
/// lock, DROPS the lock, and then runs the provider calls — the same
/// lock discipline `agent_session/compact.rs` uses for the resident path.
///
/// The view is bounded by the paged open caps: the resident tail window's
/// projection (messages + per-entry metadata) and the branch metadata. The
/// skipped prefix stays on disk and is streamed by the summarizer.
pub struct PagedCompactionInputs {
    paged: PagedWindow,
    path: std::path::PathBuf,
    projection: SessionProjection,
    branch: Vec<PathEntryMeta>,
}

impl PagedCompactionInputs {
    /// Read the resident window view out of a paged session. Fails exactly
    /// like [`compact_paged`]'s own entry checks: `Unsupported` for a session
    /// that is not paged, `Failed` for a projection error.
    pub fn capture(session: &SessionManager) -> Result<Self, PagedCompactionError> {
        let paged = session.paged_window().ok_or_else(|| {
            PagedCompactionError::Unsupported("session is not open in paged mode".to_owned())
        })?;
        let Some(path) = session.session_file().map(std::path::Path::to_path_buf) else {
            return Err(PagedCompactionError::Failed(
                "paged session has no backing file".to_owned(),
            ));
        };
        let projection = session
            .build_session_projection()
            .map_err(|error| PagedCompactionError::Failed(error.to_string()))?;
        let branch = session.branch_path_meta(None);
        Ok(Self {
            paged,
            path,
            projection,
            branch,
        })
    }

    /// Whether the captured window crosses the compaction threshold — the
    /// exact pre-run decision [`compact_paged_inputs`] takes. A host that
    /// must announce the run before it starts (the print host's
    /// `compaction_start` frame) asks this instead of building a second
    /// projection.
    pub fn threshold_met(&self, model: &Model, config: &CompactionConfig) -> bool {
        threshold_tokens(&self.projection.messages, model, config).1
    }
}

/// Run the threshold compaction over a paged session (the caller persists the
/// resulting entry and rebuilds its context).
pub async fn compact_paged(
    session: &SessionManager,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
) -> Result<PagedCompaction, PagedCompactionError> {
    let inputs = PagedCompactionInputs::capture(session)?;
    compact_paged_inputs(inputs, model, config, complete, custom_instructions).await
}

/// [`compact_paged`] over a pre-captured resident view. The session lock may
/// be released after [`PagedCompactionInputs::capture`]: this half reads the
/// backing file (bounded, streaming) and calls the provider, and touches no
/// `SessionManager`.
pub async fn compact_paged_inputs(
    inputs: PagedCompactionInputs,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
) -> Result<PagedCompaction, PagedCompactionError> {
    let PagedCompactionInputs {
        paged,
        path,
        projection,
        branch,
    } = inputs;

    // Trigger. The usage record on the newest assistant message is the
    // provider's own count of the request that carried the whole transcript, so
    // the check is exact without scanning the prefix; a session with no usable
    // usage falls back to the window estimate (conservative: the caller may then
    // not compact on a huge prefix — see `PagedCompactionError::ThresholdNotMet`).
    // The estimator+gate pair is shared with [`threshold_met`], so a caller
    // that must know the decision BEFORE the run (the print host's lifecycle
    // events) applies exactly this rule.
    let (tokens_before, met) = threshold_tokens(&projection.messages, model, config);
    if !met {
        return Err(PagedCompactionError::ThresholdNotMet);
    }

    // The window-side plan: the cut point, the kept tail, and — when the
    // window itself carries a compaction — the previous summary. All resident.
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    let refs = ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
    let preparation: BorrowedCompactionPreparation<'_> =
        prepare_compaction_refs(&refs, &branch, config)
            .ok_or_else(|| PagedCompactionError::Failed("nothing to compact".to_owned()))?;
    let window_has_compaction = projection
        .entries
        .iter()
        .any(|entry| matches!(entry.source_entry.as_ref(), FileEntry::Compaction(_)));

    // Marker scan over the prefix (bounded memory, no parse): where does the
    // newest prefix compaction start (if any), and is there a context edit that
    // forces the widening fallback?
    let scan = scan_prefix(&path, paged.page_start)?;
    if scan.has_context_edit {
        return Err(PagedCompactionError::Unsupported(format!(
            "session {} holds a context_edit entry before the resident window",
            path.display()
        )));
    }

    let mut stats = PagedCompactionStats {
        resumed_from_prefix_compaction: scan.last_compaction.is_some() && !window_has_compaction,
        ..PagedCompactionStats::default()
    };
    let mut file_ops = FileOperations::default();

    // The running summary: the window's own previous summary wins (it is the
    // newest compaction on the leaf path); otherwise the prefix's newest
    // compaction supplies it and the span starts at that checkpoint's CUT
    // point (see `prefix_span_start`: the retained tail between the cut and
    // the checkpoint still belongs to the context).
    let (span_start, previous_summary) = if window_has_compaction {
        (None, preparation.previous_summary.clone())
    } else {
        match &scan.last_compaction {
            Some(compaction) => (
                Some(prefix_span_start(&path, compaction)?),
                Some(compaction.summary.clone()),
            ),
            None => (None, preparation.previous_summary.clone()),
        }
    };

    let framing = SpanFraming::History;
    let mut chunker = StreamingChunker::new(
        model,
        config.reserve_tokens,
        framing,
        previous_summary.as_deref(),
        Arc::clone(&complete),
    )
    .with_custom_instructions(custom_instructions);
    stats.chunk_cap_chars = chunker.budget_chars();

    // The streamed span: the prefix (from the newest prefix compaction's cut
    // point, or the file start, to the resident window) followed by the
    // window's own pre-cut messages.
    let mut pushed = 0usize;
    if let Some(span_start) = span_start {
        let limit = paged.page_start;
        pushed += stream_span_into_chunker(
            &path,
            span_start,
            limit,
            &mut chunker,
            &mut file_ops,
            &mut stats,
        )
        .await?;
    } else if !window_has_compaction && paged.page_start > 0 {
        pushed += stream_span_into_chunker(
            &path,
            0,
            paged.page_start,
            &mut chunker,
            &mut file_ops,
            &mut stats,
        )
        .await?;
    }
    for message in preparation.messages_to_summarize.iter() {
        chunker
            .push(message)
            .await
            .map_err(PagedCompactionError::Failed)?;
        pushed += 1;
    }
    if pushed == 0 {
        // The same honest degradation as the resident path: a compaction that
        // found nothing to summarize FAILS instead of persisting an empty
        // checkpoint.
        return Err(PagedCompactionError::Failed(
            "compaction found no messages to summarize".to_owned(),
        ));
    }
    let summarized = chunker
        .finish()
        .await
        .map_err(PagedCompactionError::Failed)?;

    let mut summary = summarized.text;
    let mut usage = summarized.usage;
    // The split-turn prefix lives in the resident window: summarize it with the
    // resident (bounded) path and merge, exactly like the resident compaction.
    if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let (turn_prefix_text, turn_prefix_usage) = super::run_turn_prefix_summary(
            preparation.turn_prefix_messages.as_slice(),
            custom_instructions,
            model,
            config,
            complete,
        )
        .await
        .map_err(PagedCompactionError::Failed)?;
        summary =
            format!("{summary}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_text}");
        usage = super::combine_usage(usage, turn_prefix_usage);
    }

    // File-operation tracking: the prefix's tool calls (collected while
    // streaming) plus the window's own span (already tracked by the plan).
    extract_file_ops_from_message_refs(preparation.messages_to_summarize.as_slice(), &mut file_ops);
    let (read_files, modified_files) = compute_file_lists(&file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(PagedCompaction {
        result: CompactionResult {
            summary,
            tokens_before,
            usage,
            first_kept_index: preparation.first_kept_message_index,
            first_kept_entry_id: preparation.first_kept_entry_id.clone(),
            retained_tail: preparation
                .kept_messages
                .iter()
                .map(|message| (*message).clone())
                .collect(),
            read_files,
            modified_files,
        },
        stats,
    })
}

/// A prefix compaction entry found by the marker scan.
struct PrefixCompaction {
    id: String,
    line_start: u64,
    summary: String,
    /// The checkpoint's cut point: the first entry its summary does NOT
    /// cover. It normally precedes the checkpoint on the leaf path.
    first_kept_entry_id: String,
}

/// The fields of one parsed prefix compaction line the paged span needs.
struct ParsedCompactionLine {
    id: String,
    summary: String,
    first_kept_entry_id: String,
}

/// What the prefix marker scan found.
struct PrefixScan {
    last_compaction: Option<PrefixCompaction>,
    has_context_edit: bool,
}

/// The byte patterns the scan looks for (the on-disk v3 JSONL shape).
const COMPACTION_MARKER: &[u8] = b"\"type\":\"compaction\"";
const CONTEXT_EDIT_MARKER: &[u8] = b"\"type\":\"context_edit\"";

/// Scan `[0, limit)` for the newest compaction entry and for any context edit,
/// with a bounded buffer (no parse, no retention). The compaction entry itself
/// is then read back (one line) to parse its summary.
fn scan_prefix(path: &std::path::Path, limit: u64) -> Result<PrefixScan, PagedCompactionError> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        PagedCompactionError::Failed(format!("cannot read session {}: {error}", path.display()))
    })?;
    let mut buffer = vec![0u8; SESSION_READ_BUFFER_SIZE];
    let overlap = COMPACTION_MARKER.len().max(CONTEXT_EDIT_MARKER.len());
    // ONE reusable scan window for the whole prefix: a fresh ~1 MiB
    // concatenation per read block (the 100 MB prefix is 100 blocks) was pure
    // allocation churn on the very path that exists to keep memory flat.
    let mut window: Vec<u8> = Vec::with_capacity(SESSION_READ_BUFFER_SIZE + overlap);
    let mut consumed = 0u64;
    let mut compaction_offsets: Vec<u64> = Vec::new();
    let mut has_context_edit = false;
    // Bytes carried from the previous block so a marker split across the seam
    // is still found.
    let mut carried = 0usize;
    while consumed < limit {
        let read = file.read(&mut buffer).map_err(|error| {
            PagedCompactionError::Failed(format!("cannot read session {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        let limit_now = ((limit - consumed) as usize).min(read);
        consumed += limit_now as u64;
        window.truncate(carried);
        window.extend_from_slice(&buffer[..limit_now]);
        let base = consumed - window.len() as u64;
        if !find_all(&window, CONTEXT_EDIT_MARKER).is_empty() {
            has_context_edit = true;
        }
        compaction_offsets.extend(
            find_all(&window, COMPACTION_MARKER)
                .into_iter()
                .map(|index| base + index as u64),
        );
        carried = overlap.saturating_sub(1).min(window.len());
        let keep_from = window.len() - carried;
        window.copy_within(keep_from.., 0);
    }

    compaction_offsets.sort_unstable();
    compaction_offsets.dedup();
    // A marker inside a nested JSON value is not a compaction entry. Walk
    // candidates newest-first and skip lines whose top-level type is not
    // `compaction`; only a genuinely malformed compaction line fails the scan.
    let mut last_compaction = None;
    let mut scan_error = None;
    for offset in compaction_offsets.into_iter().rev() {
        let line_start = line_start_before(&mut file, offset)?;
        match read_compaction_line(&mut file, line_start, offset) {
            Ok(Some(parsed)) => {
                last_compaction = Some(PrefixCompaction {
                    id: parsed.id,
                    line_start,
                    summary: parsed.summary,
                    first_kept_entry_id: parsed.first_kept_entry_id,
                });
                break;
            }
            Ok(None) => continue,
            Err(error) => {
                scan_error = Some(error);
                break;
            }
        }
    }
    if last_compaction.is_none() {
        if let Some(error) = scan_error {
            return Err(error);
        }
    }
    Ok(PrefixScan {
        last_compaction,
        has_context_edit,
    })
}

/// The last occurrence of `needle` in `haystack`.
///
/// Tuned for the 1 MiB scan blocks: a byte scan for the marker's first byte
/// (cheap, early-exit) narrows the candidates, and only those pay a slice
/// comparison. The naive "compare at every index" form cost ~22 byte
/// comparisons per input byte, seconds on a 100 MB prefix.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    let first = needle[0];
    let mut found = Vec::new();
    let mut search_from = 0usize;
    while search_from + needle.len() <= haystack.len() {
        let Some(relative) = haystack[search_from..]
            .iter()
            .position(|byte| *byte == first)
        else {
            break;
        };
        let index = search_from + relative;
        if index + needle.len() > haystack.len() {
            break;
        }
        if &haystack[index..index + needle.len()] == needle {
            found.push(index);
        }
        search_from = index + 1;
    }
    found
}

/// The byte offset of the start of the line containing `offset`.
fn line_start_before(file: &mut std::fs::File, offset: u64) -> Result<u64, PagedCompactionError> {
    let mut position = offset;
    let mut buffer = vec![0u8; 64 * 1024];
    while position > 0 {
        let read_start = position.saturating_sub(buffer.len() as u64);
        let length = (position - read_start) as usize;
        file.seek(SeekFrom::Start(read_start))
            .map_err(|error| PagedCompactionError::Failed(error.to_string()))?;
        file.read_exact(&mut buffer[..length])
            .map_err(|error| PagedCompactionError::Failed(error.to_string()))?;
        for index in (0..length).rev() {
            if buffer[index] == b'\n' {
                return Ok(read_start + index as u64 + 1);
            }
        }
        position = read_start;
    }
    Ok(0)
}

/// Read one line starting at `line_start` and parse its compaction entry.
///
/// The line's length is NOT known in advance: a real compaction summary is
/// routinely tens of KiB (measured 6.5-34 KiB in `~/.pi` sessions), so a
/// fixed `marker + 4 KiB` window cut the JSON before its newline and the
/// parse failed for EVERY realistic checkpoint. The read therefore GROWS
/// (doubling) until it finds the line's newline, bounded by a hard cap: a
/// longer line is corrupt/foreign and is reported instead of allocated.
fn read_compaction_line(
    file: &mut std::fs::File,
    line_start: u64,
    marker_offset: u64,
) -> Result<Option<ParsedCompactionLine>, PagedCompactionError> {
    /// The first read covers every measured compaction line (the largest was
    /// ~34 KiB); the buffer only grows when the line continues past it.
    const INITIAL_READ_BYTES: u64 = 64 * 1024;
    /// Hard cap for one compaction line. This is a checkpoint summary, not a
    /// bulk transfer: a longer line cannot be a realistic compaction entry.
    const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;

    // Always include the marker (it sits inside the line), whatever the
    // initial window; `clamp` keeps the doubled window within [initial, cap].
    let mut length = marker_offset
        .saturating_sub(line_start)
        .saturating_add(1)
        .clamp(INITIAL_READ_BYTES, MAX_LINE_BYTES);
    loop {
        let mut buffer = vec![0u8; length as usize];
        file.seek(SeekFrom::Start(line_start))
            .map_err(|error| PagedCompactionError::Failed(error.to_string()))?;
        let read = file
            .read(&mut buffer)
            .map_err(|error| PagedCompactionError::Failed(error.to_string()))?;
        buffer.truncate(read);
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        match newline {
            // The line is complete.
            Some(end) => return parse_compaction_line(&buffer[..end]),
            // EOF before a newline: the file's final line has no trailing
            // newline (the full loader repairs that on open); parse it whole.
            None if read < length as usize => return parse_compaction_line(&buffer),
            None => {}
        }
        if length == MAX_LINE_BYTES {
            return Err(PagedCompactionError::Failed(format!(
                "the newest prefix compaction entry exceeds the {MAX_LINE_BYTES}-byte line cap"
            )));
        }
        length = (length * 2).min(MAX_LINE_BYTES);
    }
}

/// Parse one complete compaction line.
fn parse_compaction_line(
    line: &[u8],
) -> Result<Option<ParsedCompactionLine>, PagedCompactionError> {
    let text = String::from_utf8_lossy(line);
    match parse_entry_line_with_version(&text, None) {
        Some(FileEntry::Compaction(compaction)) => Ok(Some(ParsedCompactionLine {
            id: compaction.id,
            summary: compaction.summary,
            first_kept_entry_id: compaction.first_kept_entry_id,
        })),
        // The marker was inside a valid non-compaction entry (for example a
        // tool-call argument); it is not a checkpoint candidate.
        Some(_) => Ok(None),
        None => Err(PagedCompactionError::Failed(
            "the newest prefix compaction entry could not be parsed".to_owned(),
        )),
    }
}

/// The byte offset the streamed span must start at for one prefix compaction.
///
/// A self-retaining checkpoint (`firstKeptEntryId` is the compaction's own id,
/// or empty in a synthetic entry) retains nothing before itself, so the span
/// starts at the checkpoint line. Otherwise the cut point precedes the
/// checkpoint on the leaf path: the checkpoint summary only covers what came
/// before `firstKeptEntryId`, while the entries between that cut and the
/// checkpoint are still part of the model context. The span must resume at the
/// CUT, not at the checkpoint line — starting at the checkpoint silently
/// dropped the retained tail from the next summary and the rebuilt context.
///
/// A cut point that cannot be located before the checkpoint keeps the
/// checkpoint line, which is exactly what the resident projection does when
/// `firstKeptEntryId` is not on the retained path (`found_first_kept` stays
/// false, so nothing before the checkpoint contributes).
fn prefix_span_start(
    path: &std::path::Path,
    compaction: &PrefixCompaction,
) -> Result<u64, PagedCompactionError> {
    if compaction.first_kept_entry_id.is_empty() || compaction.first_kept_entry_id == compaction.id
    {
        return Ok(compaction.line_start);
    }
    Ok(
        find_entry_line(path, compaction.line_start, &compaction.first_kept_entry_id)?
            .unwrap_or(compaction.line_start),
    )
}

/// Locate the line carrying the entry `entry_id` within `[0, limit)`.
///
/// Bounded like every other prefix stream (a read buffer plus one line); only
/// lines whose bytes contain the entry-id marker are PARSED, and the marker
/// hit is then verified against the entry's own top-level id, so a tool
/// result's nested `id` field can never select the wrong line.
fn find_entry_line(
    path: &std::path::Path,
    limit: u64,
    entry_id: &str,
) -> Result<Option<u64>, PagedCompactionError> {
    if limit == 0 {
        return Ok(None);
    }
    let needle = serde_json::to_string(entry_id).unwrap_or_default();
    let needle = needle.as_bytes();
    let mut file = std::fs::File::open(path).map_err(|error| {
        PagedCompactionError::Failed(format!("cannot read session {}: {error}", path.display()))
    })?;
    let mut buffer = vec![0u8; SESSION_READ_BUFFER_SIZE];
    let mut pending: Vec<u8> = Vec::new();
    // Absolute byte offset of `pending[0]`.
    let mut pending_start: u64 = 0;
    let mut consumed = 0u64;
    while consumed < limit {
        let read = file.read(&mut buffer).map_err(|error| {
            PagedCompactionError::Failed(format!("cannot read session {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        let limit_now = ((limit - consumed) as usize).min(read);
        pending.extend_from_slice(&buffer[..limit_now]);
        consumed += limit_now as u64;
        let mut offset = 0usize;
        while let Some(relative) = pending[offset..].iter().position(|byte| *byte == b'\n') {
            let end = offset + relative;
            let line = &pending[offset..end];
            if line.windows(needle.len()).any(|window| window == needle) {
                // Compare the TOP-LEVEL `id` only: the cut point may itself be
                // a custom-message role a typed parse would reject on an older
                // session file, while the line's identity is all this scan
                // needs.
                let text = String::from_utf8_lossy(line);
                let matches = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(|id| id == entry_id)
                    })
                    .unwrap_or(false);
                if matches {
                    return Ok(Some(pending_start + offset as u64));
                }
            }
            offset = end + 1;
        }
        pending.drain(..offset);
        pending_start += offset as u64;
        if pending.len() > MAX_PREFIX_ENTRY_LINE_BYTES {
            return Err(PagedCompactionError::Failed(format!(
                "session {} holds an over-long entry line (>{MAX_PREFIX_ENTRY_LINE_BYTES} bytes) before the resident window",
                path.display()
            )));
        }
    }
    Ok(None)
}

/// Stream `[start, limit)` through the chunker, one entry at a time (the prefix
/// never becomes resident: the peak is the read buffer plus one entry).
/// Stream one span region; returns the number of messages pushed into the
/// chunker.
async fn stream_span_into_chunker(
    path: &std::path::Path,
    start: u64,
    limit: u64,
    chunker: &mut StreamingChunker<'_>,
    file_ops: &mut FileOperations,
    stats: &mut PagedCompactionStats,
) -> Result<usize, PagedCompactionError> {
    let mut pushed = 0usize;
    let mut file = std::fs::File::open(path).map_err(|error| {
        PagedCompactionError::Failed(format!("cannot read session {}: {error}", path.display()))
    })?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| PagedCompactionError::Failed(error.to_string()))?;
    let mut buffer = vec![0u8; SESSION_READ_BUFFER_SIZE];
    let mut pending: Vec<u8> = Vec::new();
    let mut header_version: Option<u32> = None;
    let mut previous_id: Option<String> = None;
    let mut consumed = start;
    while consumed < limit {
        let read = file.read(&mut buffer).map_err(|error| {
            PagedCompactionError::Failed(format!("cannot read session {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        let limit_now = ((limit - consumed) as usize).min(read);
        pending.extend_from_slice(&buffer[..limit_now]);
        consumed += limit_now as u64;
        stats.prefix_bytes += limit_now as u64;
        let mut line_start = 0usize;
        while let Some(relative) = pending[line_start..].iter().position(|byte| *byte == b'\n') {
            let end = line_start + relative;
            let text = String::from_utf8_lossy(&pending[line_start..end]);
            line_start = end + 1;
            if text.trim().is_empty() {
                continue;
            }
            let Some(entry) = parse_entry_line_with_version(&text, header_version) else {
                continue;
            };
            if header_version.is_none() {
                if let Some(header) = entry.as_header() {
                    header_version = Some(header.version.unwrap_or(1));
                }
            }
            // The prefix stream is only valid while it follows one linear
            // leaf path. A branch left by `/tree` or a fork would otherwise be
            // summarized as if it were history; widen instead.
            if let (Some(previous), Some(parent)) = (&previous_id, entry.parent_id()) {
                if parent != previous {
                    return Err(PagedCompactionError::Unsupported(format!(
                        "session {} has a branch in the skipped prefix",
                        path.display()
                    )));
                }
            }
            previous_id = Some(entry.entry_id().to_owned());
            match &entry {
                FileEntry::Compaction(_) => {
                    // The span started after the newest prefix compaction;
                    // older compactions inside the span are already represented
                    // by the running summary (their details are cumulative).
                    continue;
                }
                FileEntry::ContextEdit(_) => {
                    return Err(PagedCompactionError::Unsupported(format!(
                        "session {} holds a context_edit entry before the resident window",
                        path.display()
                    )));
                }
                FileEntry::Session(_) => continue,
                _ => {}
            }
            stats.prefix_entries += 1;
            for message in session_entry_to_context_messages(&entry) {
                if matches!(message, Message::System(_)) {
                    continue;
                }
                {
                    let refs = [&message];
                    extract_file_ops_from_message_refs(refs.as_slice(), file_ops);
                }
                // The message was parsed for this entry only: move it into the
                // chunk window instead of cloning tool-result file contents.
                chunker
                    .push_owned(message)
                    .await
                    .map_err(PagedCompactionError::Failed)?;
                pushed += 1;
            }
        }
        pending.drain(..line_start);
        if pending.len() > MAX_PREFIX_ENTRY_LINE_BYTES {
            return Err(PagedCompactionError::Failed(format!(
                "session {} holds an over-long entry line (>{MAX_PREFIX_ENTRY_LINE_BYTES} bytes)",
                path.display()
            )));
        }
    }
    stats.summarizer_calls = chunker.chunks_emitted();
    Ok(pushed)
}

/// Whether a paged session must widen before it can serve a complete model
/// context: `true` whenever the resident window is NOT self-contained (its
/// leaf-path projection does not start at a resident checkpoint boundary), and
/// `true` when the threshold does not fire either (see the module docs).
///
/// A window is self-contained only when its LEAF-PATH projection starts at a
/// compaction checkpoint whose retained range it also holds: the projection
/// then drops everything before that checkpoint, so the skipped prefix is
/// fully represented by the summary. The previous check ("any resident entry
/// is a compaction") was wrong twice over:
///
/// - a compaction on an ABANDONED branch made it claim completeness while the
///   leaf path had no checkpoint at all, so the model context was silently a
///   tail window;
/// - a checkpoint whose `firstKeptEntryId` fell into the skipped prefix still
///   projects a truncated kept range.
pub fn paged_requires_widening(
    session: &SessionManager,
    model: &Model,
    config: &CompactionConfig,
) -> bool {
    if session.paged_window().is_none() {
        return false;
    }
    let Ok(projection) = session.build_session_projection() else {
        return true;
    };
    // `build_session_projection` puts the newest on-path compaction FIRST; an
    // older compaction retained inside the kept range contributes nothing.
    let self_contained = match projection
        .entries
        .first()
        .map(|entry| entry.source_entry.as_ref())
    {
        Some(FileEntry::Compaction(compaction)) => {
            // The retained range must be resident too (a self-retaining
            // checkpoint trivially is; its own id is on the path).
            session.entry(&compaction.first_kept_entry_id).is_some()
        }
        _ => false,
    };
    if self_contained {
        // The window already holds the complete model context (the prefix is
        // represented by the checkpoint summary), so widening would only
        // re-read an already-summarized prefix.
        return false;
    }
    !threshold_tokens(&projection.messages, model, config).1
}

/// The paged trigger's estimator+gate pair over one already-built window
/// projection: the exact rule [`compact_paged`] decides with, shared with
/// [`threshold_met`] and [`paged_requires_widening`] so the trigger never
/// exists in two versions. Returns `(estimated tokens before, trigger met)`.
fn threshold_tokens(messages: &[Message], model: &Model, config: &CompactionConfig) -> (u64, bool) {
    let tokens = estimate_context_tokens(messages).tokens;
    (
        tokens,
        should_compact_tokens(tokens, model.context_window, config),
    )
}

/// Whether [`compact_paged`] would run for this paged session: its first
/// check, before the prefix scan or any summarizer call. The estimated token
/// count comes from the resident window's projection (the newest assistant
/// usage plus the trailing window messages), so the decision is exact without
/// reading the skipped prefix.
///
/// Hosts that must announce the run BEFORE it starts — the print host's
/// `compaction_start` frame — ask this instead of duplicating the rule.
/// `Ok(false)` is exactly the [`PagedCompactionError::ThresholdNotMet`] no-op
/// (do not emit around it); `Err` is the projection failure
/// [`compact_paged`] reports as [`PagedCompactionError::Failed`].
///
/// Cost: one resident-window projection plus the estimate. The window is
/// bounded by the paged open caps (`DEFAULT_TAIL_PAGE_BYTES` = 1 MiB by
/// default), and a caller that skips the call on `Ok(false)` pays exactly the
/// projection `compact_paged` would have built for its own check; a caller
/// that proceeds pays one extra window projection (never the skipped prefix).
pub fn threshold_met(
    session: &SessionManager,
    model: &Model,
    config: &CompactionConfig,
) -> crate::Result<bool> {
    let projection = session.build_session_projection()?;
    Ok(threshold_tokens(&projection.messages, model, config).1)
}

/// The chunk budget a paged compaction will use (diagnostics/hosts).
pub fn paged_chunk_budget(model: &Model, reserve_tokens: u64) -> super::SummarizationChunkBudget {
    super::summarization_chunk_budget(model, reserve_tokens)
}

/// The resident window's kept messages (the tail a paged compaction retains).
pub fn resident_kept_messages(session: &SessionManager) -> Vec<Message> {
    session.build_context_entries().unwrap_or_default()
}
