//! W16-M: the native-plugin `EventInterest` contract vs `on_event` arms.
//!
//! A change made native plugin event dispatch filter by each plugin's declared
//! [`EventInterest`]. The risk class this suite pins: a native plugin that
//! silently relied on receiving an event it never declared gets nothing, and
//! the two places that answer "is anyone interested?" must agree.
//!
//! ## The three answers a dispatch path can give (post-fix state)
//!
//! 1. [`PluginHost::has_native_handlers`] (`src/plugins/host.rs:1332-1336`) —
//!    `plugin.events().matches(event)`, where
//!    [`EventInterest::matches`] (`src/plugins/plugin.rs:289-295`) returns
//!    **false for `EventInterest::None`**. Every gated emitter uses this as a
//!    pre-gate (`agent/agent_loop.rs:67`, `:105`, `:1649`,
//!    `agent_session/session_store.rs:133`, `notifications.rs:44`, ...).
//! 2. `native_plugin_wants_event` (`src/plugins/host.rs:2330-2347`) — used by
//!    `emit`, `emit_native`, `emit_read_only_json`, `dispatch_terminal_input`.
//!    It now also returns **false for `EventInterest::None`**, so (1) and (2)
//!    are the same function of the declaration.
//! 3. `dispatch_hosted_event` (`src/agent/tool_execution.rs:1307-1344`) —
//!    `tool_call` / `tool_result` / `user_bash` — applies **no interest filter
//!    at all**: every non-JS plugin's `on_event` runs.
//!
//! ## Documented residual: (3) deliberately ignores declarations
//!
//! `dispatch_hosted_event` is the ONE dispatch entry that does not consult
//! `EventInterest`, and it stays that way: `todo`'s idempotent `tool_result`
//! REPLACE arm (`src/plugins/builtin/todo.rs:2100-2123`) is reachable only
//! through it. It is not reachable through (1)/(2) because `todo` declares
//! only `session_start` — so for `tool_call`/`tool_result`/`user_bash` an
//! undeclared arm still runs when the hosted walk fires. No in-tree plugin
//! makes that path load-bearing except `todo`, whose arm is a no-op duplicate
//! of the live tool commit. Do not "fix" it to filter; the parity contract
//! (`tool_result` handlers see every finalized result) relies on the walk.
//!
//! Everything here is hermetic: inline plugins and the builtin factories
//! (`instantiate_builtin` — construction only, no `register()` side effects),
//! no JS runtime, no network, no model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pir::agent::tool_batch::{FinalizedToolCall, ToolResult};
use pir::agent::tool_execution::{AgentTool, ToolContext};
use pir::plugins::{
    builtin_names, instantiate_builtin, AgentPlugin, EventInterest, PluginEvent,
    PluginEventOutcome, PluginFuture, PluginHost, PluginInfo, RegistrationContext,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A native plugin whose declaration and `on_event` behaviour are independent,
/// so the two can be compared. It records every event the fan-out hands it.
struct Recorder {
    info: PluginInfo,
    interest: EventInterest,
    seen: Arc<Mutex<Vec<String>>>,
    /// Optional decision returned for `terminal_input` (the only outcome the
    /// terminal fan-out reads back).
    consume_terminal_input: bool,
}

impl Recorder {
    fn new(name: &str, interest: EventInterest) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                info: PluginInfo::inline(name),
                interest,
                seen: Arc::clone(&seen),
                consume_terminal_input: false,
            }),
            seen,
        )
    }

    fn consuming_terminal_input(name: &str, interest: EventInterest) -> Arc<Self> {
        Arc::new(Self {
            info: PluginInfo::inline(name),
            interest,
            seen: Arc::new(Mutex::new(Vec::new())),
            consume_terminal_input: true,
        })
    }
}

impl AgentPlugin for Recorder {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn events(&self) -> EventInterest {
        self.interest.clone()
    }

    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
        let name = event.name.to_string();
        let seen = Arc::clone(&self.seen);
        let consume = self.consume_terminal_input;
        Box::pin(async move {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(name.clone());
            if consume && name == "terminal_input" {
                PluginEventOutcome::TerminalInput {
                    consume: true,
                    data: None,
                }
            } else {
                PluginEventOutcome::None
            }
        })
    }
}

fn drain_seen(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// `Vec<Arc<Concrete>>` does not coerce to `Vec<Arc<dyn AgentPlugin>>`, so
/// every fixture goes through here before it reaches `PluginHost::build`.
fn as_plugin<T: AgentPlugin + 'static>(plugin: Arc<T>) -> Arc<dyn AgentPlugin> {
    plugin
}

// ---------------------------------------------------------------------------
// (2) `native_plugin_wants_event`: an explicit `List` is the dispatch fence
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn list_interest_is_a_dispatch_fence_for_emit_and_emit_native() {
    let (plugin, seen) = Recorder::new("w16-list", EventInterest::List(vec!["input".to_owned()]));
    let host = PluginHost::build(None, vec![as_plugin(plugin)]);

    // Declared: dispatched by both entries, and reported by both predicates.
    let mut payload = json!({ "type": "input", "text": "hello" });
    host.emit_native("input", &mut payload).await;
    assert_eq!(drain_seen(&seen), vec!["input".to_owned()]);

    // Undeclared: `emit_native` / `emit` skip it (the declaration is the fence).
    let mut payload = json!({ "type": "tool_result", "toolName": "x" });
    host.emit_native("tool_result", &mut payload).await;
    let mut payload = json!({ "type": "message_update" });
    host.emit("message_update", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        drain_seen(&seen),
        vec!["input".to_owned()],
        "an undeclared event must not reach a List plugin through emit/emit_native"
    );

    assert!(host.has_native_handlers("input"));
    assert!(!host.has_native_handlers("tool_result"));
    assert!(!host.has_handlers("tool_result"));
    assert!(host.has_handlers("input"));
}

#[tokio::test(flavor = "current_thread")]
async fn list_interest_fences_the_read_only_dispatch_too() {
    let (tree_plugin, tree_seen) = Recorder::new(
        "w16-read-only-tree",
        EventInterest::List(vec!["session_tree".to_owned()]),
    );
    let (other, other_seen) = Recorder::new(
        "w16-read-only-other",
        EventInterest::List(vec!["input".to_owned()]),
    );
    let host = PluginHost::build(None, vec![as_plugin(tree_plugin), as_plugin(other)]);

    let mut payload = json!({ "type": "session_tree" });
    host.emit_read_only_json("session_tree", "{}".to_owned(), Some(&mut payload))
        .await;

    assert_eq!(drain_seen(&tree_seen), vec!["session_tree".to_owned()]);
    assert!(
        drain_seen(&other_seen).is_empty(),
        "emit_read_only_json must honor the same List fence as emit"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn list_interest_fences_the_terminal_input_fan_out() {
    let interested = Recorder::consuming_terminal_input(
        "w16-terminal-interested",
        EventInterest::List(vec!["terminal_input".to_owned()]),
    );
    let uninterested = Recorder::consuming_terminal_input(
        "w16-terminal-uninterested",
        EventInterest::List(vec!["input".to_owned()]),
    );
    let host = PluginHost::build(None, vec![as_plugin(interested), as_plugin(uninterested)]);

    let action = host.dispatch_terminal_input("x").await;
    assert_eq!(
        action,
        pir::plugins::services::TerminalInputAction::Consume,
        "the declaring plugin's consume decision wins"
    );
    // The uninterested plugin would also consume if it ran; the walk short-
    // circuits on the first plugin, so prove the fence separately.
    let (only_uninterested, only_seen) = Recorder::new(
        "w16-terminal-fence",
        EventInterest::List(vec!["input".to_owned()]),
    );
    let host = PluginHost::build(None, vec![as_plugin(only_uninterested)]);
    let action = host.dispatch_terminal_input("x").await;
    assert_eq!(
        action,
        pir::plugins::services::TerminalInputAction::Pass,
        "a plugin that did not declare terminal_input must not be consulted"
    );
    assert!(drain_seen(&only_seen).is_empty());
}

// ---------------------------------------------------------------------------
// The declaration is the single source of truth (the fix)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn all_interest_is_advertised_and_dispatched() {
    let (plugin, seen) = Recorder::new("w16-all", EventInterest::All);
    let host = PluginHost::build(None, vec![as_plugin(plugin)]);

    assert!(host.has_native_handlers("anything"));
    assert!(host.has_handlers("anything"));

    let mut payload = json!({});
    host.emit_native("anything", &mut payload).await;
    let mut payload = json!({});
    host.emit("other", &mut payload).await.expect("emit");
    assert_eq!(
        drain_seen(&seen),
        vec!["anything".to_owned(), "other".to_owned()]
    );
}

/// THE FIXED CONTRACT: the default `EventInterest::None` admits NOTHING on
/// every path that consults a declaration.
///
/// A native plugin that implements `on_event` but leaves the default
/// `EventInterest::None` is (post-fix) never dispatched to by the ordered
/// fan-outs and never advertised as a handler — the two answers agree:
/// - `has_native_handlers` / `has_handlers` return false
///   (`host.rs:1332-1336` → `EventInterest::matches`, `plugin.rs:291`), and
/// - `emit` / `emit_native` / `emit_read_only_json` /
///   `dispatch_terminal_input` — which reach the decision through
///   `native_plugin_wants_event` (`host.rs:2330-2347`, `None => false`) — skip
///   the plugin too.
///
/// This is the property the earlier `None => true` arm broke: it made the
/// plugin's delivery depend on whether an emitter happened to pre-gate on
/// `has_native_handlers` (`agent_loop.rs:67`/`:105`/`:1649`,
/// `session_store.rs:133`, `notifications.rs:44`, `build.rs:44`,
/// `runtime_loader.rs:777`, `tools.rs:273`/`:333`, `mcp.rs:1146`) or emitted
/// straight into `emit` (`session_store.rs:69`, `:395`, `:655`,
/// `compact.rs:303`, `:569`, `settle.rs:262`, `:479`,
/// `stream.rs:173`/`:203`/`:504`, `coding_agent/print.rs:299`/`:368`).
///
/// The one remaining unfiltered path is `dispatch_hosted_event` (see the
/// module doc and the test below); it is deliberate.
#[tokio::test(flavor = "current_thread")]
async fn none_interest_is_neither_dispatched_nor_advertised() {
    // No `events()` override => `EventInterest::None`.
    struct DefaultInterestPlugin {
        info: PluginInfo,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl AgentPlugin for DefaultInterestPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let name = event.name.to_string();
            let seen = Arc::clone(&self.seen);
            Box::pin(async move {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(name);
                PluginEventOutcome::None
            })
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![as_plugin(Arc::new(DefaultInterestPlugin {
            info: PluginInfo::inline("w16-default-interest"),
            seen: Arc::clone(&seen),
        }))],
    );

    // (1) Every gate says "nobody is interested".
    assert!(
        !host.has_native_handlers("input"),
        "EventInterest::None must not advertise a handler (host.rs:1332-1336)"
    );
    assert!(!host.has_handlers("input"));

    // (2) The fan-out agrees: nothing is delivered, gated or not.
    let mut payload = json!({ "type": "input", "text": "x" });
    host.emit_native("input", &mut payload).await;
    let mut payload = json!({ "type": "turn_end" });
    host.emit("turn_end", &mut payload).await.expect("emit");
    let mut payload = json!({ "type": "session_tree" });
    host.emit_read_only_json("session_tree", "{}".to_owned(), Some(&mut payload))
        .await;

    assert!(
        drain_seen(&seen).is_empty(),
        "EventInterest::None must admit no event on emit / emit_native / \
         emit_read_only_json (native_plugin_wants_event, host.rs:2330-2347)"
    );

    // The gated-emitter shape used by the agent loop (`agent_loop.rs:63-71`):
    // same answer, no double standard.
    let mut payload = json!({ "type": "tool_execution_update" });
    if host.has_native_handlers("tool_execution_update") {
        host.emit_native("tool_execution_update", &mut payload)
            .await;
    }
    assert!(drain_seen(&seen).is_empty());

    // The terminal-input path, one level deeper. `has_terminal_input_handlers`
    // (host.rs:869-876, built from `events().matches` at host.rs:796-798) and
    // `dispatch_terminal_input` (host.rs:926-934, via
    // `native_plugin_wants_event`) now agree: a `None`-declaring plugin is
    // never consulted, so its `TerminalInput { consume: true }` is genuinely
    // dead code. No in-tree plugin is in this state (every native `on_event`
    // override declares a `List`), which the audit test asserts.
    let consuming = Recorder::consuming_terminal_input("w16-none-consumer", EventInterest::None);
    let terminal_host = PluginHost::build(None, vec![as_plugin(consuming)]);
    assert!(
        !terminal_host.has_terminal_input_handlers(),
        "has_terminal_input_handlers uses matches() (None => false)"
    );
    assert_eq!(
        terminal_host.dispatch_terminal_input("x").await,
        pir::plugins::services::TerminalInputAction::Pass,
        "dispatch_terminal_input now uses the same None => false rule, so the \
         handler is unreachable (not Consume)"
    );
}

// ---------------------------------------------------------------------------
// Documented residual: `dispatch_hosted_event` deliberately ignores the fence
// ---------------------------------------------------------------------------

/// The hosted `tool_call` / `tool_result` / `user_bash` walk
/// (`src/agent/tool_execution.rs:1307-1344`) calls `on_event` on every non-JS
/// plugin with no `native_plugin_wants_event` check. This is the ONE remaining
/// path that bypasses a declaration, and it stays that way on purpose: `todo`'s
/// idempotent `tool_result` REPLACE arm is reachable only through it, and the
/// `tool_result` parity contract (every finalized result reaches handlers)
/// depends on the unfiltered walk.
///
/// Pinned so nobody "fixes" it in passing: if this assertion ever starts
/// failing because the walk began filtering, `todo`'s defensive arm becomes
/// dead code and this test should be deleted together with that arm — not
/// silently relaxed.
#[tokio::test(flavor = "current_thread")]
async fn hosted_tool_result_walk_ignores_the_declaration_fence() {
    let (plugin, seen) = Recorder::new(
        "w16-hosted-bypass",
        EventInterest::List(vec!["session_start".to_owned()]),
    );
    let host = PluginHost::build(None, vec![as_plugin(plugin)]);

    assert!(!host.has_native_handlers("tool_result"));

    // Sanity: the ordered fan-out honors the fence.
    let mut payload = json!({ "type": "tool_result" });
    host.emit_native("tool_result", &mut payload).await;
    assert!(drain_seen(&seen).is_empty());

    // The hosted result walk does not.
    let results = vec![FinalizedToolCall {
        index: 0,
        call_id: "call-1".to_owned(),
        tool_name: "some-tool".to_owned(),
        result: ToolResult::ok("done"),
        usage: None,
        duration_ms: None,
    }];
    let _ = pir::agent::tool_execution::emit_tool_results_with_host(
        None,
        Some(&host),
        &HashMap::<String, serde_json::Value>::new(),
        &results,
        None,
    )
    .await;

    assert_eq!(
        drain_seen(&seen),
        vec!["tool_result".to_owned()],
        "dispatch_hosted_event must deliver regardless of declared interest \
         (agent/tool_execution.rs:1340-1343) — the todo defensive arm relies on it"
    );
}

// ---------------------------------------------------------------------------
// Audit: every compiled builtin's declaration
// ---------------------------------------------------------------------------

/// The audited declaration table for every native builtin compiled into this
/// build. Adding a builtin (or an `on_event` override to a handler-free one)
/// must update this list — that is the point of the test.
fn expected_builtin_events() -> Vec<(&'static str, EventInterest)> {
    vec![
        // Handler-free builtins: tool/command registrations only, default
        // `EventInterest::None` (verified: no `on_event` override in each module).
        #[cfg(feature = "subagent")]
        ("subagent", EventInterest::None),
        #[cfg(feature = "extensions")]
        ("codemode", EventInterest::None),
        #[cfg(feature = "python")]
        ("python", EventInterest::None),
        #[cfg(feature = "tool-search")]
        ("tool-search", EventInterest::None),
        // `session_start` only: connect/discover (mcp) and replay re-arm (todo).
        #[cfg(feature = "mcp")]
        ("mcp", EventInterest::List(vec!["session_start".to_owned()])),
        #[cfg(feature = "todo")]
        (
            "todo",
            EventInterest::List(vec!["session_start".to_owned()]),
        ),
        // Two enforcement hooks + identity capture.
        #[cfg(feature = "context-mode")]
        (
            "context-mode",
            EventInterest::List(vec![
                "tool_call".to_owned(),
                "tool_result".to_owned(),
                "session_start".to_owned(),
            ]),
        ),
        // Phase-0 request sanitiser; `message_end` rides the dedicated
        // `message_end_repair` seam (`agent_session/tool_repair.rs:58`), not events.
        #[cfg(feature = "tool-repair")]
        (
            "tool-repair",
            EventInterest::List(vec!["before_provider_request".to_owned()]),
        ),
        // The phone feed: exactly the bridge's mapping table.
        #[cfg(feature = "phone")]
        (
            "phone",
            EventInterest::List(
                pir::plugins::builtin::phone::PHONE_HOST_EVENT_NAMES
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
            ),
        ),
    ]
}

#[test]
fn every_compiled_builtin_declares_the_audited_event_set() {
    let expected = expected_builtin_events();
    let compiled = builtin_names();

    // Completeness first: a new builtin must be added to the audit table.
    let unaudited: Vec<&str> = compiled
        .iter()
        .copied()
        .filter(|name| !expected.iter().any(|(audited, _)| audited == name))
        .collect();
    assert!(
        unaudited.is_empty(),
        "builtins with no audited EventInterest declaration: {unaudited:?} — \
         add them to expected_builtin_events() (and declare `events()` if they \
         override on_event)"
    );

    for (name, want) in expected {
        let plugin = instantiate_builtin(name)
            .unwrap_or_else(|| panic!("builtin `{name}` is listed but did not instantiate"));
        assert_eq!(
            plugin.events(),
            want,
            "builtin `{name}` declaration drifted; on_event arms must be covered"
        );
    }
}

/// A declared name that is not a real wired/native-only event can never match,
/// so the declaration silently disables the handler. Catch typos here rather
/// than in a live session.
#[test]
fn every_declared_builtin_event_name_exists() {
    for (name, interest) in expected_builtin_events() {
        let EventInterest::List(names) = interest else {
            continue;
        };
        for event in names {
            let known = pir::plugins::is_native_event(&event)
                || pir::plugins::NATIVE_ONLY_EVENTS.contains(&event.as_str());
            assert!(
                known,
                "builtin `{name}` declares `{event}`, which is not in \
                 WIRED_NATIVE_EVENTS or NATIVE_ONLY_EVENTS — the declaration \
                 can never match"
            );
        }
    }
}

/// Regression for the native event-name validation gap (P3-06): a native
/// `EventInterest::List` is a plain string fence, so a misspelled event is
/// silently dead code. `coverage::invalid_native_event_names` is the
/// build-time validator; `PluginHost::build` calls it over every plugin's
/// declaration and records a warning diagnostic. This test pins the
/// validator's answers (including the synthetic names with a native emit
/// site) and the build-time diagnostic.
#[test]
fn native_event_interest_validation_names_typos_and_accepts_dispatchable_events() {
    for (name, interest) in expected_builtin_events() {
        let invalid = pir::plugins::coverage::invalid_native_event_names(&interest);
        assert!(
            invalid.is_empty(),
            "builtin `{name}` declares non-native event names {invalid:?}"
        );
    }

    // Wired native events, the native-only presentation projections, and the
    // synthetic names with a native dispatch site (`entry_appended` and
    // `run_failed` are emitted directly; the summarization retries and
    // `thinking_level_changed` ride the session-event bridge) are all
    // dispatchable; anything else is a typo the build must diagnose.
    let valid = EventInterest::List(vec![
        "terminal_input".to_owned(),
        "status".to_owned(),
        "extension_ui_request".to_owned(),
        "entry_appended".to_owned(),
        "thinking_level_changed".to_owned(),
        "summarization_retry_scheduled".to_owned(),
        "summarization_retry_attempt_start".to_owned(),
        "summarization_retry_finished".to_owned(),
        "run_failed".to_owned(),
        "provider:refreshModels".to_owned(),
    ]);
    assert!(
        pir::plugins::coverage::invalid_native_event_names(&valid).is_empty(),
        "every name `pi.on` accepts as a known event must be declarable by a \
         native plugin when it has an emit site"
    );

    let typo = EventInterest::List(vec!["tool_cal".to_owned(), "input".to_owned()]);
    assert_eq!(
        pir::plugins::coverage::invalid_native_event_names(&typo),
        vec!["tool_cal"],
        "the validator must name only the misspelled entry"
    );

    assert!(pir::plugins::coverage::invalid_native_event_names(&EventInterest::None).is_empty());
    assert!(pir::plugins::coverage::invalid_native_event_names(&EventInterest::All).is_empty());
}

/// Arm-by-arm audit: the one plugin that ships an `on_event` arm outside its
/// declaration, plus the invariants that keep a second one from appearing
/// unnoticed. This asserts the audited declarations only — arms are not
/// runtime-introspectable — so it exists to make a declaration change here
/// require re-reading the arms.
///
/// - `todo` (src/plugins/builtin/todo.rs:2083-2128): declares `session_start`,
///   also matches `tool_result`. The arm is an idempotent whole-state REPLACE
///   reachable only through the direct hosted walk (asserted above); live
///   state is committed by the tool call itself and rebuilt from the
///   transcript in `on_session_context`, so nothing depends on the delivery.
///   Declaring `tool_result` would switch the agent loop's hosted result
///   fan-out on for every batch just to re-apply state it already has.
/// - `mcp` (mcp.rs:1364-1396), `tool_repair` (tool_repair.rs:198-231) and
///   `phone` (phone/mod.rs:843-865) early-return on any undeclared name, so no
///   undeclared arm exists; `context_mode` (context_mode/mod.rs:903-919)
///   matches exactly its three declared names.
///
/// Post-fix consequence worth pinning: because `EventInterest::None` now admits
/// NO event (`host.rs:2330-2347`), an `on_event` override added to one of the
/// four declaration-less builtins would be silent DEAD CODE on every filtered
/// path. The declaration half below keeps those four at `None`; the
/// `terminal_input` half pins that no in-tree plugin can hit the now-dead
/// terminal-input branch.
#[test]
fn audited_plugins_with_undeclared_arms_stay_limited_to_todo() {
    let declared = expected_builtin_events();

    #[cfg(feature = "todo")]
    {
        let (_, interest) = declared
            .iter()
            .find(|(name, _)| *name == "todo")
            .expect("todo is audited");
        assert_eq!(
            interest,
            &EventInterest::List(vec!["session_start".to_owned()]),
            "adding tool_result here would turn on the hosted result fan-out"
        );
    }

    // Everything else must declare a fence (or have no handlers at all), so a
    // new undeclared arm cannot slip in unnoticed.
    for (name, interest) in &declared {
        if matches!(*name, "subagent" | "codemode" | "python" | "tool-search") {
            assert_eq!(
                interest,
                &EventInterest::None,
                "`{name}` has no on_event override; an arm needs a declaration"
            );
        }
    }
}

/// After the `None => false` fix, `dispatch_terminal_input`'s native plugin
/// branch is reachable only through a plugin that DECLARES `terminal_input`.
/// No in-tree builtin does — asserted here against the real factories, so the
/// dead branch stays provably dead and a future terminal-input plugin must
/// declare the event to work at all.
#[test]
fn no_compiled_builtin_declares_terminal_input() {
    for name in builtin_names() {
        let plugin = instantiate_builtin(name)
            .unwrap_or_else(|| panic!("builtin `{name}` did not instantiate"));
        assert!(
            !plugin.events().matches("terminal_input"),
            "`{name}` declares terminal_input: it would now be the ONLY way the \
             native terminal-input branch in PluginHost::dispatch_terminal_input \
             runs — re-check that path (host.rs:926-934) before adding it"
        );
    }
}

/// The build-time half of P3-06: the shared validator's warning must actually
/// land in the host diagnostics, with the plugin path and the misspelled name.
#[test]
fn build_warns_about_a_typo_in_a_native_event_declaration() {
    struct TypoEventPlugin {
        info: PluginInfo,
    }

    impl AgentPlugin for TypoEventPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["tool_cal".to_owned(), "input".to_owned()])
        }
    }

    let host = PluginHost::build(
        None,
        vec![as_plugin(Arc::new(TypoEventPlugin {
            info: PluginInfo::inline("w16-typo"),
        }))],
    );
    let diagnostics = host.diagnostics();
    let warning = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.kind == pir::plugins::DiagnosticKind::Warning
                && diagnostic.path == "<inline:w16-typo>"
        })
        .expect("a misspelled native event must produce a load warning");
    assert!(
        warning.message.contains("unknown event `tool_cal`"),
        "the warning names the typo: {}",
        warning.message
    );
    assert_eq!(
        warning.message.matches("tool_cal").count(),
        1,
        "only the misspelled entry is reported: {}",
        warning.message
    );
}

/// Native tool metadata reaches the shared `ToolDef` (the JS `getAllTools()`
/// view): label defaults are captured explicitly, promptSnippet /
/// promptGuidelines survive, and the native object advertises its
/// prepareLoadout capability instead of the old hard-coded `false`.
#[test]
fn native_tool_metadata_reaches_the_shared_definition() {
    struct MetadataTool;

    impl AgentTool for MetadataTool {
        fn name(&self) -> &str {
            "w16-metadata-tool"
        }

        fn label(&self) -> &str {
            "W16 Metadata Tool"
        }

        fn description(&self) -> &str {
            "metadata probe"
        }

        fn prompt_snippet(&self) -> Option<String> {
            Some("use the metadata probe".to_owned())
        }

        fn prompt_guidelines(&self) -> Vec<String> {
            vec!["keep metadata aligned".to_owned()]
        }

        fn run(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> pir::agent::tool_execution::BoxFuture<'static, ToolResult> {
            Box::pin(async { ToolResult::ok("ok") })
        }
    }

    struct MetadataPlugin {
        info: PluginInfo,
    }

    impl AgentPlugin for MetadataPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
            ctx.register_tool(Arc::new(MetadataTool));
            Ok(())
        }
    }

    let host = PluginHost::build(
        None,
        vec![as_plugin(Arc::new(MetadataPlugin {
            info: PluginInfo::inline("w16-metadata-plugin"),
        }))],
    );
    let def = host
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "w16-metadata-tool")
        .expect("the native tool is in the shared registry");
    assert_eq!(def.label.as_deref(), Some("W16 Metadata Tool"));
    assert_eq!(
        def.prompt_snippet.as_deref(),
        Some("use the metadata probe")
    );
    assert_eq!(def.prompt_guidelines, vec!["keep metadata aligned"]);
    assert!(
        def.prepare_loadout,
        "a native tool always exposes its prepareLoadout hook"
    );
}

// ---------------------------------------------------------------------------
// Result-contract short-circuit / merge across the native face
// ---------------------------------------------------------------------------

/// `emit_native` is the JS-adapter-free twin of `emit`: a cancel outcome must
/// stop the remaining native plugins. The old implementation merged `acc.stop`
/// but never consulted it, so later plugins still ran after a cancel.
#[tokio::test(flavor = "current_thread")]
async fn emit_native_short_circuits_after_a_native_cancel() {
    struct CancelPlugin {
        info: PluginInfo,
    }

    impl AgentPlugin for CancelPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["session_before_switch".to_owned()])
        }

        fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            Box::pin(async { PluginEventOutcome::SessionBefore { cancel: true } })
        }
    }

    let (after, seen) = Recorder::new(
        "w16-after-cancel",
        EventInterest::List(vec!["session_before_switch".to_owned()]),
    );
    let host = PluginHost::build(
        None,
        vec![
            as_plugin(Arc::new(CancelPlugin {
                info: PluginInfo::inline("w16-cancel"),
            })),
            as_plugin(after),
        ],
    );

    let mut payload = json!({ "type": "session_before_switch", "reason": "switch" });
    let outcome = host
        .emit_native("session_before_switch", &mut payload)
        .await;

    assert_eq!(
        outcome.result["cancel"],
        json!(true),
        "the cancel outcome is the merged result: {:?}",
        outcome.result
    );
    assert!(
        drain_seen(&seen).is_empty(),
        "a later native plugin must not run after a cancel: {:?}",
        drain_seen(&seen)
    );
}

/// A raw `Generic` session-before result OVERLAYS the merged object instead of
/// replacing it, so an earlier typed `SessionBeforeCompact { compaction }`
/// replacement survives a later native plugin answering `{ cancel: false }`.
/// It also honors `cancel: true` for every `session_before_*` variant, not
/// only `session_before_tree`.
#[tokio::test(flavor = "current_thread")]
async fn generic_session_before_results_overlay_and_cancel_across_variants() {
    struct TypedCompactionPlugin {
        info: PluginInfo,
    }

    impl AgentPlugin for TypedCompactionPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["session_before_compact".to_owned()])
        }

        fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            Box::pin(async {
                PluginEventOutcome::SessionBeforeCompact {
                    cancel: false,
                    compaction: Some(json!({ "summary": "from-typed" })),
                }
            })
        }
    }

    struct GenericPlugin {
        info: PluginInfo,
        event: &'static str,
        value: serde_json::Value,
    }

    impl AgentPlugin for GenericPlugin {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec![self.event.to_owned()])
        }

        fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let value = self.value.clone();
            Box::pin(async move { PluginEventOutcome::Generic(value) })
        }
    }

    let host = PluginHost::build(
        None,
        vec![
            as_plugin(Arc::new(TypedCompactionPlugin {
                info: PluginInfo::inline("w16-typed-compact"),
            })),
            as_plugin(Arc::new(GenericPlugin {
                info: PluginInfo::inline("w16-generic-compact"),
                event: "session_before_compact",
                value: json!({ "cancel": false }),
            })),
        ],
    );
    let mut payload = json!({ "type": "session_before_compact", "reason": "manual" });
    let outcome = host
        .emit_native("session_before_compact", &mut payload)
        .await;
    assert_eq!(
        outcome.result["compaction"],
        json!({ "summary": "from-typed" }),
        "a later generic result must not drop the earlier typed replacement: {:?}",
        outcome.result
    );
    assert_eq!(outcome.result["cancel"], json!(false));

    // A Generic `cancel: true` on a non-tree variant short-circuits as well.
    let (after, seen) = Recorder::new(
        "w16-generic-after-cancel",
        EventInterest::List(vec!["session_before_switch".to_owned()]),
    );
    let host = PluginHost::build(
        None,
        vec![
            as_plugin(Arc::new(GenericPlugin {
                info: PluginInfo::inline("w16-generic-switch"),
                event: "session_before_switch",
                value: json!({ "cancel": true }),
            })),
            as_plugin(after),
        ],
    );
    let mut payload = json!({ "type": "session_before_switch", "reason": "switch" });
    let outcome = host
        .emit_native("session_before_switch", &mut payload)
        .await;
    assert_eq!(outcome.result["cancel"], json!(true));
    assert!(
        drain_seen(&seen).is_empty(),
        "Generic {{cancel:true}} must stop later handlers for every \
         session_before_* variant"
    );
}

/// `before_provider_request` is a result-contract event: the returned object
/// replaces the request body. A later native handler in the ordered walk must
/// observe an earlier handler's replacement in `payload.payload`, exactly like
/// it observes `message_end`/`tool_result`/`input` replacements.
#[tokio::test(flavor = "current_thread")]
async fn before_provider_request_chains_a_native_replacement_into_the_payload() {
    struct ReplaceProviderBody {
        info: PluginInfo,
    }

    impl AgentPlugin for ReplaceProviderBody {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["before_provider_request".to_owned()])
        }

        fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            Box::pin(async {
                PluginEventOutcome::Generic(json!({
                    "model": "replaced-model",
                    "messages": [],
                }))
            })
        }
    }

    struct ObserveProviderBody {
        info: PluginInfo,
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl AgentPlugin for ObserveProviderBody {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["before_provider_request".to_owned()])
        }

        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let body = event
                .payload
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let seen = Arc::clone(&self.seen);
            Box::pin(async move {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(body);
                PluginEventOutcome::None
            })
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![
            as_plugin(Arc::new(ReplaceProviderBody {
                info: PluginInfo::inline("w16-provider-replace"),
            })),
            as_plugin(Arc::new(ObserveProviderBody {
                info: PluginInfo::inline("w16-provider-observe"),
                seen: Arc::clone(&seen),
            })),
        ],
    );
    let mut payload = json!({
        "type": "before_provider_request",
        "payload": { "model": "original", "messages": [] },
    });
    let outcome = host
        .emit_native("before_provider_request", &mut payload)
        .await;

    assert_eq!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![json!({ "model": "replaced-model", "messages": [] })],
        "the second handler must observe the first replacement"
    );
    assert_eq!(
        outcome.result,
        json!({ "model": "replaced-model", "messages": [] }),
        "the merged replacement remains the host's request body"
    );
}

// ---------------------------------------------------------------------------
// terminal_input panic isolation
// ---------------------------------------------------------------------------

/// The native `UiService::on_terminal_input` registry is the other half of
/// the raw-input fan-out, and its callbacks are extension code too: a panic
/// must not unwind the TUI input thread, and a later registered handler must
/// still receive the chunk.
#[tokio::test(flavor = "current_thread")]
async fn native_terminal_input_ui_handler_panic_is_isolated() {
    use pir::plugins::services::{TerminalInputAction, TerminalInputHandler};

    struct PanicRawHandler;

    impl TerminalInputHandler for PanicRawHandler {
        fn handle(&self, _input: &str) -> TerminalInputAction {
            panic!("raw terminal UI boom")
        }
    }

    struct ConsumeRawHandler {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl TerminalInputHandler for ConsumeRawHandler {
        fn handle(&self, input: &str) -> TerminalInputAction {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(input.to_owned());
            TerminalInputAction::Consume
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(None, Vec::new());
    let _panic = host.on_terminal_input(Arc::new(PanicRawHandler));
    let _consume = host.on_terminal_input(Arc::new(ConsumeRawHandler {
        seen: Arc::clone(&seen),
    }));
    assert!(host.has_terminal_input_handlers());

    assert_eq!(
        host.dispatch_terminal_input("!").await,
        TerminalInputAction::Consume,
        "the panicking handler must fail open and the later handler consume"
    );
    assert_eq!(
        drain_seen(&seen),
        vec!["!".to_owned()],
        "the later raw-input handler still received the original chunk"
    );
}

/// A panicking native `terminal_input` handler must not unwind the TUI input
/// thread: the host reports it as an isolated error and later handlers still
/// get the chunk. `dispatch_terminal_input` used to call `on_event` directly,
/// bypassing the panic boundary every other native fan-out uses.
#[tokio::test(flavor = "current_thread")]
async fn terminal_input_panic_is_isolated_and_later_handlers_run() {
    struct PanicOnTerminal {
        info: PluginInfo,
    }

    impl AgentPlugin for PanicOnTerminal {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["terminal_input".to_owned()])
        }

        fn on_event<'a>(&'a self, _event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            Box::pin(async { panic!("terminal input boom") })
        }
    }

    struct ConsumeOnTerminal {
        info: PluginInfo,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl AgentPlugin for ConsumeOnTerminal {
        fn info(&self) -> &PluginInfo {
            &self.info
        }

        fn events(&self) -> EventInterest {
            EventInterest::List(vec!["terminal_input".to_owned()])
        }

        fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a, PluginEventOutcome> {
            let shown = event.payload["data"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let seen = Arc::clone(&self.seen);
            Box::pin(async move {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(shown);
                PluginEventOutcome::TerminalInput {
                    consume: true,
                    data: None,
                }
            })
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::build(
        None,
        vec![
            as_plugin(Arc::new(PanicOnTerminal {
                info: PluginInfo::inline("w16-terminal-panic"),
            })),
            as_plugin(Arc::new(ConsumeOnTerminal {
                info: PluginInfo::inline("w16-terminal-consume"),
                seen: Arc::clone(&seen),
            })),
        ],
    );

    assert_eq!(
        host.dispatch_terminal_input("!").await,
        pir::plugins::services::TerminalInputAction::Consume,
        "the panic must fail open and the later handler's consume must win"
    );
    assert_eq!(
        drain_seen(&seen),
        vec!["!".to_owned()],
        "the later handler received the original chunk"
    );
}

/// Behavioural contract for every declaration kind: what
/// `has_native_handlers` advertises is exactly what `emit_native` delivers.
/// With `EventInterest::None` now mapped to `false` on both sides, all three
/// kinds agree (before the fix, `None` failed this in one direction: gate
/// `false`, delivery `true`).
#[tokio::test(flavor = "current_thread")]
async fn gate_and_fan_out_agree_for_list_all_and_none_declarations() {
    for (label, interest, expected_events) in [
        (
            "list",
            EventInterest::List(vec!["input".to_owned()]),
            vec!["input"],
        ),
        (
            "all",
            EventInterest::All,
            vec!["input", "tool_result", "message_update"],
        ),
        ("none", EventInterest::None, vec![]),
    ] {
        let (plugin, seen) = Recorder::new(&format!("w16-agree-{label}"), interest);
        let host = PluginHost::build(None, vec![as_plugin(plugin)]);

        for event in ["input", "tool_result", "message_update"] {
            let before = drain_seen(&seen).len();
            let mut payload = json!({ "type": event });
            host.emit_native(event, &mut payload).await;
            let delivered = drain_seen(&seen).len() > before;
            assert_eq!(
                delivered,
                host.has_native_handlers(event),
                "{label}: has_native_handlers and emit_native disagree on `{event}`"
            );
            assert_eq!(
                delivered,
                expected_events.contains(&event),
                "{label}: unexpected delivery for `{event}`"
            );
        }
    }
    // `All` admits every event — `message_update` included (it is a real
    // upstream event type, `agent-session.ts:1312-1314`), so the `all` row
    // must list all three events the loop below emits. Listing only two used
    // to assert that `All` had a hole in it, contradicting both the
    // declaration contract and the `has_native_handlers` agreement asserted
    // on the line above.
}
