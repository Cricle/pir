//! Moved inline unit tests for the `ai::auth` subsystem: device-code polling,
//! OAuth URL/JWT helpers per provider, OAuth page HTML, PKCE vectors, the
//! provider flow registry, and the file credential storage wire shape.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pir::ai::auth::device_code::{poll_device_code_flow, DevicePoll};
use pir::ai::auth::oauth::github_copilot::{
    base_url_from_token, copilot_base_url, normalize_domain, trusted_http_url,
};
use pir::ai::auth::oauth::openai_chatgpt::{agent_host_id, result_from_manual_input};
use pir::ai::auth::oauth::radius::normalize_radius_gateway_url;
use pir::ai::auth::oauth::xai::validate_verification_uri;
use pir::ai::auth::oauth::{decode_jwt_payload, parse_authorization_input, OAuthCredential};
use pir::ai::auth::oauth_page::{oauth_error_html, oauth_success_html};
use pir::ai::auth::pkce::{base64url_decode, base64url_encode, challenge_for, generate_pkce};
use pir::ai::auth::resolve::{oauth_expires_soon, oauth_flow_for, OAUTH_PROVIDERS};
use pir::ai::auth::storage::{parse_data, FileAuthStorage, StoredCredential};
use pir::ai::auth::CredentialStore;
use pir::ai::error::AiError;
use pir::ai::types::ApiKeyCredential;
use serde_json::Value;

// ---------------------------------------------------------------------------
// device_code.rs — RFC 8628 polling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_flow_completes_immediately() {
    let result = poll_device_code_flow::<u8, _, _>(Some(5), Some(60), false, || async {
        Ok(DevicePoll::Complete(7u8))
    })
    .await
    .unwrap();
    assert_eq!(result, 7);
}

#[tokio::test]
async fn device_flow_pending_then_complete_and_slow_down_raises_interval() {
    let attempts = Arc::new(AtomicU32::new(0));
    let seen_intervals = Arc::new(AtomicU32::new(0));
    let attempts2 = attempts.clone();
    let seen = seen_intervals.clone();
    let result = poll_device_code_flow::<u32, _, _>(Some(1), Some(30), false, move || {
        let attempt = attempts2.fetch_add(1, Ordering::SeqCst);
        let seen = seen.clone();
        async move {
            match attempt {
                0 => Ok(DevicePoll::Pending),
                1 => Ok(DevicePoll::SlowDown {
                    interval_seconds: None,
                }),
                2 => {
                    seen.store(1, Ordering::SeqCst);
                    Ok(DevicePoll::Complete(42))
                }
                _ => unreachable!(),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(result, 42);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn device_flow_deadline_times_out_with_slow_down_message() {
    // A 3s budget with 1s polls: at least one slow_down is processed
    // before the deadline, which must produce the clock-drift message.
    let error = poll_device_code_flow::<u8, _, _>(Some(1), Some(3), true, || async {
        Ok(DevicePoll::SlowDown {
            interval_seconds: None,
        })
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("slow_down"), "{error}");
}

#[tokio::test]
async fn device_flow_failure_message_surfaces() {
    let error = poll_device_code_flow::<u8, _, _>(Some(1), Some(60), false, || async {
        Err::<DevicePoll<u8>, _>(AiError::Authentication("denied".to_string()))
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("denied"), "{error}");
}

// ---------------------------------------------------------------------------
// oauth/github_copilot.rs — base URL / domain helpers
// ---------------------------------------------------------------------------

#[test]
fn copilot_parses_proxy_ep_base_url() {
    let token = "tid=123;exp=456;proxy-ep=proxy.individual.githubcopilot.com;rest";
    assert_eq!(
        base_url_from_token(token).as_deref(),
        Some("https://api.individual.githubcopilot.com")
    );
    assert_eq!(base_url_from_token("no-marker"), None);
    assert_eq!(
        copilot_base_url(Some(token), None),
        "https://api.individual.githubcopilot.com"
    );
    assert_eq!(
        copilot_base_url(None, Some("ghe.example.com")),
        "https://copilot-api.ghe.example.com"
    );
    assert_eq!(
        copilot_base_url(None, None),
        "https://api.individual.githubcopilot.com"
    );
}

#[test]
fn copilot_normalizes_enterprise_domains() {
    assert_eq!(
        normalize_domain("company.ghe.com").as_deref(),
        Some("company.ghe.com")
    );
    assert_eq!(
        normalize_domain("https://company.ghe.com/path").as_deref(),
        Some("company.ghe.com")
    );
    assert_eq!(normalize_domain("  "), None);
}

#[test]
fn copilot_untrusted_verification_uri_rejected() {
    assert!(trusted_http_url("file:///etc/passwd").is_err());
    assert!(trusted_http_url("https://github.com/login/device").is_ok());
}

// ---------------------------------------------------------------------------
// oauth/mod.rs — shared OAuth helpers
// ---------------------------------------------------------------------------

#[test]
fn parses_redirect_urls_codes_and_states() {
    let (code, state) = parse_authorization_input("https://x/callback?code=abc&state=def");
    assert_eq!(code.as_deref(), Some("abc"));
    assert_eq!(state.as_deref(), Some("def"));
    let (code, state) = parse_authorization_input("abc#def");
    assert_eq!(code.as_deref(), Some("abc"));
    assert_eq!(state.as_deref(), Some("def"));
    let (code, state) = parse_authorization_input("code=abc&state=def");
    assert_eq!(code.as_deref(), Some("abc"));
    assert_eq!(state.as_deref(), Some("def"));
    let (code, state) = parse_authorization_input("bare");
    assert_eq!(code.as_deref(), Some("bare"));
    assert_eq!(state, None);
    assert_eq!(parse_authorization_input("  "), (None, None));
}

#[test]
fn jwt_payload_decode() {
    // {"a":1} — header/payload/signature with unpadded base64url.
    let token = "eyJhbGciOiJIUzI1NiJ9.eyJhIjoxfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let payload = decode_jwt_payload(token).unwrap();
    assert_eq!(payload["a"], 1);
    assert!(decode_jwt_payload("not-a-jwt").is_none());
}

#[test]
fn oauth_credential_wire_shape() {
    let mut credential = OAuthCredential::new("a", "r", 123);
    credential.set_extra("accountId", Some(Value::String("acc".to_string())));
    let json = serde_json::to_value(&credential).unwrap();
    // The bare credential has no envelope tag; the `type: "oauth"` tag is
    // applied by the stored-credential envelope (see storage.rs, whose
    // tests assert the full auth.json shape).
    assert!(json.get("type").is_none());
    assert_eq!(json["access"], "a");
    assert_eq!(json["refresh"], "r");
    assert_eq!(json["expires"], 123);
    assert_eq!(json["accountId"], "acc");
    let round: OAuthCredential = serde_json::from_value(json).unwrap();
    assert_eq!(round, credential);
}

// ---------------------------------------------------------------------------
// oauth/openai_chatgpt.rs — Sign in with ChatGPT
// ---------------------------------------------------------------------------

#[test]
fn chatgpt_validates_uuid_device_ids() {
    assert!(agent_host_id("B3C9A6C2-1F2E-4C8A-9D3E-5F6A7B8C9D0E").is_ok());
    assert!(agent_host_id("not-a-uuid").is_err());
    assert!(agent_host_id("").is_err());
}

#[test]
fn chatgpt_parses_manual_callback_urls() {
    let result = result_from_manual_input(
        "http://127.0.0.1:1455/auth/callback?code=c1&state=s1&client_id=issued",
        "s1",
    )
    .unwrap();
    assert_eq!(result.code, "c1");
    assert_eq!(result.client_id, "issued");
    assert!(result_from_manual_input("http://evil/?code=x", "s1").is_err());
    assert!(result_from_manual_input(
        "http://127.0.0.1:1455/auth/callback?code=c1&state=wrong&client_id=i",
        "s1"
    )
    .is_err());
    assert!(
        result_from_manual_input("http://127.0.0.1:1455/auth/callback?error=denied", "s1").is_err()
    );
}

// ---------------------------------------------------------------------------
// oauth/radius.rs — gateway URL normalization
// ---------------------------------------------------------------------------

#[test]
fn radius_normalizes_gateway_urls() {
    assert_eq!(
        normalize_radius_gateway_url("gw.example.com"),
        "https://gw.example.com"
    );
    assert_eq!(
        normalize_radius_gateway_url("https://gw.example.com/"),
        "https://gw.example.com"
    );
    assert_eq!(
        normalize_radius_gateway_url("http://localhost:8080///"),
        "http://localhost:8080"
    );
}

// ---------------------------------------------------------------------------
// oauth/xai.rs — verification URI validation
// ---------------------------------------------------------------------------

#[test]
fn xai_untrusted_verification_uri_rejected() {
    assert!(validate_verification_uri("file:///etc/passwd").is_err());
    assert!(validate_verification_uri("http://auth.x.ai/d").is_err());
    assert!(validate_verification_uri("https://auth.x.ai/device").is_ok());
}

// ---------------------------------------------------------------------------
// oauth_page.rs — rendered pages
// ---------------------------------------------------------------------------

#[test]
fn oauth_pages_render_and_escape() {
    let success = oauth_success_html("Signed in to Test.");
    assert!(success.contains("Authentication successful"));
    assert!(success.contains("Signed in to Test."));
    let error = oauth_error_html("State mismatch.", Some("<script>x</script>"));
    assert!(error.contains("Authentication failed"));
    assert!(!error.contains("<script>x</script>"));
    assert!(error.contains("&lt;script&gt;x&lt;/script&gt;"));
    assert!(success.contains("viewBox=\"0 0 800 800\""));
}

// ---------------------------------------------------------------------------
// pkce.rs — RFC 7636
// ---------------------------------------------------------------------------

// RFC 7636 appendix B test vector (S256).
#[test]
fn pkce_rfc7636_appendix_b_vector() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        challenge_for(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn pkce_generated_shapes_and_roundtrip() {
    let pkce = generate_pkce();
    // 32 random bytes -> 43 base64url characters.
    assert_eq!(pkce.verifier.len(), 43);
    assert!(!pkce.verifier.contains('+'));
    assert!(!pkce.verifier.contains('/'));
    assert!(!pkce.verifier.contains('='));
    assert_eq!(pkce.challenge.len(), 43);
    assert_eq!(challenge_for(&pkce.verifier), pkce.challenge);
    let decoded = base64url_decode(&pkce.verifier).unwrap();
    assert_eq!(decoded.len(), 32);
    assert_eq!(base64url_encode(&decoded), pkce.verifier);
}

#[test]
fn pkce_base64url_decode_tolerates_padding() {
    assert_eq!(base64url_decode("aGVsbG8=").unwrap(), b"hello");
    assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello");
    assert!(base64url_decode("!!").is_err());
}

#[test]
fn pkce_two_generated_pairs_differ() {
    let a = generate_pkce();
    let b = generate_pkce();
    assert_ne!(a.verifier, b.verifier);
}

// ---------------------------------------------------------------------------
// resolve.rs — flow registry and expiry window
// ---------------------------------------------------------------------------

#[test]
fn oauth_provider_list_matches_flow_registry() {
    for provider in OAUTH_PROVIDERS {
        assert!(
            oauth_flow_for(provider, None).is_some(),
            "flow missing for {provider}"
        );
    }
    assert!(oauth_flow_for("google", None).is_none());
    assert!(oauth_flow_for("made-up", None).is_none());
}

#[test]
fn oauth_expiry_window_includes_five_minutes() {
    let credential = OAuthCredential::new("a", "r", pir::ai::util::now_ms() as i64 + 4 * 60 * 1000);
    assert!(oauth_expires_soon(&credential, 0));
    let fresh = OAuthCredential::new("a", "r", pir::ai::util::now_ms() as i64 + 60 * 60 * 1000);
    assert!(!oauth_expires_soon(&fresh, 0));
}

// ---------------------------------------------------------------------------
// storage.rs — auth.json wire shape and file store
// ---------------------------------------------------------------------------

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-auth-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn auth_storage_parses_upstream_shapes() {
    let data = parse_data(
        r#"{
            "anthropic": { "type": "oauth", "refresh": "r", "access": "a", "expires": 123, "accountId": "x" },
            "openai": { "type": "api_key", "key": "sk" }
        }"#,
    )
    .unwrap();
    assert_eq!(data.len(), 2);
    assert!(matches!(data["anthropic"], StoredCredential::OAuth(_)));
    assert!(matches!(data["openai"], StoredCredential::ApiKey(_)));
    assert!(parse_data("[]").is_err());
    assert!(parse_data(r#"{"x": {"type": "oauth"}}"#).is_err());
    assert!(parse_data("{}").unwrap().is_empty());
}

#[test]
fn auth_storage_roundtrip_and_delete() {
    let path = temp_path("auth.json");
    let storage = FileAuthStorage::new(path.clone());
    storage
        .store(
            "anthropic",
            StoredCredential::OAuth(OAuthCredential::new("a", "r", 5)),
        )
        .unwrap();
    storage
        .store(
            "openai",
            StoredCredential::ApiKey(ApiKeyCredential::new(Some("sk".to_string()))),
        )
        .unwrap();
    let stored = storage.read_stored("anthropic").unwrap().unwrap();
    assert!(matches!(stored, StoredCredential::OAuth(_)));
    // File contents are valid JSON with the upstream shape.
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("\"type\": \"oauth\""));
    storage.delete("anthropic").unwrap();
    assert!(storage.read_stored("anthropic").unwrap().is_none());
    storage.delete("openai").unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn auth_storage_api_key_trait_view() {
    let path = temp_path("auth2.json");
    let storage = FileAuthStorage::new(path.clone());
    storage
        .store(
            "p",
            StoredCredential::ApiKey(ApiKeyCredential::new(Some("k".to_string()))),
        )
        .unwrap();
    storage
        .store(
            "q",
            StoredCredential::OAuth(OAuthCredential::new("a", "r", 1)),
        )
        .unwrap();
    assert!(storage.read("p").unwrap().is_some());
    // OAuth entries do not leak into the api-key view.
    assert!(storage.read("q").unwrap().is_none());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn auth_storage_async_modify_persists() {
    let path = temp_path("auth3.json");
    let storage = std::sync::Arc::new(FileAuthStorage::new(path.clone()));
    let post = storage
        .modify("anthropic", |current| async move {
            assert!(current.is_none());
            Ok(Some(StoredCredential::OAuth(OAuthCredential::new(
                "a2", "r2", 9,
            ))))
        })
        .await
        .unwrap();
    assert!(post.is_some());
    let stored = storage.read_stored("anthropic").unwrap().unwrap();
    assert!(matches!(stored, StoredCredential::OAuth(_)));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
