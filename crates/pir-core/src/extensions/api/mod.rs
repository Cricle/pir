//! JS-side extension API glue and virtual module shims.
//!
//! The `pi` object surface, per-event `ctx` construction, handler dispatch
//! (registration order + per-handler error isolation), tool/command registries
//! and the load/commit/discard lifecycle are all implemented in JavaScript
//! (hosted in QuickJS); the Rust host talks to it through a small
//! `__pir_host` function table (registered in [`super::runtime`]) and
//! JSON-string arguments. Keeping the dispatch loop in JS avoids storing
//! JS values on the Rust side.

pub mod bytecode;
pub mod embedded;
pub mod node_shims;
mod prelude;
pub mod shims;

pub use prelude::{HOST_PRELUDE, HOST_PRELUDE_MODULE};

/// Script-mode boot overlay embedded in the core crate so packaged `pir-sdk`
/// builds do not reach outside the root package source tree.
///
/// The facade crate (`src/main_cli/js_mode.rs`) reads it through
/// [`script_boot_source`] rather than `include_str!`: the asset lives under
/// `crates/pir-core`, so a relative `include_str!` from the facade no longer
/// resolves, and `main_cli`'s AOT path (`SCRIPT_BOOT`) needs the bytes for the
/// bytecode generator as well.
pub const SCRIPT_BOOT: &str = include_str!("js/script_boot.js.txt");

/// The script-mode boot overlay source (see [`SCRIPT_BOOT`]).
pub fn script_boot_source() -> &'static str {
    SCRIPT_BOOT
}

/// Every embedded JS shim asset as `(source file name, blob)`.
///
/// Aggregates the three `api` registries with the worker boot overlay the
/// runtime host finds embeds, so the byte-identity test in `g_extensions` can
/// walk exactly the set that is `include_bytes!`-embedded.
pub fn all_shims() -> Vec<(&'static str, &'static embedded::Shim)> {
    let mut shims = Vec::with_capacity(90);
    shims.extend_from_slice(prelude::SHIMS);
    shims.extend_from_slice(shims::SHIMS);
    shims.extend_from_slice(node_shims::SHIMS);
    shims.extend_from_slice(super::runtime::worker_shim_manifest());
    shims
}

/// Every virtual module the loader can resolve, as `(canonical name, shim)`.
/// The AOT generator compiles each one under its canonical name so QuickJS's
/// module cache (keyed by the resolved name) sees a single instance.
pub fn all_virtual_modules() -> Vec<(&'static str, &'static embedded::Shim)> {
    let mut modules = shims::known_modules();
    modules.extend(node_shims::known_modules());
    modules
}
