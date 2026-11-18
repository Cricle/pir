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
            kind: ApprovalKind::Plan,
        }
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.state == ApprovalState::Requested
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
