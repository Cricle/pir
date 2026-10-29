//! `RemoteExecutionEnv`: one machine's environment, reached through a pi-env
//! daemon — upstream `packages/env/src/remote-env.ts`, the same surface the JS
//! shim carries (`pi_env_shim.js.txt`). Results match the daemon-side
//! `NodeExecutionEnv` running on that machine: the daemon performs the system
//! calls and this struct applies Node's path, error and result rules.
//!
//! Operation set (what pir's `bash`/`read`/`write`/`edit` tools need, plus the
//! protocol's own listing surface):
//!
//! | upstream | here |
//! |---|---|
//! | `_resolve` / `absolutePath` / `joinPath` | [`RemoteExecutionEnv::resolve`], [`RemoteExecutionEnv::absolute_path`], [`RemoteExecutionEnv::join_path`] |
//! | `fileInfo` / `exists` / `canonicalPath` | [`RemoteExecutionEnv::file_info`], [`RemoteExecutionEnv::exists`], [`RemoteExecutionEnv::canonical_path`] |
//! | `readTextFile` / `readBinaryFile` | [`RemoteExecutionEnv::read_text_file`], [`RemoteExecutionEnv::read_binary_file`] |
//! | `writeFile` / `appendFile` / `truncateFile` / `flushFile` / `renameFile` | the same names (`write_file`, `append_file`, `truncate_file`, `flush_file`, `rename_file`) |
//! | `createDir` / `remove` / `createTempDir` / `createTempFile` | the same names |
//! | `listDir` / `listFiles` | [`RemoteExecutionEnv::list_dir`], [`RemoteExecutionEnv::list_files`] |
//! | `openBinaryReader` / `openTextLineReader` / `openDirReader` / `readTextLines` | [`RemoteExecutionEnv::open_binary_reader`], [`RemoteExecutionEnv::open_text_line_reader`], [`RemoteExecutionEnv::open_dir_reader`], [`RemoteExecutionEnv::read_text_lines`] |
//! | `RemoteBinaryReader` / `RemoteTextLineReader` / `RemoteDirReader` | [`BinaryReader`], [`TextLineReader`], [`DirReader`] |
//! | `exec` / `cleanup` | [`RemoteExecutionEnv::exec`], [`RemoteExecutionEnv::cleanup`] |
//! | `watch` | [`RemoteExecutionEnv::watch`] (`RemoteWatcher`) |
//!
//! Deltas from upstream, all deliberate:
//!
//! - `cwd` is optional: `None` means "the daemon's own working directory"
//!   (`hello.cwd`). Upstream requires the caller to name one; a tool-side
//!   consumer has no remote directory to name, so the daemon's own is the
//!   documented default (`PI_ENV_REMOTE_CWD` overrides it).
//! - `writeFile`'s chunk fan-out is sequential. The daemon serializes chunks in
//!   arrival order anyway, and the abort check before each chunk — the part
//!   upstream's shape exists for — is preserved.
//! - `readFile`'s and `BinaryReader.read`'s `pread` window is issued in batches
//!   of `READ_DEPTH` requests instead of a sliding window; a short read still
//!   continues right after the read that ended early.
//! - Callbacks (`on_output`) are infallible in Rust, so durable's
//!   `callback_error` cannot arise here (the code is kept in
//!   [`ExecutionErrorCode`] for the contract's sake).
//! - An `options` object is flattened into arguments where upstream takes one
//!   (`open_binary_reader`'s `no_follow`, `read_text_lines`'s `max_lines`,
//!   `scan_lines`'s `start_line`/`end_line`), and each reader's `close` is
//!   `&mut self` rather than a `context`-taking method: the closed flag is the
//!   reader's own state (`#closed`), and upstream passes a context it ignores.
//! - `read`/`scanLines` take `u64` offsets and line numbers, so the negative
//!   half of `Number.isSafeInteger` is a type, not a runtime error; the
//!   `> MAX_SAFE_INTEGER` half is still checked and reported with upstream's
//!   message.
//! - `exec` with `inheritEnv` on a **local** daemon forwards this process's
//!   live environment (see [`crate::env_client::connection::ConnectionOptions::local`]).
//!   Upstream's caller
//!   passes a `shellEnv` snapshot; a local daemon is long-lived, so forwarding
//!   the current environment keeps `inheritEnv` true to what a local command
//!   sees after `std::env::set_var`. The daemon still inherits its own
//!   environment, and the request's `env` (with `shellEnv`) wins over it. Only
//!   Unicode `KEY=VALUE` pairs are forwarded; see `RemoteExecutionEnv::exec`.
//!
//! Not ported (no pir consumer; upstream's durable-side surface): the terminal
//! (`openTerminal` / `pty`). Its wire op (`pty`) is implemented by the daemon
//! either way, so a consumer can be added without touching the protocol layer.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{json, Map, Value};
use tokio::sync::OnceCell;

use crate::ai::types::AbortSignal;
use crate::env_daemon::decode::{decode_into, range_decoder, StreamDecoder};

use super::connection::{lock, Connection, EventFn, RemoteInfo, Reply, RequestOptions};
use super::errors::{
    to_file_error, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, RemoteError,
};
use super::paths::{drive_cwd, file_url_to_path, RemotePathFlavor};
use super::watch::{RemoteWatcher, WatchChangeFn, WatchOptions};

/// Node's `MAX_TIMEOUT_MS` (2^31 - 1), in milliseconds.
const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// `readFile` and [`BinaryReader::read`] issue this many `pread` requests at once.
const READ_DEPTH: usize = 8;
/// Bytes per `pread` request of a binary reader; several are in flight at once.
const READ_CHUNK: usize = 256 * 1024;
/// `NodeTextLineReader`'s read size.
const LINE_CHUNK: usize = 64 * 1024;
/// Node's `writeFile` writes at most this much per call and checks for an abort before each.
const WRITE_CHUNK: usize = 512 * 1024;
/// `readFile`: chunk of known-size reads, chunk of unknown-size reads, and the largest file it reads.
const READ_FILE_CHUNK: usize = 512 * 1024;
const READ_FILE_UNKNOWN_CHUNK: usize = 64 * 1024;
const READ_FILE_MAX: u64 = (1 << 31) - 1;
/// Largest `maxEntries`/`maxBytes`/`timeoutMs` accepted: `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// `readdir` page size of `listDir`.
const READDIR_PAGE: u64 = 1000;

/// Kind bytes of `listFiles` records: `f`, `d`, `l`.
fn listed_kind(byte: u8) -> Option<&'static str> {
    match byte {
        b'f' => Some("file"),
        b'd' => Some("directory"),
        b'l' => Some("symlink"),
        _ => None,
    }
}

/// What a file is (`info.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Directory => "directory",
            FileKind::Symlink => "symlink",
            FileKind::Other => "other",
        }
    }

    fn parse(kind: &str) -> FileKind {
        match kind {
            "file" => FileKind::File,
            "directory" => FileKind::Directory,
            "symlink" => FileKind::Symlink,
            _ => FileKind::Other,
        }
    }
}

/// One `fileInfo` result (`NodeExecutionEnv`'s `FileInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: f64,
}

/// A `listFiles` entry: a path relative to the listed root, with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    pub path: String,
    pub kind: &'static str,
}

/// A `listFiles` result (`{ entries, truncated }`).
#[derive(Debug, Clone, PartialEq)]
pub struct FileListing {
    pub entries: Vec<ListedEntry>,
    pub truncated: bool,
}

/// `listFiles` options; `None` fields are not sent, so the daemon applies its defaults.
#[derive(Debug, Clone, Default)]
pub struct ListFilesOptions {
    pub hidden: Option<bool>,
    pub gitignore: Option<bool>,
    pub exclude: Option<Vec<String>>,
    pub max_entries: Option<u64>,
    pub max_bytes: Option<u64>,
    pub timeout_ms: Option<u64>,
}

/// Where lines of a file are, as [`BinaryReader::scan_lines`] found them
/// (Durable's `LineScan`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineScan {
    /// Newline bytes in the whole file; it has `newlines + 1` lines.
    pub newlines: u64,
    /// Byte range of the selected lines: from the start of the first to the end
    /// of the last, without the newline that ends it.
    pub start: u64,
    pub end: u64,
    /// Where the first selected line ends: its newline, or the end of the file.
    pub first_line_end: u64,
    /// Where the last selected line starts.
    pub last_line_start: u64,
    /// UTF-8 byte length of the decoded selection, and of its first line.
    pub selected_bytes: u64,
    pub first_line_bytes: u64,
}

impl LineScan {
    /// The daemon's `LineScan` record (`scanLines`); upstream hands the reply's
    /// JSON over unread, so a field that is missing or not a number reads 0.
    fn from_json(json: &Value) -> LineScan {
        let number = |key: &str| json.get(key).and_then(Value::as_u64).unwrap_or(0);
        LineScan {
            newlines: number("newlines"),
            start: number("start"),
            end: number("end"),
            first_line_end: number("firstLineEnd"),
            last_line_start: number("lastLineStart"),
            selected_bytes: number("selectedBytes"),
            first_line_bytes: number("firstLineBytes"),
        }
    }
}

/// One line of a text file ([`TextLineReader::read_line`]), Durable's `TextLine`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextLine {
    /// The line's text, without the newline that ends it.
    pub text: String,
    /// Whether the line ended with a newline (the last line of a file that does
    /// not end with one is not terminated).
    pub terminated: bool,
}

/// A page of a directory listing ([`DirReader::next`]).
#[derive(Debug, Clone, PartialEq)]
pub struct DirPage {
    pub entries: Vec<FileInfo>,
    /// The listing is complete: the next call has nothing more to report.
    pub done: bool,
}

/// The per-call context upstream spells `{ abortSignal }`.
#[derive(Default, Clone)]
pub struct Context {
    pub signal: Option<AbortSignal>,
}

impl Context {
    pub fn new(signal: Option<AbortSignal>) -> Context {
        Context { signal }
    }

    pub fn aborted(&self) -> bool {
        self.signal.as_ref().is_some_and(AbortSignal::is_aborted)
    }

    /// `abortResult`: the aborted `FileError` while the signal is set, else `None`.
    pub fn abort_result(&self, path: &str) -> Option<FileError> {
        if self.aborted() {
            Some(FileError::new(
                FileErrorCode::Aborted,
                "aborted",
                path.to_string(),
            ))
        } else {
            None
        }
    }
}

/// A command: shell text or an argv array.
#[derive(Debug, Clone)]
pub enum ExecCommand {
    Shell(String),
    Argv(Vec<String>),
}

/// `onOutput(text, info)` of a running command.
pub type OutputFn = Arc<dyn Fn(&str, OutputInfo) + Send + Sync>;

/// What an output event carries (`{ stream, skipped? }`).
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub stream: &'static str,
    pub skipped: Option<Value>,
}

/// `exec` options (upstream `opts`).
#[derive(Default, Clone)]
pub struct ExecOptions {
    pub cwd: Option<String>,
    /// A `None` value removes an inherited variable from the request, as
    /// upstream's `undefined` does. (The protocol has no "unset": a variable
    /// the daemon itself inherited when it started stays in its environment.)
    pub env: BTreeMap<String, Option<String>>,
    pub inherit_env: Option<bool>,
    pub timeout_secs: Option<f64>,
    pub spill: Option<Value>,
    pub window: Option<Value>,
    pub on_output: Option<OutputFn>,
}

/// A finished command.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    pub exit_code: i64,
    pub spill_path: Option<String>,
}

/// Options for a [`RemoteExecutionEnv`].
#[derive(Clone)]
pub struct RemoteExecutionEnvOptions {
    pub id: String,
    pub connection: Connection,
    /// The environment's working directory; `None` uses the daemon's own
    /// (`hello.cwd`).
    pub cwd: Option<String>,
    pub shell_path: Option<String>,
    pub shell_env: BTreeMap<String, String>,
    pub watch: WatchOptions,
}

impl RemoteExecutionEnvOptions {
    pub fn new(id: impl Into<String>, connection: Connection) -> RemoteExecutionEnvOptions {
        RemoteExecutionEnvOptions {
            id: id.into(),
            connection,
            cwd: None,
            shell_path: None,
            shell_env: BTreeMap::new(),
            watch: WatchOptions::default(),
        }
    }
}

/// One `watch` target: a resolved path, whether it recurses, and what to skip.
#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub path: String,
    pub recursive: bool,
    pub exclude: Option<WatchExclude>,
}

/// `{ hidden, names }` of a watch target.
#[derive(Debug, Clone, Default)]
pub struct WatchExclude {
    pub hidden: bool,
    pub names: Vec<String>,
}

/// An `ExecutionEnv` on another machine, reached through a pi-env daemon.
pub struct RemoteExecutionEnv {
    pub id: String,
    pub connection: Connection,
    cwd: Option<String>,
    resolved_cwd: OnceCell<String>,
    shell_path: Option<String>,
    shell_env: BTreeMap<String, String>,
    watch_options: WatchOptions,
    /// Running commands this environment started, for `cleanup()`.
    running: Arc<Mutex<HashMap<u32, u32>>>,
}

impl RemoteExecutionEnv {
    pub fn new(options: RemoteExecutionEnvOptions) -> RemoteExecutionEnv {
        RemoteExecutionEnv {
            id: options.id,
            connection: options.connection,
            cwd: options.cwd,
            resolved_cwd: OnceCell::new(),
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            watch_options: options.watch,
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn info(&self) -> Result<RemoteInfo, RemoteError> {
        self.connection.info().await
    }

    /// The remote system's path rules (`remotePath`).
    pub async fn paths(&self) -> Result<RemotePathFlavor, RemoteError> {
        remote_paths(&self.connection).await
    }

    /// The environment's working directory: the configured one, or the
    /// daemon's own (`hello.cwd`).
    pub async fn root(&self) -> Result<String, RemoteError> {
        if let Some(cwd) = &self.cwd {
            return Ok(cwd.clone());
        }
        self.resolved_cwd
            .get_or_try_init(|| async {
                let info = self.connection.info().await?;
                if info.cwd.is_empty() {
                    return Err(RemoteError::new(
                        "unknown",
                        "The pi-env daemon did not report a working directory",
                    ));
                }
                Ok(info.cwd)
            })
            .await
            .cloned()
    }

    /// Node's `resolvePath` on the remote system: `~`, `file://` URLs, the
    /// remote system's separators, its home directory and (on Windows) its
    /// drives' own working directories.
    pub async fn resolve(&self, path: &str) -> Result<String, RemoteError> {
        let info = self.info().await?;
        let (platform, _) = info.node_platform();
        let paths = RemotePathFlavor::for_platform(&platform);
        let windows = paths == RemotePathFlavor::Win32;
        let mut normalized = path.to_string();
        if normalized == "~" {
            normalized = info.home.clone();
        } else if let Some(rest) = normalized.strip_prefix("~/") {
            normalized = paths.join(&[&info.home, rest]);
        } else if windows {
            if let Some(rest) = normalized.strip_prefix("~\\") {
                normalized = paths.join(&[&info.home, rest]);
            }
        }
        if normalized.starts_with("file://") {
            // A malformed URL stays an ordinary path, as Node does.
            if let Some(converted) = file_url_to_path(&normalized, paths) {
                normalized = converted;
            }
        }
        if paths.is_absolute(&normalized) {
            // A Windows root-relative path (`\x`) is absolute, but resolves on
            // the base's drive (`path.win32.resolve` semantics), so it goes
            // through the base-aware form with the environment's own cwd.
            if paths.is_root_relative(&normalized) {
                let root = self.root().await?;
                return Ok(paths.resolve(&root, &normalized));
            }
            return Ok(paths.normalize(&normalized));
        }
        let root = self.root().await?;
        if windows {
            // A drive-relative path on another drive: Node on Windows resolves
            // it against that drive's working directory (`=D:`), else against
            // its own when it is on that drive, else against the drive's root.
            if let Some(drive) = drive_prefix(&normalized) {
                let same_drive = root
                    .get(..2)
                    .is_some_and(|current| current.eq_ignore_ascii_case(&drive));
                if !same_drive {
                    let mut base =
                        drive_cwd(&info.drive_cwds, &drive).unwrap_or_else(|| root.clone());
                    let mismatched = base
                        .get(..2)
                        .is_none_or(|current| !current.eq_ignore_ascii_case(&drive));
                    if mismatched && base.get(2..3) == Some("\\") {
                        base = format!("{drive}\\");
                    }
                    return Ok(paths.resolve(&base, &normalized));
                }
            }
        }
        Ok(paths.resolve(&root, &normalized))
    }

    /// `absolutePath`: the resolved path, as a `FileError` on failure.
    pub async fn absolute_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        if let Some(aborted) = context.abort_result(path) {
            return Err(aborted);
        }
        self.resolve(path)
            .await
            .map_err(|error| to_file_error(&error, path))
    }

    /// `joinPath`: the remote system's `path.join` of `parts`.
    pub async fn join_path(&self, parts: &[&str], _context: &Context) -> Result<String, FileError> {
        match self.paths().await {
            Ok(paths) => Ok(paths.join(parts)),
            Err(error) => Err(to_file_error(&error, "")),
        }
    }

    /// `fileInfo`: `lstat` of the path.
    pub async fn file_info(&self, path: &str, context: &Context) -> Result<FileInfo, FileError> {
        let paths = self.paths().await.map_err(|e| to_file_error(&e, path))?;
        self.file_op(path, context, false, |resolved| async move {
            let reply = self
                .connection
                .request(
                    "lstat",
                    json!({ "path": resolved }),
                    RequestOptions::default(),
                )
                .await?;
            to_info(&resolved, &reply.json, paths)
                .ok_or_else(|| RemoteError::new("invalid", "Unsupported file type"))
        })
        .await
    }

    /// `exists`: `not_found` is `false`, every other failure is an error.
    pub async fn exists(&self, path: &str, context: &Context) -> Result<bool, FileError> {
        match self.file_info(path, context).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// `canonicalPath`: the daemon's `realpath`.
    pub async fn canonical_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        self.file_op(path, context, false, |resolved| async move {
            let reply = self
                .connection
                .request(
                    "realpath",
                    json!({ "path": resolved }),
                    RequestOptions::default(),
                )
                .await?;
            Ok(reply.json["path"].as_str().unwrap_or(&resolved).to_string())
        })
        .await
    }

    /// Node's `readFile` without UTF-8 decoding, over a file opened like
    /// `open(path, "r")`.
    pub async fn read_binary_file(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Vec<u8>, FileError> {
        self.read_file(path, false, context)
            .await
            .map(|(bytes, _)| bytes)
    }

    /// Node's `readFile(path, "utf8")`.
    pub async fn read_text_file(&self, path: &str, context: &Context) -> Result<String, FileError> {
        self.read_file(path, true, context)
            .await
            .map(|(_, text)| text)
    }

    /// `_readFile`: the whole file in reads of at most `READ_FILE_CHUNK`, or
    /// sequential reads when the size is unknown (FIFOs, devices).
    async fn read_file(
        &self,
        path: &str,
        encoding: bool,
        context: &Context,
    ) -> Result<(Vec<u8>, String), FileError> {
        let signal = context.signal.clone();
        let aborted = move || signal.as_ref().is_some_and(AbortSignal::is_aborted);
        self.file_op(path, context, false, |resolved| async move {
            let opened = self.open(resolved, json!({ "mode": "read" })).await?;
            let handle = opened.handle;
            let result = read_opened(&handle, &opened.json, encoding, &aborted).await;
            let _ = handle.close().await;
            result
        })
        .await
    }

    /// Node's `writeFile`: the first request creates parents, opens and writes
    /// the first chunk; later chunks go to the same open file.
    pub async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        self.write(path, content, false, context).await
    }

    /// Node's `appendFile`.
    pub async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        self.write(path, content, true, context).await
    }

    async fn write(
        &self,
        path: &str,
        bytes: &[u8],
        append: bool,
        context: &Context,
    ) -> Result<(), FileError> {
        let first = bytes.len().min(WRITE_CHUNK);
        let keep = bytes.len() > WRITE_CHUNK;
        let content = bytes.to_vec();
        let signal = if append { None } else { context.signal.clone() };
        self.file_op(path, context, append, |resolved| async move {
            let reply = self
                .connection
                .request(
                    "write",
                    json!({ "path": resolved, "append": append, "keep": keep }),
                    RequestOptions {
                        payload: content[..first].to_vec(),
                        ..RequestOptions::default()
                    },
                )
                .await?;
            if !keep {
                return Ok(());
            }
            let handle = Handle {
                connection: self.connection.clone(),
                id: reply.json["handle"].as_u64().unwrap_or(0),
                session: reply.session,
                path: resolved.clone(),
            };
            let result = async {
                let mut offset = WRITE_CHUNK;
                while offset < content.len() {
                    if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
                        return Err(RemoteError::new("aborted", "The operation was aborted"));
                    }
                    let end = (offset + WRITE_CHUNK).min(content.len());
                    // The daemon refuses later chunks after a failed one, so a
                    // poisoned write handle cannot leave a gap.
                    handle
                        .request(
                            "writeChunk",
                            json!({}),
                            RequestOptions {
                                payload: content[offset..end].to_vec(),
                                ..RequestOptions::default()
                            },
                        )
                        .await?;
                    offset = end;
                }
                Ok(())
            }
            .await;
            let _ = handle.close().await;
            result
        })
        .await
    }

    pub async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        context: &Context,
    ) -> Result<(), FileError> {
        self.file_op(path, context, true, |resolved| async move {
            self.connection
                .request(
                    "truncate",
                    json!({ "path": resolved, "size": size }),
                    RequestOptions::default(),
                )
                .await?;
            Ok(())
        })
        .await
    }

    pub async fn flush_file(&self, path: &str, context: &Context) -> Result<(), FileError> {
        self.file_op(path, context, true, |resolved| async move {
            self.connection
                .request(
                    "fsync",
                    json!({ "path": resolved }),
                    RequestOptions::default(),
                )
                .await?;
            Ok(())
        })
        .await
    }

    pub async fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
        context: &Context,
    ) -> Result<(), FileError> {
        let (source, destination) =
            tokio::join!(self.resolve(source_path), self.resolve(destination_path));
        let (source, destination) = match (source, destination) {
            (Ok(source), Ok(destination)) => (source, destination),
            (Err(error), _) | (_, Err(error)) => {
                return Err(to_file_error(&error, source_path));
            }
        };
        if let Some(aborted) = context.abort_result(&destination) {
            return Err(aborted);
        }
        self.connection
            .request(
                "rename",
                json!({ "path": source, "to": destination }),
                RequestOptions::default(),
            )
            .await
            .map(|_| ())
            .map_err(|error| to_file_error(&error, &source))
    }

    pub async fn create_dir(
        &self,
        path: &str,
        recursive: Option<bool>,
        context: &Context,
    ) -> Result<(), FileError> {
        self.file_op(path, context, false, |resolved| async move {
            self.connection
                .request(
                    "mkdir",
                    json!({ "path": resolved, "recursive": recursive.unwrap_or(true) }),
                    RequestOptions::default(),
                )
                .await?;
            Ok(())
        })
        .await
    }

    pub async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> Result<(), FileError> {
        self.file_op(path, context, false, |resolved| async move {
            self.connection
                .request(
                    "rm",
                    json!({ "path": resolved, "recursive": recursive, "force": force }),
                    RequestOptions::default(),
                )
                .await?;
            Ok(())
        })
        .await
    }

    pub async fn create_temp_dir(
        &self,
        prefix: &str,
        context: &Context,
    ) -> Result<String, FileError> {
        if let Some(aborted) = context.abort_result("") {
            return Err(aborted);
        }
        let info = self.info().await.map_err(|e| to_file_error(&e, ""))?;
        let paths = self.paths().await.map_err(|e| to_file_error(&e, ""))?;
        let reply = self
            .connection
            .request(
                "mkdtemp",
                json!({ "path": paths.join(&[&info.tmpdir, prefix]) }),
                RequestOptions::default(),
            )
            .await
            .map_err(|error| to_file_error(&error, ""))?;
        Ok(reply.json["path"].as_str().unwrap_or_default().to_string())
    }

    pub async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        context: &Context,
    ) -> Result<String, FileError> {
        let directory = self.create_temp_dir("tmp-", context).await?;
        let paths = self
            .paths()
            .await
            .map_err(|e| to_file_error(&e, &directory))?;
        let file_path = paths.join(&[
            &directory,
            &format!("{prefix}{}{suffix}", uuid::Uuid::new_v4()),
        ]);
        // Node's `writeFile(filePath, "")`, which does not create parents.
        self.connection
            .request(
                "write",
                json!({ "path": file_path, "append": false, "parents": false }),
                RequestOptions::default(),
            )
            .await
            .map_err(|error| to_file_error(&error, &file_path))?;
        Ok(file_path)
    }

    /// `listDir`: every entry of a directory, its `lstat` information included.
    pub async fn list_dir(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Vec<FileInfo>, FileError> {
        let paths = self.paths().await.map_err(|e| to_file_error(&e, path))?;
        self.file_op(path, context, false, |resolved| async move {
            let reply = self
                .connection
                .request(
                    "opendir",
                    json!({ "path": resolved }),
                    RequestOptions::default(),
                )
                .await?;
            let handle = Handle {
                connection: self.connection.clone(),
                id: reply.json["handle"].as_u64().unwrap_or(0),
                session: reply.session,
                path: resolved.clone(),
            };
            let result = list_dir_entries(&handle, &resolved, paths, context).await;
            let _ = handle.close().await;
            result
        })
        .await
    }

    /// `listFiles` through the daemon; `not_supported` with a daemon released
    /// before the `listFiles` feature.
    pub async fn list_files(
        &self,
        path: &str,
        options: &ListFilesOptions,
        context: &Context,
    ) -> Result<FileListing, FileError> {
        for (key, value) in [
            ("maxEntries", options.max_entries),
            ("maxBytes", options.max_bytes),
            ("timeoutMs", options.timeout_ms),
        ] {
            if value.is_some_and(|value| value > MAX_SAFE_INTEGER) {
                return Err(FileError::new(
                    FileErrorCode::Invalid,
                    format!("{key} must be a non-negative safe integer"),
                    path.to_string(),
                ));
            }
        }
        let resolved = self
            .resolve(path)
            .await
            .map_err(|error| to_file_error(&error, path))?;
        if let Some(aborted) = context.abort_result(&resolved) {
            return Err(aborted);
        }
        let info = self
            .connection
            .info()
            .await
            .map_err(|error| to_file_error(&error, &resolved))?;
        if !info.has_feature("listFiles") {
            return Err(FileError::new(
                FileErrorCode::NotSupported,
                "The pi-env daemon does not list files; update it",
                resolved.clone(),
            ));
        }
        // Fields left `None` are not sent; the daemon applies the defaults.
        let mut fields = Map::new();
        fields.insert("path".to_string(), Value::String(resolved.clone()));
        if let Some(hidden) = options.hidden {
            fields.insert("hidden".to_string(), Value::Bool(hidden));
        }
        if let Some(gitignore) = options.gitignore {
            fields.insert("gitignore".to_string(), Value::Bool(gitignore));
        }
        if let Some(exclude) = &options.exclude {
            fields.insert("exclude".to_string(), json!(exclude));
        }
        if let Some(max_entries) = options.max_entries {
            fields.insert("maxEntries".to_string(), json!(max_entries));
        }
        if let Some(max_bytes) = options.max_bytes {
            fields.insert("maxBytes".to_string(), json!(max_bytes));
        }
        if let Some(timeout_ms) = options.timeout_ms {
            fields.insert("timeoutMs".to_string(), json!(timeout_ms));
        }
        let reply = match self
            .connection
            .request(
                "listFiles",
                Value::Object(fields),
                RequestOptions {
                    signal: context.signal.clone(),
                    ..RequestOptions::default()
                },
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => return Err(to_file_error(&error, &resolved)),
        };
        if let Some(aborted) = context.abort_result(&resolved) {
            return Err(aborted);
        }
        Ok(FileListing {
            entries: decode_listing(&reply.payload),
            truncated: reply.json["truncated"].as_bool() == Some(true),
        })
    }

    /// Run a shell string or an argv array on the remote machine, with `event`
    /// frames delivered to `options.on_output` while it runs. Aborting the
    /// context cancels the request (the daemon answers `aborted`).
    pub async fn exec(
        &self,
        command: &ExecCommand,
        options: &ExecOptions,
        context: &Context,
    ) -> Result<ExecResult, ExecutionError> {
        let signal = context.signal.clone();
        if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        if let Some(timeout) = options.timeout_secs {
            if !timeout.is_finite() || timeout <= 0.0 {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: must be a finite number of seconds",
                ));
            }
            if timeout * 1000.0 > MAX_TIMEOUT_MS {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    format!(
                        "Invalid timeout: maximum is {} seconds",
                        MAX_TIMEOUT_MS / 1000.0
                    ),
                ));
            }
        }
        let inherit_env = options.inherit_env.unwrap_or(true);
        let cwd = match &options.cwd {
            Some(cwd) => self.resolve(cwd).await,
            None => self.root().await,
        }
        .map_err(|error| ExecutionError::new(ExecutionErrorCode::Unknown, error.message.clone()))?;
        if signal.as_ref().is_some_and(AbortSignal::is_aborted) {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        // The daemon builds the remote process's environment: the inherited
        // environment, with this call's variables over it (a variable set to
        // `None` removes an inherited one from the request).
        //
        // A *local* daemon inherited this process's environment when it
        // started, but it outlives any number of environment changes, so the
        // live environment is forwarded with each inheriting call. That keeps
        // `inheritEnv` matching what a local command would see without
        // restarting the daemon. For an SSH connection the daemon's host is
        // another machine: its own environment is the base, and this process's
        // variables are not forwarded.
        //
        // Deliberate filtering of the forwarded snapshot: only `KEY=VALUE`
        // pairs that are valid Unicode cross the JSON wire (both halves are
        // UTF-8 strings in the protocol), and Windows' drive-cwd
        // pseudo-variables (`=C:`) are left out because they are not names a
        // child process accepts. The daemon itself applies no filtering: it
        // inherits its whole environment as upstream does.
        let mut env: Map<String, Value> = Map::new();
        if inherit_env {
            if self.connection.is_local() {
                for (key, value) in std::env::vars_os() {
                    let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
                        continue;
                    };
                    if key.is_empty() || key.starts_with('=') {
                        continue;
                    }
                    env.insert(key.to_string(), Value::String(value.to_string()));
                }
            }
            for (key, value) in &self.shell_env {
                env.insert(key.clone(), Value::String(value.clone()));
            }
        }
        for (key, value) in &options.env {
            match value {
                Some(value) => {
                    env.insert(key.clone(), Value::String(value.clone()));
                }
                None => {
                    env.remove(key);
                }
            }
        }
        let mut fields = Map::new();
        match command {
            ExecCommand::Shell(text) => {
                fields.insert("command".to_string(), Value::String(text.clone()));
            }
            ExecCommand::Argv(argv) => {
                fields.insert("argv".to_string(), json!(argv));
            }
        }
        fields.insert("cwd".to_string(), Value::String(cwd));
        fields.insert("env".to_string(), Value::Object(env));
        fields.insert("inheritEnv".to_string(), Value::Bool(inherit_env));
        if let Some(shell_path) = &self.shell_path {
            fields.insert("shellPath".to_string(), Value::String(shell_path.clone()));
        }
        if let Some(timeout) = options.timeout_secs {
            fields.insert("timeoutMs".to_string(), json!(timeout * 1000.0));
        }
        if let Some(spill) = &options.spill {
            fields.insert("spill".to_string(), spill.clone());
        }
        if let Some(window) = &options.window {
            fields.insert("window".to_string(), window.clone());
        }
        let on_event: Option<EventFn> = options.on_output.clone().map(|on_output| {
            Arc::new(move |event: &Value, payload: &[u8]| {
                if event["kind"] != "output" {
                    return;
                }
                let text = String::from_utf8_lossy(payload).into_owned();
                if text.is_empty() {
                    return;
                }
                on_output(
                    &text,
                    OutputInfo {
                        stream: if event["stream"] == "stderr" {
                            "stderr"
                        } else {
                            "stdout"
                        },
                        skipped: event.get("skipped").cloned(),
                    },
                );
            }) as EventFn
        });
        let running = self.running.clone();
        let on_start = Arc::new(move |request_id: u32, session: u32| {
            lock(&running).insert(request_id, session);
        });
        let result = self
            .connection
            .request(
                "exec",
                Value::Object(fields),
                RequestOptions {
                    signal,
                    on_event,
                    on_start: Some(on_start),
                    payload: Vec::new(),
                    session: None,
                },
            )
            .await;
        match result {
            Ok(reply) => {
                lock(&self.running).remove(&reply.request_id);
                Ok(ExecResult {
                    exit_code: reply.json["exitCode"].as_i64().unwrap_or(0),
                    spill_path: reply
                        .json
                        .get("spillPath")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            }
            Err(error) => Err(ExecutionError::from_remote(&error, options.timeout_secs)),
        }
    }

    /// `watch`: the daemon's `NodeFileWatcher` next to the files.
    pub async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchChangeFn,
        context: &Context,
    ) -> Result<Arc<RemoteWatcher>, FileError> {
        if let Some(aborted) = context.abort_result("") {
            return Err(aborted);
        }
        let mut resolved = Vec::with_capacity(targets.len());
        for target in targets {
            let path = self
                .resolve(&target.path)
                .await
                .map_err(|error| to_file_error(&error, &target.path))?;
            resolved.push(WatchTarget {
                path,
                recursive: target.recursive,
                exclude: target.exclude.clone(),
            });
        }
        if let Some(aborted) = context.abort_result("") {
            return Err(aborted);
        }
        RemoteWatcher::open(
            self.connection.clone(),
            resolved,
            on_change,
            self.watch_options.clone(),
        )
        .await
        .map_err(|error| to_file_error(&error, ""))
    }

    /// Kill every running command without aborting it, so the commands settle
    /// with their killed status, as `NodeExecutionEnv` does.
    pub async fn cleanup(&self, _context: &Context) {
        let running: Vec<(u32, u32)> = lock(&self.running).drain().collect();
        for (request_id, session) in running {
            self.connection.kill(request_id, session);
        }
    }

    /// `_fileOp`: resolve, check for an abort, run, and map failures.
    async fn file_op<T, F, Fut>(
        &self,
        path: &str,
        context: &Context,
        check_after: bool,
        run: F,
    ) -> Result<T, FileError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, RemoteError>>,
    {
        let resolved = match self.resolve(path).await {
            Ok(resolved) => resolved,
            Err(error) => return Err(to_file_error(&error, path)),
        };
        if let Some(aborted) = context.abort_result(&resolved) {
            return Err(aborted);
        }
        match run(resolved.clone()).await {
            Ok(value) => {
                if check_after {
                    if let Some(aborted) = context.abort_result(&resolved) {
                        return Err(aborted);
                    }
                }
                Ok(value)
            }
            Err(error) => Err(to_file_error(&error, &resolved)),
        }
    }

    /// `_open`: open one file and keep its daemon handle.
    async fn open(&self, resolved: String, json: Value) -> Result<Opened, RemoteError> {
        let mut fields = match json {
            Value::Object(fields) => fields,
            _ => Map::new(),
        };
        fields.insert("path".to_string(), Value::String(resolved.clone()));
        let reply = self
            .connection
            .request("open", Value::Object(fields), RequestOptions::default())
            .await?;
        Ok(Opened {
            handle: Handle {
                connection: self.connection.clone(),
                id: reply.json["handle"].as_u64().unwrap_or(0),
                session: reply.session,
                path: resolved,
            },
            json: reply.json,
        })
    }

    /// `openBinaryReader` (with `noFollow`; upstream's `options?.noFollow`):
    /// a handle to one opened regular file, for positional reads and one-pass
    /// line scans. Directories fail with `is_directory`, other non-regular
    /// files with `invalid`, and with `no_follow` a final symbolic link with
    /// `invalid` too (the daemon's `SYMLINK`).
    pub async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: bool,
        context: &Context,
    ) -> Result<BinaryReader, FileError> {
        let handle = self
            .file_op(path, context, false, |resolved| async move {
                self.open(resolved, json!({ "noFollow": no_follow }))
                    .await
                    .map(|opened| opened.handle)
            })
            .await?;
        if let Some(aborted) = context.abort_result(&handle.path) {
            let _ = handle.close().await;
            return Err(aborted);
        }
        Ok(BinaryReader::new(handle))
    }

    /// `openTextLineReader`: a handle to a file opened like `open(path, "r")`,
    /// read line by line.
    pub async fn open_text_line_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<TextLineReader, FileError> {
        let handle = self
            .file_op(path, context, false, |resolved| async move {
                self.open(resolved, json!({ "mode": "read" }))
                    .await
                    .map(|opened| opened.handle)
            })
            .await?;
        if let Some(aborted) = context.abort_result(&handle.path) {
            let _ = handle.close().await;
            return Err(aborted);
        }
        Ok(TextLineReader::new(handle))
    }

    /// `readTextLines`: up to `max_lines` lines (`None` reads to the end of the
    /// file). A `max_lines` of 0 reads nothing, as upstream returns `[]`.
    pub async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<u64>,
        context: &Context,
    ) -> Result<Vec<String>, FileError> {
        if max_lines == Some(0) {
            return Ok(Vec::new());
        }
        let mut reader = self.open_text_line_reader(path, context).await?;
        // Upstream's `try/finally`: the reader is closed whatever the loop does.
        let lines: Result<Vec<String>, FileError> = async {
            let mut lines: Vec<String> = Vec::new();
            while max_lines.is_none_or(|max_lines| (lines.len() as u64) < max_lines) {
                match reader.read_line(context).await? {
                    Some(line) => lines.push(line.text),
                    None => break,
                }
            }
            Ok(lines)
        }
        .await;
        reader.close().await;
        lines
    }

    /// `openDirReader`: paged entries of one directory, metadata read only for
    /// returned entries.
    pub async fn open_dir_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<DirReader, FileError> {
        let handle = self
            .file_op(path, context, false, |resolved| async move {
                let reply = self
                    .connection
                    .request(
                        "opendir",
                        json!({ "path": resolved }),
                        RequestOptions::default(),
                    )
                    .await?;
                Ok(Handle {
                    connection: self.connection.clone(),
                    id: reply.json["handle"].as_u64().unwrap_or(0),
                    session: reply.session,
                    path: resolved,
                })
            })
            .await?;
        if let Some(aborted) = context.abort_result(&handle.path) {
            let _ = handle.close().await;
            return Err(aborted);
        }
        Ok(DirReader::new(handle))
    }
}

struct Opened {
    handle: Handle,
    json: Value,
}

/// A daemon handle: valid only in the session that opened it.
struct Handle {
    connection: Connection,
    id: u64,
    session: u32,
    /// The resolved path the handle was opened on.
    path: String,
}

impl Handle {
    async fn request(
        &self,
        op: &str,
        json: Value,
        options: RequestOptions,
    ) -> Result<Reply, RemoteError> {
        let mut fields = match json {
            Value::Object(fields) => fields,
            _ => Map::new(),
        };
        fields.insert("handle".to_string(), json!(self.id));
        self.connection
            .request(
                op,
                Value::Object(fields),
                RequestOptions {
                    session: Some(self.session),
                    ..options
                },
            )
            .await
    }

    async fn pread(&self, offset: Option<u64>, length: usize) -> Result<Vec<u8>, RemoteError> {
        let json = match offset {
            Some(offset) => json!({ "offset": offset, "length": length }),
            None => json!({ "length": length }),
        };
        Ok(self
            .request("pread", json, RequestOptions::default())
            .await?
            .payload)
    }

    async fn close(&self) -> Result<(), RemoteError> {
        self.request("close", json!({}), RequestOptions::default())
            .await
            .map(|_| ())
    }
}

/// `remotePath(connection)`: the remote system's path rules, from its `hello`.
/// Upstream's readers reach the same function through `handle.env.connection`.
async fn remote_paths(connection: &Connection) -> Result<RemotePathFlavor, RemoteError> {
    let (platform, _) = connection.info().await?.node_platform();
    Ok(RemotePathFlavor::for_platform(&platform))
}

/// Positional reads of one opened regular file (`openBinaryReader`): upstream
/// `RemoteBinaryReader`. Every call sees the same file even if its path is
/// renamed, because each request carries the daemon handle.
pub struct BinaryReader {
    handle: Handle,
    closed: bool,
}

impl BinaryReader {
    fn new(handle: Handle) -> BinaryReader {
        BinaryReader {
            handle,
            closed: false,
        }
    }

    /// The resolved path the reader was opened on.
    pub fn path(&self) -> &str {
        &self.handle.path
    }

    /// `#closedResult`: the closed-reader error, while it is closed.
    fn closed_result(&self) -> Option<FileError> {
        self.closed.then(|| {
            FileError::new(
                FileErrorCode::Invalid,
                "Binary reader is closed",
                self.handle.path.clone(),
            )
        })
    }

    /// Metadata of the opened file, not of whatever its path names now.
    pub async fn info(&self, context: &Context) -> Result<FileInfo, FileError> {
        if let Some(early) = context
            .abort_result(self.path())
            .or_else(|| self.closed_result())
        {
            return Err(early);
        }
        let reply = match self
            .handle
            .request("fstat", json!({}), RequestOptions::default())
            .await
        {
            Ok(reply) => reply,
            Err(error) => return Err(to_file_error(&error, self.path())),
        };
        let paths = remote_paths(&self.handle.connection)
            .await
            .map_err(|error| to_file_error(&error, self.path()))?;
        to_info(self.path(), &reply.json, paths).ok_or_else(|| {
            FileError::new(
                FileErrorCode::Invalid,
                "Unsupported file type",
                self.path().to_string(),
            )
        })
    }

    /// Up to `length` bytes at `offset`; fewer only at the end of the file.
    pub async fn read(
        &self,
        offset: u64,
        length: u64,
        context: &Context,
    ) -> Result<Vec<u8>, FileError> {
        if let Some(early) = context
            .abort_result(self.path())
            .or_else(|| self.closed_result())
        {
            return Err(early);
        }
        // `Number.isSafeInteger(offset) && offset >= 0`, the same for `length`:
        // a `u64` parameter cannot be negative or fractional, so the upper bound
        // is what is left to check.
        let invalid = || {
            FileError::new(
                FileErrorCode::Invalid,
                "Offset and length must be non-negative safe integers",
                self.path().to_string(),
            )
        };
        if offset > MAX_SAFE_INTEGER || length > MAX_SAFE_INTEGER {
            return Err(invalid());
        }
        // A 32-bit target cannot address every safe integer; the daemon could
        // not answer such an offset anyway.
        let (Ok(offset), Ok(length)) = (usize::try_from(offset), usize::try_from(length)) else {
            return Err(invalid());
        };
        let signal = context.signal.clone();
        let aborted = move || signal.as_ref().is_some_and(AbortSignal::is_aborted);
        match read_range(&self.handle, offset, length, READ_CHUNK, &aborted).await {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(FileError::new(
                FileErrorCode::Aborted,
                "aborted",
                self.path().to_string(),
            )),
            Err(error) => Err(to_file_error(&error, self.path())),
        }
    }

    /// One pass over the file that locates lines `[start_line, end_line)`
    /// (`end_line` `None`: to the end), 0-based, where line `k` starts after
    /// the `k`-th newline byte.
    pub async fn scan_lines(
        &self,
        start_line: u64,
        end_line: Option<u64>,
        context: &Context,
    ) -> Result<LineScan, FileError> {
        if let Some(early) = context
            .abort_result(self.path())
            .or_else(|| self.closed_result())
        {
            return Err(early);
        }
        let invalid_range = || {
            FileError::new(
                FileErrorCode::Invalid,
                "Invalid line range",
                self.path().to_string(),
            )
        };
        if start_line > MAX_SAFE_INTEGER
            || end_line.is_some_and(|end| end <= start_line || end > MAX_SAFE_INTEGER)
        {
            return Err(invalid_range());
        }
        let json = match end_line {
            Some(end_line) => json!({ "startLine": start_line, "endLine": end_line }),
            None => json!({ "startLine": start_line }),
        };
        match self
            .handle
            .request(
                "scanLines",
                json,
                RequestOptions {
                    signal: context.signal.clone(),
                    ..RequestOptions::default()
                },
            )
            .await
        {
            Ok(reply) => Ok(LineScan::from_json(&reply.json)),
            Err(error) => Err(to_file_error(&error, self.path())),
        }
    }

    /// Close the daemon handle; idempotent. A close failure is dropped, as
    /// upstream's `Handle.close` swallows it.
    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.handle.close().await;
    }
}

/// Pages of one directory's entries (`openDirReader`): upstream
/// `RemoteDirReader`. Entries come in the order the remote file system returns
/// them, and one that disappeared before its metadata was read is skipped.
pub struct DirReader {
    handle: Handle,
    done: bool,
    closed: bool,
}

impl DirReader {
    fn new(handle: Handle) -> DirReader {
        DirReader {
            handle,
            done: false,
            closed: false,
        }
    }

    /// The resolved path of the directory being read.
    pub fn path(&self) -> &str {
        &self.handle.path
    }

    /// Up to `max_entries` entries, continuing where the previous call stopped;
    /// `done` marks the end. After a failed or aborted call, close the reader.
    pub async fn next(
        &mut self,
        max_entries: u64,
        context: &Context,
    ) -> Result<DirPage, FileError> {
        let path = self.handle.path.clone();
        if let Some(aborted) = context.abort_result(&path) {
            return Err(aborted);
        }
        if self.closed {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Directory reader is closed",
                path,
            ));
        }
        if max_entries == 0 || max_entries > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "maxEntries must be a positive safe integer",
                path,
            ));
        }
        let paths = match remote_paths(&self.handle.connection).await {
            Ok(paths) => paths,
            Err(error) => return Err(to_file_error(&error, &path)),
        };
        let mut entries: Vec<FileInfo> = Vec::new();
        // Like `NodeDirReader`, count only entries that are part of the listing.
        while !self.done && (entries.len() as u64) < max_entries {
            let reply = match self
                .handle
                .request(
                    "readdir",
                    json!({ "max": max_entries - entries.len() as u64 }),
                    RequestOptions::default(),
                )
                .await
            {
                Ok(reply) => reply,
                Err(error) => return Err(to_file_error(&error, &path)),
            };
            if let Some(aborted) = context.abort_result(&path) {
                return Err(aborted);
            }
            if let Some(page) = reply.json["entries"].as_array() {
                for entry in page {
                    let name = entry["name"].as_str().unwrap_or_default();
                    let entry_path = paths.resolve(&path, name);
                    if let Some(error) = entry.get("error") {
                        // Removed between enumeration and lstat: not part of the
                        // listing any more.
                        if error["code"].as_str() == Some("ENOENT") {
                            continue;
                        }
                        return Err(to_file_error(&RemoteError::from_json(error), &entry_path));
                    }
                    if let Some(info) = to_info(&entry_path, &entry["info"], paths) {
                        entries.push(info);
                    }
                }
            }
            self.done = reply.json["done"] == Value::Bool(true);
        }
        Ok(DirPage {
            entries,
            done: self.done,
        })
    }

    /// Close the daemon handle; idempotent, a close failure dropped, as
    /// [`BinaryReader::close`].
    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.handle.close().await;
    }
}

/// Lines of a text file, read positionally (`openTextLineReader`): upstream
/// `RemoteTextLineReader`, `NodeTextLineReader`'s rules over a file opened like
/// `open(path, "r")`.
pub struct TextLineReader {
    handle: Handle,
    /// Durable's streaming decoder: a leading byte-order mark is dropped, and an
    /// incomplete character is held back until its remaining bytes arrive.
    decoder: StreamDecoder,
    /// The next positional read's offset; the daemon's file position is not
    /// relied on, so this is the reader's own.
    offset: u64,
    buffered: TextLineBuffer,
    ended: bool,
    closed: bool,
}

/// A line-splitting buffer for [`TextLineReader`]: chunks are appended and
/// lines are taken from a cursor, so a burst of short lines does not copy the
/// whole remainder of the buffer once per line (the previous
/// `buffered = buffered[newline+1..].to_string()` was O(n²) per chunk). The
/// consumed prefix is compacted in amortized O(1) only once it is both large
/// (64 KiB) and at least half the buffer.
///
/// Public (hidden) so the copy behavior can be pinned without a daemon.
#[derive(Default)]
#[doc(hidden)]
pub struct TextLineBuffer {
    text: String,
    cursor: usize,
    compacted_bytes: usize,
}

/// Compact the consumed prefix once it reaches this size (and at least half
/// the buffer), so compaction is amortized linear.
const LINE_BUFFER_COMPACT_THRESHOLD: usize = 64 * 1024;

impl TextLineBuffer {
    pub fn new() -> TextLineBuffer {
        TextLineBuffer::default()
    }

    /// Append one decoded chunk.
    pub fn push_str(&mut self, chunk: &str) {
        self.text.push_str(chunk);
    }

    /// The next newline-terminated line (newline excluded), or `None` when the
    /// buffer holds no complete line yet.
    pub fn next_line(&mut self) -> Option<String> {
        let relative = self.text[self.cursor..].find('\n')?;
        let newline = self.cursor + relative;
        let line = self.text[self.cursor..newline].to_string();
        self.cursor = newline + 1;
        self.compact();
        Some(line)
    }

    /// The remaining text at end of input (`None` when nothing is left).
    pub fn take_rest(&mut self) -> Option<String> {
        if self.cursor >= self.text.len() {
            return None;
        }
        let rest = self.text[self.cursor..].to_string();
        self.text.clear();
        self.cursor = 0;
        Some(rest)
    }

    /// Bytes moved by compaction so far (the linear-cost regression seam).
    pub fn compacted_bytes(&self) -> usize {
        self.compacted_bytes
    }

    fn compact(&mut self) {
        if self.cursor >= LINE_BUFFER_COMPACT_THRESHOLD && self.cursor * 2 >= self.text.len() {
            self.compacted_bytes += self.text.len() - self.cursor;
            self.text.drain(..self.cursor);
            self.cursor = 0;
        }
    }
}

impl TextLineReader {
    fn new(handle: Handle) -> TextLineReader {
        TextLineReader {
            handle,
            decoder: StreamDecoder::new(),
            offset: 0,
            buffered: TextLineBuffer::new(),
            ended: false,
            closed: false,
        }
    }

    /// The resolved path the reader was opened on.
    pub fn path(&self) -> &str {
        &self.handle.path
    }

    /// The next line, or `None` at the end of the file. The text excludes the
    /// newline that ends the line; the last line of a file that does not end
    /// with a newline is reported unterminated.
    pub async fn read_line(&mut self, context: &Context) -> Result<Option<TextLine>, FileError> {
        let path = self.handle.path.clone();
        if let Some(aborted) = context.abort_result(&path) {
            return Err(aborted);
        }
        if self.closed {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Text line reader is closed",
                path,
            ));
        }
        loop {
            if let Some(text) = self.buffered.next_line() {
                return Ok(Some(TextLine {
                    text,
                    terminated: true,
                }));
            }
            if self.ended {
                let Some(text) = self.buffered.take_rest() else {
                    return Ok(None);
                };
                return Ok(Some(TextLine {
                    text,
                    terminated: false,
                }));
            }
            let bytes = match self.handle.pread(Some(self.offset), LINE_CHUNK).await {
                Ok(bytes) => bytes,
                Err(error) => return Err(to_file_error(&error, &path)),
            };
            if let Some(aborted) = context.abort_result(&path) {
                return Err(aborted);
            }
            self.offset += bytes.len() as u64;
            if bytes.is_empty() {
                // The end of the stream: flush a character the decoder held back.
                self.buffered.push_str(&self.decoder.decode(&[], true));
                self.ended = true;
            } else {
                self.buffered.push_str(&self.decoder.decode(&bytes, false));
            }
        }
    }

    /// Close the daemon handle and drop the buffer; idempotent.
    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.buffered = TextLineBuffer::new();
        let _ = self.handle.close().await;
    }
}

/// `_readFile`'s body over an opened handle.
async fn read_opened(
    handle: &Handle,
    opened: &Value,
    encoding: bool,
    aborted: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(Vec<u8>, String), RemoteError> {
    let abort = || RemoteError::new("aborted", "The operation was aborted");
    if let Some(error) = opened.get("statError") {
        return Err(RemoteError::from_json(error));
    }
    if aborted() {
        return Err(abort());
    }
    let stat = opened.get("stat").cloned().unwrap_or(Value::Null);
    let size = if stat["kind"] == "file" {
        stat["size"].as_u64().unwrap_or(0)
    } else {
        0
    };
    if size > READ_FILE_MAX {
        return Err(RemoteError::new(
            "unknown",
            format!("File size ({size}) is greater than 2 GiB"),
        ));
    }
    if size == 0 {
        // Unknown size: sequential reads until the end, as for FIFOs and devices.
        // `StringDecoder("utf8")` does not strip a BOM and holds an incomplete
        // character across reads; the daemon's BOM-removing TextDecoder (used
        // for line reads) would be wrong here.
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut decoder = range_decoder();
        let mut text = String::new();
        loop {
            if aborted() {
                return Err(abort());
            }
            let bytes = handle.pread(None, READ_FILE_UNKNOWN_CHUNK).await?;
            if bytes.is_empty() {
                break;
            }
            if encoding {
                text.push_str(&decode_into(&mut decoder, &bytes, false));
            } else {
                chunks.push(bytes);
            }
        }
        if encoding {
            text.push_str(&decode_into(&mut decoder, &[], true));
            return Ok((Vec::new(), text));
        }
        return Ok((concat(&chunks), String::new()));
    }
    if !encoding {
        // A single read of the whole size, or reads of 512 KiB until the size
        // or the end of the file.
        if size <= READ_FILE_CHUNK as u64 {
            return Ok((handle.pread(Some(0), size as usize).await?, String::new()));
        }
        let bytes = read_range(handle, 0, size as usize, READ_FILE_CHUNK, aborted)
            .await?
            .ok_or_else(abort)?;
        return Ok((bytes, String::new()));
    }
    // Decoding: reads of min(size, 512 KiB) until the size, a short read, or the
    // end of the file.
    let length = (size as usize).min(READ_FILE_CHUNK);
    // Node's `StringDecoder("utf8")` starts at the first chunk: a multi-byte
    // character split between the first and second read is held back, not
    // replaced at each side of the boundary. It also keeps a leading BOM
    // (this is `readFile`, not a TextDecoder line stream).
    let mut decoder = range_decoder();
    let mut text = String::new();
    let mut total = 0usize;
    let mut first = true;
    let mut next = 0usize;
    loop {
        let batch: Vec<(usize, usize)> = (0..READ_DEPTH)
            .map(|index| (next + index * length, length))
            .collect();
        next += READ_DEPTH * length;
        for (offset, requested) in batch {
            let bytes = handle.pread(Some(offset as u64), requested).await?;
            if aborted() {
                return Err(abort());
            }
            total += bytes.len();
            if bytes.is_empty() || total >= size as usize || bytes.len() != requested {
                if first {
                    // `Buffer.from(bytes).toString("utf8")`: the only chunk is
                    // decoded as a whole, exactly like a short first read.
                    return Ok((Vec::new(), String::from_utf8_lossy(&bytes).into_owned()));
                }
                text.push_str(&decode_into(&mut decoder, &bytes, false));
                text.push_str(&decode_into(&mut decoder, &[], true));
                return Ok((Vec::new(), text));
            }
            text.push_str(&decode_into(&mut decoder, &bytes, false));
            first = false;
        }
    }
}

/// Consecutive reads of at most `chunk` bytes, several in flight, ending early
/// at the end of the file. `None` means the signal was aborted.
async fn read_range(
    handle: &Handle,
    offset: usize,
    length: usize,
    chunk: usize,
    aborted: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Option<Vec<u8>>, RemoteError> {
    let end = offset + length;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    let mut next = offset;
    while total < length {
        let mut batch: Vec<(usize, usize)> = Vec::new();
        while batch.len() < READ_DEPTH && next < end {
            let size = (end - next).min(chunk);
            batch.push((next, size));
            next += size;
        }
        if batch.is_empty() {
            break;
        }
        let reads = futures_util::future::join_all(
            batch
                .iter()
                .map(|(at, size)| handle.pread(Some(*at as u64), *size)),
        )
        .await;
        let mut short = false;
        for ((_, requested), bytes) in batch.iter().zip(reads) {
            if aborted() {
                return Ok(None);
            }
            let bytes = bytes?;
            if bytes.is_empty() {
                return Ok(Some(concat(&chunks)));
            }
            let read = bytes.len();
            chunks.push(bytes);
            total += read;
            if read < *requested {
                // Reads already in flight assumed a full read; continue right
                // after the read that ended early.
                short = true;
                break;
            }
        }
        if short {
            next = offset + total;
        }
    }
    Ok(Some(concat(&chunks)))
}

/// `listDir`'s entry loop: page `readdir` until done, sort on POSIX, then
/// `lstat` every entry (Node's `readdir` fails on any entry it cannot lstat).
async fn list_dir_entries(
    handle: &Handle,
    resolved: &str,
    paths: RemotePathFlavor,
    context: &Context,
) -> Result<Vec<FileInfo>, RemoteError> {
    let mut entries: Vec<Value> = Vec::new();
    loop {
        let page = handle
            .request(
                "readdir",
                json!({ "max": READDIR_PAGE }),
                RequestOptions::default(),
            )
            .await?;
        if let Some(page_entries) = page.json["entries"].as_array() {
            entries.extend(page_entries.iter().cloned());
        }
        if page.json["done"].as_bool() == Some(true) {
            break;
        }
    }
    // libuv sorts names by their bytes on POSIX; on Windows it keeps the file
    // system's order, which the daemon reports.
    if paths == RemotePathFlavor::Posix {
        // Cached keys: the comparator runs O(n log n) times, and allocating a
        // `Vec<u8>` inside it copied every name once per comparison.
        entries.sort_by_cached_key(|entry| {
            entry
                .get("raw")
                .and_then(Value::as_str)
                .map(|raw| raw.as_bytes().to_vec())
                .unwrap_or_else(|| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .as_bytes()
                        .to_vec()
                })
        });
    }
    let mut infos = Vec::with_capacity(entries.len());
    for entry in &entries {
        if context.aborted() {
            return Err(RemoteError::new("aborted", "aborted"));
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let entry_path = paths.join(&[resolved, name]);
        if let Some(Value::Object(error)) = entry.get("error") {
            return Err(RemoteError::from_json(&Value::Object(error.clone())));
        }
        let info = entry.get("info").cloned().unwrap_or(Value::Null);
        if let Some(info) = to_info(&entry_path, &info, paths) {
            infos.push(info);
        }
    }
    Ok(infos)
}

/// The `listFiles` payload: records of a kind byte, the UTF-8 path, and a NUL.
fn decode_listing(payload: &[u8]) -> Vec<ListedEntry> {
    let mut entries = Vec::new();
    let mut start = 0usize;
    while start < payload.len() {
        let Some(end) = payload[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|index| start + index)
        else {
            break;
        };
        if let Some(kind) = listed_kind(payload[start]) {
            entries.push(ListedEntry {
                path: String::from_utf8_lossy(&payload[start + 1..end]).into_owned(),
                kind,
            });
        }
        start = end + 1;
    }
    entries
}

/// `toInfo`: the daemon's `{ kind, size, mtimeSec, mtimeNsec }` as a `FileInfo`.
fn to_info(path: &str, remote: &Value, paths: RemotePathFlavor) -> Option<FileInfo> {
    let kind = FileKind::parse(remote["kind"].as_str()?);
    if kind == FileKind::Other {
        return None;
    }
    Some(FileInfo {
        name: paths.basename(path),
        path: path.to_string(),
        kind,
        size: remote["size"].as_u64().unwrap_or(0),
        mtime_ms: remote["mtimeSec"].as_f64().unwrap_or(0.0) * 1000.0
            + remote["mtimeNsec"].as_f64().unwrap_or(0.0) / 1e6,
    })
}

/// `concat`: one byte vector of the chunks.
fn concat(chunks: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut bytes = Vec::with_capacity(total);
    for chunk in chunks {
        bytes.extend_from_slice(chunk);
    }
    bytes
}

/// The `C:` drive of a Windows path, when it has one.
fn drive_prefix(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Some(path[..2].to_string());
    }
    None
}
