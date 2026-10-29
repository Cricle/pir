//! On-disk session record types, byte-compatible with upstream pi
//! (`packages/coding-agent/src/core/session-manager.ts`, `CURRENT_SESSION_VERSION = 3`).
//!
//! Layout: line 1 is the session header (`{"type":"session",...}`); every
//! following line is one append-only entry tagged by `type`. Entries form a
//! tree through `id`/`parentId` (ids are 8 hex chars, `parentId: null` on the
//! root). Entry ids, field spellings, and the compaction/usage/context-edit
//! shapes mirror the upstream TypeScript serializer field-for-field. Lines
//! that fail to deserialize (unknown upstream entry types, malformed JSON)
//! are never silently erased: the raw text is retained (refusing migration
//! rewrites) and carried verbatim through fork, like upstream's
//! keep-unknown-data behavior.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::sync::Arc;

use crate::ai::types::{AssistantMessage, ToolResultMessage, Usage, UserMessage};

/// Session file format version (upstream `CURRENT_SESSION_VERSION`).
pub const CURRENT_SESSION_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// Ids and timestamps
// ---------------------------------------------------------------------------

/// Generate a session entry id: 8 hex chars, like upstream `generateId`
/// (`randomUUID().slice(0, 8)`). Uniqueness is enforced by the caller's
/// collision check against the entry index.
pub fn new_entry_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// Generate a session id in UUIDv7 textual form (upstream `createSessionId`
/// uses `uuidv7`): 48-bit big-endian millisecond timestamp, version 7, RFC
/// 4122 variant, random remainder. Random bits come from `uuid`'s v4 source.
pub fn new_session_id() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let now_ms = crate::ai::util::now_ms();
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    // 48-bit unix-ts-ms.
    out[0] = (now_ms >> 40) as u8;
    out[1] = (now_ms >> 32) as u8;
    out[2] = (now_ms >> 24) as u8;
    out[3] = (now_ms >> 16) as u8;
    out[4] = (now_ms >> 8) as u8;
    out[5] = now_ms as u8;
    // Version 7 + variant 10.
    out[6] = (out[6] & 0x0f) | 0x70;
    out[8] = (out[8] & 0x3f) | 0x80;
    let hex = uuid::Uuid::from_bytes(out).as_hyphenated().to_string();
    hex.to_ascii_lowercase()
}

/// Validate a caller-provided session id (upstream `assertValidSessionId`,
/// regex `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$`): the FIRST and LAST
/// characters must be strictly alphanumeric; separators are only allowed in
/// between (a single character must also be alphanumeric).
pub fn assert_valid_session_id(id: &str) -> crate::Result<()> {
    let valid = |mut chars: std::str::Chars| {
        let first = chars.next();
        let last = chars.next_back();
        let all_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        match (first, last) {
            (Some(f), Some(l)) => all_ok && f.is_ascii_alphanumeric() && l.is_ascii_alphanumeric(),
            (Some(single), None) => single.is_ascii_alphanumeric(),
            _ => false,
        }
    };
    if id.is_empty() || !valid(id.chars()) {
        anyhow::bail!(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', \
             and start and end with an alphanumeric character"
        );
    }
    Ok(())
}

/// Epoch milliseconds → upstream timestamp string (`new Date(ms).toISOString()`):
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, always UTC.
pub fn iso_from_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Parse an upstream ISO timestamp (`YYYY-MM-DDTHH:MM:SS(.mmm)Z`) to epoch
/// milliseconds; returns `None` for anything unparsable (upstream `Date.parse`
/// NaN handling). Clock fields are range-checked (hour ≤ 23, minute/second ≤
/// 59, day valid for the month with leap years) and a fractional second of any
/// 1+ digit length parses with the same weighting as `Date.parse`
/// (`.5` → 500 ms, `.05` → 50 ms, `.123`/`.1234…` → 123 ms).
pub fn ms_from_iso(timestamp: &str) -> Option<u64> {
    let bytes = timestamp.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&bytes[range]).ok()?.parse::<i64>().ok()
    };
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    let millis = if bytes.len() > 19 && bytes[19] == b'.' {
        let fraction = &bytes[20..];
        let digit_count = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digit_count == 0 {
            return None;
        }
        let mut millis = 0u64;
        let weights = [100u64, 10, 1];
        for (index, weight) in weights.iter().enumerate() {
            if index >= digit_count {
                break;
            }
            millis += u64::from(fraction[index] - b'0') * weight;
        }
        millis
    } else {
        0
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // days_from_civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total_ms = (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis as i64;
    if total_ms < 0 {
        return None;
    }
    Some(total_ms as u64)
}

/// Days in a month (Gregorian, proleptic leap-year rule).
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Line 1 of every session file (upstream `SessionHeader`). The `type` tag is
/// injected by [`FileEntry::Session`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// `undefined` on v1 sessions; always [`CURRENT_SESSION_VERSION`] on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    /// Parent session path (`forkFrom` / branched-session exports).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    pub fn new(id: impl Into<String>, cwd: impl Into<String>, timestamp: String) -> Self {
        Self {
            version: Some(CURRENT_SESSION_VERSION),
            id: id.into(),
            timestamp,
            cwd: cwd.into(),
            parent_session: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// One persisted JSONL line (upstream `FileEntry = SessionHeader | SessionEntry`).
/// The header is the `Session` variant; [`FileEntry::is_header`] separates it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FileEntry {
    #[serde(rename = "session")]
    Session(SessionHeader),
    #[serde(rename = "message")]
    Message(MessageEntry),
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    #[serde(rename = "model_change")]
    ModelChange(ModelChangeEntry),
    #[serde(rename = "usage")]
    Usage(UsageEntry),
    #[serde(rename = "compaction")]
    Compaction(CompactionEntry),
    #[serde(rename = "branch_summary")]
    BranchSummary(BranchSummaryEntry),
    #[serde(rename = "custom")]
    Custom(CustomEntry),
    #[serde(rename = "custom_message")]
    CustomMessage(CustomMessageEntry),
    #[serde(rename = "context_edit")]
    ContextEdit(ContextEditEntry),
    #[serde(rename = "label")]
    Label(LabelEntry),
    #[serde(rename = "session_info")]
    SessionInfo(SessionInfoEntry),
}

impl FileEntry {
    pub fn is_header(&self) -> bool {
        matches!(self, FileEntry::Session(_))
    }

    pub fn as_header(&self) -> Option<&SessionHeader> {
        match self {
            FileEntry::Session(header) => Some(header),
            _ => None,
        }
    }

    /// Entry id (`None` on the header).
    pub fn id(&self) -> Option<&str> {
        match self {
            FileEntry::Session(_) => None,
            other => Some(other.entry_id()),
        }
    }

    pub fn entry_id(&self) -> &str {
        match self {
            FileEntry::Session(_) => "",
            FileEntry::Message(entry) => &entry.id,
            FileEntry::ThinkingLevelChange(entry) => &entry.id,
            FileEntry::ModelChange(entry) => &entry.id,
            FileEntry::Usage(entry) => &entry.id,
            FileEntry::Compaction(entry) => &entry.id,
            FileEntry::BranchSummary(entry) => &entry.id,
            FileEntry::Custom(entry) => &entry.id,
            FileEntry::CustomMessage(entry) => &entry.id,
            FileEntry::ContextEdit(entry) => &entry.id,
            FileEntry::Label(entry) => &entry.id,
            FileEntry::SessionInfo(entry) => &entry.id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            FileEntry::Session(_) => None,
            FileEntry::Message(entry) => entry.parent_id.as_deref(),
            FileEntry::ThinkingLevelChange(entry) => entry.parent_id.as_deref(),
            FileEntry::ModelChange(entry) => entry.parent_id.as_deref(),
            FileEntry::Usage(entry) => entry.parent_id.as_deref(),
            FileEntry::Compaction(entry) => entry.parent_id.as_deref(),
            FileEntry::BranchSummary(entry) => entry.parent_id.as_deref(),
            FileEntry::Custom(entry) => entry.parent_id.as_deref(),
            FileEntry::CustomMessage(entry) => entry.parent_id.as_deref(),
            FileEntry::ContextEdit(entry) => entry.parent_id.as_deref(),
            FileEntry::Label(entry) => entry.parent_id.as_deref(),
            FileEntry::SessionInfo(entry) => entry.parent_id.as_deref(),
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            FileEntry::Session(header) => &header.timestamp,
            FileEntry::Message(entry) => &entry.timestamp,
            FileEntry::ThinkingLevelChange(entry) => &entry.timestamp,
            FileEntry::ModelChange(entry) => &entry.timestamp,
            FileEntry::Usage(entry) => &entry.timestamp,
            FileEntry::Compaction(entry) => &entry.timestamp,
            FileEntry::BranchSummary(entry) => &entry.timestamp,
            FileEntry::Custom(entry) => &entry.timestamp,
            FileEntry::CustomMessage(entry) => &entry.timestamp,
            FileEntry::ContextEdit(entry) => &entry.timestamp,
            FileEntry::Label(entry) => &entry.timestamp,
            FileEntry::SessionInfo(entry) => &entry.timestamp,
        }
    }
}

/// Message entry (upstream `SessionMessageEntry`): construction order is
/// `type, id, parentId, timestamp, message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    /// v1 sessions carry no ids; `migrate_to_current_version` fills them.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub message: StoredMessage,
}

/// Thinking-level state entry (upstream `ThinkingLevelChangeEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub thinking_level: String,
}

/// Model state entry (upstream `ModelChangeEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub provider: String,
    pub model_id: String,
}

/// Model-attributed usage that does not participate in LLM context (upstream
/// `UsageEntry`); `kind` includes `"cache_warm"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Compaction checkpoint (upstream `CompactionEntry`). `firstKeptEntryId` is
/// always a string: `null` passed to `appendCompaction` is replaced by the
/// compaction entry's own id (self-retaining cut).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub summary: String,
    /// Self-retaining cut points store the entry's own id; v1 files carry the
    /// numeric `firstKeptEntryIndex` instead (migrated on load).
    #[serde(default)]
    pub first_kept_entry_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "firstKeptEntryIndex"
    )]
    pub first_kept_entry_index: Option<u64>,
    pub tokens_before: u64,
    /// Extension-specific data (e.g. `CompactionDetails` readFiles/modifiedFiles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    /// Usage from the LLM call(s) that generated this summary, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// True if generated by an extension; omitted when pi-generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// Complete prompt/tool state at the compaction boundary (the current
    /// system message), when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<StoredSystemMessage>,
}

/// Branch summary entry (upstream `BranchSummaryEntry`); construction order is
/// `type, id, parentId, timestamp, fromId, summary, details, usage, fromHook`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub from_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Extension state entry; does NOT participate in LLM context (upstream
/// `CustomEntry`); construction order is `type, customType, data, id,
/// parentId, timestamp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
}

/// Extension-injected context message entry (upstream `CustomMessageEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntry {
    pub custom_type: String,
    /// String or `TextContent`/`ImageContent` block array.
    pub content: JsonValue,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
}

/// Append-only edit to one earlier entry's context contribution (upstream
/// `ContextEditEntry`). `replacement: null` omits the target from context;
/// `{content}` replaces only its content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub target_id: String,
    pub replacement: Option<ContextEditReplacement>,
}

/// The `{content}` replacement payload (upstream inline object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEditReplacement {
    /// String or `TextContent`/`ImageContent` block array.
    pub content: JsonValue,
}

/// Label entry (upstream `LabelEntry`); `label` omitted when cleared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Session metadata entry (upstream `SessionInfoEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Stored messages
// ---------------------------------------------------------------------------

/// Upstream `SystemMessage` wire shape: `content` is a plain string (pir's
/// in-memory `Message::System` carries text blocks; the conversion joins the
/// text blocks on load/store).
///
/// `toolsAdded`/`toolsRemoved` are the transcript's tool-declaration record
/// (upstream `declareToolChanges`): a resumed session replays them to restore
/// the active tool loadout (`_restoreToolsFromTranscript`), so both directions
/// of the conversion preserve them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSystemMessage {
    pub role: SystemRole,
    /// Instruction text. Upstream `SystemMessage.content` is
    /// `string | TextContent[]`; a block-array line is read lossily as its
    /// joined text blocks instead of failing the whole entry (an upstream
    /// session must never lose a declaration/checkpoint entry to a shape the
    /// loader could have rendered).
    #[serde(deserialize_with = "deserialize_system_content")]
    pub content: String,
    /// Named prompt sections (upstream `sections`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<JsonValue>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "toolsAdded"
    )]
    pub tools_added: Option<Arc<JsonValue>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "toolsRemoved"
    )]
    pub tools_removed: Option<JsonValue>,
    pub timestamp: u64,
}

/// Role tag for [`StoredSystemMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemRole {
    #[serde(rename = "system")]
    System,
}

/// Read a stored system message's `content` as TEXT: upstream types it
/// `string | TextContent[]`, so a block array is joined on its text blocks
/// (the same lossy reduction pir's `Message -> StoredMessage` conversion
/// applies on write). Any other shape reads as `""` rather than failing the
/// entry; `serde_json::Value` accepts the raw forms upstream can emit.
fn deserialize_system_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = JsonValue::deserialize(deserializer)?;
    Ok(match value {
        JsonValue::String(text) => text,
        JsonValue::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    })
}

/// Role tag for [`StoredCustomMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustomRole {
    #[serde(rename = "custom")]
    Custom,
}

impl StoredSystemMessage {
    pub fn from_text(content: &str, timestamp: u64) -> Self {
        Self {
            role: SystemRole::System,
            content: content.to_owned(),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp,
        }
    }

    /// Build the stored shape from an in-memory system message, keeping the
    /// named sections and the tool-declaration record (upstream
    /// `SystemMessage.sections` / `toolsAdded` / `toolsRemoved`).
    pub fn from_message(message: &crate::ai::types::UserMessage) -> Self {
        Self {
            role: SystemRole::System,
            content: system_text(message),
            sections: message.sections.as_ref().map(sections_to_json),
            tools_added: message
                .tools_added
                .as_ref()
                .and_then(|tools| serde_json::to_value(tools).ok())
                .map(Arc::new),
            tools_removed: message
                .tools_removed
                .as_ref()
                .and_then(|tools| serde_json::to_value(tools).ok()),
            timestamp: message.timestamp,
        }
    }

    /// Replay the stored shape as an in-memory system message: the text, the
    /// named sections (a `null` entry removes a section, upstream
    /// `Record<string, string | null>`), and the tool-declaration record. The
    /// inverse of [`Self::from_message`] for every field.
    pub fn to_message(&self) -> crate::ai::types::Message {
        crate::ai::types::Message::System(crate::ai::types::UserMessage {
            role: crate::ai::types::UserRole::System,
            content: vec![crate::ai::types::ContentBlock::Text(
                crate::ai::types::TextContent {
                    text: self.content.clone(),
                    text_signature: None,
                },
            )],
            tools_added: self
                .tools_added
                .as_ref()
                .and_then(|tools| serde_json::from_value((**tools).clone()).ok()),
            tools_removed: self
                .tools_removed
                .as_ref()
                .and_then(|tools| serde_json::from_value(tools.clone()).ok()),
            sections: self.sections.as_ref().and_then(sections_from_json),
            timestamp: self.timestamp,
        })
    }
}

/// Join a system message's TEXT blocks (upstream `contentText`: text blocks
/// joined with `"\n"`, non-text blocks skipped).
fn system_text(message: &crate::ai::types::UserMessage) -> String {
    use crate::ai::types::ContentBlock;
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Serialize named prompt sections to the upstream wire shape
/// (`Record<string, string | null>`; `None` is the JSON `null` that removes a
/// section during replay).
fn sections_to_json(sections: &std::collections::BTreeMap<String, Option<String>>) -> JsonValue {
    JsonValue::Object(
        sections
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    value
                        .as_ref()
                        .map(|text| JsonValue::String(text.clone()))
                        .unwrap_or(JsonValue::Null),
                )
            })
            .collect(),
    )
}

/// Read the upstream `Record<string, string | null>` section shape; anything
/// that is not an object reads as `None` (no sections).
fn sections_from_json(
    value: &JsonValue,
) -> Option<std::collections::BTreeMap<String, Option<String>>> {
    let object = value.as_object()?;
    Some(
        object
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().map(str::to_owned)))
            .collect(),
    )
}

/// Upstream `CustomMessage` wire shape (role `custom`), stored inline in a
/// message entry. Legacy fields (`customType`, `display`, `details`,
/// `timestamp`) are optional so a v2 `hookMessage` line with any subset of
/// them migrates losslessly: only the role is renamed (upstream
/// `migrateV2ToV3`), everything else is preserved verbatim on rewrite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCustomMessage {
    pub role: CustomRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
    /// String or `TextContent`/`ImageContent` block array.
    pub content: JsonValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

/// Role tag for [`StoredBashExecutionMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BashExecutionRole {
    #[serde(rename = "bashExecution")]
    BashExecution,
}

/// Upstream `BashExecutionMessage` (`core/messages.ts:29-40`): a `!` command's
/// record, stored as a normal message entry with role `bashExecution`.
///
/// `exitCode` is `undefined` when the command never produced one (spawn
/// failure/cancel), so it is omitted from the wire like upstream's
/// `JSON.stringify`. `excludeFromContext` marks a `!!` command: the entry stays
/// in the transcript but is skipped by the model-context projection
/// (`convertToLlm`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredBashExecutionMessage {
    pub role: BashExecutionRole,
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

impl StoredBashExecutionMessage {
    /// Render the record as the user text `convertToLlm` produces (upstream
    /// `bashExecutionToText`, `core/messages.ts:82-98`).
    pub fn to_text(&self) -> String {
        bash_execution_to_text(
            &self.command,
            &self.output,
            self.exit_code,
            self.cancelled,
            self.truncated,
            self.full_output_path.as_deref(),
        )
    }

    /// Whether the record is excluded from the model context (`!!` prefix).
    pub fn excluded(&self) -> bool {
        self.exclude_from_context == Some(true)
    }
}

/// Upstream `bashExecutionToText`: the exact user-text rendering shared by the
/// stored-message conversion and the ephemeral (session-less) transcript.
pub fn bash_execution_to_text(
    command: &str,
    output: &str,
    exit_code: Option<i64>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<&str>,
) -> String {
    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code.filter(|code| *code != 0) {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    if truncated {
        if let Some(path) = full_output_path {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
    }
    text
}

/// A stored agent message, wire-compatible with the upstream `AgentMessage`
/// JSON shapes (user/assistant/toolResult carry pir's `ai::types` payloads
/// verbatim — camelCase, role-tagged; system uses the upstream string-content
/// shape). Untagged because every payload carries its own `role` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum StoredMessage {
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    System(StoredSystemMessage),
    /// Upstream `BashExecutionMessage` (role `bashExecution`): the `!` command
    /// record. Kept distinct so pi sessions round-trip losslessly instead of
    /// landing in `unparsed_raw_lines`.
    BashExecution(StoredBashExecutionMessage),
    /// Upstream `CustomMessage` (role `custom`): v2 `hookMessage` entries
    /// after the v2→v3 migration (upstream renames only the role in place).
    Custom(StoredCustomMessage),
    User(UserMessage),
}

impl StoredMessage {
    /// The message role on the wire (upstream `message.role`).
    pub fn role(&self) -> &'static str {
        match self {
            StoredMessage::Assistant(_) => "assistant",
            StoredMessage::ToolResult(_) => "toolResult",
            StoredMessage::System(_) => "system",
            StoredMessage::BashExecution(_) => "bashExecution",
            StoredMessage::Custom(_) => "custom",
            StoredMessage::User(user) => match user.role {
                crate::ai::types::UserRole::User => "user",
                crate::ai::types::UserRole::System => "system",
                crate::ai::types::UserRole::Developer => "developer",
            },
        }
    }
}

/// The session-write form of one tool result.
///
/// A large shell result reaches this conversion carrying the same body in
/// several places. Measured on a real 5 000-row `bash` run
/// (`scripts/tui_conpty/out/w16k-final/s49_long_output_spill_expand`), one
/// `toolResult` line was 260 559 B:
///
/// | field | bytes | what it is |
/// |---|---|---|
/// | `content` | 51 337 | the model-facing text: retained tail + `[Showing lines …]` footer |
/// | `details.truncation.content` | 51 183 | the same retained tail, footer excluded |
/// | `structuredContent.output` | 140 000 | the FULL stream the model never sees |
///
/// The first two are kept:
///
/// * `content` is load-bearing for every reader — the next provider request
///   (adapters serialize only the content blocks), the TUI body and `ctrl+o`
///   expansion, session replay, `/export`, and compaction.
/// * `details.truncation` is the upstream `BashToolDetails` shape
///   (`packages/coding-agent/src/core/tools/bash.ts:338-356` stores the whole
///   `TruncationResult`, `content` included), so the duplicate is pi-session
///   parity. Readers use its COUNTERS (`truncated`, `truncatedBy`,
///   `outputLines`, `totalLines`, `maxBytes`, `firstLineExceedsLimit`) and
///   `details.fullOutputPath`; none reads `details.truncation.content`.
///
/// `structuredContent` is dropped. It is an execution-time channel for
/// programmatic callers (codemode scripts, `tool_result` handlers, the
/// `tool_execution_end` event), all of which read the live
/// `AgentToolResult` — never a persisted message. Upstream does not put it on
/// the message either: `ToolResultMessage`
/// (`packages/ai/src/types.ts:625-637`) has no such field and
/// `createToolResultMessage` (`packages/agent/src/agent-loop.ts:930-946`)
/// does not copy it, so the session record is content + details only. Keeping
/// it here cost the largest of the three copies (up to
/// `STRUCTURED_OUTPUT_MAX_BYTES`, 1 MiB) with no reader; dropping it makes
/// the pi-format line match upstream and shrinks the measured line to
/// ~120 KB.
fn session_tool_result(mut message: ToolResultMessage) -> ToolResultMessage {
    message.structured_content = None;
    message
}

impl From<crate::ai::types::Message> for StoredMessage {
    fn from(message: crate::ai::types::Message) -> Self {
        use crate::ai::types::Message;
        match message {
            Message::Assistant(inner) => StoredMessage::Assistant(inner),
            Message::ToolResult(inner) => StoredMessage::ToolResult(session_tool_result(inner)),
            Message::User(inner) => StoredMessage::User(inner),
            // Upstream system messages store string content; pir's block
            // content is reduced to its text (text blocks joined with "\n").
            // The sections and the declaration record survive the store
            // (upstream keeps `sections`/`toolsAdded`/`toolsRemoved` on the
            // system message; a resumed session restores its prompt sections
            // and active tool loadout from them).
            Message::System(inner) | Message::Developer(inner) => {
                StoredMessage::System(StoredSystemMessage::from_message(&inner))
            }
        }
    }
}

impl From<StoredMessage> for crate::ai::types::Message {
    fn from(stored: StoredMessage) -> Self {
        use crate::ai::types::{ContentBlock, Message, TextContent, UserRole};
        match stored {
            StoredMessage::Assistant(inner) => Message::Assistant(inner),
            StoredMessage::ToolResult(inner) => Message::ToolResult(inner),
            StoredMessage::User(inner) => match inner.role {
                UserRole::System => Message::System(inner),
                UserRole::Developer => Message::Developer(inner),
                UserRole::User => Message::User(inner),
            },
            StoredMessage::System(inner) => inner.to_message(),
            // Upstream keeps bashExecution messages in LLM context and
            // `convertToLlm` renders them as user text (unless the `!!` flag
            // excludes them; the projection filters those before conversion).
            StoredMessage::BashExecution(inner) => Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: inner.to_text(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                sections: None,
                timestamp: inner.timestamp,
            }),
            // Upstream keeps custom-role messages in LLM context and
            // `convertToLlm` turns them into user messages; pir renders the
            // content directly as user text (same rule as CustomMessageEntry).
            StoredMessage::Custom(inner) => {
                let text = match &inner.content {
                    JsonValue::String(text) => text.clone(),
                    JsonValue::Array(blocks) => blocks
                        .iter()
                        .filter_map(|block| block.get("text").and_then(JsonValue::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                    _ => String::new(),
                };
                Message::User(UserMessage {
                    role: UserRole::User,
                    content: vec![ContentBlock::Text(TextContent {
                        text,
                        text_signature: None,
                    })],
                    tools_added: None,
                    tools_removed: None,
                    sections: None,
                    timestamp: inner.timestamp.unwrap_or(0),
                })
            }
        }
    }
}
