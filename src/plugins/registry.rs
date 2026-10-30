//! The ONE shared plugin registry.
//!
//! Both Rust builtin plugins and QuickJS JS extensions register here. The
//! registry applies the upstream conflict semantics
//! (`packages/coding-agent/src/core/resource-loader.ts` +
//! `core/extensions/runner.ts`):
//!
//! - `omitReplacedExtensions`: a `replaceable` plugin is dropped when a
//!   non-replaceable plugin registers the same tool/command/flag name; a
//!   builtin victim produces the upstream warning message.
//! - Tools/flags: first registration per name wins (upstream
//!   `getAllRegisteredTools`/`getFlags` iteration order).
//! - Commands: duplicate names get `name:2`, `name:3`, ... invocation names
//!   (upstream `resolveRegisteredCommands`).
//! - Reserved builtin keybindings block extension shortcuts;
//!   other conflicts warn and the last registration wins (upstream
//!   `getShortcuts`).
//! - Tool/flag name clashes across plugins are reported as conflict
//!   diagnostics; precedence stays with load order (all plugins stay
//!   loaded).
//!
//! Renderer/markdown-transformer registrations are NOT part of this
//! registry: the JS runtime exports them through its own registry snapshot
//! (`ExtensionHandle::renderer_defs`) and the TUI consumes them there.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use super::plugin::{CommandBackend, PluginRegistrations, ShortcutBackend, ToolBackend};
use super::source::PluginInfo;
use crate::extensions::{CommandDef, ShortcutDef, ToolDef};

/// Severity of a load/conflict diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Error,
    Warning,
}

/// One load/conflict diagnostic (upstream `ResourceDiagnostic`).
#[derive(Debug, Clone)]
pub struct PluginDiagnostic {
    pub kind: DiagnosticKind,
    pub path: String,
    pub message: String,
}

/// A plugin in the unified list with its captured registrations.
pub struct RegisteredPlugin {
    pub info: PluginInfo,
    pub registrations: PluginRegistrations,
}

/// Keybinding ids whose keybindings extension shortcuts may never override
/// (upstream `RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`).
pub const RESERVED_KEYBINDING_IDS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// A slash command with its resolved invocation name and backend.
#[derive(Clone)]
pub struct ResolvedCommand {
    pub def: CommandDef,
    /// `name`, or `name:2`/`name:3`... when duplicated (upstream
    /// `invocationName`).
    pub invocation_name: String,
    pub backend: Option<CommandBackend>,
}

/// A keyboard shortcut with its backend.
#[derive(Clone)]
pub struct ResolvedShortcut {
    pub def: ShortcutDef,
    pub backend: Option<ShortcutBackend>,
}

/// Reserved-keybinding map handed to [`PluginRegistry::resolve_shortcuts`]:
/// normalized (lowercase) key combo → the owning builtin keybinding id with
/// its `restrictOverride` flag (upstream `buildBuiltinKeybindings`).
pub type ReservedKeys = BTreeMap<String, (String, bool)>;

/// The unified registry over the ordered plugin list.
pub struct PluginRegistry {
    plugins: Vec<RegisteredPlugin>,
    diagnostics: Vec<PluginDiagnostic>,
    /// First-wins tool resolution, computed on first use and retained for the
    /// life of this immutable registry snapshot. Lazy so a registry rebuild
    /// (which also runs at startup and on every JS/MCP definition change)
    /// does not pay for a resolution no consumer may request; once requested,
    /// [`Self::resolved_tools`] / [`Self::tool_backend`] are allocation-light
    /// instead of rebuilding a name set / scanning every registration.
    tools: OnceLock<ToolResolution>,
    /// Command resolution (`name:2` invocation dedup), likewise computed on
    /// first use. Each entry points at its registration and carries the
    /// derived invocation name; [`Self::resolved_commands`] materializes the
    /// owned [`ResolvedCommand`]s with the same clones the old algorithm
    /// performed, without re-running the `BTreeMap`/`format!` pass per call
    /// (the TUI reads this on every command-list refresh).
    resolved_commands: OnceLock<Vec<ResolvedCommandEntry>>,
}

/// First-wins tool positions in dispatch order, plus the name → position map
/// the hot backend lookup uses.
struct ToolResolution {
    resolved: Vec<(usize, usize)>,
    by_name: HashMap<String, (usize, usize)>,
}

/// One resolved command registration: the source position plus the derived
/// invocation name. Kept as indices so the registry does not duplicate every
/// [`CommandDef`]/backend pair in long-lived memory.
struct ResolvedCommandEntry {
    plugin: usize,
    command: usize,
    invocation_name: String,
}

impl PluginRegistry {
    /// Build the registry from candidates in dispatch order, applying the
    /// upstream omission + conflict rules. `initial` diagnostics (load
    /// errors such as unknown `builtin:` names) are carried through.
    pub fn build(candidates: Vec<RegisteredPlugin>, initial: Vec<PluginDiagnostic>) -> Self {
        let mut diagnostics: Vec<PluginDiagnostic> = initial;
        let plugins = omit_replaced(candidates, &mut diagnostics);
        detect_conflicts(&plugins, &mut diagnostics);
        Self {
            plugins,
            diagnostics,
            tools: OnceLock::new(),
            resolved_commands: OnceLock::new(),
        }
    }

    /// The ordered plugin list after omission (dispatch order).
    pub fn plugins(&self) -> &[RegisteredPlugin] {
        &self.plugins
    }

    /// Omission + conflict diagnostics.
    pub fn diagnostics(&self) -> &[PluginDiagnostic] {
        &self.diagnostics
    }

    /// First-wins tools across the ordered plugin list.
    pub fn resolved_tools(&self) -> Vec<&(ToolDef, ToolBackend)> {
        // The first-wins resolution is computed once per registry snapshot
        // (first call) with borrowed keys; this only materializes the
        // reference list the signature promises.
        self.tool_resolution()
            .resolved
            .iter()
            .map(|(plugin, tool)| &self.plugins[*plugin].registrations.tools[*tool])
            .collect()
    }

    /// The first-registered backend for `name`, or `None`.
    ///
    /// The hot lookup behind [`crate::plugins::PluginHost::call_tool_with_context`]:
    /// exactly the `resolved_tools().into_iter().find(...)` answer (first
    /// registration wins), served from the name index with no scan, `Vec`, or
    /// `ToolDef` clone.
    pub fn tool_backend(&self, name: &str) -> Option<&ToolBackend> {
        let (plugin, tool) = *self.tool_resolution().by_name.get(name)?;
        Some(&self.plugins[plugin].registrations.tools[tool].1)
    }

    fn tool_resolution(&self) -> &ToolResolution {
        self.tools.get_or_init(|| resolve_tool_index(&self.plugins))
    }

    fn command_entries(&self) -> &[ResolvedCommandEntry] {
        self.resolved_commands
            .get_or_init(|| resolve_command_entries(&self.plugins))
    }

    /// Commands with `name:2`-style invocation dedup (upstream
    /// `resolveRegisteredCommands`).
    ///
    /// The occurrence dedup is computed once per registry snapshot; the
    /// returned records own the same [`CommandDef`]/backend clones the
    /// previous on-demand resolution produced.
    pub fn resolved_commands(&self) -> Vec<ResolvedCommand> {
        self.command_entries()
            .iter()
            .map(|entry| {
                let (def, backend) =
                    &self.plugins[entry.plugin].registrations.commands[entry.command];
                ResolvedCommand {
                    def: def.clone(),
                    invocation_name: entry.invocation_name.clone(),
                    backend: backend.clone(),
                }
            })
            .collect()
    }

    /// The first resolved command matching `name` (invocation or declared),
    /// cloning only that one record. This is the per-dispatch lookup; the
    /// full [`Self::resolved_commands`] list remains the enumeration API.
    pub fn find_command(&self, name: &str) -> Option<ResolvedCommand> {
        let entry = self.command_entries().iter().find(|entry| {
            let (def, _) = &self.plugins[entry.plugin].registrations.commands[entry.command];
            entry.invocation_name == name || def.name == name
        })?;
        let (def, backend) = &self.plugins[entry.plugin].registrations.commands[entry.command];
        Some(ResolvedCommand {
            def: def.clone(),
            invocation_name: entry.invocation_name.clone(),
            backend: backend.clone(),
        })
    }

    /// The shortcut whose normalized key matches `key`, last registration
    /// wins (upstream `getShortcuts` with no reserved-key overrides — the
    /// [`crate::plugins::PluginHost::invoke_shortcut`] call). Borrowed so a
    /// keypress does not clone the whole shortcut set.
    pub fn shortcut(&self, key: &str) -> Option<(&ShortcutDef, Option<&ShortcutBackend>)> {
        let normalized = key.to_lowercase();
        let mut found = None;
        for plugin in &self.plugins {
            for (def, backend) in &plugin.registrations.shortcuts {
                if def.key.to_lowercase() == normalized {
                    found = Some((def, backend.as_ref()));
                }
            }
        }
        found
    }

    /// Shortcut conflict resolution (upstream `getShortcuts`): reserved
    /// builtin keybindings block extension shortcuts; other conflicts warn
    /// and last-wins. Returns the resolved shortcuts plus per-conflict
    /// warnings.
    pub fn resolve_shortcuts(
        &self,
        reserved: &ReservedKeys,
    ) -> (Vec<ResolvedShortcut>, Vec<PluginDiagnostic>) {
        let mut diagnostics: Vec<PluginDiagnostic> = Vec::new();
        let mut resolved: std::collections::BTreeMap<String, ResolvedShortcut> =
            std::collections::BTreeMap::new();
        let mut owners: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();

        for plugin in &self.plugins {
            for (def, backend) in &plugin.registrations.shortcuts {
                let normalized = def.key.to_lowercase();
                match reserved.get(&normalized) {
                    Some((_keybinding, true)) => {
                        diagnostics.push(PluginDiagnostic {
                            kind: DiagnosticKind::Warning,
                            path: plugin.info.path.clone(),
                            message: format!(
                                "Extension shortcut '{}' from {} conflicts with built-in shortcut. Skipping.",
                                def.key, plugin.info.path
                            ),
                        });
                        continue;
                    }
                    Some((keybinding, false)) => {
                        diagnostics.push(PluginDiagnostic {
                            kind: DiagnosticKind::Warning,
                            path: plugin.info.path.clone(),
                            message: format!(
                                "Extension shortcut conflict: '{}' is built-in shortcut for {} and {}. Using {}.",
                                def.key, keybinding, plugin.info.path, plugin.info.path
                            ),
                        });
                    }
                    None => {}
                }
                if owners.contains_key(&normalized) {
                    diagnostics.push(PluginDiagnostic {
                        kind: DiagnosticKind::Warning,
                        path: plugin.info.path.clone(),
                        message: format!(
                            "Extension shortcut conflict: '{}' registered by both {} and {}. Using {}.",
                            def.key,
                            owners.get(&normalized).cloned().unwrap_or_default(),
                            plugin.info.path,
                            plugin.info.path
                        ),
                    });
                }
                owners.insert(normalized.clone(), plugin.info.path.clone());
                resolved.insert(
                    normalized,
                    ResolvedShortcut {
                        def: def.clone(),
                        backend: backend.clone(),
                    },
                );
            }
        }
        (resolved.into_values().collect(), diagnostics)
    }

    /// All provider definitions in plugin order.
    pub fn provider_defs(&self) -> Vec<serde_json::Value> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.registrations.providers.clone())
            .collect()
    }

    /// All virtual-model definitions in plugin order (upstream
    /// `registerVirtualModel`).
    pub fn virtual_model_defs(&self) -> Vec<serde_json::Value> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.registrations.virtual_models.clone())
            .collect()
    }
}

/// First-wins tool positions in dispatch order, plus the name → position map
/// the hot backend lookup uses.
fn resolve_tool_index(plugins: &[RegisteredPlugin]) -> ToolResolution {
    let mut resolved = Vec::new();
    let mut by_name: HashMap<String, (usize, usize)> = HashMap::new();
    for (plugin_index, plugin) in plugins.iter().enumerate() {
        for (tool_index, (def, _)) in plugin.registrations.tools.iter().enumerate() {
            if !by_name.contains_key(def.name.as_str()) {
                let position = (plugin_index, tool_index);
                by_name.insert(def.name.clone(), position);
                resolved.push(position);
            }
        }
    }
    ToolResolution { resolved, by_name }
}

/// Resolve command invocation names (upstream `resolveRegisteredCommands`).
/// Same algorithm the registry used to run per
/// [`PluginRegistry::resolved_commands`] call, but it now runs once per
/// registry snapshot and keeps only indices + the derived names.
fn resolve_command_entries(plugins: &[RegisteredPlugin]) -> Vec<ResolvedCommandEntry> {
    let mut commands: Vec<(usize, usize, &CommandDef)> = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (plugin_index, plugin) in plugins.iter().enumerate() {
        for (command_index, (def, _)) in plugin.registrations.commands.iter().enumerate() {
            commands.push((plugin_index, command_index, def));
            *counts.entry(def.name.clone()).or_insert(0) += 1;
        }
    }

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut resolved = Vec::with_capacity(commands.len());
    for (plugin, command, def) in commands {
        let occurrence = seen.get(&def.name).copied().unwrap_or(0) + 1;
        seen.insert(def.name.clone(), occurrence);

        let mut invocation_name = if counts.get(&def.name).copied().unwrap_or(0) > 1 {
            format!("{}:{}", def.name, occurrence)
        } else {
            def.name.clone()
        };
        if taken.contains(&invocation_name) {
            let mut suffix = occurrence;
            loop {
                suffix += 1;
                invocation_name = format!("{}:{}", def.name, suffix);
                if !taken.contains(&invocation_name) {
                    break;
                }
            }
        }
        taken.insert(invocation_name.clone());
        resolved.push(ResolvedCommandEntry {
            plugin,
            command,
            invocation_name,
        });
    }
    resolved
}

/// Claim-kind tags for [`claim_names`] (stable numeric order keeps the scan
/// order tool → command → flag, matching `claimed_names`).
const CLAIM_TOOL: u8 = 0;
const CLAIM_COMMAND: u8 = 1;
const CLAIM_FLAG: u8 = 2;

/// Borrowed form of [`PluginRegistrations::claimed_names`]: same order, no
/// `format!("tool:...")` allocation per name. Used by the build-time
/// omission/conflict passes, which only need to *compare* names.
fn claim_names(
    registrations: &super::plugin::PluginRegistrations,
) -> impl Iterator<Item = (u8, &str)> + '_ {
    registrations
        .tools
        .iter()
        .map(|(def, _)| (CLAIM_TOOL, def.name.as_str()))
        .chain(
            registrations
                .commands
                .iter()
                .map(|(def, _)| (CLAIM_COMMAND, def.name.as_str())),
        )
        .chain(
            registrations
                .flags
                .iter()
                .map(|flag| (CLAIM_FLAG, flag.name.as_str())),
        )
}

fn claim_kind_name(kind: u8) -> &'static str {
    match kind {
        CLAIM_COMMAND => "command",
        CLAIM_FLAG => "flag",
        _ => "tool",
    }
}

/// Drop replaceable plugins whose tool/command/flag names are claimed by a
/// non-replaceable plugin (upstream `omitReplacedExtensions`, including the
/// builtin-victim warning text).
fn omit_replaced(
    candidates: Vec<RegisteredPlugin>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Vec<RegisteredPlugin> {
    // Borrowed keys/owners: this runs on every registry rebuild, and the old
    // `claimed_names()` form allocated one `format!("kind:name")` string per
    // registered definition plus a cloned owner path per claim.
    let mut taken: HashMap<(u8, &str), &str> = HashMap::new();
    for plugin in &candidates {
        if plugin.info.replaceable {
            continue;
        }
        for (kind, name) in claim_names(&plugin.registrations) {
            taken.entry((kind, name)).or_insert(&plugin.info.path);
        }
    }
    let mut removed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (index, plugin) in candidates.iter().enumerate() {
        if !plugin.info.replaceable {
            continue;
        }
        let replacement = claim_names(&plugin.registrations)
            .find_map(|(kind, name)| taken.get(&(kind, name)).map(|owner| (kind, name, *owner)));
        let Some((kind, name, owner)) = replacement else {
            continue;
        };
        let kind_label = claim_kind_name(kind);
        let registered_name = match kind_label {
            "command" => format!("/{name}"),
            "flag" => format!("--{name}"),
            _ => name.to_owned(),
        };
        if let Some(builtin_name) = super::source::builtin_name(&plugin.info.path) {
            diagnostics.push(PluginDiagnostic {
                kind: DiagnosticKind::Warning,
                path: plugin.info.path.clone(),
                message: format!(
                    "Extension {owner} registers {kind_label} `{registered_name}`, so built-in extension `{builtin_name}` was not loaded. To use `{builtin_name}`, run `pi config` and make sure it is enabled under Built-in extensions, then disable or remove the existing extension. We recommend only having one or the other loaded at a time."
                ),
            });
        }
        removed.insert(index);
    }
    candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, plugin)| (!removed.contains(&index)).then_some(plugin))
        .collect()
}

/// Report tool/flag name clashes across different plugins (upstream
/// `detectExtensionConflicts`); all plugins stay loaded, precedence follows
/// dispatch order.
fn detect_conflicts(plugins: &[RegisteredPlugin], diagnostics: &mut Vec<PluginDiagnostic>) {
    // Borrowed keys/owners (no per-tool/flag name or path clone per rebuild).
    let mut tool_owners: HashMap<&str, &str> = HashMap::new();
    let mut flag_owners: HashMap<&str, &str> = HashMap::new();
    for plugin in plugins {
        for (def, _) in &plugin.registrations.tools {
            match tool_owners.get(def.name.as_str()) {
                Some(owner) if *owner != plugin.info.path.as_str() => {
                    diagnostics.push(PluginDiagnostic {
                        kind: DiagnosticKind::Error,
                        path: plugin.info.path.clone(),
                        message: format!("Tool \"{}\" conflicts with {}", def.name, *owner),
                    });
                }
                _ => {
                    tool_owners.insert(def.name.as_str(), plugin.info.path.as_str());
                }
            }
        }
        for flag in &plugin.registrations.flags {
            match flag_owners.get(flag.name.as_str()) {
                Some(owner) if *owner != plugin.info.path.as_str() => {
                    diagnostics.push(PluginDiagnostic {
                        kind: DiagnosticKind::Error,
                        path: plugin.info.path.clone(),
                        message: format!("Flag \"--{}\" conflicts with {}", flag.name, *owner),
                    });
                }
                _ => {
                    flag_owners.insert(flag.name.as_str(), plugin.info.path.as_str());
                }
            }
        }
    }
}
