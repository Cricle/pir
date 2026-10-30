//! Regression: startup must create the pi-compatible agent-directory layout.
//!
//! Upstream pi 1.4 creates `<agentDir>/auth.json` (`{}`, 0600) when the
//! credential store is constructed and `<agentDir>/models-store.json` (`{}`,
//! 0600) on the startup catalog refresh (`packages/coding-agent/src/core/
//! auth-storage.ts` `ensureFileExists`, `core/models-store.ts` over the same
//! `FileAuthStorageBackend`). `settings.json` / `models.json` are only written
//! on an explicit persist and must NOT appear on a plain startup, and the
//! `sessions/<encoded-cwd>/` tree belongs to `SessionManager`, not to
//! bootstrap.
//!
//! Before this fix a fresh `HOME` (no upstream `pi` installed, no `~/.pi`)
//! plus `pir --list-models` left only an empty `~/.pi/agent/` behind, so
//! there was no pi-compatible configuration at all. These tests pin the
//! bootstrap contract: create the two files, preserve whatever already exists
//! (contents and modes), follow every agent-dir override, and never fail the
//! process on an IO error.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pir::coding_agent::agent_bootstrap::bootstrap_agent_dir;

/// A scratch directory this module owns and removes on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-agent-bootstrap-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// A missing agent dir gets exactly the files upstream creates on startup,
/// with upstream's creation modes, and none of the files that are only
/// written on first persist.
#[test]
fn bootstrap_creates_the_upstream_files_only() {
    let scratch = ScratchDir::new("layout");
    let agent = scratch.path().join("nested").join("agent");
    assert!(!agent.exists());

    bootstrap_agent_dir(&agent);

    assert!(agent.is_dir(), "agent dir must be created");
    assert_eq!(read(&agent.join("auth.json")), "{}");
    assert_eq!(read(&agent.join("models-store.json")), "{}");
    for absent in ["settings.json", "models.json", "trust.json"] {
        assert!(
            !agent.join(absent).exists(),
            "bootstrap must not invent {absent} (upstream only writes it on demand)"
        );
    }
    assert!(
        !agent.join("sessions").exists(),
        "sessions/<encoded-cwd> belongs to SessionManager, not bootstrap"
    );

    #[cfg(unix)]
    {
        assert_eq!(mode(&agent), 0o700, "agent dir mode (upstream 0o700)");
        assert_eq!(
            mode(&agent.join("auth.json")),
            0o600,
            "auth.json mode (upstream 0o600)"
        );
        assert_eq!(
            mode(&agent.join("models-store.json")),
            0o600,
            "models-store.json mode (upstream 0o600)"
        );
    }

    // Idempotent: a second startup leaves the files alone.
    bootstrap_agent_dir(&agent);
    assert_eq!(read(&agent.join("auth.json")), "{}");
    assert_eq!(read(&agent.join("models-store.json")), "{}");
}

/// Existing files survive byte-for-byte and keep their modes (never overwrite
/// an administrator-managed auth store).
#[test]
fn bootstrap_preserves_existing_files_and_modes() {
    let scratch = ScratchDir::new("preserve");
    let agent = scratch.path().join("agent");
    std::fs::create_dir_all(&agent).expect("create agent dir");

    let auth = agent.join("auth.json");
    let auth_contents = r#"{"test-provider":{"type":"api_key","key":"test-only"}}"#;
    std::fs::write(&auth, auth_contents).expect("write auth.json");
    let models = agent.join("models-store.json");
    let models_contents = r#"{"test-provider":{"models":[]}}"#;
    std::fs::write(&models, models_contents).expect("write models-store.json");
    let settings = agent.join("settings.json");
    std::fs::write(&settings, r#"{"theme":"dark"}"#).expect("write settings.json");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
            .expect("chmod agent dir");
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o644))
            .expect("chmod auth.json");
        std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o640))
            .expect("chmod models-store.json");
    }

    bootstrap_agent_dir(&agent);

    assert_eq!(read(&auth), auth_contents);
    assert_eq!(read(&models), models_contents);
    assert_eq!(read(&settings), r#"{"theme":"dark"}"#);

    #[cfg(unix)]
    {
        assert_eq!(mode(&agent), 0o755, "existing agent dir mode is preserved");
        assert_eq!(mode(&auth), 0o644, "existing auth.json mode is preserved");
        assert_eq!(
            mode(&models),
            0o640,
            "existing models-store.json mode is preserved"
        );
    }
}

/// The bootstrap must not fail (or panic) when the agent dir cannot be
/// created, e.g. a parent component is a regular file / the filesystem is
/// read-only: startup keeps going without configuration.
#[test]
fn bootstrap_is_best_effort_on_io_failure() {
    let scratch = ScratchDir::new("blocked");
    let blocker = scratch.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("write blocker file");

    // Must return normally (a warning is printed, but startup continues).
    bootstrap_agent_dir(&blocker.join("agent"));

    assert!(
        !blocker.join("agent").exists(),
        "no agent dir can exist under a regular file"
    );
}

/// `bootstrap_default_agent_dir` resolves the process agent dir, so every
/// supported override (`PIR_AGENT_DIR`, `PIR_CODING_AGENT_DIR`,
/// `PI_CODING_AGENT_DIR`) is honored.
#[tokio::test]
async fn bootstrap_default_agent_dir_honors_each_override() {
    use crate::common::agent_dir_env_lock;

    let _lock = agent_dir_env_lock().lock().await;
    let vars = [
        "PIR_AGENT_DIR",
        "PIR_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ];
    let _env = EnvVarsGuard::isolate(&vars);
    let scratch = ScratchDir::new("env");

    for (index, var) in vars.iter().enumerate() {
        let agent = scratch.path().join(format!("agent-{index}"));
        std::env::set_var(var, &agent);

        pir::coding_agent::agent_bootstrap::bootstrap_default_agent_dir();

        assert_eq!(
            read(&agent.join("auth.json")),
            "{}",
            "{var} must select the bootstrap target"
        );
        assert_eq!(
            read(&agent.join("models-store.json")),
            "{}",
            "{var} must select the bootstrap target"
        );
        std::env::remove_var(var);
    }
}

/// RAII restore for process env mutations (drop runs while the caller's
/// [`agent_dir_env_lock`](crate::common::agent_dir_env_lock) guard is still
/// alive, because locals drop in reverse declaration order).
struct EnvVarsGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvVarsGuard {
    fn isolate(vars: &[&'static str]) -> Self {
        let saved = vars
            .iter()
            .map(|var| (*var, std::env::var_os(var)))
            .collect();
        for var in vars {
            std::env::remove_var(var);
        }
        Self { saved }
    }
}

impl Drop for EnvVarsGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The bootstrap runs after the legacy-credential migration: writing a fresh
/// empty `auth.json` first would make `migrate_auth_to_auth_json` skip and
/// silently strand `oauth.json` credentials (upstream migrates before the
/// credential store creates the file).
#[test]
fn list_models_migrates_legacy_auth_before_bootstrapping() {
    let scratch = ScratchDir::new("legacy-auth");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create isolated home");
    let agent = scratch.path().join("agent");
    std::fs::create_dir_all(&agent).expect("create agent dir");
    std::fs::write(
        agent.join("oauth.json"),
        r#"{"legacy-provider":{"access":"legacy-access","refresh":"legacy-refresh","expires":9999999999999}}"#,
    )
    .expect("write legacy oauth.json");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .arg("--list-models")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_AGENT_DIR", &agent)
        .env_remove("PIR_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_DIR");
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let output = command.output().expect("pir binary should start");
    assert!(output.status.success(), "got: {output:?}");

    let auth: serde_json::Value =
        serde_json::from_str(&read(&agent.join("auth.json"))).expect("auth.json parses");
    assert_eq!(
        auth["legacy-provider"]["type"], "oauth",
        "legacy oauth.json must be migrated before bootstrap: {auth}"
    );
    assert_eq!(auth["legacy-provider"]["access"], "legacy-access");
    assert!(
        agent.join("oauth.json.migrated").exists(),
        "the consumed legacy source is renamed (upstream behavior)"
    );
    assert_eq!(read(&agent.join("models-store.json")), "{}");
}

/// The actual reported repro: `pir --list-models` in a fresh isolated HOME
/// with no upstream `pi` install must leave the pi-compatible files behind.
#[test]
fn list_models_bootstraps_a_missing_agent_dir() {
    let scratch = ScratchDir::new("cli");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create isolated home");
    let agent = scratch.path().join("missing-agent");
    assert!(!agent.exists());

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .arg("--list-models")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_AGENT_DIR", &agent)
        .env_remove("PIR_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_DIR");
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }

    let output = command.output().expect("pir binary should start");
    assert!(
        output.status.success(),
        "exit={:?}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(read(&agent.join("auth.json")), "{}");
    assert_eq!(read(&agent.join("models-store.json")), "{}");
    assert!(
        !agent.join("sessions").exists(),
        "--list-models uses an in-memory session upstream (no sessions dir)"
    );
    assert!(!agent.join("settings.json").exists());
    assert!(!agent.join("models.json").exists());
    assert!(
        !home.join(".pi").exists(),
        "PIR_AGENT_DIR must win over the HOME default"
    );
}
