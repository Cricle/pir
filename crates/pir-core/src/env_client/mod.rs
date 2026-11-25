//! The Rust client half of the pi-env wire: upstream `packages/env/src`
//! (`connection.ts`, `errors.ts`, `list-files.ts`, `remote-env.ts`, `ssh.ts`,
//! `watch.ts`), ported so pir can be the **client** of the same protocol its
//! built-in daemon (`crate::env_daemon`) serves — and of any upstream `pi-env`
//! daemon, since `packages/env/docs/protocol.md` is the contract.
//!
//! The authoritative client-side reference is the JS shim pir already ships
//! (`src/extensions/api/js/pi_env_shim.js.txt`, a statement-by-statement port
//! of those upstream modules); this module mirrors it, with the deltas each
//! file documents.
//!
//! | upstream / shim | here |
//! |---|---|
//! | `Connection`, `isConnectionLost`, `nodePlatform`, `RemoteError` | [`connection`], [`errors`] |
//! | `RemoteExecutionEnv` | [`remote_env::RemoteExecutionEnv`] |
//! | `RemoteBinaryReader`, `RemoteDirReader`, `RemoteTextLineReader` | [`remote_env::BinaryReader`], [`remote_env::DirReader`], [`remote_env::TextLineReader`] |
//! | `RemoteWatcher` | [`watch::RemoteWatcher`] |
//! | `openTerminal`, `RemoteTerminal`, `TerminalForeground` | [`terminal`] |
//! | `sshArguments`/`scanHostKey`/`acceptHostKey`/`forgetHostKey`/`detectPlatform`/`deployDaemon`/`sshConnection`/`connectSsh`/`localDaemon`/`packagedDaemon` | [`ssh`] |
//! | Node's `path.posix`/`path.win32` rules | [`paths`] |
//! | the tool-side "which environment" decision | [`selection`] |
//!
//! Framing is *not* duplicated: the frame codec is the daemon's own
//! (`crate::env_daemon::frame`), so both ends of the wire share one
//! implementation of `u32 length | u8 type | u32 id | u32 jsonLength | json |
//! payload`.

pub mod connection;
pub mod errors;
pub mod paths;
pub mod remote_env;
pub mod selection;
pub mod ssh;
pub mod terminal;
pub mod watch;

pub use connection::{
    node_platform, Connection, ConnectionOptions, RemoteInfo, Reply, RequestOptions, MAX_FRAME,
};
pub use errors::{
    is_connection_lost, to_file_error, ExecutionError, ExecutionErrorCode, FileError,
    FileErrorCode, RemoteError,
};
pub use paths::RemotePathFlavor;
pub use remote_env::{
    BinaryReader, Context, DirPage, DirReader, ExecCommand, ExecOptions, ExecResult, FileInfo,
    FileKind, LineScan, ListFilesOptions, ListedEntry, OutputFn, OutputInfo, RemoteExecutionEnv,
    RemoteExecutionEnvOptions, TextLine, TextLineReader, WatchExclude, WatchTarget,
};
#[doc(hidden)]
pub use selection::configuration_key_for_test;
pub use selection::{
    clear_override, configured_remote_env, install_override, tool_env_description,
    workspace_path_remote,
};
pub use terminal::{
    open_terminal, was_closed, writer_queue_limits, Exited, RemoteTerminal, TerminalDataFn,
    TerminalForeground, TerminalOptions, WRITE_CHUNK,
};
pub use watch::{RemoteWatcher, WatchChange, WatchChangeFn, WatchOptions};
