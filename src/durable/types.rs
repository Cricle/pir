//! The storage contract and record shapes.
//!
//! This is a deliberate subset of upstream `src/types.ts` (1274 lines). Only the
//! types the entries + storage core can implement are ported:
//!
//! * records: [`ConversationRecord`], [`EntryRecord`], [`TaskRecord`],
//!   [`SubmissionRecord`], [`DocumentRecord`] and their create/draft forms;
//! * document content and commits: [`DocumentContent`], [`DocumentCreate`],
//!   [`DocumentCommitChange`], [`StoredDocument`];
//! * the commit unit: [`StorageWrite`];
//! * paging: [`Page`], [`Cursor`], [`ScanOrder`];
//! * queries: [`ConversationQuery`], [`EntryQuery`], [`TaskQuery`],
//!   [`SubmissionQuery`], [`DocumentQuery`];
//! * the [`Storage`] trait.
//!
//! Skipped upstream types, and why:
//!
//! * `Tx`, `Session`, `DocumentObserver`, `DocumentReader`, `CommitPublication`,
//!   `SessionEnd`, watch handles — they belong to the Session/Harness layer.
//! * `TaskDefinition`, `TaskRuntime`, `PhaseHandler`, `HookRunner`, `Report*`,
//!   `SetTimer`, `HarnessOptions` — the harness is explicitly out of scope.
//! * `ContextEdit` is included because it is part of an entry record.
//! * `Message` comes from `@earendil-works/pi-ai` and is not available in this
//!   crate's durable module; model messages are kept as opaque JSON values.
//!
//! Serialization rules, matching upstream:
//!
//! * **Writes are exact**: the structs serialize with `camelCase` keys and omit
//!   absent optional fields (the TS code uses `JSON.stringify`, which drops
//!   `undefined`).
//! * **Reads are lenient**: unknown fields are ignored by serde's default, so a
//!   log written by a newer producer still opens as long as the fields this
//!   subset understands are valid. Missing optional fields default to `None`.
//! * `value: JsonObject | null` in a retirement publication is represented as
//!   `Option<JsonObject>` and always serialized (as `null`) in
//!   [`DocumentCommitChange::Document`].

use crate::durable::delta::Op;
use crate::durable::errors::StorageError;
use crate::durable::harness::telemetry::TraceCarrier;
use crate::durable::ids::{
    id_from_number, ConversationId, DocumentId, EntryId, Id, IdKind, Seq, SubmissionId, TaskId,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// JSON object used as the root of every durable document.
pub type JsonObject = serde_json::Map<String, serde_json::Value>;

/// Serde for a record's optional trace carrier.
///
/// Carriers persist as one JSON object of string entries. The deserializer also
/// accepts the `[[key, value], …]` pair-array form, so a carrier encoded by an
/// integration that treats [`TraceCarrier::into_entries`] as the wire shape
/// still reads. An absent or `null` field is `None`.
mod trace_carrier_serde {
    use super::TraceCarrier;
    use serde::de::{SeqAccess, Visitor};
    use serde::ser::SerializeMap;
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S>(carrier: &Option<TraceCarrier>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match carrier {
            None => serializer.serialize_none(),
            Some(carrier) => {
                let mut map = serializer.serialize_map(Some(carrier.len()))?;
                for (key, value) in carrier.pairs() {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<TraceCarrier>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(OptionalCarrier)
    }

    struct OptionalCarrier;

    impl<'de> Visitor<'de> for OptionalCarrier {
        type Value = Option<TraceCarrier>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a trace carrier object, a [key, value] pair array, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_any(Carrier)
        }
    }

    struct Carrier;

    impl<'de> Visitor<'de> for Carrier {
        type Value = Option<TraceCarrier>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a trace carrier object or a [key, value] pair array")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut pairs = Vec::with_capacity(map.size_hint().unwrap_or(0));
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                pairs.push((key, value));
            }
            Ok(Some(TraceCarrier::from_pairs(pairs)))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut pairs = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(entry) = seq.next_element::<(String, String)>()? {
                pairs.push(entry);
            }
            Ok(Some(TraceCarrier::from_pairs(pairs)))
        }

        fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Err(E::custom("a trace carrier must be an object or pair array"))
        }
    }
}

/// Serde for a task's immutable creation-time trace links.
///
/// Upstream persists `links?: readonly TelemetryCarrier[]`; each element is a
/// carrier object. The deserializer also accepts the `[[key, value], …]`
/// pair-array form per element, matching [`trace_carrier_serde`], and an absent
/// field defaults to an empty vector.
mod trace_links_serde {
    use super::TraceCarrier;
    use serde::ser::{SerializeMap, SerializeSeq};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(links: &[TraceCarrier], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(links.len()))?;
        for link in links {
            seq.serialize_element(&CarrierRef(link))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<TraceCarrier>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let links = Vec::<CarrierWire>::deserialize(deserializer)?;
        Ok(links
            .into_iter()
            .map(|link| match link {
                CarrierWire::Map(entries) => TraceCarrier::from_pairs(entries),
                CarrierWire::Pairs(entries) => TraceCarrier::from_pairs(entries),
            })
            .collect())
    }

    struct CarrierRef<'a>(&'a TraceCarrier);

    impl Serialize for CarrierRef<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (key, value) in self.0.pairs() {
                map.serialize_entry(key, value)?;
            }
            map.end()
        }
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum CarrierWire {
        Map(BTreeMap<String, String>),
        Pairs(Vec<(String, String)>),
    }
}

// ─── Conversation ───────────────────────────────────────────────────────────

/// Fork source and inclusive parent entry through which history is inherited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationParent {
    /// The conversation history is inherited from.
    pub conversation_id: ConversationId,
    /// The inclusive parent entry.
    pub at: EntryId,
}

/// Creator edge used for attribution, subtree abort and subtree idle waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationOwner {
    /// The owning conversation.
    pub conversation_id: ConversationId,
    /// The owning task.
    pub task_id: TaskId,
}

/// Immutable identity, history ancestry and task ownership of a transcript scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRecord {
    /// The conversation identifier.
    pub id: ConversationId,
    /// Fork source, absent for a root conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationParent>,
    /// Creator edge, absent for an ownerless conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConversationOwner>,
}

// ─── Entries ────────────────────────────────────────────────────────────────

/// An immutable override of one visible entry's contribution to model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEdit {
    /// Entry whose model messages are omitted or replaced.
    pub target: EntryId,
    /// The override itself; tagged with `action`.
    #[serde(flatten)]
    pub action: ContextEditAction,
}

/// The override applied by a [`ContextEdit`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum ContextEditAction {
    /// Omit the target entry's messages.
    Omit,
    /// Replace the target entry's messages.
    Replace {
        /// Messages contributed instead of the target's model messages.
        messages: Vec<serde_json::Value>,
    },
}

/// An immutable transcript event with separate model-facing and
/// application-facing payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRecord {
    /// Entry identifier.
    pub id: EntryId,
    /// Conversation the entry belongs to.
    pub conversation_id: ConversationId,
    /// Application-defined discriminator (see [`crate::durable::entries`]).
    pub kind: String,
    /// Messages contributed to model context; absent for display/bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<serde_json::Value>>,
    /// JSON payload consumed by views, extensions or bookkeeping logic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// First entry in the active context selected by this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryId>,
    /// Context-only overrides of earlier visible entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<ContextEdit>>,
    /// Task that appended this entry, when produced by durable work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_task_id: Option<TaskId>,
}

impl EntryRecord {
    /// Whether this entry selects a new active-context head.
    pub fn is_head(&self) -> bool {
        self.head.is_some()
    }

    /// Deserialize `data` into a typed payload.
    ///
    /// The Rust analogue of upstream's `TypedEntry<D>`: the kind guard proves
    /// the discriminator, and this decodes the payload.
    pub fn typed<D: serde::de::DeserializeOwned>(&self) -> Option<TypedEntry<D>> {
        let data = serde_json::from_value(self.data.clone()?).ok()?;
        Some(TypedEntry {
            record: self.clone(),
            data,
        })
    }
}

/// An entry whose `data` has been decoded into `D`.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedEntry<D> {
    /// The underlying record.
    pub record: EntryRecord,
    /// The decoded payload.
    pub data: D,
}

/// An entry record of any kind.
///
/// Upstream's `Entry` is the token type; in Rust the token is
/// [`EntryKind`](crate::durable::entries::EntryKind) and this alias names the
/// record it guards.
pub type Entry = EntryRecord;

// ─── Tasks ──────────────────────────────────────────────────────────────────

/// JSON-safe error snapshot persisted instead of a runtime error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutcomeError {
    /// Error message.
    pub message: String,
    /// Optional structured diagnostic data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Durable reason and optional result recorded when a task becomes terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskOutcome {
    /// Completed with a result.
    Completed {
        /// The task result.
        result: serde_json::Value,
    },
    /// Expected task or domain failure.
    Failed {
        /// The failure.
        error: TaskOutcomeError,
        /// Optional partial result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },
    /// Explicit cancellation handled by the abort protocol.
    Aborted {
        /// Optional cancellation reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Optional partial result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
    },
    /// The task cannot resume because its definition or migration is unavailable.
    Orphaned {
        /// Why the task is orphaned.
        reason: String,
    },
    /// Runtime-detected contract failure.
    Faulted {
        /// The failure.
        error: TaskOutcomeError,
    },
}

/// How a waiting task treats the tasks it waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    /// Stop at the first failure.
    FailFast,
    /// Wait for every task.
    AllSettled,
}

/// The status discriminator of a [`TaskState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Eligible for scheduling.
    Pending,
    /// Reserved by one in-memory invocation.
    Running,
    /// Parked until every task in `on` is terminal.
    Waiting,
    /// Parked until the harness clock reaches `until`.
    Sleeping,
    /// Outcome decided; waits for owned work to settle.
    Completing,
    /// Permanently settled.
    Terminal,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Sleeping => "sleeping",
            Self::Completing => "completing",
            Self::Terminal => "terminal",
        };
        f.write_str(text)
    }
}

/// Complete durable execution state of a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum TaskState {
    /// Eligible for scheduling.
    Pending {
        /// Complete durable state from which execution resumes.
        checkpoint: serde_json::Value,
    },
    /// Reserved by one in-memory task invocation.
    Running {
        /// Complete durable state from which execution resumes.
        checkpoint: serde_json::Value,
    },
    /// Parked until every task in `on` is terminal.
    Waiting {
        /// Complete durable state from which execution resumes.
        checkpoint: serde_json::Value,
        /// Tasks to wait for.
        on: Vec<TaskId>,
        /// How the wait treats failures.
        policy: JoinPolicy,
    },
    /// Parked until the harness clock reaches `until`.
    Sleeping {
        /// Complete durable state from which execution resumes.
        checkpoint: serde_json::Value,
        /// Wake-up time in milliseconds on the harness clock.
        until: i64,
    },
    /// Outcome decided; runs no more code.
    Completing {
        /// The decided outcome.
        outcome: TaskOutcome,
    },
    /// Permanently settled durable result receipt.
    Terminal {
        /// The settled outcome.
        outcome: TaskOutcome,
    },
}

impl TaskState {
    /// The status discriminator.
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Pending { .. } => TaskStatus::Pending,
            Self::Running { .. } => TaskStatus::Running,
            Self::Waiting { .. } => TaskStatus::Waiting,
            Self::Sleeping { .. } => TaskStatus::Sleeping,
            Self::Completing { .. } => TaskStatus::Completing,
            Self::Terminal { .. } => TaskStatus::Terminal,
        }
    }

    /// Whether the state can no longer run code.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }
}

/// Complete replacement record for one durable task state machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    /// Task identifier.
    pub id: TaskId,
    /// Conversation that owns the task.
    pub conversation_id: ConversationId,
    /// Registered task definition name.
    pub kind: String,
    /// Definition version used to migrate live input and checkpoints.
    pub version: i64,
    /// Original task input, retained while live or terminal.
    pub input: serde_json::Value,
    /// Owning task of a child task; absent for a conversation-owned task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    /// Whether this conversation-owned task is excluded from idle waits and aborts.
    pub background: bool,
    /// Durable abort mark checked before run-mode progress is committed.
    pub abort_requested: bool,
    /// Why the mark was set when not by an abort request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
    /// Set from `TaskOptions.abandonOnRestart`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_on_restart: Option<bool>,
    /// Wall-clock milliseconds of the first change to `running`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// Wall-clock milliseconds of the change to `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// How many consecutive crashes cut the task off while it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<i64>,
    /// The durable state machine.
    pub state: TaskState,
    /// Small first-writer-wins values retained while the task can run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memos: Option<serde_json::Map<String, serde_json::Value>>,
    /// Trace context inherited from the submissions that started the task.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "trace_carrier_serde"
    )]
    pub trace: Option<TraceCarrier>,
    /// Immutable creation-time trace links inherited from the task's other
    /// submissions (upstream `TaskRecordBase.links`). Empty is omitted, and
    /// old JSON without the field reads as empty.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "trace_links_serde"
    )]
    pub links: Vec<TraceCarrier>,
}

impl TaskRecord {
    /// The status discriminator of the current state.
    pub fn status(&self) -> TaskStatus {
        self.state.status()
    }
}

// ─── Submissions ────────────────────────────────────────────────────────────

/// Whether a submission carried user input or a passive write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionType {
    /// User input awaiting an answer.
    Input,
    /// Passive entry write.
    Write,
}

/// Lifecycle status of a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionStatus {
    /// Admitted but not yet represented in the transcript.
    Queued,
    /// Added to the transcript and owned by an active run.
    Placed,
    /// Successfully answered or appended.
    Done,
    /// Terminal and can no longer be answered.
    Unanswered,
}

impl fmt::Display for SubmissionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Queued => "queued",
            Self::Placed => "placed",
            Self::Done => "done",
            Self::Unanswered => "unanswered",
        };
        f.write_str(text)
    }
}

/// Durable lifecycle of one admitted user input or passive entry write.
///
/// Upstream models this as a discriminated union of eight shapes. The Rust
/// struct keeps the same wire keys (`type`, `status`, `entry`, `answer`,
/// `reason`, `detail`) but does not statically forbid impossible combinations;
/// the Session layer is the only writer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionRecord {
    /// Submission identifier.
    pub id: SubmissionId,
    /// Conversation the submission belongs to.
    pub conversation_id: ConversationId,
    /// Host-provided deduplication key, scoped to the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Input or passive write.
    #[serde(rename = "type")]
    pub submission_type: SubmissionType,
    /// Lifecycle status.
    pub status: SubmissionStatus,
    /// Entry the submission placed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    /// Entry that answered the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<EntryId>,
    /// Why a terminal submission could not be placed or answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Structured detail for `reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// Trace context of the admission that created this submission.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "trace_carrier_serde"
    )]
    pub trace: Option<TraceCarrier>,
}

// ─── Documents ──────────────────────────────────────────────────────────────

/// Ownership and lifetime scope of a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocumentScope {
    /// One session-wide singleton/family.
    Session,
    /// A conversation-scoped document.
    Conversation {
        /// Owning conversation.
        #[serde(rename = "conversationId")]
        conversation_id: ConversationId,
    },
    /// A task-scoped document.
    Task {
        /// Owning task.
        #[serde(rename = "taskId")]
        task_id: TaskId,
    },
}

impl DocumentScope {
    /// The scope discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Conversation { .. } => "conversation",
            Self::Task { .. } => "task",
        }
    }

    /// The owning conversation, if any.
    pub fn conversation_id(&self) -> Option<ConversationId> {
        match self {
            Self::Conversation { conversation_id } => Some(*conversation_id),
            Self::Session | Self::Task { .. } => None,
        }
    }
}

/// Whether a conversation document keeps history for as-of reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentHistory {
    /// Retain only current state.
    Latest,
    /// Retain history needed for as-of reads.
    Rewindable,
}

/// How a forked conversation document initializes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentFork {
    /// Initialize at the fork cutoff.
    AsOf,
    /// Initialize from current source state.
    Current,
    /// Initialize from the definition's initial value.
    Initial,
}

/// Persisted lifecycle record for one create-to-retire document incarnation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRecord {
    /// Unique incarnation identifier; never reused.
    pub id: DocumentId,
    /// Stable document definition kind.
    pub kind: String,
    /// Family member key; absent for singleton documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Commit that created the incarnation, stamped by storage.
    pub created_at: Seq,
    /// Commit that retired the incarnation; absent while current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Seq>,
    /// Ownership scope.
    pub scope: DocumentScope,
    /// Conversation history semantics (conversation documents only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    /// Fork semantics (conversation documents only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
}

/// Fields supplied when storage creates and stamps a new [`DocumentRecord`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentCreate {
    /// Incarnation identifier.
    pub id: DocumentId,
    /// Stable document definition kind.
    pub kind: String,
    /// Family member key; absent for singleton documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Ownership scope.
    pub scope: DocumentScope,
    /// Conversation history semantics (conversation documents only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    /// Fork semantics (conversation documents only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
}

impl DocumentCreate {
    /// Whether this incarnation keeps only its current state.
    ///
    /// Matches upstream: anything that is not a conversation document, plus a
    /// conversation document declared `history: "latest"`.
    pub fn is_current_only(&self) -> bool {
        is_current_only_semantics(&self.scope, self.history)
    }
}

impl DocumentRecord {
    /// Whether this incarnation keeps only its current state.
    pub fn is_current_only(&self) -> bool {
        is_current_only_semantics(&self.scope, self.history)
    }
}

fn is_current_only_semantics(scope: &DocumentScope, history: Option<DocumentHistory>) -> bool {
    match scope {
        DocumentScope::Conversation { .. } => history == Some(DocumentHistory::Latest),
        DocumentScope::Session | DocumentScope::Task { .. } => true,
    }
}

/// Complete checkpoint or operation batch selected by the owning Session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DocumentContent {
    /// A complete document value at a definition version.
    Base {
        /// Definition version of `value`.
        version: i64,
        /// The complete value.
        value: JsonObject,
    },
    /// Operations applied to the newest preceding base.
    Delta {
        /// Definition version the operations target.
        version: i64,
        /// The operation batch.
        ops: Vec<Op>,
    },
}

impl DocumentContent {
    /// The definition version this content targets.
    pub fn version(&self) -> i64 {
        match self {
            Self::Base { version, .. } | Self::Delta { version, .. } => *version,
        }
    }

    /// The base value, when this content is a base.
    pub fn as_base(&self) -> Option<&JsonObject> {
        match self {
            Self::Base { value, .. } => Some(value),
            Self::Delta { .. } => None,
        }
    }
}

/// Exact persisted source selected for a definition-free document copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentCopySource {
    /// Source incarnation.
    pub id: DocumentId,
    /// Point in the source incarnation to copy.
    pub at: DocumentPoint,
}

/// Current state or one historical commit sequence.
///
/// On the wire this is a commit number or the literal `"current"`, matching the
/// upstream `number | "current"` union.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentPoint {
    /// The incarnation's current state.
    Current,
    /// The state as of (and including) this commit.
    Seq(Seq),
}

impl From<Seq> for DocumentPoint {
    fn from(seq: Seq) -> Self {
        Self::Seq(seq)
    }
}

impl DocumentPoint {
    /// The sequence bound, if this is a historical read.
    pub fn seq(self) -> Option<Seq> {
        match self {
            Self::Current => None,
            Self::Seq(seq) => Some(seq),
        }
    }
}

impl Serialize for DocumentPoint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Current => serializer.serialize_str("current"),
            Self::Seq(seq) => serializer.serialize_i64(seq.as_i64()),
        }
    }
}

impl<'de> Deserialize<'de> for DocumentPoint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(i64),
            Text(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Number(seq) => Ok(Self::Seq(Seq::from_raw(seq))),
            Repr::Text(text) if text == "current" => Ok(Self::Current),
            Repr::Text(other) => Err(serde::de::Error::custom(format!(
                "invalid document point: {other}"
            ))),
        }
    }
}

/// Detached materialized value and stored definition version at a point.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredDocument {
    /// The incarnation record.
    pub record: DocumentRecord,
    /// Definition version of `value`.
    pub version: i64,
    /// The materialized value.
    pub value: JsonObject,
    /// Deltas replayed after the selected base.
    pub deltas_since_base: usize,
}

/// Committed change of one document incarnation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DocumentCommitChange {
    /// A create, change or retirement.
    #[serde(rename = "document")]
    Document {
        /// The incarnation record (with `retiredAt` when retired).
        record: DocumentRecord,
        /// Conversation owning the document; `None` only for session documents.
        #[serde(
            default,
            rename = "conversationId",
            skip_serializing_if = "Option::is_none"
        )]
        conversation_id: Option<ConversationId>,
        /// Definition version of `value`; absent when this commit retired it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<i64>,
        /// Exact adopted immutable revision, or `null` when retired.
        #[serde(default)]
        value: Option<JsonObject>,
        /// Exact adopted operations; empty for creation and retirement.
        #[serde(default)]
        ops: Vec<Op>,
    },
    /// Definition-free child initialization.
    #[serde(rename = "document.copy")]
    Copy {
        /// The new incarnation record.
        record: DocumentRecord,
        /// Conversation owning the copy.
        ///
        /// The enum-level `rename_all = "camelCase"` applies to variant names
        /// only (serde does not rename variant fields from it), so the field
        /// needs its own rename to match the wire shape of the `Document`
        /// variant above.
        #[serde(rename = "conversationId")]
        conversation_id: ConversationId,
        /// Exact source.
        source: DocumentCopySource,
    },
}

// ─── Writes ─────────────────────────────────────────────────────────────────

/// One record or document mutation in an atomic storage commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
// Wire-shaped public enum: `Box`ing a variant would change every construction
// and match site (and the public API) without changing the serialized bytes;
// a commit batch holds a handful of writes at a time.
#[allow(clippy::large_enum_variant)]
pub enum StorageWrite {
    /// Create or replace a conversation record.
    #[serde(rename = "conversation")]
    Conversation {
        /// The record.
        value: ConversationRecord,
    },
    /// Append an entry.
    #[serde(rename = "entry")]
    Entry {
        /// The record.
        value: EntryRecord,
    },
    /// Create or replace a task record.
    #[serde(rename = "task")]
    Task {
        /// The record.
        value: TaskRecord,
    },
    /// Create or replace a submission record.
    #[serde(rename = "submission")]
    Submission {
        /// The record.
        value: SubmissionRecord,
    },
    /// Create a document incarnation.
    #[serde(rename = "document.create")]
    DocumentCreate {
        /// The incarnation.
        record: DocumentCreate,
        /// The initial content; must be a base (validated at commit).
        content: DocumentContent,
    },
    /// Create a document incarnation from another one.
    #[serde(rename = "document.copy")]
    DocumentCopy {
        /// The new incarnation.
        record: DocumentCreate,
        /// The exact source.
        source: DocumentCopySource,
    },
    /// Apply content to an existing incarnation.
    #[serde(rename = "document.change")]
    DocumentChange {
        /// The incarnation.
        id: DocumentId,
        /// The new content.
        content: DocumentContent,
    },
    /// Retire an existing incarnation.
    #[serde(rename = "document.retire")]
    DocumentRetire {
        /// The incarnation.
        id: DocumentId,
    },
}

impl StorageWrite {
    /// The record family this write targets, for diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Conversation { .. } => "conversation",
            Self::Entry { .. } => "entry",
            Self::Task { .. } => "task",
            Self::Submission { .. } => "submission",
            Self::DocumentCreate { .. } => "document.create",
            Self::DocumentCopy { .. } => "document.copy",
            Self::DocumentChange { .. } => "document.change",
            Self::DocumentRetire { .. } => "document.retire",
        }
    }
}

// ─── Paging and queries ─────────────────────────────────────────────────────

/// ID order of a scan: ascending is oldest first, descending newest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanOrder {
    /// Oldest first.
    #[default]
    Ascending,
    /// Newest first.
    Descending,
}

/// Backend-owned JSON continuation state for a scan.
///
/// Upstream types this as an open `Record<string, JsonValue>` because a cursor
/// is opaque to callers. The Rust subset always writes `{ after, order? }`, so
/// the typed form round-trips exactly the same JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The last identifier a previous page returned.
    pub after: i64,
    /// The order the cursor was created with; absent for a legacy cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

/// One ordered scan result and its optional continuation state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    /// The returned items.
    pub items: Vec<T>,
    /// Continuation state, absent when the scan is exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Cursor>,
}

/// Optional filters for an ordered conversation scan.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationQuery {
    /// Only conversations owned by this conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_conversation_id: Option<ConversationId>,
    /// Only conversations owned by this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_task_id: Option<TaskId>,
    /// Default ascending; with a cursor, the cursor's order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

/// Inclusive ID bounds for a scan of one conversation's fork-aware history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryQuery {
    /// The conversation to scan.
    pub conversation_id: ConversationId,
    /// Oldest entry ID that may be returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_entry_id: Option<EntryId>,
    /// Newest entry ID that may be returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entry_id: Option<EntryId>,
    /// Default descending; with a cursor, the cursor's order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

/// Optional filters for an ordered scan of durable task records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQuery {
    /// Only tasks in this conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    /// Only tasks of this definition kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Only tasks in this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    /// Only tasks with this abort mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_requested: Option<bool>,
    /// Only background (or foreground) tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Default ascending; with a cursor, the cursor's order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

/// Optional filters for an ordered scan of submission records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionQuery {
    /// Only submissions in this conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    /// Only submissions in this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SubmissionStatus>,
    /// Default ascending; with a cursor, the cursor's order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}

/// Exact logical identity of a singleton or one keyed family member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentAddress {
    /// Stable document definition kind.
    pub kind: String,
    /// Ownership scope.
    pub scope: DocumentScope,
    /// Absent selects the singleton; present selects one family member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Ordered scan of document incarnations alive in one exact scope at one point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentQuery {
    /// The scope to scan.
    pub scope: DocumentScope,
    /// The point at which incarnations must be alive.
    pub at: DocumentPointQuery,
    /// Only incarnations of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Serializable form of [`DocumentPoint`] used by [`DocumentQuery`].
///
/// `DocumentPoint` itself is a plain Rust enum; this mirror exists because
/// queries derive `Serialize`/`Deserialize` and the upstream point is
/// `number | "current"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DocumentPointQuery {
    /// A commit sequence.
    Seq(i64),
    /// The current state.
    Current(CurrentPoint),
}

/// The `"current"` literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CurrentPoint {
    /// Current state.
    #[serde(rename = "current")]
    Current,
}

impl DocumentPointQuery {
    /// Convert to the runtime point.
    pub fn to_point(self) -> DocumentPoint {
        match self {
            Self::Seq(seq) => DocumentPoint::Seq(Seq::from_raw(seq)),
            Self::Current(_) => DocumentPoint::Current,
        }
    }
}

impl From<DocumentPoint> for DocumentPointQuery {
    fn from(point: DocumentPoint) -> Self {
        match point {
            DocumentPoint::Current => Self::Current(CurrentPoint::Current),
            DocumentPoint::Seq(seq) => Self::Seq(seq.as_i64()),
        }
    }
}

/// One entry and the sequence of the commit that persisted it.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryWithSeq {
    /// The entry.
    pub entry: EntryRecord,
    /// Commit sequence.
    pub commit_seq: Seq,
}

// ─── Storage contract ───────────────────────────────────────────────────────

/// The future type every [`Storage`] method returns.
///
/// The trait must be object-safe (the harness holds an `Arc<dyn Storage>`), and
/// `async fn` in a trait is not dyn-compatible; it also cannot promise a `Send`
/// future through a `dyn` call, which the Session's owner task requires. Every
/// method therefore returns this explicitly `Send` boxed future.
pub type StorageFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The storage contract every backend implements.
///
/// The upstream `Context` parameter (chord cancellation) is deliberately not
/// carried: upstream passes it so a remote backend can abandon work, but the
/// spec's own guarantee is that an admitted commit settles regardless of caller
/// cancellation (spec §4), so the Session strips it before settlement anyway.
/// Backends here are local and never observe cancellation. The two upstream
/// `entry()` overloads become [`Storage::entry`] and [`Storage::entry_in`].
///
/// Reads return [`StorageError::Request`] only for invalid requests. Any other
/// error is fatal for the instance (the JSONL backend poisons itself); the
/// Session's guard reads that rule, not the backend.
pub trait Storage: Send + Sync {
    /// Atomically persist one batch and return its sequence.
    ///
    /// Once this returns `Ok`, later reads through this storage observe the
    /// batch.
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>>;

    /// Return a fresh candidate from the storage-global numeric ID namespace.
    ///
    /// The returned value is greater than every ID minted or stored before,
    /// also across reopen. It is a raw number: brands are compile-time only
    /// (upstream `mintId`), so the caller brands it at the trusted boundary
    /// with [`decode_id`] — a generic method would make the trait
    /// non-object-safe, and the harness holds an `Arc<dyn Storage>`.
    fn mint_id(&self) -> StorageFuture<'_, Result<i64, StorageError>>;

    /// Look up one conversation by exact ID.
    fn conversation(
        &self,
        id: ConversationId,
    ) -> StorageFuture<'_, Result<Option<ConversationRecord>, StorageError>>;

    /// Scan conversations in `query.order` (default ascending) by ID.
    fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<ConversationRecord>, StorageError>>;

    /// Look up one global entry and the sequence that persisted it.
    fn entry(&self, id: EntryId) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>>;

    /// Look up one entry only when it is visible through the conversation's ancestry.
    fn entry_in(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>>;

    /// Return the newest visible entry with a `head` at or below the optional
    /// inclusive cutoff. Its `head` is the range's actual lower bound.
    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageFuture<'_, Result<Option<EntryRecord>, StorageError>>;

    /// Scan the inclusive visible range in `query.order` (default descending).
    fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<EntryRecord>, StorageError>>;

    /// Look up the latest complete record for one task.
    fn task(&self, id: TaskId) -> StorageFuture<'_, Result<Option<TaskRecord>, StorageError>>;

    /// Scan task records matching every supplied filter.
    fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<TaskRecord>, StorageError>>;

    /// Look up the latest complete record for one submission.
    fn submission(
        &self,
        id: SubmissionId,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>>;

    /// Scan submissions matching every supplied filter.
    fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<SubmissionRecord>, StorageError>>;

    /// Find a submission by its conversation-scoped deduplication key.
    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>>;

    /// Resolve the incarnation occupying one exact logical address at a point.
    fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<DocumentRecord>, StorageError>>;

    /// Materialize one specific incarnation by ID at a point.
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<StoredDocument>, StorageError>>;

    /// Scan incarnations alive in one exact scope at a point.
    fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<DocumentRecord>, StorageError>>;

    /// Release backend resources; all later operations must reject.
    fn close(&self) -> StorageFuture<'_, Result<(), StorageError>>;
}

/// Convert a raw number into a branded ID at a trusted decoding boundary.
pub fn decode_id<K: IdKind>(value: i64) -> Id<K> {
    id_from_number(value)
}
