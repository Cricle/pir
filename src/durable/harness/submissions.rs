//! Admission and terminal transitions for submissions.
//!
//! Ports the pure half of upstream `harness/submissions.ts` (spec §6). Two
//! decisions live here, both free of storage access so they can be tested
//! directly:
//!
//! * [`admit`] — what a new draft becomes, given whether the conversation is
//!   busy, what the inbox already holds, and whether a reset is queued;
//! * [`settle_run_inputs`] — what a run's end does to the inputs it placed and
//!   (only for a failed run) to the inputs queued behind them.
//!
//! # The admission table (spec §6)
//!
//! | draft | busy / inbox | result |
//! |---|---|---|
//! | display-only write, no queued reset | either | [`Admission::DisplayOnlyPlaced`]: appended and `done` at once |
//! | input, `whenBusy: reject` | busy | [`Admission::Rejected`]: nothing written |
//! | input or other write | busy or non-empty inbox | [`Admission::Queued`]: item appended to `pi.inbox` |
//! | write | idle, empty inbox | [`Admission::Done`]: appended and `done` |
//! | input | idle, empty inbox | [`Admission::Placed`]: placed and starts a run |
//!
//! A display-only write (only `kind` and `data`; no `model`, `head` or `edits`,
//! present counts) changes nothing the model sees, so it is admitted and
//! settled ahead of earlier queued submissions even while the conversation is
//! busy. The one exception is a queued reset: appended before it, the entry
//! would fall outside the context the reset starts, so it queues behind the
//! reset and is placed after it at the same boundary.
//!
//! An idle input queues with mode `steer` when `whenBusy` is `steer` and
//! `followUp` otherwise (including `reject`, which matters only while busy).
//!
//! # Terminal transitions
//!
//! | run end | inputs | inbox |
//! |---|---|---|
//! | answer | `done`, with the answer entry | untouched |
//! | failure (`model_error`, `no_model`) | `unanswered`, with the failure reason | per [`AfterFailedRun`] |
//! | abort, fault, orphan | `unanswered`, with the reason | untouched |
//!
//! `afterFailedRun` is read only for a failure: `keep` leaves queued inputs
//! where they are, `continue` leaves them to the caller's final boundary (which
//! places them and starts the next run), and `withdraw` settles every queued
//! steer/follow-up `unanswered` with reason [`REASON_RUN_FAILED`]; queued
//! writes stay for later placement. Abort, fault and orphan keep the inbox,
//! exactly as upstream does.
//!
//! # Join point (not implemented here)
//!
//! These functions decide *what* happens; the session facade performs it inside
//! one commit. It reads `pi.live` (`run`) and `pi.inbox` on the line, calls
//! [`admit`]/[`settle_run_inputs`], appends entries, and stages every returned
//! change with
//! [`Transaction::stage_submission_change`](crate::durable::session::Transaction::stage_submission_change)
//! (new records go through `Transaction::stage_submission`). A stale write
//! found by the caller ([`is_stale_entry`](super::inbox::is_stale_entry)) is
//! staged the same way, as `unanswered` with reason `stale`, and its item
//! removed from the draft. Nothing in this module reads or writes storage.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::durable::errors::{ConversationBusy, StorageError};
use crate::durable::harness::generation::{self, input_draft};
use crate::durable::harness::inbox::{
    boundary as inbox_boundary, check_input_entry, has_queued_reset, is_display_only,
    is_stale_entry, remove_selected, Boundary, InboxItem, InboxState, InputEntry, InputMode,
    SelectedKind, INBOX_DEFINITION, REASON_ABORTED, REASON_STALE,
};
use crate::durable::harness::live::{live_from_object, LIVE_DEFINITION};
use crate::durable::harness::settings::{AfterFailedRun, QueueMode, Settings};
use crate::durable::ids::{ConversationId, EntryId, SubmissionId};
use crate::durable::session::line::SessionState;
use crate::durable::session::transaction::{EntryDraft, HeadTarget, Transaction};
use crate::durable::session::SubmissionChange;
use crate::durable::types::{
    EntryRecord, JsonObject, StorageWrite, SubmissionRecord, SubmissionStatus, SubmissionType,
};

/// Settlement reason of a queued input withdrawn because a run failed.
pub const REASON_RUN_FAILED: &str = "run_failed";

/// Settlement reason of an input whose run task faulted.
pub const REASON_FAULTED: &str = "faulted";

/// Status a submission reports after admission or reuse.
///
/// Upstream's `Admission["status"]`. A terminal `unanswered` record reports
/// `done` when reused by `requestId`: the host asked about a submission that no
/// longer needs an answer, and there is no new admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdmissionStatus {
    /// Waiting in `pi.inbox`.
    Queued,
    /// Present in the transcript and owned by a run.
    Placed,
    /// Finished: appended, answered, or terminally unanswered.
    Done,
}

/// What a new submission becomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Idle conversation with an empty inbox: the input is placed and starts a
    /// run.
    Placed,
    /// A display-only write: appended and settled `done` in the same commit,
    /// busy or not and ahead of queued items.
    DisplayOnlyPlaced,
    /// Idle conversation with an empty inbox: a write is appended and settled
    /// `done` without starting a run.
    Done,
    /// Busy conversation or non-empty inbox: the item is appended to
    /// `pi.inbox`.
    Queued,
    /// A busy input with `whenBusy: reject`; nothing is written.
    Rejected(ConversationBusy),
}

impl Admission {
    /// The status the admission reports to the caller.
    pub fn status(&self) -> Option<AdmissionStatus> {
        match self {
            Self::Placed => Some(AdmissionStatus::Placed),
            Self::DisplayOnlyPlaced | Self::Done => Some(AdmissionStatus::Done),
            Self::Queued => Some(AdmissionStatus::Queued),
            Self::Rejected(_) => None,
        }
    }

    /// Whether admission wrote nothing durable.
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }
}

/// How a busy conversation treats a submitted input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WhenBusy {
    /// Queue as steering input and place it at the next boundary.
    Steer,
    /// Queue as a follow-up and place it at a final boundary.
    #[default]
    FollowUp,
    /// Reject the submission with [`ConversationBusy`].
    Reject,
}

impl WhenBusy {
    /// The inbox mode a queued input waits in: `steer` or `followUp`.
    ///
    /// `reject` queues as a follow-up when it reaches an idle conversation.
    pub fn mode(self) -> InputMode {
        match self {
            Self::Steer => InputMode::Steer,
            Self::FollowUp | Self::Reject => InputMode::FollowUp,
        }
    }
}

/// A host submission: user input that may start a run, or a passive entry
/// write.
///
/// `request_id` is upstream's conversation-scoped deduplication key; absent
/// admits a fresh submission.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionDraft {
    /// User input.
    Input {
        /// The user message content, as given by the host.
        content: Value,
        /// How a busy conversation treats it.
        when_busy: WhenBusy,
        /// The entry to place it as; `None` places a `pi.user` entry.
        entry: Option<InputEntry>,
        /// Host-provided deduplication key, scoped to the conversation.
        request_id: Option<String>,
    },
    /// A passive entry write, stored as plain JSON like an `EntryDraft`.
    Write {
        /// The entry draft.
        entry: JsonObject,
        /// Host-provided deduplication key, scoped to the conversation.
        request_id: Option<String>,
    },
}

impl SubmissionDraft {
    /// An input draft with no request key.
    pub fn input(content: Value, when_busy: WhenBusy, entry: Option<InputEntry>) -> Self {
        Self::Input {
            content,
            when_busy,
            entry,
            request_id: None,
        }
    }

    /// A write draft with no request key.
    pub fn write(entry: JsonObject) -> Self {
        Self::Write {
            entry,
            request_id: None,
        }
    }

    /// The same draft with `request_id` as its deduplication key.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        match &mut self {
            Self::Input {
                request_id: key, ..
            }
            | Self::Write {
                request_id: key, ..
            } => {
                *key = Some(request_id.into());
            }
        }
        self
    }

    /// The draft's conversation-scoped deduplication key.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Input { request_id, .. } | Self::Write { request_id, .. } => {
                request_id.as_deref()
            }
        }
    }

    /// The submission type this draft admits as.
    pub fn submission_type(&self) -> SubmissionType {
        match self {
            Self::Input { .. } => SubmissionType::Input,
            Self::Write { .. } => SubmissionType::Write,
        }
    }

    /// The `whenBusy` value; a write has no choice of its own and reports the
    /// default.
    pub fn when_busy(&self) -> WhenBusy {
        match self {
            Self::Input { when_busy, .. } => *when_busy,
            Self::Write { .. } => WhenBusy::FollowUp,
        }
    }
}

/// The queue item a draft becomes when it is not admitted at once.
///
/// A write's draft is stored as plain JSON; an input keeps its content and its
/// entry's kind and data. `id` is minted by the caller.
pub fn queued_item(id: SubmissionId, draft: &SubmissionDraft) -> InboxItem {
    match draft {
        SubmissionDraft::Input {
            content,
            when_busy,
            entry,
            ..
        } => InboxItem::Input {
            id,
            mode: when_busy.mode(),
            content: content.clone(),
            entry: entry.clone(),
        },
        SubmissionDraft::Write { entry, .. } => InboxItem::Write {
            id,
            entry: entry.clone(),
        },
    }
}

/// Decide what a draft becomes (spec §6).
///
/// * `conversation_id` names the conversation, which a rejected input reports
///   in [`ConversationBusy`] (upstream throws the same value).
/// * `busy` is `pi.live.run !== undefined`; `inbox_non_empty` is whether
///   `pi.inbox.items` holds anything; `has_queued_reset` is
///   [`has_queued_reset`] on the same state.
///
/// The caller must reject an invalid input entry kind with
/// [`check_input_entry`](super::inbox::check_input_entry) and resolve
/// `requestId` with [`resolve_request_id`] before calling this.
///
/// The evaluation order matches upstream exactly: the display-only shortcut
/// comes before the reject and queue decisions, so a display-only write is
/// admitted at once even while a run is active.
pub fn admit(
    conversation_id: ConversationId,
    busy: bool,
    inbox_non_empty: bool,
    draft: &SubmissionDraft,
    has_queued_reset: bool,
) -> Admission {
    if let SubmissionDraft::Write { entry, .. } = draft {
        if is_display_only(entry) && !has_queued_reset {
            return Admission::DisplayOnlyPlaced;
        }
    }
    if busy
        && matches!(draft, SubmissionDraft::Input { .. })
        && draft.when_busy() == WhenBusy::Reject
    {
        return Admission::Rejected(ConversationBusy { conversation_id });
    }
    if busy || inbox_non_empty {
        return Admission::Queued;
    }
    match draft {
        SubmissionDraft::Write { .. } => Admission::Done,
        SubmissionDraft::Input { .. } => Admission::Placed,
    }
}

/// [`admit`] with the busy flags taken from `inbox` itself.
pub fn admit_in_inbox(
    conversation_id: ConversationId,
    busy: bool,
    inbox: &InboxState,
    draft: &SubmissionDraft,
) -> Admission {
    admit(
        conversation_id,
        busy,
        !inbox.is_empty(),
        draft,
        has_queued_reset(inbox),
    )
}

/// How many queued items of each input mode a boundary places (upstream
/// `QueueModes`, the two queue-relevant settings fields).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueModes {
    /// How many steering inputs the boundary places.
    pub steering_mode: QueueMode,
    /// How many follow-up inputs the boundary places.
    pub follow_up_mode: QueueMode,
}

impl QueueModes {
    /// The modes a resolved [`Settings`] carries.
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
        }
    }

    /// The settings a boundary selection consults: only these two fields are
    /// read by [`inbox::boundary`](crate::durable::harness::inbox::boundary).
    fn settings(self) -> Settings {
        Settings {
            steering_mode: self.steering_mode,
            follow_up_mode: self.follow_up_mode,
            ..Settings::default()
        }
    }
}

/// What a commit-time admission produced.
///
/// Upstream `Admission` is `{id, status}`; the pure [`Admission`] enum keeps
/// only the decision, so the new (or reused) id travels beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Admitted {
    /// The created submission, or the one a `requestId` already named.
    ///
    /// `None` only when [`Admission::Rejected`] returned before writing.
    pub id: Option<SubmissionId>,
    /// The decision the caller reports.
    pub admission: Admission,
}

impl Admitted {
    /// An admission that wrote, or reused, one submission.
    fn new(id: SubmissionId, admission: Admission) -> Self {
        Self {
            id: Some(id),
            admission,
        }
    }
}

/// Admit a submission inside a commit (spec §6, upstream `admitSubmission`).
///
/// A known `requestId` returns its existing submission without writing. A
/// display-only write (only `kind` and `data`) is appended and settled `done`
/// at once, busy or not, unless a reset is queued. A busy conversation queues
/// anything else in `pi.inbox`; a `whenBusy: reject` input is reported as
/// [`Admission::Rejected`] with nothing written. An idle conversation with
/// queued items queues behind them and places a final boundary; an empty idle
/// inbox places the input (`pi.user` or the input's own entry) and starts a
/// run, or appends the write and settles it `done` (or `stale` when its head
/// reaches before the active range).
///
/// All table reads happen before the first table write: `requestId`, the
/// conversation, `pi.inbox` and `pi.live`, then the head marker a boundary
/// needs. `pi.inbox` is staged through the same document patterns the session
/// uses ([`StagedDocument`](crate::durable::session::transaction::StagedDocument)
/// with its loaded incarnation); the run itself is staged by
/// [`start_run_in_state`](crate::durable::harness::generation::start_run_in_state).
pub(crate) async fn admit_submission_in(
    state: &SessionState<'_>,
    tx: &mut Transaction,
    conversation: ConversationId,
    draft: SubmissionDraft,
    now: i64,
    queue_modes: QueueModes,
) -> Result<Admitted, StorageError> {
    let submission_type = draft.submission_type();
    if let SubmissionDraft::Input {
        entry: Some(entry), ..
    } = &draft
    {
        check_input_entry(entry).map_err(|error| StorageError::invalid(error.to_string()))?;
    }

    let context = tx.context().clone();
    let storage = state.storage.storage();
    if !tx.creates_conversation(conversation)
        && state
            .storage
            .read(&context, storage.conversation(conversation))
            .await?
            .is_none()
    {
        return Err(StorageError::invalid(format!(
            "Conversation {conversation} does not exist"
        )));
    }

    if let Some(request_id) = draft.request_id() {
        let existing = state
            .storage
            .read(
                &context,
                storage.submission_by_request(conversation, request_id),
            )
            .await?;
        if let Some(existing) = existing {
            if existing.submission_type != submission_type {
                return Err(StorageError::invalid(
                    RequestIdConflict::new(request_id, existing.submission_type).to_string(),
                ));
            }
            return Ok(Admitted::new(existing.id, admission_for(existing.status)));
        }
    }

    let (inbox_value, inbox_committed) = generation::read_conversation_document_state(
        state,
        tx,
        &context,
        &INBOX_DEFINITION,
        conversation,
    )
    .await?;
    let mut inbox: InboxState = from_object(inbox_value);

    // A display-only write changes nothing the model sees, so it is appended
    // and settled ahead of anything queued unless a reset is waiting.
    if let SubmissionDraft::Write { entry, .. } = &draft {
        if is_display_only(entry) && !has_queued_reset(&inbox) {
            let placed = append_entry_in(
                state,
                tx,
                conversation,
                generation::entry_draft_from_object(entry)
                    .map_err(|error| StorageError::invalid(error.to_string()))?,
            )
            .await?;
            let record = create_submission_in(
                state,
                tx,
                conversation,
                draft.request_id().map(str::to_owned),
                SubmissionType::Write,
                SubmissionStatus::Done,
                Some(placed.id),
                None,
                None,
            )
            .await?;
            return Ok(Admitted::new(record.id, Admission::DisplayOnlyPlaced));
        }
    }

    let (live_value, _) = generation::read_conversation_document_state(
        state,
        tx,
        &context,
        &LIVE_DEFINITION,
        conversation,
    )
    .await?;
    let live = live_from_object(&live_value);
    let busy = live.run.is_some();

    if busy
        && matches!(
            draft,
            SubmissionDraft::Input {
                when_busy: WhenBusy::Reject,
                ..
            }
        )
    {
        return Ok(Admitted {
            id: None,
            admission: Admission::Rejected(ConversationBusy {
                conversation_id: conversation,
            }),
        });
    }

    // The boundary reads the head marker; it is prepared before the first
    // table write, and a busy conversation needs none. The read goes through
    // the transaction (upstream `tx.latestHeadMarker` in `prepareBoundary`), so
    // it honors read-your-writes and stays on the revision this commit is
    // staged against; the raw storage read is kept only for a transaction
    // without an attached backend.
    let had_queued = !inbox.is_empty();
    let head = if busy {
        None
    } else {
        // A staged entry's own head marker wins (read-your-writes); otherwise
        // `latest_head` reads the committed revision this commit is based on.
        // Only a bare transaction (no attached backend) falls back to a plain
        // storage read.
        let inside = tx.latest_head(conversation).await?;
        if inside.is_some() || tx.has_commit_storage() {
            inside
        } else {
            let marker = state
                .storage
                .read(
                    &context,
                    storage.find_latest_head_marker(conversation, None),
                )
                .await?;
            marker.and_then(|record| record.head)
        }
    };

    if busy || had_queued {
        let record = create_submission_in(
            state,
            tx,
            conversation,
            draft.request_id().map(str::to_owned),
            submission_type,
            SubmissionStatus::Queued,
            None,
            None,
            None,
        )
        .await?;
        inbox.items.push(queued_item(record.id, &draft));
        generation::stage_conversation_document_state(
            tx,
            conversation,
            &INBOX_DEFINITION,
            to_object(&inbox),
            inbox_committed,
        );
        if busy {
            return Ok(Admitted::new(record.id, Admission::Queued));
        }
        let users = place_boundary_in(
            state,
            tx,
            conversation,
            &mut inbox,
            Boundary::Final,
            head,
            now,
            queue_modes,
        )
        .await?;
        generation::stage_conversation_document_state(
            tx,
            conversation,
            &INBOX_DEFINITION,
            to_object(&inbox),
            None,
        );
        if !users.is_empty() {
            generation::start_run_in_state(state, tx, conversation, &users).await?;
        }
        let admission = match draft {
            SubmissionDraft::Write { .. } => Admission::Done,
            SubmissionDraft::Input { .. } => {
                if users.last() == Some(&record.id) {
                    Admission::Placed
                } else {
                    Admission::Queued
                }
            }
        };
        return Ok(Admitted::new(record.id, admission));
    }

    match &draft {
        SubmissionDraft::Write { entry, .. } => {
            if head.is_some_and(|start| is_stale_entry(entry, start)) {
                let record = create_submission_in(
                    state,
                    tx,
                    conversation,
                    draft.request_id().map(str::to_owned),
                    SubmissionType::Write,
                    SubmissionStatus::Unanswered,
                    None,
                    Some(REASON_STALE.to_owned()),
                    None,
                )
                .await?;
                return Ok(Admitted::new(record.id, Admission::Done));
            }
            let placed = append_entry_in(
                state,
                tx,
                conversation,
                generation::entry_draft_from_object(entry)
                    .map_err(|error| StorageError::invalid(error.to_string()))?,
            )
            .await?;
            let record = create_submission_in(
                state,
                tx,
                conversation,
                draft.request_id().map(str::to_owned),
                SubmissionType::Write,
                SubmissionStatus::Done,
                Some(placed.id),
                None,
                None,
            )
            .await?;
            Ok(Admitted::new(record.id, Admission::Done))
        }
        SubmissionDraft::Input { content, entry, .. } => {
            let placed = append_entry_in(
                state,
                tx,
                conversation,
                input_draft(content.clone(), entry.clone(), now),
            )
            .await?;
            let record = create_submission_in(
                state,
                tx,
                conversation,
                draft.request_id().map(str::to_owned),
                SubmissionType::Input,
                SubmissionStatus::Placed,
                Some(placed.id),
                None,
                None,
            )
            .await?;
            generation::start_run_in_state(state, tx, conversation, &[record.id]).await?;
            Ok(Admitted::new(record.id, Admission::Placed))
        }
    }
}

/// The admission status an existing `requestId` reports.
fn admission_for(status: SubmissionStatus) -> Admission {
    match status {
        SubmissionStatus::Queued => Admission::Queued,
        SubmissionStatus::Placed => Admission::Placed,
        SubmissionStatus::Done | SubmissionStatus::Unanswered => Admission::Done,
    }
}

/// Place the queued items a final boundary selects (upstream `applyBoundary`).
///
/// The positionally selected items are removed from `inbox`; the caller stages
/// it. `head` is the active range start, advanced by heads placed here. Every
/// appended entry goes into `tx`; the returned ids are the placed inputs, in
/// placement order.
#[allow(clippy::too_many_arguments)]
async fn place_boundary_in(
    state: &SessionState<'_>,
    tx: &mut Transaction,
    conversation: ConversationId,
    inbox: &mut InboxState,
    at: Boundary,
    head: Option<EntryId>,
    now: i64,
    queue_modes: QueueModes,
) -> Result<Vec<SubmissionId>, StorageError> {
    let selected = inbox_boundary(inbox, at, &queue_modes.settings());
    let mut active_range_start = head;
    let mut users = Vec::new();
    for item in &selected {
        let Some(queued) = inbox.items.get(item.index).cloned() else {
            continue;
        };
        match item.kind {
            SelectedKind::Write => {
                let Some(entry) = queued.write_entry().cloned() else {
                    continue;
                };
                if active_range_start.is_some_and(|start| is_stale_entry(&entry, start)) {
                    tx.stage_submission_change(
                        item.id,
                        SubmissionChange::Unanswered {
                            reason: REASON_STALE.to_owned(),
                            detail: None,
                        },
                    );
                    continue;
                }
                let draft = generation::entry_draft_from_object(&entry)
                    .map_err(|error| StorageError::invalid(error.to_string()))?;
                let placed_head = draft.head;
                let record = append_entry_in(state, tx, conversation, draft).await?;
                match placed_head {
                    Some(HeadTarget::SelfEntry) => active_range_start = Some(record.id),
                    Some(HeadTarget::Entry(id)) => active_range_start = Some(id),
                    None => {}
                }
                tx.stage_submission_change(item.id, SubmissionChange::Placed { entry: record.id });
            }
            SelectedKind::User => {
                let (content, entry) = match &queued {
                    InboxItem::Input { content, entry, .. } => (content.clone(), entry.clone()),
                    InboxItem::Write { .. } => continue,
                };
                let record =
                    append_entry_in(state, tx, conversation, input_draft(content, entry, now))
                        .await?;
                tx.stage_submission_change(item.id, SubmissionChange::Placed { entry: record.id });
                users.push(item.id);
            }
        }
    }
    remove_selected(inbox, &selected);
    Ok(users)
}

/// Mint one raw id inside the commit.
async fn mint_id(state: &SessionState<'_>) -> Result<i64, StorageError> {
    let storage = state.storage.storage();
    state.storage.write(storage.mint_id()).await
}

/// Append one entry and stage its write, mirroring the session's
/// `append_entry` commit convenience.
async fn append_entry_in(
    state: &SessionState<'_>,
    tx: &mut Transaction,
    conversation: ConversationId,
    draft: EntryDraft,
) -> Result<EntryRecord, StorageError> {
    let id = EntryId::from_raw(mint_id(state).await?);
    let head = match draft.head {
        Some(HeadTarget::SelfEntry) => Some(id),
        Some(HeadTarget::Entry(entry)) => Some(entry),
        None => None,
    };
    let record = EntryRecord {
        id,
        conversation_id: conversation,
        kind: draft.kind,
        model: draft.model,
        data: draft.data,
        head,
        edits: draft.edits,
        by_task_id: tx.scope().task_id,
    };
    tx.push_write(StorageWrite::Entry {
        value: record.clone(),
    });
    Ok(record)
}

/// Create one submission record and stage it.
#[allow(clippy::too_many_arguments)]
async fn create_submission_in(
    state: &SessionState<'_>,
    tx: &mut Transaction,
    conversation: ConversationId,
    request_id: Option<String>,
    submission_type: SubmissionType,
    status: SubmissionStatus,
    entry: Option<EntryId>,
    reason: Option<String>,
    detail: Option<Value>,
) -> Result<SubmissionRecord, StorageError> {
    let record = SubmissionRecord {
        id: SubmissionId::from_raw(mint_id(state).await?),
        conversation_id: conversation,
        request_id,
        submission_type,
        status,
        entry,
        answer: None,
        reason,
        detail,
        trace: tx.context().trace_carrier().cloned(),
    };
    tx.stage_submission(record.clone());
    Ok(record)
}

/// Decode a document object, falling back to the type's default. A malformed
/// committed document reads as its initial value, as the generation store
/// does.
fn from_object<T: DeserializeOwned + Default>(object: JsonObject) -> T {
    serde_json::from_value(Value::Object(object)).unwrap_or_default()
}

/// Encode a document value as an object.
fn to_object<T: Serialize>(value: &T) -> JsonObject {
    match serde_json::to_value(value) {
        Ok(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

/// The outcome of a `requestId` lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestIdResolution {
    /// No prior submission carries the key: admit normally.
    Fresh,
    /// The key already names a submission of the same type: return it instead
    /// of writing anything.
    Existing {
        /// The original submission.
        id: SubmissionId,
        /// The status the caller reports for it.
        status: AdmissionStatus,
    },
}

/// A `requestId` was reused for the other submission type.
///
/// The message matches upstream's thrown error text: the type name is the wire
/// value (`input` or `write`), so a host that matched on the old text keeps
/// working.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestIdConflict {
    /// The reused key.
    pub request_id: String,
    /// The type the original submission had.
    pub existing_type: SubmissionType,
}

impl std::fmt::Display for RequestIdConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Request {} already identifies a submission of type {}",
            self.request_id,
            existing_type_name(self.existing_type)
        )
    }
}

impl std::error::Error for RequestIdConflict {}

impl RequestIdConflict {
    /// Build a conflict for one key and its original type.
    pub fn new(request_id: impl Into<String>, existing_type: SubmissionType) -> Self {
        Self {
            request_id: request_id.into(),
            existing_type,
        }
    }
}

/// The wire string of a submission type.
///
/// [`SubmissionType`] has no `Display` impl in `types.rs` (the record writes it
/// via serde) and this lane does not own that file, so the name is derived
/// here from the persisted value rather than hand-written twice.
pub fn existing_type_name(submission_type: SubmissionType) -> String {
    match serde_json::to_value(submission_type) {
        Ok(Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

/// Resolve `requestId` against the conversation's prior submission (spec §6).
///
/// Deduplication runs before any write: a known key returns its existing
/// submission without creating a record, and reusing one for the other type
/// rejects. `existing` is what
/// `Storage::submission_by_request(conversation_id, request_id)` returned; a
/// missing key or no key at all is [`RequestIdResolution::Fresh`].
pub fn resolve_request_id(
    draft_type: SubmissionType,
    request_id: Option<&str>,
    existing: Option<&SubmissionRecord>,
) -> Result<RequestIdResolution, RequestIdConflict> {
    let Some(request_id) = request_id else {
        return Ok(RequestIdResolution::Fresh);
    };
    let Some(record) = existing else {
        return Ok(RequestIdResolution::Fresh);
    };
    if record.submission_type != draft_type {
        return Err(RequestIdConflict::new(request_id, record.submission_type));
    }
    Ok(RequestIdResolution::Existing {
        id: record.id,
        status: admission_status(record.status),
    })
}

/// The status a reused submission reports: `unanswered` reads as `done`.
pub fn admission_status(status: SubmissionStatus) -> AdmissionStatus {
    match status {
        SubmissionStatus::Queued => AdmissionStatus::Queued,
        SubmissionStatus::Placed => AdmissionStatus::Placed,
        SubmissionStatus::Done | SubmissionStatus::Unanswered => AdmissionStatus::Done,
    }
}

/// Find the submission a key already names in one conversation.
///
/// This is the pure predicate the facade's `submission_by_request` read
/// implements; `requestId` is scoped to the conversation, so a key used in
/// another conversation does not match.
pub fn find_request_submission<'a>(
    records: impl IntoIterator<Item = &'a SubmissionRecord>,
    conversation_id: ConversationId,
    request_id: &str,
) -> Option<&'a SubmissionRecord> {
    records.into_iter().find(|record| {
        record.conversation_id == conversation_id
            && record.request_id.as_deref() == Some(request_id)
    })
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum RunOutcome {
    /// The run produced an answer entry.
    Answered(EntryId),
    /// A model failure (`model_error`, `no_model`): the settlement reason and
    /// optional detail (upstream's text).
    Failed(String, Option<Value>),
    /// The run task was aborted; the abort protocol settles the inputs.
    Aborted,
    /// Runtime-detected contract failure; the value is the error message.
    Faulted(Value),
    /// The scheduler orphaned the task; the value is why.
    Orphaned(String),
}

impl RunOutcome {
    /// Whether this end is a model failure, the only one `afterFailedRun`
    /// governs.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_, _))
    }
}

/// The changes a run's end stages (spec §6).
///
/// `inputs` is the id list of `pi.live.run`, in order. Those are settled first.
/// Only for a model failure under [`AfterFailedRun::Withdraw`] are the queued
/// steer/follow-up items appended too, in ID order, with reason
/// [`REASON_RUN_FAILED`]; queued writes are never settled here.
///
/// [`AfterFailedRun::Continue`] returns no queued change: the caller applies
/// the final boundary in the same commit, which places the items and starts the
/// successor generation. Abort, fault and orphan ignore the policy and leave
/// the inbox alone.
pub fn settle_run_inputs(
    inputs: &[SubmissionId],
    inbox: &InboxState,
    outcome: RunOutcome,
    after_failed_run: AfterFailedRun,
) -> Vec<(SubmissionId, SubmissionChange)> {
    let mut changes: Vec<(SubmissionId, SubmissionChange)> = match &outcome {
        RunOutcome::Answered(answer) => inputs
            .iter()
            .map(|id| (*id, SubmissionChange::Done { answer: *answer }))
            .collect(),
        RunOutcome::Failed(reason, detail) => inputs
            .iter()
            .map(|id| {
                (
                    *id,
                    SubmissionChange::Unanswered {
                        reason: reason.clone(),
                        detail: detail.clone(),
                    },
                )
            })
            .collect(),
        RunOutcome::Aborted => inputs
            .iter()
            .map(|id| {
                (
                    *id,
                    SubmissionChange::Unanswered {
                        reason: REASON_ABORTED.to_owned(),
                        detail: None,
                    },
                )
            })
            .collect(),
        RunOutcome::Faulted(detail) => inputs
            .iter()
            .map(|id| {
                (
                    *id,
                    SubmissionChange::Unanswered {
                        reason: REASON_FAULTED.to_owned(),
                        detail: Some(detail.clone()),
                    },
                )
            })
            .collect(),
        RunOutcome::Orphaned(reason) => inputs
            .iter()
            .map(|id| {
                (
                    *id,
                    SubmissionChange::Unanswered {
                        reason: reason.clone(),
                        detail: None,
                    },
                )
            })
            .collect(),
    };

    if outcome.is_failure() && after_failed_run == AfterFailedRun::Withdraw {
        for item in &inbox.items {
            if item.is_write() {
                continue;
            }
            changes.push((
                item.id(),
                SubmissionChange::Unanswered {
                    reason: REASON_RUN_FAILED.to_owned(),
                    detail: None,
                },
            ));
        }
    }
    changes
}
