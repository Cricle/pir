//! Summarization prompts, ported verbatim from upstream
//! `packages/coding-agent/src/core/compaction/` (`compaction.ts`,
//! `utils.ts`).

// Upstream `core/messages.ts` summary framing constants. The single
// definition lives in `session_manager::projection` (the projection applies
// it when replaying summaries); re-exported here so the compaction module
// reads them from one place.
pub use crate::session_manager::projection::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX,
};

/// Upstream `SUMMARIZATION_SYSTEM_PROMPT` (`compaction/utils.ts`).
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Upstream `SUMMARIZATION_PROMPT` (`compaction/compaction.ts`).
pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Upstream `UPDATE_SUMMARIZATION_INSTRUCTIONS` (`compaction/compaction.ts`).
const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = r#"Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Upstream `TURN_PREFIX_SUMMARIZATION_PROMPT` (`compaction/compaction.ts`):
/// the "checkpoint" framing used by the second, smaller summarizer call when
/// a compaction cuts mid-turn.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages."#;

/// Maximum characters for a tool result in serialized summaries (upstream
/// `TOOL_RESULT_MAX_CHARS` in `compaction/utils.ts`).
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Assemble upstream `UPDATE_SUMMARIZATION_PROMPT`
/// (`compaction/compaction.ts`), which interpolates
/// `UPDATE_SUMMARIZATION_INSTRUCTIONS` after a fixed preamble.
pub fn update_summarization_prompt() -> String {
    format!(
        "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\n{UPDATE_SUMMARIZATION_INSTRUCTIONS}"
    )
}

/// Truncate text to a maximum character length for summarization, keeping the
/// beginning and appending a truncation marker (upstream
/// `truncateForSummary`). Both the limit and the remainder are counted in
/// characters (upstream `String.slice`/`length` are UTF-16 code units), never
/// in bytes, so multi-byte content cannot split a codepoint.
pub fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated_chars = text.chars().count() - max_chars;
    let prefix: String = text.chars().take(max_chars).collect();
    format!(
        "{}\n\n[... {} more characters truncated]",
        prefix, truncated_chars
    )
}
