//! D32 (`6f2f164ab`, #10887): a resumed session restores the active-tool
//! loadout its transcript records, with the selection's `+name`/`-name`
//! entries applied — and keeps the explicit-selection / no-record cases on the
//! settings/CLI base.
//!
//! Upstream's `default-tools-setting.test.ts` `resume` block is the model:
//! `createRecordedSession` appends the system message a run records
//! (`toolsAdded`), then the session is created over that transcript and its
//! active tool set is asserted. Here the same transcript is built through
//! [`SessionManager::append_message`] (which now persists the declaration with
//! its tools, see `session_manager::types`) and the active set is read from the
//! tool list of the run's provider request — the same observable the
//! `d23_reach_consumption` suite uses.
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent, ThinkingLevel, Tool, Usage, UserMessage, UserRole,
};
use pir::coding_agent::tool_selection::resolve_tool_selection;
use pir::coding_agent::ToolSelection;
use pir::config::settings::DEFAULT_TOOL_NAMES;
use pir::session_manager::SessionManager;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-d32-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 8192,
        max_tokens: 512,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

/// A core-style tool: direct exposure, activated by registration.
struct NamedTool {
    name: &'static str,
}

impl AgentTool for NamedTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "d32 test tool"
    }
    fn parameters(&self) -> Option<Value> {
        Some(json!({ "type": "object" }))
    }
    fn run(&self, _args: &Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ok") })
    }
}

/// A provider answering every stream with a final text message and recording
/// the tool names each request asked for.
struct CapturingProvider {
    calls: AtomicUsize,
    captured: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Provider for CapturingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(
                context
                    .tools
                    .as_ref()
                    .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                    .unwrap_or_default(),
            );
        let message = AssistantMessage {
            api: "fake".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: "done".into(),
                text_signature: None,
            })],
            stop_reason: StopReason::Stop,
            timestamp: 1,
            usage: Usage::default(),
            ..AssistantMessage::default()
        };
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

/// The system message a run records when it declares its loadout (upstream
/// `declareToolChanges` / `createRecordedSession`): `toolsAdded` on a system
/// message, no content.
fn declaration(names: &[&str]) -> Message {
    Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: Some(
            names
                .iter()
                .map(|name| Tool {
                    name: (*name).to_owned(),
                    description: format!("{name} tool"),
                    parameters: json!({ "type": "object" }),
                    constrained_sampling: None,
                })
                .collect(),
        ),
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

/// The transcript a resumed session starts from: one recorded loadout plus a
/// user turn.
fn recorded_storage(cwd: &Path, names: &[&str]) -> SessionManager {
    let mut storage =
        SessionManager::in_memory(&cwd.display().to_string(), None).expect("in-memory session");
    storage.append_message(declaration(names)).expect("append");
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 2,
            sections: None,
        }))
        .expect("append");
    storage
}

struct Fixture {
    session: Arc<AgentSession>,
    captured: Arc<Mutex<Vec<Vec<String>>>>,
}

fn session_with(
    cwd: &Path,
    registered: &[&'static str],
    storage: Option<SessionManager>,
    selection: Option<ToolSelection>,
) -> Fixture {
    let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CapturingProvider {
        calls: AtomicUsize::new(0),
        captured: Arc::clone(&captured),
    }));
    let mut tools = ToolRegistry::new();
    for name in registered {
        tools.register(Arc::new(NamedTool { name }));
    }
    let session = Arc::new(AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: storage,
        compaction: None,
        max_turns: 4,
        system_prompt: Some("base system prompt".into()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: selection,
    }));
    Fixture { session, captured }
}

/// Run one prompt and report the tool names the request declared, sorted (the
/// active set: `context.tools` is exactly the declared loadout).
async fn declared_tools(fixture: &Fixture) -> Vec<String> {
    fixture
        .session
        .prompt("go", Arc::new(|_| {}))
        .await
        .expect("prompt");
    let mut names = fixture
        .captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .unwrap_or_default();
    names.sort();
    names
}

fn select(
    settings_default_tools: Option<&[String]>,
    cli_tools: &[String],
    cli_exclude: &[String],
    available: &[String],
) -> ToolSelection {
    resolve_tool_selection(
        settings_default_tools,
        cli_tools,
        cli_exclude,
        false,
        false,
        available,
        &DEFAULT_TOOL_NAMES,
    )
    .expect("valid selection")
    .expect("a selection")
}

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|name| (*name).to_string()).collect()
}

const REGISTERED: &[&str] = &["read", "bash", "edit", "write", "grep"];

// ---------------------------------------------------------------------------
// Resume: the recorded loadout
// ---------------------------------------------------------------------------

/// Upstream "restores the loadout the transcript records": the session had
/// `read` and `write` active when it was left; the resumed session must start
/// from those, not from the full registry.
#[tokio::test(flavor = "current_thread")]
async fn resume_restores_the_recorded_loadout() {
    let dir = TempDir::new("recorded");
    let fixture = session_with(
        &dir.0,
        REGISTERED,
        Some(recorded_storage(&dir.0, &["read", "write"])),
        None,
    );
    assert_eq!(declared_tools(&fixture).await, names(&["read", "write"]));
}

/// Upstream "applies +name and -name tool options to the recorded loadout":
/// `--tools -bash +grep` removes `bash` from the recorded set and adds `grep`,
/// in that order.
#[tokio::test(flavor = "current_thread")]
async fn resume_applies_the_cli_modifiers_to_the_recorded_loadout() {
    let dir = TempDir::new("modifiers");
    let available = names(&["read", "bash", "edit", "write", "grep", "codeware"]);
    let selection = select(None, &names(&["-bash", "+grep"]), &[], &available);
    assert_eq!(selection.modifiers, names(&["-bash", "+grep"]));
    assert!(selection.uses_default_tools);
    let fixture = session_with(
        &dir.0,
        REGISTERED,
        Some(recorded_storage(&dir.0, &["read", "bash", "edit"])),
        Some(selection),
    );
    assert_eq!(
        declared_tools(&fixture).await,
        names(&["edit", "grep", "read"])
    );
}

/// A `+name` entry activates a recorded tool through the selection, and a
/// `-name` entry removes one that the recorded loadout had.
#[tokio::test(flavor = "current_thread")]
async fn resume_modifiers_both_directions_on_the_recorded_set() {
    let dir = TempDir::new("directions");
    let available = names(&["read", "bash", "edit", "write", "grep"]);
    let selection = select(None, &names(&["+write", "-read"]), &[], &available);
    assert_eq!(selection.modifiers, names(&["+write", "-read"]));
    let fixture = session_with(
        &dir.0,
        REGISTERED,
        Some(recorded_storage(&dir.0, &["read", "bash"])),
        Some(selection),
    );
    assert_eq!(declared_tools(&fixture).await, names(&["bash", "write"]));
}

/// The recorded loadout replaces a settings `defaultTools` base too (upstream:
/// `usesDefaultTools`), as long as the base's registry reaches the recorded
/// names.
#[tokio::test(flavor = "current_thread")]
async fn resume_replaces_the_settings_base_with_the_recorded_loadout() {
    let dir = TempDir::new("settings-base");
    let available = names(&["read", "bash", "edit", "write", "grep"]);
    let settings = names(&["read", "bash", "edit", "write"]);
    let selection = select(Some(&settings), &[], &[], &available);
    assert!(selection.uses_default_tools);
    let fixture = session_with(
        &dir.0,
        REGISTERED,
        Some(recorded_storage(&dir.0, &["read", "write"])),
        Some(selection),
    );
    assert_eq!(declared_tools(&fixture).await, names(&["read", "write"]));
}

/// Upstream "uses the default tools when the transcript records no loadout":
/// without a recorded entry the settings/CLI base stays in effect.
#[tokio::test(flavor = "current_thread")]
async fn resume_without_a_recorded_loadout_keeps_the_settings_base() {
    let dir = TempDir::new("no-record");
    let available = names(&["read", "bash", "edit", "write", "grep"]);
    let settings = names(&["read", "bash"]);
    let selection = select(Some(&settings), &[], &[], &available);
    let mut storage =
        SessionManager::in_memory(&dir.0.display().to_string(), None).expect("in-memory");
    storage
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append");
    let fixture = session_with(&dir.0, REGISTERED, Some(storage), Some(selection));
    assert_eq!(declared_tools(&fixture).await, names(&["bash", "read"]));
}

/// A session with no transcript at all (a fresh start) keeps the base, so the
/// restore never fires outside a resume.
#[tokio::test(flavor = "current_thread")]
async fn fresh_session_keeps_the_base() {
    let dir = TempDir::new("fresh");
    let fixture = session_with(&dir.0, REGISTERED, None, None);
    assert_eq!(declared_tools(&fixture).await, {
        let mut all = names(REGISTERED);
        all.sort();
        all
    });
}

// ---------------------------------------------------------------------------
// Explicit selections and exclusions win over the recorded loadout
// ---------------------------------------------------------------------------

/// Upstream "uses explicit tool options instead of the recorded loadout":
/// `--tools read,bash` selects explicitly, so the recorded set is ignored.
#[tokio::test(flavor = "current_thread")]
async fn explicit_tools_keep_the_base_instead_of_the_recorded_loadout() {
    let dir = TempDir::new("explicit");
    let available = names(&["read", "bash", "edit", "write", "grep"]);
    let selection = select(None, &names(&["read", "bash"]), &[], &available);
    assert!(!selection.uses_default_tools);
    assert!(selection.modifiers.is_empty());
    let fixture = session_with(
        &dir.0,
        REGISTERED,
        Some(recorded_storage(&dir.0, &["read", "edit"])),
        Some(selection),
    );
    assert_eq!(declared_tools(&fixture).await, names(&["bash", "read"]));
}

/// Upstream "does not restore excluded tools": `--exclude-tools grep` is not an
/// explicit selection, so the recorded loadout is restored — minus `grep`.
#[tokio::test(flavor = "current_thread")]
async fn excluded_tools_are_not_restored() {
    let dir = TempDir::new("excluded");
    let available = names(&["read", "bash", "edit", "write", "grep"]);
    let selection = select(None, &[], &names(&["grep"]), &available);
    assert!(selection.uses_default_tools);
    let fixture = session_with(
        &dir.0,
        REGISTERED,
        Some(recorded_storage(&dir.0, &["read", "grep"])),
        Some(selection),
    );
    assert_eq!(declared_tools(&fixture).await, names(&["read"]));
}

// ---------------------------------------------------------------------------
// The recorded entry is persisted (session file round trip)
// ---------------------------------------------------------------------------

/// The recorded loadout must survive the session file: a resumed session reads
/// it back from the stored declaration (`toolsAdded` on the system message) and
/// restores it.
#[tokio::test(flavor = "current_thread")]
async fn resume_from_a_persisted_session_file_restores_the_recorded_loadout() {
    let dir = TempDir::new("persisted");
    let path = {
        let mut storage =
            SessionManager::create(&dir.0.display().to_string(), &dir.0).expect("create session");
        storage
            .append_message(declaration(&["read", "bash"]))
            .expect("append");
        // The file is created on the first user/assistant message (upstream
        // `_hasConversation`), so the recording rides a real turn.
        storage
            .append_message(Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "hi".into(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 2,
                sections: None,
            }))
            .expect("append");
        storage.path().to_path_buf()
    };
    let reopened = SessionManager::open(&path).expect("reopen session");
    let fixture = session_with(&dir.0, REGISTERED, Some(reopened), None);
    assert_eq!(declared_tools(&fixture).await, names(&["bash", "read"]));
}
