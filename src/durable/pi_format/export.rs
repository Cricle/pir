//! Export durable records into a pi session file (version 3 JSONL).
//!
//! See the module docs of [`super`] for the mapping table. This direction is
//! intentionally conservative: an entry kind without a pi equivalent is
//! emitted as a `custom` entry (kind + data preserved) rather than dropped, so
//! a durable-only record survives a round trip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::durable::ids::EntryId;
use crate::durable::pi_format::import::OpaqueEntry;
use crate::durable::types::{ContextEditAction, EntryRecord};
use crate::session_manager::types::{
    iso_from_ms, BranchSummaryEntry, CompactionEntry, ContextEditEntry, ContextEditReplacement,
    CustomEntry, CustomMessageEntry, FileEntry, LabelEntry, MessageEntry, ModelChangeEntry,
    SessionHeader, SessionInfoEntry, StoredMessage, StoredSystemMessage, ThinkingLevelChangeEntry,
    CURRENT_SESSION_VERSION,
};

/// Everything the exporter needs for one file.
pub struct ExportInput<'a> {
    /// The session header (written as the first line).
    pub header: SessionHeader,
    /// Entries in transcript order.
    pub entries: &'a [EntryRecord],
    /// The `pi.agent` document (emits `model_change` / `thinking_level_change`).
    pub agent: Option<&'a Map<String, Value>>,
    /// The `pi.usage` document (emits one aggregate `usage` entry).
    pub usage: Option<&'a Map<String, Value>>,
    /// The `pi.session` document (its `name` becomes a `session_info` entry).
    pub session_info: Option<&'a Map<String, Value>>,
    /// Labels re-emitted at the tail.
    pub labels: &'a BTreeMap<EntryId, String>,
    /// Lines the import preserved verbatim, re-emitted at their original
    /// position in the chain ([`OpaqueEntry::position`]).
    pub opaque: &'a [OpaqueEntry],
}

/// Why an export could not be rendered or written.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// One entry could not be mapped.
    #[error("entry {id}: {message}")]
    Entry {
        /// The durable entry id.
        id: i64,
        /// What is wrong.
        message: String,
    },
    /// Two durable ids folded to the same 8-hex pi id.
    #[error("pi id collision: entries {first} and {second} both map to {pi_id}")]
    IdCollision {
        /// The first durable id.
        first: i64,
        /// The second durable id.
        second: i64,
        /// The folded pi id.
        pi_id: String,
    },
    /// The file could not be written.
    #[error("failed to write {path}: {source}")]
    Io {
        /// The destination path.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Render one pi session file body.
pub fn render_session(input: &ExportInput<'_>) -> Result<String, ExportError> {
    // Opaque lines keep the pi id they were imported with, so the folded ids
    // entries get must avoid them: a colliding entry would otherwise be emitted
    // under the same id as the opaque line.
    let opaque_ids: std::collections::BTreeSet<String> = input
        .opaque
        .iter()
        .filter(|opaque| !opaque.id.is_empty())
        .map(|opaque| opaque.id.clone())
        .collect();
    let ids = pi_ids(input.entries, &opaque_ids)?;
    let mut out = String::new();
    let header = FileEntry::Session(SessionHeader {
        version: Some(CURRENT_SESSION_VERSION),
        ..input.header.clone()
    });
    push_line(&mut out, &to_value(&header)?);

    // State rows (model/thinking/usage/session_info/labels) need ids too. They
    // must not collide with an entry's folded id, nor with an opaque line that
    // keeps its original pi id, so the allocator is seeded with both and hands
    // out only free ids. Deterministic for a given input.
    let mut ids_in_use: std::collections::BTreeSet<String> = ids.values().cloned().collect();
    for opaque in input.opaque {
        if !opaque.id.is_empty() {
            ids_in_use.insert(opaque.id.clone());
        }
    }
    let entry_ids: std::collections::BTreeSet<String> = ids.values().cloned().collect();
    let mut allocator = IdAllocator {
        used: ids_in_use,
        entry_ids,
        claimed: std::collections::BTreeSet::new(),
        sequence: 0,
    };

    let mut previous: Option<String> = None;
    let push = |value: Value, previous: &mut Option<String>, out: &mut String| {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut value = value;
        if let Value::Object(map) = &mut value {
            map.insert(
                "parentId".to_owned(),
                previous.clone().map(Value::String).unwrap_or(Value::Null),
            );
            // Keep `type`/`id` first for readability; field order is not part
            // of the contract (pi parses the object).
            let mut ordered = Map::new();
            if let Some(kind) = map.remove("type") {
                ordered.insert("type".to_owned(), kind);
            }
            if let Some(id) = map.remove("id") {
                ordered.insert("id".to_owned(), id);
            }
            if let Some(parent) = map.remove("parentId") {
                ordered.insert("parentId".to_owned(), parent);
            }
            ordered.extend(std::mem::take(map));
            *map = ordered;
        }
        push_line(out, &value);
        *previous = Some(id);
    };

    // Opaque lines carry their (linear) position among the entry drafts.
    let mut opaque_by_position: BTreeMap<usize, Vec<&OpaqueEntry>> = BTreeMap::new();
    for opaque in input.opaque {
        opaque_by_position
            .entry(opaque.position)
            .or_default()
            .push(opaque);
    }
    let emit_opaque = |position: usize,
                       opaque_by_position: &mut BTreeMap<usize, Vec<&OpaqueEntry>>,
                       allocator: &mut IdAllocator,
                       previous: &mut Option<String>,
                       out: &mut String| {
        let Some(entries) = opaque_by_position.remove(&position) else {
            return;
        };
        for opaque in entries {
            let value = opaque_value(opaque, allocator);
            push(value, previous, out);
        }
    };

    for (index, entry) in input.entries.iter().enumerate() {
        emit_opaque(
            index,
            &mut opaque_by_position,
            &mut allocator,
            &mut previous,
            &mut out,
        );
        for value in entry_to_values(entry, input, &ids, &mut allocator)? {
            push(value, &mut previous, &mut out);
        }
    }
    // Opaque lines after the last draft (the common case: an unknown line at
    // the tail of the file).
    for position in opaque_by_position.keys().copied().collect::<Vec<_>>() {
        emit_opaque(
            position,
            &mut opaque_by_position,
            &mut allocator,
            &mut previous,
            &mut out,
        );
    }

    // Live-document state that pi stores as transcript entries.
    if let Some(agent) = input.agent {
        if let Some(model) = agent.get("model").and_then(Value::as_object) {
            let value = serde_json::to_value(FileEntry::ModelChange(ModelChangeEntry {
                id: String::new(),
                parent_id: None,
                timestamp: input.header.timestamp.clone(),
                provider: model
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                model_id: model
                    .get("modelId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }))
            .map_err(|error| export_error(0, error))?;
            let value = with_id(value, allocator.next());
            push(value, &mut previous, &mut out);
        }
        if let Some(level) = agent.get("thinkingLevel").and_then(Value::as_str) {
            let value =
                serde_json::to_value(FileEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
                    id: String::new(),
                    parent_id: None,
                    timestamp: input.header.timestamp.clone(),
                    thinking_level: level.to_owned(),
                }))
                .map_err(|error| export_error(0, error))?;
            let value = with_id(value, allocator.next());
            push(value, &mut previous, &mut out);
        }
    }

    if let Some(usage) = input.usage {
        if !usage.is_empty() {
            let value = usage_value(usage, input.header.timestamp.clone(), allocator.next());
            push(value, &mut previous, &mut out);
        }
    }

    if let Some(session_info) = input.session_info {
        if let Some(name) = session_info.get("name").and_then(Value::as_str) {
            let value = serde_json::to_value(FileEntry::SessionInfo(SessionInfoEntry {
                id: allocator.next(),
                parent_id: None,
                timestamp: input.header.timestamp.clone(),
                name: Some(name.to_owned()),
            }))
            .map_err(|error| export_error(0, error))?;
            push(with_parent_placeholder(value), &mut previous, &mut out);
        }
    }

    for (target, label) in input.labels {
        let target_id = ids
            .get(&target.as_i64())
            .cloned()
            .unwrap_or_else(|| stable_pi_id(target.as_i64()));
        let value = serde_json::to_value(FileEntry::Label(LabelEntry {
            id: allocator.next(),
            parent_id: None,
            timestamp: input.header.timestamp.clone(),
            target_id,
            label: Some(label.clone()),
        }))
        .map_err(|error| export_error(0, error))?;
        push(with_parent_placeholder(value), &mut previous, &mut out);
    }

    Ok(out)
}

/// Render and write one pi session file.
pub fn write_session(path: &Path, input: &ExportInput<'_>) -> Result<(), ExportError> {
    let body = render_session(input)?;
    std::fs::write(path, body).map_err(|source| ExportError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Stable 8-hex pi id for a durable numeric id: the high and low 32-bit halves
/// are XOR-folded, so ids that differ only above 2^32 still separate.
pub fn stable_pi_id(value: i64) -> String {
    let bits = value as u64;
    let folded = (bits ^ (bits >> 32)) as u32;
    format!("{folded:08x}")
}

fn pi_ids(
    entries: &[EntryRecord],
    reserved: &std::collections::BTreeSet<String>,
) -> Result<BTreeMap<i64, String>, ExportError> {
    let mut ids = BTreeMap::new();
    let mut seen: BTreeMap<String, i64> = BTreeMap::new();
    let mut assigned: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut sequence = 0usize;
    for entry in entries {
        let raw = entry.id.as_i64();
        let mut pi_id = stable_pi_id(raw);
        if seen.contains_key(&pi_id) || reserved.contains(&pi_id) {
            // A fold collision (or a collision with an opaque line's preserved
            // id) is resolved deterministically by allocating the next free id,
            // so every line in the exported file has a distinct id.
            loop {
                sequence += 1;
                let candidate = stable_pi_id(-(sequence as i64) - 1);
                if !seen.contains_key(&candidate)
                    && !assigned.contains(&candidate)
                    && !reserved.contains(&candidate)
                {
                    pi_id = candidate;
                    break;
                }
            }
        }
        seen.insert(pi_id.clone(), raw);
        assigned.insert(pi_id.clone());
        ids.insert(raw, pi_id);
    }
    Ok(ids)
}

fn entry_to_values(
    entry: &EntryRecord,
    input: &ExportInput<'_>,
    ids: &BTreeMap<i64, String>,
    allocator: &mut IdAllocator,
) -> Result<Vec<Value>, ExportError> {
    let pi_id = ids
        .get(&entry.id.as_i64())
        .cloned()
        .unwrap_or_else(|| stable_pi_id(entry.id.as_i64()));
    let data = entry.data.clone().unwrap_or(Value::Null);
    let timestamp = data
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| input.header.timestamp.clone());

    let file_entries: Vec<FileEntry> = match entry.kind.as_str() {
        "pi.user" | "pi.assistant" | "pi.tool-result" | "pi.system" | "pi.bash-execution" => {
            message_entries(entry, &pi_id, allocator, timestamp)?
        }
        "pi.custom" => {
            let source = data.get("source").and_then(Value::as_str).unwrap_or("");
            if source == "custom_message" {
                // The import keeps the original content; only entries written
                // before that field existed fall back to the flattened text.
                let content = data.get("content").cloned().unwrap_or_else(|| {
                    let text = entry
                        .model
                        .as_ref()
                        .and_then(|model| model.first())
                        .and_then(|message| {
                            message
                                .get("content")
                                .and_then(Value::as_array)
                                .and_then(|blocks| blocks.first())
                                .and_then(|block| block.get("text"))
                                .and_then(Value::as_str)
                        })
                        .unwrap_or_default();
                    Value::String(text.to_owned())
                });
                vec![FileEntry::CustomMessage(CustomMessageEntry {
                    custom_type: data
                        .get("customType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    content,
                    display: data.get("display").and_then(Value::as_bool).unwrap_or(true),
                    details: data.get("details").cloned(),
                    id: pi_id.clone(),
                    parent_id: None,
                    timestamp,
                })]
            } else if source == "message" || is_custom_role_message(entry) {
                // A `role: "custom"` transcript message: emit the message line
                // again (the import marked it with `source: "message"`).
                message_entries(entry, &pi_id, allocator, timestamp)?
            } else {
                vec![FileEntry::Custom(CustomEntry {
                    custom_type: data
                        .get("customType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    data: data.get("data").cloned(),
                    id: pi_id.clone(),
                    parent_id: None,
                    timestamp,
                })]
            }
        }
        "pi.compaction" => {
            let branch = data.get("kind").and_then(Value::as_str) == Some("branch");
            if branch {
                // Prefer the resolved target: the import stores the entry's
                // durable id (`fromEntry`) and the export maps it to the id the
                // exported line carries, so the reference never dangles.
                let from_id = data
                    .get("fromEntry")
                    .and_then(Value::as_i64)
                    .and_then(|target| ids.get(&target).cloned())
                    .or_else(|| {
                        data.get("fromId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                vec![FileEntry::BranchSummary(BranchSummaryEntry {
                    id: pi_id.clone(),
                    parent_id: None,
                    timestamp,
                    from_id,
                    summary: data
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    details: data.get("details").cloned(),
                    usage: data
                        .get("usage")
                        .cloned()
                        .and_then(|usage| serde_json::from_value(usage).ok()),
                    from_hook: data.get("fromHook").and_then(Value::as_bool),
                })]
            } else {
                let first_kept = entry
                    .head
                    .map(|head| head.as_i64())
                    .and_then(|head| ids.get(&head).cloned())
                    .unwrap_or_else(|| pi_id.clone());
                vec![FileEntry::Compaction(CompactionEntry {
                    id: pi_id.clone(),
                    parent_id: None,
                    timestamp,
                    summary: data
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    first_kept_entry_id: first_kept,
                    first_kept_entry_index: None,
                    tokens_before: data
                        .get("tokensBefore")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    details: data.get("details").cloned(),
                    usage: data
                        .get("usage")
                        .cloned()
                        .and_then(|usage| serde_json::from_value(usage).ok()),
                    from_hook: data.get("fromHook").and_then(Value::as_bool),
                    system_message: data.get("systemMessage").cloned().and_then(|system| {
                        serde_json::from_value::<StoredSystemMessage>(system).ok()
                    }),
                })]
            }
        }
        "pi.context-edit" => {
            let edits = entry
                .edits
                .as_ref()
                .filter(|edits| !edits.is_empty())
                .ok_or_else(|| export_error(entry.id.as_i64(), "context edit has no edits"))?;
            // A durable entry may carry several edits (the import emits one
            // line per pi edit, so a round trip always has one); emit one line
            // per edit rather than dropping the rest.
            let mut file_entries = Vec::with_capacity(edits.len());
            for (index, edit) in edits.iter().enumerate() {
                let target_id = ids
                    .get(&edit.target.as_i64())
                    .cloned()
                    .unwrap_or_else(|| stable_pi_id(edit.target.as_i64()));
                let replacement = match &edit.action {
                    ContextEditAction::Omit => None,
                    ContextEditAction::Replace { messages } => Some(ContextEditReplacement {
                        content: messages.first().cloned().unwrap_or(Value::Null),
                    }),
                };
                let id = if index == 0 {
                    pi_id.clone()
                } else {
                    allocator.next()
                };
                file_entries.push(FileEntry::ContextEdit(ContextEditEntry {
                    id,
                    parent_id: None,
                    timestamp: timestamp.clone(),
                    target_id,
                    replacement,
                }));
            }
            file_entries
        }
        other => {
            // Durable-only kind: preserve it as a custom entry rather than
            // dropping it (a re-import maps it back to `pi.custom`).
            vec![FileEntry::Custom(CustomEntry {
                custom_type: other.to_owned(),
                data: Some(serde_json::json!({
                    "model": entry.model,
                    "data": entry.data,
                })),
                id: pi_id.clone(),
                parent_id: None,
                timestamp,
            })]
        }
    };

    file_entries
        .iter()
        .map(|file_entry| {
            let value = serde_json::to_value(file_entry)
                .map_err(|error| export_error(entry.id.as_i64(), error))?;
            Ok(with_parent_placeholder(value))
        })
        .collect()
}

/// The `message` lines one durable entry exports.
///
/// A message entry normally carries exactly one model message. Durable records
/// allow several (an entry may contribute more than one message to the context),
/// so every message is emitted as its own line: the first keeps the entry's pi
/// id, the rest get deterministic ids from the state namespace. Dropping the
/// extras would change the model-visible transcript on re-import.
fn message_entries(
    entry: &EntryRecord,
    pi_id: &str,
    allocator: &mut IdAllocator,
    fallback_timestamp: String,
) -> Result<Vec<FileEntry>, ExportError> {
    let messages = entry
        .model
        .as_ref()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| export_error(entry.id.as_i64(), "message entry has no model"))?;
    let mut file_entries = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let stored: StoredMessage = serde_json::from_value(message.clone())
            .map_err(|error| export_error(entry.id.as_i64(), error))?;
        let timestamp = stored_timestamp(&stored)
            .map(iso_from_ms)
            .unwrap_or_else(|| fallback_timestamp.clone());
        let id = if index == 0 {
            pi_id.to_owned()
        } else {
            allocator.next()
        };
        file_entries.push(FileEntry::Message(MessageEntry {
            id,
            parent_id: None,
            timestamp,
            message: stored,
        }));
    }
    Ok(file_entries)
}

/// Whether the entry's first model message is a `role: "custom"` message.
fn is_custom_role_message(entry: &EntryRecord) -> bool {
    entry
        .model
        .as_ref()
        .and_then(|model| model.first())
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("custom")
}

/// Deterministic id allocator for lines that do not own a durable id.
struct IdAllocator {
    /// Every id the file must not reuse: folded entry ids and opaque ids.
    used: std::collections::BTreeSet<String>,
    /// The folded entry ids, so an opaque line that kept one of them can be
    /// moved aside instead of shadowing an entry.
    entry_ids: std::collections::BTreeSet<String>,
    /// Opaque ids already emitted, so two opaque lines never share one.
    claimed: std::collections::BTreeSet<String>,
    sequence: usize,
}

impl IdAllocator {
    /// The next free id in the deterministic state namespace.
    fn next(&mut self) -> String {
        loop {
            self.sequence += 1;
            let candidate = stable_pi_id(-(self.sequence as i64) - 1);
            if self.used.insert(candidate.clone()) {
                return candidate;
            }
        }
    }

    /// An opaque line's id when it is free, otherwise a freshly allocated id.
    ///
    /// The line keeps its original pi id unless another emitted line already
    /// holds it (the same id twice, or an entry whose folded id matches), so an
    /// import/export round trip never produces two lines with one id.
    fn claim_or_next(&mut self, id: &str) -> String {
        if id.is_empty() || self.entry_ids.contains(id) || !self.claimed.insert(id.to_owned()) {
            self.next()
        } else {
            id.to_owned()
        }
    }
}

/// One opaque line, re-emitted with a free id.
///
/// The raw line is preserved verbatim (unknown fields included); its `id` is
/// re-used when the original wrote one.
fn opaque_value(opaque: &OpaqueEntry, allocator: &mut IdAllocator) -> Value {
    let mut value = opaque.raw.clone();
    if let Value::Object(map) = &mut value {
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let id = allocator.claim_or_next(&id);
        map.insert("id".to_owned(), Value::String(id));
    }
    value
}

/// Keep a `parentId: null` placeholder in the object so the writer can chain
/// it (`FileEntry`'s `parent_id` is skipped only when `None`, so force it in).
fn with_parent_placeholder(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.insert("parentId".to_owned(), Value::Null);
    }
    value
}

fn with_id(mut value: Value, id: String) -> Value {
    if let Value::Object(map) = &mut value {
        map.insert("id".to_owned(), Value::String(id));
    }
    with_parent_placeholder(value)
}

fn stored_timestamp(message: &StoredMessage) -> Option<u64> {
    match message {
        StoredMessage::User(user) => Some(user.timestamp),
        StoredMessage::System(system) => Some(system.timestamp),
        StoredMessage::BashExecution(bash) => Some(bash.timestamp),
        StoredMessage::Custom(custom) => custom.timestamp,
        StoredMessage::Assistant(assistant) => Some(assistant.timestamp),
        StoredMessage::ToolResult(result) => Some(result.timestamp),
    }
}

fn usage_value(usage: &Map<String, Value>, timestamp: String, id: String) -> Value {
    // Integral counters must stay integers: `Usage` deserializes them as u64,
    // and `3.0` would fail the whole entry on re-import.
    let number = |key: &str| -> Value {
        match usage.get(key) {
            Some(Value::Number(number)) if number.is_u64() => {
                Value::from(number.as_u64().unwrap_or(0))
            }
            Some(Value::Number(number)) => Value::from(number.as_f64().unwrap_or(0.0)),
            _ => Value::from(0u64),
        }
    };
    let cost = usage.get("cost").and_then(Value::as_object);
    let cost_field = |key: &str| {
        cost.and_then(|cost| cost.get(key))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    serde_json::json!({
        "type": "usage",
        "id": id,
        "parentId": Value::Null,
        "timestamp": timestamp,
        "kind": "aggregate",
        "provider": "",
        "model": "",
        "usage": {
            "input": number("input"),
            "output": number("output"),
            "cacheRead": number("cacheRead"),
            "cacheWrite": number("cacheWrite"),
            "totalTokens": number("totalTokens"),
            "cost": {
                "input": cost_field("input"),
                "output": cost_field("output"),
                "cacheRead": cost_field("cacheRead"),
                "cacheWrite": cost_field("cacheWrite"),
                "total": cost_field("total"),
            },
        },
    })
}

fn to_value(entry: &FileEntry) -> Result<Value, ExportError> {
    serde_json::to_value(entry).map_err(|error| export_error(0, error))
}

fn push_line(out: &mut String, value: &Value) {
    out.push_str(&value.to_string());
    out.push('\n');
}

fn export_error(id: i64, error: impl std::fmt::Display) -> ExportError {
    ExportError::Entry {
        id,
        message: error.to_string(),
    }
}
