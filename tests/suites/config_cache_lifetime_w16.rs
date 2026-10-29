//! W16-D: the two config-derived process-wide caches and their lifetime
//! (env audit §D6 and §D7).
//!
//! Before this lane both caches were frozen for the process lifetime:
//!
//! * **D6** — `install_telemetry_enabled` (`src/ai/provider.rs`) stored the
//!   settings-derived install-telemetry flag in a process `AtomicU8` on the
//!   first call, so switching the agent dir (or editing its `settings.json`)
//!   kept serving the first value.
//! * **D7** — the `!command` result cache
//!   (`src/ai/providers/config_value.rs`) was keyed by the command string
//!   alone and never cleared in product, so a command whose output depends on
//!   the agent dir kept replaying dir A's value in dir B.
//!
//! Both are now keyed by their config source and validated with a cheap
//! `metadata` stamp (length + mtime), never a per-call read-and-parse:
//!
//! * D6 caches one entry for the resolved agent dir, valid while that dir's
//!   `settings.json` stamp is unchanged
//!   ([`pir::ai::provider::install_telemetry_enabled`]);
//! * D7 caches up to `COMMAND_CACHE_CAPACITY` `(agent dir, command)` entries,
//!   each valid while the agent dir's `models.json` / `mcp.json` / `auth.json`
//!   stamps are unchanged
//!   ([`pir::ai::providers::config_value::resolve_config_value`]).
//!
//! Covered here:
//!
//! * **(a)** the telemetry gate reflects a setting changed after the first
//!   read — both an agent-dir switch and an in-place `settings.json` edit;
//! * **(b)** a `!command` value refreshes when its config changes — the agent
//!   dir switch and an agent-dir config edit;
//! * **(c)** neither cache grows without bound across many revisions.
//!
//! Hermetic: every test drives the explicit-config-source entry points
//! (`install_telemetry_enabled_in`, `resolve_config_value_in`) with its own
//! temp agent dir, so no test here depends on `PIR_AGENT_DIR`, cwd, or a real
//! `~/.pi`. The telemetry tests pin `PI_TELEMETRY` at "unset" through
//! [`NoTelemetryOverride`] so a developer shell's override cannot decide the
//! gate. The env-reading wrappers are the same functions with the
//! process-resolved dir plugged in, so these also pin that a host bound to one
//! dir never serves another dir's values.

use std::path::{Path, PathBuf};

use pir::ai::providers::config_value::{
    clear_config_value_cache, command_cache_len, resolve_config_value_in,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Serializes this suite: every test observes the same two process-wide caches,
/// and `cargo test` runs the tests of one binary on parallel threads. The locks
/// inside the caches keep them consistent; this lock keeps this suite's exact
/// entry-count assertions meaningful. No other suite in `tests/g_ai.rs` resolves
/// a `!command` value or calls the telemetry gate, so the counts below start
/// from an emptied cache and stay exact.
static SUITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`SUITE_LOCK`], recovering from a poisoned lock (a failed test must not
/// cascade into "poisoned" failures for the rest of the suite).
fn suite_guard() -> std::sync::MutexGuard<'static, ()> {
    SUITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Pin `PI_TELEMETRY` at the empty string (an unrecognized value, so
/// [`telemetry_override`](pir::config::env::telemetry_override) reports "no
/// override") and restore the caller's value on drop. Without this, a
/// developer shell that exports `PI_TELEMETRY` would decide the gate instead of
/// the fixture. Safe against the rest of this binary: no other suite in
/// `tests/g_ai.rs` reads `PI_TELEMETRY` (`tests/telemetry_cache.rs` is its own
/// binary and child process).
struct NoTelemetryOverride(Option<std::ffi::OsString>);

impl NoTelemetryOverride {
    fn new() -> Self {
        let previous = std::env::var_os("PI_TELEMETRY");
        std::env::set_var("PI_TELEMETRY", "");
        Self(previous)
    }
}

impl Drop for NoTelemetryOverride {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("PI_TELEMETRY", value),
            None => std::env::remove_var("PI_TELEMETRY"),
        }
    }
}

/// Write `<dir>/settings.json` with an explicit `enableInstallTelemetry`.
fn write_telemetry_setting(dir: &Path, enabled: bool) {
    std::fs::create_dir_all(dir).expect("create agent dir");
    let body = format!(r#"{{"enableInstallTelemetry": {enabled}}}"#);
    std::fs::write(dir.join("settings.json"), body).expect("write settings.json");
}

/// A `!command` whose output is the content of `<dir>/value.txt` (so the same
/// command string yields a different value per agent dir).
fn read_value_command(dir: &Path) -> String {
    let path = dir.join("value.txt");
    // The config-value executor uses the SAME shell the bash tool does
    // (`resolve_shell_config`: Git Bash on Windows, `/bin/bash` or `sh`
    // elsewhere) and only falls back to `cmd /C` when no bash can be started
    // (upstream `executeCommandUncached`, resolve-config-value.ts:198-206).
    // The command text therefore has to match whichever shell this host
    // resolves, or the test would be asserting cmd semantics against a bash
    // execution.
    let bash = pir::coding_agent::tools::resolve_shell_config(None).is_ok();
    if cfg!(windows) && !bash {
        // `cmd`'s `type`. Unquoted on purpose: `type "C:\...\value.txt"`
        // fails under `cmd /C` (the argument encoder turns the inner quotes
        // into `\"`, which cmd does not accept as an escape), while the
        // scratch directories under `%TEMP%` contain no spaces.
        format!("!type {}", path.display())
    } else {
        // POSIX `cat`; Windows paths are forwarded with forward slashes so
        // Git Bash does not read the backslashes as escapes.
        format!("!cat \"{}\"", path.to_string_lossy().replace('\\', "/"))
    }
}

/// A unique temporary directory removed on drop.
struct ScratchRoot(PathBuf);

impl ScratchRoot {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-w16d-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch root");
        Self(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// (a) The install-telemetry gate follows the setting after the first read
// ---------------------------------------------------------------------------

#[test]
fn telemetry_gate_follows_an_agent_dir_change() {
    let _suite = suite_guard();
    let _telemetry = NoTelemetryOverride::new();
    let root = ScratchRoot::new("telemetry-dir");
    let off_dir = root.join("off");
    let on_dir = root.join("on");
    write_telemetry_setting(&off_dir, false);
    write_telemetry_setting(&on_dir, true);

    // First read populates the cache from the "off" agent dir.
    assert!(
        !pir::ai::provider::install_telemetry_enabled_in(&off_dir),
        "settings in the first agent dir disable install telemetry"
    );

    // Different agent dir: the gate must be re-derived from the new dir's
    // settings, not replayed from the first read (the D6 defect).
    assert!(
        pir::ai::provider::install_telemetry_enabled_in(&on_dir),
        "the gate must follow the resolved agent dir"
    );

    // And back: the previously seen dir is re-derived, not remembered.
    assert!(!pir::ai::provider::install_telemetry_enabled_in(&off_dir));
}

#[test]
fn telemetry_gate_follows_an_in_place_settings_edit() {
    let _suite = suite_guard();
    let _telemetry = NoTelemetryOverride::new();
    let root = ScratchRoot::new("telemetry-edit");
    let dir = root.join("agent");
    write_telemetry_setting(&dir, false);

    assert!(!pir::ai::provider::install_telemetry_enabled_in(&dir));

    // Rewrite the SAME settings file with a different value and a different
    // length: the cached entry's stamp no longer matches, so the next call
    // re-reads it.
    std::fs::write(
        dir.join("settings.json"),
        r#"{"enableInstallTelemetry": true }
"#,
    )
    .expect("rewrite settings.json");
    assert!(
        pir::ai::provider::install_telemetry_enabled_in(&dir),
        "a settings edit after the first read must take effect"
    );
    assert_eq!(
        pir::ai::provider::install_telemetry_cache_len(),
        1,
        "an edit replaces the single cached entry instead of adding one"
    );

    // Deleting the file is a config change too (back to the default, on).
    std::fs::write(
        dir.join("settings.json"),
        r#"{"enableInstallTelemetry": false}"#,
    )
    .expect("rewrite settings.json");
    assert!(!pir::ai::provider::install_telemetry_enabled_in(&dir));
    std::fs::remove_file(dir.join("settings.json")).expect("remove settings.json");
    assert!(
        pir::ai::provider::install_telemetry_enabled_in(&dir),
        "a missing settings file falls back to the default (telemetry on)"
    );
}

// ---------------------------------------------------------------------------
// (b) A `!command` value refreshes when its config changes
// ---------------------------------------------------------------------------

#[test]
fn command_value_follows_an_agent_dir_change() {
    let _suite = suite_guard();
    let root = ScratchRoot::new("command-dir");
    let first = root.join("first");
    let second = root.join("second");
    for (dir, value) in [(&first, "one"), (&second, "two")] {
        std::fs::create_dir_all(dir).expect("create agent dir");
        std::fs::write(dir.join("models.json"), r#"{"providers":{}}"#).expect("write models.json");
        std::fs::write(dir.join("value.txt"), value).expect("write value.txt");
    }
    let command = read_value_command(&first);
    let command_second = read_value_command(&second);

    // No other suite in this binary resolves a `!command` value, so an emptied
    // cache makes the counts below deterministic.
    clear_config_value_cache();
    assert_eq!(
        resolve_config_value_in(&first, &command, None).as_deref(),
        Some("one")
    );
    assert_eq!(command_cache_len(), 1, "the first resolution caches once");

    // The same command string under another agent dir must be a MISS, not a
    // replay of the first dir's cached output (the D7 defect). The key
    // carries the resolved agent dir, so both entries coexist.
    assert_eq!(
        resolve_config_value_in(&second, &command_second, None).as_deref(),
        Some("two"),
        "a command value must not be served from another agent dir's cache"
    );
    assert_eq!(
        resolve_config_value_in(&second, &command, None).as_deref(),
        Some("one"),
        "the same command string under the other dir re-runs"
    );
    assert_eq!(
        command_cache_len(),
        3,
        "entries are keyed by (agent dir, command), not by command alone"
    );

    // An unchanged config source still hits: no new entry.
    assert_eq!(
        resolve_config_value_in(&second, &command, None).as_deref(),
        Some("one")
    );
    assert_eq!(command_cache_len(), 3);
    clear_config_value_cache();
}

#[test]
fn command_value_refreshes_when_its_config_file_changes() {
    let _suite = suite_guard();
    let root = ScratchRoot::new("command-config-edit");
    let dir = root.join("agent");
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(dir.join("models.json"), r#"{"providers":{}}"#).expect("write models.json");
    std::fs::write(dir.join("value.txt"), "before").expect("write value.txt");
    let command = read_value_command(&dir);

    clear_config_value_cache();
    assert_eq!(
        resolve_config_value_in(&dir, &command, None).as_deref(),
        Some("before")
    );

    // Edit an agent-dir config source (a different length, so the stamp
    // changes) and the cached command output must be re-derived.
    std::fs::write(dir.join("value.txt"), "after-edit").expect("rewrite value.txt");
    std::fs::write(
        dir.join("models.json"),
        "{\"providers\":{},\"defaultProvider\":null}",
    )
    .expect("rewrite models.json");
    assert_eq!(
        resolve_config_value_in(&dir, &command, None).as_deref(),
        Some("after-edit"),
        "an agent-dir config edit must invalidate the cached command value"
    );

    // `auth.json` is a config source too.
    std::fs::write(dir.join("value.txt"), "after-auth").expect("rewrite value.txt");
    std::fs::write(dir.join("auth.json"), r#"{"acme":{"type":"api_key"}}"#)
        .expect("write auth.json");
    assert_eq!(
        resolve_config_value_in(&dir, &command, None).as_deref(),
        Some("after-auth")
    );

    // `mcp.json` as well.
    std::fs::write(dir.join("value.txt"), "after-mcp").expect("rewrite value.txt");
    std::fs::write(dir.join("mcp.json"), r#"{"mcpServers":{}}"#).expect("write mcp.json");
    assert_eq!(
        resolve_config_value_in(&dir, &command, None).as_deref(),
        Some("after-mcp")
    );
    clear_config_value_cache();
}

#[test]
fn unchanged_config_still_hits_the_command_cache() {
    let _suite = suite_guard();
    let root = ScratchRoot::new("command-hit");
    let dir = root.join("agent");
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(dir.join("models.json"), r#"{"providers":{}}"#).expect("write models.json");
    // The command appends a marker per run, so a second run is observable.
    let marker = dir.join("runs.txt");
    #[cfg(windows)]
    let command = format!("echo run>>\"{}\" & echo stable", marker.display());
    #[cfg(not(windows))]
    let command = format!("echo run >> \"{}\"; echo stable", marker.display());
    let config = format!("!{command}");

    clear_config_value_cache();
    assert_eq!(
        resolve_config_value_in(&dir, &config, None).as_deref(),
        Some("stable")
    );
    assert_eq!(
        resolve_config_value_in(&dir, &config, None).as_deref(),
        Some("stable")
    );

    let runs = std::fs::read_to_string(&marker).expect("command ran");
    assert_eq!(
        runs.matches("run").count(),
        1,
        "an unchanged config source must keep serving the cached value \
         (the fast path is a stamp comparison, not a re-run)"
    );
    clear_config_value_cache();
}

// ---------------------------------------------------------------------------
// (c) Neither cache grows without bound across many revisions
// ---------------------------------------------------------------------------

#[test]
fn telemetry_gate_cache_stays_at_one_entry_across_many_agent_dirs() {
    let _suite = suite_guard();
    let _telemetry = NoTelemetryOverride::new();
    let root = ScratchRoot::new("telemetry-bound");
    for revision in 0..40u32 {
        let dir = root.join(&format!("rev-{revision}"));
        let expected = revision % 2 == 0;
        write_telemetry_setting(&dir, expected);
        assert_eq!(
            pir::ai::provider::install_telemetry_enabled_in(&dir),
            expected,
            "revision {revision} must reflect its own agent dir"
        );
        assert_eq!(
            pir::ai::provider::install_telemetry_cache_len(),
            1,
            "the gate cache holds exactly the current config source"
        );
    }
}

#[test]
fn command_cache_is_bounded_across_many_revisions() {
    let _suite = suite_guard();
    let root = ScratchRoot::new("command-bound");
    let dir = root.join("agent");
    std::fs::create_dir_all(&dir).expect("create agent dir");
    std::fs::write(dir.join("models.json"), r#"{"providers":{}}"#).expect("write models.json");

    clear_config_value_cache();

    // 8 distinct command strings: the map keeps every one (well under its
    // 64-entry bound).
    for revision in 0..8u32 {
        let config = format!("!echo rev-{revision}");
        assert_eq!(
            resolve_config_value_in(&dir, &config, None).as_deref(),
            Some(format!("rev-{revision}").as_str())
        );
    }
    assert_eq!(
        command_cache_len(),
        8,
        "distinct commands are all retained under the bound"
    );

    // 10 in-place revisions of `models.json` re-run one command each: each
    // revision replaces that command's entry, so the map grows by exactly one
    // (never one entry per revision).
    for revision in 0..10u32 {
        std::fs::write(
            dir.join("models.json"),
            format!("{{\"providers\":{{}},\"revision\":{revision}}}"),
        )
        .expect("rewrite models.json");
        assert_eq!(
            resolve_config_value_in(&dir, "!echo stable", None).as_deref(),
            Some("stable")
        );
    }
    assert_eq!(
        command_cache_len(),
        9,
        "in-place config revisions replace an entry instead of adding one"
    );

    // Past the capacity the oldest entries are evicted: the map is pinned at
    // its bound, so any number of further revisions cannot grow it.
    let capacity = 64usize;
    for revision in 0..(capacity + 4) {
        let config = format!("!echo many-{revision}");
        assert_eq!(
            resolve_config_value_in(&dir, &config, None).as_deref(),
            Some(format!("many-{revision}").as_str())
        );
        assert!(
            command_cache_len() <= capacity,
            "the command cache must stay within its capacity bound, got {}",
            command_cache_len()
        );
    }
    clear_config_value_cache();
    assert_eq!(command_cache_len(), 0);
}
