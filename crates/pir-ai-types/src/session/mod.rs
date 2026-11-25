//! The on-disk session record types and the summary framing constants.
//!
//! `types` is the version-3 JSONL record set (the shape `SessionManager`
//! writes and the pi-format bridge reads). The four `*_SUMMARY_*` constants
//! frame a stored compaction/branch summary as a user text message, because
//! pir's `Message` union has no dedicated summary role (upstream
//! `core/messages.ts` `convertToLlm`).

pub mod types;

/// Framing prefix of a compaction summary stored as user text (upstream
/// `core/messages.ts`).
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Framing suffix of a compaction summary.
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// Framing prefix of a branch summary.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// Framing suffix of a branch summary.
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";
