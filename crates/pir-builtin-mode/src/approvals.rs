//! Foreman approval records (durable queue entries).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::journal::Verification;

/// Lifecycle of one approval request.
///
/// `Requested` → `Approved`/`Denied`; `Expired` when it ages out undecided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalState {
    /// Waiting for a user decision.
    Requested,
    /// Approved for the task bound to this record's id.
    Approved,
    /// Refused; the task id may be requested again.
    Denied,
    /// Dropped without a decision.
    Expired,
}

impl ApprovalState {
    /// The wire label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    /// Parse a [`ApprovalState::label`]; `None` if unknown.
    ///
    /// ```
    /// use pir_builtin_mode::ApprovalState;
    ///
    /// assert_eq!(ApprovalState::parse("approved"), Some(ApprovalState::Approved));
    /// assert_eq!(ApprovalState::Approved.label(), "approved");
    /// assert_eq!(ApprovalState::parse("bogus"), None);
    /// ```
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "requested" => Some(Self::Requested),
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// What an approval record is waiting on. Defaults to worker delegations so
/// old journals/sidecars replay unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalKind {
    /// A `subagent` worker delegation.
    #[default]
    Worker,
    /// A plan submitted through `submit_plan`.
    Plan,
}

impl ApprovalKind {
    /// The wire/label spelling (`worker` or `plan`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Plan => "plan",
        }
    }
}

/// One worker delegation or submitted plan waiting for (or carrying) a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRecord {
    /// Stable id the decision is bound to: approving A never admits task B.
    pub id: String,
    /// The tool-call id that raised the request.
    pub call: String,
    /// Short human-readable description shown in `/approvals`.
    pub summary: String,
    /// Current lifecycle state.
    pub state: ApprovalState,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The full submitted plan text, when `kind == Plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Optional execution-time verification contract, when `kind == Plan`.
    /// Old journals/sidecars without it replay as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// Whether this gates a worker delegation or a plan.
    #[serde(default)]
    pub kind: ApprovalKind,
}

impl ApprovalRecord {
    /// A pending worker-delegation request.
    pub fn requested(id: String, call: String, summary: String) -> Self {
        Self {
            id,
            call,
            summary,
            state: ApprovalState::Requested,
            created_at_ms: now_ms(),
            plan: None,
            verification: None,
            kind: ApprovalKind::Worker,
        }
    }

    /// A pending plan approval carrying the submitted plan text.
    pub fn plan(id: String, call: String, summary: String, plan: String) -> Self {
        Self {
            id,
            call,
            summary,
            state: ApprovalState::Requested,
            created_at_ms: now_ms(),
            plan: Some(plan),
            verification: None,
            kind: ApprovalKind::Plan,
        }
    }

    /// A pending plan approval that also carries an execution-time
    /// verification contract ([`Verification`]).
    pub fn plan_with_verification(
        id: String,
        call: String,
        summary: String,
        plan: String,
        verification: Option<Verification>,
    ) -> Self {
        let mut record = Self::plan(id, call, summary, plan);
        record.verification = verification;
        record
    }

    /// Whether the record is still awaiting a decision.
    pub fn is_pending(&self) -> bool {
        self.state == ApprovalState::Requested
    }

    /// Approvals older than this are stale and must not be decidable. Shared by
    /// the live plugin and the detached CLI so both compute expiry identically.
    pub const MAX_AGE_MS: u64 = 24 * 60 * 60 * 1_000;

    /// Whether this record is a pending request that has aged past the expiry
    /// window at `now`. Never expires a non-pending (decided) record.
    pub fn is_expired_at(&self, now: u64) -> bool {
        self.is_pending() && now.saturating_sub(self.created_at_ms) > Self::MAX_AGE_MS
    }
}

/// Milliseconds since the Unix epoch, or `0` if the system clock is before it.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
