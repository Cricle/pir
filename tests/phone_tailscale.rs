//! Hermetic tests for the native Tailscale Serve integration
//! (`features = "phone"`).
//!
//! Every test injects a recording [`TailscaleRunner`], so the suite asserts
//! the exact argv and canned JSON without ever invoking a real `tailscale`
//! binary or touching a tailnet.
#![cfg(feature = "phone")]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir::plugins::builtin::phone::runtime_state::RuntimeStore;
use pir::plugins::builtin::phone::tailscale::{
    disable_args, enable_args, serve_path, CommandFuture, CommandOutput, Tailscale, TailscaleRunner,
};
use pir::plugins::builtin::phone::{new_phone_plugin, PhonePlugin};
use pir::plugins::{AgentPlugin, CommandBackend, PluginRegistrations, RegistrationContext};

const DNS_NAME: &str = "phone-box.tailnet.ts.net";

fn ok(stdout: &str) -> Result<CommandOutput, String> {
    Ok(CommandOutput {
        code: 0,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

fn status_json() -> Result<CommandOutput, String> {
    ok(&format!(r#"{{"Self":{{"DNSName":"{DNS_NAME}."}}}}"#))
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn empty_web() -> Result<CommandOutput, String> {
    ok(r#"{"Web":{}}"#)
}

fn web_with_handler(path: &str, proxy: &str) -> Result<CommandOutput, String> {
    ok(&format!(
        r#"{{"Web":{{"{DNS_NAME}:443":{{"Handlers":{{"{path}":{{"Proxy":"{proxy}"}}}}}}}}}}"#
    ))
}

fn active_web(port: u16) -> Result<CommandOutput, String> {
    web_with_handler(&serve_path(port), &format!("http://127.0.0.1:{port}"))
}

#[derive(Clone)]
struct FakeRunner {
    state: Arc<FakeState>,
}

struct FakeState {
    calls: Mutex<Vec<Vec<String>>>,
    responses: Mutex<VecDeque<Result<CommandOutput, String>>>,
}

impl FakeRunner {
    fn new(responses: Vec<Result<CommandOutput, String>>) -> Self {
        Self {
            state: Arc::new(FakeState {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            }),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.state.calls.lock().unwrap().clone()
    }

    fn client(&self) -> Tailscale {
        Tailscale::with_runner(Arc::new(self.clone()))
    }
}

impl TailscaleRunner for FakeRunner {
    fn run<'a>(&'a self, args: &'a [String], _timeout: Duration) -> CommandFuture<'a> {
        let state = Arc::clone(&self.state);
        let args = args.to_vec();
        Box::pin(async move {
            state.calls.lock().unwrap().push(args.clone());
            match state.responses.lock().unwrap().pop_front() {
                Some(response) => response,
                None => Err(format!("unexpected tailscale invocation: {args:?}")),
            }
        })
    }
}

#[test]
fn serve_path_is_per_port() {
    assert_eq!(serve_path(8787), "/phone-8787");
    assert_eq!(serve_path(9100), "/phone-9100");
}

#[tokio::test]
async fn enable_emits_the_exact_reference_argv_and_reports_ready() {
    let fake = FakeRunner::new(vec![
        status_json(),
        empty_web(),
        ok(""),
        status_json(),
        active_web(8787),
    ]);
    let client = fake.client();

    let result = client.enable(8787).await;

    assert!(result.enabled, "{result:?}");
    assert!(result.changed, "{result:?}");
    assert!(!result.replaced_existing, "{result:?}");
    assert_eq!(
        result.url,
        format!("https://{DNS_NAME}/phone-8787/"),
        "{result:?}"
    );
    assert_eq!(result.error, "");
    assert_eq!(
        fake.calls(),
        vec![
            argv(&["status", "--json"]),
            argv(&["serve", "status", "--json"]),
            enable_args(8787),
            argv(&["status", "--json"]),
            argv(&["serve", "status", "--json"]),
        ],
        "the enable argv is exactly the reference one"
    );
    assert_eq!(
        fake.calls()[2],
        vec![
            "serve",
            "--bg",
            "--yes",
            "--https=443",
            "--set-path=phone-8787",
            "http://127.0.0.1:8787",
        ]
    );
}

#[tokio::test]
async fn enable_is_idempotent_when_the_route_already_points_at_us() {
    let fake = FakeRunner::new(vec![status_json(), active_web(8787)]);
    let result = fake.client().enable(8787).await;

    assert!(result.enabled, "{result:?}");
    assert!(!result.changed, "{result:?}");
    assert_eq!(
        result.url,
        format!("https://{DNS_NAME}/phone-8787/"),
        "{result:?}"
    );
    assert_eq!(
        fake.calls(),
        vec![vec!["status", "--json"], vec!["serve", "status", "--json"],],
        "an active route must not re-run `tailscale serve`"
    );
}

#[tokio::test]
async fn enable_reports_replaced_existing_config_on_command_failure() {
    let fake = FakeRunner::new(vec![
        status_json(),
        web_with_handler("/", "http://127.0.0.1:9999"),
        Ok(CommandOutput {
            code: 1,
            stdout: String::new(),
            stderr: "serve failed\n".to_owned(),
        }),
    ]);
    let result = fake.client().enable(8787).await;

    assert!(!result.enabled, "{result:?}");
    assert!(!result.changed, "{result:?}");
    assert!(result.replaced_existing, "{result:?}");
    assert_eq!(result.error, "serve failed");
}

#[tokio::test]
async fn disable_removes_only_our_path_scoped_route() {
    let fake = FakeRunner::new(vec![status_json(), active_web(8787), ok("")]);
    let result = fake.client().disable(8787).await;

    assert!(result.disabled);
    assert_eq!(result.error, "");
    assert_eq!(
        fake.calls(),
        vec![
            argv(&["status", "--json"]),
            argv(&["serve", "status", "--json"]),
            disable_args(8787),
        ]
    );
    assert_eq!(
        fake.calls()[2],
        vec![
            "serve",
            "--yes",
            "--https=443",
            "--set-path=phone-8787",
            "off",
        ],
        "the off argv must target exactly one mount"
    );
    assert!(
        !fake.calls()[2].iter().any(|arg| arg == "phone-8788"),
        "other instances' routes are never named"
    );
}

#[tokio::test]
async fn disable_is_a_no_op_when_our_route_is_absent() {
    let fake = FakeRunner::new(vec![
        status_json(),
        web_with_handler("/phone-8788", "http://127.0.0.1:8788"),
    ]);
    let result = fake.client().disable(8787).await;

    assert!(!result.disabled);
    assert_eq!(result.error, "");
    assert_eq!(
        fake.calls().len(),
        2,
        "no off command without an active route for this port"
    );
}

#[tokio::test]
async fn serve_info_accepts_loopback_targets_and_paths_without_a_leading_slash() {
    for (path, proxy) in [
        ("phone-8787", "http://localhost:8787/"),
        ("/phone-8787", "http://[::1]:8787/ws"),
        ("/phone-8787/", "http://127.0.0.1:8787"),
    ] {
        let fake = FakeRunner::new(vec![status_json(), web_with_handler(path, proxy)]);
        let info = fake.client().serve_info(8787).await;
        assert!(info.active, "{path} {proxy}: {info:?}");
        assert_eq!(info.url, format!("https://{DNS_NAME}/phone-8787/"));
        assert!(info.had_any_web_config);
        assert_eq!(info.error, "");
    }
}

#[tokio::test]
async fn serve_info_ignores_other_ports_and_paths() {
    for (path, proxy) in [
        ("/phone-8788", "http://127.0.0.1:8788"),
        ("/phone-8787", "http://127.0.0.1:8788"),
        ("/", "http://127.0.0.1:8787"),
        ("/phone-8787", "https://127.0.0.1:8787"),
    ] {
        let fake = FakeRunner::new(vec![status_json(), web_with_handler(path, proxy)]);
        let info = fake.client().serve_info(8787).await;
        assert!(!info.active, "{path} {proxy}: {info:?}");
        assert!(info.had_any_web_config);
    }
}

#[tokio::test]
async fn serve_info_reports_malformed_and_failed_status_softly() {
    let malformed = FakeRunner::new(vec![status_json(), ok("not json")]);
    let info = malformed.client().serve_info(8787).await;
    assert!(!info.active);
    assert_eq!(info.error, "Failed to parse tailscale serve status output.");

    let failed = FakeRunner::new(vec![
        status_json(),
        Ok(CommandOutput {
            code: 1,
            stdout: String::new(),
            stderr: "denied\n".to_owned(),
        }),
    ]);
    let info = failed.client().serve_info(8787).await;
    assert!(!info.active);
    assert_eq!(info.error, "denied");
    assert_eq!(
        info.url,
        format!("https://{DNS_NAME}/phone-8787/"),
        "the URL probe still succeeds independently"
    );
}

#[tokio::test]
async fn a_missing_tailscale_binary_is_a_soft_error() {
    // A program name that cannot resolve: spawn fails with ENOENT, which the
    // client reports as an error string. No real tailscale is involved.
    let client = Tailscale::with_program("pir-phone-test-missing-tailscale-binary");
    let info = client.serve_info(8787).await;
    assert!(!info.active);
    assert!(!info.error.is_empty());
    assert_eq!(info.url, "");

    let enabled = client.enable(8787).await;
    assert!(!enabled.enabled);
    assert!(enabled
        .error
        .contains("pir-phone-test-missing-tailscale-binary"));

    let disabled = client.disable(8787).await;
    assert!(!disabled.disabled);
    assert!(disabled
        .error
        .contains("pir-phone-test-missing-tailscale-binary"));
}

// ---------------------------------------------------------------------------
// Plugin defaults / production factory
// ---------------------------------------------------------------------------

#[test]
fn new_plugin_defaults_to_a_noop_tailscale_runner() {
    assert_eq!(PhonePlugin::new().tailscale_runner_kind(), "noop");
    assert_eq!(PhonePlugin::default().tailscale_runner_kind(), "noop");

    let injected = PhonePlugin::with_tailscale_runner(Arc::new(FakeRunner::new(Vec::new())));
    assert_eq!(injected.tailscale_runner_kind(), "custom");
}

#[test]
fn builtin_factory_installs_the_real_process_runner() {
    let factory = new_phone_plugin();
    let plugin = factory
        .as_any()
        .and_then(|any| any.downcast_ref::<PhonePlugin>())
        .expect("factory returns a PhonePlugin");
    assert_eq!(
        plugin.tailscale_runner_kind(),
        "process",
        "the builtin assembly must use the real tailscale runner"
    );
}

fn temp_store(tag: &str) -> RuntimeStore {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    RuntimeStore::new(std::env::temp_dir().join(format!(
        "pir-phone-tailscale-test-{tag}-{}-{nanos}",
        std::process::id()
    )))
}

/// A directory containing fake `tailscale` executables that record an
/// invocation by writing `spawned.txt`.
fn fake_tailscale_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-phone-fake-tailscale-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("fake tailscale dir");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let sentinel = dir.join("spawned.txt");
        let script = dir.join("tailscale");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf spawned >> '{}'\nexit 0\n",
                sentinel.display()
            ),
        )
        .expect("fake tailscale script");
        let mut permissions = std::fs::metadata(&script)
            .expect("fake script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("fake script mode");
    }
    #[cfg(windows)]
    {
        // `CreateProcess` would only auto-resolve `tailscale.exe`, but these
        // files prove the default path never shells out either.
        for name in ["tailscale.cmd", "tailscale.bat"] {
            std::fs::write(dir.join(name), "@echo spawned>\"%~dp0spawned.txt\"\r\n")
                .expect("fake tailscale script");
        }
    }

    dir
}

/// Prepends a directory to `PATH` for the duration of the guard (the original
/// entries stay, so concurrent subprocesses keep resolving normally).
struct PathGuard {
    original: Option<OsString>,
}

impl PathGuard {
    fn prepend(dir: &Path) -> Self {
        let original = std::env::var_os("PATH");
        let mut entries = vec![dir.to_path_buf()];
        if let Some(value) = &original {
            entries.extend(std::env::split_paths(value));
        }
        std::env::set_var(
            "PATH",
            std::env::join_paths(entries).expect("join fake PATH"),
        );
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }
}

async fn status_text(plugin: &PhonePlugin) -> String {
    let mut registrations = PluginRegistrations::default();
    let mut context = RegistrationContext::new(&mut registrations);
    plugin.register(&mut context).expect("registration");
    let handler = registrations
        .commands
        .iter()
        .find(|(def, _)| def.name == "phone-status")
        .and_then(|(_, backend)| match backend {
            Some(CommandBackend::Rust(handler)) => Some(Arc::clone(handler)),
            _ => None,
        })
        .expect("status handler");
    handler("").await.error.expect("status text")
}

/// `PhonePlugin::new()` never invokes the CLI, even when a fake `tailscale`
/// executable is the first entry on `PATH`; start, status and shutdown all
/// report the soft "disabled" error instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_plugin_never_spawns_tailscale_from_path() {
    let fake_dir = fake_tailscale_dir("path");
    let sentinel = fake_dir.join("spawned.txt");
    let path_guard = PathGuard::prepend(&fake_dir);
    let store = temp_store("path");
    let plugin = PhonePlugin::new();
    plugin.set_runtime_store_for_test(store.clone());
    assert_eq!(plugin.tailscale_runner_kind(), "noop");

    plugin.start_for_test("0 -").await.expect("ephemeral start");
    assert!(
        !sentinel.exists(),
        "PhonePlugin::new() spawned tailscale during start"
    );

    let text = status_text(&plugin).await;
    assert!(text.contains("Pi Phone running at"), "{text}");
    assert!(
        text.contains(Tailscale::DISABLED_ERROR),
        "status must use the injected no-op runner: {text}"
    );
    assert!(
        !sentinel.exists(),
        "PhonePlugin::new() spawned tailscale during status"
    );

    plugin.on_shutdown().await.expect("shutdown");
    assert!(!plugin.is_running());
    assert!(
        !sentinel.exists(),
        "PhonePlugin::new() spawned tailscale during shutdown"
    );

    drop(path_guard);
    let _ = std::fs::remove_dir_all(&fake_dir);
    let _ = std::fs::remove_dir_all(store.root());
}
