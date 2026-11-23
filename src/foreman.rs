//! Public helpers for the detached `pir foreman` CLI.
//!
//! The mode plugin owns the live state; this module is the small bridge that
//! reads its sidecar/journal and applies approval decisions from a separate
//! process. It deliberately exposes plain strings rather than the plugin's
//! internal enums.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForemanRouter {
    Inline,
    Process,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForemanApprovals {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForemanApproval {
    pub id: String,
    pub call: String,
    pub summary: String,
    pub state: String,
    pub kind: String,
    /// The submitted plan text when `kind == "plan"`.
    pub plan: Option<String>,
    /// The execution verification contract when `kind == "plan"`.
    pub verification: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForemanStatus {
    pub mode: String,
    pub phase: String,
    pub generation: u32,
    pub nudges: u32,
    pub allow_next_worker: bool,
    pub pending: Vec<ForemanApproval>,
}

/// Write the `.mode.json` sidecar that the detached child restores on start.
pub fn write_running_sidecar(
    session_path: &Path,
    worktree: bool,
    router: ForemanRouter,
    approvals: ForemanApprovals,
) -> std::io::Result<()> {
    use crate::plugins::builtin::mode::{
        start_foreman_session, ApprovalMode, ModeSettings, RouterMode,
    };
    let settings = ModeSettings {
        approvals: match approvals {
            ForemanApprovals::Auto => ApprovalMode::Auto,
            ForemanApprovals::Manual => ApprovalMode::Manual,
        },
        router: match router {
            ForemanRouter::Inline => RouterMode::Inline,
            ForemanRouter::Process => RouterMode::Process,
        },
        worktree,
    };
    start_foreman_session(session_path, settings)?;
    Ok(())
}

/// Read the durable foreman state. `None` means no journal has been written.
pub fn read_status(session_path: &Path) -> Option<ForemanStatus> {
    let state = crate::plugins::builtin::mode::read_journal_state(session_path)?;
    Some(ForemanStatus {
        mode: state.mode.label().to_owned(),
        phase: state.phase.label().to_owned(),
        generation: state.generation,
        nudges: state.nudges,
        allow_next_worker: state.allow_next_worker,
        pending: state
            .approvals
            .into_iter()
            .filter(|record| record.is_pending())
            .map(|record| ForemanApproval {
                id: record.id,
                call: record.call,
                summary: record.summary,
                state: record.state.label().to_owned(),
                kind: record.kind.label().to_owned(),
                plan: record.plan.clone(),
                verification: record.verification.clone(),
                created_at_ms: record.created_at_ms,
            })
            .collect(),
    })
}

/// Apply an approval decision and persist it. `id=None` decides every pending
/// approval. When an approved plan is decided, the session sidecar is switched
/// to normal mode so the detached execution child starts with full tools.
pub fn decide_approval(
    session_path: &Path,
    id: Option<&str>,
    approved: bool,
) -> Result<Vec<ForemanApproval>, String> {
    crate::plugins::builtin::mode::decide_approval_for_session(session_path, id, approved).map(
        |records| {
            let has_approved_plan = approved
                && records.iter().any(|record| {
                    record.kind == crate::plugins::builtin::mode::ModeApprovalKind::Plan
                });
            if has_approved_plan {
                crate::plugins::builtin::mode::mark_normal_after_plan(session_path);
            }
            records
                .into_iter()
                .map(|record| ForemanApproval {
                    id: record.id,
                    call: record.call,
                    summary: record.summary,
                    state: record.state.label().to_owned(),
                    kind: record.kind.label().to_owned(),
                    plan: record.plan.clone(),
                    verification: record.verification.clone(),
                    created_at_ms: record.created_at_ms,
                })
                .collect()
        },
    )
}
