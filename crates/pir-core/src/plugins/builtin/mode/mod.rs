//! `builtin:mode` — the user-triggered session mode command.
//!
//! Modes:
//!
//! - `normal`: the pre-mode behavior.
//! - `foreman`: the main session is a read-only router. It delegates every
//!   workspace mutation to the existing `subagent` tool and must pass an
//!   independent `reviewer` before finishing.
//! - `plan`: read-only investigation and a concrete plan; mutations are
//!   blocked and only read-only subagents are allowed.
//!
//! The plugin is deliberately small and does not touch the subagent tool or
//! its child-session engine. It uses the existing plugin seams:
//!
//! - `/mode` is a Rust-backed command, so only the user can switch modes (it
//!   is not a model-visible tool);
//! - `before_agent_start` adds the active mode contract as a prompt section;
//! - `tool_call` blocks direct mutating tools and records
//!   worker/reviewer `subagent` calls;
//! - `agent_before_settle` turns a missing or failed reviewer into a bounded
//!   continuation (at most [`MAX_GATE_NUDGES`] times) and marks the run
//!   `UNVERIFIED` when that budget is exhausted;
//! - `hidden_tool_declarations` removes the unusable tools from the
//!   model-facing request, so the model does not waste a turn on a blocked
//!   call (the `tool_call` gate remains the execution fence);
//! - `StatusService` renders the mode/phase cells and `UiService` posts the
//!   mode-change and gate notices.
//!
//! Mode selection is persisted in a tiny sidecar next to the session file
//! (`<session>.mode.json`) and restored on resume/reload. The worker/reviewer
//! gate state is reconstructed from the session transcript, so a resumed
//! foreman session continues with the same pending verification. Ephemeral
//! sessions (no session file) keep the mode in memory only.
//!
//! Worker child sessions load their own `ModePlugin` instance, whose state
//! starts at `Normal`; foreman/plan restrictions therefore never leak into
//! the existing single/parallel/chain subagent execution.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{Map, Value};

use super::builtin_extension_def;
use crate::extensions::CommandOutcome;
use crate::plugins::plugin::{
    AgentPlugin, CommandBackend, CommandHandler, EventInterest, PluginContext, PluginEvent,
    PluginEventOutcome, PluginFuture, RegistrationContext,
};
use crate::plugins::services::{
    DeliverAs, DialogOptions, NotifyLevel, SelectOption, SendUserMessageOptions, UserContent,
    WidgetPlacement,
};
use crate::plugins::source::PluginInfo;

mod approvals;
mod journal;
mod settings;
mod submit_plan;

pub(crate) use approvals::ApprovalKind as ModeApprovalKind;
use approvals::{now_ms, ApprovalKind, ApprovalRecord, ApprovalState};
use journal::{ExecutionContract, JournalEvent, JournalState, Verification};
pub(crate) use settings::{ApprovalMode, ModeSettings, RouterMode};

pub(crate) use journal::decide_approval_for_session;

/// Begin a detached foreman run: set the sidecar AND the journal to a fresh
/// foreman state. The journal takes precedence on restore, so writing only the
/// sidecar would silently drop CLI flags on sessions that already have one.
pub fn start_foreman_session(session_path: &Path, settings: ModeSettings) -> std::io::Result<()> {
    let sidecar = sidecar_path_for(session_path);
    persist_mode_path(&sidecar, Mode::Foreman, settings);
    let state = JournalState {
        mode: Mode::Foreman,
        phase: Phase::Planning,
        settings,
        ..JournalState::default()
    };
    match journal::ForemanJournal::open(session_path) {
        Ok(mut journal) => journal.write_snapshot(&state, true),
        Err(error) => Err(error),
    }
}

/// Rewrite the mode sidecar to normal after an approved plan, so the detached
/// execution child starts with the normal (full) tool surface.
pub fn mark_normal_after_plan(session_path: &Path) {
    let settings = journal::read_state(session_path)
        .map(|state| state.settings)
        .unwrap_or_default();
    let sidecar = sidecar_path_for(session_path);
    persist_mode_path(&sidecar, Mode::Normal, settings);
}

/// Read a session journal for the detached CLI.
pub fn read_journal_state(session_path: &Path) -> Option<JournalState> {
    journal::read_state(session_path)
}

/// `builtin:mode` name (the `builtin:` prefix is added by [`PluginInfo`]).
pub use pir_builtin_mode::PLUGIN_NAME;
/// The slash command. It is never registered as a tool, so the model cannot
/// enter or leave a mode on its own.
pub const COMMAND_NAME: &str = "mode";
/// Settings command (`/foreman`). Also not model-visible.
pub const FOREMAN_COMMAND_NAME: &str = "foreman";
/// Approval queue command (`/approvals`).
pub const APPROVALS_COMMAND_NAME: &str = "approvals";
/// Footer status key for the pending-approval count.
pub const APPROVALS_STATUS_KEY: &str = "foreman_approvals";
/// Widget key for the pending-approval badge.
pub const APPROVALS_WIDGET_KEY: &str = "foreman_approvals";
/// Footer status key for the mode badge.
pub const STATUS_KEY: &str = "mode";
/// Footer status key for the mode phase.
pub const PHASE_STATUS_KEY: &str = "mode_phase";
/// Footer text while foreman mode is active.
pub const FOREMAN_STATUS: &str = "MODE: FOREMAN";
/// Footer text while plan mode is active.
pub const PLAN_STATUS: &str = "MODE: PLAN";
/// Prompt section key used by the foreman `before_agent_start` hook.
pub const FOREMAN_SECTION: &str = "foreman_mode";
/// Prompt section key used by the plan `before_agent_start` hook.
pub const PLAN_SECTION: &str = "plan_mode";
/// Custom-message type used by the settle gate continuation.
pub const GATE_CUSTOM_TYPE: &str = "foreman_gate";
/// Maximum settle-gate continuations for one worker generation.
pub const MAX_GATE_NUDGES: u32 = 3;
/// Sidecar suffix appended to the session file path.
const MODE_SIDECAR_SUFFIX: &str = ".mode.json";

/// The settle-gate instruction. It is a custom message, so it projects as a
/// model-facing user turn and stays visible in the transcript.
pub const GATE_MESSAGE: &str = "FOREMAN GATE: the latest worker has not passed an independent reviewer. Call the `subagent` tool with agent \"reviewer\". For a runnable deliverable, pass its path, working directory, an authorized smoke command, and the expected startup and first execution cycle. If no safe command is available, report the blocker rather than accepting syntax-only evidence. The reviewer must reply with a JSON object on its first line, {\"verdict\":\"PASS\"|\"FAIL\",\"evidence\":\"...\"}; a plain PASS or FAIL first line is also accepted. If it fails, delegate the fixes with a worker and re-run the reviewer before finishing.";

/// The gate-cap warning: the run may settle, but it is no longer described as
/// verified.
pub const UNVERIFIED_MESSAGE: &str = "FOREMAN UNVERIFIED: the reviewer gate reached its 3-nudge limit. The run may finish, but the result has not been independently verified. Next: send a new message to rework it with a fresh reviewer budget (a new worker delegation resets the gate), or switch to /mode normal to accept the result unverified.";

/// Custom-message type for the plan execution-contract verification gate.
pub const VERIFICATION_GATE_CUSTOM_TYPE: &str = "verification_gate";
/// Model-facing nudge when an approved plan has no successful verification tool
/// result yet. Domain-neutral: it asks for a real command, not a specific check.
pub const VERIFICATION_GATE_MESSAGE: &str = "EXECUTION CONTRACT: the approved plan is not verified yet. Run a real verification command with the available tools and confirm it succeeded (zero exit status) before reporting done. Reasoning or claiming 'verified' does not count; a successful tool result is required. Fix any failure and run it again.";
/// Cap-warning for the contract verification gate.
pub const VERIFICATION_UNVERIFIED_MESSAGE: &str = "EXECUTION CONTRACT UNVERIFIED: the verification gate reached its 3-nudge limit. The run may finish, but the approved plan's verification criteria were not confirmed by a tool result. Next: run one of the approved verification commands; a success still retires the contract. If it cannot pass, send a new message describing the remaining failure.";

/// Model-facing nudge when the latest worker failed or was aborted.
pub const WORKER_FAILURE_GATE_MESSAGE: &str = "FOREMAN GATE: the latest worker call failed or was aborted (error, non-zero exit, or stderr). A reviewer cannot pass failed work. Delegate a concrete fix to a worker, confirm it succeeds, then run the reviewer again before finishing.";

/// Direct tools a foreman/plan session may call. Everything else is blocked at
/// the `tool_call` seam and removed from the model-facing declarations.
///
/// The allowlist and the plan/reviewer child sets are owned by the engine-free
/// crate (they are the delegation contract); re-exported here.
pub use pir_builtin_mode::{MODE_ALLOWED_TOOLS, PLAN_CHILD_TOOLS, REVIEWER_CHILD_TOOLS};

/// The foreman contract. This is intentionally static: user input never
/// becomes part of the system prompt through this plugin.
pub const FOREMAN_PROMPT: &str = r#"<foreman_mode>
You are in FOREMAN mode. The user explicitly enabled this mode.

Your job is to act as the foreman/router:
- Understand the user's goal, then make a short plan. Use the `todo` tool for
  a multi-step plan when useful.
- Do not edit files, run shell commands, or otherwise mutate the workspace
  yourself. Those direct tools are hidden and blocked in this mode.
- Your model-facing tool surface is restricted in this mode. Only the
  read-only tools, `subagent`, `todo`, and background status/abort tools are
  exposed. Never try to call an unlisted tool; delegate that work instead.
- Delegate every workspace mutation and command execution to the existing
  `subagent` tool. Use single mode for one task, parallel mode for independent
  tasks, and chain mode for dependent steps.
- Every worker task must be a compact brief, not a copy of the whole
  conversation: the goal, constraints, files/paths, and a concrete delivery
  contract (deliverable, minimum verifiable result, how success is checked).
  A worker that only reports progress, or whose call errors/aborts, has NOT
  delivered; do not treat it as done.
- Use only read-only tools (`read`, `grep`, `find`, `ls`, `tool_search`) to
  inspect the workspace and worker results.
- A worker whose call is errored or aborted is a failure. Delegate a concrete
  fix or report the blocker; never ask a reviewer to pass failed work. The
  settle gate rejects a reviewer PASS that follows a failed/aborted worker.
- After ANY successful worker delegation, run an independent verification with a
  `subagent` call whose agent is exactly `reviewer` before finishing.
- Give the reviewer the diff/test commands and the acceptance checklist, not
  the whole parent transcript. For a runnable deliverable, also give its path,
  working directory, an authorized smoke command, and the expected startup and
  first execution cycle. Require independent execution on the delivered version,
  not syntax-only evidence or a worker's report. If no safe command is
  available, report the blocker; do not settle with a PASS.
- The reviewer must reply with a JSON object on its first line:
  {"verdict":"PASS"|"FAIL","visual":"verified"|"not_checked","evidence":"..."}.
  `visual` may be `verified` only when a rendered artifact/screenshot was
  actually inspected; otherwise it must be `not_checked`. A plain `PASS` or
  `FAIL` first line is accepted for compatibility. Treat only `PASS` as
  accepted.
- On `FAIL`, an errored reviewer, or no reviewer result, delegate concrete
  fixes and verify again. Do not report completion in those states.
- If you try to finish without a passing reviewer, the settle gate sends you
  back (at most three times). Treat each gate message as a required next step.
  If the gate limit is reached, the UI marks the run UNVERIFIED.
- Report a short plan/phase summary to the user. Do not dump raw worker
  transcripts; the `/subagents` view already carries the details.
- NEVER waste time with sleep/poll loops. Subagent results and gate messages
  arrive as messages after your turn: issue the tool call, then END your turn
  and wait for the notification instead of sleeping, retrying, or doing busy
  work. If an approval is pending, ask the user and end the turn.
- Stay in foreman mode until the user runs `/mode normal` or `/mode plan`.
</foreman_mode>"#;

/// The plan-mode contract.
pub const PLAN_PROMPT: &str = r#"<plan_mode>
You are in PLAN mode. The user explicitly enabled this mode.

Your job is to investigate and produce a concrete plan:
- Do not edit files, run mutating shell commands, or otherwise change the
  workspace. Those tools are hidden and blocked in this mode.
- Your model-facing tool surface is restricted to read-only tools and
  `subagent`; only read-only subagents (`scout`, `planner`, `reviewer`) are
  allowed. Never try to call an unlisted tool.
- Inspect the relevant files and repository state. You may delegate read-only
  investigation to scout/planner subagents.
- Produce a concrete plan: steps in order, files to change, risks, and how to
  verify the result. Do not execute the plan.
- When the plan is ready, call the `submit_plan` tool with the full plan text.
  `verification` is REQUIRED and must be a non-empty array of exact shell
  commands (for example ["cargo test"]); prose is rejected because it cannot
  be checked. The approved plan completes only when one of those commands runs
  and succeeds. For a deliverable that can run (a program, script, or page),
  at least one command must execute the delivered artifact's startup and first
  execution cycle; a parser, grep, or source inspection alone is not enough.
  If no safe runtime command exists, say so in the plan and name the closest
  executable check. That sends it to the user for approval. If the user approves,
  the session switches to normal mode and a follow-up message asks you to
  implement the approved plan. If the user denies or asks for changes, revise
  the plan and submit it again. Do not try to execute the plan before it is
  approved.
- Ask for clarification only if a missing decision blocks a correct plan.
- NEVER sleep, poll, or spin while investigation or approval is pending.
  Subagent results and plan decisions arrive as messages after you END your
  turn; after `submit_plan`, stop immediately and wait for the decision
  follow-up instead of doing more work or calling sleep/wait tools.
- Stay in plan mode until your plan is approved (which switches to normal), or
  until the user runs `/mode normal` or `/mode foreman`.
</plan_mode>"#;

const BUSY_MESSAGE: &str =
    "Cannot switch mode while a run is in progress; finish it, abort it, or use /mode normal --force.";
const USAGE_MESSAGE: &str = "Usage: /mode foreman | /mode plan | /mode normal [--force]";
// `Mode` / `Phase` are the host-neutral state labels; the single definition
// lives in the engine-free `pir-builtin-mode` crate.
pub use pir_builtin_mode::{Mode, Phase};

/// Per-plugin mode state. The parent session and each worker child session get
/// their own `ModePlugin` instance, so the state is naturally session-local.
struct ModeState {
    mode: AtomicU8,
    phase: AtomicU8,
    context: RwLock<Option<PluginContext>>,
    /// Session identity currently bound; a rebind to another session must
    /// reload that session's persisted mode instead of carrying the old one.
    session_id: RwLock<Option<String>>,
    session_path: RwLock<Option<PathBuf>>,
    /// Increments for every non-reviewer `subagent` call; a new worker makes
    /// any earlier reviewer verdict stale.
    worker_generation: AtomicU32,
    /// The `toolCallId` of the latest worker (non-reviewer) `subagent` call,
    /// used to detect a failed/aborted worker at the settle gate.
    last_worker_call_id: RwLock<Option<String>>,
    /// The `toolCallId` of the latest reviewer `subagent` call.
    last_reviewer_call_id: RwLock<Option<String>>,
    /// Settle-gate continuations already injected for this worker generation.
    gate_nudges: AtomicU32,
    /// The worker generation whose reviewer already returned PASS. Persisted
    /// so a resumed session does not re-gate an already verified worker.
    review_passed_generation: RwLock<Option<u32>>,
    settings: RwLock<ModeSettings>,
    approvals: RwLock<Vec<ApprovalRecord>>,
    /// Approval ids whose worker delegation was approved and may pass once
    /// without prompting again. Bound to specific requests so approving A can
    /// never admit a different task B; approve-all can grant several at once.
    /// Persisted for detached round-trips.
    allow_next_worker_ids: RwLock<HashSet<String>>,
    /// The plan text submitted by `submit_plan` in plan mode.
    submitted_plan: RwLock<Option<String>>,
    /// The approved plan's execution contract, carried into Normal mode.
    active_contract: RwLock<Option<ExecutionContract>>,
    /// Approval id whose verification evidence passed. Audit-only once the
    /// one-shot contract is retired.
    verification_passed_id: RwLock<Option<String>>,
    /// Tool-call id whose successful result completed the contract.
    verified_call: RwLock<Option<String>>,
    /// One-shot note announcing a mode transition on the next turn. A
    /// probabilistic model must be told the rules changed; swapping the
    /// system-prompt section alone leaves the old instructions in history.
    pending_transition: RwLock<Option<String>>,
    /// Declared verification commands for this session (`--check`, or the task
    /// record's checks once B lands). Separate from `settings` because
    /// `ModeSettings` is `Copy` and travels through the foreman routing paths;
    /// this is per-session data, not a mode preference.
    declared_checks: RwLock<Vec<String>>,
    journal: RwLock<Option<journal::ForemanJournal>>,
}

impl ModeState {
    fn new() -> Self {
        Self {
            mode: AtomicU8::new(Mode::Normal as u8),
            phase: AtomicU8::new(Phase::Idle as u8),
            context: RwLock::new(None),
            session_id: RwLock::new(None),
            session_path: RwLock::new(None),
            worker_generation: AtomicU32::new(0),
            last_worker_call_id: RwLock::new(None),
            last_reviewer_call_id: RwLock::new(None),
            gate_nudges: AtomicU32::new(0),
            review_passed_generation: RwLock::new(None),
            settings: RwLock::new(ModeSettings::default()),
            approvals: RwLock::new(Vec::new()),
            allow_next_worker_ids: RwLock::new(HashSet::new()),
            submitted_plan: RwLock::new(None),
            active_contract: RwLock::new(None),
            verification_passed_id: RwLock::new(None),
            verified_call: RwLock::new(None),
            pending_transition: RwLock::new(None),
            declared_checks: RwLock::new(Vec::new()),
            journal: RwLock::new(None),
        }
    }

    fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(Ordering::SeqCst))
    }

    fn set_mode(&self, mode: Mode) {
        self.mode.store(mode as u8, Ordering::SeqCst);
    }

    fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::SeqCst))
    }

    /// Set the advisory phase and repaint the footer when it changed.
    fn set_phase(&self, phase: Phase) {
        if self.phase.swap(phase as u8, Ordering::SeqCst) != phase as u8 {
            self.refresh_status();
        }
    }

    fn settings(&self) -> ModeSettings {
        *self
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_settings(&self, settings: ModeSettings) {
        *self
            .settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    }

    fn approvals(&self) -> Vec<ApprovalRecord> {
        self.approvals
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn pending_approvals(&self) -> Vec<ApprovalRecord> {
        self.approvals()
            .into_iter()
            .filter(ApprovalRecord::is_pending)
            .collect()
    }

    fn clear_approvals(&self) {
        self.approvals
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .allow_next_worker_ids
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = HashSet::new();
        self.refresh_approval_status();
    }

    /// Add a requested approval, deduplicating by the stable approval id.
    /// Returns the record id and whether this call created a new request.
    fn request_approval(&self, call: &str, input: &Value) -> (String, bool) {
        let id = approval_id(input);
        let mut approvals = self
            .approvals
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let summary = approval_summary(input);
        if let Some(existing) = approvals.iter_mut().find(|record| record.id == id) {
            if existing.is_pending() {
                return (existing.id.clone(), false);
            }
            // A terminal (denied/approved/expired) request with the same stable
            // id is REOPENED, not deduped: otherwise a denied worker could never
            // be re-requested and the delegation was permanently blocked.
            existing.state = ApprovalState::Requested;
            existing.created_at_ms = now_ms();
            existing.call = call.to_owned();
            existing.summary = summary.clone();
            existing.plan = None;
            existing.kind = ApprovalKind::Worker;
            drop(approvals);
            // Reopening is a fresh authority: the allowance for THIS id must
            // not carry over (other still-valid approvals are untouched).
            self.allow_next_worker_ids
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            self.journal_append(
                JournalEvent::Approval {
                    id: id.clone(),
                    call: call.to_owned(),
                    state: ApprovalState::Requested.label().to_owned(),
                    summary,
                    kind: ApprovalKind::Worker,
                    plan: None,
                    verification: None,
                    created_at_ms: now_ms(),
                },
                true,
            );
            self.journal_snapshot(true);
            self.refresh_approval_status();
            return (id, true);
        }
        approvals.push(ApprovalRecord::requested(
            id.clone(),
            call.to_owned(),
            summary.clone(),
        ));
        if approvals.len() > 64 {
            let excess = approvals.len() - 64;
            approvals.drain(0..excess);
        }
        drop(approvals);
        self.journal_append(
            JournalEvent::Approval {
                id: id.clone(),
                call: call.to_owned(),
                state: ApprovalState::Requested.label().to_owned(),
                summary,
                kind: ApprovalKind::Worker,
                plan: None,
                verification: None,
                created_at_ms: now_ms(),
            },
            true,
        );
        self.refresh_approval_status();
        (id, true)
    }

    fn decide_approval(&self, id: &str, approved: bool) -> Option<ApprovalRecord> {
        // A stale pending request is not decidable. Expire first so TUI and
        // CLI decisions share the same authority check.
        self.expire_stale_approvals();
        let mut approvals = self
            .approvals
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = approvals
            .iter_mut()
            .find(|record| record.id == id && record.is_pending())?;
        let state = if approved {
            ApprovalState::Approved
        } else {
            ApprovalState::Denied
        };
        record.state = state;
        let record = record.clone();
        drop(approvals);
        // A decision is the latest authority for THIS id: approve admits that
        // worker once, deny revokes any earlier allowance for it. Other ids
        // already approved stay valid (approve-all can grant several). Plan
        // approvals never touch the worker allowance.
        if record.kind == ApprovalKind::Worker {
            let mut allowance = self
                .allow_next_worker_ids
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            allowance.remove(&record.id);
            if approved {
                allowance.insert(record.id.clone());
            }
        }
        self.journal_append(
            JournalEvent::Approval {
                id: record.id.clone(),
                call: record.call.clone(),
                state: state.label().to_owned(),
                summary: record.summary.clone(),
                kind: record.kind,
                plan: record.plan.clone(),
                verification: record.verification.clone(),
                created_at_ms: record.created_at_ms,
            },
            true,
        );
        self.journal_snapshot(true);
        self.refresh_approval_status();
        Some(record)
    }

    /// Submit a plan for approval in plan mode. Returns the stable request id
    /// and whether this is a new request (a re-submission of the same text is
    /// deduped). The full plan is journaled so it survives resume.
    fn submit_plan(
        &self,
        call: &str,
        plan: &str,
        verification: Option<Verification>,
    ) -> (String, bool) {
        let id = plan_approval_id(plan, verification.as_ref());
        *self
            .submitted_plan
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(plan.to_owned());
        let mut approvals = self
            .approvals
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if approvals
            .iter()
            .any(|record| record.id == id && record.is_pending())
        {
            return (id, false);
        }
        let summary = plan_summary(plan);
        approvals.push(ApprovalRecord::plan_with_verification(
            id.clone(),
            call.to_owned(),
            summary.clone(),
            plan.to_owned(),
            verification.clone(),
        ));
        if approvals.len() > 64 {
            let excess = approvals.len() - 64;
            approvals.drain(0..excess);
        }
        drop(approvals);
        self.journal_append(
            JournalEvent::PlanSubmitted {
                id: id.clone(),
                plan: plan.to_owned(),
                verification: verification.clone(),
            },
            true,
        );
        self.journal_append(
            JournalEvent::Approval {
                id: id.clone(),
                call: call.to_owned(),
                state: ApprovalState::Requested.label().to_owned(),
                summary: summary.clone(),
                kind: ApprovalKind::Plan,
                plan: Some(plan.to_owned()),
                verification: verification.clone(),
                created_at_ms: now_ms(),
            },
            true,
        );
        // Do NOT snapshot here: the append-only PlanSubmitted/Approval events
        // are the durable record of the submission; a snapshot would fold them
        // away and make "submitted" unobservable to readers/inspection.
        self.refresh_approval_status();
        (id, true)
    }

    /// A plan was revised: clear the submitted copy so it is no longer shown
    /// as the pending plan on resume.
    fn revise_plan(&self) {
        *self
            .submitted_plan
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .active_contract
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .verification_passed_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .verified_call
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.journal_append(JournalEvent::PlanRevision, true);
    }

    /// Install the approved plan as the active execution contract in memory.
    /// Callers persist it via the next journal snapshot (`apply_mode` to Normal
    /// snapshots after this, and detached approval snapshots at decision end).
    fn set_active_contract(&self, contract: ExecutionContract) {
        *self
            .verification_passed_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .verified_call
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .active_contract
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(contract);
    }

    fn active_contract(&self) -> Option<ExecutionContract> {
        self.active_contract
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn declared_checks(&self) -> Vec<String> {
        self.declared_checks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the session's declared checks. Empty clears the floor.
    fn set_declared_checks(&self, checks: Vec<String>) {
        *self
            .declared_checks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = checks;
    }

    /// Arm the declared-check verification floor for one task boundary.
    ///
    /// Called once per prompt, not once per session: `complete_contract`
    /// retires a satisfied contract, so a session that runs several prompts
    /// would otherwise lose its floor after the first success. The baseline is
    /// recomputed every time for the same reason — carrying it forward would
    /// let task 1's evidence satisfy task 2's check.
    ///
    /// Returns `true` when a contract was installed.
    fn arm_declared_check_floor(&self, context: &PluginContext) -> bool {
        if self.mode() != Mode::Normal {
            // Foreman and plan have their own gates; plan installs the real
            // contract on approval. Only plain Normal needs this floor.
            return false;
        }
        // An approved plan's contract is strictly stronger: it carries the
        // plan the model must follow. Never let a `--check` line replace it.
        if self
            .active_contract()
            .is_some_and(|contract| !contract.plan.trim().is_empty())
        {
            return false;
        }
        let checks = self.declared_checks();
        if checks.is_empty() {
            return false;
        }
        // `set_active_contract` clears the pass markers, so a fresh prompt
        // starts a fresh gate. Re-arming mid-run would reset a satisfied
        // contract, which is why this is called at the prompt boundary only.
        self.set_active_contract(ExecutionContract {
            // A declared-check floor has no plan. The prompt-injection arm
            // tolerates that (see the `verification`-only branch).
            plan: String::new(),
            verification: Some(journal::Verification(checks)),
            approval_id: None,
            evidence_baseline: shell_call_baseline(context),
        });
        true
    }

    /// Take `--check` from the process environment, once.
    ///
    /// The CLI sets `PIR_CHECK` at startup (one process, one invocation),
    /// mirroring `PIR_PERF_PROFILE`. It is **consumed** here rather than read:
    /// a foreman worker is a child session in the same process, and it must
    /// not inherit the parent's check — a worker has its own job and gating it
    /// on the parent's command would stall it three nudges for nothing.
    fn take_declared_checks_from_env() -> Vec<String> {
        let Ok(raw) = std::env::var("PIR_CHECK") else {
            return Vec::new();
        };
        // Consume: later sessions in this process must not see it.
        std::env::remove_var("PIR_CHECK");
        raw.split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn verification_passed_id(&self) -> Option<String> {
        self.verification_passed_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn verified_call(&self) -> Option<String> {
        self.verified_call
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Retire a satisfied one-shot contract. The approval id and call id stay
    /// as audit markers; the plan stops being injected and gated immediately.
    fn complete_contract(&self, approval_id: &str, call: Option<String>) {
        *self
            .verification_passed_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(approval_id.to_owned());
        *self
            .verified_call
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = call.clone();
        *self
            .active_contract
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.gate_nudges.store(0, Ordering::SeqCst);
        self.journal_append(
            JournalEvent::ContractCompleted {
                approval_id: approval_id.to_owned(),
                call,
            },
            true,
        );
        self.journal_snapshot(true);
    }

    fn set_pending_transition(&self, message: String) {
        *self
            .pending_transition
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message);
    }

    fn take_pending_transition(&self) -> Option<String> {
        self.pending_transition
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn expire_stale_approvals(&self) {
        let now = now_ms();
        let mut expired = Vec::new();
        {
            let mut approvals = self
                .approvals
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for record in approvals.iter_mut() {
                if record.is_expired_at(now) {
                    record.state = ApprovalState::Expired;
                    expired.push(record.clone());
                }
            }
        }
        for record in expired {
            self.journal_append(
                JournalEvent::Approval {
                    id: record.id,
                    call: record.call,
                    state: ApprovalState::Expired.label().to_owned(),
                    summary: record.summary,
                    kind: record.kind,
                    plan: record.plan,
                    verification: record.verification,
                    created_at_ms: record.created_at_ms,
                },
                true,
            );
        }
        self.refresh_approval_status();
    }

    /// Consume the bound allowance only when the current delegation's approval
    /// id is one that was approved. A mismatched task never consumes another
    /// task's authorization; each approved id is consumed exactly once.
    fn take_allow_for(&self, id: &str) -> bool {
        let consumed = self
            .allow_next_worker_ids
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if consumed {
            // Persist the consumption before the worker runs: an in-memory-only
            // removal is re-armed by the old Approved snapshot after a crash.
            self.journal_append(JournalEvent::AllowanceConsumed { id: id.to_owned() }, true);
        }
        consumed
    }

    fn journal_state(&self) -> JournalState {
        JournalState {
            mode: self.mode(),
            phase: self.phase(),
            generation: self.worker_generation.load(Ordering::SeqCst),
            last_worker_call: self
                .last_worker_call_id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            reviewer: self
                .last_reviewer_call_id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            nudges: self.gate_nudges.load(Ordering::SeqCst),
            settings: self.settings(),
            approvals: self.approvals(),
            review_passed_generation: *self
                .review_passed_generation
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            allow_next_worker: !self
                .allow_next_worker_ids
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            // Both allowance fields must be deterministic: HashSet iteration
            // order is random, so take the first of the SORTED ids for the
            // legacy single-id mirror too.
            allow_next_worker_id: {
                let guard = self
                    .allow_next_worker_ids
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut ids: Vec<&String> = guard.iter().collect();
                ids.sort();
                ids.first().map(|id| (*id).clone())
            },
            allow_next_worker_ids: {
                let mut ids: Vec<String> = self
                    .allow_next_worker_ids
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .cloned()
                    .collect();
                ids.sort();
                ids
            },
            submitted_plan: self
                .submitted_plan
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            active_contract: self.active_contract(),
            verification_passed_id: self.verification_passed_id(),
            verified_call: self.verified_call(),
        }
    }

    fn open_journal(&self, context: &PluginContext) {
        if self
            .journal
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        let Some(session) = context.session() else {
            return;
        };
        let Some(path) = session.session_path() else {
            return;
        };
        match journal::ForemanJournal::open(path.as_ref()) {
            Ok(journal) => {
                *self
                    .journal
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(journal);
            }
            Err(error) => crate::ext_diag!(
                "[mode] failed to open foreman journal {}: {error}",
                path.display()
            ),
        }
    }

    fn close_journal(&self) {
        *self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn restore_journal(&self, state: JournalState) {
        self.set_mode(state.mode);
        self.set_settings(state.settings);
        self.set_phase(state.phase);
        self.worker_generation
            .store(state.generation, Ordering::SeqCst);
        *self
            .last_worker_call_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.last_worker_call;
        *self
            .last_reviewer_call_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.reviewer;
        self.gate_nudges.store(state.nudges, Ordering::SeqCst);
        *self
            .review_passed_generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.review_passed_generation;
        *self
            .approvals
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.approvals;
        *self
            .allow_next_worker_ids
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            state.allow_next_worker_ids.iter().cloned().collect();
        *self
            .submitted_plan
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.submitted_plan;
        *self
            .active_contract
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.active_contract;
        *self
            .verification_passed_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.verification_passed_id;
        *self
            .verified_call
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.verified_call;
    }

    fn refresh_approval_status(&self) {
        let pending_all = self.pending_approvals();
        let pending = pending_all.len();
        let has_plan = pending_all
            .iter()
            .any(|record| record.kind == ApprovalKind::Plan);
        let Some(context) = self.context() else {
            return;
        };
        if let Some(status) = context.status() {
            let text = (pending > 0).then(|| format!("APPROVALS: {pending}"));
            status.set_status(APPROVALS_STATUS_KEY, text.as_deref());
        }
        if let Some(ui) = context.ui() {
            if pending == 0 {
                ui.clear_widget(APPROVALS_WIDGET_KEY);
            } else {
                let label = if has_plan {
                    "PLAN APPROVALS"
                } else {
                    "FOREMAN APPROVALS"
                };
                ui.set_widget_static(
                    APPROVALS_WIDGET_KEY,
                    &[format!("{label}: {pending} pending — run /approvals")],
                    WidgetPlacement::AboveEditor,
                );
            }
        }
    }

    /// Clear all worker/reviewer tracking and the nudge budget.
    fn reset_gate(&self) {
        self.worker_generation.store(0, Ordering::SeqCst);
        self.gate_nudges.store(0, Ordering::SeqCst);
        *self
            .last_worker_call_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .review_passed_generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .last_reviewer_call_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        // Gate state belongs to the run that was interrupted by the mode
        // switch; clear it (the contract itself, if any, is handled by the
        // caller).
        *self
            .verification_passed_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .verified_call
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Record one `subagent` tool call. Worker calls start a new generation
    /// (invalidating any earlier PASS); reviewer calls remember their call id.
    fn record_subagent_agents(&self, call_id: Option<&str>, agents: &[String]) {
        let has_worker = agents.iter().any(|agent| agent != "reviewer");
        let has_reviewer = agents.iter().any(|agent| agent == "reviewer");
        if has_worker {
            self.worker_generation.fetch_add(1, Ordering::SeqCst);
            self.gate_nudges.store(0, Ordering::SeqCst);
            *self
                .last_worker_call_id
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                call_id.filter(|id| !id.is_empty()).map(str::to_owned);
            *self
                .review_passed_generation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *self
                .last_reviewer_call_id
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            self.set_phase(Phase::Delegating);
        }
        if has_reviewer && !has_worker {
            // A fresh reviewer delegation supersedes any earlier PASS for the
            // current generation: the new verdict (which may be FAIL/errored)
            // must be authoritative at the settle gate. Without this, a second
            // reviewer that fails is masked by the first one's PASS.
            //
            // A call that mixes worker and reviewer agents cannot verify
            // itself: both share one toolCallId, so a PASS could cover work
            // that ran concurrently (parallel) or after it (chain). Requiring
            // a separate reviewer call matches the documented foreman
            // contract in `FOREMAN_PROMPT`.
            *self
                .review_passed_generation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            if let Some(call_id) = call_id.filter(|call_id| !call_id.is_empty()) {
                *self
                    .last_reviewer_call_id
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(call_id.to_owned());
            }
            self.set_phase(Phase::Review);
        }
        let settings = self.settings();
        self.journal_append(
            JournalEvent::Delegation {
                call: call_id.unwrap_or_default().to_owned(),
                agents: agents.to_vec(),
                executor: settings.router.label().to_owned(),
                worktree: settings.worktree && has_worker,
            },
            false,
        );
        self.journal_maybe_compact();
    }

    /// Ask for one more turn when a settled run has not met its completion
    /// contract. Two independent gates share the same 3-nudge budget:
    ///   - Foreman: a worker exists but the reviewer result is missing/FAIL.
    ///   - Plan contract: an approved plan with verification criteria but no
    ///     successful verification tool result.
    ///
    /// A plain Normal run with no contract is never gated.
    fn gate_continuation(&self, payload: &Value) -> Option<Value> {
        let mode = self.mode();
        if mode == Mode::Foreman {
            return self.foreman_gate_continuation(payload);
        }
        if mode == Mode::Normal {
            return self.contract_gate_continuation(payload);
        }
        None
    }

    /// Foreman reviewer gate (unchanged semantics).
    fn foreman_gate_continuation(&self, payload: &Value) -> Option<Value> {
        if self.worker_generation.load(Ordering::SeqCst) == 0 {
            return None;
        }
        // A failed/aborted worker is never verifiable, even if a reviewer was
        // (incorrectly) asked; delegate a fix instead of accepting the run.
        let worker_call_id = self
            .last_worker_call_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if worker_call_id
            .as_deref()
            .is_some_and(|call_id| worker_failed(payload, call_id))
        {
            return self.gate_nudge(
                WORKER_FAILURE_GATE_MESSAGE,
                GATE_CUSTOM_TYPE,
                UNVERIFIED_MESSAGE,
            );
        }
        let generation = self.worker_generation.load(Ordering::SeqCst);
        if self
            .review_passed_generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            == Some(&generation)
        {
            self.set_phase(Phase::Verified);
            return None;
        }
        let reviewer_call_id = self
            .last_reviewer_call_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if reviewer_call_id
            .as_deref()
            .is_some_and(|call_id| reviewer_passed(payload, call_id))
        {
            self.set_phase(Phase::Verified);
            *self
                .review_passed_generation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(generation);
            if let Some(call_id) = reviewer_call_id {
                self.journal_append(
                    JournalEvent::Review {
                        call: call_id,
                        pass: true,
                    },
                    false,
                );
            }
            self.journal_append(
                JournalEvent::Phase {
                    phase: Phase::Verified.label().to_owned(),
                },
                false,
            );
            self.journal_snapshot(false);
            return None;
        }
        self.gate_nudge(GATE_MESSAGE, GATE_CUSTOM_TYPE, UNVERIFIED_MESSAGE)
    }

    /// Execution-contract verification gate for Normal runs carrying an
    /// approved plan. A contract without a verification clause keeps the
    /// previous "visible but ungated" behavior. A contract is one-shot: the
    /// first matching successful command retires it.
    fn contract_gate_continuation(&self, payload: &Value) -> Option<Value> {
        let contract = self.active_contract()?;
        let verification = contract.verification.as_ref()?;
        if verification.commands().is_empty() {
            return None;
        }
        let approval_id = contract
            .approval_id
            .clone()
            .unwrap_or_else(|| "plan".to_owned());
        // Without a durable boundary we cannot tell investigation-time
        // evidence from execution-time evidence. Fail closed: never accept a
        // command whose provenance we cannot prove.
        let Some(baseline) = contract.evidence_baseline.as_deref() else {
            return self.gate_nudge(
                VERIFICATION_GATE_MESSAGE,
                VERIFICATION_GATE_CUSTOM_TYPE,
                VERIFICATION_UNVERIFIED_MESSAGE,
            );
        };
        if let Some(call_id) =
            verification_evidence_passed(payload, verification.commands(), baseline)
        {
            self.set_phase(Phase::Verified);
            self.journal_append(
                JournalEvent::Verification {
                    approval_id: approval_id.clone(),
                    pass: true,
                    call: Some(call_id.clone()),
                },
                false,
            );
            self.journal_append(
                JournalEvent::Phase {
                    phase: Phase::Verified.label().to_owned(),
                },
                false,
            );
            self.complete_contract(&approval_id, Some(call_id));
            return None;
        }
        self.gate_nudge(
            VERIFICATION_GATE_MESSAGE,
            VERIFICATION_GATE_CUSTOM_TYPE,
            VERIFICATION_UNVERIFIED_MESSAGE,
        )
    }

    /// Shared bounded nudge: one budget, one custom-message draft per boundary,
    /// capped below the host's continuation cap.
    fn gate_nudge(
        &self,
        message: &str,
        custom_type: &str,
        unverified_message: &str,
    ) -> Option<Value> {
        if self.gate_nudges.load(Ordering::SeqCst) >= MAX_GATE_NUDGES {
            if self.phase() != Phase::Unverified {
                self.set_phase(Phase::Unverified);
                self.journal_append(
                    JournalEvent::Phase {
                        phase: Phase::Unverified.label().to_owned(),
                    },
                    true,
                );
                self.notify_level(NotifyLevel::Warning, unverified_message);
            }
            return None;
        }
        if self.gate_nudges.fetch_add(1, Ordering::SeqCst) == 0 {
            self.notify_level(NotifyLevel::Warning, message);
        }
        self.set_phase(Phase::Rework);
        let nudges = self.gate_nudges.load(Ordering::SeqCst);
        self.journal_append(
            JournalEvent::Gate {
                generation: self.worker_generation.load(Ordering::SeqCst),
                nudge: nudges,
                phase: Phase::Rework.label().to_owned(),
            },
            true,
        );
        Some(serde_json::json!({
            "continue": true,
            "entries": [{
                "type": "custom_message",
                "customType": custom_type,
                "content": message,
                "display": true,
            }],
        }))
    }

    /// Rebuild the worker/reviewer state from the session transcript (the
    /// sidecar stores the mode; the transcript is the gate journal).
    fn reconstruct_gate(&self, context: &PluginContext) {
        self.reset_gate();
        let mut generation = 0u32;
        let mut last_reviewer = None;
        let mut last_worker = None;
        let Some(session) = context.session() else {
            return;
        };
        let mut scanned = false;
        if let Some(entries) = session.entries() {
            if let Some(views) = entries.as_array() {
                for view in views {
                    if let Some(messages) = view.get("messages").and_then(Value::as_array) {
                        for message in messages {
                            scan_subagent_calls(
                                message,
                                &mut generation,
                                &mut last_reviewer,
                                &mut last_worker,
                            );
                            scanned = true;
                        }
                    } else if let Some(message) =
                        view.get("entry").and_then(|entry| entry.get("message"))
                    {
                        scan_subagent_calls(
                            message,
                            &mut generation,
                            &mut last_reviewer,
                            &mut last_worker,
                        );
                        scanned = true;
                    }
                }
            }
        }
        if generation == 0 && !scanned {
            // No host snapshot yet: read the session journal directly. The
            // entries snapshot is preferred because it already follows the
            // active branch; this fallback keeps resume working on a host
            // that has not pushed the snapshot.
            if let Some(path) = session.session_path() {
                if let Ok(journal) = std::fs::read_to_string(path.as_ref()) {
                    for line in journal.lines() {
                        let Ok(entry) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        if entry.get("type").and_then(Value::as_str) != Some("message") {
                            continue;
                        }
                        if let Some(message) = entry.get("message") {
                            scan_subagent_calls(
                                message,
                                &mut generation,
                                &mut last_reviewer,
                                &mut last_worker,
                            );
                        }
                    }
                }
            }
        }
        self.worker_generation.store(generation, Ordering::SeqCst);
        *self
            .last_worker_call_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = last_worker;
        *self
            .last_reviewer_call_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = last_reviewer;
        self.set_phase(if generation == 0 {
            Phase::Planning
        } else if self
            .last_reviewer_call_id
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            Phase::Review
        } else {
            Phase::Delegating
        });
    }

    /// Restore the persisted mode and, for foreman, the durable gate state.
    fn restore_from_context(&self, context: &PluginContext) {
        self.close_journal();
        let (mode, settings) = persisted_mode_settings(context);
        let journaled = context
            .session()
            .and_then(|session| session.session_path())
            .and_then(|path| journal::read_state(path.as_ref()));
        if let Some(state) = journaled {
            self.restore_journal(state);
        } else {
            self.set_mode(mode);
            self.set_settings(settings);
            match mode {
                Mode::Normal => {
                    self.reset_gate();
                    self.set_phase(Phase::Idle);
                }
                Mode::Plan => {
                    self.reset_gate();
                    self.set_phase(Phase::Planning);
                }
                Mode::Foreman => self.reconstruct_gate(context),
            }
        }
        if self.mode() != Mode::Normal || self.active_contract().is_some() {
            self.open_journal(context);
        } else {
            self.clear_approvals();
        }
        self.expire_stale_approvals();
        self.refresh_status();
        self.refresh_approval_status();
        if let Some(message) = self.resume_transition_message() {
            self.set_pending_transition(message);
        }
    }

    /// A resumed model must not infer its state from old history. Tell it
    /// explicitly which mode is active and whether the approved contract is
    /// still open. Plain Normal sessions have nothing to announce.
    fn resume_transition_message(&self) -> Option<String> {
        match self.mode() {
            Mode::Foreman => Some(
                "SESSION RESUMED in foreman mode. Foreman rules are active: delegate \
                 mutations via subagent and obtain an independent reviewer PASS before \
                 finishing."
                    .to_owned(),
            ),
            Mode::Plan => Some(
                "SESSION RESUMED in plan mode. Stay read-only; submit a plan for approval \
                 when investigation is complete."
                    .to_owned(),
            ),
            Mode::Normal => {
                let contract = self.active_contract()?;
                if contract.plan.trim().is_empty() {
                    return None;
                }
                Some(
                    "SESSION RESUMED in normal mode with an approved plan still awaiting \
                     verification. Continue that plan and run its verification command \
                     before reporting done."
                        .to_owned(),
                )
            }
        }
    }

    fn context(&self) -> Option<PluginContext> {
        self.context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_context(&self, context: PluginContext) {
        *self
            .context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context);
    }

    fn session_identity(&self) -> (Option<String>, Option<PathBuf>) {
        (
            self.session_id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            self.session_path
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

    fn set_session_identity(&self, context: &PluginContext) {
        let (id, path) = context
            .session()
            .map(|session| {
                (
                    session.session_id().map(|id| id.to_string()),
                    session
                        .session_path()
                        .map(|path| path.as_ref().to_path_buf()),
                )
            })
            .unwrap_or((None, None));
        *self
            .session_id
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = id;
        *self
            .session_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }

    /// Publish the mode and phase cells. A host without a status service
    /// simply observes a no-op; the mode itself still works.
    fn refresh_status(&self) {
        let Some(context) = self.context() else {
            return;
        };
        let Some(status) = context.status() else {
            return;
        };
        let mode_text = match self.mode() {
            Mode::Normal => None,
            Mode::Foreman => Some(FOREMAN_STATUS),
            Mode::Plan => Some(PLAN_STATUS),
        };
        status.set_status(STATUS_KEY, mode_text);
        let phase_text = match self.mode() {
            // An approved plan keeps executing in Normal mode under a
            // verification contract; its gate phase must stay visible even
            // though the mode badge is gone.
            Mode::Normal if self.active_contract().is_some() && self.phase() != Phase::Idle => {
                Some(format!("PHASE: {}", self.phase().label()))
            }
            Mode::Normal => None,
            Mode::Foreman | Mode::Plan => Some(format!("PHASE: {}", self.phase().label())),
        };
        status.set_status(PHASE_STATUS_KEY, phase_text.as_deref());
    }

    fn notify(&self, message: &str) {
        self.notify_level(NotifyLevel::Info, message);
    }

    fn notify_level(&self, level: NotifyLevel, message: &str) {
        let Some(context) = self.context() else {
            return;
        };
        let Some(ui) = context.ui() else {
            return;
        };
        ui.notify(level, message);
    }
}

/// The native `mode` builtin.
pub struct ModePlugin {
    info: PluginInfo,
    state: Arc<ModeState>,
}

impl ModePlugin {
    fn new() -> Self {
        let mut info = PluginInfo::builtin(PLUGIN_NAME);
        info.replaceable = builtin_extension_def(PLUGIN_NAME).is_some_and(|def| def.replaceable);
        Self {
            info,
            state: Arc::new(ModeState::new()),
        }
    }
}

impl AgentPlugin for ModePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        submit_plan::register_submit_plan_tool(ctx)?;
        let state = Arc::clone(&self.state);
        let handler: CommandHandler = Arc::new(move |args: &str| {
            let state = Arc::clone(&state);
            let args = args.to_owned();
            Box::pin(async move { handle_mode_command(&state, &args) })
        });
        ctx.register_command(
            COMMAND_NAME,
            Some(
                "Switch session mode: /mode foreman | /mode plan | /mode normal [--force].                  Foreman routes mutations to workers and requires a reviewer; plan is read-only                  until a submitted plan is approved. Run /approvals to decide pending approvals,                  /foreman to tune settings. Use --force to leave a busy session in an emergency.",
            ),
            Some(CommandBackend::Rust(handler)),
        );
        let settings_state = Arc::clone(&self.state);
        let settings_handler: CommandHandler = Arc::new(move |args: &str| {
            let state = Arc::clone(&settings_state);
            let args = args.to_owned();
            Box::pin(async move { handle_foreman_command(&state, &args) })
        });
        ctx.register_command(
            FOREMAN_COMMAND_NAME,
            Some(
                "Foreman settings: /foreman [worktree on|off] [router inline|process] \
                 [approvals auto|manual] [status|clean]",
            ),
            Some(CommandBackend::Rust(settings_handler)),
        );
        let approvals_state = Arc::clone(&self.state);
        let approvals_handler: CommandHandler = Arc::new(move |args: &str| {
            let state = Arc::clone(&approvals_state);
            let args = args.to_owned();
            Box::pin(async move { handle_approvals_command(&state, &args).await })
        });
        ctx.register_command(
            APPROVALS_COMMAND_NAME,
            Some("Review pending foreman worker and plan approvals"),
            Some(CommandBackend::Rust(approvals_handler)),
        );
        Ok(())
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec![
            "session_start".to_owned(),
            "before_agent_start".to_owned(),
            "tool_call".to_owned(),
            "agent_before_settle".to_owned(),
        ])
    }

    fn handles_event_now(&self, event: &str) -> bool {
        // The settle boundary serializes the whole transcript before the
        // handlers run. Only foreman and Normal runs carrying an approved
        // execution contract have a settle handler; ordinary normal/plan
        // sessions keep the no-handler fast path.
        event != "agent_before_settle"
            || self.state.mode() == Mode::Foreman
            || (self.state.mode() == Mode::Normal && self.state.active_contract().is_some())
    }

    fn hidden_tool_declarations(&self) -> Vec<String> {
        // A proactive version of the `tool_call` gate: foreman/plan never see
        // the tools they cannot use, so a well-behaved model never wastes a
        // turn on a call that would be blocked. The hard gate stays as the
        // execution fence for a model that guesses a hidden name.
        let mode = self.state.mode();
        let Some(context) = self.state.context() else {
            return Vec::new();
        };
        if !context.is_active() {
            return Vec::new();
        }
        let Some(session) = context.session() else {
            return Vec::new();
        };
        session
            .all_tools()
            .iter()
            // A tool is hidden when it is NOT allowed for the current mode.
            // `submit_plan` is only allowed in plan mode; the rest of
            // MODE_ALLOWED_TOOLS are allowed in both foreman and plan but not
            // in normal (where no mode gating applies to real sessions).
            .filter(|tool| {
                let name = tool.name.as_str();
                match mode {
                    // Normal sessions gate nothing; only the plan-exit tool
                    // (registered in every session) must stay hidden.
                    Mode::Normal => name == submit_plan::SUBMIT_PLAN_TOOL,
                    Mode::Foreman => {
                        !MODE_ALLOWED_TOOLS.contains(&name) || name == submit_plan::SUBMIT_PLAN_TOOL
                    }
                    Mode::Plan => !MODE_ALLOWED_TOOLS.contains(&name),
                }
            })
            .map(|tool| tool.name.clone())
            .collect()
    }

    fn on_event<'a>(&'a self, mut event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            match event.name.as_ref() {
                "session_start" => {
                    // Restore this session's persisted mode. A new session
                    // has no sidecar and starts normal; a resumed/reloaded
                    // session restores foreman/plan and reconstructs the gate
                    // from its transcript.
                    let context = self.state.context();
                    if let Some(context) = context.as_ref().filter(|context| context.is_active()) {
                        self.state.restore_from_context(context);
                    } else {
                        self.state.close_journal();
                        self.state.set_mode(Mode::Normal);
                        self.state.reset_gate();
                        self.state.clear_approvals();
                        self.state.set_phase(Phase::Idle);
                    }
                    // Declared checks: `--check` on this invocation wins, else the session's
                    // own persisted list. A resumed task keeps the check it was
                    // started with, which is the whole point of persisting it.
                    let from_env = ModeState::take_declared_checks_from_env();
                    if !from_env.is_empty() {
                        self.state.set_declared_checks(from_env.clone());
                        if let Some(context) =
                            context.as_ref().filter(|context| context.is_active())
                        {
                            persist_declared_checks(context, &from_env);
                        }
                    } else if let Some(context) =
                        context.as_ref().filter(|context| context.is_active())
                    {
                        self.state
                            .set_declared_checks(persisted_declared_checks(context));
                    }
                    self.state.refresh_status();
                    PluginEventOutcome::None
                }
                "before_agent_start" => {
                    // Every prompt is a task boundary for the declared-check
                    // floor: `complete_contract` retires a satisfied contract,
                    // so a multi-prompt session would lose the floor after the
                    // first success without re-arming here.
                    if let Some(context) = self.state.context() {
                        if context.is_active() {
                            self.state.arm_declared_check_floor(&context);
                        }
                    }
                    let transition = self.state.take_pending_transition();
                    let outcome = match self.state.mode() {
                        Mode::Foreman => {
                            let prompt = format!(
                                "{FOREMAN_PROMPT}{}",
                                self.state.settings().prompt_addendum()
                            );
                            mode_prompt(event, FOREMAN_SECTION, &prompt)
                        }
                        Mode::Plan => mode_prompt(event, PLAN_SECTION, PLAN_PROMPT),
                        Mode::Normal => match self.state.active_contract() {
                            // A declared-check floor has an empty plan. The
                            // model still has to be told which command
                            // satisfies the gate (matching is verbatim), so
                            // the verification list is injected on its own.
                            Some(contract) if contract.plan.trim().is_empty() => {
                                match contract.verification.as_ref() {
                                    Some(verification) if !verification.display().is_empty() => {
                                        let mut prompt = String::from(DECLARED_CHECK_PROMPT_HEAD);
                                        prompt.push_str(
                                            "Verification criteria: run one of these \
                                                         commands EXACTLY (no rewriting):\n",
                                        );
                                        prompt.push_str(verification.display().trim());
                                        prompt.push_str(DECLARED_CHECK_PROMPT_TAIL);
                                        mode_prompt(event, EXECUTION_CONTRACT_SECTION, &prompt)
                                    }
                                    _ => PluginEventOutcome::None,
                                }
                            }
                            Some(contract) => {
                                let mut prompt = String::from(EXECUTION_CONTRACT_PROMPT_HEAD);
                                prompt.push_str("Approval: ");
                                prompt.push_str(
                                    contract.approval_id.as_deref().unwrap_or("approved"),
                                );
                                prompt.push('\n');
                                prompt.push_str(&plan_summary(contract.plan.trim()));
                                prompt.push_str(
                                    "\nThe full plan is in the approval message earlier in this \
                                     conversation. Bounded excerpt for resume/context safety:\n",
                                );
                                prompt.push_str(&truncate_chars(
                                    contract.plan.trim(),
                                    MAX_CONTRACT_PLAN_PROMPT_CHARS,
                                ));
                                if let Some(verification) = contract.verification.as_ref() {
                                    let text = verification.display();
                                    if !text.trim().is_empty() {
                                        prompt.push_str(
                                            "\n\nVerification criteria: run one of these commands \
                                             EXACTLY (no rewriting):\n",
                                        );
                                        prompt.push_str(text.trim());
                                    }
                                }
                                prompt.push_str(EXECUTION_CONTRACT_PROMPT_TAIL);
                                mode_prompt(event, EXECUTION_CONTRACT_SECTION, &prompt)
                            }
                            _ => PluginEventOutcome::None,
                        },
                    };
                    with_transition(outcome, transition)
                }
                "tool_call" => {
                    let mode = self.state.mode();
                    let Some(tool_name) = event.tool_name().map(str::to_owned) else {
                        return PluginEventOutcome::None;
                    };
                    let tool = tool_name.as_str();
                    // `submit_plan` is registered in every session, so it must
                    // be blocked even in normal/foreman mode as defense in
                    // depth (the model cannot normally see it there).
                    if tool == submit_plan::SUBMIT_PLAN_TOOL && mode != Mode::Plan {
                        return PluginEventOutcome::ToolCall {
                            block: true,
                            reason: Some(
                                "submit_plan is only available in plan mode (/mode plan)."
                                    .to_owned(),
                            ),
                            terminate: false,
                        };
                    }
                    if mode == Mode::Normal {
                        // A contract-backed Normal run may still delegate to a
                        // worker. Track worker generations so new output
                        // invalidates earlier verification evidence (the same
                        // rule the foreman gate uses), without applying the
                        // foreman read-only fence to ordinary Normal sessions.
                        if self.state.active_contract().is_some() && tool == "subagent" {
                            let call_id =
                                event.str_field("toolCallId").unwrap_or_default().to_owned();
                            let agents = event
                                .payload
                                .get("input")
                                .map(|input| subagent_agents(input))
                                .unwrap_or_default();
                            let agents: Vec<String> =
                                agents.into_iter().map(str::to_owned).collect();
                            if agents.iter().any(|agent| agent != "reviewer") {
                                self.state.record_subagent_agents(Some(&call_id), &agents);
                            }
                        }
                        return PluginEventOutcome::None;
                    }
                    // Plan-mode exit surface: submit the plan for approval.
                    // The tool call is blocked (never executed) while the
                    // approval queue takes over; the same select/follow-up
                    // machinery as foreman worker approvals drives the result.
                    if mode == Mode::Plan && tool == submit_plan::SUBMIT_PLAN_TOOL {
                        let call_id = event.str_field("toolCallId").unwrap_or_default().to_owned();
                        let plan = event
                            .payload
                            .get("input")
                            .and_then(|input| input.get("plan"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_owned();
                        if plan.is_empty() {
                            return PluginEventOutcome::ToolCall {
                                block: true,
                                reason: Some(
                                    "submit_plan requires a non-empty `plan` string with the \
                                     ordered steps, files, risks and verification."
                                        .to_owned(),
                                ),
                                terminate: false,
                            };
                        }
                        let verification = parse_verification(
                            event
                                .payload
                                .get("input")
                                .and_then(|input| input.get("verification")),
                        );
                        if verification.is_none() {
                            return PluginEventOutcome::ToolCall {
                                block: true,
                                reason: Some(
                                    "submit_plan requires `verification` as a non-empty array of \
                                     shell command strings that will be run verbatim (for \
                                     example [\"cargo test\"]). Free-form prose is not accepted."
                                        .to_owned(),
                                ),
                                terminate: false,
                            };
                        }
                        let (id, created) = self.state.submit_plan(&call_id, &plan, verification);
                        if created {
                            self.state.notify_level(
                                NotifyLevel::Warning,
                                &format!(
                                    "PLAN APPROVAL: plan `{id}` is waiting. Run /approvals to \
                                     approve or request changes. End your turn now; the decision \
                                     arrives as a follow-up (no sleeping/polling)."
                                ),
                            );
                        }
                        return PluginEventOutcome::ToolCall {
                            block: true,
                            reason: Some(plan_approval_block_reason(&id)),
                            terminate: false,
                        };
                    }
                    if !MODE_ALLOWED_TOOLS.contains(&tool) {
                        return PluginEventOutcome::ToolCall {
                            block: true,
                            reason: Some(match mode {
                                Mode::Plan => format!(
                                    "Plan mode: direct tool `{tool}` is blocked. Investigate with \
                                     read-only tools and produce a plan."
                                ),
                                _ => format!(
                                    "Foreman mode: direct tool `{tool}` is blocked. Delegate this \
                                     work to a subagent, or use a read-only tool."
                                ),
                            }),
                            terminate: false,
                        };
                    }
                    if mode == Mode::Plan && tool == "subagent" {
                        let agents = event
                            .payload
                            .get("input")
                            .map(subagent_agents)
                            .unwrap_or_default();
                        if let Some(agent) = agents
                            .iter()
                            .find(|agent| !matches!(**agent, "scout" | "planner" | "reviewer"))
                        {
                            return PluginEventOutcome::ToolCall {
                                block: true,
                                reason: Some(format!(
                                    "Plan mode: subagent `{agent}` is not read-only. Use scout, \
                                     planner, or reviewer, and produce a plan instead of executing."
                                )),
                                terminate: false,
                            };
                        }
                        // HARD read-only fence: the built-in scout/reviewer carry
                        // `bash` in their own frontmatter, so merely allowing
                        // their NAMES is not enough. Override every child task's
                        // tool allowlist with a read-only set so even a model
                        // that tries `bash` inside a subagent cannot mutate.
                        if let Some(input) = event.payload.get_mut("input") {
                            force_readonly_subagent_tools(input);
                        }
                    }
                    if mode == Mode::Foreman && tool == "subagent" {
                        let agents: Vec<String> = event
                            .payload
                            .get("input")
                            .map(subagent_agents)
                            .unwrap_or_default()
                            .into_iter()
                            .map(str::to_owned)
                            .collect();
                        let has_worker = agents.iter().any(|agent| agent != "reviewer");
                        let settings = self.state.settings();
                        let call_id = event.str_field("toolCallId").unwrap_or_default().to_owned();
                        let worker_input = event.payload.get("input").unwrap_or(&Value::Null);
                        let worker_approval_id = approval_id(worker_input);
                        if has_worker
                            && settings.approvals == ApprovalMode::Manual
                            && !self.state.take_allow_for(&worker_approval_id)
                        {
                            let (id, created) = self.state.request_approval(&call_id, worker_input);
                            if created {
                                self.state.notify_level(
                                    NotifyLevel::Warning,
                                    &format!(
                                        "FOREMAN APPROVAL: worker delegation `{id}` is waiting. \
                                         Run /approvals to approve or deny it. End your turn now; \
                                         the decision arrives as a follow-up (no sleeping/polling)."
                                    ),
                                );
                            }
                            return PluginEventOutcome::ToolCall {
                                block: true,
                                reason: Some(approval_block_reason(&id, settings)),
                                terminate: false,
                            };
                        }
                        if let Some(input) = event.tool_call_input_mut() {
                            apply_foreman_routing(input, settings, has_worker);
                            if !has_worker {
                                // Reviewer-only calls skip approval entirely,
                                // so they may never carry write-capable tools.
                                force_reviewer_subagent_tools(input);
                            }
                        }
                        self.state.record_subagent_agents(Some(&call_id), &agents);
                    }
                    PluginEventOutcome::None
                }
                "agent_before_settle" => match self.state.gate_continuation(event.payload) {
                    Some(result) => PluginEventOutcome::Generic(result),
                    None => PluginEventOutcome::None,
                },
                _ => PluginEventOutcome::None,
            }
        })
    }

    fn on_context(&self, ctx: &PluginContext) {
        let identity = self.state.session_identity();
        let next_identity = ctx
            .session()
            .map(|session| {
                (
                    session.session_id().map(|id| id.to_string()),
                    session
                        .session_path()
                        .map(|path| path.as_ref().to_path_buf()),
                )
            })
            .unwrap_or((None, None));
        self.state.set_context(ctx.clone());
        // A first bind or a bind to another session reloads that session's
        // mode. A same-session rebind keeps the live gate state in memory.
        if (identity != next_identity || self.state.mode() == Mode::Normal) && ctx.is_active() {
            self.state.restore_from_context(ctx);
        }
        self.state.set_session_identity(ctx);
        self.state.refresh_status();
        self.state.refresh_approval_status();
    }

    fn on_reload(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            if let Some(context) = self.state.context() {
                if context.is_active() {
                    self.state.restore_from_context(&context);
                } else {
                    self.state.close_journal();
                    self.state.set_mode(Mode::Normal);
                    self.state.reset_gate();
                    self.state.clear_approvals();
                    self.state.set_phase(Phase::Idle);
                }
            } else {
                self.state.close_journal();
                self.state.set_mode(Mode::Normal);
                self.state.reset_gate();
                self.state.clear_approvals();
                self.state.set_phase(Phase::Idle);
            }
            self.state.refresh_status();
            Ok(())
        })
    }

    fn on_shutdown(&self) -> PluginFuture<'_, anyhow::Result<()>> {
        Box::pin(async {
            // The sidecar/journal stay for a later resume; only the live
            // in-memory state is cleared.
            self.state.journal_snapshot(true);
            self.state.close_journal();
            self.state.set_mode(Mode::Normal);
            self.state.reset_gate();
            self.state.clear_approvals();
            self.state.set_phase(Phase::Idle);
            self.state.refresh_status();
            Ok(())
        })
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Factory for [`super::BUILTIN_EXTENSION_FACTORIES`].
pub fn new_mode_plugin() -> Arc<dyn AgentPlugin> {
    Arc::new(ModePlugin::new())
}

fn handle_mode_command(state: &ModeState, args: &str) -> CommandOutcome {
    let trimmed = args.trim().to_ascii_lowercase();
    let force = trimmed.ends_with(" --force");
    let base = if force {
        trimmed.trim_end_matches(" --force").trim()
    } else {
        trimmed.as_str()
    };
    match base {
        "" => CommandOutcome {
            ok: true,
            error: Some(format!(
                "Current mode: {}. Switch with /mode foreman | /mode plan | /mode normal [--force].",
                state.mode().label()
            )),
        },
        "foreman" => switch_mode(state, Mode::Foreman, false),
        "normal" => switch_mode(state, Mode::Normal, force),
        "plan" => switch_mode(state, Mode::Plan, false),
        _ => CommandOutcome {
            ok: false,
            error: Some(USAGE_MESSAGE.to_owned()),
        },
    }
}

fn switch_mode(state: &ModeState, target: Mode, force: bool) -> CommandOutcome {
    if state.mode() == target {
        return CommandOutcome {
            ok: true,
            error: Some(format!("Already in {} mode.", target.label())),
        };
    }

    if let Some(context) = state.context() {
        if !context.is_active() {
            return CommandOutcome {
                ok: false,
                error: Some("Session changed while switching modes; run /mode again.".to_owned()),
            };
        }
        if let Some(session) = context.session() {
            if !session.is_idle() && !(force && target == Mode::Normal) {
                return CommandOutcome {
                    ok: false,
                    error: Some(BUSY_MESSAGE.to_owned()),
                };
            }
            if target == Mode::Foreman {
                let active = session.active_tools();
                // An empty snapshot means this host does not expose active
                // tools; do not reject a mode the host may support.
                if !active.is_empty() && !active.iter().any(|name| name.as_ref() == "subagent") {
                    return CommandOutcome {
                        ok: false,
                        error: Some(
                            "Foreman mode requires the `subagent` tool to be active. \
                             Enable it or remove the tool restriction first."
                                .to_owned(),
                        ),
                    };
                }
            }
        }
    }

    state.reset_gate();
    let discarded = state.pending_approvals().len();
    state.clear_approvals();
    let mut notice = mode_switch_message(target).to_owned();
    if discarded > 0 {
        notice = format!("Discarded {discarded} pending approval(s). {notice}");
    }
    apply_mode_with_notice(state, target, Some(&notice));
    // Tell the model the rules changed. Without this, a probabilistic model
    // can keep following the previous mode's instructions that are still in
    // the transcript even though the system prompt already changed.
    state.set_pending_transition(format!(
        "MODE CHANGED to {}. This supersedes every earlier mode instruction in this conversation.\n{}",
        target.label(),
        notice
    ));
    CommandOutcome {
        ok: true,
        error: Some(notice),
    }
}

/// Perform the mode transition's state mutation and persistence without the
/// user-facing guardrails. Used by the plan-approval flow, which switches to
/// Normal while the submit-plan run is still active (the busy check would
/// otherwise reject the transition and the execution follow-up would run in
/// plan mode).
fn apply_mode_with_notice(state: &ModeState, target: Mode, notice: Option<&str>) {
    state.reset_gate();
    state.set_mode(target);
    state.set_phase(match target {
        Mode::Normal => Phase::Idle,
        Mode::Foreman | Mode::Plan => Phase::Planning,
    });
    if let Some(context) = state.context() {
        if context.is_active() {
            persist_mode(&context, target, state.settings());
            if target != Mode::Normal {
                state.open_journal(&context);
                state.journal_append(
                    JournalEvent::Mode {
                        mode: target.label().to_owned(),
                        settings: state.settings(),
                    },
                    true,
                );
                state.journal_snapshot(true);
            } else if state.active_contract().is_some() {
                // An approved plan keeps a verification contract alive in
                // Normal mode. Keep the journal open so the verification /
                // gate-nudge events persist across interrupts and resume.
                state.journal_snapshot(true);
            } else {
                state.journal_snapshot(true);
                state.close_journal();
            }
        }
    }
    state.refresh_status();
    state.refresh_approval_status();
    state.notify(notice.unwrap_or_else(|| mode_switch_message(target)));
}

fn mode_switch_message(target: Mode) -> &'static str {
    match target {
        Mode::Foreman => {
            "Foreman mode enabled. Direct mutations are blocked; delegate via subagent."
        }
        Mode::Normal => "Normal mode restored.",
        Mode::Plan => "Plan mode enabled. Mutations are blocked; investigate and produce a plan.",
    }
}

/// Add the active mode contract to this run's prompt. Prefer the shared
/// `systemPromptOptions.sections` object (the same seam the memory builtin
/// uses) so other handlers' prompt changes survive; fall back to appending to
/// the already-rendered prompt only for hosts without options.
fn mode_prompt(event: PluginEvent<'_>, section: &str, prompt: &str) -> PluginEventOutcome {
    if let Some(options) = event
        .payload
        .get_mut("systemPromptOptions")
        .and_then(Value::as_object_mut)
    {
        let sections = options
            .entry("sections")
            .or_insert_with(|| Value::Object(Map::new()));
        if !sections.is_object() {
            *sections = Value::Object(Map::new());
        }
        if let Some(sections) = sections.as_object_mut() {
            sections.insert(section.to_owned(), Value::String(prompt.to_owned()));
            return PluginEventOutcome::None;
        }
    }

    let base = event
        .payload
        .get("systemPrompt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let prompt = if base.trim().is_empty() {
        prompt.to_owned()
    } else {
        format!("{base}\n\n{prompt}")
    };
    PluginEventOutcome::BeforeAgentStart {
        system_prompt: Some(prompt),
        message: None,
        messages: Vec::new(),
    }
}

/// Attach a one-shot mode-transition note to the turn's prompt outcome. The
/// note is a custom message, not a synthetic user prompt: it rides the
/// `before_agent_start` injection channel and cannot start a new run.
fn with_transition(outcome: PluginEventOutcome, transition: Option<String>) -> PluginEventOutcome {
    let Some(text) = transition else {
        return outcome;
    };
    let message = serde_json::json!({
        "type": "custom_message",
        "customType": "mode_transition",
        "content": [{ "type": "text", "text": text }],
        // The UI already shows a mode notice; the model needs the text, the
        // transcript does not need another visible bubble on every resume.
        "display": false,
    });
    match outcome {
        PluginEventOutcome::BeforeAgentStart {
            system_prompt,
            messages,
            ..
        } => {
            let mut messages = messages;
            messages.push(message.clone());
            PluginEventOutcome::BeforeAgentStart {
                system_prompt,
                message: Some(message),
                messages,
            }
        }
        PluginEventOutcome::None => PluginEventOutcome::BeforeAgentStart {
            system_prompt: None,
            message: Some(message.clone()),
            messages: vec![message],
        },
        other => other,
    }
}

/// Read the persisted mode/settings from `<session>.mode.json`. Missing or
/// invalid state loads as normal/default.
fn persisted_mode_settings(context: &PluginContext) -> (Mode, ModeSettings) {
    let Some(path) = sidecar_path(context) else {
        return (Mode::Normal, ModeSettings::default());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (Mode::Normal, ModeSettings::default());
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (Mode::Normal, ModeSettings::default());
    };
    let mode = match value.get("mode").and_then(Value::as_str) {
        Some("foreman") => Mode::Foreman,
        Some("plan") => Mode::Plan,
        _ => Mode::Normal,
    };
    let settings = value
        .get("settings")
        .cloned()
        .and_then(|settings| serde_json::from_value(settings).ok())
        .unwrap_or_default();
    (mode, settings)
}

pub fn persist_mode(context: &PluginContext, mode: Mode, settings: ModeSettings) {
    let Some(path) = sidecar_path(context) else {
        return;
    };
    persist_mode_path(&path, mode, settings);
}

/// Sidecar for the declared verification checks (`--check`).
///
/// Deliberately a separate file from `<session>.mode.json`: the mode sidecar
/// is rewritten wholesale by every mode transition, so a field stored there
/// would be lost the first time the user switches mode. Two files, two
/// owners, no read-modify-write dance.
const DECLARED_CHECKS_SUFFIX: &str = ".checks.json";

fn declared_checks_path_for(session_path: &Path) -> PathBuf {
    let mut os = session_path.as_os_str().to_owned();
    os.push(DECLARED_CHECKS_SUFFIX);
    PathBuf::from(os)
}

fn persisted_declared_checks(context: &PluginContext) -> Vec<String> {
    let Some(path) = context
        .session()
        .and_then(|session| session.session_path())
        .map(|path| declared_checks_path_for(path.as_ref()))
    else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("checks")
        .and_then(Value::as_array)
        .map(|checks| {
            checks
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn persist_declared_checks(context: &PluginContext, checks: &[String]) {
    let Some(path) = context
        .session()
        .and_then(|session| session.session_path())
        .map(|path| declared_checks_path_for(path.as_ref()))
    else {
        return;
    };
    let value = serde_json::json!({ "version": 1, "checks": checks });
    if let Err(error) = journal::write_atomic_json(&path, &value) {
        crate::ext_diag!(
            "[mode] failed to persist declared checks to {}: {error}",
            path.display()
        );
    }
}

pub fn persist_mode_path(path: &Path, mode: Mode, settings: ModeSettings) {
    let value = serde_json::json!({
        "version": 2,
        "mode": mode.label(),
        "settings": settings,
    });
    if let Err(error) = journal::write_atomic_json(path, &value) {
        crate::ext_diag!(
            "[mode] failed to persist mode to {}: {error}",
            path.display()
        );
    }
}

pub fn sidecar_path(context: &PluginContext) -> Option<PathBuf> {
    let path = context.session()?.session_path()?;
    Some(sidecar_path_for(path.as_ref()))
}

pub fn sidecar_path_for(session_path: &Path) -> PathBuf {
    let mut os = session_path.as_os_str().to_owned();
    os.push(MODE_SIDECAR_SUFFIX);
    PathBuf::from(os)
}

/// Find the first successful shell-command result that can satisfy the active
/// contract, returning its tool-call id. This is deliberately domain-neutral:
/// it checks tool facts only (call/result pairing, tool name, error flag, exit
/// code), never the command's meaning or an artifact format. It mirrors
/// `compaction::evidence`'s rules.
///
/// `baseline` holds call-id hashes that already existed when the plan was
/// approved. Those results can never satisfy the contract, so the gate does
/// not depend on a `PLAN APPROVED` text marker that compaction may erase.
fn verification_evidence_passed(
    payload: &Value,
    declared: &[String],
    baseline: &[u64],
) -> Option<String> {
    let messages = payload
        .pointer("/context/llmMessages")
        .and_then(Value::as_array)?;
    // Map tool call id -> (shell tool name, declared command argument). Mirrors
    // compaction::evidence so the strict path can compare exact commands.
    // While scanning, remember the last call that could change the workspace:
    // evidence only counts if it runs after every mutation, or a model could
    // verify and then edit the delivered code.
    let mut shell_calls: HashMap<String, Option<String>> = HashMap::new();
    let mut last_mutation: Option<usize> = None;
    for (index, message) in messages.iter().enumerate() {
        let Some(assistant) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in assistant {
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(name, "bash" | "shell" | "powershell" | "sh") {
                let command = block
                    .get("arguments")
                    .and_then(|args| args.get("command"))
                    .and_then(Value::as_str);
                let is_declared = command
                    .is_some_and(|command| declared.iter().any(|d| d.trim() == command.trim()));
                if !is_declared {
                    last_mutation = Some(index);
                }
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    shell_calls.insert(id.to_owned(), command.map(str::to_owned));
                }
                continue;
            }
            if matches!(name, "write" | "edit" | "apply_patch") {
                last_mutation = Some(index);
                continue;
            }
            if name == "subagent" {
                let agents = block
                    .get("arguments")
                    .map(subagent_agents)
                    .unwrap_or_default();
                if agents.iter().any(|agent| *agent != "reviewer") {
                    last_mutation = Some(index);
                }
            }
        }
    }
    messages.iter().enumerate().find_map(|(position, result)| {
        if last_mutation.is_some_and(|mutation| mutation > position) {
            return None;
        }
        let call_id = result
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if call_id.is_empty() {
            return None;
        }
        // A call that predates approval is investigation evidence, not
        // execution evidence.
        if baseline.contains(&journal::call_id_hash(call_id)) {
            return None;
        }
        let tool_name = result
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let paired = shell_calls.get(call_id);
        let is_shell =
            paired.is_some() || matches!(tool_name, "bash" | "shell" | "powershell" | "sh");
        if !is_shell || result.get("isError").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        // Strict contract: the executed command must equal one of the declared
        // commands verbatim (trim only). No substring or shell-semantic parsing.
        let actual = paired
            .and_then(|entry| entry.as_deref())
            .or_else(|| result.pointer("/details/command").and_then(Value::as_str))
            .map(str::trim)?;
        if !declared.iter().any(|expected| expected.trim() == actual) {
            return None;
        }
        // Explicit non-zero exit code anywhere in details -> fail.
        if result
            .pointer("/details/exitCode")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
        {
            return None;
        }
        let text = text_content(result.get("content"));
        let lower = text.to_ascii_lowercase();
        for marker in ["command failed", "command aborted", "command timed out"] {
            if lower.contains(marker) {
                return None;
            }
        }
        let has_status = lower.contains("command exited with code ");
        let ok = if has_status {
            lower.contains("command exited with code 0")
        } else {
            !text.trim().is_empty()
        };
        ok.then(|| call_id.to_owned())
    })
}
/// Whether the worker tool result for `call_id` failed or was aborted. Used by
/// the foreman gate so a failed worker cannot be followed by a reviewer PASS
/// that lets the run settle as verified. Reads only generic tool-result facts.
fn worker_failed(payload: &Value, call_id: &str) -> bool {
    let Some(messages) = payload
        .pointer("/context/llmMessages")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(result) = messages
        .iter()
        .rev()
        .find(|message| message.get("toolCallId").and_then(Value::as_str) == Some(call_id))
    else {
        return false;
    };
    if result.get("toolName").and_then(Value::as_str) != Some("subagent") {
        return false;
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let results = result.pointer("/details/results").and_then(Value::as_array);
    if let Some(results) = results {
        return results.iter().any(|entry| {
            // Reviewers inside a shared call are not the worker.
            if entry.get("agent").and_then(Value::as_str) == Some("reviewer") {
                return false;
            }
            entry
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
                || matches!(
                    entry.get("stopReason").and_then(Value::as_str),
                    Some("error" | "aborted" | "length")
                )
                || entry.get("stderr").is_some_and(|v| !v.is_null())
                || entry.get("errorMessage").is_some_and(|v| !v.is_null())
        });
    }
    // No structured details: fall back to the result's failure text.
    let text = text_content(result.get("content")).to_ascii_lowercase();
    text.contains("subagent was aborted")
        || text.contains("agent error:")
        || text.contains("agent aborted:")
}

/// Whether the reviewer tool result matching `call_id` is a non-error PASS.
fn reviewer_passed(payload: &Value, call_id: &str) -> bool {
    let Some(messages) = payload
        .pointer("/context/llmMessages")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(result) = messages
        .iter()
        .rev()
        .find(|message| message.get("toolCallId").and_then(Value::as_str) == Some(call_id))
    else {
        return false;
    };
    if result.get("toolName").and_then(Value::as_str) != Some("subagent") {
        return false;
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let text = match result.pointer("/details/results").and_then(Value::as_array) {
        Some(results) => {
            let Some(reviewer) = results
                .iter()
                .rev()
                .find(|entry| entry.get("agent").and_then(Value::as_str) == Some("reviewer"))
            else {
                return false;
            };
            if reviewer
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
            {
                return false;
            }
            if matches!(
                reviewer.get("stopReason").and_then(Value::as_str),
                Some("error" | "aborted" | "length")
            ) || reviewer
                .get("stderr")
                .is_some_and(|stderr| !stderr.is_null())
            {
                return false;
            }
            match final_assistant_text(reviewer.get("messages")) {
                Some(text) => text,
                None => return false,
            }
        }
        // No structured details: the result's model-facing text is the only
        // evidence available.
        None => text_content(result.get("content")),
    };
    verdict_is_pass(&text)
}

/// The concatenated text of the last assistant message in a child transcript.
fn final_assistant_text(messages: Option<&Value>) -> Option<String> {
    let messages = messages?.as_array()?;
    let message = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))?;
    Some(text_content(message.get("content")))
}

/// Flatten a string-or-text-block content value.
fn text_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The reviewer verdict contract: a structured JSON verdict wins; plain text
/// `PASS`/`FAIL` remains accepted for older/simple reviewers.
fn verdict_is_pass(text: &str) -> bool {
    if let Some(pass) = structured_verdict(text) {
        return pass;
    }
    let upper = text.trim().to_ascii_uppercase();
    if upper.is_empty() {
        return false;
    }
    let first = upper.lines().next().unwrap_or_default();
    let first = first.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '*' | '#' | '`' | '_' | '>' | '-')
    });
    // Only a leading PASS token counts. A whole-text "contains PASS" fallback
    // made "the review would not pass" read as a successful verification.
    if !matches!(
        first.split(|c: char| !c.is_ascii_alphanumeric()).next(),
        Some("PASS")
    ) {
        return false;
    }
    // The first line must be a CLEAN PASS verdict. Text like
    // "PASS criteria not met, verdict FAIL" starts with PASS but is a failure —
    // checking only whole-text "NOT PASS" missed it. Reject explicit verdict
    // reversals on the same line. Keep the markers narrow: an evidence clause
    // like "PASS {not valid json" or "evidence: no failure found" must still
    // count as PASS, so generic words ("NOT", "BUT", "?") are NOT reversals.
    const REVERSALS: &[&str] = &["NOT PASS", "PASS?", "FAIL", "VERDICT", "CRITERIA"];
    !REVERSALS.iter().any(|marker| first.contains(marker))
}

/// Parse `{"verdict":"PASS"|"FAIL", ...}` (optionally inside a Markdown fence)
/// from the reviewer text.
fn structured_verdict(text: &str) -> Option<bool> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let verdict = value
        .get("verdict")
        .or_else(|| value.get("result"))
        .or_else(|| value.get("status"))?
        .as_str()?
        .trim()
        .to_ascii_uppercase();
    match verdict.as_str() {
        "PASS" => Some(true),
        "FAIL" => Some(false),
        _ => None,
    }
}

/// Parse the mandatory verification clause. Only a non-empty array of command
/// strings is accepted: free-form prose cannot be checked, so it would let a
/// model's claim substitute for evidence.
fn parse_verification(input: Option<&Value>) -> Option<Verification> {
    let Value::Array(items) = input? else {
        return None;
    };
    let commands: Vec<String> = items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect();
    if commands.is_empty() {
        None
    } else {
        Some(Verification(commands))
    }
}

/// Delegation pure functions (approval ids/summaries, subagent-call scanning,
/// routing, child tool forcing) plus the per-session `Verification`
/// accessor: all owned by the engine-free crate so the plugin and the crate
/// cannot compute a different approval id or summary.
pub use pir_builtin_mode::{
    apply_foreman_routing, approval_block_reason, approval_id, approval_summary,
    force_readonly_subagent_tools, force_reviewer_subagent_tools, plan_approval_block_reason,
    plan_approval_id, plan_summary, scan_subagent_calls, subagent_agents, truncate_chars,
};

fn handle_foreman_command(state: &Arc<ModeState>, args: &str) -> CommandOutcome {
    let args = args.trim().to_ascii_lowercase();
    if args.is_empty() || args == "status" {
        let settings = state.settings();
        let pending = state.pending_approvals().len();
        return CommandOutcome {
            ok: true,
            error: Some(format!(
                "Foreman settings: worktree={}, router={}, approvals={}; phase={}; \
                 pending approvals={pending}. Use `/foreman worktree on|off`, \
                 `/foreman router inline|process`, `/foreman approvals auto|manual`.",
                if settings.worktree { "on" } else { "off" },
                settings.router.label(),
                settings.approvals.label(),
                state.phase().label(),
            )),
        };
    }
    if let Some(context) = state.context() {
        if !context.is_active() {
            return command_error("Session changed; run /foreman again.");
        }
        if let Some(session) = context.session() {
            if !session.is_idle() {
                return command_error(BUSY_MESSAGE);
            }
        }
    }

    let mut settings = state.settings();
    let message: String;
    if let Some(rest) = args.strip_prefix("worktree ") {
        settings.worktree = match rest.trim() {
            "on" => {
                if let Some(context) = state.context() {
                    if let Some(cwd) = context.session().and_then(|session| session.cwd()) {
                        match git_repo_clean(cwd.as_ref()) {
                            Ok(true) => {}
                            Ok(false) => {
                                return command_error(
                                    "Worktree mode requires a clean git working tree.",
                                )
                            }
                            Err(error) => return command_error(&error),
                        }
                    }
                }
                true
            }
            "off" => false,
            _ => return command_error("Usage: /foreman worktree on|off"),
        };
        message = format!(
            "Foreman worktree: {}.",
            if settings.worktree { "on" } else { "off" }
        );
    } else if let Some(rest) = args.strip_prefix("router ") {
        settings.router = match RouterMode::parse(rest.trim()) {
            Some(router) => router,
            None => return command_error("Usage: /foreman router inline|process"),
        };
        message = format!("Foreman router: {}.", settings.router.label());
    } else if let Some(rest) = args.strip_prefix("approvals ") {
        settings.approvals = match ApprovalMode::parse(rest.trim()) {
            Some(approvals) => approvals,
            None => return command_error("Usage: /foreman approvals auto|manual"),
        };
        message = format!("Foreman approvals: {}.", settings.approvals.label());
    } else if args == "clean" {
        let cleaned = clean_retained_worktrees(state);
        return CommandOutcome {
            ok: true,
            error: Some(cleaned),
        };
    } else {
        return command_error(
            "Usage: /foreman [status|clean] or /foreman worktree on|off, \
             /foreman router inline|process, /foreman approvals auto|manual",
        );
    }

    state.set_settings(settings);
    if let Some(context) = state.context() {
        if context.is_active() {
            persist_mode(&context, state.mode(), settings);
            state.open_journal(&context);
            state.journal_append(
                JournalEvent::Mode {
                    mode: state.mode().label().to_owned(),
                    settings,
                },
                true,
            );
            state.journal_snapshot(true);
        }
    }
    state.refresh_status();
    state.notify(&message);
    CommandOutcome {
        ok: true,
        error: Some(message),
    }
}

fn command_error(message: &str) -> CommandOutcome {
    CommandOutcome {
        ok: false,
        error: Some(message.to_owned()),
    }
}

fn git_repo_clean(cwd: &Path) -> Result<bool, String> {
    let root = std::process::Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("Cannot run git: {error}"))?;
    if !root.status.success() {
        return Err("Worktree mode requires a git repository.".to_owned());
    }
    let status = std::process::Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| format!("Cannot run git status: {error}"))?;
    if !status.status.success() {
        return Err("Cannot inspect the git working tree.".to_owned());
    }
    Ok(status.stdout.is_empty())
}

fn clean_retained_worktrees(state: &ModeState) -> String {
    let Some(context) = state.context() else {
        return "No active session; nothing to clean.".to_owned();
    };
    let Some(session) = context.session() else {
        return "No active session; nothing to clean.".to_owned();
    };
    let Some(path) = session.session_path() else {
        return "This session has no file-backed foreman worktrees.".to_owned();
    };
    let mut removed = 0usize;
    let mut failed = 0usize;
    if let Ok(text) = std::fs::read_to_string(path.as_ref()) {
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(results) = entry
                .pointer("/message/details/results")
                .and_then(Value::as_array)
            else {
                continue;
            };
            for result in results {
                let Some(worktree) = result.get("worktree") else {
                    continue;
                };
                // `cleaned` means the worktree is already gone. A merged
                // worktree whose cleanup failed is still retained.
                if worktree.get("cleaned").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let (Some(repo), Some(worktree_path)) = (
                    worktree.get("repo").and_then(Value::as_str),
                    worktree.get("path").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let removed_ok = std::process::Command::new("git")
                    .args(["-C", repo, "worktree", "remove", "--force", worktree_path])
                    .status();
                if removed_ok.is_ok_and(|status| status.success()) {
                    removed += 1;
                } else {
                    failed += 1;
                }
                if let Some(branch) = worktree.get("branch").and_then(Value::as_str) {
                    let _ = std::process::Command::new("git")
                        .args(["-C", repo, "branch", "-D", branch])
                        .status();
                }
            }
        }
    }
    format!("Cleaned {removed} retained worktree(s); {failed} failed.")
}

async fn handle_approvals_command(state: &Arc<ModeState>, _args: &str) -> CommandOutcome {
    let pending = state.pending_approvals();
    if pending.is_empty() {
        return CommandOutcome {
            ok: true,
            error: Some("No pending approvals.".to_owned()),
        };
    }
    let Some(context) = state.context() else {
        return CommandOutcome {
            ok: true,
            error: Some(format!(
                "Pending approvals: {}",
                pending
                    .iter()
                    .map(|approval| approval.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    };
    let Some(ui) = context.ui_arc() else {
        return CommandOutcome {
            ok: true,
            error: Some(format!(
                "Pending approvals: {}. No UI is attached; use `pir foreman approve`.",
                pending
                    .iter()
                    .map(|approval| approval.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    };
    let messages = context.messages_arc();
    let message = run_approval_ui(state, ui, messages).await;

    CommandOutcome {
        ok: true,
        error: Some(message),
    }
}

async fn run_approval_ui(
    state: &Arc<ModeState>,
    ui: Arc<dyn crate::plugins::services::UiService>,
    messages: Option<Arc<dyn crate::plugins::services::MessageService>>,
) -> String {
    loop {
        // A request past its 24h window must not be offered as decidable:
        // expire before listing, the same rule the CLI status/decide path
        // applies.
        state.expire_stale_approvals();
        let pending = state.pending_approvals();
        if pending.is_empty() {
            return "No pending approvals.".to_owned();
        }
        let has_plan = pending
            .iter()
            .any(|approval| approval.kind == ApprovalKind::Plan);
        let mut options: Vec<SelectOption> = Vec::with_capacity(pending.len() * 3 + 2);
        for approval in &pending {
            let label = if approval.kind == ApprovalKind::Plan {
                "plan"
            } else {
                "worker"
            };
            let detail = approval_dialog_detail(approval);
            options.push(SelectOption::new(
                format!("Approve {label} #{} — {detail}", approval.id),
                format!("approve:{}", approval.id),
            ));
            options.push(SelectOption::new(
                format!("Deny {label} #{} — {detail}", approval.id),
                format!("deny:{}", approval.id),
            ));
            if approval.kind == ApprovalKind::Plan {
                // The plan text is far too long for a one-line picker; offer
                // an explicit read-only view (Esc returns to this menu). It
                // stays below Approve/Deny so the existing fast path (Enter
                // approves) is unchanged.
                options.push(SelectOption::new(
                    format!("Review plan #{} — {detail}", approval.id),
                    format!("review:{}", approval.id),
                ));
            }
        }
        if pending.len() > 1 {
            options.push(SelectOption::new("Approve all", "approve_all"));
        }
        options.push(SelectOption::new("Cancel", "cancel"));
        let Some(choice) = ui
            .select(
                if has_plan {
                    "Plan & approvals"
                } else {
                    "Foreman approvals"
                },
                &options,
                DialogOptions::default(),
            )
            .await
            .ok()
            .flatten()
        else {
            return "Approval cancelled.".to_owned();
        };
        if choice == "cancel" {
            return "Approval cancelled.".to_owned();
        }
        if let Some(id) = choice.strip_prefix("review:") {
            if let Some(record) = pending.iter().find(|record| record.id == id) {
                let mut text = record.plan.clone().unwrap_or_default();
                if let Some(verification) = record.verification.as_ref() {
                    let commands = verification.display();
                    if !commands.trim().is_empty() {
                        text.push_str("\n\nVerification commands:\n");
                        text.push_str(commands.trim());
                    }
                }
                // Display-only: the editor returns an edited copy, but the
                // approved plan is the one recorded at submission. Esc closes
                // and the menu reopens.
                let _ = ui
                    .editor(
                        &format!("Plan {}", record.id),
                        Some(&text),
                        DialogOptions::default(),
                    )
                    .await;
            }
            continue;
        }
        if choice == "approve_all" {
            let mut count = 0usize;
            let mut approved_plan = false;
            for approval in pending {
                if let Some(record) = state.decide_approval(&approval.id, true) {
                    approved_plan |= record.kind == ApprovalKind::Plan;
                    apply_decision(state, messages.as_deref(), &record, true);
                    count += 1;
                }
            }
            return if approved_plan {
                format!(
                    "Approved {count} request(s); executing the plan under its verification contract."
                )
            } else {
                format!("Approved {count} request(s).")
            };
        }
        let Some((decision, id)) = choice.split_once(':') else {
            return "Approval cancelled.".to_owned();
        };
        let approved = match decision {
            "approve" => true,
            "deny" => false,
            _ => return "Approval cancelled.".to_owned(),
        };
        if let Some(record) = state.decide_approval(id, approved) {
            apply_decision(state, messages.as_deref(), &record, approved);
            return match (record.kind == ApprovalKind::Plan, approved) {
                (true, true) => format!(
                    "Approved plan {}; switching to normal mode to execute the verification contract.",
                    record.id
                ),
                (true, false) => format!(
                    "Denied plan {}. Still in plan mode; revise it and call submit_plan again.",
                    record.id
                ),
                (false, true) => format!("Approved worker {}.", record.id),
                (false, false) => format!("Denied worker {}.", record.id),
            };
        }
        return "Approval was already decided.".to_owned();
    }
}

const EXECUTION_CONTRACT_SECTION: &str = "execution_contract";
/// Per-run system-prompt excerpt cap. The full plan stays in the one-time
/// approval follow-up and the journal; repeating it on every turn was pure
/// context cost (and cache_read kept paying for it).
const MAX_CONTRACT_PLAN_PROMPT_CHARS: usize = 2000;
const EXECUTION_CONTRACT_PROMPT_HEAD: &str = r#"<execution_contract>
An approved plan is active for this run. It is the only active plan: ignore
any earlier plan text in this conversation that conflicts with it. Keep it as
the acceptance contract:
"#;
const EXECUTION_CONTRACT_PROMPT_TAIL: &str = r#"
Before reporting done you MUST actually run a verification command with the
available tools and observe a successful result. Saying "verified" or reasoning
about correctness is not verification: a real tool result with a zero exit
status is required. If verification fails, fix the cause and run it again.
</execution_contract>"#;

/// Head of the declared-check prompt. This is the plan-free variant of the
/// execution contract: there is no approved plan to restate, only a check the
/// user declared up front, so the prompt says that and nothing more. Without
/// this arm a `--check` contract would gate every settle while never telling
/// the model the exact string it must run.
const DECLARED_CHECK_PROMPT_HEAD: &str = r#"<declared_verification>
This session declares a verification check. The run is not complete until one
of the commands below has actually been executed and succeeded.
"#;
const DECLARED_CHECK_PROMPT_TAIL: &str = r#"
A tool result with a zero exit status is required. Saying "verified" or
reasoning about correctness does not count. If the check fails, fix the cause
and run the exact command again.
</declared_verification>"#;

/// Apply an approval decision's side effects: send the follow-up message and,
/// for an approved plan, switch the session to normal mode so the follow-up
/// executes with the full tool surface. A denied plan stays in plan mode with
/// the submitted plan retained for revision.
/// Hashes of shell tool calls already present in the session transcript. Used
/// as the evidence baseline when a plan is approved, so investigation-time
/// commands can never satisfy the execution contract even if the approval
/// message later disappears from the compacted context.
fn shell_call_baseline(context: &PluginContext) -> Option<Vec<u64>> {
    let session = context.session()?;
    let entries = session.entries()?;
    let views = entries.as_array()?;
    let mut hashes = Vec::new();
    for view in views {
        if let Some(messages) = view.get("messages").and_then(Value::as_array) {
            for message in messages {
                collect_shell_call_hashes(message, &mut hashes);
            }
        } else if let Some(message) = view.get("entry").and_then(|entry| entry.get("message")) {
            collect_shell_call_hashes(message, &mut hashes);
        }
    }
    hashes.sort_unstable();
    hashes.dedup();
    Some(hashes)
}

fn collect_shell_call_hashes(message: &Value, hashes: &mut Vec<u64>) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
            continue;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(name, "bash" | "shell" | "powershell" | "sh") {
            continue;
        }
        if let Some(id) = block.get("id").and_then(Value::as_str) {
            hashes.push(journal::call_id_hash(id));
        }
    }
}

fn apply_decision(
    state: &Arc<ModeState>,
    messages: Option<&dyn crate::plugins::services::MessageService>,
    record: &ApprovalRecord,
    approved: bool,
) {
    if record.kind == ApprovalKind::Plan {
        if approved {
            // Install the contract in memory BEFORE the mode switch: apply_mode
            // snapshots the journal, so the contract is persisted with the
            // switch to Normal and survives resume.
            if let Some(plan) = record.plan.as_deref() {
                state.set_active_contract(journal::ExecutionContract {
                    plan: plan.to_owned(),
                    verification: record.verification.clone(),
                    approval_id: Some(record.id.clone()),
                    evidence_baseline: state.context().as_ref().and_then(shell_call_baseline),
                });
            }
            // Persist + announce the mode switch before the follow-up message
            // is pumped, so the execution run starts in Normal mode. Bypass
            // the busy guard (the submit-plan run is still active here).
            if state.mode() == Mode::Plan {
                apply_mode_with_notice(
                    state,
                    Mode::Normal,
                    Some("Plan approved. Leaving plan mode; the verification contract is active."),
                );
            }
        } else {
            state.revise_plan();
        }
    }
    if let Some(messages) = messages {
        send_approval_message(messages, record, approved);
    }
}

fn approval_dialog_detail(approval: &ApprovalRecord) -> String {
    // The plan dialog is a single-line picker; show the summary plus the
    // first verification command (the contract being approved) instead of a
    // bare title. A rejected plan needs the same context as an approval.
    let mut detail = approval.summary.clone();
    if let Some(command) = approval
        .verification
        .as_ref()
        .and_then(|verification| verification.commands().first())
    {
        detail.push_str(" · verify: ");
        detail.push_str(&crate::text::truncate_chars_with_marker(command, 100));
    }
    detail
}

fn send_approval_message(
    messages: &dyn crate::plugins::services::MessageService,
    record: &ApprovalRecord,
    approved: bool,
) {
    let content = if record.kind == ApprovalKind::Plan {
        if approved {
            let plan = record.plan.as_deref().unwrap_or("(see submitted plan)");
            let mut content = format!(
                "PLAN APPROVED: the user approved your plan `{}`.\n\n{}\n\nImplement this plan \
                 now. Use the appropriate tools and verify the result before reporting done.",
                record.id, plan
            );
            if let Some(verification) = record.verification.as_ref() {
                let text = verification.display();
                if !text.trim().is_empty() {
                    content.push_str(
                        "\n\nExecution verification criteria (a real tool result must show success):\n",
                    );
                    content.push_str("Run one of these commands EXACTLY:\n");
                    content.push_str(text.trim());
                }
            }
            content
        } else {
            format!(
                "PLAN DENIED: the user did not approve plan `{}`. Revise it or ask a clarifying \
                 question, then call submit_plan again; follow any feedback the user adds in \
                 their next message.",
                record.id
            )
        }
    } else if approved {
        format!(
            "FOREMAN APPROVAL APPROVED: `{}` ({}). Retry that worker delegation now, then \
             run the independent reviewer before finishing.",
            record.id, record.summary
        )
    } else {
        format!(
            "FOREMAN APPROVAL DENIED: `{}` ({}). Do not retry this delegation; choose a \
             different approach or report the blocker to the user.",
            record.id, record.summary
        )
    };
    messages.send_user_message(
        UserContent::Text(content),
        SendUserMessageOptions {
            deliver_as: Some(DeliverAs::FollowUp),
        },
    );
}
