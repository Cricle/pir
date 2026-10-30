//! Pure compaction algorithms: token estimation, trigger predicate, and cut
//! point selection over projected session entries.
//!
//! Rust port of upstream `packages/coding-agent/src/core/compaction/compaction.ts`
//! (`calculateContextTokens`, `estimateContextTokens`, `estimateTokens`,
//! `estimateProjectedContextTokens`, `shouldCompact`, `findProjectedCutPoint`).
//! Estimation uses upstream compaction's `chars / 4` heuristic
//! (`compaction.ts` `estimateTokens`; the `/3.5` figure belongs to
//! `utils/estimate.ts` semantics only); the cut point walks *projected*
//! entries so omitted (context-edited) entries and the newest-compaction-only
//! rule are respected.

use serde::{Deserialize, Serialize};

use crate::ai::types::{ContentBlock, Context, Message, Model, StopReason, Usage};
use crate::session_manager::{FileEntry, StoredMessage};

/// Estimated character weight of an image content block (upstream
/// `ESTIMATED_IMAGE_CHARS`).
pub const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// Chars-per-token heuristic (upstream `compaction.ts` `estimateTokens`
/// divides by 4).
pub const CHARS_PER_TOKEN: f64 = 4.0;

// ============================================================================
// Configuration
// ============================================================================

/// Compaction settings (upstream `CompactionSettings` / pi `settings.compaction`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Whether automatic compaction is enabled.
    pub enabled: bool,
    /// Token head-room reserved for the next model response (upstream
    /// `reserveTokens`).
    pub reserve_tokens: u64,
    /// Approximate token budget kept verbatim at the tail of the context
    /// (upstream `keepRecentTokens`).
    pub keep_recent_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

// ============================================================================
// Token calculation
// ============================================================================

/// Total context tokens from a usage record.  Uses the native `totalTokens`
/// field when available, falls back to computing from components (upstream
/// `calculateContextTokens`).
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

/// Usage from an assistant message if available.  Skips aborted, error, and
/// all-zero usage messages as they don't have valid usage data (upstream
/// `getAssistantUsage`).
fn assistant_usage(message: &Message) -> Option<&Usage> {
    if let Message::Assistant(assistant) = message {
        if !matches!(
            assistant.stop_reason,
            StopReason::Aborted | StopReason::Error
        ) && calculate_context_tokens(&assistant.usage) > 0
        {
            return Some(&assistant.usage);
        }
    }
    None
}

/// Every message variant carries an upstream `timestamp` (Unix ms).
fn message_timestamp(message: &Message) -> u64 {
    match message {
        Message::User(message) | Message::System(message) | Message::Developer(message) => {
            message.timestamp
        }
        Message::Assistant(message) => message.timestamp,
        Message::ToolResult(message) => message.timestamp,
    }
}

/// Upstream `getLastAssistantUsageInfo` (`estimate.ts:71-95`): scan forward,
/// keeping the newest prefix timestamp; an assistant usage only applies when
/// its own timestamp is at least that prefix timestamp. This rejects a usage
/// captured before a later-inserted prefix message (e.g. a compaction summary)
/// describes the current prefix.
///
/// System messages are prompt state, not conversation: their timestamps record
/// when the prompt/declaration was materialized (a tool declaration is stamped
/// `now_ms` on the request that first needs it), not a position in the
/// conversational prefix, and they carry no usage of their own. Counting them
/// would let a declaration inserted before a response invalidate that
/// response's usage — the print host's threshold check then missed the very
/// usage its next request had just produced.
fn last_assistant_usage_info<M: MessageSequence + ?Sized>(messages: &M) -> Option<(usize, &Usage)> {
    let mut latest_prefix_timestamp = 0u64;
    let mut usage_info: Option<(usize, &Usage)> = None;
    for index in 0..messages.message_count() {
        let message = messages.message_at(index);
        let timestamp = message_timestamp(message);
        if let Some(usage) = assistant_usage(message) {
            if timestamp >= latest_prefix_timestamp {
                usage_info = Some((index, usage));
            }
        }
        if !matches!(message, Message::System(_)) {
            latest_prefix_timestamp = latest_prefix_timestamp.max(timestamp);
        }
    }
    usage_info
}
/// Detailed result of [`estimate_context_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the last assistant usage record.
    pub usage_tokens: u64,
    /// Estimated tokens of the messages trailing that usage record.
    pub trailing_tokens: u64,
    /// Index of the message the usage was taken from, when any.
    pub last_usage_index: Option<usize>,
}

/// Text-and-image character count of user-side content (upstream
/// `estimateTextAndImageContentChars`): text blocks contribute their length,
/// image blocks a fixed 4800 characters, everything else nothing. Character
/// (not byte) counting approximates upstream's UTF-16 `.length`, so CJK
/// content is not overestimated 2-3x.
fn estimate_text_and_image_chars(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.chars().count(),
            ContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
            _ => 0,
        })
        .sum()
}

fn ceil_div_chars(chars: usize) -> u64 {
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

/// Character weight of a single message, before the chars-per-token rounding.
///
/// A system message counts its text/image content PLUS its named prompt
/// sections and its declared tool definitions, exactly like upstream
/// (`estimateTokens`'s `system` case: every non-null section's length plus
/// `JSON.stringify(toolsAdded).length`). Leaving those out under-estimated a
/// session whose prompt is mostly tool declarations, so a compaction could
/// trigger late; `toolsRemoved` is not counted upstream either.
fn estimate_message_chars(message: &Message) -> usize {
    match message {
        Message::System(system) => {
            let sections: usize = system
                .sections
                .iter()
                .flat_map(|sections| sections.values())
                .filter_map(|text| text.as_ref())
                .map(|text| text.chars().count())
                .sum();
            let tools = system
                .tools_added
                .as_ref()
                .and_then(|tools| serde_json::to_string(tools).ok())
                .map_or(0, |json| json.chars().count());
            estimate_text_and_image_chars(&system.content) + sections + tools
        }
        Message::User(user) | Message::Developer(user) => {
            estimate_text_and_image_chars(&user.content)
        }
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.text.chars().count(),
                ContentBlock::Thinking(thinking) => thinking.thinking.chars().count(),
                ContentBlock::ToolCall(call) => {
                    let arguments = serde_json::to_string(&call.arguments).unwrap_or_default();
                    call.name.chars().count() + arguments.chars().count()
                }
                _ => 0,
            })
            .sum(),
        Message::ToolResult(result) => estimate_text_and_image_chars(&result.content),
    }
}

/// Estimate the token count of a single message using the chars/4 heuristic
/// (upstream compaction `estimateTokens`). Conservative (overestimates
/// tokens).
pub fn estimate_message_tokens(message: &Message) -> u64 {
    ceil_div_chars(estimate_message_chars(message))
}

/// Borrowed message-sequence view: either a contiguous owned message slice or
/// a slice of message references (a projection's virtual concatenation of
/// projected-entry messages plus a not-yet-projected context tail, or a
/// borrowed summarization span). ONE estimator/serializer implementation runs
/// over both — no message is cloned to walk a span.
pub trait MessageSequence {
    fn message_count(&self) -> usize;
    fn message_at(&self, index: usize) -> &Message;

    fn is_empty(&self) -> bool {
        self.message_count() == 0
    }
}

impl MessageSequence for [Message] {
    fn message_count(&self) -> usize {
        self.len()
    }
    fn message_at(&self, index: usize) -> &Message {
        &self[index]
    }
}

impl MessageSequence for [&Message] {
    fn message_count(&self) -> usize {
        self.len()
    }
    fn message_at(&self, index: usize) -> &Message {
        self[index]
    }
}

/// Estimate context tokens from messages, using the last assistant usage when
/// available. Messages after the last usage record are estimated with
/// [`estimate_message_tokens`] (upstream `estimateContextTokens`).
///
/// The usage scan is upstream's `getLastAssistantUsageInfo`
/// (`packages/ai/src/utils/estimate.ts:71-95`): a forward scan guarded by the
/// newest prefix timestamp. An assistant usage only applies when its message
/// timestamp is at least the newest timestamp seen so far, so a compaction
/// summary inserted after a response invalidates that response's usage for
/// the current prefix. (The earlier reverse scan was wrong: upstream does
/// have this guard.) Callers that need a post-compaction check still apply
/// the projection-level invalidation in [`estimate_projected_context_tokens`]
/// (compaction.ts:227-262).
pub fn estimate_context_tokens(messages: &[Message]) -> ContextUsageEstimate {
    estimate_context_tokens_over(messages)
}

/// [`estimate_context_tokens`] over any borrowed [`MessageSequence`]: the same
/// estimate with no message materialization. The projection estimator feeds it
/// the entry messages plus the live context tail as one virtual sequence.
fn estimate_context_tokens_over<M: MessageSequence + ?Sized>(messages: &M) -> ContextUsageEstimate {
    let usage_info = last_assistant_usage_info(messages);

    let Some((index, usage)) = usage_info else {
        // No usage record describes this context: estimate the WHOLE context
        // by character count and round once. Rounding per message instead
        // adds up to a token of rounding slack per message, systematically
        // inflating a long transcript; the context is a single token budget,
        // so its estimate is `ceil(total_chars / 4)`.
        let chars: usize = (0..messages.message_count())
            .map(|i| estimate_message_chars(messages.message_at(i)))
            .sum();
        let estimated = ceil_div_chars(chars);
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(usage);
    let trailing_tokens: u64 = (index + 1..messages.message_count())
        .map(|i| estimate_message_tokens(messages.message_at(i)))
        .sum();
    ContextUsageEstimate {
        tokens: usage_tokens.saturating_add(trailing_tokens),
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// Check whether compaction should trigger for this context and model
/// (upstream `shouldCompact`): `contextTokens > contextWindow - reserveTokens`.
pub fn should_compact(context: &Context, model: &Model, config: &CompactionConfig) -> bool {
    should_compact_tokens(
        estimate_context_tokens(&context.messages).tokens,
        model.context_window,
        config,
    )
}

/// [`should_compact`] over a precomputed token count (upstream
/// `shouldCompact(contextTokens, contextWindow, settings)`).
pub fn should_compact_tokens(
    context_tokens: u64,
    context_window: u64,
    config: &CompactionConfig,
) -> bool {
    if !config.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(config.reserve_tokens)
}

// ============================================================================
// Projected entries (light view for cut-point selection)
// ============================================================================

/// Per-entry metadata of a projection WITHOUT the messages: the shared source
/// entry plus the NUMBER of model-visible messages it contributes.
///
/// MEMORY SHAPE (pir extension): the compaction decision path (trigger check →
/// cut point → summarization) reads the live context's messages in entry
/// order, so it only needs to know how many messages each entry contributed
/// and what the entry is — never a second copy of the conversation. A
/// transcript-sized session therefore runs the whole compaction pipeline over
/// slices of the context it already holds.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionEntryMeta {
    pub source_entry: std::sync::Arc<FileEntry>,
    /// Number of model-visible messages this entry contributes (its
    /// [`crate::session_manager::ProjectedSessionEntry::message_count`]).
    pub message_count: usize,
}

impl ProjectionEntryMeta {
    /// Derive the metadata from a materialized projection (ranges — cheap).
    pub fn from_projection(
        entries: &[crate::session_manager::ProjectedSessionEntry],
    ) -> Vec<ProjectionEntryMeta> {
        entries
            .iter()
            .map(|entry| ProjectionEntryMeta {
                source_entry: std::sync::Arc::clone(&entry.source_entry),
                message_count: entry.message_count(),
            })
            .collect()
    }

    /// The borrowed cut basis over `live` (the context the loop is about to
    /// send, or the session projection's own message list): one
    /// [`CutEntryRef`] per entry, sliced by the recorded counts, plus one
    /// synthetic reference per message left over at the end (the run's
    /// not-yet-projected tail, exactly the entries
    /// [`materialize_projected_tail`] synthesizes).
    pub fn cut_refs<'a>(
        metas: &'a [ProjectionEntryMeta],
        live: &'a [Message],
    ) -> Vec<CutEntryRef<'a>> {
        // A checkpoint's stored system-message replay is part of the session
        // PROJECTION's message list, but the in-memory context a successful
        // compaction returns is only `[summary, retained tail]`: it never
        // replays the checkpoint system message (that replay restores prompt
        // state on a RESUME). When the metadata came from the session
        // projection and `live` is such a rebuilt context, the checkpoint
        // entry's leading count would swallow the first retained message and
        // shift every later entry's range by one. Drop that one contribution
        // for this call so the entry ranges stay aligned with `live`.
        let skip_leading_replay = metas.first().is_some_and(|meta| {
            meta.message_count > 1
                && matches!(
                    meta.source_entry.as_ref(),
                    FileEntry::Compaction(compaction) if compaction.system_message.is_some()
                )
                && !matches!(live.first(), Some(Message::System(_)))
        });
        let mut refs: Vec<CutEntryRef<'a>> = Vec::with_capacity(metas.len());
        let mut index = 0usize;
        for (position, meta) in metas.iter().enumerate() {
            let count = if position == 0 && skip_leading_replay {
                meta.message_count - 1
            } else {
                meta.message_count
            };
            let end = index.saturating_add(count).min(live.len());
            refs.push(CutEntryRef::from_source(
                &meta.source_entry,
                &live[index.min(end)..end],
            ));
            index = end;
        }
        refs.extend(live[index..].iter().map(CutEntryRef::from_message));
        refs
    }
}

/// A projection entry reduced to what cut-point selection and preparation
/// need (the projection's `sourceEntry` plus its projected messages).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CutEntry {
    /// Session entry id; `None` for messages not yet persisted to the session
    /// (e.g. the current turn's streamed messages).
    pub id: Option<String>,
    pub is_compaction: bool,
    /// Compaction entry summary text (`None` for other entries).
    pub compaction_summary: Option<String>,
    /// Compaction entry details object + `fromHook` (for file-op carry-over).
    pub compaction_details: Option<serde_json::Value>,
    pub compaction_from_hook: bool,
    pub is_context_edit: bool,
    pub context_edit_target_id: Option<String>,
    /// context_edit whose replacement is non-null.
    pub has_replacement: bool,
    /// `sessionEntryToContextMessages(entry).len() > 0 && !context_edit`
    /// (upstream `isIntrinsicallyVisible`).
    pub intrinsically_visible: bool,
    /// Source entry is a message entry with an assistant-role message
    /// (upstream's omitted recovery-attempt detection).
    pub is_assistant: bool,
    /// Model-visible messages after context edits.
    pub messages: Vec<Message>,
}

/// The entry metadata + message view cut-point selection, estimation, and
/// preparation work on.
///
/// Implemented by the owning [`CutEntry`] (the public, materialized shape) and
/// by the borrowed [`CutEntryRef`] — ONE implementation of every algorithm for
/// both, so the session paths can run the whole compaction pipeline over
/// borrowed slices of the live context instead of materializing a second
/// transcript-sized copy of it.
pub trait ProjectedEntry {
    fn id(&self) -> Option<&str>;
    fn is_compaction(&self) -> bool;
    fn compaction_summary(&self) -> Option<&str>;
    fn compaction_details(&self) -> Option<&serde_json::Value>;
    fn compaction_from_hook(&self) -> bool;
    fn is_context_edit(&self) -> bool;
    fn context_edit_target_id(&self) -> Option<&str>;
    fn has_replacement(&self) -> bool;
    fn intrinsically_visible(&self) -> bool;
    fn is_assistant(&self) -> bool;
    /// Model-visible messages after context edits (possibly empty).
    fn messages(&self) -> &[Message];

    /// A message that may bound a retained tail (upstream
    /// `isCutPointMessage`): never tool results, which must follow their
    /// tool call.
    fn is_cut_point(&self) -> bool {
        !self.is_compaction() && self.messages().iter().any(is_cut_point_message)
    }

    /// Whether any of the entry's messages starts a turn (upstream
    /// `isTurnStartMessage`).
    fn is_turn_start(&self) -> bool {
        !self.is_compaction() && self.messages().iter().any(is_turn_start_message)
    }
}

/// Borrowed twin of [`CutEntry`]: same metadata, but the messages are a slice
/// of the caller's conversation — building one costs no message clone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CutEntryRef<'a> {
    pub id: Option<&'a str>,
    pub is_compaction: bool,
    pub compaction_summary: Option<&'a str>,
    pub compaction_details: Option<&'a serde_json::Value>,
    pub compaction_from_hook: bool,
    pub is_context_edit: bool,
    pub context_edit_target_id: Option<&'a str>,
    pub has_replacement: bool,
    pub intrinsically_visible: bool,
    pub is_assistant: bool,
    pub messages: &'a [Message],
}

impl<'a> ProjectedEntry for CutEntryRef<'a> {
    fn id(&self) -> Option<&str> {
        self.id
    }
    fn is_compaction(&self) -> bool {
        self.is_compaction
    }
    fn compaction_summary(&self) -> Option<&str> {
        self.compaction_summary
    }
    fn compaction_details(&self) -> Option<&serde_json::Value> {
        self.compaction_details
    }
    fn compaction_from_hook(&self) -> bool {
        self.compaction_from_hook
    }
    fn is_context_edit(&self) -> bool {
        self.is_context_edit
    }
    fn context_edit_target_id(&self) -> Option<&str> {
        self.context_edit_target_id
    }
    fn has_replacement(&self) -> bool {
        self.has_replacement
    }
    fn intrinsically_visible(&self) -> bool {
        self.intrinsically_visible
    }
    fn is_assistant(&self) -> bool {
        self.is_assistant
    }
    fn messages(&self) -> &[Message] {
        self.messages
    }
}

impl<'a> CutEntryRef<'a> {
    /// Borrow the light view of one RAW session entry plus the messages it
    /// contributes (a slice of the live context at the entry's position).
    pub fn from_source(source: &'a FileEntry, messages: &'a [Message]) -> CutEntryRef<'a> {
        let (is_compaction, compaction_summary, compaction_details, compaction_from_hook) =
            match source {
                FileEntry::Compaction(compaction) => (
                    true,
                    Some(compaction.summary.as_str()),
                    compaction.details.as_ref(),
                    compaction.from_hook.unwrap_or(false),
                ),
                _ => (false, None, None, false),
            };
        let (is_context_edit, context_edit_target_id, has_replacement) = match source {
            FileEntry::ContextEdit(edit) => (
                true,
                Some(edit.target_id.as_str()),
                edit.replacement.is_some(),
            ),
            _ => (false, None, false),
        };
        let is_assistant = match source {
            FileEntry::Message(inner) => matches!(&inner.message, StoredMessage::Assistant(_)),
            _ => false,
        };
        CutEntryRef {
            id: source.id(),
            is_compaction,
            compaction_summary,
            compaction_details,
            compaction_from_hook,
            is_context_edit,
            context_edit_target_id,
            has_replacement,
            intrinsically_visible: !is_context_edit
                && crate::session_manager::projection::projected_message_count(source, None) > 0,
            is_assistant,
            messages,
        }
    }

    /// Borrow the light view of one projected session entry: the metadata is
    /// derived from the shared source entry and the messages are the entry's
    /// range inside `projection_messages` — no clone at all.
    pub fn from_projection(
        entry: &'a crate::session_manager::ProjectedSessionEntry,
        projection_messages: &'a [Message],
    ) -> CutEntryRef<'a> {
        Self::from_source(&entry.source_entry, entry.messages_in(projection_messages))
    }

    /// Borrow the light view for one message not backed by a session entry
    /// (the not-yet-persisted tail of the live context).
    pub fn from_message(message: &'a Message) -> CutEntryRef<'a> {
        CutEntryRef {
            id: None,
            is_compaction: false,
            compaction_summary: None,
            compaction_details: None,
            compaction_from_hook: false,
            is_context_edit: false,
            context_edit_target_id: None,
            has_replacement: false,
            intrinsically_visible: true,
            is_assistant: matches!(message, Message::Assistant(_)),
            messages: std::slice::from_ref(message),
        }
    }
}

impl ProjectedEntry for CutEntry {
    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    fn is_compaction(&self) -> bool {
        self.is_compaction
    }
    fn compaction_summary(&self) -> Option<&str> {
        self.compaction_summary.as_deref()
    }
    fn compaction_details(&self) -> Option<&serde_json::Value> {
        self.compaction_details.as_ref()
    }
    fn compaction_from_hook(&self) -> bool {
        self.compaction_from_hook
    }
    fn is_context_edit(&self) -> bool {
        self.is_context_edit
    }
    fn context_edit_target_id(&self) -> Option<&str> {
        self.context_edit_target_id.as_deref()
    }
    fn has_replacement(&self) -> bool {
        self.has_replacement
    }
    fn intrinsically_visible(&self) -> bool {
        self.intrinsically_visible
    }
    fn is_assistant(&self) -> bool {
        self.is_assistant
    }
    fn messages(&self) -> &[Message] {
        &self.messages
    }
}

impl CutEntry {
    /// Build the light view from a projected session entry (OWNED twin; the
    /// session paths use [`CutEntryRef::from_projection`], which clones
    /// nothing).
    pub fn from_projection(
        entry: &crate::session_manager::ProjectedSessionEntry,
        projection_messages: &[Message],
    ) -> Self {
        let borrowed = CutEntryRef::from_projection(entry, projection_messages);
        Self {
            id: borrowed.id.map(str::to_owned),
            is_compaction: borrowed.is_compaction,
            compaction_summary: borrowed.compaction_summary.map(str::to_owned),
            compaction_details: borrowed.compaction_details.cloned(),
            compaction_from_hook: borrowed.compaction_from_hook,
            is_context_edit: borrowed.is_context_edit,
            context_edit_target_id: borrowed.context_edit_target_id.map(str::to_owned),
            has_replacement: borrowed.has_replacement,
            intrinsically_visible: borrowed.intrinsically_visible,
            is_assistant: borrowed.is_assistant,
            messages: borrowed.messages.to_vec(),
        }
    }

    /// Synthesize the light view for a message not backed by a session entry.
    pub fn from_message(message: Message) -> Self {
        let is_assistant = matches!(message, Message::Assistant(_));
        Self {
            intrinsically_visible: true,
            is_assistant,
            messages: vec![message],
            ..Self::default()
        }
    }

    /// A message that may bound a retained tail (the inherent twin of
    /// [`ProjectedEntry::is_cut_point`], kept for the public pre-trait shape).
    pub fn is_cut_point(&self) -> bool {
        <Self as ProjectedEntry>::is_cut_point(self)
    }

    /// Whether any of the entry's messages starts a turn (the inherent twin of
    /// [`ProjectedEntry::is_turn_start`]).
    pub fn is_turn_start(&self) -> bool {
        <Self as ProjectedEntry>::is_turn_start(self)
    }
}

/// A message that may bound a retained tail (upstream `isCutPointMessage`):
/// never tool results, which must follow their tool call.
pub fn is_cut_point_message(message: &Message) -> bool {
    matches!(message, Message::User(_) | Message::Assistant(_))
}

/// A message that starts a turn (upstream `isTurnStartMessage`).
pub fn is_turn_start_message(message: &Message) -> bool {
    matches!(message, Message::User(_))
}

// ============================================================================
// Cut point selection (upstream `findProjectedCutPoint`)
// ============================================================================

/// Result of cut-point selection (upstream `CutPointResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first entry to keep.
    pub first_kept_entry_index: usize,
    /// Index of the user message that starts the turn being split, or `-1`
    /// when not splitting.
    pub turn_start_index: i64,
    /// Whether this cut splits a turn (cut point is not a user message).
    pub is_split_turn: bool,
}

/// Find the cut point over PROJECTED entries that keeps approximately
/// `keep_recent_tokens` (upstream `findProjectedCutPoint`).
///
/// Only considers entries between `start_index` and `end_index` (exclusive).
/// The trailing-tool-results-over-budget fallback cuts at the LAST valid cut
/// point, not the first message; a closed suffix of an omitted recovery
/// attempt plus its context edits advances the cut one entry further.
pub fn find_projected_cut_point<E: ProjectedEntry>(
    entries: &[E],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points: Vec<usize> = (start_index..end_index)
        .filter(|index| entries[*index].is_cut_point())
        .collect();

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: -1,
            is_split_turn: false,
        };
    }

    // Walk backwards from newest, accumulating estimated sizes.
    let mut accumulated_tokens: u64 = 0;
    let mut exceeded_budget = false;
    let mut cut_index = cut_points[0];
    for index in (start_index..end_index).rev() {
        let entry = &entries[index];
        // Only conversation messages budget the retained tail. System messages
        // are prompt state: they are excluded from the summarized spans
        // (`span_messages`) and replayed by the checkpoint, so charging
        // their (declaration-heavy) size to `keep_recent_tokens` moved the cut
        // past a turn start and turned a plain history checkpoint into a
        // spurious split turn.
        let message_tokens: u64 = entry
            .messages()
            .iter()
            .filter(|message| !matches!(message, Message::System(_)))
            .map(estimate_message_tokens)
            .sum();
        if message_tokens == 0 {
            continue;
        }
        accumulated_tokens = accumulated_tokens.saturating_add(message_tokens);
        if accumulated_tokens >= keep_recent_tokens {
            exceeded_budget = true;
            // Prefer the closest valid cut point at or after this entry. If
            // trailing tool results exceed the budget by themselves, keep
            // their preceding assistant tool call instead of falling back to
            // the first message.
            cut_index = cut_points
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
                .unwrap_or(cut_points[cut_points.len() - 1]);
            break;
        }
    }

    // A recovery attempt and its omission edits are context-invisible after
    // the last visible input. Advance only for a closed suffix containing an
    // omitted assistant attempt; arbitrary metadata must not move the cut
    // past unsent input.
    let suffix = &entries[(cut_index + 1).min(end_index)..end_index];
    let is_intrinsically_visible =
        |entry: &E| !entry.is_context_edit() && entry.intrinsically_visible();
    let is_omitted = |entry: &E| is_intrinsically_visible(entry) && entry.messages().is_empty();
    let omitted_suffix_ids: std::collections::HashSet<&str> = suffix
        .iter()
        .filter(|entry| is_omitted(entry))
        .filter_map(|entry| entry.id())
        .collect();
    let has_external_replacement = suffix.iter().any(|entry| {
        entry.is_context_edit()
            && entry.has_replacement()
            && entry
                .context_edit_target_id()
                .is_none_or(|target| !omitted_suffix_ids.contains(target))
    });
    let is_recovery_omission_suffix = exceeded_budget
        && !has_external_replacement
        && suffix
            .iter()
            .any(|entry| entry.is_assistant() && is_omitted(entry))
        && suffix.iter().all(|entry| {
            !entry.is_compaction() && (!is_intrinsically_visible(entry) || is_omitted(entry))
        });
    if is_recovery_omission_suffix {
        cut_index += 1;
    }

    // Scan backwards from the cut to include adjacent metadata entries that do
    // not affect context.
    while cut_index > start_index {
        let previous = &entries[cut_index - 1];
        if previous.is_compaction() || !previous.messages().is_empty() {
            break;
        }
        cut_index -= 1;
    }

    let starts_turn = entries[cut_index].is_turn_start();
    let turn_start_index = if starts_turn {
        -1
    } else {
        find_projected_turn_start_index(entries, cut_index, start_index)
    };
    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index != -1,
    }
}

/// Find the context-visible turn-start entry at or before `entry_index`
/// (upstream `findProjectedTurnStartIndex`).
pub fn find_projected_turn_start_index<E: ProjectedEntry>(
    entries: &[E],
    entry_index: usize,
    start_index: usize,
) -> i64 {
    for index in (start_index..=entry_index.min(entries.len().saturating_sub(1))).rev() {
        if entries[index].is_turn_start() {
            return index as i64;
        }
    }
    -1
}

// ============================================================================
// Projected context estimation (upstream `estimateProjectedContextTokens`)
// ============================================================================

/// The leaf-path entry facts the projected estimate needs (upstream reads the
/// raw branch entries): an id and the two entry kinds that invalidate a stale
/// usage record. Implemented by [`FileEntry`] and by the borrowed metadata
/// twin [`PathEntryMeta`], so the session paths never clone the branch.
pub trait PathEntry {
    fn id(&self) -> Option<&str>;
    fn is_compaction(&self) -> bool;
    fn is_context_edit(&self) -> bool;
}

impl PathEntry for FileEntry {
    fn id(&self) -> Option<&str> {
        FileEntry::id(self)
    }
    fn is_compaction(&self) -> bool {
        matches!(self, FileEntry::Compaction(_))
    }
    fn is_context_edit(&self) -> bool {
        matches!(self, FileEntry::ContextEdit(_))
    }
}

/// Borrowed metadata of one leaf-path entry (see [`PathEntry`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathEntryMeta {
    pub id: Option<String>,
    pub is_compaction: bool,
    pub is_context_edit: bool,
}

impl PathEntry for PathEntryMeta {
    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    fn is_compaction(&self) -> bool {
        self.is_compaction
    }
    fn is_context_edit(&self) -> bool {
        self.is_context_edit
    }
}

/// Estimate projected context without trusting usage captured before a later
/// edit or compaction (upstream `estimateProjectedContextTokens`).
///
/// `branch_entries` is the raw leaf path the projection was built from: when
/// the last assistant usage lies at or before the newest compaction/context
/// edit, the usage is stale and the estimate falls back to per-message
/// character estimation (including the current system message).
pub fn estimate_projected_context_tokens<E: ProjectedEntry, P: PathEntry>(
    projected: &[E],
    branch_entries: &[P],
) -> ContextUsageEstimate {
    estimate_projected_context_tokens_with_tail(projected, &[], branch_entries)
}

/// [`estimate_projected_context_tokens`] over a projection PLUS the live
/// context tail that is not yet part of the projection (the current run's
/// messages after the prompt-start snapshot): the borrowed twin the threshold
/// hook uses, so the threshold check costs no message/entry clone and the
/// materialization ([`materialize_projected_tail`]) is deferred until the
/// check actually decides to compact.
///
/// Equivalent to calling [`estimate_projected_context_tokens`] on
/// `projected` extended with one synthetic message entry per tail message (the
/// shape [`materialize_projected_tail`] builds): a usage message inside the
/// tail maps to no entry id (`None`), exactly like the synthetic entries'
/// [`CutEntry::id`].
pub fn estimate_projected_context_tokens_with_tail<E: ProjectedEntry, P: PathEntry>(
    projected: &[E],
    tail: &[Message],
    branch_entries: &[P],
) -> ContextUsageEstimate {
    // The virtual concatenation, as borrowed references (no message clone).
    let mut refs: Vec<&Message> = Vec::with_capacity(
        projected
            .iter()
            .map(|entry| entry.messages().len())
            .sum::<usize>()
            + tail.len(),
    );
    for entry in projected {
        refs.extend(entry.messages().iter());
    }
    refs.extend(tail.iter());
    let estimate = estimate_context_tokens_over(refs.as_slice());
    if let Some(usage_index) = estimate.last_usage_index {
        // Map the usage message back to its projected entry id. A usage inside
        // the tail walks past every projected entry and stays `None`.
        let mut message_index = 0usize;
        let mut usage_entry_id: Option<&str> = None;
        for entry in projected {
            let next = message_index + entry.messages().len();
            if usage_index < next {
                usage_entry_id = entry.id();
                break;
            }
            message_index = next;
        }

        let usage_entry_index = usage_entry_id.and_then(|id| {
            branch_entries
                .iter()
                .position(|entry| entry.id() == Some(id))
        });
        let latest_invalidating_index = branch_entries
            .iter()
            .rposition(|entry| entry.is_context_edit() || entry.is_compaction());
        let usage_index_i64 = usage_entry_index.map_or(-1, |index| index as i64);
        let latest_i64 = latest_invalidating_index.map_or(-1, |index| index as i64);
        if usage_index_i64 > latest_i64 {
            return estimate;
        }
    }

    // Stale (or no) usage: estimate every message, plus the current system
    // message, by character count.
    let mut tokens = current_system_chars(projected, tail).map_or(0, ceil_div_chars);
    for message in &refs {
        if !matches!(message, Message::System(_)) {
            tokens = tokens.saturating_add(estimate_message_tokens(message));
        }
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

/// Character count of the CURRENT system message over the projection + tail
/// sequence (the last `Message::System`): its text blocks joined with `"\n"`,
/// exactly the text
/// `session_manager::projection::current_system_message` builds from a
/// materialized slice. The materializing twin needs the owned slice this
/// borrowed estimator avoids building, so the join is reproduced here (only
/// the character count is consumed).
fn current_system_chars<E: ProjectedEntry>(projected: &[E], tail: &[Message]) -> Option<usize> {
    let mut system: Option<&Message> = None;
    for message in projected
        .iter()
        .flat_map(|entry| entry.messages().iter())
        .chain(tail)
    {
        if matches!(message, Message::System(_)) {
            system = Some(message);
        }
    }
    system.map(|message| match message {
        Message::System(user) => user
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .count(),
        _ => 0,
    })
}

// ============================================================================
// Deferred projection materialization (threshold-hook building block)
// ============================================================================

thread_local! {
    /// Per-thread count of projection materializations (see
    /// [`projection_materializations`]). Thread-local so tests running in
    /// parallel in one process cannot observe each other's counts.
    static PROJECTION_MATERIALIZATIONS: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// The DEFERRED heavy step of the session threshold hook: materialize the
/// projection basis that cut-point selection and preparation need — the
/// snapshot entries deep-cloned plus the not-yet-projected context tail as
/// synthetic message entries ([`CutEntry::from_message`]).
///
/// The threshold check itself runs on the borrowed basis
/// ([`estimate_projected_context_tokens_with_tail`], plus
/// [`should_compact_tokens`]); only a positive decision pays for this copy.
/// Before the deferral the hook built this vector on EVERY provider request,
/// cloning the whole session projection and branch just to test the
/// threshold.
pub fn materialize_projected_tail(entries: &[CutEntry], tail: &[Message]) -> Vec<CutEntry> {
    PROJECTION_MATERIALIZATIONS.with(|counter| counter.set(counter.get().saturating_add(1)));
    let mut materialized = entries.to_vec();
    materialized.extend(tail.iter().cloned().map(CutEntry::from_message));
    materialized
}

/// Test/profiling probe (`#[doc(hidden)]`): how many projection
/// materializations ([`materialize_projected_tail`]) this thread performed.
/// Pins the deferral: a sub-threshold provider request must not materialize
/// anything, a threshold-triggering one materializes once.
#[doc(hidden)]
pub fn projection_materializations() -> u64 {
    PROJECTION_MATERIALIZATIONS.with(std::cell::Cell::get)
}

/// Reset this thread's [`projection_materializations`] counter (the probe's
/// starting point).
#[doc(hidden)]
pub fn reset_projection_materializations() {
    PROJECTION_MATERIALIZATIONS.with(|counter| counter.set(0));
}
