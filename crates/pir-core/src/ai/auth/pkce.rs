//! PKCE (RFC 7636) utilities: code verifier/challenge generation.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/pkce.ts` — the
//! verifier is 32 random bytes base64url-encoded and the challenge is the
//! SHA-256 digest of the verifier, base64url-encoded (S256).

use sha2::{Digest, Sha256};

use crate::ai::error::{AiError, AiResult};

/// A PKCE verifier/challenge pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Base64url-encode without padding (RFC 4648 §5, as Web Crypto's
/// `btoa(...).replace(...)` pipeline produces).
pub fn base64url_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Base64url-decode, tolerating both padded and unpadded input.
pub fn base64url_decode(value: &str) -> AiResult<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    // Strip padding so both padded and unpadded forms decode.
    let trimmed = value.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .map_err(|error| AiError::Authentication(format!("invalid base64url data: {error}")))
}

/// 32 random bytes. Sourced from two UUIDv4 values (the only RNG dependency
/// pir carries); each contributes ~122 bits of entropy for 244 total, well
/// above the RFC 7636 minimum for an S256 verifier.
fn random_bytes() -> [u8; 32] {
    let first = *uuid::Uuid::new_v4().as_bytes();
    let second = *uuid::Uuid::new_v4().as_bytes();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&first);
    bytes[16..].copy_from_slice(&second);
    bytes
}

/// Compute the S256 challenge for a verifier (public for test vectors).
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_encode(&digest)
}

/// Generate a PKCE verifier/challenge pair (upstream `generatePKCE`).
pub fn generate_pkce() -> Pkce {
    let verifier = base64url_encode(&random_bytes());
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}
