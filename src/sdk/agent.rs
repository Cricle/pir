//! The [`Agent`] handle: the embeddable agent object SDK consumers hold.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::Stream;

use crate::agent::agent_loop::{AgentEvent, EventHandler};
use crate::agent_session::{AgentSession, ClearedQueue, ContextUsage, SessionEvent, SessionStats};
use crate::ai::types::{ContentBlock, ImageContent, Usage};
use crate::config::settings::{FollowUpMode, SteeringMode};
use crate::session_manager::{FileEntry, SessionManager, StoredMessage};

use super::error::SdkError;
use super::event::{LoopOutcome, SdkEvent, SharedSink};
use super::outcome::AgentOutcome;

/// Default capacity of the single bounded event channel behind a streaming
/// run. Sized far above any realistic event burst, so a healthy consumer
/// never causes drops; a lagging consumer loses events instead of blocking
/// the agent loop. Configurable per agent with
/// [`Agent::set_event_channel_capacity`].
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Push one SDK event onto the run's stream, if one is attached.
///
/// [`None`] means this is a NON-streaming run ([`Agent::prompt`]): it has no
/// stream of its own and must never forward into the shared sink, which may
/// belong to a different (possibly already-rejected) stream that attached
/// while this run was in flight (P2-M). The bridge plugin covers the
/// `tool_execution_update` half on its own.
///
/// The push is a non-blocking `try_send`: a full channel drops the event
/// (documented on [`super::event::push_to_sink`] and [`SdkEvent`]); the run never blocks on a
/// slow consumer.
fn forward_stream_event(event_tx: &Option<tokio::sync::mpsc::Sender<SdkEvent>>, event: SdkEvent) {
    if let Some(sender) = event_tx {
        let _ = sender.try_send(event);
    }
}

/// Shared predicate selecting which [`SdkEvent`]s a streaming run forwards
/// ([`Agent::prompt_stream_with`]). Cloned into the per-run event handler.
pub(crate) type EventFilter = Arc<dyn Fn(&SdkEvent) -> bool + Send + Sync>;

/// How long [`Agent::shutdown`] waits for an in-flight run to settle after
/// aborting before giving up with an error (a run wedged in a tool that
/// ignores the abort signal must not hang teardown forever).
const SHUTDOWN_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A cheap-cloneable handle to a running agent.
///
/// Cloning shares the same underlying session; all methods take `&self`, so
/// one agent can be driven from many tasks concurrently (prompt runs are
/// serialized by the session itself — a second `prompt` while one is in
/// flight fails with [`SdkError::Other`]; queue work with
/// [`Agent::steer`]/[`Agent::follow_up`] instead).
///
/// Dropping the last handle performs no async work and never touches a Tokio
/// runtime: `Drop` is safe after the runtime has been shut down. Use
/// [`Agent::shutdown`] for the explicit graceful path (abort, settle, plugin
/// `on_shutdown` hooks).
#[derive(Clone)]
pub struct Agent {
    pub(crate) inner: Arc<AgentInner>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("model", &self.model())
            .field("streaming", &self.is_streaming())
            .field("session_path", &self.session_path())
            .finish()
    }
}

/// Signed display offset over an [`AgentSession`]'s monotonic turn/token
/// counters.
///
/// The facade's counters accumulate across the whole `AgentSession` lifetime
/// and cannot be reset from the SDK. After a storage switch the SDK therefore
/// records the difference between the live counters and the totals that
/// belong to the CURRENT session, so [`Agent::usage_totals`] keeps reporting
/// per-session numbers (zero after `new_session`/`reset`, the loaded history
/// after `attach_session`/`continue_session`).
#[derive(Debug, Clone, Copy, Default)]
struct UsageOffset {
    turns: i128,
    tokens: i128,
}

pub(crate) struct AgentInner {
    /// The headless session facade doing the work.
    pub session: Arc<AgentSession>,
    /// Live streaming sink slot (owned by the in-flight `prompt_stream`).
    /// The SAME slot is handed to the bridge plugin at construction, so
    /// `tool_execution_update` payloads ride this run's stream.
    pub sink: SharedSink,
    /// Admission flag for the ONE per-agent streaming slot. `true` from the
    /// synchronous `prompt_stream*` call until its task has emitted the
    /// terminal pair and detached. A concurrent stream is rejected BEFORE it
    /// can attach (or steal) the sink or race a session prompt, so the
    /// winner's bridge events always belong to the winner's stream.
    pub stream_active: AtomicBool,
    /// Capacity of the bounded event channel used by streaming runs.
    /// Defaults to [`EVENT_CHANNEL_CAPACITY`]; read at stream start so a
    /// change applies to the next run.
    pub event_channel_capacity: AtomicUsize,
    /// Set by [`Agent::shutdown`]; post-shutdown prompts are rejected.
    pub shut_down: AtomicBool,
    /// The agent's working directory (new sessions are stamped with it).
    pub cwd: PathBuf,
    /// The agent's OWN config directory (D2): the builder's `.agent_dir(..)`
    /// when supplied, else the process environment resolved once at build.
    /// Every session-scoped settings/auth/sessions-root read of this agent
    /// goes through it, so two agents built around different directories in
    /// one process never share one directory's state.
    pub agent_dir: PathBuf,
    /// Directory fresh session files are created in, when the agent has
    /// session storage ([`Agent::new_session`]). `None` for
    /// [`SessionMode::None`](crate::sdk::SessionMode::None) agents.
    ///
    /// Build-bound (D9): resolved once from the agent's directory (or the
    /// explicit [`SessionMode::Dir`](crate::sdk::SessionMode::Dir) /
    /// [`SessionMode::Existing`](crate::sdk::SessionMode::Existing) path) and
    /// never re-resolved from the environment afterwards; see
    /// [`Agent::session_dir`].
    pub session_dir: Option<PathBuf>,
    /// Display offset over the session's live `turns`/`tokens` counters (see
    /// [`UsageOffset`]).
    usage_offset: Mutex<UsageOffset>,
    /// Whether the model was explicitly chosen (a builder model setup shape
    /// or [`Agent::set_model`]). An attached session's recorded model then
    /// does not override it.
    model_pinned: AtomicBool,
    /// Whether the thinking level was explicitly chosen (builder
    /// [`crate::sdk::AgentBuilder::thinking_level`] or
    /// [`Agent::set_thinking_level`]). An attached session's recorded level
    /// then does not override it.
    thinking_pinned: AtomicBool,
    /// Provider the configured API key belongs to, when it came from the
    /// environment for one provider. Restoring a session model from another
    /// provider would send that key to the wrong endpoint, so the model
    /// restore is skipped for such a cross-provider attach.
    api_key_provider: Option<String>,
}

/// RAII owner of one streaming slot: detaches the bridge sink and releases
/// the per-agent admission flag when the streaming task ends, including on
/// cancellation/panic. Detach runs FIRST so a newly admitted stream can never
/// observe the old sender.
struct StreamRunGuard {
    inner: Arc<AgentInner>,
}

impl Drop for StreamRunGuard {
    fn drop(&mut self) {
        *self
            .inner
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.inner.stream_active.store(false, Ordering::SeqCst);
    }
}

/// Adapt a receiver into the boxed, `Unpin` SDK stream shape shared by every
/// `prompt_stream*` return path (including the up-front rejected stream).
fn sdk_event_stream(
    receiver: tokio::sync::mpsc::Receiver<SdkEvent>,
) -> Pin<Box<dyn Stream<Item = SdkEvent> + Send>> {
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| (event, receiver))
    });
    Box::pin(stream)
}

/// A stream that never starts a run: it delivers the documented terminal
/// rejection pair (`PromptRejected` + `AgentSettled`) with backpressure, then
/// closes. Used before any sink attachment, so a rejected caller cannot
/// observe (or steal) the in-flight run's events.
fn rejected_event_stream(
    sender: tokio::sync::mpsc::Sender<SdkEvent>,
    receiver: tokio::sync::mpsc::Receiver<SdkEvent>,
    message: impl Into<String> + Send + 'static,
) -> Pin<Box<dyn Stream<Item = SdkEvent> + Send>> {
    let message = message.into();
    tokio::spawn(async move {
        let _ = sender.send(SdkEvent::PromptRejected { message }).await;
        let _ = sender.send(SdkEvent::AgentSettled { aborted: false }).await;
    });
    sdk_event_stream(receiver)
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session: Arc<AgentSession>,
        sink: SharedSink,
        cwd: PathBuf,
        session_dir: Option<PathBuf>,
        agent_dir: PathBuf,
        model_pinned: bool,
        thinking_pinned: bool,
        initial_usage: UsageSnapshot,
        api_key_provider: Option<String>,
    ) -> Self {
        let state = session.state();
        // Session-level events (queue/compaction/entry/session-info/retry/
        // bash/background completion) are emitted outside the agent-loop event
        // enum; subscribe once and mirror them onto whichever prompt stream
        // (if any) is active. The sink slot is shared, so an event with no
        // live stream is dropped and the subscription never outlives the
        // session.
        {
            let event_sink = Arc::clone(&sink);
            session.on_session_event(Arc::new(move |event| {
                if let Some(sdk_event) = SdkEvent::from_session_event(&event) {
                    super::event::push_to_sink(&event_sink, sdk_event);
                }
            }));
        }
        Self {
            inner: Arc::new(AgentInner {
                session,
                sink,
                stream_active: AtomicBool::new(false),
                event_channel_capacity: AtomicUsize::new(EVENT_CHANNEL_CAPACITY),
                shut_down: AtomicBool::new(false),
                cwd,
                agent_dir,
                session_dir,
                usage_offset: Mutex::new(UsageOffset {
                    turns: initial_usage.turns as i128 - state.turns as i128,
                    tokens: initial_usage.total_tokens as i128 - state.tokens as i128,
                }),
                model_pinned: AtomicBool::new(model_pinned),
                thinking_pinned: AtomicBool::new(thinking_pinned),
                api_key_provider,
            }),
        }
    }

    /// Run one user prompt through the agent loop and wait for the outcome.
    ///
    /// No [`SdkEvent`] stream is exposed on this path; use
    /// [`Agent::prompt_stream`] when you need one. Aborted runs return the
    /// partial outcome with [`AgentOutcome::aborted`] set; an abort before
    /// any assistant text was produced yields [`SdkError::Aborted`].
    ///
    /// The returned future is heap-allocated (see [`Agent::prompt_boxed`]):
    /// the direct run future is large in unoptimized builds and used to
    /// exceed the default Windows main-thread stack in debug builds.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use pir::sdk::{Agent, AgentBuilder};
    ///
    /// let agent = AgentBuilder::new()
    ///     .endpoint(pir::sdk::Endpoint::new(
    ///         "https://api.example.com/v1",
    ///         pir::sdk::ApiKind::OpenAiCompletions,
    ///         "my-model",
    ///     ))
    ///     .api_key("sk-...")
    ///     .no_session()
    ///     .build()
    ///     .await?;
    /// let outcome = agent.prompt("Summarize this repository.").await?;
    /// println!("{}", outcome.message_text().unwrap_or_default());
    /// # Ok(())
    /// # }
    /// ```
    pub fn prompt(
        &self,
        text: impl Into<String>,
    ) -> impl Future<Output = Result<AgentOutcome, SdkError>> + Send + '_ {
        self.prompt_boxed(text)
    }

    /// [`Agent::prompt`] with an explicitly boxed future.
    ///
    /// The prompt run's generated future is large in debug builds; boxing it
    /// keeps the caller's stack frame small even on platforms with a 1 MiB
    /// main-thread stack (Windows). [`Agent::prompt`] already takes this path;
    /// this method exists for callers that want the box in the signature.
    pub fn prompt_boxed(
        &self,
        text: impl Into<String>,
    ) -> Pin<Box<dyn Future<Output = Result<AgentOutcome, SdkError>> + Send + '_>> {
        self.run_prompt(text.into(), Vec::new())
    }

    /// Like [`Agent::prompt`], with image attachments: the images become
    /// image content blocks appended to the starting user message (text
    /// first, then the images in order), so the provider receives one user
    /// message whose content is `[text, image, ...]`.
    ///
    /// The images ride the session's normal prompt path
    /// ([`AgentSession::prompt_with_images`]): the `input` extension event
    /// sees them and may rewrite them, and the same handler/outcome capture as
    /// [`Agent::prompt`] applies. Passing an empty vector is equivalent to
    /// [`Agent::prompt`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use pir::ai::types::ImageContent;
    /// use pir::sdk::AgentBuilder;
    ///
    /// # let agent = AgentBuilder::new().no_session().build().await?;
    /// let image = ImageContent {
    ///     data: "iVBORw0KGgo=".to_owned(),
    ///     mime_type: "image/png".to_owned(),
    /// };
    /// let outcome = agent.prompt_with_images("What is in this image?", vec![image]).await?;
    /// println!("{}", outcome.message_text().unwrap_or_default());
    /// # Ok(())
    /// # }
    /// ```
    pub fn prompt_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> impl Future<Output = Result<AgentOutcome, SdkError>> + Send + '_ {
        self.run_prompt(text.into(), images)
    }

    /// Shared prompt path: one user message (text + optional images) through
    /// the session, with the SDK event handler capturing the run outcome.
    ///
    /// Returns an ALREADY-BOXED future: the session prompt state machines are
    /// themselves boxed at each `.await`, so constructing this future does not
    /// build a deep inline state machine on the caller's stack.
    fn run_prompt(
        &self,
        text: String,
        images: Vec<ImageContent>,
    ) -> Pin<Box<dyn Future<Output = Result<AgentOutcome, SdkError>> + Send + '_>> {
        Box::pin(async move {
            if self.inner.shut_down.load(Ordering::SeqCst) {
                return Err(SdkError::Other("agent has been shut down".to_owned()));
            }
            let outcome_slot: Arc<Mutex<Option<LoopOutcome>>> = Arc::new(Mutex::new(None));
            let settled_aborted = Arc::new(AtomicBool::new(false));
            let handler = self.event_handler(
                Arc::clone(&outcome_slot),
                settled_aborted,
                None,
                Arc::new(|_: &SdkEvent| true),
                true,
            );
            let terminal = if images.is_empty() {
                Box::pin(self.inner.session.prompt(text.as_str(), handler))
                    .await
                    .map_err(SdkError::from_session_boundary)?
            } else {
                let images = images.into_iter().map(ContentBlock::Image).collect();
                Box::pin(
                    self.inner
                        .session
                        .prompt_with_images(text.as_str(), images, handler),
                )
                .await
                .map_err(SdkError::from_session_boundary)?
            };
            let captured = outcome_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let outcome = match captured {
                Some(outcome) => AgentOutcome::from_parts(outcome),
                // Defensive: a run that returned a terminal message without an
                // `AgentEnd` event (should not happen; kept total).
                None => AgentOutcome::from_terminal(terminal),
            };
            if outcome.aborted && outcome.message_text().is_none() {
                return Err(SdkError::Aborted);
            }
            Ok(outcome)
        })
    }

    /// Run one user prompt, streaming typed [`SdkEvent`]s as the run
    /// progresses.
    ///
    /// Events flow through one bounded async channel (default capacity
    /// 1024, configurable with [`Agent::set_event_channel_capacity`]).
    /// Intermediate-event delivery is best-effort: pushing an event never
    /// blocks the agent loop, and when a slow consumer has filled the channel
    /// the event is dropped (the run keeps going). The terminal
    /// [`SdkEvent::AgentSettled`] (and [`SdkEvent::AgentOutcome`] on the
    /// outcome-carrying variant) is sent with backpressure after the whole
    /// prompt run settles, so it is not dropped. The stream ends because the
    /// sender closes. Dropping the stream detaches the observer (the run
    /// itself keeps going; use [`Agent::abort`] to stop it). The final event
    /// is always [`SdkEvent::AgentSettled`] unless the receiver was dropped
    /// first.
    ///
    /// One prompt stream may be active per agent. A concurrent call is
    /// rejected before it can attach the shared event sink: its stream yields
    /// [`SdkEvent::PromptRejected`] followed by [`SdkEvent::AgentSettled`],
    /// while the in-flight run's stream is untouched.
    ///
    /// This path yields events only; it does not capture an
    /// [`AgentOutcome`](crate::sdk::AgentOutcome), so it also skips the
    /// settlement clone of the terminal message and tool results. Use
    /// [`Agent::prompt_stream_with_outcome`] when the terminal outcome must
    /// ride the stream, [`Agent::prompt`] for a non-streaming run,
    /// [`SdkEvent::MessageEnd`] for each completed assistant message,
    /// [`SdkEvent::ToolExecutionUpdate`] for tool-defined partial output, and
    /// [`Agent::usage_totals`] after settlement for totals.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use futures_util::StreamExt;
    /// use pir::sdk::{Agent, AgentBuilder, SdkEvent};
    ///
    /// let agent = AgentBuilder::new()
    ///     .endpoint(pir::sdk::Endpoint::new(
    ///         "https://api.example.com/v1",
    ///         pir::sdk::ApiKind::AnthropicMessages,
    ///         "my-model",
    ///     ))
    ///     .api_key("sk-...")
    ///     .no_session()
    ///     .build()
    ///     .await?;
    /// let mut events = agent.prompt_stream("Write a haiku about Rust.");
    /// while let Some(event) = events.next().await {
    ///     if let SdkEvent::MessageUpdate { delta, .. } = event {
    ///         print!("{delta}");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn prompt_stream(&self, text: impl Into<String>) -> impl Stream<Item = SdkEvent> {
        self.prompt_stream_with(text, |_| true)
    }

    /// Like [`Agent::prompt_stream`], with a caller-supplied event filter:
    /// events for which `filter` returns `false` are dropped at the per-run
    /// event handler and never reach the returned stream (upstream
    /// `promptStream` option `filter`-style pre-filtering). `filter` runs on
    /// the loop thread — keep it cheap and non-blocking. The filtered stream
    /// still captures no [`AgentOutcome`](crate::sdk::AgentOutcome); see
    /// [`Agent::prompt_stream`] for the available substitutes and
    /// [`Agent::prompt_stream_with_outcome`] for the outcome-carrying
    /// variant.
    ///
    /// Not filtered: the terminal events
    /// ([`SdkEvent::AgentSettled`], [`SdkEvent::PromptRejected`], and
    /// [`SdkEvent::AgentOutcome`]), `tool_execution_update` payloads pushed by
    /// the live bridge plugin while a run is attached (they bypass the loop's
    /// event handler), and the SESSION-level projections the agent's session
    /// subscription pushes onto the same sink (`QueueUpdate`,
    /// `CompactionStart`/`CompactionEnd`, `EntryAppended`,
    /// `SessionInfoChanged`, `ThinkingLevelChanged`, `BashExecutionUpdate`,
    /// `AutoRetryStart`/`AutoRetryEnd`,
    /// `SummarizationRetryScheduled`/`SummarizationRetryAttemptStart`/
    /// `SummarizationRetryFinished`, `BackgroundToolEnd`) — those are emitted
    /// outside the agent loop. The stream always ends with its terminal event
    /// pair even when the filter drops every loop event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use futures_util::StreamExt;
    /// use pir::sdk::{Agent, AgentBuilder, SdkEvent};
    ///
    /// # let agent: Agent = AgentBuilder::new()
    /// #     .no_session()
    /// #     .build()
    /// #     .await?;
    /// // Stream only the tool lifecycle, dropping streaming text deltas.
    /// let mut events = agent.prompt_stream_with("List files.", |event| {
    ///     !matches!(event, SdkEvent::MessageUpdate { .. })
    /// });
    /// while let Some(event) = events.next().await {
    ///     if let SdkEvent::ToolExecutionEnd { tool_name, .. } = event {
    ///         println!("finished: {tool_name}");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn prompt_stream_with<F>(
        &self,
        text: impl Into<String>,
        filter: F,
    ) -> impl Stream<Item = SdkEvent>
    where
        F: Fn(&SdkEvent) -> bool + Send + Sync + 'static,
    {
        self.prompt_stream_with_content(text.into(), Vec::new(), filter, false)
    }

    /// Like [`Agent::prompt_stream`], but the stream also carries the complete
    /// terminal [`AgentOutcome`] as [`SdkEvent::AgentOutcome`], emitted
    /// immediately before the final [`SdkEvent::AgentSettled`].
    ///
    /// This is the streaming counterpart of [`Agent::prompt`]: it exposes the
    /// terminal message, the run's tool-result messages, usage, turn count
    /// and the aborted flag without requiring a second run. The outcome is
    /// captured (and its terminal message/tool results cloned) only for this
    /// method; [`Agent::prompt_stream`] skips that work.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use futures_util::StreamExt;
    /// use pir::sdk::{AgentBuilder, SdkEvent};
    ///
    /// # let agent: pir::sdk::Agent = AgentBuilder::new().no_session().build().await?;
    /// let mut events = agent.prompt_stream_with_outcome("Summarize this repo.");
    /// while let Some(event) = events.next().await {
    ///     if let SdkEvent::AgentOutcome(outcome) = event {
    ///         println!("{}", outcome.message_text().unwrap_or_default());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn prompt_stream_with_outcome(
        &self,
        text: impl Into<String>,
    ) -> impl Stream<Item = SdkEvent> {
        self.prompt_stream_with_content(text.into(), Vec::new(), |_| true, true)
    }

    /// Like [`Agent::prompt_stream`], with image attachments: the images
    /// become image content blocks appended to the starting user message
    /// (text first, then the images in order), exactly like
    /// [`Agent::prompt_with_images`]. The stream carries [`SdkEvent`]s only;
    /// pair it with [`Agent::usage_totals`] after settlement when totals are
    /// needed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use futures_util::StreamExt;
    /// use pir::ai::types::ImageContent;
    /// use pir::sdk::{AgentBuilder, SdkEvent};
    ///
    /// # let agent = AgentBuilder::new().no_session().build().await?;
    /// let image = ImageContent {
    ///     data: "iVBORw0KGgo=".to_owned(),
    ///     mime_type: "image/png".to_owned(),
    /// };
    /// let mut events = agent.prompt_stream_with_images("Describe it.", vec![image]);
    /// while let Some(event) = events.next().await {
    ///     if let SdkEvent::MessageUpdate { delta, .. } = event {
    ///         print!("{delta}");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn prompt_stream_with_images(
        &self,
        text: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> impl Stream<Item = SdkEvent> {
        self.prompt_stream_with_content(text.into(), images, |_| true, false)
    }

    /// Shared streaming path: builds the bounded event channel and runs one
    /// prompt (text plus optional image blocks), reporting a rejected prompt
    /// on the stream itself. `capture_outcome` enables the settlement clone
    /// and the trailing [`SdkEvent::AgentOutcome`].
    fn prompt_stream_with_content<F>(
        &self,
        text: String,
        images: Vec<ImageContent>,
        filter: F,
        capture_outcome: bool,
    ) -> impl Stream<Item = SdkEvent>
    where
        F: Fn(&SdkEvent) -> bool + Send + Sync + 'static,
    {
        let filter: EventFilter = Arc::new(filter);
        // ONE bounded async channel: the synchronous event handler and the
        // async bridge plugin both `try_send` onto it, and the consumer reads
        // it directly. No blocking cross-stage handoff, so a slow consumer
        // can never deadlock a current_thread runtime.
        let capacity = self
            .inner
            .event_channel_capacity
            .load(Ordering::Relaxed)
            .max(1);
        let (event_tx, stream_rx) = tokio::sync::mpsc::channel::<SdkEvent>(capacity);

        // Admission FIRST, synchronously, before touching the sink: a second
        // stream must not attach (and must not later start a session prompt
        // whose bridge events would be routed to nobody after the winner
        // detached). The loser still gets the documented terminal pair.
        if self.inner.stream_active.swap(true, Ordering::SeqCst) {
            return rejected_event_stream(
                event_tx,
                stream_rx,
                "cannot start a prompt stream: another stream is already streaming for this agent",
            );
        }
        // A run started through ANOTHER entry point (`Agent::prompt`, a host
        // driving the session directly, or a manual compaction) already owns
        // the session. Reject before attaching, so its session-level events
        // are not mirrored onto this never-started stream.
        if self.inner.session.is_streaming() || self.inner.session.is_compacting() {
            self.inner.stream_active.store(false, Ordering::SeqCst);
            return rejected_event_stream(
                event_tx,
                stream_rx,
                "cannot start a prompt stream: the session is already busy with a run or compaction",
            );
        }

        // The sink is attached only after admission, so it is guaranteed to
        // be empty: the in-flight stream's bridge events can never be stolen.
        *self
            .inner
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(event_tx.clone());

        let session = Arc::clone(&self.inner.session);
        // RAII: created before the task is spawned and moved into it, so the
        // sink detaches and the admission flag clears on normal completion,
        // cancellation, and panic alike.
        let run_guard = StreamRunGuard {
            inner: Arc::clone(&self.inner),
        };
        // Read the shutdown flag INSIDE the task, not here: the task does not
        // poll until the caller yields, so `prompt_stream(..)` followed by
        // `shutdown().await` (a documented teardown order) must still observe
        // the shutdown and reject on the stream instead of starting a run on a
        // torn-down plugin surface.
        let agent = Arc::clone(&self.inner);
        let outcome_slot: Arc<Mutex<Option<LoopOutcome>>> = Arc::new(Mutex::new(None));
        // The loop's `AgentEnd` is per low-level run, not per prompt run: a
        // session may issue further requests (`agent_before_settle`
        // continuation, overflow recovery) after it. This flag carries the
        // FINAL loop run's aborted state to the terminal event the task below
        // emits once the whole session run has settled.
        let settled_aborted = Arc::new(AtomicBool::new(false));
        // A clone kept out of the handler so the task can report a rejected
        // prompt (the session refuses a second concurrent run) on the stream.
        let reject_tx = event_tx.clone();
        let handler = self.event_handler(
            Arc::clone(&outcome_slot),
            Arc::clone(&settled_aborted),
            Some(event_tx),
            filter,
            capture_outcome,
        );
        // Image blocks are appended to the starting user message by the
        // session path; a text-only prompt keeps the unchanged `prompt()` API.
        let images = images
            .into_iter()
            .map(ContentBlock::Image)
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            // Owns the sink detach and admission release. It is dropped after
            // the terminal sends below, so a late bridge update can never be
            // queued after `AgentSettled` and the next stream is admitted only
            // once this one has fully settled.
            let _run_guard = run_guard;
            if agent.shut_down.load(Ordering::SeqCst) {
                // Rejection terminals are sent with backpressure (this task is
                // async, so waiting cannot stall the agent loop) — a caller
                // that configured a tiny channel must still observe the
                // documented `PromptRejected` + `AgentSettled` pair.
                let _ = reject_tx
                    .send(SdkEvent::PromptRejected {
                        message: "agent has been shut down".to_owned(),
                    })
                    .await;
                let _ = reject_tx
                    .send(SdkEvent::AgentSettled { aborted: false })
                    .await;
            } else {
                // A prompt that never starts (already streaming, shut down,
                // session storage failure) must still terminate the stream
                // with a terminal settled event, never silently end it.
                let result = if images.is_empty() {
                    session.prompt(text.as_str(), handler).await
                } else {
                    session
                        .prompt_with_images(text.as_str(), images, handler)
                        .await
                };
                // `session.prompt` returns only after the whole prompt run
                // settled (every `agent_before_settle` continuation and
                // post-run recovery included), so this is the SDK's terminal
                // point. Deliver the outcome and the settled event with
                // backpressure: terminal events are not best-effort like the
                // intermediate stream. The guard then detaches the sink before
                // the task exits.
                match result {
                    Ok(terminal) => {
                        if capture_outcome {
                            let outcome = outcome_slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .take();
                            let outcome = match outcome {
                                Some(outcome) => AgentOutcome::from_parts(outcome),
                                // Defensive: a run that returned a terminal
                                // message without an `AgentEnd` capture.
                                None => AgentOutcome::from_terminal(terminal),
                            };
                            let _ = reject_tx
                                .send(SdkEvent::AgentOutcome(Box::new(outcome)))
                                .await;
                        }
                        let _ = reject_tx
                            .send(SdkEvent::AgentSettled {
                                aborted: settled_aborted.load(Ordering::SeqCst),
                            })
                            .await;
                    }
                    Err(error) => {
                        let _ = reject_tx
                            .send(SdkEvent::PromptRejected {
                                message: error.to_string(),
                            })
                            .await;
                        let _ = reject_tx
                            .send(SdkEvent::AgentSettled {
                                aborted: settled_aborted.load(Ordering::SeqCst),
                            })
                            .await;
                    }
                }
            }
        });

        // Boxed so the returned stream is `Unpin` (`.next()` works without
        // explicit pinning) while staying a single `impl Stream` type.
        sdk_event_stream(stream_rx)
    }

    /// Build the per-run event handler: maps loop events into SDK events,
    /// captures the run outcome (`capture_outcome`; `false` on the plain
    /// streaming path so settlement clones nothing), records the final loop
    /// run's aborted state, applies the event filter, and (optionally) pushes
    /// events to the streaming sink.
    ///
    /// Terminal events are NOT produced here: [`SdkEvent::AgentSettled`] (and
    /// [`SdkEvent::AgentOutcome`] when captured) is emitted by the caller
    /// after [`crate::agent_session::AgentSession::prompt`] returns, because
    /// the session can re-enter the agent loop (`agent_before_settle`
    /// continuation) before the prompt run settles.
    ///
    /// `MessageEnd` is de-duplicated: the internal loop emits a terminal
    /// stream event on both the consumed-stream and the final-message paths,
    /// so a run can observe two `StreamEnd`s per turn; the SDK surfaces one
    /// `MessageEnd` per `MessageStart`. The de-duplication tracks only the
    /// ASSISTANT pair; the non-assistant
    /// [`SdkEvent::TranscriptMessageStart`]/[`SdkEvent::TranscriptMessageEnd`]
    /// pair (and every other event) passes through one-for-one. The filter
    /// runs after the dedup, on the final SDK event, and also gates pushes to
    /// this run's own `event_tx`.
    ///
    /// Pushes are always non-blocking (`try_send`): a full channel drops the
    /// event instead of stalling the loop. A non-streaming run (`event_tx`
    /// `None`, [`Agent::prompt`]) forwards nothing at all — it must never
    /// feed a stream owned by a different run.
    fn event_handler(
        &self,
        outcome_slot: Arc<Mutex<Option<LoopOutcome>>>,
        settled_aborted: Arc<AtomicBool>,
        event_tx: Option<tokio::sync::mpsc::Sender<SdkEvent>>,
        filter: EventFilter,
        capture_outcome: bool,
    ) -> EventHandler {
        let awaiting_message_end = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Arc::new(move |event: AgentEvent| {
            // Record the run's terminal aborted flag from the ONE event that
            // carries it. The SDK's terminal event is emitted after the whole
            // session run settles, not here.
            if let AgentEvent::AgentEnd { result } = &event {
                settled_aborted.store(result.aborted, Ordering::SeqCst);
            }
            let (sdk_event, outcome) =
                super::event::from_loop_event(event, capture_outcome, event_tx.is_some());
            if let Some(outcome) = outcome {
                *outcome_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
            }
            let awaiting_end = &awaiting_message_end;
            let sdk_event = match sdk_event {
                Some(SdkEvent::MessageStart) => {
                    awaiting_end.store(true, std::sync::atomic::Ordering::SeqCst);
                    Some(SdkEvent::MessageStart)
                }
                Some(event @ SdkEvent::MessageEnd { .. }) => {
                    if awaiting_end.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        Some(event)
                    } else {
                        None
                    }
                }
                other => other,
            };
            // Caller-supplied filter: drop the event entirely when the
            // predicate rejects it (the outcome capture above already ran).
            let sdk_event = sdk_event.filter(|event| filter(event));
            if let Some(event) = sdk_event {
                forward_stream_event(&event_tx, event);
            }
        })
    }

    /// Set the capacity (in events) of the bounded channel used by
    /// subsequent streaming runs on this agent. Defaults to 1024, which is
    /// far above any realistic event burst. Values below 1 are clamped to 1.
    ///
    /// The capacity is read when a stream starts, so the change applies to
    /// the next [`Agent::prompt_stream`] (or
    /// [`Agent::prompt_stream_with_outcome`]) call. As documented there, a
    /// full channel drops events rather than blocking the run.
    pub fn set_event_channel_capacity(&self, capacity: usize) {
        self.inner
            .event_channel_capacity
            .store(capacity.max(1), Ordering::Relaxed);
    }

    /// The capacity used by the next streaming run (see
    /// [`Agent::set_event_channel_capacity`]).
    pub fn event_channel_capacity(&self) -> usize {
        self.inner.event_channel_capacity.load(Ordering::Relaxed)
    }

    /// Queue a steering message. It is queued immediately and delivered when
    /// a prompt run consumes it: during the in-flight run (after the current
    /// assistant turn finishes its tool calls, before the next model request)
    /// or, between runs, at the start of the next prompt run.
    pub fn steer(&self, text: impl Into<String>) {
        self.inner.session.steer(text.into().as_str());
    }

    /// Queue a follow-up message. It is queued immediately and delivered once
    /// a prompt run has otherwise finished its turns: the run in flight (or
    /// the next prompt run, when the agent is idle) picks it up and continues.
    pub fn follow_up(&self, text: impl Into<String>) {
        self.inner.session.follow_up(text.into().as_str());
    }

    /// Install (or clear) the per-tool lifecycle hooks every subsequent run
    /// uses (upstream `AgentLoopConfig.beforeToolCall` / `afterToolCall`).
    ///
    /// `before` runs after argument validation and before execution (it may
    /// block the call and set the batch termination hint); `after` runs after
    /// the tool and before `tool_execution_end` (it may replace the result's
    /// content/details/error flag/usage and set termination). The extension
    /// `tool_call` / `tool_result` faces keep their upstream timing and run
    /// exactly once. Passing `None` clears the hooks; a change applies to runs
    /// started after it.
    pub fn set_tool_hooks(&self, hooks: Option<Arc<crate::agent::tool_execution::ToolHooks>>) {
        self.inner.session.set_tool_hooks(hooks);
    }

    /// The installed per-tool lifecycle hooks, if any.
    pub fn tool_hooks(&self) -> Option<Arc<crate::agent::tool_execution::ToolHooks>> {
        self.inner.session.tool_hooks()
    }

    /// Abort the in-flight run. The run settles with
    /// [`SdkEvent::AgentSettled { aborted: true }`] / an aborted
    /// [`AgentOutcome`]. No-op between runs.
    pub fn abort(&self) {
        self.inner.session.abort();
    }

    /// Compact the conversation now. Returns the compaction summary details
    /// (tokens before, entries kept). Fails when nothing can be compacted
    /// or the summarizer call fails.
    pub async fn compact(&self) -> Result<crate::compaction::CompactionResult, SdkError> {
        self.inner
            .session
            .compact(None)
            .await
            .map_err(SdkError::from_anyhow)
    }

    /// Whether a prompt run is currently in flight.
    pub fn is_streaming(&self) -> bool {
        self.inner.session.is_streaming()
    }

    /// Whether the agent is idle, i.e. no prompt run is in flight. This is
    /// the inverse of [`Agent::is_streaming`].
    pub fn is_idle(&self) -> bool {
        !self.is_streaming()
    }

    /// Steering plus follow-up messages queued but not yet delivered
    /// (upstream `session.pendingMessageCount`). Settles back to zero once the
    /// run consumes them; a steering message queued while idle is delivered
    /// by the next prompt run.
    pub fn pending_message_count(&self) -> usize {
        self.inner.session.pending_message_count()
    }

    /// The tools the next run declares to the model: the session's active
    /// loadout after the builder's tool selection
    /// ([`crate::sdk::AgentBuilder::tools`] /
    /// [`crate::sdk::AgentBuilder::exclude_tools`] /
    /// [`crate::sdk::AgentBuilder::no_tools`]) has been applied to the
    /// MERGED registry (builder-registered tools plus every tool registered
    /// by the attached plugin host). Hidden tools are excluded;
    /// `codemode`/`deferred` tools that are not active are callable but not
    /// declared, so they are not listed here until something activates them.
    /// Sorted by name so the snapshot is deterministic.
    ///
    /// This is the live declaration view: plugin tools registered after the
    /// build appear once active, and a tool removed by a plugin disappears.
    /// Which subset a single run actually declares can still be narrowed by
    /// loadout hooks (`prepareLoadout`), which run per prompt.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// # let agent = pir::sdk::AgentBuilder::new().no_session().build().await?;
    /// for tool in agent.tools() {
    ///     println!("{}: {}", tool.name, tool.description);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn tools(&self) -> Vec<ToolInfo> {
        let (registry, active) = self
            .inner
            .session
            .active_tool_snapshot(self.inner.session.plugin_host());
        let mut tools: Vec<ToolInfo> = active
            .iter()
            .filter_map(|name| registry.get(name))
            .map(|tool| ToolInfo {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.parameters(),
            })
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    /// Drain the presentation-class extension notifications queued by the
    /// session's plugin host (upstream `ctx.ui.notify`/`setStatus`/`setTitle`
    /// and the other records the TUI drain consumes), for headless hosts that
    /// render them themselves.
    ///
    /// Actionable notifications (`setModel`, `setThinkingLevel`,
    /// `setActiveTools`, `abort`, ...) are left in the queue for the session
    /// host to apply at the end of the run — the two classes partition the
    /// queue exactly like the TUI/session drains. The underlying sink is
    /// drain-only (there is no non-draining peek), so a `notifications()`
    /// snapshot is intentionally not exposed; call this when you are ready to
    /// consume the queue. Returns an empty vector when the session has no JS
    /// extension runtime.
    pub fn take_notifications(&self) -> Vec<crate::extensions::Notification> {
        self.inner
            .session
            .plugin_host()
            .map(|host| host.take_notifications(crate::extensions::Notification::is_presentation))
            .unwrap_or_default()
    }

    /// Subscribe to the session-level events that are not tied to one prompt
    /// stream: queue changes, compaction lifecycle, transcript appends,
    /// session-name changes, thinking-level changes, out-of-loop bash output,
    /// transport/summarizer retries, background tool completions, and
    /// `AgentSettled`.
    ///
    /// This is the [`Agent`]-facade counterpart of
    /// [`AgentSession::on_session_event`](crate::agent_session::AgentSession::on_session_event)
    /// and the subscription to use when the agent is driven with
    /// [`Agent::prompt`] (which has no [`SdkEvent`] stream) or when a host
    /// wants session events between prompt runs. Handlers run synchronously on
    /// the emitting task: keep them cheap and hand off to a channel/thread for
    /// anything heavier. A subscription lives for the session's lifetime
    /// (there is no unsubscribe handle), so register one handler and fan out
    /// inside it if needed.
    ///
    /// Project an event onto the typed SDK surface with
    /// [`SdkEvent::from_session_event`](crate::sdk::SdkEvent::from_session_event);
    /// that projection returns `None` for `SessionEvent::AgentSettled`, whose
    /// SDK stream terminal is emitted by the run driver.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// # let agent = pir::sdk::AgentBuilder::new().no_session().build().await?;
    /// use pir::sdk::{SessionEvent, SdkEvent};
    ///
    /// agent.on_session_event(|event| {
    ///     let projected = SdkEvent::from_session_event(&event);
    ///     if matches!(&event, SessionEvent::EntryAppended { .. }) {
    ///         // Mirror the transcript append in a host UI / audit log.
    ///     }
    ///     let _ = projected;
    /// });
    /// # Ok(())
    /// # }
    /// ```
    pub fn on_session_event<F>(&self, handler: F)
    where
        F: Fn(SessionEvent) + Send + Sync + 'static,
    {
        self.inner.session.on_session_event(Arc::new(handler));
    }

    /// The agent config directory this agent identifies with (D2).
    ///
    /// [`crate::sdk::AgentBuilder::agent_dir`] when the caller supplied one,
    /// else the process environment (`PIR_AGENT_DIR` /
    /// `PIR_CODING_AGENT_DIR` / `PI_CODING_AGENT_DIR`, else `~/.pi/agent`)
    /// resolved once at build. Every session-scoped settings, `auth.json` and
    /// sessions-root read of this agent goes through this value, so a
    /// mid-process environment change never re-points a live agent (D9).
    pub fn agent_dir(&self) -> &Path {
        &self.inner.agent_dir
    }

    /// The directory fresh session files are created in, when session storage
    /// is enabled (D9).
    ///
    /// Build-bound: resolved once, from the agent's own directory for
    /// [`SessionMode::DefaultDir`](crate::sdk::SessionMode::DefaultDir) or
    /// from the explicit directory/file the caller passed. Unlike every other
    /// agent-dir reader, it does NOT follow the process environment after the
    /// build — a session belongs to the directory (and cwd) it was created
    /// under, and [`Agent::new_session`] / [`Agent::continue_session`] keep
    /// writing there. Build a new agent (with a new
    /// [`crate::sdk::AgentBuilder::agent_dir`]) to change roots.
    pub fn session_dir(&self) -> Option<&Path> {
        self.inner.session_dir.as_deref()
    }

    /// The session file path, when session persistence is enabled.
    ///
    /// The file itself is created lazily by the first persisted message, so
    /// a freshly built agent may report a path that does not exist yet.
    pub fn session_path(&self) -> Option<PathBuf> {
        let state = self.inner.session.state();
        state
            .session_path
            .map(std::path::PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    }

    /// The persisted session id, when session storage is enabled. Refreshed
    /// by [`Agent::new_session`] and [`Agent::attach_session`].
    pub fn session_id(&self) -> Option<String> {
        self.inner.session.session_id()
    }

    /// The current session display name, if any (upstream
    /// `session.sessionName`). A cleared name (empty or `None`) reads as
    /// `None`.
    pub async fn session_name(&self) -> Option<String> {
        self.inner
            .session
            .session_name()
            .await
            .filter(|name| !name.trim().is_empty())
    }

    /// Set or clear the session display name (upstream `/name` →
    /// `sessionManager.appendSessionInfo`). Persisted as a `session_info`
    /// entry when the session has storage; kept in memory otherwise. Passing
    /// `None` clears it. Returns the normalized name the session reports back
    /// (upstream warns when it differs from the input); `None` when cleared.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// # let agent = pir::sdk::AgentBuilder::new().no_session().build().await?;
    /// agent.set_session_name(Some("Refactor plan")).await?;
    /// assert_eq!(agent.session_name().await.as_deref(), Some("Refactor plan"));
    /// agent.set_session_name(None).await?;
    /// assert_eq!(agent.session_name().await, None);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_session_name(&self, name: Option<&str>) -> Result<Option<String>, SdkError> {
        match name {
            Some(name) => {
                let normalized = self
                    .inner
                    .session
                    .set_session_name(name)
                    .await
                    .map_err(SdkError::from_anyhow)?;
                Ok(normalized.filter(|name| !name.trim().is_empty()))
            }
            None => {
                if let Some(manager) = self.inner.session.session_manager() {
                    manager
                        .lock()
                        .await
                        .set_session_name(None)
                        .map_err(SdkError::from_anyhow)?;
                } else {
                    // Ephemeral session: store an empty name, which the
                    // accessor above reports as cleared.
                    self.inner
                        .session
                        .set_session_name("")
                        .await
                        .map_err(SdkError::from_anyhow)?;
                }
                Ok(None)
            }
        }
    }

    /// Reject a session switch while a run OR a compaction owns the session.
    ///
    /// `is_streaming()` alone is not enough: a MANUAL compaction
    /// ([`Agent::compact`]) claims the session without setting the streaming
    /// flag, and swapping the storage under it would let the compaction append
    /// its summary entry to the replacement transcript (the compactor reads the
    /// session manager after the summarizer call). The same
    /// `is_streaming() || is_compacting()` gate the session host's
    /// `/reload` path uses.
    fn ensure_session_switch_allowed(&self, action: &str) -> Result<(), SdkError> {
        if self.is_streaming() {
            return Err(SdkError::Other(format!(
                "cannot {action} while a run is in flight"
            )));
        }
        if self.inner.session.is_compacting() {
            return Err(SdkError::Other(format!(
                "cannot {action} while a compaction is in flight"
            )));
        }
        Ok(())
    }

    /// Start a fresh session in the agent's session directory (upstream
    /// `newSession`): a new session file id/path, an empty transcript, and
    /// the same runtime, tools, and plugins. The previous session file is
    /// left untouched; [`Agent::usage_totals`] restarts from zero.
    ///
    /// Fails when the agent has no session storage
    /// ([`SessionMode::None`](crate::sdk::SessionMode::None)), when a run or a
    /// manual compaction is in flight (the transcript must not switch under a
    /// live run, and a compaction appends to the transcript it started
    /// against), or when a `session_before_switch` plugin handler cancels the
    /// switch.
    pub async fn new_session(&self) -> Result<(), SdkError> {
        self.ensure_session_switch_allowed("start a new session")?;
        let Some(session_dir) = self.inner.session_dir.clone() else {
            return Err(SdkError::Session(
                "session storage is disabled; new_session requires a persistent agent".to_owned(),
            ));
        };
        let cwd_text = self.inner.cwd.to_string_lossy();
        let manager = SessionManager::create(&cwd_text, &session_dir)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        // A background call of the OLD session must never write into the new
        // transcript (same guard the TUI's `/new` applies).
        self.inner.session.abort_all_background();
        if self.inner.session.replace_session_storage(manager).await {
            self.set_usage_totals(UsageSnapshot::default());
            Ok(())
        } else {
            Err(SdkError::Session(
                "the session switch was cancelled by a plugin".to_owned(),
            ))
        }
    }

    /// Attach an existing session file, replaying its transcript for the next
    /// prompt (the SDK equivalent of the TUI's `/resume` `OpenSession`: open
    /// the file, swap the live session storage, and continue from the loaded
    /// branch). The file is opened tail-first like the TUI/print hosts; a run
    /// widens a paged session before building its context, so no transcript
    /// is silently truncated.
    ///
    /// The switch also restores the session's recorded model and thinking
    /// level and recomputes [`Agent::usage_totals`] from the loaded history.
    /// The recorded settings are skipped for a model/thinking level the
    /// builder pinned explicitly (`.endpoint(...)`, `.model(...)`,
    /// `.with_registry(...)`, `.thinking_level(...)`) or one changed later
    /// through [`Agent::set_model`] / [`Agent::set_thinking_level`].
    ///
    /// Fails when a run or a manual compaction is in flight, when the agent
    /// has no session storage
    /// ([`SessionMode::None`](crate::sdk::SessionMode::None)), when the file
    /// cannot be opened, or when a `session_before_switch` plugin handler
    /// cancels the switch.
    pub async fn attach_session(&self, path: &Path) -> Result<(), SdkError> {
        self.ensure_session_switch_allowed("attach a session")?;
        if self.inner.session.session_manager().is_none() {
            return Err(SdkError::Session(
                "session storage is disabled; attach_session requires a persistent agent"
                    .to_owned(),
            ));
        }
        let manager = crate::coding_agent::session_resolve::open_session_tail_first(path)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        self.attach_manager(manager).await
    }

    /// Resume the most recent session for the agent's working directory
    /// (upstream `--continue` / `SessionManager::continue_recent`), or start a
    /// fresh one when the directory has no session yet. Like
    /// [`Agent::attach_session`], the switch restores the session's recorded
    /// model/thinking level (unless pinned) and recomputes the usage totals.
    ///
    /// Requires session storage; fails while a run or a manual compaction is in
    /// flight or when a `session_before_switch` plugin handler cancels the
    /// switch.
    pub async fn continue_session(&self) -> Result<(), SdkError> {
        self.ensure_session_switch_allowed("continue a session")?;
        if self.inner.session.session_manager().is_none() {
            return Err(SdkError::Session(
                "session storage is disabled; continue_session requires a persistent agent"
                    .to_owned(),
            ));
        }
        let Some(session_dir) = self.inner.session_dir.clone() else {
            return Err(SdkError::Session(
                "session storage is disabled; continue_session requires a persistent agent"
                    .to_owned(),
            ));
        };
        let cwd_text = self.inner.cwd.to_string_lossy();
        let manager = SessionManager::continue_recent(&session_dir, &cwd_text)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        self.attach_manager(manager).await
    }

    /// Replace the live session with an empty in-memory transcript
    /// (`SessionMode::None` semantics): the current branch is dropped and
    /// nothing is written to disk. The agent keeps its runtime, tools, and
    /// plugins; [`Agent::usage_totals`] restarts from zero.
    ///
    /// Requires session storage (a `SessionMode::None` agent never has a
    /// transcript to reset); fails while a run or a manual compaction is in
    /// flight or when a `session_before_switch` plugin handler cancels the
    /// switch.
    pub async fn reset(&self) -> Result<(), SdkError> {
        self.ensure_session_switch_allowed("reset a session")?;
        if self.inner.session.session_manager().is_none() {
            return Err(SdkError::Session(
                "session storage is disabled; reset requires a persistent agent".to_owned(),
            ));
        }
        let cwd_text = self.inner.cwd.to_string_lossy();
        let manager = SessionManager::in_memory(&cwd_text, None)
            .map_err(|error| SdkError::Session(error.to_string()))?;
        // A background call of the OLD session must never write into the
        // fresh transcript.
        self.inner.session.abort_all_background();
        if self.inner.session.replace_session_storage(manager).await {
            self.set_usage_totals(UsageSnapshot::default());
            Ok(())
        } else {
            Err(SdkError::Session(
                "the session switch was cancelled by a plugin".to_owned(),
            ))
        }
    }

    /// Shared attach body: recompute the loaded session's totals and restore
    /// its recorded model/thinking level, then swap the live storage.
    async fn attach_manager(&self, manager: SessionManager) -> Result<(), SdkError> {
        // The branch may record a VIRTUAL model (`record_virtual_selection`),
        // whose catalog entry only exists once its persisted
        // `pi.virtual-model-state` registration is in the registry overlay.
        // Restore that FIRST: looking the recorded selection up against an
        // overlay without it silently kept the current model.
        let declared_virtual = crate::agent_session::restore_virtual_models_from(
            self.inner.session.provider_registry().as_ref(),
            &manager,
        );
        // Read the recorded settings and totals BEFORE the swap: a paged
        // session streams its skipped prefix one entry at a time, so this
        // never materializes the whole transcript.
        let projection = manager.build_session_projection().ok();
        let recorded_model = projection
            .as_ref()
            .and_then(|projection| projection.model.clone());
        // `get_session_context_settings` defaults the level to "off" when the
        // branch never recorded one; only an explicit change entry may
        // override the agent's current level.
        let recorded_thinking = session_recorded_thinking(&manager);
        let totals = session_history_totals(&manager);
        let session_id = manager.id().to_owned();
        // Validate + resolve BEFORE the swap: a resumed branch whose recorded
        // virtual model cannot be rebuilt must fail with nothing changed.
        let mut restored_model = None;
        if !self.inner.model_pinned.load(Ordering::SeqCst) {
            if let Some((provider, model_id)) = recorded_model {
                let key_matches_provider = self
                    .inner
                    .api_key_provider
                    .as_deref()
                    .is_none_or(|key_provider| key_provider == provider);
                if key_matches_provider {
                    match self.inner.session.find_model(&provider, &model_id) {
                        Some(model) => restored_model = Some(model),
                        // A recorded VIRTUAL model this process cannot rebuild is
                        // an error, never a silent stay-on-the-previous-model.
                        None => {
                            if declared_virtual
                                .iter()
                                .any(|(p, id)| p == &provider && id == &model_id)
                            {
                                return Err(SdkError::NoModel {
                                    message: format!(
                                        "session {session_id} records the virtual model \
                                         `{provider}/{model_id}`, which cannot be restored: no \
                                         `{entry}` registration for it on the branch (re-install \
                                         the extension that registered it)",
                                        entry = crate::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY,
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }
        // A background call of the old session must never write into the
        // attached transcript.
        self.inner.session.abort_all_background();
        if !self.inner.session.replace_session_storage(manager).await {
            return Err(SdkError::Session(
                "the session switch was cancelled by a plugin".to_owned(),
            ));
        }
        self.set_usage_totals(totals);
        if let Some(model) = restored_model {
            // `"restore"`, not `"set"`: a resume's recorded selection is the
            // upstream `_emitModelSelect(..., "restore")` case a plugin/host
            // keys on, exactly like the TUI's `/resume` path. `set_model`
            // would report a resume as a user-initiated switch.
            self.inner
                .session
                .restore_model(model)
                .await
                .map_err(SdkError::from_anyhow)?;
        }
        // Apply the model first: the thinking level is clamped against it.
        //
        // The clamp runs even when the branch records no explicit level (and
        // when the level is pinned): the CURRENT level may be one the
        // restored model does not support, and `Agent::thinking_level`
        // promises the EFFECTIVE, clamped level. Re-applying the current value
        // is a no-op when it is still supported (the mutator only records and
        // emits on an effective change), so a recorded level simply wins over
        // it when one exists.
        let requested_level = if self.inner.thinking_pinned.load(Ordering::SeqCst) {
            None
        } else {
            recorded_thinking
        }
        .unwrap_or_else(|| self.inner.session.state().thinking);
        self.inner.session.set_thinking_level(requested_level).await;
        Ok(())
    }

    /// Point [`Agent::usage_totals`] at `totals` for the current session by
    /// offsetting the facade's monotonic counters.
    fn set_usage_totals(&self, totals: UsageSnapshot) {
        let state = self.inner.session.state();
        let mut offset = self
            .inner
            .usage_offset
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        offset.turns = totals.turns as i128 - state.turns as i128;
        offset.tokens = totals.total_tokens as i128 - state.tokens as i128;
    }

    /// Session-level usage totals across every prompt run of the CURRENT
    /// session.
    ///
    /// [`UsageSnapshot::turns`] counts model round-trips, not prompts: a
    /// prompt that calls a tool and then receives a final answer contributes
    /// two turns. A session switch restarts the totals
    /// ([`Agent::new_session`] / [`Agent::reset`]) or replaces them with the
    /// loaded history's totals ([`Agent::attach_session`] /
    /// [`Agent::continue_session`]).
    pub fn usage_totals(&self) -> UsageSnapshot {
        let state = self.inner.session.state();
        let offset = *self
            .inner
            .usage_offset
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let turns = (state.turns as i128 + offset.turns).max(0);
        let tokens = (state.tokens as i128 + offset.tokens).max(0);
        UsageSnapshot {
            turns: usize::try_from(turns).unwrap_or(usize::MAX),
            total_tokens: u64::try_from(tokens).unwrap_or(u64::MAX),
        }
    }

    /// The model currently in use (`provider` + `id` snapshot).
    pub fn model(&self) -> ModelRef {
        let state = self.inner.session.state();
        ModelRef {
            provider: state.provider,
            id: state.model,
        }
    }

    /// The thinking level currently in effect (the clamped, applied level —
    /// see [`Agent::set_thinking_level`]).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use pir::sdk::AgentBuilder;
    ///
    /// # let agent = AgentBuilder::new().no_session().build().await?;
    /// println!("{}", agent.thinking_level().wire_key());
    /// # Ok(())
    /// # }
    /// ```
    pub fn thinking_level(&self) -> crate::ai::types::ThinkingLevel {
        self.inner.session.state().thinking
    }

    /// Switch the model for subsequent runs (upstream `types.ts`
    /// `AgentSession.setModel`): the change applies to the next prompt run
    /// and is recorded on the session branch when the model actually
    /// changes. The model must be resolvable from the same provider registry
    /// the agent was built with; the switch itself is validated by the
    /// session (a model change is always accepted, an unknown id simply
    /// never routes).
    ///
    /// The current thinking level is re-applied to the new model and clamped
    /// to what it supports (upstream `_getThinkingLevelForModelSwitch`), so
    /// `thinking_level()` reports the effective level after the switch (for
    /// example `off` when the new model does not reason).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use pir::sdk::AgentBuilder;
    ///
    /// # let agent = AgentBuilder::new().no_session().build().await?;
    /// # let other = pir::ai::providers::construction::model(
    /// #     "provider", "other-model", "other-model", "openai-completions",
    /// #     "http://localhost:11434/v1", 128_000, 8_192, vec!["text"],
    /// # );
    /// agent.set_model(other).await?;
    /// assert_eq!(agent.model().id, "other-model");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_model(&self, model: crate::ai::types::Model) -> Result<(), SdkError> {
        self.inner
            .session
            .set_model(model)
            .await
            .map_err(SdkError::from_anyhow)?;
        // Upstream `AgentSession.setModel` re-applies the thinking level for
        // the new model (its `_getThinkingLevelForModelSwitch`), so switching
        // from a reasoning model to one that does not reason clamps the
        // effective level to `off` instead of leaving the previous level
        // active. Without this, `Agent::thinking_level` kept reporting a level
        // the current model does not support.
        let current = self.inner.session.state().thinking;
        self.inner.session.set_thinking_level(current).await;
        // An explicit runtime switch is a pin: a later attach must not
        // replace it with the attached session's recorded model.
        self.inner.model_pinned.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Set the thinking level for subsequent runs (upstream `types.ts`
    /// `AgentSession.setThinkingLevel`); returns the effective level after
    /// clamping to what the current model supports (`off` for non-reasoning
    /// models).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), pir::sdk::SdkError> {
    /// use pir::ai::types::ThinkingLevel;
    /// use pir::sdk::AgentBuilder;
    ///
    /// # let agent = AgentBuilder::new().no_session().build().await?;
    /// let effective = agent.set_thinking_level(ThinkingLevel::High).await?;
    /// assert_eq!(agent.thinking_level(), effective);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_thinking_level(
        &self,
        level: crate::ai::types::ThinkingLevel,
    ) -> Result<crate::ai::types::ThinkingLevel, SdkError> {
        let effective = self.inner.session.set_thinking_level(level).await;
        // An explicit runtime switch is a pin: a later attach must not
        // replace it with the attached session's recorded level.
        self.inner.thinking_pinned.store(true, Ordering::SeqCst);
        Ok(effective)
    }

    /// Names of the native (Rust backend) plugin slash commands registered
    /// on the session's plugin host, with their registry descriptions:
    /// `(invocation name, description)` pairs, sorted by name. Both the plain
    /// name and the deduped `name:2`/`name:3` invocation spellings are
    /// included. JS-extension commands are not listed here (the SDK has no
    /// extension command registry surface); builtins such as `/todos`,
    /// `/phone-start`, and `/mcp` are.
    ///
    /// This is the SDK counterpart of the TUI's native-command snapshot; feed
    /// a name to [`Agent::dispatch_command`] to run it.
    pub fn commands(&self) -> Vec<(String, Option<String>)> {
        self.inner.session.native_plugin_command_specs()
    }

    /// Dispatch one native plugin slash command by name through the session's
    /// plugin host (upstream `PluginHost::dispatch_command`): a Rust command
    /// runs natively, anything else falls back to the JS runtime.
    ///
    /// The outcome maps the way print mode's command path maps it: native
    /// commands return their display text through
    /// [`CommandOutcome::error`](crate::extensions::CommandOutcome::error)
    /// even when `ok` is true (the `/todos` list, `/phone-start`'s URL), so
    /// callers report that text for both success and failure.
    ///
    /// Fails when no plugin registers `name` (the same "not registered"
    /// error the TUI's worker reports).
    pub async fn dispatch_command(
        &self,
        name: &str,
        args: &str,
    ) -> Result<crate::extensions::CommandOutcome, SdkError> {
        self.inner
            .session
            .dispatch_plugin_command(name, args)
            .await
            .map_err(SdkError::from_anyhow)
    }

    /// The transcript as the session sees it (upstream `session.messages`):
    /// the current branch projection for persisted sessions, the in-memory
    /// history for ephemeral ones.
    ///
    /// A session opened PAGED (a huge file: [`Agent::attach_session`] opens
    /// tail-first) returns only its resident window here. That is deliberate —
    /// materializing a 100 MB session to read the newest messages would defeat
    /// paged mode. Walk the skipped prefix with
    /// [`Agent::transcript_cursor`] / [`Agent::older_transcript_messages`],
    /// which stream one bounded page at a time without requiring the whole
    /// session resident.
    pub async fn transcript(&self) -> Vec<crate::ai::types::Message> {
        self.inner.session.transcript().await
    }

    /// Whether the attached session holds only a resident tail window (a
    /// large session opened by [`Agent::attach_session`] /
    /// [`Agent::continue_session`]).
    pub async fn is_paged_session(&self) -> bool {
        match self.inner.session.session_manager() {
            Some(manager) => manager.lock().await.is_paged(),
            None => false,
        }
    }

    /// The paged-open window of the attached session, or `None` when the whole
    /// transcript is resident.
    ///
    /// Complements [`Agent::is_paged_session`] for consumers that size their
    /// "load older" affordance: [`SessionManager::paged_window`] reports the
    /// resident window's start byte, the file length at open, and how many
    /// entries stayed on disk. Reading it never widens the session; walk the
    /// disk side with [`Agent::transcript_cursor`] /
    /// [`Agent::older_transcript_messages`].
    pub async fn paged_window(&self) -> Option<crate::session_manager::PagedWindow> {
        match self.inner.session.session_manager() {
            Some(manager) => manager.lock().await.paged_window(),
            None => None,
        }
    }

    /// The cursor [`Agent::older_transcript_messages`] continues from: the
    /// oldest resident entry of a paged session, `None` otherwise.
    pub async fn transcript_cursor(&self) -> Option<u64> {
        self.inner.session.older_transcript_cursor().await
    }

    /// ONE OLDER page of a PAGED session's skipped transcript prefix, in
    /// branch-projection form, plus the cursor to continue from.
    ///
    /// `cursor` comes from [`Agent::transcript_cursor`] (or the continuation
    /// cursor a previous call returned) and moves strictly older with every
    /// call. `None` means "not a paged session" or "reached the file start".
    /// The page is parsed with the caller's byte/entry caps, so reading the
    /// whole transcript never requires it resident at once.
    pub async fn older_transcript_messages(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> Result<Option<(Vec<crate::ai::types::Message>, u64)>, SdkError> {
        self.inner
            .session
            .older_transcript_messages(cursor, max_bytes, max_entries)
            .await
            .map_err(SdkError::from_anyhow)
    }

    /// [`Agent::older_transcript_messages`] with the session layer's default
    /// page caps ([`crate::session_manager::DEFAULT_TAIL_PAGE_BYTES`] /
    /// [`crate::session_manager::DEFAULT_TAIL_PAGE_ENTRIES`]) — the same caps
    /// the tail-first open uses, so "load older" pages match the resident
    /// window size. Pass explicit caps to
    /// [`Agent::older_transcript_messages`] when the consumer wants a
    /// different granularity; the paging itself stays in `SessionManager`.
    pub async fn next_older_page(
        &self,
        cursor: u64,
    ) -> Result<Option<(Vec<crate::ai::types::Message>, u64)>, SdkError> {
        self.older_transcript_messages(
            cursor,
            crate::session_manager::DEFAULT_TAIL_PAGE_BYTES,
            crate::session_manager::DEFAULT_TAIL_PAGE_ENTRIES,
        )
        .await
    }

    /// [`Agent::transcript_json`] over ONE OLDER page of a paged session's
    /// skipped prefix: the same `AgentMessage` JSON shape, plus the cursor to
    /// continue from.
    ///
    /// [`Agent::transcript_json`] returns the resident window of a paged
    /// session (the whole transcript otherwise), so a JSON consumer — an RPC
    /// mirror, an export, a language binding — needs this companion to reach
    /// the skipped prefix without widening the session. Semantics match
    /// [`Agent::older_transcript_messages`] exactly (`None` when the session
    /// is not paged or the cursor is the file start; the returned cursor
    /// moves strictly older).
    pub async fn older_transcript_json(
        &self,
        cursor: u64,
        max_bytes: usize,
        max_entries: usize,
    ) -> Result<Option<(Vec<serde_json::Value>, u64)>, SdkError> {
        let page = self
            .older_transcript_messages(cursor, max_bytes, max_entries)
            .await?;
        Ok(page.map(|(messages, next)| {
            let json = messages
                .iter()
                .map(|message| serde_json::to_value(message).unwrap_or(serde_json::Value::Null))
                .collect();
            (json, next)
        }))
    }

    /// [`Agent::transcript_json`] serialized to the upstream `AgentMessage` JSON
    /// shape.
    ///
    /// A PAGED session returns its resident window here, exactly like
    /// [`Agent::transcript`]; walk the skipped prefix with
    /// [`Agent::transcript_cursor`] / [`Agent::older_transcript_json`] (or
    /// [`Agent::older_transcript_messages`] for typed messages).
    pub async fn transcript_json(&self) -> Vec<serde_json::Value> {
        self.inner.session.transcript_json().await
    }

    /// The in-memory transcript of an ephemeral (no-session-storage) agent:
    /// every user, assistant, and tool-result message appended so far, in run
    /// order. Returns an empty vector for a persisted session — read
    /// [`Agent::transcript`] there.
    pub async fn ephemeral_transcript(&self) -> Vec<crate::ai::types::Message> {
        self.inner.session.ephemeral_transcript().await
    }

    /// Aggregate token/cost/message totals over the whole session (upstream
    /// `getSessionStats`), including history compaction removed.
    ///
    /// PAGED sessions (a huge file opened tail-first): this does NOT widen the
    /// session. The skipped prefix is streamed entry by entry
    /// ([`SessionManager::for_each_prefix_entry`], bounded memory) and folded
    /// into the same totals the resident window contributes, so the numbers
    /// match a full load without ever requiring the whole transcript resident.
    pub async fn session_stats(&self) -> SessionStats {
        let Some(manager) = self.inner.session.session_manager() else {
            return self.inner.session.session_stats().await;
        };
        // Stream the skipped prefix without holding the session lock across
        // the resident read (the file scan re-opens the session file, so it
        // must not run under the manager mutex).
        let prefix = {
            let manager = manager.lock().await;
            if manager.is_paged() {
                let mut stats = SessionStats {
                    session_file: self
                        .inner
                        .session
                        .session_path()
                        .map(|path| path.to_string_lossy().into_owned()),
                    session_id: self.inner.session.session_id().unwrap_or_default(),
                    user_messages: 0,
                    assistant_messages: 0,
                    tool_calls: 0,
                    tool_results: 0,
                    total_messages: 0,
                    tokens: crate::agent_session::SessionTokens::default(),
                    cost: 0.0,
                    context_usage: None,
                };
                let _ = manager.for_each_prefix_entry(|entry| {
                    fold_entry_stats(&mut stats, &entry);
                });
                Some(stats)
            } else {
                None
            }
        };
        let mut stats = self.inner.session.session_stats().await;
        if let Some(prefix) = prefix {
            stats.user_messages += prefix.user_messages;
            stats.assistant_messages += prefix.assistant_messages;
            stats.tool_calls += prefix.tool_calls;
            stats.tool_results += prefix.tool_results;
            stats.total_messages += prefix.total_messages;
            stats.tokens.input = stats.tokens.input.saturating_add(prefix.tokens.input);
            stats.tokens.output = stats.tokens.output.saturating_add(prefix.tokens.output);
            stats.tokens.cache_read = stats
                .tokens
                .cache_read
                .saturating_add(prefix.tokens.cache_read);
            stats.tokens.cache_write = stats
                .tokens
                .cache_write
                .saturating_add(prefix.tokens.cache_write);
            stats.tokens.total = stats
                .tokens
                .input
                .saturating_add(stats.tokens.output)
                .saturating_add(stats.tokens.cache_read)
                .saturating_add(stats.tokens.cache_write);
            stats.cost += prefix.cost;
        }
        stats
    }

    /// Context-window usage of the current transcript (upstream
    /// `getContextUsage`), or `None` when the model reports no window.
    pub async fn context_usage(&self) -> Option<ContextUsage> {
        self.inner.session.context_usage().await
    }

    /// Drop every queued steering/follow-up message (upstream `clearQueue`)
    /// and report what was dropped.
    pub async fn clear_queue(&self) -> ClearedQueue {
        self.inner.session.clear_queue().await
    }

    /// Every queued-but-not-yet-delivered message, as
    /// `(steering, follow_up)` in queue order (upstream
    /// `peekQueuedMessages`; peek semantics, nothing is drained).
    pub async fn queued_messages(&self) -> (Vec<String>, Vec<String>) {
        (
            self.inner.session.get_steering_messages().await,
            self.inner.session.get_follow_up_messages().await,
        )
    }

    /// The queued-but-not-yet-delivered steering messages, in queue order
    /// (peek semantics: nothing is drained).
    pub async fn steering_messages(&self) -> Vec<String> {
        self.inner.session.get_steering_messages().await
    }

    /// The queued-but-not-yet-delivered follow-up messages, in queue order
    /// (peek semantics: nothing is drained).
    pub async fn follow_up_messages(&self) -> Vec<String> {
        self.inner.session.get_follow_up_messages().await
    }

    /// The live steering queue mode (upstream `session.steeringMode`).
    pub fn steering_mode(&self) -> SteeringMode {
        self.inner.session.steering_mode()
    }

    /// Set the steering queue mode (upstream `setSteeringMode`); applies from
    /// the next drain on, including inside a running prompt.
    pub fn set_steering_mode(&self, mode: SteeringMode) {
        self.inner.session.set_steering_mode(mode);
    }

    /// The live follow-up queue mode (upstream `session.followUpMode`).
    pub fn follow_up_mode(&self) -> FollowUpMode {
        self.inner.session.follow_up_mode()
    }

    /// Set the follow-up queue mode (upstream `setFollowUpMode`).
    pub fn set_follow_up_mode(&self, mode: FollowUpMode) {
        self.inner.session.set_follow_up_mode(mode);
    }

    /// Wait until no prompt run is in flight (upstream `agent.waitForIdle`).
    /// Returns immediately when the agent is already idle.
    pub async fn wait_for_idle(&self) {
        while self.is_streaming() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Shut the agent down: abort any in-flight run and every background tool
    /// call, wait for the run to settle (bounded — a run wedged inside a tool
    /// that ignores the abort signal fails the shutdown with
    /// [`SdkError::Other`] instead of hanging forever), mark the agent shut
    /// down (later prompts are rejected), and tear the session's plugin
    /// surface down through [`AgentSession::shutdown`] /
    /// [`crate::plugins::PluginHost::shutdown`]: every builtin and inline
    /// plugin (including the ones from `extension_paths`) gets
    /// `session_shutdown {reason: "quit"}` and its `on_shutdown` hook exactly
    /// once, and the JS extension runtime emits the same event to its handlers
    /// before its actor thread exits. This is the explicit teardown; `Drop`
    /// performs no async work (channels and the session close naturally when
    /// the last handle is dropped).
    pub async fn shutdown(&self) -> Result<(), SdkError> {
        self.inner.shut_down.store(true, Ordering::SeqCst);
        // A detached background call outlives its turn, never its session.
        self.inner.session.abort_all_background();
        self.abort();
        let deadline = std::time::Instant::now() + SHUTDOWN_SETTLE_TIMEOUT;
        let mut settle_error = None;
        // A manual compaction is not a prompt run (`is_streaming()` stays
        // false) but it still owns the session and the summarizer stream:
        // tearing the plugin surface down under it is the same race a run
        // would be. `abort()` above already signalled it (SESSION_COMPACTING is
        // one of the states it records the request for).
        while self.is_streaming() || self.inner.session.is_compacting() {
            if std::time::Instant::now() >= deadline {
                settle_error = Some(SdkError::Other(format!(
                    "shutdown timed out after {}s: the in-flight run or compaction did not settle",
                    SHUTDOWN_SETTLE_TIMEOUT.as_secs()
                )));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // The session owns the ONE teardown path: it emits
        // `session_shutdown` to the native plugins, runs every `on_shutdown`
        // hook exactly once (the `PluginHost` one-shot guard), and reaches
        // builtins instantiated from `extension_paths`. Plugin hook failures
        // are diagnosed there; a timed-out settle still tears down first.
        self.inner.session.shutdown().await;
        match settle_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Fold ONE stored entry into a [`SessionStats`] accumulator, mirroring the
/// `SessionStats` walk in [`crate::agent_session`] (message roles, tool calls,
/// usage records). Used for a paged session's skipped prefix, so
/// [`Agent::session_stats`] reports whole-session totals without widening.
fn fold_entry_stats(stats: &mut SessionStats, entry: &FileEntry) {
    if entry.is_header() {
        return;
    }
    let add_usage = |stats: &mut SessionStats, usage: &Usage| {
        stats.tokens.input = stats.tokens.input.saturating_add(usage.input);
        stats.tokens.output = stats.tokens.output.saturating_add(usage.output);
        stats.tokens.cache_read = stats.tokens.cache_read.saturating_add(usage.cache_read);
        stats.tokens.cache_write = stats.tokens.cache_write.saturating_add(usage.cache_write);
        stats.cost += usage.cost.total;
    };
    match entry {
        FileEntry::Message(message_entry) => {
            stats.total_messages += 1;
            match &message_entry.message {
                StoredMessage::User(user) => {
                    if matches!(user.role, crate::ai::types::UserRole::User) {
                        stats.user_messages += 1;
                    }
                }
                StoredMessage::Custom(_) => stats.user_messages += 1,
                StoredMessage::BashExecution(bash) => {
                    if !bash.excluded() {
                        stats.user_messages += 1;
                    }
                }
                StoredMessage::Assistant(assistant) => {
                    stats.assistant_messages += 1;
                    stats.tool_calls += assistant
                        .content
                        .iter()
                        .filter(|block| {
                            matches!(block, crate::ai::types::ContentBlock::ToolCall(_))
                        })
                        .count();
                    add_usage(stats, &assistant.usage);
                }
                StoredMessage::ToolResult(result) => {
                    stats.tool_results += 1;
                    if let Some(usage) = &result.usage {
                        add_usage(stats, usage);
                    }
                }
                StoredMessage::System(_) => {}
            }
        }
        FileEntry::Usage(usage_entry) => add_usage(stats, &usage_entry.usage),
        FileEntry::Compaction(compaction_entry) => {
            if let Some(usage) = &compaction_entry.usage {
                add_usage(stats, usage);
            }
        }
        FileEntry::BranchSummary(summary_entry) => {
            if let Some(usage) = &summary_entry.usage {
                add_usage(stats, usage);
            }
        }
        _ => {}
    }
}

/// Token contribution of one usage record, matching
/// [`crate::agent_session::SessionStats`]'s aggregation
/// (input + output + cache read + cache write).
fn usage_contribution(usage: &Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.output)
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write)
}

/// Fold one stored entry into the running historical totals. Mirrors the
/// `session_stats` walk: assistant messages are turns (model round-trips) and
/// every assistant / tool-result / usage / summary record contributes tokens.
fn accumulate_entry_totals(entry: &FileEntry, turns: &mut usize, tokens: &mut u64) {
    match entry {
        FileEntry::Message(message_entry) => match &message_entry.message {
            StoredMessage::Assistant(assistant) => {
                *turns = turns.saturating_add(1);
                *tokens = tokens.saturating_add(usage_contribution(&assistant.usage));
            }
            StoredMessage::ToolResult(result) => {
                if let Some(usage) = &result.usage {
                    *tokens = tokens.saturating_add(usage_contribution(usage));
                }
            }
            _ => {}
        },
        FileEntry::Usage(usage_entry) => {
            *tokens = tokens.saturating_add(usage_contribution(&usage_entry.usage));
        }
        FileEntry::Compaction(compaction_entry) => {
            if let Some(usage) = &compaction_entry.usage {
                *tokens = tokens.saturating_add(usage_contribution(usage));
            }
        }
        FileEntry::BranchSummary(summary_entry) => {
            if let Some(usage) = &summary_entry.usage {
                *tokens = tokens.saturating_add(usage_contribution(usage));
            }
        }
        _ => {}
    }
}

/// The thinking level recorded on the session's active branch, when the
/// branch carries an explicit `thinking_level_change` entry.
///
/// `get_session_context_settings` reports `"off"` as its default for a branch
/// with no entry; applying that default would silently disable thinking on
/// every resumed session, so absence is reported as `None` here.
pub(crate) fn session_recorded_thinking(
    manager: &SessionManager,
) -> Option<crate::ai::types::ThinkingLevel> {
    let path = crate::session_manager::projection::build_session_path(
        manager.resident_entries(),
        manager.leaf_id(),
    );
    path.iter().rev().find_map(|entry| match entry {
        FileEntry::ThinkingLevelChange(change) => change
            .thinking_level
            .parse::<crate::ai::types::ThinkingLevel>()
            .ok(),
        _ => None,
    })
}

/// Recompute a session's historical turn/token totals without materializing
/// its transcript: a paged session streams its skipped prefix one entry at a
/// time (bounded memory), then walks the resident window.
pub(crate) fn session_history_totals(manager: &SessionManager) -> UsageSnapshot {
    let mut turns = 0usize;
    let mut tokens = 0u64;
    let _ = manager
        .for_each_prefix_entry(|entry| accumulate_entry_totals(&entry, &mut turns, &mut tokens));
    for entry in manager.resident_entries() {
        accumulate_entry_totals(entry.as_ref(), &mut turns, &mut tokens);
    }
    UsageSnapshot {
        turns,
        total_tokens: tokens,
    }
}

/// Metadata for one tool the agent can use or declare (the snapshot
/// [`Agent::tools`] returns).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    /// Tool name as the model sees it.
    pub name: String,
    /// Model-facing description.
    pub description: String,
    /// JSON Schema of the tool's parameters, when it declares one.
    pub parameters: Option<serde_json::Value>,
}

/// Usage totals across the session's prompt runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageSnapshot {
    /// Loop turns executed, i.e. model round-trips across every prompt run —
    /// not the number of prompts. One prompt that calls a tool and then gets
    /// a final answer counts as two turns.
    pub turns: usize,
    /// Total tokens reported by assistant usage records so far.
    pub total_tokens: u64,
}

/// The model currently backing an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    /// Provider id (`openai`, `anthropic`, a custom id, ...).
    pub provider: String,
    /// Model id within the provider.
    pub id: String,
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.id)
    }
}
