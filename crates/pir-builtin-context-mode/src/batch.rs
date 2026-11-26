//! Pure `ctx_batch` request validation and reply shaping.
//!
//! The host tool keeps the side-effecting half (shell spawn, per-command
//! deadline, index writes). Everything that can be decided from the arguments
//! alone lives here so it can be tested without a runtime:
//!
//! - command/query extraction and bounds (1-16 commands, 1-32 queries of at
//!   most 512 bytes each),
//! - deadline resolution (`timeoutMs` wins over `timeout`, clamped to 120 s,
//!   default 30 s),
//! - the stable batch source label (`ctx_batch:<fnv1a>`),
//! - the 16 KiB reply cap.

use std::time::Duration;

use serde_json::Value as JsonValue;

use crate::truncate_utf8;

/// Reply budget (upstream `OUTPUT_CAP`).
pub const OUTPUT_CAP: usize = 16 * 1024;
/// Commands per call (upstream A13 bound).
pub const MAX_COMMANDS: usize = 16;
/// Upper bound on the number of queries in one call (matches `ctx_search`).
pub const MAX_SEARCH_QUERIES: usize = 32;
/// Upper bound on one query's length in bytes.
pub const MAX_QUERY_BYTES: usize = 512;
/// Hits returned per query by the batch reply.
pub const HITS_PER_QUERY: usize = 5;
/// Default whole-call deadline.
pub const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
/// Hard ceiling for the whole-call deadline.
pub const MAX_TIMEOUT_SECS: f64 = 120.0;

/// A validated `ctx_batch` call.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchRequest {
    pub commands: Vec<String>,
    pub queries: Vec<String>,
    pub timeout: Duration,
}

impl BatchRequest {
    /// Parse and validate raw tool arguments.
    ///
    /// Non-string array elements and blank entries are dropped, matching the
    /// core tool's tolerant extraction; the bounds are then applied to the
    /// surviving values. Errors are user-facing (the tool returns them as a
    /// tool error).
    pub fn parse(args: &JsonValue) -> Result<Self, String> {
        let commands = string_list(args, "commands");
        if commands.is_empty() {
            return Err(
                "ctx_batch: `commands` is required and must contain at least one command"
                    .to_owned(),
            );
        }
        if commands.len() > MAX_COMMANDS {
            return Err(format!(
                "ctx_batch: at most {MAX_COMMANDS} commands are allowed per call (got {})",
                commands.len()
            ));
        }

        let queries = string_list(args, "queries");
        if queries.is_empty() {
            return Err(
                "ctx_batch: `queries` is required and must contain at least one question"
                    .to_owned(),
            );
        }
        if queries.len() > MAX_SEARCH_QUERIES {
            return Err(format!(
                "ctx_batch: at most {MAX_SEARCH_QUERIES} queries are allowed per call (got {})",
                queries.len()
            ));
        }
        if let Some(query) = queries.iter().find(|query| query.len() > MAX_QUERY_BYTES) {
            return Err(format!(
                "ctx_batch: each query must be at most {MAX_QUERY_BYTES} bytes (got {} bytes)",
                query.len()
            ));
        }

        Ok(Self {
            commands,
            queries,
            timeout: resolve_timeout(args),
        })
    }
}

/// Trimmed, non-empty string entries of `key`. A missing key, a non-array
/// value, and non-string items all yield no entries.
fn string_list(args: &JsonValue, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `timeoutMs` wins over `timeout` when positive; both clamp to
/// [`MAX_TIMEOUT_SECS`]; the default applies when neither is usable.
fn resolve_timeout(args: &JsonValue) -> Duration {
    let seconds = args
        .get("timeoutMs")
        .and_then(JsonValue::as_f64)
        .filter(|millis| *millis > 0.0)
        .map(|millis| (millis / 1_000.0).min(MAX_TIMEOUT_SECS))
        .or_else(|| {
            args.get("timeout")
                .and_then(JsonValue::as_f64)
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| seconds.min(MAX_TIMEOUT_SECS))
        })
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs_f64(seconds)
}

/// Stable label: `ctx_batch:<fnv1a(cwd + NUL + commands)>`. Windows paths are
/// lowercased first so two spellings of the same directory share one label.
pub fn batch_source(cwd: &str, commands: &[String]) -> String {
    let mut key = String::with_capacity(cwd.len() + 64);
    let cwd = if cfg!(windows) {
        cwd.to_ascii_lowercase()
    } else {
        cwd.to_owned()
    };
    key.push_str(&cwd);
    for command in commands {
        key.push('\0');
        key.push_str(command);
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("ctx_batch:{hash:016x}")
}

/// Cap a batch reply at [`OUTPUT_CAP`] bytes on a UTF-8 boundary and append
/// the truncation marker.
pub fn cap_reply(reply: String) -> String {
    if reply.len() <= OUTPUT_CAP {
        return reply;
    }
    let mut capped = truncate_utf8(&reply, OUTPUT_CAP).to_owned();
    capped.push_str("\n... [reply truncated at 16 KiB] ...");
    capped
}
