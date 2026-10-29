//! Consumer-style public-surface pin for the Rust SDK (`pir::sdk`).
//!
//! `mod consumer` below imports ONLY `pir::sdk::*` for anything `pir` (plus
//! `std`, `serde_json` and `futures_util` for payloads/streams) — the import
//! set an external embedder writes — and names every public SDK item,
//! including every `SdkEvent`/`SdkError` variant and the documented method
//! surface. The target is checked with the default features and with the
//! bare crate plus the `sdk` feature (the facade is optional; there is no
//! stub module when the feature is off):
//!
//! ```text
//! cargo check --test sdk_consumer_surface
//! cargo check --no-default-features --features sdk --test sdk_consumer_surface
//! ```
//!
//! A refactor that silently moves an SDK item behind another cargo feature
//! (or hides it from the facade) breaks the bare-plus-`sdk` check at build time —
//! instead of breaking an embedder's runtime. The runtime half also pins the
//! honest error a feature-less build produces for `endpoint()` (no
//! `provider-*` feature = no wire adapter).

use pir::sdk::{AgentBuilder, ModelRef, SdkError, UsageSnapshot};

/// The consumer-shaped module: pir imports come from the SDK facade only.
mod consumer {
    use std::path::PathBuf;

    use pir::sdk::{
        Agent, AgentBuilder, AgentOutcome, ApiKind, Endpoint, ModelRef, ModelRuntime,
        ModelScopeDiagnostic, MutationQueues, ResolveCliModelResult, ResolveModelScopeResult,
        ResourceKey, SdkError, SdkEvent, SessionMode, UsageSnapshot,
    };

    // Compile-time signature pins for `AgentOutcome` helpers (an outcome
    // value cannot be manufactured outside the crate).
    const _: fn(&AgentOutcome) -> Option<String> = AgentOutcome::message_text;
    const _: fn(&AgentOutcome) -> Vec<String> = AgentOutcome::tool_result_texts;

    /// Every documented [`AgentBuilder`] configuration path. Nothing is
    /// built here: this proves the setters exist and type-check with the
    /// SDK's own types (the embed target builds for real).
    pub fn builder_surface() {
        let _ = AgentBuilder::new()
            .endpoint(Endpoint::new(
                "http://127.0.0.1:1/v1",
                ApiKind::OpenAiCompletions,
                "pin-model",
            ))
            .api_key("sk-pin")
            .system_prompt("pin")
            .append_system_prompt("pin")
            .cwd(PathBuf::from("."))
            .max_turns(1)
            .core_tools(true)
            .tools(["read"])
            .exclude_tools(["bash"])
            .retry(Some(1), Some(10))
            .tool_hooks(tool_hooks_surface())
            .shared_tool_hooks(std::sync::Arc::new(tool_hooks_surface()))
            .on_event("input", |_payload| ())
            .no_session()
            .no_compaction();

        // The other model-resolution shapes, independently.
        let _ = AgentBuilder::new().model("pin-model");
        let _ = AgentBuilder::new().model_in_provider("mock", "pin-model");
        let _ = AgentBuilder::new().no_tools();

        // Every session mode.
        let _ = AgentBuilder::new().session(SessionMode::DefaultDir);
        let _ = AgentBuilder::new().session(SessionMode::Dir(PathBuf::from(".")));
        let _ = AgentBuilder::new().session(SessionMode::Existing(PathBuf::from("pin.jsonl")));
        let _ = AgentBuilder::new().session(SessionMode::None);
    }

    /// The per-tool lifecycle hook surface (upstream `beforeToolCall` /
    /// `afterToolCall`): the hook bundle plus every callback payload type is
    /// reachable from `pir::sdk` alone.
    pub fn tool_hooks_surface() -> pir::sdk::ToolHooks {
        pir::sdk::ToolHooks {
            before: Some(Box::new(
                |_call: &pir::sdk::PreparedToolCall, _ctx: &pir::sdk::ToolContext| {
                    Box::pin(async {
                        pir::sdk::BeforeToolCallResult {
                            block: false,
                            reason: None,
                            terminate: false,
                        }
                    })
                },
            )),
            after: Some(Box::new(
                |_call: &pir::sdk::PreparedToolCall,
                 _result: &pir::sdk::ToolResult,
                 _ctx: &pir::sdk::ToolContext| {
                    Box::pin(async {
                        pir::sdk::AfterToolCallResult {
                            content_blocks: Some(vec![pir::sdk::ToolResultContentBlock::Text(
                                "pin".to_owned(),
                            )]),
                            ..Default::default()
                        }
                    })
                },
            )),
        }
    }

    /// The explicit-endpoint surface and the wire ids of [`ApiKind`].
    pub fn endpoint_surface() -> [&'static str; 3] {
        let mut endpoint = Endpoint::new(
            "http://127.0.0.1:1/v1",
            ApiKind::OpenAiCompletions,
            "pin-model",
        );
        endpoint.api_key = Some("sk-pin".to_owned());
        endpoint.context_window = 8_192;
        endpoint.max_tokens = 1_024;
        let _ = endpoint.base_url.as_str();
        let _ = endpoint.model_id.as_str();
        let _ = endpoint.api;
        [
            ApiKind::OpenAiCompletions.wire_id(),
            ApiKind::AnthropicMessages.wire_id(),
            ApiKind::GoogleGenerativeAi.wire_id(),
        ]
    }

    /// Describes every [`SdkEvent`] variant and its public payload fields.
    /// `TurnEnd`/`MessageEnd` are matched (their `message` payload is a core
    /// `AssistantMessage` the SDK does not re-export); the rest are also
    /// constructed in [`constructible_events`].
    pub fn describe_event(event: &SdkEvent) -> String {
        match event {
            SdkEvent::TurnStart { turn_index } => format!("turn_start:{turn_index}"),
            SdkEvent::TurnEnd {
                turn_index,
                message,
                ..
            } => {
                format!("turn_end:{turn_index}:{}", message.content.len())
            }
            SdkEvent::MessageStart => "message_start".to_owned(),
            SdkEvent::MessageUpdate { delta, thinking } => {
                format!("message_update:{thinking}:{delta}")
            }
            SdkEvent::MessageEnd { message } => format!("message_end:{}", message.content.len()),
            SdkEvent::TranscriptMessageStart { turn, message } => {
                format!("transcript_message_start:{turn}:{}", message.role())
            }
            SdkEvent::TranscriptMessageEnd { turn, message } => {
                format!("transcript_message_end:{turn}:{}", message.role())
            }
            SdkEvent::ToolCallStart {
                turn,
                content_index,
            } => format!("toolcall_start:{turn}:{content_index}"),
            SdkEvent::ToolCallDelta {
                turn,
                content_index,
                delta,
            } => format!("toolcall_delta:{turn}:{content_index}:{delta}"),
            SdkEvent::ToolCallEnd {
                turn,
                content_index,
                tool_call,
            } => format!(
                "toolcall_end:{turn}:{content_index}:{}:{}",
                tool_call.id, tool_call.name
            ),
            SdkEvent::MessageDone {
                turn,
                reason,
                message,
            } => format!("message_done:{turn}:{reason:?}:{}", message.content.len()),
            SdkEvent::MessageError {
                turn,
                reason,
                message,
            } => format!("message_error:{turn}:{reason:?}:{}", message.content.len()),
            SdkEvent::ToolExecutionStart {
                call_id,
                tool_name,
                parent_call_id,
                ..
            } => format!("tool_start:{call_id}:{tool_name}:{parent_call_id:?}"),
            SdkEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                parent_call_id,
                partial,
                ..
            } => format!("tool_update:{tool_call_id}:{tool_name:?}:{parent_call_id:?}:{partial}"),
            SdkEvent::ToolExecutionEnd {
                call_id,
                tool_name,
                is_error,
                duration_ms,
                parent_call_id,
                ..
            } => format!(
                "tool_end:{call_id}:{tool_name}:{is_error}:{duration_ms:?}:{parent_call_id:?}"
            ),
            SdkEvent::Compaction => "compaction".to_owned(),
            SdkEvent::SummarizationRetryScheduled { attempt, .. } => {
                format!("summarization_retry_scheduled:{attempt}")
            }
            SdkEvent::SummarizationRetryAttemptStart { source, reason } => {
                format!("summarization_retry_attempt_start:{source}:{reason:?}")
            }
            SdkEvent::SummarizationRetryFinished => "summarization_retry_finished".to_owned(),
            SdkEvent::PromptRejected { message } => format!("rejected:{message}"),
            SdkEvent::AgentEnd {
                messages,
                will_retry,
            } => format!("agent_end:{}:{will_retry}", messages.len()),
            SdkEvent::AgentSettled { aborted } => format!("settled:{aborted}"),
            _ => "unknown".to_owned(),
        }
    }

    /// Constructs every [`SdkEvent`] variant whose payload needs no
    /// non-SDK type.
    pub fn constructible_events() -> Vec<SdkEvent> {
        vec![
            SdkEvent::TurnStart { turn_index: 1 },
            SdkEvent::MessageStart,
            SdkEvent::MessageUpdate {
                delta: "delta".to_owned(),
                thinking: false,
            },
            SdkEvent::ToolCallStart {
                turn: 1,
                content_index: 0,
            },
            SdkEvent::ToolCallDelta {
                turn: 1,
                content_index: 0,
                delta: "{\"a\":".to_owned(),
            },
            SdkEvent::ToolExecutionStart {
                call_id: "call".to_owned(),
                tool_name: "tool".to_owned(),
                parent_call_id: None,
                args: serde_json::json!(null),
            },
            SdkEvent::ToolExecutionUpdate {
                tool_call_id: "call".to_owned(),
                tool_name: Some("tool".to_owned()),
                parent_call_id: None,
                args: serde_json::json!(null),
                partial: serde_json::json!({"content": []}),
            },
            SdkEvent::ToolExecutionEnd {
                call_id: "call".to_owned(),
                tool_name: "tool".to_owned(),
                is_error: false,
                duration_ms: Some(1),
                parent_call_id: None,
                result: serde_json::json!(null),
            },
            SdkEvent::Compaction,
            SdkEvent::SummarizationRetryScheduled {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 10,
                error_message: "busy".to_owned(),
            },
            SdkEvent::SummarizationRetryAttemptStart {
                source: "compaction".to_owned(),
                reason: Some("threshold".to_owned()),
            },
            SdkEvent::SummarizationRetryFinished,
            SdkEvent::PromptRejected {
                message: "busy".to_owned(),
            },
            SdkEvent::AgentEnd {
                messages: Vec::new(),
                will_retry: false,
            },
            SdkEvent::AgentSettled { aborted: false },
        ]
    }

    /// Constructs every [`SdkError`] variant whose payload needs no non-SDK
    /// type; `Provider` is matched in [`describe_error`] (its payload is a
    /// core `AiError`).
    pub fn constructible_errors() -> Vec<SdkError> {
        vec![
            SdkError::NoModel {
                message: "none".to_owned(),
            },
            SdkError::Io(std::io::Error::other("io")),
            SdkError::Session("session".to_owned()),
            SdkError::Extension("extension".to_owned()),
            SdkError::Aborted,
            SdkError::Other("other".to_owned()),
        ]
    }

    /// Describes every [`SdkError`] variant.
    pub fn describe_error(error: &SdkError) -> String {
        match error {
            SdkError::NoModel { message } => format!("no_model:{message}"),
            SdkError::Provider(inner) => format!("provider:{inner}"),
            SdkError::Io(inner) => format!("io:{inner}"),
            SdkError::Session(message) => format!("session:{message}"),
            SdkError::Extension(message) => format!("extension:{message}"),
            SdkError::Aborted => "aborted".to_owned(),
            SdkError::Other(message) => format!("other:{message}"),
            _ => "unknown".to_owned(),
        }
    }

    /// [`Agent`]'s read-only surface, exercised against a real endpoint
    /// agent by the test when `provider-openai` is enabled and compiled
    /// (signature-pinned) without it. `Agent: Clone` is part of the
    /// guarantee.
    #[allow(dead_code)]
    pub fn agent_surface(agent: &Agent) -> (ModelRef, UsageSnapshot, bool, String) {
        let clone: Agent = agent.clone();
        let model: ModelRef = clone.model();
        let usage: UsageSnapshot = clone.usage_totals();
        let streaming: bool = clone.is_streaming();
        let thinking: String = clone.thinking_level().wire_key().to_owned();
        clone.steer("steer");
        clone.follow_up("follow up");
        clone.set_tool_hooks(None);
        let _ = clone.tool_hooks();
        clone.abort();
        let _ = clone.session_path();
        (model, usage, streaming, thinking)
    }

    /// [`ModelRef`]/[`UsageSnapshot`] value surface (public fields, `Display`,
    /// `Default`, `Copy`).
    pub fn value_surface(model: ModelRef, usage: UsageSnapshot) -> (String, usize, u64) {
        let rendered: String = model.to_string();
        let _ = model.provider.as_str();
        let _ = model.id.as_str();
        let copied: UsageSnapshot = usage;
        let defaults: UsageSnapshot = UsageSnapshot::default();
        (rendered, copied.turns, defaults.total_tokens)
    }

    /// [`AgentOutcome`]'s published fields (an outcome value cannot be
    /// constructed in the consumer module — `#[non_exhaustive]` — so this is
    /// a field/signature pin; `prompt()` is driven for real by the embed
    /// target).
    #[allow(dead_code)]
    pub fn outcome_fields(outcome: &AgentOutcome) -> (u64, usize, bool) {
        let _ = outcome.message.stop_reason;
        let _ = outcome.tool_results.len();
        (outcome.usage.total_tokens, outcome.turns, outcome.aborted)
    }

    /// Upstream `withFileMutationQueue`: the queue/key types and the
    /// path-keyed helper (the operation runs for real in the test).
    pub async fn mutation_queue_surface() -> usize {
        let queues = MutationQueues::default();
        let key = ResourceKey::logical(std::path::Path::new("pin.txt"), std::path::Path::new("."));
        pir::sdk::with_file_mutation_queue(
            &queues,
            std::path::Path::new("pin.txt"),
            std::path::Path::new("."),
            async { key.as_str().len() },
        )
        .await
        .unwrap_or(0)
    }

    /// Upstream `ModelRuntime` / `ModelScopeDiagnostic` / `ResolveCliModelResult`
    /// resolution surface, run for real against an empty model list (one
    /// no-match diagnostic per pattern).
    pub fn model_diagnostics_surface() -> (usize, usize, bool, &'static str) {
        let (scoped, diagnostics): ResolveModelScopeResult =
            pir::sdk::resolve_model_scope(&["missing-model".to_owned()], &[]);
        let diagnostic: &ModelScopeDiagnostic = &diagnostics[0];
        assert!(!diagnostic.is_empty());
        let cli: Result<ResolveCliModelResult, String> =
            pir::sdk::resolve_cli_model(None, "missing-model", &[]);
        (
            scoped.len(),
            diagnostics.len(),
            cli.is_err(),
            std::any::type_name::<ModelRuntime>(),
        )
    }

    /// Upstream `SessionStats` / `ModelCycleResult` type exports.
    pub fn session_stats_surface() -> (usize, usize) {
        (
            std::mem::size_of::<pir::sdk::SessionStats>(),
            std::mem::size_of::<pir::sdk::ModelCycleResult>(),
        )
    }

    /// Upstream tool-output helpers (`formatSize`, `truncateHead`) with the
    /// explicit limit pair the Rust functions take.
    pub fn tool_helpers_surface() -> (String, String, usize) {
        let truncated: pir::sdk::TruncationResult =
            pir::sdk::truncate_head("a\nb\nc", 2, pir::sdk::DEFAULT_MAX_BYTES);
        (
            pir::sdk::format_size(2048),
            truncated.content,
            pir::sdk::DEFAULT_MAX_LINES,
        )
    }

    /// Upstream `AgentSessionRuntimeDiagnostic` (the settings diagnostic).
    pub fn runtime_diagnostic_surface() -> usize {
        let _: Option<pir::sdk::AgentSessionRuntimeDiagnostic> = None;
        let _: Option<pir::sdk::SettingsDiagnosticType> = None;
        std::mem::size_of::<pir::sdk::SettingsDiagnostic>()
    }

    /// Upstream `RpcClient` plus its option/command/response types. Compiles
    /// only with the `client` feature; the method paths are signature-pinned.
    #[cfg(feature = "client")]
    pub fn rpc_surface() -> (String, std::time::Duration) {
        let options = pir::sdk::RpcClientOptions::new("pir");
        let command: pir::sdk::RpcCommand = serde_json::json!({ "type": "hello" });
        let response: pir::sdk::RpcResponse = command;
        let _state: pir::sdk::RpcSessionState = response;
        let listener: std::sync::Arc<pir::sdk::RpcEventListener> =
            std::sync::Arc::new(|_event: &serde_json::Value| {});
        drop(listener);
        (
            options.cli_path.to_string_lossy().into_owned(),
            options.request_timeout,
        )
    }

    #[cfg(feature = "client")]
    #[allow(dead_code)]
    pub async fn rpc_client_surface(
        client: &pir::sdk::RpcClient,
    ) -> Result<(), pir::sdk::RpcClientError> {
        let _ = client.hello().await?;
        let _ = client.get_state().await?;
        let mut events = client.events();
        let _ = events.try_recv();
        let _ = pir::sdk::REQUEST_TIMEOUT;
        client.close().await
    }

    /// Upstream `runPrintMode` / `PrintModeOptions` / `PrintOutcome`.
    #[cfg(feature = "print")]
    pub fn print_mode_surface() -> (usize, usize) {
        let _ = pir::sdk::run_print_mode;
        let _ = pir::sdk::OutputMode::Text;
        (
            std::mem::size_of::<pir::sdk::PrintModeOptions>(),
            std::mem::size_of::<pir::sdk::PrintOutcome>(),
        )
    }

    /// Upstream theme/keybinding utilities. The pure helpers run for real;
    /// `init_theme` and `run_interactive_mode` are signature-pinned (calling
    /// them would take over the process terminal / mutate global theme state).
    #[cfg(feature = "tui")]
    #[allow(clippy::type_complexity)]
    pub fn theme_keybinding_surface(
    ) -> (String, String, String, String, Option<&'static str>, usize) {
        let keybindings: pir::sdk::KeybindingsManager = pir::sdk::Keybindings::new();
        let key = pir::sdk::key_text(&keybindings, "app.interrupt");
        let hint = pir::sdk::key_hint(&keybindings, "app.interrupt", "interrupt");
        let raw = pir::sdk::raw_key_hint("ctrl+c", "clear");
        let markdown: pir::sdk::MarkdownTheme = pir::sdk::get_markdown_theme();
        let heading = (markdown.heading)("Title");
        let language = pir::sdk::get_language_from_path("src/main.rs");
        let highlighted = pir::sdk::highlight_code("fn main() {}", Some("rust"));
        let range =
            pir::sdk::format_read_line_range(&serde_json::json!({ "offset": 2, "limit": 3 }));
        assert!(!range.is_empty());
        let _: fn(&str) -> String = pir::sdk::format_key_text;
        let _: fn(&str) = pir::sdk::init_theme;
        let _ = pir::sdk::run_interactive_mode;
        let options: pir::sdk::InteractiveModeOptions = Default::default();
        let _: Option<pir::sdk::Theme> = None;
        let _: Option<pir::sdk::ThemeToken> = None;
        let _: Option<pir::sdk::ThemeAppearance> = None;
        let _: Option<pir::sdk::TuiAction> = None;
        (
            key,
            hint,
            raw,
            heading,
            language,
            highlighted.len() + usize::from(options.no_themes),
        )
    }

    /// Upstream `detectSupportedImageMimeTypeFromFile` / `resizeImage`.
    #[cfg(any(feature = "print", feature = "tool-read"))]
    pub fn image_mime_surface(path: &std::path::Path) -> std::io::Result<Option<&'static str>> {
        pir::sdk::detect_supported_image_mime_type_from_file(path)
    }

    #[cfg(all(
        any(feature = "image-tools", all(feature = "print", feature = "tui")),
        any(feature = "print", feature = "tool-read")
    ))]
    pub fn image_resize_surface(bytes: &[u8]) -> Result<(String, usize), String> {
        pir::sdk::resize_image(bytes, "image/png", pir::sdk::ImageResizeOptions::default())
            .map(|image: pir::sdk::ResizedImage| (image.mime_type, image.hints.len()))
            .map_err(|error: pir::sdk::ImageResizeError| error.to_string())
    }

    /// The run paths whose execution needs a live provider/model: prompt,
    /// streaming, manual compaction (`set_model`/`set_thinking_level` take
    /// core AI types and are pinned in the target's outer section).
    #[allow(dead_code)]
    pub async fn run_paths(agent: &Agent) -> Result<(), SdkError> {
        let outcome: AgentOutcome = agent.prompt("pin").await?;
        let _ = outcome.message_text();
        let _events = agent.prompt_stream("pin");
        let _filtered = agent.prompt_stream_with("pin", |_event: &SdkEvent| true);
        let _compacted = agent.compact().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The switches that take core AI types (the SDK re-exports neither), still
// entirely public `pir` paths.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
async fn model_switches(
    agent: &pir::sdk::Agent,
    model: pir::ai::types::Model,
    level: pir::ai::types::ThinkingLevel,
) -> Result<(), SdkError> {
    agent.set_model(model).await?;
    let _effective: pir::ai::types::ThinkingLevel = agent.set_thinking_level(level).await?;
    Ok(())
}

#[tokio::test]
async fn consumer_surface_compiles_with_the_sdk_feature() {
    // The sdk-only module's builders, enums and errors type-check and run.
    consumer::builder_surface();
    assert_eq!(
        consumer::endpoint_surface(),
        [
            "openai-completions",
            "anthropic-messages",
            "google-generative-ai"
        ]
    );

    let events = consumer::constructible_events();
    assert_eq!(events.len(), 15);
    for event in &events {
        assert!(!consumer::describe_event(event).is_empty());
    }
    let errors = consumer::constructible_errors();
    assert_eq!(errors.len(), 6);
    for error in &errors {
        assert!(!consumer::describe_error(error).is_empty());
    }

    let (rendered, turns, tokens) = consumer::value_surface(
        ModelRef {
            provider: "mock".to_owned(),
            id: "pin-model".to_owned(),
        },
        UsageSnapshot {
            turns: 2,
            total_tokens: 7,
        },
    );
    assert_eq!(rendered, "mock/pin-model");
    assert_eq!(turns, 2);
    assert_eq!(tokens, 0, "UsageSnapshot::default() is the zero state");

    // A real endpoint agent through the public facade: no network, no
    // session file (`no_session`). `provider-openai` is the endpoint's wire
    // adapter; without it the same build must fail with the honest
    // diagnostic naming the feature.
    let builder = AgentBuilder::new()
        .endpoint(pir::sdk::Endpoint::new(
            "http://127.0.0.1:1/v1",
            pir::sdk::ApiKind::OpenAiCompletions,
            "pin-model",
        ))
        .no_session();

    #[cfg(feature = "provider-openai")]
    {
        use futures_util::StreamExt as _;

        let agent = builder
            .build()
            .await
            .expect("the endpoint path builds without any configured provider");
        let (model, usage, streaming, thinking) = consumer::agent_surface(&agent);
        assert_eq!(model.provider, "sdk-endpoint");
        assert_eq!(model.id, "pin-model");
        assert_eq!(usage.turns, 0);
        assert!(!streaming);
        assert!(!thinking.is_empty());

        // Post-shutdown prompts are rejected before any provider call, so
        // the prompt paths also run for real.
        agent.shutdown().await.expect("shutdown");
        let error = agent
            .prompt("after shutdown")
            .await
            .expect_err("post-shutdown prompts are rejected");
        assert!(!consumer::describe_error(&error).is_empty());

        let rejected: Vec<pir::sdk::SdkEvent> =
            agent.prompt_stream("after shutdown").collect().await;
        assert!(matches!(
            rejected.first(),
            Some(pir::sdk::SdkEvent::PromptRejected { .. })
        ));
        assert!(matches!(
            rejected.last(),
            Some(pir::sdk::SdkEvent::AgentSettled { .. })
        ));

        let filtered: Vec<pir::sdk::SdkEvent> = agent
            .prompt_stream_with("after shutdown", |_event| true)
            .collect()
            .await;
        assert!(matches!(
            filtered.last(),
            Some(pir::sdk::SdkEvent::AgentSettled { .. })
        ));
    }

    #[cfg(not(feature = "provider-openai"))]
    {
        // Bare crate + `sdk` (no provider feature): no wire adapter is
        // compiled, so the build must fail with the diagnostic naming the
        // missing feature — never hand back an agent that only fails later,
        // mid-prompt.
        let error = builder
            .build()
            .await
            .expect_err("a minimal build has no OpenAI-compatible adapter");
        assert!(error.to_string().contains("provider-openai"), "{error}");
    }
}

/// `core_tools(true)` in a build that compiled no built-in tool is a
/// configuration error, not a silent no-op: the caller asked for a tool
/// surface the build cannot provide.
///
/// The gate is the COMPILED `CORE_TOOL_NAMES`, not the aggregate `tools`
/// feature: a valid `sdk,tool-read` (or any single-leaf) shape has core tools
/// even though `tools` is off, so the rejection only applies when every leaf
/// is off.
/// Core upstream-`index.ts` parity items that exist in every `sdk` build:
/// the file mutation queue, model-resolution diagnostics, and the session
/// stats/cycle types.
#[tokio::test]
async fn parity_core_surface_runs() {
    assert!(
        consumer::mutation_queue_surface().await > 0,
        "with_file_mutation_queue runs the operation under the file lock"
    );

    let (scoped, diagnostics, cli_failed, runtime) = consumer::model_diagnostics_surface();
    assert_eq!(scoped, 0, "an empty catalog resolves no scoped models");
    assert_eq!(diagnostics, 1, "one no-match diagnostic per pattern");
    assert!(cli_failed, "resolve_cli_model reports the missing model");
    assert!(
        runtime.contains("ModelRegistry"),
        "ModelRuntime is the ModelRegistry alias: {runtime}"
    );

    let (stats, cycle) = consumer::session_stats_surface();
    assert!(stats > 0, "SessionStats is the public session totals type");
    assert!(cycle > 0, "ModelCycleResult is the public cycle type");

    let (size, head, max_lines) = consumer::tool_helpers_surface();
    assert_eq!(size, "2.0KB");
    assert_eq!(head, "a\nb");
    assert_eq!(max_lines, 2_000);
    assert!(
        consumer::runtime_diagnostic_surface() > 0,
        "AgentSessionRuntimeDiagnostic aliases the public SettingsDiagnostic"
    );
}

/// `RpcClient` and the JSON-line client types (feature `client`).
#[cfg(feature = "client")]
#[test]
fn rpc_client_surface_is_exported() {
    let (cli_path, timeout) = consumer::rpc_surface();
    assert_eq!(cli_path, "pir");
    assert_eq!(timeout, pir::sdk::REQUEST_TIMEOUT);
    let _: Option<pir::sdk::RpcExtensionUIRequest> = Some(serde_json::json!(null));
    let _: Option<pir::sdk::RpcExtensionUIResponse> = Some(serde_json::json!(null));
}

/// `runPrintMode` and its option/result types (feature `print`).
#[cfg(feature = "print")]
#[test]
fn print_mode_surface_is_exported() {
    let (options, outcome) = consumer::print_mode_surface();
    assert!(options > 0, "PrintModeOptions is a non-empty public type");
    assert!(outcome > 0, "PrintOutcome is a non-empty public type");
}

/// Theme/keybinding utilities (feature `tui`).
#[cfg(feature = "tui")]
#[test]
fn theme_and_keybinding_surface_runs() {
    let (key, hint, raw, heading, language, highlighted) = consumer::theme_keybinding_surface();
    assert_eq!(key, "esc", "the platform-default interrupt key");
    assert!(hint.contains("interrupt"), "{hint:?}");
    assert!(raw.contains("clear"), "{raw:?}");
    assert!(heading.contains("Title"), "{heading:?}");
    assert_eq!(language, Some("rust"));
    assert!(highlighted > 0, "highlight_code returns styled lines");
}

/// Image MIME sniffing and resizing (needs the tool/print image module; the
/// resize half additionally needs the `image` crate).
#[cfg(all(
    any(feature = "image-tools", all(feature = "print", feature = "tui")),
    any(feature = "print", feature = "tool-read")
))]
#[test]
fn image_surface_detects_and_resizes_a_png() {
    // 1x1 transparent PNG (the same bytes the image suites use).
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let dir = std::env::temp_dir().join(format!(
        "pir-sdk-image-surface-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create image temp dir");
    let path = dir.join("pin.png");
    std::fs::write(&path, PNG_1X1).expect("write image");

    assert_eq!(
        consumer::image_mime_surface(&path).expect("sniff the file"),
        Some("image/png")
    );
    let (mime, _hints) = consumer::image_resize_surface(PNG_1X1).expect("resize the png");
    assert_eq!(mime, "image/png");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn core_tools_without_a_tool_feature_fails_the_build() {
    if !pir::coding_agent::tools::CORE_TOOL_NAMES.is_empty() {
        // This build compiled at least one core-tool leaf: `core_tools(true)`
        // is a valid request (covered by the surface pin above).
        return;
    }
    let error = AgentBuilder::new()
        .core_tools(true)
        .no_session()
        .build()
        .await
        .expect_err("a tool-less build must reject core_tools(true)");
    let message = error.to_string();
    assert!(message.contains("core_tools(true)"), "{message}");
    assert!(message.contains("tools"), "{message}");
}

/// `EventBus::off` removes by registration id, and ids are PER-BUS counters
/// (every bus starts at 1). A handle from another bus must never unsubscribe
/// a local handler that happens to share its id.
#[test]
fn event_bus_off_ignores_a_handle_from_another_bus() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let bus_a: pir::sdk::EventBus<u32> = pir::sdk::create_event_bus();
    let bus_b: pir::sdk::EventBus<u32> = pir::sdk::create_event_bus();
    let hits_a = std::sync::Arc::new(AtomicUsize::new(0));
    let hits_b = std::sync::Arc::new(AtomicUsize::new(0));

    let sub_a = {
        let hits = std::sync::Arc::clone(&hits_a);
        bus_a.on(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
        })
    };
    let sub_b = {
        let hits = std::sync::Arc::clone(&hits_b);
        bus_b.on(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
        })
    };
    assert_eq!(
        sub_a.id(),
        sub_b.id(),
        "both buses number their first registration identically"
    );

    assert!(
        !bus_b.off(&sub_a),
        "a foreign subscription handle must not remove a local handler"
    );
    assert_eq!(bus_b.listener_count(), 1, "bus B still has its handler");
    assert!(
        sub_a.is_active(),
        "the foreign handle is still registered on A"
    );

    bus_b.emit(&7);
    assert_eq!(hits_b.load(Ordering::SeqCst), 1, "bus B still dispatches");
    assert_eq!(hits_a.load(Ordering::SeqCst), 0, "bus A was not emitted on");

    assert!(bus_a.off(&sub_a), "the owning bus removes it");
    assert!(!bus_a.off(&sub_a), "off is idempotent");
}

/// A subscription's `is_active` reports the real registration state: it turns
/// `false` after `off`, after `clear`, and when the bus itself is gone (not
/// just after `unsubscribe`/drop).
#[test]
fn event_bus_subscription_activity_tracks_the_registration() {
    let bus: pir::sdk::EventBus<u8> = pir::sdk::EventBus::new();

    let sub = bus.on(|_| {});
    assert!(sub.is_active());
    assert!(bus.off(&sub));
    assert!(
        !sub.is_active(),
        "off removed the registration this handle named"
    );
    assert_eq!(bus.listener_count(), 0);

    let cleared = bus.on(|_| {});
    assert!(cleared.is_active());
    bus.clear();
    assert!(
        !cleared.is_active(),
        "clear drops every registration, so the handle is inactive"
    );
    assert_eq!(bus.listener_count(), 0);

    let dropped = bus.on(|_| {});
    assert_eq!(bus.listener_count(), 1);
    drop(dropped);
    assert_eq!(bus.listener_count(), 0, "dropping the handle unsubscribes");

    let doomed = pir::sdk::EventBus::<u8>::new();
    let orphan = doomed.on(|_| {});
    drop(doomed);
    assert!(!orphan.is_active(), "a dropped bus leaves no registration");
}
