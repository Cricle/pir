use super::*;

use rquickjs::Object;

pub(super) fn register_tool_exec_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
) -> rquickjs::Result<()> {
    macro_rules! sync_fn {
        ($key:literal, $closure:expr) => {
            host.set($key, Func::from($closure))?;
        };
    }

    // -- ctx.executeTool round-trip (nested tool calls) -----------------------
    //
    // The scheduler binds a `NestedToolExecutor` per in-flight tool call and
    // `ExtensionTool::run` registers it here for the call's duration; the JS
    // `ctx.executeTool` resolves against it, so nested calls run through the
    // same gate/scheduler/event path (upstream `ctx.executeTool`).
    //
    // `background` is the prelude's explicit fourth argument
    // (`ctx.executeTool(name, args, { background: true })`): the scheduler's
    // background request travels BESIDE the args, so the arguments the tool
    // receives are exactly the caller's (validated against the tool schema).
    {
        let st = state.clone();
        host.set(
            "executeTool",
            Func::from(JsAsync(
                move |call_id: String, name: String, args_json: String, background: Option<bool>| {
                    let st = st.clone();
                    async move {
                        let executor = {
                            let s = lock_state(&st);
                            // The registry stores `Arc<NestedToolExecutor>`:
                            // this clone is a refcount bump (no allocation)
                            // and the `Arc` is what the future holds across
                            // the `.await` below, so the state lock is never
                            // held across the nested call.
                            s.nested_executors.get(&call_id).cloned()
                        };
                        let Some(executor) = executor else {
                            return r#"{"error":"executeTool is only available inside a running tool call"}"#
                                .to_string();
                        };
                        let args: JsonValue =
                            serde_json::from_str(&args_json).unwrap_or(JsonValue::Null);
                        let options = crate::agent::nested_tool_calls::ExecuteToolOptions {
                            background: background.unwrap_or(false),
                        };
                        let outcome = executor.execute_with_options(&name, args.clone(), options).await;
                        // `json!` lowers every non-literal expression to
                        // `serde_json::to_value(&expr)`, i.e. a DEEP copy — of
                        // values this call owns and drops right after (the
                        // id/name strings, the result text, the `details` /
                        // `structuredContent` trees, and `args`). Build the
                        // maps explicitly so each owned value MOVES in; keys
                        // are inserted in the same order the replaced `json!`
                        // literal used, so with `preserve_order` (the
                        // serde_json feature this crate enables) the emitted
                        // JSON is byte-identical.
                        let mut tool_call = serde_json::Map::new();
                        tool_call.insert("id".into(), JsonValue::String(outcome.tool_call_id));
                        tool_call.insert("name".into(), JsonValue::String(outcome.tool_name));
                        tool_call.insert("arguments".into(), args);
                        let mut content = serde_json::Map::new();
                        content.insert("type".into(), JsonValue::String("text".into()));
                        content.insert("text".into(), JsonValue::String(outcome.result.content));
                        let mut result = serde_json::Map::new();
                        result.insert(
                            "content".into(),
                            JsonValue::Array(vec![JsonValue::Object(content)]),
                        );
                        result.insert(
                            "details".into(),
                            outcome.result.details.unwrap_or(JsonValue::Null),
                        );
                        result.insert(
                            "structuredContent".into(),
                            outcome.result.structured_content.unwrap_or(JsonValue::Null),
                        );
                        let mut envelope = serde_json::Map::new();
                        envelope.insert("toolCall".into(), JsonValue::Object(tool_call));
                        envelope.insert("result".into(), JsonValue::Object(result));
                        envelope.insert("isError".into(), JsonValue::Bool(outcome.is_error));
                        envelope.insert(
                            "durationMs".into(),
                            outcome
                                .duration_ms
                                .map_or(JsonValue::Null, |ms| JsonValue::Number(ms.into())),
                        );
                        serde_json::to_string(&JsonValue::Object(envelope)).unwrap_or_else(|_| {
                            r#"{"error":"failed to serialize the tool outcome"}"#.into()
                        })
                    }
                },
            )),
        )?;
    }

    // -- ctx.ui interactive dialogs (request/reply with the TUI) --------------
    //
    // Headless hosts answer `null` immediately; TUI hosts receive a
    // `Notification::DialogRequest` through the drain and reply via
    // `ExtensionHandle::respond_dialog`. The wait is bounded so an absent UI
    // cannot wedge the JS runtime.
    {
        let st = state.clone();
        host.set(
            "uiDialog",
            Func::from(JsAsync(move |kind: String, payload_json: String| {
                let st = st.clone();
                async move {
                    let has_ui = { lock_state(&st).config.has_ui };
                    if !has_ui {
                        return "null".to_string();
                    }
                    let options: JsonValue =
                        serde_json::from_str(&payload_json).unwrap_or(JsonValue::Null);
                    // Per-dialog timeout (upstream ExtensionUIDialogOptions
                    // `timeout`, types.ts:117-118): a request-level `timeout`
                    // (ms) overrides the runtime's global dialog TTL for THIS
                    // dialog only (same resolve-null-on-timeout semantics).
                    let request_ttl = options
                        .get("timeout")
                        .and_then(JsonValue::as_f64)
                        .filter(|ms| ms.is_finite() && *ms > 0.0)
                        // `Instant + Duration` panics on overflow; the clamp
                        // keeps absurd JS timeouts from aborting the host.
                        .map(|ms| std::time::Duration::from_millis(ms.min(u32::MAX as f64) as u64));
                    let id = {
                        let mut s = lock_state(&st);
                        s.next_dialog_id += 1;
                        let id = s.next_dialog_id;
                        let title = options
                            .get("title")
                            .and_then(JsonValue::as_str)
                            .map(String::from);
                        s.push(Notification::DialogRequest {
                            id,
                            // `kind` is owned by this future and unused after
                            // this push; move it instead of cloning.
                            kind,
                            title,
                            options,
                        });
                        id
                    };
                    // One `String` key reused by the registration and every
                    // re-check below: the four `id.to_string()` calls it
                    // replaces were a fresh heap allocation each (and the
                    // reply/timeout checks re-allocated once per poll).
                    let id_key = id.to_string();
                    // P1-3: per-dialog Notify — this waiter registers its own
                    // entry before checking, so a reply inserted afterwards
                    // wakes exactly this waiter (a permit is stored even if
                    // the reply lands before the first poll). The shared
                    // Notify it replaced could wake the WRONG waiter, whose
                    // re-check consumed the permit and stalled this dialog's
                    // await for up to the timeout.
                    let (ttl, notify) = {
                        let mut s = lock_state(&st);
                        let ttl = request_ttl.unwrap_or(s.dialog_ttl);
                        s.sweep_expired_dialog_state();
                        let notify = s
                            .dialog_notifies
                            .entry(id_key.clone())
                            .or_insert_with(|| {
                                (
                                    std::sync::Arc::new(tokio::sync::Notify::new()),
                                    std::time::Instant::now(),
                                    ttl,
                                )
                            })
                            .0
                            .clone();
                        (ttl, notify)
                    };
                    let deadline = std::time::Instant::now() + ttl;
                    loop {
                        {
                            let mut s = lock_state(&st);
                            if let Some((value, _, _)) = s.dialog_responses.remove(&id_key) {
                                // Also drop this waiter's registration (it
                                // may have been (re)created after the reply
                                // landed) so nothing accumulates (P2-9).
                                s.dialog_notifies.remove(&id_key);
                                return serde_json::to_string(&value)
                                    .unwrap_or_else(|_| "null".into());
                            }
                        }
                        let now = std::time::Instant::now();
                        if now >= deadline {
                            // Timed out: drop this waiter's registration so
                            // it cannot accumulate (P2-9).
                            lock_state(&st).dialog_notifies.remove(&id_key);
                            return "null".to_string();
                        }
                        let _ = tokio::time::timeout(deadline - now, notify.notified()).await;
                    }
                }
            })),
        )?;
    }

    // -- ctx command-context session actions (print/rpc command surface) -----
    //
    // Upstream `ExtensionCommandContextActions` (extensions/types.ts:2208-2228;
    // print-mode.ts:78-100 / rpc-mode.ts:322-344 bind real implementations):
    // `ctx.newSession` / `ctx.fork` / `ctx.navigateTree` / `ctx.switchSession`
    // / `ctx.reload`. The JS prelude forwards each call here; the parsed
    // action is recorded on the crate-level queue
    // (`crate::agent_session::record_session_command_action`) and the session
    // host applies it against its session-control plane after the command
    // handler settles (`crate::agent_session::apply_session_command_to_storage`
    // for the print host, `AgentSession::apply_session_command_actions` for
    // AgentSession-backed hosts). The synchronous host fn answers with the
    // upstream runner-DEFAULT result (`{cancelled: false}`, accepted);
    // application failures surface on the host side, and invalid requests are
    // rejected here as `{error}` (the prelude throws them).
    {
        let st = state.clone();
        host.set(
            "commandAction",
            Func::from(JsAsync(move |name: String, payload_json: String| {
                let st = st.clone();
                async move {
                    // Stamp each recorded action with THIS runtime's host
                    // identity (mode + pushed session id) so the owning
                    // session host — and only it — claims the action later.
                    let origin = {
                        let state = lock_state(&st);
                        crate::agent_session::SessionOrigin::for_runtime_mode(
                            &state.config.mode,
                            state.session.session_id.clone(),
                        )
                    };
                    // Residual-origin guards (record time — the bounded queue
                    // must only carry actions SOME host will claim):
                    // - a storage-less AgentSession host (`session_id: None`,
                    //   e.g. `--no-session`) REJECTS the action outright,
                    //   following the session-control plane's "no attached
                    //   session storage" precedent — recording under a `None`
                    //   identity would let ANY storage-less host cross-claim
                    //   it;
                    // - a subagent child runtime (`session_id` =
                    //   `subagent-<n>`) has NO host that ever claims its
                    //   actions (the child is ephemeral), so they are dropped
                    //   here — otherwise they crowd the cap-64 queue and
                    //   evict a real host's pending actions.
                    match &origin {
                        crate::agent_session::SessionOrigin::AgentSession { session_id: None } => {
                            return r#"{"error":"no attached session storage"}"#.to_string();
                        }
                        crate::agent_session::SessionOrigin::AgentSession {
                            session_id: Some(session_id),
                        } if session_id.starts_with("subagent-") => {
                            return r#"{"cancelled":false}"#.to_string();
                        }
                        _ => {}
                    };
                    let payload: JsonValue =
                        serde_json::from_str(&payload_json).unwrap_or(JsonValue::Null);
                    // The caller's `withSession` callback token (prelude
                    // `registerSessionCallback`). Absent when the caller did
                    // not pass one; a non-number is a malformed bridge payload
                    // and is rejected rather than silently dropping the
                    // callback.
                    let with_session = match payload.get("withSession") {
                        None | Some(JsonValue::Null) => None,
                        Some(value) => match value.as_u64() {
                            Some(token) => Some(token),
                            None => {
                                return format!(
                                    "{{\"error\":\"{name}: withSession must be a callback token\"}}"
                                )
                            }
                        },
                    };
                    let action = match name.as_str() {
                        "newSession" => crate::agent_session::SessionCommandAction::NewSession {
                            parent_session: payload
                                .get("parentSession")
                                .and_then(JsonValue::as_str)
                                .map(String::from),
                            with_session,
                        },
                        "fork" => crate::agent_session::SessionCommandAction::Fork {
                            entry_id: payload
                                .get("entryId")
                                .and_then(JsonValue::as_str)
                                .map(String::from),
                            position: payload
                                .get("position")
                                .and_then(JsonValue::as_str)
                                .map(String::from),
                            with_session,
                        },
                        "navigateTree" => {
                            let Some(target_id) = payload
                                .get("targetId")
                                .and_then(JsonValue::as_str)
                                .map(String::from)
                            else {
                                return r#"{"error":"navigateTree requires a target entry id"}"#
                                    .to_string();
                            };
                            if with_session.is_some() {
                                return r#"{"error":"navigateTree does not take a withSession callback"}"#
                                    .to_string();
                            }
                            crate::agent_session::SessionCommandAction::NavigateTree {
                                target_id,
                                summarize: payload.get("summarize").and_then(JsonValue::as_bool)
                                    == Some(true),
                                custom_instructions: payload
                                    .get("customInstructions")
                                    .and_then(JsonValue::as_str)
                                    .map(String::from),
                                replace_instructions: payload
                                    .get("replaceInstructions")
                                    .and_then(JsonValue::as_bool)
                                    == Some(true),
                                label: payload
                                    .get("label")
                                    .and_then(JsonValue::as_str)
                                    .map(String::from),
                            }
                        }
                        "switchSession" => {
                            let Some(session_path) = payload
                                .get("sessionPath")
                                .and_then(JsonValue::as_str)
                                .map(String::from)
                            else {
                                return r#"{"error":"switchSession requires a session path"}"#
                                    .to_string();
                            };
                            crate::agent_session::SessionCommandAction::SwitchSession {
                                session_path,
                                with_session,
                            }
                        }
                        "reload" => {
                            if with_session.is_some() {
                                return r#"{"error":"reload does not take a withSession callback"}"#
                                    .to_string();
                            }
                            crate::agent_session::SessionCommandAction::Reload
                        }
                        other => {
                            return format!(
                                "{{\"error\":\"unknown command-context action: {other}\"}}"
                            );
                        }
                    };
                    crate::agent_session::record_session_command_action(origin, action);
                    r#"{"cancelled":false}"#.to_string()
                }
            })),
        )?;
    }

    // Async exec (host-backed promise).
    //
    // Mirrors upstream `pi.exec` (`loader.ts:411-414`): the command runs in
    // the EXTENSION's cwd unless `options.cwd` overrides it (upstream
    // `options?.cwd ?? cwd` at loader.ts:413), stdin is not inherited
    // (`exec.ts:44`), and a caller-provided `AbortSignal` kills the child
    // (`exec.ts:52-72`). The prelude relays the signal's `abort` event here as
    // `execAbort(<call id>)`; the per-call handle is stored for the duration
    // of the call so an abort racing ahead of the worker still lands.
    let exec_aborts: Arc<StdMutex<HashMap<String, ExecAbort>>> =
        Arc::new(StdMutex::new(HashMap::new()));
    {
        let exec_aborts = exec_aborts.clone();
        sync_fn!("execAbort", move |call_id: String| -> () {
            let mut registry = lock_state(&exec_aborts);
            match registry.get(&call_id) {
                Some(handle) => handle.trigger(),
                None => {
                    // The abort raced ahead of the exec future registering:
                    // store a triggered placeholder; `exec` consumes it, and
                    // `execAbortDone` clears it if the call never registers.
                    let handle = ExecAbort::new(None);
                    handle.trigger();
                    registry.insert(call_id, handle);
                }
            }
        });
    }
    {
        let exec_aborts = exec_aborts.clone();
        sync_fn!("execAbortDone", move |call_id: String| -> () {
            lock_state(&exec_aborts).remove(&call_id);
        });
    }
    {
        let st = state.clone();
        let exec_aborts = exec_aborts.clone();
        host.set(
            "exec",
            Func::from(JsAsync(
                move |command: String,
                      args_json: String,
                      cwd: Option<String>,
                      timeout_ms: Option<f64>,
                      abort_id: Option<String>| {
                    let st = st.clone();
                    let exec_aborts = exec_aborts.clone();
                    async move {
                        let args: Vec<String> =
                            serde_json::from_str(&args_json).unwrap_or_default();
                        // Default cwd: the extension's cwd (upstream
                        // loader.ts:413 `options?.cwd ?? cwd`); a provided
                        // `options.cwd` is passed through unchanged, resolved
                        // relative to the host process cwd by `spawn` exactly
                        // like upstream.
                        let cwd = match cwd {
                            Some(cwd) => Some(cwd),
                            None => Some(lock_state(&st).config.cwd.display().to_string()),
                        };
                        // The prelude mints an abort id only when a signal was
                        // provided (upstream `exec.ts:66`).
                        let abort = abort_id.map(|id| {
                            let shared_st = st.clone();
                            let handle = {
                                let mut registry = lock_state(&exec_aborts);
                                match registry.remove(&id) {
                                    // An abort already fired before this
                                    // future registered (pre-aborted signal):
                                    // keep the triggered placeholder.
                                    Some(pending) => pending,
                                    None => ExecAbort::new(Some(Arc::new(move || {
                                        lock_state(&shared_st).tool_call_aborted
                                    }))),
                                }
                            };
                            lock_state(&exec_aborts).insert(id.clone(), handle.clone());
                            (id, handle)
                        });
                        let result = run_exec(
                            &command,
                            &args,
                            cwd.as_deref(),
                            timeout_ms,
                            abort.as_ref().map(|(_, handle)| handle.clone()),
                        )
                        .await;
                        if let Some((id, _)) = &abort {
                            lock_state(&exec_aborts).remove(id);
                        }
                        serde_json::to_string(&result).unwrap_or_else(|_| {
                            r#"{"stdout":"","stderr":"","code":1,"killed":false}"#.into()
                        })
                    }
                },
            )),
        )?;
    }

    Ok(())
}
