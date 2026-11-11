//! Session-scoped read ledger backing the read-before-edit constraint.
//!
//! The ledger remembers, per canonical file path, the file identity observed
//! by the most recent `read` (mtime + size + total line count), which 0-based
//! line ranges were actually delivered to the model, and whether the union of
//! those ranges covers the whole file. `edit`/`write` consult it before
//! mutating an *existing* file:
//!
//! * never read in this session -> [`ReadCheck::NotRead`],
//! * read but not the whole file -> [`ReadCheck::Partial`],
//! * file changed since the read -> [`ReadCheck::Stale`],
//! * fully read and unchanged -> [`ReadCheck::Allowed`].
//!
//! New-file writes (the target does not exist) bypass the check, and contexts
//! without a session environment (bare `ToolContext`, tool-level unit tests)
//! never construct a ledger, so they keep the pre-existing behavior.
//!
//! The mode comes from `PIR_READ_BEFORE_EDIT` and is resolved ONCE when the
//! owning session is built ([`ReadBeforeEditMode::from_env`]); the ledger never
//! re-reads the environment per turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// How strictly the read-before-edit constraint is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBeforeEditMode {
    /// Every mutation is allowed; the ledger is not consulted.
    Off,
    /// A mutation proceeds, but the tool result carries a warning.
    Warn,
    /// A mutation of a file not fully read in this session is refused.
    Enforce,
    /// `efficient` profile: `write` stays strict, but `edit` may proceed with
    /// a warning for a partial/stale read. Exact `oldText` matching remains
    /// the edit operation's own safety check; a never-read file is still
    /// refused.
    EditWarn,
}

impl ReadBeforeEditMode {
    /// Resolve the process-level default: `PIR_READ_BEFORE_EDIT` accepts
    /// `off`, `warn` and `enforce` (case-insensitive; `0`/`false` also mean
    /// off). Unset or unrecognized values fall back to [`Self::Enforce`], the
    /// production default.
    pub fn from_env() -> Self {
        Self::from_env_with_profile(
            crate::config::settings::PerformanceSettings::from_env().profile(),
        )
    }

    /// Resolve the mode from an explicit performance profile. An explicit
    /// `PIR_READ_BEFORE_EDIT` value always wins; otherwise `efficient` uses
    /// [`Self::EditWarn`] and `balanced` keeps [`Self::Enforce`].
    pub fn from_env_with_profile(profile: crate::config::settings::PerformanceProfile) -> Self {
        if let Ok(value) = std::env::var("PIR_READ_BEFORE_EDIT") {
            return Self::parse(Some(&value));
        }
        if profile == crate::config::settings::PerformanceProfile::Efficient {
            Self::EditWarn
        } else {
            Self::Enforce
        }
    }

    /// Parse one `PIR_READ_BEFORE_EDIT` value ([`Self::from_env`]'s rule).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("off" | "0" | "false" | "disabled") => Self::Off,
            Some("warn" | "warning") => Self::Warn,
            Some("edit-warn" | "edit_warn") => Self::EditWarn,
            _ => Self::Enforce,
        }
    }

    /// The canonical spelling (for diagnostics and test assertions).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Enforce => "enforce",
            Self::EditWarn => "edit-warn",
        }
    }
}

/// One file's read history.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Canonical path the entry is keyed on (also stored for diagnostics).
    pub canonical_path: PathBuf,
    /// File modification time observed when the read started.
    pub mtime: SystemTime,
    /// File size observed when the read started.
    pub size: u64,
    /// Total line count reported by the read (`TextScan` semantics: a trailing
    /// newline contributes one synthetic final line).
    pub total_lines: usize,
    /// 0-based, end-exclusive line ranges actually delivered to the model,
    /// sorted and coalesced.
    pub ranges: Vec<(usize, usize)>,
    /// Whether the coalesced ranges cover `0..total_lines`.
    pub full_read: bool,
    /// When this entry was last updated by a read.
    pub last_read: SystemTime,
}

/// The read-before-edit verdict for one existing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCheck {
    /// The file was fully read in this session and has not changed since.
    Allowed,
    /// The file has no read record in this session.
    NotRead,
    /// The file was read, but not over its whole line range.
    Partial,
    /// The file changed (mtime or size) after the last read.
    Stale,
}

/// A mutation gate decision derived from a [`ReadCheck`] plus the ledger mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditGate {
    /// The mutation may proceed silently.
    Allowed,
    /// The mutation may proceed, but the result should carry this warning.
    Warn(String),
    /// The mutation must be refused with this error.
    Blocked(String),
}

/// Session-scoped read ledger, shared through an `Arc` by every turn.
#[derive(Debug)]
pub struct ReadLedger {
    mode: ReadBeforeEditMode,
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

impl ReadLedger {
    /// A ledger with an explicit mode (the session build passes
    /// [`ReadBeforeEditMode::from_env`]).
    pub fn new(mode: ReadBeforeEditMode) -> Self {
        Self {
            mode,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// A ledger whose mode is resolved from the process environment once.
    pub fn from_env() -> Self {
        Self::new(ReadBeforeEditMode::from_env())
    }

    /// A ledger resolved from an explicit performance profile (used by the
    /// session/print hosts, whose profile may come from settings rather than
    /// `PIR_PERF_PROFILE`).
    pub fn from_env_with_profile(profile: crate::config::settings::PerformanceProfile) -> Self {
        Self::new(ReadBeforeEditMode::from_env_with_profile(profile))
    }

    /// The ledger's mode.
    pub fn mode(&self) -> ReadBeforeEditMode {
        self.mode
    }

    /// Whether the constraint is disabled (all mutations allowed).
    pub fn is_off(&self) -> bool {
        self.mode == ReadBeforeEditMode::Off
    }

    /// Record one successful text read.
    ///
    /// `start_line`/`end_line` are 0-based and end-exclusive: the lines the
    /// read response actually delivered. A fresh stat (changed mtime/size or
    /// line count) discards the previous ranges — content the model saw before
    /// the change no longer describes the file.
    pub fn record_read(
        &self,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        total_lines: usize,
        start_line: usize,
        end_line: usize,
    ) {
        let key = canonical_key(path);
        let now = SystemTime::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.entry(key.clone()).or_insert_with(|| Entry {
            canonical_path: key,
            mtime,
            size,
            total_lines,
            ranges: Vec::new(),
            full_read: total_lines == 0,
            last_read: now,
        });
        if entry.mtime != mtime || entry.size != size || entry.total_lines != total_lines {
            entry.mtime = mtime;
            entry.size = size;
            entry.total_lines = total_lines;
            entry.ranges.clear();
            entry.full_read = total_lines == 0;
        }
        let start = start_line.min(total_lines);
        let end = end_line.min(total_lines);
        if end > start {
            entry.ranges.push((start, end));
            entry.ranges.sort_unstable();
            // Coalesce overlapping/adjacent ranges so the coverage test is
            // independent of the order in which chunks were read.
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(entry.ranges.len());
            for (range_start, range_end) in entry.ranges.drain(..) {
                match merged.last_mut() {
                    Some((_, last_end)) if range_start <= *last_end => {
                        *last_end = (*last_end).max(range_end);
                    }
                    _ => merged.push((range_start, range_end)),
                }
            }
            entry.ranges = merged;
        }
        entry.full_read = entry.full_read || covers_whole_file(&entry.ranges, total_lines);
        entry.last_read = now;
    }

    /// Record a successful whole-file write. The model authored the complete
    /// content, so the new file is considered fully read for subsequent
    /// edits; otherwise the next `edit` would demand a redundant full read
    /// just because our own write changed the mtime.
    pub fn record_write(&self, path: &Path, content: &str) {
        let key = canonical_key(path);
        let Ok(metadata) = std::fs::metadata(&key) else {
            return;
        };
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let total_lines = content.lines().count();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.insert(
            key.clone(),
            Entry {
                canonical_path: key,
                mtime,
                size: metadata.len(),
                total_lines,
                ranges: vec![(0, total_lines)],
                full_read: true,
                last_read: SystemTime::now(),
            },
        );
    }

    /// Refresh an entry after a successful `edit`. The previous read coverage
    /// (and therefore `full_read`) is preserved; only the file identity that
    /// the mutation itself changed is refreshed.
    pub fn record_edit(&self, path: &Path) {
        let key = canonical_key(path);
        let Ok(metadata) = std::fs::metadata(&key) else {
            return;
        };
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(&key) {
            entry.mtime = mtime;
            entry.size = metadata.len();
            entry.last_read = SystemTime::now();
        }
    }

    /// Check whether `path` may be mutated by `edit`/`write`.
    ///
    /// The caller only invokes this for an existing target. The path is
    /// canonicalized with the same workspace identity rule the trust store
    /// uses ([`crate::config::trust::normalize_cwd`]); a path that cannot be
    /// canonicalized (e.g. already deleted) keeps its lexical spelling, and a
    /// recorded entry for it is reported stale because its stat no longer
    /// matches.
    pub fn check_for_edit(&self, path: &Path) -> ReadCheck {
        let key = canonical_key(path);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = entries.get_mut(&key) else {
            return ReadCheck::NotRead;
        };
        let metadata = match std::fs::metadata(&key) {
            Ok(metadata) => metadata,
            Err(_) => return ReadCheck::Stale,
        };
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if mtime != entry.mtime || metadata.len() != entry.size {
            return ReadCheck::Stale;
        }
        if entry.full_read {
            ReadCheck::Allowed
        } else {
            ReadCheck::Partial
        }
    }

    /// Apply the ledger's mode to a check for one existing target, producing
    /// the exact gate decision the `edit`/`write` tools act on.
    pub fn gate_for_edit(&self, path: &Path, action: &str) -> EditGate {
        if self.mode == ReadBeforeEditMode::Off {
            return EditGate::Allowed;
        }
        let check = self.check_for_edit(path);
        if check == ReadCheck::Allowed {
            return EditGate::Allowed;
        }
        let reason = match check {
            ReadCheck::NotRead => "the file has not been read in this session; read the whole file first (use the read tool with no offset/limit), then retry".to_string(),
            ReadCheck::Partial => "only part of the file has been read in this session; read the whole file (no offset/limit) so every line is covered, then retry".to_string(),
            ReadCheck::Stale => "the file changed since it was last read (mtime or size differs); read it again, then retry".to_string(),
            ReadCheck::Allowed => unreachable!("Allowed handled above"),
        };
        match self.mode {
            ReadBeforeEditMode::Off => EditGate::Allowed,
            ReadBeforeEditMode::Warn => EditGate::Warn(format!(
                "Warning: cannot {action} '{}' without a full, unchanged read ({reason}). The {action} proceeds because PIR_READ_BEFORE_EDIT=warn.",
                path.display()
            )),
            ReadBeforeEditMode::Enforce => EditGate::Blocked(format!(
                "Cannot {action} '{}': {reason}.",
                path.display()
            )),
            ReadBeforeEditMode::EditWarn => {
                if action == "edit" && check != ReadCheck::NotRead {
                    EditGate::Warn(format!(
                        "Warning: editing '{}' without a full, unchanged read ({reason}). The edit proceeds because the exact oldText match is the safety check; re-read if you need more context.",
                        path.display()
                    ))
                } else {
                    EditGate::Blocked(format!(
                        "Cannot {action} '{}': {reason}.",
                        path.display()
                    ))
                }
            }
        }
    }
}

impl Default for ReadLedger {
    /// The production default: enforcement on, mode resolved from the
    /// environment once at construction.
    fn default() -> Self {
        Self::from_env()
    }
}

/// [`crate::config::trust::normalize_cwd`] applied to a file path: the same
/// canonical identity rule the rest of the workspace uses. This is a pure
/// identity normalization, never a containment check.
fn canonical_key(path: &Path) -> PathBuf {
    crate::config::trust::normalize_cwd(path)
}

/// Whether the sorted, coalesced ranges cover `0..total_lines`.
fn covers_whole_file(ranges: &[(usize, usize)], total_lines: usize) -> bool {
    if total_lines == 0 {
        return true;
    }
    let mut covered_until = 0usize;
    for (start, end) in ranges {
        if *start > covered_until {
            return false;
        }
        covered_until = covered_until.max(*end);
        if covered_until >= total_lines {
            return true;
        }
    }
    false
}
