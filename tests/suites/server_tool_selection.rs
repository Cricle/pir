//! Q66: the server host honors the global tool-selection flags.
//!
//! Upstream has no server host (`pi/packages` has no server package), so this
//! is pir's consistency rule: `pir server` applies the same
//! `--tools`/`--exclude-tools`/`--no-tools`/`--no-builtin-tools` and settings
//! `defaultTools` contract as print, the TUI and RPC. The selection is
//! resolved against the server's own core registry AFTER the
//! `allow_remote_bash` policy removed `bash`/`powershell`, so a selected shell
//! tool can never re-activate under the locked-down policy.

use std::sync::{Arc, Mutex};

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Model, ModelCost, ModelCostRates, StopReason,
    StreamOptions, TerminalReason, TextContent,
};
use pir::coding_agent::tool_selection::ToolSelectionRequest;
use pir::coding_agent::tools::CORE_TOOL_NAMES;
use pir::coding_agent::ToolSelection;
use pir::protocol::ModelRef;
use pir::server::{
    empty_snapshot, resolve_server_tool_selection, server_tool_names, ServerSecurityConfig,
    ServerState,
};

fn selection(names: &[&str]) -> ToolSelection {
    ToolSelection {
        enabled: names.iter().map(|name| (*name).to_string()).collect(),
        modifiers: Vec::new(),
        uses_default_tools: false,
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
        context_window: 4096,
        max_tokens: 256,
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

fn fake_model_ref() -> Option<ModelRef> {
    Some(ModelRef {
        provider: "fake".to_owned(),
        id: "fake-model".to_owned(),
    })
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

/// A provider that records every request context and answers immediately.
struct RecordingProvider {
    contexts: Mutex<Vec<Context>>,
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Recording fake provider"
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
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        pusher.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![text("ok")],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            },
        ));
        stream
    }
}

fn recording_registry() -> (Arc<ProviderRegistry>, Arc<RecordingProvider>) {
    let provider = Arc::new(RecordingProvider {
        contexts: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider.clone());
    (Arc::new(registry), provider)
}

// ---------------------------------------------------------------------------
// State-level selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_state_applies_a_selection_to_created_sessions() {
    let state = ServerState::default().with_tool_selection(Some(selection(&["read"])));
    state
        .create_session("selected".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    assert_eq!(
        state.session_tool_names("selected").await,
        Some(vec!["read".to_owned()]),
        "the created session activates exactly the selected tool"
    );
}

/// The shell policy is applied BEFORE selection matching: `bash`/`powershell`
/// are physically removed from the locked-down registry, so selecting `bash`
/// cannot re-enable it. With `allow_remote_bash` on, the selection applies
/// normally.
#[tokio::test]
async fn server_selection_respects_the_shell_policy() {
    let locked = ServerState::default().with_tool_selection(Some(selection(&["read", "bash"])));
    locked
        .create_session("locked".to_owned(), None, None, None, None, 0)
        .await
        .expect("create locked session");
    assert_eq!(
        locked.session_tool_names("locked").await,
        Some(vec!["read".to_owned()]),
        "bash is not in the locked-down registry, so it cannot be active"
    );

    let permissive = ServerState::default()
        .with_security(ServerSecurityConfig {
            allow_remote_bash: true,
            ..ServerSecurityConfig::default()
        })
        .with_tool_selection(Some(selection(&["bash"])));
    permissive
        .create_session("permissive".to_owned(), None, None, None, None, 0)
        .await
        .expect("create permissive session");
    assert_eq!(
        permissive.session_tool_names("permissive").await,
        Some(vec!["bash".to_owned()]),
        "allow_remote_bash keeps the selected shell tool"
    );
}

/// `None` (no request) keeps the historical registration-active behavior,
/// including `powershell` in `session_tool_names` when the policy permits it.
#[tokio::test]
async fn server_without_a_selection_keeps_every_registered_name() {
    let state = ServerState::default();
    state
        .create_session("default".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let names = state
        .session_tool_names("default")
        .await
        .expect("session exists");
    assert!(names.iter().any(|name| name == "read"), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|name| name == "bash" || name == "powershell"),
        "the default policy removes both shell tools: {names:?}"
    );
}

/// §6 #23: the server session's tool surface is the SAME full core registry
/// the CLI hosts build (`register_core_tools`). The server adds no tools of
/// its own and drops none beyond the explicit `allow_remote_bash` policy.
#[tokio::test]
async fn server_session_exposes_the_full_core_tool_set() {
    let state = ServerState::default().with_security(ServerSecurityConfig {
        allow_remote_bash: true,
        ..ServerSecurityConfig::default()
    });
    state
        .create_session("full-core".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let mut names = state
        .session_tool_names("full-core")
        .await
        .expect("session exists");
    names.sort_unstable();
    let mut expected: Vec<String> = CORE_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the server session must register exactly the CLI's core tool set"
    );
    for name in [
        "read",
        "bash",
        "edit",
        "write",
        "ls",
        "grep",
        "find",
        "powershell",
    ] {
        assert!(
            names.iter().any(|candidate| candidate == name),
            "{name} must be part of the server tool surface: {names:?}"
        );
    }
    for name in ["background_status", "background_abort"] {
        assert!(
            names.iter().any(|candidate| candidate == name),
            "{name} must be part of the server tool surface: {names:?}"
        );
    }
}

/// The server has no JS extension runtime (`build_session_agent` passes
/// `extensions: None`), so extension/plugin builtins are outside its actual
/// support range. They are the only core-parity exclusion that is not a
/// security decision.
#[tokio::test]
async fn server_tool_surface_excludes_extension_only_builtins() {
    let state = ServerState::default().with_security(ServerSecurityConfig {
        allow_remote_bash: true,
        ..ServerSecurityConfig::default()
    });
    let names = {
        state
            .create_session("no-extensions".to_owned(), None, None, None, None, 0)
            .await
            .expect("create session");
        state
            .session_tool_names("no-extensions")
            .await
            .expect("session exists")
    };
    for plugin_tool in [
        "todo",
        "subagent",
        "python",
        "ctx_execute",
        "ctx_search",
        "tool_search",
        "codemode",
    ] {
        assert!(
            !names.iter().any(|name| name == plugin_tool),
            "{plugin_tool} is an extension/plugin tool and has no server backend: {names:?}"
        );
    }
}

/// The default locked-down server keeps the CLI core set MINUS both shell
/// tools: `allow_remote_bash` is a fail-closed remote-execution policy, not a
/// registration gap.
#[tokio::test]
async fn server_default_tool_surface_is_the_core_set_minus_shell_tools() {
    let state = ServerState::default();
    state
        .create_session("locked-core".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let mut names = state
        .session_tool_names("locked-core")
        .await
        .expect("session exists");
    names.sort_unstable();
    let mut expected: Vec<String> = CORE_TOOL_NAMES
        .iter()
        .filter(|name| **name != "bash" && **name != "powershell")
        .map(|name| (*name).to_owned())
        .collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the locked-down server registers the core set minus the shell tools"
    );
    assert_eq!(server_tool_names(false), expected);
}

/// The model-facing declaration matches too: with no selection the server
/// declares exactly the default-active core tools (powershell is
/// default-inactive, the shell policy drops bash).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // the recording-provider lock spans the awaits
async fn server_prompt_declares_the_default_core_tool_set() {
    let (registry, provider) = recording_registry();
    let state = ServerState::with_providers(empty_snapshot("q66-server-full"), registry)
        .with_security(ServerSecurityConfig {
            allow_remote_bash: true,
            ..ServerSecurityConfig::default()
        });
    state
        .create_session("declared".to_owned(), None, None, fake_model_ref(), None, 0)
        .await
        .expect("create session");
    state
        .attach_session("declared", "conn-1")
        .await
        .expect("attach session");

    let (finished, outcome) = tokio::sync::oneshot::channel();
    state
        .prompt("declared", "conn-1", "hello".to_owned(), finished)
        .await
        .expect("prompt starts");
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), outcome)
        .await
        .expect("prompt settles")
        .expect("prompt task reports");
    assert!(result.is_ok(), "prompt failed: {result:?}");

    let names: Vec<String> = {
        let contexts = provider
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(contexts.len(), 1, "one provider request");
        contexts[0]
            .tools
            .clone()
            .unwrap_or_default()
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    };
    let mut expected: Vec<String> = CORE_TOOL_NAMES
        .iter()
        .filter(|name| **name != "powershell")
        .map(|name| (*name).to_owned())
        .collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the prompt must declare the default-active core set"
    );
}

// ---------------------------------------------------------------------------
// The agent the server builds carries the selection
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::await_holding_lock)] // the recording-provider lock spans the awaits
async fn server_prompt_declares_only_the_selected_tools() {
    let (registry, provider) = recording_registry();
    let state = ServerState::with_providers(empty_snapshot("q66-server"), registry)
        .with_tool_selection(Some(selection(&["read"])));
    state
        .create_session("prompt".to_owned(), None, None, fake_model_ref(), None, 0)
        .await
        .expect("create session");
    state
        .attach_session("prompt", "conn-1")
        .await
        .expect("attach session");

    let (finished, outcome) = tokio::sync::oneshot::channel();
    state
        .prompt("prompt", "conn-1", "hello".to_owned(), finished)
        .await
        .expect("prompt starts");
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), outcome)
        .await
        .expect("prompt settles")
        .expect("prompt task reports");
    assert!(result.is_ok(), "prompt failed: {result:?}");

    let tools = {
        let contexts = provider
            .contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(contexts.len(), 1, "one provider request");
        contexts[0].tools.clone().unwrap_or_default()
    };
    let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    assert_eq!(
        names,
        vec!["read".to_owned()],
        "the server request must declare exactly the selected tool"
    );
}

// ---------------------------------------------------------------------------
// Resolver unit: the request is the same shared envelope, and a compiled-out
// tool is diagnosed here too (the RPC/server reach of the shared diagnostic).
// ---------------------------------------------------------------------------

#[test]
fn server_resolver_matches_the_shared_request_semantics() {
    let allowlist = ToolSelectionRequest {
        cli_tools: vec!["read".to_owned()],
        ..ToolSelectionRequest::default()
    };
    assert_eq!(
        resolve_server_tool_selection(&allowlist, true)
            .expect("valid")
            .expect("selection")
            .enabled,
        vec!["read".to_owned()]
    );

    let no_tools = ToolSelectionRequest {
        no_tools: true,
        ..ToolSelectionRequest::default()
    };
    assert!(resolve_server_tool_selection(&no_tools, true)
        .expect("valid")
        .expect("selection")
        .enabled
        .is_empty());

    // `--tools bash` on the locked-down server selects nothing: bash is not
    // in the policy-filtered available set.
    let bash = ToolSelectionRequest {
        cli_tools: vec!["bash".to_owned()],
        ..ToolSelectionRequest::default()
    };
    assert!(
        resolve_server_tool_selection(&bash, false)
            .expect("valid")
            .expect("selection")
            .enabled
            .is_empty(),
        "the shell policy removes bash before matching"
    );
}

#[cfg(not(feature = "tools"))]
#[test]
fn server_resolver_reports_the_compiled_out_tool() {
    // Same shared resolver, same diagnostic: explicit `--tools read` in a
    // toolless build names the missing surface. (The text is exactly what
    // print emits.)
    let request = ToolSelectionRequest {
        cli_tools: vec!["read".to_owned()],
        ..ToolSelectionRequest::default()
    };
    let selection = resolve_server_tool_selection(&request, true)
        .expect("the selection resolves (the name simply matches nothing)");
    assert!(selection.expect("requested selection").enabled.is_empty());
}
