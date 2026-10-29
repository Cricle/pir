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

use std::collections::BTreeMap;

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
        // Borrowed keys: every `ToolDef` lives in `self.plugins` for at least
        // as long as the returned slice, so deduping by `&str` answers the
        // same first-wins question without one `String` clone per registered
        // tool (this list is rebuilt per tool call / provider request).
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut resolved = Vec::new();
        for plugin in &self.plugins {
            for entry in &plugin.registrations.tools {
                if seen.insert(entry.0.name.as_str()) {
                    resolved.push(entry);
                }
            }
        }
        resolved
    }

    /// The first-registered backend for `name`, or `None`.
    ///
    /// The hot lookup behind [`crate::plugins::PluginHost::call_tool_with_context`]:
    /// walking the registrations in load order and returning on the first name
    /// match is exactly `resolved_tools().into_iter().find(...)` (first
    /// registration wins), but it allocates no intermediate `Vec` and clones
    /// no `ToolDef`. The caller only needs the backend; the definition is read
    /// from the registry when a provider request enumerates tools.
    pub fn tool_backend(&self, name: &str) -> Option<&ToolBackend> {
        for plugin in &self.plugins {
            for (def, backend) in &plugin.registrations.tools {
                if def.name == name {
                    return Some(backend);
                }
            }
        }
        None
    }

    /// Commands with `name:2`-style invocation dedup (upstream
    /// `resolveRegisteredCommands`).
    pub fn resolved_commands(&self) -> Vec<ResolvedCommand> {
        let mut commands: Vec<(&CommandDef, Option<&CommandBackend>)> = Vec::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for plugin in &self.plugins {
            for (def, backend) in &plugin.registrations.commands {
                commands.push((def, backend.as_ref()));
                *counts.entry(def.name.clone()).or_insert(0) += 1;
            }
        }

        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut resolved = Vec::with_capacity(commands.len());
        for (def, backend) in commands {
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
            resolved.push(ResolvedCommand {
                def: def.clone(),
                invocation_name,
                backend: backend.cloned(),
            });
        }
        resolved
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

/// Drop replaceable plugins whose tool/command/flag names are claimed by a
/// non-replaceable plugin (upstream `omitReplacedExtensions`, including the
/// builtin-victim warning text).
fn omit_replaced(
    candidates: Vec<RegisteredPlugin>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Vec<RegisteredPlugin> {
    let mut taken: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for plugin in &candidates {
        if plugin.info.replaceable {
            continue;
        }
        for name in plugin.registrations.claimed_names() {
            taken
                .entry(name)
                .or_insert_with(|| plugin.info.path.clone());
        }
    }
    candidates
        .into_iter()
        .filter(|plugin| {
            if !plugin.info.replaceable {
                return true;
            }
            let replacement = plugin
                .registrations
                .claimed_names()
                .into_iter()
                .find_map(|name| taken.get(&name).map(|owner| (name, owner.clone())));
            let Some((name, owner)) = replacement else {
                return true;
            };
            let mut parts = name.splitn(2, ':');
            let kind = parts.next().unwrap_or("tool");
            let raw = parts.next().unwrap_or("");
            let registered_name = match kind {
                "command" => format!("/{raw}"),
                "flag" => format!("--{raw}"),
                _ => raw.to_owned(),
            };
            if let Some(builtin_name) =
                super::source::builtin_name(&plugin.info.path)
            {
                diagnostics.push(PluginDiagnostic {
                    kind: DiagnosticKind::Warning,
                    path: plugin.info.path.clone(),
                    message: format!(
                        "Extension {owner} registers {kind} `{registered_name}`, so built-in extension `{builtin_name}` was not loaded. To use `{builtin_name}`, run `pi config` and make sure it is enabled under Built-in extensions, then disable or remove the existing extension. We recommend only having one or the other loaded at a time."
                    ),
                });
            }
            false
        })
        .collect()
}

/// Report tool/flag name clashes across different plugins (upstream
/// `detectExtensionConflicts`); all plugins stay loaded, precedence follows
/// dispatch order.
fn detect_conflicts(plugins: &[RegisteredPlugin], diagnostics: &mut Vec<PluginDiagnostic>) {
    let mut tool_owners: BTreeMap<String, String> = BTreeMap::new();
    let mut flag_owners: BTreeMap<String, String> = BTreeMap::new();
    for plugin in plugins {
        for (def, _) in &plugin.registrations.tools {
            match tool_owners.get(&def.name) {
                Some(owner) if owner != &plugin.info.path => {
                    diagnostics.push(PluginDiagnostic {
                        kind: DiagnosticKind::Error,
                        path: plugin.info.path.clone(),
                        message: format!("Tool \"{}\" conflicts with {}", def.name, owner),
                    });
                }
                _ => {
                    tool_owners.insert(def.name.clone(), plugin.info.path.clone());
                }
            }
        }
        for flag in &plugin.registrations.flags {
            match flag_owners.get(&flag.name) {
                Some(owner) if owner != &plugin.info.path => {
                    diagnostics.push(PluginDiagnostic {
                        kind: DiagnosticKind::Error,
                        path: plugin.info.path.clone(),
                        message: format!("Flag \"--{}\" conflicts with {}", flag.name, owner),
                    });
                }
                _ => {
                    flag_owners.insert(flag.name.clone(), plugin.info.path.clone());
                }
            }
        }
    }
}
