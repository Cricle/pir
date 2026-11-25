//! Persisted foreman settings.

use serde::{Deserialize, Serialize};

/// How worker delegations are admitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Admit workers immediately (the pre-v2 behavior).
    #[default]
    Auto,
    /// Queue a user decision before every worker call.
    Manual,
}

impl ApprovalMode {
    /// The `/foreman approvals <mode>` spelling.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    /// Parse the value the user typed after `/foreman approvals`.
    ///
    /// ```
    /// use pir_builtin_mode::ApprovalMode;
    ///
    /// assert_eq!(ApprovalMode::parse("manual"), Some(ApprovalMode::Manual));
    /// assert_eq!(ApprovalMode::default(), ApprovalMode::Auto);
    /// assert_eq!(ApprovalMode::parse("sometimes"), None);
    /// ```
    pub fn parse(value: &str) -> Option<Self> {
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
pub enum RouterMode {
    /// Existing in-process child sessions.
    #[default]
    Inline,
    /// One `pir` process per task.
    Process,
}

impl RouterMode {
    /// The `/foreman router <mode>` spelling.
    pub fn label(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Process => "process",
        }
    }

    /// Parse the value the user typed after `/foreman router`.
    ///
    /// ```
    /// use pir_builtin_mode::RouterMode;
    ///
    /// assert_eq!(RouterMode::parse("process"), Some(RouterMode::Process));
    /// assert_eq!(RouterMode::default(), RouterMode::Inline);
    /// assert_eq!(RouterMode::parse("docker"), None);
    /// ```
    pub fn parse(value: &str) -> Option<Self> {
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
pub struct ModeSettings {
    pub approvals: ApprovalMode,
    pub router: RouterMode,
    pub worktree: bool,
}

impl ModeSettings {
    /// Prompt addendum describing the active settings. Only static text is
    /// inserted; user input never becomes part of the system prompt.
    pub fn prompt_addendum(self) -> String {
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
