//! Protocol message schemas, typed structs, and wire-format validation.
//!
//! Schema validation is based on `serde_json::Value` so that the same
//! validation path is used for both CBOR-decoded and in-memory construction.
//! The typed structs are used only for encoding (via serde → ciborium CBOR).

mod client;
mod commands;
mod delta;
mod error;
mod rpc;
mod server;
mod session;
mod transcript;
mod types;
mod validate;

pub use client::*;
pub use commands::*;
pub use delta::*;
pub use error::*;
pub use rpc::*;
pub use server::*;
pub use session::*;
pub use types::*;
pub use validate::*;

pub const PROTOCOL_VERSION: i64 = 8;
