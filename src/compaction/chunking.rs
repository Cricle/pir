//! Bounded (chunked) summarization for spans that do not fit the model's
//! context window — a **pir extension**; upstream always issues ONE
//! summarizer call over the whole span (`generateSummary` /
//! `generateBranchSummary`).
//!
//! Why: compaction's history span has no token budget of its own (upstream
//! relies on the trigger, so the span is bounded by the whole window), which
//! on a real gateway means a huge session is summarized by a single request
//! far above the model's window — rejected with `context_length_exceeded` —
//! and locally it materializes the whole serialized conversation (a ~100 MB
//! session produced a ~78 MB request and a `String` copy of the same size).
//!
//! The split here is deliberately conservative and turn-aware:
//!
//! - a chunk's weight is the **exact serialized length** of the text
//!   [`serialize_conversation`](super::serialize_conversation) would produce
//!   for it (so the request the summarizer sends is bounded by construction,
//!   not by an estimate that ignores the role labels);
//! - a chunk never *opens* on a tool result: the trailing tool results of the
//!   last accepted assistant tool call stay in the same chunk (a turn/tool
//!   pair is never split at its seam);
//! - when a turn boundary (user message) exists inside the accepted span the
//!   cut moves to it, so chunks end at turn boundaries where possible;
//! - a single message that alone exceeds the whole cap is kept whole and
//!   truncated at prompt-build time with the standard
//!   "[... N more characters truncated]" marker
//!   ([`write_chunk`](super::write_chunk)); the session keeps the
//!   untruncated message — the summary is lossy by definition.
//!
//! The chunk list is a set of **borrowed ranges** ([`SummarizationChunk`]
//! holds indices, not messages), so splitting a 100 MB span allocates a few
//! `usize` per message, never a copy of the conversation.

use crate::ai::types::{Message, Model};

use super::algorithm::{estimate_message_tokens, MessageSequence, CHARS_PER_TOKEN};

/// Context window assumed when the model declares none (upstream
/// `model.contextWindow || 128000`).
pub const FALLBACK_CONTEXT_WINDOW: u64 = 128_000;

/// Share of the usable window (`window - reserve`) ONE chunk's serialized
/// conversation may carry. The remaining quarter holds the framing prompt,
/// the running summary and the response cap, so a chunked request stays
/// inside the window even when the model's tokenizer is denser than the
/// chars/4 estimate.
const CHUNK_WINDOW_NUMERATOR: u64 = 3;
const CHUNK_WINDOW_DENOMINATOR: u64 = 4;

/// Size below which the `3/4 x (window - reserve)` target is NOT raised: the
/// chunk budget is **window-relative, never absolute**.
///
/// The earlier shape used this as a hard floor, which let a chunk exceed the
/// model window: a 1 000-token window with a 400-token reserve produced a
/// 1 024-token chunk (4 096 chars) — bigger than the window it was supposed to
/// summarize — so the *branch* path stopped chunking a span that cannot be
/// sent in one request. Progress is guaranteed by the splitter itself (every
/// chunk holds at least one message), so a positive window-relative budget is
/// all that is needed; a degenerate window (usable = 0) degrades to tiny
/// chunks instead of one request the provider would reject.
pub const MIN_CHUNK_BUDGET_TOKENS: u64 = 1_024;

/// Separator [`serialize_conversation`](super::serialize_conversation) joins
/// message parts with (`"\n\n"`), counted in every weight.
const PART_SEPARATOR_CHARS: usize = 2;

/// One summarizer chunk: a borrowed range of the span's message slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarizationChunk {
    /// First message index (inclusive) in the split slice.
    pub start: usize,
    /// Last message index (exclusive).
    pub end: usize,
    /// Exact serialized length (characters) of `[start, end)`.
    pub chars: usize,
    /// The chunk holds a single message that alone exceeds the cap: its
    /// serialized text is truncated to the cap with the standard marker.
    pub oversize: bool,
}

impl SummarizationChunk {
    /// Number of messages in the chunk.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Whether the chunk carries no message (never produced by the splitter).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The chunk's message range as a slice index.
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

/// Token and character form of the per-chunk budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarizationChunkBudget {
    /// Chunk weight in estimated tokens (`chars / 4`).
    pub tokens: u64,
    /// Chunk weight in serialized characters.
    pub chars: usize,
}

/// The per-chunk budget for `model` under `reserve_tokens` (the compaction
/// `reserveTokens`): `3/4 x (window - reserve)`, and at least 1 token.
///
/// INVARIANT (window-relative, never absolute): the budget is ≤ the usable
/// window, so a chunk (and therefore a summarizer request, which adds the
/// framing prompt plus the running summary) can never be heavier than the
/// window it summarizes. `MIN_CHUNK_BUDGET_TOKENS` documents the size below
/// which a NORMAL window is not raised; it is deliberately NOT applied as a
/// floor (a 1 024-token floor used to produce a 4 096-char chunk for a
/// 1 000-token window, which stopped the branch path from chunking a span that
/// cannot be sent in one request).
///
/// A zero `model.context_window` falls back to [`FALLBACK_CONTEXT_WINDOW`] like
/// every other window read in this module.
pub fn summarization_chunk_budget(model: &Model, reserve_tokens: u64) -> SummarizationChunkBudget {
    let window = if model.context_window > 0 {
        model.context_window
    } else {
        FALLBACK_CONTEXT_WINDOW
    };
    let usable = window.saturating_sub(reserve_tokens);
    // WINDOW-RELATIVE cap only (`MIN_CHUNK_BUDGET_TOKENS` is the documented
    // minimum for a NORMAL window, not an absolute floor — see its docs): a
    // floor above `3/4 x usable` would let a chunk exceed the window, which is
    // exactly the bug the branch-summarization budget test caught.
    let tokens = (usable.saturating_mul(CHUNK_WINDOW_NUMERATOR) / CHUNK_WINDOW_DENOMINATOR).max(1);
    SummarizationChunkBudget {
        tokens,
        chars: (tokens as f64 * CHARS_PER_TOKEN) as usize,
    }
}

/// Estimated tokens of one chunk (its serialized weight under the module's
/// chars/4 estimator) — the number to compare against
/// [`SummarizationChunkBudget::tokens`] and the model window.
pub fn chunk_estimated_tokens(chunk: &SummarizationChunk) -> u64 {
    (chunk.chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

/// Estimated tokens of a whole span under the same chars/4 weighting the
/// compaction trigger uses (the "does this span fit" answer without the
/// split).
pub fn span_estimated_tokens<M: MessageSequence + ?Sized>(messages: &M) -> u64 {
    (0..messages.message_count())
        .map(|index| estimate_message_tokens(messages.message_at(index)))
        .sum()
}

/// Whether a message opens a turn (upstream `isTurnStartMessage`).
fn is_turn_start(message: &Message) -> bool {
    matches!(message, Message::User(_))
}

/// Serialized characters of a message list under the streamer's per-message
/// accounting (each message's parts plus one `"\n\n"` between messages) — the
/// same measure [`StreamingChunker::window_chars`] maintains incrementally.
/// Used to re-derive the closed chunk's weight after a turn-boundary cut.
fn serialized_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| super::message_parts_chars(message).0)
        .sum::<usize>()
        + PART_SEPARATOR_CHARS * messages.len().saturating_sub(1)
}

/// Prefix sums over the serialized parts, so any range's exact serialized
/// length is O(1) and the splitter never re-serializes the span.
///
/// MEMORY SHAPE (pir extension): the weights are computed **one message at a
/// time** ([`ChunkWeights::new`] serializes a message, records its counts, and
/// drops the text) — the former shape collected `Vec<Vec<String>>` for the
/// whole span first, which materialized the entire serialized conversation
/// (tens of megabytes on a huge session) just to weight it.
struct ChunkWeights {
    /// Serialized character length of the parts each message contributes.
    chars_per_message: Vec<usize>,
    /// `chars_per_message[0..i]` summed.
    prefix_chars: Vec<usize>,
    /// Number of serialized parts in `[0, i)` (an assistant message with text
    /// AND tool calls contributes two, joined by the separator).
    prefix_parts: Vec<usize>,
}

impl ChunkWeights {
    fn new<M: MessageSequence + ?Sized>(messages: &M) -> Self {
        let count = messages.message_count();
        let mut chars_per_message = Vec::with_capacity(count);
        let mut prefix_chars = Vec::with_capacity(count + 1);
        let mut prefix_parts = Vec::with_capacity(count + 1);
        prefix_chars.push(0usize);
        prefix_parts.push(0usize);
        for index in 0..count {
            // Count ONE message's serialized weight without materializing its
            // text: the peak is a handful of counters, never a copy of the
            // message (a 256 KiB tool result used to be copied and truncated
            // here just to learn its 2 015-character serialized length).
            let (chars, part_count) = super::message_parts_chars(messages.message_at(index));
            chars_per_message.push(chars);
            prefix_chars.push(prefix_chars[prefix_chars.len() - 1] + chars);
            prefix_parts.push(prefix_parts[prefix_parts.len() - 1] + part_count);
        }
        Self {
            chars_per_message,
            prefix_chars,
            prefix_parts,
        }
    }

    /// Exact serialized length of `[start, end)`: the parts plus the `"\n\n"`
    /// separators (a single part carries none).
    fn range_chars(&self, start: usize, end: usize) -> usize {
        let chars = self.prefix_chars[end].saturating_sub(self.prefix_chars[start]);
        let parts = self.prefix_parts[end].saturating_sub(self.prefix_parts[start]);
        chars + PART_SEPARATOR_CHARS * parts.saturating_sub(1)
    }

    /// Serialized length of the parts one message contributes.
    fn message_chars(&self, index: usize) -> usize {
        self.chars_per_message[index]
    }
}

// ============================================================================
// Streaming chunked summarization (the paged/disk-backed span path)
// ============================================================================

/// Chunked summarization that consumes messages ONE AT A TIME and holds at
/// most one chunk's messages (bounded by the chunk budget), so a span that
/// lives on disk (a paged session's skipped prefix) never becomes resident.
///
/// The chunk boundaries reproduce [`split_summarization_chunks`]'s rules
/// exactly, evaluated incrementally:
///
/// - greedy fit under the serialized cap;
/// - a chunk never OPENS on a tool result (the trailing tool results of the
///   last accepted assistant call are carried into the closing chunk, bounded
///   by one extra cap);
/// - the cut prefers the last turn boundary inside the accepted span;
/// - a single message over the whole cap stays alone and is truncated at
///   serialize time.
///
/// The caller pushes projected messages in span order and calls
/// [`Self::finish`]; each closed chunk is summarized immediately (chunk 0 with
/// the style's opening prompt, later chunks folding into the running summary).
pub struct StreamingChunker<'a> {
    complete: std::sync::Arc<super::CompleteFn>,
    framing: super::SpanFraming<'a>,
    /// The `/compact` focus appended to EVERY summarizer prompt
    /// (`Additional focus:`), exactly like the resident chunked path. `None`
    /// keeps the prompts byte-identical to the pre-focus shape.
    custom_instructions: Option<&'a str>,
    budget: SummarizationChunkBudget,
    window: Vec<Message>,
    /// Reused serialization buffer for the closing chunk: its capacity survives
    /// the whole run, so a 247-chunk span does not allocate (and retain) a
    /// fresh chunk-sized string per call.
    chunk_text: String,
    /// Reused prompt buffer; the finished prompt is MOVED into the request.
    prompt_text: String,
    /// Serialized characters of `window` including the `"\n\n"` separators.
    window_chars: usize,
    /// Characters of trailing tool results carried beyond the fit.
    carried: usize,
    running: Option<String>,
    total_usage: Option<super::Usage>,
    chunks_emitted: usize,
    finished: bool,
    /// A chunk reported a TERMINAL abort: [`Self::finish`] would surface the
    /// aborted outcome instead of a summary. Never set in practice: the
    /// streaming path passes `abort_is_terminal = false` to
    /// `finish_summarized`, so an `Aborted` stop reason is accepted as a
    /// partial response through the standard failure guards — the
    /// `History`/`TurnPrefix` semantics (upstream checks the abort first only
    /// in the branch summarizer, which does not stream).
    aborted: bool,
}

impl<'a> StreamingChunker<'a> {
    /// `previous_summary` seeds the running summary (the newest compaction
    /// before the streamed span, when there is one).
    pub fn new(
        model: &Model,
        reserve_tokens: u64,
        framing: super::SpanFraming<'a>,
        previous_summary: Option<&str>,
        complete: std::sync::Arc<super::CompleteFn>,
    ) -> Self {
        let budget = summarization_chunk_budget(model, reserve_tokens);
        Self {
            complete,
            framing,
            custom_instructions: None,
            budget,
            window: Vec::new(),
            chunk_text: String::new(),
            prompt_text: String::new(),
            window_chars: 0,
            carried: 0,
            running: previous_summary.map(str::to_owned),
            total_usage: None,
            chunks_emitted: 0,
            finished: false,
            aborted: false,
        }
    }

    /// Attach the `/compact` focus the user passed (`/compact <instructions>`):
    /// it is appended as `Additional focus:` to the opening prompt AND to every
    /// folding prompt, so a later chunk cannot silently drop it (the resident
    /// chunked path passes it on every call too).
    pub fn with_custom_instructions(mut self, custom_instructions: Option<&'a str>) -> Self {
        self.custom_instructions = custom_instructions;
        self
    }

    /// The per-chunk serialized cap this streamer closes chunks at.
    pub fn budget_chars(&self) -> usize {
        self.budget.chars
    }

    /// How many summarizer calls have been made so far.
    pub fn chunks_emitted(&self) -> usize {
        self.chunks_emitted
    }

    /// Push one projected message (span order). Closes and summarizes the
    /// current chunk when the message does not fit.
    pub async fn push(&mut self, message: &Message) -> Result<(), String> {
        // Borrowed callers keep their copy; `push_owned` is the no-copy path.
        self.push_owned(message.clone()).await
    }

    /// [`Self::push`] for a message the caller already owns (the paged prefix
    /// reader parses one entry at a time): the window takes the message by
    /// move instead of deep-cloning a tool result's file contents into a
    /// second copy. The chunking decision itself is unchanged.
    pub async fn push_owned(&mut self, message: Message) -> Result<(), String> {
        if self.finished {
            return Err(format!(
                "{} failed: message pushed after the stream finished",
                self.framing.label()
            ));
        }
        // Count-only weight: the message is serialized for real only if it
        // lands in a chunk that closes (the paged prefix reader parses one
        // entry at a time; weighting used to copy every parsed tool result).
        let (weight, _) = super::message_parts_chars(&message);
        let separator = if self.window.is_empty() {
            0
        } else {
            PART_SEPARATOR_CHARS
        };

        if !self.window.is_empty() && self.window_chars + separator + weight > self.budget.chars {
            let window_oversize = self.window.len() == 1 && self.window_chars > self.budget.chars;
            let carry = matches!(message, Message::ToolResult(_))
                && !window_oversize
                && self.carried < self.budget.chars;
            if carry {
                // The closing chunk keeps the trailing tool results of its last
                // accepted assistant call (bounded by one extra cap).
                self.window_chars += separator + weight;
                self.carried += separator + weight;
                self.window.push(message);
                return Ok(());
            }
            self.emit_chunk(false).await?;
        }
        self.window_chars += if self.window.is_empty() {
            weight
        } else {
            PART_SEPARATOR_CHARS + weight
        };
        self.window.push(message);
        Ok(())
    }

    /// Close the last chunk and return the consolidated summary.
    ///
    /// INVARIANT (drain to empty): a turn-boundary cut inside the closing
    /// window (`emit_chunk`'s remainder) leaves messages that belong to a chunk
    /// of their own — exactly like the resident splitter, which continues its
    /// `start = cut` loop. Emitting only once silently DROPPED that remainder
    /// (a span whose tail contains a turn start lost its last messages and one
    /// summarizer call). The remainder shrinks by at least one message per
    /// iteration (the cut never lands before index 1), so the loop terminates.
    pub async fn finish(mut self) -> Result<super::SummarizedSpan, String> {
        // The closing window is the TAIL of the span: no turn-boundary cut (see
        // `split_summarization_chunks`) — it would split a fitting span into a
        // second call for the tail, which is exactly what the resident splitter
        // no longer does (the two must agree, see the equivalence test).
        if !self.window.is_empty() {
            self.emit_chunk(true).await?;
        }
        self.finished = true;
        Ok(super::SummarizedSpan {
            text: self.running.take().unwrap_or_default(),
            usage: self.total_usage.take().unwrap_or_default(),
            aborted: self.aborted,
        })
    }

    /// Close the window into a chunk, cut back to the last turn boundary when
    /// one exists inside the accepted span, and issue one summarizer call.
    ///
    /// `final_chunk` marks the span's TAIL: no boundary cut is applied there
    /// (nothing follows it), matching [`split_summarization_chunks`].
    async fn emit_chunk(&mut self, final_chunk: bool) -> Result<(), String> {
        let oversize = self.window.len() == 1 && self.window_chars > self.budget.chars;
        // Prefer a turn boundary: the LAST user message inside the accepted
        // span ends this chunk and opens the next one (skipped when tool
        // results were carried, for the indivisible case, and for the tail).
        let mut remainder: Vec<Message> = Vec::new();
        let mut remainder_chars = 0usize;
        if self.carried == 0 && !oversize && !final_chunk && self.window.len() > 1 {
            let cut = self.window[1..]
                .iter()
                .rposition(is_turn_start)
                .map(|relative| relative + 1);
            if let Some(cut) = cut {
                remainder = self.window.split_off(cut);
                remainder_chars = serialized_chars(&remainder);
                // The chunk constructed below is the POST-split window (`end`
                // is its new length): drop the remainder's serialized weight
                // plus the separator that joined it, so `chars` is the closed
                // chunk's own weight — exactly what the resident splitter
                // records for its post-cut range (only the oversize warning
                // reads this number, but the two paths must not disagree).
                self.window_chars = self
                    .window_chars
                    .saturating_sub(remainder_chars + PART_SEPARATOR_CHARS);
            }
        }
        let chunk = super::chunking::SummarizationChunk {
            start: 0,
            end: self.window.len(),
            chars: self.window_chars,
            oversize,
        };
        let mut chunk_text = std::mem::take(&mut self.chunk_text);
        chunk_text.clear();
        super::write_chunk(
            &mut chunk_text,
            self.window.as_slice(),
            &chunk,
            self.budget.chars,
        );
        let label = self.framing.label();
        let mut prompt_text = std::mem::take(&mut self.prompt_text);
        prompt_text.clear();
        if self.chunks_emitted == 0 {
            self.framing.write_opening_prompt(
                &mut prompt_text,
                &chunk_text,
                self.running.as_deref(),
                self.custom_instructions,
            );
        } else {
            self.framing.write_folding_prompt(
                &mut prompt_text,
                &chunk_text,
                self.running.as_deref(),
                self.custom_instructions,
            );
        }
        // The chunk text's capacity is kept for the next call; the prompt
        // buffer is MOVED into the request context below (no copy of it).
        self.chunk_text = chunk_text;
        tracing::debug!(
            chunk = self.chunks_emitted + 1,
            chars = self.window_chars,
            label,
            "streamed summarizer call"
        );
        let response = super::complete_summarization_with_retry(
            &self.complete,
            super::summarizer_context_owned(prompt_text),
        )
        .await
        .map_err(|error| {
            format!(
                "{label} failed (chunk {}, no checkpoint persisted): {error}",
                self.chunks_emitted + 1
            )
        })?;
        // History/TurnPrefix accept an aborted partial through the standard
        // failure guards (upstream checks the abort first only in the branch
        // summarizer, which never streams), so this call's abort is not a
        // terminal outcome.
        let summarized = super::finish_summarized(response, label, false)?;
        if summarized.aborted {
            // A terminal abort (a framing that opts into one) is reported
            // through `finish()` (never as an error): the caller maps
            // `aborted` to the upstream cancelled outcome. Unreachable for the
            // History framing this type is built with today; see the field.
            self.aborted = true;
            return Ok(());
        }
        self.chunks_emitted += 1;
        self.running = Some(summarized.text);
        self.total_usage = Some(match self.total_usage.take() {
            Some(accumulated) => super::combine_usage(accumulated, summarized.usage),
            None => summarized.usage,
        });
        self.window = remainder;
        self.carried = 0;
        self.window_chars = remainder_chars;
        Ok(())
    }
}

/// Split a summarization span into chunks no heavier than `max_chars` of
/// serialized text (see the module docs for the boundary rules). The span is
/// weighted one message at a time (see [`ChunkWeights::new`]) — the peak is
/// one message's text, never the span's.
///
/// `max_chars == 0` is treated as 1 so the splitter always makes progress.
pub fn split_summarization_chunks<M: MessageSequence + ?Sized>(
    messages: &M,
    max_chars: usize,
) -> Vec<SummarizationChunk> {
    let len = messages.message_count();
    if len == 0 {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let weights = ChunkWeights::new(messages);

    let mut chunks: Vec<SummarizationChunk> = Vec::new();
    let mut start = 0usize;
    while start < len {
        // Greedy fit: the first message always enters (even when it alone is
        // over the cap — that is the indivisible case).
        let mut end = start + 1;
        while end < len && weights.range_chars(start, end + 1) <= max_chars {
            end += 1;
        }
        let oversize = weights.range_chars(start, end) > max_chars;

        // A chunk must never OPEN on a tool result: the trailing tool results
        // of the last accepted assistant tool call stay with it. Bounded by
        // one extra cap worth of results (each result is truncated to
        // `TOOL_RESULT_MAX_CHARS` in the serialized text), so a pathological
        // transcript cannot make a chunk unbounded.
        let mut carried = 0usize;
        while !oversize
            && end < len
            && matches!(messages.message_at(end), Message::ToolResult(_))
            && carried < max_chars
        {
            carried += weights.message_chars(end) + PART_SEPARATOR_CHARS;
            end += 1;
        }

        // Prefer a turn boundary: when a user message (a turn start) exists
        // inside the accepted span, the chunk ends right before it so the next
        // chunk opens a fresh turn. Skipped when tool results were carried
        // (they must stay attached), for the indivisible case, and for the
        // LAST chunk (`end == len`): the boundary preference exists to keep the
        // NEXT chunk from opening mid-turn, and at the end of the span there is
        // no next chunk — cutting there would split a span that fits into two
        // calls (upstream sends one, and the "a fitting span is one call" rule
        // is part of this module's contract).
        let mut cut = end;
        if carried == 0 && !oversize && end < len {
            for candidate in (start + 1..end).rev() {
                if is_turn_start(messages.message_at(candidate)) {
                    cut = candidate;
                    break;
                }
            }
        }
        if cut <= start {
            cut = end;
        }
        let chars = weights.range_chars(start, cut);
        chunks.push(SummarizationChunk {
            start,
            end: cut,
            chars,
            oversize: oversize && cut == end,
        });
        start = cut;
    }
    chunks
}
