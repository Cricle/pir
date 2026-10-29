//! The application-owned extension registry.
//!
//! Ports upstream `harness/registry.ts` and the registry half of
//! `harness/types.ts` / `harness/define.ts`: [`Extension`],
//! [`RegistrySnapshot`], [`RegistryReader`], [`Registry`], the built-in task
//! definitions, and the `install` validation rules.
//!
//! # Engine-free by design
//!
//! This layer deliberately knows nothing about task execution, tool calls,
//! prompts or models. Everything an extension *does* is carried as an opaque
//! handle or a callback the later lanes join onto:
//!
//! | member | form here | join point |
//! |---|---|---|
//! | tool implementation | [`BoxedTool`] = `Arc<dyn Any + Send + Sync>` | the tool-execution lane downcasts it; the registry never calls it |
//! | section rendering | [`PromptSection::render`] | a request-preparation lane supplies [`PromptInput`] |
//! | hooks | [`HookRegistration::handlers`] = opaque [`BoxedHook`] values | the hooks lane dispatches them by task name |
//! | wrappers | [`Wrap`] callbacks | resolution ([`crate::durable::harness::agent`]) applies them |
//!
//! A section renderer is synchronous here. Upstream's `render` may return a
//! promise; the async form is the generation lane's business, and a renderer
//! that must await can capture a runtime handle and block there. The section
//! [`PromptInput`] is likewise the reduced, engine-free part of upstream's
//! input (conversation, shown sections); the agent, environment and document
//! reader join through the caller that renders.
//!
//! # Publication
//!
//! A snapshot is an immutable value and cheap to clone. `install` and
//! `uninstall` build the next snapshot, validate it whole, publish it
//! synchronously and then notify subscribers. A failed validation publishes
//! nothing.
//!
//! Two Rust-specific differences from upstream:
//!
//! * A wrapper reports a failure by returning `Err(String)` instead of
//!   throwing; resolution drops the target and reports the message, which is
//!   the same observable result without panics.
//! * A [`Subscription`] unsubscribes when dropped, so a caller can never leak
//!   a listener by forgetting the explicit [`Subscription::unsubscribe`] call
//!   (which stays available). Upstream's disposer only acts when called.

use crate::durable::harness::settings::ToolExecutionMode;
use crate::durable::ids::ConversationId;
use crate::durable::tasks::Task;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::any::Any;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// The reserved section key of the agent's `instructions`.
pub const INSTRUCTIONS_KEY: &str = "instructions";

/// An opaque tool implementation.
///
/// The registry never inspects or calls it. The tool-execution lane downcasts
/// to its own concrete type; until then a registration may leave it `None`,
/// which resolution treats as "registered but not executable here".
pub type BoxedTool = Arc<dyn Any + Send + Sync>;

/// An opaque hook handler.
///
/// Hook dispatch belongs to the hooks lane, which owns the concrete handler
/// types and matches them by the task name on [`HookRegistration`].
pub type BoxedHook = Arc<dyn Any + Send + Sync>;

/// Who may call a tool: the model, other tools through nested calls, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCaller {
    /// A model-issued call.
    Model,
    /// A nested call made by another tool.
    Tools,
}

/// Whether an interrupted execution may rerun on recovery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayPolicy {
    /// Rerun after a crash.
    Safe,
    /// Never rerun; an unfinished call is abandoned on recovery.
    #[default]
    Unsafe,
}

/// An executable tool registered in a registry.
///
/// Only the declarative fields live here: name, description, JSON-Schema
/// parameters, callers, deferral, replay policy and the per-tool execution
/// override. `implementation` is the opaque join point (see [`BoxedTool`]).
#[derive(Clone, Default)]
pub struct ToolRegistration {
    /// Unique within one extension; across extensions a later one replaces an
    /// earlier one for conversations selecting both.
    pub name: String,
    /// Free-form description shown to the model.
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
    /// Who may call the tool; `None` means both, matching upstream's default.
    pub callers: Option<Vec<ToolCaller>>,
    /// Not offered to the model until `modelTools` loads it.
    pub deferred: bool,
    /// Whether an interrupted execution may rerun.
    pub replay: ReplayPolicy,
    /// Overrides the settings' `toolExecution` for this tool's round.
    pub execution_mode: Option<ToolExecutionMode>,
    /// The opaque implementation, if this registry entry has one.
    pub implementation: Option<BoxedTool>,
}

impl std::fmt::Debug for ToolRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistration")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("callers", &self.callers)
            .field("deferred", &self.deferred)
            .field("replay", &self.replay)
            .field("execution_mode", &self.execution_mode)
            .field("has_implementation", &self.implementation.is_some())
            .finish()
    }
}

impl PartialEq for ToolRegistration {
    /// Compares the declaration. The opaque implementation handle is identity
    /// only and is not compared.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.parameters == other.parameters
            && self.callers == other.callers
            && self.deferred == other.deferred
            && self.replay == other.replay
            && self.execution_mode == other.execution_mode
    }
}

impl ToolRegistration {
    /// A tool with `name` and otherwise empty declaration.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            parameters: Value::Object(Map::new()),
            ..Self::default()
        }
    }

    /// Set the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Set the JSON-Schema parameters.
    pub fn with_parameters(mut self, parameters: Value) -> Self {
        self.parameters = parameters;
        self
    }

    /// Restrict who may call the tool.
    pub fn with_callers(mut self, callers: Vec<ToolCaller>) -> Self {
        self.callers = Some(callers);
        self
    }

    /// Mark the tool deferred: the model is not offered it until `modelTools`
    /// names it.
    pub fn deferred(mut self) -> Self {
        self.deferred = true;
        self
    }

    /// Mark the tool replay-safe.
    pub fn replay_safe(mut self) -> Self {
        self.replay = ReplayPolicy::Safe;
        self
    }

    /// Override the settings' tool execution mode for this tool's round.
    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    /// Attach the opaque implementation handle.
    pub fn with_implementation(mut self, implementation: BoxedTool) -> Self {
        self.implementation = Some(implementation);
        self
    }

    /// Whether `caller` may invoke the tool; an unset `callers` allows both.
    pub fn is_callable_by(&self, caller: ToolCaller) -> bool {
        match &self.callers {
            Some(callers) => callers.contains(&caller),
            None => true,
        }
    }
}

/// What a section renderer may read in this engine-free layer.
///
/// Upstream's `PromptInput` also carries the resolved agent, the environment
/// and a document reader; those belong to the lanes that own those types and
/// join through the renderer's captured state.
#[derive(Clone, Debug, Default)]
pub struct PromptInput {
    /// The conversation whose request renders the section.
    pub conversation_id: ConversationId,
    /// Sections already in effect after replaying the active transcript.
    pub shown: BTreeMap<String, String>,
}

/// A prompt section renderer.
///
/// Takes the input and returns the section text, or `None` to omit the
/// section (upstream `string | undefined`).
pub type SectionRenderer = Arc<dyn Fn(&PromptInput) -> Option<String> + Send + Sync>;

/// One system prompt section; the agent's sections render in order before each
/// request.
#[derive(Clone)]
pub struct PromptSection {
    /// Unique within one extension; across extensions a later one replaces an
    /// earlier one in place.
    pub key: String,
    /// Produces the section text; `None` omits it.
    pub render: SectionRenderer,
    /// Wrap the text as `<key>\n...\n</key>`; default `true`.
    pub tag: bool,
}

impl std::fmt::Debug for PromptSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptSection")
            .field("key", &self.key)
            .field("tag", &self.tag)
            .finish()
    }
}

impl PartialEq for PromptSection {
    /// Compares the declaration, not the renderer callback.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.tag == other.tag
    }
}

impl PromptSection {
    /// A tagged section (the default): rendered text is wrapped in its key.
    pub fn new(key: impl Into<String>, render: SectionRenderer) -> Self {
        Self {
            key: key.into(),
            render,
            tag: true,
        }
    }

    /// A section that renders constant text.
    pub fn text(key: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self::new(key, Arc::new(move |_| Some(text.clone())))
    }

    /// Make the section untagged, so its text is used as-is.
    pub fn untagged(mut self) -> Self {
        self.tag = false;
        self
    }

    /// Render the section, or `None` when it is omitted.
    pub fn render(&self, input: &PromptInput) -> Option<String> {
        (self.render)(input)
    }
}

/// Built by [`hook`](HookRegistration::new); matches tasks by name.
#[derive(Clone, Default)]
pub struct HookRegistration {
    /// The task name whose hooks these are.
    pub task: String,
    /// Hook name to opaque handler, in invocation order.
    pub handlers: BTreeMap<String, BoxedHook>,
}

impl std::fmt::Debug for HookRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistration")
            .field("task", &self.task)
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PartialEq for HookRegistration {
    /// Compares the task and the handler names; handlers are opaque code.
    fn eq(&self, other: &Self) -> bool {
        self.task == other.task && self.handlers.keys().eq(other.handlers.keys())
    }
}

impl HookRegistration {
    /// Hooks for the task named `task`.
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            handlers: BTreeMap::new(),
        }
    }

    /// Add one opaque handler.
    pub fn handler(mut self, name: impl Into<String>, handler: BoxedHook) -> Self {
        self.handlers.insert(name.into(), handler);
        self
    }
}

/// A tool wrapper: returns a replacement tool or a failure message.
///
/// Upstream wrappers throw; here a failure is an `Err` carrying the message
/// resolution reports. A wrapper must be pure and must not rename its target.
pub type ToolWrapper =
    Arc<dyn Fn(&ToolRegistration) -> Result<ToolRegistration, String> + Send + Sync>;

/// A section wrapper: returns a replacement section or a failure message.
pub type SectionWrapper =
    Arc<dyn Fn(&PromptSection) -> Result<PromptSection, String> + Send + Sync>;

/// Targets a tool name or a section key; wrappers are pure.
#[derive(Clone)]
pub enum Wrap {
    /// Wrap the tool with this name wherever the wrapping extension is
    /// selected.
    Tool {
        /// The target tool name.
        tool: String,
        /// The wrapper.
        wrap: ToolWrapper,
    },
    /// Wrap the section with this key wherever the wrapping extension is
    /// selected.
    Section {
        /// The target section key.
        section: String,
        /// The wrapper.
        wrap: SectionWrapper,
    },
}

impl std::fmt::Debug for Wrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool { tool, .. } => f.debug_struct("Wrap::Tool").field("tool", tool).finish(),
            Self::Section { section, .. } => f
                .debug_struct("Wrap::Section")
                .field("section", section)
                .finish(),
        }
    }
}

impl PartialEq for Wrap {
    /// Compares the target; the wrapper callback is opaque code.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Tool { tool: left, .. }, Self::Tool { tool: right, .. }) => left == right,
            (Self::Section { section: left, .. }, Self::Section { section: right, .. }) => {
                left == right
            }
            _ => false,
        }
    }
}

impl Wrap {
    /// Wrap a tool by name.
    pub fn tool(tool: impl Into<String>, wrap: ToolWrapper) -> Self {
        Self::Tool {
            tool: tool.into(),
            wrap,
        }
    }

    /// Wrap a section by key.
    pub fn section(section: impl Into<String>, wrap: SectionWrapper) -> Self {
        Self::Section {
            section: section.into(),
            wrap,
        }
    }
}

/// Named bundle of code; installed in a registry and selected by conversations
/// by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extension {
    /// Registry identity. Installing another extension with the same name
    /// replaces this one in place.
    pub name: String,
    /// Tools this extension provides.
    pub tools: Vec<ToolRegistration>,
    /// Prompt sections this extension provides.
    pub sections: Vec<PromptSection>,
    /// Hooks for tasks, matched by task name.
    pub hooks: Vec<HookRegistration>,
    /// Wrappers applied where this extension is selected, in order.
    pub wraps: Vec<Wrap>,
    /// Task definitions resolved by name for every task, whichever
    /// conversations select this extension.
    pub tasks: Vec<Task>,
}

impl Extension {
    /// An extension with `name` and nothing else.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Add tools.
    pub fn with_tools(mut self, tools: Vec<ToolRegistration>) -> Self {
        self.tools = tools;
        self
    }

    /// Add sections.
    pub fn with_sections(mut self, sections: Vec<PromptSection>) -> Self {
        self.sections = sections;
        self
    }

    /// Add hooks.
    pub fn with_hooks(mut self, hooks: Vec<HookRegistration>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Add wrappers.
    pub fn with_wraps(mut self, wraps: Vec<Wrap>) -> Self {
        self.wraps = wraps;
        self
    }

    /// Add task definitions.
    pub fn with_tasks(mut self, tasks: Vec<Task>) -> Self {
        self.tasks = tasks;
        self
    }
}

/// A rejected registry publication.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// Two tools in one extension share a name.
    #[error("Extension {extension} has two tools named {tool}")]
    DuplicateTool {
        /// Owning extension.
        extension: String,
        /// The repeated tool name.
        tool: String,
    },
    /// A section key does not match `^[a-z][a-z0-9_-]*$`.
    #[error("Section key {key:?} must match ^[a-z][a-z0-9_-]*$")]
    InvalidSectionKey {
        /// The offending key.
        key: String,
    },
    /// A section claimed the reserved key.
    #[error("Section key {key} is reserved for the agent's instructions")]
    ReservedSectionKey {
        /// The reserved key.
        key: String,
    },
    /// Two sections in one extension share a key.
    #[error("Extension {extension} has two sections with key {key}")]
    DuplicateSection {
        /// Owning extension.
        extension: String,
        /// The repeated key.
        key: String,
    },
    /// A task name is already installed (as a built-in or by another
    /// extension).
    #[error("Task {task} of extension {extension} is already installed")]
    TaskAlreadyInstalled {
        /// The clashing task name.
        task: String,
        /// The extension trying to install it.
        extension: String,
    },
}

/// The built-in task definitions every registry holds.
///
/// They are not an extension: nothing uninstalls or replaces them, and any
/// extension may register hooks against them. Names, versions and phases match
/// upstream `GenerationTask`, `ToolTask` and `CompactionTask`.
pub fn builtin_tasks() -> Vec<Task> {
    vec![
        builtin_task(
            "pi.generation",
            1,
            &["prepare", "request", "retry", "poll", "tools"],
        ),
        builtin_task("pi.tool", 2, &["call", "execute"]),
        builtin_task("pi.compaction", 1, &["select", "summarize", "retry"]),
    ]
}

fn builtin_task(kind: &str, version: i64, phases: &[&str]) -> Task {
    let definition = crate::durable::tasks::TaskDefinition::define(
        kind,
        version,
        phases.iter().map(|phase| (*phase).to_owned()).collect(),
    )
    .expect("built-in task definitions are valid");
    crate::durable::tasks::define_task(definition)
}

/// Immutable view of one published registry state.
///
/// Cloning is cheap (the extension and task lists are shared), so a phase
/// handler can take one snapshot and pass it on.
#[derive(Clone)]
pub struct RegistrySnapshot {
    extensions: Arc<Vec<Extension>>,
    tasks: Arc<Vec<Task>>,
}

impl std::fmt::Debug for RegistrySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrySnapshot")
            .field(
                "extensions",
                &self
                    .extensions
                    .iter()
                    .map(|extension| extension.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "tasks",
                &self
                    .tasks
                    .iter()
                    .map(|task| task.definition.kind.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl RegistrySnapshot {
    /// The empty registry: no extensions, only the built-in tasks.
    pub fn empty() -> Self {
        Self::build(Vec::new()).expect("the empty registry is valid")
    }

    /// Validate `extensions` as a whole and build a snapshot from them.
    ///
    /// Checks what upstream `install()` checks: unique tool names and section
    /// keys within an extension, valid and unreserved section keys, and task
    /// names unique across the built-ins and every extension.
    pub fn build(extensions: Vec<Extension>) -> Result<Self, RegistryError> {
        let mut tasks = builtin_tasks();
        for extension in &extensions {
            validate_extension(extension)?;
            for task in &extension.tasks {
                if tasks
                    .iter()
                    .any(|held| held.definition.kind == task.definition.kind)
                {
                    return Err(RegistryError::TaskAlreadyInstalled {
                        task: task.definition.kind.clone(),
                        extension: extension.name.clone(),
                    });
                }
                tasks.push(task.clone());
            }
        }
        Ok(Self {
            extensions: Arc::new(extensions),
            tasks: Arc::new(tasks),
        })
    }

    /// Every installed extension, in install order.
    pub fn installed(&self) -> &[Extension] {
        &self.extensions
    }

    /// The installed extension with this name.
    pub fn extension(&self, name: &str) -> Option<&Extension> {
        self.extensions
            .iter()
            .find(|extension| extension.name == name)
    }

    /// Every installed tool with its extension, in install order.
    ///
    /// Names may repeat across extensions; resolution decides which one wins.
    pub fn tools(&self) -> Vec<(&Extension, &ToolRegistration)> {
        self.extensions
            .iter()
            .flat_map(|extension| extension.tools.iter().map(move |tool| (extension, tool)))
            .collect()
    }

    /// Every installed section with its extension, in install order.
    pub fn sections(&self) -> Vec<(&Extension, &PromptSection)> {
        self.extensions
            .iter()
            .flat_map(|extension| {
                extension
                    .sections
                    .iter()
                    .map(move |section| (extension, section))
            })
            .collect()
    }

    /// The built-in and installed task definitions.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// The built-in or installed task definition with this name.
    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.definition.kind == name)
    }
}

/// Nine characters or fewer, starting with a lowercase letter, then lowercase
/// letters, digits, `_` or `-`. Upstream's `SECTION_KEY`.
fn is_section_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// Unique tool names and section keys within one extension; valid, unreserved
/// section keys.
fn validate_extension(extension: &Extension) -> Result<(), RegistryError> {
    let mut tools = HashSet::new();
    for tool in &extension.tools {
        if !tools.insert(tool.name.as_str()) {
            return Err(RegistryError::DuplicateTool {
                extension: extension.name.clone(),
                tool: tool.name.clone(),
            });
        }
    }
    let mut sections = HashSet::new();
    for section in &extension.sections {
        if !is_section_key(&section.key) {
            return Err(RegistryError::InvalidSectionKey {
                key: section.key.clone(),
            });
        }
        if section.key == INSTRUCTIONS_KEY {
            return Err(RegistryError::ReservedSectionKey {
                key: section.key.clone(),
            });
        }
        if !sections.insert(section.key.as_str()) {
            return Err(RegistryError::DuplicateSection {
                extension: extension.name.clone(),
                key: section.key.clone(),
            });
        }
    }
    Ok(())
}

/// A registry change listener.
pub type RegistryListener = Arc<dyn Fn() + Send + Sync>;

/// A live subscription; unsubscribes when dropped or when
/// [`unsubscribe`](Subscription::unsubscribe) is called.
pub struct Subscription {
    unsubscribe: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Subscription")
    }
}

impl Subscription {
    /// Stop listening now. Idempotent across calls because the token is
    /// consumed.
    pub fn unsubscribe(mut self) {
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }

    /// Whether dropping this value will still unsubscribe.
    pub fn is_active(&self) -> bool {
        self.unsubscribe.is_some()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

/// Read side of a registry consumed by a Harness.
pub trait RegistryReader: Send + Sync {
    /// Immutable view of the whole current registry.
    fn snapshot(&self) -> RegistrySnapshot;

    /// Called synchronously after every publication; wakes the scheduler to
    /// reconsider blocked tasks.
    fn subscribe(&self, listener: RegistryListener) -> Subscription;
}

/// Application-owned registry of extensions.
///
/// Safe to share across threads and long-lived: it may outlive a Harness, and
/// it is never persisted.
pub struct Registry {
    current: RwLock<RegistrySnapshot>,
    listeners: Arc<Mutex<BTreeMap<u64, RegistryListener>>>,
    next_listener: AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.snapshot();
        f.debug_struct("Registry")
            .field(
                "extensions",
                &snapshot
                    .installed()
                    .iter()
                    .map(|extension| extension.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Registry {
    /// A registry holding only the built-in tasks.
    pub fn new() -> Self {
        Self {
            current: RwLock::new(RegistrySnapshot::empty()),
            listeners: Arc::new(Mutex::new(BTreeMap::new())),
            next_listener: AtomicU64::new(1),
        }
    }

    /// Install `extension`, or replace the installed extension with its name in
    /// place. Publishes at once.
    ///
    /// Returns the validation error and publishes nothing when the resulting
    /// registry would be invalid.
    pub fn install(&self, extension: Extension) -> Result<(), RegistryError> {
        let current = self.snapshot();
        let installed = current.installed();
        let next = match installed
            .iter()
            .position(|existing| existing.name == extension.name)
        {
            Some(index) => installed
                .iter()
                .enumerate()
                .map(|(at, existing)| {
                    if at == index {
                        extension.clone()
                    } else {
                        existing.clone()
                    }
                })
                .collect(),
            None => {
                let mut next = installed.to_vec();
                next.push(extension);
                next
            }
        };
        self.publish(next)
    }

    /// Remove the installed extension with `extension.name`, whichever object
    /// it is. A later install appends. Does nothing when absent.
    pub fn uninstall(&self, extension: &Extension) {
        self.uninstall_by_name(&extension.name);
    }

    /// Remove the installed extension with this name. A later install appends.
    pub fn uninstall_by_name(&self, name: &str) {
        let current = self.snapshot();
        let installed = current.installed();
        if !installed.iter().any(|extension| extension.name == name) {
            return;
        }
        let next = installed
            .iter()
            .filter(|extension| extension.name != name)
            .cloned()
            .collect();
        // Uninstall cannot introduce a collision: the remaining extensions were
        // already valid together.
        if let Ok(snapshot) = RegistrySnapshot::build(next) {
            self.store(snapshot);
            self.notify();
        }
    }

    /// Build and validate the next state, which fails on a task name
    /// collision, then publish it synchronously.
    fn publish(&self, extensions: Vec<Extension>) -> Result<(), RegistryError> {
        let snapshot = RegistrySnapshot::build(extensions)?;
        self.store(snapshot);
        self.notify();
        Ok(())
    }

    fn store(&self, snapshot: RegistrySnapshot) {
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
    }

    fn notify(&self) {
        let listeners: Vec<RegistryListener> = self
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for listener in listeners {
            listener();
        }
    }
}

impl RegistryReader for Registry {
    fn snapshot(&self) -> RegistrySnapshot {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn subscribe(&self, listener: RegistryListener) -> Subscription {
        let id = self.next_listener.fetch_add(1, Ordering::SeqCst);
        self.listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, listener);
        let listeners = Arc::clone(&self.listeners);
        Subscription {
            unsubscribe: Some(Box::new(move || {
                listeners
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
            })),
        }
    }
}

/// Create an application-owned registry holding only the built-in tasks.
pub fn create_registry() -> Registry {
    Registry::new()
}
