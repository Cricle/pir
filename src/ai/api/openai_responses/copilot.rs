//! Re-export of the shared Copilot dynamic-header helpers (moved to
//! [`crate::ai::api::copilot_headers`] so the Anthropic adapter can use them
//! without the `provider-openai` feature).

pub use crate::ai::api::copilot_headers::{
    build_copilot_dynamic_headers, has_copilot_vision_input, infer_copilot_initiator,
    is_github_copilot_model,
};
