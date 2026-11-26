//! CC residual trust propagation (§2 #12): `load_pi_config_with` is the
//! `pir models list` / RPC / TUI config resolver (`load_pi_config` wraps it),
//! so it must use the same pinned-aware project-trust order as the run gate:
//!
//! 1. the pinned startup decision (extension grant/denial or an explicit
//!    `--approve`/`--no-approve`) when the host resolved one;
//! 2. otherwise the headless order (`PIR_TRUST_PROJECT=1`, then the recorded
//!    extension decision, then the trust store / `defaultProjectTrust`);
//! 3. with no extension decision at all, the store/policy fallback is exactly
//!    the old `resolve_cwd_trust` behavior.
//!
//! The fixtures use the process-global `PIR_AGENT_DIR` (shared guard) and the
//! engine-free decision record: `dispatch_project_trust` stores exactly this
//! [`record_decision`] value, so the grant path is the state an extension
//! loader leaves behind.

use std::path::{Path, PathBuf};

use pir::ai::providers::{load_pi_config_with_in, load_pi_config_with_in_and_override};
use pir::config::trust::ProjectTrustStore;
use pir::extensions::project_trust::{
    clear_recorded_decision, pin_decision, record_decision, ProjectTrustDecision,
};

const PROJECT_THEME: &str = "project-config-theme";

/// A project dir with trust-gated `.pi/settings.json` and an agent dir whose
/// `models.json` names one provider, so `load_pi_config_with` returns `Some`
/// and the settled settings snapshot is observable.
fn fixture(label: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "pir-pi-config-trust-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let project = root.join("project");
    let agent = root.join("agent");
    std::fs::create_dir_all(project.join(".pi")).expect("project dir");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::write(
        project.join(".pi/settings.json"),
        format!(r#"{{"theme":"{PROJECT_THEME}"}}"#),
    )
    .expect("project settings");
    std::fs::write(
        agent.join("models.json"),
        r#"{
            "providers": {
                "acme": {
                    "name": "Acme",
                    "baseUrl": "https://acme.example/v1",
                    "api": "openai-completions",
                    "models": [{ "id": "acme-large" }]
                }
            }
        }"#,
    )
    .expect("models.json");
    (project, agent)
}

/// The config resolver bound to `agent` — the explicit-dir form, so no test
/// here relies on the process agent dir.
fn configured_theme_at(agent: &Path, project: &Path) -> Option<String> {
    load_pi_config_with_in(agent, Some(project))
        .expect("load pi config")
        .expect("models.json must produce a configured model")
        .settings
        .theme
        .clone()
}

/// Clears ONE scenario's recorded extension decision and pin, on entry and on
/// drop. Scoped to the test's own project directory: the process-wide
/// `clear_recorded_decisions()` would erase a concurrently running scenario's
/// in-flight decision, which is contention the suite must not have.
struct DecisionsGuard(std::path::PathBuf);

impl DecisionsGuard {
    fn new(project: &Path) -> Self {
        clear_recorded_decision(project);
        Self(project.to_path_buf())
    }
}

impl Drop for DecisionsGuard {
    fn drop(&mut self) {
        clear_recorded_decision(&self.0);
    }
}

/// An extension grant recorded before the config load opens the project
/// settings scope for `models list` / RPC / TUI, even though the grant was
/// session-only (nothing written to `trust.json`).
#[tokio::test(flavor = "current_thread")]
async fn models_config_reads_project_scope_after_an_extension_grant() {
    let (project, agent) = fixture("grant");
    let _decisions = DecisionsGuard::new(&project);

    record_decision(&project, ProjectTrustDecision::Yes);
    assert_eq!(
        ProjectTrustStore::new(&agent)
            .get(&project)
            .map(|entry| entry.decision),
        None,
        "the session-only grant must not write trust.json"
    );

    assert_eq!(
        configured_theme_at(&agent, &project).as_deref(),
        Some(PROJECT_THEME),
        "an extension grant must open the project settings scope for the config resolver"
    );
    let _ = std::fs::remove_dir_all(project.parent().expect("fixture root"));
}

/// The startup gate's pinned denial (`--no-approve` / an interactive "no")
/// wins over both a recorded extension grant and an operator override
/// (`PIR_TRUST_PROJECT=1`); the config resolver must keep the project scope
/// closed.
#[tokio::test(flavor = "current_thread")]
async fn models_config_honors_a_pinned_denial_over_an_extension_grant() {
    let (project, agent) = fixture("pinned-deny");
    let _decisions = DecisionsGuard::new(&project);

    pin_decision(&project, false);
    // A later extension decision cannot change the pinned run decision.
    record_decision(&project, ProjectTrustDecision::Yes);

    // The operator override outranks the extension decision but not the pin.
    // It is supplied explicitly — the value `PIR_TRUST_PROJECT=1` would carry —
    // so this test needs no process-wide environment mutation.
    let theme = load_pi_config_with_in_and_override(&agent, Some(&project), Some(true))
        .expect("load pi config")
        .expect("models.json must produce a configured model")
        .settings
        .theme
        .clone();
    assert_eq!(
        theme, None,
        "a pinned no-approve must keep the config resolver off the project settings"
    );
    let _ = std::fs::remove_dir_all(project.parent().expect("fixture root"));
}

/// With no extension decision, the config resolver keeps the old fallback:
/// the trust store's nearest-ancestor decision, then `defaultProjectTrust`
/// (headless `ask` resolves to untrusted).
#[tokio::test(flavor = "current_thread")]
async fn models_config_without_an_extension_decision_keeps_store_and_policy_fallback() {
    let (project, agent) = fixture("fallback");
    let _decisions = DecisionsGuard::new(&project);

    // No decision anywhere + `ask` under a headless load => untrusted.
    assert_eq!(configured_theme_at(&agent, &project), None);

    let store = ProjectTrustStore::new(&agent);
    store.set(&project, Some(true)).expect("store yes");
    assert_eq!(
        configured_theme_at(&agent, &project).as_deref(),
        Some(PROJECT_THEME),
        "no extension decision => a stored yes still opens the project scope"
    );

    store.set(&project, Some(false)).expect("store no");
    assert_eq!(
        configured_theme_at(&agent, &project),
        None,
        "no extension decision => a stored no still closes the project scope"
    );

    store.set(&project, None).expect("clear store");
    std::fs::write(
        agent.join("settings.json"),
        r#"{"defaultProjectTrust":"always"}"#,
    )
    .expect("agent settings");
    assert_eq!(
        configured_theme_at(&agent, &project).as_deref(),
        Some(PROJECT_THEME),
        "defaultProjectTrust:always must still decide when no extension did"
    );
    let _ = std::fs::remove_dir_all(project.parent().expect("fixture root"));
}
