use super::embedded::Shim;

/// Host prelude: per-runtime state, the `pi` object factory, `ctx` factory,
/// JSON-Schema validation, event dispatch, tool execution, command/shortcut
/// dispatch and the load lifecycle. Injected as the `pir-host-prelude`
/// bootstrap module, which also registers every glue entry point as a global.
///
/// Event merge semantics mirror upstream `runner.ts` (`emitToolCall`,
/// `emitContext`, `emitMessageEnd`, `emitUserBash`, `emitInput`,
/// `emitBeforeProviderRequest`, `emitBeforeProviderHeaders`,
/// `emitResourcesDiscover`): handlers run in registration order, per-handler
/// errors are isolated, and event-specific results chain across handlers.
pub const HOST_PRELUDE_MODULE: &str = "pir-host-prelude";

pub static HOST_PRELUDE: Shim = Shim::new(
    "prelude.js.txt",
    include_bytes!("../../../assets/shims/prelude.js.txt.deflate"),
);

/// Every file-backed shim in this module, for the byte-identity test.
pub(crate) const SHIMS: &[(&str, &Shim)] = &[("prelude.js.txt", &HOST_PRELUDE)];
