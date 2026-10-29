//! Regression tests for the TUI OAuth login race (F1) and the callback
//! server's cancel wake-up (F10).
//!
//! The TUI login runs on a `new_current_thread()` runtime: the OAuth callback
//! server's accept loop is a task on that same runtime. The login interaction
//! must therefore await its internal dialog reply instead of blocking the
//! thread, otherwise the browser redirect can never be accepted while the
//! manual "paste the code" prompt is pending.
//!
//! `TuiLoginInteraction` itself is private to `src/tui/terminal.rs`, so these
//! tests exercise the same contract through the public interaction boundary:
//! a fake `AuthInteraction` whose `prompt` awaits a tokio oneshot (exactly
//! what the TUI now does) raced against a real `OAuthCallbackServer` on a
//! current-thread runtime.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use pir::ai::auth::callback_server::{CallbackWait, OAuthCallbackServer};
use pir::ai::auth::{AuthEvent, AuthInteraction, AuthPrompt};
use pir::ai::error::{AiError, AiResult};

/// An interaction whose prompt awaits a oneshot, matching the TUI's
/// `InternalReply::ForwardedAsync` path.
struct AsyncPromptInteraction {
    reply: Mutex<Option<oneshot::Receiver<String>>>,
    prompt_polled: AtomicBool,
    prompt_resolved: AtomicBool,
}

impl AsyncPromptInteraction {
    fn new(reply: oneshot::Receiver<String>) -> Self {
        Self {
            reply: Mutex::new(Some(reply)),
            prompt_polled: AtomicBool::new(false),
            prompt_resolved: AtomicBool::new(false),
        }
    }
}

impl AuthInteraction for AsyncPromptInteraction {
    fn prompt<'a>(
        &'a self,
        _prompt: AuthPrompt,
    ) -> Pin<Box<dyn Future<Output = AiResult<String>> + Send + 'a>> {
        Box::pin(async move {
            self.prompt_polled.store(true, Ordering::SeqCst);
            let receiver = self
                .reply
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .ok_or_else(|| AiError::Authentication("prompt already taken".to_string()))?;
            let text = receiver
                .await
                .map_err(|_| AiError::Authentication("login prompt closed".to_string()))?;
            self.prompt_resolved.store(true, Ordering::SeqCst);
            Ok(text)
        })
    }

    fn notify(&self, _event: AuthEvent) {}
}

/// Deliver one callback over a raw loopback connection, then report progress.
async fn deliver_callback(port: u16, delivered: Arc<AtomicBool>) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the callback server");
    stream
        .write_all(
            b"GET /callback?code=code-1&state=st HTTP/1.1\r\n\
              Host: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write the callback request");
    delivered.store(true, Ordering::SeqCst);
}

/// With a current-thread runtime, the callback server's spawned task must make
/// progress (and settle the wait) while the manual prompt is still pending.
/// A blocking `recv` in the prompt path starves exactly this task.
#[tokio::test(flavor = "current_thread")]
async fn callback_settles_while_async_prompt_is_pending() {
    let server = OAuthCallbackServer::start("Test", "127.0.0.1", 0, "/callback", None, Some("st"))
        .await
        .expect("callback server starts");
    let port = server.port();

    let (reply_tx, reply_rx) = oneshot::channel();
    // Keep the prompt pending for the whole race: dropping the sender would
    // resolve (error) the prompt instead.
    let _reply_tx = reply_tx;
    let interaction = AsyncPromptInteraction::new(reply_rx);

    let delivered = Arc::new(AtomicBool::new(false));
    tokio::spawn(deliver_callback(port, Arc::clone(&delivered)));

    let wait = server.wait(Some(Duration::from_secs(5)));
    let prompt = interaction.prompt(AuthPrompt::ManualCode {
        message: "paste the code".to_string(),
        placeholder: None,
    });
    tokio::pin!(wait);
    tokio::pin!(prompt);

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut wait => {
                assert!(
                    !interaction.prompt_resolved.load(Ordering::SeqCst),
                    "the callback must win while the prompt is still pending"
                );
                result
            }
            text = &mut prompt => {
                panic!("the prompt resolved before the callback: {text:?}")
            }
        }
    })
    .await
    .expect("the callback must be served while the prompt awaits")
    .expect("callback wait result");

    assert!(interaction.prompt_polled.load(Ordering::SeqCst));
    assert!(!interaction.prompt_resolved.load(Ordering::SeqCst));
    match result {
        CallbackWait::Completed(pairs) => {
            assert_eq!(
                pairs
                    .iter()
                    .find(|(key, _)| key == "code")
                    .map(|(_, value)| value.as_str()),
                Some("code-1")
            );
        }
        other => panic!("expected a completed callback, got {other:?}"),
    }

    // The spawned delivery task finished too: the loosing prompt future did
    // not prevent the runtime from driving it.
    tokio::time::timeout(Duration::from_secs(1), async {
        while !delivered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("callback delivery task made progress");

    server.confirm_page(None);
    server.close();
}

/// The manual prompt winning cancels the callback wait; `cancel()` must wake a
/// waiter that is already parked on `Notify` (the lost-wakeup regression).
#[tokio::test(flavor = "current_thread")]
async fn manual_prompt_wins_and_cancels_the_pending_callback_wait() {
    let server = Arc::new(
        OAuthCallbackServer::start("Test", "127.0.0.1", 0, "/callback", None, Some("st"))
            .await
            .expect("callback server starts"),
    );

    let (reply_tx, reply_rx) = oneshot::channel();
    let interaction = AsyncPromptInteraction::new(reply_rx);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let _ = reply_tx.send("pasted-code".to_string());
    });

    let mut waiter = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.wait(None).await })
    };
    let prompt = interaction.prompt(AuthPrompt::ManualCode {
        message: "paste the code".to_string(),
        placeholder: None,
    });
    tokio::pin!(prompt);

    let text = tokio::select! {
        result = &mut waiter => panic!("the callback won unexpectedly: {result:?}"),
        text = &mut prompt => text.expect("prompt reply"),
    };
    assert_eq!(text, "pasted-code");

    // The flow's losing branch: cancel the callback wait, which must resolve
    // the parked waiter instead of hanging it until the server closes.
    server.cancel();
    let cancelled = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("cancel must wake the pending callback wait")
        .expect("waiter join");
    assert_eq!(cancelled.expect("wait result"), CallbackWait::Cancelled);
}

/// A cancel racy with the waiter's flag check must never be lost: run many
/// start-wait-cancel rounds.
#[tokio::test(flavor = "current_thread")]
async fn cancel_never_loses_the_wakeup_race() {
    for _ in 0..64 {
        let server = Arc::new(
            OAuthCallbackServer::start("Test", "127.0.0.1", 0, "/callback", None, None)
                .await
                .expect("callback server starts"),
        );
        let waiter = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.wait(None).await })
        };
        // Give the waiter a chance to park on the notify before cancelling,
        // exercising the wake path; the flag check path covers the other side
        // of the race.
        tokio::task::yield_now().await;
        server.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("cancel must wake the waiter")
            .expect("waiter join");
        assert_eq!(result.expect("wait result"), CallbackWait::Cancelled);
    }
}
