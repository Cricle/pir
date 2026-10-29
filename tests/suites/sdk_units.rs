//! Unit tests for `pir::sdk::builder` helpers that the facade suites do not
//! exercise directly (tool-selection bookkeeping and allowlist filtering).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use futures_util::StreamExt;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::providers::construction::model;
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, StreamOptions,
    TerminalReason, TextContent, ThinkingLevel, Tool, UserMessage, UserRole,
};
use pir::sdk::builder::{retain_explicitly_selected, AgentBuilder};
use pir::sdk::{
    create_agent_session, create_agent_session_from_services, create_agent_session_runtime,
    create_coding_tools, create_event_bus, create_read_only_tools, create_read_tool, create_tool,
    Agent, AgentSessionServices, CreateAgentSessionOptions, DefaultPackageManager,
    DefaultResourceLoader, ModelRegistry, ResourceLoader, SdkEvent, SessionEvent, SessionMode,
    SettingsManager,
};
use pir::session_manager::SessionManager;

#[test]
fn empty_exclude_does_not_request_selection() {
    let builder = AgentBuilder::new().exclude_tools(Vec::<String>::new());
    assert!(!builder.tool_selection_requested());
    assert!(builder.tool_denylist().is_empty());

    let builder = AgentBuilder::new().exclude_tools(["write"]);
    assert_eq!(builder.tool_denylist().to_vec(), vec!["write".to_string()]);
}

#[test]
fn default_builder_matches_new() {
    // `Default` must be the documented constructor, not the derived
    // all-`None` shape (which silently disabled threshold compaction).
    assert!(AgentBuilder::new().compaction_enabled());
    assert!(
        AgentBuilder::default().compaction_enabled(),
        "AgentBuilder::default() must match AgentBuilder::new()"
    );
    assert!(!AgentBuilder::new().no_compaction().compaction_enabled());
}

#[test]
fn explicit_plain_allowlist_drops_mcp_carve_out() {
    let allowlist = vec!["read".to_string()];
    let enabled = vec!["read".to_string(), "mcp__srv__tool".to_string()];
    assert_eq!(
        retain_explicitly_selected(&allowlist, enabled),
        vec!["read".to_string()]
    );
}

#[test]
fn explicit_modifiers_start_from_empty_base() {
    let allowlist = vec!["+read".to_string(), "-bash".to_string()];
    let enabled = vec![
        "bash".to_string(),
        "mcp__srv__tool".to_string(),
        "read".to_string(),
    ];
    assert_eq!(
        retain_explicitly_selected(&allowlist, enabled),
        vec!["read".to_string()]
    );
}

/// The allowlist filter uses the same name matcher as the shared resolver:
/// only `*` is a wildcard, `?` is literal, and matching is case-sensitive
/// (upstream `createToolNameMatcher` / `toolPatternRegExp`).
#[test]
fn retain_explicitly_selected_uses_upstream_pattern_semantics() {
    let enabled = vec!["read".to_string(), "write".to_string()];
    assert_eq!(
        retain_explicitly_selected(&["re?d".to_string()], enabled.clone()),
        Vec::<String>::new(),
        "`?` is a literal name character, not a wildcard"
    );
    assert_eq!(
        retain_explicitly_selected(&["READ".to_string()], enabled.clone()),
        Vec::<String>::new(),
        "matching is case-sensitive"
    );
    assert_eq!(
        retain_explicitly_selected(&["re*".to_string()], enabled.clone()),
        vec!["read".to_string()],
        "`*` is still a wildcard"
    );
    assert_eq!(
        retain_explicitly_selected(&["read".to_string()], enabled),
        vec!["read".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Fresh vs resumed exclusion patterns (resume re-expresses the denylist)
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-sdk-units-{label}-{}-{}",
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

fn test_model() -> Model {
    model(
        "fake",
        "fake-model",
        "fake-model",
        "openai-completions",
        "http://127.0.0.1:9/v1",
        4_096,
        256,
        vec!["text"],
    )
}

/// Minimal provider for the hermetic `with_registry` builds below; never
/// streams in these tests.
struct IdleProvider;

impl Provider for IdleProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn name(&self) -> &str {
        "Fake provider"
    }

    fn get_models(&self) -> Vec<Model> {
        vec![test_model()]
    }

    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }

    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "openai-completions".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        ));
        stream
    }
}

/// A fresh (`session == None`) or resumed (`SessionMode::Existing`) SDK build
/// with the core tools registered, optionally excluding one name/pattern.
async fn build_sdk(cwd: &Path, exclude: Option<&str>, session: Option<PathBuf>) -> Agent {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    let builder = AgentBuilder::new()
        .with_registry(Arc::new(registry), test_model())
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .cwd(cwd.to_path_buf())
        .core_tools(true);
    let builder = match exclude {
        Some(pattern) => builder.exclude_tools([pattern]),
        None => builder,
    };
    let builder = match session {
        Some(path) => builder.session(SessionMode::Existing(path)),
        None => builder.no_session(),
    };
    builder.build().await.expect("build")
}

/// Persist the transcript a run records: the declared loadout as `toolsAdded`
/// on a system message, then a user turn (the shape `recorded_tool_loadout`
/// restores).
fn record_session(dir: &Path, names: &[String]) -> PathBuf {
    let mut storage =
        SessionManager::create(&dir.display().to_string(), dir).expect("create session");
    storage
        .append_message(Message::System(UserMessage {
            role: UserRole::System,
            content: Vec::new(),
            tools_added: Some(
                names
                    .iter()
                    .map(|name| Tool {
                        name: name.clone(),
                        description: format!("{name} tool"),
                        parameters: json!({ "type": "object" }),
                        constrained_sampling: None,
                    })
                    .collect(),
            ),
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append declaration");
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
        .expect("append user");
    storage.path().to_path_buf()
}

fn declared_names(agent: &Agent) -> Vec<String> {
    agent.tools().into_iter().map(|tool| tool.name).collect()
}

/// Regression: a resumed build re-expressed `.exclude_tools` entries as
/// `-name` modifiers with the OLD `glob_matches` semantics (a `?` wildcard,
/// case-insensitive), while the fresh selection uses the shared resolver's
/// upstream-faithful matcher (only `*` is a wildcard, `?` is literal,
/// case-sensitive). `.exclude_tools(["READ"])` and `.exclude_tools(["re?d"])`
/// therefore kept `read` on a fresh build but dropped it after a resume.
#[tokio::test(flavor = "current_thread")]
async fn exclude_patterns_keep_fresh_and_resumed_declared_sets_in_agreement() {
    let sessions = TempDir::new("exclude-pattern-sessions");
    let cwd = TempDir::new("exclude-pattern-cwd");

    // The loadout an earlier run (before the denylist was configured) recorded:
    // every default-active core tool, `read` included.
    let unfiltered = build_sdk(&cwd.0, None, None).await;
    let recorded_base = declared_names(&unfiltered);
    assert!(
        recorded_base.iter().any(|name| name == "read"),
        "the fixture transcript must record `read`: {recorded_base:?}"
    );

    // (pattern, whether it must remove `read`)
    for (pattern, removes_read) in [
        // The regression: neither of these excludes `read`.
        ("READ", false),
        ("re?d", false),
        // The exact name and a `*` pattern still select it.
        ("read", true),
        ("re*", true),
    ] {
        let fresh = build_sdk(&cwd.0, Some(pattern), None).await;
        let fresh_names = declared_names(&fresh);
        assert!(
            fresh_names.len() > 2,
            "the core tools must be registered: {fresh_names:?}"
        );
        assert_eq!(
            fresh_names.iter().any(|name| name == "read"),
            !removes_read,
            "fresh build with `.exclude_tools([{pattern:?}])` declared {fresh_names:?}"
        );

        // Resuming from the recorded loadout must declare exactly the fresh
        // set: the exclusions are re-expressed as `-name` modifiers, so the
        // recorded `read` must survive `READ`/`re?d` and still be removed by
        // the exact name and by a `*` pattern.
        let path = record_session(&sessions.0, &recorded_base);
        let resumed = build_sdk(&cwd.0, Some(pattern), Some(path)).await;
        assert_eq!(
            declared_names(&resumed),
            fresh_names,
            "`.exclude_tools([{pattern:?}])` must keep the same declared set on resume"
        );
    }
}

// ---------------------------------------------------------------------------
// Session-level SDK export surface (audit §4 #10-14 / #27 / #28)
// ---------------------------------------------------------------------------

#[test]
fn event_bus_delivers_in_order_and_unsubscribes() {
    let bus = create_event_bus::<u32>();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let first = bus.on({
        let seen = Arc::clone(&seen);
        move |value: &u32| seen.lock().unwrap().push(*value)
    });
    let second = bus.on(|_| {});
    bus.emit(&1);
    bus.emit(&2);
    assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
    assert_eq!(bus.listener_count(), 2);
    assert!(bus.off(&second));
    assert_eq!(bus.listener_count(), 1);
    drop(first);
    assert_eq!(bus.listener_count(), 0);
    bus.emit(&3);
    assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
    bus.on(|_| {});
    bus.clear();
    assert!(!bus.has_listeners());
}

#[test]
fn session_events_project_into_sdk_events() {
    let queue = SdkEvent::from_session_event(&SessionEvent::QueueUpdate {
        steering: vec!["a".into()],
        follow_up: vec!["b".into()],
        delivered_steering: vec!["a".into()],
        delivered_follow_up: Vec::new(),
    });
    match queue {
        Some(SdkEvent::QueueUpdate {
            steering,
            follow_up,
        }) => {
            assert_eq!(steering, vec!["a".to_string()]);
            assert_eq!(follow_up, vec!["b".to_string()]);
        }
        other => panic!("unexpected projection: {other:?}"),
    }

    assert!(matches!(
        SdkEvent::from_session_event(&SessionEvent::CompactionStart { reason: "manual" }),
        Some(SdkEvent::CompactionStart { reason }) if reason == "manual"
    ));
    let compaction_end = SdkEvent::from_session_event(&SessionEvent::CompactionEnd {
        reason: "threshold",
        result: Some(serde_json::json!({"summary": "s"})),
        aborted: false,
        will_retry: true,
        error_message: Some("warn".into()),
    });
    match compaction_end {
        Some(SdkEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        }) => {
            assert_eq!(reason, "threshold");
            assert_eq!(result.unwrap()["summary"], serde_json::json!("s"));
            assert!(!aborted);
            assert!(will_retry);
            assert_eq!(error_message.as_deref(), Some("warn"));
        }
        other => panic!("unexpected projection: {other:?}"),
    }

    assert!(matches!(
        SdkEvent::from_session_event(&SessionEvent::EntryAppended {
            entry: serde_json::json!({"id": "e1", "message": {}})
        }),
        Some(SdkEvent::EntryAppended { entry }) if entry["id"] == serde_json::json!("e1")
    ));
    assert!(matches!(
        SdkEvent::from_session_event(&SessionEvent::SessionInfoChanged { name: None }),
        Some(SdkEvent::SessionInfoChanged { name: None })
    ));
    assert!(matches!(
        SdkEvent::from_session_event(&SessionEvent::ThinkingLevelChanged { level: "high".into() }),
        Some(SdkEvent::ThinkingLevelChanged { level }) if level == "high"
    ));
    assert!(matches!(
        SdkEvent::from_session_event(&SessionEvent::BashExecutionUpdate {
            id: Some("bash-1".into()),
            delta: "out".into()
        }),
        Some(SdkEvent::BashExecutionUpdate { id: Some(id), delta }) if id == "bash-1" && delta == "out"
    ));
    assert!(matches!(
        SdkEvent::from_session_event(&SessionEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 500,
            error_message: "429".into()
        }),
        Some(SdkEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 500,
            ..
        })
    ));
    let retry_end = SdkEvent::from_session_event(&SessionEvent::AutoRetryEnd {
        success: false,
        attempt: 3,
        final_error: Some("boom".into()),
    });
    assert!(matches!(
        retry_end,
        Some(SdkEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some(error),
        }) if error == "boom"
    ));
    let background = SdkEvent::from_session_event(&SessionEvent::BackgroundToolEnd {
        call_id: "call".into(),
        tool_name: "tool".into(),
        parent_call_id: None,
        is_error: false,
        duration_ms: Some(5),
        output: "done".into(),
        images: Vec::new(),
        details: None,
        structured_content: None,
        usage: None,
    });
    assert!(matches!(
        background,
        Some(SdkEvent::BackgroundToolEnd {
            call_id,
            duration_ms: Some(5),
            ..
        }) if call_id == "call"
    ));
    // The session's settle is the SDK stream's own terminal event: projecting
    // it too would duplicate AgentSettled.
    assert!(SdkEvent::from_session_event(&SessionEvent::AgentSettled { aborted: false }).is_none());
}

#[test]
fn tool_factories_expose_the_compiled_core_tools() {
    assert_eq!(create_read_tool().expect("read tool").name(), "read");
    let coding = create_coding_tools().expect("core tools");
    assert!(coding.iter().any(|tool| tool.name() == "read"));
    assert!(pir::sdk::has_core_tool("read"));
    let read_only = create_read_only_tools().expect("read-only tools");
    let names: Vec<&str> = read_only.iter().map(|tool| tool.name()).collect();
    for expected in ["read", "grep", "find", "ls"] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    assert!(matches!(
        create_tool("definitely-not-a-tool"),
        Err(pir::sdk::SdkError::Unsupported(_))
    ));
}

#[test]
fn default_resource_loader_discovers_and_merges_contributed_paths() {
    let dir = TempDir::new("resource-loader");
    let agent_dir = dir.0.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let home = dir.0.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let skills = dir.0.join("skills");
    std::fs::create_dir_all(skills.join("first")).unwrap();
    std::fs::write(
        skills.join("first/SKILL.md"),
        "---\nname: first\ndescription: First skill\n---\nFirst body\n",
    )
    .unwrap();
    let prompts = dir.0.join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(
        prompts.join("greet.md"),
        "---\ndescription: Greeting\n---\nHello $1\n",
    )
    .unwrap();

    let mut loader = DefaultResourceLoader::new(&dir.0, &agent_dir)
        .with_project_trusted(false)
        .with_home(&home)
        .with_skill_paths([skills.clone()])
        .with_prompt_paths([prompts]);
    let discovered = loader.reload();
    assert!(discovered.skills.iter().any(|skill| skill.name == "first"));
    assert!(discovered
        .prompt_templates
        .iter()
        .any(|template| template.name == "greet"));
    assert!(loader.skills().iter().any(|skill| skill.name == "first"));
    assert!(loader
        .prompt_templates()
        .iter()
        .any(|template| template.name == "greet"));

    // A contributed path with a colliding name keeps the first definition and
    // reports the collision; a new name is appended.
    let contributed = dir.0.join("contributed");
    std::fs::create_dir_all(contributed.join("second")).unwrap();
    std::fs::write(
        contributed.join("second/SKILL.md"),
        "---\nname: second\ndescription: Second skill\n---\nSecond body\n",
    )
    .unwrap();
    let contributed_prompts = dir.0.join("contributed-prompts");
    std::fs::create_dir_all(&contributed_prompts).unwrap();
    std::fs::write(
        contributed_prompts.join("greet.md"),
        "---\ndescription: Collision\n---\nNope\n",
    )
    .unwrap();
    let diagnostics = loader.extend_resources(&pir::sdk::ExtensionResourcePaths {
        skill_paths: vec![contributed],
        prompt_paths: vec![contributed_prompts],
        theme_paths: Vec::new(),
    });
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("collision")),
        "expected collision diagnostics: {diagnostics:?}"
    );
    let merged = loader.reload();
    let names: Vec<&str> = merged
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert!(
        names.contains(&"first") && names.contains(&"second"),
        "{names:?}"
    );
    assert!(merged
        .prompt_templates
        .iter()
        .find(|template| template.name == "greet")
        .is_some_and(|template| template.description == "Greeting"));
}

#[test]
fn model_registry_and_package_manager_wrap_the_core_managers() {
    let dir = TempDir::new("registry-packages");
    let agent_dir = dir.0.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    let models = ModelRegistry::from_registry(Arc::new(registry));
    assert_eq!(models.models().len(), 1);
    assert!(models.get_model("fake", "fake-model").is_some());
    assert!(
        models.available_models().is_empty(),
        "the fake provider has no configured credential"
    );

    let settings = SettingsManager::load(&agent_dir, &dir.0.join(".pi"), &dir.0, false);
    let packages = DefaultPackageManager::new(&agent_dir, &dir.0)
        .with_project_trusted(false)
        .resolve(&settings);
    assert!(packages.extensions.is_empty());
    assert!(packages.skills.is_empty());
    assert!(packages.prompts.is_empty());
    assert!(packages.themes.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn create_agent_session_builds_a_hermetic_session_and_runtime() {
    let dir = TempDir::new("create-session");
    let agent_dir = dir.0.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    let registry = ModelRegistry::from_registry(Arc::new(registry));

    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: Some(dir.0.clone()),
        agent_dir: Some(agent_dir.clone()),
        model_registry: Some(registry.clone()),
        model: Some(test_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        no_tools: true,
        ..Default::default()
    })
    .await
    .expect("create session");
    assert_eq!(created.model.id, "fake-model");
    assert!(created.session.session_path().is_none());
    assert!(created.resources.skills.is_empty());

    let result = create_agent_session_runtime(CreateAgentSessionOptions {
        cwd: Some(dir.0.clone()),
        agent_dir: Some(agent_dir),
        model_registry: Some(registry),
        model: Some(test_model()),
        thinking_level: Some(ThinkingLevel::Off),
        no_session: true,
        no_tools: true,
        ..Default::default()
    })
    .await
    .expect("create runtime");
    assert!(result.runtime.session_dir().is_none());
    let reload = result.runtime.reload().await.expect("reload");
    assert!(reload.extensions.is_none());
    assert!(reload.resources.is_empty());
    assert!(matches!(
        result.runtime.new_session().await,
        Err(pir::sdk::SdkError::Session(_))
    ));
    result.runtime.dispose().await.expect("dispose");
}

/// `create_agent_session_from_services` documents that the SERVICES win for
/// the stored session; a session carried on the options value must not replace
/// it.
#[tokio::test(flavor = "current_thread")]
async fn from_services_prefers_the_services_session_over_the_options_session() {
    let services_dir = TempDir::new("from-services-session");
    let options_dir = TempDir::new("from-services-options");
    let agent_dir = TempDir::new("from-services-agent");

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(IdleProvider));
    let services_session =
        SessionManager::create(&services_dir.0.display().to_string(), &services_dir.0)
            .expect("create the services session");
    let services_path = services_session.path().to_path_buf();
    let options_session =
        SessionManager::create(&options_dir.0.display().to_string(), &options_dir.0)
            .expect("create the options session");
    let options_path = options_session.path().to_path_buf();
    assert_ne!(services_path, options_path);

    let services = AgentSessionServices {
        model_registry: ModelRegistry::from_registry(Arc::new(registry)),
        settings: SettingsManager::load(
            &agent_dir.0,
            &services_dir.0.join(".pi"),
            &services_dir.0,
            false,
        ),
        session: Some(services_session),
        resources: DefaultResourceLoader::new(services_dir.0.clone(), agent_dir.0.clone())
            .with_project_trusted(false),
    };
    let created = create_agent_session_from_services(
        services,
        CreateAgentSessionOptions {
            cwd: Some(services_dir.0.clone()),
            agent_dir: Some(agent_dir.0.clone()),
            model: Some(test_model()),
            thinking_level: Some(ThinkingLevel::Off),
            no_tools: true,
            session: Some(options_session),
            ..Default::default()
        },
    )
    .await
    .expect("build from services");
    assert_eq!(
        created.session.session_path(),
        Some(services_path),
        "the services' session must win over one passed on the options"
    );
}

// --- resources_discover injection + session-event projection, end to end ---

struct CapturingProvider {
    contexts: Arc<std::sync::Mutex<Vec<Context>>>,
    model: Model,
}

impl Provider for CapturingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Capturing provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts.lock().unwrap().push(context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "openai-completions".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        ));
        stream
    }
}

#[tokio::test(flavor = "current_thread")]
async fn resources_discover_paths_reach_skill_expansion_and_events_stream() {
    let dir = TempDir::new("resource-discover-e2e");
    let skill_dir = dir.0.join("contrib");
    std::fs::create_dir_all(skill_dir.join("contributed")).unwrap();
    std::fs::write(
        skill_dir.join("contributed/SKILL.md"),
        "---\nname: contributed\ndescription: Contributed skill\n---\nContributed body\n",
    )
    .unwrap();
    let extension = dir.0.join("ext.ts");
    std::fs::write(
        &extension,
        format!(
            r#"
            export default function (pi) {{
                pi.on("resources_discover", () => ({{
                    skillPaths: [{}],
                }}));
            }}
            "#,
            serde_json::to_string(&skill_dir.display().to_string()).unwrap()
        ),
    )
    .unwrap();

    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CapturingProvider {
        contexts: Arc::clone(&contexts),
        model: test_model(),
    }));
    let agent = AgentBuilder::new()
        .with_registry(Arc::new(registry), test_model())
        .extension_paths(vec![extension])
        .session(SessionMode::Dir(dir.0.join("sessions")))
        .no_compaction()
        .cwd(dir.0.clone())
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await
        .expect("build");

    let events: Vec<SdkEvent> = agent
        .prompt_stream("/skill:contributed hello")
        .collect::<Vec<_>>()
        .await;
    {
        let captured = contexts.lock().unwrap();
        let saw_skill = captured.iter().any(|context| {
            context.messages.iter().any(|message| match message {
                Message::User(user) => user.content.iter().any(|block| match block {
                    ContentBlock::Text(text) => {
                        text.text.contains("<skill name=\"contributed\"")
                            && text.text.contains("Contributed body")
                    }
                    _ => false,
                }),
                _ => false,
            })
        });
        assert!(
            saw_skill,
            "the extension-contributed skill must expand in the prompt: {captured:#?}"
        );
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SdkEvent::EntryAppended { .. })),
        "entry_appended must be projected onto the SDK stream"
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, SdkEvent::AgentSettled { .. })));
}
