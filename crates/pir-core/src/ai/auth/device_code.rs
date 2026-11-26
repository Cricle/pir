//! RFC 8628 device-code polling shared by the device flows.
//!
//! Rust equivalent of upstream `packages/ai/src/auth/oauth/device-code.ts`:
//! interval handling, `slow_down` adjustment (server-provided interval wins,
//! otherwise +5s per RFC 8628 §3.5), deadline enforcement, and the distinct
//! timeout messages for clock-drift diagnosis.

use std::time::{Duration, Instant};

use crate::ai::error::{AiError, AiResult};

const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: u64 = 1000;
/// RFC 8628 section 3.2: a missing `interval` means the client polls every 5s.
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// RFC 8628 section 3.5: `slow_down` increases the interval by 5 seconds.
const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// An incomplete poll outcome (upstream `OAuthDeviceCodeIncompletePollResult`).
pub enum DevicePoll<T> {
    Pending,
    SlowDown { interval_seconds: Option<u64> },
    Failed { message: String },
    Complete(T),
}

impl<T> DevicePoll<T> {
    /// Map a poll result through a function (convenience for adapters).
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> DevicePoll<U> {
        match self {
            DevicePoll::Pending => DevicePoll::Pending,
            DevicePoll::SlowDown { interval_seconds } => DevicePoll::SlowDown { interval_seconds },
            DevicePoll::Failed { message } => DevicePoll::Failed { message },
            DevicePoll::Complete(value) => DevicePoll::Complete(f(value)),
        }
    }
}

fn timeout_error(slow_downs: u32) -> AiError {
    AiError::Authentication(
        if slow_downs > 0 {
            SLOW_DOWN_TIMEOUT_MESSAGE
        } else {
            TIMEOUT_MESSAGE
        }
        .to_string(),
    )
}

/// Poll until completion, failure, or the deadline (upstream
/// `pollOAuthDeviceCodeFlow`). `wait_before_first_poll` mirrors upstream's
/// `waitBeforeFirstPoll` (GitHub/Kimi/xAI set it).
pub async fn poll_device_code_flow<T, F, Fut>(
    interval_seconds: Option<u64>,
    expires_in_seconds: Option<u64>,
    wait_before_first_poll: bool,
    mut poll: F,
) -> AiResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AiResult<DevicePoll<T>>>,
{
    // `expires_in` is provider-controlled. `Instant + Duration` panics on
    // overflow, so an absurd value (which a server could always omit to get
    // the same no-deadline behavior) degrades to "no deadline" instead of
    // crashing the login flow.
    let deadline = expires_in_seconds
        .and_then(|seconds| Instant::now().checked_add(Duration::from_secs(seconds)));
    let mut interval_ms = interval_seconds
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
        .saturating_mul(1000)
        .max(MINIMUM_INTERVAL_MS);
    let mut slow_downs: u32 = 0;

    if wait_before_first_poll {
        let remaining = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        match remaining {
            Some(remaining) if remaining.is_zero() => return Err(timeout_error(slow_downs)),
            Some(remaining) => {
                tokio::time::sleep(remaining.min(Duration::from_millis(interval_ms))).await;
            }
            None => {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        }
    }

    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(timeout_error(slow_downs));
            }
        }
        match poll().await? {
            DevicePoll::Complete(value) => return Ok(value),
            DevicePoll::Failed { message } => {
                return Err(AiError::Authentication(message));
            }
            DevicePoll::SlowDown { interval_seconds } => {
                slow_downs += 1;
                // A server-provided interval is authoritative (GitHub reports
                // the new minimum); otherwise apply RFC 8628 §3.5 (+5s).
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds > 0 => {
                        seconds.saturating_mul(1000).max(MINIMUM_INTERVAL_MS)
                    }
                    _ => (interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS).max(MINIMUM_INTERVAL_MS),
                };
            }
            DevicePoll::Pending => {}
        }
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_error(slow_downs));
            }
            tokio::time::sleep(remaining.min(Duration::from_millis(interval_ms))).await;
        } else {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }
}
