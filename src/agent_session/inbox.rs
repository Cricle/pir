use super::*;

/// Depth of the queued-text backlog past which the inbox reports that nobody
/// is draining it.
///
/// The queue is deliberately UNBOUNDED, matching upstream's
/// `PendingMessageQueue`: every entry is explicit user input (a steering
/// message typed during a run, or a follow-up queued for after it), and
/// dropping or blocking a user's message to save memory would be the wrong
/// trade — upstream queues them without limit too, and the loop drains the
/// queue on every turn (`one-at-a-time` or `all`). What the inbox must not do
/// is grow silently, so a backlog past this water mark is reported
/// ([`INBOX_REPORT_EVERY`] cadence): 1024 queued messages is far past any
/// interactive session and means the drain loop has stopped running.
const INBOX_WARN_DEPTH: usize = 1024;

/// Diagnostic cadence: the first crossing is reported, then one line per this
/// many further crossings.
const INBOX_REPORT_EVERY: usize = 1024;

/// Steering/follow-up inbox (upstream `PendingMessageQueue`): ONE unbounded
/// command channel served by ONE consumer task that owns the queued messages.
/// Messages queued with [`AgentSession::steer`] / [`AgentSession::follow_up`]
/// (plain text) or [`AgentSession::steer_message`] /
/// [`AgentSession::follow_up_message`] (text plus images) or their custom
/// variants carry their model-facing message plus (for a custom message) the
/// custom wire shape and storage sink ([`agent_loop::PromptMessage`]) before
/// or during a run are unbounded. Senders clone the channel side freely; the
/// drains (the loop's per-turn poll, [`AgentSession::clear_queue`]) and the
/// peek getters ride as [`InboxCommand`]s, so the receiver is owned
/// exclusively by the consumer task and never lock-guarded across a poll.
pub struct Inbox {
    /// Command channel (enqueues plus drain/peek requests); clones freely.
    commands: mpsc::UnboundedSender<InboxCommand>,
    /// Handoff slot parking the single receiver until the first in-runtime
    /// interaction moves it into the consumer task (the facade can be built
    /// outside a tokio runtime; commands buffer in the unbounded channel
    /// until then). Emptied exactly once; never held across a poll.
    consumer: Arc<Mutex<Option<mpsc::UnboundedReceiver<InboxCommand>>>>,
}

impl Clone for Inbox {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            // Clones share the same consumer task and its handoff slot.
            consumer: Arc::clone(&self.consumer),
        }
    }
}

/// One request to an [`Inbox`]'s consumer task (upstream
/// `PendingMessageQueue` operations; the drain mode rides with each drain).
enum InboxCommand {
    /// Queue one message (the enqueue path).
    Enqueue(Box<agent_loop::PromptMessage>),
    /// Take the oldest message only (`one-at-a-time`) or everything queued
    /// (`all`), per the mode current at poll time.
    Drain(
        QueueMode,
        tokio::sync::oneshot::Sender<Vec<agent_loop::PromptMessage>>,
    ),
    /// Take everything queued (upstream `clearQueue`).
    DrainAll(tokio::sync::oneshot::Sender<Vec<agent_loop::PromptMessage>>),
    /// Snapshot the queued messages without draining (upstream
    /// `getSteeringMessages` / `getFollowUpMessages`).
    Peek(tokio::sync::oneshot::Sender<Vec<agent_loop::PromptMessage>>),
}

impl Inbox {
    /// Build an empty inbox. The consumer task starts lazily on the first
    /// drain or peek (inside a tokio runtime); until then commands buffer in
    /// the unbounded channel.
    pub(super) fn new() -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        Self {
            commands,
            consumer: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    /// Enqueue one message (buffers even before the consumer task exists).
    pub(super) fn send(&self, message: agent_loop::PromptMessage) {
        let _ = self.commands.send(InboxCommand::Enqueue(Box::new(message)));
    }

    /// Start the single consumer task (exactly once, inside a runtime).
    pub(super) fn ensure_consumer(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if let Some(receiver) = self
            .consumer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            tokio::spawn(run_inbox_consumer(receiver));
        }
    }

    /// The messages queued but not yet delivered, in queue order (peek: the
    /// loop's poll owns delivery).
    pub(super) async fn peek(&self) -> Vec<agent_loop::PromptMessage> {
        // No runtime = no consumer task can ever be spawned = the command
        // would sit in the channel forever and the oneshot would never
        // resolve. The queue semantics are unavailable in that state, so
        // degrade explicitly to the empty snapshot instead of hanging the
        // caller.
        if tokio::runtime::Handle::try_current().is_err() {
            return Vec::new();
        }
        self.ensure_consumer();
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(InboxCommand::Peek(reply));
        match answer.await {
            Ok(delivered) => delivered,
            // The consumer task is gone (panic or dropped receiver): the
            // reply was dropped with the queued messages still inside it. Do
            // not let that loss stay silent.
            Err(_) => {
                tracing::warn!(
                    "inbox consumer dropped the reply; queued messages may have been lost"
                );
                Vec::new()
            }
        }
    }

    /// Take per the drain mode (`one-at-a-time`: only the oldest message).
    pub(super) async fn drain(&self, mode: QueueMode) -> Vec<agent_loop::PromptMessage> {
        // Same no-runtime degradation as [`Inbox::peek`].
        if tokio::runtime::Handle::try_current().is_err() {
            return Vec::new();
        }
        self.ensure_consumer();
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(InboxCommand::Drain(mode, reply));
        match answer.await {
            Ok(delivered) => delivered,
            Err(_) => {
                tracing::warn!(
                    "inbox consumer dropped the reply; queued messages may have been lost"
                );
                Vec::new()
            }
        }
    }

    /// Take everything queued (upstream `clearQueue`).
    pub(super) async fn drain_all(&self) -> Vec<agent_loop::PromptMessage> {
        // Same no-runtime degradation as [`Inbox::peek`].
        if tokio::runtime::Handle::try_current().is_err() {
            return Vec::new();
        }
        self.ensure_consumer();
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(InboxCommand::DrainAll(reply));
        match answer.await {
            Ok(delivered) => delivered,
            Err(_) => {
                tracing::warn!(
                    "inbox consumer dropped the reply; queued messages may have been lost"
                );
                Vec::new()
            }
        }
    }
}

/// The ONE consumer of an [`Inbox`]: owns the queued messages and serves
/// commands in arrival order (FIFO delivery on the single queue). Ends when
/// every command sender is dropped (the owning session and its runs are
/// gone).
async fn run_inbox_consumer(mut commands: mpsc::UnboundedReceiver<InboxCommand>) {
    let mut queued = VecDeque::new();
    let mut warnings = 0u64;
    while let Some(command) = commands.recv().await {
        match command {
            InboxCommand::Enqueue(message) => {
                queued.push_back(*message);
                // Observation only (see [`INBOX_WARN_DEPTH`]): the queue is
                // deliberately unbounded, so a stalled drain is reported
                // instead of being silently tolerated.
                if queued.len() >= INBOX_WARN_DEPTH {
                    warnings += 1;
                    if warnings == 1 || warnings.is_multiple_of(INBOX_REPORT_EVERY as u64) {
                        crate::ext_diag!(
                            "[session] steering/follow-up inbox at {} queued messages (the drain loop is not running; {} reports)",
                            queued.len(),
                            warnings
                        );
                    }
                }
            }
            InboxCommand::Drain(mode, reply) => {
                let delivered = match mode {
                    QueueMode::OneAtATime => queued.pop_front().into_iter().collect(),
                    QueueMode::All => queued.drain(..).collect(),
                };
                let _ = reply.send(delivered);
            }
            InboxCommand::DrainAll(reply) => {
                let _ = reply.send(queued.drain(..).collect());
            }
            InboxCommand::Peek(reply) => {
                let _ = reply.send(queued.iter().cloned().collect());
            }
        }
    }
}
