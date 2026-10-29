//! Execution of custom `BashOperations` returned by a `user_bash` handler
//! (upstream `UserBashEventResult.operations`, extensions/types.ts:1466-1476;
//! `BashOperations` shape `core/tools/bash.ts:75-87`).
//!
//! The handler's `exec(command, cwd, {onData, signal, timeout, env})` closure
//! cannot cross the JSON host boundary, so the prelude keeps it in QuickJS
//! state and exports it under a token
//! (`{ operations: { "__pirBashOperations": <token> } }`). This module is the
//! host-side execution path shared by every `!`-command surface:
//!
//! 1. register the chunk sink for the token (the sync
//!    `__pir_host.bashOperationData` host fn appends here);
//! 2. call `__pir_runBashOperations(token, {command, cwd, ...})`, which awaits
//!    the handler's `exec` and resolves `{exitCode}`;
//! 3. return the streamed output plus the exit code (or the error) to the
//!    caller, which records it like an ordinary bash result.
//!
//! Upstream never falls back to local execution when a handler supplied
//! operations (interactive-mode/rpc-mode both pass them straight into
//! `executeBash`), and neither does this path: an unusable operations object
//! is reported as a failed command, not run locally.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value as JsonValue;

use super::ExtensionHandle;

/// Wire key the prelude puts on an exported `{operations}` object.
pub const OPERATIONS_TOKEN_KEY: &str = "__pirBashOperations";

/// Live chunk sinks, keyed by the exported operations token. The sync host fn
/// runs on the extension actor thread; the session-side executor registers and
/// removes the entry around the call. Deliberately a small process-wide map
/// (not runtime state): the host fn is a plain closure that must not re-enter
/// the actor or take the runtime-state lock while a JS turn is suspended.
type ChunkSink = Arc<dyn Fn(&str) + Send + Sync>;

fn sinks() -> &'static Mutex<HashMap<String, ChunkSink>> {
    static SINKS: OnceLock<Mutex<HashMap<String, ChunkSink>>> = OnceLock::new();
    SINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Deliver one output chunk to the token's registered sink. Unknown tokens
/// (a released/aborted command) drop the chunk.
pub fn deliver_chunk(token: &str, data: &str) {
    let sink = sinks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(token)
        .cloned();
    if let Some(sink) = sink {
        sink(data);
    }
}

/// Registers a sink outside the JS path. Test-only seam used by
/// `tests/suites/user_bash_sink.rs`; the returned guard unregisters on drop.
#[doc(hidden)]
pub fn register_sink_for_test(token: &str, sink: Arc<dyn Fn(&str) + Send + Sync>) -> impl Drop {
    sinks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(token.to_owned(), sink);
    SinkRegistration {
        token: token.to_owned(),
    }
}

/// Removes the sink when the execution ends (normal, error or future drop).
struct SinkRegistration {
    token: String,
}

impl Drop for SinkRegistration {
    fn drop(&mut self) {
        sinks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.token);
    }
}

/// Outcome of executing one exported `BashOperations.exec`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BashOperationsResult {
    /// Combined output streamed through `onData`.
    pub output: String,
    /// Process exit code; `None` when the callback resolved `null` or failed.
    pub exit_code: Option<i32>,
    /// Whether the command was aborted.
    pub cancelled: bool,
    /// The `exec` rejection / dispatch failure, when there was one.
    pub error: Option<String>,
}

/// Execute `operations` for `command` in `cwd`. `on_chunk` receives every
/// streamed output chunk (the caller emits the live `bash_execution_update`
/// event and any host callback).
///
/// A `{operations}` object without the prelude's token (for example a native
/// Rust plugin's serializable operations) cannot be executed by this host;
/// the result reports that honestly instead of falling back to a local shell.
pub async fn execute(
    handle: &ExtensionHandle,
    operations: &JsonValue,
    command: &str,
    cwd: &Path,
    on_chunk: Arc<dyn Fn(&str) + Send + Sync>,
) -> BashOperationsResult {
    let Some(token) = operations
        .get(OPERATIONS_TOKEN_KEY)
        .and_then(JsonValue::as_str)
    else {
        return BashOperationsResult {
            error: Some(
                "the user_bash handler supplied custom bash operations this host cannot \
execute (the operations object carries no JS exec callback; upstream never falls back to \
local execution)"
                    .to_owned(),
            ),
            ..BashOperationsResult::default()
        };
    };
    let token = token.to_owned();
    let output = Arc::new(Mutex::new(String::new()));
    {
        let output = Arc::clone(&output);
        sinks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                token.clone(),
                Arc::new(move |data: &str| {
                    output
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_str(data);
                    on_chunk(data);
                }),
            );
    }
    let _registration = SinkRegistration {
        token: token.clone(),
    };
    let request = serde_json::json!({
        "command": command,
        "cwd": cwd.to_string_lossy(),
    });
    let reply = handle.call_bash_operations(&token, &request).await;
    let collected = std::mem::take(
        &mut *output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    match reply {
        Ok(reply) => BashOperationsResult {
            output: collected,
            exit_code: reply
                .get("exitCode")
                .and_then(JsonValue::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
            cancelled: false,
            error: None,
        },
        Err(error) => {
            let cancelled = error.contains("abort")
                || error.contains("interrupt")
                || error.contains("cancelled");
            BashOperationsResult {
                output: collected,
                exit_code: None,
                cancelled,
                error: Some(error),
            }
        }
    }
}
