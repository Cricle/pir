//! Loopback OAuth redirect handler shared by the browser sign-in flows.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/callback-server.ts`
//! (`startOAuthCallbackServer` / `waitForCallbackOrManualInput`), served with a
//! hand-rolled HTTP/1.1 responder over a Tokio TCP listener. The server binds
//! loopback only, serves the upstream success/error pages, and every wait path
//! is bounded (timeout, explicit close, or explicit cancel).
//!
//! Each accepted connection is handled in its own spawned task (upstream
//! `node:http` concurrency): an idle browser spare connection can never stall
//! the accept loop, and `close()` mirrors upstream's
//! `closeAllConnections` semantics by stopping completion.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Notify};

use super::oauth_page::{oauth_error_html, oauth_success_html};
use crate::ai::error::{AiError, AiResult};

/// How long a single browser connection may take to deliver its request.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on one request head (URL line + headers).
const MAX_HEAD_BYTES: usize = 16 * 1024;

async fn send_page(stream: &mut TcpStream, status: u16, reason: &str, html: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n{html}",
        status = status,
        reason = reason,
        len = html.len(),
        html = html,
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Extract the request target (path + query) from one HTTP request head.
fn request_target(bytes: &[u8]) -> Option<String> {
    let head_end = bytes.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
    let line = head.lines().next()?;
    let target = line.split_whitespace().nth(1)?;
    Some(target.to_string())
}

/// Read from the socket until the `\r\n\r\n` head terminator arrives, bounded
/// by [`CONNECTION_TIMEOUT`] and [`MAX_HEAD_BYTES`].
async fn read_request_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            buffer.truncate(head_end + 4);
            return Some(buffer);
        }
        if buffer.len() >= MAX_HEAD_BYTES {
            return None;
        }
        let remaining = MAX_HEAD_BYTES - buffer.len().min(MAX_HEAD_BYTES);
        let want = chunk.len().min(remaining);
        let read = tokio::time::timeout(CONNECTION_TIMEOUT, stream.read(&mut chunk[..want]))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Parse a query string into `(key, value)` pairs, percent-decoding both.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    fn decode(value: &str) -> String {
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
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                    if let Ok(byte) = u8::from_str_radix(hex, 16) {
                        out.push(byte);
                        index += 3;
                    } else {
                        out.push(bytes[index]);
                        index += 1;
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
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Look up a query parameter.
pub fn query_param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// The provider-reported authorization error carried by a `?error=` callback
/// (upstream rejects the wait with `finish({error})`). `None` when the query
/// carries no `error` parameter.
pub fn authorization_error(query: &[(String, String)]) -> Option<String> {
    let error = query_param(query, "error")?;
    Some(match query_param(query, "error_description") {
        Some(description) if description != error => format!("{error}: {description}"),
        _ => error.to_string(),
    })
}

/// Query parameters delivered by one browser callback.
type CallbackQuery = Vec<(String, String)>;

/// The deferred browser page outcome for a delivered callback (upstream sends
/// the success/failure page only after `complete` settles).
enum PageDecision {
    Success,
    Failed(String),
}

/// A running callback server. Dropping it detaches the accept loop; the wait
/// side is finished with [`Self::wait`], [`Self::cancel`], or [`Self::close`].
pub struct OAuthCallbackServer {
    redirect_uri: String,
    port: u16,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<CallbackQuery>>>,
    state: Arc<ServerState>,
}

struct ServerState {
    /// A completion (or terminal stop) has been processed.
    settled: AtomicBool,
    /// `cancel()` was called: wait resolves with `Cancelled`.
    cancelled: AtomicBool,
    /// `close()` was called: the accept loop exits on its next wake-up.
    closed: AtomicBool,
    notify: Notify,
    /// Sender half of the deferred-page gate: `confirm_page` takes it and
    /// answers the connection handler that delivered the callback.
    page_gate: Mutex<Option<oneshot::Sender<PageDecision>>>,
    /// Receiver half, parked until the connection handler that received a
    /// valid `code` takes it and waits for the flow's exchange outcome.
    page_decision: Mutex<Option<oneshot::Receiver<PageDecision>>>,
}

impl ServerState {
    fn wake(&self) {
        self.notify.notify_waiters();
    }
}

/// Result of waiting on the callback server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackWait {
    /// The browser callback delivered its full parsed query string. Flows
    /// extract `code` (and any flow-specific parameters such as the issued
    /// OpenAI `client_id`) from the pairs.
    Completed(CallbackQuery),
    /// `cancel()` stopped the wait; fall back to the manual prompt.
    Cancelled,
}

impl OAuthCallbackServer {
    /// Start the loopback callback server (upstream `startOAuthCallbackServer`).
    ///
    /// `host` must be loopback. Port `0` picks a free port (the bound port is
    /// reflected in [`Self::redirect_uri`]/[`Self::port`]). When `state` is
    /// given the callback must echo it, mirroring upstream's state check.
    pub async fn start(
        provider_name: &str,
        host: &str,
        port: u16,
        path: &str,
        redirect_host: Option<&str>,
        state: Option<&str>,
    ) -> AiResult<Self> {
        let is_loopback = host == "localhost"
            || host == "::1"
            || host
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        if !is_loopback {
            return Err(AiError::Authentication(format!(
                "OAuth callback server refuses to bind non-loopback host {host}"
            )));
        }
        let listener = TcpListener::bind((host, port))
            .await
            .map_err(|error| AiError::Io(std::io::Error::new(error.kind(), error.to_string())))?;
        let bound_port = listener.local_addr().map_err(AiError::Io)?.port();
        let redirect_host = redirect_host.unwrap_or(host);
        let host_part = if redirect_host.contains(':') {
            format!("[{redirect_host}]")
        } else {
            redirect_host.to_string()
        };
        let redirect_uri = format!("http://{host_part}:{bound_port}{path}");

        let (complete_tx, complete_rx) = mpsc::unbounded_channel::<CallbackQuery>();
        let (page_tx, page_rx) = oneshot::channel::<PageDecision>();
        let expected_state = state.map(str::to_string);
        let state = Arc::new(ServerState {
            settled: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
            page_gate: Mutex::new(Some(page_tx)),
            page_decision: Mutex::new(Some(page_rx)),
        });

        let accept_state = state.clone();
        let provider = provider_name.to_string();
        let path = path.to_string();
        tokio::spawn(async move {
            accept_loop(
                listener,
                accept_state,
                provider,
                expected_state,
                path,
                complete_tx,
            )
            .await;
        });

        Ok(Self {
            redirect_uri,
            port: bound_port,
            receiver: Mutex::new(Some(complete_rx)),
            state,
        })
    }

    /// The redirect URI the provider must be configured with.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The bound port (useful when port 0 was requested).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for the browser callback, bounded by `timeout` when given.
    pub async fn wait(&self, timeout: Option<Duration>) -> AiResult<CallbackWait> {
        let mut receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                AiError::Authentication("OAuth callback already waited or closed".to_string())
            })?;
        let wait = async {
            loop {
                // Register the waiter BEFORE re-checking the flags:
                // `notify_waiters()` stores no permit, so a `cancel()` landing
                // between the check and the first poll of `notified()` would
                // otherwise be lost and this wait would never wake up.
                let notified = self.state.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                // Re-check the flags after registering so a notify that fired
                // before this round is still observed.
                if self.state.cancelled.load(Ordering::SeqCst) {
                    return Ok(CallbackWait::Cancelled);
                }
                if self.state.closed.load(Ordering::SeqCst) {
                    return Ok(CallbackWait::Cancelled);
                }
                tokio::select! {
                    pairs = receiver.recv() => {
                        return Ok(match pairs {
                            Some(pairs) => CallbackWait::Completed(pairs),
                            None => CallbackWait::Cancelled,
                        });
                    }
                    () = notified => {
                        // A cancel/close raced the recv; re-check the flags.
                        continue;
                    }
                }
            }
        };
        match timeout {
            Some(timeout) => tokio::time::timeout(timeout, wait).await.map_err(|_| {
                AiError::Authentication("OAuth callback wait timed out".to_string())
            })?,
            None => wait.await,
        }
    }

    /// Stop waiting for the browser unless a callback was already completed
    /// (upstream `cancel`).
    pub fn cancel(&self) {
        if !self.state.settled.load(Ordering::SeqCst) {
            self.state.cancelled.store(true, Ordering::SeqCst);
            self.state.settled.store(true, Ordering::SeqCst);
            self.state.wake();
        }
    }

    /// Answer the deferred browser page for a callback that was delivered:
    /// `None` sends the success page, `Some(error)` sends a 502 failure page
    /// carrying the exchange error (upstream awaits `complete(code)` before
    /// the page, so the browser sees real exchange failures). Harmless when no
    /// callback connection is waiting.
    pub fn confirm_page(&self, error: Option<String>) {
        let sender = self
            .state
            .page_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let decision = match error {
                None => PageDecision::Success,
                Some(error) => PageDecision::Failed(error),
            };
            let _ = sender.send(decision);
        }
    }

    /// Stop the server; any pending wait resolves cancelled (upstream `close`
    /// plus `closeAllConnections`: in-flight handlers no longer complete).
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        self.state.settled.store(true, Ordering::SeqCst);
        // A connection handler may still be waiting for the deferred page
        // outcome; failing it beats leaving the browser hanging after the
        // server is gone (upstream closes the server, ending the wait).
        let sender = self
            .state
            .page_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(PageDecision::Failed(
                "OAuth callback server closed".to_string(),
            ));
        }
        self.state.wake();
    }
}

/// Accept loop: spawn a handler task per connection until a close or a cancel.
/// Every connection is bounded by [`CONNECTION_TIMEOUT`]; an idle connection
/// (a browser spare opened ahead of the callback) never blocks the others.
async fn accept_loop(
    listener: TcpListener,
    state: Arc<ServerState>,
    provider_name: String,
    expected_state: Option<String>,
    path: String,
    complete_tx: mpsc::UnboundedSender<CallbackQuery>,
) {
    loop {
        // Register before re-checking the flags: `notify_waiters()` stores no
        // permit, so a close/cancel between the check and the await could be
        // lost and leave this accept loop parked forever.
        let notified = state.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if state.closed.load(Ordering::SeqCst) || state.cancelled.load(Ordering::SeqCst) {
            return;
        }
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = notified => {
                if state.closed.load(Ordering::SeqCst) || state.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
        };
        let Ok((stream, _)) = accepted else {
            return;
        };
        let handler_state = state.clone();
        let provider = provider_name.clone();
        let expected = expected_state.clone();
        let route = path.clone();
        let tx = complete_tx.clone();
        tokio::spawn(async move {
            if handle_connection(
                stream,
                &provider,
                expected.as_deref(),
                &route,
                &handler_state,
                &tx,
            )
            .await
            {
                handler_state.settled.store(true, Ordering::SeqCst);
                handler_state.wake();
            }
        });
    }
}

/// Validate and answer one callback request; returns whether it completed the
/// sign-in (mirrors upstream's route/state/claimed/error/code checks). The
/// full parsed query travels through the channel so flows can read
/// flow-specific parameters (the issued OpenAI client id, etc.).
async fn handle_connection(
    mut stream: TcpStream,
    provider_name: &str,
    expected_state: Option<&str>,
    path: &str,
    state: &ServerState,
    complete_tx: &mpsc::UnboundedSender<CallbackQuery>,
) -> bool {
    let Some(bytes) = read_request_head(&mut stream).await else {
        return false;
    };
    let Some(target) = request_target(&bytes) else {
        send_page(
            &mut stream,
            404,
            "Not Found",
            &oauth_error_html("Callback route not found.", None),
        )
        .await;
        return false;
    };
    let (request_path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.clone(), Vec::new()),
    };
    if request_path != path {
        send_page(
            &mut stream,
            404,
            "Not Found",
            &oauth_error_html("Callback route not found.", None),
        )
        .await;
        return false;
    }
    if let Some(expected) = expected_state {
        if query_param(&query, "state") != Some(expected) {
            send_page(
                &mut stream,
                400,
                "Bad Request",
                &oauth_error_html("State mismatch.", None),
            )
            .await;
            return false;
        }
    }
    if state.settled.load(Ordering::SeqCst) {
        send_page(
            &mut stream,
            409,
            "Conflict",
            &oauth_error_html("This sign-in has already been handled.", None),
        )
        .await;
        return false;
    }
    if let Some(error) = query_param(&query, "error") {
        let description = query_param(&query, "error_description")
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_owned());
        // Settle the pending wait with the error (upstream `finish({error})`
        // rejects): deliver the query so the flow's wait resolves and the
        // login fails with the provider's error instead of hanging until the
        // manual prompt decides the outcome.
        state.settled.store(true, Ordering::SeqCst);
        let _ = complete_tx.send(query.clone());
        send_page(
            &mut stream,
            400,
            "Bad Request",
            &oauth_error_html(
                &format!("{provider_name} authorization failed."),
                Some(description.as_str()),
            ),
        )
        .await;
        return true;
    }
    if query_param(&query, "code").is_none() {
        send_page(
            &mut stream,
            400,
            "Bad Request",
            &oauth_error_html("Missing authorization code.", None),
        )
        .await;
        return false;
    }
    // Claim the sign-in before answering (upstream `claimed = true`): a
    // concurrent callback gets 409 while the exchange runs.
    state.settled.store(true, Ordering::SeqCst);
    let decision = state
        .page_decision
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match decision {
        Some(receiver) => {
            // Deferred page: deliver the query first so the flow can run the
            // token exchange while the browser waits, then answer with the
            // real outcome (upstream sends the page after `complete`).
            let completed = complete_tx.send(query).is_ok();
            match receiver.await.ok() {
                Some(PageDecision::Success) => {
                    send_page(
                        &mut stream,
                        200,
                        "OK",
                        &oauth_success_html(&format!(
                            "Signed in to {provider_name}. You may now close this page."
                        )),
                    )
                    .await;
                }
                outcome => {
                    let message = match outcome {
                        Some(PageDecision::Failed(message)) => message,
                        _ => {
                            format!("{provider_name} sign-in failed before the page was confirmed.")
                        }
                    };
                    send_page(
                        &mut stream,
                        502,
                        "Bad Gateway",
                        &oauth_error_html(
                            &format!("{provider_name} sign-in failed."),
                            Some(&message),
                        ),
                    )
                    .await;
                }
            }
            completed
        }
        None => {
            send_page(
                &mut stream,
                200,
                "OK",
                &oauth_success_html(&format!(
                    "Signed in to {provider_name}. You may now close this page."
                )),
            )
            .await;
            complete_tx.send(query).is_ok()
        }
    }
}
