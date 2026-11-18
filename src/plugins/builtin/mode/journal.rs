//! Durable foreman journal.
//!
//! One append-only JSONL sidecar next to the session file. A `snapshot` event
//! resets the replayed state; later events are applied in order. The journal
//! is small (tens of records per session), so it is rewritten as one snapshot
//! when it exceeds [`MAX_JOURNAL_BYTES`] or at shutdown.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::approvals::{now_ms, ApprovalKind, ApprovalRecord, ApprovalState};
use super::settings::ModeSettings;
use super::{Mode, Phase};

const JOURNAL_SUFFIX: &str = ".foreman.jsonl";
const JOURNAL_VERSION: u8 = 1;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024;
const MAX_APPROVAL_HISTORY: usize = 64;

/// Replayed foreman state.
#[derive(Debug, Clone)]
pub(crate) struct JournalState {
    pub(crate) mode: Mode,
    pub(crate) phase: Phase,
    pub(crate) generation: u32,
    pub(crate) reviewer: Option<String>,
    pub(crate) nudges: u32,
    /// Worker generation with a persisted reviewer PASS.
    pub(crate) review_passed_generation: Option<u32>,
    pub(crate) settings: ModeSettings,
    pub(crate) approvals: Vec<ApprovalRecord>,
    /// Derived compatibility flag: true iff `allow_next_worker_id` is Some.
    pub(crate) allow_next_worker: bool,
    /// The specific approval id authorized to pass once.
    pub(crate) allow_next_worker_id: Option<String>,
    /// The most recently submitted plan text (plan mode).
    pub(crate) submitted_plan: Option<String>,
}

impl Default for JournalState {
    fn default() -> Self {
        Self {
            mode: Mode::Normal,
            phase: Phase::Idle,
            generation: 0,
            reviewer: None,
            nudges: 0,
            review_passed_generation: None,
            settings: ModeSettings::default(),
            approvals: Vec::new(),
            allow_next_worker: false,
            allow_next_worker_id: None,
            submitted_plan: None,
        }
    }
}

impl JournalState {
    fn apply(&mut self, event: JournalEvent) {
        match event {
            JournalEvent::Snapshot {
                mode,
                phase,
                generation,
                reviewer,
                nudges,
                review_passed_generation,
                settings,
                approvals,
                allow_next_worker,
                allow_next_worker_id,
                submitted_plan,
            } => {
                self.mode = Mode::from_label(&mode).unwrap_or(Mode::Normal);
                self.phase = Phase::from_label(&phase).unwrap_or(Phase::Idle);
                self.generation = generation;
                self.reviewer = reviewer;
                self.nudges = nudges;
                self.review_passed_generation = review_passed_generation;
                self.settings = settings;
                self.approvals = approvals;
                self.allow_next_worker_id = allow_next_worker_id;
                // Prefer the explicit id; fall back to the legacy bool.
                self.allow_next_worker = self.allow_next_worker_id.is_some() || allow_next_worker;
                self.submitted_plan = submitted_plan;
            }
            JournalEvent::Mode { mode, settings } => {
                self.mode = Mode::from_label(&mode).unwrap_or(Mode::Normal);
                self.settings = settings;
            }
            JournalEvent::Delegation { .. } => {
                self.generation = self.generation.saturating_add(1);
                self.reviewer = None;
                self.nudges = 0;
                self.phase = Phase::Delegating;
            }
            JournalEvent::Review { pass, .. } => {
                self.phase = if pass { Phase::Verified } else { Phase::Review };
                if pass {
                    self.review_passed_generation = Some(self.generation);
                } else {
                    self.review_passed_generation = None;
                }
            }
            JournalEvent::Gate {
                generation,
                nudge,
                phase,
            } => {
                self.generation = generation;
                self.nudges = nudge;
                self.phase = Phase::from_label(&phase).unwrap_or(Phase::Rework);
            }
            JournalEvent::Phase { phase } => {
                self.phase = Phase::from_label(&phase).unwrap_or(self.phase);
            }
            JournalEvent::Approval {
                id,
                call,
                state,
                summary,
                kind,
                plan,
            } => {
                let Some(state) = ApprovalState::parse(&state) else {
                    return;
                };
                let approval_id = id.clone();
                if let Some(record) = self.approvals.iter_mut().find(|record| record.id == id) {
                    record.state = state;
                    if state == ApprovalState::Requested {
                        // A re-submitted approval reuses the stable id; refresh
                        // its timestamp/content so it cannot inherit a stale
                        // expiry from the earlier request.
                        record.created_at_ms = now_ms();
                        record.plan = plan;
                        record.kind = kind;
                        record.summary = summary;
                    }
                } else if state == ApprovalState::Requested {
                    self.approvals.push(ApprovalRecord {
                        id,
                        call,
                        summary,
                        state,
                        created_at_ms: now_ms(),
                        plan,
                        kind,
                    });
                }
                if kind == ApprovalKind::Worker {
                    if state == ApprovalState::Approved {
                        self.allow_next_worker_id = Some(approval_id);
                        self.allow_next_worker = true;
                    } else if matches!(state, ApprovalState::Denied | ApprovalState::Expired) {
                        self.allow_next_worker_id = None;
                        self.allow_next_worker = false;
                    }
                }
                if self.approvals.len() > MAX_APPROVAL_HISTORY {
                    let excess = self.approvals.len() - MAX_APPROVAL_HISTORY;
                    self.approvals.drain(0..excess);
                }
            }
            JournalEvent::Worktree { .. } => {}
            JournalEvent::PlanSubmitted { plan, .. } => {
                self.submitted_plan = Some(plan);
            }
            JournalEvent::PlanRevision => {
                self.submitted_plan = None;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(super) enum JournalEvent {
    Snapshot {
        mode: String,
        phase: String,
        generation: u32,
        reviewer: Option<String>,
        nudges: u32,
        #[serde(default)]
        review_passed_generation: Option<u32>,
        settings: ModeSettings,
        #[serde(default)]
        approvals: Vec<ApprovalRecord>,
        #[serde(default)]
        allow_next_worker: bool,
        #[serde(default)]
        allow_next_worker_id: Option<String>,
        #[serde(default)]
        submitted_plan: Option<String>,
    },
    Mode {
        mode: String,
        settings: ModeSettings,
    },
    Delegation {
        call: String,
        agents: Vec<String>,
        #[serde(default)]
        executor: String,
        #[serde(default)]
        worktree: bool,
    },
    Review {
        call: String,
        pass: bool,
    },
    Gate {
        generation: u32,
        nudge: u32,
        phase: String,
    },
    Phase {
        phase: String,
    },
    Approval {
        id: String,
        call: String,
        state: String,
        summary: String,
        #[serde(default)]
        kind: ApprovalKind,
        #[serde(default)]
        plan: Option<String>,
    },
    /// A plan was submitted for approval (plan mode).
    PlanSubmitted {
        id: String,
        plan: String,
    },
    /// The submitted plan was revised/replaced; clear the persisted copy.
    PlanRevision,
    Worktree {
        state: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        call: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalLine {
    #[serde(default = "journal_version")]
    v: u8,
    #[serde(flatten)]
    event: JournalEvent,
}

fn journal_version() -> u8 {
    JOURNAL_VERSION
}

/// `<session>.foreman.jsonl`.
pub(super) fn journal_path(session_path: &Path) -> PathBuf {
    let mut os = session_path.as_os_str().to_owned();
    os.push(JOURNAL_SUFFIX);
    PathBuf::from(os)
}

/// Read and replay the journal. `None` when the file is missing or contains no
/// usable event.
pub(super) fn read_state(session_path: &Path) -> Option<JournalState> {
    let path = journal_path(session_path);
    let mut file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let mut state = JournalState::default();
    let mut saw_event = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<JournalLine>(line) else {
            // A torn trailing line is the normal crash shape; skip it.
            continue;
        };
        if record.v != JOURNAL_VERSION {
            continue;
        }
        state.apply(record.event);
        saw_event = true;
    }
    saw_event.then_some(state)
}

/// Append-only journal handle.
pub(super) struct ForemanJournal {
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
}

impl ForemanJournal {
    pub(crate) fn open(session_path: &Path) -> std::io::Result<Self> {
        let path = journal_path(session_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self { path, file, bytes })
    }

    pub(crate) fn append(&mut self, event: JournalEvent, sync: bool) -> std::io::Result<()> {
        let line = JournalLine {
            v: JOURNAL_VERSION,
            event,
        };
        let mut bytes = serde_json::to_vec(&line).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        if sync {
            self.file.sync_data()?;
        }
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    pub(crate) fn maybe_compact(&mut self, state: &JournalState) -> std::io::Result<()> {
        if self.bytes > MAX_JOURNAL_BYTES {
            self.write_snapshot(state, true)?;
        }
        Ok(())
    }

    /// Rewrite the journal as one snapshot. The temp-file + rename swap means
    /// a concurrent reader sees either the old stream or the new snapshot.
    pub(crate) fn write_snapshot(
        &mut self,
        state: &JournalState,
        sync: bool,
    ) -> std::io::Result<()> {
        let line = JournalLine {
            v: JOURNAL_VERSION,
            event: JournalEvent::Snapshot {
                mode: state.mode.label().to_owned(),
                phase: state.phase.label().to_owned(),
                generation: state.generation,
                reviewer: state.reviewer.clone(),
                nudges: state.nudges,
                review_passed_generation: state.review_passed_generation,
                settings: state.settings,
                approvals: state.approvals.clone(),
                allow_next_worker: state.allow_next_worker,
                allow_next_worker_id: state.allow_next_worker_id.clone(),
                submitted_plan: state.submitted_plan.clone(),
            },
        };
        let mut bytes = serde_json::to_vec(&line).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        write_atomic(&self.path, &bytes, sync)?;
        self.bytes = bytes.len() as u64;
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        Ok(())
    }
}

/// Atomically replace one sidecar. Used for `.mode.json` and journal
/// snapshots.
pub(super) fn write_atomic_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    write_atomic(path, &bytes, true)
}

fn write_atomic(path: &Path, bytes: &[u8], sync: bool) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("foreman");
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let result = (|| {
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(bytes)?;
            if sync {
                file.sync_all()?;
            }
        }
        std::fs::rename(&tmp, path)?;
        if sync {
            sync_parent_directory(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Apply an approval decision to a session journal and return the affected
/// records. Used by the detached `pir foreman approve/deny` command.
pub(crate) fn decide_approval_for_session(
    session_path: &Path,
    id: Option<&str>,
    approved: bool,
) -> Result<Vec<ApprovalRecord>, String> {
    let mut state = read_state(session_path).ok_or_else(|| {
        "this session has no foreman journal; start it with /mode foreman or pir foreman run"
            .to_owned()
    })?;
    // Apply the same expiry rule as the live plugin before deciding: an
    // approval that aged past the window is never decidable from the CLI.
    let now = now_ms();
    for record in state.approvals.iter_mut() {
        if record.is_expired_at(now) {
            record.state = ApprovalState::Expired;
        }
    }
    let mut decided = Vec::new();
    let state_name = if approved {
        ApprovalState::Approved
    } else {
        ApprovalState::Denied
    };
    for record in state.approvals.iter_mut() {
        if !record.is_pending() {
            continue;
        }
        if id.is_some_and(|id| id != record.id) {
            continue;
        }
        record.state = state_name;
        decided.push(record.clone());
    }
    if decided.is_empty() {
        return Err(match id {
            Some(id) => format!("approval not found or already decided: {id}"),
            None => "no pending approvals in this session".to_owned(),
        });
    }
    // Only worker approvals gate the next worker; plan approvals are handled
    // by the live plugin's mode switch and must not leave a stale allowance.
    let approved_worker_id = if approved {
        decided
            .iter()
            .find(|record| record.kind == ApprovalKind::Worker)
            .map(|record| record.id.clone())
    } else {
        None
    };
    if approved_worker_id.is_some() || !approved {
        state.allow_next_worker = approved_worker_id.is_some();
        state.allow_next_worker_id = approved_worker_id;
    }
    // An approved plan transitions the session to normal mode in the journal
    // too, so a resumed/child process (which prefers the journal over the
    // sidecar) restores the full tool surface and executes the plan.
    let plan_approved = approved
        && decided
            .iter()
            .any(|record| record.kind == ApprovalKind::Plan);
    if plan_approved {
        state.mode = Mode::Normal;
        state.phase = Phase::Idle;
        state.submitted_plan = None;
    }
    let mut journal = ForemanJournal::open(session_path)
        .map_err(|error| format!("cannot open foreman journal: {error}"))?;
    if plan_approved {
        journal
            .append(
                JournalEvent::Mode {
                    mode: Mode::Normal.label().to_owned(),
                    settings: state.settings,
                },
                true,
            )
            .map_err(|error| format!("cannot append plan mode switch: {error}"))?;
    }
    for record in &decided {
        journal
            .append(
                JournalEvent::Approval {
                    id: record.id.clone(),
                    call: record.call.clone(),
                    state: state_name.label().to_owned(),
                    summary: record.summary.clone(),
                    kind: record.kind,
                    plan: record.plan.clone(),
                },
                true,
            )
            .map_err(|error| format!("cannot append approval decision: {error}"))?;
    }
    journal
        .write_snapshot(&state, true)
        .map_err(|error| format!("cannot persist approval decision: {error}"))?;
    Ok(decided)
}

impl super::ModeState {
    pub(super) fn journal_append(&self, event: JournalEvent, sync: bool) {
        let mut guard = self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = guard.as_mut() else {
            return;
        };
        if let Err(error) = journal.append(event, sync) {
            crate::ext_diag!("[mode] failed to append foreman journal: {error}");
        }
    }

    pub(super) fn journal_snapshot(&self, sync: bool) {
        let state = self.journal_state();
        let mut guard = self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = guard.as_mut() else {
            return;
        };
        if let Err(error) = journal.write_snapshot(&state, sync) {
            crate::ext_diag!("[mode] failed to write foreman journal snapshot: {error}");
        }
    }

    pub(super) fn journal_maybe_compact(&self) {
        let state = self.journal_state();
        let mut guard = self
            .journal
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(journal) = guard.as_mut() else {
            return;
        };
        if let Err(error) = journal.maybe_compact(&state) {
            crate::ext_diag!("[mode] failed to compact foreman journal: {error}");
        }
    }
}
