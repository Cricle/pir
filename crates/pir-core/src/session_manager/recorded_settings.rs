//! Lightweight projection of a session's recorded model/thinking settings.
//!
//! This lives outside `sdk` because the headless `print` and `rpc` hosts need
//! it too, and both compile without the `sdk` feature (`--no-default-features
//! --features cli`). Reading it walks only the branch's state entries, so it
//! never materializes the transcript projection.

use crate::ai::types::ThinkingLevel;
use crate::session_manager::{FileEntry, SessionManager, StoredMessage};

/// The two recorded settings an attach/branch restore needs.
///
/// `get_session_context_settings` reports `"off"` as its thinking default for
/// a branch with no entry; applying that default would silently disable
/// thinking on every resumed session, so absence is reported as `None`.
#[derive(Debug, Default)]
pub struct RecordedSessionSettings {
    /// `(provider, model_id)` of the last model-recorded entry, if any.
    pub model: Option<(String, String)>,
    /// Explicitly recorded thinking level, if the branch carries one.
    pub thinking: Option<ThinkingLevel>,
}

/// Read the recorded model/thinking settings from a session's leaf path.
///
/// The old shape built the full projection (cloning every message of the
/// conversation) just to read `projection.model`; this walks the same path
/// with no transcript copy, so attaching a large session is bounded.
pub fn recorded_session_settings(manager: &SessionManager) -> RecordedSessionSettings {
    let path = manager.projected_branch_path(manager.leaf_id());
    let mut settings = RecordedSessionSettings::default();
    for entry in &path {
        match entry {
            FileEntry::ThinkingLevelChange(change) => {
                // Last PARSEABLE level wins, matching the previous reverse
                // `find_map` (a corrupt trailing value must not hide an
                // earlier valid one).
                if let Ok(level) = change.thinking_level.parse() {
                    settings.thinking = Some(level);
                }
            }
            FileEntry::ModelChange(change) => {
                settings.model = Some((change.provider.clone(), change.model_id.clone()));
            }
            FileEntry::Message(message_entry) => {
                if let StoredMessage::Assistant(assistant) = &message_entry.message {
                    settings.model = Some((assistant.provider.clone(), assistant.model.clone()));
                }
            }
            _ => {}
        }
    }
    settings
}
