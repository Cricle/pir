//! JS-side extension API glue and virtual module shims.
//!
//! The `pi` object surface, per-event `ctx` construction, handler dispatch
//! (registration order + per-handler error isolation), tool/command registries
//! and the load/commit/discard lifecycle are all implemented in JavaScript
//! (hosted in QuickJS); the Rust host talks to it through a small
//! `__pir_host` function table (registered in [`super::runtime`]) and
//! JSON-string arguments. Keeping the dispatch loop in JS avoids storing
//! JS values on the Rust side.

pub mod embedded;
pub mod node_shims;
mod prelude;
pub mod shims;

pub use prelude::{HOST_PRELUDE, HOST_PRELUDE_MODULE};

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
