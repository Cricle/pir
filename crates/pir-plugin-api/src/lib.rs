//! Stable, engine-free protocol shared by builtin implementation crates and
//! the pir host. This crate intentionally depends on no workspace crate.

#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Prefix that marks a plugin path as a compiled-in builtin (`builtin:<name>`).
pub const BUILTIN_PATH_PREFIX: &str = "builtin:";

/// The builtin name inside a `builtin:<name>` path, or `None` when `path` is
/// not a builtin path.
///
/// ```
/// use pir_plugin_api::builtin_name;
///
/// assert_eq!(builtin_name("builtin:todo"), Some("todo"));
/// assert_eq!(builtin_name("builtin:"), None);        // empty name
/// assert_eq!(builtin_name("npm:@scope/pkg"), None);  // not a builtin
/// ```
pub fn builtin_name(path: &str) -> Option<&str> {
    path.strip_prefix(BUILTIN_PATH_PREFIX)
        .filter(|name| !name.is_empty())
}

/// Whether `path` names a compiled-in builtin.
///
/// ```
/// use pir_plugin_api::is_builtin_path;
///
/// assert!(is_builtin_path("builtin:memory"));
/// assert!(!is_builtin_path("builtin:"));
/// ```
pub fn is_builtin_path(path: &str) -> bool {
    builtin_name(path).is_some()
}

/// Identity of one plugin in the load plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    /// Registration name (a JS package name, or the bare builtin name).
    pub name: String,
    /// Load path; builtins use `builtin:<name>` (see [`BUILTIN_PATH_PREFIX`]).
    pub path: String,
    /// Whether a later plugin may replace this one in the ordered dispatch.
    pub replaceable: bool,
}

impl PluginInfo {
    /// The `builtin:<name>` identity for a compiled-in builtin.
    ///
    /// ```
    /// use pir_plugin_api::PluginInfo;
    ///
    /// let info = PluginInfo::builtin("todo");
    /// assert_eq!(info.name, "todo");
    /// assert_eq!(info.path, "builtin:todo");
    /// assert!(!info.replaceable);
    /// ```
    pub fn builtin(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            path: format!("{BUILTIN_PATH_PREFIX}{name}"),
            name,
            replaceable: false,
        }
    }
}

/// The session facts a plugin may observe when the session context changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContextSnapshot {
    /// The resolved model, as the host's JSON shape.
    pub model: Value,
    /// Current session id, absent for an ephemeral run.
    pub session_id: Option<String>,
    /// Working directory the session runs in, when known.
    pub cwd: Option<String>,
    /// Whether the project scope is trusted (see the host's trust store).
    pub project_trusted: bool,
}

/// One error a plugin reported while handling an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventError {
    /// Human-readable message; surfaced to the host's diagnostics.
    pub message: String,
}

/// What a plugin's event handler decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginEventOutcome {
    /// Leave the payload untouched; keep dispatching to later plugins.
    Continue,
    /// Stop dispatching to later plugins; the payload is unchanged.
    Handled,
    /// Stop dispatching and replace the payload with this value.
    Replace(Value),
    /// Stop dispatching and report these errors.
    Errors(Vec<EventError>),
}

/// One event handed to a plugin, with its payload borrowed for in-place mutation.
#[derive(Debug)]
pub enum PluginEvent<'a> {
    /// A named host event; `payload` is mutable so the plugin can transform it.
    Named {
        /// Event name (the host's event key).
        name: &'a str,
        /// Event payload, mutable in place.
        payload: &'a mut Value,
    },
}

/// A tool result in the runtime-neutral shape every builtin returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The text the model sees.
    pub content: String,
    /// Structured details for the host's renderer; `Null` when there are none.
    pub details: Value,
    /// Whether this result reports a failure.
    pub is_error: bool,
}

impl ToolResult {
    /// A successful text result with no structured details.
    ///
    /// ```
    /// use pir_plugin_api::ToolResult;
    ///
    /// let ok = ToolResult::text("files: 3");
    /// assert_eq!(ok.content, "files: 3");
    /// assert!(!ok.is_error);
    /// ```
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            details: Value::Null,
            is_error: false,
        }
    }

    /// A failed text result with no structured details.
    ///
    /// ```
    /// use pir_plugin_api::ToolResult;
    ///
    /// let err = ToolResult::error("permission denied");
    /// assert!(err.is_error);
    /// assert_eq!(err.content, "permission denied");
    /// ```
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            details: Value::Null,
            is_error: true,
        }
    }
}

/// The boxed future a [`BuiltinPlugin`] event handler returns.
pub type PluginFuture<'a> = Pin<Box<dyn Future<Output = PluginEventOutcome> + Send + 'a>>;

/// Minimal runtime-independent plugin contract. Host-specific registration and
/// services are deliberately represented by opaque API adapters in later
/// extraction batches rather than leaking core session/runtime types here.
pub trait BuiltinPlugin: Send + Sync {
    /// This plugin's identity in the load plan.
    fn info(&self) -> &PluginInfo;
    /// Observe a new session-context snapshot. Default: no-op.
    fn on_session_context(&self, _snapshot: &SessionContextSnapshot) {}
    /// Handle one event. Default: [`PluginEventOutcome::Continue`].
    fn on_event<'a>(&'a self, event: PluginEvent<'a>) -> PluginFuture<'a> {
        let _ = event;
        Box::pin(async { PluginEventOutcome::Continue })
    }
}

/// Constructor a catalog descriptor names; see [`BuiltinDescriptor::factory`].
pub type BuiltinPluginFactory = fn() -> Arc<dyn BuiltinPlugin>;

/// Generate the identity shell every builtin implementation crate shares.
///
/// Each builtin has the same three pieces of boilerplate — the `plugin_name`
/// constant, an `info: PluginInfo` field initialized from it, and a
/// `new_<name>_plugin()` factory — and they must all agree with the catalog's
/// descriptor name. Repeating them by hand in ten crates let the spelling drift
/// silently (a factory that built `builtin:toolsearch` would only fail at
/// runtime). This macro makes the name the single argument.
///
/// ```ignore
/// pub struct MyPlugin { info: PluginInfo }
/// pir_plugin_api::builtin_plugin_shell! {
///     plugin = MyPlugin,
///     name = "my-builtin",
///     factory = new_my_plugin,
/// }
/// ```
#[macro_export]
macro_rules! builtin_plugin_shell {
    (plugin = $plugin:ident, name = $name:literal, factory = $factory:ident $(,)?) => {
        /// The `builtin:<name>` identity of this plugin.
        pub const PLUGIN_NAME: &str = $name;

        impl Default for $plugin {
            fn default() -> Self {
                Self::new()
            }
        }

        /// The factory the catalog descriptor names.
        pub fn $factory() -> ::std::sync::Arc<dyn $crate::BuiltinPlugin> {
            ::std::sync::Arc::new(<$plugin>::new())
        }
    };
}

/// One row of a builtin catalog: identity, ordering metadata and constructor.
#[derive(Clone, Copy)]
pub struct BuiltinDescriptor {
    /// Registration name of the builtin (the bare name, without `builtin:`).
    pub name: &'static str,
    /// The cargo feature of the *host* engine that compiles this builtin in.
    /// Used only for the "not compiled into this build" diagnostic, so the
    /// string is the host's feature name (`mode` rides the host's `subagent`
    /// feature, `codemode` rides `extensions`).
    pub feature: &'static str,
    /// Whether a JS package of the same name may replace this builtin.
    pub replaceable: bool,
    /// Whether a default load plan enables this builtin without an explicit
    /// selection.
    pub default_enabled: bool,
    /// Constructor for the plugin instance.
    pub factory: BuiltinPluginFactory,
}
