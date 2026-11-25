use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pir_core::sdk::{Agent, AgentBuilder, SdkEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::watch;

/// A model reference exposed to the GUI without tying its wire state to the
/// SDK's internal metadata type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuiModelRef {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuiModelOption {
    pub provider: String,
    pub id: String,
    pub name: Option<String>,
    pub reasoning: bool,
    pub context_window: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuiSessionRow {
    pub path: String,
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub modified_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuiContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuiToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub args: JsonValue,
    pub partial: Option<JsonValue>,
    pub result: Option<JsonValue>,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    pub pending: bool,
}
/// A message rendered by the GUI conversation surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuiMessage {
    pub role: String,
    pub text: String,
    pub streaming: bool,
    pub thinking: bool,
}

/// The platform-neutral state consumed by a Native Shell view.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GuiState {
    pub messages: Vec<GuiMessage>,
    pub composer: String,
    pub model: Option<GuiModelRef>,
    pub running: bool,
    pub current_tool: Option<String>,
    pub current_tool_result: Option<JsonValue>,
    pub error: Option<String>,
    pub run_id: u64,
    pub aborting: bool,
    pub thinking_level: String,
    pub available_thinking_levels: Vec<String>,
    pub models: Vec<GuiModelOption>,
    pub sessions: Vec<GuiSessionRow>,
    pub context_usage: Option<GuiContextUsage>,
    pub tools: BTreeMap<String, GuiToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuiCommand {
    SetComposer {
        text: String,
    },
    Submit {
        text: String,
    },
    ListSessions,
    AttachSession {
        path: String,
    },
    NewSession,
    RenameSession {
        name: Option<String>,
    },
    LoadEarlier {
        before: Option<String>,
        count: usize,
    },
    SetThinkingLevel {
        level: String,
    },
    CycleModel,
    CycleThinking,
    Refresh,
    Compact,
    DispatchCommand {
        name: String,
        args: String,
    },
    Abort,
    Steer {
        text: String,
    },
    FollowUp {
        text: String,
    },
}

/// Notifications emitted after a command or SDK event changes the state.
///
/// `State` carries the whole [`GuiState`] by value on purpose: this event is
/// pushed through an `Arc`-shared `watch` channel, so every subscriber reads
/// one shared copy instead of receiving a per-subscriber clone. Boxing would
/// add an allocation on every state publish (the hot path here) to shrink a
/// value the channel already shares.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuiEvent {
    State { state: GuiState },
    Error { message: String },
}

/// Headless bridge used by both tests and a Native Shell host.
///
/// The bridge owns the SDK agent handle and exposes bounded UI notifications;
/// it never blocks a platform/main thread on the agent session. A desktop
/// adapter can serialize [`GuiEvent::State`] directly into its view model.
pub struct NativeShellBridge {
    agent: Agent,
    state: Arc<Mutex<GuiState>>,
    events: watch::Sender<GuiEvent>,
    active_run: Option<tokio::task::JoinHandle<()>>,
    next_run_id: u64,
}

impl NativeShellBridge {
    pub fn new(agent: Agent, events: watch::Sender<GuiEvent>) -> Self {
        let model = agent.model();
        let model_ref = GuiModelRef {
            provider: model.provider.clone(),
            id: model.id.clone(),
        };
        let models = agent
            .available_models()
            .into_iter()
            .map(|model| GuiModelOption {
                provider: model.provider.clone(),
                id: model.id.clone(),
                name: Some(model.name.clone()),
                reasoning: model.reasoning,
                context_window: model.context_window,
            })
            .collect();
        let thinking_levels = agent
            .available_thinking_levels()
            .into_iter()
            .map(|level| level.wire_key().to_owned())
            .collect();
        let thinking_level = agent.thinking_level().wire_key().to_owned();
        Self {
            agent,
            state: Arc::new(Mutex::new(GuiState {
                model: Some(model_ref),
                thinking_level,
                available_thinking_levels: thinking_levels,
                models,
                ..GuiState::default()
            })),
            events,
            active_run: None,
            next_run_id: 0,
        }
    }

    pub fn state(&self) -> GuiState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn refresh_metadata(&self) {
        let model = self.agent.model_info();
        let models = self
            .agent
            .available_models()
            .into_iter()
            .map(|model| GuiModelOption {
                provider: model.provider.clone(),
                id: model.id.clone(),
                name: Some(model.name.clone()),
                reasoning: model.reasoning,
                context_window: model.context_window,
            })
            .collect();
        let levels = self
            .agent
            .available_thinking_levels()
            .into_iter()
            .map(|level| level.wire_key().to_owned())
            .collect();
        let thinking_level = self.agent.thinking_level().wire_key().to_owned();
        self.with_state(|state| {
            state.model = Some(GuiModelRef {
                provider: model.provider,
                id: model.id,
            });
            state.models = models;
            state.available_thinking_levels = levels;
            state.thinking_level = thinking_level;
        });
        self.publish();
    }

    /// Handle a GUI command. Prompt execution is spawned so the caller can
    /// immediately return to its event loop.
    pub async fn command(&mut self, command: GuiCommand) {
        match command {
            GuiCommand::SetComposer { text } => {
                self.with_state(|state| state.composer = text);
                self.publish();
            }
            GuiCommand::Submit { text } => self.submit(text).await,
            GuiCommand::SetThinkingLevel { level } => {
                if let Ok(level) = level.parse() {
                    if let Err(error) = self.agent.set_thinking_level(level).await {
                        self.with_state(|state| state.error = Some(error.to_string()));
                        self.publish();
                    }
                }
            }
            GuiCommand::ListSessions
            | GuiCommand::AttachSession { .. }
            | GuiCommand::NewSession
            | GuiCommand::RenameSession { .. }
            | GuiCommand::LoadEarlier { .. } => {
                self.with_state(|state| {
                    state.error = Some("session command requires a persistent host".to_owned())
                });
                self.publish();
            }
            GuiCommand::CycleModel | GuiCommand::CycleThinking => {}
            GuiCommand::Refresh => self.publish(),
            GuiCommand::Compact => {
                if let Err(error) = self.agent.compact().await {
                    self.with_state(|state| state.error = Some(error.to_string()));
                    self.publish();
                }
            }
            GuiCommand::DispatchCommand { name, args } => {
                if let Err(error) = self.agent.dispatch_command(&name, &args).await {
                    self.with_state(|state| state.error = Some(error.to_string()));
                    self.publish();
                }
            }
            GuiCommand::Abort => {
                if self.state().running {
                    self.with_state(|state| state.aborting = true);
                    self.agent.abort();
                    self.publish();
                }
            }
            GuiCommand::Steer { text } => self.agent.steer(text),
            GuiCommand::FollowUp { text } => self.agent.follow_up(text),
        }
    }

    pub async fn submit(&mut self, text: impl Into<String>) {
        let text = text.into();
        if text.trim().is_empty() || self.state().running {
            return;
        }
        self.next_run_id = self.next_run_id.wrapping_add(1).max(1);
        let run_id = self.next_run_id;
        self.with_state(|state| {
            state.run_id = run_id;
            state.composer.clear();
            state.running = true;
            state.aborting = false;
            state.error = None;
            state.current_tool = None;
            state.current_tool_result = None;
            state.messages.push(GuiMessage {
                role: "user".to_owned(),
                text: text.clone(),
                streaming: false,
                thinking: false,
            });
        });
        self.publish();

        let agent = self.agent.clone();
        let state = Arc::clone(&self.state);
        let events = self.events.clone();
        self.active_run = Some(tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut stream = agent.prompt_stream_with_outcome(text);
            let mut settled = false;
            while let Some(event) = stream.next().await {
                if matches!(event, SdkEvent::AgentSettled { .. }) {
                    settled = true;
                }
                let mut snapshot = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if snapshot.run_id != run_id {
                    continue;
                }
                NativeShellBridge::reduce(&mut snapshot, event);
                let event = GuiEvent::State {
                    state: snapshot.clone(),
                };
                drop(snapshot);
                events.send_replace(event);
            }
            if !settled {
                let mut snapshot = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if snapshot.run_id == run_id {
                    snapshot.running = false;
                    snapshot.aborting = false;
                    snapshot.error = Some("agent stream ended before settlement".to_owned());
                    events.send_replace(GuiEvent::State {
                        state: snapshot.clone(),
                    });
                }
            }
        }));
    }

    fn with_state(&self, update: impl FnOnce(&mut GuiState)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        update(&mut state);
    }

    fn publish(&self) {
        self.events.send_replace(GuiEvent::State {
            state: self.state(),
        });
    }
    pub fn reduce(state: &mut GuiState, event: SdkEvent) {
        match event {
            SdkEvent::MessageStart => {
                state.running = true;
                state.messages.push(GuiMessage {
                    role: "assistant".to_owned(),
                    text: String::new(),
                    streaming: true,
                    thinking: false,
                });
            }
            SdkEvent::MessageUpdate { delta, thinking } => {
                if let Some(message) = state.messages.last_mut() {
                    if message.role == "assistant" && message.streaming {
                        message.thinking = thinking;
                        message.text.push_str(&delta);
                    }
                }
            }
            SdkEvent::MessageEnd { .. } => {
                if let Some(message) = state.messages.last_mut() {
                    if message.role == "assistant" {
                        message.streaming = false;
                    }
                }
            }
            SdkEvent::ToolExecutionStart {
                call_id,
                tool_name,
                args,
                ..
            } => {
                state.tools.insert(
                    call_id.clone(),
                    GuiToolCall {
                        call_id,
                        tool_name: tool_name.clone(),
                        args,
                        partial: None,
                        result: None,
                        is_error: false,
                        duration_ms: None,
                        pending: true,
                    },
                );
                state.current_tool = Some(tool_name);
            }
            SdkEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => {
                if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                    if tool.pending {
                        tool.partial = Some(partial.clone());
                    }
                }
                state.current_tool_result = Some(partial);
            }
            SdkEvent::ToolExecutionEnd {
                call_id,
                result,
                is_error,
                duration_ms,
                ..
            } => {
                if let Some(tool) = state.tools.get_mut(&call_id) {
                    tool.result = Some(result.clone());
                    tool.is_error = is_error;
                    tool.duration_ms = duration_ms;
                    tool.pending = false;
                }
                state.current_tool_result = Some(result);
                state.current_tool = None;
            }
            SdkEvent::ThinkingLevelChanged { level } => {
                state.thinking_level = level;
            }
            SdkEvent::QueueUpdate {
                steering,
                follow_up,
            } => {
                state.error = None;
                let _ = (steering, follow_up);
            }
            SdkEvent::CompactionStart { .. } => {}
            SdkEvent::CompactionEnd { error_message, .. } => {
                state.error = error_message;
            }
            SdkEvent::AgentSettled { .. } => {
                state.running = false;
                state.aborting = false;
                state.current_tool = None;
            }
            SdkEvent::AgentOutcome(outcome) => {
                state.running = true;
                if let Some(message) = state
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == "assistant")
                {
                    if let Some(text) = outcome.message_text() {
                        message.text = text;
                        message.streaming = false;
                    }
                }
            }
            SdkEvent::PromptRejected { message, .. } => {
                state.running = false;
                state.error = Some(message);
            }
            _ => {}
        }
    }

    pub fn reduce_if_current(state: &mut GuiState, run_id: u64, event: SdkEvent) -> bool {
        if state.run_id != run_id {
            return false;
        }
        Self::reduce(state, event);
        true
    }
}

/// A small JSON command/event codec for the NSH `nsh/host` channel.
#[derive(Debug, Default)]
pub struct GuiCodec {
    pub pending: BTreeMap<String, String>,
}

impl GuiCodec {
    pub fn decode_command(data: &JsonValue) -> Result<GuiCommand, serde_json::Error> {
        serde_json::from_value(data.clone())
    }

    pub fn encode_event(event: &GuiEvent) -> Result<JsonValue, serde_json::Error> {
        serde_json::to_value(event)
    }
}

/// Keep the bridge type explicitly sendable through the event loop boundary;
/// the SDK agent itself is cloneable and all mutable UI state remains local.
pub type SharedBridge = Arc<tokio::sync::Mutex<NativeShellBridge>>;
