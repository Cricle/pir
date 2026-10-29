//! Tests for the tool exposure model (upstream `ToolExposure` /
//! `ToolLoadout` / `prepareLoadout`), the `--tools` pattern selection, the
//! system-prompt `hiddenTools` omission, and virtual models (registration,
//! routing, session-state persistence).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    callable_tools, declared_tool_definitions, declared_tools, AgentTool, BoxFuture, ToolContext,
    ToolExposure, ToolLoadout, ToolLoadoutChanges, ToolNamespace, ToolRegistry,
};
use pir::agent_session::{AgentSession, AgentSessionConfig, SharedEventStreamFn};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::tool_selection::resolve_tool_selection;
use pir::extensions::ExtensionConfig;
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

fn physical_model() -> Model {
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![physical_model()]
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

fn assistant_message(provider: &str, model: &str, text_value: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake-api".into(),
        provider: provider.into(),
        model: model.into(),
        content: vec![text(text_value)],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Exposure test tools
// ---------------------------------------------------------------------------

/// A synchronous loadout hook (upstream `prepareLoadout`).
type LoadoutHook = Box<dyn Fn(&ToolLoadout) -> Option<ToolLoadoutChanges> + Send + Sync>;

/// A tool with configurable exposure metadata.
struct ExposedTool {
    name: &'static str,
    exposure: ToolExposure,
    default_active: bool,
    namespace: Option<ToolNamespace>,
    loadout: Option<LoadoutHook>,
}

impl ExposedTool {
    fn new(name: &'static str, exposure: ToolExposure) -> Self {
        Self {
            name,
            exposure,
            default_active: true,
            namespace: None,
            loadout: None,
        }
    }
}

impl AgentTool for ExposedTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "exposure test tool"
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn default_active(&self) -> bool {
        self.default_active
    }
    fn namespace(&self) -> Option<ToolNamespace> {
        self.namespace.clone()
    }
    fn prepare_loadout(&self, loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        let hook = self.loadout.as_ref()?;
        hook(loadout)
    }
    fn run(&self, _args: &serde_json::Value, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        Box::pin(async { ToolResult::ok("ran") })
    }
}

fn exposure_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ExposedTool::new(
        "direct_tool",
        ToolExposure::Direct,
    )));
    registry.register(Arc::new(ExposedTool::new(
        "model_only_tool",
        ToolExposure::ModelOnly,
    )));
    registry.register(Arc::new(ExposedTool::new(
        "codemode_tool",
        ToolExposure::Codemode,
    )));
    registry.register(Arc::new(ExposedTool::new(
        "deferred_tool",
        ToolExposure::Deferred,
    )));
    registry.register(Arc::new(ExposedTool::new(
        "hidden_tool",
        ToolExposure::Hidden,
    )));
    registry
}

fn names(tools: &[Arc<dyn AgentTool>]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name()).collect()
}

// ---------------------------------------------------------------------------
// 1. Exposure states: declared vs callable matrix
// ---------------------------------------------------------------------------

#[test]
fn exposure_states_default_and_wire_names_round_trip() {
    assert_eq!(ToolExposure::default(), ToolExposure::Direct);
    for (wire, parsed) in [
        ("direct", ToolExposure::Direct),
        ("model-only", ToolExposure::ModelOnly),
        ("codemode", ToolExposure::Codemode),
        ("deferred", ToolExposure::Deferred),
        ("hidden", ToolExposure::Hidden),
    ] {
        assert_eq!(ToolExposure::parse(wire), Some(parsed));
        assert_eq!(parsed.as_str(), wire);
    }
    assert_eq!(ToolExposure::parse("nope"), None);
}

#[test]
fn hidden_tools_are_unreachable_in_the_registry() {
    let mut registry = exposure_registry();
    assert_eq!(registry.len(), 5);
    let removed = registry.remove_exposure(ToolExposure::Hidden);
    assert_eq!(removed, vec!["hidden_tool".to_string()]);
    assert!(registry.get("hidden_tool").is_none());
    assert_eq!(registry.len(), 4);
}

#[test]
fn active_set_is_direct_and_model_only_with_default_active() {
    let mut registry = exposure_registry();
    registry.remove_exposure(ToolExposure::Hidden);
    // direct + model-only activate on registration; codemode/deferred do not.
    let mut active = registry.active_names(None);
    active.sort();
    assert_eq!(active, vec!["direct_tool", "model_only_tool"]);

    // `defaultActive: false` keeps a direct tool out of the default set.
    let mut quiet = ExposedTool::new("quiet_direct", ToolExposure::Direct);
    quiet.default_active = false;
    registry.register(Arc::new(quiet));
    let mut active = registry.active_names(None);
    active.sort();
    assert_eq!(active, vec!["direct_tool", "model_only_tool"]);

    // The override names exactly the requested non-hidden tools.
    let override_active = registry.active_names(Some(&["quiet_direct".to_string()]));
    assert_eq!(override_active, vec!["quiet_direct"]);
}

#[test]
fn declared_vs_callable_matrix_matches_upstream() {
    let mut registry = exposure_registry();
    registry.remove_exposure(ToolExposure::Hidden);
    let active = registry.active_names(None);
    let mut active = active;
    active.sort();

    let declared = declared_tools(&registry, &active);
    let mut declared = declared;
    declared.sort_by(|left, right| left.name().cmp(right.name()));
    assert_eq!(
        names(&declared),
        vec!["direct_tool", "model_only_tool"],
        "declared = active direct + model-only"
    );

    let callable = callable_tools(&registry, &active);
    assert_eq!(
        names(&callable),
        vec!["codemode_tool", "deferred_tool", "direct_tool"],
        "callable = active direct + every codemode/deferred; model-only never callable"
    );

    // The active override activates a codemode tool's declaration too:
    // upstream's loadout declares the whole active non-hidden set, and a
    // codemode/deferred tool only enters the active set through such an
    // explicit activation (tool_search's load included).
    let override_active = vec![
        "direct_tool".to_string(),
        "codemode_tool".to_string(),
        "model_only_tool".to_string(),
    ];
    let declared = declared_tools(&registry, &override_active);
    let mut declared = declared;
    declared.sort_by(|left, right| left.name().cmp(right.name()));
    assert_eq!(
        names(&declared),
        vec!["codemode_tool", "direct_tool", "model_only_tool"]
    );
    let callable = callable_tools(&registry, &override_active);
    assert_eq!(
        names(&callable),
        vec!["codemode_tool", "deferred_tool", "direct_tool"]
    );
}

#[test]
fn loadout_definitions_rewrite_descriptions_and_drop_hidden_declarations() {
    let registry = exposure_registry();
    let active = vec!["direct_tool".to_string(), "model_only_tool".to_string()];

    // No hooks: original descriptions, nothing hidden.
    let declared = declared_tools(&registry, &active);
    let definitions =
        declared_tool_definitions(&declared, &Default::default(), &Default::default());
    assert_eq!(definitions.len(), 2);
    assert!(definitions
        .iter()
        .all(|tool| tool.description == "exposure test tool"));

    // A hook rewrites one description and hides one declaration (still active).
    let mut descriptions = std::collections::BTreeMap::new();
    descriptions.insert(
        "direct_tool".to_string(),
        "rewritten description".to_string(),
    );
    let mut hidden = std::collections::BTreeSet::new();
    hidden.insert("model_only_tool".to_string());
    let definitions = declared_tool_definitions(&declared, &descriptions, &hidden);
    assert_eq!(
        definitions.len(),
        1,
        "hidden declaration omitted from request"
    );
    assert_eq!(definitions[0].name, "direct_tool");
    assert_eq!(definitions[0].description, "rewritten description");
    // ... but still active (registered) - just not declared or callable
    // (`model-only` is never callable).
    assert!(registry.get("model_only_tool").is_some());
    assert!(!callable_tools(&registry, &active)
        .iter()
        .any(|tool| tool.name() == "model_only_tool"));
}

#[test]
fn rust_prepare_loadout_hook_receives_the_loadout() {
    let mut orchestrator = ExposedTool::new("orchestrator", ToolExposure::Direct);
    orchestrator.loadout = Some(Box::new(|loadout: &ToolLoadout| {
        // The hook sees the callable set and rewrites its own description to
        // list it (upstream codemode behavior).
        let callable: Vec<&str> = loadout.callable.iter().map(|tool| tool.name()).collect();
        assert!(callable.contains(&"codemode_tool"));
        assert_eq!(
            loadout.get_exposure("codemode_tool"),
            ToolExposure::Codemode
        );
        assert_eq!(loadout.get_exposure("direct_tool"), ToolExposure::Direct);
        Some(ToolLoadoutChanges::with_description(
            "orchestrator",
            format!("orchestrates: {}", callable.join(", ")),
        ))
    }));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(orchestrator));
    registry.register(Arc::new(ExposedTool::new(
        "codemode_tool",
        ToolExposure::Codemode,
    )));

    let active = registry.active_names(None);
    let declared = declared_tools(&registry, &active);
    let callable = callable_tools(&registry, &active);
    let mut exposures = std::collections::BTreeMap::new();
    for tool in registry.iter() {
        exposures.insert(tool.name().to_string(), tool.exposure());
    }
    let loadout = ToolLoadout {
        declared: declared.clone(),
        callable,
        registered: registry.iter().cloned().collect(),
        exposures,
        namespaces: std::collections::BTreeMap::new(),
    };
    let mut descriptions = std::collections::BTreeMap::new();
    for tool in &declared {
        if let Some(changes) = tool.prepare_loadout(&loadout) {
            descriptions.extend(changes.descriptions);
        }
    }
    let rewritten = descriptions
        .get("orchestrator")
        .map(String::as_str)
        .expect("orchestrator rewrote its description");
    assert!(
        rewritten.starts_with("orchestrates: codemode_tool"),
        "{rewritten}"
    );
    let definitions = declared_tool_definitions(&declared, &descriptions, &Default::default());
    assert!(
        definitions[0]
            .description
            .starts_with("orchestrates: codemode_tool"),
        "{}",
        definitions[0].description
    );
}

// ---------------------------------------------------------------------------
// 2. --tools pattern selection (`*` patterns, modifiers, no MCP carve-out)
// ---------------------------------------------------------------------------

/// Tool-name matching follows upstream `toolPatternRegExp`, NOT a generic
/// shell glob: only `*` is a wildcard, `?` is a literal character, and
/// matching is case-sensitive. Pinned through the public resolver
/// (`match_allowlist` -> `tool_pattern_matches`), which is the path every
/// host's `--tools` selection takes; the matcher itself is private.
#[test]
fn tool_name_patterns_are_star_only_and_case_sensitive() {
    let available = vec![
        "read".to_string(),
        "read_many_files".to_string(),
        "bash".to_string(),
    ];
    let defaults = ["read", "bash"];
    let resolve = |pattern: &str| {
        resolve_tool_selection(
            None,
            &[pattern.to_string()],
            &[],
            false,
            false,
            &available,
            &defaults,
        )
        .expect("valid selection")
        .expect("an explicit allowlist changes the defaults")
        .enabled
    };
    // `*` matches any run of characters, including none.
    assert_eq!(
        resolve("r*"),
        vec!["read".to_string(), "read_many_files".to_string()]
    );
    // `?` is literal, not a one-character wildcard: no tool is named `r?ad`.
    assert!(resolve("r?ad").is_empty());
    // Matching is case-sensitive, unlike the generic glob helper.
    assert!(resolve("READ").is_empty());
}

#[test]
fn tools_allowlist_accepts_globs_and_never_declares_unnamed_mcp_tools() {
    let available = vec![
        "read".to_string(),
        "read_many_files".to_string(),
        "bash".to_string(),
        "mcp__docs_search".to_string(),
        "mcp__docs_fetch".to_string(),
    ];
    let selection = resolve_tool_selection(
        None,
        &["read*".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("valid selection")
    .expect("selection changes the defaults");
    // Glob match ONLY: an MCP tool that no entry names or matches stays
    // REGISTERED but undeclared (upstream `_isAllowedTool` vs `_isActivatable`,
    // agent-session.ts:1542-1558). `enabled` is the declared set, so the old
    // carve-out appended both MCP names here.
    assert_eq!(
        selection.enabled,
        vec!["read".to_string(), "read_many_files".to_string()]
    );

    // An explicit mcp__* allowlist entry names (and so activates) one MCP tool.
    let selection = resolve_tool_selection(
        None,
        &["mcp__docs_search".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("valid selection")
    .expect("selection");
    assert_eq!(selection.enabled, vec!["mcp__docs_search".to_string()]);

    // --exclude-tools accepts patterns too.
    let selection = resolve_tool_selection(
        None,
        &[],
        &["mcp__docs_*".to_string()],
        false,
        false,
        &available,
        &["read", "bash", "edit", "write"],
    )
    .expect("valid selection")
    .expect("selection");
    assert_eq!(
        selection.enabled,
        vec![
            "read".to_string(),
            "bash".to_string(),
            "edit".to_string(),
            "write".to_string()
        ],
        "only the matched MCP tools are excluded"
    );
}

#[test]
fn tool_list_validation_rejects_mixed_and_pattern_modifiers() {
    let available = vec!["read".to_string(), "bash".to_string()];
    let error = resolve_tool_selection(
        None,
        &["read".to_string(), "+bash".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash"],
    )
    .expect_err("mixed names and modifiers are invalid");
    assert!(error.contains("cannot be mixed"), "{error}");

    let error = resolve_tool_selection(
        None,
        &["+read*".to_string()],
        &[],
        false,
        false,
        &available,
        &["read", "bash"],
    )
    .expect_err("modifiers take exact names");
    assert!(error.contains("not patterns"), "{error}");
}

// ---------------------------------------------------------------------------
// 3. System prompt hiddenTools omission
// ---------------------------------------------------------------------------

#[test]
fn system_prompt_omits_hidden_and_non_declared_tools() {
    use pir::system_prompt::{build_system_prompt_sections, SystemPromptInput, ToolInfo};
    // Only snippet-carrying tools are listed (upstream `_rebuildSystemPrompt`
    // builds `toolSnippets` from snippet-carrying tools only: "Tools without
    // a snippet are not listed", agent-session.ts:1701-1706), so `read`
    // carries a prompt snippet.
    let read = ToolInfo {
        snippet: Some("Read files".into()),
        ..ToolInfo::new("read", "Read files")
    };
    let tools = vec![read, ToolInfo::new("codemode", "Run scripts")];
    // `codemode` is callable but not declared: the tool list omits it.
    let hidden = vec!["codemode".to_string()];
    let input = SystemPromptInput {
        preamble_override: None,
        append: Vec::new(),
        tools: &tools,
        hidden_tools: &hidden,
        project_files: &[],
        skills: &[],
        cwd: std::path::Path::new("/work"),
        // No package dir: this test asserts the tool-list omission only.
        package_dir: None,
        custom_sections: Vec::new(),
    };
    let sections = build_system_prompt_sections(&input);
    let tools_section = sections
        .get(pir::system_prompt::SystemPromptSection::Tools)
        .expect("tools section");
    assert!(tools_section.contains("- read: Read files"));
    assert!(
        !tools_section.contains("codemode"),
        "hidden declaration omitted from the tool list: {tools_section}"
    );
}

// ---------------------------------------------------------------------------
// 4. Virtual models: catalog overlay, routing, persistence
// ---------------------------------------------------------------------------

fn virtual_registration() -> pir::ai::virtual_::VirtualModelRegistration {
    pir::ai::virtual_::VirtualModelRegistration {
        id: "virtual-x".to_string(),
        provider: "fake".to_string(),
        name: Some("Virtual X".to_string()),
        physical: pir::ai::virtual_::PhysicalModelRef {
            provider: "fake".to_string(),
            id: "fake-model".to_string(),
        },
        reasoning: Some(true),
        system_prompt: None,
        model_override: Some(pir::ai::virtual_::VirtualModelOverride {
            context_window: Some(123_456),
            max_tokens: None,
        }),
        source: "host".to_string(),
    }
}

#[test]
fn virtual_registry_registers_and_get_model_overlays_the_catalog() {
    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant_message("fake", "fake-model", "ok")
    }));
    assert!(registry.get_physical_model("fake", "fake-model").is_some());

    // A virtual id that conflicts with a physical model is rejected.
    let conflict = pir::ai::virtual_::VirtualModelRegistration {
        id: "fake-model".to_string(),
        ..virtual_registration()
    };
    assert!(registry.register_virtual_model(conflict).is_err());

    registry
        .register_virtual_model(virtual_registration())
        .expect("registration");

    // The catalog serves the virtual entry with the limit overrides applied.
    let model = registry
        .get_model("fake", "virtual-x")
        .expect("virtual model in catalog");
    assert!(pir::ai::virtual_::is_virtual_model(&model));
    assert_eq!(model.api, "pi-virtual");
    assert_eq!(model.context_window, 123_456);
    assert!(model.reasoning);

    // `getPhysicalModel` never returns the virtual entry.
    assert!(registry.get_physical_model("fake", "virtual-x").is_none());
    assert!(registry.get_physical_model("fake", "fake-model").is_some());

    // `get_models` appends the overlay entry.
    assert!(registry
        .get_models()
        .iter()
        .any(|model| model.id == "virtual-x"));

    // Unregistration removes it again.
    assert!(registry.unregister_virtual_model("fake", "virtual-x"));
    assert!(registry.get_model("fake", "virtual-x").is_none());
}

#[test]
fn virtual_model_from_wire_validates_and_parses() {
    let value = serde_json::json!({
        "id": "router",
        "physical": { "provider": "fake", "id": "fake-model" },
        "reasoning": false,
        "modelOverride": { "contextWindow": 1000 }
    });
    let registration =
        pir::ai::virtual_::VirtualModelRegistration::from_wire(&value, "plugin:x.ts")
            .expect("valid definition");
    // Provider defaults to the physical provider.
    assert_eq!(registration.provider, "fake");
    assert_eq!(registration.source, "plugin:x.ts");
    assert_eq!(
        registration.model_override.and_then(|o| o.context_window),
        Some(1000)
    );

    let missing_physical = serde_json::json!({ "id": "router" });
    assert!(
        pir::ai::virtual_::VirtualModelRegistration::from_wire(&missing_physical, "x").is_err()
    );
    let empty_id = serde_json::json!({ "id": "  ", "physical": {"provider": "fake", "id": "m"} });
    assert!(pir::ai::virtual_::VirtualModelRegistration::from_wire(&empty_id, "x").is_err());
}

/// Routing e2e: the virtual selection streams through the physical model, the
/// recorded response keeps the PHYSICAL identity (upstream
/// `agent-session.ts:614-637`; the virtual selection is a `model_change`
/// entry), and the registration persists as a `pi.virtual-model-state` custom
/// entry for resume.
#[tokio::test(flavor = "current_thread")]
async fn virtual_model_routes_to_the_physical_model_and_persists_state() {
    let temp = std::env::temp_dir().join(format!("pir-virtual-routing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant_message("fake", "fake-model", "routed answer")
    }));
    registry
        .register_virtual_model(virtual_registration())
        .unwrap();

    // The session's model is the VIRTUAL catalog entry.
    let virtual_model = registry.get_model("fake", "virtual-x").unwrap();
    let session = SessionManager::create(".", &temp).unwrap();
    let path = session.path().to_path_buf();

    // Record the model each provider request actually carries.
    let request_models: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let request_log = Arc::clone(&request_models);
    let stream_registry = Arc::clone(&registry);
    let event_stream: SharedEventStreamFn = Arc::new(move |request| {
        let model = request.model.expect("routed request carries a model");
        request_log
            .lock()
            .unwrap()
            .push((model.provider.clone(), model.id.clone()));
        let registry_registry = Arc::clone(&stream_registry);
        Box::pin(async move {
            // `ProviderRequest.context` is shared (`Arc<Context>`); the
            // provider trait takes an owned context.
            registry_registry.stream_unchecked(
                model,
                pir::agent::agent_loop::context_into_owned(request.context),
                StreamOptions::default(),
            )
        })
    });

    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: virtual_model,
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: Some(event_stream),
        extensions: None,
        tool_selection: None,
    });

    let terminal = agent.prompt("hello", Arc::new(|_| {})).await.unwrap();
    // Physical model handled the request...
    assert_eq!(
        request_models.lock().unwrap().clone(),
        vec![("fake".to_string(), "fake-model".to_string())]
    );
    // ...and the recorded response keeps the PHYSICAL identity: upstream
    // persists what the provider answered (`agent-session.ts:614-637`). The
    // former virtual stamping made every replayed assistant look like it came
    // from another model, so `is_same_model` dropped its signed thinking on the
    // next turn (P10).
    assert_eq!(terminal.provider, "fake");
    assert_eq!(terminal.model, "fake-model");
    assert!(
        pir::ai::virtual_::is_virtual_model(&registry.get_model("fake", "virtual-x").unwrap()),
        "the catalog entry itself stays virtual"
    );

    // The registration persisted as a custom entry on the branch.
    let entries = SessionManager::open(&path).unwrap().entries().to_vec();
    let state_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter_map(|entry| match entry {
            pir::session_manager::FileEntry::Custom(custom)
                if custom.custom_type == pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY =>
            {
                custom.data.as_ref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(state_entries.len(), 1, "one persisted registration");
    assert_eq!(
        state_entries[0]
            .get("provider")
            .and_then(serde_json::Value::as_str),
        Some("fake")
    );
    assert_eq!(
        state_entries[0]
            .get("state")
            .and_then(|state| state.get("id"))
            .and_then(serde_json::Value::as_str),
        Some("virtual-x")
    );

    // P10: the VIRTUAL selection is recorded on the branch as its own
    // `model_change` entry (upstream `_recordSelection`, agent-session.ts:625-638)
    // — responses can only name the physical model that answered.
    let selection = SessionManager::open(&path)
        .unwrap()
        .build_session_projection()
        .expect("projection")
        .model;
    assert_eq!(
        selection,
        Some(("fake".to_string(), "virtual-x".to_string())),
        "a resume must restore the virtual selection, not the physical response"
    );

    // Resume: a fresh session over the same storage restores the registration
    // into its registry overlay, keeping the selection routable.
    let resumed_storage = SessionManager::open(&path).unwrap();
    let resumed = AgentSession::new(AgentSessionConfig {
        provider_registry: fake_registry(Arc::new(|_index, _context| {
            assistant_message("fake", "fake-model", "again")
        })),
        model: resumed_storage
            .entries()
            .iter()
            .find_map(|entry| match entry {
                pir::session_manager::FileEntry::Custom(custom)
                    if custom.custom_type == pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY =>
                {
                    let state = custom.data.as_ref()?.get("state")?;
                    let registration =
                        pir::ai::virtual_::VirtualModelRegistration::from_wire(state, "session")
                            .ok()?;
                    Some(registration.catalog_model(None))
                }
                _ => None,
            })
            .expect("virtual selection restored"),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(resumed_storage),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    assert!(resumed
        .virtual_models()
        .iter()
        .any(|registration| registration.id == "virtual-x"));
    assert!(resumed.get_physical_model("fake", "fake-model").is_some());

    let _ = std::fs::remove_dir_all(&temp);
}

/// P10: a signed thinking block replayed under a VIRTUAL selection survives.
///
/// The session's model is the virtual catalog entry, but the request routes to
/// the physical model, so upstream persists the PHYSICAL identity on the
/// response (`agent-session.ts:614-637`) and `transform_messages`'
/// `is_same_model` still matches it on the next turn. While pir stamped the
/// virtual identity onto the persisted assistant, every replay looked like a
/// model switch and dropped the signed thinking — losing Anthropic
/// thinking-cache replays and OpenAI-Responses encrypted reasoning on exactly
/// the routed runs.
#[tokio::test(flavor = "current_thread")]
async fn virtual_selection_replays_signed_thinking() {
    let temp = std::env::temp_dir().join(format!(
        "pir-virtual-thinking-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let registry = fake_registry(Arc::new(|_index, _context| AssistantMessage {
        api: "fake-api".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "reasoning the provider signed".into(),
                thinking_signature: Some("sig-abc".into()),
                redacted: false,
            }),
            text("routed answer"),
        ],
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }));
    registry
        .register_virtual_model(virtual_registration())
        .unwrap();

    let session = SessionManager::create(".", &temp).unwrap();
    let path = session.path().to_path_buf();
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: registry.get_model("fake", "virtual-x").unwrap(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    let terminal = agent.prompt("hello", Arc::new(|_| {})).await.unwrap();
    assert_eq!(terminal.model, "fake-model", "physical identity persisted");

    let messages = SessionManager::open(&path)
        .unwrap()
        .build_session_projection()
        .expect("projection")
        .messages;
    let stored_thinking = messages.iter().find_map(|message| match message {
        Message::Assistant(assistant) => assistant.content.iter().find_map(|block| match block {
            ContentBlock::Thinking(thinking) => Some(thinking.clone()),
            _ => None,
        }),
        _ => None,
    });
    assert_eq!(
        stored_thinking.and_then(|thinking| thinking.thinking_signature),
        Some("sig-abc".into()),
        "the signed block is stored with the response"
    );

    // The next turn's replay: transform the stored transcript against the
    // physical model the router sends the request to.
    let replayed = pir::ai::api::transform::transform_messages(&messages, &physical_model(), None);
    let replayed_blocks: Vec<&ContentBlock> = replayed
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.content.iter()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        replayed_blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Thinking(thinking) if thinking.thinking_signature.as_deref() == Some("sig-abc")
        )),
        "signed thinking must replay under a virtual selection, got {replayed_blocks:?}"
    );

    let _ = std::fs::remove_dir_all(&temp);
}

/// Compaction checks the ROUTED model's context window: a virtual selection
/// with a tiny `modelOverride.contextWindow` compacts even though the physical
/// model's window is large (upstream checks `route.model` in prepareRequest).
#[tokio::test(flavor = "current_thread")]
async fn compaction_threshold_uses_the_routed_virtual_model() {
    let temp = std::env::temp_dir().join(format!("pir-virtual-compact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant_message("fake", "fake-model", "summary of the conversation")
    }));
    // Physical window 4096 would never compact; the override window (small)
    // makes the ROUTED model's threshold fire.
    let mut registration = virtual_registration();
    registration.model_override = Some(pir::ai::virtual_::VirtualModelOverride {
        context_window: Some(100),
        max_tokens: None,
    });
    registry.register_virtual_model(registration).unwrap();

    let mut session = SessionManager::create(".", &temp).unwrap();
    let path = session.path().to_path_buf();
    // Seed a transcript that already exceeds the tiny routed window so the
    // pre-response threshold check fires on the first request.
    let filler = "filler ".repeat(200);
    session
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text(&filler)],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .unwrap();
    session
        .append_message(Message::Assistant(assistant_message(
            "fake",
            "fake-model",
            "ack",
        )))
        .unwrap();
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: registry.get_model("fake", "virtual-x").unwrap(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: Some(pir::compaction::CompactionConfig {
            enabled: true,
            reserve_tokens: 50,
            keep_recent_tokens: 0,
        }),
        max_turns: 4,
        system_prompt: None,
        cwd: Some(temp.clone()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    let terminal = agent
        .prompt(
            "filler ".repeat(200).trim().to_string().as_str(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    // The compaction entry (reason: threshold against the routed model) landed.
    let entries = SessionManager::open(&path).unwrap().entries().to_vec();
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, pir::session_manager::FileEntry::Compaction(_))),
        "threshold compaction ran against the routed model"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

/// Session-level exposure: an inline Rust plugin registers tools of every
/// exposure; the request declares only the active direct/model-only tools,
/// `codemode` stays callable-but-undeclared, and `hidden` is unreachable.
#[tokio::test(flavor = "current_thread")]
async fn session_declares_only_the_active_direct_and_model_only_tools() {
    static DECLARED_TOOLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct ExposurePlugin;

    impl pir::plugins::AgentPlugin for ExposurePlugin {
        fn info(&self) -> &pir::plugins::PluginInfo {
            static INFO: std::sync::OnceLock<pir::plugins::PluginInfo> = std::sync::OnceLock::new();
            INFO.get_or_init(|| pir::plugins::PluginInfo::inline("exposure-test"))
        }

        fn register(&self, ctx: &mut pir::plugins::RegistrationContext<'_>) -> anyhow::Result<()> {
            ctx.register_tool(Arc::new(ExposedTool::new(
                "direct_tool",
                ToolExposure::Direct,
            )));
            ctx.register_tool(Arc::new(ExposedTool::new(
                "model_only_tool",
                ToolExposure::ModelOnly,
            )));
            ctx.register_tool(Arc::new(ExposedTool::new(
                "codemode_tool",
                ToolExposure::Codemode,
            )));
            ctx.register_tool(Arc::new(ExposedTool::new(
                "hidden_tool",
                ToolExposure::Hidden,
            )));
            Ok(())
        }
    }

    let registry = fake_registry(Arc::new(|_index, context| {
        // Record the declared tool names the model actually sees.
        let declared: Vec<String> = context
            .tools
            .iter()
            .flatten()
            .map(|tool| tool.name.clone())
            .collect();
        *DECLARED_TOOLS.lock().unwrap() = declared;
        assistant_message("fake", "fake-model", "ok")
    }));

    let agent = AgentSession::new_with_plugins(
        AgentSessionConfig {
            provider_registry: registry.clone(),
            model: physical_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![Arc::new(ExposurePlugin)],
    );

    let terminal = agent.prompt("hello", Arc::new(|_| {})).await.unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Stop);

    let declared = DECLARED_TOOLS.lock().unwrap().clone();
    assert_eq!(
        declared,
        vec!["direct_tool", "model_only_tool"],
        "only active direct/model-only tools are declared to the model"
    );
}

// ---------------------------------------------------------------------------
// 5. JS surface: registerTool exposure fields, prepareLoadout,
//    registerVirtualModel / unregisterVirtualModel / getPhysicalModel
// ---------------------------------------------------------------------------

fn js_config(cwd: &std::path::Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn seed_and_load(
    handle: &pir::extensions::ExtensionHandle,
    dir: &std::path::Path,
    name: &str,
    source: &str,
) {
    std::fs::write(dir.join(name), source).unwrap();
    let path = dir.join(name);
    let result = handle.load(std::slice::from_ref(&path)).await.unwrap();
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

const EXPOSURE_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.registerTool({
    name: "codemode_js",
    description: "runs scripts",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
    exposure: "codemode",
    namespace: { name: "mcp__docs", description: "docs server" },
    annotations: { readOnlyHint: true, destructiveHint: false },
    defaultActive: false,
  });
  pi.registerTool({
    name: "loadout_js",
    description: "original",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
    prepareLoadout: (loadout) => ({
      descriptions: { loadout_js: "rewritten: " + loadout.callable.join(",") },
      hiddenDeclarations: ["loadout_js"],
    }),
  });
  pi.registerVirtualModel({
    id: "js-virtual",
    physical: { provider: "fake", id: "fake-model" },
    reasoning: true,
    systemPrompt: "be brief",
    modelOverride: { contextWindow: 7777 },
  });
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn js_register_tool_parses_exposure_fields_and_loadout_hook() {
    let dir = std::env::temp_dir().join(format!("pir-exposure-js-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let handle = pir::extensions::ExtensionHandle::start(js_config(&dir)).unwrap();
    seed_and_load(&handle, &dir, "exposure.ts", EXPOSURE_EXTENSION_TS).await;

    let defs = handle.tool_defs();
    let codemode = defs
        .iter()
        .find(|def| def.name == "codemode_js")
        .expect("codemode_js registered");
    assert_eq!(codemode.exposure, "codemode");
    assert!(!codemode.default_active);
    let namespace = codemode.namespace.as_ref().expect("namespace carried");
    assert_eq!(namespace.name, "mcp__docs");
    assert_eq!(namespace.description.as_deref(), Some("docs server"));
    let annotations = codemode.annotations.expect("annotations carried");
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));

    let loadout = defs
        .iter()
        .find(|def| def.name == "loadout_js")
        .expect("loadout_js registered");
    assert!(loadout.prepare_loadout, "hook flagged on the ToolDef");

    // The JS hook runs through the runtime and returns its changes.
    let loadout_json = serde_json::json!({
        "declared": [{ "name": "loadout_js", "description": "original" }],
        "callable": ["codemode_js"],
        "registered": ["codemode_js", "loadout_js"],
    });
    let changes = handle
        .call_prepare_loadout("loadout_js", &loadout_json)
        .await
        .expect("hook ran")
        .expect("changes returned");
    assert_eq!(
        changes
            .get("descriptions")
            .and_then(|d| d.get("loadout_js"))
            .and_then(serde_json::Value::as_str),
        Some("rewritten: codemode_js")
    );
    assert_eq!(
        changes
            .get("hiddenDeclarations")
            .and_then(serde_json::Value::as_array)
            .map(|names| names.len()),
        Some(1)
    );

    // Exposure metadata flows into the AgentTool wrapper.
    let mut registry = ToolRegistry::new();
    pir::agent::tool_execution::merge_extension_tools(&mut registry, &handle);
    let tool = registry.get("codemode_js").expect("merged");
    assert_eq!(tool.exposure(), ToolExposure::Codemode);
    assert!(!tool.default_active());
    assert_eq!(
        tool.namespace().map(|namespace| namespace.name),
        Some("mcp__docs".to_string())
    );
    assert_eq!(
        tool.annotations()
            .map(|annotations| annotations.read_only_hint),
        Some(Some(true))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn js_register_virtual_model_and_physical_accessor() {
    let dir = std::env::temp_dir().join(format!("pir-virtual-js-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let handle = pir::extensions::ExtensionHandle::start(js_config(&dir)).unwrap();
    seed_and_load(&handle, &dir, "virtual.ts", EXPOSURE_EXTENSION_TS).await;

    let defs = handle.virtual_model_defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(
        defs[0].get("id").and_then(serde_json::Value::as_str),
        Some("js-virtual")
    );
    assert!(
        defs[0]
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .starts_with("plugin:"),
        "definition tagged with its plugin source"
    );

    // getPhysicalModel reads the host-pushed registry snapshot and never
    // returns a virtual entry.
    let models = serde_json::json!([
        serde_json::to_value(physical_model()).unwrap(),
        { "id": "js-virtual", "api": "pi-virtual", "provider": "fake" }
    ]);
    handle.update_model_registry(models, serde_json::json!([]));
    let physical = handle
        .get_physical_model("fake", "fake-model")
        .expect("physical model found");
    assert_eq!(
        physical.get("api").and_then(serde_json::Value::as_str),
        Some("fake-api")
    );
    assert!(handle.get_physical_model("fake", "js-virtual").is_none());

    // Unregistration removes the definition.
    handle.remove_virtual_model("fake", "js-virtual");
    assert!(handle.virtual_model_defs().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Plugin-registered virtual models sync into the session's provider registry
/// at prompt time and persist as `pi.virtual-model-state` entries.
#[tokio::test(flavor = "current_thread")]
async fn js_virtual_model_syncs_into_the_session_and_persists() {
    let dir = std::env::temp_dir().join(format!("pir-virtual-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let handle = pir::extensions::ExtensionHandle::start(js_config(&dir)).unwrap();
    seed_and_load(&handle, &dir, "virtual.ts", EXPOSURE_EXTENSION_TS).await;

    let registry = fake_registry(Arc::new(|_index, _context| {
        assistant_message("fake", "fake-model", "ok")
    }));
    let session = SessionManager::create(".", &dir).unwrap();
    let path = session.path().to_path_buf();
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: registry.clone(),
        model: physical_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 4,
        system_prompt: None,
        cwd: Some(dir.clone()),
        event_stream: None,
        extensions: Some(handle),
        tool_selection: None,
    });

    agent.prompt("hello", Arc::new(|_| {})).await.unwrap();

    // The plugin registration reached the overlay...
    assert!(agent
        .virtual_models()
        .iter()
        .any(|registration| registration.id == "js-virtual"));
    // ...and persisted for resume.
    let entries = SessionManager::open(&path).unwrap().entries().to_vec();
    assert!(entries.iter().any(|entry| matches!(
        entry,
        pir::session_manager::FileEntry::Custom(custom)
            if custom.custom_type == pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY
    )));
    let _ = std::fs::remove_dir_all(&dir);
}
