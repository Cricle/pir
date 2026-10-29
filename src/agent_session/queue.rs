use super::*;

impl AgentSession {
    // ------------------------------------------------------------------
    // Queue modes (upstream steeringMode / followUpMode)
    // ------------------------------------------------------------------

    /// The live steering queue mode (upstream `session.steeringMode`).
    pub fn steering_mode(&self) -> SteeringMode {
        *self
            .steering_mode
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The live follow-up queue mode (upstream `session.followUpMode`).
    pub fn follow_up_mode(&self) -> FollowUpMode {
        *self
            .follow_up_mode
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the steering queue mode (upstream `setSteeringMode`). Applies from
    /// the next drain on, including inside a running prompt.
    pub fn set_steering_mode(&self, mode: SteeringMode) {
        *self
            .steering_mode
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    }

    /// Set the follow-up queue mode (upstream `setFollowUpMode`).
    pub fn set_follow_up_mode(&self, mode: FollowUpMode) {
        *self
            .follow_up_mode
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    }

    /// Apply both queue modes (upstream `syncQueueModesFromSettings`, called
    /// at session construction and after a settings reload).
    pub fn apply_queue_modes(&self, steering: SteeringMode, follow_up: FollowUpMode) {
        self.set_steering_mode(steering);
        self.set_follow_up_mode(follow_up);
    }

    /// Drop every queued steering/follow-up message (upstream `clearQueue`)
    /// and report what was dropped (the message texts, the historical wire
    /// shape).
    pub async fn clear_queue(&self) -> ClearedQueue {
        let steering = self.steering_inbox.drain_all().await;
        let follow_up = self.follow_up_inbox.drain_all().await;
        let dropped = steering.len() + follow_up.len();
        if dropped > 0 {
            let _ =
                self.pending_messages
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                        Some(current.saturating_sub(dropped))
                    });
        }
        let steering: Vec<String> = steering
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect();
        let follow_up: Vec<String> = follow_up
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect();
        // Mirror + `queue_update`: the queues emptied (upstream
        // `_emitQueueUpdate` fires from `clearQueue` too).
        let snapshot = {
            let mut mirror = self
                .queue_mirror
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mirror.steering.clear();
            mirror.follow_up.clear();
            (mirror.steering.clone(), mirror.follow_up.clone())
        };
        self.emit_session_event(SessionEvent::QueueUpdate {
            steering: snapshot.0,
            follow_up: snapshot.1,
            delivered_steering: Vec::new(),
            delivered_follow_up: Vec::new(),
        });
        ClearedQueue {
            steering,
            follow_up,
        }
    }
}

/// Account delivered messages against the pending counter
/// (`ctx.hasPendingMessages()` / `pendingMessageCount`).
fn account_delivered(pending: &AtomicUsize, delivered: usize) {
    if delivered > 0 {
        let _ = pending.try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            Some(current.saturating_sub(delivered))
        });
    }
}

/// Drain the session's steering queue per the live `steeringMode` (upstream
/// `PendingMessageQueue.drain`): the mode is read at poll time, so a
/// mid-session change applies to the next turn. After the drain the queue
/// mirror is updated and a `queue_update` event is emitted (upstream
/// `_emitQueueUpdate` fires from the drain sites).
pub(super) fn steering_fn(
    inbox: Inbox,
    pending: Arc<AtomicUsize>,
    mode: Arc<RwLock<SteeringMode>>,
    mirror: Arc<Mutex<QueueMirror>>,
    events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
) -> FollowUpFn {
    Box::new(move || {
        let inbox = inbox.clone();
        let pending = Arc::clone(&pending);
        let mode = Arc::clone(&mode);
        let mirror = Arc::clone(&mirror);
        let events = Arc::clone(&events);
        Box::pin(async move {
            let mode = match *mode
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                SteeringMode::All => QueueMode::All,
                SteeringMode::OneAtATime => QueueMode::OneAtATime,
            };
            let delivered = inbox.drain(mode).await;
            account_delivered(&pending, delivered.len());
            if delivered.is_empty() {
                // No change, no event (upstream _emitQueueUpdate fires on
                // the shift/push/clear sites only).
                return delivered;
            }
            let delivered_texts: Vec<String> = delivered
                .iter()
                .map(|prompt| prompt.text_content())
                .collect();
            let snapshot = {
                let mut mirror = mirror
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                QueueMirror::remove_delivered(&mut mirror.steering, &delivered_texts);
                (mirror.steering.clone(), mirror.follow_up.clone())
            };
            emit_session_event_to(
                &events,
                SessionEvent::QueueUpdate {
                    steering: snapshot.0,
                    follow_up: snapshot.1,
                    delivered_steering: delivered_texts,
                    delivered_follow_up: Vec::new(),
                },
            );
            delivered
        })
    })
}

/// Drain the session's follow-up queue per the live `followUpMode` (see
/// [`steering_fn`] for the `queue_update` semantics).
pub(super) fn follow_up_fn(
    inbox: Inbox,
    pending: Arc<AtomicUsize>,
    mode: Arc<RwLock<FollowUpMode>>,
    mirror: Arc<Mutex<QueueMirror>>,
    events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
) -> FollowUpFn {
    Box::new(move || {
        let inbox = inbox.clone();
        let pending = Arc::clone(&pending);
        let mode = Arc::clone(&mode);
        let mirror = Arc::clone(&mirror);
        let events = Arc::clone(&events);
        Box::pin(async move {
            let mode = match *mode
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                FollowUpMode::All => QueueMode::All,
                FollowUpMode::OneAtATime => QueueMode::OneAtATime,
            };
            let delivered = inbox.drain(mode).await;
            account_delivered(&pending, delivered.len());
            if delivered.is_empty() {
                // No change, no event (upstream _emitQueueUpdate fires on
                // the shift/push/clear sites only).
                return delivered;
            }
            let delivered_texts: Vec<String> = delivered
                .iter()
                .map(|prompt| prompt.text_content())
                .collect();
            let snapshot = {
                let mut mirror = mirror
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                QueueMirror::remove_delivered(&mut mirror.follow_up, &delivered_texts);
                (mirror.steering.clone(), mirror.follow_up.clone())
            };
            emit_session_event_to(
                &events,
                SessionEvent::QueueUpdate {
                    steering: snapshot.0,
                    follow_up: snapshot.1,
                    delivered_steering: Vec::new(),
                    delivered_follow_up: delivered_texts,
                },
            );
            delivered
        })
    })
}

/// What a `clear_queue` call dropped (upstream `clearQueue` returns the
/// drained steering and follow-up message texts).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ClearedQueue {
    pub steering: Vec<String>,
    #[serde(rename = "followUp")]
    pub follow_up: Vec<String>,
}

impl AgentSession {
    /// The storage sink for one queued CUSTOM message
    /// ([`agent_loop::PromptMessage::custom`]): append a custom-message entry
    /// (upstream `appendCustomMessageEntry`) and dispatch `entry_appended`,
    /// mirroring the starting-message sink
    /// (`prompt_loop::AgentSession::preamble_persist_fn`).
    ///
    /// `None` for a store-less (ephemeral) session: the loop keeps the
    /// projected message in the run's produced list, which is the in-memory
    /// transcript's only copy.
    pub(super) fn queued_custom_persist_fn(
        &self,
        custom: CustomRunMessage,
        wire: JsonValue,
    ) -> Option<agent_loop::PreamblePersistFn> {
        let session = self.session.clone()?;
        let plugins = self.plugins.clone();
        let session_events = Arc::clone(&self.session_events);
        let session_event_subscribers = self.has_session_event_subscribers();
        Some(Arc::new(move |_message: Message| {
            let session = Arc::clone(&session);
            let plugins = plugins.clone();
            let session_events = Arc::clone(&session_events);
            let custom = custom.clone();
            let wire = wire.clone();
            Box::pin(async move {
                let appended = tokio::task::spawn_blocking(move || -> crate::Result<String> {
                    let mut session = session.blocking_lock();
                    session.append_custom_message_entry(
                        &custom.custom_type,
                        custom.content,
                        custom.display,
                        custom.details,
                    )
                })
                .await
                .map_err(|error| {
                    anyhow::anyhow!("queued custom-message persistence task failed: {error}")
                })?;
                let entry_id = appended?;
                let entry_json = serde_json::json!({
                    "id": entry_id,
                    "message": wire,
                });
                if let Some(plugins) = &plugins {
                    if plugins.has_handlers("entry_appended") {
                        emit_plugin_event(
                            plugins,
                            "entry_appended",
                            serde_json::json!({
                                "type": "entry_appended",
                                "entry": entry_json.clone(),
                            }),
                        )
                        .await;
                    }
                }
                if session_event_subscribers {
                    emit_session_event_to(
                        &session_events,
                        SessionEvent::EntryAppended { entry: entry_json },
                    );
                }
                Ok(())
            })
        }))
    }
}
