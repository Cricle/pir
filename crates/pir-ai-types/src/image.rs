//! Image-generation and classifier model types (upstream `packages/ai/src/
//! types.ts`: `ImageModel`, `ClassifierModel`, `ModelType`, plus the
//! `ImagesContext` / `AssistantImages` and `ClassifierContext` /
//! `ClassifierResult` payloads).
//!
//! pir models the three upstream model kinds (chat / image / classifier) as
//! separate Rust types instead of TypeScript structural subtypes: chat keeps
//! the existing [`super::model::Model`] untouched (the whole chat pipeline —
//! catalog, providers, streaming — compiles against it), while image and
//! classifier models are new structs carrying the upstream `BaseModel` fields
//! plus their kind-specific ones (`ImageModel.output`, `ClassifierModel.
//! contextWindow`). [`ModelKind`] and [`AnyModel`] tag the kind so a chat
//! model can never be passed to `generateImages`/`classify` by accident.

use std::collections::BTreeMap;

use super::messages::ImageContent;
use super::model::{Model, ModelCost};
use super::stream::Usage;
use super::ProviderId;

// ---------------------------------------------------------------------------
// Model kinds
// ---------------------------------------------------------------------------

/// What a catalog entry is for (upstream `ModelType`): which operation
/// accepts it — chat streaming, `generateImages`, or `classify`.
///
/// The strum derives keep the wire strings in sync with the serde renames
/// (`lowercase` of each variant is exactly its serde name).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ModelKind {
    /// Chat model: usable with `stream()` and friends. The default — like
    /// upstream, models without a `type` field are chat models.
    #[default]
    Chat,
    /// Image-generation model: usable with `generateImages()` only.
    Image,
    /// Structured classifier model: usable with `classify()` only.
    Classifier,
}

impl ModelKind {
    /// The upstream wire name (`"chat"` / `"image"` / `"classifier"`).
    pub fn wire_name(&self) -> &'static str {
        self.into()
    }
}

/// An image-generation API (upstream `KnownImageApi`). Catalog entries carry
/// the typed variant, so dispatch happens by match, never by string compare.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum ImageApi {
    /// Image generation over OpenRouter's chat-completions endpoint
    /// (upstream `"openrouter-images"`).
    #[serde(rename = "openrouter-images")]
    OpenRouterImages,
}

impl ImageApi {
    /// The upstream wire name (`"openrouter-images"`): an explicit mapping,
    /// not strum's kebab-case (which would emit `open-router-images`).
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::OpenRouterImages => "openrouter-images",
        }
    }
}

/// A structured-classifier API (upstream `KnownClassifierApi`). pir ships the
/// adapters whose models exist in the committed catalog snapshot. Upstream's
/// `llama-cpp-classify` is a deliberate absence: it wraps an external
/// llama.cpp service, has no committed catalog entries, and **no
/// implementation source exists in this checkout** — the typed enum therefore
/// has no such variant (a catalog entry carrying that api string fails typed
/// at load time rather than silently mis-dispatching).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum ClassifierApi {
    /// TypeSafe's native System One protocol; OpenRouter, Vercel AI Gateway
    /// and OpenCode Zen serve the same protocol (upstream
    /// `"typesafe-system-one"`).
    #[serde(rename = "typesafe-system-one")]
    TypesafeSystemOne,
    /// System One on the Cloudflare Workers AI REST endpoint (upstream
    /// `"cloudflare-workers-ai-system-one"`).
    #[serde(rename = "cloudflare-workers-ai-system-one")]
    CloudflareWorkersAiSystemOne,
    /// OpenAI's Decisions API, `POST /v1/decisions` (upstream
    /// `"openai-decisions"`).
    #[serde(rename = "openai-decisions")]
    OpenAiDecisions,
}

impl ClassifierApi {
    /// The upstream wire name: explicit mappings, not strum's kebab-case
    /// (which would emit `open-ai-decisions`).
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::TypesafeSystemOne => "typesafe-system-one",
            Self::CloudflareWorkersAiSystemOne => "cloudflare-workers-ai-system-one",
            Self::OpenAiDecisions => "openai-decisions",
        }
    }
}

/// Supported input modalities (upstream `ModelInputModality`, subset).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum InputModality {
    /// Plain text input.
    Text,
    /// Base64-encoded image input.
    Image,
}

/// Supported output modalities of an image model (upstream
/// `ImageModel.output`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum OutputModality {
    /// Text blocks may be returned alongside images.
    Text,
    /// Generated images.
    Image,
}

// ---------------------------------------------------------------------------
// Image and classifier models
// ---------------------------------------------------------------------------

/// An image-generation model (upstream `ImageModel<ImageApi>`): usable with
/// `generateImages()` only.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImageModel {
    pub id: String,
    pub name: String,
    pub api: ImageApi,
    pub provider: ProviderId,
    pub base_url: String,
    /// Input modalities; only `["image"]`-capable models accept image
    /// references in `ImagesContext.input`.
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    /// Default HTTP headers (never exposed to scripts; upstream drops
    /// `headers` from script-visible model info).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Output modalities. Always includes `image`; `text` means the model can
    /// also return text blocks.
    pub output: Vec<OutputModality>,
}

impl ImageModel {
    /// Whether the model accepts image blocks in its input (upstream checks
    /// `"image" in model.input`).
    pub fn accepts_image_input(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }
}

/// A structured classifier model (upstream `ClassifierModel<ClassifierApi>`):
/// usable with `classify()` only.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassifierModel {
    pub id: String,
    pub name: String,
    pub api: ClassifierApi,
    pub provider: ProviderId,
    pub base_url: String,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    pub context_window: u64,
}

impl ClassifierModel {
    /// Whether the model accepts the `images` of a `ClassifierContext`
    /// (upstream: only models whose `input` includes `"image"`; other models
    /// return an error result before the request).
    pub fn accepts_image_input(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }
}

/// Any catalog entry regardless of kind (upstream `AnyModel`). Chat models
/// keep the full [`Model`] shape.
// `Chat(Model)` is intentionally the large variant: boxing `Model` would
// churn every construction/match site across the catalog and provider
// layers for a size win that does not matter at catalog scale (entries are
// few and short-lived), so the lint is tolerated.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AnyModel {
    /// Chat model (the default kind).
    Chat(Model),
    /// Image-generation model.
    Image(ImageModel),
    /// Structured classifier model.
    Classifier(ClassifierModel),
}

impl AnyModel {
    /// The entry's kind.
    pub fn kind(&self) -> ModelKind {
        match self {
            AnyModel::Chat(_) => ModelKind::Chat,
            AnyModel::Image(_) => ModelKind::Image,
            AnyModel::Classifier(_) => ModelKind::Classifier,
        }
    }

    /// The shared identity fields (`provider/id`), for listings and lookup.
    pub fn identity(&self) -> (&str, &str) {
        match self {
            AnyModel::Chat(model) => (&model.provider, &model.id),
            AnyModel::Image(model) => (&model.provider, &model.id),
            AnyModel::Classifier(model) => (&model.provider, &model.id),
        }
    }
}

// ---------------------------------------------------------------------------
// Image generation
// ---------------------------------------------------------------------------

/// One input or output block of an image request/result (upstream
/// `ImagesInputContent` / `ImagesOutputContent`). Tagged so the script-facing
/// JSON keeps the upstream `{ type: "text" | "image", ... }` block shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImagesContent {
    /// A text prompt or returned text.
    #[serde(rename = "text")]
    Text { text: String },
    /// A base64-encoded image reference or generated image.
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// The request context of one image generation (upstream `ImagesContext`).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ImagesContext {
    /// Prompt text plus optional image references (e.g. img2img inputs).
    #[serde(default)]
    pub input: Vec<ImagesContent>,
}

impl ImagesContext {
    /// A prompt-only context (the common `models.generateImages("...", ...)`
    /// shape).
    pub fn from_prompt(prompt: impl Into<String>) -> Self {
        Self {
            input: vec![ImagesContent::Text {
                text: prompt.into(),
            }],
        }
    }
}

/// Why an image request ended (upstream `ImagesStopReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagesStopReason {
    /// The request completed.
    Stop,
    /// The request failed (message in `error_message`).
    Error,
    /// The caller aborted the request.
    Aborted,
}

/// The result of one image generation (upstream `AssistantImages`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssistantImages {
    pub api: ImageApi,
    pub provider: ProviderId,
    pub model: String,
    /// Returned text/image blocks. Image `data` is base64, mirroring
    /// upstream (scripts serialize the blocks themselves).
    pub output: Vec<ImagesContent>,
    #[serde(
        rename = "responseId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    pub stop_reason: ImagesStopReason,
    #[serde(
        rename = "errorMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub error_message: Option<String>,
    /// Unix timestamp in milliseconds when the request started.
    pub timestamp: u64,
}

/// Request options for image generation (upstream `ImagesOptions`, the subset
/// pir's adapters consume).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImagesOptions {
    /// Explicit API key; when unset, callers fall back to the provider's env
    /// key (upstream resolves provider auth before dispatching).
    pub api_key: Option<String>,
    /// Extra request headers (caller values override model defaults).
    pub headers: BTreeMap<String, String>,
}

/// Extended image-generation request options (upstream `ImagesOptions`
/// extending `ProviderRequestOptions`): the [`ImagesOptions`] surface plus the
/// request-scoped knobs — cancellation, provider-scoped env overrides,
/// metadata, and the transport timeout/retry budget.
#[derive(Debug, Clone, Default)]
pub struct ImagesRequestOptions {
    /// Explicit API key; when unset, `env` then the provider's process env
    /// key are consulted.
    pub api_key: Option<String>,
    /// Extra request headers (caller values override model defaults).
    pub headers: BTreeMap<String, String>,
    /// Cancellation signal for this request.
    pub signal: Option<super::stream::AbortSignal>,
    /// Provider-scoped environment overrides (upstream
    /// `ProviderRequestOptions.env`), checked before the process environment
    /// for auth keys.
    pub env: Option<super::stream::ProviderEnv>,
    /// Optional metadata to include in API requests; providers extract the
    /// fields they understand and ignore the rest.
    pub metadata: Option<BTreeMap<String, JsonValue>>,
    /// HTTP request timeout override, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Maximum client-side retry attempts for this request.
    pub max_retries: Option<u32>,
    /// Cap on a server-requested retry delay, in milliseconds.
    pub max_retry_delay_ms: Option<u64>,
}

impl From<&ImagesOptions> for ImagesRequestOptions {
    fn from(options: &ImagesOptions) -> Self {
        Self {
            api_key: options.api_key.clone(),
            headers: options.headers.clone(),
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// The meanings of a `bool` question's two outcomes (upstream
/// `ClassifierBoolQuestion.criteria`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassifierBoolCriteria {
    /// What a `true` answer means.
    #[serde(rename = "true")]
    pub true_meaning: String,
    /// What a `false` answer means.
    #[serde(rename = "false")]
    pub false_meaning: String,
}

/// One question a classifier must answer (upstream `ClassifierQuestion`).
/// Tagged so the script-facing JSON keeps the upstream
/// `{ type: "choice" | "score" | "bool", ... }` shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClassifierQuestion {
    /// Pick one of the labeled options; the map gives each label's meaning.
    #[serde(rename = "choice")]
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// Pick a level from an ordered scale, lowest first.
    #[serde(rename = "score")]
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    /// A yes/no judgment with the meanings of both outcomes.
    #[serde(rename = "bool")]
    Bool {
        instructions: String,
        criteria: ClassifierBoolCriteria,
    },
}

/// The state a classifier judges (upstream `ClassifierContext`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassifierContext {
    /// Arbitrary JSON state sent to the model.
    pub state: JsonValue,
    /// Images judged together with `state`. Only models whose `input`
    /// includes `image` accept them; other models return an error result
    /// before any request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageContent>,
    /// Questions keyed by id.
    pub questions: BTreeMap<String, ClassifierQuestion>,
}

/// One answer (upstream `ClassifierAnswer`). Tagged with the upstream
/// `{ type: "choice" | "score" | "bool", ... }` shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClassifierAnswer {
    /// The chosen label, its normalized probabilities, and the confidence.
    #[serde(rename = "choice")]
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    /// The chosen level index (0 = lowest) and the confidence.
    #[serde(rename = "score")]
    Score { score: f64, confidence: f64 },
    /// The probability that the answer is true.
    #[serde(rename = "bool")]
    Bool { probability: f64 },
}

/// Why a classification ended (upstream `ClassifierStopReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierStopReason {
    /// The request completed.
    Stop,
    /// The request failed (message in `error_message`).
    Error,
    /// The caller aborted the request.
    Aborted,
}

/// The result of one classification (upstream `ClassifierResult`), one answer
/// per requested question.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassifierResult {
    pub api: ClassifierApi,
    pub provider: ProviderId,
    pub model: String,
    pub answers: BTreeMap<String, ClassifierAnswer>,
    /// Token usage and its cost at the model's catalog price, when the
    /// service reports token counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    pub stop_reason: ClassifierStopReason,
    #[serde(
        rename = "errorMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub error_message: Option<String>,
    /// Unix timestamp in milliseconds when the request started.
    pub timestamp: u64,
}

/// Request options for classification (upstream `ClassifierOptions`, subset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassifierOptions {
    /// Explicit API key; when unset, callers fall back to the provider's env
    /// key.
    pub api_key: Option<String>,
    /// Extra request headers (caller values override model defaults).
    pub headers: BTreeMap<String, String>,
    /// Divides the answer logits before normalization (upstream
    /// `ClassifierOptions.temperature`); APIs that cannot apply it ignore it.
    pub temperature: Option<f64>,
}

/// Extended classification request options (upstream `ClassifierOptions`
/// extending `ProviderRequestOptions`): the [`ClassifierOptions`] surface plus
/// cancellation, provider-scoped env overrides, and the transport
/// timeout/retry budget.
#[derive(Debug, Clone, Default)]
pub struct ClassifierRequestOptions {
    /// Explicit API key; when unset, `env` then the provider's process env
    /// key are consulted.
    pub api_key: Option<String>,
    /// Extra request headers (caller values override model defaults).
    pub headers: BTreeMap<String, String>,
    /// Divides the answer logits before normalization; APIs that cannot apply
    /// it ignore it.
    pub temperature: Option<f64>,
    /// Cancellation signal for this request.
    pub signal: Option<super::stream::AbortSignal>,
    /// Provider-scoped environment overrides, checked before the process
    /// environment for auth keys.
    pub env: Option<super::stream::ProviderEnv>,
    /// HTTP request timeout override, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Maximum client-side retry attempts for this request.
    pub max_retries: Option<u32>,
    /// Cap on a server-requested retry delay, in milliseconds.
    pub max_retry_delay_ms: Option<u64>,
}

impl From<&ClassifierOptions> for ClassifierRequestOptions {
    fn from(options: &ClassifierOptions) -> Self {
        Self {
            api_key: options.api_key.clone(),
            headers: options.headers.clone(),
            temperature: options.temperature,
            ..Self::default()
        }
    }
}

/// Type alias used by the images/classifiers payloads for JSON state.
pub type JsonValue = serde_json::Value;
