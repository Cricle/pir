//! pi-phone native port: the HTTP/1.1 + WebSocket server core.
//!
//! This is a tokio + `tokio-tungstenite` port of the reference
//! `phone-server-runtime.ts` server surface:
//!
//! - `GET /` and the embedded client tree through [`super::assets::lookup`]
//!   (ETag/304, no-cache shell assets, `index.html` mount-path rewrite, gzip
//!   negotiation with per-representation ETags, and the reference server's
//!   `index.html` fallback for missing files);
//! - `GET|HEAD /api/health` (any method, like the reference), `/api/quota`,
//!   `/api/image/<key>`;
//! - `POST /__pi_phone__/control/stop` -> always 403; non-POST -> 405;
//! - non-GET/HEAD on every other route -> 405;
//! - the `/ws` (and `/phone-<port>/ws`) RFC 6455 upgrade, token query
//!   validation (close 1008 `invalid-token`), single-client replacement
//!   (close 4009), idle stop (close 4010), text/binary transport echo, and
//!   ping/pong. The whole of RFC 6455 is `tokio-tungstenite`'s: the opening
//!   handshake (`accept_async_with_config`), masking, fragmentation, UTF-8,
//!   the control-frame rules, the close handshake, and the frame/message caps.
//!   能用库就用库: a hand-rolled codec would only re-derive the RFC's edge cases;
//!   the crate is already in the dependency graph (`provider-openai`) and is
//!   the same one the Codex transport uses. What stays here is the application
//!   policy on top of it (the 400/426 handshake answers, token gate,
//!   origin/authority, single-client admission, the reader/writer task split,
//!   and the idle/stalled-frame deadlines).
//!
//! The HTTP/1.1 layer (request parse, response write, routing) is still
//! hand-written in this stage; `hyper` is the audited follow-up
//! (`docs/archive/library-audit.md` §3, stage 2).
//!
//! The session bridge lives in `session.rs`. Each `/ws` connection is a
//! bounded subscriber: JSON protocol lines are fed to
//! [`SessionBridge::handle_client_line`], and the bridge's per-client queue is
//! written back as text frames. Boot `status`/`catalog` frames travel through
//! the same mapping path. The socket is split into a reader task and a writer
//! loop (each owning its own `SplitSink`/`SplitStream` half of one
//! [`tokio_tungstenite::WebSocketStream`]) so an outbound bridge line can
//! never cancel a partially-read message, and a slow client is dropped by the
//! bridge's bounded queue instead of blocking the session actor.
//!
//! [`PhoneHost`] is the plugin's [`HostCommands`] implementation: the host lane
//! installs the live adapter with
//! [`super::PhonePlugin::set_host_commands`]; until then it answers standalone
//! introspection commands and fails state-changing ones honestly.
//!
//! Lifecycle state is a [`PhoneState`] with a `tokio::sync::Mutex`; start and
//! stop are serialized by a second mutex so concurrent command invocations
//! cannot bind twice. Nothing here blocks a synchronous registration thread:
//! command handlers are async and binding happens inside them.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::task::AtomicWaker;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Map as JsonMap, Value as JsonValue};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame as WsCloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

use crate::extensions::ExtensionHandle;

use super::assets;
use super::runtime_state::{self, Reconcile, RuntimeStore};
use super::session::{
    BridgeConfig, BridgeSubscriber, CloseReason, ForwardedCommand, HostCommands, HostFuture,
    HostReply, PhoneCommand, SessionBridge, SubscribeError,
};
use super::tailscale::{EnableResult, ServeInfo, Tailscale};

/// Status key of the phone entry (the reference `ctx.ui.setStatus` key).
pub const STATUS_KEY: &str = "pi-phone";

/// Default first port to try; subsequent sessions auto-increment.
pub const DEFAULT_PHONE_PORT: u16 = 8787;
/// Upper bound for the auto-port scan (inclusive).
pub const MAX_AUTO_PORT: u16 = 8987;
/// Default idle auto-stop: two hours.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 2 * 60 * 60_000;

/// Maximum accepted assembled WebSocket message size (fragments included).
const MAX_WS_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum payload of a single WebSocket frame. tungstenite checks the
/// declared length against this cap before reading or allocating the payload
/// (`CapacityError::MessageTooLong`, sent as close 1009).
pub const MAX_WS_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Maximum concurrently admitted TCP connections. Beyond this the server
/// answers 503 and closes immediately.
pub const MAX_CONNECTIONS: usize = 64;
/// Maximum HTTP request head size (request line + headers + terminator).
const MAX_HTTP_HEAD_BYTES: usize = 64 * 1024;
/// Maximum length of one HTTP header/request line.
const MAX_HTTP_HEADER_LINE_BYTES: usize = 8 * 1024;
/// Maximum number of HTTP headers in one request.
const MAX_HTTP_HEADER_COUNT: usize = 100;
/// Maximum accepted request body. The phone protocol has no body-bearing
/// endpoints, so anything larger is rejected before it is drained.
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;
/// Per-request safety net, matching the reference server's 15 s watchdog.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Deadline for one HTTP/WebSocket write (or handshake write).
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// Deadline to finish a WebSocket frame once its first byte arrived.
const WS_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle-client deadline when idle auto-stop is disabled: how long a
/// connection may wait for the first byte of its next frame before it is
/// closed. With idle auto-stop enabled the server-wide timer fires first
/// (see [`ws_idle_timeout`]), so that feature keeps its behavior.
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Upper bound on waiting for the reader task to observe an abort signal.
const WS_TASK_ABORT_TIMEOUT: Duration = Duration::from_secs(2);
/// RFC 6455 7.1.1: how long the reader task may take to consume the peer's
/// close reply after our close frame (tungstenite owns the drain itself).
const WS_CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound on a graceful stop before the accept task is aborted.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// The runtime control path. Another pi session must never stop this server
/// over HTTP (the reference keeps it as a hard 403).
pub const CONTROL_STOP_PATH: &str = "/__pi_phone__/control/stop";

/// The four commands, in registration order.
pub const COMMAND_NAMES: [&str; 4] = ["phone-start", "phone-stop", "phone-status", "phone-token"];

// ---------------------------------------------------------------------------
// Configuration and argument parsing
// ---------------------------------------------------------------------------

/// Runtime configuration (the reference `PhoneConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneConfig {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub cwd: PathBuf,
    pub idle_timeout_ms: u64,
}

impl PhoneConfig {
    /// The reference initializer: `PI_PHONE_TOKEN`, `PI_PHONE_IDLE_MINUTES`,
    /// host `127.0.0.1`, port `8787`, cwd `process.cwd()`, idle 2 h.
    pub fn from_env() -> Self {
        let token = std::env::var("PI_PHONE_TOKEN").unwrap_or_default();
        let idle_timeout_ms = std::env::var("PI_PHONE_IDLE_MINUTES")
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|minutes| minutes.is_finite())
            .map(|minutes| (minutes.max(0.0) * 60_000.0).round() as u64)
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_MS);
        Self {
            host: "127.0.0.1".to_owned(),
            port: DEFAULT_PHONE_PORT,
            token,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            idle_timeout_ms,
        }
    }

    /// The cwd as the client displays it.
    pub fn cwd_string(&self) -> String {
        self.cwd.to_string_lossy().into_owned()
    }
}

/// Parsed `/phone-start` arguments (the reference `ParsedPhoneArgs`).
#[derive(Debug, Clone)]
pub struct ParsedPhoneArgs {
    pub config: PhoneConfig,
    pub token_specified: bool,
    pub idle_specified: bool,
    /// True when the user pinned a port (auto-scan disabled).
    pub port_specified: bool,
}

/// Parse `/phone-start` arguments exactly like `phone-args.ts`.
///
/// One deliberate difference: an invalid `--port` value is rejected with an
/// actionable error instead of being silently ignored, and `0` explicitly
/// selects an ephemeral port (the reference only accepts `0` through the bare
/// numeric form). Both keep the same surface shape (`--port N`, `--port=N`,
/// bare port, `--host`, `--token`, `-`, `--cwd`, `--idle-mins`, `--idle-secs`
/// and their `=` forms).
pub fn parse_start_args(args: &str, current: &PhoneConfig) -> Result<ParsedPhoneArgs, String> {
    let mut next = current.clone();
    let mut token_specified = false;
    let mut idle_specified = false;
    let mut port_specified = false;

    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(ParsedPhoneArgs {
            config: next,
            token_specified,
            idle_specified,
            port_specified,
        });
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];

        if token == "--port" && index + 1 < tokens.len() {
            port_specified = true;
            next.port = parse_port(tokens[index + 1])?;
            index += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--port=") {
            port_specified = true;
            next.port = parse_port(value)?;
            index += 1;
            continue;
        }

        if token == "--host" && index + 1 < tokens.len() {
            next.host = tokens[index + 1].to_owned();
            index += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--host=") {
            next.host = value.to_owned();
            index += 1;
            continue;
        }

        if token == "--token" && index + 1 < tokens.len() {
            token_specified = true;
            next.token = normalize_token(tokens[index + 1]);
            index += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--token=") {
            token_specified = true;
            next.token = normalize_token(value);
            index += 1;
            continue;
        }

        if token == "--cwd" && index + 1 < tokens.len() {
            next.cwd = resolve_cwd(tokens[index + 1]);
            index += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--cwd=") {
            next.cwd = resolve_cwd(value);
            index += 1;
            continue;
        }

        if token == "--idle-mins" && index + 1 < tokens.len() {
            idle_specified = true;
            if let Some(minutes) = parse_non_negative(tokens[index + 1])? {
                next.idle_timeout_ms = (minutes * 60_000.0).round() as u64;
            }
            index += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--idle-mins=") {
            idle_specified = true;
            if let Some(minutes) = parse_non_negative(value)? {
                next.idle_timeout_ms = (minutes * 60_000.0).round() as u64;
            }
            index += 1;
            continue;
        }

        if token == "--idle-secs" && index + 1 < tokens.len() {
            idle_specified = true;
            if let Some(seconds) = parse_non_negative(tokens[index + 1])? {
                next.idle_timeout_ms = (seconds * 1_000.0).round() as u64;
            }
            index += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--idle-secs=") {
            idle_specified = true;
            if let Some(seconds) = parse_non_negative(value)? {
                next.idle_timeout_ms = (seconds * 1_000.0).round() as u64;
            }
            index += 1;
            continue;
        }

        if token.chars().all(|ch| ch.is_ascii_digit()) && !token.is_empty() {
            port_specified = true;
            next.port = parse_port(token)?;
            index += 1;
            continue;
        }

        // The bare token form: only the first non-flag token is taken, and
        // only while the token is still the one we started from (reference
        // `next.token === current.token`).
        if !token.starts_with("--") && next.token == current.token {
            token_specified = true;
            next.token = normalize_token(token);
            index += 1;
            continue;
        }

        index += 1;
    }

    Ok(ParsedPhoneArgs {
        config: next,
        token_specified,
        idle_specified,
        port_specified,
    })
}

fn normalize_token(value: &str) -> String {
    if value == "-" {
        String::new()
    } else {
        value.to_owned()
    }
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port: u32 = value
        .parse()
        .map_err(|_| format!("Invalid port `{value}`: expected a number from 0 to 65535"))?;
    if port > u16::MAX as u32 {
        return Err(format!(
            "Invalid port `{value}`: expected a number from 0 to 65535"
        ));
    }
    Ok(port as u16)
}

/// `None` for the reference's "not a finite number" case (value ignored).
fn parse_non_negative(value: &str) -> Result<Option<f64>, String> {
    match value.parse::<f64>() {
        Ok(number) if number.is_finite() && number >= 0.0 => Ok(Some(number)),
        Ok(_) => Ok(None),
        Err(_) => Err(format!(
            "Invalid value `{value}`: expected a non-negative number"
        )),
    }
}

/// Whether a bind host is loopback (`localhost`, `127.0.0.0/8`, `::1`).
///
/// A hostname is treated as non-loopback: resolving it could reach a LAN
/// address, and refusing is the safe default for a token-disabled server.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

/// The reference `resolve()` for `--cwd`: absolute relative to the process
/// cwd, lexically normalized (the target need not exist).
fn resolve_cwd(value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    std::path::absolute(&absolute).unwrap_or(absolute)
}

/// Generate a session token in the reference shape `xxxxxx-xxxxxx-xxxx`.
///
/// 12 bytes from the OS-seeded `RandomState` mixed with the clock and an
/// atomic counter, hashed with SHA-1, base64url (no padding), then split
/// 6-6-4 exactly like `randomBytes(12).toString("base64url")` in the
/// reference.
pub fn generate_token() -> String {
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let state = hasher.finish();

    let mut input = Vec::with_capacity(20);
    input.extend_from_slice(&nanos.to_le_bytes());
    input.extend_from_slice(&state.to_le_bytes());
    let digest = Sha1::digest(input);
    let raw = URL_SAFE_NO_PAD.encode(&digest[..12]);
    format!("{}-{}-{}", &raw[..6], &raw[6..12], &raw[12..16])
}

/// The reference status-line text: `📱 ● phone on` / `📱 ○ phone off`.
pub fn status_line(running: bool) -> String {
    if running {
        "📱 ● phone on".to_owned()
    } else {
        "📱 ○ phone off".to_owned()
    }
}

fn status_text(config: &PhoneConfig, running: bool) -> String {
    if !running {
        return "Pi Phone is stopped".to_owned();
    }
    let idle = if config.idle_timeout_ms > 0 {
        format!(
            "{}m idle auto-stop",
            std::cmp::max(1, (config.idle_timeout_ms as f64 / 60_000.0).round() as u64)
        )
    } else {
        "idle auto-stop disabled".to_owned()
    };
    format!(
        "Pi Phone running at http://{}:{} for {} ({}) · mirroring the current CLI session with optional parallel sessions · owner: cli · {}",
        config.host,
        config.port,
        config.cwd_string(),
        if config.token.is_empty() {
            "no token"
        } else {
            "token enabled"
        },
        idle
    )
}

/// The `/phone-start` result suffix for one Tailscale enable attempt.
///
/// Mirrors the reference notifications: ready/changed, already pointing, or a
/// soft failure with the manual fallback. A missing `tailscale` never fails
/// the local server.
fn tailscale_start_suffix(result: &EnableResult, port: u16) -> String {
    if result.enabled {
        if result.changed {
            if result.url.is_empty() {
                " Tailscale Serve ready for this device.".to_owned()
            } else {
                format!(" Tailscale Serve ready: {}", result.url)
            }
        } else if result.url.is_empty() {
            " Tailscale Serve already points to Pi Phone.".to_owned()
        } else {
            format!(
                " Tailscale Serve already points to Pi Phone: {}",
                result.url
            )
        }
    } else if result.error.is_empty() {
        String::new()
    } else {
        format!(
            " Could not configure Tailscale Serve automatically: {}\n\
             Manual fallback: tailscale serve --bg --https=443 http://127.0.0.1:{port}",
            result.error
        )
    }
}

/// The `/phone-status` suffix for one Tailscale probe (the reference
/// `handlePhoneStatus` notifications), appended to the server status text.
fn tailscale_status_suffix(info: &ServeInfo, running: bool) -> String {
    if info.active {
        if running {
            if info.url.is_empty() {
                " · Tailscale Serve: enabled for Pi Phone".to_owned()
            } else {
                format!(" · Tailscale Serve: {}", info.url)
            }
        } else if info.url.is_empty() {
            " · Tailscale Serve is still pointing at Pi Phone.".to_owned()
        } else {
            format!(
                " · Tailscale Serve is still pointing at Pi Phone: {}",
                info.url
            )
        }
    } else if running {
        if info.error.is_empty() {
            " · Tailscale Serve is not currently pointing to Pi Phone.".to_owned()
        } else {
            format!(" · Tailscale Serve check failed: {}", info.error)
        }
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Image store
// ---------------------------------------------------------------------------

const MAX_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Hard cap on the number of distinct images retained. [`MAX_IMAGE_CACHE_BYTES`]
/// bounds the payload bytes only; without an entry cap, millions of tiny
/// distinct images could still exhaust memory through per-entry overhead.
pub const MAX_IMAGE_CACHE_ENTRIES: usize = 512;
const IMAGE_CACHE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// How often a read may re-run the TTL sweep. The sweep walks every entry, so
/// `get` throttles it: an idle server pays one O(n) pass per minute at most
/// (the old code only pruned when a NEW image was stored, so a store that
/// stopped receiving images never released its expired entries).
const IMAGE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
/// Path prefix of the stored-image route (reference `IMAGE_ROUTE`).
const IMAGE_ROUTE: &str = "/api/image/";

/// The only content types served from `/api/image/*`.
fn normalize_image_mime(mime_type: &str) -> &'static str {
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/png" => "image/png",
        "image/jpeg" | "image/jpg" => "image/jpeg",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

struct ImageEntry {
    bytes: Arc<[u8]>,
    mime_type: String,
    created_at: Instant,
}

/// In-memory content-addressed image store (the reference `PhoneImageStore`).
///
/// Keys are a truncated base64url SHA-1 of the decoded bytes: identical images
/// share one entry, and snapshots can reference `/api/image/<key>` instead of
/// re-sending base64 on every frame.
#[derive(Default)]
pub struct ImageStore {
    entries: HashMap<String, ImageEntry>,
    total_bytes: usize,
    /// Last TTL sweep (`None` = never; the first `get` sweeps).
    last_prune: Option<Instant>,
}

impl ImageStore {
    /// Decode and store one base64 image; returns its cache key.
    pub fn store(&mut self, data: &str, mime_type: &str) -> Option<String> {
        let bytes = STANDARD
            .decode(data)
            .or_else(|_| URL_SAFE.decode(data))
            .ok()?;
        self.store_bytes(&bytes, mime_type)
    }

    /// Store already-decoded bytes.
    pub fn store_bytes(&mut self, bytes: &[u8], mime_type: &str) -> Option<String> {
        self.store_bytes_with_budget(bytes, mime_type, MAX_IMAGE_CACHE_BYTES)
    }

    /// [`Self::store_bytes`] with the largest single image the store accepts.
    ///
    /// An image above the whole-store budget could never be served: it would
    /// be inserted and immediately evicted, and a snapshot that replaced its
    /// inline `data` with the returned URL would point at a 404. Refusing it
    /// keeps the caller's inline bytes intact instead.
    fn store_bytes_with_budget(
        &mut self,
        bytes: &[u8],
        mime_type: &str,
        max_image_bytes: usize,
    ) -> Option<String> {
        if bytes.is_empty() || bytes.len() > max_image_bytes {
            return None;
        }
        let digest = Sha1::digest(bytes);
        let key = URL_SAFE_NO_PAD.encode(digest)[..22].to_owned();
        if let Some(existing) = self.entries.get_mut(&key) {
            existing.created_at = Instant::now();
            return Some(key);
        }
        let entry = ImageEntry {
            bytes: Arc::from(bytes),
            // Never echo an arbitrary stored MIME: only image types the
            // browser can render safely are kept, everything else is served
            // as an opaque download with `nosniff` (finding 16).
            mime_type: normalize_image_mime(mime_type).to_owned(),
            created_at: Instant::now(),
        };
        self.total_bytes += entry.bytes.len();
        self.entries.insert(key.clone(), entry);
        self.prune();
        self.evict_if_needed();
        Some(key)
    }

    /// Test seam: store with an explicit single-image budget (the TTL/budget
    /// probes cannot allocate a 64 MiB fixture).
    #[doc(hidden)]
    pub fn store_bytes_with_budget_for_test(
        &mut self,
        bytes: &[u8],
        mime_type: &str,
        max_image_bytes: usize,
    ) -> Option<String> {
        self.store_bytes_with_budget(bytes, mime_type, max_image_bytes)
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.last_prune = Some(now);
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.created_at) > IMAGE_CACHE_TTL)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            if let Some(entry) = self.entries.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes.len());
            }
        }
    }

    fn evict_if_needed(&mut self) {
        // Both caps are enforced: total decoded bytes AND entry count (the
        // latter bounds the per-entry overhead a flood of tiny images would
        // otherwise accumulate). Oldest first, so recently published snapshot
        // images survive.
        while self.total_bytes > MAX_IMAGE_CACHE_BYTES
            || self.entries.len() > MAX_IMAGE_CACHE_ENTRIES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes.len());
            }
        }
    }

    /// Read one stored image, re-running the TTL sweep at most once per
    /// [`IMAGE_PRUNE_INTERVAL`]. A store that only receives reads (an idle
    /// phone session) therefore still releases entries past the TTL instead
    /// of holding them until the next `store_bytes`.
    fn get(&mut self, key: &str) -> Option<(Arc<[u8]>, String)> {
        let now = Instant::now();
        let due = self
            .last_prune
            .is_none_or(|last| now.duration_since(last) >= IMAGE_PRUNE_INTERVAL);
        if due {
            self.prune();
        }
        self.entries
            .get(key)
            .map(|entry| (Arc::clone(&entry.bytes), entry.mime_type.clone()))
    }

    /// Test seam: read one entry (the TTL/budget probes cannot reach the
    /// private `get`).
    #[doc(hidden)]
    pub fn get_for_test(&mut self, key: &str) -> Option<(Arc<[u8]>, String)> {
        self.get(key)
    }

    /// Test seam: `(entry count, decoded bytes)`.
    #[doc(hidden)]
    pub fn stats_for_test(&self) -> (usize, usize) {
        self.stats()
    }

    /// Test seam: move every entry's timestamp into the past (the TTL probe
    /// cannot wait two hours).
    #[doc(hidden)]
    fn age_entries_for_test(&mut self, age: Duration) {
        let created_at = Instant::now() - age;
        for entry in self.entries.values_mut() {
            entry.created_at = created_at;
        }
        // Force the next `get` to sweep.
        self.last_prune = None;
    }

    /// `(entry count, decoded bytes)` — the reference `stats` getter.
    fn stats(&self) -> (usize, usize) {
        (self.entries.len(), self.total_bytes)
    }
}

// ---------------------------------------------------------------------------
// Snapshot image extraction
// ---------------------------------------------------------------------------

/// True when one content part is an inline base64 image this store can take.
fn is_inline_image_part(part: &JsonValue) -> bool {
    part.get("type").and_then(JsonValue::as_str) == Some("image")
        && part.get("data").and_then(JsonValue::as_str).is_some()
}

/// Fast path for one `messages` value: no image part with a string `data`
/// means the caller keeps the value untouched (no clone, no store lock).
fn messages_have_inline_images(messages: &JsonValue) -> bool {
    messages.as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message
                .get("content")
                .and_then(JsonValue::as_array)
                .is_some_and(|content| content.iter().any(is_inline_image_part))
        })
    })
}

/// Replace every inline image part of one `messages` array with a
/// `/api/image/<key>` reference (reference `extractImages`).
///
/// A part whose bytes cannot be stored (bad base64 / empty) keeps its `data`:
/// an image is never lost because extraction failed.
fn rewrite_message_images(store: &mut ImageStore, messages: &mut JsonValue) {
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(JsonValue::as_array_mut) else {
            continue;
        };
        for part in content {
            let Some(object) = part.as_object_mut() else {
                continue;
            };
            if object.get("type").and_then(JsonValue::as_str) != Some("image") {
                continue;
            }
            let Some(data) = object.get("data").and_then(JsonValue::as_str) else {
                continue;
            };
            let mime_type = object
                .get("mimeType")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            if let Some(key) = store.store(data, mime_type) {
                object.remove("data");
                object.insert(
                    "previewUrl".to_owned(),
                    json!(format!("{IMAGE_ROUTE}{key}")),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin state
// ---------------------------------------------------------------------------

struct RunningServer {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    bridge: SessionBridge,
}

struct Inner {
    config: PhoneConfig,
    /// In-memory token kept across stop/start (never written to disk).
    session_token: String,
    latest_error: String,
    server: Option<RunningServer>,
    /// The persisted runtime record written by this start (removed on stop).
    runtime_state_path: Option<PathBuf>,
}

/// The bridge handle shared with event producers and the WebSocket tasks.
///
/// `host_tx` is the bounded inbound queue from [`SessionBridge::start`]; the
/// plugin's event feed uses [`mpsc::Sender::try_send`] so a slow phone client
/// can never block the agent actor.
#[derive(Clone)]
pub struct BridgeRuntime {
    pub bridge: SessionBridge,
    pub host_tx: mpsc::Sender<JsonValue>,
}

/// The most recent live-session publications (adversarial finding 3).
///
/// Written by [`super::PhonePlugin`]'s publish path and read by the running
/// server: `/api/health` merges [`Self::status`] over its server-owned fields,
/// and a new `/ws` subscription forwards the real status/catalog/snapshot
/// instead of waiting for the client to ask. Until the first publish the
/// server keeps its static placeholders, which is exactly the standalone
/// (no live host) behavior.
#[doc(hidden)]
#[derive(Default)]
pub struct HostPublishedState {
    status: Option<JsonValue>,
    catalog: Option<JsonValue>,
    snapshot: Option<JsonValue>,
}

/// Shared, plugin-owned server state. The [`super::PhonePlugin`] holds this in
/// an `Arc`, and every command closure captures a clone.
pub struct PhoneState {
    inner: Mutex<Inner>,
    /// Serializes start/stop so two concurrent invocations cannot both bind.
    lifecycle: Mutex<()>,
    running: Arc<AtomicBool>,
    addr: StdMutex<Option<SocketAddr>>,
    images: Arc<StdMutex<ImageStore>>,
    /// Best-effort status channel: when the host attached a JS runtime, the
    /// status entry is pushed through `__pir_host.setStatus` (the only status
    /// channel available to a Rust plugin today).
    runtime: StdMutex<Option<ExtensionHandle>>,
    /// The live session adapter installed by the host lane through
    /// [`PhoneState::set_host_commands`]. Shared with the running
    /// [`PhoneHost`], so installing it after `/phone-start` takes effect
    /// immediately.
    host_commands: Arc<StdMutex<Option<Arc<dyn HostCommands>>>>,
    /// The bridge of the currently running server (`None` while stopped).
    /// Kept outside the async `Inner` mutex so the event feed can inspect it
    /// synchronously and never block.
    bridge: Arc<StdMutex<Option<BridgeRuntime>>>,
    /// Host events dropped because the bounded bridge queue was full or the
    /// bridge was already closed. Observability only; never an error.
    host_events_dropped: Arc<AtomicU64>,
    /// The live host half of `/api/health` + the boot frames (see
    /// [`HostPublishedState`]). Shared with every running [`ServerMeta`].
    host_published: Arc<StdMutex<HostPublishedState>>,
    /// Persisted per-`host:port` runtime records (stale-session detection).
    runtime_store: StdMutex<RuntimeStore>,
    /// Tailscale Serve client; injectable so tests never touch a real tailnet.
    tailscale: StdMutex<Tailscale>,
}

impl PhoneState {
    /// Default state: Tailscale disabled (a no-op runner that never spawns).
    ///
    /// The builtin factory uses [`PhoneState::from_env_with_tailscale`] with
    /// the real client; a default-built state can never reconfigure a real
    /// Serve setup.
    pub fn from_env() -> Self {
        Self::from_env_with_tailscale(Tailscale::disabled())
    }

    /// State with an explicit Tailscale client.
    ///
    /// `PhonePlugin::new()` passes [`Tailscale::disabled`] so a default-built
    /// plugin can never spawn the CLI; the builtin factory passes the real
    /// client. Tests inject fake runners through
    /// [`super::PhonePlugin::with_tailscale_runner`].
    #[doc(hidden)]
    pub fn from_env_with_tailscale(tailscale: Tailscale) -> Self {
        let config = PhoneConfig::from_env();
        let session_token = config.token.clone();
        Self {
            inner: Mutex::new(Inner {
                config,
                session_token,
                latest_error: String::new(),
                server: None,
                runtime_state_path: None,
            }),
            lifecycle: Mutex::new(()),
            running: Arc::new(AtomicBool::new(false)),
            addr: StdMutex::new(None),
            images: Arc::new(StdMutex::new(ImageStore::default())),
            runtime: StdMutex::new(None),
            host_commands: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            host_events_dropped: Arc::new(AtomicU64::new(0)),
            host_published: Arc::new(StdMutex::new(HostPublishedState::default())),
            runtime_store: StdMutex::new(RuntimeStore::default()),
            tailscale: StdMutex::new(tailscale),
        }
    }

    /// Record the latest live status for `/api/health` and the boot frame.
    ///
    /// Called from [`super::PhonePlugin::publish_status`]; the server merges
    /// this object over its server-owned fields.
    #[doc(hidden)]
    pub fn set_host_status(&self, status: JsonValue) {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status = Some(status);
    }

    /// Record the latest live catalog for new WebSocket subscriptions.
    #[doc(hidden)]
    pub fn set_host_catalog(&self, catalog: JsonValue) {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .catalog = Some(catalog);
    }

    /// Record the latest live snapshot for new WebSocket subscriptions.
    #[doc(hidden)]
    pub fn set_host_snapshot(&self, snapshot: JsonValue) {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot = Some(snapshot);
    }

    /// The latest live status, when a live host ever published one.
    #[doc(hidden)]
    pub fn host_status(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status
            .clone()
    }

    /// The latest live catalog, when one was published.
    #[doc(hidden)]
    pub fn host_catalog(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .catalog
            .clone()
    }

    /// The latest live snapshot, when one was published.
    #[doc(hidden)]
    pub fn host_snapshot(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot
            .clone()
    }

    /// Install the live-session adapter (the documented host seam).
    ///
    /// The adapter receives every command the phone client sends and is
    /// expected to publish state back through
    /// [`PhonePlugin::on_event`](super::PhonePlugin::on_event) (or by feeding
    /// [`SessionBridge::dispatch_host_event`] directly). Until this is
    /// installed, [`PhoneHost`] answers a small set of standalone
    /// introspection commands so the server remains honest.
    pub fn set_host_commands(&self, host: Arc<dyn HostCommands>) {
        *self
            .host_commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(host);
    }

    /// The host adapter currently installed, if any.
    pub fn host_commands(&self) -> Option<Arc<dyn HostCommands>> {
        self.host_commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Point persisted-runtime bookkeeping at a specific directory.
    ///
    /// A test seam: production uses `<temp_dir>/pi-phone-extension`.
    #[doc(hidden)]
    pub fn set_runtime_store(&self, store: RuntimeStore) {
        *self
            .runtime_store
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = store;
    }

    fn runtime_store(&self) -> RuntimeStore {
        self.runtime_store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the Tailscale Serve client.
    ///
    /// A test seam: production spawns the `tailscale` binary from `PATH`.
    #[doc(hidden)]
    pub fn set_tailscale(&self, client: Tailscale) {
        *self
            .tailscale
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = client;
    }

    fn tailscale_client(&self) -> Tailscale {
        self.tailscale
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The injected Tailscale runner's kind (test/inspection seam).
    #[doc(hidden)]
    pub fn tailscale_runner_kind(&self) -> &'static str {
        self.tailscale_client().runner_kind()
    }

    /// Feed one host event into the running bridge.
    ///
    /// Returns `false` when no server is running or the bounded inbound queue
    /// rejected the event (full/closed). Never blocks the caller.
    pub fn forward_host_event(&self, event: JsonValue) -> bool {
        let sender = self
            .bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|runtime| runtime.host_tx.clone());
        let Some(sender) = sender else {
            return false;
        };
        match sender.try_send(event) {
            Ok(()) => true,
            Err(_) => {
                self.host_events_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// How many host events were dropped by the bounded bridge queue.
    pub fn dropped_host_events(&self) -> u64 {
        self.host_events_dropped.load(Ordering::Relaxed)
    }

    /// Whether a running bridge is available to receive host events.
    ///
    /// Cheap enough for every plugin-event dispatch; lets
    /// [`super::PhonePlugin::on_event`] skip cloning payloads while the phone
    /// server is stopped.
    pub fn has_bridge(&self) -> bool {
        self.bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// The bridge of the running server (test/host-lane inspection seam).
    #[doc(hidden)]
    pub fn bridge_for_test(&self) -> Option<SessionBridge> {
        self.bridge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|runtime| runtime.bridge.clone())
    }

    /// Whether a listener is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// The bound address, when running.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *self.addr.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Attach the host's JS runtime handle (only the status bridge uses it).
    pub fn attach_runtime(&self, js: Option<ExtensionHandle>) {
        let text = status_line(self.is_running());
        *self.runtime.lock().unwrap_or_else(PoisonError::into_inner) = js.clone();
        if let Some(js) = js {
            spawn_status_publish(js, text);
        }
    }

    /// Publish the current status line to the attached runtime, if any.
    pub fn sync_status(&self) {
        let js = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(js) = js {
            spawn_status_publish(js, status_line(self.is_running()));
        }
    }

    /// Store an image for `/api/image/<key>` (used by the session lane and
    /// tests).
    #[doc(hidden)]
    pub fn store_image(&self, data: &str, mime_type: &str) -> Option<String> {
        self.images
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .store(data, mime_type)
    }

    /// `(entry count, decoded bytes)` of the image store (test/observability
    /// seam, the reference `PhoneImageStore.stats`).
    #[doc(hidden)]
    pub fn image_stats(&self) -> (usize, usize) {
        self.images
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stats()
    }

    /// Test seam: move every stored image past the TTL (the real wait is two
    /// hours).
    #[doc(hidden)]
    pub fn age_images_for_test(&self, age: std::time::Duration) {
        self.images
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .age_entries_for_test(age);
    }

    /// Test seam: read one image through the TTL-throttled `get` path.
    #[doc(hidden)]
    pub fn image_for_test(&self, key: &str) -> Option<(Arc<[u8]>, String)> {
        self.images
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
    }

    /// Extract inline base64 images from one owned snapshot in place: every
    /// `{type:"image", data, mimeType}` part of the top-level `messages` array
    /// (and of `state.messages`, when the producer nests them) is stored and
    /// rewritten to `{type:"image", mimeType, previewUrl:"/api/image/<key>"}`.
    ///
    /// Runs before the snapshot is stored/forwarded, so the WebSocket frame
    /// carries short URLs instead of the raw bytes on every refresh. A
    /// snapshot with nothing to extract is left completely untouched; the
    /// TTL/byte-cap pruning only runs when there is an image to store.
    pub(crate) fn extract_snapshot_images(&self, snapshot: &mut JsonValue) {
        let top_level = snapshot
            .get("messages")
            .is_some_and(messages_have_inline_images);
        let nested = snapshot
            .get("state")
            .and_then(|state| state.get("messages"))
            .is_some_and(messages_have_inline_images);
        if !top_level && !nested {
            return;
        }

        let mut store = self.images.lock().unwrap_or_else(PoisonError::into_inner);
        // Same TTL as the reference: entries older than two hours would
        // otherwise accumulate for the life of a long-running server. The
        // byte cap (oldest first) stays enforced by `store_bytes`.
        store.prune();
        if top_level {
            if let Some(messages) = snapshot.get_mut("messages") {
                rewrite_message_images(&mut store, messages);
            }
        }
        if nested {
            if let Some(messages) = snapshot
                .get_mut("state")
                .and_then(|state| state.get_mut("messages"))
            {
                rewrite_message_images(&mut store, messages);
            }
        }
    }

    /// The `/phone-start` command.
    pub async fn start_command(&self, args: &str) -> Result<String, String> {
        let _lifecycle = self.lifecycle.lock().await;
        self.reap_finished().await;

        let current = {
            let inner = self.inner.lock().await;
            inner.config.clone()
        };
        let parsed = parse_start_args(args, &current)?;
        let mut config = parsed.config.clone();

        // Token stability within one session, mirroring the reference: an
        // auto-generated token survives stop/start; `-` disables it; an
        // explicit token replaces it.
        {
            let mut inner = self.inner.lock().await;
            if parsed.token_specified {
                inner.session_token = config.token.clone();
            } else if config.token.is_empty() {
                if inner.session_token.is_empty() {
                    let token = generate_token();
                    inner.session_token = token.clone();
                    config.token = token;
                } else {
                    config.token = inner.session_token.clone();
                }
            }
        }

        // A disabled token is loopback-only: without a credential, a LAN or
        // tailnet bind would let any host (or a DNS-rebinding browser) drive
        // the agent. Refuse rather than silently exposing an open server.
        if config.token.is_empty() && !is_loopback_host(&config.host) {
            let message = format!(
                "Refusing to bind Pi Phone to non-loopback host `{}` with the token disabled. \
                 Pass a token or use `--host 127.0.0.1`.",
                config.host
            );
            self.inner.lock().await.latest_error = message.clone();
            self.sync_status();
            return Err(message);
        }

        let running_now = {
            let inner = self.inner.lock().await;
            inner.server.is_some()
        } && self.running.load(Ordering::SeqCst);
        let changed = config != current;

        if running_now && changed {
            self.stop_inner().await;
        }

        let mut notes: Vec<String> = Vec::new();

        if !running_now || changed {
            // Reconcile the persisted record for the port we are about to try.
            // The idempotent path never reaches here: our own live record must
            // never be mistaken for stale state.
            let probe_port = if parsed.port_specified {
                config.port
            } else {
                DEFAULT_PHONE_PORT
            };
            if probe_port != 0 {
                match runtime_state::reconcile_runtime(
                    &self.runtime_store(),
                    &config.host,
                    probe_port,
                )
                .await
                {
                    // The owner process is gone: its Serve route is a dangling
                    // 502 and no live session can be hurt by removing it.
                    Reconcile::RemovedDead { .. } => {
                        let _ = self.tailscale_client().disable(probe_port).await;
                    }
                    // Another live pi session owns the port. Never touch it or
                    // its route; report it and let the bind/next-port logic
                    // decide what happens.
                    Reconcile::OtherSession { pid } => {
                        notes.push(runtime_state::ownership_message(
                            &config.host,
                            probe_port,
                            pid,
                        ));
                    }
                    Reconcile::None | Reconcile::RemovedOwn => {}
                }
            }

            let requested = if parsed.port_specified {
                Some(config.port)
            } else {
                None
            };
            let bound = bind_listener(&config.host, requested, parsed.port_specified).await;
            let (listener, actual_port) = match bound {
                Ok(bound) => bound,
                Err(error) => {
                    let message = if notes.is_empty() {
                        error
                    } else {
                        format!("{error}\n{}", notes.join("\n"))
                    };
                    self.inner.lock().await.latest_error = message.clone();
                    self.sync_status();
                    return Err(message);
                }
            };
            config.port = actual_port;

            let addr = match listener.local_addr() {
                Ok(addr) => addr,
                Err(error) => {
                    let message = format!(
                        "Failed to read the bound address for {}: {error}",
                        config.host
                    );
                    self.inner.lock().await.latest_error = message.clone();
                    self.sync_status();
                    return Err(message);
                }
            };

            // One bridge per listener: the WS transport feeds it every inbound
            // JSON line and drains each client's bounded queue. The host slot
            // is shared with `set_host_commands`, so installing the live
            // adapter later works without restarting the server.
            //
            // `ServerStatus` is shared with `PhoneHost`, so the standalone
            // `refresh` fallback re-sends the full status object (finding 8)
            // instead of wiping the client's header state.
            let status = Arc::new(ServerStatus {
                host: config.host.clone(),
                port: config.port,
                token: config.token.clone(),
                cwd_string: config.cwd_string(),
                idle_timeout_ms: config.idle_timeout_ms,
                last_activity: Arc::new(StdMutex::new(Instant::now())),
                client: Arc::new(StdMutex::new(None)),
                running: Arc::clone(&self.running),
                active_connections: Arc::new(AtomicUsize::new(0)),
            });
            let phone_host = Arc::new(PhoneHost::new(
                Arc::clone(&self.host_commands),
                Arc::clone(&status),
            ));
            let (bridge, host_tx) = SessionBridge::start(BridgeConfig::default(), phone_host);

            let meta = Arc::new(ServerMeta {
                status,
                images: Arc::clone(&self.images),
                next_client_id: Arc::new(AtomicU64::new(1)),
                bridge: bridge.clone(),
                live_host: Arc::clone(&self.host_commands),
                host_published: Arc::clone(&self.host_published),
                // `strip_mount` derives the same prefix from the port.
                gzip: GzipAssets::build(&format!("/phone-{}", config.port)),
            });
            self.running.store(true, Ordering::SeqCst);
            let (shutdown, shutdown_rx) = watch::channel(false);
            let task = tokio::spawn(accept_loop(listener, Arc::clone(&meta), shutdown_rx));
            *self.bridge.lock().unwrap_or_else(PoisonError::into_inner) = Some(BridgeRuntime {
                bridge: bridge.clone(),
                host_tx,
            });

            // Persist this start for cross-session stale detection. Only a
            // digest of a fresh runtime control token is written; best effort,
            // because a failed temp-file write must never fail the server.
            let runtime_control_token = generate_token();
            let runtime_state_path =
                match self
                    .runtime_store()
                    .write(&config.host, config.port, &runtime_control_token)
                {
                    Ok(path) => Some(path),
                    Err(error) => {
                        tracing::warn!("failed to persist Pi Phone runtime state: {error}");
                        None
                    }
                };

            {
                let mut inner = self.inner.lock().await;
                inner.config = config.clone();
                inner.latest_error.clear();
                inner.server = Some(RunningServer {
                    shutdown,
                    task,
                    bridge,
                });
                inner.runtime_state_path = runtime_state_path;
            }
            *self.addr.lock().unwrap_or_else(PoisonError::into_inner) = Some(addr);
        }

        self.sync_status();
        let mut text = status_text(&config, true);
        if !notes.is_empty() {
            text.push('\n');
            text.push_str(&notes.join("\n"));
        }
        // Tailscale is optional: enable reports a soft failure and never
        // fails the local server.
        let serve = self.tailscale_client().enable(config.port).await;
        text.push_str(&tailscale_start_suffix(&serve, config.port));
        Ok(text)
    }

    /// The `/phone-stop` command.
    pub async fn stop_command(&self) -> Result<String, String> {
        let _lifecycle = self.lifecycle.lock().await;
        self.reap_finished().await;
        let config = {
            let inner = self.inner.lock().await;
            inner.config.clone()
        };
        let stopped = self.stop_inner().await;
        self.sync_status();

        if stopped {
            // Remove only OUR path-scoped route; other sessions' routes stay
            // intact. A missing tailscale is a soft no-op.
            let tailscale = self.tailscale_client().disable(config.port).await;
            return Ok(if tailscale.disabled {
                "Pi Phone stopped and matching Tailscale Serve route disabled".to_owned()
            } else {
                "Pi Phone stopped".to_owned()
            });
        }

        // No local server. The reference still inspects the persisted record:
        // our own leftover state is removable, a dead owner's route is a
        // dangling 502, and a live session's port/route are never touched.
        match runtime_state::reconcile_runtime(&self.runtime_store(), &config.host, config.port)
            .await
        {
            Reconcile::None => Ok("Pi Phone is already stopped.".to_owned()),
            Reconcile::RemovedOwn => {
                Ok("Removed stale Pi Phone runtime state from an earlier start.".to_owned())
            }
            Reconcile::RemovedDead { .. } => {
                let tailscale = self.tailscale_client().disable(config.port).await;
                let mut text =
                    "Removed stale Pi Phone runtime state. The session that started it is gone."
                        .to_owned();
                if tailscale.disabled {
                    text.push_str(
                        "\nRemoved the dangling Tailscale Serve route left by that session.",
                    );
                }
                Ok(text)
            }
            Reconcile::OtherSession { pid } => Ok(runtime_state::ownership_message(
                &config.host,
                config.port,
                pid,
            )),
        }
    }

    /// The `/phone-status` command.
    pub async fn status_command(&self) -> Result<String, String> {
        self.reap_finished().await;
        let config = {
            let inner = self.inner.lock().await;
            inner.config.clone()
        };
        let running = self.is_running();
        let serve = self.tailscale_client().serve_info(config.port).await;
        let mut text = status_text(&config, running);
        text.push_str(&tailscale_status_suffix(&serve, running));
        Ok(text)
    }

    /// The `/phone-token` command. The reference only reports the token (it is
    /// never rotated by a command); `-` disables it on the next start.
    pub async fn token_command(&self) -> Result<String, String> {
        let config = {
            let inner = self.inner.lock().await;
            inner.config.clone()
        };
        if config.token.is_empty() {
            Ok("Pi Phone token is disabled for this server.".to_owned())
        } else {
            Ok(format!("Pi Phone token: {}", config.token))
        }
    }

    /// Stop on host shutdown/reload.
    pub async fn shutdown(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        let config = {
            let inner = self.inner.lock().await;
            inner.config.clone()
        };
        // Only a server this session actually ran owns a Serve route to remove
        // (the reference `handleSessionShutdown` guards on `hadServer`).
        if self.stop_inner().await {
            let _ = self.tailscale_client().disable(config.port).await;
        }
    }

    /// Direct test seam: run `/phone-start` and return the bound port.
    #[doc(hidden)]
    pub async fn start_for_test(&self, args: &str) -> Result<u16, String> {
        self.start_command(args).await?;
        self.local_addr()
            .map(|addr| addr.port())
            .ok_or_else(|| "phone server did not report a bound address".to_owned())
    }

    /// Direct test seam: stop the server; true when one was running.
    #[doc(hidden)]
    pub async fn stop_for_test(&self) -> bool {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_inner().await
    }

    async fn reap_finished(&self) {
        let mut inner = self.inner.lock().await;
        let finished = inner
            .server
            .as_ref()
            .map(|server| server.task.is_finished())
            .unwrap_or(false);
        if finished {
            if let Some(server) = inner.server.take() {
                server.bridge.close();
            }
            if let Some(path) = inner.runtime_state_path.take() {
                let _ = self.runtime_store().remove_path(&path);
            }
            self.running.store(false, Ordering::SeqCst);
            *self.addr.lock().unwrap_or_else(PoisonError::into_inner) = None;
            *self.bridge.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// Stop without taking the lifecycle lock (callers hold it).
    async fn stop_inner(&self) -> bool {
        let (running, runtime_state_path) = {
            let mut inner = self.inner.lock().await;
            (inner.server.take(), inner.runtime_state_path.take())
        };
        self.running.store(false, Ordering::SeqCst);
        *self.addr.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.bridge.lock().unwrap_or_else(PoisonError::into_inner) = None;

        // The record belongs to this listener; a missing file is fine.
        if let Some(path) = runtime_state_path {
            let _ = self.runtime_store().remove_path(&path);
        }

        let Some(mut running) = running else {
            return false;
        };

        // Close first: every subscriber gets `ServerClosed` and every
        // connection task wakes even if the accept loop is stuck.
        running.bridge.close();
        let _ = running.shutdown.send(true);
        if tokio::time::timeout(STOP_TIMEOUT, &mut running.task)
            .await
            .is_err()
        {
            running.task.abort();
            let _ = running.task.await;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Host seam
// ---------------------------------------------------------------------------

/// The plugin's [`HostCommands`] implementation: the documented seam between
/// the phone transport and the live CLI session.
///
/// The host lane installs its adapter with
/// [`PhonePlugin::set_host_commands`](super::PhonePlugin::set_host_commands)
/// (or [`PhoneState::set_host_commands`]). Every command parsed by
/// [`SessionBridge`] is then handed to that adapter, which owns the session
/// borrows and answers with [`HostReply`]. The adapter is also expected to
/// publish state back through [`super::PhonePlugin::on_event`], which the
/// plugin feeds into the same bridge.
///
/// Without a live adapter (standalone server, tests) this type answers a small
/// set of introspection commands locally, matching the transport slice that
/// existed before the session host was wired: `refresh` resyncs the client with
/// a server status, an empty catalog and an empty snapshot, while
/// `get_commands` / `get_state` / `get_messages` / `get_available_models` /
/// `get_session_stats` / `phone_get_tree` / `phone_list_sessions` /
/// `path_suggestions` return the same placeholder payloads. State-changing
/// commands fail honestly instead of pretending to run.
pub struct PhoneHost {
    live: Arc<StdMutex<Option<Arc<dyn HostCommands>>>>,
    /// The server-owned status fields. Used by the standalone fallback to
    /// re-send the complete status object on `refresh`.
    status: Arc<ServerStatus>,
}

impl PhoneHost {
    /// Wrap the shared live-adapter slot owned by [`PhoneState`] plus the
    /// status source owned by the running listener.
    fn new(live: Arc<StdMutex<Option<Arc<dyn HostCommands>>>>, status: Arc<ServerStatus>) -> Self {
        Self { live, status }
    }

    /// Whether the host lane has installed a live adapter.
    pub fn has_live_host(&self) -> bool {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    fn local_dispatch(&self, command: PhoneCommand) -> Result<HostReply, String> {
        match command {
            PhoneCommand::Refresh => Ok(HostReply {
                data: None,
                broadcast: vec![
                    json!({
                        "type": "status",
                        "data": self.status.status_json(),
                    }),
                    json!({
                        "type": "catalog",
                        "data": { "activeSessionId": JsonValue::Null, "sessions": [] },
                    }),
                    json!({
                        "type": "snapshot",
                        "state": empty_state(),
                        "messages": [],
                        "historyTruncated": false,
                        "totalMessageCount": 0,
                        "commands": command_catalog(),
                    }),
                ],
            }),
            PhoneCommand::GetSessionStats => Ok(HostReply::data(json!({
                "tokens": {
                    "input": 0, "cacheRead": 0, "cacheWrite": 0,
                    "output": 0, "reasoning": 0,
                },
                "toolCalls": 0,
                "totalMessages": 0,
                "cost": 0,
            }))),
            PhoneCommand::PhoneGetTree => Ok(HostReply::data(json!({ "nodes": [] }))),
            PhoneCommand::PhoneListSessions { .. } => Ok(HostReply::data(
                json!({ "sessions": [], "subagentCount": 0 }),
            )),
            PhoneCommand::Raw { command } => {
                let command_type = command
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let data = match command_type {
                    "get_commands" => json!({ "commands": command_catalog() }),
                    "get_available_models" => json!({ "models": [] }),
                    "get_messages" => json!({ "messages": [] }),
                    "get_state" => empty_state(),
                    // Both spellings are accepted; the transport normalizes
                    // the client's `path-suggestions` before it reaches the
                    // bridge so the response `command` is `path_suggestions`
                    // (finding 9).
                    "path_suggestions" | "path-suggestions" => json!({
                        "requestId": command.get("requestId").cloned().unwrap_or(json!(0)),
                        "mode": command.get("mode").cloned().unwrap_or(json!("mention")),
                        "query": command.get("query").cloned().unwrap_or(json!("")),
                        "suggestions": [],
                    }),
                    "" => {
                        return Err("Missing rpc command type.".to_owned());
                    }
                    other => {
                        return Err(format!(
                            "Command `{other}` is not available yet: no live session host \
                             is attached to the phone server."
                        ));
                    }
                };
                Ok(HostReply::data(data))
            }
            other => Err(format!(
                "Command `{}` requires the live session host: install it with \
                 PhonePlugin::set_host_commands.",
                other.wire_name()
            )),
        }
    }
}

impl HostCommands for PhoneHost {
    fn dispatch(&self, command: ForwardedCommand) -> HostFuture {
        let live = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(host) = live {
            return host.dispatch(command);
        }
        let result = self.local_dispatch(command.command);
        Box::pin(async move { result })
    }
}

/// Push `📱 ●/○ phone ...` through the JS runtime's `setStatus` host function.
///
/// This uses the public `eval_json` seam because the plugin framework's
/// `RegistrationContext` has no status-registration method yet; see the
/// `TODO` in `mod.rs`. Best-effort: a plan-only/dead runtime is ignored.
fn spawn_status_publish(js: ExtensionHandle, text: String) {
    let key = serde_json::to_string(STATUS_KEY).unwrap_or_else(|_| "\"pi-phone\"".to_owned());
    let text = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_owned());
    // `eval_json` expects the expression's value to be a JSON *string*.
    let code = format!("__pir_host.setStatus({key}, {text}); JSON.stringify(true);");
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = js.eval_json(&code).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Listening / accept loop
// ---------------------------------------------------------------------------

async fn bind_listener(
    host: &str,
    requested: Option<u16>,
    explicit: bool,
) -> Result<(TcpListener, u16), String> {
    if host.trim().is_empty() {
        return Err("Invalid host: the address is empty.".to_owned());
    }

    let candidates: Vec<u16> = match requested {
        Some(port) => vec![port],
        None => (DEFAULT_PHONE_PORT..=MAX_AUTO_PORT).collect(),
    };

    let mut last_error = String::new();
    for port in candidates {
        match TcpListener::bind((host, port)).await {
            Ok(listener) => {
                let actual = listener
                    .local_addr()
                    .map(|addr| addr.port())
                    .unwrap_or(port);
                return Ok((listener, actual));
            }
            Err(error) => {
                last_error = format!("{error}");
                if error.kind() == ErrorKind::AddrInUse {
                    if explicit {
                        return Err(format!(
                            "Port {host}:{port} is already in use by another program. \
                             Try /phone-start again without a port to pick the next free one."
                        ));
                    }
                    continue;
                }
                if explicit {
                    return Err(format!("Failed to bind {host}:{port}: {error}"));
                }
            }
        }
    }

    if requested.is_some() {
        Err(format!(
            "Port {host}:{} is already in use by another program. \
             Try /phone-start again without a port to pick the next free one.",
            requested.unwrap_or_default()
        ))
    } else {
        Err(format!(
            "No free phone port in {DEFAULT_PHONE_PORT}..={MAX_AUTO_PORT} on {host} ({last_error})"
        ))
    }
}

/// The server-owned status fields.
///
/// Shared by [`ServerMeta`] (HTTP `/api/health`, boot frames) and
/// [`PhoneHost`] (the standalone `refresh` fallback), so a refresh cannot
/// replace the client's status object with a partial one.
struct ServerStatus {
    host: String,
    port: u16,
    cwd_string: String,
    token: String,
    idle_timeout_ms: u64,
    last_activity: Arc<StdMutex<Instant>>,
    client: Arc<StdMutex<Option<ClientSlot>>>,
    running: Arc<AtomicBool>,
    /// Connections currently admitted through the cap.
    active_connections: Arc<AtomicUsize>,
}

impl ServerStatus {
    fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    fn status_json(&self) -> JsonValue {
        let connected = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        json!({
            "cwd": self.cwd_string,
            "hasToken": !self.token.is_empty(),
            "isRunning": self.running.load(Ordering::SeqCst),
            "childRunning": false,
            "isStreaming": false,
            "lastError": "",
            "pid": std::process::id(),
            "childPid": JsonValue::Null,
            "piCommand": "live cli + parallel pi --mode rpc",
            "connectedClients": if connected { 1 } else { 0 },
            "sessionCount": 0,
            "host": self.host,
            "port": self.port,
            "idleTimeoutMs": self.idle_timeout_ms,
            "lastActivityAt": epoch_millis(),
            "singleClientMode": true,
            "controlOwner": "cli",
            "pendingMessages": [],
            "serverRunning": self.running.load(Ordering::SeqCst),
        })
    }
}

/// Status keys the server owns even when a live host published status.
///
/// Everything else in the host object (`model`, `isStreaming`,
/// `controlOwner`, `thinkingLevel`, `sessionCount`, `pendingMessages`, ...)
/// is live session state and must win over the static placeholders.
const SERVER_OWNED_STATUS_KEYS: [&str; 9] = [
    "pid",
    "host",
    "port",
    "hasToken",
    "connectedClients",
    "idleTimeoutMs",
    "singleClientMode",
    "serverRunning",
    "isRunning",
];

/// Precompressed static responses for one running listener.
///
/// Compressible embedded assets come from the process-wide
/// [`assets::gzip_representation`] cache. `index.html` is rewritten with this
/// listener's mount prefix before it is served, so its gzip body is compressed
/// once here at startup as well. Request handling is a lookup only.
struct GzipAssets {
    index_html: Option<Vec<u8>>,
}

impl GzipAssets {
    /// Compress the mount-specific `index.html` and warm the shared cache for
    /// every other embedded asset.
    fn build(mount: &str) -> Self {
        let index_html = assets::lookup("index.html").and_then(|(content_type, bytes)| {
            let rewritten = rewrite_index_base(bytes, mount);
            assets::gzip_encoded(content_type, &rewritten)
        });
        assets::warm_gzip_cache();
        Self { index_html }
    }

    /// The gzip representation for a normalized asset path, if one exists.
    fn get(&self, asset_path: &'static str) -> Option<&[u8]> {
        if asset_path == "index.html" {
            self.index_html.as_deref()
        } else {
            assets::gzip_representation(asset_path)
        }
    }
}

struct ServerMeta {
    status: Arc<ServerStatus>,
    images: Arc<StdMutex<ImageStore>>,
    next_client_id: Arc<AtomicU64>,
    /// The bridge every `/ws` connection subscribes to and feeds.
    bridge: SessionBridge,
    /// The host-lane slot shared with [`PhoneHost`]; used to decide whether a
    /// placeholder catalog is still the only source of truth.
    live_host: Arc<StdMutex<Option<Arc<dyn HostCommands>>>>,
    /// Live publications written by the plugin's publish path. Read for
    /// `/api/health`, the boot frame, and new-subscription catch-up.
    host_published: Arc<StdMutex<HostPublishedState>>,
    /// Cached gzip representations of the embedded client (mount-specific for
    /// `index.html`).
    gzip: GzipAssets,
}

impl ServerMeta {
    fn has_live_host(&self) -> bool {
        self.live_host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    fn host_status(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status
            .clone()
    }

    fn host_catalog(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .catalog
            .clone()
    }

    fn host_snapshot(&self) -> Option<JsonValue> {
        self.host_published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot
            .clone()
    }

    fn touch(&self) {
        self.status.touch();
    }

    fn idle_for(&self) -> Duration {
        self.status.idle_for()
    }

    /// The server-owned status object with the live host half merged over it.
    ///
    /// Server-owned keys stay authoritative (pids, bind address, token,
    /// connection count, idle timeout, single-client mode, running flag); the
    /// host is authoritative for the live session fields. Without a live
    /// publish the static placeholders remain (standalone server).
    fn status_json(&self) -> JsonValue {
        let host = self.host_status();
        let mut value = self.status.status_json();
        let Some(host) = host else {
            return value;
        };
        let Some(host) = host.as_object() else {
            return value;
        };
        if let Some(object) = value.as_object_mut() {
            for (key, entry) in host {
                if !SERVER_OWNED_STATUS_KEYS.contains(&key.as_str()) {
                    object.insert(key.clone(), entry.clone());
                }
            }
        }
        value
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn accept_loop(
    listener: TcpListener,
    meta: Arc<ServerMeta>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut idle_tick = tokio::time::interval(Duration::from_secs(5));
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = idle_tick.tick(), if meta.status.idle_timeout_ms > 0 => {
                let idle = meta.idle_for();
                if idle >= Duration::from_millis(meta.status.idle_timeout_ms) {
                    request_client_close(
                        &meta.status.client,
                        4010,
                        "idle-timeout",
                    );
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        // Admission control: reject immediately instead of
                        // spawning an unbounded number of connection tasks.
                        let active = Arc::clone(&meta.status.active_connections);
                        let admitted = active.fetch_add(1, Ordering::SeqCst);
                        if admitted >= MAX_CONNECTIONS {
                            active.fetch_sub(1, Ordering::SeqCst);
                            // Non-blocking best effort: a tiny static 503 on
                            // a fresh socket, then close. Handled inline so
                            // over-cap peers cannot spawn unbounded tasks.
                            reject_connection(stream).await;
                            continue;
                        }
                        let guard = ConnectionGuard { active };
                        let meta = Arc::clone(&meta);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            let _guard = guard;
                            let _ = handle_connection(stream, meta, shutdown).await;
                        });
                    }
                    Err(_) => {
                        // Transient accept errors (EMFILE, aborted handshake)
                        // must not kill the server.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }

    // Tear down the single client (if any) and the bridge when the server
    // stops, including the idle-timeout exit.
    request_client_close(&meta.status.client, 1001, "server-stopped");
    meta.bridge.close();
    meta.status.running.store(false, Ordering::SeqCst);
}

/// Decrements the admitted-connection counter on every exit path.
struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answer 503 on an over-cap connection and close it without waiting for a
/// request head.
///
/// `try_write` is best effort and never blocks: the payload fits the send
/// buffer of a fresh socket, and a peer that cannot take it is simply closed.
async fn reject_connection(mut stream: TcpStream) {
    const BODY: &[u8] = br#"{"error":"Too many connections"}"#;
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        BODY.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(BODY);
    let _ = stream.try_write(&bytes);
    let _ = stream.shutdown().await;
}

fn request_client_close(client: &StdMutex<Option<ClientSlot>>, code: u16, reason: &str) {
    let slot = client
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|slot| slot.close.clone());
    if let Some(close) = slot {
        let _ = close.send(Some(CloseFrame {
            code,
            reason: reason.to_owned(),
        }));
    }
}

// ---------------------------------------------------------------------------
// HTTP request parsing / response writing
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    /// Authority from an absolute-form request target, if any.
    authority: Option<String>,
    /// Validated `Content-Length` (body bytes still on the wire).
    content_length: Option<usize>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    head_only: bool,
}

impl HttpResponse {
    fn json(status: u16, value: &JsonValue) -> Self {
        Self {
            status,
            headers: vec![(
                "Content-Type".to_owned(),
                "application/json; charset=utf-8".to_owned(),
            )],
            body: serde_json::to_vec(value).unwrap_or_default(),
            head_only: false,
        }
    }

    fn json_error(status: u16, message: &str) -> Self {
        Self::json(status, &json!({ "error": message }))
    }

    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            304 => "Not Modified",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            426 => "Upgrade Required",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "OK",
        }
    }
}

/// A rejected request: the status to answer with and the reason.
struct HttpParseError {
    status: u16,
    message: String,
}

impl HttpParseError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: 413,
            message: message.into(),
        }
    }
}

/// Read the request head (bounded) and return it plus any bytes already read
/// past the head (the start of a body).
async fn read_http_request(
    stream: &mut TcpStream,
) -> Result<Option<(HttpRequest, Vec<u8>)>, HttpParseError> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    loop {
        if let Some(end) = find_header_end(&buffer) {
            if end > MAX_HTTP_HEAD_BYTES {
                return Err(HttpParseError::bad_request("request head too large"));
            }
            let head = buffer[..end].to_vec();
            let leftover = buffer[end..].to_vec();
            return parse_http_request(&head).map(|request| Some((request, leftover)));
        }
        if buffer.len() > MAX_HTTP_HEAD_BYTES {
            return Err(HttpParseError::bad_request("request head too large"));
        }
        let read = stream.read(&mut chunk).await.map_err(|error| {
            HttpParseError::bad_request(format!("failed to read request: {error}"))
        })?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Index just past the blank line that ends the request head, i.e. the first
/// byte that follows the head (a body, or a frame pipelined behind an upgrade).
///
/// The terminator belongs to the head, not to the leftovers: `leftover` is
/// replayed to the WebSocket codec, and a leading `\r\n\r\n` there decodes as
/// a frame whose first byte is `0x0d` (FIN=0, reserved opcode 13) — a 1002
/// protocol error that closes every connection before its first real frame.
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

fn is_http_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'a'..=b'z'
    )
}

fn is_http_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| (byte >= 0x20 && byte != 0x7f) || byte == b'\t')
}

fn is_http_header_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_http_token_byte)
}

/// Reject bare CR/LF so the only accepted line ending is CRLF.
fn validate_crlf(head: &[u8]) -> Result<(), HttpParseError> {
    let mut index = 0;
    while index < head.len() {
        match head[index] {
            b'\r' => {
                if head.get(index + 1) != Some(&b'\n') {
                    return Err(HttpParseError::bad_request(
                        "bare carriage return in request head",
                    ));
                }
                index += 2;
            }
            b'\n' => {
                return Err(HttpParseError::bad_request(
                    "bare line feed in request head",
                ));
            }
            _ => index += 1,
        }
    }
    Ok(())
}

fn parse_http_request(head: &[u8]) -> Result<HttpRequest, HttpParseError> {
    validate_crlf(head)?;
    let text = std::str::from_utf8(head)
        .map_err(|_| HttpParseError::bad_request("request head is not valid UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpParseError::bad_request("missing request line"))?;
    if request_line.len() > MAX_HTTP_HEADER_LINE_BYTES {
        return Err(HttpParseError::bad_request("request line too long"));
    }

    // Exactly `METHOD SP TARGET SP VERSION`; repeated spaces are malformed.
    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .filter(|method| !method.is_empty() && method.bytes().all(is_http_token_byte))
        .ok_or_else(|| HttpParseError::bad_request("missing request method"))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .filter(|target| !target.is_empty())
        .ok_or_else(|| HttpParseError::bad_request("missing request target"))?;
    let version = parts
        .next()
        .ok_or_else(|| HttpParseError::bad_request("missing HTTP version"))?;
    if parts.next().is_some() {
        return Err(HttpParseError::bad_request("malformed request line"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpParseError::bad_request("unsupported HTTP version"));
    }

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut header_count = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > MAX_HTTP_HEADER_COUNT {
            return Err(HttpParseError::bad_request("too many headers"));
        }
        if line.len() > MAX_HTTP_HEADER_LINE_BYTES {
            return Err(HttpParseError::bad_request("header line too long"));
        }
        // Obsolete line folding (a leading SP/HTAB) is not accepted.
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(HttpParseError::bad_request("obsolete header folding"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpParseError::bad_request("malformed header line"))?;
        if !is_http_header_name(name) {
            return Err(HttpParseError::bad_request("malformed header name"));
        }
        let value = value.trim();
        if !is_http_header_value(value) {
            return Err(HttpParseError::bad_request("malformed header value"));
        }
        let name = name.to_ascii_lowercase();
        if let Some(existing) = headers.get(&name) {
            match name.as_str() {
                "host" => return Err(HttpParseError::bad_request("duplicate Host header")),
                "content-length" | "transfer-encoding" if existing != value => {
                    return Err(HttpParseError::bad_request(format!(
                        "conflicting {name} headers"
                    )));
                }
                "content-length" | "transfer-encoding" => {}
                _ => {}
            }
            continue;
        }
        headers.insert(name, value.to_owned());
    }

    if version == "HTTP/1.1" && !headers.contains_key("host") {
        return Err(HttpParseError::bad_request("missing Host header"));
    }

    // No endpoint accepts a body, and request smuggling defenses require
    // rejecting ambiguity instead of choosing one framing.
    if headers.contains_key("transfer-encoding") {
        return Err(HttpParseError::bad_request(
            if headers.contains_key("content-length") {
                "Content-Length and Transfer-Encoding must not be combined"
            } else {
                "Transfer-Encoding is not supported"
            },
        ));
    }
    let content_length = match headers.get("content-length") {
        Some(value) => {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpParseError::bad_request("invalid Content-Length header"));
            }
            let length = value
                .parse::<usize>()
                .map_err(|_| HttpParseError::bad_request("invalid Content-Length header"))?;
            if length > MAX_HTTP_BODY_BYTES {
                return Err(HttpParseError::payload_too_large(format!(
                    "request body exceeds {MAX_HTTP_BODY_BYTES} bytes"
                )));
            }
            Some(length)
        }
        None => None,
    };

    // Absolute-form targets (proxies) carry a scheme and authority. The
    // authority is validated against Host later; keep the path form here.
    let mut target_path = target.to_owned();
    let mut authority = None;
    if let Some(rest) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
    {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let parsed_authority = &rest[..end];
        if parsed_authority.is_empty() {
            return Err(HttpParseError::bad_request(
                "absolute-form target has no authority",
            ));
        }
        authority = Some(parsed_authority.to_owned());
        target_path = if end == rest.len() {
            "/".to_owned()
        } else if rest.as_bytes()[end] == b'/' {
            rest[end..].to_owned()
        } else {
            format!("/{}", &rest[end..])
        };
    }

    let (path, query) = match target_path.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target_path.as_str(), ""),
    };
    let path = path.split('#').next().unwrap_or("/").to_owned();
    let path = if path.is_empty() {
        "/".to_owned()
    } else {
        path
    };

    Ok(HttpRequest {
        method,
        path,
        query: query.to_owned(),
        headers,
        authority,
        content_length,
    })
}

/// The write side of one response, wrapped in [`WRITE_TIMEOUT`] so a peer
/// that stops reading cannot pin the connection task forever.
async fn write_http_response(
    stream: &mut TcpStream,
    response: &HttpResponse,
) -> std::io::Result<()> {
    match tokio::time::timeout(WRITE_TIMEOUT, write_http_response_inner(stream, response)).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            ErrorKind::TimedOut,
            "http response write timed out",
        )),
    }
}

async fn write_http_response_inner(
    stream: &mut TcpStream,
    response: &HttpResponse,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, response.reason());
    for (name, value) in &response.headers {
        // Defense in depth: never emit a header whose name/value could split
        // the response (finding 16).
        if !is_http_header_name(name) || !is_http_header_value(value) {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if response.status != 304 {
        head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    if response.status != 304 && !response.head_only {
        stream.write_all(&response.body).await?;
    }
    stream.flush().await
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

async fn handle_connection(
    mut stream: TcpStream,
    meta: Arc<ServerMeta>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let (request, leftover) =
        match tokio::time::timeout(REQUEST_TIMEOUT, read_http_request(&mut stream)).await {
            Ok(Ok(Some(parsed))) => parsed,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => {
                // Malformed or oversized heads get a bounded 400/413 and the
                // socket is closed deliberately.
                let response = HttpResponse::json_error(error.status, &error.message);
                let _ = write_http_response(&mut stream, &response).await;
                let _ = stream.shutdown().await;
                return Ok(());
            }
            Err(_) => return Err("request timed out".to_owned()),
        };
    meta.touch();

    if let Err((status, message)) = validate_request_authority(&meta, &request) {
        let response = HttpResponse::json_error(status, &message);
        let _ = write_http_response(&mut stream, &response).await;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if is_websocket_upgrade(&request) {
        // RFC 6455 4.2.1: an opening handshake is always a GET.
        if request.method != "GET" {
            let response = HttpResponse::json_error(400, "a WebSocket upgrade must use GET");
            let _ = write_http_response(&mut stream, &response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        if request.content_length.unwrap_or(0) > 0 {
            let response =
                HttpResponse::json_error(400, "a WebSocket upgrade must not carry a body");
            let _ = write_http_response(&mut stream, &response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        return serve_websocket(stream, request, leftover, meta, shutdown).await;
    }

    // Drain a small declared body before answering so a proxy does not see an
    // RST instead of the response. The cap is enforced by the parser.
    if let Some(length) = request.content_length {
        if length > 0
            && request
                .header("expect")
                .map(|value| value.eq_ignore_ascii_case("100-continue"))
                .unwrap_or(false)
        {
            let _ = tokio::time::timeout(
                WRITE_TIMEOUT,
                stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n"),
            )
            .await;
        }
        // One total deadline for the body, so a trickling sender cannot
        // reset a per-read timer indefinitely.
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            drain_http_body(&mut stream, length, leftover.len()),
        )
        .await
        .map_err(|_| "request body timed out".to_owned())??;
    }

    let response = handle_http(&meta, &request).await;
    write_http_response(&mut stream, &response)
        .await
        .map_err(|error| format!("failed to write response: {error}"))?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn drain_http_body(
    stream: &mut TcpStream,
    declared: usize,
    already_read: usize,
) -> Result<(), String> {
    let mut remaining = declared.saturating_sub(already_read);
    let mut buffer = [0u8; 4096];
    while remaining > 0 {
        let take = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..take])
            .await
            .map_err(|error| format!("failed to read request body: {error}"))?;
        if read == 0 {
            return Err("connection closed while reading the request body".to_owned());
        }
        remaining -= read;
    }
    Ok(())
}

/// Split `host[:port]` / `[v6]:port` into a lowercase host and optional port.
fn split_authority(authority: &str) -> Option<(String, Option<u16>)> {
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains('/')
        || authority.contains('?')
        || authority.contains('#')
    {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail {
            "" => None,
            _ => Some(parse_authority_port(tail.strip_prefix(':')?)?),
        };
        return Some((host.to_ascii_lowercase(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() || host.contains(':') {
                return None;
            }
            Some((host.to_ascii_lowercase(), Some(parse_authority_port(port)?)))
        }
        None => Some((authority.to_ascii_lowercase(), None)),
    }
}

fn parse_authority_port(port: &str) -> Option<u16> {
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok()
}

/// Whether a Host header value may refer to this server's bind address.
fn host_matches_bind(bound: &str, candidate: &str) -> bool {
    let bound = bound.trim();
    let bound = bound
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(bound)
        .to_ascii_lowercase();
    let candidate = candidate.to_ascii_lowercase();
    if candidate == bound {
        return true;
    }
    let candidate_ip = candidate.parse::<IpAddr>().ok();
    if candidate == "localhost" {
        return bound
            .parse::<IpAddr>()
            .map(|address| address.is_loopback() || address.is_unspecified())
            .unwrap_or(bound == "localhost");
    }
    match bound.parse::<IpAddr>() {
        // A wildcard bind accepts any literal address (the client's address
        // bar may be a LAN IP) but never a DNS name, which blocks rebinding.
        Ok(address) if address.is_unspecified() => candidate_ip.is_some(),
        Ok(address) if address.is_loopback() => candidate_ip
            .map(|candidate| candidate.is_loopback())
            .unwrap_or(false),
        Ok(address) => candidate_ip == Some(address),
        Err(_) => bound == "localhost" && candidate_ip.map(|ip| ip.is_loopback()).unwrap_or(false),
    }
}

fn default_origin_port(scheme: &str) -> u16 {
    match scheme {
        "https" | "wss" => 443,
        _ => 80,
    }
}

fn validate_origin(origin: &str, host: Option<&str>, port: u16) -> Result<(), (u16, String)> {
    if origin.eq_ignore_ascii_case("null") {
        return Err((403, "Origin `null` is not accepted".to_owned()));
    }
    let (scheme, authority) = origin
        .split_once("://")
        .ok_or_else(|| (403, "Invalid Origin header".to_owned()))?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss") {
        return Err((403, "Origin scheme is not accepted".to_owned()));
    }
    let (origin_host, origin_port) =
        split_authority(authority).ok_or_else(|| (403, "Invalid Origin header".to_owned()))?;
    if origin_port.unwrap_or(default_origin_port(&scheme)) != port {
        return Err((403, "Cross-origin request rejected".to_owned()));
    }
    if let Some(host) = host {
        if !origin_host.eq_ignore_ascii_case(host) {
            return Err((403, "Cross-origin request rejected".to_owned()));
        }
    }
    Ok(())
}

/// Validate `Host` against the bound address/port and `Origin` against the
/// request's authority (finding 7).
fn validate_request_authority(
    meta: &ServerMeta,
    request: &HttpRequest,
) -> Result<(), (u16, String)> {
    let bound_port = meta.status.port;
    let Some(host_header) = request.header("host") else {
        // HTTP/1.0 clients may omit Host; a present Origin still has to match.
        if let Some(origin) = request.header("origin") {
            validate_origin(origin, None, bound_port)?;
        }
        return Ok(());
    };
    let (host, port) =
        split_authority(host_header).ok_or_else(|| (400, "Invalid Host header".to_owned()))?;
    if port.unwrap_or(80) != bound_port {
        return Err((
            403,
            format!("Host `{host_header}` is not this phone server (port {bound_port})"),
        ));
    }
    if !host_matches_bind(&meta.status.host, &host) {
        return Err((
            403,
            format!("Host `{host}` is not accepted by this phone server"),
        ));
    }
    if let Some(authority) = &request.authority {
        let (absolute_host, absolute_port) = split_authority(authority)
            .ok_or_else(|| (400, "Invalid absolute-form authority".to_owned()))?;
        if absolute_host != host || absolute_port.unwrap_or(80) != port.unwrap_or(80) {
            return Err((
                400,
                "absolute-form authority does not match the Host header".to_owned(),
            ));
        }
    }
    if let Some(origin) = request.header("origin") {
        validate_origin(origin, Some(&host), bound_port)?;
    }
    Ok(())
}

/// Whether a comma-separated header value contains `token` as a full token.
///
/// `Connection` is a token list (`keep-alive, Upgrade`), not a free-form
/// string: a substring test would accept `UpgradeX` and other lookalikes.
fn header_contains_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

/// Whether an `Accept-Encoding` value accepts the `gzip` coding.
///
/// The header is a comma-separated coding list with optional `;q=` weights.
/// Parameters are trimmed; an explicit `gzip;q=0` refuses the coding even when
/// `*` would allow it; `*` accepts gzip only when `gzip` is not listed; an
/// absent or empty header means identity. An unparsable weight is treated as
/// `0` (refusal), matching the conservative reading of the RFC.
fn accepts_gzip(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let mut gzip_quality: Option<f32> = None;
    let mut wildcard_quality: Option<f32> = None;

    for element in value.split(',') {
        let mut parts = element.split(';');
        let coding = parts.next().unwrap_or_default().trim();
        if coding.is_empty() {
            continue;
        }
        let mut quality = 1.0f32;
        for parameter in parts {
            let Some((name, raw)) = parameter.split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("q") {
                quality = raw.trim().parse::<f32>().unwrap_or(0.0);
            }
        }
        if coding.eq_ignore_ascii_case("gzip") {
            gzip_quality = Some(gzip_quality.map_or(quality, |current| current.max(quality)));
        } else if coding == "*" {
            wildcard_quality =
                Some(wildcard_quality.map_or(quality, |current| current.max(quality)));
        }
    }

    match gzip_quality {
        Some(quality) => quality > 0.0,
        None => wildcard_quality.is_some_and(|quality| quality > 0.0),
    }
}

fn is_websocket_upgrade(request: &HttpRequest) -> bool {
    // Both headers are comma-separated token lists (`Upgrade: h2c, websocket`,
    // `Connection: keep-alive, Upgrade`); a substring match would also accept
    // lookalikes such as `UpgradeX`.
    request
        .header("upgrade")
        .map(|value| header_contains_token(value, "websocket"))
        .unwrap_or(false)
        && request
            .header("connection")
            .map(|value| header_contains_token(value, "upgrade"))
            .unwrap_or(false)
}

/// Strip the per-instance reverse-proxy mount (`/phone-<port>`), returning the
/// routed path and the mount prefix used for `index.html` rewriting.
fn strip_mount(path: &str, port: u16) -> (String, String) {
    let prefix = format!("/phone-{port}");
    if path == prefix {
        ("/".to_owned(), prefix)
    } else if let Some(rest) = path.strip_prefix(&format!("{prefix}/")) {
        (format!("/{rest}"), prefix)
    } else {
        (path.to_owned(), prefix)
    }
}

async fn handle_http(meta: &ServerMeta, request: &HttpRequest) -> HttpResponse {
    let (path, mount) = strip_mount(&request.path, meta.status.port);
    let head_only = request.method == "HEAD";

    // The reference strips the mount prefix before routing.
    if path == CONTROL_STOP_PATH {
        if request.method != "POST" {
            return HttpResponse::json_error(405, "Method not allowed");
        }
        return HttpResponse::json_error(
            403,
            "Forbidden: stopping a phone server from another pi session is not supported.",
        );
    }

    if path == "/api/health" {
        return HttpResponse {
            status: 200,
            headers: vec![
                (
                    "Content-Type".to_owned(),
                    "application/json; charset=utf-8".to_owned(),
                ),
                ("Cache-Control".to_owned(), "no-store".to_owned()),
            ],
            body: serde_json::to_vec(&meta.status_json()).unwrap_or_default(),
            head_only,
        };
    }

    if let Some(rest) = path.strip_prefix("/api/image/") {
        return handle_image(meta, rest, head_only);
    }

    if path == "/api/quota" {
        if request.method != "GET" && request.method != "HEAD" {
            return HttpResponse::json_error(405, "Method not allowed");
        }
        let query = parse_query(&request.query);
        let provider = query.get("provider").map(String::as_str);
        let model_id = query.get("modelId").map(String::as_str);
        let quota = quota_for_model(provider, model_id).await;
        return HttpResponse {
            status: 200,
            headers: vec![
                (
                    "Content-Type".to_owned(),
                    "application/json; charset=utf-8".to_owned(),
                ),
                ("Cache-Control".to_owned(), "no-store".to_owned()),
            ],
            body: serde_json::to_vec(&quota).unwrap_or_default(),
            head_only,
        };
    }

    // Unknown API route: an honest 404 instead of the reference's static
    // index fallback (the client never requests one; a 200 HTML body for an
    // API path is a debugging trap).
    if path == "/api" || path.starts_with("/api/") {
        return HttpResponse::json_error(404, "Not found");
    }

    if request.method != "GET" && request.method != "HEAD" {
        return HttpResponse::json_error(405, "Method not allowed");
    }

    if path_has_traversal(&path) {
        return HttpResponse::json_error(403, "Forbidden");
    }

    let asset_path = if path == "/" { "/index.html" } else { &path };
    if let Some((asset_key, content_type, bytes)) = assets::lookup_with_path(asset_path) {
        let cache_control = if matches!(
            asset_key.rsplit('.').next(),
            Some("html" | "js" | "css" | "webmanifest" | "json")
        ) || asset_key == "sw.js"
        {
            "no-cache"
        } else {
            "public, max-age=300"
        };
        let identity_etag = assets::etag(asset_key).unwrap_or_default();
        let gzip_body = if accepts_gzip(request.header("accept-encoding")) {
            meta.gzip.get(asset_key)
        } else {
            None
        };
        // Each representation has its own strong tag, so a conditional
        // request can only match the representation that was negotiated.
        let etag = if gzip_body.is_some() {
            assets::gzip_etag(&identity_etag).unwrap_or_default()
        } else {
            identity_etag
        };

        if !etag.is_empty() {
            let not_modified = request
                .header("if-none-match")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .any(|candidate| candidate == etag || candidate == format!("W/{etag}"))
                })
                .unwrap_or(false);
            if not_modified {
                let mut headers = vec![
                    ("Content-Type".to_owned(), content_type.to_owned()),
                    ("Cache-Control".to_owned(), cache_control.to_owned()),
                    ("Vary".to_owned(), "Accept-Encoding".to_owned()),
                ];
                if gzip_body.is_some() {
                    headers.push(("Content-Encoding".to_owned(), "gzip".to_owned()));
                }
                headers.push(("ETag".to_owned(), etag));
                return HttpResponse {
                    status: 304,
                    headers,
                    body: Vec::new(),
                    head_only: true,
                };
            }
        }

        let body = match gzip_body {
            Some(compressed) => compressed.to_vec(),
            None if asset_key == "index.html" => rewrite_index_base(bytes, &mount),
            None => bytes.to_vec(),
        };
        let mut headers = vec![
            ("Content-Type".to_owned(), content_type.to_owned()),
            ("Cache-Control".to_owned(), cache_control.to_owned()),
            ("Vary".to_owned(), "Accept-Encoding".to_owned()),
        ];
        if gzip_body.is_some() {
            headers.push(("Content-Encoding".to_owned(), "gzip".to_owned()));
        }
        if !etag.is_empty() {
            headers.push(("ETag".to_owned(), etag));
        }
        return HttpResponse {
            status: 200,
            headers,
            body,
            head_only,
        };
    }

    // Missing static file: the reference serves `index.html` with `no-store`.
    match assets::lookup_with_path("/index.html") {
        Some((asset_key, content_type, bytes)) => {
            let gzip_body = if accepts_gzip(request.header("accept-encoding")) {
                meta.gzip.get(asset_key)
            } else {
                None
            };
            let mut headers = vec![
                ("Content-Type".to_owned(), content_type.to_owned()),
                ("Cache-Control".to_owned(), "no-store".to_owned()),
                ("Vary".to_owned(), "Accept-Encoding".to_owned()),
            ];
            if gzip_body.is_some() {
                headers.push(("Content-Encoding".to_owned(), "gzip".to_owned()));
            }
            HttpResponse {
                status: 200,
                headers,
                body: match gzip_body {
                    Some(compressed) => compressed.to_vec(),
                    None => rewrite_index_base(bytes, &mount),
                },
                head_only,
            }
        }
        None => HttpResponse::json_error(500, "Failed to serve index.html"),
    }
}

fn path_has_traversal(path: &str) -> bool {
    path.contains('\\')
        || path.contains('\0')
        || path.contains('%')
        || path.split('/').any(|segment| segment == "..")
}

fn rewrite_index_base(bytes: &[u8], mount: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    text.replace(
        "window.PI_PHONE_BASE = \"/\";",
        &format!("window.PI_PHONE_BASE = \"{mount}\";"),
    )
    .into_bytes()
}

fn handle_image(meta: &ServerMeta, key: &str, head_only: bool) -> HttpResponse {
    if key.is_empty()
        || key.len() > 64
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return HttpResponse::json_error(400, "Invalid image key");
    }
    let entry = meta
        .images
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key);
    match entry {
        Some((bytes, mime_type)) => HttpResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_owned(), mime_type),
                (
                    "Cache-Control".to_owned(),
                    "public, max-age=31536000, immutable".to_owned(),
                ),
            ],
            body: bytes.to_vec(),
            head_only,
        },
        None => HttpResponse::json_error(404, "Image not found"),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        result.insert(percent_decode(key), percent_decode(value));
    }
    result
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1]);
                let low = hex_value(bytes[index + 2]);
                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Quota (reference phone-quota.ts)
// ---------------------------------------------------------------------------

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const SPARK_MODEL_ID: &str = "gpt-5.3-codex-spark";
const SPARK_LIMIT_NAME: &str = "GPT-5.3-Codex-Spark";
const MISSING_AUTH_PREFIX: &str = "Missing openai-codex OAuth access/accountId";

async fn quota_for_model(provider: Option<&str>, model_id: Option<&str>) -> JsonValue {
    let visible = provider == Some("openai-codex")
        && model_id
            .map(|id| id.to_ascii_lowercase().starts_with("gpt-"))
            .unwrap_or(false);
    if !visible {
        return json!({
            "visible": false,
            "limited": false,
            "primaryWindow": JsonValue::Null,
            "secondaryWindow": JsonValue::Null,
        });
    }

    match request_codex_usage(model_id.unwrap_or_default()).await {
        Ok(quota) => quota,
        Err(error) => json!({
            "visible": false,
            "limited": false,
            "primaryWindow": JsonValue::Null,
            "secondaryWindow": JsonValue::Null,
            "error": error,
        }),
    }
}

/// Credential file the Codex quota route authenticates with
/// (`<agent-dir>/auth.json`, entry `openai-codex`).
///
/// The agent dir comes from the ONE canonical resolver
/// ([`crate::ai::providers::agent_dir`]: `PIR_AGENT_DIR` →
/// `PIR_CODING_AGENT_DIR` → `PI_CODING_AGENT_DIR` → `~/.pi/agent`), resolved
/// per call. This plugin used to carry a private resolver that read only
/// `PI_CODING_AGENT_DIR`, so a harness pinning the documented primary variable
/// (`PIR_AGENT_DIR`) made the quota route read the operator's real
/// `~/.pi/agent/auth.json` — a credential read outside the configured agent
/// dir. Public so tests can assert the resolved path without reading any
/// credential file.
pub fn quota_auth_path() -> PathBuf {
    crate::ai::providers::agent_dir().join("auth.json")
}

async fn request_codex_usage(model_id: &str) -> Result<JsonValue, String> {
    let auth_path = quota_auth_path();
    let auth_raw = tokio::fs::read_to_string(&auth_path)
        .await
        .map_err(|error| format!("{MISSING_AUTH_PREFIX} in {}: {error}", auth_path.display()))?;
    let auth: JsonValue = serde_json::from_str(&auth_raw)
        .map_err(|error| format!("Failed to parse {}: {error}", auth_path.display()))?;
    let entry = auth.get("openai-codex").and_then(JsonValue::as_object);
    let oauth =
        entry.filter(|entry| entry.get("type").and_then(JsonValue::as_str) == Some("oauth"));
    let access = oauth
        .and_then(|entry| entry.get("access"))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let account = oauth
        .and_then(|entry| entry.get("accountId").or_else(|| entry.get("account_id")))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (Some(access), Some(account)) = (access, account) else {
        return Err(format!("{MISSING_AUTH_PREFIX} in {}", auth_path.display()));
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("Failed to create HTTP client: {error}"))?;
    let response = client
        .get(CODEX_USAGE_URL)
        .header("accept", "*/*")
        .header("authorization", format!("Bearer {access}"))
        .header("chatgpt-account-id", account)
        .header("content-type", "application/json")
        .header("user-agent", "codex-cli")
        .send()
        .await
        .map_err(|error| format!("Codex usage request failed: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("Codex usage request failed ({})", status.as_u16()));
    }
    let usage: JsonValue = response
        .json()
        .await
        .map_err(|error| format!("Codex usage response was not JSON: {error}"))?;

    let bucket = select_rate_limit_bucket(&usage, model_id);
    let primary = build_quota_window("5h", bucket.and_then(|bucket| bucket.get("primary_window")));
    let secondary = build_quota_window(
        "7d",
        bucket.and_then(|bucket| bucket.get("secondary_window")),
    );
    let limited = bucket
        .and_then(|bucket| bucket.get("limit_reached"))
        .and_then(JsonValue::as_bool)
        == Some(true)
        || bucket
            .and_then(|bucket| bucket.get("allowed"))
            .and_then(JsonValue::as_bool)
            == Some(false);

    Ok(json!({
        "visible": primary.is_some() || secondary.is_some(),
        "limited": limited,
        "primaryWindow": primary.unwrap_or(JsonValue::Null),
        "secondaryWindow": secondary.unwrap_or(JsonValue::Null),
    }))
}

fn normalize_rate_limit_bucket(value: &JsonValue) -> Option<&JsonMap<String, JsonValue>> {
    let record = value.as_object()?;
    let has_field = [
        "primary_window",
        "secondary_window",
        "limit_reached",
        "allowed",
    ]
    .iter()
    .any(|field| record.contains_key(*field));
    has_field.then_some(record)
}

fn select_rate_limit_bucket<'a>(
    usage: &'a JsonValue,
    model_id: &str,
) -> Option<&'a JsonMap<String, JsonValue>> {
    if model_id == SPARK_MODEL_ID {
        let additional = usage.get("additional_rate_limits")?;
        let entries: Vec<&JsonValue> = if let Some(array) = additional.as_array() {
            array.iter().collect()
        } else {
            additional.as_object()?.values().collect()
        };
        for entry in entries {
            let Some(record) = entry.as_object() else {
                continue;
            };
            if record
                .get("limit_name")
                .and_then(JsonValue::as_str)
                .map(str::trim)
                != Some(SPARK_LIMIT_NAME)
            {
                continue;
            }
            if let Some(bucket) = record
                .get("rate_limit")
                .and_then(normalize_rate_limit_bucket)
            {
                return Some(bucket);
            }
        }
        return None;
    }
    usage
        .get("rate_limit")
        .and_then(normalize_rate_limit_bucket)
}

fn build_quota_window(label: &str, window: Option<&JsonValue>) -> Option<JsonValue> {
    let used_percent = window
        .and_then(|window| window.get("used_percent"))
        .and_then(JsonValue::as_f64)
        .filter(|value| value.is_finite())?;
    let left_percent = clamp_percent(100.0 - used_percent);
    let rounded_left = left_percent.round();
    let rounded_used = clamp_percent(used_percent).round();
    Some(json!({
        "label": label,
        "leftPercent": rounded_left,
        "usedPercent": rounded_used,
        "resetAfterSeconds": get_reset_seconds(window),
        "text": format!("{}%", rounded_left as i64),
    }))
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn get_reset_seconds(window: Option<&JsonValue>) -> Option<f64> {
    let window = window?;
    if let Some(seconds) = window
        .get("reset_after_seconds")
        .and_then(JsonValue::as_f64)
    {
        return Some(seconds);
    }
    let reset_at = window.get("reset_at").and_then(JsonValue::as_f64)?;
    let reset_at = if reset_at > 100_000_000_000.0 {
        reset_at / 1000.0
    } else {
        reset_at
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    Some((reset_at - now).max(0.0))
}

// ---------------------------------------------------------------------------
// WebSocket (RFC 6455)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ClientSlot {
    id: u64,
    close: watch::Sender<Option<CloseFrame>>,
    /// Hard-abort signal for the reader/writer tasks (replacement/stop).
    abort: watch::Sender<bool>,
    /// The bridge subscriber this connection owns, so a replacement can
    /// remove the subscription immediately instead of waiting for the old
    /// tasks to notice the close signal.
    bridge_client_id: u64,
}

#[derive(Clone, Debug)]
struct CloseFrame {
    code: u16,
    reason: String,
}

async fn serve_websocket(
    mut stream: TcpStream,
    request: HttpRequest,
    leftover: Vec<u8>,
    meta: Arc<ServerMeta>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let (path, _mount) = strip_mount(&request.path, meta.status.port);
    if path != "/ws" {
        // The reference destroys the socket on any other upgrade path.
        return Err("websocket upgrade on an unsupported path".to_owned());
    }

    // RFC 6455 4.2.1: the version must be exactly 13; otherwise answer 426
    // with the version the server supports (4.2.2).
    let version = request.header("sec-websocket-version").unwrap_or("");
    if version != "13" {
        let response = HttpResponse {
            status: 426,
            headers: vec![
                ("Sec-WebSocket-Version".to_owned(), "13".to_owned()),
                (
                    "Content-Type".to_owned(),
                    "application/json; charset=utf-8".to_owned(),
                ),
            ],
            body: serde_json::to_vec(&json!({ "error": "Unsupported WebSocket version" }))
                .unwrap_or_default(),
            head_only: false,
        };
        let _ = write_http_response(&mut stream, &response).await;
        return Ok(());
    }

    // RFC 6455 4.2.1: the key is base64 for exactly 16 random bytes. A
    // malformed or missing key is a bad request, not a connection drop.
    //
    // This stays here because it is *policy*: the crate validates the whole
    // handshake before a callback can run and cancels the connection without
    // an HTTP response on a bad key, while this server answers 400. The same
    // applies to the 426 above and to the method/body checks in
    // [`handle_connection`].
    let key = request.header("sec-websocket-key").unwrap_or("");
    if !is_valid_websocket_key(key) {
        let response = HttpResponse::json_error(400, "invalid Sec-WebSocket-Key");
        let _ = write_http_response(&mut stream, &response).await;
        return Ok(());
    }

    // The 101 itself is the crate's: `accept_async_with_config` parses the
    // opening handshake, derives `Sec-WebSocket-Accept` (RFC 6455 4.2.2) and
    // writes the response through this stream. The head it parses is replayed
    // from memory because this module's HTTP layer already read it (the
    // audited stage 2 replaces that reader with `hyper`); the bytes that
    // arrived after the head are replayed too, so a client that pipelines its
    // first frame behind the upgrade does not lose it.
    //
    // Everything from the 101 down is the crate's, so the RFC 6455 details
    // this file used to hand-roll are guaranteed by construction:
    //
    // - Masking: a client frame without a mask is a protocol error
    //   (`UnmaskedFrameFromClient`) unless `accept_unmasked_frames` is set,
    //   which it is not (RFC 6455 5.1).
    // - Fragmentation, control-frame rules (never fragmented, payload <= 125),
    //   reserved bits/opcodes, UTF-8 validation of text and close reasons, and
    //   close-code validity are enforced in `tungstenite::protocol`.
    // - Ping is answered with a matching pong by the read side itself, even
    //   mid-fragmented-message; the writer never has to relay it.
    // - The close handshake: a peer close is echoed once, and the server, in
    //   `Role::Server`, closes the TCP connection first after both sides have
    //   closed (RFC 6455 7.1.1 / 5.5.1) — no hand-written drain loop.
    // - 4009/4010 are `CloseCode::Library`, which `is_allowed()` accepts, so
    //   both pass through unchanged (previously validated by hand).
    //
    // `UpgradedStream` replays the head and feeds the byte counter that
    // preserves the idle-vs-stalled-frame deadline split (see
    // [`read_ws_message`]).
    let progress = Arc::new(ReadProgress::default());
    let upgraded = UpgradedStream::new(
        upgrade_request_head(&request),
        leftover,
        stream,
        Arc::clone(&progress),
    );
    let mut ws = tokio::time::timeout(
        WRITE_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(upgraded, Some(ws_config())),
    )
    .await
    .map_err(|_| "timed out in the websocket handshake".to_owned())?
    .map_err(|error| format!("websocket handshake failed: {error}"))?;

    let query = parse_query(&request.query);
    let token_ok = meta.status.token.is_empty()
        || query.get("token").map(String::as_str) == Some(meta.status.token.as_str());
    if !token_ok {
        close_ws(&mut ws, 1008, "invalid-token").await;
        return Ok(());
    }
    if *shutdown.borrow() {
        close_ws(&mut ws, 1001, "server-stopped").await;
        return Ok(());
    }

    // Single-client admission is a hard invariant: the slot lock serializes
    // concurrent upgrades, the newcomer subscribes while the incumbent is
    // still present (so a failed admission never kicks it), and then the old
    // connection is signalled, hard-aborted, and removed from the bridge.
    let client_id = meta.next_client_id.fetch_add(1, Ordering::Relaxed);
    let (close_tx, close_rx) = watch::channel(None);
    let (abort_tx, abort_rx) = watch::channel(false);
    let subscribed = {
        let mut slot = meta
            .status
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match meta.bridge.subscribe() {
            Ok(subscriber) => {
                if let Some(previous) = slot.take() {
                    let _ = previous.close.send(Some(CloseFrame {
                        code: 4009,
                        reason: "replaced-by-new-client".to_owned(),
                    }));
                    let _ = previous.abort.send(true);
                    // Remove the old subscription now: waiting for a wedged
                    // reader/writer would leak the slot and eventually make
                    // legitimate replacements fail with 1013.
                    meta.bridge.disconnect_client(previous.bridge_client_id);
                }
                *slot = Some(ClientSlot {
                    id: client_id,
                    close: close_tx.clone(),
                    abort: abort_tx.clone(),
                    bridge_client_id: subscriber.id(),
                });
                Ok(subscriber)
            }
            Err(error) => Err(error),
        }
    };
    let subscriber = match subscribed {
        Ok(subscriber) => subscriber,
        Err(SubscribeError::Closed) => {
            close_ws(&mut ws, 1011, "bridge-closed").await;
            return Ok(());
        }
        Err(SubscribeError::TooManyClients { .. }) => {
            close_ws(&mut ws, 1013, "too-many-clients").await;
            return Ok(());
        }
    };

    meta.touch();

    // Boot frames travel through the bridge exactly like live host events, so
    // the transport has a single envelope path. With a live host, replay the
    // latest real status/catalog/snapshot immediately: the client must not
    // have to send `refresh` to see the mirrored session. Without one, the
    // static placeholder catalog remains the only source of truth.
    meta.bridge.dispatch_host_event(json!({
        "type": "status",
        "data": meta.status_json(),
    }));
    if meta.has_live_host() {
        if let Some(catalog) = meta.host_catalog() {
            meta.bridge.dispatch_host_event(json!({
                "type": "catalog",
                "data": catalog,
            }));
        }
        if let Some(snapshot) = meta.host_snapshot() {
            let mut event = snapshot;
            if let Some(object) = event.as_object_mut() {
                object.insert("type".to_owned(), JsonValue::String("snapshot".to_owned()));
            }
            meta.bridge.dispatch_host_event(event);
        }
    } else {
        meta.bridge.dispatch_host_event(json!({
            "type": "catalog",
            "data": { "activeSessionId": JsonValue::Null, "sessions": [] },
        }));
    }

    // Both halves must be drained: a reader parked in `next()` keeps the
    // underlying `BiLock` of the split stream busy, and dropping the writer
    // first would leave the reader owning the socket.
    let (writer, reader) = ws.split();
    let bridge_client_id = subscriber.id();
    let result = websocket_session(
        reader,
        writer,
        Arc::clone(&progress),
        Arc::clone(&meta),
        meta.bridge.clone(),
        bridge_client_id,
        subscriber,
        close_rx,
        abort_tx,
        abort_rx,
        shutdown,
    )
    .await;

    // Only clear the slot if it is still ours (a newer client may have
    // replaced us already).
    {
        let mut slot = meta
            .status
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.as_ref().map(|slot| slot.id) == Some(client_id) {
            *slot = None;
        }
    }
    result
}

/// One complete client-originated transport action, handed from the reader
/// task to the writer task through a bounded queue.
enum WsAction {
    Text(String),
    Binary(Vec<u8>),
    /// A protocol failure diagnosed by the reader: send this close and stop.
    Close(u16, String),
    /// The peer started the close handshake. tungstenite has already queued
    /// the reply close frame (the peer's own code, or 1002 for a code the RFC
    /// does not allow), so the writer must flush it and stop without adding a
    /// frame of its own.
    PeerClosed,
}

/// Bounded reader -> writer queue. Protocol echoes are rare; if a client
/// somehow floods them, the reader stops instead of buffering without bound.
const WS_ACTION_QUEUE: usize = 32;

fn bridge_close_frame(reason: Option<CloseReason>) -> (u16, &'static str) {
    match reason {
        Some(CloseReason::SlowConsumer) => (1013, "slow-consumer"),
        Some(CloseReason::ServerClosed) => (1001, "server-stopped"),
        Some(CloseReason::ClientDropped) | None => (1000, "closed"),
    }
}

/// Drive one connection: a reader task (messages -> bridge/actions) and a
/// writer loop (bridge queue/actions/control -> socket).
///
/// The split is kept from the hand-written transport (it is the point of the
/// two halves): the read side is its own task, so an outbound bridge line can
/// never cancel a partially-read message. tungstenite backs that with a second
/// guarantee — a partial frame lives in the socket's codec, not in the read
/// future — but the tasks still keep the writer's `select!` off the read path
/// and let tungstenite flush pongs/close replies while the writer is parked.
// The single WS connection driver; grouping its borrows into a struct would
// only re-pack the same arguments without making the handoff clearer.
#[allow(clippy::too_many_arguments)]
async fn websocket_session(
    reader: WsSource,
    writer: WsWriter,
    progress: Arc<ReadProgress>,
    meta: Arc<ServerMeta>,
    bridge: SessionBridge,
    client_id: u64,
    subscriber: BridgeSubscriber,
    close_rx: watch::Receiver<Option<CloseFrame>>,
    abort_tx: watch::Sender<bool>,
    abort_rx: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let (actions_tx, actions_rx) = mpsc::channel(WS_ACTION_QUEUE);

    let mut reader = tokio::spawn(websocket_reader(
        reader,
        Arc::clone(&progress),
        meta,
        bridge,
        client_id,
        actions_tx,
        abort_rx,
        shutdown.clone(),
    ));

    let result = websocket_writer(writer, subscriber, actions_rx, close_rx, shutdown).await;

    // RFC 6455 7.1.1 close drain: after our close frame the read side stays
    // open for a bounded window so the peer's close reply (and its FIN) are
    // consumed before the socket drops; a socket dropped with unread bytes
    // answers the close with RST. tungstenite consumes the reply as part of
    // its own close handshake, so waiting for the reader task *is* the drain
    // the hand-written transport implemented with a raw 1 s read loop.
    //
    // A `JoinHandle` may be polled to completion exactly once: polling it
    // again panics with "JoinHandle polled after completion". So the drain
    // result decides whether a second poll is allowed. `Timeout` polls its
    // inner future before its deadline, so `Ok` proves the handle completed
    // (its output is already taken) and must not be re-awaited, while `Err`
    // proves the last poll was still `Pending` -- a completed task that has
    // not been polled since only has its output *staged*, so re-polling it is
    // safe. The same holds for the third poll below (`abort().await`).
    let drain_timed_out = tokio::time::timeout(WS_CLOSE_DRAIN_TIMEOUT, &mut reader)
        .await
        .is_err();

    // A reader parked in a host dispatch must not keep this session (and its
    // bridge subscription) alive; the abort watch cancels the dispatch and
    // the timeout is a final hard stop.
    let _ = abort_tx.send(true);
    if drain_timed_out
        && tokio::time::timeout(WS_TASK_ABORT_TIMEOUT, &mut reader)
            .await
            .is_err()
    {
        reader.abort();
        let _ = reader.await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn websocket_reader(
    mut reader: WsSource,
    progress: Arc<ReadProgress>,
    meta: Arc<ServerMeta>,
    bridge: SessionBridge,
    client_id: u64,
    actions: mpsc::Sender<WsAction>,
    mut abort: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let idle_timeout = ws_idle_timeout(&meta);
    // Separate receivers for the host-dispatch select: a host adapter whose
    // promise never resolves must not keep the reader (and the bridge
    // subscription) alive during replacement/stop.
    let mut dispatch_abort = abort.clone();
    let mut dispatch_shutdown = shutdown.clone();

    loop {
        // A receiver cloned after the signal was sent does not wake on
        // `changed()`; check the current value each iteration as well.
        if *abort.borrow() || *shutdown.borrow() {
            return;
        }
        tokio::select! {
            changed = abort.changed() => {
                if changed.is_err() || *abort.borrow() {
                    return;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            message = read_ws_message(&mut reader, &progress, idle_timeout) => {
                meta.touch();
                let message = match message {
                    Ok(message) => message,
                    // The peer went away; the writer exits when the action
                    // channel closes.
                    Err(WsReadError::Closed) => return,
                    Err(WsReadError::Idle) => {
                        let _ = actions.try_send(WsAction::Close(
                            1001,
                            "idle-timeout".to_owned(),
                        ));
                        return;
                    }
                    Err(WsReadError::Timeout) => {
                        let _ = actions.try_send(WsAction::Close(
                            1002,
                            "frame read timeout".to_owned(),
                        ));
                        return;
                    }
                    Err(WsReadError::Protocol(code, reason)) => {
                        let _ = actions.try_send(WsAction::Close(code, reason));
                        return;
                    }
                };

                // Fragmentation, UTF-8 (1007), the control-frame rules, and
                // the message cap (1009) are tungstenite's: it either yields
                // one complete `Text`/`Binary` message or fails the read with
                // the matching error, so the reader never assembles or
                // validates frames itself.
                match message {
                    Message::Text(text) => {
                        if !process_message(
                            &bridge,
                            client_id,
                            text,
                            &actions,
                            &mut dispatch_abort,
                            &mut dispatch_shutdown,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Message::Binary(payload) => {
                        if actions.try_send(WsAction::Binary(payload)).is_err() {
                            return;
                        }
                    }
                    // tungstenite queued the matching pong while it read the
                    // ping, and flushes it on the next read/flush; the writer
                    // only has to keep going so that flush can happen.
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => {
                        // The close reply is already queued by the read side
                        // (same code and reason the peer sent — RFC 6455
                        // 5.5.1; 1002 when the code is not allowed). Ask the
                        // writer to flush it and stop; a second close frame
                        // here would violate the single-close-echo rule.
                        let _ = actions.send(WsAction::PeerClosed).await;
                        return;
                    }
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

/// Feed one complete text message to the bridge (JSON protocol lines) or echo
/// it (the plain-text transport probe). Returns `false` when the connection
/// should stop.
///
/// Binary messages are echoed by the reader directly; they never carry the
/// protocol.
async fn process_message(
    bridge: &SessionBridge,
    client_id: u64,
    text: String,
    actions: &mpsc::Sender<WsAction>,
    abort: &mut watch::Receiver<bool>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let parsed = serde_json::from_str::<JsonValue>(&text).ok();
    let is_protocol_line = parsed
        .as_ref()
        .map(|value| value.is_object())
        .unwrap_or(false);
    if is_protocol_line {
        let normalized = parsed.as_ref().and_then(normalize_protocol_line);
        let line = normalized.as_deref().unwrap_or(&text);
        // A host dispatch is awaited with the abort/shutdown signals, so a
        // never-resolving adapter cannot wedge replacement or stop.
        let dispatched = tokio::select! {
            dispatched = bridge.handle_client_line(client_id, line) => dispatched,
            changed = abort.changed() => {
                let _ = changed;
                false
            }
            changed = shutdown.changed() => {
                let _ = changed;
                false
            }
        };
        if !dispatched {
            let _ = actions.try_send(WsAction::Close(1001, "bridge-closed".to_owned()));
            return false;
        }
    } else if actions.try_send(WsAction::Text(text)).is_err() {
        return false;
    }
    true
}

/// Rewrite command spellings the client and the response handler disagree on.
///
/// The bridge copies a `Raw` command's `type` into the response `command`, so
/// the client's `path-suggestions` request must reach it as
/// `path_suggestions` (PROTOCOL.md: the client checks
/// `payload.command === "path_suggestions"`). Both the `rpc` and
/// `local-command` envelopes are normalized.
fn normalize_protocol_line(value: &JsonValue) -> Option<String> {
    let (command, nested) = match value.get("kind").and_then(JsonValue::as_str) {
        Some("rpc") | Some("local-command") => (value.get("command")?, true),
        None => (value, false),
        Some(_) => return None,
    };
    if command.get("type").and_then(JsonValue::as_str) != Some("path-suggestions") {
        return None;
    }
    let mut rewritten = value.clone();
    let target = if nested {
        rewritten.get_mut("command")?
    } else {
        &mut rewritten
    };
    target.as_object_mut()?.insert(
        "type".to_owned(),
        JsonValue::String("path_suggestions".to_owned()),
    );
    Some(rewritten.to_string())
}

async fn websocket_writer(
    mut writer: WsWriter,
    mut subscriber: BridgeSubscriber,
    mut actions: mpsc::Receiver<WsAction>,
    mut close_rx: watch::Receiver<Option<CloseFrame>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    loop {
        if *shutdown.borrow() {
            let _ = write_ws_close(&mut writer, 1001, "server-stopped").await;
            break;
        }
        let pending_close = close_rx.borrow().clone();
        if let Some(frame) = pending_close {
            let _ = write_ws_close(&mut writer, frame.code, &frame.reason).await;
            break;
        }

        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = write_ws_close(&mut writer, 1001, "server-stopped").await;
                    break;
                }
            }
            changed = close_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let frame = close_rx.borrow().clone();
                if let Some(frame) = frame {
                    let _ = write_ws_close(&mut writer, frame.code, &frame.reason).await;
                    break;
                }
            }
            line = subscriber.recv() => {
                match line {
                    Some(line) => {
                        write_ws_message(&mut writer, Message::Text(line))
                            .await
                            .map_err(|error| format!("failed to write bridge line: {error}"))?;
                    }
                    None => {
                        let (code, reason) = bridge_close_frame(subscriber.close_reason());
                        let _ = write_ws_close(&mut writer, code, reason).await;
                        break;
                    }
                }
            }
            action = actions.recv() => {
                match action {
                    Some(WsAction::Text(text)) => {
                        write_ws_message(&mut writer, Message::Text(text))
                            .await
                            .map_err(|error| format!("failed to echo text frame: {error}"))?;
                    }
                    Some(WsAction::Binary(payload)) => {
                        write_ws_message(&mut writer, Message::Binary(payload))
                            .await
                            .map_err(|error| format!("failed to echo binary frame: {error}"))?;
                    }
                    Some(WsAction::PeerClosed) => {
                        // tungstenite queued the close echo while the reader
                        // saw the peer's close frame; flushing is the whole
                        // reply (RFC 6455 5.5.1: exactly one close per side).
                        let _ = tokio::time::timeout(WRITE_TIMEOUT, writer.flush()).await;
                        break;
                    }
                    Some(WsAction::Close(code, reason)) => {
                        let _ = write_ws_close(&mut writer, code, &reason).await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    // No explicit TCP shutdown: the socket is owned by the split halves and
    // is closed (FIN) when both are dropped, which `websocket_session` drives
    // after the reader exits. Every close frame written above was flushed by
    // `SinkExt::send`; tungstenite additionally flushes a queued pong/close
    // reply on the next read or flush, so nothing is left buffered here.
    Ok(())
}

fn empty_state() -> JsonValue {
    json!({
        "sessionId": JsonValue::Null,
        "sessionName": JsonValue::Null,
        "model": JsonValue::Null,
        "thinkingLevel": "off",
        "isStreaming": false,
        "isCompacting": false,
        "usageTotals": {
            "input": 0, "output": 0, "reasoning": 0,
            "cacheRead": 0, "cacheWrite": 0,
        },
        "contextUsage": { "tokens": 0, "contextWindow": 0, "percent": 0.0 },
    })
}

/// RFC 6455 4.2.1: `Sec-WebSocket-Key` is the base64 encoding of exactly 16
/// random bytes. The decoded value is only used for validation (the accept
/// hash is computed over the wire text), so the 16-byte decode is the whole
/// cost.
fn is_valid_websocket_key(key: &str) -> bool {
    STANDARD
        .decode(key)
        .map(|decoded| decoded.len() == 16)
        .unwrap_or(false)
}

/// Re-encode the already-parsed request as the opening handshake the crate
/// expects to read.
///
/// The head was consumed (and validated) by this module's HTTP layer before
/// the upgrade, so the handshake is replayed from the parsed fields instead of
/// being re-read from the socket. Every header is forwarded verbatim —
/// `Sec-WebSocket-Key` for `Sec-WebSocket-Accept`, plus `Origin`/`Host`, which
/// the crate's parser never inspects but which keep the replayed request
/// faithful — except `Upgrade`/`Connection`: those are rewritten to the bare
/// tokens because the crate compares them literally, while this server (and
/// `tests/ws_rfc6455.rs::handshake_upgrade_and_connection_are_token_lists`)
/// accepts the RFC 7230 token-list forms (`Upgrade: h2c, websocket`,
/// `Connection: keep-alive, Upgrade`). The request was accepted by
/// [`is_websocket_upgrade`] before this runs, so the rewrite states the same
/// fact in the one spelling the crate understands.
///
/// The request line always says `HTTP/1.1`: the crate rejects anything below
/// 1.1 (`WrongHttpVersion`), and an upgrade is 1.1 by definition (RFC 6455
/// 4.2.1).
fn upgrade_request_head(request: &HttpRequest) -> Vec<u8> {
    let mut head = format!(
        "GET {} HTTP/1.1\r\n",
        if request.query.is_empty() {
            request.path.clone()
        } else {
            format!("{}?{}", request.path, request.query)
        }
    );
    head.push_str("upgrade: websocket\r\nconnection: Upgrade\r\n");
    // `HashMap` iteration order is unspecified; the crate's parser is
    // order-agnostic, and sorting keeps the replayed bytes deterministic.
    let mut headers: Vec<(&String, &String)> = request
        .headers
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "upgrade" | "connection"))
        .collect();
    headers.sort_by(|left, right| left.0.cmp(right.0));
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// The upgraded `TcpStream` as the crate sees it: the replayed handshake head
/// first, then the socket.
///
/// The head is handed back because `accept_async_with_config` reads the
/// opening handshake from the stream and requires the upgrade request to be
/// the first thing it sees. `leftover` (bytes the HTTP layer read past the
/// head) follows it, so a client that pipelines its first frame behind the
/// upgrade does not lose it; both cursors drain as they are consumed, after
/// which reads go straight to the socket. Each socket read is recorded in the
/// byte counter that preserves the idle-vs-stalled-frame deadline split (see
/// [`read_ws_message`]).
///
/// The two prefixes stay separate on purpose: the crate fails the handshake
/// with `JunkAfterRequest` if anything follows the head in the buffer it
/// parsed, so the pipelined bytes must not be handed over in the same read as
/// the last head byte.
struct UpgradedStream {
    head: std::io::Cursor<Vec<u8>>,
    leftover: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
    progress: Arc<ReadProgress>,
}

impl UpgradedStream {
    fn new(
        head: Vec<u8>,
        leftover: Vec<u8>,
        inner: TcpStream,
        progress: Arc<ReadProgress>,
    ) -> Self {
        Self {
            head: std::io::Cursor::new(head),
            leftover: std::io::Cursor::new(leftover),
            inner,
            progress,
        }
    }
}

/// Copy what remains of `cursor` into `buf`; false when it is exhausted.
fn replay_into(cursor: &mut std::io::Cursor<Vec<u8>>, buf: &mut tokio::io::ReadBuf<'_>) -> bool {
    let start = cursor.position() as usize;
    let take = cursor
        .get_ref()
        .len()
        .saturating_sub(start)
        .min(buf.remaining());
    if take == 0 {
        return false;
    }
    cursor.set_position((start + take) as u64);
    if let Some(rest) = cursor.get_ref().get(start..start + take) {
        buf.put_slice(rest);
    }
    true
}

impl tokio::io::AsyncRead for UpgradedStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // The replayed bytes are local (no waker involved), so they are always
        // ready and never count as socket activity. A zero-length caller
        // buffer must not turn into a zero-byte "read", which the caller would
        // read as EOF.
        if buf.remaining() > 0
            && (replay_into(&mut this.head, buf) || replay_into(&mut this.leftover, buf))
        {
            return std::task::Poll::Ready(Ok(()));
        }
        let filled_before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            this.progress.record(buf.filled().len() - filled_before);
        }
        polled
    }
}

impl tokio::io::AsyncWrite for UpgradedStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, data)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The read half of one `/ws` connection (tungstenite over the upgraded
/// `TcpStream`).
///
/// The two halves are split with `futures_util::StreamExt::split`, so each
/// task owns a `BiLock` handle instead of a socket half: reads and writes can
/// proceed concurrently, and neither task can be cancelled by the other's
/// `select!`. tungstenite is built for exactly this split (`Stream` + `Sink`
/// over one codec that keeps partially-read frames), which is why the
/// hand-written reader/writer pair could be dropped.
type WsSource = SplitStream<WebSocketStream<UpgradedStream>>;
/// The write half of one `/ws` connection.
type WsWriter = SplitSink<WebSocketStream<UpgradedStream>, Message>;

/// The protocol configuration for a server-side connection.
///
/// Both caps keep today's numbers: a single frame and an assembled
/// (fragmented) message may each reach [`MAX_WS_FRAME_BYTES`] /
/// [`MAX_WS_MESSAGE_BYTES`] (8 MiB), and nothing larger is read or buffered.
/// tungstenite enforces them before allocating the payload and reports
/// `CapacityError::MessageTooLong`, which [`ws_read_failure`] turns into the
/// same 1009 close the hand-written reader sent.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_frame_size: Some(MAX_WS_FRAME_BYTES),
        max_message_size: Some(MAX_WS_MESSAGE_BYTES),
        // RFC 6455 5.1: a server must fail the connection on an unmasked
        // client frame. tungstenite's default; spelled out so nobody flips it
        // while debugging a non-conforming client.
        accept_unmasked_frames: false,
        ..WebSocketConfig::default()
    }
}

enum WsReadError {
    /// Normal end of the connection: the peer's close handshake completed or
    /// the socket ended. Nothing to send.
    Closed,
    /// No complete message arrived within the idle deadline.
    Idle,
    /// A message started but did not complete within
    /// [`WS_FRAME_READ_TIMEOUT`].
    Timeout,
    /// A protocol failure: send this close code/reason and stop.
    Protocol(u16, String),
}

/// Read-progress counter for one `/ws` socket.
///
/// The hand-written transport measured two deadlines directly on the socket:
/// [`WS_IDLE_TIMEOUT`] waiting for the first byte of a frame, then
/// [`WS_FRAME_READ_TIMEOUT`] for the rest of that frame. tungstenite hands
/// back whole messages, so the byte counter (fed by [`UpgradedStream`]) is
/// what keeps that distinction: a read that expires before any byte arrived
/// is an idle disconnect (close 1001 `idle-timeout`), one that expires
/// mid-message is a stalled frame (close 1002 `frame read timeout`).
#[derive(Default)]
struct ReadProgress {
    bytes: AtomicU64,
    activity: AtomicWaker,
}

impl ReadProgress {
    fn record(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.bytes.fetch_add(count as u64, Ordering::Relaxed);
        self.activity.wake();
    }

    fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Resolves once at least one byte has been read past `start`.
struct Activity<'a> {
    progress: &'a ReadProgress,
    start: u64,
}

impl std::future::Future for Activity<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.progress.bytes() != self.start {
            return std::task::Poll::Ready(());
        }
        // Register first, then re-check: a byte arriving between the check
        // and the registration must not be missed (that would park the read
        // until the idle deadline).
        self.progress.activity.register(cx.waker());
        if self.progress.bytes() != self.start {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

/// Per-client idle deadline. When idle auto-stop is configured, the server
/// timer must win: otherwise an idle-disconnect + automatic reconnect would
/// keep touching the server and defeat `--idle-mins`.
fn ws_idle_timeout(meta: &ServerMeta) -> Duration {
    if meta.status.idle_timeout_ms == 0 {
        WS_IDLE_TIMEOUT
    } else {
        Duration::from_millis(meta.status.idle_timeout_ms) + Duration::from_secs(30)
    }
}

/// Read one complete message (fragments assembled, control frames handled by
/// tungstenite) with the two original deadlines.
///
/// Ping/pong stay inside the crate: a ping is answered by the read side, and
/// the pong is flushed on the next read or write, so the reader simply keeps
/// going. A peer close comes back as `Message::Close` after the reply frame
/// has been queued, and the caller only has to flush it.
async fn read_ws_message(
    reader: &mut WsSource,
    progress: &ReadProgress,
    idle_timeout: Duration,
) -> Result<Message, WsReadError> {
    let started = progress.bytes();
    let first = tokio::select! {
        message = reader.next() => Some(message),
        _ = Activity {
            progress,
            start: started,
        } => None,
        _ = tokio::time::sleep(idle_timeout) => return Err(WsReadError::Idle),
    };

    // Either the message is complete, or its first byte arrived; from here
    // the rest of the message must arrive within the frame deadline (the
    // hand-written reader bounded the rest of each frame the same way).
    let message = match first {
        Some(message) => message,
        None => match tokio::time::timeout(WS_FRAME_READ_TIMEOUT, reader.next()).await {
            Ok(message) => message,
            Err(_) => return Err(WsReadError::Timeout),
        },
    };

    match message {
        // A closed/fused stream (`None`) or a connection that ended between
        // messages is a normal end.
        None => Err(WsReadError::Closed),
        Some(Ok(message)) => Ok(message),
        Some(Err(error)) => Err(ws_read_failure(error)),
    }
}

/// Map a tungstenite read error onto the close this server sends.
///
/// The protocol errors already carry RFC 6455's close codes implicitly; these
/// are the same mappings the hand-written reader made explicit:
/// - oversized frame/message -> 1009 `message too big`;
/// - every other protocol violation (reserved bits, unmasked frame,
///   fragmented/oversized control frame, bad opcode, continuation without a
///   start, interleaved fragment, invalid close payload/code) -> 1002;
/// - invalid UTF-8 in text or in a close reason -> 1007 `invalid utf-8`.
///
/// A plain I/O failure is the peer going away; tungstenite reports a clean
/// close handshake as `ConnectionClosed`/`AlreadyClosed`, which also sends
/// nothing back.
fn ws_read_failure(error: WsError) -> WsReadError {
    match error {
        WsError::Capacity(_) => WsReadError::Protocol(1009, "message too big".to_owned()),
        WsError::Utf8 => WsReadError::Protocol(1007, "invalid utf-8".to_owned()),
        WsError::Protocol(_) => WsReadError::Protocol(1002, "protocol error".to_owned()),
        WsError::ConnectionClosed | WsError::AlreadyClosed | WsError::Io(_) => WsReadError::Closed,
        _ => WsReadError::Closed,
    }
}

/// Send one message, bounded by [`WRITE_TIMEOUT`] so a peer that stops
/// reading cannot pin the writer task forever.
///
/// The error is boxed: `tungstenite::Error` is large (136 bytes) and this
/// function's `Ok` value is a unit, so an unboxed `Err` would inflate every
/// call frame for an error path that terminates the loop anyway.
async fn write_ws_message(writer: &mut WsWriter, message: Message) -> Result<(), Box<WsError>> {
    match tokio::time::timeout(WRITE_TIMEOUT, writer.send(message)).await {
        Ok(result) => result.map_err(Box::new),
        Err(_) => Err(Box::new(WsError::Io(std::io::Error::new(
            ErrorKind::TimedOut,
            "websocket write timed out",
        )))),
    }
}

/// Start the close handshake with `code`/`reason` and flush it.
///
/// 4009 and 4010 are `CloseCode::Library`, which `is_allowed()` accepts, so
/// tungstenite sends them as-is; the reason is truncated at tungstenite's
/// 123-byte control-frame limit by `CloseFrame` construction here (the crate
/// does not truncate a too-long reason itself).
async fn close_ws(ws: &mut WebSocketStream<UpgradedStream>, code: u16, reason: &str) {
    let reason = truncate_close_reason(reason).to_owned();
    let _ = tokio::time::timeout(
        WRITE_TIMEOUT,
        ws.close(Some(WsCloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        })),
    )
    .await;
}

/// The close handshake from a split writer half (the writer loop's path).
async fn write_ws_close(writer: &mut WsWriter, code: u16, reason: &str) {
    let reason = truncate_close_reason(reason).to_owned();
    let _ = tokio::time::timeout(
        WRITE_TIMEOUT,
        writer.send(Message::Close(Some(WsCloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        }))),
    )
    .await;
}

/// Control-frame payloads carry at most 125 bytes: two for the code, 123 for
/// the reason (RFC 6455 5.5).
fn truncate_close_reason(reason: &str) -> &str {
    if reason.len() <= 123 {
        return reason;
    }
    let mut end = 123;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

// ---------------------------------------------------------------------------
// Command catalog
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CommandInfo {
    name: &'static str,
    description: &'static str,
    source: &'static str,
}

fn command_catalog() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "phone-start",
            description: "Start the phone web UI. Usage: /phone-start [port] [token] [--cwd path] [--host 127.0.0.1] [--idle-mins 20]",
            source: "extension",
        },
        CommandInfo {
            name: "phone-stop",
            description: "Stop the phone web UI server and remove the matching Tailscale Serve route",
            source: "extension",
        },
        CommandInfo {
            name: "phone-status",
            description: "Show phone server and Tailscale Serve status",
            source: "extension",
        },
        CommandInfo {
            name: "phone-token",
            description: "Show the current phone UI token",
            source: "extension",
        },
    ]
}

impl serde::Serialize for CommandInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("name", self.name)?;
        map.serialize_entry("description", self.description)?;
        map.serialize_entry("source", self.source)?;
        map.end()
    }
}

// ---------------------------------------------------------------------------
// Small helpers used by tests
// ---------------------------------------------------------------------------

/// The four command names/descriptions (also embedded in the browser client
/// command catalog).
pub fn command_descriptions() -> [(&'static str, &'static str); 4] {
    let catalog = command_catalog();
    [
        (catalog[0].name, catalog[0].description),
        (catalog[1].name, catalog[1].description),
        (catalog[2].name, catalog[2].description),
        (catalog[3].name, catalog[3].description),
    ]
}
