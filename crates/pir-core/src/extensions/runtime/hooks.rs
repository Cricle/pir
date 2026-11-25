use super::*;

impl ExtensionRuntime {
    /// Incremental streaming variant of the reserved-prefix hook dispatch:
    /// run the `__pir_stream:` `streamSimple` callback and forward each
    /// produced event through `events` (upstream returns a live
    /// `AssistantMessageEventStream`; pir pulls it event-by-event across the
    /// QuickJS boundary).
    ///
    /// Protocol: one bounded `__pir_startProviderStream` round trip (the
    /// legacy batch forms — an event array or `{events: [...]}` — arrive
    /// whole in that reply), then one `__pir_streamNext` pull per event for
    /// the incremental async-iterable form. The configured hook timeout
    /// (`extensionLimits.hookTimeoutMs` / `PI_EXT_HOOK_TIMEOUT_MS`; default
    /// [`HOOK_CALL_TIMEOUT`]) applies per round trip, so in incremental mode
    /// it is the maximum gap between two events (the batch form keeps the
    /// single long window it always had; upstream itself enforces no
    /// timeout). Every failure path — JS
    /// throw, deadline, malformed reply — terminates the channel with a
    /// [`HookStreamItem::Failed`] (fail-closed); none panics or wedges the
    /// actor. A dropped `events` channel (consumer gone or aborted) stops
    /// the pull loop and asks the JS side to close the session
    /// (async-generator `return()`).
    pub async fn call_hook_stream(
        &self,
        hook_key: &str,
        payload: &JsonValue,
        events: tokio::sync::mpsc::Sender<HookStreamItem>,
    ) {
        use crate::ai::providers::JS_STREAM_HOOK_PREFIX;
        if !hook_key.starts_with(JS_STREAM_HOOK_PREFIX) {
            let _ = events
                .send(HookStreamItem::Failed(format!(
                    "streaming hook dispatch only supports `{JS_STREAM_HOOK_PREFIX}` keys"
                )))
                .await;
            return;
        }
        let payload_json = match serde_json::to_string(payload) {
            Ok(payload_json) => payload_json,
            Err(error) => {
                let _ = events
                    .send(HookStreamItem::Failed(format!(
                        "streamSimple payload serialization failed: {error}"
                    )))
                    .await;
                return;
            }
        };

        // Start round trip: legacy batch replies arrive whole here.
        let timeout = self.hook_timeout();
        let token = self.arm_deadline_for(timeout);
        let result = tokio::time::timeout(
            timeout,
            self.invoke_str2(
                "__pir_startProviderStream",
                hook_key.to_string(),
                payload_json,
            ),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            // Disarm after the recovery drain (P1-4): recovered
            // continuations still run under an armed deadline.
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(token);
        let mut start = match result {
            Ok(Ok(start_json)) => match serde_json::from_str::<JsonValue>(&start_json) {
                Ok(start) => start,
                Err(error) => {
                    let _ = events
                        .send(HookStreamItem::Failed(format!(
                            "streamSimple hook returned an invalid start reply: {error}"
                        )))
                        .await;
                    return;
                }
            },
            Ok(Err(error)) => {
                let _ = events.send(HookStreamItem::Failed(error.to_string())).await;
                return;
            }
            Err(_elapsed) => {
                let _ = events
                    .send(HookStreamItem::Failed(format!(
                        "streamSimple hook `{hook_key}` timed out"
                    )))
                    .await;
                return;
            }
        };
        if let Some(error) = start.get("error").and_then(JsonValue::as_str) {
            let _ = events.send(HookStreamItem::Failed(error.to_owned())).await;
            return;
        }
        match start.get("mode").and_then(JsonValue::as_str) {
            Some("batch") => {
                // Take the batch out of the (owned) start reply instead of
                // cloning each event: a `streamSimple` batch carries the
                // provider's message events (text/thinking/tool-call blocks),
                // and the reply is discarded right after. `mem::take` leaves
                // an empty array behind but the reply has no other reader.
                let Some(batch) = start
                    .get_mut("events")
                    .and_then(JsonValue::as_array_mut)
                    .map(std::mem::take)
                else {
                    let _ = events
                        .send(HookStreamItem::Failed(
                            "streamSimple hook returned an invalid batch reply (missing events array)"
                                .to_owned(),
                        ))
                        .await;
                    return;
                };
                for event in batch {
                    if events.send(HookStreamItem::Event(event)).await.is_err() {
                        // Consumer dropped; nothing more to deliver.
                        return;
                    }
                }
                let _ = events.send(HookStreamItem::End).await;
            }
            Some("stream") => {
                let Some(session_id) = start
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
                else {
                    let _ = events
                        .send(HookStreamItem::Failed(
                            "streamSimple hook returned an invalid stream reply (missing session id)"
                                .to_owned(),
                        ))
                        .await;
                    return;
                };
                loop {
                    let timeout = self.hook_timeout();
                    let token = self.arm_deadline_for(timeout);
                    let result = tokio::time::timeout(
                        timeout,
                        self.invoke_str1("__pir_streamNext", session_id.clone()),
                    )
                    .await;
                    if !matches!(result, Ok(Ok(_))) {
                        self.recover_after_interrupt().await;
                    }
                    self.disarm_deadline(token);
                    // Owned reply binding: the `event` member is MOVED out
                    // below (`Value::take`) instead of deep-cloned. This is the
                    // per-delta path — a provider streams one event per token
                    // chunk — and each event embeds the accumulated message
                    // block, so the clone was O(reply) per delta.
                    let mut next = match result {
                        Ok(Ok(next_json)) => match serde_json::from_str::<JsonValue>(&next_json) {
                            Ok(next) => next,
                            Err(error) => {
                                let _ = events
                                    .send(HookStreamItem::Failed(format!(
                                        "streamSimple hook returned an invalid next reply: {error}"
                                    )))
                                    .await;
                                self.cancel_stream_session(&session_id).await;
                                return;
                            }
                        },
                        Ok(Err(error)) => {
                            let _ = events.send(HookStreamItem::Failed(error.to_string())).await;
                            self.cancel_stream_session(&session_id).await;
                            return;
                        }
                        Err(_elapsed) => {
                            let _ = events
                                .send(HookStreamItem::Failed(format!(
                                    "streamSimple hook `{hook_key}` timed out between events"
                                )))
                                .await;
                            self.cancel_stream_session(&session_id).await;
                            return;
                        }
                    };
                    if let Some(error) = next.get("error").and_then(JsonValue::as_str) {
                        let _ = events.send(HookStreamItem::Failed(error.to_owned())).await;
                        return;
                    }
                    if next.get("aborted").and_then(JsonValue::as_bool) == Some(true) {
                        // The runtime abort flag fired (host abort): surface a
                        // terminal failure; the consumer maps it to the typed
                        // aborted error event.
                        let _ = events
                            .send(HookStreamItem::Failed("stream aborted".to_owned()))
                            .await;
                        return;
                    }
                    if next.get("done").and_then(JsonValue::as_bool) == Some(true) {
                        let _ = events.send(HookStreamItem::End).await;
                        return;
                    }
                    match next.get_mut("event").map(JsonValue::take) {
                        Some(event) => {
                            if events.send(HookStreamItem::Event(event)).await.is_err() {
                                // Consumer dropped (abort or teardown): close
                                // the JS session so its generator can run its
                                // finally blocks.
                                self.cancel_stream_session(&session_id).await;
                                return;
                            }
                        }
                        None => {
                            let _ = events
                                .send(HookStreamItem::Failed(
                                    "streamSimple hook returned an invalid next reply".to_owned(),
                                ))
                                .await;
                            self.cancel_stream_session(&session_id).await;
                            return;
                        }
                    }
                }
            }
            _ => {
                let _ = events
                    .send(HookStreamItem::Failed(
                        "streamSimple hook returned an invalid start reply".to_owned(),
                    ))
                    .await;
            }
        }
    }

    /// Execute one exported `user_bash` custom `BashOperations.exec`
    /// (upstream `UserBashEventResult.operations`,
    /// extensions/types.ts:1466-1476): the `exec` closure stays inside
    /// QuickJS (`__pir_runBashOperations`; the prelude exports the handler's
    /// `exec` closure under a token), receives `(command, cwd,
    /// {onData, signal, timeout, env})`
    /// and resolves `{exitCode}`. Output chunks reach the host through the
    /// synchronous `__pir_host.bashOperationData` sink while the call runs.
    ///
    /// The long hook budget applies (`HOOK_CALL_TIMEOUT`): a remote
    /// transport may legitimately run for many minutes.
    pub async fn call_bash_operations(
        &self,
        token: &str,
        request: &JsonValue,
    ) -> Result<JsonValue, String> {
        let request_json = serde_json::to_string(request)
            .map_err(|error| format!("bash operation request serialization failed: {error}"))?;
        let timeout = self.hook_timeout();
        let deadline = self.arm_deadline_for(timeout);
        let result = tokio::time::timeout(
            timeout,
            self.invoke_str2("__pir_runBashOperations", token.to_string(), request_json),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            // Recovered continuations still run under an armed deadline.
            self.recover_after_interrupt().await;
        }
        self.disarm_deadline(deadline);
        let reply_json = match result {
            Ok(Ok(reply)) => reply,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_elapsed) => return Err("custom bash operations timed out".to_owned()),
        };
        let parsed: JsonValue = serde_json::from_str(&reply_json).map_err(|error| {
            format!("custom bash operations returned an invalid reply: {error}")
        })?;
        if let Some(error) = parsed.get("error").and_then(JsonValue::as_str) {
            return Err(error.to_owned());
        }
        Ok(parsed)
    }

    /// Best-effort close of one incremental streamSimple session: the JS side
    /// drops the session and calls `return()` on the iterator (async
    /// generators run their `finally` blocks).
    async fn cancel_stream_session(&self, session_id: &str) {
        let _ = self
            .invoke_str1("__pir_streamCancel", session_id.to_string())
            .await;
    }
}
