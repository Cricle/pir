//! Bedrock credential-chain extension tests: `credential_process` (real
//! subprocess), SigV4-signed `sts:AssumeRole` against a local mock STS
//! endpoint, the IMDSv2 link against a local mock metadata service, and
//! chain-order assertions. No real AWS requests are made.
//!
//! Tests that touch process environment state serialize behind one mutex
//! (snapshotting guard) so they cannot interfere with each other.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::{Mutex, MutexGuard};

use pir::ai::api::bedrock::credentials::{
    imds_credentials, parse_credential_process_output, parse_imds_credentials, resolve_credentials,
    resolve_profile, run_credential_process, split_command_line, sts_assume_role,
};
use pir::ai::error::AiError;

// ---------------------------------------------------------------------------
// Environment serialization
// ---------------------------------------------------------------------------

/// Every chain-relevant environment variable, snapshotted and restored by
/// [`EnvGuard`].
const CHAIN_ENV_VARS: &[&str] = &[
    "AWS_PROFILE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_CONFIG_FILE",
    "AWS_EC2_METADATA_DISABLED",
    "AWS_EC2_METADATA_SERVICE_ENDPOINT",
];

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serializes env-touching tests and restores the snapshotted variables on
/// drop (panic-safe via `into_inner` on a poisoned lock).
struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn acquire() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = CHAIN_ENV_VARS
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        Self { saved, _lock: lock }
    }

    fn set(&self, name: &str, value: &str) {
        std::env::set_var(name, value);
    }

    fn remove(&self, name: &str) {
        std::env::remove_var(name);
    }

    /// Clear every chain variable (fresh-slate chain tests).
    fn clear_all(&self) {
        for name in CHAIN_ENV_VARS {
            std::env::remove_var(name);
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared-file fixtures
// ---------------------------------------------------------------------------

/// Write a fixture under a unique temp directory and point the shared-file
/// env overrides at it.
fn isolate_shared_files(name: &str, credentials: Option<&str>, config: Option<&str>) {
    let dir = std::env::temp_dir().join(format!(
        "pir-bedrock-chain-ext-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    for (file, content) in [("credentials", credentials), ("config", config)] {
        if let Some(content) = content {
            std::fs::write(dir.join(file), content).expect("fixture write");
        }
    }
    if credentials.is_some() {
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", dir.join("credentials"));
    } else {
        std::env::remove_var("AWS_SHARED_CREDENTIALS_FILE");
    }
    if config.is_some() {
        std::env::set_var("AWS_CONFIG_FILE", dir.join("config"));
    } else {
        std::env::remove_var("AWS_CONFIG_FILE");
    }
}

/// Bind a local port, drop the listener, and return the (now closed) port —
/// a deterministic fast connection-refused endpoint.
fn closed_local_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// HTTP request read from a mock connection: headers + full body.
struct MockRequest {
    first_line: String,
    raw: String,
}

fn read_mock_request(stream: &mut std::net::TcpStream) -> MockRequest {
    let mut raw: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("mock read");
        assert!(read > 0, "mock connection closed before headers completed");
        raw.extend_from_slice(&buffer[..read]);
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while raw.len() < header_end + content_length {
        let read = stream.read(&mut buffer).expect("mock body read");
        assert!(read > 0, "mock connection closed before body completed");
        raw.extend_from_slice(&buffer[..read]);
    }
    let first_line = headers.lines().next().unwrap_or_default().to_string();
    MockRequest {
        first_line,
        raw: String::from_utf8_lossy(&raw).to_string(),
    }
}

fn respond(stream: &mut std::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).expect("mock write");
    stream.flush().expect("mock flush");
    stream.shutdown(Shutdown::Both).ok();
}

// ---------------------------------------------------------------------------
// credential_process: command-line splitting (pure)
// ---------------------------------------------------------------------------

#[test]
fn split_command_line_splits_on_whitespace_and_groups_quotes() {
    assert_eq!(
        split_command_line("aws configure export-credentials --format process"),
        Some(vec![
            "aws".to_string(),
            "configure".to_string(),
            "export-credentials".to_string(),
            "--format".to_string(),
            "process".to_string(),
        ])
    );
    assert_eq!(
        split_command_line("python -c \"print('{\"a\": 1}')\""),
        Some(vec![
            "python".to_string(),
            "-c".to_string(),
            "print('{\"a\": 1}')".to_string(),
        ])
    );
    assert_eq!(
        split_command_line("'C:/Program Files/helper.exe' --flag"),
        Some(vec![
            "C:/Program Files/helper.exe".to_string(),
            "--flag".to_string(),
        ])
    );
    assert_eq!(split_command_line("   "), None);
    assert_eq!(split_command_line("echo 'unterminated"), None);
}

// ---------------------------------------------------------------------------
// credential_process: real subprocess
// ---------------------------------------------------------------------------

/// Build a `credential_process` command line whose subprocess prints the
/// given text on stdout (executed directly by pir, no shell on pir's side).
#[cfg(windows)]
fn stdout_echo_command(text: &str) -> String {
    // PowerShell's single-quoted literal is passed through verbatim, so the
    // JSON's double quotes survive.
    format!("powershell.exe -NoProfile -Command \"Write-Output '{text}'\"")
}

#[cfg(unix)]
fn stdout_echo_command(text: &str) -> String {
    format!("/bin/sh -c \"printf '%s' '{text}'\"")
}

const CREDENTIAL_PROCESS_JSON: &str = r#"{"AccessKeyId":"AKID-CP","SecretAccessKey":"cp-secret","SessionToken":"cp-token","Expiration":"2026-10-09T00:00:00Z","Version":1}"#;

#[tokio::test]
async fn credential_process_resolves_via_real_subprocess() {
    let command = stdout_echo_command(CREDENTIAL_PROCESS_JSON);
    let credentials = run_credential_process(&command)
        .await
        .expect("credential_process should resolve from subprocess stdout");
    assert_eq!(credentials.access_key_id, "AKID-CP");
    assert_eq!(credentials.secret_access_key, "cp-secret");
    assert_eq!(credentials.session_token.as_deref(), Some("cp-token"));
    assert_eq!(credentials.source, "credential-process");
}

#[tokio::test]
async fn credential_process_accepts_version_one_string_and_optional_token() {
    let command = stdout_echo_command(
        r#"{"AccessKeyId":"AKID-V1","SecretAccessKey":"v1-secret","Version":"1"}"#,
    );
    let credentials = run_credential_process(&command)
        .await
        .expect("Version \"1\" without SessionToken should parse");
    assert_eq!(credentials.access_key_id, "AKID-V1");
    assert_eq!(credentials.session_token, None);
}

#[tokio::test]
async fn credential_process_fails_closed_on_nonzero_exit() {
    #[cfg(windows)]
    let command = "powershell.exe -NoProfile -Command \"Write-Output 'partial'; exit 3\"";
    #[cfg(unix)]
    let command = "/bin/sh -c \"printf partial; exit 3\"";
    let error = run_credential_process(command)
        .await
        .expect_err("non-zero exit must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("exit code 3"));
}

#[tokio::test]
async fn credential_process_fails_closed_on_malformed_json() {
    let command = stdout_echo_command("not json at all");
    let error = run_credential_process(&command)
        .await
        .expect_err("non-JSON stdout must fail closed");
    assert!(error.to_string().contains("not valid JSON"));
}

#[tokio::test]
async fn credential_process_fails_closed_on_missing_fields() {
    let command = stdout_echo_command(r#"{"AccessKeyId":"AKID-ONLY"}"#);
    let error = run_credential_process(&command)
        .await
        .expect_err("missing SecretAccessKey must fail closed");
    assert!(error
        .to_string()
        .contains("missing AccessKeyId or SecretAccessKey"));
}

#[tokio::test]
async fn credential_process_fails_closed_on_unspawnable_program() {
    let error = run_credential_process("pir-definitely-not-a-real-binary-9021 --flag")
        .await
        .expect_err("an unknown program must fail closed");
    assert!(error.to_string().contains("failed to spawn"));
}

// ---------------------------------------------------------------------------
// credential_process: output parsing (pure)
// ---------------------------------------------------------------------------

#[test]
fn parse_credential_process_output_rejects_unsupported_version() {
    let error =
        parse_credential_process_output(r#"{"AccessKeyId":"A","SecretAccessKey":"B","Version":2}"#)
            .expect_err("Version 2 must be rejected");
    assert!(error.to_string().contains("Version 2"));
}

#[test]
fn parse_credential_process_output_rejects_invalid_json_and_empty_keys() {
    assert!(parse_credential_process_output("{").is_err());
    assert!(
        parse_credential_process_output(r#"{"AccessKeyId":"","SecretAccessKey":"B"}"#).is_err()
    );
}

// ---------------------------------------------------------------------------
// SSO-style assume-role (role_arn + source_profile)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sts_assume_role_signs_with_source_credentials_against_mock() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("mock accept");
        let request = read_mock_request(&mut stream);
        assert!(
            request.first_line.starts_with("POST /"),
            "unexpected request line: {}",
            request.first_line
        );
        assert!(
            request.raw.contains("authorization: AWS4-HMAC-SHA256"),
            "AssumeRole must be SigV4-signed: {}",
            request.raw
        );
        assert!(
            request.raw.contains("x-amz-security-token: source-token"),
            "the source session token must be sent: {}",
            request.raw
        );
        assert!(
            request
                .raw
                .contains("content-type: application/x-www-form-urlencoded"),
            "{}",
            request.raw
        );
        assert!(
            request
                .raw
                .contains("Action=AssumeRole&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A123%3Arole%2Ftarget&RoleSessionName=pir-session"),
            "form body mismatch: {}",
            request.raw
        );
        respond(
            &mut stream,
            "<AssumeRoleResponse><AssumeRoleResult><Credentials>\
             <AccessKeyId>AKID-AR</AccessKeyId>\
             <SecretAccessKey>ar-secret</SecretAccessKey>\
             <SessionToken>ar-token</SessionToken>\
             <Expiration>2026-10-09T00:00:00Z</Expiration>\
             </Credentials></AssumeRoleResult></AssumeRoleResponse>",
        );
    });
    let source = pir::ai::api::bedrock::credentials::AwsCredentials {
        access_key_id: "AKID-SOURCE".to_string(),
        secret_access_key: "source-secret".to_string(),
        session_token: Some("source-token".to_string()),
        source: "profile:base".to_string(),
    };
    let credentials = sts_assume_role(
        &format!("http://127.0.0.1:{port}/"),
        "us-east-1",
        "arn:aws:iam::123:role/target",
        "pir-session",
        &source,
    )
    .await
    .expect("mocked AssumeRole should resolve");
    assert_eq!(credentials.access_key_id, "AKID-AR");
    assert_eq!(credentials.secret_access_key, "ar-secret");
    assert_eq!(credentials.session_token.as_deref(), Some("ar-token"));
    assert_eq!(credentials.source, "assume-role");
    server.join().expect("mock server thread");
}

#[tokio::test]
async fn profile_role_arn_without_source_profile_is_a_typed_error() {
    let files = pir::ai::api::bedrock::credentials::parse_profile_files(
        Some("[jump]\nrole_arn = arn:aws:iam::1:role/x\n"),
        None,
    );
    let error = resolve_profile(&files, "jump", "test files")
        .await
        .expect_err("role_arn without source_profile must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("role_arn"));
    assert!(error.to_string().contains("source_profile"));
}

#[tokio::test]
async fn profile_sso_session_still_unsupported() {
    let files = pir::ai::api::bedrock::credentials::parse_profile_files(
        Some("[sso]\nsso_session = my-sso\nrole_arn = arn:aws:iam::1:role/x\n"),
        None,
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("sso_session profiles must fail closed");
    assert!(error.to_string().contains("sso_session"));
}

#[tokio::test]
async fn profile_source_profile_cycle_is_detected() {
    let files = pir::ai::api::bedrock::credentials::parse_profile_files(
        Some(
            "[a]\nrole_arn = arn:aws:iam::1:role/a\nsource_profile = b\n\
             [b]\nrole_arn = arn:aws:iam::1:role/b\nsource_profile = a\n",
        ),
        None,
    );
    let error = resolve_profile(&files, "a", "test files")
        .await
        .expect_err("an assume-role cycle must be detected");
    assert!(error.to_string().contains("cycle"));
}

#[tokio::test]
async fn profile_source_profile_depth_is_capped() {
    let mut content = String::new();
    for level in 0..8 {
        content.push_str(&format!(
            "[p{level}]\nrole_arn = arn:aws:iam::1:role/r{level}\nsource_profile = p{}\n",
            level + 1
        ));
    }
    let files = pir::ai::api::bedrock::credentials::parse_profile_files(Some(&content), None);
    let error = resolve_profile(&files, "p0", "test files")
        .await
        .expect_err("deeply nested assume-role chains must be capped");
    assert!(error.to_string().contains("nested deeper"));
}

#[tokio::test]
async fn profile_credential_process_feature_flag_flows_through_resolver() {
    // The resolver's feature dispatch happens before any network call for a
    // broken command, so this exercises the credential_process branch of
    // resolve_profile without spawning anything real.
    let files = pir::ai::api::bedrock::credentials::parse_profile_files(
        Some("[cp]\ncredential_process = \"   \"\n"),
        None,
    );
    let error = resolve_profile(&files, "cp", "test files")
        .await
        .expect_err("an empty credential_process must fail closed");
    assert!(error.to_string().contains("credential_process"));
}

// ---------------------------------------------------------------------------
// IMDS (EC2 instance role)
// ---------------------------------------------------------------------------

#[test]
fn imds_payload_requires_all_three_fields() {
    let parsed = parse_imds_credentials(
        r#"{"AccessKeyId":"AKID-IMDS","SecretAccessKey":"imds-secret","Token":"imds-token","Expiration":"2026-10-09T00:00:00Z"}"#,
    )
    .expect("IMDS payload should parse");
    assert_eq!(parsed.access_key_id, "AKID-IMDS");
    assert_eq!(parsed.session_token.as_deref(), Some("imds-token"));
    assert_eq!(parsed.source, "ec2-imds");
    assert!(parse_imds_credentials(r#"{"AccessKeyId":"A","SecretAccessKey":"B"}"#).is_err());
}

#[tokio::test]
async fn imds_full_flow_against_local_mock() {
    let _guard = EnvGuard::acquire();
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        for expected in ["PUT /latest/api/token", "role-listing", "role-document"] {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            let request = read_mock_request(&mut stream);
            match expected {
                "PUT /latest/api/token" => {
                    assert!(request.first_line.starts_with("PUT /latest/api/token"));
                    assert!(
                        request
                            .raw
                            .contains("x-aws-ec2-metadata-token-ttl-seconds: 21600"),
                        "token PUT must request the TTL: {}",
                        request.raw
                    );
                    respond(&mut stream, "mock-token");
                }
                "role-listing" => {
                    assert!(
                        request
                            .first_line
                            .starts_with("GET /latest/meta-data/iam/security-credentials/"),
                        "{}",
                        request.first_line
                    );
                    assert!(request.raw.contains("x-aws-ec2-metadata-token: mock-token"));
                    respond(&mut stream, "mock-role\n");
                }
                _ => {
                    assert!(
                        request.first_line.starts_with(
                            "GET /latest/meta-data/iam/security-credentials/mock-role"
                        ),
                        "{}",
                        request.first_line
                    );
                    assert!(request.raw.contains("x-aws-ec2-metadata-token: mock-token"));
                    respond(
                        &mut stream,
                        r#"{"AccessKeyId":"AKID-IMDS","SecretAccessKey":"imds-secret","Token":"imds-token","Expiration":"2026-10-09T00:00:00Z"}"#,
                    );
                }
            }
        }
    });
    _guard.set(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        &format!("http://127.0.0.1:{port}"),
    );
    _guard.remove("AWS_EC2_METADATA_DISABLED");
    let credentials = imds_credentials()
        .await
        .expect("the mocked IMDS flow should resolve");
    assert_eq!(credentials.access_key_id, "AKID-IMDS");
    assert_eq!(credentials.secret_access_key, "imds-secret");
    assert_eq!(credentials.session_token.as_deref(), Some("imds-token"));
    assert_eq!(credentials.source, "ec2-imds");
    server.join().expect("mock server thread");
}

#[tokio::test]
async fn imds_skips_silently_when_disabled() {
    let _guard = EnvGuard::acquire();
    _guard.set("AWS_EC2_METADATA_DISABLED", "true");
    assert!(imds_credentials().await.is_none());
}

#[tokio::test]
async fn imds_skips_silently_and_quickly_when_endpoint_unreachable() {
    let _guard = EnvGuard::acquire();
    _guard.set(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        &format!("http://127.0.0.1:{}", closed_local_port()),
    );
    _guard.remove("AWS_EC2_METADATA_DISABLED");
    let started = std::time::Instant::now();
    assert!(imds_credentials().await.is_none());
    assert!(
        started.elapsed() < IMDS_SKIP_BUDGET,
        "a dead endpoint must be skipped quickly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn imds_skips_silently_on_invalid_endpoint_override() {
    let _guard = EnvGuard::acquire();
    _guard.set(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        "http://evil.example.com",
    );
    _guard.remove("AWS_EC2_METADATA_DISABLED");
    assert!(imds_credentials().await.is_none());
}

#[tokio::test]
async fn imds_token_failure_skips_the_link() {
    let _guard = EnvGuard::acquire();
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("mock accept");
        let request = read_mock_request(&mut stream);
        assert!(request.first_line.starts_with("PUT /latest/api/token"));
        let response = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(response.as_bytes()).expect("mock write");
        stream.flush().expect("mock flush");
        stream.shutdown(Shutdown::Both).ok();
    });
    _guard.set(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        &format!("http://127.0.0.1:{port}"),
    );
    _guard.remove("AWS_EC2_METADATA_DISABLED");
    assert!(imds_credentials().await.is_none());
    server.join().expect("mock server thread");
}

/// Generous bound for the "skips quickly" assertion (the real budget is 3s).
const IMDS_SKIP_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Chain order (resolve_credentials, env-driven, serialized)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chain_order_profile_beats_static_env() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    isolate_shared_files(
        "order-profile-env",
        Some("[work]\naws_access_key_id = AKID-WORK\naws_secret_access_key = work-secret\n"),
        None,
    );
    _guard.set("AWS_PROFILE", "work");
    _guard.set("AWS_ACCESS_KEY_ID", "AKID-ENV");
    _guard.set("AWS_SECRET_ACCESS_KEY", "env-secret");
    let resolved = resolve_credentials(None).await.expect("profile wins");
    assert_eq!(resolved.credentials.access_key_id, "AKID-WORK");
    assert_eq!(resolved.credentials.source, "profile:work");
}

#[tokio::test]
async fn chain_order_static_env_beats_web_identity() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    let token_file = std::env::temp_dir().join(format!(
        "pir-bedrock-chain-ext-token-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&token_file, "dummy-token").expect("token fixture");
    _guard.set("AWS_ACCESS_KEY_ID", "AKID-ENV");
    _guard.set("AWS_SECRET_ACCESS_KEY", "env-secret");
    _guard.set(
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        &token_file.display().to_string(),
    );
    _guard.set("AWS_ROLE_ARN", "arn:aws:iam::1:role/x");
    let resolved = resolve_credentials(None).await.expect("env wins");
    assert_eq!(resolved.credentials.source, "environment");
    std::fs::remove_file(&token_file).ok();
}

#[tokio::test]
async fn chain_order_web_identity_beats_ecs() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    let token_file = std::env::temp_dir().join(format!(
        "pir-bedrock-chain-ext-token-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&token_file, "dummy-token").expect("token fixture");
    _guard.set(
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        &token_file.display().to_string(),
    );
    _guard.set("AWS_ROLE_ARN", "arn:aws:iam::1:role/x");
    // An invalid region makes the web-identity link fail closed before any
    // network call; the error text proves web identity was selected over ECS.
    _guard.set("AWS_REGION", "not a region");
    _guard.set(
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        &format!("http://127.0.0.1:{}/creds", closed_local_port()),
    );
    let error = resolve_credentials(None)
        .await
        .expect_err("web identity (invalid region) must be attempted before ECS");
    assert!(
        error.to_string().contains("not a valid AWS region"),
        "unexpected error: {error}"
    );
    std::fs::remove_file(&token_file).ok();
}

#[tokio::test]
async fn chain_order_ecs_beats_default_profile() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    isolate_shared_files(
        "order-ecs-profile",
        Some(
            "[default]\naws_access_key_id = AKID-DEFAULT\naws_secret_access_key = default-secret\n",
        ),
        None,
    );
    _guard.set(
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        &format!("http://127.0.0.1:{}/creds", closed_local_port()),
    );
    let error = resolve_credentials(None)
        .await
        .expect_err("ECS must be attempted before the default profile");
    assert!(
        error.to_string().contains("ECS container credentials"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn chain_order_default_profile_fail_closed_before_imds() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    // A present-but-incomplete default profile is a typed error even though
    // the IMDS link could (in principle) follow — profile precedes IMDS and
    // never silently degrades to it.
    isolate_shared_files(
        "order-profile-fail",
        Some("[default]\naws_access_key_id = A\n"),
        None,
    );
    _guard.set("AWS_EC2_METADATA_DISABLED", "true");
    let error = resolve_credentials(None)
        .await
        .expect_err("a broken default profile must fail closed");
    assert!(
        error.to_string().contains("missing"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn chain_order_imds_beats_terminal_no_credentials() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    // No shared files at all: the chain reaches IMDS (mocked here).
    std::env::remove_var("AWS_SHARED_CREDENTIALS_FILE");
    std::env::remove_var("AWS_CONFIG_FILE");
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        for expected in ["PUT /latest/api/token", "role-listing", "role-document"] {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            let _request = read_mock_request(&mut stream);
            match expected {
                "PUT /latest/api/token" => respond(&mut stream, "mock-token"),
                "role-listing" => respond(&mut stream, "mock-role\n"),
                _ => respond(
                    &mut stream,
                    r#"{"AccessKeyId":"AKID-IMDS","SecretAccessKey":"imds-secret","Token":"imds-token"}"#,
                ),
            }
        }
    });
    _guard.set(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT",
        &format!("http://127.0.0.1:{port}"),
    );
    let resolved = resolve_credentials(None)
        .await
        .expect("IMDS should resolve");
    assert_eq!(resolved.credentials.source, "ec2-imds");
    server.join().expect("mock server thread");
}

#[tokio::test]
async fn chain_order_terminal_no_credentials_mentions_every_link() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    std::env::remove_var("AWS_SHARED_CREDENTIALS_FILE");
    std::env::remove_var("AWS_CONFIG_FILE");
    _guard.set("AWS_EC2_METADATA_DISABLED", "true");
    let error = resolve_credentials(None)
        .await
        .expect_err("an empty chain must terminate with NoCredentials");
    let message = error.to_string();
    assert!(message.contains("No AWS credentials"), "{message}");
    assert!(message.contains("ECS task role"), "{message}");
    assert!(message.contains("EC2 instance role"), "{message}");
}

#[tokio::test]
async fn chain_profile_with_credential_process_resolves_end_to_end() {
    let _guard = EnvGuard::acquire();
    _guard.clear_all();
    let command = stdout_echo_command(CREDENTIAL_PROCESS_JSON);
    // Escape for INI (the value is used verbatim after INI unquoting).
    isolate_shared_files(
        "order-profile-credential-process",
        Some(&format!("[tool]\ncredential_process = {command}\n")),
        None,
    );
    _guard.set("AWS_PROFILE", "tool");
    let resolved = resolve_credentials(None)
        .await
        .expect("a credential_process profile should resolve end to end");
    assert_eq!(resolved.credentials.source, "credential-process");
    assert_eq!(resolved.credentials.access_key_id, "AKID-CP");
}
