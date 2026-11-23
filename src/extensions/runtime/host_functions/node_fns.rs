//! Host glue for the legacy `__pir_host.node(module, op, payloadJson)` string
//! entry used by the shims' async pumps, plus the two pieces of the Node
//! surface pir still owns: npm resolution / module-source transpilation
//! (`NodeHooks::npm_lookup`/`npm_source`) and the sandbox root computation
//! the loader hands to `pir-node`.
//!
//! Every Node builtin op itself lives in `crates/pir-node`: this module only
//! parses the JSON payload once and forwards it, so there is exactly one
//! implementation of each module. The `NodeError` shape and the path-fence
//! helpers below stay here because the npm hook and `pir-node`'s host
//! callbacks share them.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use rquickjs::Object;

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

/// Install the legacy string entry `__pir_host.node(module, op, payloadJson)`.
/// Only the wire lives here; [`super::node_string_call`] dispatches into
/// `pir-node`, which owns every implementation.
pub(super) fn register_node_host_fns(
    host: &Object,
    runtime: &Arc<crate::pir_node::NodeRuntime>,
) -> rquickjs::Result<()> {
    let runtime = runtime.clone();
    host.set(
        "node",
        Func::from(
            move |module: String, op: String, payload: String| -> String {
                super::node_string_call(&runtime, &module, &op, &payload)
            },
        ),
    )?;
    Ok(())
}

/// Borrowing payload accessor for the fields that are only read: the text
/// stays inside the parsed payload instead of being copied into an owned
/// `String` that is dropped right after use.
fn payload_text<'a>(payload: &'a JsonValue, key: &str) -> Result<&'a str, NodeError> {
    payload
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing string payload field `{key}`")))
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
pub(super) fn read_file_capped(path: &Path) -> Result<Vec<u8>, NodeError> {
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

/// Read a whole UTF-8 file under the same [`MAX_READ_FILE_BYTES`] bound as
/// [`read_file_capped`]. Module sources (`require`) and Worker entry files go
/// through this helper so they cannot bypass the `fs.readFile` cap and
/// materialize an unbounded source string on the host heap. Invalid UTF-8 is
/// `EINVAL`, matching Node's module loader.
pub(super) fn read_text_capped(
    path: &Path,
    syscall: &str,
    display: &str,
) -> Result<String, NodeError> {
    let bytes = read_file_capped(path)?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(_) => Err(NodeError::new(
            "EINVAL",
            format!("EINVAL: invalid UTF-8, {syscall} '{display}'"),
        )),
    }
}

/// Convert this table's error into the `pir-node` error type (same fields,
/// owned).
pub(super) fn into_pir_node_error(error: NodeError) -> crate::pir_node::NodeError {
    let mut out = crate::pir_node::NodeError::new(error.code, error.message);
    out.syscall = error.syscall.map(str::to_owned);
    out.path = error.path;
    out.errno = error.errno;
    out
}


pub(super) fn npm_resolve_op(payload: &JsonValue) -> NodeResult {
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

/// Read + transpile one already-fenced module source for the `__pirNode` npm
/// hook.
pub(super) fn npm_source_value(path: &Path, as_cjs: bool) -> NodeResult {
    use crate::extensions::loader::npm_resolve::{module_kind, ModuleKind};

    let display = path.to_string_lossy().into_owned();
    match module_kind(path) {
        // Without `asCjs` an ESM source stays on the loader's native async
        // pipeline (the empty source is a marker the JS layer never
        // dereferences). With `asCjs` (the require(esm) path) the source is
        // TS-stripped and rewritten to a CommonJS body by `esm_cjs`, so the
        // synchronous CJS wrapper can evaluate it like Node 22+ does.
        ModuleKind::Esm => {
            if !as_cjs {
                return Ok(serde_json::json!({ "kind": "esm", "source": "" }));
            }
            let source = read_text_capped(path, "open", &display)?;
            let js = crate::extensions::transpile::transpile(&display, &source)
                .map_err(|error| NodeError::new("ERR_INVALID_SYNTAX", error))?;
            let cjs = crate::extensions::esm_cjs::esm_to_cjs(&display, &js)
                .map_err(|error| NodeError::new(error.code, error.message))?;
            Ok(serde_json::json!({ "kind": "esm", "source": cjs }))
        }
        ModuleKind::Json => {
            let source = read_text_capped(path, "open", &display)?;
            Ok(serde_json::json!({ "kind": "json", "source": source }))
        }
        ModuleKind::Cjs => {
            let source = read_text_capped(path, "open", &display)?;
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

