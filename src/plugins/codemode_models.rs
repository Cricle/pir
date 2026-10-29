//! The codemode `models` namespace bridge (upstream
//! `packages/coding-agent/src/extensions/codemode/execute.ts`
//! `createModelGlobals`): the sandbox's `models.generateImages(...)` /
//! `models.classify(...)` calls route through two hidden bridge tools
//! registered by the codemode plugin, executed via the SAME nested-call
//! bridge as `tools.<name>` — so they surface as nested call rows (upstream
//! shows them as rows too), work identically in the worker-process and
//! thread sandboxes, and never receive a script-supplied base URL or
//! headers beyond explicit request options.
//!
//! ## Shape adaptation (documented deviation)
//!
//! Upstream scripts pass a resolved catalog model object
//! (`models.getModelOfType("image", provider, id)` then
//! `models.generateImages(model, context)`), because upstream's tool context
//! reaches the session model registry. pir's `ToolContext` has no model
//! registry seam (see the `models` note in `codemode.rs`'s module doc), so
//! pir's scripts pass the provider/id as request options instead:
//!
//! - `models.generateImages(promptOrBlocks, { provider, model, apiKey? })`
//! - `models.classify({ state, questions, images? }, { provider, model, apiKey? })`
//!
//! The model is resolved from pir's embedded catalog
//! ([`crate::ai::providers::catalog`]), and auth comes from the explicit
//! `apiKey` option or the provider's standard environment key (upstream
//! resolves provider auth the same way, explicit winning). Results return to
//! the script as the full JSON object (upstream `AssistantImages` /
//! `ClassifierResult`): image blocks carry base64 `data` + `mimeType` in the
//! `output` array, exactly like upstream.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolExposure};
use crate::ai::providers::catalog;
use crate::ai::types::{ClassifierContext, ClassifierOptions, ImagesContent, ImagesContext};

/// The bridge tool that resolves `models.generateImages(...)`
/// (upstream `"models.generateImages"` nested-call rows).
pub const GENERATE_IMAGES_TOOL: &str = "models.generateImages";

/// The bridge tool that resolves `models.classify(...)`.
pub const CLASSIFY_TOOL: &str = "models.classify";

/// Request options of both bridge calls (the second script argument).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeOptions {
    provider: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    /// Extra request headers (caller values override model defaults).
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Classifier only: answer-logit temperature (upstream
    /// `ClassifierOptions.temperature`).
    #[serde(default)]
    temperature: Option<f64>,
}

/// The `{ payload, options }` argument envelope of both bridges.
#[derive(Debug, serde::Deserialize)]
struct BridgeRequest {
    #[serde(default)]
    payload: JsonValue,
    #[serde(default)]
    options: Option<BridgeOptions>,
}

fn fail(message: impl Into<String>) -> ToolResult {
    ToolResult::error(message.into())
}

fn ok_json(value: &impl serde::Serialize) -> ToolResult {
    match serde_json::to_string(value) {
        Ok(json) => ToolResult::ok(json),
        Err(error) => ToolResult::error(format!("model result serialization failed: {error}")),
    }
}

/// Catalog lookup flattened to `Option`; a parse error fails the call with
/// the catalog error instead of masking it as "unknown model". The error is
/// the user-facing message (`fail()` wraps it at the call site), which keeps
/// the `Err` variant small (clippy `result_large_err`).
fn lookup<T>(result: crate::ai::error::AiResult<Option<T>>) -> Result<Option<T>, String> {
    match result {
        Ok(found) => Ok(found),
        Err(error) => Err(error.user_message()),
    }
}

/// Which operation one bridge tool serves (the typed replacement for
/// dispatching on the tool-name string; upstream keys its registry on the
/// `"models.generateImages"` / `"models.classify"` names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeKind {
    GenerateImages,
    Classify,
}

impl BridgeKind {
    /// The registered tool name (upstream `"models.generateImages"` /
    /// `"models.classify"`).
    fn tool_name(self) -> &'static str {
        match self {
            BridgeKind::GenerateImages => GENERATE_IMAGES_TOOL,
            BridgeKind::Classify => CLASSIFY_TOOL,
        }
    }

    /// The tool description shown in the tool registry.
    fn description(self) -> &'static str {
        match self {
            BridgeKind::GenerateImages => "codemode models.generateImages bridge",
            BridgeKind::Classify => "codemode models.classify bridge",
        }
    }
}

/// One bridge tool shared by both operations; the operation is picked by the
/// concrete `name()`, keeping the tool pair value-less and stateless.
pub struct ModelBridgeTool {
    kind: BridgeKind,
}

impl ModelBridgeTool {
    /// The image-generation bridge (upstream `"models.generateImages"`).
    pub fn generate_images() -> Self {
        Self {
            kind: BridgeKind::GenerateImages,
        }
    }

    /// The classifier bridge (upstream `"models.classify"`).
    pub fn classify() -> Self {
        Self {
            kind: BridgeKind::Classify,
        }
    }

    /// Both bridge tools, in registration order.
    pub fn pair() -> [Self; 2] {
        [Self::generate_images(), Self::classify()]
    }

    fn unknown_model_error(kind: &str, noun: &str, provider: &str, model: &str) -> ToolResult {
        // Upstream distinguishes "unknown" from "exists as another kind" so
        // scripts pass the right type; pir checks the same provider's chat
        // catalog for the hint (`kind` carries the article form).
        let hint = match catalog::load_provider(provider) {
            Ok(Some(provider_entry)) if provider_entry.models.iter().any(|m| m.id == model) => {
                format!("\"{provider}/{model}\" is a chat model, not {kind} model. ")
            }
            _ => String::new(),
        };
        fail(format!(
            "{hint}Unknown {noun} model \"{provider}/{model}\"."
        ))
    }
}

impl AgentTool for ModelBridgeTool {
    fn name(&self) -> &'static str {
        self.kind.tool_name()
    }

    fn description(&self) -> &'static str {
        self.kind.description()
    }

    /// Callable from codemode scripts, never declared to the model (upstream
    /// `exposure: "codemode"` for the models namespace).
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Codemode
    }

    fn run(&self, args: &JsonValue, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let args = args.clone();
        let signal = ctx.signal.clone();
        let kind = self.kind;
        Box::pin(async move {
            let request: BridgeRequest = match serde_json::from_value(args) {
                Ok(request) => request,
                Err(error) => {
                    return fail(format!(
                        "models bridge expects {{ payload, options: {{ provider, model }} }}: {error}"
                    ));
                }
            };
            let Some(options) = request.options else {
                return fail(
                    "models bridge requires options: { provider, model } naming a catalog model",
                );
            };
            match kind {
                BridgeKind::GenerateImages => {
                    let context = match images_context_from_payload(&request.payload) {
                        Ok(context) => context,
                        Err(message) => return fail(message),
                    };
                    let model = match lookup(catalog::load_image_model(
                        &options.provider,
                        &options.model,
                    )) {
                        Ok(Some(model)) => model,
                        Ok(None) => {
                            return Self::unknown_model_error(
                                "an image",
                                "image",
                                &options.provider,
                                &options.model,
                            );
                        }
                        Err(message) => return fail(message),
                    };
                    let image_options = crate::ai::types::ImagesOptions {
                        api_key: options.api_key,
                        headers: options.headers,
                    };
                    let result = crate::ai::images::generate_images(
                        &model,
                        &context,
                        &image_options,
                        signal.as_ref(),
                    )
                    .await;
                    ok_json(&result)
                }
                BridgeKind::Classify => {
                    let context: ClassifierContext = match serde_json::from_value(request.payload) {
                        Ok(context) => context,
                        Err(error) => {
                            return fail(format!(
                                "models.classify() expects a context {{ state, questions, images? }}: {error}"
                            ));
                        }
                    };
                    if context.questions.is_empty() {
                        return fail(
                            "models.classify() context.questions must map question IDs to questions",
                        );
                    }
                    let model = match lookup(catalog::load_classifier_model(
                        &options.provider,
                        &options.model,
                    )) {
                        Ok(Some(model)) => model,
                        Ok(None) => {
                            return Self::unknown_model_error(
                                "a classifier",
                                "classifier",
                                &options.provider,
                                &options.model,
                            );
                        }
                        Err(message) => return fail(message),
                    };
                    let classifier_options = ClassifierOptions {
                        api_key: options.api_key,
                        headers: options.headers,
                        temperature: options.temperature,
                    };
                    let result = crate::ai::classify::classify(
                        &model,
                        &context,
                        &classifier_options,
                        signal.as_ref(),
                    )
                    .await;
                    ok_json(&result)
                }
            }
        })
    }
}

/// Build the image context from the script's first argument: a prompt string,
/// a blocks array (`{type: "text"|"image", ...}`), or a full
/// `{ input: [...] }` context (upstream `checkImagesContext`).
fn images_context_from_payload(payload: &JsonValue) -> Result<ImagesContext, String> {
    // Upstream checkImagesContext (codemode/execute.ts:173) rejects mistakes
    // such as a `{ prompt }` object: an object payload must carry a NON-EMPTY
    // `input` blocks array (serde's `default` would otherwise accept any
    // object as an empty context).
    const EXPECTED: &str = "models.generateImages() expects a prompt string, a blocks array \
         ([{ type: \"text\", text }, { type: \"image\", data, mimeType }]), \
         or a context { input: [...] }";
    match payload {
        JsonValue::String(prompt) => Ok(ImagesContext::from_prompt(prompt.clone())),
        JsonValue::Object(_) => serde_json::from_value::<ImagesContext>(payload.clone())
            .ok()
            .filter(|context| !context.input.is_empty())
            .ok_or_else(|| EXPECTED.to_owned()),
        JsonValue::Array(_) => {
            // A bare blocks array: `[{type: "text", ...}, ...]`.
            serde_json::from_value::<Vec<ImagesContent>>(payload.clone())
                .map(|input| ImagesContext { input })
                .map_err(|_| EXPECTED.to_owned())
        }
        JsonValue::Null => Err(
            "models.generateImages() expects a prompt string or a blocks array, got null"
                .to_owned(),
        ),
        other => Err(format!(
            "models.generateImages() expects a prompt string or a blocks array, got {other}"
        )),
    }
}
