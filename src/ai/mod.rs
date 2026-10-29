//! AI provider abstractions, model configuration, and authentication.
//!
//! This module is the Rust equivalent of the upstream `packages/ai/src`.
//! Core contracts, transport adapters, and authentication are available;
//! providers are user-configured custom endpoints (`~/.pi/agent/models.json`)
//! served by the protocol adapters (`openai-chat`, `anthropic-messages`, and
//! `google-generative-ai`).

pub mod api;
pub mod auth;
/// Structured-classifier APIs (upstream `api/classifier-shared.ts` + the
/// System One and OpenAI Decisions adapters).
pub mod classify;
pub mod error;
pub mod event_stream;
/// Image-generation APIs and their registry (upstream `images.ts` +
/// `images-api-registry.ts` + `api/openrouter-images.ts`).
pub mod images;
pub mod provider;
pub mod providers;
pub mod transport;
pub mod types;
pub mod util;
/// Virtual models: catalog entries that route requests to a physical model.
/// The file is `virtual.rs` (the module id avoids the reserved `virtual`).
#[path = "virtual.rs"]
pub mod virtual_;
