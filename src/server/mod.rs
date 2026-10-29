//! TCP remote session server for the protocol v8 generic RPC envelope.
//!
//! The server owns the connection lifecycle and emits schema-validated CBOR
//! frames through [`crate::protocol`].  Protocol v8 replaces the concrete v1
//! command catalog with a generic envelope: clients send
//! `request {id, target, call}` frames where `call` is a chord service call
//! (`{serviceId, member, args}`).  Two services are hosted:
//!
//! * `$chord.service` — built-in control service (`catalogue`, `subscribe`,
//!   `unsubscribe`).
//! * `pir.session` — the session service; its members are the former v1
//!   commands (`list`, `create`, `attach`, `detach`, `prompt`, `steer`,
//!   `abort`, `set_model`, `set_thinking`) with arguments taken from the call
//!   args array and, for session members, from the RPC target fencing.
//!
//! Attach/detach is the attachment-fencing model (upstream
//! `packages/server/src/session-router.ts`): an `attach` call adds the
//! connection as one attachment of the session — a session carries SEVERAL
//! attachments at once — the server answers with an out-of-band
//! `attachment` frame carrying the session route, and every session-fenced
//! call must present the caller's leased `attachmentId`.  Session progress
//! and snapshot changes are broadcast to subscribed connections as
//! `service_update` frames carrying chord `state` updates in the delta tuple
//! vocabulary (`["r", <TranscriptProgress|SessionSnapshot>]`); the historical
//! `{op, value}` object form stays available through
//! [`StateUpdateFormat::Legacy`](crate::protocol::StateUpdateFormat) /
//! [`ServerState::with_legacy_state_updates`], and clients consume either
//! shape through [`crate::protocol::StateUpdateDecoder`]. When the server
//! state is built with a session directory ([`ServerState::with_session_dir`]),
//! sessions are backed by durable JSONL files: `list` merges the directory,
//! attaching reopens a persisted session, and disconnects/restarts keep the
//! conversation.
//!
//! Prompt runs execute in spawned per-session tasks: the prompt response is
//! delivered by the run task when the run finishes (same response id), so the
//! owning connection's read loop stays free and abort/steer/cancel frames are
//! processed while a run is in flight. Connections apply the
//! [`ServerSecurityConfig`] policy: optional preface auth, connection/body
//! limits, an idle read timeout, a bounded outbound frame channel, session
//! and subscription caps, bash-tool exclusion unless `allow_remote_bash`, and
//! remote-`create` cwd confinement (default: reject any cwd but the server's
//! own; `PIR_SERVER_ALLOWED_ROOTS` widens it).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::RwLock;

use crate::protocol::{FrameOptions, ServerSnapshot, SessionMetadata, PROTOCOL_VERSION};

mod connection;
mod dispatch;
mod handshake;
mod policy;
mod progress;
mod session;
mod session_state;

use connection::serve_connection;
pub use policy::ServerSecurityConfig;
pub use session::{
    resolve_server_tool_selection, server_tool_names, stall_next_attach_open_for_test, ServerState,
    SessionCommandError, SessionRecord,
};

/// A bound protocol server.
pub struct Server {
    listener: TcpListener,
    state: ServerState,
    frame_options: FrameOptions,
    security: ServerSecurityConfig,
    active: Arc<RwLock<HashMap<String, ()>>>,
}

impl Server {
    /// Bind a TCP listener with an empty default snapshot.
    pub async fn bind<A: ToSocketAddrs>(address: A) -> io::Result<Self> {
        Self::bind_with_state(address, ServerState::default()).await
    }

    /// Bind a TCP listener with caller-provided snapshot state.
    pub async fn bind_with_snapshot<A: ToSocketAddrs>(
        address: A,
        snapshot: ServerSnapshot,
    ) -> io::Result<Self> {
        Self::bind_with_state(address, ServerState::new(snapshot)).await
    }

    /// Bind a TCP listener with shared state.
    pub async fn bind_with_state<A: ToSocketAddrs>(
        address: A,
        state: ServerState,
    ) -> io::Result<Self> {
        Self::bind_with_state_and_security(address, state, ServerSecurityConfig::default()).await
    }

    /// Bind a listener with explicit authentication, limits, and remote policy.
    pub async fn bind_with_security<A: ToSocketAddrs>(
        address: A,
        security: ServerSecurityConfig,
    ) -> io::Result<Self> {
        Self::bind_with_state_and_security(address, ServerState::default(), security).await
    }

    /// Bind a listener with shared state and explicit security settings.
    pub async fn bind_with_state_and_security<A: ToSocketAddrs>(
        address: A,
        state: ServerState,
        security: ServerSecurityConfig,
    ) -> io::Result<Self> {
        security.validate()?;
        let listener = TcpListener::bind(address).await?;
        // The state carries the policy its sessions enforce (bash exclusion,
        // caps, cwd policy); the connection layer reads the same config.
        let state = state.with_security(security.clone());
        Ok(Self {
            listener,
            state,
            frame_options: FrameOptions {
                max_frame_length: security.max_request_body_bytes,
            },
            security,
            active: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Return the local address assigned by the operating system.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Return a reference to the shared server state.
    pub fn state(&self) -> &ServerState {
        &self.state
    }

    /// Accept and serve clients until the listener errors.
    pub async fn run(self) -> io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let state = self.state.clone();
            let active = self.active.clone();
            let options = self.frame_options;
            let security = self.security.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, state, active, options, security).await
                {
                    tracing::debug!(%error, "protocol connection ended with error");
                }
            });
        }
    }

    /// Accept and serve one client. Useful for embedded servers and tests.
    pub async fn accept_once(&self) -> io::Result<()> {
        let (stream, _) = self.listener.accept().await?;
        serve_connection(
            stream,
            self.state.clone(),
            self.active.clone(),
            self.frame_options,
            self.security.clone(),
        )
        .await
        .map_err(|error| io::Error::other(error.to_string()))
    }

    /// Return the security config the server enforces (embedded hosts reuse
    /// it for a matching client setup).
    pub fn security(&self) -> &ServerSecurityConfig {
        &self.security
    }
}

/// Unix-domain-socket transport for the same protocol (upstream
/// `packages/server/src/transports/unix/`). Available on Unix only; the
/// socket file is unlinked before bind (stale-socket recovery, upstream
/// `removeStaleSocket`) and created with `0600` permissions so only the
/// owner may connect (upstream `DEFAULT_SOCKET_MODE`). Windows hosts keep
/// the TCP transport (`[net::TcpListener]` named-pipe equivalents are out of
/// scope).
#[cfg(unix)]
pub struct UnixServer {
    listener: tokio::net::UnixListener,
    socket_path: std::path::PathBuf,
    state: ServerState,
    frame_options: FrameOptions,
    security: ServerSecurityConfig,
    active: Arc<RwLock<HashMap<String, ()>>>,
}

#[cfg(unix)]
impl UnixServer {
    /// Bind a Unix-domain socket at `path` with default snapshot state.
    /// A stale socket file at `path` is removed first.
    pub async fn bind(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        Self::bind_with_security(path, ServerSecurityConfig::default()).await
    }

    /// Bind a Unix-domain socket with explicit security settings.
    pub async fn bind_with_security(
        path: impl AsRef<std::path::Path>,
        security: ServerSecurityConfig,
    ) -> io::Result<Self> {
        Self::bind_with_state_and_security(path, ServerState::default(), security).await
    }

    /// Bind a Unix-domain socket with caller-provided state (the CLI host
    /// carries the resolved global tool selection into `ServerState`) and
    /// explicit security settings.
    pub async fn bind_with_state_and_security(
        path: impl AsRef<std::path::Path>,
        state: ServerState,
        security: ServerSecurityConfig,
    ) -> io::Result<Self> {
        security.validate()?;
        let path = path.as_ref().to_path_buf();
        // Stale socket recovery (upstream removeStaleSocket): a leftover file
        // from a crashed server would fail the bind otherwise. Only remove
        // actual sockets, never regular files — and never a LIVE server's
        // socket. A successful connect proves the listener is still serving;
        // ECONNREFUSED/ENOENT is the stale case. Removing unconditionally
        // would silently orphan the running server.
        use std::os::unix::fs::FileTypeExt;
        if let Ok(before) = std::fs::metadata(&path) {
            if before.file_type().is_socket() {
                // A live listener accepts even when the server is not polling:
                // the kernel completes the connection from its backlog. A
                // wedged server can therefore block the probe; a timeout is
                // not proof of staleness, so fail closed with AddrInUse.
                let probe = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    tokio::net::UnixStream::connect(&path),
                )
                .await;
                match probe {
                    Ok(Ok(_live)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!(
                                "unix socket {} is already served by a live process",
                                path.display()
                            ),
                        ));
                    }
                    Ok(Err(error))
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        // The probe proves the old listener is gone, but not
                        // that the path still names the same inode: another
                        // starter may have rebound it while we probed.
                        match std::fs::metadata(&path) {
                            Ok(after) if same_socket_inode(&before, &after) => {
                                match std::fs::remove_file(&path) {
                                    Ok(()) => {}
                                    // Another starter removed it first: the
                                    // path is free, so continue to bind.
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                    Err(error) => return Err(error),
                                }
                            }
                            Ok(_) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::AddrInUse,
                                    format!(
                                        "unix socket {} was rebound while checking staleness",
                                        path.display()
                                    ),
                                ));
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                    // Permission/transport errors do not prove staleness:
                    // fail the bind instead of stealing the path.
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!(
                                "unix socket {} did not answer the liveness probe",
                                path.display()
                            ),
                        ));
                    }
                }
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        // 0600 (upstream DEFAULT_SOCKET_MODE): only the owner may connect. A
        // failure here must not leave a world-accessible socket behind.
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let state = state.with_security(security.clone());
        Ok(Self {
            listener,
            socket_path: path,
            state,
            frame_options: FrameOptions {
                max_frame_length: security.max_request_body_bytes,
            },
            security,
            active: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// The bound socket path.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Return a reference to the shared server state.
    pub fn state(&self) -> &ServerState {
        &self.state
    }

    /// Accept and serve clients until the listener errors. On drop of the
    /// last handle the socket file may remain; call [`Self::close`] for an
    /// explicit cleanup.
    pub async fn run(self) -> io::Result<()> {
        let socket_path = self.socket_path.clone();
        let result = self.run_inner().await;
        // Best-effort socket removal so a restart re-binds cleanly.
        let _ = std::fs::remove_file(&socket_path);
        result
    }

    async fn run_inner(self) -> io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let state = self.state.clone();
            let active = self.active.clone();
            let options = self.frame_options;
            let security = self.security.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, state, active, options, security).await
                {
                    tracing::debug!(%error, "protocol connection ended with error");
                }
            });
        }
    }

    /// Remove the socket file (idempotent best-effort cleanup).
    pub fn close(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Whether two metadata snapshots still describe the same socket inode.
#[cfg(unix)]
fn same_socket_inode(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    after.file_type().is_socket() && before.dev() == after.dev() && before.ino() == after.ino()
}

#[derive(Debug, thiserror::Error)]
enum ServerError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] crate::protocol::ProtocolError),
    #[error("authentication failed")]
    Authentication,
    #[error("connection limit reached")]
    ConnectionLimit,
    #[error("connection body limit exceeded")]
    BodyLimit,
    #[error("connection idle timeout")]
    IdleTimeout,
    #[error("outbound frame channel full")]
    FrameChannelFull,
}

impl From<crate::protocol::framing::FrameError> for ServerError {
    fn from(error: crate::protocol::framing::FrameError) -> Self {
        Self::Protocol(crate::protocol::ProtocolError::Framing(error))
    }
}

/// Construct a metadata-only snapshot for small integrations and tests.
pub fn empty_snapshot(server_id: impl Into<String>) -> ServerSnapshot {
    ServerSnapshot {
        server_id: server_id.into(),
        protocol_version: PROTOCOL_VERSION,
        revision: 0,
        sessions: Vec::<SessionMetadata>::new(),
        models: Vec::new(),
    }
}
