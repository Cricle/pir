//! Import a pi session file (version 3 JSONL) into durable records.
//!
//! See the module docs of [`super`] for the mapping table and the deliberate
//! lossy mappings. The importer never fails on an unknown entry `type`: such a
//! line is preserved verbatim in [`ImportedSession::opaque`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::durable::ids::{EntryId, ROOT_CONVERSATION_ID};
use crate::durable::session::transaction::{EntryDraft, HeadTarget};
use crate::durable::types::{ContextEdit, ContextEditAction, ConversationRecord};
use crate::session_manager::types::{
    BranchSummaryEntry, CompactionEntry, ContextEditEntry, CustomEntry, CustomMessageEntry,
    FileEntry, LabelEntry, MessageEntry, ModelChangeEntry, SessionHeader, StoredMessage,
    UsageEntry, CURRENT_SESSION_VERSION,
};

/// A line whose `type` the importer does not understand, preserved verbatim so
/// a later export can round-trip it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpaqueEntry {
    /// The pi entry id (`""` when the line carried none).
    pub id: String,
    /// The pi parent id.
    pub parent_id: Option<String>,
    /// The pi timestamp.
    pub timestamp: String,
    /// The line's `type` string.
    pub kind: String,
    /// How many mapped entry drafts preceded this line, so an export can put it
    /// back at the same point in the (linear) exported chain.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub position: usize,
    /// The whole line, verbatim.
    pub raw: Value,
}

/// `skip_serializing_if` helper for [`OpaqueEntry::position`].
fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Everything one pi session file contributes to a durable import.
#[derive(Debug, Clone)]
pub struct ImportedSession {
    /// The original header (the caller seeds `pi.session` from it).
    pub header: SessionHeader,
    /// The root conversation record (id [`ROOT_CONVERSATION_ID`]).
    pub conversation: ConversationRecord,
    /// Entry drafts in original file order.
    pub entries: Vec<EntryDraft>,
    /// pi entry id → **provisional** durable entry id (1-based draft order).
    ///
    /// `EntryDraft` carries no id (assembly mints one inside the commit), so a
    /// provisional id is the draft's 1-based position. `head` targets use these
    /// values; a caller appending the drafts in one transaction gets ids minted
    /// in the same order and can translate by position.
    pub entry_ids: BTreeMap<String, EntryId>,
    /// Initial `pi.agent` document value (`model`, `thinkingLevel`).
    pub agent: Map<String, Value>,
    /// Initial `pi.usage` document value (numeric fields accumulated).
    pub usage: Map<String, Value>,
    /// Final `target → label` map (`pi` has no durable label record).
    pub labels: BTreeMap<EntryId, String>,
    /// Lines the importer did not map.
    pub opaque: Vec<OpaqueEntry>,
    /// The last `session_info` name, for the caller's `pi.session` seed.
    pub session_info: Option<String>,
}

impl ImportedSession {
    /// Rewrite every provisional id to the id the caller's append actually
    /// minted.
    ///
    /// `minted` must hold one id per [`Self::entries`] draft, in the same order
    /// the caller appended them. Durable ids are minted by storage (they do not
    /// start at 1 and are not contiguous), so the provisional 1-based ids are
    /// only a placeholder; this method translates the head targets, context
    /// edits, labels, branch-summary targets (`fromEntry` and the raw pi
    /// `fromId`, kept on the id an export of the minted record will carry) and
    /// [`Self::entry_ids`] so the imported session addresses the real records.
    ///
    /// Callers that append the drafts in one transaction can obtain `minted`
    /// from the commit's documented mint order (a `create`/`append` returns the
    /// record it minted) — the order is the draft order.
    pub fn remap_entry_ids(&mut self, minted: &[EntryId]) -> Result<(), ImportError> {
        if minted.len() != self.entries.len() {
            return Err(ImportError::RemapLength {
                drafts: self.entries.len(),
                minted: minted.len(),
            });
        }
        let translate = |id: EntryId| -> EntryId {
            let position = id.as_i64();
            if position >= 1 && (position as usize) <= minted.len() {
                minted[position as usize - 1]
            } else {
                // Not a provisional id (an already-minted target or a dangling
                // reference): leave it untouched.
                id
            }
        };
        for draft in &mut self.entries {
            if let Some(HeadTarget::Entry(target)) = draft.head {
                draft.head = Some(HeadTarget::Entry(translate(target)));
            }
            if let Some(edits) = &mut draft.edits {
                for edit in edits {
                    edit.target = translate(edit.target);
                }
            }
            if let Some(Value::Object(data)) = &mut draft.data {
                if let Some(target) = data.get("fromEntry").and_then(Value::as_i64) {
                    let translated = translate(EntryId::from_raw(target));
                    data.insert("fromEntry".to_owned(), Value::from(translated.as_i64()));
                    // `fromId` is a pi id. Once the durable target is known,
                    // keep the copied pi id equal to the id the exported line
                    // for that target will carry, so a record inspected after
                    // remapping never points at a pi id that no longer exists
                    // in the exported chain.
                    data.insert(
                        "fromId".to_owned(),
                        Value::String(super::export::stable_pi_id(translated.as_i64())),
                    );
                }
            }
        }
        for value in self.entry_ids.values_mut() {
            *value = translate(*value);
        }
        let labels = std::mem::take(&mut self.labels);
        self.labels = labels
            .into_iter()
            .map(|(target, label)| (translate(target), label))
            .collect();
        Ok(())
    }
}

/// Why a pi session file could not be imported.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The file could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// The file being read.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// A line was not valid JSON.
    #[error("line {line}: {message}")]
    Json {
        /// 1-based line number.
        line: usize,
        /// The parser's message.
        message: String,
    },
    /// The first line was not a `session` header.
    #[error("session file has no `session` header as its first line")]
    MissingHeader,
    /// The file is not a version-3 session.
    #[error("unsupported session version {found:?}; this importer reads version {supported}")]
    UnsupportedVersion {
        /// The version found in the header (`None` for v1 files).
        found: Option<u32>,
        /// The version this importer supports.
        supported: u32,
    },
    /// The caller-supplied minted ids do not match the drafts one-for-one.
    #[error("remapping entry ids needs one minted id per draft: {drafts} drafts, {minted} ids")]
    RemapLength {
        /// The number of drafts the import produced.
        drafts: usize,
        /// The number of ids the caller supplied.
        minted: usize,
    },
}

/// Read and map one pi session file.
pub fn read_session_file(path: &Path) -> Result<ImportedSession, ImportError> {
    let text = std::fs::read_to_string(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    import_session_text(&text)
}

/// [`read_session_file`] over an in-memory session file body.
pub fn import_session_text(text: &str) -> Result<ImportedSession, ImportError> {
    let mut lines = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());

    let (_, header_line) = lines.next().ok_or(ImportError::MissingHeader)?;
    let header_value: Value =
        serde_json::from_str(header_line).map_err(|error| ImportError::Json {
            line: 1,
            message: error.to_string(),
        })?;
    if header_value.get("type").and_then(Value::as_str) != Some("session") {
        return Err(ImportError::MissingHeader);
    }
    let header: SessionHeader =
        serde_json::from_value(header_value).map_err(|error| ImportError::Json {
            line: 1,
            message: error.to_string(),
        })?;
    if header.version != Some(CURRENT_SESSION_VERSION) {
        return Err(ImportError::UnsupportedVersion {
            found: header.version,
            supported: CURRENT_SESSION_VERSION,
        });
    }

    let mut importer = Importer::default();
    for (index, line) in lines {
        let line_number = index + 1;
        let value: Value = serde_json::from_str(line).map_err(|error| ImportError::Json {
            line: line_number,
            message: error.to_string(),
        })?;
        importer.absorb(value);
    }

    Ok(ImportedSession {
        header,
        conversation: ConversationRecord {
            id: ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        },
        entries: importer.entries,
        entry_ids: importer.entry_ids,
        agent: importer.agent,
        usage: importer.usage,
        labels: importer.labels,
        opaque: importer.opaque,
        session_info: importer.session_info,
    })
}

#[derive(Default)]
struct Importer {
    entries: Vec<EntryDraft>,
    entry_ids: BTreeMap<String, EntryId>,
    agent: Map<String, Value>,
    usage: Map<String, Value>,
    labels: BTreeMap<EntryId, String>,
    opaque: Vec<OpaqueEntry>,
    session_info: Option<String>,
}

impl Importer {
    fn absorb(&mut self, value: Value) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // A known type that fails its typed decode is still preserved, never
        // dropped: pi writes fields this port may not know yet.
        let decoded: Option<FileEntry> = serde_json::from_value(value.clone()).ok();
        let Some(entry) = decoded else {
            self.push_opaque(&kind, value);
            return;
        };

        match entry {
            FileEntry::Session(_) => self.push_opaque(&kind, value),
            FileEntry::Message(MessageEntry {
                id,
                message,
                timestamp,
                ..
            }) => {
                self.next_id(&id);
                let kind = message_kind(&message);
                // A `role: "custom"` message is a transcript message, not an
                // extension `custom` entry. Mark it so the export re-emits the
                // same `message` line instead of an empty custom entry.
                let data = matches!(message, StoredMessage::Custom(_))
                    .then(|| serde_json::json!({ "source": "message", "timestamp": timestamp }));
                self.entries.push(EntryDraft {
                    kind: kind.to_owned(),
                    model: Some(vec![serde_json::to_value(&message).unwrap_or(Value::Null)]),
                    data,
                    head: None,
                    edits: None,
                });
            }
            FileEntry::Compaction(inner) => self.absorb_compaction(inner),
            FileEntry::BranchSummary(inner) => self.absorb_branch_summary(inner),
            FileEntry::ContextEdit(inner) => self.absorb_context_edit(inner, value),
            FileEntry::Label(inner) => self.absorb_label(inner, value),
            FileEntry::SessionInfo(inner) => {
                self.session_info = inner.name;
            }
            FileEntry::ModelChange(inner) => self.absorb_model_change(inner),
            FileEntry::ThinkingLevelChange(inner) => {
                self.agent.insert(
                    "thinkingLevel".to_owned(),
                    Value::String(inner.thinking_level),
                );
            }
            FileEntry::Usage(inner) => self.absorb_usage(inner),
            FileEntry::Custom(inner) => self.absorb_custom(inner),
            FileEntry::CustomMessage(inner) => self.absorb_custom_message(inner),
        }
    }

    fn push_opaque(&mut self, kind: &str, value: Value) {
        self.opaque.push(OpaqueEntry {
            id: value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            parent_id: value
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            timestamp: value
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            kind: kind.to_owned(),
            // The export emits a linear chain, so an opaque line only needs its
            // place among the entry drafts to be re-emitted at the same point.
            position: self.entries.len(),
            raw: value,
        });
    }

    /// Assign the next provisional id to one pi id and return it.
    fn next_id(&mut self, pi_id: &str) -> EntryId {
        let provisional = EntryId::from_raw(self.entries.len() as i64 + 1);
        if !pi_id.is_empty() {
            self.entry_ids.insert(pi_id.to_owned(), provisional);
        }
        provisional
    }

    fn absorb_compaction(&mut self, inner: CompactionEntry) {
        self.next_id(&inner.id);
        let head = if inner.first_kept_entry_id == inner.id {
            Some(HeadTarget::SelfEntry)
        } else {
            self.entry_ids
                .get(&inner.first_kept_entry_id)
                .copied()
                .map(HeadTarget::Entry)
        };
        let mut data = Map::new();
        data.insert("kind".to_owned(), Value::String("compaction".to_owned()));
        data.insert("summary".to_owned(), Value::String(inner.summary.clone()));
        data.insert("tokensBefore".to_owned(), Value::from(inner.tokens_before));
        data.insert(
            "timestamp".to_owned(),
            Value::String(inner.timestamp.clone()),
        );
        insert_optional(&mut data, "details", inner.details);
        insert_optional(
            &mut data,
            "usage",
            inner
                .usage
                .and_then(|usage| serde_json::to_value(usage).ok()),
        );
        insert_optional(&mut data, "fromHook", inner.from_hook.map(Value::Bool));
        insert_optional(
            &mut data,
            "systemMessage",
            inner
                .system_message
                .and_then(|system| serde_json::to_value(system).ok()),
        );
        let timestamp = timestamp_ms(&inner.timestamp);
        self.entries.push(EntryDraft {
            kind: "pi.compaction".to_owned(),
            model: Some(vec![framed_summary(
                crate::session_manager::projection::COMPACTION_SUMMARY_PREFIX,
                &inner.summary,
                crate::session_manager::projection::COMPACTION_SUMMARY_SUFFIX,
                timestamp,
            )]),
            data: Some(Value::Object(data)),
            head,
            edits: None,
        });
    }

    fn absorb_branch_summary(&mut self, inner: BranchSummaryEntry) {
        self.next_id(&inner.id);
        let mut data = Map::new();
        data.insert("kind".to_owned(), Value::String("branch".to_owned()));
        data.insert("fromId".to_owned(), Value::String(inner.from_id.clone()));
        // The durable side addresses entries by id, so keep the resolved
        // provisional target next to the raw pi id: the export maps it back
        // through the minted ids instead of emitting a pi id that no exported
        // line carries any more.
        if let Some(target) = self.entry_ids.get(&inner.from_id).copied() {
            data.insert("fromEntry".to_owned(), Value::from(target.as_i64()));
        }
        data.insert(
            "timestamp".to_owned(),
            Value::String(inner.timestamp.clone()),
        );
        data.insert("summary".to_owned(), Value::String(inner.summary.clone()));
        insert_optional(&mut data, "details", inner.details);
        insert_optional(
            &mut data,
            "usage",
            inner
                .usage
                .and_then(|usage| serde_json::to_value(usage).ok()),
        );
        insert_optional(&mut data, "fromHook", inner.from_hook.map(Value::Bool));
        let timestamp = timestamp_ms(&inner.timestamp);
        self.entries.push(EntryDraft {
            kind: "pi.compaction".to_owned(),
            model: Some(vec![framed_summary(
                crate::session_manager::projection::BRANCH_SUMMARY_PREFIX,
                &inner.summary,
                crate::session_manager::projection::BRANCH_SUMMARY_SUFFIX,
                timestamp,
            )]),
            data: Some(Value::Object(data)),
            head: None,
            edits: None,
        });
    }

    fn absorb_context_edit(&mut self, inner: ContextEditEntry, raw: Value) {
        let Some(target) = self.entry_ids.get(&inner.target_id).copied() else {
            // A target that never mapped cannot be expressed in durable's
            // edit model; keep the line verbatim (unknown fields included)
            // instead of dropping it.
            self.push_opaque("context_edit", raw);
            return;
        };
        self.next_id(&inner.id);
        let action = match inner.replacement {
            None => ContextEditAction::Omit,
            Some(replacement) => ContextEditAction::Replace {
                messages: vec![replacement.content],
            },
        };
        self.entries.push(EntryDraft {
            kind: "pi.context-edit".to_owned(),
            model: None,
            data: Some(serde_json::json!({ "timestamp": inner.timestamp })),
            head: None,
            edits: Some(vec![ContextEdit { target, action }]),
        });
    }

    fn absorb_label(&mut self, inner: LabelEntry, raw: Value) {
        let Some(target) = self.entry_ids.get(&inner.target_id).copied() else {
            // Keep the line verbatim (unknown fields included).
            self.push_opaque("label", raw);
            return;
        };
        match inner.label {
            Some(label) => {
                self.labels.insert(target, label);
            }
            None => {
                self.labels.remove(&target);
            }
        }
    }

    fn absorb_model_change(&mut self, inner: ModelChangeEntry) {
        self.agent.insert(
            "model".to_owned(),
            serde_json::json!({ "provider": inner.provider, "modelId": inner.model_id }),
        );
    }

    fn absorb_usage(&mut self, inner: UsageEntry) {
        let Ok(value) = serde_json::to_value(inner.usage) else {
            return;
        };
        accumulate_numbers(&mut self.usage, &value);
    }

    fn absorb_custom(&mut self, inner: CustomEntry) {
        self.next_id(&inner.id);
        let mut data = Map::new();
        data.insert("source".to_owned(), Value::String("custom".to_owned()));
        data.insert("customType".to_owned(), Value::String(inner.custom_type));
        data.insert(
            "timestamp".to_owned(),
            Value::String(inner.timestamp.clone()),
        );
        insert_optional(&mut data, "data", inner.data);
        self.entries.push(EntryDraft {
            kind: "pi.custom".to_owned(),
            model: None,
            data: Some(Value::Object(data)),
            head: None,
            edits: None,
        });
    }

    fn absorb_custom_message(&mut self, inner: CustomMessageEntry) {
        self.next_id(&inner.id);
        let mut data = Map::new();
        data.insert(
            "source".to_owned(),
            Value::String("custom_message".to_owned()),
        );
        data.insert("customType".to_owned(), Value::String(inner.custom_type));
        data.insert("display".to_owned(), Value::Bool(inner.display));
        data.insert(
            "timestamp".to_owned(),
            Value::String(inner.timestamp.clone()),
        );
        // The projection's text form is what `model` carries; the original
        // content keeps its full shape (images and other blocks included) here
        // so an export does not have to flatten it again.
        data.insert("content".to_owned(), inner.content.clone());
        insert_optional(&mut data, "details", inner.details);
        let timestamp = timestamp_ms(&inner.timestamp);
        self.entries.push(EntryDraft {
            kind: "pi.custom".to_owned(),
            model: Some(vec![serde_json::json!({
                "role": "user",
                "content": content_blocks_of(&inner.content),
                "timestamp": timestamp,
            })]),
            data: Some(Value::Object(data)),
            head: None,
            edits: None,
        });
    }
}

/// The content blocks of a custom message, keeping every block type.
///
/// A plain string becomes one text block; a block array passes through
/// unchanged. Unlike the text-only projection form this preserves images, so
/// the exported `custom_message` line carries the same content it imported.
fn content_blocks_of(content: &Value) -> Value {
    match content {
        Value::String(text) => serde_json::json!([{ "type": "text", "text": text }]),
        Value::Array(_) => content.clone(),
        _ => serde_json::json!([]),
    }
}

fn message_kind(message: &StoredMessage) -> &'static str {
    match message {
        StoredMessage::Assistant(_) => "pi.assistant",
        StoredMessage::ToolResult(_) => "pi.tool-result",
        StoredMessage::System(_) => "pi.system",
        StoredMessage::BashExecution(_) => "pi.bash-execution",
        StoredMessage::Custom(_) => "pi.custom",
        StoredMessage::User(user) => match user.role {
            crate::ai::types::UserRole::User => "pi.user",
            crate::ai::types::UserRole::System | crate::ai::types::UserRole::Developer => {
                "pi.system"
            }
        },
    }
}

fn insert_optional(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value);
    }
}

fn timestamp_ms(timestamp: &str) -> u64 {
    crate::session_manager::types::ms_from_iso(timestamp).unwrap_or(0)
}

/// The framed user-text summary message pi's projection uses for compaction and
/// branch-summary entries.
fn framed_summary(prefix: &str, summary: &str, suffix: &str, timestamp: u64) -> Value {
    serde_json::json!({
        "role": "user",
        "content": [{ "type": "text", "text": format!("{prefix}{summary}{suffix}") }],
        "timestamp": timestamp,
    })
}

/// Add every numeric field of `source` into `target`, recursively (objects
/// only; arrays and strings are ignored). Used for `pi.usage` accumulation.
fn accumulate_numbers(target: &mut Map<String, Value>, source: &Value) {
    let Value::Object(source) = source else {
        return;
    };
    for (key, value) in source {
        match value {
            Value::Number(number) => {
                // Keep integral counters integral: the pi `usage` entry's
                // counters deserialize as u64, and a float (`3.0`) would make
                // the whole line unparsable on the way back in.
                // Read the current value BEFORE inserting (the borrow ends
                // here). Both forms are kept: the integral one because the pi
                // `usage` counters are u64, the float one for `cost`.
                let (current_u64, current_f64) = match target.get(key) {
                    Some(current) => (current.as_u64(), current.as_f64().unwrap_or(0.0)),
                    None => (None, 0.0),
                };
                let sum = match (current_u64, number.as_u64()) {
                    (current, Some(delta)) => {
                        Value::from(current.unwrap_or(0).saturating_add(delta))
                    }
                    (_, None) => Value::from(current_f64 + number.as_f64().unwrap_or(0.0)),
                };
                target.insert(key.clone(), sum);
            }
            Value::Object(_) => {
                let entry = target
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(nested) = entry {
                    accumulate_numbers(nested, value);
                }
            }
            _ => {}
        }
    }
}
