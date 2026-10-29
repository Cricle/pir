use super::*;

use rquickjs::Object;

/// The `@earendil-works/pi-codemode` shim's host bridge (DRIFT §2.3 item 2 /
/// D28): an extension's `CodemodeSandboxBase.execute()` / `start()` runs on
/// the host codemode sandbox (`crate::plugins::codemode` — the same engine
/// the builtin `codemode` tool uses), and the JS side answers the script's
/// calls through this pull protocol:
///
/// - `codemodeStart(payloadJson)` → `{"id":"N"}` (or `{"error":"..."}`):
///   spawns the run and returns its session id immediately.
/// - `codemodePoll(runId)` → `Promise<eventJson>`: the next event —
///   `started` / `call` / `idle` / `done` / `error`. The host owns the run,
///   the JS side drains events (the `__pir_host.worker` + `workerRecvPoll`
///   shape, and upstream's `host ↔ VM` message exchange with the extension
///   actor in the VM's place).
/// - `codemodeDeliver(runId, callId, payloadJson)`: resolve the call the JS
///   side just executed (`CodemodeRun.deliver` / a declared tool's result).
/// - `codemodeAbort(runId)`: interrupt the VM and fail its pending calls
///   (`close()`, an aborted signal).
/// - `codemodeDetach(runId)`: the JS promise chain died (an exception in the
///   poll loop); fail the pending calls instead of leaving the sandbox thread
///   waiting forever.
///
/// The session ids are process-global (the run registry lives in
/// `crate::plugins::codemode`), so two runtimes in one process never share
/// one; a run is removed from the registry once its `done` event is polled.
pub(super) fn register_codemode_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
) -> rquickjs::Result<()> {
    use crate::plugins::codemode::{
        abort_extension_run, deliver_extension_result, deliver_extension_result_value,
        detach_extension_run, next_extension_event, start_extension_run,
    };

    // -- start: spawn the run, hand back its session id ----------------------
    {
        let st = state.clone();
        host.set(
            "codemodeStart",
            Func::from(move |payload_json: String| -> String {
                let executor = resolve_session_executor(&st);
                start_extension_run(&payload_json, executor)
            }),
        )?;
    }

    // -- poll: the next event of a running session ---------------------------
    //
    // The await is the JS side's back-pressure: it is only ever resolved by an
    // event (a call the script made, an idle acknowledge, or the outcome), so
    // a script that computes for a while simply keeps this pending — exactly
    // the host fn pattern `ctx.ui.*` uses for its dialog wait.
    host.set(
        "codemodePoll",
        Func::from(JsAsync(move |run_id: String| async move {
            let run_id = parse_run_id(&run_id).unwrap_or(u64::MAX);
            next_extension_event(run_id).await
        })),
    )?;

    // -- deliver: answer one call -------------------------------------------
    host.set(
        "codemodeDeliver",
        Func::from(
            move |run_id: String, call_id: String, payload_json: String| -> String {
                let Some(run_id) = parse_run_id(&run_id) else {
                    return r#"{"error":"invalid codemode run id"}"#.to_owned();
                };
                let Some(call_id) = parse_run_id(&call_id) else {
                    return r#"{"error":"invalid codemode call id"}"#.to_owned();
                };
                deliver_extension_result(run_id, call_id, &payload_json)
            },
        ),
    )?;

    // -- session call: run one call against the host session's tool set ------
    //
    // The sandbox referred a tool name it did not declare to the running
    // session's tools (the builtin `codemode` tool's model). The nested-call
    // pipeline runs INSIDE this host function's future — the same task context
    // `ctx.executeTool` uses — so its `tool_call`/`tool_result` event dispatch
    // interleaves with the script's suspended await; the tool's outcome is
    // delivered to the waiting call before this resolves.
    {
        let st = state.clone();
        host.set(
            "codemodeSessionCall",
            Func::from(JsAsync(
                move |run_id: String, call_id: String, name: String, args_json: String| {
                    let st = st.clone();
                    async move {
                        let (Some(run_id), Some(call_id)) =
                            (parse_run_id(&run_id), parse_run_id(&call_id))
                        else {
                            return r#"{"error":"invalid codemode run or call id"}"#.to_owned();
                        };
                        let executor = resolve_session_executor(&st);
                        // The delivery takes the owned payload `Value` (both
                        // arms build one; the session tier already returns
                        // one), so nothing is folded to text for the callee to
                        // parse back. The `Value`-taking entry point moves the
                        // message out of the envelope map directly.
                        let payload = match executor {
                            Some(executor) => {
                                crate::plugins::codemode::session_call_payload(
                                    executor, name, args_json,
                                )
                                .await
                            }
                            None => {
                                let message = format!("Unknown tool \"{name}\"");
                                serde_json::json!({ "ok": false, "message": message })
                            }
                        };
                        deliver_extension_result_value(run_id, call_id, payload)
                    }
                },
            )),
        )?;
    }

    // -- abort / detach: stop the run ---------------------------------------
    host.set(
        "codemodeAbort",
        Func::from(move |run_id: String| -> String {
            parse_run_id(&run_id)
                .map(abort_extension_run)
                .unwrap_or_else(|| r#"{"error":"invalid codemode run id"}"#.to_owned())
        }),
    )?;
    host.set(
        "codemodeDetach",
        Func::from(move |run_id: String| -> String {
            parse_run_id(&run_id)
                .map(detach_extension_run)
                .unwrap_or_else(|| r#"{"error":"invalid codemode run id"}"#.to_owned())
        }),
    )?;

    Ok(())
}

/// The run ids are small monotonic counters; the JS side passes them as
/// strings so a session can never be addressed with a rounded number.
fn parse_run_id(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

/// The host session's in-flight nested-call executor, the fallback for tool
/// names an extension sandbox did not declare (the builtin `codemode` tool's
/// "the `tools` proxy resolves whatever the registry carries at execution
/// time" model).
///
/// The runtime binds one executor per in-flight extension tool call
/// ([`crate::agent::tool_execution::ExtensionTool`]); the shim cannot know
/// which call it runs under (the call id is a prelude-local of
/// `__pir_callTool`), so:
/// - exactly one bound executor → use it (the sandbox was created inside that
///   call, the only shape the extension API offers today);
/// - none or several → no fallback, and an undeclared tool name fails closed
///   with upstream's `Unknown tool "..."` rather than calling an arbitrary
///   call's tool set.
fn resolve_session_executor(
    state: &Arc<StdMutex<RuntimeState>>,
) -> Option<Arc<crate::agent::nested_tool_calls::NestedToolExecutor>> {
    let state = lock_state(state);
    let mut executors = state.nested_executors.values();
    let first = executors.next()?;
    if executors.next().is_some() {
        return None;
    }
    // The registry already holds `Arc<NestedToolExecutor>`, and every consumer
    // (`start_extension_run` / `session_call_payload` in
    // `crate::plugins::codemode`) takes that `Arc` too, so the fallback is a
    // refcount bump: no executor value clone per sandbox start / session call.
    Some(Arc::clone(first))
}
