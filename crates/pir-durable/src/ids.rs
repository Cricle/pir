//! Branded record identifiers and commit sequences.
//!
//! Upstream (`src/ids.ts`) brands plain numbers with a phantom type. Rust gets
//! the same guarantee from a newtype carrying a zero-sized kind marker: an
//! `EntryId` cannot be passed where a `ConversationId` is expected, while the
//! wire form stays the bare JSON number the TypeScript storage writes.
//!
//! `Id<K>` is `i64` backed rather than `u64`: the upstream value is a JavaScript
//! safe integer (signed) and cursors subtract one from it, so the signed domain
//! keeps the arithmetic identical without extra casts.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::marker::PhantomData;

/// The record family an [`Id`] belongs to.
pub trait IdKind: 'static {
    /// Stable name used in diagnostics.
    const NAME: &'static str;
}

/// A branded numeric identifier. Serializes as its bare number.
///
/// `PhantomData<fn() -> K>` keeps the marker out of the auto-trait surface, so
/// every `Id<K>` is `Send`, `Sync` and `Copy` regardless of `K`. The trait
/// impls are written by hand for the same reason: a derive would add
/// `K: Clone`-style bounds to the generated impls.
pub struct Id<K: IdKind> {
    value: i64,
    _kind: PhantomData<fn() -> K>,
}

impl<K: IdKind> fmt::Debug for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", K::NAME, self.value)
    }
}

impl<K: IdKind> Clone for Id<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: IdKind> Copy for Id<K> {}

impl<K: IdKind> Default for Id<K> {
    fn default() -> Self {
        Self::from_raw(0)
    }
}

impl<K: IdKind> PartialEq for Id<K> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<K: IdKind> Eq for Id<K> {}

impl<K: IdKind> PartialOrd for Id<K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: IdKind> Ord for Id<K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<K: IdKind> std::hash::Hash for Id<K> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<K: IdKind> Id<K> {
    /// Wrap a trusted numeric allocation or decoding boundary value.
    pub const fn from_raw(value: i64) -> Self {
        Self {
            value,
            _kind: PhantomData,
        }
    }

    /// The underlying number.
    pub const fn as_i64(self) -> i64 {
        self.value
    }

    /// The next identifier in numeric order.
    pub const fn next(self) -> Self {
        Self::from_raw(self.value.saturating_add(1))
    }
}

impl<K: IdKind> fmt::Display for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.value, f)
    }
}

impl<K: IdKind> Serialize for Id<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.value)
    }
}

impl<'de, K: IdKind> Deserialize<'de> for Id<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        i64::deserialize(deserializer).map(Self::from_raw)
    }
}

/// Apply the erased brand at a trusted numeric allocation or decoding boundary.
pub const fn id_from_number<K: IdKind>(value: i64) -> Id<K> {
    Id::from_raw(value)
}

/// Marker for conversation identifiers.
pub struct ConversationIdKind;
impl IdKind for ConversationIdKind {
    const NAME: &'static str = "conversation";
}

/// Marker for entry identifiers.
pub struct EntryIdKind;
impl IdKind for EntryIdKind {
    const NAME: &'static str = "entry";
}

/// Marker for task identifiers.
pub struct TaskIdKind;
impl IdKind for TaskIdKind {
    const NAME: &'static str = "task";
}

/// Marker for submission identifiers.
pub struct SubmissionIdKind;
impl IdKind for SubmissionIdKind {
    const NAME: &'static str = "submission";
}

/// Marker for document incarnation identifiers.
pub struct DocumentIdKind;
impl IdKind for DocumentIdKind {
    const NAME: &'static str = "document";
}

/// Identifier of one conversation.
pub type ConversationId = Id<ConversationIdKind>;
/// Identifier of one transcript entry.
pub type EntryId = Id<EntryIdKind>;
/// Identifier of one durable task.
pub type TaskId = Id<TaskIdKind>;
/// Identifier of one admitted submission.
pub type SubmissionId = Id<SubmissionIdKind>;
/// Identifier of one document incarnation.
pub type DocumentId = Id<DocumentIdKind>;

/// The root conversation always uses this reserved identifier.
pub const ROOT_CONVERSATION_ID: ConversationId = ConversationId::from_raw(1);

/// Strictly increasing sequence assigned to one atomic storage commit.
///
/// Gaps are permitted, matching the upstream contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(i64);

impl Seq {
    /// Wrap a trusted storage sequence.
    pub const fn from_raw(value: i64) -> Self {
        Self(value)
    }

    /// The underlying number.
    pub const fn as_i64(self) -> i64 {
        self.0
    }

    /// The next sequence in numeric order.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for Seq {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for Seq {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        i64::deserialize(deserializer).map(Self::from_raw)
    }
}

/// Apply the erased commit-sequence brand at a trusted storage boundary.
pub const fn seq_from_number(value: i64) -> Seq {
    Seq::from_raw(value)
}
