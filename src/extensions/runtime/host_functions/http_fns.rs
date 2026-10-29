//! Host HTTP backend for the Web globals (global `fetch`): a third generic
//! native entry `__pir_host.http(op, payloadJson) -> String`, registered
//! alongside `node`/`nodeExt` by [`super::register_host_functions`]. The JS
//! side (`js/fetch_global.js.txt`) speaks the same `{ok,value}` /
//! `{ok:false,code,message}` JSON envelope as the node surfaces, so host
//! failures raise the familiar coded `Error` there.
//!
//! Shape (the [`super::child_fns`] pattern, reused not reinvented): a fetch
//! call registers a handle in a static registry and returns immediately with
//! an id; a detached worker thread drives the async `reqwest` client on the
//! process-wide tokio runtime (the crate is already a dependency with the
//! `stream` feature, so no new crates were added). That runtime is shared and
//! long-lived on purpose: it owns the connection-pool tasks of the shared
//! clients, so keep-alive connections survive between requests. The response
//! headers land in the handle once received and the body streams into a
//! bounded buffer that the JS poll loop drains chunk by chunk (`fetchPoll`),
//! exactly like the child_process pipe readers. Aborts (AbortSignal / timeout)
//! flip a `CancellationToken` the worker selects on, which tears down the
//! in-flight request and marks the stream aborted.
//!
//! NOTE (permissions): intentionally no new permission gate — outbound HTTP
//! is the same capability class as child_process (an extension can already
//! reach the network through a spawned process). A future destination
//! allowlist would be installed here.
//!
//! The same entry also carries the `node:net` loopback TCP ops (`netListen`,
//! `netConnect`, `netAccept`, `netRead`, `netWrite`, `netEnd`, `netClose`,
//! `netSetNoDelay`, `netServerClose`) — one dispatch table, one envelope, one
//! owner. See the `net` section at the bottom of this file.
//!
//! Queue bounds (all measured, none of them a silent drop):
//! - the fetch response body window ([`MAX_STREAM_BYTES`]) parks the body
//!   reader when full — the transport stops being polled, i.e. Node's
//!   `ReadableStream` backpressure — and is released by `fetchPoll`'s drain;
//!   a body poll that finds nothing to report waits up to its `waitMs` slice
//!   ([`MAX_POLL_WAIT_MS`]), which is what paces the JS body pump (a
//!   zero-wait poll would spin it without ever yielding to the driver);
//! - the `http.Server` request queue ([`MAX_PENDING_REQUESTS`]) parks the
//!   connection thread before a request becomes visible to `serverPoll` (the
//!   socket stops being read); a wait that outlives the connection deadline
//!   closes that connection instead of parking a thread forever;
//! - a request's streaming body window ([`MAX_SERVER_BODY_BUFFER`]) parks the
//!   connection thread the same way while `serverBody` drains it.
//!
//! In every case the producer is the one that waits (the source is a real
//! socket or an async body reader, so pausing it is end-to-end backpressure);
//! the only byte-dropping path left is the defensive single-chunk-larger-than-
//! the-window case, which reports `truncated`.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use futures_util::StreamExt;
use rquickjs::prelude::Func;
use rquickjs::Object;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Registration + dispatch
// ---------------------------------------------------------------------------

/// Install `__pir_host.http(op, payloadJson)` on the shared `__pir_host`
/// object. No state is captured: the fetch surface is sandbox-independent
/// (see the permission note in the module docs).
pub(super) fn register_http_host_fns(
    host: &Object,
    // The owning runtime's generation: fetches/servers/sockets are tagged so
    // a reload or shutdown can cancel exactly its own resources.
    generation: u64,
) -> rquickjs::Result<()> {
    host.set(
        "http",
        Func::from(move |op: String, payload: String| -> String {
            http_call(&op, &payload, generation)
        }),
    )?;
    Ok(())
}

fn http_call(op: &str, payload: &str, generation: u64) -> String {
    let started = std::time::Instant::now();
    let payload: JsonValue = serde_json::from_str(payload).unwrap_or(JsonValue::Null);
    let result = match op {
        "fetchStart" => fetch_start(&payload, generation),
        "fetchPoll" => fetch_poll(&payload),
        "fetchAbort" => fetch_abort(&payload),
        "fetchEnd" => fetch_end(&payload),
        "serverListen" => server_listen(&payload, generation),
        "serverPoll" => server_poll(&payload),
        "serverRespond" => server_respond(&payload),
        "serverBody" => server_body(&payload),
        "serverWaitIdle" => server_wait_idle(&payload),
        "serverClose" => server_close(&payload),
        // node:net loopback TCP backend (see the `net` section below). Same
        // entry, same envelope: the JS net shim polls these ops from its own
        // timer-driven pump exactly like `node:http` polls `serverPoll`.
        "netListen" => net_listen(&payload, generation),
        "netServerClose" => net_server_close(&payload),
        "netAccept" => net_accept(&payload),
        "netConnect" => net_connect(&payload, generation),
        "netRead" => net_read(&payload),
        "netWrite" => net_write(&payload),
        "netEnd" => net_end(&payload),
        "netClose" => net_close(&payload),
        "netSetNoDelay" => net_set_no_delay(&payload),
        other => Err(FnError::new("ENOSYS", format!("unknown http op `{other}`"))),
    };
    let output = match result {
        Ok(value) => serde_json::json!({ "ok": true, "value": value }).to_string(),
        Err(error) => error.to_json().to_string(),
    };
    // Static label: the operation family, never a URL/payload.
    let label = if op.starts_with("fetch") {
        "fetch"
    } else if op.starts_with("server") {
        "http.server"
    } else if op.starts_with("net") {
        "net"
    } else {
        "http.other"
    };
    super::super::host_op_stats::record(label, started.elapsed());
    output
}

// ---------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------

/// One failure in the shared envelope. `code` uses Node/Web-flavored names:
/// `ERR_INVALID_URL`, `ENOSYS`, `EINVAL` for call-shape problems and the
/// transport labels (`ECONNREFUSED`, `EIO`, `ABORT_ERR`, ...) for network
/// problems; the JS layer turns `ABORT_ERR` into an `AbortError`.
struct FnError {
    code: &'static str,
    message: String,
}

impl FnError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn to_json(&self) -> JsonValue {
        serde_json::json!({
            "ok": false,
            "code": self.code,
            "message": self.message,
        })
    }
}

type OpResult = Result<JsonValue, FnError>;

fn payload_id(payload: &JsonValue) -> Result<u64, FnError> {
    payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing fetch id"))
}

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Fetch registry
// ---------------------------------------------------------------------------

/// Maximum bytes buffered for one response body before the body reader pauses.
///
/// The JS poll loop drains continuously while the consumer reads, so the
/// buffer only fills when the consumer stops asking; at that point the reader
/// parks and the transport stops being polled, which is exactly Node's
/// `ReadableStream` backpressure (`fetch` resolves headers but the body stops
/// flowing — the TCP window closes — until the consumer reads). Nothing is
/// dropped: the cap is `highWaterMark`-shaped (Node's stream default is
/// 64 KiB; 8 MiB gives the poll protocol a full window of a fast producer).
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// Hard cap on concurrent host fetches. Each fetch owns a worker thread, a
/// socket, and a bounded body window; EMFILE is the Node-shaped resource error.
const MAX_FETCHES: usize = 64;

/// Upper bound of one `fetchPoll` wait. The JS loop asks for a smaller slice
/// so the runtime thread is never parked for long, and this wait is what paces
/// a waiting body consumer: once the headers were reported, an empty poll
/// keeps waiting (1 ms steps) for body data / `done` / `aborted` / an error
/// instead of returning instantly — a zero-wait return would spin the JS pump
/// with no yield to the runtime's driver (and so freeze every tokio timer,
/// leaving only the engine's interrupt deadline to bound the call).
const MAX_POLL_WAIT_MS: u64 = 200;

/// How long a body reader parks on a full window before re-checking the
/// cancellation token (a parked reader must not outlive an aborted fetch
/// whose consumer stopped polling).
const BODY_PARK_POLL: Duration = Duration::from_millis(50);

/// Longest single condvar park slice of the fetch/server poll loops. The
/// producer notifies the matching condvar on every state change, so this is
/// only the lost-wakeup safety net (the former 1-5 ms sleeps were pure
/// polling).
const RESPONSE_WAIT_SLICE: Duration = Duration::from_millis(100);

/// Status + headers, recorded once by the worker before the body streams.
struct ResponseMeta {
    url: String,
    status: u16,
    headers: Vec<(String, String)>,
}

/// Bounded body window: the worker pushes, `fetchPoll` drains. A full window
/// parks the worker (see [`MAX_STREAM_BYTES`] / [`MAX_SERVER_BODY_BUFFER`]);
/// `dropped` remains for the defensive single-chunk-over-cap case and is
/// reported as `truncated`.
struct BodyBuf {
    data: VecDeque<u8>,
    dropped: bool,
    eof: bool,
    /// How many chunks had to wait for a drain (observation: the reader is
    /// applying backpressure).
    pauses: u64,
    /// Byte cap of this window.
    cap: usize,
}

impl BodyBuf {
    fn new(cap: usize) -> Self {
        Self {
            data: VecDeque::new(),
            dropped: false,
            eof: false,
            pauses: 0,
            cap,
        }
    }

    /// Append one chunk. Returns `false` when the window is full — the caller
    /// (a body reader) must wait for a drain, never drop.
    fn push(&mut self, chunk: &[u8]) -> bool {
        if chunk.len() >= self.cap {
            // Defensive: one chunk larger than the whole window (both readers
            // hand over small frames) keeps its newest bytes rather than
            // parking forever on a chunk that can never fit.
            self.data.clear();
            self.data.extend(&chunk[chunk.len() - self.cap..]);
            self.dropped = true;
            return true;
        }
        if self.data.len() + chunk.len() > self.cap {
            self.pauses = self.pauses.saturating_add(1);
            return false;
        }
        self.data.extend(chunk);
        true
    }

    fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    fn take(&mut self) -> (Vec<u8>, bool) {
        let dropped = self.dropped;
        self.dropped = false;
        (self.data.drain(..).collect(), dropped)
    }
}

/// One fetch's activity signal: a monotone counter + condvar. The worker
/// bumps it for every reportable state change (headers, body chunk, error,
/// done, abort) and `fetchPoll` parks on it instead of sleeping 1 ms steps.
struct FetchActivity {
    state: StdMutex<(u64, u64)>,
    signal: Condvar,
}

impl FetchActivity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new((0, 0)),
            signal: Condvar::new(),
        })
    }

    fn notify(&self) {
        let mut state = lock(&self.state);
        state.0 = state.0.wrapping_add(1);
        drop(state);
        self.signal.notify_all();
    }

    /// Current activity generation (see [`Self::wait_since`]).
    fn epoch(&self) -> u64 {
        lock(&self.state).0
    }

    /// Park until a state change newer than `seen`, or `timeout`; returns
    /// whether a notification arrived (an already-newer generation returns
    /// immediately, so a change landing between the caller's check and its
    /// park is never lost). The park count (second word) is the low-resource
    /// seam.
    fn wait_since(&self, seen: u64, timeout: Duration) -> bool {
        let mut state = lock(&self.state);
        state.1 = state.1.wrapping_add(1);
        let (state, outcome) = self
            .signal
            .wait_timeout_while(state, timeout, |state| state.0 == seen)
            .unwrap_or_else(PoisonError::into_inner);
        drop(state);
        !outcome.timed_out()
    }
}

/// One in-flight (or completed) fetch. Removing the entry drops the handle;
/// the worker holds its own `Arc`, so cancellation still reaches it.
struct FetchHandle {
    /// Owning runtime generation (see `release_generation`).
    generation: u64,
    /// Wakes `fetchPoll` on every reportable state change.
    activity: Arc<FetchActivity>,
    response: StdMutex<Option<ResponseMeta>>,
    body: StdMutex<BodyBuf>,
    /// Wakes a body reader parked on a full [`BodyBuf`] once `fetchPoll`
    /// drained it.
    body_space: tokio::sync::Notify,
    error: StdMutex<Option<(&'static str, String)>>,
    done: StdMutex<bool>,
    aborted: StdMutex<bool>,
    cancel: CancellationToken,
    /// Whether a `fetchPoll` has already reported the response headers.
    ///
    /// This is the poll's wait rule: the headers are the reportable event
    /// exactly ONCE (the JS header loop parks on them), so before this is set a
    /// poll returns the moment the meta exists; afterwards an empty poll must
    /// WAIT up to `waitMs` for body progress (data / `done` / `aborted` /
    /// error) instead of returning instantly. Without the flag the break
    /// condition `response.is_some()` is permanently true, every body poll is a
    /// zero-wait return, and the JS body pump degrades into a spin that never
    /// yields to the runtime's driver (which freezes every tokio timer — the
    /// call's own deadline could then only surface as an engine interrupt).
    headers_reported: AtomicBool,
}

impl FetchHandle {
    fn set_error(&self, code: &'static str, message: String) {
        let mut guard = lock(&self.error);
        if guard.is_none() {
            *guard = Some((code, message));
        }
        *lock(&self.done) = true;
        lock(&self.body).eof = true;
        drop(guard);
        self.activity.notify();
    }
}

static FETCHES: OnceLock<StdMutex<HashMap<u64, Arc<FetchHandle>>>> = OnceLock::new();
static NEXT_FETCH_ID: AtomicU64 = AtomicU64::new(1);

fn fetches() -> &'static StdMutex<HashMap<u64, Arc<FetchHandle>>> {
    FETCHES.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn handle_by_id(id: u64) -> Result<Arc<FetchHandle>, FnError> {
    fetches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no fetch with id {id}")))
}

/// Shared `reqwest` client: no overall timeout (cancellation owns the
/// lifetime), default redirect policy (up to 10, matching `fetch`).
fn shared_client() -> Result<&'static reqwest::Client, FnError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("pir-extensions/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(client) => Ok(client),
        Err(message) => Err(FnError::new(
            "EIO",
            format!("http client build failed: {message}"),
        )),
    }
}

/// Second shared client with redirect following disabled: `node:http`
/// request()/get() return 3xx responses instead of following them (Node's
/// own default), unlike the Web `fetch` surface which follows.
fn shared_client_no_redirect() -> Result<&'static reqwest::Client, FnError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("pir-extensions/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(client) => Ok(client),
        Err(message) => Err(FnError::new(
            "EIO",
            format!("http client build failed: {message}"),
        )),
    }
}

/// Node-flavored transport label for a reqwest failure: walk the error source
/// chain for the underlying `std::io::Error` (ECONNREFUSED / ECONNRESET /
/// ETIMEDOUT / ...) and fall back to sniffing DNS/connect text that hyper
/// wraps without an io source. The JS client surfaces this as `error.code`.
fn transport_code(error: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(source) = current {
        if let Some(io) = source.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
                std::io::ErrorKind::ConnectionReset => "ECONNRESET",
                std::io::ErrorKind::ConnectionAborted => "ECONNABORTED",
                std::io::ErrorKind::BrokenPipe => "EPIPE",
                std::io::ErrorKind::TimedOut => "ETIMEDOUT",
                std::io::ErrorKind::PermissionDenied => "EACCES",
                std::io::ErrorKind::NotFound => "ENOTFOUND",
                _ => "EIO",
            };
        }
        let text = source.to_string();
        if text.contains("dns error")
            || text.contains("failed to lookup address")
            || text.contains("Name or service not known")
            || text.contains("no such host")
            || text.contains("nodename nor servname")
        {
            return "ENOTFOUND";
        }
        if text.contains("Connection refused") || text.contains("connection refused") {
            return "ECONNREFUSED";
        }
        if text.contains("Connection reset") || text.contains("connection reset") {
            return "ECONNRESET";
        }
        current = source.source();
    }
    "EIO"
}

// ---------------------------------------------------------------------------
// fetchStart: register + spawn the worker
// ---------------------------------------------------------------------------

struct RequestSpec {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
    /// `node:http` client requests return redirects instead of following.
    no_redirect: bool,
}

fn fetch_start(payload: &JsonValue, generation: u64) -> OpResult {
    let url = payload
        .get("url")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FnError::new("EINVAL", "missing string payload field `url`"))?
        .to_owned();
    // Belt-and-suspenders URL check (the JS layer validates too): only
    // absolute http/https reach the host client.
    let lower = url.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) || url.contains(' ') {
        return Err(FnError::new(
            "ERR_INVALID_URL",
            format!("Failed to parse URL: invalid scheme or characters in '{url}'"),
        ));
    }
    let method = payload
        .get("method")
        .and_then(JsonValue::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    // Bound live host fetches: each one owns a worker thread, a socket and an
    // 8 MiB body window. The JS side treats EMFILE like Node's resource error.
    if fetches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
        >= MAX_FETCHES
    {
        return Err(FnError::new(
            "EMFILE",
            format!("too many concurrent fetches (max {MAX_FETCHES})"),
        ));
    }
    let headers = payload_header_pairs(payload);
    // Body channel, preferred order: `bodyText` (the UTF-8 image of the bytes,
    // sent by the JS side for pure-string bodies — no base64 round trip on
    // either leg) then `bodyB64` (byte bodies: Buffer/typed arrays, or any
    // string chunk that is not plain UTF-8 text). A non-string `bodyText` is
    // ignored rather than rejected so the byte channel stays the ABI default.
    let body = if let Some(text) = payload.get("bodyText").and_then(JsonValue::as_str) {
        Some(text.as_bytes().to_vec())
    } else {
        payload
            .get("bodyB64")
            .and_then(JsonValue::as_str)
            .map(|data| {
                STANDARD.decode(data.as_bytes()).map_err(|error| {
                    FnError::new("EINVAL", format!("invalid base64 body: {error}"))
                })
            })
            .transpose()?
    };
    let timeout_ms = payload.get("timeoutMs").and_then(JsonValue::as_u64);
    let no_redirect = payload
        .get("noRedirect")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);

    let handle = Arc::new(FetchHandle {
        generation,
        activity: FetchActivity::new(),
        response: StdMutex::new(None),
        body: StdMutex::new(BodyBuf::new(MAX_STREAM_BYTES)),
        body_space: tokio::sync::Notify::new(),
        error: StdMutex::new(None),
        done: StdMutex::new(false),
        aborted: StdMutex::new(false),
        cancel: CancellationToken::new(),
        headers_reported: AtomicBool::new(false),
    });
    let id = NEXT_FETCH_ID.fetch_add(1, Ordering::SeqCst);
    fetches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&handle));
    spawn_worker(
        RequestSpec {
            url,
            method,
            headers,
            body,
            timeout_ms,
            no_redirect,
        },
        handle,
    );
    Ok(serde_json::json!({ "id": id }))
}

/// Process-wide runtime every fetch worker drives its future on. It must
/// outlive the shared `reqwest` clients: a client's connection pool (and the
/// tasks hyper spawns for its sockets on the runtime that built it) keeps
/// keep-alive connections reusable only while that runtime lives, so the old
/// per-request runtime tore them down the moment a response finished — a
/// concurrent request could then be handed a *dying* pooled connection.
///
/// Multi-thread rather than current-thread because each request calls
/// `block_on` from its own worker thread: `Runtime::block_on` may be called
/// concurrently from any number of threads on a multi-thread runtime (the
/// scheduler, I/O and timer drivers are shared; every future is still driven
/// by the worker thread that called `block_on`).
fn fetch_runtime() -> Result<&'static tokio::runtime::Runtime, FnError> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    match RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(FnError::new(
            "EIO",
            format!("http worker runtime failed: {message}"),
        )),
    }
}

/// Detached worker thread: each fetch keeps its own thread (and so never
/// touches the host's own tokio runtime), but the future runs on the shared
/// [`fetch_runtime`] so the pooled connections it leaves behind stay alive for
/// later requests.
fn spawn_worker(spec: RequestSpec, handle: Arc<FetchHandle>) {
    let runtime = match fetch_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            handle.set_error(error.code, error.message);
            return;
        }
    };
    std::thread::spawn(move || {
        runtime.block_on(drive_fetch(spec, handle));
    });
}

async fn drive_fetch(spec: RequestSpec, handle: Arc<FetchHandle>) {
    let method = match reqwest::Method::from_bytes(spec.method.as_bytes()) {
        Ok(method) => method,
        Err(_) => {
            handle.set_error("EINVAL", format!("invalid HTTP method `{}`", spec.method));
            return;
        }
    };
    let client = if spec.no_redirect {
        match shared_client_no_redirect() {
            Ok(client) => client,
            Err(error) => {
                handle.set_error(error.code, error.message);
                return;
            }
        }
    } else {
        match shared_client() {
            Ok(client) => client,
            Err(error) => {
                handle.set_error(error.code, error.message);
                return;
            }
        }
    };
    let mut request = client.request(method, &spec.url);
    for (name, value) in &spec.headers {
        request = request.header(name, value);
    }
    if let Some(body) = spec.body {
        request = request.body(body);
    }
    let send = request.send();

    // Headers first: the poll surface reports ready as soon as they arrive,
    // then the body streams below.
    let response = if let Some(timeout_ms) = spec.timeout_ms {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            tokio::select! {
                _ = handle.cancel.cancelled() => None,
                response = send => Some(response),
            }
        })
        .await
        {
            Ok(inner) => inner,
            Err(_) => {
                handle.set_error(
                    "ABORT_ERR",
                    "The operation was aborted due to timeout".to_owned(),
                );
                return;
            }
        }
    } else {
        tokio::select! {
            _ = handle.cancel.cancelled() => None,
            response = send => Some(response),
        }
    };

    let response = match response {
        Some(Ok(response)) => response,
        Some(Err(error)) => {
            let code = transport_code(&error);
            handle.set_error(code, format!("fetch failed: {error}"));
            return;
        }
        None => {
            // Cancelled while connecting: the JS side already saw `aborted`.
            *lock(&handle.done) = true;
            lock(&handle.body).eof = true;
            handle.activity.notify();
            return;
        }
    };

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or("").to_owned(),
            )
        })
        .collect::<Vec<(String, String)>>();
    let url = response.url().to_string();
    *lock(&handle.response) = Some(ResponseMeta {
        url,
        status,
        headers,
    });
    // The headers are the first reportable event; a parked `fetchPoll` must
    // wake for them.
    handle.activity.notify();

    let mut stream = response.bytes_stream();
    loop {
        tokio::select! {
            _ = handle.cancel.cancelled() => {
                *lock(&handle.aborted) = true;
                *lock(&handle.done) = true;
                lock(&handle.body).eof = true;
                handle.activity.notify();
                return;
            }
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    // Backpressure: a full window parks this reader until
                    // `fetchPoll` drains (the unseen body stays in the
                    // transport/socket, i.e. TCP window backpressure, Node's
                    // `ReadableStream` shape). Nothing is dropped here.
                    if !push_body(&handle, &bytes).await {
                        // Cancelled while parked: the JS side already saw
                        // `aborted`.
                        return;
                    }
                }
                Some(Err(error)) => {
                    let code = transport_code(&error);
                    handle.set_error(code, format!("fetch body read failed: {error}"));
                    return;
                }
                None => {
                    *lock(&handle.done) = true;
                    lock(&handle.body).eof = true;
                    handle.activity.notify();
                    return;
                }
            },
        }
    }
}

/// Push one body chunk, parking while the window is full. Returns `false` only
/// when the fetch was cancelled (or aborted) while parked.
///
/// The waiter is created before the retry check, so a drain landing in between
/// leaves a stored permit that the await below consumes immediately (no lost
/// wakeup); the park is also time-bounded so the cancellation token stays
/// responsive even if a notify is missed.
async fn push_body(handle: &Arc<FetchHandle>, chunk: &[u8]) -> bool {
    loop {
        if handle.cancel.is_cancelled() {
            *lock(&handle.aborted) = true;
            *lock(&handle.done) = true;
            lock(&handle.body).eof = true;
            handle.activity.notify();
            return false;
        }
        let notified = handle.body_space.notified();
        if lock(&handle.body).push(chunk) {
            handle.activity.notify();
            return true;
        }
        let _ = tokio::time::timeout(BODY_PARK_POLL, notified).await;
    }
}

// ---------------------------------------------------------------------------
// fetchPoll: bounded wait, drain one window of body bytes
// ---------------------------------------------------------------------------

fn fetch_poll(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let handle = handle_by_id(id)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(MAX_POLL_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    // Bounded wait for something to report: body data, an error, an abort, the
    // end of the stream — and the response headers, which are reportable
    // exactly once (the JS header loop parks on them). After that first report
    // an EMPTY poll keeps waiting instead of returning instantly: the JS body
    // pump is paced by this wait, so a permanent "headers exist" break would
    // turn it into a zero-wait spin that never yields to the runtime's driver.
    // The worker notifies `activity` for every reportable state change, so the
    // poll parks on that condvar; the bounded slice is only the lost-wakeup
    // safety net (the former 1 ms sleep was 1000 wakeups/s per fetch).
    loop {
        // Snapshot the activity generation BEFORE the state check: a change
        // landing between the check and the park makes `wait_since` return
        // immediately instead of losing the wakeup.
        let seen = handle.activity.epoch();
        let headers_are_news =
            !handle.headers_reported.load(Ordering::SeqCst) && lock(&handle.response).is_some();
        if headers_are_news
            || lock(&handle.body).has_data()
            || *lock(&handle.done)
            || *lock(&handle.aborted)
            || lock(&handle.error).is_some()
        {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        handle
            .activity
            .wait_since(seen, (deadline - now).min(RESPONSE_WAIT_SLICE));
    }
    let response = lock(&handle.response);
    let error = lock(&handle.error).clone();
    let done = *lock(&handle.done);
    let aborted = *lock(&handle.aborted);
    let (body, dropped) = lock(&handle.body).take();
    // Release a body reader parked on the full window (the drain above made
    // room; the reader re-checks the cancellation token as well).
    handle.body_space.notify_one();
    let meta = response.as_ref();
    if meta.is_some() {
        handle.headers_reported.store(true, Ordering::SeqCst);
    }
    Ok(serde_json::json!({
        "ready": meta.is_some() || done,
        "url": meta.map(|meta| meta.url.clone()),
        "status": meta.map(|meta| meta.status),
        "headers": meta.map(|meta| meta.headers.clone()),
        "body": STANDARD.encode(&body),
        "truncated": dropped,
        "done": done,
        "aborted": aborted,
        "error": error.map(|(code, message)| serde_json::json!({
            "code": code,
            "message": message,
        })),
    }))
}

// ---------------------------------------------------------------------------
// fetchAbort / fetchEnd: cancel + registry cleanup
// ---------------------------------------------------------------------------

fn fetch_abort(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    // Unknown id is fine: the JS side aborts opportunistically (e.g. a signal
    // that fired after the fetch already finished and was reaped).
    let handle = fetches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
    if let Some(handle) = handle {
        *lock(&handle.aborted) = true;
        handle.cancel.cancel();
        // A `fetchPoll` parked on the body window must observe the abort.
        handle.activity.notify();
        handle.body_space.notify_one();
    }
    Ok(JsonValue::Bool(true))
}

/// JS-side end-of-life: the final poll was consumed, drop the entry.
///
/// This is also the "consumer went away" edge of the body window's
/// backpressure invariant: a reader parked on a full [`BodyBuf`] must be
/// released (the cancellation token ends its wait, so the parked push returns
/// and the worker thread finishes) before the entry disappears. The normal
/// close path calls this only after the body was drained, so the release is a
/// no-op there; the error path (headers never arrived) and a consumer that
/// abandons the body both need it.
fn fetch_end(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    if let Some(handle) = fetches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id)
    {
        lock(&handle.body).eof = true;
        handle.cancel.cancel();
        handle.activity.notify();
        handle.body_space.notify_one();
    }
    Ok(JsonValue::Bool(true))
}

// ---------------------------------------------------------------------------
// node:http server host ops: a real loopback-only HTTP/1.1 listener
// ---------------------------------------------------------------------------
//
// Real server for `http.Server` (the pi-phone pairing link binds a local
// server): `serverListen` binds 127.0.0.1 (loopback only, documented) and
// runs an accept loop on a detached thread; every connection is parsed by a
// hand-written HTTP/1.1 reader (no new crates: the request line, headers,
// `Content-Length` bodies and `Transfer-Encoding: chunked` are decoded here,
// headers capped at 64 KiB, bodies at 16 MiB).
//
// KEEP-ALIVE: HTTP/1.1 connections persist (and HTTP/1.0 with an explicit
// `Connection: keep-alive`) — the connection thread loops reading serialized
// requests, answers each with `connection: keep-alive`, and closes on the
// negotiated close (client `Connection: close`, HTTP/1.0 default, or server
// shutdown). The idle window between requests is ~5 s (`SERVER_IDLE_TIMEOUT`)
// like Node's default `keepAliveTimeout`. `serverWaitIdle` lets the JS
// `server.close()` wait for in-flight requests to drain.
//
// REQUEST BODIES STREAM: the head is queued as soon as it is parsed and the
// body (Content-Length or chunked) streams into a bounded per-request buffer
// that JS drains through `serverBody` — a chunked upload reaches the handler
// chunk by chunk instead of being aggregated.

/// Largest accepted request body (`Content-Length` or total chunked size).
const MAX_SERVER_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Largest accepted request head (request line + headers).
const MAX_SERVER_HEAD_BYTES: usize = 64 * 1024;

/// Idle/read deadline per connection's FIRST request and for body reads
/// (also bounds a handler that never answers: the connection thread gives up
/// and closes silently).
const SERVER_CONN_TIMEOUT: Duration = Duration::from_secs(120);

/// Idle keep-alive window between requests on one connection (Node's default
/// `server.keepAliveTimeout`).
const SERVER_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Most requests one `serverPoll` drains back to JS.
const MAX_POLL_REQUESTS: usize = 32;

/// Most parsed request heads queued for JS before connection threads park.
///
/// A full queue parks the connection thread that would enqueue one more
/// request: the request is not visible to `serverPoll` until it is enqueued, so
/// a JS side that stopped polling stops consuming new requests instead of
/// growing this queue, and the parked thread's socket closes its TCP window —
/// the same shape Node has (a stalled event loop leaves requests in the kernel
/// backlog; node's user-space side never buffers an unbounded request set).
/// 256 is 8 `serverPoll` windows ([`MAX_POLL_REQUESTS`]) of headroom, and each
/// queued entry is a parsed head bounded by [`MAX_SERVER_HEAD_BYTES`].
const MAX_PENDING_REQUESTS: usize = 256;

/// Hard cap on live HTTP server connections (threads) per process. Each
/// accepted connection owns an OS thread; without a cap a peer opening
/// thousands of sockets would exhaust threads and descriptors before the
/// request queue bound could apply. Over the cap a tiny 503 is written and
/// the socket is closed inline, so no thread is spawned.
const MAX_SERVER_CONNECTIONS: usize = 64;

/// Byte window of one request's streaming body (see [`BodyBuf`]); the
/// connection thread parks on a full window and resumes when `serverBody`
/// drains. Smaller than the fetch window: request bodies are counted in full
/// by the JS pump (`serverBody` drains continuously), and the queue bound
/// multiplies this across connections.
const MAX_SERVER_BODY_BUFFER: usize = 1024 * 1024;

/// Streaming request body: the connection thread pushes, `serverBody` drains.
/// The `space` condvar parks a connection thread whose window is full (the
/// socket stops being read — TCP backpressure — until the JS handler drains,
/// or the wait expires).
struct ReqBody {
    buf: StdMutex<BodyBuf>,
    space: Condvar,
    /// Owning runtime generation: a reload can release a request body whose
    /// connection thread is parked on the window.
    generation: u64,
    /// Set by `release_generation`; a parked push aborts instead of waiting
    /// for a consumer that no longer exists.
    closed: AtomicBool,
}

impl ReqBody {
    fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            buf: StdMutex::new(BodyBuf::new(MAX_SERVER_BODY_BUFFER)),
            space: Condvar::new(),
            generation,
            closed: AtomicBool::new(false),
        })
    }

    /// Push one body chunk, parking while the window is full. Returns `false`
    /// when the wait expired: the caller fails the body read, so the
    /// connection is torn down rather than parking a thread forever on a JS
    /// side that stopped draining.
    fn push_blocking(&self, chunk: &[u8]) -> bool {
        let deadline = Instant::now() + SERVER_CONN_TIMEOUT;
        let mut buf = lock(&self.buf);
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            if buf.push(chunk) {
                // Wake a `serverBody` parked on the empty window (this also
                // wakes a backpressured peer, which re-checks and returns).
                drop(buf);
                self.space.notify_all();
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                crate::ext_diag!(
                    "[ext] http server request body not drained for {:?}; closing the connection (backpressure timeout)",
                    SERVER_CONN_TIMEOUT
                );
                return false;
            }
            let (next, _) = self
                .space
                .wait_timeout(buf, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            buf = next;
        }
    }
}

struct ParsedServerRequest {
    conn_id: u64,
    /// Unique per request (not per connection): the streaming-body key.
    req_id: u64,
    method: String,
    path: String,
    version: String,
    headers: Vec<(String, String)>,
}

struct ServerResponseMsg {
    status: u16,
    status_message: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The negotiated keep-alive decision for this response (from the
    /// request's version/Connection headers + the server state): decides the
    /// `connection:` header and whether the socket persists.
    keep_alive: bool,
}

/// One server: the flag + queue are shared with the accept/conn threads.
///
/// The accept loop BLOCKS in `accept()` (zero idle wakeups, immediate
/// loopback connect); `serverClose`/`release_generation` wake it with one
/// loopback connect after setting `closed`, so the loop observes the flag and
/// exits. The previous nonblocking loop slept 1 ms per `WouldBlock` — 1000
/// wakeups/s per idle server, far outside the <2 wakeups/s budget.
struct ServerHandle {
    /// Owning runtime generation (see `release_generation`).
    generation: u64,
    closed: StdMutex<bool>,
    /// Bound port, used for the self-connect that wakes a blocked `accept`.
    port: u16,
    /// Live accepted connections (bounded by [`MAX_SERVER_CONNECTIONS`]).
    active_connections: AtomicUsize,
    pending: StdMutex<VecDeque<ParsedServerRequest>>,
    /// Wakes connection threads parked on a full `pending` after `serverPoll`
    /// drained it (and on close), and wakes `serverPoll` when a request is
    /// enqueued.
    pending_space: Condvar,
    /// Times a connection thread had to park on a full `pending` (observation:
    /// the queue bound is engaged).
    pending_waits: AtomicU64,
    /// Connections with a request in flight (queued but not yet answered) —
    /// `serverWaitIdle` blocks on this set draining, woken by `busy_changed`.
    busy: StdMutex<std::collections::HashSet<u64>>,
    /// Signalled whenever `busy` loses a connection (or the server closes):
    /// `serverWaitIdle` parks here rather than sleeping 5 ms steps.
    busy_changed: Condvar,
    /// Held (never read) so dropping the handle from `serverClose` releases
    /// the listening socket; the accept thread runs on its own clone.
    #[allow(dead_code)]
    listener: TcpListener,
}

impl ServerHandle {
    /// Park the calling connection thread while `pending` is at
    /// [`MAX_PENDING_REQUESTS`]. Returns `false` when the server closed or the
    /// wait expired (the caller tears its connection down).
    fn wait_for_pending_room(&self) -> bool {
        let deadline = Instant::now() + SERVER_CONN_TIMEOUT;
        let mut pending = lock(&self.pending);
        let mut waited = false;
        while pending.len() >= MAX_PENDING_REQUESTS {
            if *lock(&self.closed) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                crate::ext_diag!(
                    "[ext] http server pending-request queue full ({} requests) and not drained for {:?}; closing the connection (backpressure timeout)",
                    MAX_PENDING_REQUESTS,
                    SERVER_CONN_TIMEOUT
                );
                return false;
            }
            if !waited {
                waited = true;
                let waits = self.pending_waits.fetch_add(1, Ordering::SeqCst) + 1;
                if waits == 1 || waits.is_multiple_of(MAX_PENDING_REQUESTS as u64) {
                    crate::ext_diag!(
                        "[ext] http server pending-request queue full ({} requests): connection threads are parked until serverPoll drains ({} waits)",
                        MAX_PENDING_REQUESTS,
                        waits
                    );
                }
            }
            let (next, _) = self
                .pending_space
                .wait_timeout(pending, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            pending = next;
        }
        !*lock(&self.closed)
    }
}

/// One connection waiting for the JS-side response for its current request.
struct ConnSlot {
    response: StdMutex<Option<ServerResponseMsg>>,
    signal: Condvar,
    /// The request-side keep-alive verdict (`HTTP/1.1` default or explicit
    /// keep-alive on 1.0); `serverRespond` combines it with the JS headers.
    request_keep_alive: StdMutex<bool>,
    /// Set by `release_generation`: the connection thread waiting for the
    /// JS-side response stops instead of waiting out `SERVER_CONN_TIMEOUT`.
    closed: AtomicBool,
}

static SERVERS: OnceLock<StdMutex<HashMap<u64, Arc<ServerHandle>>>> = OnceLock::new();
static SERVER_CONNS: OnceLock<StdMutex<HashMap<u64, Arc<ConnSlot>>>> = OnceLock::new();
static REQ_BODIES: OnceLock<StdMutex<HashMap<u64, Arc<ReqBody>>>> = OnceLock::new();
static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_REQ_ID: AtomicU64 = AtomicU64::new(1);

fn servers() -> &'static StdMutex<HashMap<u64, Arc<ServerHandle>>> {
    SERVERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn server_conns() -> &'static StdMutex<HashMap<u64, Arc<ConnSlot>>> {
    SERVER_CONNS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn req_bodies() -> &'static StdMutex<HashMap<u64, Arc<ReqBody>>> {
    REQ_BODIES.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Header pairs shared by `fetchStart` and the server ops: a JSON array of
/// `[name, value]` string pairs.
fn payload_header_pairs(payload: &JsonValue) -> Vec<(String, String)> {
    payload
        .get("headers")
        .and_then(JsonValue::as_array)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    Some((
                        pair.first()?.as_str()?.to_owned(),
                        pair.get(1)?.as_str()?.to_owned(),
                    ))
                })
                .collect::<Vec<(String, String)>>()
        })
        .unwrap_or_default()
}

fn server_listen(payload: &JsonValue, generation: u64) -> OpResult {
    let port = payload
        .get("port")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(65535) as u16;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
        let code = match error.kind() {
            std::io::ErrorKind::AddrInUse => "EADDRINUSE",
            std::io::ErrorKind::PermissionDenied => "EACCES",
            _ => "EIO",
        };
        FnError::new(code, format!("server listen failed: {error}"))
    })?;
    // A BLOCKING accept: zero idle wakeups and no added loopback connect
    // latency. `serverClose` wakes it with one loopback connect after setting
    // `closed` (see `wake_accept`), so the loop can still be stopped from JS.
    let bound_port = listener
        .local_addr()
        .map_err(|error| FnError::new("EIO", format!("local addr failed: {error}")))?
        .port();
    let handle = Arc::new(ServerHandle {
        generation,
        closed: StdMutex::new(false),
        port: bound_port,
        active_connections: AtomicUsize::new(0),
        pending: StdMutex::new(VecDeque::new()),
        pending_space: Condvar::new(),
        pending_waits: AtomicU64::new(0),
        busy: StdMutex::new(std::collections::HashSet::new()),
        busy_changed: Condvar::new(),
        listener: listener
            .try_clone()
            .map_err(|error| FnError::new("EIO", format!("listener clone failed: {error}")))?,
    });
    let id = NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst);
    servers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&handle));
    let accept_listener = listener;
    let accept_handle = Arc::clone(&handle);
    std::thread::Builder::new()
        .name(format!("pir-http-server-{id}"))
        .spawn(move || loop {
            match accept_listener.accept() {
                Ok((mut stream, _)) => {
                    // The self-connect that wakes a closing server is dropped
                    // here without counting as a connection.
                    if *lock(&accept_handle.closed) {
                        let _ = stream.shutdown(Shutdown::Both);
                        break;
                    }
                    let active = accept_handle
                        .active_connections
                        .fetch_add(1, Ordering::SeqCst);
                    if active >= MAX_SERVER_CONNECTIONS {
                        accept_handle
                            .active_connections
                            .fetch_sub(1, Ordering::SeqCst);
                        reject_server_connection(&mut stream);
                        continue;
                    }
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(SERVER_CONN_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::SeqCst);
                    let conn_handle = Arc::clone(&accept_handle);
                    let guard = ServerConnectionGuard(Arc::clone(&accept_handle));
                    if std::thread::Builder::new()
                        .name(format!("pir-http-conn-{conn_id}"))
                        .spawn(move || {
                            let _guard = guard;
                            serve_connection(&mut stream, conn_id, conn_handle);
                        })
                        .is_err()
                    {
                        // Thread exhaustion: the failed closure drops both
                        // the guard (releasing the slot) and the stream
                        // (closing the connection); the accept loop keeps
                        // serving other peers.
                    }
                }
                Err(_) => {
                    if *lock(&accept_handle.closed) {
                        break;
                    }
                    // Transient accept errors (EMFILE after descriptor
                    // pressure) must not spin: back off briefly and retry.
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })
        .map_err(|error| FnError::new("EIO", format!("server thread spawn failed: {error}")))?;
    Ok(serde_json::json!({ "id": id, "port": bound_port }))
}

/// Releases one connection slot when its thread exits.
struct ServerConnectionGuard(Arc<ServerHandle>);

impl Drop for ServerConnectionGuard {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wake a blocking `accept()` after `closed` is set: one loopback connection
/// completes the pending accept; the loop drops it and exits.
fn wake_accept(handle: &ServerHandle) {
    let address = SocketAddr::from(([127, 0, 0, 1], handle.port));
    let _ = TcpStream::connect_timeout(&address, Duration::from_millis(250));
}

/// Answer 503 on an over-cap connection and close it inline (no thread).
fn reject_server_connection(stream: &mut TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let body = b"Too many connections";
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// One connection: the keep-alive request loop. Each iteration reads one
/// request head, queues it with a fresh streaming-body slot, streams the body
/// into the slot, waits for the JS-side response, writes it, and continues
/// while the connection stays keep-alive.
fn serve_connection(stream: &mut TcpStream, conn_id: u64, server: Arc<ServerHandle>) {
    let mut reader = ConnReader {
        stream,
        buffer: Vec::new(),
    };
    let mut first_request = true;
    loop {
        if *lock(&server.closed) {
            break;
        }
        // Idle window between requests: the first request gets the full
        // connection deadline, keep-alive successors get ~5 s (Node's
        // keepAliveTimeout). A read timeout mid-request (body) is the 120 s
        // deadline set at accept time.
        let idle = if first_request {
            SERVER_CONN_TIMEOUT
        } else {
            SERVER_IDLE_TIMEOUT
        };
        reader.set_read_timeout(idle);
        first_request = false;
        let head = match read_request_head(&mut reader) {
            Ok(head) => head,
            Err(ConnFail::TooLarge) => {
                reader.write_simple(413, "Payload Too Large");
                break;
            }
            Err(ConnFail::Malformed) => {
                reader.write_simple(400, "Bad Request");
                break;
            }
            Err(ConnFail::Io) => break,
        };
        if *lock(&server.closed) {
            break;
        }
        // Keep-alive verdict from the request side: HTTP/1.1 persists unless
        // the client asked to close; HTTP/1.0 needs the explicit opt-in.
        let request_keep_alive = if head.version.contains("1.0") {
            header_has_keep_alive(&head.headers)
        } else {
            !header_has_close(&head.headers)
        };
        let req_id = NEXT_REQ_ID.fetch_add(1, Ordering::SeqCst);
        let body = ReqBody::new(server.generation);
        req_bodies()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(req_id, Arc::clone(&body));
        let slot = Arc::new(ConnSlot {
            response: StdMutex::new(None),
            signal: Condvar::new(),
            request_keep_alive: StdMutex::new(request_keep_alive),
            closed: AtomicBool::new(false),
        });
        server_conns()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(conn_id, Arc::clone(&slot));
        lock(&server.busy).insert(conn_id);
        // Backpressure on the JS-side queue: a full `pending` parks this thread
        // BEFORE the request becomes visible, so a stalled consumer stops
        // consuming requests (the socket's TCP window closes) instead of
        // growing the queue. The wait is bounded; on expiry the connection is
        // dropped.
        if !server.wait_for_pending_room() {
            req_bodies()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&req_id);
            break;
        }
        let headers = head.headers.clone();
        {
            // Notify while holding the queue lock: a `serverPoll` checks the
            // queue and parks under the same mutex, so this cannot land
            // between its check and its wait (no lost wakeup).
            let mut pending = lock(&server.pending);
            pending.push_back(ParsedServerRequest {
                conn_id,
                req_id,
                method: head.method.clone(),
                path: head.path.clone(),
                version: head.version.clone(),
                headers,
            });
            server.pending_space.notify_all();
            drop(pending);
        }
        // Stream the request body into the slot (chunked chunks land one by
        // one; the handler drains through `serverBody` concurrently). The
        // socket must consume the whole body either way — keep-alive framing
        // and an early response both depend on it.
        let body_ok = stream_request_body(&mut reader, &head, &body).is_ok();
        // Wait for the JS-side response (in-flight deadline), then answer.
        let deadline = Instant::now() + SERVER_CONN_TIMEOUT;
        let mut guard = lock(&slot.response);
        let response = loop {
            if let Some(message) = guard.take() {
                break Some(message);
            }
            // A released generation (reload/shutdown) tears this connection
            // down instead of leaving the thread parked for the full
            // connection deadline (`release_generation` sets the slot flag and
            // notifies). A graceful `serverClose` deliberately does NOT set
            // it: in-flight handlers still get to answer while
            // `serverWaitIdle` waits for them.
            if slot.closed.load(Ordering::SeqCst) {
                break None;
            }
            let now = Instant::now();
            if now >= deadline {
                break None;
            }
            let (next, _timeout) = slot
                .signal
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            guard = next;
        };
        drop(guard);
        lock(&server.busy).remove(&conn_id);
        server.busy_changed.notify_all();
        server_conns()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&conn_id);
        req_bodies()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&req_id);
        let keep_alive = body_ok
            && !*lock(&server.closed)
            && match &response {
                Some(message) => message.keep_alive,
                None => false,
            };
        match response {
            Some(message) => {
                reader.write_response(&message);
            }
            // No answer from JS within the deadline: close silently (the
            // client sees a truncated response; the handler stalled).
            None => break,
        }
        if !keep_alive {
            let _ = reader.stream.shutdown(Shutdown::Write);
            break;
        }
    }
    finish_connection(conn_id, &server);
}

/// Negotiated response keep-alive: the request-side verdict, the JS response
/// headers (an explicit `connection: close` wins), and the server state.
fn negotiated_keep_alive(slot: &ConnSlot, headers: &[(String, String)], closed: bool) -> bool {
    if closed {
        return false;
    }
    if headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close")
    }) {
        return false;
    }
    *lock(&slot.request_keep_alive)
}

fn header_has_close(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close")
    })
}

fn header_has_keep_alive(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("keep-alive")
    })
}

fn finish_connection(conn_id: u64, server: &ServerHandle) {
    server_conns()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&conn_id);
    lock(&server.busy).remove(&conn_id);
    server.busy_changed.notify_all();
}

enum ConnFail {
    Io,
    TooLarge,
    Malformed,
}

/// Buffered reader over one connection socket.
struct ConnReader<'a> {
    stream: &'a mut TcpStream,
    buffer: Vec<u8>,
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl ConnReader<'_> {
    fn set_read_timeout(&self, timeout: Duration) {
        let _ = self.stream.set_read_timeout(Some(timeout));
    }

    fn write_response(&mut self, response: &ServerResponseMsg) {
        write_response(self.stream, response);
    }

    fn write_simple(&mut self, status: u16, reason: &str) {
        let message = ServerResponseMsg {
            status,
            status_message: reason.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
            keep_alive: false,
        };
        write_response(self.stream, &message);
    }

    fn fill(&mut self) -> Result<usize, ConnFail> {
        let mut chunk = [0u8; 8192];
        let read = self.stream.read(&mut chunk).map_err(|_| ConnFail::Io)?;
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// Reads up to and including `pattern`; `cap` bounds the scan.
    fn take_until(&mut self, pattern: &[u8], cap: usize) -> Result<Vec<u8>, ConnFail> {
        loop {
            if let Some(position) = find_subslice(&self.buffer, pattern) {
                return Ok(self.buffer.drain(..position + pattern.len()).collect());
            }
            if self.buffer.len() > cap {
                return Err(ConnFail::TooLarge);
            }
            if self.fill()? == 0 {
                return Err(ConnFail::Io);
            }
        }
    }

    fn read_exact_bytes(&mut self, count: usize) -> Result<Vec<u8>, ConnFail> {
        while self.buffer.len() < count {
            if self.fill()? == 0 {
                return Err(ConnFail::Io);
            }
        }
        Ok(self.buffer.drain(..count).collect())
    }

    /// One CRLF-terminated line (terminator stripped).
    fn read_line(&mut self, cap: usize) -> Result<Vec<u8>, ConnFail> {
        let mut line = self.take_until(b"\r\n", cap)?;
        line.truncate(line.len().saturating_sub(2));
        Ok(line)
    }

    /// `Transfer-Encoding: chunked` body: size lines, chunk data, trailers,
    /// with each chunk handed to `sink` as it arrives (streaming semantics).
    /// A sink refusal (the streaming window's backpressure expired) aborts the
    /// read, so the socket is not drained into a request that is already
    /// doomed.
    fn read_chunked_streaming(
        &mut self,
        max_total: usize,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), ConnFail>,
    ) -> Result<(), ConnFail> {
        let mut total = 0usize;
        loop {
            let size_line = self.read_line(1024)?;
            let text = String::from_utf8_lossy(&size_line).into_owned();
            let size_text = text.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_text, 16).map_err(|_| ConnFail::Malformed)?;
            if size == 0 {
                break;
            }
            if total + size > max_total {
                return Err(ConnFail::TooLarge);
            }
            total += size;
            // Read the chunk through the shared buffer in bounded windows so
            // large chunks stream instead of buffering whole.
            let mut remaining = size;
            while remaining > 0 {
                if self.buffer.is_empty() && self.fill()? == 0 {
                    return Err(ConnFail::Io);
                }
                let take = remaining.min(self.buffer.len());
                let data: Vec<u8> = self.buffer.drain(..take).collect();
                remaining -= data.len();
                sink(&data)?;
            }
            let crlf = self.read_exact_bytes(2)?; // trailing CRLF
            if crlf != b"\r\n" {
                return Err(ConnFail::Malformed);
            }
        }
        // Trailers (or the empty line) are best-effort: the request is
        // already complete once the zero chunk arrived.
        for _ in 0..64 {
            match self.read_line(1024) {
                Ok(line) if line.is_empty() => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        Ok(())
    }
}

/// One request head (the body streams separately into the request's
/// `ReqBody` slot).
struct RequestHead {
    method: String,
    path: String,
    version: String,
    headers: Vec<(String, String)>,
    content_length: Option<usize>,
    chunked: bool,
}

fn read_request_head(reader: &mut ConnReader<'_>) -> Result<RequestHead, ConnFail> {
    let head = reader.take_until(b"\r\n\r\n", MAX_SERVER_HEAD_BYTES)?;
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ConnFail::Malformed)?
        .to_ascii_uppercase();
    let path = parts.next().ok_or(ConnFail::Malformed)?.to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1").to_owned();
    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ConnFail::Malformed);
        };
        let (name, value) = (name.trim(), value.trim());
        headers.push((name.to_owned(), value.to_owned()));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true;
            }
            _ => {}
        }
    }
    Ok(RequestHead {
        method,
        path,
        version,
        headers,
        content_length,
        chunked,
    })
}

/// Stream the request body into the request's slot: chunked bodies push each
/// chunk as it arrives (streaming semantics); Content-Length bodies push in
/// bounded windows. A full window parks this thread (`ReqBody::push_blocking`)
/// — the socket stops being read, so the client's TCP window closes, Node's
/// paused-`IncomingMessage` shape — and an expired park fails the read so the
/// connection is torn down instead of silently truncating the body.
/// Otherwise the whole body is consumed: keep-alive framing and early
/// responses depend on the socket being drained.
fn stream_request_body(
    reader: &mut ConnReader<'_>,
    head: &RequestHead,
    body: &ReqBody,
) -> Result<(), ConnFail> {
    let result = if head.chunked {
        reader.read_chunked_streaming(MAX_SERVER_BODY_BYTES, &mut |chunk| {
            if body.push_blocking(chunk) {
                Ok(())
            } else {
                Err(ConnFail::Io)
            }
        })
    } else {
        match head.content_length {
            Some(0) | None => Ok(()),
            Some(length) if length > MAX_SERVER_BODY_BYTES => Err(ConnFail::TooLarge),
            Some(length) => {
                let mut remaining = length;
                let chunk = [0u8; 8192];
                while remaining > 0 {
                    let want = remaining.min(chunk.len());
                    let data = reader.read_exact_bytes(want)?;
                    remaining -= data.len();
                    if !body.push_blocking(&data) {
                        return Err(ConnFail::Io);
                    }
                }
                Ok(())
            }
        }
    };
    {
        // A successful read ends at EOF; a failed one still ends the stream
        // (the JS pump sees `done`) and the connection loop tears the socket
        // down. Either way, wake a `serverBody` parked on the empty window.
        let mut buf = lock(&body.buf);
        buf.eof = true;
        drop(buf);
    }
    body.space.notify_all();
    result
}

fn server_poll(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(MAX_POLL_WAIT_MS);
    let handle = lock(servers())
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no http server with id {id}")))?;
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    {
        // Park on the queue's condvar: the connection thread notifies after
        // each enqueue and `serverClose`/`release_generation` notify on close.
        let mut pending = lock(&handle.pending);
        loop {
            if !pending.is_empty() || *lock(&handle.closed) {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = handle
                .pending_space
                .wait_timeout(pending, (deadline - now).min(RESPONSE_WAIT_SLICE))
                .unwrap_or_else(PoisonError::into_inner);
            pending = next;
        }
    }
    let mut requests = Vec::new();
    let mut pending = lock(&handle.pending);
    while requests.len() < MAX_POLL_REQUESTS {
        let Some(request) = pending.pop_front() else {
            break;
        };
        requests.push(serde_json::json!({
            "connId": request.conn_id,
            "reqId": request.req_id,
            "method": request.method,
            "path": request.path,
            "httpVersion": request.version,
            "headers": request.headers,
        }));
    }
    drop(pending);
    // Release the connection threads parked on a full queue (see
    // `MAX_PENDING_REQUESTS`): the drain above made room.
    handle.pending_space.notify_all();
    Ok(serde_json::json!({ "requests": requests }))
}

/// Drain one window of a request's streaming body (bounded wait like
/// `fetchPoll`). The entry disappears with the connection; an unknown
/// req id reports `done` so the JS pump ends the stream.
fn server_body(payload: &JsonValue) -> OpResult {
    let req_id = payload
        .get("reqId")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing field `reqId`"))?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(MAX_POLL_WAIT_MS);
    let body = match lock(req_bodies()).get(&req_id).cloned() {
        Some(body) => body,
        None => {
            return Ok(serde_json::json!({ "body": "", "done": true, "truncated": false }));
        }
    };
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    {
        // The connection thread notifies `space` after every pushed chunk;
        // park there instead of probing the buffer every 5 ms.
        let mut buf = lock(&body.buf);
        loop {
            if buf.has_data() || buf.eof {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = body
                .space
                .wait_timeout(buf, (deadline - now).min(RESPONSE_WAIT_SLICE))
                .unwrap_or_else(PoisonError::into_inner);
            buf = next;
        }
    }
    let (data, dropped) = lock(&body.buf).take();
    let done = lock(&body.buf).eof;
    // Release the connection thread parked on a full body window (see
    // `MAX_SERVER_BODY_BUFFER`): the drain above made room.
    body.space.notify_all();
    Ok(serde_json::json!({
        "body": STANDARD.encode(&data),
        "done": done,
        "truncated": dropped,
    }))
}

/// Bounded wait until the server has no in-flight requests (`server.close()`
/// waits for active connections to drain). `serverClose` marks the handle
/// closed but keeps it registered until this reaches zero, so JS polls this
/// and the entry is dropped here.
fn server_wait_idle(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(MAX_POLL_WAIT_MS);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let handle = lock(servers()).get(&id).cloned();
    let active = match handle {
        None => 0,
        Some(handle) => {
            // Connection completion notifies `busy_changed`; park there instead
            // of sampling the set every 5 ms.
            let mut busy = lock(&handle.busy);
            loop {
                if busy.is_empty() {
                    break 0;
                }
                let now = Instant::now();
                if now >= deadline {
                    break busy.len();
                }
                let (next, _) = handle
                    .busy_changed
                    .wait_timeout(busy, (deadline - now).min(RESPONSE_WAIT_SLICE))
                    .unwrap_or_else(PoisonError::into_inner);
                busy = next;
            }
        }
    };
    if active == 0 {
        // Fully drained (and closed): release the registry entry.
        servers()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }
    Ok(serde_json::json!({ "active": active }))
}

fn server_respond(payload: &JsonValue) -> OpResult {
    let conn_id = payload
        .get("connId")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing field `connId`"))?;
    let status = payload
        .get("status")
        .and_then(JsonValue::as_u64)
        .unwrap_or(200)
        .min(999) as u16;
    let status_message = payload
        .get("statusMessage")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_owned();
    let headers = payload_header_pairs(payload);
    // Body channel, preferred order (mirrors `fetchStart`): `bodyText` (the
    // exact UTF-8 image of the bytes, sent by the JS side for pure-string
    // response bodies — no base64 round trip on either leg) then `bodyB64`
    // (byte bodies: Buffer/typed arrays, or any string chunk that is not plain
    // UTF-8 text). A non-string `bodyText` is ignored rather than rejected so
    // the byte channel stays the ABI default.
    let body = if let Some(text) = payload.get("bodyText").and_then(JsonValue::as_str) {
        text.as_bytes().to_vec()
    } else {
        payload
            .get("bodyB64")
            .and_then(JsonValue::as_str)
            .map(|data| {
                STANDARD.decode(data.as_bytes()).map_err(|error| {
                    FnError::new("EINVAL", format!("invalid base64 body: {error}"))
                })
            })
            .transpose()?
            .unwrap_or_default()
    };
    let slot = lock(server_conns()).get(&conn_id).cloned().ok_or_else(|| {
        FnError::new(
            "ESRCH",
            format!("no pending http server connection {conn_id}"),
        )
    })?;
    // The keep-alive verdict combines the request side with the JS headers
    // and the server state (the JS layer cannot see the request verdict).
    let closed = payload
        .get("serverId")
        .and_then(JsonValue::as_u64)
        .and_then(|id| lock(servers()).get(&id).cloned())
        .map(|handle| *lock(&handle.closed))
        .unwrap_or(false);
    let keep_alive = negotiated_keep_alive(&slot, &headers, closed);
    *lock(&slot.response) = Some(ServerResponseMsg {
        status,
        status_message,
        headers,
        body,
        keep_alive,
    });
    slot.signal.notify_one();
    Ok(JsonValue::Bool(true))
}

fn server_close(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    if let Some(handle) = servers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
    {
        // Mark closed (stops the accept loop + new keep-alive cycles) but
        // keep the handle registered until `serverWaitIdle` sees zero
        // in-flight requests — `server.close()` waits for the drain first.
        *lock(&handle.closed) = true;
        lock(&handle.pending).clear();
        // Wake the BLOCKING accept loop: it observes `closed` and exits
        // without the former 1 ms poll.
        wake_accept(&handle);
        // Release connection threads parked on the full queue: `closed` is
        // checked on every wakeup, so they tear their connections down.
        handle.pending_space.notify_all();
        handle.busy_changed.notify_all();
    }
    Ok(JsonValue::Bool(true))
}

fn write_response(stream: &mut TcpStream, response: &ServerResponseMsg) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status, response.status_message
    );
    let mut has_content_length = false;
    let mut has_connection = false;
    for (name, value) in &response.headers {
        match name.to_ascii_lowercase().as_str() {
            "content-length" => has_content_length = true,
            "connection" => has_connection = true,
            _ => {}
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if !has_content_length && response.status != 204 && response.status != 304 {
        head.push_str(&format!("content-length: {}\r\n", response.body.len()));
    }
    if !has_connection {
        head.push_str(if response.keep_alive {
            "connection: keep-alive\r\n"
        } else {
            "connection: close\r\n"
        });
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
//
// Real loopback TCP backend for `node:net` (minimal tier: plain TCP over
// 127.0.0.1, no TLS, no Unix-domain sockets, no DNS beyond loopback names).
// The shape is the http server's, reused instead of reinvented — with one
// structural difference the net surface forces: *there is no reader thread
// per connection*.
//
// The http server could afford a thread per connection because each
// connection is a request/response stream the JS side consumes through
// `serverPoll`/`serverBody`; a net socket is a raw byte duplex the JS side
// owns end to end (including the moment it writes), so a host-side reader
// thread would have to hand bytes over through a queue while the JS side
// still needed a poll loop to learn about them. Instead every socket here is
// a **non-blocking** `TcpStream` and each op is a bounded probe:
//
//   * `netListen`   — bind 127.0.0.1 (loopback ONLY), keep the non-blocking
//                     listener registered; `netAccept` probes it.
//   * `netConnect`  — loopback-only `connect_timeout`; the socket is connected
//                     by the time the op returns, so the JS `'connect'` event
//                     is a scheduling detail, not a second host round trip.
//   * `netRead`     — drain up to `maxBytes`; `waitMs > 0` parks the caller
//                     (1 ms probes) until bytes/EOF/deadline, `waitMs = 0` is
//                     the JS pump's non-blocking step. Transport failures
//                     return as the `error` FIELD, not the error envelope:
//                     the JS side reports them as the socket's `'error'`
//                     event (where Node puts them), not as a throw.
//   * `netWrite`    — bounded `write_all` (5 s deadline): a same-thread peer
//                     that never drains cannot hang the runtime forever; the
//                     deadline reports `{ bytes, blocked: true }`.
//   * `netEnd` / `netClose` / `netSetNoDelay` — half close / both-way close
//                     with registry release / TCP_NODELAY.
//
// The JS net pump (net_shim.js.txt) drives accept + read on a timer chain and
// ref-counts its handles, so a live socket/server keeps the process alive and
// `unref()` lets it go — Node's contract.

/// Upper bound of one `netAccept`/`netRead` bounded wait (the JS pump asks
/// for 0 on its steady-state step).
const NET_MAX_WAIT_MS: u64 = 200;

/// Largest window one `netRead` returns (the JS pump loops for more).
const NET_MAX_READ_BYTES: usize = 256 * 1024;

/// Deadline for one `netWrite` before it reports EWOULDBLOCK.
const NET_WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// Partial-window retries that yield instead of sleeping (see `net_write`).
const NET_WRITE_SPINS: u32 = 16;

/// Deadline for one `netConnect` (loopback connects settle immediately; a
/// black-holed address is the only way to reach this).
const NET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One listening server: the non-blocking listener plus the closed flag
/// (`netServerClose` takes the listener out, which releases the port).
struct NetServerHandle {
    /// Owning runtime generation (see `release_generation`).
    generation: u64,
    listener: StdMutex<Option<TcpListener>>,
    closed: StdMutex<bool>,
}

/// One connected socket: a single non-blocking stream. No interior mutex is
/// needed — `TcpStream` is `Sync` and every op runs on the JS thread, so
/// read/write/shutdown only need `&TcpStream`.
struct NetSocketHandle {
    /// Owning runtime generation (see `release_generation`).
    generation: u64,
    stream: TcpStream,
}

static NET_SERVERS: OnceLock<StdMutex<HashMap<u64, Arc<NetServerHandle>>>> = OnceLock::new();
static NET_SOCKETS: OnceLock<StdMutex<HashMap<u64, Arc<NetSocketHandle>>>> = OnceLock::new();
static NEXT_NET_SERVER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_NET_SOCKET_ID: AtomicU64 = AtomicU64::new(1);

fn net_servers() -> &'static StdMutex<HashMap<u64, Arc<NetServerHandle>>> {
    NET_SERVERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn net_sockets() -> &'static StdMutex<HashMap<u64, Arc<NetSocketHandle>>> {
    NET_SOCKETS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Node-style code for a transport failure (the label the JS layer puts on
/// the emitted `Error`).
fn net_io_code(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        std::io::ErrorKind::ConnectionReset => "ECONNRESET",
        std::io::ErrorKind::ConnectionAborted => "ECONNABORTED",
        std::io::ErrorKind::BrokenPipe => "EPIPE",
        std::io::ErrorKind::TimedOut => "ETIMEDOUT",
        std::io::ErrorKind::NotConnected => "ENOTCONN",
        std::io::ErrorKind::AddrInUse => "EADDRINUSE",
        std::io::ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::InvalidInput => "EINVAL",
        std::io::ErrorKind::UnexpectedEof => "ECONNRESET",
        std::io::ErrorKind::WouldBlock => "EAGAIN",
        _ => "EIO",
    }
}

/// Accepted `netListen` hosts: the listener always binds loopback (documented),
/// so the spellings that name loopback are allowed and everything else is
/// rejected with a clear error.
fn net_listen_host_ok(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("loopback"),
    }
}

/// Every loopback address a `netConnect` host may resolve to. A literal must
/// be loopback; a name is resolved and filtered to loopback results (IPv4
/// first: an `::1`-first client would miss a `127.0.0.1` listener).
fn net_loopback_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, FnError> {
    let host = host.trim();
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))]);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !ip.is_loopback() {
            return Err(FnError::new(
                "ERR_NOT_SUPPORTED",
                format!(
                    "node:net in pir connects to loopback addresses only (host `{host}` rejected)"
                ),
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let mut loopbacks: Vec<SocketAddr> =
                addrs.filter(|addr| addr.ip().is_loopback()).collect();
            loopbacks.sort_by_key(|addr| addr.is_ipv6());
            if loopbacks.is_empty() {
                return Err(FnError::new(
                    "ERR_NOT_SUPPORTED",
                    format!("node:net in pir connects to loopback addresses only (host `{host}` rejected)"),
                ));
            }
            Ok(loopbacks)
        }
        Err(_) => Err(FnError::new(
            "ENOTFOUND",
            format!("getaddrinfo ENOTFOUND {host}"),
        )),
    }
}

fn net_socket_addr_fields(stream: &TcpStream) -> (Option<SocketAddr>, Option<SocketAddr>) {
    (stream.local_addr().ok(), stream.peer_addr().ok())
}

/// Register a freshly connected/accepted stream as a socket handle and return
/// the id plus the address fields the JS side stores on the Socket.
fn net_register_socket(stream: TcpStream, generation: u64) -> JsonValue {
    let (local, remote) = net_socket_addr_fields(&stream);
    let id = NEXT_NET_SOCKET_ID.fetch_add(1, Ordering::SeqCst);
    net_sockets()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::new(NetSocketHandle { generation, stream }));
    serde_json::json!({
        "socketId": id,
        "localAddress": local.map(|addr| addr.ip().to_string()),
        "localPort": local.map(|addr| u64::from(addr.port())),
        "remoteAddress": remote.map(|addr| addr.ip().to_string()),
        "remotePort": remote.map(|addr| u64::from(addr.port())),
        "remoteFamily": remote.map(|addr| if addr.is_ipv6() { "IPv6" } else { "IPv4" }),
        "closed": false,
    })
}

fn net_socket_by_id(id: u64) -> Result<Arc<NetSocketHandle>, FnError> {
    lock(net_sockets())
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no net socket with id {id}")))
}

fn net_listen(payload: &JsonValue, generation: u64) -> OpResult {
    let port = payload
        .get("port")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(65535) as u16;
    if let Some(host) = payload.get("host").and_then(JsonValue::as_str) {
        if !net_listen_host_ok(host) {
            return Err(FnError::new(
                "ERR_NOT_SUPPORTED",
                format!("node:net in pir listens on the loopback interface only (host `{host}` rejected)"),
            ));
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
        let code = net_io_code(&error);
        FnError::new(code, format!("server listen failed: {error}"))
    })?;
    listener.set_nonblocking(true).map_err(|error| {
        FnError::new(
            net_io_code(&error),
            format!("listener nonblocking failed: {error}"),
        )
    })?;
    let bound_port = listener
        .local_addr()
        .map_err(|error| FnError::new("EIO", format!("local addr failed: {error}")))?
        .port();
    let id = NEXT_NET_SERVER_ID.fetch_add(1, Ordering::SeqCst);
    net_servers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            id,
            Arc::new(NetServerHandle {
                generation,
                listener: StdMutex::new(Some(listener)),
                closed: StdMutex::new(false),
            }),
        );
    Ok(serde_json::json!({
        "id": id,
        "port": bound_port,
        "address": "127.0.0.1",
        "family": "IPv4",
    }))
}

fn net_server_close(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let handle = lock(net_servers()).remove(&id);
    if let Some(handle) = handle {
        *lock(&handle.closed) = true;
        // Taking the listener out drops the bound socket and releases the
        // port immediately; an accept already probing it returns `socketId: 0`.
        *lock(&handle.listener) = None;
    }
    Ok(JsonValue::Bool(true))
}

fn net_accept(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(NET_MAX_WAIT_MS);
    let handle = lock(net_servers())
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no net server with id {id}")))?;
    if *lock(&handle.closed) {
        return Ok(serde_json::json!({ "socketId": 0, "closed": true }));
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let guard = lock(&handle.listener);
    let Some(listener) = guard.as_ref() else {
        return Ok(serde_json::json!({ "socketId": 0, "closed": true }));
    };
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true).map_err(|error| {
                    FnError::new(
                        net_io_code(&error),
                        format!("accepted socket nonblocking failed: {error}"),
                    )
                })?;
                // Node's default is Nagle ON; leave the OS default and let
                // `netSetNoDelay` change it.
                return Ok(net_register_socket(stream, handle.generation));
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Ok(serde_json::json!({ "socketId": 0 }));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                return Err(FnError::new(
                    net_io_code(&error),
                    format!("accept failed: {error}"),
                ))
            }
        }
    }
}

fn net_connect(payload: &JsonValue, generation: u64) -> OpResult {
    let port = payload
        .get("port")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing field `port`"))?;
    if port == 0 || port > 65535 {
        return Err(FnError::new(
            "ERR_SOCKET_BAD_PORT",
            format!("options.port should be >= 0 and < 65536. Received {port}."),
        ));
    }
    let host = payload
        .get("host")
        .and_then(JsonValue::as_str)
        .unwrap_or("localhost");
    let candidates = net_loopback_addrs(host, port as u16)?;
    let mut last_error: Option<std::io::Error> = None;
    for addr in candidates {
        match TcpStream::connect_timeout(&addr, NET_CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_nonblocking(true).map_err(|error| {
                    FnError::new(
                        net_io_code(&error),
                        format!("socket nonblocking failed: {error}"),
                    )
                })?;
                return Ok(net_register_socket(stream, generation));
            }
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotConnected, "no loopback address")
    });
    let code = net_io_code(&error);
    Err(FnError::new(
        code,
        format!("connect {code} 127.0.0.1:{port}"),
    ))
}

fn net_read(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let wait_ms = payload
        .get("waitMs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0)
        .min(NET_MAX_WAIT_MS);
    let max_bytes = payload
        .get("maxBytes")
        .and_then(JsonValue::as_u64)
        .unwrap_or(65536)
        .clamp(1, NET_MAX_READ_BYTES as u64) as usize;
    let socket = net_socket_by_id(id)?;
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut window = vec![0u8; max_bytes];
    let mut filled = 0usize;
    let mut eof = false;
    let mut error_code: Option<&'static str> = None;
    loop {
        let mut reader = &socket.stream;
        match reader.read(&mut window[filled..]) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(read) => {
                filled += read;
                if filled >= max_bytes {
                    break;
                }
                // Keep draining while the socket is immediately readable: one
                // op per window instead of one op per 64 KiB read.
                continue;
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if filled > 0 || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                error_code = Some(net_io_code(&error));
                break;
            }
        }
    }
    Ok(serde_json::json!({
        "data": STANDARD.encode(&window[..filled]),
        "eof": eof,
        "error": error_code,
    }))
}

fn net_write(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let data = payload
        .get("data")
        .and_then(JsonValue::as_str)
        .map(|encoded| {
            STANDARD
                .decode(encoded.as_bytes())
                .map_err(|error| FnError::new("EINVAL", format!("invalid base64 payload: {error}")))
        })
        .transpose()?
        .unwrap_or_default();
    let socket = net_socket_by_id(id)?;
    let deadline = Instant::now() + NET_WRITE_DEADLINE;
    let mut written = 0usize;
    // A window-full non-blocking socket accepts only what fits; the rest needs
    // the peer's window to open. That happens in the KERNEL (the peer's TCP
    // stack ACKs the bytes already buffered), so the loop must retry fast:
    // `thread::sleep(1ms)` is a ~15 ms OS timer tick on Windows, which turned a
    // 32 KiB loopback write into ~60 ms of dead time per partial window
    // (measured: the E2E's ack path dropped to ~1 ms per 32 KiB write once the
    // spin was added). A short spin covers the common loopback case; the sleep
    // fallback keeps a genuinely stalled peer (one that must read on this same
    // thread) from burning CPU until the deadline.
    let mut spins = 0u32;
    while written < data.len() {
        let progress = {
            let mut writer = &socket.stream;
            match writer.write(&data[written..]) {
                Ok(0) => None,
                Ok(count) => Some(count),
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
                Err(error) => {
                    let code = net_io_code(&error);
                    return Err(FnError::new(code, format!("socket write failed: {error}")));
                }
            }
        };
        match progress {
            Some(count) => {
                written += count;
                spins = 0;
            }
            None => {
                if Instant::now() >= deadline {
                    return Ok(serde_json::json!({ "bytes": written, "blocked": true }));
                }
                if spins < NET_WRITE_SPINS {
                    spins += 1;
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    Ok(serde_json::json!({ "bytes": written, "blocked": false }))
}

fn net_end(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let socket = net_socket_by_id(id)?;
    // A half close on an already-closed peer is not an error (`end()` is
    // idempotent in Node); only the transport teardown is reported.
    match socket.stream.shutdown(Shutdown::Write) {
        Ok(()) => Ok(JsonValue::Bool(true)),
        Err(ref error) if error.kind() == std::io::ErrorKind::NotConnected => {
            Ok(JsonValue::Bool(false))
        }
        Err(error) => {
            let code = net_io_code(&error);
            Err(FnError::new(
                code,
                format!("socket shutdown failed: {error}"),
            ))
        }
    }
}

fn net_set_no_delay(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let value = payload
        .get("value")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);
    let socket = net_socket_by_id(id)?;
    socket.stream.set_nodelay(value).map_err(|error| {
        let code = net_io_code(&error);
        FnError::new(code, format!("setNoDelay failed: {error}"))
    })?;
    Ok(JsonValue::Bool(true))
}

fn net_close(payload: &JsonValue) -> OpResult {
    let id = payload_id(payload)?;
    let socket = lock(net_sockets()).remove(&id);
    if let Some(socket) = socket {
        let _ = socket.stream.shutdown(Shutdown::Both);
    }
    Ok(JsonValue::Bool(true))
}

// ---------------------------------------------------------------------------
// Generation release (runtime drop / actor Shutdown)
// ---------------------------------------------------------------------------

/// Cancel/release every fetch, server, request body and net handle opened by
/// `generation`. A `/reload` (or session shutdown) must not leave the previous
/// generation's listener bound, its connections parked for 120 s, or its
/// fetch worker streaming a response for a runtime that is gone. Handles of a
/// successor generation are untouched.
pub(super) fn release_generation(generation: u64) {
    // 1) In-flight fetches: cancel the worker, mark the stream aborted, and
    //    release a body reader parked on a full window.
    let stale_fetches: Vec<Arc<FetchHandle>> = {
        let mut table = fetches().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for handle in stale_fetches {
        *lock(&handle.aborted) = true;
        *lock(&handle.done) = true;
        lock(&handle.body).eof = true;
        handle.cancel.cancel();
        handle.activity.notify();
        handle.body_space.notify_one();
    }

    // 2) HTTP servers: stop the accept loop, drop the listener, release the
    //    connection threads waiting for a JS response, and remove every
    //    request body the server owned.
    let stale_servers: Vec<Arc<ServerHandle>> = {
        let mut table = servers().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for server in stale_servers {
        *lock(&server.closed) = true;
        lock(&server.pending).clear();
        wake_accept(&server);
        server.pending_space.notify_all();
        server.busy_changed.notify_all();
        let conn_ids: Vec<u64> = lock(&server.busy).iter().copied().collect();
        for conn_id in conn_ids {
            if let Some(slot) = server_conns()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&conn_id)
            {
                // The response wait checks `closed` before parking again, so
                // the connection thread leaves now instead of after the
                // connection deadline. Setting the flag under the same mutex
                // the waiter parks on closes the check-then-park race.
                let _guard = lock(&slot.response);
                slot.closed.store(true, Ordering::SeqCst);
                slot.signal.notify_all();
            }
        }
    }
    let stale_bodies: Vec<Arc<ReqBody>> = {
        let mut table = req_bodies().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, body)| body.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for body in stale_bodies {
        let _guard = lock(&body.buf);
        body.closed.store(true, Ordering::SeqCst);
        body.space.notify_all();
    }

    // 3) node:net: close listeners (which releases their ports immediately)
    //    and sockets.
    let stale_net_servers: Vec<Arc<NetServerHandle>> = {
        let mut table = net_servers().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for handle in stale_net_servers {
        *lock(&handle.closed) = true;
        *lock(&handle.listener) = None;
    }
    let stale_net_sockets: Vec<Arc<NetSocketHandle>> = {
        let mut table = net_sockets().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for socket in stale_net_sockets {
        let _ = socket.stream.shutdown(Shutdown::Both);
    }
}

/// Live HTTP/net handle counts `(fetches, servers, request bodies, net
/// servers, net sockets)`; the generation-release regression reads them.
#[doc(hidden)]
pub fn http_resource_counts_for_tests() -> (usize, usize, usize, usize, usize) {
    let fetches = fetches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let servers = servers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let bodies = req_bodies()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let net_servers = net_servers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let net_sockets = net_sockets()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    (fetches, servers, bodies, net_servers, net_sockets)
}
