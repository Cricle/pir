//! Storage backends and the scan helpers they share.

pub mod jsonl;
pub mod memory;
pub mod scan;

// `rusqlite` is an optional dependency enabled by `extensions` and
// `context-mode`; the backend exists exactly when the driver does. A dedicated
// `durable-sqlite` feature would be narrower, but adding one needs a
// Cargo.toml edit (out of this lane's scope).
#[cfg(any(feature = "extensions", feature = "context-mode"))]
pub mod sqlite;
