//! `mcp-auth.json` storage contract:
//!
//! * a missing/empty/BOM-only file is an empty store (first run);
//! * a non-empty file that does not parse is an ERROR — a truncated file must
//!   never be silently reset, or one bad write would wipe every server's
//!   credentials on the next refresh;
//! * writes replace the file atomically (same-directory temp + rename) with
//!   private (`0600`) permissions on Unix.
#![cfg(feature = "mcp")]

use std::path::{Path, PathBuf};

use pir::mcp::oauth::{
    refresh_access_token, AuthorizationServerMetadata, McpOAuthCredentialStore, McpOAuthError,
    McpOAuthSettings, McpOAuthState, OAuthDiscoveryState, OAuthTokens,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-oauth-store-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
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

fn state(server_url: &str, access_token: &str) -> McpOAuthState {
    McpOAuthState {
        server_url: server_url.to_owned(),
        tokens: Some(OAuthTokens {
            access_token: access_token.to_owned(),
            token_type: "Bearer".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_missing_or_empty_store_is_an_empty_table() {
    let dir = TempDir::new("missing");
    let path = dir.path().join("mcp-auth.json");
    let store = McpOAuthCredentialStore::new(path.clone());
    let server = store.for_server("alpha", "https://alpha.example/sse");

    assert!(
        server.load().await.expect("missing file loads").is_none(),
        "a missing store has no state"
    );
    assert!(
        !server.remove().await.expect("remove on a missing store"),
        "nothing was stored"
    );

    std::fs::write(&path, "").expect("write empty store");
    assert!(
        server.load().await.expect("empty file loads").is_none(),
        "an empty store has no state"
    );
    std::fs::write(&path, "\u{feff}").expect("write BOM-only store");
    assert!(
        server.load().await.expect("BOM-only file loads").is_none(),
        "a BOM-only store has no state"
    );

    server
        .save(&state("https://alpha.example/sse", "token-a"))
        .await
        .expect("save after an empty store");
    assert!(
        server.load().await.expect("load saved state").is_some(),
        "the store is usable again"
    );
}

#[tokio::test]
async fn a_corrupt_store_errors_and_is_never_overwritten() {
    let dir = TempDir::new("corrupt");
    let path = dir.path().join("mcp-auth.json");
    let store = McpOAuthCredentialStore::new(path.clone());
    let alpha = store.for_server("alpha", "https://alpha.example/sse");
    let beta = store.for_server("beta", "https://beta.example/sse");

    alpha
        .save(&state("https://alpha.example/sse", "token-a"))
        .await
        .expect("save alpha");
    beta.save(&state("https://beta.example/sse", "token-b"))
        .await
        .expect("save beta");
    let saved = std::fs::read(&path).expect("read the saved store");
    let saved_text = String::from_utf8(saved.clone()).expect("utf-8 store");
    assert!(
        saved_text.contains("alpha.example") && saved_text.contains("beta.example"),
        "both servers persisted: {saved_text}"
    );

    // A truncated/otherwise corrupt file must fail the read-modify-write:
    // treating it as empty would drop both credentials on the next save.
    let corrupt = b"{ this is not json".to_vec();
    std::fs::write(&path, &corrupt).expect("corrupt the store");
    let error = alpha
        .update(|stored| stored.tokens = None)
        .await
        .expect_err("a corrupt store must be an explicit error");
    assert!(
        matches!(error, McpOAuthError::Fatal(_)),
        "parse failure is fatal: {error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read the corrupt store"),
        corrupt,
        "the error path must not rewrite the corrupt file"
    );
    assert!(
        alpha.load().await.is_err(),
        "loading a corrupt store reports the parse failure"
    );

    // Restoring the valid contents (e.g. after a manual repair) recovers both
    // servers: the error did not destroy the data.
    std::fs::write(&path, &saved).expect("restore the store");
    let loaded = alpha
        .load()
        .await
        .expect("load alpha")
        .expect("alpha state");
    assert_eq!(
        loaded
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("token-a")
    );
    let loaded = beta.load().await.expect("load beta").expect("beta state");
    assert_eq!(
        loaded
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("token-b")
    );
}

#[tokio::test]
async fn a_save_replaces_the_store_atomically_and_privately() {
    let dir = TempDir::new("atomic");
    let path = dir.path().join("mcp-auth.json");
    let store = McpOAuthCredentialStore::new(path.clone());
    let server = store.for_server("alpha", "https://alpha.example/sse");

    server
        .save(&state("https://alpha.example/sse", "token-one"))
        .await
        .expect("first save");

    #[cfg(unix)]
    let before = {
        use std::os::unix::fs::MetadataExt;
        let before = std::fs::metadata(&path).expect("metadata before the save");
        assert_eq!(
            before.mode() & 0o777,
            0o600,
            "a new credential file is private"
        );
        before
    };
    server
        .save(&state("https://alpha.example/sse", "token-two"))
        .await
        .expect("second save");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let after = std::fs::metadata(&path).expect("metadata after the save");
        assert_ne!(
            before.ino(),
            after.ino(),
            "the save replaces the file via temp + rename, not an in-place truncate"
        );
        assert_eq!(after.mode() & 0o777, 0o600, "the replacement stays private");
    }

    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .expect("list the store directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp file is left behind: {leftovers:?}"
    );

    let loaded = server.load().await.expect("load").expect("stored state");
    assert_eq!(
        loaded
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("token-two")
    );
}

/// Upstream #11743 (`bb266bf35`): a failed connection refresh is refresh-only.
/// Without a stored client it must not register one or replace a pending
/// sign-in's verifier/state; it reports that a sign-in is required instead.
#[tokio::test]
async fn a_refresh_without_a_client_keeps_a_pending_signin() {
    let dir = TempDir::new("refresh-only");
    let store = McpOAuthCredentialStore::new(dir.path().join("mcp-auth.json"));
    let server = store.for_server("alpha", "https://alpha.example/mcp");
    server
        .save(&McpOAuthState {
            server_url: "https://alpha.example/mcp".to_owned(),
            tokens: Some(OAuthTokens {
                access_token: "stale".to_owned(),
                token_type: "Bearer".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                ..OAuthTokens::default()
            }),
            code_verifier: Some("pending-verifier".to_owned()),
            oauth_state: Some("pending-state".to_owned()),
            discovery: Some(OAuthDiscoveryState {
                authorization_server_url: "https://auth.example".to_owned(),
                authorization_server_metadata: Some(AuthorizationServerMetadata {
                    issuer: "https://auth.example".to_owned(),
                    authorization_endpoint: "https://auth.example/authorize".to_owned(),
                    token_endpoint: "https://auth.example/token".to_owned(),
                    ..AuthorizationServerMetadata::default()
                }),
                resource_metadata: None,
                resource_metadata_url: None,
            }),
            ..McpOAuthState::default()
        })
        .await
        .expect("seed state");

    let settings = McpOAuthSettings::default();
    let error = refresh_access_token(&server, &settings, "https://alpha.example/mcp", None)
        .await
        .expect_err("a refresh without a client cannot authorize");
    assert!(
        matches!(error, McpOAuthError::AuthorizationRequired),
        "refresh reports that a sign-in is needed: {error:?}"
    );

    let after = server.load().await.expect("load").expect("stored state");
    assert!(
        after.client_information.is_none(),
        "the refresh must not register a client: {after:?}"
    );
    assert_eq!(
        after.code_verifier.as_deref(),
        Some("pending-verifier"),
        "the pending sign-in's verifier stays usable"
    );
    assert_eq!(
        after.oauth_state.as_deref(),
        Some("pending-state"),
        "the pending sign-in's state stays usable"
    );
    assert_eq!(
        after
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("stale"),
        "a refresh-only failure leaves the stored tokens for the sign-in"
    );
}
