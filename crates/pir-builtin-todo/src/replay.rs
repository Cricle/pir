//! Transcript replay: rebuild the last task snapshot from session entries.
//!
//! Replay is two-phase. Phase one walks the entries once and computes a cheap
//! fingerprint plus the index of the last matching `todo` tool result, without
//! deserializing anything. Phase two deserializes only that last snapshot
//! (last-write-wins) when the fingerprint moved.

use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::constants::TOOL_NAME;
use crate::types::{Task, TodoState};

/// One replay scan over the transcript entries.
#[derive(Debug, Clone)]
pub struct ReplayScan {
    /// Cheap change detector over the entry count, match count, and the
    /// identity plus structural hash of the last matching snapshot.
    pub fingerprint: u64,
    /// The last entry whose `details` parsed as a task snapshot.
    pub state: Option<TodoState>,
    /// How many entries were examined (the cost counter).
    pub entries_visited: u64,
    /// How many `todo` tool results were seen.
    pub matches: u64,
    /// How many task snapshots were deserialized for [`Self::state`].
    pub deserializations: u64,
}

/// Cheap first phase: the fingerprint plus the index of the LAST matching
/// result, with no deserialization and no allocation.
#[derive(Debug, Clone)]
pub(crate) struct ReplayIndex {
    pub(crate) fingerprint: u64,
    pub(crate) last_match: Option<usize>,
    pub(crate) entries_visited: u64,
    pub(crate) matches: u64,
}

/// FNV-1a over bytes (the fingerprint hash).
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FNV-1a over one byte (kind tags for the structural JSON hash).
fn fnv1a_byte(byte: u8, hash: u64) -> u64 {
    fnv1a(&[byte], hash)
}

/// Allocation-free structural hash of a JSON value for the change detector.
/// It only needs to move when the snapshot's meaning moves; tagging the JSON
/// kinds keeps `1`/`"1"`/`true` distinct.
fn hash_json_value(value: &JsonValue, mut hash: u64) -> u64 {
    match value {
        JsonValue::Null => fnv1a_byte(b'n', hash),
        JsonValue::Bool(flag) => fnv1a_byte(if *flag { b't' } else { b'f' }, hash),
        JsonValue::Number(number) => {
            if let Some(value) = number.as_u64() {
                hash = fnv1a_byte(b'u', hash);
                fnv1a(&value.to_le_bytes(), hash)
            } else if let Some(value) = number.as_i64() {
                hash = fnv1a_byte(b'i', hash);
                fnv1a(&value.to_le_bytes(), hash)
            } else if let Some(value) = number.as_f64() {
                hash = fnv1a_byte(b'd', hash);
                fnv1a(&value.to_bits().to_le_bytes(), hash)
            } else {
                fnv1a_byte(b'?', hash)
            }
        }
        JsonValue::String(text) => {
            hash = fnv1a_byte(b's', hash);
            fnv1a(text.as_bytes(), hash)
        }
        JsonValue::Array(items) => {
            hash = fnv1a_byte(b'[', hash);
            for item in items {
                hash = hash_json_value(item, hash);
            }
            fnv1a_byte(b']', hash)
        }
        JsonValue::Object(map) => {
            hash = fnv1a_byte(b'{', hash);
            for (key, item) in map {
                hash = fnv1a_byte(b'k', hash);
                hash = fnv1a(key.as_bytes(), hash);
                hash = hash_json_value(item, hash);
            }
            fnv1a_byte(b'}', hash)
        }
    }
}

/// Whether `details` matches the persisted snapshot shape (a `tasks` array
/// plus a numeric `nextId`).
fn is_task_details(value: &JsonValue) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("tasks").is_some_and(JsonValue::is_array)
        && object.get("nextId").is_some_and(JsonValue::is_u64)
}

/// Parse one `details` envelope into a task snapshot. `None` for anything
/// else.
pub fn snapshot_from_details(details: &JsonValue) -> Option<TodoState> {
    if !is_task_details(details) {
        return None;
    }
    // Deserialize straight from the borrowed envelope: `&Value` implements
    // `Deserializer`, so a replayed snapshot does not clone the whole array.
    let tasks = Vec::<Task>::deserialize(details.get("tasks")?).ok()?;
    let next_id = details.get("nextId").and_then(JsonValue::as_u64)?;
    Some(TodoState { tasks, next_id })
}

/// Whether a transcript entry is a `todo` tool result carrying a snapshot.
fn is_todo_result(object: &serde_json::Map<String, JsonValue>) -> bool {
    object.get("role").and_then(JsonValue::as_str) == Some("toolResult")
        && object.get("toolName").and_then(JsonValue::as_str) == Some(TOOL_NAME)
        && object.get("details").is_some_and(is_task_details)
}

/// Phase one: walk the entries once, compute the fingerprint, and remember
/// the LAST matching snapshot. No snapshot is deserialized here.
pub(crate) fn scan_entries(entries: &[JsonValue]) -> ReplayIndex {
    let mut matches = 0_u64;
    let mut last_match: Option<usize> = None;
    for (index, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            continue;
        };
        if !is_todo_result(object) {
            continue;
        }
        matches += 1;
        last_match = Some(index);
    }
    let mut hash = fnv1a(&(entries.len() as u64).to_le_bytes(), 0xcbf2_9ce4_8422_2325);
    hash = fnv1a(&matches.to_le_bytes(), fnv1a_byte(b'm', hash));
    if let Some(object) = last_match.and_then(|index| entries[index].as_object()) {
        hash = match object.get("toolCallId").and_then(JsonValue::as_str) {
            Some(call_id) => fnv1a(call_id.as_bytes(), fnv1a_byte(b'c', hash)),
            None => fnv1a_byte(b'C', hash),
        };
        hash = match object.get("timestamp").and_then(JsonValue::as_u64) {
            Some(timestamp) => fnv1a(&timestamp.to_le_bytes(), fnv1a_byte(b't', hash)),
            None => fnv1a_byte(b'T', hash),
        };
        if let Some(details) = object.get("details") {
            hash = hash_json_value(details, hash);
        }
    }
    ReplayIndex {
        fingerprint: hash,
        last_match,
        entries_visited: entries.len() as u64,
        matches,
    }
}

/// Phase two: deserialize the last matching snapshot, walking backwards only
/// if that entry is shape-valid but malformed. Returns the snapshot and the
/// number of deserialization attempts, so callers can expose the cost.
pub(crate) fn snapshot_from_last_match(
    entries: &[JsonValue],
    last_match: Option<usize>,
) -> (Option<TodoState>, u64) {
    let Some(last) = last_match else {
        return (None, 0);
    };
    let mut deserializations = 0_u64;
    for entry in entries[..=last].iter().rev() {
        let Some(object) = entry.as_object() else {
            continue;
        };
        if !is_todo_result(object) {
            continue;
        }
        let Some(details) = object.get("details") else {
            continue;
        };
        deserializations += 1;
        if let Some(snapshot) = snapshot_from_details(details) {
            return (Some(snapshot), deserializations);
        }
    }
    (None, deserializations)
}

/// Two-phase replay for one-shot callers: the cheap fingerprint pass, then
/// the LAST matching snapshot (last-write-wins).
///
/// [`TodoStore::replay`](crate::store::TodoStore::replay) instead compares the
/// fingerprint first and calls the phase-two helper only when it moved, so an
/// unchanged re-push performs zero deserializations.
pub fn replay_from_entries(entries: &[JsonValue]) -> ReplayScan {
    let index = scan_entries(entries);
    let (state, deserializations) = snapshot_from_last_match(entries, index.last_match);
    ReplayScan {
        fingerprint: index.fingerprint,
        state,
        entries_visited: index.entries_visited,
        matches: index.matches,
        deserializations,
    }
}

/// The phase-one fingerprint alone (exposed for hosts that cache by push).
pub fn entries_fingerprint(entries: &[JsonValue]) -> u64 {
    scan_entries(entries).fingerprint
}
