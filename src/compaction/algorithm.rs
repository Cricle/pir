//! Pure compaction algorithms: token estimation, trigger predicate, and cut
//! point selection over projected session entries.
//!
//! Rust port of upstream `packages/coding-agent/src/core/compaction/compaction.ts`
//! (`calculateContextTokens`, `estimateContextTokens`, `estimateTokens`,
//! `estimateProjectedContextTokens`, `shouldCompact`, `findProjectedCutPoint`).
//! TOKEN ESTIMATION (pir extension): upstream's `chars / 4` heuristic
//! severely under-estimates CJK transcripts — a Han character is roughly one
//! token, not a quarter token. Every production estimator in this module (and
//! its callers: compaction trigger, chunking, prompt/max_tokens clamps,
//! `context_usage`, cost projection) now runs through the SAME Unicode-aware
//! unit counter below:
//!
//! - ASCII: 1 unit/char (4 chars per token);
//! - CJK (Han, Hiragana, Katakana, Hangul): 4 units/char (≈1 token/char);
//! - emoji: 8 units/char (≈2 tokens/char), other symbols: 4 units/char;
//! - other non-ASCII (Cyrillic, Greek, accented Latin, …): 2 units/char.
//!
//! Units are quarter-tokens, so `tokens = ceil(units / 4)`. The conservative
//! path divides by 3.5 instead, which keeps the old `estimate.ts` safety
//! margin for every character class (and is never below the normal estimate).
//! This stays dependency-free (no tokenizer crate required offline).
//!
//! The cut point walks *projected* entries so omitted (context-edited)
//! entries and the newest-compaction-only rule are respected.

use serde::{Deserialize, Serialize};

use crate::ai::types::{ContentBlock, Context, Message, Model, StopReason, Tool, Usage};
use crate::session_manager::{FileEntry, StoredMessage};

/// Estimated character weight of an image content block (upstream
/// `ESTIMATED_IMAGE_CHARS`). Images are charged as if they were this many
/// ASCII characters.
pub const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// Legacy chars-per-token constant, retained for API compatibility. New code
/// must use [`estimate_text_tokens`] / [`estimate_message_tokens`]: plain
/// character counts are not token counts for CJK content.
pub const CHARS_PER_TOKEN: f64 = 4.0;

/// Quarter-token units in an ASCII character (4 chars/token).
const ASCII_UNITS: u64 = 1;
/// Quarter-token units in a CJK character (≈1 token/char).
const CJK_UNITS: u64 = 4;
/// Quarter-token units in a non-CJK non-ASCII letter (≈0.5 token/char).
const OTHER_UNITS: u64 = 2;
/// Quarter-token units in a symbol (≈1 token/char).
const SYMBOL_UNITS: u64 = 4;
/// Quarter-token units in an emoji / pictograph (≈2 tokens/char).
const EMOJI_UNITS: u64 = 8;

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

impl CompactionConfig {
    /// The keep-recent budget actually usable for a cut on `context_window`.
    ///
    /// The upstream default (20 000) can exceed the whole window of a small
    /// model (e.g. 8 192 or 2 000 tokens). `find_projected_cut_point` only
    /// exhausts its budget when the transcript itself exceeds
    /// `keep_recent_tokens`, so such a session always reported "nothing to
    /// compact" even while its request was already over the window. Capping
    /// the retained tail at half the window guarantees a cut becomes possible
    /// while leaving the other half for the summary, the prompt/tool
    /// declarations, and the response reserve.
    pub fn effective_keep_recent_tokens(&self, context_window: u64) -> u64 {
        self.keep_recent_tokens
            .min((context_window.max(1) / 2).max(1))
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

/// Whether `ch` belongs to a CJK script (Han, Hiragana, Katakana, Hangul and
/// their punctuation/compatibility blocks).
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1100..=0x11FF // Hangul Jamo
            | 0x2E80..=0x2FFF // CJK radicals / Kangxi
            | 0x3000..=0x303F // CJK symbols and punctuation
            | 0x3040..=0x309F // Hiragana
            | 0x30A0..=0x30FF // Katakana
            | 0x3130..=0x318F // Hangul compatibility Jamo
            | 0x31F0..=0x31FF // Katakana phonetic extensions
            | 0x3400..=0x4DBF // CJK Extension A
            | 0x4E00..=0x9FFF // CJK Unified Ideographs
            | 0xA960..=0xA97F // Hangul Jamo Extended-A
            | 0xAC00..=0xD7AF // Hangul Syllables
            | 0xD7B0..=0xD7FF // Hangul Jamo Extended-B
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFE30..=0xFE4F // CJK compatibility forms
            | 0xFF00..=0xFFEF // Halfwidth/fullwidth forms
            | 0x1AFF0..=0x1B16F // Kana supplement / extended
            | 0x20000..=0x2FA1F // CJK Extensions B–F
            | 0x30000..=0x3134F // CJK Extension G
    )
}

/// Whether `ch` is an emoji / pictographic symbol (charged at [`EMOJI_UNITS`]).
fn is_emoji(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF // emoji, pictographs, regional indicators
            | 0x2600..=0x27BF // misc symbols / dingbats
    )
}

/// Whether `ch` is a non-emoji symbol/punctuation outside ASCII
/// (charged at [`SYMBOL_UNITS`]).
fn is_symbol(ch: char) -> bool {
    if ch.is_ascii() {
        return false;
    }
    if is_emoji(ch) || is_cjk(ch) {
        return false;
    }
    matches!(
        ch as u32,
        0x2000..=0x206F // general punctuation
            | 0x2070..=0x209F // super/subscripts
            | 0x20A0..=0x20CF // currency symbols
            | 0x2100..=0x214F // letterlike symbols
            | 0x2150..=0x218F // number forms
            | 0x2190..=0x21FF // arrows
            | 0x2200..=0x22FF // mathematical operators
            | 0x2300..=0x23FF // misc technical
            | 0x2460..=0x24FF // enclosed alphanumerics
            | 0x2500..=0x259F // box drawing / block elements
            | 0x25A0..=0x25FF // geometric shapes
            | 0x2900..=0x297F // supplemental arrows-B
    )
}

/// Quarter-token units of one character (see the module docs).
pub fn char_token_units(ch: char) -> u64 {
    if ch.is_ascii() {
        ASCII_UNITS
    } else if is_cjk(ch) {
        CJK_UNITS
    } else if is_emoji(ch) {
        EMOJI_UNITS
    } else if is_symbol(ch) {
        SYMBOL_UNITS
    } else {
        OTHER_UNITS
    }
}

/// Quarter-token weight of `text` under the shared Unicode-aware estimator.
/// Zero-width joiner/variation selectors contribute nothing (they ride along
/// with the emoji that owns them).
///
/// ASCII fast path: every ASCII char is [`ASCII_UNITS`] (1) and ASCII chars
/// equal bytes, so the sum is just the byte length. `str::is_ascii` is
/// word/SIMD-vectorised, and this function is the per-byte inner loop of
/// every estimate the compaction trigger, cut-point selection and cost
/// projection run per turn (measured on the endurance script: pure-ASCII
/// transcripts dominate, and the char `match` costs ~10x the vectorised
/// scan). Non-ASCII text takes the exact same per-char path as before —
/// the returned value is identical for every input.
pub fn text_token_units(text: &str) -> u64 {
    if text.is_ascii() {
        return text.len() as u64;
    }
    text.chars()
        .map(|ch| match ch as u32 {
            0x200D | 0xFE0E | 0xFE0F => 0,
            _ => char_token_units(ch),
        })
        .sum()
}

/// Normal (4 chars/token for ASCII) token estimate of `text`.
pub fn estimate_text_tokens(text: &str) -> u64 {
    ceil_units(text_token_units(text))
}

/// Conservative (`3.5 chars/token` for ASCII) token estimate of `text`.
pub fn estimate_text_tokens_conservative(text: &str) -> u64 {
    ceil_units_conservative(text_token_units(text))
}

fn ceil_units(units: u64) -> u64 {
    units.div_ceil(4)
}

fn ceil_units_conservative(units: u64) -> u64 {
    (units as f64 / 3.5).ceil() as u64
}

/// Quarter-token weight of user-side content (upstream
/// `estimateTextAndImageContentChars`): text blocks contribute their weighted
/// length, image blocks a fixed [`ESTIMATED_IMAGE_CHARS`] ASCII characters,
/// everything else nothing.
fn estimate_text_and_image_units(content: &[ContentBlock]) -> u64 {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text_token_units(&text.text),
            ContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS as u64,
            _ => 0,
        })
        .sum()
}

/// Quarter-token weight of the request's leading prompt state: the prompt text
/// plus the JSON-serialized tool declarations.
///
/// `Context.system_prompt` and `Context.tools` live outside `messages` in pir,
/// while upstream folds them into a leading system message before the provider
/// sees the context. Callers that estimate a request must add this prefix
/// exactly when the message list does not already carry a usage record (a
/// usage record includes the leading prompt).
fn prompt_token_units(
    system_prompt: Option<&str>,
    tools: Option<&[crate::ai::types::Tool]>,
) -> u64 {
    let prompt_units = system_prompt.map_or(0, text_token_units);
    prompt_units.saturating_add(tool_declaration_units(tools))
}

/// Quarter-token weight of a tool declaration list, JSON-serialized exactly as
/// the request's `tools[]` field carries it.
fn tool_declaration_units(tools: Option<&[crate::ai::types::Tool]>) -> u64 {
    tools
        .filter(|tools| !tools.is_empty())
        .and_then(|tools| serde_json::to_string(tools).ok())
        .map_or(0, |json| text_token_units(&json))
}

/// Whether the newest system message already carries `prompt` verbatim in its
/// text blocks or its named prompt sections.
///
/// pir keeps the prompt in `Context::system_prompt` and its session system
/// entries have empty `content`/`sections`, so this is `false` for the normal
/// session-backed request and the prompt text is charged. A message that DOES
/// carry the prompt (an upstream-shaped checkpoint whose `sections` hold it, a
/// replayed initial system message) already pays for it inside
/// [`estimate_message_units`], and charging the out-of-band copy again would
/// count the prompt twice.
fn system_describes_prompt(system: Option<&Message>, prompt: &str) -> bool {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return true;
    }
    let Some(Message::System(system)) = system else {
        return false;
    };
    let in_content = system
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text(text) if text.text.contains(prompt)));
    in_content
        || system
            .sections
            .iter()
            .flat_map(|sections| sections.values())
            .flatten()
            .any(|text| text.contains(prompt))
}

/// Quarter-token weight of the request's tool declarations that the newest
/// system message does NOT already describe.
///
/// TOOL DECLARATIONS ARE COUNTED ONCE (PIR-BIN-070). A content floor charges
/// the newest system message in full via [`estimate_message_units`], whose
/// `Message::System` arm includes `toolsAdded` — the definitions the session
/// entry records and the request passes in `tools[]` are the same
/// declarations. Adding [`prompt_token_units`]' `tools` term on top charged
/// that block twice: measured at 790 tokens on a 3-tool allowlist
/// (`toolsAdded` 3 160 B vs wire `tools[]` 3 127 B) and 1 906 tokens on the
/// default 9-tool loadout, so the threshold fired 781 tokens early and
/// compacted 24 times over 26 turns where upstream compacted 8.
///
/// The wire is authoritative, so the request's `tools[]` stays charged and only
/// the names the newest system message already declares are subtracted. A
/// mid-run `declareToolChanges` delta declares a SUBSET of the live loadout, so
/// subtracting by name (rather than dropping the whole term) keeps the floor at
/// the full request size; conversely, an entry without a declaration record (a
/// prompt-only checkpoint after a resume) charges every tool. The subset is
/// weighed as its own JSON array (`[` + separators + `]`), which is exactly the
/// text the full array would carry for those entries, so the floor is never
/// below the real request.
///
/// `full_list_units` is the request's FULL tool-declaration weight
/// ([`tool_declaration_units`], array framing included) when the caller already
/// computed it: the `declared == None` branch charges the whole `tools[]`
/// array, so a caller that weighed that same array for the usage-grounded
/// estimate's prefix ([`estimate_request_projected_context_tokens_pair`])
/// passes the value here and one request serializes the declarations once
/// instead of once per estimator. The declarations the newest system message
/// DOES carry still serialize their undeclared subset — a different byte
/// string, nothing to reuse.
fn undeclared_tool_units(
    system: Option<&Message>,
    tools: Option<&[crate::ai::types::Tool]>,
    full_list_units: Option<u64>,
) -> u64 {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else {
        return 0;
    };
    let declared = match system {
        Some(Message::System(system)) => system
            .tools_added
            .as_deref()
            .filter(|declared| !declared.is_empty()),
        _ => None,
    };
    let Some(declared) = declared else {
        return full_list_units.unwrap_or_else(|| tool_declaration_units(Some(tools)));
    };
    let declared_names: std::collections::BTreeSet<&str> =
        declared.iter().map(|tool| tool.name.as_str()).collect();
    let mut units = 0u64;
    let mut undeclared = 0u64;
    for tool in tools {
        if declared_names.contains(tool.name.as_str()) {
            continue;
        }
        units = units
            .saturating_add(serde_json::to_string(tool).map_or(0, |json| text_token_units(&json)));
        undeclared = undeclared.saturating_add(1);
    }
    if undeclared == 0 {
        return 0;
    }
    // Array framing: `[` + (n-1) separators + `]`.
    units.saturating_add(undeclared.saturating_add(1))
}

/// Quarter-token weight of the leading prompt state a content floor has NOT
/// already charged through the newest system message: the prompt text (unless
/// that message carries it — [`system_describes_prompt`]) plus only the tool
/// declarations it does not describe ([`undeclared_tool_units`]).
///
/// This is PIR-BIN-070's single-count rule, shared by both floor entry points
/// so the projected and the ephemeral bases can never diverge.
fn request_floor_prompt_units(
    system: Option<&Message>,
    system_prompt: Option<&str>,
    tools: Option<&[crate::ai::types::Tool]>,
) -> u64 {
    request_floor_prompt_units_precomputed(system, system_prompt, tools, None, None)
}

/// [`request_floor_prompt_units`] with the request's prompt-text weight and
/// full tool-declaration weight already computed (`(prompt_text_units,
/// tool_list_units)`; see [`estimate_request_projected_context_tokens_pair`]).
/// Identical numbers — the leading prompt is simply weighed once per request
/// instead of once per estimator.
fn request_floor_prompt_units_precomputed(
    system: Option<&Message>,
    system_prompt: Option<&str>,
    tools: Option<&[crate::ai::types::Tool]>,
    prompt_text_units: Option<u64>,
    tool_list_units: Option<u64>,
) -> u64 {
    let text_units = system_prompt.map_or(0, |prompt| {
        if system_describes_prompt(system, prompt) {
            0
        } else {
            prompt_text_units.unwrap_or_else(|| text_token_units(prompt))
        }
    });
    text_units.saturating_add(undeclared_tool_units(system, tools, tool_list_units))
}

/// Estimated tokens of the request's leading prompt state: the prompt text
/// plus the JSON-serialized tool declarations, using the same Unicode-aware
/// estimator as the transcript.
pub fn estimate_prompt_tokens(
    system_prompt: Option<&str>,
    tools: Option<&[crate::ai::types::Tool]>,
) -> u64 {
    ceil_units(prompt_token_units(system_prompt, tools))
}

/// Upstream's AI-side estimator (`estimateTextTokens`, `utils/estimate.ts`)
/// uses 3.5 chars/token; the compaction estimator uses 4.0. The
/// `max_tokens` clamp is a safety boundary, so it uses the MORE
/// conservative 3.5 for the prompt prefix (an under-estimate there lets a
/// request exceed the window; the transcript estimate already carries
/// usage-grounded numbers when a usage record exists).
pub const CONSERVATIVE_CHARS_PER_TOKEN: f64 = 3.5;

/// [`estimate_prompt_tokens`] at the conservative 3.5-units-per-token rate.
pub fn estimate_prompt_tokens_conservative(
    system_prompt: Option<&str>,
    tools: Option<&[crate::ai::types::Tool]>,
) -> u64 {
    ceil_units_conservative(prompt_token_units(system_prompt, tools))
}

/// Add the leading prompt estimate (`system_prompt` + `tools`) to a
/// transcript estimate unless a usage record already covers the prompt (a
/// usage record includes the leading prompt).
fn add_request_prompt_prefix(
    estimate: &mut ContextUsageEstimate,
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) {
    if estimate.last_usage_index.is_none() {
        let prefix = estimate_prompt_tokens(system_prompt, tools);
        estimate.tokens = estimate.tokens.saturating_add(prefix);
        estimate.trailing_tokens = estimate.trailing_tokens.saturating_add(prefix);
    }
}

/// Estimate a full request from a message list plus the out-of-band leading
/// prompt ([`Context::system_prompt`] / [`Context::tools`]): the messages-only
/// [`estimate_context_tokens`] plus the prompt prefix exactly when the
/// transcript carries no usage record. Callers that genuinely need only the
/// transcript (e.g. summary chunking) keep using
/// [`estimate_context_tokens`].
pub fn estimate_request_messages_tokens(
    messages: &[Message],
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> ContextUsageEstimate {
    let mut estimate = estimate_context_tokens(messages);
    add_request_prompt_prefix(&mut estimate, system_prompt, tools);
    estimate
}

/// Estimate a full request [`Context`]: the transcript estimate plus the
/// leading prompt (`system_prompt` + `tools`) exactly when the transcript
/// carries no usage record (a usage record already includes the leading
/// prompt).
pub fn estimate_request_context_tokens(context: &Context) -> ContextUsageEstimate {
    estimate_request_messages_tokens(
        &context.messages,
        context.system_prompt.as_deref(),
        context.tools.as_deref(),
    )
}

/// [`estimate_request_context_tokens`] with the conservative 3.5
/// units/token rate — the variant the `max_tokens` clamp uses.
pub fn estimate_request_context_tokens_conservative(context: &Context) -> ContextUsageEstimate {
    let mut estimate = estimate_context_tokens_conservative(&context.messages);
    if estimate.last_usage_index.is_none() {
        let prefix = estimate_prompt_tokens_conservative(
            context.system_prompt.as_deref(),
            context.tools.as_deref(),
        );
        estimate.tokens = estimate.tokens.saturating_add(prefix);
        estimate.trailing_tokens = estimate.trailing_tokens.saturating_add(prefix);
    }
    estimate
}

/// Message-content token floor that deliberately IGNORES usage records, plus
/// the newest system message that floor charged in full (the caller needs it to
/// avoid charging the request's leading prompt a second time).
///
/// A provider usage record is normally the exact size of the request that
/// produced it, but it can be stale, bogus, or (in mocked/relay setups) far
/// below the transcript actually sent. The compaction trigger takes the
/// maximum of the usage-grounded estimate and this content floor so a large
/// transcript can never hide behind an understated usage chunk and disable
/// compaction forever.
fn content_token_units<M: MessageSequence + ?Sized>(messages: &M) -> (u64, Option<&Message>) {
    let mut units = 0u64;
    let mut current_system: Option<&Message> = None;
    for index in 0..messages.message_count() {
        let message = messages.message_at(index);
        if matches!(message, Message::System(_)) {
            // Like the stale-usage branch above: only the CURRENT system
            // message (the last one) is prompt state; older checkpoints'
            // system replays are history.
            current_system = Some(message);
        } else {
            units = units.saturating_add(estimate_message_units(message));
        }
    }
    if let Some(system) = current_system {
        units = units.saturating_add(estimate_message_units(system));
    }
    (units, current_system)
}

/// [`estimate_request_messages_tokens`] ignoring any usage record: the
/// content-derived token floor of the request (messages + leading prompt).
///
/// The leading prompt is charged ONCE (PIR-BIN-070): when the newest system
/// message in `messages` records its tool declarations, the request's
/// `tools[]` contributes only the declarations that message does not
/// describe — otherwise the same block is charged both through the system
/// message and through `prompt_token_units`.
pub fn estimate_request_messages_tokens_floor(
    messages: &[Message],
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> u64 {
    let (units, system) = content_token_units(messages);
    ceil_units(units.saturating_add(request_floor_prompt_units(system, system_prompt, tools)))
}

/// [`estimate_request_projected_context_tokens`] ignoring any usage record.
///
/// Same single-count rule as [`estimate_request_messages_tokens_floor`]
/// (PIR-BIN-070): the newest projected system message is charged in full
/// (`estimate_message_units` includes its `toolsAdded`), and the out-of-band
/// prompt contributes only what that message does not already describe.
pub fn estimate_request_projected_context_tokens_floor<E: ProjectedEntry>(
    projected: &[E],
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> u64 {
    let mut units = 0u64;
    let mut current_system: Option<&Message> = None;
    let mut current_system_units = 0u64;
    for entry in projected {
        for message in entry.messages() {
            if matches!(message, Message::System(_)) {
                // Only the newest system message is live prompt state; older
                // checkpoint replays are history (see `content_token_units`).
                current_system = Some(message);
                current_system_units = estimate_message_units(message);
            } else {
                units = units.saturating_add(estimate_message_units(message));
            }
        }
    }
    ceil_units(units.saturating_add(current_system_units).saturating_add(
        request_floor_prompt_units(current_system, system_prompt, tools),
    ))
}

/// Quarter-token weight of a single message, before the final rounding.
///
/// A system message counts its text/image content PLUS its named prompt
/// sections and its declared tool definitions, exactly like upstream
/// (`estimateTokens`'s `system` case: every non-null section's length plus
/// `JSON.stringify(toolsAdded).length`). Leaving those out under-estimated a
/// session whose prompt is mostly tool declarations, so a compaction could
/// trigger late; `toolsRemoved` is not counted upstream either.
fn estimate_message_units(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            let sections: u64 = system
                .sections
                .iter()
                .flat_map(|sections| sections.values())
                .filter_map(|text| text.as_ref())
                .map(|text| text_token_units(text))
                .sum();
            let tools = system
                .tools_added
                .as_ref()
                .and_then(|tools| serde_json::to_string(tools).ok())
                .map_or(0, |json| text_token_units(&json));
            estimate_text_and_image_units(&system.content) + sections + tools
        }
        Message::User(user) | Message::Developer(user) => {
            estimate_text_and_image_units(&user.content)
        }
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text_token_units(&text.text),
                ContentBlock::Thinking(thinking) => text_token_units(&thinking.thinking),
                ContentBlock::ToolCall(call) => {
                    let arguments = serde_json::to_string(&call.arguments).unwrap_or_default();
                    text_token_units(&call.name) + text_token_units(&arguments)
                }
                _ => 0,
            })
            .sum(),
        Message::ToolResult(result) => estimate_text_and_image_units(&result.content),
    }
}

/// Estimate the token count of a single message with the shared Unicode-aware
/// estimator (CJK ≈1 token/char, ASCII ≈4 chars/token).
pub fn estimate_message_tokens(message: &Message) -> u64 {
    ceil_units(estimate_message_units(message))
}

/// [`estimate_message_tokens`] at the conservative 3.5-units-per-token rate.
pub fn estimate_message_tokens_conservative(message: &Message) -> u64 {
    ceil_units_conservative(estimate_message_units(message))
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
    estimate_context_tokens_over(messages, false)
}

/// [`estimate_context_tokens`] at the conservative 3.5-units-per-token rate.
pub fn estimate_context_tokens_conservative(messages: &[Message]) -> ContextUsageEstimate {
    estimate_context_tokens_over(messages, true)
}

/// [`estimate_context_tokens`] over any borrowed [`MessageSequence`]: the same
/// estimate with no message materialization. The projection estimator feeds it
/// the entry messages plus the live context tail as one virtual sequence.
pub(crate) fn estimate_context_tokens_over<M: MessageSequence + ?Sized>(
    messages: &M,
    conservative: bool,
) -> ContextUsageEstimate {
    let usage_info = last_assistant_usage_info(messages);

    let Some((index, usage)) = usage_info else {
        // No usage record describes this context: estimate the WHOLE context
        // by weighted units and round once. Rounding per message instead
        // adds up to a token of rounding slack per message, systematically
        // inflating a long transcript; the context is a single token budget.
        let units: u64 = (0..messages.message_count())
            .map(|i| estimate_message_units(messages.message_at(i)))
            .sum();
        let estimated = if conservative {
            ceil_units_conservative(units)
        } else {
            ceil_units(units)
        };
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(usage);
    let trailing_tokens: u64 = (index + 1..messages.message_count())
        .map(|i| {
            let message = messages.message_at(i);
            if conservative {
                estimate_message_tokens_conservative(message)
            } else {
                estimate_message_tokens(message)
            }
        })
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
///
/// The estimate is the FULL request: transcript messages plus the context's
/// out-of-band `system_prompt` and `tools` when no usage record covers them.
pub fn should_compact(context: &Context, model: &Model, config: &CompactionConfig) -> bool {
    should_compact_tokens(
        estimate_request_context_tokens(context).tokens,
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

/// Owned [`PathEntry`] over a SHARED session entry handle
/// ([`crate::session_manager::FileEntry`]): the branch-snapshot shape the
/// prompt-run compaction gate and its per-run threshold hook hold across turns.
///
/// The hook reads `id()` equality plus the two entry-kind flags only.
/// [`PathEntryMeta`] owns one id `String` per path entry, so every prompt paid
/// an allocation + copy for every entry on the leaf path
/// ([`crate::session_manager::SessionManager::branch_path_meta`]);
/// this shape keeps the manager's `Arc<FileEntry>` instead (a refcount bump,
/// no allocation, no id copy — the entries are already shared with the
/// projection metadata) and derives the SAME id/flags from it.
#[derive(Debug, Clone)]
pub struct PathEntryShared(pub std::sync::Arc<FileEntry>);

impl PathEntryShared {
    /// Borrow the shared session entry.
    pub fn entry(&self) -> &FileEntry {
        &self.0
    }
    /// Same fact as [`PathEntry::is_compaction`], for callers holding the
    /// concrete type (`branch.last().is_some_and(...)`).
    pub fn is_compaction(&self) -> bool {
        matches!(&*self.0, FileEntry::Compaction(_))
    }
    /// Same fact as [`PathEntry::is_context_edit`], for callers holding the
    /// concrete type.
    pub fn is_context_edit(&self) -> bool {
        matches!(&*self.0, FileEntry::ContextEdit(_))
    }
}

impl PathEntry for PathEntryShared {
    fn id(&self) -> Option<&str> {
        self.0.id()
    }
    fn is_compaction(&self) -> bool {
        PathEntryShared::is_compaction(self)
    }
    fn is_context_edit(&self) -> bool {
        PathEntryShared::is_context_edit(self)
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

/// [`estimate_projected_context_tokens`] plus the request's out-of-band
/// leading prompt (`system_prompt` + `tools`) when the estimate carries no
/// usage record. This is the compaction-threshold estimator over a projection:
/// a transcript without a usage record would otherwise omit the prompt from
/// the trigger check.
pub fn estimate_request_projected_context_tokens<E: ProjectedEntry, P: PathEntry>(
    projected: &[E],
    branch_entries: &[P],
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> ContextUsageEstimate {
    let mut estimate = estimate_projected_context_tokens(projected, branch_entries);
    add_request_prompt_prefix(&mut estimate, system_prompt, tools);
    estimate
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
    let estimate = estimate_context_tokens_over(refs.as_slice(), false);
    if let Some(usage_index) = estimate.last_usage_index {
        // Map the usage message back to its projected entry id. A usage inside
        // the tail walks past every projected entry and stays `None`; such a
        // record is NEWER than every projected/branch entry, so it can never
        // be invalidated by a compaction/context edit later in the branch and
        // must be trusted (upstream `estimateContextTokens(projection.messages)`
        // has no tail: the in-run assistant is already a projected entry).
        let projected_message_count: usize =
            projected.iter().map(|entry| entry.messages().len()).sum();
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
        let usage_in_tail = usage_index >= projected_message_count;

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
        // A synthetic (id-less) basis has no branch identity to compare: when
        // the caller supplied no path metadata (ephemeral/no-session run)
        // there is no compaction/context edit that could invalidate it.
        let synthetic_basis = usage_entry_id.is_none() && branch_entries.is_empty();
        if usage_in_tail || usage_index_i64 > latest_i64 || synthetic_basis {
            return estimate;
        }
    }

    // Stale (or no) usage: estimate every message, plus the current system
    // message, with the shared Unicode-aware estimator.
    let mut tokens = current_system_units(projected, tail).map_or(0, ceil_units);
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

/// Quarter-token weight of the CURRENT system message over the projection +
/// tail sequence (the last `Message::System`): its text blocks joined with
/// `"\n"`, exactly the text
/// `session_manager::projection::current_system_message` builds from a
/// materialized slice. The materializing twin needs the owned slice this
/// borrowed estimator avoids building, so the join is reproduced here (only
/// the estimated weight is consumed).
fn current_system_units<E: ProjectedEntry>(projected: &[E], tail: &[Message]) -> Option<u64> {
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
    system.map(system_text_join_units)
}

/// Quarter-token weight of a system message's text blocks joined with `"\n"`
/// ([`current_system_units`]'s join value): the blocks' weights summed plus
/// `texts - 1` separator characters. [`text_token_units`] is a per-character
/// sum and a newline is one ASCII unit, so this is exactly the joined string's
/// weight — with no intermediate `String` and no second scan of the copy.
fn system_text_join_units(message: &Message) -> u64 {
    let Message::System(system) = message else {
        return 0;
    };
    let mut units = 0u64;
    let mut texts = 0u64;
    for block in &system.content {
        if let ContentBlock::Text(text) = block {
            units = units.saturating_add(text_token_units(&text.text));
            texts = texts.saturating_add(1);
        }
    }
    if texts == 0 {
        0
    } else {
        units.saturating_add(texts - 1)
    }
}

/// BOTH numbers the per-request compaction trigger decides with, from ONE
/// traversal of `projected` (+ `tail`): the value of
/// [`estimate_request_projected_context_tokens`] over the same basis (the tail
/// seen exactly like [`estimate_projected_context_tokens_with_tail`] sees it)
/// and the value of [`estimate_request_projected_context_tokens_floor`],
/// returned as `(usage_grounded, floor)`.
///
/// [`crate::compaction::should_compact_request_refs`] uses this instead of
/// calling the two estimators separately (PIR-BIN-095): the transcript's
/// per-message weight is computed once per request (the floor already walks
/// every message; the usage-grounded estimate re-walked the transcript after
/// it), the leading prompt's text and the JSON-serialized tool declarations
/// are weighed at most once and only when a branch consumes them, no
/// `Vec<&Message>` view is materialized, and the two branch lookups travel in
/// one backward pass. The numbers are identical to the two public estimators —
/// pinned by `p21_bin095_pair_matches_the_public_estimators`.
///
/// Tail semantics: the public floor has no tail parameter (it walks
/// `projected` only), while the est's stale path sees the newest system
/// message over `projected` PLUS `tail`; the two are tracked separately, so
/// this stays exact for a caller that passes a tail.
#[doc(hidden)] // internal seam of `should_compact_request_refs`; tests pin the equality
pub fn estimate_request_projected_context_tokens_pair<E: ProjectedEntry, P: PathEntry>(
    projected: &[E],
    tail: &[Message],
    branch_entries: &[P],
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> (u64, u64) {
    let projected_message_count: usize = projected.iter().map(|entry| entry.messages().len()).sum();

    // One traversal. Every number below is a running sum over the per-message
    // weights (`estimate_message_units` is the only expensive per-message
    // step, and it runs exactly once per message):
    //  * `floor_units`         — Σ units over NON-system projected messages
    //                            (the floor rounds that sum once);
    //  * `floor_system_units`  — the newest PROJECTED system message's units;
    //  * `stale_system_message`— the newest system message over projection +
    //                            tail (the stale path charges its JOINED TEXT,
    //                            see `system_text_join_units`);
    //  * `stale_ceil_units`    — Σ ceil(units) over non-system messages (the
    //                            stale path rounds PER MESSAGE);
    //  * `trailing_ceil_units` — Σ ceil(units) over the messages after the
    //                            newest ACCEPTED usage record;
    //  * `usage_info`          — that record, under the same forward timestamp
    //                            guard as `last_assistant_usage_info`.
    let mut floor_units = 0u64;
    let mut floor_system_units = 0u64;
    let mut stale_ceil_units = 0u64;
    let mut trailing_ceil_units = 0u64;
    let mut floor_system_message: Option<&Message> = None;
    let mut stale_system_message: Option<&Message> = None;
    let mut usage_info: Option<(usize, &Usage)> = None;
    let mut latest_prefix_timestamp = 0u64;
    for (index, message) in projected
        .iter()
        .flat_map(|entry| entry.messages().iter())
        .chain(tail.iter())
        .enumerate()
    {
        let units = estimate_message_units(message);
        if matches!(message, Message::System(_)) {
            // The stale path charges the NEWEST system message (over
            // projection + tail) by its joined TEXT only — no sections, no
            // `toolsAdded`, exactly `current_system_units`' rule. The floor
            // needs the full message and only ever sees the newest PROJECTED
            // one (its public entry point has no tail).
            stale_system_message = Some(message);
            if index < projected_message_count {
                floor_system_message = Some(message);
                floor_system_units = units;
            }
        } else {
            // The content floor walks `projected` ONLY — its public entry
            // point has no tail (a tail message is not-yet-projected state).
            // The stale path walks the whole projection + tail.
            if index < projected_message_count {
                floor_units = floor_units.saturating_add(units);
            }
            stale_ceil_units = stale_ceil_units.saturating_add(ceil_units(units));
        }
        // Same forward guard as `last_assistant_usage_info`: a usage only
        // applies when its timestamp is at least the newest prefix timestamp
        // seen so far, and only NON-system messages advance that prefix
        // timestamp. A rejected usage is still an ordinary message after the
        // accepted record, so it stays in the trailing sum.
        let timestamp = message_timestamp(message);
        if let Some(usage) =
            assistant_usage(message).filter(|_| timestamp >= latest_prefix_timestamp)
        {
            usage_info = Some((index, usage));
            trailing_ceil_units = 0;
        } else if usage_info.is_some() {
            trailing_ceil_units = trailing_ceil_units.saturating_add(ceil_units(units));
        }
        if !matches!(message, Message::System(_)) {
            latest_prefix_timestamp = latest_prefix_timestamp.max(timestamp);
        }
    }

    // The usage-grounded estimate: `estimate_context_tokens_over`'s usage
    // branch (the usage record plus the per-message-rounded messages after it)
    // plus `estimate_projected_context_tokens_with_tail`'s stale-usage
    // invalidation against the branch.
    let mut has_usage = false;
    let mut estimated = 0u64;
    if let Some((usage_index, usage)) = usage_info {
        estimated = calculate_context_tokens(usage).saturating_add(trailing_ceil_units);
        has_usage = true;
        // Map the usage message back to its projected entry id (a usage inside
        // the tail maps to no entry id) — the same walk the borrowed estimator
        // runs.
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
        // ONE backward pass yields both branch facts: `usage_entry_index` ends
        // at the FIRST index carrying the usage entry id (the last assignment
        // wins while walking backwards, matching `position`) and
        // `latest_invalidating_index` is the first match from the end,
        // matching `rposition`.
        let mut usage_entry_index: Option<usize> = None;
        let mut latest_invalidating_index: Option<usize> = None;
        for (index, entry) in branch_entries.iter().enumerate().rev() {
            if usage_entry_id.is_some() && entry.id() == usage_entry_id {
                usage_entry_index = Some(index);
            }
            if latest_invalidating_index.is_none()
                && (entry.is_context_edit() || entry.is_compaction())
            {
                latest_invalidating_index = Some(index);
            }
        }
        let usage_index_i64 = usage_entry_index.map_or(-1, |index| index as i64);
        let latest_i64 = latest_invalidating_index.map_or(-1, |index| index as i64);
        let usage_in_tail = usage_index >= projected_message_count;
        // A synthetic (id-less) basis has no branch identity to compare: when
        // the caller supplied no path metadata (ephemeral/no-session run)
        // there is no compaction/context edit that could invalidate it.
        let synthetic_basis = usage_entry_id.is_none() && branch_entries.is_empty();
        if !(usage_in_tail || usage_index_i64 > latest_i64 || synthetic_basis) {
            // Stale: the estimate falls back to content, like the borrowed
            // estimator's tail of the same name.
            has_usage = false;
        }
    }
    if !has_usage {
        // The stale (or no-usage) fallback: the newest system message's JOINED
        // text plus PER-MESSAGE rounded weights of every non-system message.
        // System messages are otherwise skipped, so their sections and
        // `toolsAdded` are NOT charged here — the content floor below carries
        // them (the public estimator's rule).
        let system_units = stale_system_message.map_or(0, system_text_join_units);
        estimated = ceil_units(system_units).saturating_add(stale_ceil_units);
    }

    // The leading prompt's weights, weighed at most once for BOTH estimators
    // and only when a branch consumes them: the common session case (a trusted
    // usage record plus a system replay declaring the live tools) needs
    // neither the prompt-text scan nor the `tools[]` serialization.
    let described =
        system_prompt.is_some_and(|prompt| system_describes_prompt(floor_system_message, prompt));
    let prefix_needed = !has_usage;
    let prompt_text_needed = system_prompt.is_some() && (prefix_needed || !described);
    let declared_tools = match floor_system_message {
        Some(Message::System(system)) => system
            .tools_added
            .as_deref()
            .filter(|declared| !declared.is_empty()),
        _ => None,
    };
    let tool_list_needed =
        tools.is_some_and(|tools| !tools.is_empty()) && (prefix_needed || declared_tools.is_none());
    let prompt_text_units = if prompt_text_needed {
        Some(system_prompt.map_or(0, text_token_units))
    } else {
        None
    };
    let tool_list_units = if tool_list_needed {
        Some(tool_declaration_units(tools))
    } else {
        None
    };
    if !has_usage {
        // The leading prompt rides exactly when no usage record covers it
        // (`last_usage_index.is_none()` in the public estimator).
        let prefix = ceil_units(
            prompt_text_units
                .unwrap_or_else(|| system_prompt.map_or(0, text_token_units))
                .saturating_add(tool_list_units.unwrap_or_else(|| tool_declaration_units(tools))),
        );
        estimated = estimated.saturating_add(prefix);
    }

    // The content floor: the newest PROJECTED system message in full plus
    // every non-system projected message plus the single-count leading prompt
    // (tool declarations counted once, PIR-BIN-070).
    let floor = ceil_units(
        floor_units
            .saturating_add(floor_system_units)
            .saturating_add(request_floor_prompt_units_precomputed(
                floor_system_message,
                system_prompt,
                tools,
                prompt_text_units,
                tool_list_units,
            )),
    );
    (estimated, floor)
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
