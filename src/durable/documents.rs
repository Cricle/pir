//! Document definitions, logical addresses and typed-access checks.
//!
//! Ports upstream `src/documents.ts`. The upstream file carries the overloaded
//! `defineDoc`/`defineDocFamily` factories whose TypeScript overloads select the
//! scope; Rust expresses the same choices with explicit constructors.
//!
//! Two definition shapes exist:
//!
//! * [`DocDefinition`] / [`DocFamilyDefinition`] are the *identity* form: a
//!   validated kind, version and [`DocumentSemantics`]. They were the whole of
//!   the earlier port, and their constructors are kept unchanged for callers
//!   that only need to build a storage create record.
//! * [`AnyDocDefinition`] / [`AnyDocFamilyDefinition`] are the *behaviour* form,
//!   named after upstream's erased `AnyDocDefinition`. They add the three
//!   members upstream's `CommonDocDefinition` carries: `initial()`, `migrate`
//!   and `checkpointWhen`. The Session kernel uses these; the identity form is
//!   what a storage-only caller passes to [`document_create`].
//!
//! The behaviour form is not called `DocDefinition` because the name is taken
//! by the existing identity type, whose field access (`definition.version`)
//! existing callers rely on.

use crate::durable::errors::{DocumentError, StorageError, StorageRequestError};
use crate::durable::ids::{ConversationId, DocumentId, TaskId};
use crate::durable::types::{
    DocumentAddress, DocumentCreate, DocumentFork, DocumentHistory, DocumentRecord, DocumentScope,
    JsonObject, StoredDocument,
};
use serde_json::Value;

/// Ownership and lifetime semantics of a document definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentSemantics {
    /// One session-wide singleton/family.
    Session,
    /// A latest-only conversation document.
    ConversationLatest {
        /// How a fork initializes.
        fork: DocumentFork,
    },
    /// A conversation document that keeps history for as-of reads.
    ConversationRewindable {
        /// How a fork initializes.
        fork: DocumentFork,
    },
    /// A task-scoped singleton/family.
    Task,
}

impl DocumentSemantics {
    /// The scope discriminator.
    pub fn scope_kind(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::ConversationLatest { .. } | Self::ConversationRewindable { .. } => "conversation",
            Self::Task => "task",
        }
    }

    /// The history semantics, for conversation documents.
    pub fn history(self) -> Option<DocumentHistory> {
        match self {
            Self::ConversationLatest { .. } => Some(DocumentHistory::Latest),
            Self::ConversationRewindable { .. } => Some(DocumentHistory::Rewindable),
            Self::Session | Self::Task => None,
        }
    }

    /// The fork semantics, for conversation documents.
    pub fn fork(self) -> Option<DocumentFork> {
        match self {
            Self::ConversationLatest { fork } | Self::ConversationRewindable { fork } => Some(fork),
            Self::Session | Self::Task => None,
        }
    }
}

/// A validated singleton document definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocDefinition {
    /// Stable persisted kind; part of the public protocol.
    pub kind: String,
    /// Positive integer version of the stored value shape.
    pub version: i64,
    /// Ownership and lifetime semantics.
    pub semantics: DocumentSemantics,
}

/// A validated keyed document family definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocFamilyDefinition {
    /// Stable persisted kind; part of the public protocol.
    pub kind: String,
    /// Positive integer version of the stored value shape.
    pub version: i64,
    /// Ownership and lifetime semantics.
    pub semantics: DocumentSemantics,
}

impl DocDefinition {
    /// Define a singleton document. `version` must be a positive integer.
    pub fn define(
        kind: impl Into<String>,
        version: i64,
        semantics: DocumentSemantics,
    ) -> Result<Self, StorageRequestError> {
        validate(kind.into(), version, semantics).map(|(kind, version)| Self {
            kind,
            version,
            semantics,
        })
    }

    /// Define a Session-scoped singleton.
    pub fn session(kind: impl Into<String>, version: i64) -> Result<Self, StorageRequestError> {
        Self::define(kind, version, DocumentSemantics::Session)
    }

    /// Define a latest-only conversation singleton.
    pub fn conversation_latest(
        kind: impl Into<String>,
        version: i64,
        fork: DocumentFork,
    ) -> Result<Self, StorageRequestError> {
        Self::define(
            kind,
            version,
            DocumentSemantics::ConversationLatest { fork },
        )
    }

    /// Define a rewindable conversation singleton.
    pub fn conversation_rewindable(
        kind: impl Into<String>,
        version: i64,
        fork: DocumentFork,
    ) -> Result<Self, StorageRequestError> {
        Self::define(
            kind,
            version,
            DocumentSemantics::ConversationRewindable { fork },
        )
    }

    /// Define a task-scoped singleton.
    pub fn task(kind: impl Into<String>, version: i64) -> Result<Self, StorageRequestError> {
        Self::define(kind, version, DocumentSemantics::Task)
    }

    /// Lift this validated identity into the behaviour-carrying form.
    pub fn behaviour(
        self,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> AnyDocDefinition {
        AnyDocDefinition::from_definition(self, initial, migrate, checkpoint_when)
    }
}

impl DocFamilyDefinition {
    /// Define a keyed document family. `version` must be a positive integer.
    pub fn define(
        kind: impl Into<String>,
        version: i64,
        semantics: DocumentSemantics,
    ) -> Result<Self, StorageRequestError> {
        validate(kind.into(), version, semantics).map(|(kind, version)| Self {
            kind,
            version,
            semantics,
        })
    }

    /// Define a Session-scoped family.
    pub fn session(kind: impl Into<String>, version: i64) -> Result<Self, StorageRequestError> {
        Self::define(kind, version, DocumentSemantics::Session)
    }

    /// Define a latest-only conversation family.
    pub fn conversation_latest(
        kind: impl Into<String>,
        version: i64,
        fork: DocumentFork,
    ) -> Result<Self, StorageRequestError> {
        Self::define(
            kind,
            version,
            DocumentSemantics::ConversationLatest { fork },
        )
    }

    /// Define a rewindable conversation family.
    pub fn conversation_rewindable(
        kind: impl Into<String>,
        version: i64,
        fork: DocumentFork,
    ) -> Result<Self, StorageRequestError> {
        Self::define(
            kind,
            version,
            DocumentSemantics::ConversationRewindable { fork },
        )
    }

    /// Define a task-scoped family.
    pub fn task(kind: impl Into<String>, version: i64) -> Result<Self, StorageRequestError> {
        Self::define(kind, version, DocumentSemantics::Task)
    }

    /// Lift this validated identity into the behaviour-carrying form.
    pub fn behaviour(
        self,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> AnyDocFamilyDefinition {
        AnyDocFamilyDefinition::from_definition(self, initial, migrate, checkpoint_when)
    }
}

fn validate(
    kind: String,
    version: i64,
    semantics: DocumentSemantics,
) -> Result<(String, i64), StorageRequestError> {
    if version < 1 {
        return Err(StorageRequestError::new(format!(
            "Document {kind} version must be a positive integer"
        )));
    }
    // Upstream states this in the type system, not at runtime:
    // `LatestConversationSemantics` declares `fork: "current" | "initial"`, so
    // a latest-only document cannot ask to be forked as-of a sequence it does
    // not keep. The Rust enum cannot express that, so the pair is rejected
    // here — otherwise a fork would silently copy current state and the child
    // would see a value the parent never had at the fork point.
    if let DocumentSemantics::ConversationLatest {
        fork: DocumentFork::AsOf,
    } = semantics
    {
        return Err(StorageRequestError::new(format!(
            "Document {kind} is latest-only and cannot declare asOf fork semantics"
        )));
    }
    Ok((kind, version))
}

// ─── Behaviour-carrying definitions ─────────────────────────────────────────

/// The initial value of a document or family member.
pub type DocInitial = fn() -> JsonObject;

/// Migrate a stored value from an older version to the definition's version.
///
/// The first argument is the version the value was stored at, matching the
/// upstream `migrate(value, fromVersion)` argument order once the value is
/// passed second. The existing [`materialize_document_value`] helper takes the
/// value first because it mirrors upstream's closure shape directly; the two
/// are bridged by [`AnyDocDefinition::materialize_value`].
pub type DocMigrate = fn(i64, Value) -> Result<Value, DocumentError>;

/// Decide whether a prepared value is committed as a complete base.
///
/// `true` means "commit this change as a base instead of a delta", matching
/// upstream `checkpointWhen`. The Session kernel calls it after every
/// successful prepare; `None` never forces a checkpoint. Upstream also passes
/// the pending operations and the stored-delta count; that state belongs to
/// the caller here, which can capture it when it builds the definition.
pub type DocCheckpointWhen = fn(&JsonObject) -> bool;

/// Definition fields shared by singleton documents and document families.
///
/// Upstream `CommonDocDefinition` carries `kind`, `version`, `initial`,
/// `migrate` and `checkpointWhen`; the scope/history/fork triple is held by
/// [`DocumentSemantics`], which already encodes the same choices and the valid
/// fork set for each history mode. Behaviour lives in [`AnyDocDefinition`] /
/// [`AnyDocFamilyDefinition`], which wrap this common part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonDocDefinition {
    /// Stable persisted kind; part of the public protocol.
    pub kind: String,
    /// Positive integer version of the stored value shape.
    pub version: i64,
    /// Ownership and lifetime semantics.
    pub semantics: DocumentSemantics,
}

impl CommonDocDefinition {
    /// Build and validate the common part. `version` must be a positive
    /// integer.
    pub fn new(
        kind: impl Into<String>,
        version: i64,
        semantics: DocumentSemantics,
    ) -> Result<Self, StorageRequestError> {
        validate(kind.into(), version, semantics).map(|(kind, version)| Self {
            kind,
            version,
            semantics,
        })
    }

    /// A Session-scoped common part.
    pub fn session(kind: impl Into<String>, version: i64) -> Result<Self, StorageRequestError> {
        Self::new(kind, version, DocumentSemantics::Session)
    }

    /// A latest-only conversation common part.
    pub fn conversation_latest(
        kind: impl Into<String>,
        version: i64,
        fork: DocumentFork,
    ) -> Result<Self, StorageRequestError> {
        Self::new(
            kind,
            version,
            DocumentSemantics::ConversationLatest { fork },
        )
    }

    /// A rewindable conversation common part.
    pub fn conversation_rewindable(
        kind: impl Into<String>,
        version: i64,
        fork: DocumentFork,
    ) -> Result<Self, StorageRequestError> {
        Self::new(
            kind,
            version,
            DocumentSemantics::ConversationRewindable { fork },
        )
    }

    /// A task-scoped common part.
    pub fn task(kind: impl Into<String>, version: i64) -> Result<Self, StorageRequestError> {
        Self::new(kind, version, DocumentSemantics::Task)
    }

    /// Lift the common part into a singleton definition.
    pub fn behaviour(
        self,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> AnyDocDefinition {
        AnyDocDefinition::define(self, initial, migrate, checkpoint_when)
    }

    /// Lift the common part into a family definition.
    pub fn behaviour_family(
        self,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> AnyDocFamilyDefinition {
        AnyDocFamilyDefinition::define(self, initial, migrate, checkpoint_when)
    }
}

/// One singleton document definition and its behaviour.
///
/// Named after upstream's erased `AnyDocDefinition`. The name [`DocDefinition`]
/// stays on the identity-only struct so existing storage callers keep working;
/// use [`DocDefinition::behaviour`] to lift one into this form.
#[derive(Clone)]
pub struct AnyDocDefinition {
    /// Identity and ownership semantics.
    pub common: CommonDocDefinition,
    /// The value a new incarnation starts from.
    pub initial: DocInitial,
    /// Transform a value stored at an older version; `None` rejects them.
    pub migrate: Option<DocMigrate>,
    /// Whether a prepared change must be committed as a complete base.
    pub checkpoint_when: Option<DocCheckpointWhen>,
}

impl std::fmt::Debug for AnyDocDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyDocDefinition")
            .field("common", &self.common)
            .field("has_migrate", &self.migrate.is_some())
            .field("has_checkpoint_when", &self.checkpoint_when.is_some())
            .finish()
    }
}

impl AnyDocDefinition {
    /// Define a singleton document with behaviour. `common.version` must be a
    /// positive integer.
    pub fn define(
        common: CommonDocDefinition,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> Self {
        Self {
            common,
            initial,
            migrate,
            checkpoint_when,
        }
    }

    /// The initial value for a new incarnation.
    pub fn initial_value(&self) -> JsonObject {
        (self.initial)()
    }

    /// Whether the definition declares a migration.
    pub fn has_migration(&self) -> bool {
        self.migrate.is_some()
    }

    /// Whether a prepared value must be committed as a complete base.
    pub fn forces_checkpoint(&self, value: &JsonObject) -> bool {
        match self.checkpoint_when {
            Some(predicate) => predicate(value),
            None => false,
        }
    }

    /// Reject a stored version this definition cannot use.
    ///
    /// The behaviour-aware counterpart of [`check_record_version`]: a newer
    /// version is rejected, an older one only when no migration is declared.
    pub fn check_record_version(
        &self,
        record_id: DocumentId,
        record_kind: &str,
        version: i64,
    ) -> Result<(), StorageError> {
        if version > self.common.version {
            return Err(StorageError::invalid(format!(
                "Document {record_id} ({record_kind}) has newer version {version} than {}",
                self.common.version
            )));
        }
        if version < self.common.version && self.migrate.is_none() {
            return Err(StorageError::invalid(format!(
                "Document {record_id} ({record_kind}) requires migration from version {version}"
            )));
        }
        Ok(())
    }

    /// Migrate a value stored at `from_version` to the definition's version.
    ///
    /// A value already at the current version is returned unchanged.
    pub fn migrate_value(&self, value: Value, from_version: i64) -> Result<Value, DocumentError> {
        if from_version > self.common.version {
            return Err(DocumentError::new(format!(
                "Document {} has newer version {from_version} than {}",
                self.common.kind, self.common.version
            )));
        }
        if from_version == self.common.version {
            return Ok(value);
        }
        match self.migrate {
            Some(migrate) => migrate(from_version, value),
            None => Err(DocumentError::new(format!(
                "Document {} requires migration from version {from_version}",
                self.common.kind
            ))),
        }
    }

    /// Validate and migrate one detached value, as typed access does.
    ///
    /// The value-first wrapper over the existing
    /// [`materialize_document_value`], which needs a borrowed migration
    /// closure; this adapts the definition's [`DocMigrate`] to it.
    pub fn materialize_value(
        &self,
        record: &DocumentRecord,
        version: i64,
        value: JsonObject,
    ) -> Result<JsonObject, StorageError> {
        check_stored_record_scope(self.common.semantics, record)?;
        self.check_record_version(record.id, &record.kind, version)?;
        if version == self.common.version {
            return Ok(value);
        }
        let migrated = self.migrate_value(Value::Object(value), version)?;
        match migrated {
            Value::Object(value) => Ok(value),
            _ => Err(StorageError::invalid(format!(
                "Document {} migration must return a JSON object",
                record.id
            ))),
        }
    }

    /// Lift an identity-only definition into the behaviour-carrying form.
    pub fn from_definition(
        definition: DocDefinition,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> Self {
        Self {
            common: CommonDocDefinition {
                kind: definition.kind,
                version: definition.version,
                semantics: definition.semantics,
            },
            initial,
            migrate,
            checkpoint_when,
        }
    }
}

/// One keyed document family definition and its behaviour.
///
/// Named after upstream's erased `AnyDocDefinition` for families. Family
/// members share one definition; `initial` runs only when a member is absent.
/// Upstream's family `initial(seed)` takes a seed; this port has no seed
/// because a plain function pointer cannot capture it — the caller that
/// creates the member builds the value instead.
#[derive(Clone)]
pub struct AnyDocFamilyDefinition {
    /// Identity and ownership semantics.
    pub common: CommonDocDefinition,
    /// The value a new member starts from.
    pub initial: DocInitial,
    /// Transform a member value stored at an older version; `None` rejects
    /// them.
    pub migrate: Option<DocMigrate>,
    /// Whether a prepared change must be committed as a complete base.
    pub checkpoint_when: Option<DocCheckpointWhen>,
}

impl std::fmt::Debug for AnyDocFamilyDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyDocFamilyDefinition")
            .field("common", &self.common)
            .field("has_migrate", &self.migrate.is_some())
            .field("has_checkpoint_when", &self.checkpoint_when.is_some())
            .finish()
    }
}

impl AnyDocFamilyDefinition {
    /// Define a document family with behaviour. `common.version` must be a
    /// positive integer.
    pub fn define(
        common: CommonDocDefinition,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> Self {
        Self {
            common,
            initial,
            migrate,
            checkpoint_when,
        }
    }

    /// Whether this definition describes a family. Always `true`; it mirrors
    /// upstream's `family: true` discriminator.
    pub fn is_family(&self) -> bool {
        true
    }

    /// The initial value for a new member.
    pub fn initial_value(&self) -> JsonObject {
        (self.initial)()
    }

    /// Whether the definition declares a migration.
    pub fn has_migration(&self) -> bool {
        self.migrate.is_some()
    }

    /// Whether a prepared value must be committed as a complete base.
    pub fn forces_checkpoint(&self, value: &JsonObject) -> bool {
        match self.checkpoint_when {
            Some(predicate) => predicate(value),
            None => false,
        }
    }

    /// Reject a stored version this definition cannot use.
    pub fn check_record_version(
        &self,
        record_id: DocumentId,
        record_kind: &str,
        version: i64,
    ) -> Result<(), StorageError> {
        if version > self.common.version {
            return Err(StorageError::invalid(format!(
                "Document {record_id} ({record_kind}) has newer version {version} than {}",
                self.common.version
            )));
        }
        if version < self.common.version && self.migrate.is_none() {
            return Err(StorageError::invalid(format!(
                "Document {record_id} ({record_kind}) requires migration from version {version}"
            )));
        }
        Ok(())
    }

    /// Migrate a member value stored at `from_version` to the current version.
    pub fn migrate_value(&self, value: Value, from_version: i64) -> Result<Value, DocumentError> {
        if from_version > self.common.version {
            return Err(DocumentError::new(format!(
                "Document {} has newer version {from_version} than {}",
                self.common.kind, self.common.version
            )));
        }
        if from_version == self.common.version {
            return Ok(value);
        }
        match self.migrate {
            Some(migrate) => migrate(from_version, value),
            None => Err(DocumentError::new(format!(
                "Document {} requires migration from version {from_version}",
                self.common.kind
            ))),
        }
    }

    /// Lift an identity-only family definition into the behaviour form.
    pub fn from_definition(
        definition: DocFamilyDefinition,
        initial: DocInitial,
        migrate: Option<DocMigrate>,
        checkpoint_when: Option<DocCheckpointWhen>,
    ) -> Self {
        Self {
            common: CommonDocDefinition {
                kind: definition.kind,
                version: definition.version,
                semantics: definition.semantics,
            },
            initial,
            migrate,
            checkpoint_when,
        }
    }
}

// ─── Tokens ─────────────────────────────────────────────────────────────────

/// Which owner a document token's scope needs.
///
/// The Rust analogue of upstream's `SessionDocToken`, `ConversationDocToken`
/// and `TaskDocToken` aliases, which differ only in their scope type parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocTokenKind {
    /// One session-wide document.
    Session,
    /// A conversation document.
    Conversation,
    /// A task document.
    Task,
}

impl DocTokenKind {
    /// The token kind a definition's semantics require.
    pub fn of(semantics: DocumentSemantics) -> Self {
        match semantics {
            DocumentSemantics::Session => Self::Session,
            DocumentSemantics::ConversationLatest { .. }
            | DocumentSemantics::ConversationRewindable { .. } => Self::Conversation,
            DocumentSemantics::Task => Self::Task,
        }
    }
}

/// A typed singleton document token, passed explicitly to typed access.
#[derive(Clone, Debug)]
pub struct DocToken {
    /// Which owner the scope needs.
    pub kind: DocTokenKind,
    /// The definition.
    pub definition: AnyDocDefinition,
}

impl DocToken {
    /// Wrap a definition, deriving the token kind from its semantics.
    pub fn new(definition: AnyDocDefinition) -> Self {
        Self {
            kind: DocTokenKind::of(definition.common.semantics),
            definition,
        }
    }
}

/// A typed document family token, passed explicitly to typed access.
#[derive(Clone, Debug)]
pub struct DocFamilyToken {
    /// Which owner the scope needs.
    pub kind: DocTokenKind,
    /// The definition.
    pub definition: AnyDocFamilyDefinition,
}

impl DocFamilyToken {
    /// Wrap a family definition, deriving the token kind from its semantics.
    pub fn new(definition: AnyDocFamilyDefinition) -> Self {
        Self {
            kind: DocTokenKind::of(definition.common.semantics),
            definition,
        }
    }
}

/// Define a session, conversation or task document (upstream `defineDoc`).
pub fn define_doc(definition: AnyDocDefinition) -> DocToken {
    DocToken::new(definition)
}

/// Define a document family (upstream `defineDocFamily`).
pub fn define_doc_family(definition: AnyDocFamilyDefinition) -> DocFamilyToken {
    DocFamilyToken::new(definition)
}

/// The owner a scoped definition needs, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocOwner {
    /// A conversation-scoped document.
    Conversation(ConversationId),
    /// A task-scoped document.
    Task(TaskId),
}

/// A resolved logical address plus the index after the owner and family key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAddress {
    /// The logical address.
    pub address: DocumentAddress,
    /// Stable string identity of the address.
    pub id: String,
}

impl ResolvedAddress {
    /// Resolve an address from an explicit owner and family key.
    pub fn new(
        kind: impl Into<String>,
        semantics: DocumentSemantics,
        owner: Option<DocOwner>,
        key: Option<String>,
    ) -> Result<Self, StorageError> {
        let scope = match (semantics.scope_kind(), owner) {
            ("session", None) => DocumentScope::Session,
            ("conversation", Some(DocOwner::Conversation(conversation_id))) => {
                DocumentScope::Conversation { conversation_id }
            }
            ("task", Some(DocOwner::Task(task_id))) => DocumentScope::Task { task_id },
            (scope, _) => {
                return Err(StorageError::Request(StorageRequestError::new(format!(
                    "Document requires a {scope} owner"
                ))));
            }
        };
        let address = DocumentAddress {
            kind: kind.into(),
            scope,
            key,
        };
        let id = address_id(&address);
        Ok(Self { address, id })
    }
}

/// Stable string identity of one logical address.
pub fn address_id(address: &DocumentAddress) -> String {
    let owner = match &address.scope {
        DocumentScope::Session => "null".to_owned(),
        DocumentScope::Conversation { conversation_id } => conversation_id.as_i64().to_string(),
        DocumentScope::Task { task_id } => task_id.as_i64().to_string(),
    };
    // `{:?}` is a stable, injective rendering for these strings and cannot
    // fail, so the identity of an address never depends on serialization.
    let key = match &address.key {
        Some(key) => format!("{key:?}"),
        None => "null".to_owned(),
    };
    format!(
        "[{:?}, {:?}, {}, {}]",
        address.kind,
        address.scope.kind(),
        owner,
        key
    )
}

/// Build the storage create record for a new incarnation at an address.
pub fn document_create(
    semantics: DocumentSemantics,
    address: &DocumentAddress,
    id: DocumentId,
) -> DocumentCreate {
    DocumentCreate {
        id,
        kind: address.kind.clone(),
        key: address.key.clone(),
        scope: address.scope,
        history: semantics.history(),
        fork: semantics.fork(),
    }
}

/// Reject typed access whose definition disagrees with the persisted scope,
/// history or fork semantics.
pub fn check_record_scope(
    semantics: DocumentSemantics,
    record: &DocumentCreate,
) -> Result<(), StorageError> {
    let matches = record.scope.kind() == semantics.scope_kind()
        && match &record.scope {
            DocumentScope::Conversation { .. } => {
                record.history == semantics.history() && record.fork == semantics.fork()
            }
            DocumentScope::Session | DocumentScope::Task { .. } => true,
        };
    if matches {
        Ok(())
    } else {
        Err(StorageError::invalid(format!(
            "Document {} ({}) does not match the supplied definition semantics",
            record.id, record.kind
        )))
    }
}

/// Reject typed access whose definition disagrees with a stored record.
pub fn check_stored_record_scope(
    semantics: DocumentSemantics,
    record: &DocumentRecord,
) -> Result<(), StorageError> {
    let matches = record.scope.kind() == semantics.scope_kind()
        && match &record.scope {
            DocumentScope::Conversation { .. } => {
                record.history == semantics.history() && record.fork == semantics.fork()
            }
            DocumentScope::Session | DocumentScope::Task { .. } => true,
        };
    if matches {
        Ok(())
    } else {
        Err(StorageError::invalid(format!(
            "Document {} ({}) does not match the supplied definition semantics",
            record.id, record.kind
        )))
    }
}

/// Reject typed access to a stored version the definition cannot use.
///
/// A newer stored version is always rejected. An older one is rejected unless
/// the caller supplies a migration function. This identity-only signature is
/// kept for existing callers; definitions that carry their own `migrate`
/// ([`AnyDocDefinition::check_record_version`]) use it directly.
pub fn check_record_version(
    definition: &DocDefinition,
    record_id: DocumentId,
    record_kind: &str,
    version: i64,
    has_migration: bool,
) -> Result<(), StorageError> {
    if version > definition.version {
        return Err(StorageError::invalid(format!(
            "Document {record_id} ({record_kind}) has newer version {version} than {}",
            definition.version
        )));
    }
    if version < definition.version && !has_migration {
        return Err(StorageError::invalid(format!(
            "Document {record_id} ({record_kind}) requires migration from version {version}"
        )));
    }
    Ok(())
}

/// Validate and materialize a detached stored value for typed access.
///
/// The detached closure form is kept for callers that pass their own
/// migration ([`AnyDocDefinition::materialize_value`] adapts a definition's
/// [`DocMigrate`] to it).
pub fn materialize_document(
    definition: &DocDefinition,
    stored: &StoredDocument,
    migrate: Option<&dyn Fn(JsonObject, i64) -> Result<JsonObject, StorageError>>,
) -> Result<JsonObject, StorageError> {
    materialize_document_value(
        definition,
        &stored.record,
        stored.version,
        stored.value.clone(),
        migrate,
    )
}

/// Validate and materialize one detached value before its first persisted
/// incarnation.
pub fn materialize_document_value(
    definition: &DocDefinition,
    record: &DocumentRecord,
    version: i64,
    value: JsonObject,
    migrate: Option<&dyn Fn(JsonObject, i64) -> Result<JsonObject, StorageError>>,
) -> Result<JsonObject, StorageError> {
    check_stored_record_scope(definition.semantics, record)?;
    check_record_version(
        definition,
        record.id,
        &record.kind,
        version,
        migrate.is_some(),
    )?;
    if version == definition.version {
        return Ok(value);
    }
    match migrate {
        Some(migrate) => migrate(value, version),
        None => Err(StorageError::invalid(format!(
            "Document {} ({}) requires migration from version {version}",
            record.id, record.kind
        ))),
    }
}
