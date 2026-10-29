//! Host-side backends behind the single JS entry point
//! `__pir_host.node(module, op, payloadJson) -> String`: the filesystem, os,
//! crypto and process surfaces the phase-2 node shims (`@node/fs`,
//! `@node/os`, `@node/crypto`, the `process` bootstrap) call synchronously.
//!
//! Every call is one JSON round trip: success is `{"ok":true,"value":...}`,
//! failure is `{"ok":false,"code":"ENOENT","message":"...","syscall":...,
//! "path":...,"errno":-4058}` (the `errno` key is omitted when the failure
//! has no OS errno, as in Node) and the JS layer raises a Node-shaped `Error`
//! from it.
//! Runtime fs reads are global (the child_process surface already grants full
//! read capability, so the sandbox is a mis-write fuse rather than a security
//! boundary); write targets stay confined to the sandbox roots: the loader's
//! `allowed_roots`, the agent directory, the OS temp dir and the runtime cwd.
//! The module-loading reads (`npm` `readSource`) keep the stricter check.

use std::io::{Read as _, Write as _};
use std::net::ToSocketAddrs as _;
use std::path::{Component, Path, PathBuf};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rquickjs::Object;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha512};

use super::*;

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

/// One node-surface failure: Node-style code plus the conventional
/// `syscall`/`path` fields the JS `Error` carries.
#[derive(Debug)]
pub(super) struct NodeError {
    code: &'static str,
    message: String,
    syscall: Option<&'static str>,
    path: Option<String>,
    /// `err.errno`: Node reports the negated OS errno on POSIX and libuv's
    /// `UV__*` constant on Windows (`ENOENT` is -2 on Linux, -4058 on
    /// Windows). `None` for failures that carry no OS errno — argument
    /// validation and the `ERR_*` classes — because Node leaves the field
    /// absent there too.
    errno: Option<i32>,
}

impl NodeError {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            syscall: None,
            path: None,
            errno: code_errno(code),
        }
    }

    /// Write-sandbox violation (write target outside every allowed root).
    pub(super) fn denied(syscall: &'static str, path: &str) -> Self {
        Self {
            code: "EACCES",
            message: format!("EACCES: permission denied, {syscall} '{path}'"),
            syscall: Some(syscall),
            path: Some(path.to_owned()),
            errno: code_errno("EACCES"),
        }
    }

    /// Map a real IO failure, inferring the Node code from the error kind and
    /// the raw OS error where the kind is too coarse.
    fn io(error: &std::io::Error, syscall: &'static str, path: &Path) -> Self {
        let code = io_error_code(error);
        let path = path.to_string_lossy().into_owned();
        // The message text is Node's (`uv_strerror`): `std::io::Error`'s own
        // Display goes through FormatMessage on Windows and would leak the
        // host locale into `err.message`, which both upstream tests and
        // consumer code match on.
        let message = match code_message(code) {
            Some(text) => format!("{code}: {text}, {syscall} '{path}'"),
            None => format!("{code}: {error}, {syscall} '{path}'"),
        };
        Self {
            code,
            message,
            syscall: Some(syscall),
            path: Some(path),
            errno: io_errno(error, code),
        }
    }

    fn to_json(&self) -> JsonValue {
        let mut out = serde_json::Map::new();
        out.insert("ok".to_owned(), JsonValue::Bool(false));
        out.insert("code".to_owned(), JsonValue::String(self.code.to_owned()));
        out.insert(
            "message".to_owned(),
            JsonValue::String(self.message.clone()),
        );
        if let Some(syscall) = self.syscall {
            out.insert("syscall".to_owned(), JsonValue::String(syscall.to_owned()));
        }
        if let Some(path) = &self.path {
            out.insert("path".to_owned(), JsonValue::String(path.clone()));
        }
        if let Some(errno) = self.errno {
            out.insert("errno".to_owned(), JsonValue::Number(errno.into()));
        }
        JsonValue::Object(out)
    }
}

type NodeResult = Result<JsonValue, NodeError>;

/// `err.errno` for a failure whose only information is the node code: the
/// libuv constant on Windows, the libc number on Linux. A code absent from
/// the table keeps `errno` unset rather than carrying a wrong number (macOS
/// diverges from Linux for `ENOTEMPTY`/`ENOTSUP`/`ELOOP`/`ENAMETOOLONG`, and
/// every code-only failure here is a pir-side validation error anyway).
#[cfg(windows)]
fn code_errno(code: &str) -> Option<i32> {
    // Verbatim from libuv `include/uv/errno.h` (the `_WIN32` fallback block).
    Some(match code {
        "E2BIG" => -4093,
        "EACCES" => -4092,
        "EADDRINUSE" => -4091,
        "EADDRNOTAVAIL" => -4090,
        "EAFNOSUPPORT" => -4089,
        "EAGAIN" => -4088,
        "EALREADY" => -4084,
        "EBADF" => -4083,
        "EBUSY" => -4082,
        "ECANCELED" => -4081,
        "ECHARSET" => -4080,
        "ECONNABORTED" => -4079,
        "ECONNREFUSED" => -4078,
        "ECONNRESET" => -4077,
        "EDESTADDRREQ" => -4076,
        "EEXIST" => -4075,
        "EFAULT" => -4074,
        "EHOSTUNREACH" => -4073,
        "EINTR" => -4072,
        "EINVAL" => -4071,
        "EIO" => -4070,
        "EISCONN" => -4069,
        "EISDIR" => -4068,
        "ELOOP" => -4067,
        "EMFILE" => -4066,
        "EMSGSIZE" => -4065,
        "ENAMETOOLONG" => -4064,
        "ENETDOWN" => -4063,
        "ENETUNREACH" => -4062,
        "ENFILE" => -4061,
        "ENOBUFS" => -4060,
        "ENODEV" => -4059,
        "ENOENT" => -4058,
        "ENOMEM" => -4057,
        "ENONET" => -4056,
        "ENOSPC" => -4055,
        "ENOSYS" => -4054,
        "ENOTCONN" => -4053,
        "ENOTDIR" => -4052,
        "ENOTEMPTY" => -4051,
        "ENOTSOCK" => -4050,
        "ENOTSUP" => -4049,
        "EPERM" => -4048,
        "EPIPE" => -4047,
        "EPROTO" => -4046,
        "EPROTONOSUPPORT" => -4045,
        "EPROTOTYPE" => -4044,
        "EROFS" => -4043,
        "ESHUTDOWN" => -4042,
        "ESPIPE" => -4041,
        "ESRCH" => -4040,
        "ETIMEDOUT" => -4039,
        "ETXTBSY" => -4038,
        "EXDEV" => -4037,
        "EFBIG" => -4036,
        "ENOPROTOOPT" => -4035,
        "ERANGE" => -4034,
        "ENXIO" => -4033,
        "EMLINK" => -4032,
        "EHOSTDOWN" => -4031,
        "EREMOTEIO" => -4030,
        "ENOTTY" => -4029,
        "EFTYPE" => -4028,
        "EILSEQ" => -4027,
        "EOVERFLOW" => -4026,
        "ESOCKTNOSUPPORT" => -4025,
        "ENODATA" => -4024,
        "EUNATCH" => -4023,
        "ENOEXEC" => -4022,
        _ => return None,
    })
}

#[cfg(all(unix, target_os = "linux"))]
fn code_errno(code: &str) -> Option<i32> {
    Some(match code {
        "EPERM" => -1,
        "ENOENT" => -2,
        "ESRCH" => -3,
        "EINTR" => -4,
        "EIO" => -5,
        "ENXIO" => -6,
        "E2BIG" => -7,
        "ENOEXEC" => -8,
        "EBADF" => -9,
        "EAGAIN" => -11,
        "ENOMEM" => -12,
        "EACCES" => -13,
        "EFAULT" => -14,
        "EBUSY" => -16,
        "EEXIST" => -17,
        "EXDEV" => -18,
        "ENODEV" => -19,
        "ENOTDIR" => -20,
        "EISDIR" => -21,
        "EINVAL" => -22,
        "ENFILE" => -23,
        "EMFILE" => -24,
        "ENOTTY" => -25,
        "ETXTBSY" => -26,
        "EFBIG" => -27,
        "ENOSPC" => -28,
        "ESPIPE" => -29,
        "EROFS" => -30,
        "EMLINK" => -31,
        "EPIPE" => -32,
        "ERANGE" => -34,
        "ENAMETOOLONG" => -36,
        "ENOSYS" => -38,
        "ENOTEMPTY" => -39,
        "ELOOP" => -40,
        "ENODATA" => -61,
        "EPROTO" => -71,
        "EOVERFLOW" => -75,
        "ENOTSOCK" => -88,
        "EDESTADDRREQ" => -89,
        "EMSGSIZE" => -90,
        "EPROTOTYPE" => -91,
        "ENOPROTOOPT" => -92,
        "EPROTONOSUPPORT" => -93,
        "ENOTSUP" => -95,
        "EADDRINUSE" => -98,
        "EADDRNOTAVAIL" => -99,
        "ENETDOWN" => -100,
        "ENETUNREACH" => -101,
        "ECONNABORTED" => -103,
        "ECONNRESET" => -104,
        "ENOBUFS" => -105,
        "EISCONN" => -106,
        "ENOTCONN" => -107,
        "ESHUTDOWN" => -108,
        "ETIMEDOUT" => -110,
        "ECONNREFUSED" => -111,
        "EHOSTUNREACH" => -113,
        "EALREADY" => -114,
        "EINPROGRESS" => -115,
        "ECANCELED" => -125,
        _ => return None,
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn code_errno(_code: &str) -> Option<i32> {
    None
}

/// Prefer the real OS error: `-errno` on POSIX, the libuv constant derived
/// from the node code on Windows (Win32 raw codes are not libuv codes).
fn io_errno(error: &std::io::Error, code: &str) -> Option<i32> {
    match error.raw_os_error() {
        Some(raw) if !cfg!(windows) => Some(-raw),
        _ => code_errno(code),
    }
}

/// `uv_strerror`'s English text for a node code — the wording Node puts after
/// `"<CODE>: "` in `err.message`, so `"EEXIST: file already exists, mkdir"`.
/// Identical on Windows and POSIX for every code listed (libuv ships the same
/// strings, and glibc/musl's are the same words); an unlisted code falls back
/// to the platform's own message.
fn code_message(code: &str) -> Option<&'static str> {
    Some(match code {
        "E2BIG" => "argument list too long",
        "EACCES" => "permission denied",
        "EADDRINUSE" => "address already in use",
        "EAGAIN" => "resource temporarily unavailable",
        "EALREADY" => "connection already in progress",
        "EBADF" => "bad file descriptor",
        "EBUSY" => "resource busy or locked",
        "ECANCELED" => "operation canceled",
        "ECONNABORTED" => "software caused connection abort",
        "ECONNREFUSED" => "connection refused",
        "ECONNRESET" => "connection reset by peer",
        "EDESTADDRREQ" => "destination address required",
        "EEXIST" => "file already exists",
        "EFAULT" => "bad address in system call argument",
        "EFBIG" => "file too large",
        "EFTYPE" => "inappropriate file type or format",
        "EHOSTDOWN" => "host is down",
        "EHOSTUNREACH" => "host is unreachable",
        "EILSEQ" => "illegal byte sequence",
        "EINPROGRESS" => "operation now in progress",
        "EINTR" => "interrupted system call",
        "EINVAL" => "invalid argument",
        "EIO" => "i/o error",
        "EISCONN" => "socket is already connected",
        "EISDIR" => "illegal operation on a directory",
        "ELOOP" => "too many symbolic links encountered",
        "EMFILE" => "too many open files",
        "EMLINK" => "too many links",
        "EMSGSIZE" => "message too long",
        "ENAMETOOLONG" => "name too long",
        "ENETDOWN" => "network is down",
        "ENETUNREACH" => "network is unreachable",
        "ENFILE" => "file table overflow",
        "ENODATA" => "no data available",
        "ENODEV" => "no such device",
        "ENOENT" => "no such file or directory",
        "ENOEXEC" => "exec format error",
        "ENOLCK" => "no locks available",
        "ENOMEM" => "not enough memory",
        "ENOMSG" => "no message of desired type",
        "ENONET" => "machine is not on the network",
        "ENOPROTOOPT" => "protocol not available",
        "ENOSPC" => "no space left on device",
        "ENOSYS" => "function not implemented",
        "ENOTCONN" => "socket is not connected",
        "ENOTDIR" => "not a directory",
        "ENOTEMPTY" => "directory not empty",
        "ENOTSOCK" => "socket operation on non-socket",
        "ENOTSUP" => "operation not supported on socket",
        "ENOTTY" => "inappropriate ioctl for device",
        "ENXIO" => "no such device or address",
        "EOVERFLOW" => "value too large for defined data type",
        "EPERM" => "operation not permitted",
        "EPIPE" => "broken pipe",
        "EPROTO" => "protocol error",
        "EPROTONOSUPPORT" => "protocol not supported",
        "EPROTOTYPE" => "protocol wrong type for socket",
        "ERANGE" => "result too large",
        "EREMOTEIO" => "remote I/O error",
        "EROFS" => "read-only file system",
        "ESHUTDOWN" => "cannot send after transport endpoint shutdown",
        "ESOCKTNOSUPPORT" => "socket type not supported",
        "ESPIPE" => "invalid seek",
        "ESRCH" => "no such process",
        "ETIMEDOUT" => "connection timed out",
        "ETXTBSY" => "text file is busy",
        "EUNATCH" => "protocol driver not attached",
        "EXDEV" => "cross-device link not permitted",
        _ => return None,
    })
}

fn io_error_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    let raw = error.raw_os_error();
    // Win32 codes the Rust `ErrorKind` is too coarse for. The mapping matches
    // what Node reports for the same failure (libuv's
    // `uv_translate_sys_error` table): 1314 ERROR_PRIVILEGE_NOT_HELD — the
    // symlink-without-developer-mode case, which Node's own fs tests exercise —
    // is EPERM, sharing/lock violations are EBUSY, and so on.
    if cfg!(windows) {
        match raw {
            Some(5) | Some(1314) => return "EPERM",
            Some(32) | Some(33) => return "EBUSY",
            Some(17) => return "EXDEV",
            Some(112) => return "ENOSPC",
            Some(145) => return "ENOTEMPTY",
            Some(206) => return "ENAMETOOLONG",
            Some(267) => return "ENOTDIR",
            Some(87) | Some(123) | Some(1004) => return "EINVAL",
            Some(995) => return "ECANCELED",
            _ => {}
        }
    }
    match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        // Windows maps ERROR_ACCESS_DENIED (raw 5) to EPERM, like Node's
        // libuv-backed fs; Unix keeps EACCES.
        ErrorKind::PermissionDenied => {
            if cfg!(windows) && raw == Some(5) {
                "EPERM"
            } else {
                "EACCES"
            }
        }
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::IsADirectory => "EISDIR",
        _ => match raw {
            // Cross-device rename: ERROR_NOT_SAME_DEVICE / Unix EXDEV.
            Some(17) if cfg!(windows) => "EXDEV",
            Some(18) if !cfg!(windows) => "EXDEV",
            Some(5) if cfg!(windows) => "EPERM",
            // libc EBADF (pread/pwrite on an O_WRONLY/O_RDONLY descriptor).
            // Rust reports it as `Uncategorized`, so without this raw mapping
            // the node fs surface would return EIO instead of Node's EBADF.
            Some(9) if !cfg!(windows) => "EBADF",
            _ => "EIO",
        },
    }
}

// ---------------------------------------------------------------------------
// Registration + dispatch
// ---------------------------------------------------------------------------

pub(super) fn register_node_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    generation: u64,
) -> rquickjs::Result<()> {
    let state = state.clone();
    let allowed_roots = allowed_roots.clone();
    host.set(
        "node",
        Func::from(
            move |module: String, op: String, payload: String| -> String {
                node_call(&state, &allowed_roots, generation, &module, &op, &payload)
            },
        ),
    )?;
    Ok(())
}

fn node_call(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    generation: u64,
    module: &str,
    op: &str,
    payload: &str,
) -> String {
    let started = std::time::Instant::now();
    let payload: JsonValue = serde_json::from_str(payload).unwrap_or(JsonValue::Null);
    let result = match module {
        "process" => process_op(state, op, &payload),
        "fs" => fs_op(state, allowed_roots, generation, op, &payload),
        "os" => os_op(op),
        "crypto" => crypto_op(op, &payload),
        // Raw-DEFLATE zlib ops (the gzip/zlib-wrapped pair stays on the
        // legacy `nodeExt` table in child_fns); see `__pirNodeCall`'s op
        // routing in js/node_call_helper.js.txt.
        "zlib" => zlib_raw_op(op, &payload),
        // `dns/promises.lookup` behind the @node/dns/promises shim.
        "dns" => dns_op(op, &payload),
        // npm resolution + CJS source reads behind `createRequire` and the
        // loader's CJS wrapper.
        "npm" => npm_op(state, allowed_roots, op, &payload),
        // Real SQLite (rusqlite bundled) behind the `node:sqlite`
        // DatabaseSync shim and the better-sqlite3 / sqlite3 virtual package
        // shims.
        "sqlite" => {
            super::sqlite_fns::sqlite_op(state, allowed_roots, generation, op, &payload)
        }
        // EksBlowfish behind the `bcrypt` virtual package shim (the real
        // package is a NAPI native addon).
        "bcrypt" => super::bcrypt_fns::bcrypt_op(op, &payload),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown node host module `{other}`"),
        )),
    };
    let output = match result {
        // `Display for Value` is the compact serialization (`to_writer`), the
        // same bytes `to_string` produces, so the module op's own value is
        // written straight into the envelope text. `json!({ "ok": true,
        // "value": v })` would instead allocate the envelope `Map` plus its two
        // key strings AND rebuild the value through `to_value(&v)` — a full
        // recursive copy of, e.g., the `fs.readFile` base64 blob or the
        // `crypto` `update` ciphertext chunk.
        Ok(value) => format!(r#"{{"ok":true,"value":{value}}}"#),
        Err(error) => error.to_json().to_string(),
    };
    // `PI_TIMING=1` attribution: the elapsed time of this bridge call by
    // static `module.op` (never the path/payload), so a slow discovery scan is
    // attributable to `fs.readdir`/`fs.stat`/... even when no single call
    // crosses the slow-call log threshold.
    super::super::host_op_stats::record(node_op_label(module, op), started.elapsed());
    output
}

/// Static timing label for one `node` bridge call (`fs.readdir`, ...). The
/// module/op strings come from the JS shim's own call sites; an unknown pair
/// is truncated to a short identifier instead of carrying arbitrary text.
fn node_op_label(module: &str, op: &str) -> &'static str {
    match (module, op) {
        ("fs", "readFile") => "fs.readFile",
        ("fs", "stat") => "fs.stat",
        ("fs", "lstat") => "fs.lstat",
        ("fs", "readdir") => "fs.readdir",
        ("fs", "realpath") => "fs.realpath",
        ("fs", "exists") => "fs.exists",
        ("fs", "readlink") => "fs.readlink",
        ("fs", "access") => "fs.access",
        ("fs", "writeFile") => "fs.writeFile",
        ("npm", "resolve") => "npm.resolve",
        ("npm", "readSource") => "npm.readSource",
        ("crypto", _) => "crypto",
        ("os", _) => "os",
        ("process", _) => "process",
        ("dns", _) => "dns",
        ("sqlite", _) => "sqlite",
        ("bcrypt", _) => "bcrypt",
        ("zlib", _) => "zlib",
        _ => "node.other",
    }
}

pub(super) fn payload_str(payload: &JsonValue, key: &str) -> Result<String, NodeError> {
    payload
        .get(key)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing string payload field `{key}`")))
}

/// Borrowing counterpart of [`payload_str`] for the fields that are only read:
/// the text stays inside the parsed payload instead of being copied into an
/// owned `String` that is dropped right after use. The copy `payload_str` makes
/// is proportional to the field, so the base64 data blobs, cipher key material
/// and PEM keys of the crypto/fs ops pay it on every call.
fn payload_text<'a>(payload: &'a JsonValue, key: &str) -> Result<&'a str, NodeError> {
    payload
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing string payload field `{key}`")))
}

fn payload_bool(payload: &JsonValue, key: &str) -> bool {
    payload
        .get(key)
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Path resolution + write sandbox
// ---------------------------------------------------------------------------

/// Effective sandbox roots: the loader allow-list plus the agent directory,
/// the pi config root (`~/.pi`, extension data dirs live at `~/.pi/<ext>`),
/// the OS temp directory and the runtime cwd, each in check-normal form.
pub(super) fn effective_roots(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = allowed_roots
        .lock()
        .map(|roots| roots.clone())
        .unwrap_or_default();
    let agent_dir = pir_agent_dir();
    // Allowing `~/.pi/agent` (with credentials) while rejecting sibling
    // extension data dirs (`~/.pi/<ext>`, e.g. context-mode's sessions) was
    // not coherent; the whole pi config root is the extension data boundary.
    if let Some(pi_root) = agent_dir.parent() {
        roots.push(pi_root.to_path_buf());
    }
    roots.push(agent_dir);
    // Extensions use os.tmpdir() as scratch space (pi-subagents'
    // `pi-subagents-<scope>` temp root and result dirs are created during
    // load); the OS temp dir is the designated per-process scratch area.
    roots.push(std::env::temp_dir());
    roots.push(lock_state(state).config.cwd.clone());
    roots
        .into_iter()
        .map(|root| normalize_for_check(&root))
        .collect()
}

/// Canonicalize when the path exists; otherwise canonicalize the deepest
/// existing parent and append the remainder lexically, so a symlinked
/// directory inside an allowed root cannot smuggle a target outside it.
pub(super) fn normalize_for_check(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_extended_prefix(canonical);
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            let mut joined = strip_extended_prefix(canonical_parent);
            joined.push(name);
            return lexical_normalize(&joined);
        }
    }
    lexical_normalize(path)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `\\?\C:\...` -> `C:\...` (and `\\?\UNC\a\b` -> `\\a\b`) so canonical and
/// lexical forms display and compare identically.
fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

/// Component-wise prefix test, case-insensitive on Windows (NTFS is).
pub(super) fn path_within(candidate: &Path, root: &Path) -> bool {
    let root_parts: Vec<String> = root.components().map(component_key).collect();
    if root_parts.is_empty() {
        return false;
    }
    let candidate_parts: Vec<String> = candidate.components().map(component_key).collect();
    candidate_parts.len() >= root_parts.len()
        && root_parts.iter().zip(&candidate_parts).all(|(a, b)| a == b)
}

fn component_key(component: Component<'_>) -> String {
    let text = component.as_os_str().to_string_lossy().into_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// Resolve one payload path field and enforce the root fence (fs write
/// targets and the module-loading reads; runtime fs reads skip it). IO then
/// uses the original path text: `..`/links are resolved by the OS call itself.
fn checked_path(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
    key: &str,
    syscall: &'static str,
) -> Result<PathBuf, NodeError> {
    let raw = payload_text(payload, key)?;
    // Relative paths join the runtime cwd first (Node's process.cwd()
    // semantics): the raw form is neither inside any absolute root nor
    // meaningful on its own, so checking it unjoined would deny every
    // relative write.
    let path = resolve_fs_path(state, raw);
    let checked = normalize_for_check(&path);
    let roots = effective_roots(state, allowed_roots);
    if !roots.iter().any(|root| path_within(&checked, root)) {
        return Err(NodeError::denied(syscall, raw));
    }
    // A dangling symlink canonicalizes to its parent + name, so the root
    // check passes, but the subsequent open/write follows the link and can
    // create a file outside the sandbox. Reject any symlink leaf; the
    // canonical parent check above already covers symlinked directories.
    if std::fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(NodeError::denied(syscall, raw));
    }
    Ok(path)
}

/// Fs write ops fence their target through `checked_path`; the distinct name
/// states the policy at the fs call sites (writes are fenced, reads are not).
fn check_write(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
    key: &str,
    syscall: &'static str,
) -> Result<PathBuf, NodeError> {
    checked_path(state, allowed_roots, payload, key, syscall)
}

/// Resolve one payload path field for a read: runtime reads are global (the
/// child_process surface already grants full read capability), so no sandbox
/// check applies — but a relative path still resolves against the runtime
/// cwd, exactly like Node resolves fs paths against `process.cwd()`. The pir
/// process's own working directory is NOT the same thing (a tool session runs
/// against the project dir while pir.exe may have been launched elsewhere).
fn read_path(
    state: &Arc<StdMutex<RuntimeState>>,
    payload: &JsonValue,
    key: &str,
) -> Result<PathBuf, NodeError> {
    Ok(resolve_fs_path(state, payload_text(payload, key)?))
}

/// Node fs semantics for relative paths: join them onto the runtime cwd.
/// Absolute paths (and Windows drive/UNC forms) pass through untouched.
fn resolve_fs_path(state: &Arc<StdMutex<RuntimeState>>, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        lock_state(state).config.cwd.join(path)
    }
}

// ---------------------------------------------------------------------------
// fs
// ---------------------------------------------------------------------------

/// Largest file `fs.readFile` will materialize in the host.
///
/// `readFile` returns one base64 JSON string, so an unbounded read of a
/// multi-GB file allocates the file, its base64 image and the JSON envelope
/// (3x) on the host heap before QuickJS even sees it. 32 MiB matches the
/// context-mode file budget and is far above source/config assets, while a
/// deliberate read of a bigger file must use the fd/stream surface (which
/// reads bounded windows). Node reports `E2BIG` for an argument that exceeds
/// an implementation limit; the message names the cap.
const MAX_READ_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Read a whole file with [`MAX_READ_FILE_BYTES`] as a hard upper bound.
///
/// Uses `take(cap + 1)` rather than a metadata pre-check: the file can grow
/// between the check and the read, and the bound must hold either way. A
/// file that exceeds the cap is refused with `E2BIG`; only `cap + 1` bytes
/// were ever buffered.
fn read_file_capped(path: &Path) -> Result<Vec<u8>, NodeError> {
    let file = std::fs::File::open(path).map_err(|error| read_file_error(&error, path))?;
    let mut bytes = Vec::new();
    file.take(MAX_READ_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| read_file_error(&error, path))?;
    if bytes.len() as u64 > MAX_READ_FILE_BYTES {
        return Err(NodeError::new(
            "E2BIG",
            format!(
                "E2BIG: file exceeds the {}-MiB readFile limit, read",
                MAX_READ_FILE_BYTES / (1024 * 1024)
            ),
        ));
    }
    Ok(bytes)
}

/// Write-data channel, preferred order: `dataText` (the exact UTF-8 image of
/// the bytes, sent by the JS side for plain-string writes — no base64 round
/// trip on either leg) then the base64 `data` (byte payloads: Buffer/typed
/// arrays, or any string that is not plain UTF-8 text). A non-string
/// `dataText` is ignored rather than rejected so the base64 channel stays the
/// ABI default.
fn payload_write_data(payload: &JsonValue) -> Result<Vec<u8>, NodeError> {
    if let Some(text) = payload.get("dataText").and_then(JsonValue::as_str) {
        return Ok(text.as_bytes().to_vec());
    }
    decode_base64(payload_text(payload, "data")?)
}

/// fs op table: reads run globally; writes, write-capable opens and mixed-op
/// targets stay fenced to the effective roots.
fn fs_op(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    generation: u64,
    op: &str,
    payload: &JsonValue,
) -> NodeResult {
    match op {
        "readFile" => {
            let path = read_path(state, payload, "path")?;
            let bytes = read_file_capped(&path)?;
            Ok(JsonValue::String(STANDARD.encode(bytes)))
        }
        "writeFile" => {
            let path = check_write(state, allowed_roots, payload, "path", "open")?;
            let bytes = payload_write_data(payload)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true);
            if payload_bool(payload, "append") {
                options.append(true);
            } else {
                options.truncate(true);
            }
            let mut file = options
                .open(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            file.write_all(&bytes)
                .map_err(|error| NodeError::io(&error, "write", &path))?;
            Ok(JsonValue::Null)
        }
        "stat" | "lstat" => {
            let symlink = op == "lstat";
            let syscall = if symlink { "lstat" } else { "stat" };
            let path = read_path(state, payload, "path")?;
            let meta = if symlink {
                std::fs::symlink_metadata(&path)
            } else {
                std::fs::metadata(&path)
            }
            .map_err(|error| NodeError::io(&error, syscall, &path))?;
            Ok(metadata_value(&meta, meta.file_type().is_symlink()))
        }
        "readdir" => {
            let path = read_path(state, payload, "path")?;
            let dirents = payload_bool(payload, "dirents");
            let entries = std::fs::read_dir(&path)
                .map_err(|error| NodeError::io(&error, "scandir", &path))?;
            let mut names: Vec<JsonValue> = Vec::new();
            let mut typed: Vec<JsonValue> = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|error| NodeError::io(&error, "scandir", &path))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if dirents {
                    let file_type = entry
                        .file_type()
                        .map_err(|error| NodeError::io(&error, "scandir", &path))?;
                    typed.push(serde_json::json!({
                        "name": name,
                        "isFile": file_type.is_file(),
                        "isDirectory": file_type.is_dir(),
                        "isSymbolicLink": file_type.is_symlink(),
                    }));
                } else {
                    names.push(JsonValue::String(name));
                }
            }
            Ok(if dirents {
                JsonValue::Array(typed)
            } else {
                JsonValue::Array(names)
            })
        }
        "mkdir" => {
            let path = check_write(state, allowed_roots, payload, "path", "mkdir")?;
            let result = if payload_bool(payload, "recursive") {
                std::fs::create_dir_all(&path)
            } else {
                std::fs::create_dir(&path)
            };
            result.map_err(|error| NodeError::io(&error, "mkdir", &path))?;
            Ok(JsonValue::Null)
        }
        "rm" => {
            let path = check_write(state, allowed_roots, payload, "path", "rm")?;
            rm_at(
                &path,
                payload_bool(payload, "recursive"),
                payload_bool(payload, "force"),
            )
            .map_err(|error| NodeError::io(&error, "rm", &path))?;
            Ok(JsonValue::Null)
        }
        "unlink" => {
            let path = check_write(state, allowed_roots, payload, "path", "unlink")?;
            std::fs::remove_file(&path).map_err(|error| NodeError::io(&error, "unlink", &path))?;
            Ok(JsonValue::Null)
        }
        "rename" => {
            // rename removes the source directory entry, so both ends are
            // write operations; a read-only source fence would let an
            // extension move/delete a file outside the allowed roots.
            let from = check_write(state, allowed_roots, payload, "from", "rename")?;
            let to = check_write(state, allowed_roots, payload, "to", "rename")?;
            std::fs::rename(&from, &to).map_err(|error| NodeError::io(&error, "rename", &from))?;
            Ok(JsonValue::Null)
        }
        "copyFile" => {
            let from = read_path(state, payload, "from")?;
            let to = check_write(state, allowed_roots, payload, "to", "copyfile")?;
            std::fs::copy(&from, &to).map_err(|error| NodeError::io(&error, "copyfile", &from))?;
            Ok(JsonValue::Null)
        }
        "realpath" => {
            let path = read_path(state, payload, "path")?;
            let canonical = std::fs::canonicalize(&path)
                .map_err(|error| NodeError::io(&error, "realpath", &path))?;
            Ok(JsonValue::String(
                strip_extended_prefix(canonical)
                    .to_string_lossy()
                    .into_owned(),
            ))
        }
        "access" => {
            let mode = payload.get("mode").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            // W_OK (2) probes the target for writing: fence only that path.
            let path = if mode & 2 != 0 {
                check_write(state, allowed_roots, payload, "path", "access")?
            } else {
                read_path(state, payload, "path")?
            };
            access_check(&path, mode).map_err(|error| NodeError::io(&error, "access", &path))?;
            Ok(JsonValue::Null)
        }
        "readlink" => {
            let path = read_path(state, payload, "path")?;
            let target = std::fs::read_link(&path)
                .map_err(|error| NodeError::io(&error, "readlink", &path))?;
            Ok(JsonValue::String(target.to_string_lossy().into_owned()))
        }
        "chmod" => {
            let path = check_write(state, allowed_roots, payload, "path", "chmod")?;
            let mode = payload_mode(payload);
            chmod_path(&path, mode).map_err(|error| NodeError::io(&error, "chmod", &path))?;
            Ok(JsonValue::Null)
        }
        "cp" => {
            let from = read_path(state, payload, "from")?;
            let to = check_write(state, allowed_roots, payload, "to", "cp")?;
            let meta =
                std::fs::metadata(&from).map_err(|error| NodeError::io(&error, "cp", &from))?;
            if meta.is_dir() {
                if !payload_bool(payload, "recursive") {
                    // Node refuses a directory copy without {recursive:true}.
                    return Err(NodeError {
                        code: "ERR_FS_EISDIR",
                        message: format!(
                            "Path is a directory: cp '{}' -> '{}'",
                            from.display(),
                            to.display()
                        ),
                        syscall: Some("cp"),
                        path: Some(from.to_string_lossy().into_owned()),
                        // Node's own class error: no OS errno behind it.
                        errno: None,
                    });
                }
                copy_tree(&from, &to).map_err(|error| NodeError::io(&error, "cp", &from))?;
            } else {
                std::fs::copy(&from, &to).map_err(|error| NodeError::io(&error, "cp", &from))?;
            }
            Ok(JsonValue::Null)
        }
        "mkdtemp" => {
            let prefix = payload_text(payload, "prefix")?;
            let dir = mkdtemp(state, allowed_roots, prefix)?;
            Ok(JsonValue::String(dir.to_string_lossy().into_owned()))
        }
        "rmdir" => {
            let path = check_write(state, allowed_roots, payload, "path", "rmdir")?;
            let result = if payload_bool(payload, "recursive") {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_dir(&path)
            };
            result.map_err(|error| NodeError::io(&error, "rmdir", &path))?;
            Ok(JsonValue::Null)
        }
        "truncate" => {
            // Mutates the file: fenced like every other write target.
            let path = check_write(state, allowed_roots, payload, "path", "truncate")?;
            let length = payload.get("len").and_then(JsonValue::as_u64).unwrap_or(0);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            file.set_len(length)
                .map_err(|error| NodeError::io(&error, "ftruncate", &path))?;
            Ok(JsonValue::Null)
        }
        "utimes" => {
            // Metadata mutation: fenced like a write.
            let path = check_write(state, allowed_roots, payload, "path", "utimes")?;
            let to_time = |value: Option<f64>| -> std::time::SystemTime {
                // Negative/epoch-preceding stamps clamp to the epoch (some
                // platforms refuse pre-1970 times in the syscall).
                let millis = value.unwrap_or(0.0).max(0.0) as u64;
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
            };
            let atime = to_time(payload.get("atimeMs").and_then(JsonValue::as_f64));
            let mtime = to_time(payload.get("mtimeMs").and_then(JsonValue::as_f64));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            let times = std::fs::FileTimes::new()
                .set_accessed(atime)
                .set_modified(mtime);
            file.set_times(times)
                .map_err(|error| NodeError::io(&error, "utime", &path))?;
            Ok(JsonValue::Null)
        }
        "symlink" => {
            // Node stores any target string (it is never followed here); only
            // the link path itself is sandbox-checked.
            let target = payload_text(payload, "target")?;
            let path = check_write(state, allowed_roots, payload, "path", "symlink")?;
            let kind = payload
                .get("kind")
                .and_then(JsonValue::as_str)
                .unwrap_or("file");
            symlink_impl(target, &path, kind)
                .map_err(|error| NodeError::io(&error, "symlink", &path))?;
            Ok(JsonValue::Null)
        }
        // fd family: `open` fences every write-capable descriptor; later ops
        // act on the held handle, like Node.
        "open" => {
            let flags = open_flags(payload)?;
            let path = if flags.pure_read() {
                read_path(state, payload, "path")?
            } else {
                check_write(state, allowed_roots, payload, "path", "open")?
            };
            let (options, append) = flags.options(payload_mode(payload), &path)?;
            let file = options
                .open(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            let fd = lock_state(state)
                .fd_table
                .insert(file, path, append, generation)
                .map_err(|()| {
                    NodeError::new(
                        "EMFILE",
                        format!(
                            "EMFILE: too many open files, open (limit {})",
                            super::super::state::MAX_OPEN_FDS
                        ),
                    )
                })?;
            Ok(serde_json::json!(fd))
        }
        "read" => {
            let fd = payload_fd(payload)?;
            let length = payload
                .get("length")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0) as usize;
            // 64 MiB cap: a single readSync is caller-buffer-bound anyway.
            if length > (1 << 26) {
                return Err(NodeError::new("EINVAL", "read length out of range"));
            }
            let position = payload.get("position").and_then(JsonValue::as_u64);
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            let path = entry.path.clone();
            let bytes = entry
                .read(length, position)
                .map_err(|error| NodeError::io(&error, "read", &path))?;
            Ok(JsonValue::String(STANDARD.encode(bytes)))
        }
        "write" => {
            let fd = payload_fd(payload)?;
            let data = payload_write_data(payload)?;
            let position = payload.get("position").and_then(JsonValue::as_u64);
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            let path = entry.path.clone();
            let written = entry
                .write(&data, position)
                .map_err(|error| NodeError::io(&error, "write", &path))?;
            Ok(serde_json::json!(written))
        }
        "fstat" => {
            let fd = payload_fd(payload)?;
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            let meta = entry
                .file
                .metadata()
                .map_err(|error| NodeError::io(&error, "fstat", &entry.path))?;
            Ok(metadata_value(&meta, false))
        }
        "fsync" => {
            let fd = payload_fd(payload)?;
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            entry
                .file
                .sync_all()
                .map_err(|error| NodeError::io(&error, "fsync", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "fchmod" => {
            let fd = payload_fd(payload)?;
            let mode = payload_mode(payload);
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            chmod_file(&entry.file, mode)
                .map_err(|error| NodeError::io(&error, "fchmod", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "ftruncate" => {
            let fd = payload_fd(payload)?;
            let length = payload.get("len").and_then(JsonValue::as_u64).unwrap_or(0);
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            entry
                .file
                .set_len(length)
                .map_err(|error| NodeError::io(&error, "ftruncate", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "fdatasync" => {
            let fd = payload_fd(payload)?;
            let mut guard = lock_state(state);
            let entry = guard.fd_table.get_mut(fd).ok_or_else(|| fd_error(fd))?;
            entry
                .file
                .sync_data()
                .map_err(|error| NodeError::io(&error, "fdatasync", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "close" => {
            let fd = payload_fd(payload)?;
            lock_state(state)
                .fd_table
                .remove(fd)
                .ok_or_else(|| fd_error(fd))?;
            Ok(JsonValue::Null)
        }
        "exists" => {
            let path = read_path(state, payload, "path")?;
            Ok(JsonValue::Bool(path.exists()))
        }
        other => Err(NodeError::new("ENOSYS", format!("unknown fs op `{other}`"))),
    }
}

/// `readFile`'s failure shape. `std::fs::read` cannot open a directory on
/// Windows (Win32 needs FILE_FLAG_BACKUP_SEMANTICS) and reports the raw denial
/// as EPERM, where Node reports EISDIR; the error path re-checks the target and
/// answers with Node's own wording ("EISDIR: illegal operation on a directory,
/// read", no `path` field). Only the failure path pays for the check.
fn read_file_error(error: &std::io::Error, path: &Path) -> NodeError {
    if path.is_dir() {
        return NodeError {
            code: "EISDIR",
            message: "EISDIR: illegal operation on a directory, read".to_owned(),
            syscall: Some("read"),
            path: None,
            errno: code_errno("EISDIR"),
        };
    }
    NodeError::io(error, "open", path)
}

/// Node's `flags` argument for `open`: either a flag string (`"r"`, `"ax+"`,
/// …) or an int32 bit mask, i.e. a combination of the `fs.constants` `O_*`
/// values. The JS shim runs node's `stringToFlags` validation (int32 range,
/// `null` → `O_RDONLY`, unknown strings rejected) and forwards whichever form
/// it received, so both are parsed here.
enum OpenFlags<'a> {
    Text(&'a str),
    Bits(i64),
}

/// `fs.constants` bits, verbatim from the values the `fs` shim publishes
/// (node's win32 table — `pir`'s own `fs.constants` shim is that table, so a
/// numeric mask is interpreted against the constants the caller could read).
/// The access mode is packed in the low two bits (`O_ACCMODE`).
const O_WRONLY: i64 = 1;
const O_RDWR: i64 = 2;
const O_ACCMODE: i64 = 3;
const O_APPEND: i64 = 8;
const O_CREAT: i64 = 256;
const O_TRUNC: i64 = 512;
const O_EXCL: i64 = 1024;

fn open_flags(payload: &JsonValue) -> Result<OpenFlags<'_>, NodeError> {
    match payload.get("flags") {
        // The string form keeps the host-side validation `open_options` has
        // always done (unknown flag strings are `ERR_INVALID_ARG_VALUE`).
        Some(JsonValue::String(text)) => Ok(OpenFlags::Text(text)),
        Some(value) if value.is_number() => value
            .as_i64()
            .filter(|bits| i32::try_from(*bits).is_ok())
            .map(OpenFlags::Bits)
            .ok_or_else(|| {
                NodeError::new(
                    "ERR_OUT_OF_RANGE",
                    format!(
                        "The value of \"flags\" is out of range. It must be >= -2147483648 && <= 2147483647. Received {value}"
                    ),
                )
            }),
        Some(value) => Err(NodeError::new(
            "ERR_INVALID_ARG_VALUE",
            format!("The argument 'flags' is invalid. Received '{value}'"),
        )),
        None => Err(NodeError::new("EINVAL", "missing `flags` payload field")),
    }
}

impl OpenFlags<'_> {
    /// Whether the open can only read (the sandbox fence): a read-only access
    /// mode with no create/truncate/append bit. `O_RDONLY|O_CREAT` creates a
    /// file and stays fenced even though the access mode reads.
    fn pure_read(&self) -> bool {
        match self {
            // `r`/`rs`/`sr` only reads (global); `w`/`a`/`+`/`x` writes or
            // creates (`r+` is a write) and stays fenced.
            OpenFlags::Text(text) => !text.contains(['w', 'a', '+', 'x']),
            OpenFlags::Bits(bits) => {
                (bits & O_ACCMODE) == 0 && (bits & (O_APPEND | O_CREAT | O_TRUNC | O_EXCL)) == 0
            }
        }
    }

    fn options(&self, mode: u32, path: &Path) -> Result<(std::fs::OpenOptions, bool), NodeError> {
        match self {
            OpenFlags::Text(text) => open_options(text, mode),
            OpenFlags::Bits(bits) => open_options_bits(*bits, mode, path),
        }
    }
}

/// Node's Windows shape for a numeric mask the OS refuses (`uv_fs_open` →
/// `EINVAL`, libuv errno -4071): the message carries the syscall and path, no
/// argument-validation wording.
fn invalid_open_flags(path: &Path) -> NodeError {
    let text = path.to_string_lossy().into_owned();
    NodeError {
        code: "EINVAL",
        message: format!("EINVAL: invalid argument, open '{text}'"),
        syscall: Some("open"),
        path: Some(text),
        errno: code_errno("EINVAL"),
    }
}

/// Numeric `flags`: map the access mode plus the `O_CREAT`/`O_EXCL`/
/// `O_TRUNC`/`O_APPEND` bits onto the host `OpenOptions`. Access mode 3
/// (`O_WRONLY|O_RDWR`) and a read-only truncation are invalid combinations and
/// answer with node's `EINVAL`; unknown bits (e.g. node's read-only `O_SYNC`
/// hints) are ignored like the OS does, and `O_EXCL` only takes effect with
/// `O_CREAT` (the CRT/`O_EXCL` contract node inherits on win32, where
/// `O_RDONLY|O_CREAT` also opens without creating). Returns the prepared
/// options plus whether the descriptor must append.
fn open_options_bits(
    bits: i64,
    mode: u32,
    path: &Path,
) -> Result<(std::fs::OpenOptions, bool), NodeError> {
    let mut options = std::fs::OpenOptions::new();
    let access = bits & O_ACCMODE;
    let write_access = access == O_WRONLY || access == O_RDWR;
    match access {
        0 => {
            options.read(true);
        }
        O_WRONLY => {
            options.write(true);
        }
        O_RDWR => {
            options.read(true).write(true);
        }
        // `O_WRONLY|O_RDWR`: not a valid `O_ACCMODE` value.
        _ => return Err(invalid_open_flags(path)),
    }
    // A read-only truncation is refused by the OS (`EINVAL` on win32's CRT,
    // `open(2)` on Linux); rejecting it here keeps that error shape instead of
    // letting `truncate(true)` turn it into an access denial.
    if bits & O_TRUNC != 0 {
        if !write_access {
            return Err(invalid_open_flags(path));
        }
        options.truncate(true);
    }
    if bits & O_CREAT != 0 && write_access {
        if bits & O_EXCL != 0 {
            options.create_new(true);
        } else {
            options.create(true);
        }
    }
    let append = bits & O_APPEND != 0 && write_access;
    if append {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode & 0o777);
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok((options, append))
}

/// Node's flag strings, verbatim: the r/w/a bases with the optional `+`
/// (read+write), `x` (fail when the path exists) and `s`/`as` sync hints
/// (ignored — the host never buffers, so the hint changes nothing observable).
/// Returns the prepared options plus whether the descriptor must append.
/// `mode` reaches the Unix create call only; Windows keeps the default
/// attributes (the platform has no permission bits beyond readonly).
fn open_options(flags: &str, mode: u32) -> Result<(std::fs::OpenOptions, bool), NodeError> {
    // Node rejects an unknown flag string in the fs layer itself: that is a
    // JS-level argument error (`ERR_INVALID_ARG_VALUE`, no OS errno), not an
    // EINVAL from the OS. The accepted set is node's `stringToFlags` switch
    // (both orders of the two-character tails), so near-misses like `rz` are
    // refused instead of being read as `r`.
    let invalid = || {
        NodeError::new(
            "ERR_INVALID_ARG_VALUE",
            format!("The argument 'flags' is invalid. Received '{flags}'"),
        )
    };
    // (read, write, append, create, create_new, truncate)
    let spec = match flags {
        "r" | "rs" | "sr" => (true, false, false, false, false, false),
        "r+" | "rs+" | "sr+" => (true, true, false, false, false, false),
        "w" => (false, true, false, true, false, true),
        "w+" => (true, true, false, true, false, true),
        "wx" | "xw" => (false, true, false, false, true, false),
        "wx+" | "xw+" => (true, true, false, false, true, false),
        "a" | "as" | "sa" => (false, true, true, true, false, false),
        "ax" | "xa" => (false, true, true, false, true, false),
        "a+" | "as+" | "sa+" => (true, true, true, true, false, false),
        "ax+" | "xa+" => (true, true, true, false, true, false),
        _ => return Err(invalid()),
    };
    let (read, write, append, create, create_new, truncate) = spec;
    let mut options = std::fs::OpenOptions::new();
    options.read(read).write(write).append(append);
    if create_new {
        // O_CREAT|O_EXCL: the create bit is implied and `truncate` would be
        // meaningless on a file that must not exist.
        options.create_new(true);
    } else if create {
        options.create(true);
    }
    if truncate {
        options.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Node's `mode` only applies at file creation (O_CREAT).
        options.mode(mode & 0o777);
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok((options, append))
}

fn payload_mode(payload: &JsonValue) -> u32 {
    payload.get("mode").and_then(JsonValue::as_u64).unwrap_or(0) as u32
}

fn payload_fd(payload: &JsonValue) -> Result<i32, NodeError> {
    payload
        .get("fd")
        .and_then(JsonValue::as_i64)
        .map(|fd| fd as i32)
        .ok_or_else(|| NodeError::new("EINVAL", "missing numeric `fd` payload field"))
}

/// Node reports an unknown/closed descriptor as EBADF with the raw number.
fn fd_error(fd: i32) -> NodeError {
    NodeError::new("EBADF", format!("EBADF: bad file descriptor, fd {fd}"))
}

/// Permissions for a Node `mode`. Windows maps the owner-write bit (0o200)
/// onto the readonly attribute, the only permission bit the platform has;
/// Unix applies the full bit set.
#[cfg(windows)]
fn mode_permissions(meta: &std::fs::Metadata, mode: u32) -> std::fs::Permissions {
    let mut permissions = meta.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    permissions
}

#[cfg(unix)]
fn mode_permissions(_meta: &std::fs::Metadata, mode: u32) -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::Permissions::from_mode(mode & 0o7777)
}

fn chmod_path(path: &Path, mode: u32) -> std::io::Result<()> {
    let meta = std::fs::metadata(path)?;
    std::fs::set_permissions(path, mode_permissions(&meta, mode))
}

fn chmod_file(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
    let meta = file.metadata()?;
    file.set_permissions(mode_permissions(&meta, mode))
}

/// Recursive copy core for `cp`: files via `std::fs::copy` (contents plus
/// permissions), directories created and walked; symlinks are followed (the
/// audited extension surface never asks for link-preserving copies).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// `mkdtemp` core: Node appends six random characters to the prefix. The
/// parent directory is fenced like every other write target (cwd, the sandbox
/// roots, the agent dir, the OS temp dir) — Node's own tests call mkdtemp on a
/// directory under the test tree, so a temp-dir-only fence would be stricter
/// than Node for no safety gain. A relative prefix resolves inside the system
/// temp dir, as before.
fn mkdtemp(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    prefix: &str,
) -> Result<PathBuf, NodeError> {
    let raw = PathBuf::from(prefix);
    let base = if raw.is_absolute() {
        raw
    } else {
        std::env::temp_dir().join(prefix)
    };
    let (parent, stem) = if prefix.ends_with('\\') || prefix.ends_with('/') {
        (base.clone(), String::new())
    } else {
        (
            base.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| normalize_for_check(&std::env::temp_dir())),
            base.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };
    let checked_parent = normalize_for_check(&parent);
    let roots = effective_roots(state, allowed_roots);
    if !roots.iter().any(|root| path_within(&checked_parent, root)) {
        return Err(NodeError::denied("mkdtemp", prefix));
    }
    for _ in 0..128 {
        let candidate = parent.join(format!("{stem}{}", random_suffix()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(NodeError::io(&error, "mkdtemp", &candidate)),
        }
    }
    Err(NodeError::new(
        "EEXIST",
        format!("EEXIST: mkdtemp could not create a unique directory for `{prefix}`"),
    ))
}

/// Six random lowercase-alphanumeric characters (Node's alphabet is a
/// superset; six characters is the observable contract).
fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut entropy = [0u8; 6];
    if getrandom::fill(&mut entropy).is_err() {
        // Never panic over a suffix: fall back to the clock's low bytes.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (index, byte) in entropy.iter_mut().enumerate() {
            *byte = (nanos >> (index * 8)) as u8;
        }
    }
    entropy
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(windows)]
fn symlink_impl(target: &str, path: &Path, kind: &str) -> std::io::Result<()> {
    // The junction kind approximates as a directory symlink; privilege
    // failures surface as the platform's own error.
    if kind == "dir" || kind == "junction" {
        std::os::windows::fs::symlink_dir(target, path)
    } else {
        std::os::windows::fs::symlink_file(target, path)
    }
}

#[cfg(unix)]
fn symlink_impl(target: &str, path: &Path, _kind: &str) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// `stat.mode`: libuv's value — the file-type bits plus whatever the platform
/// actually tracks. Windows has no POSIX permission bits; libuv reports `0o444`
/// for a read-only entry and `0o666` otherwise (`S_IREAD|S_IWRITE`), which is
/// exactly the bit pair `chmod`/`fchmod` toggle there.
#[cfg(windows)]
fn metadata_mode(meta: &std::fs::Metadata) -> u32 {
    let kind = if meta.is_dir() { 0o040000 } else { 0o100000 };
    let perm = if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    };
    kind | perm
}

#[cfg(unix)]
fn metadata_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode()
}

/// Node-parity stat value shared by `stat`/`lstat`.
fn metadata_value(meta: &std::fs::Metadata, symlink: bool) -> JsonValue {
    let millis = |time: std::io::Result<std::time::SystemTime>| -> f64 {
        time.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    };
    serde_json::json!({
        "isFile": meta.is_file(),
        "isDirectory": meta.is_dir(),
        "isSymbolicLink": symlink,
        "mode": metadata_mode(meta),
        "size": meta.len(),
        "mtimeMs": millis(meta.modified()),
        "birthtimeMs": millis(meta.created()),
    })
}

/// `fs.rm` core: symlinks are removed as links (never followed), missing
/// targets are a success under `force`.
fn rm_at(path: &Path, recursive: bool, force: bool) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if force && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    }
}

/// `fs.access` core. F_OK is existence; R_OK for files is an open probe;
/// W_OK for files is an open-for-write probe and for directories the readonly
/// attribute; X_OK degrades to existence (no portable execute check).
fn access_check(path: &Path, mode: u32) -> std::io::Result<()> {
    const W_OK: u32 = 2;
    const R_OK: u32 = 4;
    let meta = std::fs::metadata(path)?;
    if mode & R_OK != 0 && !meta.is_dir() {
        std::fs::File::open(path)?;
    }
    if mode & W_OK != 0 {
        if meta.is_dir() {
            if meta.permissions().readonly() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ));
            }
        } else {
            std::fs::OpenOptions::new().write(true).open(path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// process / os
// ---------------------------------------------------------------------------

fn process_op(state: &Arc<StdMutex<RuntimeState>>, op: &str, _payload: &JsonValue) -> NodeResult {
    match op {
        "env" => Ok(env_snapshot()),
        "cwd" => Ok(JsonValue::String(
            lock_state(state).config.cwd.display().to_string(),
        )),
        "pid" => Ok(serde_json::json!(std::process::id())),
        "cpu_usage" => Ok(cpu_usage_snapshot()),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown process op `{other}`"),
        )),
    }
}

fn os_op(op: &str) -> NodeResult {
    match op {
        "env" => Ok(env_snapshot()),
        "homedir" => Ok(JsonValue::String(homedir())),
        "tmpdir" => Ok(JsonValue::String(trim_trailing_separators(
            &std::env::temp_dir(),
        ))),
        "hostname" => Ok(JsonValue::String(hostname())),
        "cpus_count" => Ok(serde_json::json!(std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1))),
        other => Err(NodeError::new("ENOSYS", format!("unknown os op `{other}`"))),
    }
}

fn env_snapshot() -> JsonValue {
    let mut map = serde_json::Map::new();
    for (key, value) in std::env::vars_os() {
        // Lossy conversion keeps non-UTF-8 entries visible instead of panicking.
        map.insert(
            key.to_string_lossy().into_owned(),
            JsonValue::String(value.to_string_lossy().into_owned()),
        );
    }
    JsonValue::Object(map)
}

/// `process.cpuUsage()` raw counters: `{user, system}` in microseconds from
/// the same source libuv's `uv_getrusage` reads (GetProcessTimes on Windows,
/// `getrusage(RUSAGE_SELF)` elsewhere). Both counters are monotonic — the
/// vendored official test asserts they never decrease and that a
/// `prevValue` diff is `>= 0` — and never negative, so the JS-side
/// `prevValue` validation can subtract without clamping.
fn cpu_usage_snapshot() -> JsonValue {
    let (user, system) = process_cpu_micros();
    serde_json::json!({ "user": user, "system": system })
}

/// Windows process CPU time: `GetProcessTimes` on the current-process
/// pseudo-handle. FILETIME counts 100 ns ticks, so microseconds are
/// ticks / 10 (the same unit conversion libuv does). A failing call (a
/// pseudo-handle never fails in practice) degrades to zeros rather than
/// breaking `process.cpuUsage`.
#[cfg(windows)]
fn process_cpu_micros() -> (u64, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn micros(time: &FILETIME) -> u64 {
        ((time.dwHighDateTime as u64) << 32 | time.dwLowDateTime as u64) / 10
    }

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: the process pseudo-handle is always valid, the four FILETIME
    // locals are live out-parameters, and the call touches no other memory.
    #[expect(
        unsafe_code,
        reason = "GetProcessTimes on the current-process pseudo-handle"
    )]
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return (0, 0);
    }
    (micros(&user), micros(&kernel))
}

/// Unix process CPU time: `getrusage(RUSAGE_SELF)` user/system `timeval`s
/// (Node/libuv's own source on this platform).
#[cfg(unix)]
fn process_cpu_micros() -> (u64, u64) {
    fn micros(time: libc::timeval) -> u64 {
        (time.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(time.tv_usec as u64)
    }

    // SAFETY: `libc::rusage` is plain old data; its all-zero bit pattern is
    // valid, and `getrusage` overwrites it on success.
    #[expect(unsafe_code, reason = "zeroed rusage out-parameter for getrusage")]
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `RUSAGE_SELF` asks the kernel for this process's own counters;
    // `usage` is a live out-parameter owned by this frame.
    #[expect(unsafe_code, reason = "getrusage(RUSAGE_SELF) reads our own counters")]
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return (0, 0);
    }
    (micros(usage.ru_utime), micros(usage.ru_stime))
}

/// No CPU-time source outside Windows/Unix: honest zeros (they stay
/// non-negative and finite, so the surface keeps its shape).
#[cfg(not(any(windows, unix)))]
fn process_cpu_micros() -> (u64, u64) {
    (0, 0)
}

fn homedir() -> String {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let drive = std::env::var("HOMEDRIVE").ok()?;
                let path = std::env::var("HOMEPATH").ok()?;
                Some(format!("{drive}{path}"))
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").unwrap_or_default()
    }
}

fn hostname() -> String {
    let var = if cfg!(windows) {
        "COMPUTERNAME"
    } else {
        "HOSTNAME"
    };
    std::env::var(var)
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// Node's `os.tmpdir()` has no trailing separator; keep a bare root intact.
fn trim_trailing_separators(path: &Path) -> String {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        text.into_owned()
    } else {
        trimmed.to_owned()
    }
}

// ---------------------------------------------------------------------------
// dns
// ---------------------------------------------------------------------------

/// `dns` op table: `lookup` runs the OS resolver synchronously
/// (`ToSocketAddrs` on `host:0`) and returns every address with its Node
/// family number; a resolution that yields nothing is ENOTFOUND.
fn dns_op(op: &str, payload: &JsonValue) -> NodeResult {
    match op {
        "lookup" => {
            let host = payload_text(payload, "host")?;
            let query = format!("{host}:0");
            let addresses: Vec<JsonValue> = match query.to_socket_addrs() {
                Ok(iter) => iter
                    .map(|addr| {
                        serde_json::json!({
                            "address": addr.ip().to_string(),
                            "family": if addr.is_ipv4() { 4 } else { 6 },
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            if addresses.is_empty() {
                return Err(NodeError {
                    code: "ENOTFOUND",
                    message: format!("getaddrinfo ENOTFOUND {host}"),
                    syscall: Some("getaddrinfo"),
                    path: None,
                    // libuv `UV__EAI_NONAME`; the EAI block is identical on
                    // Windows and POSIX (node reports the same -3008).
                    errno: Some(-3008),
                });
            }
            Ok(serde_json::json!({ "addresses": addresses }))
        }
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown dns op `{other}`"),
        )),
    }
}

// ---------------------------------------------------------------------------
// npm (require / createRequire)
// ---------------------------------------------------------------------------

/// `npm` op table: `resolve` (bare/relative/absolute/builtin classification
/// via [`npm_resolve`]) and `readSource` (CJS/JSON source text for the CJS
/// runtime, root-checked: module-loading reads keep the stricter fence).
fn npm_op(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    op: &str,
    payload: &JsonValue,
) -> NodeResult {
    match op {
        "resolve" => npm_resolve_op(payload),
        "readSource" => npm_read_source_op(state, allowed_roots, payload),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown npm op `{other}`"),
        )),
    }
}

fn npm_resolve_op(payload: &JsonValue) -> NodeResult {
    use crate::extensions::api::node_shims::canonical_node_name;
    use crate::extensions::loader::npm_resolve::{
        module_kind, resolve_bare_searching, resolve_path_specifier, ModuleKind, ResolveMode,
    };

    let dir = PathBuf::from(payload_text(payload, "dir")?);
    let spec = payload_text(payload, "spec")?;
    let mode = match payload.get("mode").and_then(JsonValue::as_str) {
        Some("require") => ResolveMode::Require,
        _ => ResolveMode::Esm,
    };
    // Builtins win over node_modules for both spellings (Node precedence).
    if let Some(canonical) = canonical_node_name(spec) {
        let name = canonical.trim_start_matches("@node/");
        return Ok(serde_json::json!({ "kind": "builtin", "name": name }));
    }
    // Bare-package virtual shims (better-sqlite3): a real npm package sits in
    // node_modules, but its native binding cannot load in this runtime — the
    // virtual shim wins so `require("better-sqlite3")` resolves like a
    // builtin and hands back the host-backed JS implementation.
    if let Some(alias) = crate::extensions::api::shims::canonical_bare_package_alias(spec) {
        return Ok(serde_json::json!({ "kind": "builtin", "name": alias }));
    }
    // `node:` outside the shimmed table keeps the scan-stage wording.
    if spec.starts_with("node:") {
        return Ok(serde_json::json!({
            "kind": "error",
            "code": "ERR_UNKNOWN_BUILTIN_MODULE",
            "message": format!(
                "unsupported dependency `{spec}`: npm/node modules are not available in this runtime"
            ),
        }));
    }
    let resolved =
        if spec.starts_with('.') || spec.starts_with('/') || Path::new(spec).is_absolute() {
            resolve_path_specifier(&dir, spec)
        } else {
            // The search roots cover standalone-script requires (`pir js`):
            // a script has no extension directory to derive an npm root
            // from, so the user npm root backs the walk-up (NODE_PATH
            // fallback semantics, same as the loader's ESM path).
            resolve_bare_searching(
                &dir,
                &crate::extensions::loader::bare_search_roots(),
                spec,
                mode,
            )
            .map(|resolved| resolved.path)
        };
    let path = match resolved {
        Ok(path) => path,
        Err(error) => return Ok(resolve_error_value(&error)),
    };
    let kind = module_kind(&path);
    if kind == ModuleKind::Native {
        return Ok(serde_json::json!({
            "kind": "error",
            "code": "ERR_DLOPEN_DISABLED",
            "message": "native addons (.node) are not supported in this runtime",
        }));
    }
    Ok(serde_json::json!({
        "kind": "file",
        "path": path.to_string_lossy(),
        "moduleKind": match kind {
            ModuleKind::Esm => "esm",
            ModuleKind::Cjs => "cjs",
            ModuleKind::Json => "json",
            ModuleKind::Native => "native",
        },
    }))
}

/// Map a resolve failure onto the `{kind}` value the JS layer consumes:
/// not-found shapes are "missing" (MODULE_NOT_FOUND), everything else is a
/// coded "error".
fn resolve_error_value(error: &crate::extensions::loader::npm_resolve::ResolveError) -> JsonValue {
    use crate::extensions::loader::npm_resolve::ResolveError;
    let kind = match error {
        ResolveError::NotFound { .. } | ResolveError::ModuleNotFound { .. } => "missing",
        _ => "error",
    };
    serde_json::json!({
        "kind": kind,
        "code": error.code(),
        "message": error.to_string(),
    })
}

fn npm_read_source_op(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
) -> NodeResult {
    use crate::extensions::loader::npm_resolve::{module_kind, ModuleKind};

    let path = checked_path(state, allowed_roots, payload, "path", "open")?;
    let display = path.to_string_lossy().into_owned();
    match module_kind(&path) {
        // Without `asCjs` an ESM source stays on the loader's native async
        // pipeline (the empty source is a marker the JS layer never
        // dereferences). With `asCjs` (the require(esm) path) the source is
        // TS-stripped and rewritten to a CommonJS body by `esm_cjs`, so the
        // synchronous CJS wrapper can evaluate it like Node 22+ does.
        ModuleKind::Esm => {
            if !payload_bool(payload, "asCjs") {
                return Ok(serde_json::json!({ "kind": "esm", "source": "" }));
            }
            let source = std::fs::read_to_string(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            let js = crate::extensions::transpile::transpile(&display, &source)
                .map_err(|error| NodeError::new("ERR_INVALID_SYNTAX", error))?;
            let cjs = crate::extensions::esm_cjs::esm_to_cjs(&display, &js)
                .map_err(|error| NodeError::new(error.code, error.message))?;
            Ok(serde_json::json!({ "kind": "esm", "source": cjs }))
        }
        ModuleKind::Json => {
            let source = std::fs::read_to_string(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            Ok(serde_json::json!({ "kind": "json", "source": source }))
        }
        ModuleKind::Cjs => {
            let source = std::fs::read_to_string(&path)
                .map_err(|error| NodeError::io(&error, "open", &path))?;
            // TS-strip first: CJS files may still carry modern/TS syntax.
            let js = crate::extensions::transpile::transpile(&display, &source)
                .map_err(|error| NodeError::new("ERR_INVALID_SYNTAX", error))?;
            Ok(serde_json::json!({ "kind": "cjs", "source": js }))
        }
        ModuleKind::Native => Err(NodeError::new(
            "ERR_DLOPEN_DISABLED",
            "native addons (.node) are not supported in this runtime",
        )),
    }
}

// ---------------------------------------------------------------------------
// crypto
// ---------------------------------------------------------------------------

fn crypto_op(op: &str, payload: &JsonValue) -> NodeResult {
    match op {
        "hash" => {
            let algo = payload_text(payload, "algo")?;
            let data = decode_base64(payload_text(payload, "data")?)?;
            let digest = hash_bytes(algo, &data)?;
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
            // 1 GiB cap: an extension asking for more is a bug, not a request.
            if count > (1 << 30) {
                return Err(NodeError::new("EINVAL", "byte count out of range"));
            }
            let mut buffer = vec![0u8; count as usize];
            getrandom::fill(&mut buffer).map_err(|error| {
                NodeError::new("EIO", format!("entropy source failed: {error}"))
            })?;
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

fn hash_bytes(algo: &str, data: &[u8]) -> Result<Vec<u8>, NodeError> {
    // Tolerate WebCrypto's hyphenated spelling ("sha-256").
    let normalized: String = algo.chars().filter(|c| *c != '-').collect();
    Ok(match normalized.as_str() {
        "sha256" => Sha256::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        "sha1" => Sha1::digest(data).to_vec(),
        "md5" => md5::Md5::digest(data).to_vec(),
        _ => {
            return Err(NodeError::new(
                "EINVAL",
                format!("unsupported hash algorithm `{algo}`"),
            ))
        }
    })
}

fn hmac_bytes(algo: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>, NodeError> {
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
    let normalized: String = algo.chars().filter(|c| *c != '-').collect();
    let digest = match normalized.as_str() {
        "sha256" => digest!(Sha256),
        "sha512" => digest!(Sha512),
        "sha1" => digest!(Sha1),
        "md5" => digest!(md5::Md5),
        _ => {
            return Err(NodeError::new(
                "EINVAL",
                format!("unsupported hmac algorithm `{algo}`"),
            ))
        }
    };
    Ok(digest)
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
    let normalized: String = algo.chars().filter(|c| *c != '-').collect();
    match normalized.as_str() {
        "sha256" => pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, out),
        "sha512" => pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iterations, out),
        "sha1" => pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, iterations, out),
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

fn zlib_raw_op(op: &str, payload: &JsonValue) -> NodeResult {
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
    let lowered = alg.to_ascii_lowercase();
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
    let params =
        scrypt::Params::new(log_n as u8, r as u32, p as u32, keylen as usize).map_err(|error| {
            NodeError::new(
                "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
                format!("Invalid scrypt params: {error}"),
            )
        })?;
    let mut out = vec![0u8; keylen as usize];
    scrypt::scrypt(&password, &salt, &params, &mut out).map_err(|error| {
        NodeError::new(
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            format!("Invalid scrypt params: {error}"),
        )
    })?;
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
    let normalized: String = alg
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c: char| c.to_ascii_lowercase())
        .collect();
    let digest = normalized.strip_prefix("rsa").unwrap_or(&normalized);
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
