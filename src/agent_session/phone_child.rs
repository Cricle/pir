//! `PI_PHONE_CHILD` inline image-token prompts (the reference
//! `register-phone-child-extension.ts`).
//!
//! The phone pool spawns parallel sessions as `pir --mode rpc` with
//! `PI_PHONE_CHILD=1`. The browser renders attached images inline in the text
//! box as `⟦imgN⟧` tokens (legacy `{imgN}` / `{img}`), then sends one prompt
//! whose `text` still contains those tokens plus the ordered `images` array.
//! This module rebuilds the intended order: text and images interleaved by
//! token position, with images beyond the last token appended at the end.
//!
//! The reference extension branches on session state:
//! - busy, or the text is a slash command -> the prompt is transformed into
//!   the flattened text plus the ordered images and the normal turn runs;
//! - idle -> a custom `phone-inline-user-message` (displayed in the
//!   transcript) starts the turn through `pi.sendMessage(...,
//!   {triggerTurn:true})`.
//!
//! The environment is read once at session construction
//! ([`child_mode_from_env`] from `AgentSession::build`), so the per-prompt
//! guard is a plain bool test; parent sessions never reach this code.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value as JsonValue};

use super::CustomRunMessage;

/// The environment variable the phone pool sets on every child.
pub const CHILD_ENV: &str = "PI_PHONE_CHILD";

/// The custom message type the idle path sends (reference
/// `INLINE_MESSAGE_TYPE`).
pub const INLINE_MESSAGE_TYPE: &str = "phone-inline-user-message";

/// The reference token pattern, byte-for-byte:
/// `/⟦img\d+⟧|\{img\d*\}/g` (the second alternative also matches `{img}`).
const INLINE_IMAGE_TOKEN_PATTERN: &str = "⟦img\\d+⟧|\\{img\\d*\\}";

fn token_regex() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(INLINE_IMAGE_TOKEN_PATTERN).expect("the inline image token regex is static")
    })
}

/// One image carried by an RPC prompt (`{data, mimeType}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlinePromptImage {
    pub data: String,
    pub mime_type: String,
}

impl InlinePromptImage {
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// The reference's `image.data && image.mimeType` guard.
    fn is_usable(&self) -> bool {
        !self.data.is_empty() && !self.mime_type.is_empty()
    }
}

/// The rebuilt prompt content (`buildInlineContent`): an ordered array of
/// `{type:"text",text}` / `{type:"image",data,mimeType}` parts.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineContent {
    pub content: JsonValue,
    /// Tokens actually paired with an image (`min(tokens, images)`).
    pub matched_count: usize,
}

/// What the prompt hook must do with one RPC prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum InlinePromptAction {
    /// No token, no image, or no token/image pair: leave the prompt alone.
    Unchanged,
    /// Busy or slash-command: run a normal turn with the flattened text and
    /// the images in token order.
    Transform {
        text: String,
        images: Vec<InlinePromptImage>,
    },
    /// Idle: display + persist the interleaved content and trigger a turn.
    Custom(CustomRunMessage),
}

/// Whether this process was spawned as a phone child (`PI_PHONE_CHILD=1`).
///
/// Read once from [`super::AgentSession::build`] (never per prompt).
pub fn child_mode_from_env() -> bool {
    std::env::var(CHILD_ENV)
        .map(|value| value == "1")
        .unwrap_or(false)
}

/// The reference `isInlineImageTokenPrompt`: does the text contain at least
/// one inline image token?
pub fn contains_inline_image_token(text: &str) -> bool {
    token_regex().is_match(text)
}

fn text_part(text: &str) -> JsonValue {
    json!({ "type": "text", "text": text })
}

fn image_part(image: &InlinePromptImage) -> JsonValue {
    json!({
        "type": "image",
        "data": image.data,
        "mimeType": image.mime_type,
    })
}

/// Build the interleaved content exactly like the reference
/// `buildInlineContent`:
///
/// - text before each token becomes a text part;
/// - a token consumes the next usable image; a token with no usable image
///   left (or a consumed-up token run) stays literal text;
/// - trailing text is appended, then any unused images.
pub fn build_inline_content(text: &str, images: &[InlinePromptImage]) -> InlineContent {
    let matches: Vec<(usize, &str)> = token_regex()
        .find_iter(text)
        .map(|token| (token.start(), token.as_str()))
        .collect();
    if matches.is_empty() || images.is_empty() {
        let content = if text.is_empty() {
            JsonValue::Array(Vec::new())
        } else {
            JsonValue::Array(vec![text_part(text)])
        };
        return InlineContent {
            content,
            matched_count: 0,
        };
    }

    let mut content = Vec::new();
    let mut last_index = 0usize;
    let mut image_index = 0usize;

    for (index, token) in matches.iter().copied() {
        if image_index >= images.len() {
            break;
        }
        let before = &text[last_index..index];
        if !before.is_empty() {
            content.push(text_part(before));
        }
        let image = &images[image_index];
        if image.is_usable() {
            content.push(image_part(image));
            image_index += 1;
        } else {
            // Keep the token as text and retry the same image at the next
            // token (the reference's `else` branch).
            content.push(text_part(token));
        }
        last_index = index + token.len();
    }

    let after = &text[last_index..];
    if !after.is_empty() {
        content.push(text_part(after));
    }
    while image_index < images.len() {
        let image = &images[image_index];
        if image.is_usable() {
            content.push(image_part(image));
        }
        image_index += 1;
    }

    InlineContent {
        content: JsonValue::Array(content),
        matched_count: matches.len().min(images.len()),
    }
}

/// The concatenated text of a content array (reference `flattenText`).
pub fn flatten_text(content: &JsonValue) -> String {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(JsonValue::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(JsonValue::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// The image parts of a content array, in order.
pub fn inline_images(content: &JsonValue) -> Vec<InlinePromptImage> {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(JsonValue::as_str) == Some("image"))
                .filter_map(|part| {
                    let data = part.get("data").and_then(JsonValue::as_str)?;
                    let mime_type = part.get("mimeType").and_then(JsonValue::as_str)?;
                    Some(InlinePromptImage::new(data, mime_type))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decide what one RPC prompt does (the reference `input` hook branching).
///
/// `busy` is `!ctx.isIdle()`: an in-flight run or a still-starting run.
pub fn plan_inline_prompt(
    text: &str,
    images: &[InlinePromptImage],
    busy: bool,
) -> InlinePromptAction {
    if images.is_empty() || !contains_inline_image_token(text) {
        return InlinePromptAction::Unchanged;
    }
    let built = build_inline_content(text, images);
    if built.matched_count == 0 {
        return InlinePromptAction::Unchanged;
    }
    if busy || text.trim_start().starts_with('/') {
        return InlinePromptAction::Transform {
            text: flatten_text(&built.content),
            images: inline_images(&built.content),
        };
    }
    InlinePromptAction::Custom(CustomRunMessage {
        custom_type: INLINE_MESSAGE_TYPE.to_owned(),
        content: built.content,
        display: true,
        details: None,
    })
}
