//! Bedrock credential-chain tests (task G5 / P1-5): shared profile INI
//! parsing, chain resolution order, ECS/STS response parsing, and local
//! mock-server round trips for the ECS fetch and the anonymous
//! `sts:AssumeRoleWithWebIdentity` call. No real AWS requests are made.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;

use pir::ai::api::bedrock::credentials::{
    parse_ecs_credentials, parse_ini, parse_profile_files, parse_sts_credentials, plan_source,
    profile_credentials_from, resolve_profile, sts_assume_role_with_web_identity, sts_endpoint,
    SourcePlan,
};
use pir::ai::error::AiError;

// ---------------------------------------------------------------------------
// Shared profile INI parsing
// ---------------------------------------------------------------------------

#[test]
fn ini_parses_sections_keys_comments_and_quotes() {
    let content = "\
# leading comment
[default]
aws_access_key_id = AKID-DEFAULT
; semicolon comment
aws_secret_access_key=\"quoted secret\"
region = us-west-2

[dev #1]
aws_session_token = 'single quoted'
";
    let sections = parse_ini(content);
    let default = sections.get("default").expect("default section");
    assert_eq!(default.get("aws_access_key_id").unwrap(), "AKID-DEFAULT");
    assert_eq!(
        default.get("aws_secret_access_key").unwrap(),
        "quoted secret"
    );
    assert_eq!(default.get("region").unwrap(), "us-west-2");
    let dev = sections.get("dev #1").expect("dev section");
    assert_eq!(dev.get("aws_session_token").unwrap(), "single quoted");
}

#[test]
fn ini_handles_backslash_continuation() {
    // NOTE: `AKID-\\\n\` — the INI value carries a literal trailing backslash
    // (`\\` in the literal), followed by a real newline (`\n`), followed by the
    // Rust line-continuation `\` that strips the source-line indent. A single
    // `\n\` here would let Rust swallow the backslash as line continuation,
    // leaving no continuation marker for the parser.
    let content = "[default]\n\
                   aws_access_key_id = AKID-\\\n\
                   CONTINUED\n\
                   aws_secret_access_key = s3cret\n";
    let sections = parse_ini(content);
    let default = sections.get("default").expect("default section");
    assert_eq!(default.get("aws_access_key_id").unwrap(), "AKID- CONTINUED");
    assert_eq!(default.get("aws_secret_access_key").unwrap(), "s3cret");
}

#[test]
fn ini_skips_malformed_lines() {
    // The unterminated header does not open a section and an empty key is
    // dropped; keys before any section land in the anonymous "" bucket that
    // profile lookups never read.
    let content = "[bad\nno_equals_line\n[section]\n= empty_key\nkey = ok\n";
    let sections = parse_ini(content);
    assert!(!sections.contains_key("bad"));
    let section = sections.get("section").expect("section");
    assert_eq!(section.get("key").unwrap(), "ok");
    assert!(!section.contains_key(""));
}

// ---------------------------------------------------------------------------
// Cross-file profile merging
// ---------------------------------------------------------------------------

fn sample_files() -> pir::ai::api::bedrock::credentials::AwsProfileFiles {
    let credentials = "\
[default]
aws_access_key_id = AKID-CREDS
aws_secret_access_key = secret-from-credentials
aws_session_token = token-from-credentials

[dev]
aws_access_key_id = AKID-DEV
aws_secret_access_key = dev-secret
";
    let config = "\
[default]
region = eu-central-1

[profile dev]
region = ap-northeast-1
output = json
";
    parse_profile_files(Some(credentials), Some(config))
}

#[test]
fn profile_merges_config_region_with_credentials_keys() {
    let files = sample_files();
    let resolved = profile_credentials_from(&files, "default", "test files")
        .expect("default profile should resolve");
    assert_eq!(resolved.credentials.access_key_id, "AKID-CREDS");
    assert_eq!(
        resolved.credentials.secret_access_key,
        "secret-from-credentials"
    );
    assert_eq!(
        resolved.credentials.session_token.as_deref(),
        Some("token-from-credentials")
    );
    assert_eq!(resolved.credentials.source, "profile:default");
    assert_eq!(resolved.region.as_deref(), Some("eu-central-1"));
}

#[test]
fn profile_config_sections_use_profile_prefix() {
    let files = sample_files();
    let resolved =
        profile_credentials_from(&files, "dev", "test files").expect("dev profile should resolve");
    assert_eq!(resolved.credentials.access_key_id, "AKID-DEV");
    assert_eq!(resolved.region.as_deref(), Some("ap-northeast-1"));
}

#[test]
fn profile_missing_is_a_typed_error() {
    let files = sample_files();
    let error = profile_credentials_from(&files, "ghost", "test files")
        .expect_err("missing profile must fail");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("ghost"));
}

#[test]
fn profile_with_role_arn_is_reported_unsupported() {
    let files = parse_profile_files(
        Some("[jump]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = default\n"),
        None,
    );
    let error = profile_credentials_from(&files, "jump", "test files")
        .expect_err("assume-role profiles must fail closed");
    assert!(error.to_string().contains("role_arn"));
}

#[test]
fn profile_without_credentials_fails_closed() {
    let files = parse_profile_files(None, Some("[profile only-region]\nregion = us-east-2\n"));
    let error = profile_credentials_from(&files, "only-region", "test files")
        .expect_err("profile without keys must fail");
    assert!(error.to_string().contains("missing"));
}

// ---------------------------------------------------------------------------
// Chain resolution order
// ---------------------------------------------------------------------------

#[test]
fn plan_prefers_explicit_profile_over_everything() {
    let plan = plan_source(
        Some("work"),
        Some("AKID"),
        Some("secret"),
        Some("/tokens/file"),
        Some("/v2/credentials"),
        None,
    );
    assert_eq!(
        plan,
        SourcePlan::Profile {
            name: "work".to_string()
        }
    );
}

#[test]
fn plan_static_env_requires_both_keys() {
    assert_eq!(
        plan_source(None, Some("AKID"), Some("secret"), None, None, None),
        SourcePlan::StaticEnv
    );
    assert_eq!(
        plan_source(None, Some("AKID"), None, None, None, None),
        SourcePlan::StaticEnvIncomplete
    );
    assert_eq!(
        plan_source(None, None, Some("secret"), None, None, None),
        SourcePlan::StaticEnvIncomplete
    );
}

#[test]
fn plan_order_web_identity_then_ecs() {
    assert_eq!(
        plan_source(None, None, None, Some("/tokens/file"), None, None),
        SourcePlan::WebIdentity
    );
    assert_eq!(
        plan_source(
            None,
            None,
            None,
            None,
            Some("/v2/credentials"),
            Some("http://localhost/x")
        ),
        SourcePlan::Ecs {
            url: "http://169.254.170.2/v2/credentials".to_string()
        }
    );
    assert_eq!(
        plan_source(
            None,
            None,
            None,
            None,
            None,
            Some("http://localhost:9876/creds")
        ),
        SourcePlan::Ecs {
            url: "http://localhost:9876/creds".to_string()
        }
    );
}

#[test]
fn plan_rejects_non_http_full_uri_and_reports_missing() {
    assert_eq!(
        plan_source(None, None, None, None, None, Some("unix:///tmp/sock")),
        SourcePlan::InvalidEcsUri {
            value: "unix:///tmp/sock".to_string()
        }
    );
    assert_eq!(
        plan_source(None, None, None, None, None, None),
        SourcePlan::NoCredentials
    );
}

// ---------------------------------------------------------------------------
// ECS credentials payload
// ---------------------------------------------------------------------------

#[test]
fn ecs_payload_parses_with_optional_token() {
    let credentials =
        parse_ecs_credentials(r#"{"AccessKeyId":"AKID-ECS","SecretAccessKey":"ecs-secret","Token":"ecs-token","Expiration":"2026-10-09T00:00:00Z"}"#)
            .expect("ECS payload should parse");
    assert_eq!(credentials.access_key_id, "AKID-ECS");
    assert_eq!(credentials.secret_access_key, "ecs-secret");
    assert_eq!(credentials.session_token.as_deref(), Some("ecs-token"));
    assert_eq!(credentials.source, "ecs-container");
    let no_token =
        parse_ecs_credentials(r#"{"AccessKeyId":"AKID-ECS","SecretAccessKey":"ecs-secret"}"#)
            .expect("payload without Token should parse");
    assert!(no_token.session_token.is_none());
}

#[test]
fn ecs_payload_rejects_malformed_json_and_missing_fields() {
    let not_json = parse_ecs_credentials("not json").expect_err("bad JSON must fail");
    assert!(matches!(not_json, AiError::Authentication(_)));
    let missing =
        parse_ecs_credentials(r#"{"AccessKeyId":"AKID"}"#).expect_err("missing secret must fail");
    assert!(matches!(missing, AiError::Authentication(_)));
}

// ---------------------------------------------------------------------------
// STS AssumeRoleWithWebIdentity response
// ---------------------------------------------------------------------------

const STS_RESPONSE: &str = r#"<?xml version="1.0"?>
<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>AKID-WEB</AccessKeyId>
      <SecretAccessKey>web-secret</SecretAccessKey>
      <SessionToken>web-token</SessionToken>
      <Expiration>2026-10-09T00:00:00Z</Expiration>
    </Credentials>
    <SubjectFromWebIdentityToken>amzn1.account</SubjectFromWebIdentityToken>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>"#;

#[test]
fn sts_xml_parses_temporary_credentials() {
    let credentials = parse_sts_credentials(STS_RESPONSE).expect("STS response should parse");
    assert_eq!(credentials.access_key_id, "AKID-WEB");
    assert_eq!(credentials.secret_access_key, "web-secret");
    assert_eq!(credentials.session_token.as_deref(), Some("web-token"));
}

#[test]
fn sts_xml_rejects_incomplete_credentials() {
    let truncated = r#"<Credentials><AccessKeyId>AKID</AccessKeyId></Credentials>"#;
    let error = parse_sts_credentials(truncated).expect_err("incomplete XML must fail");
    assert!(matches!(error, AiError::Authentication(_)));
}

#[test]
fn sts_endpoint_uses_cn_suffix_for_cn_regions() {
    assert_eq!(
        sts_endpoint("us-east-1"),
        "https://sts.us-east-1.amazonaws.com"
    );
    assert_eq!(
        sts_endpoint("cn-north-1"),
        "https://sts.cn-north-1.amazonaws.com.cn"
    );
}

// ---------------------------------------------------------------------------
// Local mock round trips (bounded-timeout HTTP against 127.0.0.1)
// ---------------------------------------------------------------------------

/// Serve exactly one HTTP response and capture the request bytes.
fn serve_once(status_line: &str, content_type: &str, body: &str) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
    let addr = listener.local_addr().expect("local addr");
    let response = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one connection");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until the full request body arrived (per Content-Length) so a
        // single TCP segment split never truncates the captured request.
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&request);
            // Once the header block is complete, the request is fully read
            // when the body reaches Content-Length (0 when the client omits
            // the header, e.g. for bodyless GETs).
            let Some(head_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let length = text[..head_end]
                .to_ascii_lowercase()
                .split("\r\n")
                .find_map(|header| header.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if text.len() - head_end - 4 >= length {
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

#[tokio::test]
async fn ecs_fetch_round_trips_a_local_mock() {
    let (url, server) = serve_once(
        "HTTP/1.1 200 OK",
        "application/json",
        r#"{"AccessKeyId":"AKID-MOCK","SecretAccessKey":"mock-secret","Token":"mock-token"}"#,
    );
    let credentials = pir::ai::api::bedrock::credentials::fetch_ecs_credentials(&url)
        .await
        .expect("mock ECS fetch should succeed");
    assert_eq!(credentials.access_key_id, "AKID-MOCK");
    assert_eq!(credentials.session_token.as_deref(), Some("mock-token"));
    let request = server.join().expect("mock server thread");
    assert!(request.starts_with("GET / "));
    assert!(request
        .to_ascii_lowercase()
        .contains("accept: application/json"));
}

#[tokio::test]
async fn ecs_fetch_fails_closed_on_error_status() {
    let (url, server) = serve_once("HTTP/1.1 404 Not Found", "text/plain", "gone");
    let error = pir::ai::api::bedrock::credentials::fetch_ecs_credentials(&url)
        .await
        .expect_err("404 must fail closed");
    assert!(matches!(error, AiError::Authentication(_)));
    assert!(error.to_string().contains("404"));
    let _ = server.join();
}

#[tokio::test]
async fn sts_web_identity_round_trips_a_local_mock() {
    let (url, server) = serve_once("HTTP/1.1 200 OK", "text/xml", STS_RESPONSE);
    let credentials = sts_assume_role_with_web_identity(
        &url,
        "arn:aws:iam::123456789012:role/pir-test",
        "pir-session",
        "web-token-value",
    )
    .await
    .expect("mock STS call should succeed");
    assert_eq!(credentials.access_key_id, "AKID-WEB");
    assert_eq!(credentials.source, "web-identity");
    let request = server.join().expect("mock server thread");
    // The anonymous form call carries the action, role, and token fields
    // (URL-encoded by the HTTP client).
    assert!(request.starts_with("POST / "));
    assert!(request.contains("Action=AssumeRoleWithWebIdentity"));
    assert!(request.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fpir-test"));
    assert!(request.contains("WebIdentityToken=web-token-value"));
}

// ---------------------------------------------------------------------------
// IAM Identity Center (SSO) profiles
//
// The token-cache lookup is hermetic (AWS_SSO_CACHE_DIR); these cases cover
// every fail-closed path that runs BEFORE the portal request, so no network
// is touched.
// ---------------------------------------------------------------------------

/// Serializes tests that mutate `AWS_SSO_CACHE_DIR` and restores it on drop.
struct SsoCacheDir {
    _guard: std::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}

static SSO_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl SsoCacheDir {
    fn new(tag: &str) -> Self {
        let guard = SSO_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("pir-sso-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create SSO cache dir");
        std::env::set_var("AWS_SSO_CACHE_DIR", &dir);
        Self { _guard: guard, dir }
    }
}

impl Drop for SsoCacheDir {
    fn drop(&mut self) {
        std::env::remove_var("AWS_SSO_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sso_files(config: &str) -> pir::ai::api::bedrock::credentials::AwsProfileFiles {
    parse_profile_files(None, Some(config))
}

#[tokio::test]
async fn sso_profile_without_cached_token_fails_before_network() {
    let _cache = SsoCacheDir::new("missing");
    let files = sso_files(
        "[profile sso]\n\
         sso_session = corp\n\
         sso_account_id = 123456789012\n\
         sso_role_name = Developer\n\
         \n\
         [sso-session corp]\n\
         sso_start_url = https://example.awsapps.com/start\n\
         sso_region = us-east-1\n",
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("no cached token must fail closed");
    let message = error.to_string();
    assert!(
        message.contains("no cached, unexpired SSO access token"),
        "{message}"
    );
    assert!(message.contains("aws sso login"), "{message}");
}

#[tokio::test]
async fn sso_profile_referencing_an_undefined_session_fails_closed() {
    let _cache = SsoCacheDir::new("undefined");
    let files = sso_files(
        "[profile sso]\n\
         sso_session = ghost\n\
         sso_account_id = 123456789012\n\
         sso_role_name = Developer\n",
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("undefined sso-session must fail closed");
    assert!(
        error.to_string().contains("references sso-session 'ghost'"),
        "{error}"
    );
}

#[tokio::test]
async fn sso_profile_missing_account_or_role_fails_closed() {
    let _cache = SsoCacheDir::new("incomplete");
    let files = sso_files(
        "[profile sso]\n\
         sso_start_url = https://example.awsapps.com/start\n\
         sso_region = us-east-1\n\
         sso_account_id = 123456789012\n",
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("missing sso_role_name must fail closed");
    assert!(
        error.to_string().contains("missing 'sso_role_name'"),
        "{error}"
    );

    // An SSO profile with neither account nor role is rejected before the
    // session lookup, with a message naming both keys.
    let files = sso_files(
        "[profile sso]\n\
         sso_start_url = https://example.awsapps.com/start\n\
         sso_region = us-east-1\n",
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("incomplete SSO profile must fail closed");
    assert!(
        error
            .to_string()
            .contains("uses SSO without 'sso_account_id'/'sso_role_name'"),
        "{error}"
    );
}

#[tokio::test]
async fn sso_expired_cached_token_is_not_used() {
    let cache = SsoCacheDir::new("expired");
    std::fs::write(
        cache.dir.join("token.json"),
        r#"{
            "accessToken": "expired-token",
            "expiresAt": "2000-01-01T00:00:00Z",
            "startUrl": "https://example.awsapps.com/start",
            "region": "us-east-1",
            "sessionName": "corp"
        }"#,
    )
    .expect("write SSO token cache");
    let files = sso_files(
        "[profile sso]\n\
         sso_session = corp\n\
         sso_account_id = 123456789012\n\
         sso_role_name = Developer\n\
         \n\
         [sso-session corp]\n\
         sso_start_url = https://example.awsapps.com/start\n\
         sso_region = us-east-1\n",
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("expired token must fail closed");
    assert!(
        error
            .to_string()
            .contains("no cached, unexpired SSO access token"),
        "{error}"
    );
}

/// The cached `expiresAt` is local, untrusted input: its year must stay in
/// `1970..=9999`. An out-of-range year is rejected by the parser and the
/// lookup treats the entry as expired (fail closed) instead of accepting an
/// absurd far-future timestamp.
#[test]
fn sso_iso_timestamp_rejects_out_of_range_years() {
    use pir::ai::api::bedrock::credentials::parse_iso8601_utc_seconds;

    assert!(parse_iso8601_utc_seconds("2026-10-25T12:00:00Z").is_some());
    assert!(parse_iso8601_utc_seconds("2026-10-25T12:00:00UTC").is_some());
    for invalid in [
        "1969-12-31T23:59:59Z",
        "0000-01-01T00:00:00Z",
        "10000-01-01T00:00:00Z",
        "99999-01-01T00:00:00Z",
        "-2026-01-01T00:00:00Z",
    ] {
        assert_eq!(
            parse_iso8601_utc_seconds(invalid),
            None,
            "out-of-range year must not parse: {invalid}"
        );
    }
}

#[tokio::test]
async fn sso_cache_entry_with_out_of_range_expiry_fails_closed() {
    let cache = SsoCacheDir::new("year-range");
    std::fs::write(
        cache.dir.join("token.json"),
        r#"{
            "accessToken": "far-future-token",
            "expiresAt": "10000-01-01T00:00:00Z",
            "startUrl": "https://example.awsapps.com/start",
            "region": "us-east-1",
            "sessionName": "corp"
        }"#,
    )
    .expect("write SSO token cache");
    let files = sso_files(
        "[profile sso]\n\
         sso_session = corp\n\
         sso_account_id = 123456789012\n\
         sso_role_name = Developer\n\
         \n\
         [sso-session corp]\n\
         sso_start_url = https://example.awsapps.com/start\n\
         sso_region = us-east-1\n",
    );
    let error = resolve_profile(&files, "sso", "test files")
        .await
        .expect_err("an out-of-range expiry must fail closed before the portal call");
    assert!(
        error
            .to_string()
            .contains("no cached, unexpired SSO access token"),
        "{error}"
    );
}
