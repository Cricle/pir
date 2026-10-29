//! D8 (W16-E): the TUI session root is a SOURCE, not a startup string.
//!
//! `docs/archive/env-audit-w14.md` §D8: the TUI captured the CLI-resolved session
//! directory once and never re-resolved it. The default layout is
//! `<agent-dir>/sessions/<encoded-cwd>`, so after a runtime agent-dir change
//! (`/reload`, a session switch in an embedder/test, a settings change) the
//! session picker listed the OLD root while `/new`/`/fork` kept writing there
//! too, even though the rest of the app had moved.
//!
//! The fix keeps [`SessionDirSource`] (the startup CLASSIFICATION) and resolves
//! the path at each session-facing command seam: `/new` and `/fork` in the
//! worker, `/resume` and `/sessions` in the UI. Startup still resolves to the
//! exact CLI value, and resolution is pure path arithmetic — no filesystem
//! access, so the steady-state frame pays nothing.
//!
//! NOTE: registered by the foreman (`#[path = "suites/tui_snapshot_refresh_w16.rs"]`
//! in a `g_*` merged binary). The suite is self-contained (a local
//! `PIR_AGENT_DIR` guard instead of `crate::common`), so it needs no
//! `mod common;`. The steady-state test reads the process-global
//! `session_list_file_reads` probe, so register it in a binary whose other
//! suites do not call `SessionManager::list_sessions` concurrently —
//! `tests/g_tui.rs` today (the only such suites are in `tests/g_agent.rs`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use pir::session_manager::{
    reset_session_list_file_reads, session_list_file_reads, SessionManager,
};
use pir::tui::terminal::{default_session_dir_under, SessionDirSource};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `PIR_AGENT_DIR` is process-global; serialize the tests that move it (the
/// same discipline as `tests/common::AgentDirEnvGuard`, kept local so this
/// suite needs no `mod common;`).
static AGENT_DIR_LOCK: Mutex<()> = Mutex::new(());

struct AgentDirGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Option<OsString>,
}

impl AgentDirGuard {
    fn pin(dir: &Path) -> Self {
        let lock = AGENT_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var_os("PIR_AGENT_DIR");
        std::env::set_var("PIR_AGENT_DIR", dir);
        Self { _lock: lock, saved }
    }

    fn switch_to(&mut self, dir: &Path) {
        std::env::set_var("PIR_AGENT_DIR", dir);
    }
}

impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
            None => std::env::remove_var("PIR_AGENT_DIR"),
        }
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-w16-snapshot-{name}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// An absolute cwd per platform, so `resolve_path_str` (and therefore the
/// encoded directory name) is deterministic without touching the process cwd.
fn test_cwd() -> String {
    if cfg!(windows) {
        r"C:\work\pir".to_string()
    } else {
        "/work/pir".to_string()
    }
}

/// The default root a run under `agent_dir` resolves at startup.
fn startup_root(agent_dir: &Path) -> String {
    default_session_dir_under(agent_dir, &test_cwd())
}

// ---------------------------------------------------------------------------
// (a) A switch re-resolves the session-facing root
// ---------------------------------------------------------------------------

#[test]
fn default_root_follows_the_agent_dir_at_every_session_command() {
    let cwd = test_cwd();
    let agent_a = temp_dir("a");
    let agent_b = temp_dir("b");

    // Startup: the CLI hands over the default-layout path; it classifies as
    // the re-resolvable default, and re-resolving it now is a no-op (startup
    // behaviour is identical to the pre-fix string).
    let startup = startup_root(&agent_a);
    let source = SessionDirSource::from_startup(&startup, &cwd, None, None, &agent_a);
    assert_eq!(source, SessionDirSource::AgentDirDefault);
    assert_eq!(source.resolve_with(&agent_a, &cwd), startup);

    // The agent dir moves (embedder/test/reload). Every session-facing command
    // resolves the source again and now sees the NEW root.
    let moved = source.resolve_with(&agent_b, &cwd);
    assert_ne!(moved, startup);
    assert_eq!(moved, default_session_dir_under(&agent_b, &cwd));
    assert!(
        Path::new(&moved).starts_with(agent_b.join("sessions")),
        "the moved root must live under the new agent dir's sessions tree: {moved}"
    );
    assert!(
        !Path::new(&moved).starts_with(agent_a.as_path()),
        "the moved root must not stay under the old agent dir: {moved}"
    );
}

#[test]
fn a_new_session_after_the_switch_is_created_under_the_new_root() {
    let cwd = test_cwd();
    let agent_a = temp_dir("new-a");
    let agent_b = temp_dir("new-b");
    let source =
        SessionDirSource::from_startup(&startup_root(&agent_a), &cwd, None, None, &agent_a);

    // `/new` re-resolves before creating (worker `WorkerCommand::NewSession`).
    let root_b = source.resolve_with(&agent_b, &cwd);
    let manager = SessionManager::create(&cwd, &root_b).expect("create session under the new root");
    let path = manager
        .session_file()
        .expect("a persisted session has a file")
        .to_path_buf();
    assert!(
        path.starts_with(agent_b.join("sessions")),
        "new session file must land under the new root, got {}",
        path.display()
    );
    assert!(
        !path.starts_with(agent_a.join("sessions")),
        "new session file must not land under the old root, got {}",
        path.display()
    );
    // The new root is exactly where the manager put the file, and the root
    // directory now exists (the picker scans it): `SessionManager::create`
    // materializes the directory, the file lands on the first append.
    assert_eq!(
        path.parent(),
        Some(Path::new(&root_b)),
        "the manager must hold the file under the re-resolved root"
    );
    assert!(
        Path::new(&root_b).is_dir(),
        "`/new` must have materialized the new root directory"
    );
    assert!(
        !Path::new(&startup_root(&agent_a)).exists(),
        "the old root must stay untouched (no directory was created there)"
    );
}

#[test]
fn resolve_follows_the_live_agent_dir_environment() {
    let cwd = test_cwd();
    let agent_a = temp_dir("env-a");
    let agent_b = temp_dir("env-b");
    let mut guard = AgentDirGuard::pin(&agent_a);

    let startup = startup_root(&agent_a);
    let source = SessionDirSource::from_startup(&startup, &cwd, None, None, &agent_a);
    assert_eq!(source.resolve(&cwd), startup);

    guard.switch_to(&agent_b);
    let after = source.resolve(&cwd);
    assert_eq!(after, default_session_dir_under(&agent_b, &cwd));
    assert_ne!(after, startup);
}

// ---------------------------------------------------------------------------
// (b) Process-fixed fields stay put
// ---------------------------------------------------------------------------

#[test]
fn explicit_startup_overrides_are_pinned_across_a_switch() {
    let cwd = test_cwd();
    let agent_a = temp_dir("pin-a");
    let agent_b = temp_dir("pin-b");

    // 1. `--session-dir <path>`: the TUI cannot see the flag, so a startup
    //    value the chain cannot reproduce is classified pinned.
    let cli_choice = temp_dir("cli-session-dir").join("sessions");
    let cli_text = cli_choice.to_string_lossy().into_owned();
    let source = SessionDirSource::from_startup(&cli_text, &cwd, None, None, &agent_a);
    assert_eq!(source, SessionDirSource::Pinned(cli_text.clone()));
    assert_eq!(source.resolve_with(&agent_b, &cwd), cli_text);

    // 2. settings `sessionDir`.
    let settings_choice = temp_dir("settings-session-dir").join("sessions");
    let settings_text = settings_choice.to_string_lossy().into_owned();
    let source =
        SessionDirSource::from_startup(&settings_text, &cwd, Some(&settings_text), None, &agent_a);
    assert!(matches!(&source, SessionDirSource::Pinned(_)));
    assert_eq!(source.resolve_with(&agent_b, &cwd), settings_text);

    // 3. the session-dir env override: explicit configuration even when the
    //    value happens to equal the default layout for this run.
    let env_value = startup_root(&agent_a);
    let source = SessionDirSource::from_startup(&env_value, &cwd, None, Some(&env_value), &agent_a);
    assert!(matches!(&source, SessionDirSource::Pinned(_)));
    assert_eq!(source.resolve_with(&agent_b, &cwd), env_value);
}

#[test]
fn pinned_root_ignores_a_live_agent_dir_switch() {
    let cwd = test_cwd();
    let agent_a = temp_dir("pinned-env-a");
    let agent_b = temp_dir("pinned-env-b");
    let mut guard = AgentDirGuard::pin(&agent_a);

    let cli_choice = temp_dir("pinned-env-cli").join("sessions");
    let cli_text = cli_choice.to_string_lossy().into_owned();
    let source = SessionDirSource::from_startup(&cli_text, &cwd, None, None, &agent_a);

    guard.switch_to(&agent_b);
    assert_eq!(
        source.resolve(&cwd),
        cli_text,
        "an explicit `--session-dir` must not follow the agent dir"
    );
}

#[test]
fn default_layout_matches_the_session_manager_resolution() {
    // The TUI's explicit-agent-dir derivation must stay byte-identical to
    // `session_manager::default_session_dir_path` under the same agent dir,
    // so startup keeps today's path and the pickers list the same tree.
    let cwd = test_cwd();
    let agent_dir = temp_dir("layout");
    let guard = AgentDirGuard::pin(&agent_dir);
    let shared = pir::session_manager::default_session_dir_path(&cwd)
        .to_string_lossy()
        .into_owned();
    assert_eq!(default_session_dir_under(&agent_dir, &cwd), shared);
    drop(guard);
}

// ---------------------------------------------------------------------------
// (c) A steady frame performs no extra disk work
// ---------------------------------------------------------------------------

#[test]
fn steady_state_resolution_does_no_disk_work() {
    let cwd = test_cwd();
    // An agent dir that does NOT exist: resolution must not create it.
    let agent = temp_dir("steady").join("never-used");
    assert!(!agent.exists(), "fixture must not exist yet");

    // Startup under that agent dir classifies as the re-resolvable default.
    let source = SessionDirSource::from_startup(
        &default_session_dir_under(&agent, &cwd),
        &cwd,
        None,
        None,
        &agent,
    );
    assert_eq!(source, SessionDirSource::AgentDirDefault);

    // Resolution is what the session-facing seam runs; the frame path never
    // calls it. Pinning both halves: 1024 resolutions (far more than a frame
    // budget would ever do) touch no session file and create nothing on disk.
    reset_session_list_file_reads();
    let mut last = String::new();
    for _ in 0..1024 {
        last = source.resolve_with(&agent, &cwd);
    }
    assert_eq!(
        session_list_file_reads(),
        0,
        "re-resolving the session root must not open session files"
    );
    assert!(
        !agent.exists(),
        "resolution must not create the session directory (only `/new` does)"
    );
    assert_eq!(last, default_session_dir_under(&agent, &cwd));

    // A pinned root is cheaper still: it returns the startup string verbatim.
    let pinned = SessionDirSource::Pinned("C:/pinned".to_string());
    for _ in 0..1024 {
        assert_eq!(pinned.resolve_with(&agent, &cwd), "C:/pinned");
    }
    assert!(
        !agent.exists(),
        "a pinned root must not touch the filesystem at all"
    );
}
