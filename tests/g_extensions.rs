// Merged test binary: one link unit for a whole subsystem (test-suite link
// time dominates `cargo test`; fewer binaries = faster compile). Each file
// keeps its own module namespace, so helpers cannot collide.
#[path = "suites/codemode_builtin.rs"]
mod codemode_builtin;
#[path = "suites/codemode_strict.rs"]
mod codemode_strict;
#[path = "suites/codemode_worker.rs"]
mod codemode_worker;
#[path = "suites/extensions_audit_fixes.rs"]
mod extensions_audit_fixes;
#[path = "suites/extensions_events.rs"]
mod extensions_events;
#[path = "suites/extensions_js_stream.rs"]
mod extensions_js_stream;
// Configurable QuickJS resource limits (`extensionLimits` / `PI_EXT_*`).
#[path = "suites/extensions_limits.rs"]
mod extensions_limits;
#[path = "suites/extensions_regressions.rs"]
mod extensions_regressions;
// Extension-originated stdio routing (TUI frame vs. real stdio per host mode).
#[path = "suites/extensions_stdio_routing.rs"]
mod extensions_stdio_routing;
#[path = "suites/extensions_wiring.rs"]
mod extensions_wiring;
#[path = "suites/js_extension_api.rs"]
mod js_extension_api;
#[path = "suites/npm_resolve_units.rs"]
mod npm_resolve_units;
#[path = "suites/subagent_strict.rs"]
mod subagent_strict;
#[path = "suites/system_prompt_options_units.rs"]
mod system_prompt_options_units;
#[path = "suites/user_bash_sink.rs"]
mod user_bash_sink;
