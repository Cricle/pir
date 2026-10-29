use super::*;

// ---------------------------------------------------------------------------
// Cost calculation (upstream `calculateCost`, packages/ai/src/models.ts)
// ---------------------------------------------------------------------------

/// Compute the token cost of one usage record against `model`'s rates
/// (upstream `calculateCost`): writes `usage.cost` and returns it.
///
/// Rules mirrored from upstream:
/// - the tier input count is `input + cacheRead + cacheWrite` (all cache
///   writes, short and 1h, count toward the threshold);
/// - the applicable tier is the one with the GREATEST `input_tokens_above`
///   that is still below the input count (`inputTokens > tier.
///   inputTokensAbove && tier.inputTokensAbove > matchedThreshold`);
/// - Anthropic charges 2x the base input rate for 1h cache writes, so
///   `cache_write` splits into a short part at `rates.cache_write` and the
///   1h part at `rates.input * 2`.
pub fn calculate_cost(model: &Model, usage: &mut Usage) -> crate::ai::types::Cost {
    calculate_cost_with(&model.cost, usage)
}

/// [`calculate_cost`] against a bare [`ModelCost`] (a caller that only keeps
/// the model's rates — e.g. an SSE parser applying a fallback model's cost —
/// does not need the whole model).
pub fn calculate_cost_with(model_cost: &ModelCost, usage: &mut Usage) -> crate::ai::types::Cost {
    let input_tokens = usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);
    let mut rates = model_cost.rates;
    let mut matched_threshold: i64 = -1;
    for tier in &model_cost.tiers {
        let above = tier.input_tokens_above;
        if input_tokens > above && (above as i64) > matched_threshold {
            rates = tier.rates;
            matched_threshold = above as i64;
        }
    }

    // Anthropic charges 2x base input for 1h cache writes.
    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write.saturating_sub(long_write);

    usage.cost.input = rates.input / 1_000_000.0 * usage.input as f64;
    usage.cost.output = rates.output / 1_000_000.0 * usage.output as f64;
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * usage.cache_read as f64;
    usage.cost.cache_write = (rates.cache_write * short_write as f64
        + rates.input * 2.0 * long_write as f64)
        / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    // The total above is a catalog estimate, never a provider-billed amount
    // (upstream `calculateCost` sets `totalFromProvider = false`).
    usage.cost.total_from_provider = Some(false);
    usage.cost
}

// ---------------------------------------------------------------------------
// Model definition
// ---------------------------------------------------------------------------

/// Pricing per million tokens.
///
/// Wire shape is upstream camelCase (`cacheRead`, `cacheWrite`); the snake_case
/// spellings are accepted as deserialize aliases so previously serialized
/// snapshots still load.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    #[serde(alias = "cache_read")]
    pub cache_read: f64,
    #[serde(alias = "cache_write")]
    pub cache_write: f64,
}

/// A pricing tier for requests exceeding a token threshold.
///
/// Upstream keeps the tier's rates flat next to `inputTokensAbove`; the struct
/// groups them under `rates` internally and flattens them on the wire. Legacy
/// snapshots with a nested `rates` object (and snake_case spellings) still
/// deserialize.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    /// Use this tier for requests whose total input usage exceeds this token count.
    pub input_tokens_above: u64,
}

impl<'de> serde::Deserialize<'de> for ModelCostTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(default)]
            input: f64,
            #[serde(default)]
            output: f64,
            #[serde(default, alias = "cache_read")]
            cache_read: f64,
            #[serde(default, alias = "cache_write")]
            cache_write: f64,
            #[serde(default)]
            rates: Option<ModelCostRates>,
            #[serde(alias = "input_tokens_above")]
            input_tokens_above: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            rates: wire.rates.unwrap_or(ModelCostRates {
                input: wire.input,
                output: wire.output,
                cache_read: wire.cache_read,
                cache_write: wire.cache_write,
            }),
            input_tokens_above: wire.input_tokens_above,
        })
    }
}

/// Full model cost information.
///
/// Upstream's `ModelCost` is flat (`{input, output, cacheRead, cacheWrite,
/// tiers?}`); the struct groups the rates under `rates` internally and
/// flattens them on the wire. Legacy snapshots with a nested `rates` object
/// (and snake_case spellings) still deserialize.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    /// Request-wide pricing tiers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<ModelCostTier>,
}

impl<'de> serde::Deserialize<'de> for ModelCost {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(default)]
            input: f64,
            #[serde(default)]
            output: f64,
            #[serde(default, alias = "cache_read")]
            cache_read: f64,
            #[serde(default, alias = "cache_write")]
            cache_write: f64,
            #[serde(default)]
            rates: Option<ModelCostRates>,
            #[serde(default)]
            tiers: Vec<ModelCostTier>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            rates: wire.rates.unwrap_or(ModelCostRates {
                input: wire.input,
                output: wire.output,
                cache_read: wire.cache_read,
                cache_write: wire.cache_write,
            }),
            tiers: wire.tiers,
        })
    }
}
