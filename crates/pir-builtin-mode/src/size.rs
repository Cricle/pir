//! Task sizing: a pure function of inputs that exist before turn one.
//!
//! This is D2a. It deliberately does **not** ask a model "how hard is this?":
//! a self-assessment is unmeasurable, and the whole point of the lane decision
//! is that it can be a regression test pinned to real tasks.
//!
//! Two design rules, both from measurements:
//!
//! 1. **Token count alone is not a sizing signal.** `RESULTS.md` records
//!    `c3-two-turn` at 24,954 tokens over 9 turns — cheaper than the large
//!    `l1-ledger` (219,700) yet a plan→implement task by *shape*. A rule that
//!    ranks on volume calls it small, so prompt shape is an input here.
//! 2. **Every threshold is a named constant with its own test row**, so a
//!    future change to one is visible in the fixture table rather than buried
//!    in a comparison.
//!
//! The third input the full design wants — "this task already failed once" —
//! reads the durable task record's attempt ledger and is D2b, shipping after
//! B1. It is an `Option` field here so that stage is additive, not a rewrite.

use serde::{Deserialize, Serialize};

/// A named file the prompt referred to (`@path`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskInputs {
    /// Paths the user named with `@`. Each is usually the whole scope.
    pub named_files: Vec<String>,
    /// Distinct modules the workspace has, bounded by the caller (a `find`
    /// over the cwd). `0` means "not measured" — treated as unknown, not tiny.
    pub touched_modules: usize,
    /// Shape read from the prompt text itself.
    pub prompt_shape: PromptShape,
    /// Failed attempts recorded for this task. `None` until D2b (B1) supplies
    /// the attempt ledger.
    pub failed_attempts: Option<u32>,
}

/// What the prompt asks for, in terms that need no model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptShape {
    /// How many distinct deliverables the prompt names ("fix X and Y", a
    /// numbered/bulleted list). `0` when the prompt is a single sentence.
    pub deliverables: usize,
    /// The prompt spells out an ordered sequence ("first … then …", "1. … 2. …").
    pub sequential: bool,
}

impl PromptShape {
    /// Read a prompt. Deliberately crude and cheap: the failure mode of a
    /// cleverer heuristic is a rule nobody can explain in the ledger.
    pub fn from_prompt(prompt: &str) -> Self {
        let lower = prompt.to_ascii_lowercase();
        let lines = prompt.lines().count();
        let bullets = prompt
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("- ")
                    || trimmed.starts_with("* ")
                    || trimmed
                        .split_once('.')
                        .is_some_and(|(head, _)| head.chars().all(|c| c.is_ascii_digit()))
            })
            .count();
        // A comma-joined enumeration ("fix A, B and C") is the same signal as a
        // bulleted list, without the formatting: each " and " joins two items,
        // so N joins means N+1 deliverables. Only read on a single line, where
        // there is no list structure to be confused with.
        let joined = lower.matches(" and ").count();
        let enumerated = if joined > 0 { joined + 1 } else { 0 };
        let deliverables = if bullets > 0 {
            bullets
        } else if lines == 1 {
            enumerated
        } else {
            0
        };
        let sequential = lower.contains(" then ")
            || lower.contains("after that")
            || lower.contains("first ")
            || bullets >= 2;
        Self {
            deliverables,
            sequential,
        }
    }
}

/// How much machinery a task warrants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskSize {
    Small,
    Medium,
    Large,
}

impl TaskSize {
    /// One line for the task ledger. A wrong guess must be explainable, so the
    /// reason is data rather than a log line someone has to reconstruct.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Small => "one deliverable, no sequence, few named files",
            Self::Medium => "several deliverables, or a sequential prompt",
            Self::Large => "many deliverables, a long sequence, or a retry",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

/// Thresholds, each justified by the `RESULTS.md` fixture it must classify.
pub mod thresholds {
    /// `>` this many modules means the repo is not a single-file fix.
    pub const MODULES_LARGE: usize = 20;
    /// `>=` this many named files stops being a one-file change.
    pub const NAMED_FILES_MEDIUM: usize = 2;
    /// `>=` this many deliverables is a multi-part task.
    pub const DELIVERABLES_MEDIUM: usize = 2;
    /// `>=` this many deliverables is large even without a sequence.
    pub const DELIVERABLES_LARGE: usize = 4;
    /// `>=` this many failed attempts escalates regardless of shape.
    pub const FAILED_ATTEMPTS_LARGE: u32 = 1;
}

/// Classify a task. Pure: same inputs, same answer, no environment, no model.
///
/// ```
/// use pir_builtin_mode::size::{size_task, PromptShape, TaskInputs, TaskSize};
///
/// let one = TaskInputs {
///     named_files: vec!["src/duration.rs".into()],
///     touched_modules: 3,
///     prompt_shape: PromptShape::default(),
///     failed_attempts: None,
/// };
/// assert_eq!(size_task(&one), TaskSize::Small);
/// ```
pub fn size_task(input: &TaskInputs) -> TaskSize {
    use thresholds::*;

    // A retry is never small again: the task already defeated one attempt.
    if input
        .failed_attempts
        .is_some_and(|failed| failed >= FAILED_ATTEMPTS_LARGE)
    {
        return TaskSize::Large;
    }

    let shape = input.prompt_shape;
    let named = input.named_files.len();

    // Large needs more than "several ordered steps": that is exactly the
    // `c3-two-turn` shape (two ordered steps, 24,954 tokens) and it must stay
    // Medium. Large is reserved for a wide surface — many deliverables, or a
    // repo too big to be a focused change.
    if shape.deliverables >= DELIVERABLES_LARGE || input.touched_modules > MODULES_LARGE {
        return TaskSize::Large;
    }

    if shape.deliverables >= DELIVERABLES_MEDIUM || shape.sequential || named >= NAMED_FILES_MEDIUM
    {
        return TaskSize::Medium;
    }

    TaskSize::Small
}
