//! Regression tests for the Bedrock credential-chain review fixes:
//! - P1-2: the terminal `default` shared-profile fallback (no `AWS_PROFILE`
//!   required) via a temp `HOME` / `AWS_SHARED_CREDENTIALS_FILE` fixture.
//! - P1-3: the ECS container-credentials `Authorization` token
//!   (`AWS_CONTAINER_AUTHORIZATION_TOKEN` / `_FILE`) against a local
//!   TcpListener mock.
//! - P2: the `AWS_CONTAINER_CREDENTIALS_FULL_URI` loopback/link-local host
//!   restriction (pure-function assertions).
//!
//! Env-touching tests mutate process state, so they are serialized behind a
//! process-wide mutex; no real AWS requests are made.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;

use pir::ai::api::bedrock::credentials::{
    default_profile_fallback, ecs_authorization_token, ecs_full_uri_host_allowed,
    fetch_ecs_credentials, is_valid_sts_region, plan_source, resolve_credentials, SourcePlan,
};
use pir::ai::error::AiError;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A fresh scratch directory for one test's profile fixtures (cleared first
/// so leftovers from a previous run cannot leak in).
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("pir-p1-credentials-tests")
        .join(format!("{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_file(path: &PathBuf, content: &str) {
    std::fs::write(path, content).expect("write fixture file");
}

/// Point the shared-profile resolution at an isolated fixture root: `HOME`
/// wins over `USERPROFILE` (checked first), and both env overrides are set so
/// a developer's real `~/.aws` files can never leak into the assertions.
fn isolate_shared_files(dir: &PathBuf, credentials: Option<&PathBuf>, config: Option<&PathBuf>) {
    std::env::set_var("HOME", dir);
    std::env::set_var(
        "AWS_SHARED_CREDENTIALS_FILE",
        credentials.unwrap_or(&dir.join("absent-credentials")),
    );
    std::env::set_var(
        "AWS_CONFIG_FILE",
        config.unwrap_or(&dir.join("absent-config")),
    );
}

/// Unset every variable `resolve_credentials` consults so the plan is fully
/// determined by the fixture files.
fn clear_chain_env() {
    for var in [
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ] {
        std::env::remove_var(var);
    }
}

// ---------------------------------------------------------------------------
// P1-2: the `default` shared-profile terminal fallback
// ---------------------------------------------------------------------------

#[test]
fn default_profile_fallback_is_none_without_shared_files() {
    let _env = lock_env();
    let dir = scratch_dir("no-files");
    isolate_shared_files(&dir, None, None);
    assert!(default_profile_fallback().is_none());
}

#[test]
fn default_profile_fallback_resolves_the_default_section() {
    let _env = lock_env();
    let dir = scratch_dir("default-ok");
    let credentials = dir.join("credentials");
    write_file(
        &credentials,
        "[default]\naws_access_key_id = AKID-FALLBACK\naws_secret_access_key = fallback-secret\n",
    );
    let config = dir.join("config");
    write_file(&config, "[default]\nregion = eu-west-1\n");
    isolate_shared_files(&dir, Some(&credentials), Some(&config));

    let resolved = default_profile_fallback()
        .expect("shared files exist, so the fallback must be attempted")
        .expect("the default section should resolve");
    assert_eq!(resolved.credentials.access_key_id, "AKID-FALLBACK");
    assert_eq!(resolved.credentials.secret_access_key, "fallback-secret");
    assert_eq!(resolved.credentials.source, "profile:default");
    assert_eq!(resolved.region.as_deref(), Some("eu-west-1"));
}

#[test]
fn default_profile_fallback_fails_closed_on_missing_keys() {
    let _env = lock_env();
    let dir = scratch_dir("default-incomplete");
    let credentials = dir.join("credentials");
    write_file(&credentials, "[default]\naws_access_key_id = AKID-ONLY\n");
    isolate_shared_files(&dir, Some(&credentials), None);

    let error = default_profile_fallback()
        .expect("shared file exists, so the fallback must be attempted")
        .expect_err("an incomplete default profile must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("missing"));
}

#[test]
fn default_profile_fallback_fails_closed_on_role_arn() {
    let _env = lock_env();
    let dir = scratch_dir("default-role-arn");
    let credentials = dir.join("credentials");
    write_file(
        &credentials,
        "[default]\nrole_arn = arn:aws:iam::1:role/x\naws_access_key_id = A\naws_secret_access_key = s\n",
    );
    isolate_shared_files(&dir, Some(&credentials), None);

    let error = default_profile_fallback()
        .expect("shared file exists, so the fallback must be attempted")
        .expect_err("a default profile with role_arn must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("role_arn"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn resolve_falls_back_to_the_default_shared_profile() {
    let _env = lock_env();
    clear_chain_env();
    let dir = scratch_dir("resolve-default");
    let credentials = dir.join("credentials");
    write_file(
        &credentials,
        "[default]\naws_access_key_id = AKID-RESOLVE\naws_secret_access_key = resolve-secret\n",
    );
    isolate_shared_files(&dir, Some(&credentials), None);

    let resolved = resolve_credentials(None)
        .await
        .expect("the default-profile fallback should resolve");
    assert_eq!(resolved.credentials.access_key_id, "AKID-RESOLVE");
    assert_eq!(resolved.credentials.source, "profile:default");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn resolve_reports_no_credentials_when_no_shared_files_exist() {
    let _env = lock_env();
    clear_chain_env();
    let dir = scratch_dir("resolve-none");
    isolate_shared_files(&dir, None, None);

    let error = resolve_credentials(None)
        .await
        .expect_err("an empty chain must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("No AWS credentials for Bedrock"));
}

// ---------------------------------------------------------------------------
// P1-3: the ECS container-credentials Authorization token
// ---------------------------------------------------------------------------

#[test]
fn ecs_authorization_token_reads_direct_env_and_file_variant() {
    let _env = lock_env();
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    assert_eq!(ecs_authorization_token().expect("token probe"), None);

    std::env::set_var("AWS_CONTAINER_AUTHORIZATION_TOKEN", "  direct-token  ");
    assert_eq!(
        ecs_authorization_token().expect("token probe").as_deref(),
        Some("direct-token")
    );

    let dir = scratch_dir("token-file");
    let token_path = dir.join("token");
    write_file(&token_path, "file-token\n");
    std::env::set_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", &token_path);
    // The file variant takes precedence over the direct variable.
    assert_eq!(
        ecs_authorization_token().expect("token probe").as_deref(),
        Some("file-token")
    );

    std::env::set_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", dir.join("absent"));
    let error = ecs_authorization_token().expect_err("an unreadable token file must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));

    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
}

/// Serve exactly one HTTP response and capture the request bytes (same shape
/// as the suites/ mock; a GET has no body, so one read pass is enough once
/// the header terminator arrives).
fn serve_once(status_line: &str, body: &str) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let addr = listener.local_addr().expect("local addr");
    let response = format!(
        "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one connection");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            // Accumulate first, then check: hyper writes the whole GET head
            // in one TCP write, so checking before extending would miss the
            // terminator on read #1 and block on a second read that never
            // comes.
            request.extend_from_slice(&chunk[..read]);
            if String::from_utf8_lossy(&request).contains("\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(response.as_bytes())
            .expect("write mock response");
        String::from_utf8_lossy(&request).to_string()
    });
    (format!("http://{addr}"), handle)
}

const ECS_OK_BODY: &str =
    r#"{"AccessKeyId":"AKID-MOCK","SecretAccessKey":"mock-secret","Token":"mock-token"}"#;

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn ecs_fetch_sends_the_authorization_header_when_token_configured() {
    let _env = lock_env();
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    std::env::set_var("AWS_CONTAINER_AUTHORIZATION_TOKEN", "pod-token");
    let (url, server) = serve_once("HTTP/1.1 200 OK", ECS_OK_BODY);

    let credentials = fetch_ecs_credentials(&url)
        .await
        .expect("mock ECS fetch with token should succeed");
    assert_eq!(credentials.access_key_id, "AKID-MOCK");

    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    let request = server.join().expect("mock server thread");
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: pod-token"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn ecs_fetch_omits_the_authorization_header_without_token_env() {
    let _env = lock_env();
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    let (url, server) = serve_once("HTTP/1.1 200 OK", ECS_OK_BODY);

    let credentials = fetch_ecs_credentials(&url)
        .await
        .expect("mock ECS fetch without token should succeed");
    assert_eq!(credentials.access_key_id, "AKID-MOCK");

    let request = server.join().expect("mock server thread");
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the env lock must span the runtime awaits
async fn ecs_fetch_uses_the_token_file_contents_as_header_value() {
    let _env = lock_env();
    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    let dir = scratch_dir("fetch-token-file");
    let token_path = dir.join("token");
    write_file(&token_path, "file-pod-token\n");
    std::env::set_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", &token_path);
    let (url, server) = serve_once("HTTP/1.1 200 OK", ECS_OK_BODY);

    let credentials = fetch_ecs_credentials(&url)
        .await
        .expect("mock ECS fetch with token file should succeed");
    assert_eq!(credentials.access_key_id, "AKID-MOCK");

    std::env::remove_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    let request = server.join().expect("mock server thread");
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: file-pod-token"));
}

// ---------------------------------------------------------------------------
// P2: the FULL_URI loopback/link-local host restriction
// ---------------------------------------------------------------------------

#[test]
fn full_uri_host_check_allows_only_loopback_and_link_local() {
    for allowed in [
        "http://localhost/creds",
        "http://LOCALHOST:2777/creds",
        "http://127.0.0.1:8080/v2/credentials",
        "http://127.254.1.2/creds",
        "http://[::1]:9/creds",
        "http://169.254.170.2/v2/credentials",
        "http://169.254.170.23/creds",
        "http://169.254.12.34/creds",
    ] {
        assert!(
            ecs_full_uri_host_allowed(allowed),
            "{allowed} must be allowed"
        );
    }
    for rejected in [
        "http://example.com/creds",
        "http://metadata.google.internal/creds",
        "http://10.1.2.3/creds",
        "http://192.168.0.5/creds",
        "http://172.16.0.9/creds",
        "http://169.255.0.1/creds",
        "http://[fe80::1]/creds",
        "not a url",
    ] {
        assert!(
            !ecs_full_uri_host_allowed(rejected),
            "{rejected} must be rejected"
        );
    }
}

#[test]
fn plan_accepts_link_local_full_uri_and_rejects_other_hosts() {
    assert_eq!(
        plan_source(
            None,
            None,
            None,
            None,
            None,
            Some("http://169.254.170.23/v2/creds")
        ),
        SourcePlan::Ecs {
            url: "http://169.254.170.23/v2/creds".to_string()
        }
    );
    assert_eq!(
        plan_source(
            None,
            None,
            None,
            None,
            None,
            Some("http://internal.example/creds")
        ),
        SourcePlan::InvalidEcsHost {
            value: "http://internal.example/creds".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// Security follow-ups: STS region validation
// ---------------------------------------------------------------------------

#[test]
fn sts_region_validation_accepts_only_region_shaped_strings() {
    for valid in [
        "us-east-1",
        "cn-north-1",
        "eu-west-1",
        "us-gov-west-1",
        "a-0",
    ] {
        assert!(is_valid_sts_region(valid), "{valid} must be valid");
    }
    for invalid in [
        "",
        "foo.attacker.com#",
        "us_east_1",
        "US-EAST-1",
        "us east 1",
        "../../evil",
        "us-east-1\n",
    ] {
        assert!(!is_valid_sts_region(invalid), "{invalid:?} must be invalid");
    }
}
