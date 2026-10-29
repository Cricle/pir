//! The stored `pi.agent` state, its changes, and agent resolution.
//!
//! Ports the agent half of upstream `harness/types.ts` (`ModelRef`,
//! `AgentState`, `AgentChange`, `Agent`) plus `harness/agent.ts`'s
//! `resolveAgent`, `applyChange`, `agentHooks`, `sectionExtension` and
//! `toolExtension`.
//!
//! # Stored state versus resolved agent
//!
//! [`AgentState`] is what the `pi.agent` document holds: *names*, not objects,
//! plus the model and directory. Every field is optional and an unset field
//! follows the host (the registry and the settings). It serializes with the
//! same `camelCase` keys upstream writes.
//!
//! [`Agent`] is the resolution of that state against one registry snapshot and
//! the resolved [`Settings`]: concrete extensions, the offered tools, the
//! nested-callable tools, the deferred tools, and the ordered prompt sections.
//!
//! # `AgentChange` in Rust
//!
//! Upstream's three-way `undefined` / `null` / value per field cannot be
//! written as `Option<Option<T>>` without being unreadable at every call site.
//! [`Change<T>`] names the same three cases: [`Change::Keep`] leaves the stored
//! field alone, [`Change::Clear`] removes it, and [`Change::Set`] replaces it.
//! [`Change::Keep`] is the default, so `AgentChange { cwd: Change::Set(..),
//! ..Default::default() }` changes exactly one field.
//!
//! # Wrapper failures
//!
//! Upstream's wrapper may throw; here it returns `Err(message)`.
//! [`resolve_agent`] drops a failing target silently (the observable result of
//! the throw) and [`resolve_agent_with`] also hands the message and the
//! wrapping extension's name to a reporter, which is the `onReport` join point.

use crate::ai::types::ThinkingLevel;
use crate::durable::harness::registry::{
    Extension, HookRegistration, PromptSection, RegistrySnapshot, ToolCaller, ToolRegistration,
    Wrap,
};
use crate::durable::harness::settings::Settings;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The reserved section key of the agent's `instructions`.
pub const INSTRUCTIONS_KEY: &str = "instructions";

/// Provider and model ID resolved through the model catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    /// Provider ID.
    pub provider: String,
    /// Model ID within that provider.
    pub model_id: String,
}

impl ModelRef {
    /// A provider/model pair.
    pub fn new(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model_id: model_id.into(),
        }
    }

    /// The exact `"provider/modelId"` key the compaction `modelOverrides` map
    /// uses.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }
}

// ─── Stored state ───────────────────────────────────────────────────────────

/// Stored extension selection: exactly these names, or an edit of the host's
/// default selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtensionSelection {
    /// An array selects exactly these extensions, in order.
    List(Vec<String>),
    /// An object edits the host default selection: `add` is appended, then
    /// `remove` is dropped.
    ///
    /// Each member keeps its presence, so a present-but-empty `add` stays
    /// distinguishable from an absent one, matching upstream's
    /// `(add === undefined ? {} : { add })` spread.
    Edit {
        /// Names appended to the default selection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        add: Option<Vec<String>>,
        /// Names dropped from the default selection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remove: Option<Vec<String>>,
    },
}

impl ExtensionSelection {
    /// The selection that keeps the host default unchanged.
    pub fn inherit() -> Self {
        Self::Edit {
            add: None,
            remove: None,
        }
    }

    /// Edit the host selection by appending `add` and dropping `remove`.
    pub fn edit(add: Option<Vec<String>>, remove: Option<Vec<String>>) -> Self {
        Self::Edit { add, remove }
    }
}

/// Stored filter over the selected extensions' tools: the enabled tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolSelection {
    /// An array enables exactly these names, in order.
    List(Vec<String>),
    /// An object drops these names and keeps the rest.
    Edit {
        /// Names to drop.
        remove: Vec<String>,
    },
}

/// Stored filter over the enabled tools the model may call: the offered tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelToolSelection {
    /// An array offers exactly these names, in order, deferred ones too.
    List(Vec<String>),
    /// An object drops `remove` and loads the deferred tools named by `add`.
    Edit {
        /// Names to drop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remove: Option<Vec<String>>,
        /// Deferred tool names to load; non-deferred names have no effect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        add: Option<Vec<String>>,
    },
}

impl ModelToolSelection {
    /// The filter that changes nothing: no removals and no loads.
    pub fn inherit() -> Self {
        Self::Edit {
            remove: None,
            add: None,
        }
    }
}

/// Stored choices of one conversation; names, not objects. Unset fields follow
/// the host.
///
/// This is the value of the conversation document `pi.agent` (`version: 1`,
/// `history: rewindable`, `fork: asOf`), so a fork starts from the agent as of
/// its fork entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    /// Model the conversation uses; unset fails a run with `no_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Thinking level; unset means `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    /// Selected extensions: exactly these, or an edit of the host default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<ExtensionSelection>,
    /// Filters the selected extensions' tools: the enabled tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolSelection>,
    /// Filters the enabled tools the model may call: the offered tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tools: Option<ModelToolSelection>,
    /// Rendered after every extension section, as the section `instructions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Directory within the environment's file system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

// ─── Changes ────────────────────────────────────────────────────────────────

/// One field of an [`AgentChange`]: keep the stored value, clear it, or replace
/// it.
///
/// `Keep` is the default so a change names only the fields it touches.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Change<T> {
    /// `undefined`: leave the stored field alone.
    #[default]
    Keep,
    /// `null`: remove the stored field.
    Clear,
    /// A value: replace the stored field.
    Set(T),
}

impl<T> Change<T> {
    /// Whether this change leaves the stored field alone.
    pub fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    /// Apply the change to a stored optional value.
    pub fn apply(self, stored: Option<T>) -> Option<T> {
        match self {
            Self::Keep => stored,
            Self::Clear => None,
            Self::Set(value) => Some(value),
        }
    }
}

/// An extension selection as an [`AgentChange`] wrote it.
///
/// Upstream passes extension objects; an object stands for its name, so this
/// type takes the objects and reduces them to names when applied.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionEdit {
    /// Select exactly these extensions, in order.
    Select(Vec<Extension>),
    /// Edit the host default selection. Each member keeps its presence.
    Edit {
        /// Extensions appended to the default selection.
        add: Option<Vec<Extension>>,
        /// Extensions dropped from the default selection.
        remove: Option<Vec<Extension>>,
    },
}

impl ExtensionEdit {
    /// An edit that only appends.
    pub fn add(extensions: Vec<Extension>) -> Self {
        Self::Edit {
            add: Some(extensions),
            remove: None,
        }
    }

    /// An edit that only drops.
    pub fn remove(extensions: Vec<Extension>) -> Self {
        Self::Edit {
            add: None,
            remove: Some(extensions),
        }
    }
}

/// A `tools` change: an array enables exactly these, `{ remove }` drops names.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolEdit {
    /// Enable exactly these tools, in order.
    Select(Vec<ToolRegistration>),
    /// Drop these tools and keep the rest.
    Edit {
        /// Tools to drop.
        remove: Vec<ToolRegistration>,
    },
}

/// A `modelTools` change: an array offers exactly these, `{ remove, add }`
/// drops and loads.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelToolsEdit {
    /// Offer exactly these tools, in order, deferred ones too.
    Select(Vec<ToolRegistration>),
    /// Drop `remove` and load the deferred tools in `add`.
    Edit {
        /// Tools to drop.
        remove: Option<Vec<ToolRegistration>>,
        /// Deferred tools to load.
        add: Option<Vec<ToolRegistration>>,
    },
}

/// A change to `pi.agent`: a set field replaces the stored one, a cleared field
/// is removed, and a kept field changes nothing.
///
/// Objects stand for their names: a registration passed here is stored as its
/// name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentChange {
    /// Replace or clear the model.
    pub model: Change<ModelRef>,
    /// Replace or clear the thinking level.
    pub thinking_level: Change<ThinkingLevel>,
    /// Replace or clear the extension selection.
    pub extensions: Change<ExtensionEdit>,
    /// Replace or clear the tools filter.
    pub tools: Change<ToolEdit>,
    /// Replace or clear the model-tools filter. Replaces the stored filter as a
    /// whole; tools loaded through `control.addTools` are in its `add`.
    pub model_tools: Change<ModelToolsEdit>,
    /// Replace or clear the instructions.
    pub instructions: Change<String>,
    /// Replace or clear the working directory.
    pub cwd: Change<String>,
}

impl AgentChange {
    /// A change that touches only the model.
    pub fn model(model: ModelRef) -> Self {
        Self {
            model: Change::Set(model),
            ..Self::default()
        }
    }

    /// A change that touches only the instructions.
    pub fn instructions(instructions: impl Into<String>) -> Self {
        Self {
            instructions: Change::Set(instructions.into()),
            ..Self::default()
        }
    }

    /// A change that touches only the working directory.
    pub fn cwd(cwd: impl Into<String>) -> Self {
        Self {
            cwd: Change::Set(cwd.into()),
            ..Self::default()
        }
    }

    /// Whether this change touches no field, so applying it writes nothing.
    ///
    /// Upstream relies on this: a document write of an equal value records no
    /// change, and a change of only kept fields produces one.
    pub fn is_empty(&self) -> bool {
        self.model.is_keep()
            && self.thinking_level.is_keep()
            && self.extensions.is_keep()
            && self.tools.is_keep()
            && self.model_tools.is_keep()
            && self.instructions.is_keep()
            && self.cwd.is_keep()
    }
}

/// Apply one change to stored state: a set field replaces the stored one, a
/// cleared field is removed, a kept field changes nothing.
///
/// Matches upstream `applyChange`. Registration lists are reduced to names and
/// keep each member's presence, so a present-but-empty `add` stays present.
pub fn configure(state: &AgentState, change: &AgentChange) -> AgentState {
    AgentState {
        model: change.model.clone().apply(state.model.clone()),
        thinking_level: change.thinking_level.clone().apply(state.thinking_level),
        extensions: apply_extensions(&state.extensions, &change.extensions),
        tools: apply_tools(&state.tools, &change.tools),
        model_tools: apply_model_tools(&state.model_tools, &change.model_tools),
        instructions: change
            .instructions
            .clone()
            .apply(state.instructions.clone()),
        cwd: change.cwd.clone().apply(state.cwd.clone()),
    }
}

fn apply_extensions(
    stored: &Option<ExtensionSelection>,
    change: &Change<ExtensionEdit>,
) -> Option<ExtensionSelection> {
    match change {
        Change::Keep => stored.clone(),
        Change::Clear => None,
        Change::Set(ExtensionEdit::Select(extensions)) => {
            Some(ExtensionSelection::List(names_of(extensions)))
        }
        Change::Set(ExtensionEdit::Edit { add, remove }) => Some(ExtensionSelection::Edit {
            add: add.as_ref().map(|extensions| names_of(extensions)),
            remove: remove.as_ref().map(|extensions| names_of(extensions)),
        }),
    }
}

fn apply_tools(stored: &Option<ToolSelection>, change: &Change<ToolEdit>) -> Option<ToolSelection> {
    match change {
        Change::Keep => stored.clone(),
        Change::Clear => None,
        Change::Set(ToolEdit::Select(tools)) => Some(ToolSelection::List(names_of(tools))),
        Change::Set(ToolEdit::Edit { remove }) => Some(ToolSelection::Edit {
            remove: names_of(remove),
        }),
    }
}

fn apply_model_tools(
    stored: &Option<ModelToolSelection>,
    change: &Change<ModelToolsEdit>,
) -> Option<ModelToolSelection> {
    match change {
        Change::Keep => stored.clone(),
        Change::Clear => None,
        Change::Set(ModelToolsEdit::Select(tools)) => {
            Some(ModelToolSelection::List(names_of(tools)))
        }
        Change::Set(ModelToolsEdit::Edit { remove, add }) => Some(ModelToolSelection::Edit {
            remove: remove.as_ref().map(|tools| names_of(tools)),
            add: add.as_ref().map(|tools| names_of(tools)),
        }),
    }
}

/// The names of a list of named registrations, in order.
fn names_of<T: Named>(items: &[T]) -> Vec<String> {
    items.iter().map(Named::name).collect()
}

/// Anything stored in `pi.agent` by name.
trait Named {
    /// The registry name.
    fn name(&self) -> String;
}

impl Named for Extension {
    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Named for ToolRegistration {
    fn name(&self) -> String {
        self.name.clone()
    }
}

// ─── Resolved agent ─────────────────────────────────────────────────────────

/// A conversation's agent resolved against a registry snapshot and the
/// settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    /// Model the conversation uses; absent fails a request with `no_model`.
    pub model: Option<ModelRef>,
    /// Thinking level; `off` when unset.
    pub thinking_level: ThinkingLevel,
    /// The selected installed extensions, in resolution order.
    pub extensions: Vec<Extension>,
    /// The tools a request offers, in order: enabled, callable by the model,
    /// and selected by `modelTools`.
    pub tools: Vec<ToolRegistration>,
    /// The tools nested calls resolve among, in order: enabled and callable by
    /// tools.
    pub callable: Vec<ToolRegistration>,
    /// Enabled deferred tools the model may call and is not offered yet, in
    /// registry order: what a tool-search tool loads.
    pub deferred: Vec<ToolRegistration>,
    /// Extension sections, then `instructions` when set.
    pub sections: Vec<PromptSection>,
    /// The stored instructions, when set.
    pub instructions: Option<String>,
    /// The stored working directory, when set.
    pub cwd: Option<String>,
}

impl Agent {
    /// The section with this key, if the agent renders one.
    pub fn section(&self, key: &str) -> Option<&PromptSection> {
        self.sections.iter().find(|section| section.key == key)
    }

    /// The tool offered to the model with this name, if any.
    pub fn tool(&self, name: &str) -> Option<&ToolRegistration> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

/// Resolve an agent from its stored state, a registry snapshot, and resolved
/// settings.
///
/// A wrapper that fails or renames drops its target; this form reports
/// nothing. Use [`resolve_agent_with`] to receive those failures.
pub fn resolve_agent(
    snapshot: &RegistrySnapshot,
    settings: &Settings,
    state: &AgentState,
) -> Agent {
    let mut no_report = |_: &str, _: &str| {};
    resolve_agent_with(snapshot, settings, state, &mut no_report)
}

/// [`resolve_agent`], reporting every dropped wrapper target.
///
/// `report` receives the wrapper's failure message and the wrapping
/// extension's name, matching upstream's `report(error, extension)`. A wrapper
/// that finds no target does nothing and is not reported.
pub fn resolve_agent_with(
    snapshot: &RegistrySnapshot,
    settings: &Settings,
    state: &AgentState,
    report: &mut dyn FnMut(&str, &str),
) -> Agent {
    let extensions = select_extensions(state.extensions.as_ref(), snapshot, settings);

    // Composed tools and sections: later same-name entries replace earlier ones
    // in place, so the order is the first provider's position.
    let mut composed = Ordered::new();
    for extension in &extensions {
        for tool in &extension.tools {
            composed.upsert(tool.name.clone(), tool.clone());
        }
    }
    let mut sections = Ordered::new();
    for extension in &extensions {
        for section in &extension.sections {
            sections.upsert(section.key.clone(), section.clone());
        }
    }
    for extension in &extensions {
        for wrap in &extension.wraps {
            let mut wrap_report = |message: &str| report(message, &extension.name);
            match wrap {
                Wrap::Tool { tool, wrap } => {
                    let Some(target) = composed.get(tool).cloned() else {
                        continue;
                    };
                    match wrap(&target) {
                        Ok(wrapped) if wrapped.name == *tool => {
                            composed.set(tool, wrapped);
                        }
                        Ok(wrapped) => {
                            composed.remove(tool);
                            wrap_report(&format!("Wrapper renamed {tool} to {}", wrapped.name));
                        }
                        Err(message) => {
                            composed.remove(tool);
                            wrap_report(&message);
                        }
                    }
                }
                Wrap::Section { section, wrap } => {
                    let Some(target) = sections.get(section).cloned() else {
                        continue;
                    };
                    match wrap(&target) {
                        Ok(wrapped) if wrapped.key == *section => {
                            sections.set(section, wrapped);
                        }
                        Ok(wrapped) => {
                            sections.remove(section);
                            wrap_report(&format!("Wrapper renamed {section} to {}", wrapped.key));
                        }
                        Err(message) => {
                            sections.remove(section);
                            wrap_report(&message);
                        }
                    }
                }
            }
        }
    }

    let enabled = filter_tools(composed.into_vec(), state.tools.as_ref());
    let model_callable: Vec<ToolRegistration> = enabled
        .iter()
        .filter(|tool| tool.is_callable_by(ToolCaller::Model))
        .cloned()
        .collect();
    let tools = offered_tools(&model_callable, state.model_tools.as_ref());
    let offered: HashSet<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let deferred = model_callable
        .iter()
        .filter(|tool| tool.deferred && !offered.contains(tool.name.as_str()))
        .cloned()
        .collect();
    let callable = enabled
        .into_iter()
        .filter(|tool| tool.is_callable_by(ToolCaller::Tools))
        .collect();

    let instructions = state.instructions.clone();
    let mut agent_sections = sections.into_vec();
    if let Some(instructions) = &instructions {
        agent_sections.push(PromptSection::text(INSTRUCTIONS_KEY, instructions.clone()));
    }

    Agent {
        model: state.model.clone(),
        thinking_level: state.thinking_level.unwrap_or(ThinkingLevel::Off),
        extensions,
        tools,
        callable,
        deferred,
        sections: agent_sections,
        instructions,
        cwd: state.cwd.clone(),
    }
}

/// Handlers of the selected extensions' hooks for a task name, in extension
/// order (upstream `agentHooks`).
pub fn hooks_for_task<'a>(agent: &'a Agent, task: &str) -> Vec<(&'a str, &'a HookRegistration)> {
    let mut found = Vec::new();
    for extension in &agent.extensions {
        for hook in &extension.hooks {
            if hook.task == task {
                found.push((extension.name.as_str(), hook));
            }
        }
    }
    found
}

/// The extension whose code a section of `agent` runs: the last selected one
/// that provides or wraps it (upstream `sectionExtension`).
pub fn section_extension<'a>(agent: &'a Agent, key: &str) -> Option<&'a str> {
    let mut found = None;
    for extension in &agent.extensions {
        let provides = extension.sections.iter().any(|section| section.key == key);
        let wraps = extension
            .wraps
            .iter()
            .any(|wrap| matches!(wrap, Wrap::Section { section, .. } if section == key));
        if provides || wraps {
            found = Some(extension.name.as_str());
        }
    }
    found
}

/// The extension whose code a tool of `agent` runs: the last selected one that
/// provides or wraps it (upstream `toolExtension`).
pub fn tool_extension<'a>(agent: &'a Agent, name: &str) -> Option<&'a str> {
    let mut found = None;
    for extension in &agent.extensions {
        let provides = extension.tools.iter().any(|tool| tool.name == name);
        let wraps = extension
            .wraps
            .iter()
            .any(|wrap| matches!(wrap, Wrap::Tool { tool, .. } if tool == name));
        if provides || wraps {
            found = Some(extension.name.as_str());
        }
    }
    found
}

/// Selected installed extensions: the stored array, or the default selection
/// edited by `{ add, remove }`.
///
/// Duplicates keep their first position; names not installed are skipped.
fn select_extensions(
    stored: Option<&ExtensionSelection>,
    snapshot: &RegistrySnapshot,
    settings: &Settings,
) -> Vec<Extension> {
    let selected: Vec<String> = match stored {
        Some(ExtensionSelection::List(list)) => list.clone(),
        other => {
            let edit = match other {
                Some(ExtensionSelection::Edit { add, remove }) => (add.clone(), remove.clone()),
                _ => (None, None),
            };
            let base: Vec<String> = match &settings.extensions {
                Some(extensions) => names_of(extensions),
                None => snapshot
                    .installed()
                    .iter()
                    .map(|extension| extension.name.clone())
                    .collect(),
            };
            let removed: HashSet<String> = edit.1.unwrap_or_default().into_iter().collect();
            base.into_iter()
                .chain(edit.0.unwrap_or_default())
                .filter(|name| !removed.contains(name))
                .collect()
        }
    };
    let mut seen = HashSet::new();
    selected
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .filter_map(|name| snapshot.extension(&name).cloned())
        .collect()
}

/// `tools` through a stored filter: an array selects exactly its names, in its
/// order; `{ remove }` drops names (upstream `filterTools`).
fn filter_tools(
    tools: Vec<ToolRegistration>,
    filter: Option<&ToolSelection>,
) -> Vec<ToolRegistration> {
    match filter {
        None => tools,
        Some(ToolSelection::List(list)) => {
            let by_name: HashMap<&str, &ToolRegistration> = tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool))
                .collect();
            let mut seen = HashSet::new();
            list.iter()
                .filter(|name| seen.insert(name.as_str()))
                .filter_map(|name| by_name.get(name.as_str()).map(|tool| (**tool).clone()))
                .collect()
        }
        Some(ToolSelection::Edit { remove }) => {
            let removed: HashSet<&str> = remove.iter().map(String::as_str).collect();
            tools
                .into_iter()
                .filter(|tool| !removed.contains(tool.name.as_str()))
                .collect()
        }
    }
}

/// The tools offered to the model, as `AgentState.modelTools` describes
/// (upstream `offeredTools`).
///
/// Unset or an object: every non-deferred tool except `remove`, in registry
/// order, followed by the deferred tools in `add` that are not removed, in
/// `add` order. An array: exactly its names, in its order, deferred ones too.
fn offered_tools(
    tools: &[ToolRegistration],
    filter: Option<&ModelToolSelection>,
) -> Vec<ToolRegistration> {
    match filter {
        Some(ModelToolSelection::List(list)) => {
            let by_name: HashMap<&str, &ToolRegistration> = tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool))
                .collect();
            let mut seen = HashSet::new();
            list.iter()
                .filter(|name| seen.insert(name.as_str()))
                .filter_map(|name| by_name.get(name.as_str()).map(|tool| (**tool).clone()))
                .collect()
        }
        other => {
            let (removed, added) = match other {
                Some(ModelToolSelection::Edit { remove, add }) => (
                    remove.clone().unwrap_or_default(),
                    add.clone().unwrap_or_default(),
                ),
                _ => (Vec::new(), Vec::new()),
            };
            let removed: HashSet<&str> = removed.iter().map(String::as_str).collect();
            let kept: Vec<ToolRegistration> = tools
                .iter()
                .filter(|tool| !tool.deferred && !removed.contains(tool.name.as_str()))
                .cloned()
                .collect();
            // Loaded deferred tools follow in load order, so loading one appends
            // to the declared tools.
            let by_name: HashMap<&str, &ToolRegistration> = tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool))
                .collect();
            let mut seen = HashSet::new();
            let loaded: Vec<ToolRegistration> = added
                .iter()
                .filter(|name| seen.insert(name.as_str()))
                .filter_map(|name| by_name.get(name.as_str()).map(|tool| (**tool).clone()))
                .filter(|tool| tool.deferred && !removed.contains(tool.name.as_str()))
                .collect();
            kept.into_iter().chain(loaded).collect()
        }
    }
}

/// An insertion-ordered map keyed by name, with in-place replacement.
///
/// Upstream composes tools and sections with an insertion-ordered `Map`, whose
/// `set` keeps an existing key's position. `BTreeMap`/`HashMap` would reorder
/// them, and section order is prompt order.
struct Ordered<T> {
    items: Vec<T>,
    index: HashMap<String, usize>,
}

impl<T> Ordered<T> {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Insert under `key`, replacing an existing entry in place.
    fn upsert(&mut self, key: String, value: T) {
        match self.index.get(&key) {
            Some(&at) => self.items[at] = value,
            None => {
                self.index.insert(key, self.items.len());
                self.items.push(value);
            }
        }
    }

    fn get(&self, key: &str) -> Option<&T> {
        self.index.get(key).map(|&at| &self.items[at])
    }

    /// Replace the entry under `key`; a missing key is left alone.
    fn set(&mut self, key: &str, value: T) {
        if let Some(&at) = self.index.get(key) {
            self.items[at] = value;
        }
    }

    /// Remove the entry under `key`, shifting later positions down.
    fn remove(&mut self, key: &str) {
        let Some(at) = self.index.remove(key) else {
            return;
        };
        self.items.remove(at);
        for position in self.index.values_mut() {
            if *position > at {
                *position -= 1;
            }
        }
    }

    fn into_vec(self) -> Vec<T> {
        self.items
    }
}
