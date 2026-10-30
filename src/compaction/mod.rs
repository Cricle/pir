//! Context compaction for long sessions (upstream
//! `packages/coding-agent/src/core/compaction/compaction.ts` + `utils.ts`).
//!
//! The algorithmic pieces (estimation, trigger predicate, projected cut
//! point) live in [`algorithm`]; the prompts in [`prompts`]. This module
//! ports upstream's reworked algorithm:
//!
//! - [`prepare_compaction`] mirrors upstream `prepareCompaction`: it computes
//!   the cut point over PROJECTED entries, keeps only the newest compaction's
//!   summary for iterative update, and splits turns (cut at an assistant
//!   message) into a history span plus a turn prefix.
//! - [`compact_with_preparation`] mirrors upstream `compact`: ONE history
//!   summarizer call, plus — for split turns — a second, smaller turn-prefix
//!   summarizer call using the "checkpoint" framing prompt, merged with the
//!   `**Turn Context (split turn):**` separator.
//! - [`recover_from_overflow`] keeps pir's context-overflow recovery (drop
//!   the failed trailing assistant message and compact once).
//!
//! Deviations from upstream, by design:
//!
//! - `cacheRetention: "none"` and fresh routing session ids are provider
//!   options owned by the caller's [`CompleteFn`] implementation.
//! - Upstream `custom_message`/`bashExecution`/`branchSummary`/
//!   `compactionSummary` agent-message roles are rendered as their framed
//!   user-text equivalents before summarization (pir's `Message` union has no
//!   such roles).
//! - The file-op extraction reads codemode `nestedCalls` defensively from the
//!   tool result's `details` object (upstream reads a typed `nestedCalls`
//!   field on the message).
//! - **Bounded (chunked) summarization** ([`chunking`]): upstream always
//!   sends ONE summarizer request over the whole span. A span that does not
//!   fit the model window is split into turn-aligned chunks and summarized
//!   sequentially, each chunk folding into the running summary through the
//!   existing "update" prompt. A span that fits keeps the single upstream
//!   call byte for byte; the chunked shape is a pir extension (the session
//!   format and the persisted summary shape are unchanged).
//!
//! Token estimation (`CHARS_PER_TOKEN`): the fix-list brief cited an upstream
//! `estimate.ts` with a 3.5 chars/token constant, but the checked-in upstream
//! (`core/compaction/compaction.ts:298-349`, `estimateTokens`) divides by 4 —
//! pir's 4.0 constant already matches upstream, so it is kept unchanged.

pub mod algorithm;
pub mod branch_summarization;
pub mod chunking;
pub mod paged;
pub mod prompts;

use std::sync::Arc;

use serde_json::Value as JsonValue;
use tracing::Instrument;

use crate::agent::agent_loop::{BoxFuture, CompactionFn};
use crate::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, Usage,
};
use crate::ai::util::now_ms;
use crate::session_manager::projection::{user_text_message, user_text_message_owned};
pub use algorithm::{
    calculate_context_tokens, estimate_context_tokens, estimate_message_tokens,
    estimate_projected_context_tokens, estimate_projected_context_tokens_with_tail,
    find_projected_cut_point, is_cut_point_message, is_turn_start_message,
    materialize_projected_tail, projection_materializations, reset_projection_materializations,
    should_compact, should_compact_tokens, CompactionConfig, ContextUsageEstimate, CutEntry,
    CutEntryRef, CutPointResult, MessageSequence, PathEntry, PathEntryMeta, ProjectedEntry,
    ProjectionEntryMeta, CHARS_PER_TOKEN,
};
pub use branch_summarization::{
    branch_summary_max_tokens, collect_entries_for_branch_summary, generate_branch_summary,
    generate_branch_summary_with_observer, prepare_branch_entries, BranchPreparation,
    BranchSummaryResult, CollectEntriesResult, DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS,
};
pub use chunking::{
    chunk_estimated_tokens, span_estimated_tokens, split_summarization_chunks,
    summarization_chunk_budget, StreamingChunker, SummarizationChunk, SummarizationChunkBudget,
    FALLBACK_CONTEXT_WINDOW, MIN_CHUNK_BUDGET_TOKENS,
};
pub use paged::{
    compact_paged, paged_chunk_budget, paged_requires_widening, resident_kept_messages,
    PagedCompaction, PagedCompactionError, PagedCompactionStats,
};

/// Injectable non-streaming completion callback.  The caller supplies the
/// mechanism (typically draining a [`crate::ai::provider::ProviderRegistry`]
/// stream); tests inject fakes.  Returning `Err` fails the compaction.
///
/// The implementation owns the provider call options that pir's [`Context`]
/// cannot carry (`cacheRetention: "none"`, upstream `completeSummarization`).
/// Summarization requests deliberately set NO `maxTokens`: the provider sizes
/// output from the actual request using the model's own output limit reduced to
/// fit the remaining context, exactly as for normal turns (upstream #11036).
/// `reserveTokens` only controls when compaction starts.
pub type CompleteFn =
    Box<dyn Fn(Context) -> BoxFuture<'static, Result<AssistantMessage, String>> + Send + Sync>;

/// One summarizer retry transition (upstream `RetryCallbacks`,
/// ai/src/utils/retry.ts:148-161; the session maps these onto
/// `summarization_retry_*` events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizationRetryEvent {
    /// A retry was scheduled, before its backoff (upstream
    /// `onRetryScheduled`).
    Scheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// The backoff elapsed and the call is being re-issued (upstream
    /// `onRetryAttemptStart`).
    AttemptStart,
    /// The retry loop closed (upstream `onRetryFinished`).
    Finished,
}

/// Observer for [`SummarizationRetryEvent`] values (the session installs one
/// around a compaction/branch-summary run; see [`with_summarization_retry`]).
pub type SummarizationRetryObserver = Arc<dyn Fn(SummarizationRetryEvent) + Send + Sync>;

tokio::task_local! {
    /// The observer active for the summarization call(s) of ONE compaction or
    /// branch-summary run. Task-local so concurrent sessions never see each
    /// other's retries and no compaction signature carries session state.
    static SUMMARIZATION_RETRY_OBSERVER: SummarizationRetryObserver;
}

/// Scope `future` with `observer` as the summarizer retry observer. The
/// session wraps each compaction/branch-summary run, so every nested
/// summarizer call (history + split-turn prefix + chunk folds) reports its
/// retries to that run's session-event channel.
pub async fn with_summarization_retry<F, T>(observer: SummarizationRetryObserver, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    SUMMARIZATION_RETRY_OBSERVER.scope(observer, future).await
}

/// Result of a successful compaction (upstream `CompactionResult`, plus the
/// fields pir's callers need to rebuild the live context).
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    /// The generated summary text (including the formatted file-operation
    /// sections and the split-turn context block, like upstream).
    pub summary: String,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Usage from the summarizer call(s), when the response carried one.
    pub usage: Usage,
    /// Index into the summarized message list where the kept messages start
    /// (surfaces as the RPC `firstKeptIndex`).
    pub first_kept_index: usize,
    /// Session entry id of the first kept entry (upstream
    /// `firstKeptEntryId`). `None` when the cut lands on a not-yet-persisted
    /// message (the compaction entry then stores its own id: self-retaining)
    /// or when there is no session storage.
    pub first_kept_entry_id: Option<String>,
    /// Messages kept verbatim after the cut point (upstream reloads them from
    /// the session; pir returns them so the caller can rebuild the context).
    pub retained_tail: Vec<Message>,
    /// Files only read (never modified) by the summarized span (upstream
    /// `details.readFiles`).
    pub read_files: Vec<String>,
    /// Files read AND written/edited by the summarized span (upstream
    /// `details.modifiedFiles`).
    pub modified_files: Vec<String>,
}

// ============================================================================
// File operation tracking (upstream `compaction/utils.ts`)
// ============================================================================

/// File paths extracted from `read`/`write`/`edit` tool calls in the
/// summarized span (upstream `FileOperations` sets).
#[derive(Debug, Default, Clone)]
pub struct FileOperations {
    read: std::collections::BTreeSet<String>,
    written: std::collections::BTreeSet<String>,
    edited: std::collections::BTreeSet<String>,
}

/// Extract file operations from tool calls in assistant messages, and from
/// the nested calls recorded on a tool result (upstream
/// `extractFileOpsFromMessage`). Codemode calls arrive in the tool result's
/// `details.nestedCalls.calls` array; read defensively.
pub fn extract_file_ops_from_messages(messages: &[Message], ops: &mut FileOperations) {
    for message in messages {
        extract_file_ops_from_message(message, ops);
    }
}

fn add_file_op(tool_name: &str, args: Option<&JsonValue>, ops: &mut FileOperations) {
    let Some(path) = args
        .and_then(|args| args.get("path"))
        .and_then(JsonValue::as_str)
    else {
        return;
    };
    if path.is_empty() {
        return;
    }
    match tool_name {
        "read" => {
            ops.read.insert(path.to_owned());
        }
        "write" => {
            ops.written.insert(path.to_owned());
        }
        "edit" => {
            ops.edited.insert(path.to_owned());
        }
        _ => {}
    }
}

/// Carry the previous compaction's file tracking into the next one (upstream
/// `extractFileOperations`: only pi-generated details, i.e. `!fromHook`).
pub(crate) fn carry_previous_file_ops(details: Option<&JsonValue>, ops: &mut FileOperations) {
    let Some(details) = details else {
        return;
    };
    if let Some(read) = details.get("readFiles").and_then(JsonValue::as_array) {
        for path in read.iter().filter_map(JsonValue::as_str) {
            ops.read.insert(path.to_owned());
        }
    }
    if let Some(modified) = details.get("modifiedFiles").and_then(JsonValue::as_array) {
        for path in modified.iter().filter_map(JsonValue::as_str) {
            ops.edited.insert(path.to_owned());
        }
    }
}

/// Compute the final summary lists (upstream `computeFileLists`): modified =
/// edited ∪ written; read-only = read − modified. Both sorted.
pub fn compute_file_lists(ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: Vec<String> = ops
        .edited
        .union(&ops.written)
        .cloned()
        .collect::<std::collections::BTreeSet<String>>()
        .into_iter()
        .collect();
    let read_only: Vec<String> = ops
        .read
        .iter()
        .filter(|path| !ops.edited.contains(*path) && !ops.written.contains(*path))
        .cloned()
        .collect();
    (read_only, modified)
}

/// Format the file-operation sections appended to the summary (upstream
/// `formatFileOperations`): `<read-files>` / `<modified-files>` XML tags.
pub(crate) fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

// ============================================================================
// Conversation serialization
// ============================================================================

/// Text of the text blocks of a content list joined with `\n` (upstream
/// `contentText`).
fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Character length of [`content_text`] WITHOUT building the joined string —
/// the weight path only needs the count, never a copy of the message text.
fn content_text_chars(content: &[ContentBlock]) -> usize {
    let mut chars = 0usize;
    let mut blocks = 0usize;
    for block in content {
        if let ContentBlock::Text(text) = block {
            chars = chars.saturating_add(text.text.chars().count());
            blocks += 1;
        }
    }
    if blocks == 0 {
        0
    } else {
        chars + blocks - 1
    }
}

/// Write at most `max_chars` characters of [`content_text`] into `out` and
/// return the FULL joined length in characters. The part beyond `max_chars`
/// is counted but never materialized: a multi-megabyte tool result that the
/// serializer truncates to 2000 characters must not be copied whole first.
fn write_content_text_limited(
    out: &mut String,
    content: &[ContentBlock],
    max_chars: usize,
) -> usize {
    let mut total = 0usize;
    let mut written = 0usize;
    let mut first = true;
    for block in content {
        let ContentBlock::Text(text) = block else {
            continue;
        };
        if !first {
            total += 1;
            if written < max_chars {
                out.push('\n');
                written += 1;
            }
        }
        first = false;
        total = total.saturating_add(text.text.chars().count());
        if written < max_chars {
            let mut utf8 = [0u8; 4];
            for ch in text.text.chars() {
                if written >= max_chars {
                    break;
                }
                out.push_str(ch.encode_utf8(&mut utf8));
                written += 1;
            }
        }
    }
    total
}

/// Decimal digit count of `value` (for the truncation marker's length).
fn decimal_digits(value: usize) -> usize {
    let mut digits = 1usize;
    let mut remaining = value;
    while remaining >= 10 {
        remaining /= 10;
        digits += 1;
    }
    digits
}

/// Character length of [`prompts::truncate_for_summary`]'s result for a text
/// of `total_chars` characters, without building the truncated text.
fn truncated_summary_chars(total_chars: usize, max_chars: usize) -> usize {
    if total_chars <= max_chars {
        return total_chars;
    }
    // "\n\n[... " + decimal digits + " more characters truncated]"
    max_chars + 2 + 5 + decimal_digits(total_chars - max_chars) + 27
}

/// `(serialized characters, part count)` of ONE message under the exact
/// [`serialize_message_parts`] rule, without materializing any text. The
/// chunk splitter and the streaming chunker weigh every message with this
/// (their peak must be one message, not the span), so it must stay
/// byte-for-byte equivalent to the serializer — a unit test pins that.
pub(crate) fn message_parts_chars(message: &Message) -> (usize, usize) {
    match message {
        Message::User(user) => {
            let content = content_text_chars(&user.content);
            if content == 0 {
                (0, 0)
            } else {
                // "[User]: "
                (8 + content, 1)
            }
        }
        Message::Assistant(assistant) => {
            let mut text_chars = 0usize;
            let mut text_blocks = 0usize;
            let mut tool_calls_chars = 0usize;
            let mut tool_calls = 0usize;
            for block in &assistant.content {
                match block {
                    ContentBlock::Text(text) => {
                        text_chars = text_chars.saturating_add(text.text.chars().count());
                        text_blocks += 1;
                    }
                    ContentBlock::ToolCall(call) => {
                        let mut args_chars = 0usize;
                        let mut args = 0usize;
                        for (key, value) in &call.arguments {
                            let json = serde_json::to_string(value).unwrap_or_default();
                            args_chars = args_chars
                                .saturating_add(key.chars().count() + 1 + json.chars().count());
                            args += 1;
                        }
                        if args > 0 {
                            args_chars = args_chars.saturating_add(2 * (args - 1));
                        }
                        // "name(" + args + ")"
                        tool_calls_chars = tool_calls_chars
                            .saturating_add(call.name.chars().count() + 2 + args_chars);
                        tool_calls += 1;
                    }
                    _ => {}
                }
            }
            let mut chars = 0usize;
            let mut parts = 0usize;
            if text_blocks > 0 {
                // "[Assistant]: " + the joined text blocks (empty blocks still
                // contribute their "\n" separators).
                chars = chars.saturating_add(13 + text_chars + text_blocks - 1);
                parts += 1;
            }
            if tool_calls > 0 {
                chars = chars.saturating_add(
                    // "[Assistant tool calls]: " + calls joined with "; "
                    24 + tool_calls_chars + 2 * (tool_calls - 1),
                );
                parts += 1;
            }
            (chars, parts)
        }
        Message::ToolResult(result) => {
            let total = content_text_chars(&result.content);
            if total == 0 {
                (0, 0)
            } else {
                // "[Tool result]: " + the truncated text
                (
                    15 + truncated_summary_chars(total, prompts::TOOL_RESULT_MAX_CHARS),
                    1,
                )
            }
        }
        Message::System(_) | Message::Developer(_) => (0, 0),
    }
}

/// Serialize ONE message into the part(s) [`serialize_conversation`] would
/// contribute for it (empty when the message contributes nothing: system and
/// developer messages, empty content, ...). A message that carries both
/// visible text and tool calls contributes TWO parts, exactly like upstream's
/// two independent `parts.push` calls. This is the single per-message
/// formatting rule the whole module shares — the conversation serializer, the
/// chunk splitter's exact weights, and the capped chunk serializer all call
/// it, so they can never drift apart.
pub(crate) fn serialize_message_parts(message: &Message) -> Vec<String> {
    match message {
        Message::User(user) => {
            let content = content_text(&user.content);
            if content.is_empty() {
                Vec::new()
            } else {
                vec![format!("[User]: {content}")]
            }
        }
        Message::Assistant(assistant) => {
            use std::fmt::Write as _;

            let has_text = assistant
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(_)));
            // One `content_text` for the whole message (the old shape scanned
            // the blocks once for `any` and again inside `content_text`).
            let text = if has_text {
                content_text(&assistant.content)
            } else {
                String::new()
            };
            let mut tool_calls: Vec<String> = Vec::new();
            for block in &assistant.content {
                let ContentBlock::ToolCall(call) = block else {
                    continue;
                };
                let mut args = String::new();
                for (index, (key, value)) in call.arguments.iter().enumerate() {
                    if index > 0 {
                        args.push_str(", ");
                    }
                    let _ = write!(args, "{key}=");
                    args.push_str(&serde_json::to_string(value).unwrap_or_default());
                }
                tool_calls.push(format!("{}({args})", call.name));
            }

            let mut parts: Vec<String> = Vec::new();
            if has_text {
                parts.push(format!("[Assistant]: {text}"));
            }
            if !tool_calls.is_empty() {
                parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
            }
            parts
        }
        Message::ToolResult(result) => {
            // Build the (possibly truncated) text directly: the old shape
            // materialized the WHOLE tool result through `content_text` just
            // to keep its first 2000 characters.
            let mut truncated = String::new();
            let total = write_content_text_limited(
                &mut truncated,
                &result.content,
                prompts::TOOL_RESULT_MAX_CHARS,
            );
            if total == 0 {
                Vec::new()
            } else {
                if total > prompts::TOOL_RESULT_MAX_CHARS {
                    use std::fmt::Write as _;
                    let _ = write!(
                        truncated,
                        "\n\n[... {} more characters truncated]",
                        total - prompts::TOOL_RESULT_MAX_CHARS
                    );
                }
                vec![format!("[Tool result]: {truncated}")]
            }
        }
        Message::System(_) | Message::Developer(_) => Vec::new(),
    }
}

/// Serialize messages to text for summarization so the model doesn't try to
/// continue the conversation (upstream `serializeConversation`).  Tool results
/// are truncated to keep the summarization request within reasonable token
/// budgets.
///
/// Thinking is omitted (upstream #10842's companion change, `15e7bdc14`):
/// providers return summaries of the real reasoning, and replayed thinking is
/// often not part of the model's live context (a block that does not match its
/// request is dropped), so including it could make the summarization request
/// larger than the context it summarizes. Visible text, tool calls, and tool
/// results record what happened.
pub fn serialize_conversation(messages: &[Message]) -> String {
    serialize_conversation_over(messages)
}

/// [`serialize_conversation`] over any borrowed [`MessageSequence`] (a plain
/// message slice, a slice of message references, or a projection's virtual
/// concatenation) — the single serializer, so a span that lives as references
/// is serialized exactly like an owned one.
pub fn serialize_conversation_over<M: MessageSequence + ?Sized>(messages: &M) -> String {
    // One output buffer: the old shape collected every part into a `Vec`
    // (one allocation per part) and `join`ed it into a SECOND full-sized
    // string, doubling the peak for a transcript-sized span.
    let mut out = String::new();
    let mut first = true;
    for index in 0..messages.message_count() {
        for part in serialize_message_parts(messages.message_at(index)) {
            if !first {
                out.push_str("\n\n");
            }
            first = false;
            out.push_str(&part);
        }
    }
    out
}

/// Serialize ONE chunk for the summarizer prompt. A normal chunk serializes
/// exactly like [`serialize_conversation`] (the splitter guarantees it is
/// within the cap); an indivisible message that alone exceeds the cap is
/// truncated with the standard "[... N more characters truncated]" marker, in
/// message order, so the request can never exceed the budget.
/// [`write_chunk`] into a REUSED buffer: the streaming chunker keeps one
/// chunk buffer for the whole run, so a 247-chunk span does not allocate a
/// fresh chunk-sized string per call. Byte-identical to the allocating twin.
pub(crate) fn write_chunk<M: MessageSequence + ?Sized>(
    out: &mut String,
    messages: &M,
    chunk: &chunking::SummarizationChunk,
    max_chars: usize,
) {
    let mut first = out.is_empty();
    write_chunk_body(out, messages, chunk, max_chars, &mut first);
}

/// [`write_chunk`] for a destination that ALREADY carries the prompt's
/// framing prefix: the chunk's first part must not be preceded by the
/// message separator (`out.is_empty()` would be the wrong test).
fn write_chunk_prefixed<M: MessageSequence + ?Sized>(
    out: &mut String,
    messages: &M,
    chunk: &chunking::SummarizationChunk,
    max_chars: usize,
) {
    let mut first = true;
    write_chunk_body(out, messages, chunk, max_chars, &mut first);
}

fn write_chunk_body<M: MessageSequence + ?Sized>(
    out: &mut String,
    messages: &M,
    chunk: &chunking::SummarizationChunk,
    max_chars: usize,
    first: &mut bool,
) {
    let len = messages.message_count();
    let start = chunk.start.min(len);
    let end = chunk.end.min(len);
    if !chunk.oversize {
        for index in start..end {
            for part in serialize_message_parts(messages.message_at(index)) {
                if !*first {
                    out.push_str("\n\n");
                }
                *first = false;
                out.push_str(&part);
            }
        }
        return;
    }
    tracing::warn!(
        oversize_chars = chunk.chars,
        cap_chars = max_chars,
        "a single message exceeds the summarizer chunk budget; truncating it for the summary          (the session keeps the full message)"
    );
    let mut used = 0usize;
    for index in start..end {
        for part in serialize_message_parts(messages.message_at(index)) {
            if !*first {
                out.push_str("\n\n");
                used += 2;
            }
            *first = false;
            let part_chars = part.chars().count();
            if used + part_chars > max_chars {
                let remaining = max_chars.saturating_sub(used);
                if remaining > 0 {
                    out.push_str(&prompts::truncate_for_summary(&part, remaining));
                }
                return;
            }
            used += part_chars;
            out.push_str(&part);
        }
    }
}

// ============================================================================
// Summarization
// ============================================================================

/// Returns an error message when a summarization response cannot safely be
/// persisted.  A length stop contains partial text and must not become a
/// session checkpoint (upstream `getSummarizationFailure`).
fn summarization_failure(response: &AssistantMessage, label: &str) -> Option<String> {
    match response.stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        )),
        StopReason::Length => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}

/// Build the summarizer prompt into a REUSED buffer: the streaming chunker
/// keeps one prompt buffer for the whole run, so a 247-chunk span does not
/// allocate (and retain) a fresh chunk-sized string per call.
pub(crate) fn write_summarization_prompt(
    out: &mut String,
    conversation_text: &str,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) {
    write_summarization_prompt_parts(
        out,
        |out| out.push_str(conversation_text),
        previous_summary,
        custom_instructions,
    );
}

/// [`write_summarization_prompt`] whose conversation body is WRITTEN by
/// `write_conversation` directly into the prompt buffer. The single-call
/// compaction path uses this to serialize the chunk straight into the prompt:
/// the previous shape built the chunk text, copied it into the prompt, and
/// copied the prompt again into the request (three full copies of the
/// largest string in the run).
fn write_summarization_prompt_parts<F>(
    out: &mut String,
    write_conversation: F,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) where
    F: FnOnce(&mut String),
{
    out.push_str("<conversation>\n");
    write_conversation(out);
    out.push_str("\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        out.push_str("<previous-summary>\n");
        out.push_str(previous);
        out.push_str("\n</previous-summary>\n\n");
    }
    if previous_summary.is_some() {
        out.push_str(&prompts::update_summarization_prompt());
    } else {
        out.push_str(prompts::SUMMARIZATION_PROMPT);
    }
    if let Some(instructions) = custom_instructions {
        out.push_str("\n\nAdditional focus: ");
        out.push_str(instructions);
    }
}

/// Write the turn-prefix summarizer prompt (upstream
/// `generateTurnPrefixSummary`, compaction.ts:1078-1081): `# Conversation` +
/// `# Instructions` framing around the checkpoint prompt, plus the
/// `Additional focus:` suffix when custom instructions are present.
fn write_turn_prefix_prompt_parts<F>(
    out: &mut String,
    write_conversation: F,
    custom_instructions: Option<&str>,
) where
    F: FnOnce(&mut String),
{
    out.push_str("# Conversation\n");
    write_conversation(out);
    out.push_str("\n\n# Instructions\n");
    out.push_str(prompts::TURN_PREFIX_SUMMARIZATION_PROMPT);
    if let Some(instructions) = custom_instructions {
        out.push_str("\n\nAdditional focus: ");
        out.push_str(instructions);
    }
}

/// The model's own output limit (`0` = no model cap; the provider layer keeps
/// its default). Summarization requests carry no fixed cap: the provider sizes
/// output from the actual request, as for normal turns (upstream #11036).
///
/// `_reserve_tokens` is unused and kept for existing callers; `reserveTokens`
/// now only controls when compaction starts.
pub fn summarization_max_tokens(_reserve_tokens: u64, model: &Model) -> u64 {
    model.max_tokens
}

/// The model's own output limit for the turn-prefix summarizer call (`0` = no
/// model cap). Upstream removed the smaller reserve-fraction budget: the
/// provider sizes output from the actual request (upstream #11036).
///
/// `_reserve_tokens` is unused and kept for existing callers.
pub fn turn_prefix_max_tokens(_reserve_tokens: u64, model: &Model) -> u64 {
    model.max_tokens
}

/// The summarizer request context. It TAKES ownership of the prompt text
/// (the chunked runners build the prompt in one reusable buffer): the
/// request's message owns the string instead of holding a copy of it.
pub(crate) fn summarizer_context_owned(prompt_text: String) -> Context {
    Context {
        system_prompt: Some(prompts::SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![user_text_message_owned(prompt_text, now_ms())],
        tools: None,
    }
}

/// Validate a summarizer response and extract its text (upstream
/// `getSummarizationFailure` + tool-call guard).
pub(crate) fn summarizer_output(
    response: AssistantMessage,
    label: &str,
) -> Result<(String, Usage), String> {
    if let Some(failure) = summarization_failure(&response, label) {
        return Err(failure);
    }
    if response
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    {
        return Err(format!("{label} attempted to call a tool"));
    }
    let text = content_text(&response.content);
    Ok((text, response.usage))
}

/// Which framing prompt a summarization run opens with (the chunked runner
/// distinguishes the history checkpoint from the split-turn checkpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarizationStyle {
    /// History summarizer: `<conversation>` framing + [`prompts::SUMMARIZATION_PROMPT`]
    /// (upstream `generateSummaryWithUsage`).
    History,
    /// Turn-prefix summarizer: `# Conversation` / `# Instructions` framing +
    /// [`prompts::TURN_PREFIX_SUMMARIZATION_PROMPT`]
    /// (upstream `generateTurnPrefixSummary`).
    TurnPrefix,
}

impl SummarizationStyle {
    /// The response guard label ("Summarization" / "Turn prefix
    /// summarization") — the exact strings upstream callers/tests pin.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::History => "Summarization",
            Self::TurnPrefix => "Turn prefix summarization",
        }
    }
}

/// The prompt framing of one summarization run — [`SummarizationStyle`] plus
/// the branch summarizer's instruction prompt (which upstream builds from
/// `BRANCH_SUMMARY_PROMPT` / extension-supplied replacement instructions
/// before the call).
#[derive(Debug, Clone, Copy)]
pub enum SpanFraming<'a> {
    History,
    TurnPrefix,
    /// Branch summarizer: `<conversation>` framing followed by the resolved
    /// instruction text (upstream `generateBranchSummary`).
    Branch {
        instructions: &'a str,
    },
}

impl SpanFraming<'_> {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::History => SummarizationStyle::History.label(),
            Self::TurnPrefix => SummarizationStyle::TurnPrefix.label(),
            Self::Branch { .. } => "Branch summarization",
        }
    }

    /// The prompt for chunk 0 (the style's own framing), built into a REUSED
    /// buffer (the streaming chunker's shape).
    pub(crate) fn write_opening_prompt(
        self,
        out: &mut String,
        conversation_text: &str,
        previous_summary: Option<&str>,
        custom_instructions: Option<&str>,
    ) {
        self.write_opening_prompt_parts(
            out,
            |out| out.push_str(conversation_text),
            previous_summary,
            custom_instructions,
        );
    }

    /// [`Self::write_opening_prompt`] with the conversation body written
    /// straight into the prompt buffer — no intermediate chunk string.
    pub(crate) fn write_opening_prompt_for_chunk<M: MessageSequence + ?Sized>(
        self,
        out: &mut String,
        messages: &M,
        chunk: &chunking::SummarizationChunk,
        max_chars: usize,
        previous_summary: Option<&str>,
        custom_instructions: Option<&str>,
    ) {
        self.write_opening_prompt_parts(
            out,
            |out| write_chunk_prefixed(out, messages, chunk, max_chars),
            previous_summary,
            custom_instructions,
        );
    }

    fn write_opening_prompt_parts<F>(
        self,
        out: &mut String,
        write_conversation: F,
        previous_summary: Option<&str>,
        custom_instructions: Option<&str>,
    ) where
        F: FnOnce(&mut String),
    {
        match self {
            Self::History => write_summarization_prompt_parts(
                out,
                write_conversation,
                previous_summary,
                custom_instructions,
            ),
            Self::TurnPrefix => {
                write_turn_prefix_prompt_parts(out, write_conversation, custom_instructions);
            }
            Self::Branch { instructions } => {
                out.push_str("<conversation>\n");
                write_conversation(out);
                out.push_str("\n</conversation>\n\n");
                out.push_str(instructions);
            }
        }
    }

    /// The prompt for every later chunk: the "update" framing folds the chunk
    /// into the running summary, built into a REUSED buffer.
    pub(crate) fn write_folding_prompt(
        self,
        out: &mut String,
        conversation_text: &str,
        running: Option<&str>,
        custom_instructions: Option<&str>,
    ) {
        write_summarization_prompt(out, conversation_text, running, custom_instructions)
    }

    /// [`Self::write_folding_prompt`] with the conversation body written
    /// straight into the prompt buffer.
    pub(crate) fn write_folding_prompt_for_chunk<M: MessageSequence + ?Sized>(
        self,
        out: &mut String,
        messages: &M,
        chunk: &chunking::SummarizationChunk,
        max_chars: usize,
        running: Option<&str>,
        custom_instructions: Option<&str>,
    ) {
        let _ = self;
        write_summarization_prompt_parts(
            out,
            |out| write_chunk_prefixed(out, messages, chunk, max_chars),
            running,
            custom_instructions,
        );
    }

    /// Whether an `Aborted` terminal response is a terminal outcome for this
    /// framing (upstream checks it first only in the branch summarizer).
    pub(crate) fn abort_is_terminal(self) -> bool {
        matches!(self, Self::Branch { .. })
    }
}

/// Summarize a message span with the model, **chunking it when it does not fit
/// the context window** (pir extension; upstream always sends one call).
///
/// - A span that fits the per-chunk budget is summarized in ONE call whose
///   prompt is byte-identical to the upstream single-call prompt (the chunked
///   path is invisible).
/// - An oversized span is split by [`chunking::split_summarization_chunks`]
///   into turn-aligned chunks; chunk `0` opens with the style's prompt (and
///   the previous summary, when there is one) and every later chunk folds into
///   the running summary through the existing "update" prompt — the same
///   prompt upstream uses to fold new messages into an existing summary. The
///   result is ONE consolidated summary in the upstream format; the call count
///   is the chunk count (the last chunk's call is the merge).
/// - A chunk failure aborts the whole run with the underlying error and no
///   partial summary: the caller keeps the original context and persists no
///   checkpoint (the same honest degradation as a failed single call).
/// - Usage is summed over the calls so the compaction entry reports the whole
///   summarization cost.
///
/// The span is a borrowed [`MessageSequence`], so a huge span living as
/// references is never cloned for the call.
pub async fn summarize_messages_chunked_span<M: MessageSequence + ?Sized>(
    messages: &M,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    style: SummarizationStyle,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<(String, Usage), String> {
    let framing = match style {
        SummarizationStyle::History => SpanFraming::History,
        SummarizationStyle::TurnPrefix => SpanFraming::TurnPrefix,
    };
    summarize_span_chunked(
        messages,
        previous_summary,
        custom_instructions,
        framing,
        model,
        config,
        complete,
    )
    .await
    .map(|summarized| (summarized.text, summarized.usage))
}

/// Outcome of one chunked summarization run.
#[derive(Debug, Clone, PartialEq)]
pub struct SummarizedSpan {
    pub text: String,
    pub usage: Usage,
    /// The model's terminal stop reason was `Aborted` (the `/tree` Esc path):
    /// the partial text must NOT be persisted as a checkpoint (upstream
    /// branch-summarization.ts:355-358 checks this BEFORE the failure guards).
    pub aborted: bool,
}

/// [`summarize_messages_chunked_span`] for the branch summarizer: the same
/// chunked runner with the branch instruction prompt as chunk 0's framing.
pub(crate) async fn summarize_branch_span_chunked<M: MessageSequence + ?Sized>(
    messages: &M,
    instructions: &str,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<SummarizedSpan, String> {
    summarize_span_chunked(
        messages,
        None,
        None,
        SpanFraming::Branch { instructions },
        model,
        config,
        complete,
    )
    .await
}

/// The shared chunked runner (see [`summarize_messages_chunked_span`]).
async fn summarize_span_chunked<M: MessageSequence + ?Sized>(
    messages: &M,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    framing: SpanFraming<'_>,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<SummarizedSpan, String> {
    if messages.is_empty() {
        return Err(format!(
            "{} failed: no messages to summarize",
            framing.label()
        ));
    }
    let label = framing.label();
    let budget = chunking::summarization_chunk_budget(model, config.reserve_tokens);
    let chunks = chunking::split_summarization_chunks(messages, budget.chars);

    if chunks.len() <= 1 {
        // Upstream single-call path, byte for byte: the one chunk is the whole
        // span (a zero-message span never reaches here). The chunk is
        // serialized straight into the prompt buffer (one allocation), and the
        // prompt is MOVED into the request instead of being cloned.
        let mut prompt_text = String::new();
        match chunks.first() {
            Some(chunk) => framing.write_opening_prompt_for_chunk(
                &mut prompt_text,
                messages,
                chunk,
                budget.chars,
                previous_summary,
                custom_instructions,
            ),
            None => framing.write_opening_prompt(
                &mut prompt_text,
                "",
                previous_summary,
                custom_instructions,
            ),
        }
        tracing::debug!(label, "summarizer call");
        let response =
            complete_summarization_with_retry(&complete, summarizer_context_owned(prompt_text))
                .await
                .map_err(|error| format!("{label} failed: {error}"))?;
        return finish_summarized(response, label, framing.abort_is_terminal());
    }

    // Chunked path (pir extension): sequential map-with-running-summary. Each
    // request carries one bounded chunk plus the running summary, so no request
    // can exceed the window, and the peak holds one chunk's text at a time.
    // No fixed `maxTokens` is sent: the provider sizes output from the actual
    // request using the model's own limit (upstream #11036).
    tracing::debug!(
        chunks = chunks.len(),
        budget_chars = budget.chars,
        budget_tokens = budget.tokens,
        "chunked summarizer calls (pir extension over the single upstream call)"
    );
    let mut running: Option<String> = previous_summary.map(str::to_owned);
    let mut total_usage: Option<Usage> = None;
    for (index, chunk) in chunks.iter().enumerate() {
        // Serialize the bounded chunk directly into the prompt buffer and move
        // the finished prompt into the request (no chunk string, no clone).
        let mut prompt_text = String::new();
        if index == 0 {
            framing.write_opening_prompt_for_chunk(
                &mut prompt_text,
                messages,
                chunk,
                budget.chars,
                running.as_deref(),
                custom_instructions,
            );
        } else {
            framing.write_folding_prompt_for_chunk(
                &mut prompt_text,
                messages,
                chunk,
                budget.chars,
                running.as_deref(),
                custom_instructions,
            );
        }
        let response =
            complete_summarization_with_retry(&complete, summarizer_context_owned(prompt_text))
                .await
                .map_err(|error| {
                    format!(
                        "{label} failed (chunk {}/{}, no checkpoint persisted): {error}",
                        index + 1,
                        chunks.len()
                    )
                })?;
        let summarized = finish_summarized(response, label, framing.abort_is_terminal())?;
        if summarized.aborted {
            // A cancelled run produces no checkpoint at all (upstream checks
            // the abort outcome before anything else). The ABORTED OUTCOME is
            // reported as such — never as an error — so a multi-chunk span
            // keeps the single-call path's `/tree` Esc semantics
            // (`aborted: true`, leaf and transcript untouched).
            return Ok(SummarizedSpan {
                text: summarized.text,
                usage: total_usage.unwrap_or_default(),
                aborted: true,
            });
        }
        running = Some(summarized.text);
        total_usage = Some(match total_usage {
            Some(accumulated) => combine_usage(accumulated, summarized.usage),
            None => summarized.usage,
        });
    }
    Ok(SummarizedSpan {
        text: running.unwrap_or_default(),
        usage: total_usage.unwrap_or_default(),
        aborted: false,
    })
}

/// Validate one summarizer response and shape it as a [`SummarizedSpan`]
/// (upstream `getSummarizationFailure` + tool-call guard + the aborted flag).
///
/// `abort_is_terminal` marks the styles whose upstream caller checks the
/// abort BEFORE anything else (the branch summarizer); the history/turn-prefix
/// styles keep upstream's behavior of accepting a partial aborted response
/// through the standard failure guards.
pub(crate) fn finish_summarized(
    response: AssistantMessage,
    label: &str,
    abort_is_terminal: bool,
) -> Result<SummarizedSpan, String> {
    if abort_is_terminal && matches!(response.stop_reason, StopReason::Aborted) {
        return Ok(SummarizedSpan {
            text: String::new(),
            usage: response.usage,
            aborted: true,
        });
    }
    let (text, usage) = summarizer_output(response, label)?;
    Ok(SummarizedSpan {
        text,
        usage,
        aborted: false,
    })
}

/// [`summarize_messages_chunked_span`] over an owned message slice (the
/// historical public entry point).
pub async fn summarize_messages_chunked(
    messages: &[Message],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    style: SummarizationStyle,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<(String, Usage), String> {
    summarize_messages_chunked_span(
        messages,
        previous_summary,
        custom_instructions,
        style,
        model,
        config,
        complete,
    )
    .await
}

async fn run_history_summary<M: MessageSequence + ?Sized>(
    messages_to_summarize: &M,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<(String, Usage), String> {
    summarize_messages_chunked_span(
        messages_to_summarize,
        previous_summary,
        custom_instructions,
        SummarizationStyle::History,
        model,
        config,
        complete,
    )
    .await
}

pub(crate) async fn run_turn_prefix_summary<M: MessageSequence + ?Sized>(
    turn_prefix_messages: &M,
    custom_instructions: Option<&str>,
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<(String, Usage), String> {
    summarize_messages_chunked_span(
        turn_prefix_messages,
        None,
        custom_instructions,
        SummarizationStyle::TurnPrefix,
        model,
        config,
        complete,
    )
    .await
}

/// Shared choke point for every summarization call (upstream
/// `completeSummarization` + `retryAssistantCall`, compaction.ts:619-639):
/// wraps the single LLM call in ONE transient retry so a dropped stream
/// (`complete` returning `Err`) does not fail the whole compaction on the
/// first attempt. The first error is discarded only after the retry also
/// fails; the retry's error is reported.
pub(crate) async fn complete_summarization_with_retry(
    complete: &Arc<CompleteFn>,
    context: Context,
) -> Result<AssistantMessage, String> {
    // The observer is installed by the session around the whole compaction /
    // branch-summary run (`with_summarization_retry`); a standalone
    // compaction caller has none and keeps the silent single retry.
    let observer = SUMMARIZATION_RETRY_OBSERVER.try_with(Clone::clone).ok();
    match (complete)(context.clone()).await {
        Ok(response) => Ok(response),
        Err(first_error) => {
            // Upstream `retryAssistantCall` (retry.ts:236-253): the scheduled
            // notification fires before the backoff, the attempt-start after
            // it, and the finished notification once when the retried call
            // resolves. pir retries a dropped summarizer stream exactly once.
            if let Some(observer) = observer.as_ref() {
                observer(SummarizationRetryEvent::Scheduled {
                    attempt: 1,
                    max_attempts: 1,
                    delay_ms: 0,
                    error_message: first_error.clone(),
                });
            }
            tracing::warn!(
                "summarization stream dropped ({first_error}); retrying once (upstream retryAssistantCall)"
            );
            if let Some(observer) = observer.as_ref() {
                observer(SummarizationRetryEvent::AttemptStart);
            }
            let result = (complete)(context).await;
            if let Some(observer) = observer.as_ref() {
                observer(SummarizationRetryEvent::Finished);
            }
            result
        }
    }
}

pub(crate) fn combine_usage(a: Usage, b: Usage) -> Usage {
    // Optional splits (upstream `usage-totals.ts:31-47`): keep each split when
    // EITHER side reports it — a missing side contributes zero to the sum.
    let sum_opt =
        |a: Option<u64>, b: Option<u64>| a.map(|a| a.saturating_add(b.unwrap_or(0))).or(b);
    Usage {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cache_read: a.cache_read.saturating_add(b.cache_read),
        cache_write: a.cache_write.saturating_add(b.cache_write),
        cache_write_1h: sum_opt(a.cache_write_1h, b.cache_write_1h),
        reasoning: sum_opt(a.reasoning, b.reasoning),
        total_tokens: a.total_tokens.saturating_add(b.total_tokens),
        cost: crate::ai::types::Cost {
            input: a.cost.input + b.cost.input,
            output: a.cost.output + b.cost.output,
            cache_read: a.cost.cache_read + b.cost.cache_read,
            cache_write: a.cost.cache_write + b.cost.cache_write,
            total: a.cost.total + b.cost.total,
            // A sum is provider-reported only when every part is (upstream
            // `combineUsage`).
            total_from_provider: Some(
                a.cost.total_from_provider == Some(true)
                    && b.cost.total_from_provider == Some(true),
            ),
        },
    }
}

// ============================================================================
// Preparation (upstream `prepareCompaction`)
// ============================================================================

/// Pre-calculated compaction plan (upstream `CompactionPreparation`).
#[derive(Debug, Clone)]
pub struct CompactionPreparation {
    /// Session entry id of the first kept entry; `None` when the cut lands on
    /// a not-yet-persisted message (the compaction entry self-retains).
    pub first_kept_entry_id: Option<String>,
    /// Messages that will be summarized and discarded.
    pub messages_to_summarize: Vec<Message>,
    /// Messages that will be turned into a turn-prefix summary (split turns).
    pub turn_prefix_messages: Vec<Message>,
    /// Whether this is a split turn (cut point in the middle of a turn).
    pub is_split_turn: bool,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Summary from the previous compaction, for iterative update.
    pub previous_summary: Option<String>,
    /// File operations extracted from the summarized span.
    pub file_ops: FileOperations,
    /// Index into the flat projected message list where the kept messages
    /// start (surfaces as `firstKeptIndex`).
    pub first_kept_message_index: usize,
    /// Kept messages (everything from the cut point onward).
    pub kept_messages: Vec<Message>,
    /// Compaction settings in effect.
    pub settings: CompactionConfig,
}

/// The BORROWED twin of the upstream `getMessagesFromProjectedEntryForCompaction`
/// filter: compaction entries contribute nothing (they are represented by the
/// previous summary) and system messages are prompt state, replayed by the
/// compaction entry.
fn span_messages<'a, E: ProjectedEntry>(entry: &'a E) -> impl Iterator<Item = &'a Message> + 'a {
    entry
        .messages()
        .iter()
        .filter(|message| !matches!(message, Message::System(_)))
        .filter(move |_| !entry.is_compaction())
}

/// Whether the entry contributes at least one message to a summarized span.
fn span_has_messages<E: ProjectedEntry>(entry: &E) -> bool {
    span_messages(entry).next().is_some()
}

/// Entry-level index arithmetic of a compaction preparation, shared by the
/// owned and the borrowed materializers so the two can never drift.
struct PreparationPlan {
    first_kept_entry_id: Option<String>,
    tokens_before: u64,
    previous_summary: Option<String>,
    file_ops: FileOperations,
    first_kept_message_index: usize,
    /// Entry range of the history span (summarized and discarded).
    history_span: std::ops::Range<usize>,
    /// Entry range of the split-turn prefix span (empty unless split).
    turn_prefix_span: std::ops::Range<usize>,
    /// Entry range of the kept tail.
    kept_span: std::ops::Range<usize>,
    is_split_turn: bool,
}

/// Compute the compaction plan over PROJECTED entries (upstream
/// `prepareCompaction`): the cut point, the summarized spans, the kept tail,
/// the previous summary, and the file-operation tracking.
fn plan_compaction<E: ProjectedEntry, P: PathEntry>(
    projected: &[E],
    path_entries: &[P],
    settings: &CompactionConfig,
) -> Option<PreparationPlan> {
    if let Some(last) = path_entries.last() {
        if last.is_compaction() {
            return None;
        }
    }

    // The newest compaction is projected first. Older compaction entries can
    // still occur in its retained raw range, but their projected contribution
    // is empty.
    let prev_compaction_index = projected
        .iter()
        .position(|entry| entry.is_compaction() && !entry.messages().is_empty());

    let mut previous_summary: Option<String> = None;
    let mut previous_details: Option<JsonValue> = None;
    let mut boundary_start = 0usize;
    if let Some(index) = prev_compaction_index {
        let entry = &projected[index];
        previous_summary = entry.compaction_summary().map(str::to_owned);
        if !entry.compaction_from_hook() {
            previous_details = entry.compaction_details().cloned();
        }
        boundary_start = index + 1;
    }

    let tokens_before = estimate_projected_context_tokens(projected, path_entries).tokens;
    let cut_point = find_projected_cut_point(
        projected,
        boundary_start,
        projected.len(),
        settings.keep_recent_tokens,
    );

    let first_kept_entry_id = projected
        .get(cut_point.first_kept_entry_index)
        .and_then(|entry| entry.id().map(str::to_owned));
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index.max(0) as usize
    } else {
        cut_point.first_kept_entry_index
    };

    // Flat message index where the kept range starts (for firstKeptIndex).
    let first_kept_message_index: usize = projected[..cut_point.first_kept_entry_index]
        .iter()
        .map(|entry| entry.messages().len())
        .sum();

    let history_span = boundary_start..history_end;
    let turn_prefix_span = if cut_point.is_split_turn {
        cut_point.turn_start_index.max(0) as usize..cut_point.first_kept_entry_index
    } else {
        0..0
    };
    let kept_span = cut_point.first_kept_entry_index.min(projected.len())..projected.len();

    // No `Vec<&Message>` per entry: the old shape allocated one just to test
    // emptiness and another for the file-op scan of every entry.
    let history_empty = projected[history_span.clone()]
        .iter()
        .all(|entry| !span_has_messages(entry));
    let turn_prefix_empty = projected[turn_prefix_span.clone()]
        .iter()
        .all(|entry| !span_has_messages(entry));
    if history_empty && turn_prefix_empty {
        return None;
    }

    // Extract file operations from the summarized span and the previous
    // compaction's details (plus the turn prefix when splitting).
    let mut file_ops = FileOperations::default();
    carry_previous_file_ops(previous_details.as_ref(), &mut file_ops);
    for entry in &projected[history_span.clone()] {
        extract_file_ops_from_entry(entry, &mut file_ops);
    }
    if cut_point.is_split_turn {
        for entry in &projected[turn_prefix_span.clone()] {
            extract_file_ops_from_entry(entry, &mut file_ops);
        }
    }

    Some(PreparationPlan {
        first_kept_entry_id,
        tokens_before,
        previous_summary,
        file_ops,
        first_kept_message_index,
        history_span,
        turn_prefix_span,
        kept_span,
        is_split_turn: cut_point.is_split_turn,
    })
}

/// [`extract_file_ops_from_messages`] over borrowed messages (the borrowed
/// preparation path clones nothing to scan tool calls).
pub(crate) fn extract_file_ops_from_message_refs(messages: &[&Message], ops: &mut FileOperations) {
    for message in messages {
        extract_file_ops_from_message(message, ops);
    }
}

/// The ONE per-message file-op rule shared by the owned list, the borrowed
/// list, and the projected-entry scan.
fn extract_file_ops_from_message(message: &Message, ops: &mut FileOperations) {
    match message {
        Message::ToolResult(result) => {
            if let Some(calls) = result
                .details
                .as_ref()
                .and_then(|details| details.get("nestedCalls"))
                .and_then(|nested| nested.get("calls"))
                .and_then(JsonValue::as_array)
            {
                for call in calls {
                    add_file_op(
                        call.get("name").and_then(JsonValue::as_str).unwrap_or(""),
                        call.get("arguments"),
                        ops,
                    );
                }
            }
        }
        Message::Assistant(assistant) => {
            for block in &assistant.content {
                let ContentBlock::ToolCall(call) = block else {
                    continue;
                };
                let arguments = serde_json::to_value(&call.arguments).unwrap_or_default();
                add_file_op(&call.name, Some(&arguments), ops);
            }
        }
        _ => {}
    }
}

/// Scan the messages of ONE projected entry in place: the old shape built a
/// `Vec<&Message>` per entry just to feed [`extract_file_ops_from_message_refs`].
fn extract_file_ops_from_entry<E: ProjectedEntry>(entry: &E, ops: &mut FileOperations) {
    for message in entry.messages() {
        extract_file_ops_from_message(message, ops);
    }
}

/// Compute the compaction plan over PROJECTED entries (upstream
/// `prepareCompaction`).
///
/// `path_entries` is the raw leaf path the projection was built from (used
/// for the "newest entry is a compaction" short-circuit and the stale-usage
/// check); `projected` holds the light projected entries.
///
/// The OWNED shape (public API, tests, the ephemeral path): the spans are
/// materialized. The session paths use [`prepare_compaction_refs`], which runs
/// the identical plan without cloning the conversation.
pub fn prepare_compaction<E: ProjectedEntry, P: PathEntry>(
    projected: &[E],
    path_entries: &[P],
    settings: &CompactionConfig,
) -> Option<CompactionPreparation> {
    let plan = plan_compaction(projected, path_entries, settings)?;
    let messages_to_summarize: Vec<Message> = projected[plan.history_span.clone()]
        .iter()
        .flat_map(|entry| span_messages(entry).cloned())
        .collect();
    let turn_prefix_messages: Vec<Message> = projected[plan.turn_prefix_span.clone()]
        .iter()
        .flat_map(|entry| span_messages(entry).cloned())
        .collect();
    let kept_messages: Vec<Message> = projected[plan.kept_span.clone()]
        .iter()
        .flat_map(|entry| entry.messages().iter().cloned())
        .collect();
    Some(CompactionPreparation {
        first_kept_entry_id: plan.first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: plan.is_split_turn,
        tokens_before: plan.tokens_before,
        previous_summary: plan.previous_summary,
        file_ops: plan.file_ops,
        first_kept_message_index: plan.first_kept_message_index,
        kept_messages,
        settings: settings.clone(),
    })
}

/// The BORROWED compaction preparation: same plan, but every span is a list of
/// references into the caller's projected messages and tail — the shape the
/// session paths (auto/manual/navigate-tree compaction) consume, so a
/// transcript-sized span is never cloned just to be summarized.
#[derive(Debug, Clone)]
pub struct BorrowedCompactionPreparation<'a> {
    /// Session entry id of the first kept entry (`None` on a not-yet-persisted
    /// cut point; the compaction entry then self-retains).
    pub first_kept_entry_id: Option<String>,
    /// Messages that will be summarized and discarded (borrowed).
    pub messages_to_summarize: Vec<&'a Message>,
    /// Split-turn prefix messages (borrowed).
    pub turn_prefix_messages: Vec<&'a Message>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    /// Summary from the previous compaction, for iterative update.
    pub previous_summary: Option<String>,
    /// File operations extracted from the summarized span.
    pub file_ops: FileOperations,
    /// Index into the flat projected message list where the kept messages
    /// start (surfaces as `firstKeptIndex`).
    pub first_kept_message_index: usize,
    /// Kept messages, borrowed (the caller materializes the tail it needs).
    pub kept_messages: Vec<&'a Message>,
    /// Compaction settings in effect.
    pub settings: CompactionConfig,
}

/// [`prepare_compaction`] without materializing any span.
pub fn prepare_compaction_refs<'a, E: ProjectedEntry, P: PathEntry>(
    projected: &'a [E],
    path_entries: &[P],
    settings: &CompactionConfig,
) -> Option<BorrowedCompactionPreparation<'a>> {
    let plan = plan_compaction(projected, path_entries, settings)?;
    let messages_to_summarize: Vec<&Message> = projected[plan.history_span.clone()]
        .iter()
        .flat_map(|entry| span_messages(entry))
        .collect();
    let turn_prefix_messages: Vec<&Message> = projected[plan.turn_prefix_span.clone()]
        .iter()
        .flat_map(|entry| span_messages(entry))
        .collect();
    let kept_messages: Vec<&Message> = projected[plan.kept_span.clone()]
        .iter()
        .flat_map(|entry| entry.messages().iter())
        .collect();
    Some(BorrowedCompactionPreparation {
        first_kept_entry_id: plan.first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: plan.is_split_turn,
        tokens_before: plan.tokens_before,
        previous_summary: plan.previous_summary,
        file_ops: plan.file_ops,
        first_kept_message_index: plan.first_kept_message_index,
        kept_messages,
        settings: settings.clone(),
    })
}

// ============================================================================
// Compaction execution (upstream `compact`)
// ============================================================================

/// Run the prepared compaction: generate summaries (history +, for split
/// turns, the turn prefix) and merge them into one summary with the file
/// operation sections appended.
pub async fn compact_with_preparation(
    preparation: CompactionPreparation,
    model: &Model,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, String> {
    // Compaction span (upstream `TelemetryContext.startSpan("compaction")`):
    // carries the estimated context token count before the cut. Async-correct:
    // the span wraps the summarization body via `Instrument`.
    let span = tracing::info_span!("compaction.run", tokens_before = preparation.tokens_before);
    compact_with_preparation_in_span(preparation, model, complete, custom_instructions)
        .instrument(span)
        .await
}

/// The BORROWED twin of [`compact_with_preparation`]: identical summarization
/// and merge semantics over a preparation whose spans are references — the
/// session compaction paths use it so the summarized span is never cloned.
pub async fn compact_with_preparation_refs(
    preparation: BorrowedCompactionPreparation<'_>,
    model: &Model,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, String> {
    let span = tracing::info_span!("compaction.run", tokens_before = preparation.tokens_before);
    compact_with_preparation_refs_in_span(preparation, model, complete, custom_instructions)
        .instrument(span)
        .await
}

/// Body of [`compact_with_preparation_refs`], executed inside the
/// `compaction.run` span. The retained tail is materialized here (it is the
/// bounded keep-recent window the rebuilt context owns).
async fn compact_with_preparation_refs_in_span(
    preparation: BorrowedCompactionPreparation<'_>,
    model: &Model,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, String> {
    let BorrowedCompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        first_kept_message_index,
        kept_messages,
        settings,
    } = preparation;

    let (summary, summary_usage) = summarize_and_merge(
        messages_to_summarize.as_slice(),
        turn_prefix_messages.as_slice(),
        is_split_turn,
        previous_summary.as_deref(),
        custom_instructions,
        model,
        &settings,
        complete,
    )
    .await?;
    let mut summary = summary;
    let (read_files, modified_files) = compute_file_lists(&file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    tracing::info!(
        summary_tokens = summary_usage.total_tokens,
        first_kept_index = first_kept_message_index,
        "compaction finished"
    );
    Ok(CompactionResult {
        summary,
        tokens_before,
        usage: summary_usage,
        first_kept_index: first_kept_message_index,
        first_kept_entry_id,
        retained_tail: kept_messages.into_iter().cloned().collect(),
        read_files,
        modified_files,
    })
}

/// The shared summarize-and-merge body of both compaction entry points
/// (upstream `compact`): the history checkpoint, the split-turn checkpoint,
/// and the `**Turn Context (split turn):**` merge.
#[allow(clippy::too_many_arguments)]
async fn summarize_and_merge(
    messages_to_summarize: &[&Message],
    turn_prefix_messages: &[&Message],
    is_split_turn: bool,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
    model: &Model,
    settings: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<(String, Usage), String> {
    if is_split_turn && !turn_prefix_messages.is_empty() {
        let mut history_text = previous_summary.unwrap_or("No prior history.").to_owned();
        let mut history_usage: Option<Usage> = None;
        if !messages_to_summarize.is_empty() {
            let (text, usage) = run_history_summary(
                messages_to_summarize,
                previous_summary,
                custom_instructions,
                model,
                settings,
                Arc::clone(&complete),
            )
            .await?;
            history_text = text;
            history_usage = Some(usage);
        }
        let (turn_prefix_text, turn_prefix_usage) = run_turn_prefix_summary(
            turn_prefix_messages,
            custom_instructions,
            model,
            settings,
            complete,
        )
        .await?;
        let summary = format!(
            "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_text}"
        );
        let summary_usage = history_usage
            .map(|history| combine_usage(history, turn_prefix_usage))
            .unwrap_or(turn_prefix_usage);
        Ok((summary, summary_usage))
    } else {
        run_history_summary(
            messages_to_summarize,
            previous_summary,
            custom_instructions,
            model,
            settings,
            complete,
        )
        .await
    }
}

/// Body of [`compact_with_preparation`], executed inside the `compaction.run`
/// span.
async fn compact_with_preparation_in_span(
    preparation: CompactionPreparation,
    model: &Model,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, String> {
    let CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        first_kept_message_index,
        kept_messages,
        settings,
    } = preparation;

    // The shared body runs over borrowed spans; the owned lists stay alive
    // for it (no copy is made to call it).
    let history_refs: Vec<&Message> = messages_to_summarize.iter().collect();
    let turn_prefix_refs: Vec<&Message> = turn_prefix_messages.iter().collect();
    let (summary_text, summary_usage) = summarize_and_merge(
        history_refs.as_slice(),
        turn_prefix_refs.as_slice(),
        is_split_turn,
        previous_summary.as_deref(),
        custom_instructions,
        model,
        &settings,
        complete,
    )
    .await?;
    let mut summary = summary_text;

    // Compute file lists and append to the summary.
    let (read_files, modified_files) = compute_file_lists(&file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    // Compaction summary (upstream `addEvent`/`setStatus` on the compaction
    // span): one line per successful compaction with the summary token usage
    // and the cut point.
    tracing::info!(
        summary_tokens = summary_usage.total_tokens,
        first_kept_index = first_kept_message_index,
        "compaction finished"
    );
    Ok(CompactionResult {
        summary,
        tokens_before,
        usage: summary_usage,
        first_kept_index: first_kept_message_index,
        first_kept_entry_id,
        retained_tail: kept_messages,
        read_files,
        modified_files,
    })
}

/// The framed summary user message replayed after a compaction (upstream
/// `createCompactionSummaryMessage` + `convertToLlm` framing).
fn compaction_summary_message(summary: &str) -> Message {
    user_text_message(
        &format!(
            "{}{}{}",
            prompts::COMPACTION_SUMMARY_PREFIX,
            summary,
            prompts::COMPACTION_SUMMARY_SUFFIX
        ),
        now_ms(),
    )
}

/// Rebuild a context after a successful compaction: the framed summary user
/// message followed by the kept messages.
pub fn rebuilt_context(original: &Context, result: &CompactionResult) -> Context {
    let mut messages = Vec::with_capacity(1 + result.retained_tail.len());
    messages.push(compaction_summary_message(&result.summary));
    messages.extend(result.retained_tail.iter().cloned());
    Context {
        system_prompt: original.system_prompt.clone(),
        messages,
        tools: original.tools.clone(),
    }
}

/// Run the whole compaction over an explicitly supplied projection (the
/// widening fallback of a paged session, and the overflow-recovery path): the
/// spans stay borrowed, so nothing beyond the projection's own messages is
/// materialized.
pub async fn compact_projection(
    projection: &crate::session_manager::SessionProjection,
    branch: &[PathEntryMeta],
    model: &Model,
    config: &CompactionConfig,
    complete: Arc<CompleteFn>,
) -> Result<CompactionResult, String> {
    let metas = ProjectionEntryMeta::from_projection(&projection.entries);
    let refs = ProjectionEntryMeta::cut_refs(&metas, &projection.messages);
    match prepare_compaction_refs(&refs, branch, config) {
        Some(preparation) => {
            compact_with_preparation_refs(preparation, model, complete, None).await
        }
        None => Err("compaction found no messages to summarize".to_string()),
    }
}

// ============================================================================
// Agent-loop hook (message-level fallback for session-less contexts)
// ============================================================================

/// Build the agent-loop compaction hook from a Context alone (session-less
/// callers): each message is treated as its own projected entry, so the cut
/// point can never carry a session entry id.
///
/// When the trigger fires, the hook cuts and makes the summarizer call(s);
/// on failure it returns the original context unchanged (upstream aborts
/// persistence on failure).
pub fn compaction_fn(model: Model, complete: CompleteFn, config: CompactionConfig) -> CompactionFn {
    let complete = Arc::new(complete);
    Box::new(move |context: Arc<Context>| {
        let model = model.clone();
        let complete = Arc::clone(&complete);
        let config = config.clone();
        Box::pin(async move {
            if !should_compact(&context, &model, &config) {
                // Identity preserved: the loop's "did it compact?" check is a
                // pointer comparison, not a structural one.
                return context;
            }
            // Borrowed basis: one synthetic entry per message, no clones.
            let refs: Vec<CutEntryRef<'_>> = context
                .messages
                .iter()
                .map(CutEntryRef::from_message)
                .collect();
            let result = match prepare_compaction_refs(&refs, &[] as &[PathEntryMeta], &config) {
                Some(preparation) => {
                    compact_with_preparation_refs(preparation, &model, complete, None).await
                }
                None => Err("compaction found no messages to summarize".to_string()),
            };
            match result {
                Ok(result) => Arc::new(rebuilt_context(&context, &result)),
                Err(_) => context,
            }
        })
    })
}

/// Run one message-level compaction directly (upstream `compact` on a
/// session-less message list): cut, summarize, and return the result.
pub fn compact_context(
    context: Context,
    model: &Model,
    complete: Arc<CompleteFn>,
    config: &CompactionConfig,
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> BoxFuture<'static, Result<CompactionResult, String>> {
    // Borrowed basis: one synthetic entry per message (no clone of the
    // conversation); the future owns only the small plan metadata, so the
    // context is moved in rather than cloned.
    let model = model.clone();
    let config = config.clone();
    let previous_summary = previous_summary.map(str::to_owned);
    let custom_instructions = custom_instructions.map(str::to_owned);
    Box::pin(async move {
        let refs: Vec<CutEntryRef<'_>> = context
            .messages
            .iter()
            .map(CutEntryRef::from_message)
            .collect();
        let mut preparation = prepare_compaction_refs(&refs, &[] as &[PathEntryMeta], &config)
            .ok_or_else(|| "compaction found no messages to summarize".to_string())?;
        if previous_summary.is_some() {
            preparation.previous_summary = previous_summary;
        }
        compact_with_preparation_refs(
            preparation,
            &model,
            complete,
            custom_instructions.as_deref(),
        )
        .await
    })
}

// ============================================================================
// Overflow recovery
// ============================================================================

/// Recover from a context-overflow error (upstream trigger point 1 in
/// `agent-session.ts`): drop the failed trailing assistant message, compact
/// once, and hand the compacted context back for a single retry.
///
/// Returns `None` when there is no trailing assistant message to drop or the
/// compaction fails; the caller then keeps the original context.
pub fn recover_from_overflow(
    context: Context,
    error_message: &str,
    model: &Model,
    complete: Arc<CompleteFn>,
    config: &CompactionConfig,
) -> BoxFuture<'static, Option<Context>> {
    let mut context = context;
    if !matches!(context.messages.last(), Some(Message::Assistant(_))) {
        return Box::pin(async { None });
    }
    context.messages.pop();

    let model = model.clone();
    let config = config.clone();
    let focus = format!("Recover from a context-overflow error: {error_message}");
    Box::pin(async move {
        match compact_context(
            context.clone(),
            &model,
            complete,
            &config,
            None,
            Some(&focus),
        )
        .await
        {
            Ok(result) => Some(rebuilt_context(&context, &result)),
            Err(_) => None,
        }
    })
}
