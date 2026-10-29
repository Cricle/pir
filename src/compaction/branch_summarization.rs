//! Branch summarization for tree navigation (upstream
//! `packages/coding-agent/src/core/compaction/branch-summarization.ts`).
//!
//! When navigating to a different point in the session tree, the abandoned
//! branch (the old leaf back to the common ancestor with the target) is
//! summarized so context is not lost:
//!
//! - [`collect_entries_for_branch_summary`] mirrors upstream
//!   `collectEntriesForBranchSummary`: it walks from the old leaf back to the
//!   deepest node shared with the target path. Compaction boundaries are NOT
//!   stop points — their entries are included and their summaries become
//!   context.
//! - [`prepare_branch_entries`] mirrors upstream `prepareBranchEntries`:
//!   messages are collected NEWEST-first until the token budget is hit, so the
//!   most recent context survives a too-long branch. File operations are
//!   accumulated from ALL entries (even ones dropped by the budget) so nested
//!   `branch_summary` details carry over cumulatively.
//! - [`generate_branch_summary`] mirrors upstream `generateBranchSummary`:
//!   one summarizer request over the serialized conversation, with the same
//!   prompt text, the same preamble, and the same response guards. Failures
//!   surface as [`BranchSummaryResult::error`] — never a panic. A cancelled
//!   summarizer stream (`StopReason::Aborted`, the `/tree` Esc path) returns
//!   [`BranchSummaryResult::aborted`] like upstream.
//!
//! Deviations from upstream, by design: pir's `Message` union has no
//! `custom`/`branchSummary`/`compactionSummary` roles, so those entries are
//! rendered as their framed user-text equivalents before serialization (same
//! rule as the session projection and the compaction module). The
//! [`CompleteFn`] seam carries no options parameter; the `/tree` abort signal
//! therefore rides the [`CompleteFn`] the caller builds
//! (`AgentSession::complete_fn_for_signal`), and an `StopReason::Aborted`
//! response is rejected like upstream's `aborted` outcome.

use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::ai::types::{Message, Model, Usage};
use crate::session_manager::projection::{
    custom_message_text, framed_user_message, user_text_message, BRANCH_SUMMARY_PREFIX,
    BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX,
};
use crate::session_manager::types::ms_from_iso;
use crate::session_manager::{FileEntry, SessionManager};

use super::algorithm::estimate_message_tokens;
use super::{
    compute_file_lists, extract_file_ops_from_messages, format_file_operations, CompactionConfig,
    CompleteFn, FileOperations,
};

// ============================================================================
// Types (upstream `BranchSummaryResult`, `BranchPreparation`,
// `CollectEntriesResult`, `GenerateBranchSummaryOptions`)
// ============================================================================

/// Default tokens reserved when selecting branch history (upstream
/// `reserveTokens` default, `GenerateBranchSummaryOptions.reserveTokens`).
pub const DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS: u64 = 16384;

/// Result of a branch-summary generation (upstream `BranchSummaryResult`).
/// `summary: None` + `error: Some(..)` is the failure shape; a `None` summary
/// with no error does not occur (the empty-branch case yields the static
/// "No content to summarize" text, like upstream).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchSummaryResult {
    /// The generated summary text (preamble + model output + formatted file
    /// operations).
    pub summary: Option<String>,
    /// Usage from the summarizer call, when the response carried one.
    pub usage: Option<Usage>,
    /// Files only read (never modified) on the abandoned branch (upstream
    /// `details.readFiles`).
    pub read_files: Vec<String>,
    /// Files read AND written/edited on the abandoned branch (upstream
    /// `details.modifiedFiles`).
    pub modified_files: Vec<String>,
    /// The summarizer call was aborted (upstream `aborted`, checked BEFORE
    /// the failure guards at branch-summarization.ts:355-358): no summary is
    /// returned and the navigation leaves the leaf unchanged.
    pub aborted: bool,
    /// Why the summarizer failed (upstream `error`; navigation is aborted and
    /// the leaf left unchanged when this is set).
    pub error: Option<String>,
}

/// Entries prepared for summarization (upstream `BranchPreparation`).
#[derive(Debug, Clone, Default)]
pub struct BranchPreparation {
    /// Messages extracted for summarization, in chronological order.
    pub messages: Vec<Message>,
    /// File operations extracted from tool calls and nested branch summaries.
    pub file_ops: FileOperations,
    /// Total estimated tokens in `messages`.
    pub total_tokens: u64,
}

/// Entries collected for the abandoned branch (upstream
/// `CollectEntriesResult`).
#[derive(Debug, Clone, Default)]
pub struct CollectEntriesResult {
    /// Entries to summarize, in chronological order.
    pub entries: Vec<FileEntry>,
    /// Common ancestor between the old and new position, if any.
    pub common_ancestor_id: Option<String>,
}

// ============================================================================
// Entry collection (upstream `collectEntriesForBranchSummary`)
// ============================================================================

/// Collect the entries that should be summarized when navigating from one
/// position to another: everything from `old_leaf_id` back to the common
/// ancestor with `target_id`, in chronological order. No old position means
/// nothing to summarize.
pub fn collect_entries_for_branch_summary(
    session: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> CollectEntriesResult {
    let Some(old_leaf_id) = old_leaf_id else {
        return CollectEntriesResult::default();
    };

    // Find the common ancestor: the deepest node on both paths (upstream
    // walks the target path root-first backwards).
    let old_path_entries = session
        .branch_entries(Some(old_leaf_id))
        .unwrap_or_default();
    let old_path: std::collections::HashSet<&str> = old_path_entries
        .iter()
        .filter_map(|entry| entry.id())
        .collect();
    let target_path = session.branch_entries(Some(target_id)).unwrap_or_default();

    let mut common_ancestor_id: Option<String> = None;
    for entry in target_path.iter().rev() {
        if let Some(id) = entry.id() {
            if old_path.contains(id) {
                common_ancestor_id = Some(id.to_owned());
                break;
            }
        }
    }

    // Walk from the old leaf back to the common ancestor (exclusive), then
    // reverse to chronological order.
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut current: Option<String> = Some(old_leaf_id.to_owned());
    while let Some(id) = current {
        if common_ancestor_id.as_deref() == Some(id.as_str()) {
            break;
        }
        let Some(entry) = session.entry(&id) else {
            break;
        };
        current = entry.parent_id().map(str::to_owned);
        entries.push(entry.clone());
    }
    entries.reverse();

    CollectEntriesResult {
        entries,
        common_ancestor_id,
    }
}

// ============================================================================
// Leaf-staleness guard (navigate_tree phase 3)
// ============================================================================

/// Validate that the session leaf did not move between the branch collection
/// (navigate_tree phase 1, under the storage lock) and the summary attach
/// (phase 3, lock re-acquired): the summarize gap releases the lock, so a
/// concurrent append can move the leaf and make the collected branch stale.
/// Returns the navigation error in that case — failing the navigation is
/// preferred over attaching a summary computed from the wrong entries.
pub fn ensure_leaf_unchanged(
    current_leaf: Option<&str>,
    collected_leaf: Option<&str>,
) -> Result<(), String> {
    if current_leaf != collected_leaf {
        return Err("session changed during branch summarization; navigation aborted".to_owned());
    }
    Ok(())
}

// ============================================================================
// Entry to message conversion (upstream `getMessageFromEntry`)
// ============================================================================

/// Extract the model message from a session entry (upstream
/// `getMessageFromEntry`): message entries skip tool results (the context
/// lives in the assistant's tool call); custom-message, branch-summary, and
/// compaction entries contribute their framed user text; state entries
/// contribute nothing.
fn get_message_from_entry(entry: &FileEntry) -> Option<Message> {
    match entry {
        FileEntry::Message(inner) => {
            let message: Message = inner.message.clone().into();
            // Skip tool results - context is in assistant's tool call.
            if matches!(message, Message::ToolResult(_)) {
                None
            } else {
                Some(message)
            }
        }
        FileEntry::CustomMessage(inner) => Some(user_text_message(
            &custom_message_text(&inner.content),
            ms_from_iso(entry.timestamp()).unwrap_or(0),
        )),
        FileEntry::BranchSummary(inner) if !inner.summary.is_empty() => Some(framed_user_message(
            BRANCH_SUMMARY_PREFIX,
            &inner.summary,
            BRANCH_SUMMARY_SUFFIX,
            entry.timestamp(),
        )),
        FileEntry::Compaction(inner) => Some(framed_user_message(
            COMPACTION_SUMMARY_PREFIX,
            &inner.summary,
            COMPACTION_SUMMARY_SUFFIX,
            entry.timestamp(),
        )),
        // thinking_level_change, model_change, custom, label, session_info,
        // usage, context_edit: no conversation content.
        _ => None,
    }
}

/// Read the cumulative file tracking off a prior branch summary's details
/// (upstream `BranchSummaryDetails`): only pi-generated summaries
/// (`fromHook !== true`) carry it.
fn carry_branch_summary_details(details: Option<&JsonValue>, ops: &mut FileOperations) {
    let Some(details) = details else {
        return;
    };
    if let Some(read) = details.get("readFiles").and_then(JsonValue::as_array) {
        for path in read.iter().filter_map(JsonValue::as_str) {
            ops.read.insert(path.to_owned());
        }
    }
    if let Some(modified) = details.get("modifiedFiles").and_then(JsonValue::as_array) {
        // Modified files go into edited for proper deduplication (upstream
        // adds to `edited` only; `compute_file_lists` unions with written).
        for path in modified.iter().filter_map(JsonValue::as_str) {
            ops.edited.insert(path.to_owned());
        }
    }
}

// ============================================================================
// Preparation (upstream `prepareBranchEntries`)
// ============================================================================

/// Prepare entries for summarization with a token budget (upstream
/// `prepareBranchEntries`): walk from NEWEST to OLDEST, adding messages until
/// the budget is hit, so the most recent context is kept when the branch is
/// too long. `token_budget: 0` means no limit. Summary entries (compaction /
/// branch_summary) still fit when at least 10% of the budget remains.
pub fn prepare_branch_entries(entries: &[FileEntry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<Message> = Vec::new();
    let mut file_ops = FileOperations::default();
    let mut total_tokens: u64 = 0;

    // First pass: collect file ops from ALL entries (even those dropped by
    // the token budget) so nested branch summaries carry cumulative tracking.
    for entry in entries {
        if let FileEntry::BranchSummary(inner) = entry {
            if inner.from_hook != Some(true) {
                carry_branch_summary_details(inner.details.as_ref(), &mut file_ops);
            }
        }
    }

    // Second pass: newest to oldest, until the budget.
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };

        // Extract file ops from assistant messages (tool calls).
        extract_file_ops_from_messages(std::slice::from_ref(&message), &mut file_ops);

        let tokens = estimate_message_tokens(&message);

        // Check the budget before adding.
        if token_budget > 0 && total_tokens + tokens > token_budget {
            // Summary entries still fit while under 90% of the budget
            // (important context, upstream).
            if matches!(
                entry,
                FileEntry::Compaction(_) | FileEntry::BranchSummary(_)
            ) && (total_tokens as f64) < token_budget as f64 * 0.9
            {
                messages.push(message);
                total_tokens += tokens;
            }
            // Stop - the budget is hit.
            break;
        }

        messages.push(message);
        total_tokens += tokens;
    }
    // The walk collected newest-first; restore chronological order (upstream
    // `messages.unshift` per accepted message).
    messages.reverse();

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

// ============================================================================
// Summary generation (upstream `generateBranchSummary`)
// ============================================================================

/// Upstream `BRANCH_SUMMARY_PREAMBLE` (branch-summarization.ts:253).
const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// Upstream `BRANCH_SUMMARY_PROMPT` (branch-summarization.ts:258-285),
/// verbatim.
pub const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// The model's own output limit for the branch summarizer call (`0` = no model
/// cap). Upstream removed the hardcoded 4096-token cap: the provider sizes
/// output from the actual request using the model's limit (upstream #11036).
pub fn branch_summary_max_tokens(model: &Model) -> u64 {
    model.max_tokens
}

/// Generate a summary of the abandoned branch entries (upstream
/// `generateBranchSummary`): one summarizer request over the serialized
/// conversation, retried once on a dropped stream like every other
/// summarization call. An empty message set yields the static "No content to
/// summarize" text; a failed call yields [`BranchSummaryResult::error`].
pub async fn generate_branch_summary(
    entries: &[FileEntry],
    model: &Model,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
    reserve_tokens: u64,
) -> BranchSummaryResult {
    generate_branch_summary_with_observer(
        entries,
        model,
        complete,
        custom_instructions,
        replace_instructions,
        reserve_tokens,
        None,
    )
    .await
}

/// [`generate_branch_summary`] with the session's summarizer-retry observer
/// installed for the whole run (upstream
/// `_summarizationRetryCallbacks({source: "branchSummary"})`,
/// agent-session.ts:4120-4129).
pub async fn generate_branch_summary_with_observer(
    entries: &[FileEntry],
    model: &Model,
    complete: Arc<CompleteFn>,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
    reserve_tokens: u64,
    retry_observer: Option<super::SummarizationRetryObserver>,
) -> BranchSummaryResult {
    let run = async {
        // Token budget = context window minus reserved space for prompt + response
        // (upstream `model.contextWindow || 128000`).
        let context_window = if model.context_window > 0 {
            model.context_window
        } else {
            128_000
        };
        let token_budget = context_window.saturating_sub(reserve_tokens);

        let preparation = prepare_branch_entries(entries, token_budget);

        if preparation.messages.is_empty() {
            return BranchSummaryResult {
                summary: Some("No content to summarize".to_owned()),
                ..BranchSummaryResult::default()
            };
        }

        // Build the prompt instructions (upstream instructions selection).
        let instructions = if replace_instructions && custom_instructions.is_some() {
            custom_instructions.unwrap_or_default().to_owned()
        } else if let Some(focus) = custom_instructions {
            format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {focus}")
        } else {
            BRANCH_SUMMARY_PROMPT.to_owned()
        };

        // The chunked runner (pir extension): a branch prepared within the window
        // budget takes the upstream single call byte for byte; a span that still
        // exceeds the per-chunk budget (context edits, a window smaller than the
        // reserve, ...) is split turn-aligned and folded through the same prompts,
        // so no summarizer request can exceed the model window.
        let config = CompactionConfig {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens: 0,
        };
        let summarized = match super::summarize_branch_span_chunked(
            preparation.messages.as_slice(),
            &instructions,
            model,
            &config,
            complete,
        )
        .await
        {
            Ok(summarized) => summarized,
            // The runner's errors are already the upstream-shaped texts: a
            // complete failure is "Branch summarization failed: {error}", a
            // rejected response keeps its own guard text ("Branch summarization
            // attempted to call a tool"). Wrapping them again would mangle the
            // guard text, so they pass through VERBATIM.
            Err(error) => {
                return BranchSummaryResult {
                    error: Some(error),
                    ..BranchSummaryResult::default()
                };
            }
        };
        // An aborted response carries partial text that must not be persisted as
        // the branch checkpoint (upstream branch-summarization.ts:355-358 checks
        // `aborted` BEFORE the failure guards and returns the aborted outcome).
        if summarized.aborted {
            return BranchSummaryResult {
                aborted: true,
                ..BranchSummaryResult::default()
            };
        }
        let text = summarized.text;
        let usage = summarized.usage;

        // Prepend the preamble to provide context about the branch summary.
        let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{text}");

        // Compute the file lists and append them to the summary.
        let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
        summary.push_str(&format_file_operations(&read_files, &modified_files));

        BranchSummaryResult {
            summary: if summary.is_empty() {
                Some("No summary generated".to_owned())
            } else {
                Some(summary)
            },
            usage: Some(usage),
            read_files,
            modified_files,
            aborted: false,
            error: None,
        }
    };
    match retry_observer {
        Some(observer) => super::with_summarization_retry(observer, run).await,
        None => run.await,
    }
}
