//! Bridge between the engine's plugin contract ([`AgentPlugin`]) and the
//! engine-free protocol in `pir_plugin_api`.
//!
//! The extraction is deliberately two-layered:
//!
//! - **pure logic** lives in the `pir-builtin-*` crates, behind
//!   [`pir_plugin_api::BuiltinPlugin`] — a protocol that names no engine type;
//! - **runtime state** (QuickJS, SQLite, rmcp transports, `AgentSession`, tool
//!   registration) stays in this crate, behind [`AgentPlugin`].
//!
//! [`BuiltinPluginAdapter`] is the seam. It lets an application layer install a
//! catalog whose implementations come from outside this crate
//! ([`super::install_builtin_catalog`] with a [`BuiltinResolver`]) without the
//! host knowing any concrete plugin type: the resolver hands back
//! `Arc<dyn BuiltinPlugin>`, the adapter presents it as `Arc<dyn AgentPlugin>`.
//!
//! ## What the adapter translates
//!
//! | engine | protocol |
//! |---|---|
//! | [`crate::plugins::source::PluginInfo`] | [`pir_plugin_api::PluginInfo`] |
//! | [`crate::extensions::SessionContextSnapshot`] (rich) | [`pir_plugin_api::SessionContextSnapshot`] (4 fields) |
//! | [`PluginEvent`] (`Cow<str>` + `&mut Value`) | [`pir_plugin_api::PluginEvent::Named`] |
//! | [`PluginEventOutcome`] (11 variants) | 4 variants (`Continue`/`Handled`/`Replace`/`Errors`) |
//!
//! The outcome mapping is where the two layers differ in richness, so it is
//! defined in one direction only: the protocol's `Replace(Value)` is
//! interpreted through [`PluginEventOutcome::from_wire`], which is already the
//! canonical "raw result JSON for this event name" decoder the JS bridge uses.
//! A protocol plugin therefore writes the same wire shape a JS extension
//! returns and gets identical merge semantics for free.
//!
//! [`BuiltinResolver`]: super::BuiltinResolver

use std::sync::Arc;

use super::plugin::{AgentPlugin, PluginEvent, PluginEventOutcome};
use super::source::PluginInfo;
use pir_plugin_api as api;

/// Presents one [`api::BuiltinPlugin`] as an engine [`AgentPlugin`].
///
/// The engine-side identity is materialized once (the protocol's
/// [`api::PluginInfo`] is owned by the inner plugin, while
/// [`AgentPlugin::info`] must return a reference to the engine's type).
pub struct BuiltinPluginAdapter {
    inner: Arc<dyn api::BuiltinPlugin>,
    info: PluginInfo,
}

impl BuiltinPluginAdapter {
    /// Wrap one protocol plugin, translating its identity eagerly.
    pub fn new(inner: Arc<dyn api::BuiltinPlugin>) -> Self {
        let info = {
            let source = inner.info();
            PluginInfo {
                name: source.name.clone(),
                path: source.path.clone(),
                replaceable: source.replaceable,
            }
        };
        Self { inner, info }
    }

    /// The wrapped protocol plugin (capability queries a host may need).
    pub fn inner(&self) -> &Arc<dyn api::BuiltinPlugin> {
        &self.inner
    }
}

/// Project the engine's rich session snapshot onto the protocol's four fields.
///
/// A protocol plugin that needs more than this is asking for engine state, so
/// the narrow snapshot is the contract, not a limitation to work around: the
/// extraction boundary is "pure logic must not read the session".
fn project_snapshot(
    snapshot: &crate::extensions::SessionContextSnapshot,
) -> api::SessionContextSnapshot {
    api::SessionContextSnapshot {
        model: snapshot.model.clone(),
        session_id: snapshot.session_id.clone(),
        cwd: snapshot.cwd.clone(),
        project_trusted: snapshot.project_trusted,
    }
}

/// Translate a protocol outcome into the engine's typed outcome.
///
/// `event` is needed because the protocol's `Replace` carries a *wire result*
/// whose meaning ("block the call", "cancel the switch", ...) depends on the
/// event family — the same reason `PluginEventOutcome::from_wire` exists.
pub fn outcome_from_protocol(event: &str, outcome: api::PluginEventOutcome) -> PluginEventOutcome {
    match outcome {
        api::PluginEventOutcome::Continue => PluginEventOutcome::None,
        // "The handler consumed the event." The engine can only express that
        // for the two families that have a consumer in their merge contract
        // (`input`, `terminal_input`); for every other family the honest
        // translation is "no result", because inventing a `Generic(true)`
        // would make the host read a boolean result where the event's wire
        // shape is an object. A protocol plugin that wants to CHANGE another
        // event returns `Replace` with that event's result shape.
        api::PluginEventOutcome::Handled => match event {
            "input" => PluginEventOutcome::Input {
                action: super::plugin::InputAction::Handled,
                text: None,
                images: None,
            },
            "terminal_input" => PluginEventOutcome::TerminalInput {
                consume: true,
                data: None,
            },
            _ => PluginEventOutcome::None,
        },
        // A real return value: decode it exactly like a JS handler's result so
        // both faces share one interpretation of the wire shape.
        api::PluginEventOutcome::Replace(value) => PluginEventOutcome::from_wire(event, &value),
        // The protocol reports failures as a list; the engine's variant
        // carries one message. Join them (the merge layer isolates the whole
        // handler, so the detail must not be dropped) and never claim a
        // deadline the plugin did not report. A plugin that reports no error
        // at all made no claim, so it is `None`, not an empty failure.
        api::PluginEventOutcome::Errors(errors) => {
            if errors.is_empty() {
                return PluginEventOutcome::None;
            }
            PluginEventOutcome::Error {
                error: errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; "),
                timed_out: false,
            }
        }
    }
}

impl AgentPlugin for BuiltinPluginAdapter {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn on_session_context(&self, snapshot: &crate::extensions::SessionContextSnapshot) {
        self.inner.on_session_context(&project_snapshot(snapshot));
    }

    fn on_event<'a>(
        &'a self,
        event: PluginEvent<'a>,
    ) -> super::plugin::PluginFuture<'a, PluginEventOutcome> {
        let PluginEvent { name, payload } = event;
        Box::pin(async move {
            // `name` is a `Cow`: reborrow it inside the block instead of
            // allocating an owned `String` per dispatch. Both the inner
            // plugin and the name are borrowed for this block's lifetime,
            // which is what the protocol's event accepts.
            let outcome = self
                .inner
                .on_event(api::PluginEvent::Named {
                    name: name.as_ref(),
                    payload,
                })
                .await;
            outcome_from_protocol(name.as_ref(), outcome)
        })
    }
}

/// Wrap one protocol plugin as an engine plugin.
pub fn adapt_builtin(inner: Arc<dyn api::BuiltinPlugin>) -> Arc<dyn AgentPlugin> {
    Arc::new(BuiltinPluginAdapter::new(inner))
}
