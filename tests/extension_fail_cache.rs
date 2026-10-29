//! Integration tests for the persistent extension-load-failure negative
//! cache (`src/agent_session/runtime_loader.rs`): a deterministically broken
//! extension must be skipped by the one-shot headless hosts (`print` covers
//! `--mode json`) after its failure has been recorded, keyed on the entry
//! file's mtime+size fingerprint, while interactive hosts and
//! `PI_EXT_RELOAD=1` always run the real load.

use std::path::{Path, PathBuf};

use pir::extensions::ExtensionConfig;

/// The cache location and the reload bypass are environment variables, and
/// env vars are process-wide: every test that touches them holds this lock
/// for its whole body (HANDOFF verification protocol). A tokio mutex: the
/// guard must live across `load_extension_runtime` awaits, which a std
/// `MutexGuard` may not.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII guard: point `PIR_AGENT_DIR` at a per-test temp directory and restore
/// the touched variables on drop.
struct EnvGuard {
    saved_agent_dir: Option<std::ffi::OsString>,
    saved_ext_reload: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn with_agent_dir(agent_dir: &Path) -> Self {
        let saved_agent_dir = std::env::var_os("PIR_AGENT_DIR");
        let saved_ext_reload = std::env::var_os("PI_EXT_RELOAD");
        std::env::set_var("PIR_AGENT_DIR", agent_dir);
        std::env::remove_var("PI_EXT_RELOAD");
        Self {
            saved_agent_dir,
            saved_ext_reload,
        }
    }

    fn set_ext_reload(&self, enabled: bool) {
        if enabled {
            std::env::set_var("PI_EXT_RELOAD", "1");
        } else {
            std::env::remove_var("PI_EXT_RELOAD");
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(value) = self.saved_agent_dir.take() {
            std::env::set_var("PIR_AGENT_DIR", value);
        } else {
            std::env::remove_var("PIR_AGENT_DIR");
        }
        if let Some(value) = self.saved_ext_reload.take() {
            std::env::set_var("PI_EXT_RELOAD", value);
        } else {
            std::env::remove_var("PI_EXT_RELOAD");
        }
    }
}

/// Per-test fixture directory (the hygiene gate forbids tracked `.js` files,
/// so extension fixtures are generated at runtime).
fn fixture_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-ext-fail-cache-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

fn print_config_at(cwd: &Path, mode: &str) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: mode.to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// mtime+size fingerprint of a fixture file, matching the runtime's
/// `extension_fingerprint` encoding.
fn fingerprint(path: &Path) -> (u64, u32, u64) {
    let metadata = std::fs::metadata(path).expect("fixture metadata");
    let since_epoch = metadata
        .modified()
        .expect("fixture mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("mtime after epoch");
    (
        since_epoch.as_secs(),
        since_epoch.subsec_nanos(),
        metadata.len(),
    )
}

/// Cache key the runtime uses: the path with backslashes normalized.
fn cache_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Manually record a failure for `path` under its real fingerprint, as if a
/// previous run had failed to load it.
fn record_failure(agent_dir: &Path, path: &Path, reason: &str) {
    let (mtime_secs, mtime_nanos, size) = fingerprint(path);
    let file = serde_json::json!({
        "version": 1,
        "entries": {
            cache_key(path): {
                "mtime_secs": mtime_secs,
                "mtime_nanos": mtime_nanos,
                "size": size,
                "reason": reason,
            }
        }
    });
    std::fs::create_dir_all(agent_dir).expect("create agent dir");
    std::fs::write(
        agent_dir.join("extension-fail-cache.json"),
        serde_json::to_string_pretty(&file).expect("serialize cache"),
    )
    .expect("write cache");
}

fn read_cache(agent_dir: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(agent_dir.join("extension-fail-cache.json"))
        .expect("cache file exists");
    serde_json::from_str(&text).expect("cache file parses")
}

fn has_flag(handle: &pir::extensions::ExtensionHandle, name: &str) -> bool {
    handle.flags().iter().any(|flag| flag.name == name)
}

const PROBE_FLAG: &str = "fail_cache_probe";

const GOOD_JS: &str = r#"
export default function (pi) {
	pi.registerFlag("fail_cache_probe", {
		type: "boolean",
		default: false,
		description: "negative-cache probe flag",
	});
}
"#;

const FAILING_JS: &str = r#"
export default function (pi) {
	throw new Error("intentional fail-cache failure");
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn cached_failure_skips_print_mode_and_reloads_on_bypass_or_change() {
    let _env = ENV_LOCK.lock().await;
    let agent_dir = fixture_dir("agent-skip");
    let cwd = fixture_dir("cwd-skip");
    let entry = cwd.join("good.js");
    std::fs::write(&entry, GOOD_JS).expect("write fixture");
    let _guard = EnvGuard::with_agent_dir(&agent_dir);

    // 1. A recorded failure with a matching fingerprint is honored: the
    //    extension is skipped (no flag registered) instead of loaded.
    record_failure(&agent_dir, &entry, "synthetic recorded failure");
    let handle = pir::agent_session::load_extension_runtime(
        std::slice::from_ref(&entry),
        print_config_at(&cwd, "print"),
    )
    .await
    .expect("print-mode runtime");
    assert!(
        !has_flag(&handle, PROBE_FLAG),
        "print mode must skip an extension with a cached failure"
    );

    // 2. PI_EXT_RELOAD=1 forces the real load even with a fresh cache entry.
    _guard.set_ext_reload(true);
    let handle = pir::agent_session::load_extension_runtime(
        std::slice::from_ref(&entry),
        print_config_at(&cwd, "print"),
    )
    .await
    .expect("reload runtime");
    assert!(
        has_flag(&handle, PROBE_FLAG),
        "PI_EXT_RELOAD=1 must bypass the negative cache"
    );

    // 3. The successful reload self-heals: the stale entry is gone, so the
    //    next print-mode run loads the extension normally.
    _guard.set_ext_reload(false);
    let cache = read_cache(&agent_dir);
    assert!(
        cache["entries"].get(cache_key(&entry)).is_none(),
        "a successful load must drop its stale failure entry: {cache}"
    );
    let handle = pir::agent_session::load_extension_runtime(
        std::slice::from_ref(&entry),
        print_config_at(&cwd, "print"),
    )
    .await
    .expect("post-heal runtime");
    assert!(
        has_flag(&handle, PROBE_FLAG),
        "after the entry is dropped the extension loads again"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_load_is_recorded_with_fingerprint_and_refreshes_on_change() {
    let _env = ENV_LOCK.lock().await;
    let agent_dir = fixture_dir("agent-record");
    let cwd = fixture_dir("cwd-record");
    let entry = cwd.join("bad.js");
    std::fs::write(&entry, FAILING_JS).expect("write fixture");
    let _guard = EnvGuard::with_agent_dir(&agent_dir);

    // A real failed print-mode load records the entry with the file's
    // fingerprint and a one-line reason.
    let handle = pir::agent_session::load_extension_runtime(
        std::slice::from_ref(&entry),
        print_config_at(&cwd, "print"),
    )
    .await
    .expect("runtime survives an extension failure");
    assert!(!has_flag(&handle, PROBE_FLAG));
    let cache = read_cache(&agent_dir);
    let recorded = &cache["entries"][cache_key(&entry)];
    assert_eq!(cache["version"], 1);
    assert!(
        recorded["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("intentional fail-cache failure")),
        "the recorded reason must summarize the failure: {cache}"
    );
    let (mtime_secs, mtime_nanos, size) = fingerprint(&entry);
    assert_eq!(recorded["mtime_secs"], mtime_secs);
    assert_eq!(recorded["mtime_nanos"], mtime_nanos);
    assert_eq!(recorded["size"], size);

    // Editing the extension file invalidates the fingerprint: the next load
    // runs the real (still failing) load and refreshes the recorded entry.
    let mut edited = std::fs::read_to_string(&entry).expect("read fixture");
    edited.push_str("\n// edited to move the fingerprint\n");
    std::fs::write(&entry, edited).expect("rewrite fixture");
    let _ = pir::agent_session::load_extension_runtime(
        std::slice::from_ref(&entry),
        print_config_at(&cwd, "print"),
    )
    .await;
    let cache = read_cache(&agent_dir);
    let refreshed = &cache["entries"][cache_key(&entry)];
    let (mtime_secs, mtime_nanos, size) = fingerprint(&entry);
    assert_eq!(refreshed["mtime_secs"], mtime_secs);
    assert_eq!(refreshed["mtime_nanos"], mtime_nanos);
    assert_eq!(refreshed["size"], size);
}

#[tokio::test(flavor = "current_thread")]
async fn interactive_mode_never_skips_and_rpc_mode_never_skips() {
    let _env = ENV_LOCK.lock().await;
    let agent_dir = fixture_dir("agent-interactive");
    let cwd = fixture_dir("cwd-interactive");
    let entry = cwd.join("good.js");
    std::fs::write(&entry, GOOD_JS).expect("write fixture");
    let _guard = EnvGuard::with_agent_dir(&agent_dir);
    record_failure(&agent_dir, &entry, "synthetic recorded failure");

    for mode in ["interactive", "rpc"] {
        let handle = pir::agent_session::load_extension_runtime(
            std::slice::from_ref(&entry),
            print_config_at(&cwd, mode),
        )
        .await
        .expect("runtime");
        assert!(
            has_flag(&handle, PROBE_FLAG),
            "{mode} mode must never skip via the negative cache"
        );
    }

    // The successful interactive load drops the stale entry (cache is
    // maintained by every host, only consulted by the headless ones).
    let cache = read_cache(&agent_dir);
    assert!(
        cache["entries"].get(cache_key(&entry)).is_none(),
        "a successful load must drop its stale failure entry: {cache}"
    );
}
