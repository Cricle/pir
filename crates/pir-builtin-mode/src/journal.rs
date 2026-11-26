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

/// Stable hash of a tool-call id. Evidence watermarks store hashes instead of
/// raw ids so a long session's journal stays small. FNV-1a is used instead of
/// `DefaultHasher` because the latter is not guaranteed stable across Rust
/// releases, while these hashes persist in the journal and are compared after
/// an upgrade.
pub fn call_id_hash(id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Verification clause of an approved plan: one or more concrete shell
/// commands. The exact command (one of the listed strings) must be executed
/// and succeed. Free-form prose is deliberately not representable: it cannot
/// be checked, so it would let a probabilistic model's claim stand in for
/// evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Verification(pub Vec<String>);

impl Verification {
    /// The verification commands, in order.
    pub fn commands(&self) -> &[String] {
        &self.0
    }

    /// The commands as one newline-joined block.
    ///
    /// ```
    /// use pir_builtin_mode::Verification;
    ///
    /// let v = Verification(vec!["cargo test".into(), "cargo clippy".into()]);
    /// assert_eq!(v.display(), "cargo test\ncargo clippy");
    /// assert_eq!(v.commands().len(), 2);
    /// ```
    pub fn display(&self) -> String {
        self.0.join("\n")
    }

    /// Deterministic material for approval-id hashing.
    pub fn hash_key(&self) -> String {
        let mut joined = String::from("commands:");
        joined.push_str(&self.0.join("\u{1f}"));
        joined
    }
}

/// Execution-time acceptance contract carried from an approved plan.
///
/// A contract is **one-shot**: the first settle boundary that sees a matching
/// successful command records completion and retires it. Later work is plain
/// Normal and is not judged by a stale plan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionContract {
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub verification: Option<Verification>,
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Hashes of shell tool-call ids that already existed when the contract was
    /// approved. A result from one of those calls can never satisfy the
    /// contract. `None` (host could not enumerate the transcript) makes the
    /// gate fail closed rather than accept unknown history.
    #[serde(default)]
    pub evidence_baseline: Option<Vec<u64>>,
}

/// Replayed foreman state.
#[derive(Debug, Clone)]
pub struct JournalState {
    pub mode: Mode,
    pub phase: Phase,
    pub generation: u32,
    /// Latest worker (non-reviewer) subagent call id, for failure detection.
    pub last_worker_call: Option<String>,
    pub reviewer: Option<String>,
    pub nudges: u32,
    /// Worker generation with a persisted reviewer PASS.
    pub review_passed_generation: Option<u32>,
    /// The approved plan's execution contract, carried into Normal mode.
    pub active_contract: Option<ExecutionContract>,
    /// The approval id whose verification tool evidence passed. Kept as an
    /// audit marker after the contract is retired; it never gates by itself.
    pub verification_passed_id: Option<String>,
    /// The tool-call id whose successful result completed the contract.
    pub verified_call: Option<String>,
    pub settings: ModeSettings,
    pub approvals: Vec<ApprovalRecord>,
    /// Derived compatibility flag: true iff `allow_next_worker_id` is Some.
    pub allow_next_worker: bool,
    /// The specific approval id authorized to pass once. Bound to a specific
    /// request so approving A can never admit a different task B. Kept as the
    /// single "latest" mirror for older journal snapshots.
    pub allow_next_worker_id: Option<String>,
    /// All approval ids authorized to pass once (approve-all can grant several
    /// distinct workers in one decision). Each is consumed independently.
    pub allow_next_worker_ids: Vec<String>,
    /// The most recently submitted plan text (plan mode).
    pub submitted_plan: Option<String>,
}

impl Default for JournalState {
    fn default() -> Self {
        Self {
            mode: Mode::Normal,
            phase: Phase::Idle,
            generation: 0,
            last_worker_call: None,
            reviewer: None,
            nudges: 0,
            review_passed_generation: None,
            active_contract: None,
            verification_passed_id: None,
            verified_call: None,
            settings: ModeSettings::default(),
            approvals: Vec::new(),
            allow_next_worker: false,
            allow_next_worker_id: None,
            allow_next_worker_ids: Vec::new(),
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
                last_worker_call,
                reviewer,
                nudges,
                review_passed_generation,
                active_contract,
                verification_passed_id,
                verified_call,
                settings,
                approvals,
                allow_next_worker,
                allow_next_worker_id,
                allow_next_worker_ids,
                submitted_plan,
            } => {
                self.mode = Mode::from_label(&mode).unwrap_or(Mode::Normal);
                self.phase = Phase::from_label(&phase).unwrap_or(Phase::Idle);
                self.generation = generation;
                self.last_worker_call = last_worker_call;
                self.reviewer = reviewer;
                self.nudges = nudges;
                self.review_passed_generation = review_passed_generation;
                self.active_contract = active_contract;
                self.verification_passed_id = verification_passed_id;
                self.verified_call = verified_call;
                self.settings = settings;
                self.approvals = approvals;
                self.allow_next_worker_id = allow_next_worker_id;
                // Old snapshots only carry the single id; seed the set from it.
                self.allow_next_worker_ids = if allow_next_worker_ids.is_empty() {
                    self.allow_next_worker_id.clone().into_iter().collect()
                } else {
                    allow_next_worker_ids
                };
                // Prefer the explicit ids; fall back to the legacy bool.
                self.allow_next_worker =
                    !self.allow_next_worker_ids.is_empty() || allow_next_worker;
                self.submitted_plan = submitted_plan;
            }
            JournalEvent::Mode { mode, settings } => {
                self.mode = Mode::from_label(&mode).unwrap_or(Mode::Normal);
                self.settings = settings;
            }
            JournalEvent::Delegation { call, agents, .. } => {
                // Mirror `record_subagent_agents`: only worker calls advance
                // the generation. Reviewer-only calls must not invalidate the
                // worker's gate state, otherwise a resumed session would
                // re-nudge a run that was already under review.
                let has_worker = agents.iter().any(|agent| agent != "reviewer");
                let has_reviewer = agents.iter().any(|agent| agent == "reviewer");
                if has_worker {
                    self.generation = self.generation.saturating_add(1);
                    self.nudges = 0;
                    self.last_worker_call = if call.is_empty() {
                        None
                    } else {
                        Some(call.clone())
                    };
                    self.review_passed_generation = None;
                    self.reviewer = None;
                    self.phase = Phase::Delegating;
                }
                if has_reviewer && !has_worker {
                    // A fresh reviewer verdict supersedes an earlier PASS. A
                    // mixed worker+reviewer call shares one call id and cannot
                    // verify itself, so it never becomes the reviewer.
                    self.review_passed_generation = None;
                    if !call.is_empty() {
                        self.reviewer = Some(call);
                    }
                    self.phase = Phase::Review;
                }
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
                verification,
                created_at_ms,
            } => {
                let Some(state) = ApprovalState::parse(&state) else {
                    return;
                };
                let approval_id = id.clone();
                let created_at = if created_at_ms > 0 {
                    created_at_ms
                } else {
                    now_ms()
                };
                if let Some(record) = self.approvals.iter_mut().find(|record| record.id == id) {
                    record.state = state;
                    if state == ApprovalState::Requested {
                        // A re-submitted approval reuses the stable id; refresh
                        // its timestamp/content so it cannot inherit a stale
                        // expiry from the earlier request. A replay carries the
                        // original timestamp, so replay never extends expiry.
                        record.created_at_ms = created_at;
                        record.plan = plan;
                        record.verification = verification;
                        record.kind = kind;
                        record.summary = summary;
                    }
                } else if state == ApprovalState::Requested {
                    self.approvals.push(ApprovalRecord {
                        id,
                        call,
                        summary,
                        state,
                        created_at_ms: created_at,
                        plan,
                        verification,
                        kind,
                    });
                }
                // An approved plan becomes the active execution contract. The
                // verification text is copied from the approval record so a
                // resumed journal does not need the original submitted plan.
                if kind == ApprovalKind::Plan && state == ApprovalState::Approved {
                    if let Some(record) = self
                        .approvals
                        .iter()
                        .find(|record| record.id == approval_id)
                    {
                        self.active_contract = record.plan.as_ref().map(|plan| ExecutionContract {
                            plan: plan.clone(),
                            verification: record.verification.clone(),
                            approval_id: Some(approval_id.clone()),
                            // The approval event does not carry the transcript
                            // baseline; a snapshot written right after the
                            // decision restores it. Without one the gate fails
                            // closed (never guesses a boundary).
                            evidence_baseline: None,
                        });
                        self.verification_passed_id = None;
                        self.verified_call = None;
                    }
                }
                if kind == ApprovalKind::Worker {
                    if state == ApprovalState::Requested {
                        // A pending request never inherits an older allowance,
                        // matching the live `request_approval` path (the process
                        // may die between its journal append and its snapshot).
                        self.allow_next_worker_ids.retain(|id| id != &approval_id);
                        if self.allow_next_worker_ids.is_empty() {
                            self.allow_next_worker_id = None;
                            self.allow_next_worker = false;
                        } else {
                            self.allow_next_worker_id = self.allow_next_worker_ids.last().cloned();
                        }
                    } else if state == ApprovalState::Approved {
                        self.allow_next_worker_ids.retain(|id| id != &approval_id);
                        self.allow_next_worker_ids.push(approval_id.clone());
                        self.allow_next_worker_id = Some(approval_id);
                        self.allow_next_worker = true;
                    } else if matches!(state, ApprovalState::Denied | ApprovalState::Expired) {
                        self.allow_next_worker_ids.retain(|id| id != &approval_id);
                        if self.allow_next_worker_ids.is_empty() {
                            self.allow_next_worker_id = None;
                            self.allow_next_worker = false;
                        } else {
                            self.allow_next_worker_id = self.allow_next_worker_ids.last().cloned();
                        }
                    }
                }
                if self.approvals.len() > MAX_APPROVAL_HISTORY {
                    let excess = self.approvals.len() - MAX_APPROVAL_HISTORY;
                    self.approvals.drain(0..excess);
                }
            }
            JournalEvent::AllowanceConsumed { id } => {
                self.allow_next_worker_ids.retain(|allowed| allowed != &id);
                if self.allow_next_worker_ids.is_empty() {
                    self.allow_next_worker_id = None;
                    self.allow_next_worker = false;
                } else if self.allow_next_worker_id.as_deref() == Some(id.as_str()) {
                    self.allow_next_worker_id = self.allow_next_worker_ids.last().cloned();
                }
            }
            JournalEvent::Worktree { .. } => {}
            JournalEvent::PlanSubmitted { plan, .. } => {
                self.submitted_plan = Some(plan);
            }
            JournalEvent::PlanRevision => {
                self.submitted_plan = None;
                // A revised plan invalidates any contract derived from it.
                self.active_contract = None;
                self.verification_passed_id = None;
                self.verified_call = None;
            }
            JournalEvent::Contract { contract } => {
                self.active_contract = Some(contract);
                self.verification_passed_id = None;
                self.verified_call = None;
            }
            JournalEvent::ContractCompleted { approval_id, call } => {
                // One-shot contract: completion retires it. The pass id and
                // call stay as audit markers; no later settle is gated by this
                // plan.
                self.active_contract = None;
                self.verification_passed_id = Some(approval_id);
                self.verified_call = call;
                self.phase = Phase::Verified;
            }
            JournalEvent::Verification {
                approval_id,
                pass,
                call,
            } => {
                if pass {
                    self.verification_passed_id = Some(approval_id);
                    self.verified_call = call;
                    self.phase = Phase::Verified;
                } else {
                    self.verification_passed_id = None;
                    self.verified_call = None;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
// `Snapshot` intentionally carries the whole persisted state; it is written
// once per compaction, and boxing it would complicate the wire shape.
#[allow(clippy::large_enum_variant)]
pub enum JournalEvent {
    Snapshot {
        mode: String,
        phase: String,
        generation: u32,
        #[serde(default)]
        last_worker_call: Option<String>,
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
        allow_next_worker_ids: Vec<String>,
        #[serde(default)]
        submitted_plan: Option<String>,
        #[serde(default)]
        active_contract: Option<ExecutionContract>,
        #[serde(default)]
        verification_passed_id: Option<String>,
        #[serde(default)]
        verified_call: Option<String>,
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
        #[serde(default)]
        verification: Option<Verification>,
        /// Original request time. Replaying an old journal must not reset the
        /// 24h expiry to `now`; a genuine re-submission writes a fresh value.
        #[serde(default)]
        created_at_ms: u64,
    },
    /// A one-shot worker approval was consumed by a delegation. Persisted so a
    /// crash or restart cannot re-arm the same allowance.
    AllowanceConsumed {
        id: String,
    },
    /// A plan was submitted for approval (plan mode).
    PlanSubmitted {
        id: String,
        plan: String,
        #[serde(default)]
        verification: Option<Verification>,
    },
    /// The submitted plan was revised/replaced; clear the persisted copy.
    PlanRevision,
    /// An approved plan became the active execution contract (Normal mode).
    Contract {
        contract: ExecutionContract,
    },
    /// The one-shot contract was satisfied and retired. The approval id and
    /// successful call id remain as audit markers.
    ContractCompleted {
        approval_id: String,
        #[serde(default)]
        call: Option<String>,
    },
    /// Execution-time verification evidence was judged (pass/fail) for a contract.
    Verification {
        approval_id: String,
        pass: bool,
        /// The tool-call id that satisfied the contract.
        #[serde(default)]
        call: Option<String>,
    },
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
pub fn journal_path(session_path: &Path) -> PathBuf {
    let mut os = session_path.as_os_str().to_owned();
    os.push(JOURNAL_SUFFIX);
    PathBuf::from(os)
}

/// Read and replay the journal. `None` when the file is missing or contains no
/// usable event.
///
/// Public because the host plugin reads the persisted journal directly when it
/// starts or rebinds a session (the crate owns the format, the engine owns the
/// session path).
pub fn read_state(session_path: &Path) -> Option<JournalState> {
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
pub struct ForemanJournal {
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
}

impl ForemanJournal {
    /// Open (creating if needed) the `<session>.foreman.jsonl` sidecar for
    /// `session_path`, positioned to append.
    pub fn open(session_path: &Path) -> std::io::Result<Self> {
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

    pub fn append(&mut self, event: JournalEvent, sync: bool) -> std::io::Result<()> {
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

    pub fn maybe_compact(&mut self, state: &JournalState) -> std::io::Result<()> {
        if self.bytes > MAX_JOURNAL_BYTES {
            self.write_snapshot(state, true)?;
        }
        Ok(())
    }

    /// Rewrite the journal as one snapshot. The temp-file + rename swap means
    /// a concurrent reader sees either the old stream or the new snapshot.
    pub fn write_snapshot(&mut self, state: &JournalState, sync: bool) -> std::io::Result<()> {
        let line = JournalLine {
            v: JOURNAL_VERSION,
            event: JournalEvent::Snapshot {
                mode: state.mode.label().to_owned(),
                phase: state.phase.label().to_owned(),
                generation: state.generation,
                last_worker_call: state.last_worker_call.clone(),
                reviewer: state.reviewer.clone(),
                nudges: state.nudges,
                review_passed_generation: state.review_passed_generation,
                settings: state.settings,
                approvals: state.approvals.clone(),
                allow_next_worker: state.allow_next_worker,
                allow_next_worker_id: state.allow_next_worker_id.clone(),
                allow_next_worker_ids: state.allow_next_worker_ids.clone(),
                submitted_plan: state.submitted_plan.clone(),
                active_contract: state.active_contract.clone(),
                verification_passed_id: state.verification_passed_id.clone(),
                verified_call: state.verified_call.clone(),
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
pub fn write_atomic_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
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
/// Hashes of shell tool calls already present in the session transcript at
/// approval time. Used as the contract's evidence baseline in the CLI approval
/// path, where there is no live plugin context to scan.
fn session_shell_call_hashes(path: &Path) -> Option<Vec<u64>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut hashes = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(serde_json::Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        collect_shell_call_hashes(message, &mut hashes);
    }
    hashes.sort_unstable();
    hashes.dedup();
    Some(hashes)
}

/// Collect shell tool-call ids from one message's content blocks.
fn collect_shell_call_hashes(message: &serde_json::Value, hashes: &mut Vec<u64>) {
    let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("toolCall") {
            continue;
        }
        let name = block
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !matches!(name, "bash" | "shell" | "powershell" | "sh") {
            continue;
        }
        if let Some(id) = block.get("id").and_then(serde_json::Value::as_str) {
            hashes.push(call_id_hash(id));
        }
    }
}

pub fn decide_approval_for_session(
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
    // A single CLI decision can approve several pending workers at once, so
    // grant every one of them (each is consumed independently by take_allow_for).
    let approved_worker_ids: Vec<String> = if approved {
        decided
            .iter()
            .filter(|record| record.kind == ApprovalKind::Worker)
            .map(|record| record.id.clone())
            .collect()
    } else {
        Vec::new()
    };
    if approved || !approved_worker_ids.is_empty() {
        // A deny clears ALL outstanding allowances (a stale approval must not
        // survive a later deny), matching the live plugin's decide_approval.
        if !approved {
            state.allow_next_worker_ids.clear();
        } else {
            for id in &approved_worker_ids {
                state
                    .allow_next_worker_ids
                    .retain(|existing| existing != id);
                state.allow_next_worker_ids.push(id.clone());
            }
        }
        state.allow_next_worker = !state.allow_next_worker_ids.is_empty();
        state.allow_next_worker_id = state.allow_next_worker_ids.last().cloned();
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
        // Carry the approved plan into an execution contract instead of
        // dropping it: the resumed Normal run must keep the acceptance criteria.
        if let Some(record) = decided
            .iter()
            .find(|record| record.kind == ApprovalKind::Plan && record.plan.is_some())
        {
            state.active_contract = record.plan.as_ref().map(|plan| ExecutionContract {
                plan: plan.clone(),
                verification: record.verification.clone(),
                approval_id: Some(record.id.clone()),
                evidence_baseline: session_shell_call_hashes(session_path),
            });
            state.verification_passed_id = None;
            state.verified_call = None;
        }
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
                    verification: record.verification.clone(),
                    created_at_ms: record.created_at_ms,
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
