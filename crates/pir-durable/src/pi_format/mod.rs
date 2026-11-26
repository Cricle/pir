//! pi session format (version 3 JSONL) ↔ durable records.
//!
//! The bridge between `crate::session_manager`'s on-disk pi format and
//! `crate::durable`'s committed record model. `import` reads a pi session file
//! into entry drafts plus the initial live-document state; `export` renders
//! durable entries back into a pi file that `SessionManager::open` accepts.
//!
//! # Mapping table
//!
//! | pi entry (`type`) | durable output | notes |
//! |---|---|---|
//! | `session` (header) | [`ImportedSession::header`] + the root `ConversationRecord` | the caller seeds `pi.session` from the header (see §Lossy below) |
//! | `message` (role `user`) | entry kind `pi.user`, `model = [original Message]` | |
//! | `message` (role `assistant`) | `pi.assistant` | |
//! | `message` (role `toolResult`) | `pi.tool-result` | |
//! | `message` (role `system`/`developer`) | `pi.system` | |
//! | `message` (role `custom`) | `pi.custom` | |
//! | `compaction` | `pi.compaction`, `head = firstKeptEntryId` | summary framed as the upstream user-text message; `data.kind = "compaction"` |
//! | `branch_summary` | `pi.compaction` with `data.kind = "branch"` | no `head` (a branch summary does not cut context) |
//! | `context_edit` | entry kind `pi.context-edit` with `edits` | `replacement: null` → `Omit`; `{content}` → `Replace` |
//! | `label` | [`ImportedSession::labels`] | not an entry: durable has no label record |
//! | `session_info` | [`ImportedSession::session_info`] | last name wins |
//! | `model_change` | [`ImportedSession::agent`] `model` | |
//! | `thinking_level_change` | [`ImportedSession::agent`] `thinkingLevel` | |
//! | `usage` | [`ImportedSession::usage`] | numeric fields accumulate |
//! | `custom` | entry kind `pi.custom`, `data = {source:"custom", customType, data}` | |
//! | `custom_message` | `pi.custom`, `model = [user message]`, `data = {source:"custom_message", content, …}` | non-text blocks survive in `data.content` |
//! | unknown `type` | [`OpaqueEntry`] | never fails the import; the export re-emits the line verbatim |
//!
//! # Lossy mappings (deliberate, documented)
//!
//! 1. **`pi.session` document.** `ImportedSession` carries the header and the
//!    last `session_info` name; the caller seeds `pi.session` with both. The
//!    import cannot create the document because documents are transaction
//!    drafts, not part of the entry stream.
//! 2. **Entry ids.** `EntryDraft` has no id (assembly mints one inside the
//!    commit). [`ImportedSession::entry_ids`] therefore maps each pi id to a
//!    **provisional** `EntryId` equal to its 1-based position among the mapped
//!    entries, and `head` targets use those provisional values. Storage mints
//!    durable ids from its own counter, so a caller that appends the drafts (in
//!    order) translates them with [`ImportedSession::remap_entry_ids`]; after
//!    that every head, edit, label and branch-summary target addresses a real
//!    record.
//! 3. **`label` entries** lose their position and timestamps; only the final
//!    `target → label` map survives (the export re-emits labels at the tail).
//! 4. **`context_edit`** entries keep one edit per pi line (durable entries may
//!    carry several; the export emits one line per edit).
//! 5. **`usage`** entries are accumulated into one `pi.usage` map; the
//!    per-entry `kind`/`provider`/`model`/`note`/timestamp attribution is not
//!    preserved.
//! 6. **`custom_message`** is mapped (not opaque) because dropping extension
//!    context messages would change the model-visible transcript.
//!
//! # Export guarantees
//!
//! One compact JSON object per line, `\n`-terminated, header first. Durable
//! numeric ids become stable 8-hex pi ids (high/low 32-bit XOR fold), so the
//! same session exported twice is byte-identical. The entry chain is linear:
//! every line's `parentId` is the previous line's `id` (the first entry's is
//! `null`). Opaque lines from [`ExportInput::opaque`] are re-emitted verbatim
//! at their original position among the entries. The file opens through
//! `SessionManager::open` and projects to the same model-visible transcript.

pub mod export;
pub mod import;

pub use export::{render_session, write_session, ExportError, ExportInput};
pub use import::{
    import_session_text, read_session_file, ImportError, ImportedSession, OpaqueEntry,
};
