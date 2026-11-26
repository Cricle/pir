//! Extension-decided project trust (upstream `ProjectTrustEventResult`,
//! `core/extensions/types.ts:700-722`, consumed by
//! `core/project-trust.ts:55-70`).
//!
//! Extensions are the FIRST trust input in upstream's resolution order: a
//! handler returning `{trusted: "yes"}` (or `"no"`) decides the session
//! before the trust store / `defaultProjectTrust` policy are consulted;
//! `"undecided"` falls through. pir dispatches `project_trust` at extension
//! load time and again for native plugins once the plugin host exists, so
//! the decision must survive from the loader to the session build.
//!
//! This module owns the process-wide record of those decisions (keyed by the
//! normalized working directory) and the resolution helper every trust
//! consumer uses:
//!
//! 1. `PIR_TRUST_PROJECT=1` (the explicit operator override) always wins;
//! 2. the extension decision, when one was recorded;
//! 3. the persisted trust store / `defaultProjectTrust` policy
//!    ([`crate::config::trust::resolve_cwd_trust`]).
//!
//! A decision with `remember: true` is persisted to `<agent_dir>/trust.json`
//! by the dispatcher ([`crate::agent_session::dispatch_project_trust`]);
//! a session-only decision lives in this map for the process lifetime, which
//! is the closest native equivalent of upstream's per-run
//! `SettingsManager.setProjectTrusted(true)`.
//!
//! The CLI startup host resolves the boolean the run actually uses (its
//! explicit `--approve`/`--no-approve`, then the same env > extension > store
//! order above, including the interactive selector) and calls
//! [`pin_decision`]: later dispatches — the final extension load after trust
//! is resolved, a session reload — cannot change it, matching upstream's
//! one-shot `emitProjectTrustEvent` before `projectTrusted` is frozen.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value as JsonValue;

/// One extension trust decision (upstream `ProjectTrustEventDecision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrustDecision {
    /// Trust the project.
    Yes,
    /// Do not trust the project.
    No,
    /// No decision (fall through to the next handler / the store).
    Undecided,
}

impl ProjectTrustDecision {
    /// Parse the upstream wire value (`"yes"` / `"no"` / `"undecided"`).
    /// Boolean values are accepted for native-host compatibility (`true` =
    /// yes, `false` = no).
    pub fn from_wire(value: Option<&JsonValue>) -> Self {
        match value {
            Some(JsonValue::String(text)) if text == "yes" => Self::Yes,
            Some(JsonValue::String(text)) if text == "no" => Self::No,
            Some(JsonValue::Bool(true)) => Self::Yes,
            Some(JsonValue::Bool(false)) => Self::No,
            _ => Self::Undecided,
        }
    }

    /// The boolean decision, or `None` for [`ProjectTrustDecision::Undecided`].
    pub fn trusted(self) -> Option<bool> {
        match self {
            Self::Yes => Some(true),
            Self::No => Some(false),
            Self::Undecided => None,
        }
    }

    /// The upstream wire value.
    pub fn wire_key(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Undecided => "undecided",
        }
    }
}

/// Recorded decisions keyed by [`crate::config::trust::normalize_cwd`] output.
static DECISIONS: Mutex<BTreeMap<PathBuf, bool>> = Mutex::new(BTreeMap::new());

/// Directories whose decision was pinned by the current run's startup gate
/// ([`pin_decision`]). Later `project_trust` dispatches (the final extension
/// load after the trust is resolved, a reload) cannot overwrite it: upstream
/// emits the event once, before the store is consulted, and the resolved
/// `projectTrusted` then governs the run.
static PINNED: Mutex<std::collections::BTreeSet<PathBuf>> =
    Mutex::new(std::collections::BTreeSet::new());

/// Record one decided result for `cwd` (a no-op for `Undecided`, and for a
/// directory whose decision is already pinned by the run's startup gate).
pub fn record_decision(cwd: &Path, decision: ProjectTrustDecision) {
    let Some(trusted) = decision.trusted() else {
        return;
    };
    let key = crate::config::trust::normalize_cwd(cwd);
    let pinned = PINNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&key);
    if pinned {
        return;
    }
    DECISIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, trusted);
}

/// Pin `cwd` to this run's resolved trust decision and record it. Called by
/// the CLI startup host once the full order (explicit override, environment
/// override, extension decision, store/policy/selector) produced the boolean
/// the run actually uses, so every later consumer in this process — the
/// session's settings load, the extension snapshot, a session rebuilt after a
/// reload — sees the same value, and a later extension load cannot change it.
pub fn pin_decision(cwd: &Path, trusted: bool) {
    let key = crate::config::trust::normalize_cwd(cwd);
    DECISIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.clone(), trusted);
    PINNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key);
}

/// The recorded decision for `cwd`, when an extension made one.
pub fn recorded_decision(cwd: &Path) -> Option<bool> {
    DECISIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&crate::config::trust::normalize_cwd(cwd))
        .copied()
}

/// Whether [`pin_decision`] fixed `cwd` for this process. A pinned decision
/// makes later `project_trust` dispatches observational only: neither the
/// recorded value nor `remember: true` may change the decision the run
/// already resolved.
pub fn is_decision_pinned(cwd: &Path) -> bool {
    PINNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&crate::config::trust::normalize_cwd(cwd))
}

/// Drop every recorded decision and pin (test/embedding reset seam; not part
/// of the extension API).
///
/// This is a PROCESS-wide wipe: in a binary running several independent
/// scenarios at once, one caller's teardown erases another's in-flight pin.
/// A caller that owns exactly one directory (one scratch project per
/// scenario) uses [`clear_recorded_decision`] instead.
#[doc(hidden)]
pub fn clear_recorded_decisions() {
    DECISIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    PINNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Drop the recorded decision and pin for ONE `cwd`, leaving every other
/// directory's state untouched (the per-scenario teardown).
#[doc(hidden)]
pub fn clear_recorded_decision(cwd: &Path) {
    let key = crate::config::trust::normalize_cwd(cwd);
    DECISIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
    PINNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
}

/// [`env_override_decision`] against an EXPLICIT raw environment value: only
/// the documented `1` spelling forces trust; every other value (and an absent
/// variable) leaves the decision to the extension / store, because upstream
/// has no "force untrusted" environment override.
///
/// Pure, so a caller that owns its resolution inputs — an embedder, or a test
/// that must not write the process environment — states the operator override
/// as an argument instead of racing every other consumer in the process.
pub fn env_override_decision_value(value: Option<&str>) -> Option<bool> {
    value.is_some_and(|text| text == "1").then_some(true)
}

/// The explicit operator override (`PIR_TRUST_PROJECT=1`) as a decision.
///
/// The only ambient read in the trust chain: it is the process-level operator
/// flag. Everything downstream takes the resolved override as an argument
/// (the `_with` forms), so adding another environment layer here is the whole
/// exposure.
pub fn env_override_decision() -> Option<bool> {
    env_override_decision_value(std::env::var("PIR_TRUST_PROJECT").ok().as_deref())
}

/// The first two layers of the upstream trust order: the operator override,
/// then the extension decision recorded by [`record_decision`]. `None` means
/// neither applies and the trust store / `defaultProjectTrust` policy decides.
///
/// Callers that can show a UI selector (the CLI startup host) use this to
/// skip the selector when the decision is already known: an extension can
/// both grant and deny trust before the store is consulted
/// (`core/project-trust.ts:55-70`).
///
/// Consumers that load project-scoped state AFTER the startup gate (model
/// environment, print resources) must use [`resolved_decision`] instead, so a
/// pinned explicit `--approve`/`--no-approve` is not undone by the environment
/// layer.
pub fn override_or_extension_decision(cwd: &Path) -> Option<bool> {
    override_or_extension_decision_with(cwd, env_override_decision())
}

/// [`override_or_extension_decision`] with the operator override supplied by
/// the caller instead of read from `PIR_TRUST_PROJECT`.
pub fn override_or_extension_decision_with(
    cwd: &Path,
    operator_override: Option<bool>,
) -> Option<bool> {
    operator_override.or_else(|| recorded_decision(cwd))
}

/// The trust decision consumers that load project-scoped state AFTER the
/// startup gate must apply: the run's pinned decision when
/// [`pin_decision`] fixed one, otherwise the headless order
/// ([`override_or_extension_decision`]). `None` means neither applies and the
/// trust store / `defaultProjectTrust` policy decides.
///
/// The pinned boolean is authoritative because it is exactly what the run gate
/// resolved — including the explicit `--approve`/`--no-approve`, which
/// outranks `PIR_TRUST_PROJECT=1`. Re-applying the environment layer here (as
/// [`override_or_extension_decision`] does) would undo a `--no-approve`.
pub fn resolved_decision(cwd: &Path) -> Option<bool> {
    resolved_decision_with(cwd, env_override_decision())
}

/// [`resolved_decision`] with the operator override supplied by the caller
/// instead of read from `PIR_TRUST_PROJECT`.
pub fn resolved_decision_with(cwd: &Path, operator_override: Option<bool>) -> Option<bool> {
    if is_decision_pinned(cwd) {
        return recorded_decision(cwd);
    }
    override_or_extension_decision_with(cwd, operator_override)
}

/// Resolve whether `cwd` is a trusted project using the full upstream order
/// (the pinned run decision, else env override, extension decision, trust
/// store / policy).
pub fn resolve_project_trust(cwd: &Path, agent_dir: &Path) -> bool {
    resolve_project_trust_with(cwd, agent_dir, env_override_decision())
}

/// [`resolve_project_trust`] with the operator override supplied by the
/// caller instead of read from `PIR_TRUST_PROJECT`.
pub fn resolve_project_trust_with(
    cwd: &Path,
    agent_dir: &Path,
    operator_override: Option<bool>,
) -> bool {
    if let Some(trusted) = resolved_decision_with(cwd, operator_override) {
        return trusted;
    }
    crate::config::trust::resolve_cwd_trust_with(cwd, agent_dir, operator_override)
}
