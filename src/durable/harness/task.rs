//! Executable task definitions and their erasure.
//!
//! Port of upstream `packages/durable/src/harness/types.ts`'s `Task`,
//! `TaskDefinition`, `PhaseHandler` and `NextTaskState`, plus `src/tasks.ts`'s
//! `defineTask`.
//!
//! # Shapes
//!
//! Upstream is generic in the input `I`, checkpoint `S`, result `R` and the
//! host hook object `H`:
//!
//! ```text
//! TaskDefinition<I, S extends { phase: string }, R, H> {
//!   name, version, initial(input) -> S,
//!   phases: { [P in S["phase"]]: PhaseHandler<...> },
//!   abort(task, runtime, context), migrate?(input, checkpoint, from) -> { input, checkpoint },
//!   hooks?
//! }
//! ```
//!
//! The scheduler is deliberately **not** generic: it stores `AnyTaskRecord`
//! (a [`TaskRecord`] whose input, checkpoint and result are `serde_json::Value`)
//! and erases the definition behind [`ErasedTask`]. This module keeps exactly
//! the same split — the erased surface is [`TaskDefinition`], the token the
//! registry holds is [`Task`], and the scheduler's map is
//! `Arc<dyn ErasedTask>`.
//!
//! # Migration
//!
//! Upstream's `migrate` returns both a migrated input and checkpoint. The
//! lane brief specifies `fn(i64, &Value) -> Result<Value, String>`, which can
//! only carry one value. [`Migrate`] therefore has two forms: [`Migrate::Checkpoint`]
//! (the brief's shape, transforming the checkpoint) and [`Migrate::Full`]
//! (upstream's shape, transforming input and checkpoint). Both run inside the
//! reservation commit, atomically with `pending`/`waiting`/`sleeping` ->
//! `running`.
//!
//! # Runtime reference
//!
//! Handlers receive `&RunningTask` and `&TaskRuntime`. `TaskRuntime` lives in
//! [`crate::durable::harness::scheduler::runtime`]; the reference is safe in
//! both directions (Rust allows mutually dependent modules in one crate).

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::durable::context::Context;
use crate::durable::harness::scheduler::runtime::TaskRuntime;
use crate::durable::ids::{ConversationId, TaskId};
use crate::durable::types::{JsonObject, TaskRecord, TaskState};

/// A boxed, `Send` future returned by a phase or abort handler.
pub type BoxFuture<'a, T = ()> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One phase or abort handler: `(task, runtime, context) -> future`.
///
/// The scheduler calls a handler off the Session line; everything a handler
/// does durably goes through `runtime.commit()` and its gated siblings, each of
/// which lands as one commit on the line.
pub type PhaseHandler = Arc<
    dyn for<'a> Fn(&'a RunningTask, &'a TaskRuntime, Context) -> BoxFuture<'a, ()> + Send + Sync,
>;

/// Build a [`PhaseHandler`] from a closure.
///
/// Exists so call sites do not have to spell the higher-ranked bound:
///
/// ```ignore
/// let handler = phase_handler(|task: &RunningTask, runtime: &TaskRuntime, context: Context| {
///     Box::pin(async move { /* ... */ })
/// });
/// ```
pub fn phase_handler<F>(handler: F) -> PhaseHandler
where
    F: for<'a> Fn(&'a RunningTask, &'a TaskRuntime, Context) -> BoxFuture<'a, ()>
        + Send
        + Sync
        + 'static,
{
    Arc::new(handler)
}

/// The complete durable state a new task starts from.
pub type InitialFn = fn(&Value) -> Value;

/// A migrated task input and checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Migrated {
    /// The input at the new definition version.
    pub input: Value,
    /// The checkpoint at the new definition version.
    pub checkpoint: Value,
}

/// How an older stored version migrates to the definition's version.
#[derive(Clone, Copy)]
pub enum Migrate {
    /// `(from_version, checkpoint) -> checkpoint`.
    ///
    /// The shape the lane brief specifies. Use it when only the checkpoint
    /// changed shape; the input is carried over verbatim.
    Checkpoint(fn(i64, &Value) -> Result<Value, String>),
    /// `(from_version, input, checkpoint) -> (input, checkpoint)`.
    ///
    /// Upstream's shape, for migrations that reshape the input too.
    Full(fn(i64, &Value, &Value) -> Result<Migrated, String>),
}

impl fmt::Debug for Migrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Checkpoint(_) => "checkpoint",
            Self::Full(_) => "full",
        };
        f.write_str(kind)
    }
}

impl Migrate {
    /// Run the migration over a stored input and checkpoint.
    pub fn apply(
        self,
        from_version: i64,
        input: &Value,
        checkpoint: &Value,
    ) -> Result<Migrated, String> {
        match self {
            Self::Checkpoint(migrate) => Ok(Migrated {
                input: input.clone(),
                checkpoint: migrate(from_version, checkpoint)?,
            }),
            Self::Full(migrate) => migrate(from_version, input, checkpoint),
        }
    }
}

/// A running task, as a phase handler sees it.
///
/// Upstream hands the handler the `RunningTask` record. The Rust port narrows
/// that to the fields a handler reads: the record's immutable identity, its
/// input, and the checkpoint of the current state. `memos`, `abort` and the
/// ownership fields are carried too, because an abort handler legitimately
/// inspects the mark it was started for.
#[derive(Clone, Debug, PartialEq)]
pub struct RunningTask {
    /// Task identifier.
    pub id: TaskId,
    /// Conversation that owns the task.
    pub conversation_id: ConversationId,
    /// Registered definition name.
    pub kind: String,
    /// Definition version the record is stored at.
    pub version: i64,
    /// Original input, retained for the life of the task.
    pub input: Value,
    /// Complete checkpoint of the current `running` state.
    pub checkpoint: Value,
    /// First-writer-wins values retained while the task can run.
    pub memos: Option<JsonObject>,
    /// Durable abort mark.
    pub abort_requested: bool,
    /// `restart` when the mark abandons the task after a restart.
    pub abort_reason: Option<String>,
    /// Whether this conversation-owned task is excluded from idle and abort.
    pub background: bool,
}

impl RunningTask {
    /// Project a stored record, if it is `running`.
    pub fn from_record(record: &TaskRecord) -> Option<Self> {
        let TaskState::Running { checkpoint } = &record.state else {
            return None;
        };
        Some(Self {
            id: record.id,
            conversation_id: record.conversation_id,
            kind: record.kind.clone(),
            version: record.version,
            input: record.input.clone(),
            checkpoint: checkpoint.clone(),
            memos: record.memos.clone(),
            abort_requested: record.abort_requested,
            abort_reason: record.abort_reason.clone(),
            background: record.background,
        })
    }

    /// The checkpoint's `phase` field, when it is a string.
    #[must_use]
    pub fn phase(&self) -> Option<&str> {
        self.checkpoint.get("phase").and_then(Value::as_str)
    }

    /// Project the record for an abort handler.
    ///
    /// Upstream's abort handler receives the `RunningTask`; a task that was
    /// abort-marked while `waiting` or `sleeping` left that state before its
    /// abort invocation starts (spec §5.5), so the port projects the stored
    /// checkpoint into a `running` view. The handler therefore sees the
    /// checkpoint it would have resumed from, and the durable mark.
    #[must_use]
    pub fn for_abort(record: &TaskRecord) -> Option<Self> {
        if let Some(running) = Self::from_record(record) {
            return Some(running);
        }
        Some(Self {
            id: record.id,
            conversation_id: record.conversation_id,
            kind: record.kind.clone(),
            version: record.version,
            input: record.input.clone(),
            checkpoint: crate::durable::harness::scheduler::checkpoint_of(&record.state).clone(),
            memos: record.memos.clone(),
            abort_requested: record.abort_requested,
            abort_reason: record.abort_reason.clone(),
            background: record.background,
        })
    }
}

/// One executable task definition, erased over its input/checkpoint/result
/// types.
///
/// The scheduler stores `Arc<dyn ErasedTask>`; input, checkpoint, memos and
/// outcomes stay `serde_json::Value` on the durable record, exactly as
/// upstream erases the generics at the scheduler boundary.
pub struct TaskDefinition {
    /// Registered definition name (`TaskRecord.kind`).
    pub name: String,
    /// Definition version used to migrate live input and checkpoints.
    pub version: i64,
    /// Complete durable state a new task starts from.
    pub initial: InitialFn,
    /// Phase handlers, keyed by the checkpoint's `phase`.
    pub phases: BTreeMap<String, PhaseHandler>,
    /// Abort handler. A task without one cannot settle `aborted`; its abort
    /// invocation faults instead (see spec §5.1).
    pub abort: Option<PhaseHandler>,
    /// Migration from an older stored version.
    pub migrate: Option<Migrate>,
    /// Opaque host hook object, matched by task name by the hooks lane.
    pub hooks: Option<Arc<dyn Any + Send + Sync>>,
}

impl fmt::Debug for TaskDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskDefinition")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("phases", &self.phases.keys().collect::<Vec<_>>())
            .field("has_abort", &self.abort.is_some())
            .field("migrate", &self.migrate)
            .field("has_hooks", &self.hooks.is_some())
            .finish()
    }
}

impl TaskDefinition {
    /// The phase handler named by `checkpoint`'s `phase` field.
    #[must_use]
    pub fn phase_for(&self, checkpoint: &Value) -> Option<&PhaseHandler> {
        checkpoint
            .get("phase")
            .and_then(Value::as_str)
            .and_then(|phase| self.phases.get(phase))
    }

    /// Whether `checkpoint` names a declared phase.
    #[must_use]
    pub fn knows_phase(&self, checkpoint: &Value) -> bool {
        checkpoint
            .get("phase")
            .and_then(Value::as_str)
            .is_some_and(|phase| self.phases.contains_key(phase))
    }

    /// Whether this definition can take over a task stored at `version`.
    ///
    /// Same version, or a newer one with a migration. This is the handover
    /// predicate of spec §5.4.
    #[must_use]
    pub fn can_reserve_version(&self, version: i64) -> bool {
        self.version == version || (self.version > version && self.migrate.is_some())
    }
}

/// A task kind ready to be registered in a harness catalogue.
#[derive(Clone)]
pub struct Task {
    definition: Arc<TaskDefinition>,
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Task").field(&self.definition.name).finish()
    }
}

impl Task {
    /// The definition this token carries.
    #[must_use]
    pub fn definition(&self) -> &TaskDefinition {
        &self.definition
    }

    /// Erase the definition for the scheduler's catalogue.
    #[must_use]
    pub fn erased(self) -> Arc<dyn ErasedTask> {
        Arc::new(self)
    }

    /// Erase a shared token.
    #[must_use]
    pub fn erased_shared(self: Arc<Self>) -> Arc<dyn ErasedTask> {
        self
    }
}

/// The erased surface the scheduler runs a definition through.
pub trait ErasedTask: Send + Sync {
    /// Registered definition name.
    fn name(&self) -> &str;
    /// Definition version.
    fn version(&self) -> i64;
    /// The complete state a new task starts from.
    fn initial_state(&self, input: &Value) -> Value;
    /// The handler for the phase named by a checkpoint.
    fn phase_for(&self, checkpoint: &Value) -> Option<PhaseHandler>;
    /// The abort handler, if any.
    fn abort_handler(&self) -> Option<PhaseHandler>;
    /// The migration, if any.
    fn migration(&self) -> Option<Migrate>;
    /// Whether this definition can take over a task stored at `version`: the
    /// same version, or a newer one with a migration.
    fn can_reserve_version(&self, version: i64) -> bool {
        self.version() == version || (self.version() > version && self.migration().is_some())
    }
    /// The opaque hooks object.
    fn hooks(&self) -> Option<Arc<dyn Any + Send + Sync>>;
    /// The concrete definition, for extension code that downcasts.
    fn as_definition(&self) -> &TaskDefinition;
}

impl ErasedTask for Task {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn version(&self) -> i64 {
        self.definition.version
    }

    fn initial_state(&self, input: &Value) -> Value {
        (self.definition.initial)(input)
    }

    fn phase_for(&self, checkpoint: &Value) -> Option<PhaseHandler> {
        self.definition.phase_for(checkpoint).cloned()
    }

    fn abort_handler(&self) -> Option<PhaseHandler> {
        self.definition.abort.clone()
    }

    fn migration(&self) -> Option<Migrate> {
        self.definition.migrate
    }

    fn hooks(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.definition.hooks.clone()
    }

    fn as_definition(&self) -> &TaskDefinition {
        &self.definition
    }
}

/// A rejected task definition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskDefinitionError {
    /// The name was empty.
    #[error("Task name must be a non-empty string")]
    EmptyName,
    /// The version was below 1.
    #[error("Task {task} version must be a positive integer")]
    InvalidVersion {
        /// The offending task name.
        task: String,
    },
    /// No phase was declared.
    #[error("Task {task} must declare at least one phase")]
    NoPhases {
        /// The offending task name.
        task: String,
    },
    /// A phase name was declared twice.
    ///
    /// Upstream's `define` takes a plain object, so a repeated key silently
    /// keeps the last handler. The builder is explicit about it instead: a
    /// definition that names one phase twice is ambiguous, and running the
    /// wrong handler is worse than refusing to build.
    #[error("Task {task} declares phase {phase} twice")]
    DuplicatePhase {
        /// The offending task name.
        task: String,
        /// The repeated phase.
        phase: String,
    },
    /// A phase name was empty.
    #[error("Task {task} has an empty phase name")]
    EmptyPhase {
        /// The offending task name.
        task: String,
    },
}

/// Start defining an executable task.
///
/// ```ignore
/// let task = define_task("demo", 1, |_input| json!({"phase": "start"}))
///     .phase("start", phase_handler(run_start))
///     .abort(phase_handler(run_abort))
///     .build()?;
/// ```
pub fn define_task(name: impl Into<String>, version: i64, initial: InitialFn) -> TaskBuilder {
    TaskBuilder {
        name: name.into(),
        version,
        initial,
        phases: BTreeMap::new(),
        duplicated: Vec::new(),
        abort: None,
        migrate: None,
        hooks: None,
    }
}

/// The builder [`define_task`] returns.
pub struct TaskBuilder {
    name: String,
    version: i64,
    initial: InitialFn,
    phases: BTreeMap<String, PhaseHandler>,
    /// Phases named more than once, in declaration order.
    duplicated: Vec<String>,
    abort: Option<PhaseHandler>,
    migrate: Option<Migrate>,
    hooks: Option<Arc<dyn Any + Send + Sync>>,
}

impl TaskBuilder {
    /// Add one phase handler.
    ///
    /// A duplicate name is rejected by [`TaskBuilder::build`] with
    /// [`TaskDefinitionError::DuplicatePhase`]. The first handler is kept so
    /// the builder stays total, but the definition is never usable: silently
    /// letting the second handler replace the first would run a phase the
    /// author did not intend, which is a worse failure than a build error.
    pub fn phase(mut self, name: impl Into<String>, handler: PhaseHandler) -> Self {
        let name = name.into();
        if self.phases.contains_key(&name) {
            self.duplicated.push(name);
            return self;
        }
        self.phases.insert(name, handler);
        self
    }

    /// Add the abort handler.
    pub fn abort(mut self, handler: PhaseHandler) -> Self {
        self.abort = Some(handler);
        self
    }

    /// Add a checkpoint-only migration (the brief's shape).
    pub fn migrate(mut self, migrate: fn(i64, &Value) -> Result<Value, String>) -> Self {
        self.migrate = Some(Migrate::Checkpoint(migrate));
        self
    }

    /// Add a migration that reshapes input and checkpoint (upstream's shape).
    pub fn migrate_with_input(
        mut self,
        migrate: fn(i64, &Value, &Value) -> Result<Migrated, String>,
    ) -> Self {
        self.migrate = Some(Migrate::Full(migrate));
        self
    }

    /// Attach the opaque hooks object.
    pub fn hooks(mut self, hooks: Arc<dyn Any + Send + Sync>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Validate and build the task.
    pub fn build(self) -> Result<Task, TaskDefinitionError> {
        if self.name.is_empty() {
            return Err(TaskDefinitionError::EmptyName);
        }
        if self.version < 1 {
            return Err(TaskDefinitionError::InvalidVersion { task: self.name });
        }
        if let Some(phase) = self.duplicated.into_iter().next() {
            return Err(TaskDefinitionError::DuplicatePhase {
                task: self.name,
                phase,
            });
        }
        if self.phases.is_empty() {
            return Err(TaskDefinitionError::NoPhases { task: self.name });
        }
        if self.phases.keys().any(String::is_empty) {
            return Err(TaskDefinitionError::EmptyPhase { task: self.name });
        }
        Ok(Task {
            definition: Arc::new(TaskDefinition {
                name: self.name,
                version: self.version,
                initial: self.initial,
                phases: self.phases,
                abort: self.abort,
                migrate: self.migrate,
                hooks: self.hooks,
            }),
        })
    }
}
