//! AI provider abstractions, model configuration, and authentication.
//!
//! This module is the Rust equivalent of the upstream `packages/ai/src`.
//! Core contracts, transport adapters, and authentication are available;
//! providers are user-configured custom endpoints (`~/.pi/agent/models.json`)
//! served by the protocol adapters (`openai-chat`, `anthropic-messages`, and
//! `google-generative-ai`).
//!
//! The value types, the error type, the transport *seam*, the retry
//! classification helpers and the assistant event stream live in the
//! `pir-ai-types` crate and are re-exported here at their historical paths, so
//! every `crate::ai::{types,error,transport,util,event_stream}::…` use and the
//! public `pir::ai::…` surface is unchanged. The `reqwest` transport
//! implementation and the retry loop stay in this module's `transport`.

pub mod api;
pub mod auth;
/// Structured-classifier APIs (upstream `api/classifier-shared.ts` + the
/// System One and OpenAI Decisions adapters).
pub mod classify;
/// The AI error type and retry classification (`pir-ai-types`).
pub use pir_ai_types::error;
/// The assistant message event stream (`pir-ai-types`).
pub use pir_ai_types::event_stream;
/// Image-generation APIs and their registry (upstream `images.ts` +
/// `images-api-registry.ts` + `api/openrouter-images.ts`).
pub mod images;
pub mod provider;
pub mod providers;
pub mod transport;
/// Core AI value types (`pir-ai-types`).
pub use pir_ai_types as types;
/// Retry classification / context-overflow helpers (`pir-ai-types`).
pub use pir_ai_types::util;
/// Virtual models: catalog entries that route requests to a physical model.
/// The file is `virtual.rs` (the module id avoids the reserved `virtual`).
#[path = "virtual.rs"]
pub mod virtual_;
