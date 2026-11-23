//! Node `crypto` module — ported verbatim from pir's `node_fns.rs` crypto
//! block (hash/HMAC/PBKDF2/scrypt/AES-GCM/RSA sign+verify, zlib raw). The
//! op semantics and error codes are unchanged; only the crate imports differ.

use std::borrow::Cow;
use std::io::Read as _;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use serde_json::Value as JsonValue;
use sha1::Sha1;
use sha2::{Digest as _, Sha224, Sha256, Sha384, Sha512};
use sha3::digest::{ExtendableOutput, XofReader};
use sha3::{Sha3_224, Sha3_256, Sha3_384, Sha3_512, Shake128, Shake256};

use crate::pir_node::bridge::{payload_bool, payload_text};
use crate::pir_node::error::{NodeError, NodeResult};

pub fn crypto_op(op: &str, payload: &JsonValue) -> NodeResult {
    match op {
        "hash" => {
            let algo = payload_text(payload, "algo")?;
            let data = decode_base64(payload_text(payload, "data")?)?;
            let output_length = payload
                .get("outputLength")
                .and_then(JsonValue::as_u64)
                .map(|length| length as usize);
            let digest = hash_bytes(algo, &data, output_length)?;
            Ok(JsonValue::String(STANDARD.encode(digest)))
        }
        "hmac" => {
            let algo = payload_text(payload, "algo")?;
            let key = decode_base64(payload_text(payload, "key")?)?;
            let data = decode_base64(payload_text(payload, "data")?)?;
            let digest = hmac_bytes(algo, &key, &data)?;
            Ok(JsonValue::String(STANDARD.encode(digest)))
        }
        "randomBytes" => {
            let count = payload
                .get("n")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| NodeError::new("EINVAL", "missing byte count"))?;
            let buffer = random_bytes(count as usize)?;
            Ok(JsonValue::String(STANDARD.encode(buffer)))
        }
        "randomUUID" => Ok(JsonValue::String(uuid::Uuid::new_v4().to_string())),
        "pbkdf2" => {
            let algo = payload_text(payload, "algo")?;
            let password = decode_base64(payload_text(payload, "password")?)?;
            let salt = decode_base64(payload_text(payload, "salt")?)?;
            let iterations = payload
                .get("iterations")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| NodeError::new("EINVAL", "missing `iterations` payload field"))?;
            let keylen = payload
                .get("keylen")
                .and_then(JsonValue::as_u64)
                .ok_or_else(|| NodeError::new("EINVAL", "missing `keylen` payload field"))?;
            // 16 MiB cap: a derived key larger than that is a bug, not a request.
            if keylen > (1 << 24) {
                return Err(NodeError::new("EINVAL", "keylen out of range"));
            }
            if iterations > u32::MAX as u64 {
                return Err(NodeError::new("EINVAL", "iterations out of range"));
            }
            let mut out = vec![0u8; keylen as usize];
            pbkdf2_bytes(algo, &password, &salt, iterations as u32, &mut out)?;
            Ok(JsonValue::String(STANDARD.encode(out)))
        }
        // Streaming AES cipher/decipher (one host round trip per JS
        // `update()`/`final()`), state carried opaquely through the JS layer.
        "cipheriv" => crypto_cipheriv(payload),
        // Real scrypt (RFC 7914) over the `scrypt` crate.
        "scrypt" => crypto_scrypt(payload),
        // RSASSA-PKCS1-v1_5 over SHA-256/384/512 (`createSign`/`createVerify`).
        "sign" => crypto_sign(payload),
        "verify" => crypto_verify(payload),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown crypto op `{other}`"),
        )),
    }
}

/// Borrowing counterpart of [`unb64`] for the request payload fields: every
/// caller hands over a slice of the parsed payload ([`payload_text`], or the
/// `iv`/`aad`/`authTag` accessors that read it directly), so the encoded
/// argument is decoded in place and is never first copied into an owned
/// `String`.
fn decode_base64(data: &str) -> Result<Vec<u8>, NodeError> {
    STANDARD
        .decode(data.as_bytes())
        .map_err(|error| NodeError::new("EINVAL", format!("invalid base64 payload: {error}")))
}

/// Algorithm names are matched with `-` removed (`sha-256` == `sha256`). The
/// common spelling has no hyphen, so borrow it instead of collecting a copy on
/// every hash/HMAC call.
fn without_hyphens(algo: &str) -> Cow<'_, str> {
    if algo.as_bytes().contains(&b'-') {
        Cow::Owned(algo.chars().filter(|c| *c != '-').collect())
    } else {
        Cow::Borrowed(algo)
    }
}

/// Read `output_length` bytes from an extendable-output function.
fn shake_output<X: ExtendableOutput>(state: X, output_length: usize) -> Vec<u8> {
    let mut out = vec![0u8; output_length];
    state.finalize_xof().read(&mut out);
    out
}

/// Raw digest over bytes (the typed `crypto.createHash().update().digest()`
/// fast path; [`crypto_op`]'s `hash` arm is the base64 payload spelling).
///
/// `output_length` is Node's `options.outputLength` for the SHAKE XOF pair;
/// the fixed-size digests ignore it (the JS layer has already checked that it
/// matches the digest size). Node's defaults are 16 bytes for SHAKE128 and
/// 32 bytes for SHAKE256.
pub fn hash_bytes(
    algo: &str,
    data: &[u8],
    output_length: Option<usize>,
) -> Result<Vec<u8>, NodeError> {
    // Tolerate WebCrypto's hyphenated spelling ("sha-256").
    let normalized = without_hyphens(algo);
    Ok(match normalized.as_ref() {
        "sha256" => Sha256::digest(data).to_vec(),
        "sha224" => Sha224::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        "sha384" => Sha384::digest(data).to_vec(),
        "sha1" => Sha1::digest(data).to_vec(),
        "md5" => md5::Md5::digest(data).to_vec(),
        // The SHA-3 family used to run as pure JS (BigInt Keccak lanes):
        // ~3-6 s/MiB interpreted versus ~1-3 ms here.
        "sha3224" => Sha3_224::digest(data).to_vec(),
        "sha3256" => Sha3_256::digest(data).to_vec(),
        "sha3384" => Sha3_384::digest(data).to_vec(),
        "sha3512" => Sha3_512::digest(data).to_vec(),
        // Node's default XOF lengths: shake128 -> 16, shake256 -> 32.
        "shake128" => {
            let mut state = Shake128::default();
            sha3::digest::Update::update(&mut state, data);
            shake_output(state, output_length.unwrap_or(16))
        }
        "shake256" => {
            let mut state = Shake256::default();
            sha3::digest::Update::update(&mut state, data);
            shake_output(state, output_length.unwrap_or(32))
        }
        _ => {
            return Err(NodeError::new(
                "EINVAL",
                format!("unsupported hash algorithm `{algo}`"),
            ))
        }
    })
}

/// Raw HMAC over bytes (the typed `crypto.createHmac().update().digest()`
/// fast path).
pub fn hmac_bytes(algo: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>, NodeError> {
    macro_rules! digest {
        ($hash:ty) => {{
            // HMAC accepts keys of any length, so new_from_slice cannot fail
            // in practice; the error is still propagated explicitly to keep
            // production code free of panic-on-Result constructs.
            let mut mac = <Hmac<$hash> as Mac>::new_from_slice(key)
                .map_err(|_| NodeError::new("EINVAL", "HMAC key initialization failed"))?;
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }};
    }
    let normalized = without_hyphens(algo);
    let digest = match normalized.as_ref() {
        "sha256" => digest!(Sha256),
        "sha224" => digest!(Sha224),
        "sha512" => digest!(Sha512),
        "sha384" => digest!(Sha384),
        "sha1" => digest!(Sha1),
        "md5" => digest!(md5::Md5),
        "sha3224" => digest!(Sha3_224),
        "sha3256" => digest!(Sha3_256),
        "sha3384" => digest!(Sha3_384),
        "sha3512" => digest!(Sha3_512),
        _ => {
            return Err(NodeError::new(
                "EINVAL",
                format!("unsupported hmac algorithm `{algo}`"),
            ))
        }
    };
    Ok(digest)
}

// ---------------------------------------------------------------------------
// Streaming hash/HMAC state (Node's `Hash`/`Hmac` are stateful)
// ---------------------------------------------------------------------------
//
// The one-shot `hash_bytes`/`hmac_bytes` above re-read the whole payload on
// every call, so a JS `update()` loop has to concatenate its chunks before the
// single digest call. These types keep the RustCrypto state alive between
// calls instead: the shim hands each chunk over as it arrives, and the host
// returns a small integer handle that the per-runtime registry in
// `bytes_ops` maps back to the state. That is the structural match for Node's
// Hash, which also updates its state per `update()`.

/// Fixed-size digest states plus the two SHAKE XOF states (which have no
/// `finalize()` output size of their own: the length arrives at digest time
/// from Node's `options.outputLength`).
#[derive(Clone)]
pub enum HashState {
    Md5(md5::Md5),
    Sha1(Sha1),
    Sha224(Sha224),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
    Sha3_224(Sha3_224),
    Sha3_256(Sha3_256),
    Sha3_384(Sha3_384),
    Sha3_512(Sha3_512),
    Shake128(Shake128),
    Shake256(Shake256),
}

impl HashState {
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Md5(state) => state.update(data),
            Self::Sha1(state) => state.update(data),
            Self::Sha224(state) => state.update(data),
            Self::Sha256(state) => state.update(data),
            Self::Sha384(state) => state.update(data),
            Self::Sha512(state) => state.update(data),
            Self::Sha3_224(state) => state.update(data),
            Self::Sha3_256(state) => state.update(data),
            Self::Sha3_384(state) => state.update(data),
            Self::Sha3_512(state) => state.update(data),
            Self::Shake128(state) => sha3::digest::Update::update(state, data),
            Self::Shake256(state) => sha3::digest::Update::update(state, data),
        }
    }

    /// Finalize the state. `output_length` is only meaningful for the SHAKE
    /// pair (Node's default lengths apply when it is absent); every fixed-size
    /// digest reports its own length.
    pub fn finish(self, output_length: Option<usize>) -> Vec<u8> {
        match self {
            Self::Md5(state) => state.finalize().to_vec(),
            Self::Sha1(state) => state.finalize().to_vec(),
            Self::Sha224(state) => state.finalize().to_vec(),
            Self::Sha256(state) => state.finalize().to_vec(),
            Self::Sha384(state) => state.finalize().to_vec(),
            Self::Sha512(state) => state.finalize().to_vec(),
            Self::Sha3_224(state) => state.finalize().to_vec(),
            Self::Sha3_256(state) => state.finalize().to_vec(),
            Self::Sha3_384(state) => state.finalize().to_vec(),
            Self::Sha3_512(state) => state.finalize().to_vec(),
            Self::Shake128(state) => shake_output(state, output_length.unwrap_or(16)),
            Self::Shake256(state) => shake_output(state, output_length.unwrap_or(32)),
        }
    }
}

impl HashState {
    /// `None` for an algorithm this host does not serve; the shim keeps its
    /// own JS implementation for that case.
    pub fn new(algo: &str) -> Option<Self> {
        let normalized = without_hyphens(algo);
        Some(match normalized.as_ref() {
            "md5" => Self::Md5(md5::Md5::new()),
            "sha1" => Self::Sha1(Sha1::new()),
            "sha224" => Self::Sha224(Sha224::new()),
            "sha256" => Self::Sha256(Sha256::new()),
            "sha384" => Self::Sha384(Sha384::new()),
            "sha512" => Self::Sha512(Sha512::new()),
            "sha3224" => Self::Sha3_224(Sha3_224::new()),
            "sha3256" => Self::Sha3_256(Sha3_256::new()),
            "sha3384" => Self::Sha3_384(Sha3_384::new()),
            "sha3512" => Self::Sha3_512(Sha3_512::new()),
            "shake128" => Self::Shake128(Shake128::default()),
            "shake256" => Self::Shake256(Shake256::default()),
            _ => return None,
        })
    }
}

macro_rules! hmac_state_enum {
    ($($variant:ident => $ty:ty),+ $(,)?) => {
        #[derive(Clone)]
        pub enum HmacState {
            $($variant(Hmac<$ty>)),+
        }

        impl HmacState {
            pub fn update(&mut self, data: &[u8]) {
                match self {
                    $(Self::$variant(state) => state.update(data)),+
                }
            }

            pub fn finish(self) -> Vec<u8> {
                match self {
                    $(Self::$variant(state) => state.finalize().into_bytes().to_vec()),+
                }
            }
        }
    };
}

hmac_state_enum!(
    Md5 => md5::Md5,
    Sha1 => Sha1,
    Sha224 => Sha224,
    Sha256 => Sha256,
    Sha384 => Sha384,
    Sha512 => Sha512,
    Sha3_224 => Sha3_224,
    Sha3_256 => Sha3_256,
    Sha3_384 => Sha3_384,
    Sha3_512 => Sha3_512,
);

impl HmacState {
    /// HMAC accepts keys of any length, so the only `None` is an algorithm
    /// this host does not serve.
    pub fn new(algo: &str, key: &[u8]) -> Option<Self> {
        let normalized = without_hyphens(algo);
        // The key length is unconstrained, so `new_from_slice` cannot fail in
        // practice; `.ok()` keeps the unsupported-algorithm path `None`.
        Some(match normalized.as_ref() {
            "md5" => Self::Md5(<Hmac<md5::Md5> as Mac>::new_from_slice(key).ok()?),
            "sha1" => Self::Sha1(<Hmac<Sha1> as Mac>::new_from_slice(key).ok()?),
            "sha224" => Self::Sha224(<Hmac<Sha224> as Mac>::new_from_slice(key).ok()?),
            "sha256" => Self::Sha256(<Hmac<Sha256> as Mac>::new_from_slice(key).ok()?),
            "sha384" => Self::Sha384(<Hmac<Sha384> as Mac>::new_from_slice(key).ok()?),
            "sha512" => Self::Sha512(<Hmac<Sha512> as Mac>::new_from_slice(key).ok()?),
            "sha3224" => Self::Sha3_224(<Hmac<Sha3_224> as Mac>::new_from_slice(key).ok()?),
            "sha3256" => Self::Sha3_256(<Hmac<Sha3_256> as Mac>::new_from_slice(key).ok()?),
            "sha3384" => Self::Sha3_384(<Hmac<Sha3_384> as Mac>::new_from_slice(key).ok()?),
            "sha3512" => Self::Sha3_512(<Hmac<Sha3_512> as Mac>::new_from_slice(key).ok()?),
            _ => return None,
        })
    }
}

/// Lowercase hex for `digest("hex")`, built host-side: the JS `Buffer`
/// `toString("hex")` loop was ~3.5 µs for a 32-byte digest.
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 0x0f) as usize]);
    }
    // Every byte written above is ASCII.
    String::from_utf8(out).unwrap_or_else(|e| panic!("hex digits are not ASCII: {e}"))
}

/// Node's `digest("base64")` spelling (standard alphabet, padded).
pub fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Node's `digest("base64url")` spelling (URL alphabet, no padding).
pub fn encode_base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh RFC 4122 v4 UUID in Node's hyphenated lowercase form.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}


/// `n` bytes from the OS entropy source (1 GiB cap: an extension asking for
/// more is a bug, not a request).
pub fn random_bytes(count: usize) -> Result<Vec<u8>, NodeError> {
    if count > (1 << 30) {
        return Err(NodeError::new("EINVAL", "byte count out of range"));
    }
    let mut buffer = vec![0u8; count];
    getrandom::fill(&mut buffer)
        .map_err(|error| NodeError::new("EIO", format!("entropy source failed: {error}")))?;
    Ok(buffer)
}

/// PBKDF2 fill over the same digest set as `hmac_bytes` (RFC 2898 with
/// HMAC as the PRF; `pbkdf2` crate, no new primitive backends).
fn pbkdf2_bytes(
    algo: &str,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), NodeError> {
    let normalized = without_hyphens(algo);
    match normalized.as_ref() {
        "sha256" => pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, out),
        "sha224" => pbkdf2::pbkdf2_hmac::<Sha224>(password, salt, iterations, out),
        "sha512" => pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iterations, out),
        "sha384" => pbkdf2::pbkdf2_hmac::<Sha384>(password, salt, iterations, out),
        "sha1" => pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, iterations, out),
        "sha3224" => pbkdf2::pbkdf2_hmac::<Sha3_224>(password, salt, iterations, out),
        "sha3256" => pbkdf2::pbkdf2_hmac::<Sha3_256>(password, salt, iterations, out),
        "sha3384" => pbkdf2::pbkdf2_hmac::<Sha3_384>(password, salt, iterations, out),
        "sha3512" => pbkdf2::pbkdf2_hmac::<Sha3_512>(password, salt, iterations, out),
        _ => {
            return Err(NodeError::new(
                "EINVAL",
                format!("unsupported pbkdf2 digest `{algo}`"),
            ))
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// zlib raw (deflateRaw / inflateRaw)
// ---------------------------------------------------------------------------

/// Raw-DEFLATE pair (RFC 1951, no zlib/gzip wrapper), matching Node's
/// `zlib.deflateRawSync`/`inflateRawSync` observable output; the same flate2
/// engine already backs the gzip/deflate ops on the legacy `nodeExt` table.
/// Routed here by `__pirNodeCall`'s op whitelist (see
/// js/node_call_helper.js.txt) because the legacy zlib table lacks the pair.
/// Output bound of `inflateRawSync` (a raw-deflate bomb must not allocate on
/// the host heap outside QuickJS's memory limit). Same 32 MiB budget as the
/// wrapped decompressors in `child_fns`.
const MAX_INFLATE_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;

pub fn zlib_raw_op(op: &str, payload: &JsonValue) -> NodeResult {
    // Match the op *before* touching the payload: the dispatcher probes this
    // function for every `zlib` op and falls through to `child::zlib_op` on
    // ENOSYS. Decoding first made every wrapped gzip/deflate/zstd call pay a
    // full base64 pass (measured 678µs/MiB) only to be thrown away.
    if !matches!(op, "deflateRawSync" | "inflateRawSync") {
        return Err(NodeError::new("ENOSYS", format!("unknown zlib op `{op}`")));
    }
    let data = decode_base64(payload_text(payload, "data")?)?;
    let level = flate2::Compression::default();
    let out = match op {
        "deflateRawSync" => {
            let mut out = Vec::new();
            flate2::read::DeflateEncoder::new(&data[..], level)
                .read_to_end(&mut out)
                .map_err(|error| NodeError::new("EIO", format!("compression failed: {error}")))?;
            out
        }
        "inflateRawSync" => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(&data[..])
                .take(MAX_INFLATE_OUTPUT_BYTES + 1)
                .read_to_end(&mut out)
                .map_err(|error| {
                    // Node's decompression failures carry `Z_DATA_ERROR`.
                    NodeError::new(
                        "Z_DATA_ERROR",
                        format!("Z_DATA_ERROR: decompression failed: {error}"),
                    )
                })?;
            if out.len() as u64 > MAX_INFLATE_OUTPUT_BYTES {
                return Err(NodeError::new(
                    "Z_BUF_ERROR",
                    format!(
                        "Z_BUF_ERROR: decompressed output exceeds the {}-MiB limit",
                        MAX_INFLATE_OUTPUT_BYTES / (1024 * 1024)
                    ),
                ));
            }
            out
        }
        other => {
            return Err(NodeError::new(
                "ENOSYS",
                format!("unknown zlib op `{other}`"),
            ))
        }
    };
    Ok(JsonValue::String(STANDARD.encode(out)))
}

// ---------------------------------------------------------------------------
// crypto: AES cipheriv (CBC / CTR / GCM) + scrypt + RSA PKCS#1 v1.5 sign/verify
// ---------------------------------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode one state field; a malformed state is a programming error on the
/// JS side, surfaced as EINVAL rather than a panic.
fn unb64(text: &str) -> Result<Vec<u8>, NodeError> {
    STANDARD
        .decode(text.as_bytes())
        .map_err(|error| NodeError::new("EINVAL", format!("invalid cipher state: {error}")))
}

fn invalid_iv() -> NodeError {
    NodeError::new("ERR_CRYPTO_INVALID_IV", "Invalid initialization vector")
}

fn wrong_final_block() -> NodeError {
    NodeError::new(
        "ERR_OSSL_EVP_WRONG_FINAL_BLOCK_LENGTH",
        "error:1C80006B:wrong final block length",
    )
}

fn bad_decrypt() -> NodeError {
    NodeError::new("ERR_OSSL_EVP_BAD_DECRYPT", "error:1C800064:bad decrypt")
}

/// One AES key schedule over the three standard key lengths; CBC/CTR/GCM
/// composition lives below so the per-update streaming state stays explicit
/// and serializable across the host boundary.
enum AesKey {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl AesKey {
    fn new(key: &[u8]) -> Result<Self, NodeError> {
        Ok(match key.len() {
            16 => Self::A128(aes::Aes128::new(GenericArray::from_slice(key))),
            24 => Self::A192(aes::Aes192::new(GenericArray::from_slice(key))),
            32 => Self::A256(aes::Aes256::new(GenericArray::from_slice(key))),
            _ => {
                return Err(NodeError::new(
                    "ERR_CRYPTO_INVALID_KEYLEN",
                    "Invalid key length",
                ))
            }
        })
    }

    fn encrypt_block(&self, block: &mut [u8; 16]) {
        let mut buffer = GenericArray::clone_from_slice(block);
        match self {
            Self::A128(cipher) => cipher.encrypt_block(&mut buffer),
            Self::A192(cipher) => cipher.encrypt_block(&mut buffer),
            Self::A256(cipher) => cipher.encrypt_block(&mut buffer),
        }
        block.copy_from_slice(&buffer);
    }

    fn decrypt_block(&self, block: &mut [u8; 16]) {
        let mut buffer = GenericArray::clone_from_slice(block);
        match self {
            Self::A128(cipher) => cipher.decrypt_block(&mut buffer),
            Self::A192(cipher) => cipher.decrypt_block(&mut buffer),
            Self::A256(cipher) => cipher.decrypt_block(&mut buffer),
        }
        block.copy_from_slice(&buffer);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CipherMode {
    Cbc,
    Ctr,
    Gcm,
}

struct CipherSpec {
    mode: CipherMode,
    key_len: usize,
}

fn unknown_cipher(alg: &str) -> NodeError {
    NodeError::new(
        "ERR_CRYPTO_UNKNOWN_CIPHER",
        format!("Unknown cipher `{alg}`"),
    )
}

/// Recognized algorithms: `aes-128|192|256` x `cbc|ctr|gcm` — the exact set
/// the JS shim advertises through `getCiphers`.
fn parse_cipher_alg(alg: &str) -> Result<CipherSpec, NodeError> {
    // Common case: the shim already passes the lowercase spelling, so skip the
    // copy (which exists only to fold an uppercase name to lowercase).
    let lowered = if alg.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(alg.to_ascii_lowercase())
    } else {
        Cow::Borrowed(alg)
    };
    let core = lowered
        .strip_prefix("aes-")
        .ok_or_else(|| unknown_cipher(alg))?;
    let (bits, mode) = core.split_once('-').ok_or_else(|| unknown_cipher(alg))?;
    let key_len = match bits {
        "128" => 16,
        "192" => 24,
        "256" => 32,
        _ => return Err(unknown_cipher(alg)),
    };
    let mode = match mode {
        "cbc" => CipherMode::Cbc,
        "ctr" => CipherMode::Ctr,
        "gcm" => CipherMode::Gcm,
        _ => return Err(unknown_cipher(alg)),
    };
    Ok(CipherSpec { mode, key_len })
}

fn vec_to_block(bytes: &[u8]) -> Result<[u8; 16], NodeError> {
    if bytes.len() != 16 {
        return Err(NodeError::new(
            "EINVAL",
            "invalid cipher state block length",
        ));
    }
    let mut block = [0u8; 16];
    block.copy_from_slice(bytes);
    Ok(block)
}

fn xor_block(target: &mut [u8; 16], source: &[u8; 16]) {
    for index in 0..16 {
        target[index] ^= source[index];
    }
}

/// GCM's 32-bit big-endian counter increment (the low four bytes).
fn inc32(counter: &mut [u8; 16]) {
    for index in (12..16).rev() {
        counter[index] = counter[index].wrapping_add(1);
        if counter[index] != 0 {
            break;
        }
    }
}

/// GF(2^128) multiplication in the GCM (bit-reflected) convention:
/// `x <- x * y`.
fn ghash_mul(x: &mut [u8; 16], y: &[u8; 16]) {
    let mut z = [0u8; 16];
    let mut v = *y;
    for bit in 0..128 {
        if (x[bit / 8] >> (7 - (bit % 8))) & 1 == 1 {
            xor_block(&mut z, &v);
        }
        let lsb = v[15] & 1;
        let mut carry = 0u8;
        for byte in &mut v {
            let next = *byte & 1;
            *byte = (*byte >> 1) | (carry << 7);
            carry = next;
        }
        if lsb == 1 {
            v[0] ^= 0xe1;
        }
    }
    *x = z;
}

/// The GCM pre-counter block J0 (NIST SP 800-38D 7.1): `IV || 0^31 || 1` for a
/// 96-bit IV, GHASH of the padded IV plus its bit length otherwise.
fn gcm_j0(h: &[u8; 16], iv: &[u8]) -> [u8; 16] {
    if iv.len() == 12 {
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        return j0;
    }
    let mut acc = [0u8; 16];
    for chunk in iv.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        xor_block(&mut acc, &block);
        ghash_mul(&mut acc, h);
    }
    let mut len_block = [0u8; 16];
    len_block[8..].copy_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
    xor_block(&mut acc, &len_block);
    ghash_mul(&mut acc, h);
    acc
}

fn cipher_value(
    data: Vec<u8>,
    state: Option<impl serde::Serialize>,
    auth_tag: Option<Vec<u8>>,
) -> JsonValue {
    let state = match state {
        Some(st) => JsonValue::String(serde_json::to_string(&st).unwrap_or_default()),
        None => JsonValue::Null,
    };
    let auth_tag = match auth_tag {
        Some(tag) => JsonValue::String(b64(&tag)),
        None => JsonValue::Null,
    };
    serde_json::json!({ "data": b64(&data), "state": state, "authTag": auth_tag })
}

/// Payload shared by every `cipheriv` call: the algorithm triple, direction,
/// key/iv, this update's bytes and the opaque continuation state.
struct CipherCall<'a> {
    cipher: AesKey,
    encrypt: bool,
    iv: Option<Vec<u8>>,
    data: Vec<u8>,
    is_final: bool,
    auto_padding: bool,
    /// Borrowed out of the payload: the state text is only ever deserialized,
    /// so it is not copied into an owned `String` per `update()`.
    state: Option<&'a str>,
    aad: Option<Vec<u8>>,
    auth_tag: Option<Vec<u8>>,
}

fn crypto_cipheriv(payload: &JsonValue) -> NodeResult {
    let alg = payload_text(payload, "alg")?;
    let spec = parse_cipher_alg(alg)?;
    let key = decode_base64(payload_text(payload, "key")?)?;
    if key.len() != spec.key_len {
        return Err(NodeError::new(
            "ERR_CRYPTO_INVALID_KEYLEN",
            "Invalid key length",
        ));
    }
    let call = CipherCall {
        cipher: AesKey::new(&key)?,
        encrypt: payload.get("dir").and_then(JsonValue::as_str) != Some("decrypt"),
        iv: match payload.get("iv").and_then(JsonValue::as_str) {
            Some(text) => Some(decode_base64(text)?),
            None => None,
        },
        data: decode_base64(payload_text(payload, "data")?)?,
        is_final: payload_bool(payload, "final"),
        auto_padding: payload
            .get("autoPadding")
            .and_then(JsonValue::as_bool)
            .unwrap_or(true),
        state: payload
            .get("state")
            .and_then(JsonValue::as_str)
            .filter(|text| !text.is_empty()),
        aad: match payload.get("aad").and_then(JsonValue::as_str) {
            Some(text) => Some(decode_base64(text)?),
            None => None,
        },
        auth_tag: match payload.get("authTag").and_then(JsonValue::as_str) {
            Some(text) => Some(decode_base64(text)?),
            None => None,
        },
    };
    match spec.mode {
        CipherMode::Cbc => cbc_step(&call),
        CipherMode::Ctr => ctr_step(&call),
        CipherMode::Gcm => gcm_step(&call),
    }
}

/// CBC continuation state: previous ciphertext block, the undecided byte
/// tail and (decrypt only) the one plaintext block held back until the
/// padding verdict at `final()`.
#[derive(serde::Serialize, serde::Deserialize)]
struct CbcState {
    prev: String,
    pending: String,
    held: String,
}

fn cbc_step(call: &CipherCall<'_>) -> NodeResult {
    let mut st: CbcState = match call.state {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| NodeError::new("EINVAL", format!("invalid cipher state: {error}")))?,
        None => {
            let iv = call.iv.as_deref().ok_or_else(invalid_iv)?;
            if iv.len() != 16 {
                return Err(invalid_iv());
            }
            CbcState {
                prev: b64(iv),
                pending: String::new(),
                held: String::new(),
            }
        }
    };
    let mut prev = vec_to_block(&unb64(&st.prev)?)?;
    let mut pending = unb64(&st.pending)?;
    let mut held = unb64(&st.held)?;
    let mut out: Vec<u8> = Vec::with_capacity(call.data.len());
    pending.extend_from_slice(&call.data);
    // Whole blocks are consumed with a cursor: `Vec::split_off` per block
    // re-allocates and copies the whole remaining tail on every round, which
    // is quadratic in the payload size. The single `drain` below leaves
    // `pending` holding exactly the tail the old loop left behind.
    let mut offset = 0usize;
    while pending.len() - offset >= 16 {
        let mut block = [0u8; 16];
        block.copy_from_slice(&pending[offset..offset + 16]);
        offset += 16;
        if call.encrypt {
            xor_block(&mut block, &prev);
            call.cipher.encrypt_block(&mut block);
            prev = block;
            out.extend_from_slice(&block);
        } else {
            let ciphertext = block;
            call.cipher.decrypt_block(&mut block);
            xor_block(&mut block, &prev);
            prev = ciphertext;
            if held.len() == 16 {
                out.extend_from_slice(&held);
            }
            // Reuse the 16-byte slot instead of a fresh `to_vec` per block.
            held.clear();
            held.extend_from_slice(&block);
        }
    }
    pending.drain(..offset);
    if call.is_final {
        if call.encrypt {
            if call.auto_padding {
                // PKCS#7: an empty tail pads a full block (OpenSSL parity).
                let pad = (16 - pending.len() % 16) as u8;
                for _ in 0..pad {
                    pending.push(pad);
                }
                while pending.len() >= 16 {
                    let mut block = [0u8; 16];
                    block.copy_from_slice(&pending[..16]);
                    pending = pending.split_off(16);
                    xor_block(&mut block, &prev);
                    call.cipher.encrypt_block(&mut block);
                    prev = block;
                    out.extend_from_slice(&block);
                }
            } else if !pending.is_empty() {
                return Err(wrong_final_block());
            }
        } else {
            if !pending.is_empty() {
                return Err(wrong_final_block());
            }
            if call.auto_padding {
                if held.len() == 16 {
                    let pad = held[15] as usize;
                    let valid = (1..=16).contains(&pad)
                        && held[16 - pad..].iter().all(|b| *b as usize == pad);
                    if !valid {
                        return Err(bad_decrypt());
                    }
                    out.extend_from_slice(&held[..16 - pad]);
                }
            } else {
                out.extend_from_slice(&held);
            }
        }
        return Ok(cipher_value(out, Option::<CbcState>::None, None));
    }
    st.prev = b64(&prev);
    st.pending = b64(&pending);
    st.held = b64(&held);
    Ok(cipher_value(out, Some(st), None))
}

/// CTR continuation state: the current counter block, its keystream block and
/// the offset already consumed from it.
#[derive(serde::Serialize, serde::Deserialize)]
struct CtrState {
    counter: String,
    ks: String,
    off: u8,
}

fn ctr_step(call: &CipherCall<'_>) -> NodeResult {
    let (mut counter, mut ks, mut off) = match call.state {
        Some(text) => {
            let st: CtrState = serde_json::from_str(text).map_err(|error| {
                NodeError::new("EINVAL", format!("invalid cipher state: {error}"))
            })?;
            (
                vec_to_block(&unb64(&st.counter)?)?,
                vec_to_block(&unb64(&st.ks)?)?,
                st.off as usize,
            )
        }
        None => {
            let iv = call.iv.as_deref().ok_or_else(invalid_iv)?;
            if iv.len() != 16 {
                return Err(invalid_iv());
            }
            let mut counter = [0u8; 16];
            counter.copy_from_slice(iv);
            let mut ks = counter;
            call.cipher.encrypt_block(&mut ks);
            (counter, ks, 0)
        }
    };
    let mut out = Vec::with_capacity(call.data.len());
    for &byte in &call.data {
        out.push(byte ^ ks[off]);
        off += 1;
        if off == 16 {
            inc32(&mut counter);
            ks = counter;
            call.cipher.encrypt_block(&mut ks);
            off = 0;
        }
    }
    if call.is_final {
        return Ok(cipher_value(out, Option::<CtrState>::None, None));
    }
    let st = CtrState {
        counter: b64(&counter),
        ks: b64(&ks),
        off: off as u8,
    };
    Ok(cipher_value(out, Some(st), None))
}

/// GCM continuation state: the hash subkey, J0, the running counter and
/// keystream position, the GHASH accumulator plus the partial ciphertext
/// block, and the AAD/ciphertext byte lengths feeding the final length block.
#[derive(serde::Serialize, serde::Deserialize)]
struct GcmState {
    h: String,
    j0: String,
    ctr: String,
    ks: String,
    off: u8,
    ghash: String,
    pending: String,
    la: u64,
    lc: u64,
    aad_absorbed: bool,
}

fn gcm_step(call: &CipherCall<'_>) -> NodeResult {
    let mut st: GcmState = match call.state {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| NodeError::new("EINVAL", format!("invalid cipher state: {error}")))?,
        None => {
            let iv = call.iv.as_deref().ok_or_else(invalid_iv)?;
            if iv.is_empty() {
                return Err(invalid_iv());
            }
            let mut h = [0u8; 16];
            call.cipher.encrypt_block(&mut h);
            let j0 = gcm_j0(&h, iv);
            let mut ctr = j0;
            inc32(&mut ctr);
            let mut ks = ctr;
            call.cipher.encrypt_block(&mut ks);
            GcmState {
                h: b64(&h),
                j0: b64(&j0),
                ctr: b64(&ctr),
                ks: b64(&ks),
                off: 0,
                ghash: b64(&[0u8; 16]),
                pending: String::new(),
                la: 0,
                lc: 0,
                aad_absorbed: false,
            }
        }
    };
    let h = vec_to_block(&unb64(&st.h)?)?;
    let mut ghash = vec_to_block(&unb64(&st.ghash)?)?;
    let mut pending = unb64(&st.pending)?;
    let mut ks = vec_to_block(&unb64(&st.ks)?)?;
    let mut ctr = vec_to_block(&unb64(&st.ctr)?)?;
    let mut off = st.off as usize;
    if let Some(aad) = &call.aad {
        if !st.aad_absorbed {
            st.aad_absorbed = true;
            st.la = aad.len() as u64;
            for chunk in aad.chunks(16) {
                let mut block = [0u8; 16];
                block[..chunk.len()].copy_from_slice(chunk);
                xor_block(&mut ghash, &block);
                ghash_mul(&mut ghash, &h);
            }
        }
    }
    let mut out = Vec::with_capacity(call.data.len());
    for &byte in &call.data {
        let stream = ks[off];
        if call.encrypt {
            let cipher_byte = byte ^ stream;
            out.push(cipher_byte);
            pending.push(cipher_byte);
        } else {
            pending.push(byte);
            out.push(byte ^ stream);
        }
        off += 1;
        if off == 16 {
            inc32(&mut ctr);
            ks = ctr;
            call.cipher.encrypt_block(&mut ks);
            off = 0;
        }
        if pending.len() == 16 {
            let mut block = [0u8; 16];
            block.copy_from_slice(&pending);
            pending.clear();
            xor_block(&mut ghash, &block);
            ghash_mul(&mut ghash, &h);
        }
    }
    st.lc += call.data.len() as u64;
    let mut auth_out: Option<Vec<u8>> = None;
    if call.is_final {
        if !pending.is_empty() {
            let mut block = [0u8; 16];
            block[..pending.len()].copy_from_slice(&pending);
            xor_block(&mut ghash, &block);
            ghash_mul(&mut ghash, &h);
        }
        let mut len_block = [0u8; 16];
        len_block[..8].copy_from_slice(&(st.la * 8).to_be_bytes());
        len_block[8..].copy_from_slice(&(st.lc * 8).to_be_bytes());
        xor_block(&mut ghash, &len_block);
        ghash_mul(&mut ghash, &h);
        let j0 = vec_to_block(&unb64(&st.j0)?)?;
        let mut tag = j0;
        call.cipher.encrypt_block(&mut tag);
        for index in 0..16 {
            tag[index] ^= ghash[index];
        }
        if call.encrypt {
            auth_out = Some(tag.to_vec());
        } else if call.auth_tag.as_deref() != Some(tag.as_slice()) {
            return Err(bad_decrypt());
        }
        return Ok(cipher_value(out, Option::<GcmState>::None, auth_out));
    }
    st.ghash = b64(&ghash);
    st.pending = b64(&pending);
    st.ks = b64(&ks);
    st.ctr = b64(&ctr);
    st.off = off as u8;
    Ok(cipher_value(out, Some(st), auth_out))
}

/// Real scrypt (RFC 7914) over the `scrypt` crate; `maxmem` mirrors Node's
/// guard (default 32 MiB, 128*N*r bytes as the estimate).
fn crypto_scrypt(payload: &JsonValue) -> NodeResult {
    let password = decode_base64(payload_text(payload, "password")?)?;
    let salt = decode_base64(payload_text(payload, "salt")?)?;
    let log_n = payload
        .get("logN")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", "missing `logN` payload field"))?;
    let r = payload
        .get("r")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", "missing `r` payload field"))?;
    let p = payload
        .get("p")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", "missing `p` payload field"))?;
    let keylen = payload
        .get("keylen")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", "missing `keylen` payload field"))?;
    let maxmem = payload
        .get("maxmem")
        .and_then(JsonValue::as_u64)
        .unwrap_or(32 * 1024 * 1024);
    if keylen > (1 << 24) {
        return Err(NodeError::new("EINVAL", "keylen out of range"));
    }
    if !(1..=63).contains(&log_n) || r == 0 || p == 0 {
        return Err(NodeError::new(
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            "Invalid scrypt params",
        ));
    }
    let memory = 128u128 * (r as u128) * (1u128 << log_n);
    if memory > u128::from(maxmem) {
        return Err(NodeError::new(
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            "Invalid scrypt params",
        ));
    }
    // `scrypt::Params::new` rejects output lengths below 10, while OpenSSL
    // (and therefore Node) allows any keylen, including 0. Derive the shortest
    // supported length and truncate: scrypt's output is a PBKDF2 stream, so a
    // shorter key is exactly the prefix.
    const SCRYPT_MIN_OUTPUT: usize = 10;
    let requested_len = keylen as usize;
    let derive_len = requested_len.max(SCRYPT_MIN_OUTPUT);
    let params =
        scrypt::Params::new(log_n as u8, r as u32, p as u32, derive_len).map_err(|error| {
            NodeError::new(
                "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
                format!("Invalid scrypt params: {error}"),
            )
        })?;
    let mut out = vec![0u8; derive_len];
    scrypt::scrypt(&password, &salt, &params, &mut out).map_err(|error| {
        NodeError::new(
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            format!("Invalid scrypt params: {error}"),
        )
    })?;
    out.truncate(requested_len);
    Ok(JsonValue::String(STANDARD.encode(out)))
}

#[derive(Clone, Copy)]
enum SignDigest {
    Sha256,
    Sha384,
    Sha512,
}

/// Accepts Node's spellings: "RSA-SHA256", "sha256", "SHA-256", "RSA-SHA384".
fn parse_sign_digest(alg: &str) -> Result<SignDigest, NodeError> {
    // The filter keeps ASCII alphanumerics only, so the case fold is a plain
    // ASCII map and can run per character before the string is collected.
    // Folding after the collect (as before) built a second `String` for it.
    // An already-lowercase alphanumeric name needs neither step: borrow it.
    let normalized = if alg
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Cow::Borrowed(alg)
    } else {
        Cow::Owned(
            alg.chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c: char| c.to_ascii_lowercase())
                .collect(),
        )
    };
    let digest = normalized.strip_prefix("rsa").unwrap_or(normalized.as_ref());
    Ok(match digest {
        "sha256" | "sha2256" => SignDigest::Sha256,
        "sha384" | "sha2384" => SignDigest::Sha384,
        "sha512" | "sha2512" => SignDigest::Sha512,
        _ => {
            return Err(NodeError::new(
                "EINVAL",
                format!("unsupported sign/verify digest `{alg}`"),
            ))
        }
    })
}

fn sign_digest_bytes(digest: SignDigest, data: &[u8]) -> Vec<u8> {
    use rsa::sha2::{Digest as _, Sha256 as RsaSha256, Sha384 as RsaSha384, Sha512 as RsaSha512};
    match digest {
        SignDigest::Sha256 => RsaSha256::digest(data).to_vec(),
        SignDigest::Sha384 => RsaSha384::digest(data).to_vec(),
        SignDigest::Sha512 => RsaSha512::digest(data).to_vec(),
    }
}

fn rsa_private_from_pem(pem: &str) -> Result<rsa::RsaPrivateKey, NodeError> {
    match rsa::RsaPrivateKey::from_pkcs8_pem(pem) {
        Ok(key) => Ok(key),
        Err(_) => rsa::RsaPrivateKey::from_pkcs1_pem(pem).map_err(|error| {
            NodeError::new(
                "ERR_CRYPTO_INVALID_KEY",
                format!("invalid private key: {error}"),
            )
        }),
    }
}

fn rsa_public_from_pem(pem: &str) -> Result<rsa::RsaPublicKey, NodeError> {
    match rsa::RsaPublicKey::from_public_key_pem(pem) {
        Ok(key) => Ok(key),
        Err(_) => rsa::RsaPublicKey::from_pkcs1_pem(pem).map_err(|error| {
            NodeError::new(
                "ERR_CRYPTO_INVALID_KEY",
                format!("invalid public key: {error}"),
            )
        }),
    }
}

/// RSASSA-PKCS1-v1_5 signature over the SHA-2 family (`createSign().sign`).
fn crypto_sign(payload: &JsonValue) -> NodeResult {
    use rsa::sha2::{Sha256 as RsaSha256, Sha384 as RsaSha384, Sha512 as RsaSha512};
    let digest = parse_sign_digest(payload_text(payload, "algo")?)?;
    let key = rsa_private_from_pem(payload_text(payload, "key")?)?;
    let data = decode_base64(payload_text(payload, "data")?)?;
    let hashed = sign_digest_bytes(digest, &data);
    let signature = match digest {
        SignDigest::Sha256 => key.sign(rsa::Pkcs1v15Sign::new::<RsaSha256>(), &hashed),
        SignDigest::Sha384 => key.sign(rsa::Pkcs1v15Sign::new::<RsaSha384>(), &hashed),
        SignDigest::Sha512 => key.sign(rsa::Pkcs1v15Sign::new::<RsaSha512>(), &hashed),
    }
    .map_err(|error| {
        NodeError::new("ERR_CRYPTO_SIGN_FAILED", format!("signing failed: {error}"))
    })?;
    Ok(JsonValue::String(STANDARD.encode(signature)))
}

/// Verification counterpart; a wrong signature is `false`, never an error
/// (Node's `verify()` contract), while an unparsable key still throws.
fn crypto_verify(payload: &JsonValue) -> NodeResult {
    use rsa::sha2::{Sha256 as RsaSha256, Sha384 as RsaSha384, Sha512 as RsaSha512};
    let digest = parse_sign_digest(payload_text(payload, "algo")?)?;
    let key = rsa_public_from_pem(payload_text(payload, "key")?)?;
    let data = decode_base64(payload_text(payload, "data")?)?;
    let signature = decode_base64(payload_text(payload, "signature")?)?;
    let hashed = sign_digest_bytes(digest, &data);
    let verdict = match digest {
        SignDigest::Sha256 => {
            key.verify(rsa::Pkcs1v15Sign::new::<RsaSha256>(), &hashed, &signature)
        }
        SignDigest::Sha384 => {
            key.verify(rsa::Pkcs1v15Sign::new::<RsaSha384>(), &hashed, &signature)
        }
        SignDigest::Sha512 => {
            key.verify(rsa::Pkcs1v15Sign::new::<RsaSha512>(), &hashed, &signature)
        }
    };
    Ok(JsonValue::Bool(verdict.is_ok()))
}
