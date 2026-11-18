//! Persisted foreman settings.

use serde::{Deserialize, Serialize};

/// How worker delegations are admitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApprovalMode {
    /// Admit workers immediately (the pre-v2 behavior).
    #[default]
    Auto,
    /// Queue a user decision before every worker call.
    Manual,
}

impl ApprovalMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// Where worker tasks execute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RouterMode {
    /// Existing in-process child sessions.
    #[default]
    Inline,
    /// One `pir` process per task.
    Process,
}

impl RouterMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Process => "process",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "inline" => Some(Self::Inline),
            "process" => Some(Self::Process),
            _ => None,
        }
    }
}

/// Foreman-only settings. `Default` is the pre-v2 behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ModeSettings {
    pub(crate) approvals: ApprovalMode,
    pub(crate) router: RouterMode,
    pub(crate) worktree: bool,
}

impl ModeSettings {
    /// Prompt addendum describing the active settings. Only static text is
    /// inserted; user input never becomes part of the system prompt.
    pub(crate) fn prompt_addendum(self) -> String {
        let mut lines = Vec::new();
        if self.worktree {
            lines.push(
                "- Workers run in isolated git worktrees. Their changes are committed and merged \
                 into the main workspace after a successful run. Review the merged workspace, not \
                 the worker transcript alone.",
            );
        }
        if self.router == RouterMode::Process {
            lines.push(
                "- Workers run as separate processes. Each one only sees the tools and model in \
                 its agent definition; no in-process state is shared.",
            );
        }
        if self.approvals == ApprovalMode::Manual {
            lines.push(
                "- Worker delegations may be blocked with an approval id. Ask the user to run \
                 `/approvals` (TUI) or `pir foreman approve <id>` (detached run), then STOP and end \
                 your turn: the approval arrives as a follow-up message that starts a new run. \
                 Never sleep, poll, or keep working while waiting for that decision. Do not \
                 bypass the gate.",
            );
        }
        if lines.is_empty() {
            String::new()
        } else {
            format!(
                "\n<foreman_settings>\n{}\n</foreman_settings>",
                lines.join("\n")
            )
        }
    }
}
