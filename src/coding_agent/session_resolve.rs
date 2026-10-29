//! Session flag resolution for the product CLI: `-c/--continue`,
//! `-r/--resume`, `--session`, `--session-id`, `--fork`, and `--no-session`.
//!
//! Mirrors upstream `packages/coding-agent/src/cli/args.ts` (flag names and
//! conflict rules) and `main.ts` `createSessionManager` (resolution order and
//! error messages). Sessions persist as JSONL only.
//!
//! Resolution order (upstream `createSessionManager`): `--no-session` >
//! `--fork` > `--session` > `--resume` > `--continue` > `--session-id` >
//! new session.

use std::path::Path;

use crate::session_manager::SessionManager;

/// The session-related CLI flags (upstream `Args` subset).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFlags {
    /// `--no-session`: ephemeral run, nothing persisted.
    pub no_session: bool,
    /// `-c` / `--continue`: most recent session for the current cwd.
    pub continue_recent: bool,
    /// `-r` / `--resume`: interactive session picker.
    pub resume: bool,
    /// `--session <path|id>`: open a specific session file or (partial) id.
    pub session: Option<String>,
    /// `--session-id <id>`: exact project session id, created when missing.
    pub session_id: Option<String>,
    /// `--fork <path|id>`: fork the given session into a new one.
    pub fork: Option<String>,
}

impl SessionFlags {
    pub fn any(&self) -> bool {
        self.no_session
            || self.continue_recent
            || self.resume
            || self.session.is_some()
            || self.session_id.is_some()
            || self.fork.is_some()
    }
}

/// The resolved session plan for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPlan {
    /// `--no-session`: keep the transcript in memory only. `id` carries an
    /// explicit `--session-id` (recorded but unused: nothing is persisted).
    Ephemeral {
        /// Explicit session id from `--session-id`, when given.
        id: Option<String>,
    },
    /// Fresh session, optionally with an explicit id (`--session-id`).
    New { id: Option<String> },
    /// Open this session file.
    Open(std::path::PathBuf),
    /// Fork `source` into a new session (optionally with an explicit id).
    Fork {
        source: std::path::PathBuf,
        id: Option<String>,
    },
    /// `-r` in interactive mode: the host must show the session picker.
    Pick,
}

/// Whether `arg` looks like a file path rather than a session id (upstream
/// `resolveSessionPath`).
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/') || arg.contains('\\') || arg.ends_with(".jsonl")
}

enum Resolved {
    Found(std::path::PathBuf),
    /// The id matched a session stored under a DIFFERENT project cwd
    /// (upstream `resolveSessionPath` type `"global"`: interactive prompts to
    /// fork; headless pir warns and refuses).
    CrossProject {
        path: std::path::PathBuf,
        cwd: String,
    },
    NotFound(String),
}

/// Whether a session header cwd equals the current cwd (resolved-path
/// comparison, like `SessionManager` discovery).
fn cwd_matches(session_cwd: &str, cwd: &Path) -> bool {
    crate::session_manager::resolve_path_str(session_cwd)
        == crate::session_manager::resolve_path_str(&cwd.to_string_lossy())
}

/// Resolve a `--session`/`--fork` argument: literal path first, then exact id
/// match, then unique id-prefix match against the directory listing (upstream
/// `resolveSessionPath`). Id matches are restricted to sessions whose header
/// cwd matches the current cwd; an id that only matches cross-project
/// sessions resolves to [`Resolved::CrossProject`] instead of silently
/// opening another project's session.
fn resolve_session_ref(arg: &str, cwd: &Path, session_dir: &Path) -> Resolved {
    if looks_like_path(arg) {
        let path = if Path::new(arg).is_absolute() {
            std::path::PathBuf::from(arg)
        } else {
            cwd.join(arg)
        };
        return Resolved::Found(path);
    }
    let sessions = SessionManager::list_sessions(session_dir, None);
    let mut prefix_same: Option<std::path::PathBuf> = None;
    let mut exact_cross: Option<(std::path::PathBuf, String)> = None;
    let mut prefix_cross: Option<(std::path::PathBuf, String)> = None;
    for session in &sessions {
        let same_cwd = cwd_matches(&session.cwd, cwd);
        if session.id == arg {
            if same_cwd {
                // An exact same-project match outranks everything.
                return Resolved::Found(session.path.clone());
            }
            if exact_cross.is_none() {
                exact_cross = Some((session.path.clone(), session.cwd.clone()));
            }
        } else if session.id.starts_with(arg) {
            if same_cwd {
                if prefix_same.is_none() {
                    prefix_same = Some(session.path.clone());
                }
            } else if prefix_cross.is_none() {
                prefix_cross = Some((session.path.clone(), session.cwd.clone()));
            }
        }
    }
    if let Some(path) = prefix_same {
        return Resolved::Found(path);
    }
    if let Some((path, session_cwd)) = exact_cross.or(prefix_cross) {
        return Resolved::CrossProject {
            path,
            cwd: session_cwd,
        };
    }
    Resolved::NotFound(arg.to_owned())
}

/// The notice upstream prints before it offers to fork a session that lives
/// in another project (`main.ts:400`: `Session found in different project:
/// <cwd>`). The interactive host shows the fork confirmation after it; a
/// headless host cannot prompt and forks directly (see
/// [`resolve_session_plan`]).
pub fn cross_project_session_notice(cwd: &str) -> String {
    format!("Session found in different project: {cwd}")
}

/// Reject flag combinations upstream refuses (`validateForkFlags` /
/// `validateSessionIdFlags`).
pub fn validate_flags(flags: &SessionFlags) -> crate::Result<()> {
    if flags.fork.is_some() {
        let mut conflicts: Vec<&str> = Vec::new();
        if flags.session.is_some() {
            conflicts.push("--session");
        }
        if flags.continue_recent {
            conflicts.push("--continue");
        }
        if flags.resume {
            conflicts.push("--resume");
        }
        if flags.no_session {
            conflicts.push("--no-session");
        }
        if !conflicts.is_empty() {
            anyhow::bail!("--fork cannot be combined with {}", conflicts.join(", "));
        }
    }
    if flags.session_id.is_some() {
        let mut conflicts: Vec<&str> = Vec::new();
        if flags.session.is_some() {
            conflicts.push("--session");
        }
        if flags.continue_recent {
            conflicts.push("--continue");
        }
        if flags.resume {
            conflicts.push("--resume");
        }
        if !conflicts.is_empty() {
            anyhow::bail!(
                "--session-id cannot be combined with {}",
                conflicts.join(", ")
            );
        }
    }
    Ok(())
}

/// The error for a `--session`/`--fork` reference that matched no session.
///
/// With the `session` feature on, this is the upstream wording unchanged. With
/// it off no session file can be listed or read at all, so "No session found"
/// would hide the real cause; the typed persistence-disabled text is reported
/// instead.
pub fn session_not_found_message(arg: &str) -> String {
    #[cfg(feature = "session")]
    {
        format!("No session found matching '{arg}'")
    }
    #[cfg(not(feature = "session"))]
    {
        let _ = arg;
        crate::session_manager::SESSION_PERSISTENCE_DISABLED.to_owned()
    }
}

/// The `--session-id <id>` notice for an id no project session matches
/// (upstream warns and creates a new session with that exact id).
///
/// With the `session` feature off nothing is persisted, so the notice must not
/// promise a session file: it says the run stays in memory.
pub fn new_session_id_warning(id: &str) -> String {
    #[cfg(feature = "session")]
    {
        format!(
            "Warning: No project session found with id '{id}'; creating a new session with that id."
        )
    }
    #[cfg(not(feature = "session"))]
    {
        format!(
            "Warning: session persistence is disabled in this build (the `session` cargo feature \
             is off); session id '{id}' will run in memory only."
        )
    }
}

/// Resolve the session flags into a concrete plan for one run.
///
/// `interactive` selects the `--resume` behavior: the interactive host shows
/// the picker ([`SessionPlan::Pick`]); a headless host (print/json/RPC)
/// cannot show a picker, so it resumes the most recent session of the current
/// project — the default row of the interactive picker — and starts a new
/// session when the project has none (the same fallback the interactive host
/// applies when its picker would be empty).
pub fn resolve_session_plan(
    flags: &SessionFlags,
    cwd: &Path,
    session_dir: &Path,
    interactive: bool,
) -> crate::Result<SessionPlan> {
    validate_flags(flags)?;

    if flags.no_session {
        // `--session-id` is recorded but unused: nothing is persisted.
        return Ok(SessionPlan::Ephemeral {
            id: flags.session_id.clone(),
        });
    }

    if let Some(fork) = &flags.fork {
        if let Some(id) = &flags.session_id {
            if SessionManager::find_session_by_id(session_dir, id).is_some() {
                anyhow::bail!("Session already exists with id '{id}'");
            }
        }
        // Forking a cross-project session is allowed (upstream forks it into
        // the current directory without prompting): the fork's header records
        // the CURRENT cwd.
        return match resolve_session_ref(fork, cwd, session_dir) {
            Resolved::Found(path) | Resolved::CrossProject { path, .. } => Ok(SessionPlan::Fork {
                source: path,
                id: flags.session_id.clone(),
            }),
            Resolved::NotFound(arg) => Err(anyhow::anyhow!(session_not_found_message(&arg))),
        };
    }

    if let Some(session) = &flags.session {
        return match resolve_session_ref(session, cwd, session_dir) {
            Resolved::Found(path) => Ok(SessionPlan::Open(path)),
            Resolved::CrossProject { path, cwd: other } => {
                // Upstream warns "Session found in different project: <cwd>"
                // and asks "Fork this session into current directory?": the
                // cross-project session is never opened in place, it is
                // COPIED into the current project (forkSessionOrExit). A
                // headless host cannot ask the question, so the upstream
                // default action (fork, new header cwd = current) runs
                // directly. Only genuinely unresolvable refs stay errors.
                eprintln!("{}", cross_project_session_notice(&other));
                Ok(SessionPlan::Fork {
                    source: path,
                    id: None,
                })
            }
            Resolved::NotFound(arg) => Err(anyhow::anyhow!(session_not_found_message(&arg))),
        };
    }

    if flags.resume {
        if interactive {
            return Ok(SessionPlan::Pick);
        }
        // Headless `-r`: no picker is available, so take the picker's default
        // row — the most recent session of this project (upstream
        // `SessionManager.list` order). With none, fall back to a new session
        // like the interactive host's empty-picker handling.
        let recent = SessionManager::list_sessions(session_dir, Some(&cwd.to_string_lossy()))
            .into_iter()
            .next()
            .map(|summary| summary.path);
        return Ok(match recent {
            Some(path) => SessionPlan::Open(path),
            None => {
                eprintln!("pir: no sessions found for this directory; starting a new session");
                SessionPlan::New { id: None }
            }
        });
    }

    if flags.continue_recent {
        let recent = SessionManager::list_sessions(session_dir, Some(&cwd.to_string_lossy()))
            .into_iter()
            .next()
            .map(|summary| summary.path);
        return Ok(match recent {
            Some(path) => SessionPlan::Open(path),
            None => SessionPlan::New { id: None },
        });
    }

    if let Some(id) = &flags.session_id {
        if let Some(path) = SessionManager::find_session_by_id(session_dir, id) {
            return Ok(SessionPlan::Open(path));
        }
        // Upstream warns and creates a new session with that exact id.
        eprintln!("{}", new_session_id_warning(id));
        return Ok(SessionPlan::New {
            id: Some(id.clone()),
        });
    }

    Ok(SessionPlan::New { id: None })
}

/// Open a session file TAIL-FIRST (`SessionManager::open_paged`): only a
/// byte- and entry-capped tail window becomes resident, the skipped prefix
/// stays on disk.
///
/// Falls back to the full streaming load when the paged open REFUSES the file
/// (a legacy v1/v2 session wants a migration rewrite, a header-less file is
/// rejected by the paged header probe): both cases are about the paged guard,
/// not about the file being unreadable, and the full loader is the correct
/// path for them (it migrates a legacy file in place, exactly the behavior
/// every host had before paging landed). A file the full loader also cannot
/// read keeps the PAGED error, so the refusal text still names the paged mode.
pub fn open_session_tail_first(path: &Path) -> crate::Result<SessionManager> {
    match SessionManager::open_paged(
        path,
        crate::session_manager::DEFAULT_TAIL_PAGE_BYTES,
        crate::session_manager::DEFAULT_TAIL_PAGE_ENTRIES,
    ) {
        Ok(manager) => Ok(manager),
        Err(paged_error) => match SessionManager::open(path) {
            Ok(manager) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %paged_error,
                    "tail-first open refused this session; loaded it fully instead"
                );
                Ok(manager)
            }
            Err(_) => Err(paged_error),
        },
    }
}

/// Widen a session opened TAIL-FIRST back to its full resident transcript.
///
/// The one seam every command that operates on the WHOLE session calls before
/// it reads the resident entry list (`/export`, `/share`, `/bug`, `/tree`,
/// `/fork` listings, `/session`, `/compact`, the RPC read commands): after
/// this, [`SessionManager::entries`] and every projection built from it see
/// the complete history instead of a silently truncated tail window. Returns
/// `false` (and does nothing) for a session that already holds everything —
/// calling it is free on the common non-paged path.
pub fn ensure_full_transcript(manager: &mut SessionManager) -> crate::Result<bool> {
    if !manager.is_paged() {
        return Ok(false);
    }
    manager.widen_to_full()?;
    Ok(true)
}

/// Materialize a resolved plan into session storage (`None` for ephemeral).
pub fn open_session_manager(
    plan: &SessionPlan,
    cwd: &str,
    session_dir: &Path,
) -> crate::Result<Option<SessionManager>> {
    let manager = match plan {
        SessionPlan::Ephemeral { .. } => None,
        SessionPlan::New { id: Some(id) } => {
            Some(SessionManager::create_with_id(cwd, session_dir, id)?)
        }
        SessionPlan::New { id: None } => Some(SessionManager::create(cwd, session_dir)?),
        // The large-session path opens TAIL-FIRST: only a byte- and
        // entry-capped window is parsed, and the compaction hook summarizes
        // the skipped prefix straight from the file
        // (`compaction::compact_paged`). A session that does not need
        // compaction first widens back to the full resident view inside
        // `run_print` (`paged_requires_widening`), so semantics are unchanged
        // for every session that fits. A legacy (v1/v2) file falls back to the
        // full load, which migrates it (see `open_session_tail_first`).
        SessionPlan::Open(path) => Some(open_session_tail_first(path)?),
        // `fork_from_source` reads the whole source verbatim by design (the
        // fork is a new file with the full prefix), so it stays on the full
        // load.
        SessionPlan::Fork { source, id } => Some(SessionManager::fork_from_source(
            source,
            cwd,
            session_dir,
            id.as_deref(),
        )?),
        SessionPlan::Pick => anyhow::bail!("internal error: unresolved session picker plan"),
    };
    Ok(manager)
}
