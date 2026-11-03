//! Structural mounts of one conversation's transcript and built-in documents.
//!
//! Port of upstream `packages/durable/src/harness/view.ts` onto the Rust
//! session kernel (spec §9.3). A [`ConversationView`] is the raw active
//! transcript (`ContextView.entries`: the head marker, then the non-head
//! entries from its head) plus the five built-in conversation documents keyed
//! by kind. It is derived from committed publications only.
//!
//! # Mounts
//!
//! [`ConversationViews`] keeps **at most one mount per conversation**. A mount
//! is built by its first observer ([`ConversationViews::attach`]) and dropped
//! with its last ([`Detach::detach`]), exactly as upstream. Building happens
//! inside a `read_on_line` job, so the mount is hydrated from exactly one
//! committed state, and the observer is registered before the line accepts
//! another commit — it sees every later publication and nothing earlier.
//!
//! # What advances a mount
//!
//! The adapter registers one **internal** commit listener (upstream
//! `session.observeCommits`): mounted conversations are structural state, so a
//! bug that throws while advancing them must fail the session rather than let
//! its memory lag behind storage. Every observer call is wrapped in
//! [`contained`], so observer code cannot fail the session either.
//!
//! Entry writes are published in ID order. An ordinary entry is appended; a
//! head marker keeps the non-head entries from its head (always a suffix) and
//! goes in front. Document changes are applied only for the five mounted kinds
//! and only for this conversation; a change to a different incarnation or
//! definition version sets the whole document instead of forwarding its exact
//! operations, so an observer never applies an operation to a shape it does not
//! hold.
//!
//! # Deliberate deviations from upstream
//!
//! * **No chord values.** Upstream's `state()`/`watch()` wrap a mount in a
//!   chord `CommittedStateSource`/`CommittedWatch`. The Rust kernel has no
//!   generic chord value type (its `CommittedStateSource` is document-JSON
//!   specific), so observers implement [`ViewObserver`] directly. The exact
//!   operation batch and the resulting revision are both handed to them, which
//!   is the whole contract those wrappers existed to carry.
//! * **A synchronous factory.** `attach`'s factory cannot be `async`: a Rust
//!   closure cannot return a future that borrows its argument without boxing,
//!   so `create` is synchronous and receives the revision. Reading committed
//!   storage during creation is therefore not supported here.
//! * **`failure` is a concrete error.** Upstream passes
//!   `{ error: unknown } | undefined`; the port passes
//!   `Option<&StorageError>`, the kernel's session failure.
//! * **The mount value is shared through `Arc`.** Upstream shares structural
//!   JSON; the typed Rust value is rebuilt once per publication that changes
//!   the mount and shared through `Arc` with every observer otherwise.
//! * **The view's containers are copy-on-write.** A publication clones the
//!   [`ConversationView`] shell (one conversation record, two refcount bumps)
//!   and replaces only the container it changed: the chunked append-only
//!   [`EntryList`] when an entry was written, one document value when a
//!   document changed, the map shell once per document write. Appending one
//!   entry shares every historical [`EntryRecord`] and copies at most one
//!   chunk table, so a long transcript no longer pays an O(n) deep clone per
//!   publication. A `pi.live` progress publication therefore copies neither
//!   the transcript nor the unchanged documents, which is the per-publication
//!   O(state) clone this module used to pay.
//! * **Document deltas are replayed or replaced.** Upstream applies the
//!   forwarded operations to its shared value. The port applies the
//!   publication's complete value for the document (the exact adopted
//!   revision) while still forwarding the exact operations, so a replay failure
//!   cannot desynchronise the mount.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Index;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::durable::context::Context;
use crate::durable::delta::{Op, Seg};
use crate::durable::documents::{DocOwner, ResolvedAddress};
use crate::durable::errors::{StorageError, StorageRequestError};
use crate::durable::harness::documents::conversation_definitions;
use crate::durable::harness::util::{closed_error, contained, scan_all, Report};
use crate::durable::ids::{ConversationId, DocumentId};
use crate::durable::session::facade::Session;
use crate::durable::session::line::{JobValue, SessionState};
use crate::durable::session::publication::{
    CommitChange, CommitPublication, InternalListener, TableCommitChange,
};
use crate::durable::types::{
    ConversationRecord, DocumentCommitChange, DocumentPoint, DocumentScope, EntryQuery,
    EntryRecord, JsonObject, ScanOrder,
};

/// Page size of a storage scan (upstream `context.ts` `SCAN_PAGE_SIZE`).
const SCAN_PAGE_SIZE: usize = 256;

/// Entries per chunk of [`EntryList`].
const ENTRY_CHUNK_SIZE: usize = 256;

/// An append-only list of entry records.
///
/// Entries are stored in fixed-size chunks of shared [`Arc`] handles
/// ([`EntryList`] is itself kept behind one [`Arc`] in
/// [`ConversationView::entries`]). Appending one entry copies at most one
/// chunk's pointer table and never an [`EntryRecord`]: every earlier revision
/// keeps its own chunk list while the records themselves are shared. A head
/// marker rebuilds the chunk table from the kept suffix (pointer copies only)
/// and reuses the same record allocations.
///
/// The type exposes a slice-like read surface (`len`, `is_empty`, `get`,
/// `iter`, `to_vec` and indexing) without materializing a contiguous
/// `Vec<EntryRecord>`.
#[derive(Clone, Default)]
pub struct EntryList {
    /// Immutable chunks; all but the last hold exactly `ENTRY_CHUNK_SIZE`
    /// records, so `index / ENTRY_CHUNK_SIZE` locates a chunk directly.
    chunks: Vec<Arc<[Arc<EntryRecord>]>>,
    len: usize,
}

impl EntryList {
    /// Build a list from owned records, wrapping each in one shared handle.
    #[must_use]
    pub fn from_vec(entries: Vec<EntryRecord>) -> Self {
        let items: Vec<Arc<EntryRecord>> = entries.into_iter().map(Arc::new).collect();
        Self::from_arcs(items)
    }

    fn from_arcs(items: Vec<Arc<EntryRecord>>) -> Self {
        let len = items.len();
        let mut chunks = Vec::with_capacity(len.div_ceil(ENTRY_CHUNK_SIZE));
        let mut items = items.into_iter();
        loop {
            let chunk: Vec<Arc<EntryRecord>> = items.by_ref().take(ENTRY_CHUNK_SIZE).collect();
            if chunk.is_empty() {
                break;
            }
            chunks.push(Arc::from(chunk));
        }
        Self { chunks, len }
    }

    /// The number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One record by position.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&EntryRecord> {
        if index >= self.len {
            return None;
        }
        let chunk = index / ENTRY_CHUNK_SIZE;
        let offset = index % ENTRY_CHUNK_SIZE;
        self.chunks
            .get(chunk)
            .and_then(|chunk| chunk.get(offset))
            .map(Arc::as_ref)
    }

    /// Iterate the records in order.
    pub fn iter(&self) -> impl Iterator<Item = &EntryRecord> + '_ {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.iter().map(Arc::as_ref))
    }

    /// The records as one owned vector (a deep copy, for consumers that need a
    /// `Vec<EntryRecord>`).
    #[must_use]
    pub fn to_vec(&self) -> Vec<EntryRecord> {
        self.iter().cloned().collect()
    }

    /// A new revision with `entry` appended.
    #[must_use]
    pub fn appended(&self, entry: Arc<EntryRecord>) -> Self {
        let mut chunks = self.chunks.clone();
        if let Some(last) = chunks.last_mut() {
            if last.len() < ENTRY_CHUNK_SIZE {
                let mut items: Vec<Arc<EntryRecord>> = last.iter().cloned().collect();
                items.push(entry);
                *last = Arc::from(items);
                return Self {
                    chunks,
                    len: self.len + 1,
                };
            }
        }
        chunks.push(Arc::from(vec![entry]));
        Self {
            chunks,
            len: self.len + 1,
        }
    }

    /// A new revision with `entry` at the front and the records from `kept`
    /// on kept (a head marker keeps only the suffix from its `head`).
    #[must_use]
    pub fn prefixed(&self, kept: usize, entry: Arc<EntryRecord>) -> Self {
        let mut items = Vec::with_capacity(self.len.saturating_sub(kept) + 1);
        items.push(entry);
        items.extend(self.arcs().skip(kept).cloned());
        Self::from_arcs(items)
    }

    fn arcs(&self) -> impl Iterator<Item = &Arc<EntryRecord>> + '_ {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
}

impl Index<usize> for EntryList {
    type Output = EntryRecord;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("entry index out of bounds")
    }
}

impl PartialEq for EntryList {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl std::fmt::Debug for EntryList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl Serialize for EntryList {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.len))?;
        for entry in self.iter() {
            seq.serialize_element(entry)?;
        }
        seq.end()
    }
}

/// One conversation's raw active transcript and built-in documents.
///
/// Both containers are shared copy-on-write ([`Arc`]): cloning a revision is a
/// refcount bump, and the mount replaces only the container a publication
/// actually changed. A document delta (the streaming `pi.live` progress path)
/// therefore leaves the transcript and every unchanged document shared with the
/// previous revision instead of deep-cloning them. The transcript itself is a
/// chunked append-only [`EntryList`], so appending one entry shares every
/// historical record instead of copying the entries vector.
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationView {
    /// The immutable conversation record.
    pub conversation: ConversationRecord,
    /// Raw active entries, as `ContextView.entries`: the head marker, then the
    /// non-head entries from its head.
    pub entries: Arc<EntryList>,
    /// Built-in conversation documents keyed by kind; absent documents are
    /// absent. A value is shared by every revision that still shows it, so a
    /// write to one document leaves the others where they are.
    pub docs: Arc<BTreeMap<String, MountedDocument>>,
}

/// One mounted conversation document, shared by every revision that shows it.
///
/// A document publication swaps one of these handles; the documents it did not
/// touch stay in the same allocation.
pub type MountedDocument = Arc<JsonObject>;

impl ConversationView {
    /// The raw active entries.
    #[must_use]
    pub fn entries(&self) -> &EntryList {
        &self.entries
    }

    /// The mounted built-in documents, keyed by kind.
    #[must_use]
    pub fn docs(&self) -> &BTreeMap<String, MountedDocument> {
        &self.docs
    }

    /// One mounted built-in document by kind.
    #[must_use]
    pub fn doc(&self, kind: &str) -> Option<&JsonObject> {
        self.docs.get(kind).map(Arc::as_ref)
    }
}

/// Receives each next revision of a mount, and the Session's close.
///
/// Upstream's `ViewObserver` has optional `advance`/`publication` members and a
/// required `closeSession`; the trait's default bodies are those optional
/// members. Methods take `&self` because the mount shares observers
/// (`Arc<dyn ViewObserver>`); an observer that keeps state uses interior
/// mutability.
pub trait ViewObserver: Send + Sync {
    /// Each next revision, with the exact operation batch that produced it.
    fn advance(&self, _value: &ConversationView, _ops: &[Op], _context: &Context) {}

    /// Every publication, after the mount took it; `ops` are the mount's,
    /// possibly none.
    fn publication(
        &self,
        _before: &ConversationView,
        _after: &ConversationView,
        _ops: &[Op],
        _publication: &CommitPublication,
        _context: &Context,
    ) {
    }

    /// The Session closed; `failure` is its failure, if it failed.
    fn close_session(&self, failure: Option<&StorageError>);
}

/// One document incarnation a mount shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Incarnation {
    id: DocumentId,
    version: i64,
}

type ObserverSlot = (u64, Arc<dyn ViewObserver>);

/// One conversation's current revision, the document incarnations it shows, and
/// its observers.
struct Mount {
    value: Arc<ConversationView>,
    docs: BTreeMap<String, Incarnation>,
    observers: Vec<ObserverSlot>,
}

struct SharedInner {
    mounts: BTreeMap<ConversationId, Arc<Mutex<Mount>>>,
    closed: bool,
    next_registration: u64,
}

/// A removable observer registration.
///
/// Upstream returns a bare `detach: () => void`; this is the same handle with
/// `Send + Sync` identity so it can be stored anywhere a callback can.
#[derive(Clone)]
pub struct Detach {
    shared: Arc<Mutex<SharedInner>>,
    conversation: ConversationId,
    registration: u64,
}

impl Detach {
    /// Drop the observer; the mount is dropped with its last observer. Calling
    /// it more than once is harmless.
    pub fn detach(&self) {
        let mount = {
            let shared = lock(&self.shared);
            match shared.mounts.get(&self.conversation) {
                Some(mount) => mount.clone(),
                None => return,
            }
        };
        let empty = {
            let mut mount = lock(&mount);
            mount
                .observers
                .retain(|(registration, _)| *registration != self.registration);
            mount.observers.is_empty()
        };
        if empty {
            let mut shared = lock(&self.shared);
            if shared
                .mounts
                .get(&self.conversation)
                .is_some_and(|held| Arc::ptr_eq(held, &mount))
            {
                shared.mounts.remove(&self.conversation);
            }
        }
    }
}

/// An observer created from the current revision and its registration.
pub struct AttachedView<O: ViewObserver> {
    /// The observer, as created by `attach`'s factory.
    pub observer: Arc<O>,
    /// Drops the registration (and, with the last one, the mount).
    pub detach: Detach,
}

/// The view mounts of one Session.
///
/// Open one per session (upstream keys them on the harness). The adapter
/// installs an internal commit listener and a close listener for the life of
/// the session; both are dropped when the session closes.
pub struct ConversationViews {
    session: Session,
    shared: Arc<Mutex<SharedInner>>,
    report: Report,
}

impl ConversationViews {
    /// Install the view adapter over `session`, reporting listener errors to a
    /// no-op sink.
    pub async fn open(session: Session) -> Result<Self, StorageError> {
        Self::open_with_report(session, Arc::new(|_| {})).await
    }

    /// [`ConversationViews::open`] with a report sink for contained listener
    /// errors (upstream `session.report`).
    pub async fn open_with_report(session: Session, report: Report) -> Result<Self, StorageError> {
        let shared = Arc::new(Mutex::new(SharedInner {
            mounts: BTreeMap::new(),
            closed: false,
            next_registration: 1,
        }));

        // Upstream `session.observeCommits((publication, context) => ...)`.
        let listener_shared = Arc::downgrade(&shared);
        let listener_report = report.clone();
        let listener: InternalListener = Arc::new(move |publication, context| {
            if let Some(shared) = listener_shared.upgrade() {
                advance_mounts(&shared, publication, context, &listener_report);
            }
        });
        session
            .handle()
            .maintain(move |inner| {
                inner.internal_listeners.add(listener);
            })
            .await?;

        // Upstream `session.subscribeClose(() => ...)`.
        let close_shared = Arc::downgrade(&shared);
        let close_report = report.clone();
        let close_session = session.clone();
        session
            .handle()
            .maintain(move |inner| {
                let shared = close_shared.clone();
                let report = close_report.clone();
                let session = close_session.clone();
                inner.close_listeners.push(Arc::new(move || {
                    let failure = Session::failure(&session);
                    if let Some(shared) = shared.upgrade() {
                        close_mounts(&shared, failure.as_ref(), &report);
                    }
                }));
            })
            .await?;

        Ok(Self {
            session,
            shared,
            report,
        })
    }

    /// Report an observer's listener error (upstream `report(error)`).
    pub fn report(&self, message: &str) {
        (self.report)(message);
    }

    /// Whether the session closed and dropped every mount.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        lock(&self.shared).closed
    }

    /// The number of live mounts: at most one per conversation.
    #[must_use]
    pub fn mount_count(&self) -> usize {
        lock(&self.shared).mounts.len()
    }

    /// Whether a conversation has a live mount.
    #[must_use]
    pub fn is_mounted(&self, id: ConversationId) -> bool {
        lock(&self.shared).mounts.contains_key(&id)
    }

    /// Register an observer created from the current revision, atomically on
    /// the Session line: it sees every later publication and nothing earlier.
    ///
    /// The first observer of a conversation builds the mount; the returned
    /// [`Detach`] drops the observer, and the mount with its last observer.
    ///
    /// # Errors
    ///
    /// The conversation does not exist, the session closed or failed, or the
    /// attach context was aborted.
    pub async fn attach<O, F>(
        &self,
        id: ConversationId,
        create: F,
        context: &Context,
    ) -> Result<AttachedView<O>, StorageError>
    where
        O: ViewObserver + 'static,
        F: FnOnce(&ConversationView) -> O + Send + 'static,
    {
        if self.is_closed() {
            return Err(closed_error(&self.session));
        }
        let shared = self.shared.clone();
        let handle = self.session.handle().clone();
        let context = context.clone();
        self.session
            .handle()
            .read_on_line(move |state| {
                Box::pin(async move {
                    // Upstream `#build` runs on the line: one committed state
                    // for the conversation record, the entries and the
                    // documents.
                    let existing = {
                        let shared = lock(&shared);
                        shared.mounts.get(&id).cloned()
                    };
                    let mount = match existing {
                        Some(mount) => mount,
                        None => Arc::new(Mutex::new(build_mount(state, id, &context).await?)),
                    };

                    let value = lock(&mount).value.clone();
                    let observer = Arc::new(create(&value));

                    // Close or cancellation may begin while the mount
                    // hydrates; register nothing then.
                    if lock(&shared).closed {
                        return Err(closed_error(&handle));
                    }
                    if context.is_aborted() {
                        return Err(StorageError::Request(StorageRequestError::new(
                            "The attach context was aborted",
                        )));
                    }

                    let registration = {
                        // One lock order everywhere: the registry, then the
                        // mount. `Detach` and the close listener do the same,
                        // and the commit listener snapshots the registry
                        // before touching a mount.
                        let mut inner = lock(&shared);
                        let registration = inner.next_registration;
                        inner.next_registration = registration.wrapping_add(1);
                        let mut mount_state = lock(&mount);
                        inner.mounts.insert(id, mount.clone());
                        mount_state
                            .observers
                            .push((registration, observer.clone() as Arc<dyn ViewObserver>));
                        registration
                    };

                    let value: JobValue = Box::new(AttachedView {
                        observer,
                        detach: Detach {
                            shared,
                            conversation: id,
                            registration,
                        },
                    });
                    Ok(value)
                })
            })
            .await
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The built-in document kinds a view mounts (upstream `MOUNTED`).
fn mounted_kinds() -> &'static BTreeSet<&'static str> {
    static KINDS: std::sync::OnceLock<BTreeSet<&'static str>> = std::sync::OnceLock::new();
    KINDS.get_or_init(|| {
        conversation_definitions()
            .into_iter()
            .map(|(kind, _)| kind)
            .collect()
    })
}

/// Upstream `#build`: the conversation record, its active entries and its
/// mounted conversation documents, read from one committed state.
async fn build_mount(
    state: &SessionState<'_>,
    id: ConversationId,
    context: &Context,
) -> Result<Mount, StorageError> {
    let storage = state.storage.storage();
    let conversation = state
        .storage
        .read(context, storage.conversation(id))
        .await?
        .ok_or_else(|| StorageError::invalid(format!("Conversation {id} does not exist")))?;
    let entries = active_entries(state, id, context).await?;

    let mut docs = BTreeMap::new();
    let mut incarnations = BTreeMap::new();
    for (kind, definition) in conversation_definitions() {
        // Upstream `session.conversationDocumentOnLine`: resolve the current
        // incarnation, materialize its value at the definition version, and
        // record the incarnation the mount now shows.
        let resolved = ResolvedAddress::new(
            kind,
            definition.common.semantics,
            Some(DocOwner::Conversation(id)),
            None,
        )?;
        let Some(record) = state
            .storage
            .read(
                context,
                storage.find_document(resolved.address, DocumentPoint::Current),
            )
            .await?
        else {
            continue;
        };
        let Some(stored) = state
            .storage
            .read(context, storage.document(record.id, DocumentPoint::Current))
            .await?
        else {
            return Err(StorageError::invalid(format!(
                "Current document {} ({}) cannot be read",
                record.id, record.kind
            )));
        };
        let value = definition.materialize_value(&record, stored.version, stored.value)?;
        incarnations.insert(
            kind.to_owned(),
            Incarnation {
                id: record.id,
                version: definition.common.version,
            },
        );
        docs.insert(kind.to_owned(), Arc::new(value));
    }

    Ok(Mount {
        value: Arc::new(ConversationView {
            conversation,
            entries: Arc::new(EntryList::from_vec(entries)),
            docs: Arc::new(docs),
        }),
        docs: incarnations,
        observers: Vec::new(),
    })
}

/// Upstream `activeEntries` + `captureContextBounds`: the head marker followed
/// by the visible non-head entries from its head, oldest first.
async fn active_entries(
    state: &SessionState<'_>,
    id: ConversationId,
    context: &Context,
) -> Result<Vec<EntryRecord>, StorageError> {
    let storage = state.storage.storage();
    // `captureContextBounds`: the newest visible entry, then the newest head
    // marker at or before it, with two O(1) reads.
    let newest = state
        .storage
        .read(
            context,
            storage.scan_entries(
                EntryQuery {
                    conversation_id: id,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: Some(ScanOrder::Descending),
                },
                1,
                None,
            ),
        )
        .await?;
    let Some(tail) = newest.items.first().map(|entry| entry.id) else {
        return Ok(Vec::new());
    };
    let head = state
        .storage
        .read(context, storage.find_latest_head_marker(id, Some(tail)))
        .await?;

    // The visible range: from the head marker's head, or transcript start,
    // through the tail. A descending scan paged by cursor is reversed to get
    // the oldest-first order the derivation expects.
    let query = EntryQuery {
        conversation_id: id,
        min_entry_id: head.as_ref().and_then(|head| head.head),
        max_entry_id: Some(tail),
        order: Some(ScanOrder::Descending),
    };
    let mut range: Vec<EntryRecord> = scan_all(|cursor| {
        state.storage.read(
            context,
            storage.scan_entries(query.clone(), SCAN_PAGE_SIZE, cursor),
        )
    })
    .await?;
    range.reverse();

    Ok(match &head {
        Some(head) => std::iter::once(head.clone())
            .chain(range.into_iter().filter(|entry| entry.head.is_none()))
            .collect(),
        None => range,
    })
}

/// Whether one publication change touches this mount at all.
///
/// A document change matches only when the change's *scope* is this
/// conversation. The published `conversation_id` alone is not enough: a
/// task-scoped document carries the conversation that owns its task, and the
/// built-in kinds (`pi.live`, `pi.usage`, …) exist in both scopes, so a task
/// document would otherwise overwrite the mount's conversation document and
/// leave it permanently different from what a remount derives.
fn relevant(change: &CommitChange, id: ConversationId) -> bool {
    match change {
        CommitChange::Table(TableCommitChange::Entry(entry)) => entry.conversation_id == id,
        CommitChange::Document(DocumentCommitChange::Document { record, .. }) => {
            matches!(
                record.scope,
                DocumentScope::Conversation { conversation_id } if conversation_id == id
            ) && record.key.is_none()
                && mounted_kinds().contains(record.kind.as_str())
        }
        _ => false,
    }
}

/// Advance every mount with one publication (upstream `advance()`).
fn advance_mounts(
    shared: &Arc<Mutex<SharedInner>>,
    publication: &CommitPublication,
    context: &Context,
    report: &Report,
) {
    let mounts: Vec<(ConversationId, Arc<Mutex<Mount>>)> = {
        let inner = lock(shared);
        inner
            .mounts
            .iter()
            .map(|(id, mount)| (*id, mount.clone()))
            .collect()
    };
    let frame_context = context.without_abort_signal();
    for (id, mount) in mounts {
        let (before, after, ops, observers) = {
            let mut mount = lock(&mount);
            let before = mount.value.clone();
            let ops = if publication
                .changes
                .iter()
                .any(|change| relevant(change, id))
            {
                let mut next = (*mount.value).clone();
                let ops = apply_changes(&mut mount, &mut next, id, publication);
                if !ops.is_empty() {
                    mount.value = Arc::new(next);
                }
                ops
            } else {
                Vec::new()
            };
            let after = mount.value.clone();
            let observers: Vec<Arc<dyn ViewObserver>> = mount
                .observers
                .iter()
                .map(|(_, observer)| observer.clone())
                .collect();
            (before, after, ops, observers)
        };
        if !ops.is_empty() {
            for observer in &observers {
                contained(report, || {
                    observer.advance(&after, &ops, &frame_context);
                });
            }
        }
        for observer in &observers {
            contained(report, || {
                observer.publication(&before, &after, &ops, publication, &frame_context);
            });
        }
    }
}

/// Derive one publication's operations for a mount and apply them to a
/// copy-on-write next revision.
///
/// `next` starts as a clone of the mounted revision, which shares every
/// container through `Arc`. A document delta unshares only that document and
/// the (small) map shell ([`Arc::make_mut`]); an entry write replaces the
/// transcript handle with a new append-only [`EntryList`] revision that shares
/// every historical record, so neither path copies the transcript. A
/// publication that produces no operation leaves `next` fully shared and the
/// caller keeps the mounted revision as it is.
fn apply_changes(
    mount: &mut Mount,
    next: &mut ConversationView,
    id: ConversationId,
    publication: &CommitPublication,
) -> Vec<Op> {
    let mut doc_ops: Vec<Op> = Vec::new();
    let mut entry_ops: Vec<Op> = Vec::new();
    for change in &publication.changes {
        match change {
            CommitChange::Table(TableCommitChange::Entry(entry)) if entry.conversation_id == id => {
                let Ok(json) = serde_json::to_value(entry) else {
                    continue;
                };
                // Every entry change builds a new append-only revision that
                // shares all historical records; only the new record is
                // allocated. The mounted revision keeps its own chunk table,
                // so this is safe even though `next` is a shallow clone of it.
                let shared = Arc::new(entry.clone());
                match entry.head {
                    None => {
                        // Entry writes are published in ID order.
                        entry_ops.push(Op::Splice {
                            path: entries_path(),
                            index: next.entries.len(),
                            remove: 0,
                            items: vec![json],
                        });
                        next.entries = Arc::new(next.entries.appended(shared));
                    }
                    Some(target) => {
                        // A head marker keeps the non-head entries from its
                        // head, which are always a suffix, and goes in front.
                        let kept = next
                            .entries
                            .iter()
                            .position(|candidate| {
                                candidate.head.is_none() && candidate.id >= target
                            })
                            .unwrap_or(next.entries.len());
                        entry_ops.push(Op::Splice {
                            path: entries_path(),
                            index: 0,
                            remove: kept,
                            items: vec![json],
                        });
                        next.entries = Arc::new(next.entries.prefixed(kept, shared));
                    }
                }
            }
            CommitChange::Document(DocumentCommitChange::Document {
                record,
                version,
                value,
                ops,
                ..
            }) => {
                // Scope, not just the owning conversation: a task-scoped
                // document shares the built-in kind (`pi.live`, `pi.usage`, …)
                // and would otherwise replace the mounted conversation value.
                let mounted_scope = matches!(
                    record.scope,
                    DocumentScope::Conversation { conversation_id } if conversation_id == id
                );
                if !mounted_scope {
                    continue;
                }
                let kind = record.kind.as_str();
                if record.key.is_some() || !mounted_kinds().contains(kind) {
                    continue;
                }
                let path = doc_path(kind);
                let mounted = mount.docs.get(kind).copied();
                match value {
                    // A retirement removes the document only when it retires
                    // the incarnation this mount shows.
                    None => {
                        if mounted.map(|incarnation| incarnation.id) != Some(record.id) {
                            continue;
                        }
                        mount.docs.remove(kind);
                        Arc::make_mut(&mut next.docs).remove(kind);
                        doc_ops.push(Op::Delete(path));
                    }
                    Some(value) => {
                        match mounted {
                            Some(incarnation)
                                if incarnation.id == record.id
                                    && Some(incarnation.version) == *version =>
                            {
                                // Same shape: forward the exact operations and
                                // take the publication's complete value.
                                for op in ops {
                                    doc_ops.push(prefixed(op, &path));
                                }
                            }
                            _ => {
                                mount.docs.insert(
                                    kind.to_owned(),
                                    Incarnation {
                                        id: record.id,
                                        version: version.unwrap_or_default(),
                                    },
                                );
                                doc_ops.push(Op::Set(path, Value::Object(value.clone())));
                            }
                        }
                        // One handle swap: the documents this publication did
                        // not touch stay in their existing allocation.
                        Arc::make_mut(&mut next.docs)
                            .insert(kind.to_owned(), Arc::new(value.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    doc_ops.extend(entry_ops);
    doc_ops
}

/// `op` moved under `prefix`; a root replacement becomes a set of the prefix.
fn prefixed(op: &Op, prefix: &[Seg]) -> Op {
    let at =
        |path: &[Seg]| -> Vec<Seg> { prefix.iter().cloned().chain(path.iter().cloned()).collect() };
    match op {
        Op::Replace(value) => Op::Set(prefix.to_vec(), value.clone()),
        Op::Set(path, value) => Op::Set(at(path), value.clone()),
        Op::Delete(path) => Op::Delete(at(path)),
        Op::Append(path, text) => Op::Append(at(path), text.clone()),
        Op::Truncate(path, count) => Op::Truncate(at(path), *count),
        Op::Splice {
            path,
            index,
            remove,
            items,
        } => Op::Splice {
            path: at(path),
            index: *index,
            remove: *remove,
            items: items.clone(),
        },
        Op::Move { path, permutation } => Op::Move {
            path: at(path),
            permutation: permutation.clone(),
        },
    }
}

fn entries_path() -> Vec<Seg> {
    vec![Seg::Key("entries".to_owned())]
}

fn doc_path(kind: &str) -> Vec<Seg> {
    vec![Seg::Key("docs".to_owned()), Seg::Key(kind.to_owned())]
}

/// Close every mount: notify each observer, then forget the mounts
/// (upstream `subscribeClose`).
fn close_mounts(shared: &Arc<Mutex<SharedInner>>, failure: Option<&StorageError>, report: &Report) {
    let mounts: Vec<Arc<Mutex<Mount>>> = {
        let mut inner = lock(shared);
        inner.closed = true;
        std::mem::take(&mut inner.mounts).into_values().collect()
    };
    for mount in mounts {
        let observers: Vec<Arc<dyn ViewObserver>> = {
            let mut mount = lock(&mount);
            std::mem::take(&mut mount.observers)
                .into_iter()
                .map(|(_, observer)| observer)
                .collect()
        };
        for observer in observers {
            contained(report, || observer.close_session(failure));
        }
    }
}
