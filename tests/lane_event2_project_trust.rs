//! Lane EVENT2 regression: `project_trust` / `session_shutdown` fire exactly
//! once per face.
//!
//! Upstream PI emits the sequence `project_trust, session_start, ...,
//! session_shutdown` for one print run. pir used to emit `project_trust`
//! twice (once from the disposable pre-trust bootstrap load, once from the
//! final strict/host load) and a spurious `session_shutdown` when the
//! bootstrap handle was dropped (the actor's mailbox-close tail).
//!
//! These tests pin the fixed contract at the loader seams and through a real
//! CLI `--help-extensions` run (which exercises the same bootstrap + final
//! load pair without needing a model):
//!
//! 1. the bootstrap load dispatches `project_trust` once and its disposal
//!    emits no `session_shutdown`;
//! 2. every final load emits no `project_trust`, while a real shutdown still
//!    emits exactly one `session_shutdown {reason:"quit"}`;
//! 3. the bootstrap decision still grants trust;
//! 4. the native face observes `project_trust` exactly once;
//! 5. the CLI's two-pass startup produces one `project_trust` and no
//!    startup-time `session_shutdown`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::extensions::ExtensionConfig;
use pir::plugins::{
    AgentPlugin, EventInterest, PluginEvent, PluginEventOutcome, PluginFuture, PluginHost,
    PluginInfo,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-lane-event2-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "print".to_owned(),
        ..Default::default()
    }
}

/// Write an extension that appends `name[:reason]` for the requested events.
fn logging_extension(dir: &Path, file: &str, log: &Path, events: &[&str]) -> PathBuf {
    let path = dir.join(file);
    let log_js = format!("{:?}", log.display().to_string());
    let handlers = events
        .iter()
        .map(|name| {
            format!(
                "  pi.on({name:?}, (event) => {{ fs.appendFileSync({log_js}, \
                 {name:?} + (event && event.reason ? \":\" + event.reason : \"\") + \"\\n\"); }});\n"
            )
        })
        .collect::<String>();
    std::fs::write(
        &path,
        format!(
            "import fs from \"node:fs\";\n\
             export default function (pi) {{\n{handlers}}}\n"
        ),
    )
    .expect("write extension");
    path
}

fn read_events(log: &Path) -> Vec<String> {
    match std::fs::read_to_string(log) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read {}: {error}", log.display()),
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Native recorder
// ---------------------------------------------------------------------------

struct RecordingPlugin {
    info: PluginInfo,
    seen: Arc<Mutex<Vec<String>>>,
}

impl RecordingPlugin {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                info: PluginInfo::inline("lane-event2-recorder"),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl AgentPlugin for RecordingPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        EventInterest::List(vec!["project_trust".to_owned()])
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.name.to_string());
            PluginEventOutcome::None
        })
    }
}

// ---------------------------------------------------------------------------
// 1 + 2: loader lifecycle contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bootstrap_pass_emits_project_trust_once_and_disposes_without_shutdown() {
    let temp = TempDir::new("bootstrap-dispose");
    let log = temp.0.join("events.log");
    let ext = logging_extension(
        &temp.0,
        "bootstrap.ts",
        &log,
        &["project_trust", "session_shutdown"],
    );

    let Some(handle) = pir::agent_session::load_extension_runtime_bootstrap(
        std::slice::from_ref(&ext),
        config(&temp.0),
    )
    .await
    else {
        // Build without the JS extension runtime.
        return;
    };

    // The one allowed `project_trust` dispatch has completed before the
    // loader returns; no `session_start`/shutdown belongs to this pass.
    assert_eq!(
        read_events(&log),
        vec!["project_trust".to_owned()],
        "the bootstrap pass must dispatch project_trust exactly once"
    );

    // Dropping the disposable handle closes the actor mailbox. The suppressed
    // tail must NOT look like a real session ending. The readiness flag is a
    // shared AtomicBool probe (a handle clone would keep the mailbox open).
    let ready = handle.ready_flag();
    drop(handle);
    wait_until(
        || !ready.load(std::sync::atomic::Ordering::Relaxed),
        "the bootstrap actor to exit",
    )
    .await;
    assert_eq!(
        read_events(&log),
        vec!["project_trust".to_owned()],
        "disposing the bootstrap runtime must not emit session_shutdown"
    );
}

#[tokio::test]
async fn final_load_emits_no_project_trust_and_real_shutdown_once() {
    let temp = TempDir::new("final-load");
    let log = temp.0.join("events.log");
    let ext = logging_extension(
        &temp.0,
        "final.ts",
        &log,
        &["project_trust", "session_shutdown"],
    );

    let Some(handle) = pir::agent_session::load_extension_runtime_with_catalog(
        std::slice::from_ref(&ext),
        config(&temp.0),
        None,
    )
    .await
    else {
        return;
    };

    assert!(
        read_events(&log).is_empty(),
        "a final host load must not dispatch project_trust: {:?}",
        read_events(&log)
    );

    handle.shutdown().await.expect("explicit shutdown");
    assert_eq!(
        read_events(&log),
        vec!["session_shutdown:quit".to_owned()],
        "a real session teardown emits session_shutdown exactly once"
    );
    handle.shutdown().await.expect("idempotent shutdown");
    assert_eq!(
        read_events(&log),
        vec!["session_shutdown:quit".to_owned()],
        "a second teardown must not re-dispatch session_shutdown"
    );
}

// ---------------------------------------------------------------------------
// 3: the bootstrap decision still changes trust
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bootstrap_pass_still_records_the_project_trust_decision() {
    let temp = TempDir::new("bootstrap-trust");
    let ext = temp.0.join("grant.ts");
    std::fs::write(
        &ext,
        "export default function (pi) { \
         pi.on(\"project_trust\", () => ({ trusted: \"yes\" })); }\n",
    )
    .expect("write extension");

    let Some(handle) = pir::agent_session::load_extension_runtime_bootstrap(
        std::slice::from_ref(&ext),
        config(&temp.0),
    )
    .await
    else {
        return;
    };

    assert_eq!(
        pir::extensions::project_trust::recorded_decision(&temp.0),
        Some(true),
        "the bootstrap handler's grant must be recorded"
    );
    let agent_dir = temp.0.join("agent");
    assert!(
        pir::extensions::project_trust::resolve_project_trust(&temp.0, &agent_dir),
        "every later trust consumer must see the bootstrap decision"
    );
    drop(handle);
}

// ---------------------------------------------------------------------------
// 4: native face
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_face_observes_project_trust_exactly_once() {
    let temp = TempDir::new("native-once");
    let (plugin, seen) = RecordingPlugin::new();
    let host = PluginHost::build(None, vec![Arc::new(plugin) as Arc<dyn AgentPlugin>]);

    pir::agent_session::dispatch_project_trust(None, Some(&host), &temp.0.display().to_string())
        .await;

    assert_eq!(
        *seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["project_trust".to_owned()],
        "the native face must observe project_trust exactly once"
    );
}

// ---------------------------------------------------------------------------
// 5: real CLI two-pass startup
// ---------------------------------------------------------------------------

#[test]
fn cli_help_extensions_dispatches_project_trust_once_and_no_startup_shutdown() {
    let Some(bin) = option_env!("CARGO_BIN_EXE_pir") else {
        return;
    };
    let fixture = TempDir::new("cli-help");
    let project = fixture.0.join("project");
    let agent = fixture.0.join("agent");
    let home = fixture.0.join("home");
    // A trust-requiring project resource makes the CLI run the pre-trust pass.
    std::fs::create_dir_all(project.join(".pi").join("extensions")).expect("project dirs");
    std::fs::create_dir_all(agent.join("extensions")).expect("agent dir");
    std::fs::create_dir_all(&home).expect("home dir");

    let log = fixture.0.join("events.log");
    let ext = logging_extension(
        &fixture.0,
        "cli.ts",
        &log,
        &["project_trust", "session_shutdown"],
    );

    let ext_arg = ext.display().to_string();
    let output = std::process::Command::new(bin)
        .current_dir(&project)
        .args([
            "--help-extensions",
            "--no-extensions",
            "-e",
            ext_arg.as_str(),
        ])
        .env("PIR_AGENT_DIR", &agent)
        .env("PI_CODING_AGENT_DIR", &agent)
        // Hermetic home: no user `.agents/skills`, no real trust store.
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("PIR_TRUST_PROJECT")
        .output()
        .expect("pir binary should start");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        read_events(&log),
        vec!["project_trust".to_owned()],
        "the CLI bootstrap + final loads must emit project_trust exactly once \
         and no startup session_shutdown; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
