//! One Node-style error surface for the whole `pir-node` crate.
//!
//! All module implementations (fs/crypto/child/http/worker/...) produce this
//! type; the pir-side C-ABI glue converts it to a live JS object with the same
//! `{ok:false, code, message, syscall?, path?, errno?}` shape the legacy JSON
//! envelope used, so `node_call_helper.js.txt` reconstructs `Error` verbatim.

use std::path::Path;

use serde_json::{json, Map, Value as JsonValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeError {
    pub code: String,
    pub message: String,
    pub syscall: Option<String>,
    pub path: Option<String>,
    /// Node's `err.errno`: a negated libuv constant on Windows, a negated OS
    /// errno on POSIX. Absent for validation/`ERR_*` failures (Node leaves it
    /// off there too).
    pub errno: Option<i32>,
}

pub type NodeResult<T = JsonValue> = Result<T, NodeError>;

impl NodeError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let errno = code_errno(&code);
        Self {
            code,
            message: message.into(),
            syscall: None,
            path: None,
            errno,
        }
    }

    /// Sandbox violation: write target outside every allowed root.
    pub fn denied(syscall: &str, path: &str) -> Self {
        Self {
            code: "EACCES".to_owned(),
            message: format!("EACCES: permission denied, {syscall} '{path}'"),
            syscall: Some(syscall.to_owned()),
            path: Some(path.to_owned()),
            errno: code_errno("EACCES"),
        }
    }

    /// A real I/O failure. The message uses Node's `uv_strerror` text (not
    /// `std::io::Error`'s locale-dependent Display), matching upstream.
    pub fn io(error: &std::io::Error, syscall: &str, path: &Path) -> Self {
        let code = io_error_code(error);
        let path_text = path.to_string_lossy().into_owned();
        let message = match code_message(code) {
            Some(text) => format!("{code}: {text}, {syscall} '{path_text}'"),
            None => format!("{code}: {error}, {syscall} '{path_text}'"),
        };
        Self {
            code: code.to_owned(),
            message,
            syscall: Some(syscall.to_owned()),
            path: Some(path_text),
            errno: io_errno(error, code),
        }
    }

    /// The JSON envelope the JS helper consumes.
    pub fn to_json_value(&self) -> JsonValue {
        // Six possible keys (`ok`, `code`, `message`, `syscall`, `path`,
        // `errno`); sizing the map once avoids its growth reallocations.
        let mut out = Map::with_capacity(6);
        out.insert("ok".into(), JsonValue::Bool(false));
        out.insert("code".into(), JsonValue::String(self.code.clone()));
        out.insert("message".into(), JsonValue::String(self.message.clone()));
        if let Some(syscall) = &self.syscall {
            out.insert("syscall".into(), JsonValue::String(syscall.clone()));
        }
        if let Some(path) = &self.path {
            out.insert("path".into(), JsonValue::String(path.clone()));
        }
        if let Some(errno) = self.errno {
            out.insert("errno".into(), json!(errno));
        }
        JsonValue::Object(out)
    }

    /// Success envelope, the mirror of `to_json_value`.
    pub fn ok(value: JsonValue) -> JsonValue {
        // Every successful host op builds this two-key map; size it once.
        let mut out = Map::with_capacity(2);
        out.insert("ok".into(), JsonValue::Bool(true));
        out.insert("value".into(), value);
        JsonValue::Object(out)
    }
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NodeError {}

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
        ErrorKind::InvalidInput => "EINVAL",
        ErrorKind::TimedOut => "ETIMEDOUT",
        ErrorKind::Interrupted => "EINTR",
        ErrorKind::ConnectionRefused => "ECONNREFUSED",
        ErrorKind::ConnectionReset => "ECONNRESET",
        ErrorKind::ConnectionAborted => "ECONNABORTED",
        ErrorKind::NotConnected | ErrorKind::BrokenPipe => "EPIPE",
        ErrorKind::AddrInUse => "EADDRINUSE",
        ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        ErrorKind::WouldBlock => "EAGAIN",
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

#[cfg(windows)]
pub(crate) fn code_errno(code: &str) -> Option<i32> {
    // Verbatim from libuv's `_WIN32` errno table (the subset pir surfaces).
    Some(match code {
        "E2BIG" => -4093,
        "EACCES" => -4092,
        "EADDRINUSE" => -4091,
        "EADDRNOTAVAIL" => -4090,
        "EAFNOSUPPORT" => -4089,
        "EAGAIN" => -4088,
        "EBADF" => -4083,
        "EBUSY" => -4082,
        "ECANCELED" => -4081,
        "ECONNABORTED" => -4079,
        "ECONNREFUSED" => -4078,
        "ECONNRESET" => -4077,
        "EEXIST" => -4075,
        "EHOSTUNREACH" => -4073,
        "EINTR" => -4072,
        "EINVAL" => -4071,
        "EIO" => -4070,
        "EISDIR" => -4068,
        "ENOENT" => -4058,
        "ENOMEM" => -4055,
        "ENOSPC" => -4053,
        "ENOTDIR" => -4052,
        "EPIPE" => -4047,
        "EROFS" => -4044,
        "ETIMEDOUT" => -4038,
        "ENOTEMPTY" => -4060,
        "ELOOP" => -4054,
        "ENAMETOOLONG" => -4051,
        _ => return None,
    })
}

#[cfg(not(windows))]
pub(crate) fn code_errno(code: &str) -> Option<i32> {
    // POSIX: Node reports the negated libc errno.
    Some(match code {
        "E2BIG" => -7,
        "EACCES" => -13,
        "EADDRINUSE" => -98,
        "EADDRNOTAVAIL" => -99,
        "EAFNOSUPPORT" => -97,
        "EAGAIN" => -11,
        "EBADF" => -9,
        "EBUSY" => -16,
        "ECANCELED" => -125,
        "ECONNABORTED" => -103,
        "ECONNREFUSED" => -111,
        "ECONNRESET" => -104,
        "EEXIST" => -17,
        "EHOSTUNREACH" => -113,
        "EINTR" => -4,
        "EINVAL" => -22,
        "EIO" => -5,
        "EISDIR" => -21,
        "ENOENT" => -2,
        "ENOMEM" => -12,
        "ENOSPC" => -28,
        "ENOTDIR" => -20,
        "EPIPE" => -32,
        "EROFS" => -30,
        "ETIMEDOUT" => -110,
        "ENOTEMPTY" => -39,
        "ELOOP" => -40,
        "ENAMETOOLONG" => -36,
        _ => return None,
    })
}

fn io_errno(error: &std::io::Error, code: &str) -> Option<i32> {
    // On Windows the raw OS code is a Win32 error (145, 1314, …), not a libuv
    // errno; Node reports the libuv constant from its own table there.
    match error.raw_os_error() {
        Some(raw) if !cfg!(windows) => Some(-raw),
        _ => code_errno(code),
    }
}
