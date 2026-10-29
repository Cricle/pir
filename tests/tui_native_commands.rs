//! Native plugin slash commands reachable from the TUI (`/phone-start`,
//! `/todos`, ...).
//!
//! The TUI slash handler used to consult only the JS extension runtime's
//! `command_defs()`, so a name registered by a Rust builtin (`/phone-start`,
//! `/todos`) fell through to the normal prompt submission and was sent to the
//! model. The integration fix has two seams, pinned here without a terminal:
//!
//! 1. the pure route decision ([`non_builtin_command_route`]): a native name
//!    dispatches through the worker, a JS name keeps the extension-runtime
//!    path, and an unknown name still falls through to the prompt;
//! 2. the worker seam ([`AgentSession::native_plugin_command_names`] /
//!    [`AgentSession::dispatch_plugin_command`], the calls behind
//!    `WorkerCommand::DispatchPluginCommand`): the session's `PluginHost`
//!    registry reports the native names, and dispatch runs the registered
//!    Rust handler with the typed argument.
//!
//! `handle_command` itself is crossterm/UI-bound (private `App` +
//! `WorkerHandle`), so the route helper and the worker seam are tested
//! separately, as the TUI's other dispatch contracts are.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::extensions::CommandOutcome;
use pir::plugins::{AgentPlugin, CommandBackend, CommandHandler, PluginInfo, RegistrationContext};
use pir::tui::terminal::{non_builtin_command_route, unknown_command_route, UnknownCommandRoute};

// ---------------------------------------------------------------------------
// Route decision
// ---------------------------------------------------------------------------

#[test]
fn native_js_and_unknown_routes_stay_distinct() {
    // A native (Rust backend) name dispatches — NOT a prompt.
    assert_eq!(
        non_builtin_command_route(true, false),
        UnknownCommandRoute::NativePlugin
    );
    // Native wins when both registries know the name; the PluginHost
    // registry resolves its own entry first, so the worker dispatch keeps
    // the native handler.
    assert_eq!(
        non_builtin_command_route(true, true),
        UnknownCommandRoute::NativePlugin
    );
    // A JS-only name keeps the existing extension-runtime dispatcher path.
    assert_eq!(
        non_builtin_command_route(false, true),
        UnknownCommandRoute::Extension
    );
    // An unknown name still falls through to the prompt (upstream's
    // `/skill:<name>` / template path).
    assert_eq!(
        non_builtin_command_route(false, false),
        UnknownCommandRoute::Prompt
    );
    // The legacy JS-only helper is unchanged.
    assert_eq!(unknown_command_route(true), UnknownCommandRoute::Extension);
    assert_eq!(unknown_command_route(false), UnknownCommandRoute::Prompt);
}

// ---------------------------------------------------------------------------
// Worker seam: session PluginHost snapshot + dispatch
// ---------------------------------------------------------------------------

struct NativeCommandPlugin {
    info: PluginInfo,
    runs: Arc<AtomicUsize>,
    last_args: Arc<Mutex<String>>,
}

impl NativeCommandPlugin {
    fn new(runs: Arc<AtomicUsize>, last_args: Arc<Mutex<String>>) -> Self {
        Self {
            info: PluginInfo::inline("native-command-probe"),
            runs,
            last_args,
        }
    }
}

impl AgentPlugin for NativeCommandPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        let runs = Arc::clone(&self.runs);
        let last_args = Arc::clone(&self.last_args);
        let handler: CommandHandler = Arc::new(move |args: &str| {
            let runs = Arc::clone(&runs);
            let last_args = Arc::clone(&last_args);
            let args = args.to_owned();
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                *last_args
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = args.clone();
                CommandOutcome {
                    ok: true,
                    error: Some(format!("started on port {args}")),
                }
            })
        });
        ctx.register_command(
            "phone-start",
            Some("Start the phone server"),
            Some(CommandBackend::Rust(handler)),
        );
        Ok(())
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

fn agent_config(cwd: PathBuf) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_plugin_command_is_exposed_and_dispatches() {
    let cwd = std::env::temp_dir().join(format!(
        "pir-tui-native-commands-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&cwd).expect("temp dir");

    let runs = Arc::new(AtomicUsize::new(0));
    let last_args = Arc::new(Mutex::new(String::new()));
    let plugin: Arc<dyn AgentPlugin> = Arc::new(NativeCommandPlugin::new(
        Arc::clone(&runs),
        Arc::clone(&last_args),
    ));
    // Inline plugin, no JS runtime: exactly the builtin-only session shape
    // the TUI worker builds for a default extension plan.
    let session = AgentSession::new_with_plugins(agent_config(cwd.clone()), vec![plugin]);

    // The snapshot the worker sends as `WorkerEvent::NativeCommands`.
    let names = session.native_plugin_command_names();
    assert!(
        names.iter().any(|name| name == "phone-start"),
        "the native registry entry must be part of the TUI snapshot: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "skill:unknown"),
        "unknown names must not be advertised as native commands: {names:?}"
    );

    // The dynamic autocomplete metadata comes from the same registry entry:
    // names alone are not enough — the slash menu shows the registry's
    // description without any hardcoded list in the TUI.
    let specs = session.native_plugin_command_specs();
    let (_, description) = specs
        .iter()
        .find(|(name, _)| name == "phone-start")
        .expect("the native registry entry must carry autocomplete metadata");
    assert_eq!(description.as_deref(), Some("Start the phone server"));

    // The slash route built from that snapshot chooses DISPATCH (not prompt)
    // for the native name, and still falls through for an unknown name.
    let is_native = |candidate: &str| names.iter().any(|name| name == candidate);
    assert_eq!(
        non_builtin_command_route(is_native("phone-start"), false),
        UnknownCommandRoute::NativePlugin,
        "`/phone-start 8799` must not be submitted to the model"
    );
    assert_eq!(
        non_builtin_command_route(is_native("skill:unknown"), false),
        UnknownCommandRoute::Prompt
    );

    // The worker command's dispatch (`handle_session_command` →
    // `AgentSession::dispatch_plugin_command`): the handler runs and its
    // display text (the `CommandOutcome.error` channel, print-mode parity)
    // comes back.
    let outcome = session
        .dispatch_plugin_command("phone-start", "8799")
        .await
        .expect("native command dispatch");
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.error.as_deref(), Some("started on port 8799"));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the registered handler ran");
    assert_eq!(
        last_args
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_str(),
        "8799"
    );

    // An unregistered name is an error (the TUI reports it as a notice), not
    // a silently swallowed prompt.
    let error = session
        .dispatch_plugin_command("no-such-command", "")
        .await
        .expect_err("unknown command must fail");
    assert!(
        error.to_string().contains("not registered"),
        "unexpected error text: {error}"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

/// The TUI feeds native plugin commands into the slash menu through the
/// `CommandProvider` seam (never a fixed list): whatever the session registry
/// reports appears, with its description, and clearing the provider removes
/// it again.
#[test]
fn native_command_provider_feeds_the_slash_menu() {
    use pir::tui::autocomplete::{
        register_native_command_provider, CombinedAutocompleteProvider, CommandProvider,
        SlashCommand,
    };

    struct StaticCommands(Vec<SlashCommand>);
    impl CommandProvider for StaticCommands {
        fn commands(&self) -> Vec<SlashCommand> {
            self.0.clone()
        }
    }

    register_native_command_provider(Some(Arc::new(StaticCommands(vec![SlashCommand::new(
        "phone-start",
        "Start the phone server",
    )]))));
    let provider = CombinedAutocompleteProvider::with_builtin_commands(PathBuf::from("."));
    let suggestions = provider
        .suggestions(&["/phone-start".to_string()], 0, 12, false)
        .expect("the native command must appear in the slash menu");
    assert!(
        suggestions.items.iter().any(|item| {
            item.value == "phone-start"
                && item.description.as_deref() == Some("Start the phone server")
        }),
        "native suggestion missing: {:?}",
        suggestions.items
    );

    register_native_command_provider(None);
    assert!(
        pir::tui::autocomplete::native_command_provider().is_none(),
        "clearing the provider must take effect"
    );
}
