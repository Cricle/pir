//! Startup settings diagnostics (upstream `core/settings-diagnostics.ts`).
//!
//! The settings manager records load failures — malformed JSON, a non-object
//! root, an unreadable file — without failing startup. This module turns them
//! into the upstream-shaped runtime diagnostics that the CLI reports on
//! stderr (`Warning: …`) and, eventually, the interactive hosts surface as a
//! chat notice.
//!
//! Scope: only [`SettingsManager::drain_errors`] entries are collected here.
//! pir's per-field tolerant-parse warnings ([`SettingsManager::warning_messages`])
//! are a pir-only superset: those fields already fell back to their defaults
//! and the rest of the document is usable, so they stay out of this list and
//! remain available separately.

use std::collections::HashSet;

use super::*;

/// Severity of one startup settings diagnostic (upstream
/// `AgentSessionRuntimeDiagnostic["type"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsDiagnosticType {
    Info,
    Warning,
    Error,
}

/// One startup settings diagnostic (upstream
/// `AgentSessionRuntimeDiagnostic`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsDiagnostic {
    pub type_: SettingsDiagnosticType,
    pub message: String,
}

impl SettingsDiagnostic {
    /// The stderr prefix upstream `reportDiagnostics` uses for this severity
    /// (`""` for info, `"Warning: "`, `"Error: "`).
    pub fn stderr_prefix(&self) -> &'static str {
        match self.type_ {
            SettingsDiagnosticType::Info => "",
            SettingsDiagnosticType::Warning => "Warning: ",
            SettingsDiagnosticType::Error => "Error: ",
        }
    }

    /// The complete line the non-interactive startup prints to stderr.
    pub fn stderr_line(&self) -> String {
        format!("{}{}", self.stderr_prefix(), self.message)
    }
}

impl SettingsError {
    /// The upstream diagnostic message for this error:
    /// `Invalid settings file ${path}: ${error.message}` when a path was
    /// recorded, else `Invalid ${scope} settings: ${error.message}`.
    pub fn diagnostic_message(&self) -> String {
        match &self.path {
            Some(path) => format!("Invalid settings file {}: {}", path.display(), self.message),
            None => format!("Invalid {} settings: {}", self.scope_name(), self.message),
        }
    }
}

/// Drain the settings manager's recorded load errors as warning diagnostics
/// (upstream `collectSettingsDiagnostics`). Non-draining per-field warnings
/// are deliberately excluded; see the module docs.
pub fn collect_settings_diagnostics(manager: &mut SettingsManager) -> Vec<SettingsDiagnostic> {
    manager
        .drain_errors()
        .into_iter()
        .map(|error| SettingsDiagnostic {
            type_: SettingsDiagnosticType::Warning,
            message: error.diagnostic_message(),
        })
        .collect()
}

/// Remove duplicate `(type, message)` diagnostics, preserving the first
/// occurrence (upstream `deduplicateDiagnostics`). Startup and runtime
/// settings managers can report the same file error.
pub fn deduplicate_diagnostics(diagnostics: Vec<SettingsDiagnostic>) -> Vec<SettingsDiagnostic> {
    let mut seen = HashSet::new();
    diagnostics
        .into_iter()
        .filter(|diagnostic| seen.insert((diagnostic.type_, diagnostic.message.clone())))
        .collect()
}
