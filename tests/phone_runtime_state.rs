//! Hermetic tests for the persisted per-port phone runtime state and its
//! lifecycle wiring (`features = "phone"`).
//!
//! The state directory is a unique temp dir per test (the [`RuntimeStore`]
//! seam), and every plugin test injects a fake Tailscale runner, so the suite
//! never reads/writes the real runtime directory and never invokes a real
//! `tailscale`.
#![cfg(feature = "phone")]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir::extensions::CommandOutcome;
use pir::plugins::builtin::phone::runtime_state::{
    digest_control_token, host_key, is_process_running, reconcile_runtime, Reconcile, RuntimeStore,
};
use pir::plugins::builtin::phone::tailscale::{
    disable_args, serve_path, CommandFuture, CommandOutput, ProcessTailscaleRunner, TailscaleRunner,
};
use pir::plugins::builtin::phone::PhonePlugin;
use pir::plugins::{
    AgentPlugin, CommandBackend, CommandHandler, PluginRegistrations, RegistrationContext,
};

const DNS_NAME: &str = "phone-box.tailnet.ts.net";
const HOST: &str = "127.0.0.1";

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Fake Tailscale runner
// ---------------------------------------------------------------------------

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

fn empty_web() -> Result<CommandOutput, String> {
    ok(r#"{"Web":{}}"#)
}

fn active_web(port: u16) -> Result<CommandOutput, String> {
    ok(&format!(
        r#"{{"Web":{{"{DNS_NAME}:443":{{"Handlers":{{"{path}":{{"Proxy":"http://127.0.0.1:{port}"}}}}}}}}}}"#,
        path = serve_path(port),
    ))
}

#[derive(Clone)]
struct FakeTailscale {
    state: Arc<FakeState>,
}

struct FakeState {
    calls: Mutex<Vec<Vec<String>>>,
    responses: Mutex<VecDeque<Result<CommandOutput, String>>>,
}

impl FakeTailscale {
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
}

impl TailscaleRunner for FakeTailscale {
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_store(tag: &str) -> RuntimeStore {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    RuntimeStore::new(std::env::temp_dir().join(format!(
        "pir-phone-runtime-test-{tag}-{}-{nanos}",
        std::process::id()
    )))
}

fn cleanup(store: &RuntimeStore) {
    let _ = std::fs::remove_dir_all(store.root());
}

fn rewrite_pid(store: &RuntimeStore, host: &str, port: u16, pid: u32) {
    let path = store.path_for(host, port);
    let text = std::fs::read_to_string(&path).expect("state file");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("state json");
    value["pid"] = serde_json::json!(pid);
    std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn spawn_sleeper() -> std::process::Child {
    #[cfg(windows)]
    {
        std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping sleeper")
    }
    #[cfg(unix)]
    {
        std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep sleeper")
    }
}

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind((HOST, 0))
        .await
        .expect("free port bind");
    let port = listener.local_addr().expect("free port addr").port();
    drop(listener);
    port
}

fn registrations(plugin: &PhonePlugin) -> PluginRegistrations {
    let mut registrations = PluginRegistrations::default();
    let mut context = RegistrationContext::new(&mut registrations);
    plugin.register(&mut context).expect("registration");
    registrations
}

fn command_handler(registrations: &PluginRegistrations, name: &str) -> CommandHandler {
    registrations
        .commands
        .iter()
        .find(|(def, _)| def.name == name)
        .and_then(|(_, backend)| match backend {
            Some(CommandBackend::Rust(handler)) => Some(Arc::clone(handler)),
            _ => None,
        })
        .expect("rust command handler")
}

async fn run(handler: &CommandHandler, args: &str) -> CommandOutcome {
    handler(args).await
}

// ---------------------------------------------------------------------------
// State file schema
// ---------------------------------------------------------------------------

#[test]
fn state_path_is_keyed_by_host_and_port() {
    assert_eq!(host_key("127.0.0.1"), "local");
    assert_eq!(host_key("localhost"), "local");
    assert_eq!(host_key("::1"), "local");
    assert_eq!(host_key("[::1]"), "local");
    assert_eq!(host_key("0.0.0.0"), "local");
    assert_eq!(host_key("::"), "local");
    assert_eq!(host_key("phone-box.local"), "phone-box.local");
    assert_eq!(host_key("fe80::1"), "fe80%3A%3A1");
    assert_eq!(host_key("host name"), "host%20name");

    let store = RuntimeStore::new(std::env::temp_dir().join("pir-phone-runtime-path"));
    assert_eq!(
        store.path_for("127.0.0.1", 8787),
        store.root().join("local-8787.json")
    );
    assert_eq!(
        store.path_for("fe80::1", 8788),
        store.root().join("fe80%3A%3A1-8788.json")
    );
    assert_eq!(
        RuntimeStore::default().root(),
        std::env::temp_dir().join("pi-phone-extension").as_path()
    );
}

#[test]
fn write_persists_only_the_control_token_digest() {
    let store = temp_store("digest");
    let phone_token = "secret-phone-token";
    let control_token = "runtime-control-token";
    let path = store.write(HOST, 8787, control_token).expect("write state");

    let raw = std::fs::read_to_string(&path).expect("read state");
    assert!(
        !raw.contains(control_token),
        "the raw control token must never be persisted: {raw}"
    );
    assert!(
        !raw.contains(phone_token),
        "phone tokens must never be persisted: {raw}"
    );

    let value: serde_json::Value = serde_json::from_str(&raw).expect("state json");
    assert_eq!(value["pid"].as_u64(), Some(u64::from(std::process::id())));
    assert_eq!(value["host"].as_str(), Some(HOST));
    assert_eq!(value["port"].as_u64(), Some(8787));
    let digest = value["controlTokenDigest"].as_str().expect("digest");
    assert_eq!(digest, digest_control_token(control_token));
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let started_at = value["startedAt"].as_str().expect("startedAt");
    assert_eq!(started_at.len(), 20, "{started_at}");
    assert!(started_at.ends_with('Z'), "{started_at}");
    assert!(started_at.contains('T'), "{started_at}");

    let record = store.read(HOST, 8787).expect("read record");
    assert_eq!(record.pid, std::process::id());
    assert_eq!(record.host, HOST);
    assert_eq!(record.port, 8787);
    assert_eq!(record.control_token_digest, digest);
    assert_eq!(record.started_at, started_at);
    cleanup(&store);
}

#[test]
fn malformed_state_files_are_tolerated_and_removal_is_idempotent() {
    let store = temp_store("malformed");
    std::fs::create_dir_all(store.root()).expect("state dir");
    let path = store.path_for(HOST, 8787);

    for content in [
        "not json",
        "[]",
        "{}",
        r#"{"host":"127.0.0.1"}"#,
        r#"{"host":"127.0.0.1","port":"8787","controlTokenDigest":"x"}"#,
        r#"{"host":"127.0.0.1","port":8787}"#,
    ] {
        std::fs::write(&path, content).expect("write malformed state");
        assert!(store.read(HOST, 8787).is_none(), "{content}");
    }

    store.remove(HOST, 8787).expect("remove");
    store
        .remove(HOST, 8787)
        .expect("removing a missing file is not an error");
    assert!(store.read(HOST, 8787).is_none());
    cleanup(&store);
}

#[test]
fn epoch_started_at_is_accepted() {
    let store = temp_store("epoch");
    std::fs::create_dir_all(store.root()).expect("state dir");
    let path = store.path_for(HOST, 8787);
    std::fs::write(
        &path,
        r#"{"pid":1,"host":"127.0.0.1","port":8787,"controlTokenDigest":"abc","startedAt":1700000000}"#,
    )
    .expect("write state");
    let record = store.read(HOST, 8787).expect("epoch state accepted");
    assert_eq!(record.started_at, "2023-11-14T22:13:20Z");
    cleanup(&store);
}

// ---------------------------------------------------------------------------
// Reconcile semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_same_pid_state_is_removed() {
    let store = temp_store("same-pid");
    store.write(HOST, 8787, "token").expect("write state");

    assert_eq!(
        reconcile_runtime(&store, HOST, 8787).await,
        Reconcile::RemovedOwn
    );
    assert!(store.read(HOST, 8787).is_none());
    // A second reconcile is a no-op.
    assert_eq!(reconcile_runtime(&store, HOST, 8787).await, Reconcile::None);
    cleanup(&store);
}

#[tokio::test]
async fn dead_pid_state_is_removed() {
    let store = temp_store("dead-pid");
    store.write(HOST, 8787, "token").expect("write state");
    rewrite_pid(&store, HOST, 8787, u32::MAX);

    assert!(!is_process_running(u32::MAX).await);
    assert_eq!(
        reconcile_runtime(&store, HOST, 8787).await,
        Reconcile::RemovedDead { pid: u32::MAX }
    );
    assert!(store.read(HOST, 8787).is_none());
    cleanup(&store);
}

#[tokio::test]
async fn live_other_pid_state_is_never_touched() {
    let store = temp_store("live-other");
    let mut sleeper = spawn_sleeper();
    let pid = sleeper.id();
    store.write(HOST, 8787, "token").expect("write state");
    rewrite_pid(&store, HOST, 8787, pid);

    assert!(is_process_running(pid).await, "sleeper pid {pid} is live");
    assert_eq!(
        reconcile_runtime(&store, HOST, 8787).await,
        Reconcile::OtherSession { pid }
    );
    let record = store.read(HOST, 8787).expect("state must remain");
    assert_eq!(record.pid, pid);
    assert!(
        is_process_running(pid).await,
        "reconcile must never kill another process"
    );

    let _ = sleeper.kill();
    let _ = sleeper.wait();
    cleanup(&store);
}

// ---------------------------------------------------------------------------
// Plugin lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_persists_the_state_and_stop_removes_it() {
    let _serial = TEST_LOCK.lock().await;
    let store = temp_store("plugin-start-stop");
    let fake = FakeTailscale::new(vec![
        // enable (start)
        status_json(),
        empty_web(),
        ok(""),
        status_json(),
        empty_web(),
        // disable probe (stop; nothing active, so no off command)
        status_json(),
        empty_web(),
    ]);
    let plugin = PhonePlugin::with_tailscale_runner(Arc::new(fake.clone()));
    plugin.set_runtime_store_for_test(store.clone());

    let port = plugin
        .start_for_test("0 --token secret-phone-token")
        .await
        .expect("ephemeral start");
    let path = store.path_for(HOST, port);
    assert!(path.exists(), "start must persist {}", path.display());
    let record = store.read(HOST, port).expect("runtime record");
    assert_eq!(record.pid, std::process::id());
    assert_eq!(record.host, HOST);
    assert_eq!(record.port, port);
    assert_eq!(record.control_token_digest.len(), 64);
    let raw = std::fs::read_to_string(&path).expect("state file");
    assert!(!raw.contains("secret-phone-token"));

    let registrations = registrations(&plugin);
    let stop = command_handler(&registrations, "phone-stop");
    let outcome = run(&stop, "").await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.error.as_deref(), Some("Pi Phone stopped"));
    assert!(!path.exists(), "stop must remove the runtime record");
    assert!(!plugin.is_running());

    let calls = fake.calls();
    assert_eq!(calls.len(), 7, "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|args| args.last().map(String::as_str) == Some("off")),
        "an inactive route is never disabled: {calls:?}"
    );
    cleanup(&store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_owner_cleanup_disables_only_our_route_before_bind() {
    let _serial = TEST_LOCK.lock().await;
    let store = temp_store("dead-owner");
    let port = free_port().await;

    // Left behind by a process that no longer exists.
    store.write(HOST, port, "token").expect("write state");
    rewrite_pid(&store, HOST, port, u32::MAX);

    let fake = FakeTailscale::new(vec![
        // Reconcile -> disable: probe, then remove exactly this mount.
        status_json(),
        active_web(port),
        ok(""),
        // enable after bind: the route now points at our port.
        status_json(),
        active_web(port),
    ]);
    let plugin = PhonePlugin::with_tailscale_runner(Arc::new(fake.clone()));
    plugin.set_runtime_store_for_test(store.clone());

    let bound = plugin
        .start_for_test(&format!("{port} -"))
        .await
        .expect("start after dead-owner cleanup");
    assert_eq!(bound, port);

    let calls = fake.calls();
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert_eq!(calls[0], ["status", "--json"]);
    assert_eq!(calls[1], ["serve", "status", "--json"]);
    assert_eq!(calls[2], disable_args(port));
    assert_eq!(
        calls[2],
        vec![
            "serve".to_owned(),
            "--yes".to_owned(),
            "--https=443".to_owned(),
            format!("--set-path=phone-{port}"),
            "off".to_owned(),
        ],
        "only the dead owner's path-scoped route is removed"
    );
    assert!(
        calls
            .iter()
            .filter(|args| args.last().map(String::as_str) == Some("off"))
            .count()
            == 1,
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .flatten()
            .any(|arg| arg.starts_with("--set-path=") && arg != &format!("--set-path=phone-{port}")),
        "no other port's route is named: {calls:?}"
    );

    // The new owner is recorded (the stale record was replaced).
    let record = store.read(HOST, port).expect("new runtime record");
    assert_eq!(record.pid, std::process::id());
    assert_ne!(record.pid, u32::MAX);

    assert!(plugin.stop_for_test().await);
    assert!(store.read(HOST, port).is_none());
    cleanup(&store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_other_owner_is_reported_and_never_touched() {
    let _serial = TEST_LOCK.lock().await;
    let store = temp_store("live-owner");
    let listener = tokio::net::TcpListener::bind((HOST, 0))
        .await
        .expect("occupy port");
    let port = listener.local_addr().expect("occupied addr").port();
    let mut sleeper = spawn_sleeper();
    let owner_pid = sleeper.id();
    store.write(HOST, port, "token").expect("write state");
    rewrite_pid(&store, HOST, port, owner_pid);

    // No Tailscale response is queued: a live owner must not trigger any
    // probe or teardown.
    let fake = FakeTailscale::new(Vec::new());
    let plugin = PhonePlugin::with_tailscale_runner(Arc::new(fake.clone()));
    plugin.set_runtime_store_for_test(store.clone());

    let error = plugin
        .start_for_test(&format!("{port} -"))
        .await
        .expect_err("an occupied port must fail to bind");
    assert!(
        error.contains(&format!(
            "Pi Phone on {HOST}:{port} belongs to another pi session (pid {owner_pid})"
        )),
        "{error}"
    );
    assert!(error.contains("already in use"), "{error}");

    assert!(
        fake.calls().is_empty(),
        "Tailscale was touched: {:?}",
        fake.calls()
    );
    let record = store.read(HOST, port).expect("other session's state");
    assert_eq!(record.pid, owner_pid, "the owner's state is untouched");
    assert!(
        is_process_running(owner_pid).await,
        "the other session's process must still be running"
    );

    let _ = sleeper.kill();
    let _ = sleeper.wait();
    drop(listener);
    cleanup(&store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_the_tailscale_state() {
    let _serial = TEST_LOCK.lock().await;
    let fake = FakeTailscale::new(vec![status_json(), active_web(8787)]);
    let plugin = PhonePlugin::with_tailscale_runner(Arc::new(fake.clone()));

    let registrations = registrations(&plugin);
    let status = command_handler(&registrations, "phone-status");
    let outcome = run(&status, "").await;
    assert!(outcome.ok);
    let text = outcome.error.expect("status text");
    assert!(text.contains("Pi Phone is stopped"), "{text}");
    assert!(text.contains("still pointing at Pi Phone"), "{text}");
    assert!(
        text.contains(&format!("https://{DNS_NAME}/phone-8787/")),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_survives_tailscale_absence_with_a_manual_fallback() {
    let _serial = TEST_LOCK.lock().await;
    let store = temp_store("no-tailscale");
    // A binary name that cannot resolve: every invocation is a soft ENOENT.
    let plugin = PhonePlugin::with_tailscale_runner(Arc::new(ProcessTailscaleRunner::new(
        "pir-phone-test-missing-tailscale-binary",
    )));
    plugin.set_runtime_store_for_test(store.clone());

    let registrations = registrations(&plugin);
    let start = command_handler(&registrations, "phone-start");
    let outcome = run(&start, "0 -").await;
    assert!(outcome.ok, "tailscale absence must never fail the server");
    let text = outcome.error.expect("start text");
    assert!(text.contains("Pi Phone running at"), "{text}");
    assert!(
        text.contains("Could not configure Tailscale Serve automatically"),
        "{text}"
    );
    assert!(
        text.contains("Manual fallback: tailscale serve --bg --https=443 http://127.0.0.1:"),
        "{text}"
    );
    assert!(plugin.is_running());
    let port = plugin.local_addr().expect("bound address").port();
    assert!(store.read(HOST, port).is_some());

    assert!(plugin.stop_for_test().await);
    assert!(store.read(HOST, port).is_none());
    cleanup(&store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_uses_the_injected_runner_and_removes_our_route() {
    let _serial = TEST_LOCK.lock().await;
    let store = temp_store("shutdown");
    let port = free_port().await;
    let fake = FakeTailscale::new(vec![
        // enable (start)
        status_json(),
        empty_web(),
        ok(""),
        status_json(),
        empty_web(),
        // disable probe (shutdown)
        status_json(),
        active_web(port),
        ok(""),
    ]);
    let plugin = PhonePlugin::with_tailscale_runner(Arc::new(fake.clone()));
    plugin.set_runtime_store_for_test(store.clone());
    plugin
        .start_for_test(&format!("{port} -"))
        .await
        .expect("explicit-port start");
    assert_eq!(fake.calls().len(), 5, "{:?}", fake.calls());

    plugin.on_shutdown().await.expect("shutdown");
    assert!(!plugin.is_running());
    assert!(store.read(HOST, port).is_none());

    let calls = fake.calls();
    assert_eq!(calls.len(), 8, "{calls:?}");
    assert_eq!(calls[5], ["status", "--json"]);
    assert_eq!(calls[6], ["serve", "status", "--json"]);
    assert_eq!(
        calls[7],
        disable_args(port),
        "shutdown disables exactly this instance's route through the injected runner"
    );
    cleanup(&store);
}
