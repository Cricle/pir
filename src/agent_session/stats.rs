use super::*;
use crate::session_manager::StoredMessage;

impl AgentSession {
    // ------------------------------------------------------------------
    // Session statistics (upstream getSessionStats)
    // ------------------------------------------------------------------

    /// Aggregate token/cost/message totals over the whole session (upstream
    /// `getSessionStats`), including history that compaction removed.
    pub async fn session_stats(&self) -> SessionStats {
        let mut stats = SessionStats {
            session_file: self
                .session_path()
                .map(|path| path.to_string_lossy().into_owned()),
            session_id: self.session_id().unwrap_or_default(),
            user_messages: 0,
            assistant_messages: 0,
            tool_calls: 0,
            tool_results: 0,
            total_messages: 0,
            tokens: SessionTokens::default(),
            cost: 0.0,
            context_usage: None,
        };
        match &self.session {
            Some(session) => {
                let session = session.lock().await;
                // Borrowed fold: `entries()` clones EVERY stored entry
                // (messages/tool results included) just to read it. The
                // resident list (header first) walked by reference yields the
                // same non-header entry sequence, with no entry clones.
                for entry in session.resident_entries() {
                    let entry = entry.as_ref();
                    if entry.is_header() {
                        continue;
                    }
                    match entry {
                        FileEntry::Message(message_entry) => {
                            add_stored_message_to_stats(&mut stats, &message_entry.message);
                        }
                        FileEntry::Usage(usage_entry) => {
                            add_usage_to_stats(&mut stats, &usage_entry.usage)
                        }
                        FileEntry::Compaction(compaction_entry) => {
                            if let Some(usage) = &compaction_entry.usage {
                                add_usage_to_stats(&mut stats, usage);
                            }
                        }
                        FileEntry::BranchSummary(summary_entry) => {
                            if let Some(usage) = &summary_entry.usage {
                                add_usage_to_stats(&mut stats, usage);
                            }
                        }
                        _ => {}
                    }
                }
            }
            None => {
                let snapshot = self.history_snapshot();
                for message in snapshot.iter() {
                    add_message_to_stats(&mut stats, message);
                }
            }
        }
        stats.context_usage = self.context_usage().await;
        stats
    }

    /// The context-window usage of the current transcript (upstream
    /// `getContextUsage`), or `None` when the model reports no window.
    ///
    /// The fast path is computed over the projection's ENTRY METADATA plus the
    /// per-entry message slices it needs, not over a materialized transcript:
    /// `transcript_snapshot()` copies every stored message into a fresh
    /// `Vec<Message>` just so the estimator can find the last usage record and
    /// measure the few messages after it. The value is identical (same
    /// retention walk, same estimator, same order).
    ///
    /// A usage record only describes the CURRENT context when it was reported
    /// after the newest compaction/context edit (upstream
    /// `estimateProjectedContextTokens`). After a compaction no assistant has
    /// responded yet, so the count is unknown (`tokens`/`percent` are `None`,
    /// upstream `getContextUsage`) instead of the pre-compaction size; with a
    /// stale usage but no compaction (a newer context edit) the projection is
    /// re-estimated by characters over the materialized messages.
    pub async fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.current_model();
        if model.context_window == 0 {
            return None;
        }
        let tokens: Option<u64> = match &self.session {
            Some(session) => {
                // Borrowed metadata: one `Arc` bump per retained entry, no
                // message materialization.
                let session = session.lock().await;
                let metas = session.projection_entry_meta();
                let path = session.branch_path_meta(None);
                // Upstream `getContextUsage`: the latest assistant usage
                // reflects pre-compaction context size, so after a compaction
                // it may only be trusted when that assistant is projected AND
                // sits on the branch after the compaction entry.
                if latest_compaction_lacks_projected_usage(&metas, &path) {
                    return Some(ContextUsage {
                        tokens: None,
                        context_window: model.context_window,
                        percent: None,
                    });
                }
                match projected_context_tokens(&metas, &path) {
                    Some(tokens) => Some(tokens),
                    // No usage record, or one predating the latest compaction
                    // or context edit: re-estimate the whole projection.
                    None => projected_context_reestimate(&session, &metas, &path),
                }
            }
            // Ephemeral history is already a shared, zero-copy snapshot.
            None => {
                let transcript = self.history_snapshot();
                Some(compaction::estimate_context_tokens(&transcript).tokens)
            }
        };
        Some(ContextUsage {
            tokens,
            context_window: model.context_window,
            percent: tokens.map(|tokens| tokens as f64 / model.context_window as f64 * 100.0),
        })
    }
}

/// Upstream `getContextUsage`'s post-compaction check: `true` when the branch
/// has a compaction and no projected entry after it carries a valid assistant
/// usage. The caller then reports an UNKNOWN token count: the last usage
/// describes the pre-compaction context, and nothing has measured the
/// projected (shrunken) context yet.
///
/// `projected_assistants` mirrors upstream's set of projected source-entry
/// ids whose projected messages contain a valid assistant usage. An entry
/// omitted by a context edit (`message_count == 0`) contributes no message
/// and cannot vouch for its usage.
fn latest_compaction_lacks_projected_usage(
    metas: &[compaction::ProjectionEntryMeta],
    path: &[compaction::PathEntryMeta],
) -> bool {
    let Some(compaction_index) = path.iter().rposition(|entry| entry.is_compaction) else {
        return false;
    };
    let projected_assistants: Vec<&str> = metas
        .iter()
        .filter(|meta| meta.message_count > 0)
        .filter(|meta| stored_assistant_usage(meta.source_entry.as_ref()).is_some())
        .filter_map(|meta| meta.source_entry.id())
        .collect();
    !path[compaction_index + 1..]
        .iter()
        .filter_map(|entry| entry.id.as_deref())
        .any(|id| projected_assistants.contains(&id))
}

/// Whole-projection re-estimate used when the last usage cannot be trusted
/// (upstream `estimateProjectedContextTokens`'s character fallback): the
/// materialized projection, the newest system message, and every message
/// estimated by [`compaction::estimate_message_tokens`] — the reported usage
/// is ignored.
fn projected_context_reestimate(
    session: &crate::session_manager::SessionManager,
    metas: &[compaction::ProjectionEntryMeta],
    path: &[compaction::PathEntryMeta],
) -> Option<u64> {
    // Context edits replace projected CONTENT, which the metadata walk does
    // not carry, so this (cold) path materializes the projection. The guarded
    // estimator then drops the stale usage itself (upstream's branch-position
    // invalidation).
    let projection = session.build_session_projection().ok()?;
    let refs = compaction::ProjectionEntryMeta::cut_refs(metas, &projection.messages);
    Some(compaction::estimate_projected_context_tokens_with_tail(&refs, &[], path).tokens)
}

/// [`compaction::estimate_context_tokens`] over a projection's metadata
/// ([`SessionManager::projection_entry_meta`]) instead of its materialized
/// message list.
///
/// Identical arithmetic: the last valid assistant usage contributes its
/// reported tokens, and the messages AFTER it are estimated one entry at a
/// time (`session_entry_to_context_messages` + `estimate_message_tokens`), so
/// only the trailing entries are ever materialized — the transcript prefix
/// before the usage record is never copied.
///
/// Returns `None` when no usage record exists, or when the last one lies at
/// or before the newest compaction/context edit on the branch
/// (`path`): upstream `estimateProjectedContextTokens` treats such a usage as
/// stale and re-estimates the whole projection by characters, which the
/// caller does (the metadata cannot — an edit's replacement content is not
/// part of it).
fn projected_context_tokens(
    metas: &[compaction::ProjectionEntryMeta],
    path: &[compaction::PathEntryMeta],
) -> Option<u64> {
    // The last projected assistant usage wins (upstream
    // `getLastAssistantUsageInfo`). A usage entry's projected contribution is
    // exactly its one message; `message_count == 0` entries (state entries,
    // stale compactions, omitted edits) carry no message at all.
    let (index, usage_tokens, usage_id) = metas
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, meta)| meta.message_count > 0)
        .find_map(|(index, meta)| {
            stored_assistant_usage(meta.source_entry.as_ref())
                .map(|tokens| (index, tokens, meta.source_entry.id()))
        })?;
    // Upstream invalidation: a usage only describes the current context when
    // its entry is STRICTLY newer on the branch than the newest compaction or
    // context edit.
    let usage_path_index = usage_id.and_then(|id| {
        path.iter()
            .position(|entry| entry.id.as_deref() == Some(id))
    })?;
    if let Some(invalidating_index) = path
        .iter()
        .rposition(|entry| entry.is_compaction || entry.is_context_edit)
    {
        if usage_path_index <= invalidating_index {
            return None;
        }
    }
    let mut tokens = usage_tokens;
    for meta in &metas[index + 1..] {
        if meta.message_count == 0 {
            continue;
        }
        for message in crate::session_manager::projection::session_entry_to_context_messages(
            meta.source_entry.as_ref(),
        ) {
            tokens = tokens.saturating_add(compaction::estimate_message_tokens(&message));
        }
    }
    Some(tokens)
}

/// The reported context tokens of a stored assistant message, using the same
/// validity gate as the estimator (`assistant_usage` in
/// `compaction::algorithm`): aborted/error messages and all-zero usage carry
/// no usable data.
fn stored_assistant_usage(entry: &FileEntry) -> Option<u64> {
    let FileEntry::Message(message_entry) = entry else {
        return None;
    };
    let crate::session_manager::StoredMessage::Assistant(assistant) = &message_entry.message else {
        return None;
    };
    if matches!(
        assistant.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        return None;
    }
    let tokens = compaction::calculate_context_tokens(&assistant.usage);
    (tokens > 0).then_some(tokens)
}

/// Fold one transcript message into the running session statistics.
fn add_message_to_stats(stats: &mut SessionStats, message: &Message) {
    stats.total_messages += 1;
    match message {
        Message::User(_) => stats.user_messages += 1,
        Message::Assistant(assistant) => add_assistant_to_stats(stats, assistant),
        Message::ToolResult(result) => add_tool_result_to_stats(stats, result),
        _ => {}
    }
}

/// Fold one STORED transcript message into the running session statistics
/// without materializing (cloning) a [`Message`].
///
/// The role mapping mirrors `impl From<StoredMessage> for Message` exactly:
/// a stored user-role message becomes [`Message::User`] and counts as a user
/// message, a custom message renders as user content and counts too, while
/// stored system messages and system/developer-role user messages only
/// contribute to the total.
fn add_stored_message_to_stats(stats: &mut SessionStats, message: &StoredMessage) {
    stats.total_messages += 1;
    match message {
        StoredMessage::User(user) => {
            if matches!(user.role, UserRole::User) {
                stats.user_messages += 1;
            }
        }
        StoredMessage::Custom(_) => stats.user_messages += 1,
        // A context-visible bash record renders as user text; the `!!` form is
        // excluded from context and only contributes to the total.
        StoredMessage::BashExecution(bash) => {
            if !bash.excluded() {
                stats.user_messages += 1;
            }
        }
        StoredMessage::Assistant(assistant) => add_assistant_to_stats(stats, assistant),
        StoredMessage::ToolResult(result) => add_tool_result_to_stats(stats, result),
        StoredMessage::System(_) => {}
    }
}

/// Fold one assistant message's tool calls and usage into the totals.
fn add_assistant_to_stats(
    stats: &mut SessionStats,
    assistant: &crate::ai::types::AssistantMessage,
) {
    stats.assistant_messages += 1;
    stats.tool_calls += assistant
        .content
        .iter()
        .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
        .count();
    add_usage_to_stats(stats, &assistant.usage);
}

/// Fold one tool-result message's usage into the totals.
fn add_tool_result_to_stats(
    stats: &mut SessionStats,
    result: &crate::ai::types::ToolResultMessage,
) {
    stats.tool_results += 1;
    if let Some(usage) = &result.usage {
        add_usage_to_stats(stats, usage);
    }
}

/// Fold one usage record into the running token/cost totals.
fn add_usage_to_stats(stats: &mut SessionStats, usage: &crate::ai::types::Usage) {
    stats.tokens.input = stats.tokens.input.saturating_add(usage.input);
    stats.tokens.output = stats.tokens.output.saturating_add(usage.output);
    stats.tokens.cache_read = stats.tokens.cache_read.saturating_add(usage.cache_read);
    stats.tokens.cache_write = stats.tokens.cache_write.saturating_add(usage.cache_write);
    stats.tokens.total = stats
        .tokens
        .input
        .saturating_add(stats.tokens.output)
        .saturating_add(stats.tokens.cache_read)
        .saturating_add(stats.tokens.cache_write);
    stats.cost += usage.cost.total;
}

// ---------------------------------------------------------------------------
// /session breakdown (upstream `getUsageCostBreakdown`, usage-totals.ts:61-97,
// and `computeCacheWaste`, cache-stats.ts:104-150). Pure scans over the
// session file entries; the `/session` renderer consumes the results.
// ---------------------------------------------------------------------------

/// One per-model cost bucket (upstream `UsageCostBreakdownEntry`).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageCostEntry {
    /// `provider/model` (or `Tools/summaries` for tool-result / compaction /
    /// branch-summary usage).
    pub key: String,
    pub cost: f64,
    /// input + output + cacheRead + cacheWrite.
    pub tokens: u64,
}

/// Group model-attributed usage by model, with everything else in a separate
/// bucket (upstream `getUsageCostBreakdown`); cost-descending, positive
/// entries only.
pub fn usage_cost_breakdown(
    entries: &[crate::session_manager::types::FileEntry],
) -> Vec<UsageCostEntry> {
    use crate::session_manager::types::{FileEntry, StoredMessage};
    let mut order: Vec<String> = Vec::new();
    let mut totals: std::collections::HashMap<String, (f64, u64)> =
        std::collections::HashMap::new();

    let mut add = |key: String, usage: &crate::ai::types::Usage| {
        if !totals.contains_key(&key) {
            order.push(key.clone());
        }
        let entry = totals.entry(key).or_default();
        entry.0 += usage.cost.total;
        entry.1 = entry
            .1
            .saturating_add(usage.input)
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
    };

    for entry in entries {
        match entry {
            FileEntry::Message(message_entry) => match &message_entry.message {
                StoredMessage::Assistant(assistant) => {
                    let key = format!(
                        "{}/{}",
                        assistant.provider,
                        assistant
                            .response_model
                            .as_deref()
                            .unwrap_or(&assistant.model)
                    );
                    add(key, &assistant.usage);
                }
                StoredMessage::ToolResult(result) => {
                    if let Some(usage) = &result.usage {
                        add("Tools/summaries".to_owned(), usage);
                    }
                }
                _ => {}
            },
            FileEntry::Usage(usage_entry) => {
                add(
                    format!("{}/{}", usage_entry.provider, usage_entry.model),
                    &usage_entry.usage,
                );
            }
            FileEntry::Compaction(compaction_entry) => {
                if let Some(usage) = &compaction_entry.usage {
                    add("Tools/summaries".to_owned(), usage);
                }
            }
            FileEntry::BranchSummary(summary_entry) => {
                if let Some(usage) = &summary_entry.usage {
                    add("Tools/summaries".to_owned(), usage);
                }
            }
            _ => {}
        }
    }

    let mut breakdown: Vec<UsageCostEntry> = order
        .into_iter()
        .filter_map(|key| {
            totals
                .remove(&key)
                .map(|(cost, tokens)| UsageCostEntry { key, cost, tokens })
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0)
        .collect();
    breakdown.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    breakdown
}

/// Cumulative cache waste (upstream `CacheWasteTotals`): prompt tokens that
/// were in the previous turn's prompt but were re-billed instead of read
/// from cache.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    /// Number of counted misses (turns above the noise floor).
    pub miss_count: u64,
}

/// Per-turn misses at or below this are cache breakpoint granularity noise
/// (upstream `NOISE_FLOOR_TOKENS`).
const CACHE_NOISE_FLOOR_TOKENS: u64 = 1024;

/// The last request seen by the scan (upstream `PreviousRequest`).
struct PreviousRequest {
    prompt_tokens: u64,
    /// Sticky: some earlier request in this scan segment reported cache
    /// activity.
    reported_cache: bool,
}

/// Cumulative cache waste across the session (upstream `computeCacheWaste`).
/// `price_cache_read` supplies the model's cache-read rate in $/MTok
/// (upstream's `ModelPriceSource`); `None` means the pricing is unknown.
pub fn compute_cache_waste(
    entries: &[crate::session_manager::types::FileEntry],
    price_cache_read: &dyn Fn(&str, &str) -> Option<f64>,
) -> CacheWasteTotals {
    use crate::session_manager::types::{FileEntry, StoredMessage};
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();

    for entry in entries {
        match entry {
            // The context legitimately changed; the next turn's prompt is new
            // content, not re-billed content. Model switches are NOT exempt.
            FileEntry::Compaction(_) | FileEntry::BranchSummary(_) => {
                prev = None;
            }
            FileEntry::Usage(usage_entry) => {
                if usage_entry.kind != crate::cache_warmer::USAGE_KIND_CACHE_WARM {
                    continue;
                }
                let usage = &usage_entry.usage;
                let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
                if prompt_tokens > 0 {
                    prev = Some(PreviousRequest {
                        prompt_tokens,
                        reported_cache: true,
                    });
                }
            }
            FileEntry::Message(message_entry) => {
                let StoredMessage::Assistant(assistant) = &message_entry.message else {
                    continue;
                };
                let usage = &assistant.usage;
                let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
                if let Some(previous) = &prev {
                    // A zero-cache turn only counts when cache activity was
                    // reported before.
                    let counts = prompt_tokens > 0
                        && (usage.cache_read + usage.cache_write > 0 || previous.reported_cache);
                    if counts {
                        let missed_tokens = previous
                            .prompt_tokens
                            .min(prompt_tokens)
                            .saturating_sub(usage.cache_read);
                        if missed_tokens > CACHE_NOISE_FLOOR_TOKENS {
                            // Extra cost = missed tokens at the paid rate
                            // instead of the cache-read rate.
                            let paid_tokens = usage.input + usage.cache_write;
                            let paid_per_token = if paid_tokens > 0 {
                                (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64
                            } else {
                                0.0
                            };
                            let read_per_token = if usage.cache_read > 0 {
                                usage.cost.cache_read / usage.cache_read as f64
                            } else {
                                price_cache_read(&assistant.provider, &assistant.model)
                                    .unwrap_or(0.0)
                                    / 1_000_000.0
                            };
                            totals.missed_tokens += missed_tokens;
                            totals.missed_cost +=
                                missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0);
                            totals.miss_count += 1;
                        }
                    }
                }
                if prompt_tokens > 0 {
                    let reported = prev.as_ref().is_some_and(|p| p.reported_cache)
                        || usage.cache_read + usage.cache_write > 0;
                    prev = Some(PreviousRequest {
                        prompt_tokens,
                        reported_cache: reported,
                    });
                }
            }
            _ => {}
        }
    }
    totals
}
