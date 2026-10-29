//! W18-B / env-audit §D11 + §D12: two low-severity env-identity splits.
//!
//! **D11 — the SSH env cache key must track the file actually used.**
//! `env_client::selection` caches one `RemoteExecutionEnv` per configuration,
//! guarded by `configuration_key`. The environment's `known_hosts` file is
//! `PI_ENV_SSH_KNOWN_HOSTS` when set, else the agent dir's own
//! `<agent_dir>/mobile/known_hosts`. Keying on the env var alone left a cached
//! environment writing host keys into the previous agent dir's file after a
//! change; keying on the var *and* the agent dir over-invalidates the other
//! way (an explicit path makes the agent dir irrelevant to the connection).
//! The contract pinned here: **the key changes exactly when the resolved
//! `known_hosts` path changes** — the value-derived key the migration's design
//! rule asks for.
//!
//! **D12 — the crash log writer and the crash notice must name one file.**
//! The panic hooks (`crash_log::install_hook` from `main`, and the TUI's
//! terminal-restoring hook) and the startup/exit notice
//! (`take_unnotified_crash`) must all resolve the agent dir **at call time**.
//! A hook that pinned its installation-time directory decided the record's
//! location by itself — whichever hook runs first writes the record, since
//! `record_panic` deduplicates per thread — so after an agent-dir change a
//! panic landed in A while the notice looked in B. The contract: both sides
//! follow the live dir, so the record written after a change is the one the
//! notice reads.
//!
//! The key tests are pure (the key function is driven through its
//! `configuration_key_for_test` seam, never the process environment). The D12
//! test holds [`crate::common::AgentDirEnvGuard`], the process-wide lock for
//! `PIR_AGENT_DIR` mutations, and drives the real panic hook on a scratch
//! thread so the per-thread dedupe flag starts clear.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pir::coding_agent::crash_log::{
    crash_log_path, install_hook, live_crash_log_path, read_crash_log, take_unnotified_crash,
};
use pir::env_client::configuration_key_for_test;

/// A scratch directory this module owns and removes on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-w18b-{label}-{}-{}",
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// D11: the cache key follows the resolved known_hosts path
// ---------------------------------------------------------------------------

/// The default path: unset `PI_ENV_SSH_KNOWN_HOSTS` means the agent dir's own
/// file, so two agent dirs must produce two keys. This is the defect the audit
/// named — a cached environment must not survive an agent-dir change.
#[test]
fn ssh_key_changes_when_the_default_known_hosts_agent_dir_changes() {
    let dir_a = ScratchDir::new("d11-key-a");
    let dir_b = ScratchDir::new("d11-key-b");
    let values: [(&str, &str); 0] = [];

    let key_a = configuration_key_for_test("example.test", &values, dir_a.path());
    let key_b = configuration_key_for_test("example.test", &values, dir_b.path());

    assert_ne!(
        key_a, key_b,
        "an agent-dir change must invalidate the cached SSH environment: the default \
         known_hosts path moves with it"
    );
    // The key carries the resolved path, not merely something dir-dependent.
    assert!(
        key_a.contains(
            dir_a
                .path()
                .join("mobile")
                .join("known_hosts")
                .to_string_lossy()
                .as_ref()
        ),
        "the key must name the resolved known_hosts path, got {key_a:?}"
    );
}

/// Same configuration, same dir: the key is stable, so a cached environment is
/// reused (the point of the cache).
#[test]
fn ssh_key_is_stable_for_an_unchanged_configuration() {
    let dir = ScratchDir::new("d11-key-stable");
    let values = [("PI_ENV_SSH_USER", "dev"), ("PI_ENV_SSH_PORT", "2222")];

    assert_eq!(
        configuration_key_for_test("example.test", &values, dir.path()),
        configuration_key_for_test("example.test", &values, dir.path()),
    );
}

/// With an explicit `PI_ENV_SSH_KNOWN_HOSTS`, the agent dir no longer feeds the
/// connection, so an agent-dir change must NOT invalidate it. Keying on the raw
/// variable *and* the agent dir would tear down a live connection whose target
/// did not move.
#[test]
fn ssh_key_ignores_the_agent_dir_when_the_path_is_explicit() {
    let dir_a = ScratchDir::new("d11-explicit-a");
    let dir_b = ScratchDir::new("d11-explicit-b");
    let explicit = dir_a.path().join("shared_known_hosts");
    let explicit = explicit.to_string_lossy();
    let values = [("PI_ENV_SSH_KNOWN_HOSTS", explicit.as_ref())];

    assert_eq!(
        configuration_key_for_test("example.test", &values, dir_a.path()),
        configuration_key_for_test("example.test", &values, dir_b.path()),
        "an explicit known_hosts path makes the agent dir irrelevant, so the key must \
         not change with it"
    );
}

/// The resolved path is what the key tracks: changing only
/// `PI_ENV_SSH_KNOWN_HOSTS` (agent dir held fixed) must change the key.
#[test]
fn ssh_key_changes_when_the_explicit_known_hosts_path_changes() {
    let dir = ScratchDir::new("d11-explicit-change");
    let first = ScratchDir::new("d11-explicit-first");
    let second = ScratchDir::new("d11-explicit-second");
    let first_path = first
        .path()
        .join("known_hosts")
        .to_string_lossy()
        .into_owned();
    let second_path = second
        .path()
        .join("known_hosts")
        .to_string_lossy()
        .into_owned();

    let key_first = configuration_key_for_test(
        "example.test",
        &[("PI_ENV_SSH_KNOWN_HOSTS", first_path.as_str())],
        dir.path(),
    );
    let key_second = configuration_key_for_test(
        "example.test",
        &[("PI_ENV_SSH_KNOWN_HOSTS", second_path.as_str())],
        dir.path(),
    );

    assert_ne!(key_first, key_second);
}

/// Every other configuration variable still invalidates the key: the fix must
/// not have dropped a field.
#[test]
fn ssh_key_tracks_the_remaining_configuration_variables() {
    let dir = ScratchDir::new("d11-remaining");
    let base: [(&str, &str); 0] = [];
    let baseline = configuration_key_for_test("example.test", &base, dir.path());

    for (name, value) in [
        ("PI_ENV_SSH_USER", "dev"),
        ("PI_ENV_SSH_PORT", "2222"),
        ("PI_ENV_SSH_KEY", "/tmp/id_ed25519"),
        ("PI_ENV_SSH_PROGRAM", "/usr/bin/ssh"),
        ("PI_ENV_SSH_ALIAS", "pi-env-other"),
        ("PI_ENV_REMOTE_CWD", "/srv/work"),
        ("PI_ENV_REMOTE_SHELL_PATH", "/bin/bash"),
    ] {
        assert_ne!(
            configuration_key_for_test("example.test", &[(name, value)], dir.path()),
            baseline,
            "{name} must be part of the cache key"
        );
    }

    // A different host is a different environment.
    assert_ne!(
        configuration_key_for_test("other.test", &base, dir.path()),
        baseline
    );
}

// ---------------------------------------------------------------------------
// D12: the crash writer and the crash notice follow the live agent dir
// ---------------------------------------------------------------------------

/// The process-global panic hook's type, as `std::panic::take_hook` returns it
/// (upstream spells it as the boxed `PanicHookInfo` closure).
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Restores whatever panic hook was global before a test replaced it, even
/// when the test fails: the hook is process-global, and a merged test binary
/// runs its suites in one process.
struct PanicHookRestore(Option<PanicHook>);

impl Drop for PanicHookRestore {
    fn drop(&mut self) {
        if let Some(hook) = self.0.take() {
            std::panic::set_hook(hook);
        }
    }
}

/// The defect, end to end: the hook is installed while the agent dir is A, the
/// environment then moves to B, a panic fires, and the *notice* — reading the
/// live dir, as startup and exit do — must find that crash under B.
#[tokio::test]
async fn a_panic_after_an_agent_dir_change_is_found_by_the_live_notice() {
    let guard = crate::common::AgentDirEnvGuard::scratch("w18b-d12-a", None).await;
    let dir_a = guard.dir().expect("scratch agent dir").to_path_buf();
    let dir_b = ScratchDir::new("w18b-d12-b");

    // Take the global hook over for the duration: silence the panic output the
    // scratch thread is about to produce, and restore the previous hook on
    // every exit path.
    let _restore = PanicHookRestore(Some(std::panic::take_hook()));
    std::panic::set_hook(Box::new(|_| {}));

    // Install the real hook while the environment points at A.
    install_hook();

    // The agent dir moves; the hook must not have pinned A.
    std::env::set_var("PIR_AGENT_DIR", dir_b.path());

    // Panic on a scratch thread: `record_panic` deduplicates per thread, so a
    // fresh thread has its flag clear.
    // Panic on a scratch thread: `record_panic` deduplicates per thread, so a
    // fresh thread has its flag clear. `catch_unwind` INSIDE the thread keeps
    // the panic from reaching libtest (the harness would report the test
    // itself as failed); the thread therefore returns normally with the
    // caught payload in `Ok`, which is what the assertion below reads.
    let panicked =
        std::thread::spawn(|| std::panic::catch_unwind(|| panic!("w18b-d12 live dir panic")))
            .join()
            .expect("the scratch thread must not panic outside catch_unwind");
    assert!(
        panicked.is_err(),
        "catch_unwind must have observed the scratch panic"
    );

    // The record landed under B (the live dir)...
    let in_b = read_crash_log(&crash_log_path(dir_b.path()));
    assert!(
        in_b.iter()
            .any(|record| record.message.contains("w18b-d12 live dir panic")),
        "the panic must be recorded under the live agent dir, not the install-time one"
    );

    // ...and not under A (the install-time dir).
    assert!(
        read_crash_log(&crash_log_path(&dir_a)).is_empty(),
        "a hook that pinned its install-time dir would have written here"
    );

    // The notice's own resolution finds it and marks it announced.
    let taken = take_unnotified_crash(&live_crash_log_path(), now_ms())
        .expect("the live notice must find the crash the hook just wrote");
    assert!(taken.message.contains("w18b-d12 live dir panic"));
    assert!(
        take_unnotified_crash(&live_crash_log_path(), now_ms()).is_none(),
        "the notice announces a crash once"
    );
}
