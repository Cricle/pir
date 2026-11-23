//! Node `fs` module (ported from `node_fns.rs`).
//!
//! Reads run globally (the child-process surface already grants full read
//! capability); writes, write-capable opens and mixed-op targets stay fenced
//! to the effective roots through [`crate::pir_node::NodeRuntime::effective_roots`].

use std::borrow::Cow;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{MutexGuard, PoisonError};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value as JsonValue;

use crate::pir_node::error::{code_errno, NodeError, NodeResult};
use crate::pir_node::fd::{FdTable, MAX_OPEN_FDS};

/// Lock the runtime descriptor table, recovering from a poisoned lock (a
/// panicking op must not wedge every later fs call).
fn fd_guard(runtime: &crate::pir_node::NodeRuntime) -> MutexGuard<'_, FdTable> {
    runtime
        .fd_table
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Required payload string field, with the legacy (pre-migration) message so
/// fs error text stays byte-identical.
fn payload_text<'a>(payload: &'a JsonValue, key: &str) -> Result<&'a str, NodeError> {
    payload
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing string payload field `{key}`")))
}

fn decode_base64(data: &str) -> Result<Vec<u8>, NodeError> {
    STANDARD
        .decode(data.as_bytes())
        .map_err(|error| NodeError::new("EINVAL", format!("invalid base64 payload: {error}")))
}

fn payload_bool(payload: &JsonValue, key: &str) -> bool {
    payload
        .get(key)
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

/// `OsString` -> `String` without the extra copy `to_string_lossy().into_owned()`
/// performs for names that are already valid Unicode (the common case). The
/// invalid-Unicode fallback is byte-for-byte the same lossy replacement.
fn os_string_lossy(text: std::ffi::OsString) -> String {
    match text.into_string() {
        Ok(text) => text,
        Err(text) => text.to_string_lossy().into_owned(),
    }
}

/// Node's `err.errno` for a code on this platform.
///
/// The crate-level [`crate::pir_node::error::code_errno`] table predates Node 24 and has
/// several drifted Windows constants (`ENOTEMPTY`, `ELOOP`, `ENOMEM`, …), so
/// the fs surface pins the values `util.getSystemErrorMap()` reports on
/// win32 (libuv's `uv_translate_sys_error` table) and falls back to the shared
/// table for anything else. On POSIX the shared table is already exact.
fn fs_errno(code: &str) -> Option<i32> {
    #[cfg(windows)]
    {
        if let Some(errno) = match code {
            "E2BIG" => Some(-4093),
            "EACCES" => Some(-4092),
            "EBADF" => Some(-4083),
            "EBUSY" => Some(-4082),
            "EEXIST" => Some(-4075),
            "EINVAL" => Some(-4071),
            "EIO" => Some(-4070),
            "EISDIR" => Some(-4068),
            "ELOOP" => Some(-4067),
            "EMFILE" => Some(-4066),
            "ENAMETOOLONG" => Some(-4064),
            "ENOENT" => Some(-4058),
            "ENOMEM" => Some(-4057),
            "ENOSPC" => Some(-4055),
            "ENOSYS" => Some(-4054),
            "ENOTDIR" => Some(-4052),
            "ENOTEMPTY" => Some(-4051),
            "ENOTSUP" => Some(-4049),
            "EPERM" => Some(-4048),
            "EPIPE" => Some(-4047),
            "EROFS" => Some(-4043),
            "EXDEV" => Some(-4037),
            _ => None,
        } {
            return Some(errno);
        }
    }
    code_errno(code)
}

/// [`NodeError::io`] with the Node-24 errno table above.
fn fs_io(error: &std::io::Error, syscall: &str, path: &Path) -> NodeError {
    let mut mapped = NodeError::io(error, syscall, path);
    if let Some(errno) = fs_errno(&mapped.code) {
        mapped.errno = Some(errno);
    }
    mapped
}

/// A hand-shaped OS error whose message carries extra context (Node appends
/// `-> '<dest>'` to `rename`/`copyfile` failures).
fn io_message(code: &str, message: String, syscall: &str, path: &Path) -> NodeError {
    let mut error = NodeError::new(code, message);
    error.syscall = Some(syscall.to_owned());
    error.path = Some(path.to_string_lossy().into_owned());
    error.errno = fs_errno(code);
    error
}

// ---------------------------------------------------------------------------
// Path resolution + write sandbox
// ---------------------------------------------------------------------------

/// Effective sandbox roots: the loader allow-list plus the agent directory,
/// the pi config root (`~/.pi`, extension data dirs live at `~/.pi/<ext>`),
/// the OS temp directory and the runtime cwd, each in check-normal form.
fn effective_roots(runtime: &crate::pir_node::NodeRuntime) -> Vec<PathBuf> {
    runtime.effective_roots()
}
/// Canonicalize when the path exists; otherwise canonicalize the deepest
/// existing parent and append the remainder lexically, so a symlinked
/// directory inside an allowed root cannot smuggle a target outside it.
pub(crate) fn normalize_for_check(path: &Path) -> PathBuf {
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
    let absolute = path.is_absolute();
    let mut out = PathBuf::new();
    // True while the last meaningful component is a root/prefix: `..` cannot
    // climb above it (GetFullPathNameW drops it, as does Node).
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if rooted {
                    continue;
                }
                let at_start_or_parent = matches!(
                    out.components().next_back(),
                    None | Some(Component::ParentDir)
                );
                if at_start_or_parent {
                    // A relative path keeps leading `..`; an absolute one has
                    // a root, so this cannot happen there.
                    if !absolute {
                        out.push("..");
                    }
                } else {
                    out.pop();
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                out.push(component.as_os_str());
                rooted = true;
            }
            other => {
                out.push(other.as_os_str());
                rooted = false;
            }
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
///
/// Streams both component iterators instead of materializing two
/// `Vec<String>` per root per fenced call.
pub(crate) fn path_within(candidate: &Path, root: &Path) -> bool {
    let mut candidate_components = candidate.components();
    let mut matched = 0usize;
    for root_component in root.components() {
        match candidate_components.next() {
            Some(candidate_component) => {
                if !component_eq(candidate_component, root_component) {
                    return false;
                }
                matched += 1;
            }
            None => return false,
        }
    }
    matched > 0
}

/// Component equality with the old `component_key` semantics: lossy text,
/// lowercased on Windows. Comparing the lowercased character streams is
/// exactly what comparing two `str::to_lowercase()` strings did, without
/// building either string.
fn component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    let left = left.as_os_str().to_string_lossy();
    let right = right.as_os_str().to_string_lossy();
    if cfg!(windows) {
        left.chars()
            .flat_map(char::to_lowercase)
            .eq(right.chars().flat_map(char::to_lowercase))
    } else {
        left.as_ref() == right.as_ref()
    }
}

/// Resolve one payload path field and enforce the root fence (fs write
/// targets and the module-loading reads; runtime fs reads skip it). IO then
/// uses the original path text: `..`/links are resolved by the OS call itself.
fn checked_path(
    runtime: &crate::pir_node::NodeRuntime,
    payload: &JsonValue,
    key: &str,
    syscall: &'static str,
) -> Result<PathBuf, NodeError> {
    checked_path_raw(runtime, payload_text(payload, key)?, syscall, false)
}

/// Root-fence one already-extracted path; the shared body of [`checked_path`]
/// and the typed `fs.writeFileSync` byte fast path. `allow_symlink_leaf` is for
/// operations that only touch the directory ENTRY (unlink/rmdir/rename source/
/// lutimes): removing or retiming a link cannot write through it, so the
/// symlink-leaf rejection below must not apply.
fn checked_path_raw(
    runtime: &crate::pir_node::NodeRuntime,
    raw: &str,
    syscall: &'static str,
    allow_symlink_leaf: bool,
) -> Result<PathBuf, NodeError> {
    // Relative paths join the runtime cwd first (Node's process.cwd()
    // semantics): the raw form is neither inside any absolute root nor
    // meaningful on its own, so checking it unjoined would deny every
    // relative write.
    let path = resolve_fs_path(runtime, raw);
    // An empty path can never create anything; let the OS call answer with
    // Node's own `ENOENT ... open ''` instead of the fence's EACCES. Node
    // passes `""` straight to libuv (it does not join it onto the cwd).
    if raw.is_empty() {
        return Ok(path);
    }
    let checked = normalize_for_check(&path);
    let roots = effective_roots(runtime);
    if !roots.iter().any(|root| path_within(&checked, root)) {
        return Err(NodeError::denied(syscall, raw));
    }
    // A dangling symlink canonicalizes to its parent + name, so the root
    // check passes, but the subsequent open/write follows the link and can
    // create a file outside the sandbox. Reject any symlink leaf; the
    // canonical parent check above already covers symlinked directories.
    if !allow_symlink_leaf
        && std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(NodeError::denied(syscall, raw));
    }
    Ok(path)
}

/// Entry-only fence: same roots, but the leaf itself may be a reparse point
/// (unlink/rmdir/rename source/lutimes operate on the entry, never through it).
pub(crate) fn check_write_entry(
    runtime: &crate::pir_node::NodeRuntime,
    payload: &JsonValue,
    key: &str,
    syscall: &'static str,
) -> Result<PathBuf, NodeError> {
    checked_path_raw(runtime, payload_text(payload, key)?, syscall, true)
}

/// Fs write ops fence their target through `checked_path`; the distinct name
/// states the policy at the fs call sites (writes are fenced, reads are not).
pub(crate) fn check_write(
    runtime: &crate::pir_node::NodeRuntime,
    payload: &JsonValue,
    key: &str,
    syscall: &'static str,
) -> Result<PathBuf, NodeError> {
    checked_path(runtime, payload, key, syscall)
}

/// Resolve one payload path field for a read: runtime reads are global (the
/// child_process surface already grants full read capability), so no sandbox
/// check applies — but a relative path still resolves against the runtime
/// cwd, exactly like Node resolves fs paths against `process.cwd()`. The pir
/// process's own working directory is NOT the same thing (a tool session runs
/// against the project dir while pir.exe may have been launched elsewhere).
fn read_path(
    runtime: &crate::pir_node::NodeRuntime,
    payload: &JsonValue,
    key: &str,
) -> Result<PathBuf, NodeError> {
    Ok(resolve_fs_path(runtime, payload_text(payload, key)?))
}

/// Node fs semantics for relative paths: join them onto the runtime cwd.
/// Absolute paths (and Windows drive/UNC forms) pass through untouched.
///
/// On Windows libuv hands every path back from `GetFullPathNameW`, which
/// normalizes separators; Node therefore reports `dir\child` (never
/// `dir/child`) in `err.path`/`err.message` even when the caller used forward
/// slashes. The resolved path is normalized the same way here so both the OS
/// call and the error text see the Node spelling.
///
/// An empty path stays empty: Node passes `""` to libuv (which cannot make a
/// full path out of it) instead of resolving it to the cwd, so `statSync("")`
/// is `ENOENT ... stat ''` and `existsSync("")` is false.
fn resolve_fs_path(runtime: &crate::pir_node::NodeRuntime, raw: &str) -> PathBuf {
    if raw.is_empty() {
        return PathBuf::new();
    }
    let path = PathBuf::from(raw);
    let resolved = if path.is_absolute() {
        path
    } else {
        runtime.cwd().join(path)
    };
    // libuv's `GetFullPathNameW` collapses `.`/`..` segments lexically; Node's
    // error paths come from that full path, so `statSync(".")` reports the cwd
    // (not `cwd\.`).
    normalize_separators(lexical_normalize(&resolved))
}

/// Windows: `/` -> `\` (libuv's full-path normalization). Other platforms
/// keep the caller's separators (they have only one).
fn normalize_separators(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if text.contains('/') {
            return PathBuf::from(text.replace('/', "\\"));
        }
    }
    path
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
pub(crate) fn read_file_capped(path: &Path) -> Result<Vec<u8>, NodeError> {
    let file = std::fs::File::open(path).map_err(|error| read_file_error(&error, path))?;
    // Size hint from the opened descriptor (not the path) so a 1 MiB read does
    // not grow the buffer through ~15 reallocations; the `take(cap + 1)` bound
    // below still holds if the file grows after this point.
    let hint = file
        .metadata()
        .map(|meta| meta.len().min(MAX_READ_FILE_BYTES + 1) as usize)
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(hint);
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
pub(crate) fn read_text_capped(
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

/// Whole-file byte read for the typed `fs.readFileSync` fast path.
///
/// Same resolution and [`MAX_READ_FILE_BYTES`] bound as the `readFile` op; the
/// bytes stay bytes instead of taking a base64 lap through the JSON payload.
pub fn read_file(runtime: &crate::pir_node::NodeRuntime, raw: &str) -> Result<Vec<u8>, NodeError> {
    let path = resolve_fs_path(runtime, raw);
    // A file symlink created without SeCreateSymbolicLinkPrivilege is an
    // ADS-backed emulated link: an empty leaf file plus a `:pir_symlink`
    // stream holding the target. stat/readlink already follow it; a plain
    // File::open would read the empty leaf, so resolve it here too.
    #[cfg(windows)]
    let path = emulated_link_target(&path).unwrap_or(path);
    read_file_capped(&path)
}

/// Flags for one write: Node's normalized `stringToFlags` result, i.e. either
/// the flag string (`"wx"`, `"r+"`, …) or the int32 `fs.constants` mask.
pub enum WriteFlag<'a> {
    Text(&'a str),
    Bits(i64),
}

impl WriteFlag<'_> {
    /// Whether the descriptor can write. A read-only open (`"r"`, `"rs"`,
    /// `"sr"`, or access mode `O_RDONLY`) reaches the data write in Node and
    /// fails there with `EBADF: bad file descriptor, write`.
    fn write_access(&self) -> bool {
        match self {
            WriteFlag::Text(text) => !matches!(*text, "r" | "rs" | "sr"),
            WriteFlag::Bits(bits) => {
                let access = bits & O_ACCMODE;
                access == O_WRONLY || access == O_RDWR
            }
        }
    }

    /// `x` variants: fail `EEXIST` when the path already exists.
    fn exclusive(&self) -> bool {
        match self {
            WriteFlag::Text(text) => text.contains('x'),
            WriteFlag::Bits(bits) => (bits & O_CREAT) != 0 && (bits & O_EXCL) != 0,
        }
    }

    /// Flags that make libuv open with O_TRUNC (`w`/`w+`).
    fn truncates(&self) -> bool {
        match self {
            WriteFlag::Text(text) => matches!(*text, "w" | "w+"),
            WriteFlag::Bits(bits) => (bits & O_TRUNC) != 0,
        }
    }
}

/// Whole-file byte write for the typed `fs.writeFileSync`/`appendFileSync`
/// fast path: same fence as the `writeFile` op (`check_write`), bytes supplied
/// directly instead of base64-encoded in the payload.
pub fn write_file(
    runtime: &crate::pir_node::NodeRuntime,
    raw: &str,
    bytes: &[u8],
    append: bool,
) -> Result<(), NodeError> {
    let path = checked_path_raw(runtime, raw, "open", false)?;
    let flag = if append {
        WriteFlag::Text("a")
    } else {
        WriteFlag::Text("w")
    };
    write_at(&path, bytes, &flag, 0o666)
}

/// Flag-aware byte write for the typed `__pirFsWriteFlags` entry: every flag
/// Node accepts for `writeFileSync`/`appendFileSync` (`w`, `w+`, `wx`, `wx+`,
/// `r+`, `a`, `a+`, `ax`, `ax+`, the `s` sync hints and the numeric masks) is
/// mapped onto `OpenOptions` here, behind the same root fence.
pub fn write_file_flags(
    runtime: &crate::pir_node::NodeRuntime,
    raw: &str,
    bytes: &[u8],
    flag: &WriteFlag<'_>,
    mode: u32,
) -> Result<(), NodeError> {
    let path = checked_path_raw(runtime, raw, "open", false)?;
    write_at(&path, bytes, flag, mode)
}

/// Open with `flag`, then write the whole buffer.
///
/// `create_new` (the `x` variants) fails `EEXIST` without touching the file;
/// the read/write base never truncates (`r+` writes over the existing bytes),
/// while `w`/`w+` truncate and `a`/`a+` append.
///
/// A directory target mirrors Node's three distinct outcomes (verified against
/// node v24.22.1 win32): the `x` flags are `EEXIST` at open, `w`/`w+` are
/// `EISDIR` at open (libuv refuses the truncating open), and the append /
/// read-write opens succeed and fail the data write with `EISDIR` (no `path`).
/// A read-only flag opens and fails with `EBADF` at the write, like Node.
fn write_at(path: &Path, bytes: &[u8], flag: &WriteFlag<'_>, mode: u32) -> Result<(), NodeError> {
    let options = match flag {
        WriteFlag::Text(text) => open_options(text, mode)?.0,
        WriteFlag::Bits(bits) => open_options_bits(*bits, mode, path)?.0,
    };
    let ebadf = || NodeError {
        code: "EBADF".to_string(),
        message: "EBADF: bad file descriptor, write".to_string(),
        syscall: Some("write".to_string()),
        path: None,
        errno: fs_errno("EBADF"),
    };
    if path.is_dir() {
        let text = path.to_string_lossy().into_owned();
        if flag.exclusive() {
            return Err(NodeError {
                code: "EEXIST".to_string(),
                message: format!("EEXIST: file already exists, open '{text}'"),
                syscall: Some("open".to_string()),
                path: Some(text),
                errno: fs_errno("EEXIST"),
            });
        }
        if flag.truncates() {
            return Err(NodeError {
                code: "EISDIR".to_string(),
                message: format!("EISDIR: illegal operation on a directory, open '{text}'"),
                syscall: Some("open".to_string()),
                path: Some(text),
                errno: fs_errno("EISDIR"),
            });
        }
        if !flag.write_access() {
            return Err(ebadf());
        }
        return Err(NodeError {
            code: "EISDIR".to_string(),
            message: "EISDIR: illegal operation on a directory, write".to_string(),
            syscall: Some("write".to_string()),
            path: None,
            errno: fs_errno("EISDIR"),
        });
    }
    let mut file = options
        .open(path)
        .map_err(|error| fs_io(&error, "open", path))?;
    if !flag.write_access() {
        // Node opens successfully with O_RDONLY and only then fails the data
        // write: `EBADF: bad file descriptor, write` (no `path` field).
        return Err(ebadf());
    }
    file.write_all(bytes)
        .map_err(|error| fs_io(&error, "write", path))?;
    Ok(())
}

/// Structured `stat`/`lstat` result for the typed host entry.
///
/// Mirrors the `metadata_value` payload field-for-field so the JS `Stats`
/// constructor sees exactly what the JSON op would have produced, without
/// building a `serde_json` tree (or its string) per call.
///
/// The integer fields are the ones Node's `Stats`/`BigIntStats` expose; the
/// typed entry hands them over as BigInt so `ino`/`dev` (64-bit on Windows)
/// keep full precision in `{ bigint: true }` mode, while the JS side derives
/// the Number fields from them for the plain `Stats`.
pub struct StatParts {
    pub is_file: bool,
    pub is_directory: bool,
    pub is_symbolic_link: bool,
    pub mode: u32,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub birthtime_ns: i64,
}

impl StatParts {
    /// Node's Number `Stats` stores milliseconds as an f64 derived from the
    /// nanosecond timestamp, so a 100 ns Windows file time keeps a fractional
    /// `.206`-style tail.
    pub fn atime_ms(&self) -> f64 {
        self.atime_ns as f64 / 1_000_000.0
    }
    pub fn mtime_ms(&self) -> f64 {
        self.mtime_ns as f64 / 1_000_000.0
    }
    pub fn ctime_ms(&self) -> f64 {
        self.ctime_ns as f64 / 1_000_000.0
    }
    pub fn birthtime_ms(&self) -> f64 {
        self.birthtime_ns as f64 / 1_000_000.0
    }
}

/// `stat`/`lstat` for the typed `fs.statSync` fast path. Reads are not
/// root-fenced (same as the `stat` op, which uses `read_path`).
pub fn stat_parts(
    runtime: &crate::pir_node::NodeRuntime,
    raw: &str,
    symlink: bool,
) -> Result<StatParts, NodeError> {
    let path = resolve_fs_path(runtime, raw);
    let syscall = if symlink { "lstat" } else { "stat" };
    #[cfg(windows)]
    if let Some(target) = emulated_link_target(&path) {
        let meta = if symlink {
            std::fs::symlink_metadata(&path)
        } else {
            std::fs::metadata(&target)
        }
        .map_err(|error| fs_io(&error, syscall, &path))?;
        return Ok(stat_parts_of(&meta, symlink));
    }
    let meta = if symlink {
        std::fs::symlink_metadata(&path)
    } else {
        std::fs::metadata(&path)
    }
    .map_err(|error| fs_io(&error, syscall, &path))?;
    Ok(stat_parts_of(&meta, meta.file_type().is_symlink()))
}

/// One `readdir` entry for the typed `fs.readdirSync` fast path.
pub struct DirEntryParts {
    pub name: String,
    pub is_file: bool,
    pub is_directory: bool,
    pub is_symbolic_link: bool,
}

/// `readdir` for the typed `fs.readdirSync` fast path. `dirents` selects the
/// per-entry kind lookup; the plain form only needs the names.
pub fn readdir_parts(
    runtime: &crate::pir_node::NodeRuntime,
    raw: &str,
    dirents: bool,
) -> Result<Vec<DirEntryParts>, NodeError> {
    let path = resolve_fs_path(runtime, raw);
    let entries = std::fs::read_dir(&path).map_err(|error| fs_io(&error, "scandir", &path))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| fs_io(&error, "scandir", &path))?;
        let name = os_string_lossy(entry.file_name());
        let (is_file, is_directory, is_symbolic_link) = if dirents {
            let file_type = entry
                .file_type()
                .map_err(|error| fs_io(&error, "scandir", &path))?;
            (
                file_type.is_file(),
                file_type.is_dir(),
                file_type.is_symlink(),
            )
        } else {
            (false, false, false)
        };
        out.push(DirEntryParts {
            name,
            is_file,
            is_directory,
            is_symbolic_link,
        });
    }
    Ok(out)
}

/// `exists` for the typed `fs.existsSync` fast path (never errors: Node's
/// `existsSync` answers `false` for everything it cannot stat).
pub fn exists(runtime: &crate::pir_node::NodeRuntime, raw: &str) -> bool {
    let path = resolve_fs_path(runtime, raw);
    // A broken emulated file link is a real (marker) file on disk; Node's
    // exists follows the link, so probe the stored target instead.
    #[cfg(windows)]
    if let Some(target) = emulated_link_target(&path) {
        return target.exists();
    }
    path.exists()
}

/// The write family's flag: a `flag` payload field (string or int32 mask) when
/// the shim sends one, otherwise the legacy `append` boolean (`w`/`a`).
fn write_flag_from_payload<'a>(payload: &'a JsonValue) -> Result<WriteFlag<'a>, NodeError> {
    match payload.get("flag") {
        Some(JsonValue::String(text)) => Ok(WriteFlag::Text(text)),
        Some(value) if value.is_number() => value
            .as_i64()
            .filter(|bits| i32::try_from(*bits).is_ok())
            .map(WriteFlag::Bits)
            .ok_or_else(|| {
                NodeError::new(
                    "ERR_OUT_OF_RANGE",
                    format!(
                        "The value of \"flag\" is out of range. It must be >= -2147483648 && <= 2147483647. Received {value}"
                    ),
                )
            }),
        _ => Ok(if payload_bool(payload, "append") {
            WriteFlag::Text("a")
        } else {
            WriteFlag::Text("w")
        }),
    }
}

/// Write-data channel, preferred order: `dataText` (the exact UTF-8 image of
/// the bytes, sent by the JS side for plain-string writes — no base64 round
/// trip on either leg) then the base64 `data` (byte payloads: Buffer/typed
/// arrays, or any string that is not plain UTF-8 text). A non-string
/// `dataText` is ignored rather than rejected so the base64 channel stays the
/// ABI default.
///
/// `dataText` is borrowed straight out of the parsed payload: the previous
/// `Vec` copy existed only to hand `write_at` an owned slice.
fn payload_write_data(payload: &JsonValue) -> Result<Cow<'_, [u8]>, NodeError> {
    if let Some(text) = payload.get("dataText").and_then(JsonValue::as_str) {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }
    Ok(Cow::Owned(decode_base64(payload_text(payload, "data")?)?))
}

/// fs op table: reads run globally; writes, write-capable opens and mixed-op
/// targets stay fenced to the effective roots.
pub(crate) fn fs_op(
    runtime: &crate::pir_node::NodeRuntime,
    generation: u64,
    op: &str,
    payload: &JsonValue,
) -> NodeResult {
    match op {
        "readFile" => {
            let path = read_path(runtime, payload, "path")?;
            let bytes = read_file_capped(&path)?;
            Ok(JsonValue::String(STANDARD.encode(bytes)))
        }
        "writeFile" => {
            let path = check_write(runtime, payload, "path", "open")?;
            let bytes = payload_write_data(payload)?;
            let flag = write_flag_from_payload(payload)?;
            // Node's default creation mode is 0o666; only an explicit
            // `options.mode` overrides it.
            let mode = payload
                .get("mode")
                .and_then(JsonValue::as_u64)
                .map(|mode| mode as u32)
                .unwrap_or(0o666);
            write_at(&path, bytes.as_ref(), &flag, mode)?;
            Ok(JsonValue::Null)
        }
        "statfs" => {
            let path = read_path(runtime, payload, "path")?;
            #[cfg(windows)]
            {
                let (bsize, blocks, free) =
                    windows_disk_space(&path).map_err(|error| fs_io(&error, "statfs", &path))?;
                Ok(serde_json::json!({
                    "type": 0u64,
                    "bsize": bsize,
                    "frsize": bsize,
                    "blocks": blocks,
                    "bfree": free,
                    "bavail": free,
                    // libuv leaves the inode counts zero on Windows.
                    "files": 0u64,
                    "ffree": 0u64,
                }))
            }
            #[cfg(not(windows))]
            {
                Err(NodeError::new(
                    "ENOSYS",
                    "statfs is not implemented on this platform",
                ))
            }
        }
        "stat" | "lstat" => {
            let symlink = op == "lstat";
            let syscall = if symlink { "lstat" } else { "stat" };
            let path = read_path(runtime, payload, "path")?;
            // Unprivileged file-symlink emulation: lstat is the marker file
            // itself, stat follows the recorded target.
            #[cfg(windows)]
            if let Some(target) = emulated_link_target(&path) {
                let meta = if symlink {
                    std::fs::symlink_metadata(&path)
                } else {
                    std::fs::metadata(&target)
                }
                .map_err(|error| fs_io(&error, syscall, &path))?;
                return Ok(metadata_value(&meta, symlink));
            }
            let meta = if symlink {
                std::fs::symlink_metadata(&path)
            } else {
                std::fs::metadata(&path)
            }
            .map_err(|error| fs_io(&error, syscall, &path))?;
            Ok(metadata_value(&meta, symlink))
        }
        "readdir" => {
            let path = read_path(runtime, payload, "path")?;
            let dirents = payload_bool(payload, "dirents");
            let entries =
                std::fs::read_dir(&path).map_err(|error| fs_io(&error, "scandir", &path))?;
            let mut names: Vec<JsonValue> = Vec::new();
            let mut typed: Vec<JsonValue> = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|error| fs_io(&error, "scandir", &path))?;
                let name = os_string_lossy(entry.file_name());
                if dirents {
                    let file_type = entry
                        .file_type()
                        .map_err(|error| fs_io(&error, "scandir", &path))?;
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
            let path = check_write(runtime, payload, "path", "mkdir")?;
            let result = if payload_bool(payload, "recursive") {
                std::fs::create_dir_all(&path)
            } else {
                std::fs::create_dir(&path)
            };
            result.map_err(|error| fs_io(&error, "mkdir", &path))?;
            Ok(JsonValue::Null)
        }
        "rm" => {
            let path = check_write_entry(runtime, payload, "path", "rm")?;
            rm_at(
                &path,
                payload_bool(payload, "recursive"),
                payload_bool(payload, "force"),
            )
            .map_err(|error| fs_io(&error, "rm", &path))?;
            Ok(JsonValue::Null)
        }
        "unlink" => {
            let path = check_write_entry(runtime, payload, "path", "unlink")?;
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(JsonValue::Null),
                // A junction is a directory object; removing the directory
                // entry is Node's `unlink` of a directory symlink.
                Err(error) if cfg!(windows) && is_directory_symlink(&path) => {
                    std::fs::remove_dir(&path).map_err(|_| fs_io(&error, "unlink", &path))?;
                    Ok(JsonValue::Null)
                }
                Err(error) => Err(fs_io(&error, "unlink", &path)),
            }
        }
        "rename" => {
            // rename removes the source directory entry, so both ends are
            // write operations; a read-only source fence would let an
            // extension move/delete a file outside the allowed roots.
            let from = check_write_entry(runtime, payload, "from", "rename")?;
            let to = check_write(runtime, payload, "to", "rename")?;
            std::fs::rename(&from, &to).map_err(|error| rename_io(&error, &from, &to))?;
            Ok(JsonValue::Null)
        }
        "copyFile" => {
            let from = read_path(runtime, payload, "from")?;
            let to = check_write(runtime, payload, "to", "copyfile")?;
            let mode = payload.get("mode").and_then(JsonValue::as_u64).unwrap_or(0) as u32;
            copy_file(&from, &to, mode)?;
            Ok(JsonValue::Null)
        }
        "realpath" => {
            let path = read_path(runtime, payload, "path")?;
            // libuv resolves through lstat; Node reports `syscall: "lstat"`
            // for a missing target.
            let canonical =
                std::fs::canonicalize(&path).map_err(|error| fs_io(&error, "lstat", &path))?;
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
                check_write(runtime, payload, "path", "access")?
            } else {
                read_path(runtime, payload, "path")?
            };
            access_check(&path, mode).map_err(|error| fs_io(&error, "access", &path))?;
            Ok(JsonValue::Null)
        }
        "readlink" => {
            let path = read_path(runtime, payload, "path")?;
            #[cfg(windows)]
            if let Some(text) = emulated_link_text(&path) {
                return Ok(JsonValue::String(text));
            }
            let target = std::fs::read_link(&path).map_err(|error| {
                let mapped = fs_io(&error, "readlink", &path);
                // libuv maps ERROR_NOT_A_REPARSE_POINT to EINVAL; Rust's
                // coarse ErrorKind lands on EIO.
                #[cfg(windows)]
                if mapped.code == "EIO" {
                    return io_message(
                        "EINVAL",
                        format!(
                            "EINVAL: invalid argument, readlink '{}'",
                            path.to_string_lossy()
                        ),
                        "readlink",
                        &path,
                    );
                }
                mapped
            })?;
            #[cfg(windows)]
            let target = {
                // Junction targets are stored in the NT namespace form
                // (`\??\C:\...`); Node reports the plain path.
                let text = target.to_string_lossy();
                let text = text.strip_prefix(r"\??\").unwrap_or(&text);
                PathBuf::from(text)
            };
            Ok(JsonValue::String(target.to_string_lossy().into_owned()))
        }
        "chmod" => {
            let path = check_write(runtime, payload, "path", "chmod")?;
            let mode = payload_mode(payload);
            chmod_path(&path, mode).map_err(|error| fs_io(&error, "chmod", &path))?;
            Ok(JsonValue::Null)
        }
        "cp" => {
            let from = read_path(runtime, payload, "from")?;
            let to = check_write(runtime, payload, "to", "cp")?;
            let meta = std::fs::metadata(&from).map_err(|error| fs_io(&error, "cp", &from))?;
            if meta.is_dir() {
                if !payload_bool(payload, "recursive") {
                    // Node refuses a directory copy without {recursive:true}.
                    return Err(NodeError {
                        code: "ERR_FS_EISDIR".to_string(),
                        message: format!(
                            "Path is a directory: cp '{}' -> '{}'",
                            from.display(),
                            to.display()
                        ),
                        syscall: Some("cp".to_string()),
                        path: Some(from.to_string_lossy().into_owned()),
                        // Node's own class error: no OS errno behind it.
                        errno: None,
                    });
                }
                copy_tree(&from, &to).map_err(|error| fs_io(&error, "cp", &from))?;
            } else {
                std::fs::copy(&from, &to).map_err(|error| fs_io(&error, "cp", &from))?;
            }
            Ok(JsonValue::Null)
        }
        "mkdtemp" => {
            let prefix = payload_text(payload, "prefix")?;
            let dir = mkdtemp(runtime, prefix)?;
            Ok(JsonValue::String(dir))
        }
        "rmdir" => {
            let path = check_write_entry(runtime, payload, "path", "rmdir")?;
            let recursive = payload_bool(payload, "recursive");
            let result = if recursive {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_dir(&path)
            };
            result.map_err(|error| {
                let mapped = fs_io(&error, "rmdir", &path);
                // libuv turns Windows' ERROR_DIRECTORY (a file handed to
                // RemoveDirectoryW) into ENOENT; Node reports that, not
                // ENOTDIR.
                #[cfg(windows)]
                if !recursive && mapped.code == "ENOTDIR" {
                    return io_message(
                        "ENOENT",
                        format!(
                            "ENOENT: no such file or directory, rmdir '{}'",
                            path.to_string_lossy()
                        ),
                        "rmdir",
                        &path,
                    );
                }
                mapped
            })?;
            Ok(JsonValue::Null)
        }
        "truncate" => {
            // Mutates the file: fenced like every other write target.
            let path = check_write(runtime, payload, "path", "truncate")?;
            let length = payload.get("len").and_then(JsonValue::as_u64).unwrap_or(0);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|error| fs_io(&error, "open", &path))?;
            file.set_len(length)
                .map_err(|error| fs_io(&error, "ftruncate", &path))?;
            Ok(JsonValue::Null)
        }
        "utimes" => {
            // Metadata mutation: fenced like a write.
            let path = check_write(runtime, payload, "path", "utimes")?;
            let to_time = |value: Option<f64>| -> std::time::SystemTime {
                // Negative/epoch-preceding stamps clamp to the epoch (some
                // platforms refuse pre-1970 times in the syscall).
                let millis = value.unwrap_or(0.0).max(0.0) as u64;
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
            };
            let atime = to_time(payload.get("atimeMs").and_then(JsonValue::as_f64));
            let mtime = to_time(payload.get("mtimeMs").and_then(JsonValue::as_f64));
            utimes_path(&path, atime, mtime).map_err(|error| fs_io(&error, "utime", &path))?;
            Ok(JsonValue::Null)
        }
        "lutimes" => {
            // Metadata mutation on the link itself (Node's `lutimes`).
            let path = check_write_entry(runtime, payload, "path", "lutimes")?;
            let to_time = |value: Option<f64>| -> std::time::SystemTime {
                let millis = value.unwrap_or(0.0).max(0.0) as u64;
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
            };
            let atime = to_time(payload.get("atimeMs").and_then(JsonValue::as_f64));
            let mtime = to_time(payload.get("mtimeMs").and_then(JsonValue::as_f64));
            lutimes_path(&path, atime, mtime).map_err(|error| fs_io(&error, "lutime", &path))?;
            Ok(JsonValue::Null)
        }
        "futimes" => {
            // FileHandle.utimes: the same metadata mutation on an open
            // descriptor (Node's syscall name is `futime`).
            let fd = payload_fd(payload)?;
            let to_time = |value: Option<f64>| -> std::time::SystemTime {
                let millis = value.unwrap_or(0.0).max(0.0) as u64;
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
            };
            let atime = to_time(payload.get("atimeMs").and_then(JsonValue::as_f64));
            let mtime = to_time(payload.get("mtimeMs").and_then(JsonValue::as_f64));
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "futime"))?;
            let times = std::fs::FileTimes::new()
                .set_accessed(atime)
                .set_modified(mtime);
            entry
                .file
                .set_times(times)
                .map_err(|error| fs_io(&error, "futime", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "symlink" => {
            // Node stores any target string (it is never followed here); only
            // the link path itself is sandbox-checked.
            let target = payload_text(payload, "target")?;
            let path = check_write(runtime, payload, "path", "symlink")?;
            let kind = payload
                .get("kind")
                .and_then(JsonValue::as_str)
                .unwrap_or("file");
            symlink_impl(target, &path, kind).map_err(|error| fs_io(&error, "symlink", &path))?;
            Ok(JsonValue::Null)
        }
        // fd family: `open` fences every write-capable descriptor; later ops
        // act on the held handle, like Node.
        "open" => {
            let flags = open_flags(payload)?;
            let path = if flags.pure_read() {
                read_path(runtime, payload, "path")?
            } else {
                check_write(runtime, payload, "path", "open")?
            };
            let (options, append) = flags.options(payload_mode(payload), &path)?;
            // Node/libuv can open a directory read-only (test-fs-stat opens
            // '.' and fstats it). MSVC's plain CreateFile refuses that with
            // ERROR_ACCESS_DENIED unless FILE_FLAG_BACKUP_SEMANTICS is set.
            let file = if path.is_dir() && flags.pure_read() {
                open_read_directory(&path)
            } else if path.is_dir() && flags.dir_write_open() {
                open_directory_readwrite(&path)
            } else {
                options.open(&path)
            }
            .map_err(|error| fs_io(&error, "open", &path))?;
            let fd = fd_guard(runtime)
                .insert(file, path, append, generation)
                .map_err(|()| {
                    NodeError::new(
                        "EMFILE",
                        format!("EMFILE: too many open files, open (limit {})", MAX_OPEN_FDS),
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
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "read"))?;
            // No path clone on the success path: the mutable borrow of `entry`
            // ends with the read, so the error closure can borrow its path.
            let read = entry.read(length, position);
            let bytes = read.map_err(|error| fs_io(&error, "read", &entry.path))?;
            Ok(JsonValue::String(STANDARD.encode(bytes)))
        }
        "write" => {
            let fd = payload_fd(payload)?;
            let data = payload_write_data(payload)?;
            let position = payload.get("position").and_then(JsonValue::as_u64);
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "write"))?;
            // No path clone on the success path (see the `read` arm).
            let written = entry.write(data.as_ref(), position);
            let written = written.map_err(|error| fs_io(&error, "write", &entry.path))?;
            Ok(serde_json::json!(written))
        }
        "fstat" => {
            let fd = payload_fd(payload)?;
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "fstat"))?;
            let meta = entry
                .file
                .metadata()
                .map_err(|error| fs_io(&error, "fstat", &entry.path))?;
            Ok(metadata_value(&meta, false))
        }
        "fsync" => {
            let fd = payload_fd(payload)?;
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "fsync"))?;
            // FlushFileBuffers on a directory handle is ACCESS_DENIED on
            // Windows; Node/libuv treat fsync of a directory as a no-op
            // (test-fs-utimes fstats/fyncs the tmpdir).
            if entry
                .file
                .metadata()
                .is_ok_and(|metadata| metadata.is_dir())
            {
                return Ok(JsonValue::Null);
            }
            entry
                .file
                .sync_all()
                .map_err(|error| fs_io(&error, "fsync", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "fchmod" => {
            let fd = payload_fd(payload)?;
            let mode = payload_mode(payload);
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "fchmod"))?;
            chmod_file(&entry.file, mode).map_err(|error| fs_io(&error, "fchmod", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "ftruncate" => {
            let fd = payload_fd(payload)?;
            let length = payload.get("len").and_then(JsonValue::as_u64).unwrap_or(0);
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "ftruncate"))?;
            entry
                .file
                .set_len(length)
                .map_err(|error| fs_io(&error, "ftruncate", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "fdatasync" => {
            let fd = payload_fd(payload)?;
            let mut guard = fd_guard(runtime);
            let entry = guard.get_mut(fd).ok_or_else(|| fd_error(fd, "fdatasync"))?;
            entry
                .file
                .sync_data()
                .map_err(|error| fs_io(&error, "fdatasync", &entry.path))?;
            Ok(JsonValue::Null)
        }
        "close" => {
            let fd = payload_fd(payload)?;
            fd_guard(runtime)
                .remove(fd)
                .ok_or_else(|| fd_error(fd, "close"))?;
            Ok(JsonValue::Null)
        }
        "link" => {
            // Hard link: the destination entry is created AND the source inode
            // becomes writable through the new name, so both ends are write
            // targets. Fencing both keeps a link from smuggling an outside
            // file into the sandbox as a writable alias (copyFile fences only
            // `to` because it copies bytes, not identity).
            let from = check_write(runtime, payload, "from", "link")?;
            let to = check_write(runtime, payload, "to", "link")?;
            std::fs::hard_link(&from, &to).map_err(|error| fs_io(&error, "link", &from))?;
            Ok(JsonValue::Null)
        }
        "exists" => {
            let path = read_path(runtime, payload, "path")?;
            #[cfg(windows)]
            if let Some(target) = emulated_link_target(&path) {
                return Ok(JsonValue::Bool(target.exists()));
            }
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
            code: "EISDIR".to_string(),
            message: "EISDIR: illegal operation on a directory, read".to_owned(),
            syscall: Some("read".to_string()),
            path: None,
            errno: fs_errno("EISDIR"),
        };
    }
    fs_io(error, "open", path)
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

    /// `r+` on a directory: opens (attribute rights) on Windows in Node,
    /// without creating/truncating/appending anything.
    fn dir_write_open(&self) -> bool {
        match self {
            OpenFlags::Text(text) => matches!(*text, "r+" | "rs+" | "sr+"),
            OpenFlags::Bits(bits) => {
                (bits & O_ACCMODE) == O_RDWR
                    && (bits & (O_APPEND | O_CREAT | O_TRUNC | O_EXCL)) == 0
            }
        }
    }
}

/// Node's Windows shape for a numeric mask the OS refuses (`uv_fs_open` →
/// `EINVAL`, libuv errno -4071): the message carries the syscall and path, no
/// argument-validation wording.
fn invalid_open_flags(path: &Path) -> NodeError {
    let text = path.to_string_lossy().into_owned();
    NodeError {
        code: "EINVAL".to_string(),
        message: format!("EINVAL: invalid argument, open '{text}'"),
        syscall: Some("open".to_string()),
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
/// Read-only open of a directory (Node's `fs.open(dir, 'r')`). Windows needs
/// `FILE_FLAG_BACKUP_SEMANTICS`; Unix `File::open` already accepts it.
#[cfg(windows)]
fn open_read_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(unix)]
fn open_read_directory(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Node/libuv opens a directory `r+` (test-fs-utimes' `openSync(dir, 'r+')`
/// then `futimes`). A generic write-open of a directory is ERROR_ACCESS_DENIED,
/// so request only the attribute rights a directory handle supports.
#[cfg(windows)]
fn open_directory_readwrite(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(unix)]
fn open_directory_readwrite(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

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
        // The Windows CRT refuses O_TRUNC without O_CREAT (`_open` EINVAL);
        // Node/libuv surface that as EINVAL for the numeric mask form. The
        // flag-string form supplies the create bit itself.
        #[cfg(windows)]
        if bits & O_CREAT == 0 {
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
fn fd_error(fd: i32, syscall: &str) -> NodeError {
    // Node's message names the syscall, not the descriptor:
    // `EBADF: bad file descriptor, fstat`.
    let _ = fd;
    let mut error = NodeError::new("EBADF", format!("EBADF: bad file descriptor, {syscall}"));
    error.syscall = Some(syscall.to_owned());
    error.errno = fs_errno("EBADF");
    error
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

/// Node's `copyfile` error text always names both ends:
/// `ENOENT: no such file or directory, copyfile '<src>' -> '<dst>'`; the
/// `path` field is the source.
fn copyfile_io(error: &std::io::Error, from: &Path, to: &Path) -> NodeError {
    let mut mapped = fs_io(error, "copyfile", from);
    mapped.message = format!("{} -> '{}'", mapped.message, to.to_string_lossy());
    mapped
}

fn copyfile_message(code: &str, text: &str, from: &Path, to: &Path) -> NodeError {
    io_message(
        code,
        format!(
            "{code}: {text}, copyfile '{}' -> '{}'",
            from.to_string_lossy(),
            to.to_string_lossy()
        ),
        "copyfile",
        from,
    )
}

/// `rename` failure: Node keeps `path` as the source but appends the
/// destination to the message.
fn rename_io(error: &std::io::Error, from: &Path, to: &Path) -> NodeError {
    let mut mapped = fs_io(error, "rename", from);
    mapped.message = format!("{} -> '{}'", mapped.message, to.to_string_lossy());
    mapped
}

/// `fs.copyFile` core.
///
/// `COPYFILE_EXCL` (1) reserves the destination with `CREATE_NEW`, so an
/// existing file is `EEXIST` without being touched. `COPYFILE_FICLONE` (2) is
/// a hint: this host falls back to a normal copy. `COPYFILE_FICLONE_FORCE`
/// (4) demands a reflink, which libuv on Windows cannot provide and Node
/// reports as `ENOSYS` (checked against node v24.22.1 win32); it is checked
/// before EXCL, matching libuv's order.
fn copy_file(from: &Path, to: &Path, mode: u32) -> Result<(), NodeError> {
    // libuv opens the source first: a missing source is ENOENT even when the
    // destination also fails (or EXCL is set).
    let meta = std::fs::metadata(from).map_err(|error| copyfile_io(&error, from, to))?;
    if meta.is_dir() {
        return Err(copyfile_message(
            "EPERM",
            "operation not permitted",
            from,
            to,
        ));
    }
    if mode & 4 != 0 {
        #[cfg(windows)]
        return Err(copyfile_message(
            "ENOSYS",
            "function not implemented",
            from,
            to,
        ));
        // POSIX: no portable reflink probe; a normal copy is the documented
        // fallback (`COPYFILE_FICLONE` behaviour) rather than a hard failure.
    }
    if mode & 1 != 0 {
        let mut dest = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    copyfile_message("EEXIST", "file already exists", from, to)
                } else {
                    copyfile_io(&error, from, to)
                }
            })?;
        let mut source =
            std::fs::File::open(from).map_err(|error| copyfile_io(&error, from, to))?;
        std::io::copy(&mut source, &mut dest).map_err(|error| copyfile_io(&error, from, to))?;
        let _ = dest.set_permissions(meta.permissions());
        return Ok(());
    }
    std::fs::copy(from, to).map_err(|error| copyfile_io(&error, from, to))?;
    Ok(())
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

/// `mkdtemp` core: Node appends six random characters to the prefix and
/// returns the prefix spelling the caller used (a relative prefix stays
/// relative). The parent directory is fenced like every other write target
/// (cwd, the sandbox roots, the agent dir, the OS temp dir).
///
/// A relative prefix resolves against the runtime cwd, exactly like every
/// other fs path (Node's `process.cwd()` semantics); it used to resolve under
/// the system temp dir, which made `mkdtempSync("r1/tmp-")` fail ENOENT when
/// `r1/` lived in the cwd.
///
/// The error path is Node's template (`<prefix>XXXXXX`), not the candidate
/// that was attempted, matching libuv.
fn mkdtemp(runtime: &crate::pir_node::NodeRuntime, prefix: &str) -> Result<String, NodeError> {
    let relative = !Path::new(prefix).is_absolute();
    let base = if relative {
        runtime.cwd().join(prefix)
    } else {
        PathBuf::from(prefix)
    };
    let (parent, stem) = if prefix.is_empty() {
        (runtime.cwd(), String::new())
    } else if prefix.ends_with('\\') || prefix.ends_with('/') {
        (base.clone(), String::new())
    } else {
        (
            base.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| runtime.cwd()),
            base.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };
    let checked_parent = normalize_for_check(&parent);
    let roots = effective_roots(runtime);
    if !roots.iter().any(|root| path_within(&checked_parent, root)) {
        return Err(NodeError::denied("mkdtemp", prefix));
    }
    let template = format!("{prefix}XXXXXX");
    for _ in 0..128 {
        let suffix = random_suffix();
        let candidate = parent.join(format!("{stem}{suffix}"));
        match std::fs::create_dir(&candidate) {
            // Node/libuv return the template text with the random suffix, i.e.
            // the caller's separator spelling survives.
            Ok(()) => return Ok(format!("{prefix}{suffix}")),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(fs_io(&error, "mkdtemp", Path::new(&template))),
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

/// Set the target's timestamps (`utimes`): FILE_WRITE_ATTRIBUTES +
/// FILE_FLAG_BACKUP_SEMANTICS also opens directories, which Node's utimes
/// supports (a plain write-open of a directory is EPERM).
#[cfg(windows)]
#[allow(unsafe_code)]
fn utimes_path(
    path: &Path,
    atime: std::time::SystemTime,
    mtime: std::time::SystemTime,
) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(atime)
        .set_modified(mtime);
    file.set_times(times)
}

#[cfg(not(windows))]
fn utimes_path(
    path: &Path,
    atime: std::time::SystemTime,
    mtime: std::time::SystemTime,
) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(atime)
        .set_modified(mtime);
    file.set_times(times)
}

/// Set the link's own timestamps (`lutimes`): open the link object with
/// FILE_FLAG_OPEN_REPARSE_POINT / FILE_WRITE_ATTRIBUTES so the target is not
/// touched. Emulated file links are plain marker files, and their lstat view
/// is exactly the file being opened here.
#[cfg(windows)]
#[allow(unsafe_code)]
fn lutimes_path(
    path: &Path,
    atime: std::time::SystemTime,
    mtime: std::time::SystemTime,
) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(atime)
        .set_modified(mtime);
    file.set_times(times)
}

#[cfg(not(windows))]
fn lutimes_path(
    _path: &Path,
    _atime: std::time::SystemTime,
    _mtime: std::time::SystemTime,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "lutimes is not implemented on this platform",
    ))
}

/// Free/total space of the volume holding `path` (libuv's `uv_fs_statfs`):/// `(bsize, blocks, bfree)`. Clusters are the unit; `bsize` is cluster bytes.
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_disk_space(path: &Path) -> std::io::Result<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;

    extern "system" {
        fn GetDiskFreeSpaceW(
            root: *const u16,
            sectors_per_cluster: *mut u32,
            bytes_per_sector: *mut u32,
            free_clusters: *mut u32,
            total_clusters: *mut u32,
        ) -> i32;
    }
    // The API wants a volume directory; a file path is resolved to its parent
    // first (the containing directory is on the same volume).
    let root = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut sectors = 0u32;
    let mut bytes = 0u32;
    let mut free = 0u32;
    let mut total = 0u32;
    // SAFETY: the path is NUL-terminated; the four out-pointers are valid
    // locals for the duration of the call.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors,
            &mut bytes,
            &mut free,
            &mut total,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bsize = (bytes as u64).saturating_mul(sectors as u64);
    Ok((bsize, total as u64, free as u64))
}

#[cfg(windows)]
#[cfg(windows)]
fn is_directory_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn symlink_impl(target: &str, path: &Path, kind: &str) -> std::io::Result<()> {
    // Junctions use FSCTL_SET_REPARSE_POINT (no SeCreateSymbolicLinkPrivilege
    // needed); `dir`/`file` go through the normal API and fall back on an
    // unprivileged account: directories become junctions, files become the
    // ADS-marker emulation below.
    if kind == "junction" {
        return create_junction(target, path);
    }
    let primary = if kind == "dir" {
        std::os::windows::fs::symlink_dir(target, path)
    } else {
        std::os::windows::fs::symlink_file(target, path)
    };
    match primary {
        Ok(()) => Ok(()),
        // ERROR_PRIVILEGE_NOT_HELD: the account has no symlink privilege
        // (UAC-filtered admin tokens included).
        Err(error) if error.raw_os_error() == Some(1314) => {
            if kind == "dir" {
                create_junction(target, path)
            } else {
                create_emulated_file_link(target, path)
            }
        }
        Err(error) => Err(error),
    }
}

/// Marker for the unprivileged file-symlink emulation: the link path is an
/// empty file whose `:pir_symlink` alternate data stream stores the target.
/// Every read-side op (stat/readlink/exists) consults it first.
#[cfg(windows)]
fn emulated_link_ads(path: &Path) -> std::ffi::OsString {
    let mut ads = path.as_os_str().to_owned();
    ads.push(":pir_symlink");
    ads
}

#[cfg(windows)]
fn create_emulated_file_link(target: &str, path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    std::fs::write(emulated_link_ads(path), target.as_bytes())
}

/// The raw target string of an emulated file link, if `path` is one.
#[cfg(windows)]
fn emulated_link_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(emulated_link_ads(path)).ok()
}

/// The target an emulated link resolves to (relative targets are relative to
/// the link's parent, like a real symlink).
#[cfg(windows)]
fn emulated_link_target(path: &Path) -> Option<PathBuf> {
    let text = emulated_link_text(path)?;
    let target = PathBuf::from(&text);
    if target.is_absolute() {
        Some(target)
    } else {
        Some(path.parent().unwrap_or_else(|| Path::new(".")).join(target))
    }
}

/// Create an NTFS junction without SeCreateSymbolicLinkPrivilege:
/// a directory plus an IO_REPARSE_TAG_MOUNT_POINT reparse point.
#[cfg(windows)]
#[allow(unsafe_code)]
fn create_junction(target: &str, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    type Handle = *mut std::ffi::c_void;
    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut std::ffi::c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            handle: Handle,
            code: u32,
            input: *mut std::ffi::c_void,
            input_size: u32,
            output: *mut std::ffi::c_void,
            output_size: u32,
            returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    const INVALID_HANDLE_VALUE: isize = -1;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const FILE_SHARE_DELETE: u32 = 4;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    std::fs::create_dir(path)?;
    let absolute = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };
    let substitute = format!(r"\??\{}", absolute.display());
    let print = absolute.display().to_string();
    let substitute: Vec<u16> = std::ffi::OsStr::new(&substitute).encode_wide().collect();
    let print: Vec<u16> = std::ffi::OsStr::new(&print).encode_wide().collect();
    let substitute_bytes: Vec<u8> = substitute.iter().flat_map(|u| u.to_le_bytes()).collect();
    let print_bytes: Vec<u8> = print.iter().flat_map(|u| u.to_le_bytes()).collect();
    let path_bytes = substitute_bytes.len() + 2 + print_bytes.len() + 2;
    let data_length = 8 + path_bytes;
    let mut buffer = vec![0u8; 8 + data_length];
    buffer[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer[4..6].copy_from_slice(&(data_length as u16).to_le_bytes());
    let mut offset = 8;
    buffer[offset..offset + 2].copy_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    offset += 2;
    buffer[offset..offset + 2].copy_from_slice(&(substitute_bytes.len() as u16).to_le_bytes());
    offset += 2;
    buffer[offset..offset + 2]
        .copy_from_slice(&((substitute_bytes.len() + 2) as u16).to_le_bytes());
    offset += 2;
    buffer[offset..offset + 2].copy_from_slice(&(print_bytes.len() as u16).to_le_bytes());
    offset += 2;
    buffer[offset..offset + substitute_bytes.len()].copy_from_slice(&substitute_bytes);
    offset += substitute_bytes.len() + 2;
    buffer[offset..offset + print_bytes.len()].copy_from_slice(&print_bytes);

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated; the handle is closed by the `File`
    // wrapper below; the reparse buffer is a live local during the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let result = if handle as isize == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: the raw handle is valid; `File` assumes ownership and
        // closes it, and the buffers outlive the ioctl.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as Handle,
                FSCTL_SET_REPARSE_POINT,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_dir(path);
    }
    result
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

/// Node-parity stat fields shared by `stat`/`lstat` (both the JSON op and the
/// typed host entry read this one shape).
///
/// Windows values are libuv's: `st_dev` is the volume serial number, `st_ino`
/// the 64-bit file index, `st_nlink` the link count, `st_uid`/`st_gid`/
/// `st_rdev` are 0, `st_blksize` 4096 and `st_blocks` 0. Times come from the
/// FILETIMEs (100 ns since 1601, converted to Unix ns); like libuv, `ctime`
/// and `birthtime` both report the creation time there.
///
/// `volume_serial_number`/`file_index`/`number_of_links` are still nightly-only
/// on `std::os::windows::fs::MetadataExt`, so this stable build reports Node's
/// fallback values for them: `dev`/`ino` 0; `nlink` is 1 (what Node reports
/// for ordinary files and directories on NTFS). `blocks` is 0 and `blksize`
/// 4096 on Windows, matching Node.
#[cfg(windows)]
fn stat_parts_of(meta: &std::fs::Metadata, symlink: bool) -> StatParts {
    use std::os::windows::fs::MetadataExt as _;

    // 100 ns ticks between 1601-01-01 and 1970-01-01.
    const EPOCH_DELTA_100NS: i128 = 116_444_736_000_000_000;
    let filetime_ns = |ticks: u64| -> i64 {
        let ns = (ticks as i128 - EPOCH_DELTA_100NS).saturating_mul(100);
        ns.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    };
    let creation = meta.creation_time();
    StatParts {
        is_file: meta.is_file(),
        is_directory: meta.is_dir(),
        is_symbolic_link: symlink,
        mode: metadata_mode(meta),
        size: meta.len(),
        dev: 0,
        ino: 0,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        blocks: 0,
        atime_ns: filetime_ns(meta.last_access_time()),
        mtime_ns: filetime_ns(meta.last_write_time()),
        ctime_ns: filetime_ns(creation),
        birthtime_ns: filetime_ns(creation),
    }
}

#[cfg(unix)]
fn stat_parts_of(meta: &std::fs::Metadata, symlink: bool) -> StatParts {
    use std::os::unix::fs::MetadataExt as _;

    let ns = |seconds: i64, nanos: i64| seconds.saturating_mul(1_000_000_000).saturating_add(nanos);
    StatParts {
        is_file: meta.is_file(),
        is_directory: meta.is_dir(),
        is_symbolic_link: symlink,
        mode: meta.mode(),
        size: meta.size(),
        dev: meta.dev(),
        ino: meta.ino(),
        nlink: meta.nlink(),
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev(),
        blksize: meta.blksize(),
        blocks: meta.blocks(),
        atime_ns: ns(meta.atime(), meta.atime_nsec()),
        mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
        ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        // libuv leaves birthtime at 0 where the platform does not expose it.
        birthtime_ns: 0,
    }
}

/// Node-parity stat value shared by `stat`/`lstat`.
///
/// Exact integer fields ride as decimal strings so the JSON channel (which has
/// no BigInt) keeps full 64-bit precision; the JS `Stats` side converts them.
fn metadata_value(meta: &std::fs::Metadata, symlink: bool) -> JsonValue {
    let p = stat_parts_of(meta, symlink);
    serde_json::json!({
        "isFile": p.is_file,
        "isDirectory": p.is_directory,
        "isSymbolicLink": p.is_symbolic_link,
        "mode": p.mode,
        "size": p.size.to_string(),
        "dev": p.dev.to_string(),
        "ino": p.ino.to_string(),
        "nlink": p.nlink.to_string(),
        "uid": p.uid.to_string(),
        "gid": p.gid.to_string(),
        "rdev": p.rdev.to_string(),
        "blksize": p.blksize.to_string(),
        "blocks": p.blocks.to_string(),
        "atimeMs": p.atime_ms(),
        "mtimeMs": p.mtime_ms(),
        "ctimeMs": p.ctime_ms(),
        "birthtimeMs": p.birthtime_ms(),
        "atimeNs": p.atime_ns.to_string(),
        "mtimeNs": p.mtime_ns.to_string(),
        "ctimeNs": p.ctime_ns.to_string(),
        "birthtimeNs": p.birthtime_ns.to_string(),
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
