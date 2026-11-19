//! Foreman approval records (durable queue entries).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Lifecycle of one approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApprovalState {
    Requested,
    Approved,
    Denied,
    Expired,
}

impl ApprovalState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
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
pub(crate) enum ApprovalKind {
    #[default]
    Worker,
    Plan,
}

impl ApprovalKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Plan => "plan",
        }
    }
}

/// One worker delegation or submitted plan waiting for (or carrying) a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApprovalRecord {
    pub(crate) id: String,
    pub(crate) call: String,
    pub(crate) summary: String,
    pub(crate) state: ApprovalState,
    pub(crate) created_at_ms: u64,
    /// The full submitted plan text, when `kind == Plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<String>,
    /// Optional execution-time verification contract, when `kind == Plan`.
    /// Old journals/sidecars without it replay as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verification: Option<String>,
    #[serde(default)]
    pub(crate) kind: ApprovalKind,
}

impl ApprovalRecord {
    pub(crate) fn requested(id: String, call: String, summary: String) -> Self {
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

    pub(crate) fn plan(id: String, call: String, summary: String, plan: String) -> Self {
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

    pub(crate) fn plan_with_verification(
        id: String,
        call: String,
        summary: String,
        plan: String,
        verification: Option<String>,
    ) -> Self {
        let mut record = Self::plan(id, call, summary, plan);
        record.verification = verification;
        record
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.state == ApprovalState::Requested
    }

    /// Approvals older than this are stale and must not be decidable. Shared by
    /// the live plugin and the detached CLI so both compute expiry identically.
    pub(crate) const MAX_AGE_MS: u64 = 24 * 60 * 60 * 1_000;

    /// Whether this record is a pending request that has aged past the expiry
    /// window at `now`. Never expires a non-pending (decided) record.
    pub(crate) fn is_expired_at(&self, now: u64) -> bool {
        self.is_pending() && now.saturating_sub(self.created_at_ms) > Self::MAX_AGE_MS
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
