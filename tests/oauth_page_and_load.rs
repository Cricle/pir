//! Regression tests for two CLI experience fixes:
//!
//! 1. OAuth callback page deferral (upstream `callback-server.ts` sends the
//!    browser page only after `complete(code)` settles): the success page
//!    (200) goes out only after [`OAuthCallbackServer::confirm_page`] reports
//!    a successful exchange, a failed exchange renders the 502 failure page
//!    carrying the error, and a server close without confirmation fails the
//!    pending page instead of leaving the browser hanging.
//! 2. Extension strict-validation handoff: after
//!    [`pir::agent_session::stash_validated_extension_runtime`], a compatible
//!    [`pir::agent_session::load_extension_runtime`] call reuses the validated
//!    runtime (top-level extension code executed exactly once, proven by a
//!    file rewritten after validation) while an incompatible host config keeps
//!    its own load path.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pir::ai::auth::callback_server::{CallbackWait, OAuthCallbackServer};

const TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// OAuth callback page deferral
// ---------------------------------------------------------------------------

/// Deliver one browser callback over a raw loopback connection.
async fn deliver_callback(server: &OAuthCallbackServer, target: &str) -> tokio::net::TcpStream {
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", server.port()));
    let mut stream = tokio::time::timeout(TIMEOUT, connect)
        .await
        .expect("connect within timeout")
        .expect("tcp connect");
    let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write callback request");
    stream
}

/// Assert the connection stays silent: the page is deferred until the flow
/// confirms the exchange outcome.
async fn assert_no_response(stream: &mut tokio::net::TcpStream) {
    let mut probe = [0u8; 1];
    match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut probe)).await {
        Err(_elapsed) => {} // still pending, as expected
        Ok(Ok(0)) => panic!("connection closed without a page"),
        Ok(Ok(read)) => panic!("page was sent before confirm_page ({read} byte(s))"),
        Ok(Err(error)) => panic!("read error while page should be pending: {error}"),
    }
}

/// Read the full response (the server closes the connection after the page).
async fn read_response(stream: &mut tokio::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut buffer))
        .await
        .expect("response within timeout")
        .expect("read response");
    String::from_utf8_lossy(&buffer).into_owned()
}

#[tokio::test]
async fn callback_page_waits_for_exchange_confirmation() {
    let server = OAuthCallbackServer::start("Test", "127.0.0.1", 0, "/callback", None, Some("st"))
        .await
        .expect("callback server starts");
    let mut stream = deliver_callback(&server, "/callback?code=c1&state=st").await;

    let pairs = match server.wait(Some(TIMEOUT)).await {
        Ok(CallbackWait::Completed(pairs)) => pairs,
        other => panic!("callback wait failed: {other:?}"),
    };
    assert_eq!(
        pairs
            .iter()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.as_str()),
        Some("c1")
    );

    // The page is deferred: nothing goes out before the exchange outcome is
    // confirmed (rescan #10 — no premature "Signed in" page).
    assert_no_response(&mut stream).await;

    server.confirm_page(None);
    let response = read_response(&mut stream).await;
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
    assert!(
        response.contains("Authentication successful"),
        "got: {response}"
    );
    assert!(response.contains("Signed in to Test"), "got: {response}");
}

#[tokio::test]
async fn failed_exchange_renders_502_page() {
    let server = OAuthCallbackServer::start("Test", "127.0.0.1", 0, "/callback", None, Some("st"))
        .await
        .expect("callback server starts");
    let mut stream = deliver_callback(&server, "/callback?code=bad&state=st").await;

    match server.wait(Some(TIMEOUT)).await {
        Ok(CallbackWait::Completed(_)) => {}
        other => panic!("callback wait failed: {other:?}"),
    }

    // A failed exchange (upstream `complete` throwing) renders the 502 page
    // carrying the error instead of a success page.
    server.confirm_page(Some("token endpoint rejected the grant".to_string()));
    let response = read_response(&mut stream).await;
    assert!(response.starts_with("HTTP/1.1 502"), "got: {response}");
    assert!(
        response.contains("Authentication failed"),
        "got: {response}"
    );
    assert!(response.contains("Test sign-in failed"), "got: {response}");
    assert!(
        response.contains("token endpoint rejected the grant"),
        "got: {response}"
    );
}

#[tokio::test]
async fn server_close_fails_a_pending_page_instead_of_hanging() {
    let server = OAuthCallbackServer::start("Test", "127.0.0.1", 0, "/callback", None, Some("st"))
        .await
        .expect("callback server starts");
    let mut stream = deliver_callback(&server, "/callback?code=c3&state=st").await;

    match server.wait(Some(TIMEOUT)).await {
        Ok(CallbackWait::Completed(_)) => {}
        other => panic!("callback wait failed: {other:?}"),
    }

    // A flow that gives up without confirming (server close) still answers
    // the browser with a failure page.
    server.close();
    let response = read_response(&mut stream).await;
    assert!(response.starts_with("HTTP/1.1 502"), "got: {response}");
    assert!(
        response.contains("Authentication failed"),
        "got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Extension strict-validation handoff
// ---------------------------------------------------------------------------

const V1_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("v1-probe", {
    description: "registered by the validated runtime",
    handler: async () => {},
  });
}
"#;

const V2_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("v2-probe", {
    description: "registered by a second load from disk",
    handler: async () => {},
  });
}
"#;

/// Serialize the extension tests: the handoff slot is process-global.
static EXT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ext_guard() -> std::sync::MutexGuard<'static, ()> {
    EXT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-oauth-page-and-load-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn validation_config(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "print".to_string(),
        limits: Some(Default::default()),
        ..Default::default()
    }
}

/// Host config for the print-family load: same mode/UI as the validation, but
/// with the model the host resolves later (deliberately not compared by the
/// handoff gate).
fn host_config(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        model: Some("host-model".to_string()),
        limits: Some(Default::default()),
        ..validation_config(cwd)
    }
}

fn command_names(runtime: &pir::extensions::ExtensionHandle) -> Vec<String> {
    runtime
        .command_defs()
        .into_iter()
        .map(|command| command.name)
        .collect()
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the ext-env lock must span the runtime awaits
async fn validated_runtime_is_reused_by_compatible_host_load() {
    let _guard = ext_guard();
    let dir = TempDir::new("handoff-consumed");
    let extension_path = dir.path().join("ext.ts");
    std::fs::write(&extension_path, V1_EXTENSION_TS).expect("write v1 extension");

    let config = validation_config(dir.path());
    let (validated, errors) = pir::agent_session::load_extension_runtime_strict(
        std::slice::from_ref(&extension_path),
        config.clone(),
    )
    .await
    .expect("strict validation succeeds");
    assert!(errors.is_empty(), "validation errors: {errors:?}");
    let validated = validated.expect("validated runtime handle");
    assert!(
        command_names(&validated).contains(&"v1-probe".to_string()),
        "the extension ran during validation"
    );

    // Rewrite the file AFTER validation: only a reused validated runtime can
    // still expose the v1 command — a second load from disk would register
    // v2-probe instead (this is the top-level side-effect counter).
    std::fs::write(&extension_path, V2_EXTENSION_TS).expect("write v2 extension");
    pir::agent_session::stash_validated_extension_runtime(
        std::slice::from_ref(&extension_path),
        config,
        validated,
    );

    let host = pir::agent_session::load_extension_runtime(
        std::slice::from_ref(&extension_path),
        host_config(dir.path()),
    )
    .await
    .expect("host runtime");
    let names = command_names(&host);
    assert!(
        names.contains(&"v1-probe".to_string()),
        "host consumed the validated runtime: {names:?}"
    );
    assert!(
        !names.contains(&"v2-probe".to_string()),
        "host must not load the extension a second time: {names:?}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the ext-env lock must span the runtime awaits
async fn incompatible_host_config_keeps_its_own_load() {
    let _guard = ext_guard();
    let dir = TempDir::new("handoff-incompatible");
    let extension_path = dir.path().join("ext.ts");
    std::fs::write(&extension_path, V1_EXTENSION_TS).expect("write v1 extension");

    let config = validation_config(dir.path());
    let (validated, errors) = pir::agent_session::load_extension_runtime_strict(
        std::slice::from_ref(&extension_path),
        config.clone(),
    )
    .await
    .expect("strict validation succeeds");
    assert!(errors.is_empty(), "validation errors: {errors:?}");
    pir::agent_session::stash_validated_extension_runtime(
        std::slice::from_ref(&extension_path),
        config,
        validated.expect("validated runtime handle"),
    );

    // The rpc host loads under a different mode/UI config: it must drop the
    // handoff and run its own load (reading the v2 file from disk).
    std::fs::write(&extension_path, V2_EXTENSION_TS).expect("write v2 extension");
    let host = pir::agent_session::load_extension_runtime(
        &[extension_path],
        pir::extensions::ExtensionConfig {
            mode: "rpc".to_string(),
            has_ui: true,
            limits: Some(Default::default()),
            ..Default::default()
        },
    )
    .await
    .expect("host runtime");
    let names = command_names(&host);
    assert!(
        names.contains(&"v2-probe".to_string()),
        "incompatible host loaded its own runtime: {names:?}"
    );
    assert!(
        !names.contains(&"v1-probe".to_string()),
        "incompatible host must not consume the handoff: {names:?}"
    );
}
